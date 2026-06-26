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

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("valid test IP literal")
    }

    #[test]
    fn accepts_netbird_addresses() {
        validate_netbird_bind_addr(ip("100.64.0.1")).unwrap();
        validate_netbird_bind_addr(ip("100.92.10.20")).unwrap();
        validate_netbird_bind_addr(ip("100.127.255.255")).unwrap();
    }

    #[test]
    fn rejects_just_outside_netbird_range() {
        assert_eq!(
            validate_netbird_bind_addr(ip("100.63.255.255")),
            Err(BindAddrError::NotNetbird(ip("100.63.255.255")))
        );
        assert_eq!(
            validate_netbird_bind_addr(ip("100.128.0.0")),
            Err(BindAddrError::NotNetbird(ip("100.128.0.0")))
        );
    }

    #[test]
    fn rejects_unspecified_and_loopback_as_forbidden() {
        assert_eq!(
            validate_netbird_bind_addr(ip("0.0.0.0")),
            Err(BindAddrError::Forbidden(ip("0.0.0.0")))
        );
        assert_eq!(
            validate_netbird_bind_addr(ip("127.0.0.1")),
            Err(BindAddrError::Forbidden(ip("127.0.0.1")))
        );
        // IPv6 unspecified and loopback are Forbidden too (checked before the
        // NotNetbird path).
        assert_eq!(
            validate_netbird_bind_addr(ip("::")),
            Err(BindAddrError::Forbidden(ip("::")))
        );
        assert_eq!(
            validate_netbird_bind_addr(ip("::1")),
            Err(BindAddrError::Forbidden(ip("::1")))
        );
    }

    #[test]
    fn rejects_rfc1918_private_addresses() {
        assert_eq!(
            validate_netbird_bind_addr(ip("10.0.0.1")),
            Err(BindAddrError::NotNetbird(ip("10.0.0.1")))
        );
        assert_eq!(
            validate_netbird_bind_addr(ip("192.168.1.1")),
            Err(BindAddrError::NotNetbird(ip("192.168.1.1")))
        );
        assert_eq!(
            validate_netbird_bind_addr(ip("172.16.0.1")),
            Err(BindAddrError::NotNetbird(ip("172.16.0.1")))
        );
    }

    #[test]
    fn rejects_public_ipv4() {
        assert_eq!(
            validate_netbird_bind_addr(ip("8.8.8.8")),
            Err(BindAddrError::NotNetbird(ip("8.8.8.8")))
        );
    }

    #[test]
    fn rejects_all_ipv6() {
        // A global IPv6 address: not loopback/unspecified, so NotNetbird.
        assert_eq!(
            validate_netbird_bind_addr(ip("2001:db8::1")),
            Err(BindAddrError::NotNetbird(ip("2001:db8::1")))
        );
        // An IPv4-mapped IPv6 address is still IPv6 here -> NotNetbird.
        assert_eq!(
            validate_netbird_bind_addr(ip("::ffff:100.64.0.1")),
            Err(BindAddrError::NotNetbird(ip("::ffff:100.64.0.1")))
        );
    }
}
