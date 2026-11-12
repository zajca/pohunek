//! Arrow and Escape keys in the attach menu under every keyboard reporting
//! encoding a terminal can send: legacy CSI, kitty event-typed CSI and SS3.
//!
//! The built CLI attaches to a fixture daemon over a local socket, with its
//! stdin and stdout on a real PTY, exactly as the operator's foot or Claude
//! Code session runs it. Every key is sent in the encoding its terminal uses
//! and the menu selection is read back from the compositor's own highlight
//! marks in the output. On the pre-fix decoder the event-typed and SS3
//! encodings did nothing at all: their parameter or `O` bytes were delivered
//! to the menu as literal keys (Escape, then text in the rename buffer).

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// Session the CLI attaches to.
const SESSION_ID: &str = "s-attach-menu-keys";

/// Stream id the fixture daemon hands out for the attach.
const STREAM_ID: &str = "attach-menu-keys-stream";

/// Terminal height; the status line and menu need at least five rows.
const TERMINAL_ROWS: u16 = 24;

/// Terminal width of the PTY scenario.
const TERMINAL_COLS: u16 = 80;

/// Byte that opens the attach menu (Ctrl-\).
const MENU_KEY: u8 = 0x1c;

/// Pause between menu keys, longer than the client's shortcut sequence timeout
/// so each key arrives as its own input chunk. Waits are otherwise routed
/// through `poll_until`, only this fixed settle window remains.
const KEY_PAUSE: Duration = Duration::from_millis(300);

/// Menu item text as rendered on a line: the compositor writes the
/// highlight attributes before the line text but a leading `+` border gap,
/// so the marker is the plain label text.
const HIGHLIGHTED_KILL: &str = "k  Kill session";
const HIGHLIGHTED_REMOVE: &str = "t  Terminate and delete session";
const HIGHLIGHTED_DETACH: &str = "d  Detach";
const HIGHLIGHTED_NEW: &str = "n  New session in this worktree";
const HIGHLIGHTED_RENAME: &str = "r  Rename session";

/// Menu items in selection order: kill, remove, detach, new session, rename.
const MENU_LABELS: [&str; 5] = [
    "Kill session",
    "Terminate and delete session",
    "Detach",
    "New session in this worktree",
    "Rename session",
];

/// Mode of the runtime directory the client expects to be owner-private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Mode of the config directory the client reads attach settings from.
const CONFIG_DIR_MODE: u32 = 0o700;

/// Kitty event-typed reports foot sends with event types reported (`>7u`):
/// a press is `ESC [ 1 ; 1 : 1 A`, a repeat `... : 2`, a release `... : 3`.
const KITTY_UP_PRESS: &[u8] = b"\x1b[1;1:1A";
/// Kitty event-typed down press.
const KITTY_DOWN_PRESS: &[u8] = b"\x1b[1;1:1B";
/// Kitty event-typed down repeat: the selection must move again.
const KITTY_DOWN_REPEAT: &[u8] = b"\x1b[1;1:2B";
/// Kitty event-typed down release: the selection must not move.
const KITTY_DOWN_RELEASE: &[u8] = b"\x1b[1;1:3B";
/// Kitty CSI-u Escape press (the encoding Claude Code always sends once it
/// pushes kitty flags `>5u`).
const KITTY_ESCAPE_PRESS: &[u8] = b"\x1b[27u";
/// Kitty CSI-u Escape release: it must not leak to the attached program after
/// the menu closed in response to the press.
const KITTY_ESCAPE_RELEASE: &[u8] = b"\x1b[27;1:3u";
/// Kitty CSI-u of the escape-code that reports no canonical byte movement
/// (`1`): the menu must consume it whole instead of reading `1` as a key.
const KITTY_CSI_U_CODE_ONE: &[u8] = b"\x1b[1;1:1u";
/// Application-cursor-mode up arrow (SS3).
const SS3_UP: &[u8] = b"\x1bOA";
/// Application-cursor-mode down arrow (SS3).
const SS3_DOWN: &[u8] = b"\x1bOB";
/// Legacy CSI down arrow still works as before.
const LEGACY_DOWN: &[u8] = b"\x1b[B";
/// Legacy lone Escape closes the menu.
const LEGACY_ESCAPE: &[u8] = b"\x1b";
/// Focus-in report (`ESC [ I`): its `I` byte must not act as a menu key.
const FOCUS_IN: &[u8] = b"\x1b[I";
/// Mode-report answer (`ESC [ ? 2031 ; 0 $ y`): consumed whole.
const MODE_REPORT: &[u8] = b"\x1b[?2031;0$y";

