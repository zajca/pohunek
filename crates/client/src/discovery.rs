//! `NetBird` peer discovery and bounded daemon classification.
//!
//! This module is the single protocol-aware discovery implementation used by
//! the CLI and daemon. `NetBird` remains the synchronous, dependency-light source
//! of peer state; this module owns the asynchronous health exchange instead.

use std::net::SocketAddr;
use std::num::{NonZeroU16, NonZeroUsize};
use std::time::Duration;

use futures::{stream, StreamExt as _};
use protocol::{
    method, HostClass, HostRecord, Request, Response, MAX_CONTROL_LINE_BYTES,
    SUPPORTED_PROTOCOL_VERSIONS,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::transport::RequestOrigin;

/// Default lifetime for discovery snapshots.
///
/// Thirty seconds keeps launcher-style repeated calls responsive while ensuring
/// host availability changes become visible promptly.
pub const DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(30);

/// Default bound for one peer's connection and health exchange.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Default maximum number of simultaneous peer probes.
///
/// Sixteen probes bound aggregate mesh pressure while avoiding serial delays on
/// a large peer list.
pub const DEFAULT_PROBE_CONCURRENCY: usize = 16;

/// Default deadline for one complete local discovery operation.
///
/// This bounds `netbird status --json` plus all bounded peer probes. It is
/// deliberately larger than the common six 1.5-second batches of 96 peers.
pub const DEFAULT_DISCOVERY_DEADLINE: Duration = Duration::from_secs(12);

/// Extra time a cache waiter reserves after a valid discovery deadline.
pub const DISCOVERY_LOCK_WAIT_MARGIN: Duration = Duration::from_secs(3);

/// Largest deadline that retains room for [`DISCOVERY_LOCK_WAIT_MARGIN`].
pub const MAX_DISCOVERY_DEADLINE: Duration = Duration::from_secs(u64::MAX - 3);

/// Discovery probing settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryOptions {
    port: NonZeroU16,
    probe_timeout: Duration,
    concurrency: NonZeroUsize,
    deadline: Duration,
}

impl DiscoveryOptions {
    /// Build options for a non-zero remote daemon port.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ClientError::InvalidDiscoveryOptions`] when `port` is
    /// zero.
    pub fn new(port: u16) -> Result<Self, crate::ClientError> {
        if port == 0 {
            return Err(invalid_option("remote daemon port must be non-zero"));
        }
        Ok(Self {
            port: NonZeroU16::new(port).expect("non-zero port was checked"),
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            concurrency: NonZeroUsize::new(DEFAULT_PROBE_CONCURRENCY)
                .expect("default concurrency is non-zero"),
            deadline: DEFAULT_DISCOVERY_DEADLINE,
        })
    }

    /// Return the validated remote daemon port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port.get()
    }

    /// Return the bounded timeout for one peer health exchange.
    #[must_use]
    pub fn probe_timeout(&self) -> Duration {
        self.probe_timeout
    }

    /// Return the validated maximum concurrent probe count.
    #[must_use]
    pub fn concurrency(&self) -> usize {
        self.concurrency.get()
    }

    /// Return the complete discovery deadline.
    #[must_use]
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Return options with a custom probe timeout.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ClientError::InvalidDiscoveryOptions`] when
    /// `probe_timeout` is zero.
    pub fn with_probe_timeout(
        mut self,
        probe_timeout: Duration,
    ) -> Result<Self, crate::ClientError> {
        if probe_timeout.is_zero() {
            return Err(invalid_option("probe timeout must be non-zero"));
        }
        self.probe_timeout = probe_timeout;
        Ok(self)
    }

    /// Return options with a custom concurrency bound.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ClientError::InvalidDiscoveryOptions`] when
    /// `concurrency` is zero.
    pub fn with_concurrency(mut self, concurrency: usize) -> Result<Self, crate::ClientError> {
        if concurrency == 0 {
            return Err(invalid_option("discovery concurrency must be non-zero"));
        }
        self.concurrency =
            NonZeroUsize::new(concurrency).expect("non-zero concurrency was checked");
        Ok(self)
    }

    /// Return options with a bounded complete discovery deadline.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ClientError::InvalidDiscoveryOptions`] when `deadline`
    /// is zero or too large to leave room for the cache lock wait margin.
    pub fn with_deadline(mut self, deadline: Duration) -> Result<Self, crate::ClientError> {
        if deadline.is_zero() || deadline > MAX_DISCOVERY_DEADLINE {
            return Err(invalid_option(
                "discovery deadline must be non-zero and leave room for the lock wait margin",
            ));
        }
        self.deadline = deadline;
        Ok(self)
    }
}

