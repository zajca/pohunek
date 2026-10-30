//! Scenarios of the `refresh-path` transaction at the engine boundary.
//!
//! The service manager is the in-memory [`Fake`]; every restart of its daemon
//! gives the main process a new start identity, so readiness that must prove a
//! restart can be told from an old process answering the same version.

use super::*;
use crate::service::record::RefreshJournal;
use crate::service::report::RefreshPathReport;

/// An installation of [`V1`] recording `old`, whose login shell now reports `new`.
struct Refreshable {
    harness: Harness,
    old: PathBuf,
    new: PathBuf,
}

async fn refreshable() -> Refreshable {
    let mut harness = Harness::new();
    let old = harness.root.join("old-bin");
    let new = harness.root.join("new-bin");
    harness.context = discovering(&harness.context, &harness.root, "shell-a", &old);
    harness.install(V1).await.expect("install");
    harness.context = discovering(&harness.context, &harness.root, "shell-b", &new);
    Refreshable { harness, old, new }
}

/// Like [`refreshable`], after an upgrade to [`V2`] that accepted `risk_a`.
async fn refreshable_with_ledger() -> Refreshable {
    let mut harness = Harness::new();
    let old = harness.root.join("old-bin");
    let new = harness.root.join("new-bin");
    harness.context = discovering(&harness.context, &harness.root, "shell-a", &old);
    harness.install(V1).await.expect("install");
    harness
        .upgrade_over(V2, &[risk_a()], true)
        .await
        .expect("the flag accepts the risk");
    harness.context = discovering(&harness.context, &harness.root, "shell-b", &new);
    Refreshable { harness, old, new }
}

/// The directory as the recorded path spells it.
fn canonical(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).expect("canonical directory")
}

fn recorded(harness: &Harness) -> Vec<PathBuf> {
    harness
        .config()
        .expect("service.toml")
        .search_path()
        .entries()
        .to_vec()
}

/// The `PATH` of the registered daemon job.
fn job_path_env(harness: &Harness) -> String {
    harness
        .fake
        .world()
        .registered
        .as_ref()
        .expect("a registered job")
        .environment()
        .get("PATH")
        .cloned()
        .expect("the job carries a PATH")
}

fn replaces(harness: &Harness) -> usize {
    harness.fake.world().replaces
}

/// Daemon starts so far; a change means the daemon process is another one.
fn daemon_starts(harness: &Harness) -> u64 {
    harness.fake.world().restarts
}

fn inode(harness: &Harness) -> u64 {
    std::fs::metadata(harness.context.config_path())
        .expect("service.toml metadata")
        .ino()
}

fn record_bytes(harness: &Harness) -> Option<Vec<u8>> {
    std::fs::read(harness.engine().store.path()).ok()
}

/// Runs one refresh with the prepared `engine`.
async fn refresh_with(
    harness: &Harness,
    engine: Engine<'_>,
    dry_run: bool,
) -> Result<RefreshPathReport, Error> {
    crate::service::refresh_discovering(&harness.context, &engine, dry_run).await
}

/// Refreshes over the stopped-session verdicts `stopped`.
async fn refresh_over(
    harness: &Harness,
    stopped: &[SessionVerdict],
    accept: bool,
    dry_run: bool,
) -> Result<RefreshPathReport, Error> {
    let engine = harness
        .engine()
        .with_runtime_loss_accepted(accept)
        .with_adoption_preflight(judging(stopped));
    refresh_with(harness, engine, dry_run).await
}

/// Leaves a refresh interrupted right after `step`.
async fn interrupt_after(harness: &Harness, step: Step) {
    let mut engine = harness.engine();
    engine.interrupt_after = Some(step);
    let error = refresh_with(harness, engine, false)
        .await
        .expect_err("the refresh is interrupted");
    assert!(
        matches!(error, Error::Interrupted(at) if at == step),
        "{error:?}"
    );
}

