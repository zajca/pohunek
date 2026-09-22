//! Serves the private daemon-worker Unix protocol.

// Rust guideline compliant 2026-09-22

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pohunek_paths::{validate_socket_path, Platform, SocketKind};
use pohunek_platform::filesystem::{
    AdvisoryLock, EntryIdentity, EntryKind, FsError, MoveOutcome, StageOutcome, TrustedDir,
};
use pohunek_platform::process::{
    HostInspector, ProcessIdentity as OsProcessIdentity, ProcessInspector, StartIdentity,
};
use pohunek_worker_protocol as protocol;
use protocol::{
    ActiveIdentityClaim, AttachStart, Capability, CloseReason, ControlCode, ControlError,
    ControlEvent, ControlMessage, ControlReader, ControlRequest, ControlResponse, ControlWriter,
    Cursor, DaemonId, DataFrame, DataToken, Dimensions, EventKind, ExitStatus, FrameHeader,
    FrameKind, Initialize, InspectSnapshot, LeaseChallenge, LeaseId, OutputGap,
    ProcessIdentity as WireProcessIdentity, ReleasedIdentityClaim, ReportedLaunchIdentity,
    RequestKind, ResponseKind, RuntimeId, RuntimePhase as WireRuntimePhase, RuntimeScope,
    SessionId, StreamId, StreamMode, SubagentPhase as WireSubagentPhase, SubagentSnapshot,
    TerminalSnapshot as WireTerminalSnapshot, TransactionId, Version, WorkerId, WriteAck, WriteId,
    ATTACH_SNAPSHOT_VERSION, SUPPORTED_RANGE,
};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, oneshot, watch, Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;
use tracing::{event, Level};

use crate::identity::{PeerContext, RejectReason};
use crate::journal::{
    ActiveIdentity, ChildIdentity, JournalRecord, LaunchIdentity, PendingLaunchClaim,
    ReleasedIdentity, RuntimeOutcome, RuntimePhase as JournalPhase,
    SubagentPhase as JournalSubagentPhase, SubagentRecord,
};
use crate::launch::{self, LaunchClaimStatus};
use crate::output::OutputCompletion;
use crate::{
    Command, ControllerLease, Exit, InputFragment, InputPlan, Journal, LeaseError, LeaseOwner,
    OutputChunk, OutputEvent, ProcessIdentity, PtyError, PtyOwner, WorkerConfig, WorkerError,
};

/// Number of worker events buffered per control connection.
const EVENT_BUFFER: usize = 256;
/// Maximum simultaneously running subagents accepted from provider hooks.
const ACTIVE_SUBAGENT_CAPACITY: usize = 64;
/// Recent terminal subagents retained for reconnecting clients.
const TERMINAL_SUBAGENT_CAPACITY: usize = 32;
/// Delay between terminal-journal retries after a transient storage failure.
///
/// A quarter second keeps the runtime publicly draining without either a busy
/// loop or a long recovery delay once the journal becomes writable again.
const TERMINAL_JOURNAL_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Maximum accepted provider-native subagent identifier length.
const MAX_SUBAGENT_ID_BYTES: usize = 256;
/// Maximum accepted provider-defined subagent type length.
const MAX_SUBAGENT_TYPE_BYTES: usize = 128;
/// Maximum outstanding one-use data tokens per worker.
const DATA_TOKEN_CAPACITY: usize = 4_096;
/// Entropy bytes used for opaque worker credentials and runtime IDs.
const RANDOM_BYTES: usize = 16;
/// Prefix shared by daemon-generated control input identifiers.
const CONTROL_INPUT_PREFIX: &str = "input-";
/// Prefix shared by stream-scoped raw attach input identifiers.
const ATTACH_INPUT_PREFIX: &str = "attach-";
/// Environment variables inherited from the systemd worker but never the child.
const WORKER_ONLY_ENV: [&str; 5] = [
    "NOTIFY_SOCKET",
    "WATCHDOG_PID",
    "WATCHDOG_USEC",
    "POHUNEK_CONTROLLER_TOKEN",
    "POHUNEK_BOOTSTRAP_TOKEN",
];
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
    /// Owner-private worker socket.
    pub socket_path: PathBuf,
    /// Worker-owned durable journal.
    pub journal_path: PathBuf,
    /// Stable daemon socket exposed to notification hooks.
    pub daemon_socket_path: PathBuf,
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
        let temporary = tempfile::tempdir().expect("create worker socket guard fixture");
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
        let session_id = SessionId::new(&args.session_id)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let worker_id = WorkerId::new(&args.worker_id)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        let prepared_socket = prepare_socket(&args.socket_path, platform).await?;
        let longest_bind_path = prepared_socket.directory.path().join(".s0000000000000000");
        validate_socket_path(&longest_bind_path, platform, SocketKind::Worker)?;
        let (bind_name, std_listener, socket_identity) = prepared_socket
            .directory
            .bind_unix_listener_staged(".s", SOCKET_MODE)?;
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
        let record = JournalRecord::bootstrap(
            args.session_id,
            args.worker_id,
            std::process::id(),
            worker_start.to_string(),
            boot_identity.to_string(),
            (
                u16::try_from(SUPPORTED_RANGE.minimum().get()).unwrap_or(u16::MAX),
                u16::try_from(SUPPORTED_RANGE.maximum().get()).unwrap_or(u16::MAX),
            ),
            timestamp(),
        );
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
            config: args.config,
            journal,
            state: AsyncMutex::new(State::new(record)),
            lease: ControllerLease::new(),
            tokens: Mutex::new(TokenState::new()),
            events,
            next_event: AtomicU64::new(1),
            started: Instant::now(),
            initialized_tx,
            lease_epoch_tx,
            state_changed: Notify::new(),
            shutdown: CancellationToken::new(),
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
                    if let Err(error) = retry_launch_claims(&self.shared, verify_launch_claim).await {
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
    config: WorkerConfig,
    journal: Journal,
    state: AsyncMutex<State>,
    lease: ControllerLease,
    tokens: Mutex<TokenState>,
    events: broadcast::Sender<ControlEvent>,
    next_event: AtomicU64,
    started: Instant,
    initialized_tx: watch::Sender<bool>,
    lease_epoch_tx: watch::Sender<u64>,
    state_changed: Notify,
    shutdown: CancellationToken,
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
    runtime_id: Option<RuntimeId>,
    pty: Option<PtyOwner>,
    initialize_transaction: Option<TransactionId>,
    launch_agent_base: Option<String>,
    stop_grace: Duration,
    terminal_retention: Duration,
    exit: Option<ExitStatus>,
    stop_requested: bool,
    journal: JournalRecord,
}

