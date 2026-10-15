//! Bare `pohunek setup` against the real native service manager.
//!
//! The setup runs in-process against an isolated installation, the same way
//! the `service_*` tests drive the engine. The service, the daemon, the RPCs,
//! the hooks and the config are real. Most scenarios script the release
//! planner with canned reports, because they need a particular plan. The
//! fresh-host scenario at the end runs the production planner, the release
//! daemon's `ensure-official-runtimes --detect`, over real package archives.
//!
//! A fixture daemon cannot stand in for the release daemon, because the
//! supervisor refuses a main process whose executable differs from the unit's
//! `ExecStart`.
//!
//! Linux drives the systemd user manager and is ignored unless
//! `POHUNEK_SYSTEMD_E2E=1` opts in; macOS drives `gui/<uid>` in the ordinary
//! test run. See `support/native.rs` for isolation and binary lookup.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-10-14

#[path = "support/native.rs"]
mod native;
#[path = "support/sealed_archive.rs"]
mod sealed_archive;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

use ed25519_dalek::SigningKey;
use native::Installation;
use package::directory::build_directory_archive;
use package::{read_archive, CatalogEntry, Limits, PackageDigest, ANCHOR_FILE_NAME};

use pohunek_cli::host_setup::{apply, plan, ReleasePlanner, ServiceStep, SetupReport};
use pohunek_cli::service::context::PathDiscovery;
use pohunek_cli::service::runtimes::DaemonRuntimes;
use pohunek_cli::service::{self, Context, VERSION};
use pohunek_daemon::agent::host::definition_from_archive;
use pohunek_service_config::runtimes::{EnsureReport, PackageStatus};
use pohunek_test_support::catalog::{
    anchor_for, attestations_for, catalog_of, host_platform, signed, test_key, ANY_CORE,
    RUNTIME_API, WINDOW_END,
};
use pohunek_test_support::fs::copy_shell_image;
use pohunek_test_support::workspace_root;
use protocol::{method, BindingProvenance, RuntimeId, SessionId, SessionNewParams};
use serde_json::{json, Value};

/// Error code of a setup that stopped after the service install.
const PLAN_CHANGED_CODE: &str = "setup_plan_changed_after_install";

/// Name of the pohunek config directory below the XDG config root.
const CONFIG_DIR: &str = "pohunek";

/// Hermes home the default selection reads, below the isolated home.
const HERMES_HOME: &str = ".hermes";

/// Planner whose reports are returned in order; the last one repeats.
///
/// Stands in for the release's `pohunekd ensure-official-runtimes --detect`.
#[derive(Debug)]
struct ScriptedPlanner {
    reports: Vec<EnsureReport>,
    calls: AtomicUsize,
}

impl ScriptedPlanner {
    fn new(reports: Vec<EnsureReport>) -> Self {
        Self {
            reports,
            calls: AtomicUsize::new(0),
        }
    }
}

impl ReleasePlanner for ScriptedPlanner {
    fn plan<'a>(
        &'a self,
        _context: &'a Context,
        _dir: &'a Path,
        _search_path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<EnsureReport, service::Error>> + Send + 'a>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let index = call.min(self.reports.len() - 1);
        let report = self.reports[index].clone();
        Box::pin(async move { Ok(report) })
    }
}

/// A planner report that lists `integrations` and nothing else.
fn report(integrations: &Value) -> EnsureReport {
    serde_json::from_value(json!({
        "report_version": 1,
        "daemon_version": VERSION,
        "dry_run": true,
        "packages": [],
        "catalog": "unavailable",
        "profiles": [],
        "integrations": integrations,
        "refusals": [],
    }))
    .expect("a planner report of this layout")
}

/// A planner report in which the official Hermes package is present, the
/// way the release step plans a host whose `hermes` program it detected. It
/// lists no Hermes home: setup plans those itself.
fn hermes_report() -> EnsureReport {
    planned(
        &json!([{
            "runtime": "hermes",
            "package": "hermes-agent",
            "version": "1.0.0",
            "digest": format!("sha256:{}", "b".repeat(64)),
            "status": "present",
            "reasons": ["detected"],
        }]),
        &json!([]),
    )
}

/// The release directory of the installation: this build's binaries.
fn release(installation: &Installation) -> PathBuf {
    installation.stage()
}

/// Snapshot of a directory tree: each entry's bytes, or `None` for a directory.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(listing) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in listing {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("below root").to_path_buf();
            if entry.file_type().expect("file type").is_dir() {
                entries.insert(relative, None);
                pending.push(path);
            } else {
                entries.insert(relative, Some(fs::read(&path).expect("read file")));
            }
        }
    }
    entries
}

/// Creates an unmarked Hermes home with one owner file in it.
fn unmarked_hermes_home(installation: &Installation) -> PathBuf {
    let home = installation.root.join("home").join(HERMES_HOME);
    fs::create_dir_all(home.join("plugins")).expect("hermes home");
    fs::write(home.join("config.yaml"), "owner: value\n").expect("owner file");
    home
}

/// The config directory of the installation.
fn config_dir(installation: &Installation) -> PathBuf {
    installation.root.join("config").join(CONFIG_DIR)
}

/// Whether the installation's service is installed at this release's version.
async fn installed_at_this_version(installation: &Installation) -> bool {
    let status = service::status_in(&installation.context)
        .await
        .expect("status of the installation");
    status.installed && status.active_version.as_deref() == Some(VERSION)
}

