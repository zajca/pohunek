//! Framed request/response transport for the public SDK client.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use protocol::{
    AttachHeader, Event, Method, ProtocolError, ProtocolVersion, Request, Response,
    MAX_CONTROL_LINE_BYTES,
};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

use crate::ClientError;

/// Reserved host name that routes to this machine's Unix socket.
pub const LOCAL_HOST: &str = "local";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected SDK client over either the local Unix socket or remote TCP.
#[derive(Debug)]
pub struct Client {
    inner: ClientInner,
}

/// A live subscription connection that yields raw event JSON lines.
#[derive(Debug)]
pub struct Subscription {
    inner: SubscriptionInner,
}

/// A raw, unframed control connection used for attach byte streams.
#[derive(Debug)]
#[non_exhaustive]
pub enum RawStream {
    /// Local Unix-socket transport.
    Local(UnixStream),
    /// Remote `NetBird` TCP transport.
    Remote(TcpStream),
}

/// Client transport settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClientOptions {
    /// Maximum time to wait for one daemon response.
    pub request_timeout: Duration,
    /// Maximum time to wait for connection setup and remote discovery.
    pub connect_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }
}

impl ClientOptions {
    /// Return options with a custom per-request response timeout.
    #[must_use]
    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Return options with a custom connection setup timeout.
    #[must_use]
    pub fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }
}

#[derive(Debug)]
enum ClientInner {
    Local(Conn<UnixStream>),
    Remote(Conn<TcpStream>),
}

#[derive(Debug)]
enum SubscriptionInner {
    Local(Conn<UnixStream>),
    Remote(Conn<TcpStream>),
}

#[derive(Debug)]
struct Conn<S> {
    framed: Framed<S, LinesCodec>,
    remote_host: Option<String>,
    request_timeout: Duration,
    poisoned: Option<String>,
}

impl Client {
    /// Connect to a daemon, selecting local or remote transport from `host`.
    ///
    /// Empty `host` and `"local"` connect to `socket_path`; any other host is
    /// resolved through `NetBird` and dialed over TCP.
    pub async fn connect(host: &str, socket_path: impl AsRef<Path>) -> Result<Self, ClientError> {
        Self::connect_with_options(host, socket_path, ClientOptions::default()).await
    }

    /// Connect to a daemon with explicit transport settings.
    pub async fn connect_with_options(
        host: &str,
        socket_path: impl AsRef<Path>,
        options: ClientOptions,
    ) -> Result<Self, ClientError> {
        if is_local_host(host) {
            Self::connect_local_with_options(socket_path, options).await
        } else {
            let addr = resolve_remote_addr(host.to_owned(), options.connect_timeout).await?;
            Self::connect_tcp_addr_with_options(host, addr, options).await
        }
    }

    /// Connect to a local daemon Unix socket.
    pub async fn connect_local(socket_path: impl AsRef<Path>) -> Result<Self, ClientError> {
        Self::connect_local_with_options(socket_path, ClientOptions::default()).await
    }

    /// Connect to a local daemon Unix socket with explicit transport settings.
    pub async fn connect_local_with_options(
        socket_path: impl AsRef<Path>,
        options: ClientOptions,
    ) -> Result<Self, ClientError> {
        let socket_path = socket_path.as_ref();
        let stream = connect_unix(socket_path, options.connect_timeout).await?;

        Ok(Self {
            inner: ClientInner::Local(Conn::new(stream, None, options)),
        })
    }

    /// Connect to a daemon on `addr`, preserving `host` for remote errors.
    pub async fn connect_tcp_addr(
        host: impl Into<String>,
        addr: SocketAddr,
    ) -> Result<Self, ClientError> {
        Self::connect_tcp_addr_with_options(host, addr, ClientOptions::default()).await
    }

    /// Connect to a daemon on `addr` with explicit transport settings.
    pub async fn connect_tcp_addr_with_options(
        host: impl Into<String>,
        addr: SocketAddr,
        options: ClientOptions,
    ) -> Result<Self, ClientError> {
        let host = host.into();
        let stream = connect_tcp(&host, addr, options.connect_timeout).await?;

        Ok(Self {
            inner: ClientInner::Remote(Conn::new(stream, Some(host), options)),
        })
    }

