//! Serves the private daemon-worker Unix protocol.

// Rust guideline compliant 2026-07-27

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use pohunek_worker_protocol as protocol;
use protocol::{
    ActiveIdentityClaim, Capability, CloseReason, ControlCode, ControlError, ControlEvent,
    ControlMessage, ControlReader, ControlRequest, ControlResponse, ControlWriter, Cursor,
    DaemonId, DataFrame, DataToken, Dimensions, EventKind, ExitStatus, FrameHeader, FrameKind,
    Initialize, InspectSnapshot, LeaseChallenge, LeaseId, ProcessIdentity as WireProcessIdentity,
    ReportedLaunchIdentity, RequestKind, ResponseKind, RuntimeId, RuntimePhase as WireRuntimePhase,
    RuntimeScope, SessionId, StreamId, StreamMode, TerminalSnapshot as WireTerminalSnapshot,
    TokenClaims, TokenVault, TransactionId, Version, WorkerId, WriteAck, SUPPORTED_RANGE,
};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;
use tracing::{event, Level};

use crate::journal::{
    ActiveIdentity, ChildIdentity, JournalRecord, LaunchIdentity, RuntimeOutcome,
    RuntimePhase as JournalPhase,
};
use crate::{
    Command, ControllerLease, Exit, InputFragment, InputPlan, Journal, LeaseError, LeaseOwner,
    OutputChunk, OutputEvent, ProcessIdentity, PtyOwner, WorkerConfig, WorkerError,
};

/// Number of worker events buffered per control connection.
const EVENT_BUFFER: usize = 256;
/// Maximum outstanding one-use data tokens per worker.
const DATA_TOKEN_CAPACITY: usize = 4_096;
/// Entropy bytes used for opaque worker credentials and runtime IDs.
const RANDOM_BYTES: usize = 16;
/// Environment variables inherited from the systemd worker but never the child.
const WORKER_ONLY_ENV: [&str; 5] = [
    "NOTIFY_SOCKET",
    "WATCHDOG_PID",
    "WATCHDOG_USEC",
    "POHUNEK_CONTROLLER_TOKEN",
    "POHUNEK_BOOTSTRAP_TOKEN",
];

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
    shared: Arc<Shared>,
}

impl Server {
    /// Binds and journals an uninitialized worker.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] for invalid IDs, unsafe paths, or bind failure.
    pub async fn bind(args: ServerArgs) -> Result<Self, WorkerError> {
        args.config.validate()?;
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
        prepare_socket(&args.socket_path).await?;
        let listener =
            UnixListener::bind(&args.socket_path).map_err(|source| WorkerError::Socket {
                path: args.socket_path.clone(),
                source,
            })?;
        fs::set_permissions(&args.socket_path, fs::Permissions::from_mode(0o600)).map_err(
            |source| WorkerError::Socket {
                path: args.socket_path.clone(),
                source,
            },
        )?;

        let worker_start = process_start(std::process::id())?;
        let journal = Journal::new(&args.journal_path);
        let record = JournalRecord::bootstrap(
            args.session_id,
            args.worker_id,
            std::process::id(),
            worker_start.to_string(),
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
            tokens: Mutex::new(TokenState::new().map_err(|error| {
                WorkerError::Protocol(format!("token vault initialization failed: {error}"))
            })?),
            events,
            next_event: AtomicU64::new(1),
            started: Instant::now(),
            initialized_tx,
            lease_epoch_tx,
            shutdown: CancellationToken::new(),
        });
        Ok(Self { listener, shared })
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

