//! In-memory session registry and supervisor.
//!
//! Milestone 3 keeps session metadata in memory only. Each session owns a PTY
//! handle and has a watcher task that records process exit.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use protocol::{
    event, AgentKind, ErrorClass, Event, ProtocolError, SessionId, SessionInfo, SessionNewParams,
    SessionState, SessionStopResult, StateSource,
};
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::pty::{PtyCommand, PtyError, PtyExit, PtyHandle};

const DEFAULT_ATTACH_TOKEN_TTL: Duration = Duration::from_secs(10);

/// Shell command configuration used for `AgentKind::Shell`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommand {
    program: String,
    args: Vec<String>,
}

impl ShellCommand {
    /// Build a shell command from a program and arguments.
    pub fn new<P, I, A>(program: P, args: I) -> Self
    where
        P: Into<String>,
        I: IntoIterator<Item = A>,
        A: Into<String>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    fn to_pty_command(&self, cwd: PathBuf, cols: u16, rows: u16) -> PtyCommand {
        PtyCommand {
            program: self.program.clone(),
            args: self.args.clone(),
            cwd,
            cols,
            rows,
        }
    }
}

impl Default for ShellCommand {
    fn default() -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        Self::new(shell, std::iter::empty::<String>())
    }
}

/// Runtime configuration for the in-memory registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRegistryConfig {
    /// Command used for `AgentKind::Shell`.
    pub shell_command: ShellCommand,
    /// Grace period after SIGTERM before falling back to a hard kill.
    pub stop_grace: Duration,
    /// How long a one-shot attach token may remain pending before redemption.
    pub attach_token_ttl: Duration,
}

impl Default for SessionRegistryConfig {
    fn default() -> Self {
        Self {
            shell_command: ShellCommand::default(),
            stop_grace: Duration::from_millis(500),
            attach_token_ttl: DEFAULT_ATTACH_TOKEN_TTL,
        }
    }
}

/// In-memory registry shared by control-connection tasks.
#[derive(Debug, Clone)]
pub struct SessionRegistry {
    inner: Arc<SessionRegistryInner>,
}

#[derive(Debug)]
struct SessionRegistryInner {
    sessions: Mutex<HashMap<SessionId, SessionEntry>>,
    pending_attaches: Mutex<HashMap<String, PendingAttach>>,
    active_attaches: Mutex<HashMap<String, ActiveAttach>>,
    next_id: AtomicU64,
    next_stream_id: AtomicU64,
    config: SessionRegistryConfig,
    events: broadcast::Sender<Event>,
}

#[derive(Debug, Clone)]
struct SessionEntry {
    info: SessionInfo,
    pty: PtyHandle,
    stopping: bool,
}

#[derive(Debug, Clone)]
struct PendingAttach {
    session_id: SessionId,
    expires_at: tokio::time::Instant,
}

#[derive(Debug, Clone)]
struct ActiveAttach {
    session_id: SessionId,
    cancel: CancellationToken,
}

/// Redeemed raw attach stream state for the API bridge.
#[derive(Debug, Clone)]
pub struct RedeemedAttach {
    /// One-shot stream id that was redeemed.
    pub stream_id: String,
    /// Session being attached.
    pub session_id: SessionId,
    /// PTY handle backing the session.
    pub pty: PtyHandle,
    /// Cancellation signal fired by `session.detach` or session exit.
    pub cancel: CancellationToken,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new(SessionRegistryConfig::default())
    }
}

impl SessionRegistry {
    /// Create a new empty registry with the supplied runtime config.
    #[must_use]
    pub fn new(config: SessionRegistryConfig) -> Self {
        let (events, _) = broadcast::channel(128);
        Self {
            inner: Arc::new(SessionRegistryInner {
                sessions: Mutex::new(HashMap::new()),
                pending_attaches: Mutex::new(HashMap::new()),
                active_attaches: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                next_stream_id: AtomicU64::new(1),
                config,
                events,
            }),
        }
    }