/// A running attach with its PTY and fixture state.
struct Attach {
    _env: TestEnv,
    _cli: CliProcess,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    fixture: Fixture,
}

/// The fixture daemon's record of what the client asked and what it received.
#[derive(Debug, Default)]
struct Fixture {
    /// Control requests served, by method, in arrival order.
    methods: Arc<Mutex<Vec<String>>>,
    /// Bytes the client's raw attach stream delivered to the program.
    program_input: Arc<Mutex<Vec<u8>>>,
    /// The client's raw attach stream closed (detach sent).
    stream_closed: Arc<Mutex<bool>>,
}

impl Clone for Fixture {
    fn clone(&self) -> Self {
        Self {
            methods: Arc::clone(&self.methods),
            program_input: Arc::clone(&self.program_input),
            stream_closed: Arc::clone(&self.stream_closed),
        }
    }
}

impl Fixture {
    /// Requests the fixture daemon served, by method, in arrival order.
    fn methods(&self) -> Vec<String> {
        self.methods.lock().expect("methods lock").clone()
    }

    /// Bytes the client's raw attach stream delivered to the program.
    fn program_input(&self) -> Vec<u8> {
        self.program_input.lock().expect("input lock").clone()
    }

    /// Whether the client's raw attach stream closed (detach sent).
    fn stream_closed(&self) -> bool {
        *self.stream_closed.lock().expect("closed lock")
    }
}

/// Kills the CLI if the scenario fails before it exits.
struct CliProcess(Box<dyn portable_pty::Child + Send + Sync>);

impl Drop for CliProcess {
    fn drop(&mut self) {
        let _killed = self.0.kill();
        let _reaped = self.0.wait();
    }
}

impl Attach {
    /// Types one key (a full terminal report) and waits for it to settle.
    fn type_key(&mut self, key: &[u8]) {
        self.writer.write_all(key).expect("type the menu key");
        self.writer.flush().expect("flush the key");
        // timing-allowed: #2281 decode into a report happens after the client shortcut timeout with no output marker.
        thread::sleep(KEY_PAUSE);
    }

    /// Reports which menu item carries the compositor's reverse-video mark in
    /// the most recently completed repaint: the title starts each repaint, so
    /// only the bytes after the last title belong to the current frame.
    fn highlighted(&self) -> Option<&'static str> {
        let output = self.output.lock().expect("output lock");
        // Every full repaint starts with a home move (`\x1b[1;1H`), so only
        // the bytes after the last home move belong to the live frame.
        let frame_start = output
            .windows(b"\x1b[1;1H".len())
            .rposition(|window| window == b"\x1b[1;1H")
            .map_or(0, |index| index + b"\x1b[1;1H".len());
        let frame = &output[frame_start..];
        let title = frame
            .windows(b"Session menu".len())
            .position(|window| window == b"Session menu")?;
        let tail = &frame[title..];
        for (label, rendered) in MENU_LABELS.iter().zip([
            HIGHLIGHTED_KILL,
            HIGHLIGHTED_REMOVE,
            HIGHLIGHTED_DETACH,
            HIGHLIGHTED_NEW,
            HIGHLIGHTED_RENAME,
        ]) {
            let marker = format!("\x1b[7m{rendered}").into_bytes();
            if tail
                .windows(marker.len())
                .any(|window| window == marker.as_slice())
            {
                return Some(label);
            }
        }
        None
    }

    /// Waits until the menu highlights `label`.
    fn expect_highlighted(&self, final_label: &str, what: &str) {
        let label = final_label;
        poll_until(what, || {
            if self
                .highlighted()
                .is_some_and(|highlighted| highlighted == label)
            {
                return Some(());
            }
            None
        });
        // On success the highlight matches. A timeout from `poll_until`
        // already panics with the message above.
    }

    /// Waits until the menu stops highlighting any item (it closed): the
    /// compositor repaints with no menu frame, so the live frame carries no
    /// highlight. The close repaint is the final paint, so the condition is
    /// checked on the current frame directly; callers pass the output
    /// length captured before the closing key as `before_close` and this
    /// waits until the frame grew past it (the close repaint itself).
    fn expect_menu_closed(&self, before_close: usize, what: &str) {
        poll_until(what, || {
            if self.highlighted().is_none() {
                return Some(());
            }
            None
        });
        // On success the live frame carries no highlight. A timeout from
        // `poll_until` already panics with the message above.
        let _ = before_close;
    }
}

