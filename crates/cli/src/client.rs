//! Transport-agnostic control-protocol client.
//!
//! Connects to a daemon and performs request/response exchanges using
//! newline-delimited JSON (the shared `protocol` crate). The same exchange logic
//! serves two transports:
//! - the local Unix socket (`host == "local"`), and
//! - a remote TCP connection over NetBird (any other host), resolved from
//!   `netbird status --json` (see [`crate::target`] for the host grammar).
//!
//! The request/response framing is identical across transports; only the dial
//! step differs. Remote dialing fails closed: a missing `netbird` CLI, an
//! unreadable NetBird state, an unknown host, or a refused connection each map to
//! a distinct typed [`CliError`] so a script can branch on the cause.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{ProtocolError, Request, Response};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

use crate::error::CliError;
use crate::paths::Paths;
use crate::target::LOCAL_HOST;

/// Default per-request timeout. Bounds the CLI when the daemon is wedged.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Mirrors the daemon's max control-line length.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// A framed control connection over an arbitrary byte stream.
///
/// Generic over the underlying transport so the request/response exchange logic
/// is written exactly once and reused for both Unix and TCP. Carried inside the
/// crate-visible [`Client`] enum, so it shares that visibility.
#[derive(Debug)]
pub(crate) struct Conn<S> {
    framed: Framed<S, LinesCodec>,
    /// The host name when this is a remote NetBird TCP transport; `None` for the
    /// local Unix socket. Used solely to attach host context to remote-side
    /// failures — a connection that closes or times out after a successful dial,
    /// or a daemon-returned error — so they name the peer and (for the no-reply
    /// case) carry the daemon-class `remote_daemon_unavailable` code rather than
    /// a generic, host-less framing error (DoD #7: name the host, separate the
    /// failure layer).
    remote_host: Option<String>,
}

impl<S> Conn<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Wrap a freshly connected stream in the line codec.
    ///
    /// `remote_host` is `Some(host)` for a remote TCP connection and `None` for
    /// the local socket; it only drives host-named error mapping.
    fn new(stream: S, remote_host: Option<String>) -> Self {
        let codec = LinesCodec::new_with_max_length(MAX_LINE_BYTES);
        Self {
            framed: Framed::new(stream, codec),
            remote_host,
        }
    }

    /// Send a request and await its response, applying the request timeout.
    async fn request(&mut self, request: &Request) -> Result<Value, CliError> {
        let line = serde_json::to_string(request)?;
        match tokio::time::timeout(REQUEST_TIMEOUT, self.exchange(line)).await {
            Ok(result) => result,
            // A timeout after a successful connect is a remote-*daemon* failure
            // (the transport is up), so name the host where we have one.
            Err(_elapsed) => Err(no_response_error(
                self.remote_host.as_deref(),
                "timed out waiting for daemon response",
            )),
        }
    }

    /// Perform one line send + one line receive.
    async fn exchange(&mut self, line: String) -> Result<Value, CliError> {
        let host = self.remote_host.as_deref();

        self.framed
            .send(line)
            .await
            .map_err(|err| map_codec_err_for(host, err))?;

        let reply = match self.framed.next().await {
            Some(reply) => reply.map_err(|err| map_codec_err_for(host, err))?,
            // Connected, but the daemon closed the stream before replying.
            None => {
                return Err(no_response_error(
                    host,
                    "daemon closed the connection without a response",
                ))
            }
        };

        let response: Response = match serde_json::from_str(&reply) {
            Ok(response) => response,
            Err(err) => return Err(unparseable_reply_error(host, err)),
        };
        match response {
            Response::Ok { ok, .. } => Ok(ok),
            Response::Err { err, .. } => Err(map_daemon_error(host, err)),
        }
    }
}

/// The error for "no usable response from the daemon" — the connection closed
/// without a reply, or the request timed out after a successful connect.
///
/// Over a remote transport this is a host-named, daemon-class
/// [`CliError::RemoteDaemonUnavailable`] (TCP succeeded, the daemon layer did
/// not). Locally it stays a [`CliError::Framing`] error carrying `local_msg`, so
/// the existing local diagnostics are unchanged.
fn no_response_error(remote_host: Option<&str>, local_msg: &str) -> CliError {
    match remote_host {
        Some(host) => CliError::RemoteDaemonUnavailable {
            host: host.to_owned(),
        },
        None => CliError::Framing(local_msg.to_owned()),
    }
}

