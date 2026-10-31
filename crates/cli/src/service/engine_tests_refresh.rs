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

    let Error::RefreshAtRisk {
        sessions, warnings, ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(sessions[0].session_id, SESSION);
    assert_eq!(error.code(), "service_upgrade_sessions_at_risk");
    assert!(warnings.is_empty(), "no stale-path warning: {warnings:?}");
    let hint = error.hint().expect("hint");
    assert_steps_in_order(
        hint,
        &[
            "`pohunek service refresh-path --accept-runtime-loss`",
            "accept the loss",
            "records nothing",
        ],
    );
    assert!(
        !hint.contains("rerun this upgrade") && !hint.contains("recorded so"),
        "the hint names no upgrade and no ledger: {hint}"
    );
    assert_eq!(config_bytes(harness), bytes);
    assert_eq!(inode(harness), node);
    assert_eq!(harness.pending(), None);
    assert_eq!(replaces(harness), replaced);
    let dry = refresh(harness, true)
        .await
        .expect_err("a dry run refuses too");
    assert!(matches!(dry, Error::RefreshAtRisk { .. }), "{dry:?}");

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

    assert!(matches!(error, Error::RefreshAtRisk { .. }), "{error:?}");
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

    let Error::RefreshAtRisk {
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
    assert_steps_in_order(
        error.hint().expect("hint"),
        &["rerun the same command", "resumes the rollback"],
    );
    let error = harness.upgrade(V2).await.expect_err("pending refresh");
    assert_eq!(error.code(), "service_refresh_pending", "{error:?}");
    assert_steps_in_order(
        error.hint().expect("hint"),
        &[
            "`pohunek service refresh-path`",
            "`./pohunek`",
            "`pohunek service refresh-path --accept-runtime-loss`",
        ],
    );
    let again = refresh(harness, false).await.expect_err("still refused");
    assert_eq!(
        again.code(),
        "service_rollback_sessions_at_risk",
        "{again:?}"
    );
    assert!(
        matches!(again, Error::RefreshRollbackAtRisk { .. }),
        "{again:?}"
    );
    let hint = again.hint().expect("hint");
    assert_steps_in_order(
        hint,
        &[
            "rerun `pohunek service refresh-path`",
            "`pohunek service refresh-path --accept-runtime-loss`",
            "records nothing",
        ],
    );
    assert!(!hint.contains("rerun the upgrade"), "{hint}");
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
            assert_steps_in_order(
                error.hint().expect("hint"),
                &[
                    "rerun `pohunek service refresh-path` without --dry-run",
                    "the same `pohunek`",
                    "`./pohunek`",
                    "resumes or rolls back the pending refresh",
                ],
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
        assert_steps_in_order(
            error.hint().expect("hint"),
            &[
                "Restore it as the refresh wrote it or as it was before the refresh",
                "then rerun `pohunek service refresh-path`",
                "or run `pohunek service uninstall`",
                "session-checked",
            ],
        );
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
        assert_daemon_job_missing(&error, FILE_UNTOUCHED, RECORD_ABSENT);
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
    assert_steps_in_order(
        error.hint().expect("hint"),
        &[
            "rerun `pohunek service refresh-path` without --dry-run",
            "a dry run is refused while a refresh is pending",
            "the same `pohunek`",
            "resumes or rolls back the pending refresh",
        ],
    );
    assert_eq!(harness.pending().expect("kept").step, Step::Config);
}

#[tokio::test]
async fn a_rerun_at_ready_reads_the_file_and_keeps_the_record_when_it_is_unusable() {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Ready).await;
    let text = String::from_utf8(config_bytes(harness)).expect("utf-8");
    let older = text.replacen("schema_version = 7", "schema_version = 6", 1);
    assert_ne!(older, text, "the fixture declares schema 7");
    std::fs::write(harness.context.config_path(), &older).expect("write schema 6");
    let record = record_bytes(harness);
    let replaced = replaces(harness);

    let error = refresh(harness, false).await.expect_err("other schema");

    assert_eq!(error.code(), "service_refresh_config_schema", "{error}");
    assert_eq!(record_bytes(harness), record, "the record is kept");
    assert_eq!(
        config_bytes(harness),
        older.as_bytes(),
        "the file is untouched"
    );
    assert_eq!(replaces(harness), replaced);

    std::fs::write(harness.context.config_path(), b"not = [valid").expect("write junk");
    let error = refresh(harness, false).await.expect_err("unreadable");
    assert_eq!(error.code(), "service_config_invalid", "{error}");
    assert_eq!(record_bytes(harness), record, "the record is kept");
}

#[tokio::test]
async fn a_hand_restored_previous_path_with_another_value_changed_is_refused() {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Config).await;
    let previous = SearchPath::new(journal(harness).previous).expect("previous path");
    harness
        .config()
        .expect("config")
        .with_search_path(previous)
        .expect("previous config")
        .write(&harness.context.config_path())
        .expect("restore by hand");
    let restored = String::from_utf8(config_bytes(harness)).expect("utf-8");
    let changed = restored.replacen(
        "launchctl_command_ms = 10000",
        "launchctl_command_ms = 10001",
        1,
    );
    assert_ne!(
        changed, restored,
        "the fixture records the launchctl deadline"
    );
    std::fs::write(harness.context.config_path(), &changed).expect("change a value");
    let record = record_bytes(harness);
    let replaced = replaces(harness);

    let error = refresh(harness, false).await.expect_err("edited");
    assert_eq!(error.code(), "service_refresh_config_changed", "{error}");
    let error = harness
        .upgrade(V1)
        .await
        .map(drop)
        .expect_err("the rollback refuses too");
    assert_eq!(error.code(), "service_refresh_config_changed", "{error}");

    assert_eq!(config_bytes(harness), changed.as_bytes());
    assert_eq!(record_bytes(harness), record);
    assert_eq!(replaces(harness), replaced);
}