    /// Subscribe to session lifecycle events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Create a new shell-backed PTY session.
    pub async fn create(&self, params: SessionNewParams) -> Result<SessionInfo, ProtocolError> {
        validate_new_params(&params)?;
        let cwd = match params.cwd.clone() {
            Some(cwd) => cwd,
            None => std::env::current_dir().map_err(|source| {
                ProtocolError::new(
                    ErrorClass::Runtime,
                    "cwd_failed",
                    format!("failed to resolve daemon current directory: {source}"),
                    None,
                )
            })?,
        };

        let id = SessionId(format!(
            "s-{}",
            self.inner.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let command =
            self.inner
                .config
                .shell_command
                .to_pty_command(cwd.clone(), params.cols, params.rows);
        let pty = tokio::task::spawn_blocking(move || PtyHandle::spawn(command))
            .await
            .map_err(|_| runtime_error("spawn_failed", "PTY spawn task panicked"))?
            .map_err(pty_error_to_protocol)?;

        let now = timestamp_now();
        let info = SessionInfo {
            id: id.clone(),
            agent: params.agent,
            cwd,
            pid: pty.pid(),
            cols: params.cols,
            rows: params.rows,
            state: SessionState::Running,
            state_source: StateSource::Process,
            created_at: now.clone(),
            updated_at: now,
            exit_code: None,
        };

        {
            let mut sessions = self.inner.sessions.lock().await;
            sessions.insert(
                id.clone(),
                SessionEntry {
                    info: info.clone(),
                    pty: pty.clone(),
                    stopping: false,
                },
            );
        }

        self.emit(event::SESSION_CREATED, &info);
        self.spawn_exit_watcher(id, pty);
        Ok(info)
    }

    /// List all known sessions.
    pub async fn list(&self) -> Vec<SessionInfo> {
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .await
            .values()
            .map(|entry| entry.info.clone())
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        sessions
    }

    /// Inspect a session by id.
    pub async fn inspect(&self, id: &SessionId) -> Result<SessionInfo, ProtocolError> {
        let sessions = self.inner.sessions.lock().await;
        sessions
            .get(id)
            .map(|entry| entry.info.clone())
            .ok_or_else(|| session_not_found(&id.0))
    }

    /// Inspect a session by raw id string.
    pub async fn inspect_str(&self, id: &str) -> Result<SessionInfo, ProtocolError> {
        self.inspect(&SessionId(id.to_owned())).await
    }

    /// Mint a one-shot raw attach stream token for a running session.
    pub async fn attach(
        &self,
        id: &SessionId,
    ) -> Result<protocol::SessionAttachResult, ProtocolError> {
        self.prune_expired_pending_attaches().await;
        {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(id));
            }
        }

        let stream_id = format!(
            "a-{}",
            self.inner.next_stream_id.fetch_add(1, Ordering::Relaxed)
        );
        let pending = PendingAttach {
            session_id: id.clone(),
            expires_at: tokio::time::Instant::now() + self.inner.config.attach_token_ttl,
        };
        let mut pending_attaches = self.inner.pending_attaches.lock().await;
        pending_attaches.insert(stream_id.clone(), pending);
        Ok(protocol::SessionAttachResult { stream_id })
    }

    /// Redeem a one-shot attach token and register a live attach stream.
    pub async fn redeem_attach(&self, stream_id: &str) -> Result<RedeemedAttach, ProtocolError> {
        self.prune_expired_pending_attaches().await;
        let pending = {
            let mut pending_attaches = self.inner.pending_attaches.lock().await;
            pending_attaches.remove(stream_id)
        }
        .ok_or_else(|| attach_token_error("attach_not_found", stream_id))?;

        if tokio::time::Instant::now() > pending.expires_at {
            return Err(attach_token_error("attach_expired", stream_id));
        }

        let pty = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get(&pending.session_id)
                .ok_or_else(|| session_not_found(&pending.session_id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(&pending.session_id));
            }
            entry.pty.clone()
        };

