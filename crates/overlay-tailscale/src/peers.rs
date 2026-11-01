//! Peer selection and name resolution over one status snapshot.
//!
//! A peer is selectable only when the netmap holds it, its key has not
//! expired, it is not flagged `ShareeNode` (a node of a user this tailnet shared a machine to), and the status
//! lists an address Tailscale assigns for that exact peer. Tagged peers are
//! selectable like user-owned ones. The identity is the stable node `ID`; the
//! map key and `HostName` are never used.

use std::collections::HashSet;
use std::net::IpAddr;

use overlay::{CandidateIdentity, ExternalIdentity, ExternalIdentityKind};

use crate::is_tailscale_node_addr;
use crate::status::{Node, TailscaleError, TailscaleStatus};

/// The address to dial for `node`: its first IPv4 node address, else its first
/// IPv6 node address.
///
/// The remote listener binds IPv4 only, so IPv6 is the fallback for a node
/// that has no IPv4 address.
pub(crate) fn dial_address(node: &Node) -> Option<IpAddr> {
    let mut v6 = None;
    for address in node.addresses().filter(|a| is_tailscale_node_addr(*a)) {
        if address.is_ipv4() {
            return Some(address);
        }
        v6.get_or_insert(address);
    }
    v6
}

/// Every Tailscale node address `node` lists, each once.
fn node_addresses(node: &Node) -> HashSet<IpAddr> {
    node.addresses()
        .filter(|a| is_tailscale_node_addr(*a))
        .collect()
}

/// The addresses that two or more of this machine and the selectable peers list.
pub(crate) fn shared_node_addresses(status: &TailscaleStatus) -> HashSet<IpAddr> {
    let holders = status
        .self_node()
        .into_iter()
        .chain(status.peer_nodes().filter(|peer| is_selectable(peer)));
    overlay::shared_addresses(holders.flat_map(|node| node_addresses(node).into_iter()))
}

/// True when `node` lists an address in `shared`.
pub(crate) fn lists_any(node: &Node, shared: &HashSet<IpAddr>) -> bool {
    node.addresses().any(|address| shared.contains(&address))
}

/// True when the owner may route to `peer`.
pub(crate) fn is_selectable(peer: &Node) -> bool {
    peer.in_network_map() && !peer.expired() && !peer.sharee_node()
}

/// Selectable peers that have an address to dial.
fn dialable_peers(status: &TailscaleStatus) -> impl Iterator<Item = &Node> {
    status
        .peer_nodes()
        .filter(|peer| is_selectable(peer) && dial_address(peer).is_some())
}

/// The first label of a fully qualified name; `None` for an empty label.
pub(crate) fn short_name(fqdn: &str) -> Option<&str> {
    fqdn.split('.').next().filter(|label| !label.is_empty())
}

/// What a selector names in one status snapshot.
#[derive(Debug)]
pub(crate) enum Selected<'a> {
    /// This machine itself.
    LocalHost,
    /// One selectable peer.
    Peer(&'a Node),
}

/// Resolve `name` to one selectable peer.
///
/// # Errors
///
/// [`TailscaleError::HostUnknown`] and [`TailscaleError::HostAmbiguous`].
pub(crate) fn resolve_peer<'a>(
    status: &'a TailscaleStatus,
    name: &str,
) -> Result<&'a Node, TailscaleError> {
    match resolve_staged(status, name, false)? {
        Selected::Peer(peer) => Ok(peer),
        Selected::LocalHost => unreachable!("no own identity is matched when it is not asked for"),
    }
}

/// Resolve `name` against this machine's own identity and its peers.
///
/// This machine's addresses, name and short name are matched at the same
/// steps as a peer's, so a name that this machine and a peer both carry at one
/// step is ambiguous, never a silent choice.
///
/// # Errors
///
/// [`TailscaleError::HostUnknown`] and [`TailscaleError::HostAmbiguous`].
pub(crate) fn resolve_selector<'a>(
    status: &'a TailscaleStatus,
    name: &str,
) -> Result<Selected<'a>, TailscaleError> {
    resolve_staged(status, name, true)
}

/// Resolve one typed canonical identity to a selectable peer.
///
/// # Errors
///
/// [`TailscaleError::HostUnknown`] and [`TailscaleError::HostAmbiguous`].
pub(crate) fn resolve_peer_identity<'a>(
    status: &'a TailscaleStatus,
    identity: &ExternalIdentity,
) -> Result<&'a Node, TailscaleError> {
    let value = identity.value();
    let peer = match identity.kind() {
        ExternalIdentityKind::PeerId => unique_peer(
            status,
            dialable_peers(status).filter(|peer| peer.id() == Some(value)),
            value,
        )?,
        ExternalIdentityKind::Fqdn => unique_peer(
            status,
            dialable_peers(status).filter(|peer| fqdn_equals(peer, value)),
            value,
        )?,
    };
    peer.ok_or_else(|| TailscaleError::HostUnknown(value.to_owned()))
}

