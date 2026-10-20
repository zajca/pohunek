//! Connecting by this machine's own overlay name, and pinning a host once.
//!
//! A stub overlay provider reports this machine and one peer. The scenarios
//! connect through the production client transport to a local Unix socket
//! daemon and a loopback TCP daemon, so each outcome is the daemon that
//! answered.

// Rust guideline compliant 2026-10-20

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, LocalIdentity,
    OverlayError, OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
    ResolvedSelector,
};
use pohunek_client::protocol::{self, Request};
use pohunek_client::{
    next_request_id, resolve_host_target_in, Client, ClientError, ClientOptions, OriginSource,
    ResolvedHost,
};
use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, UnixListener};

/// This machine's own overlay name.
const SELF_FQDN: &str = "self.overlay.test";

/// The peer's key, name and a name both the peer and this machine carry.
const PEER_KEY: &str = "peer-key";
const PEER_NAME: &str = "peer-b";
const TWIN_NAME: &str = "twin";

/// Provider that knows this machine and one peer on the loopback address.
#[derive(Debug)]
struct StubOverlay {
    id: OverlayId,
    peer: Option<ResolvedPeer>,
}

impl StubOverlay {
    fn peer(identified: bool) -> ResolvedPeer {
        ResolvedPeer {
            peer_id: identified.then(|| PEER_KEY.to_owned()),
            display_name: Some(PEER_NAME.to_owned()),
            fqdn: identified.then(|| format!("{PEER_NAME}.overlay.test")),
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
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
        let result = match &self.peer {
            Some(peer) if host == PEER_NAME || host == TWIN_NAME => Ok(peer.clone()),
            _ => Err(self.unknown(host)),
        };
        Box::pin(async move { result })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        let result = match &self.peer {
            Some(peer) if identity.value() == PEER_KEY => Ok(peer.clone()),
            _ => Err(self.unknown(identity.value())),
        };
        Box::pin(async move { result })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn resolve_selector<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedSelector> {
        let result = if host == SELF_FQDN {
            Ok(ResolvedSelector::LocalHost)
        } else if host == TWIN_NAME {
            Err(OverlayError::PeerCollision {
                host: host.to_owned(),
                overlay: self.id.clone(),
                candidates: Vec::new(),
            })
        } else {
            match &self.peer {
                Some(peer) if host == PEER_NAME => Ok(ResolvedSelector::Peer(peer.clone())),
                _ => Err(self.unknown(host)),
            }
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

fn registry(port: u16, identified_peer: bool) -> OverlayRegistry {
    let transport = StubOverlay {
        id: OverlayId::new("stub").expect("overlay id"),
        peer: Some(StubOverlay::peer(identified_peer)),
    };
    OverlayRegistry::new(vec![
        ConfiguredTransport::new(Arc::new(transport), port).expect("configured transport")
    ])
    .expect("registry")
}

fn options() -> ClientOptions {
    ClientOptions::default().with_origin_source(OriginSource::Omitted)
}

/// Daemon on `listener` that counts the requests it answered.
macro_rules! serve_counting {
    ($listener:expr) => {{
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&counter);
        let listener = $listener;
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let request: Request =
                            serde_json::from_str(&line).expect("a request line");
                        let response = protocol::Response::ok(
                            protocol::PROTOCOL_VERSION,
                            request.id(),
                            json!({
                                "status": "ok",
                                "daemon_version": "fixture",
                                "protocol_version": protocol::PROTOCOL_VERSION.get()
                            }),
                        )
                        .expect("a response");
                        let mut reply =
                            serde_json::to_vec(&response).expect("serialize the response");
                        reply.push(b'\n');
                        writer.write_all(&reply).await.expect("write the reply");
                    }
                });
            }
        });
        counter
    }};
}

fn health() -> Request {
    Request::new(
        next_request_id(protocol::method::DAEMON_HEALTH),
        protocol::method::DAEMON_HEALTH,
        Value::Null,
    )
    .expect("health request")
}

struct Daemons {
    _env: TestEnv,
    socket: std::path::PathBuf,
    local: Arc<std::sync::atomic::AtomicUsize>,
    tcp: Arc<std::sync::atomic::AtomicUsize>,
    port: u16,
}

async fn daemons() -> Daemons {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let socket = env.root().join("d.sock");
    let local = serve_counting!(UnixListener::bind(&socket).expect("bind the socket"));
    let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind the TCP daemon");
    let port = tcp_listener.local_addr().expect("address").port();
    let tcp = serve_counting!(tcp_listener);
    Daemons {
        _env: env,
        socket,
        local,
        tcp,
        port,
    }
}