    /// Send one framed control request and return the daemon's `ok` payload.
    pub async fn request(&mut self, request: &Request) -> Result<Value, ClientError> {
        match &mut self.inner {
            ClientInner::Local(conn) => conn.request(request).await,
            ClientInner::Remote(conn) => conn.request(request).await,
        }
    }

    /// Send one typed control-method request and decode its success payload.
    ///
    /// This is the public SDK path for normal request/response methods. The
    /// lower-level [`Self::request`] remains available for callers that need raw
    /// JSON envelopes or are testing framing behavior directly.
    pub async fn call<M>(&mut self, params: M::Params) -> Result<M::Output, ClientError>
    where
        M: Method,
    {
        let request = Request::new(
            next_request_id(M::NAME),
            M::NAME,
            serde_json::to_value(params)?,
        );
        let value = self.request(&request).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Probe the daemon and return the negotiated protocol version it reports.
    pub async fn handshake(&mut self) -> Result<ProtocolVersion, ClientError> {
        let result = self.call::<protocol::method::DaemonHealth>(()).await?;
        Ok(result.protocol_version)
    }

    /// Send a subscription request and return a live raw event-line stream.
    ///
    /// The connection is consumed because a successful subscription turns the
    /// request/response channel into a one-way event stream.
    pub async fn subscribe(self, request: &Request) -> Result<Subscription, ClientError> {
        match self.inner {
            ClientInner::Local(mut conn) => {
                conn.subscribe(request).await?;
                Ok(Subscription {
                    inner: SubscriptionInner::Local(conn),
                })
            }
            ClientInner::Remote(mut conn) => {
                conn.subscribe(request).await?;
                Ok(Subscription {
                    inner: SubscriptionInner::Remote(conn),
                })
            }
        }
    }
}

/// Build a unique correlation id for one SDK control request.
///
/// Format: `sdk-<method>-<run-token>-<seq>`. The method keeps ids readable in
/// daemon logs; the run token and sequence keep repeated and concurrent calls
/// distinct across one process lifetime.
#[must_use]
pub fn next_request_id(method: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("sdk-{method}-{}-{seq}", run_token())
}

fn run_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        format!("{:x}{:x}", std::process::id(), nanos)
    })
}

impl Subscription {
    /// Return the next raw event JSON line, or `None` when the daemon closes.
    pub async fn next_line(&mut self) -> Result<Option<String>, ClientError> {
        match &mut self.inner {
            SubscriptionInner::Local(conn) => conn.next_line().await,
            SubscriptionInner::Remote(conn) => conn.next_line().await,
        }
    }

    /// Return the next decoded [`Event`], or `None` when the daemon closes.
    ///
    /// This is the typed counterpart to [`Self::next_line`]: it reads one line
    /// and decodes it into a protocol [`Event`]. A malformed line surfaces as a
    /// typed error, mapped by transport exactly like an unparseable reply.
    pub async fn next_event(&mut self) -> Result<Option<Event>, ClientError> {
        match &mut self.inner {
            SubscriptionInner::Local(conn) => conn.next_event().await,
            SubscriptionInner::Remote(conn) => conn.next_event().await,
        }
    }
}

