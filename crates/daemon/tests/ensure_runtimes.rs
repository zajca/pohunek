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
use package::PackageDigest;
use package::ANCHOR_FILE_NAME;
use pohunek_daemon::lock::InstanceLock;
use pohunek_paths::{METADATA_STORE_NAME as STORE_NAME, PLUGINS_SUBDIR};
use pohunek_service_config::runtimes::{
    CatalogStatus, EnsureReport, PackageStatus, ProfileStatus, UseReason, CODE_DAEMON_RUNNING,
    CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM, CODE_PROFILE_NOT_MIGRATABLE,
    CODE_RELEASE_CATALOG_UNAVAILABLE, DRY_RUN_FLAG, ENSURE_REPORT_VERSION, ENSURE_RUNTIMES_ARG,
    RELEASE_RUNTIME_DIR,
};
use pohunek_test_support::bin_exe;
use pohunek_test_support::env::TestEnv;
use protocol::{PackageErrorKind, ProtocolError};

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

    /// Trusts the release key and publishes `runtimes` as the release, with
    /// the catalog at `sequence`.
    fn publish_at(&self, runtimes: &[&str], sequence: u64) {
        fs::write(
            self.binary_dir().join(ANCHOR_FILE_NAME),
            self.catalog.anchor_bytes(),
        )
        .expect("write the trust anchor");
        self.catalog
            .write_release_at(&self.release_dir(), runtimes, sequence);
    }

    /// Trusts the release key and publishes the given archives as the release,
    /// under a catalog at `sequence`.
    fn publish_archives(&self, sequence: u64, archives: &[(&str, &[u8], &PackageDigest)]) {
        fs::write(
            self.binary_dir().join(ANCHOR_FILE_NAME),
            self.catalog.anchor_bytes(),
        )
        .expect("write the trust anchor");
        self.catalog
            .write_release_archives(&self.release_dir(), sequence, archives);
    }

    /// Removes the root of the installed package `digest`.
    fn remove_package_root(&self, digest: &str) {
        let hex = digest.strip_prefix("sha256:").expect("a sha256 digest");
        fs::remove_dir_all(self.plugins_dir().join("packages").join(hex))
            .expect("remove the package root");
    }

    /// Writes the host profile `name` with `text`.
    fn write_profile(&self, name: &str, text: &str) -> PathBuf {
        let path = self.profiles_dir().join(format!("{name}.toml"));
        fs::write(&path, text).expect("write the profile");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("secure the profile");
        path
    }

    /// Publishes `runtimes` as the release, with the first catalog sequence.
    fn publish(&self, runtimes: &[&str]) {
        self.publish_at(runtimes, FIRST_SEQUENCE);
    }

    /// Removes the release catalog and its trust anchor, as a release without
    /// a `runtime/` directory does.
    fn remove_release(&self) {
        fs::remove_file(self.binary_dir().join(ANCHOR_FILE_NAME)).expect("remove the anchor");
        fs::remove_dir_all(self.release_dir()).expect("remove the release directory");
    }

    /// Appends a line to the first file under the installed package root, so
    /// the root is present but does not match its recorded manifest.
    fn alter_installed_root(&self) {
        let packages = self.plugins_dir().join("packages");
        let mut pending = vec![fs::read_dir(&packages)
            .expect("the packages directory")
            .next()
            .expect("one installed package root")
            .expect("a package root entry")
            .path()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("a package directory") {
                let path = entry.expect("a directory entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let mut content = fs::read(&path).expect("a package file");
                    content.extend_from_slice(b"altered\n");
                    fs::write(&path, content).expect("alter the package file");
                    return;
                }
            }
        }
        panic!("the installed package root holds no file");
    }

    fn registry_path(&self) -> PathBuf {
        self.plugins_dir().join("registry.json")
    }

    fn catalog_state_path(&self) -> PathBuf {
        self.plugins_dir().join("catalog-state.json")
    }

    /// Removes the root of the one installed package, as a damaged install
    /// would lack it.
    fn remove_installed_root(&self) {
        let packages = self.plugins_dir().join("packages");
        let root = fs::read_dir(&packages)
            .expect("the packages directory")
            .next()
            .expect("one installed package root")
            .expect("a package root entry")
            .path();
        fs::remove_dir_all(root).expect("remove the package root");
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

/// Catalog sequence of the first release a test publishes.
const FIRST_SEQUENCE: u64 = 1;

/// A metadata store with one session of the Claude runtime pinned to an
/// installed package, as a release that ran the package recorded it.
const PACKAGE_SESSION_STORE: &str = include_str!("fixtures/ensure/claude-package-session.jsonl");

/// File name of the managed Claude state hook under `<config>/hooks`.
const CLAUDE_STATE_HOOK: &str = "pohunek-agent-state.sh";

impl Host {
    fn store_path(&self) -> PathBuf {
        self.env.data_home().join(APP_DIR).join(STORE_NAME)
    }

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

/// Publishes a newer release catalog, applies it, and checks that the
/// recorded trust state moves with it: a repeat finds it current.
#[test]
fn a_newer_release_catalog_is_recorded_and_a_repeat_finds_it_current() {
    let host = Host::new();
    host.publish_at(&["claude"], 1);
    host.write_claude_profile();
    let first = host.step(false);
    assert_eq!(first.catalog, CatalogStatus::Recorded);

    host.publish_at(&["claude"], 2);
    let plan = host.step(true);
    assert_eq!(plan.catalog, CatalogStatus::Record);
    assert_eq!(plan.packages[0].status, PackageStatus::Present);

    let applied = host.step(false);
    assert_eq!(applied.catalog, CatalogStatus::Recorded);
    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(applied.packages[0].status, PackageStatus::Present);

    let repeat = host.step(false);
    assert_eq!(repeat.catalog, CatalogStatus::Current);
    assert_eq!(repeat.packages[0].status, PackageStatus::Present);
    assert!(repeat.profiles.is_empty());
}

/// A release catalog older than the recorded watermark is refused by the
/// verifier, so the step keeps the installed package and records nothing.
#[test]
fn a_release_catalog_older_than_the_recorded_one_keeps_the_installed_package() {
    let host = Host::new();
    host.publish_at(&["claude"], 2);
    host.write_claude_profile();
    host.step(false);
    let recorded = fs::read(host.catalog_state_path()).expect("the recorded catalog state");

    host.publish_at(&["claude"], 1);
    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert_eq!(report.catalog, CatalogStatus::Unavailable);
        assert_eq!(report.packages.len(), 1);
        assert_eq!(report.packages[0].status, PackageStatus::Kept);
        assert!(report.profiles.is_empty());
    }
    assert_eq!(
        fs::read(host.catalog_state_path()).expect("the catalog state"),
        recorded,
        "an older catalog never rewrites the recorded trust state"
    );
}

/// A session launched from an installed official package keeps its runtime
/// planned, and its frozen digest is never rebound.
#[test]
fn a_session_launched_from_an_installed_package_plans_its_runtime() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.write_store(PACKAGE_SESSION_STORE);
    let store_before = fs::read(host.store_path()).expect("the metadata store");

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.packages.len(), 1);
    assert_eq!(plan.packages[0].runtime, "claude");
    assert_eq!(plan.packages[0].status, PackageStatus::Install);
    assert!(plan.packages[0].reasons.contains(&UseReason::Session));

    let applied = host.step(false);
    assert_eq!(applied.packages[0].status, PackageStatus::Installed);
    let repeat = host.step(false);
    assert_eq!(repeat.packages[0].status, PackageStatus::Present);
    assert_eq!(
        fs::read(host.store_path()).expect("the metadata store"),
        store_before,
        "the step never writes the metadata store"
    );
}