        loop {
            tokio::select! {
                () = self.shared.shutdown.cancelled() => return Ok(()),
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
    shutdown: CancellationToken,
}

impl Shared {
    async fn record_never_initialized(&self) -> Result<(), WorkerError> {
        let record = {
            let mut state = self.state.lock().await;
            state.phase = WireRuntimePhase::Faulted;
            state.journal.phase = JournalPhase::NeverInitialized;
            state.journal.updated_at = timestamp();
            state.journal.clone()
        };
        persist(self.journal.clone(), record).await
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
    vault: TokenVault,
    grants: HashMap<DataToken, DataGrant>,
}

impl TokenState {
    fn new() -> Result<Self, protocol::TokenError> {
        Ok(Self {
            vault: TokenVault::new(DATA_TOKEN_CAPACITY)?,
            grants: HashMap::new(),
        })
    }
}

#[derive(Debug, Clone)]
struct DataGrant {
    lease_id: LeaseId,
    runtime_id: RuntimeId,
    stream_id: StreamId,
    mode: StreamMode,
    after_offset: Option<u64>,
}

#[derive(Debug)]
struct Connection {
    peer_pid: u32,
    peer_start: u64,
    daemon_id: Option<DaemonId>,
    selected_version: Option<Version>,
    challenge: Option<LeaseChallenge>,
    owner: Option<LeaseOwner>,
    lease_id: Option<LeaseId>,
}

impl Connection {
    fn new(peer_pid: u32, peer_start: u64) -> Self {
        Self {
            peer_pid,
            peer_start,
            daemon_id: None,
            selected_version: None,
            challenge: None,
            owner: None,
            lease_id: None,
        }
    }
}

async fn serve_connection(shared: Arc<Shared>, mut stream: UnixStream) -> Result<(), WorkerError> {
    verify_peer(&stream)?;
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
        serve_json(shared, prefixed).await
    } else {
        serve_data(shared, prefixed).await
    }
}

async fn serve_json(
    shared: Arc<Shared>,
    stream: PrefixStream<UnixStream>,
) -> Result<(), WorkerError> {
    let credentials = getsockopt(stream.inner(), PeerCredentials)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    let peer_pid = u32::try_from(credentials.pid())
        .map_err(|_range_error| WorkerError::Protocol("peer PID is invalid".to_owned()))?;
    let peer_start = process_start(peer_pid)?;
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
            serve_control(
                Arc::clone(&shared),
                &mut reader,
                &mut writer,
                request,
                peer_pid,
                peer_start,
            )
            .await
        }
        Ok(_) => Err(WorkerError::Protocol(
            "first control message must be a request".to_owned(),
        )),
        Err(_) => serve_identity_hook(shared, &mut writer, first).await,
    }
}

async fn serve_control<R, W>(
    shared: Arc<Shared>,
    reader: &mut ControlReader<R>,
    writer: &mut ControlWriter<W>,
    first: ControlRequest,
    peer_pid: u32,
    peer_start: u64,
) -> Result<(), WorkerError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut connection = Connection::new(peer_pid, peer_start);
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
        } => open_data_request(shared, connection, &scope, stream_id, mode, after_offset).await,
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

    ControlResponse {
        request_id,
        kind: result.unwrap_or_else(|error| ResponseKind::Error { error }),
    }
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
        capabilities: capabilities(),
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
    let lease_value = random_value("lease")
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let lease_id = LeaseId::new(lease_value)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let owner = LeaseOwner {
        daemon_id: daemon_id.to_string(),
        peer_pid: connection.peer_pid,
        peer_start_identity: connection.peer_start.to_string(),
    };
    shared
        .lease
        .acquire(owner.clone(), lease_id.to_string())
        .map_err(lease_control_error)?;
    connection.owner = Some(owner);
    connection.lease_id = Some(lease_id.clone());
    Ok(ResponseKind::ControllerAcquired {
        lease_id,
        capabilities: capabilities()
            .into_iter()
            .filter(|capability| requested.contains(capability))
            .collect(),
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
    let starting = {
        let mut state = shared.state.lock().await;
        state.phase = WireRuntimePhase::Starting;
        state.initialize_transaction = Some(initialize.transaction_id.clone());
        state.runtime_id = Some(runtime_id.clone());
        state.journal.phase = JournalPhase::Starting;
        state.journal.runtime_id = Some(runtime_id.to_string());
        state.journal.updated_at = timestamp();
        state.journal.clone()
    };
    persist_control(shared.journal.clone(), starting).await?;

    let command = command_from_initialize(shared, &initialize);
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
    let live = {
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
        state.journal.clone()
    };
    if let Err(error) = persist(shared.journal.clone(), live).await {
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
) -> Result<ResponseKind, ControlError> {
    validate_scope(shared, connection, scope).await?;
    let token_value = random_value("data")
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let token = DataToken::new(token_value)
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    let now = shared.now_ms();
    let ttl = u64::try_from(shared.config.data_token_ttl.as_millis()).unwrap_or(u64::MAX);
    let expires_at_ms = now.saturating_add(ttl);
    let grant = DataGrant {
        lease_id: scope.lease_id.clone(),
        runtime_id: scope.runtime_id.clone(),
        stream_id: stream_id.clone(),
        mode,
        after_offset,
    };
    let mut tokens = lock(&shared.tokens);
    let _ = tokens.vault.purge_expired(now);
    tokens
        .vault
        .insert(
            token.clone(),
            TokenClaims {
                lease_id: grant.lease_id.clone(),
                runtime_id: grant.runtime_id.clone(),
                stream_id: grant.stream_id.clone(),
                expires_at_ms,
            },
            now,
        )
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    tokens.grants.insert(token.clone(), grant);
    Ok(ResponseKind::DataStreamOpened {
        token,
        expires_at_ms,
    })
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
    pty.input()
        .execute(local)
        .await
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
    let pty = scoped_pty(shared, connection, scope).await?;
    let grace = {
        let mut state = shared.state.lock().await;
        state.stop_requested = true;
        state.phase = WireRuntimePhase::Stopping;
        state.stop_grace
    };
    let exit = pty
        .stop(transaction_id.as_str(), grace)
        .await
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))?;
    Ok(ResponseKind::Stopped {
        exit: Some(exit_status(&exit, true)),
    })
}

