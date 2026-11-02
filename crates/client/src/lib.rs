//! SDK client primitives for pohunek.

#![forbid(unsafe_code)]

mod discovery;
mod error;
mod integration;
mod notifications;
mod overlay_config;
mod transport;

#[doc(inline)]
pub use discovery::{
    discover_hosts, discover_hosts_with_options, DiscoveryOptions, DEFAULT_DISCOVERY_DEADLINE,
    DEFAULT_PROBE_CONCURRENCY, DEFAULT_PROBE_TIMEOUT, DISCOVERY_CACHE_TTL,
    DISCOVERY_LOCK_WAIT_MARGIN, MAX_DISCOVERY_DEADLINE,
};
pub use error::ClientError;
pub use overlay::{
    ConfiguredTransport, ExternalIdentity, ExternalIdentityKind, HostTarget, LocalIdentity,
    OverlayDiagnostic, OverlayRegistry, RegistryError,
};
pub use overlay_config::{
    default_overlay_registry, diagnose_overlays, overlay_registry_from, OverlayConfig,
    DEFAULT_REMOTE_PORT, OVERLAY_CONFIG_FILE, REMOTE_PORT_ENV,
};
pub use protocol;
pub use transport::{
    attach_raw, attach_raw_local, attach_raw_local_with_options, attach_raw_tcp_addr,
    attach_raw_tcp_addr_with_options, attach_raw_with_options, connect_raw, connect_raw_local,
    connect_raw_local_with_options, connect_raw_tcp_addr, connect_raw_tcp_addr_with_options,
    connect_raw_with_options, is_local_host, names_this_machine, names_this_machine_in,
    next_request_id, pinned_host_id, remote_host_with_port, resolve_host_peer_in,
    resolve_host_target, resolve_host_target_in, session_new_request_deadline, Client,
    ClientOptions, OriginSource, RawStream, ResolvedHost, ResolvedPeer, Subscription,
    DEFAULT_REQUEST_TIMEOUT, LIFECYCLE_STEP_BOUND_MS, LOCAL_HOST, RESUME_SWEEP_PASSES,
    SESSION_FORK_REQUEST_BUDGET_MS, SESSION_NEW_REQUEST_BUDGET_MS,
    SESSION_REMOVE_REQUEST_BUDGET_MS, SESSION_RESUME_REQUEST_BUDGET_MS,
    SESSION_STOP_REQUEST_BUDGET_MS, WORKTREE_HOOK_TIMEOUT_MS,
};
