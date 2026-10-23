//! Binary-level scenarios of the owner-private overlay configuration
//! (`<config>/pohunek/overlays.toml`).
//!
//! Each scenario runs the real `pohunek` binary against a fake `netbird` on
//! `PATH`, in a hermetic home. The fake appends one line to a call log per
//! invocation, so a scenario can prove that a configuration failure happens
//! before any provider is consulted.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use pohunek_test_support::env::TestEnv;
use serde_json::Value;

#[path = "support/connect_redirect.rs"]
mod connect_redirect;

/// Peer address the fake `netbird` reports for `fixture-remote`.
const PEER_ADDRESS: &str = "100.64.0.42";

/// Code of every configuration failure of `overlays.toml`.
const INVALID_CODE: &str = "overlay_configuration_invalid";

/// Port the file configures in the scenarios that need a value other than the
/// default.
const FILE_PORT: u16 = 9100;

/// Port the environment override sets in the precedence scenario.
const ENV_PORT: u16 = 9200;

/// Size of a file past the loader's 64 KiB ceiling.
const OVERSIZED_FILE_BYTES: usize = 70 * 1024;

/// One unusable file: label, file bytes, fragments the message must carry, and
/// values from the file it must not quote.
type Case = (
    &'static str,
    Vec<u8>,
    &'static [&'static str],
    &'static [&'static str],
);

/// A hermetic home with a fake `netbird` and a call log.
struct Fixture {
    env: TestEnv,
    calls: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let bin = env.root().join("bin");
        fs::create_dir_all(&bin).expect("create bin");
        let status = format!(
            "{{\"peers\":{{\"details\":[{{\"fqdn\":\"fixture-remote.netbird.test\",\
             \"netbirdIp\":\"{PEER_ADDRESS}\",\"status\":\"Connected\"}}]}}}}"
        );
        pohunek_test_support::fs::write_executable(
            bin.join("netbird"),
            format!(
                "#!/bin/sh\nprintf 'invoked\\n' >> \"$POHUNEK_TEST_NETBIRD_CALLS\"\n\
                 printf '%s\\n' '{status}'\n"
            ),
        )
        .expect("write fake netbird");
        let calls = env.root().join("netbird-calls.log");
        Self { env, calls }
    }

    /// `<config>/pohunek`, where the loader looks.
    fn config_dir(&self) -> PathBuf {
        self.env.config_home().join("pohunek")
    }

    /// Creates the config directory with `mode`.
    fn make_config_dir(&self, mode: u32) -> PathBuf {
        let dir = self.config_dir();
        fs::create_dir_all(&dir).expect("create the config directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode))
            .expect("set the config directory mode");
        dir
    }

    /// Writes `overlays.toml` with `mode` into an owner-private config directory.
    fn write_config(&self, contents: impl AsRef<[u8]>, mode: u32) -> PathBuf {
        let dir = self.make_config_dir(0o700);
        let path = dir.join("overlays.toml");
        fs::write(&path, contents).expect("write overlays.toml");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
            .expect("set the overlays.toml mode");
        path
    }

    fn command(&self) -> Command {
        let mut command = self.env.command(pohunek_test_support::bin_exe("pohunek"));
        command
            .env("PATH", self.env.root().join("bin"))
            .env("POHUNEK_TEST_NETBIRD_CALLS", &self.calls);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .expect("run the pohunek CLI")
    }

    fn netbird_was_consulted(&self) -> bool {
        self.calls.exists()
    }
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is one JSON document ({error}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Asserts a typed configuration failure and returns its message.
fn assert_invalid(output: &Output, config_path: &Path) -> String {
    assert_eq!(output.status.code(), Some(1), "typed failures exit with 1");
    let document = json(output);
    assert_eq!(document["err"]["code"], INVALID_CODE, "{document}");
    assert_eq!(document["err"]["class"], "configuration", "{document}");
    let message = document["err"]["msg"].as_str().expect("error message");
    assert!(
        message.contains(&config_path.display().to_string()),
        "the message names the file: {message}"
    );
    message.to_owned()
}

fn host_records(output: &Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "host list failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    json(output)["ok"]
        .as_array()
        .expect("host records")
        .iter()
        .filter(|record| record.get("local").is_none())
        .cloned()
        .collect()
}

fn captured_targets(capture: &Path) -> Vec<String> {
    fs::read_to_string(capture)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Scenario 1: with no file, the host works over `NetBird` on the default port.
#[test]
fn an_absent_file_is_the_netbird_default() {
    let fixture = Fixture::new();
    let (redirect, capture) = connect_redirect::build(fixture.env.root());
    let output = fixture
        .command()
        .env("LD_PRELOAD", redirect)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["host", "list", "--json"])
        .output()
        .expect("run host list");
    let records = host_records(&output);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["overlay"], "netbird");
    assert_eq!(records[0]["port"], pohunek_client::DEFAULT_REMOTE_PORT);
    assert!(
        captured_targets(&capture).contains(&format!(
            "{PEER_ADDRESS}:{}",
            pohunek_client::DEFAULT_REMOTE_PORT
        )),
        "the default port reaches connect: {:?}",
        captured_targets(&capture)
    );
}

/// Scenario 2: the file's port reaches the records and the connect target, and
/// a discovery cache written under another port is not served.
#[test]
fn the_file_port_reaches_the_client_and_invalidates_the_cache() {
    let fixture = Fixture::new();
    let (redirect, capture) = connect_redirect::build(fixture.env.root());

    // A first run without a file caches a snapshot under the default port.
    let first = fixture
        .command()
        .env("LD_PRELOAD", &redirect)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["host", "list", "--json"])
        .output()
        .expect("run host list");
    let cached = host_records(&first);
    assert_eq!(cached[0]["port"], pohunek_client::DEFAULT_REMOTE_PORT);
    fs::remove_file(&capture).expect("the first run dialed the peer");

    fixture.write_config(
        format!("providers = [\"netbird\"]\nport = {FILE_PORT}\n"),
        0o600,
    );
    let output = fixture
        .command()
        .env("LD_PRELOAD", redirect)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["host", "list", "--json"])
        .output()
        .expect("run host list");
    let records = host_records(&output);
    assert_eq!(records[0]["port"], FILE_PORT, "{records:?}");
    assert!(
        captured_targets(&capture).contains(&format!("{PEER_ADDRESS}:{FILE_PORT}")),
        "the file port reaches connect: {:?}",
        captured_targets(&capture)
    );
}

