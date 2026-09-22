//! Daemon-owned automatic session retention.
//!
//! Logical sessions that can never come back — terminal ones, and ones whose
//! PTY runtime was lost — stay in the registry forever unless something evicts
//! them, which also leaves their pohunek-owned worktrees on disk. This module
//! owns the policy that ages them out, the pure selection rule, and the
//! background task that applies it.

// Rust guideline compliant 2026-09-22

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, PoisonError, RwLock},
    time::Duration,
};

use pohunek_platform::filesystem::{AtomicReplaceError, FsError, TrustedDir};
use protocol::{
    ErrorClass, ProtocolError, RuntimeState, SessionInfo, SessionRetentionCandidate,
    SessionRetentionHold, SessionRetentionParams, SessionRetentionPolicy, SessionRetentionReason,
    SessionRetentionResult,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::SessionRegistry;
use crate::worktree::WorktreeWork;

/// File name of the durable session policy below the daemon data directory.
pub const POLICY_FILE_NAME: &str = "session-policy.json";

/// Temporary name used for the atomic policy replace.
const POLICY_TEMP_FILE_NAME: &str = "session-policy.json.tmp";

/// Exact mode required of the policy file and accepted on read.
const OWNER_PRIVATE_FILE_MODE: u32 = 0o600;

/// Exact mode required of the data directory holding the policy file.
const OWNER_PRIVATE_DIRECTORY_MODE: u32 = 0o700;

/// Maximum accepted size of the persisted policy document.
///
/// The document is a handful of scalar fields; 64 KiB is orders of magnitude
/// above any legitimate encoding and bounds memory if the file is replaced by
/// something larger.
const MAX_POLICY_BYTES: usize = 64 * 1024;

/// Lower bound accepted for [`SessionRetentionPolicy::sweep_interval_secs`].
///
/// One minute keeps a misconfigured policy from turning the sweep into a busy
/// loop that re-reads the whole registry continuously.
const MIN_SWEEP_INTERVAL_SECS: u32 = 60;

/// Lower bound accepted for either retention TTL.
///
/// Removal deletes session logs and any pohunek-owned worktree, so a one-hour
/// floor keeps a typo from configuring a policy that evicts sessions the moment
/// they stop.
const MIN_TTL_SECS: u32 = 3_600;

/// Maximum number of selected sessions reported by one sweep result.
///
/// The counts in [`SessionRetentionResult`] stay exact; only the per-session
/// detail is truncated, so a host with thousands of stale sessions cannot
/// produce a control line above the protocol's framing limit.
const MAX_REPORTED_CANDIDATES: usize = 200;

/// Maximum worktree safety probes one sweep performs.
///
/// Each probe runs up to three bounded `git` commands against one checkout, so an
/// unbounded count would let a host with hundreds of stale worktrees hold the
/// sweep lock far longer than the shutdown budget. It is comfortably above the
/// default per-sweep removal cap, and a candidate past it is held as unknown
/// rather than removed, so the bound only delays cleanup to the next sweep.
pub(super) const MAX_WORKTREE_PROBES_PER_SWEEP: usize = 64;

/// Maximum time to wait for a running sweep during daemon shutdown.
///
/// A sweep performs bounded local filesystem work per session. Five seconds
/// matches the notification retention shutdown budget while bounding exit.
const RETENTION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Mutable retention state owned by one [`SessionRegistry`].
#[derive(Debug)]
pub(super) struct RetentionState {
    /// Durable policy document, absent when persistence is not configured.
    path: Option<PathBuf>,
    /// Current policy, authoritative for both the task and the public API.
    policy: RwLock<SessionRetentionPolicy>,
    /// Serializes sweeps so an automatic and a manual one cannot interleave
    /// their removals and double-report the same session.
    sweep_lock: tokio::sync::Mutex<()>,
}

impl RetentionState {
    /// Creates retention state backed by `path`, starting from the default policy.
    pub(super) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            policy: RwLock::new(SessionRetentionPolicy::default()),
            sweep_lock: tokio::sync::Mutex::new(()),
        }
    }
}

