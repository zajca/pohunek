//! Daemon-wide shutdown: one tree of tasks stopped in ordered, bounded phases.
//!
//! The session registry stops its own background work through
//! `BackgroundTasks`. The tasks that drive the registry from outside (idle
//! stop, retention, notifications, the agent-state hook flush) and the
//! event-log drains belong to [`DaemonShutdown`], which runs them in four
//! phases: in-flight creates, the drivers, the registry, the event logs. Each
//! phase has its own bound from `service.toml`, and the bounds together fit
//! inside the service manager's exit timeout (see
//! `Deadlines::fitted_shutdown`), so a slow phase cannot push the
//! event-log flush past the kill.
//!
//! Drivers are cooperative: shutdown asks them to stop and waits. A driver
//! still running at the phase bound is named in the report and left running;
//! it is never aborted, so a store transaction it holds is not cut in half.

// Rust guideline compliant 2026-10-28

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::future::join_all;
use pohunek_service_config::{
    Deadlines, ShutdownShortfall, DEFAULT_CREATE_DRAIN, DEFAULT_DRIVER_SHUTDOWN,
    DEFAULT_EVENT_LOG_FLUSH, DEFAULT_REGISTRY_SHUTDOWN,
};
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;
use tracing::{debug, warn};

use crate::session::SessionRegistry;

type StopFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type Stop = Box<dyn FnOnce() -> StopFuture + Send>;

/// The phases of a daemon shutdown, in the order they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownPhase {
    /// Session creates in flight commit or compensate.
    Creates,
    /// The tasks that drive the registry from outside stop.
    Drivers,
    /// The session registry stops its background work.
    Registry,
    /// The event-log drains flush what they buffered.
    EventLogs,
}

impl ShutdownPhase {
    /// The phase name used in logs and reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creates => "creates",
            Self::Drivers => "drivers",
            Self::Registry => "registry",
            Self::EventLogs => "event_logs",
        }
    }
}

/// Where a task stops within the daemon shutdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// An external driver of the registry; stops with the other drivers.
    Driver,
    /// A consumer of what the drivers produce; stops after every driver, under
    /// the drivers' bound, so the last items they hand over are still accepted.
    Sink,
    /// An event-log drain; stops in the event-log phase, after the registry.
    EventLog,
}

/// The bound of each shutdown phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownPlan {
    /// Bound of [`ShutdownPhase::Creates`].
    pub create_drain: Duration,
    /// Bound of [`ShutdownPhase::Drivers`], shared by every driver and sink.
    pub driver_shutdown: Duration,
    /// Bound of [`ShutdownPhase::Registry`].
    pub registry_shutdown: Duration,
    /// Bound of [`ShutdownPhase::EventLogs`].
    pub event_log_flush: Duration,
    /// Set when the bounds `service.toml` records did not fit the exit timeout
    /// and were shortened.
    pub shortfall: Option<ShutdownShortfall>,
}

impl ShutdownPlan {
    /// The bounds `service.toml` records, fitted to its exit timeout.
    #[must_use]
    pub fn from_deadlines(deadlines: &Deadlines) -> Self {
        let fitted = deadlines.fitted_shutdown();
        Self {
            create_drain: fitted.create_drain,
            driver_shutdown: fitted.driver_shutdown,
            registry_shutdown: fitted.registry_shutdown,
            event_log_flush: fitted.event_log_flush,
            shortfall: fitted.shortfall,
        }
    }
}

impl Default for ShutdownPlan {
    /// The defaults of a daemon that runs without a `service.toml`.
    fn default() -> Self {
        Self {
            create_drain: DEFAULT_CREATE_DRAIN,
            driver_shutdown: DEFAULT_DRIVER_SHUTDOWN,
            registry_shutdown: DEFAULT_REGISTRY_SHUTDOWN,
            event_log_flush: DEFAULT_EVENT_LOG_FLUSH,
            shortfall: None,
        }
    }
}

/// What one phase found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseReport {
    /// The phase.
    pub phase: ShutdownPhase,
    /// Names of the tasks still running when the phase bound passed.
    pub still_running: Vec<&'static str>,
}

