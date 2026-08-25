//! Logical session registry and durable-worker supervisor.
//!
//! Production sessions delegate PTY ownership to per-session workers. The
//! daemon retains logical metadata and reconstructible semantic observers.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use protocol::{
    event, AgentActivity, AgentKind, AgentStateEvent, AttachEvent, CwdSource, ErrorClass, Event,
    ProjectRemoveResult, ProtocolError, RuntimeInventoryEntry, RuntimeInventoryResult,
    RuntimeState, SessionAttachParams, SessionEvent, SessionForkParams, SessionId, SessionInfo,
    SessionInputParams, SessionInputResult, SessionNativeRecoveredEvent, SessionNewParams,
    SessionReleaseAgentParams, SessionReleaseAgentResult, SessionRemoveResult,
    SessionReportAgentParams, SessionReportAgentResult, SessionReportNativeIdParams,
    SessionReportNativeIdResult, SessionRuntime, SessionSetMetadataResult, SessionState,
    SessionStopResult, SessionWarning, StateSource, WorktreeRemoveResult, PROTOCOL_VERSION,
};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, watch, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use ulid::Ulid;

use crate::agent::{
    adapter_for, agent_fork_unsupported, agent_not_resumable, base_resume_template,
    build_pty_command, default_args, default_program, fork_pty_command_from_template,
    launch_adapter_for, resume_pty_command_from_template, AgentAdapter, ForkTemplate, InputRules,
    LaunchCommand, LaunchOpts, ProfileRegistry, ResolvedAgent, ResumeTemplate, SessionRef,
    SessionRefKind, ValidatedLaunchProgram,
};
use crate::detect::{identify_agent, ActivityTransition, Detector, DetectorConfig, Manifest};
use crate::external::{
    external_session_id, ExternalSessionChange, ExternalSessions, TranscriptCandidate,
    TranscriptIndex, EXTERNAL_TERMINAL_COLS, EXTERNAL_TERMINAL_ROWS,
};
use crate::integration::{
    ENV_DAEMON_ID, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
};
use crate::procwatch::{ExitWatch, Pid, ProcessFact, ProcessInspector};
use crate::project::detect::{project_id, DetectedProject};
use crate::project::{detect_at, ProjectManager};
use crate::runtime::{
    DimensionUpdate, SystemdWorkerLauncher, Worker, WorkerError, WorkerLaunchMode, WorkerLauncher,
};
use crate::store::{
    DesiredState, ProjectRecord, ResumeBinding, RuntimeRecord, SessionRecord, SessionTransaction,
    SessionWriteOutcome, Store, TransactionKind, WorktreeStatus,
};
use crate::time::now_rfc3339;
use crate::worktree::{
    canonical_or_original, run_hook, HookContext, HookEvent, WorktreeManager, WorktreeRequest,
};

mod attach;
mod detector;
mod diff;
mod hooks;
mod input;
mod lag;
mod observation;
mod procwatch;
mod read;
mod reconcile;
mod resume;
mod target;

pub use attach::{RedeemedAttach, RedeemedRuntime};

pub(crate) use observation::{observation_worker_error, runtime_identity};

use attach::{generate_daemon_instance_id, ActiveAttach, PendingAttach};
use hooks::SessionHookRequest;
#[cfg(test)]
use hooks::{parse_agent_activity, spawn_agent_state_hook_dispatcher};
#[cfg(test)]
use input::build_input_writes;
use input::{input_rules_for_agent, plan_initial_input_delivery};
use lag::{log_lag_warn, LagWarnThrottle};
use resume::ResumeSnapshot;
use target::{build_launch_command, LaunchCommandPlan, PtySessionSpec, TargetResolution};

const DEFAULT_ATTACH_TOKEN_TTL: Duration = Duration::from_secs(10);
/// Time to retain a failed raw attach outcome for one control-plane lookup.
///
/// Clients query the outcome immediately after observing raw EOF. One minute
/// tolerates scheduler stalls without retaining stale stream errors indefinitely.
const DEFAULT_ATTACH_RESULT_TTL: Duration = Duration::from_mins(1);
/// Maximum number of failed raw attach outcomes retained between EOF and detach.
///
/// Attach failures are exceptional and consumed once. This bound prevents a
/// disconnected or malicious client from accumulating daemon memory.
const DEFAULT_ATTACH_RESULT_CAPACITY: usize = 128;
/// Maximum time to wait for a newly activated worker bootstrap socket.
const DEFAULT_WORKER_CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// Initial retry interval while a systemd worker binds its bootstrap socket.
const WORKER_CONNECT_RETRY: Duration = Duration::from_millis(100);
/// Bounds optimistic terminal-state CAS retries before surfacing contention.
const MAX_RUNTIME_TRANSITION_COMMIT_ATTEMPTS: usize = 8;
/// Per-subscriber worker output queue. It absorbs repaint bursts without
/// duplicating the larger raw-history budget for every subscriber.
const DEFAULT_WORKER_SUBSCRIBER_BYTES: u64 = 1_000_000;
/// Number of completed input plans retained by a worker for deduplication.
const DEFAULT_WORKER_WRITE_DEDUP_ENTRIES: u32 = 4_096;
/// Final runtime retention while no daemon is present.
const DEFAULT_WORKER_TERMINAL_RETENTION: Duration = Duration::from_hours(24);
/// Initial durable logical-session record schema.
const SESSION_RECORD_SCHEMA_VERSION: u32 = 1;
/// Bound on how long a graceful shutdown waits for the event-log drain to flush
/// its backlog, so a wedged log write can never hang shutdown.
const EVENT_LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

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
const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_mins(5);

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
/// Default process-observer poll interval.
///
/// Polling is only the discovery and fallback path; pidfd exit watches provide
/// immediate stop detection after a process has been observed. One second keeps
/// launch detection responsive without continuously scanning procfs.
const DEFAULT_PROCWATCH_POLL: Duration = Duration::from_secs(1);
/// Default maximum age for an unbound active-agent hook claim.
///
/// Hooks are rich but lossy claims. Thirty seconds gives procwatch enough time
/// to observe a legitimate live process while ensuring a stale claim cannot pin
/// `active_agent` forever.
const DEFAULT_ACTIVE_AGENT_CLAIM_TTL: Duration = Duration::from_secs(30);

/// Short observation waits bound abandoned dedicated connections.
pub const DEFAULT_OBSERVATION_WAIT: Duration =
    Duration::from_millis(protocol::MAX_SESSION_WAIT_MS as u64);
/// Default maximum number of concurrent bounded waits across the daemon.
pub const DEFAULT_GLOBAL_WAITERS: usize = 128;
/// Default maximum number of concurrent bounded waits for one session.
pub const DEFAULT_SESSION_WAITERS: usize = 8;
/// Default maximum rendered terminal row count accepted by the daemon.
pub const DEFAULT_SCREEN_ROWS: u16 = 200;
/// Default maximum rendered terminal column count accepted by the daemon.
pub const DEFAULT_SCREEN_COLS: u16 = 500;

const MAX_SESSION_METADATA_KEYS: usize = 32;
const MAX_SESSION_METADATA_KEY_BYTES: usize = 64;
const MAX_SESSION_METADATA_VALUE_BYTES: usize = 4096;
const MAX_SESSION_METADATA_SERIALIZED_BYTES: usize = 16 * 1024;

/// Upper bound on a session display name, in bytes. Generous enough for a short
/// human label (it renders in a single GUI row and one CLI table cell) while
/// bounding the per-session state the daemon stores and persists in the resume
/// binding. A name is cosmetic, so the limit can change freely.
const MAX_SESSION_NAME_BYTES: usize = 128;

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
    fn id(&self) -> &'static str {
        "shell"
    }

    fn launch(&self, opts: &LaunchOpts) -> Result<LaunchCommand, ProtocolError> {
        crate::agent::build_pty_command(&self.program, self.args.clone(), opts)
    }

    fn input_rules(&self) -> InputRules {
        InputRules::unrestricted(false, Duration::ZERO)
    }

    fn manifest(&self) -> &crate::detect::Manifest {
        crate::detect::generic_shell_manifest()
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
    /// How long a failed attach outcome remains available to `session.detach`.
    pub attach_result_ttl: Duration,
    /// Maximum failed attach outcomes retained for one-shot retrieval.
    pub attach_result_capacity: usize,
    /// Per-session cap on the raw-output history buffer replayed on attach.
    pub output_history_limit_bytes: usize,
    /// Maximum raw bytes returned by one `session.output` response.
    pub observation_output_bytes: usize,
    /// Maximum bounded wait accepted by `session.output`.
    pub observation_output_wait: Duration,
    /// Maximum bounded wait accepted by `session.wait`.
    pub session_wait: Duration,
    /// Maximum terminal rows accepted in a screen snapshot.
    pub observation_screen_rows: u16,
    /// Maximum terminal columns accepted in a screen snapshot.
    pub observation_screen_cols: u16,
    /// Maximum serialized `session.screen` result size.
    pub observation_screen_bytes: usize,
    /// Maximum concurrent bounded observation waiters.
    pub observation_global_waiters: usize,
    /// Maximum concurrent bounded waiters for one session.
    pub observation_session_waiters: usize,
    /// Delay before sending Claude Code's Ink submit byte as a separate write.
    pub claude_submit_delay: Duration,
    /// Upper bound on how long [`SessionRegistry::create`] waits for a freshly
    /// spawned agent to emit its first PTY output before injecting a
    /// `session.new --input` prompt. The wait short-circuits as soon as the
    /// agent produces any output, so this caps the delay rather than imposing
    /// it; a value of `Duration::ZERO` disables the gate and injects
    /// immediately. Prevents the prompt from being delivered to a TUI that has
    /// not yet entered raw/bracketed-paste input mode.
    pub initial_input_startup_grace: Duration,
    /// Control socket path injected into session PTYs so direct or nested agent
    /// hooks can call home. `None` disables hook-handshake env injection (e.g.
    /// in unit tests that do not exercise the hook).
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
    /// Directory containing bounded structured logs. When set, removing a
    /// stopped session also removes its owner-private worker log family.
    pub log_dir: Option<PathBuf>,
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
    /// Poll interval for per-session process discovery and fallback cleanup.
    pub procwatch_poll: Duration,
    /// Maximum age for an active-agent claim with no backing observed process.
    pub active_agent_claim_ttl: Duration,
    /// Whether to observe same-user agents started outside pohunek-owned PTYs.
    pub observe_external_agents: bool,
    /// Root containing fixed per-session worker sockets.
    ///
    /// Production and tests both require this path; there is no daemon-owned
    /// PTY fallback.
    pub worker_runtime_root: Option<PathBuf>,
    /// Root containing durable per-worker journals.
    pub worker_state_root: Option<PathBuf>,
    /// Bound on worker unit activation and socket negotiation.
    pub worker_connect_deadline: Duration,
    /// Validated systemd template used for worker instance names.
    pub worker_unit_template: crate::runtime::UnitTemplate,
}

