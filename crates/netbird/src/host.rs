//! Resolving a host *name* to a `NetBird` IP from a parsed status.

use std::net::IpAddr;

use overlay::{CandidateIdentity, ExternalIdentity, ExternalIdentityKind};

use crate::is_netbird_ip;
use crate::status::{NetbirdError, NetbirdStatus};

/// Resolve a host name to a `NetBird` IP using a parsed [`NetbirdStatus`].
///
/// Matching is tried in this order:
/// 1. A peer whose stable public key exactly equals `name`.
/// 2. If `name` parses as an IP, it must match a current peer entry.
/// 3. A peer whose `fqdn` equals `name` case-insensitively.
/// 4. A peer whose short hostname (the first DNS label of its `fqdn`) equals
///    `name`.
/// 5. A peer whose `netbirdIp` string equals `name` (a literal IP that happens
///    to be a known peer).
///
/// In every case the resolved address must lie inside the `NetBird` CGNAT range
/// (`100.64.0.0/10`); a peer whose advertised `netbirdIp` is outside it (a
/// loopback, link-local/cloud-metadata, LAN, or public address — through `NetBird`
/// output drift or a compromised coordinator) is treated as **not matched** and
/// is never dialed. This is the same fail-closed gate the bind validator and the
/// raw-IP step apply.
///
/// # Errors
///
/// Returns [`NetbirdError::HostUnknown`] when `name` matches no `NetBird` peer
/// inside the CGNAT range.
pub fn resolve_host(status: &NetbirdStatus, name: &str) -> Result<IpAddr, NetbirdError> {
    resolve_peer(status, name).map(|peer| {
        peer.ip()
            .expect("peer resolution only returns policy-valid addresses")
    })
}

pub(crate) fn resolve_peer<'a>(
    status: &'a NetbirdStatus,
    name: &str,
) -> Result<&'a crate::Peer, NetbirdError> {
    match resolve_staged(status, name, None)? {
        Selected::Peer(peer) => Ok(peer),
        Selected::LocalHost => unreachable!("no own identity is matched without one"),
    }
}

/// What a selector names in one status snapshot.
#[derive(Debug)]
pub(crate) enum Selected<'a> {
    /// This machine itself.
    LocalHost,
    /// One current peer.
    Peer(&'a crate::Peer),
}

/// Resolve `name` against this machine's own identity and its peers.
///
/// The own `NetBird` IP, fqdn and short hostname are matched at the same steps
/// as a peer's (see [`resolve_host`]), so a name that this machine and a peer
/// both carry at one step is ambiguous, never a silent choice.
///
/// # Errors
///
/// Returns [`NetbirdError::HostAmbiguous`] for such a collision and
/// [`NetbirdError::HostUnknown`] when nothing matches.
pub(crate) fn resolve_selector<'a>(
    status: &'a NetbirdStatus,
    name: &str,
) -> Result<Selected<'a>, NetbirdError> {
    let own = OwnIdentity {
        address: status.self_netbird_ip(),
        fqdn: status.self_fqdn(),
    };
    resolve_staged(status, name, Some(own))
}

/// One matching step: whether a peer matches, and whether this machine does.
type Stage<'f> = (&'f dyn Fn(&crate::Peer) -> bool, bool);

/// This machine's identity as its own status reports it.
#[derive(Debug, Clone, Copy)]
struct OwnIdentity<'a> {
    address: Option<IpAddr>,
    fqdn: Option<&'a str>,
}

fn resolve_staged<'a>(
    status: &'a NetbirdStatus,
    name: &str,
    own: Option<OwnIdentity<'_>>,
) -> Result<Selected<'a>, NetbirdError> {
    let needle = name.trim();
    let needle_ip = needle
        .parse::<IpAddr>()
        .ok()
        .filter(|ip| is_netbird_ip(*ip));
    let own_address = own.and_then(|own| own.address);
    let own_fqdn = own.and_then(|own| own.fqdn);

    // 1. Public keys are provider identities and remain stable across IP changes.
    let by_peer_id = |peer: &crate::Peer| peer.peer_id() == Some(needle);
    // 2. A raw NetBird IP is accepted only when current state contains it.
    let by_ip = |peer: &crate::Peer| needle_ip.is_some() && peer.ip() == needle_ip;
    // 3. Exact fqdn match.
    let by_fqdn = |peer: &crate::Peer| {
        peer.fqdn
            .as_deref()
            .is_some_and(|fqdn| fqdn.eq_ignore_ascii_case(needle))
    };
    // 4. Short hostname (first DNS label) match.
    let by_short = |peer: &crate::Peer| {
        peer.fqdn
            .as_deref()
            .and_then(short_hostname)
            .is_some_and(|short| short.eq_ignore_ascii_case(needle))
    };
    // 5. Literal IP string equal to a peer's netbirdIp (e.g. a CIDR-bearing
    //    peer string the caller pasted verbatim is normalized by `Peer::ip`).
    let by_raw_ip = |peer: &crate::Peer| {
        peer.netbird_ip
            .as_deref()
            .is_some_and(|raw| raw.eq_ignore_ascii_case(needle))
    };
    let stages: [Stage<'_>; 5] = [
        (&by_peer_id, false),
        (&by_ip, needle_ip.is_some() && own_address == needle_ip),
        (
            &by_fqdn,
            own_fqdn.is_some_and(|fqdn| fqdn.eq_ignore_ascii_case(needle)),
        ),
        (
            &by_short,
            own_fqdn
                .and_then(short_hostname)
                .is_some_and(|short| short.eq_ignore_ascii_case(needle)),
        ),
        (&by_raw_ip, false),
    ];
    for (matches_peer, matches_own) in stages {
        let peer = match unique_peer(
            status,
            status.peers().iter().filter(|peer| matches_peer(peer)),
            name,
        ) {
            // Several peers collide and this machine matches at the same step,
            // so it is one of the candidates too.
            Err(NetbirdError::HostAmbiguous {
                host,
                mut candidates,
            }) if matches_own => {
                candidates.insert(0, CandidateIdentity::this_machine(own_fqdn));
                return Err(NetbirdError::HostAmbiguous { host, candidates });
            }
            other => other?,
        };
        match (peer, matches_own) {
            (Some(peer), true) => {
                return Err(NetbirdError::HostAmbiguous {
                    host: name.to_owned(),
                    candidates: vec![
                        CandidateIdentity::this_machine(own_fqdn),
                        peer_candidate(peer),
                    ],
                })
            }
            (Some(peer), false) => return Ok(Selected::Peer(peer)),
            (None, true) => return Ok(Selected::LocalHost),
            (None, false) => {}
        }
    }
    Err(NetbirdError::HostUnknown(name.to_owned()))
}

