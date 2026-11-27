//! `host.discover` against an overlay peer set at and above the daemon's count bound.
//!
//! A stub overlay provider reports a configurable number of address-less peers,
//! which stay unprobed candidates, so no probe leaves the test host. The daemon
//! answers on its local Unix socket: one peer more than the bound is the typed
//! `host_discover_too_large` error, while exactly the bound lists completely in
//! one reply.

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
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

const OVERLAY: &str = "stub";
const PORT: u16 = 18_722;

/// The daemon's hard count bound on the `host.discover` reply, mirroring
/// `MAX_HOST_DISCOVER_RECORDS` in `crates/daemon/src/api/handler/host.rs`.
const BOUND: usize = 1024;

/// Provider that reports `peers` address-less candidate peers.
#[derive(Debug)]
struct StubOverlay {
    id: OverlayId,
    peers: usize,
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
        let peers = (0..self.peers)
            .map(|index| DiscoveredPeer {
                peer_id: Some(format!("peer-{index:04}")),
                display_name: Some(format!("peer-{index:04}")),
                fqdn: Some(format!("peer-{index:04}.overlay.test")),
                // No address: the peer stays an unprobed candidate.
                address: None,
                address_ambiguous: false,
            })
            .collect();
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
    let transport = StubOverlay {
        id: OverlayId::new(OVERLAY).expect("overlay id"),
        peers,
    };
    OverlayRegistry::new(vec![
        ConfiguredTransport::new(Arc::new(transport), PORT).expect("configured transport")
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
            registry(peers),
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
