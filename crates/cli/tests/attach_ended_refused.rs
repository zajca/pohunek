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
    /// #2440 L5: the daemon goes away between the re-attach refusal and
    /// the confirm: the first post-EOF probe sees `running`, the re-attach
    /// is refused `session_not_running`, the confirm connection is dropped
    /// without a reply, and the next probe sees the confirming `stopped`.
    /// The attach must retry through the reconnect budget and end with
    /// exit 0 instead of erroring on the dropped confirm connection.
    ConfirmConnectDropped,
    /// #2441 case d: probe sees `running`, the re-attach is refused, and
    /// the confirming inspect sees `stopped`.
    #[default]
    RefusedReattach,
    /// A first attach to a missing session: refused with
    /// `session_not_found`, surfaced unchanged (non-zero exit).
    RefuseFirstAttach,
    /// #2440 R2: after the stream EOF `session.detach` is answered
    /// normally; the first post-EOF `session.inspect` is answered with
    /// `session_not_found`, every later one with `stopped`. The attach
    /// must end at once on the gone answer.
    RemovedBeforeProbe,
    /// #2440 T1: after the stream EOF the fixture drops the accepted
    /// probe connection without a reply while it has not seen the
    /// scripted restart count, then answers `starting`, `running`,
    /// serves a second attach, and ends that stream with a stop. The
    /// attach must reattach.
    RestartAndResume,
    /// #2440 T3: after the stream EOF the fixture drops the accepted
    /// probe connection without a reply while it has not seen the
    /// scripted restart count, then answers `session_not_found` (the
    /// removal committed during the restart). The attach must end at
    /// once.
    RemovedDuringRestart,
    /// #2448: after the stream EOF the first two probes are refused
    /// with `daemon_shutting_down` while the daemon goes away, then the
    /// probe sees `running`, serves a second attach, and ends that stream
    /// with a stop. The attach must retry across the shutting-down probes
    /// and reattach instead of failing.
    DaemonShuttingDown,
    /// #2448 follow-up: after the stream EOF the probes see `running`,
    /// but the fixture refuses the first re-attach with
    /// `daemon_shutting_down` (the drain started between the probe and
    /// the re-attach). The attach must route that retryable re-attach
    /// refusal back through the reconnect budget and window, then
    /// re-attach on the retried probe instead of erroring.
    ShuttingDownReattach,
}

/// What the fixture tracks around the attach stream EOF.
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
    /// The raw re-attach stream open after the restart serves it, or
    /// after the shutting-down probes end and the re-attach runs.
    resumed_stream: Arc<Mutex<Option<UnixStream>>>,
    /// The client sent `session.detach` after reading the stream EOF.
    detached: Arc<Mutex<bool>>,
    /// Post-EOF `session.inspect` confirm connections to drop without a
    /// reply before scripted answers resume. Only the L5 confirm-drop mode
    /// starts with one; every other mode keeps the count at zero.
    dropped_confirms: Arc<Mutex<usize>>,
    /// Post-EOF probe connections the fixture dropped without a reply
    /// while the replacement daemon was still away.
    refused_probes: Arc<Mutex<usize>>,
    /// Post-EOF probe connections still to drop before the fixture
    /// serves scripted answers again.
    restart_connects: Arc<Mutex<usize>>,
    /// Post-EOF `session.attach` requests, refused or served: the restart
    /// re-attach, the re-attach after a shutting-down run, or both
    /// attempts of the refused re-attach.
    restart_attaches: Arc<Mutex<usize>>,
    /// Fixture behavior after the stream EOF.
    mode: Arc<Mutex<Mode>>,
}

/// What a scripted post-EOF `session.inspect` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectAnswer {
    /// Answer with the given session state (`starting`, `running`,
    /// `stopped`).
    State(&'static str),
    /// Refuse with `session_not_found`.
    NotFound,
    /// Refuse with `daemon_shutting_down`: the daemon is going away,
    /// so the client must retry instead of failing.
    ShuttingDown,
}

