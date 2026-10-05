//! `package.list`, `package.inspect`, `package.doctor`, `package.install`,
//! `package.link`, `package.set_enabled`, `package.select`,
//! `package.uninstall` and `package.bind_profile`: the runtime package lifecycle.
//!
//! The methods extend the owner's launch authority on the host, so they are
//! served on the local control socket only. Each handler refuses a remote
//! connection before it parses a parameter or touches the filesystem.

// Rust guideline compliant 2026-10-04

use protocol::{
    PackageBindProfileParams, PackageDoctorParams, PackageErrorKind, PackageInspectParams,
    PackageInstallParams, PackageLinkParams, PackageSelectParams, PackageSetEnabledParams,
    PackageUninstallParams, ProtocolError, Request, Response,
};

use super::util::{error_value, ok_value, parse_optional_params, parse_params};
use super::{ControlTransport, DaemonState};

/// Refuses a request that did not arrive on the local control socket.
fn require_local(request: &Request, state: &DaemonState) -> Result<(), Response> {
    match state.transport {
        ControlTransport::Local => Ok(()),
        ControlTransport::Remote => Err(error_value(
            request,
            ProtocolError::from(PackageErrorKind::LocalOnly),
        )),
    }
}

/// Serializes a lifecycle result or maps its error kind to the wire error.
fn respond<T: serde::Serialize>(
    request: &Request,
    result: Result<T, PackageErrorKind>,
) -> Response {
    match result {
        Ok(value) => ok_value(request, &value),
        Err(kind) => error_value(request, ProtocolError::from(kind)),
    }
}

pub(super) async fn handle_package_list(request: &Request, state: &DaemonState) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    respond(request, state.sessions.package_list().await)
}

pub(super) async fn handle_package_inspect(request: &Request, state: &DaemonState) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    match parse_params::<PackageInspectParams>(request) {
        Ok(params) => respond(request, state.sessions.package_inspect(params.digest).await),
        Err(error) => error_value(request, error),
    }
}

pub(super) async fn handle_package_doctor(request: &Request, state: &DaemonState) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    match parse_optional_params::<PackageDoctorParams>(request) {
        Ok(params) => respond(request, state.sessions.package_doctor(params.package).await),
        Err(error) => error_value(request, error),
    }
}

pub(super) async fn handle_package_install(request: &Request, state: &DaemonState) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    match parse_params::<PackageInstallParams>(request) {
        Ok(params) => respond(request, state.sessions.package_install(params).await),
        Err(error) => error_value(request, error),
    }
}

pub(super) async fn handle_package_link(request: &Request, state: &DaemonState) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    match parse_params::<PackageLinkParams>(request) {
        Ok(params) => respond(request, state.sessions.package_link(params).await),
        Err(error) => error_value(request, error),
    }
}

pub(super) async fn handle_package_set_enabled(request: &Request, state: &DaemonState) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    match parse_params::<PackageSetEnabledParams>(request) {
        Ok(params) => respond(request, state.sessions.package_set_enabled(params).await),
        Err(error) => error_value(request, error),
    }
}

pub(super) async fn handle_package_select(request: &Request, state: &DaemonState) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    match parse_params::<PackageSelectParams>(request) {
        Ok(params) => respond(request, state.sessions.package_select(params).await),
        Err(error) => error_value(request, error),
    }
}

pub(super) async fn handle_package_uninstall(request: &Request, state: &DaemonState) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    match parse_params::<PackageUninstallParams>(request) {
        Ok(params) => respond(request, state.sessions.package_uninstall(params).await),
        Err(error) => error_value(request, error),
    }
}

pub(super) async fn handle_package_bind_profile(
    request: &Request,
    state: &DaemonState,
) -> Response {
    if let Err(response) = require_local(request, state) {
        return response;
    }
    match parse_params::<PackageBindProfileParams>(request) {
        Ok(params) => respond(request, state.sessions.package_bind_profile(params).await),
        Err(error) => error_value(request, error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use protocol::{method, Request};
    use serde_json::Value;

    use super::super::{handle_request, ControlTransport, DaemonState, HealthInfo};
    use crate::governance::HostGovernanceService;
    use crate::session::SessionRegistry;

    const PACKAGE_METHODS: [&str; 9] = [
        method::PACKAGE_LIST,
        method::PACKAGE_INSPECT,
        method::PACKAGE_DOCTOR,
        method::PACKAGE_INSTALL,
        method::PACKAGE_LINK,
        method::PACKAGE_SET_ENABLED,
        method::PACKAGE_SELECT,
        method::PACKAGE_UNINSTALL,
        method::PACKAGE_BIND_PROFILE,
    ];

    async fn state() -> (DaemonState, tempfile::TempDir) {
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
        );
        (state, dir)
    }

    async fn code(state: &DaemonState, method: &str) -> String {
        let request = Request::new("package-transport", method, Value::Null).expect("request");
        handle_request(&request, state)
            .await
            .into_result()
            .expect_err("every call is refused here")
            .code
    }

    #[tokio::test]
    async fn a_remote_state_refuses_every_package_method_before_reading_parameters() {
        let (state, _dir) = state().await;
        let remote = state.with_transport(ControlTransport::Remote);
        for name in PACKAGE_METHODS {
            assert_eq!(code(&remote, name).await, "local_only_method", "{name}");
        }
    }

    #[tokio::test]
    async fn a_local_state_serves_package_methods() {
        let (state, _dir) = state().await;
        assert_eq!(state.transport, ControlTransport::Local);
        // The default registry has no package store, so the call gets past the
        // transport check and fails on the registry instead.
        assert_eq!(
            code(&state, method::PACKAGE_LIST).await,
            "package_registry_failed"
        );
    }
}
