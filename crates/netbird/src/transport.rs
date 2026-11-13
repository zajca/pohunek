//! `NetBird` implementation of the generic overlay contract.

use std::net::IpAddr;
use std::time::Duration;

use overlay::{
    BindAddrError, DiscoveredPeer, ExternalIdentity, ExternalIdentityKind, LocalIdentity,
    OverlayError, OverlayFuture, OverlayId, OverlaySnapshot, OverlayTransport, ResolvedPeer,
    ResolvedSelector,
};

use crate::{NetbirdError, NetbirdStatus, Peer, StaticPeers};

/// Stable identifier used by every `NetBird` route and wire record.
pub const NETBIRD_OVERLAY_ID: &str = "netbird";

/// `NetBird` provider adapter.
#[derive(Debug)]
pub struct NetbirdTransport {
    id: OverlayId,
    status_deadline: Duration,
    static_peers: Option<StaticPeers>,
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
            static_peers: None,
        }
    }

    /// Create the adapter with `status_deadline` and owner-configured static
    /// peers. The peers are used only while NetBird withholds its status.
    #[must_use]
    pub fn with_static_peers(status_deadline: Duration, static_peers: Option<StaticPeers>) -> Self {
        Self {
            id: OverlayId::new(NETBIRD_OVERLAY_ID)
                .expect("the static NetBird overlay identifier is valid"),
            status_deadline,
            static_peers,
        }
    }

    async fn load_status(&self) -> Result<NetbirdStatus, OverlayError> {
        crate::run_status_async(self.status_deadline)
            .await
            .map_err(|error| map_error(error, &self.id))
    }

    async fn load_view(&self) -> Result<View, OverlayError> {
        match crate::run_status_async(self.status_deadline).await {
            Ok(status) => Ok(View::Full(status)),
            Err(NetbirdError::DetailRestricted { daemon_status }) => {
                Ok(View::Restricted { daemon_status })
            }
            Err(error) => Err(map_error(error, &self.id)),
        }
    }
}

enum View {
    Full(NetbirdStatus),
    Restricted { daemon_status: String },
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
            match self.load_view().await? {
                View::Full(status) => {
                    self.cross_check(&status)?;
                    resolve_from_status(&status, host, self.id())
                }
                View::Restricted { daemon_status } => self.static_peer(host, &daemon_status),
            }
        })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        Box::pin(async move {
            match self.load_view().await? {
                View::Full(status) => {
                    self.cross_check(&status)?;
                    resolve_identity_from_status(&status, identity, self.id())
                }
                View::Restricted { daemon_status } => {
                    let restricted = restricted_error(&daemon_status, self.id());
                    if !connected_restricted(&daemon_status) || self.static_peers.is_none() {
                        return Err(restricted);
                    }
                    match identity.kind() {
                        ExternalIdentityKind::Fqdn => self
                            .static_peers
                            .as_ref()
                            .and_then(|peers| peers.match_fqdn(identity.value()))
                            .map(static_peer)
                            .or_else(|| None)
                            .ok_or(restricted),
                        ExternalIdentityKind::PeerId => Err(restricted),
                    }
                }
            }
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
            match self.load_view().await? {
                View::Full(status) => {
                    self.cross_check(&status)?;
                    selector_from_status(&status, host, self.id())
                }
                View::Restricted { daemon_status } => Ok(ResolvedSelector::Peer(
                    self.static_peer(host, &daemon_status)?,
                )),
            }
        })
    }

    fn local_identity(&self) -> OverlayFuture<'_, Option<LocalIdentity>> {
        Box::pin(async move {
            let status = self.load_status().await?;
            Ok(local_identity_from_status(&status, self.id()))
        })
    }
}

impl NetbirdTransport {
    fn static_peer(&self, host: &str, daemon_status: &str) -> Result<ResolvedPeer, OverlayError> {
        if !connected_restricted(daemon_status) || self.static_peers.is_none() {
            return Err(restricted_error(daemon_status, self.id()));
        }
        self.static_peers
            .as_ref()
            .and_then(|peers| peers.match_selector(host))
            .map(static_peer)
            .ok_or_else(|| restricted_error(daemon_status, self.id()))
    }

    fn cross_check(&self, status: &NetbirdStatus) -> Result<(), OverlayError> {
        let Some(peers) = &self.static_peers else {
            return Ok(());
        };
        let invalid = |detail: String| OverlayError::InvalidConfig {
            overlay: self.id.clone(),
            detail,
        };
        if let Some(own) = status.self_netbird_ip() {
            if !peers.contains(own) {
                return Err(invalid(
                    "this host's NetBird address is outside netbird.subnet".to_owned(),
                ));
            }
        }
        for entry in peers.peers() {
            let entry_ip = IpAddr::V4(entry.address);
            if status.self_netbird_ip() == Some(entry_ip) {
                return Err(invalid(format!(
                    "netbird.static_peers.{} lists this host's own NetBird address",
                    entry.name
                )));
            }
            for peer in status.peers() {
                let peer_ip = peer.ip();
                let peer_fqdn = peer.fqdn.as_deref();
                if peer_fqdn.is_some_and(|fqdn| fqdn.eq_ignore_ascii_case(&entry.fqdn))
                    && peer_ip.is_some_and(|ip| ip != entry_ip)
                {
                    return Err(invalid(format!(
                        "netbird.static_peers.{}: NetBird status lists its fqdn at another address",
                        entry.name
                    )));
                }
                if peer_ip == Some(entry_ip)
                    && peer_fqdn.is_some_and(|fqdn| !fqdn.eq_ignore_ascii_case(&entry.fqdn))
                {
                    return Err(invalid(format!(
                        "netbird.static_peers.{}: NetBird status gives its address to another peer",
                        entry.name
                    )));
                }
            }
        }
        Ok(())
    }
}

