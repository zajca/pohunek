//! Framed request/response transport for the public SDK client.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use protocol::{
    AttachHeader, Event, Method, ProtocolError, ProtocolVersion, ProtocolVersionRange, Request,
    Response, SessionId, SessionInputParams, SessionInputResult, SessionOutputParams,
    SessionOutputResult, SessionResizeParams, SessionResizeResult, SessionResumeResult,
    SessionScreenParams, SessionScreenResult, SessionSetMetadataParams, SessionSetMetadataResult,
    SessionWaitParams, SessionWaitResult, ENV_DAEMON_ID, ENV_SESSION_ID, MAX_CONTROL_LINE_BYTES,
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
/// Transport processing budget added after a validated daemon-side deadline.
///
/// One second leaves room for request framing, waiter teardown, scheduling,
/// response serialization, and local mesh latency without racing the daemon's
/// authoritative overall delivery-and-wait deadline.
const DEDICATED_WAIT_TRANSPORT_HEADROOM: Duration = Duration::from_secs(1);

/// A connected SDK client over either the local Unix socket or remote TCP.
#[derive(Debug)]
pub struct Client {
    inner: ClientInner,
    endpoint: Endpoint,
    options: ClientOptions,
    selected_version: Option<ProtocolVersion>,
    origin: Option<RequestOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestOrigin {
    session_id: SessionId,
    daemon_id: String,
}

impl RequestOrigin {
    pub(crate) fn from_environment() -> Result<Option<Self>, ClientError> {
        Self::from_values(
            read_origin_environment(ENV_SESSION_ID)?,
            read_origin_environment(ENV_DAEMON_ID)?,
        )
    }

    pub(crate) fn from_values(
        session_id: Option<String>,
        daemon_id: Option<String>,
    ) -> Result<Option<Self>, ClientError> {
        match (session_id, daemon_id) {
            (None, None) => Ok(None),
            (Some(session_id), Some(daemon_id)) => {
                let origin = Self {
                    session_id: SessionId(session_id),
                    daemon_id,
                };
                origin
                    .apply(
                        Request::new("origin-validation", "daemon.health", Value::Null)
                            .expect("constant validation request is valid"),
                    )
                    .map_err(|_error| ClientError::InvalidOriginEnvironment)?;
                Ok(Some(origin))
            }
            _ => Err(ClientError::IncompleteOriginEnvironment),
        }
    }

    pub(crate) fn apply(&self, request: Request) -> Result<Request, ClientError> {
        request
            .with_origin(Some(self.session_id.clone()), Some(self.daemon_id.clone()))
            .map_err(|_error| ClientError::InvalidOriginEnvironment)
    }
}

fn read_origin_environment(name: &str) -> Result<Option<String>, ClientError> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_value)) => Err(ClientError::InvalidOriginEnvironment),
    }
}

/// A live subscription connection that yields raw event JSON lines.
#[derive(Debug)]
pub struct Subscription {
    inner: SubscriptionInner,
    selected_version: ProtocolVersion,
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

/// Reconnectable destination for dedicated bounded wait connections.
#[derive(Debug, Clone)]
enum Endpoint {
    Local(PathBuf),
    Remote { host: String, addr: SocketAddr },
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
        // Validate the atomic pair before local dialing or remote discovery so
        // a partial marker can never escape on a request or be masked by I/O.
        RequestOrigin::from_environment()?;
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
        let origin = RequestOrigin::from_environment()?;
        let socket_path = socket_path.as_ref();
        let stream = connect_unix(socket_path, options.connect_timeout).await?;

