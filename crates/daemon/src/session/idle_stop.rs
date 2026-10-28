//! Idle stop of opted-in sessions.
//!
//! A session whose metadata carries [`IDLE_STOP_METADATA_KEY`] is stopped once
//! its agent has been `idle` for the requested time. The host default behind the
//! value `default` is a record of its own, apart from the retention policy.
//! A stop keeps the worktree and goes through the ordinary stop transaction, so
//! the `session-stop` hook fires and the session ends `stopped`.
//!
//! The clock is [`SessionEntry::activity_since`]: it moves only when the activity
//! value changes and is not persisted. One task owns the deadlines: it sleeps
//! until the earliest one, wakes on every change that can move a deadline, and
//! re-reads the session under the stop transaction's own lock before it stops
//! anything.

// Rust guideline compliant 2026-10-22

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use pohunek_platform::filesystem::AtomicReplaceError;
use protocol::{
    AgentActivity, ErrorClass, IdleStopOptIn, NotificationCreateParams, NotificationKind,
    NotificationSeverity, NotificationSource, ProtocolError, SessionId, SessionIdleStopParams,
    SessionIdleStopResult, SessionState, IDLE_STOP_METADATA_KEY,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::retention::{
    blocking, invalid_policy, open_policy_dir, policy_error, policy_io_error, MAX_POLICY_BYTES,
    OWNER_PRIVATE_FILE_MODE,
};
use super::{RuntimeHandle, SessionEntry, SessionRegistry};
use crate::notifications::{NotificationError, NotificationService};
use crate::shutdown::{DaemonShutdown, DriverHandle, Stage};

/// File name of the durable idle-stop default below the daemon data directory.
pub const IDLE_STOP_FILE_NAME: &str = "session-idle-stop.json";

/// Temporary name used for the atomic replace of the idle-stop default.
const IDLE_STOP_TEMP_FILE_NAME: &str = "session-idle-stop.json.tmp";

/// Shortest idle time the host accepts, for the default and for a session's own
/// value.
///
/// A stop is not destructive (the worktree stays), but an agent that is idle for
/// a few seconds between turns is normal; one minute keeps a typo from stopping
/// a session that is only between two prompts.
pub(super) const DEFAULT_IDLE_STOP_MIN_SECS: u32 = 60;

/// How long the task waits before it looks again at a session whose stop was held
/// back by an unread attention notification, a waiting caller or a failed stop.
///
/// Those conditions end without any event the task listens to, so a timer is the
/// only way to notice. Thirty seconds bounds the extra idle time a held session
/// accrues without turning the task into a busy loop.
pub(super) const DEFAULT_IDLE_STOP_RECHECK: Duration = Duration::from_secs(30);

/// Value of the `reason` metadata of the notification recorded for a stop.
const NOTIFICATION_REASON: &str = "idle_stop";

/// Provider name of the notification recorded for a stop.
const NOTIFICATION_PROVIDER: &str = "pohunek";

/// Event name of the notification recorded for a stop.
const NOTIFICATION_EVENT: &str = "session.idle_stop";

/// Durable form of the idle-stop default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdleStopRecord {
    /// Absent when no default is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_after_secs: Option<u32>,
}

/// Mutable idle-stop state owned by one [`SessionRegistry`].
#[derive(Debug)]
pub(super) struct IdleStopState {
    /// Durable record, absent when persistence is not configured.
    path: Option<std::path::PathBuf>,
    /// Current default, authoritative for the task and the public API.
    default_after: std::sync::RwLock<Option<u32>>,
    /// Wakes the task when a change may have moved a deadline.
    wake: Notify,
    /// Idle stops still to fail before they begin.
    #[cfg(test)]
    faults: std::sync::atomic::AtomicU32,
    /// Passes of the task that began and that ran to their end.
    #[cfg(test)]
    passes: (std::sync::atomic::AtomicU64, std::sync::atomic::AtomicU64),
}

impl IdleStopState {
    /// Creates state backed by `path`, starting without a default.
    pub(super) fn new(path: Option<std::path::PathBuf>) -> Self {
        Self {
            path,
            default_after: std::sync::RwLock::new(None),
            wake: Notify::new(),
            #[cfg(test)]
            faults: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            passes: (
                std::sync::atomic::AtomicU64::new(0),
                std::sync::atomic::AtomicU64::new(0),
            ),
        }
    }

