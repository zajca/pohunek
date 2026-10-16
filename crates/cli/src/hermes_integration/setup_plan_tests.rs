//! Scenarios of the Hermes step of bare `pohunek setup` and of the Hermes
//! reinstall of `pohunek service upgrade`: planning the marked Hermes homes
//! and applying the plan with the `hermes` of a given search path, against an
//! installed official Hermes package and a controlled `hermes`. They need the
//! test Python interpreter, as every module below `hermes_integration` does.

// Rust guideline compliant 2026-10-15

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use package::registry::{InstallRequest, PackageSource, Registry};
use protocol::{PackageId, PackageIdentity, PackageVersion};

use super::assets;
use super::lifecycle;
use super::policy::{
    policy_path, AccessMode, Policy, PolicyInput, WildcardConfirmation, MAX_CONCURRENCY,
    MAX_OUTPUT_BYTES, MAX_SCREEN_BYTES, MAX_TIMEOUT_MS,
};
use super::runner::HermesRunner;
use super::target::{TargetContext, TargetSelection};
use super::test_package;
use super::test_python;
use crate::commands::integration::{
    hermes_home_plan, reinstall_previewed_hermes_homes, IntegrationEnv, HERMES_CURRENT,
    HERMES_UPDATE,
};
use crate::service::report::IntegrationReinstall;
use crate::service::runtimes::{CliIntegrations, IntegrationStep as _};

fn private_directory(path: &Path) {
    fs::create_dir_all(path).expect("create private directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .expect("set private directory mode");
}

/// Returns a private fixture directory guard and a private child directory of
/// it; the guard removes everything when it drops, also when a test fails.
fn temporary_directory(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let guard = pohunek_test_support::tempdir().expect("create fixture directory");
    let path = guard.path().join(tag);
    private_directory(&path);
    (guard, path)
}

/// The pinned Hermes release the controlled `hermes` reports.
const SUPPORTED_HERMES: &str = "Hermes Agent v0.20.0 (2026.8.3)";

/// A host with the official Hermes package selected in its package store,
/// a controlled `hermes` that logs each call in a directory of its own,
/// and the plugin of the default Hermes home installed from that package;
/// the install leaves it enabled.
struct HermesHost {
    _guard: tempfile::TempDir,
    env: IntegrationEnv,
    hermes_home: PathBuf,
    policy_path: PathBuf,
    /// The search path that holds the controlled `hermes`; the test
    /// process's own `PATH` does not.
    search_path: std::ffi::OsString,
    log: PathBuf,
    state: PathBuf,
    /// The banner the controlled `hermes --version` prints.
    version: PathBuf,
}

impl HermesHost {
    fn new() -> Self {
        let (guard, root) = temporary_directory("hermes-host");
        let home = root.join("home");
        let hermes_home = home.join(".hermes");
        private_directory(&hermes_home);
        let state_dir = root.join("state").join("pohunek");
        private_directory(&state_dir);
        let plugins = state_dir.join(pohunek_paths::PLUGINS_SUBDIR);

        install_official_hermes(&plugins);

        let target = TargetContext::new(hermes_home.clone(), home.clone(), Vec::new())
            .and_then(|context| context.resolve(TargetSelection::CustomHome(hermes_home.clone())))
            .expect("resolve the default Hermes home");
        let log = root.join("hermes-calls.log");
        let state = root.join("hermes-plugin-state");
        fs::write(&state, "disabled\n").expect("initial plugin state");
        let version = root.join("hermes-version");
        fs::write(&version, format!("{SUPPORTED_HERMES}\n")).expect("version banner");
        let hermes = write_controlled_hermes(&root, target.plugin_root(), &log, &state, &version);
        let venv_bin = hermes.parent().expect("venv bin").to_path_buf();

        let policy_path = policy_path(&state_dir, &target).expect("policy path");
        let versions = protocol::CURRENT_PROTOCOL_VERSIONS;
        let policy = Policy::new(PolicyInput {
            pohunek_cli: std::env::current_exe().expect("test executable"),
            protocol_min: i32::try_from(versions.minimum().get()).expect("protocol minimum"),
            protocol_max: i32::try_from(versions.maximum().get()).expect("protocol maximum"),
            access_mode: AccessMode::Manage,
            allowed_hosts: vec!["local".to_owned()],
            tool_timeout_ms: MAX_TIMEOUT_MS / 2,
            request_timeout_ms: MAX_TIMEOUT_MS / 4,
            max_output_bytes: MAX_OUTPUT_BYTES / 2,
            max_screen_bytes: MAX_SCREEN_BYTES / 2,
            max_concurrency: MAX_CONCURRENCY / 2,
            wildcard_confirmation: WildcardConfirmation::new(false),
        })
        .expect("policy");
        let bundle = assets::selected_bundle(&plugins, &policy_path).expect("the selected package");
        let mut runner = HermesRunner::new(&hermes).expect("controlled Hermes runner");
        lifecycle::install(
            &mut runner,
            &lifecycle::InstallRequest::new(&target, &policy_path, &policy, false, &bundle),
        )
        .expect("install the plugin from the selected package");
        fs::write(&log, "").expect("clear the call log");

        let env = IntegrationEnv {
            paths: crate::paths::Paths {
                runtime_dir: root.join("run"),
                socket: root.join("run/daemon.sock"),
                data_dir: root.join("data"),
                log_dir: root.join("log"),
                cache_dir: root.join("cache"),
                config_home: root.join("config"),
                config_dir: root.join("config/pohunek"),
                origin_source: pohunek_client::OriginSource::Environment,
            },
            home: Some(home),
            state_dir,
        };
        Self {
            _guard: guard,
            env,
            hermes_home,
            policy_path,
            search_path: venv_bin.into_os_string(),
            log,
            state,
            version,
        }
    }

    /// The files of the Hermes home and of the policy directory, by path.
    fn files(&self) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        let mut pending = vec![
            self.hermes_home.clone(),
            self.policy_path
                .parent()
                .expect("policy directory")
                .to_path_buf(),
        ];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).expect("read directory") {
                let path = entry.expect("directory entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let bytes = fs::read(&path).expect("read file");
                    files.insert(path, bytes);
                }
            }
        }
        files
    }

    fn calls(&self) -> String {
        fs::read_to_string(&self.log).expect("read the call log")
    }
}

