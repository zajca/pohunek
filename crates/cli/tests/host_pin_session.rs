//! `pohunek session <action> <name>` against a remote host whose name changes
//! peer between the session-name lookup and the action.
//!
//! The built CLI resolves the session name with `session.list` and then sends
//! the action. After the first connection was dialed, the fake `netbird` moves
//! the short host name to another peer. Every connection of the command must
//! still reach the first peer.

// Rust guideline compliant 2026-10-22

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[path = "support/pin_overlay.rs"]
mod pin_overlay;

/// Peer the name `peer-a` selects before the first connection.
const FIRST_IP: &str = "100.64.0.11";

/// Peer the name `peer-a` selects after the first connection.
const SECOND_IP: &str = "100.64.0.12";

/// Mode of the runtime and state directories the client expects private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Display name the commands target; the fixture daemon maps it to [`SESSION_ID`].
const SESSION_NAME: &str = "alpha";

/// Stable id behind [`SESSION_NAME`].
const SESSION_ID: &str = "s-pin-session-1";

/// Requests the fixture daemon served, by method, in arrival order.
type Served = Arc<Mutex<Vec<String>>>;

/// Params of the `session.list` requests the fixture daemon served.
type ListParams = Arc<Mutex<Vec<Value>>>;

fn session() -> Value {
    json!({
        "id": SESSION_ID,
        "name": SESSION_NAME,
        "agent": "codex",
        "agent_base": "codex",
        "capabilities": {"fork": false, "resume": true},
        "cwd": "/workspace/pohunek",
        "pid": 42424,
        "state": "running",
        "state_source": "process",
        "cols": 80,
        "rows": 24,
        "created_at": "2026-07-08T00:00:00Z",
        "updated_at": "2026-07-08T00:01:00Z"
    })
}

/// Answers `session.list` with the one session; every other method gets an
/// empty result, which the CLI may reject after the request was served.
fn result_for(request: &Request) -> Value {
    match request.method() {
        protocol::method::SESSION_LIST => json!([session()]),
        _ => json!({}),
    }
}

fn serve(listener: TcpListener, served: Served, lists: ListParams) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&served);
            let lists = Arc::clone(&lists);
            thread::spawn(move || serve_connection(stream, &served, &lists));
        }
    });
}

fn serve_connection(stream: TcpStream, served: &Served, lists: &ListParams) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).expect("read a fixture line") == 0 {
            return;
        }
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse a request");
        served
            .lock()
            .expect("served requests")
            .push(request.method().to_owned());
        if request.method() == protocol::method::SESSION_LIST {
            lists
                .lock()
                .expect("served list params")
                .push(request.params().clone());
        }
        let response = Response::ok(PROTOCOL_VERSION, request.id(), result_for(&request))
            .expect("valid fixture response");
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize the response")
        )
        .expect("write the fixture response");
    }
}

/// What one CLI run did: the methods the first peer's daemon served and the
/// overlay addresses the CLI dialed.
struct Run {
    served: Vec<String>,
    lists: Vec<Value>,
    dialed: Vec<String>,
    diagnostics: String,
}

/// Runs `pohunek --host peer-a session <args>` with the name moving to the
/// second peer after the first dialed connection.
fn run_session_command(args: &[&str]) -> Run {
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
    let (library, capture) = connect_redirect::build(&root);
    let before = pin_overlay::status(
        None,
        &[
            pin_overlay::Peer::new("peer-a.netbird.test", FIRST_IP, "key-first"),
            pin_overlay::Peer::new("peer-y.netbird.test", SECOND_IP, "key-second"),
        ],
    );
    let after = pin_overlay::status(
        None,
        &[
            pin_overlay::Peer::new("peer-x-old.netbird.test", FIRST_IP, "key-first"),
            pin_overlay::Peer::new("peer-a.netbird.test", SECOND_IP, "key-second"),
        ],
    );
    let path = pin_overlay::install(&root.join("bin"), &capture, &before, &after);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let served = Served::default();
    let lists = ListParams::default();
    serve(listener, Arc::clone(&served), Arc::clone(&lists));

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &path)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", port.to_string())
        .args(["--host", "peer-a", "session"])
        .args(args)
        .output()
        .expect("run pohunek session");
    let diagnostics = format!(
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let served = served.lock().expect("served requests").clone();
    let lists = lists.lock().expect("served list params").clone();
    let dialed = pin_overlay::dialed_ips(&capture);
    Run {
        served,
        lists,
        dialed,
        diagnostics,
    }
}

