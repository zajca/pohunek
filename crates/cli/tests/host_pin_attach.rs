//! `pohunek attach` against a remote host whose name changes peer mid-operation.
//!
//! The built CLI attaches to a fixture daemon reached through a fake `netbird`.
//! After the first connection was dialed, the fake moves the short name to
//! another peer. Every later connection of the attach (automatic re-attach,
//! live status subscription, menu action) must still reach the first peer; the
//! dialed `NetBird` addresses show which peer each connection used.

// Rust guideline compliant 2026-10-20

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[path = "support/pin_overlay.rs"]
mod pin_overlay;

/// Session the CLI attaches to.
const SESSION_ID: &str = "s-pin-attach-1";

/// Stream id the fixture daemon hands out for every attach.
const STREAM_ID: &str = "pin-attach-stream";

/// Peer the name `peer-a` selects before the first connection.
const FIRST_IP: &str = "100.64.0.11";

/// Peer the name `peer-a` selects after the first connection.
const SECOND_IP: &str = "100.64.0.12";

/// Automatic re-attach attempts the CLI makes after the stream closes.
const RECONNECT_ATTEMPTS: usize = 3;

/// Terminal height; the status line and menu need at least five rows.
const TERMINAL_ROWS: u16 = 24;

/// Terminal width of the PTY scenario.
const TERMINAL_COLS: u16 = 80;

/// Byte that opens the attach menu (Ctrl-\).
const MENU_KEY: u8 = 0x1c;

/// Pause between menu keys, longer than the client's shortcut sequence timeout
/// so each key arrives as its own input chunk.
const KEY_PAUSE: std::time::Duration = std::time::Duration::from_millis(300);

/// Mode of the runtime and state directories the client expects private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Requests the fixture daemon served, by method, in arrival order.
type Served = Arc<Mutex<Vec<String>>>;

/// Kills the CLI if the scenario fails before it exits.
struct CliProcess(Child);

impl Drop for CliProcess {
    fn drop(&mut self) {
        let _killed = self.0.kill();
        let _reaped = self.0.wait();
    }
}

/// A hermetic home, the fake overlay and the fixture daemon.
struct Scene {
    env: TestEnv,
    path: std::ffi::OsString,
    capture: PathBuf,
    library: PathBuf,
    port: u16,
    served: Served,
}

impl Scene {
    /// `hold_streams` keeps every raw attach stream open until the client
    /// closes it; otherwise a stream closes right after its prelude.
    fn new(hold_streams: bool) -> Self {
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
        serve(listener, Arc::clone(&served), hold_streams);
        Self {
            env,
            path,
            capture,
            library,
            port,
            served,
        }
    }

    fn command(&self) -> std::process::Command {
        let mut command = self.env.command(pohunek_test_support::bin_exe("pohunek"));
        command
            .env("PATH", &self.path)
            .env("LD_PRELOAD", &self.library)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &self.capture)
            .env("POHUNEK_REMOTE_PORT", self.port.to_string())
            .args(["--host", "peer-a", "attach", SESSION_ID]);
        command
    }

    fn served(&self) -> Vec<String> {
        self.served.lock().expect("served requests").clone()
    }

    /// Addresses dialed so far, whose first entry is the attach itself.
    fn dialed(&self) -> Vec<String> {
        pin_overlay::dialed_ips(&self.capture)
    }
}

/// Serves control requests and raw attach streams on `listener`.
///
/// A raw attach stream closes right after its prelude, which the client takes
/// as a lost stream; a `subscribe` request keeps its connection open.
fn serve(listener: TcpListener, served: Served, hold_streams: bool) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&served);
            thread::spawn(move || serve_connection(stream, &served, hold_streams));
        }
    });
}

fn serve_connection(stream: TcpStream, served: &Served, hold_streams: bool) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).expect("read a fixture line") == 0 {
            return;
        }
        let first: Value = serde_json::from_str(line.trim_end()).expect("parse a fixture line");
        if first.get("attach").and_then(Value::as_str) == Some(STREAM_ID) {
            served
                .lock()
                .expect("served requests")
                .push("raw.attach".to_owned());
            if hold_streams {
                let mut input = Vec::new();
                let _ = std::io::Read::read_to_end(&mut reader, &mut input);
            }
            return;
        }
        let request: Request = serde_json::from_value(first).expect("parse a request");
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
        if request.method() == protocol::method::SUBSCRIBE {
            // Hold the subscription open until the client closes it.
            let mut rest = String::new();
            let _ = reader.read_line(&mut rest);
            return;
        }
    }
}