/// A live session the new daemon would not adopt, seeded below `paths`.
fn seed_at_risk_at(paths: &BasePaths, executable: &Path) {
    seed_live_session_at(paths, executable, SESSION, "w-1");
    let store = paths.data_dir.join(pohunek_paths::METADATA_STORE_NAME);
    let body = std::fs::read_to_string(&store).expect("read store");
    let mut line: serde_json::Value = serde_json::from_str(body.trim()).expect("line");
    line.as_object_mut().expect("record").remove("runtime");
    std::fs::write(&store, format!("{line}\n")).expect("write store");
}

fn seed_at_risk(harness: &Harness) {
    let executable = harness.layout().worker_executable(V1).expect("worker");
    seed_at_risk_at(harness.context.paths(), &executable);
}

/// Makes the next `replace` of the daemon job seed an at-risk session, as the
/// old daemon accepting work meanwhile.
fn seed_at_risk_on_replace(harness: &Harness) {
    let paths = harness.context.paths().clone();
    let executable = harness.layout().worker_executable(V1).expect("worker");
    harness.fake.world().replace_hook = Some(ReplaceHook(Arc::new(move || {
        seed_at_risk_at(&paths, &executable);
    })));
}

fn journal(harness: &Harness) -> RefreshJournal {
    harness
        .pending()
        .expect("a pending record")
        .refresh
        .expect("a refresh journal")
}

#[tokio::test]
async fn a_changed_path_is_recorded_and_the_daemon_job_replaced_with_it() {
    let r = refreshable().await;
    let harness = &r.harness;
    let worker = worker(worker_id(SESSION), ServiceState::Running);
    harness.fake.world().workers.push(worker.clone());
    let starts = daemon_starts(harness);
    let installed_before = replaces(harness);

    let report = refresh(harness, false).await.expect("refresh");

    assert!(report.changed && report.applied && !report.resumed && !report.dry_run);
    assert_eq!(report.daemon_restart, "restarted");
    assert!(!report.daemon_started);
    assert_eq!(report.workers_kept, Some(1));
    assert!(report.rolled_back.is_none());
    assert_eq!(report.recorded, [canonical(&r.old)]);
    assert_eq!(report.discovered, [canonical(&r.new)]);
    assert_eq!(recorded(harness), [canonical(&r.new)]);
    assert_eq!(
        job_path_env(harness),
        canonical(&r.new).display().to_string()
    );
    assert_eq!(
        replaces(harness),
        installed_before + 1,
        "exactly one replacement"
    );
    assert_eq!(daemon_starts(harness), starts + 1, "the daemon restarted");
    assert_eq!(
        harness.fake.world().workers,
        [worker],
        "worker jobs are untouched"
    );
    assert_eq!(harness.pending(), None, "the record is cleared");
    assert_eq!(harness.ledger_bytes(), None, "no ledger is written");
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
}

