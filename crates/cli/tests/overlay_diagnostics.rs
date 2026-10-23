//! Binary-level scenarios of the overlay diagnostics: `pohunek doctor` over the
//! configured overlay, the daemon's overlay checks, the file-versus-daemon
//! comparison and the unreachable-peer error.
//!
//! Each scenario runs the real `pohunek` (and, where the daemon reports, the
//! real `pohunekd`) against a fake `netbird` in a hermetic home. The search path
//! holds only that fake, so no agent binary of the host leaks in.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

use pohunek_platform::supervisor::BOOTSTRAP_ENV_ALLOWLIST;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use pohunek_test_support::{bin_exe, bind_redirect, worker_binary};
use serde_json::Value;

#[path = "support/connect_redirect.rs"]
mod connect_redirect;

/// The address the fake `netbird` reports for this machine.
const SELF_ADDRESS: &str = "100.64.0.7";

/// The address the fake `netbird` reports for the remote peer.
const PEER_ADDRESS: &str = "100.64.0.8";

/// Port the file configures for the daemon scenarios. The daemon's overlay bind
/// is moved to an ephemeral loopback port, so the number never occupies a real
/// socket.
const FILE_PORT: u16 = 9731;

/// Port the file is changed to after the daemon started.
const CHANGED_PORT: u16 = 9732;

/// A daemon process, killed and reaped on drop.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A hermetic home whose executable search path holds a fake `netbird`, or
/// nothing when `with_netbird` is false.
struct Fixture {
    env: TestEnv,
    bin: PathBuf,
}

impl Fixture {
    fn new(with_netbird: bool) -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let bin = env.root().join("bin");
        fs::create_dir_all(&bin).expect("create bin");
        if with_netbird {
            let status = format!(
                "{{\"netbirdIp\":\"{SELF_ADDRESS}\",\"peers\":{{\"details\":[{{\
                 \"fqdn\":\"fixture-remote.netbird.test\",\"netbirdIp\":\"{PEER_ADDRESS}\",\
                 \"status\":\"Connected\"}}]}}}}"
            );
            pohunek_test_support::fs::write_executable(
                bin.join("netbird"),
                format!("#!/bin/sh\nprintf '%s\\n' '{status}'\n"),
            )
            .expect("write fake netbird");
        }
        Self { env, bin }
    }

    /// Writes an owner-private `overlays.toml` with `contents`.
    fn write_config(&self, contents: &str, mode: u32) {
        let dir = self.env.config_home().join("pohunek");
        fs::create_dir_all(&dir).expect("create the config directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .expect("make the config directory private");
        let file = dir.join("overlays.toml");
        fs::write(&file, contents).expect("write overlays.toml");
        fs::set_permissions(&file, fs::Permissions::from_mode(mode))
            .expect("set the overlays.toml mode");
    }

    fn pohunek(&self) -> Command {
        let mut command = self.env.command(bin_exe("pohunek"));
        command.env("PATH", &self.bin);
        command
    }

    /// The doctor checks as a JSON array.
    fn doctor(&self) -> Vec<Value> {
        let output = self
            .pohunek()
            .args(["doctor", "--json"])
            .output()
            .expect("run doctor");
        checks_of(&output)
    }
}

fn checks_of(output: &Output) -> Vec<Value> {
    let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "doctor stdout is one JSON document ({error}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    document["ok"]["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("doctor checks in {document}"))
        .clone()
}

fn check<'a>(checks: &'a [Value], name: &str) -> &'a Value {
    checks
        .iter()
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("check {name} in {checks:?}"))
}

fn detail(check: &Value) -> &str {
    check["detail"].as_str().expect("check detail")
}

fn names(checks: &[Value]) -> Vec<&str> {
    checks
        .iter()
        .map(|check| check["name"].as_str().expect("check name"))
        .collect()
}

/// Starts the real daemon the way a service job does: the bootstrap allowlist
/// and the search path only. The bind shim moves its overlay bind to loopback.
fn start_daemon(fixture: &Fixture) -> Daemon {
    let (bind_library, bind_capture) = bind_redirect::build(fixture.env.root());
    let mut daemon = Command::new(bin_exe("pohunek").with_file_name("pohunekd"));
    daemon.env_clear();
    for key in BOOTSTRAP_ENV_ALLOWLIST {
        if let Some(value) = fixture.env.environment().get(std::ffi::OsStr::new(key)) {
            daemon.env(key, value);
        }
    }
    let log =
        fs::File::create(fixture.env.root().join("daemon.out")).expect("create the daemon log");
    let child = daemon
        .env("PATH", &fixture.bin)
        .env("LD_PRELOAD", &bind_library)
        .env(bind_redirect::CAPTURE_ENV, &bind_capture)
        .env("SHELL", "/bin/sh")
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .env("POHUNEK_WORKER_BIN", worker_binary())
        .stdin(Stdio::null())
        .stdout(log.try_clone().expect("duplicate the log handle"))
        .stderr(log)
        .spawn()
        .expect("spawn pohunekd");
    Daemon(child)
}