impl Clone for Fixture {
    fn clone(&self) -> Self {
        Self {
            methods: Arc::clone(&self.methods),
            attaches: Arc::clone(&self.attaches),
            resumed_stream: Arc::clone(&self.resumed_stream),
            refused_probes: Arc::clone(&self.refused_probes),
            restart_attaches: Arc::clone(&self.restart_attaches),
            restart_connects: Arc::clone(&self.restart_connects),
            post_eof_inspects: Arc::clone(&self.post_eof_inspects),
            stream: Arc::clone(&self.stream),
            detached: Arc::clone(&self.detached),
            dropped_confirms: Arc::clone(&self.dropped_confirms),
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

    /// Post-EOF confirm connections still to drop without a reply.
    fn dropped_confirms_remaining(&self) -> usize {
        *self.dropped_confirms.lock().expect("dropped confirms lock")
    }

    /// Post-EOF probe connections the fixture dropped without a reply
    /// while the replacement daemon was still away.
    fn refused_probes(&self) -> usize {
        *self.refused_probes.lock().expect("refused probes lock")
    }

    /// `session.attach` calls served after the stream EOF detach.
    fn restart_attaches(&self) -> usize {
        *self.restart_attaches.lock().expect("restart attaches lock")
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
/// always sets a long reconnect window and a short interval, so a fixed run
/// can only finish through a decision, never through expiry. The restart
/// modes drop the accepted probe connection without a reply while the
/// replacement daemon is away, as a killed daemon does: the client reads
/// EOF at once (`framing`, a `Retry`).
#[allow(
    clippy::too_many_lines,
    reason = "the fixture answers every method inline, including the restart modes"
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
        dropped_confirms: Arc::new(Mutex::new(match mode {
            Mode::ConfirmConnectDropped => 1,
            _ => 0,
        })),
        restart_connects: Arc::new(Mutex::new(match mode {
            Mode::RestartAndResume | Mode::RemovedDuringRestart => 2,
            Mode::RefusedReattach
            | Mode::RefuseFirstAttach
            | Mode::RemovedBeforeProbe
            | Mode::DaemonShuttingDown
            | Mode::ShuttingDownReattach
            | Mode::ConfirmConnectDropped => 0,
        })),
        ..Fixture::default()
    };
    if matches!(
        mode,
        Mode::RestartAndResume
            | Mode::RemovedDuringRestart
            | Mode::RemovedBeforeProbe
            | Mode::DaemonShuttingDown
            | Mode::ShuttingDownReattach
            | Mode::ConfirmConnectDropped
    ) {
        // The modes script every post-EOF answer explicitly. The outage
        // itself needs no scripted answer: the fixture drops the first
        // `restart_connects` accepted probe connections while the
        // replacement daemon is away, then serves the queued answers.
        fixture
            .post_eof_inspects
            .lock()
            .expect("inspects lock")
            .extend(match mode {
                Mode::RestartAndResume => vec![
                    InspectAnswer::State("starting"),
                    InspectAnswer::State("running"),
                ],
                // The first gone answer ends the fixed client at once;
                // the trailing `stopped` ends a reverted client on the
                // next probe instead, which then fails the
                // removed-message and tail assertions. S3 shares the
                // script; only the outage model differs.
                Mode::RemovedBeforeProbe | Mode::RemovedDuringRestart => {
                    vec![InspectAnswer::NotFound, InspectAnswer::State("stopped")]
                }
                // #2448: the going-away daemon refuses the first two
                // probes with `daemon_shutting_down`, which the client
                // must retry; the `running` answer then re-attaches like
                // the restart resume above.
                Mode::DaemonShuttingDown => vec![
                    InspectAnswer::ShuttingDown,
                    InspectAnswer::ShuttingDown,
                    InspectAnswer::State("running"),
                ],
                // In mode ShuttingDownReattach the refused re-attach
                // consumes no scripted answer. Probe 1 answers `running`
                // and the client re-attaches; the fixture refuses that
                // attach with `daemon_shutting_down` and the client
                // retries. Probe 2 answers `running` and the attach is
                // served. The empty queue then answers `stopped`, which
                // ends the served stream as usual.
                Mode::ShuttingDownReattach => vec![
                    InspectAnswer::State("running"),
                    InspectAnswer::State("running"),
                ],
                // In mode ConfirmConnectDropped the first post-EOF probe
                // answers `running` so the client decides `Reattach`, then
                // the confirming probe after the dropped confirm answers
                // `stopped`.
                Mode::ConfirmConnectDropped => vec![
                    InspectAnswer::State("running"),
                    InspectAnswer::State("stopped"),
                ],
                // The first attach and the refused modes script no
                // post-EOF answer: the match above already selected them
                // out, so this arm is unreachable.
                Mode::RefusedReattach | Mode::RefuseFirstAttach => {
                    unreachable!("unscripted modes never queue post-EOF answers")
                }
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
    let mut cli = CliProcess(child);
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
        // The raw stream and the status updater's subscription arrive on
        // two fixture threads, so neither orders the other: wait for both
        // the subscription record and the stored first stream before the
        // kill takes the stream the client reads from.
        poll_until("the fixture to serve the attach and subscription", || {
            let subscribed = fixture
                .methods()
                .iter()
                .any(|method| method == protocol::method::SUBSCRIBE);
            let stored = fixture.stream.lock().expect("stream lock").is_some();
            (subscribed && stored).then_some(())
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

    if matches!(
        mode,
        Mode::RestartAndResume | Mode::DaemonShuttingDown | Mode::ShuttingDownReattach
    ) {
        // End the resumed stream when the client has opened it: the stop
        // made the client detach and probe again, and the fixture stores
        // the re-attach stream when the raw-stream connection arrives.
        // Wait for the first served re-attach and then the stored stream
        // in two steps so a connection that arrives before its reply
        // cannot end the wrong stream. Either wait also ends when the
        // client has exited: a refused re-attach must fail the assertions
        // below with the client output, never at the hang guard. The
        // count is only the gate: in ShuttingDownReattach the refused
        // attach sets it to 1 too, so wait with `>=` and let the stored
        // stream (only the served attach opens one) decide which re-attach
        // to stop.
        poll_until("the fixture to serve the re-attach", || {
            (fixture.restart_attaches() >= 1).then_some(()).or_else(|| {
                cli.0
                    .try_wait()
                    .expect("poll the attach client")
                    .map(|_| ())
            })
        });
        poll_until("the fixture to store the re-attach stream", || {
            fixture
                .resumed_stream
                .lock()
                .expect("resumed stream lock")
                .is_some()
                .then_some(())
                .or_else(|| {
                    cli.0
                        .try_wait()
                        .expect("poll the attach client")
                        .map(|_| ())
                })
        });
        if cli
            .0
            .try_wait()
            .expect("poll the attach client before the stop")
            .is_none()
        {
            fixture
                .resumed_stream
                .lock()
                .expect("resumed stream lock")
                .take()
                .expect("the re-attach stream is open")
                .shutdown(std::net::Shutdown::Both)
                .expect("stop the resumed stream");
        }
    }

    // The client must end on its own once the stop commits. With the
    // trailing `stopped` scripted, a reverted client ends on the
    // `stopped` probe and fails the removed-message and tail
    // assertions.
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

/// Starts the fixture listener thread and returns once it owns the socket
/// path: no client request can arrive before the fixture is ready to
/// answer it.
fn serve_fixture_daemon(socket: &Path, fixture: &Fixture) {
    let (ready, waiter) = std::sync::mpsc::channel::<std::io::Result<()>>();
    let fixture = fixture.clone();
    let socket = socket.to_path_buf();
    thread::spawn(move || match UnixListener::bind(&socket) {
        Ok(listener) => {
            let _ignored = ready.send(Ok(()));
            serve_on_listener(listener, &fixture);
        }
        Err(error) => {
            let _ignored = ready.send(Err(error));
        }
    });
    // The fixture process shares no other gate with the client, so a
    // failed listener bind must surface here, before the client races a
    // socket path with nobody listening on it.
    waiter
        .recv()
        .expect("the fixture listener thread starts")
        .expect("bind the fixture daemon");
}

/// Accept loop of the pre-bound fixture listener: every accepted stream is
/// already connected, so the client can never send a request that lands on
/// a socket path with nobody listening.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the listener is moved in from the single bind of serve_fixture_daemon"
)]
fn serve_on_listener(listener: UnixListener, fixture: &Fixture) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { return };
        let fixture = fixture.clone();
        thread::spawn(move || serve_connection(stream, &fixture));
    }
}

/// The fixture: answers control requests, and holds each attach stream
/// open until the scenario thread stops it. Restart modes model the
/// outage by dropping the accepted probe connections without a reply
/// until the queued post-EOF answers say the replacement daemon is back.
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the fixture answers every method inline; the spawned thread owns the fixture"
)]
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
            // The re-attach after the outage reuses the same stream id.
            // Once the scenario killed the first stream, the next
            // connection is the resumed stream the scenario stops.
            if fixture.detached()
                && matches!(
                    fixture.mode(),
                    Mode::RestartAndResume | Mode::DaemonShuttingDown | Mode::ShuttingDownReattach
                )
                && fixture.stream.lock().expect("stream lock").is_none()
            {
                *fixture.resumed_stream.lock().expect("resumed stream lock") = Some(stream);
            } else {
                *fixture.stream.lock().expect("stream lock") = Some(stream);
            }
            return;
        }
        let request: Request = serde_json::from_value(first).expect("parse a request");
        if request.method() == protocol::method::SESSION_INSPECT
            && fixture.detached()
            && matches!(
                fixture.mode(),
                Mode::RestartAndResume | Mode::RemovedDuringRestart
            )
        {
            let remaining = {
                let mut remaining = fixture
                    .restart_connects
                    .lock()
                    .expect("restart connects lock");
                let outstanding = *remaining;
                if outstanding > 0 {
                    *remaining -= 1;
                }
                outstanding
            };
            if remaining > 0 {
                // The replacement daemon is still away: drop this probe
                // connection without a reply, so the client fails with a
                // `Retry` error instead of reading a scripted answer. The
                // drop is counted, so the restart modes assert the
                // outage was actually observed.
                *fixture.refused_probes.lock().expect("refused probes lock") += 1;
                return;
            }
        }
        if request.method() == protocol::method::SESSION_INSPECT
            && fixture.detached()
            && fixture.mode() == Mode::ConfirmConnectDropped
            && fixture.dropped_confirms_remaining() > 0
        {
            // The daemon went away between the re-attach refusal and the
            // confirm: drop the confirm connection without a reply, so it
            // fails with a `Retry` mapped from a refused connection as a
            // killed daemon does.
            *fixture
                .dropped_confirms
                .lock()
                .expect("dropped confirms lock") -= 1;
            return;
        }
        if request.method() == protocol::method::SESSION_INSPECT && fixture.detached() {
            // Take the next scripted answer without holding the lock
            // across the reply: the guard must not survive into the
            // branch, where the same mutex is touched again.
            let scripted = match fixture.mode() {
                Mode::RestartAndResume
                | Mode::RemovedDuringRestart
                | Mode::DaemonShuttingDown
                | Mode::ShuttingDownReattach
                | Mode::ConfirmConnectDropped => fixture
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
                    InspectAnswer::ShuttingDown => {
                        fixture.record(request.method());
                        let response = Response::err(
                            PROTOCOL_VERSION,
                            request.id(),
                            ProtocolError::new(
                                ErrorClass::Runtime,
                                "daemon_shutting_down",
                                "the daemon is shutting down",
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
                }
            }
        }
        fixture.record(request.method());
        let response = result_for(&request, fixture.clone());
        if request.method() == protocol::method::SESSION_DETACH {
            // Mark the detach before writing its reply: every post-EOF
            // behavior keys off this flag, and the client cannot probe
            // before it has read the reply.
            *fixture.detached.lock().expect("detached lock") = true;
        }
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize the response")
        )
        .expect("write the fixture response");
        if request.method() == protocol::method::SUBSCRIBE {
            let _ = reader.read_line(&mut String::new());
            return;
        }
    }
}