/// A Hermes home on a host whose plan serves no Hermes package is neither
/// planned nor touched: setup installs Hermes hooks only for a planned Hermes.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn a_hermes_home_without_a_planned_hermes_package_is_left_alone() {
    let installation = Installation::new();
    let release = release(&installation);
    let planner = ScriptedPlanner::new(vec![report(&json!([]))]);
    let home = unmarked_hermes_home(&installation);
    let home_before = snapshot(&home);

    let host_plan = plan(&installation.context, &release, &planner)
        .await
        .expect("plan the host");
    assert!(
        host_plan.preview.hermes_homes.is_empty(),
        "no Hermes home is planned without a planned Hermes package"
    );

    let outcome = apply(&installation.context, host_plan, &planner)
        .await
        .expect("setup applies");
    assert!(outcome.verdict.hook_gaps.is_empty());
    assert!(outcome.verdict.hooks_failed.is_empty());
    assert_eq!(snapshot(&home), home_before, "the Hermes home is untouched");
}

/// With a Hermes home that has no pohunek marker, setup applies, leaves the
/// home byte-for-byte as it was, reports the gap with its install command,
/// passes the verdict, and keeps an existing config file.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn unmarked_hermes_home_is_a_gap_and_existing_config_is_kept() {
    let installation = Installation::new();
    let release = release(&installation);
    let planner = ScriptedPlanner::new(vec![hermes_report()]);
    let home = unmarked_hermes_home(&installation);
    fs::create_dir_all(home.join("profiles").join("work")).expect("unmarked profile home");
    let home_before = snapshot(&home);
    fs::create_dir_all(config_dir(&installation)).expect("config dir");
    let attach = config_dir(&installation).join("attach.conf");
    fs::write(&attach, "user-edited").expect("existing config");

    let host_plan = plan(&installation.context, &release, &planner)
        .await
        .expect("plan the host");
    assert!(matches!(
        host_plan.preview.service,
        ServiceStep::Install { .. }
    ));
    assert_eq!(
        host_plan.preview.hermes_homes.len(),
        2,
        "the default home and one profile home are both planned"
    );
    assert!(
        host_plan
            .preview
            .hermes_homes
            .iter()
            .all(|home| home.status == "gap"),
        "both are unmarked, so both are gaps"
    );
    let gap = host_plan.preview.hermes_homes[0]
        .detail
        .clone()
        .unwrap_or_default();
    assert!(
        gap.contains("pohunek integration hermes install"),
        "the gap names its install command: {gap}"
    );

    let outcome = apply(&installation.context, host_plan, &planner)
        .await
        .expect("setup applies");
    assert!(outcome.verdict.hooks_failed.is_empty(), "no hook failed");
    assert_eq!(outcome.verdict.hook_gaps.len(), 2, "both gaps are reported");
    assert!(outcome.verdict.packages_missing.is_empty());
    assert_eq!(outcome.verdict.health, "ok");
    assert_eq!(
        snapshot(&home),
        home_before,
        "setup writes nothing into an unmarked Hermes home"
    );
    assert_eq!(
        fs::read_to_string(&attach).expect("read config"),
        "user-edited",
        "an existing config file is kept"
    );
    assert!(outcome
        .config
        .skipped
        .iter()
        .any(|path| path.ends_with("attach.conf")));
    assert!(
        outcome
            .applied
            .iter()
            .any(|line| line.starts_with("config created: ") && line.ends_with("issue.tmpl")),
        "the config files created are among the applied steps: {:?}",
        outcome.applied
    );
    assert!(outcome
        .config
        .created
        .iter()
        .any(|path| path.ends_with("issue.tmpl")));
    assert!(installed_at_this_version(&installation).await);
}

/// A plan read after the install that differs from the preview stops setup
/// with the installed service and nothing else changed. Fixing the cause and
/// re-running converges: the re-run sees the installed service, takes the
/// present step, and plans from the installed directory.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn a_plan_that_changes_after_the_install_stops_then_converges_on_rerun() {
    let installation = Installation::new();
    let release = release(&installation);
    // The preview shows no integrations; the plan read after the install
    // lists Hermes, and it keeps listing it once the cause is fixed.
    let planner = ScriptedPlanner::new(vec![report(&json!([])), hermes_report()]);
    let host_plan = plan(&installation.context, &release, &planner)
        .await
        .expect("plan the host");

    let error = apply(&installation.context, host_plan, &planner)
        .await
        .expect_err("the changed plan stops setup");
    assert_eq!(error.code(), PLAN_CHANGED_CODE);
    let message = error.to_string();
    assert!(
        message.contains(VERSION),
        "names the installed version: {message}"
    );
    assert!(
        message.contains("Nothing else was changed.") && message.contains("Changes made:"),
        "states what was changed and what was left alone: {message}"
    );
    assert!(error
        .recover()
        .is_some_and(|next| next.contains("re-run `pohunek setup`")));
    assert!(
        installed_at_this_version(&installation).await,
        "the service is kept"
    );
    assert!(
        !config_dir(&installation).join("attach.conf").exists(),
        "no config file was written"
    );

    // The cause is fixed: the installed plan and the preview agree.
    let rerun = plan(&installation.context, &release, &planner)
        .await
        .expect("the re-run plans");
    assert_eq!(
        rerun.preview.service,
        ServiceStep::Present,
        "the re-run takes the present step"
    );
    let converged = apply(&installation.context, rerun, &planner)
        .await
        .expect("the re-run converges");
    assert_eq!(converged.preview.service, ServiceStep::Present);
    assert!(
        converged
            .applied
            .iter()
            .all(|line| !line.starts_with("service: installed")),
        "the re-run does not reinstall the service"
    );
    assert_eq!(
        converged.config.created.len(),
        3,
        "the config is written on the re-run"
    );
}