fn invalid_option(detail: &str) -> crate::ClientError {
    crate::ClientError::InvalidDiscoveryOptions {
        detail: detail.to_owned(),
    }
}

/// Discover and classify this machine's `NetBird` peers.
///
/// Only peers advertising an address in the `NetBird` CGNAT range are dialed.
/// Peers with no safe address remain candidates. A transport, framing, or health
/// failure is represented as [`HostClass::Unreachable`] rather than failing the
/// whole enumeration.
///
/// # Errors
///
/// Returns [`crate::ClientError::Netbird`] when local `NetBird` state or its
/// configured port cannot be loaded. Returns
/// [`crate::ClientError::RemoteDiscoveryFailed`] when complete discovery
/// exceeds its deadline.
pub async fn discover_hosts() -> Result<Vec<HostRecord>, crate::ClientError> {
    let options =
        DiscoveryOptions::new(netbird::remote_port().map_err(crate::ClientError::Netbird)?)?;
    discover_hosts_with_options(options).await
}

/// Discover peers using caller-supplied bounded probe settings.
///
/// # Errors
///
/// Returns [`crate::ClientError::Netbird`] when local `NetBird` state cannot
/// be loaded. Returns [`crate::ClientError::RemoteDiscoveryFailed`] when
/// complete discovery exceeds [`DiscoveryOptions::deadline`]. Returns an
/// origin-environment error when exactly one origin marker is present or a
/// marker value is invalid.
pub async fn discover_hosts_with_options(
    options: DiscoveryOptions,
) -> Result<Vec<HostRecord>, crate::ClientError> {
    let origin = RequestOrigin::from_environment()?;
    discover_with_status(
        options,
        async {
            netbird::run_status_async()
                .await
                .map_err(crate::ClientError::Netbird)
        },
        origin,
    )
    .await
}

async fn discover_with_status<F>(
    options: DiscoveryOptions,
    status: F,
    origin: Option<RequestOrigin>,
) -> Result<Vec<HostRecord>, crate::ClientError>
where
    F: std::future::Future<Output = Result<netbird::NetbirdStatus, crate::ClientError>>,
{
    tokio::time::timeout(options.deadline(), async {
        let status = status.await?;
        Ok(discover_status(&status, options, origin.as_ref()).await)
    })
    .await
    .map_err(|_timeout| crate::ClientError::RemoteDiscoveryFailed {
        detail: "discovery exceeded its configured deadline".to_owned(),
    })?
}

/// Classify peers from an already loaded `NetBird` status snapshot.
#[must_use]
async fn discover_status(
    status: &netbird::NetbirdStatus,
    options: DiscoveryOptions,
    origin: Option<&RequestOrigin>,
) -> Vec<HostRecord> {
    stream::iter(status.peers().iter().cloned())
        .map(|peer| async move {
            let name = peer.fqdn.as_deref().map(short_hostname).map(str::to_owned);
            let fqdn = peer.fqdn.clone();
            let address = peer.netbird_ip.clone();
            let class = match probe_target(&peer, options.port()) {
                Some(addr) => classify(addr, options.probe_timeout(), origin).await,
                None => HostClass::Candidate,
            };
            HostRecord {
                name,
                fqdn,
                address,
                overlay: "netbird".to_owned(),
                class,
            }
        })
        // Preserve NetBird's deterministic peer ordering while still starting
        // up to `concurrency` probes at once. CLI JSON consumers rely on a
        // stable ordering for reproducible output.
        .buffered(options.concurrency())
        .collect()
        .await
}