impl State {
    fn new(journal: JournalRecord) -> Self {
        Self {
            phase: WireRuntimePhase::Uninitialized,
            runtime_id: None,
            pty: None,
            initialize_transaction: None,
            launch_agent_base: None,
            stop_grace: Duration::from_millis(500),
            terminal_retention: Duration::from_hours(24),
            exit: None,
            stop_requested: false,
            journal,
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
    runtime_id: RuntimeId,
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
                            writer.write(&ControlMessage::Event(event)).await
                                .map_err(|error| WorkerError::Protocol(error.to_string()))?;
                            continue;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => None,
                    }
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
        } => initialize_request(shared, connection, &lease_id, initialize).await,
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
        runtime_id: state.runtime_id.clone(),
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
    let state = shared.state.lock().await;
    Ok(ResponseKind::Inspected {
        snapshot: Box::new(inspect_snapshot(shared, &state).await?),
    })
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

    {
        let state = shared.state.lock().await;
        if let (Some(transaction), Some(runtime_id), Some(pty)) = (
            state.initialize_transaction.as_ref(),
            state.runtime_id.as_ref(),
            state.pty.as_ref(),
        ) {
            if transaction == &initialize.transaction_id {
                return Ok(ResponseKind::Initialized {
                    runtime_id: runtime_id.clone(),
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
    let runtime_id = RuntimeId::new(runtime_value)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    {
        let mut state = shared.state.lock().await;
        state.phase = WireRuntimePhase::Starting;
        state.initialize_transaction = Some(initialize.transaction_id.clone());
        state.runtime_id = Some(runtime_id.clone());
        state.journal.phase = JournalPhase::Starting;
        state.journal.runtime_id = Some(runtime_id.to_string());
        state.journal.updated_at = timestamp();
        persist_control(shared.journal.clone(), state.journal.clone()).await?;
    };

    let command = command_from_initialize(shared, &initialize, &runtime_id);
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
        stop_grace,
        terminal_retention: retention,
        ..shared.config.clone()
    };
    effective_config
        .validate()
        .map_err(|error| control_error(ControlCode::InvalidRequest, error, false))?;
    let pty = spawn_pty(command, &effective_config)?;
    let child_process = wire_identity(pty.identity())?;
    let live_commit = {
        let mut state = shared.state.lock().await;
        state.phase = WireRuntimePhase::Running;
        state.stop_grace = stop_grace;
        state.terminal_retention = retention;
        state.launch_agent_base = Some(initialize.launch.agent_base);
        state.journal.phase = JournalPhase::Live;
        state.journal.child = Some(journal_identity(pty.identity()));
        state.journal.pty_created_at = Some(timestamp());
        state.journal.cols = Some(initialize.dimensions.columns());
        state.journal.rows = Some(initialize.dimensions.rows());
        state.journal.updated_at = timestamp();
        state.pty = Some(pty.clone());
        persist(shared.journal.clone(), state.journal.clone()).await
    };
    if let Err(error) = live_commit {
        let _ = pty.stop("journal-failure", stop_grace).await;
        return Err(control_error(ControlCode::RuntimeFault, error, false));
    }
    shared.initialized_tx.send_replace(true);
    shared.emit(EventKind::RuntimeStarted {
        runtime_id: runtime_id.clone(),
        child_process,
    });
    spawn_runtime_monitors(Arc::clone(shared), pty, runtime_id.clone());
    Ok(ResponseKind::Initialized {
        runtime_id,
        child_process,
    })
}

fn spawn_pty(command: Command, config: &WorkerConfig) -> Result<PtyOwner, ControlError> {
    PtyOwner::spawn(
        command,
        config.history_bytes,
        config.subscriber_bytes,
        config.input_dedup_entries,
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
        runtime_id: scope.runtime_id.clone(),
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
        runtime_id: scope.runtime_id.clone(),
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
        runtime_id: scope.runtime_id.clone(),
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
    .map_err(input_control_error)?;
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
        runtime_id: state.runtime_id.clone(),
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
    })
}

fn spawn_runtime_monitors(shared: Arc<Shared>, pty: PtyOwner, runtime_id: RuntimeId) {
    let (output_eof_tx, output_eof_rx) = oneshot::channel();
    let output_shared = Arc::clone(&shared);
    let output_pty = pty.clone();
    let output_runtime = runtime_id.clone();
    tokio::spawn(async move {
        let result = monitor_runtime_output(&output_shared, &output_pty, &output_runtime).await;
        let _ = output_eof_tx.send(result);
    });

    tokio::spawn(coordinate_runtime_completion(
        shared,
        pty,
        runtime_id,
        output_eof_rx,
    ));
}

async fn monitor_runtime_output(
    shared: &Shared,
    pty: &PtyOwner,
    runtime_id: &RuntimeId,
) -> Result<OutputCompletion, WorkerError> {
    let mut subscriber = pty
        .subscribe_output(Some(0))
        .map_err(|error| WorkerError::Pty(PtyError::Output(error)))?;
    loop {
        match subscriber.recv().await {
            Some(OutputEvent::Replay(_) | OutputEvent::TerminalSnapshot(_)) => {}
            Some(OutputEvent::Output(chunk)) => {
                shared.emit(EventKind::OutputAdvanced {
                    runtime_id: runtime_id.clone(),
                    next_offset: chunk
                        .offset
                        .saturating_add(u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX)),
                });
            }
            Some(OutputEvent::Gap { watermark, .. }) => {
                shared.emit(EventKind::TerminalChanged {
                    runtime_id: runtime_id.clone(),
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
    runtime_id: RuntimeId,
    mut output_eof_rx: oneshot::Receiver<Result<OutputCompletion, WorkerError>>,
) {
    let exit = match wait_runtime_exit(&pty).await {
        Ok(exit) => exit,
        Err(error) => {
            let _ = pty
                .stop("root-observation-failure", shared.config.stop_grace)
                .await;
            mark_runtime_faulted(&shared, runtime_id, WorkerError::Pty(error)).await;
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
            mark_runtime_faulted(&shared, runtime_id, error).await;
            shared.shutdown.cancel();
            return;
        }
    };
    let next_output_offset = match completion {
        OutputCompletion::Eof { next_offset } => next_offset,
        OutputCompletion::ForcedClosed { next_offset } => {
            mark_runtime_faulted_at_offset(
                &shared,
                runtime_id,
                WorkerError::Pty(PtyError::OutputForcedClosed),
                Some(next_offset),
            )
            .await;
            shared.shutdown.cancel();
            return;
        }
    };
    if let Err(error) = pty.finish_natural().await {
        mark_runtime_faulted(&shared, runtime_id, WorkerError::Pty(error)).await;
        shared.shutdown.cancel();
        return;
    }
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
        runtime_id,
        exit: status,
    });
    tokio::select! {
        () = tokio::time::sleep(retention) => shared.shutdown.cancel(),
        () = shared.shutdown.cancelled() => {}
    }
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
                match pty.stop("natural-drain-timeout", grace).await {
                    Ok(_) | Err(PtyError::OutputForcedClosed) => {}
                    Err(error) => return Err(WorkerError::Pty(error)),
                }
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
    });
    journal.next_output_offset = next_output_offset;
    journal.updated_at = timestamp();
    persist(shared.journal.clone(), journal.clone()).await?;
    state.phase = WireRuntimePhase::Exited;
    state.exit = Some(status.clone());
    state.journal = journal;
    shared.state_changed.notify_waiters();
    Ok(state.terminal_retention)
}

async fn wait_runtime_exit(pty: &PtyOwner) -> Result<Exit, PtyError> {
    pty.wait_exit().await
}

async fn mark_runtime_faulted(shared: &Shared, runtime_id: RuntimeId, error: WorkerError) {
    mark_runtime_faulted_at_offset(shared, runtime_id, error, None).await;
}

async fn mark_runtime_faulted_at_offset(
    shared: &Shared,
    runtime_id: RuntimeId,
    error: WorkerError,
    next_output_offset: Option<u64>,
) {
    let commit = {
        let mut state = shared.state.lock().await;
        state.phase = WireRuntimePhase::Faulted;
        state.journal.phase = JournalPhase::Faulted;
        if let Some(next_output_offset) = next_output_offset {
            state.journal.next_output_offset = next_output_offset;
        }
        state.journal.pending_launch_claims.clear();
        mark_running_subagents_lost(&mut state.journal, unix_ms());
        state.journal.updated_at = timestamp();
        persist(shared.journal.clone(), state.journal.clone()).await
    };
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
        runtime_id: Some(runtime_id),
        error: control_error(ControlCode::RuntimeFault, error, false),
    });
}

#[expect(
    clippy::too_many_lines,
    reason = "the bidirectional data-stream select loop keeps lease, output, and input ordering in one place"
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
            &header.runtime_id,
            after_offset,
            observation,
            grant.lease_epoch,
            &mut lease_epoch,
        )
        .await;
    }
    let mut subscriber = if let Some(attach) = attach {
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
                    &header.runtime_id,
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
            &header.runtime_id,
            FrameKind::AttachReady { dimensions },
            Vec::new(),
        )
        .await?;
        subscriber
    } else {
        pty.subscribe_output(after_offset)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?
    };
    let version = header.version;
    let stream_id = header.stream_id;
    let runtime_id = header.runtime_id;
    let mut next_input_sequence = 1_u64;
    let mut root_exit = pty.exit_receiver();
    let mut pending_output_exit = None;

    loop {
        tokio::select! {
            changed = lease_epoch.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                write_data_frame(
                    &mut write_half,
                    version,
                    &stream_id,
                    &runtime_id,
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
                    if *lease_epoch.borrow() != grant.lease_epoch {
                        return Ok(());
                    }
                    authorize_data_frame(&shared, &peer, &grant.lease_owner)?;
                    write_runtime_output_event(
                        &mut write_half,
                        version,
                        &stream_id,
                        &runtime_id,
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
                authorize_data_frame(&shared, &peer, &grant.lease_owner)?;
                write_runtime_output_event(
                    &mut write_half,
                    version,
                    &stream_id,
                    &runtime_id,
                    shared.config.data_payload_bytes,
                    output,
                    pty.output_forced_closed(),
                ).await?;
            }
            input = protocol::read_frame(&mut read_half) => {
                let input = match input {
                    Ok(Some(input)) => input,
                    Ok(None) => return Ok(()),
                    Err(error) => {
                        return write_data_error(
                            &mut write_half,
                            version,
                            &stream_id,
                            &runtime_id,
                            control_error(ControlCode::InvalidRequest, error, false),
                        ).await;
                    }
                };
                authorize_data_frame(&shared, &peer, &grant.lease_owner)?;
                if mode != StreamMode::Attach {
                    return write_data_error(
                        &mut write_half,
                        version,
                        &stream_id,
                        &runtime_id,
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
                        &mut write_half,
                        version,
                        &stream_id,
                        &runtime_id,
                        control_error_message(
                            ControlCode::InvalidRequest,
                            "attach data stream received a non-input frame",
                            false,
                        ),
                    ).await;
                };
                if input_header.runtime_id != runtime_id || input_header.stream_id != stream_id {
                    return write_data_error(
                        &mut write_half,
                        version,
                        &stream_id,
                        &runtime_id,
                        identity_mismatch(),
                    ).await;
                }
                if let Err(error) =
                    validate_attach_write_id(&write_id, &stream_id, next_input_sequence)
                {
                    return write_data_error(
                        &mut write_half,
                        version,
                        &stream_id,
                        &runtime_id,
                        error,
                    ).await;
                }
                let byte_count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if let Err(error) = pty.input().execute_stream(vec![InputFragment {
                        bytes,
                        delay_after: Duration::ZERO,
                    }]).await {
                    return write_data_error(
                        &mut write_half,
                        version,
                        &stream_id,
                        &runtime_id,
                        input_control_error(error),
                    ).await;
                }
                write_data_frame(
                    &mut write_half,
                    version,
                    &stream_id,
                    &runtime_id,
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
                            &mut write_half,
                            version,
                            &stream_id,
                            &runtime_id,
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
            || grant.runtime_id != header.runtime_id
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
    runtime_id: &RuntimeId,
    after_offset: Option<u64>,
    observation: ObservationGrant,
    expected_lease_epoch: u64,
    lease_epoch: &mut watch::Receiver<u64>,
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
                runtime_id,
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
                runtime_id,
                control_error_message(
                    ControlCode::InvalidRequest,
                    "observation stream is output-only",
                    false,
                ),
            )
            .await;
        }
    };

    if *lease_epoch.borrow() != expected_lease_epoch {
        return write_data_frame(
            writer,
            version,
            stream_id,
            runtime_id,
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
        runtime_id,
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
    runtime_id: &RuntimeId,
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
        runtime_id,
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
        runtime_id,
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
        runtime_id,
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
    runtime_id: &RuntimeId,
    error: ControlError,
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    write_data_frame(
        writer,
        version,
        stream_id,
        runtime_id,
        FrameKind::Error { error },
        Vec::new(),
    )
    .await
}

async fn write_output_event<W>(
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    runtime_id: &RuntimeId,
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
                runtime_id,
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
                runtime_id,
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
                runtime_id,
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
                runtime_id,
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
                runtime_id,
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
    runtime_id: &RuntimeId,
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
            runtime_id,
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
        runtime_id,
        payload_limit,
        output,
    )
    .await
}

async fn write_output_chunks<W>(
    writer: &mut W,
    version: Version,
    stream_id: &StreamId,
    runtime_id: &RuntimeId,
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
        write_data_frame(writer, version, stream_id, runtime_id, kind, bytes.to_vec()).await?;
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
    runtime_id: &RuntimeId,
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
            runtime_id,
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
    runtime_id: &RuntimeId,
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
            runtime_id: runtime_id.clone(),
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
        runtime_id: String,
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
        expires_at: String,
        reference_kind: Option<String>,
        native_reference: Option<String>,
    },
    IdentityRelease {
        runtime_id: String,
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
    },
    SubagentStart {
        runtime_id: String,
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
        subagent_id: String,
        parent_id: Option<String>,
        agent_type: Option<String>,
    },
    SubagentStop {
        runtime_id: String,
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
        subagent_id: String,
        outcome: Option<JournalSubagentPhase>,
    },
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
    serve_identity_hook_with_probe(shared, writer, value, peer, verify_launch_claim).await
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
    let (accepted, launch_identity_accepted, runtime_id, subagent_changed) = match request {
        HookRequest::IdentityReport {
            runtime_id,
            provider,
            pid,
            start_identity,
            sequence,
            expires_at,
            reference_kind,
            native_reference,
        } => {
            let mut state = shared.state.lock().await;
            let current_runtime = state.runtime_id.as_ref().map(ToString::to_string);
            let inadmissible = hook_claim_admissible(
                known_identity_provider(&provider),
                current_runtime.as_deref() == Some(runtime_id.as_str()),
                state.phase == WireRuntimePhase::Running,
            )
            .or_else(|| {
                (!valid_identity_expiry(&expires_at)).then_some(RejectReason::ClaimExpired)
            });
            if let Some(reason) = inadmissible {
                reject_identity(&shared, "identity_report", reason, peer);
                (false, false, current_runtime, false)
            } else {
                let pty = state.pty.as_ref().ok_or_else(|| {
                    WorkerError::Protocol("runtime is not initialized".to_owned())
                })?;
                let root_identity = pty.identity().clone();
                let process_valid = hook_claim_valid(
                    &shared,
                    "identity_report",
                    peer,
                    pid,
                    start_identity,
                    root_identity.pid,
                );
                let sequence_valid = identity_sequence_is_fresh(&state.journal, sequence);
                if !sequence_valid {
                    reject_identity(
                        &shared,
                        "identity_report",
                        RejectReason::SequenceStale,
                        peer,
                    );
                }
                if !process_valid || !sequence_valid {
                    (false, false, current_runtime, false)
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
                    if let (Some(reference_kind), Some(native_reference), Some(launch_base)) = (
                        reference_kind,
                        native_reference,
                        state.launch_agent_base.as_ref(),
                    ) {
                        if &provider == launch_base {
                            let claim = PendingLaunchClaim {
                                retry_pending: true,
                                runtime_id,
                                root: journal_identity(&root_identity),
                                identity: LaunchIdentity {
                                    provider,
                                    process,
                                    reference_kind,
                                    native_reference,
                                },
                                expires_at,
                            };
                            launch_identity_status = launch::submit(
                                &mut journal,
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
                        }
                    }
                    journal.updated_at = timestamp();
                    persist(shared.journal.clone(), journal.clone()).await?;
                    state.journal = journal;
                    (
                        true,
                        launch_identity_status == LaunchClaimStatus::Accepted,
                        current_runtime,
                        false,
                    )
                }
            }
        }
        HookRequest::IdentityRelease {
            runtime_id,
            provider,
            pid,
            start_identity,
            sequence,
        } => {
            let mut state = shared.state.lock().await;
            let current_runtime = state.runtime_id.as_ref().map(ToString::to_string);
            let inadmissible = hook_claim_admissible(
                known_identity_provider(&provider),
                current_runtime.as_deref() == Some(runtime_id.as_str()),
                true,
            );
            if let Some(reason) = inadmissible {
                reject_identity(&shared, "identity_release", reason, peer);
                (false, false, current_runtime, false)
            } else {
                let pty = state.pty.as_ref().ok_or_else(|| {
                    WorkerError::Protocol("runtime is not initialized".to_owned())
                })?;
                let process_valid = hook_claim_valid(
                    &shared,
                    "identity_release",
                    peer,
                    pid,
                    start_identity,
                    pty.identity().pid,
                );
                let released_process = if process_valid {
                    state
                        .journal
                        .active_identity
                        .as_ref()
                        .filter(|active| {
                            active.provider == provider
                                && active.process.pid == pid
                                && active.process.start_identity == start_identity.to_string()
                                && sequence > active.sequence
                                && state
                                    .journal
                                    .active_identity_release
                                    .as_ref()
                                    .is_none_or(|release| sequence > release.sequence)
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
                    journal.updated_at = timestamp();
                    persist(shared.journal.clone(), journal.clone()).await?;
                    state.journal = journal;
                    (true, false, current_runtime, false)
                } else {
                    (false, false, current_runtime, false)
                }
            }
        }
        HookRequest::SubagentStart {
            runtime_id,
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
            let current_runtime = state.runtime_id.as_ref().map(ToString::to_string);
            let inadmissible = hook_claim_admissible(
                known_subagent_provider(&provider),
                current_runtime.as_deref() == Some(runtime_id.as_str()),
                state.phase == WireRuntimePhase::Running,
            )
            .or_else(|| {
                let text_valid = valid_subagent_text(&subagent_id, MAX_SUBAGENT_ID_BYTES)
                    && parent_id
                        .as_deref()
                        .is_none_or(|value| valid_subagent_text(value, MAX_SUBAGENT_ID_BYTES))
                    && agent_type
                        .as_deref()
                        .is_none_or(|value| valid_subagent_text(value, MAX_SUBAGENT_TYPE_BYTES));
                (!text_valid).then_some(RejectReason::SubagentClaimInvalid)
            });
            if let Some(reason) = inadmissible {
                reject_identity(&shared, "subagent_start", reason, peer);
            }
            let valid = inadmissible.is_none();
            let process_valid = state.pty.as_ref().is_some_and(|pty| {
                hook_claim_valid(
                    &shared,
                    "subagent_start",
                    peer,
                    pid,
                    start_identity,
                    pty.identity().pid,
                )
            });
            if !valid || !process_valid {
                (false, false, current_runtime, false)
            } else {
                let mut journal = state.journal.clone();
                let accepted = start_subagent(
                    &mut journal,
                    provider,
                    subagent_id,
                    parent_id,
                    agent_type,
                    sequence,
                    occurred_at_ms,
                );
                if accepted {
                    journal.updated_at = timestamp();
                    persist(shared.journal.clone(), journal.clone()).await?;
                    state.journal = journal;
                }
                (accepted, false, current_runtime, accepted)
            }
        }
        HookRequest::SubagentStop {
            runtime_id,
            provider,
            pid,
            start_identity,
            sequence,
            subagent_id,
            outcome,
        } => {
            let occurred_at_ms = unix_ms();
            let mut state = shared.state.lock().await;
            let current_runtime = state.runtime_id.as_ref().map(ToString::to_string);
            let inadmissible = hook_claim_admissible(
                known_subagent_provider(&provider),
                current_runtime.as_deref() == Some(runtime_id.as_str()),
                state.phase == WireRuntimePhase::Running,
            )
            .or_else(|| {
                let text_valid = valid_subagent_text(&subagent_id, MAX_SUBAGENT_ID_BYTES)
                    && outcome.is_none_or(|phase| phase != JournalSubagentPhase::Running);
                (!text_valid).then_some(RejectReason::SubagentClaimInvalid)
            });
            if let Some(reason) = inadmissible {
                reject_identity(&shared, "subagent_stop", reason, peer);
            }
            let valid = inadmissible.is_none();
            let process_valid = state.pty.as_ref().is_some_and(|pty| {
                hook_claim_valid(
                    &shared,
                    "subagent_stop",
                    peer,
                    pid,
                    start_identity,
                    pty.identity().pid,
                )
            });
            if !valid || !process_valid {
                (false, false, current_runtime, false)
            } else {
                let mut journal = state.journal.clone();
                let accepted = stop_subagent(
                    &mut journal,
                    &provider,
                    &subagent_id,
                    outcome.unwrap_or(JournalSubagentPhase::Completed),
                    sequence,
                    occurred_at_ms,
                );
                if accepted {
                    journal.updated_at = timestamp();
                    persist(shared.journal.clone(), journal.clone()).await?;
                    state.journal = journal;
                }
                (accepted, false, current_runtime, accepted)
            }
        }
    };
    if accepted {
        if let Some(runtime_id) = runtime_id.and_then(|value| RuntimeId::new(value).ok()) {
            if subagent_changed {
                shared.emit(EventKind::SubagentsChanged { runtime_id });
            } else {
                shared.emit(EventKind::IdentityChanged { runtime_id });
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

fn known_identity_provider(provider: &str) -> bool {
    matches!(provider, "shell" | "codex" | "claude" | "hermes")
}

fn verify_launch_claim(claim: &PendingLaunchClaim) -> Result<bool, WorkerError> {
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
        match HostInspector::new().identity(expected.pid) {
            Ok(Some(current)) if current == expected => {}
            Ok(_) | Err(pohunek_platform::process::Error::Race { .. }) => return Ok(false),
            Err(error) => return Err(WorkerError::Protocol(error.to_string())),
        }
    }
    // This probe also rechecks root/candidate generations after executable and
    // ancestry inspection. The stored PID alone never authorizes a retry.
    let designated = designated_launch_process(root, &claim.identity.provider)?;
    Ok(designated.is_some_and(|process| {
        process.pid == candidate.pid
            && process.start_identity.to_string() == candidate.start_identity.to_string()
    }))
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
        if let (true, Some(runtime_id)) = (launch_became_accepted, state.runtime_id.clone()) {
            shared.emit(EventKind::IdentityChanged { runtime_id });
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

fn known_subagent_provider(provider: &str) -> bool {
    matches!(provider, "codex" | "claude")
}

fn valid_subagent_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn start_subagent(
    journal: &mut JournalRecord,
    provider: String,
    id: String,
    parent_id: Option<String>,
    agent_type: Option<String>,
    sequence: u64,
    occurred_at_ms: u64,
) -> bool {
    if journal
        .subagents
        .iter()
        .any(|item| item.provider == provider && item.id == id)
    {
        return false;
    }
    if journal
        .subagents
        .iter()
        .filter(|item| item.phase == JournalSubagentPhase::Running)
        .count()
        >= ACTIVE_SUBAGENT_CAPACITY
    {
        return false;
    }
    journal.subagent_revision = journal.subagent_revision.saturating_add(1);
    journal.subagents.push(SubagentRecord {
        id,
        parent_id,
        provider,
        agent_type,
        phase: JournalSubagentPhase::Running,
        sequence,
        revision: journal.subagent_revision,
        started_at_ms: occurred_at_ms,
        updated_at_ms: occurred_at_ms,
        finished_at_ms: None,
    });
    true
}

fn stop_subagent(
    journal: &mut JournalRecord,
    provider: &str,
    id: &str,
    phase: JournalSubagentPhase,
    sequence: u64,
    occurred_at_ms: u64,
) -> bool {
    let existing_index = journal
        .subagents
        .iter()
        .position(|item| item.provider == provider && item.id == id);
    if existing_index.is_some_and(|index| {
        journal.subagents[index].phase != JournalSubagentPhase::Running
            || sequence <= journal.subagents[index].sequence
    }) {
        return false;
    }
    journal.subagent_revision = journal.subagent_revision.saturating_add(1);
    if let Some(index) = existing_index {
        let subagent = &mut journal.subagents[index];
        subagent.phase = phase;
        subagent.sequence = sequence;
        subagent.revision = journal.subagent_revision;
        subagent.updated_at_ms = occurred_at_ms;
        subagent.finished_at_ms = Some(occurred_at_ms);
    } else {
        journal.subagents.push(SubagentRecord {
            id: id.to_owned(),
            parent_id: None,
            provider: provider.to_owned(),
            agent_type: None,
            phase,
            sequence,
            revision: journal.subagent_revision,
            started_at_ms: occurred_at_ms,
            updated_at_ms: occurred_at_ms,
            finished_at_ms: Some(occurred_at_ms),
        });
    }
    prune_terminal_subagents(journal);
    true
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

fn identity_sequence_is_fresh(journal: &JournalRecord, sequence: u64) -> bool {
    let active_is_fresh = journal
        .active_identity
        .as_ref()
        .is_none_or(|active| sequence > active.sequence);
    active_is_fresh
        && journal
            .active_identity_release
            .as_ref()
            .is_none_or(|release| sequence > release.sequence)
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

fn validate_hook_process(pid: u32, start_identity: u64, root_pid: u32) -> Result<(), WorkerError> {
    if process_start(pid)? != start_identity || !is_descendant(pid, root_pid)? {
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
/// environment carries the runtime id and the private socket path to everything
/// the operator runs.
///
/// The ancestry walk reads live parent links, so the relationship holds only
/// while the chain is alive. A reporter whose intermediate parent has exited is
/// reparented out of the subject's subtree and is rejected, which is the
/// conservative direction and stays distinguishable through its own reason code.
///
/// The stricter `peer == subject` binding belongs to the explicit in-process
/// identity mode in <https://github.com/zajca/pohunek/issues/52>, which can
/// demand it on a wire shape of its own without breaking child hooks.
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
/// Provider, runtime generation, and runtime phase are the rules every hook
/// variant shares. Reporting them as distinct codes is what lets an operator
/// tell a stale hook from a rejected one; before this they were indistinguishable
/// from a hook that never ran.
fn hook_claim_admissible(
    provider_known: bool,
    runtime_matches: bool,
    phase_running: bool,
) -> Option<RejectReason> {
    if !provider_known {
        return Some(RejectReason::ProviderNotAllowed);
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
    pid: u32,
    start_identity: u64,
    root_pid: u32,
) -> bool {
    if let Err(reason) = authorize_hook_report(peer, pid) {
        reject_identity(shared, operation, reason, peer);
        return false;
    }
    if validate_hook_process(pid, start_identity, root_pid).is_err() {
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
        || state.runtime_id.as_ref() != Some(&scope.runtime_id)
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

fn command_from_initialize(
    shared: &Shared,
    initialize: &Initialize,
    runtime_id: &RuntimeId,
) -> Command {
    let mut environment = initialize.environment.clone().into_inner();
    for name in WORKER_ONLY_ENV {
        environment.remove(name);
    }
    environment.retain(|name, _| !name.starts_with("POHUNEK_"));
    let mut reserved = BTreeMap::from([
        ("POHUNEK_ENV".to_owned(), "1".to_owned()),
        (
            "POHUNEK_SESSION_ID".to_owned(),
            shared.session_id.to_string(),
        ),
        ("POHUNEK_WORKER_ID".to_owned(), shared.worker_id.to_string()),
        ("POHUNEK_RUNTIME_ID".to_owned(), runtime_id.to_string()),
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
    ]);
    if let Some(reference_kind) = &initialize.launch.reference_kind {
        reserved.insert(
            "POHUNEK_NATIVE_REFERENCE_KIND".to_owned(),
            reference_kind.clone(),
        );
    }
    environment.extend(reserved);
    Command {
        program: initialize.executable.to_string_lossy().into_owned(),
        args: initialize.arguments.clone(),
        env: environment.into_iter().collect(),
        cwd: initialize.cwd.clone(),
        cols: initialize.dimensions.columns(),
        rows: initialize.dimensions.rows(),
    }
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

fn input_control_error(error: crate::InputError) -> ControlError {
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

fn process_parent(pid: u32) -> Result<u32, WorkerError> {
    HostInspector::new()
        .parent_pid(pid)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?
        .ok_or_else(|| WorkerError::Protocol("process no longer exists".to_owned()))
}

fn is_descendant(mut pid: u32, root: u32) -> Result<bool, WorkerError> {
    for _ in 0..128 {
        if pid == root {
            return Ok(true);
        }
        if pid <= 1 {
            return Ok(false);
        }
        let parent = process_parent(pid)?;
        if parent == pid {
            return Ok(false);
        }
        pid = parent;
    }
    Ok(false)
}

fn designated_launch_process(
    root: OsProcessIdentity,
    provider: &str,
) -> Result<Option<WireProcessIdentity>, WorkerError> {
    let inspector = HostInspector::new();
    let mut processes = inspector
        .descendant_identities(root)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    processes.push(root);
    let provider = provider.to_ascii_lowercase();
    let mut candidates = Vec::new();
    for process in processes {
        let Some(executable) = inspector
            .executable(process.pid)
            .map_err(|error| WorkerError::Protocol(error.to_string()))?
        else {
            continue;
        };
        let executable_name = executable
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !executable_name.contains(&provider) {
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
        || !is_descendant(candidate.pid, root.pid)?
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
mod tests {
    use std::time::Duration;

    use super::{
        authorize_control_request, await_observation_page, capabilities, control_input_id,
        event_visible_to_connection, identity_sequence_is_fresh, known_identity_provider,
        mark_running_subagents_lost, observation_timed_out, random_value, redeem_data_grant,
        signal_number, start_subagent, stop_subagent, valid_identity_expiry,
        validate_attach_write_id, validate_data_start, validate_observation_request,
        validate_terminal_snapshot_dimensions, validate_terminal_snapshot_response,
        wait_runtime_exit, write_data_error, write_observation_page, write_output_chunks,
        write_runtime_output_event, write_terminal_chunks, Connection, ControlInputId, DataGrant,
        ObservationGrant, ObservationWaitOutcome, PrefixStream, TokenState, WireTerminalSnapshot,
    };
    use pohunek_worker_protocol::{
        self as protocol, AttachStart, Capability, ControlCode, ControlError, ControlMessage,
        ControlResponse, Cursor, DataToken, Dimensions, EventKind, ExitStatus, FrameHeader,
        FrameKind, LeaseId, RequestId, ResponseKind, RuntimeId, StreamId, StreamMode, Version,
        WriteId, CURRENT_VERSION, MAX_DATA_PAYLOAD_BYTES, PREVIOUS_VERSION,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    use crate::journal::SubagentPhase;
    use crate::{ChildIdentity, JournalRecord, ReleasedIdentity};

    /// Extra bytes force exactly one partial frame after a full wire payload.
    const OVERSIZED_PAYLOAD_EXTRA: usize = 257;

    struct BarrierDirectory(std::path::PathBuf);

    impl BarrierDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(random_value("output-barrier").unwrap());
            std::fs::create_dir(&path).expect("create output barrier directory");
            std::fs::set_permissions(
                &path,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
            )
            .expect("make output barrier directory private");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for BarrierDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn retained_session_lock_rejects_a_second_worker_bind() {
        let directory = BarrierDirectory::new();
        let args = super::ServerArgs {
            session_id: "s-112".to_owned(),
            worker_id: "worker-lock".to_owned(),
            socket_path: directory.path().join("worker.sock"),
            journal_path: directory.path().join("journal.json"),
            daemon_socket_path: directory.path().join("daemon.sock"),
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
                socket_path: worker_socket.clone(),
                journal_path: journal.clone(),
                daemon_socket_path: overlong,
                config: crate::WorkerConfig::new(),
            })
            .await,
            Err(crate::WorkerError::Paths(_))
        ));
        assert!(!worker_socket.exists());
        assert!(!journal.exists());
        assert!(!directory.path().join(super::SOCKET_LOCK_NAME).exists());
    }

    async fn launch_claim_fixture() -> (super::Server, crate::PtyOwner, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(random_value("worker-claim").unwrap());
        let config = crate::WorkerConfig::new();
        let server = super::Server::bind(super::ServerArgs {
            session_id: "s-112".into(),
            worker_id: "worker-claim".into(),
            socket_path: directory.join("worker.sock"),
            journal_path: directory.join("journal.json"),
            daemon_socket_path: directory.join("daemon.sock"),
            config: config.clone(),
        })
        .await
        .unwrap();
        let pty = super::spawn_pty(
            crate::Command {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "read -r line".into()],
                env: Vec::new(),
                cwd: directory.clone(),
                cols: 80,
                rows: 24,
            },
            &config,
        )
        .unwrap();
        {
            let mut state = server.shared.state.lock().await;
            state.phase = super::WireRuntimePhase::Running;
            state.runtime_id = Some(RuntimeId::new("runtime-claim").unwrap());
            state.launch_agent_base = Some("claude".into());
            state.pty = Some(pty.clone());
            state.journal.phase = super::JournalPhase::Live;
            state.journal.runtime_id = Some("runtime-claim".into());
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
    async fn deferred_launch_ack_owns_durable_original_claim_and_retry_commits_before_event() {
        let (server, pty, directory) = launch_claim_fixture().await;
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
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// A peer that no longer exists leaves the journal untouched.
    ///
    /// `reverify` distinguishes a vanished peer from a substituted one, and the
    /// hook path must reject before it writes: proving the return value alone
    /// would say nothing about whether durable state moved.
    #[tokio::test]
    async fn a_vanished_peer_is_rejected_without_touching_the_journal() {
        let (server, pty, directory) = launch_claim_fixture().await;
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
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn immediate_launch_rejection_commits_fence_before_acknowledgment() {
        let (server, pty, directory) = launch_claim_fixture().await;
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
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn failed_pending_commit_never_acknowledges_or_changes_memory() {
        let (server, pty, directory) = launch_claim_fixture().await;
        std::fs::rename(
            server.shared.journal.path(),
            directory.join("original.json"),
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
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn identity_provider_allowlist_is_exact() {
        for provider in ["shell", "codex", "claude", "hermes"] {
            assert!(
                known_identity_provider(provider),
                "{provider} must be accepted"
            );
        }

        for provider in ["", "Hermes", "HERMES", "hermes ", "unknown"] {
            assert!(
                !known_identity_provider(provider),
                "{provider:?} must be rejected"
            );
        }
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

        assert!(!identity_sequence_is_fresh(&journal, 7));
        assert!(!identity_sequence_is_fresh(&journal, 8));
        assert!(identity_sequence_is_fresh(&journal, 9));
    }

    #[test]
    fn subagent_lifecycle_is_ordered_and_runtime_exit_marks_running_entries_lost() {
        let mut journal = JournalRecord::bootstrap(
            "s-subagents".to_owned(),
            "worker-subagents".to_owned(),
            10,
            "100".to_owned(),
            "boot-test".to_owned(),
            (4, 5),
            "2026-09-11T00:00:00Z".to_owned(),
        );

        assert!(start_subagent(
            &mut journal,
            "claude".to_owned(),
            "child-a".to_owned(),
            None,
            Some("Explore".to_owned()),
            10,
            100,
        ));
        assert!(!stop_subagent(
            &mut journal,
            "claude",
            "child-a",
            SubagentPhase::Completed,
            9,
            101,
        ));
        assert!(start_subagent(
            &mut journal,
            "codex".to_owned(),
            "child-b".to_owned(),
            Some("child-a".to_owned()),
            None,
            20,
            110,
        ));
        assert!(stop_subagent(
            &mut journal,
            "claude",
            "child-a",
            SubagentPhase::Completed,
            11,
            120,
        ));

        mark_running_subagents_lost(&mut journal, 130);

        assert_eq!(journal.subagents[0].phase, SubagentPhase::Completed);
        assert_eq!(journal.subagents[1].phase, SubagentPhase::Lost);
        assert_eq!(journal.subagents[1].finished_at_ms, Some(130));
        assert!(journal.subagents[1].revision > journal.subagents[0].revision);
        let lost_revision = journal.subagents[1].revision;
        assert!(!stop_subagent(
            &mut journal,
            "codex",
            "child-b",
            SubagentPhase::Completed,
            21,
            140,
        ));
        assert!(!start_subagent(
            &mut journal,
            "codex".to_owned(),
            "child-b".to_owned(),
            None,
            None,
            22,
            150,
        ));
        assert_eq!(journal.subagents[1].phase, SubagentPhase::Lost);
        assert_eq!(journal.subagents[1].revision, lost_revision);
    }

    #[test]
    fn subagent_stop_tombstone_rejects_a_delayed_start() {
        let mut journal = JournalRecord::bootstrap(
            "s-subagents".to_owned(),
            "worker-subagents".to_owned(),
            10,
            "100".to_owned(),
            "boot-test".to_owned(),
            (4, 5),
            "2026-09-11T00:00:00Z".to_owned(),
        );

        assert!(stop_subagent(
            &mut journal,
            "codex",
            "child-a",
            SubagentPhase::Completed,
            20,
            120,
        ));
        assert!(!start_subagent(
            &mut journal,
            "codex".to_owned(),
            "child-a".to_owned(),
            None,
            Some("explore".to_owned()),
            19,
            100,
        ));
        assert!(!start_subagent(
            &mut journal,
            "codex".to_owned(),
            "child-a".to_owned(),
            None,
            Some("explore".to_owned()),
            21,
            130,
        ));

        assert_eq!(journal.subagents.len(), 1);
        assert_eq!(journal.subagents[0].phase, SubagentPhase::Completed);
        assert_eq!(journal.subagents[0].sequence, 20);
    }

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
                env: Vec::new(),
                cwd: std::env::temp_dir(),
                cols: 80,
                rows: 24,
            },
            config.history_bytes,
            config.subscriber_bytes,
            config.input_dedup_entries,
        )
        .expect("spawn PTY");
        let mut subscriber = pty.subscribe_output(Some(0)).expect("subscribe output");

        tokio::time::timeout(Duration::from_secs(3), wait_runtime_exit(&pty))
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

    #[tokio::test]
    async fn natural_exit_bounds_a_descendant_that_never_closes_the_pty() {
        let directory = BarrierDirectory::new();
        let config = crate::WorkerConfig {
            post_exit_drain_timeout: Duration::from_millis(50),
            stop_grace: Duration::from_millis(50),
            ..crate::WorkerConfig::new()
        };
        let server = super::Server::bind(super::ServerArgs {
            session_id: "s-113".to_owned(),
            worker_id: "worker-bounded-drain".to_owned(),
            socket_path: directory.path().join("worker.sock"),
            journal_path: directory.path().join("journal.json"),
            daemon_socket_path: directory.path().join("daemon.sock"),
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
                env: Vec::new(),
                cwd: directory.path().to_path_buf(),
                cols: 80,
                rows: 24,
            },
            &config,
        )
        .expect("spawn PTY");
        let runtime_id = RuntimeId::new("runtime-bounded-drain").expect("runtime id");
        let mut state = server.shared.state.lock().await;
        state.phase = super::WireRuntimePhase::Running;
        state.runtime_id = Some(runtime_id.clone());
        state.pty = Some(pty.clone());
        state.stop_grace = config.stop_grace;
        state.journal.phase = super::JournalPhase::Live;
        state.journal.runtime_id = Some(runtime_id.as_str().to_owned());
        state.journal.child = Some(super::journal_identity(pty.identity()));
        drop(state);
        super::spawn_runtime_monitors(
            std::sync::Arc::clone(&server.shared),
            pty.clone(),
            runtime_id,
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if server.shared.state.lock().await.phase == super::WireRuntimePhase::Exited {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("bounded natural drain completion");
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

    #[tokio::test]
    async fn forced_output_completion_does_not_wait_for_the_output_monitor() {
        let directory = BarrierDirectory::new();
        let ready = directory.path().join("held-monitor-ready");
        let config = crate::WorkerConfig {
            post_exit_drain_timeout: Duration::from_millis(50),
            stop_grace: Duration::from_millis(50),
            ..crate::WorkerConfig::new()
        };
        let server = super::Server::bind(super::ServerArgs {
            session_id: "s-115".to_owned(),
            worker_id: "worker-held-monitor".to_owned(),
            socket_path: directory.path().join("worker.sock"),
            journal_path: directory.path().join("journal.json"),
            daemon_socket_path: directory.path().join("daemon.sock"),
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
                env: Vec::new(),
                cwd: directory.path().to_path_buf(),
                cols: 80,
                rows: 24,
            },
            &config,
        )
        .expect("spawn PTY");
        server.shared.state.lock().await.stop_grace = config.stop_grace;
        tokio::time::timeout(Duration::from_secs(2), pty.wait_exit())
            .await
            .expect("root exit deadline")
            .expect("root exit");

        let (_output_eof_tx, mut output_eof_rx) = tokio::sync::oneshot::channel();
        let completion = tokio::time::timeout(
            Duration::from_secs(1),
            super::wait_for_output_completion(&server.shared, &pty, &mut output_eof_rx),
        )
        .await
        .expect("forced completion deadline")
        .expect("forced completion");

        assert!(matches!(
            completion,
            crate::output::OutputCompletion::ForcedClosed { next_offset }
                if next_offset == pty.output().next_offset() && next_offset > 0
        ));
    }

    #[tokio::test]
    async fn escaped_pty_holder_faults_with_the_exact_final_output_offset() {
        let directory = BarrierDirectory::new();
        let ready = directory.path().join("escaped-ready");
        let config = crate::WorkerConfig {
            post_exit_drain_timeout: Duration::from_millis(50),
            stop_grace: Duration::from_millis(50),
            ..crate::WorkerConfig::new()
        };
        let server = super::Server::bind(super::ServerArgs {
            session_id: "s-114".to_owned(),
            worker_id: "worker-escaped-output".to_owned(),
            socket_path: directory.path().join("worker.sock"),
            journal_path: directory.path().join("journal.json"),
            daemon_socket_path: directory.path().join("daemon.sock"),
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
                env: Vec::new(),
                cwd: directory.path().to_path_buf(),
                cols: 80,
                rows: 24,
            },
            &config,
        )
        .expect("spawn PTY");
        let runtime_id = RuntimeId::new("runtime-escaped-output").expect("runtime id");
        let () = {
            let mut state = server.shared.state.lock().await;
            state.phase = super::WireRuntimePhase::Running;
            state.runtime_id = Some(runtime_id.clone());
            state.pty = Some(pty.clone());
            state.stop_grace = config.stop_grace;
            state.journal.phase = super::JournalPhase::Live;
            state.journal.runtime_id = Some(runtime_id.as_str().to_owned());
            state.journal.child = Some(super::journal_identity(pty.identity()));
        };
        super::spawn_runtime_monitors(
            std::sync::Arc::clone(&server.shared),
            pty.clone(),
            runtime_id,
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if server.shared.state.lock().await.phase == super::WireRuntimePhase::Faulted {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("bounded forced-close fault");
        let final_offset = pty.output().next_offset();
        let journal = server.shared.journal.load().expect("load fault journal");
        assert_eq!(journal.phase, super::JournalPhase::Faulted);
        assert_eq!(journal.next_output_offset, final_offset);
        assert!(final_offset > 0);
    }

    #[tokio::test]
    async fn terminal_commit_retries_before_publishing_exited_state() {
        let (server, pty, directory) = launch_claim_fixture().await;
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
        let retention = tokio::time::timeout(Duration::from_secs(2), &mut completion)
            .await
            .expect("terminal commit retry deadline")
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
            tokio::time::timeout(Duration::from_secs(1), &mut terminal_confirmation)
                .await
                .expect("terminal confirmation deadline")
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
        pty.stop("test-cleanup", Duration::from_millis(100))
            .await
            .expect("stop fixture PTY");
        drop(server);
        std::fs::remove_dir_all(directory).expect("remove fixture directory");
    }

    #[test]
    fn generated_credentials_use_protocol_safe_bytes() {
        let value = random_value("worker").expect("operating-system entropy");

        assert!(value.starts_with("worker-"));
        assert!(value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'));
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
        assert!(capabilities(CURRENT_VERSION).contains(&Capability::ControlPlaneObservation));
        assert!(capabilities(PREVIOUS_VERSION).contains(&Capability::ControlPlaneObservation));
        assert!(capabilities(CURRENT_VERSION).contains(&Capability::SubagentObservation));
        assert!(!capabilities(PREVIOUS_VERSION).contains(&Capability::SubagentObservation));
        assert!(capabilities(PREVIOUS_VERSION).contains(&Capability::AttachSnapshot));
    }

    #[test]
    fn subagent_events_are_hidden_from_version_four_connections() {
        let event = protocol::ControlEvent {
            event_sequence: 1,
            kind: EventKind::SubagentsChanged {
                runtime_id: RuntimeId::new("runtime-1").expect("runtime id"),
            },
        };
        let mut previous = Connection::new(std::sync::Arc::new(
            crate::identity::PeerContext::for_test(pohunek_platform::process::ProcessIdentity {
                pid: std::process::id(),
                start_identity: pohunek_platform::process::StartIdentity::new(1),
            }),
        ));
        previous.selected_version = Some(PREVIOUS_VERSION);
        previous.capabilities = capabilities(PREVIOUS_VERSION);
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
            runtime_id: RuntimeId::new("runtime-token").expect("runtime id"),
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
            runtime_id: grant.runtime_id.clone(),
            kind: FrameKind::Open {
                token,
                mode: grant.mode,
                after_offset: grant.after_offset,
                attach: grant.attach.clone(),
            },
        }
    }

    /// A data grant is redeemable only by the process that acquired the lease.
    ///
    /// The grant is minted for one kernel-attested peer; a different process —
    /// and a reused process id with a different start identity — must both be
    /// refused, and the refusal must leave the one-use token intact so a
    /// foreign attempt cannot burn a legitimate grant.
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
        let (server, _pty, directory) = shared;

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
        std::fs::remove_dir_all(directory).unwrap();
    }

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
                runtime_id: RuntimeId::new("runtime-snapshot").expect("runtime id"),
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
        let runtime_id = RuntimeId::new("runtime-error").expect("runtime id");
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
            &runtime_id,
            expected.clone(),
        )
        .await
        .expect("write typed stream error");
        let frame = protocol::read_frame(&mut reader)
            .await
            .expect("read error frame")
            .expect("error frame");

        assert_eq!(frame.header().stream_id, stream_id);
        assert_eq!(frame.header().runtime_id, runtime_id);
        assert_eq!(frame.header().kind, FrameKind::Error { error: expected });
    }

    #[tokio::test]
    async fn forced_output_close_is_framed_as_a_runtime_fault() {
        let stream_id = StreamId::new("forced-close").expect("stream id");
        let runtime_id = RuntimeId::new("runtime-forced-close").expect("runtime id");
        let (mut reader, mut writer) = tokio::io::duplex(4_096);

        write_runtime_output_event(
            &mut writer,
            CURRENT_VERSION,
            &stream_id,
            &runtime_id,
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
        let runtime_id = RuntimeId::new("runtime-1").expect("valid runtime");
        let (mut reader, mut writer) = tokio::io::duplex(payload.len() * 2);

        write_output_chunks(
            &mut writer,
            CURRENT_VERSION,
            &stream_id,
            &runtime_id,
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
        let runtime_id = RuntimeId::new("runtime-1").expect("valid runtime");
        let (mut reader, mut writer) = tokio::io::duplex(payload.len() * 2);
        let end = u64::try_from(payload.len()).expect("payload fits u64");

        write_observation_page(
            &mut writer,
            CURRENT_VERSION,
            &stream_id,
            &runtime_id,
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
        let runtime_id = RuntimeId::new("runtime-1").expect("valid runtime");
        let (mut reader, mut writer) = tokio::io::duplex(ansi.len() * 2);

        write_terminal_chunks(
            &mut writer,
            CURRENT_VERSION,
            &stream_id,
            &runtime_id,
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
}