/// Scenario 3: `POHUNEK_REMOTE_PORT` wins over the file's port.
#[test]
fn the_environment_override_wins_over_the_file() {
    let fixture = Fixture::new();
    let (redirect, capture) = connect_redirect::build(fixture.env.root());
    fixture.write_config(
        format!("providers = [\"netbird\"]\nport = {FILE_PORT}\n"),
        0o600,
    );
    let output = fixture
        .command()
        .env("LD_PRELOAD", redirect)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", ENV_PORT.to_string())
        .args(["host", "list", "--json"])
        .output()
        .expect("run host list");
    let records = host_records(&output);
    assert_eq!(records[0]["port"], ENV_PORT, "{records:?}");
    let targets = captured_targets(&capture);
    assert!(
        targets.contains(&format!("{PEER_ADDRESS}:{ENV_PORT}")),
        "the override reaches connect: {targets:?}"
    );
    assert!(
        !targets.contains(&format!("{PEER_ADDRESS}:{FILE_PORT}")),
        "the file port is not dialed: {targets:?}"
    );
}

/// Scenario 4, content failures: every one is `overlay_configuration_invalid`,
/// names the file, quotes no file value, and is reported before any provider
/// call.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one flat row per case keeps the failure table in one place"
)]
fn an_unusable_file_content_fails_before_any_provider_call() {
    let oversized = format!("# {}\n", "x".repeat(OVERSIZED_FILE_BYTES));
    let cases: Vec<Case> = vec![
        (
            "unknown id",
            b"providers = [\"zzz-unknown\"]\n".to_vec(),
            &["unknown overlay id", "netbird"],
            &["zzz-unknown"],
        ),
        (
            "tailscale is unknown in this release",
            b"providers = [\"tailscale\"]\n".to_vec(),
            &["unknown overlay id"],
            &["tailscale"],
        ),
        (
            "duplicate",
            b"providers = [\"netbird\", \"netbird\"]\n".to_vec(),
            &["more than one overlay"],
            &[],
        ),
        (
            "empty list",
            b"providers = []\n".to_vec(),
            &["exactly one overlay provider"],
            &[],
        ),
        (
            "two providers",
            b"providers = [\"netbird\", \"tailscale\"]\n".to_vec(),
            &["more than one overlay", "#1113"],
            &["tailscale"],
        ),
        (
            "two entries, one of them not a string",
            b"providers = [\"netbird\", 1]\n".to_vec(),
            &["more than one overlay", "#1113"],
            &[],
        ),
        (
            "port zero",
            b"providers = [\"netbird\"]\nport = 0\n".to_vec(),
            &["port must be a number in 1..=65535"],
            &[],
        ),
        (
            "port above the range",
            b"providers = [\"netbird\"]\nport = 65536\n".to_vec(),
            &["port must be a number in 1..=65535"],
            &["65536"],
        ),
        (
            "port as a string",
            b"providers = [\"netbird\"]\nport = \"9100\"\n".to_vec(),
            &["line 2"],
            &["9100"],
        ),
        (
            "unknown key",
            b"providers = [\"netbird\"]\nsecret-looking-key = 1\n".to_vec(),
            &["line 2"],
            &["secret-looking-key"],
        ),
        (
            "wrong type",
            b"providers = \"netbird\"\n".to_vec(),
            &["line 1"],
            &[],
        ),
        (
            "missing providers",
            b"port = 9100\n".to_vec(),
            &["overlays schema"],
            &[],
        ),
        ("not utf-8", vec![0xff, 0xfe, b'\n'], &["not UTF-8"], &[]),
        (
            "over 64 KiB",
            oversized.into_bytes(),
            &["exceeds 65536 bytes"],
            &[],
        ),
    ];
    for (label, bytes, required, forbidden) in cases {
        let fixture = Fixture::new();
        let path = fixture.write_config(&bytes, 0o600);
        let output = fixture.run(&["host", "list", "--json"]);
        let message = assert_invalid(&output, &path);
        for fragment in required {
            assert!(
                message.contains(fragment),
                "{label}: the message carries {fragment:?}: {message}"
            );
        }
        for value in forbidden {
            assert!(
                !message.contains(value),
                "{label}: the message quotes no file value {value:?}: {message}"
            );
        }
        assert!(
            !fixture.netbird_was_consulted(),
            "{label}: the provider is not consulted"
        );
    }
}