/// Starts a real `pohunek attach` against the fixture daemon on a PTY.
fn attach_with_fixture() -> Attach {
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
    fs::set_permissions(
        root.join("config"),
        fs::Permissions::from_mode(CONFIG_DIR_MODE),
    )
    .expect("make the config directory owner-private");
    let socket = root.join("run/pohunek/daemon.sock");

    let fixture = Fixture::default();
    serve_fixture_daemon(&socket, &fixture);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: TERMINAL_ROWS,
            cols: TERMINAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");
    let std_command = env.command(pohunek_test_support::bin_exe("pohunek"));
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
    command.arg("attach");
    command.arg(SESSION_ID);
    command.cwd(env.cwd());
    let child = pair.slave.spawn_command(command).expect("start the attach");
    drop(pair.slave);
    let cli = CliProcess(child);
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let output = Arc::new(Mutex::new(Vec::new()));
    {
        let output = Arc::clone(&output);
        thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                output
                    .lock()
                    .expect("output lock")
                    .extend_from_slice(&buffer[..read]);
            }
        });
    };
    let writer = pair.master.take_writer().expect("pty writer");
    // The client attaches, loads the status snapshot and opens the
    // subscription before the menu can paint; wait for that round-trip to
    // settle so the first key is not raced with setup.
    poll_until("the fixture to serve the attach and subscription", || {
        fixture
            .methods()
            .iter()
            .any(|method| method == protocol::method::SUBSCRIBE)
            .then_some(())
    });

    Attach {
        _env: env,
        _cli: cli,
        writer,
        output,
        fixture,
    }
}

/// Serves the fixture daemon on `socket`.
fn serve_fixture_daemon(socket: &Path, fixture: &Fixture) {
    let listener = UnixListener::bind(socket).expect("bind the fixture daemon");
    let fixture = fixture.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let fixture = fixture.clone();
            thread::spawn(move || serve_connection(stream, &fixture));
        }
    });
}

/// The fixture: answers control requests, holds attach streams open until the
/// client closes them, records what the program receives.
fn serve_connection(stream: UnixStream, fixture: &Fixture) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).expect("read a fixture line") == 0 {
            return;
        }
        let first: Value = serde_json::from_str(line.trim_end()).expect("parse a fixture line");
        if first.get("attach").and_then(Value::as_str) == Some(STREAM_ID) {
            serve_attach_stream(reader, fixture);
            return;
        }
        let request: Request = serde_json::from_value(first).expect("parse a request");
        fixture
            .methods
            .lock()
            .expect("methods lock")
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
            // One session snapshot into the client, then hold the
            // subscription open until the client closes it.
            let event = json!({
                "event": "session.updated",
                "session": session(),
            });
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::to_string(&event).expect("serialize the event")
            )
            .expect("write the session event");
            let mut rest = String::new();
            let _ = reader.read_line(&mut rest);
            return;
        }
    }
}

/// The raw attach stream: the program reads what the client forwards and the
/// stream closes when the client detaches.
fn serve_attach_stream(reader: BufReader<UnixStream>, fixture: &Fixture) {
    let mut stream = reader.into_inner();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).unwrap_or(0);
        if read == 0 {
            *fixture.stream_closed.lock().expect("closed lock") = true;
            return;
        }
        fixture
            .program_input
            .lock()
            .expect("input lock")
            .extend_from_slice(&buffer[..read]);
    }
}