impl SessionRegistry {
    /// Returns the current session retention policy.
    #[must_use]
    pub fn retention_policy(&self) -> SessionRetentionPolicy {
        *self
            .inner
            .retention
            .policy
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Loads the durable retention policy into the registry.
    ///
    /// A missing document yields the documented default. Call this once at
    /// daemon startup so a corrupt or unsafe policy file fails the process
    /// instead of silently reverting to defaults.
    ///
    /// # Errors
    ///
    /// Returns a `runtime` [`ProtocolError`] when the document cannot be read,
    /// does not parse, or fails validation.
    pub async fn load_retention_policy(&self) -> Result<SessionRetentionPolicy, ProtocolError> {
        let Some(path) = self.inner.retention.path.clone() else {
            return Ok(self.retention_policy());
        };
        let policy = blocking(move || read_policy(&path)).await?;
        *self
            .inner
            .retention
            .policy
            .write()
            .unwrap_or_else(PoisonError::into_inner) = policy;
        Ok(policy)
    }

    /// Validates, persists, and applies a replacement retention policy.
    ///
    /// The new policy takes effect on the next sweep; no daemon restart is
    /// needed. When persistence is not configured the policy is applied in
    /// memory only.
    ///
    /// # Errors
    ///
    /// Returns a `bad_request` [`ProtocolError`] for a policy outside the
    /// accepted bounds, and a `runtime` error when the document cannot be
    /// written.
    pub async fn set_retention_policy(
        &self,
        policy: SessionRetentionPolicy,
    ) -> Result<SessionRetentionPolicy, ProtocolError> {
        validate_policy(&policy)?;
        if let Some(path) = self.inner.retention.path.clone() {
            blocking(move || write_policy(&path, &policy)).await?;
        }
        *self
            .inner
            .retention
            .policy
            .write()
            .unwrap_or_else(PoisonError::into_inner) = policy;
        Ok(policy)
    }

    /// Runs one retention sweep against the current policy.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] only when the sweep itself cannot run; a
    /// removal that fails is counted in [`SessionRetentionResult::failed`] and
    /// leaves that session in place.
    pub async fn sweep_retention(
        &self,
        params: &SessionRetentionParams,
    ) -> Result<SessionRetentionResult, ProtocolError> {
        self.sweep_retention_at(params, OffsetDateTime::now_utc())
            .await
    }

    /// Runs one retention sweep as if `now` were the current time.
    pub(crate) async fn sweep_retention_at(
        &self,
        params: &SessionRetentionParams,
        now: OffsetDateTime,
    ) -> Result<SessionRetentionResult, ProtocolError> {
        let _guard = self.inner.retention.sweep_lock.lock().await;
        let policy = self.retention_policy();
        let sessions = self.list_raw().await;
        let examined = u32::try_from(sessions.len()).unwrap_or(u32::MAX);
        let mut candidates = select_candidates(&sessions, &policy, now);
        self.apply_worktree_holds(&mut candidates).await;
        let held = count(
            candidates
                .iter()
                .filter(|candidate| candidate.hold.is_some()),
        );
        let eligible = count(
            candidates
                .iter()
                .filter(|candidate| candidate.hold.is_none()),
        );
        let budget = removal_budget(&policy, params.limit);

        let mut removed = 0_u32;
        let mut worktrees_cleaned = 0_u32;
        let mut worktrees_failed = 0_u32;
        let mut failed = 0_u32;
        if !params.dry_run {
            for candidate in candidates
                .iter_mut()
                .filter(|candidate| candidate.hold.is_none())
                .take(budget)
            {
                match self.remove(&candidate.session_id).await {
                    // The removal reports the checkouts it really deleted, so a
                    // `git worktree remove` failure cannot inflate the count.
                    Ok(result) => {
                        worktrees_cleaned =
                            worktrees_cleaned.saturating_add(result.worktrees_removed);
                        worktrees_failed = worktrees_failed.saturating_add(result.worktrees_failed);
                        if result.worktrees_failed > 0 {
                            warn!(
                                session_id = %candidate.session_id.0,
                                session.worktrees_failed = result.worktrees_failed,
                                "session retention removed a session but left its worktree on disk"
                            );
                        }
                        if result.removed {
                            candidate.removed = true;
                            removed = removed.saturating_add(1);
                        }
                        // Otherwise a concurrent remove already evicted this
                        // session; it is gone either way.
                    }
                    Err(error) => {
                        failed = failed.saturating_add(1);
                        warn!(
                            session_id = %candidate.session_id.0,
                            error = %error,
                            "session retention could not remove a selected session"
                        );
                    }
                }
            }
        }

        candidates.truncate(MAX_REPORTED_CANDIDATES);
        Ok(SessionRetentionResult {
            dry_run: params.dry_run,
            examined,
            eligible,
            held,
            removed,
            worktrees_cleaned,
            worktrees_failed,
            failed,
            sessions: candidates,
        })
    }

    /// Marks every candidate the sweep must not remove because its owned
    /// worktree holds work, or because that worktree could not be inspected.
    ///
    /// An automatic sweep is unattended and `git worktree remove --force`
    /// deletes the checkout without asking, so a selected session only stays
    /// removable while its worktree is provably empty of work. `session rm` is an
    /// explicit operator action and is deliberately not subject to this rule.
    ///
    /// At most [`MAX_WORKTREE_PROBES_PER_SWEEP`] checkouts are inspected per
    /// sweep; anything left over is held as unknown.
    pub(super) async fn apply_worktree_holds(&self, candidates: &mut [SessionRetentionCandidate]) {
        let Some(worktree) = self.inner.worktree.clone() else {
            return;
        };
        let mut probes = 0_usize;
        for candidate in candidates
            .iter_mut()
            .filter(|candidate| candidate.worktree_path.is_some())
        {
            if probes >= MAX_WORKTREE_PROBES_PER_SWEEP {
                // Unprobed means unproven, which means kept. Candidates are
                // ordered oldest first, so the next sweep starts with these.
                candidate.hold = Some(SessionRetentionHold::WorktreeUnknown);
                debug!(
                    session_id = %candidate.session_id.0,
                    "session retention kept a session it had no probe budget left to inspect"
                );
                continue;
            }
            probes += 1;
            let manager = Arc::clone(&worktree);
            let session_id = candidate.session_id.0.clone();
            let work = match tokio::task::spawn_blocking(move || manager.unsaved_work(&session_id))
                .await
            {
                Ok(work) => work,
                Err(_join) => WorktreeWork::Held {
                    hold: SessionRetentionHold::WorktreeUnknown,
                    detail: "worktree inspection task panicked".to_owned(),
                },
            };
            if let WorktreeWork::Held { hold, detail } = work {
                candidate.hold = Some(hold);
                info!(
                    session_id = %candidate.session_id.0,
                    session.hold = hold.as_str(),
                    detail = %detail,
                    "session retention kept a session whose worktree holds work"
                );
            }
        }
    }
}

/// Background task that applies the current session retention policy.
#[derive(Debug)]
pub struct SessionRetentionTask {
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
}

impl SessionRetentionTask {
    /// Spawns automatic session retention for `sessions`.
    ///
    /// The first sweep runs immediately so an upgraded daemon cleans stale
    /// records without waiting a full interval. Both the enable flag and the
    /// interval are re-read from the current policy every cycle, so a policy
    /// change takes effect on the next sweep without a restart.
    #[must_use]
    pub fn spawn(sessions: SessionRegistry) -> Self {
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let handle = tokio::spawn(async move {
            loop {
                if sessions.retention_policy().enabled {
                    // The sweep runs in its own task so a panic anywhere in the
                    // removal chain arrives here as a `JoinError` instead of
                    // unwinding this loop and killing automatic retention until
                    // the next daemon restart.
                    let registry = sessions.clone();
                    report_sweep(
                        tokio::spawn(async move {
                            registry
                                .sweep_retention(&SessionRetentionParams::default())
                                .await
                        })
                        .await,
                    );
                }

                let interval =
                    Duration::from_secs(u64::from(sessions.retention_policy().sweep_interval_secs));
                tokio::select! {
                    () = task_shutdown.cancelled() => break,
                    () = tokio::time::sleep(interval) => {}
                }
            }
        });
        Self { shutdown, handle }
    }

