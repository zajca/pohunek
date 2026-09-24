//! Supervisor evidence for durable-worker reconciliation.
//!
//! Reconciliation joins three independent sources per `(session, generation)`:
//! the native supervisor's job, the per-session worker socket, and the
//! worker's journal. This module turns the supervisor side into
//! [`JobEvidence`], decides what a generation without a reachable worker is
//! ([`classify_unreachable`]), reaps the processes a proven-crashed generation
//! left behind, and retries sessions whose supervisor could not be inspected.
//!
//! Every classification fails closed: a job that may still own a live PTY is
//! never retired or swept, and uncertain evidence is reported, not acted on.

use std::collections::BTreeSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use pohunek_platform::process::{
    sweep_runtime, ProcessIdentity, ProcessInspector, SkipReason, StartIdentity, SweepRequest,
};
use pohunek_platform::supervisor::{Error as SupervisorError, ServiceObservation, WorkerKey};
use protocol::{RuntimeInventoryEntry, RuntimeInventoryStatus, RuntimeState};

use super::{SessionId, SessionRecord, SessionRegistry, SessionRegistryInner};
use crate::runtime::lifecycle::{
    observation_live, Generation, Lifecycle, SUPERVISION_AMBIGUOUS, SUPERVISION_UNAVAILABLE,
};

// Rust guideline compliant 2026-09-24

/// Reason recorded when a generation's worker is proven crashed and the
/// marker sweep confirmed that no process of its runtime is left.
pub(crate) const RUNTIME_LOST: &str = "runtime_lost";

/// Reason recorded when a generation's worker is proven crashed but the marker
/// sweep could not confirm that every process of its runtime exited.
///
/// The runtime is still `Lost`: the worker is gone, and the sweep is never
/// repeated as a kill loop. Operators inspect the remaining processes.
pub(crate) const RUNTIME_LOST_CLEANUP_UNCONFIRMED: &str = "runtime_lost_cleanup_unconfirmed";

/// Inventory reason of an adopted live worker whose job the supervisor
/// proved absent: the native manager no longer tracks a worker that still
/// owns a PTY.
pub(crate) const UNSUPERVISED_WORKER: &str = "worker_job_absent";

/// Inventory reason of a live job whose generation no durable record names.
pub(crate) const STALE_GENERATION: &str = "stale_worker_generation";

/// Upper bound on marker-sweep passes for one lost runtime.
///
/// A single pass selects its targets before signalling, so a process forked
/// during the pass is only found by the next one. Three passes absorb a
/// short fork chain of a dying PTY tree; a runtime that still yields targets
/// after that is reported as unconfirmed instead of being swept forever.
const MAX_SWEEP_PASSES: usize = 3;

/// Liveness polling interval inside one sweep's grace windows.
///
/// Clamped to the configured grace. 50 ms notices an exit promptly without
/// busy-reading the process table while reconciliation waits.
const SWEEP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// First delay before re-reconciling sessions whose supervisor was unavailable.
///
/// A restarting user manager usually answers again within a second.
const SUPERVISION_RETRY_INITIAL: Duration = Duration::from_secs(1);

/// Interval between inspections of an abandoned create's job while it is
/// watched until its initialization deadline.
///
/// Matches the lifecycle engine's own polling of a not-yet-ready job; the
/// outcome only changes when the job ends or the deadline passes.
const ABANDONED_CREATE_POLL: Duration = Duration::from_millis(100);

/// Longest delay between supervision retries.
///
/// Retries double up to this bound so a long manager outage costs one
/// inspection per session per minute, while recovery is still noticed soon.
const SUPERVISION_RETRY_MAX: Duration = Duration::from_mins(1);

/// What the native supervisor says about one generation's job.
#[derive(Debug)]
pub(super) enum JobEvidence {
    /// The supervisor proved that the job does not exist.
    Absent,
    /// The job exists; its state and process are evidence, not readiness.
    Present(ServiceObservation),
    /// The supervisor could not be inspected, or the job kept changing.
    Unavailable(String),
}

/// Inspects the job of exactly `generation`.
///
/// A missing lifecycle engine (no supervisor configured) is unavailable
/// evidence, never absence.
pub(super) async fn observe(
    lifecycle: Option<&Lifecycle<'_>>,
    generation: &Generation,
) -> JobEvidence {
    let Some(lifecycle) = lifecycle else {
        return JobEvidence::Unavailable("no worker supervisor is configured".to_owned());
    };
    match lifecycle.inspect(generation).await {
        Ok(Some(observation)) => JobEvidence::Present(observation),
        Ok(None) => JobEvidence::Absent,
        Err(error) => JobEvidence::Unavailable(error.to_string()),
    }
}

