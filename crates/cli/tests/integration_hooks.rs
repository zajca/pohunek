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

/// Answers each request on one connection with the next scripted outcome and
/// returns every request's method and params once the client disconnects.
fn serve_script(
    socket: &Path,
    script: Vec<Result<Value, ProtocolError>>,
) -> thread::JoinHandle<Vec<(String, Value)>> {
    let listener = UnixListener::bind(socket).expect("bind fake daemon");
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept CLI connection");
        let mut reader = BufReader::new(stream);
        let mut seen = Vec::new();
        for outcome in script {
            let mut line = String::new();
            if reader.read_line(&mut line).expect("read CLI request") == 0 {
                break;
            }
            let request: Request = serde_json::from_str(line.trim_end()).expect("parse request");
            let response = match outcome {
                Ok(result) => Response::ok(PROTOCOL_VERSION, request.id(), result),
                Err(error) => Response::err(PROTOCOL_VERSION, request.id(), error),
            }
            .expect("response");
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::to_string(&response).expect("serialize response")
            )
            .expect("write response");
            seen.push((request.method().to_owned(), request.params().clone()));
        }
        // A client that sends more than the script answers shows up here.
        let mut extra = String::new();
        while reader.read_line(&mut extra).unwrap_or(0) > 0 {
            let request: Request = serde_json::from_str(extra.trim_end()).expect("parse request");
            seen.push((request.method().to_owned(), request.params().clone()));
            extra.clear();
        }
        seen
    })
}

/// A status result of a daemon that honors the home selectors.
fn selector_status_result() -> Value {
    json!({ "agents": [], "home_selectors": true })
}

