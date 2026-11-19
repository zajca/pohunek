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
//! A `Mode` chooses the fixture's behavior after the stream EOF. The
//! refused-reattach and first-attach-refusal modes above cover #2441's R3
//! and F1 cases; the reconnect modes cover zajca/pohunek#2440 (a session
//! removed before or during a reconnect must end the attach at once, while a
//! daemon restart is still retried until the window ends).

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
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

/// How long the reconnect path may keep retrying: long enough that a fixed
/// run can only finish through a decision, never through expiry.
const RECONNECT_WINDOW_SECONDS: &str = "600";

/// Delay between reconnect probe attempts.
const RECONNECT_INTERVAL_SECONDS: &str = "0.1";

/// Consecutive failing reconnect attempts allowed inside the window.
const RECONNECT_MAX_ATTEMPTS: &str = "3";

/// Message the client prints once the session end is confirmed.
const ENDED_MESSAGE: &str = "[pohunek] session s-attach-ended-refused stopped; attach ended";

/// Message the reconnect probe prints once it sees the session gone.
const REMOVED_MESSAGE: &str = "[pohunek] session s-attach-ended-refused removed; attach ended";

/// Fixture behavior after the attach stream EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    /// #2441 case d: probe sees `running`, the re-attach is refused, and
    /// the confirming inspect sees `stopped`.
    #[default]
    RefusedReattach,
    /// A first attach to a missing session: refused with
    /// `session_not_found`, surfaced unchanged (non-zero exit).
    RefuseFirstAttach,
    /// #2440 R2: after the stream EOF the fixture answers `session.detach`
    /// normally and every post-EOF `session.inspect` with
    /// `session_not_found`. The attach must end at once.
    RemovedBeforeProbe,
    /// #2440 T1: after the stream EOF the fixture drops the detach and the
    /// first probe connection (the daemon going away), answers the next
    /// probe with `starting`, then `running`, serves a second attach, and
    /// ends that stream with a stop. The attach must reattach.
    RestartAndResume,
    /// #2440 T3: after the stream EOF the fixture drops the detach and the
    /// first probe connection (the daemon going away), then answers
    /// `session_not_found` (the removal committed during the restart). The
    /// attach must end at once.
    RemovedDuringRestart,
}

/// Phase of the refused-reattach sequence the fixture has reached.
#[derive(Debug, Default)]
struct Fixture {
    /// Control requests served, by method, in arrival order.
    methods: Arc<Mutex<Vec<String>>>,
    /// `session.attach` calls served so far.
    attaches: Arc<Mutex<usize>>,
    /// Post-EOF `session.inspect` answers when the mode scripts them.
    post_eof_inspects: Arc<Mutex<Vec<InspectAnswer>>>,
    /// The raw attach stream open before the stop kills it.
    stream: Arc<Mutex<Option<UnixStream>>>,
    /// The client sent `session.detach` after reading the stream EOF.
    detached: Arc<Mutex<bool>>,
    /// Fixture behavior after the stream EOF.
    mode: Arc<Mutex<Mode>>,
    /// The live fixture listener. Restart modes take it (dropping closes
    /// it) right after the first attach stream opens, so the next probe
    /// connection fails at connect like a daemon going away.
    listener: Arc<Mutex<Option<UnixListener>>>,
}

/// What a scripted post-EOF `session.inspect` answers.
#[derive(Debug, Clone, Copy)]
enum InspectAnswer {
    /// Answer with the given session state (`starting`, `running`,
    /// `stopped`).
    State(&'static str),
    /// Refuse with `session_not_found`.
    NotFound,
}

impl Clone for Fixture {
    fn clone(&self) -> Self {
        Self {
            methods: Arc::clone(&self.methods),
            attaches: Arc::clone(&self.attaches),
            listener: Arc::clone(&self.listener),
            post_eof_inspects: Arc::clone(&self.post_eof_inspects),
            stream: Arc::clone(&self.stream),
            detached: Arc::clone(&self.detached),
            mode: Arc::clone(&self.mode),
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

    /// Current fixture mode.
    fn mode(&self) -> Mode {
        *self.mode.lock().expect("mode lock")
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
/// The fixture behaves as `mode` selects after the stream EOF; `attach.conf`
/// always sets a long reconnect window, a short interval, and a small
/// attempt cap, so a fixed run can only finish through a decision, never
/// through expiry.
#[allow(
    clippy::too_many_lines,
    reason = "the fixture answers every method inline"
)]
fn attach_scenario(mode: Mode) -> (bool, String, Fixture) {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(
            root.join(directory),
            fs::Permissions::from_mode(PRIVATE_DIR_MODE),
        )
        .expect("make the directory owner-private");
    }
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(CONFIG_DIR_MODE))
        .expect("make the config directory owner-private");
    fs::write(
        config_dir.join("attach.conf"),
        format!(
            "attach_reconnect_seconds = {RECONNECT_WINDOW_SECONDS}\n\
             attach_reconnect_interval_seconds = {RECONNECT_INTERVAL_SECONDS}\n\
             attach_reconnect_max_attempts = {RECONNECT_MAX_ATTEMPTS}\n"
        ),
    )
    .expect("write the attach reconnect settings");
    let socket = root.join("run/pohunek/daemon.sock");