#[tokio::test]
async fn a_current_path_changes_nothing_and_restarts_nothing() {
    let r = refreshable().await;
    let harness = &r.harness;
    refresh(harness, false).await.expect("first refresh");
    let (bytes, node) = (config_bytes(harness), inode(harness));
    let (replaced, starts) = (replaces(harness), daemon_starts(harness));

    let again = refresh(harness, false).await.expect("second refresh");

    assert!(!again.changed && !again.applied);
    assert_eq!(again.daemon_restart, "none");
    assert_eq!(config_bytes(harness), bytes);
    assert_eq!(inode(harness), node, "no file is replaced");
    assert_eq!(replaces(harness), replaced, "the job is not replaced");
    assert_eq!(daemon_starts(harness), starts);
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn a_dry_run_takes_no_lock_and_changes_nothing() {
    let r = refreshable().await;
    let harness = &r.harness;
    let (bytes, node) = (config_bytes(harness), inode(harness));
    let (replaced, starts) = (replaces(harness), daemon_starts(harness));
    let held = harness
        .engine()
        .store
        .lock()
        .await
        .expect("the test holds the transaction lock");

    let report = refresh(harness, true)
        .await
        .expect("a dry run is not blocked by the lock");
    drop(held);

    assert!(report.dry_run && report.changed && !report.applied);
    assert_eq!(report.daemon_restart, "would_restart");
    assert!(report.preflight.is_some(), "the adoption verdict is shown");
    assert_eq!(config_bytes(harness), bytes);
    assert_eq!(inode(harness), node);
    assert_eq!(replaces(harness), replaced);
    assert_eq!(daemon_starts(harness), starts);
    assert_eq!(harness.pending(), None, "a dry run writes no record");
}

#[tokio::test]
async fn an_early_gate_refusal_changes_nothing_and_the_flag_lets_it_through() {
    let r = refreshable().await;
    let harness = &r.harness;
    seed_at_risk(harness);
    let (bytes, node) = (config_bytes(harness), inode(harness));
    let replaced = replaces(harness);

    let error = refresh(harness, false).await.expect_err("refused");

    let Error::UpgradeAtRisk { sessions, .. } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(sessions[0].session_id, SESSION);
    assert_eq!(config_bytes(harness), bytes);
    assert_eq!(inode(harness), node);
    assert_eq!(harness.pending(), None);
    assert_eq!(replaces(harness), replaced);
    let dry = refresh(harness, true)
        .await
        .expect_err("a dry run refuses too");
    assert!(matches!(dry, Error::UpgradeAtRisk { .. }), "{dry:?}");

    let report = refresh_with(
        harness,
        harness.engine().with_runtime_loss_accepted(true),
        false,
    )
    .await
    .expect("the flag accepts the loss");
    assert!(report.applied && report.accepted_runtime_loss);
    assert_eq!(recorded(harness), [canonical(&r.new)]);
    assert_eq!(harness.ledger_bytes(), None, "the consent is not recorded");
}

#[tokio::test]
async fn a_late_gate_refusal_restores_the_path_without_touching_the_daemon() {
    let r = refreshable().await;
    let harness = &r.harness;
    let bytes = config_bytes(harness);
    let (replaced, starts) = (replaces(harness), daemon_starts(harness));
    let paths = harness.context.paths().clone();
    let executable = harness.layout().worker_executable(V1).expect("worker");
    let mut engine = harness.engine();
    engine.before_swap = Some(SwapHook(Box::new(move || {
        seed_at_risk_at(&paths, &executable);
    })));

    let error = refresh_with(harness, engine, false)
        .await
        .expect_err("the late gate refuses");

    assert!(matches!(error, Error::UpgradeAtRisk { .. }), "{error:?}");
    assert_eq!(recorded(harness), [canonical(&r.old)]);
    assert_eq!(config_bytes(harness), bytes, "the old file is back");
    assert_eq!(replaces(harness), replaced, "no replacement was issued");
    assert_eq!(daemon_starts(harness), starts);
    assert_eq!(harness.pending(), None, "the record is cleared");
}

#[tokio::test]
async fn an_acceptance_of_the_installed_version_covers_the_refresh_and_the_ledger_stays() {
    let r = refreshable_with_ledger().await;
    let harness = &r.harness;
    let ledger = harness.ledger_bytes().expect("the upgrade left a ledger");

    let report = refresh_over(harness, &[risk_a()], false, false)
        .await
        .expect("the identical risk was accepted by the upgrade that installed this version");

    assert!(report.applied && !report.accepted_runtime_loss);
    assert_eq!(harness.ledger_bytes().as_deref(), Some(ledger.as_slice()));
}

#[tokio::test]
async fn a_risk_no_upgrade_accepted_refuses_and_the_flag_leaves_the_ledger_untouched() {
    let r = refreshable_with_ledger().await;
    let harness = &r.harness;
    let ledger = harness.ledger_bytes().expect("ledger");
    let (bytes, replaced) = (config_bytes(harness), replaces(harness));

    let error = refresh_over(harness, &[risk_a(), risk_b()], false, false)
        .await
        .expect_err("the second session was never accepted");

    let Error::UpgradeAtRisk {
        stopped_sessions, ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(stopped_sessions, &[risk_b()]);
    assert_eq!(config_bytes(harness), bytes);
    assert_eq!(replaces(harness), replaced);
    assert_eq!(harness.ledger_bytes().as_deref(), Some(ledger.as_slice()));

    refresh_over(harness, &[risk_a(), risk_b()], true, false)
        .await
        .expect("the flag accepts the new risk");
    assert_eq!(
        harness.ledger_bytes().as_deref(),
        Some(ledger.as_slice()),
        "a refresh never writes the ledger"
    );
}

#[tokio::test]
async fn an_interrupted_refresh_converges_on_the_journaled_target_at_every_step() {
    for step in [
        Step::Started,
        Step::Config,
        Step::Definition,
        Step::Registering,
        Step::Registered,
        Step::Ready,
    ] {
        let mut r = refreshable().await;
        interrupt_after(&r.harness, step).await;
        assert_eq!(r.harness.pending().expect("record").step, step);
        // The login shell now reports a third directory; the journal wins.
        let third = r.harness.root.join("third-bin");
        r.harness.context = discovering(&r.harness.context, &r.harness.root, "shell-c", &third);

        let report = refresh(&r.harness, false)
            .await
            .unwrap_or_else(|error| panic!("{step:?}: {error:?}"));

        let harness = &r.harness;
        assert!(report.resumed && report.applied, "{step:?}");
        assert_eq!(report.discovered, [canonical(&r.new)], "{step:?}");
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("differs from the one the interrupted refresh")),
            "{step:?}: {:?}",
            report.warnings
        );
        assert_eq!(recorded(harness), [canonical(&r.new)], "{step:?}");
        assert_eq!(
            job_path_env(harness),
            canonical(&r.new).display().to_string(),
            "{step:?}"
        );
        assert_eq!(
            replaces(harness),
            1,
            "{step:?}: exactly one replacement however often it was interrupted"
        );
        assert_eq!(harness.pending(), None, "{step:?}");
    }
}

