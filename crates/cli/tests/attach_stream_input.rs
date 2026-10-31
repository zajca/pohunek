//! Binary-level scenario for the attach client's stream input.
//!
//! The built CLI attaches to a fixture daemon whose raw attach stream does not
//! read the client's input while it writes output, as the daemon does while a
//! paste waits for the program to read it. The client must keep showing that
//! output instead of waiting for its own input write to complete. The reverse
//! holds as well: a terminal that takes no output must neither keep the client
//! from forwarding keystrokes nor hold back a detach.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::thread;

use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::{poll_until, HANG_GUARD};
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// Session the CLI attaches to.
const SESSION_ID: &str = "s-attach-input-1";

/// Stream id the fixture daemon hands out for the attach.
const STREAM_ID: &str = "attach-input-stream";

/// Bytes piped into the CLI as one paste: far more than the socket buffers
/// between the client and the daemon hold.
const PASTE_BYTES: usize = 1024 * 1024;

/// Bytes per pasted line, newline included.
const PASTE_LINE_BYTES: usize = 64;

/// Bytes the fixture writes to the stream before it reads any input: more than
/// the socket buffers between the daemon and the client hold.
const STREAM_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

/// Line that ends the fixture's output.
const OUTPUT_MARKER: &[u8] = b"stream-output-done";

/// Line typed while the terminal reads none of the output.
const KEYSTROKE: &[u8] = b"typed-while-the-terminal-is-stalled\n";

/// Detach shortcut byte of the attach client.
const DETACH_BYTE: u8 = 0x1d;

/// Bytes per write while the fixture floods the client.
const FLOOD_CHUNK_BYTES: usize = 4096;

/// Capacity the CLI's stdout pipe is shrunk to, so that it fills at once.
#[cfg(target_os = "linux")]
const STDOUT_PIPE_BYTES: usize = 4096;

/// Mode of the runtime directory the client expects to be owner-private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// What the fixture's raw attach stream observed.
#[derive(Debug, PartialEq, Eq)]
enum StreamEvent {
    /// The whole output was written before any input was read.
    OutputWritten,
    /// The client's input, read to its end after the output.
    InputReceived(usize),
    /// The client forwarded the line typed while its terminal took no output.
    KeystrokeReceived,
}

/// How the fixture's raw attach stream behaves.
#[derive(Debug, Clone, Copy)]
enum StreamBehavior {
    /// Writes the whole output before it reads any input.
    OutputThenInput,
    /// Floods output until the client goes away while it reads input.
    FloodWhileReading,
}

/// Kills the CLI if the scenario fails before it exits.
struct CliProcess(Child);

impl Drop for CliProcess {
    fn drop(&mut self) {
        let _killed = self.0.kill();
        let _reaped = self.0.wait();
    }
}