/// Doctor checks once the daemon answers `daemon.doctor` with its overlay report.
fn doctor_of_running_daemon(fixture: &Fixture) -> Vec<Value> {
    poll_until("the daemon to report its overlay", || {
        let checks = fixture.doctor();
        checks
            .iter()
            .any(|check| check["name"] == "overlay_netbird_listener")
            .then_some(checks)
    })
}

/// With no `overlays.toml`, the default overlay keeps its `netbird_cli` check in
/// the same place, and its missing CLI is a warning.
#[test]
fn an_absent_file_checks_the_default_overlay_as_optional() {
    let fixture = Fixture::new(false);

    let checks = fixture.doctor();

    let netbird = check(&checks, "netbird_cli");
    assert_eq!(netbird["status"], "warn", "{netbird}");
    assert!(detail(netbird).contains("optional"), "{netbird}");
    let order = names(&checks);
    let position = order.iter().position(|name| *name == "netbird_cli");
    assert_eq!(
        position.map(|position| order[position - 1]),
        Some("log_dir_writable"),
        "{order:?}"
    );
    assert_eq!(
        position.map(|position| order[position + 1]),
        Some("schema_version"),
        "{order:?}"
    );
    let config = check(&checks, "overlay_config");
    assert_eq!(config["status"], "ok", "{config}");
    assert!(detail(config).contains("netbird on port 18722"), "{config}");
    assert!(detail(config).contains("no overlays.toml"), "{config}");
}

/// A provider listed in `overlays.toml` is required: its missing CLI fails the
/// `<overlay>_cli` check with the way to fix it.
#[test]
fn a_configured_overlay_without_its_cli_fails_with_a_fix_hint() {
    let fixture = Fixture::new(false);
    fixture.write_config("providers = [\"netbird\"]\n", 0o600);

    let checks = fixture.doctor();

    let netbird = check(&checks, "netbird_cli");
    assert_eq!(netbird["status"], "fail", "{netbird}");
    assert!(detail(netbird).contains("overlays.toml"), "{netbird}");
    assert!(detail(netbird).contains("install"), "{netbird}");
    let config = check(&checks, "overlay_config");
    assert_eq!(config["status"], "ok", "{config}");
    assert!(
        detail(config).contains("overlays.toml configures"),
        "{config}"
    );
}

/// A configured overlay whose CLI works reports the address it found.
#[test]
fn a_working_overlay_cli_reports_this_hosts_address() {
    let fixture = Fixture::new(true);
    fixture.write_config("providers = [\"netbird\"]\n", 0o600);

    let checks = fixture.doctor();

    let netbird = check(&checks, "netbird_cli");
    assert_eq!(netbird["status"], "ok", "{netbird}");
    assert!(detail(netbird).contains(SELF_ADDRESS), "{netbird}");
}

/// A file the loader refuses is reported by the doctor rather than aborting it,
/// and no provider is probed.
#[test]
fn an_unusable_file_fails_the_overlay_config_check() {
    let fixture = Fixture::new(true);
    fixture.write_config("providers = [\"netbird\"]\n", 0o644);

    let checks = fixture.doctor();

    let config = check(&checks, "overlay_config");
    assert_eq!(config["status"], "fail", "{config}");
    assert!(detail(config).contains("overlays.toml"), "{config}");
    assert!(
        !names(&checks).contains(&"netbird_cli"),
        "no provider is probed: {:?}",
        names(&checks)
    );
}

/// An unusable `POHUNEK_REMOTE_PORT` is named as the cause even though no file
/// exists, and no provider is probed.
#[test]
fn an_unusable_port_override_is_named_instead_of_blaming_a_file() {
    let fixture = Fixture::new(true);

    let output = fixture
        .pohunek()
        .env("POHUNEK_REMOTE_PORT", "not-a-port")
        .args(["doctor", "--json"])
        .output()
        .expect("run doctor");

    let checks = checks_of(&output);
    let config = check(&checks, "overlay_config");
    assert_eq!(config["status"], "fail", "{config}");
    assert!(detail(config).contains("POHUNEK_REMOTE_PORT"), "{config}");
    assert!(!detail(config).contains("remove the file"), "{config}");
    assert!(
        !names(&checks).contains(&"netbird_cli"),
        "{:?}",
        names(&checks)
    );
}

