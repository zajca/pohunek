//! `integration.install` and `integration.status` agent hook RPC handlers.

// Rust guideline compliant 2026-08-31

use protocol::{
    IntegrationInstallParams, IntegrationInstallResult, IntegrationStatusParams,
    IntegrationStatusResult, ProtocolError, Request, Response,
};

use super::util::{error_value, parse_optional_params, parse_params};

pub(super) async fn handle_integration_install(request: &Request) -> Response {
    let params = match parse_params::<IntegrationInstallParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    run_integration_install_blocking(request, move || crate::integration::install(params.agent))
        .await
}

pub(super) async fn handle_integration_status(request: &Request) -> Response {
    let params = match parse_optional_params::<IntegrationStatusParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    run_integration_status_blocking(request, move || crate::integration::status(params)).await
}

/// Run integration installation off the Tokio request task.
///
/// A blocking-task panic becomes a typed daemon error. The helper is exposed to
/// handler tests that assert the join path.
pub(super) async fn run_integration_install_blocking<F>(request: &Request, op: F) -> Response
where
    F: FnOnce() -> Result<IntegrationInstallResult, ProtocolError> + Send + 'static,
{
    super::util::run_blocking(
        request,
        op,
        "integration_install_task_panicked",
        "integration installation task panicked",
        Some("retry the request; if it repeats, inspect daemon logs"),
    )
    .await
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
