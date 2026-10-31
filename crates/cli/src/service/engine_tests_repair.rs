//! Scenarios of `upgrade --repair` and the `install` resume repair at the
//! engine boundary.
//!
//! The service manager is the in-memory [`Fake`]; the version directories,
//! `service.toml` and the record are real files below a temporary root, so
//! every refusal is the one `verify_installed` gives for a real mode change.

use std::os::unix::fs::PermissionsExt as _;

use pohunek_paths::valid_install_version;

use super::*;

/// The version directory of `version`.
fn version_dir(harness: &Harness, version: &str) -> PathBuf {
    harness
        .layout()
        .version_dir(version)
        .expect("version directory")
}

/// Makes `version`'s directory group-writable, which `verify_installed` refuses.
fn refuse(harness: &Harness, version: &str) {
    std::fs::set_permissions(
        version_dir(harness, version),
        std::fs::Permissions::from_mode(0o775),
    )
    .expect("loosen the version directory");
}

fn is_trusted(harness: &Harness, version: &str) -> bool {
    layout::verify_installed(&harness.layout(), &version_dir(harness, version)).is_ok()
}

fn refused_entries(harness: &Harness) -> Vec<PathBuf> {
    layout::refused_entries(&harness.layout()).expect("refused entries")
}

fn config_bytes(harness: &Harness) -> Vec<u8> {
    std::fs::read(harness.context.config_path()).expect("service.toml")
}

/// Installed daemon-job calls so far: installs, replaces and removals.
fn daemon_calls(harness: &Harness) -> (usize, usize, usize) {
    let world = harness.fake.world();
    (world.installs, world.replaces, world.uninstalls)
}

/// Removes the daemon job from the service manager, as an operator's `systemctl
/// disable` or a deleted agent plist does.
fn vanish_daemon_job(harness: &Harness) {
    let mut world = harness.fake.world();
    world.registered = None;
    world.running = false;
}

/// Upgrades to `version` with the prepared `engine` from the staged release.
async fn repair_with(
    harness: &Harness,
    engine: Engine<'_>,
    version: &str,
) -> Result<UpgradeReport, Error> {
    engine
        .with_repair(true)
        .upgrade(&harness.staged(version), version)
        .await
}

/// An installation of [`V1`] whose daemon is stopped, one live worker, and a
/// refused active version directory.
async fn refused_with_live_worker() -> Harness {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.seed(
        vec![session(SESSION, Some("review"))],
        vec![worker(worker_id(SESSION), ServiceState::Running)],
    );
    harness.fake.world().running = false;
    refuse(&harness, V1);
    harness
}

#[tokio::test]
async fn repair_replaces_a_refused_active_directory_and_keeps_the_worker_and_the_settings() {
    let harness = refused_with_live_worker().await;
    let settings = config_bytes(&harness);
    let worker_before = harness.fake.world().workers.clone();
    let calls = daemon_calls(&harness);

    let report = repair_with(&harness, harness.engine(), V1)
        .await
        .expect("repair");

    assert!(report.unchanged);
    let repair = report.repair.expect("the repair block");
    assert!(repair.restored);
    assert_eq!(repair.daemon, "started");
    let moved = repair.set_aside.expect("the refused entry is set aside");
    assert!(
        moved
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".pohunek_refused-1.0.0-")),
        "{moved:?}"
    );
    assert_eq!(refused_entries(&harness), std::slice::from_ref(&moved));
    assert_eq!(report.refused_entries, [moved]);
    assert!(is_trusted(&harness, V1), "the published copy is trusted");
    assert_eq!(
        config_bytes(&harness),
        settings,
        "service.toml is untouched"
    );
    assert_eq!(harness.fake.world().workers, worker_before, "worker kept");
    let after = daemon_calls(&harness);
    assert_eq!(
        (after.0, after.1, after.2),
        (calls.0, calls.1 + 1, calls.2),
        "the daemon job was started once"
    );
    harness.assert_installed(V1);

    let removed = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect("the uninstall now reaches the daemon");
    assert_eq!(removed.stopped_sessions, [SESSION]);
    assert_eq!(removed.refused_entries.len(), 1, "the evidence is reported");
}

