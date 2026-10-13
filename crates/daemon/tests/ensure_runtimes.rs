//! The offline runtime step `pohunekd ensure-official-runtimes`, run as the
//! installer runs it: the real daemon binary, copied beside a trust anchor and
//! a release `runtime/` directory, over a hermetic XDG environment.
//!
//! The release archives are the ones `runtime-packages` builds, and the
//! catalog is signed by a throwaway key that only the copied binary trusts.

// Rust guideline compliant 2026-10-14

#[path = "support/official_runtimes.rs"]
mod official_runtimes;

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;

use official_runtimes::OfficialCatalog;
use package::ANCHOR_FILE_NAME;
use pohunek_daemon::lock::InstanceLock;
use pohunek_paths::{METADATA_STORE_NAME as STORE_NAME, PLUGINS_SUBDIR};
use pohunek_service_config::runtimes::{
    EnsureReport, PackageStatus, ProfileStatus, UseReason, CODE_DAEMON_RUNNING,
    CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM, CODE_PROFILE_NOT_MIGRATABLE,
    CODE_RELEASE_CATALOG_UNAVAILABLE, DRY_RUN_FLAG, ENSURE_REPORT_VERSION, ENSURE_RUNTIMES_ARG,
    RELEASE_RUNTIME_DIR,
};
use pohunek_test_support::bin_exe;
use pohunek_test_support::env::TestEnv;

/// Name of the host agent profiles directory under the config directory.
const PROFILES_SUBDIR: &str = "agents";

/// Name of the daemon's directory under each XDG base directory.
const APP_DIR: &str = "pohunek";

/// Name of the daemon's single-instance lock file in its runtime directory.
const INSTANCE_LOCK_NAME: &str = "daemon.lock";

/// Profile over the official Claude runtime, the only profile these tests write.
const CLAUDE_PROFILE: &str = "official";

/// A hermetic host: the daemon binary beside its trust anchor and release, the
/// XDG directories of the environment, and the signing key of the release.
struct Host {
    env: TestEnv,
    catalog: OfficialCatalog,
    binary: PathBuf,
}

impl Host {
    fn new() -> Self {
        let env = TestEnv::new().expect("hermetic environment");
        let binary_dir = env.root().join("bin");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&binary_dir)
            .expect("the binary directory");
        let binary = binary_dir.join("pohunekd");
        fs::copy(bin_exe("pohunekd"), &binary).expect("copy the daemon beside its anchor");
        Self {
            env,
            catalog: OfficialCatalog::new(),
            binary,
        }
    }

    fn binary_dir(&self) -> &Path {
        self.binary.parent().expect("the binary has a directory")
    }

    fn release_dir(&self) -> PathBuf {
        self.binary_dir().join(RELEASE_RUNTIME_DIR)
    }

    /// Trusts the release key and publishes `runtimes` as the release.
    fn publish(&self, runtimes: &[&str]) {
        fs::write(
            self.binary_dir().join(ANCHOR_FILE_NAME),
            self.catalog.anchor_bytes(),
        )
        .expect("write the trust anchor");
        self.catalog.write_release(&self.release_dir(), runtimes);
    }

    fn plugins_dir(&self) -> PathBuf {
        self.env.state_home().join(APP_DIR).join(PLUGINS_SUBDIR)
    }

    fn profiles_dir(&self) -> PathBuf {
        let dir = self.env.config_home().join(APP_DIR).join(PROFILES_SUBDIR);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("the agents directory");
        dir
    }

    fn profile_path(&self) -> PathBuf {
        self.profiles_dir().join(format!("{CLAUDE_PROFILE}.toml"))
    }

    fn write_claude_profile(&self) -> String {
        let text = "base = \"claude\"\n".to_owned();
        fs::write(self.profile_path(), &text).expect("write the profile");
        fs::set_permissions(self.profile_path(), fs::Permissions::from_mode(0o600))
            .expect("secure the profile");
        text
    }

    fn profile_text(&self) -> String {
        fs::read_to_string(self.profile_path()).expect("read the profile")
    }

    /// Runs the step and returns its raw output.
    fn raw_step(&self, dry_run: bool) -> Output {
        let mut command = self.env.command(&self.binary);
        command.arg(ENSURE_RUNTIMES_ARG);
        if dry_run {
            command.arg(DRY_RUN_FLAG);
        }
        command.output().expect("run the daemon binary")
    }

    /// Runs the step, which must exit successfully, and parses its report.
    fn step(&self, dry_run: bool) -> EnsureReport {
        let output = self.raw_step(dry_run);
        assert!(
            output.status.success(),
            "the step exits 0, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("the step prints its report as JSON")
    }
}