        Ok(Self {
            inner: ClientInner::Local(Conn::new(stream, None, options)),
            endpoint: Endpoint::Local(socket_path.to_path_buf()),
            options,
            selected_version: None,
            origin,
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
        let origin = RequestOrigin::from_environment()?;
        let host = host.into();
        let stream = connect_tcp(&host, addr, options.connect_timeout).await?;

        Ok(Self {
            inner: ClientInner::Remote(Conn::new(stream, Some(host.clone()), options)),
            endpoint: Endpoint::Remote { host, addr },
            options,
            selected_version: None,
            origin,
        })
    }

    /// Send one framed control request and return the daemon's `ok` payload.
    pub async fn request(&mut self, request: &Request) -> Result<Value, ClientError> {
        let request = match &self.origin {
            Some(origin) => origin.apply(request.clone())?,
            None => request.clone(),
        };
        match &mut self.inner {
            ClientInner::Local(conn) => conn.request(&request, &mut self.selected_version).await,
            ClientInner::Remote(conn) => conn.request(&request, &mut self.selected_version).await,
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
        if M::NAME == protocol::method::SESSION_INPUT {
            let params =
                serde_json::from_value::<SessionInputParams>(serde_json::to_value(params)?)?;
            let result = self.session_input(params).await?;
            return Ok(serde_json::from_value(serde_json::to_value(result)?)?);
        }
        self.call_direct::<M>(params).await
    }

    async fn call_direct<M>(&mut self, params: M::Params) -> Result<M::Output, ClientError>
    where
        M: Method,
    {
        let request = Request::new(
            next_request_id(M::NAME),
            M::NAME,
            serde_json::to_value(params)?,
        )?;
        let value = self.request(&request).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Probe the daemon and return the negotiated protocol version it reports.
    pub async fn handshake(&mut self) -> Result<ProtocolVersion, ClientError> {
        let _result = self.call::<protocol::method::DaemonHealth>(()).await?;
        self.selected_version.ok_or_else(|| {
            ClientError::Framing(
                "daemon health response did not select a protocol version".to_owned(),
            )
        })
    }

    /// Returns the version selected by the first valid response on this connection.
    #[must_use]
    pub const fn selected_version(&self) -> Option<ProtocolVersion> {
        self.selected_version
    }

    /// Read one current terminal screen from the connected host.
    pub async fn session_screen(
        &mut self,
        params: SessionScreenParams,
    ) -> Result<SessionScreenResult, ClientError> {
        self.call::<protocol::method::SessionScreen>(params).await
    }

    /// Read bounded retained output, using a dedicated connection when it waits.
    pub async fn session_output(
        &mut self,
        params: SessionOutputParams,
    ) -> Result<SessionOutputResult, ClientError> {
        if let Some(wait_ms) = params.wait_ms() {
            self.call_dedicated::<protocol::method::SessionOutput>(params, wait_ms)
                .await
        } else {
            self.call::<protocol::method::SessionOutput>(params).await
        }
    }

    /// Wait for a session predicate on a dedicated bounded connection.
    pub async fn session_wait(
        &mut self,
        params: SessionWaitParams,
    ) -> Result<SessionWaitResult, ClientError> {
        let timeout_ms = params.timeout_ms();
        self.call_dedicated::<protocol::method::SessionWait>(params, timeout_ms)
            .await
    }

    /// Deliver input with an optional dedicated overall-deadline connection.
    pub async fn session_input(
        &mut self,
        mut params: SessionInputParams,
    ) -> Result<SessionInputResult, ClientError> {
        if let Some(wait) = params.wait.as_mut() {
            wait.until.get_or_insert_with(Vec::new);
            let wait = wait.clone();
            let timeout_ms = validated_input_wait_timeout(&wait)?;
            let result = self
                .call_dedicated::<protocol::method::SessionInput>(params, timeout_ms)
                .await?;
            validate_input_wait_result(&wait, &result)?;
            Ok(result)
        } else {
            self.call_direct::<protocol::method::SessionInput>(params)
                .await
        }
    }

    /// Resume one logical session through the typed lifecycle API.
    pub async fn session_resume(
        &mut self,
        session_id: SessionId,
    ) -> Result<SessionResumeResult, ClientError> {
        self.call::<protocol::method::SessionResume>(session_id)
            .await
    }

    /// Resize one managed terminal through the typed lifecycle API.
    pub async fn session_resize(
        &mut self,
        params: SessionResizeParams,
    ) -> Result<SessionResizeResult, ClientError> {
        self.call::<protocol::method::SessionResize>(params).await
    }

    /// Merge owner-controlled session metadata through the typed lifecycle API.
    pub async fn session_set_metadata(
        &mut self,
        params: SessionSetMetadataParams,
    ) -> Result<SessionSetMetadataResult, ClientError> {
        self.call::<protocol::method::SessionSetMetadata>(params)
            .await
    }

    /// List authenticated and quarantined managed worker runtimes.
    pub async fn session_runtime_inventory(
        &mut self,
    ) -> Result<protocol::RuntimeInventoryResult, ClientError> {
        self.call::<protocol::method::SessionRuntimeInventory>(())
            .await
    }

    async fn call_dedicated<M>(
        &self,
        params: M::Params,
        wire_timeout_ms: u32,
    ) -> Result<M::Output, ClientError>
    where
        M: Method,
    {
        let request_timeout =
            dedicated_request_timeout(self.options.request_timeout, wire_timeout_ms);
        let options = self.options.with_request_timeout(request_timeout);
        let mut client = self.connect_dedicated(options).await?;
        client.call_direct::<M>(params).await
    }

    async fn connect_dedicated(&self, options: ClientOptions) -> Result<Self, ClientError> {
        match &self.endpoint {
            Endpoint::Local(socket_path) => {
                let stream = connect_unix(socket_path, options.connect_timeout).await?;
                Ok(Self {
                    inner: ClientInner::Local(Conn::new(stream, None, options)),
                    endpoint: Endpoint::Local(socket_path.clone()),
                    options,
                    selected_version: None,
                    origin: self.origin.clone(),
                })
            }
            Endpoint::Remote { host, addr } => {
                let stream = connect_tcp(host, *addr, options.connect_timeout).await?;
                Ok(Self {
                    inner: ClientInner::Remote(Conn::new(stream, Some(host.clone()), options)),
                    endpoint: Endpoint::Remote {
                        host: host.clone(),
                        addr: *addr,
                    },
                    options,
                    selected_version: None,
                    origin: self.origin.clone(),
                })
            }
        }
    }

    /// Send a subscription request and return a live raw event-line stream.
    ///
    /// The connection is consumed because a successful subscription turns the
    /// request/response channel into a one-way event stream.
    pub async fn subscribe(self, request: &Request) -> Result<Subscription, ClientError> {
        let Self {
            inner,
            mut selected_version,
            origin,
            ..
        } = self;
        let request = match origin {
            Some(origin) => origin.apply(request.clone())?,
            None => request.clone(),
        };
        match inner {
            ClientInner::Local(mut conn) => {
                let selected_version = conn.subscribe(&request, &mut selected_version).await?;
                Ok(Subscription {
                    inner: SubscriptionInner::Local(conn),
                    selected_version,
                })
            }
            ClientInner::Remote(mut conn) => {
                let selected_version = conn.subscribe(&request, &mut selected_version).await?;
                Ok(Subscription {
                    inner: SubscriptionInner::Remote(conn),
                    selected_version,
                })
            }
        }
    }
}

fn validated_input_wait_timeout(wait: &protocol::SessionInputWait) -> Result<u32, ClientError> {
    match wait.timeout_ms {
        Some(0) => Err(ClientError::Protocol(ProtocolError::observation(
            "session_input_invalid_wait",
            "timeout_ms must be greater than zero",
        ))),
        Some(timeout_ms) if timeout_ms > protocol::MAX_SESSION_WAIT_MS => Err(
            ClientError::Protocol(ProtocolError::session_wait_limit_exceeded()),
        ),
        Some(timeout_ms) => Ok(timeout_ms),
        None => Ok(protocol::MAX_SESSION_WAIT_MS),
    }
}

fn validate_input_wait_result(
    wait: &protocol::SessionInputWait,
    result: &SessionInputResult,
) -> Result<(), ClientError> {
    if !result.accepted {
        return Err(ClientError::InputWaitContract {
            detail: "the daemon reported that delivered input was not accepted",
        });
    }
    let activity = result.activity.ok_or(ClientError::InputWaitContract {
        detail: "the response omitted the post-submission activity",
    })?;
    if result.activity_source.is_none()
        || result.runtime.is_none()
        || result.activity_epoch.as_deref().is_none_or(str::is_empty)
        || result.activity_revision.is_none()
    {
        return Err(ClientError::InputWaitContract {
            detail: "the response omitted epoch- and runtime-scoped activity evidence",
        });
    }
    let until = wait.until.as_deref().unwrap_or_default();
    let target_matches = if until.is_empty() {
        matches!(
            activity,
            protocol::AgentActivity::Idle | protocol::AgentActivity::Blocked
        )
    } else {
        until.contains(&activity)
    };
    if !target_matches {
        return Err(ClientError::InputWaitContract {
            detail: "the response activity did not match the requested wait target",
        });
    }
    Ok(())
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
            SubscriptionInner::Local(conn) => conn.next_event(self.selected_version).await,
            SubscriptionInner::Remote(conn) => conn.next_event(self.selected_version).await,
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

    async fn request(
        &mut self,
        request: &Request,
        selected_version: &mut Option<ProtocolVersion>,
    ) -> Result<Value, ClientError> {
        if let Some(reason) = &self.poisoned {
            return Err(ClientError::Framing(format!(
                "connection is unusable: {reason}"
            )));
        }

        let line = serde_json::to_string(request)?;
        match tokio::time::timeout(
            self.request_timeout,
            self.exchange(request, line, selected_version),
        )
        .await
        {
            Ok(result) => result,
            Err(_elapsed) => {
                self.poisoned = Some(
                    "previous request timed out; pending daemon response may be stale".to_owned(),
                );
                Err(request_timeout_error(
                    self.remote_host.as_deref(),
                    self.request_timeout,
                ))
            }
        }
    }

    async fn subscribe(
        &mut self,
        request: &Request,
        selected_version: &mut Option<ProtocolVersion>,
    ) -> Result<ProtocolVersion, ClientError> {
        if let Some(reason) = &self.poisoned {
            return Err(ClientError::Framing(format!(
                "connection is unusable: {reason}"
            )));
        }

        let line = serde_json::to_string(request)?;
        match tokio::time::timeout(
            self.request_timeout,
            self.exchange(request, line, selected_version),
        )
        .await
        {
            Ok(result) => result.map(|_ok| {
                (*selected_version).expect("response validation always selects a protocol version")
            }),
            Err(_elapsed) => Err(request_timeout_error(
                self.remote_host.as_deref(),
                self.request_timeout,
            )),
        }
    }

    async fn exchange(
        &mut self,
        request: &Request,
        line: String,
        selected_version: &mut Option<ProtocolVersion>,
    ) -> Result<Value, ClientError> {
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

        if response.id() != request.id() {
            let err = response_id_mismatch_error(host, request.id(), response.id());
            self.poisoned = Some(format!(
                "previous response id mismatch; expected '{}', got '{}'",
                request.id(),
                response.id()
            ));
            return Err(err);
        }

        let response_version = response.version();
        match response.into_result() {
            Err(err) if err.code == "version_mismatch" => Err(map_daemon_error(host, err)),
            result => {
                validate_selected_version(
                    request.version_range(),
                    response_version,
                    selected_version,
                )
                .map_err(|error| map_version_validation_error(host, &error))?;
                result.map_err(|err| map_daemon_error(host, err))
            }
        }
    }

    async fn next_line(&mut self) -> Result<Option<String>, ClientError> {
        let host = self.remote_host.as_deref();
        match self.framed.next().await {
            Some(line) => line.map(Some).map_err(|err| map_codec_err_for(host, err)),
            None => Ok(None),
        }
    }

    async fn next_event(
        &mut self,
        selected_version: ProtocolVersion,
    ) -> Result<Option<Event>, ClientError> {
        let Some(line) = self.next_line().await? else {
            return Ok(None);
        };
        let host = self.remote_host.as_deref();
        match serde_json::from_str::<Event>(&line) {
            Ok(event) if event.version() == selected_version => Ok(Some(event)),
            Ok(event) => Err(map_version_validation_error(
                host,
                &ClientError::ProtocolVersionMismatch {
                    expected: exact_version_range(selected_version),
                    received: event.version(),
                },
            )),
            Err(err) => Err(unparseable_reply_error(host, err)),
        }
    }
}

fn validate_selected_version(
    request_range: ProtocolVersionRange,
    received: ProtocolVersion,
    selected_version: &mut Option<ProtocolVersion>,
) -> Result<(), ClientError> {
    let expected = selected_version.map_or(request_range, exact_version_range);
    if !request_range.contains(received)
        || selected_version.is_some_and(|selected| selected != received)
    {
        return Err(ClientError::ProtocolVersionMismatch { expected, received });
    }
    *selected_version = Some(received);
    Ok(())
}

fn exact_version_range(version: ProtocolVersion) -> ProtocolVersionRange {
    ProtocolVersionRange::new(version, version)
        .expect("a protocol version is always a valid exact range")
}

fn map_version_validation_error(remote_host: Option<&str>, error: &ClientError) -> ClientError {
    map_daemon_error(remote_host, error.to_protocol_error())
}

fn dedicated_request_timeout(configured: Duration, wire_timeout_ms: u32) -> Duration {
    Duration::from_millis(u64::from(wire_timeout_ms))
        .checked_add(DEDICATED_WAIT_TRANSPORT_HEADROOM)
        .unwrap_or(Duration::MAX)
        .max(configured)
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
    RequestOrigin::from_environment()?;
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
    RequestOrigin::from_environment()?;
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
    RequestOrigin::from_environment()?;
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

fn request_timeout_error(remote_host: Option<&str>, timeout: Duration) -> ClientError {
    ClientError::RequestTimeout {
        host: remote_host.map(str::to_owned),
        timeout,
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
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio::net::TcpListener;

    #[test]
    fn origin_markers_are_atomic_and_payload_free_on_failure() {
        assert!(RequestOrigin::from_values(None, None)
            .expect("absent pair")
            .is_none());
        let origin =
            RequestOrigin::from_values(Some("s-42".to_owned()), Some("daemon-a".to_owned()))
                .expect("complete pair")
                .expect("origin");
        let request = origin
            .apply(Request::new("r-1", "daemon.health", Value::Null).expect("request"))
            .expect("apply origin");
        assert_eq!(
            request.origin_session_id(),
            Some(&SessionId("s-42".to_owned()))
        );
        assert_eq!(request.origin_daemon_id(), Some("daemon-a"));

        for incomplete in [
            RequestOrigin::from_values(Some("private-session".to_owned()), None),
            RequestOrigin::from_values(None, Some("private-daemon".to_owned())),
        ] {
            let error = incomplete.expect_err("single marker must fail");
            let rendered = error.to_string();
            assert!(!rendered.contains("private-session"));
            assert!(!rendered.contains("private-daemon"));
            assert_eq!(
                error.to_protocol_error().code,
                "incomplete_origin_environment"
            );
        }

        let invalid = RequestOrigin::from_values(
            Some("private\0session".to_owned()),
            Some("private-daemon".to_owned()),
        )
        .expect_err("invalid pair must fail");
        assert!(matches!(invalid, ClientError::InvalidOriginEnvironment));
        assert!(!invalid.to_string().contains("private"));
    }

    #[tokio::test]
    async fn remote_raw_request_carries_complete_origin_pair() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind fixture daemon");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.expect("read request");
            let request: Request = serde_json::from_str(&line).expect("decode request");
            let response = Response::ok(
                request.version_range().maximum(),
                request.id().to_owned(),
                serde_json::json!({"healthy": true}),
            )
            .expect("response");
            let mut encoded = serde_json::to_vec(&response).expect("encode response");
            encoded.push(b'\n');
            stream
                .get_mut()
                .write_all(&encoded)
                .await
                .expect("write response");
            request
        });

        let mut client = Client::connect_tcp_addr("fixture-remote", address)
            .await
            .expect("connect remote");
        client.origin = Some(RequestOrigin {
            session_id: SessionId("s-origin".to_owned()),
            daemon_id: "daemon-origin".to_owned(),
        });
        let request = Request::new("request-1", "daemon.health", Value::Null).expect("request");
        let response = client.request(&request).await.expect("request succeeds");
        assert_eq!(response, serde_json::json!({"healthy": true}));
        let received = server.await.expect("fixture task");
        assert_eq!(
            received.origin_session_id(),
            Some(&SessionId("s-origin".to_owned()))
        );
        assert_eq!(received.origin_daemon_id(), Some("daemon-origin"));
    }

    #[tokio::test]
    async fn lifecycle_fallback_requests_carry_the_inherited_origin() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind lifecycle fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut stream = BufReader::new(stream);
            let mut requests = Vec::new();
            for _ in 0..3 {
                let mut line = String::new();
                stream.read_line(&mut line).await.expect("read request");
                let request: Request = serde_json::from_str(&line).expect("decode request");
                write_test_response(&mut stream, &request, Value::Null).await;
                requests.push(request);
            }
            requests
        });
        let options = ClientOptions::default();
        let stream = TcpStream::connect(address).await.expect("connect remote");
        let mut client = Client {
            inner: ClientInner::Remote(Conn::new(
                stream,
                Some("fixture-remote".to_owned()),
                options,
            )),
            endpoint: Endpoint::Remote {
                host: "fixture-remote".to_owned(),
                addr: address,
            },
            options,
            selected_version: None,
            origin: Some(test_request_origin()),
        };