    fn default_after(&self) -> Option<u32> {
        *self
            .default_after
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where a session's idle duration came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The session's own metadata value.
    Session,
    /// The host default.
    Default,
}

impl Source {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Default => "default",
        }
    }
}

/// A session whose idle time reached its limit.
#[derive(Debug, Clone)]
pub(super) struct Due {
    id: SessionId,
    /// The idle duration the session was configured with.
    limit: Duration,
    source: Source,
}

/// How an attempt to stop a due session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopOutcome {
    /// The session was stopped after being idle for this long.
    Stopped { idle: Duration },
    /// The session is no longer due, or is gone.
    Kept,
    /// Pending attention or a waiting caller holds the stop back.
    Held,
}

/// What one pass over the registry found.
#[derive(Debug, Default)]
pub(super) struct Scan {
    due: Vec<Due>,
    /// Earliest deadline among the sessions that are not due yet.
    next: Option<Instant>,
}

/// How a session stands against its idle limit.
#[derive(Debug, PartialEq, Eq)]
enum Standing {
    /// Not opted in, not idle, or not stoppable.
    Inert,
    /// Idle and opted in; the limit is reached at `deadline`.
    Armed {
        deadline: Instant,
        limit: Duration,
        source: Source,
    },
}

/// Reads how `entry` stands against its idle limit.
///
/// A stored value an older daemon accepted without validation makes the session
/// inert; the caller reports it.
fn standing(
    entry: &SessionEntry,
    default_after: Option<u32>,
    minimum: u32,
) -> Result<Standing, protocol::IdleStopValueError> {
    let Some(value) = entry.info.metadata.get(IDLE_STOP_METADATA_KEY) else {
        return Ok(Standing::Inert);
    };
    let opt_in = IdleStopOptIn::parse(value, minimum)?;
    let (secs, source) = match opt_in {
        IdleStopOptIn::Seconds(secs) => (secs, Source::Session),
        IdleStopOptIn::HostDefault => match default_after {
            Some(secs) => (secs, Source::Default),
            None => return Ok(Standing::Inert),
        },
    };
    let stoppable = entry.info.state == SessionState::Running
        && !entry.stopping
        && entry.info.activity == Some(AgentActivity::Idle)
        && matches!(entry.runtime, RuntimeHandle::Worker(_));
    if !stoppable {
        return Ok(Standing::Inert);
    }
    let limit = Duration::from_secs(u64::from(secs));
    Ok(Standing::Armed {
        deadline: entry.activity_since + limit,
        limit,
        source,
    })
}

/// Validates the idle-stop value of metadata being set, when it names the key.
///
/// # Errors
///
/// Returns `invalid_idle_stop_after` for a value that opts in nothing.
pub(super) fn validate_opt_in(
    metadata: &BTreeMap<String, String>,
    minimum: u32,
) -> Result<(), ProtocolError> {
    let Some(value) = metadata.get(IDLE_STOP_METADATA_KEY) else {
        return Ok(());
    };
    IdleStopOptIn::parse(value, minimum)
        .map(|_| ())
        .map_err(|error| {
            ProtocolError::new(
                ErrorClass::Daemon,
                "invalid_idle_stop_after",
                error.to_string(),
                None,
            )
        })
}

impl SessionRegistry {
    /// Returns the idle-stop default and the minimum the host accepts.
    #[must_use]
    pub fn idle_stop_policy(&self) -> SessionIdleStopResult {
        SessionIdleStopResult {
            default_after_secs: self.inner.idle_stop.default_after(),
            min_after_secs: self.inner.config.idle_stop_min_secs,
        }
    }