/// What [`DaemonShutdown::run`] found, phase by phase.
#[must_use = "a report that is not quiescent means a task outlived its phase bound"]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonShutdownReport {
    /// One entry per phase, in the order the phases ran.
    pub phases: Vec<PhaseReport>,
    /// Set when the phase bounds were shortened to fit the exit timeout.
    pub shortened: Option<ShutdownShortfall>,
}

impl DaemonShutdownReport {
    /// Whether every task of every phase ended inside its bound.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.phases
            .iter()
            .all(|phase| phase.still_running.is_empty())
    }

    /// Names of the tasks that outlived their phase bound, with their phase.
    #[must_use]
    pub fn still_running(&self) -> Vec<(ShutdownPhase, &'static str)> {
        self.phases
            .iter()
            .flat_map(|phase| phase.still_running.iter().map(|name| (phase.phase, *name)))
            .collect()
    }
}

struct Parts {
    stop: Stop,
    handle: Option<JoinHandle<()>>,
}

struct Slot {
    name: &'static str,
    stage: Stage,
    parts: Mutex<Option<Parts>>,
}

impl Slot {
    fn take(&self) -> Option<Parts> {
        self.parts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// Runs the stop request of a slot and waits for its task.
async fn stop_parts(name: &'static str, parts: Parts) {
    (parts.stop)().await;
    if let Some(handle) = parts.handle {
        if let Err(error) = handle.await {
            warn!(task = name, error = %error, "daemon task failed during shutdown");
        }
    }
}

struct State {
    closed: bool,
    slots: Vec<Arc<Slot>>,
}

struct Inner {
    plan: ShutdownPlan,
    tracker: TaskTracker,
    state: Mutex<State>,
}

/// Owner of the daemon-level tasks and their ordered shutdown.
#[derive(Clone)]
pub struct DaemonShutdown {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for DaemonShutdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonShutdown")
            .field("plan", &self.inner.plan)
            .finish_non_exhaustive()
    }
}

impl Default for DaemonShutdown {
    fn default() -> Self {
        Self::new(ShutdownPlan::default())
    }
}

/// Stops one task of the tree before the daemon shuts down, and waits for it.
#[derive(Clone)]
pub struct DriverHandle {
    slot: Arc<Slot>,
}

impl std::fmt::Debug for DriverHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverHandle")
            .field("name", &self.slot.name)
            .finish_non_exhaustive()
    }
}

impl DriverHandle {
    /// Stops the task and waits for it, without a bound of its own.
    ///
    /// A no-op when the task was already stopped, by this handle or by the
    /// daemon shutdown.
    pub async fn stop(&self) {
        if let Some(parts) = self.slot.take() {
            stop_parts(self.slot.name, parts).await;
        }
    }
}

impl DaemonShutdown {
    /// An empty tree with the phase bounds of `plan`.
    #[must_use]
    pub fn new(plan: ShutdownPlan) -> Self {
        Self {
            inner: Arc::new(Inner {
                plan,
                tracker: TaskTracker::new(),
                state: Mutex::new(State {
                    closed: false,
                    slots: Vec::new(),
                }),
            }),
        }
    }

    /// The phase bounds.
    #[must_use]
    pub fn plan(&self) -> ShutdownPlan {
        self.inner.plan
    }

