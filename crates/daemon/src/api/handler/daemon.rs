//! Daemon-scoped methods: `daemon.health` and `daemon.doctor`.

use overlay::OverlayRegistry;
use protocol::{ProtocolError, Request, Response};

use super::util::{error_value, ok_value, selected_version};
use super::ControlTransport;
use super::HealthInfo;
use crate::governance::HostGovernanceService;
use crate::session::SessionRegistry;

/// `daemon.health`: report daemon version + the protocol version of this connection.
pub(super) fn handle_health(request: &Request, health: &HealthInfo) -> Response {
    ok_value(
        request,
        &protocol::DaemonHealthResult {
            status: "ok".to_owned(),
            daemon_version: health.daemon_version.clone(),
            protocol_version: selected_version(request),
        },
    )
}

/// `daemon.doctor`: run host-side self-checks off the async runtime.
pub(super) async fn handle_daemon_doctor(
    request: &Request,
    governance: &HostGovernanceService,
    sessions: &SessionRegistry,
    overlays: &OverlayRegistry,
    overlays_from_file: bool,
    transport: ControlTransport,
) -> Response {
    if !request.params().is_null() {
        return error_value(
            request,
            ProtocolError::bad_request("daemon.doctor does not accept params"),
        );
    }
    let paths = match crate::Paths::resolve() {
        Ok(paths) => paths,
        Err(err) => {
            return error_value(
                request,
                ProtocolError::new(
                    protocol::ErrorClass::Configuration,
                    "paths_unavailable",
                    format!("failed to resolve daemon paths: {err}"),
                    Some("set the required XDG environment variables and retry".to_owned()),
                ),
            );
        }
    };
    let overlay_diagnostics =
        pohunek_client::diagnose_overlays(overlays, hostcheck::OVERLAY_PROBE_BUDGET).await;
    match crate::doctor::report(
        &paths,
        governance,
        sessions.active_supervision(),
        sessions.catalog_trust(),
        sessions.profiles(),
        sessions.inspect_base_environment(),
        hostcheck::OverlayChecks {
            diagnostics: &overlay_diagnostics,
            from_file: overlays_from_file,
        },
        transport == ControlTransport::Local,
        sessions.rejected_lines_state().await,
    )
    .await
    {
        Ok(report) => ok_value(request, &protocol::DaemonDoctorResult { report }),
        Err(_error) => error_value(
            request,
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "doctor_task_panicked",
                "daemon doctor task panicked".to_owned(),
                Some("retry the request; if it repeats, inspect daemon logs".to_owned()),
            ),
        ),
    }
}
