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
    event, ActivityRevision, AgentActivity, AgentStateEvent, AttachEvent, CwdSource, ErrorClass,
    Event, ProjectRemoveResult, ProtocolError, RuntimeId, RuntimeInventoryEntry,
    RuntimeInventoryResult, RuntimeRef, RuntimeState, SessionAttachParams, SessionEvent,
    SessionForkParams, SessionId, SessionInfo, SessionInputParams, SessionInputResult,
    SessionInputWait, SessionNativeRecoveredEvent, SessionNewParams, SessionReleaseAgentParams,
    SessionReleaseAgentResult, SessionRemoveResult, SessionReportAgentParams,
    SessionReportAgentResult, SessionReportNativeIdParams, SessionReportNativeIdResult,
    SessionRuntime, SessionRuntimeIdentity, SessionSetMetadataResult, SessionState,
    SessionStopResult, SessionWarning, StateSource, SubagentInfo, SubagentLifecycle,
    SubagentRevision, UnconfirmedProcess, WorktreeRemoveResult, PROTOCOL_VERSION,
};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, info, warn};
use ulid::Ulid;

use crate::agent::host::{self, RuntimeDefinition, RuntimeHost};
use crate::agent::{
    agent_fork_unsupported, agent_not_resumable, build_pty_command, fork_pty_command_from_launch,
    resume_pty_command_from_launch, InputRules, LaunchCommand, LaunchOpts,
    NativeReferenceProvenance, NativeSessionLaunch, ProfileRegistry, ResolvedAgent, SessionRef,
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
use crate::procwatch::{ExitWatch, Pid, ProcessFact, ProcessIdentity, ProcessInspector};
use crate::project::detect::{project_id, DetectedProject};
use crate::project::{detect_at, ProjectManager};
use crate::runtime::lifecycle::{Generation, LifecycleGuard, SessionLocks};
use crate::runtime::{DimensionUpdate, SupervisionConfig, Worker, WorkerError, WorkerLauncher};
use crate::store::{
    DesiredState, ProjectRecord, ResumeBinding, RuntimeRecord, SessionRecord, SessionTransaction,
    SessionWriteOutcome, Store, TransactionKind, WorktreeStatus,
};
use crate::time::now_rfc3339;
use crate::worktree::{
    canonical_or_original, run_hook, HookContext, HookEvent, WorktreeCleanup, WorktreeManager,
    WorktreeRequest,
};
use reconcile::LossClassification;

mod attach;
mod detector;
mod diff;
mod hooks;
mod input;
mod lag;
mod native_repair;
mod observation;
mod package_lifecycle;
mod packages;
mod procwatch;
#[cfg(test)]
mod profile_pin_tests;
mod read;
mod reconcile;
mod resume;
mod retention;
mod supervision;
mod target;

pub use attach::{RedeemedAttach, RedeemedRuntime};
pub use package_lifecycle::HostTrustAnchor;
pub use retention::{SessionRetentionTask, POLICY_FILE_NAME as RETENTION_POLICY_NAME};

pub(crate) use observation::{observation_worker_error, runtime_identity};

use attach::{generate_daemon_instance_id, ActiveAttach, PendingAttach};
use hooks::SessionHookRequest;
#[cfg(test)]
use hooks::{parse_agent_activity, spawn_agent_state_hook_dispatcher};
#[cfg(test)]
use input::{build_input_writes, InputSubmission};
use input::{input_rules_for_agent, input_rules_for_definition, plan_initial_input_delivery};
use lag::{log_lag_warn, LagWarnThrottle};
use resume::ResumeSnapshot;
pub(crate) use target::ActiveSupervision;
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
/// Caps retry load while preserving eventual recovery of unchanged metadata.
const MAX_WORKER_METADATA_RETRY_DELAY: Duration = Duration::from_secs(5);
/// Prevents a persistent retryable failure from flooding daemon logs.
const WORKER_METADATA_RETRY_WARN_INTERVAL: Duration = Duration::from_secs(30);
/// Per-subscriber worker output queue. It absorbs repaint bursts without
/// duplicating the larger raw-history budget for every subscriber.
const DEFAULT_WORKER_SUBSCRIBER_BYTES: u64 = 1_000_000;
/// Number of completed input plans retained by a worker for deduplication.
const DEFAULT_WORKER_WRITE_DEDUP_ENTRIES: u32 = 4_096;
/// Maximum number of recent activity observations retained per session.
///
/// The time window is authoritative for ordinary report rates; the hard cap
/// prevents a misbehaving local hook from growing daemon memory without bound.
const MAX_ACTIVITY_EVIDENCE_HISTORY: usize = 4_096;
/// Keeps evidence through the longest public input-wait deadline.
const ACTIVITY_EVIDENCE_RETENTION: Duration =
    Duration::from_millis(protocol::MAX_SESSION_WAIT_MS as u64);
/// Final runtime retention while no daemon is present.
const DEFAULT_WORKER_TERMINAL_RETENTION: Duration = Duration::from_hours(24);
/// Durable logical-session record schema; every persisted record kind shares the
/// store-wide schema version.
const SESSION_RECORD_SCHEMA_VERSION: u32 = crate::store::STORE_SCHEMA_VERSION;
/// Bound on how long a graceful shutdown waits for the event-log drain to flush
/// its backlog, so a wedged log write can never hang shutdown.
const EVENT_LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Default bound on how long daemon shutdown waits for in-flight create
/// transactions ([`SessionRegistryConfig::create_drain_timeout`]).
///
/// It stays well inside the daemon's native stop timeout (30 s by default),
/// so the remaining shutdown steps still fit. A create still running at the
/// deadline is recovered by startup reconciliation from its durable record.
const DEFAULT_CREATE_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

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
/// human label (it renders in a single client row and one CLI table cell) while
/// bounding the per-session state the daemon stores and persists in the resume
/// binding. A name is cosmetic, so the limit can change freely.
const MAX_SESSION_NAME_BYTES: usize = 128;

/// Shell command configuration used for the shell runtime.
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

impl ShellCommand {
    /// The program the shell runtime launches.
    pub(crate) fn program(&self) -> &str {
        &self.program
    }

    /// The arguments the shell runtime launches its program with.
    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }
}

impl Default for ShellCommand {
    fn default() -> Self {
        Self::from_login_shell(std::env::var("SHELL").ok())
    }
}

impl ShellCommand {
    /// Build the login-shell command from a raw `$SHELL` value, falling back
    /// to the daemon's default shell when it is unusable.
    pub(crate) fn from_login_shell(raw: Option<String>) -> Self {
        Self::new(
            crate::agent::resolve_login_shell(raw),
            std::iter::empty::<String>(),
        )
    }
}

/// Runtime configuration for the in-memory registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRegistryConfig {
    /// Command used for the shell runtime.
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
    /// Submit delay per runtime id, replacing the descriptor's delay for
    /// runtimes whose descriptor marks it configurable.
    pub submit_delay_overrides: BTreeMap<RuntimeId, Duration>,
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
    /// Durable session policy document (`<data_dir>/session-policy.json`).
    /// `None` keeps the retention policy in memory only, which is what unit
    /// tests that never persist a policy use.
    pub retention_policy_path: Option<PathBuf>,
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
    /// Application state directory whose host-state subdirectory holds the
    /// secret that keys profile revisions. `None` makes revisions unavailable.
    pub host_state_dir: Option<PathBuf>,
    /// Owner-private runtime package store (`<state>/plugins`). `None` serves
    /// the built-in runtimes only, with no package store to reload or verify.
    pub plugins_dir: Option<PathBuf>,
    /// Root keys and revocations the host trusts to authorize official
    /// packages through a signed catalog. `None` means the host has no anchor,
    /// so every catalog install fails closed with `official_trust_unavailable`.
    pub catalog_trust_anchor: Option<HostTrustAnchor>,
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
    /// Bound on a worker generation's socket negotiation, on reconnecting to
    /// a live worker, and on startup discovery.
    pub worker_connect_deadline: Duration,
    /// Worker job supervision (definition inputs and deadlines).
    ///
    /// `None` disables session launch with `worker_backend_required`; there is
    /// no daemon-owned PTY fallback.
    pub supervision: Option<SupervisionConfig>,
    /// Bound on how long [`SessionRegistry::drain_creates`] waits for the
    /// create transactions still running when daemon shutdown starts.
    /// Defaults to [`DEFAULT_CREATE_DRAIN_TIMEOUT`].
    pub create_drain_timeout: Duration,
}

