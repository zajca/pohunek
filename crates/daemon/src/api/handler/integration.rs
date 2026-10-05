//! `integration.install`, `integration.uninstall`, `integration.status`, and
//! `integration.doctor` agent hook RPC handlers.

// Rust guideline compliant 2026-10-05

use protocol::{
    ErrorClass, IntegrationDoctorParams, IntegrationDoctorResult, IntegrationInstallParams,
    IntegrationStatusParams, IntegrationStatusResult, IntegrationUninstallParams,
    IntegrationUninstallResult, ProtocolError, Request, Response, RuntimeRef,
};

use super::util::{error_value, parse_optional_params, parse_params};
use super::{ControlTransport, DaemonState};
use crate::agent::host::RuntimeHost;
use crate::integration::{ConfigHomes, HomeSelection};

/// Error code of a profile-aware integration request on a remote connection.
pub(crate) const PROFILE_LOCAL_ONLY_CODE: &str = "local_only_method";

/// Classifies the agent a hook request names against the runtime registry.
///
/// A historical label is `agent_kind_unsupported` and a valid id no enabled
/// runtime backs is `runtime_not_installed`; the integration operations only see
/// installed runtimes.
fn check_agent(runtimes: &RuntimeHost, agent: Option<&RuntimeRef>) -> Result<(), ProtocolError> {
    agent.map_or(Ok(()), |agent| {
        let definition = runtimes.resolve_ref(agent)?;
        runtimes.verify_launchable(&definition)
    })
}

/// Refuses a profile-aware request that did not arrive on the local control
/// socket.
///
/// Reports, errors and installed paths name directories derived from a
/// profile's environment, which can hold secrets, so only the owner at the
/// daemon host may select a profile's home.
fn require_local_for_profiles(
    request: &Request,
    state: &DaemonState,
    profile_aware: bool,
) -> Result<(), Response> {
    match (profile_aware, state.transport) {
        (false, _) | (true, ControlTransport::Local) => Ok(()),
        (true, ControlTransport::Remote) => Err(error_value(
            request,
            ProtocolError::new(
                ErrorClass::Daemon,
                PROFILE_LOCAL_ONLY_CODE,
                "profile and all_profiles select config homes derived from profile environments and are served on the local control socket only",
                Some("run the command on the host that runs the daemon".to_owned()),
            ),
        )),
    }
}

/// Resolves the config homes of this daemon, or answers with the error.
fn homes_of(request: &Request, state: &DaemonState) -> Result<ConfigHomes, Response> {
    state
        .sessions
        .integration_homes()
        .map_err(|error| error_value(request, error))
}

pub(super) async fn handle_integration_install(request: &Request, state: &DaemonState) -> Response {
    let params = match parse_params::<IntegrationInstallParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let profile_aware = params.profile.is_some() || params.all_profiles;
    if let Err(response) = require_local_for_profiles(request, state, profile_aware) {
        return response;
    }
    let sessions = &state.sessions;
    let runtimes = sessions.profiles().runtimes().clone();
    if let Err(err) = check_agent(&runtimes, params.agent.as_ref()) {
        return error_value(request, err);
    }
    let selection = match HomeSelection::from_params(params.profile, params.all_profiles) {
        Ok(selection) => selection,
        Err(err) => return error_value(request, err),
    };
    let homes = match homes_of(request, state) {
        Ok(homes) => homes,
        Err(response) => return response,
    };
    // The update runs under the shared lifecycle guard, owned by a task the
    // caller's disconnect cannot cancel.
    let agent = params.agent;
    match sessions
        .integration_install(move |retained| {
            crate::integration::install_in(&homes, agent.as_ref(), &selection, retained)
        })
        .await
    {
        Ok(result) => super::util::ok_value(request, &result),
        Err(err) => error_value(request, err),
    }
}

