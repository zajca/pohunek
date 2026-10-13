//! Serves the private daemon-worker Unix protocol.

// Rust guideline compliant 2026-10-13

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pohunek_paths::{validate_socket_path, Platform, SocketKind};
use pohunek_platform::filesystem::{
    AdvisoryLock, EntryIdentity, EntryKind, FsError, MoveOutcome, StageOutcome, TrustedDir,
};
use pohunek_platform::process::{
    survey_runtime, sweep_runtime, Error as PlatformProcessError, HostInspector, ProcessCoalitions,
    ProcessIdentity as OsProcessIdentity, ProcessInspector, SkipReason, Skipped, SpawnId,
    StartIdentity, SweepReport, SweepRequest, MAX_SWEEP_GRACE,
};
use pohunek_worker_protocol as protocol;
use pohunek_worker_protocol::is_task_text;
use protocol::{
    ActiveIdentityClaim, AncestryMatcher, AttachStart, BackgroundSnapshot, Capability, CloseReason,
    ControlCode, ControlError, ControlEvent, ControlMessage, ControlReader, ControlRequest,
    ControlResponse, ControlWriter, Cursor, DaemonId, DataFrame, DataToken, Dimensions, EventKind,
    ExitStatus, FrameError, FrameHeader, FrameKind, HookAction, HookSchema, Initialize,
    InspectSnapshot, LaunchProcess, LeaseChallenge, LeaseId, OutputGap,
    ProcessIdentity as WireProcessIdentity, PtyRelation, ReleasedIdentityClaim,
    ReportedLaunchIdentity, ReportedNativeReference, RequestKind, ResponseKind,
    RuntimePhase as WireRuntimePhase, RuntimeScope, SessionId, StreamId, StreamMode, SubagentField,
    SubagentPhase as WireSubagentPhase, SubagentSequence, SubagentSnapshot,
    TerminalSnapshot as WireTerminalSnapshot, TransactionId, Version, WorkerId, WorkerInstanceId,
    WriteAck, WriteId, ATTACH_SNAPSHOT_VERSION, BACKGROUND_MAX_ITEMS, SUPPORTED_RANGE,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, oneshot, watch, Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{event, Level};

use crate::background::{self, Candidate, Dropped};
use crate::identity::{PeerContext, RejectReason};
use crate::journal::{
    ActiveIdentity, BackgroundOwner, ChildIdentity, JournalRecord, LaunchIdentity,
    NativeReferenceClaim, PendingLaunchClaim, ReleasedIdentity, RuntimeOutcome,
    RuntimePhase as JournalPhase, SubagentPhase as JournalSubagentPhase, SubagentRecord,
    WorkerOrigin,
};
use crate::launch::{self, LaunchClaimStatus};
use crate::notification;
use crate::output::OutputCompletion;
use crate::{
    Command, ControllerLease, EnvBase, Exit, InputFragment, InputPlan, Journal, LeaseError,
    LeaseOwner, OutputChunk, OutputEvent, OutputSubscriber, ProcessIdentity, PtyError, PtyOwner,
    WorkerConfig, WorkerError,
};

/// How long an idle leased control connection may go unverified.
///
/// The lease is exclusive, so a peer that stops speaking must still be proved
/// to exist: otherwise a descriptor inherited from an exited daemon holds the
/// lease indefinitely. The interval trades a periodic pair of cheap kernel
/// lookups against how long a replacement daemon can be blocked.
const LEASE_PEER_RECHECK: Duration = Duration::from_secs(5);

/// Number of worker events buffered per control connection.
const EVENT_BUFFER: usize = 256;
/// Maximum simultaneously running subagents accepted from provider hooks.
const ACTIVE_SUBAGENT_CAPACITY: usize = 64;
/// Recent terminal subagents retained for reconnecting clients.
const TERMINAL_SUBAGENT_CAPACITY: usize = 32;
/// Stops without a recorded start remembered to order a late start.
///
/// A provider whose stop requires a start reports about one such stop per main
/// turn, and a start delivered out of order arrives within seconds of its
/// stop, so only the few most recent stops can still meet their start. The
/// bound keeps the set at a few kilobytes of ids; an evicted stop only means a
/// start arriving after it is taken for a new run.
const ORPHAN_STOP_CAPACITY: usize = 64;
/// Delay between terminal-journal retries after a transient storage failure.
///
/// A quarter second keeps the runtime publicly draining without either a busy
/// loop or a long recovery delay once the journal becomes writable again.
const TERMINAL_JOURNAL_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Upper bound on marker-sweep passes for one force-closed runtime.
///
/// A pass selects its targets before signalling, so a process forked during
/// the pass is only found by the next one. Three passes absorb a short fork
/// chain; a runtime that still yields targets after that is unconfirmed
/// instead of being swept forever.
const MAX_RUNTIME_SWEEP_PASSES: usize = 3;
/// Liveness polling interval inside one sweep's grace windows, and the pause
/// between passes of an unreadable-marker settle window.
///
/// Clamped to the grace. 50 ms notices an exit promptly without busy-reading
/// the process table.
const RUNTIME_SWEEP_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Most processes one unconfirmed cleanup names, in its error and its events.
///
/// Eight identify the blockers on a busy host while keeping the fault detail
/// the daemon receives and the log bounded; the error counts the rest.
const MAX_LOGGED_CLEANUP_BLOCKERS: usize = 8;
/// Maximum accepted provider-native subagent identifier length.
///
/// The same bound the daemon re-checks for background task ids.
const MAX_SUBAGENT_ID_BYTES: usize = protocol::BACKGROUND_TASK_ID_MAX_BYTES;
/// Maximum accepted provider-defined subagent type length.
///
/// The same bound the daemon re-checks for background agent types.
pub(crate) const MAX_SUBAGENT_TYPE_BYTES: usize = protocol::BACKGROUND_AGENT_TYPE_MAX_BYTES;
/// Maximum tasks accepted in one subagent snapshot.
///
/// Claude Code does not cap its main-`Stop` `background_tasks` list. Four
/// times [`ACTIVE_SUBAGENT_CAPACITY`] leaves room for the shells and monitors
/// listed beside the agents. The Claude state hook mirrors this value and
/// sends no snapshot for a longer list; a longer claim is rejected whole,
/// because a truncated list would read as finished agents.
const MAX_SNAPSHOT_TASKS: usize = 256;
/// Maximum accepted byte length of a snapshot task's type label.
///
/// Claude Code's task-type labels are short words such as `subagent`, `shell`,
/// or `MCP task`; 64 bytes leaves room for labels added later. A longer label
/// rejects the whole snapshot.
const MAX_SNAPSHOT_TYPE_BYTES: usize = protocol::BACKGROUND_TASK_TYPE_MAX_BYTES;
/// Maximum accepted byte length of a snapshot task's status.
///
/// Claude Code lists tasks with short statuses such as `running` or
/// `pending`; 32 bytes leaves room for statuses added later. A longer status
/// rejects the whole snapshot.
const MAX_SNAPSHOT_STATUS_BYTES: usize = protocol::BACKGROUND_TASK_STATUS_MAX_BYTES;
/// Snapshot task types that run no agent.
///
/// These are the labels of Claude Code's task-type label map for its
/// agentless tasks: `local_bash` is `shell`, `monitor_mcp` and `monitor_ws`
/// are `monitor`, and `mcp_task` is `MCP task`. Every other type may host an
/// agent that Claude does not list by agent id, such as a teammate or the
/// synchronous child of a background agent, so a snapshot listing one defers
/// unlisted subagents instead of ending them. A label missing here only
/// defers more subagents; a label wrongly added here may end a live one.
const AGENTLESS_TASK_TYPES: &[&str] = &["shell", "monitor", "MCP task"];
/// Most reporting agent processes whose last snapshot sequence is remembered.
///
/// A session's PTY runs one main agent at a time, and each restart of it, or
/// an agent started inside it, is a further reporter, so a session meets few
/// of them. The least recently reporting one is forgotten beyond this bound;
/// a forgotten reporter's never-listed subagents stay deferred through its
/// next snapshot and settle at the earliest on the one after it, so the bound
/// trades a few dozen bytes against that delay.
const SNAPSHOT_REPORTER_CAPACITY: usize = 16;
/// Maximum script bytes read and hashed while freezing and rechecking a launch
/// entrypoint.
///
/// A launcher script is a few hundred bytes. The cap bounds the read and the
/// digest that every launch-claim verification repeats; a larger script cannot
/// be frozen, so its session accepts no launch identity claim.
const MAX_ENTRYPOINT_SCRIPT_BYTES: usize = 1024 * 1024;
/// Maximum outstanding one-use data tokens per worker.
const DATA_TOKEN_CAPACITY: usize = 4_096;
/// Entropy bytes used for opaque worker credentials and worker instance IDs.
const RANDOM_BYTES: usize = 16;
/// Prefix shared by daemon-generated control input identifiers.
const CONTROL_INPUT_PREFIX: &str = "input-";
/// Prefix shared by stream-scoped raw attach input identifiers.
const ATTACH_INPUT_PREFIX: &str = "attach-";
/// `TERM` the worker sets for its PTY child.
///
/// The PTY is rendered by attaching clients' terminals and modeled by the
/// xterm-compatible `vt100` parser, while a service-manager-started worker has
/// no `TERM` of its own. The profile environment may still override it.
const CHILD_TERM: &str = "xterm-256color";
/// Package version recorded in the worker journal.
const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");
const SOCKET_DIRECTORY_MODE: u32 = 0o700;
const SOCKET_MODE: u32 = 0o600;
const SOCKET_LOCK_NAME: &str = ".worker.lock";

/// Fully resolved worker server inputs.
#[derive(Debug, Clone)]
pub struct ServerArgs {
    /// Durable logical session.
    pub session_id: String,
    /// Stable worker process ID.
    pub worker_id: String,
    /// Daemon-issued generation of this worker process.
    pub generation: String,
    /// Owner-private worker socket.
    pub socket_path: PathBuf,
    /// Worker-owned durable journal.
    pub journal_path: PathBuf,
    /// Stable daemon socket exposed to notification hooks.
    pub daemon_socket_path: PathBuf,
    /// Shared hook failure trace that every PTY child receives as
    /// `POHUNEK_HOOK_TRACE_PATH`. The worker never opens it; the state hooks
    /// append to it and ignore a relative path.
    pub hook_trace_path: PathBuf,
    /// Bootstrap and runtime policy.
    pub config: WorkerConfig,
}

/// One bound worker endpoint.
#[derive(Debug)]
pub struct Server {
    listener: UnixListener,
    _socket_lease: SocketLease,
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct SocketLease {
    directory: TrustedDir,
    _authority: AdvisoryLock,
    name: OsString,
    identity: EntryIdentity,
}

#[derive(Debug)]
struct PreparedSocket {
    directory: TrustedDir,
    authority: AdvisoryLock,
    name: OsString,
}

struct PendingSocket<'a> {
    directory: &'a TrustedDir,
    name: OsString,
    identity: EntryIdentity,
    armed: bool,
}

impl<'a> PendingSocket<'a> {
    fn new(directory: &'a TrustedDir, name: OsString, identity: EntryIdentity) -> Self {
        Self {
            directory,
            name,
            identity,
            armed: true,
        }
    }

    fn published_as(&mut self, name: &std::ffi::OsStr) {
        self.name = name.to_os_string();
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingSocket<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(StageOutcome::Staged(entry)) =
                self.directory
                    .stage_random(&self.name, ".pohunek-worker-stale-", self.identity)
            {
                let _ = entry.remove();
            }
        }
    }
}

#[cfg(test)]
mod pending_socket_tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn pending_worker_socket_guard_follows_the_published_name() {
        let temporary =
            pohunek_test_support::tempdir().expect("create worker socket guard fixture");
        std::fs::set_permissions(
            temporary.path(),
            std::fs::Permissions::from_mode(SOCKET_DIRECTORY_MODE),
        )
        .expect("set private fixture mode");
        let directory = TrustedDir::open_absolute(temporary.path(), SOCKET_DIRECTORY_MODE)
            .expect("open worker socket guard fixture");
        let (staged_name, listener, identity) = directory
            .bind_unix_listener_staged(".s", SOCKET_MODE)
            .expect("bind staged worker socket");
        let mut pending = PendingSocket::new(&directory, staged_name.clone(), identity);
        assert_eq!(
            directory
                .move_no_replace(&staged_name, &directory, "worker.sock")
                .expect("publish worker socket"),
            MoveOutcome::Moved
        );
        pending.published_as(std::ffi::OsStr::new("worker.sock"));
        drop(listener);
        drop(pending);

        assert!(!temporary.path().join("worker.sock").exists());
    }
}

impl Drop for SocketLease {
    fn drop(&mut self) {
        if let Ok(StageOutcome::Staged(entry)) =
            self.directory
                .stage_random(&self.name, ".pohunek-worker-stale-", self.identity)
        {
            let _ = entry.remove();
        }
    }
}

impl Server {
    /// Binds and journals an uninitialized worker.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] for invalid IDs, unsafe paths, or bind failure.
    #[expect(
        clippy::too_many_lines,
        reason = "validation, locked stale recovery, staged socket activation, and bootstrap journaling form one startup transaction"
    )]
    pub async fn bind(args: ServerArgs) -> Result<Self, WorkerError> {
        args.config.validate()?;
        let platform = Platform::current()?;
        validate_socket_path(&args.daemon_socket_path, platform, SocketKind::Daemon)?;
        if pohunek_paths::valid_worker_session_id(&args.session_id).is_none() {
            return Err(WorkerError::InvalidSessionId(args.session_id));
        }
        if pohunek_paths::valid_worker_id(&args.worker_id).is_none() {
            return Err(WorkerError::InvalidWorkerId(args.worker_id));
        }
        if pohunek_paths::valid_worker_generation(&args.generation).is_none() {
            return Err(WorkerError::InvalidGeneration(args.generation));
        }
        let origin = worker_origin(args.generation)?;
        let session_id = SessionId::new(&args.session_id)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let worker_id = WorkerId::new(&args.worker_id)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let prepared_socket = prepare_socket(&args.socket_path, platform).await?;
        let longest_bind_path =
            pohunek_paths::longest_staged_socket_path(prepared_socket.directory.path());
        validate_socket_path(&longest_bind_path, platform, SocketKind::Worker)?;
        let (bind_name, std_listener, socket_identity) = prepared_socket
            .directory
            .bind_unix_listener_staged(pohunek_paths::STAGED_SOCKET_PREFIX, SOCKET_MODE)?;
        let mut pending = PendingSocket::new(
            &prepared_socket.directory,
            bind_name.clone(),
            socket_identity,
        );
        let bind_path = prepared_socket.directory.path().join(&bind_name);
        std_listener
            .set_nonblocking(true)
            .map_err(|source| WorkerError::Socket {
                path: bind_path.clone(),
                source,
            })?;
        let listener =
            UnixListener::from_std(std_listener).map_err(|source| WorkerError::Socket {
                path: bind_path.clone(),
                source,
            })?;
        match prepared_socket.directory.move_no_replace(
            &bind_name,
            &prepared_socket.directory,
            &prepared_socket.name,
        ) {
            Ok(MoveOutcome::Moved) => pending.published_as(&prepared_socket.name),
            Err(error @ FsError::CommittedDurabilityUncertain { .. }) => {
                pending.published_as(&prepared_socket.name);
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
            Ok(MoveOutcome::DestinationExists) => {
                drop(listener);
                return Err(WorkerError::Socket {
                    path: args.socket_path.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        "worker socket destination became occupied while binding",
                    ),
                });
            }
            Ok(_) => {
                return Err(WorkerError::Filesystem {
                    path: args.socket_path.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "unsupported worker socket activation outcome",
                    ),
                });
            }
        }
        if prepared_socket.directory.entry_identity_with_mode(
            &prepared_socket.name,
            EntryKind::Socket,
            SOCKET_MODE,
        )? != Some(socket_identity)
        {
            drop(listener);
            return Err(WorkerError::Filesystem {
                path: args.socket_path.clone(),
                source: std::io::Error::other("activated worker socket identity changed"),
            });
        }
        pending.disarm();
        drop(pending);
        let socket_lease = SocketLease {
            directory: prepared_socket.directory,
            _authority: prepared_socket.authority,
            name: prepared_socket.name,
            identity: socket_identity,
        };

        let worker_start = process_start(std::process::id())?;
        let boot_identity = HostInspector::new()
            .boot_identity()
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let journal = Journal::new(&args.journal_path);
        let mut record = JournalRecord::bootstrap(
            args.session_id,
            args.worker_id,
            origin,
            std::process::id(),
            worker_start.to_string(),
            boot_identity.to_string(),
            (
                u16::try_from(SUPPORTED_RANGE.minimum().get()).unwrap_or(u16::MAX),
                u16::try_from(SUPPORTED_RANGE.maximum().get()).unwrap_or(u16::MAX),
            ),
            timestamp(),
        );
        record.worker_spawn_id = own_spawn_id();
        record.worker_coalitions = own_coalitions();
        journal.write(&record)?;
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let (initialized_tx, _) = watch::channel(false);
        let (lease_epoch_tx, _) = watch::channel(0_u64);
        let shared = Arc::new(Shared {
            session_id,
            worker_id,
            worker_process: WireProcessIdentity {
                pid: std::process::id(),
                start_identity: worker_start,
            },
            socket_path: args.socket_path,
            daemon_socket_path: args.daemon_socket_path,
            hook_trace_path: args.hook_trace_path,
            config: args.config,
            journal,
            state: AsyncMutex::new(State::new(record)),
            lease: ControllerLease::new(),
            tokens: Mutex::new(TokenState::new()),
            launch_rule: OnceLock::new(),
            events,
            next_event: AtomicU64::new(1),
            started: Instant::now(),
            initialized_tx,
            lease_epoch_tx,
            state_changed: Notify::new(),
            shutdown: CancellationToken::new(),
            #[cfg(test)]
            starting_gate: Mutex::new(None),
        });
        Ok(Self {
            listener,
            _socket_lease: socket_lease,
            shared,
        })
    }

    /// Returns the bound worker socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.shared.socket_path
    }

    /// Serves connections until terminal acknowledgement or retention expiry.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::InitializeTimeout`] when no valid initialization
    /// arrives before the configured deadline, or a socket error occurs.
    pub async fn serve(self) -> Result<(), WorkerError> {
        let deadline = tokio::time::sleep(self.shared.config.initialize_deadline);
        tokio::pin!(deadline);
        let mut initialized = self.shared.initialized_tx.subscribe();
        let mut launch_retry =
            tokio::time::interval(self.shared.config.launch_claim_retry_interval);
        launch_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                () = self.shared.shutdown.cancelled() => return Ok(()),
                _ = launch_retry.tick() => {
                    if let Err(error) = retry_launch_claims(&self.shared, |claim| {
                        verify_launch_claim(&self.shared, claim)
                    }).await {
                        event!(name: "worker.launch_identity.retry.failed", Level::ERROR,
                            error.type = "journal", error.message = %error,
                            "launch identity retry commit failed: {{error.message}}");
                    }
                }
                result = initialized.changed(), if !*initialized.borrow() => {
                    if result.is_err() {
                        return Err(WorkerError::Protocol(
                            "initialization state channel closed".to_owned(),
                        ));
                    }
                }
                () = &mut deadline, if !*initialized.borrow() => {
                    self.shared.record_never_initialized().await?;
                    return Err(WorkerError::InitializeTimeout);
                }
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.map_err(|source| WorkerError::Socket {
                        path: self.shared.socket_path.clone(),
                        source,
                    })?;
                    let shared = Arc::clone(&self.shared);
                    tokio::spawn(async move {
                        if let Err(error) = serve_connection(shared, stream).await {
                            event!(
                                name: "worker.connection.failed",
                                Level::WARN,
                                error.type = "private_protocol",
                                error.message = %error,
                                "worker connection failed: {{error.message}}",
                            );
                        }
                    });
                }
            }
        }
    }
}

/// Binds and runs one worker server.
///
/// # Errors
///
/// Returns [`WorkerError`] from endpoint setup or serving.
pub async fn run(args: ServerArgs) -> Result<(), WorkerError> {
    Server::bind(args).await?.serve().await
}

#[derive(Debug)]
struct Shared {
    session_id: SessionId,
    worker_id: WorkerId,
    worker_process: WireProcessIdentity,
    socket_path: PathBuf,
    daemon_socket_path: PathBuf,
    hook_trace_path: PathBuf,
    config: WorkerConfig,
    journal: Journal,
    state: AsyncMutex<State>,
    lease: ControllerLease,
    tokens: Mutex<TokenState>,
    /// Launch-process rule frozen at `Initialize` from the session's schema.
    launch_rule: OnceLock<LaunchRule>,
    events: broadcast::Sender<ControlEvent>,
    next_event: AtomicU64,
    started: Instant,
    initialized_tx: watch::Sender<bool>,
    lease_epoch_tx: watch::Sender<u64>,
    state_changed: Notify,
    shutdown: CancellationToken,
    /// Pauses `initialize` between the `Starting` commit and the child spawn.
    #[cfg(test)]
    starting_gate: Mutex<Option<StartingGate>>,
}

/// Test-only pause point that makes the transient `Starting` phase observable.
///
/// `initialize` sends on `reached` once `Starting` is committed and the state
/// lock is released, then waits on `proceed` before it spawns the child.
#[cfg(test)]
#[derive(Debug)]
struct StartingGate {
    reached: oneshot::Sender<()>,
    proceed: oneshot::Receiver<()>,
}

impl Shared {
    async fn record_never_initialized(&self) -> Result<(), WorkerError> {
        let mut state = self.state.lock().await;
        state.phase = WireRuntimePhase::Faulted;
        state.journal.phase = JournalPhase::NeverInitialized;
        state.journal.updated_at = timestamp();
        persist(self.journal.clone(), state.journal.clone()).await
    }

    fn emit(&self, kind: EventKind) {
        let sequence = self.next_event.fetch_add(1, Ordering::Relaxed);
        let _ = self.events.send(ControlEvent {
            event_sequence: sequence,
            kind,
        });
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn bump_lease_epoch(&self) {
        let next = self.lease_epoch_tx.borrow().saturating_add(1);
        self.lease_epoch_tx.send_replace(next);
    }
}

#[derive(Debug)]
struct State {
    phase: WireRuntimePhase,
    worker_instance_id: Option<WorkerInstanceId>,
    pty: Option<PtyOwner>,
    initialize_transaction: Option<TransactionId>,
    launch_agent_base: Option<String>,
    /// Hook schema the daemon resolved for this runtime; no hook report is
    /// admitted without one.
    hook_schema: Option<&'static HookSchema>,
    /// Public protocol version the daemon selected for this runtime's hooks.
    public_protocol_version: Option<u32>,
    stop_grace: Duration,
    terminal_retention: Duration,
    exit: Option<ExitStatus>,
    stop_requested: bool,
    journal: JournalRecord,
    /// Subagent stops kept out of the journal because no start preceded them.
    orphan_stops: OrphanStops,
    /// Unjournaled evidence about journaled subagents that subagent snapshots
    /// reconcile against.
    subagent_evidence: SubagentEvidence,
    /// Newest identity sequence of each recently reporting process.
    identity_marks: IdentityMarks,
}

impl State {
    fn new(journal: JournalRecord) -> Self {
        Self {
            phase: WireRuntimePhase::Uninitialized,
            worker_instance_id: None,
            pty: None,
            initialize_transaction: None,
            launch_agent_base: None,
            hook_schema: None,
            public_protocol_version: None,
            stop_grace: crate::config::DEFAULT_STOP_GRACE,
            terminal_retention: Duration::from_hours(24),
            exit: None,
            stop_requested: false,
            journal,
            orphan_stops: OrphanStops::default(),
            subagent_evidence: SubagentEvidence::default(),
            identity_marks: IdentityMarks::default(),
        }
    }
}

#[derive(Debug)]
struct TokenState {
    grants: HashMap<DataToken, DataGrant>,
    maximum: usize,
}

impl TokenState {
    fn new() -> Self {
        Self {
            grants: HashMap::with_capacity(DATA_TOKEN_CAPACITY),
            maximum: DATA_TOKEN_CAPACITY,
        }
    }

    #[cfg(test)]
    fn with_maximum(maximum: usize) -> Result<Self, TokenStateError> {
        if maximum == 0 {
            return Err(TokenStateError::InvalidCapacity);
        }
        Ok(Self {
            grants: HashMap::with_capacity(maximum),
            maximum,
        })
    }

    fn insert(
        &mut self,
        token: DataToken,
        grant: DataGrant,
        now_ms: u64,
    ) -> Result<(), TokenStateError> {
        if grant.expires_at_ms <= now_ms {
            return Err(TokenStateError::AlreadyExpired);
        }
        self.purge_expired(now_ms);
        if self.grants.contains_key(&token) {
            return Err(TokenStateError::Duplicate);
        }
        if self.grants.len() >= self.maximum {
            return Err(TokenStateError::Full {
                maximum: self.maximum,
            });
        }
        self.grants.insert(token, grant);
        Ok(())
    }

    /// Returns the lease owner a live grant was minted for, without using it.
    ///
    /// The redeeming peer is checked against this before [`TokenState::redeem`]
    /// runs, because `redeem` removes the grant whatever its validation says. A
    /// foreign redemption attempt must not burn a legitimate token.
    fn grant_owner(&mut self, token: &DataToken, now_ms: u64) -> Option<LeaseOwner> {
        self.purge_expired(now_ms);
        self.grants
            .get(token)
            .map(|grant| grant.lease_owner.clone())
    }

    fn redeem(
        &mut self,
        token: &DataToken,
        now_ms: u64,
        validate: impl FnOnce(&DataGrant) -> Result<(), WorkerError>,
    ) -> Result<DataGrant, WorkerError> {
        self.purge_expired(now_ms);
        let validation = self
            .grants
            .get(token)
            .ok_or_else(|| {
                WorkerError::Protocol("data token is invalid, expired, or already used".to_owned())
            })
            .and_then(validate);
        let grant = self.grants.remove(token);
        validation?;
        grant.ok_or_else(|| {
            WorkerError::Protocol("data token is invalid, expired, or already used".to_owned())
        })
    }

    fn purge_expired(&mut self, now_ms: u64) -> usize {
        let before = self.grants.len();
        self.grants
            .retain(|_token, grant| grant.expires_at_ms > now_ms);
        before - self.grants.len()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.grants.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum TokenStateError {
    #[cfg(test)]
    #[error("data grant capacity must be nonzero")]
    InvalidCapacity,
    #[error("data grant was already expired when issued")]
    AlreadyExpired,
    #[error("data token is already registered")]
    Duplicate,
    #[error("data grant vault is full at its configured maximum of {maximum}")]
    Full { maximum: usize },
}

#[derive(Debug, Clone)]
struct DataGrant {
    lease_owner: LeaseOwner,
    lease_id: LeaseId,
    lease_epoch: u64,
    version: Version,
    expires_at_ms: u64,
    worker_instance_id: WorkerInstanceId,
    stream_id: StreamId,
    mode: StreamMode,
    after_offset: Option<u64>,
    attach: Option<AttachStart>,
    observation: Option<ObservationGrant>,
}

#[derive(Debug, Clone)]
struct ObservationGrant {
    max_bytes: usize,
    wait: Duration,
}

#[derive(Debug)]
struct Connection {
    peer: Arc<PeerContext>,
    daemon_id: Option<DaemonId>,
    selected_version: Option<Version>,
    capabilities: Vec<Capability>,
    challenge: Option<LeaseChallenge>,
    owner: Option<LeaseOwner>,
    lease_id: Option<LeaseId>,
}

impl Connection {
    fn new(peer: Arc<PeerContext>) -> Self {
        Self {
            peer,
            daemon_id: None,
            selected_version: None,
            capabilities: Vec::new(),
            challenge: None,
            owner: None,
            lease_id: None,
        }
    }
}

async fn serve_connection(shared: Arc<Shared>, mut stream: UnixStream) -> Result<(), WorkerError> {
    // The peer is bound before the classifier byte is read, so no request shape
    // can be parsed from an unattested connection.
    let peer = Arc::new(PeerContext::capture(&stream)?);
    let mut prefix = [0_u8; 1];
    let read = stream
        .read_exact(&mut prefix)
        .await
        .map_err(|source| WorkerError::Protocol(source.to_string()))?;
    if read == 0 {
        return Ok(());
    }
    let prefixed = PrefixStream::new(prefix[0], stream);
    if prefix[0] == b'{' {
        serve_json(shared, prefixed, peer).await
    } else {
        serve_data(shared, prefixed, peer).await
    }
}

async fn serve_json(
    shared: Arc<Shared>,
    stream: PrefixStream<UnixStream>,
    peer: Arc<PeerContext>,
) -> Result<(), WorkerError> {
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = ControlReader::new(read_half);
    let mut writer = ControlWriter::new(write_half);
    let Some(first) = reader
        .read::<serde_json::Value>()
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))?
    else {
        return Ok(());
    };

    match serde_json::from_value::<ControlMessage>(first.clone()) {
        Ok(ControlMessage::Request(request)) => {
            serve_control(Arc::clone(&shared), &mut reader, &mut writer, request, peer).await
        }
        Ok(_) => Err(WorkerError::Protocol(
            "first control message must be a request".to_owned(),
        )),
        Err(_) => serve_identity_hook(shared, &mut writer, first, &peer).await,
    }
}

async fn serve_control<R, W>(
    shared: Arc<Shared>,
    reader: &mut ControlReader<R>,
    writer: &mut ControlWriter<W>,
    first: ControlRequest,
    peer: Arc<PeerContext>,
) -> Result<(), WorkerError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut connection = Connection::new(peer);
    let result = run_control_connection(&shared, reader, writer, first, &mut connection).await;
    // Release the controller lease whenever the control connection ends, for
    // ANY reason: clean EOF, a protocol error, or a failed event write to a
    // daemon that has already disconnected. Releasing only on the EOF branch
    // leaks the lease when an in-flight event write fails first, which makes
    // the worker answer `ControllerBusy` to the replacement daemon for the
    // whole connect deadline and misclassifies a live runtime as a conflict.
    if let Some(owner) = connection.owner.as_ref() {
        shared.lease.release_connection(owner);
        shared.bump_lease_epoch();
    }
    result
}

async fn run_control_connection<R, W>(
    shared: &Arc<Shared>,
    reader: &mut ControlReader<R>,
    writer: &mut ControlWriter<W>,
    first: ControlRequest,
    connection: &mut Connection,
) -> Result<(), WorkerError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut events = shared.events.subscribe();
    let mut next_request = Some(first);

    loop {
        let request = if let Some(request) = next_request.take() {
            Some(request)
        } else if connection.lease_id.is_some() {
            tokio::select! {
                received = reader.read::<ControlMessage>() => {
                    match received.map_err(|error| WorkerError::Protocol(error.to_string()))? {
                        Some(ControlMessage::Request(request)) => Some(request),
                        Some(_) => {
                            return Err(WorkerError::Protocol(
                                "controller sent a non-request message".to_owned(),
                            ));
                        }
                        None => None,
                    }
                }
                event = events.recv() => {
                    match event {
                        Ok(event) => {
                            if !event_visible_to_connection(&event, connection) {
                                continue;
                            }
                            authorize_control_request(shared, connection)?;
                            writer.write(&ControlMessage::Event(event)).await
                                .map_err(|error| WorkerError::Protocol(error.to_string()))?;
                            continue;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => None,
                    }
                }
                () = tokio::time::sleep(LEASE_PEER_RECHECK) => {
                    // A handed-off descriptor can simply go quiet: no request,
                    // no event, no EOF. Without this the lease would be held
                    // until the process exits, and a replacement daemon would
                    // keep getting `ControllerBusy` from a peer that no longer
                    // exists.
                    authorize_control_request(shared, connection)?;
                    continue;
                }
            }
        } else {
            match reader
                .read::<ControlMessage>()
                .await
                .map_err(|error| WorkerError::Protocol(error.to_string()))?
            {
                Some(ControlMessage::Request(request)) => Some(request),
                Some(_) => {
                    return Err(WorkerError::Protocol(
                        "controller sent a non-request message".to_owned(),
                    ));
                }
                None => None,
            }
        };
        let Some(request) = request else {
            return Ok(());
        };
        authorize_control_request(shared, connection)?;
        let release = matches!(request.kind, RequestKind::ReleaseController { .. });
        let response = handle_request(shared, connection, request).await;
        writer
            .write(&ControlMessage::Response(response))
            .await
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        if release {
            return Ok(());
        }
    }
}

async fn handle_request(
    shared: &Arc<Shared>,
    connection: &mut Connection,
    request: ControlRequest,
) -> ControlResponse {
    let request_id = request.request_id;
    let result = match request.kind {
        RequestKind::Negotiate {
            daemon_instance_id,
            minimum_version,
            maximum_version,
        } => {
            negotiate_request(
                shared,
                connection,
                daemon_instance_id,
                minimum_version,
                maximum_version,
            )
            .await
        }
        RequestKind::AcquireController {
            daemon_instance_id,
            challenge,
            requested_capabilities,
        } => acquire_request(
            shared,
            connection,
            &daemon_instance_id,
            &challenge,
            &requested_capabilities,
        ),
        RequestKind::Inspect { lease_id } => inspect_request(shared, connection, &lease_id).await,
        RequestKind::Initialize {
            lease_id,
            initialize,
        } => initialize_request(shared, connection, &lease_id, *initialize).await,
        RequestKind::OpenDataStream {
            scope,
            stream_id,
            mode,
            after_offset,
            attach,
        } => {
            open_data_request(
                shared,
                connection,
                &scope,
                stream_id,
                mode,
                after_offset,
                attach,
            )
            .await
        }
        RequestKind::TerminalSnapshot { scope } => {
            terminal_snapshot_request(shared, connection, &scope).await
        }
        RequestKind::ReadOutput {
            scope,
            stream_id,
            after_offset,
            max_bytes,
            wait_ms,
        } => {
            read_output_request(
                shared,
                connection,
                &scope,
                stream_id,
                after_offset,
                max_bytes,
                wait_ms,
            )
            .await
        }
        RequestKind::WritePlan { scope, plan } => {
            write_request(shared, connection, &scope, plan).await
        }
        RequestKind::Resize { scope, resize } => {
            resize_request(shared, connection, &scope, resize).await
        }
        RequestKind::Stop { scope, stop } => {
            stop_request(shared, connection, &scope, stop.transaction_id).await
        }
        RequestKind::AcknowledgeTerminal { scope } => {
            acknowledge_request(shared, connection, &scope).await
        }
        RequestKind::ReleaseController { lease_id } => {
            release_request(shared, connection, &lease_id)
        }
    };

    let mut response = ControlResponse {
        request_id,
        kind: result.unwrap_or_else(|error| ResponseKind::Error { error }),
    };
    if matches!(response.kind, ResponseKind::TerminalSnapshot { .. }) {
        if let Err(error) = validate_terminal_snapshot_response(&response, &shared.config) {
            response.kind = ResponseKind::Error { error };
        }
    }
    response
}

async fn negotiate_request(
    shared: &Shared,
    connection: &mut Connection,
    daemon_id: DaemonId,
    minimum: Version,
    maximum: Version,
) -> Result<ResponseKind, ControlError> {
    let remote = protocol::VersionRange::new(minimum, maximum)
        .map_err(|error| control_error(ControlCode::InvalidRequest, error, false))?;
    let selected = protocol::negotiate(SUPPORTED_RANGE, remote)
        .map_err(|error| control_error(ControlCode::WorkerProtocolIncompatible, error, false))?;
    let challenge_value = random_value("challenge")
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let challenge = LeaseChallenge::new(challenge_value)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    connection.daemon_id = Some(daemon_id);
    connection.selected_version = Some(selected);
    connection.challenge = Some(challenge.clone());
    let state = shared.state.lock().await;
    Ok(ResponseKind::Negotiated {
        selected_version: selected,
        supported_range: SUPPORTED_RANGE,
        session_id: shared.session_id.clone(),
        worker_id: shared.worker_id.clone(),
        worker_instance_id: state.worker_instance_id.clone(),
        worker_process: shared.worker_process,
        phase: state.phase,
        capabilities: capabilities(selected),
        challenge,
    })
}

fn acquire_request(
    shared: &Shared,
    connection: &mut Connection,
    daemon_id: &DaemonId,
    challenge: &LeaseChallenge,
    requested: &[Capability],
) -> Result<ResponseKind, ControlError> {
    if connection.daemon_id.as_ref() != Some(daemon_id)
        || connection.challenge.as_ref() != Some(challenge)
        || connection.selected_version.is_none()
    {
        return Err(control_error_message(
            ControlCode::IdentityMismatch,
            "negotiation identity or challenge does not match",
            false,
        ));
    }
    // `authorize_control_request` re-read the kernel for this request already,
    // so the identity it attested is the one the lease is minted for.
    let peer = connection.peer.identity();
    let lease_value = random_value("lease")
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let lease_id = LeaseId::new(lease_value)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let owner = LeaseOwner {
        daemon_id: daemon_id.to_string(),
        peer_pid: peer.pid,
        peer_start_identity: peer.start_identity.to_string(),
    };
    shared
        .lease
        .acquire(owner.clone(), lease_id.to_string())
        .map_err(lease_control_error)?;
    let selected = connection.selected_version.ok_or_else(identity_mismatch)?;
    let granted_capabilities = capabilities(selected)
        .into_iter()
        .filter(|capability| requested.contains(capability))
        .collect::<Vec<_>>();
    connection.owner = Some(owner);
    connection.lease_id = Some(lease_id.clone());
    connection.capabilities.clone_from(&granted_capabilities);
    Ok(ResponseKind::ControllerAcquired {
        lease_id,
        capabilities: granted_capabilities,
    })
}

async fn inspect_request(
    shared: &Shared,
    connection: &Connection,
    lease_id: &LeaseId,
) -> Result<ResponseKind, ControlError> {
    validate_lease(shared, connection, lease_id)?;
    let mut state = shared.state.lock().await;
    clear_exited_owner(shared, &mut state).await?;
    Ok(ResponseKind::Inspected {
        snapshot: Box::new(inspect_snapshot(shared, &state).await?),
    })
}

/// Clears the background snapshot once its owner process is provably gone or
/// replaced.
///
/// A killed agent sends no release, and a release can fail to reach the worker,
/// so the owner's exit is checked here, from the inspect the daemon polls. The
/// owner counts as running only when `is_running` confirms its pid still names
/// a live process with the stored start identity. That rejects a zombie, which
/// has exited but is not reaped (the worker is not a subreaper, so a grandchild
/// can stay one), and a pid reused by another process. A missing process is not
/// running. A read error, or a start identity that cannot be parsed, keeps the
/// snapshot, because the owner may still be alive.
///
/// The exit does not raise the sequence mark. A late claim from the dead owner
/// is already refused by `hook_claim_valid`, which requires the reporting
/// process to be alive with its start identity. Raising the mark would also
/// refuse a new owner's Stop whose hook read its clock just before this
/// inspect. The clear is persisted before the inspect answers.
async fn clear_exited_owner(shared: &Shared, state: &mut State) -> Result<(), ControlError> {
    let Some(owner) = state.journal.background_owner.clone() else {
        return Ok(());
    };
    let Ok(start) = owner.start_identity.parse::<u64>() else {
        return Ok(());
    };
    let Ok(running) = HostInspector::new().is_running(OsProcessIdentity {
        pid: owner.pid,
        start_identity: StartIdentity::new(start),
    }) else {
        return Ok(());
    };
    if running {
        return Ok(());
    }
    let mut journal = state.journal.clone();
    if !background::clear(&mut journal) {
        return Ok(());
    }
    journal.updated_at = timestamp();
    persist(shared.journal.clone(), journal.clone())
        .await
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    state.journal = journal;
    log_background_cleared(shared, "owner_exited");
    Ok(())
}

/// Resolves the hook schema `initialize` names against the compiled registry.
fn resolve_hook_schema(
    initialize: &Initialize,
) -> Result<Option<&'static HookSchema>, ControlError> {
    initialize
        .hook_schema
        .as_deref()
        .map(|id| {
            protocol::hook_schema(id).ok_or_else(|| {
                control_error_message(
                    ControlCode::InvalidRequest,
                    "initialize names a hook schema this worker does not provide",
                    false,
                )
            })
        })
        .transpose()
}

async fn initialize_request(
    shared: &Arc<Shared>,
    connection: &Connection,
    lease_id: &LeaseId,
    initialize: Initialize,
) -> Result<ResponseKind, ControlError> {
    validate_lease(shared, connection, lease_id)?;
    if initialize.session_id != shared.session_id
        || initialize.expected_worker_id != shared.worker_id
    {
        return Err(identity_mismatch());
    }
    let selected = connection.selected_version.ok_or_else(identity_mismatch)?;
    initialize
        .check_version(selected)
        .map_err(|error| control_error(ControlCode::InvalidRequest, error, false))?;
    let hook_schema = resolve_hook_schema(&initialize)?;

    {
        let state = shared.state.lock().await;
        if let (Some(transaction), Some(worker_instance_id), Some(pty)) = (
            state.initialize_transaction.as_ref(),
            state.worker_instance_id.as_ref(),
            state.pty.as_ref(),
        ) {
            if transaction == &initialize.transaction_id {
                return Ok(ResponseKind::Initialized {
                    worker_instance_id: worker_instance_id.clone(),
                    child_process: wire_identity(pty.identity())?,
                });
            }
            return Err(control_error_message(
                ControlCode::InvalidState,
                "worker was already initialized by another transaction",
                false,
            ));
        }
        if state.phase != WireRuntimePhase::Uninitialized {
            return Err(invalid_state("worker is not uninitialized"));
        }
    }

    let runtime_value = random_value("runtime")
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let worker_instance_id = WorkerInstanceId::new(runtime_value)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    {
        let mut state = shared.state.lock().await;
        state.phase = WireRuntimePhase::Starting;
        state.initialize_transaction = Some(initialize.transaction_id.clone());
        state.worker_instance_id = Some(worker_instance_id.clone());
        state.journal.phase = JournalPhase::Starting;
        state.journal.worker_instance_id = Some(worker_instance_id.to_string());
        state.journal.updated_at = timestamp();
        persist_control(shared.journal.clone(), state.journal.clone()).await?;
    };
    #[cfg(test)]
    pause_after_starting(shared).await;

    freeze_launch_rule(shared, hook_schema, &initialize)?;
    let command = command_from_initialize(shared, &initialize, &worker_instance_id);
    let history_bytes = usize::try_from(initialize.limits.output_history_bytes())
        .map_err(|error| control_error(ControlCode::InvalidRequest, error, false))?;
    let subscriber_bytes = usize::try_from(initialize.limits.subscriber_queue_bytes())
        .map_err(|error| control_error(ControlCode::InvalidRequest, error, false))?;
    let dedup_entries = usize::try_from(initialize.limits.write_dedup_entries())
        .map_err(|error| control_error(ControlCode::InvalidRequest, error, false))?;
    let stop_grace = Duration::from_millis(initialize.stop_policy.grace_ms());
    let retention = Duration::from_millis(initialize.limits.terminal_retention_ms());
    let effective_config = WorkerConfig {
        history_bytes,
        subscriber_bytes,
        input_dedup_entries: dedup_entries,
        input_stall: initialize
            .input_policy
            .map(|policy| Duration::from_millis(policy.stall_ms())),
        stop_grace,
        terminal_retention: retention,
        ..shared.config.clone()
    };
    effective_config
        .validate()
        .map_err(|error| control_error(ControlCode::InvalidRequest, error, false))?;
    let (pty, child_process) = spawn_running(
        shared,
        command,
        &effective_config,
        initialize.launch.agent_base,
        hook_schema,
        initialize.public_protocol_version,
        &initialize.dimensions,
    )
    .await?;
    shared.initialized_tx.send_replace(true);
    shared.emit(EventKind::RuntimeStarted {
        worker_instance_id: worker_instance_id.clone(),
        child_process,
    });
    spawn_runtime_monitors(Arc::clone(shared), pty, worker_instance_id.clone());
    Ok(ResponseKind::Initialized {
        worker_instance_id,
        child_process,
    })
}

/// Freezes the session's launch-process rule before its runtime is spawned.
///
/// # Errors
///
/// Returns a runtime fault when a rule was already frozen, which the
/// `Uninitialized` phase check rules out.
fn freeze_launch_rule(
    shared: &Shared,
    hook_schema: Option<&'static HookSchema>,
    initialize: &Initialize,
) -> Result<(), ControlError> {
    shared
        .launch_rule
        .set(launch_rule(shared, hook_schema, initialize))
        .map_err(|_rejected| {
            control_error_message(
                ControlCode::RuntimeFault,
                "worker launch rule was already frozen",
                false,
            )
        })
}

/// Spawns the runtime's child and commits `Running` in one state-lock hold.
///
/// The child can report its identity as soon as it runs, and a hook claim is
/// admitted only in `Running`. Holding the lock across the spawn makes every
/// claim wait for the commit instead of racing it and being refused.
async fn spawn_running(
    shared: &Shared,
    command: Command,
    config: &WorkerConfig,
    agent_base: String,
    hook_schema: Option<&'static HookSchema>,
    public_protocol_version: u32,
    dimensions: &Dimensions,
) -> Result<(PtyOwner, WireProcessIdentity), ControlError> {
    let (pty, child_process, committed) = {
        let mut state = shared.state.lock().await;
        let pty = spawn_pty(command, config)?;
        let child_process = wire_identity(pty.identity())?;
        state.phase = WireRuntimePhase::Running;
        state.stop_grace = config.stop_grace;
        state.terminal_retention = config.terminal_retention;
        state.launch_agent_base = Some(agent_base);
        state.hook_schema = hook_schema;
        state.public_protocol_version = Some(public_protocol_version);
        state.journal.hook_schema = hook_schema.map(|schema| schema.id.to_owned());
        state.journal.phase = JournalPhase::Live;
        state.journal.child = Some(journal_identity(pty.identity()));
        state.journal.pty_created_at = Some(timestamp());
        state.journal.cols = Some(dimensions.columns());
        state.journal.rows = Some(dimensions.rows());
        state.journal.updated_at = timestamp();
        state.pty = Some(pty.clone());
        let committed = persist(shared.journal.clone(), state.journal.clone()).await;
        (pty, child_process, committed)
    };
    if let Err(error) = committed {
        let _ = pty.stop("journal-failure", config.stop_grace).await;
        return Err(control_error(ControlCode::RuntimeFault, error, false));
    }
    Ok((pty, child_process))
}

/// Waits at the installed [`StartingGate`], if any, with the state lock free.
#[cfg(test)]
async fn pause_after_starting(shared: &Shared) {
    let gate = shared
        .starting_gate
        .lock()
        .expect("starting gate lock")
        .take();
    if let Some(gate) = gate {
        let _ = gate.reached.send(());
        let _ = gate.proceed.await;
    }
}

fn spawn_pty(command: Command, config: &WorkerConfig) -> Result<PtyOwner, ControlError> {
    PtyOwner::spawn(
        command,
        config.history_bytes,
        config.subscriber_bytes,
        config.input_dedup_entries,
        config.input_stall,
    )
    .map_err(|error| control_error(ControlCode::RuntimeFault, error, false))
}

async fn open_data_request(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
    stream_id: StreamId,
    mode: StreamMode,
    after_offset: Option<u64>,
    attach: Option<AttachStart>,
) -> Result<ResponseKind, ControlError> {
    validate_scope(shared, connection, scope).await?;
    validate_data_start(connection, mode, after_offset, attach.as_ref())?;
    let token_value = random_value("data")
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let token = DataToken::new(token_value)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let now = shared.now_ms();
    let ttl = u64::try_from(shared.config.data_token_ttl.as_millis()).unwrap_or(u64::MAX);
    let expires_at_ms = now.saturating_add(ttl);
    let (lease_owner, lease_epoch, version) = data_grant_authority(shared, connection)?;
    let grant = DataGrant {
        lease_owner,
        lease_id: scope.lease_id.clone(),
        lease_epoch,
        version,
        expires_at_ms,
        worker_instance_id: scope.worker_instance_id.clone(),
        stream_id: stream_id.clone(),
        mode,
        after_offset,
        attach,
        observation: None,
    };
    let mut tokens = lock(&shared.tokens);
    tokens
        .insert(token.clone(), grant, now)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    Ok(ResponseKind::DataStreamOpened {
        token,
        expires_at_ms,
    })
}

fn data_grant_authority(
    shared: &Shared,
    connection: &Connection,
) -> Result<(LeaseOwner, u64, Version), ControlError> {
    let lease_owner = connection.owner.clone().ok_or_else(identity_mismatch)?;
    let lease_id = connection.lease_id.as_ref().ok_or_else(identity_mismatch)?;
    shared
        .lease
        .validate(&lease_owner, lease_id.as_str())
        .map_err(lease_control_error)?;
    let lease_epoch = *shared.lease_epoch_tx.borrow();
    let version = connection.selected_version.ok_or_else(identity_mismatch)?;
    Ok((lease_owner, lease_epoch, version))
}

fn observation_capability(connection: &Connection) -> Result<(), ControlError> {
    if connection
        .capabilities
        .contains(&Capability::ControlPlaneObservation)
    {
        Ok(())
    } else {
        Err(control_error_message(
            ControlCode::WorkerFeatureUnavailable,
            "control-plane observation was not negotiated",
            false,
        ))
    }
}

async fn terminal_snapshot_request(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
) -> Result<ResponseKind, ControlError> {
    observation_capability(connection)?;
    let pty = scoped_pty(shared, connection, scope).await?;
    let snapshot = wire_terminal(&pty.output().terminal_snapshot())
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    validate_terminal_snapshot_dimensions(&snapshot, &shared.config)?;
    Ok(ResponseKind::TerminalSnapshot {
        worker_instance_id: scope.worker_instance_id.clone(),
        snapshot: Box::new(snapshot),
    })
}

fn validate_terminal_snapshot_dimensions(
    snapshot: &WireTerminalSnapshot,
    config: &WorkerConfig,
) -> Result<(), ControlError> {
    if snapshot.dimensions.rows() > config.max_snapshot_rows
        || snapshot.dimensions.columns() > config.max_snapshot_columns
    {
        return Err(control_error_message(
            ControlCode::ObservationLimitExceeded,
            "terminal snapshot dimensions exceed the worker limit",
            false,
        ));
    }
    Ok(())
}

fn validate_terminal_snapshot_response(
    response: &ControlResponse,
    config: &WorkerConfig,
) -> Result<(), ControlError> {
    let serialized = serde_json::to_vec(response)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    if serialized.len() > config.max_snapshot_bytes {
        return Err(control_error_message(
            ControlCode::ObservationLimitExceeded,
            "serialized terminal snapshot response exceeds the worker limit",
            false,
        ));
    }
    Ok(())
}

async fn read_output_request(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
    stream_id: StreamId,
    after_offset: Option<u64>,
    max_bytes: u32,
    wait_ms: u64,
) -> Result<ResponseKind, ControlError> {
    observation_capability(connection)?;
    validate_scope(shared, connection, scope).await?;
    let (max_bytes, wait) = validate_observation_request(max_bytes, wait_ms, &shared.config)?;
    let token_value = random_value("observation")
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let token = DataToken::new(token_value)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let now = shared.now_ms();
    let ttl = u64::try_from(shared.config.data_token_ttl.as_millis()).unwrap_or(u64::MAX);
    let expires_at_ms = now.saturating_add(ttl);
    let (lease_owner, lease_epoch, version) = data_grant_authority(shared, connection)?;
    let grant = DataGrant {
        lease_owner,
        lease_id: scope.lease_id.clone(),
        lease_epoch,
        version,
        expires_at_ms,
        worker_instance_id: scope.worker_instance_id.clone(),
        stream_id,
        mode: StreamMode::Observation,
        after_offset,
        attach: None,
        observation: Some(ObservationGrant { max_bytes, wait }),
    };
    let mut tokens = lock(&shared.tokens);
    tokens
        .insert(token.clone(), grant, now)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    Ok(ResponseKind::OutputReadOpened {
        token,
        expires_at_ms,
    })
}

fn validate_observation_request(
    max_bytes: u32,
    wait_ms: u64,
    config: &WorkerConfig,
) -> Result<(usize, Duration), ControlError> {
    let max_bytes = usize::try_from(max_bytes)
        .map_err(|error| control_error(ControlCode::InvalidRequest, error, false))?;
    if max_bytes == 0 || max_bytes > config.history_bytes {
        return Err(control_error_message(
            ControlCode::ObservationLimitExceeded,
            "output observation byte limit is invalid",
            false,
        ));
    }
    let wait = Duration::from_millis(wait_ms);
    if wait > config.max_observation_wait {
        return Err(control_error_message(
            ControlCode::ObservationLimitExceeded,
            "output observation wait exceeds the worker limit",
            false,
        ));
    }
    Ok((max_bytes, wait))
}

fn validate_data_start(
    connection: &Connection,
    mode: StreamMode,
    after_offset: Option<u64>,
    attach: Option<&AttachStart>,
) -> Result<(), ControlError> {
    let selected = connection.selected_version.ok_or_else(identity_mismatch)?;
    match (mode, attach) {
        (StreamMode::Attach, Some(_))
            if after_offset.is_none()
                && selected >= ATTACH_SNAPSHOT_VERSION
                && connection
                    .capabilities
                    .contains(&Capability::AttachSnapshot) =>
        {
            Ok(())
        }
        (StreamMode::Attach, None) if selected < ATTACH_SNAPSHOT_VERSION => Ok(()),
        (StreamMode::Detector, None) => Ok(()),
        (StreamMode::Observation, _) => Err(control_error_message(
            ControlCode::InvalidRequest,
            "observation streams require a read-output grant",
            false,
        )),
        (StreamMode::Attach, Some(_)) if after_offset.is_some() => Err(control_error_message(
            ControlCode::InvalidRequest,
            "snapshot attachment cannot request an output replay offset",
            false,
        )),
        (StreamMode::Attach, _) => Err(control_error_message(
            ControlCode::InvalidRequest,
            "atomic attach snapshot was not negotiated",
            false,
        )),
        (StreamMode::Detector, Some(_)) => Err(control_error_message(
            ControlCode::InvalidRequest,
            "detector streams cannot request an attach snapshot",
            false,
        )),
    }
}

async fn write_request(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
    plan: protocol::InputPlan,
) -> Result<ResponseKind, ControlError> {
    let pty = scoped_pty(shared, connection, scope).await?;
    let version = connection.selected_version.ok_or_else(identity_mismatch)?;
    let byte_len = u64::try_from(plan.byte_len()).unwrap_or(u64::MAX);
    let (write_id, fragments) = plan.into_parts();
    let producer_id = connection
        .owner
        .as_ref()
        .ok_or_else(identity_mismatch)?
        .daemon_id
        .clone();
    let lease_id = connection.lease_id.as_ref().ok_or_else(identity_mismatch)?;
    let local = InputPlan {
        write_id: write_id.to_string(),
        fragments: fragments
            .into_iter()
            .map(|fragment| InputFragment {
                bytes: fragment.bytes.into_inner(),
                delay_after: Duration::from_millis(fragment.delay_after_ms),
            })
            .collect(),
    };
    match control_input_id(&write_id, lease_id.as_str())? {
        ControlInputId::Namespaced { sequence } => {
            pty.input()
                .execute_control(lease_id.as_str(), sequence, local)
                .await
        }
        ControlInputId::Legacy => pty.input().execute_legacy(&producer_id, local).await,
    }
    .map_err(|error| input_control_error(error, version))?;
    Ok(ResponseKind::WriteCompleted {
        acknowledgement: WriteAck {
            write_id,
            bytes_written: byte_len,
        },
    })
}

async fn resize_request(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
    resize: protocol::ResizeRequest,
) -> Result<ResponseKind, ControlError> {
    let pty = scoped_pty(shared, connection, scope).await?;
    let _ = pty
        .resize(
            resize.source_id.as_str(),
            resize.sequence,
            resize.dimensions.columns(),
            resize.dimensions.rows(),
        )
        .await
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let (cols, rows) = pty.dimensions().await;
    let dimensions = Dimensions::new(cols, rows)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, false))?;
    {
        let mut state = shared.state.lock().await;
        state.journal.cols = Some(cols);
        state.journal.rows = Some(rows);
    };
    Ok(ResponseKind::Resized {
        sequence: resize.sequence,
        dimensions,
    })
}

async fn stop_request(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
    transaction_id: TransactionId,
) -> Result<ResponseKind, ControlError> {
    validate_scope(shared, connection, scope).await?;
    let preparation = {
        let mut state = shared.state.lock().await;
        prepare_stop(&mut state)?
    };
    let (pty, grace) = match preparation {
        StopPreparation::AlreadyCommitted(exit) => {
            return Ok(ResponseKind::Stopped { exit: Some(exit) });
        }
        StopPreparation::Begin { pty, grace } => (pty, grace),
    };
    pty.stop(transaction_id.as_str(), grace)
        .await
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let committed = tokio::time::timeout(
        shared.config.terminal_commit_wait_timeout,
        wait_for_terminal_commit(shared),
    )
    .await
    .map_err(|_elapsed| {
        control_error_message(
            ControlCode::RuntimeFault,
            "terminal journal commit is still pending; retry stop",
            true,
        )
    })??;
    Ok(ResponseKind::Stopped {
        exit: Some(committed),
    })
}

enum StopPreparation {
    AlreadyCommitted(ExitStatus),
    Begin { pty: PtyOwner, grace: Duration },
}

fn prepare_stop(state: &mut State) -> Result<StopPreparation, ControlError> {
    if state.phase == WireRuntimePhase::Exited {
        let exit = state.exit.clone().ok_or_else(|| {
            control_error_message(
                ControlCode::RuntimeFault,
                "terminal journal commit has no exit status",
                false,
            )
        })?;
        return Ok(StopPreparation::AlreadyCommitted(exit));
    }
    if state.phase == WireRuntimePhase::Faulted {
        return Err(control_error_message(
            ControlCode::RuntimeFault,
            "runtime faulted before its terminal journal commit",
            false,
        ));
    }
    let pty = state
        .pty
        .clone()
        .ok_or_else(|| invalid_state("runtime has no PTY"))?;
    state.stop_requested = true;
    state.phase = WireRuntimePhase::Stopping;
    Ok(StopPreparation::Begin {
        pty,
        grace: state.stop_grace,
    })
}

async fn wait_for_terminal_commit(shared: &Shared) -> Result<ExitStatus, ControlError> {
    loop {
        let notified = shared.state_changed.notified();
        let state = shared.state.lock().await;
        match state.phase {
            WireRuntimePhase::Exited => {
                return state.exit.clone().ok_or_else(|| {
                    control_error_message(
                        ControlCode::RuntimeFault,
                        "terminal journal commit has no exit status",
                        false,
                    )
                });
            }
            WireRuntimePhase::Faulted => {
                return Err(control_error_message(
                    ControlCode::RuntimeFault,
                    "runtime faulted before its terminal journal commit",
                    false,
                ));
            }
            _ => {}
        }
        drop(state);
        notified.await;
    }
}

async fn acknowledge_request(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
) -> Result<ResponseKind, ControlError> {
    validate_scope(shared, connection, scope).await?;
    {
        let mut state = shared.state.lock().await;
        if state.phase != WireRuntimePhase::Exited {
            return Err(invalid_state("runtime is not terminal"));
        }
        state.journal.terminal_acknowledged = true;
        state.journal.updated_at = timestamp();
        persist_control(shared.journal.clone(), state.journal.clone()).await?;
    };
    shared.shutdown.cancel();
    Ok(ResponseKind::TerminalAcknowledged)
}

fn release_request(
    shared: &Shared,
    connection: &mut Connection,
    lease_id: &LeaseId,
) -> Result<ResponseKind, ControlError> {
    validate_lease(shared, connection, lease_id)?;
    let owner = connection.owner.as_ref().ok_or_else(identity_mismatch)?;
    shared
        .lease
        .release(owner, lease_id.as_str())
        .map_err(lease_control_error)?;
    shared.bump_lease_epoch();
    connection.lease_id = None;
    Ok(ResponseKind::ControllerReleased)
}

/// The launch process's latest native reference as the snapshot reports it.
fn reported_native_reference(
    state: &State,
) -> Result<Option<ReportedNativeReference>, ControlError> {
    state
        .journal
        .native_reference_claim
        .as_ref()
        .map(|claim| {
            Ok(ReportedNativeReference {
                provider: claim.provider.clone(),
                process: wire_child_identity(&claim.process)?,
                sequence: claim.sequence,
                reference_kind: claim.reference_kind.clone(),
                native_reference: claim.native_reference.clone(),
            })
        })
        .transpose()
}

async fn inspect_snapshot(shared: &Shared, state: &State) -> Result<InspectSnapshot, ControlError> {
    let (child_process, dimensions, history_start_offset, next_offset) =
        if let Some(pty) = state.pty.as_ref() {
            let (cols, rows) = pty.dimensions().await;
            (
                Some(wire_identity(pty.identity())?),
                Some(
                    Dimensions::new(cols, rows)
                        .map_err(|error| control_error(ControlCode::RuntimeFault, error, false))?,
                ),
                pty.output().history_start_offset(),
                pty.output().next_offset(),
            )
        } else {
            (None, None, 0, state.journal.next_output_offset)
        };
    Ok(InspectSnapshot {
        session_id: shared.session_id.clone(),
        worker_id: shared.worker_id.clone(),
        worker_instance_id: state.worker_instance_id.clone(),
        phase: state.phase,
        worker_process: shared.worker_process,
        child_process,
        dimensions,
        history_start_offset,
        next_offset,
        exit: state.exit.clone(),
        launch_identity: state
            .journal
            .launch_identity
            .as_ref()
            .map(|identity| {
                Ok(ReportedLaunchIdentity {
                    provider: identity.provider.clone(),
                    process: wire_child_identity(&identity.process)?,
                    reference_kind: identity.reference_kind.clone(),
                    native_reference: identity.native_reference.clone(),
                })
            })
            .transpose()?,
        active_identity: state
            .journal
            .active_identity
            .as_ref()
            .filter(|identity| valid_identity_expiry(&identity.expires_at))
            .map(|identity| {
                Ok(ActiveIdentityClaim {
                    provider: identity.provider.clone(),
                    process: wire_child_identity(&identity.process)?,
                    sequence: identity.sequence,
                    expires_at: identity.expires_at.clone(),
                    reference_kind: identity.reference_kind.clone(),
                    native_reference: identity.native_reference.clone(),
                })
            })
            .transpose()?,
        native_reference: reported_native_reference(state)?,
        active_identity_release: state
            .journal
            .active_identity_release
            .as_ref()
            .map(|release| {
                Ok(ReleasedIdentityClaim {
                    provider: release.provider.clone(),
                    process: wire_child_identity(&release.process)?,
                    sequence: release.sequence,
                })
            })
            .transpose()?,
        subagents: state
            .journal
            .subagents
            .iter()
            .map(|subagent| SubagentSnapshot {
                id: subagent.id.clone(),
                parent_id: subagent.parent_id.clone(),
                provider: subagent.provider.clone(),
                agent_type: subagent.agent_type.clone(),
                phase: wire_subagent_phase(subagent.phase),
                revision: subagent.revision,
                started_at_ms: subagent.started_at_ms,
                updated_at_ms: subagent.updated_at_ms,
                finished_at_ms: subagent.finished_at_ms,
            })
            .collect(),
        hook_schema: state.journal.hook_schema.clone(),
        background: state.journal.background.clone(),
    })
}

fn spawn_runtime_monitors(
    shared: Arc<Shared>,
    pty: PtyOwner,
    worker_instance_id: WorkerInstanceId,
) {
    let (output_eof_tx, output_eof_rx) = oneshot::channel();
    let output_shared = Arc::clone(&shared);
    let output_pty = pty.clone();
    let output_runtime = worker_instance_id.clone();
    tokio::spawn(async move {
        let result = monitor_runtime_output(&output_shared, &output_pty, &output_runtime).await;
        let _ = output_eof_tx.send(result);
    });

    tokio::spawn(coordinate_runtime_completion(
        shared,
        pty,
        worker_instance_id,
        output_eof_rx,
    ));
}

async fn monitor_runtime_output(
    shared: &Shared,
    pty: &PtyOwner,
    worker_instance_id: &WorkerInstanceId,
) -> Result<OutputCompletion, WorkerError> {
    let mut subscriber = pty
        .subscribe_output(Some(0))
        .map_err(|error| WorkerError::Pty(PtyError::Output(error)))?;
    loop {
        match subscriber.recv().await {
            Some(OutputEvent::Replay(_) | OutputEvent::TerminalSnapshot(_)) => {}
            Some(OutputEvent::Output(chunk)) => {
                shared.emit(EventKind::OutputAdvanced {
                    worker_instance_id: worker_instance_id.clone(),
                    next_offset: chunk
                        .offset
                        .saturating_add(u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX)),
                });
            }
            Some(OutputEvent::Gap { watermark, .. }) => {
                shared.emit(EventKind::TerminalChanged {
                    worker_instance_id: worker_instance_id.clone(),
                    watermark,
                });
            }
            Some(OutputEvent::Exit { next_offset }) => {
                let completion = pty.output_completion().ok_or_else(|| {
                    WorkerError::Protocol(
                        "PTY output closed without a completion record".to_owned(),
                    )
                })?;
                if completion.next_offset() != next_offset {
                    return Err(WorkerError::Protocol(
                        "PTY output completion offset changed after closure".to_owned(),
                    ));
                }
                return Ok(completion);
            }
            None => {
                return Err(WorkerError::Protocol(
                    "PTY output monitor closed before EOF".to_owned(),
                ));
            }
        }
    }
}

async fn coordinate_runtime_completion(
    shared: Arc<Shared>,
    pty: PtyOwner,
    worker_instance_id: WorkerInstanceId,
    mut output_eof_rx: oneshot::Receiver<Result<OutputCompletion, WorkerError>>,
) {
    let exit = match wait_runtime_exit(&pty).await {
        Ok(exit) => exit,
        Err(error) => {
            let _ = pty
                .stop("root-observation-failure", shared.config.stop_grace)
                .await;
            mark_runtime_faulted(&shared, worker_instance_id, WorkerError::Pty(error)).await;
            shared.shutdown.cancel();
            return;
        }
    };
    let (status, stopped_by_user) = {
        let state = shared.state.lock().await;
        (
            exit_status(&exit, state.stop_requested),
            state.stop_requested,
        )
    };
    let exited_at = timestamp();
    let completion = match wait_for_output_completion(&shared, &pty, &mut output_eof_rx).await {
        Ok(completion) => completion,
        Err(error) => {
            mark_runtime_faulted(&shared, worker_instance_id, error).await;
            shared.shutdown.cancel();
            return;
        }
    };
    let (next_output_offset, output_forced_closed) = match completion {
        OutputCompletion::Eof { next_offset } => (next_offset, false),
        OutputCompletion::ForcedClosed { next_offset } => (next_offset, true),
    };
    if output_forced_closed {
        event!(
            name: "worker.pty.output.forced_close",
            Level::WARN,
            process.pid = pty.identity().pid,
            "PTY output was force-closed; the runtime's processes are swept before it ends",
        );
    }
    // The stop that sealed a forced close already reaped the root, so finishing
    // it reports the forced close again; that is not a fault. The root exit is
    // known here, so the runtime ends with it.
    match pty.finish_natural().await {
        Ok(_) => {}
        Err(PtyError::OutputForcedClosed) if output_forced_closed => {}
        Err(error) => {
            mark_runtime_faulted(&shared, worker_instance_id, WorkerError::Pty(error)).await;
            shared.shutdown.cancel();
            return;
        }
    }
    // A forced close means a process outside the root's group still holds the
    // PTY slave and was never signalled. The runtime only ends once every
    // process carrying its marker is proven gone; otherwise it is faulted and
    // the daemon's lost-runtime handling takes over.
    if output_forced_closed {
        if let Err(error) = confirm_forced_close_cleanup(&shared, &worker_instance_id).await {
            mark_runtime_faulted_at_offset(
                &shared,
                worker_instance_id,
                error,
                Some(next_output_offset),
            )
            .await;
            shared.shutdown.cancel();
            return;
        }
    }
    let status = ExitStatus {
        output_forced_closed,
        ..status
    };
    let Some(retention) = retry_runtime_completion(
        &shared,
        &exit,
        &status,
        stopped_by_user,
        &exited_at,
        next_output_offset,
    )
    .await
    else {
        return;
    };
    shared.emit(EventKind::ChildExited {
        worker_instance_id,
        exit: status,
    });
    tokio::select! {
        () = tokio::time::sleep(retention) => shared.shutdown.cancel(),
        () = shared.shutdown.cancelled() => {}
    }
}

/// Confirms that no process of this worker's force-closed runtime is left.
///
/// Builds the sweep scope from the worker's own identity and its journaled
/// lineage and coalition proofs, and bounds the sweep by the session's stop
/// grace; see [`confirm_runtime_cleanup`].
///
/// # Errors
///
/// Returns [`WorkerError::RuntimeCleanup`] when the cleanup stays unconfirmed.
async fn confirm_forced_close_cleanup(
    shared: &Shared,
    worker_instance_id: &WorkerInstanceId,
) -> Result<(), WorkerError> {
    let (grace, worker_spawn, worker_coalitions) = {
        let state = shared.state.lock().await;
        (
            state.stop_grace,
            state.journal.worker_spawn_id.clone(),
            state.journal.worker_coalitions.clone(),
        )
    };
    let scope = CleanupScope {
        session_id: shared.session_id.as_str(),
        worker_instance_id,
        worker_start: StartIdentity::new(shared.worker_process.start_identity),
        // A spawn id or coalition pair that does not parse only withholds its
        // proof, which leaves more processes unconfirmed, never fewer.
        worker_spawn: worker_spawn.and_then(|value| value.parse::<SpawnId>().ok()),
        worker_coalitions: worker_coalitions
            .and_then(|value| value.parse::<ProcessCoalitions>().ok()),
    };
    confirm_runtime_cleanup(&HostInspector::new(), &scope, grace).await
}

/// The runtime a forced-close cleanup sweeps, and the worker that owned it.
#[derive(Debug)]
struct CleanupScope<'a> {
    /// Session whose worker-marked children without an instance marker keep
    /// the cleanup unconfirmed.
    session_id: &'a str,
    /// Runtime whose marked processes are swept.
    worker_instance_id: &'a WorkerInstanceId,
    /// This worker's start identity; an unreadable-marker process that
    /// started before it is foreign to the runtime.
    worker_start: StartIdentity,
    /// This worker's creation number, on a host that reports fork lineage.
    worker_spawn: Option<SpawnId>,
    /// This worker's kernel coalitions, on a host that reports them.
    worker_coalitions: Option<ProcessCoalitions>,
}

/// Sweeps every process marked with the runtime and confirms none is left.
///
/// Runs [`sweep_runtime`] until a pass attributes no process to the runtime,
/// at most [`MAX_RUNTIME_SWEEP_PASSES`] times. `grace` bounds each wait
/// between `SIGTERM` and `SIGKILL` and is capped at [`MAX_SWEEP_GRACE`]. When
/// a pass leaves only processes whose markers cannot be read, the sweep
/// re-reads the process table for up to that same grace (see
/// [`settle_unreadable`]), as the daemon's lost-runtime sweep does, and an
/// unreadable-marker process still present afterwards keeps the cleanup
/// unconfirmed.
///
/// Only processes that carry the runtime marker, or that the fork lineage
/// proves descend from this worker, can be found; on a host that reports
/// coalitions, an unreadable-marker process outside both of this worker's
/// coalitions is foreign. A process that scrubs its
/// environment on a host without lineage is invisible to this sweep, the same
/// limit the daemon's lost-runtime sweep has.
///
/// # Errors
///
/// Returns [`WorkerError::RuntimeCleanup`] when the sweep cannot run, aborts,
/// leaves a signalled process running, skips the worker itself, or still
/// attributes processes to the runtime after the last pass.
async fn confirm_runtime_cleanup(
    inspector: &dyn ProcessInspector,
    scope: &CleanupScope<'_>,
    grace: Duration,
) -> Result<(), WorkerError> {
    let unconfirmed = |detail: String| WorkerError::RuntimeCleanup {
        runtime_id: scope.worker_instance_id.to_string(),
        detail,
    };
    let grace = grace.min(MAX_SWEEP_GRACE);
    let request = SweepRequest::new(
        scope.worker_instance_id.as_str(),
        rustix::process::geteuid().as_raw(),
        grace,
        RUNTIME_SWEEP_POLL_INTERVAL.min(grace),
    )
    .map_err(|error| unconfirmed(error.to_string()))?
    .with_runtime_session(scope.session_id)
    .with_worker_start_identity(Some(scope.worker_start))
    .with_worker_spawn_id(scope.worker_spawn)
    .with_worker_coalitions(scope.worker_coalitions);
    // Passes that signalled or decided; the pauses of an unreadable-marker
    // settle window are not counted.
    let mut passes = 0;
    let mut settle_until: Option<tokio::time::Instant> = None;
    loop {
        let pass = passes + 1;
        let report = sweep_runtime(inspector, &request)
            .await
            .map_err(|error| unconfirmed(format!("sweep pass {pass} aborted: {error}")))?;
        let only_unreadable = leaves_only_unreadable(&report);
        let settling = only_unreadable && settle_unreadable(&mut settle_until, grace).await;
        log_cleanup_pass(scope, pass, settling, &report);
        if settling {
            continue;
        }
        if !report.is_complete() {
            return Err(unconfirmed(format!(
                "{} signalled process(es) were still running after pass {pass}",
                report.unconfirmed.len()
            )));
        }
        if report
            .skipped
            .iter()
            .any(|skipped| skipped.reason == SkipReason::CurrentProcess)
        {
            return Err(unconfirmed(
                "the sweep selected the worker process itself".to_owned(),
            ));
        }
        // An unreadable-marker process that could still belong to the runtime
        // counts as selected, like every other skip.
        let selected = report.terminated.len()
            + report.killed.len()
            + report.unconfirmed.len()
            + report.skipped.len();
        if selected == 0 {
            return Ok(());
        }
        passes += 1;
        if only_unreadable {
            let blockers = cleanup_blockers(&[], &report.skipped);
            log_cleanup_blockers(inspector, scope, &blockers);
            return Err(unconfirmed(format!(
                "processes of the runtime remained after {passes} sweep pass(es): {}",
                describe_blockers(&blockers)
            )));
        }
        // The last signalling pass may have ended the last process; only a
        // look without signals tells whether anything is left.
        if passes >= MAX_RUNTIME_SWEEP_PASSES {
            return verify_runtime_cleanup(inspector, &request, scope, grace, &mut settle_until)
                .await;
        }
    }
}

/// Decides a cleanup after its last signalling pass, sending no signal.
///
/// Surveys the process table: nothing attributed to the runtime and nothing
/// skipped confirms the cleanup. When only unreadable-marker processes remain,
/// the survey repeats while the cleanup's settle window is open (see
/// [`settle_unreadable`]); anything else left keeps the cleanup unconfirmed.
///
/// # Errors
///
/// Returns [`WorkerError::RuntimeCleanup`] when the survey cannot run or a
/// process of the runtime, or one that could belong to it, is left.
async fn verify_runtime_cleanup(
    inspector: &dyn ProcessInspector,
    request: &SweepRequest,
    scope: &CleanupScope<'_>,
    grace: Duration,
    settle_until: &mut Option<tokio::time::Instant>,
) -> Result<(), WorkerError> {
    let unconfirmed = |detail: String| WorkerError::RuntimeCleanup {
        runtime_id: scope.worker_instance_id.to_string(),
        detail,
    };
    loop {
        let survey = survey_runtime(inspector, request)
            .map_err(|error| unconfirmed(format!("cleanup verification aborted: {error}")))?;
        let only_unreadable = survey.attributed.is_empty()
            && !survey.skipped.is_empty()
            && survey
                .skipped
                .iter()
                .all(|skipped| skipped.reason == SkipReason::MarkersUnreadable);
        let settling = only_unreadable && settle_unreadable(settle_until, grace).await;
        event!(
            name: "worker.runtime.sweep.verify",
            Level::INFO,
            worker_instance_id = scope.worker_instance_id.as_str(),
            sweep.attributed = survey.attributed.len(),
            sweep.skipped = survey.skipped.len(),
            sweep.settling = settling,
            "verified the cleanup of a force-closed runtime",
        );
        if survey.is_empty() {
            return Ok(());
        }
        if settling {
            continue;
        }
        let blockers = cleanup_blockers(&survey.attributed, &survey.skipped);
        log_cleanup_blockers(inspector, scope, &blockers);
        return Err(unconfirmed(format!(
            "processes of the runtime remained after the last sweep pass: {}",
            describe_blockers(&blockers)
        )));
    }
}

/// A process that keeps a cleanup open, with why it does.
#[derive(Debug)]
struct Blocker {
    identity: OsProcessIdentity,
    /// The skip reason, or `Attributed` for a process the sweep would signal.
    reason: String,
}

/// Lists the blockers of a deciding pass or survey.
fn cleanup_blockers(attributed: &[OsProcessIdentity], skipped: &[Skipped]) -> Vec<Blocker> {
    attributed
        .iter()
        .map(|identity| Blocker {
            identity: *identity,
            reason: "Attributed".to_owned(),
        })
        .chain(skipped.iter().map(|skipped| Blocker {
            identity: skipped.identity,
            reason: format!("{:?}", skipped.reason),
        }))
        .collect()
}

/// Whether a sweep pass signalled nothing and skipped only processes whose
/// ownership markers could not be read.
fn leaves_only_unreadable(report: &SweepReport) -> bool {
    !report.skipped.is_empty()
        && report.terminated.is_empty()
        && report.killed.is_empty()
        && report.unconfirmed.is_empty()
        && report
            .skipped
            .iter()
            .all(|skipped| skipped.reason == SkipReason::MarkersUnreadable)
}

/// Pauses one poll interval while the settle window of the unreadable-marker
/// processes is open, and reports whether it paused.
///
/// The window opens at the first call and lasts `grace`. Nothing was signalled
/// when only such processes remain, so repeating the sweep at once would read
/// the same table: the process is caught between images, or an orphan about
/// to exit. The caller leaves the cleanup unconfirmed once the window closed.
async fn settle_unreadable(
    settle_until: &mut Option<tokio::time::Instant>,
    grace: Duration,
) -> bool {
    let deadline = *settle_until.get_or_insert_with(|| tokio::time::Instant::now() + grace);
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return false;
    }
    tokio::time::sleep(RUNTIME_SWEEP_POLL_INTERVAL.min(remaining)).await;
    true
}

fn log_cleanup_pass(scope: &CleanupScope<'_>, pass: usize, settling: bool, report: &SweepReport) {
    event!(
        name: "worker.runtime.sweep.pass",
        Level::INFO,
        worker_instance_id = scope.worker_instance_id.as_str(),
        sweep.pass = pass,
        sweep.terminated = report.terminated.len(),
        sweep.killed = report.killed.len(),
        sweep.unconfirmed = report.unconfirmed.len(),
        sweep.skipped = report.skipped.len(),
        sweep.settling = settling,
        "swept the processes of a force-closed runtime",
    );
}

/// Names each process that kept the cleanup of a force-closed runtime open.
///
/// One event per skipped process, at most [`MAX_LOGGED_CLEANUP_BLOCKERS`]: its
/// identity next to this worker's, the skip reason, the command name, whether
/// it still runs, and the outcome of a fresh marker read. That is what tells a
/// process between images, an exited or unreaped one, and a foreign
/// non-dumpable one apart.
fn log_cleanup_blockers(
    inspector: &dyn ProcessInspector,
    scope: &CleanupScope<'_>,
    blockers: &[Blocker],
) {
    for blocker in blockers.iter().take(MAX_LOGGED_CLEANUP_BLOCKERS) {
        let pid = blocker.identity.pid;
        let comm = match inspector.process(pid) {
            Ok(Some(fact)) if fact.start_identity == blocker.identity.start_identity => fact.comm,
            Ok(_) => "exited".to_owned(),
            Err(error) => format!("unreadable: {error}"),
        };
        let markers = match inspector.ownership_markers(pid) {
            Ok(_) => "readable".to_owned(),
            Err(error) => error.to_string(),
        };
        let running = match inspector.is_running(blocker.identity) {
            Ok(true) => "running".to_owned(),
            Ok(false) => "exited".to_owned(),
            Err(error) => format!("unknown: {error}"),
        };
        event!(
            name: "worker.runtime.sweep.blocker",
            Level::WARN,
            worker_instance_id = scope.worker_instance_id.as_str(),
            process.pid = pid,
            process.start_identity = blocker.identity.start_identity.get(),
            process.comm = %comm,
            worker.start_identity = scope.worker_start.get(),
            sweep.skip_reason = %blocker.reason,
            sweep.markers = %markers,
            process.state = %running,
            "a process kept the cleanup of a force-closed runtime unconfirmed",
        );
    }
}

/// Lists the blockers of a cleanup for its error.
///
/// Only numbers and the reason: the detail reaches the daemon in the runtime
/// fault, so no command name or environment value of a foreign process is
/// included; the worker log names the command.
fn describe_blockers(blockers: &[Blocker]) -> String {
    let mut listed: Vec<String> = blockers
        .iter()
        .take(MAX_LOGGED_CLEANUP_BLOCKERS)
        .map(|blocker| {
            format!(
                "pid {} (start {}, {})",
                blocker.identity.pid, blocker.identity.start_identity, blocker.reason
            )
        })
        .collect();
    if blockers.len() > MAX_LOGGED_CLEANUP_BLOCKERS {
        listed.push(format!(
            "and {} more",
            blockers.len() - MAX_LOGGED_CLEANUP_BLOCKERS
        ));
    }
    listed.join(", ")
}

async fn wait_for_output_completion(
    shared: &Shared,
    pty: &PtyOwner,
    output_eof_rx: &mut oneshot::Receiver<Result<OutputCompletion, WorkerError>>,
) -> Result<OutputCompletion, WorkerError> {
    let completion =
        match tokio::time::timeout(shared.config.post_exit_drain_timeout, output_eof_rx).await {
            Ok(Ok(Ok(completion))) => completion,
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(_closed)) => pty.output_completion().ok_or_else(|| {
                WorkerError::Protocol("PTY output monitor stopped before EOF".to_owned())
            })?,
            Err(_elapsed) => {
                event!(
                    name: "worker.pty.drain.expired",
                    Level::WARN,
                    process.pid = pty.identity().pid,
                    "PTY drain deadline expired; terminating the retained process group",
                );
                let grace = {
                    let state = shared.state.lock().await;
                    state.stop_grace
                };
                // A forced close is not an error: the sealed completion read
                // below records it and the root's exit is known.
                pty.stop("natural-drain-timeout", grace)
                    .await
                    .map_err(WorkerError::Pty)?;
                pty.output_completion().ok_or_else(|| {
                    WorkerError::Protocol(
                        "PTY cleanup returned without sealing output completion".to_owned(),
                    )
                })?
            }
        };
    Ok(completion)
}

async fn retry_runtime_completion(
    shared: &Shared,
    exit: &Exit,
    status: &ExitStatus,
    stopped_by_user: bool,
    exited_at: &str,
    next_output_offset: u64,
) -> Option<Duration> {
    let mut terminal_commit_failed = false;
    loop {
        match commit_runtime_completion(
            shared,
            exit,
            status,
            stopped_by_user,
            exited_at,
            next_output_offset,
        )
        .await
        {
            Ok(retention) => {
                if terminal_commit_failed {
                    event!(
                        name: "worker.terminal_commit.recovered",
                        Level::INFO,
                        "worker terminal journal commit recovered",
                    );
                }
                return Some(retention);
            }
            Err(error) => {
                if !terminal_commit_failed {
                    event!(
                        name: "worker.terminal_commit.retry",
                        Level::WARN,
                        error.type = "journal",
                        error.message = %error,
                        "worker terminal journal commit failed; retrying: {{error.message}}",
                    );
                    terminal_commit_failed = true;
                }
                tokio::select! {
                    () = tokio::time::sleep(TERMINAL_JOURNAL_RETRY_DELAY) => {}
                    () = shared.shutdown.cancelled() => return None,
                }
            }
        }
    }
}

async fn commit_runtime_completion(
    shared: &Shared,
    exit: &Exit,
    status: &ExitStatus,
    stopped_by_user: bool,
    exited_at: &str,
    next_output_offset: u64,
) -> Result<Duration, WorkerError> {
    let mut state = shared.state.lock().await;
    let mut journal = state.journal.clone();
    journal.phase = JournalPhase::Terminal;
    journal.pending_launch_claims.clear();
    mark_running_subagents_lost(&mut journal, status.exited_at_ms);
    let background_cleared = background::clear(&mut journal);
    journal.outcome = Some(RuntimeOutcome {
        exit_code: exit.exit_code,
        signal: exit.signal.clone(),
        success: exit.success,
        exited_at: exited_at.to_owned(),
        reason: if stopped_by_user {
            "explicit_stop".to_owned()
        } else {
            "natural_exit".to_owned()
        },
        output_forced_closed: status.output_forced_closed,
    });
    journal.next_output_offset = next_output_offset;
    journal.updated_at = timestamp();
    persist(shared.journal.clone(), journal.clone()).await?;
    state.phase = WireRuntimePhase::Exited;
    state.exit = Some(status.clone());
    state.journal = journal;
    if background_cleared {
        log_background_cleared(shared, "runtime_end");
    }
    let state = &mut *state;
    state.subagent_evidence.retain_recorded(&state.journal);
    shared.state_changed.notify_waiters();
    Ok(state.terminal_retention)
}

async fn wait_runtime_exit(pty: &PtyOwner) -> Result<Exit, PtyError> {
    pty.wait_exit().await
}

async fn mark_runtime_faulted(
    shared: &Shared,
    worker_instance_id: WorkerInstanceId,
    error: WorkerError,
) {
    mark_runtime_faulted_at_offset(shared, worker_instance_id, error, None).await;
}

async fn mark_runtime_faulted_at_offset(
    shared: &Shared,
    worker_instance_id: WorkerInstanceId,
    error: WorkerError,
    next_output_offset: Option<u64>,
) {
    let (commit, background_cleared) = {
        let mut state = shared.state.lock().await;
        state.phase = WireRuntimePhase::Faulted;
        let mut journal = state.journal.clone();
        journal.phase = JournalPhase::Faulted;
        if let Some(next_output_offset) = next_output_offset {
            journal.next_output_offset = next_output_offset;
        }
        journal.pending_launch_claims.clear();
        mark_running_subagents_lost(&mut journal, unix_ms());
        let background_cleared = background::clear(&mut journal);
        journal.updated_at = timestamp();
        let commit = persist(shared.journal.clone(), journal.clone()).await;
        let committed = commit.is_ok();
        state.journal = journal;
        let state = &mut *state;
        state.subagent_evidence.retain_recorded(&state.journal);
        (commit, background_cleared && committed)
    };
    if background_cleared {
        log_background_cleared(shared, "runtime_end");
    }
    shared.state_changed.notify_waiters();
    if let Err(persist_error) = commit {
        event!(
            name: "worker.journal.write.failed",
            Level::ERROR,
            error.type = "journal",
            error.message = %persist_error,
            "worker journal write failed: {{error.message}}",
        );
    }
    shared.emit(EventKind::RuntimeFault {
        worker_instance_id: Some(worker_instance_id),
        error: control_error(ControlCode::RuntimeFault, error, false),
    });
}

#[expect(
    clippy::too_many_lines,
    reason = "opening redeems the token and captures the stream's start state in one authenticated sequence"
)]
async fn serve_data(
    shared: Arc<Shared>,
    stream: PrefixStream<UnixStream>,
    peer: Arc<PeerContext>,
) -> Result<(), WorkerError> {
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    // Register before redemption so a lease release in the handoff window is
    // retained by the watch receiver instead of becoming an invisible past event.
    let mut lease_epoch = shared.lease_epoch_tx.subscribe();
    let open = protocol::read_frame(&mut read_half)
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))?
        .ok_or_else(|| WorkerError::Protocol("data stream closed before open".to_owned()))?;
    let (header, payload) = open.into_parts();
    if !payload.is_empty() {
        return Err(WorkerError::Protocol(
            "data open frame carried a payload".to_owned(),
        ));
    }
    let FrameKind::Open {
        token,
        mode,
        after_offset,
        attach,
    } = header.kind.clone()
    else {
        return Err(WorkerError::Protocol(
            "first data frame must open a stream".to_owned(),
        ));
    };
    let redeemed_epoch = *lease_epoch.borrow();
    let grant = redeem_data_grant(
        &shared.tokens,
        &shared.lease,
        redeemed_epoch,
        shared.now_ms(),
        &token,
        &header,
        mode,
        after_offset,
        attach.as_ref(),
        &peer,
    )
    .inspect_err(|error| {
        if let WorkerError::PeerIdentity { reason } = error {
            reject_identity(&shared, "redeem_data_token", *reason, &peer);
        }
    })?;
    if *lease_epoch.borrow() != grant.lease_epoch {
        return Err(WorkerError::Protocol(
            "data token lease generation changed during redemption".to_owned(),
        ));
    }
    let pty = {
        let state = shared.state.lock().await;
        state
            .pty
            .clone()
            .ok_or_else(|| WorkerError::Protocol("runtime is not live".to_owned()))?
    };
    if mode == StreamMode::Observation {
        let observation = grant.observation.ok_or_else(|| {
            WorkerError::Protocol("observation data grant is incomplete".to_owned())
        })?;
        return serve_observation_data(
            &shared,
            &pty,
            &mut read_half,
            &mut write_half,
            header.version,
            &header.stream_id,
            &header.worker_instance_id,
            after_offset,
            observation,
            grant.lease_epoch,
            &mut lease_epoch,
            &peer,
            &grant.lease_owner,
        )
        .await;
    }
    let subscriber = if let Some(attach) = attach {
        let dimensions = attach
            .dimensions
            .map(|dimensions| (dimensions.columns(), dimensions.rows()));
        let (subscriber, (cols, rows)) = match pty.attach_snapshot(dimensions).await {
            Ok(result) => result,
            Err(error) => {
                return write_data_error(
                    &mut write_half,
                    header.version,
                    &header.stream_id,
                    &header.worker_instance_id,
                    control_error(ControlCode::RuntimeFault, error, true),
                )
                .await;
            }
        };
        let dimensions = Dimensions::new(cols, rows)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let mut state = shared.state.lock().await;
        state.journal.cols = Some(cols);
        state.journal.rows = Some(rows);
        drop(state);
        write_data_frame(
            &mut write_half,
            header.version,
            &header.stream_id,
            &header.worker_instance_id,
            FrameKind::AttachReady { dimensions },
            Vec::new(),
        )
        .await?;
        subscriber
    } else {
        pty.subscribe_output(after_offset)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?
    };
    serve_stream_data(
        &shared,
        &pty,
        read_half,
        &mut write_half,
        header.version,
        &header.stream_id,
        &header.worker_instance_id,
        mode,
        subscriber,
        grant.lease_epoch,
        &mut lease_epoch,
        &peer,
        &grant.lease_owner,
    )
    .await
}

/// Streams live output and, on an attach stream, accepts framed input.
///
/// One input read stays pending across every `select!` iteration (see
/// [`read_owned_frame`]), so output that becomes ready while an input frame
/// is only partly received never discards the bytes already consumed.
#[expect(
    clippy::too_many_arguments,
    reason = "stream framing keeps authenticated stream identity and lease explicit"
)]
#[expect(
    clippy::too_many_lines,
    reason = "the bidirectional data-stream select loop keeps lease, output, and input ordering in one place"
)]
async fn serve_stream_data<R, W>(
    shared: &Shared,
    pty: &PtyOwner,
    reader: R,
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    worker_instance_id: &WorkerInstanceId,
    mode: StreamMode,
    mut subscriber: OutputSubscriber,
    expected_lease_epoch: u64,
    lease_epoch: &mut watch::Receiver<u64>,
    peer: &PeerContext,
    owner: &LeaseOwner,
) -> Result<(), WorkerError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut next_input_sequence = 1_u64;
    let mut root_exit = pty.exit_receiver();
    let mut pending_output_exit = None;
    let next_input = read_owned_frame(reader);
    tokio::pin!(next_input);

    loop {
        tokio::select! {
            changed = lease_epoch.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                write_data_frame(
                    writer,
                    version,
                    stream_id,
                    worker_instance_id,
                    FrameKind::Close { reason: CloseReason::LeaseReleased },
                    Vec::new(),
                ).await?;
                return Ok(());
            }
            changed = root_exit.changed(), if pending_output_exit.is_some() => {
                changed.map_err(|_closed| WorkerError::Pty(PtyError::ExitTimeout))?;
                if root_exit.borrow().is_some() {
                    let output = pending_output_exit
                        .take()
                        .expect("guarded pending PTY EOF");
                    if *lease_epoch.borrow() != expected_lease_epoch {
                        return Ok(());
                    }
                    authorize_data_frame(shared, peer, owner)?;
                    write_runtime_output_event(
                        writer,
                        version,
                        stream_id,
                        worker_instance_id,
                        shared.config.data_payload_bytes,
                        output,
                        pty.output_forced_closed(),
                    ).await?;
                }
            }
            output = subscriber.recv(), if pending_output_exit.is_none() => {
                let Some(output) = output else {
                    return Ok(());
                };
                if matches!(&output, OutputEvent::Exit { .. })
                    && root_exit.borrow().is_none()
                {
                    pending_output_exit = Some(output);
                    continue;
                }
                authorize_data_frame(shared, peer, owner)?;
                write_runtime_output_event(
                    writer,
                    version,
                    stream_id,
                    worker_instance_id,
                    shared.config.data_payload_bytes,
                    output,
                    pty.output_forced_closed(),
                ).await?;
            }
            (reader, input) = &mut next_input => {
                next_input.set(read_owned_frame(reader));
                let input = match input {
                    Ok(Some(input)) => input,
                    Ok(None) => return Ok(()),
                    Err(error) => {
                        return write_data_error(
                            writer,
                            version,
                            stream_id,
                            worker_instance_id,
                            control_error(ControlCode::InvalidRequest, error, false),
                        ).await;
                    }
                };
                authorize_data_frame(shared, peer, owner)?;
                if mode != StreamMode::Attach {
                    return write_data_error(
                        writer,
                        version,
                        stream_id,
                        worker_instance_id,
                        control_error_message(
                            ControlCode::InvalidRequest,
                            "output data stream received a client frame",
                            false,
                        ),
                    ).await;
                }
                let (input_header, bytes) = input.into_parts();
                let FrameKind::Input { write_id } = input_header.kind else {
                    return write_data_error(
                        writer,
                        version,
                        stream_id,
                        worker_instance_id,
                        control_error_message(
                            ControlCode::InvalidRequest,
                            "attach data stream received a non-input frame",
                            false,
                        ),
                    ).await;
                };
                if input_header.worker_instance_id != *worker_instance_id || input_header.stream_id != *stream_id {
                    return write_data_error(
                        writer,
                        version,
                        stream_id,
                        worker_instance_id,
                        identity_mismatch(),
                    ).await;
                }
                if let Err(error) =
                    validate_attach_write_id(&write_id, stream_id, next_input_sequence)
                {
                    return write_data_error(
                        writer,
                        version,
                        stream_id,
                        worker_instance_id,
                        error,
                    ).await;
                }
                let byte_count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if let Err(error) = pty.input().execute_stream(vec![InputFragment {
                        bytes,
                        delay_after: Duration::ZERO,
                    }]).await {
                    return write_data_error(
                        writer,
                        version,
                        stream_id,
                        worker_instance_id,
                        input_control_error(error, version),
                    ).await;
                }
                write_data_frame(
                    writer,
                    version,
                    stream_id,
                    worker_instance_id,
                    FrameKind::InputAck {
                        write_id,
                        bytes_written: byte_count,
                    },
                    Vec::new(),
                ).await?;
                next_input_sequence = match next_input_sequence.checked_add(1) {
                    Some(sequence) => sequence,
                    None => {
                        return write_data_error(
                            writer,
                            version,
                            stream_id,
                            worker_instance_id,
                            control_error_message(
                                ControlCode::InvalidRequest,
                                "attach input sequence was exhausted",
                                false,
                            ),
                        ).await;
                    }
                };
            }
        }
    }
}

/// Reads one client frame and hands the reader back with the result.
///
/// [`protocol::read_frame`] is not cancel-safe: dropping it after part of a
/// frame was consumed loses those bytes and desynchronizes the stream. Owning
/// the reader lets the caller keep one read pending across `select!`
/// iterations instead.
async fn read_owned_frame<R>(mut reader: R) -> (R, Result<Option<DataFrame>, FrameError>)
where
    R: AsyncRead + Unpin + Send,
{
    let frame = protocol::read_frame(&mut reader).await;
    (reader, frame)
}

#[expect(
    clippy::too_many_arguments,
    reason = "redemption validates every authenticated stream claim before consuming the token"
)]
fn redeem_data_grant(
    tokens: &Mutex<TokenState>,
    lease: &ControllerLease,
    current_lease_epoch: u64,
    now_ms: u64,
    token: &DataToken,
    header: &FrameHeader,
    mode: StreamMode,
    after_offset: Option<u64>,
    attach: Option<&AttachStart>,
    peer: &PeerContext,
) -> Result<DataGrant, WorkerError> {
    let mut tokens = lock(tokens);
    // Checked before `redeem`, which consumes the grant regardless of its
    // validation result: a foreign attempt must not burn a legitimate token.
    if let Some(owner) = tokens.grant_owner(token, now_ms) {
        peer.reverify()
            .and_then(|()| {
                let identity = peer.identity();
                if owner.peer_pid == identity.pid
                    && owner.peer_start_identity == identity.start_identity.to_string()
                {
                    Ok(())
                } else {
                    Err(RejectReason::LeaseOwnerMismatch)
                }
            })
            .map_err(|reason| WorkerError::PeerIdentity { reason })?;
    }
    tokens.redeem(token, now_ms, |grant| {
        lease
            .validate(&grant.lease_owner, grant.lease_id.as_str())
            .map_err(|_error| {
                WorkerError::Protocol("data token lease is no longer active".to_owned())
            })?;
        if grant.lease_epoch != current_lease_epoch {
            return Err(WorkerError::Protocol(
                "data token lease generation is stale".to_owned(),
            ));
        }
        if grant.version != header.version
            || grant.worker_instance_id != header.worker_instance_id
            || grant.stream_id != header.stream_id
            || grant.mode != mode
            || grant.after_offset != after_offset
            || grant.attach.as_ref() != attach
        {
            return Err(WorkerError::Protocol(
                "data open frame does not match its grant".to_owned(),
            ));
        }
        Ok(())
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "observation framing keeps authenticated stream identity explicit"
)]
async fn serve_observation_data<R, W>(
    shared: &Shared,
    pty: &PtyOwner,
    reader: &mut R,
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    worker_instance_id: &WorkerInstanceId,
    after_offset: Option<u64>,
    observation: ObservationGrant,
    expected_lease_epoch: u64,
    lease_epoch: &mut watch::Receiver<u64>,
    peer: &PeerContext,
    owner: &LeaseOwner,
) -> Result<(), WorkerError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let output = pty.output();
    let outcome = await_observation_page(
        output,
        after_offset,
        observation,
        &shared.shutdown,
        expected_lease_epoch,
        lease_epoch,
        reader,
    )
    .await?;
    let (page, timed_out) = match outcome {
        ObservationWaitOutcome::Page { page, timed_out } => (page, timed_out),
        ObservationWaitOutcome::LeaseReleased => {
            return write_data_frame(
                writer,
                version,
                stream_id,
                worker_instance_id,
                FrameKind::Close {
                    reason: CloseReason::LeaseReleased,
                },
                Vec::new(),
            )
            .await;
        }
        ObservationWaitOutcome::Disconnected | ObservationWaitOutcome::Shutdown => return Ok(()),
        ObservationWaitOutcome::InvalidInput => {
            return write_data_error(
                writer,
                version,
                stream_id,
                worker_instance_id,
                control_error_message(
                    ControlCode::InvalidRequest,
                    "observation stream is output-only",
                    false,
                ),
            )
            .await;
        }
    };

    // The wait above can last seconds, so the peer that redeemed the token is
    // proved again before any PTY history leaves the worker.
    authorize_data_frame(shared, peer, owner)?;

    if *lease_epoch.borrow() != expected_lease_epoch {
        return write_data_frame(
            writer,
            version,
            stream_id,
            worker_instance_id,
            FrameKind::Close {
                reason: CloseReason::LeaseReleased,
            },
            Vec::new(),
        )
        .await;
    }

    write_observation_page(
        writer,
        version,
        stream_id,
        worker_instance_id,
        shared.config.data_payload_bytes,
        page,
        timed_out,
    )
    .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ObservationWaitOutcome {
    Page {
        page: crate::ObservationPage,
        timed_out: bool,
    },
    LeaseReleased,
    Disconnected,
    Shutdown,
    InvalidInput,
}

async fn await_observation_page<R>(
    output: &crate::OutputHub,
    after_offset: Option<u64>,
    observation: ObservationGrant,
    shutdown: &CancellationToken,
    expected_lease_epoch: u64,
    lease_epoch: &mut watch::Receiver<u64>,
    reader: &mut R,
) -> Result<ObservationWaitOutcome, WorkerError>
where
    R: AsyncRead + Unpin + Send,
{
    let deadline = tokio::time::sleep(observation.wait);
    tokio::pin!(deadline);
    loop {
        if *lease_epoch.borrow() != expected_lease_epoch {
            return Ok(ObservationWaitOutcome::LeaseReleased);
        }
        let page = output
            .observe(after_offset, observation.max_bytes)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let waiting_at_end = after_offset.is_some()
            && page.start_offset == page.runtime_end_offset
            && !page.exited
            && !observation.wait.is_zero();
        if !waiting_at_end {
            if *lease_epoch.borrow() != expected_lease_epoch {
                return Ok(ObservationWaitOutcome::LeaseReleased);
            }
            return Ok(ObservationWaitOutcome::Page {
                page,
                timed_out: false,
            });
        }
        tokio::select! {
            () = output.wait_for_observation(page.runtime_end_offset, shutdown) => {
                if shutdown.is_cancelled() {
                    return Ok(ObservationWaitOutcome::Shutdown);
                }
            }
            changed = lease_epoch.changed() => {
                return Ok(if changed.is_ok() {
                    ObservationWaitOutcome::LeaseReleased
                } else {
                    ObservationWaitOutcome::Shutdown
                });
            }
            incoming = protocol::read_frame(reader) => {
                return match incoming {
                    Ok(None) => Ok(ObservationWaitOutcome::Disconnected),
                    Ok(Some(_)) => Ok(ObservationWaitOutcome::InvalidInput),
                    Err(error) => Err(WorkerError::Protocol(error.to_string())),
                };
            }
            () = &mut deadline => {
                if *lease_epoch.borrow() != expected_lease_epoch {
                    return Ok(ObservationWaitOutcome::LeaseReleased);
                }
                let final_page = output
                    .observe(after_offset, observation.max_bytes)
                    .map_err(|error| WorkerError::Protocol(error.to_string()))?;
                let timed_out = observation_timed_out(after_offset, &final_page);
                return Ok(ObservationWaitOutcome::Page { page: final_page, timed_out });
            }
        }
    }
}

fn observation_timed_out(after_offset: Option<u64>, page: &crate::ObservationPage) -> bool {
    after_offset.is_some()
        && page.start_offset == page.runtime_end_offset
        && page.bytes.is_empty()
        && !page.exited
}

async fn write_observation_page<W>(
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    worker_instance_id: &WorkerInstanceId,
    payload_limit: usize,
    page: crate::ObservationPage,
    timed_out: bool,
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    let gap = page.gap.map(|missing| OutputGap {
        missing_start: missing.start,
        missing_end: missing.end,
    });
    write_data_frame(
        writer,
        version,
        stream_id,
        worker_instance_id,
        FrameKind::ObservationStart {
            history_start_offset: page.history_start_offset,
            start_offset: page.start_offset,
            next_offset: page.next_offset,
            runtime_end_offset: page.runtime_end_offset,
            gap,
            has_more: page.has_more,
            timed_out,
        },
        Vec::new(),
    )
    .await?;
    write_output_chunks(
        writer,
        version,
        stream_id,
        worker_instance_id,
        payload_limit,
        OutputChunk {
            offset: page.start_offset,
            bytes: page.bytes,
        },
        true,
    )
    .await?;
    write_data_frame(
        writer,
        version,
        stream_id,
        worker_instance_id,
        FrameKind::Close {
            reason: CloseReason::ObservationComplete,
        },
        Vec::new(),
    )
    .await
}

async fn write_data_error<W>(
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    worker_instance_id: &WorkerInstanceId,
    error: ControlError,
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    write_data_frame(
        writer,
        version,
        stream_id,
        worker_instance_id,
        FrameKind::Error { error },
        Vec::new(),
    )
    .await
}

async fn write_output_event<W>(
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    worker_instance_id: &WorkerInstanceId,
    payload_limit: usize,
    output: OutputEvent,
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    match output {
        OutputEvent::Replay(chunk) if chunk.bytes.is_empty() => Ok(()),
        OutputEvent::Replay(chunk) => {
            write_output_chunks(
                writer,
                version,
                stream_id,
                worker_instance_id,
                payload_limit,
                chunk,
                true,
            )
            .await
        }
        OutputEvent::Output(chunk) => {
            write_output_chunks(
                writer,
                version,
                stream_id,
                worker_instance_id,
                payload_limit,
                chunk,
                false,
            )
            .await
        }
        OutputEvent::Gap { missing, watermark } => {
            write_data_frame(
                writer,
                version,
                stream_id,
                worker_instance_id,
                FrameKind::Gap {
                    missing_start: missing.start,
                    missing_end: missing.end,
                    watermark,
                },
                Vec::new(),
            )
            .await
        }
        OutputEvent::TerminalSnapshot(chunk) => {
            let snapshot = wire_terminal(chunk.terminal())?;
            write_terminal_chunks(
                writer,
                version,
                stream_id,
                worker_instance_id,
                payload_limit,
                &snapshot,
                &chunk.bytes,
            )
            .await
        }
        OutputEvent::Exit { .. } => {
            write_data_frame(
                writer,
                version,
                stream_id,
                worker_instance_id,
                FrameKind::Close {
                    reason: CloseReason::RuntimeExited,
                },
                Vec::new(),
            )
            .await
        }
    }
}

async fn write_runtime_output_event<W>(
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    worker_instance_id: &WorkerInstanceId,
    payload_limit: usize,
    output: OutputEvent,
    forced_closed: bool,
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    if forced_closed && matches!(output, OutputEvent::Exit { .. }) {
        return write_data_error(
            writer,
            version,
            stream_id,
            worker_instance_id,
            control_error(
                ControlCode::RuntimeFault,
                PtyError::OutputForcedClosed,
                false,
            ),
        )
        .await;
    }
    write_output_event(
        writer,
        version,
        stream_id,
        worker_instance_id,
        payload_limit,
        output,
    )
    .await
}

async fn write_output_chunks<W>(
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    worker_instance_id: &WorkerInstanceId,
    payload_limit: usize,
    chunk: OutputChunk,
    replay: bool,
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    let mut chunk_offset = chunk.offset;
    for bytes in chunk.bytes.chunks(payload_limit) {
        let kind = if replay {
            FrameKind::Replay {
                offset: chunk_offset,
            }
        } else {
            FrameKind::Output {
                offset: chunk_offset,
            }
        };
        write_data_frame(
            writer,
            version,
            stream_id,
            worker_instance_id,
            kind,
            bytes.to_vec(),
        )
        .await?;
        chunk_offset = chunk_offset
            .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| WorkerError::Protocol("output frame offset overflowed".to_owned()))?;
    }
    Ok(())
}

async fn write_terminal_chunks<W>(
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    worker_instance_id: &WorkerInstanceId,
    payload_limit: usize,
    snapshot: &WireTerminalSnapshot,
    ansi: &[u8],
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    for bytes in ansi.chunks(payload_limit) {
        write_data_frame(
            writer,
            version,
            stream_id,
            worker_instance_id,
            FrameKind::TerminalSnapshot {
                snapshot: snapshot.clone(),
            },
            bytes.to_vec(),
        )
        .await?;
    }
    Ok(())
}

async fn write_data_frame<W>(
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    worker_instance_id: &WorkerInstanceId,
    kind: FrameKind,
    payload: Vec<u8>,
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    let frame = DataFrame::new(
        FrameHeader {
            version,
            stream_id: stream_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
            kind,
        },
        payload,
    )
    .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    protocol::write_frame(writer, &frame)
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HookRequest {
    IdentityReport {
        #[serde(rename = "runtime_id")]
        worker_instance_id: String,
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
        expires_at: String,
        reference_kind: Option<String>,
        native_reference: Option<String>,
        /// How the provider session started, as the hook forwards it. Only
        /// `startup` and `resume` clear the background snapshot.
        #[serde(default)]
        session_source: Option<String>,
    },
    IdentityRelease {
        #[serde(rename = "runtime_id")]
        worker_instance_id: String,
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
        /// Why the session ended, as the hook forwards it. `clear` keeps the
        /// background snapshot, because the same process starts the session again.
        #[serde(default)]
        release_reason: Option<String>,
    },
    SubagentStart {
        #[serde(rename = "runtime_id")]
        worker_instance_id: String,
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
        subagent_id: String,
        parent_id: Option<String>,
        agent_type: Option<String>,
    },
    SubagentStop {
        #[serde(rename = "runtime_id")]
        worker_instance_id: String,
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
        subagent_id: String,
        outcome: Option<JournalSubagentPhase>,
    },
    SubagentSnapshot(SnapshotRequest),
    NotificationCreate {
        #[serde(rename = "runtime_id")]
        worker_instance_id: String,
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
        params: serde_json::Value,
    },
}

/// A provider's list of its live background tasks, reported by the agent
/// process at the end of a main-agent turn.
#[derive(Debug, Deserialize)]
struct SnapshotRequest {
    #[serde(rename = "runtime_id")]
    worker_instance_id: String,
    provider: String,
    pid: u32,
    start_identity: u64,
    /// Monotonic time taken when the hook started, before it read its input.
    sequence: u64,
    tasks: Vec<SnapshotTask>,
}

/// One task of a [`SnapshotRequest`], as the provider labels it.
#[derive(Deserialize)]
struct SnapshotTask {
    /// Task id; for a background subagent it equals the subagent id.
    id: String,
    /// Provider task-type label, matched against [`AGENTLESS_TASK_TYPES`].
    #[serde(rename = "type")]
    task_type: String,
    /// Provider task status; validated but not interpreted.
    status: String,
    /// Sanitized task description. Kept as raw JSON so that a wrong-typed
    /// value drops this field alone.
    #[serde(default)]
    label: Option<serde_json::Value>,
    /// Program name of a shell task, never its arguments.
    #[serde(default)]
    program: Option<serde_json::Value>,
    /// Provider agent type of a subagent task.
    #[serde(default)]
    agent_type: Option<serde_json::Value>,
}

impl std::fmt::Debug for SnapshotTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The optional fields are provider text, so only their presence is shown.
        f.debug_struct("SnapshotTask")
            .field("id", &self.id)
            .field("task_type", &self.task_type)
            .field("status", &self.status)
            .field("label", &self.label.as_ref().map(|_| "[REDACTED]"))
            .field("program", &self.program.as_ref().map(|_| "[REDACTED]"))
            .field(
                "agent_type",
                &self.agent_type.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Debug, Serialize)]
struct HookResponse {
    ok: bool,
    launch_identity_accepted: bool,
    launch_identity_status: LaunchClaimStatus,
}

async fn serve_identity_hook<W>(
    shared: Arc<Shared>,
    writer: &mut ControlWriter<W>,
    value: serde_json::Value,
    peer: &PeerContext,
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    serve_identity_hook_with_probe(Arc::clone(&shared), writer, value, peer, |claim| {
        verify_launch_claim(&shared, claim)
    })
    .await
}

#[expect(
    clippy::too_many_lines,
    reason = "identity report and release share one validated response path"
)]
async fn serve_identity_hook_with_probe<W>(
    shared: Arc<Shared>,
    writer: &mut ControlWriter<W>,
    value: serde_json::Value,
    peer: &PeerContext,
    mut verify: impl FnMut(&PendingLaunchClaim) -> Result<bool, WorkerError> + Send,
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    let request: HookRequest =
        serde_json::from_value(value).map_err(|error| WorkerError::Protocol(error.to_string()))?;
    let mut launch_identity_status = LaunchClaimStatus::NotApplicable;
    let (accepted, launch_identity_accepted, worker_instance_id, subagent_outcome) = match request {
        HookRequest::IdentityReport {
            worker_instance_id,
            provider,
            pid,
            start_identity,
            sequence,
            expires_at,
            reference_kind,
            native_reference,
            session_source,
        } => {
            let mut state = shared.state.lock().await;
            let current_runtime = state.worker_instance_id.as_ref().map(ToString::to_string);
            let admitted = admit_hook(
                state.hook_schema,
                HookAction::IdentityReport,
                &provider,
                state.launch_agent_base.as_deref(),
            );
            let inadmissible = hook_claim_admissible(
                admitted.as_ref().err().copied(),
                current_runtime.as_deref() == Some(worker_instance_id.as_str()),
                state.phase == WireRuntimePhase::Running,
            )
            .or_else(|| (!valid_identity_expiry(&expires_at)).then_some(RejectReason::ClaimExpired))
            .or_else(|| {
                admitted
                    .is_ok_and(|schema| {
                        !native_reference_admitted(
                            schema,
                            reference_kind.as_deref(),
                            native_reference.as_deref(),
                        )
                    })
                    .then_some(RejectReason::NativeReferenceInvalid)
            });
            if let Some(reason) = inadmissible {
                reject_identity(&shared, "identity_report", reason, peer);
                (false, false, current_runtime, None)
            } else {
                let ancestry = admitted_ancestry(admitted)?;
                let pty = state.pty.as_ref().ok_or_else(|| {
                    WorkerError::Protocol("runtime is not initialized".to_owned())
                })?;
                let root_identity = pty.identity().clone();
                let process_valid = hook_claim_valid(
                    &shared,
                    "identity_report",
                    peer,
                    ancestry,
                    pid,
                    start_identity,
                    root_identity.pid,
                ) && root_report_from_root(&shared, peer, pid, &root_identity);
                let reporting = Reporter {
                    pid,
                    start_identity,
                };
                let sequence_valid = identity_sequence_is_fresh(
                    &state.journal,
                    &state.identity_marks,
                    reporting,
                    sequence,
                );
                let pinned = identity_pins(&root_identity, state.journal.launch_identity.as_ref());
                let room = process_valid
                    && sequence_valid
                    && admit_reporter(&shared, &mut state.identity_marks, reporting, &pinned, peer);
                // The background snapshot follows its reporting process, so an
                // admitted, process-valid report clears it even when the identity
                // rules below refuse the report.
                let background_clear = process_valid
                    .then(|| {
                        report_background_clear(
                            &state.journal,
                            pid,
                            start_identity,
                            sequence,
                            session_source.as_deref(),
                        )
                    })
                    .flatten();
                if !sequence_valid {
                    reject_identity(
                        &shared,
                        "identity_report",
                        RejectReason::SequenceStale,
                        peer,
                    );
                }
                if !process_valid || !sequence_valid || !room {
                    if background_clear.is_some() {
                        let mut journal = state.journal.clone();
                        if background::clear_at(&mut journal, sequence) {
                            journal.updated_at = timestamp();
                            persist(shared.journal.clone(), journal.clone()).await?;
                            state.journal = journal;
                            if let Some(reason) = background_clear {
                                log_background_cleared(&shared, reason);
                            }
                        }
                    }
                    (false, false, current_runtime, None)
                } else {
                    let mut journal = state.journal.clone();
                    let process = ChildIdentity {
                        pid,
                        process_group: root_identity.process_group,
                        start_identity: start_identity.to_string(),
                    };
                    journal.active_identity = Some(ActiveIdentity {
                        provider: provider.clone(),
                        process: process.clone(),
                        sequence,
                        expires_at: expires_at.clone(),
                        reference_kind: reference_kind.clone(),
                        native_reference: native_reference.clone(),
                    });
                    journal.active_identity_release = None;
                    let background_cleared = background_clear.and_then(|reason| {
                        background::clear_at(&mut journal, sequence).then_some(reason)
                    });
                    if let (Some(reference_kind), Some(native_reference), Some(launch_base)) = (
                        reference_kind,
                        native_reference,
                        state.launch_agent_base.as_ref(),
                    ) {
                        if &provider == launch_base {
                            let reported = NativeReferenceClaim {
                                provider: provider.clone(),
                                process: process.clone(),
                                sequence,
                                reference_kind: reference_kind.clone(),
                                native_reference: native_reference.clone(),
                            };
                            let claim = PendingLaunchClaim {
                                retry_pending: true,
                                worker_instance_id,
                                root: journal_identity(&root_identity),
                                identity: LaunchIdentity {
                                    provider,
                                    process,
                                    reference_kind,
                                    native_reference,
                                },
                                expires_at,
                                sequence,
                                latest_reference: None,
                            };
                            launch_identity_status = apply_launch_report(
                                &shared,
                                &mut journal,
                                claim,
                                reported,
                                &mut verify,
                            )?;
                        }
                    }
                    journal.updated_at = timestamp();
                    persist(shared.journal.clone(), journal.clone()).await?;
                    state.journal = journal;
                    state.identity_marks.record(reporting, sequence);
                    if let Some(reason) = background_cleared {
                        log_background_cleared(&shared, reason);
                    }
                    (
                        true,
                        launch_identity_status == LaunchClaimStatus::Accepted,
                        current_runtime,
                        None,
                    )
                }
            }
        }
        HookRequest::IdentityRelease {
            worker_instance_id,
            provider,
            pid,
            start_identity,
            sequence,
            release_reason,
        } => {
            let mut state = shared.state.lock().await;
            let current_runtime = state.worker_instance_id.as_ref().map(ToString::to_string);
            let admitted = admit_hook(
                state.hook_schema,
                HookAction::IdentityRelease,
                &provider,
                state.launch_agent_base.as_deref(),
            );
            let inadmissible = hook_claim_admissible(
                admitted.as_ref().err().copied(),
                current_runtime.as_deref() == Some(worker_instance_id.as_str()),
                true,
            );
            if let Some(reason) = inadmissible {
                reject_identity(&shared, "identity_release", reason, peer);
                (false, false, current_runtime, None)
            } else {
                let ancestry = admitted_ancestry(admitted)?;
                let pty = state.pty.as_ref().ok_or_else(|| {
                    WorkerError::Protocol("runtime is not initialized".to_owned())
                })?;
                let root_identity = pty.identity().clone();
                let process_valid = hook_claim_valid(
                    &shared,
                    "identity_release",
                    peer,
                    ancestry,
                    pid,
                    start_identity,
                    root_identity.pid,
                ) && root_report_from_root(&shared, peer, pid, &root_identity);
                // A `/clear` ends the session and starts it again in the same
                // process, so only the other reasons end the snapshot's owner.
                let owner_released = process_valid
                    && release_reason.as_deref() != Some("clear")
                    && sequence > state.journal.background_sequence
                    && state
                        .journal
                        .background_owner
                        .as_ref()
                        .is_some_and(|owner| {
                            owner.pid == pid && owner.start_identity == start_identity.to_string()
                        });
                let reporting = Reporter {
                    pid,
                    start_identity,
                };
                let pinned = identity_pins(&root_identity, state.journal.launch_identity.as_ref());
                let fresh = process_valid
                    && identity_sequence_is_fresh(
                        &state.journal,
                        &state.identity_marks,
                        reporting,
                        sequence,
                    )
                    && admit_reporter(&shared, &mut state.identity_marks, reporting, &pinned, peer);
                let released_process = if fresh {
                    state
                        .journal
                        .active_identity
                        .as_ref()
                        .filter(|active| {
                            active.provider == provider && is_reporter(&active.process, reporting)
                        })
                        .map(|active| active.process.clone())
                } else {
                    None
                };
                if let Some(process) = released_process {
                    let mut journal = state.journal.clone();
                    journal.active_identity = None;
                    journal.active_identity_release = Some(ReleasedIdentity {
                        provider,
                        process,
                        sequence,
                    });
                    let background_cleared =
                        owner_released && background::clear_at(&mut journal, sequence);
                    journal.updated_at = timestamp();
                    persist(shared.journal.clone(), journal.clone()).await?;
                    state.journal = journal;
                    state.identity_marks.record(reporting, sequence);
                    if background_cleared {
                        log_background_cleared(&shared, "release");
                    }
                    (true, false, current_runtime, None)
                } else {
                    // A fresh release of a process that is not active ends no
                    // identity and is answered `ok: false`, but it still makes
                    // that process's earlier claims stale when they are
                    // delivered after it.
                    if fresh {
                        state.identity_marks.record(reporting, sequence);
                    }
                    if owner_released {
                        // The identity release was not accepted, for example after a
                        // `clear` release whose session start never arrived. The owner
                        // still ends its snapshot.
                        let mut journal = state.journal.clone();
                        if background::clear_at(&mut journal, sequence) {
                            journal.updated_at = timestamp();
                            persist(shared.journal.clone(), journal.clone()).await?;
                            state.journal = journal;
                            log_background_cleared(&shared, "release");
                        }
                    }
                    (false, false, current_runtime, None)
                }
            }
        }
        HookRequest::SubagentStart {
            worker_instance_id,
            provider,
            pid,
            start_identity,
            sequence,
            subagent_id,
            parent_id,
            agent_type,
        } => {
            let occurred_at_ms = unix_ms();
            let mut state = shared.state.lock().await;
            let current_runtime = state.worker_instance_id.as_ref().map(ToString::to_string);
            let admitted = admit_hook(
                state.hook_schema,
                HookAction::SubagentStart,
                &provider,
                state.launch_agent_base.as_deref(),
            );
            let inadmissible = hook_claim_admissible(
                admitted.as_ref().err().copied(),
                current_runtime.as_deref() == Some(worker_instance_id.as_str()),
                state.phase == WireRuntimePhase::Running,
            )
            .or_else(|| {
                let fields_admitted = admitted.is_ok_and(|schema| {
                    schema.admits_subagents()
                        && (parent_id.is_none()
                            || schema.admits_subagent_field(SubagentField::ParentId))
                        && (agent_type.is_none()
                            || schema.admits_subagent_field(SubagentField::AgentType))
                });
                let text_valid = fields_admitted
                    && is_task_text(&subagent_id, MAX_SUBAGENT_ID_BYTES)
                    && parent_id
                        .as_deref()
                        .is_none_or(|value| is_task_text(value, MAX_SUBAGENT_ID_BYTES))
                    && agent_type
                        .as_deref()
                        .is_none_or(|value| is_task_text(value, MAX_SUBAGENT_TYPE_BYTES));
                (!text_valid).then_some(RejectReason::SubagentClaimInvalid)
            });
            if let Some(reason) = inadmissible {
                reject_identity(&shared, "subagent_start", reason, peer);
            }
            let valid = inadmissible.is_none();
            let process_valid = admitted.is_ok_and(|schema| {
                state.pty.as_ref().is_some_and(|pty| {
                    hook_claim_valid(
                        &shared,
                        "subagent_start",
                        peer,
                        schema.ancestry,
                        pid,
                        start_identity,
                        pty.identity().pid,
                    )
                })
            });
            if !valid || !process_valid {
                (false, false, current_runtime, None)
            } else {
                let (_, rule) = admitted_subagent_rule(admitted)?;
                let state = &mut *state;
                let claim = SubagentClaim {
                    action: "start",
                    provider: &provider,
                    id: &subagent_id,
                    sequence,
                };
                let mut journal = state.journal.clone();
                let outcome = start_subagent(
                    &mut journal,
                    rule,
                    claim,
                    parent_id.as_deref(),
                    agent_type.as_deref(),
                    state.orphan_stops.sequence(&provider, &subagent_id),
                    occurred_at_ms,
                );
                if outcome.persists() {
                    journal.updated_at = timestamp();
                    persist(shared.journal.clone(), journal.clone()).await?;
                    state.journal = journal;
                    // An advanced duplicate start begins a new run of the same
                    // id, so its reporter owns the run as an applied start's
                    // does.
                    state.subagent_evidence.record_start(
                        &provider,
                        &subagent_id,
                        Reporter {
                            pid,
                            start_identity,
                        },
                    );
                }
                if outcome == SubagentOutcome::Applied {
                    state.orphan_stops.forget(&provider, &subagent_id);
                } else {
                    log_subagent_outcome(&shared, claim, outcome);
                }
                (
                    outcome.acknowledged(),
                    false,
                    current_runtime,
                    Some(outcome),
                )
            }
        }
        HookRequest::SubagentStop {
            worker_instance_id,
            provider,
            pid,
            start_identity,
            sequence,
            subagent_id,
            outcome,
        } => {
            let occurred_at_ms = unix_ms();
            let mut state = shared.state.lock().await;
            let current_runtime = state.worker_instance_id.as_ref().map(ToString::to_string);
            let admitted = admit_hook(
                state.hook_schema,
                HookAction::SubagentStop,
                &provider,
                state.launch_agent_base.as_deref(),
            );
            let inadmissible = hook_claim_admissible(
                admitted.as_ref().err().copied(),
                current_runtime.as_deref() == Some(worker_instance_id.as_str()),
                state.phase == WireRuntimePhase::Running,
            )
            .or_else(|| {
                let outcome_admitted = admitted.is_ok_and(|schema| {
                    schema.admits_subagents()
                        && (outcome.is_none()
                            || schema.admits_subagent_field(SubagentField::Outcome))
                });
                let text_valid = outcome_admitted
                    && is_task_text(&subagent_id, MAX_SUBAGENT_ID_BYTES)
                    && outcome.is_none_or(|phase| phase != JournalSubagentPhase::Running);
                (!text_valid).then_some(RejectReason::SubagentClaimInvalid)
            });
            if let Some(reason) = inadmissible {
                reject_identity(&shared, "subagent_stop", reason, peer);
            }
            let valid = inadmissible.is_none();
            let process_valid = admitted.is_ok_and(|schema| {
                state.pty.as_ref().is_some_and(|pty| {
                    hook_claim_valid(
                        &shared,
                        "subagent_stop",
                        peer,
                        schema.ancestry,
                        pid,
                        start_identity,
                        pty.identity().pid,
                    )
                })
            });
            if !valid || !process_valid {
                (false, false, current_runtime, None)
            } else {
                let (schema, rule) = admitted_subagent_rule(admitted)?;
                let state = &mut *state;
                let claim = SubagentClaim {
                    action: "stop",
                    provider: &provider,
                    id: &subagent_id,
                    sequence,
                };
                let mut journal = state.journal.clone();
                let effect = stop_subagent(
                    &mut journal,
                    rule,
                    schema.stop_requires_start(&provider),
                    claim,
                    outcome.unwrap_or(JournalSubagentPhase::Completed),
                    occurred_at_ms,
                );
                if effect.persists() {
                    journal.updated_at = timestamp();
                    persist(shared.journal.clone(), journal.clone()).await?;
                    state.journal = journal;
                    state.subagent_evidence.retain_recorded(&state.journal);
                }
                if effect != SubagentOutcome::Applied {
                    if effect.reason() == Some(SubagentReason::StopWithoutStart) {
                        state.orphan_stops.record(&provider, &subagent_id, sequence);
                    }
                    log_subagent_outcome(&shared, claim, effect);
                }
                (effect.acknowledged(), false, current_runtime, Some(effect))
            }
        }
        HookRequest::SubagentSnapshot(snapshot) => {
            let ok = apply_subagent_snapshot(&shared, snapshot, peer).await?;
            return writer
                .write(&HookResponse {
                    ok,
                    launch_identity_accepted: false,
                    launch_identity_status: LaunchClaimStatus::NotApplicable,
                })
                .await
                .map_err(|error| WorkerError::Protocol(error.to_string()));
        }
        HookRequest::NotificationCreate {
            worker_instance_id,
            provider,
            pid,
            start_identity,
            sequence,
            params,
        } => {
            let line = {
                let state = shared.state.lock().await;
                let current_runtime = state.worker_instance_id.as_ref().map(ToString::to_string);
                let admitted = admit_hook(
                    state.hook_schema,
                    HookAction::Notification,
                    &provider,
                    state.launch_agent_base.as_deref(),
                );
                let inadmissible = hook_claim_admissible(
                    admitted.as_ref().err().copied(),
                    current_runtime.as_deref() == Some(worker_instance_id.as_str()),
                    state.phase == WireRuntimePhase::Running,
                );
                if let Some(reason) = inadmissible {
                    reject_identity(&shared, "notification_create", reason, peer);
                    None
                } else if !admitted.is_ok_and(|schema| {
                    state.pty.as_ref().is_some_and(|pty| {
                        hook_claim_valid(
                            &shared,
                            "notification_create",
                            peer,
                            schema.ancestry,
                            pid,
                            start_identity,
                            pty.identity().pid,
                        )
                    })
                }) {
                    None
                } else {
                    let line = state.public_protocol_version.and_then(|version| {
                        notification::build_request_line(
                            shared.session_id.as_str(),
                            version,
                            sequence,
                            params,
                            shared.config.max_notification_params_bytes,
                        )
                    });
                    if line.is_none() {
                        reject_identity(
                            &shared,
                            "notification_create",
                            RejectReason::NotificationInvalid,
                            peer,
                        );
                    }
                    line
                }
            };
            let delivered = match line {
                Some(line) => {
                    notification::deliver(
                        &shared.daemon_socket_path,
                        &line,
                        shared.config.notification_forward_timeout,
                    )
                    .await
                }
                None => false,
            };
            return writer
                .write(&HookResponse {
                    ok: delivered,
                    launch_identity_accepted: false,
                    launch_identity_status: LaunchClaimStatus::NotApplicable,
                })
                .await
                .map_err(|error| WorkerError::Protocol(error.to_string()));
        }
    };
    // A subagent claim emits only when it was applied; an ignored claim still
    // answers `ok` without an event, even when it persisted a newer sequence.
    let changed = subagent_outcome.map_or(accepted, |outcome| outcome == SubagentOutcome::Applied);
    if changed {
        if let Some(worker_instance_id) =
            worker_instance_id.and_then(|value| WorkerInstanceId::new(value).ok())
        {
            if subagent_outcome.is_some() {
                shared.emit(EventKind::SubagentsChanged { worker_instance_id });
            } else {
                shared.emit(EventKind::IdentityChanged { worker_instance_id });
            }
        }
    }
    writer
        .write(&HookResponse {
            ok: accepted,
            launch_identity_accepted,
            launch_identity_status,
        })
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))
}

/// Admits and applies one subagent snapshot, answering whether it was
/// admitted.
///
/// Admission matches a subagent start: schema, runtime generation, running
/// phase, task bounds, and the peer and ancestry of the reporting process.
/// An invalid task list is rejected whole. An admitted snapshot is reconciled
/// by [`reconcile_snapshot`]. The journal is persisted and `SubagentsChanged`
/// emitted only when a subagent was completed. The worker evidence, including
/// the reporter's last snapshot sequence, is updated for every admitted
/// snapshot, after that write when there is one.
///
/// # Errors
///
/// Returns an error when the runtime has no subagent ordering rule after
/// admission or the journal write fails.
async fn apply_subagent_snapshot(
    shared: &Shared,
    request: SnapshotRequest,
    peer: &PeerContext,
) -> Result<bool, WorkerError> {
    let occurred_at_ms = unix_ms();
    let mut guard = shared.state.lock().await;
    let state = &mut *guard;
    let current_runtime = state.worker_instance_id.as_ref().map(ToString::to_string);
    let admitted = admit_hook(
        state.hook_schema,
        HookAction::SubagentSnapshot,
        &request.provider,
        state.launch_agent_base.as_deref(),
    );
    let inadmissible = hook_claim_admissible(
        admitted.as_ref().err().copied(),
        current_runtime.as_deref() == Some(request.worker_instance_id.as_str()),
        state.phase == WireRuntimePhase::Running,
    )
    .or_else(|| {
        let valid = admitted.is_ok_and(HookSchema::admits_subagents)
            && request.tasks.len() <= MAX_SNAPSHOT_TASKS
            && request.tasks.iter().all(|task| {
                is_task_text(&task.id, MAX_SUBAGENT_ID_BYTES)
                    && is_task_text(&task.task_type, MAX_SNAPSHOT_TYPE_BYTES)
                    && is_task_text(&task.status, MAX_SNAPSHOT_STATUS_BYTES)
            });
        (!valid).then_some(RejectReason::SubagentClaimInvalid)
    });
    if let Some(reason) = inadmissible {
        reject_identity(shared, "subagent_snapshot", reason, peer);
        return Ok(false);
    }
    let process_valid = admitted.is_ok_and(|schema| {
        state.pty.as_ref().is_some_and(|pty| {
            hook_claim_valid(
                shared,
                "subagent_snapshot",
                peer,
                schema.ancestry,
                request.pid,
                request.start_identity,
                pty.identity().pid,
            )
        })
    });
    if !process_valid {
        return Ok(false);
    }
    admitted_subagent_rule(admitted)?;
    let candidates = background_candidates(shared, &request);
    let claim = SnapshotClaim {
        provider: &request.provider,
        reporter: Reporter {
            pid: request.pid,
            start_identity: request.start_identity,
        },
        sequence: request.sequence,
        tasks: &request.tasks,
    };
    let mut journal = state.journal.clone();
    let result = reconcile_snapshot(
        &mut journal,
        &state.subagent_evidence,
        claim,
        occurred_at_ms,
    );
    let reconciled = !result.completed.is_empty();

    // The background snapshot is replaced only by the active agent's report,
    // only in sequence order, and only within the item bound. Each refusal is
    // logged with its reason and leaves the stored snapshot untouched.
    let applied = apply_background(shared, &mut journal, &request, candidates, occurred_at_ms);
    let background_changed = applied.is_some();

    if reconciled || background_changed {
        journal.updated_at = timestamp();
        persist(shared.journal.clone(), journal.clone()).await?;
        state.journal = journal;
    }
    if reconciled {
        state.subagent_evidence.retain_recorded(&state.journal);
    }
    if let Some((revision, items, kinds)) = applied {
        log_background_applied(shared, revision, request.sequence, items, &kinds);
    }
    for id in &result.listed {
        state
            .subagent_evidence
            .record_listed(claim.provider, id, claim.sequence);
    }
    state
        .subagent_evidence
        .record_snapshot(claim.provider, claim.reporter, claim.sequence);
    log_snapshot(shared, claim, &result);
    let worker_instance_id = state.worker_instance_id.clone();
    drop(guard);
    if let (true, Some(worker_instance_id)) = (reconciled, worker_instance_id) {
        shared.emit(EventKind::SubagentsChanged { worker_instance_id });
    }
    Ok(true)
}

/// Checks the background fields of each task of `request`.
///
/// The optional fields it drops are logged by reason and count.
fn background_candidates(shared: &Shared, request: &SnapshotRequest) -> Vec<Candidate> {
    let mut dropped = Dropped::default();
    let candidates = request
        .tasks
        .iter()
        .map(|task| {
            Candidate::new(
                &task.id,
                &task.task_type,
                &task.status,
                task.label.as_ref(),
                task.program.as_ref(),
                task.agent_type.as_ref(),
                &mut dropped,
            )
        })
        .collect();
    if !dropped.is_empty() {
        log_background_fields_dropped(shared, dropped);
    }
    candidates
}

/// Why an admitted, process-valid identity report clears the background
/// snapshot, if it does.
///
/// Only a snapshot ordered before the report can be cleared. A report from the
/// snapshot's owner clears it when the owner starts or resumes its session. A
/// report from any other process clears it, whatever its source, because that
/// process did not report the snapshot. The owner's `clear` session keeps it,
/// since the same process starts the session again.
fn report_background_clear(
    journal: &JournalRecord,
    pid: u32,
    start_identity: u64,
    sequence: u64,
    session_source: Option<&str>,
) -> Option<&'static str> {
    let owner = journal.background_owner.as_ref()?;
    if journal.background_sequence >= sequence {
        return None;
    }
    let is_owner = owner.pid == pid && owner.start_identity == start_identity.to_string();
    if !is_owner {
        Some("agent_changed")
    } else if matches!(session_source, Some("startup" | "resume")) {
        Some("session_start")
    } else {
        None
    }
}

/// Replaces the background snapshot of `journal` with the one `request` reports.
///
/// The snapshot belongs to the process that reported it, so any admitted
/// report replaces the stored one: it must be newer than the applied mark and
/// within [`BACKGROUND_MAX_ITEMS`]. A refusal is logged with its reason and
/// leaves `journal` untouched. On success the sequence mark moves to
/// `request.sequence`, the reporter becomes the owner, and the returned tuple
/// holds the new revision, the item count and the kind summary. An item keeps
/// its `first_seen_ms` only when the same process reported it before.
fn apply_background(
    shared: &Shared,
    journal: &mut JournalRecord,
    request: &SnapshotRequest,
    candidates: Vec<Candidate>,
    occurred_at_ms: u64,
) -> Option<(u64, usize, String)> {
    // `active_identity.expires_at` is not checked here: it bounds a claim's
    // retries, while the snapshot belongs to the process that reported it.
    let refusal = if request.sequence <= journal.background_sequence {
        Some("stale_sequence")
    } else if request.tasks.len() > BACKGROUND_MAX_ITEMS {
        Some("too_many_items")
    } else {
        None
    };
    if let Some(reason) = refusal {
        log_background_ignored(shared, reason, request.sequence, request.tasks.len());
        return None;
    }
    let same_owner = journal.background_owner.as_ref().is_some_and(|owner| {
        owner.pid == request.pid && owner.start_identity == request.start_identity.to_string()
    });
    let previous = if same_owner {
        journal.background.as_ref()
    } else {
        None
    };
    let items = background::build_items(candidates, previous, occurred_at_ms);
    journal.background_revision = journal.background_revision.saturating_add(1);
    journal.background_sequence = background::clamp_mark(request.sequence);
    journal.background_owner = Some(BackgroundOwner {
        pid: request.pid,
        start_identity: request.start_identity.to_string(),
    });
    let kinds = background::kind_summary(&items);
    let applied = (journal.background_revision, items.len(), kinds);
    journal.background = Some(BackgroundSnapshot {
        revision: journal.background_revision,
        observed_at_ms: occurred_at_ms,
        items,
    });
    Some(applied)
}

/// Checks one hook operation against the session's hook schema with the rule
/// the daemon applies on the public socket.
fn admit_hook(
    schema: Option<&'static HookSchema>,
    action: HookAction,
    provider: &str,
    launch_runtime: Option<&str>,
) -> Result<&'static HookSchema, RejectReason> {
    protocol::admit_hook(schema, action, provider, launch_runtime).map_err(|denial| match denial {
        protocol::HookDenial::ActionNotAllowed => RejectReason::ActionNotAllowed,
        protocol::HookDenial::ProviderNotAllowed => RejectReason::ProviderNotAllowed,
    })
}

/// Whether a reported native reference has the shape `schema` admits: both
/// parts absent, or both present with an admitted kind.
fn native_reference_admitted(
    schema: &HookSchema,
    reference_kind: Option<&str>,
    native_reference: Option<&str>,
) -> bool {
    match (reference_kind, native_reference) {
        (None, None) => true,
        (Some(kind), Some(_)) => schema.admits_reference_kind(kind),
        _ => false,
    }
}

/// The ancestry rule of an admitted hook operation.
///
/// # Errors
///
/// Returns a protocol error when the operation was not admitted, which the
/// callers rule out before asking.
fn admitted_ancestry(
    admitted: Result<&'static HookSchema, RejectReason>,
) -> Result<AncestryMatcher, WorkerError> {
    admitted
        .map(|schema| schema.ancestry)
        .map_err(|reason| WorkerError::Protocol(format!("hook claim was not admitted: {reason}")))
}

/// The schema and subagent ordering rule of an admitted subagent claim.
///
/// # Errors
///
/// Returns a protocol error when the operation was not admitted or its schema
/// declares no ordering rule, which the callers rule out through
/// [`HookSchema::admits_subagents`] before asking.
fn admitted_subagent_rule(
    admitted: Result<&'static HookSchema, RejectReason>,
) -> Result<(&'static HookSchema, SubagentSequence), WorkerError> {
    let schema = admitted.map_err(|reason| {
        WorkerError::Protocol(format!("hook claim was not admitted: {reason}"))
    })?;
    let rule = schema.subagent_sequence.ok_or_else(|| {
        WorkerError::Protocol("hook schema declares no subagent ordering rule".to_owned())
    })?;
    Ok((schema, rule))
}

/// Launch-process rule of a session's hook schema, with the evidence it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchRule {
    /// [`LaunchProcess::NamedForProvider`].
    NamedForProvider,
    /// [`LaunchProcess::FrozenRootEntrypoint`] with the entrypoint frozen at
    /// `Initialize`, or `None` when it could not be frozen, which accepts no
    /// claim.
    FrozenRoot(Option<Box<FrozenEntrypoint>>),
}

impl LaunchRule {
    /// Stable code of the rule in structured logs.
    const fn code(&self) -> &'static str {
        match self {
            Self::NamedForProvider => "named_for_provider",
            Self::FrozenRoot(_) => "frozen_root_entrypoint",
        }
    }
}

/// Why a launch claim, or a later native reference of the launch process, was
/// refused.
///
/// The codes are the only refusal detail that reaches a log: no path, command
/// line, or native reference does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchRefusal {
    /// The worker froze no launch rule, so no claim can be verified.
    NoRuleFrozen,
    /// A report naming the frozen root came from a different kernel peer.
    NotRootPeer,
    /// The claim names a process other than the PTY root.
    NotRootProcess,
    /// The launch entrypoint could not be frozen at `Initialize`.
    NoFrozenEntrypoint,
    /// The root or the claimed process no longer exists.
    ProcessGone,
    /// The root or the claimed process is a different generation than claimed.
    GenerationChanged,
    /// The root's command line is not the frozen argv.
    CmdlineMismatch,
    /// The root's executable is not the frozen image.
    ImageMismatch,
    /// A frozen file changed since `Initialize`.
    EntrypointChanged,
    /// No process at or below the root is named for the provider.
    NoLaunchProcess,
    /// The claimed process is neither the launch process nor its provider
    /// hook-helper child.
    NotLaunchProcess,
    /// The process facts needed to verify a later reference could not be read.
    InspectionFailed,
    /// The launch identity is already set from another process.
    LaunchFixed,
    /// The same process's first claim was refused, and a later report never
    /// replaces it.
    RefusedBefore,
}

impl LaunchRefusal {
    /// Stable code used in structured logs.
    const fn code(self) -> &'static str {
        match self {
            Self::NoRuleFrozen => "no_rule_frozen",
            Self::NotRootPeer => "not_root_peer",
            Self::NotRootProcess => "not_root_process",
            Self::NoFrozenEntrypoint => "no_frozen_entrypoint",
            Self::ProcessGone => "process_gone",
            Self::GenerationChanged => "generation_changed",
            Self::CmdlineMismatch => "cmdline_mismatch",
            Self::ImageMismatch => "image_mismatch",
            Self::EntrypointChanged => "entrypoint_changed",
            Self::NoLaunchProcess => "no_launch_process",
            Self::NotLaunchProcess => "not_launch_process",
            Self::InspectionFailed => "inspection_failed",
            Self::LaunchFixed => "launch_fixed",
            Self::RefusedBefore => "refused_before",
        }
    }
}

/// Outcome of verifying one launch claim against the session's rule.
type LaunchVerdict = Result<(), LaunchRefusal>;

/// Records one refused launch claim or native reference update.
///
/// A refusal without a frozen rule is a worker defect, not a hostile or stale
/// report, so it is logged as an error.
fn reject_launch(shared: &Shared, rule: Option<&LaunchRule>, reason: LaunchRefusal, pid: u32) {
    if let Some(rule) = rule {
        event!(
            name: "worker.launch_identity.rejected",
            Level::WARN,
            worker.id = %shared.worker_id,
            session.id = %shared.session_id,
            launch.rule = rule.code(),
            launch.reason = reason.code(),
            process.pid = pid,
            "launch identity claim refused: {{launch.reason}}",
        );
    } else {
        event!(
            name: "worker.launch_identity.rejected",
            Level::ERROR,
            worker.id = %shared.worker_id,
            session.id = %shared.session_id,
            launch.rule = "none",
            launch.reason = reason.code(),
            process.pid = pid,
            "launch identity claim refused without a frozen launch rule: {{launch.reason}}",
        );
    }
}

/// Applies one identity report of the launch provider to the launch identity.
///
/// The report establishes the launch identity, keeps a pending claim, or
/// journals the launch process's later reference; every refusal is logged.
///
/// # Errors
///
/// Returns an error when the bounded pending-claim capacity is exhausted.
fn apply_launch_report(
    shared: &Shared,
    journal: &mut JournalRecord,
    claim: PendingLaunchClaim,
    reported: NativeReferenceClaim,
    verify: &mut impl FnMut(&PendingLaunchClaim) -> Result<bool, WorkerError>,
) -> Result<LaunchClaimStatus, WorkerError> {
    let rule = shared.launch_rule.get();
    let pid = claim.identity.process.pid;
    let frozen_root = matches!(rule, Some(LaunchRule::FrozenRoot(_)));
    if frozen_root && pid != claim.root.pid {
        // Only the root can ever qualify, so a descendant's claim is refused
        // without a pending entry that would use up the bounded capacity
        // before the root reports.
        reject_launch(shared, rule, LaunchRefusal::NotRootProcess, pid);
        return Ok(LaunchClaimStatus::Rejected);
    }
    let established = journal.launch_identity.is_some();
    let standing = standing_launch_refusal(journal, &claim);
    let update = claim.clone();
    let mut status = launch::submit(
        journal,
        claim,
        shared.config.pending_launch_claims,
        |claim| {
            let result = verify(claim);
            if let Err(error) = &result {
                event!(name: "worker.launch_identity.inspect.deferred", Level::WARN,
                    process.pid = pid, error.type = "process_observation", error.message = %error,
                    "launch identity verification deferred: {{error.message}}");
            }
            result
        },
    )?;
    if let Some(reason) = standing {
        reject_launch(shared, rule, reason, pid);
    }
    // The verified launch process's latest reference is journaled apart from
    // the lease-bound active claim. Under the frozen-root rule it is journaled
    // only while the root still runs the frozen entrypoint, which an exec in
    // place would otherwise escape. A claim that establishes the launch
    // identity journals its own reference inside `submit`.
    if established
        && journal
            .launch_identity
            .as_ref()
            .is_some_and(|launch| launch.process == reported.process)
    {
        let verdict = if frozen_root {
            verify(&update)
        } else {
            Ok(true)
        };
        match verdict {
            Ok(true) => journal.native_reference_claim = Some(reported),
            Ok(false) => status = LaunchClaimStatus::Rejected,
            // An update is never retried, so an unverifiable one is refused.
            Err(_error) => {
                reject_launch(shared, rule, LaunchRefusal::InspectionFailed, pid);
                status = LaunchClaimStatus::Rejected;
            }
        }
    }
    Ok(status)
}

/// Why [`launch::submit`] refuses `claim` from the journal alone, without
/// verifying it.
///
/// The launch identity is immutable once set, and a process's first claim is
/// never replaced, so neither refusal depends on the process facts.
fn standing_launch_refusal(
    journal: &JournalRecord,
    claim: &PendingLaunchClaim,
) -> Option<LaunchRefusal> {
    match journal.launch_identity.as_ref() {
        Some(launch) => {
            (launch.process != claim.identity.process).then_some(LaunchRefusal::LaunchFixed)
        }
        None => journal
            .pending_launch_claims
            .iter()
            .any(|pending| {
                !pending.retry_pending
                    && pending.identity.process == claim.identity.process
                    && pending.identity.provider == claim.identity.provider
            })
            .then_some(LaunchRefusal::RefusedBefore),
    }
}

/// Whether a report naming the PTY root comes from the root itself, when the
/// session's rule designates the frozen root.
///
/// Every descendant inherits the worker socket path and instance id, and the
/// root reports in process, so under [`LaunchProcess::FrozenRootEntrypoint`]
/// only a kernel peer that is the root may speak for it. Refusing the whole
/// report keeps a descendant from setting the root's launch identity, its
/// native reference, or the claim ordering the root's own reports must pass.
fn root_report_from_root(
    shared: &Shared,
    peer: &PeerContext,
    pid: u32,
    root: &ProcessIdentity,
) -> bool {
    let rule = shared.launch_rule.get();
    if pid != root.pid || !matches!(rule, Some(LaunchRule::FrozenRoot(_))) {
        return true;
    }
    let peer = peer.identity();
    if peer.pid == root.pid && peer.start_identity.to_string() == root.start_identity {
        return true;
    }
    reject_launch(shared, rule, LaunchRefusal::NotRootPeer, pid);
    false
}

/// Process image the worker launched, frozen at `Initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FrozenEntrypoint {
    /// Exact command line the root process runs with.
    argv: Vec<String>,
    /// The launched executable; a script when `interpreter` is set.
    entrypoint: FileIdentity,
    /// The absolute shebang interpreter the kernel runs the script with.
    interpreter: Option<FileIdentity>,
}

impl FrozenEntrypoint {
    /// The file the root process image must be.
    fn image(&self) -> &FileIdentity {
        self.interpreter.as_ref().unwrap_or(&self.entrypoint)
    }

    /// Whether every frozen file still has its frozen identity.
    fn unchanged(&self) -> bool {
        file_identity(&self.entrypoint.requested_path, true).as_ref() == Some(&self.entrypoint)
            && self.interpreter.as_ref().is_none_or(|interpreter| {
                file_identity(&interpreter.requested_path, false).as_ref() == Some(interpreter)
            })
    }
}

/// Identity of one file, taken so that a swap during the snapshot fails it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    /// The path as launched or named by the shebang.
    requested_path: PathBuf,
    /// The fully resolved path the kernel executes.
    canonical_path: PathBuf,
    /// Stamp and target of `requested_path` when it is a symbolic link.
    link: Option<(FileStamp, PathBuf)>,
    /// Stamp of the resolved regular file.
    target: FileStamp,
    /// SHA-256 of a script's contents; `None` for a file read as a binary.
    content_digest: Option<[u8; 32]>,
}

/// Inode metadata that changes when a file is replaced or rewritten.
///
/// The modification time can be set by the file's owner, but the status-change
/// time cannot: the kernel advances it on every content, metadata, or rename
/// change, so an in-place rewrite never keeps an equal stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    device: u64,
    inode: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileStamp {
    fn new(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// Why a launch entrypoint file could not be frozen.
///
/// The codes, and the I/O error kind, are the only freeze detail that reaches
/// a log: no path or file content does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FreezeReason {
    /// The launched executable path is not UTF-8, so no command line matches.
    PathNotUtf8,
    /// The file could not be inspected or read.
    Unreadable,
    /// The path does not resolve to a regular file.
    NotRegularFile,
    /// The script is larger than [`MAX_ENTRYPOINT_SCRIPT_BYTES`].
    ScriptTooLarge,
    /// The file or its link changed while it was snapshotted.
    ChangedWhileReading,
    /// The script's first line is not exactly `#!<absolute path>`.
    UnsupportedShebang,
}

impl FreezeReason {
    /// Stable code used in structured logs.
    const fn code(self) -> &'static str {
        match self {
            Self::PathNotUtf8 => "path_not_utf8",
            Self::Unreadable => "unreadable",
            Self::NotRegularFile => "not_regular_file",
            Self::ScriptTooLarge => "script_too_large",
            Self::ChangedWhileReading => "changed_while_reading",
            Self::UnsupportedShebang => "unsupported_shebang",
        }
    }
}

/// Why one file could not be frozen, with the I/O error kind when an
/// operating-system call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FreezeError {
    reason: FreezeReason,
    io_kind: Option<std::io::ErrorKind>,
}

impl FreezeError {
    const fn new(reason: FreezeReason) -> Self {
        Self {
            reason,
            io_kind: None,
        }
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "consumes the error at the `map_err` call sites"
    )]
    fn io(error: std::io::Error) -> Self {
        Self {
            reason: FreezeReason::Unreadable,
            io_kind: Some(error.kind()),
        }
    }
}

/// Why `initialize`'s entrypoint could not be frozen, naming which frozen file
/// failed: `entrypoint` or `interpreter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FreezeFailure {
    file: &'static str,
    error: FreezeError,
}

/// Snapshots the file at `path`, returning its identity and, when
/// `read_script` is set and the file starts with `#!`, its contents.
///
/// # Errors
///
/// Fails when the file is not a regular file, cannot be read, is a script
/// larger than [`MAX_ENTRYPOINT_SCRIPT_BYTES`], or changes while it is read.
fn file_snapshot(
    path: &Path,
    read_script: bool,
) -> Result<(FileIdentity, Option<Vec<u8>>), FreezeError> {
    let changed = FreezeError::new(FreezeReason::ChangedWhileReading);
    let link_before = fs::symlink_metadata(path).map_err(FreezeError::io)?;
    let link_target_before = fs::read_link(path).ok();
    let canonical_before = fs::canonicalize(path).map_err(FreezeError::io)?;
    let target_before = fs::metadata(&canonical_before).map_err(FreezeError::io)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&canonical_before)
        .map_err(FreezeError::io)?;
    let opened_before = file.metadata().map_err(FreezeError::io)?;
    if !opened_before.file_type().is_file() {
        return Err(FreezeError::new(FreezeReason::NotRegularFile));
    }
    if FileStamp::new(&target_before) != FileStamp::new(&opened_before) {
        return Err(changed);
    }
    let mut script = None;
    if read_script {
        let too_large = FreezeError::new(FreezeReason::ScriptTooLarge);
        let mut prefix = [0_u8; 2];
        let prefix_len = file.read(&mut prefix).map_err(FreezeError::io)?;
        if prefix_len == prefix.len() && &prefix == b"#!" {
            if opened_before.len() > MAX_ENTRYPOINT_SCRIPT_BYTES as u64 {
                return Err(too_large);
            }
            let mut contents = Vec::new();
            file.seek(SeekFrom::Start(0)).map_err(FreezeError::io)?;
            file.by_ref()
                .take((MAX_ENTRYPOINT_SCRIPT_BYTES + 1) as u64)
                .read_to_end(&mut contents)
                .map_err(FreezeError::io)?;
            if contents.len() > MAX_ENTRYPOINT_SCRIPT_BYTES {
                return Err(too_large);
            }
            script = Some(contents);
        }
    }
    let target_after = file.metadata().map_err(FreezeError::io)?;
    let canonical_after = fs::canonicalize(path).map_err(FreezeError::io)?;
    let link_after = fs::symlink_metadata(path).map_err(FreezeError::io)?;
    let link_target_after = fs::read_link(path).ok();
    if canonical_before != canonical_after
        || FileStamp::new(&link_before) != FileStamp::new(&link_after)
        || link_target_before != link_target_after
        || FileStamp::new(&target_before) != FileStamp::new(&target_after)
    {
        return Err(changed);
    }
    let link = if link_before.file_type().is_symlink() {
        // A link whose target cannot be read either time is unreadable.
        let target = link_target_before.ok_or(FreezeError::new(FreezeReason::Unreadable))?;
        Some((FileStamp::new(&link_before), target))
    } else {
        None
    };
    Ok((
        FileIdentity {
            requested_path: path.to_owned(),
            canonical_path: canonical_before,
            link,
            target: FileStamp::new(&target_after),
            content_digest: script
                .as_deref()
                .map(|contents| Sha256::digest(contents).into()),
        },
        script,
    ))
}

fn file_identity(path: &Path, read_script: bool) -> Option<FileIdentity> {
    file_snapshot(path, read_script)
        .ok()
        .map(|(identity, _script)| identity)
}

/// The absolute interpreter a script's shebang names, when the line is
/// exactly `#!<absolute path>` with no argument.
fn shebang_interpreter(script: &[u8]) -> Option<&str> {
    let line = script.split(|byte| *byte == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.strip_prefix("#!")?;
    let interpreter = line.strip_suffix('\r').unwrap_or(line);
    (Path::new(interpreter).is_absolute() && !interpreter.contains(char::is_whitespace))
        .then_some(interpreter)
}

/// Freezes the image `initialize` launches.
///
/// A native executable is frozen as itself. A script is frozen together with
/// the absolute, argument-free shebang interpreter that runs it, and the root
/// command line is the one the kernel builds for it: interpreter, script path,
/// then the launch arguments.
///
/// # Errors
///
/// Fails, naming the file and the reason, when either file cannot be pinned.
fn frozen_entrypoint(initialize: &Initialize) -> Result<FrozenEntrypoint, FreezeFailure> {
    let entrypoint_failure = |error| FreezeFailure {
        file: "entrypoint",
        error,
    };
    let launched = initialize
        .executable
        .to_str()
        .ok_or(entrypoint_failure(FreezeError::new(
            FreezeReason::PathNotUtf8,
        )))?;
    let (entrypoint, script) =
        file_snapshot(&initialize.executable, true).map_err(entrypoint_failure)?;
    let (interpreter, argv) = match script {
        None => {
            let mut argv = Vec::with_capacity(initialize.arguments.len() + 1);
            argv.push(launched.to_owned());
            argv.extend(initialize.arguments.iter().cloned());
            (None, argv)
        }
        Some(script) => {
            let interpreter = shebang_interpreter(&script).ok_or(entrypoint_failure(
                FreezeError::new(FreezeReason::UnsupportedShebang),
            ))?;
            let (identity, _) =
                file_snapshot(Path::new(interpreter), false).map_err(|error| FreezeFailure {
                    file: "interpreter",
                    error,
                })?;
            let mut argv = Vec::with_capacity(initialize.arguments.len() + 2);
            argv.push(interpreter.to_owned());
            argv.push(launched.to_owned());
            argv.extend(initialize.arguments.iter().cloned());
            (Some(identity), argv)
        }
    };
    Ok(FrozenEntrypoint {
        argv,
        entrypoint,
        interpreter,
    })
}

/// The launch rule `schema` declares, with the evidence it needs frozen from
/// `initialize`.
///
/// A frozen-root rule whose entrypoint cannot be frozen accepts no claim, and
/// the reason is logged once here.
fn launch_rule(
    shared: &Shared,
    schema: Option<&HookSchema>,
    initialize: &Initialize,
) -> LaunchRule {
    match schema.map_or(LaunchProcess::CORE, |schema| schema.launch_process) {
        LaunchProcess::NamedForProvider => LaunchRule::NamedForProvider,
        LaunchProcess::FrozenRootEntrypoint => match frozen_entrypoint(initialize) {
            Ok(entrypoint) => LaunchRule::FrozenRoot(Some(Box::new(entrypoint))),
            Err(failure) => {
                event!(
                    name: "worker.launch_identity.freeze.unavailable",
                    Level::WARN,
                    worker.id = %shared.worker_id,
                    session.id = %shared.session_id,
                    freeze.file = failure.file,
                    freeze.reason = failure.error.reason.code(),
                    io.kind = failure.error.io_kind.map_or_else(|| "none".to_owned(), |kind| format!("{kind:?}")),
                    "launch entrypoint could not be frozen: {{freeze.reason}}; no launch identity claim will be accepted",
                );
                LaunchRule::FrozenRoot(None)
            }
        },
    }
}

/// Verifies `claim` against the session's frozen launch rule and logs a
/// refusal.
///
/// A worker that froze no rule refuses every claim rather than fall back to a
/// looser rule.
fn verify_launch_claim(shared: &Shared, claim: &PendingLaunchClaim) -> Result<bool, WorkerError> {
    let pid = claim.identity.process.pid;
    let Some(rule) = shared.launch_rule.get() else {
        reject_launch(shared, None, LaunchRefusal::NoRuleFrozen, pid);
        return Ok(false);
    };
    let verdict = verify_launch_claim_with_rule(&HostInspector::new(), claim, rule)?;
    if let Err(reason) = verdict {
        reject_launch(shared, Some(rule), reason, pid);
    }
    Ok(verdict.is_ok())
}

#[cfg(test)]
fn verify_launch_claim_with(
    inspector: &dyn ProcessInspector,
    claim: &PendingLaunchClaim,
) -> Result<bool, WorkerError> {
    verify_launch_claim_with_rule(inspector, claim, &LaunchRule::NamedForProvider)
        .map(|verdict| verdict.is_ok())
}

fn verify_launch_claim_with_rule(
    inspector: &dyn ProcessInspector,
    claim: &PendingLaunchClaim,
    rule: &LaunchRule,
) -> Result<LaunchVerdict, WorkerError> {
    match rule {
        LaunchRule::NamedForProvider => verify_named_launch_claim(inspector, claim),
        LaunchRule::FrozenRoot(entrypoint) => {
            verify_frozen_root_claim(inspector, claim, entrypoint.as_deref())
        }
    }
}

/// The claim's root and reporting process, when both still have the exact
/// generation the claim recorded.
fn live_claim_processes(
    inspector: &dyn ProcessInspector,
    claim: &PendingLaunchClaim,
) -> Result<Result<(OsProcessIdentity, OsProcessIdentity), LaunchRefusal>, WorkerError> {
    let exact_identity = |process: &ChildIdentity| -> Result<OsProcessIdentity, WorkerError> {
        Ok(OsProcessIdentity {
            pid: process.pid,
            start_identity: process
                .start_identity
                .parse::<StartIdentity>()
                .map_err(|error| WorkerError::Protocol(error.to_string()))?,
        })
    };
    let root = exact_identity(&claim.root)?;
    let candidate = exact_identity(&claim.identity.process)?;
    for expected in [root, candidate] {
        match inspector.identity(expected.pid) {
            Ok(Some(current)) if current == expected => {}
            Ok(None) => return Ok(Err(LaunchRefusal::ProcessGone)),
            Ok(Some(_)) | Err(pohunek_platform::process::Error::Race { .. }) => {
                return Ok(Err(LaunchRefusal::GenerationChanged));
            }
            Err(error) => return Err(WorkerError::Protocol(error.to_string())),
        }
    }
    Ok(Ok((root, candidate)))
}

/// Verifies a claim under [`LaunchProcess::NamedForProvider`].
fn verify_named_launch_claim(
    inspector: &dyn ProcessInspector,
    claim: &PendingLaunchClaim,
) -> Result<LaunchVerdict, WorkerError> {
    let (root, candidate) = match live_claim_processes(inspector, claim)? {
        Ok(processes) => processes,
        Err(reason) => return Ok(Err(reason)),
    };
    // This probe also rechecks root/candidate generations after executable and
    // ancestry inspection. The stored PID alone never authorizes a retry.
    let provider = &claim.identity.provider;
    let Some(designated) = designated_launch_process_with(inspector, root, provider)? else {
        return Ok(Err(LaunchRefusal::NoLaunchProcess));
    };
    if designated.pid == candidate.pid
        && designated.start_identity.to_string() == candidate.start_identity.to_string()
    {
        return Ok(Ok(()));
    }
    Ok(
        if is_provider_child_of_launch(inspector, candidate, &designated, provider)? {
            Ok(())
        } else {
            Err(LaunchRefusal::NotLaunchProcess)
        },
    )
}

/// Verifies a claim under [`LaunchProcess::FrozenRootEntrypoint`].
///
/// Only the PTY root qualifies, and only while its command line is the frozen
/// argv, its executable is the frozen image, and the frozen files are
/// unchanged. Without a frozen entrypoint no claim qualifies. That the kernel
/// peer filing the report is the root itself is checked when the report is
/// received, by [`root_report_from_root`].
fn verify_frozen_root_claim(
    inspector: &dyn ProcessInspector,
    claim: &PendingLaunchClaim,
    entrypoint: Option<&FrozenEntrypoint>,
) -> Result<LaunchVerdict, WorkerError> {
    let inspection =
        |error: pohunek_platform::process::Error| WorkerError::Protocol(error.to_string());
    // A non-root claim is refused before any process or file inspection.
    if claim.identity.process.pid != claim.root.pid {
        return Ok(Err(LaunchRefusal::NotRootProcess));
    }
    let Some(entrypoint) = entrypoint else {
        return Ok(Err(LaunchRefusal::NoFrozenEntrypoint));
    };
    let (root, candidate) = match live_claim_processes(inspector, claim)? {
        Ok(processes) => processes,
        Err(reason) => return Ok(Err(reason)),
    };
    if candidate != root {
        return Ok(Err(LaunchRefusal::NotRootProcess));
    }
    let Some(fact) = inspector.process(root.pid).map_err(inspection)? else {
        return Ok(Err(LaunchRefusal::ProcessGone));
    };
    if fact.identity() != root {
        return Ok(Err(LaunchRefusal::GenerationChanged));
    }
    if fact.cmdline != entrypoint.argv {
        return Ok(Err(LaunchRefusal::CmdlineMismatch));
    }
    let executable = inspector.executable(root.pid).map_err(inspection)?;
    if executable.as_deref() != Some(entrypoint.image().canonical_path.as_path()) {
        return Ok(Err(LaunchRefusal::ImageMismatch));
    }
    if !entrypoint.unchanged() {
        return Ok(Err(LaunchRefusal::EntrypointChanged));
    }
    // The generation is rechecked last, so a root that exited and whose PID
    // was reused during inspection fails closed.
    Ok(match inspector.identity(root.pid).map_err(inspection)? {
        Some(current) if current == root => Ok(()),
        Some(_) => Err(LaunchRefusal::GenerationChanged),
        None => Err(LaunchRefusal::ProcessGone),
    })
}

async fn retry_launch_claims(
    shared: &Shared,
    mut verify: impl FnMut(&PendingLaunchClaim) -> Result<bool, WorkerError>,
) -> Result<(), WorkerError> {
    let mut state = shared.state.lock().await;
    if !state
        .journal
        .pending_launch_claims
        .iter()
        .any(|claim| claim.retry_pending)
    {
        return Ok(());
    }
    let running = state.phase == WireRuntimePhase::Running;
    let mut journal = state.journal.clone();
    let launch_was_accepted = journal.launch_identity.is_some();
    let changed = launch::retry(&mut journal, time::OffsetDateTime::now_utc(), |claim| {
        if running {
            verify(claim)
        } else {
            Ok(false)
        }
    });
    if changed {
        let launch_became_accepted = !launch_was_accepted && journal.launch_identity.is_some();
        journal.updated_at = timestamp();
        // All journal writers share this lock: a stale output/terminal snapshot
        // cannot overwrite ownership or a subsequently verified launch identity.
        persist(shared.journal.clone(), journal.clone()).await?;
        state.journal = journal;
        if let (true, Some(worker_instance_id)) =
            (launch_became_accepted, state.worker_instance_id.clone())
        {
            shared.emit(EventKind::IdentityChanged { worker_instance_id });
        }
        event!(name: "worker.launch_identity.retry.resolved", Level::INFO,
            launch.accepted = state.journal.launch_identity.is_some(),
            "deferred launch identity verification updated");
    }
    Ok(())
}

fn event_visible_to_connection(event: &protocol::ControlEvent, connection: &Connection) -> bool {
    !matches!(&event.kind, EventKind::SubagentsChanged { .. })
        || connection
            .capabilities
            .contains(&Capability::SubagentObservation)
}

/// One admissible subagent claim, as the ordering rule and the logs see it.
#[derive(Debug, Clone, Copy)]
struct SubagentClaim<'a> {
    /// `start` or `stop`, as logged in `subagent.action`.
    action: &'static str,
    provider: &'a str,
    id: &'a str,
    sequence: u64,
}

/// Why an admissible subagent claim left the subagent's public state
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentReason {
    /// A start ordered after the recorded claim of a subagent that is already
    /// running.
    DuplicateStart,
    /// A stop with no recorded start from a provider whose stop requires one.
    StopWithoutStart,
    /// A claim not ordered after the claim recorded for the same subagent.
    StaleSequence,
    /// A stop ordered after the recorded claim of a subagent that is already
    /// terminal.
    NotRunning,
    /// A start that would exceed [`ACTIVE_SUBAGENT_CAPACITY`].
    Capacity,
}

impl SubagentReason {
    /// Stable reason code used in logs.
    const fn code(self) -> &'static str {
        match self {
            Self::DuplicateStart => "duplicate_start",
            Self::StopWithoutStart => "stop_without_start",
            Self::StaleSequence => "stale_sequence",
            Self::NotRunning => "not_running",
            Self::Capacity => "capacity",
        }
    }
}

/// Effect of one admissible subagent claim on the journal.
///
/// A record's phase is the phase implied by the highest-sequence claim the
/// worker admitted for its subagent, so a record's sequence is the highest
/// sequence of any admitted claim for it, including claims that leave its
/// phase unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentOutcome {
    /// The claim changed the subagent's public state; the journal is persisted
    /// and the change announced.
    Applied,
    /// The claim leaves the subagent's public state unchanged by its order or
    /// because it repeats the recorded phase; the hook still succeeds.
    /// `recorded` is the sequence the claim was ordered against, when there is
    /// one. `advanced` is set when the claim is ordered after that sequence and
    /// the record's sequence was raised to the claim's: the journal is then
    /// persisted, with no revision change and no announcement, so a claim of
    /// the same run delivered later is ordered against it. This applies to
    /// every provider, so each repeated Codex stop of a finished child is one
    /// journal write.
    Ignored {
        reason: SubagentReason,
        recorded: Option<u64>,
        advanced: bool,
    },
    /// The claim would exceed a bound; the hook fails. `recorded` is as for
    /// [`Self::Ignored`].
    Refused {
        reason: SubagentReason,
        recorded: Option<u64>,
    },
}

impl SubagentOutcome {
    /// Whether the hook answers `ok`: the claim was applied or ignored.
    const fn acknowledged(self) -> bool {
        !matches!(self, Self::Refused { .. })
    }

    /// Whether the journal changed and must be persisted before the answer.
    const fn persists(self) -> bool {
        matches!(self, Self::Applied | Self::Ignored { advanced: true, .. })
    }

    /// Why the public state is unchanged, or `None` for an applied claim.
    const fn reason(self) -> Option<SubagentReason> {
        match self {
            Self::Applied => None,
            Self::Ignored { reason, .. } | Self::Refused { reason, .. } => Some(reason),
        }
    }
}

/// One stop remembered by [`OrphanStops`].
#[derive(Debug)]
struct OrphanStop {
    provider: String,
    id: String,
    sequence: u64,
}

/// Subagent stops with no recorded start from providers whose stop requires
/// one.
///
/// Such a stop creates no journal record. Its sequence is kept so that a start
/// delivered after it, out of order, is not taken for a new run. The set is in
/// memory only and evicts its oldest stop beyond [`ORPHAN_STOP_CAPACITY`].
#[derive(Debug, Default)]
struct OrphanStops {
    entries: VecDeque<OrphanStop>,
}

impl OrphanStops {
    /// Sequence of the remembered stop of `provider`'s subagent `id`.
    fn sequence(&self, provider: &str, id: &str) -> Option<u64> {
        self.entries
            .iter()
            .find(|entry| entry.provider == provider && entry.id == id)
            .map(|entry| entry.sequence)
    }

    /// Remembers a stop, keeping the greatest sequence of a repeated one.
    fn record(&mut self, provider: &str, id: &str, sequence: u64) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.provider == provider && entry.id == id)
        {
            entry.sequence = entry.sequence.max(sequence);
            return;
        }
        if self.entries.len() >= ORPHAN_STOP_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(OrphanStop {
            provider: provider.to_owned(),
            id: id.to_owned(),
            sequence,
        });
    }

    /// Drops the remembered stop of `provider`'s subagent `id`, if any.
    fn forget(&mut self, provider: &str, id: &str) {
        self.entries
            .retain(|entry| entry.provider != provider || entry.id != id);
    }
}

/// The agent process that reported a hook claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reporter {
    pid: u32,
    start_identity: u64,
}

/// Newest accepted identity claim or release sequence of each reporting
/// process that may still report.
///
/// A mark is never evicted while its process may still report, because a
/// forgotten mark would admit that process's own older claim again, for
/// example after it released. A process that exited, or whose pid now names a
/// different start identity, can never pass the process checks again, so its
/// mark is pruned when room is needed. The PTY root and the launch process are
/// pinned and never count against the bound. When the unpinned marks of live
/// processes fill the bound, a report from a further process is refused
/// instead (see [`admit_reporter`]).
///
/// The marks are kept in worker memory only: a worker process bootstraps its
/// own journal and serves one runtime, so they never need to outlive it.
#[derive(Debug, Default)]
struct IdentityMarks {
    /// `(reporter, newest accepted sequence)`.
    marks: Vec<(Reporter, u64)>,
}

impl IdentityMarks {
    /// Newest accepted sequence of `reporter`, if it has a mark.
    fn get(&self, reporter: Reporter) -> Option<u64> {
        self.marks
            .iter()
            .find(|(process, _)| *process == reporter)
            .map(|(_, sequence)| *sequence)
    }

    /// Number of marks of processes `pinned` does not name.
    fn unpinned(&self, pinned: &[Reporter]) -> usize {
        self.marks
            .iter()
            .filter(|(process, _)| !pinned.contains(process))
            .count()
    }

    /// Whether a mark for `reporter` exists or fits within `capacity`
    /// unpinned marks, pruning the marks of exited processes when the bound
    /// is reached.
    fn make_room(
        &mut self,
        reporter: Reporter,
        pinned: &[Reporter],
        capacity: usize,
        exited: impl Fn(Reporter) -> bool,
    ) -> bool {
        if pinned.contains(&reporter) || self.get(reporter).is_some() {
            return true;
        }
        if self.unpinned(pinned) >= capacity {
            self.marks
                .retain(|(process, _)| pinned.contains(process) || !exited(*process));
        }
        self.unpinned(pinned) < capacity
    }

    /// Remembers an accepted claim or release of `reporter` at `sequence`,
    /// keeping its greatest sequence.
    ///
    /// Callers admit `reporter` with [`Self::make_room`] first, so the bound
    /// holds.
    fn record(&mut self, reporter: Reporter, sequence: u64) {
        match self
            .marks
            .iter_mut()
            .find(|(process, _)| *process == reporter)
        {
            Some((_, recorded)) => *recorded = (*recorded).max(sequence),
            None => self.marks.push((reporter, sequence)),
        }
    }
}

/// The processes whose identity marks are pinned: the PTY root and, once
/// established, the launch process.
fn identity_pins(root: &ProcessIdentity, launch: Option<&LaunchIdentity>) -> Vec<Reporter> {
    [
        Some((root.pid, root.start_identity.as_str())),
        launch.map(|launch| (launch.process.pid, launch.process.start_identity.as_str())),
    ]
    .into_iter()
    .flatten()
    .filter_map(|(pid, start_identity)| {
        start_identity.parse().ok().map(|start_identity| Reporter {
            pid,
            start_identity,
        })
    })
    .collect()
}

/// Whether `reporter` may be given an identity mark, logging a refusal.
///
/// A report from a process without a mark is refused with
/// [`RejectReason::IdentityReportersExhausted`] when the marks of live,
/// unpinned processes already fill the configured bound. Refusing keeps every
/// live process's order intact, where evicting a mark would not.
fn admit_reporter(
    shared: &Shared,
    marks: &mut IdentityMarks,
    reporter: Reporter,
    pinned: &[Reporter],
    peer: &PeerContext,
) -> bool {
    let admitted = marks.make_room(
        reporter,
        pinned,
        shared.config.identity_reporters,
        reporter_exited,
    );
    if !admitted {
        reject_identity(
            shared,
            "identity_reporter",
            RejectReason::IdentityReportersExhausted,
            peer,
        );
    }
    admitted
}

/// Whether `reporter` has exited, or its pid names a different process.
///
/// A failed inspection counts as alive, so an uncertain answer keeps the mark.
fn reporter_exited(reporter: Reporter) -> bool {
    match HostInspector::new().identity(reporter.pid) {
        Ok(Some(identity)) => identity.start_identity.get() != reporter.start_identity,
        Ok(None) => true,
        Err(_error) => false,
    }
}

/// Unjournaled evidence about one journaled subagent.
#[derive(Debug)]
struct SubagentWitness {
    provider: String,
    id: String,
    /// Process whose start claim opened, reopened, or advanced the current
    /// run.
    reporter: Reporter,
    /// Greatest sequence of a snapshot that listed the subagent.
    listed_at: Option<u64>,
}

/// Worker-memory evidence that subagent snapshots reconcile against.
///
/// A subagent entry exists only for a journaled subagent whose current run
/// was opened, reopened, or advanced by a start admitted in this worker
/// process, and is dropped with its record. A subagent without an entry is
/// never reconciled, which keeps records recovered from an earlier process or
/// recorded by a stop alone out of reach of a snapshot. Those entries stay
/// within the journal's subagent bounds.
///
/// The greatest snapshot sequence of each reporting process is kept beside
/// them, for at most [`SNAPSHOT_REPORTER_CAPACITY`] reporters, because it
/// bounds from below when the reporter's next listing was built.
#[derive(Debug, Default)]
struct SubagentEvidence {
    entries: Vec<SubagentWitness>,
    /// `(provider, reporter, sequence)`, least recently reporting first.
    snapshots: VecDeque<(String, Reporter, u64)>,
}

impl SubagentEvidence {
    /// Evidence about `provider`'s subagent `id`, if any.
    fn get(&self, provider: &str, id: &str) -> Option<&SubagentWitness> {
        self.entries
            .iter()
            .find(|entry| entry.provider == provider && entry.id == id)
    }

    /// Records `reporter` as the process whose start opened, reopened, or
    /// advanced the subagent's current run.
    fn record_start(&mut self, provider: &str, id: &str, reporter: Reporter) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.provider == provider && entry.id == id)
        {
            entry.reporter = reporter;
            return;
        }
        self.entries.push(SubagentWitness {
            provider: provider.to_owned(),
            id: id.to_owned(),
            reporter,
            listed_at: None,
        });
    }

    /// Records that the snapshot at `sequence` listed the subagent.
    ///
    /// The greatest listing sequence is kept, so a snapshot delivered out of
    /// order cannot lower it. A subagent without evidence stays without it.
    fn record_listed(&mut self, provider: &str, id: &str, sequence: u64) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.provider == provider && entry.id == id)
        {
            entry.listed_at = Some(
                entry
                    .listed_at
                    .map_or(sequence, |listed| listed.max(sequence)),
            );
        }
    }

    /// Greatest snapshot sequence `reporter` reported for `provider`, if
    /// remembered.
    fn last_snapshot(&self, provider: &str, reporter: Reporter) -> Option<u64> {
        self.snapshots
            .iter()
            .find(|(known, process, _)| known == provider && *process == reporter)
            .map(|(_, _, sequence)| *sequence)
    }

    /// Remembers a snapshot of `reporter`, keeping its greatest sequence.
    ///
    /// The reporter becomes the most recent one; the least recent is
    /// forgotten beyond [`SNAPSHOT_REPORTER_CAPACITY`].
    fn record_snapshot(&mut self, provider: &str, reporter: Reporter, sequence: u64) {
        let position = self
            .snapshots
            .iter()
            .position(|(known, process, _)| known == provider && *process == reporter);
        let latest = match position.and_then(|index| self.snapshots.remove(index)) {
            Some((known, _, recorded)) => (known, reporter, recorded.max(sequence)),
            None => (provider.to_owned(), reporter, sequence),
        };
        if self.snapshots.len() >= SNAPSHOT_REPORTER_CAPACITY {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back(latest);
    }

    /// Drops the evidence of every subagent that has no record in `journal`.
    fn retain_recorded(&mut self, journal: &JournalRecord) {
        self.entries.retain(|entry| {
            journal
                .subagents
                .iter()
                .any(|record| record.provider == entry.provider && record.id == entry.id)
        });
    }
}

/// One admissible subagent snapshot, as reconciliation sees it.
#[derive(Debug, Clone, Copy)]
struct SnapshotClaim<'a> {
    provider: &'a str,
    reporter: Reporter,
    sequence: u64,
    tasks: &'a [SnapshotTask],
}

/// One running subagent that reconciliation completed.
#[derive(Debug)]
struct Reconciled {
    id: String,
    /// The subagent's own recorded sequence, which completion keeps.
    recorded: u64,
}

/// Effect of one subagent snapshot on the journal.
#[derive(Debug, Default)]
struct Reconciliation {
    /// Running subagents completed because the snapshot proves them absent,
    /// in journal order.
    completed: Vec<Reconciled>,
    /// Running subagents the snapshot listed; they stay running unchanged.
    listed: Vec<String>,
    /// Unlisted running subagents left running because their absence is not
    /// proven.
    deferred: usize,
    /// Whether the snapshot listed a task of a type that may host an agent.
    hosts: bool,
}

/// Reconciles `journal` with the snapshot `claim` taken at `occurred_at_ms`.
///
/// Claude builds a listing before its `Stop` hook reads the clock, so the
/// claim's sequence only bounds the listing from above, and a subagent's own
/// stop can precede a listing that still shows it running. The previous
/// snapshot of the same reporter, which `evidence` keeps, bounds the listing
/// from below, because the reporter's turns run one after another. `evidence`
/// keeps only the reporter's newest listing of each subagent and its newest
/// snapshot; when that evidence is not older than the claim, left by a
/// snapshot delivered out of order, it proves nothing about the claim's
/// listing and is not used. A snapshot therefore only completes running
/// subagents whose absence it proves; it never changes a terminal subagent
/// nor any record's sequence.
///
/// Only running subagents of the claim's provider whose current run
/// `evidence` ties to the claim's reporter, and whose recorded sequence is
/// less than the claim's, are considered. A listed one stays running. An
/// unlisted one is completed when the reporter's newest listing of it falls
/// within its current run and is older than the claim, because it was then a
/// known background agent whose listing preceded this one. An unlisted one
/// never listed in its run is completed only when no listed task may host an
/// agent and its recorded sequence is less than the reporter's newest
/// snapshot, itself older than the claim, which proves it started before the
/// listing was built; otherwise it is deferred, since Claude does not list a
/// teammate, the synchronous child of a background agent, or a workflow agent
/// by agent id.
///
/// A completed subagent keeps its recorded sequence, so a resuming start
/// ordered after it still reopens it, and takes a fresh revision; terminal
/// subagents beyond the retention bound are pruned after any completion.
fn reconcile_snapshot(
    journal: &mut JournalRecord,
    evidence: &SubagentEvidence,
    claim: SnapshotClaim<'_>,
    occurred_at_ms: u64,
) -> Reconciliation {
    let listed = claim
        .tasks
        .iter()
        .map(|task| task.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let previous = evidence
        .last_snapshot(claim.provider, claim.reporter)
        .filter(|previous| *previous < claim.sequence);
    let mut result = Reconciliation {
        hosts: claim
            .tasks
            .iter()
            .any(|task| !AGENTLESS_TASK_TYPES.contains(&task.task_type.as_str())),
        ..Reconciliation::default()
    };
    for index in 0..journal.subagents.len() {
        let record = &journal.subagents[index];
        if record.phase != JournalSubagentPhase::Running
            || record.provider != claim.provider
            || record.sequence >= claim.sequence
        {
            continue;
        }
        let Some(entry) = evidence
            .get(&record.provider, &record.id)
            .filter(|entry| entry.reporter == claim.reporter)
        else {
            continue;
        };
        if listed.contains(record.id.as_str()) {
            result.listed.push(record.id.clone());
            continue;
        }
        let known_background = entry
            .listed_at
            .is_some_and(|listed_at| listed_at >= record.sequence && listed_at < claim.sequence);
        let started_before_listing = previous.is_some_and(|previous| record.sequence < previous);
        if !known_background && (result.hosts || !started_before_listing) {
            result.deferred += 1;
            continue;
        }
        result.completed.push(Reconciled {
            id: record.id.clone(),
            recorded: record.sequence,
        });
        journal.subagent_revision = journal.subagent_revision.saturating_add(1);
        let record = &mut journal.subagents[index];
        record.phase = JournalSubagentPhase::Completed;
        record.revision = journal.subagent_revision;
        record.updated_at_ms = occurred_at_ms;
        record.finished_at_ms = Some(occurred_at_ms);
    }
    if !result.completed.is_empty() {
        prune_terminal_subagents(journal);
    }
    result
}

/// Records one applied subagent snapshot and every subagent it completed.
///
/// Each completed subagent logs `worker.subagent.reconciled` with the
/// snapshot's sequence as reported and the subagent's own as recorded, and
/// the snapshot logs one `worker.subagent.snapshot.applied` with its counts.
/// Task types, statuses, and the ids of unrecorded tasks are never logged.
fn log_snapshot(shared: &Shared, claim: SnapshotClaim<'_>, result: &Reconciliation) {
    for reconciled in &result.completed {
        event!(
            name: "worker.subagent.reconciled",
            Level::INFO,
            worker.id = %shared.worker_id,
            session.id = %shared.session_id,
            subagent.provider = claim.provider,
            subagent.id = reconciled.id.as_str(),
            subagent.reason = "snapshot_absent",
            subagent.sequence.reported = claim.sequence,
            subagent.sequence.recorded = reconciled.recorded,
            "subagent reconciled from snapshot: {{subagent.reason}}",
        );
    }
    event!(
        name: "worker.subagent.snapshot.applied",
        Level::INFO,
        worker.id = %shared.worker_id,
        session.id = %shared.session_id,
        subagent.provider = claim.provider,
        subagent.sequence.reported = claim.sequence,
        subagent.snapshot.tasks = claim.tasks.len(),
        subagent.snapshot.hosts = result.hosts,
        subagent.snapshot.listed = result.listed.len(),
        subagent.snapshot.terminalized = result.completed.len(),
        subagent.snapshot.deferred = result.deferred,
        "subagent snapshot applied: {{subagent.snapshot.terminalized}} completed, \
         {{subagent.snapshot.deferred}} deferred",
    );
}

/// Applies a subagent start to `journal` under `rule`.
///
/// A start ordered after a running subagent's recorded claim only raises the
/// record's sequence to its own, so a stop of the earlier run delivered later
/// is stale. A start ordered after a terminal subagent's recorded claim
/// reopens it as running, keeping its start time and any field the claim
/// omits. A start not ordered after the recorded claim changes nothing. A
/// start for an unknown subagent opens a new run unless `orphan_stop`, the
/// sequence of a remembered stop without a start, is not ordered before it.
/// Opening or reopening a run is refused once [`ACTIVE_SUBAGENT_CAPACITY`]
/// subagents are running.
fn start_subagent(
    journal: &mut JournalRecord,
    rule: SubagentSequence,
    claim: SubagentClaim<'_>,
    parent_id: Option<&str>,
    agent_type: Option<&str>,
    orphan_stop: Option<u64>,
    occurred_at_ms: u64,
) -> SubagentOutcome {
    let index = journal
        .subagents
        .iter()
        .position(|item| item.provider == claim.provider && item.id == claim.id);
    let recorded = match index {
        Some(index) => Some(journal.subagents[index].sequence),
        None => orphan_stop,
    };
    if !rule.admits_start(recorded, claim.sequence) {
        return SubagentOutcome::Ignored {
            reason: SubagentReason::StaleSequence,
            recorded,
            advanced: false,
        };
    }
    if let Some(index) = index {
        let record = &mut journal.subagents[index];
        if record.phase == JournalSubagentPhase::Running {
            record.sequence = claim.sequence;
            return SubagentOutcome::Ignored {
                reason: SubagentReason::DuplicateStart,
                recorded,
                advanced: true,
            };
        }
    }
    if journal
        .subagents
        .iter()
        .filter(|item| item.phase == JournalSubagentPhase::Running)
        .count()
        >= ACTIVE_SUBAGENT_CAPACITY
    {
        return SubagentOutcome::Refused {
            reason: SubagentReason::Capacity,
            recorded,
        };
    }
    journal.subagent_revision = journal.subagent_revision.saturating_add(1);
    if let Some(index) = index {
        let subagent = &mut journal.subagents[index];
        subagent.phase = JournalSubagentPhase::Running;
        subagent.sequence = claim.sequence;
        subagent.revision = journal.subagent_revision;
        subagent.updated_at_ms = occurred_at_ms;
        subagent.finished_at_ms = None;
        if let Some(parent_id) = parent_id {
            subagent.parent_id = Some(parent_id.to_owned());
        }
        if let Some(agent_type) = agent_type {
            subagent.agent_type = Some(agent_type.to_owned());
        }
    } else {
        journal.subagents.push(SubagentRecord {
            id: claim.id.to_owned(),
            parent_id: parent_id.map(str::to_owned),
            provider: claim.provider.to_owned(),
            agent_type: agent_type.map(str::to_owned),
            phase: JournalSubagentPhase::Running,
            sequence: claim.sequence,
            revision: journal.subagent_revision,
            started_at_ms: occurred_at_ms,
            updated_at_ms: occurred_at_ms,
            finished_at_ms: None,
        });
    }
    SubagentOutcome::Applied
}

/// Applies a subagent stop to `journal` under `rule`.
///
/// A stop ordered after a running subagent's recorded claim moves it to
/// `phase`. A stop ordered after a terminal subagent's recorded claim only
/// raises the record's sequence to its own, so a start of the earlier run
/// delivered later is stale. A stop not ordered after the recorded claim
/// changes nothing. A stop for an unknown subagent creates no record when
/// `requires_start` holds for its provider, and otherwise records a subagent
/// that is already terminal.
fn stop_subagent(
    journal: &mut JournalRecord,
    rule: SubagentSequence,
    requires_start: bool,
    claim: SubagentClaim<'_>,
    phase: JournalSubagentPhase,
    occurred_at_ms: u64,
) -> SubagentOutcome {
    let index = journal
        .subagents
        .iter()
        .position(|item| item.provider == claim.provider && item.id == claim.id);
    let recorded = index.map(|index| journal.subagents[index].sequence);
    if index.is_none() && requires_start {
        return SubagentOutcome::Ignored {
            reason: SubagentReason::StopWithoutStart,
            recorded,
            advanced: false,
        };
    }
    if !rule.admits_stop(recorded, claim.sequence) {
        return SubagentOutcome::Ignored {
            reason: SubagentReason::StaleSequence,
            recorded,
            advanced: false,
        };
    }
    if let Some(index) = index {
        let record = &mut journal.subagents[index];
        if record.phase != JournalSubagentPhase::Running {
            record.sequence = claim.sequence;
            return SubagentOutcome::Ignored {
                reason: SubagentReason::NotRunning,
                recorded,
                advanced: true,
            };
        }
    }
    journal.subagent_revision = journal.subagent_revision.saturating_add(1);
    if let Some(index) = index {
        let subagent = &mut journal.subagents[index];
        subagent.phase = phase;
        subagent.sequence = claim.sequence;
        subagent.revision = journal.subagent_revision;
        subagent.updated_at_ms = occurred_at_ms;
        subagent.finished_at_ms = Some(occurred_at_ms);
    } else {
        journal.subagents.push(SubagentRecord {
            id: claim.id.to_owned(),
            parent_id: None,
            provider: claim.provider.to_owned(),
            agent_type: None,
            phase,
            sequence: claim.sequence,
            revision: journal.subagent_revision,
            started_at_ms: occurred_at_ms,
            updated_at_ms: occurred_at_ms,
            finished_at_ms: Some(occurred_at_ms),
        });
    }
    prune_terminal_subagents(journal);
    SubagentOutcome::Applied
}

/// Logs one applied background snapshot by revision and counts per kind.
///
/// Ids, labels and programs are never logged.
fn log_background_applied(
    shared: &Shared,
    revision: u64,
    sequence: u64,
    items: usize,
    kinds: &str,
) {
    event!(
        name: "worker.background.applied",
        Level::INFO,
        worker.id = %shared.worker_id,
        session.id = %shared.session_id,
        background.revision = revision,
        background.sequence.reported = sequence,
        background.items = items,
        background.kinds = kinds,
        "background snapshot applied: revision {{background.revision}}, {{background.items}} items",
    );
}

/// Logs one background snapshot that was refused, with the reason it was.
fn log_background_ignored(shared: &Shared, reason: &'static str, sequence: u64, items: usize) {
    event!(
        name: "worker.background.ignored",
        Level::INFO,
        worker.id = %shared.worker_id,
        session.id = %shared.session_id,
        background.reason = reason,
        background.sequence.reported = sequence,
        background.items = items,
        "background snapshot ignored: {{background.reason}}",
    );
}

/// Logs the background snapshot dropped by a clear and why it was cleared.
fn log_background_cleared(shared: &Shared, reason: &'static str) {
    event!(
        name: "worker.background.cleared",
        Level::INFO,
        worker.id = %shared.worker_id,
        session.id = %shared.session_id,
        background.reason = reason,
        "background snapshot cleared: {{background.reason}}",
    );
}

/// Logs the optional snapshot fields the worker dropped, by reason, as counts.
fn log_background_fields_dropped(shared: &Shared, dropped: Dropped) {
    event!(
        name: "worker.background.fields_dropped",
        Level::INFO,
        worker.id = %shared.worker_id,
        session.id = %shared.session_id,
        background.dropped.invalid = dropped.invalid,
        background.dropped.kind_mismatch = dropped.kind_mismatch,
        "background fields dropped: {{background.dropped.invalid}} invalid, \
         {{background.dropped.kind_mismatch}} kind mismatch",
    );
}

/// Records one admissible subagent claim that left the public state unchanged.
///
/// The claim already passed schema admission and the process-ancestry check,
/// so its provider, subagent id, and sequences are logged with the reason
/// code. An ignored claim is routine provider ordering and logs at INFO; a
/// refused claim hit a bound and logs at WARN. An applied claim logs nothing.
fn log_subagent_outcome(shared: &Shared, claim: SubagentClaim<'_>, outcome: SubagentOutcome) {
    match outcome {
        SubagentOutcome::Applied => {}
        SubagentOutcome::Ignored {
            reason, recorded, ..
        } => event!(
            name: "worker.subagent.claim.ignored",
            Level::INFO,
            worker.id = %shared.worker_id,
            session.id = %shared.session_id,
            subagent.action = claim.action,
            subagent.provider = claim.provider,
            subagent.id = claim.id,
            subagent.reason = reason.code(),
            subagent.sequence.reported = claim.sequence,
            subagent.sequence.recorded = recorded,
            "subagent {{subagent.action}} claim ignored: {{subagent.reason}}",
        ),
        SubagentOutcome::Refused { reason, recorded } => event!(
            name: "worker.subagent.claim.refused",
            Level::WARN,
            worker.id = %shared.worker_id,
            session.id = %shared.session_id,
            subagent.action = claim.action,
            subagent.provider = claim.provider,
            subagent.id = claim.id,
            subagent.reason = reason.code(),
            subagent.sequence.reported = claim.sequence,
            subagent.sequence.recorded = recorded,
            "subagent {{subagent.action}} claim refused: {{subagent.reason}}",
        ),
    }
}

fn mark_running_subagents_lost(journal: &mut JournalRecord, occurred_at_ms: u64) {
    for subagent in journal
        .subagents
        .iter_mut()
        .filter(|item| item.phase == JournalSubagentPhase::Running)
    {
        journal.subagent_revision = journal.subagent_revision.saturating_add(1);
        subagent.phase = JournalSubagentPhase::Lost;
        subagent.revision = journal.subagent_revision;
        subagent.updated_at_ms = occurred_at_ms;
        subagent.finished_at_ms = Some(occurred_at_ms);
    }
    prune_terminal_subagents(journal);
}

fn prune_terminal_subagents(journal: &mut JournalRecord) {
    let mut terminal = journal
        .subagents
        .iter()
        .filter(|item| item.phase != JournalSubagentPhase::Running)
        .map(|item| item.revision)
        .collect::<Vec<_>>();
    terminal.sort_unstable_by(|left, right| right.cmp(left));
    let cutoff = terminal.get(TERMINAL_SUBAGENT_CAPACITY).copied();
    if let Some(cutoff) = cutoff {
        journal
            .subagents
            .retain(|item| item.phase == JournalSubagentPhase::Running || item.revision > cutoff);
    }
}

const fn wire_subagent_phase(phase: JournalSubagentPhase) -> WireSubagentPhase {
    match phase {
        JournalSubagentPhase::Running => WireSubagentPhase::Running,
        JournalSubagentPhase::Completed => WireSubagentPhase::Completed,
        JournalSubagentPhase::Failed => WireSubagentPhase::Failed,
        JournalSubagentPhase::Cancelled => WireSubagentPhase::Cancelled,
        JournalSubagentPhase::Lost => WireSubagentPhase::Lost,
    }
}

/// Whether an identity claim or release of `reporter` at `sequence` is newer
/// than every earlier accepted claim and release of that same process.
///
/// Reports of different processes are never ordered against each other, so no
/// process's sequence can make another process's report stale; the latest
/// accepted claim is the active identity. The process's mark is its remembered
/// one, which [`IdentityMarks`] keeps for as long as the process may report,
/// together with the journaled active identity, release, and launch reference
/// of that process.
fn identity_sequence_is_fresh(
    journal: &JournalRecord,
    marks: &IdentityMarks,
    reporter: Reporter,
    sequence: u64,
) -> bool {
    let journaled = [
        journal
            .active_identity
            .as_ref()
            .map(|active| (&active.process, active.sequence)),
        journal
            .active_identity_release
            .as_ref()
            .map(|release| (&release.process, release.sequence)),
        journal
            .native_reference_claim
            .as_ref()
            .map(|reference| (&reference.process, reference.sequence)),
    ];
    marks
        .get(reporter)
        .into_iter()
        .chain(
            journaled
                .into_iter()
                .flatten()
                .filter(|(process, _)| is_reporter(process, reporter))
                .map(|(_, mark)| mark),
        )
        .all(|mark| sequence > mark)
}

/// Whether the journaled `process` is the process `reporter`.
fn is_reporter(process: &ChildIdentity, reporter: Reporter) -> bool {
    process.pid == reporter.pid && process.start_identity == reporter.start_identity.to_string()
}

fn valid_identity_expiry(value: &str) -> bool {
    let Ok(expires_at) = time::OffsetDateTime::parse(value, &Rfc3339) else {
        return false;
    };
    let now = time::OffsetDateTime::now_utc();
    let max_expiry = now
        + time::Duration::seconds(
            i64::try_from(protocol::MAX_IDENTITY_CLAIM_TTL_SECS)
                .expect("identity TTL ceiling fits i64"),
        );
    expires_at > now && expires_at <= max_expiry
}

fn validate_hook_process(
    ancestry: AncestryMatcher,
    pid: u32,
    start_identity: u64,
    root_pid: u32,
) -> Result<(), WorkerError> {
    let related = process_start(pid)? == start_identity
        && ancestry.admits(PtyRelation::from_in_tree(is_descendant(pid, root_pid)?));
    if !related {
        return Err(WorkerError::Protocol(
            "identity hook process is outside the managed PTY tree".to_owned(),
        ));
    }
    Ok(())
}

/// Returns whether the connecting peer may speak for the reported process.
///
/// A report is accepted only from the subject itself or from a process it
/// spawned, which is the shape every shipped hook already has: Codex and Claude
/// run their hook as a child of the agent whose id they report, and the pinned
/// Hermes plugin reports from the agent process. Binding a claim to the process
/// that vouches for it keeps an unrelated command in the same terminal from
/// rewriting another process's identity, which matters because the PTY root's
/// environment carries the worker instance id and the private socket path to everything
/// the operator runs.
///
/// The ancestry walk reads live parent links, so the relationship holds only
/// while the chain is alive. A reporter whose intermediate parent has exited is
/// reparented out of the subject's subtree and is rejected, which is the
/// conservative direction and stays distinguishable through its own reason code.
///
/// The stricter `peer == subject` binding belongs to the explicit in-process
/// identity mode in <https://github.com/zajca/pohunek/issues/52>, which can
/// demand it on a wire shape of its own without breaking child hooks. A session
/// whose launch rule is the frozen root already demands it for a report naming
/// the PTY root, in [`root_report_from_root`].
fn peer_vouches_for(peer: &PeerContext, subject: u32) -> Result<bool, WorkerError> {
    let peer_pid = peer.identity().pid;
    if peer_pid == subject {
        return Ok(true);
    }
    is_descendant(peer_pid, subject)
}

/// Re-reads the kernel peer before one control request is dispatched.
///
/// Capture runs once per connection, so checking only at acquisition would let a
/// descriptor handed to another process keep issuing input, resize, stop, and
/// data-stream requests under the original peer's authority. Once a lease
/// exists the peer must also still be its owner, which is what stops an
/// inherited connection from holding a lease the real controller needs back.
///
/// Failing here ends the connection rather than answering an error, so the
/// caller releases the lease and a replacement daemon can recover.
fn authorize_control_request(shared: &Shared, connection: &Connection) -> Result<(), WorkerError> {
    let peer = connection.peer.as_ref();
    let drift = peer.reverify().err().or_else(|| {
        let identity = peer.identity();
        connection.owner.as_ref().and_then(|owner| {
            (owner.peer_pid != identity.pid
                || owner.peer_start_identity != identity.start_identity.to_string())
            .then_some(RejectReason::LeaseOwnerMismatch)
        })
    });
    match drift {
        Some(reason) => {
            reject_identity(shared, "control_request", reason, peer);
            Err(WorkerError::PeerIdentity { reason })
        }
        None => Ok(()),
    }
}

/// Re-reads the kernel peer while one data stream is still being served.
///
/// Redemption authorizes the stream once, but an attach stream then lives as
/// long as the lease. Without this, a descriptor handed to another process
/// after redemption could keep injecting input and keep receiving PTY bytes.
/// The check runs in both directions and fails the stream closed, which is why
/// it costs a kernel round trip per frame rather than a cached comparison.
fn authorize_data_frame(
    shared: &Shared,
    peer: &PeerContext,
    owner: &LeaseOwner,
) -> Result<(), WorkerError> {
    let drift = peer.reverify().err().or_else(|| {
        let identity = peer.identity();
        (owner.peer_pid != identity.pid
            || owner.peer_start_identity != identity.start_identity.to_string())
        .then_some(RejectReason::LeaseOwnerMismatch)
    });
    match drift {
        Some(reason) => {
            reject_identity(shared, "data_frame", reason, peer);
            Err(WorkerError::PeerIdentity { reason })
        }
        None => Ok(()),
    }
}

/// Returns why a hook claim is inadmissible before its process is inspected.
///
/// The schema verdict, runtime generation, and runtime phase are the rules
/// every hook variant shares. Reporting them as distinct codes is what lets
/// an operator tell a stale hook from a rejected one.
fn hook_claim_admissible(
    schema_rejection: Option<RejectReason>,
    runtime_matches: bool,
    phase_running: bool,
) -> Option<RejectReason> {
    if let Some(reason) = schema_rejection {
        return Some(reason);
    }
    if !runtime_matches {
        return Some(RejectReason::RuntimeMismatch);
    }
    if !phase_running {
        return Some(RejectReason::PhaseNotRunning);
    }
    None
}

/// Validates one hook claim end to end and records why it was rejected.
///
/// Combines the peer binding with the existing reported-process rules so every
/// hook variant applies both, and emits exactly one payload-free event naming
/// the stable reason.
fn hook_claim_valid(
    shared: &Shared,
    operation: &'static str,
    peer: &PeerContext,
    ancestry: AncestryMatcher,
    pid: u32,
    start_identity: u64,
    root_pid: u32,
) -> bool {
    if let Err(reason) = authorize_hook_report(peer, pid) {
        reject_identity(shared, operation, reason, peer);
        return false;
    }
    if validate_hook_process(ancestry, pid, start_identity, root_pid).is_err() {
        reject_identity(
            shared,
            operation,
            RejectReason::ReportedProcessInvalid,
            peer,
        );
        return false;
    }
    true
}

/// Rejects a hook whose peer cannot be bound to the process it reports.
///
/// Runs before any journal mutation, and re-reads the kernel answer first so a
/// peer that changed since the connection was accepted cannot slip through.
fn authorize_hook_report(peer: &PeerContext, subject: u32) -> Result<(), RejectReason> {
    peer.reverify()?;
    if peer_vouches_for(peer, subject).map_err(|_error| RejectReason::ReportedProcessInvalid)? {
        Ok(())
    } else {
        Err(RejectReason::PeerOutsideSubject)
    }
}

async fn scoped_pty(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
) -> Result<PtyOwner, ControlError> {
    validate_scope(shared, connection, scope).await?;
    shared
        .state
        .lock()
        .await
        .pty
        .clone()
        .ok_or_else(|| invalid_state("runtime has no PTY"))
}

async fn validate_scope(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
) -> Result<(), ControlError> {
    validate_lease(shared, connection, &scope.lease_id)?;
    let state = shared.state.lock().await;
    if scope.session_id != shared.session_id
        || scope.worker_id != shared.worker_id
        || state.worker_instance_id.as_ref() != Some(&scope.worker_instance_id)
    {
        return Err(identity_mismatch());
    }
    Ok(())
}

fn validate_lease(
    shared: &Shared,
    connection: &Connection,
    lease_id: &LeaseId,
) -> Result<(), ControlError> {
    let owner = connection.owner.as_ref().ok_or_else(identity_mismatch)?;
    if connection.lease_id.as_ref() != Some(lease_id) {
        return Err(identity_mismatch());
    }
    shared
        .lease
        .validate(owner, lease_id.as_str())
        .map_err(lease_control_error)
}

/// Builds the PTY child command from a validated `Initialize`.
///
/// From protocol version six the child environment starts empty and is layered
/// as base environment, worker `TERM`, profile environment, then the worker's
/// authoritative `POHUNEK_*` identity. A version-five `Initialize` has no base
/// environment, so the child inherits the worker's sanitized environment
/// instead. Service-manager variables are removed from the result either way.
fn command_from_initialize(
    shared: &Shared,
    initialize: &Initialize,
    worker_instance_id: &WorkerInstanceId,
) -> Command {
    let mut environment = BTreeMap::new();
    let base = match &initialize.base_environment {
        Some(base_environment) => {
            environment.extend(
                base_environment
                    .iter()
                    .map(|(name, value)| (name.to_owned(), value.to_owned())),
            );
            environment.insert("TERM".to_owned(), CHILD_TERM.to_owned());
            EnvBase::Empty
        }
        None => EnvBase::Inherited,
    };
    environment.extend(
        initialize
            .environment
            .iter()
            .filter(|(name, _)| !name.starts_with("POHUNEK_"))
            .map(|(name, value)| (name.to_owned(), value.to_owned())),
    );
    let mut reserved = BTreeMap::from([
        ("POHUNEK_ENV".to_owned(), "1".to_owned()),
        (
            "POHUNEK_SESSION_ID".to_owned(),
            shared.session_id.to_string(),
        ),
        ("POHUNEK_WORKER_ID".to_owned(), shared.worker_id.to_string()),
        (
            "POHUNEK_WORKER_INSTANCE_ID".to_owned(),
            worker_instance_id.to_string(),
        ),
        (
            "POHUNEK_WORKER_SOCKET_PATH".to_owned(),
            shared.socket_path.to_string_lossy().into_owned(),
        ),
        (
            "POHUNEK_WORKER_HOOK_PROTOCOL_VERSION".to_owned(),
            initialize.hook_protocol_version.to_string(),
        ),
        (
            "POHUNEK_SOCKET_PATH".to_owned(),
            shared.daemon_socket_path.to_string_lossy().into_owned(),
        ),
        (
            // The public daemon RPC protocol version, required by the real
            // provider hooks (`pohunek-agent-state.sh`, `pohunek-agent-notify.sh`)
            // to build `session.report_native_id` / `notification.create`
            // requests. Without it the hooks silently `exit 0` and never
            // report native id or notifications for durable-worker-managed
            // agents.
            "POHUNEK_PROTOCOL_VERSION".to_owned(),
            initialize.public_protocol_version.to_string(),
        ),
        (
            "POHUNEK_HOOK_TRACE_PATH".to_owned(),
            shared.hook_trace_path.to_string_lossy().into_owned(),
        ),
        (
            "POHUNEK_HOOK_TRACE_MAX_BYTES".to_owned(),
            pohunek_logging::config::HOOK_TRACE_MAX_FILE_BYTES.to_string(),
        ),
        (
            "POHUNEK_BACKGROUND_LABEL_MAX_BYTES".to_owned(),
            protocol::BACKGROUND_LABEL_MAX_BYTES.to_string(),
        ),
        (
            "POHUNEK_BACKGROUND_PROGRAM_MAX_BYTES".to_owned(),
            protocol::BACKGROUND_PROGRAM_MAX_BYTES.to_string(),
        ),
    ]);
    if let Some(daemon_id) = &initialize.daemon_instance_id {
        reserved.insert("POHUNEK_DAEMON_ID".to_owned(), daemon_id.to_string());
    }
    if let Some(reference_kind) = &initialize.launch.reference_kind {
        reserved.insert(
            "POHUNEK_NATIVE_REFERENCE_KIND".to_owned(),
            reference_kind.clone(),
        );
    }
    environment.extend(reserved);
    environment.retain(|name, _| !protocol::is_denylisted(name));
    Command {
        program: initialize.executable.to_string_lossy().into_owned(),
        args: initialize.arguments.clone(),
        base,
        env: environment.into_iter().collect(),
        cwd: initialize.cwd.clone(),
        cols: initialize.dimensions.columns(),
        rows: initialize.dimensions.rows(),
    }
}

/// Identifies the running worker binary for its journal.
fn worker_origin(generation: String) -> Result<WorkerOrigin, WorkerError> {
    let executable =
        std::env::current_exe().map_err(|source| WorkerError::Executable { source })?;
    if !executable.is_absolute() {
        return Err(WorkerError::Executable {
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("executable path {} is not absolute", executable.display()),
            ),
        });
    }
    Ok(WorkerOrigin {
        executable,
        version: WORKER_VERSION.to_owned(),
        generation,
    })
}

fn wire_identity(identity: &ProcessIdentity) -> Result<WireProcessIdentity, ControlError> {
    Ok(WireProcessIdentity {
        pid: identity.pid,
        start_identity: identity
            .start_identity
            .parse::<u64>()
            .map_err(|error| control_error(ControlCode::RuntimeFault, error, false))?,
    })
}

fn wire_child_identity(identity: &ChildIdentity) -> Result<WireProcessIdentity, ControlError> {
    let start_identity = identity.start_identity.parse::<u64>().map_err(|error| {
        control_error(
            ControlCode::RuntimeFault,
            format!("journal process start identity is invalid: {error}"),
            false,
        )
    })?;
    Ok(WireProcessIdentity {
        pid: identity.pid,
        start_identity,
    })
}

fn journal_identity(identity: &ProcessIdentity) -> ChildIdentity {
    ChildIdentity {
        pid: identity.pid,
        process_group: identity.process_group,
        start_identity: identity.start_identity.clone(),
    }
}

fn wire_terminal(snapshot: &crate::TerminalSnapshot) -> Result<WireTerminalSnapshot, WorkerError> {
    let dimensions = Dimensions::new(snapshot.cols, snapshot.rows)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    Ok(WireTerminalSnapshot {
        watermark: snapshot.watermark,
        dimensions,
        cursor: Cursor {
            column: snapshot.cursor_col,
            row: snapshot.cursor_row,
            visible: snapshot.cursor_visible,
        },
        alternate_screen: snapshot.alternate_screen,
        title: snapshot.title.clone(),
        progress: snapshot.progress.clone(),
        visible_lines: snapshot.visible_text.lines().map(str::to_owned).collect(),
    })
}

fn exit_status(exit: &Exit, stopped_by_user: bool) -> ExitStatus {
    ExitStatus {
        code: exit.exit_code,
        signal: exit.signal.as_deref().and_then(signal_number),
        stopped_by_user,
        exited_at_ms: unix_ms(),
        output_forced_closed: false,
    }
}

fn signal_number(name: &str) -> Option<i32> {
    let normalized = name.to_ascii_lowercase();
    if normalized.contains("term") {
        Some(libc::SIGTERM)
    } else if normalized.contains("kill") {
        Some(libc::SIGKILL)
    } else if normalized.contains("hangup") || normalized.contains("hup") {
        Some(libc::SIGHUP)
    } else if normalized.contains("interrupt") || normalized.contains("int") {
        Some(libc::SIGINT)
    } else {
        None
    }
}

fn capabilities(version: Version) -> Vec<Capability> {
    let mut capabilities = vec![
        Capability::AtomicReplay,
        Capability::TerminalSnapshot,
        Capability::DeduplicatedInput,
        Capability::IdentityHook,
    ];
    if version >= ATTACH_SNAPSHOT_VERSION {
        capabilities.push(Capability::AttachSnapshot);
    }
    if version >= protocol::CONTROL_PLANE_OBSERVATION_VERSION {
        capabilities.push(Capability::ControlPlaneObservation);
    }
    if version >= protocol::SUBAGENT_OBSERVATION_VERSION {
        capabilities.push(Capability::SubagentObservation);
    }
    capabilities
}

fn lease_control_error(error: LeaseError) -> ControlError {
    match error {
        LeaseError::Busy => {
            control_error_message(ControlCode::ControllerBusy, "controller is busy", true)
        }
        LeaseError::Mismatch => identity_mismatch(),
    }
}

fn input_control_error(error: crate::InputError, version: Version) -> ControlError {
    match error {
        crate::InputError::Conflict { .. } => control_error_message(
            ControlCode::InvalidRequest,
            "write id was reused with different content",
            false,
        ),
        crate::InputError::OutcomeUnknown { .. } => control_error_message(
            ControlCode::WriteOutcomeUnknown,
            "write outcome is no longer retained",
            false,
        ),
        stalled @ crate::InputError::Stalled { .. } => {
            // A v6 peer cannot decode `InputStalled`, so it receives the same
            // stall as a runtime fault with the identical sanitized message.
            let code = if version >= protocol::INPUT_STALL_VERSION {
                ControlCode::InputStalled
            } else {
                ControlCode::RuntimeFault
            };
            control_error(code, stalled, false)
        }
        other => control_error(ControlCode::RuntimeFault, other, true),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlInputId {
    Namespaced { sequence: u64 },
    Legacy,
}

fn control_input_id(write_id: &WriteId, lease_id: &str) -> Result<ControlInputId, ControlError> {
    let value = write_id
        .as_str()
        .strip_prefix(CONTROL_INPUT_PREFIX)
        .ok_or_else(|| invalid_control_write_id(write_id))?;
    let Some((namespace, sequence)) = value.rsplit_once('-') else {
        // Older daemons emitted `input-N` and could deliver two freshly
        // allocated IDs out of order. Preserve their bounded conservative
        // deduplication rather than applying the namespaced monotonic invariant.
        let sequence = value
            .parse::<u64>()
            .map_err(|_error| invalid_control_write_id(write_id))?;
        if sequence == 0 {
            return Err(invalid_control_write_id(write_id));
        }
        return Ok(ControlInputId::Legacy);
    };
    if namespace != lease_id {
        return Err(identity_mismatch());
    }
    let sequence = sequence
        .parse::<u64>()
        .map_err(|_error| invalid_control_write_id(write_id))?;
    if sequence == 0 {
        return Err(invalid_control_write_id(write_id));
    }
    Ok(ControlInputId::Namespaced { sequence })
}

fn validate_attach_write_id(
    write_id: &WriteId,
    stream_id: &StreamId,
    sequence: u64,
) -> Result<(), ControlError> {
    let expected = format!("{ATTACH_INPUT_PREFIX}{stream_id}-{sequence}");
    if write_id.as_str() == expected {
        Ok(())
    } else {
        Err(control_error_message(
            ControlCode::InvalidRequest,
            "attach input id is not the next stream-scoped sequence",
            false,
        ))
    }
}

fn invalid_control_write_id(write_id: &WriteId) -> ControlError {
    control_error_message(
        ControlCode::InvalidRequest,
        &format!("control input id `{write_id}` is not lease-scoped and monotonic"),
        false,
    )
}

fn identity_mismatch() -> ControlError {
    control_error_message(
        ControlCode::IdentityMismatch,
        "worker request identity does not match",
        false,
    )
}

fn invalid_state(message: &str) -> ControlError {
    control_error_message(ControlCode::InvalidState, message, false)
}

fn control_error(
    code: ControlCode,
    error: impl std::fmt::Display,
    retryable: bool,
) -> ControlError {
    control_error_message(code, &error.to_string(), retryable)
}

fn control_error_message(code: ControlCode, message: &str, retryable: bool) -> ControlError {
    ControlError {
        code,
        message: message.to_owned(),
        retryable,
    }
}

async fn persist(journal: Journal, record: JournalRecord) -> Result<(), WorkerError> {
    let result = tokio::task::spawn_blocking(move || journal.write(&record))
        .await
        .map_err(|_join_error| WorkerError::Protocol("journal writer task failed".to_owned()))?;
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.committed_durability_uncertain() => {
            // The replacement is already authoritative in this process. Treating
            // it as pre-commit would strand a live PTY in a journaled Live state
            // without its monitors. Continue and surface the reduced crash
            // durability explicitly; a later write retries the directory sync.
            event!(
                name: "worker.journal.durability.uncertain",
                Level::ERROR,
                error.type = "journal_durability",
                error.message = %error,
                "worker journal committed but durability is uncertain: {{error.message}}",
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

async fn persist_control(journal: Journal, record: JournalRecord) -> Result<(), ControlError> {
    persist(journal, record)
        .await
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))
}

/// Records one rejected private identity claim without any payload.
///
/// Only the worker and session identifiers, the operation, the stable reason
/// code, the kernel-attested peer id, and the kernel interface that attested it
/// are emitted. The caller-supplied process id, native references, tokens, and
/// protocol frames never reach a log.
fn reject_identity(
    shared: &Shared,
    operation: &'static str,
    reason: RejectReason,
    peer: &PeerContext,
) {
    event!(
        name: "worker.identity.rejected",
        Level::WARN,
        worker.id = %shared.worker_id,
        session.id = %shared.session_id,
        identity.operation = operation,
        identity.reason = reason.code(),
        peer.pid = peer.identity().pid,
        peer.provenance = %peer.provenance(),
        "private identity claim rejected: {{identity.reason}}",
    );
}

async fn prepare_socket(path: &Path, platform: Platform) -> Result<PreparedSocket, WorkerError> {
    validate_socket_path(path, platform, SocketKind::Worker)?;
    let parent = path.parent().ok_or_else(|| WorkerError::Filesystem {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket has no parent"),
    })?;
    let name = path
        .file_name()
        .ok_or_else(|| WorkerError::Filesystem {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket has no filename"),
        })?
        .to_os_string();
    let directory = TrustedDir::open_or_create_absolute(parent, SOCKET_DIRECTORY_MODE)?;
    let authority = directory.acquire_lock(SOCKET_LOCK_NAME, SOCKET_MODE)?;
    if let Some(identity) =
        directory.entry_identity_with_mode(&name, EntryKind::Socket, SOCKET_MODE)?
    {
        match UnixStream::connect(path).await {
            Ok(_) => {
                return Err(WorkerError::Socket {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        "worker socket already accepts connections",
                    ),
                });
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                match directory.stage_random(&name, ".pohunek-worker-stale-", identity)? {
                    StageOutcome::Staged(entry) => {
                        let _ = entry.remove()?;
                    }
                    StageOutcome::Missing | StageOutcome::IdentityChanged => {}
                    StageOutcome::DestinationExists => {
                        return Err(WorkerError::Filesystem {
                            path: path.to_path_buf(),
                            source: std::io::Error::new(
                                std::io::ErrorKind::AlreadyExists,
                                "worker socket cleanup staging name already exists",
                            ),
                        });
                    }
                    _ => {
                        return Err(WorkerError::Filesystem {
                            path: path.to_path_buf(),
                            source: std::io::Error::new(
                                std::io::ErrorKind::Unsupported,
                                "unsupported worker socket staging outcome",
                            ),
                        });
                    }
                }
            }
            Err(source) => {
                return Err(WorkerError::Socket {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    Ok(PreparedSocket {
        directory,
        authority,
        name,
    })
}

fn random_value(prefix: &str) -> Result<String, std::io::Error> {
    let mut bytes = [0_u8; RANDOM_BYTES];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(format!("{prefix}-{}", hex(&bytes)))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to String is infallible");
    }
    encoded
}

fn timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn process_start(pid: u32) -> Result<u64, WorkerError> {
    HostInspector::new()
        .identity(pid)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?
        .map(|identity| identity.start_identity.get())
        .ok_or_else(|| WorkerError::Protocol("process no longer exists".to_owned()))
}

/// Reads the worker's own kernel creation number for the journal.
///
/// The number lets a later runtime sweep attribute the worker's descendants
/// whose environment the host withholds. It only speeds up cleanup, so a host
/// that reports no lineage yields `None` silently and any other read failure
/// yields `None` with a warning; neither stops the worker from starting.
fn own_spawn_id() -> Option<String> {
    match HostInspector::new().lineage(std::process::id()) {
        Ok(Some(lineage)) => Some(lineage.id.to_string()),
        Ok(None) => {
            event!(
                name: "worker.spawn_id.unreadable",
                Level::WARN,
                "worker process lineage vanished while reading the spawn id; journaling none",
            );
            None
        }
        Err(PlatformProcessError::Unavailable { .. }) => None,
        Err(error) => {
            event!(
                name: "worker.spawn_id.unreadable",
                Level::WARN,
                error.type = "process_inspection",
                error.message = %error,
                "worker spawn id could not be read; journaling none: {{error.message}}",
            );
            None
        }
    }
}

/// Reads the worker's own kernel coalitions for the journal.
///
/// They let a later runtime sweep leave out the processes the host withholds
/// the environment of when the kernel puts them in other coalitions than the
/// worker's. Like the spawn id they only speed up cleanup, so a host without
/// coalitions yields `None` silently and any other read failure yields `None`
/// with a warning; neither stops the worker from starting.
fn own_coalitions() -> Option<String> {
    match HostInspector::new().coalitions(std::process::id()) {
        Ok(Some(coalitions)) => Some(coalitions.to_string()),
        Ok(None) => {
            event!(
                name: "worker.coalitions.unreadable",
                Level::WARN,
                "worker process vanished while reading its coalitions; journaling none",
            );
            None
        }
        Err(PlatformProcessError::Unavailable { .. }) => None,
        Err(error) => {
            event!(
                name: "worker.coalitions.unreadable",
                Level::WARN,
                error.type = "process_inspection",
                error.message = %error,
                "worker coalitions could not be read; journaling none: {{error.message}}",
            );
            None
        }
    }
}

fn is_descendant(pid: u32, root: u32) -> Result<bool, WorkerError> {
    is_descendant_with(&HostInspector::new(), pid, root)
}

fn is_descendant_with(
    inspector: &dyn ProcessInspector,
    mut pid: u32,
    root: u32,
) -> Result<bool, WorkerError> {
    for _ in 0..128 {
        if pid == root {
            return Ok(true);
        }
        if pid <= 1 {
            return Ok(false);
        }
        let parent = inspector
            .parent_pid(pid)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?
            .ok_or_else(|| WorkerError::Protocol("process no longer exists".to_owned()))?;
        if parent == pid {
            return Ok(false);
        }
        pid = parent;
    }
    Ok(false)
}

/// Whether the executable file name of `pid` contains the provider name.
///
/// A process without a readable executable (another user's, or one that exited)
/// is not named for any provider.
fn executable_is_named_for(
    inspector: &dyn ProcessInspector,
    pid: u32,
    provider: &str,
) -> Result<bool, WorkerError> {
    let Some(executable) = inspector
        .executable(pid)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?
    else {
        return Ok(false);
    };
    let executable_name = executable
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    Ok(executable_name.contains(&provider.to_ascii_lowercase()))
}

/// The hook-helper role of a provider: the first argument of the process its
/// launch process starts to run the provider's hooks.
///
/// Codex 0.160.0 runs its hooks from `codex app-server ...`, a direct child of
/// the launched `codex`, so that child speaks for the conversation. A provider
/// without an entry has no such helper, and only its launch process may report.
/// An independent same-provider child (a second `codex` or `claude` the launch
/// process started) carries no helper argument, so it never qualifies.
const HOOK_HELPER_ARGV1: &[(&str, &str)] = &[("codex", "app-server")];

/// Whether the command line of `candidate` is the hook-helper invocation of
/// `provider`.
///
/// Fails closed: a process whose facts cannot be read, or that exited, is not
/// the helper.
fn is_hook_helper_invocation(
    inspector: &dyn ProcessInspector,
    pid: u32,
    provider: &str,
) -> Result<bool, WorkerError> {
    let Some(role) = HOOK_HELPER_ARGV1
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, argument)| *argument)
    else {
        return Ok(false);
    };
    let fact = inspector
        .process(pid)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    Ok(fact.is_some_and(|fact| fact.cmdline.get(1).map(String::as_str) == Some(role)))
}

/// Whether `candidate` is the provider's hook helper that `designated`, the
/// launch process, started directly.
///
/// Only a direct child in the helper role qualifies: a deeper process, such as
/// an agent a tool command started, or an independent same-provider child never
/// does. Both generations are rechecked after the parent link is read, so a
/// reused PID fails closed.
fn is_provider_child_of_launch(
    inspector: &dyn ProcessInspector,
    candidate: OsProcessIdentity,
    designated: &WireProcessIdentity,
    provider: &str,
) -> Result<bool, WorkerError> {
    let designated = OsProcessIdentity {
        pid: designated.pid,
        start_identity: StartIdentity::new(designated.start_identity),
    };
    if !executable_is_named_for(inspector, candidate.pid, provider)?
        || !is_hook_helper_invocation(inspector, candidate.pid, provider)?
    {
        return Ok(false);
    }
    let parent = inspector
        .parent_pid(candidate.pid)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    if parent != Some(designated.pid) {
        return Ok(false);
    }
    for expected in [designated, candidate] {
        let current = inspector
            .identity(expected.pid)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        if current != Some(expected) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Selects the launch process for `provider` among `root` and its descendants.
///
/// A process qualifies only when its executable file name contains the provider
/// name. Interpreters (`node`, `bun`) never qualify by themselves, and neither
/// does an interpreter's script argument: a wrapper install is accepted only
/// when the process image that runs is itself named for the provider (a native
/// binary or a launcher named for it). Anything else yields `None`, which
/// leaves the claim unaccepted. This is the [`LaunchProcess::NamedForProvider`]
/// rule; a schema declaring [`LaunchProcess::FrozenRootEntrypoint`] designates
/// the frozen PTY root instead.
fn designated_launch_process_with(
    inspector: &dyn ProcessInspector,
    root: OsProcessIdentity,
    provider: &str,
) -> Result<Option<WireProcessIdentity>, WorkerError> {
    let mut processes = inspector
        .descendant_identities(root)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    processes.push(root);
    let mut candidates = Vec::new();
    for process in processes {
        if !executable_is_named_for(inspector, process.pid, provider)? {
            continue;
        }
        if inspector
            .identity(process.pid)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?
            != Some(process)
        {
            return Err(WorkerError::Protocol(
                "launch candidate changed identity during inspection".to_owned(),
            ));
        }
        candidates.push(WireProcessIdentity {
            pid: process.pid,
            start_identity: process.start_identity.get(),
        });
    }
    candidates.sort_by_key(|candidate| (candidate.start_identity, candidate.pid));
    let Some(candidate) = candidates.into_iter().next() else {
        return Ok(None);
    };
    let exact_candidate = OsProcessIdentity {
        pid: candidate.pid,
        start_identity: StartIdentity::new(candidate.start_identity),
    };
    if inspector
        .identity(root.pid)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?
        != Some(root)
        || inspector
            .identity(candidate.pid)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?
            != Some(exact_candidate)
        || !is_descendant_with(inspector, candidate.pid, root.pid)?
    {
        return Err(WorkerError::Protocol(
            "launch candidate changed ancestry during inspection".to_owned(),
        ));
    }
    if inspector
        .identity(root.pid)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?
        != Some(root)
        || inspector
            .identity(candidate.pid)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?
            != Some(exact_candidate)
    {
        return Err(WorkerError::Protocol(
            "launch candidate changed identity during ancestry inspection".to_owned(),
        ));
    }
    Ok(Some(candidate))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug)]
struct PrefixStream<S> {
    prefix: Option<u8>,
    inner: S,
}

impl<S> PrefixStream<S> {
    fn new(prefix: u8, inner: S) -> Self {
        Self {
            prefix: Some(prefix),
            inner,
        }
    }
}

impl<S> AsyncRead for PrefixStream<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Some(prefix) = self.prefix.take() {
            buf.put_slice(&[prefix]);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S> AsyncWrite for PrefixStream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod launch_layout_tests;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        admit_hook, authorize_control_request, await_observation_page, capabilities,
        control_input_id, event_visible_to_connection, identity_sequence_is_fresh,
        observation_timed_out, redeem_data_grant, serve_stream_data, signal_number,
        valid_identity_expiry, validate_attach_write_id, validate_data_start,
        validate_observation_request, validate_terminal_snapshot_dimensions,
        validate_terminal_snapshot_response, write_data_error, write_observation_page,
        write_output_chunks, write_runtime_output_event, write_terminal_chunks, Connection,
        ControlInputId, DataGrant, IdentityMarks, ObservationGrant, ObservationWaitOutcome,
        PrefixStream, Reporter, TokenState, WireTerminalSnapshot, ATTACH_INPUT_PREFIX,
    };
    use pohunek_worker_protocol::{
        self as protocol, AttachStart, Capability, ControlCode, ControlError, ControlMessage,
        ControlResponse, Cursor, DataToken, Dimensions, EventKind, ExitStatus, FrameHeader,
        FrameKind, HookAction, LeaseId, RequestId, ResponseKind, StreamId, StreamMode, Version,
        WorkerInstanceId, WriteId, CURRENT_VERSION, MAX_DATA_PAYLOAD_BYTES, PREVIOUS_VERSION,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    use crate::identity::RejectReason;
    use crate::journal::SubagentPhase;
    use crate::{ChildIdentity, JournalRecord, ReleasedIdentity};

    /// Extra bytes force exactly one partial frame after a full wire payload.
    const OVERSIZED_PAYLOAD_EXTRA: usize = 257;
    /// Client-to-worker buffering of the partial-input regression stream.
    ///
    /// With one byte of buffering a client write returns only after the worker
    /// has read every earlier byte, which pins the worker mid-frame.
    const ONE_BYTE_BUFFER: usize = 1;
    /// Bytes of the input frame sent before output becomes ready: the
    /// four-byte header length plus the opening bytes of its JSON header.
    const PARTIAL_FRAME_BYTES: usize = 6;
    /// Worker-to-client buffering of the partial-input regression stream.
    const CLIENT_BUFFER: usize = 4096;

    /// The production stop grace: a stop under test never escalates early because
    /// a loaded host was slow, and a child that dies on SIGTERM does not wait it out.
    fn server_stop_grace() -> Duration {
        crate::WorkerConfig::new().stop_grace
    }

    /// A private fixture directory removed when the guard drops, also when the
    /// test fails.
    struct BarrierDirectory(tempfile::TempDir);

    impl BarrierDirectory {
        fn new() -> Self {
            Self(pohunek_test_support::tempdir().expect("create fixture directory"))
        }

        fn path(&self) -> &std::path::Path {
            self.0.path()
        }
    }

    #[tokio::test]
    async fn retained_session_lock_rejects_a_second_worker_bind() {
        let directory = BarrierDirectory::new();
        let args = super::ServerArgs {
            session_id: "s-112".to_owned(),
            worker_id: "worker-lock".to_owned(),
            generation: "abcd2345".to_owned(),
            socket_path: directory.path().join("worker.sock"),
            journal_path: directory.path().join("journal.json"),
            daemon_socket_path: directory.path().join("daemon.sock"),
            hook_trace_path: directory
                .path()
                .join(pohunek_logging::config::HOOK_TRACE_FILE_NAME),
            config: crate::WorkerConfig::new(),
        };
        let first = super::Server::bind(args.clone())
            .await
            .expect("bind first worker");

        assert!(matches!(
            super::Server::bind(args).await,
            Err(crate::WorkerError::TrustedFilesystem(
                pohunek_platform::filesystem::FsError::LockContended { .. }
            ))
        ));
        drop(first);
    }

    #[tokio::test]
    async fn invalid_daemon_socket_override_fails_before_worker_mutation() {
        let directory = BarrierDirectory::new();
        let overlong = directory.path().join("d".repeat(256));
        let worker_socket = directory.path().join("worker.sock");
        let journal = directory.path().join("journal.json");

        assert!(matches!(
            super::Server::bind(super::ServerArgs {
                session_id: "s-113".to_owned(),
                worker_id: "worker-invalid-daemon-path".to_owned(),
                generation: "abcd2345".to_owned(),
                socket_path: worker_socket.clone(),
                journal_path: journal.clone(),
                daemon_socket_path: overlong,
                hook_trace_path: directory
                    .path()
                    .join(pohunek_logging::config::HOOK_TRACE_FILE_NAME),
                config: crate::WorkerConfig::new(),
            })
            .await,
            Err(crate::WorkerError::Paths(_))
        ));
        assert!(!worker_socket.exists());
        assert!(!journal.exists());
        assert!(!directory.path().join(super::SOCKET_LOCK_NAME).exists());
    }

    async fn launch_claim_fixture() -> (super::Server, crate::PtyOwner, BarrierDirectory) {
        let directory = BarrierDirectory::new();
        let root = directory.path().to_path_buf();
        let config = crate::WorkerConfig::new();
        let server = super::Server::bind(super::ServerArgs {
            session_id: "s-112".into(),
            worker_id: "worker-claim".into(),
            generation: "abcd2345".to_owned(),
            socket_path: root.join("worker.sock"),
            journal_path: root.join("journal.json"),
            daemon_socket_path: root.join("daemon.sock"),
            hook_trace_path: root.join(pohunek_logging::config::HOOK_TRACE_FILE_NAME),
            config: config.clone(),
        })
        .await
        .unwrap();
        let pty = super::spawn_pty(
            crate::Command {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "read -r line".into()],
                base: crate::EnvBase::Empty,
                env: crate::test_support::child_env(),
                cwd: root.clone(),
                cols: 80,
                rows: 24,
            },
            &config,
        )
        .unwrap();
        {
            let mut state = server.shared.state.lock().await;
            state.phase = super::WireRuntimePhase::Running;
            state.worker_instance_id = Some(WorkerInstanceId::new("runtime-claim").unwrap());
            state.launch_agent_base = Some("claude".into());
            state.hook_schema = protocol::hook_schema("identity-subagent-v1");
            state.pty = Some(pty.clone());
            state.journal.phase = super::JournalPhase::Live;
            state.journal.worker_instance_id = Some("runtime-claim".into());
            state.journal.child = Some(super::journal_identity(pty.identity()));
        };
        (server, pty, directory)
    }

    /// Builds a peer context standing in for the reported process itself.
    ///
    /// These tests drive the hook handler directly rather than over a socket,
    /// so they supply the identity the kernel would have attested.
    fn subject_peer(pty: &crate::PtyOwner) -> crate::identity::PeerContext {
        crate::identity::PeerContext::for_test(pohunek_platform::process::ProcessIdentity {
            pid: pty.identity().pid,
            start_identity: pohunek_platform::process::StartIdentity::new(
                pty.identity().start_identity.parse::<u64>().unwrap(),
            ),
        })
    }

    fn launch_report(pty: &crate::PtyOwner, sequence: u64, reference: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "identity_report", "runtime_id": "runtime-claim", "provider": "claude",
            "pid": pty.identity().pid,
            "start_identity": pty.identity().start_identity.parse::<u64>().unwrap(),
            "sequence": sequence,
            "expires_at": (time::OffsetDateTime::now_utc() + time::Duration::seconds(30))
                .format(&time::format_description::well_known::Rfc3339).unwrap(),
            "reference_kind": "id", "native_reference": reference,
        })
    }

    #[tokio::test]
    async fn the_launch_processs_latest_reference_outlives_the_claim_lease_and_its_release() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        assert!(send_hook(&server, &pty, launch_report(&pty, 1, "first")).await);
        assert!(send_hook(&server, &pty, launch_report(&pty, 2, "switched")).await);

        let state = server.shared.state.lock().await;
        let claim = state.journal.native_reference_claim.as_ref().unwrap();
        assert_eq!(
            (claim.native_reference.as_str(), claim.sequence),
            ("switched", 2)
        );
        assert_eq!(
            state
                .journal
                .launch_identity
                .as_ref()
                .unwrap()
                .native_reference,
            "first",
            "the launch identity stays the first claim"
        );
        drop(state);

        // The active claim's lease ends and the claim is released: only the
        // lease-bound projection disappears.
        let release = serde_json::json!({
            "type": "identity_release", "provider": "claude", "sequence": 3,
        });
        assert!(send_hook(&server, &pty, release).await);
        let mut state = server.shared.state.lock().await;
        assert!(state.journal.active_identity.is_none());
        state.journal.active_identity = None;
        let snapshot = super::inspect_snapshot(&server.shared, &state)
            .await
            .unwrap();
        assert!(snapshot.active_identity.is_none());
        let reference = snapshot.native_reference.expect("journaled reference");
        assert_eq!(reference.native_reference, "switched");
        assert_eq!(reference.sequence, 2);
        drop(state);
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_report_of_another_process_never_becomes_the_launch_reference() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        assert!(send_hook(&server, &pty, launch_report(&pty, 1, "first")).await);
        let mut other = launch_report(&pty, 2, "nested");
        other["provider"] = "codex".into();
        send_hook(&server, &pty, other).await;
        let state = server.shared.state.lock().await;
        assert_eq!(
            state
                .journal
                .native_reference_claim
                .as_ref()
                .unwrap()
                .native_reference,
            "first"
        );
        drop(state);
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn deferred_launch_ack_owns_durable_original_claim_and_retry_commits_before_event() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        let mut bytes = Vec::new();
        super::serve_identity_hook_with_probe(
            std::sync::Arc::clone(&server.shared),
            &mut super::ControlWriter::new(&mut bytes),
            launch_report(&pty, 1, "original"),
            &subject_peer(&pty),
            |_| {
                Err(crate::WorkerError::Protocol(
                    "transient observation error".into(),
                ))
            },
        )
        .await
        .unwrap();
        let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            response,
            serde_json::json!({"ok": true, "launch_identity_accepted": false, "launch_identity_status": "pending"})
        );
        let owned = server.shared.journal.load().unwrap();
        assert_eq!(
            owned.pending_launch_claims[0].identity.native_reference,
            "original"
        );
        assert!(owned.launch_identity.is_none());

        bytes.clear();
        super::serve_identity_hook_with_probe(
            std::sync::Arc::clone(&server.shared),
            &mut super::ControlWriter::new(&mut bytes),
            launch_report(&pty, 2, "continuation"),
            &subject_peer(&pty),
            |_| panic!("later report cannot replace pending claim"),
        )
        .await
        .unwrap();
        let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response["launch_identity_status"], "rejected");
        let mut events = server.shared.events.subscribe();
        super::retry_launch_claims(&server.shared, |claim| {
            assert_eq!(claim.identity.native_reference, "original");
            Ok(true)
        })
        .await
        .unwrap();
        assert!(matches!(
            events.try_recv().unwrap().kind,
            EventKind::IdentityChanged { .. }
        ));
        let committed = server.shared.journal.load().unwrap();
        assert_eq!(
            committed.launch_identity.unwrap().native_reference,
            "original"
        );
        assert_eq!(
            committed
                .active_identity
                .unwrap()
                .native_reference
                .as_deref(),
            Some("continuation")
        );
        assert!(committed.pending_launch_claims.is_empty());
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
        drop(server);
    }

    /// Sends one hook claim for the fixture's own process and returns whether
    /// the worker accepted it.
    async fn send_hook(
        server: &super::Server,
        pty: &crate::PtyOwner,
        mut claim: serde_json::Value,
    ) -> bool {
        let object = claim.as_object_mut().expect("claim object");
        object.insert("runtime_id".into(), "runtime-claim".into());
        object.insert("pid".into(), pty.identity().pid.into());
        object.insert(
            "start_identity".into(),
            pty.identity().start_identity.parse::<u64>().unwrap().into(),
        );
        let mut bytes = Vec::new();
        super::serve_identity_hook_with_probe(
            std::sync::Arc::clone(&server.shared),
            &mut super::ControlWriter::new(&mut bytes),
            claim,
            &subject_peer(pty),
            |_| Ok(true),
        )
        .await
        .expect("hook answers");
        let response: serde_json::Value = serde_json::from_slice(&bytes).expect("hook response");
        response["ok"].as_bool().expect("ok flag")
    }

    fn expiry() -> String {
        (time::OffsetDateTime::now_utc() + time::Duration::seconds(30))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    fn subagent_start_claim(sequence: u64, agent_type: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "type": "subagent_start", "provider": "codex", "sequence": sequence,
            "subagent_id": format!("child-{sequence}"), "parent_id": null,
            "agent_type": agent_type,
        })
    }

    #[tokio::test]
    async fn identity_reports_are_admitted_for_every_provider_of_the_identity_schema() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        server.shared.state.lock().await.hook_schema = protocol::hook_schema("identity-v1");
        for (sequence, provider) in (1_u64..).zip(["shell", "codex", "claude", "hermes"]) {
            let claim = serde_json::json!({
                "type": "identity_report", "provider": provider, "sequence": sequence,
                "expires_at": expiry(), "reference_kind": "id", "native_reference": "ref",
            });
            assert!(send_hook(&server, &pty, claim).await, "{provider}");
        }
        let unknown = serde_json::json!({
            "type": "identity_report", "provider": "pi", "sequence": 9,
            "expires_at": expiry(), "reference_kind": null, "native_reference": null,
        });
        assert!(!send_hook(&server, &pty, unknown).await);
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_subagent_claim_never_becomes_the_launch_reference() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        server.shared.state.lock().await.hook_schema =
            protocol::hook_schema("identity-subagent-v1");
        assert!(send_hook(&server, &pty, subagent_start_claim(1, Some("Explore"))).await);
        let state = server.shared.state.lock().await;
        assert!(state.journal.launch_identity.is_none());
        assert!(state.journal.native_reference_claim.is_none());
        assert!(state.journal.pending_launch_claims.is_empty());
        drop(state);
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_repeated_identical_launch_report_is_accepted_without_a_new_claim() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        assert!(send_hook(&server, &pty, launch_report(&pty, 1, "same")).await);
        let accepted = server
            .shared
            .state
            .lock()
            .await
            .journal
            .launch_identity
            .clone();
        assert!(send_hook(&server, &pty, launch_report(&pty, 2, "same")).await);
        let state = server.shared.state.lock().await;
        assert_eq!(state.journal.launch_identity, accepted);
        assert!(state.journal.pending_launch_claims.is_empty());
        drop(state);
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_runtime_without_a_schema_admits_no_hook_claim() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        server.shared.state.lock().await.hook_schema = None;
        let before = server.shared.state.lock().await.journal.clone();
        let identity = launch_report(&pty, 1, "ref");
        assert!(!send_hook(&server, &pty, identity).await);
        assert!(!send_hook(&server, &pty, subagent_start_claim(2, None)).await);
        let notification = serde_json::json!({
            "type": "notification_create", "provider": "codex", "sequence": 3, "params": {},
        });
        assert!(!send_hook(&server, &pty, notification).await);
        assert_eq!(server.shared.state.lock().await.journal, before);
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn native_reference_shape_follows_the_schema() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        for (sequence, kind, reference) in [
            (1_u64, Some("uri"), Some("ref")),
            (2, Some("id"), None),
            (3, None, Some("ref")),
        ] {
            let claim = serde_json::json!({
                "type": "identity_report", "provider": "claude", "sequence": sequence,
                "expires_at": expiry(), "reference_kind": kind, "native_reference": reference,
            });
            assert!(
                !send_hook(&server, &pty, claim).await,
                "{kind:?}/{reference:?}"
            );
        }
        assert!(server
            .shared
            .state
            .lock()
            .await
            .journal
            .active_identity
            .is_none());
        let accepted = serde_json::json!({
            "type": "identity_report", "provider": "claude", "sequence": 4,
            "expires_at": expiry(), "reference_kind": "path", "native_reference": "/x",
        });
        assert!(send_hook(&server, &pty, accepted).await);
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn subagent_claim_fields_follow_the_schema() {
        static NO_TYPE: protocol::HookSchema = protocol::HookSchema {
            id: "no-agent-type-test",
            handlers: &[],
            identity_providers: &["claude"],
            subagent_providers: &["codex"],
            actions: &[HookAction::SubagentStart],
            reference_kinds: &[],
            ancestry: protocol::AncestryMatcher::ManagedPtyDescendant,
            launch_process: protocol::LaunchProcess::NamedForProvider,
            nested_active: true,
            subagent_fields: &[protocol::SubagentField::ParentId],
            subagent_sequence: Some(protocol::SubagentSequence::StopAfterStart),
            stop_requires_start: &[],
        };
        let (server, pty, _directory) = launch_claim_fixture().await;
        server.shared.state.lock().await.hook_schema = Some(&NO_TYPE);
        assert!(!send_hook(&server, &pty, subagent_start_claim(1, Some("Explore"))).await);
        assert!(send_hook(&server, &pty, subagent_start_claim(2, None)).await);
        let stop = serde_json::json!({
            "type": "subagent_stop", "provider": "codex", "sequence": 3,
            "subagent_id": "child-2", "outcome": "completed",
        });
        assert!(
            !send_hook(&server, &pty, stop).await,
            "stop is not an admitted action"
        );
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_schema_without_a_sequence_rule_admits_no_subagent_claim() {
        static UNORDERED: protocol::HookSchema = protocol::HookSchema {
            id: "unordered-test",
            handlers: &[],
            identity_providers: &["claude"],
            subagent_providers: &["codex"],
            actions: &[HookAction::SubagentStart, HookAction::SubagentStop],
            reference_kinds: &[],
            ancestry: protocol::AncestryMatcher::ManagedPtyDescendant,
            launch_process: protocol::LaunchProcess::NamedForProvider,
            nested_active: true,
            subagent_fields: &[protocol::SubagentField::Outcome],
            subagent_sequence: None,
            stop_requires_start: &[],
        };
        let (server, pty, _directory) = launch_claim_fixture().await;
        server.shared.state.lock().await.hook_schema = Some(&UNORDERED);
        assert!(!send_hook(&server, &pty, subagent_start_claim(1, None)).await);
        let stop = serde_json::json!({
            "type": "subagent_stop", "provider": "codex", "sequence": 2,
            "subagent_id": "child-1", "outcome": "completed",
        });
        assert!(!send_hook(&server, &pty, stop).await);
        assert!(server
            .shared
            .state
            .lock()
            .await
            .journal
            .subagents
            .is_empty());
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_subagent_stop_is_ordered_by_the_declared_sequence_rule() {
        async fn child(server: &super::Server) -> (SubagentPhase, u64) {
            let state = server.shared.state.lock().await;
            let record = state
                .journal
                .subagents
                .iter()
                .find(|record| record.id == "child-5")
                .expect("child-5 is recorded");
            (record.phase, record.sequence)
        }
        let (server, pty, _directory) = launch_claim_fixture().await;
        server.shared.state.lock().await.hook_schema =
            protocol::hook_schema("identity-subagent-v1");
        let stop = |sequence: u64| {
            serde_json::json!({
                "type": "subagent_stop", "provider": "codex", "sequence": sequence,
                "subagent_id": "child-5", "outcome": "completed",
            })
        };
        assert!(send_hook(&server, &pty, subagent_start_claim(5, None)).await);
        assert!(
            send_hook(&server, &pty, stop(5)).await,
            "a stop that is not after its start is acknowledged"
        );
        assert_eq!(
            child(&server).await,
            (SubagentPhase::Running, 5),
            "a stop that is not after its start leaves the subagent running"
        );
        assert!(send_hook(&server, &pty, stop(6)).await);
        assert_eq!(child(&server).await, (SubagentPhase::Completed, 6));
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
    }

    /// Opens the controller connection `initialize_request` requires.
    fn controller_connection(server: &super::Server) -> (Connection, LeaseId) {
        let owner = crate::LeaseOwner {
            daemon_id: "daemon-initialize".to_owned(),
            peer_pid: std::process::id(),
            peer_start_identity: "7400".to_owned(),
        };
        let lease_id = LeaseId::new("lease-initialize").expect("lease id");
        server
            .shared
            .lease
            .acquire(owner.clone(), lease_id.as_str().to_owned())
            .expect("acquire lease");
        let mut connection = Connection::new(std::sync::Arc::new(owner_peer(&owner)));
        connection.owner = Some(owner);
        connection.lease_id = Some(lease_id.clone());
        connection.selected_version = Some(CURRENT_VERSION);
        (connection, lease_id)
    }

    #[tokio::test]
    async fn initialize_journals_the_delivered_hook_schema() {
        let directory = pohunek_test_support::tempdir().expect("fixture directory");
        let server = environment_fixture(directory.path(), "abcd2345")
            .await
            .expect("bind worker");
        let mut initialize =
            environment_initialize(Some(base_environment(&[("PATH", "/usr/bin:/bin")])), &[]);
        initialize.executable = std::path::PathBuf::from("/bin/sh");
        initialize.arguments = vec!["-c".to_owned(), "read -r line".to_owned()];
        initialize.hook_schema = Some("identity-subagent-v1".to_owned());
        let (connection, lease_id) = controller_connection(&server);

        super::initialize_request(&server.shared, &connection, &lease_id, initialize)
            .await
            .expect("initialize runtime");

        let state = server.shared.state.lock().await;
        assert_eq!(
            state.hook_schema.map(|schema| schema.id),
            Some("identity-subagent-v1")
        );
        let persisted = server.shared.journal.load().expect("load journal");
        assert_eq!(
            persisted.hook_schema.as_deref(),
            Some("identity-subagent-v1")
        );
        let snapshot = super::inspect_snapshot(&server.shared, &state)
            .await
            .expect("inspect");
        assert_eq!(
            snapshot.hook_schema.as_deref(),
            Some("identity-subagent-v1")
        );
        if let Some(pty) = state.pty.clone() {
            drop(state);
            pty.stop("test-cleanup", server.shared.config.stop_grace)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn initialize_without_a_schema_journals_none_and_refuses_an_unknown_one() {
        let directory = pohunek_test_support::tempdir().expect("fixture directory");
        let server = environment_fixture(directory.path(), "abcd2345")
            .await
            .expect("bind worker");
        let mut unknown =
            environment_initialize(Some(base_environment(&[("PATH", "/usr/bin:/bin")])), &[]);
        unknown.hook_schema = Some("identity-v9".to_owned());
        let (connection, lease_id) = controller_connection(&server);

        let error = super::initialize_request(&server.shared, &connection, &lease_id, unknown)
            .await
            .expect_err("an unknown schema id is refused");

        assert_eq!(error.code, ControlCode::InvalidRequest);
        let state = server.shared.state.lock().await;
        assert_eq!(state.phase, super::WireRuntimePhase::Uninitialized);
        assert!(state.pty.is_none());
        assert!(state.journal.hook_schema.is_none());
    }

    /// A peer that no longer exists leaves the journal untouched.
    ///
    /// `reverify` distinguishes a vanished peer from a substituted one, and the
    /// hook path must reject before it writes: proving the return value alone
    /// would say nothing about whether durable state moved.
    #[tokio::test]
    async fn a_vanished_peer_is_rejected_without_touching_the_journal() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        let before = server.shared.state.lock().await.journal.clone();
        // A start identity the live process cannot have makes the recheck fail
        // exactly as an exited-and-reused process id would.
        let vanished = crate::identity::PeerContext::for_test_live(
            pohunek_platform::process::ProcessIdentity {
                pid: std::process::id(),
                start_identity: pohunek_platform::process::StartIdentity::new(u64::MAX),
            },
        );
        let mut bytes = Vec::new();
        super::serve_identity_hook_with_probe(
            std::sync::Arc::clone(&server.shared),
            &mut super::ControlWriter::new(&mut bytes),
            launch_report(&pty, 1, "vanished-peer"),
            &vanished,
            |_| panic!("a rejected peer must never reach launch verification"),
        )
        .await
        .expect("hook answers a rejected claim");

        let response: serde_json::Value =
            serde_json::from_slice(&bytes).expect("decode hook response");
        assert_eq!(response["ok"], serde_json::Value::Bool(false));
        assert_eq!(
            server.shared.state.lock().await.journal,
            before,
            "a rejected claim must not move durable state"
        );
    }

    #[tokio::test]
    async fn immediate_launch_rejection_commits_fence_before_acknowledgment() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        let mut bytes = Vec::new();
        super::serve_identity_hook_with_probe(
            std::sync::Arc::clone(&server.shared),
            &mut super::ControlWriter::new(&mut bytes),
            launch_report(&pty, 1, "original"),
            &subject_peer(&pty),
            |_| Ok(false),
        )
        .await
        .unwrap();
        let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            response,
            serde_json::json!({"ok": true, "launch_identity_accepted": false, "launch_identity_status": "rejected"})
        );
        let committed = server.shared.journal.load().unwrap();
        assert!(committed.launch_identity.is_none());
        assert_eq!(committed.pending_launch_claims.len(), 1);
        assert!(!committed.pending_launch_claims[0].retry_pending);
        assert_eq!(
            committed.pending_launch_claims[0].identity.native_reference,
            "original"
        );

        bytes.clear();
        super::serve_identity_hook_with_probe(
            std::sync::Arc::clone(&server.shared),
            &mut super::ControlWriter::new(&mut bytes),
            launch_report(&pty, 2, "replacement"),
            &subject_peer(&pty),
            |_| panic!("rejection fence must not be reprobed"),
        )
        .await
        .unwrap();
        let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response["launch_identity_status"], "rejected");
        let committed = server.shared.journal.load().unwrap();
        assert!(committed.launch_identity.is_none());
        assert_eq!(
            committed.pending_launch_claims[0].identity.native_reference,
            "original"
        );

        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
        drop(server);
    }

    #[tokio::test]
    async fn failed_pending_commit_never_acknowledges_or_changes_memory() {
        let (server, pty, directory) = launch_claim_fixture().await;
        std::fs::rename(
            server.shared.journal.path(),
            directory.path().join("original.json"),
        )
        .unwrap();
        std::fs::create_dir(server.shared.journal.path()).unwrap();
        let mut bytes = Vec::new();
        let error = super::serve_identity_hook_with_probe(
            std::sync::Arc::clone(&server.shared),
            &mut super::ControlWriter::new(&mut bytes),
            launch_report(&pty, 1, "original"),
            &subject_peer(&pty),
            |_| {
                Err(crate::WorkerError::Protocol(
                    "transient observation error".into(),
                ))
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, crate::WorkerError::Journal(_)));
        assert!(bytes.is_empty());
        let state = server.shared.state.lock().await;
        assert!(state.journal.pending_launch_claims.is_empty());
        assert!(state.journal.active_identity.is_none());
        drop(state);
        pty.stop("test-cleanup", server.shared.config.stop_grace)
            .await
            .unwrap();
        drop(server);
    }

    #[test]
    fn identity_provider_allowlist_is_exact() {
        let schema = protocol::hook_schema("identity-subagent-v1");
        for provider in ["shell", "codex", "claude", "hermes"] {
            assert!(
                admit_hook(schema, HookAction::IdentityReport, provider, Some("codex")).is_ok(),
                "{provider} must be accepted"
            );
        }

        for provider in ["", "Hermes", "HERMES", "hermes ", "unknown"] {
            assert_eq!(
                admit_hook(schema, HookAction::IdentityReport, provider, Some("codex")).err(),
                Some(RejectReason::ProviderNotAllowed),
                "{provider:?} must be rejected"
            );
        }
    }

    #[test]
    fn hook_admission_follows_the_schema_per_action() {
        let identity = protocol::hook_schema("identity-v1");
        let subagent = protocol::hook_schema("identity-subagent-v1");
        for action in [HookAction::SubagentStart, HookAction::SubagentStop] {
            assert_eq!(
                admit_hook(identity, action, "codex", Some("hermes")).err(),
                Some(RejectReason::ActionNotAllowed),
                "a schema without subagents refuses {action}"
            );
            for provider in ["codex", "claude"] {
                admit_hook(subagent, action, provider, Some("shell"))
                    .expect("a subagent provider is admitted");
            }
            for provider in ["shell", "hermes", ""] {
                assert_eq!(
                    admit_hook(subagent, action, provider, Some("shell")).err(),
                    Some(RejectReason::ProviderNotAllowed),
                    "{provider:?} is not a subagent provider"
                );
            }
        }
        for action in [
            HookAction::IdentityReport,
            HookAction::IdentityRelease,
            HookAction::SubagentStart,
            HookAction::SubagentStop,
            HookAction::Notification,
        ] {
            assert_eq!(
                admit_hook(None, action, "codex", Some("codex")).err(),
                Some(RejectReason::ActionNotAllowed),
                "a runtime without a schema admits no {action}"
            );
        }
    }

    #[test]
    fn nested_switching_disabled_pins_identity_reports_to_the_launch_runtime() {
        static PINNED: protocol::HookSchema = protocol::HookSchema {
            id: "pinned-test",
            handlers: &[],
            identity_providers: &["hermes", "codex"],
            subagent_providers: &[],
            actions: &[HookAction::IdentityReport],
            reference_kinds: &[],
            ancestry: protocol::AncestryMatcher::ManagedPtyDescendant,
            launch_process: protocol::LaunchProcess::NamedForProvider,
            nested_active: false,
            subagent_fields: &[],
            subagent_sequence: None,
            stop_requires_start: &[],
        };
        admit_hook(
            Some(&PINNED),
            HookAction::IdentityReport,
            "hermes",
            Some("hermes"),
        )
        .expect("the launch runtime itself is admitted");
        assert_eq!(
            admit_hook(
                Some(&PINNED),
                HookAction::IdentityReport,
                "codex",
                Some("hermes")
            )
            .err(),
            Some(RejectReason::ProviderNotAllowed)
        );
    }

    #[test]
    fn identity_expiry_enforces_the_shared_ttl_ceiling() {
        let valid = (time::OffsetDateTime::now_utc() + time::Duration::seconds(30))
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format valid expiry");
        let expired = (time::OffsetDateTime::now_utc() - time::Duration::seconds(1))
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format expired expiry");
        let overlong = (time::OffsetDateTime::now_utc()
            + time::Duration::seconds(
                i64::try_from(protocol::MAX_IDENTITY_CLAIM_TTL_SECS).expect("TTL fits i64") + 1,
            ))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("format overlong expiry");

        assert!(valid_identity_expiry(&valid));
        assert!(!valid_identity_expiry(&expired));
        assert!(!valid_identity_expiry(&overlong));
        assert!(!valid_identity_expiry("not-a-timestamp"));
    }

    #[test]
    fn released_identity_sequence_rejects_late_reports_and_allows_reassertion() {
        let mut journal = JournalRecord::bootstrap(
            "s-identity".to_owned(),
            "worker-identity".to_owned(),
            crate::test_support::test_origin(),
            10,
            "100".to_owned(),
            "boot-test".to_owned(),
            (80, 24),
            "2026-08-04T00:00:00Z".to_owned(),
        );
        journal.active_identity_release = Some(ReleasedIdentity {
            provider: "claude".to_owned(),
            process: ChildIdentity {
                pid: 11,
                process_group: 11,
                start_identity: "110".to_owned(),
            },
            sequence: 8,
        });

        let marks = IdentityMarks::default();
        let released = Reporter {
            pid: 11,
            start_identity: 110,
        };
        assert!(!identity_sequence_is_fresh(&journal, &marks, released, 7));
        assert!(!identity_sequence_is_fresh(&journal, &marks, released, 8));
        assert!(identity_sequence_is_fresh(&journal, &marks, released, 9));
    }

    // Needs a descendant that keeps the PTY open after the root exits. Darwin
    // revokes the controlling terminal from every holder when the session
    // leader exits, so this scenario exists only on Linux.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn root_exit_preserves_descendant_output_until_pty_eof() {
        let config = crate::WorkerConfig::new();
        let barrier = BarrierDirectory::new();
        let release = barrier.path().join("release");
        let pty = crate::PtyOwner::spawn(
            crate::Command {
                program: "/bin/sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    concat!(
                        "trap '' HUP; (",
                        "while [ ! -e \"$1\" ] && [ -d \"$2\" ]; do sleep 0.01; done; ",
                        "if [ -e \"$1\" ]; then printf 'late-descendant-output\\n'; fi) & ",
                        "printf 'root-exited:%s\\n' \"$!\""
                    )
                    .to_owned(),
                    "pohunek-descendant-output".to_owned(),
                    release.to_string_lossy().into_owned(),
                    barrier.path().to_string_lossy().into_owned(),
                ],
                base: crate::EnvBase::Empty,
                env: crate::test_support::child_env(),
                cwd: barrier.path().to_path_buf(),
                cols: 80,
                rows: 24,
            },
            config.history_bytes,
            config.subscriber_bytes,
            config.input_dedup_entries,
            config.input_stall,
        )
        .expect("spawn PTY");
        let mut subscriber = pty.subscribe_output(Some(0)).expect("subscribe output");

        tokio::time::timeout(Duration::from_secs(3), super::wait_runtime_exit(&pty))
            .await
            .expect("root process exit deadline")
            .expect("root process exit");
        let page = pty.output().observe(None, 1_024).expect("output page");
        assert!(!page.exited, "root exit must not claim PTY EOF");
        std::fs::write(&release, b"release").expect("release descendant output");

        let (observed, final_offset) = tokio::time::timeout(Duration::from_secs(2), async {
            let mut observed = Vec::new();
            loop {
                match subscriber.recv().await.expect("output event before EOF") {
                    crate::OutputEvent::Replay(chunk) | crate::OutputEvent::Output(chunk) => {
                        observed.extend_from_slice(&chunk.bytes);
                    }
                    crate::OutputEvent::TerminalSnapshot(chunk) => {
                        observed.extend_from_slice(&chunk.bytes);
                    }
                    crate::OutputEvent::Gap { .. } => observed.clear(),
                    crate::OutputEvent::Exit { next_offset } => break (observed, next_offset),
                }
            }
        })
        .await
        .expect("descendant output and EOF deadline");
        assert!(
            String::from_utf8_lossy(&observed).contains("late-descendant-output"),
            "subscriber missed output produced after root exit"
        );
        assert_eq!(final_offset, pty.output().next_offset());
        let page = pty
            .output()
            .observe(None, 1_024)
            .expect("final output page");
        assert!(page.exited, "reader EOF must close output");
        assert_eq!(page.runtime_end_offset, final_offset);
    }

    // Needs a descendant that keeps the PTY open after the root exits. Darwin
    // revokes the controlling terminal from every holder when the session
    // leader exits, so this scenario exists only on Linux.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn natural_exit_bounds_a_descendant_that_never_closes_the_pty() {
        let directory = BarrierDirectory::new();
        // The descendant never closes the PTY, so the drain always expires; its
        // short bound only keeps the test fast. The stop that follows uses the
        // production grace.
        let config = crate::WorkerConfig {
            post_exit_drain_timeout: Duration::from_millis(50),
            ..crate::WorkerConfig::new()
        };
        let server = super::Server::bind(super::ServerArgs {
            session_id: "s-113".to_owned(),
            worker_id: "worker-bounded-drain".to_owned(),
            generation: "abcd2345".to_owned(),
            socket_path: directory.path().join("worker.sock"),
            journal_path: directory.path().join("journal.json"),
            daemon_socket_path: directory.path().join("daemon.sock"),
            hook_trace_path: directory
                .path()
                .join(pohunek_logging::config::HOOK_TRACE_FILE_NAME),
            config: config.clone(),
        })
        .await
        .expect("bind worker");
        let pty = super::spawn_pty(
            crate::Command {
                program: "/bin/sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    concat!(
                        "trap '' HUP; ",
                        "sh -c 'trap \"\" HUP TERM; while :; do sleep 1; done' & ",
                        "printf 'root-exited\\n'"
                    )
                    .to_owned(),
                ],
                base: crate::EnvBase::Empty,
                env: crate::test_support::child_env(),
                cwd: directory.path().to_path_buf(),
                cols: 80,
                rows: 24,
            },
            &config,
        )
        .expect("spawn PTY");
        let worker_instance_id =
            WorkerInstanceId::new("runtime-bounded-drain").expect("runtime id");
        let mut state = server.shared.state.lock().await;
        state.phase = super::WireRuntimePhase::Running;
        state.worker_instance_id = Some(worker_instance_id.clone());
        state.pty = Some(pty.clone());
        state.stop_grace = config.stop_grace;
        state.journal.phase = super::JournalPhase::Live;
        state.journal.worker_instance_id = Some(worker_instance_id.as_str().to_owned());
        state.journal.child = Some(super::journal_identity(pty.identity()));
        drop(state);
        super::spawn_runtime_monitors(
            std::sync::Arc::clone(&server.shared),
            pty.clone(),
            worker_instance_id,
        );

        pohunek_test_support::wait::wait_until("the natural drain to complete", || async {
            (server.shared.state.lock().await.phase == super::WireRuntimePhase::Exited)
                .then_some(())
        })
        .await;
        let state = server.shared.state.lock().await;
        assert_eq!(
            state
                .journal
                .outcome
                .as_ref()
                .expect("terminal outcome")
                .reason,
            "natural_exit"
        );
        assert!(state.journal.next_output_offset > 0);
        drop(state);
        assert!(
            super::process_start(pty.identity().pid).is_err(),
            "root must be reaped after automatic cleanup"
        );
    }

    // Needs a descendant that keeps the PTY open after the root exits. Darwin
    // revokes the controlling terminal from every holder when the session
    // leader exits, so this scenario exists only on Linux.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn forced_output_completion_does_not_wait_for_the_output_monitor() {
        let directory = BarrierDirectory::new();
        let ready = directory.path().join("held-monitor-ready");
        // The escaped holder never closes the PTY, so the drain expires and the
        // stop force-closes the output whatever these windows are; they only keep
        // the test fast.
        let config = crate::WorkerConfig {
            post_exit_drain_timeout: Duration::from_millis(50),
            stop_grace: Duration::from_millis(50),
            ..crate::WorkerConfig::new()
        };
        let server = super::Server::bind(super::ServerArgs {
            session_id: "s-115".to_owned(),
            worker_id: "worker-held-monitor".to_owned(),
            generation: "abcd2345".to_owned(),
            socket_path: directory.path().join("worker.sock"),
            journal_path: directory.path().join("journal.json"),
            daemon_socket_path: directory.path().join("daemon.sock"),
            hook_trace_path: directory
                .path()
                .join(pohunek_logging::config::HOOK_TRACE_FILE_NAME),
            config: config.clone(),
        })
        .await
        .expect("bind worker");
        let pty = super::spawn_pty(
            crate::Command {
                program: "/bin/sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    concat!(
                        "trap '' HUP; ",
                        "setsid sh -c 'trap \"\" HUP TERM; printf ready > \"$2\"; ",
                        "while [ -d \"$1\" ]; do sleep 0.01; done' escaped \"$1\" \"$2\" & ",
                        "while [ ! -e \"$2\" ]; do sleep 0.01; done; ",
                        "printf 'root-exited\\n'"
                    )
                    .to_owned(),
                    "pohunek-held-monitor".to_owned(),
                    directory.path().to_string_lossy().into_owned(),
                    ready.to_string_lossy().into_owned(),
                ],
                base: crate::EnvBase::Empty,
                env: crate::test_support::child_env(),
                cwd: directory.path().to_path_buf(),
                cols: 80,
                rows: 24,
            },
            &config,
        )
        .expect("spawn PTY");
        server.shared.state.lock().await.stop_grace = config.stop_grace;
        pohunek_test_support::wait::guard("the root to exit", pty.wait_exit())
            .await
            .expect("root exit");

        let (_output_eof_tx, mut output_eof_rx) = tokio::sync::oneshot::channel();
        // A completion that waited for the held output monitor would never
        // return, which the hang guard reports by name.
        let completion = pohunek_test_support::wait::guard(
            "forced output completion",
            super::wait_for_output_completion(&server.shared, &pty, &mut output_eof_rx),
        )
        .await
        .expect("forced completion");

        assert!(matches!(
            completion,
            crate::output::OutputCompletion::ForcedClosed { next_offset }
                if next_offset == pty.output().next_offset() && next_offset > 0
        ));
    }

    // Needs a descendant that keeps the PTY open after the root exits. Darwin
    // revokes the controlling terminal from every holder when the session
    // leader exits, so this scenario exists only on Linux.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn escaped_pty_holder_ends_with_the_exit_and_the_exact_final_output_offset() {
        let directory = BarrierDirectory::new();
        let ready = directory.path().join("escaped-ready");
        // The escaped holder never closes the PTY, so the runtime always ends with
        // the root's exit at the same final offset; these windows only keep the
        // test fast.
        let config = crate::WorkerConfig {
            post_exit_drain_timeout: Duration::from_millis(50),
            stop_grace: Duration::from_millis(50),
            ..crate::WorkerConfig::new()
        };
        let server = super::Server::bind(super::ServerArgs {
            session_id: "s-114".to_owned(),
            worker_id: "worker-escaped-output".to_owned(),
            generation: "abcd2345".to_owned(),
            socket_path: directory.path().join("worker.sock"),
            journal_path: directory.path().join("journal.json"),
            daemon_socket_path: directory.path().join("daemon.sock"),
            hook_trace_path: directory
                .path()
                .join(pohunek_logging::config::HOOK_TRACE_FILE_NAME),
            config: config.clone(),
        })
        .await
        .expect("bind worker");
        let pty = super::spawn_pty(
            crate::Command {
                program: "/bin/sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    concat!(
                        "trap '' HUP; ",
                        "setsid sh -c 'trap \"\" HUP TERM; printf ready > \"$2\"; ",
                        "while [ -d \"$1\" ]; do sleep 0.01; done' escaped \"$1\" \"$2\" & ",
                        "while [ ! -e \"$2\" ]; do sleep 0.01; done; ",
                        "printf 'root-exited\\n'"
                    )
                    .to_owned(),
                    "pohunek-escaped-output".to_owned(),
                    directory.path().to_string_lossy().into_owned(),
                    ready.to_string_lossy().into_owned(),
                ],
                base: crate::EnvBase::Empty,
                env: crate::test_support::child_env(),
                cwd: directory.path().to_path_buf(),
                cols: 80,
                rows: 24,
            },
            &config,
        )
        .expect("spawn PTY");
        let worker_instance_id =
            WorkerInstanceId::new("runtime-escaped-output").expect("runtime id");
        let () = {
            let mut state = server.shared.state.lock().await;
            state.phase = super::WireRuntimePhase::Running;
            state.worker_instance_id = Some(worker_instance_id.clone());
            state.pty = Some(pty.clone());
            state.stop_grace = config.stop_grace;
            state.journal.phase = super::JournalPhase::Live;
            state.journal.worker_instance_id = Some(worker_instance_id.as_str().to_owned());
            state.journal.child = Some(super::journal_identity(pty.identity()));
        };
        super::spawn_runtime_monitors(
            std::sync::Arc::clone(&server.shared),
            pty.clone(),
            worker_instance_id,
        );

        pohunek_test_support::wait::wait_until("the forced-close terminal commit", || async {
            (server.shared.state.lock().await.phase == super::WireRuntimePhase::Exited)
                .then_some(())
        })
        .await;
        let final_offset = pty.output().next_offset();
        let journal = server.shared.journal.load().expect("load terminal journal");
        assert_eq!(journal.phase, super::JournalPhase::Terminal);
        assert_eq!(journal.next_output_offset, final_offset);
        assert!(final_offset > 0);
        let outcome = journal
            .outcome
            .expect("the terminal journal records the exit");
        assert!(outcome.output_forced_closed);
        let state = server.shared.state.lock().await;
        let exit = state.exit.as_ref().expect("the exit is published");
        assert!(exit.output_forced_closed);
    }

    #[tokio::test]
    async fn terminal_commit_retries_before_publishing_exited_state() {
        let (server, pty, _directory) = launch_claim_fixture().await;
        let journal_path = server.shared.journal.path().to_path_buf();
        std::fs::remove_file(&journal_path).expect("remove writable journal fixture");
        std::fs::create_dir(&journal_path).expect("block journal replacement with a directory");
        let exit = crate::Exit {
            exit_code: Some(0),
            signal: None,
            success: true,
        };
        let status = ExitStatus {
            code: Some(0),
            signal: None,
            stopped_by_user: false,
            exited_at_ms: 100,
            output_forced_closed: false,
        };
        let shared = std::sync::Arc::clone(&server.shared);
        let completion = super::retry_runtime_completion(
            &shared,
            &exit,
            &status,
            false,
            "2026-09-14T00:00:00Z",
            42,
        );
        tokio::pin!(completion);
        let terminal_confirmation = super::wait_for_terminal_commit(&shared);
        tokio::pin!(terminal_confirmation);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut completion)
                .await
                .is_err(),
            "failed terminal persistence must remain retryable"
        );
        assert_eq!(
            server.shared.state.lock().await.phase,
            super::WireRuntimePhase::Running,
            "terminal state cannot be public before its journal commit"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut terminal_confirmation)
                .await
                .is_err(),
            "stop confirmation cannot precede durable terminal state"
        );

        std::fs::remove_dir(&journal_path).expect("restore writable journal path");
        let retention =
            pohunek_test_support::wait::guard("the terminal commit to be retried", &mut completion)
                .await
                .expect("completion remains active until commit");
        assert!(!retention.is_zero());
        assert_eq!(
            server.shared.state.lock().await.phase,
            super::WireRuntimePhase::Exited
        );
        let journal = server.shared.journal.load().expect("load terminal journal");
        assert_eq!(journal.phase, super::JournalPhase::Terminal);
        assert_eq!(journal.next_output_offset, 42);
        assert_eq!(
            pohunek_test_support::wait::guard(
                "the terminal confirmation",
                &mut terminal_confirmation
            )
            .await
            .expect("terminal confirmation"),
            status
        );
        let () = {
            let mut state = server.shared.state.lock().await;
            let retry = super::prepare_stop(&mut state).expect("prepare repeated stop");
            let super::StopPreparation::AlreadyCommitted(retry_status) = retry else {
                panic!("repeated stop must reuse the durable terminal commit");
            };
            assert_eq!(retry_status, status);
            assert_eq!(state.phase, super::WireRuntimePhase::Exited);
        };
        pty.stop("test-cleanup", server_stop_grace())
            .await
            .expect("stop fixture PTY");
        drop(server);
    }

    #[test]
    fn known_signal_names_map_to_numbers() {
        assert_eq!(signal_number("Terminated"), Some(libc::SIGTERM));
        assert_eq!(signal_number("Killed"), Some(libc::SIGKILL));
        assert_eq!(signal_number("unknown"), None);
    }

    #[test]
    fn control_input_ids_are_scoped_to_the_active_lease() {
        let namespaced = WriteId::new("input-lease-a-42").expect("namespaced input id");
        assert_eq!(
            control_input_id(&namespaced, "lease-a").expect("matching lease"),
            ControlInputId::Namespaced { sequence: 42 }
        );
        let mismatch = control_input_id(&namespaced, "lease-b").expect_err("old lease mismatch");
        assert_eq!(mismatch.code, ControlCode::IdentityMismatch);

        let legacy = WriteId::new("input-17").expect("legacy input id");
        assert_eq!(
            control_input_id(&legacy, "lease-a").expect("legacy compatibility"),
            ControlInputId::Legacy
        );
    }

    #[test]
    fn attach_input_ids_remain_monotonic_past_dedup_capacity() {
        let stream_id = StreamId::new("a-8192").expect("stream id");
        for sequence in 1..=8_192 {
            let write_id =
                WriteId::new(format!("attach-{stream_id}-{sequence}")).expect("attach input id");
            validate_attach_write_id(&write_id, &stream_id, sequence)
                .expect("next attach sequence");
        }

        let duplicate = WriteId::new(format!("attach-{stream_id}-8192")).expect("duplicate id");
        let error =
            validate_attach_write_id(&duplicate, &stream_id, 8_193).expect_err("old sequence");
        assert_eq!(error.code, ControlCode::InvalidRequest);
    }

    #[test]
    fn attach_snapshot_requires_current_negotiated_capability() {
        let attach = AttachStart {
            dimensions: Some(Dimensions::new(120, 40).expect("dimensions")),
        };
        let mut current = Connection::new(std::sync::Arc::new(
            crate::identity::PeerContext::for_test(pohunek_platform::process::ProcessIdentity {
                pid: std::process::id(),
                start_identity: pohunek_platform::process::StartIdentity::new(1),
            }),
        ));
        current.selected_version = Some(CURRENT_VERSION);
        current.capabilities.push(Capability::AttachSnapshot);
        validate_data_start(&current, StreamMode::Attach, None, Some(&attach))
            .expect("negotiated snapshot attach");

        current.capabilities.clear();
        let unsupported = validate_data_start(&current, StreamMode::Attach, None, Some(&attach))
            .expect_err("missing capability");
        assert_eq!(unsupported.code, ControlCode::InvalidRequest);

        let mut previous = Connection::new(std::sync::Arc::new(
            crate::identity::PeerContext::for_test(pohunek_platform::process::ProcessIdentity {
                pid: std::process::id(),
                start_identity: pohunek_platform::process::StartIdentity::new(1),
            }),
        ));
        previous.selected_version = Some(PREVIOUS_VERSION);
        previous.capabilities.push(Capability::AttachSnapshot);
        validate_data_start(&previous, StreamMode::Attach, None, Some(&attach))
            .expect("previous protocol preserves the established snapshot attach shape");
        previous.capabilities.clear();
        assert!(validate_data_start(&previous, StreamMode::Attach, None, Some(&attach)).is_err());
        assert!(validate_data_start(&current, StreamMode::Detector, None, Some(&attach)).is_err());
    }

    #[test]
    fn observation_capability_starts_at_private_version_four() {
        let version_four = protocol::CONTROL_PLANE_OBSERVATION_VERSION;
        assert!(capabilities(CURRENT_VERSION).contains(&Capability::ControlPlaneObservation));
        assert!(capabilities(version_four).contains(&Capability::ControlPlaneObservation));
        assert!(capabilities(CURRENT_VERSION).contains(&Capability::SubagentObservation));
        assert!(capabilities(PREVIOUS_VERSION).contains(&Capability::SubagentObservation));
        assert!(!capabilities(version_four).contains(&Capability::SubagentObservation));
        assert!(capabilities(version_four).contains(&Capability::AttachSnapshot));
    }

    #[test]
    fn subagent_events_are_hidden_from_version_four_connections() {
        let event = protocol::ControlEvent {
            event_sequence: 1,
            kind: EventKind::SubagentsChanged {
                worker_instance_id: WorkerInstanceId::new("runtime-1").expect("runtime id"),
            },
        };
        let mut previous = Connection::new(std::sync::Arc::new(
            crate::identity::PeerContext::for_test(pohunek_platform::process::ProcessIdentity {
                pid: std::process::id(),
                start_identity: pohunek_platform::process::StartIdentity::new(1),
            }),
        ));
        let version_four = protocol::CONTROL_PLANE_OBSERVATION_VERSION;
        previous.selected_version = Some(version_four);
        previous.capabilities = capabilities(version_four);
        let mut current = Connection::new(std::sync::Arc::new(
            crate::identity::PeerContext::for_test(pohunek_platform::process::ProcessIdentity {
                pid: std::process::id(),
                start_identity: pohunek_platform::process::StartIdentity::new(1),
            }),
        ));
        current.selected_version = Some(CURRENT_VERSION);
        current.capabilities = capabilities(CURRENT_VERSION);

        assert!(!event_visible_to_connection(&event, &previous));
        assert!(event_visible_to_connection(&event, &current));
    }

    fn data_grant(
        owner: crate::LeaseOwner,
        lease_id: LeaseId,
        lease_epoch: u64,
        version: Version,
    ) -> DataGrant {
        DataGrant {
            lease_owner: owner,
            lease_id,
            lease_epoch,
            version,
            expires_at_ms: 100,
            worker_instance_id: WorkerInstanceId::new("runtime-token").expect("runtime id"),
            stream_id: StreamId::new("stream-token").expect("stream id"),
            mode: StreamMode::Detector,
            after_offset: None,
            attach: None,
            observation: None,
        }
    }

    /// Builds the peer context a lease owner describes.
    ///
    /// Data redemption compares the redeeming peer against the owner the grant
    /// was minted for, so a token test needs the matching kernel identity.
    fn owner_peer(owner: &crate::LeaseOwner) -> crate::identity::PeerContext {
        crate::identity::PeerContext::for_test(pohunek_platform::process::ProcessIdentity {
            pid: owner.peer_pid,
            start_identity: pohunek_platform::process::StartIdentity::new(
                owner
                    .peer_start_identity
                    .parse()
                    .expect("decimal start identity"),
            ),
        })
    }

    fn insert_grant(tokens: &mut TokenState, token: DataToken, grant: DataGrant) {
        tokens.insert(token, grant, 0).expect("insert token");
    }

    fn open_header(grant: &DataGrant, token: DataToken, version: Version) -> FrameHeader {
        FrameHeader {
            version,
            stream_id: grant.stream_id.clone(),
            worker_instance_id: grant.worker_instance_id.clone(),
            kind: FrameKind::Open {
                token,
                mode: grant.mode,
                after_offset: grant.after_offset,
                attach: grant.attach.clone(),
            },
        }
    }

    /// A leased connection is re-checked on every request, not only at acquire.
    ///
    /// The lease caches the owner it was granted to, so without a fresh kernel
    /// read a descriptor handed to another process would keep issuing
    /// privileged requests — and keep holding a lease the real controller needs
    /// back. Failing here ends the connection, which releases it.
    #[tokio::test]
    async fn a_leased_connection_is_reauthorized_on_every_request() {
        let owner = crate::LeaseOwner {
            daemon_id: "daemon-reauth".to_owned(),
            peer_pid: std::process::id(),
            peer_start_identity: "7200".to_owned(),
        };
        let shared = launch_claim_fixture().await;
        let (server, _pty, _directory) = shared;

        let mut connection = Connection::new(std::sync::Arc::new(owner_peer(&owner)));
        connection.owner = Some(owner.clone());
        connection.lease_id = Some(LeaseId::new("lease-reauth").expect("lease id"));
        authorize_control_request(&server.shared, &connection)
            .expect("the owning peer stays authorized");

        // A peer whose kernel identity no longer matches the lease owner is the
        // descriptor-handoff shape; it must not be able to keep issuing work.
        let drifted = crate::LeaseOwner {
            peer_pid: owner.peer_pid,
            peer_start_identity: "7201".to_owned(),
            ..owner.clone()
        };
        let mut hijacked = Connection::new(std::sync::Arc::new(owner_peer(&drifted)));
        hijacked.owner = Some(owner);
        hijacked.lease_id = connection.lease_id.clone();
        let error = authorize_control_request(&server.shared, &hijacked)
            .expect_err("a drifted peer must lose the lease");
        assert!(
            matches!(
                error,
                super::WorkerError::PeerIdentity {
                    reason: super::RejectReason::LeaseOwnerMismatch
                }
            ),
            "expected a lease-owner mismatch, got {error:?}"
        );
    }

    /// Output that becomes ready while an attach input frame is only partly
    /// received must not discard the consumed input bytes: the worker keeps its
    /// pending read and still accepts the complete input frame afterwards.
    #[tokio::test]
    async fn attach_input_frame_survives_output_during_its_partial_read() {
        let owner = crate::LeaseOwner {
            daemon_id: "daemon-partial-input".to_owned(),
            peer_pid: std::process::id(),
            peer_start_identity: "7400".to_owned(),
        };
        let (server, pty, _directory) = launch_claim_fixture().await;
        let output = crate::OutputHub::new(64, 64, 2, 10).expect("output hub");
        let subscriber = output.subscribe(Some(0)).expect("live output subscriber");
        let stream_id = StreamId::new("a-partial").expect("stream id");
        let worker_instance_id = WorkerInstanceId::new("runtime-claim").expect("runtime id");
        let write_id =
            WriteId::new(format!("{ATTACH_INPUT_PREFIX}{stream_id}-1")).expect("write id");
        let typed = b"typed-mid-frame";
        let mut input = Vec::new();
        protocol::write_frame(
            &mut input,
            &protocol::DataFrame::new(
                FrameHeader {
                    version: CURRENT_VERSION,
                    stream_id: stream_id.clone(),
                    worker_instance_id: worker_instance_id.clone(),
                    kind: FrameKind::Input {
                        write_id: write_id.clone(),
                    },
                },
                typed.to_vec(),
            )
            .expect("input frame"),
        )
        .await
        .expect("encode input frame");

        let (worker_in, mut client_in) = tokio::io::duplex(ONE_BYTE_BUFFER);
        let (mut worker_out, mut client_out) = tokio::io::duplex(CLIENT_BUFFER);
        let (_lease_tx, mut lease_rx) = watch::channel(0_u64);
        let served_pty = pty.clone();
        let served_owner = owner.clone();
        let stream = tokio::spawn(async move {
            serve_stream_data(
                &server.shared,
                &served_pty,
                worker_in,
                &mut worker_out,
                CURRENT_VERSION,
                &stream_id,
                &worker_instance_id,
                StreamMode::Attach,
                subscriber,
                0,
                &mut lease_rx,
                &owner_peer(&served_owner),
                &served_owner,
            )
            .await
        });

        client_in
            .write_all(&input[..PARTIAL_FRAME_BYTES])
            .await
            .expect("start the input frame");
        output.push(b"live-output").expect("live output");
        let live = protocol::read_frame(&mut client_out)
            .await
            .expect("read live output")
            .expect("worker streams live output");
        assert!(matches!(live.header().kind, FrameKind::Output { .. }));
        assert_eq!(live.payload(), b"live-output");
        let remainder = client_in.write_all(&input[PARTIAL_FRAME_BYTES..]).await;
        let acknowledged = protocol::read_frame(&mut client_out)
            .await
            .expect("read input acknowledgement")
            .expect("worker answers the input frame");
        assert_eq!(
            acknowledged.header().kind,
            FrameKind::InputAck {
                write_id,
                bytes_written: u64::try_from(typed.len()).expect("input length"),
            },
            "a partly read input frame must survive racing output"
        );
        remainder.expect("finish the input frame");

        drop(client_in);
        stream
            .await
            .expect("stream task")
            .expect("client EOF ends the stream cleanly");
        pty.stop("test-cleanup", server_stop_grace())
            .await
            .expect("stop fixture PTY");
    }

    /// An observation stream proves its peer again after it stops waiting.
    ///
    /// The wait can last seconds, so a descriptor handed on in the meantime
    /// would otherwise receive a page of PTY history on the strength of the
    /// redemption that happened before it.
    #[tokio::test]
    async fn a_waiting_observation_reauthorizes_before_releasing_output() {
        let owner = crate::LeaseOwner {
            daemon_id: "daemon-observe".to_owned(),
            peer_pid: std::process::id(),
            peer_start_identity: "7300".to_owned(),
        };
        let (server, pty, _directory) = launch_claim_fixture().await;
        let output = crate::OutputHub::new(64, 64, 2, 10).expect("output hub");
        output
            .push(b"must-not-cross-a-handoff")
            .expect("retained output");
        let (_lease_tx, mut lease_rx) = watch::channel(0_u64);
        let (mut reader, _writer) = tokio::io::duplex(64);
        let mut written = Vec::new();

        // A peer whose kernel identity no longer matches the grant owner is the
        // descriptor-handoff shape.
        let drifted = crate::LeaseOwner {
            peer_start_identity: "7301".to_owned(),
            ..owner.clone()
        };
        let error = super::serve_observation_data(
            &server.shared,
            &pty,
            &mut reader,
            &mut written,
            CURRENT_VERSION,
            &StreamId::new("stream-observe").expect("stream id"),
            &WorkerInstanceId::new("runtime-claim").expect("runtime id"),
            Some(0),
            ObservationGrant {
                max_bytes: 64,
                wait: Duration::from_millis(10),
            },
            0,
            &mut lease_rx,
            &owner_peer(&drifted),
            &owner,
        )
        .await
        .expect_err("a drifted peer must not receive terminal history");
        assert!(
            matches!(
                error,
                super::WorkerError::PeerIdentity {
                    reason: super::RejectReason::LeaseOwnerMismatch
                }
            ),
            "expected a lease-owner mismatch, got {error:?}"
        );
        assert!(
            written.is_empty(),
            "no PTY history may reach a peer that changed"
        );
    }

    /// A data grant is redeemable only by the process that acquired the lease.
    ///
    /// The grant is minted for one kernel-attested peer; a different process —
    /// and a reused process id with a different start identity — must both be
    /// refused, and the refusal must leave the one-use token intact so a
    /// foreign attempt cannot burn a legitimate grant.
    #[test]
    fn data_token_redemption_is_bound_to_the_lease_owning_peer() {
        let leases = crate::ControllerLease::new();
        let owner = crate::LeaseOwner {
            daemon_id: "daemon-bound".to_owned(),
            peer_pid: 4_242,
            peer_start_identity: "7100".to_owned(),
        };
        let lease_id = LeaseId::new("lease-bound").expect("lease id");
        leases
            .acquire(owner.clone(), lease_id.to_string())
            .expect("owner lease");
        let grant = data_grant(owner.clone(), lease_id, 0, CURRENT_VERSION);
        let token = DataToken::new("token-bound").expect("token");
        let header = open_header(&grant, token.clone(), CURRENT_VERSION);
        let tokens = std::sync::Mutex::new(TokenState::new());
        insert_grant(
            &mut tokens.lock().expect("token lock"),
            token.clone(),
            grant,
        );

        let foreign = crate::LeaseOwner {
            peer_pid: owner.peer_pid + 1,
            ..owner.clone()
        };
        let reused_pid = crate::LeaseOwner {
            peer_start_identity: "7101".to_owned(),
            ..owner.clone()
        };
        for (label, impostor) in [
            ("a different process", foreign),
            ("a reused id", reused_pid),
        ] {
            let error = redeem_data_grant(
                &tokens,
                &leases,
                0,
                1,
                &token,
                &header,
                StreamMode::Detector,
                None,
                None,
                &owner_peer(&impostor),
            )
            .expect_err(label);
            assert!(
                matches!(
                    error,
                    super::WorkerError::PeerIdentity {
                        reason: super::RejectReason::LeaseOwnerMismatch
                    }
                ),
                "{label} must be refused as a lease-owner mismatch, got {error:?}"
            );
            assert_eq!(
                tokens.lock().expect("token lock").len(),
                1,
                "{label} must not consume the grant"
            );
        }

        redeem_data_grant(
            &tokens,
            &leases,
            0,
            1,
            &token,
            &header,
            StreamMode::Detector,
            None,
            None,
            &owner_peer(&owner),
        )
        .expect("the lease owner still redeems its own grant");
        assert_eq!(tokens.lock().expect("token lock").len(), 0);
    }

    #[test]
    fn data_token_redemption_rejects_a_released_lease_generation() {
        let leases = crate::ControllerLease::new();
        let first_owner = crate::LeaseOwner {
            daemon_id: "daemon-first".to_owned(),
            peer_pid: 10,
            peer_start_identity: "7001".to_owned(),
        };
        let first_lease = LeaseId::new("lease-first").expect("lease id");
        leases
            .acquire(first_owner.clone(), first_lease.to_string())
            .expect("first lease");
        let stale = data_grant(first_owner.clone(), first_lease.clone(), 0, CURRENT_VERSION);
        let stale_token = DataToken::new("token-stale").expect("token");
        let stale_header = open_header(&stale, stale_token.clone(), CURRENT_VERSION);
        let tokens = std::sync::Mutex::new(TokenState::new());
        insert_grant(
            &mut tokens.lock().expect("token lock"),
            stale_token.clone(),
            stale,
        );

        leases.release_connection(&first_owner);
        let next_owner = crate::LeaseOwner {
            daemon_id: "daemon-next".to_owned(),
            peer_pid: 11,
            peer_start_identity: "7002".to_owned(),
        };
        let next_lease = LeaseId::new("lease-next").expect("lease id");
        leases
            .acquire(next_owner.clone(), next_lease.to_string())
            .expect("replacement lease");
        redeem_data_grant(
            &tokens,
            &leases,
            1,
            1,
            &stale_token,
            &stale_header,
            StreamMode::Detector,
            None,
            None,
            &owner_peer(&first_owner),
        )
        .expect_err("stale lease token");
        assert_eq!(tokens.lock().expect("token lock").len(), 0);
    }

    #[test]
    fn abandoned_expired_grant_releases_the_single_vault_capacity() {
        let owner = crate::LeaseOwner {
            daemon_id: "daemon-capacity".to_owned(),
            peer_pid: 13,
            peer_start_identity: "7003".to_owned(),
        };
        let lease_id = LeaseId::new("lease-capacity").expect("lease id");
        let mut abandoned = data_grant(owner.clone(), lease_id.clone(), 0, CURRENT_VERSION);
        abandoned.expires_at_ms = 10;
        let mut tokens = TokenState::with_maximum(1).expect("bounded token state");
        tokens
            .insert(
                DataToken::new("token-abandoned").expect("token"),
                abandoned,
                0,
            )
            .expect("issue abandoned token");

        let mut replacement = data_grant(owner, lease_id, 0, CURRENT_VERSION);
        replacement.expires_at_ms = 20;
        let replacement_token = DataToken::new("token-replacement").expect("token");
        let full = tokens
            .insert(replacement_token.clone(), replacement.clone(), 9)
            .expect_err("live abandoned token occupies capacity");
        assert_eq!(full, super::TokenStateError::Full { maximum: 1 });

        tokens
            .insert(replacement_token, replacement, 10)
            .expect("expired orphan is purged before replacement issue");
        assert_eq!(tokens.len(), 1);
    }

    #[test]
    fn data_token_redemption_enforces_version_one_shot_and_validity() {
        let leases = crate::ControllerLease::new();
        let owner = crate::LeaseOwner {
            daemon_id: "daemon-current".to_owned(),
            peer_pid: 12,
            peer_start_identity: "7004".to_owned(),
        };
        let lease_id = LeaseId::new("lease-current").expect("lease id");
        leases
            .acquire(owner.clone(), lease_id.to_string())
            .expect("current lease");
        let current = data_grant(owner.clone(), lease_id, 0, CURRENT_VERSION);
        let current_token = DataToken::new("token-current").expect("token");
        let wrong_version = open_header(&current, current_token.clone(), PREVIOUS_VERSION);
        let tokens = std::sync::Mutex::new(TokenState::new());
        insert_grant(
            &mut tokens.lock().expect("token lock"),
            current_token.clone(),
            current.clone(),
        );
        redeem_data_grant(
            &tokens,
            &leases,
            0,
            1,
            &current_token,
            &wrong_version,
            StreamMode::Detector,
            None,
            None,
            &owner_peer(&owner),
        )
        .expect_err("v3 header for v4 grant");
        assert_eq!(tokens.lock().expect("token lock").len(), 0);

        let valid_token = DataToken::new("token-valid").expect("token");
        insert_grant(
            &mut tokens.lock().expect("token lock"),
            valid_token.clone(),
            current.clone(),
        );
        let current_header = open_header(&current, valid_token.clone(), CURRENT_VERSION);
        redeem_data_grant(
            &tokens,
            &leases,
            0,
            1,
            &valid_token,
            &current_header,
            StreamMode::Detector,
            None,
            None,
            &owner_peer(&owner),
        )
        .expect("matching token");
        redeem_data_grant(
            &tokens,
            &leases,
            0,
            1,
            &valid_token,
            &current_header,
            StreamMode::Detector,
            None,
            None,
            &owner_peer(&owner),
        )
        .expect_err("one-shot token reuse");
        let unknown = DataToken::new("token-unknown").expect("token");
        let unknown_header = open_header(&current, unknown.clone(), CURRENT_VERSION);
        redeem_data_grant(
            &tokens,
            &leases,
            0,
            1,
            &unknown,
            &unknown_header,
            StreamMode::Detector,
            None,
            None,
            &owner_peer(&owner),
        )
        .expect_err("unknown token");
    }

    #[tokio::test]
    async fn terminal_snapshot_dimension_and_full_response_bounds_are_exact() {
        let snapshot = WireTerminalSnapshot {
            watermark: 42,
            dimensions: Dimensions::new(10, 2).expect("valid dimensions"),
            cursor: Cursor {
                column: 1,
                row: 1,
                visible: true,
            },
            alternate_screen: true,
            title: Some("Unicode 界".to_owned()),
            progress: Some("50%".to_owned()),
            visible_lines: vec!["wide 界".to_owned(), "lossy �".to_owned()],
        };
        let response = ControlResponse {
            request_id: RequestId::new("snapshot-boundary").expect("request id"),
            kind: ResponseKind::TerminalSnapshot {
                worker_instance_id: WorkerInstanceId::new("runtime-snapshot").expect("runtime id"),
                snapshot: Box::new(snapshot.clone()),
            },
        };
        let serialized = serde_json::to_vec(&response)
            .expect("serialize response")
            .len();
        let exact = crate::WorkerConfig {
            max_snapshot_rows: 2,
            max_snapshot_columns: 10,
            max_snapshot_bytes: serialized,
            control_line_bytes: serialized,
            ..crate::WorkerConfig::new()
        };
        validate_terminal_snapshot_dimensions(&snapshot, &exact).expect("exact dimensions");
        validate_terminal_snapshot_response(&response, &exact).expect("exact response bound");

        let (writer, _reader) = tokio::io::duplex(serialized + 1);
        let mut exact_writer =
            protocol::ControlWriter::with_maximum(writer, serialized).expect("exact writer limit");
        exact_writer
            .write(&ControlMessage::Response(response.clone()))
            .await
            .expect("actual writer accepts exact response");

        let row_over = crate::WorkerConfig {
            max_snapshot_rows: 1,
            ..exact.clone()
        };
        let error = validate_terminal_snapshot_dimensions(&snapshot, &row_over)
            .expect_err("row above configured maximum");
        assert_eq!(error.code, ControlCode::ObservationLimitExceeded);
        assert!(!error.message.contains("Unicode"));

        let column_over = crate::WorkerConfig {
            max_snapshot_columns: 9,
            ..exact.clone()
        };
        let error = validate_terminal_snapshot_dimensions(&snapshot, &column_over)
            .expect_err("column above configured maximum");
        assert_eq!(error.code, ControlCode::ObservationLimitExceeded);

        let mut oversized = response.clone();
        let ResponseKind::TerminalSnapshot { snapshot, .. } = &mut oversized.kind else {
            panic!("terminal response");
        };
        snapshot.title = Some(format!(
            "{}x",
            snapshot.title.as_deref().unwrap_or_default()
        ));
        assert_eq!(
            serde_json::to_vec(&oversized)
                .expect("serialize oversized response")
                .len(),
            serialized + 1
        );
        let serialized_over = crate::WorkerConfig {
            max_snapshot_bytes: serialized,
            ..exact.clone()
        };
        let error = validate_terminal_snapshot_response(&oversized, &serialized_over)
            .expect_err("serialized size above configured maximum");
        assert_eq!(error.code, ControlCode::ObservationLimitExceeded);
        assert!(!error.message.contains("Unicode"));

        let (writer, _reader) = tokio::io::duplex(serialized + 2);
        let mut bounded_writer =
            protocol::ControlWriter::with_maximum(writer, serialized).expect("bounded writer");
        let writer_error = bounded_writer
            .write(&ControlMessage::Response(oversized))
            .await
            .expect_err("actual writer rejects response above limit");
        assert!(matches!(
            writer_error,
            protocol::ControlCodecError::LineTooLong { .. }
        ));
    }

    #[test]
    fn observation_request_wait_and_byte_bounds_are_exact() {
        let config = crate::WorkerConfig {
            history_bytes: 1_024,
            max_observation_wait: Duration::from_millis(250),
            ..crate::WorkerConfig::new()
        };
        assert_eq!(
            validate_observation_request(1_024, 250, &config).expect("exact observation limits"),
            (1_024, Duration::from_millis(250))
        );
        let bytes = validate_observation_request(1_025, 250, &config)
            .expect_err("byte limit above maximum");
        assert_eq!(bytes.code, ControlCode::ObservationLimitExceeded);
        let wait = validate_observation_request(1_024, 251, &config)
            .expect_err("wait limit above maximum");
        assert_eq!(wait.code, ControlCode::ObservationLimitExceeded);
    }

    #[test]
    fn timeout_is_derived_from_the_final_observation_page() {
        let page = crate::ObservationPage {
            history_start_offset: 0,
            start_offset: 10,
            next_offset: 10,
            runtime_end_offset: 10,
            bytes: Vec::new(),
            gap: None,
            has_more: false,
            exited: false,
        };
        assert!(observation_timed_out(Some(10), &page));

        let with_output = crate::ObservationPage {
            start_offset: 10,
            next_offset: 11,
            runtime_end_offset: 11,
            bytes: vec![b'x'],
            ..page.clone()
        };
        assert!(!observation_timed_out(Some(10), &with_output));

        let exited = crate::ObservationPage {
            exited: true,
            ..page
        };
        assert!(!observation_timed_out(Some(10), &exited));
    }

    #[tokio::test]
    async fn waiting_observation_releases_on_disconnect_lease_and_shutdown() {
        let output = crate::OutputHub::new(64, 64, 2, 10).expect("output hub");
        let shutdown = CancellationToken::new();

        let (mut disconnected_reader, disconnected_writer) = tokio::io::duplex(64);
        drop(disconnected_writer);
        let (_lease_tx, mut lease_rx) = watch::channel(0_u64);
        let disconnected = tokio::time::timeout(
            Duration::from_millis(100),
            await_observation_page(
                &output,
                Some(0),
                ObservationGrant {
                    max_bytes: 64,
                    wait: Duration::from_secs(10),
                },
                &shutdown,
                0,
                &mut lease_rx,
                &mut disconnected_reader,
            ),
        )
        .await
        .expect("disconnect releases promptly")
        .expect("wait result");
        assert_eq!(disconnected, ObservationWaitOutcome::Disconnected);

        let released_output = crate::OutputHub::new(64, 64, 2, 10).expect("released output");
        released_output
            .push(b"must-not-cross-lease-death")
            .expect("retained output");
        let (mut lease_reader, _lease_writer) = tokio::io::duplex(64);
        let (lease_tx, mut lease_rx) = watch::channel(0_u64);
        // The receiver is registered before redemption. Releasing here models
        // the exact handoff gap before observation wait registration.
        lease_tx.send_replace(1);
        let released = await_observation_page(
            &released_output,
            Some(0),
            ObservationGrant {
                max_bytes: 64,
                wait: Duration::from_secs(10),
            },
            &shutdown,
            0,
            &mut lease_rx,
            &mut lease_reader,
        )
        .await
        .expect("lease result");
        assert_eq!(released, ObservationWaitOutcome::LeaseReleased);

        let (mut timeout_reader, _timeout_writer) = tokio::io::duplex(64);
        let (_lease_tx, mut lease_rx) = watch::channel(0_u64);
        let timed_out = await_observation_page(
            &output,
            Some(0),
            ObservationGrant {
                max_bytes: 64,
                wait: Duration::from_millis(1),
            },
            &shutdown,
            0,
            &mut lease_rx,
            &mut timeout_reader,
        )
        .await
        .expect("timeout result");
        assert!(matches!(
            timed_out,
            ObservationWaitOutcome::Page {
                timed_out: true,
                ..
            }
        ));

        let (mut shutdown_reader, _shutdown_writer) = tokio::io::duplex(64);
        let (_lease_tx, mut lease_rx) = watch::channel(0_u64);
        shutdown.cancel();
        let stopped = await_observation_page(
            &output,
            Some(0),
            ObservationGrant {
                max_bytes: 64,
                wait: Duration::from_secs(10),
            },
            &shutdown,
            0,
            &mut lease_rx,
            &mut shutdown_reader,
        )
        .await
        .expect("shutdown result");
        assert_eq!(stopped, ObservationWaitOutcome::Shutdown);
    }

    #[tokio::test]
    async fn prefix_stream_replays_consumed_classifier_byte() {
        let (left, mut right) = tokio::io::duplex(16);
        let mut stream = PrefixStream::new(b'a', left);
        right.write_all(b"bc").await.expect("write");

        let mut output = [0_u8; 3];
        stream.read_exact(&mut output).await.expect("read");
        assert_eq!(&output, b"abc");
    }

    #[tokio::test]
    async fn data_stream_failure_is_emitted_as_a_typed_error_frame() {
        let stream_id = StreamId::new("a-error").expect("stream id");
        let worker_instance_id = WorkerInstanceId::new("runtime-error").expect("runtime id");
        let (mut reader, mut writer) = tokio::io::duplex(4_096);
        let expected = ControlError {
            code: ControlCode::RuntimeFault,
            message: "PTY input write failed".to_owned(),
            retryable: true,
        };

        write_data_error(
            &mut writer,
            CURRENT_VERSION,
            &stream_id,
            &worker_instance_id,
            expected.clone(),
        )
        .await
        .expect("write typed stream error");
        let frame = protocol::read_frame(&mut reader)
            .await
            .expect("read error frame")
            .expect("error frame");

        assert_eq!(frame.header().stream_id, stream_id);
        assert_eq!(frame.header().worker_instance_id, worker_instance_id);
        assert_eq!(frame.header().kind, FrameKind::Error { error: expected });
    }

    #[tokio::test]
    async fn forced_output_close_is_framed_as_a_runtime_fault() {
        let stream_id = StreamId::new("forced-close").expect("stream id");
        let worker_instance_id = WorkerInstanceId::new("runtime-forced-close").expect("runtime id");
        let (mut reader, mut writer) = tokio::io::duplex(4_096);

        write_runtime_output_event(
            &mut writer,
            CURRENT_VERSION,
            &stream_id,
            &worker_instance_id,
            1_024,
            crate::OutputEvent::Exit { next_offset: 42 },
            true,
        )
        .await
        .expect("write forced-close fault");
        let frame = protocol::read_frame(&mut reader)
            .await
            .expect("read forced-close frame")
            .expect("forced-close frame");

        let FrameKind::Error { error } = &frame.header().kind else {
            panic!("forced close must not be framed as normal runtime exit");
        };
        assert_eq!(error.code, ControlCode::RuntimeFault);
        assert!(!error.retryable);
        assert!(error.message.contains("force-closed"));
    }

    #[tokio::test]
    async fn oversized_replay_is_framed_with_exact_contiguous_offsets() {
        let payload = (0..MAX_DATA_PAYLOAD_BYTES + OVERSIZED_PAYLOAD_EXTRA)
            .map(|index| u8::try_from(index % 251).expect("value fits u8"))
            .collect::<Vec<_>>();
        let stream_id = StreamId::new("stream-1").expect("valid stream");
        let worker_instance_id = WorkerInstanceId::new("runtime-1").expect("valid runtime");
        let (mut reader, mut writer) = tokio::io::duplex(payload.len() * 2);

        write_output_chunks(
            &mut writer,
            CURRENT_VERSION,
            &stream_id,
            &worker_instance_id,
            MAX_DATA_PAYLOAD_BYTES,
            crate::OutputChunk {
                offset: 41,
                bytes: payload.clone(),
            },
            true,
        )
        .await
        .expect("write bounded replay");

        let mut expected_offset = 41_u64;
        let mut replay = Vec::with_capacity(payload.len());
        while replay.len() < payload.len() {
            let frame = protocol::read_frame(&mut reader)
                .await
                .expect("read replay frame")
                .expect("replay frame");
            let (header, bytes) = frame.into_parts();
            assert_eq!(
                header.kind,
                FrameKind::Replay {
                    offset: expected_offset
                }
            );
            assert!(bytes.len() <= MAX_DATA_PAYLOAD_BYTES);
            expected_offset += u64::try_from(bytes.len()).expect("frame length fits u64");
            replay.extend_from_slice(&bytes);
        }

        assert_eq!(replay, payload);
    }

    #[tokio::test]
    async fn observation_page_uses_exact_contiguous_multi_frame_replay() {
        let payload = (0..MAX_DATA_PAYLOAD_BYTES + OVERSIZED_PAYLOAD_EXTRA)
            .map(|index| u8::try_from(index % 251).expect("value fits u8"))
            .collect::<Vec<_>>();
        let stream_id = StreamId::new("observation-1").expect("valid stream");
        let worker_instance_id = WorkerInstanceId::new("runtime-1").expect("valid runtime");
        let (mut reader, mut writer) = tokio::io::duplex(payload.len() * 2);
        let end = u64::try_from(payload.len()).expect("payload fits u64");

        write_observation_page(
            &mut writer,
            CURRENT_VERSION,
            &stream_id,
            &worker_instance_id,
            MAX_DATA_PAYLOAD_BYTES,
            crate::ObservationPage {
                history_start_offset: 0,
                start_offset: 0,
                next_offset: end,
                runtime_end_offset: end,
                bytes: payload.clone(),
                gap: None,
                has_more: false,
                exited: false,
            },
            false,
        )
        .await
        .expect("write observation page");

        let start = protocol::read_frame(&mut reader)
            .await
            .expect("read metadata")
            .expect("metadata frame");
        assert!(matches!(
            start.header().kind,
            FrameKind::ObservationStart {
                start_offset: 0,
                next_offset,
                timed_out: false,
                ..
            } if next_offset == end
        ));
        let mut bytes = Vec::with_capacity(payload.len());
        let mut offset = 0_u64;
        let mut frame_lengths = Vec::new();
        while bytes.len() < payload.len() {
            let frame = protocol::read_frame(&mut reader)
                .await
                .expect("read payload")
                .expect("payload frame");
            assert!(
                matches!(frame.header().kind, FrameKind::Replay { offset: actual } if actual == offset)
            );
            assert!(frame.payload().len() <= MAX_DATA_PAYLOAD_BYTES);
            frame_lengths.push(frame.payload().len());
            offset += u64::try_from(frame.payload().len()).expect("frame length fits u64");
            bytes.extend_from_slice(frame.payload());
        }
        let close = protocol::read_frame(&mut reader)
            .await
            .expect("read close")
            .expect("close frame");
        assert_eq!(
            close.header().kind,
            FrameKind::Close {
                reason: protocol::CloseReason::ObservationComplete
            }
        );
        assert_eq!(
            frame_lengths,
            vec![MAX_DATA_PAYLOAD_BYTES, OVERSIZED_PAYLOAD_EXTRA]
        );
        assert_eq!(bytes, payload);
    }

    #[tokio::test]
    async fn oversized_terminal_repaint_is_framed_without_byte_changes() {
        let ansi = (0..MAX_DATA_PAYLOAD_BYTES + OVERSIZED_PAYLOAD_EXTRA)
            .map(|index| u8::try_from(index % 251).expect("value fits u8"))
            .collect::<Vec<_>>();
        let snapshot = WireTerminalSnapshot {
            watermark: 42,
            dimensions: Dimensions::new(10, 2).expect("valid dimensions"),
            cursor: Cursor {
                column: 0,
                row: 0,
                visible: true,
            },
            alternate_screen: false,
            title: None,
            progress: None,
            visible_lines: vec!["bounded repaint".to_owned()],
        };
        let stream_id = StreamId::new("stream-1").expect("valid stream");
        let worker_instance_id = WorkerInstanceId::new("runtime-1").expect("valid runtime");
        let (mut reader, mut writer) = tokio::io::duplex(ansi.len() * 2);

        write_terminal_chunks(
            &mut writer,
            CURRENT_VERSION,
            &stream_id,
            &worker_instance_id,
            MAX_DATA_PAYLOAD_BYTES,
            &snapshot,
            &ansi,
        )
        .await
        .expect("write bounded terminal repaint");

        let mut repaint = Vec::with_capacity(ansi.len());
        while repaint.len() < ansi.len() {
            let frame = protocol::read_frame(&mut reader)
                .await
                .expect("read terminal snapshot frame")
                .expect("terminal snapshot frame");
            let (header, bytes) = frame.into_parts();
            assert!(matches!(
                header.kind,
                FrameKind::TerminalSnapshot {
                    snapshot: ref framed
                } if framed == &snapshot
            ));
            assert!(bytes.len() <= MAX_DATA_PAYLOAD_BYTES);
            repaint.extend_from_slice(&bytes);
        }

        assert_eq!(repaint, ansi);
    }

    /// Binds a worker below a fresh private child of `root`, which tempfile
    /// may create with a umask-derived mode the trusted filesystem rejects.
    async fn environment_fixture(
        root: &std::path::Path,
        generation: &str,
    ) -> Result<super::Server, crate::WorkerError> {
        let directory = root.join("fixture");
        super::Server::bind(super::ServerArgs {
            session_id: "s-114".to_owned(),
            worker_id: "worker-environment".to_owned(),
            generation: generation.to_owned(),
            socket_path: directory.join("worker.sock"),
            journal_path: directory.join("journal.json"),
            daemon_socket_path: directory.join("daemon.sock"),
            hook_trace_path: directory.join(pohunek_logging::config::HOOK_TRACE_FILE_NAME),
            config: crate::WorkerConfig::new(),
        })
        .await
    }

    fn environment_initialize(
        base_environment: Option<protocol::BaseEnv>,
        profile: &[(&str, &str)],
    ) -> protocol::Initialize {
        protocol::Initialize {
            session_id: protocol::SessionId::new("s-114").expect("session id"),
            daemon_instance_id: Some(
                protocol::DaemonId::new("daemon-environment").expect("daemon id"),
            ),
            transaction_id: protocol::TransactionId::new("create-environment")
                .expect("transaction id"),
            expected_worker_id: protocol::WorkerId::new("worker-environment").expect("worker id"),
            launch: protocol::LaunchIdentity {
                agent: "codex".to_owned(),
                agent_base: "codex".to_owned(),
                reference_kind: Some("id".to_owned()),
            },
            executable: std::path::PathBuf::from("/usr/bin/env"),
            arguments: Vec::new(),
            cwd: std::path::PathBuf::from("/"),
            dimensions: Dimensions::new(80, 24).expect("dimensions"),
            environment: protocol::SecretEnv::new(
                profile
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
            )
            .expect("profile environment"),
            base_environment,
            limits: protocol::InitializeLimits::new(1024, 1024, 32, 60_000).expect("limits"),
            stop_policy: protocol::StopPolicy::new(500).expect("stop policy"),
            hook_protocol_version: Version::new(1).expect("hook version"),
            public_protocol_version: 7,
            hook_schema: None,
            input_policy: Some(protocol::InputPolicy::new(500).expect("input policy")),
        }
    }

    fn base_environment(pairs: &[(&str, &str)]) -> protocol::BaseEnv {
        protocol::BaseEnv::new(
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        )
        .expect("base environment")
    }

    #[tokio::test]
    async fn profile_environment_may_override_the_worker_term() {
        let directory = pohunek_test_support::tempdir().expect("fixture directory");
        let server = environment_fixture(directory.path(), "abcd2345")
            .await
            .expect("bind worker");
        let initialize = environment_initialize(Some(base_environment(&[])), &[("TERM", "screen")]);
        let worker_instance_id = WorkerInstanceId::new("runtime-environment").expect("runtime id");

        let command =
            super::command_from_initialize(&server.shared, &initialize, &worker_instance_id);

        assert!(command
            .env
            .contains(&("TERM".to_owned(), "screen".to_owned())));
    }

    #[tokio::test]
    async fn version_five_child_inherits_without_service_manager_variables() {
        let directory = pohunek_test_support::tempdir().expect("fixture directory");
        let server = environment_fixture(directory.path(), "abcd2345")
            .await
            .expect("bind worker");
        let initialize = environment_initialize(
            None,
            &[
                ("PROFILE_TOKEN", "profile-secret"),
                ("NOTIFY_SOCKET", "/run/profile-notify"),
                ("POHUNEK_BOOTSTRAP_TOKEN", "token"),
            ],
        );
        let worker_instance_id = WorkerInstanceId::new("runtime-environment").expect("runtime id");

        let command =
            super::command_from_initialize(&server.shared, &initialize, &worker_instance_id);

        assert_eq!(command.base, crate::EnvBase::Inherited);
        let names = command
            .env
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"PROFILE_TOKEN"));
        assert!(!names.contains(&"TERM"));
        assert!(!names.contains(&"NOTIFY_SOCKET"));
        assert!(!names.contains(&"POHUNEK_BOOTSTRAP_TOKEN"));
    }

    #[tokio::test]
    async fn bind_journals_the_worker_executable_version_and_generation() {
        let directory = pohunek_test_support::tempdir().expect("fixture directory");

        let server = environment_fixture(directory.path(), "mzxw6ytb")
            .await
            .expect("bind worker");

        let journal = crate::Journal::new(directory.path().join("fixture/journal.json"))
            .load()
            .expect("load bootstrap journal");
        assert_eq!(
            journal.origin,
            crate::WorkerOrigin {
                executable: std::env::current_exe().expect("test executable"),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                generation: "mzxw6ytb".to_owned(),
            }
        );
        assert!(journal.origin.executable.is_absolute());
        drop(server);
    }

    #[tokio::test]
    async fn bind_rejects_an_invalid_generation_before_touching_disk() {
        let directory = pohunek_test_support::tempdir().expect("fixture directory");

        let error = environment_fixture(directory.path(), "NOT-VALID")
            .await
            .expect_err("invalid generation must fail");

        assert!(matches!(error, crate::WorkerError::InvalidGeneration(_)));
        assert!(!directory.path().join("fixture").exists());
    }

    /// How long the lock holder watches for a child while `Starting`.
    ///
    /// Long enough for `/bin/sh` to start and write its marker if the child
    /// were spawned outside the lock; a shorter wait only lowers the test's
    /// sensitivity, never makes it fail spuriously.
    const STARTING_CHILD_WATCH: Duration = Duration::from_millis(500);

    /// Interval between checks for the child's marker file.
    const MARKER_POLL: Duration = Duration::from_millis(5);

    /// A runtime's child never runs while the worker is observably `Starting`.
    ///
    /// Hook claims are admitted only in `Running`, and a child reports its
    /// identity as soon as it runs. A child spawned before the `Running`
    /// commit could have that report rejected as `phase_not_running` and exit
    /// on the refusal, so the spawn and the commit share one state-lock hold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initialize_spawns_the_child_only_under_the_running_commit() {
        let directory = pohunek_test_support::tempdir().expect("fixture directory");
        let server = environment_fixture(directory.path(), "abcd2345")
            .await
            .expect("bind worker");
        let marker = directory.path().join("child-started");
        let mut initialize = environment_initialize(
            Some(base_environment(&[("PATH", "/usr/bin:/bin")])),
            &[("CHILD_MARKER", marker.to_str().expect("UTF-8 marker path"))],
        );
        initialize.executable = std::path::PathBuf::from("/bin/sh");
        initialize.arguments = vec![
            "-c".to_owned(),
            ": > \"$CHILD_MARKER\"; read -r line".to_owned(),
        ];
        let owner = crate::LeaseOwner {
            daemon_id: "daemon-initialize".to_owned(),
            peer_pid: std::process::id(),
            peer_start_identity: "7400".to_owned(),
        };
        let lease_id = LeaseId::new("lease-initialize").expect("lease id");
        server
            .shared
            .lease
            .acquire(owner.clone(), lease_id.as_str().to_owned())
            .expect("acquire lease");
        let mut connection = Connection::new(std::sync::Arc::new(owner_peer(&owner)));
        connection.owner = Some(owner);
        connection.lease_id = Some(lease_id.clone());
        connection.selected_version = Some(CURRENT_VERSION);

        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
        *server
            .shared
            .starting_gate
            .lock()
            .expect("starting gate lock") = Some(super::StartingGate {
            reached: reached_tx,
            proceed: proceed_rx,
        });
        let shared = std::sync::Arc::clone(&server.shared);
        let initializing = tokio::spawn(async move {
            super::initialize_request(&shared, &connection, &lease_id, initialize).await
        });

        // `initialize` is now between the durable `Starting` commit and the
        // child spawn, with the state lock free.
        pohunek_test_support::wait::guard("initialize to commit Starting", reached_rx)
            .await
            .expect("initialize reaches the Starting gate");
        let starting = server.shared.state.lock().await;
        assert_eq!(starting.phase, super::WireRuntimePhase::Starting);
        assert!(starting.pty.is_none(), "no child exists while `Starting`");
        assert!(!marker.exists(), "no child has run while `Starting`");
        proceed_tx.send(()).expect("resume initialize");
        // `initialize` now waits for the state lock this test holds, and the
        // child can only be spawned under that lock.
        let deadline = tokio::time::Instant::now() + STARTING_CHILD_WATCH;
        while tokio::time::Instant::now() < deadline {
            assert!(
                !marker.exists(),
                "the child ran while the worker was observably `Starting`"
            );
            tokio::time::sleep(MARKER_POLL).await;
        }
        drop(starting);

        let initialized = pohunek_test_support::wait::guard("initialize to finish", initializing)
            .await
            .expect("initialize task")
            .expect("initialize runtime");
        assert!(matches!(initialized, ResponseKind::Initialized { .. }));
        // The `Running` commit and the spawn shared one lock hold: once the
        // lock is free, the worker is `Running` and the child starts.
        assert_eq!(
            server.shared.state.lock().await.phase,
            super::WireRuntimePhase::Running
        );
        pohunek_test_support::wait::wait_until("the child to start", || async {
            marker.exists().then_some(())
        })
        .await;
        let pty = server
            .shared
            .state
            .lock()
            .await
            .pty
            .clone()
            .expect("running PTY");
        pty.stop("test-cleanup", server_stop_grace())
            .await
            .expect("stop child");
        // The runtime monitors journal the terminal state in the background;
        // the fixture directory must outlive that write.
        pohunek_test_support::wait::wait_until("the worker to journal the child exit", || async {
            (server.shared.state.lock().await.phase == super::WireRuntimePhase::Exited)
                .then_some(())
        })
        .await;
    }
}
