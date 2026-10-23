//! The offline runtime step `pohunekd ensure-official-runtimes`, run as the
//! installer runs it: the real daemon binary, copied beside a trust anchor and
//! a release `runtime/` directory, over a hermetic XDG environment.
//!
//! The release archives are the ones `runtime-packages` builds, and the
//! catalog is signed by a throwaway key that only the copied binary trusts.

// Rust guideline compliant 2026-10-14

#[path = "support/official_runtimes.rs"]
mod official_runtimes;
#[path = "support/service_file.rs"]
mod service_file;

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
    CatalogStatus, EnsureReport, PackageStatus, ProfileAction, ProfileStatus, SelectedPackage,
    UseReason, CODE_DAEMON_RUNNING, CODE_GAP_AGENT_NOT_ON_PATH, CODE_GAP_CONFIG_HOME_MISSING,
    CODE_GAP_CONFIG_HOME_SYMLINK, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING,
    CODE_GAP_SERVED_BY_OTHER_PACKAGE, CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST,
    CODE_GAP_UPDATE_VERSION_UNORDERED, CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM,
    CODE_PINNED_DIFFERS_FROM_KEPT_SELECTION, CODE_PINNED_TO_NEWER_VERSION,
    CODE_PINNED_TO_NON_OFFICIAL_PACKAGE, CODE_PINNED_TO_OTHER_PACKAGE, CODE_PINNED_VERSION_UNKNOWN,
    CODE_PROFILE_NOT_MIGRATABLE, CODE_RELEASE_CATALOG_UNAVAILABLE, CODE_SERVICE_CONFIG_UNREADABLE,
    DETECT_FLAG, DRY_RUN_FLAG, ENSURE_REPORT_VERSION, ENSURE_RUNTIMES_ARG, RELEASE_RUNTIME_DIR,
};
use pohunek_test_support::bin_exe;
use pohunek_test_support::env::TestEnv;
use protocol::{PackageErrorKind, ProtocolError};

/// Name of the host agent profiles directory under the config directory.
const PROFILES_SUBDIR: &str = "agents";

/// Name of the daemon's directory under each XDG base directory.
const APP_DIR: &str = "pohunek";

/// The allowlist the test service files record: the daemon's default.
fn default_allowlist() -> Vec<String> {
    pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
        .iter()
        .map(|pattern| (*pattern).to_owned())
        .collect()
}

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
        let host = Self::without_service();
        host.write_service(&default_allowlist());
        host
    }

    /// A host whose `service.toml` does not exist yet, as before `service install`.
    fn without_service() -> Self {
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

    /// The installed service file the step reads its launch environment from.
    fn service_path(&self) -> PathBuf {
        self.env
            .config_home()
            .join(APP_DIR)
            .join(pohunek_service_config::FILE_NAME)
    }

    /// Writes the installed service file with `allowlist`.
    fn write_service(&self, allowlist: &[String]) {
        let config = self.env.config_home().join(APP_DIR);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&config)
            .expect("the service directory");
        let state = self.env.state_home().join(APP_DIR);
        let runtime = self.env.runtime_dir().join(APP_DIR);
        service_file::write_service_file(
            &self.service_path(),
            self.env.root(),
            &state,
            &runtime,
            allowlist,
        );
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

    /// Disables the installed package `digest` through the registry, as the
    /// operator's `plugin disable` does.
    fn disable_package(&self, digest: &PackageDigest) {
        pohunek_daemon::agent::host::PackageStore::open(&self.plugins_dir())
            .expect("the package store")
            .registry()
            .set_enabled(digest, false)
            .expect("disable the package");
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
        let root = fs::read_dir(&packages)
            .expect("the packages directory")
            .next()
            .expect("one installed package root")
            .expect("a package root entry")
            .path();
        alter_root(root);
    }

    /// Alters the root of the installed package `digest` the same way.
    fn alter_package_root(&self, digest: &PackageDigest) {
        let hex = digest
            .as_str()
            .strip_prefix("sha256:")
            .expect("a sha256 digest");
        alter_root(self.plugins_dir().join("packages").join(hex));
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
        self.profile_text_of(CLAUDE_PROFILE)
    }

    /// The text of the host profile `name`.
    fn profile_text_of(&self, name: &str) -> String {
        let path = self.profiles_dir().join(format!("{name}.toml"));
        fs::read_to_string(path).expect("read the profile")
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

    /// Removes the Claude hooks directory `write_claude_hook` created.
    fn remove_claude_hook(&self) {
        fs::remove_dir_all(self.env.home().join(".claude").join("hooks"))
            .expect("remove the Claude hooks");
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
    let pinned_older = host.profile_text();
    let (bytes, digest) = official_runtimes::variant_archive("claude", "1.0.3");
    host.publish_archives(2, &[("claude", &bytes, &digest)]);
    let newer = host.step(false);
    assert_eq!(newer.packages[0].status, PackageStatus::Installed);
    assert_eq!(newer.packages[0].digest, digest.as_str());
    // The release repins the profile to the newer version; the older version is
    // retained again so the scenario refuses over it.
    host.write_profile(CLAUDE_PROFILE, &pinned_older);
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

/// The state a digest-only profile repair starts from: the older version is
/// installed, then replaced by a newer selected one, and nothing but the
/// digest-only profile `second` refers to the older version. Returns the older
/// digest, the newer digest, and the path of `second`.
fn digest_only_repair_state(host: &Host) -> (String, PackageDigest, PathBuf) {
    host.publish_at(&["claude"], 1);
    host.write_claude_profile();
    let older = host.step(false).packages[0].digest.clone();
    // Nothing references the older version once the profile is unpinned.
    host.write_profile("official", "base = \"claude\"\n");
    let (bytes, digest) = official_runtimes::variant_archive("claude", "1.0.3");
    host.publish_archives(2, &[("claude", &bytes, &digest)]);
    let newer = host.step(false);
    assert_eq!(newer.packages[0].status, PackageStatus::Installed);
    host.remove_package_root(&older);
    let second = host.write_profile(
        "second",
        &format!("base = \"claude\"\ndigest = \"{older}\"\n"),
    );
    (older, digest, second)
}

/// A profile that carries only an older `digest` (no `package` key) does not
/// reference that version for the bind that replaces it. With the older root
/// gone and no release catalog, the newer selected version is bound to it, and
/// the plan and the apply agree.
#[test]
fn a_digest_only_profile_is_rebound_when_its_replaced_older_version_is_damaged() {
    let host = Host::new();
    let (_older, digest, second) = digest_only_repair_state(&host);
    host.remove_release();

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.profiles.len(), 1);
    assert_eq!(plan.profiles[0].name, "second");
    assert_eq!(plan.profiles[0].status, ProfileStatus::Bind);
    assert_eq!(plan.profiles[0].digest, digest.as_str());

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(applied.profiles.len(), 1);
    assert_eq!(applied.profiles[0].status, ProfileStatus::Bound);
    let text = fs::read_to_string(&second).expect("the rebound profile");
    assert!(
        text.contains(&format!("digest = \"{}\"", digest.as_str())),
        "{text}"
    );
}

/// With the catalog still published, the newer release package is already
/// installed and selected. Its plan judges the profile's bind only: the
/// digest-only profile does not block the package it is bound to.
#[test]
fn a_digest_only_profile_does_not_block_an_installed_release_package() {
    let host = Host::new();
    let (_older, digest, _second) = digest_only_repair_state(&host);

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    let claude = plan
        .packages
        .iter()
        .find(|package| package.runtime == "claude")
        .expect("the claude package");
    assert_eq!(claude.status, PackageStatus::Present);
    assert_eq!(claude.digest, digest.as_str());
    assert_eq!(plan.profiles.len(), 1);
    assert_eq!(plan.profiles[0].status, ProfileStatus::Bind);
}

/// A release package that is selected but disabled is left disabled, with a
/// gap whose hint enables it: the operator turned it off, and a digest-only
/// profile that retains a damaged older version is not bound to it.
#[test]
fn a_selected_but_disabled_release_package_is_left_disabled_and_its_profile_unbound() {
    let host = Host::new();
    let (_older, digest, second) = digest_only_repair_state(&host);
    host.disable_package(&digest);
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profile = fs::read(&second).expect("the profile");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert!(report.packages.is_empty(), "{:?}", report.packages);
        assert!(report.profiles.is_empty(), "{:?}", report.profiles);
        let gap = report
            .gaps
            .iter()
            .find(|gap| gap.runtime == "claude")
            .expect("the claude gap");
        assert_eq!(gap.code, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING);
        assert!(
            gap.detail.contains(&enable_command("claude", &digest)),
            "{}",
            gap.detail
        );
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
    assert_eq!(fs::read(&second).expect("the profile"), profile);
}

/// The status of the profile `name` in `profiles`, which must have an entry.
fn profile_status(profiles: &[ProfileAction], name: &str) -> ProfileStatus {
    profiles
        .iter()
        .find(|profile| profile.name == name)
        .unwrap_or_else(|| panic!("the profile {name} has an entry"))
        .status
}

/// The state both missing-root scenarios start from: a healthy older version is
/// retained by the pinned profile `other`, and the newer selected version is
/// published again from its archive. Returns the older and the newer digest.
fn selected_root_missing_state(host: &Host) -> (String, PackageDigest) {
    host.publish_at(&["claude"], 1);
    host.write_profile("other", "base = \"claude\"\n");
    let older = host.step(false).packages[0].digest.clone();
    let pinned_older = host.profile_text_of("other");
    let (bytes, digest) = official_runtimes::variant_archive("claude", "1.1.1");
    host.publish_archives(2, &[("claude", &bytes, &digest)]);
    let newer = host.step(false);
    assert_eq!(newer.packages[0].status, PackageStatus::Installed);
    // The release repins `other` to the newer version; the older version is
    // retained again, as the scenarios need it.
    host.write_profile("other", &pinned_older);
    host.remove_package_root(digest.as_str());
    (older, digest)
}

/// The selected version's root is missing, and the older retained version is
/// healthy and compatible. The apply restores the root from the archive and then
/// binds the unpinned profile, so the plan plans that bind.
#[test]
fn an_unpinned_profile_binds_when_the_selected_root_is_missing_and_the_older_version_is_compatible()
{
    let host = Host::new();
    let (_older, digest) = selected_root_missing_state(&host);
    host.write_profile("target", "base = \"claude\"\n");

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    let claude = plan
        .packages
        .iter()
        .find(|package| package.runtime == "claude")
        .expect("the claude package");
    assert_eq!(claude.status, PackageStatus::Install);
    assert_eq!(
        profile_status(&plan.profiles, "target"),
        ProfileStatus::Bind
    );
    assert_eq!(
        profile_status(&plan.profiles, "other"),
        ProfileStatus::Repin
    );

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    let claude = applied
        .packages
        .iter()
        .find(|package| package.runtime == "claude")
        .expect("the claude package");
    assert_eq!(claude.status, PackageStatus::Installed);
    assert_eq!(
        profile_status(&applied.profiles, "target"),
        ProfileStatus::Bound
    );
    assert_eq!(
        profile_status(&applied.profiles, "other"),
        ProfileStatus::Repinned
    );
    let repeat = host.step(false);
    assert_eq!(repeat.packages[0].status, PackageStatus::Present);
    assert!(repeat.profiles.is_empty());
    assert_eq!(digest.as_str(), applied.packages[0].digest);
}

/// The selected version's root and the older retained version's root are both
/// missing. The older version stays unresolvable, so the bind refuses after the
/// restore; the plan refuses first, and nothing changes: the selected root stays
/// missing.
#[test]
fn an_unpinned_profile_is_refused_before_restore_when_both_roots_are_missing() {
    let host = Host::new();
    let (older, digest) = selected_root_missing_state(&host);
    host.remove_package_root(&older);
    let target = host.write_profile("target", "base = \"claude\"\n");
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profile = fs::read(&target).expect("the unpinned profile");
    let incompatible = ProtocolError::from(PackageErrorKind::IntegrationIncompatible).code;

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert_eq!(refusal_codes(report), [incompatible.as_str()]);
        assert_eq!(report.refusals[0].subject, "target");
        assert!(report
            .profiles
            .iter()
            .all(|profile| profile.name != "target"));
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
    assert_eq!(fs::read(&target).expect("the unpinned profile"), profile);
    assert!(
        !host
            .plugins_dir()
            .join("packages")
            .join(digest.as_str().strip_prefix("sha256:").expect("sha256"))
            .exists(),
        "the selected root is not restored"
    );
}

#[test]
fn a_missing_service_file_refuses_the_plan_and_changes_nothing() {
    let host = Host::new();
    host.publish(&["claude"]);
    let profile = host.write_claude_profile();
    fs::remove_file(host.service_path()).expect("remove the service file");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert_eq!(refusal_codes(report), [CODE_SERVICE_CONFIG_UNREADABLE]);
        assert!(report.packages.is_empty());
        assert!(report.profiles.is_empty());
    }
    assert_eq!(host.profile_text(), profile);
    assert!(!host.plugins_dir().exists());
}

/// The directory the detection step's `PATH` names, which holds the programs a
/// test makes available to the step.
fn programs_dir(host: &Host) -> PathBuf {
    host.env.root().join("programs")
}

impl Host {
    /// Makes an executable `name` available to the detection step.
    fn put_program(&self, name: &str) {
        let dir = programs_dir(self);
        fs::create_dir_all(&dir).expect("the programs directory");
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write the program");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("make the program executable");
    }

    /// Creates the config home `name` below the user's home, as an agent's
    /// first start does.
    fn make_home(&self, name: &str) -> PathBuf {
        let dir = self.env.home().join(name);
        fs::create_dir_all(&dir).expect("the config home");
        dir
    }

    /// Runs the detection plan with only the programs made available as its
    /// search path, and parses its report.
    fn detect_step(&self) -> EnsureReport {
        let output = self.raw_detect();
        assert!(
            output.status.success(),
            "the detection exits 0, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("the step prints its report as JSON")
    }

    fn raw_detect(&self) -> Output {
        self.raw_detect_with(&[])
    }

    /// Runs the detection plan with `extra` set on the child's environment only.
    fn raw_detect_with(&self, extra: &[(&str, &str)]) -> Output {
        let dir = programs_dir(self);
        fs::create_dir_all(&dir).expect("the programs directory");
        let mut command = self.env.command(&self.binary);
        command
            .arg(ENSURE_RUNTIMES_ARG)
            .arg(DRY_RUN_FLAG)
            .arg(DETECT_FLAG)
            .env("PATH", &dir);
        for (name, value) in extra {
            command.env(name, value);
        }
        command.output().expect("run the daemon binary")
    }
}

/// The gaps of `report` as (runtime, code) pairs, in order.
fn gap_codes(report: &EnsureReport) -> Vec<(&str, &str)> {
    report
        .gaps
        .iter()
        .map(|gap| (gap.runtime.as_str(), gap.code.as_str()))
        .collect()
}

#[test]
fn a_fresh_host_with_the_programs_on_the_search_path_detects_every_runtime_and_changes_nothing() {
    let host = Host::new();
    host.publish(&["claude", "codex", "pi"]);
    host.put_program("claude");
    host.put_program("codex");
    host.put_program("pi");
    let claude_home = host.make_home(".claude");
    let codex_home = host.make_home(".codex");

    let without_detect = host.step(true);
    let plan = host.detect_step();

    assert!(
        without_detect.packages.is_empty(),
        "without --detect nothing is planned"
    );
    assert!(without_detect.gaps.is_empty());
    assert_eq!(plan.report_version, ENSURE_REPORT_VERSION);
    assert!(plan.dry_run);
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert!(plan.gaps.is_empty(), "{:?}", plan.gaps);
    let planned: Vec<(&str, PackageStatus, &[UseReason])> = plan
        .packages
        .iter()
        .map(|package| {
            (
                package.runtime.as_str(),
                package.status,
                package.reasons.as_slice(),
            )
        })
        .collect();
    assert_eq!(
        planned,
        [
            ("claude", PackageStatus::Install, &[UseReason::Detected][..]),
            ("codex", PackageStatus::Install, &[UseReason::Detected][..]),
            ("pi", PackageStatus::Install, &[UseReason::Detected][..]),
        ]
    );
    let homes: Vec<(&str, Option<&str>)> = plan
        .integrations
        .iter()
        .map(|action| (action.runtime.as_str(), action.profile.as_deref()))
        .collect();
    assert_eq!(
        homes,
        [("claude", None), ("codex", None)],
        "Pi declares no hooks"
    );
    assert!(claude_home.is_dir() && codex_home.is_dir());
    assert!(
        !host.plugins_dir().exists(),
        "a detection creates no package store"
    );
}

#[test]
fn a_runtime_already_used_keeps_its_reasons_and_gains_detected() {
    let host = Host::new();
    host.publish(&["claude", "codex"]);
    host.put_program("claude");
    host.make_home(".claude");
    let profile = host.write_claude_profile();

    let plan = host.detect_step();

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    let claude = plan
        .packages
        .iter()
        .find(|package| package.runtime == "claude")
        .expect("the profile's runtime is planned");
    assert_eq!(claude.reasons, [UseReason::Profile, UseReason::Detected]);
    assert_eq!(
        plan.profiles.len(),
        1,
        "the profile is planned to be pinned"
    );
    assert_eq!(
        host.profile_text(),
        profile,
        "a detection leaves the profile"
    );
}

#[test]
fn a_program_missing_from_the_search_path_is_a_gap_and_its_runtime_is_not_planned() {
    let host = Host::new();
    host.publish(&["claude", "codex", "pi"]);
    host.put_program("claude");
    host.put_program("pi");
    host.make_home(".claude");

    let plan = host.detect_step();

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(gap_codes(&plan), [("codex", CODE_GAP_AGENT_NOT_ON_PATH)]);
    let planned: Vec<&str> = plan
        .packages
        .iter()
        .map(|package| package.runtime.as_str())
        .collect();
    assert_eq!(planned, ["claude", "pi"]);
    assert!(!host.plugins_dir().exists());
}

#[test]
fn a_missing_config_home_is_a_gap_and_the_package_is_still_planned_without_creating_the_home() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.put_program("claude");

    let plan = host.detect_step();

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(gap_codes(&plan), [("claude", CODE_GAP_CONFIG_HOME_MISSING)]);
    assert_eq!(plan.packages.len(), 1);
    assert_eq!(plan.packages[0].status, PackageStatus::Install);
    assert!(
        plan.integrations.is_empty(),
        "no hooks are planned into a missing home"
    );
    assert!(
        !host.env.home().join(".claude").exists(),
        "the step never creates a config home"
    );
    assert!(!host.plugins_dir().exists());
}

#[test]
fn a_profile_home_that_does_not_exist_is_a_gap_and_its_hooks_are_not_listed() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.put_program("claude");
    host.make_home(".claude");
    let work_home = host.env.root().join("work-claude");
    let work = host.profiles_dir().join("work.toml");
    fs::write(
        &work,
        format!(
            "base = \"claude\"\n\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
            work_home.display()
        ),
    )
    .expect("write the work profile");
    fs::set_permissions(&work, fs::Permissions::from_mode(0o600)).expect("secure the profile");
    // A relative config directory is not one the agent can be launched with.
    let relative = host.profiles_dir().join("relative.toml");
    fs::write(
        &relative,
        "base = \"claude\"\n\n[env]\nCLAUDE_CONFIG_DIR = \"relative-claude\"\n",
    )
    .expect("write the relative profile");
    fs::set_permissions(&relative, fs::Permissions::from_mode(0o600)).expect("secure the profile");

    let plan = host.detect_step();

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(
        gap_codes(&plan),
        [
            ("claude", CODE_GAP_CONFIG_HOME_MISSING),
            ("claude", CODE_GAP_CONFIG_HOME_MISSING),
        ]
    );
    let relative_gap = plan
        .gaps
        .iter()
        .find(|gap| gap.detail.contains("`relative`"))
        .expect("the relative profile is a gap");
    assert!(
        relative_gap.detail.contains("cannot be resolved"),
        "{}",
        relative_gap.detail
    );
    let work_gap = plan
        .gaps
        .iter()
        .find(|gap| gap.detail.contains("`work`"))
        .expect("the work profile is a gap");
    assert!(
        work_gap.detail.contains("does not exist"),
        "{}",
        work_gap.detail
    );
    let integrations: Vec<(&str, Option<&str>)> = plan
        .integrations
        .iter()
        .map(|action| (action.runtime.as_str(), action.profile.as_deref()))
        .collect();
    assert_eq!(integrations, [("claude", None)]);
    assert!(!work_home.exists(), "the step never creates a profile home");
}

#[test]
fn a_symlinked_default_home_is_a_gap_and_a_profile_home_it_points_at_is_still_planned() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.put_program("claude");
    let real = host.env.root().join("real-claude");
    fs::create_dir_all(&real).expect("the real config home");
    let default_home = host.env.home().join(".claude");
    fs::create_dir_all(host.env.home()).expect("the user's home");
    std::os::unix::fs::symlink(&real, &default_home).expect("symlink the default home");
    let work = host.profiles_dir().join("work.toml");
    fs::write(
        &work,
        format!(
            "base = \"claude\"\n\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
            real.display()
        ),
    )
    .expect("write the work profile");
    fs::set_permissions(&work, fs::Permissions::from_mode(0o600)).expect("secure the profile");

    let plan = host.detect_step();

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(gap_codes(&plan), [("claude", CODE_GAP_CONFIG_HOME_SYMLINK)]);
    let integrations: Vec<(&str, Option<&str>)> = plan
        .integrations
        .iter()
        .map(|action| (action.runtime.as_str(), action.profile.as_deref()))
        .collect();
    assert_eq!(integrations, [("claude", Some("work"))]);
    assert!(
        default_home
            .symlink_metadata()
            .expect("the default home")
            .file_type()
            .is_symlink(),
        "the step leaves the symlink as it is"
    );
    assert!(
        fs::read_dir(&real).expect("the real home").next().is_none(),
        "the step writes nothing into the real home"
    );
    assert!(!host.plugins_dir().exists());
}

#[test]
fn a_profile_pinned_to_an_unavailable_package_is_a_gap_and_does_not_shadow_a_valid_profile_home() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.put_program("claude");
    host.make_home(".claude");
    let shared = host.env.root().join("shared-claude");
    fs::create_dir_all(&shared).expect("the shared config home");
    // Sorts before the valid profile, so a dedup that ran first would keep it.
    let broken = host.profiles_dir().join("a-broken.toml");
    fs::write(
        &broken,
        format!(
            "base = \"claude\"\npackage = \"pohunek.runtime.claude\"\ndigest = \"sha256:{}\"\n\n\
             [env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
            "0".repeat(64),
            shared.display()
        ),
    )
    .expect("write the pinned profile");
    fs::set_permissions(&broken, fs::Permissions::from_mode(0o600)).expect("secure the profile");
    let valid = host.profiles_dir().join("b-valid.toml");
    fs::write(
        &valid,
        format!(
            "base = \"claude\"\n\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
            shared.display()
        ),
    )
    .expect("write the valid profile");
    fs::set_permissions(&valid, fs::Permissions::from_mode(0o600)).expect("secure the profile");

    let plan = host.detect_step();

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.gaps.len(), 1, "{:?}", plan.gaps);
    assert_eq!(plan.gaps[0].runtime, "claude");
    assert!(
        plan.gaps[0].detail.contains("`a-broken`"),
        "{}",
        plan.gaps[0].detail
    );
    let integrations: Vec<(&str, Option<&str>)> = plan
        .integrations
        .iter()
        .map(|action| (action.runtime.as_str(), action.profile.as_deref()))
        .collect();
    assert_eq!(
        integrations,
        [("claude", None), ("claude", Some("b-valid"))]
    );
}