/// Scenario 4, trust failures: the file and its directory are checked before
/// the content is read.
#[test]
fn an_untrusted_file_or_directory_fails_before_any_provider_call() {
    let valid = "providers = [\"netbird\"]\n";

    // Mode 0644 on the file.
    let fixture = Fixture::new();
    let path = fixture.write_config(valid, 0o644);
    assert_invalid(&fixture.run(&["host", "list", "--json"]), &path);
    assert!(!fixture.netbird_was_consulted(), "0644 file");

    // Directory writable by the group or others, with the file present.
    for mode in [0o777, 0o770, 0o707] {
        let fixture = Fixture::new();
        let path = fixture.write_config(valid, 0o600);
        fs::set_permissions(fixture.config_dir(), fs::Permissions::from_mode(mode))
            .expect("loosen the config directory");
        assert_invalid(&fixture.run(&["host", "list", "--json"]), &path);
        assert!(
            !fixture.netbird_was_consulted(),
            "directory {mode:o} with a file"
        );
    }

    // A symlink is never followed, even to a valid private file.
    let fixture = Fixture::new();
    let dir = fixture.make_config_dir(0o700);
    let target = dir.join("real.toml");
    fs::write(&target, valid).expect("write the link target");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("private target");
    let link = dir.join("overlays.toml");
    symlink(&target, &link).expect("create the symlink");
    assert_invalid(&fixture.run(&["host", "list", "--json"]), &link);
    assert!(!fixture.netbird_was_consulted(), "symlink");

    // A dangling symlink is something at the path, not an absent file.
    let fixture = Fixture::new();
    let dir = fixture.make_config_dir(0o700);
    let dangling = dir.join("overlays.toml");
    symlink(dir.join("missing-target.toml"), &dangling).expect("create the dangling symlink");
    assert_invalid(&fixture.run(&["host", "list", "--json"]), &dangling);
    assert!(!fixture.netbird_was_consulted(), "dangling symlink");

    // A FIFO is refused without blocking on an open.
    let fixture = Fixture::new();
    let dir = fixture.make_config_dir(0o700);
    let fifo = dir.join("overlays.toml");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo");
    fs::set_permissions(&fifo, fs::Permissions::from_mode(0o600)).expect("private fifo");
    assert_invalid(&fixture.run(&["host", "list", "--json"]), &fifo);
    assert!(!fixture.netbird_was_consulted(), "fifo");
}

