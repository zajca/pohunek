//! `host.discover` against an overlay peer set at and above the daemon's count bound.
//!
//! A stub overlay provider reports a configurable number of address-less peers,
//! which stay unprobed candidates, so no probe leaves the test host. The daemon
//! answers on its local Unix socket: one peer more than the bound is the typed
//! `host_discover_too_large` error, while exactly the bound lists completely in
//! one reply. A peer whose loopback health stub overshoots the daemon-version
//! cap is still listed, reported unreachable with the bounded
//! `daemon_version_too_large` reason and without echoing the version; a peer
//! whose identity strings overshoot their caps is kept with those fields
//! omitted and the `identity_too_large` reason.

// Rust guideline compliant 2026-11-27

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
    OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer, ResolvedSelector,
};
use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::procwatch::HostInspector;
use pohunek_daemon::runtime::SubprocessWorkerLauncher;
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use protocol::{method, Request, Response};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

const OVERLAY: &str = "stub";
const PORT: u16 = 18_722;

/// The daemon's hard count bound on the `host.discover` reply, mirroring
/// `MAX_HOST_DISCOVER_RECORDS` in `crates/daemon/src/api/handler/host.rs`.
const BOUND: usize = 1500;

/// Provider that reports the given peers verbatim.
#[derive(Debug)]
struct StubOverlay {
    id: OverlayId,
    peers: Vec<DiscoveredPeer>,
}

impl OverlayTransport for StubOverlay {
    fn id(&self) -> &OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, addr: IpAddr) -> Result<(), BindAddrError> {
        Err(BindAddrError::NotMember(addr))
    }

    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
        Box::pin(async { Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)) })
    }

    fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
        let overlay = self.id.clone();
        Box::pin(async move {
            Err(OverlayError::HostUnknown {
                host: host.to_owned(),
                overlay,
            })
        })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        let overlay = self.id.clone();
        Box::pin(async move {
            Err(OverlayError::HostUnknown {
                host: identity.value().to_owned(),
                overlay,
            })
        })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        let peers = self.peers.clone();
        Box::pin(async move { Ok(peers) })
    }

    fn resolve_selector<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedSelector> {
        let overlay = self.id.clone();
        Box::pin(async move {
            Err(OverlayError::HostUnknown {
                host: host.to_owned(),
                overlay,
            })
        })
    }
}

fn registry(peers: usize) -> OverlayRegistry {
    let peers = (0..peers)
        .map(|index| DiscoveredPeer {
            peer_id: Some(format!("peer-{index:04}")),
            display_name: Some(format!("peer-{index:04}")),
            fqdn: Some(format!("peer-{index:04}.overlay.test")),
            // No address: the peer stays an unprobed candidate.
            address: None,
            address_ambiguous: false,
        })
        .collect();
    registry_with(peers, PORT)
}

fn registry_with(peers: Vec<DiscoveredPeer>, port: u16) -> OverlayRegistry {
    let transport = StubOverlay {
        id: OverlayId::new(OVERLAY).expect("overlay id"),
        peers,
    };
    OverlayRegistry::new(vec![
        ConfiguredTransport::new(Arc::new(transport), port).expect("configured transport")
    ])
    .expect("registry")
}

/// A daemon serving one state on a Unix socket.
struct Daemon {
    socket: std::path::PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(tag: &str, peers: usize) -> Self {
        Self::start_with_registry(tag, registry(peers)).await
    }

    async fn start_with_registry(tag: &str, registry: OverlayRegistry) -> Self {
        let dir = pohunek_test_support::tempdir_with_prefix(&format!("ph-{tag}-"))
            .expect("create the socket directory");
        let socket = dir.path().join("daemon.sock");
        let governance = Arc::new(
            HostGovernanceService::open(dir.path().join("state"))
                .await
                .expect("open real isolated host-governance service"),
        );
        let sessions = SessionRegistry::new_with_launcher_and_inspector(
            SessionRegistryConfig::default(),
            Arc::new(SubprocessWorkerLauncher::new()),
            Arc::new(HostInspector::new()),
        );
        let state = DaemonState::new(
            HealthInfo::new("9.9.9-test"),
            sessions,
            governance,
            registry,
            pohunek_daemon::api::OverlaySource::Default,
        );
        let unix = ControlServer::bind_with_state(&socket, state)
            .await
            .expect("unix server binds");
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            unix.serve(
                pohunek_daemon::api::ConnectionTasks::new(
                    "control_connections",
                    &tokio_util::sync::CancellationToken::new(),
                ),
                async move {
                    let _ = rx.await;
                },
            )
            .await;
        });
        Self {
            socket,
            shutdown: Some(tx),
            task: Some(task),
            _dir: dir,
        }
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

