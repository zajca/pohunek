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
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{mpsc, Condvar, Mutex};
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

/// Bytes per write while the test measures a pipe.
#[cfg(not(target_os = "linux"))]
const PIPE_FILL_CHUNK_BYTES: usize = 1024;

/// Size of the write that shows the filled pipe is at its largest buffer.
#[cfg(not(target_os = "linux"))]
const PIPE_GROWTH_PROBE_BYTES: usize = 256 * 1024;

/// Bytes pasted before the detach shortcut: more than the sockets between the
/// client and the stream hold, less than the client's input queue limit.
const UNREAD_PASTE_BYTES: usize = 768 * 1024;

/// Output the stream writes after it read the client's input to its end.
const ANSWER: &[u8] = b"answer-after-the-input-was-delivered\n";

/// Opened by the test once the client has exited; the stream reads no input
/// before.
static INPUT_GATE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

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
    /// Reads no input until `INPUT_GATE` opens, then reads it to its end.
    HoldInput,
    /// Reads the input to its end, writes `ANSWER` and closes the stream; the
    /// answer reaches the client only while it keeps the connection open.
    AnswerAfterInput,
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
        stdin
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
    // Stdin stays open until the output was shown, so the client does not end
    // before it.
    drop(pasting.join().expect("paste thread"));
    assert_eq!(
        observed.recv_timeout(HANG_GUARD),
        Ok(StreamEvent::InputReceived(PASTE_BYTES)),
        "the whole paste reaches the stream once it is read"
    );
    let status = cli.0.wait().expect("wait for pohunek attach");
    reading.join().expect("output thread");
    let mut errors = String::new();
    stderr
        .read_to_string(&mut errors)
        .expect("read the CLI errors");
    assert!(status.success(), "pohunek attach failed: {errors}");
}

/// Sizes the CLI's stdout pipe before the CLI exists and returns the byte count
/// that means the pipe is full once the CLI wrote into it.
#[cfg(target_os = "linux")]
fn prepare_terminal_pipe(read: &OwnedFd, _write: &OwnedFd) -> u64 {
    rustix::pipe::fcntl_setpipe_size(read, STDOUT_PIPE_BYTES).expect("shrink the stdout pipe");
    rustix::pipe::fcntl_getpipe_size(read).expect("read the pipe capacity") as u64
}

/// A pipe of another platform cannot be resized and its capacity can differ
/// from one pipe to the next, so it is measured: the pipe is filled until a
/// write would block, which grows it to its largest buffer; a further large
/// write shows it cannot grow more, and half of it is read out again so that
/// the CLI's own output is what fills the pipe. The write end is blocking
/// again before the CLI inherits it.
#[cfg(not(target_os = "linux"))]
fn prepare_terminal_pipe(read: &OwnedFd, write: &OwnedFd) -> u64 {
    use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
    use rustix::io::Errno;

    let blocking = fcntl_getfl(write).expect("read the pipe flags");
    fcntl_setfl(write, blocking | OFlags::NONBLOCK).expect("make the pipe write end non-blocking");
    let fill_until_full = |chunk: &[u8]| loop {
        match rustix::io::write(write, chunk) {
            Ok(_) => {}
            Err(Errno::AGAIN) => break,
            Err(error) => panic!("fill the terminal pipe: {error}"),
        }
    };
    fill_until_full(&[0_u8; PIPE_FILL_CHUNK_BYTES]);
    let capacity = rustix::io::ioctl_fionread(read).expect("count the pipe bytes");
    // The fill grew the pipe as far as it goes; a larger write finds no room.
    fill_until_full(&vec![0_u8; PIPE_GROWTH_PROBE_BYTES]);
    assert_eq!(
        rustix::io::ioctl_fionread(read).expect("count the pipe bytes"),
        capacity,
        "the full pipe grew"
    );
    // Half of the pipe: the CLI has room to write into, and the pipe is at its
    // largest buffer already.
    let mut slice = vec![0_u8; usize::try_from(capacity / 2).expect("pipe capacity")];
    let taken = rustix::io::read(read, &mut slice).expect("read a slice out of the pipe");
    assert!(taken > 0, "the full pipe held no data");
    fcntl_setfl(write, blocking).expect("make the pipe write end blocking");
    capacity
}

/// Hermetic environment, fixture daemon and a CLI whose stdout pipe is never
/// read, so it is full as soon as the stream floods the client.
struct StalledTerminal {
    cli: CliProcess,
    stdin: std::process::ChildStdin,
    stdout: OwnedFd,
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