    /// Loads the durable idle-stop default.
    ///
    /// A missing record means no default. Call this once at daemon startup so a
    /// corrupt or unsafe record fails the process instead of silently dropping
    /// the default.
    ///
    /// # Errors
    ///
    /// Returns a `runtime` [`ProtocolError`] when the record cannot be read, does
    /// not parse, or holds a value below the minimum.
    pub async fn load_idle_stop_policy(&self) -> Result<SessionIdleStopResult, ProtocolError> {
        let Some(path) = self.inner.idle_stop.path.clone() else {
            return Ok(self.idle_stop_policy());
        };
        let record = blocking(move || read_record(&path)).await?;
        self.check_default(record.default_after_secs)?;
        *self
            .inner
            .idle_stop
            .default_after
            .write()
            .unwrap_or_else(PoisonError::into_inner) = record.default_after_secs;
        self.inner.idle_stop.wake.notify_one();
        Ok(self.idle_stop_policy())
    }

    /// Validates, persists and applies a replacement idle-stop default.
    ///
    /// `0` and an absent value clear the default. The change takes effect for
    /// armed sessions without a restart.
    ///
    /// # Errors
    ///
    /// Returns `invalid_session_policy` for a value below the minimum, and a
    /// `runtime` error when the record cannot be written.
    pub async fn set_idle_stop_policy(
        &self,
        params: SessionIdleStopParams,
    ) -> Result<SessionIdleStopResult, ProtocolError> {
        let default_after_secs = params.default_after_secs.filter(|secs| *secs != 0);
        self.check_default(default_after_secs)?;
        if let Some(path) = self.inner.idle_stop.path.clone() {
            let record = IdleStopRecord { default_after_secs };
            blocking(move || write_record(&path, record)).await?;
        }
        *self
            .inner
            .idle_stop
            .default_after
            .write()
            .unwrap_or_else(PoisonError::into_inner) = default_after_secs;
        self.inner.idle_stop.wake.notify_one();
        Ok(self.idle_stop_policy())
    }

    fn check_default(&self, default_after_secs: Option<u32>) -> Result<(), ProtocolError> {
        let minimum = self.inner.config.idle_stop_min_secs;
        match default_after_secs {
            Some(secs) if secs < minimum => Err(invalid_policy(format!(
                "default_after_secs must be at least {minimum}"
            ))),
            _ => Ok(()),
        }
    }

    /// Holds the lifecycle lock of `id`, parking any stop of the session.
    #[cfg(test)]
    pub(crate) async fn hold_lifecycle_for_test(
        &self,
        id: &SessionId,
    ) -> Box<dyn std::any::Any + Send> {
        Box::new(self.lock_lifecycle(id).await)
    }

    /// Hands the sessions of this registry to a new one over the same store and
    /// workers, the way a daemon restart does: this registry stops managing them
    /// and the new one adopts the live workers.
    #[cfg(test)]
    pub(crate) async fn restarted_for_test(
        &self,
        runtimes: crate::agent::host::RuntimeHost,
    ) -> SessionRegistry {
        let barrier = self.inner.persist_lock.lock().await;
        std::mem::forget(barrier);
        let entries: Vec<_> = self
            .inner
            .sessions
            .lock()
            .await
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in entries {
            entry.cancel_runtime_watchers();
        }
        let restarted = SessionRegistry::new_with_runtimes_and_launcher(
            self.inner.config.clone(),
            runtimes,
            self.inner
                .launcher
                .clone()
                .expect("a registry with a worker launcher"),
            std::sync::Arc::clone(&self.inner.inspector),
        );
        restarted
            .reconcile_workers()
            .await
            .expect("startup reconciliation");
        restarted
    }

    /// Whether a pass of the idle-stop task has begun and not ended.
    #[cfg(test)]
    pub(crate) fn idle_stop_pass_under_way_for_test(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        let passes = &self.inner.idle_stop.passes;
        passes.0.load(SeqCst) > passes.1.load(SeqCst)
    }

    /// Makes the next `count` idle stops fail before they begin; `0` stops the
    /// failures.
    #[cfg(test)]
    pub(crate) fn fail_idle_stops_for_test(&self, count: u32) {
        self.inner
            .idle_stop
            .faults
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }

    /// Returns once the idle-stop task has no pass under way and none began
    /// while the scheduler ran everything else that was ready. The task is not
    /// woken, so a missing wake shows as a stop that never comes.
    #[cfg(test)]
    pub(crate) async fn idle_stop_quiesce_for_test(&self, yields: usize) {
        use std::sync::atomic::Ordering::SeqCst;
        let passes = &self.inner.idle_stop.passes;
        loop {
            let began = passes.0.load(SeqCst);
            for _ in 0..yields {
                tokio::task::yield_now().await;
            }
            if passes.0.load(SeqCst) == began && passes.1.load(SeqCst) == began {
                return;
            }
        }
    }

    /// Stores `value` under the idle-stop key without validating it, the way a
    /// daemon that predates the validation would have.
    #[cfg(test)]
    pub(crate) async fn store_unvalidated_for_test(&self, id: &SessionId, value: &str) {
        let mut sessions = self.inner.sessions.lock().await;
        let entry = sessions.get_mut(id).expect("session entry");
        entry
            .info
            .metadata
            .insert(IDLE_STOP_METADATA_KEY.to_owned(), value.to_owned());
        drop(sessions);
        self.wake_idle_stop();
    }

    /// Tells the idle-stop task that a deadline may have moved.
    pub(super) fn wake_idle_stop(&self) {
        self.inner.idle_stop.wake.notify_one();
    }

    /// Whether a caller waits on `id`: a bounded observation or input wait, or a
    /// turn wait.
    fn has_waiters(&self, id: &SessionId) -> bool {
        let observing = self
            .inner
            .observation_session_waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .is_some_and(|count| *count > 0);
        observing || self.inner.turns.has_waiters(id)
    }

    /// Finds the sessions whose idle limit is reached and the earliest deadline
    /// of the rest.
    async fn scan_idle(&self, now: Instant, reported: &mut HashSet<SessionId>) -> Scan {
        let default_after = self.inner.idle_stop.default_after();
        let minimum = self.inner.config.idle_stop_min_secs;
        let sessions = self.inner.sessions.lock().await;
        let mut scan = Scan::default();
        for (id, entry) in sessions.iter() {
            match standing(entry, default_after, minimum) {
                Ok(Standing::Inert) => {
                    reported.remove(id);
                }
                Ok(Standing::Armed {
                    deadline,
                    limit,
                    source,
                }) => {
                    if deadline <= now {
                        scan.due.push(Due {
                            id: id.clone(),
                            limit,
                            source,
                        });
                    } else {
                        scan.next = Some(scan.next.map_or(deadline, |next| next.min(deadline)));
                    }
                }
                Err(error) => {
                    if reported.insert(id.clone()) {
                        warn!(
                            name: "session.idle_stop.ignored",
                            session_id = %id.0,
                            error = %error,
                            "ignoring a stored idle-stop value of session {{session_id}}: {{error}}"
                        );
                    }
                }
            }
        }
        scan
    }

    /// Stops `due` through the ordinary stop transaction when it is still idle.
    ///
    /// The limit, pending attention and waiting callers are evaluated again
    /// inside the transaction, under the lock that marks the session as
    /// stopping, so activity, an attention record or a waiting caller that
    /// arrived since the scan keeps the session running.
    async fn stop_idle(
        &self,
        due: &Due,
        notifications: &NotificationService,
    ) -> Result<StopOutcome, ProtocolError> {
        if self.inner.daemon_shutdown.is_cancelled() {
            return Ok(StopOutcome::Kept);
        }
        #[cfg(test)]
        if self
            .inner
            .idle_stop
            .faults
            .try_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |left| left.checked_sub(1),
            )
            .is_ok()
        {
            // A stop that fails after it marked the session wakes the task
            // from the rollback.
            self.wake_idle_stop();
            return Err(ProtocolError::new(
                ErrorClass::Daemon,
                "idle_stop_test_fault",
                "the idle stop was made to fail".to_owned(),
                None,
            ));
        }
        self.ensure_not_external_session(&due.id).await?;
        let _guard = self.lock_lifecycle(&due.id).await;
        let default_after = self.inner.idle_stop.default_after();
        let minimum = self.inner.config.idle_stop_min_secs;
        let now = crate::time::now();
        let held = std::sync::atomic::AtomicBool::new(false);
        let idle_millis = std::sync::atomic::AtomicU64::new(0);
        let still_due = |entry: &SessionEntry| {
            let Ok(Standing::Armed { deadline, .. }) = standing(entry, default_after, minimum)
            else {
                return false;
            };
            if deadline > now {
                return false;
            }
            if notifications.has_active_attention(&due.id) || self.has_waiters(&due.id) {
                held.store(true, std::sync::atomic::Ordering::SeqCst);
                return false;
            }
            let idle = now.saturating_duration_since(entry.activity_since);
            idle_millis.store(
                u64::try_from(idle.as_millis()).unwrap_or(u64::MAX),
                std::sync::atomic::Ordering::SeqCst,
            );
            true
        };
        let stopped = self
            .stop_with_intent(
                &due.id,
                super::DesiredState::Stopped,
                super::TransactionKind::Stop,
                Some(&still_due),
            )
            .await;
        match stopped {
            Ok(result) if result.stopped => Ok(StopOutcome::Stopped {
                idle: Duration::from_millis(idle_millis.load(std::sync::atomic::Ordering::SeqCst)),
            }),
            Ok(_) if held.load(std::sync::atomic::Ordering::SeqCst) => Ok(StopOutcome::Held),
            Ok(_) => Ok(StopOutcome::Kept),
            // The session was removed between the scan and the stop.
            Err(error) if error.code == "session_not_found" => Ok(StopOutcome::Kept),
            Err(error) => Err(error),
        }
    }
}