#[test]
fn a_default_home_spelled_with_dot_dot_is_a_gap_and_does_not_shadow_a_profile_home() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.put_program("claude");
    // The daemon reads the relocation variable from its launch environment,
    // which the service file's allowlist names.
    let mut allowlist = default_allowlist();
    allowlist.push("CLAUDE_CONFIG_DIR".to_owned());
    host.write_service(&allowlist);
    let real = host.env.root().join("real-claude");
    fs::create_dir_all(&real).expect("the real config home");
    let spelled = real.join("..").join("real-claude");
    let work = host.profiles_dir().join("work.toml");
    fs::write(
        &work,
        format!(
            "base = \"claude\"\n\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
            real.display()
        ),
    )
    .expect("write the work profile");
    fs::set_permissions(&work, fs::Permissions::from_mode(0o600)).expect("secure the profile");

    let output = host.raw_detect_with(&[("CLAUDE_CONFIG_DIR", spelled.to_str().expect("utf-8"))]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: EnsureReport =
        serde_json::from_slice(&output.stdout).expect("the step prints its report as JSON");

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(gap_codes(&plan), [("claude", "integration_path_untrusted")]);
    let integrations: Vec<(&str, Option<&str>)> = plan
        .integrations
        .iter()
        .map(|action| (action.runtime.as_str(), action.profile.as_deref()))
        .collect();
    assert_eq!(integrations, [("claude", Some("work"))]);
    assert!(
        fs::read_dir(&real).expect("the real home").next().is_none(),
        "the step writes nothing into the real home"
    );
    assert!(!host.plugins_dir().exists());
}

/// A default home spelled `<real home><suffix>`, which the installer refuses,
/// with a profile whose home is the real directory: the refused default is a
/// gap, and the profile's real home is still planned.
fn assert_refused_default_spelling_does_not_shadow_profile(suffix: &str) {
    let host = Host::new();
    host.publish(&["claude"]);
    host.put_program("claude");
    let mut allowlist = default_allowlist();
    allowlist.push("CLAUDE_CONFIG_DIR".to_owned());
    host.write_service(&allowlist);
    let real = host.env.root().join("real-claude");
    fs::create_dir_all(&real).expect("the real config home");
    let work = host.profiles_dir().join("work.toml");
    fs::write(
        &work,
        format!(
            "base = \"claude\"\n\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
            real.display()
        ),
    )
    .expect("write the work profile");
    fs::set_permissions(&work, fs::Permissions::from_mode(0o600)).expect("secure the profile");
    let spelled = format!("{}{suffix}", real.display());

    let output = host.raw_detect_with(&[("CLAUDE_CONFIG_DIR", &spelled)]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: EnsureReport =
        serde_json::from_slice(&output.stdout).expect("the step prints its report as JSON");

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(gap_codes(&plan), [("claude", "integration_path_untrusted")]);
    let integrations: Vec<(&str, Option<&str>)> = plan
        .integrations
        .iter()
        .map(|action| (action.runtime.as_str(), action.profile.as_deref()))
        .collect();
    assert_eq!(
        integrations,
        [("claude", Some("work"))],
        "spelling {suffix:?}"
    );
    assert!(
        fs::read_dir(&real).expect("the real home").next().is_none(),
        "the step writes nothing into the real home"
    );
    assert!(!host.plugins_dir().exists());
}

#[test]
fn a_default_home_with_a_trailing_slash_is_a_gap_and_does_not_shadow_a_profile_home() {
    assert_refused_default_spelling_does_not_shadow_profile("/");
}

#[test]
fn a_default_home_with_a_double_slash_is_a_gap_and_does_not_shadow_a_profile_home() {
    assert_refused_default_spelling_does_not_shadow_profile("//");
}

#[test]
fn a_fresh_host_without_a_service_file_is_planned_under_detect_and_creates_nothing() {
    let host = Host::without_service();
    host.publish(&["claude"]);
    host.put_program("claude");
    host.make_home(".claude");

    let plan = host.detect_step();

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert!(plan.gaps.is_empty(), "{:?}", plan.gaps);
    assert_eq!(plan.packages.len(), 1);
    assert_eq!(plan.packages[0].runtime, "claude");
    assert_eq!(plan.packages[0].reasons, [UseReason::Detected]);
    assert!(
        !host.service_path().exists(),
        "the step installs no service file"
    );
    assert!(!host.plugins_dir().exists());
    assert!(!host.store_path().exists());
}

#[test]
fn a_fresh_host_without_a_service_file_still_refuses_a_plain_dry_run() {
    let host = Host::without_service();
    host.publish(&["claude"]);
    host.put_program("claude");

    let report = host.step(true);

    assert_eq!(refusal_codes(&report), [CODE_SERVICE_CONFIG_UNREADABLE]);
    assert!(report.packages.is_empty());
    assert!(!host.plugins_dir().exists());
}

#[test]
fn a_malformed_service_file_still_refuses_under_detect() {
    let host = Host::new();
    host.publish(&["claude"]);
    host.put_program("claude");
    fs::write(host.service_path(), "this is not a service file = [\n")
        .expect("corrupt the service file");

    let plan = host.detect_step();

    assert_eq!(refusal_codes(&plan), [CODE_SERVICE_CONFIG_UNREADABLE]);
    assert!(plan.packages.is_empty());
    assert!(!host.plugins_dir().exists());
}

#[tokio::test]
async fn a_runtime_served_by_another_package_is_reported_and_its_installed_state_is_untouched() {
    let host = Host::new();
    host.publish(&["pi"]);
    host.put_program("pi");
    let foreign = official_runtimes::preinstall_third_party(
        &host.plugins_dir(),
        host.env.root(),
        "pi",
        "acme.runtime.pi",
    )
    .await;
    let registry_before =
        fs::read(host.registry_path()).expect("the registry before the detection");

    let plan = host.detect_step();

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(gap_codes(&plan), [("pi", CODE_GAP_SERVED_BY_OTHER_PACKAGE)]);
    assert!(
        plan.packages.is_empty(),
        "the other package is not replaced"
    );
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry after the detection"),
        registry_before,
        "a detection writes nothing"
    );
    assert!(
        host.plugins_dir()
            .join("packages")
            .join(foreign.as_str().trim_start_matches("sha256:"))
            .is_dir(),
        "the other package keeps its root"
    );
}

#[test]
fn a_platform_without_a_package_is_a_gap_with_detect_and_a_refusal_without() {
    let host = Host::new();
    host.publish(&["codex"]);
    host.write_claude_profile();

    let refused = host.step(true);
    let plan = host.detect_step();

    assert_eq!(
        refusal_codes(&refused),
        [CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM]
    );
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(
        gap_codes(&plan),
        [
            ("claude", CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM),
            ("codex", CODE_GAP_AGENT_NOT_ON_PATH),
        ]
    );
    assert!(plan.packages.is_empty());
    assert!(!host.plugins_dir().exists());
}

#[test]
fn detect_without_dry_run_is_a_usage_error_and_changes_nothing() {
    let host = Host::new();
    host.publish(&["claude"]);
    let profile = host.write_claude_profile();

    let output = host
        .env
        .command(&host.binary)
        .arg(ENSURE_RUNTIMES_ARG)
        .arg(DETECT_FLAG)
        .output()
        .expect("run the daemon binary");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(DETECT_FLAG) && stderr.contains(DRY_RUN_FLAG),
        "{stderr}"
    );
    assert!(output.stdout.is_empty());
    assert_eq!(host.profile_text(), profile);
    assert!(!host.plugins_dir().exists());
}

/// The older and the newer version the repin scenarios publish as the release
/// package of `claude`.
const OLDER_VERSION: &str = "1.0.0";
const NEWER_VERSION: &str = "1.0.1";

/// Publishes the `claude` package at `version` as the release, under a catalog
/// at `sequence`, and returns its digest.
fn publish_claude_version(host: &Host, version: &str, sequence: u64) -> PackageDigest {
    let (bytes, digest) = official_runtimes::variant_archive("claude", version);
    host.publish_archives(sequence, &[("claude", &bytes, &digest)]);
    digest
}

/// A profile pinned to an older official version of the release package is
/// moved to the release package: the plan reports the repin, the apply reports
/// it repinned, and the profile file names the release package from then on.
#[test]
fn a_profile_pinned_to_an_older_official_version_is_repinned_to_the_release_package() {
    let host = Host::new();
    let older = publish_claude_version(&host, OLDER_VERSION, 1);
    host.write_claude_profile();
    let bound = host.step(false);
    assert_eq!(bound.profiles.len(), 1);
    assert_eq!(bound.profiles[0].status, ProfileStatus::Bound);
    assert_eq!(bound.packages[0].digest, older.as_str());
    let newer = publish_claude_version(&host, NEWER_VERSION, 2);

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.packages[0].status, PackageStatus::Install);
    assert_eq!(plan.profiles.len(), 1);
    let repin = &plan.profiles[0];
    assert_eq!(repin.name, CLAUDE_PROFILE);
    assert_eq!(repin.status, ProfileStatus::Repin);
    assert_eq!(repin.digest, newer.as_str());
    assert_eq!(repin.previous_digest.as_deref(), Some(older.as_str()));
    assert_eq!(repin.previous_version.as_deref(), Some(OLDER_VERSION));
    assert_eq!(repin.version.as_deref(), Some(NEWER_VERSION));
    assert!(repin.kept_reason.is_none());
    assert!(host.profile_text().contains(older.as_str()));

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(applied.packages[0].status, PackageStatus::Installed);
    assert_eq!(applied.profiles.len(), 1);
    assert_eq!(applied.profiles[0].status, ProfileStatus::Repinned);
    assert_eq!(applied.profiles[0].digest, newer.as_str());
    let text = host.profile_text();
    assert!(text.contains(newer.as_str()), "{text}");
    assert!(!text.contains(older.as_str()), "{text}");

    let repeat = host.step(false);

    assert!(
        repeat.profiles.is_empty(),
        "a repinned profile has no entry"
    );
    assert_eq!(repeat.packages[0].status, PackageStatus::Present);
}

/// A profile whose `package` key names another package than the one its
/// digest records is an anomalous pin: the step keeps it, even though the digest
/// is an older version of the release package.
#[test]
fn a_profile_whose_package_key_disagrees_with_its_digest_is_kept() {
    let host = Host::new();
    let older = publish_claude_version(&host, OLDER_VERSION, 1);
    host.write_claude_profile();
    let bound = host.step(false);
    assert_eq!(bound.profiles[0].status, ProfileStatus::Bound);
    let text = host.profile_text();
    assert!(text.contains("pohunek.runtime.claude"), "{text}");
    host.write_profile(
        CLAUDE_PROFILE,
        &text.replacen("pohunek.runtime.claude", "acme.runtime.other", 1),
    );
    let profile = fs::read(host.profile_path()).expect("the kept profile");
    publish_claude_version(&host, NEWER_VERSION, 2);

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.profiles.len(), 1, "{:?}", plan.profiles);
    assert_kept(
        &plan.profiles[0],
        older.as_str(),
        CODE_PINNED_TO_OTHER_PACKAGE,
    );
    let applied = host.step(false);
    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(
        fs::read(host.profile_path()).expect("the kept profile"),
        profile
    );
}

/// Asserts that `profile` keeps its pin at `digest` for `reason`, so nothing
/// rewrites it.
fn assert_kept(profile: &ProfileAction, digest: &str, reason: &str) {
    assert_eq!(profile.status, ProfileStatus::Kept, "{profile:?}");
    assert_eq!(profile.name, CLAUDE_PROFILE);
    assert_eq!(profile.digest, digest);
    assert_eq!(profile.kept_reason.as_deref(), Some(reason));
    assert_eq!(profile.version, None);
}

/// A profile pinned to a newer official version than the release package is
/// kept at its pin with its reason. The release package is still installed,
/// and the newer package the registry selected is named as the one it replaces.
#[test]
fn a_profile_pinned_to_a_newer_official_version_is_kept_and_the_replaced_selection_is_named() {
    let host = Host::new();
    let newer = publish_claude_version(&host, NEWER_VERSION, 1);
    host.write_claude_profile();
    let bound = host.step(false);
    assert_eq!(bound.profiles[0].status, ProfileStatus::Bound);
    let older = publish_claude_version(&host, OLDER_VERSION, 2);
    let profile = fs::read(host.profile_path()).expect("the kept profile");
    let replaced = SelectedPackage {
        digest: newer.as_str().to_owned(),
        version: NEWER_VERSION.to_owned(),
    };

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.packages[0].status, PackageStatus::Install);
    assert_eq!(plan.packages[0].digest, older.as_str());
    assert_eq!(plan.packages[0].replaces_newer, Some(replaced.clone()));
    assert_eq!(plan.profiles.len(), 1);
    assert_kept(
        &plan.profiles[0],
        newer.as_str(),
        CODE_PINNED_TO_NEWER_VERSION,
    );
    assert_eq!(
        plan.profiles[0].previous_version.as_deref(),
        Some(NEWER_VERSION)
    );

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(applied.packages[0].status, PackageStatus::Installed);
    assert_eq!(applied.packages[0].digest, older.as_str());
    assert_eq!(applied.packages[0].replaces_newer, Some(replaced));
    assert_eq!(applied.profiles.len(), 1);
    assert_kept(
        &applied.profiles[0],
        newer.as_str(),
        CODE_PINNED_TO_NEWER_VERSION,
    );
    assert_eq!(
        fs::read(host.profile_path()).expect("the kept profile"),
        profile
    );
}

/// The selected version of a reserved runtime that has no order against the
/// release's is kept, reported as a gap and never replaced, as for Pi.
#[test]
fn a_selected_reserved_runtime_version_without_an_order_against_the_release_is_kept() {
    for runtime in ["claude", "codex", "hermes"] {
        let host = Host::new();
        let (bytes, digest) = official_runtimes::variant_archive(runtime, "2.0");
        host.publish_archives(1, &[(runtime, &bytes, &digest)]);
        host.write_profile(runtime, &format!("base = \"{runtime}\"\n"));
        let installed = host.step(false);
        assert!(installed.refusals.is_empty(), "{runtime}: {installed:?}");
        let (release, release_digest) = official_runtimes::variant_archive(runtime, "1.0.3");
        host.publish_archives(2, &[(runtime, &release, &release_digest)]);
        let registry = fs::read(host.registry_path()).expect("the registry record");

        let plan = host.step(true);
        let applied = host.step(false);

        for report in [&plan, &applied] {
            assert!(
                report.refusals.is_empty(),
                "{runtime}: {:?}",
                report.refusals
            );
            let package = report
                .packages
                .iter()
                .find(|package| package.runtime == runtime)
                .expect("the runtime package");
            assert_eq!(package.status, PackageStatus::Kept, "{runtime}");
            assert_eq!(package.version, "2.0", "{runtime}");
            assert_eq!(package.digest, digest.as_str(), "{runtime}");
            assert_eq!(package.release_version.as_deref(), Some("1.0.3"));
            assert_eq!(package.replaces_newer, None, "{runtime}");
            let unordered: Vec<_> = gap_codes(report)
                .into_iter()
                .filter(|(_, code)| *code == CODE_GAP_UPDATE_VERSION_UNORDERED)
                .collect();
            assert_eq!(unordered, [(runtime, CODE_GAP_UPDATE_VERSION_UNORDERED)]);
        }
        assert_eq!(
            fs::read(host.registry_path()).expect("the registry record"),
            registry,
            "{runtime}"
        );
    }
}

/// The version of the release the disabled-selection scenarios publish.
const RELEASE_VERSION: &str = "1.0.3";

/// The enable command a gap hint names for the package `runtime` serves.
fn enable_command(runtime: &str, digest: &PackageDigest) -> String {
    let hex = digest
        .as_str()
        .strip_prefix("sha256:")
        .expect("a sha256 digest");
    format!("pohunek plugin enable pohunek.runtime.{runtime} --digest {hex}")
}

/// A host whose `runtime` package `installed` is installed, selected and bound
/// to a profile, then disabled by the operator, with `release` published as the
/// release under a later catalog.
fn host_with_disabled_selection(
    runtime: &str,
    installed: &(Vec<u8>, PackageDigest),
    release: &(Vec<u8>, PackageDigest),
) -> Host {
    let host = Host::new();
    host.publish_archives(1, &[(runtime, &installed.0, &installed.1)]);
    host.write_profile(runtime, &format!("base = \"{runtime}\"\n"));
    let first = host.step(false);
    assert!(first.refusals.is_empty(), "{runtime}: {first:?}");
    host.disable_package(&installed.1);
    if runtime == "claude" {
        host.write_claude_hook();
    }
    host.publish_archives(2, &[(runtime, &release.0, &release.1)]);
    host
}

/// Asserts that the plan and the apply of `host` leave the runtime whose
/// disabled package is `digest` as the operator set it: no package, no
/// integration, no refusal, one gap whose hint enables `digest`, and the
/// registry and the profile unchanged.
fn assert_disabled_selection_left(host: &Host, runtime: &str, digest: &PackageDigest, label: &str) {
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profile = host.profile_text_of(runtime);

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        let context = format!("{runtime} {label}");
        assert!(
            report.refusals.is_empty(),
            "{context}: {:?}",
            report.refusals
        );
        assert!(
            report.packages.is_empty(),
            "{context}: {:?}",
            report.packages
        );
        assert!(report.integrations.is_empty(), "{context}");
        assert!(
            report.profiles.is_empty(),
            "{context}: {:?}",
            report.profiles
        );
        assert_eq!(
            gap_codes(report)
                .into_iter()
                .filter(|(name, _)| *name == runtime)
                .collect::<Vec<_>>(),
            [(runtime, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)],
            "{context}"
        );
        let gap = report
            .gaps
            .iter()
            .find(|gap| gap.runtime == runtime)
            .expect("the runtime's gap");
        assert!(
            gap.detail.contains(&enable_command(runtime, digest)),
            "{context}: {}",
            gap.detail
        );
        assert!(
            !gap.detail.contains("plugin update"),
            "{context}: {}",
            gap.detail
        );
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry,
        "{runtime} {label}"
    );
    assert_eq!(host.profile_text_of(runtime), profile, "{runtime} {label}");
}

/// A selected version with no order against the release's that the operator
/// disabled is left as set: nothing is replaced, selected or enabled, and the
/// plan reports a gap.
#[test]
fn a_disabled_unordered_selected_reserved_runtime_version_is_left_untouched() {
    for runtime in ["claude", "codex", "hermes"] {
        let installed = official_runtimes::variant_archive(runtime, "2.0");
        let release = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
        let host = host_with_disabled_selection(runtime, &installed, &release);

        assert_disabled_selection_left(&host, runtime, &installed.1, "unordered");
    }
}

/// A selected version that is ordered against the release's, older or newer or
/// equal under another digest, is left as the operator disabled it too: the
/// release's package neither replaces nor re-enables it, and a newer version is
/// never downgraded.
#[test]
fn a_disabled_ordered_selected_reserved_runtime_version_is_left_untouched() {
    for runtime in ["claude", "codex", "hermes"] {
        let alternative = format!("{runtime}-alt");
        let release = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
        for (label, installed) in [
            (
                "older",
                official_runtimes::variant_archive(runtime, "1.0.1"),
            ),
            (
                "newer",
                official_runtimes::variant_archive(runtime, "2.1.0"),
            ),
            (
                "equal",
                official_runtimes::variant_archive_with_program(
                    runtime,
                    RELEASE_VERSION,
                    &alternative,
                ),
            ),
        ] {
            let host = host_with_disabled_selection(runtime, &installed, &release);

            assert_disabled_selection_left(&host, runtime, &installed.1, label);
        }
    }
}

/// A disabled selected package whose root is missing is left as set, and the
/// hint restores the root before it enables the package: `plugin enable` of a
/// package without a root fails.
#[test]
fn a_disabled_selection_without_its_root_is_left_with_a_restoring_hint() {
    for (label, version) in [("older", "1.0.1"), ("newer", "2.1.0")] {
        let installed = official_runtimes::variant_archive("claude", version);
        let release = official_runtimes::variant_archive("claude", RELEASE_VERSION);
        let host = host_with_disabled_selection("claude", &installed, &release);
        host.remove_installed_root();
        let registry = fs::read(host.registry_path()).expect("the registry record");

        let plan = host.step(true);
        let applied = host.step(false);

        for report in [&plan, &applied] {
            assert!(report.refusals.is_empty(), "{label}: {:?}", report.refusals);
            assert!(report.packages.is_empty(), "{label}: {:?}", report.packages);
            let gap = report
                .gaps
                .iter()
                .find(|gap| gap.runtime == "claude")
                .expect("the claude gap");
            assert_eq!(gap.code, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING, "{label}");
            let restore = gap
                .detail
                .find("pohunek plugin install")
                .expect("a restore hint");
            let enable = gap
                .detail
                .find(&enable_command("claude", &installed.1))
                .expect("an enable hint");
            assert!(restore < enable, "{label}: {}", gap.detail);
        }
        assert_eq!(
            fs::read(host.registry_path()).expect("the registry record"),
            registry,
            "{label}"
        );
    }
}

/// A disabled selected package whose root is altered is refused with the
/// command that removes it, as an enabled one is, and nothing changes.
#[test]
fn a_disabled_selection_with_an_altered_root_is_refused() {
    for runtime in ["claude", "codex", "hermes"] {
        let installed = official_runtimes::variant_archive(runtime, "1.0.1");
        let release = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
        let host = host_with_disabled_selection(runtime, &installed, &release);
        host.alter_installed_root();
        let registry = fs::read(host.registry_path()).expect("the registry record");
        let root_refusal = ProtocolError::from(PackageErrorKind::RootInvalid).code;

        let plan = host.step(true);
        let applied = host.step(false);

        let detected = host.detect_step();
        for report in [&plan, &applied, &detected] {
            assert_eq!(refusal_codes(report), [root_refusal.as_str()], "{runtime}");
            assert!(
                report.refusals[0].detail.contains(&format!(
                    "pohunek plugin uninstall pohunek.runtime.{runtime} --digest {} --remove-modified --yes",
                    installed.1.as_str()
                )),
                "{}",
                report.refusals[0].detail
            );
            assert!(report.packages.is_empty(), "{runtime}");
        }
        assert_eq!(
            fs::read(host.registry_path()).expect("the registry record"),
            registry,
            "{runtime}"
        );
    }
}

/// The release's own package, selected and disabled, is left disabled: an
/// install records the enabled flag and the selection in one registry commit,
/// so only the operator leaves a package selected and disabled.
#[test]
fn a_disabled_selected_release_package_is_left_untouched() {
    for runtime in ["claude", "codex", "hermes"] {
        let release = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
        let host = host_with_disabled_selection(runtime, &release, &release);

        assert_disabled_selection_left(&host, runtime, &release.1, "release");
    }
}