#[tokio::test]
async fn the_entry_refuses_an_unmanaged_host_before_the_schema_read_and_the_connection() {
    let mut harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.context = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged);
    let text = String::from_utf8(config_bytes(&harness)).expect("utf-8");
    let older = text.replacen("schema_version = 7", "schema_version = 6", 1);
    assert_ne!(older, text, "the fixture declares schema 7");
    std::fs::write(harness.context.config_path(), &older).expect("write schema 6");
    // A record the store cannot decode must not mask the refusal either.
    std::fs::write(harness.engine().store.path(), b"not a record").expect("junk record");
    let connected = std::cell::Cell::new(false);

    let error = crate::service::refresh_entry(&harness.context, None, false, false, || {
        connected.set(true);
        async { Err(Error::SearchPathUnmanaged) }
    })
    .await
    .expect_err("unmanaged");

    assert_eq!(error.code(), "service_search_path_unmanaged", "{error}");
    assert!(!connected.get(), "the service manager is not contacted");
}

#[tokio::test]
async fn the_entry_reads_the_schema_before_it_connects_once_discovery_succeeded() {
    let mut r = refreshable().await;
    let text = String::from_utf8(config_bytes(&r.harness)).expect("utf-8");
    let older = text.replacen("schema_version = 7", "schema_version = 6", 1);
    std::fs::write(r.harness.context.config_path(), &older).expect("write schema 6");
    r.harness.context = discovering(&r.harness.context, &r.harness.root, "shell-c", &r.new);
    let connected = std::cell::Cell::new(false);

    let error = crate::service::refresh_entry(&r.harness.context, None, false, false, || {
        connected.set(true);
        async { Err(Error::SearchPathUnmanaged) }
    })
    .await
    .expect_err("other schema");

    assert_eq!(error.code(), "service_refresh_config_schema", "{error}");
    assert!(!connected.get(), "the service manager is not contacted");
}

