//! Control server (local Unix socket + `NetBird` TCP).
//!
//! Binds the control socket with owner-private permissions, recovers from a
//! stale socket left by a previous run, and serves newline-delimited JSON
//! requests using the shared `protocol` crate. Each connection is handled on its
//! own Tokio task so one client cannot stall another, and a panicking handler
//! cannot take down the daemon (per `docs/architecture.md` "Concurrency and
//! supervision").
//!
//! The same connection-serving code drives two transports: the local
//! [`ControlServer`] over a Unix socket and the [`RemoteServer`] over a `NetBird`
//! TCP listener (milestone 11). Everything below the accept loop is generic over
//! any `AsyncRead + AsyncWrite` stream, so the protocol and attach semantics are
//! identical across transports.
//!
//! The server handles `daemon.health` (milestone 2), the `session.*` lifecycle
//! methods (milestone 3), and `host.inspect` (milestone 11), and a `subscribe`
//! request turns the connection into a one-way stream of session lifecycle
//! events. Unknown methods receive a typed `method_not_found` error (the contract
//! for later milestones is already in the `protocol` crate).
//!
//! Attach streaming uses a separate connection: the first line carries an attach
//! prelude, then the connection switches from newline JSON to raw PTY bytes.

mod handler;

use std::ffi::{OsStr, OsString};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use pohunek_worker_protocol::{
    read_frame, write_frame, ControlCode, ControlError, DataFrame, FrameHeader, FrameKind, WriteId,
};
use protocol::{
    negotiate, ErrorClass, Event, ProtocolError, ProtocolVersion, ProtocolVersionRange, Request,
    Response, MAX_CONTROL_LINE_BYTES, SUPPORTED_PROTOCOL_VERSIONS,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};
use tracing::{error, info, warn};

use overlay::OverlayTransport;
use pohunek_paths::{validate_staged_socket_path, Platform, SocketKind, STAGED_SOCKET_PREFIX};
use pohunek_platform::filesystem::{
    EntryIdentity, EntryKind, FsError, MoveOutcome, StageOutcome, TrustedDir,
};

use crate::error::DaemonError;
use crate::governance::HostGovernanceService;
use crate::session::{RedeemedAttach, RedeemedRuntime, SessionRegistry};

#[cfg(not(test))]
use handler::Dispatch;
#[cfg(test)]
pub(crate) use handler::{dispatch_line, Dispatch};
pub use handler::{handle_request, DaemonState, HealthInfo};

/// Directory mode for the runtime dir: owner rwx only (`0700`).
///
/// The runtime dir holds the control socket and the daemon's local state.
/// Granting any group or other access would let a second local user reach
/// into the directory to connect to the control plane or read daemon state,
/// so it is restricted to the owning user. This is the outer access-control
/// boundary that backs the per-socket mode below.
const DIR_MODE: u32 = 0o700;
/// Socket mode: owner rw only (`0600`).
///
/// Anyone able to open the control socket can drive the daemon (create and
/// attach sessions, inspect the host). There is no further authentication on
/// the local transport, so the file mode *is* the access-control boundary:
/// limiting read/write to the owner keeps other local users off the control
/// plane.
const SOCKET_MODE: u32 = 0o600;
/// The bound control server, ready to accept connections.
#[derive(Debug)]
pub struct ControlServer {
    listener: UnixListener,
    socket_path: PathBuf,
    socket_dir: TrustedDir,
    socket_name: OsString,
    socket_identity: EntryIdentity,
    state: DaemonState,
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

    fn published_as(&mut self, name: &OsStr) {
        self.name = name.to_os_string();
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingSocket<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = remove_expected_socket(self.directory, &self.name, self.identity);
        }
    }
}