    /// Stops the task and waits for any in-flight sweep to finish.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        match tokio::time::timeout(RETENTION_SHUTDOWN_TIMEOUT, self.handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(error = %err, "session retention task failed during shutdown");
            }
            Err(_) => {
                warn!("session retention task did not finish within the shutdown timeout");
            }
        }
    }
}

/// Logs the outcome of one automatic sweep, including a panicked one.
///
/// Returns normally for every outcome: the caller is a background loop whose next
/// tick must still run, so nothing here may propagate.
fn report_sweep(outcome: Result<Result<SessionRetentionResult, ProtocolError>, JoinError>) {
    match outcome {
        Ok(Ok(result)) => {
            // Every sweep leaves evidence for backtesting; only a sweep that
            // changed something is worth INFO.
            if result.removed > 0 || result.failed > 0 || result.worktrees_failed > 0 {
                info!(
                    session.pruned = result.removed,
                    session.worktrees_cleaned = result.worktrees_cleaned,
                    session.worktrees_failed = result.worktrees_failed,
                    session.eligible = result.eligible,
                    session.held = result.held,
                    session.examined = result.examined,
                    session.failed = result.failed,
                    "completed automatic session retention"
                );
            } else {
                debug!(
                    session.pruned = result.removed,
                    session.worktrees_cleaned = result.worktrees_cleaned,
                    session.worktrees_failed = result.worktrees_failed,
                    session.eligible = result.eligible,
                    session.held = result.held,
                    session.examined = result.examined,
                    session.failed = result.failed,
                    "completed automatic session retention"
                );
            }
        }
        Ok(Err(error)) => {
            warn!(error = %error, "automatic session retention failed");
        }
        Err(error) => {
            warn!(error = %error, "automatic session retention task panicked");
        }
    }
}

