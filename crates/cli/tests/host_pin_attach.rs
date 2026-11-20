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
    /// `refuse_inspect_after_first_detach` answers the first post-detach
    /// `session.inspect` with `session_not_found`: the session was removed
    /// while the stream was down.
    fn new(hold_streams: bool) -> Self {
        Self::with_removed_before_probe(hold_streams, false)
    }

    fn with_removed_before_probe(
        hold_streams: bool,
        refuse_inspect_after_first_detach: bool,
    ) -> Self {
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
        let inspect_refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        serve(
            listener,
            Arc::clone(&served),
            hold_streams,
            refuse_inspect_after_first_detach,
            Arc::clone(&inspect_refused),
        );
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
/// as a lost stream; a `subscribe` request keeps its connection open. When
/// `refuse_inspect_after_first_detach` holds, the first post-detach
/// `session.inspect` is refused with `session_not_found` and counted in
/// `inspect_refused`, so the scenario can assert the remote end of the
/// removed-before-probe path.
fn serve(
    listener: TcpListener,
    served: Served,
    hold_streams: bool,
    refuse_inspect_after_first_detach: bool,
    inspect_refused: Arc<std::sync::atomic::AtomicBool>,
) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&served);
            let inspect_refused = Arc::clone(&inspect_refused);
            thread::spawn(move || {
                serve_connection(
                    stream,
                    &served,
                    hold_streams,
                    refuse_inspect_after_first_detach,
                    &inspect_refused,
                );
            });
        }
    });
}

fn serve_connection(
    stream: TcpStream,
    served: &Served,
    hold_streams: bool,
    refuse_inspect_after_first_detach: bool,
    inspect_refused: &Arc<std::sync::atomic::AtomicBool>,
) {
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
        let detached = served
            .lock()
            .expect("served requests")
            .iter()
            .any(|method| method == protocol::method::SESSION_DETACH);
        if refuse_inspect_after_first_detach
            && request.method() == protocol::method::SESSION_INSPECT
            && detached
            && !inspect_refused.load(std::sync::atomic::Ordering::SeqCst)
        {
            // The session was removed while the stream was down: refuse the
            // first post-detach probe as a gone session at once, instead of
            // reporting it running.
            inspect_refused.store(true, std::sync::atomic::Ordering::SeqCst);
            let response = Response::err(
                PROTOCOL_VERSION,
                request.id(),
                protocol::ProtocolError::new(
                    protocol::ErrorClass::Runtime,
                    "session_not_found",
                    format!("session not found: {SESSION_ID}"),
                    None,
                ),
            )
            .expect("valid fixture response");
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::to_string(&response).expect("serialize the response")
            )
            .expect("write the fixture response");
            continue;
        }
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
fn a_session_removed_before_the_reconnect_probe_ends_the_remote_attach() {
    let scene = Scene::with_removed_before_probe(false, true);
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
    let (mut reader, writer) = (
        pair.master.try_clone_reader().expect("pty reader"),
        pair.master.take_writer().expect("pty writer"),
    );
    let mut output = Vec::new();
    // Drop the writer only once the peer closes: closing it early would
    // hang up the PTY while the attach still runs.
    let reader_thread = thread::spawn(move || {
        let writer = writer;
        let mut buffer = [0_u8; 4096];
        while let Ok(read) = std::io::Read::read(&mut reader, &mut buffer) {
            if read == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..read]);
        }
        drop(writer);
        output
    });
    // The stream closes right after its prelude; the client detaches and
    // probes, and the fixture refuses the first post-detach probe with
    // `session_not_found`. The attach must end at once with exit 0.
    let status = poll_until("the remote attach to end", || {
        child.try_wait().expect("poll the remote attach")
    });
    let output = reader_thread.join().expect("join the pty reader");
    let output = String::from_utf8_lossy(&output).into_owned();
    assert!(
        status.success(),
        "a removed session ends the remote attach with exit 0: {output:?}"
    );
    assert!(
        output.contains(&format!(
            "[pohunek] session {SESSION_ID} removed; attach ended"
        )),
        "the attach reports the removed session: {output:?}"
    );
    let served = scene.served();
    let attaches = served
        .iter()
        .filter(|method| *method == protocol::method::SESSION_ATTACH)
        .count();
    assert_eq!(
        attaches, 1,
        "no re-attach is attempted after the removal: {served:?}"
    );
    let dialed = scene.dialed();
    assert!(
        dialed.iter().all(|ip| ip == FIRST_IP),
        "no connection of the attach follows the name to the second peer: {dialed:?}"
    );
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