/// A host whose `runtime` has the enabled version `selected_version` serving
/// it, with profile `runtime` and, for claude, a hook, beside the release's
/// package recorded as disabled (`plugin install --no-enable`). Returns the
/// host, the serving digest and the release digest.
fn host_serving_beside_a_disabled_release(
    runtime: &str,
    selected_version: &str,
) -> (Host, PackageDigest, PackageDigest) {
    let host = Host::new();
    let release = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
    let selected = official_runtimes::variant_archive(runtime, selected_version);
    host.write_profile(runtime, &format!("base = \"{runtime}\"\n"));
    host.publish_archives(1, &[(runtime, &release.0, &release.1)]);
    assert!(host.step(false).refusals.is_empty());
    host.publish_archives(2, &[(runtime, &selected.0, &selected.1)]);
    let swapped = host.step(false);
    assert!(swapped.refusals.is_empty(), "{runtime}: {swapped:?}");
    host.publish_archives(3, &[(runtime, &release.0, &release.1)]);
    let store = pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
        .expect("the package store");
    store
        .registry()
        .select(&selected.1)
        .expect("select the serving version");
    host.disable_package(&release.1);
    if runtime == "claude" {
        host.write_claude_hook();
    }
    (host, selected.1, release.1)
}

/// The release's package is installed and disabled while an enabled version,
/// older or newer than the release's, serves the runtime: the version keeps
/// serving with its package entry and integrations, nothing is enabled,
/// selected or downgraded, and the gap names the release's digest.
#[tokio::test]
async fn a_disabled_release_package_leaves_the_serving_selection_in_place() {
    for runtime in ["claude", "codex", "hermes"] {
        for (label, version) in [("older", "1.0.1"), ("newer", "2.1.0")] {
            let (host, selected, release) =
                host_serving_beside_a_disabled_release(runtime, version);
            let registry = fs::read(host.registry_path()).expect("the registry record");
            let context = format!("{runtime} {label}");

            let plan = host.step(true);
            let applied = host.step(false);

            for report in [&plan, &applied] {
                assert!(
                    report.refusals.is_empty(),
                    "{context}: {:?}",
                    report.refusals
                );
                let package = report
                    .packages
                    .iter()
                    .find(|package| package.runtime == runtime)
                    .unwrap_or_else(|| panic!("{context}: {:?}", report.packages));
                assert_eq!(package.status, PackageStatus::Kept, "{context}");
                assert_eq!(package.digest, selected.as_str(), "{context}");
                assert_eq!(package.replaces_newer, None, "{context}");
                if runtime == "claude" {
                    assert!(!report.integrations.is_empty(), "{context}");
                }
                let gap = report
                    .gaps
                    .iter()
                    .find(|gap| gap.runtime == runtime)
                    .unwrap_or_else(|| panic!("{context}: {:?}", report.gaps));
                assert_eq!(gap.code, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING, "{context}");
                assert!(
                    gap.detail.contains("keeps serving"),
                    "{context}: {}",
                    gap.detail
                );
                assert!(
                    gap.detail.contains(&enable_command(runtime, &release)),
                    "{context}: {}",
                    gap.detail
                );
            }
            assert_eq!(
                fs::read(host.registry_path()).expect("the registry record"),
                registry,
                "{context}"
            );

            official_runtimes::enable_package(&host.plugins_dir(), host.env.root(), &release).await;
            let enabled = host.step(true);
            assert!(
                enabled.refusals.is_empty(),
                "{context}: {:?}",
                enabled.refusals
            );
            let package = enabled
                .packages
                .iter()
                .find(|package| package.runtime == runtime)
                .expect("the runtime package");
            assert_eq!(package.digest, release.as_str(), "{context}");
            assert_eq!(package.status, PackageStatus::Install, "{context}");
        }
    }
}

/// The release's package is recorded disabled with an altered root while an
/// enabled selection serves the runtime: the plan refuses it with the command
/// that removes the root, as it does for any release package, and changes
/// nothing.
#[test]
fn a_disabled_release_package_with_an_altered_root_beside_a_serving_selection_is_refused() {
    for runtime in ["claude", "codex", "hermes"] {
        for version in ["1.0.1", "2.1.0"] {
            let (host, _selected, release) =
                host_serving_beside_a_disabled_release(runtime, version);
            host.alter_package_root(&release);
            let registry = fs::read(host.registry_path()).expect("the registry record");
            let root_refusal = ProtocolError::from(PackageErrorKind::RootInvalid).code;
            let context = format!("{runtime} {version}");

            let plan = host.step(true);
            let applied = host.step(false);

            for report in [&plan, &applied] {
                assert_eq!(
                    refusal_codes(report),
                    [root_refusal.as_str()],
                    "{context}: {:?}",
                    report.refusals
                );
                assert!(
                    report.refusals[0].detail.contains(&format!(
                        "pohunek plugin uninstall pohunek.runtime.{runtime} --digest {} \
                         --remove-modified --yes",
                        release.as_str()
                    )),
                    "{context}: {}",
                    report.refusals[0].detail
                );
                assert!(
                    !gap_codes(report).contains(&(runtime, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)),
                    "{context}: {:?}",
                    report.gaps
                );
            }
            assert_eq!(
                fs::read(host.registry_path()).expect("the registry record"),
                registry,
                "{context}"
            );
        }
    }
}

/// A host whose enabled Pi package `selected_version` serves the runtime
/// beside the release's own Pi package `release_version`, installed, disabled
/// and altered at its root.
async fn pi_serving_beside_an_altered_disabled_release(
    selected_version: &str,
    release_version: &str,
) -> (Host, PackageDigest) {
    let host = Host::with_installed_pi().await;
    let release = host.publish_pi(release_version, 1);
    let installed = host.step(false);
    assert!(installed.refusals.is_empty(), "{installed:?}");
    let selected = official_runtimes::variant_archive("pi", selected_version).1;
    if host
        .pi_registry()
        .0
        .iter()
        .all(|version| version != selected_version)
    {
        host.publish_pi(selected_version, 2);
        let swapped = host.step(false);
        assert!(swapped.refusals.is_empty(), "{swapped:?}");
        host.publish_pi(release_version, 3);
    }
    pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
        .expect("the package store")
        .registry()
        .select(&selected)
        .expect("select the serving version");
    host.disable_package(&release);
    host.alter_package_root(&release);
    (host, release)
}

/// For Pi the altered root of the disabled release package is refused beside an
/// older enabled selection, which the release would update, and ignored beside
/// a newer one, which is kept silently.
#[tokio::test]
async fn a_disabled_pi_release_package_with_an_altered_root_is_refused_only_beside_an_older_selection(
) {
    let (host, release) =
        pi_serving_beside_an_altered_disabled_release(PI_INSTALLED_VERSION, PI_NEWER_VERSION).await;
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let root_refusal = ProtocolError::from(PackageErrorKind::RootInvalid).code;
    for report in [host.step(true), host.step(false)] {
        assert_eq!(
            refusal_codes(&report),
            [root_refusal.as_str()],
            "{:?}",
            report.refusals
        );
        assert!(
            report.refusals[0].detail.contains(&format!(
                "pohunek plugin uninstall {PI_PACKAGE} --digest {} --remove-modified --yes",
                release.as_str()
            )),
            "{}",
            report.refusals[0].detail
        );
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );

    let (host, _release) =
        pi_serving_beside_an_altered_disabled_release(PI_NEWER_VERSION, PI_INSTALLED_VERSION).await;
    let registry = fs::read(host.registry_path()).expect("the registry record");
    for report in [host.step(true), host.step(false)] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert_eq!(pi_package(&report).status, PackageStatus::Kept);
        assert_eq!(pi_package(&report).version, PI_NEWER_VERSION);
        assert!(report.gaps.is_empty(), "{:?}", report.gaps);
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
}

/// A detecting plan drops a runtime nothing uses whose program is off the
/// search path before it judges the altered root of the disabled release
/// package, so the plan is not refused for an agent that is not there.
#[test]
fn a_detecting_plan_drops_an_unused_runtime_before_judging_a_damaged_release_package() {
    for runtime in ["codex", "hermes"] {
        let (host, _selected, release) = host_serving_beside_a_disabled_release(runtime, "1.0.1");
        host.alter_package_root(&release);
        fs::remove_file(host.profiles_dir().join(format!("{runtime}.toml")))
            .expect("remove the profile");

        let detected = host.detect_step();

        assert!(
            detected.refusals.is_empty(),
            "{runtime}: {:?}",
            detected.refusals
        );
        assert!(
            detected
                .packages
                .iter()
                .all(|package| package.runtime != runtime),
            "{runtime}: {:?}",
            detected.packages
        );
    }
}

/// The release's package is the only record of its package, disabled and not
/// selected, and nothing else serves the runtime: the upgrade neither installs
/// nor enables nor selects it, and the gap hint enables it.
#[tokio::test]
async fn a_disabled_unselected_release_package_with_nothing_serving_is_left_untouched() {
    for runtime in ["claude", "codex", "hermes"] {
        let host = Host::new();
        let release = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
        host.publish_archives(1, &[(runtime, &release.0, &release.1)]);
        host.write_profile(runtime, &format!("base = \"{runtime}\"\n"));
        assert!(host.step(false).refusals.is_empty());
        let mut registry: serde_json::Value =
            serde_json::from_slice(&fs::read(host.registry_path()).expect("registry"))
                .expect("registry json");
        registry["packages"][0]["enabled"] = serde_json::Value::Bool(false);
        registry["selected"] = serde_json::json!({});
        fs::write(host.registry_path(), registry.to_string()).expect("write the registry");

        assert_disabled_selection_left(&host, runtime, &release.1, "unselected");

        official_runtimes::enable_package(&host.plugins_dir(), host.env.root(), &release.1).await;
        let enabled = host.step(true);
        assert!(
            enabled.refusals.is_empty(),
            "{runtime}: {:?}",
            enabled.refusals
        );
        let package = enabled
            .packages
            .iter()
            .find(|package| package.runtime == runtime)
            .expect("the runtime package");
        assert_eq!(package.digest, release.1.as_str(), "{runtime}");
    }
}

/// Records a copy of the first registry package as the disabled package
/// `digest` at `version`, as a `plugin install --no-enable` of it would leave it.
fn record_disabled_copy(host: &Host, digest: &PackageDigest, version: &str) {
    let mut registry: serde_json::Value =
        serde_json::from_slice(&fs::read(host.registry_path()).expect("registry"))
            .expect("registry json");
    let mut record = registry["packages"][0].clone();
    record["digest"] = json_string(digest.as_str());
    record["identity"]["version"] = json_string(version);
    record["enabled"] = serde_json::Value::Bool(false);
    let packages = registry["packages"].as_array_mut().expect("packages");
    packages.push(record);
    packages.sort_by_key(|record| record["digest"].as_str().map(str::to_owned));
    fs::write(host.registry_path(), registry.to_string()).expect("write the registry");
}

/// A kept enabled selection is not turned into a gap by a disabled record of
/// the release's package: an unordered claude selection stays kept.
#[test]
fn a_disabled_release_record_does_not_override_a_kept_enabled_selection() {
    let host = Host::new();
    publish_claude_version(&host, "1.0.2", 1);
    host.write_claude_profile();
    assert!(host.step(false).refusals.is_empty());
    let release = publish_claude_version(&host, "2.0", 2);
    record_disabled_copy(&host, &release, "2.0");

    for report in [host.step(true), host.step(false)] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert_eq!(report.packages[0].status, PackageStatus::Kept);
        let codes = gap_codes(&report);
        assert!(
            codes.contains(&("claude", CODE_GAP_UPDATE_VERSION_UNORDERED)),
            "{codes:?}"
        );
        assert!(
            !codes.contains(&("claude", CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)),
            "{codes:?}"
        );
    }
}

/// A Pi selection newer than the release's stays kept, never downgraded, beside
/// a disabled record of the release's package.
#[tokio::test]
async fn a_disabled_release_record_does_not_override_a_newer_kept_pi_selection() {
    let host = Host::with_installed_pi().await;
    let release = host.publish_pi(PI_OLDER_VERSION, 1);
    record_disabled_copy(&host, &release, PI_OLDER_VERSION);

    for report in [host.step(true), host.step(false)] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert_eq!(pi_package(&report).status, PackageStatus::Kept);
        assert_eq!(pi_package(&report).version, PI_INSTALLED_VERSION);
        assert!(report.gaps.is_empty(), "{:?}", report.gaps);
    }
}

/// Following the hint enables the package it names, and the next upgrade then
/// proceeds as it does for an enabled package: an older version is replaced, a
/// newer one is replaced with the downgrade named, the release's own package is
/// present.
#[tokio::test]
async fn enabling_the_hinted_package_lets_the_upgrade_proceed() {
    let release = official_runtimes::variant_archive("claude", RELEASE_VERSION);
    for (label, installed) in [
        (
            "older",
            official_runtimes::variant_archive("claude", "1.0.1"),
        ),
        (
            "newer",
            official_runtimes::variant_archive("claude", "2.1.0"),
        ),
        ("release", release.clone()),
    ] {
        let host = host_with_disabled_selection("claude", &installed, &release);
        let gap = host
            .step(true)
            .gaps
            .into_iter()
            .find(|gap| gap.runtime == "claude")
            .expect("the claude gap");
        let hex = gap
            .detail
            .split("--digest ")
            .nth(1)
            .expect("the hint names a digest")
            .trim();
        let hinted = PackageDigest::parse(&format!("sha256:{hex}")).expect("the hinted digest");
        assert_eq!(hinted, installed.1, "{label}");
        official_runtimes::enable_package(&host.plugins_dir(), host.env.root(), &hinted).await;

        let plan = host.step(true);

        assert!(plan.refusals.is_empty(), "{label}: {:?}", plan.refusals);
        let package = &plan.packages[0];
        match label {
            "older" => assert_eq!(package.status, PackageStatus::Install, "{label}"),
            "newer" => {
                assert_eq!(package.status, PackageStatus::Install, "{label}");
                assert!(package.replaces_newer.is_some(), "{label}");
            }
            _ => assert_eq!(package.status, PackageStatus::Present, "{label}"),
        }
        assert_eq!(package.digest, release.1.as_str(), "{label}");
    }
}

/// A kept selection is judged by the detecting plan as the release's package
/// is: a program off the search path is a gap and the runtime is not marked
/// detected, a program on it is, and a missing config home is a gap.
#[test]
fn a_kept_unordered_selection_goes_through_the_detect_checks() {
    let host = Host::new();
    let (bytes, digest) = official_runtimes::variant_archive("claude", "2.0");
    host.publish_archives(1, &[("claude", &bytes, &digest)]);
    host.write_claude_profile();
    let installed = host.step(false);
    assert!(installed.refusals.is_empty(), "{installed:?}");
    publish_claude_version(&host, "1.0.3", 2);

    let without_program = host.detect_step();

    assert_eq!(without_program.packages[0].status, PackageStatus::Kept);
    assert_eq!(without_program.packages[0].reasons, [UseReason::Profile]);
    assert!(
        gap_codes(&without_program).contains(&("claude", CODE_GAP_AGENT_NOT_ON_PATH)),
        "{:?}",
        without_program.gaps
    );

    host.put_program("claude");

    let without_home = host.detect_step();

    assert_eq!(without_home.packages[0].status, PackageStatus::Kept);
    assert!(without_home.packages[0]
        .reasons
        .contains(&UseReason::Detected));
    let codes = gap_codes(&without_home);
    assert!(
        codes.contains(&("claude", CODE_GAP_CONFIG_HOME_MISSING)),
        "{codes:?}"
    );
    assert!(
        codes.contains(&("claude", CODE_GAP_UPDATE_VERSION_UNORDERED)),
        "{codes:?}"
    );
}

/// An enabled selected version with no order against the release's whose root
/// does not load is not replaced either: the runtime is left as it is and the
/// plan reports a gap with a repair hint.
#[test]
fn an_unloadable_unordered_selected_reserved_runtime_version_is_left_untouched() {
    for runtime in ["claude", "codex", "hermes"] {
        let host = Host::new();
        let (bytes, digest) = official_runtimes::variant_archive(runtime, "2.0");
        host.publish_archives(1, &[(runtime, &bytes, &digest)]);
        host.write_profile(runtime, &format!("base = \"{runtime}\"\n"));
        let installed = host.step(false);
        assert!(installed.refusals.is_empty(), "{runtime}: {installed:?}");
        host.remove_installed_root();
        let (release, release_digest) = official_runtimes::variant_archive(runtime, "1.0.3");
        host.publish_archives(2, &[(runtime, &release, &release_digest)]);
        let registry = fs::read(host.registry_path()).expect("the registry record");

        let plan = host.step(true);
        let applied = host.step(false);

        for report in [&plan, &applied] {
            assert!(
                report.refusals.is_empty(),
                "{runtime}: {:?}",
                report.refusals
            );
            assert!(
                report.packages.is_empty(),
                "{runtime}: {:?}",
                report.packages
            );
            let gaps: Vec<_> = report
                .gaps
                .iter()
                .filter(|gap| gap.runtime == runtime)
                .collect();
            assert_eq!(gaps.len(), 1, "{runtime}: {gaps:?}");
            assert_eq!(gaps[0].code, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING);
            assert!(gaps[0].detail.contains("pohunek plugin select"), "{gaps:?}");
        }
        assert_eq!(
            fs::read(host.registry_path()).expect("the registry record"),
            registry,
            "{runtime}"
        );
    }
}

#[tokio::test]
async fn an_unloadable_unordered_pi_selection_is_left_untouched() {
    let host = Host::with_installed_pi().await;
    let state = pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
        .expect("the package store")
        .registry()
        .state()
        .expect("the registry state");
    let digest = state
        .packages()
        .iter()
        .find(|record| record.identity().id.as_str() == PI_PACKAGE)
        .map(|record| record.digest().as_str().to_owned())
        .expect("the installed Pi package");
    host.remove_package_root(&digest);
    host.publish_pi("2.0", 1);
    host.write_profile(
        "work",
        &format!("base = \"pi\"\n{}", official_runtimes::profile_pin("pi")),
    );
    let registry = fs::read(host.registry_path()).expect("the registry record");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert!(
            report
                .packages
                .iter()
                .all(|package| package.runtime != "pi"),
            "{:?}",
            report.packages
        );
        assert_eq!(
            gap_codes(report)
                .into_iter()
                .filter(|(runtime, _)| *runtime == "pi")
                .collect::<Vec<_>>(),
            [("pi", CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)]
        );
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
}

/// A profile pinned to a digest the registry holds no record of is kept, as
/// its version cannot be compared with the release package.
#[test]
fn a_profile_pinned_to_an_unrecorded_digest_is_kept_with_its_version_unknown() {
    let host = Host::new();
    publish_claude_version(&host, OLDER_VERSION, 1);
    let unrecorded = format!("sha256:{}", "0".repeat(64));
    host.write_profile(
        CLAUDE_PROFILE,
        &format!(
            "base = \"claude\"\npackage = \"pohunek.runtime.claude\"\ndigest = \"{unrecorded}\"\n"
        ),
    );
    let profile = fs::read(host.profile_path()).expect("the kept profile");

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.profiles.len(), 1);
    assert_kept(&plan.profiles[0], &unrecorded, CODE_PINNED_VERSION_UNKNOWN);

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_kept(
        &applied.profiles[0],
        &unrecorded,
        CODE_PINNED_VERSION_UNKNOWN,
    );
    assert_eq!(
        fs::read(host.profile_path()).expect("the kept profile"),
        profile
    );
}

/// The package id of the official Pi package.
const PI_PACKAGE: &str = "pohunek.runtime.pi";

/// The version of the Pi package `runtime-packages/pi` builds, which
/// [`Host::with_installed_pi`] installs.
const PI_INSTALLED_VERSION: &str = "1.0.4";

/// A version newer than [`PI_INSTALLED_VERSION`], published as a later release.
const PI_NEWER_VERSION: &str = "1.0.5";

/// A version older than [`PI_INSTALLED_VERSION`], published as an earlier release.
const PI_OLDER_VERSION: &str = "1.0.2";

impl Host {
    /// A host whose package store holds the official packages at the versions
    /// `runtime-packages` builds, installed from the official catalog, enabled
    /// and selected.
    async fn with_installed_pi() -> Self {
        let host = Self::new();
        host.catalog
            .preinstall(&host.plugins_dir(), host.env.root())
            .await;
        host
    }

    /// Publishes a release whose Pi package declares `version`, under catalog
    /// `sequence`, and returns its digest.
    fn publish_pi(&self, version: &str, sequence: u64) -> PackageDigest {
        let (bytes, digest) = official_runtimes::variant_archive("pi", version);
        self.publish_archives(sequence, &[("pi", &bytes, &digest)]);
        digest
    }

    /// The versions of the Pi package the registry holds, and the one it
    /// selects.
    fn pi_registry(&self) -> (Vec<String>, Option<String>) {
        let state = pohunek_daemon::agent::host::PackageStore::open(&self.plugins_dir())
            .expect("the package store")
            .registry()
            .state()
            .expect("the registry state");
        let mut versions = Vec::new();
        let mut selected = None;
        for record in state.packages() {
            if record.identity().id.as_str() != PI_PACKAGE {
                continue;
            }
            let version = record.identity().version.as_str().to_owned();
            if state.selected(&record.identity().id) == Some(record.digest()) {
                selected = Some(version.clone());
            }
            versions.push(version);
        }
        versions.sort();
        (versions, selected)
    }

    /// Uninstalls the installed Pi package from the registry.
    fn uninstall_pi(&self) {
        pohunek_daemon::agent::host::PackageStore::open(&self.plugins_dir())
            .expect("the package store")
            .registry()
            .uninstall(
                &Self::installed_pi_digest(),
                &package::registry::RetainedDigests::new(),
            )
            .expect("uninstall the Pi package");
    }

    /// The digest of the installed Pi package.
    fn installed_pi_digest() -> PackageDigest {
        official_runtimes::variant_archive("pi", PI_INSTALLED_VERSION).1
    }
}

/// The entry of the Pi runtime in `report`.
fn pi_package(report: &EnsureReport) -> &pohunek_service_config::runtimes::PackageAction {
    report
        .packages
        .iter()
        .find(|package| package.runtime == "pi")
        .unwrap_or_else(|| panic!("a plan entry for pi: {:?}", report.packages))
}

#[tokio::test]
async fn the_upgrade_updates_an_installed_official_pi_package() {
    let host = Host::with_installed_pi().await;
    let newer = host.publish_pi(PI_NEWER_VERSION, 1);

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.packages.len(), 1, "{:?}", plan.packages);
    let pi = pi_package(&plan);
    assert_eq!(pi.status, PackageStatus::Install);
    assert_eq!(pi.version, PI_NEWER_VERSION);
    assert_eq!(pi.digest, newer.as_str());
    assert_eq!(pi.reasons, [UseReason::Installed]);
    assert_eq!(
        host.pi_registry(),
        (
            vec![PI_INSTALLED_VERSION.to_owned()],
            Some(PI_INSTALLED_VERSION.to_owned())
        ),
        "a dry run changes nothing"
    );

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(pi_package(&applied).status, PackageStatus::Installed);
    assert_eq!(
        host.pi_registry(),
        (
            vec![PI_INSTALLED_VERSION.to_owned(), PI_NEWER_VERSION.to_owned()],
            Some(PI_NEWER_VERSION.to_owned())
        ),
        "the release version is installed and selected, the older one stays installed"
    );
}

