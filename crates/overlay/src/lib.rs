//! Generic overlay transport abstraction for pohunek remote hosts.
//!
//! This crate defines the [`OverlayTransport`] trait that each overlay
//! (`NetBird`, Tailscale, custom `WireGuard`) implements. The daemon and client
//! consume the trait rather than any vendor-specific adapter, enabling multiple
//! overlays to coexist side by side.
//!
//! Trust boundaries remain per-implementation: each transport validates its own
//! bind addresses and resolves only peers inside its allowed range, fail-closed.

#![forbid(unsafe_code)]

mod netbird;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// Why a candidate bind address was rejected by an overlay.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindAddrError {
    /// The address is routable but outside the overlay's trusted range.
    #[error("bind address {0} is not inside the overlay's trusted address range")]
    NotMember(IpAddr),
    /// Unspecified or loopback addresses must never be used for a listener.
    #[error("bind address {0} is unspecified/loopback and must never be used")]
    Forbidden(IpAddr),
}

/// Errors returned by overlay resolution and discovery operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OverlayError {
    /// The requested host could not be resolved by this overlay.
    #[error("host {0} not found in overlay {1}")]
    HostUnknown(String, String),
    /// The overlay CLI is not installed on this machine.
    #[error("overlay CLI for {0} is not installed")]
    CliMissing(String),
    /// The overlay state could not be read or parsed.
    #[error("overlay {0} state unavailable: {1}")]
    StateUnavailable(String, String),
}

/// Stable machine identifier for an overlay transport (e.g. `"netbird"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OverlayId(pub Arc<str>);

impl std::fmt::Display for OverlayId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A peer discovered through an overlay with optional dialable address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPeer {
    pub overlay: OverlayId,
    pub peer_id: String,
    pub display_name: Option<String>,
    pub fqdn: Option<String>,
    pub addr: Option<SocketAddr>,
}

/// A host resolved to exactly one dialable socket address via an overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOverlayHost {
    pub overlay: OverlayId,
    pub peer_id: Option<String>,
    pub display_name: Option<String>,
    pub fqdn: Option<String>,
    pub addr: SocketAddr,
}

/// Trait implemented by every overlay transport (`NetBird`, Tailscale, WG).
///
/// Implementations must enforce their own trust boundary fail-closed:
/// `validate_bind_addr` rejects non-member addresses, `resolve_host` never
/// returns addresses outside the overlay's allowed range, and `discover_peers`
/// emits no addresses outside that policy.
pub trait OverlayTransport: Send + Sync {
    /// Return the stable machine identifier for this overlay.
    fn id(&self) -> OverlayId;

    /// Validate a daemon control-listener bind address against this overlay's
    /// trusted range. Rejects unspecified/loopback before membership checks.
    fn validate_bind_addr(&self, addr: IpAddr) -> Result<(), BindAddrError>;

    /// Resolve a user-supplied host name to exactly one dialable address using
    /// this overlay's peer state. Never returns non-overlay addresses.
    fn resolve_host(&self, host: &str, port: u16) -> Result<ResolvedOverlayHost, OverlayError>;

    /// Enumerate all peers visible to this overlay. Addresses outside the
    /// overlay policy are omitted (never surfaced).
    fn discover_peers(&self, port: u16) -> Result<Vec<DiscoveredPeer>, OverlayError>;
}

pub use netbird::NetbirdTransport;

// Rust guideline compliant 2026-06-26
