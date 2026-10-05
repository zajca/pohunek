//! Binary-level contracts for the daemon-backed Codex and Claude
//! `integration doctor` and `integration uninstall` commands.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{ErrorClass, ProtocolError, Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// Mode of every private fixture directory.
const PRIVATE_MODE: u32 = 0o700;

struct Fixture {
    env: TestEnv,
    root: PathBuf,
}

impl Fixture {
    fn new(_tag: &str) -> Self {
        // `TestEnv` keeps the root short and canonical, so the fixture daemon
        // socket stays within the macOS 103-byte `sun_path` limit.
        let env = TestEnv::new().expect("create the hermetic test environment");
        let root = env.root().to_path_buf();
        let path = root.join("run/pohunek");
        fs::create_dir_all(&path).expect("create fixture directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(PRIVATE_MODE))
            .expect("private fixture directory");
        Self { env, root }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/pohunek/daemon.sock")
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(arguments)
            .output()
            .expect("run pohunek binary")
    }
}

/// Serves one request on the fixture socket and returns its method and params.
fn serve_once(socket: &Path, result: Value) -> thread::JoinHandle<(String, Value)> {
    let listener = UnixListener::bind(socket).expect("bind fake daemon");
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept CLI request");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read CLI request");
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse request");
        let response = Response::ok(PROTOCOL_VERSION, request.id(), result).expect("response");
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize response")
        )
        .expect("write response");
        (request.method().to_owned(), request.params().clone())
    })
}

/// Answers one request on the fixture socket with a typed error and returns
/// the request's method and params.
fn serve_error_once(socket: &Path, error: ProtocolError) -> thread::JoinHandle<(String, Value)> {
    let listener = UnixListener::bind(socket).expect("bind fake daemon");
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept CLI request");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read CLI request");
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse request");
        let response = Response::err(PROTOCOL_VERSION, request.id(), error).expect("response");
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize response")
        )
        .expect("write response");
        (request.method().to_owned(), request.params().clone())
    })
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("JSON envelope")
}

fn doctor_result(ok: bool) -> Value {
    let severity = if ok { "info" } else { "error" };
    json!({
        "ok": ok,
        "agents": [{
            "agent": "claude",
            "ok": ok,
            "status": {
                "agent": "claude",
                "available": true,
                "expected_asset_paths": [],
                "present_asset_paths": [],
                "registration_paths": [],
                "installed_version": null,
                "expected_version": protocol::EXPECTED_INTEGRATION_VERSION,
                "state": "outdated",
                "recovery": "reinstall",
                "warnings": []
            },
            "findings": [{
                "code": "asset_missing",
                "severity": severity,
                "summary": "managed state hook is missing",
                "remediation": "run `pohunek integration install --agent claude` on the daemon host"
            }]
        }]
    })
}