impl Default for SessionRegistryConfig {
    fn default() -> Self {
        Self {
            shell_command: ShellCommand::default(),
            stop_grace: Duration::from_millis(500),
            attach_token_ttl: DEFAULT_ATTACH_TOKEN_TTL,
            attach_result_ttl: DEFAULT_ATTACH_RESULT_TTL,
            attach_result_capacity: DEFAULT_ATTACH_RESULT_CAPACITY,
            output_history_limit_bytes: DEFAULT_OUTPUT_HISTORY_LIMIT_BYTES,
            observation_output_bytes: protocol::MAX_SESSION_OUTPUT_BYTES,
            observation_output_wait: DEFAULT_OBSERVATION_WAIT,
            session_wait: DEFAULT_OBSERVATION_WAIT,
            observation_screen_rows: DEFAULT_SCREEN_ROWS,
            observation_screen_cols: DEFAULT_SCREEN_COLS,
            observation_screen_bytes: protocol::MAX_SESSION_SCREEN_RESPONSE_BYTES,
            observation_global_waiters: DEFAULT_GLOBAL_WAITERS,
            observation_session_waiters: DEFAULT_SESSION_WAITERS,
            claude_submit_delay: crate::agent::DEFAULT_CLAUDE_SUBMIT_DELAY,
            initial_input_startup_grace: DEFAULT_INITIAL_INPUT_STARTUP_GRACE,
            socket_path: None,
            store_path: None,
            worktree_root: None,
            hook_timeout: DEFAULT_HOOK_TIMEOUT,
            event_log_dir: None,
            log_dir: None,
            config_dir: None,
            agents_dir: None,
            detector_lag_warn_interval: DEFAULT_DETECTOR_LAG_WARN_INTERVAL,
            procwatch_poll: DEFAULT_PROCWATCH_POLL,
            active_agent_claim_ttl: DEFAULT_ACTIVE_AGENT_CLAIM_TTL,
            observe_external_agents: false,
            worker_runtime_root: None,
            worker_state_root: None,
            worker_connect_deadline: DEFAULT_WORKER_CONNECT_DEADLINE,
            worker_unit_template: crate::runtime::UnitTemplate::default(),
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
    runtime_inventory: Mutex<Vec<RuntimeInventoryEntry>>,
    pending_attaches: Mutex<HashMap<String, PendingAttach>>,
    active_attaches: Mutex<HashMap<String, ActiveAttach>>,
    recent_attach_failures: Mutex<VecDeque<attach::RecentAttachFailure>>,
    next_stream_id: AtomicU64,
    next_write_id: AtomicU64,
    next_resize_sequence: AtomicU64,
    /// Number of active bounded observation waits across all sessions.
    observation_waiters: AtomicUsize,
    /// Per-session active bounded observation waits.
    observation_session_waiters: std::sync::Mutex<HashMap<SessionId, usize>>,
    /// Set when daemon process shutdown starts. Natural PTY exits observed after
    /// this point are treated as restart fallout, not terminal session state.
    daemon_shutdown_started: AtomicBool,
    /// Opaque id unique to this daemon process instance, injected into every
    /// session PTY as `POHUNEK_DAEMON_ID` and compared against the attach origin
    /// so the self-feeding-attach guard fires only for this instance's own PTYs
    /// (see [`SessionRegistry::attach`]). Regenerated each start; never persisted.
    daemon_instance_id: String,
    config: SessionRegistryConfig,
    launcher: Arc<dyn WorkerLauncher>,
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
    /// Serializes explicit native recovery so repeated requests cannot replace
    /// the same runtime generation twice.
    recovery_lock: Mutex<()>,
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
    /// OS process inspector used to reconcile hook claims with live processes.
    inspector: Arc<dyn ProcessInspector>,
    /// Monotonic sequence for procwatch-generated active-agent reports.
    ///
    /// It is seeded from wall-clock milliseconds in the constructor so values live
    /// in the same numeric space as hook timestamps, while same-source ordering
    /// remains monotonic if multiple procwatch events happen within one millisecond.
    procwatch_seq: AtomicU64,
    /// Read-only external agent sessions observed outside pohunek-owned PTYs.
    external: ExternalSessions,
}

#[derive(Debug, Clone)]
struct SessionEntry {
    info: SessionInfo,
    runtime: RuntimeHandle,
    desired_state: DesiredState,
    detector_cancel: CancellationToken,
    detector_resize: watch::Sender<(u16, u16)>,
    detector_config: watch::Sender<DetectorConfig>,
    default_detector_config: DetectorConfig,
    procwatch_cancel: CancellationToken,
    runtime_watch_cancel: CancellationToken,
    procwatch_rescan: Arc<Notify>,
    stopping: bool,
    /// Resolved input-framing rules (base-kind defaults, profile-overridden), used
    /// by `session.input` so a profile's `[input_rules]` is honored on every write.
    input_rules: InputRules,
    /// Frozen structural relaunch snapshot (C.4), set once at register time and
    /// copied verbatim into every persisted [`ResumeBinding`] — so a resize-driven
    /// re-persist can never overwrite the launch-time program/args/resume shape.
    snapshot: ResumeSnapshot,
    active_agent: Option<ActiveAgentReport>,
    last_agent_report: Option<ActiveAgentReport>,
    last_native_report: Option<NativeIdentityReport>,
    observed_agents: Vec<ObservedAgent>,
}

/// Runtime transport selected for one logical session.
#[derive(Debug, Clone)]
enum RuntimeHandle {
    Worker(Worker),
    Unavailable(RuntimeState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveAgentReport {
    source: String,
    agent: String,
    seq: Option<u64>,
    pid: Option<Pid>,
    reported_at: Instant,
    activity_reported: bool,
}

type NativeIdentityReport = crate::store::NativeIdentityOrdering;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedAgent {
    pid: Pid,
    agent_base: AgentKind,
    first_seen: Instant,
    cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeExit {
    exit_code: Option<i32>,
    success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeWatchIdentity {
    worker_id: String,
    runtime_id: String,
    generation: protocol::RuntimeGeneration,
}

impl RuntimeWatchIdentity {
    fn from_info(info: &SessionInfo) -> Option<Self> {
        let runtime = info.runtime.as_ref()?;
        Some(Self {
            worker_id: runtime.worker_id.clone()?,
            runtime_id: runtime.runtime_id.clone()?,
            generation: runtime.runtime_generation,
        })
    }

    fn matches(&self, entry: &SessionEntry) -> bool {
        entry.info.runtime.as_ref().is_some_and(|runtime| {
            runtime.worker_id.as_deref() == Some(self.worker_id.as_str())
                && runtime.runtime_id.as_deref() == Some(self.runtime_id.as_str())
                && runtime.runtime_generation == self.generation
        })
    }
}

#[derive(Debug)]
enum RuntimeTransitionOutcome {
    Applied(Box<SessionInfo>),
    IdentityMismatch,
    RetryablePersistenceFailure(ProtocolError),
    RetryableConcurrentChange,
}

struct ExitTransition {
    event: &'static str,
    stop_reason: &'static str,
    detector_cancel: CancellationToken,
    procwatch_cancel: CancellationToken,
    expected: RuntimeWatchIdentity,
    base: SessionRecord,
    candidate: SessionEntry,
}

fn exit_transition(
    id: &SessionId,
    entry: &SessionEntry,
    expected: RuntimeWatchIdentity,
    exit: RuntimeExit,
    stopped_by_user: bool,
) -> ExitTransition {
    let base = SessionRegistry::session_record(id, entry, entry.desired_state, None);
    let mut candidate = entry.clone();
    let stopped =
        stopped_by_user || candidate.stopping || candidate.info.state == SessionState::Stopped;
    candidate.stopping = false;
    let stop_reason = if stopped {
        candidate.info.state = SessionState::Stopped;
        "stopped"
    } else if exit.success {
        candidate.info.state = SessionState::Done;
        "done"
    } else {
        candidate.info.state = SessionState::Failed;
        "failed"
    };
    candidate.info.state_source = StateSource::Process;
    candidate.info.activity = None;
    candidate.active_agent = None;
    candidate.last_agent_report = None;
    candidate.info.active_agent = None;
    candidate.info.active_agent_base = None;
    candidate.info.active_agent_pid = None;
    candidate.info.active_agent_session_id = None;
    candidate.info.active_agent_session_path = None;
    candidate.observed_agents.clear();
    candidate.info.exit_code = exit.exit_code;
    if let Some(runtime) = candidate.info.runtime.as_mut() {
        runtime.state = RuntimeState::Terminal;
        runtime.loss_reason = None;
    }
    candidate.info.updated_at = timestamp_now();
    ExitTransition {
        event: if stopped {
            event::SESSION_STOPPED
        } else {
            event::SESSION_UPDATED
        },
        stop_reason,
        detector_cancel: candidate.detector_cancel.clone(),
        procwatch_cancel: candidate.procwatch_cancel.clone(),
        expected,
        base,
        candidate,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CwdAssociation {
    project_id: Option<String>,
    is_linked_worktree: Option<bool>,
    repo: Option<PathBuf>,
    branch: Option<String>,
    worktree_path: Option<PathBuf>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new(SessionRegistryConfig::default())
    }
}

impl SessionRegistry {
    /// Reports whether inherited origin markers target this daemon's same session.
    #[must_use]
    pub(crate) fn is_origin_session(
        &self,
        origin_session_id: Option<&SessionId>,
        origin_daemon_id: Option<&str>,
        target: &str,
    ) -> bool {
        origin_session_id.is_some_and(|origin| origin.0 == target)
            && origin_daemon_id == Some(self.inner.daemon_instance_id.as_str())
    }

    /// Returns the latest fail-closed durable-worker discovery inventory.
    pub async fn runtime_inventory(&self) -> RuntimeInventoryResult {
        RuntimeInventoryResult {
            entries: self.inner.runtime_inventory.lock().await.clone(),
        }
    }

    async fn write_session_record(&self, record: SessionRecord) -> Result<(), ProtocolError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(());
        };
        let session_id = record.session_id.clone();
        tokio::task::spawn_blocking(move || store.record_session(&record))
            .await
            .map_err(|_join_error| {
                runtime_error(
                    "session_store_failed",
                    format!("session record write task panicked for {session_id}"),
                )
            })?
            .map_err(|error| {
                runtime_error(
                    "session_store_failed",
                    format!("failed to write session record {session_id}: {error}"),
                )
            })
            .and_then(|outcome| match outcome {
                SessionWriteOutcome::Applied => Ok(()),
                SessionWriteOutcome::AppliedDurabilityUncertain { error } => {
                    warn!(
                        session_id,
                        durability_error = %error,
                        "session record commit is visible but directory durability is uncertain"
                    );
                    Ok(())
                }
                SessionWriteOutcome::StaleRuntime => Err(runtime_error(
                    "session_runtime_commit_stale",
                    format!("session record {session_id} was superseded by another runtime commit"),
                )),
                SessionWriteOutcome::StaleSnapshot => Err(runtime_error(
                    "session_record_commit_stale",
                    format!("session record {session_id} changed before its conditional commit"),
                )),
            })
    }

    async fn write_session_record_if_current(
        &self,
        expected: SessionRecord,
        record: SessionRecord,
    ) -> Result<(), ProtocolError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(());
        };
        let session_id = record.session_id.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            store.record_session_if_current(&expected, &record)
        })
        .await
        .map_err(|_join_error| {
            runtime_error(
                "session_store_failed",
                format!("conditional session write task panicked for {session_id}"),
            )
        })?
        .map_err(|error| {
            runtime_error(
                "session_store_failed",
                format!("failed to conditionally write session record {session_id}: {error}"),
            )
        })?;
        match outcome {
            SessionWriteOutcome::Applied => Ok(()),
            SessionWriteOutcome::AppliedDurabilityUncertain { error } => {
                warn!(
                    session_id,
                    durability_error = %error,
                    "conditional session commit is visible but directory durability is uncertain"
                );
                Ok(())
            }
            SessionWriteOutcome::StaleRuntime => Err(runtime_error(
                "session_runtime_commit_stale",
                format!("session record {session_id} was superseded by another runtime commit"),
            )),
            SessionWriteOutcome::StaleSnapshot => Err(runtime_error(
                "session_record_commit_stale",
                format!("session record {session_id} changed before its conditional commit"),
            )),
        }
    }

    async fn load_durable_session_record(
        &self,
        id: &SessionId,
    ) -> Result<Option<SessionRecord>, ProtocolError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(None);
        };
        let session_id = id.0.clone();
        tokio::task::spawn_blocking(move || {
            store.load_sessions().map(|sessions| {
                sessions
                    .into_iter()
                    .find(|record| record.session_id == session_id)
            })
        })
        .await
        .map_err(|_join_error| {
            runtime_error(
                "session_store_failed",
                format!("session record read task panicked for {}", id.0),
            )
        })?
        .map_err(|error| {
            runtime_error(
                "session_store_failed",
                format!("failed to read session record {}: {error}", id.0),
            )
        })
    }

    async fn delete_session_record(&self, id: &SessionId) -> Result<(), ProtocolError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(());
        };
        let session_id = id.0.clone();
        tokio::task::spawn_blocking(move || store.remove_session(&session_id))
            .await
            .map_err(|_join_error| {
                runtime_error(
                    "session_store_failed",
                    format!("session record removal task panicked for {}", id.0),
                )
            })?
            .map(|_| ())
            .map_err(|error| {
                runtime_error(
                    "session_store_failed",
                    format!("failed to remove session record {}: {error}", id.0),
                )
            })
    }

    async fn delete_session_logs(&self, id: &SessionId) -> Result<(), ProtocolError> {
        let Some(log_dir) = self.inner.config.log_dir.clone() else {
            return Ok(());
        };
        let session_id = id.0.clone();
        tokio::task::spawn_blocking(move || {
            let files = pohunek_logging::config::worker_files(&session_id)?;
            pohunek_logging::remove_family(&log_dir, &files)
        })
        .await
        .map_err(|_join_error| {
            runtime_error(
                "session_log_cleanup_failed",
                format!("session log cleanup task panicked for {}", id.0),
            )
        })?
        .map_err(|error| {
            runtime_error(
                "session_log_cleanup_failed",
                format!("failed to clean worker logs for {}: {error}", id.0),
            )
        })
    }

    async fn cleanup_owned_worktrees_for_removal(
        &self,
        id: &SessionId,
    ) -> Result<Vec<SessionWarning>, ProtocolError> {
        let Some(worktree) = self.inner.worktree.clone() else {
            return Ok(Vec::new());
        };
        let session_id = id.0.clone();
        tokio::task::spawn_blocking(move || {
            let mut warnings = Vec::new();
            worktree.cleanup_session(&session_id, &mut warnings)?;
            Ok(warnings)
        })
        .await
        .map_err(|_join_error| {
            runtime_error(
                "worktree_cleanup_failed",
                format!("worktree cleanup task panicked for {}", id.0),
            )
        })?
    }

    fn session_record(
        id: &SessionId,
        entry: &SessionEntry,
        desired_state: DesiredState,
        transaction: Option<SessionTransaction>,
    ) -> SessionRecord {
        let runtime = entry.info.runtime.as_ref().map_or(
            RuntimeRecord {
                state: RuntimeState::Live,
                worker_id: None,
                runtime_id: None,
                unit_name: None,
                reason: None,
            },
            |runtime| RuntimeRecord {
                state: runtime.state,
                worker_id: runtime.worker_id.clone(),
                runtime_id: runtime.runtime_id.clone(),
                unit_name: Some(format!("pohunek-session@{}.service", id.0)),
                reason: runtime.loss_reason.clone(),
            },
        );
        SessionRecord {
            schema_version: SESSION_RECORD_SCHEMA_VERSION,
            session_id: id.0.clone(),
            desired_state,
            transaction,
            info: entry.info.clone(),
            recovery: Some(Self::resume_binding_from_entry(id, entry)),
            native_identity_ordering: entry.last_native_report.clone(),
            runtime,
        }
    }

    /// Create a registry.
    ///
    /// Production callers must use [`Self::new_production`]. Unit tests inject
    /// the real worker server through a test-only launcher; no constructor
    /// falls back to daemon-owned PTYs.
    #[must_use]
    pub fn new(config: SessionRegistryConfig) -> Self {
        #[cfg(test)]
        {
            let mut config = config;
            let (runtime_root, state_root) = test_worker_roots(&config);
            config.worker_runtime_root = Some(runtime_root.clone());
            config.worker_state_root = Some(state_root.clone());
            let launcher = Arc::new(crate::runtime::InProcessWorkerLauncher::new(
                runtime_root,
                state_root,
            ));
            Self::new_with_launcher_and_inspector(
                config,
                launcher,
                Arc::new(crate::procwatch::LinuxInspector::new()),
            )
        }
        #[cfg(not(test))]
        {
            let launcher = Arc::new(SystemdWorkerLauncher::new(
                config.worker_unit_template.clone(),
            ));
            Self::new_with_launcher_and_inspector(
                config,
                launcher,
                Arc::new(crate::procwatch::LinuxInspector::new()),
            )
        }
    }

    /// Create a production registry with a mandatory durable-worker backend.
    ///
    /// # Errors
    ///
    /// Returns `worker_backend_required` when the per-session worker runtime
    /// root is absent. Production never falls back to daemon-owned PTYs.
    pub fn new_production(config: SessionRegistryConfig) -> Result<Self, ProtocolError> {
        validate_observation_config(&config)?;
        if config.worker_runtime_root.is_none() || config.worker_state_root.is_none() {
            return Err(runtime_error(
                "worker_backend_required",
                "production session registry requires durable worker runtime and state roots",
            ));
        }
        let launcher = Arc::new(SystemdWorkerLauncher::new(
            config.worker_unit_template.clone(),
        ));
        Ok(Self::new_with_launcher_and_inspector(
            config,
            launcher,
            Arc::new(crate::procwatch::LinuxInspector::new()),
        ))
    }

    /// Create a registry with an injected process inspector.
    ///
    /// Production uses [`crate::procwatch::LinuxInspector`]. Tests use this to
    /// drive process facts and exit events deterministically without touching the
    /// host process table.
    #[must_use]
    pub fn new_with_inspector(
        config: SessionRegistryConfig,
        inspector: Arc<dyn ProcessInspector>,
    ) -> Self {
        #[cfg(test)]
        {
            let mut config = config;
            let (runtime_root, state_root) = test_worker_roots(&config);
            config.worker_runtime_root = Some(runtime_root.clone());
            config.worker_state_root = Some(state_root.clone());
            let launcher = Arc::new(crate::runtime::InProcessWorkerLauncher::new(
                runtime_root,
                state_root,
            ));
            Self::new_with_launcher_and_inspector(config, launcher, inspector)
        }
        #[cfg(not(test))]
        {
            let launcher = Arc::new(SystemdWorkerLauncher::new(
                config.worker_unit_template.clone(),
            ));
            Self::new_with_launcher_and_inspector(config, launcher, inspector)
        }
    }

    /// Creates a registry with explicit worker and process-observer backends.
    ///
    /// Integration tests use this surface with a separate-process worker
    /// launcher. Production uses [`Self::new_production`].
    #[must_use]
    pub fn new_with_launcher_and_inspector(
        config: SessionRegistryConfig,
        launcher: Arc<dyn WorkerLauncher>,
        inspector: Arc<dyn ProcessInspector>,
    ) -> Self {
        let external = ExternalSessions::new();
        let external_observer = config
            .observe_external_agents
            .then(|| ExternalSessions::observer_config(config.procwatch_poll));
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
                Arc::clone(store),
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
        let registry = Self {
            inner: Arc::new(SessionRegistryInner {
                sessions: Mutex::new(HashMap::new()),
                runtime_inventory: Mutex::new(Vec::new()),
                pending_attaches: Mutex::new(HashMap::new()),
                active_attaches: Mutex::new(HashMap::new()),
                recent_attach_failures: Mutex::new(VecDeque::new()),
                next_stream_id: AtomicU64::new(1),
                next_write_id: AtomicU64::new(1),
                next_resize_sequence: AtomicU64::new(1),
                observation_waiters: AtomicUsize::new(0),
                observation_session_waiters: std::sync::Mutex::new(HashMap::new()),
                daemon_shutdown_started: AtomicBool::new(false),
                daemon_instance_id: generate_daemon_instance_id(),
                config,
                launcher,
                profiles,
                events,
                store,
                persist_lock: Mutex::new(()),
                recovery_lock: Mutex::new(()),
                worktree,
                projects,
                event_log_shutdown: CancellationToken::new(),
                event_log_task: std::sync::Mutex::new(None),
                agent_state_hook_shutdown: CancellationToken::new(),
                agent_state_hook_task: std::sync::Mutex::new(None),
                inspector,
                procwatch_seq: AtomicU64::new(current_time_millis()),
                external: external.clone(),
            }),
        };
        if let Some(config) = external_observer {
            external.spawn_observer(registry.clone(), config);
        }
        registry
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

    /// This daemon process instance's opaque controller id.
    ///
    /// Durable workers use it for controller leases and expose it for
    /// diagnostics. The self-feeding attach guard uses the stable worker id,
    /// because daemon instance ids intentionally change across restarts.
    #[must_use]
    pub fn daemon_instance_id(&self) -> &str {
        &self.inner.daemon_instance_id
    }

    fn allocate_session_id() -> SessionId {
        // A ULID keeps identifiers time-sortable while its 80 random bits avoid
        // reusing a retained durable worker slot after metadata removal.
        SessionId(format!("s-{}", Ulid::new()))
    }

    /// Mark that the daemon process is shutting down.
    ///
    /// Production worker runtimes remain alive and are reconciled by the next
    /// daemon. The workerless test harness can still observe synthetic PTY exits
    /// while its daemon shuts down; those observations must not rewrite logical
    /// lifecycle state.
    pub fn begin_daemon_shutdown(&self) {
        let already_started = self
            .inner
            .daemon_shutdown_started
            .swap(true, Ordering::Relaxed);
        if !already_started {
            info!("daemon shutdown started; preserving durable worker runtimes");
        }
        self.inner.external.shutdown();
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
    #[expect(
        clippy::map_err_ignore,
        reason = "spawn_blocking JoinError has no meaningful source to surface in ProtocolError"
    )]
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
            self.list_raw()
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
            let (pruned_count, skipped_worktrees) = if prune_worktrees {
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
                pruned_worktrees: pruned_count,
                skipped_worktrees,
            })
        })
        .await
        .map_err(|_| runtime_error("project_remove_failed", "project remove task panicked"))?
    }

    /// Remove a single pohunek-owned worktree by path, dropping its binding.
    ///
    /// Fail-closed in two ways: a worktree a non-terminal (`Starting`/`Running`)
    /// session still uses is refused (`worktree_in_use`) so its checkout is not
    /// pulled out from under a live session; a path with no matching binding is
    /// an external worktree pohunek never created and is refused
    /// (`worktree_not_owned`) rather than touched.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] when worktrees are not configured, the
    /// worktree is in use, the worktree is not owned, the binding store fails, or
    /// the blocking task panics.
    pub async fn remove_worktree(
        &self,
        path: &Path,
    ) -> Result<WorktreeRemoveResult, ProtocolError> {
        let Some(manager) = self.inner.worktree.clone() else {
            return Err(runtime_error(
                "worktrees_not_configured",
                "the daemon is not configured for worktrees",
            ));
        };
        let target = canonical_or_original(path);
        // Refuse to remove a worktree a live (non-terminal) session still uses —
        // matching the prune's live-session skip, but surfaced as a hard error
        // for a targeted single-worktree removal instead of a silent skip.
        let in_use = self
            .list_raw()
            .await
            .into_iter()
            .filter(|session| !session.state.is_terminal())
            .filter_map(|session| session.worktree_path)
            .any(|worktree_path| canonical_or_original(&worktree_path) == target);
        if in_use {
            return Err(runtime_error(
                "worktree_in_use",
                "a live session is using this worktree; stop the session before removing it",
            ));
        }
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || -> Result<WorktreeRemoveResult, ProtocolError> {
            let mut warnings = Vec::new();
            let removed = manager.remove_one(&path, &mut warnings)?;
            if !removed {
                return Err(runtime_error(
                    "worktree_not_owned",
                    "pohunek did not create this worktree; remove it manually with git",
                ));
            }
            for warning in &warnings {
                warn!(
                    warning = %warning.message,
                    detail = ?warning.detail,
                    "remove hook warning during worktree remove"
                );
            }
            Ok(WorktreeRemoveResult { removed: true })
        })
        .await
        .map_err(|err| {
            runtime_error(
                "worktree_remove_failed",
                format!("worktree remove task panicked: {err}"),
            )
        })?
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
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    /// Resolves the session's **target** (design Decisions 1 & 3): which project
    /// it belongs to (a `--project` reference, else auto-detected from `--repo`/
    /// `--cwd`) and where the agent runs — **in-place** in the project's checkout
    /// (no `--branch`) or in a **dedicated worktree** bound for `(session, repo,
    /// branch)` (with `--branch`). A non-git directory yields a plain shell with
    /// no project. The session records `project_id`/`is_linked_worktree`, and any
    /// non-fatal worktree warnings ride along on the returned [`SessionInfo`].
    #[expect(
        clippy::too_many_lines,
        reason = "tracked for session module decomposition"
    )]
    pub async fn create(&self, params: SessionNewParams) -> Result<SessionInfo, ProtocolError> {
        validate_new_params(&params)?;
        let initial_input = params.input.clone();
        // Resolve and validate the runtime before allocating a logical id or
        // resolving a target: target resolution may bind a git worktree.
        let resolved = self.inner.profiles.resolve_agent(&params.agent)?;
        let base = resolved.base.clone();
        let configured_program = resolved
            .profile
            .as_ref()
            .map_or_else(|| default_program(&base), |profile| profile.program.clone());
        let validated_program =
            crate::capabilities::validate_launch_runtime(&base, &configured_program)?;
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

        let id = Self::allocate_session_id();

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
            let capabilities = resolved.capabilities();
            let input_rules = resolved
                .profile
                .as_ref()
                .and_then(|profile| profile.input_rules)
                .unwrap_or_else(|| input_rules_for_agent(&base, &self.inner.config));
            // Freeze the structural relaunch snapshot (C.4) from the resolved agent:
            // the launch program/args plus the resume template (a profile's override,
            // else the base kind's). Cloned/copied so `resolved` stays usable below.
            let snapshot = ResumeSnapshot {
                program: resolved
                    .profile
                    .as_ref()
                    .map_or_else(|| default_program(&base), |profile| profile.program.clone()),
                args: resolved
                    .profile
                    .as_ref()
                    .map_or_else(|| default_args(&base), |profile| profile.args.clone()),
                resume: capabilities.resume,
                fork: capabilities.fork,
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
            env_extra.extend(self.session_pty_env(base.clone(), &id));
            let opts = LaunchOpts {
                cwd: launch_cwd.clone(),
                cols: params.cols,
                rows: params.rows,
                env_extra,
                validated_program,
            };
            let plan = build_launch_command(
                &resolved,
                &self.inner.config.shell_command,
                &opts,
                initial_input.clone(),
            )?;

            let info = self
                .register_pty_session(PtySessionSpec {
                    id: id.clone(),
                    registration: target::PtyRegistration::Create,
                    name: validate_session_name(params.name.as_deref())?,
                    agent: resolved.name.clone(),
                    agent_base: base.clone(),
                    input_rules,
                    snapshot,
                    manifest_override,
                    cwd: launch_cwd,
                    cols: params.cols,
                    rows: params.rows,
                    command: plan.command,
                    native_session_id: None,
                    native_session_path: None,
                    project_id,
                    is_linked_worktree,
                    repo,
                    branch,
                    worktree_path,
                    metadata: params.metadata.clone(),
                    warnings,
                })
                .await?;

            Ok((info, plan.pending_initial_input))
        }
        .await;

        if launch.is_err() && worktree_bound {
            self.cleanup_bound_worktree(&id).await;
        }
        let (info, pending_initial_input) = launch?;
        self.spawn_session_hook(SessionHookRequest {
            event: HookEvent::SessionStart,
            cwd: info.cwd.clone(),
            session_id: info.id.0.clone(),
            project_id: info.project_id.clone(),
            agent: info.agent.clone(),
            stop_reason: None,
            activity: None,
        });
        if let Some(input) = pending_initial_input {
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
        let runtime = {
            let sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get(session_id) else {
                return;
            };
            entry.runtime.clone()
        };

        match runtime {
            RuntimeHandle::Worker(worker) => {
                let _ = tokio::time::timeout(grace, async {
                    loop {
                        match worker.inspect().await {
                            Ok(snapshot) if snapshot.next_offset > 0 => break,
                            Ok(_) => tokio::time::sleep(WORKER_CONNECT_RETRY).await,
                            Err(_) => break,
                        }
                    }
                })
                .await;
            }
            RuntimeHandle::Unavailable(_) => {}
        }
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

    /// Record an agent's native session id for resume or fork recovery.
    ///
    /// Called from the `session.report_native_id` handler when a `SessionStart`
    /// hook fires. Validates the native id, updates the in-memory session info
    /// (so `inspect`/`list` show it), and persists native recovery metadata.
    /// Reports for an unknown or already-terminal session are ignored, not
    /// errors (the hook fires-and-forgets).
    #[expect(
        clippy::too_many_lines,
        reason = "ordered identity validation is kept linear so every rejection precedes persistence"
    )]
    pub async fn report_native_id(
        &self,
        params: SessionReportNativeIdParams,
    ) -> SessionReportNativeIdResult {
        let not_recorded = SessionReportNativeIdResult { recorded: false };
        let session_id = params.session_id().clone();
        if !identity_claim_expiry_is_valid(params.expires_at()) {
            debug!(session_id = %session_id.0, "expired or overlong native-id report; ignoring");
            return not_recorded;
        }
        let (worker, expected_agent, expected_base, ref_kind) = {
            let sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get(&session_id) else {
                debug!(session_id = %session_id.0, "native-id report for an unknown session; ignoring");
                return not_recorded;
            };
            if is_terminal(entry.info.state) {
                debug!(session_id = %session_id.0, "native-id report for a terminal session; ignoring");
                return not_recorded;
            }
            let RuntimeHandle::Worker(worker) = &entry.runtime else {
                debug!(session_id = %session_id.0, "native-id report for an unavailable runtime; ignoring");
                return not_recorded;
            };
            let Some(ref_kind) = entry.snapshot.native_ref_kind() else {
                debug!(session_id = %session_id.0, "native-id report for a session without recovery support; ignoring");
                return not_recorded;
            };
            (
                worker.clone(),
                entry.info.agent.clone(),
                agent_kind_label(&entry.info.agent_base).to_owned(),
                ref_kind,
            )
        };
        if params.agent() != expected_agent && params.agent() != expected_base {
            debug!(session_id = %session_id.0, "native-id report provider mismatch; ignoring");
            return not_recorded;
        }
        let worker_snapshot = match worker.inspect().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                debug!(session_id = %session_id.0, error = %error, "native-id runtime validation failed");
                return not_recorded;
            }
        };
        let runtime_matches = worker_snapshot.session_id.as_str() == session_id.0
            && worker_snapshot
                .runtime_id
                .as_ref()
                .is_some_and(|runtime| runtime.as_str() == params.runtime_id());
        let process_matches = worker_snapshot
            .child_process
            .as_ref()
            .is_some_and(|process| {
                process.pid == params.pid()
                    && process.start_identity == params.pid_start_identity().get()
            });
        if !runtime_matches || !process_matches {
            debug!(session_id = %session_id.0, "native-id runtime or process identity mismatch; ignoring");
            return not_recorded;
        }

        let validated = match ref_kind {
            SessionRefKind::Id => SessionRef::id(params.native_session_id()),
            SessionRefKind::Path => params
                .transcript_path()
                .ok_or_else(|| {
                    runtime_error(
                        "native_reference_missing",
                        "path recovery report omitted its path",
                    )
                })
                .and_then(SessionRef::path),
        };
        let session_ref = match validated {
            Ok(session_ref) => session_ref,
            Err(error) => {
                debug!(session_id = %session_id.0, error = %error, "invalid native-id reference; ignoring");
                return not_recorded;
            }
        };

        let (info, record, previous_ordering, previous_native_id, previous_native_path) = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(&session_id) else {
                return not_recorded;
            };
            let current_runtime_matches = entry
                .info
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.runtime_id.as_deref())
                == Some(params.runtime_id());
            if is_terminal(entry.info.state) || !current_runtime_matches {
                return not_recorded;
            }
            let incoming_sequence = params.sequence().get();
            if !native_report_is_current(
                entry.last_native_report.as_ref(),
                params.runtime_id(),
                incoming_sequence,
            ) {
                debug!(session_id = %session_id.0, "stale native-id report; ignoring");
                return not_recorded;
            }
            let previous_ordering = entry.last_native_report.clone();
            let previous_native_id = entry.info.native_session_id.clone();
            let previous_native_path = entry.info.native_session_path.clone();
            entry.last_native_report = Some(NativeIdentityReport {
                runtime_id: params.runtime_id().to_owned(),
                pid: params.pid(),
                pid_start_identity: params.pid_start_identity().get(),
                sequence: incoming_sequence,
            });
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
            (
                entry.info.clone(),
                Self::session_record(&session_id, entry, entry.desired_state, None),
                previous_ordering,
                previous_native_id,
                previous_native_path,
            )
        };
        if let Err(error) = self.write_session_record(record).await {
            debug!(session_id = %session_id.0, error = %error, "failed to persist native-id ordering; ignoring report");
            let mut sessions = self.inner.sessions.lock().await;
            if let Some(entry) = sessions.get_mut(&session_id) {
                let accepted = NativeIdentityReport {
                    runtime_id: params.runtime_id().to_owned(),
                    pid: params.pid(),
                    pid_start_identity: params.pid_start_identity().get(),
                    sequence: params.sequence().get(),
                };
                if entry.last_native_report.as_ref() == Some(&accepted) {
                    entry.last_native_report = previous_ordering;
                    entry.info.native_session_id = previous_native_id;
                    entry.info.native_session_path = previous_native_path;
                }
            }
            return not_recorded;
        }
        // Persist from the now-current in-memory state (a resize that landed
        // first is reflected, not clobbered).
        self.persist_resume_binding(&session_id).await;
        self.emit(event::SESSION_UPDATED, &info);
        SessionReportNativeIdResult { recorded: true }
    }

    /// Record the active nested agent currently owning a live session.
    pub async fn report_agent(&self, params: SessionReportAgentParams) -> SessionReportAgentResult {
        let not_recorded = SessionReportAgentResult { recorded: false };
        let resolved = match self.inner.profiles.resolve_agent(&params.agent) {
            Ok(resolved) => resolved,
            Err(err) => {
                debug!(
                    session_id = %params.session_id.0,
                    agent = %params.agent,
                    error = %err,
                    "active-agent report for an unknown agent; ignoring"
                );
                return not_recorded;
            }
        };

        let valid_session_id =
            validate_agent_session_id(&params.session_id, params.agent_session_id.as_deref());
        let valid_session_path =
            validate_agent_session_path(&params.session_id, params.agent_session_path.as_deref());
        let reported_activity = params.activity;
        let report_sequence = params.seq.map(protocol::ReportSequence::get);
        let active_detector_config = detector_config_for_resolved_agent(&resolved);
        let (info, rescan) = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(&params.session_id) else {
                debug!(
                    session_id = %params.session_id.0,
                    "active-agent report for an unknown session; ignoring"
                );
                return not_recorded;
            };
            if is_terminal(entry.info.state) {
                debug!(
                    session_id = %params.session_id.0,
                    "active-agent report for a terminal session; ignoring"
                );
                return not_recorded;
            }
            if !report_is_current(
                entry.last_agent_report.as_ref(),
                &params.source,
                &resolved.name,
                report_sequence,
            ) {
                debug!(
                    session_id = %params.session_id.0,
                    source = %params.source,
                    agent = %resolved.name,
                    seq = ?params.seq,
                    "stale active-agent report; ignoring"
                );
                return not_recorded;
            }

            let pid = bind_report_pid(entry, params.pid, &resolved.base);
            let report = ActiveAgentReport {
                source: params.source.clone(),
                agent: resolved.name.clone(),
                seq: report_sequence,
                pid,
                reported_at: Instant::now(),
                activity_reported: reported_activity.is_some(),
            };
            entry.active_agent = Some(report.clone());
            entry.last_agent_report = Some(report);
            entry.info.active_agent = Some(resolved.name.clone());
            entry.info.active_agent_base = Some(resolved.base);
            entry.info.active_agent_pid = pid;
            entry.info.active_agent_session_id = valid_session_id;
            entry.info.active_agent_session_path = valid_session_path;
            if let Some(activity) = reported_activity {
                entry.info.activity = Some(activity);
                entry.info.state_source = StateSource::Report;
            }
            let _ = entry.detector_config.send(active_detector_config);
            entry.info.updated_at = timestamp_now();
            (entry.info.clone(), Arc::clone(&entry.procwatch_rescan))
        };

        self.emit(event::SESSION_UPDATED, &info);
        rescan.notify_one();
        if let Some(activity) = reported_activity {
            let event = crate::events::event(
                event::AGENT_STATE,
                event_payload(AgentStateEvent {
                    session_id: params.session_id.clone(),
                    activity,
                    source: StateSource::Report,
                }),
            );
            let _ = self.inner.events.send(event);
        }
        SessionReportAgentResult { recorded: true }
    }

    /// Release the active nested agent currently owning a live session.
    pub async fn release_agent(
        &self,
        params: SessionReleaseAgentParams,
    ) -> SessionReleaseAgentResult {
        let not_released = SessionReleaseAgentResult { released: false };
        let report_sequence = params.seq.map(protocol::ReportSequence::get);
        let resolved = match self.inner.profiles.resolve_agent(&params.agent) {
            Ok(resolved) => resolved,
            Err(err) => {
                debug!(
                    session_id = %params.session_id.0,
                    agent = %params.agent,
                    error = %err,
                    "active-agent release for an unknown agent; ignoring"
                );
                return not_released;
            }
        };

        let info = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(&params.session_id) else {
                debug!(
                    session_id = %params.session_id.0,
                    "active-agent release for an unknown session; ignoring"
                );
                return not_released;
            };
            if is_terminal(entry.info.state) {
                debug!(
                    session_id = %params.session_id.0,
                    "active-agent release for a terminal session; ignoring"
                );
                return not_released;
            }
            let Some(active) = entry.active_agent.as_ref() else {
                return not_released;
            };
            if !release_matches(active, &params.source, &resolved.name, report_sequence) {
                debug!(
                    session_id = %params.session_id.0,
                    source = %params.source,
                    agent = %resolved.name,
                    seq = ?params.seq,
                    "active-agent release did not match current report; ignoring"
                );
                return not_released;
            }
            let tombstone = ActiveAgentReport {
                source: params.source.clone(),
                agent: resolved.name.clone(),
                seq: report_sequence,
                pid: None,
                reported_at: Instant::now(),
                activity_reported: false,
            };
            clear_active_agent(entry, tombstone)
        };

        self.emit(event::SESSION_UPDATED, &info);
        SessionReleaseAgentResult { released: true }
    }

    /// List all known sessions, with each session's `project_label` enriched from
    /// the project store (so the switcher and `session list` show the project by
    /// name, and `--filter project=<label>` resolves). Enrichment is best-effort:
    /// a missing store or read error simply leaves labels unset.
    pub async fn list(&self) -> Vec<SessionInfo> {
        let mut sessions = self.list_raw().await;
        self.enrich_project_labels(&mut sessions).await;
        sessions
    }

    async fn list_raw(&self) -> Vec<SessionInfo> {
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .await
            .values()
            .map(|entry| entry.info.clone())
            .collect::<Vec<_>>();
        sessions.extend(self.inner.external.list().await);
        sessions.sort_by(|left, right| left.id.0.cmp(&right.id.0));
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
        if let Some(entry) = sessions.get(id) {
            return Ok(entry.info.clone());
        }
        drop(sessions);
        self.inner
            .external
            .inspect(id)
            .await
            .ok_or_else(|| session_not_found(&id.0))
    }

    /// Inspect a session by raw id string.
    pub async fn inspect_str(&self, id: &str) -> Result<SessionInfo, ProtocolError> {
        self.inspect(&SessionId(id.to_owned())).await
    }

    pub(crate) async fn ensure_known_agent(&self, id: &str) -> Result<(), ProtocolError> {
        let info = self.inspect_str(id).await?;
        info.agent_base.validate_mutation()?;
        if let Some(active) = &info.active_agent_base {
            active.validate_mutation()?;
        }
        Ok(())
    }

    pub(super) async fn record_cwd_hint(&self, id: &SessionId, path: String) {
        let cwd = PathBuf::from(path);
        if !cwd.is_absolute() {
            debug!(
                session_id = %id.0,
                cwd = %cwd.display(),
                "ignoring relative OSC 7 cwd hint"
            );
            return;
        }
        match cwd.try_exists() {
            Ok(true) => self.apply_cwd_change(id, cwd, CwdSource::Osc7).await,
            Ok(false) => {
                debug!(
                    session_id = %id.0,
                    cwd = %cwd.display(),
                    "ignoring OSC 7 cwd hint for a missing path"
                );
            }
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    cwd = %cwd.display(),
                    error = %err,
                    "failed to validate OSC 7 cwd hint"
                );
            }
        }
    }

    async fn apply_cwd_change(&self, id: &SessionId, cwd: PathBuf, source: CwdSource) {
        if !self.cwd_update_needed(id, &cwd).await {
            return;
        }

        let association = self.resolve_cwd_association(id, cwd.clone()).await;
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, "cwd update arrived for unknown session");
                return;
            };
            if entry.stopping || is_terminal(entry.info.state) || entry.info.cwd == cwd {
                return;
            }
            Some(apply_cwd_change(entry, cwd, source, association))
        };

        if let Some(info) = updated {
            self.emit(event::SESSION_UPDATED, &info);
        }
    }

    async fn cwd_update_needed(&self, id: &SessionId, cwd: &Path) -> bool {
        let sessions = self.inner.sessions.lock().await;
        let Some(entry) = sessions.get(id) else {
            return false;
        };
        !entry.stopping && !is_terminal(entry.info.state) && entry.info.cwd != cwd
    }

    async fn resolve_cwd_association(
        &self,
        id: &SessionId,
        cwd: PathBuf,
    ) -> Option<CwdAssociation> {
        let store = self.inner.store.clone();
        match tokio::task::spawn_blocking(move || {
            resolve_cwd_association(cwd.as_path(), store.as_deref())
        })
        .await
        {
            Ok(Ok(association)) => Some(association),
            Ok(Err(err)) => {
                warn!(
                    session_id = %id.0,
                    error = %err,
                    "failed to resolve cwd project/worktree association"
                );
                None
            }
            Err(err) => {
                warn!(
                    session_id = %id.0,
                    error = %err,
                    "cwd association task panicked"
                );
                None
            }
        }
    }

    /// Merge owner-controlled metadata into a session and return the updated info.
    pub async fn set_metadata(
        &self,
        id: &SessionId,
        merge: BTreeMap<String, Option<String>>,
    ) -> Result<SessionSetMetadataResult, ProtocolError> {
        self.ensure_not_external(id).await?;
        let (info, has_native) = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get_mut(id)
                .ok_or_else(|| session_not_found(&id.0))?;
            let mut candidate = entry.info.metadata.clone();
            for (key, value) in merge {
                match value {
                    Some(value) => {
                        candidate.insert(key, value);
                    }
                    None => {
                        candidate.remove(&key);
                    }
                }
            }
            validate_session_metadata(&candidate)?;
            entry.info.metadata = candidate;
            entry.info.updated_at = timestamp_now();
            let has_native =
                entry.info.native_session_id.is_some() || entry.info.native_session_path.is_some();
            (entry.info.clone(), has_native)
        };

        if has_native {
            self.persist_resume_binding(id).await;
        }

        self.emit(event::SESSION_UPDATED, &info);
        Ok(SessionSetMetadataResult { session: info })
    }

    /// Set or clear a session's display name and return the updated info.
    ///
    /// `name` is normalized and validated like the creation path
    /// ([`validate_session_name`]); `None` (or an all-whitespace name) clears it.
    ///
    /// # Errors
    ///
    /// Returns `session_not_found` for an unknown id, or the validation error
    /// when the trimmed name is too long or holds a control character.
    pub async fn rename(
        &self,
        id: &SessionId,
        name: Option<String>,
    ) -> Result<protocol::SessionRenameResult, ProtocolError> {
        self.ensure_not_external(id).await?;
        let normalized = validate_session_name(name.as_deref())?;
        let (info, has_native) = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get_mut(id)
                .ok_or_else(|| session_not_found(&id.0))?;
            entry.info.name = normalized;
            entry.info.updated_at = timestamp_now();
            let has_native =
                entry.info.native_session_id.is_some() || entry.info.native_session_path.is_some();
            (entry.info.clone(), has_native)
        };

        // The name lives in the resume binding so it survives a daemon restart;
        // refresh it only for a captured session (one with a binding to update).
        if has_native {
            self.persist_resume_binding(id).await;
        }

        self.emit(event::SESSION_UPDATED, &info);
        Ok(protocol::SessionRenameResult { session: info })
    }

    /// Resize a running session PTY and return the updated session info.
    pub async fn resize(
        &self,
        id: &SessionId,
        cols: u16,
        rows: u16,
    ) -> Result<protocol::SessionResizeResult, ProtocolError> {
        self.ensure_not_external(id).await?;
        if cols == 0 || rows == 0 {
            return Err(ProtocolError::bad_request(
                "session.resize requires non-zero cols and rows",
            ));
        }

        let runtime = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(id));
            }
            entry.runtime.clone()
        };

        let update = match runtime {
            RuntimeHandle::Worker(worker) => {
                let sequence = self
                    .inner
                    .next_resize_sequence
                    .fetch_add(1, Ordering::Relaxed);
                let source_id = pohunek_worker_protocol::StreamId::new("daemon-resize")
                    .map_err(|error| runtime_error("worker_resize_invalid", error.to_string()))?;
                let dimensions = pohunek_worker_protocol::Dimensions::new(cols, rows)
                    .map_err(|error| runtime_error("worker_resize_invalid", error.to_string()))?;
                worker
                    .resize(source_id, sequence, dimensions)
                    .await
                    .map_err(worker_error_to_protocol)?
            }
            RuntimeHandle::Unavailable(state) => {
                return Err(unavailable_runtime_error(id, state));
            }
        };

        let info = self.record_dimensions(id, &update).await?;
        Ok(protocol::SessionResizeResult { session: info })
    }

    async fn record_dimensions(
        &self,
        id: &SessionId,
        update: &DimensionUpdate,
    ) -> Result<SessionInfo, ProtocolError> {
        let dimensions = update.dimensions();
        let cols = dimensions.columns();
        let rows = dimensions.rows();
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
        Ok(info)
    }

    /// Stop a running session.
    pub async fn stop(&self, id: &SessionId) -> Result<SessionStopResult, ProtocolError> {
        self.stop_with_intent(id, DesiredState::Stopped, TransactionKind::Stop)
            .await
    }

    async fn stop_with_intent(
        &self,
        id: &SessionId,
        desired_state: DesiredState,
        transaction_kind: TransactionKind,
    ) -> Result<SessionStopResult, ProtocolError> {
        self.ensure_not_external(id).await?;
        let sequence = self.inner.next_write_id.fetch_add(1, Ordering::Relaxed);
        let operation = match transaction_kind {
            TransactionKind::Stop => "stop",
            TransactionKind::Remove => "remove",
            TransactionKind::Create => "create",
            TransactionKind::Recover => "recover",
        };
        let transaction_id = format!("{operation}-{sequence}");
        let (runtime, detector_cancel, procwatch_cancel, runtime_watch_cancel, durable_intent) = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get_mut(id)
                .ok_or_else(|| session_not_found(&id.0))?;
            if is_terminal(entry.info.state) {
                return Ok(SessionStopResult { stopped: false });
            }

            entry.stopping = true;
            entry.desired_state = desired_state;
            (
                entry.runtime.clone(),
                entry.detector_cancel.clone(),
                entry.procwatch_cancel.clone(),
                entry.runtime_watch_cancel.clone(),
                Self::session_record(
                    id,
                    entry,
                    desired_state,
                    Some(SessionTransaction {
                        id: transaction_id.clone(),
                        kind: transaction_kind,
                        phase: "requested".to_owned(),
                        previous_worker_id: None,
                        previous_runtime_id: None,
                    }),
                ),
            )
        };
        let terminal_base = durable_intent.clone();
        if let Err(error) = self.write_session_record(durable_intent).await {
            self.clear_stopping(id).await;
            return Err(error);
        }

        detector_cancel.cancel();
        procwatch_cancel.cancel();
        self.remove_pending_attaches_for_session(id).await;
        self.cancel_session_attaches(id).await;

        let exit = match runtime {
            RuntimeHandle::Worker(worker) => {
                let transaction = pohunek_worker_protocol::TransactionId::new(transaction_id)
                    .map_err(|error| runtime_error("worker_stop_invalid", error.to_string()))?;
                let status = match worker.stop(transaction).await {
                    Ok(Some(status)) => status,
                    Ok(None) => {
                        self.clear_stopping(id).await;
                        return Err(runtime_error(
                            "worker_stop_incomplete",
                            format!("worker for {} did not return a terminal outcome", id.0),
                        ));
                    }
                    Err(error) => {
                        self.clear_stopping(id).await;
                        return Err(worker_error_to_protocol(error));
                    }
                };
                RuntimeExit {
                    exit_code: status.code,
                    success: status.code == Some(0) && status.signal.is_none(),
                }
            }
            RuntimeHandle::Unavailable(state) => {
                self.clear_stopping(id).await;
                return Err(unavailable_runtime_error(id, state));
            }
        };
        self.commit_user_stop_exit(id, exit, &terminal_base).await?;
        runtime_watch_cancel.cancel();
        // `stop` is the user-owned terminal transition and must be the final
        // word on native-recovery eligibility. A concurrent exit watcher can race
        // through `record_exit` first, making our later `record_exit(..., true)`
        // take its idempotent early-return path; this extra persist is
        // intentionally idempotent and guarantees the binding is gone when
        // `stop()` returns.
        self.persist_resume_binding(id).await;
        Ok(SessionStopResult { stopped: true })
    }

    async fn commit_user_stop_exit(
        &self,
        id: &SessionId,
        exit: RuntimeExit,
        terminal_base: &SessionRecord,
    ) -> Result<(), ProtocolError> {
        let mut use_terminal_base = true;
        let mut last_persistence_error = None;
        for _ in 0..MAX_RUNTIME_TRANSITION_COMMIT_ATTEMPTS {
            let expected_record = use_terminal_base.then_some(terminal_base);
            match self
                .record_exit(id, exit, true, None, expected_record)
                .await
            {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    use_terminal_base = false;
                    last_persistence_error = None;
                }
                Err(error) => last_persistence_error = Some(error),
            }
            tokio::task::yield_now().await;
        }
        if let Some(error) = last_persistence_error {
            return Err(error);
        }
        Err(runtime_error(
            "session_runtime_transition_busy",
            format!(
                "session {} kept changing while committing its terminal state",
                id.0
            ),
        ))
    }

    /// Evict a session from the registry, stopping it first if still live.
    ///
    /// `stop` only flips a live session to a terminal state; the entry stays in
    /// the registry so `list`/`inspect` keep showing it, which is why a stopped
    /// session otherwise lingers forever. `remove` is the eviction step. A
    /// still-live worker-backed session is stopped first (so removal never
    /// orphans a live PTY), then the entry is dropped and its resume binding
    /// cleared so a daemon restart cannot resurrect it. An unavailable runtime
    /// is already outside the daemon's control, so removal evicts only its
    /// logical record and deliberately does not signal an ambiguous worker. A
    /// `session_removed` event is emitted with the final snapshot so subscribed
    /// clients drop their view of the session.
    ///
    /// # Errors
    ///
    /// Returns `session_not_found` when no session has the given id, and
    /// surfaces any PTY shutdown error from the implied stop of a live session.
    pub async fn remove(&self, id: &SessionId) -> Result<SessionRemoveResult, ProtocolError> {
        self.ensure_not_external(id).await?;
        let should_stop = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            !is_terminal(entry.info.state) && matches!(entry.runtime, RuntimeHandle::Worker(_))
        };

        let stopped = if should_stop {
            self.stop_with_intent(id, DesiredState::Removed, TransactionKind::Remove)
                .await?
                .stopped
        } else {
            let removal_intent = {
                let mut sessions = self.inner.sessions.lock().await;
                let entry = sessions
                    .get_mut(id)
                    .ok_or_else(|| session_not_found(&id.0))?;
                entry.desired_state = DesiredState::Removed;
                Self::session_record(
                    id,
                    entry,
                    DesiredState::Removed,
                    Some(SessionTransaction {
                        id: format!(
                            "remove-{}",
                            self.inner.next_write_id.fetch_add(1, Ordering::Relaxed)
                        ),
                        kind: TransactionKind::Remove,
                        phase: "requested".to_owned(),
                        previous_worker_id: None,
                        previous_runtime_id: None,
                    }),
                )
            };
            self.write_session_record(removal_intent).await?;
            false
        };

        let cleanup_warnings = self.cleanup_owned_worktrees_for_removal(id).await?;
        if !cleanup_warnings.is_empty() {
            let mut sessions = self.inner.sessions.lock().await;
            if let Some(entry) = sessions.get_mut(id) {
                entry.info.warnings.extend(cleanup_warnings);
            }
        }

        // The PTY has stopped above, so cleanup removes the accumulated family.
        // A retained terminal worker may still emit a final control diagnostic,
        // but the shared writer keeps any such file within the same hard cap.
        self.delete_session_logs(id).await?;

        let info = {
            let mut sessions = self.inner.sessions.lock().await;
            match sessions.remove(id) {
                Some(entry) => entry.info,
                // A concurrent `remove` won the race and already evicted it.
                None => {
                    return Ok(SessionRemoveResult {
                        removed: false,
                        stopped,
                    })
                }
            }
        };

        // The entry is gone, so this re-reads as "no live session" and clears any
        // lingering resume binding (idempotent for a session that already dropped
        // its binding on exit or stop).
        self.persist_resume_binding(id).await;
        self.delete_session_record(id).await?;
        self.emit(event::SESSION_REMOVED, &info);
        Ok(SessionRemoveResult {
            removed: true,
            stopped,
        })
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

    fn spawn_worker_exit_watcher(
        &self,
        id: SessionId,
        initial_worker: Worker,
        expected: RuntimeWatchIdentity,
        cancel: CancellationToken,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            let socket_path = initial_worker.socket_path().to_path_buf();
            let mut worker = initial_worker;
            let mut last_worker_identity = None;
            loop {
                let inspected = tokio::select! {
                    () = cancel.cancelled() => return,
                    inspected = worker.inspect() => inspected,
                };
                match inspected {
                    Ok(snapshot) => {
                        let worker_identity = worker_identity_fingerprint(&snapshot);
                        let initial_empty_identity = last_worker_identity.is_none()
                            && worker_identity_is_empty(&worker_identity);
                        if !initial_empty_identity
                            && last_worker_identity.as_ref() != Some(&worker_identity)
                        {
                            if registry
                                .apply_worker_identity_snapshot(&id, &snapshot)
                                .await
                            {
                                last_worker_identity = Some(worker_identity.clone());
                            }
                        } else {
                            last_worker_identity = Some(worker_identity.clone());
                        }
                        if snapshot.phase == pohunek_worker_protocol::RuntimePhase::Exited {
                            let exit = snapshot.exit.map_or(
                                RuntimeExit {
                                    exit_code: None,
                                    success: false,
                                },
                                |status| RuntimeExit {
                                    exit_code: status.code,
                                    success: status.code == Some(0) && status.signal.is_none(),
                                },
                            );
                            if matches!(
                                registry
                                    .record_exit(&id, exit, false, Some(&expected), None)
                                    .await,
                                Ok(true)
                            ) {
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        let Some(reconnected) = registry
                            .reconnect_worker(&id, &expected, &socket_path, &cancel, &error)
                            .await
                        else {
                            return;
                        };
                        worker = reconnected;
                    }
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(WORKER_CONNECT_RETRY) => {}
                }
            }
        });
    }

    async fn reconnect_worker(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        socket_path: &Path,
        cancel: &CancellationToken,
        error: &WorkerError,
    ) -> Option<Worker> {
        if !self.mark_worker_reconnecting(id, expected, error).await {
            return None;
        }
        let mut deadline = tokio::time::Instant::now() + self.inner.config.worker_connect_deadline;
        loop {
            if cancel.is_cancelled() || self.inner.daemon_shutdown_started.load(Ordering::Relaxed) {
                return None;
            }
            let connected = tokio::select! {
                () = cancel.cancelled() => return None,
                connected = Worker::connect(socket_path, &id.0, self.daemon_instance_id()) => connected,
            };
            match connected {
                Ok(worker) => match self
                    .adopt_reconnected_worker(id, expected, worker.clone())
                    .await
                {
                    RuntimeTransitionOutcome::Applied(_) => return Some(worker),
                    RuntimeTransitionOutcome::IdentityMismatch => return None,
                    RuntimeTransitionOutcome::RetryablePersistenceFailure(_)
                    | RuntimeTransitionOutcome::RetryableConcurrentChange => {
                        tokio::select! {
                            () = cancel.cancelled() => return None,
                            () = tokio::time::sleep(WORKER_CONNECT_RETRY) => {}
                        }
                    }
                },
                Err(reconnect_error) if tokio::time::Instant::now() >= deadline => {
                    match self.mark_worker_lost(id, expected, &reconnect_error).await {
                        RuntimeTransitionOutcome::Applied(_)
                        | RuntimeTransitionOutcome::IdentityMismatch => return None,
                        RuntimeTransitionOutcome::RetryablePersistenceFailure(_)
                        | RuntimeTransitionOutcome::RetryableConcurrentChange => {
                            deadline = tokio::time::Instant::now()
                                + self.inner.config.worker_connect_deadline;
                        }
                    }
                }
                Err(reconnect_error) => {
                    debug!(
                        session_id = %id.0,
                        error = %reconnect_error,
                        "session worker is not reconnectable yet"
                    );
                    tokio::select! {
                        () = cancel.cancelled() => return None,
                        () = tokio::time::sleep(WORKER_CONNECT_RETRY) => {}
                    }
                }
            }
        }
    }

    async fn mark_worker_reconnecting(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        error: &WorkerError,
    ) -> bool {
        let mut sessions = self.inner.sessions.lock().await;
        let Some(entry) = sessions.get_mut(id) else {
            return false;
        };
        if !expected.matches(entry) {
            return false;
        }
        if let Some(runtime) = entry.info.runtime.as_mut() {
            runtime.state = RuntimeState::Reconnecting;
            runtime.loss_reason = Some("worker_connection_lost".to_owned());
        }
        entry.info.updated_at = timestamp_now();
        drop(sessions);
        warn!(
            session_id = %id.0,
            error = %error,
            "worker control connection lost; runtime remains alive while reconnecting"
        );
        true
    }

    async fn mark_worker_lost(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        error: &WorkerError,
    ) -> RuntimeTransitionOutcome {
        let (base, candidate) = {
            let sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get(id) else {
                return RuntimeTransitionOutcome::IdentityMismatch;
            };
            if !expected.matches(entry) {
                return RuntimeTransitionOutcome::IdentityMismatch;
            }
            let base = Self::session_record(id, entry, entry.desired_state, None);
            let mut candidate = entry.clone();
            candidate.runtime = RuntimeHandle::Unavailable(RuntimeState::Lost);
            if let Some(runtime) = candidate.info.runtime.as_mut() {
                runtime.state = RuntimeState::Lost;
                runtime.loss_reason = Some("worker_process_lost".to_owned());
            }
            candidate.info.updated_at = timestamp_now();
            (base, candidate)
        };
        let durable_base = match self.load_durable_session_record(id).await {
            Ok(Some(record)) => record,
            Ok(None) => base.clone(),
            Err(error) => return RuntimeTransitionOutcome::RetryablePersistenceFailure(error),
        };
        let outcome = self
            .commit_runtime_transition(id, expected, &base, &durable_base, candidate)
            .await;
        if let RuntimeTransitionOutcome::RetryablePersistenceFailure(store_error) = &outcome {
            warn!(
                session_id = %id.0,
                error = %store_error,
                "failed to persist lost worker classification"
            );
        }
        if let RuntimeTransitionOutcome::Applied(info) = &outcome {
            warn!(
                session_id = %id.0,
                error = %error,
                "session worker could not be reconnected; PTY runtime is lost"
            );
            self.emit(event::SESSION_RUNTIME_LOST, info.as_ref());
        }
        outcome
    }

    async fn adopt_reconnected_worker(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        worker: Worker,
    ) -> RuntimeTransitionOutcome {
        let worker_id = worker.worker_id().await.to_string();
        let Some(runtime_id) = worker.runtime_id().await.map(|value| value.to_string()) else {
            return RuntimeTransitionOutcome::IdentityMismatch;
        };
        if worker_id != expected.worker_id || runtime_id != expected.runtime_id {
            return RuntimeTransitionOutcome::IdentityMismatch;
        }
        let (base, candidate) = {
            let sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get(id) else {
                return RuntimeTransitionOutcome::IdentityMismatch;
            };
            if !expected.matches(entry) {
                return RuntimeTransitionOutcome::IdentityMismatch;
            }
            let base = Self::session_record(id, entry, entry.desired_state, None);
            let mut candidate = entry.clone();
            candidate.runtime = RuntimeHandle::Worker(worker);
            if let Some(runtime) = candidate.info.runtime.as_mut() {
                runtime.state = RuntimeState::Live;
                runtime.worker_id = Some(worker_id);
                runtime.runtime_id = Some(runtime_id);
                runtime.last_connected_at = Some(timestamp_now());
                runtime.loss_reason = None;
            }
            candidate.info.updated_at = timestamp_now();
            (base, candidate)
        };
        let durable_base = match self.load_durable_session_record(id).await {
            Ok(Some(record)) => record,
            Ok(None) => base.clone(),
            Err(error) => return RuntimeTransitionOutcome::RetryablePersistenceFailure(error),
        };
        let outcome = self
            .commit_runtime_transition(id, expected, &base, &durable_base, candidate)
            .await;
        if let RuntimeTransitionOutcome::RetryablePersistenceFailure(error) = &outcome {
            warn!(session_id = %id.0, error = %error, "failed to persist reconnected worker");
        }
        if let RuntimeTransitionOutcome::Applied(info) = &outcome {
            self.emit(event::SESSION_RUNTIME_RECONNECTED, info.as_ref());
        }
        outcome
    }

    async fn commit_runtime_transition(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        memory_base: &SessionRecord,
        durable_base: &SessionRecord,
        mut candidate: SessionEntry,
    ) -> RuntimeTransitionOutcome {
        let mut record = Self::session_record(id, &candidate, candidate.desired_state, None);
        crate::store::preserve_newer_native_identity(durable_base, &mut record);
        candidate
            .info
            .native_session_id
            .clone_from(&record.info.native_session_id);
        candidate
            .info
            .native_session_path
            .clone_from(&record.info.native_session_path);
        candidate
            .last_native_report
            .clone_from(&record.native_identity_ordering);
        if let Err(error) = self
            .write_session_record_if_current(durable_base.clone(), record)
            .await
        {
            return match error.code.as_str() {
                "session_runtime_commit_stale" => RuntimeTransitionOutcome::IdentityMismatch,
                "session_record_commit_stale" => {
                    RuntimeTransitionOutcome::RetryableConcurrentChange
                }
                _ => RuntimeTransitionOutcome::RetryablePersistenceFailure(error),
            };
        }
        let mut sessions = self.inner.sessions.lock().await;
        let Some(current) = sessions.get(id) else {
            return RuntimeTransitionOutcome::IdentityMismatch;
        };
        if !expected.matches(current) {
            return RuntimeTransitionOutcome::IdentityMismatch;
        }
        let current_record = Self::session_record(id, current, current.desired_state, None);
        if &current_record != memory_base {
            return RuntimeTransitionOutcome::RetryableConcurrentChange;
        }
        let info = candidate.info.clone();
        sessions.insert(id.clone(), candidate);
        RuntimeTransitionOutcome::Applied(Box::new(info))
    }

    async fn record_exit(
        &self,
        id: &SessionId,
        exit: RuntimeExit,
        stopped_by_user: bool,
        expected: Option<&RuntimeWatchIdentity>,
        expected_record: Option<&SessionRecord>,
    ) -> Result<bool, ProtocolError> {
        let updated = Box::new({
            let sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get(id) else {
                debug!(session_id = %id.0, "PTY exit arrived for unknown session");
                return Ok(true);
            };
            let expected = expected
                .cloned()
                .or_else(|| RuntimeWatchIdentity::from_info(&entry.info));
            let Some(expected) = expected else {
                return Ok(true);
            };
            if !expected.matches(entry) {
                return Ok(true);
            }

            if self.inner.daemon_shutdown_started.load(Ordering::Relaxed)
                && !stopped_by_user
                && !entry.stopping
            {
                debug!(
                    session_id = %id.0,
                    "ignoring PTY exit observed after daemon shutdown started"
                );
                return Ok(true);
            }

            if entry.info.state == SessionState::Stopped && stopped_by_user && !entry.stopping {
                return Ok(true);
            }

            if is_terminal(entry.info.state) && !stopped_by_user && !entry.stopping {
                return Ok(true);
            }

            exit_transition(id, entry, expected, exit, stopped_by_user)
        });
        let loaded_durable_base;
        let durable_base = if let Some(expected_record) = expected_record {
            expected_record
        } else {
            loaded_durable_base = match Box::pin(self.load_durable_session_record(id)).await {
                Ok(Some(record)) => record,
                Ok(None) => updated.base.clone(),
                Err(error) => return Err(error),
            };
            &loaded_durable_base
        };
        let updated = *updated;

        let committed_info = match Box::pin(self.commit_runtime_transition(
            id,
            &updated.expected,
            &updated.base,
            durable_base,
            updated.candidate,
        ))
        .await
        {
            RuntimeTransitionOutcome::Applied(info) => info,
            RuntimeTransitionOutcome::IdentityMismatch => return Ok(true),
            RuntimeTransitionOutcome::RetryablePersistenceFailure(error) => {
                warn!(
                    session_id = %id.0,
                    error = %error,
                    "failed to persist terminal session outcome"
                );
                return Err(error);
            }
            RuntimeTransitionOutcome::RetryableConcurrentChange => return Ok(false),
        };
        updated.detector_cancel.cancel();
        updated.procwatch_cancel.cancel();
        self.cancel_session_attaches(id).await;
        self.remove_pending_attaches_for_session(id).await;
        self.spawn_session_hook(SessionHookRequest {
            event: HookEvent::SessionStop,
            cwd: committed_info.cwd.clone(),
            session_id: committed_info.id.0.clone(),
            project_id: committed_info.project_id.clone(),
            agent: committed_info.agent.clone(),
            stop_reason: Some(updated.stop_reason),
            activity: None,
        });
        // A terminal session must not resurrect on the next daemon restart:
        // resume is for sessions whose live PTY a restart killed, not for ones
        // the user stopped or that exited. The session is now terminal, so
        // `persist_resume_binding` re-reads it as terminal and removes its
        // binding (serialized against any racing resize/capture write).
        self.persist_resume_binding(id).await;
        self.emit(updated.event, committed_info.as_ref());
        Ok(true)
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
        self.ensure_not_external(id).await?;
        let sessions = self.inner.sessions.lock().await;
        let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
        if entry.info.state == SessionState::Running {
            Ok(())
        } else {
            Err(session_not_running(id))
        }
    }

    async fn ensure_not_external(&self, id: &SessionId) -> Result<(), ProtocolError> {
        if self.inner.external.contains_id(id).await {
            return Err(session_external_read_only(id));
        }
        let sessions = self.inner.sessions.lock().await;
        if let Some(entry) = sessions.get(id) {
            entry.info.agent_base.validate_mutation()?;
            if let Some(active) = &entry.info.active_agent_base {
                active.validate_mutation()?;
            }
        }
        Ok(())
    }

    /// Rescan same-user processes for external agents.
    pub(crate) async fn rescan_external_agents(&self, transcripts: &TranscriptIndex) {
        let facts = match self.inner.inspector.same_user_processes() {
            Ok(facts) => facts,
            Err(err) => {
                warn!(error = %err, "failed to inspect same-user processes for external agents");
                // Per-external pidfd exit watches remain the authoritative removal
                // backstop when a sweep cannot refresh process facts.
                return;
            }
        };
        let owned_pids = self.owned_process_pids(&facts).await;
        let existing_pids = self.inner.external.pids().await;
        let mut observed_pids = HashSet::new();

        for fact in facts {
            if owned_pids.contains(&fact.pid) {
                continue;
            }
            let Some(agent_base) = identify_agent(&fact) else {
                continue;
            };
            // A process carrying pohunek ownership markers is a PTY child of
            // *some* pohunek daemon — this one (already excluded by the owned
            // pid walk, markers are the backstop for ppid gaps) or another
            // instance (a nested test-suite daemon, a second dev daemon). A
            // managed agent must never surface as an external session.
            match self.inner.inspector.ownership_markers(fact.pid) {
                Ok(markers) if markers.is_marked() => continue,
                Ok(_) => {}
                Err(err) => {
                    debug!(
                        pid = fact.pid,
                        error = %err,
                        "failed to read external candidate ownership markers; keeping it observable"
                    );
                }
            }
            let cwd = match self.inner.inspector.cwd(fact.pid) {
                Ok(cwd) => cwd,
                Err(err) => {
                    debug!(
                        pid = fact.pid,
                        error = %err,
                        "failed to inspect external agent cwd"
                    );
                    continue;
                }
            };
            observed_pids.insert(fact.pid);
            let candidate = transcripts.best_match(&agent_base, &cwd, &fact);
            let association = self
                .resolve_external_cwd_association(fact.pid, cwd.clone())
                .await;
            let info = external_session_info(&fact, agent_base, cwd, candidate, association);
            let change = self.inner.external.upsert(info).await;
            match change {
                Some(ExternalSessionChange::Created(info)) => {
                    if !existing_pids.contains(&fact.pid) {
                        self.spawn_external_exit_watch(fact.pid);
                    }
                    self.emit(event::SESSION_CREATED, &info);
                }
                Some(ExternalSessionChange::Updated(info)) => {
                    self.emit(event::SESSION_UPDATED, &info);
                }
                None => {}
            }
        }

        for removed in self.inner.external.remove_unobserved(&observed_pids).await {
            self.emit(event::SESSION_REMOVED, &removed);
        }
    }

    async fn owned_process_pids(&self, facts: &[ProcessFact]) -> HashSet<Pid> {
        let roots = {
            let sessions = self.inner.sessions.lock().await;
            sessions
                .values()
                .filter(|entry| !is_terminal(entry.info.state))
                .map(|entry| entry.info.pid)
                .collect::<Vec<_>>()
        };
        let mut owned = roots.iter().copied().collect::<HashSet<_>>();
        let mut queue = VecDeque::from(roots);
        let mut children_by_parent: HashMap<Pid, Vec<Pid>> = HashMap::new();
        for fact in facts {
            children_by_parent
                .entry(fact.ppid)
                .or_default()
                .push(fact.pid);
        }
        while let Some(parent) = queue.pop_front() {
            let Some(children) = children_by_parent.get(&parent) else {
                continue;
            };
            for &child in children {
                if owned.insert(child) {
                    queue.push_back(child);
                }
            }
        }
        owned
    }

    async fn resolve_external_cwd_association(
        &self,
        pid: Pid,
        cwd: PathBuf,
    ) -> Option<CwdAssociation> {
        let store = self.inner.store.clone();
        match tokio::task::spawn_blocking(move || {
            resolve_cwd_association(cwd.as_path(), store.as_deref())
        })
        .await
        {
            Ok(Ok(association)) => Some(association),
            Ok(Err(err)) => {
                debug!(
                    pid,
                    error = %err,
                    "failed to resolve external agent cwd association"
                );
                None
            }
            Err(err) => {
                warn!(
                    pid,
                    error = %err,
                    "external cwd association task panicked"
                );
                None
            }
        }
    }

    fn spawn_external_exit_watch(&self, pid: Pid) {
        let watch = match self.inner.inspector.exit_watch(pid) {
            Ok(watch) => watch,
            Err(err) => {
                debug!(
                    pid,
                    error = %err,
                    "failed to arm external agent exit watch; falling back to poll cleanup"
                );
                return;
            }
        };
        self.spawn_external_exit_watch_task(pid, watch);
    }

    fn spawn_external_exit_watch_task(&self, pid: Pid, watch: ExitWatch) {
        let registry = self.clone();
        let shutdown = self.inner.external.shutdown_token();
        tokio::spawn(async move {
            tokio::select! {
                () = shutdown.cancelled() => {}
                result = watch.wait() => {
                    if let Err(err) = result {
                        debug!(
                            pid,
                            error = %err,
                            "external process exit watch failed"
                        );
                        return;
                    }
                    registry.on_external_agent_exit(pid).await;
                }
            }
        });
    }

    async fn on_external_agent_exit(&self, pid: Pid) {
        if let Some(info) = self.inner.external.remove_pid(pid).await {
            self.emit(event::SESSION_REMOVED, &info);
        }
    }

    fn emit(&self, name: &str, info: &SessionInfo) {
        let event = crate::events::event(
            name,
            event_payload(SessionEvent {
                session: info.clone(),
            }),
        );
        let _ = self.inner.events.send(event);
    }

    fn emit_native_recovered(&self, info: &SessionInfo, previous_runtime_id: Option<String>) {
        let runtime_id = info
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.runtime_id.clone());
        let event = crate::events::event(
            event::SESSION_NATIVE_RECOVERED,
            event_payload(SessionNativeRecoveredEvent {
                session: info.clone(),
                previous_runtime_id,
                runtime_id,
            }),
        );
        let _ = self.inner.events.send(event);
    }
}

