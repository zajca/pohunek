//! The idle-stop default through `pohunek session policy` and the real daemon.

#![cfg(unix)]

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;

use plugin_harness::Harness;
use pohunek_test_support::env::TestEnv;
use protocol::{ProtocolError, Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// The smallest idle time the daemon accepts, and the one the scenarios use.
const MIN_SECS: u64 = 60;

/// The version the fixture daemon reports in `daemon.health`.
const FIXTURE_DAEMON_VERSION: &str = "0.35.2";

/// A daemon version older than every CLI release the workspace builds.
const OLDER_DAEMON_VERSION: &str = "0.1.0";

/// A default well above the minimum.
const DEFAULT_SECS: u64 = 900;

#[tokio::test]
async fn session_policy_shows_sets_keeps_and_clears_the_idle_stop_default() {
    let harness = Harness::start().await;

    let (status, fresh) = harness.json(&["session", "policy", "get"]).await;
    assert_eq!(status, 0, "{fresh}");
    assert_eq!(
        fresh["ok"]["idle_stop"],
        json!({"min_after_secs": MIN_SECS})
    );
    assert_eq!(fresh["ok"]["retention"]["enabled"], false);

    let (status, set) = harness
        .json(&[
            "session",
            "policy",
            "set",
            "--idle-stop-after",
            &DEFAULT_SECS.to_string(),
        ])
        .await;
    assert_eq!(status, 0, "{set}");
    assert_eq!(set["ok"]["idle_stop"]["default_after_secs"], DEFAULT_SECS);
    assert_eq!(
        set["ok"]["retention"], fresh["ok"]["retention"],
        "naming only the idle default leaves the retention policy as it was"
    );

    // A later `set` of another field keeps the default.
    let (status, other) = harness
        .json(&["session", "policy", "set", "--enabled"])
        .await;
    assert_eq!(status, 0, "{other}");
    assert_eq!(other["ok"]["retention"]["enabled"], true);
    assert!(other["ok"].get("idle_stop").is_none(), "{other}");
    let (status, got) = harness.json(&["session", "policy", "get"]).await;
    assert_eq!(status, 0, "{got}");
    assert_eq!(got["ok"]["idle_stop"]["default_after_secs"], DEFAULT_SECS);

    let quiet = harness
        .run(&["session", "policy", "set", "--enabled"])
        .await;
    assert!(
        !String::from_utf8_lossy(&quiet.stdout).contains("idle_stop_after"),
        "a `set` that does not name the default says nothing about it"
    );

    let human = harness.run(&["session", "policy", "get"]).await;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains(&format!("idle_stop_after: {DEFAULT_SECS} seconds")),
        "{text}"
    );

    // Below the minimum is refused and changes nothing.
    let (status, refused) = harness
        .json(&["session", "policy", "set", "--idle-stop-after", "5"])
        .await;
    assert_ne!(status, 0, "{refused}");
    assert_eq!(
        refused["err"]["code"], "invalid_session_policy",
        "{refused}"
    );
    let (_, still) = harness.json(&["session", "policy", "get"]).await;
    assert_eq!(still["ok"]["idle_stop"]["default_after_secs"], DEFAULT_SECS);

    // A refused idle default stops the command before the retention fields are written.
    let (status, refused) = harness
        .json(&[
            "session",
            "policy",
            "set",
            "--disabled",
            "--idle-stop-after",
            "5",
        ])
        .await;
    assert_ne!(status, 0, "{refused}");
    let (_, unchanged) = harness.json(&["session", "policy", "get"]).await;
    assert_eq!(unchanged["ok"]["retention"]["enabled"], true);

    // A refused retention field beside a valid idle default changes nothing.
    let (status, refused) = harness
        .json(&[
            "session",
            "policy",
            "set",
            "--sweep-interval-secs",
            "1",
            "--idle-stop-after",
            "1200",
        ])
        .await;
    assert_ne!(status, 0, "{refused}");
    let (_, unchanged) = harness.json(&["session", "policy", "get"]).await;
    assert_eq!(
        unchanged["ok"]["idle_stop"]["default_after_secs"],
        DEFAULT_SECS
    );

    let (status, cleared) = harness
        .json(&["session", "policy", "set", "--idle-stop-after", "0"])
        .await;
    assert_eq!(status, 0, "{cleared}");
    assert_eq!(
        cleared["ok"]["idle_stop"],
        json!({"min_after_secs": MIN_SECS})
    );
    harness.stop().await;
}

