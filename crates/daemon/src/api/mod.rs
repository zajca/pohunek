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

use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use futures::{SinkExt, StreamExt};
use pohunek_worker_protocol::{
    read_frame, write_frame, ControlCode, ControlError, DataFrame, FrameHeader, FrameKind, WriteId,
};
use protocol::{ErrorClass, Event, ProtocolError, Response, MAX_CONTROL_LINE_BYTES};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};
use tracing::{error, info, warn};

use netbird::validate_netbird_bind_addr;

use crate::error::DaemonError;
use crate::session::{RedeemedAttach, RedeemedRuntime, SessionRegistry};

use handler::Dispatch;
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
    state: DaemonState,
}

impl ControlServer {
    /// Bind the control socket at `socket_path`.
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
    pub async fn bind(socket_path: &Path, health: HealthInfo) -> Result<Self, DaemonError> {
        Self::bind_with_state(
            socket_path,
            DaemonState::new(health, SessionRegistry::default()),
        )
        .await
    }

    /// Bind the control socket with explicit shared daemon state.
    pub async fn bind_with_state(
        socket_path: &Path,
        state: DaemonState,
    ) -> Result<Self, DaemonError> {
        let dir = socket_path.parent().ok_or_else(|| DaemonError::Socket {
            path: socket_path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket path has no parent directory",
            ),
        })?;

        ensure_dir_mode(dir, DIR_MODE)?;
        recover_stale_socket(socket_path).await?;

        let listener = UnixListener::bind(socket_path).map_err(|source| DaemonError::Socket {
            path: socket_path.to_path_buf(),
            source,
        })?;

        set_mode(socket_path, SOCKET_MODE)?;

        info!(socket = %socket_path.display(), "control socket bound");
        Ok(Self {
            listener,
            socket_path: socket_path.to_path_buf(),
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
        if let Err(err) = std::fs::remove_file(&self.socket_path) {
            if err.kind() != io::ErrorKind::NotFound {
                warn!(error = %err, socket = %self.socket_path.display(), "failed to remove socket on shutdown");
            }
        }
    }
}

/// The bound remote (`NetBird` TCP) control server, ready to accept connections.
///
/// Identical protocol and attach semantics to [`ControlServer`]; only the
/// transport differs. Binding is gated by [`validate_netbird_bind_addr`] so the
/// daemon never exposes the control port on a non-NetBird interface (it fails
/// closed — see [`RemoteServer::bind`]).
#[derive(Debug)]
pub struct RemoteServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    state: DaemonState,
}

impl RemoteServer {
    /// Bind a TCP control listener at `addr`.
    ///
    /// FAILS CLOSED: `addr.ip()` is validated as a `NetBird` address
    /// ([`validate_netbird_bind_addr`]) **before** the socket is opened, so an
    /// invalid or non-NetBird address never reaches the OS bind. This is the
    /// authoritative gate that keeps the control port off public, RFC1918, and
    /// loopback interfaces.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::NetbirdBind`] when the address is not a valid
    /// `NetBird` bind address, or [`DaemonError::Socket`] on an OS bind failure.
    pub async fn bind(addr: SocketAddr, state: DaemonState) -> Result<Self, DaemonError> {
        if let Err(err) = validate_netbird_bind_addr(addr.ip()) {
            return Err(DaemonError::NetbirdBind {
                addr: addr.ip(),
                reason: err.to_string(),
            });
        }

        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| DaemonError::Socket {
                path: PathBuf::from(addr.to_string()),
                source,
            })?;
        let local_addr = listener
            .local_addr()
            .map_err(|source| DaemonError::Socket {
                path: PathBuf::from(addr.to_string()),
                source,
            })?;

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

