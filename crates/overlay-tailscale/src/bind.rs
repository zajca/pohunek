//! Validation of the daemon control-listener bind address.
//!
//! The remote control listener binds only an IPv4 address Tailscale assigned to
//! this node, so the daemon is never reachable from an untrusted network.

use std::net::IpAddr;

use crate::is_tailscale_node_addr;

/// Why a candidate bind address is not a valid Tailscale control-listener
/// address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindAddrError {
    /// The address is not an IPv4 address Tailscale assigns to a node.
    #[error("bind address {0} is not a Tailscale IPv4 node address")]
    NotTailscale(IpAddr),
    /// The address is unspecified, loopback, multicast, link-local or broadcast
    /// and must never be used for the remote listener.
    #[error("bind address {0} is not a unicast overlay address and must never be used")]
    Forbidden(IpAddr),
}

/// Validate a daemon control-listener bind address. Fails closed.
///
/// Accepts only IPv4 addresses for which [`is_tailscale_node_addr`] holds.
/// Rejects, in order:
/// - unspecified, loopback, multicast and link-local addresses and the IPv4
///   broadcast address -> [`BindAddrError::Forbidden`];
/// - everything else, including every IPv6 address -> [`BindAddrError::NotTailscale`].
///
/// # Errors
///
/// Returns the [`BindAddrError`] named above.
pub fn validate_tailscale_bind_addr(ip: IpAddr) -> Result<(), BindAddrError> {
    // The most dangerous categories get a distinct error so the operator sees
    // why: binding 0.0.0.0 is a different mistake than binding a LAN address.
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || is_link_local_or_broadcast(ip)
    {
        return Err(BindAddrError::Forbidden(ip));
    }
    if ip.is_ipv4() && is_tailscale_node_addr(ip) {
        Ok(())
    } else {
        Err(BindAddrError::NotTailscale(ip))
    }
}

fn is_link_local_or_broadcast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local() || v4.is_broadcast(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}