#[tokio::test]
async fn a_file_written_before_the_config_step_was_journaled_continues() {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Started).await;
    let target = SearchPath::new(journal(harness).target).expect("journaled target");
    harness
        .config()
        .expect("config")
        .with_search_path(target)
        .expect("target config")
        .write(&harness.context.config_path())
        .expect("the crash left the new file");

    let report = refresh(harness, false).await.expect("resume");

    assert!(report.resumed && report.applied);
    assert_eq!(recorded(harness), [canonical(&r.new)]);
    assert_eq!(replaces(harness), 1);
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn a_replacement_issued_before_it_was_journaled_is_issued_again_and_proven_by_a_new_process()
{
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Registering).await;
    let pending = harness.pending().expect("record");
    let target = SearchPath::new(journal(harness).target).expect("journaled target");
    let definition = daemon_definition(
        &harness.context,
        &harness
            .config()
            .expect("config")
            .with_search_path(target)
            .expect("target config"),
    )
    .expect("definition");
    assert_eq!(pending.step, Step::Registering);
    // The process died after the manager accepted the replacement.
    DaemonSupervisor::replace(&harness.fake, &definition)
        .await
        .expect("replace");
    assert_eq!(replaces(harness), 1);

    let report = refresh(harness, false).await.expect("resume");

    assert!(report.resumed && report.applied);
    assert_eq!(
        replaces(harness),
        2,
        "the idempotent replacement is repeated"
    );
    assert_eq!(
        job_path_env(harness),
        canonical(&r.new).display().to_string()
    );
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn readiness_needs_a_new_daemon_process_and_a_same_process_answer_rolls_back() {
    let r = refreshable().await;
    let harness = &r.harness;
    let before = config_bytes(harness);
    let old_env = canonical(&r.old).display().to_string();
    // The manager accepts the new definition but the old process keeps
    // answering the same version.
    harness.fake.world().kept_process_path = Some(canonical(&r.new).display().to_string());

    let error = refresh(harness, false)
        .await
        .expect_err("a process that never restarted is not ready");

    assert!(matches!(error, Error::DaemonNotReady { .. }), "{error:?}");
    assert_eq!(recorded(harness), [canonical(&r.old)]);
    assert_eq!(config_bytes(harness), before, "the old file is back");
    assert_eq!(replaces(harness), 2, "the old definition replaced the new");
    assert_eq!(job_path_env(harness), old_env);
    assert_eq!(harness.pending(), None);
    assert!(harness.fake.world().running);
}