/// Runs the entry with a connection that must never be requested.
async fn entry_without_connection(context: &Context) -> Error {
    let connected = std::cell::Cell::new(false);
    let error = crate::service::refresh_entry(context, None, false, false, || {
        connected.set(true);
        async { Err(Error::SearchPathUnmanaged) }
    })
    .await
    .expect_err("the entry refuses before connecting");
    assert!(!connected.get(), "the service manager is not contacted");
    error
}

/// A managed context whose login shell exits with a failure.
fn failing_shell(harness: &Harness) -> Context {
    scripted_discovery(
        &harness.context,
        &harness.root,
        "exit 1",
        pohunek_test_support::wait::HANG_GUARD,
        &harness.root.join("fallback-bin"),
    )
}

#[tokio::test]
async fn the_entry_reports_a_missing_installation_before_the_unmanaged_refusal() {
    let mut harness = Harness::new();
    harness.context = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged);
    assert!(!harness.context.config_path().exists(), "nothing installed");

    let error = entry_without_connection(&harness.context).await;

    assert_eq!(error.code(), "service_not_installed", "{error}");
}

#[tokio::test]
async fn the_entry_returns_a_failed_login_shell_before_the_schema_read() {
    let mut harness = Harness::new();
    harness.install(V1).await.expect("install");
    let text = String::from_utf8(config_bytes(&harness)).expect("utf-8");
    let older = text.replacen("schema_version = 7", "schema_version = 6", 1);
    assert_ne!(older, text, "the fixture declares schema 7");
    std::fs::write(harness.context.config_path(), &older).expect("write schema 6");
    harness.context = failing_shell(&harness);

    let error = entry_without_connection(&harness.context).await;

    assert_eq!(
        error.code(),
        "service_search_path_refresh_failed",
        "{error}"
    );
}

#[tokio::test]
async fn the_entry_returns_a_failed_login_shell_before_reading_the_record_store() {
    let mut harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.context = failing_shell(&harness);
    std::fs::write(harness.engine().store.path(), b"not a record").expect("junk record");

    let error = entry_without_connection(&harness.context).await;

    assert_eq!(
        error.code(),
        "service_search_path_refresh_failed",
        "{error}"
    );
}

#[tokio::test]
async fn a_rollback_over_a_hand_formatted_original_is_finished_by_a_rerun_after_the_restore() {
    let r = refreshable().await;
    // A comment survives neither the typed rewrite of the refresh nor of the
    // rollback, so the restored file is not byte-equal to the original.
    let original = config_bytes(&r.harness);
    let mut annotated = b"# hand-written note\n".to_vec();
    annotated.extend_from_slice(&original);
    std::fs::write(r.harness.context.config_path(), &annotated).expect("annotate service.toml");
    let harness = &r.harness;
    let (old_env, new_env) = (
        canonical(&r.old).display().to_string(),
        canonical(&r.new).display().to_string(),
    );
    harness.fake.world().kept_process_path = Some(new_env);
    harness.fake.world().refused_replace_path = Some(old_env.clone());

    let error = refresh(harness, false)
        .await
        .expect_err("readiness fails and the restore fails");

    assert_eq!(error.code(), "service_rollback_failed", "{error:?}");
    assert!(harness.pending().expect("kept").rolling_back);
    assert_eq!(recorded(harness), [canonical(&r.old)], "restored");
    assert_ne!(
        config_bytes(harness),
        annotated,
        "the restored file is the typed rendering, not the original bytes"
    );
    harness.fake.world().refused_replace_path = None;

    let report = refresh(harness, false)
        .await
        .expect("the rerun finishes the rollback");

    assert!(!report.applied);
    assert_eq!(
        report.rolled_back.map(|pending| pending.operation),
        Some("refresh_path")
    );
    assert_eq!(job_path_env(harness), old_env);
    assert_eq!(recorded(harness), [canonical(&r.old)]);
    assert_eq!(harness.pending(), None);
}

/// Judges a stopped session at risk only when the daemon job's `PATH` holds `dir`.
#[derive(Debug)]
struct RiskUnder {
    inner: InProcessPreflight,
    dir: String,
}