    // The pipe is sized before the CLI exists, so the CLI starts against a pipe
    // whose capacity is known.
    let (stdout, terminal_write) = rustix::pipe::pipe().expect("create the terminal pipe");
    let capacity = prepare_terminal_pipe(&stdout, &terminal_write);
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
        (queued >= capacity).then_some(())
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

#[test]
fn an_attach_client_detaches_past_an_unread_paste_and_reports_what_it_dropped() {
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
    serve_fixture_daemon(&socket, events, StreamBehavior::HoldInput);

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
    let showing = thread::spawn(move || {
        let _count = std::io::copy(&mut stdout, &mut std::io::sink());
    });

    // The stream reads none of the paste, then the detach shortcut follows it.
    let mut typed = vec![b'x'; UNREAD_PASTE_BYTES];
    typed.push(DETACH_BYTE);
    let typing = thread::spawn(move || {
        let _written = stdin.write_all(&typed);
        stdin
    });
    let status = poll_until("the attach client to detach", || {
        cli.0.try_wait().expect("poll the attach client")
    });
    assert!(status.success(), "the detach ends the attach: {status}");
    showing.join().expect("output thread");
    drop(typing.join().expect("typing thread"));

    let (opened, wake) = &INPUT_GATE;
    *opened.lock().expect("input gate") = true;
    wake.notify_all();
    let Ok(StreamEvent::InputReceived(delivered)) = observed.recv_timeout(HANG_GUARD) else {
        panic!("the stream reports the input it received");
    };
    let mut errors = String::new();
    stderr
        .read_to_string(&mut errors)
        .expect("read the CLI errors");
    let dropped: usize = errors
        .split("detached; ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("the client reports the dropped bytes: {errors:?}"));
    assert!(dropped > 0, "{errors:?}");
    assert_eq!(
        delivered + dropped,
        UNREAD_PASTE_BYTES,
        "every pasted byte was delivered in order or reported as dropped"
    );
}

#[test]
fn a_piped_attach_keeps_reading_until_the_stream_closes_after_its_input_ended() {
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
    serve_fixture_daemon(&socket, events, StreamBehavior::AnswerAfterInput);

    let mut command = env.command(pohunek_test_support::bin_exe("pohunek"));
    command
        .args(["attach", SESSION_ID])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut cli = CliProcess(command.spawn().expect("spawn pohunek attach"));
    let mut stdin = cli.0.stdin.take().expect("piped stdin");
    let mut stdout = cli.0.stdout.take().expect("piped stdout");

    // The input ends the client's stdin; the stream answers once it read the
    // input to its end, and closes. The client keeps the connection open for
    // that answer instead of closing it with the end of its input.
    stdin.write_all(KEYSTROKE).expect("pipe the input");
    drop(stdin);
    let (output, shown_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut shown = Vec::new();
        let _count = stdout.read_to_end(&mut shown);
        let _sent = output.send(shown);
    });
    let shown = shown_rx
        .recv_timeout(HANG_GUARD)
        .expect("the client ends once the stream closed");
    let status = cli.0.wait().expect("wait for pohunek attach");

    assert!(status.success(), "{status}");
    assert_eq!(
        observed.recv_timeout(HANG_GUARD),
        Ok(StreamEvent::InputReceived(KEYSTROKE.len()))
    );
    assert!(
        shown.windows(ANSWER.len()).any(|window| window == ANSWER),
        "the client keeps reading until the stream closes: {:?}",
        String::from_utf8_lossy(&shown)
    );
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
                StreamBehavior::HoldInput => serve_input_holding_attach_stream(reader, events),
                StreamBehavior::AnswerAfterInput => serve_answering_attach_stream(reader, events),
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

/// Holds the stream's input unread until `INPUT_GATE` opens, then reports how
/// many bytes the client had delivered.
fn serve_input_holding_attach_stream(
    reader: BufReader<UnixStream>,
    events: &mpsc::Sender<StreamEvent>,
) {
    let mut input = reader.buffer().to_vec();
    let mut stream = reader.into_inner();
    {
        let (opened, wake) = &INPUT_GATE;
        let mut open = opened.lock().expect("input gate");
        while !*open {
            open = wake.wait(open).expect("input gate");
        }
    }
    stream
        .read_to_end(&mut input)
        .expect("read the client's input");
    let _sent = events.send(StreamEvent::InputReceived(input.len()));
}

/// Reads the client's input to its end, answers and closes the stream.
fn serve_answering_attach_stream(
    reader: BufReader<UnixStream>,
    events: &mpsc::Sender<StreamEvent>,
) {
    let mut input = reader.buffer().to_vec();
    let mut stream = reader.into_inner();
    stream
        .read_to_end(&mut input)
        .expect("read the client's input");
    stream.write_all(ANSWER).expect("write the answer");
    let _sent = events.send(StreamEvent::InputReceived(input.len()));
}
