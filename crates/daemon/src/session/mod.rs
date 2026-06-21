//! In-memory session registry and supervisor.
//!
//! Milestone 3 keeps session metadata in memory only. Each session owns a PTY
//! handle and has a watcher task that records process exit.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{
    event, AgentKind, ErrorClass, Event, ProtocolError, SessionId, SessionInfo, SessionInputParams,
    SessionInputResult, SessionNewParams, SessionReportNativeIdParams, SessionReportNativeIdResult,
    SessionState, SessionStopResult, SessionWarning, StateSource, PROTOCOL_VERSION,
};
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{broadcast, watch, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::agent::{
    resume_pty_command, AgentAdapter, ClaudeAdapter, CodexAdapter, InputRules, LaunchOpts,
    SessionRef,
};
use crate::detect::{ActivityTransition, Detector, DetectorConfig};
use crate::integration::{ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH};
use crate::pty::{PtyCommand, PtyError, PtyExit, PtyHandle};
use crate::store::{ResumeBinding, Store};
use crate::worktree::{WorktreeManager, WorktreeRequest};

const DEFAULT_ATTACH_TOKEN_TTL: Duration = Duration::from_secs(10);
/// Bound on how long a graceful shutdown waits for the event-log drain to flush
/// its backlog, so a wedged log write can never hang shutdown.
const EVENT_LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
const SUBMIT: &[u8] = b"\r";

/// Default per-session raw-output history cap (10 MB), replayed on attach.
///
/// Matches herdr's `DEFAULT_SCROLLBACK_LIMIT_BYTES`; overridable via
/// [`SessionRegistryConfig::output_history_limit_bytes`].
const DEFAULT_OUTPUT_HISTORY_LIMIT_BYTES: usize = 10_000_000;

/// Default wall-clock bound on a per-repository worktree setup script
/// (`.pohunek/setup`). It is a safety cap on a *hang*, not a tight budget — a
/// legitimate script may install dependencies — so it is generous; a script that
/// exceeds it is terminated and surfaced as a non-fatal `setup_script` warning.
/// Overridable via [`SessionRegistryConfig::setup_script_timeout`].
const DEFAULT_SETUP_SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);

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
            env: Vec::new(),
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
    /// Per-session cap on the raw-output history buffer replayed on attach.
    pub output_history_limit_bytes: usize,
    /// Delay before sending Claude Code's Ink submit byte as a separate write.
    pub claude_submit_delay: Duration,
    /// Control socket path injected into Codex/Claude agents so their hook can
    /// call home. `None` disables hook-handshake env injection (e.g. in unit
    /// tests that do not exercise the hook).
    pub socket_path: Option<PathBuf>,
    /// Backing file for the unified metadata store (resume + worktree bindings).
    /// `None` disables persistence (sessions are then not resumable across a
    /// restart, and worktree binding is unavailable).
    pub store_path: Option<PathBuf>,
    /// Root directory under which per-session worktrees are created
    /// (`<data_dir>/worktrees`). Must be set together with [`Self::store_path`]
    /// to enable worktree binding; when unset, a `session.new` carrying a
    /// repo+branch fails (no silent default).
    pub worktree_root: Option<PathBuf>,
    /// Wall-clock bound on a per-repository worktree setup script. A script that
    /// exceeds it is terminated and recorded as a non-fatal `setup_script`
    /// warning, so a hanging script can never wedge `session.new`. Defaults to
    /// [`DEFAULT_SETUP_SCRIPT_TIMEOUT`].
    pub setup_script_timeout: Duration,
    /// Directory for the append-only event log (`<data_dir>/events`). `None`
    /// disables event logging. Started via [`SessionRegistry::spawn_event_log`].
    pub event_log_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputWritePlan {
    immediate: Vec<u8>,
    delayed_submit: Option<(Duration, Vec<u8>)>,
}

/// Everything needed to spawn and register one PTY-backed session, shared by
/// first launch (`create`) and resume (`resume_binding`).
#[derive(Debug)]
struct PtySessionSpec {
    id: SessionId,
    agent: AgentKind,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    command: PtyCommand,
    /// Native id when relaunching a captured session (`None` on first launch).
    native_session_id: Option<String>,
    /// Source repository, when the session is bound to a worktree.
    repo: Option<PathBuf>,
    /// Branch checked out in the bound worktree.
    branch: Option<String>,
    /// Bound worktree path (equal to `cwd` for worktree sessions).
    worktree_path: Option<PathBuf>,
    /// Non-fatal worktree-setup warnings to surface on the session.
    warnings: Vec<SessionWarning>,
}

