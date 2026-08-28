use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::sync::Arc;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
    OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, RegistryError, ResolvedPeer,
};

#[derive(Debug)]
struct MemoryTransport {
    id: OverlayId,
    listener: Result<IpAddr, OverlayError>,
    members: Vec<IpAddr>,
    peers: Vec<DiscoveredPeer>,
    resolutions: HashMap<String, Result<ResolvedPeer, OverlayError>>,
}

impl MemoryTransport {
    fn new(id: &str, listener: IpAddr) -> Self {
        Self {
            id: OverlayId::new(id).expect("test overlay id"),
            listener: Ok(listener),
            members: vec![listener],
            peers: Vec::new(),
            resolutions: HashMap::new(),
        }
    }

    fn unavailable(id: &str) -> Self {
        let id = OverlayId::new(id).expect("test overlay id");
        Self {
            listener: Err(OverlayError::StateUnavailable {
                overlay: id.clone(),
                detail: "in-memory state unavailable".to_owned(),
            }),
            id,
            members: Vec::new(),
            peers: Vec::new(),
            resolutions: HashMap::new(),
        }
    }

    fn add_member(&mut self, address: IpAddr) {
        self.members.push(address);
    }

    fn add_peer(&mut self, host: &str, peer_id: Option<&str>, advertised: Option<IpAddr>) {
        let safe_address = advertised.filter(|address| self.members.contains(address));
        self.peers.push(DiscoveredPeer {
            peer_id: peer_id.map(str::to_owned),
            display_name: Some(host.to_owned()),
            fqdn: Some(format!("{host}.example")),
            address: safe_address,
        });
        if let Some(address) = safe_address {
            self.resolutions.insert(
                host.to_owned(),
                Ok(ResolvedPeer {
                    peer_id: peer_id.map(str::to_owned),
                    display_name: Some(host.to_owned()),
                    fqdn: Some(format!("{host}.example")),
                    address,
                }),
            );
        }
    }

    fn add_collision(&mut self, host: &str) {
        self.resolutions.insert(
            host.to_owned(),
            Err(OverlayError::PeerCollision {
                host: host.to_owned(),
                overlay: self.id.clone(),
            }),
        );
    }
}

impl OverlayTransport for MemoryTransport {
    fn id(&self) -> &OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, addr: IpAddr) -> Result<(), BindAddrError> {
        if addr.is_unspecified() {
            return Err(BindAddrError::Forbidden(addr));
        }
        if self.members.contains(&addr) {
            Ok(())
        } else {
            Err(BindAddrError::NotMember(addr))
        }
    }

    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
        let listener = self.listener.clone();
        Box::pin(async move { listener })
    }

    fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
        let result = self.resolutions.get(host).cloned().unwrap_or_else(|| {
            self.listener.clone().and_then(|_| {
                Err(OverlayError::HostUnknown {
                    host: host.to_owned(),
                    overlay: self.id.clone(),
                })
            })
        });
        Box::pin(async move { result })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        let result = self.listener.clone().map(|_| self.peers.clone());
        Box::pin(async move { result })
    }
}

fn configured(transport: MemoryTransport, port: u16) -> ConfiguredTransport {
    ConfiguredTransport::new(Arc::new(transport), port).expect("configured transport")
}

#[tokio::test]
async fn contract_preserves_missing_identity_and_rejects_spoofed_address() {
    let listener = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let safe = "100.64.0.2".parse().expect("safe address");
    let spoofed = "203.0.113.9".parse().expect("spoofed address");
    let mut transport = MemoryTransport::new("memory-a", listener);
    transport.add_member(safe);
    transport.add_peer("safe", Some("peer-safe"), Some(safe));
    transport.add_peer("missing-id", None, None);
    transport.add_peer("spoofed", Some("peer-spoofed"), Some(spoofed));
    let entry = configured(transport, 17421);

    let peers = entry.discover_peers().await.expect("discovery");
    assert_eq!(peers.len(), 3);
    assert_eq!(peers[0].addr, Some(SocketAddr::new(safe, 17421)));
    assert_eq!(peers[1].peer_id, None);
    assert_eq!(peers[1].addr, None);
    assert_eq!(peers[2].peer_id.as_deref(), Some("peer-spoofed"));
    assert_eq!(peers[2].addr, None);
}

#[tokio::test]
async fn registry_isolates_failure_but_rejects_name_collisions() {
    let listener = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let healthy_address = "100.64.0.2".parse().expect("healthy address");
    let mut healthy = MemoryTransport::new("healthy", listener);
    healthy.add_member(healthy_address);
    healthy.add_peer("build", Some("healthy-build"), Some(healthy_address));
    let registry = OverlayRegistry::new(vec![
        configured(MemoryTransport::unavailable("broken"), 17001),
        configured(healthy, 17002),
    ])
    .expect("registry");

    let route = registry
        .resolve_host("build")
        .await
        .expect("healthy overlay wins");
    assert_eq!(route.overlay.as_str(), "healthy");
    assert_eq!(route.addr, SocketAddr::new(healthy_address, 17002));

    let mut collision = MemoryTransport::new("collision", listener);
    collision.add_collision("build");
    let registry = OverlayRegistry::new(vec![
        configured(collision, 17003),
        registry.entries()[1].clone(),
    ])
    .expect("collision registry");
    assert!(matches!(
        registry.resolve_host("build").await,
        Err(RegistryError::AmbiguousHost { overlays, .. })
            if overlays.iter().any(|id| id.as_str() == "collision")
                && overlays.iter().any(|id| id.as_str() == "healthy")
    ));
}