impl ControlServer {
    /// Bind the control socket at `socket_path` with a validated overlay registry.
    ///
    /// The parent directory is created (mode `0700`) if missing. If a socket
    /// file already exists, it is probed: a live daemon there is a hard error
    /// (the single-instance lock should have caught this first), while a stale
    /// socket (nothing listening) is removed and rebound (stale-socket recovery,
    /// per the plan's milestone 2).
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError`] on directory, permission, probe, or bind failure.
    pub async fn bind(
        socket_path: &Path,
        health: HealthInfo,
        governance: Arc<HostGovernanceService>,
        registry: pohunek_client::OverlayRegistry,
    ) -> Result<Self, DaemonError> {
        Self::bind_with_state(
            socket_path,
            DaemonState::new(health, SessionRegistry::default(), governance, registry),
        )
        .await
    }

    /// Bind the control socket with explicit shared daemon state.
    pub async fn bind_with_state(
        socket_path: &Path,
        state: DaemonState,
    ) -> Result<Self, DaemonError> {
        let platform = Platform::current().map_err(DaemonError::Paths)?;
        validate_staged_socket_path(socket_path, platform, SocketKind::Daemon)
            .map_err(DaemonError::Paths)?;
        let dir = socket_path.parent().ok_or_else(|| DaemonError::Socket {
            path: socket_path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket path has no parent directory",
            ),
        })?;
        let socket_name = socket_path
            .file_name()
            .ok_or_else(|| DaemonError::Socket {
                path: socket_path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "socket has no filename"),
            })?
            .to_os_string();

        let socket_dir = TrustedDir::open_or_create_absolute(dir, DIR_MODE)?;
        recover_stale_socket(&socket_dir, &socket_name, socket_path).await?;

        let (bind_name, std_listener, socket_identity) =
            socket_dir.bind_unix_listener_staged(STAGED_SOCKET_PREFIX, SOCKET_MODE)?;
        let mut pending = PendingSocket::new(&socket_dir, bind_name.clone(), socket_identity);
        let bind_path = dir.join(&bind_name);
        std_listener
            .set_nonblocking(true)
            .map_err(|source| DaemonError::Socket {
                path: bind_path.clone(),
                source,
            })?;
        let listener =
            UnixListener::from_std(std_listener).map_err(|source| DaemonError::Socket {
                path: bind_path.clone(),
                source,
            })?;
        match socket_dir.move_no_replace(&bind_name, &socket_dir, &socket_name) {
            Ok(MoveOutcome::Moved) => pending.published_as(&socket_name),
            Err(error @ FsError::CommittedDurabilityUncertain { .. }) => {
                pending.published_as(&socket_name);
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
            Ok(MoveOutcome::DestinationExists) => {
                drop(listener);
                return Err(DaemonError::Socket {
                    path: socket_path.to_path_buf(),
                    source: io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "daemon socket destination became occupied while binding",
                    ),
                });
            }
            Ok(_) => {
                return Err(DaemonError::Socket {
                    path: socket_path.to_path_buf(),
                    source: io::Error::new(
                        io::ErrorKind::Unsupported,
                        "unsupported socket activation outcome",
                    ),
                });
            }
        }
        if socket_dir.entry_identity_with_mode(&socket_name, EntryKind::Socket, SOCKET_MODE)?
            != Some(socket_identity)
        {
            drop(listener);
            return Err(DaemonError::Socket {
                path: socket_path.to_path_buf(),
                source: io::Error::other("activated daemon socket identity changed"),
            });
        }
        pending.disarm();
        drop(pending);

        info!(socket = %socket_path.display(), "control socket bound");
        Ok(Self {
            listener,
            socket_path: socket_path.to_path_buf(),
            socket_dir,
            socket_name,
            socket_identity,
            state,
        })
    }

    /// The bound socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run the accept loop until `shutdown` resolves.
    ///
    /// Each accepted connection is served on its own task. The loop itself never
    /// returns an error for a single bad connection; transient accept errors are
    /// logged and the loop continues.
    pub async fn serve(self, shutdown: impl std::future::Future<Output = ()>) {
        tokio::pin!(shutdown);
        let state = self.state.clone();
        loop {
            tokio::select! {
                () = &mut shutdown => {
                    info!("shutdown signal received; stopping accept loop");
                    break;
                }
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            let state = state.clone();
                            tokio::spawn(async move {
                                if let Err(err) = serve_connection(stream, state).await {
                                    warn!(error = %err, "control connection ended with error");
                                }
                            });
                        }
                        Err(err) => {
                            // A failed accept is transient (e.g. fd limit); log
                            // and keep serving rather than crashing the daemon.
                            error!(error = %err, "accept failed");
                        }
                    }
                }
            }
        }
        // Best-effort cleanup so the next start does not need stale-socket
        // recovery. A failure here is non-fatal.
        if let Err(err) =
            remove_expected_socket(&self.socket_dir, &self.socket_name, self.socket_identity)
        {
            warn!(error = %err, socket = %self.socket_path.display(), "failed to remove socket on shutdown");
        }
    }
}

/// A listening socket produced by the opener passed to [`RemoteServer::bind_with`].
///
/// The constructor records how much [`RemoteServer::bind_with`] may trust the
/// socket's address.
#[derive(Debug)]
pub struct OpenedListener {
    listener: TcpListener,
    verify: bool,
}

impl OpenedListener {
    /// Wrap a socket the OS bound for exactly the requested address.
    ///
    /// The address is not compared again: the kernel (or a libc-level test
    /// interposer standing in for an overlay interface) owns the mapping from
    /// the requested address to the bound one.
    #[must_use]
    pub fn os_bound(listener: TcpListener) -> Self {
        Self {
            listener,
            verify: false,
        }
    }

    /// Wrap a socket the caller bound earlier.
    ///
    /// [`RemoteServer::bind_with`] requires its local address to equal the
    /// validated address exactly and rejects it otherwise.
    #[must_use]
    pub fn held(listener: TcpListener) -> Self {
        Self {
            listener,
            verify: true,
        }
    }
}

/// The bound remote (overlay TCP) control server, ready to accept connections.
///
/// Identical protocol and attach semantics to [`ControlServer`]; only the
/// transport differs. Binding is gated by [`OverlayTransport::validate_bind_addr`]
/// so the daemon never exposes the control port on a non-overlay interface.
#[derive(Debug)]
pub struct RemoteServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    state: DaemonState,
}

impl RemoteServer {
    /// Bind a TCP control listener at `addr`.
    ///
    /// FAILS CLOSED: `addr.ip()` is validated against the overlay's trusted
    /// range ([`OverlayTransport::validate_bind_addr`]) **before** the socket is
    /// opened, so an invalid or non-member address never reaches the OS bind. This is the
    /// authoritative gate that keeps the control port off public, RFC1918, and
    /// loopback interfaces.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::OverlayBind`] when the address is not a valid
    /// member of the overlay's range, or [`DaemonError::Socket`] on bind failure.
    pub async fn bind(
        addr: SocketAddr,
        state: DaemonState,
        transport: &dyn OverlayTransport,
    ) -> Result<Self, DaemonError> {
        Self::bind_with(addr, state, transport, |addr| async move {
            TcpListener::bind(addr).await.map(OpenedListener::os_bound)
        })
        .await
    }

    /// Bind a TCP control listener at `addr` using a caller-supplied socket opener.
    ///
    /// Runs the same fail-closed overlay validation as [`bind`](Self::bind)
    /// before `open` is called, so the opener only ever sees validated
    /// addresses. `open` supplies the listening socket for that address, which
    /// lets a caller that already holds a bound socket hand it over instead of
    /// releasing and re-binding the port. An [`OpenedListener::held`] listener
    /// must be bound to exactly `addr` (IP and port); any other socket is
    /// dropped unserved so the control port cannot leave the validated overlay
    /// address. An [`OpenedListener::os_bound`] listener comes straight from
    /// the OS bind of `addr` and is trusted to match it.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::OverlayBind`] when the address is not a valid
    /// member of the overlay's range or when `open` returns a listener bound
    /// to a different address or port, or [`DaemonError::Socket`] when `open`
    /// fails or the listener's local address cannot be read.
    pub async fn bind_with<F, Fut>(
        addr: SocketAddr,
        state: DaemonState,
        transport: &dyn OverlayTransport,
        open: F,
    ) -> Result<Self, DaemonError>
    where
        F: FnOnce(SocketAddr) -> Fut,
        Fut: std::future::Future<Output = std::io::Result<OpenedListener>>,
    {
        if let Err(err) = transport.validate_bind_addr(addr.ip()) {
            return Err(DaemonError::OverlayBind {
                addr: addr.ip(),
                reason: err.to_string(),
            });
        }

        let OpenedListener { listener, verify } =
            open(addr).await.map_err(|source| DaemonError::Socket {
                path: PathBuf::from(addr.to_string()),
                source,
            })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| DaemonError::Socket {
                path: PathBuf::from(addr.to_string()),
                source,
            })?;

        if verify && local_addr != addr {
            return Err(DaemonError::OverlayBind {
                addr: local_addr.ip(),
                reason: format!(
                    "opener returned a listener bound to {local_addr} instead of the validated {addr}"
                ),
            });
        }

        info!(addr = %local_addr, "remote control listener bound");
        Ok(Self {
            listener,
            local_addr,
            state,
        })
    }

    /// Wrap an already-bound listener WITHOUT `NetBird` validation.
    ///
    /// For tests and internal use only: the loopback-TCP stand-in in CI binds
    /// `127.0.0.1:0` and wraps it here, which the production [`bind`](Self::bind)
    /// path would (correctly) refuse. Production code must use
    /// [`bind`](Self::bind) so the fail-closed validation runs.
    #[must_use]
    pub fn from_listener(listener: TcpListener, state: DaemonState) -> Self {
        // local_addr() on an already-bound listener is infallible in practice;
        // fall back to the unspecified address rather than panicking if the OS
        // ever surprises us, so a test helper cannot bring down the process.
        let local_addr = listener.local_addr().unwrap_or_else(|err| {
            warn!(error = %err, "remote listener local_addr unavailable; reporting 0.0.0.0:0");
            SocketAddr::from(([0, 0, 0, 0], 0))
        });
        Self {
            listener,
            local_addr,
            state,
        }
    }

    /// The bound local address.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Run the accept loop until `shutdown` resolves.
    ///
    /// Each accepted connection is served on its own task via the same
    /// [`serve_connection`] used by the Unix server. The loop never returns an
    /// error for a single bad connection; transient accept errors are logged and
    /// the loop continues. The peer address is logged at info on each accept.
    pub async fn serve(self, shutdown: impl std::future::Future<Output = ()>) {
        tokio::pin!(shutdown);
        let state = self.state.clone();
        loop {
            tokio::select! {
                () = &mut shutdown => {
                    info!("shutdown signal received; stopping remote accept loop");
                    break;
                }
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            info!(peer = %peer, "remote control connection accepted");
                            let state = state.clone();
                            tokio::spawn(async move {
                                if let Err(err) = serve_connection(stream, state).await {
                                    warn!(error = %err, "remote control connection ended with error");
                                }
                            });
                        }
                        Err(err) => {
                            // A failed accept is transient (e.g. fd limit); log
                            // and keep serving rather than crashing the daemon.
                            error!(error = %err, "remote accept failed");
                        }
                    }
                }
            }
        }
    }
}

