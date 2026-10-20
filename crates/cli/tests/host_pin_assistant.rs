//! `pohunek assistant` against a remote host whose name changes peer between
//! the preparation and the launch.
//!
//! Preparation inspects the host and materializes the knowledge bundle; the
//! launch then creates the session with the prepared prompt. After the first
//! connection was dialed, the fake `netbird` moves the short name to another
//! peer. Both steps must reach the first peer.

// Rust guideline compliant 2026-10-20

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{HostCapabilities, Request, Response, PROTOCOL_VERSION};
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

/// Requests the fixture daemon served, by method, in arrival order.
type Served = Arc<Mutex<Vec<String>>>;

fn capabilities() -> HostCapabilities {
    HostCapabilities {
        daemon_version: "fixture".to_owned(),
        protocol_version: PROTOCOL_VERSION,
        supported_agents: vec!["codex".to_owned()],
        runtimes: vec![protocol::AgentRuntime {
            agent: "codex".to_owned(),
            agent_base: None,
            available: true,
            path: None,
            version: None,
            supported: None,
            config_home_id: None,
        }],
        git_available: true,
        worktree_supported: true,
        terminal_read_supported: true,
        output_read_supported: true,
        session_wait_supported: true,
        session_wait_turn_supported: true,
    }
}

fn session() -> Value {
    json!({
        "id": "s-pin-assistant-1",
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
        "updated_at": "2026-07-08T00:01:00Z",
        "applied_input": true
    })
}

fn result_for(request: &Request) -> Value {
    match request.method() {
        protocol::method::DAEMON_HEALTH => json!({
            "status": "ok",
            "daemon_version": "fixture",
            "protocol_version": PROTOCOL_VERSION.get()
        }),
        protocol::method::HOST_INSPECT => {
            serde_json::to_value(capabilities()).expect("serialize capabilities")
        }
        protocol::method::ASSISTANT_MATERIALIZE => json!({
            "bundle_path": "/remote/bundle",
            "snapshot_path": "/remote/snapshot.json",
            "version": knowledge::BUNDLE_VERSION,
            "content_hash": knowledge::bundle_content_hash(),
            "concepts": []
        }),
        protocol::method::SESSION_NEW => session(),
        other => panic!("the assistant sent an unexpected request: {other}"),
    }
}

fn serve(listener: TcpListener, served: Served) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&served);
            thread::spawn(move || serve_connection(stream, &served));
        }
    });
}

fn serve_connection(stream: TcpStream, served: &Served) {
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

#[test]
fn preparation_and_launch_reach_the_peer_the_name_first_selected() {
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
    serve(listener, Arc::clone(&served));

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &path)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", port.to_string())
        .args([
            "--host",
            "peer-a",
            "assistant",
            "--agent",
            "codex",
            "--project",
            "demo",
            "--no-snapshot",
            "--yes",
            "--json",
            "help me",
        ])
        .output()
        .expect("run pohunek assistant");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let served = served.lock().expect("served requests").clone();
    for method in [
        protocol::method::HOST_INSPECT,
        protocol::method::ASSISTANT_MATERIALIZE,
        protocol::method::SESSION_NEW,
    ] {
        assert!(
            served.iter().any(|served| served == method),
            "{method} reached the first peer's daemon: {served:?}"
        );
    }
    let dialed = pin_overlay::dialed_ips(&capture);
    assert!(
        dialed.len() >= 2,
        "preparation and launch use separate connections: {dialed:?}"
    );
    assert!(
        dialed.iter().all(|ip| ip == FIRST_IP),
        "no connection follows the name to the second peer: {dialed:?}"
    );
    assert!(
        !stdout.contains("peer~"),
        "the output never shows the pinned selector: {stdout}"
    );
}