async fn acknowledge_request(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
) -> Result<ResponseKind, ControlError> {
    validate_scope(shared, connection, scope).await?;
    let record = {
        let mut state = shared.state.lock().await;
        if state.phase != WireRuntimePhase::Exited {
            return Err(invalid_state("runtime is not terminal"));
        }
        state.journal.terminal_acknowledged = true;
        state.journal.updated_at = timestamp();
        state.journal.clone()
    };
    persist_control(shared.journal.clone(), record).await?;
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
    })
}

fn spawn_runtime_monitors(shared: Arc<Shared>, pty: PtyOwner, runtime_id: RuntimeId) {
    let output_shared = Arc::clone(&shared);
    let output_pty = pty.clone();
    let output_runtime = runtime_id.clone();
    tokio::spawn(async move {
        if let Ok(mut subscriber) = output_pty.subscribe_output(Some(0)) {
            while let Some(event) = subscriber.recv().await {
                match event {
                    OutputEvent::Replay(_) | OutputEvent::TerminalSnapshot(_) => {}
                    OutputEvent::Output(chunk) => {
                        output_shared.emit(EventKind::OutputAdvanced {
                            runtime_id: output_runtime.clone(),
                            next_offset: chunk.offset.saturating_add(
                                u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX),
                            ),
                        });
                    }
                    OutputEvent::Gap { watermark, .. } => {
                        output_shared.emit(EventKind::TerminalChanged {
                            runtime_id: output_runtime.clone(),
                            watermark,
                        });
                    }
                    OutputEvent::Exit { next_offset } => {
                        let record = {
                            let mut state = output_shared.state.lock().await;
                            state.journal.next_output_offset = next_offset;
                            state.journal.updated_at = timestamp();
                            state.journal.clone()
                        };
                        if let Err(error) = persist(output_shared.journal.clone(), record).await {
                            event!(
                                name: "worker.journal.write.failed",
                                Level::ERROR,
                                error.type = "journal",
                                error.message = %error,
                                "worker journal write failed: {{error.message}}",
                            );
                        }
                        break;
                    }
                }
            }
        }
    });

    tokio::spawn(async move {
        let exit = match pty.wait_exit().await {
            Ok(exit) => exit,
            Err(error) => {
                shared.emit(EventKind::RuntimeFault {
                    runtime_id: Some(runtime_id),
                    error: control_error(ControlCode::RuntimeFault, error, false),
                });
                return;
            }
        };
        let (status, retention, record) = {
            let mut state = shared.state.lock().await;
            let status = exit_status(&exit, state.stop_requested);
            state.phase = WireRuntimePhase::Exited;
            state.exit = Some(status.clone());
            state.journal.phase = JournalPhase::Terminal;
            state.journal.outcome = Some(RuntimeOutcome {
                exit_code: exit.exit_code,
                signal: exit.signal.clone(),
                success: exit.success,
                exited_at: timestamp(),
                reason: if state.stop_requested {
                    "explicit_stop".to_owned()
                } else {
                    "natural_exit".to_owned()
                },
            });
            state.journal.next_output_offset = pty.output().next_offset();
            state.journal.updated_at = timestamp();
            (status, state.terminal_retention, state.journal.clone())
        };
        if let Err(error) = persist(shared.journal.clone(), record).await {
            event!(
                name: "worker.journal.write.failed",
                Level::ERROR,
                error.type = "journal",
                error.message = %error,
                "worker terminal journal write failed: {{error.message}}",
            );
        }
        shared.emit(EventKind::ChildExited {
            runtime_id,
            exit: status,
        });
        tokio::select! {
            () = tokio::time::sleep(retention) => shared.shutdown.cancel(),
            () = shared.shutdown.cancelled() => {}
        }
    });
}