/// The refusal codes of `report`, in order.
fn refusal_codes(report: &EnsureReport) -> Vec<&str> {
    report
        .refusals
        .iter()
        .map(|refusal| refusal.code.as_str())
        .collect()
}

#[test]
fn a_dry_run_plans_the_release_package_and_the_profile_pin_and_changes_nothing() {
    let host = Host::new();
    host.publish(&["claude"]);
    let profile = host.write_claude_profile();

    let plan = host.step(true);

    assert_eq!(plan.report_version, ENSURE_REPORT_VERSION);
    assert!(plan.dry_run);
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.packages.len(), 1);
    assert_eq!(plan.packages[0].runtime, "claude");
    assert_eq!(plan.packages[0].status, PackageStatus::Install);
    assert_eq!(plan.profiles.len(), 1);
    assert_eq!(plan.profiles[0].status, ProfileStatus::Bind);
    assert_eq!(host.profile_text(), profile, "a dry run leaves the profile");
    assert!(
        !host.plugins_dir().exists(),
        "a dry run creates no package store"
    );
}

#[test]
fn an_apply_installs_the_release_package_and_pins_the_profile_and_a_repeat_changes_nothing() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.write_claude_profile();

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(applied.packages.len(), 1);
    assert_eq!(applied.packages[0].status, PackageStatus::Installed);
    assert_eq!(applied.profiles.len(), 1);
    assert_eq!(applied.profiles[0].status, ProfileStatus::Bound);
    let pinned = host.profile_text();
    assert!(
        pinned.contains(&format!("digest = \"{}\"", applied.packages[0].digest)),
        "the profile is pinned to the installed digest: {pinned}"
    );

    let repeat = host.step(false);

    // A pinned profile still uses its runtime, so the repeat reports the
    // release package as present and changes nothing.
    assert!(repeat.refusals.is_empty(), "{:?}", repeat.refusals);
    assert_eq!(repeat.packages.len(), 1);
    assert_eq!(repeat.packages[0].status, PackageStatus::Present);
    assert!(repeat.profiles.is_empty(), "a pinned profile is left alone");
    assert_eq!(host.profile_text(), pinned);
}

#[test]
fn a_held_daemon_lock_refuses_the_apply_and_changes_nothing() {
    let host = Host::new();
    host.publish(&["claude"]);
    let profile = host.write_claude_profile();
    let runtime_app = host.env.runtime_dir().join(APP_DIR);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&runtime_app)
        .expect("the runtime directory");
    let _running = InstanceLock::acquire(&runtime_app.join(INSTANCE_LOCK_NAME))
        .expect("a daemon holds the instance lock");

    let report = host.step(false);

    assert_eq!(refusal_codes(&report), [CODE_DAEMON_RUNNING]);
    assert!(report.packages.is_empty());
    assert!(report.profiles.is_empty());
    assert_eq!(host.profile_text(), profile);
    assert!(
        !host.plugins_dir().exists(),
        "the refused apply installs nothing"
    );
}

#[test]
fn a_profile_over_an_official_runtime_without_a_release_catalog_refuses_and_changes_nothing() {
    let host = Host::new();
    let profile = host.write_claude_profile();

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert_eq!(refusal_codes(report), [CODE_RELEASE_CATALOG_UNAVAILABLE]);
        assert!(report.packages.is_empty());
        assert!(report.profiles.is_empty());
    }
    assert_eq!(host.profile_text(), profile);
    assert!(!host.plugins_dir().exists());
}

#[test]
fn a_release_catalog_without_the_runtime_for_this_platform_refuses_and_changes_nothing() {
    let host = Host::new();
    host.publish(&["codex"]);
    let profile = host.write_claude_profile();

    let report = host.step(false);

    assert_eq!(
        refusal_codes(&report),
        [CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM]
    );
    assert_eq!(report.refusals[0].subject, "claude");
    assert!(report.packages.is_empty());
    assert!(report.profiles.is_empty());
    assert_eq!(host.profile_text(), profile);
    assert!(!host.plugins_dir().exists());
}

/// A metadata store written by an earlier release (schema 7) whose only
/// session is a Claude conversation the daemon recorded.
const CLAUDE_SESSION_STORE: &str = include_str!("../src/store/fixtures/schema-7/metadata.jsonl");