/// The daemon reports its overlay and listener state, and the CLI compares its
/// own file with it: a file changed after the daemon started is flagged.
#[test]
fn the_daemon_reports_its_overlay_and_the_cli_flags_a_changed_file() {
    let fixture = Fixture::new(true);
    fixture.write_config(
        &format!("providers = [\"netbird\"]\nport = {FILE_PORT}\n"),
        0o600,
    );
    let _daemon = start_daemon(&fixture);

    let checks = doctor_of_running_daemon(&fixture);

    let listener = check(&checks, "overlay_netbird_listener");
    assert_eq!(listener["status"], "ok", "{listener}");
    assert!(
        detail(listener).starts_with(&format!("port={FILE_PORT};")),
        "{listener}"
    );
    assert!(detail(listener).contains(SELF_ADDRESS), "{listener}");
    let config = check(&checks, "overlay_config");
    assert_eq!(config["status"], "ok", "{config}");
    assert!(
        detail(config).contains("as the running daemon serves"),
        "{config}"
    );

    fixture.write_config(
        &format!("providers = [\"netbird\"]\nport = {CHANGED_PORT}\n"),
        0o600,
    );
    let changed = fixture.doctor();

    let config = check(&changed, "overlay_config");
    assert_eq!(config["status"], "warn", "{config}");
    let text = detail(config);
    assert!(text.contains("restart the daemon to apply"), "{text}");
    assert!(text.contains(&FILE_PORT.to_string()), "{text}");
    assert!(text.contains(&CHANGED_PORT.to_string()), "{text}");
}

/// A shell-level `POHUNEK_REMOTE_PORT` changes the port the CLI would use, and
/// the warning says so instead of blaming the file.
#[test]
fn a_port_override_in_the_shell_is_named_in_the_mismatch() {
    let fixture = Fixture::new(true);
    fixture.write_config(
        &format!("providers = [\"netbird\"]\nport = {FILE_PORT}\n"),
        0o600,
    );
    let _daemon = start_daemon(&fixture);
    doctor_of_running_daemon(&fixture);

    let output = fixture
        .pohunek()
        .env("POHUNEK_REMOTE_PORT", CHANGED_PORT.to_string())
        .args(["doctor", "--json"])
        .output()
        .expect("run doctor");

    let checks = checks_of(&output);
    let config = check(&checks, "overlay_config");
    assert_eq!(config["status"], "warn", "{config}");
    assert!(detail(config).contains("POHUNEK_REMOTE_PORT"), "{config}");
    assert!(
        detail(config).contains("restart the daemon to apply"),
        "{config}"
    );
}

/// A refused connection to a resolved peer names the overlay, the address and
/// the port, and hints at the remote host's `overlays.toml`, in text and JSON.
#[test]
fn an_unreachable_peer_names_the_overlay_the_address_and_the_port() {
    let fixture = Fixture::new(true);
    let (redirect, capture) = connect_redirect::build(fixture.env.root());
    let run = |json: bool| {
        let mut command = fixture.pohunek();
        command
            .env("LD_PRELOAD", &redirect)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
            .env("POHUNEK_TEST_CONNECT_REFUSE", "1")
            .args(["--host", "fixture-remote", "session", "list"]);
        if json {
            command.arg("--json");
        }
        command.output().expect("run the remote CLI")
    };
    let target = format!("{PEER_ADDRESS}:{}", pohunek_client::DEFAULT_REMOTE_PORT);

    let json_output = run(true);

    assert_eq!(json_output.status.code(), Some(1));
    let document: Value = serde_json::from_slice(&json_output.stdout).expect("JSON error");
    assert_eq!(document["err"]["code"], "host_unreachable", "{document}");
    let message = document["err"]["msg"].as_str().expect("message");
    assert!(message.contains("netbird"), "{message}");
    assert!(message.contains(&target), "{message}");
    let recover = document["err"]["recover"].as_str().expect("recover hint");
    assert!(recover.contains("overlays.toml"), "{recover}");
    assert!(recover.contains("netbird"), "{recover}");

    let text_output = run(false);

    let stderr = String::from_utf8_lossy(&text_output.stderr);
    assert!(stderr.contains("netbird"), "{stderr}");
    assert!(stderr.contains(&target), "{stderr}");
    assert!(stderr.contains("overlays.toml"), "{stderr}");
}
