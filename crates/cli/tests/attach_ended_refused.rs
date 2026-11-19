//! Binary-level scenarios for an attach whose session ends around the
//! reconnect path.
//!
//! The fixture daemon mimics the `session stop` race of zajca/pohunek#1184
//! case d: it answers the first `session.inspect` after the stream died with
//! `state: running` (so the client decides `Reattach`), refuses the second
//! `session.attach` with `session_not_running` (the stop committed between
//! the two RPCs), and then answers the confirming `session.inspect` with
//! `state: stopped`. The fixture's answers are a pure function of the request
//! sequence the client has already sent — the state gate is the fixture's
//! own mutex, so the probe=Running then attach=refused interleaving holds
//! on every run with no timing involved.
//!
//! A second scenario covers the first attach: a target that is already gone
//! is refused with `session_not_found`, and the client must surface that
//! refusal unchanged (non-zero exit) instead of reporting an ended session.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use protocol::{ErrorClass, ProtocolError, Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// Session the CLI attaches to.
const SESSION_ID: &str = "s-attach-ended-refused";

/// Stream id the fixture daemon hands out for the first attach.
const STREAM_ID: &str = "attach-ended-refused-stream";

/// Terminal size of the PTY scenario: rows must clear the status modal floor.
const TERMINAL_ROWS: u16 = 24;

/// Terminal width of the PTY scenario.
const TERMINAL_COLS: u16 = 80;

/// Mode of the runtime directory the client expects to be owner-private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Mode of the config directory the client reads attach settings from.
const CONFIG_DIR_MODE: u32 = 0o700;

/// Message the fixed client prints once the confirming inspect sees the stop.
const ENDED_MESSAGE: &str = "[pohunek] session s-attach-ended-refused stopped; attach ended";

/// Phase of the refused-reattach sequence the fixture has reached.
#[derive(Debug, Default)]
struct Fixture {
    /// Control requests served, by method, in arrival order.
    methods: Arc<Mutex<Vec<String>>>,
    /// `session.attach` calls served so far.
    attaches: Arc<Mutex<usize>>,
    /// The raw attach stream open before the stop kills it.
    stream: Arc<Mutex<Option<UnixStream>>>,
    /// The client sent `session.detach` after reading the stream EOF.
    detached: Arc<Mutex<bool>>,
    /// Refuse the first `session.attach` instead of minting a stream token:
    /// models a first attach to a missing session (H1 guard scenario).
    refuse_first_attach: bool,
}

impl Clone for Fixture {
    fn clone(&self) -> Self {
        Self {
            methods: Arc::clone(&self.methods),
            attaches: Arc::clone(&self.attaches),
            stream: Arc::clone(&self.stream),
            detached: Arc::clone(&self.detached),
            refuse_first_attach: self.refuse_first_attach,
        }
    }
}

impl Fixture {
    /// Requests the fixture daemon served, by method, in arrival order.
    fn methods(&self) -> Vec<String> {
        self.methods.lock().expect("methods lock").clone()
    }

    /// Records a served method.
    fn record(&self, method: &str) {
        self.methods
            .lock()
            .expect("methods lock")
            .push(method.to_owned());
    }

    /// Whether the client sent `session.detach` after the stream EOF.
    fn detached(&self) -> bool {
        *self.detached.lock().expect("detached lock")
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

/// Starts a real `pohunek attach` against the fixture daemon on a PTY and
/// returns its exit status, combined output, and the fixture record.
///
/// When `refuse_first_attach` is set, the fixture refuses the first
/// `session.attach` with `session_not_found` and the client must exit
/// non-zero with that code; otherwise the fixture drives the full
/// refused-reattach sequence.
fn refused_reattach_scenario(refuse_first_attach: bool) -> (bool, String, Fixture) {
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

    let fixture = Fixture {
        refuse_first_attach,
        ..Fixture::default()
    };
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
    let _env = env;
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let output = Arc::new(Mutex::new(Vec::new()));
    // The reader is joined after the child exits, before the output is
    // asserted: the child writes the ended message and then exits, and
    // nothing orders a detached reader's last chunk before the exit poll.
    let output_writer = Arc::clone(&output);
    let reader_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 {
                break;
            }
            output_writer
                .lock()
                .expect("output lock")
                .extend_from_slice(&buffer[..read]);
        }
    });

    if !refuse_first_attach {
        // The client attaches and opens the subscription before the
        // reconnect path is armed; wait for that round-trip to settle so the
        // kill is not raced with setup.
        poll_until("the fixture to serve the attach and subscription", || {
            fixture
                .methods()
                .iter()
                .any(|method| method == protocol::method::SUBSCRIBE)
                .then_some(())
        });
        // The stop kills the raw stream while the session is still listed as
        // running; the client reads EOF on its next read.
        fixture
            .stream
            .lock()
            .expect("stream lock")
            .take()
            .expect("the attach stream is open")
            .shutdown(std::net::Shutdown::Both)
            .expect("kill the attach stream");
    }

    // The client must end on its own once the stop commits: the failed run
    // without the fix still ends (with exit 1 after the refusing re-attach),
    // so `poll_until` on the exit status cannot hang the suite either way.
    let mut cli = cli;
    let status = poll_until("the attach client to exit after the stop", || {
        cli.0.try_wait().expect("poll the attach client")
    });
    let exited_zero = status.success();
    drop(cli);
    reader_thread.join().expect("join the pty reader");
    let bytes = output.lock().expect("output lock").clone();
    (
        exited_zero,
        String::from_utf8_lossy(&bytes).into_owned(),
        fixture,
    )
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