fn read_record(path: &Path) -> Result<IdleStopRecord, ProtocolError> {
    let (directory, name) = open_policy_dir(path, false)?;
    let bytes = match directory.read_file(name, OWNER_PRIVATE_FILE_MODE, MAX_POLICY_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {
            return Ok(IdleStopRecord::default())
        }
        Err(error) => return Err(policy_io_error(path, &error)),
    };
    serde_json::from_slice(&bytes).map_err(|error| {
        policy_error(format!(
            "session idle-stop default at {} is not valid: {error}",
            path.display()
        ))
    })
}

fn write_record(path: &Path, record: IdleStopRecord) -> Result<(), ProtocolError> {
    let (directory, name) = open_policy_dir(path, true)?;
    let mut body = serde_json::to_vec_pretty(&record).map_err(|error| {
        policy_error(format!(
            "session idle-stop default could not be serialized: {error}"
        ))
    })?;
    body.push(b'\n');
    match directory.replace_file(
        name,
        IDLE_STOP_TEMP_FILE_NAME,
        &body,
        OWNER_PRIVATE_FILE_MODE,
    ) {
        Ok(()) => Ok(()),
        Err(AtomicReplaceError::BeforeCommit(error)) => Err(policy_io_error(path, &error)),
        // The new record is already authoritative; only the directory entry's
        // crash durability is unproven, which does not fail the request.
        Err(AtomicReplaceError::CommittedDurabilityUncertain(error)) => {
            warn!(
                path = %path.display(),
                error = %error,
                "session idle-stop default committed but durability is uncertain"
            );
            Ok(())
        }
        Err(error) => Err(policy_error(format!(
            "session idle-stop default at {} could not be replaced: {error}",
            path.display()
        ))),
    }
}