impl Default for SessionRegistryConfig {
    fn default() -> Self {
        Self {
            shell_command: ShellCommand::default(),
            stop_grace: Duration::from_millis(500),
            attach_token_ttl: DEFAULT_ATTACH_TOKEN_TTL,
            output_history_limit_bytes: DEFAULT_OUTPUT_HISTORY_LIMIT_BYTES,
            claude_submit_delay: crate::agent::DEFAULT_CLAUDE_SUBMIT_DELAY,
            socket_path: None,
            store_path: None,
            worktree_root: None,
            setup_script_timeout: DEFAULT_SETUP_SCRIPT_TIMEOUT,
            event_log_dir: None,
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
    /// Unified metadata store (resume + worktree bindings), present when
    /// persistence is configured. Shared (`Arc`) with [`Self::worktree`] so both
    /// record kinds live in one file behind one serialization point.
    store: Option<Arc<Store>>,
    /// Serializes resume-binding persistence so a resize and a native-id capture
    /// (or two resizes) racing on the same session cannot leave a stale binding:
    /// each persister re-reads current state under this lock, so the last writer
    /// wins with the freshest size. Held across the (blocking) store I/O instead
    /// of the sessions lock, keeping that hot lock free of file writes.
    persist_lock: Mutex<()>,
    /// Per-session worktree binder, present when worktree binding is configured.
    /// Shared into `spawn_blocking` for the (blocking) git subprocesses.
    worktree: Option<Arc<WorktreeManager>>,
    /// Cancellation signal for the event-log drain, fired by
    /// [`SessionRegistry::shutdown_event_log`] so the drain flushes its backlog
    /// and exits cleanly at shutdown.
    event_log_shutdown: CancellationToken,
    /// Join handle of the spawned event-log drain task, awaited at shutdown.
    event_log_task: std::sync::Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug, Clone)]
struct SessionEntry {
    info: SessionInfo,
    pty: PtyHandle,
    detector_cancel: CancellationToken,
    detector_resize: watch::Sender<(u16, u16)>,
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
        // One unified store instance, shared (`Arc`) with the worktree manager so
        // resume and worktree records live in one file behind one serialization
        // point.
        let store = config
            .store_path
            .clone()
            .map(|path| Arc::new(Store::new(path)));
        // Worktree binding needs both a root for the trees and the shared store;
        // it is enabled only when both are configured (no silent default path).
        let worktree = match (&config.worktree_root, &store) {
            (Some(root), Some(store)) => Some(Arc::new(WorktreeManager::new(
                root.clone(),
                store.clone(),
                config.setup_script_timeout,
            ))),
            _ => None,
        };
        Self {
            inner: Arc::new(SessionRegistryInner {
                sessions: Mutex::new(HashMap::new()),
                pending_attaches: Mutex::new(HashMap::new()),
                active_attaches: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                next_stream_id: AtomicU64::new(1),
                config,
                events,
                store,
                persist_lock: Mutex::new(()),
                worktree,
                event_log_shutdown: CancellationToken::new(),
                event_log_task: std::sync::Mutex::new(None),
            }),
        }
    }

    /// Subscribe to session lifecycle events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Start the append-only event log, if [`SessionRegistryConfig::event_log_dir`]
    /// is configured.
    ///
    /// Opens the log (creating the events directory `0700` and the file `0600`)
    /// and spawns a background task draining this registry's event broadcast into
    /// it. Call once at startup, **before** any session is created or resumed, so
    /// every lifecycle event is captured. A no-op when no event-log dir is set.
    ///
    /// # Errors
    ///
    /// Returns the open error so the daemon can fail fast on a misconfigured log
    /// location.
    pub fn spawn_event_log(&self) -> std::io::Result<()> {
        let Some(dir) = &self.inner.config.event_log_dir else {
            return Ok(());
        };
        let log = Arc::new(crate::events::EventLog::open(dir)?);
        let handle = crate::events::spawn_drain(
            log,
            self.subscribe(),
            self.inner.event_log_shutdown.clone(),
        );
        *self
            .inner
            .event_log_task
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Some(handle);
        Ok(())
    }

    /// Flush and stop the event-log drain at shutdown.
    ///
    /// Cancels the drain so it makes a final pass over any buffered events, then
    /// awaits the task (bounded by [`EVENT_LOG_FLUSH_TIMEOUT`]) so the process
    /// does not exit while audit lines are still unwritten. A no-op when no event
    /// log was started.
    pub async fn shutdown_event_log(&self) {
        self.inner.event_log_shutdown.cancel();
        let handle = self
            .inner
            .event_log_task
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(handle) = handle {
            if tokio::time::timeout(EVENT_LOG_FLUSH_TIMEOUT, handle)
                .await
                .is_err()
            {
                warn!("event-log drain did not finish flushing within the shutdown timeout");
            }
        }
    }

    /// Create a new PTY-backed session.
    ///
    /// When `params` carries a repo + branch, a dedicated worktree is bound (or
    /// reused) and the agent is launched **inside** it; otherwise the agent is
    /// launched in the resolved `cwd` (today's behavior). Non-fatal worktree
    /// warnings ride along on the returned [`SessionInfo`].
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

        // Bind a worktree when a repo+branch was requested; launch there instead
        // of the plain cwd. A plain cwd (no repo) keeps today's behavior.
        let bound = self.resolve_worktree(&id, &params).await?;
        let worktree_bound = bound.is_some();
        let (launch_cwd, repo, branch, worktree_path, warnings) = match bound {
            Some(bound) => (
                bound.path.clone(),
                Some(bound.repository),
                Some(bound.branch),
                Some(bound.path),
                bound.warnings,
            ),
            None => (cwd, None, None, None, Vec::new()),
        };

        // The worktree is now bound and its branch is checked out. Any failure
        // building the launch command or spawning the PTY must roll that back: a
        // leftover worktree keeps the branch checked out and blocks the next
        // `session.new` on it with `worktree_branch_in_use` (an orphan a fresh
        // session id would never reuse). Compensate here — not in
        // `register_pty_session`, which `resume_binding` shares and where the
        // worktree must be kept.
        let launch = async {
            let env_extra = self.hook_env(params.agent, &id);
            let command = build_launch_command(
                params.agent,
                &self.inner.config.shell_command,
                launch_cwd.clone(),
                params.cols,
                params.rows,
                env_extra,
            )?;

            self.register_pty_session(PtySessionSpec {
                id: id.clone(),
                agent: params.agent,
                cwd: launch_cwd,
                cols: params.cols,
                rows: params.rows,
                command,
                native_session_id: None,
                repo,
                branch,
                worktree_path,
                warnings,
            })
            .await
        }
        .await;