impl AdoptionPreflight for RiskUnder {
    fn judge<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a VerifiedVersionDir,
        search_path: &'a str,
        variables: &'a BTreeMap<String, String>,
        probe_budget: Option<Duration>,
        plan: ReleasePlan,
    ) -> Judging<'a> {
        Box::pin(async move {
            let mut report = self
                .inner
                .judge(context, dir, search_path, variables, probe_budget, plan)
                .await?;
            if search_path.contains(&self.dir) {
                report.stopped_sessions = vec![risk_a()];
            }
            Ok(report)
        })
    }
}

fn risk_under(dir: &Path) -> RiskUnder {
    RiskUnder {
        inner: InProcessPreflight::default(),
        dir: canonical(dir).display().to_string(),
    }
}

#[tokio::test]
async fn an_upgrade_over_a_pending_refresh_judges_the_path_it_runs_with_not_the_target() {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Config).await;
    assert_eq!(
        recorded(harness),
        [canonical(&r.new)],
        "the target is written"
    );

    // The session is at risk only under the refresh target, which the upgrade
    // rolls back before it runs.
    let checked = crate::service::check_with_preflight(
        &harness.context,
        &harness.staged(V2),
        &risk_under(&r.new),
        V2,
        false,
    )
    .await
    .expect("the check previews the restored path, which is not at risk");
    assert_eq!(checked.pending_action, Some("roll_back"));
    assert!(
        checked
            .preflight
            .expect("the check judged")
            .stopped_sessions
            .is_empty(),
        "the restored path is judged"
    );
    harness
        .engine()
        .with_adoption_preflight(Box::new(risk_under(&r.new)))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("the upgrade is not refused over a risk of the target");
    harness.assert_installed(V2);
    assert_eq!(recorded(harness), [canonical(&r.old)]);
}

#[tokio::test]
async fn an_upgrade_over_a_pending_refresh_is_refused_for_a_risk_under_the_restored_path() {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Config).await;
    let (bytes, pending) = (config_bytes(harness), record_bytes(harness));

    let checked = crate::service::check_with_preflight(
        &harness.context,
        &harness.staged(V2),
        &risk_under(&r.old),
        V2,
        false,
    )
    .await
    .expect_err("the check previews the verdict the upgrade gives");
    let Error::UpgradeAtRisk {
        warnings: check_warnings,
        ..
    } = &checked
    else {
        panic!("the check refuses like the upgrade: {checked:?}");
    };
    assert!(
        check_warnings
            .iter()
            .any(|warning| warning.contains(super::preflight::RESTORED_PATH_NOTE)),
        "the check judges with the restored path and says so: {check_warnings:?}"
    );
    let error = harness
        .engine()
        .with_adoption_preflight(Box::new(risk_under(&r.old)))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("refused before the refresh is rolled back");
    assert!(matches!(error, Error::UpgradeAtRisk { .. }), "{error:?}");
    assert_eq!(config_bytes(harness), bytes, "nothing was rolled back");
    assert_eq!(record_bytes(harness), pending);
    let Error::UpgradeAtRisk { warnings, .. } = &error else {
        unreachable!("matched above");
    };
    let [warning] = warnings.as_slice() else {
        panic!("one warning names the judged path: {warnings:?}");
    };
    assert_steps_in_order(
        warning,
        &[
            super::preflight::STALE_PATH_WARNING,
            canonical(&r.old).to_str().expect("utf-8"),
            super::preflight::RESTORED_PATH_NOTE,
            "do not accept the loss",
            "the same `pohunek` that started the pending refresh",
            "`pohunek service refresh-path` to finish it",
            "a dry run is refused while a refresh is pending",
            "rerun this upgrade",
        ],
    );
    assert!(
        !warning.contains("install records it") && !warning.contains("refresh-path --dry-run"),
        "{warning}"
    );
    assert_steps_in_order(
        error.hint().expect("hint"),
        &[
            "do not accept the loss",
            "the same `pohunek` that started the pending search-path refresh",
            "`pohunek service refresh-path` without --dry-run",
            "then rerun the upgrade",
            "otherwise preserve the required runtime",
        ],
    );

    harness
        .engine()
        .with_runtime_loss_accepted(true)
        .with_adoption_preflight(Box::new(risk_under(&r.old)))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("the flag accepts the risk the upgrade runs with");
    harness.assert_installed(V2);
}