/// Installs the official Hermes package of this workspace into the
/// package store at `plugins`, enabled and selected.
fn install_official_hermes(plugins: &Path) {
    let source = pohunek_test_support::workspace_root().join("runtime-packages/hermes");
    let bytes = package::directory::build_directory_archive(&source, &package::Limits::DEFAULT)
        .expect("the Hermes package builds");
    let digest = package::read_archive(&bytes, &package::Limits::DEFAULT)
        .expect("the Hermes archive reads")
        .digest()
        .clone();
    let manifest: toml::Table = fs::read_to_string(source.join("runtime.toml"))
        .expect("read the Hermes runtime manifest")
        .parse()
        .expect("parse the Hermes runtime manifest");
    let identity = PackageIdentity {
        id: PackageId::parse(manifest["id"].as_str().expect("package id"))
            .expect("valid package id"),
        version: PackageVersion::parse(manifest["version"].as_str().expect("version"))
            .expect("valid package version"),
    };
    Registry::open_at(plugins, package::Limits::DEFAULT)
        .expect("open the package store")
        .install(&InstallRequest {
            archive: &bytes,
            expected: &digest,
            identity,
            source: PackageSource::Official,
            enabled: true,
            select: true,
            installed_at_unix_seconds: 0,
        })
        .expect("install the official Hermes package");
}

/// Writes a `hermes` below `root` with its contained Python runtime, as a
/// Hermes installation lays them out. It appends each call's arguments to
/// `log` and keeps the plugin's enabled state in `state`.
fn write_controlled_hermes(
    root: &Path,
    plugin_root: &Path,
    log: &Path,
    state: &Path,
    version: &Path,
) -> PathBuf {
    let venv_bin = root.join("hermes-install/venv/bin");
    let runtime = root.join("hermes-install/python/bin/python3");
    private_directory(&venv_bin);
    private_directory(runtime.parent().expect("runtime directory"));
    write_script(
        &runtime,
        &format!("exec '{}' \"$@\"", test_python::interpreter().display()),
    );
    std::os::unix::fs::symlink("../../python/bin/python3", venv_bin.join("python3"))
        .expect("link the contained runtime");
    write_script(
        &venv_bin.join("hermes"),
        &format!(
            r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  --version) /bin/cat '{version}' ;;
  'plugins list --json')
if [ ! -d '{plugin_root}' ]; then printf '%s\n' '[]'; exit 0; fi
IFS= read -r state < '{state}'
printf '[{{"name":"pohunek","status":"%s","source":"user"}}]\n' "$state" ;;
  'plugins enable pohunek --no-allow-tool-override') printf '%s\n' enabled > '{state}' ;;
  'plugins disable pohunek') printf '%s\n' disabled > '{state}' ;;
  *) exit 90 ;;
esac"#,
            log = log.display(),
            plugin_root = plugin_root.display(),
            state = state.display(),
            version = version.display(),
        ),
    )
}

