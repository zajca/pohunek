//! Resolving a host *name* to a NetBird IP from a parsed status.

use std::net::IpAddr;

use crate::is_netbird_ip;
use crate::status::{NetbirdError, NetbirdStatus};

/// Resolve a host name to a NetBird IP using a parsed [`NetbirdStatus`].
///
/// Matching is case-insensitive and tried in this order:
/// 1. If `name` itself parses as an IP inside `100.64.0.0/10`, it is returned
///    directly (lets a user dial a raw NetBird IP without a peer entry).
/// 2. A peer whose `fqdn` equals `name`.
/// 3. A peer whose short hostname (the first DNS label of its `fqdn`) equals
///    `name`.
/// 4. A peer whose `netbirdIp` string equals `name` (a literal IP that happens
///    to be a known peer).
///
/// In every case the resolved address must lie inside the NetBird CGNAT range
/// (`100.64.0.0/10`); a peer whose advertised `netbirdIp` is outside it (a
/// loopback, link-local/cloud-metadata, LAN, or public address — through NetBird
/// output drift or a compromised coordinator) is treated as **not matched** and
/// is never dialed. This is the same fail-closed gate the bind validator and the
/// raw-IP step apply.
///
/// Returns [`NetbirdError::HostUnknown`] when nothing matches.
pub fn resolve_host(status: &NetbirdStatus, name: &str) -> Result<IpAddr, NetbirdError> {
    let needle = name.trim();

    // 1. A raw NetBird IP dials through directly.
    if let Ok(ip) = needle.parse::<IpAddr>() {
        if is_netbird_ip(ip) {
            return Ok(ip);
        }
    }

    // 2. Exact fqdn match.
    if let Some(ip) = status
        .peers()
        .iter()
        .filter(|peer| {
            peer.fqdn
                .as_deref()
                .is_some_and(|fqdn| fqdn.eq_ignore_ascii_case(needle))
        })
        .find_map(|peer| peer.ip().filter(|ip| is_netbird_ip(*ip)))
    {
        return Ok(ip);
    }

    // 3. Short hostname (first DNS label) match.
    if let Some(ip) = status
        .peers()
        .iter()
        .filter(|peer| {
            peer.fqdn
                .as_deref()
                .and_then(short_hostname)
                .is_some_and(|short| short.eq_ignore_ascii_case(needle))
        })
        .find_map(|peer| peer.ip().filter(|ip| is_netbird_ip(*ip)))
    {
        return Ok(ip);
    }

    // 4. Literal IP string equal to a peer's netbirdIp (e.g. a CIDR-bearing
    //    peer string the caller pasted verbatim is normalized by `Peer::ip`).
    if let Some(ip) = status
        .peers()
        .iter()
        .filter(|peer| {
            peer.netbird_ip
                .as_deref()
                .is_some_and(|raw| raw.eq_ignore_ascii_case(needle))
        })
        .find_map(|peer| peer.ip().filter(|ip| is_netbird_ip(*ip)))
    {
        return Ok(ip);
    }

    Err(NetbirdError::HostUnknown(name.to_owned()))
}

/// The short hostname: the first DNS label of a fully qualified name.
///
/// Returns `None` for an empty input or a leading-dot name.
fn short_hostname(fqdn: &str) -> Option<&str> {
    fqdn.split('.').next().filter(|label| !label.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::parse_status;

    const STATUS_CURRENT: &str = include_str!("../tests/fixtures/status_current.json");

    fn status() -> NetbirdStatus {
        parse_status(STATUS_CURRENT).expect("fixture parses")
    }

    #[test]
    fn resolves_by_full_fqdn() {
        let ip = resolve_host(&status(), "host-b.netbird.cloud").unwrap();
        assert_eq!(ip, "100.92.30.40".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn resolves_by_short_hostname() {
        let ip = resolve_host(&status(), "host-b").unwrap();
        assert_eq!(ip, "100.92.30.40".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn resolves_case_insensitively() {
        let ip = resolve_host(&status(), "HOST-B.NetBird.Cloud").unwrap();
        assert_eq!(ip, "100.92.30.40".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn resolves_by_literal_peer_ip() {
        let ip = resolve_host(&status(), "100.92.30.40").unwrap();
        assert_eq!(ip, "100.92.30.40".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn resolves_raw_netbird_ip_even_without_peer() {
        // 100.64.0.99 is not a peer in the fixture but is a valid NetBird IP.
        let ip = resolve_host(&status(), "100.64.0.99").unwrap();
        assert_eq!(ip, "100.64.0.99".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn raw_non_netbird_ip_does_not_resolve() {
        // A public IP literal must not bypass peer matching.
        let err = resolve_host(&status(), "8.8.8.8").unwrap_err();
        assert!(matches!(err, NetbirdError::HostUnknown(_)));
    }

    #[test]
    fn peer_with_non_netbird_ip_does_not_resolve() {
        // A peer that matches by name/fqdn/ip but advertises an address OUTSIDE
        // the NetBird range must be treated as not-matched (fail closed), so the
        // CLI never dials a loopback / cloud-metadata / LAN / public host the
        // peer table happens to carry.
        let status = parse_status(
            r#"{"peers":[{"fqdn":"evil.netbird.cloud","netbirdIp":"169.254.169.254","status":"Connected"}]}"#,
        )
        .expect("inline status parses");

        for name in ["evil", "evil.netbird.cloud", "169.254.169.254"] {
            assert!(
                matches!(
                    resolve_host(&status, name),
                    Err(NetbirdError::HostUnknown(_))
                ),
                "name {name} must not resolve to a non-NetBird IP"
            );
        }
    }

    #[test]
    fn unknown_host_is_error() {
        let err = resolve_host(&status(), "no-such-host").unwrap_err();
        match err {
            NetbirdError::HostUnknown(name) => assert_eq!(name, "no-such-host"),
            other => panic!("expected HostUnknown, got {other:?}"),
        }
    }

    #[test]
    fn short_hostname_extracts_first_label() {
        assert_eq!(short_hostname("a.b.c"), Some("a"));
        assert_eq!(short_hostname("solo"), Some("solo"));
        assert_eq!(short_hostname(""), None);
        assert_eq!(short_hostname(".leading"), None);
    }
}