/// Serve one control connection: read newline-delimited JSON requests, dispatch
/// each, and write back one response line per request.
///
/// Generic over the underlying stream so the same logic serves a local Unix
/// connection ([`ControlServer`]) and a `NetBird` TCP connection ([`RemoteServer`])
/// without divergence.
///
/// A `subscribe` request is the exception: after its OK ack the connection turns
/// into a one-way stream of session lifecycle events ([`run_event_subscription`])
/// and is consumed there until the client disconnects.
async fn serve_connection<S>(stream: S, state: DaemonState) -> Result<(), io::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let codec = LinesCodec::new_with_max_length(MAX_CONTROL_LINE_BYTES);
    let mut framed = Framed::new(stream, codec);
    let mut negotiated_version = None;

    while let Some(line) = framed.next().await {
        let line = match line {
            Ok(line) => line,
            Err(LinesCodecError::MaxLineLengthExceeded) => {
                warn!("control line exceeded max length; closing connection");
                break;
            }
            Err(LinesCodecError::Io(err)) => return Err(err),
        };

        if let Ok(request) = serde_json::from_str::<Request>(&line) {
            if let Some(rejection) = enforce_connection_version(
                &request,
                &mut negotiated_version,
                SUPPORTED_PROTOCOL_VERSIONS,
            ) {
                let response = serde_json::to_string(&rejection)
                    .expect("validated response serialization is infallible");
                framed.send(response).await.map_err(codec_to_io)?;
                continue;
            }
        }

        match handler::dispatch_line(&line, &state).await {
            Dispatch::Reply(response_line) => {
                framed.send(response_line).await.map_err(codec_to_io)?;
            }
            Dispatch::Subscribe(ack_line, version) => {
                // Subscribe BEFORE sending the ack so no event emitted between
                // the ack and the recv loop is missed.
                let mut session_events = state.sessions.subscribe();
                let (_notification_sender, mut notification_events) =
                    if let Some(notifications) = &state.notifications {
                        (None, notifications.subscribe())
                    } else {
                        let (sender, receiver) = broadcast::channel(1);
                        (Some(sender), receiver)
                    };
                framed.send(ack_line).await.map_err(codec_to_io)?;
                run_event_subscription(
                    &mut framed,
                    &mut session_events,
                    &mut notification_events,
                    version,
                )
                .await?;
                // The connection is consumed by the subscription stream.
                return Ok(());
            }
            Dispatch::Attach(stream_id) => {
                run_attach_connection(framed, state.sessions.clone(), stream_id).await?;
                return Ok(());
            }
        }
    }
    Ok(())
}