/// Journal facts that identify one generation's worker process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct JournalWorker<'a> {
    /// Worker process identifier recorded by the worker itself.
    pub(super) pid: u32,
    /// Worker process start identity, as recorded.
    pub(super) start_identity: &'a str,
    /// Runtime (PTY) identifier after initialization; its processes carry it
    /// as their ownership marker.
    pub(super) runtime_id: Option<&'a str>,
}

impl JournalWorker<'_> {
    /// Parses the recorded worker process identity.
    fn identity(&self) -> Result<ProcessIdentity, String> {
        let start_identity = self
            .start_identity
            .parse::<StartIdentity>()
            .map_err(|error| format!("worker journal start identity is malformed: {error}"))?;
        Ok(ProcessIdentity {
            pid: self.pid,
            start_identity,
        })
    }
}

/// Returns why a present job is not exactly `generation`'s job, if it is not.
///
/// The backend's recorded definition must run the record's executable with
/// the session's `--session-id` and `--worker-generation` arguments, and its
/// main process must be the worker the journal names.
pub(super) fn job_identity_mismatch(
    observation: &ServiceObservation,
    generation: &Generation,
    worker: Option<&JournalWorker<'_>>,
) -> Option<String> {
    if let Some(definition) = &observation.definition {
        if definition.executable != generation.executable() {
            return Some(format!(
                "job {} runs {} instead of {}",
                observation.id,
                definition.executable.display(),
                generation.executable().display()
            ));
        }
        let names = |flag: &str, value: &str| {
            definition
                .arguments
                .windows(2)
                .any(|pair| pair[0] == flag && pair[1] == value)
        };
        if !names("--session-id", generation.session_id())
            || !names("--worker-generation", generation.generation())
        {
            return Some(format!(
                "job {} does not name session {} generation {}",
                observation.id,
                generation.session_id(),
                generation.generation()
            ));
        }
    }
    match (observation.process, worker.map(JournalWorker::identity)) {
        (Some(_), Some(Err(detail))) => Some(detail),
        (Some(process), Some(Ok(journal))) if process != journal => Some(format!(
            "job {} main process {} is not the journaled worker {}",
            observation.id, process.pid, journal.pid
        )),
        _ => None,
    }
}

/// Classification of a generation whose worker socket yielded no worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Unreachable {
    /// The supervisor could not be inspected; nothing is touched.
    Unavailable(String),
    /// The generation may still own a live PTY; nothing is touched.
    Ambiguous(String),
    /// The job or its process is not the recorded generation's; fail closed.
    Mismatch(String),
    /// The job is absent or ended and the worker process is proven gone.
    Ended {
        /// Whether the generation's worker had journaled at all; a journaled
        /// worker that is gone crashed, while a missing journal means it
        /// never started.
        journaled: bool,
        /// Runtime whose marked processes may have survived the worker.
        runtime_id: Option<String>,
    },
}

/// Decides what a generation without a reachable worker is.
///
/// Worker death is proven only when the job is absent or ended without a
/// process AND the journaled worker PID is not running with its recorded
/// start identity (a reused PID therefore counts as not running).
pub(super) fn classify_unreachable(
    job: &JobEvidence,
    generation: &Generation,
    worker: Option<&JournalWorker<'_>>,
    inspector: &dyn ProcessInspector,
) -> Unreachable {
    match job {
        JobEvidence::Unavailable(detail) => return Unreachable::Unavailable(detail.clone()),
        JobEvidence::Present(observation) => {
            if let Some(detail) = job_identity_mismatch(observation, generation, worker) {
                return Unreachable::Mismatch(detail);
            }
            if observation_live(observation) {
                return Unreachable::Ambiguous(format!(
                    "job {} is present but its worker socket is not reachable",
                    observation.id
                ));
            }
        }
        JobEvidence::Absent => {}
    }
    let Some(worker) = worker else {
        return Unreachable::Ended {
            journaled: false,
            runtime_id: None,
        };
    };
    let identity = match worker.identity() {
        Ok(identity) => identity,
        Err(detail) => return Unreachable::Mismatch(detail),
    };
    match inspector.is_running(identity) {
        Ok(false) => Unreachable::Ended {
            journaled: true,
            runtime_id: worker.runtime_id.map(ToOwned::to_owned),
        },
        Ok(true) => Unreachable::Ambiguous(format!(
            "worker process {} is running but its socket is not reachable",
            identity.pid
        )),
        Err(error) => Unreachable::Ambiguous(format!(
            "worker process {} cannot be inspected: {error}",
            identity.pid
        )),
    }
}

