//! `NetBird` adapter for pohunek remote hosts.
//!
//! This crate is the foundation shared by the daemon and the CLI for the
//! "remote hosts over `NetBird`" feature (Phase 2). It is deliberately small,
//! synchronous, and dependency-light so both downstream crates can depend on it
//! without pulling in an async runtime: subprocess execution uses
//! [`std::process::Command`], not tokio.
//!
//! Responsibilities:
//! - Parse `netbird status --json`. `NetBird`'s JSON output drifts across
//!   versions, so the [`NetbirdStatus`] / [`Peer`] types are intentionally
//!   defensive: unknown fields are ignored, missing optional fields default,
//!   and two documented shapes (current source vs. legacy/docs) are tolerated by
//!   the same types. See [`parse_status`].
//! - Resolve a host *name* to a `NetBird` IP from a parsed status
//!   ([`resolve_host`]).
//! - Validate a daemon control-listener bind address, failing closed
//!   ([`validate_netbird_bind_addr`]) so the daemon never opens a socket on a
//!   non-NetBird interface.
//! - Resolve the remote control port from the environment ([`remote_port`]).
//!
//! `NetBird` assigns addresses from the RFC 6598 CGNAT range `100.64.0.0/10`
//! (`100.64.0.0` ..= `100.127.255.255`); that range is the trust boundary used
//! throughout this crate.

#![forbid(unsafe_code)]

mod bind;
mod host;
mod port;
mod status;
mod transport;

pub use bind::{validate_netbird_bind_addr, BindAddrError};
pub use host::resolve_host;
pub use port::{remote_port, DEFAULT_REMOTE_PORT, REMOTE_PORT_ENV};
pub use status::{
    parse_status, run_status, run_status_async, run_status_with_program, NetbirdError,
    NetbirdStatus, Peer,
};
pub use transport::{configured_registry, NetbirdTransport, NETBIRD_OVERLAY_ID};

/// True when `ip` is an IPv4 address inside the `NetBird` CGNAT range
/// `100.64.0.0/10` (RFC 6598).
///
/// `100.64.0.0/10` is exactly: first octet `100` AND second octet in `64..=127`.
/// IPv6 is never a `NetBird` address.
///
/// Public so callers that dial a peer address (e.g. the CLI's `host discover`
/// probe) can apply the same fail-closed range gate the resolver and bind
/// validator use, rather than trusting an arbitrary IP a peer advertises.
#[must_use]
pub fn is_netbird_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            a == 100 && (64..=127).contains(&b)
        }
        std::net::IpAddr::V6(_) => false,
    }
}

/// Parse an address string that may carry a trailing CIDR mask (e.g.
/// `100.64.0.10/16`) into an [`std::net::IpAddr`], stripping the mask.
///
/// Returns `None` if the address part does not parse.
#[must_use]
pub(crate) fn parse_addr_strip_cidr(value: &str) -> Option<std::net::IpAddr> {
    let trimmed = value.trim();
    let addr_part = trimmed.split('/').next().unwrap_or(trimmed).trim();
    addr_part.parse::<std::net::IpAddr>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn netbird_range_boundaries() {
        assert!(is_netbird_ip("100.64.0.0".parse::<IpAddr>().unwrap()));
        assert!(is_netbird_ip("100.127.255.255".parse::<IpAddr>().unwrap()));
        assert!(is_netbird_ip("100.92.10.20".parse::<IpAddr>().unwrap()));
        // Just outside the lower and upper edges.
        assert!(!is_netbird_ip("100.63.255.255".parse::<IpAddr>().unwrap()));
        assert!(!is_netbird_ip("100.128.0.0".parse::<IpAddr>().unwrap()));
        // Clearly outside.
        assert!(!is_netbird_ip("10.0.0.1".parse::<IpAddr>().unwrap()));
        assert!(!is_netbird_ip("8.8.8.8".parse::<IpAddr>().unwrap()));
        // IPv6 is never NetBird.
        assert!(!is_netbird_ip("::1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn strip_cidr_mask() {
        assert_eq!(
            parse_addr_strip_cidr("100.64.0.10/16"),
            Some("100.64.0.10".parse::<IpAddr>().unwrap())
        );
        assert_eq!(
            parse_addr_strip_cidr("100.92.10.20"),
            Some("100.92.10.20".parse::<IpAddr>().unwrap())
        );
        assert_eq!(
            parse_addr_strip_cidr("  100.64.0.1 "),
            Some("100.64.0.1".parse().unwrap())
        );
        assert_eq!(parse_addr_strip_cidr("not-an-ip"), None);
        assert_eq!(parse_addr_strip_cidr(""), None);
    }
}