/// Scenario 4, environment: an unusable `POHUNEK_REMOTE_PORT` is reported with
/// the same code while a valid file exists, and the file stays validated when
/// the override is valid.
#[test]
fn the_environment_override_and_the_file_are_both_validated() {
    let fixture = Fixture::new();
    let path = fixture.write_config("providers = [\"netbird\"]\n", 0o600);
    let output = fixture
        .command()
        .env("POHUNEK_REMOTE_PORT", "0")
        .args(["host", "list", "--json"])
        .output()
        .expect("run host list");
    let message = assert_invalid(&output, &path);
    assert!(message.contains("POHUNEK_REMOTE_PORT"), "{message}");
    assert!(!fixture.netbird_was_consulted());

    // A valid override does not excuse an invalid file.
    let fixture = Fixture::new();
    let path = fixture.write_config("providers = []\n", 0o600);
    let output = fixture
        .command()
        .env("POHUNEK_REMOTE_PORT", ENV_PORT.to_string())
        .args(["host", "list", "--json"])
        .output()
        .expect("run host list");
    assert_invalid(&output, &path);
    assert!(!fixture.netbird_was_consulted());
}

/// Scenario 5: the machine's own-name lookup does not swallow the failure.
#[test]
fn a_host_flag_with_an_invalid_file_reports_the_configuration_error() {
    let fixture = Fixture::new();
    let path = fixture.write_config("providers = []\n", 0o600);
    let output = fixture.run(&["--host", "somepeer", "session", "list", "--json"]);
    assert_invalid(&output, &path);
    assert!(!fixture.netbird_was_consulted());
}

/// No file means no configuration to trust: an unsafe directory does not
/// block the `NetBird` default.
#[test]
fn an_absent_file_under_an_unsafe_directory_uses_the_defaults() {
    // Group- and world-writable config directory.
    for mode in [0o770, 0o757, 0o777] {
        let fixture = Fixture::new();
        fixture.make_config_dir(mode);
        let records = host_records(&fixture.run(&["host", "list", "--json"]));
        assert_eq!(records[0]["overlay"], "netbird", "directory {mode:o}");
        assert_eq!(
            records[0]["port"],
            pohunek_client::DEFAULT_REMOTE_PORT,
            "directory {mode:o}"
        );
    }

    // The environment override still applies on top of the defaults.
    let fixture = Fixture::new();
    fixture.make_config_dir(0o777);
    let output = fixture
        .command()
        .env("POHUNEK_REMOTE_PORT", ENV_PORT.to_string())
        .args(["host", "list", "--json"])
        .output()
        .expect("run host list");
    assert_eq!(host_records(&output)[0]["port"], ENV_PORT);

    // The config home reached through a symlink (stow, home-manager).
    let fixture = Fixture::new();
    fixture.make_config_dir(0o700);
    let link = fixture.env.root().join("config-link");
    symlink(fixture.env.config_home(), &link).expect("symlink the config home");
    let output = fixture
        .command()
        .env("XDG_CONFIG_HOME", &link)
        .args(["host", "list", "--json"])
        .output()
        .expect("run host list");
    let records = host_records(&output);
    assert_eq!(records[0]["overlay"], "netbird");
    assert_eq!(records[0]["port"], pohunek_client::DEFAULT_REMOTE_PORT);
}

/// A present file is trusted only through a safe path: a valid file below a
/// symlinked config home is refused.
#[test]
fn a_present_file_under_a_symlinked_config_home_is_refused() {
    let fixture = Fixture::new();
    fixture.write_config("providers = [\"netbird\"]\n", 0o600);
    let link = fixture.env.root().join("config-link");
    symlink(fixture.env.config_home(), &link).expect("symlink the config home");
    let output = fixture
        .command()
        .env("XDG_CONFIG_HOME", &link)
        .args(["host", "list", "--json"])
        .output()
        .expect("run host list");
    assert_invalid(&output, &link.join("pohunek/overlays.toml"));
    assert!(!fixture.netbird_was_consulted());
}