        let cancel = CancellationToken::new();
        {
            let mut active_attaches = self.inner.active_attaches.lock().await;
            active_attaches.insert(
                stream_id.to_owned(),
                ActiveAttach {
                    session_id: pending.session_id.clone(),
                    cancel: cancel.clone(),
                },
            );
        }

        if let Err(err) = self.ensure_session_running(&pending.session_id).await {
            let mut active_attaches = self.inner.active_attaches.lock().await;
            active_attaches.remove(stream_id);
            return Err(err);
        }

        self.emit_attach(event::ATTACH_OPENED, &pending.session_id, stream_id);
        Ok(RedeemedAttach {
            stream_id: stream_id.to_owned(),
            session_id: pending.session_id,
            pty,
            cancel,
        })
    }

    /// Cancel an active raw attach stream. Unknown streams are a no-op.
    pub async fn detach(&self, stream_id: &str) -> protocol::SessionDetachResult {
        let cancel = {
            let active_attaches = self.inner.active_attaches.lock().await;
            active_attaches.get(stream_id).and_then(|active| {
                if active.cancel.is_cancelled() {
                    None
                } else {
                    Some(active.cancel.clone())
                }
            })
        };
        let detached = if let Some(cancel) = cancel {
            cancel.cancel();
            true
        } else {
            false
        };
        protocol::SessionDetachResult { detached }
    }

    /// Deregister a raw attach stream after its bridge exits.
    pub async fn finish_attach(&self, stream_id: &str) {
        let active = {
            let mut active_attaches = self.inner.active_attaches.lock().await;
            active_attaches.remove(stream_id)
        };

        if let Some(active) = active {
            self.emit_attach(event::ATTACH_CLOSED, &active.session_id, stream_id);
        }
    }

    /// Resize a running session PTY and return the updated session info.
    pub async fn resize(
        &self,
        id: &SessionId,
        cols: u16,
        rows: u16,
    ) -> Result<protocol::SessionResizeResult, ProtocolError> {
        if cols == 0 || rows == 0 {
            return Err(ProtocolError::bad_request(
                "session.resize requires non-zero cols and rows",
            ));
        }

        let pty = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(id));
            }
            entry.pty.clone()
        };

        pty.resize(cols, rows)
            .await
            .map_err(pty_error_to_protocol)?;

        let info = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get_mut(id)
                .ok_or_else(|| session_not_found(&id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(id));
            }
            entry.info.cols = cols;
            entry.info.rows = rows;
            entry.info.updated_at = timestamp_now();
            entry.info.clone()
        };

        self.emit(event::SESSION_UPDATED, &info);
        Ok(protocol::SessionResizeResult { session: info })
    }

    /// Stop a running session.
    pub async fn stop(&self, id: &SessionId) -> Result<SessionStopResult, ProtocolError> {
        let pty = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get_mut(id)
                .ok_or_else(|| session_not_found(&id.0))?;
            if is_terminal(entry.info.state) {
                return Ok(SessionStopResult { stopped: false });
            }

            entry.stopping = true;
            entry.pty.clone()
        };

        self.remove_pending_attaches_for_session(id).await;
        self.cancel_session_attaches(id).await;

        let exit = match pty.shutdown(self.inner.config.stop_grace).await {
            Ok(exit) => exit,
            Err(err) => {
                self.clear_stopping(id).await;
                return Err(pty_error_to_protocol(err));
            }
        };
        self.record_exit(id, exit, true).await;
        Ok(SessionStopResult { stopped: true })
    }

    /// Wait until a session reaches a terminal process-exit state.
    pub async fn wait_for_exit(
        &self,
        id: &SessionId,
        timeout: Duration,
    ) -> Result<SessionInfo, ProtocolError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let info = self.inspect(id).await?;
            if matches!(info.state, SessionState::Done | SessionState::Failed) {
                return Ok(info);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(runtime_error(
                    "session_exit_timeout",
                    format!("timed out waiting for session {} to exit", id.0),
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn spawn_exit_watcher(&self, id: SessionId, pty: PtyHandle) {
        let registry = self.clone();
        tokio::spawn(async move {
            match pty.wait_exit().await {
                Ok(exit) => {
                    if let Err(err) = pty.join_reader_thread().await {
                        warn!(session_id = %id.0, error = %err, "failed to join PTY reader thread");
                    }
                    registry.record_exit(&id, exit, false).await;
                }
                Err(err) => {
                    warn!(session_id = %id.0, error = %err, "failed while waiting for PTY exit");
                    registry
                        .record_exit(
                            &id,
                            PtyExit {
                                exit_code: None,
                                success: false,
                            },
                            false,
                        )
                        .await;
                }
            }
        });
    }

    async fn record_exit(&self, id: &SessionId, exit: PtyExit, stopped_by_user: bool) {
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, "PTY exit arrived for unknown session");
                return;
            };

            if entry.info.state == SessionState::Stopped && stopped_by_user && !entry.stopping {
                return;
            }

            if is_terminal(entry.info.state) && !stopped_by_user && !entry.stopping {
                return;
            }

            let stopped =
                stopped_by_user || entry.stopping || entry.info.state == SessionState::Stopped;
            entry.stopping = false;

            if stopped {
                entry.info.state = SessionState::Stopped;
            } else if exit.success {
                entry.info.state = SessionState::Done;
            } else {
                entry.info.state = SessionState::Failed;
            }
            entry.info.state_source = StateSource::Process;
            entry.info.exit_code = exit.exit_code;
            entry.info.updated_at = timestamp_now();
            let event = if stopped {
                event::SESSION_STOPPED
            } else {
                event::SESSION_UPDATED
            };
            (event, entry.info.clone())
        };

        self.cancel_session_attaches(id).await;
        self.remove_pending_attaches_for_session(id).await;
        self.emit(updated.0, &updated.1);
    }

    async fn clear_stopping(&self, id: &SessionId) {
        let mut sessions = self.inner.sessions.lock().await;
        if let Some(entry) = sessions.get_mut(id) {
            if !is_terminal(entry.info.state) {
                entry.stopping = false;
            }
        }
    }

    async fn ensure_session_running(&self, id: &SessionId) -> Result<(), ProtocolError> {
        let sessions = self.inner.sessions.lock().await;
        let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
        if entry.info.state == SessionState::Running {
            Ok(())
        } else {
            Err(session_not_running(id))
        }
    }

    async fn cancel_session_attaches(&self, id: &SessionId) {
        let active_attaches = self.inner.active_attaches.lock().await;
        for (stream_id, active) in active_attaches.iter() {
            if active.session_id == *id {
                debug!(session_id = %id.0, stream_id, "cancelling active attach");
                active.cancel.cancel();
            }
        }
    }

    async fn prune_expired_pending_attaches(&self) {
        let now = tokio::time::Instant::now();
        let mut pending_attaches = self.inner.pending_attaches.lock().await;
        pending_attaches.retain(|_, pending| pending.expires_at > now);
    }

    async fn remove_pending_attaches_for_session(&self, id: &SessionId) {
        let mut pending_attaches = self.inner.pending_attaches.lock().await;
        pending_attaches.retain(|_, pending| pending.session_id != *id);
    }

    fn emit(&self, name: &str, info: &SessionInfo) {
        let event = Event::new(name, json!({ "session": info }));
        let _ = self.inner.events.send(event);
    }

    fn emit_attach(&self, name: &str, session_id: &SessionId, stream_id: &str) {
        let event = Event::new(
            name,
            json!({
                "session_id": session_id,
                "stream_id": stream_id,
            }),
        );
        let _ = self.inner.events.send(event);
    }
}