/// Whether the marker sweep of a lost runtime is known to be complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cleanup {
    /// A final pass selected no process of the runtime.
    Complete,
    /// Some process may survive; the loss reason records it.
    Unconfirmed,
}

/// Sessions waiting for their supervisor to answer again.
#[derive(Debug, Default)]
pub(super) struct SupervisionRetries {
    state: std::sync::Mutex<RetryState>,
}

#[derive(Debug, Default)]
struct RetryState {
    pending: BTreeSet<String>,
    running: bool,
}

impl SessionRegistry {
    /// Reaps the processes a proven-crashed generation's runtime left behind.
    ///
    /// Runs [`sweep_runtime`] until a pass selects nothing, at most
    /// [`MAX_SWEEP_PASSES`] times. An unconfirmed exit or a sweep error stops
    /// immediately and is reported; the sweep is never retried as a kill loop.
    pub(super) async fn sweep_lost_runtime(&self, session_id: &str, runtime_id: &str) -> Cleanup {
        let Some(grace) = self
            .inner
            .config
            .supervision
            .as_ref()
            .map(|supervision| supervision.sweep_grace)
        else {
            tracing::warn!(
                session_id,
                runtime_id,
                "lost runtime is not swept without a supervision configuration"
            );
            return Cleanup::Unconfirmed;
        };
        let request = match SweepRequest::new(
            runtime_id,
            rustix::process::geteuid().as_raw(),
            grace,
            SWEEP_POLL_INTERVAL.min(grace),
        ) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(session_id, runtime_id, error = %error, "lost runtime cannot be swept");
                return Cleanup::Unconfirmed;
            }
        };
        for pass in 1..=MAX_SWEEP_PASSES {
            let report = match sweep_runtime(self.inner.inspector.as_ref(), &request).await {
                Ok(report) => report,
                Err(error) => {
                    tracing::warn!(
                        session_id,
                        runtime_id,
                        sweep.pass = pass,
                        error = %error,
                        "lost runtime sweep aborted; remaining processes are left alone"
                    );
                    return Cleanup::Unconfirmed;
                }
            };
            let unreadable = report
                .skipped
                .iter()
                .filter(|skipped| skipped.reason == SkipReason::MarkersUnreadable)
                .count();
            let selected = report.terminated.len()
                + report.killed.len()
                + report.unconfirmed.len()
                + report.skipped.len()
                - unreadable;
            tracing::info!(
                session_id,
                runtime_id,
                sweep.pass = pass,
                sweep.terminated = report.terminated.len(),
                sweep.killed = report.killed.len(),
                sweep.unconfirmed = report.unconfirmed.len(),
                sweep.unreadable = unreadable,
                "swept processes of a lost runtime"
            );
            if !report.is_complete()
                || report
                    .skipped
                    .iter()
                    .any(|skipped| skipped.reason == SkipReason::CurrentProcess)
            {
                tracing::warn!(
                    session_id,
                    runtime_id,
                    "lost runtime sweep could not confirm every process exited"
                );
                return Cleanup::Unconfirmed;
            }
            if selected == 0 {
                return Cleanup::Complete;
            }
        }
        tracing::warn!(
            session_id,
            runtime_id,
            sweep.passes = MAX_SWEEP_PASSES,
            "lost runtime still had marked processes after every sweep pass"
        );
        Cleanup::Unconfirmed
    }

    /// Retires ended jobs of generations no durable record names.
    ///
    /// A job whose generation is not a record's current generation (or whose
    /// session has no record) is re-inspected under its session's lifecycle
    /// lock. Proven ended, it is retired by exact service ID, which also
    /// removes its definition. Still live, it is left alone and returned as an
    /// orphaned inventory entry. Discovery failure only skips this cleanup;
    /// every record's own generation is inspected individually.
    pub(super) async fn retire_stale_jobs(
        &self,
        lifecycle: Option<&Lifecycle<'_>>,
        records: &[SessionRecord],
    ) -> Vec<RuntimeInventoryEntry> {
        let Some(lifecycle) = lifecycle else {
            return Vec::new();
        };
        let observations = match lifecycle.supervisor.discover().await {
            Ok(observations) => observations,
            Err(error) => {
                tracing::warn!(error = %error, "worker job discovery failed; stale jobs are kept");
                return Vec::new();
            }
        };
        let mut orphans = Vec::new();
        for observation in observations {
            let key = match WorkerKey::from_service_id(&observation.id) {
                Ok(key) => key,
                Err(error) => {
                    tracing::warn!(service_id = %observation.id, error = %error, "ignoring an unowned worker job");
                    continue;
                }
            };
            let current = records.iter().any(|record| {
                record.session_id == key.session_id()
                    && record.runtime.generation.as_deref() == Some(key.generation())
            });
            if current {
                continue;
            }
            let _guard = self
                .lock_lifecycle(&SessionId(key.session_id().to_owned()))
                .await;
            match stale_job_ended(lifecycle, &observation).await {
                Ok(true) => match lifecycle.supervisor.retire(&observation.id).await {
                    Ok(()) | Err(SupervisorError::NotFound(_)) => {
                        tracing::info!(service_id = %observation.id, "retired an ended stale worker job");
                    }
                    Err(error) => {
                        tracing::warn!(service_id = %observation.id, error = %error, "failed to retire an ended stale worker job");
                    }
                },
                Ok(false) => {
                    tracing::warn!(service_id = %observation.id, "stale worker job is still live; leaving it untouched");
                    orphans.push(RuntimeInventoryEntry {
                        runtime_slot: observation.id.to_string(),
                        claimed_session_id: None,
                        worker_id: None,
                        runtime_id: None,
                        status: RuntimeInventoryStatus::Orphaned,
                        reason: Some(STALE_GENERATION.to_owned()),
                    });
                }
                Err(error) => {
                    tracing::warn!(service_id = %observation.id, error = %error, "stale worker job cannot be inspected; leaving it untouched");
                }
            }
        }
        orphans
    }

    /// Re-reconciles `id` in the background until its evidence settles.
    ///
    /// Sessions wait here while their supervisor is unavailable or while
    /// their generation is ambiguous (a job or process that may still be
    /// live but no reachable worker). One task serves every pending session
    /// with a doubling delay bounded by [`SUPERVISION_RETRY_MAX`]; each pass
    /// classifies from fresh evidence and never kills while it stays
    /// ambiguous. The task stops when no session is pending, when daemon
    /// shutdown starts, or when the registry is dropped; however it ends, a
    /// later call starts a new one.
    pub(super) fn schedule_supervision_retry(&self, id: &SessionId) {
        let start = {
            let mut state = self
                .inner
                .supervision_retries
                .state
                .lock()
                .expect("supervision retry state is never poisoned");
            state.pending.insert(id.0.clone());
            !std::mem::replace(&mut state.running, true)
        };
        if start {
            let inner = Arc::downgrade(&self.inner);
            let shutdown = self.inner.daemon_shutdown.clone();
            tokio::spawn(async move {
                run_supervision_retries(inner, shutdown).await;
            });
        }
    }

    /// Runs one retry pass of `id` in its own task.
    ///
    /// A pass that panics (a broken reconciliation invariant) ends only that
    /// pass: it is logged, and `false` keeps the session pending, so the next
    /// pass re-checks it and the loop keeps serving every other session.
    async fn retry_supervised_session_isolated(&self, id: &SessionId) -> bool {
        let registry = self.clone();
        let task_id = id.clone();
        match tokio::spawn(async move { registry.retry_supervised_session(&task_id).await }).await {
            Ok(settled) => settled,
            Err(error) => {
                tracing::warn!(
                    session_id = %id.0,
                    task.panicked = error.is_panic(),
                    error = %error,
                    "supervision retry pass ended abnormally; the session stays pending"
                );
                false
            }
        }
    }

    /// Re-reconciles one session left `runtime_supervision_unavailable` or
    /// `runtime_supervision_ambiguous`.
    ///
    /// Returns whether the session no longer needs a retry: it was resolved,
    /// removed, or changed by another lifecycle operation.
    async fn retry_supervised_session(&self, id: &SessionId) -> bool {
        let _guard = self.lock_lifecycle(id).await;
        let waiting = self
            .inner
            .sessions
            .lock()
            .await
            .get(id)
            .is_some_and(|entry| {
                matches!(entry.runtime, super::RuntimeHandle::Unavailable(_))
                    && entry.info.runtime.as_ref().is_some_and(|runtime| {
                        matches!(
                            (runtime.state, runtime.loss_reason.as_deref()),
                            (RuntimeState::Reconnecting, Some(SUPERVISION_UNAVAILABLE))
                                | (RuntimeState::Conflict, Some(SUPERVISION_AMBIGUOUS))
                        )
                    })
            });
        if !waiting {
            return true;
        }
        let record = match self.load_durable_session_record(id).await {
            Ok(Some(record)) => record,
            Ok(None) => return true,
            Err(error) => {
                tracing::debug!(session_id = %id.0, error = %error, "supervision retry cannot load the record");
                return false;
            }
        };
        !self.reconcile_single_session(record).await
    }
}