#[tokio::test]
async fn the_upgrade_repins_a_pi_profile_pinned_to_the_older_official_version() {
    let host = Host::with_installed_pi().await;
    let newer = host.publish_pi(PI_NEWER_VERSION, 1);
    host.write_profile(
        "work",
        &format!("base = \"pi\"\n{}", official_runtimes::profile_pin("pi")),
    );

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.profiles.len(), 1, "{:?}", plan.profiles);
    assert_eq!(plan.profiles[0].name, "work");
    assert_eq!(plan.profiles[0].status, ProfileStatus::Repin);
    assert_eq!(plan.profiles[0].digest, newer.as_str());
    assert_eq!(
        plan.profiles[0].previous_version.as_deref(),
        Some(PI_INSTALLED_VERSION)
    );

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(applied.profiles[0].status, ProfileStatus::Repinned);
    assert!(
        host.profile_text_of("work")
            .contains(&format!("digest = \"{}\"", newer.as_str())),
        "{}",
        host.profile_text_of("work")
    );
}

#[tokio::test]
async fn pi_profile_pins_follow_the_official_repin_rules() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    let claude = official_runtimes::profile_pin("claude");
    let unrecorded = format!("sha256:{}", "0".repeat(64));
    host.write_profile("other-package", &format!("base = \"pi\"\n{claude}"));
    host.write_profile(
        "unknown-version",
        &format!("base = \"pi\"\npackage = \"{PI_PACKAGE}\"\ndigest = \"{unrecorded}\"\n"),
    );
    let before = (
        host.profile_text_of("other-package"),
        host.profile_text_of("unknown-version"),
    );

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    let mut kept: Vec<(&str, Option<&str>)> = applied
        .profiles
        .iter()
        .map(|profile| (profile.name.as_str(), profile.kept_reason.as_deref()))
        .collect();
    kept.sort_unstable();
    assert_eq!(
        kept,
        [
            ("other-package", Some(CODE_PINNED_TO_OTHER_PACKAGE)),
            ("unknown-version", Some(CODE_PINNED_VERSION_UNKNOWN)),
        ]
    );
    assert_eq!(
        (
            host.profile_text_of("other-package"),
            host.profile_text_of("unknown-version")
        ),
        before
    );
}

#[tokio::test]
async fn an_unpinned_pi_profile_is_not_bound_by_the_upgrade() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    host.write_profile("work", "base = \"pi\"\n");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert!(report.profiles.is_empty(), "{:?}", report.profiles);
    }
    assert_eq!(host.profile_text_of("work"), "base = \"pi\"\n");
    assert_eq!(pi_package(&applied).status, PackageStatus::Installed);
}

#[test]
fn the_upgrade_never_installs_an_absent_pi_package() {
    let host = Host::new();
    host.publish_at(&["pi"], 1);
    host.write_profile("work", "base = \"pi\"\n");
    host.put_program("pi");

    let plan = host.step(true);
    assert!(!host.plugins_dir().exists(), "a dry run creates no store");
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert!(report.packages.is_empty(), "{:?}", report.packages);
        assert!(report.profiles.is_empty(), "{:?}", report.profiles);
    }
    assert_eq!(
        host.pi_registry().0,
        Vec::<String>::new(),
        "nothing installed"
    );
}

#[tokio::test]
async fn the_upgrade_never_installs_pi_beside_other_installed_official_packages() {
    let host = Host::with_installed_pi().await;
    host.uninstall_pi();
    host.publish_pi(PI_NEWER_VERSION, 1);
    host.write_profile("work", "base = \"pi\"\n");
    host.put_program("pi");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert!(report.packages.is_empty(), "{:?}", report.packages);
        assert!(report.profiles.is_empty(), "{:?}", report.profiles);
        assert!(report.gaps.is_empty(), "{:?}", report.gaps);
    }
    assert_eq!(
        host.pi_registry().0,
        Vec::<String>::new(),
        "nothing installed"
    );
}

#[tokio::test]
async fn a_non_official_pi_package_is_not_managed() {
    let host = Host::new();
    official_runtimes::preinstall_third_party(
        &host.plugins_dir(),
        host.env.root(),
        "pi",
        "acme.runtime.pi",
    )
    .await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    host.write_profile("work", "base = \"pi\"\n");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert!(report.packages.is_empty(), "{:?}", report.packages);
        assert!(report.gaps.is_empty(), "{:?}", report.gaps);
    }
}

#[tokio::test]
async fn a_current_pi_package_is_reported_present_and_idempotent() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_INSTALLED_VERSION, 1);
    let registry = fs::read(host.registry_path()).expect("the registry record");

    let first = host.step(false);
    let second = host.step(false);

    for report in [&first, &second] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert_eq!(pi_package(report).status, PackageStatus::Present);
    }
    assert_eq!(first.packages, second.packages);
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
}

#[tokio::test]
async fn a_newer_selected_pi_package_is_never_downgraded() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_OLDER_VERSION, 1);
    host.write_profile(
        "work",
        &format!("base = \"pi\"\n{}", official_runtimes::profile_pin("pi")),
    );
    let profile = host.profile_text_of("work");
    let registry = fs::read(host.registry_path()).expect("the registry record");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        let pi = pi_package(report);
        assert_eq!(pi.status, PackageStatus::Kept);
        assert_eq!(pi.version, PI_INSTALLED_VERSION);
        assert_eq!(pi.release_version.as_deref(), Some(PI_OLDER_VERSION));
        assert_eq!(pi.replaces_newer, None);
        assert!(report.profiles.is_empty(), "{:?}", report.profiles);
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
    assert_eq!(host.profile_text_of("work"), profile);
}

#[tokio::test]
async fn a_selected_pi_version_without_an_order_against_the_release_is_kept() {
    for release_version in ["2.0", "1.0"] {
        let host = Host::with_installed_pi().await;
        host.publish_pi(release_version, 1);
        let registry = fs::read(host.registry_path()).expect("the registry record");

        let plan = host.step(true);
        let applied = host.step(false);

        for report in [&plan, &applied] {
            assert!(report.refusals.is_empty(), "{:?}", report.refusals);
            let pi = pi_package(report);
            assert_eq!(pi.status, PackageStatus::Kept, "{release_version}");
            assert_eq!(pi.version, PI_INSTALLED_VERSION);
            assert_eq!(pi.release_version.as_deref(), Some(release_version));
            assert_eq!(
                gap_codes(report),
                [("pi", CODE_GAP_UPDATE_VERSION_UNORDERED)],
                "{release_version}"
            );
        }
        assert_eq!(
            fs::read(host.registry_path()).expect("the registry record"),
            registry,
            "{release_version}"
        );
    }
}

#[tokio::test]
async fn a_disabled_unordered_pi_selection_is_reported_once_by_the_detecting_plan() {
    let host = Host::with_installed_pi().await;
    let state = pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
        .expect("the package store")
        .registry()
        .state()
        .expect("the registry state");
    let id = state
        .packages()
        .iter()
        .find(|record| record.identity().id.as_str() == PI_PACKAGE)
        .map(|record| record.digest().clone())
        .expect("the installed Pi package");
    host.disable_package(&id);
    host.publish_pi("2.0", 1);
    host.write_profile(
        "work",
        &format!("base = \"pi\"\n{}", official_runtimes::profile_pin("pi")),
    );

    let hex = id
        .as_str()
        .strip_prefix("sha256:")
        .expect("a sha256 digest");
    let command = format!("pohunek plugin enable {PI_PACKAGE} --digest {hex}");
    for plan in [host.detect_step(), host.step(true)] {
        let not_serving: Vec<_> = plan
            .gaps
            .iter()
            .filter(|gap| gap.runtime == "pi" && gap.code == CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)
            .collect();
        assert_eq!(not_serving.len(), 1, "{:?}", plan.gaps);
        let detail = &not_serving[0].detail;
        assert!(!detail.contains("plugin update"), "{detail}");
        assert!(detail.contains(&command), "{detail}");
    }
    // The hint's digest selects the disabled package, and enabling it works.
    pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
        .expect("the package store")
        .registry()
        .set_enabled(&id, true)
        .expect("the hinted package can be enabled");
}

/// An enabled selected version with no order against the release's whose root
/// is present but altered is refused with the command that removes it, and
/// nothing changes: a hint that `plugin install` would not accept is never
/// printed for it.
#[test]
fn an_altered_unordered_selected_reserved_runtime_version_is_refused() {
    for runtime in ["claude", "codex", "hermes"] {
        let host = Host::new();
        let (bytes, digest) = official_runtimes::variant_archive(runtime, "2.0");
        host.publish_archives(1, &[(runtime, &bytes, &digest)]);
        host.write_profile(runtime, &format!("base = \"{runtime}\"\n"));
        let installed = host.step(false);
        assert!(installed.refusals.is_empty(), "{runtime}: {installed:?}");
        host.alter_installed_root();
        let (release, release_digest) = official_runtimes::variant_archive(runtime, "1.0.3");
        host.publish_archives(2, &[(runtime, &release, &release_digest)]);
        let registry = fs::read(host.registry_path()).expect("the registry record");
        let root_refusal = ProtocolError::from(PackageErrorKind::RootInvalid).code;

        let plan = host.step(true);
        let applied = host.step(false);

        for report in [&plan, &applied] {
            assert_eq!(refusal_codes(report), [root_refusal.as_str()], "{runtime}");
            assert!(
                report.refusals[0].detail.contains(&format!(
                    "pohunek plugin uninstall pohunek.runtime.{runtime} --digest {} --remove-modified --yes",
                    digest.as_str()
                )),
                "{}",
                report.refusals[0].detail
            );
            assert!(report.packages.is_empty(), "{runtime}");
        }
        assert_eq!(
            fs::read(host.registry_path()).expect("the registry record"),
            registry,
            "{runtime}"
        );
    }
}

/// A host whose selected claude version 1.0.2 has no order against the release's
/// `2.0`, so the step keeps it, and whose profile `official` is bound to it.
/// Returns the host, the older version 1.0.1 the profile was bound to before,
/// and the kept one.
fn host_keeping_a_selection_over_an_older_version() -> (Host, PackageDigest, PackageDigest) {
    let host = Host::new();
    let older = publish_claude_version(&host, "1.0.1", 1);
    host.write_claude_profile();
    let bound = host.step(false);
    assert_eq!(bound.profiles[0].status, ProfileStatus::Bound, "{bound:?}");
    let kept = publish_claude_version(&host, "1.0.2", 2);
    let repinned = host.step(false);
    assert_eq!(
        repinned.profiles[0].status,
        ProfileStatus::Repinned,
        "{repinned:?}"
    );
    publish_claude_version(&host, "2.0", 3);
    (host, older, kept)
}

/// Writes the profile with `edit` applied to its text, and asserts that the plan
/// and the apply keep it for `reason` beside the kept selection, unchanged.
fn assert_pin_kept_beside_a_kept_selection(
    host: &Host,
    edit: impl Fn(&str) -> String,
    reason: &str,
) {
    let pinned = edit(&host.profile_text());
    assert_ne!(pinned, host.profile_text());
    fs::write(host.profile_path(), &pinned).expect("write the profile");
    for report in [host.step(true), host.step(false)] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert_eq!(report.packages[0].status, PackageStatus::Kept);
        assert_eq!(report.profiles.len(), 1, "{:?}", report.profiles);
        assert_eq!(report.profiles[0].status, ProfileStatus::Kept);
        assert_eq!(report.profiles[0].kept_reason.as_deref(), Some(reason));
    }
    assert_eq!(host.profile_text(), pinned);
}

/// A profile pinned to another official version of the kept selection's package
/// is reported as kept, is never repinned, and has the reason that names the
/// difference.
#[test]
fn a_profile_pinned_elsewhere_is_reported_beside_a_kept_unordered_selection() {
    let (host, older, kept) = host_keeping_a_selection_over_an_older_version();

    assert_pin_kept_beside_a_kept_selection(
        &host,
        |text| text.replace(kept.as_str(), older.as_str()),
        CODE_PINNED_DIFFERS_FROM_KEPT_SELECTION,
    );
}

/// A pin to a digest the registry does not record keeps its precise reason
/// beside a kept selection.
#[test]
fn a_profile_pinned_to_an_unknown_digest_beside_a_kept_selection_has_its_precise_reason() {
    let (host, _older, kept) = host_keeping_a_selection_over_an_older_version();
    let elsewhere = format!("sha256:{}", "b".repeat(64));

    assert_pin_kept_beside_a_kept_selection(
        &host,
        |text| text.replace(kept.as_str(), &elsewhere),
        CODE_PINNED_VERSION_UNKNOWN,
    );
}

/// A pin whose `package` key names another package than its digest records
/// keeps its precise reason beside a kept selection.
#[test]
fn a_profile_naming_another_package_beside_a_kept_selection_has_its_precise_reason() {
    let (host, older, kept) = host_keeping_a_selection_over_an_older_version();

    assert_pin_kept_beside_a_kept_selection(
        &host,
        |text| {
            text.replace(kept.as_str(), older.as_str())
                .replace("pohunek.runtime.claude", "acme.runtime.other")
        },
        CODE_PINNED_TO_OTHER_PACKAGE,
    );
}

/// A pin to a package that was not installed from the official catalog keeps its
/// precise reason beside a kept selection.
#[tokio::test]
async fn a_profile_pinned_to_a_third_party_package_beside_a_kept_selection_has_its_precise_reason()
{
    let host = Host::new();
    let foreign = official_runtimes::preinstall_third_party(
        &host.plugins_dir(),
        host.env.root(),
        "pi",
        "acme.runtime.pi",
    )
    .await;
    let kept = publish_claude_version(&host, "1.0.2", 1);
    host.write_claude_profile();
    let bound = host.step(false);
    assert_eq!(bound.profiles[0].status, ProfileStatus::Bound, "{bound:?}");
    publish_claude_version(&host, "2.0", 2);

    assert_pin_kept_beside_a_kept_selection(
        &host,
        |text| {
            text.replace(kept.as_str(), foreign.as_str())
                .replace("pohunek.runtime.claude", "acme.runtime.pi")
        },
        CODE_PINNED_TO_NON_OFFICIAL_PACKAGE,
    );
}

/// The detecting plan judges a kept selection by the program of the kept
/// package, not by the program of the release's package.
#[test]
fn a_kept_selection_is_detected_by_its_own_program() {
    const KEPT_PROGRAM: &str = "claude-kept";
    let host = Host::new();
    let (bytes, digest) =
        official_runtimes::variant_archive_with_program("claude", "2.0", KEPT_PROGRAM);
    host.publish_archives(1, &[("claude", &bytes, &digest)]);
    host.write_claude_profile();
    let installed = host.step(false);
    assert!(installed.refusals.is_empty(), "{installed:?}");
    publish_claude_version(&host, "1.0.3", 2);
    host.put_program("claude");

    let release_program_only = host.detect_step();

    assert_eq!(release_program_only.packages[0].status, PackageStatus::Kept);
    assert_eq!(
        release_program_only.packages[0].reasons,
        [UseReason::Profile]
    );
    assert!(
        gap_codes(&release_program_only).contains(&("claude", CODE_GAP_AGENT_NOT_ON_PATH)),
        "{:?}",
        release_program_only.gaps
    );

    host.put_program(KEPT_PROGRAM);

    let kept_program = host.detect_step();

    assert!(
        kept_program.packages[0]
            .reasons
            .contains(&UseReason::Detected),
        "{:?}",
        kept_program.packages
    );
    assert!(
        !gap_codes(&kept_program).contains(&("claude", CODE_GAP_AGENT_NOT_ON_PATH)),
        "{:?}",
        kept_program.gaps
    );
}

/// A runtime nothing uses whose selected version is disabled, whatever its
/// order against the release's, and whose program is off the search path, is
/// dropped by the detecting plan: no refusal, no not-serving gap.
#[test]
fn an_unused_undetected_disabled_selection_is_dropped_whatever_its_order() {
    for installed_version in ["1.0.1", "2.1.0", "2.0"] {
        let host = Host::new();
        let installed = official_runtimes::variant_archive("claude", installed_version);
        host.publish_archives(1, &[("claude", &installed.0, &installed.1)]);
        host.write_claude_profile();
        assert!(host.step(false).refusals.is_empty());
        fs::remove_file(host.profile_path()).expect("remove the profile");
        host.disable_package(&installed.1);
        publish_claude_version(&host, RELEASE_VERSION, 2);

        let plan = host.detect_step();

        assert!(
            plan.refusals.is_empty(),
            "{installed_version}: {:?}",
            plan.refusals
        );
        assert!(
            plan.packages
                .iter()
                .all(|package| package.runtime != "claude"),
            "{installed_version}: {:?}",
            plan.packages
        );
        let codes = gap_codes(&plan);
        assert!(
            !codes.contains(&("claude", CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)),
            "{installed_version}: {codes:?}"
        );
        assert!(
            codes.contains(&("claude", CODE_GAP_AGENT_NOT_ON_PATH)),
            "{installed_version}: {codes:?}"
        );
    }
}

/// A runtime nothing uses whose unordered selection is altered, disabled or
/// does not load, and whose program is off the search path, is dropped by the
/// detecting plan as a replacing plan drops it: no refusal, no not-serving gap.
#[test]
fn an_unused_undetected_unordered_selection_is_dropped_whatever_its_state() {
    for state in ["altered", "disabled", "missing"] {
        let host = Host::new();
        let (bytes, digest) = official_runtimes::variant_archive("claude", "2.0");
        host.publish_archives(1, &[("claude", &bytes, &digest)]);
        host.write_claude_profile();
        let installed = host.step(false);
        assert!(installed.refusals.is_empty(), "{state}: {installed:?}");
        fs::remove_file(host.profile_path()).expect("remove the profile");
        match state {
            "altered" => host.alter_installed_root(),
            "disabled" => host.disable_package(&digest),
            _ => host.remove_installed_root(),
        }
        publish_claude_version(&host, "1.0.3", 2);

        let plan = host.detect_step();

        assert!(plan.refusals.is_empty(), "{state}: {:?}", plan.refusals);
        assert!(
            plan.packages
                .iter()
                .all(|package| package.runtime != "claude"),
            "{state}: {:?}",
            plan.packages
        );
        let codes = gap_codes(&plan);
        assert!(
            !codes.contains(&("claude", CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)),
            "{state}: {codes:?}"
        );
        assert!(
            codes.contains(&("claude", CODE_GAP_AGENT_NOT_ON_PATH)),
            "{state}: {codes:?}"
        );
    }
}

/// A Pi selection kept because it is newer than the release's leaves a profile
/// pinned elsewhere unreported: only an unordered keep is.
#[tokio::test]
async fn a_profile_pinned_elsewhere_is_not_reported_beside_a_newer_pi_keep() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_OLDER_VERSION, 1);
    let elsewhere = format!("sha256:{}", "b".repeat(64));
    host.write_profile(
        "work",
        &format!("base = \"pi\"\npackage = \"{PI_PACKAGE}\"\ndigest = \"{elsewhere}\"\n"),
    );

    let plan = host.step(true);

    assert_eq!(pi_package(&plan).status, PackageStatus::Kept);
    assert!(plan.profiles.is_empty(), "{:?}", plan.profiles);
}

/// A kept unordered selection of a runtime nothing uses, whose program is off
/// the search path, is not planned, and the plan does not claim it is kept.
#[test]
fn an_unused_unordered_selection_without_its_program_is_not_reported_as_kept() {
    let host = Host::new();
    let (bytes, digest) = official_runtimes::variant_archive("claude", "2.0");
    host.publish_archives(1, &[("claude", &bytes, &digest)]);
    host.write_claude_profile();
    let installed = host.step(false);
    assert!(installed.refusals.is_empty(), "{installed:?}");
    fs::remove_file(host.profile_path()).expect("remove the profile");
    publish_claude_version(&host, "1.0.3", 2);

    let plan = host.detect_step();

    assert!(
        plan.packages
            .iter()
            .all(|package| package.runtime != "claude"),
        "{:?}",
        plan.packages
    );
    let codes = gap_codes(&plan);
    assert!(
        codes.contains(&("claude", CODE_GAP_AGENT_NOT_ON_PATH)),
        "{codes:?}"
    );
    assert!(
        !codes.contains(&("claude", CODE_GAP_UPDATE_VERSION_UNORDERED)),
        "{codes:?}"
    );
}

/// A kept Pi selection is not judged by the detect checks of the search path.
#[tokio::test]
async fn a_kept_pi_selection_skips_the_detect_search_path_check() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_OLDER_VERSION, 1);
    host.write_profile(
        "work",
        &format!("base = \"pi\"\n{}", official_runtimes::profile_pin("pi")),
    );

    let plan = host.detect_step();

    assert_eq!(pi_package(&plan).status, PackageStatus::Kept);
    assert!(
        !gap_codes(&plan).contains(&("pi", CODE_GAP_AGENT_NOT_ON_PATH)),
        "{:?}",
        plan.gaps
    );
}

#[tokio::test]
async fn a_pi_package_missing_from_the_catalog_is_a_gap() {
    let host = Host::with_installed_pi().await;
    host.publish_at(&["claude"], 1);
    let registry = fs::read(host.registry_path()).expect("the registry record");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert!(report.packages.is_empty(), "{:?}", report.packages);
        assert_eq!(
            gap_codes(report),
            [("pi", CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM)]
        );
        assert!(
            report.gaps[0].detail.contains(&format!(
                "pohunek plugin update {PI_PACKAGE} runtime/packages/pi.tar.zst --catalog \
                 runtime/runtime-catalog.json --yes"
            )),
            "{}",
            report.gaps[0].detail
        );
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
}

#[tokio::test]
async fn a_damaged_installed_pi_package_refuses_the_upgrade() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_INSTALLED_VERSION, 1);
    let hex = Host::installed_pi_digest()
        .as_str()
        .strip_prefix("sha256:")
        .expect("a sha256 digest")
        .to_owned();
    let root = host.plugins_dir().join("packages").join(hex);
    let mut pending = vec![root];
    let mut altered = false;
    while let Some(directory) = pending.pop() {
        if altered {
            break;
        }
        for entry in fs::read_dir(&directory).expect("a package directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if !altered {
                let mut content = fs::read(&path).expect("a package file");
                content.extend_from_slice(b"altered\n");
                fs::write(&path, content).expect("alter the package file");
                altered = true;
            }
        }
    }
    assert!(altered, "the Pi package root holds a file");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert_eq!(report.refusals.len(), 1, "{:?}", report.refusals);
        assert_eq!(report.refusals[0].subject, "pi");
        assert!(
            report.refusals[0]
                .detail
                .contains("pohunek plugin uninstall pohunek.runtime.pi --digest"),
            "{}",
            report.refusals[0].detail
        );
    }
}

#[tokio::test]
async fn without_a_catalog_an_installed_pi_package_is_kept() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    host.remove_release();
    host.write_profile("work", "base = \"pi\"\n");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        let pi = pi_package(report);
        assert_eq!(pi.status, PackageStatus::Kept);
        assert_eq!(pi.version, PI_INSTALLED_VERSION);
        assert_eq!(pi.release_version, None);
    }
}

