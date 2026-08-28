use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, OverlayError, OverlayId, OverlayRegistry,
    OverlayTransport, RegistryError, ResolvedPeer,
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

    fn listener_addr(&self) -> Result<IpAddr, OverlayError> {
        self.listener.clone()
    }

    fn resolve_peer(&self, host: &str) -> Result<ResolvedPeer, OverlayError> {
        self.resolutions.get(host).cloned().unwrap_or_else(|| {
            self.listener.clone().and_then(|_| {
                Err(OverlayError::HostUnknown {
                    host: host.to_owned(),
                    overlay: self.id.clone(),
                })
            })
        })
    }

    fn discover_peers(&self) -> Result<Vec<DiscoveredPeer>, OverlayError> {
        self.listener.clone().map(|_| self.peers.clone())
    }
}

fn configured(transport: MemoryTransport, port: u16) -> ConfiguredTransport {
    ConfiguredTransport::new(Arc::new(transport), port).expect("configured transport")
}

#[test]
fn contract_preserves_missing_identity_and_rejects_spoofed_address() {
    let listener = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let safe = "100.64.0.2".parse().expect("safe address");
    let spoofed = "203.0.113.9".parse().expect("spoofed address");
    let mut transport = MemoryTransport::new("memory-a", listener);
    transport.add_member(safe);
    transport.add_peer("safe", Some("peer-safe"), Some(safe));
    transport.add_peer("missing-id", None, None);
    transport.add_peer("spoofed", Some("peer-spoofed"), Some(spoofed));
    let entry = configured(transport, 17421);

    let peers = entry.discover_peers().expect("discovery");
    assert_eq!(peers.len(), 3);
    assert_eq!(peers[0].addr, Some(SocketAddr::new(safe, 17421)));
    assert_eq!(peers[1].peer_id, None);
    assert_eq!(peers[1].addr, None);
    assert_eq!(peers[2].peer_id.as_deref(), Some("peer-spoofed"));
    assert_eq!(peers[2].addr, None);
}

#[test]
fn registry_isolates_failure_but_rejects_name_collisions() {
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
        registry.resolve_host("build"),
        Err(RegistryError::AmbiguousHost { overlays, .. })
            if overlays.iter().any(|id| id.as_str() == "collision")
                && overlays.iter().any(|id| id.as_str() == "healthy")
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

#[test]
fn diagnostics_and_concurrent_listeners_keep_per_overlay_ports() {
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
    let diagnostics = registry.diagnostics();
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
