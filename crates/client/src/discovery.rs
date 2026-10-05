//! Overlay peer discovery and bounded daemon classification.
//!
//! This module is the protocol-aware discovery implementation used by the CLI
//! and daemon. Each configured overlay supplies its own peer snapshot; this
//! module isolates per-overlay failures, aggregates their peers, and owns the
//! asynchronous health exchange instead.

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::time::Duration;

use futures::{stream, StreamExt as _};
use overlay::{OverlayFailure, OverlayRegistry, RoutedPeer};
use protocol::{
    method, HostClass, HostRecord, ProtocolVersionRange, Request, Response,
    CLIENT_PROTOCOL_VERSIONS, MAX_CONTROL_LINE_BYTES,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::transport::{OriginSource, RequestOrigin};

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
    probe_timeout: Duration,
    concurrency: NonZeroUsize,
    deadline: Duration,
    origin_source: OriginSource,
    protocol_range: ProtocolVersionRange,
}

impl DiscoveryOptions {
    /// Build the default bounded discovery settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            concurrency: NonZeroUsize::new(DEFAULT_PROBE_CONCURRENCY)
                .expect("default concurrency is non-zero"),
            deadline: DEFAULT_DISCOVERY_DEADLINE,
            origin_source: OriginSource::default(),
            protocol_range: CLIENT_PROTOCOL_VERSIONS,
        }
    }

    /// Return the protocol range probes offer and classification accepts.
    #[must_use]
    pub fn protocol_range(&self) -> ProtocolVersionRange {
        self.protocol_range
    }

    /// Probe and classify peers for one protocol range.
    ///
    /// The default is the client window, which is what a standalone client
    /// speaks. A daemon answering `host.discover` passes the range the asking
    /// client advertised, so the classification matches what that client can
    /// actually talk to: a peer is reachable when it answers inside the range.
    #[must_use]
    pub fn with_protocol_range(mut self, range: ProtocolVersionRange) -> Self {
        self.protocol_range = range;
        self
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

    /// Return where the probes take their request origin from.
    #[must_use]
    pub fn origin_source(&self) -> OriginSource {
        self.origin_source
    }

    /// Return options that take the request origin from `origin_source`.
    #[must_use]
    pub fn with_origin_source(mut self, origin_source: OriginSource) -> Self {
        self.origin_source = origin_source;
        self
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

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self::new()
    }
}

fn invalid_option(detail: &str) -> crate::ClientError {
    crate::ClientError::InvalidDiscoveryOptions {
        detail: detail.to_owned(),
    }
}

/// Discover and classify peers from the configured overlay registry.
///
/// Only peers advertising an address in the `NetBird` CGNAT range are dialed.
/// Peers with no safe address remain candidates. A transport, framing, or health
/// failure is represented as [`HostClass::Unreachable`] rather than failing the
/// whole enumeration.
///
/// # Errors
///
/// Returns [`crate::ClientError::InvalidDiscoveryOptions`] when a configured
/// port is invalid. Returns [`crate::ClientError::RemoteDiscoveryFailed`] when
/// no configured overlay completes discovery before the deadline.
pub async fn discover_hosts(
    registry: &OverlayRegistry,
) -> Result<Vec<HostRecord>, crate::ClientError> {
    discover_hosts_with_options(registry, DiscoveryOptions::new()).await
}

/// Discover peers using caller-supplied bounded probe settings.
///
/// An empty transport collection returns no peers; per-overlay failures are
/// logged and isolated so another configured overlay can still contribute.
///
/// # Errors
///
/// Returns [`crate::ClientError::RemoteDiscoveryFailed`] when no configured
/// overlay completes before [`DiscoveryOptions::deadline`]. Completed healthy
/// overlay snapshots remain usable when another provider reaches that
/// deadline. With [`crate::OriginSource::Environment`] (the default) returns an
/// origin-environment error when exactly one origin marker is present or a
/// marker value is invalid.
pub async fn discover_hosts_with_options(
    registry: &OverlayRegistry,
    options: DiscoveryOptions,
) -> Result<Vec<HostRecord>, crate::ClientError> {
    let origin = options.origin_source().resolve()?;
    discover_with_registry(registry, options, origin).await
}