/// A second setup on a configured host changes nothing: the service is kept,
/// every config file is kept, the Hermes gap is listed again, and the verdict
/// passes.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn a_second_setup_on_a_configured_host_is_a_no_op() {
    let installation = Installation::new();
    let release = release(&installation);
    let planner = ScriptedPlanner::new(vec![hermes_report()]);
    let home = unmarked_hermes_home(&installation);

    let first = plan(&installation.context, &release, &planner)
        .await
        .expect("first plan");
    apply(&installation.context, first, &planner)
        .await
        .expect("first setup applies");
    let home_after_first = snapshot(&home);
    let config_after_first = snapshot(&config_dir(&installation));

    let second_plan = plan(&installation.context, &release, &planner)
        .await
        .expect("second plan");
    assert_eq!(second_plan.preview.service, ServiceStep::Present);
    let second = apply(&installation.context, second_plan, &planner)
        .await
        .expect("second setup applies");
    assert!(second
        .applied
        .iter()
        .all(|line| !line.starts_with("service:")));
    assert!(
        second.config.created.is_empty(),
        "no config file is rewritten"
    );
    assert_eq!(second.config.skipped.len(), 3);
    assert_eq!(
        second.verdict.hook_gaps.len(),
        1,
        "the default home is still a gap"
    );
    assert!(second.verdict.hooks_failed.is_empty());
    assert_eq!(
        snapshot(&home),
        home_after_first,
        "the Hermes home is unchanged"
    );
    assert_eq!(
        snapshot(&config_dir(&installation)),
        config_after_first,
        "the config directory is unchanged"
    );
}

/// A planner report with the given packages and profiles and no integrations.
fn planned(packages: &Value, profiles: &Value) -> EnsureReport {
    serde_json::from_value(json!({
        "report_version": 1,
        "daemon_version": VERSION,
        "dry_run": true,
        "packages": packages,
        "catalog": "unavailable",
        "profiles": profiles,
        "integrations": [],
        "refusals": [],
    }))
    .expect("a planner report of this layout")
}

/// A Claude package the release lists, with the given planned status.
fn claude(status: &str) -> Value {
    json!([{
        "runtime": "claude",
        "package": "claude-code",
        "version": "1.0.0",
        "digest": format!("sha256:{}", "a".repeat(64)),
        "status": status,
        "reasons": ["profile"],
    }])
}

/// An unpinned profile over the Claude runtime, to be pinned.
fn unpinned_profile() -> Value {
    json!([{
        "name": "consumer",
        "base": "claude",
        "digest": format!("sha256:{}", "a".repeat(64)),
        "status": "bind",
    }])
}

/// A profile the preview pins that leaves the plan after the install is a
/// mismatch unless the install engine reports it bound. Here the engine
/// reports nothing bound (this release has no catalog for the package, so the
/// engine's own runtime step does nothing), so the stop names the profile and
/// the service is the only change.
///
/// The accepted transition, where the engine reports the package installed and
/// the profile bound, cannot be produced here: the engine's runtime step needs a
/// signed release catalog with real archives, which this branch's harness does
/// not build. That path is covered only by the release harness.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn a_profile_that_vanishes_without_an_engine_pin_stops_setup() {
    let installation = Installation::new();
    let release = release(&installation);
    let preview = planned(&claude("present"), &unpinned_profile());
    let after = planned(&claude("present"), &json!([]));
    let planner = ScriptedPlanner::new(vec![preview, after]);

    let host_plan = plan(&installation.context, &release, &planner)
        .await
        .expect("plan the host");
    let error = apply(&installation.context, host_plan, &planner)
        .await
        .expect_err("a pin the engine did not make stops setup");
    assert_eq!(error.code(), PLAN_CHANGED_CODE);
    let message = error.to_string();
    assert!(
        message.contains(
            "profile consumer left the plan after the install, but the install engine did not pin it"
        ),
        "the stop names the profile: {message}"
    );
    assert!(
        message.contains("Changes made: service: installed version"),
        "{message}"
    );
    assert!(
        !config_dir(&installation).join("attach.conf").exists(),
        "no config file was written"
    );
    assert!(
        installed_at_this_version(&installation).await,
        "the service is kept"
    );
}

/// A runtime the preview planned that the plan after the apply omits fails
/// the verdict and is named. The service stays installed and the changes made
/// before the stop are listed.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn a_verdict_plan_that_drops_a_previewed_runtime_fails_and_names_it() {
    let installation = Installation::new();
    let release = release(&installation);
    let preview = planned(&claude("present"), &json!([]));
    let after_install = planned(&claude("present"), &json!([]));
    let verdict_plan = planned(&json!([]), &json!([]));
    let planner = ScriptedPlanner::new(vec![preview, after_install, verdict_plan]);

    let host_plan = plan(&installation.context, &release, &planner)
        .await
        .expect("plan the host");
    let error = apply(&installation.context, host_plan, &planner)
        .await
        .expect_err("the verdict fails");
    assert_eq!(error.code(), "setup_incomplete");
    let message = error.to_string();
    assert!(
        message.contains("runtimes not present after the apply: claude"),
        "the verdict names the dropped runtime: {message}"
    );
    assert!(message.contains("Changes made: service: installed version"));
    assert!(
        installed_at_this_version(&installation).await,
        "the service is kept"
    );
}