/// Map a daemon-returned [`ProtocolError`] to a [`CliError`], adding host context
/// over a remote transport.
///
/// Remotely it becomes [`CliError::RemoteProtocol`] (names the host, preserves
/// the daemon's stable class/code/recover); locally it passes through as
/// [`CliError::Protocol`] unchanged.
fn map_daemon_error(remote_host: Option<&str>, err: ProtocolError) -> CliError {
    match remote_host {
        Some(host) => CliError::RemoteProtocol {
            host: host.to_owned(),
            source: err,
        },
        None => CliError::Protocol(err),
    }
}

/// Map a framing/IO failure that occurs *after* a successful connect.
///
/// Over a remote transport an oversized or malformed framed line, or a mid-
/// stream IO failure, means the peer is not delivering a usable response, so it
/// becomes a host-named [`CliError::RemoteDaemonUnavailable`] (DoD #7: name the
/// host). Locally it keeps the original framing/IO mapping (see [`map_codec_err`]).
fn map_codec_err_for(remote_host: Option<&str>, err: LinesCodecError) -> CliError {
    match remote_host {
        Some(host) => CliError::RemoteDaemonUnavailable {
            host: host.to_owned(),
        },
        None => map_codec_err(err),
    }
}

/// Map a failure to parse a reply line as a control [`Response`].
///
/// Over a remote transport, a peer that answers with something that is not a
/// valid control response is not a compatible zagentmesh daemon — the original
/// finding's "non-zagent service" case — so it becomes a host-named
/// [`CliError::RemoteDaemonUnavailable`] rather than an opaque, host-less JSON
/// error. Locally it stays a [`CliError::Json`]: a real local daemon answering
/// unparseable JSON is a genuine bug, surfaced as-is.
fn unparseable_reply_error(remote_host: Option<&str>, err: serde_json::Error) -> CliError {
    match remote_host {
        Some(host) => CliError::RemoteDaemonUnavailable {
            host: host.to_owned(),
        },
        None => CliError::Json(err),
    }
}

/// A connected control client over either transport.
#[derive(Debug)]
pub(crate) enum Client {
    /// Local Unix-socket transport.
    Local(Conn<UnixStream>),
    /// Remote NetBird TCP transport.
    Remote(Conn<TcpStream>),
}

impl Client {
    /// Connect to the daemon for `host`.
    ///
    /// `host` is the *effective* host string: the reserved [`LOCAL_HOST`] (or an
    /// empty string) dials the local Unix socket; any other name is resolved to a
    /// NetBird IP and dialed over TCP.
    ///
    /// # Errors
    ///
    /// - [`CliError::DaemonUnreachable`] when the local socket cannot be dialed.
    /// - [`CliError::NetbirdCliMissing`] / [`CliError::NetbirdStateUnavailable`] /
    ///   [`CliError::HostUnknown`] when the remote host cannot be resolved.
    /// - [`CliError::HostUnreachable`] when the remote TCP connection fails.
    pub(crate) async fn connect(host: &str, paths: &Paths) -> Result<Self, CliError> {
        if is_local_host(host) {
            let stream = connect_unix(&paths.socket).await?;
            Ok(Client::Local(Conn::new(stream, None)))
        } else {
            let addr = resolve_remote_addr(host)?;
            let stream = connect_tcp(host, addr).await?;
            Ok(Client::Remote(Conn::new(stream, Some(host.to_owned()))))
        }
    }

    /// Send a request and await its response.
    ///
    /// # Errors
    ///
    /// Framing, timeout, or daemon-side protocol errors.
    pub(crate) async fn request(&mut self, request: &Request) -> Result<Value, CliError> {
        match self {
            Client::Local(conn) => conn.request(request).await,
            Client::Remote(conn) => conn.request(request).await,
        }
    }
}

/// A raw, *unframed* control connection used for the attach byte stream.
///
/// The attach protocol opens a second connection whose first line is the attach
/// header and whose subsequent bytes are raw PTY traffic, so it must not be wrapped
/// in the line codec. This enum carries the bare stream over either transport;
/// `attach.rs` splits it generically.
#[derive(Debug)]
pub(crate) enum RawStream {
    /// Local Unix-socket transport.
    Local(UnixStream),
    /// Remote NetBird TCP transport.
    Remote(TcpStream),
}