#[tokio::test]
async fn upgrade_without_repair_refuses_a_refused_directory_and_changes_nothing() {
    let harness = refused_with_live_worker().await;
    let calls = daemon_calls(&harness);

    let error = harness
        .upgrade(V1)
        .await
        .expect_err("a refused directory needs --repair");

    assert_eq!(error.code(), "service_version_untrusted");
    assert!(
        error.hint().expect("hint").contains("--repair"),
        "{error:?}"
    );
    assert!(refused_entries(&harness).is_empty(), "nothing was renamed");
    assert!(!is_trusted(&harness, V1), "the directory is as it was");
    assert_eq!(daemon_calls(&harness), calls, "no daemon call");
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn upgrade_restores_a_missing_active_directory_without_a_flag() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.world().running = false;
    std::fs::remove_dir_all(version_dir(&harness, V1)).expect("remove the active directory");
    let calls = daemon_calls(&harness);

    let report = harness.upgrade(V1).await.expect("restore");

    let repair = report.repair.expect("the repair block");
    assert!(repair.restored);
    assert_eq!(repair.set_aside, None, "nothing existed to set aside");
    assert_eq!(repair.daemon, "started");
    assert_eq!(daemon_calls(&harness).1, calls.1 + 1);
    assert!(refused_entries(&harness).is_empty());
    harness.assert_installed(V1);
}

#[tokio::test]
async fn repair_moves_a_symbolic_link_aside_and_never_follows_it() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.world().running = false;
    let dir = version_dir(&harness, V1);
    let elsewhere = harness.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("link target");
    std::fs::write(elsewhere.join("keep"), b"untouched").expect("target file");
    std::fs::remove_dir_all(&dir).expect("remove the directory");
    std::os::unix::fs::symlink(&elsewhere, &dir).expect("link in place of the directory");

    let report = repair_with(&harness, harness.engine(), V1)
        .await
        .expect("repair");

    let moved = report
        .repair
        .and_then(|repair| repair.set_aside)
        .expect("the link is set aside");
    assert!(
        std::fs::symlink_metadata(&moved)
            .expect("the moved entry")
            .file_type()
            .is_symlink(),
        "the link itself moved"
    );
    assert_eq!(
        std::fs::read(elsewhere.join("keep")).expect("target"),
        b"untouched"
    );
    assert!(is_trusted(&harness, V1));
    harness.assert_installed(V1);
}

/// A running daemon is judged before any rename and restarted as a new
/// process; a refusal changes nothing.
#[tokio::test]
async fn repair_gates_a_running_daemon_before_it_moves_anything() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    refuse(&harness, V1);
    let calls = daemon_calls(&harness);
    let starts = harness.fake.world().restarts;

    let refused = repair_with(
        &harness,
        harness
            .engine()
            .with_adoption_preflight(judging(&[risk_a()])),
        V1,
    )
    .await
    .expect_err("the at-risk session refuses the restart");
    assert!(
        matches!(refused, Error::UpgradeAtRisk { .. }),
        "{refused:?}"
    );
    assert!(refused_entries(&harness).is_empty(), "nothing was renamed");
    assert!(!is_trusted(&harness, V1));
    assert_eq!(daemon_calls(&harness), calls, "the daemon was not replaced");

    let preflight = judging(&[risk_a()]);
    let plans = Arc::clone(&preflight.plans);
    let report = repair_with(
        &harness,
        harness
            .engine()
            .with_runtime_loss_accepted(true)
            .with_adoption_preflight(preflight),
        V1,
    )
    .await
    .expect("the accepted loss lets the repair proceed");
    assert_eq!(report.repair.expect("repair").daemon, "restarted");
    assert!(report.accepted_runtime_loss);
    assert_eq!(
        plans.lock().expect("plans").as_slice(),
        [ReleasePlan::Omitted],
        "the release is judged without a runtime plan"
    );
    assert_eq!(daemon_calls(&harness).1, calls.1 + 1, "replaced once");
    assert_eq!(
        harness.fake.world().restarts,
        starts + 1,
        "the daemon is a new process"
    );
    assert_eq!(refused_entries(&harness).len(), 1);
    harness.assert_installed(V1);
}