impl<S> Conn<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn new(stream: S, remote_host: Option<String>, options: ClientOptions) -> Self {
        Self {
            framed: Framed::new(
                stream,
                LinesCodec::new_with_max_length(MAX_CONTROL_LINE_BYTES),
            ),
            remote_host,
            request_timeout: options.request_timeout,
            poisoned: None,
        }
    }

    async fn request(&mut self, request: &Request) -> Result<Value, ClientError> {
        if let Some(reason) = &self.poisoned {
            return Err(ClientError::Framing(format!(
                "connection is unusable: {reason}"
            )));
        }

        let line = serde_json::to_string(request)?;
        match tokio::time::timeout(self.request_timeout, self.exchange(&request.id, line)).await {
            Ok(result) => result,
            Err(_elapsed) => {
                self.poisoned = Some(
                    "previous request timed out; pending daemon response may be stale".to_owned(),
                );
                Err(no_response_error(
                    self.remote_host.as_deref(),
                    "timed out waiting for daemon response",
                ))
            }
        }
    }

    async fn subscribe(&mut self, request: &Request) -> Result<(), ClientError> {
        if let Some(reason) = &self.poisoned {
            return Err(ClientError::Framing(format!(
                "connection is unusable: {reason}"
            )));
        }

        let line = serde_json::to_string(request)?;
        match tokio::time::timeout(self.request_timeout, self.exchange(&request.id, line)).await {
            Ok(result) => result.map(|_ok| ()),
            Err(_elapsed) => Err(no_response_error(
                self.remote_host.as_deref(),
                "timed out waiting for subscription ack",
            )),
        }
    }

    async fn exchange(&mut self, request_id: &str, line: String) -> Result<Value, ClientError> {
        let host = self.remote_host.as_deref();

        self.framed
            .send(line)
            .await
            .map_err(|err| map_codec_err_for(host, err))?;

        let reply = match self.framed.next().await {
            Some(reply) => reply.map_err(|err| map_codec_err_for(host, err))?,
            None => {
                return Err(no_response_error(
                    host,
                    "daemon closed the connection without a response",
                ));
            }
        };

        let response: Response = match serde_json::from_str(&reply) {
            Ok(response) => response,
            Err(err) => return Err(unparseable_reply_error(host, err)),
        };

        if response.id() != request_id {
            let err = response_id_mismatch_error(host, request_id, response.id());
            self.poisoned = Some(format!(
                "previous response id mismatch; expected '{request_id}', got '{}'",
                response.id()
            ));
            return Err(err);
        }

        match response {
            Response::Ok { ok, .. } => Ok(ok),
            Response::Err { err, .. } => Err(map_daemon_error(host, err)),
        }
    }

    async fn next_line(&mut self) -> Result<Option<String>, ClientError> {
        let host = self.remote_host.as_deref();
        match self.framed.next().await {
            Some(line) => line.map(Some).map_err(|err| map_codec_err_for(host, err)),
            None => Ok(None),
        }
    }

    async fn next_event(&mut self) -> Result<Option<Event>, ClientError> {
        let Some(line) = self.next_line().await? else {
            return Ok(None);
        };
        let host = self.remote_host.as_deref();
        match serde_json::from_str::<Event>(&line) {
            Ok(event) => Ok(Some(event)),
            Err(err) => Err(unparseable_reply_error(host, err)),
        }
    }
}

/// Open a raw, unframed control connection, selecting local or remote transport
/// from `host`.
pub async fn connect_raw(
    host: &str,
    socket_path: impl AsRef<Path>,
) -> Result<RawStream, ClientError> {
    connect_raw_with_options(host, socket_path, ClientOptions::default()).await
}

/// Open a raw, unframed control connection with explicit transport settings.
pub async fn connect_raw_with_options(
    host: &str,
    socket_path: impl AsRef<Path>,
    options: ClientOptions,
) -> Result<RawStream, ClientError> {
    if is_local_host(host) {
        connect_raw_local_with_options(socket_path, options).await
    } else {
        let addr = resolve_remote_addr(host.to_owned(), options.connect_timeout).await?;
        connect_raw_tcp_addr_with_options(host, addr, options).await
    }
}

/// Open a raw, unframed connection to the local daemon Unix socket.
pub async fn connect_raw_local(socket_path: impl AsRef<Path>) -> Result<RawStream, ClientError> {
    connect_raw_local_with_options(socket_path, ClientOptions::default()).await
}

/// Open a raw, unframed connection to the local daemon Unix socket with explicit
/// transport settings.
pub async fn connect_raw_local_with_options(
    socket_path: impl AsRef<Path>,
    options: ClientOptions,
) -> Result<RawStream, ClientError> {
    Ok(RawStream::Local(
        connect_unix(socket_path.as_ref(), options.connect_timeout).await?,
    ))
}

/// Open a raw, unframed TCP connection to a daemon on `addr`.
pub async fn connect_raw_tcp_addr(
    host: impl Into<String>,
    addr: SocketAddr,
) -> Result<RawStream, ClientError> {
    connect_raw_tcp_addr_with_options(host, addr, ClientOptions::default()).await
}

/// Open a raw, unframed TCP connection to a daemon on `addr` with explicit
/// transport settings.
pub async fn connect_raw_tcp_addr_with_options(
    host: impl Into<String>,
    addr: SocketAddr,
    options: ClientOptions,
) -> Result<RawStream, ClientError> {
    let host = host.into();
    Ok(RawStream::Remote(
        connect_tcp(&host, addr, options.connect_timeout).await?,
    ))
}