#[test]
fn an_attach_client_keeps_showing_output_while_its_paste_is_not_read() {
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
    let socket = root.join("run/pohunek/daemon.sock");
    let (events, observed) = mpsc::channel();
    serve_fixture_daemon(&socket, events, StreamBehavior::OutputThenInput);

    let mut command = env.command(pohunek_test_support::bin_exe("pohunek"));
    command
        .args(["attach", SESSION_ID])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut cli = CliProcess(command.spawn().expect("spawn pohunek attach"));
    let mut stdin = cli.0.stdin.take().expect("piped stdin");
    let mut stdout = cli.0.stdout.take().expect("piped stdout");
    let mut stderr = cli.0.stderr.take().expect("piped stderr");

    // The paste is written from its own thread; it completes only once the
    // client forwarded it and the fixture read it.
    let mut line = vec![b'x'; PASTE_LINE_BYTES - 1];
    line.push(b'\n');
    let paste = line.repeat(PASTE_BYTES / PASTE_LINE_BYTES);
    let pasting = thread::spawn(move || {
        stdin.write_all(&paste).expect("write the paste");
    });
    let (shown, shown_rx) = mpsc::channel();
    let reading = thread::spawn(move || {
        let mut tail = Vec::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut announced = false;
        loop {
            let count = stdout.read(&mut buffer).expect("read the CLI output");
            if count == 0 {
                return;
            }
            if announced {
                continue;
            }
            tail.extend_from_slice(&buffer[..count]);
            if tail
                .windows(OUTPUT_MARKER.len())
                .any(|window| window == OUTPUT_MARKER)
            {
                announced = true;
                let _sent = shown.send(());
                continue;
            }
            let keep = tail.len().min(OUTPUT_MARKER.len() - 1);
            tail.drain(..tail.len() - keep);
        }
    });

    assert_eq!(
        observed.recv_timeout(HANG_GUARD),
        Ok(StreamEvent::OutputWritten),
        "the client reads the stream's output while its paste is not read"
    );
    shown_rx
        .recv_timeout(HANG_GUARD)
        .expect("the client shows the stream's output");
    assert_eq!(
        observed.recv_timeout(HANG_GUARD),
        Ok(StreamEvent::InputReceived(PASTE_BYTES)),
        "the whole paste reaches the stream once it is read"
    );
    pasting.join().expect("paste thread");
    let status = cli.0.wait().expect("wait for pohunek attach");
    reading.join().expect("output thread");
    let mut errors = String::new();
    stderr
        .read_to_string(&mut errors)
        .expect("read the CLI errors");
    assert!(status.success(), "pohunek attach failed: {errors}");
}

/// Shrinks the CLI's stdout pipe where the platform allows it and returns the
/// byte count that means the pipe is full.
#[cfg(target_os = "linux")]
fn stdout_pipe_capacity(stdout: &std::os::fd::OwnedFd) -> usize {
    rustix::pipe::fcntl_setpipe_size(stdout, STDOUT_PIPE_BYTES).expect("shrink the stdout pipe");
    rustix::pipe::fcntl_getpipe_size(stdout).expect("read the pipe capacity")
}

/// A pipe of another platform cannot be resized; the CLI's output fills at
/// least this many bytes of it before the pipe is full.
#[cfg(not(target_os = "linux"))]
fn stdout_pipe_capacity(_stdout: &std::os::fd::OwnedFd) -> usize {
    16 * 1024
}

/// Hermetic environment, fixture daemon and a CLI whose stdout pipe is shrunk
/// and never read, so it is full as soon as the stream floods the client.
struct StalledTerminal {
    cli: CliProcess,
    stdin: std::process::ChildStdin,
    stdout: std::os::fd::OwnedFd,
    observed: mpsc::Receiver<StreamEvent>,
    _env: TestEnv,
}

fn attach_with_stalled_terminal() -> StalledTerminal {
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
    let socket = root.join("run/pohunek/daemon.sock");
    let (events, observed) = mpsc::channel();
    serve_fixture_daemon(&socket, events, StreamBehavior::FloodWhileReading);

    // The pipe is shrunk before the CLI exists, so it cannot hold more than its
    // new capacity yet.
    let (stdout, terminal_write) = rustix::pipe::pipe().expect("create the terminal pipe");
    let capacity = stdout_pipe_capacity(&stdout);
    let mut command = env.command(pohunek_test_support::bin_exe("pohunek"));
    command
        .args(["attach", SESSION_ID])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(terminal_write))
        .stderr(Stdio::null());
    let mut cli = CliProcess(command.spawn().expect("spawn pohunek attach"));
    let stdin = cli.0.stdin.take().expect("piped stdin");
    drop(command);
    // The pipe is never read: once it holds its capacity, the terminal takes
    // no more output.
    poll_until("the terminal pipe to fill", || {
        let queued = rustix::io::ioctl_fionread(&stdout).expect("count the pipe bytes");
        (queued >= capacity as u64).then_some(())
    });
    StalledTerminal {
        cli,
        stdin,
        stdout,
        observed,
        _env: env,
    }
}