#[tokio::test]
async fn a_failed_replacement_restores_the_path_and_surfaces_the_original_error() {
    let r = refreshable().await;
    let harness = &r.harness;
    let before = config_bytes(harness);
    harness.fake.world().refused_replace_path = Some(canonical(&r.new).display().to_string());

    let error = refresh(harness, false).await.expect_err("replace fails");

    assert!(matches!(error, Error::Supervisor { .. }), "{error:?}");
    assert_eq!(config_bytes(harness), before);
    assert_eq!(recorded(harness), [canonical(&r.old)]);
    assert_eq!(
        job_path_env(harness),
        canonical(&r.old).display().to_string(),
        "the previous definition is back"
    );
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn a_refused_rollback_gate_keeps_the_record_rolling_back_until_the_flag_finishes_it() {
    let r = refreshable().await;
    let harness = &r.harness;
    harness.fake.world().kept_process_path = Some(canonical(&r.new).display().to_string());
    seed_at_risk_on_replace(harness);

    let error = refresh(harness, false)
        .await
        .expect_err("readiness fails and the rollback gate refuses");

    assert_eq!(error.code(), "service_rollback_failed", "{error:?}");
    let pending = harness.pending().expect("the record is kept");
    assert!(pending.rolling_back, "{pending:?}");
    assert_eq!(pending.step, Step::Registered);
    assert_eq!(
        recorded(harness),
        [canonical(&r.new)],
        "nothing was restored"
    );

    // The other commands refuse to start over it.
    let error = harness.upgrade(V2).await.expect_err("pending refresh");
    assert_eq!(error.code(), "service_refresh_pending", "{error:?}");
    let again = refresh(harness, false).await.expect_err("still refused");
    assert_eq!(
        again.code(),
        "service_rollback_sessions_at_risk",
        "{again:?}"
    );
    assert!(harness.pending().expect("kept").rolling_back);

    let report = refresh_with(
        harness,
        harness.engine().with_runtime_loss_accepted(true),
        false,
    )
    .await
    .expect("the flag finishes the rollback");

    assert!(!report.applied, "no new refresh starts in the same run");
    assert_eq!(
        report.daemon_restart, "restarted",
        "the rollback replaced the job with the previous definition"
    );
    assert!(report.workers_kept.is_some());
    assert_eq!(
        report.rolled_back.as_ref().map(|pending| pending.operation),
        Some("refresh_path")
    );
    assert!(report.accepted_runtime_loss);
    assert_eq!(recorded(harness), [canonical(&r.old)]);
    assert_eq!(
        job_path_env(harness),
        canonical(&r.old).display().to_string()
    );
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn a_rollback_that_stopped_after_restoring_the_file_is_finished_by_a_rerun() {
    let r = refreshable().await;
    let harness = &r.harness;
    let (old_env, new_env) = (
        canonical(&r.old).display().to_string(),
        canonical(&r.new).display().to_string(),
    );
    harness.fake.world().kept_process_path = Some(new_env);
    // The restoring replacement is refused, after the file was put back.
    harness.fake.world().refused_replace_path = Some(old_env.clone());

    let error = refresh(harness, false)
        .await
        .expect_err("readiness fails and the restore fails");

    assert_eq!(error.code(), "service_rollback_failed", "{error:?}");
    assert!(harness.pending().expect("kept").rolling_back);
    assert_eq!(
        recorded(harness),
        [canonical(&r.old)],
        "the file was restored"
    );
    harness.fake.world().refused_replace_path = None;

    let report = refresh(harness, false)
        .await
        .expect("the rerun finishes it");

    assert!(!report.applied);
    assert_eq!(report.daemon_restart, "restarted");
    assert_eq!(
        report.rolled_back.map(|pending| pending.operation),
        Some("refresh_path")
    );
    assert_eq!(job_path_env(harness), old_env);
    assert_eq!(recorded(harness), [canonical(&r.old)]);
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn an_upgrade_rolls_a_refresh_before_registering_back_and_never_resumes_it_as_an_upgrade() {
    let r = refreshable().await;
    let harness = &r.harness;
    let before = config_bytes(harness);
    interrupt_after(harness, Step::Config).await;
    assert_eq!(
        recorded(harness),
        [canonical(&r.new)],
        "the file was written"
    );

    let report = harness
        .upgrade(V1)
        .await
        .expect("the same release is rerun over the pending refresh");

    let rolled_back = report.rolled_back.expect("the refresh is rolled back");
    assert_eq!(rolled_back.operation, "refresh_path");
    assert!(report.unchanged && !report.resumed);
    assert_eq!(config_bytes(harness), before, "the path is restored");
    assert_eq!(replaces(harness), 0, "the daemon was never touched");
    assert_eq!(harness.pending(), None);

    // A version-changing upgrade starts over from the restored path.
    interrupt_after(harness, Step::Definition).await;
    let report = harness.upgrade(V2).await.expect("upgrade");
    assert_eq!(
        report.rolled_back.map(|pending| pending.operation),
        Some("refresh_path")
    );
    assert!(!report.resumed);
    harness.assert_installed(V2);
    assert_eq!(recorded(harness), [canonical(&r.old)]);
}

#[tokio::test]
async fn a_refresh_that_reached_registering_blocks_upgrade_and_install_and_leaves_everything() {
    for step in [Step::Registering, Step::Registered, Step::Ready] {
        let r = refreshable().await;
        let harness = &r.harness;
        interrupt_after(harness, step).await;
        let (bytes, record) = (config_bytes(harness), record_bytes(harness));
        let replaced = replaces(harness);

        for (name, error) in [
            ("upgrade", harness.upgrade(V2).await.map(drop)),
            ("same-version upgrade", harness.upgrade(V1).await.map(drop)),
            ("install", harness.install(V1).await.map(drop)),
        ] {
            let error = error.expect_err(name);
            assert_eq!(error.code(), "service_refresh_pending", "{step:?} {name}");
            assert_eq!(
                error.hint(),
                Some("rerun `pohunek service refresh-path`; it resumes or rolls back the pending refresh")
            );
        }

        assert_eq!(config_bytes(harness), bytes, "{step:?}");
        assert_eq!(record_bytes(harness), record, "{step:?}");
        assert_eq!(replaces(harness), replaced, "{step:?}");
    }
}

#[tokio::test]
async fn install_rolls_back_a_refresh_before_registering_and_then_refuses_as_installed() {
    let r = refreshable().await;
    let harness = &r.harness;
    let before = config_bytes(harness);
    interrupt_after(harness, Step::Config).await;

    let error = harness.install(V1).await.expect_err("already installed");

    assert!(matches!(error, Error::AlreadyInstalled { .. }), "{error:?}");
    assert_eq!(
        config_bytes(harness),
        before,
        "the rollback restored the path"
    );
    assert_eq!(harness.pending(), None);
    assert_eq!(replaces(harness), 0);
}

#[tokio::test]
async fn uninstall_rolls_a_refresh_back_before_registering_and_takes_the_full_flow_after() {
    let r = refreshable().await;
    interrupt_after(&r.harness, Step::Config).await;
    let removed = r
        .harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall");
    assert_eq!(
        removed.rolled_back.map(|pending| pending.operation),
        Some("refresh_path")
    );
    r.harness.assert_clean();

    let r = refreshable().await;
    interrupt_after(&r.harness, Step::Registered).await;
    let removed = r
        .harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall");
    assert!(
        removed.rolled_back.is_none(),
        "a registered refresh takes the session-checked flow, not a rollback"
    );
    r.harness.assert_clean();
}

#[tokio::test]
async fn a_file_edited_during_a_pending_refresh_is_never_overwritten() {
    for step in [Step::Config, Step::Registering, Step::Registered] {
        let r = refreshable().await;
        let harness = &r.harness;
        interrupt_after(harness, step).await;
        let mut text = String::from_utf8(config_bytes(harness)).expect("utf-8");
        text.push_str("\n# edited by the operator\n");
        std::fs::write(harness.context.config_path(), &text).expect("edit");
        let record = record_bytes(harness);
        let replaced = replaces(harness);

        let error = refresh(harness, false).await.expect_err("edited");
        assert_eq!(error.code(), "service_refresh_config_changed", "{step:?}");
        let error = harness
            .upgrade(V1)
            .await
            .map(drop)
            .expect_err("the rollback refuses too");
        if step < Step::Registering {
            assert_eq!(error.code(), "service_refresh_config_changed", "{step:?}");
        }

        assert_eq!(config_bytes(harness), text.as_bytes(), "{step:?}");
        assert_eq!(record_bytes(harness), record, "{step:?}");
        assert_eq!(replaces(harness), replaced, "{step:?}");
    }
}

#[tokio::test]
async fn an_edited_file_never_blocks_the_uninstall_of_a_refresh_before_registering() {
    for step in [Step::Config, Step::Definition] {
        let r = refreshable().await;
        let harness = &r.harness;
        interrupt_after(harness, step).await;
        let mut text = String::from_utf8(config_bytes(harness)).expect("utf-8");
        text.push_str("\n# edited by the operator\n");
        std::fs::write(harness.context.config_path(), &text).expect("edit");
        let error = refresh(harness, false).await.expect_err("edited");
        assert_eq!(error.code(), "service_refresh_config_changed", "{step:?}");

        let removed = harness
            .engine()
            .uninstall(UninstallOptions::default())
            .await
            .unwrap_or_else(|error| panic!("{step:?}: {error:?}"));

        assert!(
            removed.rolled_back.is_none(),
            "{step:?}: nothing was restored over the edited file"
        );
        harness.assert_clean();
        assert_eq!(harness.pending(), None, "{step:?}");
    }
}

#[tokio::test]
async fn a_file_edited_after_the_refresh_finished_its_work_is_left_alone_and_the_record_cleared() {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Ready).await;
    let mut text = String::from_utf8(config_bytes(harness)).expect("utf-8");
    text.push_str("\n# edited by the operator\n");
    std::fs::write(harness.context.config_path(), &text).expect("edit");
    let replaced = replaces(harness);

    let report = refresh(harness, false)
        .await
        .expect("only the clear is left, whatever the file holds");

    assert!(report.resumed && report.applied);
    assert_eq!(harness.pending(), None);
    assert_eq!(config_bytes(harness), text.as_bytes(), "the edit is kept");
    assert_eq!(replaces(harness), replaced, "nothing is replaced again");
}

#[tokio::test]
async fn a_file_restored_to_the_previous_path_by_hand_is_written_again_by_the_resume() {
    let mut r = refreshable().await;
    // A hand-written comment makes the original bytes differ from the
    // canonical rendering a rollback writes.
    let mut original = String::from_utf8(config_bytes(&r.harness)).expect("utf-8");
    original.push_str("\n# my note\n");
    std::fs::write(r.harness.context.config_path(), &original).expect("annotate");
    interrupt_after(&r.harness, Step::Config).await;
    let harness = &r.harness;
    let previous = SearchPath::new(journal(harness).previous).expect("previous path");
    harness
        .config()
        .expect("config")
        .with_search_path(previous)
        .expect("previous config")
        .write(&harness.context.config_path())
        .expect("restore by hand");
    assert_ne!(config_bytes(harness), original.as_bytes());
    r.harness.context = discovering(&r.harness.context, &r.harness.root, "shell-c", &r.old);
    let harness = &r.harness;

    let report = refresh(harness, false).await.expect("resume");

    assert!(report.resumed && report.applied);
    assert_eq!(recorded(harness), [canonical(&r.new)]);
    assert_eq!(replaces(harness), 1);
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn a_failing_login_shell_does_not_block_resolving_a_pending_refresh() {
    for step in [Step::Config, Step::Registered, Step::Ready] {
        let mut r = refreshable().await;
        interrupt_after(&r.harness, step).await;
        let fallback = r.harness.root.join("fallback-bin");
        r.harness.context = scripted_discovery(
            &r.harness.context,
            &r.harness.root,
            "exit 1",
            pohunek_test_support::wait::HANG_GUARD,
            &fallback,
        );
        let harness = &r.harness;

        let report = refresh(harness, false)
            .await
            .unwrap_or_else(|error| panic!("{step:?}: {error:?}"));

        assert!(report.resumed && report.applied, "{step:?}");
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("login shell discovery failed")),
            "{step:?}: {:?}",
            report.warnings
        );
        assert_eq!(report.discovered, [canonical(&r.new)], "{step:?}");
        assert_eq!(recorded(harness), [canonical(&r.new)], "{step:?}");
        assert_eq!(replaces(harness), 1, "{step:?}");
        assert_eq!(harness.pending(), None, "{step:?}");
    }
}

#[tokio::test]
async fn a_failing_login_shell_does_not_block_finishing_a_pending_rollback() {
    let mut r = refreshable().await;
    let (old_env, new_env) = (
        canonical(&r.old).display().to_string(),
        canonical(&r.new).display().to_string(),
    );
    r.harness.fake.world().kept_process_path = Some(new_env);
    r.harness.fake.world().refused_replace_path = Some(old_env.clone());
    refresh(&r.harness, false)
        .await
        .expect_err("the restore is refused");
    r.harness.fake.world().refused_replace_path = None;
    let fallback = r.harness.root.join("fallback-bin");
    r.harness.context = scripted_discovery(
        &r.harness.context,
        &r.harness.root,
        "exit 1",
        pohunek_test_support::wait::HANG_GUARD,
        &fallback,
    );
    let harness = &r.harness;

    let report = refresh(harness, false)
        .await
        .expect("the rollback needs no discovery");

    assert!(report.rolled_back.is_some() && !report.applied);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("login shell discovery failed")),
        "{:?}",
        report.warnings
    );
    assert_eq!(job_path_env(harness), old_env);
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn without_a_daemon_job_nothing_changes_and_a_stopped_job_is_started() {
    let r = refreshable().await;
    let harness = &r.harness;
    let before = config_bytes(harness);
    harness.fake.world().registered = None;
    harness.fake.world().running = false;
    for dry_run in [false, true] {
        let error = refresh(harness, dry_run).await.expect_err("no job");
        assert_eq!(error.code(), "service_daemon_job_missing");
    }
    assert_eq!(config_bytes(harness), before);
    assert_eq!(harness.pending(), None);
    assert_eq!(replaces(harness), 0);
    assert_eq!(
        harness.fake.world().installs,
        1,
        "only the original install"
    );

    let r = refreshable().await;
    let harness = &r.harness;
    harness.fake.world().running = false;
    let report = refresh(harness, false)
        .await
        .expect("a stopped job is started");
    assert!(report.daemon_started && report.applied);
    assert!(harness.fake.world().running);
    assert_eq!(
        job_path_env(harness),
        canonical(&r.new).display().to_string()
    );
}