fn connected_restricted(daemon_status: &str) -> bool {
    daemon_status.trim().eq_ignore_ascii_case("Connected")
}

fn restricted_error(daemon_status: &str, id: &OverlayId) -> OverlayError {
    map_error(
        NetbirdError::DetailRestricted {
            daemon_status: daemon_status.to_owned(),
        },
        id,
    )
}

fn static_peer(entry: &crate::StaticPeer) -> ResolvedPeer {
    ResolvedPeer {
        peer_id: None,
        display_name: Some(entry.name.clone()),
        fqdn: Some(entry.fqdn.clone()),
        address: IpAddr::V4(entry.address),
        expected_host_id: Some(entry.host_id.clone()),
    }
}

fn map_error(error: NetbirdError, id: &OverlayId) -> OverlayError {
    match error {
        NetbirdError::CliMissing { refused } => OverlayError::CliMissing {
            overlay: id.clone(),
            refused,
        },
        NetbirdError::StateUnavailable(detail) | NetbirdError::Parse(detail) => {
            OverlayError::StateUnavailable {
                overlay: id.clone(),
                detail,
            }
        }
        NetbirdError::DetailRestricted { daemon_status } => OverlayError::StateRestricted {
            overlay: id.clone(),
            detail: format!(
                "this account does not own the active NetBird profile (daemon status: {daemon_status})"
            ),
        },
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
        expected_host_id: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_status;
    use std::net::Ipv4Addr;

    fn transport() -> NetbirdTransport {
        NetbirdTransport::with_static_peers(
            overlay::DEFAULT_STATUS_DEADLINE,
            Some(StaticPeers::new(
                Ipv4Addr::new(100, 64, 0, 0),
                10,
                vec![crate::StaticPeer {
                    name: "vps2".to_owned(),
                    fqdn: "vps2.remote-box.netbird.test".to_owned(),
                    address: Ipv4Addr::new(100, 64, 0, 21),
                    host_id: "host_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
                }],
            )),
        )
    }

    #[test]
    fn a_full_status_wins_over_the_table() {
        let status = parse_status(
            r#"{"netbirdIp":"100.64.0.7","peers":{"details":[
                {"fqdn":"vps2.remote-box.netbird.test","netbirdIp":"100.64.0.21","status":"Connected"}
            ]}}"#,
        )
        .expect("status parses");
        let resolved = resolve_from_status(&status, "vps2", &transport().id)
            .expect("the status peer resolves");
        assert_eq!(resolved.address, IpAddr::V4(Ipv4Addr::new(100, 64, 0, 21)));
        assert!(
            resolved.expected_host_id.is_none(),
            "status peers carry no pin"
        );
    }

    #[test]
    fn a_contradicting_table_fails_with_invalid_config() {
        let status = parse_status(
            r#"{"netbirdIp":"100.64.0.7","peers":{"details":[
                {"fqdn":"other.remote-box.netbird.test","netbirdIp":"100.64.0.21","status":"Connected"}
            ]}}"#,
        )
        .expect("status parses");
        let error = transport()
            .cross_check(&status)
            .expect_err("a contradicting table is refused");
        assert!(
            matches!(error, OverlayError::InvalidConfig { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_table_listing_this_hosts_address_fails_with_invalid_config() {
        let status = parse_status(r#"{"netbirdIp":"100.64.0.21","peers":{"details":[]}}"#)
            .expect("status parses");
        let error = transport()
            .cross_check(&status)
            .expect_err("the own address is refused");
        assert!(
            matches!(error, OverlayError::InvalidConfig { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_status_result_carries_the_table_address_and_pin_when_restricted() {
        let transport = transport();
        let peer = transport
            .static_peer("vps2", "Connected")
            .expect("the static peer resolves");
        assert_eq!(peer.address, IpAddr::V4(Ipv4Addr::new(100, 64, 0, 21)));
        assert_eq!(peer.fqdn.as_deref(), Some("vps2.remote-box.netbird.test"));
        assert_eq!(
            peer.expected_host_id.as_deref(),
            Some("host_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        );
        assert!(transport.static_peer("nobody", "Connected").is_err());
        assert!(transport.static_peer("vps2", "Connecting").is_err());
    }
}