type WorkerIdentityFingerprint = (
    Option<pohunek_worker_protocol::ReportedLaunchIdentity>,
    Option<pohunek_worker_protocol::ActiveIdentityClaim>,
    Option<pohunek_worker_protocol::ReleasedIdentityClaim>,
);

fn worker_identity_fingerprint(
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
) -> WorkerIdentityFingerprint {
    (
        snapshot.launch_identity.clone(),
        snapshot.active_identity.clone(),
        snapshot.active_identity_release.clone(),
    )
}

fn worker_identity_is_empty(identity: &WorkerIdentityFingerprint) -> bool {
    identity.0.is_none() && identity.1.is_none() && identity.2.is_none()
}

fn identity_claim_expiry_is_valid(value: &str) -> bool {
    let Ok(expires_at) = OffsetDateTime::parse(value, &Rfc3339) else {
        return false;
    };
    let now = OffsetDateTime::now_utc();
    let max_expiry = now
        + time::Duration::seconds(
            i64::try_from(protocol::MAX_IDENTITY_CLAIM_TTL_SECS)
                .expect("identity TTL ceiling fits i64"),
        );
    expires_at > now && expires_at <= max_expiry
}

fn event_payload<T>(payload: T) -> Value
where
    T: serde::Serialize,
{
    serde_json::to_value(payload).expect("protocol event payload serialization is infallible")
}

