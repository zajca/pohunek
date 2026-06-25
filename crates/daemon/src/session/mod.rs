//! In-memory session registry and supervisor.
//!
//! Milestone 3 keeps session metadata in memory only. Each session owns a PTY
//! handle and has a watcher task that records process exit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::{
    event, AgentActivity, AgentKind, ErrorClass, Event, ProjectRemoveResult, ProtocolError,
    SessionAttachParams, SessionId, SessionInfo, SessionInputParams, SessionInputResult,
    SessionNewParams, SessionReportNativeIdParams, SessionReportNativeIdResult, SessionState,
    SessionStopResult, SessionWarning, StateSource, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc, watch, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::agent::{
    adapter_for, agent_not_resumable, base_resume_template, build_pty_command, default_program,
    launch_adapter_for, resume_pty_command_from_template, AgentAdapter, AgentCommand, InputRules,
    LaunchOpts, ProfileRegistry, ResolvedAgent, ResumeTemplate, SessionRef, SessionRefKind,
};
use crate::detect::{ActivityTransition, Detector, DetectorConfig, Manifest};
use crate::integration::{
    ENV_DAEMON_ID, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
};
use crate::project::detect::DetectedProject;
use crate::project::{detect_at, ProjectManager};
use crate::pty::{PtyCommand, PtyError, PtyExit, PtyHandle};
use crate::store::{ProjectRecord, ResumeBinding, Store};
use crate::worktree::{
    canonical_or_original, run_hook, HookContext, HookEvent, WorktreeManager, WorktreeRequest,
};

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
/// Overridable via [`SessionRegistryConfig::hook_timeout`].
const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(300);

/// Default grace period to wait for a freshly spawned agent to produce its first
/// PTY output before injecting a `session.new --input` prompt. It is an upper
/// bound, not a fixed delay: the input is sent as soon as the agent emits any
/// output (proxy for "TUI started, stdin reader ready") or this elapses,
/// whichever comes first. Overridable via
/// [`SessionRegistryConfig::initial_input_startup_grace`].
const DEFAULT_INITIAL_INPUT_STARTUP_GRACE: Duration = Duration::from_millis(500);

/// Default minimum interval between detector "PTY output lag" WARN logs per
/// session. The first lag in a window logs immediately; further lags are counted
/// and folded into one summary WARN once the window elapses. A runaway session
/// (e.g. a self-feeding attach loop) overflows the detector's broadcast channel
/// continuously, so unthrottled logging would flood the log; the detector still
/// resyncs on every lag — only the logging is rate-limited. Overridable via
/// [`SessionRegistryConfig::detector_lag_warn_interval`].
const DEFAULT_DETECTOR_LAG_WARN_INTERVAL: Duration = Duration::from_secs(5);
/// Debounce window for session-layer `agent-state` hooks. The detector/event log
/// still sees every transition immediately; only hook side effects wait briefly
/// so a short-lived visual flap does not run a hook for each intermediate value.
const AGENT_STATE_HOOK_DEBOUNCE: Duration = Duration::from_millis(50);

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
}

impl Default for ShellCommand {
    fn default() -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        Self::new(shell, std::iter::empty::<String>())
    }
}

impl AgentAdapter for ShellCommand {
    fn id(&self) -> &str {
        "shell"
    }

    fn launch(&self, opts: &LaunchOpts) -> Result<PtyCommand, ProtocolError> {
        crate::agent::build_pty_command(&self.program, self.args.clone(), opts)
    }

    fn input_rules(&self) -> InputRules {
        InputRules {
            bracketed_paste: false,
            submit_delay: Duration::ZERO,
        }
    }

    fn manifest(&self) -> &crate::detect::Manifest {
        crate::detect::generic_shell_manifest()
    }

    fn resume(&self, _session_ref: &SessionRef) -> Result<AgentCommand, ProtocolError> {
        Err(crate::agent::agent_not_resumable(self.id()))
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
    /// Upper bound on how long [`SessionRegistry::create`] waits for a freshly
    /// spawned agent to emit its first PTY output before injecting a
    /// `session.new --input` prompt. The wait short-circuits as soon as the
    /// agent produces any output, so this caps the delay rather than imposing
    /// it; a value of `Duration::ZERO` disables the gate and injects
    /// immediately. Prevents the prompt from being delivered to a TUI that has
    /// not yet entered raw/bracketed-paste input mode (Codex has a zero submit
    /// delay, so without this its bracketed-paste prompt can race startup).
    pub initial_input_startup_grace: Duration,
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
    /// Wall-clock bound on each worktree/session lifecycle hook. A hook that
    /// exceeds it is terminated and recorded/logged as a non-fatal hook warning,
    /// so a hanging hook can never wedge a session operation. Defaults to
    /// [`DEFAULT_HOOK_TIMEOUT`].
    pub hook_timeout: Duration,
    /// Directory for the append-only event log (`<data_dir>/events`). `None`
    /// disables event logging. Started via [`SessionRegistry::spawn_event_log`].
    pub event_log_dir: Option<PathBuf>,
    /// Host config directory (`<config_dir>` = `$XDG_CONFIG_HOME/pohunek` or
    /// `~/.config/pohunek`). The host-default layer for templates/actions/prompts
    /// (Part A), lifecycle hooks (Part B), and agent profiles (Part C). `None`
    /// disables the host-default layer (e.g. unit tests that exercise only the
    /// in-repo layer). Read through [`SessionRegistry::config_dir`].
    pub config_dir: Option<PathBuf>,
    /// Directory holding host agent profiles (`<config_dir>/agents`). `None`
    /// disables host profiles (a bare `shell`/`codex`/`claude` still resolves).
    /// Part C: a profile extends a base kind with program/args/env/input-rules.
    pub agents_dir: Option<PathBuf>,
    /// Minimum interval between per-session "PTY output lag" WARN logs. The first
    /// lag in each window logs immediately; further lags are folded into one
    /// summary WARN when the window elapses, so a runaway session cannot flood the
    /// log. Defaults to [`DEFAULT_DETECTOR_LAG_WARN_INTERVAL`].
    pub detector_lag_warn_interval: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputWritePlan {
    immediate: Vec<u8>,
    delayed_submit: Option<(Duration, Vec<u8>)>,
}

/// Rate limiter for the per-session "PTY output lag" WARN.
///
/// A runaway session (e.g. a self-feeding attach loop, before the
/// [`attach_self_feedback`] guard catches it, or any other output storm)
/// overflows the detector's bounded broadcast channel continuously, so logging
/// every overflow would bury the log. The first lag of a *storm* logs
/// immediately; every further lag less than `interval` after the previous one is
/// folded into the storm and reported as ONE summary per window — flushed by
/// [`Self::poll`] (the detector's periodic tick, once the window elapses) or by
/// [`Self::flush`] (session teardown), so a quiesced or killed-mid-storm session
/// still reports its trailing batch instead of silently dropping it. A lag that
/// arrives at least `interval` after the previous one starts a *new* storm and so
/// logs immediately again. At most one line per `interval`. It is pure and
/// `Instant`-fed, so it is unit-testable without real time; the detector still
/// calls `resync_after_lag()` on every lag, so only the logging is throttled,
/// never the recovery.
#[derive(Debug)]
struct LagWarnThrottle {
    interval: Duration,
    /// Start of the current summary window; `None` when no window is open (before
    /// the first lag, and after a window is flushed/closed). Drives [`Self::poll`]
    /// timing only — *not* the first-vs-fold decision, which uses
    /// [`Self::last_lag_at`] so a fresh storm after a quiet gap still logs a
    /// `First` even while a previous window is technically still open.
    window_started: Option<Instant>,
    /// Instant of the most recently observed lag; `None` until the first. Used to
    /// decide whether a lag continues the current storm (gap `< interval`) or
    /// starts a new one (gap `>= interval`).
    last_lag_at: Option<Instant>,
    /// Lag events folded into the current window since its first (already-logged)
    /// lag.
    pending_events: u64,
    /// Total chunks skipped across [`Self::pending_events`].
    pending_skipped: u64,
}

/// A line the detector loop should log for PTY output lag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LagWarn {
    /// First lag of a storm: log it immediately with its own skip count.
    First { skipped: u64 },
    /// One summary of the lags folded into a window after its first.
    Summary { events: u64, skipped: u64 },
}

impl LagWarnThrottle {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            window_started: None,
            last_lag_at: None,
            pending_events: 0,
            pending_skipped: 0,
        }
    }

    /// Record one lag observed at `now` that dropped `skipped` chunks.
    ///
    /// Returns the line to log, if any: the first lag of a storm logs immediately;
    /// later lags (less than `interval` after the previous one) fold silently and
    /// are reported by [`Self::poll`] or [`Self::flush`]. A lag at least `interval`
    /// after the previous one is treated as a new storm and logs immediately. A
    /// zero interval disables folding entirely (every lag logs immediately).
    fn observe(&mut self, now: Instant, skipped: u64) -> Option<LagWarn> {
        if self.interval.is_zero() {
            return Some(LagWarn::First { skipped });
        }
        // A lag continues the current storm only if the previous lag is less than
        // one interval old; otherwise (no prior lag, or a long quiet gap) it is the
        // first lag of a fresh storm and must log immediately. Keying this on the
        // last lag — not on `window_started` — means a new burst after silence is
        // never mislabeled as a continuation just because `poll` left a window open.
        let continues_storm = match self.last_lag_at {
            Some(last) => now.saturating_duration_since(last) < self.interval,
            None => false,
        };
        self.last_lag_at = Some(now);
        if continues_storm {
            self.pending_events = self.pending_events.saturating_add(1);
            self.pending_skipped = self.pending_skipped.saturating_add(skipped);
            if self.window_started.is_none() {
                self.window_started = Some(now);
            }
            None
        } else {
            self.window_started = Some(now);
            self.pending_events = 0;
            self.pending_skipped = 0;
            Some(LagWarn::First { skipped })
        }
    }

    /// Flush a window whose `interval` has elapsed, called on the detector's
    /// periodic tick so a folded batch is reported even when no further lag
    /// arrives. Emits a summary if any lags were folded, then opens a fresh
    /// window; if the elapsed window folded nothing it is simply closed (its first
    /// lag was already logged), so the next lag logs as a fresh `First`.
    fn poll(&mut self, now: Instant) -> Option<LagWarn> {
        let started = self.window_started?;
        if now.saturating_duration_since(started) < self.interval {
            return None;
        }
        if self.pending_events > 0 {
            let summary = LagWarn::Summary {
                events: self.pending_events,
                skipped: self.pending_skipped,
            };
            self.window_started = Some(now);
            self.pending_events = 0;
            self.pending_skipped = 0;
            Some(summary)
        } else {
            self.window_started = None;
            None
        }
    }

    /// Flush any folded lags unconditionally (used at session teardown, when the
    /// window may never elapse because the session died mid-storm). Emits the
    /// trailing summary if any lags were folded, then resets.
    fn flush(&mut self) -> Option<LagWarn> {
        if self.pending_events > 0 {
            let summary = LagWarn::Summary {
                events: self.pending_events,
                skipped: self.pending_skipped,
            };
            self.window_started = None;
            self.pending_events = 0;
            self.pending_skipped = 0;
            Some(summary)
        } else {
            None
        }
    }
}

/// Emit the WARN line for one [`LagWarn`] decision, tagged with the session id.
fn log_lag_warn(session_id: &SessionId, warn_kind: LagWarn) {
    match warn_kind {
        LagWarn::First { skipped } => warn!(
            session_id = %session_id.0,
            skipped,
            "resyncing detector state after PTY output lag"
        ),
        LagWarn::Summary { events, skipped } => warn!(
            session_id = %session_id.0,
            lag_events = events,
            skipped,
            "PTY output lag persisting; detector kept resyncing (summary since last log)"
        ),
    }
}

/// Everything needed to spawn and register one PTY-backed session, shared by
/// first launch (`create`) and resume (`resume_binding`).
#[derive(Debug)]
struct PtySessionSpec {
    id: SessionId,
    /// Resolved agent NAME (a host-profile name, or a bare base-kind name).
    agent: String,
    /// Resolved base kind backing the agent (detection/resume/handshake env).
    agent_base: AgentKind,
    /// Input-framing rules for this session (base-kind defaults, profile-overridden).
    input_rules: InputRules,
    /// Frozen structural relaunch snapshot (C.4): launch program/args + the resolved
    /// resume template (`None` ⇒ not resumable). Persisted verbatim so a restart
    /// resumes with the launch-time shape even after the profile changes.
    snapshot: ResumeSnapshot,
    /// Detection-manifest override (a profile's `manifest =`), threaded to the
    /// detector at spawn. `None` ⇒ inherit the base kind's manifest. Re-resolved by
    /// agent name on the resume path (never persisted).
    manifest_override: Option<Manifest>,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    command: PtyCommand,
    /// Native id when relaunching a captured session (`None` on first launch).
    native_session_id: Option<String>,
    /// Native transcript path when relaunching a path-resuming captured session.
    native_session_path: Option<String>,
    /// Project this session belongs to (derived id), when one was resolved.
    project_id: Option<String>,
    /// Whether the session's checkout is a linked worktree (`None` if no git).
    is_linked_worktree: Option<bool>,
    /// Source repository, when the session is bound to a worktree.
    repo: Option<PathBuf>,
    /// Branch checked out in the bound worktree.
    branch: Option<String>,
    /// Bound worktree path (equal to `cwd` for worktree sessions).
    worktree_path: Option<PathBuf>,
    /// Non-fatal worktree-setup warnings to surface on the session.
    warnings: Vec<SessionWarning>,
}

/// Frozen structural relaunch snapshot for a session (Part C, C.4).
///
/// Set once at launch from the [`ResolvedAgent`] and persisted verbatim on every
/// resume-binding write, so a daemon restart relaunches with the original launch
/// program/args + resume mechanics even after the host profile is edited or
/// deleted. Deliberately holds **no env** — that is re-resolved by agent name at
/// resume (it may carry secrets, which never touch the store).
#[derive(Debug, Clone)]
struct ResumeSnapshot {
    /// Launch program (the profile's `program` or the base kind's default).
    program: String,
    /// Launch args (the profile's `args`; empty for a bare base kind).
    args: Vec<String>,
    /// Resolved resume template; `None` ⇒ this session does not resume.
    resume: Option<ResumeTemplate>,
}

/// The resolved launch target for a `session.new`: where the agent runs and the
/// project/worktree metadata to stamp on the session (see
/// [`SessionRegistry::resolve_target`]).
#[derive(Debug)]
struct TargetResolution {
    /// Directory the agent is launched in (an in-place checkout or a worktree).
    launch_cwd: PathBuf,
    /// Source repository, set for a worktree session.
    repo: Option<PathBuf>,
    /// Branch checked out in the bound worktree, set for a worktree session.
    branch: Option<String>,
    /// Bound worktree path, set for a worktree session.
    worktree_path: Option<PathBuf>,
    /// Project the session belongs to (derived id), when one resolved.
    project_id: Option<String>,
    /// Whether the checkout is a linked worktree (`None` if no git identity).
    is_linked_worktree: Option<bool>,
    /// Non-fatal worktree-setup warnings to surface on the session.
    warnings: Vec<SessionWarning>,
    /// Whether a worktree was actually bound (drives launch-failure rollback).
    worktree_bound: bool,
}