/// Open an attach byte stream and write the daemon's attach prelude before
/// returning the raw transport to the caller.
pub async fn attach_raw(
    host: &str,
    socket_path: impl AsRef<Path>,
    stream_id: &str,
) -> Result<RawStream, ClientError> {
    attach_raw_with_options(host, socket_path, stream_id, ClientOptions::default()).await
}

/// Open an attach byte stream with explicit transport settings.
pub async fn attach_raw_with_options(
    host: &str,
    socket_path: impl AsRef<Path>,
    stream_id: &str,
    options: ClientOptions,
) -> Result<RawStream, ClientError> {
    let mut stream = connect_raw_with_options(host, socket_path, options).await?;
    stream.write_attach_header(stream_id).await?;
    Ok(stream)
}

/// Open a local attach byte stream and write the daemon's attach prelude before
/// returning the raw transport to the caller.
pub async fn attach_raw_local(
    socket_path: impl AsRef<Path>,
    stream_id: &str,
) -> Result<RawStream, ClientError> {
    attach_raw_local_with_options(socket_path, stream_id, ClientOptions::default()).await
}

/// Open a local attach byte stream with explicit transport settings.
pub async fn attach_raw_local_with_options(
    socket_path: impl AsRef<Path>,
    stream_id: &str,
    options: ClientOptions,
) -> Result<RawStream, ClientError> {
    let mut stream = connect_raw_local_with_options(socket_path, options).await?;
    stream.write_attach_header(stream_id).await?;
    Ok(stream)
}

/// Open a TCP attach byte stream and write the daemon's attach prelude before
/// returning the raw transport to the caller.
pub async fn attach_raw_tcp_addr(
    host: impl Into<String>,
    addr: SocketAddr,
    stream_id: &str,
) -> Result<RawStream, ClientError> {
    attach_raw_tcp_addr_with_options(host, addr, stream_id, ClientOptions::default()).await
}

/// Open a TCP attach byte stream with explicit transport settings.
pub async fn attach_raw_tcp_addr_with_options(
    host: impl Into<String>,
    addr: SocketAddr,
    stream_id: &str,
    options: ClientOptions,
) -> Result<RawStream, ClientError> {
    let mut stream = connect_raw_tcp_addr_with_options(host, addr, options).await?;
    stream.write_attach_header(stream_id).await?;
    Ok(stream)
}

impl RawStream {
    async fn write_attach_header(&mut self, stream_id: &str) -> Result<(), ClientError> {
        match self {
            RawStream::Local(stream) => write_attach_header(stream, stream_id).await,
            RawStream::Remote(stream) => write_attach_header(stream, stream_id).await,
        }
    }
}

async fn write_attach_header<S>(stream: &mut S, stream_id: &str) -> Result<(), ClientError>
where
    S: AsyncWrite + Unpin,
{
    let mut header = serde_json::to_vec(&AttachHeader {
        attach: stream_id.to_owned(),
    })?;
    header.push(b'\n');
    stream.write_all(&header).await?;
    stream.flush().await?;
    Ok(())
}

async fn connect_unix(
    socket_path: &Path,
    connect_timeout: Duration,
) -> Result<UnixStream, ClientError> {
    match tokio::time::timeout(connect_timeout, UnixStream::connect(socket_path)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(source)) => Err(map_unix_connect_error(socket_path, source)),
        Err(_elapsed) => Err(ClientError::DaemonUnreachable {
            socket: socket_path.to_path_buf(),
            source: timeout_error("daemon socket connect", connect_timeout),
        }),
    }
}

fn map_unix_connect_error(socket_path: &Path, source: io::Error) -> ClientError {
    match source.raw_os_error() {
        Some(libc::EMFILE) => ClientError::ClientFileDescriptorsExhausted {
            socket: socket_path.to_path_buf(),
            source,
        },
        Some(libc::ENFILE) => ClientError::SystemFileDescriptorsExhausted {
            socket: socket_path.to_path_buf(),
            source,
        },
        _ => ClientError::DaemonUnreachable {
            socket: socket_path.to_path_buf(),
            source,
        },
    }
}

async fn connect_tcp(
    host: &str,
    addr: SocketAddr,
    connect_timeout: Duration,
) -> Result<TcpStream, ClientError> {
    match tokio::time::timeout(connect_timeout, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(source)) => Err(ClientError::HostUnreachable {
            host: host.to_owned(),
            source,
        }),
        Err(_elapsed) => Err(ClientError::HostUnreachable {
            host: host.to_owned(),
            source: timeout_error("daemon tcp connect", connect_timeout),
        }),
    }
}