/// A disabled selected Pi version, older or newer than the release's, is left
/// as the operator set it by every kind of plan. The gap hint enables it: `plugin
/// update` refuses a newer selection as a downgrade, so it is never printed.
#[tokio::test]
async fn a_disabled_ordered_pi_selection_is_reported_not_changed() {
    for (label, release_version) in [("older", PI_NEWER_VERSION), ("newer", PI_OLDER_VERSION)] {
        let host = Host::with_installed_pi().await;
        host.publish_pi(release_version, 1);
        host.disable_package(&Host::installed_pi_digest());
        host.write_profile("work", "base = \"pi\"\n");
        let registry = fs::read(host.registry_path()).expect("the registry record");
        let hex = Host::installed_pi_digest()
            .as_str()
            .strip_prefix("sha256:")
            .expect("a sha256 digest")
            .to_owned();
        let command = format!("pohunek plugin enable {PI_PACKAGE} --digest {hex}");

        let plan = host.step(true);
        let applied = host.step(false);
        let detected = host.detect_step();

        for report in [&plan, &applied, &detected] {
            assert!(report.refusals.is_empty(), "{label}: {:?}", report.refusals);
            assert!(report.packages.is_empty(), "{label}: {:?}", report.packages);
            let not_serving: Vec<_> = report
                .gaps
                .iter()
                .filter(|gap| {
                    gap.runtime == "pi" && gap.code == CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING
                })
                .collect();
            assert_eq!(not_serving.len(), 1, "{label}: {:?}", report.gaps);
            let detail = &not_serving[0].detail;
            assert!(detail.contains(&command), "{label}: {detail}");
            assert!(!detail.contains("plugin update"), "{label}: {detail}");
        }
        assert_eq!(
            fs::read(host.registry_path()).expect("the registry record"),
            registry,
            "{label}"
        );
    }
}

/// An installed official Pi package that nothing selects is left as it is, and
/// the gap hint is the `plugin update` command: it installs and selects the
/// release's version, which `plugin enable` of the unselected package would not.
#[tokio::test]
async fn an_unselected_pi_package_is_reported_with_the_update_hint() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    let mut registry: serde_json::Value =
        serde_json::from_slice(&fs::read(host.registry_path()).expect("registry"))
            .expect("registry json");
    registry["selected"] = serde_json::json!({});
    fs::write(host.registry_path(), registry.to_string()).expect("write the registry");

    for report in [host.step(true), host.step(false)] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert!(report.packages.is_empty(), "{:?}", report.packages);
        assert_eq!(report.gaps.len(), 1, "{:?}", report.gaps);
        let gap = &report.gaps[0];
        assert_eq!(gap.code, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING);
        assert!(
            gap.detail.contains("pohunek plugin update"),
            "{}",
            gap.detail
        );
        assert!(!gap.detail.contains("plugin enable"), "{}", gap.detail);
    }
    // The detecting plan drops the runtime, and its gap, when `pi` is off the
    // search path and nothing uses it.
    let detected = host.detect_step();
    assert!(
        detected
            .gaps
            .iter()
            .all(|gap| gap.runtime != "pi" || gap.code != CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING),
        "{:?}",
        detected.gaps
    );
}

/// Following the hint of a disabled older Pi selection lets the next upgrade
/// update it; the hinted digest is the one the registry enables.
#[tokio::test]
async fn enabling_a_hinted_older_pi_selection_lets_the_upgrade_update_it() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    host.disable_package(&Host::installed_pi_digest());
    host.write_profile("work", "base = \"pi\"\n");
    let gap = host
        .step(true)
        .gaps
        .into_iter()
        .find(|gap| gap.runtime == "pi")
        .expect("the pi gap");
    let hex = gap
        .detail
        .split("--digest ")
        .nth(1)
        .expect("the hint names a digest")
        .trim();
    let hinted = PackageDigest::parse(&format!("sha256:{hex}")).expect("the hinted digest");
    assert_eq!(hinted, Host::installed_pi_digest());
    official_runtimes::enable_package(&host.plugins_dir(), host.env.root(), &hinted).await;

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(pi_package(&plan).status, PackageStatus::Install);
    assert_eq!(pi_package(&plan).version, PI_NEWER_VERSION);
}

/// A disabled Pi selection that nothing uses and whose program is off the
/// search path is dropped by the detecting plan, which does not report the
/// package as not serving; the plan without detection still does.
#[tokio::test]
async fn a_disabled_pi_selection_nothing_detects_is_not_reported_by_the_detecting_plan() {
    for release_version in [PI_NEWER_VERSION, PI_OLDER_VERSION, "2.0"] {
        let host = Host::with_installed_pi().await;
        host.publish_pi(release_version, 1);
        host.disable_package(&Host::installed_pi_digest());

        let detected = host.detect_step();
        let plan = host.step(true);

        assert!(detected.refusals.is_empty(), "{:?}", detected.refusals);
        assert!(
            !gap_codes(&detected).contains(&("pi", CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)),
            "{release_version}: {:?}",
            detected.gaps
        );
        assert!(
            gap_codes(&plan).contains(&("pi", CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)),
            "{release_version}: {:?}",
            plan.gaps
        );
    }
}

/// A disabled Pi selection cannot be enabled while a third-party package serves
/// the runtime id: the detecting plan reports that gap and prints no enable
/// hint, which `plugin enable` would refuse.
#[tokio::test]
async fn a_disabled_pi_selection_beside_a_third_party_server_is_reported_as_claimed() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    host.disable_package(&Host::installed_pi_digest());
    official_runtimes::preinstall_third_party(
        &host.plugins_dir(),
        host.env.root(),
        "pi",
        "acme.runtime.pi",
    )
    .await;
    host.put_program("pi");

    let detected = host.detect_step();

    assert!(detected.refusals.is_empty(), "{:?}", detected.refusals);
    let codes = gap_codes(&detected);
    assert!(
        codes.contains(&("pi", CODE_GAP_SERVED_BY_OTHER_PACKAGE)),
        "{:?}",
        detected.gaps
    );
    assert!(
        !codes.contains(&("pi", CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)),
        "{codes:?}"
    );
    assert!(
        detected
            .gaps
            .iter()
            .all(|gap| !gap.detail.contains("plugin enable")),
        "{:?}",
        detected.gaps
    );
}

/// The claim is judged for a disabled Pi selection whose root is missing too:
/// restoring the root does not help while a third-party package serves `pi`.
#[tokio::test]
async fn a_rootless_disabled_pi_selection_beside_a_third_party_server_is_reported_as_claimed() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    let digest = Host::installed_pi_digest();
    host.disable_package(&digest);
    host.remove_package_root(digest.as_str());
    official_runtimes::preinstall_third_party(
        &host.plugins_dir(),
        host.env.root(),
        "pi",
        "acme.runtime.pi",
    )
    .await;
    host.put_program("pi");

    let detected = host.detect_step();

    assert!(detected.refusals.is_empty(), "{:?}", detected.refusals);
    let codes = gap_codes(&detected);
    assert!(
        codes.contains(&("pi", CODE_GAP_SERVED_BY_OTHER_PACKAGE)),
        "{:?}",
        detected.gaps
    );
    assert!(
        detected
            .gaps
            .iter()
            .all(|gap| !gap.detail.contains("plugin install")),
        "{:?}",
        detected.gaps
    );
}

/// Without detection, a disabled selected Pi package that a third-party package
/// shadows is left with the `plugin doctor` pointer even when a profile is over
/// `pi`: the plan and the apply report the gap, print no enable hint, refuse
/// nothing and change nothing.
#[tokio::test]
async fn a_disabled_pi_selection_beside_a_third_party_server_is_a_gap_without_detect() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    host.disable_package(&Host::installed_pi_digest());
    official_runtimes::preinstall_third_party(
        &host.plugins_dir(),
        host.env.root(),
        "pi",
        "acme.runtime.pi",
    )
    .await;
    host.write_profile("work", "base = \"pi\"\n");
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profile = host.profile_text_of("work");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert_eq!(
            gap_codes(report),
            [("pi", CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)],
            "{:?}",
            report.gaps
        );
        let detail = &report.gaps[0].detail;
        assert!(detail.contains("pohunek plugin doctor"), "{detail}");
        assert!(!detail.contains("plugin enable"), "{detail}");
        assert!(report.packages.is_empty(), "{:?}", report.packages);
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
    assert_eq!(host.profile_text_of("work"), profile);
}

/// Without detection, the release's Pi package is refused with the
/// runtime-conflict code while a third-party package serves `pi`, which the
/// installed Pi package, whose root is missing, no longer shadows. The plan and
/// the apply both refuse and change nothing.
#[tokio::test]
async fn a_rootless_selected_pi_package_beside_a_third_party_server_is_refused_without_detect() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    host.remove_package_root(Host::installed_pi_digest().as_str());
    official_runtimes::preinstall_third_party(
        &host.plugins_dir(),
        host.env.root(),
        "pi",
        "acme.runtime.pi",
    )
    .await;
    host.write_profile("work", "base = \"pi\"\n");
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profile = host.profile_text_of("work");
    let conflict = ProtocolError::from(PackageErrorKind::RuntimeConflict).code;

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert_eq!(
            refusal_codes(report),
            [conflict.as_str()],
            "{:?}",
            report.refusals
        );
        assert_eq!(report.refusals[0].subject, "pi");
        assert_eq!(
            report.refusals[0].detail,
            "the release package cannot take this runtime id alongside the installed runtimes"
        );
        assert!(
            !gap_codes(report).contains(&("pi", CODE_GAP_SERVED_BY_OTHER_PACKAGE)),
            "{:?}",
            report.gaps
        );
        assert!(report.packages.is_empty(), "{:?}", report.packages);
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
    assert_eq!(host.profile_text_of("work"), profile);
}

/// A disabled Pi selection nothing uses, whose root is altered or whose id
/// another package serves, is reported by the plan without detection with the
/// `plugin doctor` pointer and no enable hint, which `plugin enable` would not
/// satisfy.
#[tokio::test]
async fn an_unused_disabled_pi_selection_that_cannot_be_enabled_points_to_the_doctor() {
    for state in ["altered", "claimed"] {
        let host = Host::with_installed_pi().await;
        host.publish_pi(PI_NEWER_VERSION, 1);
        let digest = Host::installed_pi_digest();
        host.disable_package(&digest);
        if state == "altered" {
            host.alter_package_root(&digest);
        } else {
            official_runtimes::preinstall_third_party(
                &host.plugins_dir(),
                host.env.root(),
                "pi",
                "acme.runtime.pi",
            )
            .await;
        }

        let plan = host.step(true);

        assert!(plan.refusals.is_empty(), "{state}: {:?}", plan.refusals);
        let gap = plan
            .gaps
            .iter()
            .find(|gap| gap.runtime == "pi" && gap.code == CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)
            .unwrap_or_else(|| panic!("{state}: {:?}", plan.gaps));
        assert!(
            gap.detail.contains("pohunek plugin doctor"),
            "{state}: {}",
            gap.detail
        );
        assert!(
            !gap.detail.contains("plugin enable"),
            "{state}: {}",
            gap.detail
        );
    }
}

#[test]
fn install_detection_never_adds_pi() {
    let host = Host::new();
    host.publish_at(&["claude", "pi"], 1);
    host.put_program("pi");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert!(report.packages.is_empty(), "{:?}", report.packages);
        assert!(report.gaps.is_empty(), "{:?}", report.gaps);
    }
    assert_eq!(
        host.pi_registry().0,
        Vec::<String>::new(),
        "nothing installed"
    );
}

/// Appends a line to the first file under the package root `root`, so the root
/// is present but does not match its recorded manifest.
fn alter_root(root: PathBuf) {
    let mut pending = vec![root];
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

/// Copies the directory `from` to `to`, recursively.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create the directory");
    for entry in fs::read_dir(from).expect("list the directory") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        let metadata = fs::symlink_metadata(entry.path()).expect("stat");
        if metadata.is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy the file");
            fs::set_permissions(&target, metadata.permissions()).expect("copy the mode");
        }
    }
}

/// Adds the third-party package serving `pi` to the registry beside the
/// official one, the state an install refuses to produce but an edited store
/// or a registry written by another release can hold.
async fn add_conflicting_pi_package(host: &Host, package_id: &str) {
    let other = TestEnv::new().expect("a second environment");
    let other_plugins = other.root().join("plugins");
    official_runtimes::preinstall_third_party(&other_plugins, other.root(), "pi", package_id).await;
    let foreign: serde_json::Value =
        serde_json::from_slice(&fs::read(other_plugins.join("registry.json")).expect("registry"))
            .expect("registry json");
    let mut registry: serde_json::Value =
        serde_json::from_slice(&fs::read(host.registry_path()).expect("registry"))
            .expect("registry json");
    let record = foreign["packages"][0].clone();
    let digest = record["digest"].as_str().expect("digest").to_owned();
    registry["packages"]
        .as_array_mut()
        .expect("packages")
        .push(record);
    // The registry keeps its records ordered by digest.
    registry["packages"]
        .as_array_mut()
        .expect("packages")
        .sort_by_key(|record| record["digest"].as_str().map(str::to_owned));
    registry["selected"][package_id] = json_string(&digest);
    let hex = digest.strip_prefix("sha256:").expect("sha256");
    copy_tree(
        &other_plugins.join("packages").join(hex),
        &host.plugins_dir().join("packages").join(hex),
    );
    fs::write(host.registry_path(), registry.to_string()).expect("write the registry");
}

fn json_string(text: &str) -> serde_json::Value {
    serde_json::Value::String(text.to_owned())
}

/// The registry holds a third-party package selected and enabled for the id
/// `pi` beside the official one. The runtime host rejects every package of a
/// duplicated runtime id (`PackageLoad`), so no package serves `pi` and the
/// claim check of the plan finds nothing to conflict with: the update proceeds
/// and leaves the foreign package's records as they are.
#[tokio::test]
async fn a_registry_with_a_foreign_package_for_pi_is_updated_without_touching_it() {
    let foreign = "acme.runtime.pi";
    let host = Host::with_installed_pi().await;
    add_conflicting_pi_package(&host, foreign).await;
    host.publish_pi(PI_NEWER_VERSION, 1);

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert_eq!(pi_package(report).version, PI_NEWER_VERSION);
    }
    let registry: serde_json::Value =
        serde_json::from_slice(&fs::read(host.registry_path()).expect("registry"))
            .expect("registry json");
    assert!(
        registry["selected"][foreign].is_string(),
        "the foreign package keeps its selection"
    );
    let record = registry["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|record| record["identity"]["id"] == foreign)
        .expect("the foreign record");
    assert_eq!(record["enabled"], true);
}

/// An archive of a runtime package and its digest.
type Archive = (Vec<u8>, PackageDigest);

/// Sequence of the release catalog a staged host publishes, above the
/// sequences the staged installs record.
const STAGED_RELEASE_SEQUENCE: u64 = 100;

/// The state of the root of the release's own package, recorded disabled.
#[derive(Clone, Copy)]
enum ReleaseRoot {
    Intact,
    Missing,
    Altered,
}

/// A host whose `runtime` has the package `selected` installed, enabled and
/// selected and the release's own package `release` installed and recorded
/// disabled, as `plugin install --no-enable` leaves it, with its root in the
/// state `root`. A reserved runtime has a profile that uses it; Pi is planned
/// because its enabled selection is managed.
async fn host_beside_a_disabled_release(
    runtime: &str,
    selected: &Archive,
    release: &Archive,
    root: ReleaseRoot,
) -> Host {
    let host = Host::new();
    let plugins = host.plugins_dir();
    let scratch = host.env.root().to_path_buf();
    for (sequence, archive, flags) in [
        (
            1,
            selected,
            official_runtimes::InstallFlags {
                enable: true,
                select: true,
            },
        ),
        (
            2,
            release,
            official_runtimes::InstallFlags {
                enable: false,
                select: false,
            },
        ),
    ] {
        host.catalog
            .install_archive(&plugins, &scratch, runtime, archive, sequence, flags)
            .await;
    }
    host.publish_archives(
        STAGED_RELEASE_SEQUENCE,
        &[(runtime, &release.0, &release.1)],
    );
    if runtime != "pi" {
        host.write_profile(runtime, &format!("base = \"{runtime}\"\n"));
    }
    match root {
        ReleaseRoot::Intact => {}
        ReleaseRoot::Missing => host.remove_package_root(release.1.as_str()),
        ReleaseRoot::Altered => host.alter_package_root(&release.1),
    }
    host.put_program(runtime);
    host
}

/// One line naming what `report` says about `runtime`: its refusals with the
/// digest their command removes, its gaps with the command their hint names,
/// and the status of its package entry.
fn judged(
    report: &EnsureReport,
    runtime: &str,
    selected: &PackageDigest,
    release: &PackageDigest,
) -> String {
    let mut parts = Vec::new();
    for refusal in report.refusals.iter().filter(|r| r.subject == runtime) {
        let target = if refusal.detail.contains(release.as_str()) {
            "release"
        } else if refusal.detail.contains(selected.as_str()) {
            "selected"
        } else {
            "-"
        };
        parts.push(format!("refused {} ({target})", refusal.code));
    }
    for gap in report
        .gaps
        .iter()
        .filter(|g| g.runtime == runtime && g.code != CODE_GAP_CONFIG_HOME_MISSING)
    {
        let hint = [
            ("restore", gap.detail.contains("plugin install")),
            (
                "enable selected",
                gap.detail.contains(&enable_command(runtime, selected)),
            ),
            (
                "enable release",
                gap.detail.contains(&enable_command(runtime, release)),
            ),
            ("select", gap.detail.contains("plugin select")),
            ("update", gap.detail.contains("plugin update")),
            ("doctor", gap.detail.contains("plugin doctor")),
        ]
        .into_iter()
        .filter_map(|(name, named)| named.then_some(name))
        .collect::<Vec<_>>();
        let hint = if hint.is_empty() {
            "-".to_owned()
        } else {
            hint.join(", ")
        };
        parts.push(format!("gap {} ({hint})", gap.code));
    }
    for package in report.packages.iter().filter(|p| p.runtime == runtime) {
        let which = if package.digest == selected.as_str() {
            "selected"
        } else {
            "release"
        };
        parts.push(format!("package {:?} ({which})", package.status));
    }
    if parts.is_empty() {
        "nothing".to_owned()
    } else {
        parts.join("; ")
    }
}

/// How the selected version of a matrix row orders against the release's.
#[derive(Clone, Copy)]
enum Order {
    Older,
    Newer,
    Unordered,
}

/// What a matrix row does to the selected package once it is installed.
#[derive(Clone, Copy)]
enum SelectedState {
    Loads,
    RootMissing,
    RootAltered,
    Disabled,
    DisabledAltered,
    Unselected,
}

/// A selection state of the package, the state of the release's disabled
/// package beside it, and the line [`judged`] gives for it by a reserved
/// runtime's plan and apply, its `--detect` plan, and the same for Pi.
struct MatrixRow {
    label: &'static str,
    order: Order,
    state: SelectedState,
    release_root: ReleaseRoot,
    reserved: &'static str,
    reserved_detect: &'static str,
    pi: &'static str,
    pi_detect: &'static str,
}

const REFUSED_RELEASE: &str = "refused package_root_invalid (release)";
const REFUSED_SELECTED: &str = "refused package_root_invalid (selected)";
const REFUSED_NO_SERVER: &str = "refused release_runtime_catalog_unavailable (-)";
const KEPT: &str = "package Kept (selected)";
const GAP_ENABLE_SELECTED: &str = "gap official_package_not_serving (enable selected)";
const GAP_ENABLE_RELEASE: &str = "gap official_package_not_serving (enable release)";
const GAP_REPAIR_SELECTED: &str = "gap official_package_not_serving (restore, select)";
const GAP_RESTORE_ENABLE_RELEASE: &str =
    "gap official_package_not_serving (restore, enable release)";
const GAP_NOT_SELECTED: &str = "gap official_package_not_serving (update)";
const GAP_DOCTOR: &str = "gap official_package_not_serving (doctor)";
const GAP_UNORDERED: &str = "gap plugin_update_version_unordered (-); package Kept (selected)";
const GAP_KEPT_ENABLE_RELEASE: &str =
    "gap official_package_not_serving (enable release); package Kept (selected)";
const GAP_KEPT_RESTORE_ENABLE_RELEASE: &str =
    "gap official_package_not_serving (restore, enable release); package Kept (selected)";