fn seen(counter: &Arc<std::sync::atomic::AtomicUsize>) -> usize {
    counter.load(std::sync::atomic::Ordering::SeqCst)
}

#[tokio::test]
async fn the_machines_own_overlay_name_reaches_the_local_socket() {
    let daemons = daemons().await;
    let mut client = Client::connect_with_registry_and_options(
        SELF_FQDN,
        &daemons.socket,
        registry(daemons.port, true),
        options(),
    )
    .await
    .expect("connect by the own overlay name");
    client.request(&health()).await.expect("health");
    assert!(seen(&daemons.local) > 0, "the local daemon answered");
    assert_eq!(seen(&daemons.tcp), 0, "no TCP dial for this machine");
}

#[tokio::test]
async fn a_name_this_machine_shares_with_a_peer_is_ambiguous_and_dials_nothing() {
    let daemons = daemons().await;
    let error = Client::connect_with_registry_and_options(
        TWIN_NAME,
        &daemons.socket,
        registry(daemons.port, true),
        options(),
    )
    .await
    .expect_err("an ambiguous name never connects");
    assert_eq!(error.to_protocol_error().code, "overlay_host_ambiguous");
    assert_eq!(seen(&daemons.local) + seen(&daemons.tcp), 0);
}

#[tokio::test]
async fn an_explicit_port_always_selects_a_peer() {
    let daemons = daemons().await;
    let error = Client::connect_with_registry_and_options(
        &format!("{SELF_FQDN}@{}", daemons.port),
        &daemons.socket,
        registry(daemons.port, true),
        options(),
    )
    .await
    .expect_err("this machine is not a peer");
    assert_eq!(error.to_protocol_error().code, "host_unknown");
    assert_eq!(seen(&daemons.local) + seen(&daemons.tcp), 0);
}

#[tokio::test]
async fn a_peer_name_still_reaches_its_overlay_address() {
    let daemons = daemons().await;
    let mut client = Client::connect_with_registry_and_options(
        PEER_NAME,
        &daemons.socket,
        registry(daemons.port, true),
        options(),
    )
    .await
    .expect("connect to the peer");
    client.request(&health()).await.expect("health");
    assert!(seen(&daemons.tcp) > 0);
    assert_eq!(seen(&daemons.local), 0);
}

#[tokio::test]
async fn resolving_a_host_once_pins_its_identity_and_keeps_the_typed_name() {
    let registry = registry(18_722, true);
    let timeout = Duration::from_secs(5);

    let own = resolve_host_target_in(SELF_FQDN, timeout, &registry)
        .await
        .expect("own name");
    assert_eq!(own, ResolvedHost::Local);
    assert_eq!(
        resolve_host_target_in("local", timeout, &registry)
            .await
            .expect("local"),
        ResolvedHost::Local
    );

    let identity = ExternalIdentity::peer_id(PEER_KEY)
        .expect("identity")
        .selector();
    let expected = format!("stub:{identity}@18722");
    match resolve_host_target_in(PEER_NAME, timeout, &registry)
        .await
        .expect("peer name")
    {
        ResolvedHost::Remote {
            pinned_route,
            display,
        } => {
            assert_eq!(pinned_route, expected);
            assert_eq!(display, PEER_NAME);
        }
        other @ ResolvedHost::Local => panic!("expected a remote host, got {other:?}"),
    }
    match resolve_host_target_in(&format!("{PEER_NAME}@19000"), timeout, &registry)
        .await
        .expect("peer name with a port")
    {
        ResolvedHost::Remote { pinned_route, .. } => {
            assert_eq!(pinned_route, format!("stub:{identity}@19000"));
        }
        other @ ResolvedHost::Local => panic!("expected a remote host, got {other:?}"),
    }
}

#[tokio::test]
async fn a_peer_without_an_identity_cannot_be_pinned() {
    let error = resolve_host_target_in(PEER_NAME, Duration::from_secs(5), &registry(18_722, false))
        .await
        .expect_err("no peer id and no FQDN");
    assert!(matches!(error, ClientError::HostIdentityUnpinnable { .. }));
    let protocol_error = error.to_protocol_error();
    assert_eq!(protocol_error.code, "host_identity_unpinnable");
    assert!(protocol_error.msg.contains(PEER_NAME));
}