#[tokio::test]
async fn an_invalid_idle_stop_value_is_refused_by_session_new_before_anything_starts() {
    let harness = Harness::start().await;

    let (status, refused) = harness
        .json(&[
            "session",
            "new",
            "--agent",
            "shell",
            "--meta",
            "pohunek.idle_stop_after_secs=soon",
        ])
        .await;
    assert_ne!(status, 0, "{refused}");
    assert_eq!(
        refused["err"]["code"], "invalid_idle_stop_after",
        "{refused}"
    );
    let (status, listed) = harness.json(&["session", "list"]).await;
    assert_eq!(status, 0, "{listed}");
    assert_eq!(
        listed["ok"],
        Value::Array(Vec::new()),
        "no session was left behind"
    );

    let (status, created) = harness
        .json(&[
            "session",
            "new",
            "--agent",
            "shell",
            "--meta",
            "pohunek.idle_stop_after_secs=default",
        ])
        .await;
    assert_eq!(status, 0, "{created}");
    harness.stop().await;
}

/// How the fixture daemon treats the idle-stop methods.
#[derive(Clone, Copy)]
enum IdleStop {
    /// The daemon of the previous release does not know them.
    Unknown,
    /// The daemon serves the default for reads and refuses every write.
    WriteFails,
}

/// Serves a daemon on `listener` until a connection closes without a request:
/// it holds a retention policy, treats the idle-stop methods as `idle_stop`
/// says, and reports every method it was asked.
fn serve_fixture_daemon(
    listener: UnixListener,
    idle_stop: IdleStop,
    daemon_version: &'static str,
) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut methods = Vec::new();
        loop {
            let (connection, _) = listener.accept().expect("accept a CLI connection");
            let mut writer = connection.try_clone().expect("clone the connection");
            let mut served = false;
            for line in BufReader::new(connection).lines() {
                let Ok(line) = line else { break };
                served = true;
                let request: Request = serde_json::from_str(&line).expect("one request line");
                methods.push(request.method().to_owned());
                let response = match request.method() {
                    protocol::method::SESSION_POLICY_GET | protocol::method::SESSION_POLICY_SET => {
                        Response::ok(
                            PROTOCOL_VERSION,
                            request.id(),
                            json!({"retention": {
                                "enabled": false,
                                "sweep_interval_secs": 21600,
                                "terminal_ttl_secs": 604_800,
                                "lost_ttl_secs": 604_800,
                                "max_removals_per_sweep": 100,
                            }}),
                        )
                    }
                    protocol::method::DAEMON_HEALTH => Response::ok(
                        PROTOCOL_VERSION,
                        request.id(),
                        json!({
                            "status": "ok",
                            "daemon_version": daemon_version,
                            "protocol_version": PROTOCOL_VERSION.get(),
                        }),
                    ),
                    protocol::method::SESSION_IDLE_STOP_GET
                        if matches!(idle_stop, IdleStop::WriteFails) =>
                    {
                        Response::ok(
                            PROTOCOL_VERSION,
                            request.id(),
                            json!({"min_after_secs": 60}),
                        )
                    }
                    protocol::method::SESSION_IDLE_STOP_SET
                        if matches!(idle_stop, IdleStop::WriteFails) =>
                    {
                        Response::err(
                            PROTOCOL_VERSION,
                            request.id(),
                            ProtocolError::new(
                                protocol::ErrorClass::Runtime,
                                "session_policy_store_error",
                                "the idle-stop record could not be written".to_owned(),
                                None,
                            ),
                        )
                    }
                    other => Response::err(
                        PROTOCOL_VERSION,
                        request.id(),
                        ProtocolError::method_not_found(other),
                    ),
                }
                .expect("a response envelope");
                writeln!(
                    writer,
                    "{}",
                    serde_json::to_string(&response).expect("response JSON")
                )
                .expect("write the response");
            }
            if !served {
                return methods;
            }
        }
    })
}