fn probe_target(peer: &netbird::Peer, port: u16) -> Option<SocketAddr> {
    peer.ip()
        .filter(|ip| netbird::is_netbird_ip(*ip))
        .map(|ip| SocketAddr::new(ip, port))
}

async fn classify(
    addr: SocketAddr,
    timeout: Duration,
    origin: Option<&RequestOrigin>,
) -> HostClass {
    match tokio::time::timeout(timeout, probe_health(addr, origin)).await {
        Ok(Ok(response)) => classify_response(&response),
        Ok(Err(_)) | Err(_) => HostClass::Unreachable,
    }
}

async fn probe_health(
    addr: SocketAddr,
    origin: Option<&RequestOrigin>,
) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = TcpStream::connect(addr).await?;
    let request = Request::new(
        crate::next_request_id(method::DAEMON_HEALTH),
        method::DAEMON_HEALTH,
        Value::Null,
    )?;
    let request = match origin {
        Some(origin) => origin.apply(request)?,
        None => request,
    };
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;
    let reply = read_line(&mut stream, MAX_CONTROL_LINE_BYTES).await?;
    let response: Response = serde_json::from_slice(&reply)?;
    if response.id() != request.id() {
        return Err("probe response id did not match request".into());
    }
    Ok(response)
}

async fn read_line(
    stream: &mut TcpStream,
    max: usize,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut buf = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let count = stream.read(&mut byte).await?;
        if count == 0 {
            return Err("peer closed the connection without a response".into());
        }
        if byte[0] == b'\n' {
            return Ok(buf);
        }
        if buf.len() >= max {
            return Err("probe response exceeded maximum length".into());
        }
        buf.push(byte[0]);
    }
}

fn classify_response(response: &Response) -> HostClass {
    let daemon_protocol_version = response.version().get();
    if !SUPPORTED_PROTOCOL_VERSIONS.contains(response.version()) {
        return HostClass::VersionMismatch {
            daemon_protocol_version,
        };
    }
    match response.result() {
        Ok(ok) => HostClass::ReachableDaemon {
            daemon_version: ok
                .get("daemon_version")
                .and_then(Value::as_str)
                .map_or_else(|| "<unknown>".to_owned(), str::to_owned),
        },
        Err(_) => HostClass::Unreachable,
    }
}