/// A status result of a daemon that predates the home selectors: it carries no
/// marker, and an install of such a daemon ignores the selector.
fn legacy_status_result() -> Value {
    json!({ "agents": [] })
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

/// The `home` label of a report for the profile home `name`.
fn profile_home(name: &str) -> Value {
    json!({ "profiles": [name] })
}

#[test]
fn install_sends_the_profile_selector_and_labels_the_home() {
    let fixture = Fixture::new("install-profile");
    let server = serve_script(
        &fixture.socket(),
        vec![
            Ok(selector_status_result()),
            Ok(json!({ "installed": [{
                "agent": "claude",
                "hook_path": "/p/work/hooks/pohunek-agent-state.sh",
                "config_paths": ["/p/work/settings.json"],
                "cleanup_incomplete": [],
                "home": profile_home("work")
            }] })),
        ],
    );

    let output = fixture.run(&[
        "integration",
        "install",
        "--agent",
        "claude",
        "--profile",
        "work",
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("human output");
    assert!(
        text.contains("installed claude hook (profile work): /p/work/hooks/pohunek-agent-state.sh"),
        "{text}"
    );
    assert_eq!(
        server.join().expect("fake daemon"),
        vec![
            (
                "integration.status".to_owned(),
                json!({ "agent": "claude", "profile": "work" })
            ),
            (
                "integration.install".to_owned(),
                json!({ "agent": "claude", "profile": "work" })
            ),
        ]
    );
}

#[test]
fn install_all_profiles_exits_non_zero_when_any_home_failed_and_names_it() {
    let fixture = Fixture::new("install-all-failed");
    let server = serve_script(
        &fixture.socket(),
        vec![
            Ok(selector_status_result()),
            Ok(json!({
                "installed": [{
                    "agent": "claude",
                    "hook_path": "/p/work/hooks/pohunek-agent-state.sh",
                    "config_paths": [],
                    "cleanup_incomplete": [],
                    "home": profile_home("work")
                }],
                "failed": [{
                    "agent": "claude",
                    "home": profile_home("personal"),
                    "error": {
                        "class": "runtime",
                        "code": "integration_settings_invalid",
                        "msg": "settings.json is not valid JSON",
                        "recover": "repair the file"
                    }
                }]
            })),
        ],
    );

    let output = fixture.run(&["integration", "install", "--all-profiles"]);

    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).expect("human output");
    assert!(
        text.contains("installed claude hook (profile work)"),
        "{text}"
    );
    assert!(
        text.contains(
            "failed claude (profile personal): integration_settings_invalid: settings.json is not valid JSON"
        ),
        "{text}"
    );
    assert!(text.contains("  fix: repair the file"), "{text}");
    let seen = server.join().expect("fake daemon");
    assert_eq!(
        seen.iter()
            .map(|(method, _)| method.as_str())
            .collect::<Vec<_>>(),
        ["integration.status", "integration.install"]
    );
    assert_eq!(seen[1].1, json!({ "all_profiles": true }));
}

#[test]
fn uninstall_all_profiles_exits_non_zero_when_any_home_failed() {
    let fixture = Fixture::new("uninstall-all-failed");
    let server = serve_script(
        &fixture.socket(),
        vec![
            Ok(selector_status_result()),
            Ok(json!({
                "uninstalled": [],
                "failed": [{
                    "agent": "claude",
                    "home": { "bare": true },
                    "error": {
                        "class": "configuration",
                        "code": "agent_config_dir_is_symlink",
                        "msg": "the config directory is a symlink"
                    }
                }]
            })),
        ],
    );

    let output = fixture.run(&[
        "integration",
        "uninstall",
        "--agent",
        "claude",
        "--all-profiles",
    ]);

    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).expect("human output");
    assert!(
        text.contains("failed claude (default): agent_config_dir_is_symlink"),
        "{text}"
    );
    let seen = server.join().expect("fake daemon");
    assert_eq!(seen[0].0, "integration.status");
    assert_eq!(
        seen[1],
        (
            "integration.uninstall".to_owned(),
            json!({ "agent": "claude", "all_profiles": true })
        )
    );
}

#[test]
fn status_and_doctor_send_the_selectors_and_render_the_home() {
    let mut status = doctor_result(true)["agents"][0]["status"].clone();
    status["home"] =
        json!({ "profiles": ["work"], "selector": { "kind": "profile", "name": "work" } });
    let fixture = Fixture::new("status-profile");
    let server = serve_once(
        &fixture.socket(),
        json!({ "agents": [status], "home_selectors": true }),
    );

    let output = fixture.run(&["integration", "status", "--profile", "work"]);

    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("human output");
    assert!(text.contains("  home: profile work"), "{text}");
    assert!(
        text.contains("run `pohunek integration install --agent claude --profile work`"),
        "{text}"
    );
    assert_eq!(
        server.join().expect("fake daemon"),
        (
            "integration.status".to_owned(),
            json!({ "profile": "work" })
        )
    );

    let mut result = doctor_result(true);
    result["agents"][0]["home"] = profile_home("work");
    result["home_selectors"] = json!(true);
    let fixture = Fixture::new("doctor-all");
    let server = serve_once(&fixture.socket(), result);

    let output = fixture.run(&["integration", "doctor", "--all-profiles"]);

    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("human output");
    assert!(text.contains("claude (profile work): ok"), "{text}");
    assert_eq!(
        server.join().expect("fake daemon"),
        (
            "integration.doctor".to_owned(),
            json!({ "all_profiles": true })
        )
    );
}

#[test]
fn the_home_selectors_exclude_each_other_and_the_hermes_selectors() {
    let fixture = Fixture::new("selector-conflicts");
    let both = fixture.run(&[
        "integration",
        "install",
        "--profile",
        "work",
        "--all-profiles",
        "--json",
    ]);
    assert_eq!(both.status.code(), Some(2));
    assert_eq!(stdout_json(&both)["err"]["code"], "cli_usage");

    for action in ["install", "status", "doctor", "uninstall"] {
        for selector in [["--profile", "work"], ["--all-profiles", ""]] {
            let mut arguments = vec![
                "integration",
                action,
                "--agent",
                "hermes",
                "--hermes-profile",
                "default",
                "--json",
                selector[0],
            ];
            if !selector[1].is_empty() {
                arguments.push(selector[1]);
            }
            let output = fixture.run(&arguments);
            assert_eq!(output.status.code(), Some(1), "{action} {selector:?}");
            assert_eq!(
                stdout_json(&output)["err"]["code"],
                "integration_profile_not_for_hermes",
                "{action} {selector:?}"
            );
        }
    }
}

#[test]
fn a_daemon_that_ignores_the_selectors_receives_no_mutating_request() {
    let legacy_rejects = ProtocolError::new(
        ErrorClass::Daemon,
        "bad_request",
        "invalid params for integration.status: unknown field `profile`",
        None,
    );
    let probes: [(&str, Vec<&str>, Result<Value, ProtocolError>); 6] = [
        (
            "install",
            vec!["install", "--agent", "claude", "--profile", "work"],
            Ok(legacy_status_result()),
        ),
        (
            "install-all",
            vec!["install", "--all-profiles"],
            Ok(legacy_status_result()),
        ),
        (
            "uninstall",
            vec!["uninstall", "--agent", "claude", "--all-profiles"],
            Ok(legacy_status_result()),
        ),
        (
            "install-rejected",
            vec!["install", "--profile", "work"],
            Err(legacy_rejects),
        ),
        (
            "status",
            vec!["status", "--profile", "work"],
            Ok(legacy_status_result()),
        ),
        (
            "doctor",
            vec!["doctor", "--all-profiles"],
            Ok(json!({ "ok": true, "agents": [] })),
        ),
    ];
    for (tag, arguments, probe_outcome) in probes {
        let fixture = Fixture::new(tag);
        // Every command is answered by the legacy daemon's first reply only;
        // a second request would be recorded and fail the assertion below.
        let server = serve_script(&fixture.socket(), vec![probe_outcome]);
        let mut command = vec!["integration"];
        command.extend(arguments);
        command.push("--json");

        let output = fixture.run(&command);

        assert_eq!(output.status.code(), Some(1), "{tag}");
        assert_eq!(
            stdout_json(&output)["err"]["code"],
            "integration_home_selectors_unsupported",
            "{tag}"
        );
        let seen = server.join().expect("fake daemon");
        assert!(
            seen.iter().all(|(method, _)| {
                method == "integration.status" || method == "integration.doctor"
            }),
            "{tag}: only read-only requests may reach a daemon that ignores the selectors: {seen:?}"
        );
        assert!(!seen.is_empty(), "{tag}");
    }
}

#[test]
fn requests_without_a_selector_send_no_preflight() {
    let fixture = Fixture::new("no-selector-no-preflight");
    let server = serve_script(
        &fixture.socket(),
        vec![Ok(json!({ "installed": [], "failed": [] }))],
    );

    let output = fixture.run(&["integration", "install", "--agent", "claude", "--json"]);

    assert!(output.status.success());
    assert_eq!(
        server.join().expect("fake daemon"),
        vec![(
            "integration.install".to_owned(),
            json!({ "agent": "claude" })
        )]
    );
}

#[test]
fn status_hints_follow_the_selector_the_daemon_verified() {
    let status = |home: Value| {
        let mut report = doctor_result(true)["agents"][0]["status"].clone();
        report["home"] = home;
        json!({ "agents": [report], "home_selectors": true })
    };
    let cases = [
        // `a-link` sorts first but resolves through a symlink the installer
        // refuses; the daemon verified `b-real`.
        (
            json!({ "profiles": ["a-link", "b-real"], "selector": { "kind": "profile", "name": "b-real" } }),
            Some("--agent claude --profile b-real"),
        ),
        // The runtime's own path was the symlink, so `bare` stays true but the
        // verified selector is the profile.
        (
            json!({ "profiles": ["real"], "bare": true, "selector": { "kind": "profile", "name": "real" } }),
            Some("--agent claude --profile real"),
        ),
        // The default home is the safe one even though a profile aliases it.
        (
            json!({ "profiles": ["alias"], "bare": true, "selector": { "kind": "default" } }),
            None,
        ),
    ];
    for (index, (home, expected)) in cases.into_iter().enumerate() {
        let fixture = Fixture::new(&format!("hint-{index}"));
        let server = serve_once(&fixture.socket(), status(home));

        let output = fixture.run(&["integration", "status", "--all-profiles"]);

        let text = String::from_utf8(output.stdout).expect("human output");
        let hint = text
            .lines()
            .find(|line| line.contains("hint: run `pohunek integration install"))
            .unwrap_or_else(|| panic!("a reinstall hint in {text}"));
        match expected {
            Some(selector) => assert!(hint.contains(selector), "{hint}"),
            None => assert!(
                hint.contains("--agent claude`") && !hint.contains("--profile"),
                "{hint}"
            ),
        }
        server.join().expect("fake daemon");
    }
}
