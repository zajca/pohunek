//! `integration.install` and `integration.status` agent hook RPC handlers.

use protocol::{
    IntegrationInstallParams, IntegrationStatusParams, IntegrationStatusResult, ProtocolError,
    Request, Response,
};

use super::util::error_value;

use super::util::{ok_value, parse_optional_params, parse_params};

pub(super) fn handle_integration_install(request: &Request) -> Response {
    let params = match parse_params::<IntegrationInstallParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    match crate::integration::install(params.agent) {
        Ok(result) => ok_value(request, &result),
        Err(err) => error_value(request, err),
    }
}

pub(super) async fn handle_integration_status(request: &Request) -> Response {
    let params = match parse_optional_params::<IntegrationStatusParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    run_integration_status_blocking(request, move || crate::integration::status(params)).await
}

/// Run filesystem inspection off the Tokio request task and map a task panic to
/// a typed daemon error. Exposed to handler tests that assert the join path.
pub(super) async fn run_integration_status_blocking<F>(request: &Request, op: F) -> Response
where
    F: FnOnce() -> Result<IntegrationStatusResult, ProtocolError> + Send + 'static,
{
    super::util::run_blocking(
        request,
        op,
        "integration_status_task_panicked",
        "integration status inspection task panicked",
        Some("retry the request; if it repeats, inspect daemon logs"),
    )
    .await
}