/// A Hermes home the preview listed that is gone when the apply runs fails the
/// verdict and is named. A home that appeared after the preview is not touched.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn a_hermes_home_that_vanishes_after_the_preview_fails_and_a_new_one_is_untouched() {
    let installation = Installation::new();
    let release = release(&installation);
    let planner = ScriptedPlanner::new(vec![hermes_report()]);
    let home = unmarked_hermes_home(&installation);
    let profiles = home.join("profiles");
    fs::create_dir_all(profiles.join("work")).expect("previewed profile home");

    let host_plan = plan(&installation.context, &release, &planner)
        .await
        .expect("plan the host");
    assert!(host_plan
        .preview
        .hermes_homes
        .iter()
        .any(|shown| shown.integration == "hermes profile work"));

    // Between consent and apply: the previewed home goes, and a new one appears.
    fs::remove_dir_all(profiles.join("work")).expect("remove previewed home");
    fs::create_dir_all(profiles.join("late")).expect("new profile home");
    fs::write(profiles.join("late").join("owner.yaml"), "owner: value\n").expect("new file");
    let late_before = snapshot(&profiles.join("late"));

    let error = apply(&installation.context, host_plan, &planner)
        .await
        .expect_err("the vanished home fails the verdict");
    assert_eq!(error.code(), "setup_incomplete");
    let message = error.to_string();
    assert!(
        message.contains("hermes profile work"),
        "the vanished home is named: {message}"
    );
    assert!(
        !message.contains("hermes profile late"),
        "the new home is not reported as applied: {message}"
    );
    assert_eq!(
        snapshot(&profiles.join("late")),
        late_before,
        "the home that appeared after the preview is untouched"
    );
}

/// A profiles directory that cannot be read is a failure in the preview, not
/// an empty list of profile homes.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn an_unreadable_profiles_directory_is_reported_in_the_preview() {
    let installation = Installation::new();
    let release = release(&installation);
    let planner = ScriptedPlanner::new(vec![hermes_report()]);
    let home = unmarked_hermes_home(&installation);
    let profiles = home.join("profiles");
    fs::create_dir_all(profiles.join("work")).expect("profile home");
    fs::set_permissions(&profiles, fs::Permissions::from_mode(0o000)).expect("unreadable");

    let outcome = plan(&installation.context, &release, &planner).await;
    // Restore before any assertion, so the temporary root can be removed.
    fs::set_permissions(&profiles, fs::Permissions::from_mode(0o700)).expect("restore");
    let host_plan = outcome.expect("plan the host");
    let failure = host_plan
        .preview
        .hermes_homes
        .iter()
        .find(|shown| shown.integration == "hermes profiles")
        .expect("the listing failure is previewed");
    assert_eq!(failure.status, "failed");
}

/// The Hermes plugin marker pohunek writes into a home it owns.
fn hermes_marker(home: &Path) -> PathBuf {
    home.join(HERMES_HOME)
        .join("plugins")
        .join("operators")
        .join("pohunek")
        .join(".pohunek-owned.json")
}

/// A planner report that lists `catalog` as its trust state and no packages.
fn catalog_report(catalog: &str) -> EnsureReport {
    serde_json::from_value(json!({
        "report_version": 1,
        "daemon_version": VERSION,
        "dry_run": true,
        "packages": [],
        "catalog": catalog,
        "profiles": [],
        "integrations": [],
        "refusals": [],
    }))
    .expect("a planner report of this layout")
}

/// A Hermes home previewed as an update whose marker is gone at apply time
/// fails the verdict and is named; it is not reported as an ordinary gap.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn a_marked_hermes_home_unmarked_after_the_preview_fails_the_verdict() {
    let installation = Installation::new();
    let release = release(&installation);
    let planner = ScriptedPlanner::new(vec![hermes_report()]);
    unmarked_hermes_home(&installation);
    let home = installation.root.join("home");
    fs::create_dir_all(hermes_marker(&home).parent().expect("plugin root")).expect("plugin root");
    fs::write(hermes_marker(&home), "{}").expect("marker");

    let host_plan = plan(&installation.context, &release, &planner)
        .await
        .expect("plan the host");
    let shown = host_plan
        .preview
        .hermes_homes
        .iter()
        .find(|shown| shown.integration == "hermes")
        .expect("the default home is previewed");
    assert_eq!(
        shown.status, "update",
        "the marked home is previewed as an update"
    );

    fs::remove_file(hermes_marker(&home)).expect("the marker goes");
    let error = apply(&installation.context, host_plan, &planner)
        .await
        .expect_err("the home that cannot be updated fails the verdict");
    assert_eq!(error.code(), "setup_incomplete");
    let message = error.to_string();
    assert!(
        message.contains("hermes: this home was marked in the preview"),
        "the failed update is named: {message}"
    );
}

