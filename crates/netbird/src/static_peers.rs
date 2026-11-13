//! Owner-configured static peers for an account that does not own the `NetBird` connection.
//!
//! `NetBird` 0.83 and later withhold the detailed status from such an account.
//! The owner lists the peers it wants to reach in `overlays.toml`; each entry
//! carries the address, the DNS name and the pohunek host id of the daemon
//! that must answer at that address.

use std::net::{IpAddr, Ipv4Addr};

/// One owner-configured peer, used only while `NetBird` withholds its status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticPeer {
    /// The `overlays.toml` table key of the entry.
    pub name: String,
    /// The provider-qualified DNS name of the peer.
    pub fqdn: String,
    /// The `NetBird` address of the peer.
    pub address: Ipv4Addr,
    /// The pohunek host id the daemon at [`Self::address`] must report.
    pub host_id: String,
}

/// The configured subnet and its peers; built only from a validated `overlays.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticPeers {
    network: Ipv4Addr,
    prefix: u8,
    peers: Vec<StaticPeer>,
}

impl StaticPeers {
    /// Build the set from values the `pohunek-client` loader has validated.
    ///
    /// The loader guarantees that `network` with `prefix` is an IPv4 network
    /// inside `100.64.0.0/10` with no host bits set, that every entry address
    /// lies inside it other than its network and broadcast address, and that
    /// the entries are unique; the `debug_assert!`s below restate that.
    #[must_use]
    pub fn new(network: Ipv4Addr, prefix: u8, peers: Vec<StaticPeer>) -> Self {
        debug_assert!((10..=30).contains(&prefix), "prefix length is valid");
        debug_assert!(
            network.octets()[0] == 100 && (64..=127).contains(&network.octets()[1]),
            "the network lies inside 100.64.0.0/10"
        );
        for peer in &peers {
            debug_assert!(
                Self::contains_in(network, prefix, IpAddr::V4(peer.address)),
                "an entry address lies inside the subnet"
            );
        }
        Self {
            network,
            prefix,
            peers,
        }
    }

    /// True when `ip` lies inside the configured subnet.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        Self::contains_in(self.network, self.prefix, ip)
    }

    fn contains_in(network: Ipv4Addr, prefix: u8, ip: IpAddr) -> bool {
        let IpAddr::V4(ip) = ip else {
            return false;
        };
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        u32::from(network) & mask == u32::from(ip) & mask
    }

    /// The configured network address, for cross-check details.
    #[must_use]
    pub fn network(&self) -> Ipv4Addr {
        self.network
    }

    /// The configured prefix length.
    #[must_use]
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// The configured entries, in validation order.
    #[must_use]
    pub fn peers(&self) -> &[StaticPeer] {
        &self.peers
    }

    /// The entry whose key, fqdn or short DNS label matches `selector`, or
    /// `None` when no entry matches. A raw IP never matches.
    #[must_use]
    pub fn match_selector(&self, selector: &str) -> Option<&StaticPeer> {
        let needle = selector.trim();
        if needle.parse::<IpAddr>().is_ok() {
            return None;
        }
        self.peers.iter().find(|peer| {
            peer.name.eq_ignore_ascii_case(needle)
                || peer.fqdn.eq_ignore_ascii_case(needle)
                || peer
                    .fqdn
                    .split('.')
                    .next()
                    .is_some_and(|short| short.eq_ignore_ascii_case(needle))
        })
    }

    /// The entry whose fqdn equals `fqdn`, case-insensitively.
    #[must_use]
    pub fn match_fqdn(&self, fqdn: &str) -> Option<&StaticPeer> {
        self.peers
            .iter()
            .find(|peer| peer.fqdn.eq_ignore_ascii_case(fqdn))
    }
}
