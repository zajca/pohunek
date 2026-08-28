//! `NetBird` implementation of the generic overlay contract.

use std::net::IpAddr;
use std::sync::Arc;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, OverlayError, OverlayId, OverlayRegistry,
    OverlayTransport, ResolvedPeer,
};

use crate::{NetbirdError, NetbirdStatus, Peer};

/// Stable identifier used by every `NetBird` route and wire record.
pub const NETBIRD_OVERLAY_ID: &str = "netbird";

/// `NetBird` provider adapter.
#[derive(Debug)]
pub struct NetbirdTransport {
    id: OverlayId,
}

impl NetbirdTransport {
    /// Create the `NetBird` provider adapter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: OverlayId::new(NETBIRD_OVERLAY_ID)
                .expect("the static NetBird overlay identifier is valid"),
        }
    }
}

impl Default for NetbirdTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayTransport for NetbirdTransport {
    fn id(&self) -> &OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, addr: IpAddr) -> Result<(), BindAddrError> {
        crate::validate_netbird_bind_addr(addr).map_err(|error| match error {
            crate::BindAddrError::Forbidden(_) => BindAddrError::Forbidden(addr),
            crate::BindAddrError::NotNetbird(_) => BindAddrError::NotMember(addr),
        })
    }

    fn listener_addr(&self) -> Result<IpAddr, OverlayError> {
        let status = load_status(self.id())?;
        status
            .self_netbird_ip()
            .ok_or_else(|| OverlayError::ListenerAddressMissing(self.id.clone()))
    }

    fn resolve_peer(&self, host: &str) -> Result<ResolvedPeer, OverlayError> {
        let status = load_status(self.id())?;
        resolve_from_status(&status, host, self.id())
    }

    fn discover_peers(&self) -> Result<Vec<DiscoveredPeer>, OverlayError> {
        let status = load_status(self.id())?;
        Ok(discover_from_status(&status))
    }
}

/// Build the production registry from fail-fast `NetBird` configuration.
///
/// # Errors
///
/// Returns [`OverlayError::InvalidConfig`] for an invalid remote port or an
/// impossible registry construction failure.
pub fn configured_registry() -> Result<OverlayRegistry, OverlayError> {
    let transport = Arc::new(NetbirdTransport::new());
    let id = transport.id().clone();
    let port = crate::remote_port().map_err(|error| map_error(error, &id))?;
    let entry =
        ConfiguredTransport::new(transport, port).map_err(|error| OverlayError::InvalidConfig {
            overlay: id.clone(),
            detail: error.to_string(),
        })?;
    OverlayRegistry::new(vec![entry]).map_err(|error| OverlayError::InvalidConfig {
        overlay: id,
        detail: error.to_string(),
    })
}

fn load_status(id: &OverlayId) -> Result<NetbirdStatus, OverlayError> {
    crate::run_status().map_err(|error| map_error(error, id))
}

fn map_error(error: NetbirdError, id: &OverlayId) -> OverlayError {
    match error {
        NetbirdError::CliMissing => OverlayError::CliMissing(id.clone()),
        NetbirdError::InvalidConfig(detail) => OverlayError::InvalidConfig {
            overlay: id.clone(),
            detail,
        },
        NetbirdError::StateUnavailable(detail) | NetbirdError::Parse(detail) => {
            OverlayError::StateUnavailable {
                overlay: id.clone(),
                detail,
            }
        }
        NetbirdError::HostUnknown(host) => OverlayError::HostUnknown {
            host,
            overlay: id.clone(),
        },
        NetbirdError::HostAmbiguous(host) => OverlayError::PeerCollision {
            host,
            overlay: id.clone(),
        },
    }
}

fn resolve_from_status(
    status: &NetbirdStatus,
    host: &str,
    id: &OverlayId,
) -> Result<ResolvedPeer, OverlayError> {
    let address = crate::resolve_host(status, host).map_err(|error| map_error(error, id))?;
    let peer = find_peer_by_ip(status, address);
    Ok(ResolvedPeer {
        peer_id: Some(address.to_string()),
        display_name: peer.and_then(short_name),
        fqdn: peer.and_then(|peer| peer.fqdn.clone()),
        address,
    })
}

fn discover_from_status(status: &NetbirdStatus) -> Vec<DiscoveredPeer> {
    let mut peers = Vec::with_capacity(status.peers().len() + 1);
    if let Some(address) = status.self_netbird_ip() {
        peers.push(DiscoveredPeer {
            peer_id: Some(address.to_string()),
            display_name: status.self_fqdn().and_then(short_fqdn),
            fqdn: status.self_fqdn().map(str::to_owned),
            address: Some(address),
        });
    }
    peers.extend(status.peers().iter().map(map_peer));
    peers
}

fn map_peer(peer: &Peer) -> DiscoveredPeer {
    let parsed = peer.ip();
    DiscoveredPeer {
        peer_id: parsed.map(|address| address.to_string()),
        display_name: short_name(peer),
        fqdn: peer.fqdn.clone(),
        address: parsed.filter(|address| crate::is_netbird_ip(*address)),
    }
}

fn find_peer_by_ip(status: &NetbirdStatus, address: IpAddr) -> Option<&Peer> {
    status
        .peers()
        .iter()
        .find(|peer| peer.ip() == Some(address))
}

fn short_name(peer: &Peer) -> Option<String> {
    peer.fqdn.as_deref().and_then(short_fqdn)
}

fn short_fqdn(fqdn: &str) -> Option<String> {
    fqdn.split('.')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(json: &str) -> NetbirdStatus {
        crate::parse_status(json).expect("status fixture")
    }

    #[test]
    fn discovery_preserves_missing_identity_and_rejects_spoofed_addresses() {
        let status = status(
            r#"{
                "netbirdIp":"100.64.0.1",
                "fqdn":"self.example",
                "peers":[
                    {"fqdn":"safe.example","netbirdIp":"100.64.0.2"},
                    {"fqdn":"missing.example"},
                    {"fqdn":"spoofed.example","netbirdIp":"127.0.0.1"}
                ]
            }"#,
        );

        let peers = discover_from_status(&status);
        assert_eq!(peers.len(), 4);
        assert_eq!(peers[1].peer_id.as_deref(), Some("100.64.0.2"));
        assert_eq!(peers[1].address, Some("100.64.0.2".parse().expect("safe")));
        assert_eq!(peers[2].peer_id, None);
        assert_eq!(peers[2].address, None);
        assert_eq!(peers[3].peer_id.as_deref(), Some("127.0.0.1"));
        assert_eq!(peers[3].address, None);
    }

    #[test]
    fn resolver_preserves_typed_unknown_error() {
        let id = OverlayId::new(NETBIRD_OVERLAY_ID).expect("id");
        let error = resolve_from_status(&status(r#"{"peers":[]}"#), "missing", &id)
            .expect_err("unknown host");
        assert!(matches!(
            error,
            OverlayError::HostUnknown { host, overlay }
                if host == "missing" && overlay == id
        ));
    }
}

// Rust guideline compliant 2026-08-28
