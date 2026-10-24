//! Address ranges Tailscale assigns node addresses from.
//!
//! Source: `net/tsaddr/tsaddr.go` at `tailscale/tailscale@v1.104.2`
//! (`IsTailscaleIP`, `ChromeOSVMRange`, `TailscaleServiceIP*`, `TailscaleViaRange`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// First address and prefix length of the carrier-grade NAT range Tailscale
/// assigns IPv4 node addresses from (`100.64.0.0/10`).
const CGNAT_BASE: [u8; 4] = [100, 64, 0, 0];
const CGNAT_PREFIX_LEN: u32 = 10;

/// The part of the CGNAT range `ChromeOS` uses to connect its host to containers
/// and VMs (`100.115.92.0/23`). Tailscale never assigns it to a node.
const CHROMEOS_BASE: [u8; 4] = [100, 115, 92, 0];
const CHROMEOS_PREFIX_LEN: u32 = 23;

/// IPv4 address of the services Tailscale provides on every node, such as the
/// `MagicDNS` proxy. It is inside the CGNAT range and local to each node.
const SERVICE_V4: Ipv4Addr = Ipv4Addr::new(100, 100, 100, 100);

/// First three 16-bit groups of the IPv6 unique-local range Tailscale assigns
/// node addresses from (`fd7a:115c:a1e0::/48`).
const ULA_PREFIX: [u16; 3] = [0xfd7a, 0x115c, 0xa1e0];

/// IPv6 address of the services Tailscale provides on every node.
const SERVICE_V6: Ipv6Addr = Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 0x53);

/// Fourth 16-bit group of the 4via6 range (`fd7a:115c:a1e0:b1a::/64`), which
/// addresses subnet-router routes and never a node.
const VIA_GROUP: u16 = 0x0b1a;

/// True when `ip` is an address Tailscale can assign to a node.
///
/// That is an IPv4 address in `100.64.0.0/10` other than the `ChromeOS` range
/// `100.115.92.0/23` and the service address `100.100.100.100`, or an IPv6
/// address in `fd7a:115c:a1e0::/48` other than the service address
/// `fd7a:115c:a1e0::53` and the 4via6 range. An IPv4-mapped IPv6 address is
/// never a node address.
///
/// Public so callers that dial a peer address can apply the same fail-closed
/// range gate the resolver and the bind validator use.
#[must_use]
pub fn is_tailscale_node_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            in_prefix_v4(v4, CGNAT_BASE, CGNAT_PREFIX_LEN)
                && !in_prefix_v4(v4, CHROMEOS_BASE, CHROMEOS_PREFIX_LEN)
                && v4 != SERVICE_V4
        }
        IpAddr::V6(v6) => {
            let groups = v6.segments();
            groups[..3] == ULA_PREFIX && groups[3] != VIA_GROUP && v6 != SERVICE_V6
        }
    }
}

fn in_prefix_v4(ip: Ipv4Addr, base: [u8; 4], prefix_len: u32) -> bool {
    let mask = u32::MAX << (u32::BITS - prefix_len);
    u32::from(ip) & mask == u32::from_be_bytes(base) & mask
}