    /// Spawns `task` as `name` in `stage`.
    ///
    /// `stop` asks the task to end (cancel its token, send it a command);
    /// shutdown then waits for it. After shutdown started the task is
    /// refused: it never runs.
    pub fn spawn<F>(
        &self,
        name: &'static str,
        stage: Stage,
        stop: impl FnOnce() + Send + 'static,
        task: F,
    ) -> DriverHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut guard = self.lock();
        let handle = (!guard.closed).then(|| self.inner.tracker.spawn(task));
        Self::register(
            &mut guard,
            name,
            stage,
            Box::new(move || {
                stop();
                Box::pin(std::future::ready(())) as StopFuture
            }),
            handle,
        )
    }

    /// Adds a task that was spawned elsewhere.
    pub fn adopt(
        &self,
        name: &'static str,
        stage: Stage,
        stop: impl FnOnce() + Send + 'static,
        handle: JoinHandle<()>,
    ) -> DriverHandle {
        let mut guard = self.lock();
        Self::register(
            &mut guard,
            name,
            stage,
            Box::new(move || {
                stop();
                Box::pin(std::future::ready(())) as StopFuture
            }),
            Some(handle),
        )
    }

    /// Adds a step that has no task of its own: `step` runs when the stage
    /// stops and the stage waits for it.
    pub fn add_step<Fut>(
        &self,
        name: &'static str,
        stage: Stage,
        step: impl FnOnce() -> Fut + Send + 'static,
    ) -> DriverHandle
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let mut guard = self.lock();
        Self::register(
            &mut guard,
            name,
            stage,
            Box::new(move || Box::pin(step()) as StopFuture),
            None,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn register(
        tree: &mut State,
        name: &'static str,
        stage: Stage,
        stop: Stop,
        handle: Option<JoinHandle<()>>,
    ) -> DriverHandle {
        let slot = Arc::new(Slot {
            name,
            stage,
            parts: Mutex::new((!tree.closed).then_some(Parts { stop, handle })),
        });
        if !tree.closed {
            tree.slots.push(Arc::clone(&slot));
        }
        DriverHandle { slot }
    }

    /// Stops everything in order and reports what outlived its bound.
    ///
    /// Closes the tree first, so a task spawned meanwhile is refused. The
    /// registry's own shutdown (including its create drain) runs between the
    /// drivers and the event logs, which are the last to flush.
    pub async fn run(&self, sessions: &SessionRegistry) -> DaemonShutdownReport {
        let slots = {
            let mut guard = self.lock();
            guard.closed = true;
            std::mem::take(&mut guard.slots)
        };
        self.inner.tracker.close();
        let plan = self.inner.plan;
        let mut phases = Vec::with_capacity(4);

        let drained = sessions.drain_creates().await;
        phases.push(PhaseReport {
            phase: ShutdownPhase::Creates,
            still_running: if drained {
                Vec::new()
            } else {
                vec!["session_creates"]
            },
        });

        let deadline = tokio::time::Instant::now() + plan.driver_shutdown;
        let mut stragglers = stop_stage(&slots, Stage::Driver, deadline).await;
        stragglers.extend(stop_stage(&slots, Stage::Sink, deadline).await);
        phases.push(PhaseReport {
            phase: ShutdownPhase::Drivers,
            still_running: stragglers,
        });

        let registry = sessions.shutdown().await;
        phases.push(PhaseReport {
            phase: ShutdownPhase::Registry,
            still_running: registry.still_running,
        });

        let deadline = tokio::time::Instant::now() + plan.event_log_flush;
        phases.push(PhaseReport {
            phase: ShutdownPhase::EventLogs,
            still_running: stop_stage(&slots, Stage::EventLog, deadline).await,
        });

        let report = DaemonShutdownReport {
            phases,
            shortened: plan.shortfall,
        };
        if let Some(shortfall) = &report.shortened {
            warn!(%shortfall, "daemon shutdown ran with shortened phase bounds");
        }
        for phase in &report.phases {
            if phase.still_running.is_empty() {
                debug!(
                    phase = phase.phase.as_str(),
                    "daemon shutdown phase finished"
                );
            } else {
                warn!(
                    phase = phase.phase.as_str(),
                    tasks = ?phase.still_running,
                    "daemon shutdown phase left tasks running at its bound"
                );
            }
        }
        report
    }
}