fn apply_cwd_change(
    entry: &mut SessionEntry,
    cwd: PathBuf,
    source: CwdSource,
    association: Option<CwdAssociation>,
) -> SessionInfo {
    entry.info.cwd = cwd;
    entry.info.cwd_source = Some(source);
    if let Some(association) = association {
        entry.info.project_id = association.project_id;
        entry.info.project_label = None;
        entry.info.is_linked_worktree = association.is_linked_worktree;
        entry.info.repo = association.repo;
        entry.info.branch = association.branch;
        entry.info.worktree_path = association.worktree_path;
    }
    entry.info.updated_at = timestamp_now();
    entry.info.clone()
}

fn external_session_info(
    fact: &ProcessFact,
    agent_base: AgentKind,
    cwd: PathBuf,
    candidate: Option<TranscriptCandidate>,
    association: Option<CwdAssociation>,
) -> SessionInfo {
    let now = timestamp_now();
    let agent = agent_kind_label(&agent_base).to_owned();
    let (native_session_id, native_session_path) = candidate.map_or((None, None), |candidate| {
        (
            candidate.native_session_id,
            Some(candidate.native_session_path),
        )
    });
    let association = association.unwrap_or_default();
    SessionInfo {
        id: external_session_id(fact.pid),
        external: Some(true),
        capabilities: protocol::SessionCapabilities::default(),
        name: None,
        agent,
        agent_base,
        cwd,
        cwd_source: Some(CwdSource::Procwatch),
        pid: fact.pid,
        runtime: None,
        cols: EXTERNAL_TERMINAL_COLS,
        rows: EXTERNAL_TERMINAL_ROWS,
        state: SessionState::Running,
        state_source: StateSource::Process,
        activity: None,
        active_agent: None,
        active_agent_base: None,
        active_agent_pid: None,
        active_agent_session_id: None,
        active_agent_session_path: None,
        native_session_id,
        native_session_path,
        project_id: association.project_id,
        project_label: None,
        is_linked_worktree: association.is_linked_worktree,
        repo: association.repo,
        branch: association.branch,
        worktree_path: association.worktree_path,
        warnings: Vec::new(),
        metadata: BTreeMap::new(),
        created_at: now.clone(),
        updated_at: now,
        exit_code: None,
    }
}