impl SessionRegistry {
    /// Settles a create whose daemon went away before its worker was ready.
    ///
    /// The create's initialization context died with the previous daemon, so
    /// its generation is never adopted. A background task takes the session's
    /// lifecycle lock (startup is not blocked), watches the job from first
    /// sight until that instant plus the worker initialization deadline, and
    /// then retires exactly that generation and compensates the create. A job
    /// that ends earlier is retired at once. An inspection or retirement
    /// failure leaves the session `runtime_supervision_unavailable` for the
    /// supervision retry; daemon shutdown leaves it to the next start. A
    /// settlement that panics is logged and handed to the supervision retry,
    /// so the session is never stranded without a re-check.
    pub(super) fn settle_abandoned_create(&self, id: SessionId, generation: Generation) {
        let registry = self.clone();
        tokio::spawn(async move {
            let settle = tokio::spawn(
                registry
                    .clone()
                    .watch_abandoned_create(id.clone(), generation.clone()),
            );
            let Err(error) = settle.await else {
                return;
            };
            tracing::warn!(
                session_id = %id.0,
                service_id = %generation.service_id(),
                task.panicked = error.is_panic(),
                error = %error,
                "abandoned create settlement ended abnormally; nothing is touched"
            );
            // The session is still marked ambiguous, which the supervision
            // retry re-checks from fresh evidence. A cancelled task means the
            // runtime is shutting down, and the next start reconciles it.
            if error.is_panic() {
                registry.schedule_supervision_retry(&id);
            }
        });
    }

