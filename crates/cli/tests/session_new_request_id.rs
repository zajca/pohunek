//! `pohunek session new --request-id` against a fixture daemon.
//!
//! The built CLI starts sessions on a fixture daemon that keeps the keys it has
//! seen, as the real daemon does: the first `session.new` under a key creates
//! the session, a repeat returns it with `replayed: true`. The scenarios pin
//! what the CLI sends, how it reports a replay, and that it refuses a daemon
//! that cannot honor a key, or a malformed key, before any `session.new` leaves
//! the process.

#![cfg(unix)]

// Rust guideline compliant 2026-11-01

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
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
const SESSION_ID: &str = "s-request-id-1";

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

/// What the fixture daemon remembers between connections.
#[derive(Default)]
struct Keys {
    seen: Mutex<HashSet<String>>,
}

fn capabilities(request_id_supported: bool) -> Value {
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
        "session_new_input_ack_supported": true
    });
    if request_id_supported {
        value["session_new_request_id_supported"] = json!(true);
    }
    value
}

fn session_new_result(request: &Request, keys: &Keys) -> Value {
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
        "created_at": "2026-11-02T00:00:00Z",
        "updated_at": "2026-11-02T00:01:00Z",
        "applied_input": true
    });
    let replayed = request.params()["client_request_id"]
        .as_str()
        .is_some_and(|key| !keys.seen.lock().expect("keys").insert(key.to_owned()));
    if replayed {
        // A replay carries neither the first response's `prompt_delivered`
        // nor its `worktree_source`.
        value["replayed"] = json!(true);
    } else {
        value["prompt_delivered"] = json!(true);
    }
    value
}

fn serve_connection(
    stream: std::os::unix::net::UnixStream,
    served: &Served,
    keys: &Keys,
    request_id_supported: bool,
) {
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
        let result = match request.method() {
            protocol::method::DAEMON_HEALTH => json!({
                "status": "ok",
                "daemon_version": "fixture",
                "protocol_version": PROTOCOL_VERSION.get()
            }),
            protocol::method::HOST_INSPECT => capabilities(request_id_supported),
            protocol::method::SESSION_NEW => session_new_result(&request, keys),
            other => panic!("the CLI sent an unexpected request: {other}"),
        };
        let response =
            Response::ok(PROTOCOL_VERSION, request.id(), result).expect("valid fixture response");
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

fn serve_local(socket: &Path, request_id_supported: bool) -> Served {
    let served = Served::default();
    let keys = Arc::new(Keys::default());
    let listener = UnixListener::bind(socket).expect("bind the fixture daemon");
    let recorded = Arc::clone(&served);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&recorded);
            let keys = Arc::clone(&keys);
            thread::spawn(move || serve_connection(stream, &served, &keys, request_id_supported));
        }
    });
    served
}

/// One fixture daemon and the environment the CLI runs in.
struct Host {
    env: TestEnv,
    served: Served,
}

impl Host {
    fn start(request_id_supported: bool) -> Self {
        let env = private_environment();
        let served = serve_local(
            &env.root().join("run/pohunek/daemon.sock"),
            request_id_supported,
        );
        Self { env, served }
    }

    fn session_new(&self, args: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(["session", "new", "--agent", "shell"])
            .args(args)
            .output()
            .expect("run pohunek session new")
    }

    fn creates(&self) -> Vec<Value> {
        self.served
            .lock()
            .expect("served requests")
            .iter()
            .filter(|(method, _)| method == protocol::method::SESSION_NEW)
            .map(|(_, params)| params.clone())
            .collect()
    }
}

fn document(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "one JSON document ({error}): stdout {}; stderr {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn a_repeated_request_id_returns_the_session_and_reports_the_replay() {
    let host = Host::start(true);
    let args = [
        "--request-id",
        "launch-1",
        "--input",
        "hello",
        "--input-ack-timeout-ms",
        "250",
        "--json",
    ];

    let first = host.session_new(&args);
    let second = host.session_new(&args);

    assert!(first.status.success(), "{first:?}");
    assert!(second.status.success(), "{second:?}");
    let (first, second_output) = (document(&first), document(&second));
    assert_eq!(first["ok"]["id"], SESSION_ID, "{first}");
    assert_eq!(first["ok"]["prompt_delivered"], true, "{first}");
    assert!(first["ok"].get("replayed").is_none(), "{first}");
    assert_eq!(second_output["ok"]["id"], SESSION_ID, "{second_output}");
    assert_eq!(second_output["ok"]["replayed"], true, "{second_output}");
    assert!(
        second.stderr.is_empty(),
        "a replay raises no prompt-delivery warning: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let creates = host.creates();
    assert_eq!(creates.len(), 2, "{creates:?}");
    assert!(creates
        .iter()
        .all(|params| params["client_request_id"] == "launch-1"));
}

#[test]
fn a_replay_is_named_in_the_human_output() {
    let host = Host::start(true);
    let args = ["--request-id", "launch-2"];

    let first = host.session_new(&args);
    let second = host.session_new(&args);

    let first = String::from_utf8_lossy(&first.stdout);
    let second = String::from_utf8_lossy(&second.stdout);
    assert!(
        first.contains(&format!("session {SESSION_ID} created")),
        "{first}"
    );
    assert!(
        second.contains(&format!(
            "session {SESSION_ID} already created by an earlier request with this --request-id"
        )),
        "{second}"
    );
}

#[test]
fn a_daemon_without_the_flag_is_refused_before_any_session_new() {
    let host = Host::start(false);

    let output = host.session_new(&["--request-id", "launch-3", "--json"]);

    assert!(!output.status.success(), "{output:?}");
    let document = document(&output);
    assert_eq!(
        document["err"]["code"], "session_new_request_id_unsupported",
        "{document}"
    );
    assert!(
        host.creates().is_empty(),
        "the daemon saw no session.new: {:?}",
        host.creates()
    );
}

#[test]
fn a_malformed_request_id_is_a_usage_error_before_any_daemon_is_dialed() {
    let host = Host::start(true);
    let too_long = "k".repeat(129);

    for key in ["", "with space", "a/b", too_long.as_str()] {
        let output = host.session_new(&["--request-id", key, "--json"]);

        assert_eq!(output.status.code(), Some(2), "{key:?}: {output:?}");
        let document = document(&output);
        assert_eq!(document["err"]["code"], "cli_usage", "{key:?}: {document}");
    }
    assert!(
        host.served.lock().expect("served requests").is_empty(),
        "nothing was sent"
    );
}

#[test]
fn a_session_new_without_a_request_id_sends_none() {
    let host = Host::start(true);

    let output = host.session_new(&["--json"]);

    assert!(output.status.success(), "{output:?}");
    let creates = host.creates();
    assert_eq!(creates.len(), 1);
    assert!(creates[0].get("client_request_id").is_none(), "{creates:?}");
}