fn resolve_cwd_association(
    cwd: &Path,
    store: Option<&Store>,
) -> Result<CwdAssociation, ProtocolError> {
    let canonical_cwd = canonical_or_original(cwd);
    let worktree = match store {
        Some(store) => active_worktree_for_cwd(store, &canonical_cwd)?,
        None => None,
    };
    let detected = detect_at(cwd)?;
    let mut association = match detected {
        Some(detected) => association_from_detected_project(&detected),
        None => CwdAssociation::default(),
    };

    if let Some(binding) = worktree {
        association.worktree_path = Some(binding.path);
        association.repo = Some(binding.repository);
        association.branch = Some(binding.branch);
        association.is_linked_worktree = Some(true);
        if binding.project_id.is_some() {
            association.project_id = binding.project_id;
        }
    }

    Ok(association)
}

/// Associates a detected repo with its *derived* project id — deliberately
/// without registering it. Cwd hints (OSC 7, procwatch focus) are transient
/// observations; letting them upsert project records means any repo a watched
/// process merely sits in — e.g. a throwaway fixture repo created by a nested
/// test-suite daemon — permanently pollutes the registry. Registration happens
/// only on `session.new` (see `SessionTarget` resolution) and explicit
/// `project add`. The derived id is stable, so it matches the record whenever
/// the project is (or later becomes) registered.
fn association_from_detected_project(detected: &DetectedProject) -> CwdAssociation {
    CwdAssociation {
        project_id: Some(project_id(&detected.git_common_dir)),
        is_linked_worktree: Some(detected.is_linked_worktree),
        repo: Some(detected.repo_root.clone()),
        branch: detected.branch.clone(),
        worktree_path: None,
    }
}

