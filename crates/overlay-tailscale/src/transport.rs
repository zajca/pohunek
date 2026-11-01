//! Tailscale implementation of the generic overlay contract.

use std::net::IpAddr;
use std::time::Duration;

use overlay::{
    BindAddrError, DiscoveredPeer, ExternalIdentity, LocalIdentity, OverlayError, OverlayFuture,
    OverlayId, OverlaySnapshot, OverlayTransport, ResolvedPeer, ResolvedSelector,
};

use crate::peers::{self, Selected};
use crate::status::{Node, TailscaleError, TailscaleStatus};

/// Stable identifier used by every Tailscale route and wire record.
pub const TAILSCALE_OVERLAY_ID: &str = "tailscale";

/// Why a running Tailscale node still has no listener address.
const MISSING_NODE_ADDRESS: &str = "Tailscale reports no IPv4 node address for this host";

/// Tailscale provider adapter.
#[derive(Debug)]
pub struct TailscaleTransport {
    id: OverlayId,
    status_deadline: Duration,
}

impl TailscaleTransport {
    /// Create the Tailscale provider adapter with the default status deadline.
    #[must_use]
    pub fn new() -> Self {
        Self::with_status_deadline(overlay::DEFAULT_STATUS_DEADLINE)
    }

    /// Create the adapter with `status_deadline` as the longest a status probe
    /// may run.
    #[must_use]
    pub fn with_status_deadline(status_deadline: Duration) -> Self {
        Self {
            id: OverlayId::new(TAILSCALE_OVERLAY_ID)
                .expect("the static Tailscale overlay identifier is valid"),
            status_deadline,
        }
    }

    async fn load_status(&self) -> Result<TailscaleStatus, OverlayError> {
        crate::run_status_async(self.status_deadline)
            .await
            .map_err(|error| map_error(error, &self.id))
    }

    /// Loads the status and refuses one that cannot carry overlay traffic.
    async fn load_usable_status(&self) -> Result<TailscaleStatus, OverlayError> {
        let status = self.load_status().await?;
        match status.unusable_reason() {
            Some(detail) => Err(OverlayError::StateUnavailable {
                overlay: self.id.clone(),
                detail,
            }),
            None => Ok(status),
        }
    }
}

impl Default for TailscaleTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayTransport for TailscaleTransport {
    fn id(&self) -> &OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, addr: IpAddr) -> Result<(), BindAddrError> {
        crate::validate_tailscale_bind_addr(addr).map_err(|error| match error {
            crate::BindAddrError::Forbidden(_) => BindAddrError::Forbidden(addr),
            crate::BindAddrError::NotTailscale(_) => BindAddrError::NotMember(addr),
        })
    }

    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
        Box::pin(async move {
            let status = self.load_status().await?;
            listener_from_status(&status).map_err(|reason| OverlayError::ListenerAddressMissing {
                overlay: self.id.clone(),
                reason: Some(reason),
            })
        })
    }

    fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
        Box::pin(async move {
            let status = self.load_usable_status().await?;
            let peer = peers::resolve_peer(&status, host).map_err(|e| map_error(e, self.id()))?;
            Ok(resolved_peer(peer))
        })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        Box::pin(async move {
            let status = self.load_usable_status().await?;
            let peer = peers::resolve_peer_identity(&status, identity)
                .map_err(|e| map_error(e, self.id()))?;
            Ok(resolved_peer(peer))
        })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        Box::pin(async move {
            let status = self.load_usable_status().await?;
            Ok(discover_from_status(&status))
        })
    }

    fn discover_snapshot(&self) -> OverlayFuture<'_, OverlaySnapshot> {
        Box::pin(async move {
            let status = self.load_usable_status().await?;
            Ok(OverlaySnapshot {
                peers: discover_from_status(&status),
                local: local_identity_from_status(&status, self.id()),
            })
        })
    }

    fn resolve_selector<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedSelector> {
        Box::pin(async move {
            let status = self.load_usable_status().await?;
            match peers::resolve_selector(&status, host).map_err(|e| map_error(e, self.id()))? {
                Selected::LocalHost => Ok(ResolvedSelector::LocalHost),
                Selected::Peer(peer) => Ok(ResolvedSelector::Peer(resolved_peer(peer))),
            }
        })
    }

    fn local_identity(&self) -> OverlayFuture<'_, Option<LocalIdentity>> {
        Box::pin(async move {
            let status = self.load_usable_status().await?;
            Ok(local_identity_from_status(&status, self.id()))
        })
    }
}

fn map_error(error: TailscaleError, id: &OverlayId) -> OverlayError {
    match error {
        TailscaleError::CliMissing { refused } => OverlayError::CliMissing {
            overlay: id.clone(),
            refused,
        },
        TailscaleError::StateUnavailable(detail) | TailscaleError::Parse(detail) => {
            OverlayError::StateUnavailable {
                overlay: id.clone(),
                detail,
            }
        }
        TailscaleError::HostUnknown(host) => OverlayError::HostUnknown {
            host,
            overlay: id.clone(),
        },
        TailscaleError::HostAmbiguous { host, candidates } => OverlayError::PeerCollision {
            host,
            overlay: id.clone(),
            candidates,
        },
    }
}

/// This host's IPv4 node address, only while Tailscale runs on a kernel
/// interface; otherwise the reason there is none.
fn listener_from_status(status: &TailscaleStatus) -> Result<IpAddr, String> {
    if let Some(reason) = status.unusable_reason() {
        return Err(reason);
    }
    status
        .self_node()
        .and_then(|own| {
            own.addresses()
                .find(|address| address.is_ipv4() && crate::is_tailscale_node_addr(*address))
        })
        .ok_or_else(|| MISSING_NODE_ADDRESS.to_owned())
}

/// This machine's identity from its own status: the `MagicDNS` name and node
/// address, never the OS hostname.
fn local_identity_from_status(status: &TailscaleStatus, id: &OverlayId) -> Option<LocalIdentity> {
    let own = status.self_node()?;
    let fqdn = own.fqdn().map(str::to_owned);
    let address = peers::dial_address(own);
    (fqdn.is_some() || address.is_some()).then(|| LocalIdentity {
        overlay: id.clone(),
        fqdn,
        address,
    })
}

fn resolved_peer(peer: &Node) -> ResolvedPeer {
    let address = peers::dial_address(peer)
        .expect("Tailscale peer resolution only returns policy-valid addresses");
    ResolvedPeer {
        peer_id: peer.id().map(str::to_owned),
        display_name: peer.fqdn().and_then(peers::short_name).map(str::to_owned),
        fqdn: peer.fqdn().map(str::to_owned),
        address,
    }
}

/// Selectable peers that have an address to dial. An offline peer is listed
/// without its address so discovery does not probe it.
fn discover_from_status(status: &TailscaleStatus) -> Vec<DiscoveredPeer> {
    let shared = peers::shared_node_addresses(status);
    status
        .peer_nodes()
        .filter(|peer| peers::is_selectable(peer) && peers::dial_address(peer).is_some())
        .map(|peer| DiscoveredPeer {
            peer_id: peer.id().map(str::to_owned),
            display_name: peer.fqdn().and_then(peers::short_name).map(str::to_owned),
            fqdn: peer.fqdn().map(str::to_owned),
            address_ambiguous: peers::lists_any(peer, &shared),
            address: peer
                .online()
                .then(|| peers::dial_address(peer))
                .flatten()
                .filter(|_| !peers::lists_any(peer, &shared)),
        })
        .collect()
}
