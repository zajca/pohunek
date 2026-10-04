//! `integration.install`, `integration.uninstall`, `integration.status`, and
//! `integration.doctor` agent hook RPC handlers.

// Rust guideline compliant 2026-08-31

use protocol::{
    IntegrationDoctorParams, IntegrationDoctorResult, IntegrationInstallParams,
    IntegrationInstallResult, IntegrationStatusParams, IntegrationStatusResult,
    IntegrationUninstallParams, IntegrationUninstallResult, ProtocolError, Request, Response,
    RuntimeRef,
};

use super::util::{error_value, parse_optional_params, parse_params};
use crate::agent::host::RuntimeHost;

/// Classifies the agent a hook request names against the runtime registry.
///
/// A historical label is `agent_kind_unsupported` and a valid id no enabled
/// runtime backs is `runtime_not_installed`; the integration operations only see
/// installed runtimes.
fn check_agent(runtimes: &RuntimeHost, agent: Option<&RuntimeRef>) -> Result<(), ProtocolError> {
    agent.map_or(Ok(()), |agent| runtimes.resolve_ref(agent).map(|_| ()))
}

pub(super) async fn handle_integration_install(
    request: &Request,
    runtimes: &RuntimeHost,
) -> Response {
    let params = match parse_params::<IntegrationInstallParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    if let Err(err) = check_agent(runtimes, params.agent.as_ref()) {
        return error_value(request, err);
    }
    run_integration_install_blocking(request, move || crate::integration::install(params.agent))
        .await
}

pub(super) async fn handle_integration_uninstall(
    request: &Request,
    runtimes: &RuntimeHost,
) -> Response {
    let params = match parse_params::<IntegrationUninstallParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    if let Err(err) = check_agent(runtimes, Some(&params.agent)) {
        return error_value(request, err);
    }
    run_integration_uninstall_blocking(request, move || {
        crate::integration::uninstall(&params.agent)
    })
    .await
}

pub(super) async fn handle_integration_doctor(
    request: &Request,
    runtimes: &RuntimeHost,
) -> Response {
    let params = match parse_optional_params::<IntegrationDoctorParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    if let Err(err) = check_agent(runtimes, params.agent.as_ref()) {
        return error_value(request, err);
    }
    run_integration_doctor_blocking(request, move || crate::integration::doctor(params)).await
}

pub(super) async fn handle_integration_status(
    request: &Request,
    runtimes: &RuntimeHost,
) -> Response {
    let params = match parse_optional_params::<IntegrationStatusParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    if let Err(err) = check_agent(runtimes, params.agent.as_ref()) {
        return error_value(request, err);
    }
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

/// Run integration removal off the Tokio request task and map a task panic to
/// a typed daemon error.
pub(super) async fn run_integration_uninstall_blocking<F>(request: &Request, op: F) -> Response
where
    F: FnOnce() -> Result<IntegrationUninstallResult, ProtocolError> + Send + 'static,
{
    super::util::run_blocking(
        request,
        op,
        "integration_uninstall_task_panicked",
        "integration removal task panicked",
        Some("retry the request; if it repeats, inspect daemon logs"),
    )
    .await
}

/// Run integration diagnosis off the Tokio request task and map a task panic
/// to a typed daemon error.
pub(super) async fn run_integration_doctor_blocking<F>(request: &Request, op: F) -> Response
where
    F: FnOnce() -> Result<IntegrationDoctorResult, ProtocolError> + Send + 'static,
{
    super::util::run_blocking(
        request,
        op,
        "integration_doctor_task_panicked",
        "integration diagnosis task panicked",
        Some("retry the request; if it repeats, inspect daemon logs"),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(wire: &str) -> Result<(), ProtocolError> {
        check_agent(&RuntimeHost::default(), Some(&RuntimeRef::from_wire(wire)))
    }

    #[test]
    fn an_absent_agent_and_the_installed_runtimes_pass() {
        check_agent(&RuntimeHost::default(), None).expect("no agent selected");
        for wire in ["codex", "claude", "hermes", "shell"] {
            classify(wire).unwrap_or_else(|error| panic!("{wire} is installed: {error:?}"));
        }
    }

    #[test]
    fn a_valid_id_no_runtime_backs_is_not_installed() {
        assert_eq!(
            classify("acme").expect_err("not installed").code,
            "runtime_not_installed"
        );
    }

    #[test]
    fn a_grammar_invalid_value_is_unsupported() {
        for wire in ["Not An Id", "", "a/b"] {
            assert_eq!(
                classify(wire).expect_err("historical").code,
                "agent_kind_unsupported",
                "{wire:?}"
            );
        }
    }
}
