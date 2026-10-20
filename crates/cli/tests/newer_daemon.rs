//! The `pohunek` binary against a daemon of the same protocol version that
//! sends an enum value this build does not know.
//!
//! A scripted control socket answers the CLI's request with a response taken
//! from a real error, so each outcome is what an operator observes. Slices that
//! make another enum tolerant add one scenario each here.

// Rust guideline compliant 2026-10-20

#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::process::Output;

use pohunek_test_support::env::TestEnv;
use protocol::{ErrorClass, ProtocolError, Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// The error a daemon of the same version answers `notification.list` with.
fn daemon_error() -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Daemon,
        "quota_exhausted",
        "the notification inbox is over its quota",
        Some("archive old notifications".to_owned()),
    )
}

/// The wire line of `error` as a newer daemon sends it, with `class` replaced
/// by a class this build does not define.
fn newer_daemon_line(id: &str, error: ProtocolError, class: &str) -> String {
    let mut response =
        serde_json::to_value(Response::err(PROTOCOL_VERSION, id, error).expect("response"))
            .expect("response JSON");
    response["err"]["class"] = json!(class);
    serde_json::to_string(&response).expect("response line")
}

/// Runs `pohunek` with `arguments` against a scripted daemon. `answer` returns
/// the result of a request, or `None` for a method the scenario does not
/// script, which gets a health result.
fn run_against_scripted_daemon(
    arguments: &[&str],
    answer: impl Fn(&Request) -> Option<Result<Value, String>> + Send + 'static,
) -> Output {
    let env = TestEnv::new().expect("private CLI environment");
    let runtime = env.runtime_dir().join("pohunek");
    std::fs::create_dir(&runtime).expect("create daemon socket directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("make socket directory private");
    let socket = runtime.join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind daemon socket");

    let server = std::thread::spawn(move || {
        loop {
            let (connection, _) = listener.accept().expect("accept a CLI connection");
            let mut writer = connection.try_clone().expect("clone the connection");
            let mut served = false;
            for line in BufReader::new(connection).lines() {
                let Ok(line) = line else { break };
                served = true;
                let request: Request = serde_json::from_str(&line).expect("one request line");
                let reply = match answer(&request) {
                    Some(Ok(result)) => serde_json::to_string(
                        &Response::ok(PROTOCOL_VERSION, request.id(), result).expect("response"),
                    )
                    .expect("response line"),
                    Some(Err(line)) => line,
                    None => {
                        let health = json!({
                            "status": "ok",
                            "daemon_version": "0.34.2",
                            "protocol_version": PROTOCOL_VERSION.get(),
                        });
                        serde_json::to_string(
                            &Response::ok(PROTOCOL_VERSION, request.id(), health)
                                .expect("response"),
                        )
                        .expect("response line")
                    }
                };
                writeln!(writer, "{reply}").expect("write the response");
            }
            // A connection without a request ends the fixture daemon.
            if !served {
                break;
            }
        }
    });

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(arguments)
        .output()
        .expect("run CLI");
    drop(std::os::unix::net::UnixStream::connect(&socket).expect("end the fixture daemon"));
    server.join().expect("the fixture daemon ends");
    output
}

/// Runs `pohunek notifications list` with `extra` arguments against a scripted
/// daemon that answers the list request with an error of an unknown `class`.
fn list_against_newer_daemon(extra: &[&str], class: &str) -> Output {
    let class = class.to_owned();
    let mut arguments = vec!["notifications", "list"];
    arguments.extend_from_slice(extra);
    run_against_scripted_daemon(&arguments, move |request| {
        (request.method() == protocol::method::NOTIFICATION_LIST)
            .then(|| Err(newer_daemon_line(request.id(), daemon_error(), &class)))
    })
}

#[test]
fn an_error_class_from_a_newer_daemon_keeps_the_daemons_code_and_message() {
    let output = list_against_newer_daemon(&[], "quota");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("quota_exhausted"), "{stderr}");
    assert!(
        stderr.contains("the notification inbox is over its quota"),
        "{stderr}"
    );
    assert!(!stderr.contains("json_error"), "{stderr}");
}

#[test]
fn an_error_class_from_a_newer_daemon_stays_a_json_error_document() {
    let output = list_against_newer_daemon(&["--json"], "quota");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(
        document["err"],
        json!({
            "class": "unknown",
            "code": "quota_exhausted",
            "msg": "the notification inbox is over its quota",
            "recover": "archive old notifications",
        }),
        "{document}"
    );
}

/// A session snapshot of a daemon that defines a newer activity, state source,
/// working-directory source, subagent lifecycle and background kind and status.
fn newer_daemon_session() -> Value {
    json!({
        "id": "s-1",
        "agent": "shell",
        "agent_base": "shell",
        "cwd": "/workspace/project",
        "cwd_source": "a_source_from_a_newer_daemon",
        "pid": 4242,
        "cols": 120,
        "rows": 40,
        "state": "running",
        "state_source": "a_source_from_a_newer_daemon",
        "activity": "an_activity_from_a_newer_daemon",
        "subagents": [{
            "id": "sub-1",
            "provider": "codex",
            "lifecycle": "a_lifecycle_from_a_newer_daemon",
            "activity": "an_activity_from_a_newer_daemon",
            "revision": "1",
            "started_at_ms": 1,
            "updated_at_ms": 2,
        }],
        "background": {
            "revision": "1",
            "observed_at_ms": 3,
            "items": [{
                "id": "task-1",
                "kind": "a_kind_from_a_newer_daemon",
                "status": "a_status_from_a_newer_daemon",
                "first_seen_ms": 1,
                "last_seen_ms": 2,
            }],
        },
        "created_at": "2026-06-17T10:00:00Z",
        "updated_at": "2026-06-17T10:01:00Z",
    })
}

/// Answers `session.list` with one snapshot and `session.inspect` with it.
fn session_snapshot_daemon(session: Value) -> impl Fn(&Request) -> Option<Result<Value, String>> {
    move |request| match request.method() {
        protocol::method::SESSION_LIST => Some(Ok(json!([session.clone()]))),
        protocol::method::SESSION_INSPECT => Some(Ok(session.clone())),
        _ => None,
    }
}

#[test]
fn a_session_list_with_display_values_from_a_newer_daemon_shows_unknown() {
    let output = run_against_scripted_daemon(
        &["session", "list"],
        session_snapshot_daemon(newer_daemon_session()),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("s-1"), "{stdout}");
    assert!(stdout.contains("unknown"), "{stdout}");
    assert!(
        stdout.contains("0/1"),
        "a subagent of unknown lifecycle is not counted as running: {stdout}"
    );
}

#[test]
fn a_session_inspect_with_display_values_from_a_newer_daemon_reemits_unknown() {
    let output = run_against_scripted_daemon(
        &["session", "inspect", "s-1", "--json"],
        session_snapshot_daemon(newer_daemon_session()),
    );

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    let session = &document["ok"];
    assert_eq!(session["activity"], "unknown", "{document}");
    assert_eq!(session["state_source"], "unknown", "{document}");
    assert_eq!(session["cwd_source"], "unknown", "{document}");
    assert_eq!(
        session["subagents"][0]["lifecycle"], "unknown",
        "{document}"
    );
    assert_eq!(session["subagents"][0]["activity"], "unknown", "{document}");
    assert_eq!(
        session["background"]["items"][0]["kind"], "other",
        "{document}"
    );
    assert_eq!(
        session["background"]["items"][0]["status"], "other",
        "{document}"
    );
}
