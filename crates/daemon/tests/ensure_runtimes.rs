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
    CODE_GAP_SERVED_BY_OTHER_PACKAGE, CODE_GAP_UPDATE_VERSION_UNORDERED,
    CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM, CODE_PINNED_TO_NEWER_VERSION,
    CODE_PINNED_TO_OTHER_PACKAGE, CODE_PINNED_VERSION_UNKNOWN, CODE_PROFILE_NOT_MIGRATABLE,
    CODE_RELEASE_CATALOG_UNAVAILABLE, CODE_SERVICE_CONFIG_UNREADABLE, DETECT_FLAG, DRY_RUN_FLAG,
    ENSURE_REPORT_VERSION, ENSURE_RUNTIMES_ARG, RELEASE_RUNTIME_DIR,
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

/// A release package that is selected but disabled is only enabled: the apply
/// makes no selecting call, so the retained set is not judged for it. A
/// digest-only profile that retains a damaged older version is then bound to
/// the enabled package, and the plan and the apply agree.
#[test]
fn a_selected_but_disabled_release_package_is_enabled_and_its_profile_bound() {
    let host = Host::new();
    let (_older, digest, second) = digest_only_repair_state(&host);
    host.disable_package(&digest);

    let plan = host.step(true);

    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    let claude = plan
        .packages
        .iter()
        .find(|package| package.runtime == "claude")
        .expect("the claude package");
    assert_eq!(claude.status, PackageStatus::Install);
    assert_eq!(plan.profiles.len(), 1);
    assert_eq!(plan.profiles[0].status, ProfileStatus::Bind);

    let applied = host.step(false);

    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    let claude = applied
        .packages
        .iter()
        .find(|package| package.runtime == "claude")
        .expect("the claude package");
    assert_eq!(claude.status, PackageStatus::Installed);
    assert_eq!(applied.profiles.len(), 1);
    assert_eq!(applied.profiles[0].status, ProfileStatus::Bound);
    assert!(fs::read_to_string(&second)
        .expect("the bound profile")
        .contains(&format!("digest = \"{}\"", digest.as_str())));

    let repeat = host.step(false);
    let claude = repeat
        .packages
        .iter()
        .find(|package| package.runtime == "claude")
        .expect("the claude package");
    assert_eq!(claude.status, PackageStatus::Present);
}

/// A selected but disabled release package is only enabled, and the apply then
/// runs the coexistence check of the profile bind it carries. Another profile
/// retains a damaged older version, so the plan refuses the bind before the
/// enable is applied, and nothing changes.
#[test]
fn a_selected_but_disabled_release_package_is_not_enabled_when_its_bind_would_refuse() {
    let host = Host::new();
    host.publish_at(&["claude"], 1);
    host.write_profile("other", "base = \"claude\"\n");
    let older = host.step(false).packages[0].digest.clone();
    let pinned_older = host.profile_text_of("other");
    let (bytes, digest) = official_runtimes::variant_archive("claude", "1.0.3");
    host.publish_archives(2, &[("claude", &bytes, &digest)]);
    let newer = host.step(false);
    assert_eq!(newer.packages[0].status, PackageStatus::Installed);
    host.write_profile("other", &pinned_older);
    host.disable_package(&digest);
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
        registry,
        "the enable is not applied"
    );
    assert_eq!(fs::read(&target).expect("the unpinned profile"), profile);
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
    let (bytes, digest) = official_runtimes::variant_archive("claude", "1.0.3");
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
    for runtime in ["claude", "codex"] {
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
const PI_INSTALLED_VERSION: &str = "1.0.3";

/// A version newer than [`PI_INSTALLED_VERSION`], published as a later release.
const PI_NEWER_VERSION: &str = "1.0.4";

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
                .contains("pohunek plugin uninstall --digest"),
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

#[tokio::test]
async fn a_disabled_or_unselected_pi_package_is_reported_not_changed() {
    let host = Host::with_installed_pi().await;
    host.publish_pi(PI_NEWER_VERSION, 1);
    host.disable_package(&Host::installed_pi_digest());
    host.write_profile("work", "base = \"pi\"\n");
    let registry = fs::read(host.registry_path()).expect("the registry record");

    let plan = host.step(true);
    let applied = host.step(false);

    for report in [&plan, &applied] {
        assert!(report.refusals.is_empty(), "{:?}", report.refusals);
        assert!(report.packages.is_empty(), "{:?}", report.packages);
        assert_eq!(
            gap_codes(report),
            [("pi", CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)]
        );
        assert!(
            report.gaps[0].detail.contains("pohunek plugin update"),
            "{}",
            report.gaps[0].detail
        );
    }
    assert_eq!(
        fs::read(host.registry_path()).expect("the registry record"),
        registry
    );
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