#[expect(
    clippy::too_many_lines,
    reason = "the bidirectional data-stream select loop keeps lease, output, and input ordering in one place"
)]
async fn serve_data(
    shared: Arc<Shared>,
    stream: PrefixStream<UnixStream>,
) -> Result<(), WorkerError> {
    let (mut read_half, mut write_half) = tokio::io::split(stream);
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
    } = header.kind
    else {
        return Err(WorkerError::Protocol(
            "first data frame must open a stream".to_owned(),
        ));
    };
    let grant = {
        let mut tokens = lock(&shared.tokens);
        let grant = tokens
            .grants
            .remove(&token)
            .ok_or_else(|| WorkerError::Protocol("data token has no matching grant".to_owned()))?;
        tokens
            .vault
            .redeem(
                &token,
                &grant.lease_id,
                &grant.runtime_id,
                &grant.stream_id,
                shared.now_ms(),
            )
            .map_err(|error| WorkerError::Protocol(error.to_string()))?;
        grant
    };
    if grant.runtime_id != header.runtime_id
        || grant.stream_id != header.stream_id
        || grant.mode != mode
        || grant.after_offset != after_offset
    {
        return Err(WorkerError::Protocol(
            "data open frame does not match its grant".to_owned(),
        ));
    }
    let pty = {
        let state = shared.state.lock().await;
        state
            .pty
            .clone()
            .ok_or_else(|| WorkerError::Protocol("runtime is not live".to_owned()))?
    };
    let mut subscriber = pty
        .subscribe_output(after_offset)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    let mut lease_epoch = shared.lease_epoch_tx.subscribe();
    let version = header.version;
    let stream_id = header.stream_id;
    let runtime_id = header.runtime_id;

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
            output = subscriber.recv() => {
                let Some(output) = output else {
                    return Ok(());
                };
                write_output_event(
                    &mut write_half,
                    version,
                    &stream_id,
                    &runtime_id,
                    shared.config.data_payload_bytes,
                    output,
                ).await?;
            }
            input = protocol::read_frame(&mut read_half), if mode == StreamMode::Attach => {
                let input = input
                    .map_err(|error| WorkerError::Protocol(error.to_string()))?
                    .ok_or_else(|| WorkerError::Protocol("attach data stream closed".to_owned()))?;
                let (input_header, bytes) = input.into_parts();
                let FrameKind::Input { write_id } = input_header.kind else {
                    return Err(WorkerError::Protocol(
                        "attach data stream received a non-input frame".to_owned(),
                    ));
                };
                if input_header.runtime_id != runtime_id || input_header.stream_id != stream_id {
                    return Err(WorkerError::Protocol(
                        "attach input scope does not match stream".to_owned(),
                    ));
                }
                let byte_count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                pty.input().execute(InputPlan {
                    write_id: write_id.to_string(),
                    fragments: vec![InputFragment {
                        bytes,
                        delay_after: Duration::ZERO,
                    }],
                }).await.map_err(|error| WorkerError::Protocol(error.to_string()))?;
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
            }
        }
    }
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
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
        expires_at: String,
        reference_kind: Option<String>,
        native_reference: Option<String>,
    },
    IdentityRelease {
        provider: String,
        pid: u32,
        start_identity: u64,
        sequence: u64,
    },
}

#[derive(Debug, Serialize)]
struct HookResponse {
    ok: bool,
    launch_identity_accepted: bool,
}