async fn discover_with_registry(
    registry: &OverlayRegistry,
    options: DiscoveryOptions,
    origin: Option<RequestOrigin>,
) -> Result<Vec<HostRecord>, crate::ClientError> {
    let deadline = tokio::time::Instant::now() + options.deadline();
    let mut results = Vec::with_capacity(registry.entries().len());
    let mut completed = vec![false; registry.entries().len()];
    let mut pending = stream::iter(registry.entries().iter().cloned().enumerate())
        .map(|(index, configured)| async move {
            let overlay = configured.id().clone();
            let result = configured.discover_peers().await;
            (index, overlay, result)
        })
        .buffer_unordered(registry.entries().len());
    let providers_timed_out = loop {
        match tokio::time::timeout_at(deadline, pending.next()).await {
            Ok(Some(result)) => {
                completed[result.0] = true;
                results.push(result);
            }
            Ok(None) => break false,
            Err(_timeout) => break true,
        }
    };
    drop(pending);

    if providers_timed_out {
        for (index, configured) in registry.entries().iter().enumerate() {
            if !completed[index] {
                tracing::debug!(
                    overlay = %configured.id(),
                    "one configured overlay exceeded the discovery deadline"
                );
            }
        }
    }

    results.sort_by_key(|(index, _, _)| *index);
    let mut snapshots = Vec::new();
    let mut failures = Vec::new();
    let mut healthy_overlays = 0_usize;
    for (_, overlay, result) in results {
        match result {
            Ok(peers) => {
                healthy_overlays += 1;
                snapshots.extend(peers);
            }
            Err(error) => {
                tracing::debug!(
                    overlay = %overlay,
                    error = %error,
                    "one configured overlay was unavailable during discovery"
                );
                failures.push(OverlayFailure { overlay, error });
            }
        }
    }
    if healthy_overlays == 0 {
        if providers_timed_out {
            return Err(crate::ClientError::RemoteDiscoveryFailed {
                detail: "no overlay completed discovery before the configured deadline".to_owned(),
            });
        }
        return Err(crate::ClientError::OverlayDiscoveryFailed { failures });
    }
    Ok(discover_peers(snapshots, options, origin.as_ref(), Some(deadline)).await)
}

/// Classify aggregated overlay peer snapshots.
#[must_use]
async fn discover_peers(
    peers: Vec<RoutedPeer>,
    options: DiscoveryOptions,
    origin: Option<&RequestOrigin>,
    deadline: Option<tokio::time::Instant>,
) -> Vec<HostRecord> {
    stream::iter(peers)
        .map(|peer| async move {
            let name = peer
                .display_name
                .clone()
                .or_else(|| peer.fqdn.as_deref().map(short_hostname).map(str::to_owned));
            let fqdn = peer.fqdn.clone();
            let address = peer.addr.map(|addr| addr.ip().to_string());
            let port = peer.port;
            let probe_timeout = deadline.map_or(options.probe_timeout(), |deadline| {
                options
                    .probe_timeout()
                    .min(deadline.saturating_duration_since(tokio::time::Instant::now()))
            });
            let class = match peer.addr {
                Some(addr) => classify(addr, probe_timeout, origin, options.protocol_range()).await,
                None => HostClass::Candidate,
            };
            HostRecord {
                name,
                fqdn,
                address,
                port,
                overlay: peer.overlay.to_string(),
                peer_id: peer.peer_id,
                class,
            }
        })
        // Preserve each overlay's deterministic peer ordering while still starting
        // up to `concurrency` probes at once. CLI JSON consumers rely on a
        // stable ordering for reproducible output.
        .buffered(options.concurrency())
        .collect()
        .await
}

async fn classify(
    addr: SocketAddr,
    timeout: Duration,
    origin: Option<&RequestOrigin>,
    range: ProtocolVersionRange,
) -> HostClass {
    match tokio::time::timeout(timeout, probe_health(addr, origin, range)).await {
        Ok(Ok(response)) => classify_response(&response, range),
        Ok(Err(_)) | Err(_) => HostClass::Unreachable,
    }
}