/// A config file that cannot be written after an earlier one was created
/// stops setup, and the stop names the files already created.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn a_config_write_that_fails_part_way_names_the_files_already_created() {
    let installation = Installation::new();
    let release = release(&installation);
    let planner = ScriptedPlanner::new(vec![report(&json!([]))]);
    let prompts = config_dir(&installation).join("prompts");
    fs::create_dir_all(&prompts).expect("prompts directory");
    // The write of pr.tmpl goes through a temporary file of this process
    // beside it. A directory of that name makes the write fail after the
    // earlier files were created, and leaves no pr.tmpl behind.
    let blocked = prompts.join(format!(".pr.tmpl.{}.tmp", std::process::id()));
    fs::create_dir_all(&blocked).expect("blocking temporary directory");

    let host_plan = plan(&installation.context, &release, &planner)
        .await
        .expect("plan the host");
    let error = apply(&installation.context, host_plan, &planner)
        .await
        .expect_err("the failed config write stops setup");
    assert_eq!(error.code(), "setup_incomplete");
    let message = error.to_string();
    assert!(
        message.contains("config created:") && message.contains("attach.conf"),
        "the created file is named: {message}"
    );
    assert!(message.contains("issue.tmpl"), "{message}");
    assert!(!prompts.join("pr.tmpl").exists(), "no partial file is left");
    assert!(
        installed_at_this_version(&installation).await,
        "the service is kept"
    );
}

/// On an installed host whose catalog is to be recorded and that installs no
/// official package, the catalog is a note, not an effect: nothing records it,
/// and the verdict does not claim it.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn a_recorded_catalog_on_an_installed_host_is_a_note_not_an_effect() {
    let installation = Installation::new();
    let release = release(&installation);
    let planner = ScriptedPlanner::new(vec![
        catalog_report("unavailable"),
        catalog_report("unavailable"),
        catalog_report("record"),
        catalog_report("record"),
    ]);
    let first = plan(&installation.context, &release, &planner)
        .await
        .expect("plan the install");
    apply(&installation.context, first, &planner)
        .await
        .expect("install");

    let rerun = plan(&installation.context, &release, &planner)
        .await
        .expect("plan the re-run");
    assert_eq!(rerun.preview.service, ServiceStep::Present);
    let note = rerun
        .preview
        .catalog_note
        .clone()
        .expect("the note is previewed");
    assert!(note.contains("next official package install"), "{note}");
    let outcome = apply(&installation.context, rerun, &planner)
        .await
        .expect("the re-run applies");
    assert_eq!(outcome.verdict.catalog, "record");
    assert!(outcome.preview.catalog_note.is_some());
}

/// Runtime packages the fresh-host release carries. `pi` is installed only by
/// the third run, once its agent is on the search path.
const FRESH_HOST_RUNTIMES: [&str; 3] = ["claude", "codex", "pi"];

/// Program names of the official agents, which the search path of the
/// scenario offers only as its own fakes.
const OFFICIAL_AGENTS: [&str; 4] = ["claude", "codex", "hermes", "pi"];

/// System directories whose tools the search path of the scenario offers.
const SYSTEM_DIRS: [&str; 2] = ["/usr/bin", "/bin"];

/// Native ids the fake agents report when they start.
const CLAUDE_NATIVE_ID: &str = "d6-claude-native";
const CODEX_NATIVE_ID: &str = "d6-codex-native";

/// Body of the fake agents, see `support/setup_agent.sh`.
const FAKE_AGENT_BODY: &str = include_str!("support/setup_agent.sh");

/// Body of the fake `pi`, which only has to be on the search path.
const FAKE_PRESENCE_BODY: &str = "exit 0\n";

/// Shell whose copy is the fake agents' interpreter. macOS needs bash: its
/// `/bin/sh` re-execs `/bin/bash`, which would drop the agent's name.
#[cfg(target_os = "macos")]
const SHELL: &str = "/bin/bash";
#[cfg(not(target_os = "macos"))]
const SHELL: &str = "/bin/sh";

/// Mode of the directories that hold the fake agents and their config homes.
const PRIVATE_DIR: u32 = 0o700;

/// Mode of the directory that holds the fake agents on the search path.
const SEARCH_DIR: u32 = 0o755;

/// Mode of a fake agent and of its interpreter copy.
const EXECUTABLE: u32 = 0o755;
const PRIVATE_EXECUTABLE: u32 = 0o700;

/// Sequence number of the release catalog the scenario signs.
const CATALOG_SEQUENCE: u64 = 1;

/// Name of the release's package directory and archive suffix, as the release
/// daemon reads them.
const PACKAGES_DIR: &str = "packages";
const ARCHIVE_SUFFIX: &str = ".tar.zst";

/// The release directory's `runtime/` entry, which the release daemon reads
/// beside its own executable.
const RUNTIME_DIR: &str = "runtime";

/// Name of the signed catalog inside `runtime/`.
const CATALOG_FILE: &str = "runtime-catalog.json";

/// Name of the `SessionStart` hook script the integration installs into each
/// agent's config home, which the fake agents run.
const STATE_HOOK: &str = "pohunek-agent-state.sh";

/// The paths whose content differs between two states of the roots.
fn changed_paths(
    before: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    after: &BTreeMap<PathBuf, Option<Vec<u8>>>,
) -> Vec<PathBuf> {
    before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect()
}

/// Canonical spelling of an existing path, which the integration installer
/// requires for a config home (`integration_path_untrusted`,
/// `config_home_is_symlink`).
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("canonical path")
}

