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

/// Runs `pohunek notifications list` with `extra` arguments against a scripted
/// daemon that answers the list request with an error of an unknown `class`.
fn list_against_newer_daemon(extra: &[&str], class: &str) -> Output {
    let env = TestEnv::new().expect("private CLI environment");
    let runtime = env.runtime_dir().join("pohunek");
    std::fs::create_dir(&runtime).expect("create daemon socket directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("make socket directory private");
    let socket = runtime.join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind daemon socket");

    let class = class.to_owned();
    let server = std::thread::spawn(move || {
        loop {
            let (connection, _) = listener.accept().expect("accept a CLI connection");
            let mut writer = connection.try_clone().expect("clone the connection");
            let mut served = false;
            for line in BufReader::new(connection).lines() {
                let Ok(line) = line else { break };
                served = true;
                let request: Request = serde_json::from_str(&line).expect("one request line");
                let reply = if request.method() == protocol::method::NOTIFICATION_LIST {
                    newer_daemon_line(request.id(), daemon_error(), &class)
                } else {
                    let health = json!({
                        "status": "ok",
                        "daemon_version": "0.34.2",
                        "protocol_version": PROTOCOL_VERSION.get(),
                    });
                    serde_json::to_string(
                        &Response::ok(PROTOCOL_VERSION, request.id(), health).expect("response"),
                    )
                    .expect("response line")
                };
                writeln!(writer, "{reply}").expect("write the response");
            }
            // A connection without a request ends the fixture daemon.
            if !served {
                break;
            }
        }
    });

    let mut arguments = vec!["notifications", "list"];
    arguments.extend_from_slice(extra);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(arguments)
        .output()
        .expect("run CLI");
    drop(std::os::unix::net::UnixStream::connect(&socket).expect("end the fixture daemon"));
    server.join().expect("the fixture daemon ends");
    output
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