        if launch.is_err() && worktree_bound {
            self.cleanup_bound_worktree(&id).await;
        }
        launch
    }

    /// Roll back a worktree bound earlier in [`Self::create`] when the session
    /// then fails to launch, removing the checkout (and its binding) so the
    /// branch is freed for a retry. Best-effort and non-fatal: a rollback failure
    /// is logged and never masks the original launch error. A no-op when worktree
    /// binding is not configured.
    async fn cleanup_bound_worktree(&self, id: &SessionId) {
        let Some(manager) = self.inner.worktree.clone() else {
            return;
        };
        let session_id = id.0.clone();
        match tokio::task::spawn_blocking(move || manager.cleanup_session(&session_id)).await {
            Ok(Ok(removed)) => {
                if removed > 0 {
                    debug!(
                        session_id = %id.0,
                        removed,
                        "rolled back worktree after a failed launch"
                    );
                }
            }
            Ok(Err(err)) => warn!(
                session_id = %id.0,
                error = %err,
                "failed to roll back worktree after a failed launch"
            ),
            Err(_) => warn!(
                session_id = %id.0,
                "worktree rollback task panicked"
            ),
        }
    }

    /// Bind a worktree for this session when `params` requests one.
    ///
    /// Returns `Ok(None)` for a plain-`cwd` session (no repo). Validates that
    /// `repo`/`branch` are supplied together and that `base_branch` is not given
    /// without them. Runs the blocking git work on a blocking thread.
    async fn resolve_worktree(
        &self,
        id: &SessionId,
        params: &SessionNewParams,
    ) -> Result<Option<crate::worktree::WorktreeBound>, ProtocolError> {
        match (params.repo.as_ref(), params.branch.as_ref()) {
            (None, None) => {
                if params.base_branch.is_some() {
                    return Err(ProtocolError::bad_request(
                        "session.new base_branch requires repo and branch",
                    ));
                }
                Ok(None)
            }
            (Some(_), None) | (None, Some(_)) => Err(ProtocolError::bad_request(
                "session.new repo and branch must be supplied together",
            )),
            (Some(repo), Some(branch)) => {
                let Some(manager) = self.inner.worktree.clone() else {
                    return Err(runtime_error(
                        "worktree_not_configured",
                        "the daemon is not configured for worktree binding",
                    ));
                };
                let request = WorktreeRequest {
                    session_id: id.0.clone(),
                    repo: repo.clone(),
                    branch: branch.clone(),
                    base_branch: params.base_branch.clone(),
                };
                let bound = tokio::task::spawn_blocking(move || manager.bind(&request))
                    .await
                    .map_err(|_| {
                        runtime_error("worktree_bind_failed", "worktree bind task panicked")
                    })??;
                Ok(Some(bound))
            }
        }
    }

    /// Spawn a PTY for `spec.command`, register the session, and start its
    /// detector and exit watcher. Shared by `create` (first launch) and
    /// `resume_binding` (relaunch after a daemon restart).
    async fn register_pty_session(
        &self,
        spec: PtySessionSpec,
    ) -> Result<SessionInfo, ProtocolError> {
        let PtySessionSpec {
            id,
            agent,
            cwd,
            cols,
            rows,
            command,
            native_session_id,
            repo,
            branch,
            worktree_path,
            warnings,
        } = spec;

        let history_limit_bytes = self.inner.config.output_history_limit_bytes;
        // Keep the program name for diagnostics: a spawn failure should name what
        // could not be launched (see `spawn_error_to_protocol`).
        let program = command.program.clone();
        let pty =
            tokio::task::spawn_blocking(move || PtyHandle::spawn(command, history_limit_bytes))
                .await
                .map_err(|_| runtime_error("spawn_failed", "PTY spawn task panicked"))?
                .map_err(|err| spawn_error_to_protocol(err, &program))?;
        let detector_output = pty.subscribe_output();
        let detector_cancel = CancellationToken::new();
        let (detector_resize, detector_resize_rx) = watch::channel((rows, cols));

        let now = timestamp_now();
        let info = SessionInfo {
            id: id.clone(),
            agent,
            cwd,
            pid: pty.pid(),
            cols,
            rows,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            native_session_id,
            repo,
            branch,
            worktree_path,
            warnings,
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
                    detector_cancel: detector_cancel.clone(),
                    detector_resize,
                    stopping: false,
                },
            );
        }

        self.emit(event::SESSION_CREATED, &info);
        self.spawn_detector(
            id.clone(),
            agent,
            detector_output,
            (rows, cols),
            detector_cancel,
            detector_resize_rx,
        );
        self.spawn_exit_watcher(id, pty);
        Ok(info)
    }

    /// Build the hook-handshake env injected into a Codex/Claude agent so its
    /// `SessionStart` hook can report its native session id back to the socket.
    /// Shell sessions (and registries without a configured socket path) get no
    /// hook env.
    fn hook_env(&self, agent: AgentKind, session_id: &SessionId) -> Vec<(String, String)> {
        match agent {
            AgentKind::Shell => Vec::new(),
            AgentKind::Codex | AgentKind::Claude => match &self.inner.config.socket_path {
                Some(socket_path) => vec![
                    (ENV_FLAG.to_owned(), "1".to_owned()),
                    (
                        ENV_SOCKET_PATH.to_owned(),
                        socket_path.display().to_string(),
                    ),
                    (ENV_SESSION_ID.to_owned(), session_id.0.clone()),
                    (
                        ENV_PROTOCOL_VERSION.to_owned(),
                        PROTOCOL_VERSION.get().to_string(),
                    ),
                ],
                None => Vec::new(),
            },
        }
    }

    /// Inject text into a running session using the agent's input framing rules.
    pub async fn input(
        &self,
        params: SessionInputParams,
    ) -> Result<SessionInputResult, ProtocolError> {
        let (pty, rules) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get(&params.session_id)
                .ok_or_else(|| session_not_found(&params.session_id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(&params.session_id));
            }
            (
                entry.pty.clone(),
                input_rules_for_agent(entry.info.agent, &self.inner.config),
            )
        };

        let writes = build_input_writes(&params.text, rules);
        pty.write_user_input(writes.immediate)
            .await
            .map_err(pty_error_to_protocol)?;

        if let Some((delay, bytes)) = writes.delayed_submit {
            let delayed_pty = pty.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Err(err) = delayed_pty.write_user_input(bytes).await {
                    warn!(error = %err, "failed to write delayed agent submit byte");
                }
            });
        }

        Ok(SessionInputResult { accepted: true })
    }

    /// Record an agent's native session id as the session's resume binding.
    ///
    /// Called from the `session.report_native_id` handler when a `SessionStart`
    /// hook fires. Validates the native id, updates the in-memory session info
    /// (so `inspect`/`list` show it), and persists a minimal resume binding.
    /// Reports for an unknown or already-terminal session are ignored, not
    /// errors (the hook fires-and-forgets).
    pub async fn report_native_id(
        &self,
        params: SessionReportNativeIdParams,
    ) -> SessionReportNativeIdResult {
        let not_recorded = SessionReportNativeIdResult { recorded: false };

        // Claude and Codex report an opaque native id, never a path.
        let session_ref = match SessionRef::id(&params.native_session_id) {
            Ok(session_ref) => session_ref,
            Err(err) => {
                debug!(
                    session_id = %params.session_id.0,
                    error = %err,
                    "ignoring native-id report with an invalid native session id"
                );
                return not_recorded;
            }
        };

        let info = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(&params.session_id) else {
                debug!(
                    session_id = %params.session_id.0,
                    "native-id report for an unknown session; ignoring"
                );
                return not_recorded;
            };
            if is_terminal(entry.info.state) {
                debug!(
                    session_id = %params.session_id.0,
                    "native-id report for a terminal session; ignoring"
                );
                return not_recorded;
            }

            entry.info.native_session_id = Some(session_ref.value().to_owned());
            entry.info.updated_at = timestamp_now();
            entry.info.clone()
        };
        // Persist from the now-current in-memory state (a resize that landed
        // first is reflected, not clobbered).
        self.persist_resume_binding(&params.session_id).await;
        self.emit(event::SESSION_UPDATED, &info);
        SessionReportNativeIdResult { recorded: true }
    }

    /// Make the persisted resume binding for `id` match the session's CURRENT
    /// in-memory state, serialized against every other persister.
    ///
    /// A live session that has captured a native id gets its binding upserted
    /// with the latest cwd/size; any other session (terminal, gone, or never
    /// captured) gets its binding removed. The whole snapshot-then-write is
    /// serialized by `persist_lock` and re-reads the session under the sessions
    /// lock, so when a resize and a native-id capture (or two resizes) race,
    /// whichever runs last reads the freshest state and writes it last — no
    /// stale size can win, and a session that went terminal in between is never
    /// resurrected (it re-reads as terminal and removes instead). Only the brief
    /// snapshot holds the sessions lock; the blocking store I/O runs under
    /// `persist_lock` alone. Best-effort: an unconfigured store or a failed
    /// write is non-fatal and only impairs restart-resume, surfaced via a warn.
    async fn persist_resume_binding(&self, id: &SessionId) {
        let Some(store) = &self.inner.store else {
            return;
        };
        let _persist = self.inner.persist_lock.lock().await;
        let desired = {
            let sessions = self.inner.sessions.lock().await;
            sessions.get(id).and_then(|entry| {
                if is_terminal(entry.info.state) {
                    return None;
                }
                entry
                    .info
                    .native_session_id
                    .as_ref()
                    .map(|native| ResumeBinding {
                        session_id: id.0.clone(),
                        agent: entry.info.agent,
                        cwd: entry.info.cwd.clone(),
                        cols: entry.info.cols,
                        rows: entry.info.rows,
                        native_session_id: Some(native.clone()),
                        native_session_path: None,
                    })
            })
        };
        let result = match &desired {
            Some(binding) => store.record_resume(binding),
            None => store.remove_resume(&id.0),
        };
        if let Err(err) = result {
            warn!(
                session_id = %id.0,
                error = %err,
                "failed to persist resume binding"
            );
        }
    }

    /// Load the resume-binding store and relaunch each resumable session.
    ///
    /// Called once at daemon startup. A daemon restart kills all live PTYs by
    /// design (see `docs/plan-phase-1.md` "Resume Model"); only sessions whose
    /// native id was captured are persisted, so only those come back here. A
    /// per-session resume failure is logged and skipped, never fatal.
    pub async fn load_and_resume(&self) {
        let Some(store) = &self.inner.store else {
            return;
        };
        let bindings = match store.load_resume() {
            Ok(bindings) => bindings,
            Err(err) => {
                warn!(error = %err, "failed to load resume-binding store; skipping resume");
                return;
            }
        };
        if bindings.is_empty() {
            return;
        }

        info!(
            count = bindings.len(),
            "resuming sessions after daemon restart"
        );
        for binding in bindings {
            let session_id = binding.session_id.clone();
            let agent = binding.agent;
            match self.resume_binding(binding).await {
                Ok(info) => {
                    info!(session_id = %info.id.0, ?agent, "resumed session via native id");
                }
                Err(err) => {
                    // A structurally-corrupt binding (a malformed/absent native
                    // ref) can never resume regardless of environment, so prune
                    // it to self-heal instead of retrying it on every restart.
                    // `agent_binary_missing` is left in place: it may be a
                    // transient PATH gap at startup that resolves on a later run.
                    if matches!(err.code.as_str(), "invalid_session_ref" | "not_resumable") {
                        // Prune via the serialized persist path: the failed
                        // resume registered no live session, so this re-reads the
                        // id as absent and removes its binding. Routing it here
                        // (instead of a direct store.remove) keeps persist_lock
                        // the single serialization point for all binding writes.
                        self.persist_resume_binding(&SessionId(session_id.clone()))
                            .await;
                        warn!(session_id = %session_id, error = %err, "dropping unresumable binding");
                    } else {
                        warn!(session_id = %session_id, error = %err, "failed to resume session");
                    }
                }
            }
        }
    }

    /// Relaunch one session from its stored resume binding, reusing its id.
    async fn resume_binding(&self, binding: ResumeBinding) -> Result<SessionInfo, ProtocolError> {
        let session_ref = match (&binding.native_session_id, &binding.native_session_path) {
            (Some(id), _) => SessionRef::id(id)?,
            (None, Some(path)) => SessionRef::path(path)?,
            (None, None) => {
                return Err(runtime_error(
                    "not_resumable",
                    format!(
                        "resume binding for {} has no native id or path",
                        binding.session_id
                    ),
                ));
            }
        };

        let id = SessionId(binding.session_id.clone());
        self.bump_next_id_past(&id);
        let env_extra = self.hook_env(binding.agent, &id);
        let opts = LaunchOpts {
            cwd: binding.cwd.clone(),
            cols: binding.cols,
            rows: binding.rows,
            env_extra,
        };
        let command = resume_pty_command(binding.agent, &session_ref, &opts)?;
        // A resumed session relaunches in its recorded cwd, which already is the
        // worktree path for worktree sessions (the worktree persists on disk
        // across a daemon restart). With the unified store the session's worktree
        // metadata (repo/branch/worktree_path) is restored too, so inspect/list
        // show it again after a restart.
        let (repo, branch, worktree_path) = self.restore_worktree_metadata(&binding.session_id);
        self.register_pty_session(PtySessionSpec {
            id,
            agent: binding.agent,
            cwd: binding.cwd,
            cols: binding.cols,
            rows: binding.rows,
            command,
            native_session_id: binding.native_session_id,
            repo,
            branch,
            worktree_path,
            warnings: Vec::new(),
        })
        .await
    }

    /// Look up a resumed session's worktree binding in the unified store and
    /// return its `(repo, branch, worktree_path)` so the restored session shows
    /// its worktree metadata again. Best-effort: a missing store, a read error,
    /// or no binding yields all-`None` — the session still resumes (its cwd is the
    /// worktree path either way); only the display metadata is absent.
    fn restore_worktree_metadata(
        &self,
        session_id: &str,
    ) -> (Option<PathBuf>, Option<String>, Option<PathBuf>) {
        let Some(store) = &self.inner.store else {
            return (None, None, None);
        };
        match store.find_worktree_for_session(session_id) {
            Ok(Some(binding)) => (
                Some(binding.repository),
                Some(binding.branch),
                Some(binding.path),
            ),
            Ok(None) => (None, None, None),
            Err(err) => {
                warn!(
                    session_id = %session_id,
                    error = %err,
                    "failed to read worktree metadata during resume"
                );
                (None, None, None)
            }
        }
    }

    /// Advance the session-id counter past a restored `s-<N>` id so a freshly
    /// created session never collides with a resumed one.
    fn bump_next_id_past(&self, id: &SessionId) {
        let Some(n) = id.0.strip_prefix("s-").and_then(|n| n.parse::<u64>().ok()) else {
            return;
        };
        let mut current = self.inner.next_id.load(Ordering::Relaxed);
        while current <= n {
            match self.inner.next_id.compare_exchange_weak(
                current,
                n + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
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

        let (info, detector_resize, has_native) = {
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
            let has_native = entry.info.native_session_id.is_some();
            (
                entry.info.clone(),
                entry.detector_resize.clone(),
                has_native,
            )
        };
        let _ = detector_resize.send((rows, cols));

        // A captured session has a persisted binding holding the pre-resize
        // size; refresh it so a daemon restart resumes at the current size.
        // Uncaptured sessions have no binding, so we skip the store entirely to
        // keep file I/O off the common resize path. `persist_resume_binding`
        // re-reads the current size, so a racing capture/resize cannot persist a
        // stale one.
        if has_native {
            self.persist_resume_binding(id).await;
        }

        self.emit(event::SESSION_UPDATED, &info);
        Ok(protocol::SessionResizeResult { session: info })
    }

    /// Stop a running session.
    pub async fn stop(&self, id: &SessionId) -> Result<SessionStopResult, ProtocolError> {
        let (pty, detector_cancel) = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get_mut(id)
                .ok_or_else(|| session_not_found(&id.0))?;
            if is_terminal(entry.info.state) {
                return Ok(SessionStopResult { stopped: false });
            }

            entry.stopping = true;
            (entry.pty.clone(), entry.detector_cancel.clone())
        };

        detector_cancel.cancel();
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

    fn spawn_detector(
        &self,
        id: SessionId,
        agent: AgentKind,
        mut output_rx: broadcast::Receiver<Vec<u8>>,
        size: (u16, u16),
        cancel: CancellationToken,
        mut resize_rx: watch::Receiver<(u16, u16)>,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            let detector_config = DetectorConfig::for_agent(agent);
            let mut tick = tokio::time::interval(detector_config.detection.recheck_after);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            let (rows, cols) = size;
            let mut detector = Detector::new(rows, cols, Instant::now(), detector_config);

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        for transition in detector.tick(Instant::now()) {
                            registry.record_activity(&id, transition).await;
                        }
                    }
                    changed = resize_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let (rows, cols) = *resize_rx.borrow();
                        detector.resize(rows, cols);
                    }
                    received = output_rx.recv() => {
                        match received {
                            Ok(chunk) => {
                                for transition in detector.feed(Instant::now(), &chunk) {
                                    registry.record_activity(&id, transition).await;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                warn!(
                                    session_id = %id.0,
                                    skipped,
                                    "resyncing detector state after PTY output lag"
                                );
                                detector.resync_after_lag();
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        });
    }

    async fn record_activity(&self, id: &SessionId, transition: ActivityTransition) {
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, "detector activity arrived for unknown session");
                return;
            };

            if entry.stopping || is_terminal(entry.info.state) {
                return;
            }

            entry.info.activity = Some(transition.activity);
            entry.info.state_source = transition.source;
            entry.info.updated_at = timestamp_now();
            transition
        };

        let event = Event::new(
            event::AGENT_STATE,
            json!({
                "session_id": id,
                "activity": updated.activity,
                "source": updated.source,
            }),
        );
        let _ = self.inner.events.send(event);
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
            entry.info.activity = None;
            entry.info.exit_code = exit.exit_code;
            entry.info.updated_at = timestamp_now();
            entry.detector_cancel.cancel();
            let event = if stopped {
                event::SESSION_STOPPED
            } else {
                event::SESSION_UPDATED
            };
            (event, entry.info.clone())
        };

        self.cancel_session_attaches(id).await;
        self.remove_pending_attaches_for_session(id).await;
        // A terminal session must not resurrect on the next daemon restart:
        // resume is for sessions whose live PTY a restart killed, not for ones
        // the user stopped or that exited. The session is now terminal, so
        // `persist_resume_binding` re-reads it as terminal and removes its
        // binding (serialized against any racing resize/capture write).
        self.persist_resume_binding(id).await;
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
    if params.cols == 0 || params.rows == 0 {
        return Err(ProtocolError::bad_request(
            "session.new requires non-zero cols and rows",
        ));
    }
    Ok(())
}