fn short_hostname(fqdn: &str) -> &str {
    fqdn.split('.').next().unwrap_or(fqdn)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use protocol::{ProtocolVersion, ProtocolVersionRange, PROTOCOL_VERSION};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn candidate_gate_is_connected_idle_and_hostile_safe() {
        let port = 7421;
        for ip in ["100.92.30.40", "100.92.30.41"] {
            let peer = netbird::Peer {
                netbird_ip: Some(ip.to_owned()),
                ..netbird::Peer::default()
            };
            assert!(probe_target(&peer, port).is_some());
        }
        let no_ip = netbird::Peer::default();
        assert_eq!(probe_target(&no_ip, port), None);
        for hostile in ["127.0.0.1", "169.254.169.254", "10.0.0.1", "8.8.8.8"] {
            let peer = netbird::Peer {
                netbird_ip: Some(hostile.to_owned()),
                ..netbird::Peer::default()
            };
            assert_eq!(probe_target(&peer, port), None, "must not dial {hostile}");
        }
    }

    #[test]
    fn options_reject_zero_port_concurrency_and_deadline() {
        let zero_port = DiscoveryOptions::new(0).expect_err("zero port");
        let options = DiscoveryOptions::new(7421).expect("options");
        let zero_concurrency = options.with_concurrency(0).expect_err("zero concurrency");
        let zero_deadline = options
            .with_deadline(Duration::ZERO)
            .expect_err("zero deadline");
        let overflow_deadline = options
            .with_deadline(Duration::MAX)
            .expect_err("deadline must retain lock margin");
        let zero_probe_timeout = options
            .with_probe_timeout(Duration::ZERO)
            .expect_err("zero probe timeout");
        for error in [
            zero_port,
            zero_concurrency,
            zero_deadline,
            overflow_deadline,
            zero_probe_timeout,
        ] {
            assert!(matches!(
                error,
                crate::ClientError::InvalidDiscoveryOptions { .. }
            ));
            let structured = error.to_protocol_error();
            assert_eq!(structured.class, protocol::ErrorClass::Configuration);
            assert_eq!(structured.code, "invalid_discovery_options");
            assert_eq!(
                structured.recover.as_deref(),
                Some("fix the invalid discovery option before running discovery")
            );
        }
    }

    #[tokio::test]
    async fn overall_deadline_bounds_status_loading() {
        let options = DiscoveryOptions::new(7421)
            .expect("options")
            .with_deadline(Duration::from_millis(10))
            .expect("deadline");
        let result = discover_with_status(
            options,
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(netbird::parse_status(r#"{"peers":[]}"#).expect("status"))
            },
            None,
        )
        .await;
        let timeout = result.expect_err("status loading must time out");
        assert!(matches!(
            timeout,
            crate::ClientError::RemoteDiscoveryFailed { .. }
        ));
        let structured = timeout.to_protocol_error();
        assert_eq!(structured.class, protocol::ErrorClass::Discovery);
        assert_eq!(structured.code, "remote_discovery_failed");
        assert_eq!(
            structured.recover.as_deref(),
            Some("retry the remote request; if it persists, check the local NetBird state")
        );
    }

    #[test]
    fn response_version_is_the_classification_authority() {
        let other = PROTOCOL_VERSION.get() + 1;
        let other_version = ProtocolVersion::new(other).expect("nonzero test version");
        let response = Response::err(
            other_version,
            "probe",
            protocol::ProtocolError::version_mismatch(
                ProtocolVersionRange::new(PROTOCOL_VERSION, PROTOCOL_VERSION)
                    .expect("valid exact test range"),
                ProtocolVersionRange::new(other_version, other_version)
                    .expect("valid exact test range"),
            ),
        )
        .expect("valid test response");
        assert_eq!(
            classify_response(&response),
            HostClass::VersionMismatch {
                daemon_protocol_version: other
            }
        );
    }

    #[tokio::test]
    async fn closed_daemon_is_unreachable() {
        let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        drop(listener);
        assert_eq!(
            classify(addr, Duration::from_millis(100), None).await,
            HostClass::Unreachable
        );
    }

    async fn health_stub(line: Vec<u8>, delay: Duration) -> SocketAddr {
        let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut byte = [0_u8; 1];
            while socket.read(&mut byte).await.expect("read") != 0 {
                if byte[0] == b'\n' {
                    break;
                }
            }
            tokio::time::sleep(delay).await;
            socket.write_all(&line).await.expect("write");
        });
        addr
    }

    async fn health_echo_stub(daemon_version: &str) -> SocketAddr {
        let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        let daemon_version = daemon_version.to_owned();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while socket.read(&mut byte).await.expect("read") != 0 {
                if byte[0] == b'\n' {
                    break;
                }
                request.push(byte[0]);
            }
            let request: Request = serde_json::from_slice(&request).expect("request");
            let response = serde_json::json!({
                "v": PROTOCOL_VERSION.get(),
                "id": request.id(),
                "ok": { "daemon_version": daemon_version },
            });
            socket
                .write_all(format!("{response}\n").as_bytes())
                .await
                .expect("write");
        });
        addr
    }

    async fn health_capture_stub() -> (SocketAddr, oneshot::Receiver<Request>) {
        let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        let (request_tx, request_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_line(&mut socket, MAX_CONTROL_LINE_BYTES)
                .await
                .expect("read request");
            let request: Request = serde_json::from_slice(&request).expect("request");
            let response = serde_json::json!({
                "v": PROTOCOL_VERSION.get(),
                "id": request.id(),
                "ok": { "daemon_version": "test" },
            });
            socket
                .write_all(format!("{response}\n").as_bytes())
                .await
                .expect("write");
            request_tx.send(request).expect("capture request");
        });
        (addr, request_rx)
    }

    #[tokio::test]
    async fn health_response_extracts_daemon_version() {
        let addr = health_echo_stub("1.2.3").await;
        assert_eq!(
            classify(addr, DEFAULT_PROBE_TIMEOUT, None).await,
            HostClass::ReachableDaemon {
                daemon_version: "1.2.3".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn discovery_health_probe_carries_complete_origin_pair() {
        let (addr, request_rx) = health_capture_stub().await;
        let origin = RequestOrigin::from_values(
            Some("session-origin".to_owned()),
            Some("daemon-origin".to_owned()),
        )
        .expect("valid origin")
        .expect("complete origin");

        let response = probe_health(addr, Some(&origin))
            .await
            .expect("health response");
        let request = request_rx.await.expect("captured request");
        assert_eq!(response.id(), request.id());
        assert_eq!(
            request.origin_session_id(),
            Some(&protocol::SessionId("session-origin".to_owned()))
        );
        assert_eq!(request.origin_daemon_id(), Some("daemon-origin"));
    }

    #[tokio::test]
    async fn mismatched_response_id_is_unreachable() {
        let line = format!(
            "{{\"v\":{},\"id\":\"wrong\",\"ok\":{{\"daemon_version\":\"1.2.3\"}}}}\n",
            PROTOCOL_VERSION.get()
        )
        .into_bytes();
        let addr = health_stub(line, Duration::ZERO).await;
        assert_eq!(
            classify(addr, DEFAULT_PROBE_TIMEOUT, None).await,
            HostClass::Unreachable
        );
    }

    #[tokio::test]
    async fn malformed_oversized_and_timeout_responses_are_unreachable() {
        let malformed = health_stub(b"not-json\n".to_vec(), Duration::ZERO).await;
        assert_eq!(
            classify(malformed, DEFAULT_PROBE_TIMEOUT, None).await,
            HostClass::Unreachable
        );

        let oversized = health_stub(vec![b'x'; MAX_CONTROL_LINE_BYTES + 1], Duration::ZERO).await;
        assert_eq!(
            classify(oversized, DEFAULT_PROBE_TIMEOUT, None).await,
            HostClass::Unreachable
        );

        let slow = health_stub(Vec::new(), Duration::from_millis(100)).await;
        assert_eq!(
            classify(slow, Duration::from_millis(10), None).await,
            HostClass::Unreachable
        );
    }

    #[tokio::test]
    async fn status_order_is_preserved_despite_concurrent_buffering() {
        let status = netbird::parse_status(
            r#"{"peers":[
                {"fqdn":"first.example","netbirdIp":"100.64.0.1"},
                {"fqdn":"second.example","netbirdIp":"100.64.0.2"}
            ]}"#,
        )
        .expect("status");
        let records = discover_status(
            &status,
            DiscoveryOptions::new(1)
                .expect("options")
                .with_probe_timeout(Duration::from_millis(1))
                .expect("probe timeout")
                .with_concurrency(2)
                .expect("concurrency"),
            None,
        )
        .await;
        assert_eq!(records[0].name.as_deref(), Some("first"));
        assert_eq!(records[1].name.as_deref(), Some("second"));
    }
}