fn session() -> Value {
    json!({
        "id": SESSION_ID,
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
        "runtime": {
            "state": "live",
            "runtime_generation": "3",
            "worker_id": "worker-fixture-1",
            "worker_instance_id": "runtime-fixture-1"
        }
    })
}

fn result_for(request: &Request) -> Value {
    match request.method() {
        protocol::method::DAEMON_HEALTH => json!({
            "status": "ok",
            "daemon_version": "fixture",
            "protocol_version": PROTOCOL_VERSION.get()
        }),
        protocol::method::SESSION_ATTACH => json!({ "stream_id": STREAM_ID }),
        protocol::method::SESSION_DETACH => json!({ "detached": true, "error": null }),
        protocol::method::SESSION_INSPECT => session(),
        protocol::method::SESSION_STOP => json!({ "stopped": true }),
        protocol::method::SUBSCRIBE => json!({ "subscribed": true }),
        // Terminal resize and the like: acknowledged, and recorded by method.
        _ => json!({}),
    }
}

#[test]
fn the_automatic_reattach_stays_on_the_peer_the_name_first_selected() {
    let scene = Scene::new(false);
    let mut command = scene.command();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut cli = CliProcess(command.spawn().expect("spawn pohunek attach"));
    let stdin = cli.0.stdin.take().expect("piped stdin");
    // The CLI's runtime waits for its blocking stdin read on exit, so the
    // pipe closes once the last attempt was served.
    wait_for_count(
        &scene,
        protocol::method::SESSION_DETACH,
        1 + RECONNECT_ATTEMPTS,
    );
    drop(stdin);

    let status = poll_until("the attach to end", || {
        cli.0.try_wait().expect("poll pohunek attach")
    });
    assert!(status.success(), "the attach gives up quietly");

    let served = scene.served();
    let attaches = served
        .iter()
        .filter(|method| *method == protocol::method::SESSION_ATTACH)
        .count();
    assert_eq!(
        attaches,
        1 + RECONNECT_ATTEMPTS,
        "the stream is lost every time: {served:?}"
    );
    let dialed = scene.dialed();
    assert!(
        dialed.len() > 1 + RECONNECT_ATTEMPTS,
        "every probe and re-attach dials a connection: {dialed:?}"
    );
    assert!(
        dialed.iter().all(|ip| ip == FIRST_IP),
        "no connection of the attach follows the name to the second peer: {dialed:?}"
    );
}

#[test]
fn the_status_subscription_and_menu_action_stay_on_the_first_peer() {
    let scene = Scene::new(true);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: TERMINAL_ROWS,
            cols: TERMINAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");
    let std_command = scene.command();
    let mut command = CommandBuilder::new(std_command.get_program());
    command.env_clear();
    for (name, value) in std_command.get_envs() {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    for argument in std_command.get_args() {
        command.arg(argument);
    }
    command.cwd(scene.env.cwd());
    let mut child = pair.slave.spawn_command(command).expect("start the attach");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    thread::spawn(move || {
        let mut sink = [0_u8; 4096];
        while std::io::Read::read(&mut reader, &mut sink).is_ok_and(|read| read > 0) {}
    });
    let mut writer = pair.master.take_writer().expect("pty writer");

    wait_served(&scene, protocol::method::SUBSCRIBE);
    // Separate chunks: batched input never confirms a destructive action.
    for keys in [&[MENU_KEY][..], b"k", b"y"] {
        writer.write_all(keys).expect("type a menu key");
        writer.flush().expect("flush the key");
        // The client folds keys within its 25 ms shortcut window into one chunk.
        // timing-allowed: #1111 no screen output marks the moment a key was consumed
        thread::sleep(KEY_PAUSE);
    }
    wait_served(&scene, protocol::method::SESSION_STOP);

    let dialed = scene.dialed();
    let _killed = child.kill();
    let _reaped = child.wait();
    assert!(
        dialed.len() >= 3,
        "attach, subscription and menu action each dial a connection: {dialed:?}"
    );
    assert!(
        dialed.iter().all(|ip| ip == FIRST_IP),
        "no later connection follows the name to the second peer: {dialed:?}"
    );
}

/// Waits until the fixture served `method`.
fn wait_served(scene: &Scene, method: &str) {
    wait_for_count(scene, method, 1);
}

/// Waits until the fixture served `method` `count` times.
fn wait_for_count(scene: &Scene, method: &str, count: usize) {
    poll_until(&format!("{count} {method} request(s)"), || {
        (scene
            .served()
            .iter()
            .filter(|served| *served == method)
            .count()
            >= count)
            .then_some(())
    });
}