/// Return whether `host` denotes the local machine.
#[must_use]
pub fn is_local_host(host: &str) -> bool {
    host.is_empty() || host == LOCAL_HOST
}

async fn resolve_remote_addr(
    host: String,
    connect_timeout: Duration,
) -> Result<SocketAddr, ClientError> {
    let discovery_host = host.clone();
    let task = tokio::task::spawn_blocking(move || {
        let status = netbird::run_status()?;
        let ip = netbird::resolve_host(&status, &discovery_host)?;
        let port = netbird::remote_port()?;
        Ok(SocketAddr::new(ip, port))
    });

    match tokio::time::timeout(connect_timeout, task).await {
        Ok(result) => result.map_err(|err| ClientError::RemoteDiscoveryFailed {
            detail: err.to_string(),
        })?,
        Err(_elapsed) => Err(ClientError::RemoteDiscoveryFailed {
            detail: format!("timed out resolving remote host '{host}' after {connect_timeout:?}"),
        }),
    }
}

fn timeout_error(action: &str, timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("{action} timed out after {timeout:?}"),
    )
}

fn no_response_error(remote_host: Option<&str>, local_msg: &str) -> ClientError {
    match remote_host {
        Some(host) => ClientError::RemoteDaemonUnavailable {
            host: host.to_owned(),
        },
        None => ClientError::Framing(local_msg.to_owned()),
    }
}

fn map_daemon_error(remote_host: Option<&str>, err: ProtocolError) -> ClientError {
    match remote_host {
        Some(host) => ClientError::RemoteProtocol {
            host: host.to_owned(),
            source: err,
        },
        None => ClientError::Protocol(err),
    }
}

fn map_codec_err_for(remote_host: Option<&str>, err: LinesCodecError) -> ClientError {
    match remote_host {
        Some(host) => ClientError::RemoteDaemonUnavailable {
            host: host.to_owned(),
        },
        None => map_codec_err(err),
    }
}

fn map_codec_err(err: LinesCodecError) -> ClientError {
    match err {
        LinesCodecError::Io(io) => ClientError::Io(io),
        LinesCodecError::MaxLineLengthExceeded => {
            ClientError::Framing("control line exceeded maximum length".to_owned())
        }
    }
}

fn unparseable_reply_error(remote_host: Option<&str>, err: serde_json::Error) -> ClientError {
    match remote_host {
        Some(host) => ClientError::RemoteDaemonUnavailable {
            host: host.to_owned(),
        },
        None => ClientError::Json(err),
    }
}

fn response_id_mismatch_error(
    remote_host: Option<&str>,
    expected: &str,
    actual: &str,
) -> ClientError {
    match remote_host {
        Some(host) => ClientError::RemoteDaemonUnavailable {
            host: host.to_owned(),
        },
        None => ClientError::Framing(format!(
            "response id mismatch: expected '{expected}', got '{actual}'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_connect_emfile_is_client_descriptor_exhaustion() {
        let socket = Path::new("/run/pohunek/daemon.sock");
        let err = map_unix_connect_error(socket, io::Error::from_raw_os_error(libc::EMFILE));

        assert!(matches!(
            err,
            ClientError::ClientFileDescriptorsExhausted {
                socket: path,
                source
            } if path == socket && source.raw_os_error() == Some(libc::EMFILE)
        ));
    }

    #[test]
    fn unix_connect_enfile_is_system_descriptor_exhaustion() {
        let socket = Path::new("/run/pohunek/daemon.sock");
        let err = map_unix_connect_error(socket, io::Error::from_raw_os_error(libc::ENFILE));

        assert!(matches!(
            err,
            ClientError::SystemFileDescriptorsExhausted {
                socket: path,
                source
            } if path == socket && source.raw_os_error() == Some(libc::ENFILE)
        ));
    }

    #[test]
    fn other_unix_connect_errors_remain_daemon_unreachable() {
        let socket = Path::new("/run/pohunek/daemon.sock");
        let err = map_unix_connect_error(socket, io::Error::from_raw_os_error(libc::ENOENT));

        assert!(matches!(
            err,
            ClientError::DaemonUnreachable {
                socket: path,
                source
            } if path == socket && source.raw_os_error() == Some(libc::ENOENT)
        ));
    }
}
