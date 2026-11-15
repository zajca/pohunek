//! `pohunek session new --no-repo-hooks` against a fixture daemon.
//!
//! The built CLI starts sessions on a fixture daemon that records every
//! request. The scenarios pin that the switch reaches the daemon only when the
//! daemon advertises `session_new_skip_repo_hooks_supported`, that the CLI
//! reports the switch in JSON and in human output, and that a session started
//! without the switch carries no `skip_repo_hooks` key.

#![cfg(unix)]

// Rust guideline compliant 2026-11-15

use std::fs;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// Mode of the runtime and state directories the client expects private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Id of the session the fixture daemon reports as created.
const SESSION_ID: &str = "s-skip-cli-1";

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

/// Whether the fixture daemon advertises `session_new_skip_repo_hooks_supported`.
#[derive(Clone, Copy)]
struct Daemon {
    skip_repo_hooks_supported: bool,
}

/// A daemon that advertises the switch and honors it.
const SUPPORTING: Daemon = Daemon {
    skip_repo_hooks_supported: true,
};

/// A daemon that predates the switch and would run the repository's hooks.
const PREDATING: Daemon = Daemon {
    skip_repo_hooks_supported: false,
};

fn capabilities(daemon: Daemon) -> Value {
    let mut value = json!({
        "daemon_version": "fixture",
        "protocol_version": PROTOCOL_VERSION.get(),
        "supported_agents": ["shell"],
        "runtimes": [],
        "git_available": true,
        "worktree_supported": true,
        "terminal_read_supported": true,
        "output_read_supported": true,
        "session_wait_supported": true
    });
    if daemon.skip_repo_hooks_supported {
        value["session_new_skip_repo_hooks_supported"] = json!(true);
    }
    value
}

/// The `session.new` result. A daemon that honors the switch records it on
/// the session, as the real daemon does.
fn session_new_result(request: &Request) -> Value {
    let mut value = json!({
        "id": SESSION_ID,
        "agent": "shell",
        "agent_base": "shell",
        "capabilities": {"fork": false, "resume": false},
        "cwd": "/project/root",
        "pid": 42424,
        "state": "running",
        "state_source": "process",
        "cols": 80,
        "rows": 24,
        "created_at": "2026-11-15T00:00:00Z",
        "updated_at": "2026-11-15T00:00:01Z"
    });
    if request.params()["skip_repo_hooks"] == json!(true) {
        value["skip_repo_hooks"] = json!(true);
    }
    value
}

fn result_for(request: &Request, daemon: Daemon) -> Value {
    match request.method() {
        protocol::method::DAEMON_HEALTH => json!({
            "status": "ok",
            "daemon_version": "fixture",
            "protocol_version": PROTOCOL_VERSION.get()
        }),
        protocol::method::HOST_INSPECT => capabilities(daemon),
        protocol::method::SESSION_NEW => session_new_result(request),
        other => panic!("the CLI sent an unexpected request: {other}"),
    }
}

fn serve_connection<S: Read + Write>(stream: S, served: &Served, daemon: Daemon) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse a request");
        served
            .lock()
            .expect("served requests")
            .push((request.method().to_owned(), request.params().clone()));
        let response = Response::ok(PROTOCOL_VERSION, request.id(), result_for(&request, daemon))
            .expect("valid fixture response");
        // The CLI may have gone away already.
        let _ = writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize the response")
        );
    }
}

fn private_environment() -> TestEnv {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(
            root.join(directory),
            fs::Permissions::from_mode(PRIVATE_DIR_MODE),
        )
        .expect("make the directory owner-private");
    }
    env
}

fn serve_local(socket: &Path, daemon: Daemon) -> Served {
    let served = Served::default();
    let listener = UnixListener::bind(socket).expect("bind the fixture daemon");
    let recorded = Arc::clone(&served);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&recorded);
            thread::spawn(move || serve_connection(stream, &served, daemon));
        }
    });
    served
}

