//! Daemon-local environment health checks.
//!
//! The host-probe logic is shared with the CLI `doctor` command via the
//! `hostcheck` crate; this module only selects which checks to run on the host
//! that owns the agent runtime and assembles them into a [`DoctorReport`].

use hostcheck::StandardCheckInputs;
use protocol::{DoctorCheck, DoctorReport, DoctorStatus, QuarantineReason};

use crate::governance::{HostGovernanceDiagnostic, HostGovernanceService};
use crate::Paths;

/// Build a daemon-local doctor report on the host that owns the agent runtime.
///
/// # Errors
///
/// Returns `Err(())` only when the bounded blocking host-check task panics.
pub async fn report(paths: &Paths, governance: &HostGovernanceService) -> Result<DoctorReport, ()> {
    let paths = paths.clone();
    let mut checks = tokio::task::spawn_blocking(move || standard_checks(&paths))
        .await
        .map_err(|_error| ())?;
    checks.extend(governance_checks(governance.diagnose().await));
    Ok(DoctorReport::from_checks(checks))
}

fn standard_checks(paths: &Paths) -> Vec<DoctorCheck> {
    let launcher_bin_dir = paths.launcher_bin_dir();
    let sway_config_dir = paths.sway_config_dir();
    hostcheck::standard_checks(StandardCheckInputs {
        socket_dir: &paths.runtime_dir,
        state_dir: &paths.data_dir,
        log_dir: &paths.log_dir,
        launcher_bin_dir: &launcher_bin_dir,
        sway_config_dir: &sway_config_dir,
    })
}

fn governance_checks(diagnostic: HostGovernanceDiagnostic) -> [DoctorCheck; 6] {
    match diagnostic {
        HostGovernanceDiagnostic::Available(status) => {
            let (consistency, quarantine) = quarantine_checks(status.quarantine());
            [
                check(
                    "host_governance_durability",
                    DoctorStatus::Ok,
                    "Stable host governance is durably loaded.",
                ),
                check(
                    "host_identity_stable",
                    DoctorStatus::Ok,
                    "Stable host identity matches the retained durable state.",
                ),
                check(
                    "host_state_private_storage",
                    DoctorStatus::Ok,
                    "Host-state storage remains owner-private and link-safe.",
                ),
                consistency,
                check(
                    "host_approval_key",
                    DoctorStatus::Ok,
                    "Approval signing material remains separate and verifies locally.",
                ),
                quarantine,
            ]
        }
        HostGovernanceDiagnostic::ReloadRequired => unavailable_checks(
            DoctorStatus::Fail,
            "Durable state requires an explicit daemon reload before governance use.",
        ),
        HostGovernanceDiagnostic::IntegrityFailed => unavailable_checks(
            DoctorStatus::Fail,
            "Durable host governance could not be safely revalidated; inspect owner-private storage.",
        ),
        HostGovernanceDiagnostic::Unavailable => unavailable_checks(
            DoctorStatus::Fail,
            "Host governance is unavailable; restart the daemon and retry inspection.",
        ),
    }
}

fn unavailable_checks(status: DoctorStatus, detail: &str) -> [DoctorCheck; 6] {
    [
        check("host_governance_durability", status, detail),
        check("host_identity_stable", status, detail),
        check("host_state_private_storage", status, detail),
        check("host_governance_consistency", status, detail),
        check("host_approval_key", status, detail),
        check("host_governance_quarantine", status, detail),
    ]
}

fn quarantine_checks(reason: Option<QuarantineReason>) -> (DoctorCheck, DoctorCheck) {
    match reason {
        None => (
            check(
                "host_governance_consistency",
                DoctorStatus::Ok,
                "Governance records are mutually consistent.",
            ),
            check(
                "host_governance_quarantine",
                DoctorStatus::Ok,
                "Host governance is not quarantined.",
            ),
        ),
        Some(QuarantineReason::ProjectionConflict) => quarantine_warning(
            "Relay projection conflicts with durable governance; reconcile the registered relay before use.",
        ),
        Some(QuarantineReason::EnrollmentConflict) => quarantine_warning(
            "Enrollment coordinates conflict; reconcile the registered relay before use.",
        ),
        Some(QuarantineReason::HostIdentityClone) => (
            check(
                "host_governance_consistency",
                DoctorStatus::Fail,
                "A stable host identity clone was detected; stop the conflicting host before use.",
            ),
            check(
                "host_governance_quarantine",
                DoctorStatus::Fail,
                "Host governance is quarantined for a stable host identity clone.",
            ),
        ),
    }
}