fn build_launch_command(
    agent: AgentKind,
    shell_command: &ShellCommand,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    env_extra: Vec<(String, String)>,
) -> Result<PtyCommand, ProtocolError> {
    match agent {
        // Shell sessions get no hook env; `to_pty_command` carries no env.
        AgentKind::Shell => Ok(shell_command.to_pty_command(cwd, cols, rows)),
        AgentKind::Codex => CodexAdapter.launch(&LaunchOpts {
            cwd,
            cols,
            rows,
            env_extra,
        }),
        AgentKind::Claude => ClaudeAdapter.launch(&LaunchOpts {
            cwd,
            cols,
            rows,
            env_extra,
        }),
    }
}

fn input_rules_for_agent(agent: AgentKind, config: &SessionRegistryConfig) -> InputRules {
    match agent {
        AgentKind::Shell => InputRules {
            bracketed_paste: false,
            submit_delay: Duration::ZERO,
        },
        AgentKind::Codex => CodexAdapter.input_rules(),
        AgentKind::Claude => InputRules {
            submit_delay: config.claude_submit_delay,
            ..ClaudeAdapter.input_rules()
        },
    }
}

fn build_input_writes(text: &str, rules: InputRules) -> InputWritePlan {
    let mut immediate = Vec::new();
    if rules.bracketed_paste {
        immediate.extend_from_slice(BRACKETED_PASTE_START);
    }
    immediate.extend_from_slice(text.as_bytes());
    if rules.bracketed_paste {
        immediate.extend_from_slice(BRACKETED_PASTE_END);
    }

    let delayed_submit = if rules.submit_delay.is_zero() {
        immediate.extend_from_slice(SUBMIT);
        None
    } else {
        Some((rules.submit_delay, SUBMIT.to_vec()))
    };

    InputWritePlan {
        immediate,
        delayed_submit,
    }
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
        PtyError::Spawn { .. } => "spawn_failed",
        PtyError::MissingPid => "spawn_failed",
        PtyError::Io(_) | PtyError::Poisoned | PtyError::ThreadPanicked | PtyError::ExitTimeout => {
            "pty_error"
        }
    };
    ProtocolError::new(ErrorClass::Runtime, code, err.to_string(), None)
}

