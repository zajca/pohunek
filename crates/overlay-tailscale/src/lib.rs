//! Tailscale adapter for pohunek remote hosts.
//!
//! The adapter reads `tailscale status --json` from the local CLI and maps it
//! onto the generic overlay contract. It never talks to `tailscaled` directly.
//! The status format is documented as subject to change, so [`parse_status`]
//! reads only the fields the mapping needs and ignores everything else.
//!
//! Responsibilities:
//! - Find the CLI through the shared trusted-executable policy and run it with
//!   a cancellable subprocess ([`run_status_async`]).
//! - Pick this host's IPv4 listener address, failing closed unless Tailscale is
//!   running on a kernel interface ([`TailscaleTransport`]).
//! - Select the peers an owner may route to: in the netmap, not expired, not
//!   flagged `ShareeNode` (a node of a user this tailnet shared a machine to), with an address Tailscale assigns
//!   that the status lists for that exact peer.
//!
//! Tailscale assigns node addresses from `100.64.0.0/10` and `fd7a:115c:a1e0::/48`.
//! Both ranges contain addresses that never belong to a node; see
//! [`is_tailscale_node_addr`].

// Rust guideline compliant 2026-10-23

#![forbid(unsafe_code)]

mod bind;
mod peers;
mod range;
mod status;
mod transport;

pub use bind::{validate_tailscale_bind_addr, BindAddrError};
pub use range::is_tailscale_node_addr;
pub use status::{parse_status, run_status_async, TailscaleError, TailscaleStatus};
pub use transport::{TailscaleTransport, TAILSCALE_OVERLAY_ID};