fn enforce_connection_version(
    request: &Request,
    frozen: &mut Option<ProtocolVersion>,
    supported: ProtocolVersionRange,
) -> Option<Response> {
    let Ok(selected) = negotiate(request.version_range(), supported) else {
        let current = (*frozen)?;
        let frozen_range = ProtocolVersionRange::new(current, current)
            .expect("a frozen protocol version forms a valid singleton range");
        return Some(
            Response::err(
                current,
                request.id(),
                ProtocolError::version_mismatch(request.version_range(), frozen_range),
            )
            .expect("deserialized request ids satisfy response validation"),
        );
    };
    match frozen {
        None => {
            *frozen = Some(selected);
            None
        }
        Some(current) if *current == selected => None,
        Some(current) => {
            let frozen_range = ProtocolVersionRange::new(*current, *current)
                .expect("a frozen protocol version forms a valid singleton range");
            Some(
                Response::err(
                    *current,
                    request.id(),
                    ProtocolError::version_mismatch(request.version_range(), frozen_range),
                )
                .expect("deserialized request ids satisfy response validation"),
            )
        }
    }
}

async fn run_attach_connection<S>(
    mut framed: Framed<S, LinesCodec>,
    registry: SessionRegistry,
    stream_id: String,
) -> Result<(), io::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut attach = match registry.redeem_attach(&stream_id).await {
        Ok(attach) => attach,
        Err(err) => {
            let response = Response::err(protocol::PROTOCOL_VERSION, stream_id, err)
                .expect("validated stream ids satisfy response validation");
            framed
                .send(handler::serialize_response(&response))
                .await
                .map_err(codec_to_io)?;
            return Ok(());
        }
    };

    let parts = framed.into_parts();
    let mut stream = parts.io;
    let bridge_result = run_attach_bridge(&mut stream, &mut attach, parts.read_buf.to_vec()).await;
    let failure = bridge_result
        .as_ref()
        .err()
        .and_then(AttachBridgeError::protocol_error);
    registry.finish_attach(&attach.stream_id, failure).await;
    bridge_result.map_err(AttachBridgeError::into_io)
}

async fn run_attach_bridge<S>(
    stream: &mut S,
    attach: &mut RedeemedAttach,
    initial_input: Vec<u8>,
) -> Result<(), AttachBridgeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let attach_stream_id = attach.stream_id.clone();
    let session_id = attach.session_id.clone();
    let cancel = attach.cancel.clone();
    match &mut attach.runtime {
        RedeemedRuntime::Worker(data) => {
            run_worker_attach_bridge(
                stream,
                &attach_stream_id,
                &session_id,
                &cancel,
                data,
                initial_input,
            )
            .await
        }
    }
}

async fn run_worker_attach_bridge<S>(
    stream: &mut S,
    attach_stream_id: &str,
    session_id: &protocol::SessionId,
    cancel: &tokio_util::sync::CancellationToken,
    data: &mut crate::runtime::DataStream,
    initial_input: Vec<u8>,
) -> Result<(), AttachBridgeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let identity = WorkerFrameIdentity {
        version: data.version,
        stream_id: data.stream_id.clone(),
        worker_instance_id: data.worker_instance_id.clone(),
    };
    bridge_worker_frames(
        stream,
        &mut data.stream,
        &identity,
        attach_stream_id,
        session_id,
        cancel,
        initial_input,
    )
    .await
}

/// Frame identity every frame of one worker attach stream carries.
#[derive(Debug)]
struct WorkerFrameIdentity {
    version: pohunek_worker_protocol::Version,
    stream_id: pohunek_worker_protocol::StreamId,
    worker_instance_id: pohunek_worker_protocol::WorkerInstanceId,
}