/// Returns the sessions this policy selects, oldest first.
///
/// The order matters: when the removal budget is smaller than the selection,
/// the sweep prunes the sessions that have been unusable the longest.
fn select_candidates(
    sessions: &[SessionInfo],
    policy: &SessionRetentionPolicy,
    now: OffsetDateTime,
) -> Vec<SessionRetentionCandidate> {
    let mut candidates: Vec<SessionRetentionCandidate> = sessions
        .iter()
        .filter_map(|session| candidate_for(session, policy, now))
        .collect();
    candidates.sort_by(|left, right| {
        right
            .age_secs
            .cmp(&left.age_secs)
            .then_with(|| left.session_id.0.cmp(&right.session_id.0))
    });
    candidates
}

/// Returns the retention candidate for `session`, or `None` to keep it.
///
/// Every unresolved case keeps the session: an entry the daemon does not own, a
/// runtime whose identity is disputed, a state that is still live or resumable
/// right now, and an unreadable timestamp all fall through to `None`.
fn candidate_for(
    session: &SessionInfo,
    policy: &SessionRetentionPolicy,
    now: OffsetDateTime,
) -> Option<SessionRetentionCandidate> {
    // An observe-only entry has no daemon-owned resources to reclaim, and a
    // peer shape that does not state ownership is treated as not ours.
    if session.external != Some(false) {
        return None;
    }
    // Same eligibility rule the GUI offers a remove action for; it rejects a
    // conflicting or protocol-incompatible runtime.
    if !session.can_remove() {
        return None;
    }
    let (reason, timestamp) = retention_reason(session)?;
    let age_secs = age_secs(timestamp, now, &session.id.0)?;
    (age_secs >= u64::from(reason.ttl_secs(policy))).then(|| SessionRetentionCandidate {
        session_id: session.id.clone(),
        reason,
        age_secs,
        worktree_path: session.worktree_path.clone(),
        // Filled in by `apply_worktree_holds`, which needs blocking git access
        // this pure selection deliberately has no part in.
        hold: None,
        removed: false,
    })
}

/// Counts an iterator into one of the sweep result's `u32` counters.
fn count<T>(items: impl Iterator<Item = T>) -> u32 {
    u32::try_from(items.count()).unwrap_or(u32::MAX)
}