    while let Some(line) = framed.next().await {
        let line = match line {
            Ok(line) => line,
            Err(LinesCodecError::MaxLineLengthExceeded) => {
                warn!("control line exceeded max length; closing connection");
                break;
            }
            Err(LinesCodecError::Io(err)) => return Err(err),
        };

        match handler::dispatch_line(&line, &state).await {
            Dispatch::Reply(response_line) => {
                framed.send(response_line).await.map_err(codec_to_io)?;
            }
            Dispatch::Subscribe(ack_line) => {
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
                run_event_subscription(&mut framed, &mut session_events, &mut notification_events)
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
            let response = Response::err(stream_id, err);
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
    let version = data.version;
    let stream_id = data.stream_id.clone();
    let runtime_id = data.runtime_id.clone();
    let (mut worker_read, mut worker_write) = tokio::io::split(&mut data.stream);
    let mut write_sequence = 1_u64;
    if !initial_input.is_empty() {
        send_worker_attach_input(
            &mut worker_write,
            version,
            &stream_id,
            &runtime_id,
            write_sequence,
            initial_input,
        )
        .await
        .map_err(AttachBridgeError::worker_stream)?;
        write_sequence = write_sequence.saturating_add(1);
    }
    let mut input = [0_u8; 8 * 1024];

    loop {
        tokio::select! {
            frame = read_frame(&mut worker_read) => {
                let Some(frame) = frame.map_err(AttachBridgeError::worker_stream)? else {
                    break;
                };
                let (header, payload) = frame.into_parts();
                if header.stream_id != stream_id || header.runtime_id != runtime_id {
                    return Err(AttachBridgeError::worker_message(
                        "worker attach frame identity mismatch",
                    ));
                }
                match header.kind {
                    FrameKind::Replay { .. }
                    | FrameKind::Output { .. }
                    | FrameKind::TerminalSnapshot { .. } => {
                        stream
                            .write_all(&payload)
                            .await
                            .map_err(AttachBridgeError::client_io)?;
                    }
                    FrameKind::Gap { .. } | FrameKind::InputAck { .. } => {}
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
                    FrameKind::Open { .. } | FrameKind::Input { .. } => {
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
                    version,
                    &stream_id,
                    &runtime_id,
                    write_sequence,
                    input[..count].to_vec(),
                )
                .await
                .map_err(AttachBridgeError::worker_stream)?;
                write_sequence = write_sequence.saturating_add(1);
            }
            () = cancel.cancelled() => break,
        }
    }
    Ok(())
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
    version: pohunek_worker_protocol::Version,
    stream_id: &pohunek_worker_protocol::StreamId,
    runtime_id: &pohunek_worker_protocol::RuntimeId,
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
    let write_id = WriteId::new(format!("attach-{stream_id}-{sequence}"))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let frame = DataFrame::new(
        FrameHeader {
            version,
            stream_id: stream_id.clone(),
            runtime_id: runtime_id.clone(),
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
                    send_event_line(framed, &event).await?;
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "event subscriber lagged; some events were dropped");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            evt = notification_events.recv() => match evt {
                Ok(event) => {
                    send_event_line(framed, &event).await?;
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
async fn recover_stale_socket(path: &Path) -> Result<(), DaemonError> {
    if !path.exists() {
        return Ok(());
    }
    if UnixStream::connect(path).await.is_ok() {
        // Something is alive on the socket. This is unexpected after the
        // single-instance lock; fail clearly rather than clobbering it.
        Err(DaemonError::Socket {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::AddrInUse,
                "a live daemon is already listening on this socket",
            ),
        })
    } else {
        warn!(socket = %path.display(), "removing stale socket from a previous run");
        std::fs::remove_file(path).map_err(|source| DaemonError::Socket {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Create `dir` (and parents) if missing and enforce `mode` on it.
fn ensure_dir_mode(dir: &Path, mode: u32) -> Result<(), DaemonError> {
    std::fs::create_dir_all(dir).map_err(|source| DaemonError::Directory {
        path: dir.to_path_buf(),
        source,
    })?;
    set_mode(dir, mode).map_err(|e| match e {
        DaemonError::Socket { source, .. } => DaemonError::Directory {
            path: dir.to_path_buf(),
            source,
        },
        other => other,
    })
}

/// Set a path's permission bits.
fn set_mode(path: &Path, mode: u32) -> Result<(), DaemonError> {
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms).map_err(|source| DaemonError::Socket {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use pohunek_worker_protocol::{
        ControlCode, ControlError, DataFrame, FrameHeader, FrameKind, RuntimeId, StreamId, Version,
    };
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use tokio_util::sync::CancellationToken;

    use super::{run_worker_attach_bridge, write_frame};
    use crate::runtime::DataStream;

    fn data_stream(stream: UnixStream) -> DataStream {
        DataStream {
            stream,
            version: Version::new(1).expect("version"),
            stream_id: StreamId::new("a-typed-error").expect("stream id"),
            runtime_id: RuntimeId::new("runtime-typed-error").expect("runtime id"),
        }
    }

    #[tokio::test]
    async fn worker_attach_bridge_preserves_typed_worker_error() {
        let (daemon_worker, mut fake_worker) = UnixStream::pair().expect("worker stream pair");
        let mut data = data_stream(daemon_worker);
        let frame = DataFrame::new(
            FrameHeader {
                version: data.version,
                stream_id: data.stream_id.clone(),
                runtime_id: data.runtime_id.clone(),
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
}
