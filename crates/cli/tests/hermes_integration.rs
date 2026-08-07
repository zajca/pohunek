//! Binary-level Hermes and legacy integration lifecycle contracts.

#![cfg(unix)]

// Rust guideline compliant 2026-08-06

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use protocol::{AgentKind, Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "pohunek-hermes-process-{tag}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create fixture root");
        set_mode(&root, 0o700);
        Self { root }
    }

    fn private_directory(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(&path).expect("create private fixture directory");
        let mut current = self.root.clone();
        for component in Path::new(relative).components() {
            current.push(component.as_os_str());
            set_mode(&current, 0o700);
        }
        path
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pohunek"));
        command
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("HERMES_HOME", self.root.join("ambient-must-not-be-read"));
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _result = fs::remove_dir_all(&self.root);
    }
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set fixture mode");
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write executable");
    set_mode(path, 0o700);
}

fn run(fixture: &Fixture, arguments: &[&str]) -> Output {
    fixture
        .command()
        .args(arguments)
        .output()
        .expect("run pohunek binary")
}

fn parse_ok(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "status={:?}, stdout={}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).expect("parse JSON envelope");
    envelope["ok"].clone()
}

fn parse_error(output: &Output) -> Value {
    assert!(
        !output.status.success(),
        "expected command failure, stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
    let envelope: Value =
        serde_json::from_slice(&output.stdout).expect("parse JSON error envelope");
    envelope["err"].clone()
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the binary contract test intentionally exercises one complete isolated lifecycle"
)]
fn binary_hermes_lifecycle_is_isolated_and_model_free() {
    let fixture = Fixture::new("lifecycle");
    let home = fixture.private_directory("home");
    fixture.private_directory("home/.hermes");
    fixture.private_directory("state");
    fixture.private_directory("run");
    fixture.private_directory("data");
    fixture.private_directory("config");
    fixture.private_directory("cache");
    let venv_bin = fixture.private_directory("installation/venv/bin");
    let python_bin = fixture.private_directory("installation/python/bin");
    write_executable(&python_bin.join("python3"), "exec /usr/bin/python3 \"$@\"");
    symlink("../../python/bin/python3", venv_bin.join("python3")).expect("link Python runtime");

    let state_file = fixture.root.join("plugin-state");
    fs::write(&state_file, "disabled\n").expect("write plugin state");
    let plugin_root = home.join(".hermes/plugins/operators/pohunek");
    let hermes_body = format!(
        r#"case "$*" in
  --version) printf '%s\n' 'Hermes Agent v0.20.0 (2026.8.3)' ;;
  'plugins list --json')
    if [ -d '{plugin_root}' ]; then state=$(/bin/cat '{state_file}'); printf '[{{"name":"pohunek","status":"%s","source":"user"}}]\n' "$state"; else printf '%s\n' '[]'; fi ;;
  'plugins enable pohunek --no-allow-tool-override') printf '%s\n' enabled > '{state_file}' ;;
  'plugins disable pohunek') printf '%s\n' disabled > '{state_file}' ;;
  *) exit 90 ;;