pub(super) async fn handle_integration_uninstall(
    request: &Request,
    state: &DaemonState,
) -> Response {
    let params = match parse_params::<IntegrationUninstallParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let profile_aware = params.profile.is_some() || params.all_profiles;
    if let Err(response) = require_local_for_profiles(request, state, profile_aware) {
        return response;
    }
    let runtimes = state.sessions.profiles().runtimes().clone();
    if let Err(err) = check_agent(&runtimes, Some(&params.agent)) {
        return error_value(request, err);
    }
    let selection = match HomeSelection::from_params(params.profile, params.all_profiles) {
        Ok(selection) => selection,
        Err(err) => return error_value(request, err),
    };
    let homes = match homes_of(request, state) {
        Ok(homes) => homes,
        Err(response) => return response,
    };
    let agent = params.agent;
    run_integration_uninstall_blocking(request, move || {
        crate::integration::uninstall_in(&homes, &agent, &selection)
    })
    .await
}

pub(super) async fn handle_integration_doctor(request: &Request, state: &DaemonState) -> Response {
    let params = match parse_optional_params::<IntegrationDoctorParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let profile_aware = params.profile.is_some() || params.all_profiles;
    if let Err(response) = require_local_for_profiles(request, state, profile_aware) {
        return response;
    }
    let runtimes = state.sessions.profiles().runtimes().clone();
    if let Err(err) = check_agent(&runtimes, params.agent.as_ref()) {
        return error_value(request, err);
    }
    let homes = match homes_of(request, state) {
        Ok(homes) => homes,
        Err(response) => return response,
    };
    run_integration_doctor_blocking(request, move || {
        crate::integration::doctor_in(&homes, params)
    })
    .await
}

pub(super) async fn handle_integration_status(request: &Request, state: &DaemonState) -> Response {
    let params = match parse_optional_params::<IntegrationStatusParams>(request) {
        Ok(params) => params,
        Err(err) => return error_value(request, err),
    };
    let profile_aware = params.profile.is_some() || params.all_profiles;
    if let Err(response) = require_local_for_profiles(request, state, profile_aware) {
        return response;
    }
    let runtimes = state.sessions.profiles().runtimes().clone();
    if let Err(err) = check_agent(&runtimes, params.agent.as_ref()) {
        return error_value(request, err);
    }
    let homes = match homes_of(request, state) {
        Ok(homes) => homes,
        Err(response) => return response,
    };
    run_integration_status_blocking(request, move || {
        crate::integration::status_in(&homes, params)
    })
    .await
}

