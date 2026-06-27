//! Framed request/response transport for the public SDK client.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{ProtocolError, Request, Response};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

use crate::ClientError;

const LOCAL_HOST: &str = "local";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LINE_BYTES: usize = 1024 * 1024;

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
pub enum RawStream {
    /// Local Unix-socket transport.
    Local(UnixStream),
    /// Remote `NetBird` TCP transport.
    Remote(TcpStream),
}

/// Client transport settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientOptions {
    /// Maximum time to wait for one daemon response.
    pub request_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
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
            let addr = resolve_remote_addr(host.to_owned()).await?;
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
        let stream = connect_unix(socket_path).await?;

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
        let stream = connect_tcp(&host, addr).await?;

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

impl Subscription {
    /// Return the next raw event JSON line, or `None` when the daemon closes.
    pub async fn next_line(&mut self) -> Result<Option<String>, ClientError> {
        match &mut self.inner {
            SubscriptionInner::Local(conn) => conn.next_line().await,
            SubscriptionInner::Remote(conn) => conn.next_line().await,
        }
    }
}

impl<S> Conn<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn new(stream: S, remote_host: Option<String>, options: ClientOptions) -> Self {
        Self {
            framed: Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE_BYTES)),
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
            Err(_elapsed) => {
                self.poisoned = Some(
                    "previous subscription timed out; pending daemon response may be stale"
                        .to_owned(),
                );
                Err(no_response_error(
                    self.remote_host.as_deref(),
                    "timed out waiting for subscription ack",
                ))
            }
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
            return Err(response_id_mismatch_error(host, request_id, response.id()));
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
}

/// Open a raw, unframed control connection, selecting local or remote transport
/// from `host`.
pub async fn connect_raw(
    host: &str,
    socket_path: impl AsRef<Path>,
) -> Result<RawStream, ClientError> {
    if is_local_host(host) {
        connect_raw_local(socket_path).await
    } else {
        let addr = resolve_remote_addr(host.to_owned()).await?;
        connect_raw_tcp_addr(host, addr).await
    }
}

/// Open a raw, unframed connection to the local daemon Unix socket.
pub async fn connect_raw_local(socket_path: impl AsRef<Path>) -> Result<RawStream, ClientError> {
    Ok(RawStream::Local(connect_unix(socket_path.as_ref()).await?))
}

/// Open a raw, unframed TCP connection to a daemon on `addr`.
pub async fn connect_raw_tcp_addr(
    host: impl Into<String>,
    addr: SocketAddr,
) -> Result<RawStream, ClientError> {
    let host = host.into();
    Ok(RawStream::Remote(connect_tcp(&host, addr).await?))
}

async fn connect_unix(socket_path: &Path) -> Result<UnixStream, ClientError> {
    UnixStream::connect(socket_path)
        .await
        .map_err(|source| ClientError::DaemonUnreachable {
            socket: socket_path.to_path_buf(),
            source,
        })
}

async fn connect_tcp(host: &str, addr: SocketAddr) -> Result<TcpStream, ClientError> {
    TcpStream::connect(addr)
        .await
        .map_err(|source| ClientError::HostUnreachable {
            host: host.to_owned(),
            source,
        })
}

fn is_local_host(host: &str) -> bool {
    host.is_empty() || host == LOCAL_HOST
}

async fn resolve_remote_addr(host: String) -> Result<SocketAddr, ClientError> {
    tokio::task::spawn_blocking(move || {
        let status = netbird::run_status()?;
        let ip = netbird::resolve_host(&status, &host)?;
        let port = netbird::remote_port()?;
        Ok(SocketAddr::new(ip, port))
    })
    .await
    .map_err(|err| ClientError::RemoteDiscoveryFailed {
        detail: err.to_string(),
    })?
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