/// Result of one `pohunek session new --json` run.
struct Run {
    status_success: bool,
    document: Value,
    served: Vec<(String, Value)>,
}

impl Run {
    /// The params of the `session.new` request the daemon received, if any.
    fn session_new(&self) -> Option<&Value> {
        self.served
            .iter()
            .find(|(method, _)| method == protocol::method::SESSION_NEW)
            .map(|(_, params)| params)
    }

    fn error_code(&self) -> &str {
        self.document["err"]["code"].as_str().unwrap_or_default()
    }
}

/// Runs `pohunek session new --agent shell <args> --json` against a local
/// fixture daemon.
fn run_json(args: &[&str], daemon: Daemon) -> Run {
    let env = private_environment();
    let root = env.root().to_path_buf();
    let served = serve_local(&root.join("run/pohunek/daemon.sock"), daemon);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["session", "new", "--agent", "shell"])
        .args(args)
        .arg("--json")
        .output()
        .expect("run pohunek session new");
    let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "one JSON document ({error}): stdout {}; stderr {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let served = served.lock().expect("served requests").clone();
    Run {
        status_success: output.status.success(),
        document,
        served,
    }
}

/// Runs `pohunek session new --agent shell <args>` (no `--json`) against a
/// fixture daemon and returns the process output with the requests served.
fn run_plain(args: &[&str], daemon: Daemon) -> (Output, Vec<(String, Value)>) {
    let env = private_environment();
    let root = env.root().to_path_buf();
    let served = serve_local(&root.join("run/pohunek/daemon.sock"), daemon);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["session", "new", "--agent", "shell"])
        .args(args)
        .output()
        .expect("run pohunek session new");
    let served = served.lock().expect("served requests").clone();
    (output, served)
}

#[test]
fn a_daemon_that_predates_the_switch_refuses_it_before_session_new() {
    let run = run_json(&["--no-repo-hooks"], PREDATING);

    assert!(!run.status_success, "{}", run.document);
    assert_eq!(
        run.error_code(),
        "session_new_skip_repo_hooks_unsupported",
        "{}",
        run.document
    );
    assert_eq!(run.document["err"]["class"], "daemon", "{}", run.document);
    assert!(
        run.session_new().is_none(),
        "no session.new was sent: {:?}",
        run.served
    );
}

#[test]
fn a_supporting_daemon_receives_the_switch_and_reports_it_in_json() {
    let run = run_json(&["--no-repo-hooks"], SUPPORTING);

    assert!(run.status_success, "{}", run.document);
    let params = run.session_new().expect("session.new was sent");
    assert_eq!(params["skip_repo_hooks"], json!(true), "{params}");
    assert_eq!(
        run.document["ok"]["skip_repo_hooks"],
        json!(true),
        "{}",
        run.document
    );
}

#[test]
fn a_supporting_daemon_reports_the_switch_for_humans() {
    let (output, served) = run_plain(&["--no-repo-hooks"], SUPPORTING);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.lines().any(|line| line == "  repo hooks: skipped"),
        "missing the repo hooks line in {stdout}"
    );
    let params = served
        .iter()
        .find(|(method, _)| method == protocol::method::SESSION_NEW)
        .map(|(_, params)| params)
        .expect("session.new was sent");
    assert_eq!(params["skip_repo_hooks"], json!(true), "{params}");
}

#[test]
fn a_session_without_the_switch_carries_no_key_and_no_repo_hooks_line() {
    let run = run_json(&[], SUPPORTING);

    assert!(run.status_success, "{}", run.document);
    let params = run.session_new().expect("session.new was sent");
    assert!(
        params.get("skip_repo_hooks").is_none(),
        "the unset switch is absent from the params: {params}"
    );
    assert!(
        run.document["ok"].get("skip_repo_hooks").is_none(),
        "the session does not record a skip: {}",
        run.document
    );
    let (output, _) = run_plain(&[], SUPPORTING);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("repo hooks:"),
        "no repo hooks line without the switch: {stdout}"
    );
}