/// Copies worker frames to the public stream and public bytes to the worker.
///
/// One worker read stays pending across every `select!` iteration (see
/// [`read_owned_frame`]), so public input arriving while a worker frame is
/// only partly received never discards the bytes already consumed.
async fn bridge_worker_frames<S, W>(
    stream: &mut S,
    worker: &mut W,
    identity: &WorkerFrameIdentity,
    attach_stream_id: &str,
    session_id: &protocol::SessionId,
    cancel: &tokio_util::sync::CancellationToken,
    initial_input: Vec<u8>,
) -> Result<(), AttachBridgeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    W: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (worker_read, mut worker_write) = tokio::io::split(worker);
    let mut write_sequence = 1_u64;
    if !initial_input.is_empty() {
        send_worker_attach_input(&mut worker_write, identity, write_sequence, initial_input)
            .await
            .map_err(AttachBridgeError::worker_stream)?;
        write_sequence = next_attach_sequence(write_sequence)?;
    }
    let mut input = [0_u8; 8 * 1024];
    let mut snapshot_started = false;
    let next_frame = read_owned_frame(worker_read);
    tokio::pin!(next_frame);

    loop {
        tokio::select! {
            (worker_read, frame) = &mut next_frame => {
                next_frame.set(read_owned_frame(worker_read));
                let Some(frame) = frame.map_err(AttachBridgeError::worker_stream)? else {
                    break;
                };
                let (header, payload) = frame.into_parts();
                if header.stream_id != identity.stream_id || header.worker_instance_id != identity.worker_instance_id {
                    return Err(AttachBridgeError::worker_message(
                        "worker attach frame identity mismatch",
                    ));
                }
                match header.kind {
                    FrameKind::Replay { .. } => {
                        return Err(AttachBridgeError::worker_message(
                            "snapshot attachment received historical replay",
                        ));
                    }
                    FrameKind::ObservationStart { .. } => {
                        return Err(AttachBridgeError::worker_message(
                            "attach stream received an observation frame",
                        ));
                    }
                    FrameKind::Output { .. } if !snapshot_started => {
                        return Err(AttachBridgeError::worker_message(
                            "snapshot attachment received live output before its repaint",
                        ));
                    }
                    FrameKind::Output { .. } | FrameKind::TerminalSnapshot { .. } => {
                        snapshot_started = true;
                        stream
                            .write_all(&payload)
                            .await
                            .map_err(AttachBridgeError::client_io)?;
                    }
                    FrameKind::Gap { .. } => snapshot_started = false,
                    FrameKind::InputAck { .. } => {}
                    FrameKind::Exit { .. } | FrameKind::Close { .. } => break,
                    FrameKind::Error { error } => {
                        warn!(
                            stream_id = attach_stream_id,
                            session_id = %session_id.0,
                            code = ?error.code,
                            "worker attach stream reported an error"
                        );
                        return Err(AttachBridgeError::worker_control(error));
                    }
                    FrameKind::Open { .. }
                    | FrameKind::AttachReady { .. }
                    | FrameKind::Input { .. } => {
                        return Err(AttachBridgeError::worker_message(
                            "worker sent an invalid attach frame",
                        ));
                    }
                }
            }
            read = stream.read(&mut input) => {
                let count = read.map_err(AttachBridgeError::client_io)?;
                if count == 0 {
                    break;
                }
                send_worker_attach_input(
                    &mut worker_write,
                    identity,
                    write_sequence,
                    input[..count].to_vec(),
                )
                .await
                .map_err(AttachBridgeError::worker_stream)?;
                write_sequence = next_attach_sequence(write_sequence)?;
            }
            () = cancel.cancelled() => break,
        }
    }
    Ok(())
}

/// Reads one worker frame and hands the reader back with the result.
///
/// [`read_frame`] is not cancel-safe: dropping it after part of a frame was
/// consumed loses those bytes and desynchronizes the stream. Owning the reader
/// lets the caller keep one read pending across `select!` iterations instead.
async fn read_owned_frame<R>(
    mut reader: R,
) -> (
    R,
    Result<Option<DataFrame>, pohunek_worker_protocol::FrameError>,
)
where
    R: AsyncRead + Unpin + Send,
{
    let frame = read_frame(&mut reader).await;
    (reader, frame)
}

fn next_attach_sequence(sequence: u64) -> Result<u64, AttachBridgeError> {
    sequence
        .checked_add(1)
        .ok_or_else(|| AttachBridgeError::worker_message("attach input sequence was exhausted"))
}

#[derive(Debug)]
struct AttachBridgeError {
    source: io::Error,
    protocol: Option<ProtocolError>,
}

impl AttachBridgeError {
    fn client_io(source: io::Error) -> Self {
        Self {
            source,
            protocol: None,
        }
    }

    fn worker_stream(error: impl std::fmt::Display) -> Self {
        Self::worker_message(error.to_string())
    }

    fn worker_message(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            source: io::Error::other(message.clone()),
            protocol: Some(ProtocolError::new(
                ErrorClass::Runtime,
                "worker_attach_stream_failed",
                message,
                None,
            )),
        }
    }

    fn worker_control(error: ControlError) -> Self {
        let code = match error.code {
            ControlCode::WorkerProtocolIncompatible => "worker_protocol_incompatible",
            ControlCode::ControllerBusy => "worker_controller_busy",
            ControlCode::IdentityMismatch => "worker_identity_mismatch",
            ControlCode::InvalidState => "worker_invalid_state",
            ControlCode::InvalidRequest => "worker_invalid_request",
            ControlCode::InvalidDataToken => "worker_invalid_data_token",
            ControlCode::WriteOutcomeUnknown => "worker_write_outcome_unknown",
            ControlCode::RuntimeFault => "worker_runtime_fault",
            ControlCode::WorkerFeatureUnavailable => "worker_feature_unavailable",
            ControlCode::ObservationLimitExceeded => "worker_observation_limit_exceeded",
        };
        Self {
            source: io::Error::other(error.message.clone()),
            protocol: Some(ProtocolError::new(
                ErrorClass::Runtime,
                code,
                error.message,
                None,
            )),
        }
    }

    fn protocol_error(&self) -> Option<ProtocolError> {
        self.protocol.clone()
    }

    fn into_io(self) -> io::Error {
        self.source
    }
}

