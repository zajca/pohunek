//! `--all-hosts` fan-out on a machine that lists itself as a local host record.
//!
//! The overlay reports this machine next to its peers, so discovery returns a
//! `local` record for it. The fan-out already targets the local daemon
//! explicitly and must not contact this machine a second time through the
//! overlay address of that record.

// Rust guideline compliant 2026-10-20

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[path = "support/pin_overlay.rs"]
mod pin_overlay;

/// Overlay address of this machine.
const SELF_IP: &str = "100.64.0.7";

/// Overlay address of the one peer.
const PEER_IP: &str = "100.64.0.42";

/// Mode of the runtime and state directories the client expects private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Methods one fixture daemon served.
type Served = Arc<Mutex<Vec<String>>>;

fn answer(request: &Request) -> Value {
    match request.method() {
        protocol::method::DAEMON_HEALTH => json!({
            "status": "ok",
            "daemon_version": "fixture",
            "protocol_version": PROTOCOL_VERSION.get()
        }),
        protocol::method::NOTIFICATION_LIST => json!({ "notifications": [] }),
        other => panic!("the fan-out sent an unexpected request: {other}"),
    }
}

fn serve_lines<S: Read + Write + Send + 'static>(stream: S, served: Served) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
            let request: Request = serde_json::from_str(line.trim_end()).expect("a request");
            served
                .lock()
                .expect("served requests")
                .push(request.method().to_owned());
            let response =
                Response::ok(PROTOCOL_VERSION, request.id(), answer(&request)).expect("a response");
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::to_string(&response).expect("serialize the response")
            )
            .expect("write the response");
            line.clear();
        }
    });
}

fn count(served: &Served, method: &str) -> usize {
    served
        .lock()
        .expect("served requests")
        .iter()
        .filter(|served| *served == method)
        .count()
}

#[test]
fn the_local_record_is_not_contacted_a_second_time_through_the_overlay() {
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
    let local_served = Served::default();
    let unix = UnixListener::bind(root.join("run/pohunek/daemon.sock")).expect("bind the socket");
    let local_for_thread = Arc::clone(&local_served);
    thread::spawn(move || {
        for stream in unix.incoming() {
            serve_lines(
                stream.expect("accept a local connection"),
                Arc::clone(&local_for_thread),
            );
        }
    });
    let overlay_served = Served::default();
    let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the overlay fixture");
    let port = tcp.local_addr().expect("fixture address").port();
    let overlay_for_thread = Arc::clone(&overlay_served);
    thread::spawn(move || {
        for stream in tcp.incoming() {
            serve_lines(
                stream.expect("accept an overlay connection"),
                Arc::clone(&overlay_for_thread),
            );
        }
    });

    let (library, capture) = connect_redirect::build(&root);
    let status = pin_overlay::status(
        Some(("fixture-self.netbird.test", SELF_IP)),
        &[pin_overlay::Peer::new(
            "fixture-remote.netbird.test",
            PEER_IP,
            "remote-key",
        )],
    );
    let path = pin_overlay::install(&root.join("bin"), &capture, &status, &status);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &path)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", port.to_string())
        .args(["notifications", "list", "--all-hosts", "--json"])
        .output()
        .expect("run the fan-out");
    assert!(
        output.status.success(),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let document: Value = serde_json::from_slice(&output.stdout).expect("versioned JSON envelope");
    let hosts: Vec<(&str, bool)> = document["ok"]["hosts"]
        .as_array()
        .expect("per-host results")
        .iter()
        .map(|host| {
            (
                host["host_id"].as_str().expect("host id"),
                host["ok"].as_bool().expect("host outcome"),
            )
        })
        .collect();
    assert_eq!(
        hosts,
        [("local", true), ("netbird:peer~cmVtb3RlLWtleQ", true)],
        "the fan-out targets this machine once and the peer once, without an error row"
    );
    assert_eq!(
        count(&local_served, protocol::method::NOTIFICATION_LIST),
        1,
        "this machine is listed once, through its local socket"
    );
    assert_eq!(
        count(&overlay_served, protocol::method::NOTIFICATION_LIST),
        1,
        "only the peer is listed through the overlay"
    );
    let dialed = pin_overlay::dialed_ips(&capture);
    assert_eq!(
        dialed.iter().filter(|ip| *ip == SELF_IP).count(),
        1,
        "the overlay address of this machine is dialed only by the discovery probe: {dialed:?}"
    );
}