/// Open a raw (unframed) control connection for `host`.
///
/// Uses the same transport selection and resolution as [`Client::connect`].
///
/// # Errors
///
/// Same as [`Client::connect`].
pub(crate) async fn connect_raw(host: &str, paths: &Paths) -> Result<RawStream, CliError> {
    if is_local_host(host) {
        Ok(RawStream::Local(connect_unix(&paths.socket).await?))
    } else {
        let addr = resolve_remote_addr(host)?;
        Ok(RawStream::Remote(connect_tcp(host, addr).await?))
    }
}

/// Whether `host` denotes the local machine.
///
/// The reserved [`LOCAL_HOST`] name and an empty string (no host supplied) both
/// route to the Unix socket; everything else is remote.
fn is_local_host(host: &str) -> bool {
    host.is_empty() || host == LOCAL_HOST
}

/// Dial the local Unix control socket.
async fn connect_unix(socket_path: &Path) -> Result<UnixStream, CliError> {
    UnixStream::connect(socket_path)
        .await
        .map_err(|source| CliError::DaemonUnreachable {
            socket: socket_path.to_path_buf(),
            source,
        })
}

/// Resolve a remote host name to its NetBird daemon control address.
///
/// Runs `netbird status --json`, resolves the host to a NetBird IP, and pairs it
/// with the configured remote control port. Each NetBird failure is mapped to a
/// distinct typed [`CliError`].
fn resolve_remote_addr(host: &str) -> Result<SocketAddr, CliError> {
    let status = netbird::run_status().map_err(map_netbird_err)?;
    let ip = netbird::resolve_host(&status, host).map_err(map_netbird_err)?;
    let port = netbird::remote_port().map_err(map_netbird_err)?;
    Ok(SocketAddr::new(ip, port))
}

/// Dial a remote daemon over TCP, mapping a connect failure to a host-named
/// transport error.
async fn connect_tcp(host: &str, addr: SocketAddr) -> Result<TcpStream, CliError> {
    TcpStream::connect(addr)
        .await
        .map_err(|source| CliError::HostUnreachable {
            host: host.to_owned(),
            source,
        })
}

/// Map a [`netbird::NetbirdError`] to the matching CLI error.
///
/// `HostUnknown` carries the host name; the discovery-class failures carry no
/// secret material (a missing CLI, or a trimmed state/parse detail).
fn map_netbird_err(err: netbird::NetbirdError) -> CliError {
    match err {
        netbird::NetbirdError::CliMissing => CliError::NetbirdCliMissing,
        netbird::NetbirdError::StateUnavailable(detail) => {
            CliError::NetbirdStateUnavailable { detail }
        }
        netbird::NetbirdError::Parse(detail) => CliError::NetbirdStateUnavailable { detail },
        netbird::NetbirdError::HostUnknown(host) => CliError::HostUnknown { host },
    }
}