async fn probe_health(
    addr: SocketAddr,
    origin: Option<&RequestOrigin>,
    range: ProtocolVersionRange,
) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = TcpStream::connect(addr).await?;
    let request = Request::new(
        crate::next_request_id(method::DAEMON_HEALTH),
        method::DAEMON_HEALTH,
        Value::Null,
    )?
    .with_version_range(range);
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

fn classify_response(response: &Response, range: ProtocolVersionRange) -> HostClass {
    let daemon_protocol_version = response.version().get();
    if !range.contains(response.version()) {
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use protocol::{ProtocolVersion, CURRENT_PROTOCOL_VERSIONS, PROTOCOL_VERSION};
    use tokio::net::{TcpListener, TcpSocket};
    use tokio::sync::oneshot;

    use super::*;
    use overlay::{
        BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
        OverlayFuture, OverlayId, OverlayTransport, ResolvedPeer,
    };

    #[derive(Debug)]
    struct TestTransport {
        id: OverlayId,
        discovery: Result<Vec<DiscoveredPeer>, OverlayError>,
    }

    impl TestTransport {
        fn healthy(id: &str, peers: Vec<DiscoveredPeer>) -> Self {
            Self {
                id: OverlayId::new(id).expect("test overlay id"),
                discovery: Ok(peers),
            }
        }

        fn unavailable(id: &str) -> Self {
            let id = OverlayId::new(id).expect("test overlay id");
            Self {
                discovery: Err(OverlayError::StateUnavailable {
                    overlay: id.clone(),
                    detail: "test state unavailable".to_owned(),
                }),
                id,
            }
        }
    }

    impl OverlayTransport for TestTransport {
        fn id(&self) -> &OverlayId {
            &self.id
        }

        fn validate_bind_addr(&self, _addr: IpAddr) -> Result<(), BindAddrError> {
            Ok(())
        }

        fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
            Box::pin(async { Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)) })
        }

        fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
            let error = OverlayError::HostUnknown {
                host: host.to_owned(),
                overlay: self.id.clone(),
            };
            Box::pin(async move { Err(error) })
        }

        fn resolve_peer_identity<'a>(
            &'a self,
            identity: &'a ExternalIdentity,
        ) -> OverlayFuture<'a, ResolvedPeer> {
            let error = OverlayError::HostUnknown {
                host: identity.value().to_owned(),
                overlay: self.id.clone(),
            };
            Box::pin(async move { Err(error) })
        }

        fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
            let discovery = self.discovery.clone();
            Box::pin(async move { discovery })
        }
    }

    #[derive(Debug)]
    struct PendingTransport {
        id: OverlayId,
        dropped: Arc<AtomicBool>,
    }

    impl OverlayTransport for PendingTransport {
        fn id(&self) -> &OverlayId {
            &self.id
        }

        fn validate_bind_addr(&self, _addr: IpAddr) -> Result<(), BindAddrError> {
            Ok(())
        }

        fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
            Box::pin(std::future::pending())
        }

        fn resolve_peer<'a>(&'a self, _host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
            Box::pin(std::future::pending())
        }

        fn resolve_peer_identity<'a>(
            &'a self,
            _identity: &'a ExternalIdentity,
        ) -> OverlayFuture<'a, ResolvedPeer> {
            Box::pin(std::future::pending())
        }

        fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
            Box::pin(DropSignal {
                dropped: Arc::clone(&self.dropped),
            })
        }
    }

    #[derive(Debug)]
    struct DropSignal {
        dropped: Arc<AtomicBool>,
    }

    impl std::future::Future for DropSignal {
        type Output = Result<Vec<DiscoveredPeer>, OverlayError>;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            std::task::Poll::Pending
        }
    }

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn configured(transport: TestTransport, port: u16) -> ConfiguredTransport {
        ConfiguredTransport::new(Arc::new(transport), port).expect("configured test transport")
    }

    #[test]
    fn options_reject_zero_concurrency_and_deadline() {
        let options = DiscoveryOptions::new();
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
            classify_response(&response, CURRENT_PROTOCOL_VERSIONS),
            HostClass::VersionMismatch {
                daemon_protocol_version: other
            }
        );
    }

    /// A daemon stub that answers a health probe like a daemon whose only
    /// protocol version is `served`: success when the probe asks for it, a typed
    /// mismatch stamped with `served` otherwise.
    async fn single_version_stub(served: ProtocolVersion) -> SocketAddr {
        let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let line = read_line(&mut socket, MAX_CONTROL_LINE_BYTES)
                .await
                .expect("read request");
            let request: Request = serde_json::from_slice(&line).expect("request");
            let own = ProtocolVersionRange::new(served, served).expect("exact range");
            let response = match protocol::negotiate(request.version_range(), own) {
                Ok(version) => Response::ok(
                    version,
                    request.id(),
                    serde_json::json!({ "daemon_version": "stub" }),
                ),
                Err(error) => Response::err(served, request.id(), error),
            }
            .expect("valid response");
            let mut out = serde_json::to_vec(&response).expect("serialize");
            out.push(b'\n');
            socket.write_all(&out).await.expect("write");
        });
        addr
    }

    fn version(value: u32) -> ProtocolVersion {
        ProtocolVersion::new(value).expect("nonzero test version")
    }

    fn exact(version: ProtocolVersion) -> ProtocolVersionRange {
        ProtocolVersionRange::new(version, version).expect("exact range")
    }

    #[tokio::test]
    async fn probe_and_classification_follow_the_requested_range() {
        let previous = protocol::MIN_PROTOCOL_VERSION;
        let reachable = HostClass::ReachableDaemon {
            daemon_version: "stub".to_owned(),
        };
        let mismatch = |daemon: ProtocolVersion| HostClass::VersionMismatch {
            daemon_protocol_version: daemon.get(),
        };

        // A previous-version requester: a v3-only daemon is reachable, a
        // v4-only one is not.
        let v3_only = single_version_stub(previous).await;
        assert_eq!(
            classify(v3_only, DEFAULT_PROBE_TIMEOUT, None, exact(previous)).await,
            reachable
        );
        let v4_only = single_version_stub(PROTOCOL_VERSION).await;
        assert_eq!(
            classify(v4_only, DEFAULT_PROBE_TIMEOUT, None, exact(previous)).await,
            mismatch(PROTOCOL_VERSION)
        );

        // A current-only requester sees the opposite.
        let v3_only = single_version_stub(previous).await;
        assert_eq!(
            classify(
                v3_only,
                DEFAULT_PROBE_TIMEOUT,
                None,
                CURRENT_PROTOCOL_VERSIONS
            )
            .await,
            mismatch(previous)
        );
        let v4_only = single_version_stub(PROTOCOL_VERSION).await;
        assert_eq!(
            classify(
                v4_only,
                DEFAULT_PROBE_TIMEOUT,
                None,
                CURRENT_PROTOCOL_VERSIONS
            )
            .await,
            reachable
        );

        // A requester that speaks the whole window reaches both.
        let v3_only = single_version_stub(previous).await;
        assert_eq!(
            classify(
                v3_only,
                DEFAULT_PROBE_TIMEOUT,
                None,
                CLIENT_PROTOCOL_VERSIONS
            )
            .await,
            reachable
        );
        let v4_only = single_version_stub(PROTOCOL_VERSION).await;
        assert_eq!(
            classify(
                v4_only,
                DEFAULT_PROBE_TIMEOUT,
                None,
                CLIENT_PROTOCOL_VERSIONS
            )
            .await,
            reachable
        );
    }

    #[tokio::test]
    async fn a_daemon_outside_the_requested_range_is_a_version_mismatch() {
        let beyond = ProtocolVersion::new(PROTOCOL_VERSION.get() + 1).expect("nonzero version");
        let stub = single_version_stub(beyond).await;
        assert_eq!(
            classify(stub, DEFAULT_PROBE_TIMEOUT, None, CLIENT_PROTOCOL_VERSIONS).await,
            HostClass::VersionMismatch {
                daemon_protocol_version: beyond.get()
            }
        );
    }

    #[test]
    fn default_options_probe_with_the_client_window() {
        assert_eq!(
            DiscoveryOptions::new().protocol_range(),
            CLIENT_PROTOCOL_VERSIONS
        );
        assert_eq!(
            DiscoveryOptions::new()
                .with_protocol_range(exact(version(3)))
                .protocol_range(),
            exact(version(3))
        );
    }

    /// A loopback endpoint that refuses every connection and that no other
    /// socket can claim while the value is alive.
    ///
    /// The socket is bound but never put into the listening state, so the
    /// kernel answers a connect with a reset. Keeping the socket open keeps the
    /// port reserved; a port that was merely released could be handed to any
    /// concurrently started listener.
    struct RefusedEndpoint {
        addr: SocketAddr,
        _reservation: TcpSocket,
    }

    impl RefusedEndpoint {
        fn reserve() -> Self {
            let socket = TcpSocket::new_v4().expect("create reservation socket");
            socket
                .bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .expect("bind reservation socket");
            let addr = socket.local_addr().expect("reservation address");
            Self {
                addr,
                _reservation: socket,
            }
        }
    }

    #[tokio::test]
    async fn closed_daemon_is_unreachable() {
        let closed = RefusedEndpoint::reserve();
        assert_eq!(
            classify(
                closed.addr,
                Duration::from_millis(100),
                None,
                CURRENT_PROTOCOL_VERSIONS
            )
            .await,
            HostClass::Unreachable
        );
    }

    #[tokio::test]
    async fn refused_endpoint_refuses_connects_and_cannot_be_claimed() {
        let closed = RefusedEndpoint::reserve();

        let connect_error = TcpStream::connect(closed.addr)
            .await
            .expect_err("a reserved, non-listening port refuses connects");
        assert_eq!(connect_error.kind(), std::io::ErrorKind::ConnectionRefused);

        let claim_error = TcpListener::bind(closed.addr)
            .await
            .expect_err("a live reservation must not be claimable by a stub server");
        assert_eq!(claim_error.kind(), std::io::ErrorKind::AddrInUse);
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
            classify(addr, DEFAULT_PROBE_TIMEOUT, None, CURRENT_PROTOCOL_VERSIONS).await,
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

        let response = probe_health(addr, Some(&origin), CURRENT_PROTOCOL_VERSIONS)
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
            classify(addr, DEFAULT_PROBE_TIMEOUT, None, CURRENT_PROTOCOL_VERSIONS).await,
            HostClass::Unreachable
        );
    }

    #[tokio::test]
    async fn malformed_oversized_and_timeout_responses_are_unreachable() {
        let malformed = health_stub(b"not-json\n".to_vec(), Duration::ZERO).await;
        assert_eq!(
            classify(
                malformed,
                DEFAULT_PROBE_TIMEOUT,
                None,
                CURRENT_PROTOCOL_VERSIONS
            )
            .await,
            HostClass::Unreachable
        );

        let oversized = health_stub(vec![b'x'; MAX_CONTROL_LINE_BYTES + 1], Duration::ZERO).await;
        assert_eq!(
            classify(
                oversized,
                DEFAULT_PROBE_TIMEOUT,
                None,
                CURRENT_PROTOCOL_VERSIONS
            )
            .await,
            HostClass::Unreachable
        );

        let slow = health_stub(Vec::new(), Duration::from_millis(100)).await;
        assert_eq!(
            classify(
                slow,
                Duration::from_millis(10),
                None,
                CURRENT_PROTOCOL_VERSIONS
            )
            .await,
            HostClass::Unreachable
        );
    }

    #[tokio::test]
    async fn peer_order_is_preserved_and_unsafe_addresses_remain_candidates() {
        let peers = vec![
            RoutedPeer {
                overlay: OverlayId::new("netbird").expect("id"),
                peer_id: Some("100.64.0.1".to_owned()),
                display_name: None,
                fqdn: Some("first.example".to_owned()),
                addr: Some("100.64.0.1:1".parse().expect("safe socket address")),
                port: 1,
            },
            RoutedPeer {
                overlay: OverlayId::new("netbird").expect("id"),
                peer_id: Some("127.0.0.1".to_owned()),
                display_name: None,
                fqdn: Some("second.example".to_owned()),
                addr: None,
                port: 1,
            },
        ];
        let records = discover_peers(
            peers,
            DiscoveryOptions::new()
                .with_probe_timeout(Duration::from_millis(1))
                .expect("probe timeout")
                .with_concurrency(2)
                .expect("concurrency"),
            None,
            None,
        )
        .await;
        assert_eq!(records[0].name.as_deref(), Some("first"));
        assert_eq!(records[1].name.as_deref(), Some("second"));
        assert_eq!(records[0].overlay, "netbird");
        assert_eq!(records[0].peer_id.as_deref(), Some("100.64.0.1"));
        assert_eq!(records[1].peer_id.as_deref(), Some("127.0.0.1"));
        assert_eq!(records[0].port, 1);
        assert_eq!(records[1].class, HostClass::Candidate);
    }

    #[tokio::test]
    async fn registry_discovery_isolates_failures_and_preserves_addressless_candidates() {
        let candidate = DiscoveredPeer {
            peer_id: None,
            display_name: Some("addressless".to_owned()),
            fqdn: Some("addressless.example".to_owned()),
            address: None,
        };
        let registry = OverlayRegistry::new(vec![
            configured(TestTransport::unavailable("broken"), 17001),
            configured(TestTransport::healthy("memory", vec![candidate]), 17002),
        ])
        .expect("test registry");

        let records = discover_with_registry(&registry, DiscoveryOptions::new(), None)
            .await
            .expect("healthy overlay remains available");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].overlay, "memory");
        assert_eq!(records[0].port, 17002);
        assert_eq!(records[0].address, None);
        assert_eq!(records[0].peer_id, None);
        assert_eq!(records[0].class, HostClass::Candidate);
    }

    #[tokio::test]
    async fn registry_discovery_reports_typed_error_when_every_overlay_fails() {
        let registry = OverlayRegistry::new(vec![
            configured(TestTransport::unavailable("broken-a"), 17001),
            configured(TestTransport::unavailable("broken-b"), 17002),
        ])
        .expect("test registry");

        let error = discover_with_registry(&registry, DiscoveryOptions::new(), None)
            .await
            .expect_err("all failed overlays must be reported");
        assert!(matches!(
            error,
            crate::ClientError::OverlayDiscoveryFailed { failures }
                if failures.len() == 2
                    && failures[0].overlay.as_str() == "broken-a"
                    && failures[1].overlay.as_str() == "broken-b"
        ));
    }

    #[tokio::test]
    async fn discovery_deadline_cancels_provider_future() {
        let dropped = Arc::new(AtomicBool::new(false));
        let pending = ConfiguredTransport::new(
            Arc::new(PendingTransport {
                id: OverlayId::new("pending").expect("id"),
                dropped: Arc::clone(&dropped),
            }),
            17001,
        )
        .expect("pending transport");
        let registry = OverlayRegistry::new(vec![pending]).expect("pending registry");
        let options = DiscoveryOptions::new()
            .with_deadline(Duration::from_millis(10))
            .expect("test deadline");

        let error = discover_with_registry(&registry, options, None)
            .await
            .expect_err("pending provider must hit the complete deadline");

        assert!(matches!(
            error,
            crate::ClientError::RemoteDiscoveryFailed { .. }
        ));
        assert!(
            dropped.load(Ordering::SeqCst),
            "deadline must drop the provider-owned future"
        );
    }

    #[tokio::test]
    async fn discovery_deadline_preserves_completed_overlay_results() {
        let dropped = Arc::new(AtomicBool::new(false));
        let pending = ConfiguredTransport::new(
            Arc::new(PendingTransport {
                id: OverlayId::new("pending").expect("id"),
                dropped: Arc::clone(&dropped),
            }),
            17001,
        )
        .expect("pending transport");
        let candidate = DiscoveredPeer {
            peer_id: None,
            display_name: Some("healthy-peer".to_owned()),
            fqdn: Some("healthy-peer.example".to_owned()),
            address: None,
        };
        let registry = OverlayRegistry::new(vec![
            pending,
            configured(TestTransport::healthy("healthy", vec![candidate]), 17002),
        ])
        .expect("mixed registry");
        let options = DiscoveryOptions::new()
            .with_deadline(Duration::from_millis(10))
            .expect("test deadline");

        let records = discover_with_registry(&registry, options, None)
            .await
            .expect("healthy overlay must survive another provider timing out");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name.as_deref(), Some("healthy-peer"));
        assert_eq!(records[0].overlay, "healthy");
        assert_eq!(records[0].class, HostClass::Candidate);
        assert!(
            dropped.load(Ordering::SeqCst),
            "deadline must cancel the pending provider future"
        );
    }
}