/// Returns the rule that applies to `session` and the timestamp it ages from.
fn retention_reason(session: &SessionInfo) -> Option<(SessionRetentionReason, &str)> {
    if session.state.is_terminal() {
        // A terminal session is finished regardless of its runtime: `resume`
        // refuses it, so the shorter terminal grace is the right one.
        return Some((SessionRetentionReason::Terminal, &session.updated_at));
    }
    let runtime = session.runtime.as_ref()?;
    if runtime.state != RuntimeState::Lost {
        return None;
    }
    // A lost runtime ages from the last time the daemon reached it, which is
    // when it actually became unusable rather than when its record last moved.
    let timestamp = runtime
        .last_connected_at
        .as_deref()
        .unwrap_or(&session.updated_at);
    Some((SessionRetentionReason::Lost, timestamp))
}

/// Returns the age of `timestamp` at `now`, or `None` when it cannot be read.
///
/// A timestamp in the future yields zero rather than a negative age, so clock
/// skew can only delay a removal.
fn age_secs(timestamp: &str, now: OffsetDateTime, session_id: &str) -> Option<u64> {
    match OffsetDateTime::parse(timestamp, &Rfc3339) {
        Ok(parsed) => Some((now - parsed).whole_seconds().try_into().unwrap_or(0)),
        Err(error) => {
            warn!(
                session_id = %session_id,
                error = %error,
                "session retention kept a session with an unreadable timestamp"
            );
            None
        }
    }
}

/// Returns how many removals this sweep may perform.
///
/// A caller-supplied limit can only lower the policy ceiling, so a manual sweep
/// cannot exceed the blast radius the operator configured.
fn removal_budget(policy: &SessionRetentionPolicy, requested: Option<u32>) -> usize {
    let limit = requested.map_or(policy.max_removals_per_sweep, |requested| {
        requested.min(policy.max_removals_per_sweep)
    });
    usize::try_from(limit).unwrap_or(usize::MAX)
}

/// Rejects a policy whose values would make the sweep unsafe or degenerate.
fn validate_policy(policy: &SessionRetentionPolicy) -> Result<(), ProtocolError> {
    if policy.sweep_interval_secs < MIN_SWEEP_INTERVAL_SECS {
        return Err(invalid_policy(format!(
            "sweep_interval_secs must be at least {MIN_SWEEP_INTERVAL_SECS}"
        )));
    }
    if policy.terminal_ttl_secs < MIN_TTL_SECS {
        return Err(invalid_policy(format!(
            "terminal_ttl_secs must be at least {MIN_TTL_SECS}"
        )));
    }
    if policy.lost_ttl_secs < MIN_TTL_SECS {
        return Err(invalid_policy(format!(
            "lost_ttl_secs must be at least {MIN_TTL_SECS}"
        )));
    }
    if policy.max_removals_per_sweep == 0 {
        return Err(invalid_policy(
            "max_removals_per_sweep must be greater than zero".to_owned(),
        ));
    }
    Ok(())
}

fn invalid_policy(message: String) -> ProtocolError {
    ProtocolError::new(ErrorClass::Runtime, "invalid_session_policy", message, None)
}

/// Reads the durable policy, falling back to the default when absent.
fn read_policy(path: &Path) -> Result<SessionRetentionPolicy, ProtocolError> {
    let (directory, name) = open_policy_dir(path, false)?;
    let bytes = match directory.read_file(name, OWNER_PRIVATE_FILE_MODE, MAX_POLICY_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.io_kind() == Some(io::ErrorKind::NotFound) => {
            return Ok(SessionRetentionPolicy::default())
        }
        Err(error) => return Err(policy_io_error(path, &error)),
    };
    let policy: SessionRetentionPolicy = serde_json::from_slice(&bytes).map_err(|error| {
        policy_error(format!(
            "session policy at {} is not valid: {error}",
            path.display()
        ))
    })?;
    validate_policy(&policy)?;
    Ok(policy)
}