    /// Watches, retires, and compensates one abandoned create; the body of
    /// [`Self::settle_abandoned_create`].
    async fn watch_abandoned_create(self, id: SessionId, generation: Generation) {
        let shutdown = self.inner.daemon_shutdown.clone();
        let _guard = self.lock_lifecycle(&id).await;
        let Ok(lifecycle) = self.lifecycle() else {
            return;
        };
        let give_up_at = tokio::time::Instant::now() + lifecycle.config.worker_initialize;
        loop {
            match lifecycle.inspect(&generation).await {
                Ok(Some(observation)) if observation_live(&observation) => {}
                Ok(_ended) => break,
                Err(error) => {
                    self.defer_abandoned_create(&id, &generation, &error).await;
                    return;
                }
            }
            let now = tokio::time::Instant::now();
            if now >= give_up_at {
                break;
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(ABANDONED_CREATE_POLL.min(give_up_at - now)) => {}
            }
        }
        match lifecycle.retire(&generation).await {
            Ok(()) => {
                tracing::info!(
                    session_id = %id.0,
                    service_id = %generation.service_id(),
                    "retired the generation of an abandoned create"
                );
                self.compensate_abandoned_create(&id).await;
            }
            Err(error) => {
                self.defer_abandoned_create(&id, &generation, &error).await;
            }
        }
    }

    /// Hands an abandoned create whose job cannot be settled to the retry.
    async fn defer_abandoned_create(
        &self,
        id: &SessionId,
        generation: &Generation,
        error: &SupervisorError,
    ) {
        tracing::warn!(
            session_id = %id.0,
            service_id = %generation.service_id(),
            error = %error,
            "abandoned create cannot be settled; nothing is touched"
        );
        match self.load_durable_session_record(id).await {
            Ok(Some(record)) => {
                self.insert_unavailable_record(
                    record,
                    RuntimeState::Reconnecting,
                    SUPERVISION_UNAVAILABLE,
                )
                .await;
                self.schedule_supervision_retry(id);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(session_id = %id.0, error = %error, "abandoned create record cannot be loaded");
            }
        }
    }
}