/// A rerun after a crash between the publish and the replacement still
/// restarts the daemon, because the flag is given again.
#[tokio::test]
async fn a_repair_rerun_restarts_the_daemon_over_a_trusted_directory() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let calls = daemon_calls(&harness);

    let plain = harness.upgrade(V1).await.expect("a plain rerun");
    assert_eq!(
        daemon_calls(&harness),
        calls,
        "a plain upgrade restarts nothing"
    );
    assert_eq!(plain.repair, None);

    let report = repair_with(&harness, harness.engine(), V1)
        .await
        .expect("repair rerun");
    let repair = report.repair.expect("repair");
    assert!(!repair.restored);
    assert_eq!(repair.daemon, "restarted");
    assert_eq!(daemon_calls(&harness).1, calls.1 + 1);
}

#[tokio::test]
async fn a_refused_directory_is_never_a_source() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.world().running = false;
    let dir = version_dir(&harness, V1);
    refuse(&harness, V1);

    for from in [dir.clone(), dir.join("nested")] {
        let error = harness
            .engine()
            .with_repair(true)
            .upgrade(&from, V1)
            .await
            .expect_err("the installed directory is not a source while refused");
        assert_eq!(error.code(), "service_version_untrusted", "{from:?}");
    }

    repair_with(&harness, harness.engine(), V1)
        .await
        .expect("repair");
    let moved = refused_entries(&harness).remove(0);
    // Even once its mode looks right again, a set-aside entry is not a source.
    std::fs::set_permissions(&moved, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let error = harness
        .engine()
        .with_repair(true)
        .upgrade(&moved, V1)
        .await
        .expect_err("a set-aside entry is not a source");
    assert_eq!(error.code(), "service_version_untrusted");
    assert!(is_trusted(&harness, V1));
}

#[tokio::test]
async fn repair_sets_a_refused_target_directory_aside_before_an_upgrade_to_it() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let leftover = version_dir(&harness, V2);
    std::fs::create_dir(&leftover).expect("leftover target directory");
    std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o775))
        .expect("loosen the leftover");

    let error = harness
        .upgrade(V2)
        .await
        .expect_err("a refused target needs --repair");
    assert_eq!(error.code(), "service_version_untrusted");
    assert_eq!(harness.pending(), None);

    let report = repair_with(&harness, harness.engine(), V2)
        .await
        .expect("upgrade over the leftover");
    assert!(!report.unchanged);
    assert_eq!(refused_entries(&harness).len(), 1);
    harness.assert_installed(V2);
}

/// The refusal of an uninstall that cannot reach the daemon names the version
/// and the way out.
#[tokio::test]
async fn uninstall_over_a_refused_active_directory_with_a_live_worker_names_the_recovery() {
    let harness = refused_with_live_worker().await;
    let calls = daemon_calls(&harness);

    let error = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect_err("the daemon cannot start");

    assert!(
        matches!(&error, Error::UntrustedActiveVersion { version, .. } if version == V1),
        "{error:?}"
    );
    assert_eq!(error.code(), "service_version_untrusted");
    assert!(error.to_string().contains(V1), "{error}");
    let hint = error.hint().expect("hint");
    assert!(hint.contains("upgrade --repair"), "{hint}");
    assert!(hint.contains("--stop-sessions"), "{hint}");
    assert!(!hint.contains("install it again"), "{hint}");
    assert_eq!(daemon_calls(&harness), calls, "nothing was started");
    assert!(harness.config().is_some(), "the installation is intact");
}