/// The path as the text the search path and the profiles carry.
fn path_text(path: &Path) -> String {
    path.to_str().expect("UTF-8 path").to_owned()
}

/// Creates a directory with `mode`.
fn directory(path: &Path, mode: u32) {
    fs::create_dir_all(path).expect("create directory");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set directory mode");
}

/// The `--version` line the compatibility lock of `runtime` pins. The
/// installed package's probe accepts that release.
fn supported_banner(runtime: &str) -> String {
    let path = workspace_root()
        .join("compat")
        .join(runtime)
        .join("compatibility-lock.json");
    let lock: Value =
        serde_json::from_slice(&fs::read(&path).expect("read the compatibility lock"))
            .expect("the compatibility lock is JSON");
    let release = lock["upstream"]["release"]
        .as_str()
        .expect("the lock pins a release");
    lock["upstream"]["version_output"]
        .as_str()
        .expect("the lock records the version output")
        .replace("{release}", release)
}

/// The package archive of `runtime-packages/<runtime>`, built the way
/// `pohunek plugin install` receives it, and the catalog entry that
/// authorizes it on this host.
fn official_archive(runtime: &str) -> (Vec<u8>, CatalogEntry) {
    let source = workspace_root().join("runtime-packages").join(runtime);
    let bytes = build_directory_archive(&source, &Limits::DEFAULT)
        .unwrap_or_else(|error| panic!("the {runtime} package builds: {error:?}"));
    let archive = read_archive(&bytes, &Limits::DEFAULT).expect("the archive reads");
    let digest: PackageDigest = archive.digest().clone();
    let definition = definition_from_archive(archive.entries(), &digest)
        .unwrap_or_else(|error| panic!("the {runtime} package parses: {error:?}"));
    let BindingProvenance::Package { package, .. } = definition.binding().provenance.clone() else {
        panic!("the {runtime} package has package provenance");
    };
    let entry = CatalogEntry {
        package_id: package.id,
        runtime_id: RuntimeId::parse(runtime).expect("runtime id"),
        version: package.version,
        digest,
        runtime_api: RUNTIME_API,
        platforms: vec![host_platform()],
        attestations: attestations_for(&[host_platform()]),
        core: ANY_CORE.to_owned(),
    };
    (bytes, entry)
}

/// Writes the release's `runtime/` directory: one archive per fresh-host
/// runtime under `packages/`, and a catalog of them signed by `key`.
fn write_release_runtime(runtime: &Path, key: &SigningKey) {
    let packages = runtime.join(PACKAGES_DIR);
    fs::create_dir_all(&packages).expect("packages directory");
    let mut entries = Vec::new();
    for name in FRESH_HOST_RUNTIMES {
        let (bytes, entry) = official_archive(name);
        fs::write(packages.join(format!("{name}{ARCHIVE_SUFFIX}")), bytes)
            .expect("write the package archive");
        entries.push(entry);
    }
    let catalog = catalog_of(CATALOG_SEQUENCE, WINDOW_END, entries);
    fs::write(runtime.join(CATALOG_FILE), signed(key, catalog)).expect("write the catalog");
}

/// Stages the release the scenario installs from as an extracted daemon
/// archive: this build's binaries, the throwaway trust anchor beside the
/// daemon and the signed `runtime/`, sealed with the archive's verifier and
/// `MANIFEST`, so setup verifies it before it runs anything from it.
fn stage_release(installation: &Installation, key: &SigningKey) -> PathBuf {
    let release = installation.stage();
    fs::write(release.join(ANCHOR_FILE_NAME), anchor_for(key)).expect("write the trust anchor");
    write_release_runtime(&release.join(RUNTIME_DIR), key);
    sealed_archive::seal(&release);
    release
}

/// Writes the fake `name` agent into `bin`. Its interpreter is a copy of the
/// shell named `fake-<name>-sh` in `interpreters`, it answers `--version`
/// with the release its package pins before anything else, and it exports
/// `env` before `body` runs.
fn write_fake_agent(
    bin: &Path,
    interpreters: &Path,
    name: &str,
    env: &[(&str, &str)],
    body: &str,
) -> PathBuf {
    let interpreter = interpreters.join(format!("fake-{name}-sh"));
    copy_shell_image(SHELL, &interpreter).expect("copy the shell as the agent interpreter");
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(PRIVATE_EXECUTABLE))
        .expect("chmod the agent interpreter");
    // A shebang line ends at the first blank, so the interpreter path must
    // hold none.
    let interpreter_text = path_text(&interpreter);
    assert!(
        !interpreter_text.contains(char::is_whitespace),
        "the interpreter path {interpreter_text:?} fits a shebang line"
    );
    let mut exports = String::new();
    for (key, value) in env {
        writeln!(exports, "export {key}={}", shell_quoted(value)).expect("format into a string");
    }
    let script = format!(
        "#!{interpreter_text}\nif [ \"${{1:-}}\" = --version ]; then printf '%s\\n' {}; exit 0; fi\n{exports}{body}",
        shell_quoted(&supported_banner(name)),
    );
    let path = bin.join(name);
    fs::write(&path, script).expect("write the fake agent");
    fs::set_permissions(&path, fs::Permissions::from_mode(EXECUTABLE)).expect("chmod the agent");
    path
}

