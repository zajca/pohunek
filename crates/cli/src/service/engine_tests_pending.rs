//! Scenarios of the guidance around a stopped upgrade: the status of the
//! pending transaction, the refusal of its resume and the way back to the
//! previous version.

use super::*;

/// The serialized `pending_transaction` of the status report.
fn pending_json(report: &StatusReport) -> serde_json::Value {
    serde_json::to_value(report.pending_transaction.as_ref().expect("pending"))
        .expect("serialize the pending transaction")
}

#[tokio::test]
async fn status_names_the_version_a_pending_upgrade_started_from_and_no_other_operation_does() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Config);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted");

    let config = harness.config();
    let status = harness
        .engine()
        .status(config.as_ref())
        .await
        .expect("status");
    let pending = pending_json(&status);
    assert_eq!(pending["operation"], "upgrade");
    assert_eq!(pending["version"], V2);
    assert_eq!(pending["previous_version"], V1);

    let installing = Harness::new();
    let mut engine = installing.engine();
    engine.interrupt_after = Some(Step::Registering);
    engine
        .install(&installing.staged(V1), &installing.prefix(), V1)
        .await
        .expect_err("interrupted");
    let status = installing
        .engine()
        .status(installing.config().as_ref())
        .await
        .expect("status");
    let pending = pending_json(&status);
    assert_eq!(pending["operation"], "install");
    assert!(pending.get("previous_version").is_none(), "{pending}");
}

/// Resume refused at risk, then the way back: the rollback to the previous
/// version restores `service.toml` byte for byte and clears the record.
#[tokio::test]
async fn a_resume_refused_at_risk_names_the_rollback_that_restores_the_previous_version() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let before = config_bytes(&harness);
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Definition);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted");

    let refused = harness
        .upgrade_over(V2, &[risk_a()], false)
        .await
        .expect_err("the resume is refused again");

    assert!(
        matches!(refused, Error::UpgradeAtRisk { .. }),
        "{refused:?}"
    );
    assert_steps_in_order(
        refused.hint().expect("hint"),
        &[
            "pass --accept-runtime-loss",
            "no session is changed; to abandon the pending upgrade instead",
            "`pohunek service upgrade --from <prefix>/libexec/pohunek/<previous version>`",
            "shows that version as `from`",
        ],
    );
    assert!(!refused.hint().expect("hint").contains("issues/1763"));
    assert_eq!(harness.pending().expect("kept").step, Step::Definition);

    let installed_u = harness.layout().version_dir(V1).expect("version directory");
    let report = harness
        .engine()
        .upgrade(&installed_u, V1)
        .await
        .expect("the rollback restores the previous version");
    assert!(report.unchanged);
    assert!(report.rolled_back.is_some());
    assert_eq!(harness.pending(), None);
    assert_eq!(config_bytes(&harness), before, "service.toml is as it was");
    harness.assert_installed(V1);
}

/// With sessions at risk under the previous version as well, the rollback to
/// it needs the flag and restores nothing without it.
#[tokio::test]
async fn the_rollback_to_the_previous_version_judges_its_adoption_again() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after the replacement");
    let installed_u = harness.layout().version_dir(V1).expect("version directory");

    let refused = harness
        .engine()
        .with_adoption_preflight(judging(&[risk_a()]))
        .upgrade(&installed_u, V1)
        .await
        .expect_err("the rollback gate refuses");

    assert_eq!(refused.code(), "service_rollback_sessions_at_risk");
    assert_eq!(
        harness.config().expect("config").active_version(),
        V2,
        "nothing was restored"
    );
    assert_eq!(
        harness.pending().map(|pending| pending.version),
        Some(V2.to_owned())
    );

    harness
        .engine()
        .with_runtime_loss_accepted(true)
        .with_adoption_preflight(judging(&[risk_a()]))
        .upgrade(&installed_u, V1)
        .await
        .expect("the flag accepts the loss");
    assert_eq!(harness.config().expect("config").active_version(), V1);
    assert_eq!(harness.pending(), None);
}

/// A resumed upgrade refused with the stale-path warning names the rollback
/// first, because `refresh-path` is refused while the upgrade is pending.
#[test]
fn the_resumed_stale_path_hint_names_the_rollback_before_refresh_path() {
    let refused = Error::UpgradeAtRisk {
        sessions: Vec::new(),
        stopped_sessions: vec![risk_a()],
        acceptance: Box::default(),
        warnings: vec![format!(
            "{} /old",
            super::super::preflight::STALE_PATH_WARNING
        )],
        resumed: true,
    };

    assert_steps_in_order(
        refused.hint().expect("hint"),
        &[
            "first roll the pending upgrade back with `pohunek service upgrade --from <prefix>/libexec/pohunek/<previous version>`",
            "`pohunek service refresh-path --dry-run`",
            "`pohunek service refresh-path`",
            "then rerun the upgrade",
        ],
    );
}
