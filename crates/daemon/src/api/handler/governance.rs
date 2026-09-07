//! Safe owner-only host-governance inspection.

use std::sync::Arc;

use protocol::{ErrorClass, ProtocolError, Request, Response};

use super::util::{error_value, ok_value, parse_params};
use crate::governance::HostGovernanceService;

const GOVERNANCE_UNAVAILABLE_CODE: &str = "host_governance_unavailable";
const GOVERNANCE_UNAVAILABLE_MESSAGE: &str = "host governance status is unavailable";
const GOVERNANCE_UNAVAILABLE_RECOVERY: &str = "reload or restart the daemon, then retry";

/// `host.governance.inspect`: return the immutable safe governance snapshot.
///
/// The shared service owns the durable repository and returns only the public
/// [`protocol::HostGovernanceStatus`] projection. This handler deliberately
/// does not receive a signer, durable outcome, or filesystem coordinate.
pub(super) async fn handle_host_governance_inspect(
    request: &Request,
    governance: &Arc<HostGovernanceService>,
) -> Response {
    if let Err(error) = parse_params::<()>(request) {
        return error_value(request, error);
    }

    match governance.inspect().await {
        Ok(status) => ok_value(request, &status),
        Err(_error) => error_value(
            request,
            ProtocolError::new(
                ErrorClass::Daemon,
                GOVERNANCE_UNAVAILABLE_CODE,
                GOVERNANCE_UNAVAILABLE_MESSAGE,
                Some(GOVERNANCE_UNAVAILABLE_RECOVERY.to_owned()),
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use protocol::{
        method, EnrollmentInfo, EnrollmentRevision, EnrollmentStatus, HostGovernanceStatus,
        HostOwner, OwnerRevision, PrincipalId, QuarantineReason, RelayId, Request,
        GOVERNANCE_ID_PAYLOAD_BYTES,
    };
    use serde_json::{json, Value};

    use super::handle_host_governance_inspect;
    use crate::api::{handle_request, DaemonState, HealthInfo};
    use crate::governance::HostGovernanceService;
    use crate::host_state::records::GovernanceState;
    use crate::session::SessionRegistry;

    fn governance_id(prefix: &str) -> String {
        format!("{prefix}{}", "A".repeat(GOVERNANCE_ID_PAYLOAD_BYTES))
    }

    fn state(governance: Arc<HostGovernanceService>) -> DaemonState {
        DaemonState::new(
            HealthInfo::new("test"),
            SessionRegistry::default(),
            governance,
            crate::test_support::overlay_registry(),
        )
    }

    async fn replace_status(
        governance: &HostGovernanceService,
        status: HostGovernanceStatus,
        quarantine_origin: Option<EnrollmentStatus>,
    ) {
        governance
            .mutate_locked(move |repository| {
                let expected = repository.governance_state().clone();
                let replacement = match quarantine_origin {
                    Some(origin) => {
                        GovernanceState::with_quarantine_origin(status, Some(origin), None, None)
                    }
                    None => GovernanceState::new(status, None, None),
                }
                .expect("valid public governance state remains persistable");
                repository
                    .replace_governance(&expected, replacement)
                    .map_err(crate::governance::HostGovernanceError::Persistence)
            })
            .await
            .expect("replace real governance repository state");
    }

    fn request(id: &str, params: Value) -> Request {
        Request::new(id, method::HOST_GOVERNANCE_INSPECT, params)
            .expect("valid governance inspection request")
    }

    #[tokio::test]
    async fn inspection_returns_explicit_absence_before_any_enrollment() {
        let governance = Arc::new(HostGovernanceService::open_test());
        let response =
            handle_request(&request("never-enrolled", Value::Null), &state(governance)).await;
        let value = response
            .into_result()
            .expect("governance inspection succeeds");

        assert!(value["host_id"].is_string());
        assert!(value["approval_key_reference"].is_string());
        assert_eq!(value["enrollment"], Value::Null);
        assert_eq!(value["owner"], Value::Null);
        assert_eq!(value["owner_revision"], Value::Null);
        assert_eq!(value["quarantine"], Value::Null);
    }

    #[tokio::test]
    async fn inspection_returns_real_enrolled_and_quarantined_snapshots() {
        let governance = Arc::new(HostGovernanceService::open_test());
        let initial = governance
            .status()
            .expect("read initial real governance state");
        let relay = RelayId::parse(&governance_id("relay_")).expect("canonical relay id");
        let owner = HostOwner::Principal(
            PrincipalId::parse(&governance_id("principal_")).expect("canonical principal id"),
        );
        let enrolled = HostGovernanceStatus::new(
            initial.host_id().clone(),
            Some(EnrollmentInfo::new(
                relay.clone(),
                EnrollmentStatus::Active,
                EnrollmentRevision::new(1).expect("nonzero enrollment revision"),
            )),
            Some(owner.clone()),
            Some(OwnerRevision::new(1).expect("nonzero owner revision")),
            None,
            initial.approval_key_reference().clone(),
        )
        .expect("consistent enrolled governance status");
        replace_status(&governance, enrolled, None).await;

        let enrolled_value =
            handle_host_governance_inspect(&request("enrolled", Value::Null), &governance)
                .await
                .into_result()
                .expect("enrolled governance inspection succeeds");
        assert_eq!(enrolled_value["enrollment"]["relay_id"], json!(relay));
        assert_eq!(enrolled_value["enrollment"]["status"], "active");
        assert_eq!(enrolled_value["owner"], json!(owner));
        assert_eq!(enrolled_value["owner_revision"], "1");
        assert_eq!(enrolled_value["quarantine"], Value::Null);

        let quarantined = HostGovernanceStatus::new(
            initial.host_id().clone(),
            Some(EnrollmentInfo::new(
                relay,
                EnrollmentStatus::Quarantined,
                EnrollmentRevision::new(2).expect("nonzero enrollment revision"),
            )),
            Some(owner),
            Some(OwnerRevision::new(1).expect("nonzero owner revision")),
            Some(QuarantineReason::HostIdentityClone),
            initial.approval_key_reference().clone(),
        )
        .expect("consistent quarantined governance status");
        replace_status(&governance, quarantined, Some(EnrollmentStatus::Active)).await;

        let quarantined_value =
            handle_host_governance_inspect(&request("quarantined", Value::Null), &governance)
                .await
                .into_result()
                .expect("quarantined governance inspection succeeds");
        assert_eq!(quarantined_value["enrollment"]["status"], "quarantined");
        assert_eq!(quarantined_value["quarantine"], "host_identity_clone");
    }

    #[tokio::test]
    async fn inspection_rejects_every_non_null_parameter_value() {
        let governance = Arc::new(HostGovernanceService::open_test());
        let state = state(governance);

        for (index, params) in [
            json!({}),
            json!([]),
            json!("unexpected"),
            json!(1),
            json!(true),
        ]
        .into_iter()
        .enumerate()
        {
            let response =
                handle_request(&request(&format!("invalid-{index}"), params), &state).await;
            let error = response
                .into_result()
                .expect_err("non-null params are rejected");
            assert_eq!(error.code, "bad_request");
        }
    }
}