/// A selected release package whose root is gone is planned for install, the
/// archive restores the root, and a repeat finds it present.
#[test]
fn a_release_package_with_a_missing_root_is_restored_by_the_step() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.write_claude_profile();
    host.step(false);
    host.remove_installed_root();

    let plan = host.step(true);
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.packages[0].status, PackageStatus::Install);

    let applied = host.step(false);
    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(applied.packages[0].status, PackageStatus::Installed);

    let repeat = host.step(false);
    assert_eq!(repeat.packages[0].status, PackageStatus::Present);
}

/// Hooks that exist only in a host profile's config home are listed under
/// that profile, and the default home is not listed.
#[test]
fn hooks_only_in_a_profile_home_are_listed_under_that_profile() {
    let host = Host::new();
    host.publish(&["claude"]);
    let work_home = host.env.root().join("work-claude");
    let hooks = work_home.join("hooks");
    fs::create_dir_all(&hooks).expect("the profile hooks directory");
    fs::write(
        hooks.join(CLAUDE_STATE_HOOK),
        "#!/bin/sh\n# an earlier hook\n",
    )
    .expect("write the profile hook");
    let profiles = host.profiles_dir();
    let work = profiles.join("work.toml");
    fs::write(
        &work,
        format!(
            "base = \"claude\"\n\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
            work_home.display()
        ),
    )
    .expect("write the work profile");
    fs::set_permissions(&work, fs::Permissions::from_mode(0o600)).expect("secure the profile");

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    let integrations: Vec<(&str, Option<&str>)> = plan
        .integrations
        .iter()
        .map(|action| (action.runtime.as_str(), action.profile.as_deref()))
        .collect();
    assert_eq!(integrations, [("claude", Some("work"))]);
}