#[test]
fn an_attach_client_forwards_keystrokes_while_the_terminal_reads_no_output() {
    let mut attach = attach_with_stalled_terminal();

    attach
        .stdin
        .write_all(KEYSTROKE)
        .expect("type the keystroke");
    assert_eq!(
        attach.observed.recv_timeout(HANG_GUARD),
        Ok(StreamEvent::KeystrokeReceived),
        "the keystroke reaches the stream while the terminal takes no output"
    );
    drop(attach.stdout);
}

#[test]
fn an_attach_client_detaches_while_the_terminal_reads_no_output() {
    let mut attach = attach_with_stalled_terminal();

    attach
        .stdin
        .write_all(&[DETACH_BYTE])
        .expect("type the detach shortcut");
    let status = poll_until("the attach client to exit", || {
        attach.cli.0.try_wait().expect("poll the attach client")
    });
    assert!(status.success(), "the detach ends the attach: {status}");
    drop(attach.stdout);
}

/// Serves `session.attach` on `socket` and one raw attach stream that behaves
/// as `behavior` says, reporting what it observes to `events`.
fn serve_fixture_daemon(
    socket: &Path,
    events: mpsc::Sender<StreamEvent>,
    behavior: StreamBehavior,
) {
    let listener = UnixListener::bind(socket).expect("bind the fixture daemon");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let events = events.clone();
            thread::spawn(move || serve_connection(stream, &events, behavior));
        }
    });
}

fn serve_connection(
    stream: UnixStream,
    events: &mpsc::Sender<StreamEvent>,
    behavior: StreamBehavior,
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
            match behavior {
                StreamBehavior::OutputThenInput => serve_attach_stream(reader, events),
                StreamBehavior::FloodWhileReading => serve_flooding_attach_stream(reader, events),
            }
            return;
        }
        let request: Request = serde_json::from_value(first).expect("parse a request");
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

fn result_for(request: &Request) -> Value {
    match request.method() {
        protocol::method::DAEMON_HEALTH => json!({
            "status": "ok",
            "daemon_version": "fixture",
            "protocol_version": PROTOCOL_VERSION.get()
        }),
        protocol::method::SESSION_ATTACH => json!({ "stream_id": STREAM_ID }),
        protocol::method::SESSION_DETACH => json!({ "detached": true, "error": null }),
        other => panic!("the attach client sent an unexpected request: {other}"),
    }
}

/// Writes the whole output, then reads the client's input to its end.
fn serve_attach_stream(reader: BufReader<UnixStream>, events: &mpsc::Sender<StreamEvent>) {
    // Input the line reader already took past the attach prelude.
    let mut input = reader.buffer().to_vec();
    let mut stream = reader.into_inner();
    let mut output = vec![b'y'; STREAM_OUTPUT_BYTES];
    output.push(b'\n');
    output.extend_from_slice(OUTPUT_MARKER);
    output.push(b'\n');
    stream.write_all(&output).expect("write the stream output");
    let _sent = events.send(StreamEvent::OutputWritten);
    stream
        .read_to_end(&mut input)
        .expect("read the client's input");
    let _sent = events.send(StreamEvent::InputReceived(input.len()));
}

/// Floods output until the client goes away and reads the client's input,
/// reporting the keystroke when it arrives.
fn serve_flooding_attach_stream(reader: BufReader<UnixStream>, events: &mpsc::Sender<StreamEvent>) {
    let mut input = reader.buffer().to_vec();
    let mut stream = reader.into_inner();
    let mut flood = stream.try_clone().expect("clone the fixture stream");
    thread::spawn(move || {
        let chunk = [b'y'; FLOOD_CHUNK_BYTES];
        while flood.write_all(&chunk).is_ok() {}
    });
    let mut buffer = [0_u8; 1024];
    let mut reported = false;
    loop {
        let count = stream.read(&mut buffer).unwrap_or(0);
        if count == 0 {
            return;
        }
        input.extend_from_slice(&buffer[..count]);
        if !reported
            && input
                .windows(KEYSTROKE.len())
                .any(|window| window == KEYSTROKE)
        {
            reported = true;
            let _sent = events.send(StreamEvent::KeystrokeReceived);
        }
    }
}