#[test]
fn doctor_sends_the_selector_and_fails_the_exit_code_on_error_findings() {
    let fixture = Fixture::new("doctor-fail");
    let server = serve_once(&fixture.socket(), doctor_result(false));

    let output = fixture.run(&["integration", "doctor", "--agent", "claude", "--json"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stdout_json(&output)["ok"]["agents"][0]["findings"][0]["code"],
        "asset_missing"
    );
    assert_eq!(
        server.join().expect("fake daemon"),
        (
            "integration.doctor".to_owned(),
            json!({ "agent": "claude" })
        )
    );
}

#[test]
fn doctor_without_an_agent_diagnoses_both_and_succeeds_when_healthy() {
    let fixture = Fixture::new("doctor-ok");
    let server = serve_once(&fixture.socket(), doctor_result(true));

    let output = fixture.run(&["integration", "doctor"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("human output");
    assert!(text.contains("claude: ok"), "{text}");
    assert!(text.contains("[info] asset_missing"), "{text}");
    assert!(
        text.contains("fix: run `pohunek integration install --agent claude`"),
        "{text}"
    );
    assert_eq!(
        server.join().expect("fake daemon"),
        ("integration.doctor".to_owned(), json!({}))
    );
}

#[test]
fn uninstall_targets_exactly_the_named_agent_and_renders_the_report() {
    let fixture = Fixture::new("uninstall");
    let server = serve_once(
        &fixture.socket(),
        json!({ "uninstalled": [{
            "agent": "codex",
            "state": "removed",
            "removed_paths": ["/h/.codex/pohunek-agent-state.sh"],
            "updated_paths": ["/h/.codex/hooks.json"],
            "preserved_paths": ["/h/.codex/pohunek-agent-notify.sh"]
        }] }),
    );

    let output = fixture.run(&["integration", "uninstall", "--agent", "codex"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("human output");
    assert!(text.contains("codex: removed managed hooks"), "{text}");
    assert!(
        text.contains("removed: /h/.codex/pohunek-agent-state.sh"),
        "{text}"
    );
    assert!(
        text.contains("updated registration: /h/.codex/hooks.json"),
        "{text}"
    );
    assert!(
        text.contains("preserved (not installer-owned): /h/.codex/pohunek-agent-notify.sh"),
        "{text}"
    );
    assert_eq!(
        server.join().expect("fake daemon"),
        (
            "integration.uninstall".to_owned(),
            json!({ "agent": "codex" })
        )
    );
}

#[test]
fn parser_keeps_the_hermes_target_requirement_and_scopes_hermes_flags() {
    let fixture = Fixture::new("parser");
    for action in ["doctor", "uninstall", "update"] {
        let output = fixture.run(&["integration", action, "--agent", "hermes", "--json"]);
        assert_eq!(output.status.code(), Some(2), "{action}");
        assert_eq!(stdout_json(&output)["err"]["code"], "cli_usage", "{action}");
    }

    let no_agent = fixture.run(&["integration", "uninstall", "--json"]);
    assert_eq!(no_agent.status.code(), Some(2));
    assert_eq!(stdout_json(&no_agent)["err"]["code"], "cli_usage");

    for action in ["doctor", "uninstall"] {
        let output = fixture.run(&[
            "integration",
            action,
            "--agent",
            "claude",
            "--hermes-profile",
            "default",
            "--json",
        ]);
        assert_eq!(output.status.code(), Some(1), "{action}");
        assert_eq!(
            stdout_json(&output)["err"]["code"],
            "integration_hermes_options_require_hermes",
            "{action}"
        );
    }

    let update = fixture.run(&["integration", "update", "--agent", "claude", "--json"]);
    assert_eq!(update.status.code(), Some(1));
    assert_eq!(
        stdout_json(&update)["err"]["code"],
        "integration_action_unsupported"
    );
}

#[test]
fn a_package_runtime_id_is_sent_to_the_daemon_as_the_selector() {
    let status_result = json!({ "agents": [doctor_result(true)["agents"][0]["status"].clone()] });
    for (action, result) in [("doctor", doctor_result(true)), ("status", status_result)] {
        let fixture = Fixture::new("package-runtime");
        let server = serve_once(&fixture.socket(), result);

        let output = fixture.run(&["integration", action, "--agent", "acme", "--json"]);

        assert_eq!(output.status.code(), Some(0), "{action}");
        assert_eq!(
            server.join().expect("fake daemon"),
            (format!("integration.{action}"), json!({ "agent": "acme" })),
            "{action}"
        );
    }
}

#[test]
fn an_unknown_runtime_id_is_refused_by_the_daemon_with_its_typed_error() {
    let fixture = Fixture::new("unknown-runtime");
    let server = serve_error_once(
        &fixture.socket(),
        ProtocolError::new(
            ErrorClass::Runtime,
            "runtime_not_installed",
            "runtime acme is not installed",
            None,
        ),
    );

    let output = fixture.run(&["integration", "uninstall", "--agent", "acme", "--json"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout_json(&output)["err"]["code"], "runtime_not_installed");
    assert_eq!(
        server.join().expect("fake daemon"),
        (
            "integration.uninstall".to_owned(),
            json!({ "agent": "acme" })
        )
    );
}

#[test]
fn a_value_outside_the_runtime_id_grammar_is_a_usage_error() {
    let fixture = Fixture::new("bad-runtime");
    for value in ["Not An Id", "a/b"] {
        let output = fixture.run(&["integration", "doctor", "--agent", value, "--json"]);
        assert_eq!(output.status.code(), Some(2), "{value}");
        assert_eq!(stdout_json(&output)["err"]["code"], "cli_usage", "{value}");
    }
}