/// Without a usable release catalog an installed package keeps serving its
/// runtime for the pinned profile: the plan is a no-op and the registry is
/// unchanged.
#[test]
fn without_a_release_catalog_an_installed_package_keeps_serving_its_runtime() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.write_claude_profile();
    let applied = host.step(false);
    let digest = applied.packages[0].digest.clone();
    let registry = fs::read(host.registry_path()).expect("the registry record");
    host.remove_release();

    let plan = host.step(true);
    let repeat = host.step(false);

    for report in [&plan, &repeat] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert_eq!(report.catalog, CatalogStatus::Unavailable);
        assert_eq!(report.packages.len(), 1);
        assert_eq!(report.packages[0].status, PackageStatus::Kept);
        assert_eq!(report.packages[0].digest, digest);
        assert!(report.profiles.is_empty());
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry,
        "a kept package changes nothing"
    );
}

/// Without a release catalog an unpinned profile over a kept runtime binds to
/// the installed package, and the apply pins it.
#[test]
fn without_a_release_catalog_an_unpinned_profile_binds_to_the_installed_package() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.write_claude_profile();
    let applied = host.step(false);
    let digest = applied.packages[0].digest.clone();
    host.write_claude_profile();
    host.remove_release();

    let plan = host.step(true);
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.packages[0].status, PackageStatus::Kept);
    assert_eq!(plan.profiles.len(), 1);
    assert_eq!(plan.profiles[0].status, ProfileStatus::Bind);
    assert_eq!(plan.profiles[0].digest, digest);

    let bound = host.step(false);
    assert!(bound.refusals.is_empty(), "{:?}", bound.refusals);
    assert_eq!(bound.profiles.len(), 1);
    assert_eq!(bound.profiles[0].status, ProfileStatus::Bound);
    assert!(host
        .profile_text()
        .contains(&format!("digest = \"{digest}\"")));
}

/// A release package whose root is present but altered is refused by the plan,
/// before any change; the apply refuses it the same way and exits successfully.
#[test]
fn a_release_package_with_an_altered_root_is_refused_by_the_plan_and_changes_nothing() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.write_claude_profile();
    host.step(false);
    host.alter_installed_root();
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profile = host.profile_text();
    let root_refusal = ProtocolError::from(PackageErrorKind::RootInvalid).code;

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert_eq!(refusal_codes(report), [root_refusal.as_str()]);
        assert_eq!(report.refusals[0].subject, "claude");
        assert!(report.packages.is_empty());
        assert!(report.profiles.is_empty());
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
    assert_eq!(host.profile_text(), profile);
}

/// Without a usable release catalog, an unpinned profile whose bind would
/// conflict with a retained version of its package is refused by the plan: the
/// installed version that keeps serving has a retained older version whose root
/// is gone, so the bind's coexistence check fails. Nothing changes.
#[test]
fn an_unpinned_profile_is_refused_when_a_retained_older_version_is_damaged() {
    let host = Host::new();
    host.publish_at(&["claude"], 1);
    host.write_claude_profile();
    let older = host.step(false).packages[0].digest.clone();
    let (bytes, digest) = official_runtimes::variant_archive("claude", "1.0.2");
    host.publish_archives(2, &[("claude", &bytes, &digest)]);
    let newer = host.step(false);
    assert_eq!(newer.packages[0].status, PackageStatus::Installed);
    assert_eq!(newer.packages[0].digest, digest.as_str());
    host.remove_package_root(&older);
    host.remove_release();
    let second = host.write_profile("second", "base = \"claude\"\n");
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profile = fs::read(&second).expect("the unpinned profile");
    let incompatible = ProtocolError::from(PackageErrorKind::IntegrationIncompatible).code;

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert_eq!(refusal_codes(report), [incompatible.as_str()]);
        assert_eq!(report.refusals[0].subject, "second");
        assert!(report.profiles.is_empty());
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
    assert_eq!(fs::read(&second).expect("the unpinned profile"), profile);
}