/// File name of the managed Claude state hook under `<config>/hooks`.
const CLAUDE_STATE_HOOK: &str = "pohunek-agent-state.sh";

impl Host {
    /// The metadata store the daemon keeps sessions in.
    fn write_store(&self, content: &str) {
        let dir = self.env.data_home().join(APP_DIR);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("the data directory");
        let store = dir.join(STORE_NAME);
        fs::write(&store, content).expect("write the metadata store");
        fs::set_permissions(&store, fs::Permissions::from_mode(0o600))
            .expect("secure the metadata store");
    }

    /// A managed Claude state hook in the default Claude config home, as an
    /// earlier daemon installed it; its content is not the current asset, so
    /// the hook reads as present but outdated.
    fn write_claude_hook(&self) {
        let hooks = self.env.home().join(".claude").join("hooks");
        fs::create_dir_all(&hooks).expect("the Claude hooks directory");
        fs::write(
            hooks.join(CLAUDE_STATE_HOOK),
            "#!/bin/sh\n# an earlier hook\n",
        )
        .expect("write the Claude hook");
    }
}

#[test]
fn a_session_and_installed_claude_hooks_keep_the_runtime_planned_and_repeat_as_present() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.write_store(CLAUDE_SESSION_STORE);
    host.write_claude_hook();

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(
        plan.integrations
            .iter()
            .map(|action| action.runtime.as_str())
            .collect::<Vec<_>>(),
        ["claude"]
    );
    assert_eq!(plan.packages.len(), 1);
    assert_eq!(plan.packages[0].runtime, "claude");
    assert_eq!(plan.packages[0].status, PackageStatus::Install);
    assert!(plan.packages[0].reasons.contains(&UseReason::Session));
    assert!(plan.packages[0].reasons.contains(&UseReason::Integration));
    assert!(
        plan.packages
            .iter()
            .all(|package| package.runtime != "hermes")
            && plan
                .integrations
                .iter()
                .all(|action| action.runtime != "hermes"),
        "the CLI installs the Hermes hooks, so the step never lists Hermes"
    );

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(applied.packages.len(), 1);
    assert_eq!(applied.packages[0].status, PackageStatus::Installed);
    assert!(applied.profiles.is_empty());

    let repeat = host.step(false);

    assert!(repeat.refusals.is_empty(), "{:?}", repeat.refusals);
    assert_eq!(repeat.packages.len(), 1);
    assert_eq!(repeat.packages[0].status, PackageStatus::Present);
    assert!(repeat.profiles.is_empty());
}

#[test]
fn a_profile_that_cannot_be_read_refuses_the_step_and_is_not_skipped() {
    let host = Host::new();
    host.publish(&["claude"]);
    let claude = host.write_claude_profile();
    let broken = host.profiles_dir().join("broken.toml");
    fs::write(&broken, "base = \n").expect("write the broken profile");
    fs::set_permissions(&broken, fs::Permissions::from_mode(0o600)).expect("secure the profile");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert_eq!(refusal_codes(report), [CODE_PROFILE_NOT_MIGRATABLE]);
        assert_eq!(report.refusals[0].subject, "broken");
        // The planned entries stay as planned: nothing was applied.
        assert!(report
            .packages
            .iter()
            .all(|package| package.status == PackageStatus::Install));
        assert!(report
            .profiles
            .iter()
            .all(|profile| profile.status == ProfileStatus::Bind));
    }
    assert_eq!(
        host.profile_text(),
        claude,
        "the readable profile is untouched"
    );
    assert!(!host.plugins_dir().exists());
}

#[test]
fn a_profile_that_fails_the_load_checks_refuses_the_step_and_is_not_skipped() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.write_claude_profile();
    let unsafe_profile = host.profiles_dir().join("unsafe.toml");
    fs::write(&unsafe_profile, "base = \"claude\"\n").expect("write the profile");
    // Other-writable: the owner-security check of a profile load refuses it.
    fs::set_permissions(&unsafe_profile, fs::Permissions::from_mode(0o666))
        .expect("make the profile other-writable");

    let plan = host.step(true);

    assert_eq!(refusal_codes(&plan), [CODE_PROFILE_NOT_MIGRATABLE]);
    assert_eq!(plan.refusals[0].subject, "unsafe");
    assert!(!host.plugins_dir().exists());
}
