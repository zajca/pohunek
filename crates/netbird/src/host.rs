//! Resolving a host *name* to a `NetBird` IP from a parsed status.

use std::net::IpAddr;

use overlay::{ExternalIdentity, ExternalIdentityKind};

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
    let needle = name.trim();

    // 1. Public keys are provider identities and remain stable across IP changes.
    if let Some(peer) = unique_peer(
        status
            .peers()
            .iter()
            .filter(|peer| peer.peer_id() == Some(needle)),
        name,
    )? {
        return Ok(peer);
    }

    // 2. A raw NetBird IP is accepted only when current peer state contains it.
    if let Ok(ip) = needle.parse::<IpAddr>() {
        if is_netbird_ip(ip) {
            if let Some(peer) = unique_peer(
                status.peers().iter().filter(|peer| peer.ip() == Some(ip)),
                name,
            )? {
                return Ok(peer);
            }
        }
    }

    // 3. Exact fqdn match.
    if let Some(peer) = unique_peer(
        status.peers().iter().filter(|peer| {
            peer.fqdn
                .as_deref()
                .is_some_and(|fqdn| fqdn.eq_ignore_ascii_case(needle))
        }),
        name,
    )? {
        return Ok(peer);
    }

    // 4. Short hostname (first DNS label) match.
    if let Some(peer) = unique_peer(
        status.peers().iter().filter(|peer| {
            peer.fqdn
                .as_deref()
                .and_then(short_hostname)
                .is_some_and(|short| short.eq_ignore_ascii_case(needle))
        }),
        name,
    )? {
        return Ok(peer);
    }

    // 5. Literal IP string equal to a peer's netbirdIp (e.g. a CIDR-bearing
    //    peer string the caller pasted verbatim is normalized by `Peer::ip`).
    if let Some(peer) = unique_peer(
        status.peers().iter().filter(|peer| {
            peer.netbird_ip
                .as_deref()
                .is_some_and(|raw| raw.eq_ignore_ascii_case(needle))
        }),
        name,
    )? {
        return Ok(peer);
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
            peers.filter(|peer| peer.peer_id() == Some(identity.value())),
            identity.value(),
        )?,
        ExternalIdentityKind::Fqdn => unique_peer(
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

fn unique_peer<'a>(
    peers: impl Iterator<Item = &'a crate::Peer>,
    name: &str,
) -> Result<Option<&'a crate::Peer>, NetbirdError> {
    let peers = peers
        .filter(|peer| peer.ip().is_some_and(is_netbird_ip))
        .collect::<Vec<_>>();
    match peers.as_slice() {
        [] => Ok(None),
        [peer] => Ok(Some(*peer)),
        _ => Err(NetbirdError::HostAmbiguous(name.to_owned())),
    }
}

/// The short hostname: the first DNS label of a fully qualified name.
///
/// Returns `None` for an empty input or a leading-dot name.
fn short_hostname(fqdn: &str) -> Option<&str> {
    fqdn.split('.').next().filter(|label| !label.is_empty())
}