async fn send_worker_attach_input<W>(
    writer: &mut W,
    identity: &WorkerFrameIdentity,
    sequence: u64,
    bytes: Vec<u8>,
) -> Result<(), io::Error>
where
    W: AsyncWrite + Unpin + Send,
{
    // Raw attach input uses stream-scoped monotonic write IDs (RFC §13.1). The
    // per-bridge `sequence` restarts at 1 for every attach stream, so it must be
    // salted with the stream identity; otherwise a reattach or a second
    // concurrent attach to the same session reuses `attach-1` with different
    // content, and the worker's per-runtime input dedup rejects it as a reused
    // write id with conflicting content, closing the stream.
    let write_id = WriteId::new(format!("attach-{}-{sequence}", identity.stream_id))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let frame = DataFrame::new(
        FrameHeader {
            version: identity.version,
            stream_id: identity.stream_id.clone(),
            worker_instance_id: identity.worker_instance_id.clone(),
            kind: FrameKind::Input { write_id },
        },
        bytes,
    )
    .map_err(io::Error::other)?;
    write_frame(writer, &frame).await.map_err(io::Error::other)
}

/// Stream control-plane events to a subscribed client until it disconnects.
///
/// Each received [`Event`] is written as one JSON line. Further input from the
/// client is ignored (a subscription is one-way in this milestone); a closed or
/// broken connection ends the stream. A lagging subscriber (slow reader) drops
/// the oldest events with a warning rather than tearing down the connection.
async fn run_event_subscription<S>(
    framed: &mut Framed<S, LinesCodec>,
    session_events: &mut broadcast::Receiver<Event>,
    notification_events: &mut broadcast::Receiver<Event>,
    version: protocol::ProtocolVersion,
) -> Result<(), io::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            incoming = framed.next() => match incoming {
                // Client closed the connection or sent an unframeable line.
                None | Some(Err(_)) => break,
                // Ignore any further input on a one-way subscription in M3.
                Some(Ok(_)) => {}
            },
            evt = session_events.recv() => match evt {
                Ok(event) => {
                    send_event_line(framed, &event.with_version(version)).await?;
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "event subscriber lagged; some events were dropped");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            evt = notification_events.recv() => match evt {
                Ok(event) => {
                    send_event_line(framed, &event.with_version(version)).await?;
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "event subscriber lagged; some events were dropped");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    Ok(())
}

async fn send_event_line<S>(
    framed: &mut Framed<S, LinesCodec>,
    event: &Event,
) -> Result<(), io::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let line = serde_json::to_string(event).expect("Event serialization is infallible");
    framed.send(line).await.map_err(codec_to_io)
}

/// Map a line-codec send error to an [`io::Error`] for connection-level handling.
fn codec_to_io(err: LinesCodecError) -> io::Error {
    match err {
        LinesCodecError::Io(io) => io,
        LinesCodecError::MaxLineLengthExceeded => io::Error::new(
            io::ErrorKind::InvalidData,
            "response exceeded max line length",
        ),
    }
}