/// Atomically replaces the durable policy document.
fn write_policy(path: &Path, policy: &SessionRetentionPolicy) -> Result<(), ProtocolError> {
    let (directory, name) = open_policy_dir(path, true)?;
    let mut body = serde_json::to_vec_pretty(policy).map_err(|error| {
        policy_error(format!("session policy could not be serialized: {error}"))
    })?;
    body.push(b'\n');
    match directory.replace_file(name, POLICY_TEMP_FILE_NAME, &body, OWNER_PRIVATE_FILE_MODE) {
        Ok(()) => Ok(()),
        Err(AtomicReplaceError::BeforeCommit(error)) => Err(policy_io_error(path, &error)),
        // The new document is already authoritative; only the directory entry's
        // crash durability is unproven, which does not fail the request.
        Err(AtomicReplaceError::CommittedDurabilityUncertain(error)) => {
            warn!(
                path = %path.display(),
                error = %error,
                "session policy committed but durability is uncertain"
            );
            Ok(())
        }
        Err(error) => Err(policy_error(format!(
            "session policy at {} could not be replaced: {error}",
            path.display()
        ))),
    }
}

/// Opens the trusted directory holding the policy document.
fn open_policy_dir(path: &Path, create: bool) -> Result<(TrustedDir, &str), ProtocolError> {
    let parent = path.parent().ok_or_else(|| {
        policy_error(format!(
            "session policy path {} has no parent",
            path.display()
        ))
    })?;
    let directory = if create {
        TrustedDir::open_or_create_absolute(parent, OWNER_PRIVATE_DIRECTORY_MODE)
    } else {
        TrustedDir::open_absolute(parent, OWNER_PRIVATE_DIRECTORY_MODE)
    }
    .map_err(|error| policy_io_error(parent, &error))?;
    // The policy file name is a crate constant, so the only reason this is not
    // the expected name is a caller passing a different path.
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| {
            policy_error(format!(
                "session policy path {} has no file name",
                path.display()
            ))
        })?;
    Ok((directory, name))
}

fn policy_io_error(path: &Path, error: &FsError) -> ProtocolError {
    policy_error(format!(
        "session policy at {} is not accessible: {error}",
        path.display()
    ))
}

fn policy_error(message: String) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_policy_store_error",
        message,
        None,
    )
}

