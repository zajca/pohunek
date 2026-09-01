//! SDK client primitives for pohunek.

#![forbid(unsafe_code)]

mod discovery;
mod error;
mod integration;
mod notifications;
mod transport;

#[doc(inline)]
pub use discovery::{
    discover_hosts, discover_hosts_with_options, DiscoveryOptions, DEFAULT_DISCOVERY_DEADLINE,
    DEFAULT_PROBE_CONCURRENCY, DEFAULT_PROBE_TIMEOUT, DISCOVERY_CACHE_TTL,
    DISCOVERY_LOCK_WAIT_MARGIN, MAX_DISCOVERY_DEADLINE,
};
pub use error::ClientError;
pub use overlay::{
    ConfiguredTransport, ExternalIdentity, ExternalIdentityKind, OverlayRegistry, RegistryError,
};
pub use protocol;
pub use transport::{
    attach_raw, attach_raw_local, attach_raw_local_with_options, attach_raw_tcp_addr,
    attach_raw_tcp_addr_with_options, attach_raw_with_options, connect_raw, connect_raw_local,
    connect_raw_local_with_options, connect_raw_tcp_addr, connect_raw_tcp_addr_with_options,
    connect_raw_with_options, is_local_host, next_request_id, remote_host_with_port, Client,
    ClientOptions, RawStream, Subscription, LOCAL_HOST,
};

/// Load the production overlay registry from fail-fast provider configuration.
///
/// # Errors
///
/// Returns a typed provider configuration error when the registry cannot be
/// constructed.
pub fn default_overlay_registry() -> Result<OverlayRegistry, ClientError> {
    netbird::configured_registry().map_err(ClientError::from)
}
