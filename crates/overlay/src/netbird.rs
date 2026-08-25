//! `NetBird` implementation of [`OverlayTransport`].
//!
//! Wraps the existing `netbird` crate's parser, range gate, and host
//! resolution. All fail-closed behavior is preserved exactly: only addresses
//! inside the CGNAT range are accepted for bind or dial.

use std::net::{IpAddr, SocketAddr};

use crate::{
    BindAddrError, DiscoveredPeer, OverlayError, OverlayId, OverlayTransport, ResolvedOverlayHost,
};
use netbird::{NetbirdStatus, Peer};

/// `NetBird` overlay transport backed by the `netbird` crate.
#[derive(Debug)]
pub struct NetbirdTransport;

impl NetbirdTransport {
    /// Create a new `NetBird` transport instance.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for NetbirdTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayTransport for NetbirdTransport {
    fn id(&self) -> OverlayId {
        OverlayId("netbird".into())
    }

    fn validate_bind_addr(&self, addr: IpAddr) -> Result<(), BindAddrError> {
        netbird::validate_netbird_bind_addr(addr).map_err(|e| match e {
            netbird::BindAddrError::Forbidden(_) => BindAddrError::Forbidden(addr),
            netbird::BindAddrError::NotNetbird(_) => BindAddrError::NotMember(addr),
        })
    }

    fn resolve_host(&self, host: &str, port: u16) -> Result<ResolvedOverlayHost, OverlayError> {
        let status = run_status_or_err()?;
        let ip = netbird::resolve_host(&status, host).map_err(|err| {
            let _ = err;
            OverlayError::HostUnknown(host.to_owned(), self.id().to_string())
        })?;
        let peer = find_peer_by_ip(&status, &ip);
        Ok(ResolvedOverlayHost {
            overlay: self.id(),
            peer_id: None,
            display_name: peer.as_ref().and_then(|p| short_name(p)),
            fqdn: peer.as_ref().and_then(|p| p.fqdn.clone()),
            addr: SocketAddr::new(ip, port),
        })
    }

    fn discover_peers(&self, port: u16) -> Result<Vec<DiscoveredPeer>, OverlayError> {
        let status = run_status_or_err()?;
        let peers = status
            .peers()
            .iter()
            .filter_map(|peer| {
                let ip = peer.ip().filter(|ip| netbird::is_netbird_ip(*ip))?;
                Some(DiscoveredPeer {
                    overlay: self.id(),
                    peer_id: peer.netbird_ip.clone()?,
                    display_name: short_name(peer),
                    fqdn: peer.fqdn.clone(),
                    addr: Some(SocketAddr::new(ip, port)),
                })
            })
            .collect();
        Ok(peers)
    }
}

fn run_status_or_err() -> Result<NetbirdStatus, OverlayError> {
    let id = "netbird";
    netbird::run_status().map_err(|err| match err {
        netbird::NetbirdError::CliMissing => OverlayError::CliMissing(id.to_owned()),
        other => OverlayError::StateUnavailable(id.to_owned(), other.to_string()),
    })
}

fn find_peer_by_ip<'a>(status: &'a NetbirdStatus, ip: &'a IpAddr) -> Option<&'a Peer> {
    status.peers().iter().find(|p| p.ip().as_ref() == Some(ip))
}

fn short_name(peer: &Peer) -> Option<String> {
    peer.fqdn.as_deref()?.split('.').next().map(str::to_owned)
}

// Rust guideline compliant 2026-06-26