        for (index, method) in [
            protocol::method::SESSION_REPORT_AGENT,
            protocol::method::SESSION_RELEASE_AGENT,
            protocol::method::SESSION_REPORT_NATIVE_ID,
        ]
        .into_iter()
        .enumerate()
        {
            let request = Request::new(format!("lifecycle-{index}"), method, Value::Null)
                .expect("valid request");
            client.request(&request).await.expect("request succeeds");
        }

        let requests = server.await.expect("fixture task");
        for request in requests {
            assert_test_request_origin(&request);
        }
    }

    #[tokio::test]
    async fn dedicated_waiting_output_carries_the_connection_origin() {
        let result = serde_json::json!({
            "session_id": "s-target",
            "runtime_id": "runtime-1",
            "runtime_generation": "1",
            "history_start_offset": "0",
            "start_offset": "0",
            "next_offset": "0",
            "runtime_end_offset": "0",
            "data_base64": "",
            "has_more": false,
            "timed_out": true
        });
        let (address, server) = spawn_dedicated_capture_server(result).await;
        let mut client = Client::connect_tcp_addr("fixture-remote", address)
            .await
            .expect("connect remote");
        client.origin = Some(test_request_origin());
        let runtime =
            protocol::SessionRuntimeIdentity::new("runtime-1", protocol::RuntimeGeneration::new(1))
                .expect("valid runtime identity");
        let params = SessionOutputParams::new(
            SessionId("s-target".to_owned()),
            Some(runtime),
            Some(protocol::OutputOffset::new(0)),
            16,
            Some(1),
        )
        .expect("valid waiting output params");

        client
            .session_output(params)
            .await
            .expect("waiting output succeeds");
        let request = server.await.expect("capture server");
        assert_eq!(request.method(), protocol::method::SESSION_OUTPUT);
        assert_test_request_origin(&request);
    }