/// A rollback to a version whose directory vanished publishes it first.
#[tokio::test]
async fn a_rollback_to_a_missing_previous_directory_publishes_it_before_the_rollback() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("the upgrade to the second version is interrupted");
    std::fs::remove_dir_all(version_dir(&harness, V1)).expect("remove the previous directory");

    let report = harness
        .upgrade(V1)
        .await
        .expect("the upgrade to the previous version rolls back");

    assert!(report.rolled_back.is_some(), "{report:?}");
    assert!(report.repair.expect("repair").restored);
    harness.assert_installed(V1);
}

/// A refused previous directory refuses the rollback without the flag, with
/// the record intact, and is set aside with it.
#[tokio::test]
async fn a_rollback_to_a_refused_previous_directory_needs_the_flag() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("the upgrade to the second version is interrupted");
    refuse(&harness, V1);
    let calls = daemon_calls(&harness);

    let error = harness
        .upgrade(V1)
        .await
        .expect_err("the refused previous directory refuses the rollback");
    assert_eq!(error.code(), "service_version_untrusted");
    assert!(harness.pending().is_some(), "the record is kept");
    assert_eq!(daemon_calls(&harness), calls);
    assert!(refused_entries(&harness).is_empty());

    let report = repair_with(&harness, harness.engine(), V1)
        .await
        .expect("repair finishes the rollback");
    assert!(report.rolled_back.is_some());
    assert_eq!(refused_entries(&harness).len(), 1);
    harness.assert_installed(V1);
}

#[tokio::test]
async fn a_resumed_upgrade_replaces_a_refused_target_directory() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Ready);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("only the CLI step is left");
    refuse(&harness, V2);

    let error = harness
        .upgrade(V2)
        .await
        .expect_err("the refused target refuses the resume");
    assert_eq!(error.code(), "service_version_untrusted");
    assert!(harness.pending().is_some());

    let report = repair_with(&harness, harness.engine(), V2)
        .await
        .expect("repair resumes");
    assert!(report.resumed);
    assert_eq!(refused_entries(&harness).len(), 1);
    harness.assert_installed(V2);
}

#[tokio::test]
async fn an_install_resume_replaces_a_refused_version_directory_with_the_flag() {
    let harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    refuse(&harness, V1);

    let error = harness
        .install(V1)
        .await
        .expect_err("the refused directory refuses the resume");
    assert_eq!(error.code(), "service_version_untrusted");
    assert!(error.hint().expect("hint").contains("--repair"));
    assert!(harness.pending().is_some());

    let report = harness
        .engine()
        .with_repair(true)
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect("the resume repairs");
    assert!(report.resumed);
    assert!(report.repair.expect("repair").set_aside.is_some());
    harness.assert_installed(V1);
}

/// A vanished daemon job is registered again from `service.toml`, and a
/// trusted directory with a running job restarts nothing.
#[tokio::test]
async fn upgrade_registers_a_missing_daemon_job_again_and_restarts_nothing_else() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let settings = config_bytes(&harness);
    let calls = daemon_calls(&harness);
    harness.upgrade(V1).await.expect("a plain rerun");
    assert_eq!(daemon_calls(&harness), calls, "a running job is left alone");

    vanish_daemon_job(&harness);
    let report = harness.upgrade(V1).await.expect("register the job again");

    assert_eq!(report.repair.expect("repair").daemon, "installed");
    assert_eq!(harness.fake.world().installs, calls.0 + 1);
    assert_eq!(config_bytes(&harness), settings);
    harness.assert_installed(V1);
}