/// Spawns the task that stops opted-in sessions after their idle limit.
///
/// The task is a driver of `tasks`: daemon shutdown cancels it and waits for an
/// in-flight stop, which is one transaction per due session bounded by the
/// worker stop bound.
#[expect(
    clippy::must_use_candidate,
    reason = "the handle only stops the task before daemon shutdown does"
)]
pub fn spawn_idle_stop(
    tasks: &DaemonShutdown,
    sessions: SessionRegistry,
    notifications: NotificationService,
) -> DriverHandle {
    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    tasks.spawn(
        "session_idle_stop",
        Stage::Driver,
        move || shutdown.cancel(),
        async move {
            let recheck = sessions.inner.config.idle_stop_recheck;
            let mut reported = HashSet::new();
            loop {
                #[cfg(test)]
                sessions
                    .inner
                    .idle_stop
                    .passes
                    .0
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let scan = sessions.scan_idle(crate::time::now(), &mut reported).await;
                let mut held = false;
                for due in &scan.due {
                    if task_shutdown.is_cancelled() {
                        return;
                    }
                    held |= !stop_one(&sessions, &notifications, due).await;
                }
                #[cfg(test)]
                sessions
                    .inner
                    .idle_stop
                    .passes
                    .1
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if held {
                    // The rollback of a failed stop wakes the task; that wake is
                    // not news, and the recheck interval paces the retry.
                    tokio::select! {
                        biased;
                        () = sessions.inner.idle_stop.wake.notified() => {}
                        () = std::future::ready(()) => {}
                    }
                }
                let held_until = held.then(|| crate::time::now() + recheck);
                let wake_at = match (scan.next, held_until) {
                    (Some(next), Some(held)) => Some(next.min(held)),
                    (next, held) => next.or(held),
                };
                let sleep = async {
                    match wake_at {
                        Some(at) => {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
                        }
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    () = task_shutdown.cancelled() => return,
                    () = sessions.inner.idle_stop.wake.notified() => {}
                    () = sleep => {}
                }
            }
        },
    )
}

/// Stops one due session unless something holds it back; returns `false` when
/// the session must be looked at again.
async fn stop_one(
    sessions: &SessionRegistry,
    notifications: &NotificationService,
    due: &Due,
) -> bool {
    if notifications.has_active_attention(&due.id) || sessions.has_waiters(&due.id) {
        debug!(
            name: "session.idle_stop.held",
            session_id = %due.id.0,
            "idle stop of session {{session_id}} is held back by pending attention or a waiting caller"
        );
        return false;
    }
    match sessions.stop_idle(due, notifications).await {
        Ok(StopOutcome::Stopped { idle }) => {
            record_stop(notifications, due, idle);
            true
        }
        Ok(StopOutcome::Kept) => true,
        Ok(StopOutcome::Held) => false,
        Err(error) => {
            warn!(
                name: "session.idle_stop.failed",
                session_id = %due.id.0,
                error = %error,
                "idle stop of session {{session_id}} failed: {{error}}"
            );
            false
        }
    }
}

/// Logs a stop and records it as a `system` notification.
fn record_stop(notifications: &NotificationService, due: &Due, idle: Duration) {
    let idle_secs = idle.as_secs();
    info!(
        name: "session.idle_stop.stopped",
        session_id = %due.id.0,
        session.idle_secs = idle_secs,
        session.idle_limit_secs = due.limit.as_secs(),
        session.idle_source = due.source.as_str(),
        "stopped session {{session_id}} after {{session.idle_secs}} idle seconds ({{session.idle_source}} limit of {{session.idle_limit_secs}} seconds)"
    );
    let source_id = format!("idle-stop:{}:{}", due.id.0, crate::time::now_rfc3339());
    let params = NotificationCreateParams {
        source: NotificationSource {
            provider: NOTIFICATION_PROVIDER.to_owned(),
            provider_event: NOTIFICATION_EVENT.to_owned(),
            host_local_source_id: source_id.clone(),
        },
        kind: NotificationKind::System,
        severity: NotificationSeverity::Info,
        title: "Session stopped while idle".to_owned(),
        body: format!(
            "Session {} was stopped after {idle_secs} idle seconds. Its worktree is kept.",
            due.id.0
        ),
        metadata: BTreeMap::from([
            ("reason".to_owned(), NOTIFICATION_REASON.to_owned()),
            ("summary".to_owned(), format!("idle_secs={idle_secs}")),
        ]),
        session_id: Some(due.id.clone()),
        agent_kind: None,
        source_id: Some(source_id),
        dedupe_key: None,
        project_id: None,
        retryable_admission: false,
    };
    match notifications.create(params) {
        Ok(_) => {}
        Err(NotificationError::KindDisabled { .. }) => debug!(
            name: "session.idle_stop.notification_disabled",
            session_id = %due.id.0,
            "the notification policy drops the idle-stop record of session {{session_id}}"
        ),
        Err(error) => warn!(
            name: "session.idle_stop.notification_failed",
            session_id = %due.id.0,
            error = %error,
            "recording the idle stop of session {{session_id}} failed: {{error}}"
        ),
    }
}