fn active_worktree_for_cwd(
    store: &Store,
    canonical_cwd: &Path,
) -> Result<Option<crate::store::WorktreeBinding>, ProtocolError> {
    let mut best = None;
    for binding in store.load_worktrees().map_err(|err| {
        runtime_error(
            "cwd_worktree_resolve_failed",
            format!("failed to load worktree bindings: {err}"),
        )
    })? {
        if binding.status != WorktreeStatus::Active {
            continue;
        }
        let path = canonical_or_original(&binding.path);
        if !canonical_cwd.starts_with(&path) {
            continue;
        }
        let depth = path.components().count();
        let replace = best
            .as_ref()
            .is_none_or(|(best_depth, _)| depth > *best_depth);
        if replace {
            best = Some((depth, binding));
        }
    }
    Ok(best.map(|(_, binding)| binding))
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
    validate_session_metadata(&params.metadata)?;
    validate_session_name(params.name.as_deref())?;
    Ok(())
}

/// Normalize and validate an owner-set session name.
///
/// Trims surrounding whitespace, treats an all-whitespace name as unset
/// (`None`), and rejects a name that exceeds [`MAX_SESSION_NAME_BYTES`] or
/// carries control characters (which would corrupt a single-line table/row).
///
/// # Errors
///
/// Returns a `bad_request` [`ProtocolError`] when the trimmed name is too long
/// or contains a control character.
fn validate_session_name(name: Option<&str>) -> Result<Option<String>, ProtocolError> {
    let Some(name) = name else {
        return Ok(None);
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() > MAX_SESSION_NAME_BYTES {
        return Err(ProtocolError::bad_request(format!(
            "session name exceeds {MAX_SESSION_NAME_BYTES} bytes"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(ProtocolError::bad_request(
            "session name must not contain control characters",
        ));
    }
    Ok(Some(trimmed.to_owned()))
}

fn validate_session_metadata(metadata: &BTreeMap<String, String>) -> Result<(), ProtocolError> {
    if metadata.len() > MAX_SESSION_METADATA_KEYS {
        return Err(ProtocolError::bad_request(format!(
            "session metadata must contain at most {MAX_SESSION_METADATA_KEYS} keys"
        )));
    }
    for (key, value) in metadata {
        let key_len = key.len();
        if key_len > MAX_SESSION_METADATA_KEY_BYTES {
            return Err(ProtocolError::bad_request(format!(
                "session metadata key exceeds {MAX_SESSION_METADATA_KEY_BYTES} bytes"
            )));
        }
        let value_len = value.len();
        if value_len > MAX_SESSION_METADATA_VALUE_BYTES {
            return Err(ProtocolError::bad_request(format!(
                "session metadata value for key {key:?} exceeds {MAX_SESSION_METADATA_VALUE_BYTES} bytes"
            )));
        }
    }
    let serialized_len = serde_json::to_vec(metadata)
        .map_err(|err| ProtocolError::bad_request(format!("session metadata is invalid: {err}")))?
        .len();
    if serialized_len > MAX_SESSION_METADATA_SERIALIZED_BYTES {
        return Err(ProtocolError::bad_request(format!(
            "session metadata serialized size exceeds {MAX_SESSION_METADATA_SERIALIZED_BYTES} bytes"
        )));
    }
    Ok(())
}

fn is_terminal(state: SessionState) -> bool {
    state.is_terminal()
}

fn agent_kind_label(agent: &AgentKind) -> &str {
    match agent {
        AgentKind::Shell => "shell",
        AgentKind::Codex => "codex",
        AgentKind::Claude => "claude",
        AgentKind::Hermes => "hermes",
        AgentKind::Unknown(value) => value,
    }
}

fn detector_config_for_resolved_agent(resolved: &ResolvedAgent) -> DetectorConfig {
    DetectorConfig::for_profile(
        &resolved.base,
        resolved
            .profile
            .as_ref()
            .and_then(|profile| profile.manifest.clone()),
    )
}

fn bind_report_pid(
    entry: &SessionEntry,
    reported_pid: Option<Pid>,
    agent_base: &AgentKind,
) -> Option<Pid> {
    if let Some(pid) = reported_pid {
        // PID-bearing hooks are exact claims. If procwatch has not observed the
        // process yet, keep the exact pid so the immediate rescan can either bind
        // it or release it instead of falling back to an ambiguous base-kind match.
        return Some(pid);
    }

    let mut matching = entry
        .observed_agents
        .iter()
        .filter(|observed| &observed.agent_base == agent_base)
        .map(|observed| observed.pid);
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

fn clear_active_agent(entry: &mut SessionEntry, tombstone: ActiveAgentReport) -> SessionInfo {
    let activity_reported = entry
        .active_agent
        .as_ref()
        .is_some_and(|active| active.activity_reported);
    entry.last_agent_report = Some(tombstone);
    entry.active_agent = None;
    entry.info.active_agent = None;
    entry.info.active_agent_base = None;
    entry.info.active_agent_pid = None;
    entry.info.active_agent_session_id = None;
    entry.info.active_agent_session_path = None;
    if activity_reported {
        entry.info.activity = None;
        entry.info.state_source = StateSource::Process;
    }
    let default_detector_config = entry.default_detector_config.clone();
    let _ = entry.detector_config.send(default_detector_config);
    entry.info.updated_at = timestamp_now();
    entry.info.clone()
}

fn report_is_current(
    current: Option<&ActiveAgentReport>,
    source: &str,
    agent: &str,
    seq: Option<u64>,
) -> bool {
    let Some(current) = current else {
        return true;
    };
    if current.source != source || current.agent != agent {
        return true;
    }
    seq_is_current(current.seq, seq)
}

fn native_report_is_current(
    current: Option<&NativeIdentityReport>,
    runtime_id: &str,
    sequence: u64,
) -> bool {
    current.is_none_or(|current| current.runtime_id != runtime_id || sequence > current.sequence)
}

fn release_matches(
    current: &ActiveAgentReport,
    source: &str,
    agent: &str,
    seq: Option<u64>,
) -> bool {
    current.source == source && current.agent == agent && seq_is_current(current.seq, seq)
}

fn seq_is_current(current: Option<u64>, incoming: Option<u64>) -> bool {
    match (current, incoming) {
        (Some(current), Some(incoming)) => incoming >= current,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn validate_agent_session_id(session_id: &SessionId, value: Option<&str>) -> Option<String> {
    value.and_then(|raw| match SessionRef::id(raw) {
        Ok(session_ref) => Some(session_ref.value().to_owned()),
        Err(err) => {
            debug!(
                session_id = %session_id.0,
                error = %err,
                "ignoring active-agent report with an invalid native session id"
            );
            None
        }
    })
}

fn validate_agent_session_path(session_id: &SessionId, value: Option<&str>) -> Option<String> {
    value.and_then(|raw| match SessionRef::path(raw) {
        Ok(session_ref) => Some(session_ref.value().to_owned()),
        Err(err) => {
            debug!(
                session_id = %session_id.0,
                error = %err,
                "ignoring active-agent report with an invalid native session path"
            );
            None
        }
    })
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

fn session_external_read_only(id: &SessionId) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "session_external_read_only",
        format!(
            "session {} is an external observe-only agent and has no pohunek-owned PTY",
            id.0
        ),
        Some(
            "start the agent through pohunek to attach, send input, resize, or stop it".to_owned(),
        ),
    )
}

fn worker_error_to_protocol(err: WorkerError) -> ProtocolError {
    match err {
        unsupported @ WorkerError::AttachSnapshotUnsupported { .. } => ProtocolError::new(
            ErrorClass::Runtime,
            "attach_snapshot_unsupported",
            unsupported.to_string(),
            Some(
                "restart the session on the upgraded worker, or fork it into a new session"
                    .to_owned(),
            ),
        ),
        other => runtime_error("worker_operation_failed", other.to_string()),
    }
}

fn unavailable_runtime_error(id: &SessionId, state: RuntimeState) -> ProtocolError {
    let code = match state {
        RuntimeState::Lost => "session_runtime_lost",
        RuntimeState::Conflict => "session_runtime_conflict",
        RuntimeState::Incompatible => "worker_protocol_incompatible",
        RuntimeState::Starting | RuntimeState::Reconnecting => "session_runtime_reconnecting",
        RuntimeState::Terminal => "session_not_running",
        RuntimeState::Live => "worker_operation_failed",
    };
    runtime_error(
        code,
        format!("session {} runtime is {}", id.0, runtime_state_label(state)),
    )
}

fn runtime_state_label(state: RuntimeState) -> &'static str {
    match state {
        RuntimeState::Starting => "starting",
        RuntimeState::Live => "live",
        RuntimeState::Reconnecting => "reconnecting",
        RuntimeState::Terminal => "terminal",
        RuntimeState::Lost => "lost",
        RuntimeState::Conflict => "conflict",
        RuntimeState::Incompatible => "incompatible",
    }
}

fn runtime_error(code: impl Into<String>, msg: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorClass::Runtime, code, msg, None)
}

fn validate_observation_config(config: &SessionRegistryConfig) -> Result<(), ProtocolError> {
    let hard_wait_ceiling = Duration::from_millis(u64::from(protocol::MAX_SESSION_WAIT_MS));
    let invalid = config.observation_output_bytes == 0
        || config.observation_output_bytes > protocol::MAX_SESSION_OUTPUT_BYTES
        || config.observation_output_wait.is_zero()
        || config.observation_output_wait > hard_wait_ceiling
        || config.session_wait.is_zero()
        || config.session_wait > hard_wait_ceiling
        || config.observation_screen_rows == 0
        || config.observation_screen_cols == 0
        || config.observation_screen_bytes == 0
        || config.observation_screen_bytes > protocol::MAX_SESSION_SCREEN_RESPONSE_BYTES
        || config.observation_global_waiters == 0
        || config.observation_session_waiters == 0
        || config.observation_session_waiters > config.observation_global_waiters;
    if invalid {
        return Err(ProtocolError::new(
            ErrorClass::Configuration,
            "observation_limits_invalid",
            "daemon observation limits are zero, exceed protocol bounds, or conflict",
            Some("fix the daemon observation configuration and restart".to_owned()),
        ));
    }
    Ok(())
}

fn timestamp_now() -> String {
    now_rfc3339()
}

fn current_time_millis() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

#[cfg(test)]
fn test_worker_roots(config: &SessionRegistryConfig) -> (PathBuf, PathBuf) {
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    // State root (journals) has no path-length limit, so it can keep sharing
    // the metadata store's temp directory when the caller did not override
    // it explicitly.
    let state_base = config.store_path.as_ref().map_or_else(
        || {
            std::env::temp_dir().join(format!(
                "pohunek-daemon-worker-test-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ))
        },
        |store| {
            store
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("worker-test")
        },
    );

    // The runtime root holds the worker's Unix domain socket
    // (`<runtime_root>/<session_id>/control.sock`), whose path is bound by
    // `SUN_LEN` (108 bytes on Linux/BSD). The metadata store's temp
    // directory embeds a test tag plus a 19-digit nanosecond timestamp and
    // routinely overflows that budget for longer tags, so -- unlike the
    // state root -- the default runtime root always uses a short, unique
    // path directly under `temp_dir()`, independent of `store_path`.
    let runtime_root = config.worker_runtime_root.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!(
            "pw-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    });
    let state_root = config
        .worker_state_root
        .clone()
        .unwrap_or_else(|| state_base.join("state"));
    (runtime_root, state_root)
}

#[cfg(test)]
mod tests;
