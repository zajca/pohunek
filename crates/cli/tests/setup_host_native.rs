//! Bare `pohunek setup` against the real native service manager.
//!
//! The setup runs in-process against an isolated installation, the same way
//! the `service_*` tests drive the engine. The service, the daemon, the RPCs,
//! the hooks and the config are real. Only the release planner is scripted:
//! the built daemon has no `--detect` until 035.3 slice 1 lands, and a
//! fixture daemon cannot stand in, because the supervisor refuses a main
//! process whose executable differs from the unit's `ExecStart`.
//!
//! Linux drives the systemd user manager and is ignored unless
//! `POHUNEK_SYSTEMD_E2E=1` opts in; macOS drives `gui/<uid>` in the ordinary
//! test run. See `support/native.rs` for isolation and binary lookup.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-10-14

#[path = "support/native.rs"]
mod native;

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

use native::Installation;
use pohunek_cli::host_setup::{apply, plan, ReleasePlanner, ServiceStep};
use pohunek_cli::service::{self, Context, VERSION};
use pohunek_service_config::runtimes::EnsureReport;
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

/// The Hermes integration a planner report lists.
fn hermes() -> Value {
    json!([{ "runtime": "hermes", "profile": null }])
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
    let planner = ScriptedPlanner::new(vec![report(&hermes())]);
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
    let planner = ScriptedPlanner::new(vec![report(&json!([])), report(&hermes())]);
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
    let planner = ScriptedPlanner::new(vec![report(&hermes())]);
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
    let planner = ScriptedPlanner::new(vec![report(&hermes())]);
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
    let planner = ScriptedPlanner::new(vec![report(&hermes())]);
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