pub(crate) fn resolve_peer_identity<'a>(
    status: &'a NetbirdStatus,
    identity: &ExternalIdentity,
) -> Result<&'a crate::Peer, NetbirdError> {
    let peers = status.peers().iter();
    let peer = match identity.kind() {
        ExternalIdentityKind::PeerId => unique_peer(
            status,
            peers.filter(|peer| peer.peer_id() == Some(identity.value())),
            identity.value(),
        )?,
        ExternalIdentityKind::Fqdn => unique_peer(
            status,
            peers.filter(|peer| {
                peer.fqdn
                    .as_deref()
                    .is_some_and(|fqdn| fqdn.eq_ignore_ascii_case(identity.value()))
            }),
            identity.value(),
        )?,
    };
    peer.ok_or_else(|| NetbirdError::HostUnknown(identity.value().to_owned()))
}

/// The single in-range peer among `peers`, if any.
///
/// A peer whose address this machine or another current peer lists too is not
/// a route: the address does not name one host, so the selector is ambiguous
/// between every host that lists it.
fn unique_peer<'a>(
    status: &'a NetbirdStatus,
    peers: impl Iterator<Item = &'a crate::Peer>,
    name: &str,
) -> Result<Option<&'a crate::Peer>, NetbirdError> {
    let peers = peers
        .filter(|peer| peer.ip().is_some_and(is_netbird_ip))
        .collect::<Vec<_>>();
    let candidates = match peers.as_slice() {
        [] => return Ok(None),
        [peer] => {
            let sharers = status
                .peers()
                .iter()
                .filter(|other| other.ip() == peer.ip())
                .collect::<Vec<_>>();
            let own = status
                .self_netbird_ip()
                .filter(|own| Some(*own) == peer.ip());
            if sharers.len() == 1 && own.is_none() {
                return Ok(Some(*peer));
            }
            let mut candidates = Vec::new();
            if own.is_some() {
                candidates.push(CandidateIdentity::this_machine(status.self_fqdn()));
            }
            candidates.extend(sharers.iter().map(|peer| peer_candidate(peer)));
            return Err(NetbirdError::HostAmbiguous {
                host: name.to_owned(),
                candidates,
            });
        }
        _ => peers,
    };
    Err(NetbirdError::HostAmbiguous {
        host: name.to_owned(),
        candidates: candidates.iter().map(|peer| peer_candidate(peer)).collect(),
    })
}

fn peer_candidate(peer: &crate::Peer) -> CandidateIdentity {
    CandidateIdentity::peer(peer.peer_id(), peer.fqdn.as_deref())
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

    #[test]
    fn a_name_this_machine_shares_with_several_peers_lists_every_candidate() {
        let status = parse_status(
            r#"{"fqdn":"box.a.test","netbirdIp":"100.64.0.7","peers":{"details":[
                {"fqdn":"box.b.test","netbirdIp":"100.64.0.8","publicKey":"key-b"},
                {"fqdn":"box.c.test","netbirdIp":"100.64.0.9","publicKey":"key-c"}
            ]}}"#,
        )
        .expect("status parses");

        let Err(NetbirdError::HostAmbiguous { host, candidates }) =
            resolve_selector(&status, "box")
        else {
            panic!("a short name shared by this machine and two peers is ambiguous");
        };
        assert_eq!(host, "box");
        let names = candidates
            .iter()
            .map(|candidate| (candidate.local, candidate.name.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                (true, "box.a.test"),
                (false, "box.b.test"),
                (false, "box.c.test")
            ]
        );
    }
}
