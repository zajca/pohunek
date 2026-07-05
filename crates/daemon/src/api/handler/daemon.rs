//! Daemon-scoped methods: `daemon.health` and `daemon.doctor`.

use protocol::{ProtocolError, Request, Response, PROTOCOL_VERSION};

use super::util::ok_value;
use super::HealthInfo;

/// `daemon.health`: report daemon version + protocol version.
pub(super) fn handle_health(request: &Request, health: &HealthInfo) -> Response {
    ok_value(
        request,
        &protocol::DaemonHealthResult {
            status: "ok".to_owned(),
            daemon_version: health.daemon_version.clone(),
            protocol_version: PROTOCOL_VERSION,
        },
    )
}

/// `daemon.doctor`: run host-side self-checks off the async runtime.
pub(super) async fn handle_daemon_doctor(request: &Request) -> Response {
    if !request.params.is_null() {
        return Response::err(
            request.id.clone(),
            ProtocolError::bad_request("daemon.doctor does not accept params"),
        );
    }
    let paths = match crate::Paths::resolve() {
        Ok(paths) => paths,
        Err(err) => {
            return Response::err(
                request.id.clone(),
                ProtocolError::new(
                    protocol::ErrorClass::Configuration,
                    "paths_unavailable",
                    format!("failed to resolve daemon paths: {err}"),
                    Some("set the required XDG environment variables and retry".to_owned()),
                ),
            );
        }
    };
    match tokio::task::spawn_blocking(move || crate::doctor::report(&paths)).await {
        Ok(report) => ok_value(request, &protocol::DaemonDoctorResult { report }),
        Err(_) => Response::err(
            request.id.clone(),
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "doctor_task_panicked",
                "daemon doctor task panicked".to_owned(),
                Some("retry the request; if it repeats, inspect daemon logs".to_owned()),
            ),
        ),
    }
}
