//! `NetBird` implementation of the generic overlay contract.

use std::net::IpAddr;
use std::time::Duration;

use overlay::{
    BindAddrError, DiscoveredPeer, ExternalIdentity, LocalIdentity, OverlayError, OverlayFuture,
    OverlayId, OverlaySnapshot, OverlayTransport, ResolvedPeer, ResolvedSelector,
};

use crate::{NetbirdError, NetbirdStatus, Peer};

/// Stable identifier used by every `NetBird` route and wire record.
pub const NETBIRD_OVERLAY_ID: &str = "netbird";

/// `NetBird` provider adapter.
#[derive(Debug)]
pub struct NetbirdTransport {
    id: OverlayId,
    status_deadline: Duration,
}

impl NetbirdTransport {
    /// Create the `NetBird` provider adapter with the default status deadline.
    #[must_use]
    pub fn new() -> Self {
        Self::with_status_deadline(overlay::DEFAULT_STATUS_DEADLINE)
    }

    /// Create the adapter with `status_deadline` as the longest a status probe
    /// may run.
    #[must_use]
    pub fn with_status_deadline(status_deadline: Duration) -> Self {
        Self {
            id: OverlayId::new(NETBIRD_OVERLAY_ID)
                .expect("the static NetBird overlay identifier is valid"),
            status_deadline,
        }
    }

    async fn load_status(&self) -> Result<NetbirdStatus, OverlayError> {
        crate::run_status_async(self.status_deadline)
            .await
            .map_err(|error| map_error(error, &self.id))
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
            let status = self.load_status().await?;
            status
                .self_netbird_ip()
                .ok_or_else(|| OverlayError::ListenerAddressMissing {
                    overlay: self.id.clone(),
                    reason: None,
                })
        })
    }

    fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
        Box::pin(async move {
            let status = self.load_status().await?;
            resolve_from_status(&status, host, self.id())
        })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        Box::pin(async move {
            let status = self.load_status().await?;
            resolve_identity_from_status(&status, identity, self.id())
        })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        Box::pin(async move {
            let status = self.load_status().await?;
            Ok(discover_from_status(&status))
        })
    }

    fn discover_snapshot(&self) -> OverlayFuture<'_, OverlaySnapshot> {
        Box::pin(async move {
            let status = self.load_status().await?;
            Ok(OverlaySnapshot {
                peers: discover_from_status(&status),
                local: local_identity_from_status(&status, self.id()),
            })
        })
    }

    fn resolve_selector<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedSelector> {
        Box::pin(async move {
            let status = self.load_status().await?;
            selector_from_status(&status, host, self.id())
        })
    }

    fn local_identity(&self) -> OverlayFuture<'_, Option<LocalIdentity>> {
        Box::pin(async move {
            let status = self.load_status().await?;
            Ok(local_identity_from_status(&status, self.id()))
        })
    }
}

fn map_error(error: NetbirdError, id: &OverlayId) -> OverlayError {
    match error {
        NetbirdError::CliMissing => OverlayError::CliMissing(id.clone()),
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
        NetbirdError::HostAmbiguous { host, candidates } => OverlayError::PeerCollision {
            host,
            overlay: id.clone(),
            candidates,
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

fn selector_from_status(
    status: &NetbirdStatus,
    host: &str,
    id: &OverlayId,
) -> Result<ResolvedSelector, OverlayError> {
    match crate::host::resolve_selector(status, host).map_err(|error| map_error(error, id))? {
        crate::host::Selected::LocalHost => Ok(ResolvedSelector::LocalHost),
        crate::host::Selected::Peer(peer) => Ok(ResolvedSelector::Peer(resolved_peer(peer))),
    }
}

/// This machine's identity from its own status: the root fqdn and the
/// `NetBird` IP, never the OS hostname.
fn local_identity_from_status(status: &NetbirdStatus, id: &OverlayId) -> Option<LocalIdentity> {
    let fqdn = status.self_fqdn().map(str::to_owned);
    let address = status.self_netbird_ip();
    (fqdn.is_some() || address.is_some()).then(|| LocalIdentity {
        overlay: id.clone(),
        fqdn,
        address,
    })
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
    let mut peers = status.peers().iter().map(map_peer).collect::<Vec<_>>();
    overlay::withhold_shared_addresses(&mut peers);
    // A peer that lists this machine's own address is no route either.
    let own = status.self_netbird_ip();
    for peer in &mut peers {
        if own.is_some() && peer.address == own {
            peer.address = None;
            peer.address_ambiguous = true;
        }
    }
    peers
}

fn map_peer(peer: &Peer) -> DiscoveredPeer {
    let parsed = peer.ip();
    DiscoveredPeer {
        peer_id: peer.peer_id().map(str::to_owned),
        display_name: short_name(peer),
        fqdn: peer.fqdn.clone(),
        address: parsed.filter(|address| crate::is_netbird_ip(*address)),
        address_ambiguous: false,
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