/// Asserts that `method` was served after the name lookup and that no
/// connection followed the name to the second peer.
fn assert_pinned(label: &str, args: &[&str], method: &str) {
    let run = run_session_command(args);
    assert!(
        run.served.iter().any(|served| served == method),
        "{label}: {method} reached the first peer's daemon: {:?} ({})",
        run.served,
        run.diagnostics
    );
    assert!(
        run.dialed.len() >= 2,
        "{label}: the lookup and the action use separate connections: {:?}",
        run.dialed
    );
    assert!(
        run.dialed.iter().all(|ip| ip == FIRST_IP),
        "{label}: no connection follows the name to the second peer: {:?}",
        run.dialed
    );
}

#[test]
fn a_command_targeting_an_exact_id_never_asks_for_the_unfiltered_list() {
    let run = run_session_command(&["screen", SESSION_ID, "--json"]);

    assert!(
        run.served
            .iter()
            .any(|served| served == protocol::method::SESSION_SCREEN),
        "the screen request reached the daemon: {:?} ({})",
        run.served,
        run.diagnostics
    );
    assert!(
        !run.lists.is_empty(),
        "the target was confirmed with session.list ({})",
        run.diagnostics
    );
    for params in &run.lists {
        assert_eq!(
            params["filters"],
            json!([{"key": "id", "value": SESSION_ID}]),
            "an exact id resolves through an id-filtered request: {params}"
        );
    }
}

#[test]
fn a_command_targeting_a_display_name_falls_back_to_the_unfiltered_list() {
    let run = run_session_command(&["screen", SESSION_NAME, "--json"]);

    assert_eq!(run.lists.len(), 2, "{:?}", run.lists);
    assert_eq!(
        run.lists[0]["filters"],
        json!([{"key": "id", "value": SESSION_NAME}]),
        "the id-filtered lookup comes first"
    );
    let full = run
        .lists
        .last()
        .is_some_and(|params| params["filters"].as_array().is_none_or(Vec::is_empty));
    assert!(
        full,
        "a name is resolved from the full list: {:?} ({})",
        run.lists, run.diagnostics
    );
    assert!(
        run.served
            .iter()
            .any(|served| served == protocol::method::SESSION_SCREEN),
        "{:?}",
        run.served
    );
}

#[test]
fn a_read_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "screen",
        &["screen", SESSION_NAME, "--json"],
        protocol::method::SESSION_SCREEN,
    );
}

#[test]
fn a_mutating_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "stop",
        &["stop", SESSION_NAME, "--json"],
        protocol::method::SESSION_STOP,
    );
}

#[test]
fn session_input_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "input",
        &["input", SESSION_NAME, "hello", "--json"],
        protocol::method::SESSION_INPUT,
    );
}

#[test]
fn the_inspect_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "inspect",
        &["inspect", SESSION_NAME, "--json"],
        protocol::method::SESSION_INSPECT,
    );
}

#[test]
fn the_detection_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "detection",
        &["detection", SESSION_NAME, "--json"],
        protocol::method::SESSION_DETECTION,
    );
}

#[test]
fn the_read_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "read",
        &["read", SESSION_NAME, "--json"],
        protocol::method::SESSION_READ,
    );
}

#[test]
fn the_resume_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "resume",
        &["resume", SESSION_NAME, "--json"],
        protocol::method::SESSION_RESUME,
    );
}

#[test]
fn the_fork_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "fork",
        &["fork", SESSION_NAME, "--json"],
        protocol::method::SESSION_FORK,
    );
}

#[test]
fn the_rm_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "rm",
        &["rm", SESSION_NAME, "--json"],
        protocol::method::SESSION_REMOVE,
    );
}

#[test]
fn the_resize_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "resize",
        &[
            "resize",
            SESSION_NAME,
            "--cols",
            "100",
            "--rows",
            "30",
            "--json",
        ],
        protocol::method::SESSION_RESIZE,
    );
}

#[test]
fn the_metadata_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "metadata",
        &["metadata", SESSION_NAME, "--set", "k=v", "--json"],
        protocol::method::SESSION_SET_METADATA,
    );
}

#[test]
fn the_rename_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "rename",
        &["rename", SESSION_NAME, "beta", "--json"],
        protocol::method::SESSION_RENAME,
    );
}

#[test]
fn the_diff_command_reaches_the_peer_the_name_first_selected() {
    assert_pinned(
        "diff",
        &["diff", SESSION_NAME, "--json"],
        protocol::method::SESSION_DIFF,
    );
}