async fn discover(socket: &Path) -> Response {
    let mut framed = Framed::new(
        UnixStream::connect(socket).await.expect("unix connect"),
        LinesCodec::new(),
    );
    let request =
        Request::new("host-discover-bound", method::HOST_DISCOVER, Value::Null).expect("request");
    framed
        .send(serde_json::to_string(&request).expect("request serializes"))
        .await
        .expect("send");
    let line = framed.next().await.expect("a reply").expect("a line");
    serde_json::from_str(&line).expect("a response")
}

#[tokio::test]
async fn a_peer_set_one_above_the_bound_is_a_typed_error() {
    let daemon = Daemon::start("hd-over", BOUND + 1).await;

    let error = discover(&daemon.socket)
        .await
        .into_result()
        .expect_err("one peer above the bound does not list");
    assert_eq!(error.code, "host_discover_too_large", "{error:?}");

    daemon.stop().await;
}

#[tokio::test]
async fn a_peer_set_at_the_bound_lists_completely_in_one_reply() {
    let daemon = Daemon::start("hd-at", BOUND).await;

    let result = discover(&daemon.socket)
        .await
        .into_result()
        .expect("the bound-sized set fits one control line");
    let records = result.as_array().expect("the peer records");
    assert_eq!(records.len(), BOUND, "every peer of the set is listed");

    daemon.stop().await;
}

/// A loopback health stub that answers one probe with a `daemon_version` of
/// `version_len` bytes, mimicking a peer whose health reply overshoots the
/// version cap. Returns the port the stub listens on.
async fn oversized_version_stub(version_len: usize) -> u16 {
    let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind the health stub");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut line = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            let count = socket.read(&mut byte).await.expect("read");
            if count == 0 || byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        let request: Request = serde_json::from_slice(&line).expect("request");
        let version =
            protocol::negotiate(request.version_range(), protocol::CURRENT_PROTOCOL_VERSIONS)
                .expect("the daemon's range overlaps the stub's");
        let response = Response::ok(
            version,
            request.id(),
            serde_json::json!({ "daemon_version": "v".repeat(version_len) }),
        )
        .expect("valid response");
        let mut out = serde_json::to_vec(&response).expect("serialize");
        out.push(b'\n');
        socket.write_all(&out).await.expect("write");
    });
    port
}

#[tokio::test]
async fn a_peer_whose_version_overshoots_the_cap_is_reported_unreachable_with_a_reason() {
    let stub_port = oversized_version_stub(1100).await;
    let hostile = DiscoveredPeer {
        peer_id: Some("hostile-peer".to_owned()),
        display_name: Some("hostile".to_owned()),
        fqdn: Some("hostile.overlay.test".to_owned()),
        address: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        address_ambiguous: false,
    };
    let daemon =
        Daemon::start_with_registry("hd-version", registry_with(vec![hostile], stub_port)).await;

    let result = discover(&daemon.socket)
        .await
        .into_result()
        .expect("an oversized version does not break the reply");
    let records = result.as_array().expect("the peer records");
    assert_eq!(records.len(), 1, "the peer is still listed");
    let record = &records[0];
    assert_eq!(record["classification"], "unreachable", "{record}");
    assert_eq!(record["reason"], "daemon_version_too_large", "{record}");
    assert!(
        record.get("daemon_version").is_none(),
        "the over-cap version is not echoed: {record}"
    );
    assert_eq!(record["name"], "hostile", "{record}");

    daemon.stop().await;
}

#[tokio::test]
async fn a_peer_with_over_cap_identity_fields_is_kept_with_omitted_fields_and_a_reason() {
    let oversized = DiscoveredPeer {
        peer_id: Some("p".repeat(100)),
        display_name: Some("n".repeat(100)),
        fqdn: Some("f".repeat(300)),
        // No address: the peer stays an unprobed candidate.
        address: None,
        address_ambiguous: false,
    };
    let daemon =
        Daemon::start_with_registry("hd-identity", registry_with(vec![oversized], PORT)).await;

    let result = discover(&daemon.socket)
        .await
        .into_result()
        .expect("over-cap identity fields do not break the reply");
    let records = result.as_array().expect("the peer records");
    assert_eq!(records.len(), 1, "the peer is still listed");
    let record = &records[0];
    assert_eq!(record["classification"], "candidate", "{record}");
    assert_eq!(record["reason"], "identity_too_large", "{record}");
    assert!(record["name"].is_null(), "over-cap name omitted: {record}");
    assert!(record["fqdn"].is_null(), "over-cap fqdn omitted: {record}");
    assert!(
        record["peer_id"].is_null(),
        "over-cap peer_id omitted: {record}"
    );

    daemon.stop().await;
}
