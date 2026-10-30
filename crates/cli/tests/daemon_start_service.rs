//! `pohunek daemon start --service` over an isolated XDG layout.
//!
//! The real CLI binary runs against a `service.toml` whose active version
//! directory holds a recording stand-in for `pohunekd`. A directory that is
//! not what publishing creates is refused with the service engine's
//! `UntrustedVersion` error before the stand-in runs.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-10-30

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use pohunek_cli::service::layout::BINARIES;
use pohunek_service_config::ServiceConfig;

/// A schema-2 `service.toml`, upgraded to the current schema by the fixture.
const SCHEMA_TWO_TEMPLATE: &str = r#"
schema_version = 2
prefix = "{PREFIX}"
active_version = "1.0.0"

[namespace]
uid = {UID}
state_root = "{STATE_ROOT}"
runtime_root = "{RUNTIME_ROOT}"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
search_path = ["/usr/bin"]
allowlist = ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_*"]

[sweep]
grace_ms = 5000

[limits]
open_files = 8192
"#;

/// The file the stand-in daemon writes its arguments to.
const MARKER: &str = "daemon-ran";

/// The message of `Error::UntrustedVersion` (code `service_version_untrusted`),
/// which `daemon start` has no `--json` to print the code with.
const UNTRUSTED_MESSAGE: &str = "cannot be trusted as an installed version";

/// An isolated XDG layout with `service.toml` and an installed `1.0.0`.
struct Host {
    root: tempfile::TempDir,
}

impl Host {
    fn new() -> Self {
        let root = pohunek_test_support::tempdir_with_prefix("phk-").expect("temporary root");
        let host = Self { root };
        for name in ["run", "state", "data", "cache", "config", "home", "prefix"] {
            private_dir(&host.path(name));
        }
        private_dir(&host.path("run").join("pohunek"));
        private_dir(&host.path("state").join("pohunek"));
        private_dir(&host.path("config").join("pohunek"));
        host.write_config();
        host.install_version();
        host
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn config_file(&self) -> PathBuf {
        self.path("config").join("pohunek").join("service.toml")
    }

    fn version_dir(&self) -> PathBuf {
        self.path("prefix")
            .join("libexec")
            .join("pohunek")
            .join("1.0.0")
    }

    fn marker(&self) -> PathBuf {
        self.root.path().join(MARKER)
    }

    fn write_config(&self) {
        let source = SCHEMA_TWO_TEMPLATE
            .replace("{PREFIX}", &self.path("prefix").display().to_string())
            .replace("{UID}", &nix::unistd::Uid::effective().as_raw().to_string())
            .replace(
                "{STATE_ROOT}",
                &self.path("state").join("pohunek").display().to_string(),
            )
            .replace(
                "{RUNTIME_ROOT}",
                &self.path("run").join("pohunek").display().to_string(),
            );
        let config = self.config_file();
        fs::write(&config, source).expect("write the schema-2 source");
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).expect("private config");
        let upgraded = ServiceConfig::load_upgrade_source(&config)
            .expect("schema-2 source")
            .config;
        ServiceConfig::new(upgraded.to_spec())
            .expect("current configuration")
            .write(&config)
            .expect("write the current schema");
    }

    /// Creates the version directory the way publishing leaves it, with a
    /// `pohunekd` that records its arguments.
    fn install_version(&self) {
        let versions = self.version_dir().parent().expect("versions").to_path_buf();
        fs::create_dir_all(&versions).expect("create the versions directory");
        public_dir(&self.path("prefix").join("libexec"));
        public_dir(&versions);
        let version = self.version_dir();
        fs::create_dir(&version).expect("create the version directory");
        public_dir(&version);
        let recorder = format!("#!/bin/sh\necho \"$@\" > '{}'\n", self.marker().display());
        for binary in BINARIES {
            let body = if binary == pohunek_paths::DAEMON_EXECUTABLE_NAME {
                recorder.as_str()
            } else {
                "#!/bin/sh\nexit 0\n"
            };
            pohunek_test_support::fs::write_executable(version.join(binary), body)
                .expect("write a version binary");
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let env_of = |name: &str| self.path(name).display().to_string();
        Command::new(pohunek_test_support::bin_exe("pohunek"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("XDG_RUNTIME_DIR", env_of("run"))
            .env("XDG_STATE_HOME", env_of("state"))
            .env("XDG_DATA_HOME", env_of("data"))
            .env("XDG_CACHE_HOME", env_of("cache"))
            .env("XDG_CONFIG_HOME", env_of("config"))
            .env("HOME", env_of("home"))
            .args(args)
            .output()
            .expect("run the real CLI")
    }

    /// Asserts the start was refused as an untrusted version and no daemon ran.
    fn assert_refused(&self, output: &Output) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{output:?}");
        assert!(
            stderr.contains(UNTRUSTED_MESSAGE),
            "the refusal is the service engine's untrusted-version error: {stderr}"
        );
        assert!(stderr.contains("hint:"), "{stderr}");
        assert!(!self.marker().exists(), "the daemon must not run");
    }
}

fn private_dir(path: &Path) {
    fs::create_dir_all(path).expect("create a directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private directory");
}

fn public_dir(path: &Path) {
    fs::create_dir_all(path).expect("create a directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("public directory");
}

#[test]
fn an_installed_active_version_runs_with_the_service_configuration() {
    let host = Host::new();

    let output = host.run(&["daemon", "start", "--detach"]);

    assert!(output.status.success(), "{output:?}");
    let config = host.config_file();
    let recorded =
        pohunek_test_support::wait::poll_until("the daemon to record its arguments", || {
            fs::read_to_string(host.marker())
                .ok()
                .filter(|text| !text.is_empty())
        });
    assert_eq!(
        recorded.trim(),
        format!("--service-config {}", config.display())
    );
}

#[test]
fn a_foreground_start_replaces_the_cli_with_the_installed_daemon() {
    let host = Host::new();

    let output = host.run(&["daemon", "start"]);

    assert!(output.status.success(), "{output:?}");
    let recorded = fs::read_to_string(host.marker()).expect("the daemon ran before the CLI exited");
    assert_eq!(
        recorded.trim(),
        format!("--service-config {}", host.config_file().display())
    );
}

#[test]
fn a_group_writable_version_directory_is_refused_before_the_daemon_runs() {
    let host = Host::new();
    fs::set_permissions(host.version_dir(), fs::Permissions::from_mode(0o775))
        .expect("loosen the version directory");

    host.assert_refused(&host.run(&["daemon", "start"]));
}

#[test]
fn an_other_writable_daemon_binary_is_refused_in_the_background_too() {
    let host = Host::new();
    fs::set_permissions(
        host.version_dir().join("pohunekd"),
        fs::Permissions::from_mode(0o757),
    )
    .expect("loosen the daemon");

    host.assert_refused(&host.run(&["daemon", "start", "--detach"]));
}

#[test]
fn a_missing_daemon_binary_is_refused() {
    let host = Host::new();
    fs::remove_file(host.version_dir().join("pohunekd")).expect("remove the daemon");

    host.assert_refused(&host.run(&["daemon", "start"]));
}

#[test]
fn a_linked_version_directory_is_refused() {
    let host = Host::new();
    let real = host.version_dir().with_file_name("elsewhere");
    fs::rename(host.version_dir(), &real).expect("move the version directory");
    std::os::unix::fs::symlink(&real, host.version_dir()).expect("link the version directory");

    host.assert_refused(&host.run(&["daemon", "start"]));
}
