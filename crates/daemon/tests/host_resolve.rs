//! `host.resolve` at the daemon boundary.
//!
//! A stub overlay provider reports this machine, a peer that carries a stable
//! identity, a peer without any, and a name shared by this machine and a peer.
//! The daemon answers on its local Unix socket with the rules every other
//! client uses, and does not serve the method over the overlay.

// Rust guideline compliant 2026-10-20

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, LocalIdentity,
    OverlayError, OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
    ResolvedSelector,
};
use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo, RemoteServer};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::procwatch::HostInspector;
use pohunek_daemon::runtime::SubprocessWorkerLauncher;
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use protocol::{method, HostResolution, HostResolveParams, Request, Response};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

const OVERLAY: &str = "stub";
const PORT: u16 = 18_722;
const SELF_FQDN: &str = "self.overlay.test";
const PEER_KEY: &str = "peer-key";
/// `PEER_KEY` as it is spelled in a `peer~` selector: unpadded base64url.
const PEER_KEY_SELECTOR: &str = "cGVlci1rZXk";
const PEER_NAME: &str = "peer-b";
const PEER_FQDN: &str = "peer-b.overlay.test";
const BARE_NAME: &str = "bare";
const TWIN_NAME: &str = "twin";
const PEER_ADDRESS: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 9));

/// Provider that knows this machine and two peers.
#[derive(Debug)]
struct StubOverlay {
    id: OverlayId,
}

impl StubOverlay {
    fn identified() -> ResolvedPeer {
        ResolvedPeer {
            peer_id: Some(PEER_KEY.to_owned()),
            display_name: Some(PEER_NAME.to_owned()),
            fqdn: Some(PEER_FQDN.to_owned()),
            address: PEER_ADDRESS,
        }
    }

    fn bare() -> ResolvedPeer {
        ResolvedPeer {
            peer_id: None,
            display_name: Some(BARE_NAME.to_owned()),
            fqdn: None,
            address: PEER_ADDRESS,
        }
    }

    fn unknown(&self, host: &str) -> OverlayError {
        OverlayError::HostUnknown {
            host: host.to_owned(),
            overlay: self.id.clone(),
        }
    }
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
        let result = match host {
            PEER_NAME | TWIN_NAME => Ok(Self::identified()),
            BARE_NAME => Ok(Self::bare()),
            _ => Err(self.unknown(host)),
        };
        Box::pin(async move { result })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        let result = if identity.value() == PEER_KEY || identity.value() == PEER_FQDN {
            Ok(Self::identified())
        } else {
            Err(self.unknown(identity.value()))
        };
        Box::pin(async move { result })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn resolve_selector<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedSelector> {
        let result = match host {
            SELF_FQDN => Ok(ResolvedSelector::LocalHost),
            TWIN_NAME => Err(OverlayError::PeerCollision {
                host: host.to_owned(),
                overlay: self.id.clone(),
                candidates: Vec::new(),
            }),
            PEER_NAME => Ok(ResolvedSelector::Peer(Self::identified())),
            BARE_NAME => Ok(ResolvedSelector::Peer(Self::bare())),
            _ => Err(self.unknown(host)),
        };
        Box::pin(async move { result })
    }

    fn local_identity(&self) -> OverlayFuture<'_, Option<LocalIdentity>> {
        let identity = LocalIdentity {
            overlay: self.id.clone(),
            fqdn: Some(SELF_FQDN.to_owned()),
            address: None,
        };
        Box::pin(async move { Ok(Some(identity)) })
    }
}

fn registry() -> OverlayRegistry {
    let transport = StubOverlay {
        id: OverlayId::new(OVERLAY).expect("overlay id"),
    };
    OverlayRegistry::new(vec![
        ConfiguredTransport::new(Arc::new(transport), PORT).expect("configured transport")
    ])
    .expect("registry")
}

