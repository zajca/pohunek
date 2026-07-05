//! Host-scoped methods: `host.inspect` and `host.discover`.

use protocol::{HostDiscoverParams, ProtocolError, Request, Response};

use super::util::{ok_value, parse_optional_params};
use super::HealthInfo;
use crate::discovery::DiscoveryCache;
use crate::session::SessionRegistry;

/// `host.inspect`: report this host's live capability snapshot.
///
/// The snapshot is built fresh on each request (agent runtimes are probed
/// against `PATH`), so it always reflects the host as it is now. Transport
/// agnostic: the same handler answers over the local Unix socket and over a
/// `NetBird` TCP connection.
pub(super) fn handle_host_inspect(
    request: &Request,
    health: &HealthInfo,
    sessions: &SessionRegistry,
) -> Response {
    ok_value(
        request,
        &crate::capabilities::host_capabilities(&health.daemon_version, sessions.profiles()),
    )
}

/// `host.discover`: enumerate `NetBird` peers and classify each daemon.
///
/// The probe is run inside the daemon and cached for a short TTL (see
/// [`DiscoveryCache`]), so repeated calls — e.g. every launcher keypress —
/// return the cached snapshot instantly; `force` bypasses the cache and
/// re-probes now. A NetBird-state failure surfaces as a typed
/// `discovery/netbird_state_unavailable` error rather than an empty result.
pub(super) async fn handle_host_discover(
    request: &Request,
    discovery: &DiscoveryCache,
) -> Response {
    let params = match parse_optional_params::<HostDiscoverParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match discovery.records(params.force).await {
        Ok(records) => ok_value(request, &records),
        Err(err) => Response::err(
            request.id.clone(),
            ProtocolError::netbird_state_unavailable(err.to_string()),
        ),
    }
}