/// Run integration installation off the Tokio request task.
///
/// Test helper for the join path; the handler runs the install through
/// `SessionRegistry::integration_install`.
///
/// A blocking-task panic becomes a typed daemon error. The helper is exposed to
/// handler tests that assert the join path.
#[cfg(test)]
pub(super) async fn run_integration_install_blocking<F>(request: &Request, op: F) -> Response
where
    F: FnOnce() -> Result<protocol::IntegrationInstallResult, ProtocolError> + Send + 'static,
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
    use std::sync::Arc;

    use protocol::method;
    use serde_json::{json, Value};

    use super::*;
    use crate::api::handler::{handle_request, HealthInfo};
    use crate::governance::HostGovernanceService;
    use crate::session::SessionRegistry;

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
    fn a_package_runtime_modified_after_install_is_refused_before_any_integration_change() {
        let dir = pohunek_test_support::tempdir().expect("private test directory");
        let plugins = dir.path().join("plugins");
        let (host, digest) = crate::agent::host::fixture::installed_pi_host(
            &plugins,
            std::path::Path::new("/bin/sh"),
        );
        let agent = RuntimeRef::from_wire("pi");
        check_agent(&host, Some(&agent)).expect("an intact package passes");

        let hex = digest
            .as_str()
            .strip_prefix("sha256:")
            .expect("digest prefix");
        let descriptor = plugins
            .join("packages")
            .join(hex)
            .join("files")
            .join("runtime.toml");
        let mut bytes = std::fs::read(&descriptor).expect("read descriptor");
        bytes.extend_from_slice(b"\n# tampered\n");
        std::fs::write(&descriptor, bytes).expect("write descriptor");

        assert_eq!(
            check_agent(&host, Some(&agent))
                .expect_err("a modified package")
                .code,
            "runtime_incompatible"
        );
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

    async fn state(transport: ControlTransport) -> (DaemonState, tempfile::TempDir) {
        let dir = pohunek_test_support::tempdir().expect("private test directory");
        let governance = Arc::new(
            HostGovernanceService::open(dir.path().join("state"))
                .await
                .expect("open the governance service"),
        );
        let state = DaemonState::new(
            HealthInfo::new("test"),
            SessionRegistry::default(),
            governance,
            crate::test_support::overlay_registry(),
        )
        .with_transport(transport);
        (state, dir)
    }

    async fn error_code(state: &DaemonState, method: &str, params: Value) -> String {
        let request = Request::new("integration-transport", method, params).expect("request");
        handle_request(&request, state)
            .await
            .into_result()
            .expect_err("every call here fails")
            .code
    }

    /// Requests that select a profile home, one per integration method.
    fn profile_requests() -> Vec<(&'static str, Value)> {
        vec![
            (method::INTEGRATION_INSTALL, json!({ "profile": "work" })),
            (method::INTEGRATION_INSTALL, json!({ "all_profiles": true })),
            (method::INTEGRATION_STATUS, json!({ "profile": "work" })),
            (method::INTEGRATION_STATUS, json!({ "all_profiles": true })),
            (method::INTEGRATION_DOCTOR, json!({ "profile": "work" })),
            (method::INTEGRATION_DOCTOR, json!({ "all_profiles": true })),
            (
                method::INTEGRATION_UNINSTALL,
                json!({ "agent": "claude", "profile": "work" }),
            ),
            (
                method::INTEGRATION_UNINSTALL,
                json!({ "agent": "claude", "all_profiles": true }),
            ),
        ]
    }

    #[tokio::test]
    async fn a_remote_connection_cannot_select_a_profile_home() {
        let (state, _dir) = state(ControlTransport::Remote).await;
        for (method, params) in profile_requests() {
            assert_eq!(
                error_code(&state, method, params.clone()).await,
                PROFILE_LOCAL_ONLY_CODE,
                "{method} {params}"
            );
        }
    }

    #[tokio::test]
    async fn a_local_connection_reaches_the_profile_selection() {
        let (state, _dir) = state(ControlTransport::Local).await;
        for (method, params) in profile_requests()
            .into_iter()
            .filter(|(_method, params)| params.get("profile").is_some())
        {
            assert_eq!(
                error_code(&state, method, params.clone()).await,
                "agent_profile_not_found",
                "{method} {params}"
            );
        }
    }

    #[tokio::test]
    async fn a_remote_connection_still_serves_requests_without_a_selector() {
        let (state, _dir) = state(ControlTransport::Remote).await;
        // The shell has no daemon-run handler, so the request passes the
        // transport gate and fails in the lifecycle with its own typed error.
        for (method, params) in [
            (method::INTEGRATION_INSTALL, json!({ "agent": "shell" })),
            (method::INTEGRATION_STATUS, json!({ "agent": "shell" })),
            (method::INTEGRATION_DOCTOR, json!({ "agent": "shell" })),
            (method::INTEGRATION_UNINSTALL, json!({ "agent": "shell" })),
        ] {
            assert_eq!(
                error_code(&state, method, params).await,
                "agent_not_installable",
                "{method}"
            );
        }
    }

    #[tokio::test]
    async fn both_selectors_together_are_a_bad_request() {
        let (state, _dir) = state(ControlTransport::Local).await;
        for method in [
            method::INTEGRATION_INSTALL,
            method::INTEGRATION_STATUS,
            method::INTEGRATION_DOCTOR,
        ] {
            assert_eq!(
                error_code(
                    &state,
                    method,
                    json!({ "agent": "claude", "profile": "work", "all_profiles": true })
                )
                .await,
                "bad_request",
                "{method}"
            );
        }
    }
}