/// A daemon serving the same state on a Unix socket and a loopback TCP
/// listener standing in for the overlay.
struct Daemon {
    socket: std::path::PathBuf,
    tcp: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(tag: &str) -> Self {
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
            registry(),
            pohunek_daemon::api::OverlaySource::Default,
        );
        let remote_state = state.clone();
        let unix = ControlServer::bind_with_state(&socket, state)
            .await
            .expect("unix server binds");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback tcp bind");
        let remote = RemoteServer::from_listener(listener, remote_state);
        let tcp = remote.local_addr();
        let (tx, rx) = oneshot::channel::<()>();
        let (unix_tx, unix_rx) = oneshot::channel::<()>();
        let (remote_tx, remote_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let fanout = async move {
                let _ = rx.await;
                let _ = unix_tx.send(());
                let _ = remote_tx.send(());
            };
            let unix_serve = unix.serve(
                pohunek_daemon::api::ConnectionTasks::new(
                    "control_connections",
                    &tokio_util::sync::CancellationToken::new(),
                ),
                async move {
                    let _ = unix_rx.await;
                },
            );
            let remote_serve = remote.serve(
                pohunek_daemon::api::ConnectionTasks::new(
                    "remote_connections",
                    &tokio_util::sync::CancellationToken::new(),
                ),
                async move {
                    let _ = remote_rx.await;
                },
            );
            tokio::join!(fanout, unix_serve, remote_serve);
        });
        Self {
            socket,
            tcp,
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

async fn unix_client(socket: &Path) -> Framed<UnixStream, LinesCodec> {
    Framed::new(
        UnixStream::connect(socket).await.expect("unix connect"),
        LinesCodec::new(),
    )
}

async fn exchange<T>(framed: &mut Framed<T, LinesCodec>, route: &str) -> Response
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let params = serde_json::to_value(HostResolveParams {
        route: route.to_owned(),
    })
    .expect("params serialize");
    let request = Request::new("host-resolve", method::HOST_RESOLVE, params).expect("request");
    framed
        .send(serde_json::to_string(&request).expect("request serializes"))
        .await
        .expect("send");
    let line = framed.next().await.expect("a reply").expect("a line");
    serde_json::from_str(&line).expect("a response")
}

fn resolution(response: &Response) -> HostResolution {
    serde_json::from_value(ok(response)).expect("a host resolution")
}

fn ok(response: &Response) -> Value {
    serde_json::to_value(response).expect("response serializes")["ok"].clone()
}

fn error_code(response: &Response) -> String {
    let value = serde_json::to_value(response).expect("response serializes");
    value["err"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("an error response: {value}"))
        .to_owned()
}

#[tokio::test]
async fn a_peer_name_resolves_to_its_endpoint_and_a_pinned_identity_route() {
    let daemon = Daemon::start("hr-peer").await;
    let mut client = unix_client(&daemon.socket).await;

    let expected = HostResolution::Remote {
        address: PEER_ADDRESS.to_string(),
        port: PORT,
        pinned_route: format!("{OVERLAY}:peer~{PEER_KEY_SELECTOR}@{PORT}"),
        overlay: OVERLAY.to_owned(),
        peer_id: Some(PEER_KEY.to_owned()),
        fqdn: Some(PEER_FQDN.to_owned()),
    };
    assert_eq!(
        resolution(&exchange(&mut client, PEER_NAME).await),
        expected
    );

    // The pinned route names the same peer, with an explicit port kept.
    let pinned = format!("{OVERLAY}:peer~{PEER_KEY_SELECTOR}@19000");
    let HostResolution::Remote {
        port, pinned_route, ..
    } = resolution(&exchange(&mut client, &pinned).await)
    else {
        panic!("a pinned route names a peer");
    };
    assert_eq!(port, 19_000);
    assert_eq!(pinned_route, pinned);

    daemon.stop().await;
}

#[tokio::test]
async fn local_names_resolve_to_this_machine() {
    let daemon = Daemon::start("hr-local").await;
    let mut client = unix_client(&daemon.socket).await;

    for route in ["local", "", SELF_FQDN] {
        assert_eq!(
            resolution(&exchange(&mut client, route).await),
            HostResolution::Local,
            "route {route:?}"
        );
    }

    daemon.stop().await;
}

#[tokio::test]
async fn unresolvable_routes_fail_with_the_typed_registry_errors() {
    let daemon = Daemon::start("hr-errors").await;
    let mut client = unix_client(&daemon.socket).await;

    assert_eq!(
        error_code(&exchange(&mut client, "nobody").await),
        "host_unknown"
    );
    assert_eq!(
        error_code(&exchange(&mut client, TWIN_NAME).await),
        "overlay_host_ambiguous"
    );
    // A peer without a peer id or an FQDN cannot be pinned.
    assert_eq!(
        error_code(&exchange(&mut client, BARE_NAME).await),
        "host_identity_unpinnable"
    );
    // An explicit port always names a peer, so this machine is not found.
    assert_eq!(
        error_code(&exchange(&mut client, &format!("{SELF_FQDN}@19000")).await),
        "host_unknown"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn the_overlay_listener_does_not_serve_the_method() {
    let daemon = Daemon::start("hr-overlay").await;
    let stream = TcpStream::connect(daemon.tcp).await.expect("tcp connect");
    let mut client = Framed::new(stream, LinesCodec::new());

    assert_eq!(
        error_code(&exchange(&mut client, PEER_NAME).await),
        "method_not_found"
    );

    daemon.stop().await;
}