fn fqdn_equals(node: &Node, name: &str) -> bool {
    node.fqdn()
        .is_some_and(|fqdn| fqdn.eq_ignore_ascii_case(name.trim_end_matches('.')))
}

fn short_equals(node: &Node, name: &str) -> bool {
    node.fqdn()
        .and_then(short_name)
        .is_some_and(|short| short.eq_ignore_ascii_case(name))
}

fn has_address(node: &Node, wanted: IpAddr) -> bool {
    node.addresses()
        .any(|address| address == wanted && is_tailscale_node_addr(address))
}

/// One matching step: how a peer matches, and whether this machine matches.
type Stage<'f> = (&'f dyn Fn(&Node) -> bool, bool);

fn resolve_staged<'a>(
    status: &'a TailscaleStatus,
    name: &str,
    with_own: bool,
) -> Result<Selected<'a>, TailscaleError> {
    let needle = name.trim();
    let needle_ip = needle
        .parse::<IpAddr>()
        .ok()
        .filter(|ip| is_tailscale_node_addr(*ip));
    let own = status.self_node().filter(|_| with_own);
    let own_fqdn = own.and_then(Node::fqdn);

    // 1. The stable node identity.
    let by_id = |peer: &Node| peer.id() == Some(needle);
    // 2. A raw node address, accepted only when the current status lists it
    //    for that exact peer.
    let by_address = |peer: &Node| needle_ip.is_some_and(|ip| has_address(peer, ip));
    // 3. The full `MagicDNS` name.
    let by_fqdn = |peer: &Node| fqdn_equals(peer, needle);
    // 4. The short name: the first label of the `MagicDNS` name.
    let by_short = |peer: &Node| short_equals(peer, needle);
    let stages: [Stage<'_>; 4] = [
        (&by_id, false),
        (
            &by_address,
            needle_ip.is_some_and(|ip| own.is_some_and(|own| has_address(own, ip))),
        ),
        (&by_fqdn, own.is_some_and(|own| fqdn_equals(own, needle))),
        (&by_short, own.is_some_and(|own| short_equals(own, needle))),
    ];
    for (matches_peer, matches_own) in stages {
        let peer = match unique_peer(
            status,
            dialable_peers(status).filter(|peer| matches_peer(peer)),
            name,
        ) {
            // Several peers collide and this machine matches at the same step,
            // so it is one of the candidates too.
            Err(TailscaleError::HostAmbiguous {
                host,
                mut candidates,
            }) if matches_own => {
                candidates.insert(0, CandidateIdentity::this_machine(own_fqdn));
                return Err(TailscaleError::HostAmbiguous { host, candidates });
            }
            other => other?,
        };
        match (peer, matches_own) {
            (Some(peer), true) => {
                return Err(TailscaleError::HostAmbiguous {
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
    Err(TailscaleError::HostUnknown(name.to_owned()))
}

/// The single dialable peer among `peers`, if any.
///
/// A peer that lists an address which this machine or another selectable peer
/// lists too is not a route: the address does not name one node, so the
/// selector is ambiguous between every node that lists it.
fn unique_peer<'a>(
    status: &'a TailscaleStatus,
    peers: impl Iterator<Item = &'a Node>,
    name: &str,
) -> Result<Option<&'a Node>, TailscaleError> {
    let peers = peers.collect::<Vec<_>>();
    let candidates = match peers.as_slice() {
        [] => return Ok(None),
        [peer] => {
            let addresses = node_addresses(peer);
            let overlaps = |other: &Node| {
                node_addresses(other)
                    .intersection(&addresses)
                    .next()
                    .is_some()
            };
            let own = status.self_node().filter(|own| overlaps(own));
            let sharers = status
                .peer_nodes()
                .filter(|other| is_selectable(other) && overlaps(other))
                .collect::<Vec<_>>();
            if own.is_none() && sharers.len() == 1 {
                return Ok(Some(*peer));
            }
            let mut candidates = Vec::new();
            if let Some(own) = own {
                candidates.push(CandidateIdentity::this_machine(own.fqdn()));
            }
            candidates.extend(sharers.iter().map(|peer| peer_candidate(peer)));
            return Err(TailscaleError::HostAmbiguous {
                host: name.to_owned(),
                candidates,
            });
        }
        _ => peers,
    };
    Err(TailscaleError::HostAmbiguous {
        host: name.to_owned(),
        candidates: candidates.iter().map(|peer| peer_candidate(peer)).collect(),
    })
}

fn peer_candidate(peer: &Node) -> CandidateIdentity {
    CandidateIdentity::peer(peer.id(), peer.fqdn())
}