fn map_codec_err(err: LinesCodecError) -> CliError {
    match err {
        LinesCodecError::Io(io) => CliError::Io(io),
        LinesCodecError::MaxLineLengthExceeded => {
            CliError::Framing("control line exceeded maximum length".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_host_routing_recognizes_local_and_empty() {
        assert!(is_local_host("local"));
        assert!(is_local_host(""));
        assert!(!is_local_host("host-b"));
        assert!(!is_local_host("100.92.10.20"));
    }

    #[test]
    fn netbird_errors_map_to_distinct_cli_variants() {
        assert!(matches!(
            map_netbird_err(netbird::NetbirdError::CliMissing),
            CliError::NetbirdCliMissing
        ));
        assert!(matches!(
            map_netbird_err(netbird::NetbirdError::StateUnavailable("down".to_owned())),
            CliError::NetbirdStateUnavailable { .. }
        ));
        // A parse failure is surfaced as state-unavailable (the local NetBird
        // state could not be read), not as a generic JSON error.
        assert!(matches!(
            map_netbird_err(netbird::NetbirdError::Parse("bad json".to_owned())),
            CliError::NetbirdStateUnavailable { .. }
        ));
        match map_netbird_err(netbird::NetbirdError::HostUnknown("host-b".to_owned())) {
            CliError::HostUnknown { host } => assert_eq!(host, "host-b"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn no_response_over_remote_is_host_named_daemon_unavailable() {
        // TCP connected but no reply: a remote peer must surface a host-named,
        // daemon-class error (DoD #7), not a generic host-less framing error.
        let err = no_response_error(Some("build-box"), "local fallback");
        match &err {
            CliError::RemoteDaemonUnavailable { host } => assert_eq!(host, "build-box"),
            other => panic!("expected RemoteDaemonUnavailable, got {other:?}"),
        }
        let pe = err.to_protocol_error();
        assert_eq!(pe.class, protocol::ErrorClass::Daemon);
        assert_eq!(pe.code, "remote_daemon_unavailable");
        assert!(pe.msg.contains("build-box"), "names host: {}", pe.msg);
    }

    #[test]
    fn no_response_over_local_stays_a_framing_error() {
        // The local path keeps its exact diagnostic message and class.
        let err = no_response_error(None, "timed out waiting for daemon response");
        match err {
            CliError::Framing(msg) => assert!(msg.contains("timed out")),
            other => panic!("expected Framing, got {other:?}"),
        }
    }

    #[test]
    fn remote_daemon_error_is_wrapped_with_host_but_keeps_stable_code() {
        use protocol::{ProtocolVersion, ProtocolError};

        // A version_mismatch returned by a remote daemon: the human message must
        // name the host while the machine code/class/recover stay canonical so a
        // script still branches on `version_mismatch` (DoD #7).
        let err = map_daemon_error(
            Some("build-box"),
            ProtocolError::version_mismatch(ProtocolVersion(1), ProtocolVersion(2)),
        );
        assert!(matches!(err, CliError::RemoteProtocol { .. }));
        let pe = err.to_protocol_error();
        assert_eq!(pe.class, protocol::ErrorClass::Daemon);
        assert_eq!(pe.code, "version_mismatch");
        assert!(pe.msg.contains("build-box"), "names host: {}", pe.msg);
        assert!(pe.recover.is_some(), "recover hint preserved");
    }

    #[test]
    fn remote_runtime_error_keeps_its_class_when_host_wrapped() {
        // Wrapping must not flatten the failure layer: a remote *runtime* error
        // stays runtime-class (DoD #7 separates transport / daemon / runtime).
        let err = map_daemon_error(
            Some("build-box"),
            protocol::ProtocolError::agent_binary_missing("claude"),
        );
        let pe = err.to_protocol_error();
        assert_eq!(pe.class, protocol::ErrorClass::Runtime);
        assert_eq!(pe.code, "agent_binary_missing");
        assert!(pe.msg.contains("build-box"), "names host: {}", pe.msg);
        assert!(pe.msg.contains("claude"), "keeps the binary name: {}", pe.msg);
    }

    #[test]
    fn local_daemon_error_passes_through_unchanged() {
        let err = map_daemon_error(None, protocol::ProtocolError::bad_request("nope"));
        match err {
            CliError::Protocol(pe) => assert_eq!(pe.code, "bad_request"),
            other => panic!("expected Protocol passthrough, got {other:?}"),
        }
    }

    #[test]
    fn remote_garbled_reply_is_host_named_daemon_unavailable() {
        // A non-zagent service answering with a non-control line must name the
        // host and use the daemon-class code, not leak as a host-less json_error
        // (the original finding's "non-zagent service" case).
        let parse_err = serde_json::from_str::<Response>("definitely not json").unwrap_err();
        let err = unparseable_reply_error(Some("build-box"), parse_err);
        match &err {
            CliError::RemoteDaemonUnavailable { host } => assert_eq!(host, "build-box"),
            other => panic!("expected RemoteDaemonUnavailable, got {other:?}"),
        }
        let pe = err.to_protocol_error();
        assert_eq!(pe.code, "remote_daemon_unavailable");
        assert!(pe.msg.contains("build-box"), "names host: {}", pe.msg);
    }

    #[test]
    fn local_garbled_reply_stays_a_json_error() {
        let parse_err = serde_json::from_str::<Response>("definitely not json").unwrap_err();
        let err = unparseable_reply_error(None, parse_err);
        assert!(matches!(err, CliError::Json(_)));
    }

    #[test]
    fn remote_framing_failure_is_host_named_daemon_unavailable() {
        let err = map_codec_err_for(Some("build-box"), LinesCodecError::MaxLineLengthExceeded);
        match err {
            CliError::RemoteDaemonUnavailable { host } => assert_eq!(host, "build-box"),
            other => panic!("expected RemoteDaemonUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn local_framing_failure_stays_framing_unchanged() {
        let err = map_codec_err_for(None, LinesCodecError::MaxLineLengthExceeded);
        match err {
            CliError::Framing(msg) => assert!(msg.contains("maximum length")),
            other => panic!("expected Framing, got {other:?}"),
        }
    }
}