    #[tokio::test]
    async fn dedicated_session_wait_carries_the_connection_origin() {
        let result = serde_json::json!({
            "reason": "timeout",
            "session": test_session_json(),
            "terminal_watermark": "0",
            "output_offset": "0"
        });
        let (address, server) = spawn_dedicated_capture_server(result).await;
        let mut client = Client::connect_tcp_addr("fixture-remote", address)
            .await
            .expect("connect remote");
        client.origin = Some(test_request_origin());
        let params = SessionWaitParams::new(
            SessionId("s-target".to_owned()),
            None,
            None,
            None,
            None,
            Some(vec![protocol::SessionState::Done]),
            None,
            1,
        )
        .expect("valid wait params");

        client
            .session_wait(params)
            .await
            .expect("session wait succeeds");
        let request = server.await.expect("capture server");
        assert_eq!(request.method(), protocol::method::SESSION_WAIT);
        assert_test_request_origin(&request);
    }

    #[tokio::test]
    async fn dedicated_input_wait_carries_the_connection_origin() {
        let result = serde_json::json!({
            "accepted": true,
            "activity": "idle",
            "activity_source": "report",
            "runtime": {
                "runtime_id": "runtime-1",
                "runtime_generation": "1"
            },
            "activity_epoch": "d-epoch-1",
            "activity_revision": "2"
        });
        let (address, server) = spawn_dedicated_capture_server(result).await;
        let mut client = Client::connect_tcp_addr("fixture-remote", address)
            .await
            .expect("connect remote");
        client.origin = Some(test_request_origin());
        let params = SessionInputParams {
            session_id: SessionId("s-target".to_owned()),
            text: "hello".to_owned(),
            wait: Some(protocol::SessionInputWait {
                until: Some(vec![protocol::AgentActivity::Idle]),
                timeout_ms: None,
            }),
        };

        client
            .session_input(params)
            .await
            .expect("session input wait succeeds");
        let request = server.await.expect("capture server");
        assert_eq!(request.method(), protocol::method::SESSION_INPUT);
        assert_test_request_origin(&request);
    }