/// Runs blocking policy I/O off the async runtime.
async fn blocking<T, F>(work: F) -> Result<T, ProtocolError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ProtocolError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_join| policy_error("session policy task panicked".to_owned()))?
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use protocol::{
        AgentKind, RuntimeGeneration, RuntimeState, SessionCapabilities, SessionId, SessionInfo,
        SessionRetentionPolicy, SessionRetentionReason, SessionRuntime, SessionState, StateSource,
    };
    use time::{format_description::well_known::Rfc3339, Duration as TimeDuration, OffsetDateTime};

    use tokio::task::JoinError;

    use super::{
        invalid_policy, read_policy, removal_budget, report_sweep, select_candidates,
        validate_policy, write_policy, POLICY_FILE_NAME,
    };

    /// The policy store requires an owner-private directory, so fixtures create one.
    fn private_policy_root() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("create policy fixture root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
            .expect("set owner-private fixture mode");
        root
    }

    /// Fixed reference instant so every age in these tests is exact.
    fn now() -> OffsetDateTime {
        OffsetDateTime::parse("2026-09-22T12:00:00Z", &Rfc3339).expect("fixed now")
    }

    fn stamp(age: TimeDuration) -> String {
        (now() - age).format(&Rfc3339).expect("format timestamp")
    }

    fn session(id: &str, state: SessionState, updated_at: String) -> SessionInfo {
        SessionInfo {
            id: SessionId(id.to_owned()),
            external: Some(false),
            capabilities: SessionCapabilities::default(),
            name: None,
            agent: "shell".to_owned(),
            agent_base: AgentKind::Shell,
            cwd: PathBuf::from("/tmp"),
            cwd_source: None,
            pid: 1,
            runtime: None,
            cols: 80,
            rows: 24,
            state,
            state_source: StateSource::Process,
            activity: None,
            subagents: Vec::new(),
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: None,
            native_session_path: None,
            project_id: None,
            project_label: None,
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            metadata: BTreeMap::new(),
            created_at: updated_at.clone(),
            updated_at,
            exit_code: None,
        }
    }

    fn runtime(state: RuntimeState, last_connected_at: Option<String>) -> SessionRuntime {
        SessionRuntime {
            state,
            runtime_generation: RuntimeGeneration::new(1),
            worker_id: Some("w-1".to_owned()),
            runtime_id: Some("r-1".to_owned()),
            started_at: None,
            last_connected_at,
            loss_reason: (state == RuntimeState::Lost).then(|| "worker_unavailable".to_owned()),
        }
    }

    fn policy() -> SessionRetentionPolicy {
        SessionRetentionPolicy {
            enabled: true,
            ..SessionRetentionPolicy::default()
        }
    }

    #[test]
    fn selects_a_terminal_session_past_its_ttl_and_keeps_a_newer_one() {
        let policy = policy();
        let old = session(
            "s-old",
            SessionState::Done,
            stamp(TimeDuration::seconds(
                i64::from(policy.terminal_ttl_secs) + 1,
            )),
        );
        let fresh = session(
            "s-fresh",
            SessionState::Stopped,
            stamp(TimeDuration::seconds(
                i64::from(policy.terminal_ttl_secs) - 1,
            )),
        );

        let selected = select_candidates(&[old, fresh], &policy, now());

        assert_eq!(selected.len(), 1, "{selected:?}");
        assert_eq!(selected[0].session_id.0, "s-old");
        assert_eq!(selected[0].reason, SessionRetentionReason::Terminal);
    }

    #[test]
    fn selects_a_lost_session_only_after_the_longer_lost_grace() {
        let policy = policy();
        let between = TimeDuration::seconds(i64::from(policy.terminal_ttl_secs) + 1);
        let past_lost = TimeDuration::seconds(i64::from(policy.lost_ttl_secs) + 1);

        let mut younger = session("s-lost-young", SessionState::Running, stamp(between));
        younger.runtime = Some(runtime(RuntimeState::Lost, Some(stamp(between))));
        let mut older = session("s-lost-old", SessionState::Running, stamp(past_lost));
        older.runtime = Some(runtime(RuntimeState::Lost, Some(stamp(past_lost))));
        older.worktree_path = Some(PathBuf::from("/data/worktrees/s-lost-old"));

        let selected = select_candidates(&[younger, older], &policy, now());

        assert_eq!(selected.len(), 1, "{selected:?}");
        assert_eq!(selected[0].session_id.0, "s-lost-old");
        assert_eq!(selected[0].reason, SessionRetentionReason::Lost);
        assert_eq!(
            selected[0].worktree_path.as_deref(),
            Some(std::path::Path::new("/data/worktrees/s-lost-old"))
        );
    }

    #[test]
    fn never_selects_external_conflicting_incompatible_or_live_sessions() {
        let policy = policy();
        let ancient = stamp(TimeDuration::seconds(i64::from(policy.lost_ttl_secs) * 10));

        let mut external = session("s-external", SessionState::Done, ancient.clone());
        external.external = Some(true);

        let mut unknown_origin = session("s-unknown", SessionState::Done, ancient.clone());
        unknown_origin.external = None;

        let mut conflict = session("s-conflict", SessionState::Done, ancient.clone());
        conflict.runtime = Some(runtime(RuntimeState::Conflict, Some(ancient.clone())));

        let mut incompatible = session("s-incompatible", SessionState::Done, ancient.clone());
        incompatible.runtime = Some(runtime(RuntimeState::Incompatible, Some(ancient.clone())));

        let mut live = session("s-live", SessionState::Running, ancient.clone());
        live.runtime = Some(runtime(RuntimeState::Live, Some(ancient.clone())));

        let mut starting = session("s-starting", SessionState::Starting, ancient.clone());
        starting.runtime = Some(runtime(RuntimeState::Starting, Some(ancient.clone())));

        let mut reconnecting = session("s-reconnecting", SessionState::Running, ancient.clone());
        reconnecting.runtime = Some(runtime(RuntimeState::Reconnecting, Some(ancient.clone())));

        let mut unreadable = session("s-unreadable", SessionState::Done, "not-a-date".to_owned());
        unreadable.runtime = None;

        let selected = select_candidates(
            &[
                external,
                unknown_origin,
                conflict,
                incompatible,
                live,
                starting,
                reconnecting,
                unreadable,
            ],
            &policy,
            now(),
        );

        assert!(selected.is_empty(), "{selected:?}");
    }

    #[test]
    fn orders_selection_oldest_first() {
        let policy = policy();
        let older = session(
            "s-b",
            SessionState::Done,
            stamp(TimeDuration::seconds(
                i64::from(policy.terminal_ttl_secs) * 3,
            )),
        );
        let newer = session(
            "s-a",
            SessionState::Done,
            stamp(TimeDuration::seconds(
                i64::from(policy.terminal_ttl_secs) + 5,
            )),
        );

        let selected = select_candidates(&[newer, older], &policy, now());

        let ids: Vec<&str> = selected
            .iter()
            .map(|candidate| candidate.session_id.0.as_str())
            .collect();
        assert_eq!(ids, ["s-b", "s-a"]);
    }

    #[test]
    fn a_future_timestamp_never_ages_a_session_out() {
        let policy = policy();
        let future = session(
            "s-future",
            SessionState::Done,
            stamp(TimeDuration::seconds(-600)),
        );

        assert!(select_candidates(&[future], &policy, now()).is_empty());
    }

    #[test]
    fn removal_budget_caps_a_request_at_the_policy_ceiling() {
        let policy = SessionRetentionPolicy {
            max_removals_per_sweep: 5,
            ..SessionRetentionPolicy::default()
        };

        assert_eq!(removal_budget(&policy, None), 5);
        assert_eq!(removal_budget(&policy, Some(2)), 2);
        assert_eq!(removal_budget(&policy, Some(500)), 5);
    }

    #[test]
    fn validation_rejects_degenerate_policies() {
        let base = SessionRetentionPolicy::default();
        validate_policy(&base).expect("the shipped default policy must validate");
        assert!(validate_policy(&SessionRetentionPolicy {
            sweep_interval_secs: 1,
            ..base
        })
        .is_err());
        assert!(validate_policy(&SessionRetentionPolicy {
            terminal_ttl_secs: 0,
            ..base
        })
        .is_err());
        assert!(validate_policy(&SessionRetentionPolicy {
            lost_ttl_secs: 0,
            ..base
        })
        .is_err());
        assert!(validate_policy(&SessionRetentionPolicy {
            max_removals_per_sweep: 0,
            ..base
        })
        .is_err());
    }

    #[test]
    fn a_missing_document_reads_as_the_conservative_default() {
        let dir = private_policy_root();
        let path = dir.path().join(POLICY_FILE_NAME);

        let policy = read_policy(&path).expect("read missing policy");

        assert_eq!(policy, SessionRetentionPolicy::default());
        assert!(!policy.enabled, "automatic sweeps must be opt-in");
    }

    #[test]
    fn a_written_policy_round_trips() {
        let dir = private_policy_root();
        let path = dir.path().join(POLICY_FILE_NAME);
        let stored = SessionRetentionPolicy {
            enabled: true,
            sweep_interval_secs: 900,
            terminal_ttl_secs: 7_200,
            lost_ttl_secs: 14_400,
            max_removals_per_sweep: 3,
        };

        write_policy(&path, &stored).expect("write policy");

        assert_eq!(read_policy(&path).expect("read policy"), stored);
    }

    #[test]
    fn a_corrupt_document_fails_instead_of_falling_back() {
        let dir = private_policy_root();
        let path = dir.path().join(POLICY_FILE_NAME);
        std::fs::write(&path, b"{ not json").expect("write corrupt policy");

        read_policy(&path).expect_err("a corrupt policy file must not parse");
    }

    /// A sweep that panics must not take the background loop with it: the loop
    /// awaits the sweep as its own task and hands the `JoinError` to
    /// `report_sweep`, which has to return normally so the next tick still runs.
    #[tokio::test]
    async fn a_panicking_sweep_is_reported_without_unwinding_the_caller() {
        let outcome = tokio::spawn(async { panic!("sweep exploded") }).await;
        assert!(
            outcome.as_ref().err().is_some_and(JoinError::is_panic),
            "the fixture must produce a real panic JoinError"
        );

        report_sweep(outcome);
    }

    #[tokio::test]
    async fn a_failed_sweep_is_reported_without_unwinding_the_caller() {
        report_sweep(Ok(Err(invalid_policy("sweep refused".to_owned()))));
    }
}