/// `value` as one single-quoted shell word.
fn shell_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Links every program of the system directories into `tools`, except the
/// official agents, so the search path the job records offers the system's
/// tools without any agent installed on the machine running the test.
fn system_tools(tools: &Path) {
    directory(tools, SEARCH_DIR);
    for system in SYSTEM_DIRS {
        let Ok(listing) = fs::read_dir(system) else {
            continue;
        };
        for entry in listing {
            let entry = entry.expect("system directory entry");
            let name = entry.file_name();
            let excluded = name
                .to_str()
                .is_some_and(|name| OFFICIAL_AGENTS.contains(&name));
            let link = tools.join(&name);
            if !excluded && !link.exists() && link.symlink_metadata().is_err() {
                std::os::unix::fs::symlink(entry.path(), &link).expect("link a system tool");
            }
        }
    }
}

/// Every file and directory under `roots`, keyed by its absolute path.
fn state_of(roots: &[PathBuf]) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut state = BTreeMap::new();
    for root in roots {
        for (relative, content) in snapshot(root) {
            state.insert(root.join(relative), content);
        }
    }
    state
}

/// Starts a session of host profile `profile` in `cwd` and waits until its
/// runtime is live.
async fn start_agent(installation: &Installation, profile: &str, cwd: &Path) -> String {
    let session = installation
        .client()
        .await
        .call::<method::SessionNew>(SessionNewParams {
            agent: profile.to_owned(),
            name: Some(profile.to_owned()),
            cwd: Some(cwd.to_path_buf()),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            extended_input_ready_wait: None,
            metadata: BTreeMap::new(),
        })
        .await
        .expect("start the agent session")
        .session
        .id
        .0;
    installation.live(&session).await;
    session
}

/// Waits until the session reports `native` as its provider-native id. The
/// daemon learns it only from the `SessionStart` hook the agent fired.
async fn wait_native_reference(installation: &Installation, session: &str, native: &str) {
    native::eventually(&format!("{session} reports {native}"), || async {
        installation
            .client()
            .await
            .call::<method::SessionInspect>(SessionId(session.to_owned()))
            .await
            .ok()
            .filter(|info| info.native_session_id.as_deref() == Some(native))
    })
    .await;
}

/// Stops a session the scenario started, so no agent writes into the
/// installation while it is compared.
async fn stop_agent(installation: &Installation, session: &str) {
    let stopped = installation
        .client()
        .await
        .call::<method::SessionStop>(SessionId(session.to_owned()))
        .await
        .expect("stop the agent session");
    assert!(stopped.stopped, "{session} was live and stops");
}

/// A fresh host for the one-command scenario: an XDG home of its own, the
/// release to install from, and the search path its installed job records.
struct FreshHost {
    installation: Installation,
    home: PathBuf,
    bin: PathBuf,
    interpreters: PathBuf,
    log: PathBuf,
    release: PathBuf,
    planner: DaemonRuntimes,
}

impl FreshHost {
    /// A host with fake `claude` and `codex` on its search path and their
    /// config homes present, and no pohunek state at all.
    fn new() -> Self {
        let mut installation = Installation::new();
        let root = canonical(&installation.root);
        let home = root.join("home");
        directory(&home, PRIVATE_DIR);
        let bin = root.join("bin");
        directory(&bin, SEARCH_DIR);
        let tools = root.join("tools");
        system_tools(&tools);
        let interpreters = root.join("interpreters");
        directory(&interpreters, PRIVATE_DIR);
        let release = stage_release(&installation, &test_key());
        installation.context =
            installation
                .context
                .clone()
                .with_path_discovery(PathDiscovery::Managed {
                    login_shell: None,
                    shell_defaulted: false,
                    fallback_directories: vec![path_text(&bin), path_text(&tools)],
                });
        let host = Self {
            installation,
            log: root.join("agent.log"),
            home,
            bin,
            interpreters,
            release,
            planner: DaemonRuntimes::default(),
        };
        host.add_agent(
            "claude",
            &host.home.join(".claude").join("hooks"),
            CLAUDE_NATIVE_ID,
        );
        host.add_agent("codex", &host.home.join(".codex"), CODEX_NATIVE_ID);
        directory(&host.home.join(".claude"), PRIVATE_DIR);
        directory(&host.home.join(".codex"), PRIVATE_DIR);
        host
    }

    /// Puts the fake `name` on the search path. It fires the hook installed in
    /// `hooks` with `native` as its native id.
    fn add_agent(&self, name: &str, hooks: &Path, native: &str) {
        write_fake_agent(
            &self.bin,
            &self.interpreters,
            name,
            &[
                ("FAKE_AGENT_LOG", &path_text(&self.log)),
                ("FAKE_AGENT_HOOK_DIR", &path_text(hooks)),
                ("FAKE_AGENT_NATIVE_ID", native),
            ],
            FAKE_AGENT_BODY,
        );
    }

    /// The trees a run may change: the pohunek config, both agent homes and
    /// the package store.
    fn roots(&self) -> [PathBuf; 4] {
        [
            config_dir(&self.installation),
            self.home.join(".claude"),
            self.home.join(".codex"),
            self.installation.context.paths().plugins_dir(),
        ]
    }