impl Default for SessionRegistryConfig {
    fn default() -> Self {
        Self {
            shell_command: ShellCommand::default(),
            stop_grace: pohunek_service_config::DEFAULT_STOP_GRACE,
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
            submit_delay_overrides: BTreeMap::new(),
            initial_input_startup_grace: DEFAULT_INITIAL_INPUT_STARTUP_GRACE,
            socket_path: None,
            store_path: None,
            worktree_root: None,
            hook_timeout: DEFAULT_HOOK_TIMEOUT,
            event_log_dir: None,
            retention_policy_path: None,
            log_dir: None,
            config_dir: None,
            agents_dir: None,
            host_state_dir: None,
            plugins_dir: None,
            catalog_trust_anchor: None,
            detector_lag_warn_interval: DEFAULT_DETECTOR_LAG_WARN_INTERVAL,
            procwatch_poll: DEFAULT_PROCWATCH_POLL,
            active_agent_claim_ttl: DEFAULT_ACTIVE_AGENT_CLAIM_TTL,
            observe_external_agents: false,
            worker_runtime_root: None,
            worker_state_root: None,
            worker_connect_deadline: DEFAULT_WORKER_CONNECT_DEADLINE,
            supervision: None,
            create_drain_timeout: DEFAULT_CREATE_DRAIN_TIMEOUT,
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
    /// Set by startup reconciliation when legacy resume bindings exist with
    /// no logical record and no migration manifest. The first logical record
    /// would make a later manifest unimportable, so every path that creates
    /// one is refused while it holds ([`SessionRegistry::ensure_migration_settled`]).
    legacy_migration_pending: AtomicBool,
    /// Cancellation signal fired when production daemon shutdown starts.
    ///
    /// Bounded input operations use this token instead of the event-log drain
    /// token so shutdown cancellation does not depend on later flush ordering.
    daemon_shutdown: CancellationToken,
    /// Opaque id unique to this daemon process instance, injected into every
    /// session PTY as `POHUNEK_DAEMON_ID` and compared against the attach origin
    /// so the self-feeding-attach guard fires only for this instance's own PTYs
    /// (see [`SessionRegistry::attach`]). Regenerated each start; never persisted.
    daemon_instance_id: String,
    config: SessionRegistryConfig,
    /// Worker job supervisor; `None` when the registry cannot launch workers.
    launcher: Option<Arc<dyn WorkerLauncher>>,
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
    /// Shared by every fresh launch from verification until its durable record
    /// exists, exclusive for a package uninstall, so a launch cannot pin a
    /// package digest between the retained set being computed and the
    /// package being removed.
    package_lifecycle: Arc<tokio::sync::RwLock<()>>,
    /// Serializes create, native recovery, stop, and remove per session, so
    /// one session never runs two lifecycle transactions at once.
    lifecycle_locks: SessionLocks,
    /// Detached create transactions, drained at daemon shutdown.
    creates: CreateTasks,
    /// Sessions whose native supervisor could not be inspected, re-reconciled
    /// in the background until it answers.
    supervision_retries: supervision::SupervisionRetries,
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
    /// Automatic session retention policy and its sweep serialization point.
    retention: retention::RetentionState,
    #[cfg(test)]
    external_association_block: std::sync::Mutex<Option<Arc<ExternalAssociationBlock>>>,
    /// Rendezvous the next waited input write meets at its send boundary.
    #[cfg(test)]
    input_send_hold: std::sync::Mutex<Option<Arc<tokio::sync::Barrier>>>,
    /// Holds the next create once its target is bound, before its launch.
    #[cfg(test)]
    bound_create_hold: std::sync::Mutex<Option<crate::runtime::lifecycle::tests::StartGate>>,
    /// Holds the next committed create before its initial input.
    #[cfg(test)]
    initial_input_hold: std::sync::Mutex<Option<crate::runtime::lifecycle::tests::StartGate>>,
    /// Holds the next create between its delivered input and the commit.
    #[cfg(test)]
    initial_input_commit_hold:
        std::sync::Mutex<Option<crate::runtime::lifecycle::tests::StartGate>>,
    /// Holds the next undelivered-create conversion before its write.
    #[cfg(test)]
    undelivered_conversion_hold:
        std::sync::Mutex<Option<crate::runtime::lifecycle::tests::StartGate>>,
    /// Default worker roots of a test registry, removed when the last clone
    /// of the registry drops.
    #[cfg(test)]
    test_dirs: std::sync::Mutex<Vec<tempfile::TempDir>>,
}

/// The detached create transactions of one registry.
///
/// Admission and closing share one lock, so a create is either tracked
/// before [`SessionRegistry::begin_daemon_shutdown`] closes the tracker, and
/// so drained, or refused.
#[derive(Debug, Default)]
struct CreateTasks {
    tracker: TaskTracker,
    admission: std::sync::Mutex<()>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct ExternalAssociationBlock {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[derive(Debug, Clone)]
struct SessionEntry {
    info: SessionInfo,
    /// Monotonic in-memory cursor advanced for every emitted activity report.
    activity_revision: u64,
    /// Bounded runtime-scoped activity history for input-wait evidence.
    activity_evidence: VecDeque<ActivityEvidence>,
    /// Serializes complete input framing transactions for this logical session.
    input_gate: Arc<Mutex<()>>,
    runtime: RuntimeHandle,
    /// Worker job of the current runtime, when the record names one.
    job: Option<Generation>,
    desired_state: DesiredState,
    detector_cancel: CancellationToken,
    detector_resize: watch::Sender<(u16, u16)>,
    detector_config: watch::Sender<DetectorConfigUpdate>,
    detector_preview: mpsc::Sender<DetectionPreviewRequest>,
    default_detector_config: DetectorConfig,
    /// Definition of the installed package this session was launched from,
    /// loaded and verified when its runtime was registered. Callbacks and
    /// observation of the session's own runtime use it instead of whatever the
    /// registry serves now. `None` for a built-in runtime or a package that
    /// did not verify.
    pinned: Option<Arc<RuntimeDefinition>>,
    procwatch_cancel: CancellationToken,
    runtime_watch_cancel: CancellationToken,
    procwatch_rescan: Arc<Notify>,
    stopping: bool,
    stop_transaction_id: Option<String>,
    /// Resolved input-framing rules (base-kind defaults, profile-overridden), used
    /// by `session.input` so a profile's `[input_rules]` is honored on every write.
    input_rules: InputRules,
    /// Frozen structural relaunch snapshot (C.4), set once at register time and
    /// copied verbatim into every persisted [`ResumeBinding`] — so a resize-driven
    /// re-persist can never overwrite the launch-time program/args/resume shape.
    snapshot: ResumeSnapshot,
    active_agent: Option<ActiveAgentReport>,
    foreground_process_group: Option<Pid>,
    last_agent_report: Option<ActiveAgentReport>,
    last_native_report: Option<NativeIdentityReport>,
    observed_agents: Vec<ObservedAgent>,
    /// When the evidence that set `info.cwd` was observed. Older cwd evidence
    /// never replaces it, so a scan that read a process before a newer OSC 7
    /// hint landed cannot move the session back.
    cwd_observed_at: Instant,
    /// Daemon instance holding the create's undelivered initial input; the
    /// running session's record then carries the `initial_input` marker
    /// naming that instance.
    initial_input_owner: Option<String>,
}

impl SessionEntry {
    /// Stops the detector, process watcher, and worker exit watcher of this
    /// entry's runtime.
    ///
    /// All three watch the same runtime identity, so a re-adopted runtime
    /// would otherwise run beside them; stopping them keeps at most one
    /// watcher set applying updates to a session.
    fn cancel_runtime_watchers(&self) {
        self.detector_cancel.cancel();
        self.procwatch_cancel.cancel();
        self.runtime_watch_cancel.cancel();
    }
}

#[derive(Debug, Clone)]
struct DetectorConfigUpdate {
    generation: u64,
    config: DetectorConfig,
}

#[derive(Debug)]
struct DetectionPreviewRequest {
    minimum_config_generation: u64,
    reply: oneshot::Sender<Result<Vec<protocol::DetectionRegionPreview>, protocol::ProtocolError>>,
}

struct DetectorInputs {
    scope: DetectorScope,
    output: broadcast::Receiver<Vec<u8>>,
    initial_size: (u16, u16),
    cancel: CancellationToken,
    resize: watch::Receiver<(u16, u16)>,
    config: watch::Receiver<DetectorConfigUpdate>,
    preview: mpsc::Receiver<DetectionPreviewRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActivityEvidence {
    activity: AgentActivity,
    source: StateSource,
    runtime: SessionRuntimeIdentity,
    activity_epoch: String,
    revision: ActivityRevision,
    observed_at: tokio::time::Instant,
}

impl ActivityEvidence {
    fn event(&self, session_id: SessionId) -> AgentStateEvent {
        AgentStateEvent {
            session_id,
            activity: self.activity,
            source: self.source,
            runtime: Some(self.runtime.clone()),
            activity_epoch: Some(self.activity_epoch.clone()),
            revision: Some(self.revision),
        }
    }
}

fn record_activity_evidence(
    entry: &mut SessionEntry,
    activity: AgentActivity,
    source: StateSource,
    activity_epoch: &str,
) -> Option<ActivityEvidence> {
    entry.activity_revision = entry
        .activity_revision
        .checked_add(1)
        .expect("session activity revision overflowed");
    let runtime = entry.info.runtime.as_ref().and_then(|runtime| {
        if runtime.state != RuntimeState::Live {
            return None;
        }
        SessionRuntimeIdentity::new(
            runtime.worker_instance_id.clone()?,
            runtime.runtime_generation,
        )
        .ok()
    })?;
    let evidence = ActivityEvidence {
        activity,
        source,
        runtime,
        activity_epoch: activity_epoch.to_owned(),
        revision: ActivityRevision::new(entry.activity_revision),
        observed_at: tokio::time::Instant::now(),
    };
    if let Some(cutoff) = evidence
        .observed_at
        .checked_sub(ACTIVITY_EVIDENCE_RETENTION)
    {
        while entry
            .activity_evidence
            .front()
            .is_some_and(|previous| previous.observed_at < cutoff)
        {
            entry.activity_evidence.pop_front();
        }
    }
    entry.activity_evidence.push_back(evidence.clone());
    while entry.activity_evidence.len() > MAX_ACTIVITY_EVIDENCE_HISTORY {
        entry.activity_evidence.pop_front();
    }
    Some(evidence)
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
    start_identity: Option<u64>,
    reported_at: Instant,
    activity_reported: bool,
}

type NativeIdentityReport = crate::store::NativeIdentityOrdering;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedAgent {
    pid: Pid,
    pgid: Pid,
    start_identity: u64,
    agent_base: RuntimeRef,
    first_seen: Instant,
    cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeExit {
    exit_code: Option<i32>,
    success: bool,
}

/// Whether a removal may proceed when its marker sweep is unconfirmed only
/// because same-user processes with unreadable environments may belong to the
/// removed runtimes.
///
/// Only an explicit caller request selects [`Self::Accept`], and it covers a
/// single removal: nothing stores it, so a retried removal, the
/// reconciliation finalizer, and the retention sweep all use
/// [`Self::Refuse`]. Every other reason a sweep is unconfirmed refuses under
/// both variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnconfirmedCleanup {
    /// Refuse the removal whenever a sweep cannot prove its runtime gone.
    Refuse,
    /// Proceed past unreadable-marker candidates, never signalling them, and
    /// report them to the caller.
    Accept,
}

impl From<bool> for UnconfirmedCleanup {
    fn from(accept: bool) -> Self {
        if accept {
            Self::Accept
        } else {
            Self::Refuse
        }
    }
}

/// What [`SessionRegistry::release_removed_session`] cleaned up.
#[derive(Debug)]
struct ReleasedSession {
    /// Whether a listed entry left the registry.
    evicted: bool,
    /// The owned worktrees the removal deleted or left behind.
    worktrees: WorktreeCleanup,
    /// Unreadable-marker processes the removal accepted without proving
    /// them gone.
    accepted_unconfirmed: Vec<UnconfirmedProcess>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeWatchIdentity {
    worker_id: String,
    worker_instance_id: String,
    generation: protocol::RuntimeGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetectorScope {
    id: SessionId,
    runtime: RuntimeWatchIdentity,
}

impl RuntimeWatchIdentity {
    fn from_info(info: &SessionInfo) -> Option<Self> {
        let runtime = info.runtime.as_ref()?;
        Some(Self {
            worker_id: runtime.worker_id.clone()?,
            worker_instance_id: runtime.worker_instance_id.clone()?,
            generation: runtime.runtime_generation,
        })
    }

    fn matches(&self, entry: &SessionEntry) -> bool {
        entry.info.runtime.as_ref().is_some_and(|runtime| {
            runtime.worker_id.as_deref() == Some(self.worker_id.as_str())
                && runtime.worker_instance_id.as_deref() == Some(self.worker_instance_id.as_str())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeMetadataPolicy {
    Live,
    Terminal,
}

struct ExitTransition {
    event: &'static str,
    stop_reason: &'static str,
    cancel_attaches: bool,
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
    candidate.stop_transaction_id = None;
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
    terminalize_running_subagents(&mut candidate.info.subagents, current_time_millis());
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
        cancel_attaches: stopped,
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

    /// Clean the worktrees owned by `id`, returning the setup warnings and how
    /// many checkouts are really gone.
    ///
    /// A `git worktree remove` failure is non-fatal, so the count is what
    /// [`SessionRegistry::remove`] reports rather than the number of bindings it
    /// processed.
    async fn cleanup_owned_worktrees_for_removal(
        &self,
        id: &SessionId,
    ) -> Result<(Vec<SessionWarning>, WorktreeCleanup), ProtocolError> {
        let Some(worktree) = self.inner.worktree.clone() else {
            return Ok((Vec::new(), WorktreeCleanup::default()));
        };
        let session_id = id.0.clone();
        tokio::task::spawn_blocking(move || {
            let mut warnings = Vec::new();
            let cleanup = worktree.cleanup_session(&session_id, &mut warnings)?;
            Ok((warnings, cleanup))
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
        let mut runtime = entry.info.runtime.as_ref().map_or(
            RuntimeRecord {
                state: RuntimeState::Live,
                worker_id: None,
                worker_instance_id: None,
                service_id: None,
                generation: None,
                executable: None,
                reason: None,
            },
            |runtime| RuntimeRecord {
                state: runtime.state,
                worker_id: runtime.worker_id.clone(),
                worker_instance_id: runtime.worker_instance_id.clone(),
                service_id: None,
                generation: None,
                executable: None,
                reason: runtime.loss_reason.clone(),
            },
        );
        if let Some(job) = &entry.job {
            job.record_into(&mut runtime);
        }
        let transaction = transaction.or_else(|| {
            let owner = entry
                .initial_input_owner
                .as_ref()
                .filter(|_| desired_state == DesiredState::Running)?;
            Some(SessionTransaction {
                id: format!("create-{}", id.0),
                kind: TransactionKind::Create,
                phase: crate::store::INITIAL_INPUT_PHASE.to_owned(),
                previous_worker_id: None,
                previous_worker_instance_id: None,
                daemon_instance_id: Some(owner.clone()),
            })
        });
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
    /// the real worker server through a test-only launcher; other builds get a
    /// registry without a worker supervisor, whose launches fail with
    /// `worker_backend_required`. No constructor falls back to daemon-owned
    /// PTYs.
    ///
    /// Unit tests observe the host through the test-only `ReadableHost` view,
    /// so an unrelated process of the test host whose ownership markers cannot
    /// be read never makes a removal sweep unconfirmed. A unit test that exercises
    /// unreadable markers injects its inspector through
    /// [`Self::new_with_inspector`].
    #[must_use]
    pub fn new(config: SessionRegistryConfig) -> Self {
        #[cfg(test)]
        let inspector: Arc<dyn ProcessInspector> =
            Arc::new(crate::procwatch::readable_host::ReadableHost::new());
        #[cfg(not(test))]
        let inspector: Arc<dyn ProcessInspector> = Arc::new(crate::procwatch::HostInspector::new());
        Self::new_with_inspector(config, inspector)
    }

    /// Create a production registry with a mandatory durable-worker backend.
    ///
    /// # Errors
    ///
    /// Returns `worker_backend_required` when the per-session worker roots or
    /// the supervision configuration are absent. Production never falls back
    /// to daemon-owned PTYs.
    pub fn new_production(
        config: SessionRegistryConfig,
        supervisor: Arc<dyn WorkerLauncher>,
    ) -> Result<Self, ProtocolError> {
        validate_observation_config(&config)?;
        if config.worker_runtime_root.is_none()
            || config.worker_state_root.is_none()
            || config.supervision.is_none()
        {
            return Err(runtime_error(
                "worker_backend_required",
                "production session registry requires durable worker roots and supervision",
            ));
        }
        let runtimes = config
            .plugins_dir
            .as_deref()
            .map(open_runtime_host)
            .transpose()?;
        Ok(Self::build(
            config,
            Some(supervisor),
            Arc::new(crate::procwatch::HostInspector::new()),
            runtimes,
        ))
    }

    /// Create a registry with an injected process inspector.
    ///
    /// Production uses [`crate::procwatch::HostInspector`]. Tests use this to
    /// drive process facts and exit events deterministically without touching the
    /// host process table.
    #[must_use]
    pub fn new_with_inspector(
        config: SessionRegistryConfig,
        inspector: Arc<dyn ProcessInspector>,
    ) -> Self {
        #[cfg(test)]
        {
            Self::new_for_test(config, inspector, None, None)
        }
        #[cfg(not(test))]
        {
            Self::build(config, None, inspector, None)
        }
    }

    /// Creates a unit-test registry over in-process workers, optionally
    /// serving `runtimes` instead of the built-in runtime set.
    #[cfg(test)]
    fn new_for_test(
        config: SessionRegistryConfig,
        inspector: Arc<dyn ProcessInspector>,
        runtimes: Option<RuntimeHost>,
        environment: Option<(crate::runtime::EnvironmentSource, Vec<String>)>,
    ) -> Self {
        let mut config = config;
        let (runtime_root, state_root, test_dirs) = owned_test_worker_roots(&config);
        config.worker_runtime_root = Some(runtime_root.clone());
        config.worker_state_root = Some(state_root.clone());
        if config.supervision.is_none() {
            config.supervision = Some(test_supervision(&runtime_root, &state_root));
        }
        if let (Some((source, allowlist)), Some(supervision)) =
            (environment, config.supervision.as_mut())
        {
            supervision.environment_source = source;
            supervision.environment_allowlist = allowlist;
        }
        let launcher = Arc::new(crate::runtime::InProcessWorkerLauncher::new(
            runtime_root,
            state_root,
        ));
        let registry = Self::build(config, Some(launcher), inspector, runtimes);
        *registry
            .inner
            .test_dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = test_dirs;
        registry
    }

    /// Creates a unit-test registry that serves `runtimes` (for example a
    /// non-built-in definition) next to or instead of the built-in set.
    #[cfg(test)]
    pub(crate) fn new_with_runtimes(config: SessionRegistryConfig, runtimes: RuntimeHost) -> Self {
        Self::new_for_test(
            config,
            Arc::new(crate::procwatch::readable_host::ReadableHost::new()),
            Some(runtimes),
            None,
        )
    }

    /// [`Self::new_with_runtimes`] with the base-environment source and
    /// allowlist workers launch their agents with.
    #[cfg(test)]
    pub(crate) fn new_with_runtimes_and_environment(
        config: SessionRegistryConfig,
        runtimes: RuntimeHost,
        source: crate::runtime::EnvironmentSource,
        allowlist: Vec<String>,
    ) -> Self {
        Self::new_for_test(
            config,
            Arc::new(crate::procwatch::readable_host::ReadableHost::new()),
            Some(runtimes),
            Some((source, allowlist)),
        )
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
        Self::build(config, Some(launcher), inspector, None)
    }

    fn build(
        config: SessionRegistryConfig,
        launcher: Option<Arc<dyn WorkerLauncher>>,
        inspector: Arc<dyn ProcessInspector>,
        runtimes: Option<RuntimeHost>,
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
        // The configured shell command decides what a bare shell session launches.
        let runtimes = runtimes
            .unwrap_or_else(RuntimeHost::from_host_environment)
            .with_shell_command(
                config.shell_command.program(),
                config.shell_command.args().to_vec(),
            );
        let profiles = ProfileRegistry::with_runtimes(config.agents_dir.clone(), runtimes)
            .with_revision_state_dir(config.host_state_dir.clone());
        let retention = retention::RetentionState::new(config.retention_policy_path.clone());
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
                legacy_migration_pending: AtomicBool::new(false),
                daemon_shutdown: CancellationToken::new(),
                daemon_instance_id: generate_daemon_instance_id(),
                config,
                launcher,
                profiles,
                events,
                store,
                persist_lock: Mutex::new(()),
                package_lifecycle: Arc::new(tokio::sync::RwLock::new(())),
                lifecycle_locks: SessionLocks::default(),
                creates: CreateTasks::default(),
                supervision_retries: supervision::SupervisionRetries::default(),
                worktree,
                projects,
                event_log_shutdown: CancellationToken::new(),
                event_log_task: std::sync::Mutex::new(None),
                agent_state_hook_shutdown: CancellationToken::new(),
                agent_state_hook_task: std::sync::Mutex::new(None),
                inspector,
                procwatch_seq: AtomicU64::new(current_time_millis()),
                external: external.clone(),
                retention,
                #[cfg(test)]
                external_association_block: std::sync::Mutex::new(None),
                #[cfg(test)]
                input_send_hold: std::sync::Mutex::new(None),
                #[cfg(test)]
                bound_create_hold: std::sync::Mutex::new(None),
                #[cfg(test)]
                initial_input_hold: std::sync::Mutex::new(None),
                #[cfg(test)]
                initial_input_commit_hold: std::sync::Mutex::new(None),
                #[cfg(test)]
                undelivered_conversion_hold: std::sync::Mutex::new(None),
                #[cfg(test)]
                test_dirs: std::sync::Mutex::new(Vec::new()),
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
        self.inner.daemon_shutdown.cancel();
        self.close_creates();
        if !already_started {
            info!("daemon shutdown started; preserving durable worker runtimes");
        }
        self.inner.external.shutdown();
    }

    /// Refuses every later create; the ones already admitted stay tracked.
    fn close_creates(&self) {
        let _admission = self
            .inner
            .creates
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner.creates.tracker.close();
    }

    /// Refuses new creates and waits for the ones still running, bounded by
    /// [`SessionRegistryConfig::create_drain_timeout`].
    ///
    /// Each create commits its session or compensates what it bound before
    /// the runtime stops, instead of being dropped mid-transaction. Daemon
    /// shutdown calls it after [`Self::begin_daemon_shutdown`]. Returns
    /// whether every create finished; one still running at the deadline is
    /// logged and left to startup reconciliation, which compensates it from
    /// its durable create record.
    pub async fn drain_creates(&self) -> bool {
        self.close_creates();
        let creates = &self.inner.creates.tracker;
        let timeout = self.inner.config.create_drain_timeout;
        if tokio::time::timeout(timeout, creates.wait()).await.is_ok() {
            return true;
        }
        warn!(
            session.creates_in_flight = creates.len(),
            timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            "create transactions did not finish within the shutdown drain timeout; startup reconciliation settles them"
        );
        false
    }

    /// Refuses a lifecycle operation that would write the first logical
    /// record while unmigrated legacy resume bindings exist.
    ///
    /// Startup imports a migration manifest only into a store without logical
    /// records, so a record written first would archive a later manifest
    /// unimported and lose those bindings. Startup reconciliation decides the
    /// gate; a later start that imports the manifest lifts it.
    ///
    /// # Errors
    ///
    /// Returns `migration_manifest_missing` while the gate holds, before
    /// anything is written.
    pub(super) fn ensure_migration_settled(&self) -> Result<(), ProtocolError> {
        if !self.inner.legacy_migration_pending.load(Ordering::Acquire) {
            return Ok(());
        }
        Err(ProtocolError::new(
            ErrorClass::Runtime,
            reconcile::MIGRATION_MANIFEST_MISSING,
            "legacy resume bindings exist without a migration manifest; creating a session now would lose them",
            Some(
                "run `pohunek migration preflight` against the legacy daemon (reinstall the legacy release and rerun the worker-aware installer), then restart this daemon so it imports the manifest"
                    .to_owned(),
            ),
        ))
    }

    /// Runs `transaction` detached from the caller and tracked for the
    /// shutdown drain.
    ///
    /// # Errors
    ///
    /// Returns `daemon_shutting_down` once daemon shutdown started, before
    /// anything of the create is written.
    fn spawn_create<F>(&self, transaction: F) -> Result<JoinHandle<F::Output>, ProtocolError>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let _admission = self
            .inner
            .creates
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.inner.creates.tracker.is_closed() {
            return Err(ProtocolError::new(
                ErrorClass::Daemon,
                "daemon_shutting_down",
                "the daemon is shutting down and accepts no new sessions",
                Some("retry once the daemon is running again".to_owned()),
            ));
        }
        Ok(self.inner.creates.tracker.spawn(transaction))
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
            return Err(ProtocolError::worktree_in_use());
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
    ///
    /// # Errors
    ///
    /// Returns `migration_manifest_missing` while unmigrated legacy resume
    /// bindings exist and `daemon_shutting_down` once shutdown started, both
    /// before anything is written, and otherwise the validation, target, or launch error. A
    /// failed create whose compensation cannot finish stays listed as
    /// `create_compensation_pending` for the supervision retry.
    pub async fn create(&self, params: SessionNewParams) -> Result<SessionInfo, ProtocolError> {
        self.ensure_migration_settled()?;
        validate_new_params(&params)?;
        // Resolve and validate the runtime before allocating a logical id or
        // resolving a target: target resolution may bind a git worktree.
        let resolved = host::LaunchSource::OwnerLocal {
            name: params.agent.clone(),
        }
        .resolve(&self.inner.profiles)?;
        let profile_env = resolved
            .profile
            .as_ref()
            .map_or(&[][..], |profile| profile.env.as_slice());
        let launch_path = self.probe_search_path(&resolved.definition, profile_env)?;
        let validated_program = host::validate_launch_runtime(
            &resolved.definition,
            resolved.program(),
            launch_path.as_deref(),
        )?;
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
        self.ensure_worker_socket(&id)?;
        // The durable intent, target binding, registration, initial input, and
        // every compensation run as one detached task, so a client dropped
        // after the worktree was bound cannot strand it: the task still
        // commits the session or compensates exactly what it created, and
        // daemon shutdown drains it.
        let registry = self.clone();
        self.spawn_create(async move {
            let (info, pending_initial_input) = Box::pin(registry.create_transaction(
                id,
                params,
                resolved,
                validated_program,
                fallback_cwd,
            ))
            .await?;
            registry.complete_create(info, pending_initial_input).await
        })?
        .await
        .map_err(|_join_error| {
            runtime_error("session_launch_failed", "session create task panicked")
        })?
    }

    /// Finishes a committed create: fires its `SessionStart` hook and
    /// delivers its initial input.
    ///
    /// Until the input is delivered, the session's record carries the
    /// `initial_input` marker, so a daemon that stops or dies first leaves a
    /// create that reconciliation rolls back instead of a session running
    /// without its prompt. Delivery clears the marker durably; a failed
    /// delivery or clearing removes the session ([`Self::remove`]), whose
    /// durable removal intent reconciliation finishes as well.
    ///
    /// # Errors
    ///
    /// Returns the delivery or persistence error after the rollback.
    async fn complete_create(
        &self,
        info: SessionInfo,
        pending_initial_input: Option<String>,
    ) -> Result<SessionInfo, ProtocolError> {
        // A reference assigned at launch makes the session resumable at once.
        if info.native_session_id.is_some() {
            self.persist_resume_binding(&info.id).await;
        }
        self.spawn_session_hook(SessionHookRequest {
            event: HookEvent::SessionStart,
            cwd: info.cwd.clone(),
            session_id: info.id.0.clone(),
            project_id: info.project_id.clone(),
            agent: info.agent.clone(),
            stop_reason: None,
            activity: None,
        });
        let Some(input) = pending_initial_input else {
            return Ok(info);
        };
        #[cfg(test)]
        self.hold_initial_input(&info.id).await;
        // Wait for the agent to come up before injecting the first prompt so
        // the bytes are not delivered to a stdin reader that has not yet
        // entered raw/bracketed-paste mode (and would drop or mis-frame
        // them). Bounded, so a silent agent can never wedge `session.new`.
        self.await_initial_input_readiness(&info.id).await;
        let delivered = match self.write_input_to_session(&info.id, &input).await {
            Ok(()) => self.commit_initial_input(&info.id).await,
            Err(error) => Err(error),
        };
        if let Err(error) = delivered {
            self.rollback_failed_initial_input(&info.id).await;
            return Err(error);
        }
        Ok(info)
    }

    /// Clears the `initial_input` marker of a delivered create, durably
    /// first and then in memory.
    ///
    /// The store clears only the marker of the current record
    /// ([`Store::clear_initial_input`]), so a stop, removal, or exit that
    /// persisted meanwhile keeps its state. A writer that still builds the
    /// marker from memory before the flag drops cannot re-add it, since the
    /// store never re-introduces a cleared marker.
    async fn commit_initial_input(&self, id: &SessionId) -> Result<(), ProtocolError> {
        #[cfg(test)]
        self.hold_initial_input_commit(id).await;
        if let Some(store) = self.inner.store.clone() {
            let session_id = id.0.clone();
            tokio::task::spawn_blocking(move || store.clear_initial_input(&session_id))
                .await
                .map_err(|_join_error| {
                    runtime_error(
                        "session_store_failed",
                        format!("initial input commit task panicked for {}", id.0),
                    )
                })?
                .map_err(|error| {
                    runtime_error(
                        "session_store_failed",
                        format!("failed to commit the initial input of {}: {error}", id.0),
                    )
                })?;
        }
        if let Some(entry) = self.inner.sessions.lock().await.get_mut(id) {
            entry.initial_input_owner = None;
        }
        Ok(())
    }

    /// Parks a create between its delivered input and the marker commit
    /// when a test armed the hold.
    #[cfg(test)]
    async fn hold_initial_input_commit(&self, id: &SessionId) {
        let gate = self
            .inner
            .initial_input_commit_hold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_if(|gate| gate.session_id.as_deref().is_none_or(|held| held == id.0));
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
    }

    /// Parks a create before its initial input when a test armed the hold.
    #[cfg(test)]
    async fn hold_initial_input(&self, id: &SessionId) {
        let gate = self
            .inner
            .initial_input_hold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_if(|gate| gate.session_id.as_deref().is_none_or(|held| held == id.0));
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
    }

    /// Records the intent of a `session.new`, binds its target, and registers
    /// its first runtime, compensating whatever it created when that fails.
    ///
    /// Runs detached from the caller, under the session's lifecycle lock, in
    /// this order (the phases of RFC section 14):
    ///
    /// 1. persist the create intent: a `create/preparing` record without a
    ///    worker generation ([`create_intent_record`]), so every worktree the
    ///    create binds is reachable from a record;
    /// 2. resolve the project and bind the worktree;
    /// 3. build the launch command;
    /// 4. register the runtime ([`Self::run_pty_registration`]), which
    ///    persists the generation before starting its job and owns every
    ///    later compensation: the worktree and the preparing record are
    ///    removed only once the generation is proven ended, and a generation
    ///    that may still run keeps both for reconciliation.
    ///
    /// A failure in phases 2 or 3 launched nothing, so the create is
    /// compensated at once ([`Self::compensate_abandoned_create`]): worktree
    /// and binding first, record last. A compensation that cannot finish
    /// keeps the record and lists the session `create_compensation_pending`
    /// while the supervision retry repeats it; the create still fails with
    /// its original error. A daemon that dies anywhere after phase 1 leaves
    /// the record for startup reconciliation.
    ///
    /// Returns the committed session and the initial input still to deliver.
    async fn create_transaction(
        &self,
        id: SessionId,
        params: SessionNewParams,
        resolved: ResolvedAgent,
        validated_program: Option<crate::agent::ValidatedLaunchProgram>,
        fallback_cwd: PathBuf,
    ) -> Result<(SessionInfo, Option<String>), ProtocolError> {
        // Verified and held before anything is persisted, so a package that
        // fails verification launches nothing and leaves no record.
        let _package_launch = self.guard_package_launch(&resolved.definition).await?;
        let guard = self.lock_lifecycle(&id).await;
        let intent = create_intent_record(&id, &params, &resolved, &fallback_cwd)?;
        self.write_session_record(intent).await?;
        let prepared = match self.resolve_target(&id, &params, fallback_cwd).await {
            Ok(target) => {
                #[cfg(test)]
                self.hold_bound_create(&id).await;
                self.create_spec(&id, &params, &resolved, validated_program, target)
            }
            Err(error) => Err(error),
        };
        let (spec, pending_initial_input) = match prepared {
            Ok(prepared) => prepared,
            Err(mut error) => {
                if !self.compensate_abandoned_create(&id).await {
                    error.msg = unfinished_compensation_message(&error.msg, &id);
                }
                return Err(error);
            }
        };
        let info = self.clone().run_pty_registration(spec, guard).await?;
        Ok((info, pending_initial_input))
    }

    /// Parks a create at its bound target when a test armed the hold.
    #[cfg(test)]
    async fn hold_bound_create(&self, id: &SessionId) {
        let gate = {
            let mut slot = self
                .inner
                .bound_create_hold
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let held = slot
                .as_ref()
                .is_some_and(|gate| gate.session_id.as_deref().is_none_or(|held| held == id.0));
            if held {
                slot.take()
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
    }

    /// Builds the first-launch registration of a `session.new` in `target`,
    /// with the initial input its launch command does not carry.
    fn create_spec(
        &self,
        id: &SessionId,
        params: &SessionNewParams,
        resolved: &ResolvedAgent,
        validated_program: Option<crate::agent::ValidatedLaunchProgram>,
        target: TargetResolution,
    ) -> Result<(PtySessionSpec, Option<String>), ProtocolError> {
        let TargetResolution {
            launch_cwd,
            repo,
            branch,
            worktree_path,
            project_id,
            is_linked_worktree,
            warnings,
            ..
        } = target;
        let base = RuntimeRef::from(resolved.base.clone());
        let native = resolved.native_launch();
        let input_rules = resolved
            .profile
            .as_ref()
            .and_then(|profile| profile.input_rules)
            .unwrap_or_else(|| {
                input_rules_for_definition(&resolved.definition, &self.inner.config)
            });
        // Freeze the structural relaunch snapshot (C.4) from the resolved agent:
        // the launch program/args plus the native-session launch spec (a profile's
        // override, else the base kind's).
        // An assigned runtime gets its reference generated here, before launch,
        // so the durable record written ahead of the spawn already holds it.
        let assigned = match native.as_ref().and_then(NativeSessionLaunch::assigned) {
            Some(assigned) => {
                let reference = crate::agent::generate_assigned_reference()?;
                let argv = assigned.launch_argv(&reference);
                Some((reference, argv))
            }
            None => None,
        };
        let snapshot = ResumeSnapshot {
            program: resolved.program().to_owned(),
            args: resolved.snapshot_args(),
            native,
            launch_binding: host::LaunchPin::of(&resolved.definition),
            reference_provenance: if assigned.is_some() {
                NativeReferenceProvenance::Assigned
            } else {
                NativeReferenceProvenance::Reported
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
        env_extra.extend(self.session_pty_env(base.clone(), id));
        let opts = LaunchOpts {
            cwd: launch_cwd.clone(),
            cols: params.cols,
            rows: params.rows,
            env_extra,
            validated_program,
        };
        let plan = build_launch_command(
            resolved,
            self.inner.profiles.runtimes(),
            &opts,
            assigned
                .as_ref()
                .map_or(&[][..], |(_, argv)| argv.as_slice()),
            params.input.clone(),
        )?;
        let spec = PtySessionSpec {
            id: id.clone(),
            registration: target::PtyRegistration::Create,
            name: validate_session_name(params.name.as_deref())?,
            agent: resolved.name.clone(),
            agent_base: base,
            input_rules,
            snapshot,
            manifest_override,
            cwd: launch_cwd,
            cols: params.cols,
            rows: params.rows,
            command: plan.command,
            native_session_id: assigned
                .as_ref()
                .map(|(reference, _)| reference.value().to_owned()),
            native_session_path: None,
            project_id,
            is_linked_worktree,
            repo,
            branch,
            worktree_path,
            metadata: params.metadata.clone(),
            warnings,
            initial_input_pending: plan.pending_initial_input.is_some(),
            package_authority: None,
        };
        Ok((spec, plan.pending_initial_input))
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
                // Shutdown stops the wait, so the input is delivered while the
                // create drain still waits for it.
                let shutdown = self.inner.daemon_shutdown.clone();
                let _ = tokio::time::timeout(grace, async {
                    loop {
                        match worker.inspect().await {
                            Ok(snapshot) if snapshot.next_offset > 0 => break,
                            Ok(_) => tokio::select! {
                                () = shutdown.cancelled() => break,
                                () = tokio::time::sleep(WORKER_CONNECT_RETRY) => {}
                            },
                            Err(_) => break,
                        }
                    }
                })
                .await;
            }
            RuntimeHandle::Unavailable(_) => {}
        }
    }

    /// Roll back a session whose initial `--input` could not be delivered or
    /// committed by removing it ([`Self::remove`]): stop the runtime, retire
    /// its job, and free its worktree, so its branch is free for a retry.
    ///
    /// The removal persists its intent before it acts, and until then the
    /// record keeps the `initial_input` marker, so reconciliation finishes a
    /// rollback this cannot. Best-effort: a failure is logged, never
    /// propagated, so the caller still returns the original input error.
    async fn rollback_failed_initial_input(&self, id: &SessionId) {
        if let Err(err) = self.remove(id).await {
            warn!(
                session_id = %id.0,
                error = %err,
                "failed to remove a session whose initial input was not delivered; reconciliation finishes the rollback"
            );
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
                .worker_instance_id
                .as_ref()
                .is_some_and(|runtime| runtime.as_str() == params.worker_instance_id());
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

        let (
            info,
            record,
            previous_ordering,
            previous_native_id,
            previous_native_path,
            previous_provenance,
        ) = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(&session_id) else {
                return not_recorded;
            };
            let current_runtime_matches = entry
                .info
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.worker_instance_id.as_deref())
                == Some(params.worker_instance_id());
            if is_terminal(entry.info.state) || !current_runtime_matches {
                return not_recorded;
            }
            let incoming_sequence = params.sequence().get();
            if !native_report_is_current(
                entry.last_native_report.as_ref(),
                params.worker_instance_id(),
                incoming_sequence,
            ) {
                debug!(session_id = %session_id.0, "stale native-id report; ignoring");
                return not_recorded;
            }
            let previous_ordering = entry.last_native_report.clone();
            let previous_native_id = entry.info.native_session_id.clone();
            let previous_native_path = entry.info.native_session_path.clone();
            let previous_provenance = entry.snapshot.reference_provenance;
            entry.snapshot.reference_provenance = NativeReferenceProvenance::Reported;
            entry.last_native_report = Some(NativeIdentityReport {
                worker_instance_id: params.worker_instance_id().to_owned(),
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
                previous_provenance,
            )
        };
        if let Err(error) = self.write_session_record(record).await {
            debug!(session_id = %session_id.0, error = %error, "failed to persist native-id ordering; ignoring report");
            let mut sessions = self.inner.sessions.lock().await;
            if let Some(entry) = sessions.get_mut(&session_id) {
                let accepted = NativeIdentityReport {
                    worker_instance_id: params.worker_instance_id().to_owned(),
                    pid: params.pid(),
                    pid_start_identity: params.pid_start_identity().get(),
                    sequence: params.sequence().get(),
                };
                if entry.last_native_report.as_ref() == Some(&accepted) {
                    entry.last_native_report = previous_ordering;
                    entry.info.native_session_id = previous_native_id;
                    entry.info.native_session_path = previous_native_path;
                    entry.snapshot.reference_provenance = previous_provenance;
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

    /// Resolves the agent a callback of session `id` names. A name that is the
    /// session's own package runtime, bare or through a profile based on it,
    /// resolves against the definition the session was launched from, not the
    /// registry's current selection; every other name resolves normally.
    async fn resolve_session_agent(
        &self,
        id: &SessionId,
        agent: &str,
    ) -> Result<ResolvedAgent, ProtocolError> {
        let pinned = self
            .inner
            .sessions
            .lock()
            .await
            .get(id)
            .and_then(|entry| entry.pinned.clone());
        match pinned {
            Some(pinned) => self.inner.profiles.resolve_agent_pinned(agent, &pinned),
            None => self.inner.profiles.resolve_agent(agent),
        }
    }

    /// Record the active nested agent currently owning a live session.
    pub async fn report_agent(&self, params: SessionReportAgentParams) -> SessionReportAgentResult {
        let not_recorded = SessionReportAgentResult { recorded: false };
        let activity_epoch = self.daemon_instance_id().to_owned();
        let resolved = match self
            .resolve_session_agent(&params.session_id, &params.agent)
            .await
        {
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
        let (info, rescan, evidence) = {
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

            let (pid, start_identity) =
                bind_report_process(entry, params.pid, &RuntimeRef::from(resolved.base.clone()));
            let report = ActiveAgentReport {
                source: params.source.clone(),
                agent: resolved.name.clone(),
                seq: report_sequence,
                pid,
                start_identity,
                reported_at: crate::time::now(),
                activity_reported: reported_activity.is_some(),
            };
            entry.active_agent = Some(report.clone());
            entry.last_agent_report = Some(report);
            entry.info.active_agent = Some(resolved.name.clone());
            entry.info.active_agent_base = Some(RuntimeRef::from(resolved.base));
            entry.info.active_agent_pid = pid;
            entry.info.active_agent_session_id = valid_session_id;
            entry.info.active_agent_session_path = valid_session_path;
            let evidence = reported_activity.and_then(|activity| {
                entry.info.activity = Some(activity);
                entry.info.state_source = StateSource::Report;
                record_activity_evidence(entry, activity, StateSource::Report, &activity_epoch)
            });
            send_detector_config(entry, active_detector_config);
            entry.info.updated_at = timestamp_now();
            (
                entry.info.clone(),
                Arc::clone(&entry.procwatch_rescan),
                evidence,
            )
        };

        self.emit(event::SESSION_UPDATED, &info);
        rescan.notify_one();
        if let Some(evidence) = evidence {
            let event = crate::events::event(
                event::AGENT_STATE,
                event_payload(evidence.event(params.session_id.clone())),
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
        let resolved = match self
            .resolve_session_agent(&params.session_id, &params.agent)
            .await
        {
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
                start_identity: None,
                reported_at: crate::time::now(),
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
        let pin = self
            .inner
            .sessions
            .lock()
            .await
            .get(&info.id)
            .map(|entry| entry.snapshot.launch_binding.clone())
            .unwrap_or_default();
        self.ensure_mutable_kinds(&info, &pin)
    }

    #[cfg(test)]
    pub(super) async fn record_cwd_hint(&self, id: &SessionId, path: String) {
        self.record_cwd_hint_scoped(id, path, None).await;
    }

    async fn record_cwd_hint_scoped(
        &self,
        id: &SessionId,
        path: String,
        expected: Option<&RuntimeWatchIdentity>,
    ) {
        let observed_at = crate::time::now();
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
            Ok(true) => {
                self.apply_cwd_change(id, cwd, CwdSource::Osc7, expected, observed_at)
                    .await;
            }
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

    /// Moves the session to `cwd` unless the evidence that set its current cwd
    /// is newer.
    ///
    /// `observed_at` is when the evidence was read. Evidence for the current
    /// cwd changes nothing, so the source that established it stays.
    async fn apply_cwd_change(
        &self,
        id: &SessionId,
        cwd: PathBuf,
        source: CwdSource,
        expected: Option<&RuntimeWatchIdentity>,
        observed_at: Instant,
    ) {
        if !self.cwd_update_needed(id, &cwd, observed_at).await {
            return;
        }

        let association = self.resolve_cwd_association(id, cwd.clone()).await;
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, "cwd update arrived for unknown session");
                return;
            };
            if expected.is_some_and(|expected| !expected.matches(entry)) {
                debug!(session_id = %id.0, "cwd update arrived for a superseded runtime");
                return;
            }
            if !cwd_evidence_moves(entry, &cwd, observed_at) {
                return;
            }
            entry.cwd_observed_at = observed_at;
            Some(apply_cwd_change(entry, cwd, source, association))
        };

        if let Some(info) = updated {
            self.emit(event::SESSION_UPDATED, &info);
        }
    }

    async fn cwd_update_needed(&self, id: &SessionId, cwd: &Path, observed_at: Instant) -> bool {
        let sessions = self.inner.sessions.lock().await;
        sessions
            .get(id)
            .is_some_and(|entry| cwd_evidence_moves(entry, cwd, observed_at))
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
        self.ensure_not_external(id).await?;
        let _guard = self.lock_lifecycle(id).await;
        self.stop_with_intent(id, DesiredState::Stopped, TransactionKind::Stop)
            .await
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the stop transaction stays linear so durable intent, runtime shutdown, and terminal commit ordering remain explicit"
    )]
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
        let (
            runtime,
            detector_cancel,
            procwatch_cancel,
            runtime_watch_cancel,
            previous_desired_state,
            stop_runtime,
            durable_intent,
        ) = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get_mut(id)
                .ok_or_else(|| session_not_found(&id.0))?;
            if is_terminal(entry.info.state) {
                return Ok(SessionStopResult { stopped: false });
            }

            let previous_desired_state = entry.desired_state;
            let stop_runtime = RuntimeWatchIdentity::from_info(&entry.info);
            entry.stopping = true;
            entry.stop_transaction_id = Some(transaction_id.clone());
            entry.desired_state = desired_state;
            (
                entry.runtime.clone(),
                entry.detector_cancel.clone(),
                entry.procwatch_cancel.clone(),
                entry.runtime_watch_cancel.clone(),
                previous_desired_state,
                stop_runtime,
                Self::session_record(
                    id,
                    entry,
                    desired_state,
                    Some(SessionTransaction {
                        id: transaction_id.clone(),
                        kind: transaction_kind,
                        phase: "requested".to_owned(),
                        previous_worker_id: None,
                        previous_worker_instance_id: None,
                        daemon_instance_id: None,
                    }),
                ),
            )
        };
        let terminal_base = match self.commit_stop_intent(id, durable_intent).await {
            Ok(record) => record,
            Err(error) => {
                self.rollback_stop_intent(
                    id,
                    previous_desired_state,
                    desired_state,
                    &transaction_id,
                    stop_runtime.as_ref(),
                )
                .await;
                return Err(error);
            }
        };

        detector_cancel.cancel();
        procwatch_cancel.cancel();
        self.remove_pending_attaches_for_session(id).await;
        self.cancel_session_attaches(id).await;

        let exit = match runtime {
            RuntimeHandle::Worker(worker) => {
                let transaction =
                    pohunek_worker_protocol::TransactionId::new(transaction_id.clone())
                        .map_err(|error| runtime_error("worker_stop_invalid", error.to_string()))?;
                let status = match worker.stop(transaction).await {
                    Ok(Some(status)) => status,
                    Ok(None) => {
                        self.clear_stopping(id, &transaction_id, stop_runtime.as_ref())
                            .await;
                        return Err(runtime_error(
                            "worker_stop_incomplete",
                            format!("worker for {} did not return a terminal outcome", id.0),
                        ));
                    }
                    Err(error) => {
                        self.clear_stopping(id, &transaction_id, stop_runtime.as_ref())
                            .await;
                        return Err(worker_error_to_protocol(error));
                    }
                };
                RuntimeExit {
                    exit_code: status.code,
                    success: status.code == Some(0) && status.signal.is_none(),
                }
            }
            RuntimeHandle::Unavailable(state) => {
                self.clear_stopping(id, &transaction_id, stop_runtime.as_ref())
                    .await;
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

    async fn commit_stop_intent(
        &self,
        id: &SessionId,
        intent: SessionRecord,
    ) -> Result<SessionRecord, ProtocolError> {
        for _ in 0..MAX_RUNTIME_TRANSITION_COMMIT_ATTEMPTS {
            let durable_base = self
                .load_durable_session_record(id)
                .await?
                .unwrap_or_else(|| intent.clone());
            let mut candidate = intent.clone();
            preserve_durable_worker_metadata(&durable_base, &mut candidate);
            match self
                .write_session_record_if_current(durable_base, candidate.clone())
                .await
            {
                Ok(()) => return Ok(candidate),
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "session_record_commit_stale" | "session_runtime_commit_stale"
                    ) =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(runtime_error(
            "session_stop_intent_busy",
            format!(
                "session {} kept changing while committing its stop intent",
                id.0
            ),
        ))
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
    /// cleared so a daemon restart cannot resurrect it. An ambiguous
    /// conflict, a reconnecting, or an incompatible runtime cannot be stopped
    /// through its worker yet may still own a live PTY, so its exact recorded
    /// generation is retired through the supervisor and its journaled worker
    /// proven gone before the record is deleted. Such a runtime whose record
    /// names no generation, a conflict over a job or worker that is not the
    /// recorded generation's, and a worker still running after retirement are
    /// refused instead. Every removal then sweeps the marked processes of the
    /// session's runtimes, since a descendant that left the worker's process
    /// group outlives both the stop and the retirement. A `session_removed`
    /// event is emitted with the final snapshot so subscribed clients drop
    /// their view of the session.
    ///
    /// Worktree cleanup is best-effort: a checkout that could not be deleted is
    /// counted in [`SessionRemoveResult::worktrees_failed`] instead of failing
    /// the removal, so a caller reporting cleaned worktrees can tell the two
    /// apart.
    ///
    /// # Errors
    ///
    /// Returns `session_not_found` when no session has the given id, and
    /// surfaces any PTY shutdown error from the implied stop of a live session.
    /// An unreachable runtime that cannot be retired (no recorded generation,
    /// or a non-ambiguous conflict) fails with its runtime-state code
    /// (`session_runtime_conflict`, `session_runtime_reconnecting`, or
    /// `worker_protocol_incompatible`); a generation the supervisor cannot
    /// retire fails with `runtime_supervision_unavailable`; a worker not
    /// proven gone afterwards fails with `runtime_supervision_ambiguous` or
    /// `runtime_identity_mismatch`. The record is kept in every case. A
    /// marker sweep that cannot confirm every marked process exited fails
    /// with `runtime_supervision_ambiguous` after the removal intent was
    /// recorded; like any cleanup that fails then, it keeps the session
    /// listed with its intent, so the removal can be retried (daemon startup
    /// reconciliation also finishes it).
    pub async fn remove(&self, id: &SessionId) -> Result<SessionRemoveResult, ProtocolError> {
        self.remove_with(id, UnconfirmedCleanup::Refuse).await
    }

    /// [`Self::remove`] with an explicit decision on unconfirmed cleanup.
    ///
    /// With [`UnconfirmedCleanup::Accept`], a runtime whose marker sweep is
    /// unconfirmed solely because of unreadable-marker processes does not
    /// refuse the removal: those processes are never signalled and are
    /// returned in [`SessionRemoveResult::accepted_unconfirmed_processes`]
    /// and logged at `warn` before any cleanup step. Every other unconfirmed outcome (a signalled
    /// process still running, a sweep error, a missing supervision
    /// configuration) refuses exactly as under [`UnconfirmedCleanup::Refuse`].
    ///
    /// # Errors
    ///
    /// The errors of [`Self::remove`].
    pub async fn remove_with(
        &self,
        id: &SessionId,
        cleanup: UnconfirmedCleanup,
    ) -> Result<SessionRemoveResult, ProtocolError> {
        self.ensure_not_external(id).await?;
        let _guard = self.lock_lifecycle(id).await;
        let should_stop = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            if let Some(state) = unreachable_live_state(entry) {
                ensure_retirable(id, entry, state)?;
            }
            !is_terminal(entry.info.state) && matches!(entry.runtime, RuntimeHandle::Worker(_))
        };

        let stopped = if should_stop {
            let stopped = self
                .stop_with_intent(id, DesiredState::Removed, TransactionKind::Remove)
                .await?
                .stopped;
            self.retire_unstopped_job(id).await?;
            stopped
        } else {
            // Retiring first means a refused or failed retirement leaves no
            // removal intent behind; the record keeps its runtime state.
            self.retire_unstopped_job(id).await?;
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
                        previous_worker_instance_id: None,
                        daemon_instance_id: None,
                    }),
                )
            };
            self.write_session_record(removal_intent).await?;
            false
        };

        let (job, worker_instance_id) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            (
                entry.job.clone(),
                entry
                    .info
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.worker_instance_id.clone()),
            )
        };
        let released = self
            .release_removed_session(id, job.as_ref(), worker_instance_id.as_deref(), cleanup)
            .await?;
        Ok(SessionRemoveResult {
            removed: released.evicted,
            stopped,
            worktrees_removed: u32::try_from(released.worktrees.removed).unwrap_or(u32::MAX),
            worktrees_failed: u32::try_from(released.worktrees.failed).unwrap_or(u32::MAX),
            accepted_unconfirmed_processes: released.accepted_unconfirmed,
        })
    }

    /// Deletes everything a removed session owns once its workers are proven
    /// gone: marked processes of its runtimes, owned worktrees, the worker log
    /// family, the registry entry, the resume binding, and finally the
    /// durable record.
    ///
    /// Shared by [`Self::remove_with`] and the reconciliation that finishes a
    /// durable removal intent, so both leave the same state behind. The
    /// marker sweep of every runtime of `generation` and of `worker_instance_id`
    /// ([`Self::sweep_removed_runtimes`], governed by `cleanup`) comes first, because nothing
    /// controls a surviving descendant once the record is gone. The record is
    /// deleted even when no entry is listed (startup reconciliation runs
    /// before the session is installed), and `session_removed` is emitted
    /// only for an evicted entry.
    ///
    /// # Errors
    ///
    /// Returns `runtime_supervision_ambiguous` when a runtime's marked
    /// processes are not proven gone, before anything is deleted, and
    /// otherwise the worktree, log, or store error that stopped the cleanup.
    /// Every step is idempotent and the durable record is deleted last, so a
    /// failed cleanup keeps the removal intent for a later attempt, and an
    /// entry evicted before the record could be deleted is listed again.
    async fn release_removed_session(
        &self,
        id: &SessionId,
        generation: Option<&Generation>,
        worker_instance_id: Option<&str>,
        cleanup: UnconfirmedCleanup,
    ) -> Result<ReleasedSession, ProtocolError> {
        let accepted_unconfirmed = self
            .sweep_removed_runtimes(id, generation, worker_instance_id, cleanup)
            .await?;
        // Every runtime is judged and the cap passed, so the removal proceeds
        // past these processes; logged before the first destructive step so
        // a later failure still leaves the decision on record.
        for (process, accepted_worker_instance_id) in &accepted_unconfirmed {
            warn!(
                session_id = %id.0,
                worker_instance_id = %accepted_worker_instance_id,
                pid = process.pid,
                start_identity = %process.start_identity,
                comm = process.command.as_deref().unwrap_or("unavailable"),
                "removal proceeds past an unreadable-marker process that may belong to the removed runtime"
            );
        }
        let (cleanup_warnings, worktrees) = self.cleanup_owned_worktrees_for_removal(id).await?;
        if !cleanup_warnings.is_empty() {
            let mut sessions = self.inner.sessions.lock().await;
            if let Some(entry) = sessions.get_mut(id) {
                entry.info.warnings.extend(cleanup_warnings);
            }
        }
        // The entry is about to leave the map and its warnings with it, so a
        // checkout that survived is reported to the caller as well.
        if worktrees.failed > 0 {
            warn!(
                session_id = %id.0,
                worktrees_failed = worktrees.failed,
                "session removal left an owned worktree on disk"
            );
        }

        // The PTY has ended, so cleanup removes the accumulated family. A
        // retained terminal worker may still emit a final control diagnostic,
        // but the shared writer keeps any such file within the same hard cap.
        self.delete_session_logs(id).await?;

        // A concurrent eviction (another removal path) leaves no entry here.
        let evicted = self.inner.sessions.lock().await.remove(id);
        if let Some(entry) = &evicted {
            entry.cancel_runtime_watchers();
        }

        // The entry is gone, so this re-reads as "no live session" and clears any
        // lingering resume binding (idempotent for a session that already dropped
        // its binding on exit or stop).
        self.persist_resume_binding(id).await;
        if let Err(error) = self.delete_session_record(id).await {
            // The entry leaves first so the binding cleanup above cannot re-read
            // it as live. The record still holds the removal intent, so the
            // entry is listed again for a retried removal; the callers hold the
            // lifecycle lock, so no other entry can have taken its place.
            if let Some(entry) = evicted {
                self.inner
                    .sessions
                    .lock()
                    .await
                    .entry(id.clone())
                    .or_insert(entry);
            }
            return Err(error);
        }
        if let Some(entry) = &evicted {
            self.emit(event::SESSION_REMOVED, &entry.info);
        }
        let accepted_unconfirmed = accepted_unconfirmed
            .into_iter()
            .map(|(process, _)| process)
            .collect();
        Ok(ReleasedSession {
            evicted: evicted.is_some(),
            worktrees,
            accepted_unconfirmed,
        })
    }

    /// Retires the worker job of a session that removal does not stop
    /// through its worker.
    ///
    /// A terminal worker only retains its final output, so its exact
    /// generation is retired. An unreachable worker that is not proven gone
    /// (see [`unreachable_live_state`]) may still own a live PTY, and deleting
    /// its record alone would leave it running unrecorded: every journaled
    /// worker of the session is collected from a fresh journal scan, the
    /// generation is retired, and those workers must then be proven gone
    /// ([`Self::retire_generation_for_removal`]). A lost
    /// runtime was already classified ended and is left alone.
    ///
    /// # Errors
    ///
    /// Returns `runtime_supervision_unavailable` when the supervisor cannot
    /// retire the job, and `runtime_supervision_ambiguous` or
    /// `runtime_identity_mismatch` when an unreachable worker is not proven
    /// gone afterwards; the logical record is kept so removal can be retried.
    async fn retire_unstopped_job(&self, id: &SessionId) -> Result<(), ProtocolError> {
        let target = {
            let sessions = self.inner.sessions.lock().await;
            sessions.get(id).and_then(|entry| {
                let terminal = entry
                    .info
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.state == RuntimeState::Terminal);
                let unreachable = unreachable_live_state(entry).is_some();
                (terminal || unreachable)
                    .then(|| entry.job.clone().map(|job| (job, unreachable)))
                    .flatten()
            })
        };
        let Some((job, unreachable)) = target else {
            return Ok(());
        };
        let lifecycle = self.lifecycle()?;
        if unreachable {
            return self
                .retire_generation_for_removal(id, &job, &lifecycle)
                .await;
        }
        lifecycle.retire(&job).await.map_err(|error| {
            runtime_error(
                crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE,
                format!(
                    "failed to retire worker generation {} of session {}: {error}",
                    job.service_id(),
                    id.0
                ),
            )
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
            let mut metadata = WorkerMetadataTracker::default();
            loop {
                let mut retry_delay = WORKER_CONNECT_RETRY;
                let inspected = tokio::select! {
                    () = cancel.cancelled() => return,
                    inspected = worker.inspect() => inspected,
                };
                match inspected {
                    Ok(snapshot) => {
                        let worker_metadata = worker_metadata_fingerprint(&snapshot);
                        let initial_empty_metadata =
                            metadata.is_initial() && worker_metadata_is_empty(&worker_metadata);
                        if !initial_empty_metadata && metadata.should_apply(&worker_metadata) {
                            let outcome = registry
                                .apply_worker_metadata_snapshot(&id, &snapshot)
                                .await;
                            match metadata.record(
                                worker_metadata.clone(),
                                outcome,
                                snapshot.phase,
                                crate::time::now(),
                            ) {
                                WorkerMetadataProgress::Complete => {}
                                WorkerMetadataProgress::IdentityDiscarded => {
                                    warn!(
                                        session_id = %id.0,
                                        "discarding unverified terminal worker identity after safe metadata commit"
                                    );
                                }
                                WorkerMetadataProgress::Retry(retry) => {
                                    if retry.should_warn {
                                        warn!(
                                            session_id = %id.0,
                                            attempts = retry.attempts,
                                            retry_delay_ms = retry.delay.as_millis(),
                                            "worker metadata remains retryable"
                                        );
                                    }
                                    retry_delay = retry.delay;
                                }
                            }
                        } else {
                            metadata.complete(worker_metadata.clone());
                        }
                        if snapshot.phase == pohunek_worker_protocol::RuntimePhase::Exited
                            && metadata.is_complete(&worker_metadata)
                        {
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
                            .reconnect_worker(
                                &id,
                                &expected,
                                &socket_path,
                                &cancel,
                                &error,
                                registry.inner.config.worker_connect_deadline,
                            )
                            .await
                        else {
                            return;
                        };
                        worker = reconnected;
                    }
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(retry_delay) => {}
                }
            }
        });
    }

    /// Reconnects a lost worker, classifying the loss whenever the socket
    /// stays unreachable for `connect_deadline`.
    ///
    /// The deadline is a parameter so the unanswered-socket test can bound
    /// this attempt without shortening the launch budget of its healthy
    /// session; the watcher passes the configured `worker_connect_deadline`.
    async fn reconnect_worker(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        socket_path: &Path,
        cancel: &CancellationToken,
        error: &WorkerError,
        connect_deadline: Duration,
    ) -> Option<Worker> {
        // A cancelled watcher no longer owns the runtime, and a daemon that is
        // shutting down leaves every worker runtime to the next daemon's
        // reconciliation, so neither rewrites the runtime's state.
        if cancel.is_cancelled() || self.inner.daemon_shutdown_started.load(Ordering::Relaxed) {
            return None;
        }
        if !self.mark_worker_reconnecting(id, expected, error).await {
            return None;
        }
        // A worker whose death is already provable is classified at once
        // instead of after the whole connect deadline.
        let classified = tokio::select! {
            () = cancel.cancelled() => return None,
            classified = self.classify_lost_worker(id, expected, true) => classified,
        };
        if classified == LossClassification::Settled {
            return None;
        }
        let mut deadline = tokio::time::Instant::now() + connect_deadline;
        loop {
            if cancel.is_cancelled() || self.inner.daemon_shutdown_started.load(Ordering::Relaxed) {
                return None;
            }
            // Each attempt ends at the deadline too: a socket that accepts and
            // then never answers negotiation or acquisition would otherwise
            // keep one attempt pending and the loss would never be classified.
            let attempt = tokio::select! {
                () = cancel.cancelled() => return None,
                attempt = tokio::time::timeout_at(
                    deadline,
                    Worker::connect(socket_path, &id.0, self.daemon_instance_id()),
                ) => attempt,
            };
            let unreachable = match attempt {
                Ok(Ok(worker)) => {
                    match self
                        .adopt_reconnected_worker(id, expected, worker.clone())
                        .await
                    {
                        RuntimeTransitionOutcome::Applied(_) => return Some(worker),
                        RuntimeTransitionOutcome::IdentityMismatch => return None,
                        RuntimeTransitionOutcome::RetryablePersistenceFailure(_)
                        | RuntimeTransitionOutcome::RetryableConcurrentChange => {
                            // The worker answered, so the deadline restarts:
                            // it bounds how long the worker stays unreachable,
                            // not how long adoption keeps being retried.
                            deadline = tokio::time::Instant::now() + connect_deadline;
                            tokio::select! {
                                () = cancel.cancelled() => return None,
                                () = tokio::time::sleep(WORKER_CONNECT_RETRY) => {}
                            }
                        }
                    }
                    continue;
                }
                Ok(Err(reconnect_error)) if tokio::time::Instant::now() < deadline => {
                    debug!(
                        session_id = %id.0,
                        error = %reconnect_error,
                        "session worker is not reconnectable yet"
                    );
                    tokio::select! {
                        () = cancel.cancelled() => return None,
                        () = tokio::time::sleep(WORKER_CONNECT_RETRY) => {}
                    }
                    continue;
                }
                Ok(Err(reconnect_error)) => reconnect_error.to_string(),
                Err(_elapsed) => format!(
                    "worker socket did not answer within {}ms",
                    connect_deadline.as_millis()
                ),
            };
            debug!(
                session_id = %id.0,
                error = %unreachable,
                "session worker stayed unreachable for the connect deadline"
            );
            let classified = tokio::select! {
                () = cancel.cancelled() => return None,
                classified = self.classify_lost_worker(id, expected, false) => classified,
            };
            if classified == LossClassification::Settled {
                return None;
            }
            deadline = tokio::time::Instant::now() + connect_deadline;
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

    /// Commits the classification of a worker whose control connection is gone.
    ///
    /// The transition is conditional on `expected` still naming the entry's
    /// runtime, and it keeps newer durable metadata; a `Lost` classification
    /// also terminalizes running subagents.
    async fn mark_worker_unavailable(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        state: RuntimeState,
        reason: &str,
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
            candidate.runtime = RuntimeHandle::Unavailable(state);
            if let Some(runtime) = candidate.info.runtime.as_mut() {
                runtime.state = state;
                runtime.loss_reason = Some(reason.to_owned());
            }
            candidate.info.updated_at = timestamp_now();
            (base, candidate)
        };
        let durable_base = match self.load_durable_session_record(id).await {
            Ok(Some(record)) => record,
            Ok(None) => base.clone(),
            Err(error) => return RuntimeTransitionOutcome::RetryablePersistenceFailure(error),
        };
        let policy = if state == RuntimeState::Lost {
            RuntimeMetadataPolicy::Terminal
        } else {
            RuntimeMetadataPolicy::Live
        };
        let outcome = self
            .commit_runtime_transition(id, expected, &base, &durable_base, candidate, policy)
            .await;
        if let RuntimeTransitionOutcome::RetryablePersistenceFailure(store_error) = &outcome {
            warn!(
                session_id = %id.0,
                error = %store_error,
                "failed to persist the unreachable worker classification"
            );
        }
        if let RuntimeTransitionOutcome::Applied(info) = &outcome {
            warn!(
                session_id = %id.0,
                runtime.state = ?state,
                reason,
                "session worker could not be reconnected"
            );
            let event_name = match state {
                RuntimeState::Lost | RuntimeState::Incompatible => event::SESSION_RUNTIME_LOST,
                RuntimeState::Conflict => event::SESSION_RUNTIME_CONFLICT,
                RuntimeState::Starting
                | RuntimeState::Live
                | RuntimeState::Reconnecting
                | RuntimeState::Terminal => event::SESSION_UPDATED,
            };
            self.emit(event_name, info.as_ref());
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
        let Some(worker_instance_id) = worker
            .worker_instance_id()
            .await
            .map(|value| value.to_string())
        else {
            return RuntimeTransitionOutcome::IdentityMismatch;
        };
        if worker_id != expected.worker_id || worker_instance_id != expected.worker_instance_id {
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
                runtime.worker_instance_id = Some(worker_instance_id);
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
            .commit_runtime_transition(
                id,
                expected,
                &base,
                &durable_base,
                candidate,
                RuntimeMetadataPolicy::Live,
            )
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
        metadata_policy: RuntimeMetadataPolicy,
    ) -> RuntimeTransitionOutcome {
        preserve_durable_transition_metadata(durable_base, &mut candidate, metadata_policy);
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
        if let Some(recovery) = &record.recovery {
            candidate.snapshot.reference_provenance = recovery.native_reference_provenance;
        }
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
        // An unavailable runtime has no worker left to watch; only a later
        // adoption, with fresh tokens, may watch this session again.
        if matches!(candidate.runtime, RuntimeHandle::Unavailable(_)) {
            candidate.cancel_runtime_watchers();
        }
        let info = candidate.info.clone();
        sessions.insert(id.clone(), candidate);
        RuntimeTransitionOutcome::Applied(Box::new(info))
    }

    /// Installs `entry`, which carries fresh watcher tokens, as the session's
    /// registry entry.
    ///
    /// The watchers of a replaced entry are cancelled, so re-adopting or
    /// reclassifying a runtime never leaves a second watcher set on it.
    async fn install_session_entry(&self, id: &SessionId, entry: SessionEntry) {
        let replaced = self.inner.sessions.lock().await.insert(id.clone(), entry);
        if let Some(replaced) = replaced {
            replaced.cancel_runtime_watchers();
        }
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
            RuntimeMetadataPolicy::Terminal,
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
        if updated.cancel_attaches {
            self.cancel_session_attaches(id).await;
        }
        self.remove_pending_attaches_for_session(id).await;
        // A terminal session must not resurrect on the next daemon restart:
        // resume is for sessions whose live PTY a restart killed, not for ones
        // the user stopped or that exited. The session is now terminal, so
        // `persist_resume_binding` re-reads it as terminal and removes its
        // binding (serialized against any racing resize/capture write).
        self.persist_resume_binding(id).await;
        self.spawn_session_hook(SessionHookRequest {
            event: HookEvent::SessionStop,
            cwd: committed_info.cwd.clone(),
            session_id: committed_info.id.0.clone(),
            project_id: committed_info.project_id.clone(),
            agent: committed_info.agent.clone(),
            stop_reason: Some(updated.stop_reason),
            activity: None,
        });
        self.emit(updated.event, committed_info.as_ref());
        Ok(true)
    }

    async fn clear_stopping(
        &self,
        id: &SessionId,
        transaction_id: &str,
        expected_runtime: Option<&RuntimeWatchIdentity>,
    ) {
        let mut sessions = self.inner.sessions.lock().await;
        if let Some(entry) = sessions.get_mut(id) {
            let runtime_matches = expected_runtime.map_or_else(
                || RuntimeWatchIdentity::from_info(&entry.info).is_none(),
                |expected| expected.matches(entry),
            );
            if !is_terminal(entry.info.state)
                && runtime_matches
                && entry.stop_transaction_id.as_deref() == Some(transaction_id)
            {
                entry.stopping = false;
                entry.stop_transaction_id = None;
            }
        }
    }

    async fn rollback_stop_intent(
        &self,
        id: &SessionId,
        previous_desired_state: DesiredState,
        attempted_desired_state: DesiredState,
        attempted_transaction_id: &str,
        expected_runtime: Option<&RuntimeWatchIdentity>,
    ) {
        let mut sessions = self.inner.sessions.lock().await;
        let Some(entry) = sessions.get_mut(id) else {
            return;
        };
        let runtime_matches = expected_runtime.map_or_else(
            || RuntimeWatchIdentity::from_info(&entry.info).is_none(),
            |expected| expected.matches(entry),
        );
        // The failed durable writer may finish after runtime replacement or a
        // newer stop/remove request. Only undo the exact in-memory intent that
        // belonged to the writer.
        if runtime_matches
            && !is_terminal(entry.info.state)
            && entry.stopping
            && entry.desired_state == attempted_desired_state
            && entry.stop_transaction_id.as_deref() == Some(attempted_transaction_id)
        {
            entry.stopping = false;
            entry.stop_transaction_id = None;
            entry.desired_state = previous_desired_state;
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
            self.ensure_mutable_kinds(&entry.info, &entry.snapshot.launch_binding)?;
        }
        Ok(())
    }

    /// Rejects a mutation of a session whose base or active agent is not a
    /// launchable runtime on this host.
    ///
    /// A session pinned to a package that is still installed stays mutable
    /// while that package is disabled or another version is selected.
    ///
    /// # Errors
    ///
    /// Returns `agent_kind_unsupported` for a kind that is not a runtime id and
    /// `runtime_not_installed` for a runtime id no definition backs.
    fn ensure_mutable_kinds(
        &self,
        info: &SessionInfo,
        pin: &host::LaunchPin,
    ) -> Result<(), ProtocolError> {
        let runtimes = self.inner.profiles.runtimes();
        runtimes.ensure_session_runtime(&info.agent_base, pin)?;
        if let Some(active) = &info.active_agent_base {
            // An active agent that is the session's own runtime is covered by
            // the session's pin; any other runtime must resolve on its own.
            let own = active == &info.agent_base;
            runtimes.ensure_session_runtime(
                active,
                if own { pin } else { &host::LaunchPin::Unpinned },
            )?;
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
        let mut observed_pids = HashSet::new();

        for fact in facts {
            if owned_pids.contains(&fact.pid) {
                continue;
            }
            let Some(agent_base) = identify_agent(self.inner.profiles.runtimes(), &fact) else {
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
            let candidate = transcripts.best_match(&agent_base, &cwd, &fact);
            let association = self
                .resolve_external_cwd_association(fact.pid, cwd.clone())
                .await;
            let info = external_session_info(&fact, agent_base, cwd, candidate, association);
            let identity = fact.identity();
            let upsert = match self
                .inner
                .external
                .upsert_if_current(
                    identity,
                    info,
                    || {
                        self.inner
                            .inspector
                            .identity(identity.pid)
                            .map(|current| current == Some(identity))
                    },
                    |change| match change {
                        ExternalSessionChange::Created(info) => {
                            self.emit(event::SESSION_CREATED, info);
                        }
                        ExternalSessionChange::Updated(info) => {
                            self.emit(event::SESSION_UPDATED, info);
                        }
                    },
                )
                .await
            {
                Ok(Some(upsert)) => upsert,
                Ok(None) => continue,
                Err(err) if err.is_race() => continue,
                Err(err) => {
                    debug!(
                        pid = identity.pid,
                        error = %err,
                        "failed to revalidate external agent identity"
                    );
                    observed_pids.insert(identity.pid);
                    continue;
                }
            };
            observed_pids.insert(identity.pid);
            if let Some(identity) = upsert.watch_identity {
                self.spawn_external_exit_watch(identity);
            }
        }

        self.inner
            .external
            .remove_unobserved(&observed_pids, |removed| {
                self.emit(event::SESSION_REMOVED, removed);
            })
            .await;
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
        #[cfg(test)]
        {
            let block = self
                .inner
                .external_association_block
                .lock()
                .expect("external association test lock")
                .clone();
            if let Some(block) = block {
                block.entered.notify_one();
                block.release.notified().await;
            }
        }
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

    fn spawn_external_exit_watch(&self, identity: ProcessIdentity) {
        let watch = match self.inner.inspector.exit_watch(identity) {
            Ok(watch) => watch,
            Err(err) => {
                debug!(
                    pid = identity.pid,
                    process_start_identity = identity.start_identity.get(),
                    error = %err,
                    "failed to arm external agent exit watch; falling back to poll cleanup"
                );
                return;
            }
        };
        self.spawn_external_exit_watch_task(identity, watch);
    }

    fn spawn_external_exit_watch_task(&self, identity: ProcessIdentity, watch: ExitWatch) {
        let registry = self.clone();
        let shutdown = self.inner.external.shutdown_token();
        tokio::spawn(async move {
            tokio::select! {
                () = shutdown.cancelled() => {}
                result = watch.wait() => {
                    if let Err(err) = result {
                        debug!(
                            pid = identity.pid,
                            process_start_identity = identity.start_identity.get(),
                            error = %err,
                            "external process exit watch failed"
                        );
                        return;
                    }
                    registry.on_external_agent_exit(identity).await;
                }
            }
        });
    }

    async fn on_external_agent_exit(&self, identity: ProcessIdentity) {
        self.inner
            .external
            .remove_identity(identity, |info| self.emit(event::SESSION_REMOVED, info))
            .await;
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

    fn emit_native_recovered(
        &self,
        info: &SessionInfo,
        previous_worker_instance_id: Option<String>,
    ) {
        let worker_instance_id = info
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.worker_instance_id.clone());
        let event = crate::events::event(
            event::SESSION_NATIVE_RECOVERED,
            event_payload(SessionNativeRecoveredEvent {
                session: info.clone(),
                previous_worker_instance_id,
                worker_instance_id,
            }),
        );
        let _ = self.inner.events.send(event);
    }
}

type WorkerMetadataFingerprint = (
    Option<pohunek_worker_protocol::ReportedLaunchIdentity>,
    Option<pohunek_worker_protocol::ActiveIdentityClaim>,
    Option<pohunek_worker_protocol::ReleasedIdentityClaim>,
    Vec<pohunek_worker_protocol::SubagentSnapshot>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMetadataApplyOutcome {
    Applied,
    Retryable(WorkerMetadataRetryCause),
    Discarded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMetadataRetryCause {
    Commit,
    IdentityValidation,
}

#[derive(Debug, Default)]
struct WorkerMetadataTracker {
    completed: Option<WorkerMetadataFingerprint>,
    pending: Option<(WorkerMetadataFingerprint, usize)>,
    last_retry_warning: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerMetadataRetry {
    attempts: usize,
    delay: Duration,
    should_warn: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMetadataProgress {
    Complete,
    IdentityDiscarded,
    Retry(WorkerMetadataRetry),
}

impl WorkerMetadataTracker {
    fn is_initial(&self) -> bool {
        self.completed.is_none() && self.pending.is_none()
    }

    fn should_apply(&self, fingerprint: &WorkerMetadataFingerprint) -> bool {
        self.completed.as_ref() != Some(fingerprint)
    }

    fn is_complete(&self, fingerprint: &WorkerMetadataFingerprint) -> bool {
        self.completed.as_ref() == Some(fingerprint) && self.pending.is_none()
    }

    fn complete(&mut self, fingerprint: WorkerMetadataFingerprint) {
        self.completed = Some(fingerprint);
        self.pending = None;
        self.last_retry_warning = None;
    }

    fn retry(
        &mut self,
        fingerprint: &WorkerMetadataFingerprint,
        now: Instant,
    ) -> WorkerMetadataRetry {
        let attempts = self
            .pending
            .as_ref()
            .filter(|(pending, _)| pending == fingerprint)
            .map_or(1, |(_, attempts)| attempts.saturating_add(1));
        self.pending = Some((fingerprint.clone(), attempts));
        let multiplier =
            2_u32.saturating_pow(u32::try_from(attempts.saturating_sub(1)).unwrap_or(u32::MAX));
        let delay = WORKER_CONNECT_RETRY
            .saturating_mul(multiplier)
            .min(MAX_WORKER_METADATA_RETRY_DELAY);
        let should_warn = self.last_retry_warning.is_none_or(|last_warning| {
            now.saturating_duration_since(last_warning) >= WORKER_METADATA_RETRY_WARN_INTERVAL
        });
        if should_warn {
            self.last_retry_warning = Some(now);
        }
        WorkerMetadataRetry {
            attempts,
            delay,
            should_warn,
        }
    }

    fn record(
        &mut self,
        fingerprint: WorkerMetadataFingerprint,
        outcome: WorkerMetadataApplyOutcome,
        phase: pohunek_worker_protocol::RuntimePhase,
        now: Instant,
    ) -> WorkerMetadataProgress {
        match outcome {
            WorkerMetadataApplyOutcome::Applied | WorkerMetadataApplyOutcome::Discarded => {
                self.complete(fingerprint);
                WorkerMetadataProgress::Complete
            }
            WorkerMetadataApplyOutcome::Retryable(WorkerMetadataRetryCause::IdentityValidation)
                if phase == pohunek_worker_protocol::RuntimePhase::Exited =>
            {
                self.complete(fingerprint);
                WorkerMetadataProgress::IdentityDiscarded
            }
            WorkerMetadataApplyOutcome::Retryable(_) => {
                WorkerMetadataProgress::Retry(self.retry(&fingerprint, now))
            }
        }
    }
}

fn worker_metadata_fingerprint(
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
) -> WorkerMetadataFingerprint {
    (
        snapshot.launch_identity.clone(),
        snapshot.active_identity.clone(),
        snapshot.active_identity_release.clone(),
        snapshot.subagents.clone(),
    )
}

fn worker_metadata_is_empty(identity: &WorkerMetadataFingerprint) -> bool {
    identity.0.is_none() && identity.1.is_none() && identity.2.is_none() && identity.3.is_empty()
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

/// Whether cwd evidence observed at `observed_at` moves a live session to `cwd`.
///
/// Evidence older than the evidence that set the current cwd is stale.
fn cwd_evidence_moves(entry: &SessionEntry, cwd: &Path, observed_at: Instant) -> bool {
    !entry.stopping
        && !is_terminal(entry.info.state)
        && observed_at >= entry.cwd_observed_at
        && entry.info.cwd != cwd
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
    agent_base: RuntimeRef,
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
        subagents: Vec::new(),
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

/// The durable intent of a `session.new` (RFC section 14, phase 2): a
/// `create/preparing` record with the create's structural metadata and no
/// worker generation.
///
/// It is written before the target is bound, so a worktree the create binds
/// is always reachable from a record: reconciliation of a create record that
/// names no generation proves nothing was launched and compensates it. The
/// launch directory is provisional until the preparing record of
/// [`SessionRegistry::run_pty_registration`] replaces the intent.
///
/// # Errors
///
/// Returns `bad_request` for an invalid session name.
fn create_intent_record(
    id: &SessionId,
    params: &SessionNewParams,
    resolved: &ResolvedAgent,
    cwd: &Path,
) -> Result<SessionRecord, ProtocolError> {
    let native = resolved.native_launch();
    let created_at = timestamp_now();
    let info = SessionInfo {
        id: id.clone(),
        external: Some(false),
        capabilities: protocol::SessionCapabilities {
            resume: native.is_some(),
            fork: native
                .as_ref()
                .is_some_and(NativeSessionLaunch::supports_fork),
        },
        name: validate_session_name(params.name.as_deref())?,
        agent: resolved.name.clone(),
        agent_base: RuntimeRef::from(resolved.base.clone()),
        cwd: cwd.to_path_buf(),
        cwd_source: Some(CwdSource::Launch),
        pid: 0,
        runtime: Some(SessionRuntime {
            state: RuntimeState::Starting,
            runtime_generation: protocol::RuntimeGeneration::new(1),
            worker_id: None,
            worker_instance_id: None,
            started_at: None,
            last_connected_at: None,
            loss_reason: None,
        }),
        cols: params.cols,
        rows: params.rows,
        state: SessionState::Starting,
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
        repo: params.repo.clone(),
        branch: params.branch.clone(),
        worktree_path: None,
        metadata: params.metadata.clone(),
        warnings: Vec::new(),
        created_at: created_at.clone(),
        updated_at: created_at,
        exit_code: None,
    };
    Ok(SessionRecord {
        schema_version: SESSION_RECORD_SCHEMA_VERSION,
        session_id: id.0.clone(),
        desired_state: DesiredState::Running,
        transaction: Some(SessionTransaction {
            id: format!("create-{}", id.0),
            kind: TransactionKind::Create,
            phase: "preparing".to_owned(),
            previous_worker_id: None,
            previous_worker_instance_id: None,
            daemon_instance_id: None,
        }),
        info,
        native_identity_ordering: None,
        recovery: None,
        runtime: RuntimeRecord {
            state: RuntimeState::Starting,
            worker_id: None,
            worker_instance_id: None,
            service_id: None,
            generation: None,
            executable: None,
            reason: None,
        },
    })
}

/// Extends a failed create's error `msg` with the compensation it left
/// pending for the supervision retry.
fn unfinished_compensation_message(msg: &str, id: &SessionId) -> String {
    format!(
        "{msg}; session {} keeps its create record until its worktree is removed, \
         and stays listed as create_compensation_pending while the daemon retries the removal",
        id.0
    )
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

fn agent_kind_label(agent: &RuntimeRef) -> &str {
    agent.as_wire()
}

impl SessionRegistry {
    /// The hook schema of the runtime a session of `agent_base` launched with
    /// `pin`, resolved through the runtime's integration.
    ///
    /// A runtime that cannot be resolved or has no integration yields `None`,
    /// so the session admits no hook report.
    pub(super) fn session_hook_schema(
        &self,
        agent_base: &RuntimeRef,
        pin: &crate::agent::host::LaunchPin,
    ) -> Option<&'static pohunek_worker_protocol::HookSchema> {
        match self
            .inner
            .profiles
            .runtimes()
            .definition_for_pin(agent_base, pin)
        {
            Ok(definition) => definition.hook_schema(),
            Err(error) => {
                if let Some(schema) = self.inner.profiles.runtimes().legacy_hook_schema(pin) {
                    return Some(schema);
                }
                debug!(
                    runtime = %agent_kind_label(agent_base),
                    error = %error,
                    "the session runtime has no resolvable hook schema"
                );
                None
            }
        }
    }
}

fn detector_config_for_resolved_agent(resolved: &ResolvedAgent) -> DetectorConfig {
    match resolved
        .profile
        .as_ref()
        .and_then(|profile| profile.manifest.clone())
    {
        Some(manifest) => DetectorConfig {
            manifest: Some(manifest),
            ..DetectorConfig::default()
        },
        None => DetectorConfig::for_definition(&resolved.definition),
    }
}

fn bind_report_process(
    entry: &SessionEntry,
    reported_pid: Option<Pid>,
    agent_base: &RuntimeRef,
) -> (Option<Pid>, Option<u64>) {
    if let Some(pid) = reported_pid {
        // PID-bearing hooks are exact claims. If procwatch has not observed the
        // process yet, keep the exact pid so the immediate rescan can either bind
        // it or release it instead of falling back to an ambiguous base-kind match.
        let start_identity = entry
            .observed_agents
            .iter()
            .find(|observed| observed.pid == pid && &observed.agent_base == agent_base)
            .map(|observed| observed.start_identity);
        return (Some(pid), start_identity);
    }

    let mut matching = entry
        .observed_agents
        .iter()
        .filter(|observed| &observed.agent_base == agent_base)
        .map(|observed| (observed.pid, observed.start_identity));
    let Some(first) = matching.next() else {
        return (None, None);
    };
    if matching.next().is_none() {
        (Some(first.0), Some(first.1))
    } else {
        (None, None)
    }
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
    send_detector_config(entry, default_detector_config);
    entry.info.updated_at = timestamp_now();
    entry.info.clone()
}

fn send_detector_config(entry: &SessionEntry, config: DetectorConfig) {
    let generation = entry
        .detector_config
        .borrow()
        .generation
        .checked_add(1)
        .expect("detector configuration generation must not exhaust u64");
    let _ = entry
        .detector_config
        .send_replace(DetectorConfigUpdate { generation, config });
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
    report_seq_is_newer(current.seq, seq)
}

fn report_seq_is_newer(current: Option<u64>, incoming: Option<u64>) -> bool {
    match (current, incoming) {
        (Some(current), Some(incoming)) => incoming > current,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn native_report_is_current(
    current: Option<&NativeIdentityReport>,
    worker_instance_id: &str,
    sequence: u64,
) -> bool {
    current.is_none_or(|current| {
        current.worker_instance_id != worker_instance_id || sequence > current.sequence
    })
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
    runtime_error(
        unavailable_runtime_code(state),
        format!("session {} runtime is {}", id.0, runtime_state_label(state)),
    )
}

fn unavailable_runtime_code(state: RuntimeState) -> &'static str {
    match state {
        RuntimeState::Lost => "session_runtime_lost",
        RuntimeState::Conflict => "session_runtime_conflict",
        RuntimeState::Incompatible => "worker_protocol_incompatible",
        RuntimeState::Starting | RuntimeState::Reconnecting => "session_runtime_reconnecting",
        RuntimeState::Terminal => "session_not_running",
        RuntimeState::Live => "worker_operation_failed",
    }
}

/// Returns the state of an entry whose worker is unreachable but not proven
/// gone.
///
/// Such a worker may still own a live PTY: the daemon cannot stop it through
/// its control connection, only retire its recorded generation through the
/// supervisor. Terminal and lost runtimes have ended.
fn unreachable_live_state(entry: &SessionEntry) -> Option<RuntimeState> {
    match entry.runtime {
        RuntimeHandle::Unavailable(RuntimeState::Terminal | RuntimeState::Lost)
        | RuntimeHandle::Worker(_) => None,
        RuntimeHandle::Unavailable(state) => Some(state),
    }
}

/// Refuses the removal of an unreachable runtime its session cannot retire.
///
/// Retirement needs the generation the record names, and a conflict is
/// retirable only while it is ambiguous whether this session's own worker
/// still runs. Any other conflict means reconciliation found a job or worker
/// that is not the recorded generation's, which is never touched.
fn ensure_retirable(
    id: &SessionId,
    entry: &SessionEntry,
    state: RuntimeState,
) -> Result<(), ProtocolError> {
    let refusal = if entry.job.is_none() {
        "its record names no worker generation to retire"
    } else if state == RuntimeState::Conflict
        && entry
            .info
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.loss_reason.as_deref())
            != Some(crate::runtime::lifecycle::SUPERVISION_AMBIGUOUS)
    {
        "its job or worker is not proven to be the recorded generation's"
    } else {
        return Ok(());
    };
    Err(runtime_error(
        unavailable_runtime_code(state),
        format!(
            "session {} runtime is {} and {refusal}; stop its worker before removing the session",
            id.0,
            runtime_state_label(state)
        ),
    ))
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

/// Builds the runtime host over the built-in runtimes and the package store at
/// `plugins_dir`.
///
/// An unsafe or uncreatable store directory fails startup instead of serving a
/// registry that silently lacks the installed packages; an unreadable
/// registry record inside a safe directory degrades to the built-in runtimes
/// with the fault reported by the host.
fn open_runtime_host(plugins_dir: &Path) -> Result<RuntimeHost, ProtocolError> {
    let store = host::PackageStore::open(plugins_dir).map_err(|error| {
        warn!(error = %error, "the runtime package store cannot be opened");
        runtime_error(
            "plugin_store_unavailable",
            "the runtime package store cannot be opened safely",
        )
    })?;
    RuntimeHost::with_packages(
        host::BuiltinSource::from_host_environment(),
        host::PackageSource::new(store),
    )
    .map_err(|error| {
        warn!(error = %error, "the built-in runtime registry is invalid");
        runtime_error(
            "runtime_registry_invalid",
            "the built-in runtime registry is invalid",
        )
    })
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

fn preserve_newer_updated_at(existing: &str, replacement: &mut String) {
    let existing_time = OffsetDateTime::parse(existing, &Rfc3339);
    let replacement_time = OffsetDateTime::parse(replacement, &Rfc3339);
    if matches!(
        (existing_time, replacement_time),
        (Ok(existing_time), Ok(replacement_time)) if existing_time > replacement_time
    ) {
        replacement.clear();
        replacement.push_str(existing);
    }
}

fn preserve_durable_worker_metadata(existing: &SessionRecord, replacement: &mut SessionRecord) {
    replacement
        .info
        .subagents
        .clone_from(&existing.info.subagents);
    replacement
        .info
        .active_agent
        .clone_from(&existing.info.active_agent);
    replacement
        .info
        .active_agent_base
        .clone_from(&existing.info.active_agent_base);
    replacement.info.active_agent_pid = existing.info.active_agent_pid;
    replacement
        .info
        .active_agent_session_id
        .clone_from(&existing.info.active_agent_session_id);
    replacement
        .info
        .active_agent_session_path
        .clone_from(&existing.info.active_agent_session_path);
    let preserved_launch = preserve_durable_launch_identity(
        existing,
        &mut replacement.info,
        replacement.native_identity_ordering.is_some(),
    );
    if preserved_launch {
        match (replacement.recovery.as_mut(), existing.recovery.as_ref()) {
            (Some(replacement), Some(existing)) => {
                replacement
                    .native_session_id
                    .clone_from(&existing.native_session_id);
                replacement
                    .native_session_path
                    .clone_from(&existing.native_session_path);
                replacement.native_reference_provenance = existing.native_reference_provenance;
            }
            (None, Some(existing)) => replacement.recovery = Some(existing.clone()),
            _ => {}
        }
    }
    crate::store::preserve_newer_native_identity(existing, replacement);
    preserve_newer_updated_at(&existing.info.updated_at, &mut replacement.info.updated_at);
}

fn preserve_durable_transition_metadata(
    existing: &SessionRecord,
    replacement: &mut SessionEntry,
    policy: RuntimeMetadataPolicy,
) {
    // Active identity is intentionally not copied from storage: terminal
    // transitions must clear it, while live transitions retain the richer
    // in-memory report needed for ordering and later process validation.
    for durable in &existing.info.subagents {
        match replacement
            .info
            .subagents
            .iter_mut()
            .find(|current| current.provider == durable.provider && current.id == durable.id)
        {
            Some(current) if durable.revision > current.revision => current.clone_from(durable),
            Some(_) => {}
            None => replacement.info.subagents.push(durable.clone()),
        }
    }
    let _ = preserve_durable_launch_identity(
        existing,
        &mut replacement.info,
        replacement.last_native_report.is_some(),
    );
    if policy == RuntimeMetadataPolicy::Terminal {
        terminalize_running_subagents(&mut replacement.info.subagents, current_time_millis());
    } else {
        sort_subagents(&mut replacement.info.subagents);
    }
    preserve_newer_updated_at(&existing.info.updated_at, &mut replacement.info.updated_at);
}

fn preserve_durable_launch_identity(
    existing: &SessionRecord,
    replacement: &mut SessionInfo,
    replacement_has_ordering: bool,
) -> bool {
    if existing.native_identity_ordering.is_some() || replacement_has_ordering {
        return false;
    }
    replacement
        .native_session_id
        .clone_from(&existing.info.native_session_id);
    replacement
        .native_session_path
        .clone_from(&existing.info.native_session_path);
    true
}

fn current_time_millis() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn terminalize_running_subagents(subagents: &mut [SubagentInfo], occurred_at_ms: u64) {
    let mut revision = subagents
        .iter()
        .map(|subagent| subagent.revision.get())
        .max()
        .unwrap_or(0);
    for subagent in subagents
        .iter_mut()
        .filter(|subagent| subagent.lifecycle == SubagentLifecycle::Running)
    {
        revision = revision.saturating_add(1);
        subagent.lifecycle = SubagentLifecycle::Lost;
        subagent.activity = None;
        subagent.revision = SubagentRevision::new(revision);
        subagent.updated_at_ms = occurred_at_ms;
        subagent.finished_at_ms = Some(occurred_at_ms);
    }
    sort_subagents(subagents);
}

fn sort_subagents(subagents: &mut [SubagentInfo]) {
    subagents.sort_by(|left, right| {
        let left_terminal = left.lifecycle != SubagentLifecycle::Running;
        let right_terminal = right.lifecycle != SubagentLifecycle::Running;
        left_terminal
            .cmp(&right_terminal)
            .then_with(|| right.started_at_ms.cmp(&left.started_at_ms))
            .then_with(|| left.provider.as_wire().cmp(right.provider.as_wire()))
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Resolves the worker roots of a test registry.
///
/// Roots the config does not name are fresh directories; the third value owns
/// them and must live as long as the registry.
#[cfg(test)]
fn owned_test_worker_roots(
    config: &SessionRegistryConfig,
) -> (PathBuf, PathBuf, Vec<tempfile::TempDir>) {
    let mut owned = Vec::new();

    // State root (journals) has no path-length limit, so it can keep sharing
    // the metadata store's temp directory when the caller did not override
    // it explicitly.
    let state_base = config.store_path.as_ref().map_or_else(
        || {
            let dir = pohunek_test_support::tempdir_with_prefix("pohunek-daemon-worker-test-")
                .expect("create the worker state base");
            let path = dir.path().to_path_buf();
            owned.push(dir);
            path
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
    // `sun_path` (108 bytes on Linux, 104 on Darwin). The metadata store's temp
    // directory embeds a test tag plus a 19-digit nanosecond timestamp and
    // routinely overflows that budget for longer tags, so -- unlike the
    // state root -- the default runtime root always uses a short, unique
    // path directly under the fixture temp root, independent of `store_path`.
    let runtime_root = config.worker_runtime_root.clone().unwrap_or_else(|| {
        let dir = pohunek_test_support::tempdir_with_prefix("pw-")
            .expect("create the worker runtime root");
        let path = dir.path().to_path_buf();
        owned.push(dir);
        path
    });
    let state_root = config
        .worker_state_root
        .clone()
        .unwrap_or_else(|| state_base.join("state"));
    (runtime_root, state_root, owned)
}

/// Supervision inputs for the in-process test launcher.
///
/// The in-process launcher serves the generation named by the service ID and
/// never runs the definition's executable, so the definition only has to
/// satisfy the job contract.
#[cfg(test)]
fn test_supervision(runtime_root: &Path, state_root: &Path) -> SupervisionConfig {
    crate::runtime::SubprocessWorkerEnvironment {
        runtime_home: runtime_root.to_path_buf(),
        state_home: state_root.to_path_buf(),
        data_home: state_root.to_path_buf(),
        config_home: state_root.to_path_buf(),
        cache_home: state_root.to_path_buf(),
        home: state_root.to_path_buf(),
        daemon_socket: runtime_root.join("test-daemon.sock"),
    }
    .supervision(PathBuf::from("/nonexistent/pohunek-sessiond"))
    .with_environment_source(crate::test_support::thread_environment_source())
}

#[cfg(test)]
mod tests;