#[tokio::test]
async fn status_and_check_name_a_pending_refresh_and_what_the_next_command_does() {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Config).await;

    let config = harness.config();
    let status = harness
        .engine()
        .status(config.as_ref())
        .await
        .expect("status");
    let pending = status.pending_transaction.expect("pending");
    assert_eq!(pending.operation, "refresh_path");
    assert_eq!(pending.step, "config");
    assert!(!pending.rolling_back);
    let checked = crate::service::check_with_preflight(
        &harness.context,
        &harness.staged(V2),
        &InProcessPreflight::default(),
        V2,
        false,
    )
    .await
    .expect("check");
    assert_eq!(checked.pending_action, Some("roll_back"));
    assert_eq!(
        checked.pending_transaction.map(|pending| pending.operation),
        Some("refresh_path")
    );
}

#[tokio::test]
async fn a_check_of_an_upgrade_is_refused_like_the_upgrade_while_a_refresh_is_registered() {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Registered).await;

    let checked = crate::service::check_with_preflight(
        &harness.context,
        &harness.staged(V2),
        &InProcessPreflight::default(),
        V2,
        false,
    )
    .await
    .expect_err("an upgrade is refused while the refresh is pending");

    assert_eq!(checked.code(), "service_refresh_pending");
}

#[tokio::test]
async fn a_dry_run_over_a_pending_refresh_fails_closed_and_leaves_the_record() {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Config).await;

    let error = refresh(harness, true).await.expect_err("dry run");
    assert_eq!(error.code(), "service_refresh_pending");
    assert_eq!(harness.pending().expect("kept").step, Step::Config);
}