#[tokio::test]
async fn qualified_host_uses_only_named_overlay_and_its_port() {
    let listener = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let address = "100.64.0.2".parse().expect("peer address");
    let mut first = MemoryTransport::new("first", listener);
    first.add_member(address);
    first.add_peer("build", Some("first-build"), Some(address));
    let mut second = MemoryTransport::new("second", listener);
    second.add_member(address);
    second.add_peer("build", Some("second-build"), Some(address));
    let registry = OverlayRegistry::new(vec![configured(first, 17001), configured(second, 17002)])
        .expect("registry");

    let route = registry
        .resolve_host("second:build")
        .await
        .expect("qualified host");
    assert_eq!(route.overlay.as_str(), "second");
    assert_eq!(route.peer_id.as_deref(), Some("second-build"));
    assert_eq!(route.addr, SocketAddr::new(address, 17002));
    assert!(matches!(
        registry.resolve_host("missing:build").await,
        Err(RegistryError::OverlayNotConfigured(id)) if id.as_str() == "missing"
    ));
}

#[tokio::test]
async fn canonical_identity_is_slash_safe_and_decoded_before_provider_resolution() {
    let listener = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let address = "100.64.0.2".parse().expect("peer address");
    let raw_peer_id = "real/key+with=padding@reserved";
    let mut transport = MemoryTransport::new("memory", listener);
    transport.add_member(address);
    transport.add_peer(raw_peer_id, Some(raw_peer_id), Some(address));
    let registry = OverlayRegistry::new(vec![configured(transport, 17001)]).expect("registry");
    let external = ExternalIdentity::peer_id(raw_peer_id).expect("external identity");
    let selector = external.selector();

    assert!(!selector.contains(['/', '+', '=', '@']));
    let route = registry
        .resolve_host(&format!("memory:{selector}"))
        .await
        .expect("canonical identity route");
    assert_eq!(route.peer_id.as_deref(), Some(raw_peer_id));
    assert_eq!(route.addr, SocketAddr::new(address, 17001));
    assert!(matches!(
        registry.resolve_host("memory:peer~YWJ=").await,
        Err(RegistryError::InvalidExternalIdentity { .. })
    ));
}

#[tokio::test]
async fn ipv6_literals_distinguish_unqualified_peers_qualifiers_and_socket_addresses() {
    let listener = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let address = "fd00::2".parse().expect("peer address");
    let mut unqualified = MemoryTransport::new("first", listener);
    unqualified.add_member(address);
    unqualified.add_peer("fd00::2", Some("first-v6"), Some(address));
    let registry = OverlayRegistry::new(vec![configured(unqualified, 17001)]).expect("registry");

    let route = registry
        .resolve_host("fd00::2")
        .await
        .expect("unqualified IPv6 peer");
    assert_eq!(route.overlay.as_str(), "first");
    assert_eq!(route.addr, SocketAddr::new(address, 17001));

    let mut first = MemoryTransport::new("first", listener);
    first.add_member(address);
    first.add_peer("fd00::2", Some("first-v6"), Some(address));
    let mut second = MemoryTransport::new("second", listener);
    second.add_member(address);
    second.add_peer("fd00::2", Some("second-v6"), Some(address));
    let registry = OverlayRegistry::new(vec![configured(first, 17001), configured(second, 17002)])
        .expect("qualified registry");

    assert!(matches!(
        registry.resolve_host("fd00::2").await,
        Err(RegistryError::AmbiguousHost { host, overlays })
            if host == "fd00::2"
                && overlays.iter().map(OverlayId::as_str).eq(["first", "second"])
    ));

    let route = registry
        .resolve_host("second:fd00::2")
        .await
        .expect("qualified IPv6 peer");
    assert_eq!(route.overlay.as_str(), "second");
    assert_eq!(route.addr, SocketAddr::new(address, 17002));
    assert!(matches!(
        registry.resolve_host("[fd00::2]:17002").await,
        Err(RegistryError::InvalidQualifiedHost(host)) if host == "[fd00::2]:17002"
    ));
}

#[test]
fn registry_rejects_duplicate_ids_and_zero_ports() {
    let listener = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let first = configured(MemoryTransport::new("duplicate", listener), 17001);
    let second = configured(MemoryTransport::new("duplicate", listener), 17002);
    assert!(matches!(
        OverlayRegistry::new(vec![first, second]),
        Err(RegistryError::DuplicateId(id)) if id.as_str() == "duplicate"
    ));

    let transport = Arc::new(MemoryTransport::new("zero-port", listener));
    assert!(matches!(
        ConfiguredTransport::new(transport, 0),
        Err(RegistryError::InvalidPort(id)) if id.as_str() == "zero-port"
    ));
}

#[tokio::test]
async fn diagnostics_and_concurrent_listeners_keep_per_overlay_ports() {
    let listener = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let first_socket = TcpListener::bind(SocketAddr::new(listener, 0)).expect("first listener");
    let first_port = first_socket.local_addr().expect("first address").port();
    let second_socket = TcpListener::bind(SocketAddr::new(listener, 0)).expect("second listener");
    let second_port = second_socket.local_addr().expect("second address").port();
    assert_ne!(first_port, second_port);

    let registry = OverlayRegistry::new(vec![
        configured(MemoryTransport::new("memory-a", listener), first_port),
        configured(MemoryTransport::new("memory-b", listener), second_port),
    ])
    .expect("registry");
    let diagnostics = registry.diagnostics().await;
    assert_eq!(
        diagnostics[0].listener,
        Some(SocketAddr::new(listener, first_port))
    );
    assert_eq!(
        diagnostics[1].listener,
        Some(SocketAddr::new(listener, second_port))
    );

    drop((first_socket, second_socket));
}

// Rust guideline compliant 2026-08-28
