//! Validation of the daemon control-listener bind address.
//!
//! The remote control listener must only ever bind to a `NetBird` interface, so
//! the daemon is never reachable from an untrusted network. Validation fails
//! closed: anything not provably inside `100.64.0.0/10` is rejected.

use std::net::IpAddr;

use crate::is_netbird_ip;

/// Why a candidate bind address is not a valid `NetBird` control-listener
/// address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindAddrError {
    /// The address is routable/typed but lies outside `100.64.0.0/10`
    /// (RFC 1918, public IPv4, or any IPv6).
    #[error("bind address {0} is not inside the NetBird range 100.64.0.0/10")]
    NotNetbird(IpAddr),
    /// The address is unspecified (`0.0.0.0` / `::`) or loopback and must never
    /// be used for the remote listener.
    #[error("bind address {0} is unspecified/loopback and must never be used")]
    Forbidden(IpAddr),
}

/// Validate a daemon control-listener bind address. Fails closed.
///
/// Accepts only IPv4 addresses inside `100.64.0.0/10`
/// (`100.64.0.0` ..= `100.127.255.255`). Rejects, in order:
/// - unspecified (`0.0.0.0` / `::`) and loopback addresses -> [`BindAddrError::Forbidden`];
/// - everything else outside the `NetBird` range (RFC 1918, public IPv4, all
///   IPv6) -> [`BindAddrError::NotNetbird`].
pub fn validate_netbird_bind_addr(ip: IpAddr) -> Result<(), BindAddrError> {
    // Reject the most dangerous categories first with a distinct error so the
    // operator sees *why* (binding 0.0.0.0 is a different mistake than binding
    // a private IP).
    if ip.is_unspecified() || ip.is_loopback() {
        return Err(BindAddrError::Forbidden(ip));
    }

    if is_netbird_ip(ip) {
        Ok(())
    } else {
        Err(BindAddrError::NotNetbird(ip))
    }
}