/// Serves the pending supervision retries of one registry.
async fn run_supervision_retries(
    inner: Weak<SessionRegistryInner>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut running = RunningGuard {
        inner: Weak::clone(&inner),
        armed: true,
    };
    let mut delay = SUPERVISION_RETRY_INITIAL;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let registry = SessionRegistry { inner };
        let pending = registry
            .inner
            .supervision_retries
            .state
            .lock()
            .expect("supervision retry state is never poisoned")
            .pending
            .clone();
        for session_id in pending {
            if shutdown.is_cancelled() {
                return;
            }
            let id = SessionId(session_id);
            if registry.retry_supervised_session_isolated(&id).await {
                registry
                    .inner
                    .supervision_retries
                    .state
                    .lock()
                    .expect("supervision retry state is never poisoned")
                    .pending
                    .remove(&id.0);
            }
        }
        {
            let mut state = registry
                .inner
                .supervision_retries
                .state
                .lock()
                .expect("supervision retry state is never poisoned");
            if state.pending.is_empty() {
                // Cleared under the same lock as the emptiness check, so a
                // session scheduled right after it starts a new task.
                state.running = false;
                running.armed = false;
                return;
            }
        }
        delay = (delay * 2).min(SUPERVISION_RETRY_MAX);
    }
}

/// Clears the retry task's `running` flag when the task ends early.
///
/// Shutdown, a dropped registry, or an unexpected unwind all end the task
/// with sessions possibly pending; clearing the flag lets the next
/// [`SessionRegistry::schedule_supervision_retry`] start a new task instead
/// of assuming one still runs. A normal exit clears the flag itself and
/// disarms the guard.
#[derive(Debug)]
struct RunningGuard {
    inner: Weak<SessionRegistryInner>,
    armed: bool,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(inner) = self.inner.upgrade() {
            // Recovering a poisoned state is required here: panicking inside
            // `drop` during an unwind aborts the daemon.
            inner
                .supervision_retries
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .running = false;
        }
    }
}

/// Proves a stale job ended with a fresh inspection; absence counts as ended.
async fn stale_job_ended(
    lifecycle: &Lifecycle<'_>,
    observation: &ServiceObservation,
) -> Result<bool, SupervisorError> {
    Ok(lifecycle
        .inspect_service(&observation.id)
        .await?
        .is_none_or(|fresh| !observation_live(&fresh)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionRegistryConfig;

    /// Bound on observing the retry task end.
    const TASK_END_DEADLINE: Duration = Duration::from_secs(10);

    fn retry_state(registry: &SessionRegistry) -> (bool, BTreeSet<String>) {
        let state = registry
            .inner
            .supervision_retries
            .state
            .lock()
            .expect("supervision retry state is never poisoned");
        (state.running, state.pending.clone())
    }

    async fn wait_not_running(registry: &SessionRegistry) {
        let deadline = tokio::time::Instant::now() + TASK_END_DEADLINE;
        while retry_state(registry).0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the retry task never cleared its running flag"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn retry_task_that_ends_early_lets_a_later_schedule_start_a_new_one() {
        let registry = SessionRegistry::new(SessionRegistryConfig::default());
        registry.begin_daemon_shutdown();
        let first = SessionId("s-401".to_owned());

        registry.schedule_supervision_retry(&first);
        wait_not_running(&registry).await;
        assert_eq!(
            retry_state(&registry).1,
            BTreeSet::from([first.0.clone()]),
            "an early end keeps the session pending"
        );

        let second = SessionId("s-402".to_owned());
        registry.schedule_supervision_retry(&second);
        wait_not_running(&registry).await;
        assert_eq!(
            retry_state(&registry).1,
            BTreeSet::from([first.0, second.0]),
            "the later schedule ran a new task"
        );
    }

    #[test]
    fn running_guard_clears_the_flag_only_while_armed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let _entered = runtime.enter();
        let registry = SessionRegistry::new(SessionRegistryConfig::default());
        let set_running = || {
            registry
                .inner
                .supervision_retries
                .state
                .lock()
                .expect("supervision retry state is never poisoned")
                .running = true;
        };

        set_running();
        drop(RunningGuard {
            inner: Arc::downgrade(&registry.inner),
            armed: false,
        });
        assert!(retry_state(&registry).0, "a disarmed guard leaves the flag");

        set_running();
        drop(RunningGuard {
            inner: Arc::downgrade(&registry.inner),
            armed: true,
        });
        assert!(!retry_state(&registry).0, "an armed guard clears the flag");
    }
}