impl Default for SessionRegistryConfig {
    fn default() -> Self {
        Self {
            shell_command: ShellCommand::default(),
            stop_grace: Duration::from_millis(500),
            attach_token_ttl: DEFAULT_ATTACH_TOKEN_TTL,
            output_history_limit_bytes: DEFAULT_OUTPUT_HISTORY_LIMIT_BYTES,
            claude_submit_delay: crate::agent::DEFAULT_CLAUDE_SUBMIT_DELAY,
            initial_input_startup_grace: DEFAULT_INITIAL_INPUT_STARTUP_GRACE,
            socket_path: None,
            store_path: None,
            worktree_root: None,
            hook_timeout: DEFAULT_HOOK_TIMEOUT,
            event_log_dir: None,
            config_dir: None,
            agents_dir: None,
            detector_lag_warn_interval: DEFAULT_DETECTOR_LAG_WARN_INTERVAL,
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
    /// Opaque id unique to this daemon process instance, injected into every
    /// session PTY as `POHUNEK_DAEMON_ID` and compared against the attach origin
    /// so the self-feeding-attach guard fires only for this instance's own PTYs
    /// (see [`SessionRegistry::attach`]). Regenerated each start; never persisted.
    daemon_instance_id: String,
    config: SessionRegistryConfig,
    /// Resolves the free-string `agent` name to a base kind + optional host-profile
    /// overrides (Part C). Built from `config.agents_dir` at construction.
    profiles: ProfileRegistry,
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
    /// Project store glue (auto-registration + reference resolution), present
    /// when the metadata store is configured. Shares the same `Arc<Store>` as the
    /// resume/worktree records, and is shared into `spawn_blocking` for the
    /// (blocking) git detection + store I/O.
    projects: Option<Arc<ProjectManager>>,
    /// Cancellation signal for the event-log drain, fired by
    /// [`SessionRegistry::shutdown_event_log`] so the drain flushes its backlog
    /// and exits cleanly at shutdown.
    event_log_shutdown: CancellationToken,
    /// Join handle of the spawned event-log drain task, awaited at shutdown.
    event_log_task: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// Cancellation signal for the agent-state hook dispatcher.
    agent_state_hook_shutdown: CancellationToken,
    /// Join handle of the spawned agent-state hook dispatcher.
    agent_state_hook_task: std::sync::Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug, Clone)]
struct AgentStateHookSnapshot {
    session_id: SessionId,
    project_id: Option<String>,
    cwd: PathBuf,
    agent: String,
    activity: AgentActivity,
}

#[derive(Debug, Clone)]
struct SessionHookRequest {
    event: HookEvent,
    cwd: PathBuf,
    session_id: String,
    project_id: Option<String>,
    agent: String,
    stop_reason: Option<&'static str>,
    activity: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct SessionEntry {
    info: SessionInfo,
    pty: PtyHandle,
    detector_cancel: CancellationToken,
    detector_resize: watch::Sender<(u16, u16)>,
    stopping: bool,
    /// Resolved input-framing rules (base-kind defaults, profile-overridden), used
    /// by `session.input` so a profile's `[input_rules]` is honored on every write.
    input_rules: InputRules,
    /// Frozen structural relaunch snapshot (C.4), set once at register time and
    /// copied verbatim into every persisted [`ResumeBinding`] — so a resize-driven
    /// re-persist can never overwrite the launch-time program/args/resume shape.
    snapshot: ResumeSnapshot,
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
                config.hook_timeout,
                config.config_dir.clone(),
            ))),
            _ => None,
        };
        // The project manager shares the same store as resume/worktree records, so
        // it is enabled exactly when persistence is. Projects need no worktree root.
        let projects = store
            .clone()
            .map(|store| Arc::new(ProjectManager::new(store)));
        // Host agent profiles resolve the free-string `agent` name; built from the
        // configured agents dir (a bare base kind still resolves when it is unset).
        let profiles = ProfileRegistry::new(config.agents_dir.clone());
        Self {
            inner: Arc::new(SessionRegistryInner {
                sessions: Mutex::new(HashMap::new()),
                pending_attaches: Mutex::new(HashMap::new()),
                active_attaches: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                next_stream_id: AtomicU64::new(1),
                daemon_instance_id: generate_daemon_instance_id(),
                config,
                profiles,
                events,
                store,
                persist_lock: Mutex::new(()),
                worktree,
                projects,
                event_log_shutdown: CancellationToken::new(),
                event_log_task: std::sync::Mutex::new(None),
                agent_state_hook_shutdown: CancellationToken::new(),
                agent_state_hook_task: std::sync::Mutex::new(None),
            }),
        }
    }

    /// Subscribe to session lifecycle events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// The host config directory (`$XDG_CONFIG_HOME/pohunek` or `~/.config/pohunek`),
    /// or `None` when the host-default layer is disabled. The single read API for the
    /// host-default layer used by Part A's `project.*` handlers, Part B's host-global
    /// hooks, and Part C's agent profiles — no consumer re-derives the path.
    #[must_use]
    pub fn config_dir(&self) -> Option<&Path> {
        self.inner.config.config_dir.as_deref()
    }

    /// The resolved host agent-profile registry (Part C), for `host.inspect` to
    /// enumerate the launchable agent names + probe each profile's program.
    #[must_use]
    pub(crate) fn profiles(&self) -> &ProfileRegistry {
        &self.inner.profiles
    }

    /// This daemon process instance's opaque id, injected into every session PTY
    /// as `POHUNEK_DAEMON_ID` and matched against the attach origin by the
    /// self-feeding-attach guard (see [`Self::attach`]). Exposed so a client (and
    /// tests) can correlate an origin to this instance.
    #[must_use]
    pub fn daemon_instance_id(&self) -> &str {
        &self.inner.daemon_instance_id
    }

    /// The project manager, when the metadata store is configured. Exposed for
    /// the `project.*` control handlers, which share the same store the session
    /// registry writes resume/worktree records through.
    #[must_use]
    pub fn projects(&self) -> Option<Arc<ProjectManager>> {
        self.inner.projects.clone()
    }

    /// Forget a project (`project rm`), optionally pruning the worktrees pohunek
    /// created for it (`--prune-worktrees`). Orchestrates the two subsystems this
    /// registry owns — the project store and the worktree manager — on a blocking
    /// thread: resolve the reference, prune owned worktrees (only those with a
    /// binding for the project; never the main checkout or unowned trees), then
    /// remove the record — **unless** a worktree was skipped because a live session
    /// is still using it, in which case the record is kept (`removed: false`) so its
    /// surviving bindings keep pointing at a real project; a later `rm` succeeds
    /// once those sessions stop. A plain `rm` (no prune) only forgets the record.
    pub async fn remove_project(
        &self,
        reference: &str,
        prune_worktrees: bool,
    ) -> Result<ProjectRemoveResult, ProtocolError> {
        let Some(projects) = self.inner.projects.clone() else {
            return Err(runtime_error(
                "projects_not_configured",
                "the daemon is not configured for projects (no metadata store)",
            ));
        };
        let worktree = self.inner.worktree.clone();
        // Gather the worktree paths of LIVE sessions up front (the session map is
        // an async lock we cannot take inside `spawn_blocking`). The prune skips a
        // worktree a live session is still using, so its checkout is not pulled out
        // from under it; an in-place session (no worktree path) never blocks a
        // prune. "Live" is the non-terminal set (`Starting`/`Running`) — a session
        // still starting up holds its worktree too — matching `project show`'s own
        // live-session filter. Keyed canonical so it matches the binder's paths.
        let live: Vec<(PathBuf, String)> = if prune_worktrees {
            self.list()
                .await
                .into_iter()
                .filter(|session| !session.state.is_terminal())
                .filter_map(|session| {
                    session
                        .worktree_path
                        .map(|path| (canonical_or_original(&path), session.id.0))
                })
                .collect()
        } else {
            Vec::new()
        };
        let reference = reference.to_owned();
        tokio::task::spawn_blocking(move || -> Result<ProjectRemoveResult, ProtocolError> {
            // Resolve first so a missing/ambiguous reference errors before any
            // worktree is touched, and so we have the id to scope the prune.
            let record = projects.resolve(&reference)?;
            let (pruned_worktrees, skipped_worktrees) = if prune_worktrees {
                match &worktree {
                    Some(manager) => {
                        let skip: HashSet<PathBuf> =
                            live.iter().map(|(path, _)| path.clone()).collect();
                        // Remove-hook warnings have no field on the prune response, so
                        // log them: the prune is a fire-and-forget admin action.
                        let mut hook_warnings = Vec::new();
                        let prune =
                            manager.cleanup_project(&record.id(), &skip, &mut hook_warnings)?;
                        for warning in &hook_warnings {
                            warn!(
                                project_id = %record.id(),
                                warning = %warning.message,
                                detail = ?warning.detail,
                                "remove hook warning during project prune"
                            );
                        }
                        // Map each skipped worktree path back to its live session id.
                        let skipped = prune
                            .skipped
                            .iter()
                            .filter_map(|path| {
                                live.iter()
                                    .find(|(live_path, _)| live_path == path)
                                    .map(|(_, id)| id.clone())
                            })
                            .collect();
                        (prune.removed, skipped)
                    }
                    // No worktree manager ⇒ no worktrees were ever created.
                    None => (0, Vec::new()),
                }
            } else {
                (0, Vec::new())
            };
            // Option (b): a skipped worktree means a live session still depends on
            // this project, so forgetting the record would leave that worktree's
            // binding pointing at a project that no longer exists. Keep the record
            // (removed = false) and report the skips; the operator retries `rm`
            // once those sessions stop. Only when nothing was skipped is the record
            // actually forgotten.
            let removed = if skipped_worktrees.is_empty() {
                projects.remove(&reference)?
            } else {
                false
            };
            Ok(ProjectRemoveResult {
                removed,
                pruned_worktrees,
                skipped_worktrees,
            })
        })
        .await
        .map_err(|_| runtime_error("project_remove_failed", "project remove task panicked"))?
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

    /// Start the agent-state hook dispatcher.
    ///
    /// This task subscribes to the registry's event stream, filters only
    /// `agent_state`, deduplicates by last-fired activity value, and runs
    /// `agent-state` hooks off the broadcast hot path. A no-op if it is already
    /// running.
    pub fn spawn_agent_state_hooks(&self) {
        let mut slot = self
            .inner
            .agent_state_hook_task
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if slot.is_some() {
            return;
        }
        let handle = spawn_agent_state_hook_dispatcher(
            self.clone(),
            self.subscribe(),
            self.inner.agent_state_hook_shutdown.clone(),
        );
        *slot = Some(handle);
    }

    /// Stop the agent-state hook dispatcher and flush any pending debounced
    /// activity values before returning.
    pub async fn shutdown_agent_state_hooks(&self) {
        self.inner.agent_state_hook_shutdown.cancel();
        let handle = self
            .inner
            .agent_state_hook_task
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(handle) = handle {
            if tokio::time::timeout(EVENT_LOG_FLUSH_TIMEOUT, handle)
                .await
                .is_err()
            {
                warn!("agent-state hook dispatcher did not finish within the shutdown timeout");
            }
        }
    }

    /// Create a new PTY-backed session.
    ///
    /// Resolves the session's **target** (design Decisions 1 & 3): which project
    /// it belongs to (a `--project` reference, else auto-detected from `--repo`/
    /// `--cwd`) and where the agent runs — **in-place** in the project's checkout
    /// (no `--branch`) or in a **dedicated worktree** bound for `(session, repo,
    /// branch)` (with `--branch`). A non-git directory yields a plain shell with
    /// no project. The session records `project_id`/`is_linked_worktree`, and any
    /// non-fatal worktree warnings ride along on the returned [`SessionInfo`].
    pub async fn create(&self, params: SessionNewParams) -> Result<SessionInfo, ProtocolError> {
        validate_new_params(&params)?;
        let initial_input = params.input.clone();
        // Fallback launch dir for a no-project (plain shell) session: the CLI's
        // own cwd for a local session, else the daemon's. A resolved project
        // overrides this with its checkout (or worktree) path.
        let fallback_cwd = match params.cwd.clone() {
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

        let TargetResolution {
            launch_cwd,
            repo,
            branch,
            worktree_path,
            project_id,
            is_linked_worktree,
            warnings,
            worktree_bound,
        } = self.resolve_target(&id, &params, fallback_cwd).await?;

        // When a worktree was bound its branch is now checked out. Any failure
        // building the launch command or spawning the PTY must roll that back: a
        // leftover worktree keeps the branch checked out and blocks the next
        // `session.new` on it with `worktree_branch_in_use` (an orphan a fresh
        // session id would never reuse). Compensate here — not in
        // `register_pty_session`, which `resume_binding` shares and where the
        // worktree must be kept.
        let launch = async {
            // Resolve the free-string agent name to a base kind + optional host
            // profile (Part C). A bad name / missing profile fails here and rolls
            // back any bound worktree, like any other launch failure.
            let resolved = self.inner.profiles.resolve_agent(&params.agent)?;
            let base = resolved.base;
            let input_rules = resolved
                .profile
                .as_ref()
                .and_then(|profile| profile.input_rules)
                .unwrap_or_else(|| input_rules_for_agent(base, &self.inner.config));
            // Freeze the structural relaunch snapshot (C.4) from the resolved agent:
            // the launch program/args plus the resume template (a profile's override,
            // else the base kind's). Cloned/copied so `resolved` stays usable below.
            let snapshot = ResumeSnapshot {
                program: resolved
                    .profile
                    .as_ref()
                    .map_or_else(|| default_program(base), |profile| profile.program.clone()),
                args: resolved
                    .profile
                    .as_ref()
                    .map_or_else(Vec::new, |profile| profile.args.clone()),
                resume: match &resolved.profile {
                    Some(profile) => profile.resume,
                    None => base_resume_template(base),
                },
            };
            // The detection-manifest override is consumed only by the detector, on
            // both the launch and resume paths; never persisted (re-resolved by name).
            let manifest_override = resolved
                .profile
                .as_ref()
                .and_then(|profile| profile.manifest.clone());
            // Profile env first, then the daemon handshake env appended last, so
            // every reserved POHUNEK_* key takes the daemon's value (last-write-wins;
            // the loader also strips POHUNEK_* from the profile env up front).
            let mut env_extra = resolved
                .profile
                .as_ref()
                .map(|profile| profile.env.clone())
                .unwrap_or_default();
            env_extra.extend(self.session_pty_env(base, &id));
            let command = build_launch_command(
                &resolved,
                &self.inner.config.shell_command,
                launch_cwd.clone(),
                params.cols,
                params.rows,
                env_extra,
            )?;

            self.register_pty_session(PtySessionSpec {
                id: id.clone(),
                agent: resolved.name.clone(),
                agent_base: base,
                input_rules,
                snapshot,
                manifest_override,
                cwd: launch_cwd,
                cols: params.cols,
                rows: params.rows,
                command,
                native_session_id: None,
                native_session_path: None,
                project_id,
                is_linked_worktree,
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
        let info = launch?;
        self.spawn_session_hook(SessionHookRequest {
            event: HookEvent::SessionStart,
            cwd: info.cwd.clone(),
            session_id: info.id.0.clone(),
            project_id: info.project_id.clone(),
            agent: info.agent.clone(),
            stop_reason: None,
            activity: None,
        });
        if let Some(input) = initial_input {
            // Wait for the agent to come up before injecting the first prompt so
            // the bytes are not delivered to a stdin reader that has not yet
            // entered raw/bracketed-paste mode (and would drop or mis-frame
            // them). Bounded, so a silent agent can never wedge `session.new`.
            self.await_initial_input_readiness(&info.id).await;
            if let Err(err) = self.write_input_to_session(&info.id, &input).await {
                // A failed initial input must roll the session back completely.
                // `stop()` alone does not free a bound worktree, so a leftover
                // checkout would keep the branch checked out and block the next
                // `session.new` on it with `worktree_branch_in_use` — exactly
                // the orphan the launch-failure path above compensates for.
                self.rollback_failed_initial_input(&info.id, worktree_bound)
                    .await;
                return Err(err);
            }
        }
        Ok(info)
    }

    /// Fire a session-layer lifecycle hook without blocking the async hot path.
    ///
    /// The hook runner itself is synchronous (process spawn + bounded wait), so
    /// session events hand it to the blocking pool and only log non-fatal
    /// warnings. Worktree hook call sites can return warnings to `session.new`;
    /// session-start/stop/agent-state have no response field to carry them.
    fn spawn_session_hook(&self, request: SessionHookRequest) {
        drop(self.spawn_session_hook_task(request));
    }

    fn spawn_session_hook_task(&self, request: SessionHookRequest) -> JoinHandle<()> {
        let timeout = self.inner.config.hook_timeout;
        let config_dir = self.inner.config.config_dir.clone();
        tokio::task::spawn_blocking(move || {
            let ctx = HookContext {
                session_id: request.session_id,
                project_id: request.project_id,
                agent: request.agent,
                repo: None,
                worktree: None,
                branch: None,
                base_branch: None,
                stop_reason: request.stop_reason,
                activity: request.activity,
            };
            let mut warnings = Vec::new();
            run_hook(
                request.event,
                &request.cwd,
                &ctx,
                timeout,
                config_dir.as_deref(),
                &mut warnings,
            );
            for warning in warnings {
                warn!(
                    event = request.event.as_env(),
                    session_id = %ctx.session_id,
                    cwd = %request.cwd.display(),
                    warning = %warning.message,
                    detail = ?warning.detail,
                    "session hook warning"
                );
            }
        })
    }

    /// Wait for a freshly spawned agent to produce its first PTY output before a
    /// `session.new --input` prompt is injected, capped at
    /// [`SessionRegistryConfig::initial_input_startup_grace`].
    ///
    /// First output is a robust, agent-agnostic proxy for "the TUI has started
    /// and its stdin reader is in raw/bracketed-paste mode". The wait
    /// short-circuits the instant any output arrives (or has already arrived),
    /// and returns after the grace period even if the agent stays silent, so it
    /// only ever delays — never blocks — the create round-trip. A zero grace
    /// disables the gate.
    async fn await_initial_input_readiness(&self, session_id: &SessionId) {
        let grace = self.inner.config.initial_input_startup_grace;
        if grace.is_zero() {
            return;
        }
        let mut output = {
            let sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get(session_id) else {
                return;
            };
            // Atomic snapshot + subscribe: if the agent has already emitted
            // output it is up, so inject immediately; otherwise wait for the
            // first live chunk.
            let (history, receiver) = entry.pty.attach_snapshot_and_subscribe();
            if !history.is_empty() {
                return;
            }
            receiver
        };
        let _ = tokio::time::timeout(grace, async {
            loop {
                match output.recv().await {
                    Ok(_) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;
    }

    /// Roll back a session whose initial `--input` injection failed: stop the
    /// PTY and, when the session bound a worktree, free that worktree too.
    ///
    /// `stop()` only terminates the PTY and records the exit; it never releases
    /// a bound worktree (that is `cleanup_bound_worktree`'s job, otherwise only
    /// reached by the launch-failure path). Skipping it here would leak the
    /// checkout and block the branch's next `session.new`. Best-effort: a stop
    /// failure is logged, never propagated, so the caller still returns the
    /// original input error.
    async fn rollback_failed_initial_input(&self, id: &SessionId, worktree_bound: bool) {
        if let Err(err) = self.stop(id).await {
            warn!(
                session_id = %id.0,
                error = %err,
                "failed to stop session while rolling back a failed initial input"
            );
        }
        if worktree_bound {
            self.cleanup_bound_worktree(id).await;
        }
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
        match tokio::task::spawn_blocking(move || {
            // Remove-hook warnings on the rollback path are logged (the launch error
            // is what the caller surfaces).
            let mut hook_warnings = Vec::new();
            let result = manager.cleanup_session(&session_id, &mut hook_warnings);
            for warning in &hook_warnings {
                warn!(
                    session_id = %session_id,
                    warning = %warning.message,
                    detail = ?warning.detail,
                    "remove hook warning during worktree rollback"
                );
            }
            result
        })
        .await
        {
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

    /// Resolve the session's target: the project it belongs to and where the
    /// agent runs (in-place checkout vs a freshly bound worktree), per Decisions
    /// 1 & 3. Runs the blocking git/store work on blocking threads.
    async fn resolve_target(
        &self,
        id: &SessionId,
        params: &SessionNewParams,
        fallback_cwd: PathBuf,
    ) -> Result<TargetResolution, ProtocolError> {
        // `base_branch` only branches a worktree; it is meaningless in-place.
        if params.base_branch.is_some() && params.branch.is_none() {
            return Err(ProtocolError::bad_request(
                "session.new base_branch requires branch",
            ));
        }

        // Phase 1: resolve the project (by reference, else by detecting a path).
        let (project, detected) = self.resolve_project(params).await?;

        // Phase 2: isolation (Decision 3).
        let Some(branch) = params.branch.clone() else {
            // A bare project has no working tree, so an in-place agent would land
            // in the bare git dir (objects/refs, no files) — useless. Refuse and
            // steer to `--branch`, which takes the worktree path below (a worktree
            // can be added off a bare repo). Detection auto-registers a bare repo
            // with `is_bare`, and a `--project` reference carries it too, so this
            // one check covers both ways a bare project reaches an in-place start.
            if project.as_ref().is_some_and(|record| record.is_bare) {
                return Err(ProtocolError::bad_request(
                    "cannot start an in-place session in a bare repository; \
                     use --branch to create a worktree",
                ));
            }
            return Ok(self.in_place_target(project, detected, fallback_cwd));
        };

        // Worktree-per-session. The source repo is an explicit `--repo`, else the
        // resolved project's main checkout; the base is `--base-branch`, else the
        // project's configured default (`None` ⇒ the repo's HEAD at creation). The
        // project id is stamped onto both the session and the worktree binding.
        let project_id = project.as_ref().map(ProjectRecord::id);
        let repo = params
            .repo
            .clone()
            .or_else(|| project.as_ref().map(|record| record.repo_root.clone()))
            .ok_or_else(|| {
                ProtocolError::bad_request(
                    "session.new branch requires --repo or a resolvable --project",
                )
            })?;
        let base_branch = params
            .base_branch
            .clone()
            .or_else(|| project.as_ref().and_then(|r| r.default_base_branch.clone()));
        let bound = self
            .bind_worktree(
                &id.0,
                repo,
                branch,
                base_branch,
                project_id.clone(),
                &params.agent,
            )
            .await?;
        Ok(TargetResolution {
            launch_cwd: bound.path.clone(),
            repo: Some(bound.repository),
            branch: Some(bound.branch),
            worktree_path: Some(bound.path),
            project_id,
            is_linked_worktree: Some(true),
            warnings: bound.warnings,
            worktree_bound: true,
        })
    }

    /// Build the in-place (no-worktree) target: run the agent in the project's
    /// checkout as-is (Decision 3), or in `fallback_cwd` when no project resolved.
    ///
    /// A bare project never reaches here: [`Self::resolve_target`] refuses an
    /// in-place start on a bare repo (no working tree to run in) and steers the
    /// caller to `--branch`. So every project passed in has a real checkout.
    fn in_place_target(
        &self,
        project: Option<ProjectRecord>,
        detected: Option<DetectedProject>,
        fallback_cwd: PathBuf,
    ) -> TargetResolution {
        let (launch_cwd, project_id, is_linked_worktree) = match (project, detected) {
            // Detected from a path: launch in this work tree's root.
            (Some(record), Some(detected)) => (
                detected.checkout_path,
                Some(record.id()),
                Some(detected.is_linked_worktree),
            ),
            // Resolved by `--project`: the in-place checkout is its main checkout.
            (Some(record), None) => {
                let id = record.id();
                (record.repo_root, Some(id), Some(false))
            }
            // No project: a plain shell in the fallback cwd (today's behavior).
            (None, _) => (fallback_cwd, None, None),
        };
        TargetResolution {
            launch_cwd,
            repo: None,
            branch: None,
            worktree_path: None,
            project_id,
            is_linked_worktree,
            warnings: Vec::new(),
            worktree_bound: false,
        }
    }

    /// Resolve the project this session belongs to, doing the blocking git
    /// detection + store I/O on a blocking thread.
    ///
    /// Order (Decision 1): a `--project <id|label>` reference resolves against the
    /// store (bumping its `last_used_at`); otherwise detect at the explicit
    /// `--repo` path, else — for a local session — the CLI's own `--cwd`,
    /// auto-registering the result. `--project` and `--repo` are mutually exclusive
    /// (rejected earlier by [`validate_new_params`]). An **explicit** `--repo` that
    /// is not a git work tree is an error (no silent fallback to a different dir),
    /// whereas an **implicit** non-git `--cwd` is the normal plain-shell case.
    /// Returns the record and, when detection ran, the [`DetectedProject`] (the
    /// in-place path needs its `checkout_path`/`is_linked_worktree`).
    async fn resolve_project(
        &self,
        params: &SessionNewParams,
    ) -> Result<(Option<ProjectRecord>, Option<DetectedProject>), ProtocolError> {
        let Some(projects) = self.inner.projects.clone() else {
            // No project subsystem (store unconfigured, e.g. some unit tests): a
            // `--project` reference cannot be honored; otherwise there is simply no
            // project, and worktree binding via `--repo`/`--branch` still works.
            if params.project.is_some() {
                return Err(runtime_error(
                    "projects_not_configured",
                    "the daemon is not configured for projects (no metadata store)",
                ));
            }
            return Ok((None, None));
        };
        let reference = params.project.clone();
        let repo = params.repo.clone();
        let cwd = params.cwd.clone();
        tokio::task::spawn_blocking(move || -> Result<_, ProtocolError> {
            // 1. Reference resolves against the store; a session start bumps recency.
            if let Some(reference) = reference {
                let record = projects.resolve(&reference)?;
                let record = projects.touch(&record.git_common_dir)?.unwrap_or(record);
                return Ok((Some(record), None));
            }
            // 2. Explicit --repo: must be a git work tree, else error — never
            // silently launch somewhere else (no-silent-defaults).
            if let Some(repo) = repo {
                let detected =
                    detect_at(&repo)?.ok_or_else(|| crate::project::not_a_git_repo(&repo))?;
                let record = projects.register(&detected, false)?;
                return Ok((Some(record), Some(detected)));
            }
            // 3. Implicit --cwd (local): a non-git cwd is the normal plain shell.
            let Some(cwd) = cwd else {
                return Ok((None, None));
            };
            let Some(detected) = detect_at(&cwd)? else {
                return Ok((None, None));
            };
            let record = projects.register(&detected, false)?;
            Ok((Some(record), Some(detected)))
        })
        .await
        .map_err(|_| runtime_error("project_resolve_failed", "project resolution task panicked"))?
    }

    /// Bind (or reuse) a worktree for `(session, repo, branch)` on a blocking
    /// thread. Errors when worktree binding is not configured.
    async fn bind_worktree(
        &self,
        session_id: &str,
        repo: PathBuf,
        branch: String,
        base_branch: Option<String>,
        project_id: Option<String>,
        agent: &str,
    ) -> Result<crate::worktree::WorktreeBound, ProtocolError> {
        let Some(manager) = self.inner.worktree.clone() else {
            return Err(runtime_error(
                "worktree_not_configured",
                "the daemon is not configured for worktree binding",
            ));
        };
        let request = WorktreeRequest {
            session_id: session_id.to_owned(),
            repo,
            branch,
            base_branch,
            project_id,
            agent: agent.to_owned(),
        };
        tokio::task::spawn_blocking(move || manager.bind(&request))
            .await
            .map_err(|_| runtime_error("worktree_bind_failed", "worktree bind task panicked"))?
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
            agent_base,
            input_rules,
            snapshot,
            manifest_override,
            cwd,
            cols,
            rows,
            command,
            native_session_id,
            native_session_path,
            project_id,
            is_linked_worktree,
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
            agent_base,
            cwd,
            pid: pty.pid(),
            cols,
            rows,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            native_session_id,
            native_session_path,
            project_id,
            // Denormalized for display, resolved fresh at `session.list` time.
            project_label: None,
            is_linked_worktree,
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
                    input_rules,
                    snapshot,
                },
            );
        }

        self.emit(event::SESSION_CREATED, &info);
        self.spawn_detector(
            id.clone(),
            agent_base,
            manifest_override,
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

    /// Build the full env injected into a session's PTY.
    ///
    /// Always carries `POHUNEK_SESSION_ID` ([`ENV_SESSION_ID`]) and
    /// `POHUNEK_DAEMON_ID` ([`ENV_DAEMON_ID`]) for **every** agent kind —
    /// including a plain shell — so a `pohunek attach` launched inside the PTY can
    /// be recognized as a self-feeding loop and rejected (see
    /// [`SessionRegistry::attach`]); the daemon id scopes that decision to this
    /// instance regardless of which transport delivers the attach. For
    /// Codex/Claude it additionally carries the hook handshake from
    /// [`Self::hook_env`] (which already includes the session id, so it is not
    /// duplicated here).
    fn session_pty_env(&self, agent: AgentKind, session_id: &SessionId) -> Vec<(String, String)> {
        let mut env = self.hook_env(agent, session_id);
        if !env.iter().any(|(key, _)| key == ENV_SESSION_ID) {
            env.push((ENV_SESSION_ID.to_owned(), session_id.0.clone()));
        }
        env.push((
            ENV_DAEMON_ID.to_owned(),
            self.inner.daemon_instance_id.clone(),
        ));
        env
    }

    /// Inject text into a running session using the agent's input framing rules.
    pub async fn input(
        &self,
        params: SessionInputParams,
    ) -> Result<SessionInputResult, ProtocolError> {
        self.write_input_to_session(&params.session_id, &params.text)
            .await?;
        Ok(SessionInputResult { accepted: true })
    }

    async fn write_input_to_session(
        &self,
        session_id: &SessionId,
        text: &str,
    ) -> Result<(), ProtocolError> {
        let (pty, rules) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get(session_id)
                .ok_or_else(|| session_not_found(&session_id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(session_id));
            }
            (entry.pty.clone(), entry.input_rules)
        };

        let writes = build_input_writes(text, rules);
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

        Ok(())
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

            // The native reference KIND is frozen at launch (a profile's `ref_kind`,
            // or `id` for a base kind). The SessionStart hook bakes a base-kind
            // literal into the wire `agent` and carries no profile identity, so the
            // wire value is ignored for kind selection — the snapshot is authoritative.
            let Some(template) = entry.snapshot.resume else {
                debug!(
                    session_id = %params.session_id.0,
                    "native-id report for a non-resumable session; ignoring"
                );
                return not_recorded;
            };
            let ref_kind = template.ref_kind;
            let validated = match ref_kind {
                SessionRefKind::Id => SessionRef::id(&params.native_session_id),
                SessionRefKind::Path => match params.transcript_path.as_deref() {
                    Some(path) => SessionRef::path(path),
                    None => {
                        debug!(
                            session_id = %params.session_id.0,
                            "ignoring path-kind native-id report without transcript_path"
                        );
                        return not_recorded;
                    }
                },
            };
            let session_ref = match validated {
                Ok(session_ref) => session_ref,
                Err(err) => {
                    debug!(
                        session_id = %params.session_id.0,
                        error = %err,
                        "ignoring native-id report with an invalid native reference"
                    );
                    return not_recorded;
                }
            };
            // Store into the field chosen by kind, clearing the other so a session
            // resumes by exactly one mechanism (the persist literal copies both).
            match ref_kind {
                SessionRefKind::Id => {
                    entry.info.native_session_id = Some(session_ref.value().to_owned());
                    entry.info.native_session_path = None;
                }
                SessionRefKind::Path => {
                    entry.info.native_session_path = Some(session_ref.value().to_owned());
                    entry.info.native_session_id = None;
                }
            }
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
                entry.snapshot.resume?;
                // Resumable once the agent has reported a native reference — an
                // opaque id (claude/codex) or a transcript path (a path-resuming
                // host profile). No reference yet ⇒ no binding.
                if entry.info.native_session_id.is_none()
                    && entry.info.native_session_path.is_none()
                {
                    return None;
                }
                Some(ResumeBinding {
                    session_id: id.0.clone(),
                    agent: entry.info.agent.clone(),
                    agent_base: entry.info.agent_base,
                    cwd: entry.info.cwd.clone(),
                    cols: entry.info.cols,
                    rows: entry.info.rows,
                    native_session_id: entry.info.native_session_id.clone(),
                    native_session_path: entry.info.native_session_path.clone(),
                    // Capture the project context so resume restores it without
                    // re-detecting (F5): a restart reads these back verbatim.
                    project_id: entry.info.project_id.clone(),
                    is_linked_worktree: entry.info.is_linked_worktree,
                    // Structural relaunch snapshot (C.4): copied verbatim from the
                    // frozen entry snapshot on EVERY persist (creation, native-id
                    // capture, the hot resize path), so a resize re-persist can never
                    // overwrite the launch-time shape. `env` is intentionally absent —
                    // it is re-resolved by agent name at resume (no secrets in store).
                    program: entry.snapshot.program.clone(),
                    args: entry.snapshot.args.clone(),
                    input_rules: entry.input_rules.into(),
                    resume_mode: entry.snapshot.resume.map(|template| template.mode),
                    ref_kind: entry.snapshot.resume.map(|template| template.ref_kind),
                    resumable: entry.snapshot.resume.is_some(),
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
            let agent = binding.agent.clone();
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
                    if matches!(
                        err.code.as_str(),
                        "invalid_session_ref" | "not_resumable" | "agent_not_resumable"
                    ) {
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
        // The resume mechanics come from the frozen structural snapshot (C.4). An
        // explicit `(resume_mode, ref_kind)` pair drives the argv; a legacy binding
        // (pre-C2, no snapshot) falls back to the base kind's native template.
        if !binding.resumable && !binding.program.is_empty() {
            return Err(agent_not_resumable(&binding.agent));
        }
        let template = match (binding.resume_mode, binding.ref_kind) {
            (Some(mode), Some(ref_kind)) => ResumeTemplate { mode, ref_kind },
            _ => base_resume_template(binding.agent_base)
                .ok_or_else(|| agent_not_resumable(&binding.agent))?,
        };
        // Build the native reference from the field the frozen `ref_kind` names, so
        // a `path`-kind profile inherits the absolute-path guard and an `id`-kind the
        // leading-dash guard (the documented asymmetry).
        let session_ref = match template.ref_kind {
            SessionRefKind::Id => match &binding.native_session_id {
                Some(value) => SessionRef::id(value)?,
                None => {
                    return Err(runtime_error(
                        "not_resumable",
                        format!(
                            "resume binding for {} is id-kind but has no native id",
                            binding.session_id
                        ),
                    ));
                }
            },
            SessionRefKind::Path => match &binding.native_session_path {
                Some(value) => SessionRef::path(value)?,
                None => {
                    return Err(runtime_error(
                        "not_resumable",
                        format!(
                            "resume binding for {} is path-kind but has no native path",
                            binding.session_id
                        ),
                    ));
                }
            },
        };

        let id = SessionId(binding.session_id.clone());
        self.bump_next_id_past(&id);

        // A legacy binding carries no snapshot program; fall back to the base kind's
        // default so it still relaunches. `program`/`input_rules` are frozen
        // structural fields — never re-resolved from the profile.
        let has_snapshot = !binding.program.is_empty();
        let program = if has_snapshot {
            binding.program.clone()
        } else {
            default_program(binding.agent_base)
        };
        let input_rules = if has_snapshot {
            binding.input_rules.to_input_rules()
        } else {
            input_rules_for_agent(binding.agent_base, &self.inner.config)
        };

        // Re-resolve the profile by NAME to recover its (possibly-secret) env + its
        // detection-manifest override — neither is ever persisted (C.4 no-secrets).
        // A deleted/renamed profile resumes from the frozen structural snapshot with
        // no profile env and a warning, never a failure.
        let (profile_env, manifest_override) = match self
            .inner
            .profiles
            .resolve_agent(&binding.agent)
        {
            Ok(resolved) => resolved.profile.map_or((Vec::new(), None), |profile| {
                (profile.env, profile.manifest)
            }),
            Err(err) => {
                warn!(
                    session_id = %binding.session_id,
                    agent = %binding.agent,
                    error = %err,
                    "agent profile no longer resolves at resume; relaunching from the structural snapshot without profile env"
                );
                (Vec::new(), None)
            }
        };
        // Profile env first, daemon handshake env appended last (POHUNEK_* wins).
        let mut env_extra = profile_env;
        env_extra.extend(self.session_pty_env(binding.agent_base, &id));
        let opts = LaunchOpts {
            cwd: binding.cwd.clone(),
            cols: binding.cols,
            rows: binding.rows,
            env_extra,
        };
        let command = resume_pty_command_from_template(
            &program,
            binding.args.clone(),
            template,
            &session_ref,
            &opts,
        )?;
        // Re-freeze the structural snapshot for the resumed entry so a later resize
        // re-persist keeps the same launch-time shape.
        let snapshot = ResumeSnapshot {
            program,
            args: binding.args.clone(),
            resume: Some(template),
        };
        // A resumed session relaunches in its recorded cwd, which already is the
        // worktree path for worktree sessions (the worktree persists on disk
        // across a daemon restart). With the unified store the session's worktree
        // metadata (repo/branch/worktree_path) is restored too, so inspect/list
        // show it again after a restart.
        let (repo, branch, worktree_path) = self.restore_worktree_metadata(&binding.session_id);
        // The project context was captured on the binding when it was persisted
        // (F5), so restore it directly — no git re-detection on the cwd at startup,
        // and a detection failure can no longer silently drop the metadata. An
        // older binding (pre-F5) carries `None`, leaving the resumed session
        // without project context until its next persist.
        let project_id = binding.project_id.clone();
        let is_linked_worktree = binding.is_linked_worktree;
        self.register_pty_session(PtySessionSpec {
            id,
            agent: binding.agent,
            agent_base: binding.agent_base,
            input_rules,
            snapshot,
            manifest_override,
            cwd: binding.cwd,
            cols: binding.cols,
            rows: binding.rows,
            command,
            native_session_id: binding.native_session_id,
            native_session_path: binding.native_session_path,
            project_id,
            is_linked_worktree,
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

    /// List all known sessions, with each session's `project_label` enriched from
    /// the project store (so the switcher and `session list` show the project by
    /// name, and `--filter project=<label>` resolves). Enrichment is best-effort:
    /// a missing store or read error simply leaves labels unset.
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
        self.enrich_project_labels(&mut sessions).await;
        sessions
    }

    /// Set each session's `project_label` from the current store (resolved fresh,
    /// so a rename shows immediately). The blocking store read runs on a blocking
    /// thread; any failure leaves labels unset (the project id is still present).
    async fn enrich_project_labels(&self, sessions: &mut [SessionInfo]) {
        if !sessions.iter().any(|session| session.project_id.is_some()) {
            return;
        }
        let Some(projects) = self.inner.projects.clone() else {
            return;
        };
        let labels = match tokio::task::spawn_blocking(move || projects.label_map()).await {
            Ok(Ok(labels)) => labels,
            Ok(Err(err)) => {
                warn!(error = %err, "failed to load project labels for session list");
                return;
            }
            Err(_) => return,
        };
        for session in sessions.iter_mut() {
            if let Some(id) = &session.project_id {
                session.project_label = labels.get(id).cloned();
            }
        }
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
    ///
    /// Rejects a *self-feeding* attach: when the client reports (via the
    /// `POHUNEK_SESSION_ID` / `POHUNEK_DAEMON_ID` it inherited from a PTY) that it
    /// is running inside this very session of this very daemon, its stdout is the
    /// session's own PTY slave, so streaming the PTY's output to it would be
    /// written straight back into the PTY as input and re-read as output — an
    /// infinite, log-flooding loop. Both the session id **and** the daemon
    /// instance id must match: a colliding id on a different daemon, or a stale
    /// value from a previous daemon process, has a different instance id and is
    /// correctly allowed. The existence/running check runs first so a stale origin
    /// pointing at a gone/stopped session yields the truthful `session_not_found`/
    /// `session_not_running` rather than a misleading self-feedback error.
    pub async fn attach(
        &self,
        params: &SessionAttachParams,
    ) -> Result<protocol::SessionAttachResult, ProtocolError> {
        let id = &params.session_id;
        self.prune_expired_pending_attaches().await;
        {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(id));
            }
        }

        // The session exists and is running; only now is a self-feed possible.
        if params.origin_session_id.as_ref() == Some(id)
            && params.origin_daemon_id.as_deref() == Some(self.daemon_instance_id())
        {
            return Err(attach_self_feedback(id));
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
            let has_native =
                entry.info.native_session_id.is_some() || entry.info.native_session_path.is_some();
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

    // The detector spawn carries the session id, base kind, manifest override, the
    // output stream, the initial size, and its cancel/resize channels — all distinct
    // runtime inputs with no natural grouping struct.
    #[allow(clippy::too_many_arguments)]
    fn spawn_detector(
        &self,
        id: SessionId,
        agent: AgentKind,
        manifest_override: Option<Manifest>,
        mut output_rx: broadcast::Receiver<Vec<u8>>,
        size: (u16, u16),
        cancel: CancellationToken,
        mut resize_rx: watch::Receiver<(u16, u16)>,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            // A host profile may override the base kind's detection manifest; absent
            // an override this is exactly `for_agent(agent)` (C.3).
            let detector_config = DetectorConfig::for_profile(agent, manifest_override);
            let mut tick = tokio::time::interval(detector_config.detection.recheck_after);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            let (rows, cols) = size;
            let mut detector = Detector::new(rows, cols, Instant::now(), detector_config);
            let mut lag_warn =
                LagWarnThrottle::new(registry.inner.config.detector_lag_warn_interval);

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        for transition in detector.tick(Instant::now()) {
                            registry.record_activity(&id, transition).await;
                        }
                        // Flush a folded lag batch whose window has elapsed, so a
                        // session that stopped lagging still reports its summary.
                        if let Some(warn_kind) = lag_warn.poll(Instant::now()) {
                            log_lag_warn(&id, warn_kind);
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
                                // Always resync; only the logging is rate-limited
                                // so a runaway session cannot flood the log.
                                if let Some(warn_kind) = lag_warn.observe(Instant::now(), skipped) {
                                    log_lag_warn(&id, warn_kind);
                                }
                                detector.resync_after_lag();
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }

            // The loop exited (cancel / resize-closed / output-closed): flush any
            // lags folded into the final, not-yet-elapsed window so a session torn
            // down mid-storm still reports its trailing batch instead of dropping it.
            if let Some(warn_kind) = lag_warn.flush() {
                log_lag_warn(&id, warn_kind);
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

    async fn agent_state_hook_snapshot(&self, id: &SessionId) -> Option<AgentStateHookSnapshot> {
        let sessions = self.inner.sessions.lock().await;
        let entry = sessions.get(id)?;
        if entry.stopping || is_terminal(entry.info.state) {
            return None;
        }
        Some(AgentStateHookSnapshot {
            session_id: entry.info.id.clone(),
            project_id: entry.info.project_id.clone(),
            cwd: entry.info.cwd.clone(),
            agent: entry.info.agent.clone(),
            activity: entry.info.activity?,
        })
    }

    async fn agent_state_hook_snapshots(&self) -> Vec<AgentStateHookSnapshot> {
        let sessions = self.inner.sessions.lock().await;
        sessions
            .values()
            .filter(|entry| !entry.stopping && !is_terminal(entry.info.state))
            .filter_map(|entry| {
                Some(AgentStateHookSnapshot {
                    session_id: entry.info.id.clone(),
                    project_id: entry.info.project_id.clone(),
                    cwd: entry.info.cwd.clone(),
                    agent: entry.info.agent.clone(),
                    activity: entry.info.activity?,
                })
            })
            .collect()
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

            let stop_reason = if stopped {
                entry.info.state = SessionState::Stopped;
                "stopped"
            } else if exit.success {
                entry.info.state = SessionState::Done;
                "done"
            } else {
                entry.info.state = SessionState::Failed;
                "failed"
            };
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
            (event, entry.info.clone(), stop_reason)
        };

        self.cancel_session_attaches(id).await;
        self.remove_pending_attaches_for_session(id).await;
        self.spawn_session_hook(SessionHookRequest {
            event: HookEvent::SessionStop,
            cwd: updated.1.cwd.clone(),
            session_id: updated.1.id.0.clone(),
            project_id: updated.1.project_id.clone(),
            agent: updated.1.agent.clone(),
            stop_reason: Some(updated.2),
            activity: None,
        });
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
    // `--project` and `--repo` are two ways to name the same target repository;
    // accepting both would let the worktree be cut from one while the session is
    // stamped with the other's project id (an incoherent binding). The CLI rejects
    // this at parse time too; this guards non-CLI / remote callers.
    if params.project.is_some() && params.repo.is_some() {
        return Err(ProtocolError::bad_request(
            "session.new: --project and --repo are mutually exclusive (both name the target repository)",
        ));
    }
    Ok(())
}

fn build_launch_command(
    resolved: &ResolvedAgent,
    shell_command: &ShellCommand,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    env_extra: Vec<(String, String)>,
) -> Result<PtyCommand, ProtocolError> {
    // Shell carries no agent-hook handshake, but it does carry the universal
    // `POHUNEK_SESSION_ID` marker (see `session_pty_env`) so a `pohunek attach`
    // launched inside it is still caught as a self-feeding loop.
    let opts = LaunchOpts {
        cwd,
        cols,
        rows,
        env_extra,
    };
    match &resolved.profile {
        // A host profile overrides the launch program/args; build via the shared
        // PATH-resolving primitive (the same one the base adapters use).
        Some(profile) => build_pty_command(&profile.program, profile.args.clone(), &opts),
        // A bare base kind launches exactly as the compiled adapter (zero change).
        None => launch_adapter_for(resolved.base, shell_command).launch(&opts),
    }
}

fn input_rules_for_agent(agent: AgentKind, config: &SessionRegistryConfig) -> InputRules {
    let mut rules = adapter_for(agent).input_rules();
    if agent == AgentKind::Claude {
        rules.submit_delay = config.claude_submit_delay;
    }
    rules
}

fn spawn_agent_state_hook_dispatcher(
    registry: SessionRegistry,
    mut events: broadcast::Receiver<Event>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_fired: HashMap<SessionId, AgentActivity> = HashMap::new();
        let mut pending: HashMap<SessionId, (AgentActivity, tokio::time::Instant)> = HashMap::new();
        let mut in_flight: HashSet<SessionId> = HashSet::new();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        let mut tick = tokio::time::interval(AGENT_STATE_HOOK_DEBOUNCE);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    drain_buffered_agent_state_events(
                        &registry,
                        &mut events,
                        &mut last_fired,
                        &mut pending,
                    ).await;
                    flush_pending_agent_state_hooks(
                        &registry,
                        &mut last_fired,
                        &mut pending,
                        &mut in_flight,
                        &done_tx,
                        true,
                    ).await;
                    while !in_flight.is_empty() {
                        let Some(session_id) = done_rx.recv().await else {
                            break;
                        };
                        in_flight.remove(&session_id);
                        flush_pending_agent_state_hooks(
                            &registry,
                            &mut last_fired,
                            &mut pending,
                            &mut in_flight,
                            &done_tx,
                            true,
                        ).await;
                    }
                    break;
                }
                Some(session_id) = done_rx.recv() => {
                    in_flight.remove(&session_id);
                    flush_pending_agent_state_hooks(
                        &registry,
                        &mut last_fired,
                        &mut pending,
                        &mut in_flight,
                        &done_tx,
                        false,
                    ).await;
                }
                _ = tick.tick() => {
                    flush_pending_agent_state_hooks(
                        &registry,
                        &mut last_fired,
                        &mut pending,
                        &mut in_flight,
                        &done_tx,
                        false,
                    ).await;
                }
                received = events.recv() => match received {
                    Ok(event) => {
                        if let Some((session_id, activity)) = parse_agent_state_event(&event) {
                            queue_agent_state_hook(&mut last_fired, &mut pending, session_id, activity);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        warn!(
                            dropped,
                            "agent-state hook dispatcher lagged; re-reading current activities"
                        );
                        for snapshot in registry.agent_state_hook_snapshots().await {
                            queue_agent_state_hook(
                                &mut last_fired,
                                &mut pending,
                                snapshot.session_id,
                                snapshot.activity,
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    })
}

async fn drain_buffered_agent_state_events(
    registry: &SessionRegistry,
    events: &mut broadcast::Receiver<Event>,
    last_fired: &mut HashMap<SessionId, AgentActivity>,
    pending: &mut HashMap<SessionId, (AgentActivity, tokio::time::Instant)>,
) {
    loop {
        match events.try_recv() {
            Ok(event) => {
                if let Some((session_id, activity)) = parse_agent_state_event(&event) {
                    queue_agent_state_hook(last_fired, pending, session_id, activity);
                }
            }
            Err(broadcast::error::TryRecvError::Lagged(dropped)) => {
                warn!(
                    dropped,
                    "agent-state hook dispatcher lagged during shutdown; re-reading current activities"
                );
                for snapshot in registry.agent_state_hook_snapshots().await {
                    queue_agent_state_hook(
                        last_fired,
                        pending,
                        snapshot.session_id,
                        snapshot.activity,
                    );
                }
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break;
            }
        }
    }
}

fn parse_agent_state_event(event: &Event) -> Option<(SessionId, AgentActivity)> {
    if event.event != event::AGENT_STATE {
        return None;
    }
    let payload = event.payload.as_object()?;
    let session_id = payload.get("session_id")?.as_str()?;
    let activity_value = payload.get("activity")?;
    let activity = match parse_agent_activity(activity_value) {
        Ok(activity) => activity,
        Err(err) => {
            warn!(
                session_id,
                activity = ?activity_value,
                error = %err,
                "failed to parse agent-state activity; hook not fired"
            );
            return None;
        }
    };
    Some((SessionId(session_id.to_owned()), activity))
}

fn parse_agent_activity(value: &Value) -> Result<AgentActivity, serde_json::Error> {
    serde_json::from_value(value.clone())
}

fn queue_agent_state_hook(
    last_fired: &mut HashMap<SessionId, AgentActivity>,
    pending: &mut HashMap<SessionId, (AgentActivity, tokio::time::Instant)>,
    session_id: SessionId,
    activity: AgentActivity,
) {
    if last_fired.get(&session_id) == Some(&activity) {
        pending.remove(&session_id);
        return;
    }
    pending.insert(
        session_id,
        (
            activity,
            tokio::time::Instant::now() + AGENT_STATE_HOOK_DEBOUNCE,
        ),
    );
}

async fn flush_pending_agent_state_hooks(
    registry: &SessionRegistry,
    last_fired: &mut HashMap<SessionId, AgentActivity>,
    pending: &mut HashMap<SessionId, (AgentActivity, tokio::time::Instant)>,
    in_flight: &mut HashSet<SessionId>,
    done_tx: &mpsc::UnboundedSender<SessionId>,
    flush_all: bool,
) {
    let now = tokio::time::Instant::now();
    let due: Vec<SessionId> = pending
        .iter()
        .filter_map(|(session_id, (_, deadline))| {
            if flush_all || *deadline <= now {
                Some(session_id.clone())
            } else {
                None
            }
        })
        .collect();

    for session_id in due {
        if in_flight.contains(&session_id) {
            continue;
        }
        let Some((pending_activity, _)) = pending.remove(&session_id) else {
            continue;
        };
        let Some(snapshot) = registry.agent_state_hook_snapshot(&session_id).await else {
            last_fired.remove(&session_id);
            continue;
        };
        if snapshot.activity != pending_activity {
            queue_agent_state_hook(last_fired, pending, session_id, snapshot.activity);
            continue;
        }
        if last_fired.get(&snapshot.session_id) == Some(&snapshot.activity) {
            continue;
        }
        fire_agent_state_hook(registry, snapshot.clone(), in_flight, done_tx);
        last_fired.insert(snapshot.session_id, snapshot.activity);
    }
}

fn fire_agent_state_hook(
    registry: &SessionRegistry,
    snapshot: AgentStateHookSnapshot,
    in_flight: &mut HashSet<SessionId>,
    done_tx: &mpsc::UnboundedSender<SessionId>,
) {
    let session_id = snapshot.session_id.clone();
    in_flight.insert(session_id.clone());
    let handle = registry.spawn_session_hook_task(SessionHookRequest {
        event: HookEvent::AgentState,
        cwd: snapshot.cwd,
        session_id: snapshot.session_id.0,
        project_id: snapshot.project_id,
        agent: snapshot.agent,
        stop_reason: None,
        activity: Some(agent_activity_env(snapshot.activity)),
    });
    let done_tx = done_tx.clone();
    tokio::spawn(async move {
        let _ = handle.await;
        let _ = done_tx.send(session_id);
    });
}

fn agent_activity_env(activity: AgentActivity) -> &'static str {
    match activity {
        AgentActivity::Working => "working",
        AgentActivity::Blocked => "blocked",
        AgentActivity::Idle => "idle",
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
    state.is_terminal()
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

/// The attach would feed the session's PTY output back into its own input.
///
/// Raised when the attaching client reports (via `POHUNEK_SESSION_ID` +
/// `POHUNEK_DAEMON_ID`) that it is running inside the very session of this very
/// daemon instance it is attaching to. Stable code: `attach_self_feedback`.
fn attach_self_feedback(id: &SessionId) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Daemon,
        "attach_self_feedback",
        format!(
            "refusing to attach to session {} from inside its own terminal: \
             that would loop the session's output back into its own input",
            id.0
        ),
        Some(
            "run the attach from a different terminal (one not already inside this session)"
                .to_owned(),
        ),
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

/// Generate an opaque id distinguishing this daemon process instance.
///
/// Combines the pid, the wall clock's distance from the epoch, and a process-wide
/// monotonic counter. Two live instances always differ (distinct live pids); a
/// restart on a recycled pid differs as long as the wall clock advances between
/// the two starts; the counter disambiguates registries built within one process
/// at the same instant (e.g. tests). The clock distance is taken in *either*
/// direction so the id never collapses to a fixed value when the clock is set
/// before 1970 (an RTC-less boot before NTP). Used to scope the
/// self-feeding-attach guard to this instance's own PTYs and to keep a stale
/// `POHUNEK_DAEMON_ID` from a previous daemon from matching (see
/// [`SessionAttachParams::origin_daemon_id`]); a residual collision only
/// false-rejects an attach (never lets a loop through) and the lag-warn throttle
/// still bounds any such loop's log output. Not a secret.
fn generate_daemon_instance_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_nanos(),
        // Clock is before the epoch: use how far before, so the value still varies
        // with the clock instead of pinning to a fixed 0.
        Err(before_epoch) => before_epoch.duration().as_nanos(),
    };
    format!("d-{}-{nanos}-{seq}", std::process::id())
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
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use protocol::{
        AgentActivity, AgentKind, Event, ProjectSource, SessionAttachParams, SessionId,
        SessionNewParams, SessionReportNativeIdParams, SessionState,
    };

    use crate::agent::{InputRules, ResumeMode, SessionRefKind};
    use crate::detect::ActivityTransition;
    use crate::integration::{
        ENV_DAEMON_ID, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
    };

    use super::{LagWarn, LagWarnThrottle, SessionRegistry, SessionRegistryConfig, ShellCommand};
    use std::time::Instant;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn params() -> SessionNewParams {
        SessionNewParams {
            agent: "shell".to_owned(),
            cwd: Some(PathBuf::from("/tmp")),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
        }
    }

    /// A plain attach (no self-feed origin) for the given session id.
    fn attach_params(id: &SessionId) -> SessionAttachParams {
        SessionAttachParams {
            session_id: id.clone(),
            origin_session_id: None,
            origin_daemon_id: None,
        }
    }

    fn temp_store_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pohunek-session-{tag}-{}-{nanos}-{n}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("metadata.jsonl")
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = temp_store_path(tag)
            .parent()
            .expect("store parent")
            .join("dir");
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_host_hook(config_dir: &std::path::Path, event: &str, body: &str) {
        let hooks = config_dir.join("hooks");
        fs::create_dir_all(&hooks).expect("create hooks dir");
        fs::write(hooks.join(event), body).expect("write hook");
    }

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, body).expect("write executable");
        let mut perms = fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o700);
        fs::set_permissions(path, perms).expect("chmod executable");
    }

    async fn wait_for_file_contains(path: &std::path::Path, needle: &str) -> String {
        for _ in 0..500 {
            if let Ok(contents) = fs::read_to_string(path) {
                if contents.contains(needle) {
                    return contents;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "timed out waiting for {} to contain {needle:?}",
            path.display()
        );
    }

    async fn wait_for_line_count(path: &std::path::Path, expected: usize) -> String {
        for _ in 0..500 {
            if let Ok(contents) = fs::read_to_string(path) {
                if contents.lines().count() >= expected {
                    return contents;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "timed out waiting for {} to contain at least {expected} lines",
            path.display()
        );
    }

    fn transition(activity: AgentActivity) -> ActivityTransition {
        ActivityTransition {
            activity,
            source: protocol::StateSource::Process,
        }
    }

    fn parse_env_dump(text: &str) -> std::collections::HashMap<String, String> {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    fn pohunek_env_keys(env: &std::collections::HashMap<String, String>) -> Vec<String> {
        let mut keys: Vec<String> = env
            .keys()
            .filter(|key| key.starts_with("POHUNEK_"))
            .cloned()
            .collect();
        keys.sort();
        keys
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

    /// Initialize a throwaway **bare** repo (no working tree) carrying a commit and
    /// HEAD, for the bare-project paths. A `--bare` clone of a normal repo gives a
    /// bare repo that still has a `main` branch, so `git worktree add` off it works.
    fn init_bare_git_repo(tag: &str) -> PathBuf {
        let source = init_git_repo(&format!("{tag}-src"));
        let bare = temp_store_path(tag)
            .parent()
            .expect("store parent")
            .join("bare.git");
        let clone = std::process::Command::new("git")
            .args(["clone", "--bare", "-q"])
            .arg(&source)
            .arg(&bare)
            .output()
            .expect("git clone --bare");
        assert!(
            clone.status.success(),
            "git clone --bare failed: {}",
            String::from_utf8_lossy(&clone.stderr)
        );
        bare
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
    async fn failed_initial_input_rollback_frees_the_bound_worktree() {
        // A worktree-bound session whose `--input` injection fails must roll the
        // worktree back, not just the PTY: `stop()` alone leaves the checkout in
        // place, blocking the next `session.new` on the branch with
        // `worktree_branch_in_use`. Drive the exact rollback the failed-input
        // branch of `create` performs and assert the branch is freed.
        let repo = init_git_repo("input-rollback");
        let store = temp_store_path("input-rollback");
        let worktree_root = store.parent().expect("store parent").join("worktrees");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            store_path: Some(store),
            worktree_root: Some(worktree_root.clone()),
            ..SessionRegistryConfig::default()
        });

        // A real worktree-bound session (launch succeeds, branch checked out).
        let info = registry
            .create(SessionNewParams {
                cwd: None,
                repo: Some(repo.clone()),
                branch: Some("feat/x".to_owned()),
                ..params()
            })
            .await
            .expect("worktree-bound session is created");
        assert!(
            info.worktree_path.is_some(),
            "session must be worktree-bound for this test: {info:?}"
        );

        registry.rollback_failed_initial_input(&info.id, true).await;

        // The worktree bound for this session must be gone so its branch is free.
        let leftover: Vec<_> = std::fs::read_dir(&worktree_root)
            .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(
            leftover.is_empty(),
            "rollback must leave no orphan worktree under {}: {leftover:?}",
            worktree_root.display()
        );

        // git no longer holds feat/x in any worktree, so a fresh bind succeeds.
        let listing = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("git worktree list");
        let listing = String::from_utf8_lossy(&listing.stdout);
        assert!(
            !listing.contains("feat/x"),
            "branch checkout must be pruned so a fresh bind succeeds: {listing}"
        );
    }

    /// A registry with persistence + worktree binding configured, using the
    /// default shell so a launch actually succeeds (for the project-wiring tests).
    fn project_registry(tag: &str) -> (SessionRegistry, PathBuf) {
        let store = temp_store_path(tag);
        let worktree_root = store.parent().expect("store parent").join("worktrees");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            store_path: Some(store),
            worktree_root: Some(worktree_root),
            ..SessionRegistryConfig::default()
        });
        let repo = init_git_repo(tag);
        (registry, repo)
    }

    #[tokio::test]
    async fn session_new_auto_registers_project_from_cwd_and_stamps_ids() {
        // The first observable change (M3): starting a session inside a git work
        // tree with no flags runs in-place and silently records an Auto project,
        // stamping the session's project_id / is_linked_worktree.
        let (registry, repo) = project_registry("auto-register");
        let info = registry
            .create(SessionNewParams {
                cwd: Some(repo.clone()),
                ..params()
            })
            .await
            .expect("session is created in the repo");

        let canonical_repo = std::fs::canonicalize(&repo).expect("canonical repo");
        assert_eq!(info.cwd, canonical_repo, "in-place runs in the checkout");
        assert_eq!(info.worktree_path, None, "in-place binds no worktree");
        assert_eq!(info.is_linked_worktree, Some(false), "the main checkout");
        let project_id = info.project_id.clone().expect("a project was stamped");

        let projects = registry
            .projects()
            .expect("projects configured")
            .store()
            .load_projects()
            .expect("load projects");
        assert_eq!(projects.len(), 1, "exactly one project auto-registered");
        assert_eq!(projects[0].source, ProjectSource::Auto);
        assert_eq!(
            projects[0].id(),
            project_id,
            "session id matches the record"
        );
        assert_eq!(
            projects[0].git_common_dir,
            std::fs::canonicalize(repo.join(".git")).expect("canonical .git")
        );
    }

    #[tokio::test]
    async fn in_place_session_on_a_bare_project_is_refused() {
        // A bare repo has no working tree; an in-place agent would land in the bare
        // git dir. The default (no --branch) start must be refused with a message
        // steering the operator to --branch, not silently launched in the git dir.
        let (registry, _repo) = project_registry("bare-inplace");
        let bare = init_bare_git_repo("bare-inplace");

        let err = registry
            .create(SessionNewParams {
                cwd: Some(bare.clone()),
                ..params()
            })
            .await
            .expect_err("in-place on a bare repo must be refused");
        assert!(
            err.msg.contains("bare repository") && err.msg.contains("--branch"),
            "error must explain the bare repo and steer to --branch: {err:?}"
        );
        // Nothing was launched.
        assert!(
            registry.list().await.is_empty(),
            "no session is created for a refused in-place bare start"
        );
    }

    #[tokio::test]
    async fn worktree_session_on_a_bare_project_is_allowed() {
        // The steer in `in_place_session_on_a_bare_project_is_refused` is only valid
        // if --branch actually works on a bare repo: a worktree is added off it.
        let (registry, _repo) = project_registry("bare-worktree");
        let bare = init_bare_git_repo("bare-worktree");

        let info = registry
            .create(SessionNewParams {
                cwd: Some(bare.clone()),
                branch: Some("feat/x".to_owned()),
                base_branch: Some("main".to_owned()),
                ..params()
            })
            .await
            .expect("a worktree session is allowed on a bare repo");
        assert!(
            info.worktree_path.is_some(),
            "a worktree was bound off the bare repo: {info:?}"
        );
        assert_eq!(info.branch.as_deref(), Some("feat/x"));
    }

    #[tokio::test]
    async fn session_new_in_a_non_git_cwd_records_no_project() {
        // A plain shell in a non-git directory: no project, no stamping, today's
        // behavior unchanged.
        let (registry, _repo) = project_registry("non-git");
        let non_git = std::env::temp_dir().join(format!(
            "pohunek-nongit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&non_git).expect("create non-git dir");

        let info = registry
            .create(SessionNewParams {
                cwd: Some(non_git.clone()),
                ..params()
            })
            .await
            .expect("plain shell session is created");

        assert_eq!(info.project_id, None, "no git ⇒ no project");
        assert_eq!(info.is_linked_worktree, None);
        assert_eq!(info.worktree_path, None);
        assert!(
            registry
                .projects()
                .expect("projects configured")
                .store()
                .load_projects()
                .expect("load")
                .is_empty(),
            "a non-git directory must register nothing"
        );
    }

    #[tokio::test]
    async fn session_new_with_project_ref_binds_the_main_checkout_in_place() {
        // Resolve a project by its id reference (the only remote-capable option):
        // an in-place session launches in the project's main checkout.
        let (registry, repo) = project_registry("by-ref");
        // Auto-register by starting once in the repo, then reference it by id.
        let first = registry
            .create(SessionNewParams {
                cwd: Some(repo.clone()),
                ..params()
            })
            .await
            .expect("first session auto-registers the project");
        let project_id = first.project_id.clone().expect("project stamped");

        let info = registry
            .create(SessionNewParams {
                cwd: None,
                project: Some(project_id.clone()),
                ..params()
            })
            .await
            .expect("session created from a --project reference");

        assert_eq!(info.project_id.as_deref(), Some(project_id.as_str()));
        assert_eq!(info.worktree_path, None, "no --branch ⇒ in-place");
        assert_eq!(info.is_linked_worktree, Some(false));
        assert_eq!(
            info.cwd,
            std::fs::canonicalize(&repo).expect("canonical repo"),
            "in-place runs in the project's main checkout"
        );
    }

    #[tokio::test]
    async fn session_new_with_unknown_project_ref_is_rejected() {
        let (registry, _repo) = project_registry("unknown-ref");
        let err = registry
            .create(SessionNewParams {
                cwd: None,
                project: Some("does-not-exist".to_owned()),
                ..params()
            })
            .await
            .expect_err("an unknown project reference must error");
        assert_eq!(err.code, "project_not_found", "got: {err:?}");
    }

    #[tokio::test]
    async fn session_new_branch_with_detected_project_binds_worktree_carrying_project_id() {
        // `--branch` in a detected project builds a worktree-per-session off the
        // project's repo; the worktree's binding carries the project id so prune /
        // `project show` can find pohunek's own worktrees later (M5).
        let (registry, repo) = project_registry("wt-project");
        let info = registry
            .create(SessionNewParams {
                cwd: Some(repo.clone()),
                branch: Some("feat/x".to_owned()),
                ..params()
            })
            .await
            .expect("worktree session created from the detected project");

        assert!(info.worktree_path.is_some(), "--branch binds a worktree");
        assert_eq!(info.is_linked_worktree, Some(true));
        let project_id = info.project_id.clone().expect("project stamped on session");

        let binding = registry
            .projects()
            .expect("projects configured")
            .store()
            .load_worktrees()
            .expect("load worktrees")
            .into_iter()
            .find(|b| b.session_id == info.id.0)
            .expect("this session has a worktree binding");
        assert_eq!(
            binding.project_id.as_deref(),
            Some(project_id.as_str()),
            "the worktree binding must carry the project id"
        );
    }

    #[tokio::test]
    async fn session_new_with_project_ref_bumps_last_used_at() {
        // The data model defines last_used_at as bumped on each session start; the
        // --project reference path must do that too, not only auto-detection.
        let (registry, repo) = project_registry("touch");
        let first = registry
            .create(SessionNewParams {
                cwd: Some(repo.clone()),
                ..params()
            })
            .await
            .expect("first session auto-registers");
        let project_id = first.project_id.clone().expect("project stamped");
        let projects = registry.projects().expect("projects");
        let store = projects.store();

        // Backdate last_used_at so the bump is unambiguously observable.
        let mut record = store
            .load_projects()
            .expect("load")
            .into_iter()
            .next()
            .expect("one project");
        record.last_used_at = "2000-01-01T00:00:00Z".to_owned();
        store.record_project(&record).expect("backdate");

        registry
            .create(SessionNewParams {
                cwd: None,
                project: Some(project_id),
                ..params()
            })
            .await
            .expect("session created by --project reference");

        let after = store
            .load_projects()
            .expect("reload")
            .into_iter()
            .next()
            .expect("one project");
        assert_ne!(
            after.last_used_at, "2000-01-01T00:00:00Z",
            "a --project reference must bump last_used_at"
        );
    }

    #[tokio::test]
    async fn session_new_with_explicit_non_git_repo_errors() {
        // An explicitly named --repo that is not a git work tree must error, not
        // silently launch a plain shell somewhere else (no silent defaults).
        let (registry, _repo) = project_registry("explicit-nonrepo");
        let nonrepo = std::env::temp_dir().join(format!(
            "pohunek-nonrepo-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&nonrepo).expect("create non-git dir");

        let err = registry
            .create(SessionNewParams {
                cwd: None,
                repo: Some(nonrepo),
                ..params()
            })
            .await
            .expect_err("an explicit non-git --repo must error");
        assert_eq!(err.code, "not_a_git_repo", "got: {err:?}");
    }

    #[tokio::test]
    async fn session_new_rejects_project_and_repo_together() {
        // --project and --repo both name the target repo; accepting both would
        // persist an incoherent binding, so the daemon rejects the combination.
        let (registry, repo) = project_registry("mutual-exclusion");
        let err = registry
            .create(SessionNewParams {
                cwd: None,
                project: Some("anything".to_owned()),
                repo: Some(repo),
                ..params()
            })
            .await
            .expect_err("--project and --repo together must be rejected");
        assert!(err.msg.contains("mutually exclusive"), "got: {err:?}");
    }

    #[tokio::test]
    async fn remove_project_with_prune_removes_owned_worktrees_and_forgets_the_record() {
        let (registry, repo) = project_registry("prune");
        // A worktree session: its binding carries the project id.
        let info = registry
            .create(SessionNewParams {
                cwd: Some(repo.clone()),
                branch: Some("feat/x".to_owned()),
                ..params()
            })
            .await
            .expect("worktree session created");
        let worktree = info.worktree_path.clone().expect("worktree path");
        let project_id = info.project_id.clone().expect("project stamped");
        assert!(worktree.exists());
        // Stop the session first; its worktree binding is intentionally kept.
        registry.stop(&info.id).await.expect("stop session");

        let result = registry
            .remove_project(&project_id, true)
            .await
            .expect("remove with prune");
        assert!(result.removed, "the project record was removed");
        assert_eq!(result.pruned_worktrees, 1, "the owned worktree was pruned");
        assert!(!worktree.exists(), "pruned worktree directory is gone");
        assert!(
            registry
                .projects()
                .expect("projects")
                .store()
                .load_projects()
                .expect("load")
                .is_empty(),
            "the project record is forgotten"
        );
    }

    #[tokio::test]
    async fn remove_project_prune_skips_a_worktree_with_a_live_session() {
        // A worktree a RUNNING session is using must not be pruned out from under
        // it; it is skipped and reported. Because a worktree was skipped, the
        // record is KEPT (removed = false) so its surviving binding keeps pointing
        // at a real project (Option (b)); a later `rm` forgets it once idle.
        let (registry, repo) = project_registry("prune-skip");
        let info = registry
            .create(SessionNewParams {
                cwd: Some(repo.clone()),
                branch: Some("feat/x".to_owned()),
                ..params()
            })
            .await
            .expect("worktree session created");
        let worktree = info.worktree_path.clone().expect("worktree path");
        let project_id = info.project_id.clone().expect("project stamped");
        // The session is left RUNNING (not stopped) — it is live in the worktree.

        let result = registry
            .remove_project(&project_id, true)
            .await
            .expect("remove with prune");
        assert!(
            !result.removed,
            "the record is kept while a live worktree remains"
        );
        assert_eq!(
            result.pruned_worktrees, 0,
            "the live worktree is not pruned"
        );
        assert_eq!(
            result.skipped_worktrees,
            vec![info.id.0.clone()],
            "the live session is reported as skipped"
        );
        assert!(
            worktree.exists(),
            "a live session's worktree is left on disk"
        );
        assert!(
            !registry
                .projects()
                .expect("projects")
                .store()
                .load_projects()
                .expect("load")
                .is_empty(),
            "the record stays so the skipped worktree's binding is not dangling"
        );
    }

    #[tokio::test]
    async fn remove_project_without_prune_leaves_worktrees_intact() {
        let (registry, repo) = project_registry("no-prune");
        let info = registry
            .create(SessionNewParams {
                cwd: Some(repo.clone()),
                branch: Some("feat/x".to_owned()),
                ..params()
            })
            .await
            .expect("worktree session created");
        let worktree = info.worktree_path.clone().expect("worktree path");
        let project_id = info.project_id.clone().expect("project stamped");
        registry.stop(&info.id).await.expect("stop session");

        let result = registry
            .remove_project(&project_id, false)
            .await
            .expect("remove without prune");
        assert!(result.removed);
        assert_eq!(
            result.pruned_worktrees, 0,
            "nothing pruned without the flag"
        );
        assert!(
            worktree.exists(),
            "a plain rm must leave the worktree on disk"
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
    async fn session_start_hook_runs_after_spawn_without_blocking_create() {
        let config_dir = temp_dir("session-start-config");
        let cwd = temp_dir("session-start-cwd");
        let marker = config_dir.join("session-start.marker");
        write_host_hook(
            &config_dir,
            "session-start",
            &format!(
                "#!/bin/sh\nprintf '%s:%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_SESSION_ID\" \"$POHUNEK_AGENT\" >> {}\n",
                marker.display()
            ),
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            config_dir: Some(config_dir.clone()),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(SessionNewParams {
                cwd: Some(cwd),
                ..params()
            })
            .await
            .expect("create session returns while hook runs best-effort");

        let contents =
            wait_for_file_contains(&marker, &format!("session-start:{}:shell", created.id.0)).await;
        assert_eq!(contents.lines().count(), 1, "session-start fires once");

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn session_start_hook_is_best_effort_when_hook_hangs() {
        let config_dir = temp_dir("session-start-hang-config");
        let cwd = temp_dir("session-start-hang-cwd");
        write_host_hook(&config_dir, "session-start", "#!/bin/sh\nsleep 30\n");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            hook_timeout: Duration::from_millis(50),
            config_dir: Some(config_dir),
            ..SessionRegistryConfig::default()
        });

        let started = std::time::Instant::now();
        let created = registry
            .create(SessionNewParams {
                cwd: Some(cwd),
                ..params()
            })
            .await
            .expect("create session returns despite a hanging session-start hook");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "session-start hook must be best-effort and not wedge create"
        );

        let _ = registry.stop(&created.id).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn session_stop_hook_reports_stopped_done_and_failed_reasons_once() {
        async fn run_case(tag: &str, command: &str, stop: bool, expected_reason: &str) {
            let config_dir = temp_dir(&format!("session-stop-config-{tag}"));
            let cwd = temp_dir(&format!("session-stop-cwd-{tag}"));
            let store_path = temp_store_path(&format!("session-stop-store-{tag}"));
            let agents_dir = temp_agents_dir_with(
                &format!("session-stop-agent-{tag}"),
                "resumable",
                &format!(
                    "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"{command}\"]\n"
                ),
            );
            let marker = config_dir.join("session-stop.marker");
            write_host_hook(
                &config_dir,
                "session-stop",
                &format!(
                    "#!/bin/sh\nprintf '%s:%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_SESSION_ID\" \"$POHUNEK_STOP_REASON\" >> {}\n",
                    marker.display()
                ),
            );
            let registry = SessionRegistry::new(SessionRegistryConfig {
                stop_grace: Duration::from_millis(50),
                config_dir: Some(config_dir),
                store_path: Some(store_path.clone()),
                agents_dir: Some(agents_dir),
                ..SessionRegistryConfig::default()
            });

            let created = registry
                .create(SessionNewParams {
                    cwd: Some(cwd),
                    ..resumable_params()
                })
                .await
                .expect("create session");
            let recorded = registry
                .report_native_id(SessionReportNativeIdParams {
                    session_id: created.id.clone(),
                    agent: "claude".to_owned(),
                    native_session_id: format!("native-{tag}"),
                    transcript_path: None,
                })
                .await;
            assert!(recorded.recorded, "native id captured for {tag}");
            assert_eq!(
                crate::store::Store::new(store_path.clone())
                    .load_resume()
                    .expect("load before terminal")
                    .len(),
                1,
                "terminal transition precondition: one resume binding for {tag}"
            );

            if stop {
                registry.stop(&created.id).await.expect("stop session");
            } else {
                registry
                    .wait_for_exit(&created.id, Duration::from_secs(2))
                    .await
                    .expect("session exits");
            }

            let expected = format!("session-stop:{}:{expected_reason}", created.id.0);
            let contents = wait_for_file_contains(&marker, &expected).await;
            assert_eq!(
                contents.lines().count(),
                1,
                "session-stop fires once for {tag}: {contents:?}"
            );
            assert!(
                crate::store::Store::new(store_path)
                    .load_resume()
                    .expect("load after terminal")
                    .is_empty(),
                "terminal transition must remove resume binding for {tag}"
            );
        }

        run_case("stopped", "sleep 30", true, "stopped").await;
        run_case("done", "sleep 0.2; exit 0", false, "done").await;
        run_case("failed", "sleep 0.2; exit 7", false, "failed").await;
    }

    #[tokio::test]
    async fn agent_state_hook_fires_once_per_distinct_activity_value() {
        let config_dir = temp_dir("agent-state-config");
        let cwd = temp_dir("agent-state-cwd");
        let marker = config_dir.join("agent-state.marker");
        write_host_hook(
            &config_dir,
            "agent-state",
            &format!(
                "#!/bin/sh\nprintf '%s:%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_SESSION_ID\" \"$POHUNEK_ACTIVITY\" >> {}\n",
                marker.display()
            ),
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            config_dir: Some(config_dir),
            ..SessionRegistryConfig::default()
        });
        registry.spawn_agent_state_hooks();
        let created = registry
            .create(SessionNewParams {
                cwd: Some(cwd),
                ..params()
            })
            .await
            .expect("create session");

        registry
            .record_activity(&created.id, transition(AgentActivity::Working))
            .await;
        let mut contents = wait_for_line_count(&marker, 1).await;
        assert!(contents.contains(&format!("agent-state:{}:working", created.id.0)));

        registry
            .record_activity(&created.id, transition(AgentActivity::Working))
            .await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        contents = fs::read_to_string(&marker).expect("read marker");
        assert_eq!(
            contents.lines().count(),
            1,
            "same-state refresh must not fire another hook: {contents:?}"
        );

        for (activity, expected_count) in [
            (AgentActivity::Blocked, 2),
            (AgentActivity::Working, 3),
            (AgentActivity::Idle, 4),
            (AgentActivity::Working, 5),
        ] {
            registry
                .record_activity(&created.id, transition(activity))
                .await;
            contents = wait_for_line_count(&marker, expected_count).await;
        }

        let lines: Vec<String> = contents.lines().map(str::to_owned).collect();
        assert_eq!(lines.len(), 5, "only distinct values fire: {contents:?}");
        assert_eq!(
            lines,
            vec![
                format!("agent-state:{}:working", created.id.0),
                format!("agent-state:{}:blocked", created.id.0),
                format!("agent-state:{}:working", created.id.0),
                format!("agent-state:{}:idle", created.id.0),
                format!("agent-state:{}:working", created.id.0),
            ]
        );

        registry.stop(&created.id).await.expect("stop session");
        registry.shutdown_agent_state_hooks().await;
    }

    #[tokio::test]
    async fn session_layer_hooks_run_with_cleared_env_and_exact_allowlist() {
        let config_dir = temp_dir("session-hook-env-config");
        let cwd = temp_dir("session-hook-env-cwd");
        let start_env = config_dir.join("session-start.env");
        let state_env = config_dir.join("agent-state.env");
        write_host_hook(
            &config_dir,
            "session-start",
            &format!("#!/bin/sh\nenv > {}\n", start_env.display()),
        );
        write_host_hook(
            &config_dir,
            "agent-state",
            &format!("#!/bin/sh\nenv > {}\n", state_env.display()),
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            config_dir: Some(config_dir),
            ..SessionRegistryConfig::default()
        });
        registry.spawn_agent_state_hooks();
        let created = registry
            .create(SessionNewParams {
                cwd: Some(cwd),
                ..params()
            })
            .await
            .expect("create session");

        wait_for_file_contains(&start_env, "POHUNEK_HOOK_EVENT=session-start").await;
        registry
            .record_activity(&created.id, transition(AgentActivity::Working))
            .await;
        wait_for_file_contains(&state_env, "POHUNEK_ACTIVITY=working").await;

        let start = parse_env_dump(&fs::read_to_string(&start_env).expect("read start env"));
        assert_eq!(
            start.get("POHUNEK_HOOK_EVENT").map(String::as_str),
            Some("session-start")
        );
        assert_eq!(
            start.get("POHUNEK_SESSION_ID").map(String::as_str),
            Some(created.id.0.as_str())
        );
        assert_eq!(
            start.get("POHUNEK_AGENT").map(String::as_str),
            Some("shell")
        );
        assert_eq!(
            pohunek_env_keys(&start),
            [
                "POHUNEK_AGENT",
                "POHUNEK_HOOK_EVENT",
                "POHUNEK_PROJECT_ID",
                "POHUNEK_SESSION_ID",
            ]
            .map(str::to_owned)
            .to_vec()
        );

        let state = parse_env_dump(&fs::read_to_string(&state_env).expect("read state env"));
        assert_eq!(
            state.get("POHUNEK_HOOK_EVENT").map(String::as_str),
            Some("agent-state")
        );
        assert_eq!(
            state.get("POHUNEK_ACTIVITY").map(String::as_str),
            Some("working")
        );
        assert_eq!(
            pohunek_env_keys(&state),
            [
                "POHUNEK_ACTIVITY",
                "POHUNEK_AGENT",
                "POHUNEK_HOOK_EVENT",
                "POHUNEK_PROJECT_ID",
                "POHUNEK_SESSION_ID",
            ]
            .map(str::to_owned)
            .to_vec()
        );

        for env in [&start, &state] {
            assert!(env.contains_key("PATH"), "PATH is passed through");
            assert!(
                !env.keys().any(|key| key.starts_with("CARGO")),
                "daemon inherited CARGO_* env must be cleared: {:?}",
                env.keys().collect::<Vec<_>>()
            );
            for forbidden in [
                "GITHUB_TOKEN",
                "ANTHROPIC_API_KEY",
                "POHUNEK_SOCKET_PATH",
                "POHUNEK_DAEMON_ID",
                "POHUNEK_ENV",
                "POHUNEK_PROTOCOL_VERSION",
                "POHUNEK_REPO",
                "POHUNEK_WORKTREE",
                "POHUNEK_BRANCH",
                "POHUNEK_BASE_BRANCH",
            ] {
                assert!(
                    !env.contains_key(forbidden),
                    "{forbidden} must not be exposed to a session-layer hook"
                );
            }
        }

        registry.stop(&created.id).await.expect("stop session");
        registry.shutdown_agent_state_hooks().await;
    }

    #[tokio::test]
    async fn in_place_session_fires_session_hooks_but_no_worktree_hooks() {
        let config_dir = temp_dir("in-place-hooks-config");
        let repo = init_git_repo("in-place-hooks-repo");
        let marker = config_dir.join("hooks.marker");
        for event_name in [
            "pre-create",
            "post-create",
            "pre-remove",
            "post-remove",
            "session-start",
            "session-stop",
            "agent-state",
        ] {
            write_host_hook(
                &config_dir,
                event_name,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$POHUNEK_HOOK_EVENT\" >> {}\n",
                    marker.display()
                ),
            );
        }
        let store = temp_store_path("in-place-hooks-store");
        let worktree_root = store.parent().expect("store parent").join("worktrees");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store),
            worktree_root: Some(worktree_root),
            config_dir: Some(config_dir),
            ..SessionRegistryConfig::default()
        });
        registry.spawn_agent_state_hooks();

        let created = registry
            .create(SessionNewParams {
                cwd: Some(repo),
                ..params()
            })
            .await
            .expect("create in-place session");
        assert_eq!(created.worktree_path, None, "no --branch means in-place");
        registry
            .record_activity(&created.id, transition(AgentActivity::Working))
            .await;
        wait_for_file_contains(&marker, "agent-state").await;
        registry.stop(&created.id).await.expect("stop session");
        let contents = wait_for_line_count(&marker, 3).await;

        let lines: Vec<&str> = contents.lines().collect();
        assert!(lines.contains(&"session-start"));
        assert!(lines.contains(&"agent-state"));
        assert!(lines.contains(&"session-stop"));
        for forbidden in ["pre-create", "post-create", "pre-remove", "post-remove"] {
            assert!(
                !lines.contains(&forbidden),
                "in-place sessions must not run {forbidden}: {contents:?}"
            );
        }

        registry.shutdown_agent_state_hooks().await;
    }

    #[tokio::test]
    async fn project_backed_session_hooks_receive_project_id() {
        let config_dir = temp_dir("project-session-hooks-config");
        let repo = init_git_repo("project-session-hooks-repo");
        let marker = config_dir.join("project-hooks.marker");
        for event_name in ["session-start", "session-stop", "agent-state"] {
            write_host_hook(
                &config_dir,
                event_name,
                &format!(
                    "#!/bin/sh\nprintf '%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_PROJECT_ID\" >> {}\n",
                    marker.display()
                ),
            );
        }
        let store = temp_store_path("project-session-hooks-store");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            store_path: Some(store),
            config_dir: Some(config_dir),
            ..SessionRegistryConfig::default()
        });
        registry.spawn_agent_state_hooks();

        let created = registry
            .create(SessionNewParams {
                cwd: Some(repo),
                ..params()
            })
            .await
            .expect("create project-backed in-place session");
        let project_id = created.project_id.clone().expect("project id stamped");
        wait_for_file_contains(&marker, &format!("session-start:{project_id}")).await;

        registry
            .record_activity(&created.id, transition(AgentActivity::Working))
            .await;
        wait_for_file_contains(&marker, &format!("agent-state:{project_id}")).await;

        registry.stop(&created.id).await.expect("stop session");
        let contents = wait_for_file_contains(&marker, &format!("session-stop:{project_id}")).await;
        for event_name in ["session-start", "agent-state", "session-stop"] {
            assert!(
                contents.contains(&format!("{event_name}:{project_id}")),
                "{event_name} must receive project id {project_id}: {contents:?}"
            );
        }

        registry.shutdown_agent_state_hooks().await;
    }

    #[tokio::test]
    async fn agent_state_hook_dispatcher_survives_lag_and_shutdown_cancellation() {
        let config_dir = temp_dir("agent-state-lag-config");
        let cwd = temp_dir("agent-state-lag-cwd");
        let marker = config_dir.join("agent-state-lag.marker");
        write_host_hook(
            &config_dir,
            "agent-state",
            &format!(
                "#!/bin/sh\nsleep 0.1\nprintf '%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\n",
                marker.display()
            ),
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            config_dir: Some(config_dir),
            ..SessionRegistryConfig::default()
        });
        let created = registry
            .create(SessionNewParams {
                cwd: Some(cwd),
                ..params()
            })
            .await
            .expect("create session");

        registry
            .record_activity(&created.id, transition(AgentActivity::Working))
            .await;
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        for n in 0..8 {
            let _ = tx.send(Event::new(
                protocol::event::SESSION_UPDATED,
                serde_json::json!({ "n": n }),
            ));
        }
        let shutdown = tokio_util::sync::CancellationToken::new();
        let handle =
            super::spawn_agent_state_hook_dispatcher(registry.clone(), rx, shutdown.clone());
        wait_for_file_contains(&marker, "working").await;
        for n in 8..16 {
            let _ = tx.send(Event::new(
                protocol::event::SESSION_UPDATED,
                serde_json::json!({ "n": n }),
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let contents = fs::read_to_string(&marker).expect("read marker after same-state lag");
        assert_eq!(
            contents.lines().count(),
            1,
            "lag re-read of the already-fired activity must not double-fire: {contents:?}"
        );

        registry
            .record_activity(&created.id, transition(AgentActivity::Blocked))
            .await;
        tx.send(Event::new(
            protocol::event::AGENT_STATE,
            serde_json::json!({
                "session_id": created.id.clone(),
                "activity": "blocked",
                "source": "process",
            }),
        ))
        .expect("send blocked event");
        wait_for_file_contains(&marker, "blocked").await;

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("dispatcher joins after cancellation")
            .expect("dispatcher task succeeds");

        registry.stop(&created.id).await.expect("stop session");
    }

    #[tokio::test]
    async fn agent_state_hook_dispatcher_flushes_buffered_event_on_shutdown() {
        let config_dir = temp_dir("agent-state-shutdown-config");
        let cwd = temp_dir("agent-state-shutdown-cwd");
        let marker = config_dir.join("agent-state-shutdown.marker");
        write_host_hook(
            &config_dir,
            "agent-state",
            &format!(
                "#!/bin/sh\nsleep 0.15\nprintf '%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\n",
                marker.display()
            ),
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            config_dir: Some(config_dir),
            ..SessionRegistryConfig::default()
        });
        let created = registry
            .create(SessionNewParams {
                cwd: Some(cwd),
                ..params()
            })
            .await
            .expect("create session");

        registry
            .record_activity(&created.id, transition(AgentActivity::Working))
            .await;
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let handle =
            super::spawn_agent_state_hook_dispatcher(registry.clone(), rx, shutdown.clone());
        tx.send(Event::new(
            protocol::event::AGENT_STATE,
            serde_json::json!({
                "session_id": created.id.clone(),
                "activity": "working",
                "source": "process",
            }),
        ))
        .expect("send buffered event");
        shutdown.cancel();
        handle.await.expect("dispatcher joins after cancellation");

        let contents = fs::read_to_string(&marker)
            .expect("dispatcher must await the hook flushed during shutdown");
        assert!(
            contents.contains("working"),
            "dispatcher must flush and await buffered activity before joining: {contents:?}"
        );
        registry.stop(&created.id).await.expect("stop session");
    }

    #[tokio::test]
    async fn agent_state_hook_coalesces_flaps_while_hook_is_in_flight() {
        let config_dir = temp_dir("agent-state-coalesce-config");
        let cwd = temp_dir("agent-state-coalesce-cwd");
        let marker = config_dir.join("agent-state-coalesce.marker");
        write_host_hook(
            &config_dir,
            "agent-state",
            &format!(
                "#!/bin/sh\nsleep 0.2\nprintf '%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\n",
                marker.display()
            ),
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            hook_timeout: Duration::from_secs(1),
            config_dir: Some(config_dir),
            ..SessionRegistryConfig::default()
        });
        registry.spawn_agent_state_hooks();
        let created = registry
            .create(SessionNewParams {
                cwd: Some(cwd),
                ..params()
            })
            .await
            .expect("create session");

        registry
            .record_activity(&created.id, transition(AgentActivity::Working))
            .await;
        tokio::time::sleep(Duration::from_millis(90)).await;
        registry
            .record_activity(&created.id, transition(AgentActivity::Blocked))
            .await;
        registry
            .record_activity(&created.id, transition(AgentActivity::Idle))
            .await;
        registry
            .record_activity(&created.id, transition(AgentActivity::Blocked))
            .await;

        let contents = wait_for_line_count(&marker, 2).await;
        assert_eq!(
            contents.lines().collect::<Vec<_>>(),
            vec!["working", "blocked"],
            "only one hook runs in flight per session and intermediate flap is coalesced"
        );

        registry.stop(&created.id).await.expect("stop session");
        registry.shutdown_agent_state_hooks().await;
    }

    #[test]
    fn invalid_agent_activity_parse_returns_error() {
        let err = super::parse_agent_activity(&serde_json::json!("future-state"))
            .expect_err("unknown activity should remain an explicit parse error");
        assert!(
            err.to_string().contains("future-state"),
            "parse error should name the invalid activity: {err}"
        );
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
            stop_grace: Duration::from_millis(500),
            attach_token_ttl: Duration::from_millis(1),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");
        let expired = registry
            .attach(&attach_params(&created.id))
            .await
            .expect("attach token");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let fresh = registry
            .attach(&attach_params(&created.id))
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
    async fn attach_from_inside_the_same_session_is_rejected() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });
        let created = registry.create(params()).await.expect("create session");
        let daemon_id = registry.daemon_instance_id().to_owned();

        let self_feed = |session: &SessionId, daemon: Option<&str>| SessionAttachParams {
            session_id: session.clone(),
            origin_session_id: Some(session.clone()),
            origin_daemon_id: daemon.map(str::to_owned),
        };

        // Origin id AND daemon id both match this instance: the client is inside
        // this session's own PTY, so attaching would loop its output into its own
        // input. Reject it.
        let err = registry
            .attach(&self_feed(&created.id, Some(&daemon_id)))
            .await
            .expect_err("self-feeding attach must be rejected");
        assert_eq!(err.code, "attach_self_feedback");
        assert_eq!(err.class, protocol::ErrorClass::Daemon);
        assert!(
            err.recover.is_some(),
            "self-feedback error must carry a recovery hint: {err:?}"
        );
        // The rejected attach mints no pending token.
        assert!(
            registry.inner.pending_attaches.lock().await.is_empty(),
            "a rejected self-feeding attach must not leave a pending token"
        );

        // Same session id but a DIFFERENT daemon instance (a colliding id on
        // another daemon, or a stale value from a previous process): no loop, so
        // it must be allowed, not falsely rejected.
        registry
            .attach(&self_feed(&created.id, Some("some-other-daemon")))
            .await
            .expect("matching id on a different daemon instance is allowed");
        // Origin id without any daemon id cannot be pinned to this instance.
        registry
            .attach(&self_feed(&created.id, None))
            .await
            .expect("origin id without a daemon id is allowed");
        // A different session's terminal (this daemon) is a legitimate origin.
        registry
            .attach(&SessionAttachParams {
                session_id: created.id.clone(),
                origin_session_id: Some(SessionId("s-other".to_owned())),
                origin_daemon_id: Some(daemon_id.clone()),
            })
            .await
            .expect("attach from a different session's terminal is allowed");
        // A plain terminal (no origin reported) is allowed.
        registry
            .attach(&attach_params(&created.id))
            .await
            .expect("attach with no origin is allowed");

        registry.stop(&created.id).await.expect("stop session");
    }

    #[test]
    fn lag_warn_throttle_logs_first_then_flushes_one_summary_per_window() {
        let interval = Duration::from_secs(5);
        let mut throttle = LagWarnThrottle::new(interval);
        let t0 = Instant::now();

        // First lag of a window logs immediately with its own skip count.
        assert_eq!(throttle.observe(t0, 2), Some(LagWarn::First { skipped: 2 }));
        // Further lags inside the window fold silently.
        assert_eq!(throttle.observe(t0 + Duration::from_secs(1), 3), None);
        assert_eq!(throttle.observe(t0 + Duration::from_secs(2), 4), None);
        // A tick before the window elapses flushes nothing.
        assert_eq!(throttle.poll(t0 + Duration::from_secs(3)), None);
        // Once the window elapses, the tick flushes ONE summary of the folded lags
        // (events 2 = the two folded; skipped 3 + 4), excluding the already-logged
        // first.
        assert_eq!(
            throttle.poll(t0 + interval),
            Some(LagWarn::Summary {
                events: 2,
                skipped: 7,
            })
        );
        // The window reopens at the flush; with nothing folded yet, the next tick
        // past the interval just closes it (no spurious summary)...
        assert_eq!(throttle.poll(t0 + interval + interval), None);
        // ...and the next lag after a closed window logs as a fresh First.
        assert_eq!(
            throttle.observe(t0 + interval + interval + Duration::from_secs(1), 9),
            Some(LagWarn::First { skipped: 9 })
        );
    }

    #[test]
    fn lag_warn_throttle_flush_emits_trailing_batch_on_teardown() {
        let interval = Duration::from_secs(5);
        let mut throttle = LagWarnThrottle::new(interval);
        let t0 = Instant::now();

        assert_eq!(throttle.observe(t0, 1), Some(LagWarn::First { skipped: 1 }));
        assert_eq!(throttle.observe(t0 + Duration::from_secs(1), 2), None);
        assert_eq!(throttle.observe(t0 + Duration::from_secs(2), 3), None);
        // Session torn down mid-window: flush reports the folded tail (events 2,
        // skipped 2 + 3) instead of silently dropping it.
        assert_eq!(
            throttle.flush(),
            Some(LagWarn::Summary {
                events: 2,
                skipped: 5,
            })
        );
        // A second flush with nothing pending is a no-op.
        assert_eq!(throttle.flush(), None);
    }

    #[test]
    fn lag_warn_throttle_zero_interval_logs_every_lag() {
        let mut throttle = LagWarnThrottle::new(Duration::ZERO);
        let t0 = Instant::now();
        // Folding disabled: every lag logs immediately, never folded or dropped.
        assert_eq!(throttle.observe(t0, 1), Some(LagWarn::First { skipped: 1 }));
        assert_eq!(throttle.observe(t0, 7), Some(LagWarn::First { skipped: 7 }));
        assert_eq!(throttle.flush(), None);
    }

    #[test]
    fn lag_warn_throttle_relogs_first_after_a_quiet_gap() {
        let interval = Duration::from_secs(5);
        let mut throttle = LagWarnThrottle::new(interval);
        let t0 = Instant::now();

        // A storm: first lag logged, a second folded within the window.
        assert_eq!(throttle.observe(t0, 1), Some(LagWarn::First { skipped: 1 }));
        assert_eq!(throttle.observe(t0 + Duration::from_secs(1), 2), None);
        // The window elapses and the tick flushes the folded lag, reopening it.
        assert_eq!(
            throttle.poll(t0 + interval),
            Some(LagWarn::Summary {
                events: 1,
                skipped: 2,
            })
        );

        // A long silence, then a brand-new lag more than an interval after the
        // previous one: it is a fresh storm and must log as a First again — never
        // folded/mislabeled as a continuation just because poll left a window open.
        let later = t0 + interval + interval + Duration::from_secs(1);
        assert_eq!(
            throttle.observe(later, 4),
            Some(LagWarn::First { skipped: 4 })
        );
    }

    #[test]
    fn daemon_instance_ids_are_distinct_per_registry() {
        // Two registries built in this one process must still get distinct ids
        // (the process-local counter disambiguates same-instant construction), so
        // the self-feeding-attach guard never conflates two daemon instances.
        let a = SessionRegistry::default();
        let b = SessionRegistry::default();
        assert_ne!(
            a.daemon_instance_id(),
            b.daemon_instance_id(),
            "each registry must get a distinct daemon instance id"
        );
        assert!(a.daemon_instance_id().starts_with("d-"));
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

    #[test]
    fn session_pty_env_marks_session_id_for_every_agent_kind() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            socket_path: Some(PathBuf::from("/run/pohunek/daemon.sock")),
            ..SessionRegistryConfig::default()
        });
        let id = SessionId("s-7".to_owned());

        // Every kind — including a plain shell, which gets no hook handshake —
        // carries POHUNEK_SESSION_ID and POHUNEK_DAEMON_ID so a self-feeding
        // attach is detectable and pinned to this daemon instance.
        for agent in [AgentKind::Shell, AgentKind::Codex, AgentKind::Claude] {
            let env = registry.session_pty_env(agent, &id);
            let session_ids: Vec<&str> = env
                .iter()
                .filter(|(k, _)| k == ENV_SESSION_ID)
                .map(|(_, v)| v.as_str())
                .collect();
            // Present exactly once (agents must not get it duplicated on top of
            // the hook env that already carries it).
            assert_eq!(
                session_ids,
                vec!["s-7"],
                "{agent:?} must carry POHUNEK_SESSION_ID exactly once"
            );
            let daemon_ids: Vec<&str> = env
                .iter()
                .filter(|(k, _)| k == ENV_DAEMON_ID)
                .map(|(_, v)| v.as_str())
                .collect();
            assert_eq!(
                daemon_ids,
                vec![registry.daemon_instance_id()],
                "{agent:?} must carry POHUNEK_DAEMON_ID once, equal to this instance's id"
            );
        }

        // The shell carries *only* the session-id marker, not the agent handshake.
        let shell_env = registry.session_pty_env(AgentKind::Shell, &id);
        assert!(
            !shell_env.iter().any(|(k, _)| k == ENV_FLAG),
            "a shell must not get the agent-hook gate flag: {shell_env:?}"
        );
    }

    #[tokio::test]
    async fn report_native_id_records_binding_and_updates_info() {
        let store_path = temp_store_path("report");
        let agents_dir = temp_resumable_agents_dir("report");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(resumable_params())
            .await
            .expect("create session");
        assert_eq!(created.native_session_id, None);

        let result = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: "native-abc".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(result.recorded);

        // In-memory info now carries the native id.
        let inspected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(inspected.native_session_id.as_deref(), Some("native-abc"));

        // The binding was persisted to the store.
        let persisted = crate::store::Store::new(store_path.clone())
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

    /// Create a temp `agents/` dir holding one profile file; return the dir path.
    fn temp_agents_dir_with(tag: &str, name: &str, body: &str) -> PathBuf {
        let dir = temp_store_path(tag)
            .parent()
            .expect("store parent")
            .join("agents");
        std::fs::create_dir_all(&dir).expect("create agents dir");
        std::fs::write(dir.join(format!("{name}.toml")), body).expect("write profile");
        dir
    }

    fn temp_resumable_agents_dir(tag: &str) -> PathBuf {
        temp_agents_dir_with(
            tag,
            "resumable",
            "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
        )
    }

    fn resumable_params() -> SessionNewParams {
        SessionNewParams {
            agent: "resumable".to_owned(),
            ..params()
        }
    }

    #[tokio::test]
    async fn report_native_id_path_profile_stores_path_and_ignores_wire_agent() {
        // The load-bearing C.3 fix: a `ref_kind = "path"` profile must store the
        // native reference into `native_session_path` (clearing `native_session_id`),
        // chosen by the FROZEN snapshot — never by the wire `agent` literal, which
        // the SessionStart hook bakes to a base-kind name carrying no profile id.
        let store_path = temp_store_path("path-profile");
        let agents_dir = temp_agents_dir_with(
            "path-profile",
            "pathy",
            "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n[resume]\nref_kind = \"path\"\n",
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(SessionNewParams {
                agent: "pathy".to_owned(),
                ..params()
            })
            .await
            .expect("create path-profile session");

        let result = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                // Wire agent is a base-kind literal (what the hook reports); ignored
                // for ref-kind selection.
                agent: "claude".to_owned(),
                native_session_id: "opaque-native-id".to_owned(),
                transcript_path: Some("/home/u/.claude/t.jsonl".to_owned()),
            })
            .await;
        assert!(result.recorded);

        let inspected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(
            inspected.native_session_path.as_deref(),
            Some("/home/u/.claude/t.jsonl"),
            "a path-kind profile stores into native_session_path"
        );
        assert_eq!(
            inspected.native_session_id, None,
            "the id field is left empty for a path-kind session"
        );

        let persisted = crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load store");
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            persisted[0].native_session_path.as_deref(),
            Some("/home/u/.claude/t.jsonl")
        );
        assert_eq!(persisted[0].native_session_id, None);
        assert_eq!(persisted[0].ref_kind, Some(SessionRefKind::Path));
        assert_eq!(persisted[0].resume_mode, Some(ResumeMode::Flag));
        assert_eq!(persisted[0].program, "/bin/sh");
        assert!(persisted[0].resumable);

        registry
            .resize(&created.id, 120, 40)
            .await
            .expect("resize path-profile session");
        let resized = crate::store::Store::new(store_path)
            .load_resume()
            .expect("load resized binding");
        assert_eq!(
            (resized[0].cols, resized[0].rows),
            (120, 40),
            "path-kind resume binding must refresh dimensions after resize"
        );

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn non_resumable_profile_ignores_native_id_reports() {
        let store_path = temp_store_path("noresume-profile");
        let agents_dir = temp_agents_dir_with(
            "noresume-profile",
            "noresume",
            "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n[resume]\nresumable = false\n",
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(SessionNewParams {
                agent: "noresume".to_owned(),
                ..params()
            })
            .await
            .expect("create non-resumable profile session");

        let result = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "codex".to_owned(),
                native_session_id: "native-ignored".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(
            !result.recorded,
            "non-resumable profile must reject native-id reports fail-closed"
        );
        assert!(
            crate::store::Store::new(store_path)
                .load_resume()
                .expect("load")
                .is_empty(),
            "non-resumable profile must not persist a resume binding"
        );

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn non_resumable_profile_binding_reports_agent_not_resumable() {
        let registry = SessionRegistry::default();
        let binding = crate::store::ResumeBinding {
            session_id: "s-noresume".to_owned(),
            agent: "noresume".to_owned(),
            agent_base: AgentKind::Codex,
            cwd: temp_dir("noresume-binding-cwd"),
            cols: 80,
            rows: 24,
            native_session_id: Some("native-ignored".to_owned()),
            native_session_path: None,
            project_id: None,
            is_linked_worktree: None,
            program: "/bin/sh".to_owned(),
            args: Vec::new(),
            input_rules: crate::store::StoredInputRules::default(),
            resume_mode: Some(ResumeMode::Flag),
            ref_kind: Some(SessionRefKind::Id),
            resumable: false,
        };

        let err = registry
            .resume_binding(binding)
            .await
            .expect_err("non-resumable binding must fail");
        assert_eq!(err.code, "agent_not_resumable");
    }

    #[tokio::test]
    async fn resume_binding_never_persists_profile_env_secrets() {
        // C.4 no-secrets invariant: a profile's `[env]` (which may hold secrets) is
        // never written to the store. The serialized resume line must contain none
        // of the env keys OR values — env is re-resolved by agent name at resume.
        let store_path = temp_store_path("env-secret");
        let agents_dir = temp_agents_dir_with(
            "env-secret",
            "withenv",
            "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n[env]\nSECRET_TOKEN = \"supersecretvalue\"\n",
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(SessionNewParams {
                agent: "withenv".to_owned(),
                ..params()
            })
            .await
            .expect("create env-profile session");
        let result = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: "native-xyz".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(result.recorded);

        let raw = std::fs::read_to_string(&store_path).expect("read store file");
        assert!(
            !raw.contains("SECRET_TOKEN") && !raw.contains("supersecretvalue"),
            "profile env (key or value) must never reach the store: {raw}"
        );

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn stopping_a_session_drops_its_resume_binding() {
        let store_path = temp_store_path("drop-on-stop");
        let agents_dir = temp_resumable_agents_dir("drop-on-stop");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(resumable_params())
            .await
            .expect("create session");
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
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
        let agents_dir = temp_resumable_agents_dir("resize-binding");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(resumable_params())
            .await
            .expect("create session");
        // Capture a native id so a resume binding exists at the launch size.
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
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
                agent: "claude".to_owned(),
                agent_base: AgentKind::Claude,
                cwd: PathBuf::from("/tmp"),
                cols: 80,
                rows: 24,
                native_session_id: None,
                native_session_path: None,
                project_id: None,
                is_linked_worktree: None,
                program: String::new(),
                args: Vec::new(),
                input_rules: crate::store::StoredInputRules::default(),
                resume_mode: None,
                ref_kind: None,
                resumable: false,
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

    #[cfg(unix)]
    #[tokio::test]
    async fn load_and_resume_uses_frozen_profile_args() {
        let store_path = temp_store_path("resume-args");
        let dir = temp_dir("resume-args-runtime");
        let script = dir.join("resume-agent");
        let marker = dir.join("argv.txt");
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nsleep 30\n",
                marker.display()
            ),
        );
        let store = crate::store::Store::new(store_path.clone());
        store
            .record_resume(&crate::store::ResumeBinding {
                session_id: "s-44".to_owned(),
                agent: "profiled".to_owned(),
                agent_base: AgentKind::Claude,
                cwd: dir.clone(),
                cols: 80,
                rows: 24,
                native_session_id: Some("native-44".to_owned()),
                native_session_path: None,
                project_id: None,
                is_linked_worktree: None,
                program: script.display().to_string(),
                args: vec!["--model".to_owned(), "sonnet".to_owned()],
                input_rules: crate::store::StoredInputRules::default(),
                resume_mode: Some(ResumeMode::Flag),
                ref_kind: Some(SessionRefKind::Id),
                resumable: true,
            })
            .expect("seed resume binding");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            store_path: Some(store_path),
            ..SessionRegistryConfig::default()
        });

        registry.load_and_resume().await;

        let argv = wait_for_file_contains(&marker, "native-44").await;
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            vec!["--model", "sonnet", "--resume", "native-44"],
            "resume relaunch must preserve frozen profile args before resume argv"
        );

        let _ = registry.stop(&SessionId("s-44".to_owned())).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resume_after_profile_edit_and_resize_uses_original_snapshot() {
        let store_path = temp_store_path("resume-edit-resize");
        let dir = temp_dir("resume-edit-resize-runtime");
        let script_v1 = dir.join("agent-v1");
        let script_v2 = dir.join("agent-v2");
        let marker_v1 = dir.join("v1-argv.txt");
        let marker_v2 = dir.join("v2-argv.txt");
        write_executable(
            &script_v1,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nsleep 30\n",
                marker_v1.display()
            ),
        );
        write_executable(
            &script_v2,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nsleep 30\n",
                marker_v2.display()
            ),
        );
        let agents_dir = temp_agents_dir_with(
            "resume-edit-resize",
            "editable",
            &format!(
                "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"sonnet\"]\n",
                script_v1.display()
            ),
        );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir.clone()),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(SessionNewParams {
                agent: "editable".to_owned(),
                cwd: Some(dir.clone()),
                ..params()
            })
            .await
            .expect("create editable profile session");
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: "native-edit-resize".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(recorded.recorded);
        registry
            .resize(&created.id, 123, 55)
            .await
            .expect("resize captured session");
        let binding = crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load resized binding")
            .into_iter()
            .next()
            .expect("resume binding exists");
        assert_eq!((binding.cols, binding.rows), (123, 55));

        registry
            .stop(&created.id)
            .await
            .expect("stop original session after snapshot capture");
        fs::write(&marker_v1, "").expect("clear v1 marker");
        fs::write(
            agents_dir.join("editable.toml"),
            format!(
                "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"opus\"]\n",
                script_v2.display()
            ),
        )
        .expect("edit profile");

        let restarted = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });
        let resumed = restarted
            .resume_binding(binding)
            .await
            .expect("resume from frozen binding");

        assert_eq!((resumed.cols, resumed.rows), (123, 55));
        let argv = wait_for_file_contains(&marker_v1, "native-edit-resize").await;
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            vec!["--model", "sonnet", "--resume", "native-edit-resize"],
            "resume must use launch-time program/args, not the edited profile"
        );
        assert!(
            !marker_v2.exists()
                || fs::read_to_string(&marker_v2)
                    .unwrap_or_default()
                    .is_empty(),
            "edited profile program must not run during resume"
        );

        let _ = restarted.stop(&resumed.id).await;
    }

    #[tokio::test]
    async fn resume_binding_persists_project_context_for_restart() {
        // F5: a resumed session's project context is restored from the persisted
        // binding, not re-detected. So the binding must carry `project_id` /
        // `is_linked_worktree` captured from the live session — verified here by
        // round-tripping through the store (record on native-id capture, read back
        // via load_resume), which is exactly what `load_and_resume` reads at start.
        let store = temp_store_path("resume-project-ctx");
        let worktree_root = store.parent().expect("store parent").join("worktrees");
        let agents_dir = temp_resumable_agents_dir("resume-project-ctx");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            store_path: Some(store),
            worktree_root: Some(worktree_root),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });
        let repo = init_git_repo("resume-project-ctx");
        let info = registry
            .create(SessionNewParams {
                cwd: Some(repo.clone()),
                ..resumable_params()
            })
            .await
            .expect("in-place session in the repo");
        let project_id = info.project_id.clone().expect("a project was stamped");
        assert_eq!(info.is_linked_worktree, Some(false), "the main checkout");

        // Capturing the native id persists the resume binding from live state.
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: info.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: "native-resume".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(recorded.recorded, "native id captured");

        let bindings = registry
            .projects()
            .expect("projects")
            .store()
            .load_resume()
            .expect("load resume bindings");
        assert_eq!(bindings.len(), 1, "exactly one resume binding persisted");
        assert_eq!(
            bindings[0].project_id.as_deref(),
            Some(project_id.as_str()),
            "project id is persisted so restart restores it without re-detecting"
        );
        assert_eq!(
            bindings[0].is_linked_worktree,
            Some(false),
            "the main-checkout flag is persisted too"
        );
    }

    #[tokio::test]
    async fn concurrent_resize_and_recapture_keep_store_consistent_with_memory() {
        let store_path = temp_store_path("concurrent-persist");
        let agents_dir = temp_resumable_agents_dir("concurrent-persist");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(resumable_params())
            .await
            .expect("create session");
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
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
                        agent: "claude".to_owned(),
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
        let agents_dir = temp_resumable_agents_dir("resize-then-stop");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(resumable_params())
            .await
            .expect("create session");
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
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
                agent: "claude".to_owned(),
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
                agent: "shell".to_owned(),
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
                agent: "shell".to_owned(),
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
