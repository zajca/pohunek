//! `pohunek store rejected` against a real daemon over a metadata store that
//! holds lines no loader accepts, and against daemons that predate the methods.

// Rust guideline compliant 2026-10-29

#![cfg(unix)]

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};

use plugin_harness::{stderr_text, stdout_text, Harness};
use pohunek_test_support::env::TestEnv;
use protocol::{ProtocolError, Request, Response};
use serde_json::json;

/// A current-schema session line that no loader accepts; its state is
/// unreadable, so it may own a live runtime.
fn live_line() -> String {
    json!({
        "kind": "session",
        "schema_version": pohunek_daemon::store::STORE_SCHEMA_VERSION,
        "session_id": "s-live",
        "desired_state": "suspended"
    })
    .to_string()
}

/// A current-schema project line that no loader accepts; it carries no pin.
fn project_line() -> String {
    json!({
        "kind": "project",
        "schema_version": pohunek_daemon::store::STORE_SCHEMA_VERSION,
        "unexpected": 1
    })
    .to_string()
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[tokio::test]
async fn the_store_commands_list_and_set_aside_rejected_lines_of_a_real_daemon() {
    let harness = Harness::start_durable().await;
    let store = harness.env.root().join("metadata/metadata.jsonl");
    let (project, live) = (project_line(), live_line());
    fs::write(&store, format!("{project}\n{live}\n")).expect("write the store");
    fs::set_permissions(&store, fs::Permissions::from_mode(0o600)).expect("private store");

    let (code, listed) = harness.json(&["store", "rejected", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    assert_eq!(listed["ok"]["total"], 2);
    assert_eq!(listed["ok"]["lines"][0]["line"], 1);
    assert_eq!(listed["ok"]["lines"][0]["kind"], "project");
    assert_eq!(listed["ok"]["lines"][0]["may_pin"], false);
    assert_eq!(
        listed["ok"]["lines"][0]["content_hash"],
        sha256_hex(&project)
    );
    assert_eq!(listed["ok"]["lines"][1]["may_own_runtime"], true);
    let human = harness.run(&["store", "rejected", "list"]).await;
    assert!(human.status.success(), "{}", stderr_text(&human));
    let text = stdout_text(&human);
    assert!(text.contains(&sha256_hex(&live)), "{text}");
    assert!(text.contains("line 2: kind=session may_pin=yes"), "{text}");

    let malformed = harness
        .run(&["store", "rejected", "set-aside", "not-a-hash"])
        .await;
    assert!(!malformed.status.success());
    assert!(stderr_text(&malformed).contains("64 lowercase hex"));

    let refused = harness
        .run(&["store", "rejected", "set-aside", &sha256_hex(&live)])
        .await;
    assert!(!refused.status.success());
    let refusal = stderr_text(&refused);
    assert!(refusal.contains("store_rejected_line_live"), "{refusal}");
    assert!(refusal.contains("--accept-pin-loss"), "{refusal}");
    assert_eq!(
        fs::read_to_string(&store).expect("read store"),
        format!("{project}\n{live}\n"),
        "a refused set-aside changes nothing"
    );

    let (code, moved) = harness
        .json(&[
            "store",
            "rejected",
            "set-aside",
            &sha256_hex(&live),
            "--accept-pin-loss",
        ])
        .await;
    assert_eq!(code, 0, "{moved}");
    assert_eq!(moved["ok"]["lines"], json!([2]));
    assert_eq!(moved["ok"]["quarantine_file"], "metadata.rejected.jsonl");
    let set_aside = harness
        .run(&["store", "rejected", "set-aside", &sha256_hex(&project)])
        .await;
    assert!(set_aside.status.success(), "{}", stderr_text(&set_aside));
    assert!(stdout_text(&set_aside).contains("metadata.rejected.jsonl"));
    let quarantine = fs::read_to_string(store.with_file_name("metadata.rejected.jsonl"))
        .expect("read the quarantine file");
    assert_eq!(quarantine.lines().count(), 2, "{quarantine}");
    assert_eq!(
        fs::read_to_string(&store).expect("read store"),
        "",
        "both lines left the store"
    );

    let (_, empty) = harness.json(&["store", "rejected", "list"]).await;
    assert_eq!(empty["ok"]["total"], 0);

    let remote = harness
        .run(&["--host", "elsewhere", "store", "rejected", "list"])
        .await;
    assert!(!remote.status.success());
    assert!(stderr_text(&remote).contains("store_local_only"));
}

/// Serves `reply` for every request except the version probe, answering that
/// with `protocol_version`.
fn serve_older_daemon(
    listener: UnixListener,
    protocol_version: u32,
    reply: fn(&str) -> Response,
) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut asked = Vec::new();
        while let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if !reader.read_line(&mut line).is_ok_and(|read| read > 0) {
                return asked;
            }
            loop {
                let request: Request =
                    serde_json::from_str(line.trim_end()).expect("parse request");
                let response = if request.method() == "daemon.health" {
                    Response::ok(
                        protocol::PROTOCOL_VERSION,
                        request.id(),
                        json!({
                            "status": "ok",
                            "daemon_version": "0.0.1",
                            "protocol_version": protocol_version
                        }),
                    )
                    .expect("health response")
                } else {
                    asked.push(request.method().to_owned());
                    reply(request.id())
                };
                writeln!(
                    reader.get_mut(),
                    "{}",
                    serde_json::to_string(&response).expect("serialize response")
                )
                .expect("write response");
                line.clear();
                if !reader.read_line(&mut line).is_ok_and(|read| read > 0) {
                    break;
                }
            }
        }
        asked
    })
}

fn method_not_found(id: &str) -> Response {
    Response::err(
        protocol::PROTOCOL_VERSION,
        id,
        ProtocolError::method_not_found("store.rejected_list"),
    )
    .expect("error response")
}

fn run_against(env: &TestEnv, protocol_version: u32) -> (std::process::Output, Vec<String>) {
    let runtime = env.runtime_dir().join("pohunek");
    fs::create_dir_all(&runtime).expect("create the socket directory");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))
        .expect("make the socket directory private");
    let socket = runtime.join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind the fixture daemon");
    let server = serve_older_daemon(listener, protocol_version, method_not_found);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["store", "rejected", "list"])
        .output()
        .expect("run the CLI");
    drop(UnixStream::connect(&socket));
    (output, server.join().expect("the fixture daemon ends"))
}

/// A daemon of the previous protocol, or one of the current protocol that
/// predates the methods, answers `method_not_found`; the CLI names the manual
/// remedy that still applies to it.
#[test]
fn a_daemon_that_predates_the_store_methods_is_pointed_at_the_manual_remedy() {
    for protocol_version in [protocol::PROTOCOL_VERSION.get(), 3] {
        let env = TestEnv::new().expect("private CLI environment");
        let (output, asked) = run_against(&env, protocol_version);
        assert!(!output.status.success(), "protocol {protocol_version}");
        assert_eq!(asked, ["store.rejected_list"]);
        let message = stderr_text(&output);
        assert!(message.contains("method_not_found"), "{message}");
        assert!(message.contains("predates `store rejected`"), "{message}");
    }
}