#[expect(
    clippy::too_many_lines,
    reason = "identity report and release share one validated response path"
)]
async fn serve_identity_hook<W>(
    shared: Arc<Shared>,
    writer: &mut ControlWriter<W>,
    value: serde_json::Value,
) -> Result<(), WorkerError>
where
    W: AsyncWrite + Unpin + Send,
{
    let request: HookRequest =
        serde_json::from_value(value).map_err(|error| WorkerError::Protocol(error.to_string()))?;
    let record = match request {
        HookRequest::IdentityReport {
            provider,
            pid,
            start_identity,
            sequence,
            expires_at,
            reference_kind,
            native_reference,
        } => {
            let mut state = shared.state.lock().await;
            let pty = state
                .pty
                .as_ref()
                .ok_or_else(|| WorkerError::Protocol("runtime is not initialized".to_owned()))?;
            let root_identity = pty.identity().clone();
            validate_hook_process(pid, start_identity, root_identity.pid)?;
            let process = ChildIdentity {
                pid,
                process_group: root_identity.process_group,
                start_identity: start_identity.to_string(),
            };
            state.journal.active_identity = Some(ActiveIdentity {
                provider: provider.clone(),
                process: process.clone(),
                sequence,
                expires_at,
                reference_kind: reference_kind.clone(),
                native_reference: native_reference.clone(),
            });
            let mut launch_identity_accepted = false;
            if let (Some(reference_kind), Some(native_reference), Some(launch_base)) = (
                reference_kind,
                native_reference,
                state.launch_agent_base.as_ref(),
            ) {
                if &provider == launch_base {
                    if let Some(designated) =
                        designated_launch_process(root_identity.pid, launch_base)?
                    {
                        if designated.pid == pid && designated.start_identity == start_identity {
                            match state.journal.launch_identity.as_ref() {
                                None => {
                                    state.journal.launch_identity = Some(LaunchIdentity {
                                        provider,
                                        process,
                                        reference_kind,
                                        native_reference,
                                    });
                                    launch_identity_accepted = true;
                                }
                                Some(existing)
                                    if existing.process.pid == pid
                                        && existing.process.start_identity
                                            == start_identity.to_string()
                                        && existing.reference_kind == reference_kind
                                        && existing.native_reference == native_reference =>
                                {
                                    launch_identity_accepted = true;
                                }
                                Some(_) => {}
                            }
                        }
                    }
                }
            }
            state.journal.updated_at = timestamp();
            (state.journal.clone(), launch_identity_accepted)
        }
        HookRequest::IdentityRelease {
            provider,
            pid,
            start_identity,
            sequence,
        } => {
            let mut state = shared.state.lock().await;
            let pty = state
                .pty
                .as_ref()
                .ok_or_else(|| WorkerError::Protocol("runtime is not initialized".to_owned()))?;
            validate_hook_process(pid, start_identity, pty.identity().pid)?;
            if state
                .journal
                .active_identity
                .as_ref()
                .is_some_and(|active| {
                    active.provider == provider
                        && active.process.pid == pid
                        && active.process.start_identity == start_identity.to_string()
                        && sequence >= active.sequence
                })
            {
                state.journal.active_identity = None;
            }
            state.journal.updated_at = timestamp();
            (state.journal.clone(), false)
        }
    };
    persist(shared.journal.clone(), record.0).await?;
    if let Some(runtime_id) = shared.state.lock().await.runtime_id.clone() {
        shared.emit(EventKind::IdentityChanged { runtime_id });
    }
    writer
        .write(&HookResponse {
            ok: true,
            launch_identity_accepted: record.1,
        })
        .await
        .map_err(|error| WorkerError::Protocol(error.to_string()))
}

fn validate_hook_process(pid: u32, start_identity: u64, root_pid: u32) -> Result<(), WorkerError> {
    if process_start(pid)? != start_identity || !is_descendant(pid, root_pid)? {
        return Err(WorkerError::Protocol(
            "identity hook process is outside the managed PTY tree".to_owned(),
        ));
    }
    Ok(())
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

fn command_from_initialize(shared: &Shared, initialize: &Initialize) -> Command {
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

fn capabilities() -> Vec<Capability> {
    vec![
        Capability::AtomicReplay,
        Capability::TerminalSnapshot,
        Capability::DeduplicatedInput,
        Capability::IdentityHook,
    ]
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
    tokio::task::spawn_blocking(move || journal.write(&record))
        .await
        .map_err(|_join_error| WorkerError::Protocol("journal writer task failed".to_owned()))??;
    Ok(())
}

async fn persist_control(journal: Journal, record: JournalRecord) -> Result<(), ControlError> {
    persist(journal, record)
        .await
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, true))
}

