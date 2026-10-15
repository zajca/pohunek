//! Binary-level scenario for a `session new` whose outcome is unknown.
//!
//! The built CLI creates a session on a fixture daemon that never answers the
//! `session.new` request, as a daemon still running a slow create hook does.
//! The CLI must report the timeout with the caller's way to find the session
//! the daemon may still create.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// Response deadline the caller gives the creation: far shorter than the
/// fixture, which never answers it.
const REQUEST_TIMEOUT_MS: &str = "200";

/// Mode of the runtime directory the client expects to be owner-private.
const PRIVATE_DIR_MODE: u32 = 0o700;

#[test]
fn a_session_new_without_a_reply_reports_how_to_find_the_session() {
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
    serve_fixture_daemon(&root.join("run/pohunek/daemon.sock"));

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args([
            "session",
            "new",
            "--agent",
            "shell",
            "--meta",
            "launch=1014",
            "--request-timeout-ms",
            REQUEST_TIMEOUT_MS,
            "--json",
        ])
        .output()
        .expect("run pohunek session new");

    assert!(!output.status.success(), "the creation is not confirmed");
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON error document");
    let error = &document["err"];
    assert_eq!(error["code"], "request_timeout", "{document}");
    let recover = error["recover"].as_str().expect("a recovery hint");
    assert!(recover.contains("--meta-eq"), "{recover}");
    assert!(
        recover.contains("an empty list does not prove that nothing was created"),
        "{recover}"
    );
}

/// Answers `daemon.health` on `socket` and never answers `session.new`.
fn serve_fixture_daemon(socket: &Path) {
    let listener = UnixListener::bind(socket).expect("bind the fixture daemon");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            thread::spawn(move || serve_connection(stream));
        }
    });
}

fn serve_connection(stream: UnixStream) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse a request");
        let result = match request.method() {
            protocol::method::DAEMON_HEALTH => json!({
                "status": "ok",
                "daemon_version": "fixture",
                "protocol_version": PROTOCOL_VERSION.get()
            }),
            // The create is still running; the reply never comes.
            protocol::method::SESSION_NEW => continue,
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