/// The rows of the case table in `docs/public-api.md`, in its order.
const MATRIX: &[MatrixRow] = &[
    MatrixRow {
        label: "1 older, loads, release root altered",
        order: Order::Older,
        state: SelectedState::Loads,
        release_root: ReleaseRoot::Altered,
        reserved: REFUSED_RELEASE,
        reserved_detect: REFUSED_RELEASE,
        pi: REFUSED_RELEASE,
        pi_detect: REFUSED_RELEASE,
    },
    MatrixRow {
        label: "2 newer, loads, release root altered",
        order: Order::Newer,
        state: SelectedState::Loads,
        release_root: ReleaseRoot::Altered,
        reserved: REFUSED_RELEASE,
        reserved_detect: REFUSED_RELEASE,
        pi: KEPT,
        pi_detect: KEPT,
    },
    MatrixRow {
        label: "3 older, loads, release root intact",
        order: Order::Older,
        state: SelectedState::Loads,
        release_root: ReleaseRoot::Intact,
        reserved: GAP_KEPT_ENABLE_RELEASE,
        reserved_detect: GAP_KEPT_ENABLE_RELEASE,
        pi: GAP_KEPT_ENABLE_RELEASE,
        pi_detect: GAP_KEPT_ENABLE_RELEASE,
    },
    MatrixRow {
        label: "4 newer, loads, release root intact",
        order: Order::Newer,
        state: SelectedState::Loads,
        release_root: ReleaseRoot::Intact,
        reserved: GAP_KEPT_ENABLE_RELEASE,
        reserved_detect: GAP_KEPT_ENABLE_RELEASE,
        pi: KEPT,
        pi_detect: KEPT,
    },
    MatrixRow {
        label: "5 older, loads, release root missing",
        order: Order::Older,
        state: SelectedState::Loads,
        release_root: ReleaseRoot::Missing,
        reserved: GAP_KEPT_RESTORE_ENABLE_RELEASE,
        reserved_detect: GAP_KEPT_RESTORE_ENABLE_RELEASE,
        pi: GAP_KEPT_RESTORE_ENABLE_RELEASE,
        pi_detect: GAP_KEPT_RESTORE_ENABLE_RELEASE,
    },
    MatrixRow {
        label: "6 newer, loads, release root missing",
        order: Order::Newer,
        state: SelectedState::Loads,
        release_root: ReleaseRoot::Missing,
        reserved: GAP_KEPT_RESTORE_ENABLE_RELEASE,
        reserved_detect: GAP_KEPT_RESTORE_ENABLE_RELEASE,
        pi: KEPT,
        pi_detect: KEPT,
    },
    MatrixRow {
        label: "7 unordered, loads, release root altered",
        order: Order::Unordered,
        state: SelectedState::Loads,
        release_root: ReleaseRoot::Altered,
        reserved: GAP_UNORDERED,
        reserved_detect: GAP_UNORDERED,
        pi: GAP_UNORDERED,
        pi_detect: GAP_UNORDERED,
    },
    MatrixRow {
        label: "8 unordered, root missing, release root altered",
        order: Order::Unordered,
        state: SelectedState::RootMissing,
        release_root: ReleaseRoot::Altered,
        reserved: GAP_REPAIR_SELECTED,
        reserved_detect: GAP_REPAIR_SELECTED,
        pi: GAP_REPAIR_SELECTED,
        pi_detect: GAP_REPAIR_SELECTED,
    },
    MatrixRow {
        label: "9 unordered, root altered, release root altered",
        order: Order::Unordered,
        state: SelectedState::RootAltered,
        release_root: ReleaseRoot::Altered,
        reserved: REFUSED_SELECTED,
        reserved_detect: REFUSED_SELECTED,
        pi: REFUSED_SELECTED,
        pi_detect: REFUSED_SELECTED,
    },
    MatrixRow {
        label: "10 older, root missing, release root altered",
        order: Order::Older,
        state: SelectedState::RootMissing,
        release_root: ReleaseRoot::Altered,
        reserved: REFUSED_RELEASE,
        reserved_detect: REFUSED_RELEASE,
        pi: REFUSED_RELEASE,
        pi_detect: REFUSED_RELEASE,
    },
    MatrixRow {
        label: "11 older, root altered, release root altered",
        order: Order::Older,
        state: SelectedState::RootAltered,
        release_root: ReleaseRoot::Altered,
        reserved: REFUSED_RELEASE,
        reserved_detect: REFUSED_RELEASE,
        pi: REFUSED_RELEASE,
        pi_detect: REFUSED_RELEASE,
    },
    MatrixRow {
        label: "12 newer, root missing, release root altered",
        order: Order::Newer,
        state: SelectedState::RootMissing,
        release_root: ReleaseRoot::Altered,
        reserved: REFUSED_RELEASE,
        reserved_detect: REFUSED_RELEASE,
        pi: REFUSED_NO_SERVER,
        pi_detect: REFUSED_NO_SERVER,
    },
    MatrixRow {
        label: "13 newer, root altered, release root altered",
        order: Order::Newer,
        state: SelectedState::RootAltered,
        release_root: ReleaseRoot::Altered,
        reserved: REFUSED_RELEASE,
        reserved_detect: REFUSED_RELEASE,
        pi: REFUSED_NO_SERVER,
        pi_detect: REFUSED_NO_SERVER,
    },
    MatrixRow {
        label: "14 older, root missing, release root intact",
        order: Order::Older,
        state: SelectedState::RootMissing,
        release_root: ReleaseRoot::Intact,
        reserved: GAP_ENABLE_RELEASE,
        reserved_detect: GAP_ENABLE_RELEASE,
        pi: GAP_ENABLE_RELEASE,
        pi_detect: GAP_ENABLE_RELEASE,
    },
    MatrixRow {
        label: "15 older, root missing, release root missing",
        order: Order::Older,
        state: SelectedState::RootMissing,
        release_root: ReleaseRoot::Missing,
        reserved: GAP_RESTORE_ENABLE_RELEASE,
        reserved_detect: GAP_RESTORE_ENABLE_RELEASE,
        pi: GAP_RESTORE_ENABLE_RELEASE,
        pi_detect: GAP_RESTORE_ENABLE_RELEASE,
    },
    MatrixRow {
        label: "16 newer, root missing, release root intact",
        order: Order::Newer,
        state: SelectedState::RootMissing,
        release_root: ReleaseRoot::Intact,
        reserved: GAP_ENABLE_RELEASE,
        reserved_detect: GAP_ENABLE_RELEASE,
        pi: REFUSED_NO_SERVER,
        pi_detect: REFUSED_NO_SERVER,
    },
    MatrixRow {
        label: "17 newer, root missing, release root missing",
        order: Order::Newer,
        state: SelectedState::RootMissing,
        release_root: ReleaseRoot::Missing,
        reserved: GAP_RESTORE_ENABLE_RELEASE,
        reserved_detect: GAP_RESTORE_ENABLE_RELEASE,
        pi: REFUSED_NO_SERVER,
        pi_detect: REFUSED_NO_SERVER,
    },
    MatrixRow {
        label: "18 older, disabled, release root altered",
        order: Order::Older,
        state: SelectedState::Disabled,
        release_root: ReleaseRoot::Altered,
        reserved: GAP_ENABLE_SELECTED,
        reserved_detect: GAP_ENABLE_SELECTED,
        pi: GAP_ENABLE_SELECTED,
        pi_detect: GAP_ENABLE_SELECTED,
    },
    MatrixRow {
        label: "19 newer, disabled, release root altered",
        order: Order::Newer,
        state: SelectedState::Disabled,
        release_root: ReleaseRoot::Altered,
        reserved: GAP_ENABLE_SELECTED,
        reserved_detect: GAP_ENABLE_SELECTED,
        pi: GAP_ENABLE_SELECTED,
        pi_detect: GAP_ENABLE_SELECTED,
    },
    MatrixRow {
        label: "20 older, disabled, root altered, release root altered",
        order: Order::Older,
        state: SelectedState::DisabledAltered,
        release_root: ReleaseRoot::Altered,
        reserved: REFUSED_SELECTED,
        reserved_detect: REFUSED_SELECTED,
        pi: GAP_DOCTOR,
        pi_detect: REFUSED_SELECTED,
    },
    MatrixRow {
        label: "21 nothing selected, release root altered",
        order: Order::Older,
        state: SelectedState::Unselected,
        release_root: ReleaseRoot::Altered,
        reserved: REFUSED_RELEASE,
        reserved_detect: REFUSED_RELEASE,
        pi: GAP_NOT_SELECTED,
        pi_detect: REFUSED_RELEASE,
    },
    MatrixRow {
        label: "22 nothing selected, release root missing",
        order: Order::Older,
        state: SelectedState::Unselected,
        release_root: ReleaseRoot::Missing,
        reserved: GAP_RESTORE_ENABLE_RELEASE,
        reserved_detect: GAP_RESTORE_ENABLE_RELEASE,
        pi: GAP_NOT_SELECTED,
        pi_detect: GAP_RESTORE_ENABLE_RELEASE,
    },
    MatrixRow {
        label: "23 nothing selected, release root intact",
        order: Order::Older,
        state: SelectedState::Unselected,
        release_root: ReleaseRoot::Intact,
        reserved: GAP_ENABLE_RELEASE,
        reserved_detect: GAP_ENABLE_RELEASE,
        pi: GAP_NOT_SELECTED,
        pi_detect: GAP_ENABLE_RELEASE,
    },
];

/// Runs every [`MATRIX`] row for each of `runtimes`: the plan and the apply
/// must say `without_detect`'s line of the row, the `--detect` plan `with_detect`'s, and the
/// registry stays as staged and the profile is pinned only in the kept rows.
async fn assert_matrix(
    runtimes: &[&str],
    without_detect: fn(&MatrixRow) -> &'static str,
    with_detect: fn(&MatrixRow) -> &'static str,
) {
    for runtime in runtimes {
        for row in MATRIX {
            let release = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
            let version = match row.order {
                Order::Older => "1.0.1",
                Order::Newer => "2.1.0",
                Order::Unordered => "2.0",
            };
            let selected = official_runtimes::variant_archive(runtime, version);
            let host =
                host_beside_a_disabled_release(runtime, &selected, &release, row.release_root)
                    .await;
            match row.state {
                SelectedState::Loads => {}
                SelectedState::RootMissing => host.remove_package_root(selected.1.as_str()),
                SelectedState::RootAltered => host.alter_package_root(&selected.1),
                SelectedState::Disabled => host.disable_package(&selected.1),
                SelectedState::DisabledAltered => {
                    host.disable_package(&selected.1);
                    host.alter_package_root(&selected.1);
                }
                SelectedState::Unselected => {
                    let mut registry: serde_json::Value =
                        serde_json::from_slice(&fs::read(host.registry_path()).expect("registry"))
                            .expect("registry json");
                    registry["selected"] = serde_json::json!({});
                    fs::write(host.registry_path(), registry.to_string())
                        .expect("write the registry");
                }
            }
            let registry = fs::read(host.registry_path()).expect("the registry record");
            let profile = (*runtime != "pi").then(|| host.profile_text_of(runtime));
            let context = format!("{runtime} row {}", row.label);

            let plan = host.step(true);
            let detected = host.detect_step();
            let applied = host.step(false);

            for (mode, report, expected) in [
                ("plan", &plan, without_detect(row)),
                ("apply", &applied, without_detect(row)),
                ("detect", &detected, with_detect(row)),
            ] {
                assert_eq!(
                    judged(report, runtime, &selected.1, &release.1),
                    expected,
                    "{context} {mode}: {report:?}"
                );
            }
            assert_eq!(
                fs::read(host.registry_path()).expect("the registry record"),
                registry,
                "{context}"
            );
            if let Some(profile) = profile {
                // The apply pins an unpinned profile to the package it keeps
                // and leaves it alone whenever the runtime is left or refused.
                if without_detect(row).contains("package Kept") {
                    let pinned = host.profile_text_of(runtime);
                    assert!(pinned.contains(selected.1.as_str()), "{context}: {pinned}");
                    assert!(!pinned.contains(release.1.as_str()), "{context}: {pinned}");
                } else {
                    assert_eq!(host.profile_text_of(runtime), profile, "{context}");
                }
            }
            // A runtime nothing uses whose program is off the search path is
            // dropped by the detecting plan with its refusals and gaps. A
            // reserved runtime is used by its profile, which is removed here;
            // Pi is used by an enabled selection, so only the rows without
            // one are checked.
            let unused = *runtime != "pi"
                || !matches!(
                    row.state,
                    SelectedState::Loads | SelectedState::RootMissing | SelectedState::RootAltered
                );
            if unused {
                fs::remove_file(programs_dir(&host).join(runtime)).expect("remove the program");
                if *runtime != "pi" {
                    fs::remove_file(host.profiles_dir().join(format!("{runtime}.toml")))
                        .expect("remove the profile");
                }
                assert_eq!(
                    judged(&host.detect_step(), runtime, &selected.1, &release.1),
                    "gap agent_not_on_search_path (-)",
                    "{context} unused and off the search path"
                );
            }
        }
    }
}

/// The case table of the reserved runtime `claude`, whose profile uses it:
/// what the plan, the apply and the `--detect` plan say for each state of the
/// selected package beside the release's disabled one.
#[tokio::test]
async fn claude_judges_the_disabled_release_package_by_the_selection_state() {
    assert_matrix(&["claude"], |row| row.reserved, |row| row.reserved_detect).await;
}

/// The case table of the reserved runtime `codex`.
#[tokio::test]
async fn codex_judges_the_disabled_release_package_by_the_selection_state() {
    assert_matrix(&["codex"], |row| row.reserved, |row| row.reserved_detect).await;
}

/// The case table of the reserved runtime `hermes`.
#[tokio::test]
async fn hermes_judges_the_disabled_release_package_by_the_selection_state() {
    assert_matrix(&["hermes"], |row| row.reserved, |row| row.reserved_detect).await;
}

/// The case table of Pi, which the upgrade plans only while an enabled
/// selection serves it or `--detect` lists it.
#[tokio::test]
async fn pi_judges_the_disabled_release_package_by_the_selection_state() {
    assert_matrix(&["pi"], |row| row.pi, |row| row.pi_detect).await;
}

/// Detail of the claim refusal of a disabled package the upgrade cannot enable.
const DISABLED_CLAIM_DETAIL: &str =
    "the disabled package cannot be enabled while another package serves this runtime id";

/// A Pi selection that cannot load and is older than the release's, beside the
/// release's own package recorded disabled with its root intact, while a
/// third-party package serves `pi`: enabling the release's package would fail,
/// so the plan and the apply refuse it with the runtime-conflict code and
/// change nothing, and the `--detect` plan reports the claim as a gap.
#[tokio::test]
async fn a_disabled_release_package_that_another_package_serves_is_refused_beside_an_unloadable_selection(
) {
    let release = official_runtimes::variant_archive("pi", RELEASE_VERSION);
    let selected = official_runtimes::variant_archive("pi", "1.0.1");
    let host = host_beside_a_disabled_release("pi", &selected, &release, ReleaseRoot::Intact).await;
    host.remove_package_root(selected.1.as_str());
    official_runtimes::preinstall_third_party(
        &host.plugins_dir(),
        host.env.root(),
        "pi",
        "acme.runtime.pi",
    )
    .await;
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let conflict = ProtocolError::from(PackageErrorKind::RuntimeConflict).code;

    let plan = host.step(true);
    let applied = host.step(false);
    let detected = host.detect_step();

    for report in [&plan, &applied] {
        assert_eq!(
            refusal_codes(report),
            [conflict.as_str()],
            "{:?}",
            report.refusals
        );
        assert_eq!(report.refusals[0].subject, "pi");
        assert_eq!(report.refusals[0].detail, DISABLED_CLAIM_DETAIL);
        assert!(
            !gap_codes(report).contains(&("pi", CODE_GAP_SERVED_BY_OTHER_PACKAGE)),
            "{:?}",
            report.gaps
        );
        assert!(report.packages.is_empty(), "{:?}", report.packages);
    }
    assert!(detected.refusals.is_empty(), "{:?}", detected.refusals);
    let gap = detected
        .gaps
        .iter()
        .find(|gap| gap.runtime == "pi" && gap.code == CODE_GAP_SERVED_BY_OTHER_PACKAGE)
        .unwrap_or_else(|| panic!("{:?}", detected.gaps));
    assert_eq!(gap.detail, DISABLED_CLAIM_DETAIL);
    assert!(
        !gap_codes(&detected).contains(&("pi", CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)),
        "{:?}",
        detected.gaps
    );
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
}

/// The program the release's archive declares instead of the installed one: the
/// same package id and version with another digest.
const SAME_VERSION_PROGRAM: &str = "same-version-alt";

/// The uninstall command a same-version gap hint names for the holder `digest`.
fn uninstall_command(runtime: &str, digest: &PackageDigest) -> String {
    let hex = digest
        .as_str()
        .strip_prefix("sha256:")
        .expect("a sha256 digest");
    format!("pohunek plugin uninstall pohunek.runtime.{runtime} --digest {hex} --yes")
}

/// What a same-version gap detail names.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hint<'a> {
    /// The uninstall command of the holder.
    Uninstall(&'a PackageDigest),
    /// No command: the fix is a release with a new version.
    NewVersion,
}

/// Asserts the same-version gap `detail` of `runtime` names `hint`.
fn assert_same_version_hint(detail: &str, runtime: &str, hint: Hint<'_>) {
    match hint {
        Hint::Uninstall(holder) => assert!(
            detail.contains(&uninstall_command(runtime, holder)),
            "{runtime}: {detail}"
        ),
        Hint::NewVersion => {
            assert!(!detail.contains("`pohunek"), "{runtime}: {detail}");
            assert!(detail.contains("a new version"), "{runtime}: {detail}");
        }
    }
}

/// Asserts that the plan and the apply of `host` keep the selection `serving`
/// of `runtime` (release version `version`) beside the release package under
/// another digest of the same version: status `kept`, one gap with `hint`, no
/// refusal, no `replaces_newer`, and the registry and the profile unchanged.
fn assert_same_version_kept(
    host: &Host,
    runtime: &str,
    serving: &PackageDigest,
    version: &str,
    hint: Hint<'_>,
) {
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profile = fs::read_to_string(host.profiles_dir().join(format!("{runtime}.toml"))).ok();

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(
            report.refusals.is_empty(),
            "{runtime}: {:?}",
            report.refusals
        );
        let package = report
            .packages
            .iter()
            .find(|package| package.runtime == runtime)
            .unwrap_or_else(|| panic!("{runtime}: {:?}", report.packages));
        assert_eq!(package.status, PackageStatus::Kept, "{runtime}");
        assert_eq!(package.digest, serving.as_str(), "{runtime}");
        assert_eq!(
            package.release_version.as_deref(),
            Some(version),
            "{runtime}"
        );
        assert_eq!(package.replaces_newer, None, "{runtime}");
        assert!(
            report.profiles.is_empty(),
            "{runtime}: {:?}",
            report.profiles
        );
        let gaps: Vec<_> = report
            .gaps
            .iter()
            .filter(|gap| gap.runtime == runtime)
            .collect();
        assert_eq!(gaps.len(), 1, "{runtime}: {gaps:?}");
        assert_eq!(gaps[0].code, CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST);
        assert_same_version_hint(&gaps[0].detail, runtime, hint);
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry,
        "{runtime}"
    );
    assert_eq!(
        fs::read_to_string(host.profiles_dir().join(format!("{runtime}.toml"))).ok(),
        profile,
        "{runtime}"
    );
}

/// The enabled selection of a reserved runtime has the release's version under
/// another digest, and the profile the earlier upgrade bound pins it: the plan
/// and the apply keep it with a gap that names no command, because no command
/// releases the profile's pin.
#[test]
fn a_pinned_selection_of_the_release_version_under_another_digest_is_kept_without_a_command() {
    for runtime in ["claude", "codex", "hermes"] {
        let installed = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
        let release = official_runtimes::variant_archive_with_program(
            runtime,
            RELEASE_VERSION,
            SAME_VERSION_PROGRAM,
        );
        let host = Host::new();
        host.publish_archives(1, &[(runtime, &installed.0, &installed.1)]);
        host.write_profile(runtime, &format!("base = \"{runtime}\"\n"));
        let first = host.step(false);
        assert!(first.refusals.is_empty(), "{runtime}: {first:?}");
        host.publish_archives(2, &[(runtime, &release.0, &release.1)]);

        assert_same_version_kept(
            &host,
            runtime,
            &installed.1,
            RELEASE_VERSION,
            Hint::NewVersion,
        );
    }
}

/// The same for Pi, whose selection a newer release would update: no command,
/// and the detail says the upgrade does not install a removed Pi package again.
#[tokio::test]
async fn an_enabled_pi_selection_of_the_release_version_under_another_digest_is_kept_with_a_gap() {
    let host = Host::with_installed_pi().await;
    let installed = Host::installed_pi_digest();
    let (bytes, digest) = official_runtimes::variant_archive_with_program(
        "pi",
        PI_INSTALLED_VERSION,
        SAME_VERSION_PROGRAM,
    );
    host.publish_archives(1, &[("pi", &bytes, &digest)]);
    host.write_profile(
        "work",
        &format!("base = \"pi\"\n{}", official_runtimes::profile_pin("pi")),
    );
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profile = host.profile_text_of("work");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        let pi = pi_package(report);
        assert_eq!(pi.status, PackageStatus::Kept);
        assert_eq!(pi.digest, installed.as_str());
        assert_eq!(pi.release_version.as_deref(), Some(PI_INSTALLED_VERSION));
        let gaps: Vec<_> = report
            .gaps
            .iter()
            .filter(|gap| gap.runtime == "pi")
            .collect();
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].code, CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST);
        assert_same_version_hint(&gaps[0].detail, "pi", Hint::NewVersion);
        assert!(
            gaps[0]
                .detail
                .contains("does not install a Pi package again"),
            "{}",
            gaps[0].detail
        );
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
    assert_eq!(host.profile_text_of("work"), profile);
}

/// A newer Pi selection beside a recorded holder of the release's version is
/// kept silently, as beside any other release package it would replace.
#[tokio::test]
async fn a_newer_pi_selection_beside_a_holder_of_the_release_version_is_kept_silently() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    let updated = host.step(false);
    assert!(updated.refusals.is_empty(), "{:?}", updated.refusals);
    let newer = official_runtimes::variant_archive("pi", PI_NEWER_VERSION).1;
    let (bytes, digest) = official_runtimes::variant_archive_with_program(
        "pi",
        PI_INSTALLED_VERSION,
        SAME_VERSION_PROGRAM,
    );
    host.publish_archives(2, &[("pi", &bytes, &digest)]);
    let registry = fs::read(host.registry_path()).expect("the registry record");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        let pi = pi_package(report);
        assert_eq!(pi.status, PackageStatus::Kept);
        assert_eq!(pi.digest, newer.as_str());
        assert!(
            report.gaps.iter().all(|gap| gap.runtime != "pi"),
            "{:?}",
            report.gaps
        );
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
}

/// A version older or newer than the release's is selected and the release's
/// version is recorded under another digest, unselected, enabled or disabled:
/// the selection is kept without a replacement warning, as the release's archive
/// cannot be installed beside the holder.
#[test]
fn a_recorded_holder_of_the_release_version_beside_another_selection_keeps_the_selection() {
    for (label, version) in [("older", "1.0.1"), ("newer", "2.1.0")] {
        for disable_holder in [false, true] {
            let runtime = "claude";
            let selected = official_runtimes::variant_archive(runtime, version);
            let holder = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
            let release = official_runtimes::variant_archive_with_program(
                runtime,
                RELEASE_VERSION,
                SAME_VERSION_PROGRAM,
            );
            let host = Host::new();
            host.write_profile(runtime, "base = \"claude\"\n");
            host.publish_archives(1, &[(runtime, &selected.0, &selected.1)]);
            assert!(host.step(false).refusals.is_empty(), "{label}");
            host.publish_archives(2, &[(runtime, &holder.0, &holder.1)]);
            assert!(host.step(false).refusals.is_empty(), "{label}");
            pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
                .expect("the package store")
                .registry()
                .select(&selected.1)
                .expect("select the other version");
            if disable_holder {
                host.disable_package(&holder.1);
            }
            host.write_claude_hook();
            host.publish_archives(3, &[(runtime, &release.0, &release.1)]);

            assert_same_version_kept(
                &host,
                runtime,
                &selected.1,
                RELEASE_VERSION,
                Hint::NewVersion,
            );
        }
    }
}

/// No profile or session pins the holder: the gap names the uninstall, and the
/// whole sequence works. The registry refuses the uninstall while the holder is
/// retained, the uninstall succeeds without it, and the next apply installs the
/// release's archive. The detecting plan reports the same gap.
#[test]
fn an_unpinned_holder_gets_the_uninstall_hint_and_the_next_apply_installs_the_release() {
    let runtime = "claude";
    let installed = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
    let release = official_runtimes::variant_archive_with_program(
        runtime,
        RELEASE_VERSION,
        SAME_VERSION_PROGRAM,
    );
    let host = Host::new();
    host.write_claude_hook();
    host.publish_archives(1, &[(runtime, &installed.0, &installed.1)]);
    let first = host.step(false);
    assert!(first.refusals.is_empty(), "{first:?}");
    assert!(
        first.profiles.is_empty(),
        "no profile exists to pin the package: {:?}",
        first.profiles
    );
    host.publish_archives(2, &[(runtime, &release.0, &release.1)]);

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    let gap = plan
        .gaps
        .iter()
        .find(|gap| gap.runtime == runtime)
        .expect("the same-version gap");
    assert_eq!(gap.code, CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST);
    assert_same_version_hint(&gap.detail, runtime, Hint::Uninstall(&installed.1));

    host.put_program(runtime);
    let detected = host.detect_step();
    assert!(
        detected
            .gaps
            .iter()
            .any(|gap| gap.runtime == runtime
                && gap.code == CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST),
        "{:?}",
        detected.gaps
    );

    let store = pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
        .expect("the package store");
    let registry = store.registry();
    let mut retained = package::registry::RetainedDigests::new();
    retained.insert(installed.1.clone());
    assert!(matches!(
        registry.uninstall(&installed.1, &retained),
        Err(package::registry::RegistryError::StillReferenced)
    ));
    registry
        .uninstall(&installed.1, &package::registry::RetainedDigests::new())
        .expect("uninstall the holder");
    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    let package = applied
        .packages
        .iter()
        .find(|package| package.runtime == runtime)
        .expect("the runtime package");
    assert_eq!(package.status, PackageStatus::Installed);
    assert_eq!(package.digest, release.1.as_str());
}