esac"#,
        plugin_root = plugin_root.display(),
        state_file = state_file.display(),
    );
    let hermes_bin = venv_bin.join("hermes");
    write_executable(&hermes_bin, &hermes_body);
    let policy_cli = fixture.root.join("policy-pohunek");
    write_executable(
        &policy_cli,
        r#"printf '%s\n' '{"cli_version":"fixture","protocol":{"minimum":2,"maximum":2},"ok":{}}'"#,
    );

    let hermes = hermes_bin.to_str().expect("Hermes UTF-8 path");
    let pohunek = policy_cli.to_str().expect("Pohunek UTF-8 path");

    let absent = parse_ok(&run(
        &fixture,
        &[
            "integration",
            "status",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            hermes,
            "--json",
        ],
    ));
    assert_eq!(absent["installed"], false);

    let wildcard_error = parse_error(&run(
        &fixture,
        &[
            "integration",
            "install",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            hermes,
            "--pohunek-bin",
            pohunek,
            "--access-mode",
            "manage",
            "--allow-host",
            "*",
            "--json",
        ],
    ));
    assert_eq!(
        wildcard_error["code"],
        "hermes_wildcard_confirmation_required"
    );

    let install = run(
        &fixture,
        &[
            "integration",
            "install",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            hermes,
            "--pohunek-bin",
            pohunek,
            "--access-mode",
            "manage",
            "--allow-host",
            "local",
            "--json",
        ],
    );
    let installed = parse_ok(&install);
    assert_eq!(installed["installed"], true);
    assert_eq!(installed["modified"], false);
    assert!(!plugin_root.join("__pycache__").exists());

    let status_arguments = [
        "integration",
        "status",
        "--agent",
        "hermes",
        "--hermes-profile",
        "default",
        "--hermes-bin",
        hermes,
    ];
    let status = run(&fixture, &status_arguments);
    assert!(
        status.status.success(),
        "status={:?}, stdout={}, stderr={}",
        status.status.code(),
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let status_text = String::from_utf8(status.stdout).expect("status text");
    assert!(status_text.contains("installed=true, enabled=true, modified=false"));
    let mut status_json_arguments = status_arguments.to_vec();
    status_json_arguments.push("--json");
    let status_json = parse_ok(&run(&fixture, &status_json_arguments));
    assert_eq!(status_json["modified"], false);

    let policy_path = fs::read_dir(fixture.root.join("state/pohunek/policies/hermes"))
        .expect("read policy directory")
        .next()
        .expect("installed policy entry")
        .expect("read policy entry")
        .path();
    let policy_bytes = fs::read(&policy_path).expect("read installed policy");
    fs::remove_file(&policy_path).expect("remove policy for status check");
    assert_eq!(
        parse_error(&run(&fixture, &status_json_arguments))["code"],
        "hermes_io_failed"
    );
    fs::write(&policy_path, b"{invalid-policy").expect("write corrupt policy");
    set_mode(&policy_path, 0o600);
    assert_eq!(
        parse_error(&run(&fixture, &status_json_arguments))["code"],
        "hermes_invalid_policy"
    );
    fs::write(&policy_path, policy_bytes).expect("restore installed policy");
    set_mode(&policy_path, 0o600);

    let update = parse_ok(&run(
        &fixture,
        &[
            "integration",
            "update",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            hermes,
            "--access-mode",
            "full",
            "--json",
        ],
    ));
    assert_eq!(update["modified"], false);
    assert_eq!(update["access_mode"], "full");

    fs::write(plugin_root.join("tools.py"), b"# explicitly modified\n")
        .expect("mutate managed plugin");
    let modified_update = [
        "integration",
        "update",
        "--agent",
        "hermes",
        "--hermes-profile",
        "default",
        "--hermes-bin",
        hermes,
        "--json",
    ];
    assert_eq!(
        parse_error(&run(&fixture, &modified_update))["code"],
        "hermes_modified_confirmation_required"
    );
    let mut confirmed_update = modified_update.to_vec();
    confirmed_update.insert(confirmed_update.len() - 1, "--confirm-modified");
    assert_eq!(
        parse_ok(&run(&fixture, &confirmed_update))["modified"],
        true
    );
    assert_eq!(
        parse_ok(&run(&fixture, &status_json_arguments))["modified"],
        false
    );

    let doctor = run(
        &fixture,
        &[
            "integration",
            "doctor",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-bin",
            hermes,
            "--json",
        ],
    );
    assert!(
        doctor.status.success(),
        "status={:?}, stdout={}, stderr={}",
        doctor.status.code(),
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    let doctor_ok = parse_ok(&doctor);
    assert_eq!(doctor_ok["doctor"]["ok"], true);
    assert_eq!(
        doctor_ok["doctor"]["checks"].as_array().map(Vec::len),
        Some(15)
    );

    fs::write(plugin_root.join("hooks.py"), b"# explicitly modified\n")
        .expect("mutate managed plugin before uninstall");
    let uninstall_arguments = [
        "integration",
        "uninstall",
        "--agent",
        "hermes",
        "--hermes-profile",
        "default",
        "--hermes-bin",
        hermes,
        "--json",
    ];
    assert_eq!(
        parse_error(&run(&fixture, &uninstall_arguments))["code"],
        "hermes_modified_confirmation_required"
    );
    let mut confirmed_uninstall = uninstall_arguments.to_vec();
    confirmed_uninstall.insert(confirmed_uninstall.len() - 1, "--confirm-modified");
    let uninstall = parse_ok(&run(&fixture, &confirmed_uninstall));
    assert_eq!(uninstall["installed"], false);
}

#[test]
fn binary_legacy_install_preserves_daemon_rpc_for_each_selector() {
    let fixture = Fixture::new("legacy-rpc");
    for directory in ["home", "state", "run/pohunek", "data", "config", "cache"] {
        fixture.private_directory(directory);
    }
    let socket = fixture.root.join("run/pohunek/daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind fake daemon");
    let server = thread::spawn(move || {
        let mut captured = Vec::new();
        for _ in 0..3 {
            let (stream, _) = listener.accept().expect("accept CLI request");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read CLI request");
            let request: Request = serde_json::from_str(line.trim_end()).expect("parse request");
            let agent = request.params()["agent"].as_str();
            let report_agent = match agent {
                Some("codex") | None => AgentKind::Codex,
                Some("claude") => AgentKind::Claude,
                other => panic!("unexpected agent: {other:?}"),
            };
            captured.push(request.params().clone());
            let response = Response::ok(
                PROTOCOL_VERSION,
                request.id(),
                json!({
                    "installed": [{
                        "agent": report_agent,
                        "hook_path": "/redacted/fixture-hook",
                        "config_paths": ["/redacted/fixture-config"]
                    }]
                }),
            )
            .expect("build daemon response");
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::to_string(&response).expect("serialize response")
            )
            .expect("write daemon response");
        }
        captured
    });

    for selector in [None, Some("codex"), Some("claude")] {
        let mut arguments = vec!["integration", "install"];
        if let Some(agent) = selector {
            arguments.extend(["--agent", agent]);
        }
        let output = run(&fixture, &arguments);
        assert!(
            output.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8(output.stdout)
            .expect("legacy output")
            .contains("installed"));
    }
    assert_eq!(
        server.join().expect("fake daemon"),
        [
            json!({}),
            json!({"agent": "codex"}),
            json!({"agent": "claude"})
        ]
    );
}

#[test]
fn binary_parser_rejects_missing_and_conflicting_hermes_targets() {
    let fixture = Fixture::new("parser");
    for action in ["status", "doctor", "update", "uninstall"] {
        let missing = run(
            &fixture,
            &["integration", action, "--agent", "hermes", "--json"],
        );
        assert_eq!(missing.status.code(), Some(2), "{action}");
        let envelope: Value = serde_json::from_slice(&missing.stdout).expect("usage JSON");
        assert_eq!(envelope["err"]["code"], "cli_usage");
    }
    let both = run(
        &fixture,
        &[
            "integration",
            "status",
            "--agent",
            "hermes",
            "--hermes-profile",
            "default",
            "--hermes-home",
            "/private/hermes",
            "--json",
        ],
    );
    assert_eq!(both.status.code(), Some(2));
    let envelope: Value = serde_json::from_slice(&both.stdout).expect("conflict JSON");
    assert_eq!(envelope["err"]["code"], "cli_usage");
}