    #[tokio::test]
    async fn input_wait_normalizes_absent_targets_before_wire() {
        let result = serde_json::json!({
            "accepted": true,
            "activity": "idle",
            "activity_source": "report",
            "runtime": {
                "runtime_id": "runtime-1",
                "runtime_generation": "1"
            },
            "activity_epoch": "d-epoch-1",
            "activity_revision": "2"
        });
        let (address, server) = spawn_dedicated_capture_server(result).await;
        let mut client = Client::connect_tcp_addr("fixture-remote", address)
            .await
            .expect("connect remote");

        client
            .session_input(SessionInputParams {
                session_id: SessionId("s-target".to_owned()),
                text: "hello".to_owned(),
                wait: Some(protocol::SessionInputWait {
                    until: None,
                    timeout_ms: None,
                }),
            })
            .await
            .expect("session input wait succeeds");

        let request = server.await.expect("capture server");
        assert_eq!(request.params()["wait"]["until"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn input_wait_rejects_legacy_success_without_runtime_evidence() {
        let result = serde_json::json!({"accepted": true});
        let (address, server) = spawn_dedicated_capture_server(result).await;
        let mut client = Client::connect_tcp_addr("fixture-remote", address)
            .await
            .expect("connect remote");
        let params = SessionInputParams {
            session_id: SessionId("s-target".to_owned()),
            text: "hello".to_owned(),
            wait: Some(protocol::SessionInputWait {
                until: Some(vec![protocol::AgentActivity::Idle]),
                timeout_ms: None,
            }),
        };

        let error = client
            .session_input(params)
            .await
            .expect_err("legacy success must fail closed");
        assert!(matches!(error, ClientError::InputWaitContract { .. }));
        assert_eq!(
            error.to_protocol_error().code,
            "session_input_wait_contract_mismatch"
        );
        let recovery = error
            .to_protocol_error()
            .recover
            .expect("contract mismatch recovery");
        assert!(recovery.contains("outcome is unknown"));
        assert!(recovery.contains("do not retry blindly"));
        let request = server.await.expect("capture server");
        assert_eq!(request.method(), protocol::method::SESSION_INPUT);
    }

    #[tokio::test]
    async fn generic_typed_call_routes_input_wait_through_contract_validation() {
        let (address, server) =
            spawn_dedicated_capture_server(serde_json::json!({"accepted": true})).await;
        let mut client = Client::connect_tcp_addr("fixture-remote", address)
            .await
            .expect("connect remote");

        let error = client
            .call::<protocol::method::SessionInput>(SessionInputParams {
                session_id: SessionId("s-target".to_owned()),
                text: "hello".to_owned(),
                wait: Some(protocol::SessionInputWait {
                    until: None,
                    timeout_ms: Some(100),
                }),
            })
            .await
            .expect_err("generic typed wait must reject a legacy success");

        assert_eq!(
            error.to_protocol_error().code,
            "session_input_wait_contract_mismatch"
        );
        let request = server.await.expect("capture server");
        assert_eq!(request.method(), protocol::method::SESSION_INPUT);
        assert_eq!(request.params()["wait"]["until"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn input_wait_rejects_invalid_timeout_before_dedicated_transport() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind invalid-wait fixture");
        let address = listener.local_addr().expect("fixture address");
        let mut client = Client::connect_tcp_addr("fixture-remote", address)
            .await
            .expect("connect shared client");
        let (_shared_stream, _) = listener.accept().await.expect("accept shared client");

        for (timeout_ms, expected_code) in [
            (0, "session_input_invalid_wait"),
            (
                protocol::MAX_SESSION_WAIT_MS + 1,
                "session_wait_limit_exceeded",
            ),
        ] {
            let error = client
                .session_input(SessionInputParams {
                    session_id: SessionId("s-target".to_owned()),
                    text: "hello".to_owned(),
                    wait: Some(protocol::SessionInputWait {
                        until: Some(vec![protocol::AgentActivity::Idle]),
                        timeout_ms: Some(timeout_ms),
                    }),
                })
                .await
                .expect_err("invalid wait timeout must fail locally");
            assert_eq!(error.to_protocol_error().code, expected_code);
            assert!(
                tokio::time::timeout(Duration::from_millis(25), listener.accept())
                    .await
                    .is_err(),
                "invalid wait opened a dedicated connection"
            );
        }
    }

    #[test]
    fn input_wait_timeout_preserves_unknown_delivery_recovery() {
        let error = ClientError::Protocol(ProtocolError::session_input_timeout());
        let recovery = error
            .to_protocol_error()
            .recover
            .expect("timeout recovery hint");

        assert!(recovery.contains("inspect the current session"));
        assert!(recovery.contains("do not retry blindly"));
    }

    #[tokio::test]
    async fn input_wait_rejects_success_without_activity_epoch() {
        let result = serde_json::json!({
            "accepted": true,
            "activity": "idle",
            "activity_source": "report",
            "runtime": {
                "runtime_id": "runtime-1",
                "runtime_generation": "1"
            },
            "activity_revision": "2"
        });
        let (address, server) = spawn_dedicated_capture_server(result).await;
        let mut client = Client::connect_tcp_addr("fixture-remote", address)
            .await
            .expect("connect remote");
        let params = SessionInputParams {
            session_id: SessionId("s-target".to_owned()),
            text: "hello".to_owned(),
            wait: Some(protocol::SessionInputWait {
                until: Some(vec![protocol::AgentActivity::Idle]),
                timeout_ms: None,
            }),
        };

        let error = client
            .session_input(params)
            .await
            .expect_err("unscoped revision must fail closed");
        assert!(matches!(error, ClientError::InputWaitContract { .. }));
        assert_eq!(
            error.to_protocol_error().code,
            "session_input_wait_contract_mismatch"
        );
        let request = server.await.expect("capture server");
        assert_eq!(request.method(), protocol::method::SESSION_INPUT);
    }

    #[tokio::test]
    async fn subscription_request_carries_the_connection_origin() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind subscription fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.expect("read request");
            let request: Request = serde_json::from_str(&line).expect("decode request");
            write_test_response(
                &mut stream,
                &request,
                serde_json::json!({"subscribed": true}),
            )
            .await;
            request
        });
        let mut client = Client::connect_tcp_addr("fixture-remote", address)
            .await
            .expect("connect remote");
        client.origin = Some(test_request_origin());
        let request = Request::new("subscribe-origin", protocol::method::SUBSCRIBE, Value::Null)
            .expect("valid subscribe request");

        let _subscription = client
            .subscribe(&request)
            .await
            .expect("subscribe succeeds");
        let received = server.await.expect("subscription fixture");
        assert_test_request_origin(&received);
    }

    async fn spawn_dedicated_capture_server(
        result: Value,
    ) -> (SocketAddr, tokio::task::JoinHandle<Request>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind dedicated fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (_idle_stream, _) = listener.accept().await.expect("accept shared client");
            let (stream, _) = listener.accept().await.expect("accept dedicated client");
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.expect("read request");
            let request: Request = serde_json::from_str(&line).expect("decode request");
            write_test_response(&mut stream, &request, result).await;
            request
        });
        (address, server)
    }