/// Map a PTY *spawn* failure to a typed protocol error, upgrading a missing-binary
/// (ENOENT) failure to the precise `agent_binary_missing` diagnostic naming the
/// program. Agent launches resolve the binary on `PATH` first (so claude/codex
/// surface `agent_binary_missing` before spawn), but a shell session — or a binary
/// removed between resolution and spawn — only fails here; this gives those the
/// same clear, recoverable diagnostic instead of a generic `spawn_failed`.
fn spawn_error_to_protocol(err: PtyError, program: &str) -> ProtocolError {
    if matches!(
        err,
        PtyError::Spawn {
            not_found: true,
            ..
        }
    ) {
        return ProtocolError::agent_binary_missing(program);
    }
    pty_error_to_protocol(err)
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
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use protocol::{
        AgentKind, SessionId, SessionNewParams, SessionReportNativeIdParams, SessionState,
    };

    use crate::agent::InputRules;
    use crate::integration::{ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH};

    use super::{SessionRegistry, SessionRegistryConfig, ShellCommand};

    fn params() -> SessionNewParams {
        SessionNewParams {
            agent: AgentKind::Shell,
            cwd: Some(PathBuf::from("/tmp")),
            cols: 80,
            rows: 24,
            repo: None,
            branch: None,
            base_branch: None,
        }
    }

    fn temp_store_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pohunek-session-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("metadata.jsonl")
    }

    /// Run git in `dir`, asserting success (test helper for the worktree path).
    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Initialize a throwaway git repo on `main` with one commit, for the
    /// worktree-binding path in `create`.
    fn init_git_repo(tag: &str) -> PathBuf {
        let dir = temp_store_path(tag)
            .parent()
            .expect("store parent")
            .join("repo");
        std::fs::create_dir_all(&dir).expect("create repo dir");
        let init = std::process::Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init", "-q"])
            .arg(&dir)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init failed");
        git_in(&dir, &["config", "user.email", "test@example.com"]);
        git_in(&dir, &["config", "user.name", "Test"]);
        git_in(&dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("README.md"), "init\n").expect("write README");
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    #[tokio::test]
    async fn failed_launch_rolls_back_the_bound_worktree() {
        // Worktree binding persists the branch checkout before the PTY is spawned.
        // A spawn failure (here: a missing shell program) must roll that back, or
        // the orphan worktree keeps the branch checked out and blocks the next
        // `session.new` on it with `worktree_branch_in_use`.
        let repo = init_git_repo("rollback");
        let store = temp_store_path("rollback");
        let worktree_root = store.parent().expect("store parent").join("worktrees");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new(
                "/nonexistent/pohunek-no-such-shell",
                std::iter::empty::<String>(),
            ),
            store_path: Some(store),
            worktree_root: Some(worktree_root.clone()),
            ..SessionRegistryConfig::default()
        });

        let create_params = SessionNewParams {
            cwd: None,
            repo: Some(repo.clone()),
            branch: Some("feat/x".to_owned()),
            ..params()
        };
        let err = registry
            .create(create_params)
            .await
            .expect_err("launch must fail with a missing shell program");
        // A missing program (ENOENT) at spawn surfaces the precise
        // `agent_binary_missing` diagnostic naming the program, with a recover
        // hint — not the generic `spawn_failed`.
        assert_eq!(err.code, "agent_binary_missing", "got: {err:?}");
        assert!(
            err.msg.contains("pohunek-no-such-shell"),
            "error must name the missing program: {err:?}"
        );
        assert!(
            err.recover.is_some(),
            "missing-binary error carries a hint: {err:?}"
        );

        // The worktree bound before the failed spawn must be gone, so its branch
        // is freed for a retry.
        let leftover: Vec<_> = std::fs::read_dir(&worktree_root)
            .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(
            leftover.is_empty(),
            "a failed launch must leave no orphan worktree under {}: {leftover:?}",
            worktree_root.display()
        );

        // And git no longer holds feat/x in any worktree, so a fresh bind succeeds.
        let listing = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("git worktree list");
        let listing = String::from_utf8_lossy(&listing.stdout);
        assert!(
            !listing.contains("feat/x"),
            "branch checkout must be pruned from git's worktree list: {listing}"
        );
    }

    #[tokio::test]
    async fn missing_program_spawn_returns_agent_binary_missing() {
        // A plain shell session whose program does not exist fails at the PTY
        // spawn (ENOENT). That must map to the typed `agent_binary_missing` error
        // naming the program and carrying a recover hint, not `spawn_failed`.
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new(
                "/nonexistent/pohunek-missing-program",
                std::iter::empty::<String>(),
            ),
            ..SessionRegistryConfig::default()
        });

        let err = registry
            .create(params())
            .await
            .expect_err("missing program must fail to spawn");

        assert_eq!(err.code, "agent_binary_missing", "got: {err:?}");
        assert!(
            err.msg.contains("pohunek-missing-program"),
            "error must name the missing program: {err:?}"
        );
        assert!(err.recover.is_some(), "must carry a recover hint: {err:?}");
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
            ..SessionRegistryConfig::default()
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

    #[test]
    fn bracketed_paste_input_frame_wraps_text_and_submit_together() {
        let writes = super::build_input_writes(
            "hello\nworld",
            InputRules {
                bracketed_paste: true,
                submit_delay: Duration::ZERO,
            },
        );

        assert_eq!(
            writes.immediate,
            b"\x1b[200~hello\nworld\x1b[201~\r".to_vec()
        );
        assert_eq!(writes.delayed_submit, None);
    }

    #[test]
    fn delayed_submit_input_frame_splits_text_and_submit() {
        let writes = super::build_input_writes(
            "hello Claude",
            InputRules {
                bracketed_paste: false,
                submit_delay: Duration::from_millis(150),
            },
        );

        assert_eq!(writes.immediate, b"hello Claude".to_vec());
        assert_eq!(
            writes.delayed_submit,
            Some((Duration::from_millis(150), b"\r".to_vec()))
        );
    }

    #[test]
    fn hook_env_injected_for_agents_with_socket_and_absent_for_shell() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            socket_path: Some(PathBuf::from("/run/pohunek/daemon.sock")),
            ..SessionRegistryConfig::default()
        });
        let id = SessionId("s-7".to_owned());

        // Shell never gets hook env.
        assert!(registry.hook_env(AgentKind::Shell, &id).is_empty());

        for agent in [AgentKind::Codex, AgentKind::Claude] {
            let env = registry.hook_env(agent, &id);
            let lookup = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
            assert_eq!(lookup(ENV_FLAG).as_deref(), Some("1"));
            assert_eq!(
                lookup(ENV_SOCKET_PATH).as_deref(),
                Some("/run/pohunek/daemon.sock")
            );
            assert_eq!(lookup(ENV_SESSION_ID).as_deref(), Some("s-7"));
            assert_eq!(
                lookup(ENV_PROTOCOL_VERSION).as_deref(),
                Some(protocol::PROTOCOL_VERSION.get().to_string().as_str())
            );
        }
    }

    #[test]
    fn hook_env_absent_without_configured_socket() {
        let registry = SessionRegistry::default();
        let id = SessionId("s-1".to_owned());
        assert!(registry.hook_env(AgentKind::Claude, &id).is_empty());
        assert!(registry.hook_env(AgentKind::Codex, &id).is_empty());
    }

    #[tokio::test]
    async fn report_native_id_records_binding_and_updates_info() {
        let store_path = temp_store_path("report");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");
        assert_eq!(created.native_session_id, None);

        let result = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: AgentKind::Shell,
                native_session_id: "native-abc".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(result.recorded);

        // In-memory info now carries the native id.
        let inspected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(inspected.native_session_id.as_deref(), Some("native-abc"));

        // The binding was persisted to the store.
        let persisted = crate::store::Store::new(store_path)
            .load_resume()
            .expect("load store");
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].session_id, created.id.0);
        assert_eq!(
            persisted[0].native_session_id.as_deref(),
            Some("native-abc")
        );

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn stopping_a_session_drops_its_resume_binding() {
        let store_path = temp_store_path("drop-on-stop");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: AgentKind::Shell,
                native_session_id: "native-stop".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(recorded.recorded);
        assert_eq!(
            crate::store::Store::new(store_path.clone())
                .load_resume()
                .expect("load")
                .len(),
            1
        );

        // Stopping the session must drop the binding so a restart does not
        // resurrect a session the user ended.
        let stopped = registry.stop(&created.id).await.expect("stop");
        assert!(stopped.stopped);
        assert!(
            crate::store::Store::new(store_path)
                .load_resume()
                .expect("load")
                .is_empty(),
            "stopped session must not leave a resume binding"
        );
    }

    #[tokio::test]
    async fn resize_after_capture_updates_persisted_binding() {
        let store_path = temp_store_path("resize-binding");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");
        // Capture a native id so a resume binding exists at the launch size.
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: AgentKind::Shell,
                native_session_id: "native-resize".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(recorded.recorded);
        let before = crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load before");
        assert_eq!(before.len(), 1);
        assert_eq!((before[0].cols, before[0].rows), (80, 24));

        // Resizing the live session must refresh the persisted dimensions so a
        // restart resumes at the new size, not the stale capture-time size.
        registry
            .resize(&created.id, 132, 50)
            .await
            .expect("resize session");

        let after = crate::store::Store::new(store_path)
            .load_resume()
            .expect("load after");
        assert_eq!(
            after.len(),
            1,
            "resize must upsert, not duplicate: {after:?}"
        );
        assert_eq!(after[0].session_id, created.id.0);
        assert_eq!(after[0].native_session_id.as_deref(), Some("native-resize"));
        assert_eq!(
            (after[0].cols, after[0].rows),
            (132, 50),
            "persisted binding must carry the post-resize dimensions"
        );

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn resize_without_captured_native_id_persists_no_binding() {
        let store_path = temp_store_path("resize-no-binding");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");
        // No native id captured yet: resizing must not fabricate a binding that
        // load_and_resume would later reject as unresumable.
        registry
            .resize(&created.id, 100, 30)
            .await
            .expect("resize session");

        assert!(
            crate::store::Store::new(store_path)
                .load_resume()
                .expect("load")
                .is_empty(),
            "resize without a native id must not create a resume binding"
        );

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn load_and_resume_prunes_structurally_unresumable_bindings() {
        let store_path = temp_store_path("prune-corrupt");
        // A hand-corrupted binding with no native id or path can never resume.
        let store = crate::store::Store::new(store_path.clone());
        store
            .record_resume(&crate::store::ResumeBinding {
                session_id: "s-corrupt".to_owned(),
                agent: AgentKind::Claude,
                cwd: PathBuf::from("/tmp"),
                cols: 80,
                rows: 24,
                native_session_id: None,
                native_session_path: None,
            })
            .expect("seed corrupt binding");

        let registry = SessionRegistry::new(SessionRegistryConfig {
            store_path: Some(store_path.clone()),
            ..SessionRegistryConfig::default()
        });
        registry.load_and_resume().await;

        assert!(
            crate::store::Store::new(store_path)
                .load_resume()
                .expect("load")
                .is_empty(),
            "an unresumable binding must be pruned, not retried forever"
        );
    }

    #[tokio::test]
    async fn concurrent_resize_and_recapture_keep_store_consistent_with_memory() {
        let store_path = temp_store_path("concurrent-persist");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: AgentKind::Shell,
                native_session_id: "native-concurrent".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(recorded.recorded);

        // Race a resize against a second native-id report (SessionStart re-fires
        // on resume/clear/compact). The persisted binding must end at the live
        // size, never the pre-resize one: persist_resume_binding re-reads under
        // persist_lock, so whichever writer runs last reflects the resize.
        let resizer = {
            let registry = registry.clone();
            let id = created.id.clone();
            tokio::spawn(async move { registry.resize(&id, 200, 60).await })
        };
        let recapture = {
            let registry = registry.clone();
            let id = created.id.clone();
            tokio::spawn(async move {
                registry
                    .report_native_id(SessionReportNativeIdParams {
                        session_id: id,
                        agent: AgentKind::Shell,
                        native_session_id: "native-concurrent".to_owned(),
                        transcript_path: None,
                    })
                    .await
            })
        };
        resizer
            .await
            .expect("resize task")
            .expect("resize succeeds");
        recapture.await.expect("recapture task");

        let inspected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!((inspected.cols, inspected.rows), (200, 60));
        let persisted = crate::store::Store::new(store_path)
            .load_resume()
            .expect("load");
        assert_eq!(persisted.len(), 1, "no duplicate binding: {persisted:?}");
        assert_eq!(
            (persisted[0].cols, persisted[0].rows),
            (inspected.cols, inspected.rows),
            "persisted binding must match the live size after a concurrent resize + recapture"
        );

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn resize_then_stop_leaves_no_binding() {
        let store_path = temp_store_path("resize-then-stop");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: AgentKind::Shell,
                native_session_id: "native-resize-stop".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(recorded.recorded);
        registry
            .resize(&created.id, 90, 30)
            .await
            .expect("resize session");
        assert_eq!(
            crate::store::Store::new(store_path.clone())
                .load_resume()
                .expect("load")
                .len(),
            1,
            "resize must refresh the existing binding"
        );

        // Stopping after a resize must still drop the (resize-refreshed) binding.
        let stopped = registry.stop(&created.id).await.expect("stop");
        assert!(stopped.stopped);
        assert!(
            crate::store::Store::new(store_path)
                .load_resume()
                .expect("load")
                .is_empty(),
            "a resized-then-stopped session must not leave a resume binding"
        );
    }

    #[tokio::test]
    async fn report_native_id_ignores_unknown_invalid_and_terminal() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });

        // Unknown session id.
        let unknown = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: SessionId("s-missing".to_owned()),
                agent: AgentKind::Claude,
                native_session_id: "native-1".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(!unknown.recorded);

        let created = registry.create(params()).await.expect("create session");

        // Invalid (empty) native id on a live session.
        let invalid = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: AgentKind::Shell,
                native_session_id: String::new(),
                transcript_path: None,
            })
            .await;
        assert!(!invalid.recorded);

        // Terminal session.
        let _ = registry.stop(&created.id).await;
        let terminal = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: AgentKind::Shell,
                native_session_id: "native-late".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(!terminal.recorded);
    }

    #[test]
    fn claude_input_rules_use_configured_submit_delay() {
        let config = SessionRegistryConfig {
            claude_submit_delay: Duration::from_millis(75),
            ..SessionRegistryConfig::default()
        };

        let rules = super::input_rules_for_agent(AgentKind::Claude, &config);

        assert!(!rules.bracketed_paste);
        assert_eq!(rules.submit_delay, Duration::from_millis(75));
    }
}
