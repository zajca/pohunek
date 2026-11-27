//! Host-scoped methods: `host.inspect`, `host.discover` and `host.resolve`.

use protocol::{
    HostDiscoverParams, HostInspectParams, HostResolveParams, ProtocolError, Request, Response,
    MAX_CONTROL_LINE_BYTES,
};

use super::util::error_value;

use super::util::{
    ok_value, ok_value_bounded, parse_optional_params, parse_params, run_blocking, selected_version,
};
use super::{ControlTransport, HealthInfo};
use crate::discovery::DiscoveryCache;
use crate::session::SessionRegistry;

/// Hard count bound on the `host.discover` reply: 1500 peer records.
///
/// The success reply is one control line of at most [`MAX_CONTROL_LINE_BYTES`]
/// (1 MiB), envelope included. One [`protocol::HostRecord`] is capped at build
/// time (see `crates/client/src/discovery.rs`): 93 bytes of keys, quotes and
/// punctuation, `name` at most [`protocol::MAX_HOST_NAME_BYTES`] + 2,
/// `fqdn` at most [`protocol::MAX_HOST_FQDN_BYTES`] + 2, `address` at most 47,
/// `port` at most 5, `overlay` at most 11 (a known provider id), `peer_id` at
/// most [`protocol::MAX_HOST_PEER_ID_BYTES`] + 2, `local` at most 5, `reason`
/// at most [`protocol::MAX_HOST_REASON_BYTES`] + 2, and the flattened class at
/// most 89 bytes (`reachable_daemon` with a capped 32-byte version) — at most
/// 670 bytes, 671 with its separating comma. The bare array adds 2 brackets and
/// the response envelope at most `MAX_SUCCESS_RESPONSE_ENVELOPE_BYTES`
/// (158 bytes). The largest count whose worst case fits is
/// (1 MiB − 158 − 2) / 671 = 1562; the constant is rounded down to 1500, whose
/// worst case 1500 × 671 + 160 = 1,006,660 bytes stays below 1,048,576. A reply
/// that is still too large is refused by the reply's byte-level check with the
/// same `host_discover_too_large` code.
const MAX_HOST_DISCOVER_RECORDS: usize = 1500;

const _: () = assert!(
    MAX_HOST_DISCOVER_RECORDS * 671 + 160 <= MAX_CONTROL_LINE_BYTES,
    "the host.discover count bound must keep its worst-case reply inside one control line"
);

/// `host.inspect`: report this host's live capability snapshot.
///
/// Presence and path of each agent program are resolved against `PATH` on every
/// request; the version probes behind them come from the daemon's probe cache
/// (see [`crate::capabilities::ProbeCache`]). The inventory is built on a
/// blocking thread, so a slow probe never stalls other requests. With
/// `skip_runtime_probes` only the feature flags and the host-global hook
/// scripts are answered and nothing is resolved or probed. Transport agnostic: the same handler answers over the
/// local Unix socket and over an overlay TCP connection. The reported protocol
/// version is the one this connection negotiated.
pub(super) async fn handle_host_inspect(
    request: &Request,
    health: &HealthInfo,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_optional_params::<Option<HostInspectParams>>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let daemon_version = health.daemon_version.clone();
    let protocol_version = selected_version(request);
    // Both answers probe `git` on the snapshot this refresh took.
    let search_path = sessions.refreshed_search_path().await;
    if params.and_then(|params| params.skip_runtime_probes) == Some(true) {
        let mut capabilities =
            crate::capabilities::host_capability_flags(&daemon_version, search_path.as_os_str());
        capabilities.protocol_version = protocol_version;
        capabilities.package_install_index_supported =
            sessions.package_authority().index_install_supported();
        capabilities.package_install_index_context_supported =
            capabilities.package_install_index_supported;
        capabilities.package_index_apply_supported = capabilities.package_install_index_supported;
        capabilities.host_hooks = Some(crate::worktree::host_hook_scripts(sessions.config_dir()));
        capabilities.subscribe_heartbeat = Some(heartbeat_capability(sessions));
        return ok_value(request, &capabilities);
    }
    let sessions = sessions.clone();
    run_blocking(
        request,
        move || {
            let mut capabilities = crate::capabilities::host_capabilities(
                &daemon_version,
                sessions.profiles(),
                &sessions.inspect_base_environment(&search_path),
                search_path.as_os_str(),
                sessions.probe_cache(),
            );
            capabilities.protocol_version = protocol_version;
            capabilities.package_install_index_supported =
                sessions.package_authority().index_install_supported();
            capabilities.package_install_index_context_supported =
                capabilities.package_install_index_supported;
            capabilities.package_index_apply_supported =
                capabilities.package_install_index_supported;
            capabilities.host_hooks =
                Some(crate::worktree::host_hook_scripts(sessions.config_dir()));
            capabilities.subscribe_heartbeat = Some(heartbeat_capability(&sessions));
            Ok(capabilities)
        },
        "host_inspect_task_panicked",
        "host inventory task panicked",
        Some("retry the request; if it repeats, inspect daemon logs"),
    )
    .await
}

/// The `heartbeat_ms` bounds `subscribe` accepts on this daemon.
fn heartbeat_capability(sessions: &SessionRegistry) -> protocol::SubscribeHeartbeatCapability {
    let settings = sessions.subscription_settings();
    protocol::SubscribeHeartbeatCapability {
        min_ms: settings.heartbeat_min_ms,
        max_ms: settings.heartbeat_max_ms,
    }
}

/// `host.discover`: enumerate overlay peers and classify each daemon.
///
/// The daemon caches the shared discovery engine for a short TTL (see
/// [`DiscoveryCache`]), so repeated calls return promptly; `force` bypasses
/// that cache. Peers are probed and classified for the range the asking client
/// advertised (a client keeps its whole window on this method, see
/// [`protocol::compat::RANGE_FORWARDING_METHODS`]), and cached per range. Discovery errors retain their typed protocol mapping rather than
/// being represented as an empty peer list.
///
/// The reply is count-bounded by [`MAX_HOST_DISCOVER_RECORDS`] and itself goes
/// through the control-line check, so an oversized result answers
/// `host_discover_too_large` instead of a frame the client refuses.
pub(super) async fn handle_host_discover(
    request: &Request,
    discovery: &DiscoveryCache,
) -> Response {
    let params = match parse_optional_params::<HostDiscoverParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    match discovery
        .records(params.force, request.version_range())
        .await
    {
        Ok(records) if records.len() > MAX_HOST_DISCOVER_RECORDS => {
            error_value(request, ProtocolError::host_discover_too_large())
        }
        Ok(records) => {
            ok_value_bounded(request, &records, ProtocolError::host_discover_too_large())
        }
        Err(err) => error_value(request, err.to_protocol_error()),
    }
}

/// `host.resolve`: map a host route to this machine or to one pinned peer.
///
/// The answer is about this daemon's own overlay view, so only the local
/// control socket serves it. Over the overlay the method does not exist, as
/// for a method the daemon never defined.
pub(super) async fn handle_host_resolve(
    request: &Request,
    discovery: &DiscoveryCache,
    transport: ControlTransport,
) -> Response {
    if transport != ControlTransport::Local {
        return error_value(request, ProtocolError::method_not_found(request.method()));
    }
    let params = match parse_params::<HostResolveParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    match discovery.resolve(&params.route).await {
        Ok(resolution) => ok_value(request, &resolution),
        Err(err) => error_value(request, err.to_protocol_error()),
    }
}