const FILE_UNTOUCHED: &str = "service.toml was not changed by this run";
const FILE_RESTORED: &str = "this run put service.toml back on the previous search path";
const RECORD_ABSENT: &str = "no transaction record exists";
const RECORD_CLEARED: &str = "the transaction record was cleared";
const RECORD_ROLLING_BACK: &str = "the transaction record stays rolling_back";

/// Asserts the message states the effects the run had, in order, and the hint
/// names `uninstall` by its outcome and #1763.
fn assert_daemon_job_missing(error: &Error, file: &str, record: &str) {
    assert_steps_in_order(
        &error.to_string(),
        &["no daemon job is registered", file, record],
    );
    assert_steps_in_order(
        error.hint().expect("hint"),
        &[
            "`pohunek service uninstall` (it removes the installation, its daemon job and service.toml, and keeps durable metadata)",
            "documented way out",
            "issues/1763",
        ],
    );
}

/// A job that vanishes while a refresh record is at `step`: a rerun before
/// `registering` rolls the refresh back (the file is restored, the record
/// cleared); at `registering` the replacement and its rollback both find no
/// job (the file is restored, the record stays `rolling_back`).
#[tokio::test]
async fn a_vanished_daemon_job_states_the_effects_of_each_record_step() {
    for step in [
        Step::Started,
        Step::Config,
        Step::Definition,
        Step::Registering,
    ] {
        let r = refreshable().await;
        let harness = &r.harness;
        interrupt_after(harness, step).await;
        harness.fake.world().registered = None;
        harness.fake.world().running = false;

        let error = refresh(harness, false).await.expect_err("the job is gone");

        if step < Step::Registering {
            assert_eq!(
                error.code(),
                "service_daemon_job_missing",
                "{step:?}: {error:?}"
            );
            let file = if step == Step::Started {
                FILE_UNTOUCHED
            } else {
                FILE_RESTORED
            };
            assert_daemon_job_missing(&error, file, RECORD_CLEARED);
            assert_eq!(harness.pending(), None, "{step:?}");
        } else {
            assert_eq!(
                error.code(),
                "service_rollback_failed",
                "{step:?}: {error:?}"
            );
            assert_daemon_job_missing(&error, FILE_RESTORED, RECORD_ROLLING_BACK);
            assert!(harness.pending().expect("kept").rolling_back);
        }
        assert_eq!(
            recorded(harness),
            [canonical(&r.old)],
            "{step:?}: the file records the previous path"
        );
    }
}

#[tokio::test]
async fn a_daemon_job_that_vanishes_after_the_record_exists_names_the_way_out_and_uninstall_takes_it(
) {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Registering).await;
    harness.fake.world().registered = None;
    harness.fake.world().running = false;

    let error = refresh(harness, false).await.expect_err("the job is gone");

    assert_eq!(error.code(), "service_rollback_failed", "{error:?}");
    assert!(
        matches!(
            &error,
            Error::RollbackFailed { original, rollback }
                if matches!(**original, Error::DaemonJobMissing { .. })
                    && matches!(**rollback, Error::DaemonJobMissing { .. })
        ),
        "the replacement and the rollback both report the missing job: {error:?}"
    );
    assert_daemon_job_missing(&error, FILE_RESTORED, RECORD_ROLLING_BACK);
    assert!(harness.pending().expect("the record is kept").rolling_back);
    assert_eq!(
        recorded(harness),
        [canonical(&r.old)],
        "the file was restored before the replacement failed"
    );

    let again = refresh(harness, false).await.expect_err("every rerun");
    assert_eq!(again.code(), "service_daemon_job_missing", "{again:?}");
    assert_daemon_job_missing(&again, FILE_UNTOUCHED, RECORD_ROLLING_BACK);

    harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("the way out: the session-checked uninstall takes the pending refresh");
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn the_entry_blocks_on_a_foreign_record_before_judging_the_schema() {
    let r = refreshable().await;
    let harness = &r.harness;
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Config);
    let error = engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("the upgrade is interrupted");
    assert!(matches!(error, Error::Interrupted(_)), "{error:?}");
    let text = String::from_utf8(config_bytes(harness)).expect("utf-8");
    let older = text.replacen("schema_version = 7", "schema_version = 6", 1);
    assert_ne!(older, text, "the fixture declares schema 7");
    std::fs::write(harness.context.config_path(), &older).expect("write schema 6");
    let record = record_bytes(harness);

    let error = entry_without_connection(&harness.context).await;

    assert_eq!(
        error.code(),
        "service_refresh_blocked_by_transaction",
        "{error}"
    );
    assert_eq!(record_bytes(harness), record, "the record is kept");
    assert_eq!(config_bytes(harness), older.as_bytes(), "the file is kept");
}