/// An unpinned profile over the runtime is bound to the kept package by the same
/// apply, so the holder is pinned afterwards: the plan and the apply name no
/// command, and their details are the same.
#[test]
fn an_unpinned_profile_that_the_apply_binds_to_the_holder_withdraws_the_uninstall_hint() {
    let runtime = "claude";
    let installed = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
    let release = official_runtimes::variant_archive_with_program(
        runtime,
        RELEASE_VERSION,
        SAME_VERSION_PROGRAM,
    );
    let host = Host::new();
    host.write_claude_hook();
    host.publish_archives(1, &[(runtime, &installed.0, &installed.1)]);
    assert!(host.step(false).refusals.is_empty());
    host.write_profile(runtime, "base = \"claude\"\n");
    host.publish_archives(2, &[(runtime, &release.0, &release.1)]);

    let plan = host.step(true);
    let applied = host.step(false);

    let details: Vec<&str> = [&plan, &applied]
        .iter()
        .map(|report| {
            let gap = report
                .gaps
                .iter()
                .find(|gap| gap.runtime == runtime)
                .expect("the same-version gap");
            assert_eq!(gap.code, CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST);
            assert_same_version_hint(&gap.detail, runtime, Hint::NewVersion);
            gap.detail.as_str()
        })
        .collect();
    assert_eq!(details[0], details[1]);
}

/// A reserved runtime planned only by `--detect` is not planned by the plain
/// upgrade, so the detecting plan names no uninstall command for it.
#[test]
fn a_runtime_planned_only_by_detection_gets_no_uninstall_hint() {
    let runtime = "claude";
    let installed = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
    let release = official_runtimes::variant_archive_with_program(
        runtime,
        RELEASE_VERSION,
        SAME_VERSION_PROGRAM,
    );
    let host = Host::new();
    host.write_claude_hook();
    host.publish_archives(1, &[(runtime, &installed.0, &installed.1)]);
    assert!(host.step(false).refusals.is_empty());
    host.remove_claude_hook();
    host.publish_archives(2, &[(runtime, &release.0, &release.1)]);
    host.put_program(runtime);

    let detected = host.detect_step();

    let gap = detected
        .gaps
        .iter()
        .find(|gap| gap.runtime == runtime && gap.code == CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST)
        .unwrap_or_else(|| panic!("{:?}", detected.gaps));
    assert_same_version_hint(&gap.detail, runtime, Hint::NewVersion);
}

/// A durable session record pins the holder: the plan and the apply name no
/// uninstall command, because the daemon's uninstall retains the record's digest.
#[test]
fn a_session_record_that_pins_the_holder_withdraws_the_uninstall_hint() {
    let runtime = "claude";
    let installed = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
    let release = official_runtimes::variant_archive_with_program(
        runtime,
        RELEASE_VERSION,
        SAME_VERSION_PROGRAM,
    );
    let host = Host::new();
    host.write_claude_hook();
    host.publish_archives(1, &[(runtime, &installed.0, &installed.1)]);
    assert!(host.step(false).refusals.is_empty());
    host.write_store(
        &PACKAGE_SESSION_STORE.replace(&format!("sha256:{}", "a".repeat(64)), installed.1.as_str()),
    );
    host.publish_archives(2, &[(runtime, &release.0, &release.1)]);

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        let gap = report
            .gaps
            .iter()
            .find(|gap| gap.runtime == runtime)
            .expect("the same-version gap");
        assert_eq!(gap.code, CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST);
        assert_same_version_hint(&gap.detail, runtime, Hint::NewVersion);
    }
}

/// A host staged for the holder table: each of `installs` (archive, enabled,
/// selected) is recorded through the catalog in order, and `release` (the
/// holders' package id and version under another digest) is published. A
/// reserved runtime has a profile over it, and every runtime has its program on
/// the search path.
async fn host_with_holder(
    runtime: &str,
    installs: &[(&Archive, bool, bool)],
    release: &Archive,
) -> Host {
    let host = Host::new();
    let plugins = host.plugins_dir();
    let scratch = host.env.root().to_path_buf();
    for (index, (archive, enable, select)) in installs.iter().enumerate() {
        let flags = official_runtimes::InstallFlags {
            enable: *enable,
            select: *select,
        };
        host.catalog
            .install_archive(
                &plugins,
                &scratch,
                runtime,
                archive,
                u64::try_from(index + 1).expect("a small index"),
                flags,
            )
            .await;
    }
    host.publish_archives(
        STAGED_RELEASE_SEQUENCE,
        &[(runtime, &release.0, &release.1)],
    );
    if runtime != "pi" {
        host.write_profile(runtime, &format!("base = \"{runtime}\"\n"));
    }
    host.put_program(runtime);
    host.put_program(SAME_VERSION_PROGRAM);
    host
}

/// The release archive of `runtime` with the holders' version and another digest.
fn holder_release(runtime: &str) -> Archive {
    official_runtimes::variant_archive_with_program(runtime, RELEASE_VERSION, SAME_VERSION_PROGRAM)
}

/// Rewrites the source of the recorded package `digest`, as a registry edited by
/// hand would carry it.
fn set_package_source(host: &Host, digest: &PackageDigest, source: &str) {
    let mut registry: serde_json::Value =
        serde_json::from_slice(&fs::read(host.registry_path()).expect("registry"))
            .expect("registry json");
    for record in registry["packages"].as_array_mut().expect("packages") {
        if record["digest"] == digest.as_str() {
            record["source"] = serde_json::Value::String(source.to_owned());
        }
    }
    fs::write(host.registry_path(), registry.to_string()).expect("write the registry");
}

/// Row 8: a newer Pi selection whose root is missing beside a holder keeps its
/// existing refusal, in the plan and in the apply.
#[tokio::test]
async fn a_newer_pi_selection_without_its_root_beside_a_holder_keeps_its_refusal() {
    let selected = official_runtimes::variant_archive("pi", PI_NEWER_VERSION);
    let holder = official_runtimes::variant_archive("pi", RELEASE_VERSION);
    let release = holder_release("pi");
    let host = host_with_holder(
        "pi",
        &[(&selected, true, true), (&holder, true, false)],
        &release,
    )
    .await;
    host.remove_package_root(selected.1.as_str());
    let registry = fs::read(host.registry_path()).expect("the registry record");

    for program in ["pi", SAME_VERSION_PROGRAM] {
        fs::remove_file(programs_dir(&host).join(program)).expect("remove the program");
    }
    for report in [host.step(true), host.step(false), host.detect_step()] {
        assert_eq!(
            refusal_codes(&report),
            [CODE_RELEASE_CATALOG_UNAVAILABLE],
            "{:?}",
            report.refusals
        );
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
}

/// A detecting plan drops a runtime nothing uses whose program is off the search
/// path before it judges the holder, as it does for the other left states.
#[tokio::test]
async fn a_detecting_plan_drops_an_unused_runtime_before_judging_its_holder() {
    for state in [
        "no selection",
        "root missing",
        "other root missing",
        "root altered",
    ] {
        let runtime = "codex";
        let selected = official_runtimes::variant_archive(runtime, "1.0.1");
        let holder = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
        let release = holder_release(runtime);
        let installs: Vec<(&Archive, bool, bool)> = match state {
            "no selection" => vec![(&holder, true, false)],
            "root missing" | "root altered" => vec![(&holder, true, true)],
            _ => vec![(&selected, true, true), (&holder, true, false)],
        };
        let host = host_with_holder(runtime, &installs, &release).await;
        match state {
            "root missing" => host.remove_package_root(holder.1.as_str()),
            "other root missing" => host.remove_package_root(selected.1.as_str()),
            "root altered" => host.alter_package_root(&holder.1),
            _ => {}
        }
        fs::remove_file(host.profiles_dir().join(format!("{runtime}.toml")))
            .expect("remove the profile");
        for program in [runtime, SAME_VERSION_PROGRAM] {
            fs::remove_file(programs_dir(&host).join(program)).expect("remove the program");
        }

        let detected = host.detect_step();

        assert!(
            detected.refusals.is_empty(),
            "{state}: {:?}",
            detected.refusals
        );
        assert!(
            detected
                .packages
                .iter()
                .all(|package| package.runtime != runtime),
            "{state}: {:?}",
            detected.packages
        );
        assert!(
            !gap_codes(&detected).contains(&(runtime, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)),
            "{state}: {:?}",
            detected.gaps
        );
    }
}

/// A host profile pins another installed, older version P beside the selected
/// holder. The plan keeps the holder and names its uninstall, because only P is
/// retained; the registry accepts that uninstall with the plan's retained set,
/// and the next apply installs the release archive and repins the profile from
/// P, judging the retained P against the release package.
#[tokio::test]
async fn a_profile_pinned_to_an_older_version_beside_the_holder_gets_the_whole_remedy() {
    for runtime in ["claude", "codex", "hermes"] {
        let older = official_runtimes::variant_archive(runtime, "1.0.1");
        let holder = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
        let release = holder_release(runtime);
        let host = host_with_holder(
            runtime,
            &[(&older, true, true), (&holder, true, true)],
            &release,
        )
        .await;
        host.write_profile(
            runtime,
            &format!(
                "base = \"{runtime}\"\npackage = \"pohunek.runtime.{runtime}\"\ndigest = \"{}\"\n",
                older.1.as_str()
            ),
        );
        let profile = host.profile_text_of(runtime);

        let plan = host.step(true);
        let applied = host.step(false);

        for report in [&plan, &applied] {
            assert!(
                report.refusals.is_empty(),
                "{runtime}: {:?}",
                report.refusals
            );
            let gap = report
                .gaps
                .iter()
                .find(|gap| gap.runtime == runtime)
                .unwrap_or_else(|| panic!("{runtime}: {:?}", report.gaps));
            assert_eq!(gap.code, CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST);
            assert_same_version_hint(&gap.detail, runtime, Hint::Uninstall(&holder.1));
        }
        assert_eq!(host.profile_text_of(runtime), profile, "{runtime}");

        let mut retained = package::registry::RetainedDigests::new();
        retained.insert(older.1.clone());
        pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
            .expect("the package store")
            .registry()
            .uninstall(&holder.1, &retained)
            .expect("the registry accepts the hinted uninstall");
        let next = host.step(false);

        assert!(next.refusals.is_empty(), "{runtime}: {:?}", next.refusals);
        let package = next
            .packages
            .iter()
            .find(|package| package.runtime == runtime)
            .unwrap_or_else(|| panic!("{runtime}: {:?}", next.packages));
        assert_eq!(package.status, PackageStatus::Installed, "{runtime}");
        assert_eq!(package.digest, release.1.as_str(), "{runtime}");
        let repinned = &next.profiles[0];
        assert_eq!(repinned.status, ProfileStatus::Repinned, "{runtime}");
        assert_eq!(repinned.previous_digest.as_deref(), Some(older.1.as_str()));
        assert_eq!(repinned.digest, release.1.as_str(), "{runtime}");
        assert!(
            host.profile_text_of(runtime).contains(release.1.as_str()),
            "{runtime}"
        );
    }
}

/// A reserved runtime that only `--detect` plans is not planned by the plain
/// upgrade, so the detecting plan names no uninstall command for the holder.
#[tokio::test]
async fn a_runtime_planned_only_by_detection_beside_a_holder_gets_no_uninstall_command() {
    let runtime = "claude";
    let holder = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
    let release = holder_release(runtime);
    let host = host_with_holder(runtime, &[(&holder, true, false)], &release).await;
    fs::remove_file(host.profiles_dir().join(format!("{runtime}.toml")))
        .expect("remove the profile");

    let detected = host.detect_step();

    let gap = detected
        .gaps
        .iter()
        .find(|gap| gap.runtime == runtime && gap.code == CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)
        .unwrap_or_else(|| panic!("{:?}", detected.gaps));
    assert!(!gap.detail.contains("`pohunek"), "{}", gap.detail);
    assert!(
        gap.detail.contains("a release that gives the package"),
        "{}",
        gap.detail
    );
}

/// Runs the select the gap names: the registry selects the holder, and the next
/// plan keeps it as the serving selection.
fn assert_select_hint_makes_the_holder_serve(host: &Host, runtime: &str, holder: &PackageDigest) {
    pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
        .expect("the package store")
        .registry()
        .select(holder)
        .expect("the registry accepts the hinted select");
    let plan = host.step(true);
    assert!(plan.refusals.is_empty(), "{runtime}: {:?}", plan.refusals);
    let package = plan
        .packages
        .iter()
        .find(|package| package.runtime == runtime)
        .unwrap_or_else(|| panic!("{runtime}: {:?}", plan.packages));
    assert_eq!(package.status, PackageStatus::Kept, "{runtime}");
    assert_eq!(package.digest, holder.as_str(), "{runtime}");
}

/// Every host profile with its text, sorted by name.
fn profiles_snapshot(host: &Host) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = fs::read_dir(host.profiles_dir())
        .expect("the profiles directory")
        .map(|entry| {
            let entry = entry.expect("a profile entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read_to_string(entry.path()).expect("the profile text"),
            )
        })
        .collect();
    all.sort();
    all
}

/// The gap detail of `runtime` in `report`, when it has one
/// `official_package_not_serving` gap.
fn not_serving_detail(report: &EnsureReport, runtime: &str) -> Option<String> {
    let gaps: Vec<_> = report
        .gaps
        .iter()
        .filter(|gap| gap.runtime == runtime && gap.code == CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)
        .collect();
    (gaps.len() == 1).then(|| gaps[0].detail.clone())
}

/// What the plan, the apply and the detecting plan of `host` say for `runtime`
/// when the holder leaves it untouched: no refusal, no package entry, no profile
/// action, one `official_package_not_serving` gap whose detail is the same in
/// all three and contains every text of `details`, and the registry and the
/// profiles unchanged. Returns the gap detail.
fn assert_held_runtime_left(host: &Host, runtime: &str, details: &[&str]) -> String {
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profiles = profiles_snapshot(host);
    let plan = host.step(true);
    let applied = host.step(false);
    let detected = host.detect_step();
    let mut seen = Vec::new();
    for (label, report) in [("plan", &plan), ("apply", &applied), ("detect", &detected)] {
        assert!(
            report.refusals.is_empty(),
            "{runtime} {label}: {:?}",
            report.refusals
        );
        assert!(
            report
                .packages
                .iter()
                .all(|package| package.runtime != runtime),
            "{runtime} {label}: {:?}",
            report.packages
        );
        assert!(report.profiles.is_empty(), "{runtime} {label}");
        let detail = not_serving_detail(report, runtime)
            .unwrap_or_else(|| panic!("{runtime} {label}: {:?}", report.gaps));
        for text in details {
            assert!(detail.contains(text), "{runtime} {label}: {detail}");
        }
        seen.push(detail);
    }
    assert!(
        seen.iter().all(|detail| *detail == seen[0]),
        "{runtime}: the three modes say the same: {seen:?}"
    );
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry,
        "{runtime}"
    );
    assert_eq!(profiles_snapshot(host), profiles, "{runtime}");
    seen.swap_remove(0)
}

/// What the plan, the apply and the detecting plan of `host` say for `runtime`
/// when a root-invalid refusal stops it: exactly that refusal with the same
/// detail in all three, containing `hint`, no package entry or profile action,
/// and the registry and the profiles unchanged. Returns the detail.
fn assert_held_runtime_refused(host: &Host, runtime: &str, hint: &str) -> String {
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profiles = profiles_snapshot(host);
    let root_refusal = ProtocolError::from(PackageErrorKind::RootInvalid).code;
    let mut seen = Vec::new();
    for (label, report) in [
        ("plan", host.step(true)),
        ("apply", host.step(false)),
        ("detect", host.detect_step()),
    ] {
        assert_eq!(
            refusal_codes(&report),
            [root_refusal.as_str()],
            "{runtime} {label}"
        );
        assert!(
            report.refusals[0].detail.contains(hint),
            "{runtime} {label}: {}",
            report.refusals[0].detail
        );
        assert!(
            report
                .packages
                .iter()
                .all(|package| package.runtime != runtime),
            "{runtime} {label}: {:?}",
            report.packages
        );
        assert!(report.profiles.is_empty(), "{runtime} {label}");
        seen.push(report.refusals[0].detail.clone());
    }
    assert!(
        seen.iter().all(|detail| *detail == seen[0]),
        "{runtime}: the three modes say the same: {seen:?}"
    );
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry,
        "{runtime}"
    );
    assert_eq!(profiles_snapshot(host), profiles, "{runtime}");
    seen.swap_remove(0)
}

/// The archive a cell installs for a role: the holder has the release's version
/// under another digest, the others are older or newer versions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Holder,
    Older,
    Newer,
}

/// What a cell does to the root or the record of a package.
#[derive(Clone, Copy)]
enum Damage {
    Missing,
    Altered,
    NonOfficial,
}

/// What pins the package of a cell.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PinKind {
    Profile,
    Session,
    /// A host profile and a session record pin it together.
    Both,
    /// A host profile with only a `digest` line pins it.
    DigestOnly,
    /// Two host profiles pin it.
    TwoProfiles,
}

/// One step of a remedy: the release of a pin, or a command the hint names.
#[derive(Clone, Copy)]
enum Cmd {
    Release,
    Uninstall(Role),
    RemoveModified(Role),
    Install,
    Update,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Gap,
    Refusal,
    /// A Pi without an enabled official selection whose holder is altered: a gap
    /// in plan and apply, the refusal under `--detect`.
    GapThenRefusal,
}

/// Where a remedy ends.
#[derive(Clone, Copy)]
enum End {
    /// The next apply installs the release archive.
    Installed,
    /// The next plain step finds Pi at the release digest.
    Present,
    /// The removal is accepted; the runtime is no longer used.
    Removed,
    /// The next plan names the next step of the same-version holder.
    Chain,
}

/// What the plan and the apply (a gap) and the detecting plan (the root-invalid
/// refusal, which supersedes the managed-runtime gap) say for an unmanaged
/// `runtime` whose unselected holder is altered: the same detail in all three,
/// and the registry and the profiles unchanged. Returns the detail.
fn assert_held_pi_altered(host: &Host, runtime: &str) -> String {
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let profiles = profiles_snapshot(host);
    let root_refusal = ProtocolError::from(PackageErrorKind::RootInvalid).code;
    let mut seen = Vec::new();
    for report in [host.step(true), host.step(false)] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        seen.push(not_serving_detail(&report, runtime).expect("the gap"));
    }
    let detected = host.detect_step();
    assert_eq!(refusal_codes(&detected), [root_refusal.as_str()]);
    assert_eq!(not_serving_detail(&detected, runtime), None);
    seen.push(detected.refusals[0].detail.clone());
    assert!(
        seen.iter().all(|detail| *detail == seen[0]),
        "the three modes say the same: {seen:?}"
    );
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
    assert_eq!(profiles_snapshot(host), profiles);
    seen.swap_remove(0)
}

const RESERVED: &[&str] = &["claude", "codex", "hermes"];
const CLAUDE: &[&str] = &["claude"];
const PI: &[&str] = &["pi"];

/// One cell of the holder contract table: a state beside the holder, the outcome
/// in plan, apply and `--detect`, and the remedy the hint names.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independent use of the runtime in the table"
)]
struct Cell {
    id: &'static str,
    runtimes: &'static [&'static str],
    /// (role, enabled, selected), installed in order through the catalog.
    installs: &'static [(Role, bool, bool)],
    damage: &'static [(Role, Damage)],
    pin: Option<(Role, PinKind)>,
    /// A second session record pins another digest; the value is the runtime
    /// it launched.
    other_session: Option<&'static str>,
    /// A host profile uses the runtime.
    profile: bool,
    /// An installed hook uses the runtime.
    hook: bool,
    /// A session record that pins no package uses the runtime.
    store: bool,
    outcome: Outcome,
    steps: &'static [Cmd],
    end: End,
    /// Proves the select the hint names instead of the removal remedy.
    select: bool,
}

const BASE: Cell = Cell {
    id: "",
    runtimes: RESERVED,
    installs: &[],
    damage: &[],
    pin: None,
    other_session: None,
    profile: true,
    hook: false,
    store: false,
    outcome: Outcome::Gap,
    steps: &[],
    end: End::Installed,
    select: false,
};

use Cmd::{Install, Release, RemoveModified, Uninstall, Update};
use Role::{Holder, Newer, Older};