fn validate_new_params(params: &SessionNewParams) -> Result<(), ProtocolError> {
    if params.agent != AgentKind::Shell {
        return Err(ProtocolError::new(
            ErrorClass::Runtime,
            "agent_not_supported",
            "only shell sessions are supported in this milestone",
            None,
        ));
    }
    if params.cols == 0 || params.rows == 0 {
        return Err(ProtocolError::bad_request(
            "session.new requires non-zero cols and rows",
        ));
    }
    Ok(())
}

fn is_terminal(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Stopped | SessionState::Done | SessionState::Failed
    )
}

fn session_not_found(id: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_not_found",
        format!("session not found: {id}"),
        None,
    )
}

fn session_not_running(id: &SessionId) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_not_running",
        format!("session is not running: {}", id.0),
        None,
    )
}

fn attach_token_error(code: &'static str, stream_id: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        code,
        format!("attach stream is not available: {stream_id}"),
        None,
    )
}

fn pty_error_to_protocol(err: PtyError) -> ProtocolError {
    let code = match err {
        PtyError::Allocate(_) => "pty_alloc_failed",
        PtyError::Spawn(_) => "spawn_failed",
        PtyError::MissingPid => "spawn_failed",
        PtyError::Io(_) | PtyError::Poisoned | PtyError::ThreadPanicked | PtyError::ExitTimeout => {
            "pty_error"
        }
    };
    ProtocolError::new(ErrorClass::Runtime, code, err.to_string(), None)
}

