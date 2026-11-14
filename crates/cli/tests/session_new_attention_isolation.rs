//! `pohunek session new --meta pohunek.attention.children=isolate` against a
//! fixture daemon.
//!
//! The built CLI starts sessions on a fixture daemon that records every
//! request. The scenarios pin what the CLI sends for the reserved attention
//! isolation pair, that a daemon without the capability is refused before
//! the session is created, and that `session list` renders the footer line
//! for a row the daemon reports as isolated.

#![cfg(unix)]

// Rust guideline compliant 2026-11-13

use std::fs;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// Mode of the runtime and state directories the client expects private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Id of the session the fixture daemon reports as created.
const SESSION_ID: &str = "s-isolate-1";

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

/// Whether the fixture daemon advertises attention isolation.
#[derive(Clone, Copy)]
struct Daemon {
    attention_isolation_supported: bool,
}

/// A daemon that honors the isolation flag.
const SUPPORTING: Daemon = Daemon {
    attention_isolation_supported: true,
};

/// A daemon that predates the isolation flag and would ignore it.
const IGNORING: Daemon = Daemon {
    attention_isolation_supported: false,
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
        "session_wait_supported": true,
        // The refusal scenarios below never create, so no key tracking is needed.
        "session_new_request_id_supported": true
    });
    if daemon.attention_isolation_supported {
        value["session_attention_isolation_supported"] = json!(true);
    }
    value
}

fn session_new_result(request: &Request) -> Value {
    json!({
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
        "created_at": "2026-07-08T00:00:00Z",
        "updated_at": "2026-07-08T00:01:00Z",
        "metadata": request.params().get("metadata").cloned().unwrap_or(json!({}))
    })
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
        protocol::method::SESSION_LIST => json!([{
            "id": "s-isolated-1",
            "agent": "shell",
            "agent_base": "shell",
            "capabilities": {"fork": false, "resume": false},
            "cwd": "/project/root",
            "pid": 42424,
            "state": "running",
            "state_source": "process",
            "cols": 80,
            "rows": 24,
            "created_at": "2026-07-08T00:00:00Z",
            "updated_at": "2026-07-08T00:01:00Z",
            "metadata": {},
            "attention_isolated_by": "s-flagged-parent"
        }]),
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

/// Result of one `pohunek session new` run.
struct Run {
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

/// Runs `pohunek session list` (human output) against a local fixture
/// daemon and returns stdout.
fn run_list(daemon: Daemon) -> String {
    let env = private_environment();
    let root = env.root().to_path_buf();
    let _served = serve_local(&root.join("run/pohunek/daemon.sock"), daemon);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["session", "list"])
        .output()
        .expect("run pohunek session list");
    assert!(
        output.status.success(),
        "list exits 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Runs `pohunek session new --agent shell <args> --json` against a local
/// fixture daemon.
fn run_local(args: &[&str], daemon: Daemon) -> Run {
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
    Run { document, served }
}

#[test]
fn the_reserved_meta_pair_reaches_session_new() {
    let run = run_local(
        &["--meta", "pohunek.attention.children=isolate"],
        SUPPORTING,
    );

    assert!(run.document.get("err").is_none(), "{}", run.document);
    let params = run.session_new().expect("session.new was sent");
    assert_eq!(
        params["metadata"],
        json!({"pohunek.attention.children": "isolate"}),
        "{params}"
    );
    let inspect = run
        .served
        .iter()
        .find(|(method, _)| method == protocol::method::HOST_INSPECT)
        .expect("host.inspect was sent");
    assert_eq!(inspect.1, json!({"skip_runtime_probes": true}));
}

#[test]
fn the_reserved_meta_pair_is_refused_by_a_daemon_that_ignores_it() {
    let run = run_local(&["--meta", "pohunek.attention.children=isolate"], IGNORING);

    assert_eq!(
        run.error_code(),
        "session_attention_isolation_unsupported",
        "{}",
        run.document
    );
    // The CLI refusal mirrors the daemon-class client refusal.
    assert_eq!(run.document["err"]["class"], "daemon", "{}", run.document);
    assert!(
        run.document["err"]["recover"]
            .as_str()
            .is_some_and(|recover| recover.contains("update and restart")),
        "{}",
        run.document
    );
    assert!(run.session_new().is_none(), "no session.new was sent");
}

#[test]
fn session_list_renders_the_isolation_footer_for_an_isolated_row() {
    let stdout = run_list(SUPPORTING);
    assert!(
        stdout.contains("isolated s-isolated-1: attention isolated by s-flagged-parent"),
        "{stdout}"
    );
}