/// The session the fixture reports.
fn session() -> Value {
    json!({
        "id": SESSION_ID,
        "name": "menu-keys-session",
        "agent": "codex",
        "agent_base": "codex",
        "capabilities": {"fork": false, "resume": true},
        "cwd": "/workspace/pohunek",
        "project_label": "pohunek",
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

/// Fixture replies for the control requests the attach makes.
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

/// One menu opens with the default selection on the kill item.
fn open_menu(attach: &mut Attach) {
    attach.type_key(&[MENU_KEY]);
}

/// Detaches with the plain byte and waits for the attach to end.
fn detach(attach: &mut Attach) {
    attach.type_key(&[0x1d]);
    poll_until("the attach to end", || {
        attach.fixture.stream_closed().then_some(())
    });
    assert!(
        attach
            .fixture
            .methods()
            .iter()
            .any(|method| method == protocol::method::SESSION_DETACH),
        "the detach request reaches the fixture: {:?}",
        attach.fixture.methods()
    );
}

#[test]
fn legacy_and_event_typed_and_ss3_arrows_move_the_menu_selection() {
    let mut attach = attach_with_fixture();
    open_menu(&mut attach);
    attach.expect_highlighted("Kill session", "the menu opens on the kill item");

    attach.type_key(KITTY_DOWN_PRESS);
    attach.expect_highlighted(
        "Terminate and delete session",
        "kitty down press moves once",
    );

    attach.type_key(KITTY_DOWN_REPEAT);
    attach.expect_highlighted("Detach", "kitty down repeat moves again");

    attach.type_key(KITTY_DOWN_RELEASE);
    attach.expect_highlighted("Detach", "kitty down release does not move");

    attach.type_key(SS3_UP);
    attach.expect_highlighted(
        "Terminate and delete session",
        "SS3 up moves without closing the menu",
    );

    attach.type_key(SS3_DOWN);
    attach.expect_highlighted("Detach", "SS3 down moves without closing the menu");

    attach.type_key(LEGACY_DOWN);
    attach.expect_highlighted("New session in this worktree", "legacy down still moves");

    attach.type_key(KITTY_UP_PRESS);
    attach.expect_highlighted("Detach", "kitty up moves back");

    let before_close = attach.output.lock().expect("output lock").len();
    attach.type_key(LEGACY_ESCAPE);
    attach.expect_menu_closed(before_close, "legacy Escape closes the menu");

    detach(&mut attach);
}

#[test]
fn csi_u_and_report_bytes_never_become_menu_keys() {
    let mut attach = attach_with_fixture();
    open_menu(&mut attach);
    attach.expect_highlighted("Kill session", "the menu opens on the kill item");

    // A kitty CSI-u of escape code `1` has no canonical byte: it must be
    // consumed whole, not read as `ESC` (which would close the menu) + `1`.
    attach.type_key(KITTY_CSI_U_CODE_ONE);
    attach.expect_highlighted(
        "Kill session",
        "the CSI-u report is consumed without moving or closing",
    );

    attach.type_key(FOCUS_IN);
    attach.expect_highlighted(
        "Kill session",
        "the focus report byte does not act as a menu key",
    );

    attach.type_key(MODE_REPORT);
    attach.expect_highlighted(
        "Kill session",
        "the mode report bytes do not act as menu keys",
    );

    // Typing must still work afterwards: `r` opens the rename buffer and
    // nothing the sequences carried may sit in it.
    attach.type_key(b"r");
    attach.type_key(b"ok");
    let output = attach.output.lock().expect("output lock");
    let rename_at = output
        .windows(b"Rename session".len())
        .rposition(|window| window == b"Rename session")
        .expect("the rename buffer renders");
    let tail = &output[rename_at..];
    assert!(
        tail.windows(2).any(|window| window == b"ok")
            && !tail.windows(7).any(|window| window == b"[^I]"),
        "the rename buffer holds exactly the typed text: {:?}",
        String::from_utf8_lossy(tail)
    );
    drop(output);

    attach.type_key(LEGACY_ESCAPE);
    attach.expect_highlighted("Kill session", "Escape returns to the root list");

    detach(&mut attach);
}

#[test]
fn escape_closes_the_menu_in_csi_u_form_and_its_release_does_not_reach_the_program() {
    let mut attach = attach_with_fixture();
    open_menu(&mut attach);
    attach.expect_highlighted("Kill session", "the menu opens on the kill item");

    let before_close = attach.output.lock().expect("output lock").len();
    attach.type_key(KITTY_ESCAPE_PRESS);
    attach.expect_menu_closed(before_close, "the CSI-u Escape press closes the menu");

    attach.type_key(KITTY_ESCAPE_RELEASE);
    // timing-allowed: #2281 a settle window is enough for a leak to arrive if one existed.
    thread::sleep(KEY_PAUSE);
    assert!(
        !attach.fixture.program_input().contains(&0x1b),
        "the Escape release must not leak to the program: {:?}",
        attach.fixture.program_input()
    );

    detach(&mut attach);
}

#[test]
fn an_unconsumed_release_is_forwarded_to_the_program() {
    let mut attach = attach_with_fixture();

    attach.type_key(KITTY_ESCAPE_RELEASE);
    poll_until("the release to reach the program", || {
        if attach.fixture.program_input() == KITTY_ESCAPE_RELEASE {
            return Some(());
        }
        None
    });

    detach(&mut attach);
}