/// Probe an existing socket file and recover if it is stale.
///
/// If `path` does not exist, do nothing. If it exists and is connectable, a live
/// daemon is there (the single-instance lock should normally prevent reaching
/// here): return an error. If it exists but refuses connection, treat it as a
/// stale socket from a previous run and remove it.
async fn recover_stale_socket(
    directory: &TrustedDir,
    name: &OsStr,
    path: &Path,
) -> Result<(), DaemonError> {
    let Some(identity) =
        directory.entry_identity_with_mode(name, EntryKind::Socket, SOCKET_MODE)?
    else {
        return Ok(());
    };
    match UnixStream::connect(path).await {
        Ok(_) => Err(DaemonError::Socket {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::AddrInUse,
                "a live daemon is already listening on this socket",
            ),
        }),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            warn!(socket = %path.display(), "removing stale socket from a previous run");
            remove_expected_socket(directory, name, identity)?;
            Ok(())
        }
        Err(source) => Err(DaemonError::Socket {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn remove_expected_socket(
    directory: &TrustedDir,
    name: &OsStr,
    identity: EntryIdentity,
) -> Result<(), DaemonError> {
    match directory.stage_random(name, ".pohunek-socket-stale-", identity)? {
        StageOutcome::Staged(entry) => {
            let _ = entry.remove()?;
        }
        StageOutcome::Missing | StageOutcome::IdentityChanged => {}
        StageOutcome::DestinationExists => {
            return Err(DaemonError::Socket {
                path: directory.path().to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "socket cleanup collision budget exhausted",
                ),
            });
        }
        _ => {
            return Err(DaemonError::Socket {
                path: directory.path().to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported socket staging outcome",
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod pending_socket_tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn pending_socket_guard_cleans_staged_and_published_names() {
        let temporary = pohunek_test_support::tempdir().expect("create socket guard fixture");
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(DIR_MODE))
            .expect("set private fixture mode");
        let directory = TrustedDir::open_absolute(temporary.path(), DIR_MODE)
            .expect("open socket guard fixture");

        let (staged_name, listener, identity) = directory
            .bind_unix_listener_staged(".s", SOCKET_MODE)
            .expect("bind staged socket");
        drop(listener);
        drop(PendingSocket::new(
            &directory,
            staged_name.clone(),
            identity,
        ));
        assert!(!temporary.path().join(&staged_name).exists());

        let (staged_name, listener, identity) = directory
            .bind_unix_listener_staged(".s", SOCKET_MODE)
            .expect("bind second staged socket");
        let mut pending = PendingSocket::new(&directory, staged_name.clone(), identity);
        assert_eq!(
            directory
                .move_no_replace(&staged_name, &directory, "daemon.sock")
                .expect("publish socket"),
            MoveOutcome::Moved
        );
        pending.published_as(OsStr::new("daemon.sock"));
        drop(listener);
        drop(pending);
        assert!(!temporary.path().join("daemon.sock").exists());
    }
}

#[cfg(test)]
mod tests {
    use pohunek_worker_protocol::{
        CloseReason, ControlCode, ControlError, Cursor, DataFrame, Dimensions, FrameHeader,
        FrameKind, StreamId, TerminalSnapshot, Version, WorkerInstanceId,
    };
    use protocol::{ProtocolVersion, ProtocolVersionRange, Request};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use tokio_util::sync::CancellationToken;

    use super::{
        bridge_worker_frames, enforce_connection_version, read_frame, run_worker_attach_bridge,
        write_frame, WorkerFrameIdentity,
    };
    use crate::runtime::DataStream;

    /// Worker-side buffering of the partial-frame regression stream.
    ///
    /// With one byte of buffering a fake-worker write returns only after the
    /// bridge has read every earlier byte, which pins the bridge mid-frame.
    const ONE_BYTE_BUFFER: usize = 1;
    /// Bytes of the first worker frame sent before public input arrives: the
    /// four-byte header length plus the opening bytes of its JSON header.
    const PARTIAL_FRAME_BYTES: usize = 6;
    /// Buffering of the public side of the partial-frame regression stream.
    const PUBLIC_BUFFER: usize = 1024;

    fn data_stream(stream: UnixStream) -> DataStream {
        DataStream {
            stream,
            version: Version::new(1).expect("version"),
            stream_id: StreamId::new("a-typed-error").expect("stream id"),
            worker_instance_id: WorkerInstanceId::new("runtime-typed-error").expect("runtime id"),
            dimension_update: None,
        }
    }

    #[test]
    fn connection_freezes_first_negotiated_version_and_rejects_switches() {
        let v1 = ProtocolVersion::new(1).expect("v1");
        let v2 = ProtocolVersion::new(2).expect("v2");
        let supported = ProtocolVersionRange::new(v1, v2).expect("supported range");
        let first: Request = serde_json::from_value(serde_json::json!({
            "v": {"minimum": 1, "maximum": 1},
            "id": "first",
            "method": "daemon.health",
            "params": null
        }))
        .expect("first request");
        let switching: Request = serde_json::from_value(serde_json::json!({
            "v": {"minimum": 1, "maximum": 2},
            "id": "switch",
            "method": "subscribe",
            "params": null
        }))
        .expect("switch request");
        let incompatible: Request = serde_json::from_value(serde_json::json!({
            "v": {"minimum": 3, "maximum": 3},
            "id": "incompatible",
            "method": "daemon.health",
            "params": null
        }))
        .expect("later incompatible request");
        let mut frozen = None;

        assert!(enforce_connection_version(&first, &mut frozen, supported).is_none());
        assert_eq!(frozen, Some(v1));
        let rejection = enforce_connection_version(&switching, &mut frozen, supported)
            .expect("later version switch must be rejected before subscription");
        assert_eq!(rejection.version(), v1);
        assert_eq!(
            rejection.into_result().expect_err("switch rejection").code,
            "version_mismatch"
        );
        assert_eq!(frozen, Some(v1), "subscription events remain pinned to v1");
        let incompatible_rejection =
            enforce_connection_version(&incompatible, &mut frozen, supported)
                .expect("later no-overlap request uses the frozen envelope");
        assert_eq!(incompatible_rejection.version(), v1);
        assert_eq!(
            incompatible_rejection
                .into_result()
                .expect_err("no-overlap rejection")
                .code,
            "version_mismatch"
        );
    }

    #[tokio::test]
    async fn worker_attach_bridge_preserves_typed_worker_error() {
        let (daemon_worker, mut fake_worker) = UnixStream::pair().expect("worker stream pair");
        let mut data = data_stream(daemon_worker);
        let frame = DataFrame::new(
            FrameHeader {
                version: data.version,
                stream_id: data.stream_id.clone(),
                worker_instance_id: data.worker_instance_id.clone(),
                kind: FrameKind::Error {
                    error: ControlError {
                        code: ControlCode::RuntimeFault,
                        message: "retained replay could not be framed".to_owned(),
                        retryable: false,
                    },
                },
            },
            Vec::new(),
        )
        .expect("error frame");
        write_frame(&mut fake_worker, &frame)
            .await
            .expect("write worker error");
        let (mut public_stream, _public_peer) = tokio::io::duplex(64);

        let failure = run_worker_attach_bridge(
            &mut public_stream,
            "a-typed-error",
            &protocol::SessionId("s-typed-error".to_owned()),
            &CancellationToken::new(),
            &mut data,
            Vec::new(),
        )
        .await
        .expect_err("worker error frame must fail the raw bridge");
        let error = failure
            .protocol_error()
            .expect("typed worker error must survive bridge");
        assert_eq!(error.class, protocol::ErrorClass::Runtime);
        assert_eq!(error.code, "worker_runtime_fault");
        assert_eq!(error.msg, "retained replay could not be framed");
    }

    #[tokio::test]
    async fn worker_attach_bridge_types_frame_read_failure() {
        let (daemon_worker, mut fake_worker) = UnixStream::pair().expect("worker stream pair");
        let mut data = data_stream(daemon_worker);
        fake_worker
            .write_all(&[1])
            .await
            .expect("write partial worker frame");
        fake_worker
            .shutdown()
            .await
            .expect("close partial worker frame");
        let (mut public_stream, _public_peer) = tokio::io::duplex(64);

        let failure = run_worker_attach_bridge(
            &mut public_stream,
            "a-frame-error",
            &protocol::SessionId("s-frame-error".to_owned()),
            &CancellationToken::new(),
            &mut data,
            Vec::new(),
        )
        .await
        .expect_err("partial worker frame must fail the raw bridge");
        let error = failure
            .protocol_error()
            .expect("frame read failure must be typed");
        assert_eq!(error.class, protocol::ErrorClass::Runtime);
        assert_eq!(error.code, "worker_attach_stream_failed");
        assert!(
            error.msg.contains("ended partway"),
            "worker frame cause must remain visible: {error:?}"
        );
    }

    /// Public input that arrives while a worker frame is only partly read must
    /// not discard the consumed bytes: the bridge keeps its pending read and
    /// still delivers that frame intact after forwarding the input.
    #[tokio::test]
    async fn worker_attach_bridge_keeps_a_partly_read_frame_across_public_input() {
        let identity = WorkerFrameIdentity {
            version: Version::new(1).expect("version"),
            stream_id: StreamId::new("a-partial").expect("stream id"),
            worker_instance_id: WorkerInstanceId::new("runtime-partial").expect("runtime id"),
        };
        let frame = |kind, payload: &[u8]| {
            DataFrame::new(
                FrameHeader {
                    version: identity.version,
                    stream_id: identity.stream_id.clone(),
                    worker_instance_id: identity.worker_instance_id.clone(),
                    kind,
                },
                payload.to_vec(),
            )
            .expect("worker frame")
        };
        let mut repaint = Vec::new();
        write_frame(
            &mut repaint,
            &frame(
                FrameKind::TerminalSnapshot {
                    snapshot: TerminalSnapshot {
                        watermark: 0,
                        dimensions: Dimensions::new(80, 24).expect("dimensions"),
                        cursor: Cursor {
                            column: 0,
                            row: 0,
                            visible: true,
                        },
                        alternate_screen: false,
                        title: None,
                        progress: None,
                        visible_lines: Vec::new(),
                    },
                },
                b"repaint-before-input",
            ),
        )
        .await
        .expect("encode repaint frame");
        let mut close = Vec::new();
        write_frame(
            &mut close,
            &frame(
                FrameKind::Close {
                    reason: CloseReason::RuntimeExited,
                },
                &[],
            ),
        )
        .await
        .expect("encode close frame");

        let (mut daemon_worker, mut fake_worker) = tokio::io::duplex(ONE_BYTE_BUFFER);
        let (mut public_stream, mut public_peer) = tokio::io::duplex(PUBLIC_BUFFER);
        let bridge = tokio::spawn(async move {
            bridge_worker_frames(
                &mut public_stream,
                &mut daemon_worker,
                &identity,
                "a-partial",
                &protocol::SessionId("s-partial".to_owned()),
                &CancellationToken::new(),
                Vec::new(),
            )
            .await
        });

        fake_worker
            .write_all(&repaint[..PARTIAL_FRAME_BYTES])
            .await
            .expect("start the worker frame");
        public_peer
            .write_all(b"typed-mid-frame")
            .await
            .expect("send public input");
        let input = read_frame(&mut fake_worker)
            .await
            .expect("read forwarded input")
            .expect("bridge forwards public input");
        assert!(matches!(input.header().kind, FrameKind::Input { .. }));
        assert_eq!(input.payload(), b"typed-mid-frame");
        let remainder = fake_worker.write_all(&repaint[PARTIAL_FRAME_BYTES..]).await;
        let closed = fake_worker.write_all(&close).await;

        bridge
            .await
            .expect("bridge task")
            .expect("a partly read worker frame must survive public input");
        remainder.expect("finish the worker frame");
        closed.expect("close the worker stream");
        let mut delivered = Vec::new();
        public_peer
            .read_to_end(&mut delivered)
            .await
            .expect("read public output");
        assert_eq!(delivered, b"repaint-before-input");
    }

    #[tokio::test]
    async fn worker_attach_bridge_rejects_historical_replay() {
        let (daemon_worker, mut fake_worker) = UnixStream::pair().expect("worker stream pair");
        let mut data = data_stream(daemon_worker);
        let frame = DataFrame::new(
            FrameHeader {
                version: data.version,
                stream_id: data.stream_id.clone(),
                worker_instance_id: data.worker_instance_id.clone(),
                kind: FrameKind::Replay { offset: 0 },
            },
            b"historical bytes".to_vec(),
        )
        .expect("replay frame");
        write_frame(&mut fake_worker, &frame)
            .await
            .expect("write replay frame");
        let (mut public_stream, _public_peer) = tokio::io::duplex(64);

        let failure = run_worker_attach_bridge(
            &mut public_stream,
            "a-replay",
            &protocol::SessionId("s-replay".to_owned()),
            &CancellationToken::new(),
            &mut data,
            Vec::new(),
        )
        .await
        .expect_err("fresh attach must reject retained history replay");
        let error = failure
            .protocol_error()
            .expect("replay contract failure must be typed");
        assert_eq!(error.code, "worker_attach_stream_failed");
        assert!(error.msg.contains("historical replay"));
    }
}