    async fn write_test_response<S>(stream: &mut BufReader<S>, request: &Request, result: Value)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let response = Response::ok(
            request.version_range().maximum(),
            request.id().to_owned(),
            result,
        )
        .expect("response");
        let mut encoded = serde_json::to_vec(&response).expect("encode response");
        encoded.push(b'\n');
        stream
            .get_mut()
            .write_all(&encoded)
            .await
            .expect("write response");
    }

    fn test_request_origin() -> RequestOrigin {
        RequestOrigin {
            session_id: SessionId("s-origin".to_owned()),
            daemon_id: "daemon-origin".to_owned(),
        }
    }

    fn assert_test_request_origin(request: &Request) {
        assert_eq!(
            request.origin_session_id(),
            Some(&SessionId("s-origin".to_owned()))
        );
        assert_eq!(request.origin_daemon_id(), Some("daemon-origin"));
    }

    fn test_session_json() -> Value {
        serde_json::json!({
            "id": "s-target",
            "agent": "codex",
            "agent_base": "codex",
            "capabilities": {"fork": false, "resume": true},
            "cwd": "/workspace/pohunek",
            "pid": 42424,
            "state": "running",
            "state_source": "process",
            "cols": 80,
            "rows": 24,
            "created_at": "2026-07-08T00:00:00Z",
            "updated_at": "2026-07-08T00:01:00Z"
        })
    }

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

    #[test]
    fn dedicated_timeout_covers_overall_deadline_and_transport_headroom() {
        assert_eq!(
            dedicated_request_timeout(Duration::from_secs(5), 8_000),
            Duration::from_secs(9)
        );
        assert_eq!(
            dedicated_request_timeout(Duration::from_secs(5), 5_000),
            Duration::from_secs(6)
        );
        assert_eq!(
            dedicated_request_timeout(Duration::from_secs(12), 5_000),
            Duration::from_secs(12)
        );
    }

    #[test]
    fn a_connection_rejects_selected_version_changes() {
        let selected = protocol::PROTOCOL_VERSION;
        let mut state = None;
        validate_selected_version(protocol::SUPPORTED_PROTOCOL_VERSIONS, selected, &mut state)
            .expect("first response selects the version");
        let changed = ProtocolVersion::new(selected.get() + 1).expect("nonzero changed version");
        assert!(matches!(
            validate_selected_version(
                ProtocolVersionRange::new(selected, changed).expect("valid test range"),
                changed,
                &mut state,
            ),
            Err(ClientError::ProtocolVersionMismatch { .. })
        ));
    }
}