const CELLS: &[Cell] = &[
    Cell {
        id: "R1 nothing selected",
        installs: &[(Holder, true, false)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R1 holder disabled",
        installs: &[(Holder, false, false)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R1 holder root missing",
        installs: &[(Holder, true, false)],
        damage: &[(Holder, Damage::Missing)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R1 hook only",
        runtimes: CLAUDE,
        profile: false,
        hook: true,
        installs: &[(Holder, true, false)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R1 session only",
        runtimes: CLAUDE,
        profile: false,
        store: true,
        installs: &[(Holder, true, false)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R2 altered, nothing selected",
        installs: &[(Holder, true, false)],
        damage: &[(Holder, Damage::Altered)],
        outcome: Outcome::Refusal,
        steps: &[RemoveModified(Holder)],
        ..BASE
    },
    Cell {
        id: "R2 altered, holder disabled",
        installs: &[(Holder, false, false)],
        damage: &[(Holder, Damage::Altered)],
        outcome: Outcome::Refusal,
        steps: &[RemoveModified(Holder)],
        ..BASE
    },
    Cell {
        id: "R3 selected, root missing",
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R4 selected, altered",
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Altered)],
        outcome: Outcome::Refusal,
        steps: &[RemoveModified(Holder)],
        ..BASE
    },
    Cell {
        id: "R5 selected holder pinned by a profile",
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        pin: Some((Holder, PinKind::Profile)),
        steps: &[Release, Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R5 holder pinned by a profile and a session together",
        runtimes: CLAUDE,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        pin: Some((Holder, PinKind::Both)),
        steps: &[Release, Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R5 holder pinned by a profile with only a digest line",
        runtimes: CLAUDE,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        pin: Some((Holder, PinKind::DigestOnly)),
        steps: &[Release, Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R5 holder pinned by a session beside another session",
        runtimes: CLAUDE,
        profile: false,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        pin: Some((Holder, PinKind::Session)),
        other_session: Some("claude"),
        steps: &[Release, Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R5 holder pinned by a session beside a Pi session",
        runtimes: CLAUDE,
        profile: false,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        pin: Some((Holder, PinKind::Session)),
        other_session: Some("pi"),
        steps: &[Release, Uninstall(Holder)],
        end: End::Removed,
        ..BASE
    },
    Cell {
        id: "R5 holder pinned by two profiles",
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        pin: Some((Holder, PinKind::TwoProfiles)),
        steps: &[Release, Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R5 unselected holder pinned by a profile",
        installs: &[(Holder, true, false)],
        pin: Some((Holder, PinKind::Profile)),
        steps: &[Release, Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R5 holder pinned by a session, hook uses it",
        runtimes: CLAUDE,
        profile: false,
        hook: true,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        pin: Some((Holder, PinKind::Session)),
        steps: &[Release, Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R5 holder pinned by the only session",
        runtimes: CLAUDE,
        profile: false,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        pin: Some((Holder, PinKind::Session)),
        steps: &[Release, Uninstall(Holder)],
        end: End::Removed,
        ..BASE
    },
    Cell {
        id: "R6 altered holder pinned by a profile",
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Altered)],
        pin: Some((Holder, PinKind::Profile)),
        outcome: Outcome::Refusal,
        steps: &[Release, RemoveModified(Holder)],
        ..BASE
    },
    Cell {
        id: "R6 altered holder pinned by a session, hook uses it",
        runtimes: CLAUDE,
        profile: false,
        hook: true,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Altered)],
        pin: Some((Holder, PinKind::Session)),
        outcome: Outcome::Refusal,
        steps: &[Release, RemoveModified(Holder)],
        ..BASE
    },
    Cell {
        id: "R6 altered holder pinned by the only session",
        runtimes: CLAUDE,
        profile: false,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Altered)],
        pin: Some((Holder, PinKind::Session)),
        outcome: Outcome::Refusal,
        steps: &[Release, RemoveModified(Holder)],
        end: End::Removed,
        ..BASE
    },
    Cell {
        id: "R12 older selected, altered, pinned by the only session",
        runtimes: CLAUDE,
        profile: false,
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Altered)],
        pin: Some((Older, PinKind::Session)),
        outcome: Outcome::Refusal,
        steps: &[Release, RemoveModified(Older)],
        end: End::Removed,
        ..BASE
    },
    Cell {
        id: "R7 selected holder not official",
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::NonOfficial)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R7 selected holder not official, altered",
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::NonOfficial), (Holder, Damage::Altered)],
        outcome: Outcome::Refusal,
        steps: &[RemoveModified(Holder)],
        ..BASE
    },
    Cell {
        id: "R9 older selected, root missing",
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Missing)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R9 older selected, root missing, holder pinned, select hint",
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Missing)],
        pin: Some((Holder, PinKind::Profile)),
        select: true,
        ..BASE
    },
    Cell {
        id: "R9 older selected, root missing, select hint",
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Missing)],
        select: true,
        ..BASE
    },
    Cell {
        id: "R9 newer selected, root missing",
        installs: &[(Newer, true, true), (Holder, true, false)],
        damage: &[(Newer, Damage::Missing)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R10 older selected, root missing, holder altered",
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Missing), (Holder, Damage::Altered)],
        steps: &[RemoveModified(Holder)],
        ..BASE
    },
    Cell {
        id: "R11 older selected, altered",
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Altered)],
        outcome: Outcome::Refusal,
        steps: &[RemoveModified(Older)],
        end: End::Chain,
        ..BASE
    },
    Cell {
        id: "R12 older selected, altered, pinned",
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Altered)],
        pin: Some((Older, PinKind::Profile)),
        outcome: Outcome::Refusal,
        steps: &[Release, RemoveModified(Older)],
        end: End::Chain,
        ..BASE
    },
    Cell {
        id: "R13 non-official selected",
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::NonOfficial)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "R13 non-official selected, select hint",
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::NonOfficial)],
        select: true,
        ..BASE
    },
    Cell {
        id: "R14 non-official selected, holder altered",
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::NonOfficial), (Holder, Damage::Altered)],
        steps: &[RemoveModified(Holder)],
        ..BASE
    },
    Cell {
        id: "P1 selected, root missing",
        runtimes: PI,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        steps: &[Uninstall(Holder), Install],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P2 selected, root missing, older recorded",
        runtimes: PI,
        installs: &[(Older, true, false), (Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        steps: &[Uninstall(Holder), Update],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P3 managed only",
        runtimes: PI,
        profile: false,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        steps: &[Uninstall(Holder), Install],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P4 selected, altered",
        runtimes: PI,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Altered)],
        outcome: Outcome::Refusal,
        steps: &[RemoveModified(Holder), Install],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P4 selected, altered, older recorded",
        runtimes: PI,
        installs: &[(Older, true, false), (Holder, true, true)],
        damage: &[(Holder, Damage::Altered)],
        outcome: Outcome::Refusal,
        steps: &[RemoveModified(Holder), Update],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P5 selected, root missing, pinned",
        runtimes: PI,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Missing)],
        pin: Some((Holder, PinKind::Profile)),
        steps: &[Release, Uninstall(Holder), Install],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P5 selected, altered, pinned",
        runtimes: PI,
        installs: &[(Holder, true, true)],
        damage: &[(Holder, Damage::Altered)],
        pin: Some((Holder, PinKind::Profile)),
        outcome: Outcome::Refusal,
        steps: &[Release, RemoveModified(Holder), Install],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P7 older selected, root missing",
        runtimes: PI,
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Missing)],
        steps: &[Uninstall(Holder)],
        ..BASE
    },
    Cell {
        id: "P7 older selected, root missing, select hint",
        runtimes: PI,
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Missing)],
        select: true,
        ..BASE
    },
    Cell {
        id: "P8 older selected, root missing, holder altered",
        runtimes: PI,
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Missing), (Holder, Damage::Altered)],
        steps: &[RemoveModified(Holder)],
        ..BASE
    },
    Cell {
        id: "P9 older selected, altered",
        runtimes: PI,
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Altered)],
        outcome: Outcome::Refusal,
        steps: &[RemoveModified(Older)],
        end: End::Chain,
        ..BASE
    },
    Cell {
        id: "P10 older selected, altered, pinned",
        runtimes: PI,
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::Altered)],
        pin: Some((Older, PinKind::Profile)),
        outcome: Outcome::Refusal,
        steps: &[Release, RemoveModified(Older)],
        end: End::Chain,
        ..BASE
    },
    Cell {
        id: "P13 nothing selected",
        runtimes: PI,
        installs: &[(Holder, true, false)],
        steps: &[Uninstall(Holder), Install],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P13 nothing selected, holder disabled",
        runtimes: PI,
        installs: &[(Holder, false, false)],
        steps: &[Uninstall(Holder), Install],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P15 nothing selected, holder pinned",
        runtimes: PI,
        installs: &[(Holder, true, false)],
        pin: Some((Holder, PinKind::Profile)),
        steps: &[Release, Uninstall(Holder), Install],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P15 nothing selected, holder altered and pinned",
        runtimes: PI,
        installs: &[(Holder, true, false)],
        damage: &[(Holder, Damage::Altered)],
        pin: Some((Holder, PinKind::Profile)),
        outcome: Outcome::GapThenRefusal,
        steps: &[Release, RemoveModified(Holder), Install],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P16 non-official selected",
        runtimes: PI,
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::NonOfficial)],
        steps: &[Uninstall(Holder), Update],
        end: End::Present,
        ..BASE
    },
    Cell {
        id: "P16 non-official selected, select hint",
        runtimes: PI,
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::NonOfficial)],
        select: true,
        ..BASE
    },
    Cell {
        id: "P17 non-official selected, holder altered",
        runtimes: PI,
        installs: &[(Older, true, true), (Holder, true, false)],
        damage: &[(Older, Damage::NonOfficial), (Holder, Damage::Altered)],
        steps: &[RemoveModified(Holder), Update],
        end: End::Present,
        ..BASE
    },
];

/// The archive of a role for `runtime`.
fn role_archive(runtime: &str, role: Role) -> Archive {
    let pi = runtime == "pi";
    let version = match (role, pi) {
        (Role::Holder, _) => RELEASE_VERSION,
        (Role::Older, true) => PI_OLDER_VERSION,
        (Role::Older, false) => "1.0.1",
        (Role::Newer, true) => PI_NEWER_VERSION,
        (Role::Newer, false) => "2.1.0",
    };
    official_runtimes::variant_archive(runtime, version)
}

/// The profile that uses `runtime` in a cell: `work` for Pi.
fn cell_profile(runtime: &str) -> &str {
    if runtime == "pi" {
        "work"
    } else {
        runtime
    }
}

/// The second host profile of a cell that two profiles pin.
fn second_profile(runtime: &str) -> String {
    format!("{}-second", cell_profile(runtime))
}

/// The profile text that pins `digest`.
fn pinned_profile(runtime: &str, digest: &PackageDigest) -> String {
    format!(
        "base = \"{runtime}\"\npackage = \"pohunek.runtime.{runtime}\"\ndigest = \"{}\"\n",
        digest.as_str()
    )
}

/// The metadata store of a cell: a session record of Claude that pins `pinned`,
/// and a second record of the runtime `other` that pins another digest.
fn session_store(pinned: Option<&PackageDigest>, other: Option<&str>) -> String {
    let first = pinned.map_or_else(String::new, |pinned| {
        PACKAGE_SESSION_STORE.replace(&format!("sha256:{}", "a".repeat(64)), pinned.as_str())
    });
    let second = other.map_or_else(String::new, |runtime| {
        PACKAGE_SESSION_STORE
            .replace(
                &format!("sha256:{}", "a".repeat(64)),
                &format!("sha256:{}", "b".repeat(64)),
            )
            .replace("RPV", "RPW")
            .replace("claude", runtime)
    });
    format!("{first}{second}")
}

/// The command text a step names, as the hint spells it.
fn step_text(
    runtime: &str,
    pin: Option<PinKind>,
    step: Cmd,
    digest: &dyn Fn(Role) -> PackageDigest,
) -> String {
    let hex = |role: Role| {
        digest(role)
            .as_str()
            .strip_prefix("sha256:")
            .expect("a sha256 digest")
            .to_owned()
    };
    match step {
        Cmd::Release => {
            let profile = cell_profile(runtime);
            let two = format!("`{profile}.toml`, `{}.toml`", second_profile(runtime));
            let profile_lines = format!(
                "remove the `package` and `digest` lines from `{profile}.toml` in the host \
                 profile directory"
            );
            let sessions = "remove the sessions launched from it";
            let (who, release) = match pin.expect("a pinned cell") {
                PinKind::Profile | PinKind::DigestOnly => {
                    (format!("the host profile `{profile}.toml` pins"), profile_lines)
                }
                PinKind::TwoProfiles => (
                    format!("the host profile {two} pin"),
                    format!(
                        "remove the `package` and `digest` lines from {two} in the host \
                         profile directory"
                    ),
                ),
                PinKind::Session => ("a session record pins".to_owned(), sessions.to_owned()),
                PinKind::Both => (
                    format!("the host profile `{profile}.toml` and a session record pin"),
                    format!("{profile_lines} and {sessions}"),
                ),
            };
            format!(
                "{who} it, so the registry refuses its removal until the pin is released: \
                 {release}; then "
            )
        }
        Cmd::Uninstall(role) => format!(
            "pohunek plugin uninstall pohunek.runtime.{runtime} --digest {} --yes",
            hex(role)
        ),
        Cmd::RemoveModified(role) => format!(
            "pohunek plugin uninstall pohunek.runtime.{runtime} --digest {} --remove-modified --yes",
            hex(role)
        ),
        Cmd::Install => format!(
            "pohunek plugin install runtime/packages/{runtime}.tar.zst --catalog \
             runtime/runtime-catalog.json --yes"
        ),
        Cmd::Update => format!(
            "pohunek plugin update pohunek.runtime.{runtime} runtime/packages/{runtime}.tar.zst \
             --catalog runtime/runtime-catalog.json --yes"
        ),
    }
}

/// Builds the host of a cell for `runtime` and returns it with the archive of
/// each role and the release archive.
async fn cell_host(cell: &Cell, runtime: &str) -> (Host, Vec<(Role, Archive)>, Archive) {
    let archives: Vec<(Role, Archive)> = cell
        .installs
        .iter()
        .map(|(role, _, _)| (*role, role_archive(runtime, *role)))
        .collect();
    let installs: Vec<(&Archive, bool, bool)> = cell
        .installs
        .iter()
        .zip(&archives)
        .map(|((_, enabled, selected), (_, archive))| (archive, *enabled, *selected))
        .collect();
    let release = holder_release(runtime);
    let host = host_with_holder(runtime, &installs, &release).await;
    let digest = |role: Role| -> PackageDigest {
        archives
            .iter()
            .find(|(candidate, _)| *candidate == role)
            .map_or_else(
                || role_archive(runtime, role).1,
                |(_, archive)| archive.1.clone(),
            )
    };
    let profile = cell_profile(runtime);
    if runtime == "pi" {
        if cell.profile {
            host.write_profile(profile, "base = \"pi\"\n");
        }
    } else if !cell.profile {
        fs::remove_file(host.profiles_dir().join(format!("{profile}.toml")))
            .expect("remove the profile");
    }
    if cell.hook {
        host.write_claude_hook();
    }
    if cell.store {
        host.write_store(PACKAGE_SESSION_STORE);
    }
    for (role, damage) in cell.damage {
        match damage {
            Damage::Missing => host.remove_package_root(digest(*role).as_str()),
            Damage::Altered => host.alter_package_root(&digest(*role)),
            Damage::NonOfficial => set_package_source(&host, &digest(*role), "explicit_digest"),
        }
    }
    if let Some((role, kind)) = cell.pin {
        if kind == PinKind::DigestOnly {
            host.write_profile(
                profile,
                &format!(
                    "base = \"{runtime}\"\ndigest = \"{}\"\n",
                    digest(role).as_str()
                ),
            );
        } else if kind != PinKind::Session {
            host.write_profile(profile, &pinned_profile(runtime, &digest(role)));
            if kind == PinKind::TwoProfiles {
                host.write_profile(
                    &second_profile(runtime),
                    &pinned_profile(runtime, &digest(role)),
                );
            }
        }
        if matches!(kind, PinKind::Session | PinKind::Both) {
            host.write_store(&session_store(Some(&digest(role)), cell.other_session));
        }
    } else if cell.other_session.is_some() {
        host.write_store(&session_store(None, cell.other_session));
    }
    (host, archives, release)
}

/// Runs the cell for `runtime`: the outcome in plan, apply and `--detect`, then
/// the remedy the hint names, step by step as the CLI sends it.
#[expect(
    clippy::too_many_lines,
    reason = "one remedy, read top to bottom: outcome, hint, steps, end"
)]
async fn run_cell(cell: &Cell, runtime: &str) {
    let label = format!("{} [{runtime}]", cell.id);
    let (host, archives, release) = cell_host(cell, runtime).await;
    let digest = |role: Role| -> PackageDigest {
        archives
            .iter()
            .find(|(candidate, _)| *candidate == role)
            .map_or_else(
                || role_archive(runtime, role).1,
                |(_, archive)| archive.1.clone(),
            )
    };
    let detail = match cell.outcome {
        Outcome::Gap => assert_held_runtime_left(&host, runtime, &[]),
        Outcome::Refusal => assert_held_runtime_refused(&host, runtime, ""),
        Outcome::GapThenRefusal => assert_held_pi_altered(&host, runtime),
    };
    if cell.select {
        // A pinned holder cannot be removed, so only the select is named.
        assert_eq!(
            detail.contains("`pohunek plugin uninstall"),
            cell.pin.is_none(),
            "{label}: {detail}"
        );
        assert_select_hint_makes_the_holder_serve(&host, runtime, &digest(Holder));
        return;
    }

    // Every command the hint names is a step, in order, and no other is named.
    let mut at = 0;
    let mut commands = 0;
    for step in cell.steps {
        let text = step_text(runtime, cell.pin.map(|(_, kind)| kind), *step, &digest);
        let found = detail[at..]
            .find(&text)
            .unwrap_or_else(|| panic!("{label}: `{text}` is not named in order: {detail}"));
        at += found + text.len();
        commands += usize::from(!matches!(step, Release));
    }
    assert_eq!(
        detail.matches("`pohunek ").count(),
        commands,
        "{label}: {detail}"
    );
    // The follow-up names the next upgrade only where the runtime is planned again.
    assert_eq!(
        detail.contains("run the upgrade again"),
        !matches!(cell.end, End::Removed | End::Present),
        "{label}: {detail}"
    );
    let pins_named =
        detail.contains("it, so the registry refuses its removal until the pin is released");
    assert_eq!(
        pins_named,
        cell.steps.iter().any(|step| matches!(step, Release)),
        "{label}: {detail}"
    );

    let store = pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
        .expect("the package store");
    let registry = store.registry();
    let mut retained = package::registry::RetainedDigests::new();
    if let Some((role, _)) = cell.pin {
        retained.insert(digest(role));
    }
    for step in cell.steps {
        match step {
            Release => {
                let target = cell
                    .steps
                    .iter()
                    .find_map(|next| match next {
                        Uninstall(role) | RemoveModified(role) => Some(digest(*role)),
                        _ => None,
                    })
                    .expect("a removal follows the release of the pin");
                assert!(
                    matches!(
                        registry.uninstall(&target, &retained),
                        Err(package::registry::RegistryError::StillReferenced)
                    ),
                    "{label}: the removal is refused while the pin stands"
                );
                let (_, kind) = cell.pin.expect("a pinned cell");
                if kind != PinKind::Session {
                    host.write_profile(cell_profile(runtime), &format!("base = \"{runtime}\"\n"));
                }
                if kind == PinKind::TwoProfiles {
                    host.write_profile(
                        &second_profile(runtime),
                        &format!("base = \"{runtime}\"\n"),
                    );
                }
                if matches!(kind, PinKind::Session | PinKind::Both) {
                    host.write_store(&session_store(None, cell.other_session));
                }
                retained = package::registry::RetainedDigests::new();
            }
            Uninstall(role) => {
                registry
                    .uninstall(&digest(*role), &retained)
                    .unwrap_or_else(|error| {
                        panic!("{label}: the uninstall is accepted: {error:?}")
                    });
            }
            RemoveModified(role) => {
                registry
                    .remove_modified(&digest(*role), &retained)
                    .unwrap_or_else(|error| panic!("{label}: the removal is accepted: {error:?}"));
            }
            Install | Update => {
                host.catalog
                    .install_release_package(
                        &host.plugins_dir(),
                        host.env.root(),
                        &host.release_dir(),
                        runtime,
                    )
                    .await;
            }
        }
    }
    match cell.end {
        End::Installed => {
            let applied = host.step(false);
            assert!(
                applied.refusals.is_empty(),
                "{label}: {:?}",
                applied.refusals
            );
            let package = applied
                .packages
                .iter()
                .find(|package| package.runtime == runtime)
                .unwrap_or_else(|| panic!("{label}: {:?}", applied.packages));
            assert_eq!(package.status, PackageStatus::Installed, "{label}");
            assert_eq!(package.digest, release.1.as_str(), "{label}");
            if runtime != "pi" && cell.profile {
                assert_eq!(
                    profile_status(&applied.profiles, runtime),
                    ProfileStatus::Bound,
                    "{label}"
                );
            }
        }
        End::Present => {
            let next = host.step(false);
            assert!(next.refusals.is_empty(), "{label}: {:?}", next.refusals);
            assert_eq!(pi_package(&next).status, PackageStatus::Present, "{label}");
            assert_eq!(pi_package(&next).digest, release.1.as_str(), "{label}");
        }
        End::Removed => {}
        End::Chain => {
            let holder = uninstall_command(runtime, &digest(Holder));
            let next = if runtime == "pi" {
                vec![
                    holder.as_str(),
                    "pohunek plugin install runtime/packages/pi.tar.zst",
                ]
            } else {
                vec![holder.as_str()]
            };
            assert_held_runtime_left(&host, runtime, &next);
        }
    }
}

/// Every cell of the holder contract table that applies to the reserved runtimes.
#[tokio::test]
async fn the_holder_contract_table_holds_for_the_reserved_runtimes() {
    for cell in CELLS {
        for runtime in cell.runtimes.iter().filter(|runtime| **runtime != "pi") {
            run_cell(cell, runtime).await;
        }
    }
}

/// Every cell of the holder contract table that applies to Pi.
#[tokio::test]
async fn the_holder_contract_table_holds_for_pi() {
    for cell in CELLS {
        if cell.runtimes.contains(&"pi") {
            run_cell(cell, "pi").await;
        }
    }
}

/// An unmanaged Pi whose unselected holder is altered: plan and apply report the
/// gap (Pi is not planned without an enabled official selection), the detecting
/// plan refuses (it lists Pi), and both name the removal and the install.
#[tokio::test]
async fn an_unselected_altered_pi_holder_is_a_gap_without_detect_and_a_refusal_with_it() {
    let holder = official_runtimes::variant_archive("pi", RELEASE_VERSION);
    let release = holder_release("pi");
    let host = host_with_holder("pi", &[(&holder, true, false)], &release).await;
    host.alter_package_root(&holder.1);
    let registry = fs::read(host.registry_path()).expect("the registry record");
    let hint = format!(
        "pohunek plugin uninstall pohunek.runtime.pi --digest {} --remove-modified --yes",
        holder.1.as_str().strip_prefix("sha256:").expect("a digest")
    );
    let install = "pohunek plugin install runtime/packages/pi.tar.zst --catalog \
                   runtime/runtime-catalog.json --yes";

    for report in [host.step(true), host.step(false)] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        let detail = not_serving_detail(&report, "pi").expect("the gap");
        assert!(
            detail.contains(&hint) && detail.contains(install),
            "{detail}"
        );
    }
    let detected = host.detect_step();
    assert_eq!(detected.refusals.len(), 1, "{:?}", detected.refusals);
    assert_eq!(
        not_serving_detail(&detected, "pi"),
        None,
        "the refusal supersedes the managed gap"
    );
    let detail = &detected.refusals[0].detail;
    assert!(
        detail.contains(&hint) && detail.contains(install),
        "{detail}"
    );
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
    pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
        .expect("the package store")
        .registry()
        .remove_modified(&holder.1, &package::registry::RetainedDigests::new())
        .expect("the registry accepts the hinted removal");
    host.catalog
        .install_release_package(
            &host.plugins_dir(),
            host.env.root(),
            &host.release_dir(),
            "pi",
        )
        .await;
    assert_eq!(pi_package(&host.step(false)).status, PackageStatus::Present);
}

/// A reserved runtime that only `--detect` plans, with an altered holder: the
/// detecting plan refuses (setup is blocked) and names the removal and the next
/// upgrade, and after the removal a detecting plan plans the install.
#[tokio::test]
async fn a_runtime_planned_only_by_detection_with_an_altered_holder_is_refused_with_its_removal() {
    let runtime = "claude";
    let holder = official_runtimes::variant_archive(runtime, RELEASE_VERSION);
    let release = holder_release(runtime);
    let host = host_with_holder(runtime, &[(&holder, true, true)], &release).await;
    fs::remove_file(host.profiles_dir().join(format!("{runtime}.toml")))
        .expect("remove the profile");
    host.alter_package_root(&holder.1);

    let detected = host.detect_step();

    assert_eq!(detected.refusals.len(), 1, "{:?}", detected.refusals);
    let detail = &detected.refusals[0].detail;
    assert!(
        detail.contains(&format!(
            "pohunek plugin uninstall pohunek.runtime.{runtime} --digest {} --remove-modified --yes`, then run the upgrade again",
            holder.1.as_str().strip_prefix("sha256:").expect("a digest")
        )),
        "{detail}"
    );
    pohunek_daemon::agent::host::PackageStore::open(&host.plugins_dir())
        .expect("the package store")
        .registry()
        .remove_modified(&holder.1, &package::registry::RetainedDigests::new())
        .expect("the registry accepts the hinted removal");
    let after = host.detect_step();
    let package = after
        .packages
        .iter()
        .find(|package| package.runtime == runtime)
        .expect("the detecting plan plans the runtime");
    assert_eq!(package.digest, release.1.as_str());
}