fn runtime_error(code: impl Into<String>, msg: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorClass::Runtime, code, msg, None)
}

/// Current UTC time as an RFC3339 string for session metadata.
///
/// Uses `now_utc()` (not `now_local()`, which can fail to resolve the local
/// offset). Formatting a valid `OffsetDateTime` as RFC3339 cannot fail in
/// practice; the fallback only guards against a future API change.
fn timestamp_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use protocol::{AgentKind, SessionNewParams, SessionState};

    use super::{SessionRegistry, SessionRegistryConfig, ShellCommand};

    fn params() -> SessionNewParams {
        SessionNewParams {
            agent: AgentKind::Shell,
            cwd: Some(PathBuf::from("/tmp")),
            cols: 80,
            rows: 24,
        }
    }

    #[tokio::test]
    async fn detects_successful_process_exit() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "exit 0"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");
        let exit = registry
            .wait_for_exit(&created.id, Duration::from_secs(2))
            .await
            .expect("session exits");

        assert_eq!(exit.state, SessionState::Done);
        assert_eq!(exit.exit_code, Some(0));
    }

    #[tokio::test]
    async fn stop_marks_running_session_stopped() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");
        let stopped = registry.stop(&created.id).await.expect("stop session");
        let inspected = registry
            .inspect(&created.id)
            .await
            .expect("inspect session");

        assert!(stopped.stopped);
        assert_eq!(inspected.state, SessionState::Stopped);
    }

    #[tokio::test]
    async fn attach_tokens_are_one_shot_and_expired_tokens_are_pruned() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            attach_token_ttl: Duration::from_millis(1),
        });

        let created = registry.create(params()).await.expect("create session");
        let expired = registry.attach(&created.id).await.expect("attach token");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let fresh = registry
            .attach(&created.id)
            .await
            .expect("fresh attach token");

        {
            let pending = registry.inner.pending_attaches.lock().await;
            assert!(
                !pending.contains_key(&expired.stream_id),
                "expired pending attach token should be pruned"
            );
            assert!(
                pending.contains_key(&fresh.stream_id),
                "fresh pending attach token should remain"
            );
        }

        let redeemed = registry
            .redeem_attach(&fresh.stream_id)
            .await
            .expect("redeem fresh attach token");
        let second_redeem = registry
            .redeem_attach(&fresh.stream_id)
            .await
            .expect_err("stream id is one-shot");
        assert_eq!(second_redeem.code, "attach_not_found");

        registry.finish_attach(&redeemed.stream_id).await;
        let stopped = registry.stop(&created.id).await.expect("stop session");
        assert!(stopped.stopped);
    }

    #[tokio::test]
    async fn inspect_missing_session_returns_not_found() {
        let registry = SessionRegistry::default();
        let missing = registry
            .inspect_str("s-missing")
            .await
            .expect_err("missing session");

        assert_eq!(missing.code, "session_not_found");
    }
}