/// A directory the owner cannot even open is refused and moved aside by the
/// rename in its parent. macOS refuses to rename a directory its owner has no
/// permissions on, so the operator makes it accessible first.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn repair_sets_aside_a_directory_the_owner_cannot_open() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.world().running = false;
    std::fs::set_permissions(
        version_dir(&harness, V1),
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("close the version directory");

    let report = repair_with(&harness, harness.engine(), V1)
        .await
        .expect("repair");

    assert!(report.repair.expect("repair").set_aside.is_some());
    harness.assert_installed(V1);
    for entry in refused_entries(&harness) {
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755))
            .expect("reopen the set-aside entry for cleanup");
    }
}

/// The set-aside name never parses as a version, so every release lists,
/// collects and refuses it as a non-version.
#[tokio::test]
async fn a_set_aside_name_is_not_a_version() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.world().running = false;
    refuse(&harness, V1);

    let report = repair_with(&harness, harness.engine(), V1)
        .await
        .expect("repair");

    let moved = report
        .repair
        .and_then(|repair| repair.set_aside)
        .expect("the refused entry is set aside");
    let name = moved
        .file_name()
        .and_then(|name| name.to_str())
        .expect("name");
    assert_eq!(valid_install_version(name), None, "{name}");
    assert_eq!(
        layout::installed_versions(&harness.layout()).expect("versions"),
        [V1]
    );
}

/// Readiness names the process the replacement must retire: a daemon that
/// keeps its main process after `replace` is not ready.
#[tokio::test]
async fn repair_rejects_a_daemon_that_keeps_its_old_process() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    refuse(&harness, V1);
    harness.fake.world().keeps_process = true;

    let error = repair_with(&harness, harness.engine(), V1)
        .await
        .expect_err("the old process still answers");

    assert!(matches!(error, Error::DaemonNotReady { .. }), "{error:?}");
}

/// An automatic rollback that meets a refused previous directory carries the
/// `--repair` hint of the inner refusal.
#[tokio::test]
async fn a_rollback_onto_a_refused_directory_names_the_repair() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let dir = version_dir(&harness, V1);
    {
        let mut world = harness.fake.world();
        world.silent_version = Some(V2.to_owned());
        world.replace_hook = Some(ReplaceHook(Arc::new(move || {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775))
                .expect("loosen the previous version directory");
        })));
    };

    let error = harness.upgrade(V2).await.expect_err("not ready");

    let Error::RollbackFailed { rollback, .. } = &error else {
        panic!("unexpected error {error:?}");
    };
    assert!(
        matches!(&**rollback, Error::UntrustedVersion { .. }),
        "{rollback:?}"
    );
    assert!(
        error
            .hint()
            .is_some_and(|hint| hint.contains("upgrade --repair")),
        "{:?}",
        error.hint()
    );
}

/// A live worker that runs from a set-aside directory and is covered by no
/// journal still counts as an unattributed worker of a legacy rollback.
#[tokio::test]
async fn a_worker_in_a_set_aside_directory_is_unattributed_for_the_rollback() {
    let previous = "0.33.0";
    let harness = Harness::new();
    harness.install(previous).await.expect("install");
    harness.fake.world().running = false;
    refuse(&harness, previous);
    let moved = repair_with(&harness, harness.engine(), previous)
        .await
        .expect("repair")
        .repair
        .and_then(|repair| repair.set_aside)
        .expect("set aside");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after replacement");
    let worker = moved.join("pohunek-sessiond");
    pohunek_test_support::hold::install_at(&worker).expect("install the holding image");
    let mut child = spawn_tracked(&mut pohunek_test_support::hold::command_for(&worker));

    let refused = harness.upgrade(previous).await;
    child.kill().expect("kill stand-in");
    child.wait().expect("reap stand-in");

    let error = refused.expect_err("the unattributed worker needs consent");
    assert_eq!(error.code(), "service_rollback_sessions_at_risk");
    assert!(
        error.to_string().contains("unattributed live worker"),
        "{error}"
    );
}