#[tokio::test]
async fn the_entry_returns_an_unreadable_record_before_judging_the_schema() {
    let r = refreshable().await;
    let harness = &r.harness;
    let text = String::from_utf8(config_bytes(harness)).expect("utf-8");
    let older = text.replacen("schema_version = 7", "schema_version = 6", 1);
    assert_ne!(older, text, "the fixture declares schema 7");
    std::fs::write(harness.context.config_path(), &older).expect("write schema 6");
    let record = harness.engine().store.path();
    std::fs::write(&record, b"not a record").expect("junk record");
    std::fs::set_permissions(&record, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("private record");

    let error = entry_without_connection(&harness.context).await;

    assert!(matches!(error, Error::Record { .. }), "{error:?}");
}

#[tokio::test]
async fn the_entry_lets_a_pending_refresh_record_reach_the_schema_judgement() {
    let r = refreshable().await;
    let harness = &r.harness;
    interrupt_after(harness, Step::Config).await;
    let text = String::from_utf8(config_bytes(harness)).expect("utf-8");
    let older = text.replacen("schema_version = 7", "schema_version = 6", 1);
    assert_ne!(older, text, "the fixture declares schema 7");
    std::fs::write(harness.context.config_path(), &older).expect("write schema 6");
    let record = record_bytes(harness);

    let error = entry_without_connection(&harness.context).await;

    assert_eq!(error.code(), "service_refresh_config_schema", "{error}");
    assert_eq!(record_bytes(harness), record, "the record is kept");
}

#[tokio::test]
async fn a_refresh_refused_for_the_store_or_the_preflight_words_the_refusal_as_a_refresh() {
    let r = refreshable().await;
    let harness = &r.harness;
    let before = config_bytes(harness);

    let error = refresh_with(
        harness,
        harness
            .engine()
            .with_adoption_preflight(Box::new(FailingFrom {
                inner: InProcessPreflight::default(),
                fail_from: 0,
            })),
        false,
    )
    .await
    .expect_err("the daemon gives no report");

    assert_eq!(
        error.code(),
        "service_upgrade_preflight_failed",
        "{error:?}"
    );
    assert!(
        matches!(error, Error::RefreshPreflightFailed { .. }),
        "{error:?}"
    );
    assert_steps_in_order(
        error.hint().expect("hint"),
        &[
            "`<prefix>/libexec/pohunek/<active>/pohunekd upgrade-preflight`",
            "the daemon of the installed release",
            "nothing was changed",
        ],
    );
    assert!(!error.hint().expect("hint").contains("<staged>"));

    seed_live_session(harness, SESSION, "w-1");
    edit_stored_line(harness, SESSION, |line| {
        line["schema_version"] = 99.into();
    });
    let path = store_path(harness);
    let body = std::fs::read_to_string(&path).expect("read store");
    std::fs::write(
        &path,
        body.replace("\"schema_version\":1", "\"schema_version\":99"),
    )
    .expect("write store");
    let error = refresh(harness, false)
        .await
        .expect_err("the store is refused");

    assert_eq!(error.code(), "service_upgrade_store_unusable", "{error:?}");
    assert!(
        matches!(error, Error::RefreshStoreUnusable { .. }),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(
        !message.contains("upgrade") && !message.contains("new daemon"),
        "{message}"
    );
    assert_steps_in_order(
        error.hint().expect("hint"),
        &[
            "--accept-runtime-loss does not apply",
            "the daemon of the installed release cannot read this store",
            "nothing was changed",
        ],
    );
    assert_eq!(config_bytes(harness), before);
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn a_record_of_another_schema_names_the_pohunek_that_started_the_transaction() {
    let r = refreshable().await;
    let harness = &r.harness;
    let record = harness.engine().store.path();
    std::fs::write(&record, br#"{"schema_version":99}"#).expect("future record");
    std::fs::set_permissions(&record, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("private record");

    let error = refresh(harness, false)
        .await
        .expect_err("unsupported record");

    assert_eq!(error.code(), "service_record_invalid", "{error:?}");
    assert!(
        error.to_string().contains("is unsupported; expected"),
        "{error}"
    );
    assert_steps_in_order(
        error.hint().expect("hint"),
        &[
            "another record schema",
            "rerun the command with the `pohunek` that started the transaction",
            "`./pohunek`",
            "`<prefix>/bin/pohunek`",
        ],
    );
}

fn put_agent(dir: &Path, name: &str) {
    pohunek_test_support::fs::write_executable(dir.join(name), "#!/bin/sh\nexit 0\n")
        .expect("write agent");
}

fn behind(context: &Context) -> Option<Vec<(&'static str, PathBuf)>> {
    crate::service::refresh::agents_behind_recorded_path(context).map(|agents| {
        agents
            .into_iter()
            .map(|agent| (agent.name, agent.found))
            .collect()
    })
}

#[tokio::test]
async fn an_agent_only_the_login_shell_path_resolves_is_reported() {
    let r = refreshable().await;
    put_agent(&r.old, "claude");
    put_agent(&r.new, "claude");
    put_agent(&r.new, "codex");

    assert_eq!(
        behind(&r.harness.context),
        Some(vec![("codex", canonical(&r.new).join("codex"))]),
        "claude resolves through the recorded path too"
    );
    assert_eq!(r.harness.pending(), None, "the probe writes no record");
}

#[tokio::test]
async fn an_agent_both_paths_resolve_is_not_reported() {
    let r = refreshable().await;
    for dir in [&r.old, &r.new] {
        put_agent(dir, "claude");
        put_agent(dir, "codex");
    }

    assert_eq!(behind(&r.harness.context), Some(Vec::new()));

    let only_recorded = refreshable().await;
    put_agent(&only_recorded.old, "claude");
    assert_eq!(
        behind(&only_recorded.harness.context),
        Some(Vec::new()),
        "a program only the recorded path resolves is not the finding"
    );
}

#[tokio::test]
async fn the_probe_is_silent_without_a_verdict() {
    let r = refreshable().await;
    put_agent(&r.new, "claude");
    let harness = &r.harness;

    let failing = scripted_discovery(
        &harness.context,
        &harness.root,
        "exit 1",
        pohunek_test_support::wait::HANG_GUARD,
        &r.old,
    );
    assert_eq!(behind(&failing), None, "a failed login shell");

    let unmanaged = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged);
    assert_eq!(behind(&unmanaged), None, "a host that records no path");

    let fresh = Harness::new();
    assert_eq!(behind(&fresh.context), None, "no installation");
}

#[tokio::test]
async fn the_probe_gives_no_verdict_while_a_record_is_pending_or_the_file_is_unreadable() {
    let r = refreshable().await;
    put_agent(&r.new, "claude");
    let harness = &r.harness;
    assert_eq!(behind(&harness.context).map(|a| a.len()), Some(1));

    interrupt_after(harness, Step::Config).await;
    assert_eq!(
        behind(&harness.context),
        None,
        "a refresh record is pending"
    );

    let engine = harness.engine();
    engine.store.clear().expect("clear the record");
    let text = String::from_utf8(config_bytes(harness)).expect("utf-8");
    std::fs::write(
        harness.context.config_path(),
        text.replacen("schema_version = 7", "schema_version = 6", 1),
    )
    .expect("write schema 6");
    assert_eq!(behind(&harness.context), None, "another schema");
}

/// A refresh rolling back over an active version directory that is refused or
/// missing: every refusal on the way names a command that makes progress, and
/// the two-step sequence ends with the record cleared and the daemon ready.
#[tokio::test]
async fn a_rolling_back_refresh_over_a_refused_or_missing_directory_is_repaired_in_two_steps() {
    for refused in [true, false] {
        let r = refreshable().await;
        let harness = &r.harness;
        let dir = harness.layout().version_dir(V1).expect("version directory");
        // The replacement is accepted but the old process keeps answering,
        // and the directory breaks right after it: the rollback meets it.
        harness.fake.world().kept_process_path = Some(canonical(&r.new).display().to_string());
        let broken = dir.clone();
        harness.fake.world().replace_hook = Some(ReplaceHook(Arc::new(move || {
            if refused {
                std::fs::set_permissions(
                    &broken,
                    std::os::unix::fs::PermissionsExt::from_mode(0o775),
                )
                .expect("loosen the version directory");
            } else {
                std::fs::remove_dir_all(&broken).expect("remove the version directory");
            }
        })));
        let way_out = [
            "`pohunek service upgrade --repair --from",
            "`service_refresh_pending`",
            "`pohunek service refresh-path`",
        ];

        let first = refresh(harness, false).await.expect_err("rollback fails");
        assert_eq!(first.code(), "service_rollback_failed", "{first:?}");
        assert!(
            matches!(
                &first,
                Error::RollbackFailed { rollback, .. }
                    if matches!(**rollback, Error::UntrustedRefreshVersion { .. })
            ),
            "{first:?}"
        );
        assert_steps_in_order(first.hint().expect("hint"), &way_out);
        let pending = record_bytes(harness).expect("the record is kept");
        assert!(harness.pending().expect("kept").rolling_back);
        let config = config_bytes(harness);
        let replaces_before = replaces(harness);

        let again = refresh(harness, false).await.expect_err("every rerun");
        assert_eq!(again.code(), "service_version_untrusted", "{again:?}");
        assert_steps_in_order(again.hint().expect("hint"), &way_out);

        if refused {
            let plain = harness.upgrade(V1).await.expect_err("needs the flag");
            assert_eq!(plain.code(), "service_version_untrusted", "{plain:?}");
            assert_steps_in_order(plain.hint().expect("hint"), &["upgrade --repair --from"]);
        }
        let upgrade = harness.engine().with_repair(refused);
        let pending_error = upgrade
            .upgrade(&harness.staged(V1), V1)
            .await
            .expect_err("the pending refresh still refuses");
        assert_eq!(
            pending_error.code(),
            "service_refresh_pending",
            "{pending_error:?}"
        );
        assert!(
            pending_error.to_string().contains("was restored"),
            "{pending_error}"
        );
        assert_steps_in_order(
            pending_error.hint().expect("hint"),
            &["`pohunek service refresh-path`"],
        );
        assert_eq!(record_bytes(harness).as_deref(), Some(pending.as_slice()));
        assert_eq!(config_bytes(harness), config, "service.toml is untouched");
        assert_eq!(replaces(harness), replaces_before, "no daemon effect");

        refresh(harness, false)
            .await
            .expect("the rollback finishes");
        assert_eq!(harness.pending(), None);
        assert!(
            crate::service::layout::verify_installed(&harness.layout(), &dir).is_ok(),
            "the version directory is trusted"
        );
        assert_eq!(recorded(harness), [canonical(&r.old)]);
        assert!(harness.fake.world().running);
    }
}