    let fixture = Fixture {
        mode: Arc::new(Mutex::new(mode)),
        ..Fixture::default()
    };
    if matches!(mode, Mode::RestartAndResume | Mode::RemovedDuringRestart) {
        // The modes script every post-EOF answer explicitly. The outage
        // itself needs no scripted answer: while no listener runs the
        // probe fails at connect.
        fixture
            .post_eof_inspects
            .lock()
            .expect("inspects lock")
            .extend(match mode {
                Mode::RestartAndResume => vec![
                    InspectAnswer::State("starting"),
                    InspectAnswer::State("running"),
                    InspectAnswer::State("stopped"),
                ],
                _ => vec![InspectAnswer::NotFound],
            });
    }
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

    if !matches!(mode, Mode::RefuseFirstAttach) {
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
    // without the fix still ends (with exit 0 after the refusing re-attach
    // or the capped retries), so `poll_until` on the exit status cannot hang
    // the suite either way.
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

/// Serves the fixture daemon on `socket`. Restart modes (`RestartAndResume`
/// and `RemovedDuringRestart`) close the listener right after the first
/// attach stream opens: the next probe connection then fails at connect
/// with `DaemonUnreachable` (a `Retry`), exactly like a daemon going away.
/// The fixture answers on a fresh listener once the scripted answers say
/// the replacement daemon is back.
fn serve_fixture_daemon(socket: &Path, fixture: &Fixture) {
    let fixture = fixture.clone();
    let socket = socket.to_path_buf();
    thread::spawn(move || {
        serve_on_listener(&socket, &fixture);
    });
}

/// Accept loop of one fixture listener generation.
fn serve_on_listener(socket: &Path, fixture: &Fixture) {
    let listener = UnixListener::bind(socket).expect("bind the fixture daemon");
    let held = {
        let mut slot = fixture.listener.lock().expect("listener lock");
        let previous = slot.replace(listener);
        drop(previous);
        slot.as_ref()
            .map(|listener| listener.try_clone().expect("clone the fixture listener"))
    };
    let Some(listener) = held else {
        return;
    };
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let fixture = fixture.clone();
        let socket = socket.to_path_buf();
        thread::spawn(move || serve_connection(stream, socket, &fixture));
    }
}

/// The fixture: answers control requests, holds the first attach stream open
/// until the scenario kills it, refuses the re-attach.
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the fixture answers every method inline; the spawned thread owns the socket path"
)]
fn serve_connection(stream: UnixStream, socket: PathBuf, fixture: &Fixture) {
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
        if request.method() == protocol::method::SESSION_DETACH
            && matches!(
                fixture.mode(),
                Mode::RestartAndResume | Mode::RemovedDuringRestart
            )
            && !fixture.detached()
        {
            // The killed stream made the client read EOF: the old daemon
            // is gone. Close the listener so the next probe connections
            // fail at connect (`DaemonUnreachable`, a `Retry`); the
            // replacement daemon starts listening a moment later. The
            // detach itself is still answered: the old daemon processed
            // it before going away. The sleep only models the restart
            // gap: no assertion reads a clock.
            *fixture.listener.lock().expect("listener lock") = None;
            let _ = fs::remove_file(&socket);
            let fixture = fixture.clone();
            let socket = socket.clone();
            thread::spawn(move || {
                thread::sleep(std::time::Duration::from_millis(500));
                serve_on_listener(&socket, &fixture);
            });
        }
        if request.method() == protocol::method::SESSION_INSPECT && fixture.detached() {
            // Take the next scripted answer without holding the lock
            // across the reply: the guard must not survive into the
            // branch, where the same mutex is touched again.
            let scripted = match fixture.mode() {
                Mode::RestartAndResume | Mode::RemovedDuringRestart => fixture
                    .post_eof_inspects
                    .lock()
                    .expect("inspects lock")
                    .first()
                    .copied(),
                _ => None,
            };
            if let Some(answer) = scripted {
                fixture
                    .post_eof_inspects
                    .lock()
                    .expect("inspects lock")
                    .remove(0);
                match answer {
                    InspectAnswer::NotFound => {
                        fixture.record(request.method());
                        let response = Response::err(
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
                        writeln!(
                            reader.get_mut(),
                            "{}",
                            serde_json::to_string(&response).expect("serialize the response")
                        )
                        .expect("write the fixture response");
                        continue;
                    }
                    InspectAnswer::State(state) => {
                        fixture.record(request.method());
                        let response = Response::ok(PROTOCOL_VERSION, request.id(), session(state))
                            .expect("valid fixture response");
                        writeln!(
                            reader.get_mut(),
                            "{}",
                            serde_json::to_string(&response).expect("serialize the response")
                        )
                        .expect("write the fixture response");
                        continue;
                    }
                }
            }
        }
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
#[allow(
    clippy::too_many_lines,
    reason = "the fixture answers every method inline"
)]
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
            if fixture.mode() == Mode::RefuseFirstAttach {
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
            if fixture.mode() == Mode::RestartAndResume {
                // The restarted daemon serves the re-attach and its stream.
                // The stop ends that stream a moment later, like the kill
                // ended the first one: the client reads EOF, detaches, and
                // probes again. The sleep only models the stop; no
                // assertion reads a clock.
                let mut attaches = fixture.attaches.lock().expect("attaches lock");
                *attaches += 1;
                let fixture = fixture.clone();
                thread::spawn(move || {
                    thread::sleep(std::time::Duration::from_millis(500));
                    fixture
                        .stream
                        .lock()
                        .expect("stream lock")
                        .take()
                        .expect("the second attach stream is open")
                        .shutdown(std::net::Shutdown::Both)
                        .expect("stop the second attach stream");
                });
                return Response::ok(
                    PROTOCOL_VERSION,
                    request.id(),
                    json!({ "stream_id": STREAM_ID }),
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
            if probe_after_eof && fixture.mode() == Mode::RemovedBeforeProbe {
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
            // The first post-EOF inspect is the reconnect probe: the stop
            // has not committed yet, so the client decides `Reattach`.
            let state = if probe_after_eof {
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

/// Post-EOF request tail after the stream EOF detach.
fn post_eof_tail(fixture: &Fixture) -> Vec<String> {
    fixture
        .methods()
        .iter()
        .skip_while(|method| *method != protocol::method::SESSION_DETACH)
        .skip(1)
        .map(Clone::clone)
        .collect()
}

#[test]
fn an_attach_whose_session_ends_between_probe_and_reattach_exits_zero() {
    let (exited_zero, output, fixture) = attach_scenario(Mode::RefusedReattach);

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
    // After the stream EOF the client detaches, probes (running), re-attaches
    // (refused), and confirms the end (stopped): no other request may run in
    // between. Probe-inspect counts alone cannot prove the confirming inspect,
    // because the status snapshot and the status updater inspect earlier.
    assert_eq!(
        post_eof_tail(&fixture),
        [
            protocol::method::SESSION_INSPECT,
            protocol::method::SESSION_ATTACH,
            protocol::method::SESSION_INSPECT,
        ],
        "the refused re-attach is confirmed by a fresh inspect: {:?}",
        fixture.methods()
    );
}

#[test]
fn a_first_attach_to_a_missing_session_still_fails_with_session_not_found() {
    let (exited_zero, output, _fixture) = attach_scenario(Mode::RefuseFirstAttach);

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

#[test]
fn a_session_removed_before_the_reconnect_probe_ends_the_attach() {
    let (exited_zero, output, fixture) = attach_scenario(Mode::RemovedBeforeProbe);

    assert!(
        exited_zero,
        "a removed session ends the attach with exit 0: {output:?}"
    );
    assert!(
        output.contains(REMOVED_MESSAGE),
        "the attach reports the removed session: {output:?}"
    );
    assert!(
        !output.contains("window expired"),
        "the attach ends at once instead of waiting for the window: {output:?}"
    );
    assert!(
        !output.contains("reattach attempts"),
        "the attempt cap is never reached: {output:?}"
    );
    assert_eq!(
        *fixture.attaches.lock().expect("attaches lock"),
        1,
        "no re-attach is attempted after the removal: {:?}",
        fixture.methods()
    );
    // After the stream EOF the client detaches and probes exactly once: no
    // other request may run in between.
    assert_eq!(
        post_eof_tail(&fixture),
        [protocol::method::SESSION_INSPECT],
        "the probe ends the attach on the first gone answer: {:?}",
        fixture.methods()
    );
}

#[test]
fn an_attach_waits_across_a_daemon_restart_and_reattaches() {
    let (exited_zero, output, fixture) = attach_scenario(Mode::RestartAndResume);

    assert!(
        exited_zero,
        "the attach survives the restart with exit 0: {output:?}"
    );
    assert!(
        !output.contains("removed; attach ended"),
        "a resumed session is not reported as removed: {output:?}"
    );
    assert!(
        !output.contains("window expired"),
        "the restart fits inside the window: {output:?}"
    );
    assert_eq!(
        *fixture.attaches.lock().expect("attaches lock"),
        2,
        "the attach re-attaches after the restart: {:?}",
        fixture.methods()
    );
    let post_eof = post_eof_tail(&fixture);
    // The tail covers both streams. After the first detach the probe
    // fails at connect while no listener runs, then sees starting and
    // running; the client re-attaches, and the stop that ends the second
    // stream is observed by another detach and probe. Connect failures
    // leave no method trace, and the best-effort status updater may add
    // its own inspect/subscribe pair on either stream, so only the
    // ordering of the load-bearing requests is asserted.
    let positions = |method: &str| -> Vec<usize> {
        post_eof
            .iter()
            .enumerate()
            .filter(|(_, seen)| *seen == method)
            .map(|(index, _)| index)
            .collect()
    };
    let inspects = positions(protocol::method::SESSION_INSPECT);
    let attaches = positions(protocol::method::SESSION_ATTACH);
    let detaches = positions(protocol::method::SESSION_DETACH);
    assert!(
        inspects.len() >= 4,
        "starting, running, and the stop-observing probes run: {:?}",
        fixture.methods()
    );
    assert_eq!(
        attaches.len(),
        1,
        "exactly one re-attach runs after the restart: {:?}",
        fixture.methods()
    );
    assert_eq!(
        detaches.len(),
        1,
        "the stop ends the second stream with one more detach: {:?}",
        fixture.methods()
    );
    assert!(
        inspects[0] < attaches[0] && attaches[0] < detaches[0],
        "the probe sees the resumed session before the re-attach, and the stop ends it after: {:?}",
        fixture.methods()
    );
    assert!(
        fixture
            .post_eof_inspects
            .lock()
            .expect("inspects lock")
            .is_empty(),
        "every scripted post-EOF answer is consumed exactly once: {:?}",
        fixture.methods()
    );
    assert!(
        !output.contains("attach ended"),
        "the stopped probe ends the second stream silently, like any terminal probe state: {output:?}"
    );
}

#[test]
fn a_session_removed_during_a_daemon_restart_ends_the_attach() {
    let (exited_zero, output, fixture) = attach_scenario(Mode::RemovedDuringRestart);

    assert!(
        exited_zero,
        "a removal during the restart ends the attach with exit 0: {output:?}"
    );
    assert!(
        output.contains(REMOVED_MESSAGE),
        "the attach reports the removed session: {output:?}"
    );
    assert!(
        !output.contains("window expired"),
        "the attach ends at once instead of waiting for the window: {output:?}"
    );
    assert_eq!(
        *fixture.attaches.lock().expect("attaches lock"),
        1,
        "no re-attach is attempted after the removal: {:?}",
        fixture.methods()
    );
}

// Rust guideline compliant 2026-06-26
