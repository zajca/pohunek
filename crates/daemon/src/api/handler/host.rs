//! Host-scoped methods: `host.inspect` and `host.discover`.

use protocol::{HostDiscoverParams, Request, Response};

use super::util::error_value;

use super::util::{ok_value, parse_optional_params, selected_version};
use super::HealthInfo;
use crate::discovery::DiscoveryCache;
use crate::session::SessionRegistry;

/// `host.inspect`: report this host's live capability snapshot.
///
/// The snapshot is built fresh on each request (agent runtimes are probed
/// against `PATH`), so it always reflects the host as it is now. Transport
/// agnostic: the same handler answers over the local Unix socket and over a
/// `NetBird` TCP connection. The reported protocol version is the one this
/// connection negotiated.
pub(super) fn handle_host_inspect(
    request: &Request,
    health: &HealthInfo,
    sessions: &SessionRegistry,
) -> Response {
    let mut capabilities = crate::capabilities::host_capabilities(
        &health.daemon_version,
        sessions.profiles(),
        &sessions.inspect_base_environment(),
    );
    capabilities.protocol_version = selected_version(request);
    ok_value(request, &capabilities)
}

/// `host.discover`: enumerate `NetBird` peers and classify each daemon.
///
/// The daemon caches the shared discovery engine for a short TTL (see
/// [`DiscoveryCache`]), so repeated calls return promptly; `force` bypasses
/// that cache. Peers are probed and classified for the version negotiated with
/// the asking connection, and cached per version. Discovery errors retain their typed protocol mapping rather than
/// being represented as an empty peer list.
pub(super) async fn handle_host_discover(
    request: &Request,
    discovery: &DiscoveryCache,
) -> Response {
    let params = match parse_optional_params::<HostDiscoverParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    match discovery
        .records(params.force, selected_version(request))
        .await
    {
        Ok(records) => ok_value(request, &records),
        Err(err) => error_value(request, err.to_protocol_error()),
    }
}