/// Fixture replies for the control requests the attach makes. The restart
/// outage lives one layer up, in `serve_connection`: this function only
/// answers the connections the fixture accepts. Unscripted modes answer
/// every probe with `running`; outage modes script their answers in
/// `post_eof_inspects`, and any later post-EOF probe reports the terminal
/// state so the stream can end.
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the fixture is cloned per connection; the owned value keeps the borrow out of the reply"
)]
fn result_for(request: &Request, fixture: Fixture) -> Response {
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
            // In ShuttingDownReattach no outage drops the attaches, so
            // refuse the first post-EOF attach with `daemon_shutting_down`:
            // the drain started between the probe and the re-attach.
            // Serve every later attach with a fresh stream.
            if fixture.detached() && fixture.mode() == Mode::ShuttingDownReattach {
                let attempt = {
                    let mut served = fixture
                        .restart_attaches
                        .lock()
                        .expect("restart attaches lock");
                    *served += 1;
                    *served
                };
                if attempt == 1 {
                    return Response::err(
                        PROTOCOL_VERSION,
                        request.id(),
                        ProtocolError::new(
                            ErrorClass::Runtime,
                            "daemon_shutting_down",
                            "the daemon is shutting down",
                            None,
                        ),
                    )
                    .expect("valid fixture response");
                }
                return Response::ok(
                    PROTOCOL_VERSION,
                    request.id(),
                    json!({ "stream_id": STREAM_ID }),
                )
                .expect("valid fixture response");
            }
            // The second attach is the re-attach of modes where the stop
            // committed between the probe inspect and this re-attach, so
            // the session is no longer attachable. The RestartAndResume
            // and DaemonShuttingDown arms below serve the re-attach after
            // the outage and its stream instead. The confirm-drop mode
            // reaches this arm too: its re-attach is refused the same way,
            // and only its confirm connection is dropped.
            if fixture.detached()
                && matches!(
                    fixture.mode(),
                    Mode::RestartAndResume | Mode::DaemonShuttingDown
                )
                && *fixture
                    .restart_attaches
                    .lock()
                    .expect("restart attaches lock")
                    == 0
            {
                // Post-detach, the single remaining attach is the
                // re-attach after the outage: serve it with a fresh
                // stream. The main thread ends the resumed stream once
                // both the reply and the stored stream are visible. The
                // probe reports the terminal state afterwards, so any
                // later attach is refused like the ended stop in the mode
                // where the stop committed before the re-attach.
                // Count it so the main thread can wait for the served
                // re-attach and then its stream.
                *fixture
                    .restart_attaches
                    .lock()
                    .expect("restart attaches lock") += 1;
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
                // Answer the first post-EOF probe with
                // `session_not_found`, every later one with `stopped`:
                // the fixed client ends on the gone answer, while a
                // reverted client retries past it, ends on the
                // `stopped` probe, and fails the removed-message and
                // tail assertions.
                let mut scripted = fixture.post_eof_inspects.lock().expect("inspects lock");
                if scripted.first() == Some(&InspectAnswer::NotFound) {
                    scripted.remove(0);
                } else {
                    drop(scripted);
                    return Response::ok(PROTOCOL_VERSION, request.id(), session("stopped"))
                        .expect("valid fixture response");
                }
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
            // After the stream EOF the client detaches and probes. Mode
            // `RefusedReattach` probes `running` once so the client decides
            // `Reattach`, then `stopped` so the R3 confirm path reports the
            // end. Outage modes script that answer from `post_eof_inspects`
            // instead; once the queue is empty the probe reports the
            // terminal state so the second stream can end.
            let state = if probe_after_eof && fixture.mode() == Mode::RefusedReattach {
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
            } else if probe_after_eof
                && matches!(
                    fixture.mode(),
                    Mode::RestartAndResume
                        | Mode::RemovedDuringRestart
                        | Mode::DaemonShuttingDown
                        | Mode::ShuttingDownReattach
                        | Mode::ConfirmConnectDropped
                )
                && fixture
                    .post_eof_inspects
                    .lock()
                    .expect("inspects lock")
                    .is_empty()
            {
                "stopped"
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
        "no re-attach failure precedes the gone answer: {output:?}"
    );
    assert_eq!(
        *fixture.attaches.lock().expect("attaches lock"),
        1,
        "no re-attach is attempted after the removal: {:?}",
        fixture.methods()
    );
    // After the stream EOF the client detaches and probes exactly once: no
    // other request may run in between. The trailing `stopped` stays
    // queued: the fixed client ends on the gone answer, while a reverted
    // client retries past it, ends on the next `stopped` probe, and fails
    // the removed-message and tail assertions.
    assert_eq!(
        post_eof_tail(&fixture),
        [protocol::method::SESSION_INSPECT],
        "the probe ends the attach on the first gone answer: {:?}",
        fixture.methods()
    );
    assert!(
        !fixture
            .post_eof_inspects
            .lock()
            .expect("inspects lock")
            .is_empty(),
        "the trailing stopped answer stays queued: {:?}",
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
        fixture.restart_attaches(),
        1,
        "the attach re-attaches after the restart: {:?}",
        fixture.methods()
    );
    let post_eof = post_eof_tail(&fixture);
    // The tail covers both streams. After the first detach the dropped
    // probe connections leave no method trace, then the probe sees
    // starting and running; the client re-attaches, and a later probe
    // observes the terminal state and ends the second stream without
    // an ended line. The best-effort status updater may add its own
    // inspect/subscribe pair on either stream, so only the ordering of
    // the load-bearing requests is asserted.
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
        inspects.len() >= 3,
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
        fixture.refused_probes() >= 1,
        "the restart outage is observed as dropped probe connections: {:?}",
        fixture.methods()
    );
    // The dropped probes leave no method trace; the scripted `starting`
    // and `running` answers are both consumed by the reconnect probe.
    assert!(
        fixture
            .post_eof_inspects
            .lock()
            .expect("inspects lock")
            .is_empty(),
        "every scripted post-EOF answer is consumed: {:?}",
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
    assert!(
        fixture.refused_probes() >= 1,
        "the restart outage is observed as dropped probe connections: {:?}",
        fixture.methods()
    );
    // The dropped probes leave no method trace; the served one is the
    // scripted `session_not_found` that ends the attach. The trailing
    // `stopped` stays queued: the fixed client ends on the gone answer,
    // while a reverted client retries past it, ends on the next
    // `stopped` probe, and fails the tail and the removed-message
    // assertions.
    assert_eq!(
        post_eof_tail(&fixture),
        [protocol::method::SESSION_INSPECT],
        "the probe ends the attach on the gone answer after the outage: {:?}",
        fixture.methods()
    );
    assert_eq!(
        fixture
            .post_eof_inspects
            .lock()
            .expect("inspects lock")
            .len(),
        1,
        "only the gone answer is consumed: {:?}",
        fixture.methods()
    );
}

#[test]
fn an_attach_retries_across_daemon_shutting_down_and_reattaches() {
    let (exited_zero, output, fixture) = attach_scenario(Mode::DaemonShuttingDown);
    assert!(
        exited_zero,
        "the attach survives the shutdown probes with exit 0: {output:?}"
    );
    assert!(
        !output.contains("attach ended"),
        "the shutting-down probes never end the attach: {output:?}"
    );
    assert!(
        !output.contains("window expired"),
        "the shutdown probes fit inside the window: {output:?}"
    );
    assert_eq!(
        fixture.restart_attaches(),
        1,
        "the attach re-attaches after the shutting-down probes: {:?}",
        fixture.methods()
    );
    // The probe refused twice with `daemon_shutting_down` before the
    // `running` answer: without the #2448 `Retry` arm the client would
    // fail on the first shutting-down probe instead of re-attaching.
    // The best-effort status updater may add its own inspect/subscribe
    // pair on either stream, so only the ordering of the load-bearing
    // requests is asserted.
    let post_eof = post_eof_tail(&fixture);
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
        inspects.len() >= 3,
        "shutting-down, running, and the stop-observing probes run: {:?}",
        fixture.methods()
    );
    assert_eq!(
        attaches.len(),
        1,
        "exactly one re-attach runs after the shutting-down probes: {:?}",
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
    // Every scripted post-EOF answer is consumed: the two
    // shutting-down refusals and the `running` state that re-attaches.
    assert!(
        fixture
            .post_eof_inspects
            .lock()
            .expect("inspects lock")
            .is_empty(),
        "every scripted post-EOF answer is consumed: {:?}",
        fixture.methods()
    );
    assert!(
        !output.contains("given up") && !output.contains("reattach attempts"),
        "no re-attach failure precedes the resumed stream: {output:?}"
    );
}

#[test]
fn a_re_attach_refused_by_a_draining_daemon_is_retried() {
    let (exited_zero, output, fixture) = attach_scenario(Mode::ShuttingDownReattach);
    assert!(
        exited_zero,
        "the attach survives the refused re-attach with exit 0: {output:?}"
    );
    assert!(
        !output.contains("daemon_shutting_down"),
        "the refused re-attach never surfaces as an error: {output:?}"
    );
    assert!(
        !output.contains("attach ended"),
        "the resumed session is not reported as ended: {output:?}"
    );
    assert_eq!(
        fixture.restart_attaches(),
        2,
        "the refused re-attach is retried until it is served: {:?}",
        fixture.methods()
    );
    let post_eof = post_eof_tail(&fixture);
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
        inspects.len() >= 2,
        "the probe and its retry run before the stop-observing probe: {:?}",
        fixture.methods()
    );
    assert_eq!(
        attaches.len(),
        2,
        "the refused re-attach runs again after the retried probe: {:?}",
        fixture.methods()
    );
    assert!(
        inspects[0] < attaches[0] && attaches[1] < detaches[0],
        "the first probe precedes the refused re-attach, the retry serves it before the stop: {:?}",
        fixture.methods()
    );
}

#[test]
fn a_dropped_confirm_connection_is_retried_until_the_session_end_is_confirmed() {
    let (exited_zero, output, fixture) = attach_scenario(Mode::ConfirmConnectDropped);
    assert!(
        exited_zero,
        "the attach ends with exit 0 after the dropped confirm: {output:?}"
    );
    assert!(
        output.contains(ENDED_MESSAGE),
        "the attach reports the ended session: {output:?}"
    );
    assert!(
        !output.contains("daemon error"),
        "no daemon error is reported for an ended session: {output:?}"
    );
    // After the stream EOF the client detaches, probes (running),
    // re-attaches (refused), drops the first confirm connection (no
    // method trace), and confirms the end on the retried probe (stopped):
    // no other request may run in between. Probe-inspect counts alone
    // cannot prove the dropped confirm, because the status snapshot and
    // the status updater inspect earlier.
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
    assert_eq!(
        fixture.dropped_confirms_remaining(),
        0,
        "the confirm connection was dropped exactly once: {:?}",
        fixture.methods()
    );
}

// Rust guideline compliant 2026-06-26