fn write_script(path: &Path, body: &str) -> PathBuf {
    pohunek_test_support::fs::write_file(path, format!("#!/bin/sh\nset -eu\n{body}\n"))
        .expect("write script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("script mode");
    path.to_owned()
}

/// The planned or applied status of the default Hermes home.
fn default_home(results: &[IntegrationReinstall]) -> &IntegrationReinstall {
    results
        .iter()
        .find(|result| result.integration == "hermes")
        .expect("the default home is listed")
}

/// A marked home whose plugin is the selected package's, unmodified and
/// enabled, is planned as current and left byte for byte as it is; a run
/// that changes the selected Hermes package plans it as an update.
#[test]
fn a_current_hermes_home_is_planned_current_and_is_not_written() {
    let host = HermesHost::new();
    let search = host.search_path.as_os_str();

    let planned = hermes_home_plan(&host.env, search, false);
    assert_eq!(default_home(&planned).status, HERMES_CURRENT, "{planned:?}");
    let before = host.files();

    let applied = reinstall_previewed_hermes_homes(&host.env, &planned, search);

    assert_eq!(default_home(&applied).status, HERMES_CURRENT, "{applied:?}");
    assert_eq!(host.files(), before, "a current home is not written");
    assert!(
        !host.calls().contains("plugins enable"),
        "nothing is enabled: {}",
        host.calls()
    );
    let changing = hermes_home_plan(&host.env, search, true);
    assert_eq!(
        default_home(&changing).status,
        HERMES_UPDATE,
        "a new Hermes selection updates the home"
    );
}

/// A marked home that is not current is updated with the `hermes` on the
/// search path the caller passes, which the test process's own `PATH`
/// does not hold; afterwards it is current.
#[test]
fn a_hermes_update_runs_the_hermes_of_the_given_search_path() {
    let host = HermesHost::new();
    fs::write(&host.state, "disabled\n").expect("disable the plugin");
    let search = host.search_path.as_os_str();
    assert!(
        std::env::var_os("PATH").is_none_or(
            |path| std::env::split_paths(&path).all(|entry| entry.as_os_str() != search)
        ),
        "the process PATH must not hold the controlled hermes"
    );

    let planned = hermes_home_plan(&host.env, search, false);
    assert_eq!(default_home(&planned).status, HERMES_UPDATE, "{planned:?}");

    let applied = reinstall_previewed_hermes_homes(&host.env, &planned, search);

    assert_eq!(default_home(&applied).status, "installed", "{applied:?}");
    assert!(
        host.calls().contains("plugins enable pohunek"),
        "the controlled hermes enabled the plugin: {}",
        host.calls()
    );
    let replanned = hermes_home_plan(&host.env, search, false);
    assert_eq!(
        default_home(&replanned).status,
        HERMES_CURRENT,
        "{replanned:?}"
    );
}

/// The upgrade's reinstall updates a marked home with the `hermes` on the
/// daemon job's `PATH` it is given, which the `PATH` of the CLI process does
/// not hold.
#[tokio::test]
async fn an_upgrade_reinstall_runs_the_hermes_of_the_job_path() {
    let host = HermesHost::new();
    fs::write(&host.state, "disabled\n").expect("disable the plugin");
    let job_path = host.search_path.to_str().expect("UTF-8 search path");
    assert!(
        std::env::var_os("PATH").is_none_or(|path| std::env::split_paths(&path)
            .all(|entry| entry.as_os_str() != host.search_path.as_os_str())),
        "the process PATH must not hold the controlled hermes"
    );

    let applied = CliIntegrations.reinstall(&host.env, &[], job_path).await;

    assert_eq!(default_home(&applied).status, "installed", "{applied:?}");
    assert!(
        host.calls().contains("plugins enable pohunek"),
        "the job's hermes enabled the plugin: {}",
        host.calls()
    );
    assert_eq!(
        default_home(&hermes_home_plan(&host.env, &host.search_path, false)).status,
        HERMES_CURRENT,
        "the home is current afterwards"
    );
}

/// A marked home whose plugin is intact and enabled stays current under any
/// build of a release the selected package supports, and is not current when
/// the `hermes` on the search path is outside the package's range, so the
/// apply's update refuses it instead of the plan keeping it.
#[test]
fn a_hermes_outside_the_package_range_does_not_leave_a_home_current() {
    let host = HermesHost::new();
    let search = host.search_path.as_os_str();
    let policy = test_package::version_policy().expect("the package version policy");
    assert_eq!(
        default_home(&hermes_home_plan(&host.env, search, false)).status,
        HERMES_CURRENT,
        "the locked release keeps the home current"
    );
    fs::write(
        &host.version,
        format!("Hermes Agent v{} (2026.9.1)\n", policy.min()),
    )
    .expect("another build");
    assert_eq!(
        default_home(&hermes_home_plan(&host.env, search, false)).status,
        HERMES_CURRENT,
        "another build of a supported release keeps the home current"
    );
    fs::write(
        &host.version,
        format!("Hermes Agent v{} (future)\n", policy.below()),
    )
    .expect("a release above the range");

    let planned = hermes_home_plan(&host.env, search, false);

    assert_eq!(default_home(&planned).status, HERMES_UPDATE, "{planned:?}");
}