fn verify_peer(stream: &UnixStream) -> Result<(), WorkerError> {
    let credentials = getsockopt(stream, PeerCredentials)
        .map_err(|error| WorkerError::Protocol(error.to_string()))?;
    if credentials.uid() != nix::unistd::Uid::effective().as_raw() {
        return Err(WorkerError::Protocol(
            "worker peer UID does not match effective UID".to_owned(),
        ));
    }
    Ok(())
}

async fn prepare_socket(path: &Path) -> Result<(), WorkerError> {
    let parent = path.parent().ok_or_else(|| WorkerError::Filesystem {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket has no parent"),
    })?;
    fs::create_dir_all(parent).map_err(|source| WorkerError::Filesystem {
        path: parent.to_path_buf(),
        source,
    })?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|source| {
        WorkerError::Filesystem {
            path: parent.to_path_buf(),
            source,
        }
    })?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
                return Err(WorkerError::Filesystem {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "worker socket path is not a real Unix socket",
                    ),
                });
            }
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
                    fs::remove_file(path).map_err(|source| WorkerError::Filesystem {
                        path: path.to_path_buf(),
                        source,
                    })?;
                }
                Err(source) => {
                    return Err(WorkerError::Socket {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(WorkerError::Filesystem {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    Ok(())
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
    let stat_path = format!("/proc/{pid}/stat");
    let stat = fs::read_to_string(&stat_path).map_err(|source| WorkerError::Filesystem {
        path: PathBuf::from(&stat_path),
        source,
    })?;
    parse_stat(&stat)
        .map(|(_, start)| start)
        .ok_or_else(|| WorkerError::Protocol("process stat is malformed".to_owned()))
}

fn process_parent(pid: u32) -> Result<u32, WorkerError> {
    let stat_path = format!("/proc/{pid}/stat");
    let stat = fs::read_to_string(&stat_path).map_err(|source| WorkerError::Filesystem {
        path: PathBuf::from(&stat_path),
        source,
    })?;
    parse_stat(&stat)
        .map(|(parent, _)| parent)
        .ok_or_else(|| WorkerError::Protocol("process stat is malformed".to_owned()))
}

fn parse_stat(stat: &str) -> Option<(u32, u64)> {
    let close = stat.rfind(')')?;
    let mut fields = stat[close + 1..].split_whitespace();
    let _state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    let start_identity = fields.nth(17)?.parse().ok()?;
    Some((parent, start_identity))
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
    root: u32,
    provider: &str,
) -> Result<Option<WireProcessIdentity>, WorkerError> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir("/proc").map_err(|source| WorkerError::Filesystem {
        path: PathBuf::from("/proc"),
        source,
    })? {
        let entry = entry.map_err(|source| WorkerError::Filesystem {
            path: PathBuf::from("/proc"),
            source,
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if !is_descendant(pid, root).unwrap_or(false) {
            continue;
        }
        let Ok(executable) = fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        let executable_name = executable
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !executable_name.contains(&provider.to_ascii_lowercase()) {
            continue;
        }
        if let Ok(start_identity) = process_start(pid) {
            candidates.push(WireProcessIdentity {
                pid,
                start_identity,
            });
        }
    }
    candidates.sort_by_key(|candidate| (candidate.start_identity, candidate.pid));
    Ok(candidates.into_iter().next())
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

    fn inner(&self) -> &S {
        &self.inner
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
    use super::{
        parse_stat, random_value, signal_number, write_output_chunks, write_terminal_chunks,
        PrefixStream, WireTerminalSnapshot,
    };
    use pohunek_worker_protocol::{
        self as protocol, Cursor, Dimensions, FrameKind, RuntimeId, StreamId, CURRENT_VERSION,
        MAX_DATA_PAYLOAD_BYTES,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Extra bytes force exactly one partial frame after a full wire payload.
    const OVERSIZED_PAYLOAD_EXTRA: usize = 257;

    #[test]
    fn proc_stat_parser_handles_spaces_and_parentheses_in_name() {
        let mut fields = vec!["S".to_owned(), "10".to_owned()];
        fields.extend((0..17).map(|value| value.to_string()));
        fields.push("4242".to_owned());
        let stat = format!("20 (name with ) parens) {}", fields.join(" "));

        assert_eq!(parse_stat(&stat), Some((10, 4242)));
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