/// The fixture: answers control requests, holds the first attach stream open
/// until the scenario kills it, refuses the re-attach.
fn serve_connection(stream: UnixStream, fixture: &Fixture) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).expect("read a fixture line") == 0 {
            return;
        }
        let first: Value = serde_json::from_str(line.trim_end()).expect("parse a fixture line");
        if first.get("attach").and_then(Value::as_str) == Some(STREAM_ID) {
            *fixture.stream.lock().expect("stream lock") = Some(stream);
            return;
        }
        let request: Request = serde_json::from_value(first).expect("parse a request");
        fixture.record(request.method());
        let response = result_for(&request, fixture);
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize the response")
        )
        .expect("write the fixture response");
        if request.method() == protocol::method::SESSION_DETACH {
            *fixture.detached.lock().expect("detached lock") = true;
        }
        if request.method() == protocol::method::SUBSCRIBE {
            let _ = reader.read_line(&mut String::new());
            return;
        }
    }
}

/// Fixture replies for the control requests the attach makes.
fn result_for(request: &Request, fixture: &Fixture) -> Response {
    match request.method() {
        protocol::method::DAEMON_HEALTH => Response::ok(
            PROTOCOL_VERSION,
            request.id(),
            json!({
                "status": "ok",
                "daemon_version": "fixture",
                "protocol_version": PROTOCOL_VERSION.get()
            }),
        ),
        protocol::method::SESSION_ATTACH => {
            if fixture.refuse_first_attach {
                // A first attach to a missing session: the refusal must
                // surface unchanged, never as an ended session.
                return Response::err(
                    PROTOCOL_VERSION,
                    request.id(),
                    ProtocolError::new(
                        ErrorClass::Runtime,
                        "session_not_found",
                        format!("session not found: {SESSION_ID}"),
                        None,
                    ),
                )
                .expect("valid fixture response");
            }
            let mut attaches = fixture.attaches.lock().expect("attaches lock");
            *attaches += 1;
            if *attaches == 1 {
                Response::ok(
                    PROTOCOL_VERSION,
                    request.id(),
                    json!({ "stream_id": STREAM_ID }),
                )
            } else {
                // The stop committed between the probe inspect and this
                // re-attach: the session is no longer attachable.
                Response::err(
                    PROTOCOL_VERSION,
                    request.id(),
                    ProtocolError::new(
                        ErrorClass::Runtime,
                        "session_not_running",
                        format!("session is not running: {SESSION_ID}"),
                        None,
                    ),
                )
            }
        }
        protocol::method::SESSION_DETACH => Response::ok(
            PROTOCOL_VERSION,
            request.id(),
            json!({ "detached": true, "error": null }),
        ),
        protocol::method::SESSION_INSPECT => {
            let probe_after_eof = fixture.detached();
            let state = if probe_after_eof {
                // The first post-EOF inspect is the reconnect probe: the stop
                // has not committed yet, so the client decides `Reattach`.
                let mut attaches = fixture.attaches.lock().expect("attaches lock");
                let first_probe = *attaches == 1;
                if first_probe {
                    *attaches += 100;
                }
                if first_probe {
                    "running"
                } else {
                    "stopped"
                }
            } else {
                "running"
            };
            Response::ok(PROTOCOL_VERSION, request.id(), session(state))
        }
        protocol::method::SUBSCRIBE => Response::ok(
            PROTOCOL_VERSION,
            request.id(),
            json!({ "subscribed": true }),
        ),
        // Terminal resize and the like: acknowledged, and recorded by method.
        _ => Response::ok(PROTOCOL_VERSION, request.id(), json!({})),
    }
    .expect("valid fixture response")
}

/// The session the fixture reports in `state`.
fn session(state: &str) -> Value {
    json!({
        "id": SESSION_ID,
        "name": "ended-refused-session",
        "agent": "shell",
        "agent_base": "shell",
        "capabilities": {"fork": false, "resume": true},
        "cwd": "/workspace/pohunek",
        "project_label": "pohunek",
        "pid": 42424,
        "state": state,
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

#[test]
fn an_attach_whose_session_ends_between_probe_and_reattach_exits_zero() {
    let (exited_zero, output, fixture) = refused_reattach_scenario(false);

    assert!(
        exited_zero,
        "the attach ends with exit 0 once the stop commits: {output:?}"
    );
    assert!(
        output.contains(ENDED_MESSAGE),
        "the attach reports the ended session: {output:?}"
    );
    assert!(
        !output.contains("daemon error"),
        "no daemon error is reported for an ended session: {output:?}"
    );
    let methods = fixture.methods();
    let tail: Vec<&str> = methods
        .iter()
        .skip_while(|method| *method != protocol::method::SESSION_DETACH)
        .skip(1)
        .map(String::as_str)
        .collect();
    // After the stream EOF the client detaches, probes (running), re-attaches
    // (refused), and confirms the end (stopped): no other request may run in
    // between. Probe-inspect counts alone cannot prove the confirming inspect,
    // because the status snapshot and the status updater inspect earlier.
    assert_eq!(
        tail,
        [
            protocol::method::SESSION_INSPECT,
            protocol::method::SESSION_ATTACH,
            protocol::method::SESSION_INSPECT,
        ],
        "the refused re-attach is confirmed by a fresh inspect: {methods:?}"
    );
}

#[test]
fn a_first_attach_to_a_missing_session_still_fails_with_session_not_found() {
    let (exited_zero, output, _fixture) = refused_reattach_scenario(true);

    assert!(
        !exited_zero,
        "a first attach to a missing session still fails: {output:?}"
    );
    assert!(
        !output.contains("attach ended"),
        "a first attach never reports an ended session: {output:?}"
    );
    assert!(
        output.contains("session_not_found"),
        "the refusal surfaces unchanged: {output:?}"
    );
}

// Rust guideline compliant 2026-06-26