    /// Plans and applies one run of setup with consent; the report carries
    /// the preview the run was planned with.
    async fn run(&self) -> SetupReport {
        let planned = plan(&self.installation.context, &self.release, &self.planner)
            .await
            .expect("plan the run");
        let outcome = apply(&self.installation.context, planned, &self.planner)
            .await
            .expect("the run applies");
        assert_eq!(outcome.verdict.health, "ok", "{:?}", outcome.verdict);
        assert!(
            outcome.verdict.hooks_failed.is_empty(),
            "{:?}",
            outcome.verdict
        );
        assert!(
            outcome.verdict.hook_gaps.is_empty(),
            "{:?}",
            outcome.verdict
        );
        outcome
    }
}

/// The runtimes a run planned to install.
fn installs(report: &SetupReport) -> Vec<&str> {
    report
        .preview
        .packages
        .iter()
        .filter(|package| package.status == PackageStatus::Install)
        .map(|package| package.runtime.as_str())
        .collect()
}

/// The first run installs the service and the packages of both agents on the
/// search path, carries the release's trust into the installed version and
/// installs both hooks, through which both agents then start.
async fn first_run_sets_up_claude_and_codex(host: &FreshHost) {
    let outcome = host.run().await;
    assert!(matches!(
        outcome.preview.service,
        ServiceStep::Install { .. }
    ));
    assert_eq!(
        installs(&outcome),
        ["claude", "codex"],
        "pi is not on the search path"
    );
    for line in [
        format!("service: installed version {VERSION}"),
        "package claude: installed".to_owned(),
        "package codex: installed".to_owned(),
    ] {
        assert!(
            outcome.applied.contains(&line),
            "{line}: {:?}",
            outcome.applied
        );
    }
    assert!(
        !outcome
            .applied
            .iter()
            .any(|line| line.starts_with("package pi")),
        "{:?}",
        outcome.applied
    );

    let status = service::status_in(&host.installation.context)
        .await
        .expect("status of the installation");
    assert!(status.installed && status.active_version.as_deref() == Some(VERSION));
    let version_dir = status
        .prefix
        .expect("the installation prefix")
        .join("libexec/pohunek")
        .join(VERSION);
    assert!(
        version_dir.join(ANCHOR_FILE_NAME).is_file(),
        "the anchor is installed"
    );
    assert!(
        version_dir.join(RUNTIME_DIR).join(CATALOG_FILE).is_file(),
        "the release catalog is installed with the version"
    );
    let claude_home = host.home.join(".claude");
    assert!(
        fs::read_to_string(claude_home.join("settings.json"))
            .expect("claude settings")
            .contains(STATE_HOOK),
        "claude's settings register the hook"
    );
    assert!(host.home.join(".codex").join(STATE_HOOK).is_file());

    let claude = start_agent(&host.installation, "claude", &host.home).await;
    let codex = start_agent(&host.installation, "codex", &host.home).await;
    wait_native_reference(&host.installation, &claude, CLAUDE_NATIVE_ID).await;
    wait_native_reference(&host.installation, &codex, CODEX_NATIVE_ID).await;
    let launches = fs::read_to_string(&host.log).expect("the agent log");
    for native in [CLAUDE_NATIVE_ID, CODEX_NATIVE_ID] {
        assert!(
            launches.contains(&format!("launch native={native}")),
            "{launches}"
        );
    }
    stop_agent(&host.installation, &claude).await;
    stop_agent(&host.installation, &codex).await;
}

/// A fresh host: one release, one run of the one command, and the agents on
/// the search path. The first run installs the service and the claude and
/// codex packages and hooks; both agents then start and fire their hook. A
/// second run changes nothing. A third run, after a fake `pi` appears on the
/// same search path, installs only the Pi package.
///
/// The host is an XDG home of its own with canonical paths and no pohunek
/// state. Claude's and codex's config homes exist before the first run, as an
/// installed agent creates them on its first start. The search path the
/// installed job records holds the fake agents and links to the system's
/// tools without any official agent, so no agent of the machine running the
/// test is detected. The Pi package declares no hook integration and no
/// config home, so the third run changes only the package store.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn a_fresh_host_is_set_up_by_one_run_and_a_rerun_changes_nothing() {
    let host = FreshHost::new();
    first_run_sets_up_claude_and_codex(&host).await;

    let before_rerun = state_of(&host.roots());
    let rerun = host.run().await;
    assert_eq!(rerun.preview.service, ServiceStep::Present);
    assert!(installs(&rerun).is_empty(), "{:?}", rerun.preview.packages);
    assert!(
        rerun.applied.is_empty(),
        "a rerun applies nothing: {:?}",
        rerun.applied
    );
    assert_eq!(
        state_of(&host.roots()),
        before_rerun,
        "a rerun changes no file"
    );

    write_fake_agent(&host.bin, &host.interpreters, "pi", &[], FAKE_PRESENCE_BODY);
    let before_pi = state_of(&host.roots());
    let outcome = host.run().await;
    assert_eq!(installs(&outcome), ["pi"], "{:?}", outcome.preview.packages);
    assert!(
        !outcome.applied.is_empty()
            && outcome
                .applied
                .iter()
                .all(|line| line.starts_with("package pi")),
        "only pi is installed: {:?}",
        outcome.applied
    );
    let store = host.installation.context.paths().plugins_dir();
    let changed = changed_paths(&before_pi, &state_of(&host.roots()));
    assert!(
        !changed.is_empty() && changed.iter().all(|path| path.starts_with(&store)),
        "only the package store changes: {changed:?}"
    );
}
