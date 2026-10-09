//! `NetBird` implementation of the generic overlay contract.

use std::net::IpAddr;
use std::sync::Arc;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
    OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
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

    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
        Box::pin(async move {
            let status = load_status(self.id()).await?;
            status
                .self_netbird_ip()
                .ok_or_else(|| OverlayError::ListenerAddressMissing(self.id.clone()))
        })
    }

    fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
        Box::pin(async move {
            let status = load_status(self.id()).await?;
            resolve_from_status(&status, host, self.id())
        })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        Box::pin(async move {
            let status = load_status(self.id()).await?;
            resolve_identity_from_status(&status, identity, self.id())
        })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        Box::pin(async move {
            let status = load_status(self.id()).await?;
            Ok(discover_from_status(&status))
        })
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

async fn load_status(id: &OverlayId) -> Result<NetbirdStatus, OverlayError> {
    crate::run_status_async()
        .await
        .map_err(|error| map_error(error, id))
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
    let peer = crate::host::resolve_peer(status, host).map_err(|error| map_error(error, id))?;
    Ok(resolved_peer(peer))
}

fn resolve_identity_from_status(
    status: &NetbirdStatus,
    identity: &ExternalIdentity,
    id: &OverlayId,
) -> Result<ResolvedPeer, OverlayError> {
    let peer = crate::host::resolve_peer_identity(status, identity)
        .map_err(|error| map_error(error, id))?;
    Ok(resolved_peer(peer))
}

fn resolved_peer(peer: &Peer) -> ResolvedPeer {
    let address = peer
        .ip()
        .expect("NetBird peer resolution only returns policy-valid addresses");
    ResolvedPeer {
        peer_id: peer.peer_id().map(str::to_owned),
        display_name: short_name(peer),
        fqdn: peer.fqdn.clone(),
        address,
    }
}

fn discover_from_status(status: &NetbirdStatus) -> Vec<DiscoveredPeer> {
    status.peers().iter().map(map_peer).collect()
}

fn map_peer(peer: &Peer) -> DiscoveredPeer {
    let parsed = peer.ip();
    DiscoveredPeer {
        peer_id: peer.peer_id().map(str::to_owned),
        display_name: short_name(peer),
        fqdn: peer.fqdn.clone(),
        address: parsed.filter(|address| crate::is_netbird_ip(*address)),
    }
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