/// Binds the daemon socket of `env`.
fn bind_fixture_socket(env: &TestEnv) -> (UnixListener, std::path::PathBuf) {
    let runtime = env.runtime_dir().join("pohunek");
    std::fs::create_dir(&runtime).expect("create daemon socket directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("make socket directory private");
    let socket = runtime.join("daemon.sock");
    (
        UnixListener::bind(&socket).expect("bind daemon socket"),
        socket,
    )
}

/// Runs the CLI against `env` with `--json`.
fn run_cli(env: &TestEnv, arguments: &[&str]) -> (Option<i32>, Value) {
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(arguments)
        .arg("--json")
        .output()
        .expect("run CLI");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    (output.status.code(), document)
}

/// Ends the fixture daemon and returns the methods it was asked.
fn end_fixture_daemon(
    server: std::thread::JoinHandle<Vec<String>>,
    socket: &std::path::Path,
) -> Vec<String> {
    drop(std::os::unix::net::UnixStream::connect(socket).expect("end the fixture daemon"));
    server.join().expect("the fixture daemon ends")
}

/// A daemon older than the CLI is pointed at the CLI's release.
#[test]
fn an_older_daemon_is_told_to_upgrade_to_the_cli_release() {
    let env = TestEnv::new().expect("private CLI environment");
    let (listener, socket) = bind_fixture_socket(&env);
    let server = serve_fixture_daemon(listener, IdleStop::Unknown, OLDER_DAEMON_VERSION);

    let (code, refused) = run_cli(
        &env,
        &["session", "policy", "set", "--idle-stop-after", "900"],
    );
    end_fixture_daemon(server, &socket);

    assert_ne!(code, Some(0), "{refused}");
    assert!(
        refused["err"]["recover"].as_str().is_some_and(|hint| hint
            .contains(&format!("(version {OLDER_DAEMON_VERSION})"))
            && hint.contains(&format!(
                "to this CLI's release ({})",
                env!("CARGO_PKG_VERSION")
            ))),
        "{refused}"
    );
}

#[test]
fn a_daemon_of_the_previous_release_refuses_the_idle_default_and_keeps_its_retention_policy() {
    let env = TestEnv::new().expect("private CLI environment");
    let (listener, socket) = bind_fixture_socket(&env);
    let server = serve_fixture_daemon(listener, IdleStop::Unknown, FIXTURE_DAEMON_VERSION);

    // Naming a retention field beside the idle default writes nothing.
    let (code, refused) = run_cli(
        &env,
        &[
            "session",
            "policy",
            "set",
            "--enabled",
            "--idle-stop-after",
            "900",
        ],
    );
    assert_ne!(code, Some(0), "{refused}");
    assert_eq!(refused["err"]["code"], "method_not_found", "{refused}");
    assert!(
        refused["err"]["recover"]
            .as_str()
            .is_some_and(|hint| hint.contains("upgrade the daemon")
                && hint.contains(&format!("(version {FIXTURE_DAEMON_VERSION})"))
                && hint.contains("to a release that includes it")
                && !hint.contains("this CLI's release")
                && hint.contains("Nothing was changed")),
        "{refused}"
    );

    // A `set` that does not name the idle default never asks for it.
    let (code, set) = run_cli(&env, &["session", "policy", "set", "--enabled"]);
    assert_eq!(code, Some(0), "{set}");
    assert!(set["ok"].get("idle_stop").is_none(), "{set}");

    // The retention policy is still readable and the missing default is not an error.
    let (code, got) = run_cli(&env, &["session", "policy", "get"]);
    assert_eq!(code, Some(0), "{got}");
    assert!(got["ok"].get("idle_stop").is_none(), "{got}");

    let methods = end_fixture_daemon(server, &socket);
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == protocol::method::SESSION_POLICY_SET)
            .count(),
        1,
        "only the `set --enabled` wrote the old policy: {methods:?}"
    );
    let first_write = methods
        .iter()
        .position(|method| method == protocol::method::SESSION_POLICY_SET)
        .expect("the retention write");
    assert_eq!(
        methods[..first_write]
            .iter()
            .filter(|method| method.starts_with("session.idle_stop"))
            .count(),
        1,
        "only the refused `--idle-stop-after` call asked for it before the `set --enabled`: {methods:?}"
    );
}

#[test]
fn a_refused_idle_default_after_the_retention_write_says_which_half_landed() {
    let env = TestEnv::new().expect("private CLI environment");
    let (listener, socket) = bind_fixture_socket(&env);
    let server = serve_fixture_daemon(listener, IdleStop::WriteFails, FIXTURE_DAEMON_VERSION);

    let (code, failed) = run_cli(
        &env,
        &[
            "session",
            "policy",
            "set",
            "--enabled",
            "--idle-stop-after",
            "900",
        ],
    );
    assert_ne!(code, Some(0), "{failed}");
    assert!(
        failed["err"]["msg"].as_str().is_some_and(|msg| msg
            .contains("the retention policy was updated, but the idle-stop default was not")),
        "{failed}"
    );

    let methods = end_fixture_daemon(server, &socket);
    let write = methods
        .iter()
        .position(|method| method == protocol::method::SESSION_POLICY_SET)
        .expect("the retention write");
    let idle = methods
        .iter()
        .position(|method| method == protocol::method::SESSION_IDLE_STOP_SET)
        .expect("the idle write");
    assert!(write < idle, "retention goes first: {methods:?}");
}