fn quarantine_warning(detail: &'static str) -> (DoctorCheck, DoctorCheck) {
    (
        check("host_governance_consistency", DoctorStatus::Warn, detail),
        check("host_governance_quarantine", DoctorStatus::Warn, detail),
    )
}

fn check(name: &'static str, status: DoctorStatus, detail: &str) -> DoctorCheck {
    DoctorCheck::new(name, status, detail)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};

    use super::*;

    fn paths_at(root: &Path) -> Paths {
        Paths {
            runtime_dir: root.join("runtime"),
            socket: root.join("runtime").join("daemon.sock"),
            lock: root.join("runtime").join("daemon.lock"),
            log_dir: root.join("logs"),
            state_dir: root.join("state"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            config_home: root.join("config"),
            config_dir: root.join("config").join("pohunek"),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pohunek-daemon-doctor-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after unix epoch")
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn report_contains_writable_daemon_paths_and_governance_checks() {
        let root = temp_dir("report");
        let paths = paths_at(&root);

        let service = crate::governance::HostGovernanceService::open_test();
        let report = report(&paths, &service).await.expect("doctor report");

        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "socket_dir_writable" && check.status == DoctorStatus::Ok));
        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "state_dir_writable" && check.status == DoctorStatus::Ok));
        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "log_dir_writable" && check.status == DoctorStatus::Ok));
        assert_eq!(
            report
                .checks
                .iter()
                .filter(|check| check.name.starts_with("host_"))
                .map(|check| check.name.as_str())
                .collect::<Vec<_>>(),
            [
                "host_governance_durability",
                "host_identity_stable",
                "host_state_private_storage",
                "host_governance_consistency",
                "host_approval_key",
                "host_governance_quarantine",
            ]
        );
    }

    #[test]
    fn projection_and_enrollment_conflicts_warn_but_identity_clone_fails() {
        for reason in [
            QuarantineReason::ProjectionConflict,
            QuarantineReason::EnrollmentConflict,
        ] {
            let checks = governance_checks(HostGovernanceDiagnostic::Available(
                quarantined_status(reason),
            ));
            assert!(checks
                .iter()
                .filter(|check| check.name.ends_with("consistency")
                    || check.name.ends_with("quarantine"))
                .all(|check| check.status == DoctorStatus::Warn));
        }
        let checks = governance_checks(HostGovernanceDiagnostic::Available(quarantined_status(
            QuarantineReason::HostIdentityClone,
        )));
        assert!(checks
            .iter()
            .filter(
                |check| check.name.ends_with("consistency") || check.name.ends_with("quarantine")
            )
            .all(|check| check.status == DoctorStatus::Fail));
    }

    #[test]
    fn available_governance_matrix_is_ok_for_never_enrolled_principal_and_team() {
        for (name, owner) in [
            ("never enrolled", None),
            ("principal", Some(principal_owner())),
            ("team", Some(team_owner())),
        ] {
            assert_governance_case(
                name,
                HostGovernanceDiagnostic::Available(available_status(owner)),
                [DoctorStatus::Ok; 6],
                DoctorStatus::Ok,
            );
        }
    }

    #[test]
    fn quarantined_governance_matrix_preserves_warning_and_failure_severities() {
        for (name, reason, statuses, overall) in [
            (
                "projection conflict",
                QuarantineReason::ProjectionConflict,
                warning_statuses(),
                DoctorStatus::Warn,
            ),
            (
                "enrollment conflict",
                QuarantineReason::EnrollmentConflict,
                warning_statuses(),
                DoctorStatus::Warn,
            ),
            (
                "identity clone",
                QuarantineReason::HostIdentityClone,
                identity_clone_statuses(),
                DoctorStatus::Fail,
            ),
        ] {
            assert_governance_case(
                name,
                HostGovernanceDiagnostic::Available(quarantined_status(reason)),
                statuses,
                overall,
            );
        }
    }

    #[test]
    fn unavailable_governance_matrix_fails_every_check() {
        for (name, diagnostic) in [
            ("reload required", HostGovernanceDiagnostic::ReloadRequired),
            (
                "integrity failed",
                HostGovernanceDiagnostic::IntegrityFailed,
            ),
            ("unavailable", HostGovernanceDiagnostic::Unavailable),
        ] {
            assert_governance_case(
                name,
                diagnostic,
                [DoctorStatus::Fail; 6],
                DoctorStatus::Fail,
            );
        }
    }

    fn assert_governance_case(
        name: &str,
        diagnostic: HostGovernanceDiagnostic,
        expected_statuses: [DoctorStatus; 6],
        expected_overall: DoctorStatus,
    ) {
        let checks = governance_checks(diagnostic);
        assert_eq!(
            checks.clone().map(|check| check.name),
            governance_check_names(),
            "{name} check order"
        );
        assert_eq!(
            checks.clone().map(|check| check.status),
            expected_statuses,
            "{name} statuses"
        );
        assert_eq!(
            DoctorReport::from_checks(Vec::from(checks)).overall,
            expected_overall,
            "{name} overall severity"
        );
    }

    fn governance_check_names() -> [&'static str; 6] {
        [
            "host_governance_durability",
            "host_identity_stable",
            "host_state_private_storage",
            "host_governance_consistency",
            "host_approval_key",
            "host_governance_quarantine",
        ]
    }

    fn warning_statuses() -> [DoctorStatus; 6] {
        [
            DoctorStatus::Ok,
            DoctorStatus::Ok,
            DoctorStatus::Ok,
            DoctorStatus::Warn,
            DoctorStatus::Ok,
            DoctorStatus::Warn,
        ]
    }

    fn identity_clone_statuses() -> [DoctorStatus; 6] {
        [
            DoctorStatus::Ok,
            DoctorStatus::Ok,
            DoctorStatus::Ok,
            DoctorStatus::Fail,
            DoctorStatus::Ok,
            DoctorStatus::Fail,
        ]
    }

    fn principal_owner() -> protocol::HostOwner {
        protocol::HostOwner::Principal(
            protocol::PrincipalId::parse(&identifier("principal_", 3)).expect("principal"),
        )
    }

    fn team_owner() -> protocol::HostOwner {
        protocol::HostOwner::Team(protocol::TeamId::parse(&identifier("team_", 4)).expect("team"))
    }

    fn available_status(owner: Option<protocol::HostOwner>) -> protocol::HostGovernanceStatus {
        let enrolled = owner.is_some();
        protocol::HostGovernanceStatus::new(
            protocol::HostId::parse(&identifier("host_", 1)).expect("host"),
            enrolled.then(|| {
                protocol::EnrollmentInfo::new(
                    protocol::RelayId::parse(&identifier("relay_", 2)).expect("relay"),
                    protocol::EnrollmentStatus::Active,
                    protocol::EnrollmentRevision::new(1).expect("revision"),
                )
            }),
            owner,
            enrolled.then(|| protocol::OwnerRevision::new(1).expect("revision")),
            None,
            protocol::ApprovalKeyReference::from_ed25519_verifying_key_bytes([7; 32]),
        )
        .expect("valid available status")
    }

    fn quarantined_status(reason: QuarantineReason) -> protocol::HostGovernanceStatus {
        protocol::HostGovernanceStatus::new(
            protocol::HostId::parse(&identifier("host_", 1)).expect("host"),
            Some(protocol::EnrollmentInfo::new(
                protocol::RelayId::parse(&identifier("relay_", 2)).expect("relay"),
                protocol::EnrollmentStatus::Quarantined,
                protocol::EnrollmentRevision::new(1).expect("revision"),
            )),
            Some(protocol::HostOwner::Principal(
                protocol::PrincipalId::parse(&identifier("principal_", 3)).expect("principal"),
            )),
            Some(protocol::OwnerRevision::new(1).expect("revision")),
            Some(reason),
            protocol::ApprovalKeyReference::from_ed25519_verifying_key_bytes([7; 32]),
        )
        .expect("valid quarantined status")
    }

    fn identifier(prefix: &str, byte: u8) -> String {
        format!("{prefix}{}", BASE64_URL_SAFE_NO_PAD.encode([byte; 32]))
    }
}