/// Stops every slot of `stage` together and waits until `deadline`; returns
/// the names of the tasks still running then.
async fn stop_stage(
    slots: &[Arc<Slot>],
    stage: Stage,
    deadline: tokio::time::Instant,
) -> Vec<&'static str> {
    let pending = Arc::new(Mutex::new(Vec::new()));
    let mut stops = Vec::new();
    for slot in slots.iter().filter(|slot| slot.stage == stage) {
        let Some(parts) = slot.take() else { continue };
        let name = slot.name;
        let pending = Arc::clone(&pending);
        pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(name);
        stops.push(async move {
            stop_parts(name, parts).await;
            let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(position) = pending.iter().position(|pending| *pending == name) {
                pending.swap_remove(position);
            }
        });
    }
    let _ = tokio::time::timeout_at(deadline, join_all(stops)).await;
    let mut left = pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    left.sort_unstable();
    left
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    use pohunek_service_config::SHUTDOWN_MARGIN;

    use super::*;
    use crate::session::SessionRegistryConfig;

    /// The service manager's stop timeout of a default installation
    /// (`deadlines.daemon_exit_timeout_ms`).
    // timing-allowed: #1625 the policy under test is the 30 s service stop timeout
    const SERVICE_EXIT_TIMEOUT: Duration = Duration::from_secs(30);

    fn registry(plan: ShutdownPlan) -> SessionRegistry {
        SessionRegistry::new(SessionRegistryConfig {
            create_drain_timeout: plan.create_drain,
            shutdown_timeout: plan.registry_shutdown,
            ..SessionRegistryConfig::default()
        })
    }

    type Log = Arc<Mutex<Vec<&'static str>>>;

    fn record(log: &Log, entry: &'static str) {
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
    }

    /// A task that ends once its token is cancelled and logs `entry` then.
    fn cancellable(
        tasks: &DaemonShutdown,
        name: &'static str,
        stage: Stage,
        log: &Log,
        entry: &'static str,
    ) {
        let token = CancellationToken::new();
        let stop = token.clone();
        let log = Arc::clone(log);
        tasks.spawn(name, stage, move || stop.cancel(), async move {
            token.cancelled().await;
            record(&log, entry);
        });
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_runs_phases_in_order() {
        let plan = ShutdownPlan::default();
        let tasks = DaemonShutdown::new(plan);
        let sessions = registry(plan);
        let log: Log = Log::default();
        cancellable(&tasks, "driver", Stage::Driver, &log, "driver ended");
        let sink_log = Arc::clone(&log);
        let sink_token = CancellationToken::new();
        let sink_stop = sink_token.clone();
        tasks.spawn(
            "sink",
            Stage::Sink,
            move || {
                record(&sink_log, "sink stopped");
                sink_stop.cancel();
            },
            async move { sink_token.cancelled().await },
        );
        let event_log = Arc::clone(&log);
        let probe = sessions.clone();
        tasks.add_step("event_log", Stage::EventLog, move || async move {
            assert!(
                probe.is_quiescent_for_test(),
                "the event log flushes after the registry stopped"
            );
            record(&event_log, "event log flushed");
        });

        let report = tasks.run(&sessions).await;

        assert!(report.is_quiescent(), "{report:?}");
        assert_eq!(
            report
                .phases
                .iter()
                .map(|phase| phase.phase)
                .collect::<Vec<_>>(),
            [
                ShutdownPhase::Creates,
                ShutdownPhase::Drivers,
                ShutdownPhase::Registry,
                ShutdownPhase::EventLogs
            ]
        );
        assert_eq!(
            *log.lock().unwrap_or_else(PoisonError::into_inner),
            ["driver ended", "sink stopped", "event log flushed"],
            "drivers stop before sinks, and the event log flushes last"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wedged_driver_is_named_and_later_phases_run() {
        let plan = ShutdownPlan::default();
        let tasks = DaemonShutdown::new(plan);
        let sessions = registry(plan);
        tasks.spawn(
            "wedged_driver",
            Stage::Driver,
            || {},
            std::future::pending(),
        );
        let flushed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&flushed);
        tasks.add_step("event_log", Stage::EventLog, move || async move {
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();

        let report = tasks.run(&sessions).await;

        assert_eq!(
            report.still_running(),
            [(ShutdownPhase::Drivers, "wedged_driver")]
        );
        assert_eq!(started.elapsed(), plan.driver_shutdown);
        assert!(
            flushed.load(Ordering::SeqCst),
            "the event log still flushed"
        );
        assert!(sessions.is_quiescent_for_test());
    }

    #[tokio::test(start_paused = true)]
    async fn wedged_registry_write_still_flushes_event_log() {
        let plan = ShutdownPlan::default();
        let tasks = DaemonShutdown::new(plan);
        let sessions = registry(plan);
        let _wedged_write = sessions.hold_transaction_for_test("wedged_store_write");
        let flushed_at = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&flushed_at);
        let started = Instant::now();
        tasks.add_step("event_log", Stage::EventLog, move || async move {
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(started.elapsed());
        });

        let report = tasks.run(&sessions).await;

        assert_eq!(
            report.still_running(),
            [(ShutdownPhase::Registry, "wedged_store_write")]
        );
        let flushed_at = flushed_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .expect("the event log flushed");
        assert_eq!(flushed_at, plan.registry_shutdown);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_steps_finish_inside_exit_budget() {
        let plan = ShutdownPlan::default();
        let tasks = DaemonShutdown::new(plan);
        let sessions = registry(plan);
        tasks.spawn(
            "wedged_driver",
            Stage::Driver,
            || {},
            std::future::pending(),
        );
        let _wedged_write = sessions.hold_transaction_for_test("wedged_store_write");
        let wedged_log = tokio::spawn(std::future::pending());
        tasks.adopt("wedged_event_log", Stage::EventLog, || {}, wedged_log);
        let started = Instant::now();

        let report = tasks.run(&sessions).await;

        assert_eq!(report.still_running().len(), 3, "{report:?}");
        assert!(
            plan.create_drain + started.elapsed() + SHUTDOWN_MARGIN <= SERVICE_EXIT_TIMEOUT,
            "every phase at its bound still ends before the service manager's kill"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shortened_plan_finishes_inside_the_short_exit_timeout_and_says_so() {
        let shortfall = ShutdownShortfall {
            required: Duration::from_secs(27),
            exit_timeout: Duration::from_secs(12),
        };
        let plan = ShutdownPlan {
            create_drain: Duration::from_secs(3),
            driver_shutdown: Duration::from_secs(2),
            registry_shutdown: Duration::from_secs(2),
            event_log_flush: Duration::from_secs(2),
            shortfall: Some(shortfall),
        };
        let tasks = DaemonShutdown::new(plan);
        let sessions = registry(plan);
        tasks.spawn(
            "wedged_driver",
            Stage::Driver,
            || {},
            std::future::pending(),
        );
        let _wedged_write = sessions.hold_transaction_for_test("wedged_store_write");
        let wedged_log = tokio::spawn(std::future::pending());
        tasks.adopt("wedged_event_log", Stage::EventLog, || {}, wedged_log);
        let started = Instant::now();

        let report = tasks.run(&sessions).await;

        assert_eq!(report.shortened, Some(shortfall));
        assert_eq!(report.still_running().len(), 3, "{report:?}");
        assert!(started.elapsed() + SHUTDOWN_MARGIN <= shortfall.exit_timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn driver_transaction_is_not_aborted_mid_write() {
        let plan = ShutdownPlan::default();
        let tasks = DaemonShutdown::new(plan);
        let sessions = registry(plan);
        let token = CancellationToken::new();
        let stop = token.clone();
        let committed = Arc::new(AtomicBool::new(false));
        let write = Arc::clone(&committed);
        tasks.spawn("writer", Stage::Driver, move || stop.cancel(), async move {
            token.cancelled().await;
            // The write that started before the stop request runs to its end.
            tokio::time::sleep(plan.driver_shutdown / 2).await;
            write.store(true, Ordering::SeqCst);
        });

        let report = tasks.run(&sessions).await;

        assert!(report.is_quiescent(), "{report:?}");
        assert!(committed.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_spawned_after_shutdown_started_never_runs() {
        let plan = ShutdownPlan::default();
        let tasks = DaemonShutdown::new(plan);
        let sessions = registry(plan);
        let report = tasks.run(&sessions).await;
        assert!(report.is_quiescent());
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let late = tasks.spawn("late", Stage::Driver, || {}, async move {
            flag.store(true, Ordering::SeqCst);
        });
        late.stop().await;
        tokio::task::yield_now().await;
        assert!(!ran.load(Ordering::SeqCst));
    }
}
