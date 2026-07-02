//! In-memory session registry and supervisor.
//!
//! Runtime session state lives in memory: each live session owns a PTY handle and
//! has a watcher task that records process exit. The resumable subset of a
//! session's public info, including owner-controlled metadata, is persisted via
//! the resume binding store.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::{
    event, AgentActivity, AgentKind, ErrorClass, Event, ProjectRemoveResult, ProtocolError,
    SessionAttachParams, SessionId, SessionInfo, SessionInputParams, SessionInputResult,
    SessionNewParams, SessionRemoveResult, SessionReportNativeIdParams,
    SessionReportNativeIdResult, SessionSetMetadataResult, SessionState, SessionStopResult,
    SessionWarning, StateSource, WorktreeRemoveResult, PROTOCOL_VERSION,
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

mod attach;
mod detector;
mod hooks;
mod input;
mod lag;
mod resume;
mod target;

pub use attach::RedeemedAttach;

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
    /// not yet entered raw/bracketed-paste input mode.
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
    /// Set when daemon process shutdown starts. Natural PTY exits observed after
    /// this point are treated as restart fallout, not terminal session state.
    daemon_shutdown_started: AtomicBool,
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
        Self {
            inner: Arc::new(SessionRegistryInner {
                sessions: Mutex::new(HashMap::new()),
                pending_attaches: Mutex::new(HashMap::new()),
                active_attaches: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                next_stream_id: AtomicU64::new(1),
                daemon_shutdown_started: AtomicBool::new(false),
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

    /// Mark that the daemon process is shutting down.
    ///
    /// After this point, background PTY exit watchers may observe child exits
    /// caused by the daemon closing its PTY handles or by a service manager
    /// terminating the process tree. Those exits must not clear resume bindings:
    /// the next daemon instance needs them for startup resume.
    pub fn begin_daemon_shutdown(&self) {
        let already_started = self
            .inner
            .daemon_shutdown_started
            .swap(true, Ordering::Relaxed);
        if !already_started {
            info!("daemon shutdown started; preserving restart-resume bindings");
        }
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
            .list()
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
            let plan = build_launch_command(
                &resolved,
                &self.inner.config.shell_command,
                launch_cwd.clone(),
                params.cols,
                params.rows,
                env_extra,
                initial_input.clone(),
            )?;

            let info = self
                .register_pty_session(PtySessionSpec {
                    id: id.clone(),
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
                    // A live chunk arrived, or the channel closed: stop waiting.
                    Ok(_) | Err(broadcast::error::RecvError::Closed) => break,
                    // A lag means we missed chunks but the agent is producing output;
                    // keep waiting for the next recv.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
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
            let expected_base = agent_kind_label(entry.info.agent_base);
            if params.agent != entry.info.agent && params.agent != expected_base {
                debug!(
                    session_id = %params.session_id.0,
                    report_agent = %params.agent,
                    session_agent = %entry.info.agent,
                    session_agent_base = %expected_base,
                    "native-id report for a different agent; ignoring"
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
                SessionRefKind::Path => {
                    if let Some(path) = params.transcript_path.as_deref() {
                        SessionRef::path(path)
                    } else {
                        debug!(
                            session_id = %params.session_id.0,
                            "ignoring path-kind native-id report without transcript_path"
                        );
                        return not_recorded;
                    }
                }
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

    /// Merge owner-controlled metadata into a session and return the updated info.
    pub async fn set_metadata(
        &self,
        id: &SessionId,
        merge: BTreeMap<String, Option<String>>,
    ) -> Result<SessionSetMetadataResult, ProtocolError> {
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

    /// Evict a session from the registry, stopping it first if still live.
    ///
    /// `stop` only flips a live session to a terminal state; the entry stays in
    /// the registry so `list`/`inspect` keep showing it, which is why a stopped
    /// session otherwise lingers forever. `remove` is the eviction step. A
    /// still-live session is stopped first (so removal never orphans a live PTY),
    /// then the entry is dropped and its resume binding cleared so a daemon
    /// restart cannot resurrect it. A `session_removed` event is emitted with the
    /// final snapshot so subscribed clients drop their view of the session.
    ///
    /// # Errors
    ///
    /// Returns `session_not_found` when no session has the given id, and
    /// surfaces any PTY shutdown error from the implied stop of a live session.
    pub async fn remove(&self, id: &SessionId) -> Result<SessionRemoveResult, ProtocolError> {
        let was_live = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            !is_terminal(entry.info.state)
        };

        let stopped = if was_live {
            self.stop(id).await?.stopped
        } else {
            false
        };

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

            if self.inner.daemon_shutdown_started.load(Ordering::Relaxed)
                && !stopped_by_user
                && !entry.stopping
            {
                debug!(
                    session_id = %id.0,
                    "ignoring PTY exit observed after daemon shutdown started"
                );
                return;
            }

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

    fn emit(&self, name: &str, info: &SessionInfo) {
        let event = Event::new(name, json!({ "session": info }));
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

fn agent_kind_label(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Shell => "shell",
        AgentKind::Codex => "codex",
        AgentKind::Claude => "claude",
    }
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

#[expect(
    clippy::needless_pass_by_value,
    reason = "used directly as a `.map_err` function pointer, which requires owning the error"
)]
fn pty_error_to_protocol(err: PtyError) -> ProtocolError {
    let code = match err {
        PtyError::Allocate(_) => "pty_alloc_failed",
        PtyError::Spawn { .. } | PtyError::MissingPid => "spawn_failed",
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
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use protocol::{
        AgentActivity, AgentKind, Event, ProjectSource, SessionAttachParams, SessionId,
        SessionInfo, SessionNewParams, SessionReportNativeIdParams, SessionState,
    };

    use crate::agent::{InputRules, ResumeMode, SessionRefKind};
    use crate::detect::ActivityTransition;
    use crate::integration::{
        ENV_DAEMON_ID, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
    };
    use crate::pty::{PtyCommand, PtyExit};

    use super::{SessionRegistry, SessionRegistryConfig, ShellCommand, MAX_SESSION_NAME_BYTES};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn params() -> SessionNewParams {
        SessionNewParams {
            name: None,
            agent: "shell".to_owned(),
            cwd: Some(PathBuf::from("/tmp")),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            metadata: BTreeMap::new(),
        }
    }

    fn metadata(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn metadata_patch(entries: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.map(str::to_owned)))
            .collect()
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

    #[cfg(unix)]
    fn write_resume_agent_script(path: &std::path::Path, marker: &std::path::Path) {
        write_executable(
            path,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nsleep 30\n",
                marker.display()
            ),
        );
    }

    #[cfg(unix)]
    fn terminate_pid(pid: u32) {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
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

    fn pty_command<'a>(program: &str, args: impl IntoIterator<Item = &'a str>) -> PtyCommand {
        PtyCommand {
            program: program.to_owned(),
            args: args.into_iter().map(str::to_owned).collect(),
            env: Vec::new(),
            cwd: PathBuf::from("/tmp"),
            cols: 80,
            rows: 24,
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

    async fn next_session_updated(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> SessionInfo {
        let event = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = rx.recv().await.expect("receive session event");
                if event.event == protocol::event::SESSION_UPDATED {
                    break event;
                }
            }
        })
        .await
        .expect("session_updated event");
        serde_json::from_value(event.payload["session"].clone()).expect("session info payload")
    }

    async fn next_session_removed(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> SessionInfo {
        let event = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = rx.recv().await.expect("receive session event");
                if event.event == protocol::event::SESSION_REMOVED {
                    break event;
                }
            }
        })
        .await
        .expect("session_removed event");
        serde_json::from_value(event.payload["session"].clone()).expect("session info payload")
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
    async fn session_new_metadata_is_validated_and_exposed() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });
        let expected = metadata(&[("owner", "cli"), ("ticket", "DMD-1356")]);

        let created = registry
            .create(SessionNewParams {
                metadata: expected.clone(),
                ..params()
            })
            .await
            .expect("create session with metadata");
        assert_eq!(created.metadata, expected);
        assert_eq!(
            registry
                .inspect(&created.id)
                .await
                .expect("inspect")
                .metadata,
            expected
        );
        let listed = registry.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].metadata, expected);

        let invalid: BTreeMap<String, String> = (0..33)
            .map(|index| (format!("key-{index}"), "value".to_owned()))
            .collect();
        let err = registry
            .create(SessionNewParams {
                metadata: invalid,
                ..params()
            })
            .await
            .expect_err("too many metadata keys must be rejected");
        assert_eq!(err.code, "bad_request");
        assert!(
            err.msg.contains("metadata"),
            "metadata validation error must be clear: {err:?}"
        );

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn set_metadata_merges_deletes_updates_timestamp_and_emits_event() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });
        let created = registry
            .create(SessionNewParams {
                metadata: metadata(&[("drop", "soon"), ("keep", "yes"), ("ticket", "old")]),
                ..params()
            })
            .await
            .expect("create session");
        let before_updated_at = created.updated_at.clone();
        let mut events = registry.subscribe();
        let expected = metadata(&[("keep", "yes"), ("owner", "daemon"), ("ticket", "new")]);

        let result = registry
            .set_metadata(
                &created.id,
                metadata_patch(&[
                    ("drop", None),
                    ("owner", Some("daemon")),
                    ("ticket", Some("new")),
                ]),
            )
            .await
            .expect("set metadata");

        assert_eq!(result.session.metadata, expected);
        assert_ne!(result.session.updated_at, before_updated_at);
        assert_eq!(
            registry
                .inspect(&created.id)
                .await
                .expect("inspect")
                .metadata,
            expected
        );
        let event_info = next_session_updated(&mut events).await;
        assert_eq!(event_info.id, created.id);
        assert_eq!(event_info.metadata, expected);

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn set_metadata_unknown_session_returns_not_found() {
        let registry = SessionRegistry::default();

        let err = registry
            .set_metadata(&SessionId("s-missing".to_owned()), BTreeMap::new())
            .await
            .expect_err("unknown session id must fail");

        assert_eq!(err.code, "session_not_found");
    }

    #[tokio::test]
    async fn create_with_name_trims_and_stores_it() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(SessionNewParams {
                name: Some("  triage build  ".to_owned()),
                ..params()
            })
            .await
            .expect("create session");

        assert_eq!(created.name.as_deref(), Some("triage build"));

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn rename_sets_then_clears_name_updates_timestamp_and_emits_event() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });
        let created = registry.create(params()).await.expect("create session");
        assert_eq!(created.name, None);
        let before_updated_at = created.updated_at.clone();
        let mut events = registry.subscribe();

        let renamed = registry
            .rename(&created.id, Some("  feature work  ".to_owned()))
            .await
            .expect("rename session");
        assert_eq!(renamed.session.name.as_deref(), Some("feature work"));
        assert_ne!(renamed.session.updated_at, before_updated_at);
        let event_info = next_session_updated(&mut events).await;
        assert_eq!(event_info.id, created.id);
        assert_eq!(event_info.name.as_deref(), Some("feature work"));

        // An all-whitespace (or `None`) name clears it back to id-only display.
        let cleared = registry
            .rename(&created.id, Some("   ".to_owned()))
            .await
            .expect("clear name");
        assert_eq!(cleared.session.name, None);
        assert_eq!(
            registry.inspect(&created.id).await.expect("inspect").name,
            None
        );

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn rename_rejects_overlong_and_control_character_names() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });
        let created = registry.create(params()).await.expect("create session");

        let too_long = registry
            .rename(&created.id, Some("x".repeat(MAX_SESSION_NAME_BYTES + 1)))
            .await
            .expect_err("overlong name must fail");
        assert_eq!(too_long.code, "bad_request");

        let control = registry
            .rename(&created.id, Some("line1\nline2".to_owned()))
            .await
            .expect_err("control character must fail");
        assert_eq!(control.code, "bad_request");

        // A rejected rename leaves the prior name untouched.
        assert_eq!(
            registry.inspect(&created.id).await.expect("inspect").name,
            None
        );

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn rename_unknown_session_returns_not_found() {
        let registry = SessionRegistry::default();

        let err = registry
            .rename(&SessionId("s-missing".to_owned()), Some("x".to_owned()))
            .await
            .expect_err("unknown session id must fail");

        assert_eq!(err.code, "session_not_found");
    }

    #[tokio::test]
    async fn invalid_metadata_rejected_for_create_or_set_and_set_leaves_session_unchanged() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });
        let mut invalid_create = BTreeMap::new();
        invalid_create.insert("owner".to_owned(), "x".repeat(4097));
        let err = registry
            .create(SessionNewParams {
                metadata: invalid_create,
                ..params()
            })
            .await
            .expect_err("oversized metadata value must be rejected");
        assert_eq!(err.code, "bad_request");
        assert!(registry.list().await.is_empty());

        let created = registry
            .create(SessionNewParams {
                metadata: metadata(&[("owner", "cli")]),
                ..params()
            })
            .await
            .expect("create valid session");
        let original = created.metadata.clone();
        let original_updated_at = created.updated_at.clone();
        let err = registry
            .set_metadata(
                &created.id,
                BTreeMap::from([("x".repeat(65), Some("bad".to_owned()))]),
            )
            .await
            .expect_err("oversized metadata key must be rejected");
        assert_eq!(err.code, "bad_request");

        let inspected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(inspected.metadata, original);
        assert_eq!(
            inspected.updated_at, original_updated_at,
            "failed metadata patch must not mutate the session"
        );

        let _ = registry.stop(&created.id).await;

        let key_64_bytes = "é".repeat(32);
        assert_eq!(key_64_bytes.len(), 64);
        let accepted = registry
            .create(SessionNewParams {
                metadata: BTreeMap::from([(key_64_bytes, "byte-boundary".to_owned())]),
                ..params()
            })
            .await
            .expect("64-byte UTF-8 metadata key is accepted");
        let _ = registry.stop(&accepted.id).await;

        let key_66_bytes = "é".repeat(33);
        assert_eq!(key_66_bytes.len(), 66);
        let err = registry
            .create(SessionNewParams {
                metadata: BTreeMap::from([(key_66_bytes, "too-long".to_owned())]),
                ..params()
            })
            .await
            .expect_err("metadata key limit is measured in bytes");
        assert_eq!(err.code, "bad_request");

        let serialized_too_large: BTreeMap<String, String> = (0..super::MAX_SESSION_METADATA_KEYS)
            .map(|index| (format!("key-{index:02}"), "x".repeat(512)))
            .collect();
        assert!(
            serde_json::to_vec(&serialized_too_large)
                .expect("metadata serializes")
                .len()
                > super::MAX_SESSION_METADATA_SERIALIZED_BYTES
        );
        let err = registry
            .create(SessionNewParams {
                metadata: serialized_too_large,
                ..params()
            })
            .await
            .expect_err("metadata serialized size limit must be enforced");
        assert_eq!(err.code, "bad_request");
        assert!(
            err.msg.contains("serialized size"),
            "serialized-size rejection should be clear: {err:?}"
        );
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
            name: None,
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
                name: None,
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
                name: None,
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
                name: None,
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
                name: None,
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
                name: None,
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
                name: None,
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
                name: None,
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
                name: None,
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
                name: None,
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
                name: None,
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
                name: None,
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
    async fn remove_worktree_removes_an_owned_idle_worktree() {
        let (registry, repo) = project_registry("wt-remove");
        let info = registry
            .create(SessionNewParams {
                cwd: Some(repo.clone()),
                name: None,
                branch: Some("feat/x".to_owned()),
                ..params()
            })
            .await
            .expect("worktree session created");
        let worktree = info.worktree_path.clone().expect("worktree path");
        assert!(worktree.exists());
        // Stop the session so it is terminal; its binding (ownership proof) stays.
        registry.stop(&info.id).await.expect("stop session");

        let result = registry
            .remove_worktree(&worktree)
            .await
            .expect("remove owned worktree");
        assert!(result.removed, "the owned worktree was removed");
        assert!(!worktree.exists(), "the worktree directory is gone");
    }

    #[tokio::test]
    async fn remove_worktree_refuses_a_live_session() {
        let (registry, repo) = project_registry("wt-remove-live");
        let info = registry
            .create(SessionNewParams {
                cwd: Some(repo.clone()),
                name: None,
                branch: Some("feat/x".to_owned()),
                ..params()
            })
            .await
            .expect("worktree session created");
        let worktree = info.worktree_path.clone().expect("worktree path");
        // The session is left RUNNING — it is live in the worktree.

        let err = registry
            .remove_worktree(&worktree)
            .await
            .expect_err("a live worktree is refused");
        assert_eq!(err.code, "worktree_in_use");
        assert!(
            worktree.exists(),
            "a live session's worktree is left on disk"
        );
    }

    #[tokio::test]
    async fn remove_worktree_refuses_an_unowned_path() {
        // The main checkout has no worktree binding, so it is not pohunek-owned and
        // must be refused rather than removed.
        let (registry, repo) = project_registry("wt-remove-unowned");
        let err = registry
            .remove_worktree(&repo)
            .await
            .expect_err("an unowned path is refused");
        assert_eq!(err.code, "worktree_not_owned");
        assert!(repo.exists(), "the main checkout is untouched");
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
    #[expect(
        clippy::too_many_lines,
        reason = "tracked for session module decomposition"
    )]
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
        let release = config_dir.join("agent-state-coalesce.release");
        write_host_hook(
            &config_dir,
            "agent-state",
            &format!(
                "#!/bin/sh\nprintf 'start:%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\nif [ \"$POHUNEK_ACTIVITY\" = working ]; then\n  while [ ! -f {} ]; do sleep 0.02; done\nfi\nprintf 'done:%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\n",
                marker.display(),
                release.display(),
                marker.display(),
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
        let contents = wait_for_file_contains(&marker, "start:working").await;
        assert_eq!(
            contents.lines().collect::<Vec<_>>(),
            vec!["start:working"],
            "the first hook must be in flight before the flap sequence starts"
        );
        registry
            .record_activity(&created.id, transition(AgentActivity::Blocked))
            .await;
        registry
            .record_activity(&created.id, transition(AgentActivity::Idle))
            .await;
        registry
            .record_activity(&created.id, transition(AgentActivity::Blocked))
            .await;
        fs::write(&release, "").expect("release first hook");

        let contents = wait_for_file_contains(&marker, "done:blocked").await;
        assert_eq!(
            contents.lines().collect::<Vec<_>>(),
            vec![
                "start:working",
                "done:working",
                "start:blocked",
                "done:blocked"
            ],
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
    async fn remove_evicts_an_already_stopped_session() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");
        registry.stop(&created.id).await.expect("stop session");
        let mut events = registry.subscribe();

        let removed = registry.remove(&created.id).await.expect("remove session");

        assert!(removed.removed);
        // The session was already terminal, so removal did not stop it again.
        assert!(!removed.stopped);
        let event = next_session_removed(&mut events).await;
        assert_eq!(event.id, created.id);
        let err = registry
            .inspect(&created.id)
            .await
            .expect_err("removed session is gone");
        assert_eq!(err.code, "session_not_found");
    }

    #[tokio::test]
    async fn remove_stops_a_live_session_then_evicts() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });

        let created = registry.create(params()).await.expect("create session");

        let removed = registry.remove(&created.id).await.expect("remove session");

        assert!(removed.removed);
        // The session was still live, so removal stopped it first.
        assert!(removed.stopped);
        let err = registry
            .inspect(&created.id)
            .await
            .expect_err("removed session is gone");
        assert_eq!(err.code, "session_not_found");
    }

    #[tokio::test]
    async fn remove_unknown_session_is_session_not_found() {
        let registry = SessionRegistry::default();

        let err = registry
            .remove(&SessionId("s-missing".to_owned()))
            .await
            .expect_err("unknown session cannot be removed");

        assert_eq!(err.code, "session_not_found");
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
        };

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
    fn bare_codex_launch_receives_initial_input_as_prompt_arg() {
        let resolved = crate::agent::ProfileRegistry::default()
            .resolve_agent("codex")
            .expect("resolve bare codex");
        let plan = super::plan_initial_input_delivery(
            &resolved,
            pty_command("codex", []),
            Some("# Pohunek Assistant".to_owned()),
        );

        assert_eq!(plan.command.args, vec!["# Pohunek Assistant".to_owned()]);
        assert_eq!(plan.pending_initial_input, None);
    }

    #[test]
    fn bare_claude_launch_receives_initial_input_as_prompt_arg() {
        let resolved = crate::agent::ProfileRegistry::default()
            .resolve_agent("claude")
            .expect("resolve bare claude");
        let plan = super::plan_initial_input_delivery(
            &resolved,
            pty_command("claude", []),
            Some("# Pohunek Assistant".to_owned()),
        );

        assert_eq!(plan.command.args, vec!["# Pohunek Assistant".to_owned()]);
        assert_eq!(plan.pending_initial_input, None);
    }

    #[test]
    fn shell_launch_keeps_initial_input_for_pty_injection() {
        let resolved = crate::agent::ProfileRegistry::default()
            .resolve_agent("shell")
            .expect("resolve shell");
        let plan = super::plan_initial_input_delivery(
            &resolved,
            pty_command("/bin/sh", ["-c", "sleep 30"]),
            Some("hello shell".to_owned()),
        );

        assert_eq!(plan.pending_initial_input.as_deref(), Some("hello shell"));
    }

    #[test]
    fn host_profile_launch_keeps_initial_input_for_pty_injection() {
        let agents_dir = temp_dir("profile-initial-prompt-agents");
        fs::write(
            agents_dir.join("wrapped-codex.toml"),
            "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
        )
        .expect("write profile");
        let registry = crate::agent::ProfileRegistry::new(Some(agents_dir));
        let resolved = registry
            .resolve_agent("wrapped-codex")
            .expect("resolve profile");

        let plan = super::plan_initial_input_delivery(
            &resolved,
            pty_command("/bin/sh", ["-c", "sleep 30"]),
            Some("# Pohunek Assistant".to_owned()),
        );

        assert_eq!(
            plan.command.args,
            vec!["-c".to_owned(), "sleep 30".to_owned()]
        );
        assert_eq!(
            plan.pending_initial_input.as_deref(),
            Some("# Pohunek Assistant")
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

    #[tokio::test]
    async fn report_native_id_ignores_reports_from_a_different_agent_base() {
        let store_path = temp_store_path("report-agent-mismatch");
        let agents_dir = temp_resumable_agents_dir("report-agent-mismatch");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(resumable_params())
            .await
            .expect("create claude session");

        let claude_report = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: "claude-native".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(claude_report.recorded);

        let codex_report = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "codex".to_owned(),
                native_session_id: "codex-thread".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(
            !codex_report.recorded,
            "a codex hook must not overwrite a claude session binding"
        );

        let inspected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(
            inspected.native_session_id.as_deref(),
            Some("claude-native")
        );

        let persisted = crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load store");
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            persisted[0].native_session_id.as_deref(),
            Some("claude-native")
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

    #[cfg(unix)]
    fn temp_agent_that_exits_then_resumes(tag: &str, marker: &std::path::Path) -> PathBuf {
        let runtime = temp_dir(&format!("{tag}-runtime"));
        let script = runtime.join("resume-agent");
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\ncase \" $* \" in *\" --resume \"*) sleep 30 ;; *) sleep 0.2; exit 0 ;; esac\n",
                marker.display()
            ),
        );
        temp_agents_dir_with(
            tag,
            "resumable",
            &format!(
                "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"sonnet\"]\n",
                script.display()
            ),
        )
    }

    fn resumable_params() -> SessionNewParams {
        SessionNewParams {
            name: None,
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
            name: None,
            agent: "noresume".to_owned(),
            agent_base: AgentKind::Codex,
            cwd: temp_dir("noresume-binding-cwd"),
            cols: 80,
            rows: 24,
            native_session_id: Some("native-ignored".to_owned()),
            native_session_path: None,
            project_id: None,
            is_linked_worktree: None,
            metadata: BTreeMap::new(),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn process_exit_after_daemon_shutdown_starts_keeps_resume_binding() {
        let store_path = temp_store_path("shutdown-keeps-binding");
        let agents_dir = temp_resumable_agents_dir("shutdown-keeps-binding");
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
                native_session_id: "native-shutdown".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(recorded.recorded, "native id captured");
        assert_eq!(
            crate::store::Store::new(store_path.clone())
                .load_resume()
                .expect("load before shutdown")
                .len(),
            1,
            "precondition: captured session has one resume binding"
        );

        registry.begin_daemon_shutdown();
        registry
            .record_exit(
                &created.id,
                PtyExit {
                    exit_code: None,
                    success: false,
                },
                false,
            )
            .await;

        let persisted = crate::store::Store::new(store_path)
            .load_resume()
            .expect("load after shutdown exit");
        let _ = registry.stop(&created.id).await;
        terminate_pid(created.pid);

        assert_eq!(
            persisted.len(),
            1,
            "a PTY exit observed during daemon shutdown must keep the restart-resume binding"
        );
        assert_eq!(persisted[0].session_id, created.id.0);
        assert_eq!(
            persisted[0].native_session_id.as_deref(),
            Some("native-shutdown")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_session_can_be_explicitly_resumed_with_same_id() {
        let store_path = temp_store_path("manual-resume");
        let marker = temp_dir("manual-resume-marker").join("argv.txt");
        let agents_dir = temp_agent_that_exits_then_resumes("manual-resume", &marker);
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path),
            agents_dir: Some(agents_dir),
            socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
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
                native_session_id: "native-manual".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(recorded.recorded, "native id captured");

        let done = registry
            .wait_for_exit(&created.id, Duration::from_secs(2))
            .await
            .expect("session exits");
        assert_eq!(done.state, SessionState::Done);

        let resumed = registry
            .resume(&created.id)
            .await
            .expect("resume terminal session");
        assert_eq!(resumed.id, created.id);
        assert_eq!(resumed.state, SessionState::Running);
        assert_eq!(resumed.native_session_id.as_deref(), Some("native-manual"));

        let argv = wait_for_file_contains(&marker, "native-manual").await;
        assert!(
            argv.contains("--resume") && argv.contains("native-manual"),
            "resume argv must target the captured native id: {argv:?}"
        );

        let _ = registry.stop(&created.id).await;
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
    async fn set_metadata_after_capture_updates_persisted_binding() {
        let store_path = temp_store_path("metadata-binding");
        let agents_dir = temp_resumable_agents_dir("metadata-binding");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });
        let created = registry
            .create(SessionNewParams {
                metadata: metadata(&[("owner", "cli"), ("ticket", "old")]),
                ..resumable_params()
            })
            .await
            .expect("create session");
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: "native-metadata".to_owned(),
                transcript_path: None,
            })
            .await;
        assert!(recorded.recorded);

        let expected = metadata(&[("owner", "daemon"), ("reviewer", "qa"), ("ticket", "old")]);
        registry
            .set_metadata(
                &created.id,
                metadata_patch(&[("owner", Some("daemon")), ("reviewer", Some("qa"))]),
            )
            .await
            .expect("set metadata after capture");

        let persisted = crate::store::Store::new(store_path)
            .load_resume()
            .expect("load binding");
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].session_id, created.id.0);
        assert_eq!(persisted[0].metadata, expected);

        let _ = registry.stop(&created.id).await;
    }

    #[tokio::test]
    async fn resume_binding_restores_metadata_from_store() {
        let store_path = temp_store_path("resume-metadata");
        let expected = metadata(&[("owner", "daemon"), ("ticket", "DMD-1356")]);
        let store = crate::store::Store::new(store_path.clone());
        store
            .record_resume(&crate::store::ResumeBinding {
                session_id: "s-42".to_owned(),
                name: None,
                agent: "claude".to_owned(),
                agent_base: AgentKind::Claude,
                cwd: temp_dir("resume-metadata-cwd"),
                cols: 80,
                rows: 24,
                native_session_id: Some("native-metadata".to_owned()),
                native_session_path: None,
                project_id: None,
                is_linked_worktree: None,
                metadata: expected.clone(),
                program: "/bin/sh".to_owned(),
                args: vec!["-c".to_owned(), "sleep 30".to_owned()],
                input_rules: crate::store::StoredInputRules::default(),
                resume_mode: Some(ResumeMode::Flag),
                ref_kind: Some(SessionRefKind::Id),
                resumable: true,
            })
            .expect("seed resume binding");
        let binding = crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load resume binding")
            .into_iter()
            .next()
            .expect("one binding");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path),
            ..SessionRegistryConfig::default()
        });

        let resumed = registry
            .resume_binding(binding)
            .await
            .expect("resume binding");

        assert_eq!(resumed.metadata, expected);
        assert_eq!(
            registry
                .inspect(&resumed.id)
                .await
                .expect("inspect resumed")
                .metadata,
            expected
        );

        let _ = registry.stop(&resumed.id).await;
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
                name: None,
                agent: "claude".to_owned(),
                agent_base: AgentKind::Claude,
                cwd: PathBuf::from("/tmp"),
                cols: 80,
                rows: 24,
                native_session_id: None,
                native_session_path: None,
                project_id: None,
                is_linked_worktree: None,
                metadata: BTreeMap::new(),
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
                name: None,
                agent: "profiled".to_owned(),
                agent_base: AgentKind::Claude,
                cwd: dir.clone(),
                cols: 80,
                rows: 24,
                native_session_id: Some("native-44".to_owned()),
                native_session_path: None,
                project_id: None,
                is_linked_worktree: None,
                metadata: BTreeMap::new(),
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
        write_resume_agent_script(&script_v1, &marker_v1);
        write_resume_agent_script(&script_v2, &marker_v2);
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
                name: None,
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
