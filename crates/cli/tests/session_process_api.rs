//! Binary-level contract tests for the session process API.
//!
//! These tests deliberately invoke the built CLI and speak the public wire
//! protocol over a Unix socket. They cover behavior that parser and command
//! unit tests cannot prove: stdout/stderr separation, the versioned process
//! envelope, dedicated wait connections, and origin propagation.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

const SESSION_ID: &str = "s-fixture-1";
const SESSION_NAME: &str = "fixture-session";
const REQUEST_CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);
const SLOW_RESPONSE_DELAY: Duration = Duration::from_millis(300);

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
enum Scenario {
    Success,
    Ambiguous,
    OutputRuntimeChanged,
    SlowWait,
    WaitTimeout,
    OutputLogSafety,
}

const OUTPUT_LOG_LINE_ONE: &str = "OUTPUT_SENTINEL_DELTA_\"quoted\"_41f62a";
const OUTPUT_LOG_LINE_TWO: &str = "OUTPUT_SENTINEL_EPSILON_\\path_79c03b";

struct RedactedRequestLog {
    writer: pohunek_logging::Writer,
}

impl RedactedRequestLog {
    fn open(socket: &Path) -> Self {
        let test_root = socket
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("fixture socket lives below test root");
        let log_dir = test_root.join("state/pohunek/logs");
        let writer = pohunek_logging::Writer::open(
            &log_dir,
            pohunek_logging::config::daemon_files().expect("daemon log filenames"),
            pohunek_logging::config::daemon_policy().expect("daemon log policy"),
        )
        .expect("open real bounded daemon log sink");
        Self { writer }
    }

    fn record(&mut self, request: &Request) {
        let event = json!({
            "level": "INFO",
            "target": "fixture_daemon",
            "message": "control request handled",
            "method": request.method(),
            "protocol": request.version_range()
        });
        writeln!(
            self.writer,
            "{}",
            serde_json::to_string(&event).expect("serialize redacted request log")
        )
        .expect("write through bounded daemon log sink");
    }
}

struct TestHome {
    root: PathBuf,
}

impl TestHome {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "pohunek-cli-process-api-{}-{sequence}",
            std::process::id()
        ));
        for directory in [
            "run/pohunek",
            "state/pohunek/logs",
            "data",
            "config",
            "cache",
            "home",
            "logs",
        ] {
            fs::create_dir_all(root.join(directory)).expect("create isolated test directory");
        }
        Self { root }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/pohunek/daemon.sock")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pohunek"));
        command
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("HOME", self.root.join("home"))
            .env_remove(protocol::ENV_SESSION_ID)
            .env_remove(protocol::ENV_DAEMON_ID);
        command
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _result = fs::remove_dir_all(&self.root);
    }
}

struct FixtureDaemon {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Request>>>,
    thread: Option<thread::JoinHandle<()>>,
}

struct TcpFixtureDaemon {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Request>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TcpFixtureDaemon {
    fn start(ip: Ipv4Addr, scenario: Scenario) -> Self {
        let listener = TcpListener::bind((ip, 0)).expect("bind TCP fixture daemon");
        listener
            .set_nonblocking(true)
            .expect("set TCP fixture listener nonblocking");
        let address = listener.local_addr().expect("TCP fixture address");
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_stop = Arc::clone(&stop);
        let thread_requests = Arc::clone(&requests);
        let thread = thread::spawn(move || {
            let mut handlers = Vec::new();
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _address)) => {
                        let requests = Arc::clone(&thread_requests);
                        handlers.push(thread::spawn(move || {
                            handle_connection(stream, scenario, &requests, None);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("TCP fixture accept failed: {error}"),
                }
            }
            for handler in handlers {
                handler.join().expect("TCP fixture connection handler");
            }
        });
        Self {
            address,
            stop,
            requests,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> Vec<Request> {
        self.stop.store(true, Ordering::Release);
        let _wake = TcpStream::connect(self.address);
        self.thread
            .take()
            .expect("TCP fixture thread")
            .join()
            .expect("TCP fixture daemon");
        self.requests
            .lock()
            .expect("TCP request capture lock")
            .clone()
    }
}

impl FixtureDaemon {
    fn start(socket: &Path, scenario: Scenario) -> Self {
        let listener = UnixListener::bind(socket).expect("bind fixture daemon");
        listener
            .set_nonblocking(true)
            .expect("set fixture listener nonblocking");
        let socket = socket.to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::new(Mutex::new(RedactedRequestLog::open(&socket)));
        let thread_stop = Arc::clone(&stop);
        let thread_requests = Arc::clone(&requests);
        let thread_request_log = Arc::clone(&request_log);
        let thread = thread::spawn(move || {
            let mut handlers = Vec::new();
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _address)) => {
                        let requests = Arc::clone(&thread_requests);
                        let request_log = Arc::clone(&thread_request_log);
                        handlers.push(thread::spawn(move || {
                            handle_connection(stream, scenario, &requests, Some(&request_log));
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
            for handler in handlers {
                handler.join().expect("fixture connection handler");
            }
        });
        Self {
            socket,
            stop,
            requests,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("request capture lock").clone()
    }

    fn finish(mut self) -> Vec<Request> {
        self.stop.store(true, Ordering::Release);
        let _wake = UnixStream::connect(&self.socket);
        self.thread
            .take()
            .expect("fixture thread")
            .join()
            .expect("fixture daemon");
        let requests = self.requests();
        let _result = fs::remove_file(&self.socket);
        requests
    }
}

fn handle_connection<S>(
    stream: S,
    scenario: Scenario,
    requests: &Arc<Mutex<Vec<Request>>>,
    request_log: Option<&Arc<Mutex<RedactedRequestLog>>>,
) where
    S: std::io::Read + std::io::Write,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).expect("read request") == 0 {
        return;
    }
    let request: Request = serde_json::from_str(line.trim_end()).expect("parse request");
    requests
        .lock()
        .expect("request capture lock")
        .push(request.clone());
    if let Some(request_log) = request_log {
        request_log
            .lock()
            .expect("redacted request log lock")
            .record(&request);
    }
    if matches!(scenario, Scenario::SlowWait) && request.method() == protocol::method::SESSION_WAIT
    {
        thread::sleep(SLOW_RESPONSE_DELAY);
    }
    let response = fixture_response(&request, scenario);
    let mut stream = reader.into_inner();
    let write_result = writeln!(
        stream,
        "{}",
        serde_json::to_string(&response).expect("serialize response")
    );
    if !matches!(scenario, Scenario::SlowWait) {
        write_result.expect("write fixture response");
    }
}

fn fixture_response(request: &Request, scenario: Scenario) -> Response {
    if request.method() == protocol::method::SESSION_OUTPUT
        && matches!(scenario, Scenario::OutputRuntimeChanged)
    {
        return Response::err(
            PROTOCOL_VERSION,
            request.id(),
            protocol::ProtocolError::session_runtime_changed(),
        )
        .expect("valid error response");
    }
    let ok = match request.method() {
        protocol::method::SESSION_LIST => session_list(scenario),
        protocol::method::SESSION_SCREEN => json!({
            "session_id": SESSION_ID,
            "worker_id": "worker-fixture-1",
            "runtime_id": "runtime-fixture-1",
            "runtime_generation": "3",
            "watermark": "7",
            "dimensions": {"cols": 80, "rows": 24},
            "cursor": {"row": 1, "col": 4, "visible": true},
            "alternate_screen": false,
            "title": "fixture terminal",
            "visible_lines": ["pohunek fixture"]
        }),
        protocol::method::SESSION_OUTPUT => {
            if matches!(scenario, Scenario::OutputLogSafety) {
                let output = format!("{OUTPUT_LOG_LINE_ONE}\n{OUTPUT_LOG_LINE_TWO}");
                let end = output.len().to_string();
                json!({
                    "session_id": SESSION_ID,
                    "runtime_id": "runtime-fixture-1",
                    "runtime_generation": "3",
                    "history_start_offset": "0",
                    "start_offset": "0",
                    "next_offset": end,
                    "runtime_end_offset": end,
                    "data_base64": base64::engine::general_purpose::STANDARD.encode(output),
                    "has_more": false,
                    "timed_out": false
                })
            } else {
                json!({
                    "session_id": SESSION_ID,
                    "runtime_id": "runtime-fixture-1",
                    "runtime_generation": "3",
                    "history_start_offset": "4",
                    "start_offset": "4",
                    "next_offset": "6",
                    "runtime_end_offset": "6",
                    "data_base64": "AJ8=",
                    "gap": {"start_offset": "2", "end_offset": "4"},
                    "has_more": false,
                    "timed_out": false
                })
            }
        }
        protocol::method::SESSION_WAIT => json!({
            "reason": if matches!(scenario, Scenario::WaitTimeout) {
                "timeout"
            } else {
                "runtime_changed"
            },
            "session": session(SESSION_ID, Some(SESSION_NAME)),
            "terminal_watermark": "7",
            "output_offset": "6"
        }),
        protocol::method::SESSION_RESUME
        | protocol::method::SESSION_RESIZE
        | protocol::method::SESSION_SET_METADATA => {
            json!({"session": session(SESSION_ID, Some(SESSION_NAME))})
        }
        protocol::method::SESSION_RUNTIME_INVENTORY => json!({
            "entries": [{
                "runtime_slot": "slot-fixture-1",
                "claimed_session_id": SESSION_ID,
                "worker_id": "worker-fixture-1",
                "runtime_id": "runtime-fixture-1",
                "status": "managed"
            }]
        }),
        protocol::method::SESSION_NEW => {
            let mut result = session(SESSION_ID, Some(SESSION_NAME));
            result["applied_input"] = json!(true);
            result
        }
        protocol::method::SESSION_INPUT => json!({"accepted": true}),
        method => panic!("unexpected fixture method: {method}"),
    };
    Response::ok(PROTOCOL_VERSION, request.id(), ok).expect("valid success response")
}

fn session_list(scenario: Scenario) -> Value {
    if matches!(scenario, Scenario::Ambiguous) {
        json!([
            session("s-ambiguous-2", Some("ambiguous")),
            session("s-ambiguous-1", Some("ambiguous"))
        ])
    } else {
        json!([session(SESSION_ID, Some(SESSION_NAME))])
    }
}

fn session(id: &str, name: Option<&str>) -> Value {
    let mut value = json!({
        "id": id,
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
        "updated_at": "2026-07-08T00:01:00Z"
    });
    if let Some(name) = name {
        value["name"] = json!(name);
    }
    value
}

fn run(home: &TestHome, fixture: FixtureDaemon, args: &[&str]) -> (Output, Vec<Request>) {
    let output = home.command().args(args).output().expect("run pohunek");
    let requests = fixture.finish();
    (output, requests)
}

fn run_with_stdin(
    home: &TestHome,
    fixture: FixtureDaemon,
    args: &[&str],
    payload: &str,
) -> (Output, Vec<Request>) {
    let mut command = home.command();
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    assert!(
        command
            .get_args()
            .all(|argument| !argument.to_string_lossy().contains(payload)),
        "stdin payload must not enter argv"
    );
    let mut child = command.spawn().expect("spawn pohunek with stdin");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(payload.as_bytes())
        .expect("write stdin payload");
    let output = child.wait_with_output().expect("wait for stdin CLI");
    let requests = fixture.finish();
    (output, requests)
}

fn assert_log_directories_exclude(home: &TestHome, needles: &[String]) {
    for directory in [home.root.join("state/pohunek/logs"), home.root.join("logs")] {
        assert_files_exclude(&directory, needles);
    }
}

fn raw_and_json_escaped_needles(lines: &[&str]) -> Vec<String> {
    lines
        .iter()
        .flat_map(|line| {
            let quoted = serde_json::to_string(line).expect("encode sentinel as JSON string");
            let escaped = quoted
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .expect("JSON string is quoted")
                .to_owned();
            [(*line).to_owned(), escaped]
        })
        .collect()
}

fn assert_files_exclude(directory: &Path, needles: &[String]) {
    for entry in fs::read_dir(directory).expect("read fixture log directory") {
        let path = entry.expect("fixture log entry").path();
        if path.is_dir() {
            assert_files_exclude(&path, needles);
            continue;
        }
        let bytes = fs::read(&path).expect("read fixture log");
        let contents = String::from_utf8_lossy(&bytes);
        for needle in needles {
            assert!(
                !contents.contains(needle),
                "sensitive payload appeared in fixture log {}",
                path.display()
            );
        }
    }
}

fn assert_json_success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "stdout: {}; stderr: {}",
        utf8(&output.stdout),
        utf8(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "stderr must stay clean");
    let document = parse_single_json(&output.stdout);
    assert!(document["cli_version"].is_string());
    assert_eq!(document["protocol"]["minimum"], PROTOCOL_VERSION.get());
    assert_eq!(document["protocol"]["maximum"], PROTOCOL_VERSION.get());
    assert!(document.get("err").is_none());
    document["ok"].clone()
}

fn assert_json_error(output: &Output, expected_code: &str) -> Value {
    assert_eq!(
        output.status.code(),
        Some(1),
        "typed failures exit with code 1"
    );
    assert!(
        output.stderr.is_empty(),
        "stderr must stay clean in JSON mode"
    );
    let document = parse_single_json(&output.stdout);
    assert!(document["cli_version"].is_string());
    assert_eq!(document["protocol"]["minimum"], PROTOCOL_VERSION.get());
    assert_eq!(document["protocol"]["maximum"], PROTOCOL_VERSION.get());
    assert_eq!(document["err"]["code"], expected_code);
    assert!(document.get("ok").is_none());
    document
}

fn parse_single_json(stdout: &[u8]) -> Value {
    let mut values = serde_json::Deserializer::from_slice(stdout).into_iter::<Value>();
    let document = values
        .next()
        .expect("stdout contains a JSON value")
        .expect("stdout JSON parses");
    assert!(
        values.next().is_none(),
        "stdout must contain exactly one JSON value"
    );
    document
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn plugin_commands_have_binary_level_json_and_wire_parity() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["session", "screen", SESSION_ID, "--json"],
            protocol::method::SESSION_SCREEN,
        ),
        (
            &[
                "session",
                "output",
                SESSION_ID,
                "--runtime-id",
                "runtime-fixture-1",
                "--runtime-generation",
                "3",
                "--after-offset",
                "2",
                "--json",
            ],
            protocol::method::SESSION_OUTPUT,
        ),
        (
            &[
                "session",
                "wait",
                SESSION_ID,
                "--state",
                "done",
                "--timeout-ms",
                "50",
                "--json",
            ],
            protocol::method::SESSION_WAIT,
        ),
        (
            &["session", "resume", SESSION_ID, "--json"],
            protocol::method::SESSION_RESUME,
        ),
        (
            &[
                "session", "resize", SESSION_ID, "--cols", "100", "--rows", "40", "--json",
            ],
            protocol::method::SESSION_RESIZE,
        ),
        (
            &[
                "session",
                "metadata",
                SESSION_ID,
                "--set",
                "owner=test",
                "--clear",
                "stale",
                "--json",
            ],
            protocol::method::SESSION_SET_METADATA,
        ),
        (
            &["session", "runtime-inventory", "--json"],
            protocol::method::SESSION_RUNTIME_INVENTORY,
        ),
    ];

    for (args, expected_method) in cases {
        let home = TestHome::new();
        let fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
        let (output, requests) = run(&home, fixture, args);
        let ok = assert_json_success(&output);
        assert!(!ok.is_null(), "success payload for {expected_method}");
        assert_eq!(
            requests.last().expect("captured command request").method(),
            *expected_method
        );
        if *expected_method == protocol::method::SESSION_RUNTIME_INVENTORY {
            assert_eq!(requests.len(), 1, "inventory needs no name resolution");
        } else {
            assert_eq!(
                requests.first().expect("captured list request").method(),
                protocol::method::SESSION_LIST
            );
        }
    }
}

#[test]
fn output_json_preserves_binary_base64_and_gap_coordinates() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let (output, _requests) = run(
        &home,
        fixture,
        &[
            "session",
            "output",
            SESSION_ID,
            "--runtime-id",
            "runtime-fixture-1",
            "--runtime-generation",
            "3",
            "--after-offset",
            "2",
            "--json",
        ],
    );
    let ok = assert_json_success(&output);
    assert_eq!(ok["data_base64"], "AJ8=");
    assert_eq!(ok["gap"], json!({"start_offset": "2", "end_offset": "4"}));
    assert!(!utf8(&output.stdout).contains('\0'));
}

#[test]
fn wait_returns_a_real_timeout_result_without_a_process_signal() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::WaitTimeout);
    let (output, requests) = run(
        &home,
        fixture,
        &[
            "session",
            "wait",
            SESSION_ID,
            "--state",
            "done",
            "--timeout-ms",
            "25",
            "--json",
        ],
    );
    let ok = assert_json_success(&output);
    assert_eq!(ok["reason"], "timeout");
    let request = requests
        .iter()
        .find(|request| request.method() == protocol::method::SESSION_WAIT)
        .expect("captured wait request");
    assert_eq!(request.params()["timeout_ms"], 25);
}

#[test]
fn multiline_utf8_stdin_reaches_new_and_input_only_on_the_wire() {
    let prompt_lines = [
        "PROMPT_SENTINEL_ALPHA_\"quoted\"_28d1f7",
        "PROMPT_SENTINEL_BETA_\\path_žluťoučký_83ac40",
        "PROMPT_SENTINEL_GAMMA_tab\t5e12bb",
    ];
    let payload = prompt_lines.join("\n");
    let log_needles = raw_and_json_escaped_needles(&prompt_lines);
    let cases: &[(&[&str], &str, &str)] = &[
        (
            &["session", "new", "--input-stdin", "--json"],
            protocol::method::SESSION_NEW,
            "input",
        ),
        (
            &["session", "input", SESSION_ID, "--stdin", "--json"],
            protocol::method::SESSION_INPUT,
            "text",
        ),
    ];

    for (args, method, field) in cases {
        let home = TestHome::new();
        let fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
        let (output, requests) = run_with_stdin(&home, fixture, args, &payload);
        assert_json_success(&output);
        assert!(!utf8(&output.stdout).contains(&payload));
        assert!(!utf8(&output.stderr).contains(&payload));
        let request = requests
            .iter()
            .find(|request| request.method() == *method)
            .expect("captured stdin command request");
        assert_eq!(request.params()[*field], payload);
        assert_log_directories_exclude(&home, &log_needles);
    }
}

#[test]
fn real_bounded_log_sink_excludes_each_output_line_in_raw_and_json_forms() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::OutputLogSafety);
    let (output, _requests) = run(&home, fixture, &["session", "output", SESSION_ID, "--json"]);
    let ok = assert_json_success(&output);
    let output_lines = [OUTPUT_LOG_LINE_ONE, OUTPUT_LOG_LINE_TWO];
    let mut log_needles = raw_and_json_escaped_needles(&output_lines);
    log_needles.push(
        ok["data_base64"]
            .as_str()
            .expect("base64 output")
            .to_owned(),
    );
    assert_log_directories_exclude(&home, &log_needles);
    let daemon_log = home.root.join("state/pohunek/logs/pohunekd.jsonl");
    let logged = fs::read_to_string(daemon_log).expect("real bounded sink wrote daemon JSONL");
    assert!(logged.contains("session.output"));
}

#[test]
fn exact_name_ambiguity_is_typed_sorted_and_sends_no_mutation() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::Ambiguous);
    let (output, requests) = run(
        &home,
        fixture,
        &["session", "resume", "ambiguous", "--json"],
    );
    let document = assert_json_error(&output, "ambiguous_session_name");
    let message = document["err"]["msg"].as_str().expect("error message");
    assert!(message.contains("s-ambiguous-1, s-ambiguous-2"));
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method(), protocol::method::SESSION_LIST);
}

#[test]
fn typed_daemon_failure_keeps_stdout_machine_readable_and_stderr_clean() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::OutputRuntimeChanged);
    let (output, requests) = run(&home, fixture, &["session", "output", SESSION_ID, "--json"]);
    assert_json_error(&output, "session_runtime_changed");
    assert_eq!(
        requests.last().expect("output request").method(),
        protocol::method::SESSION_OUTPUT
    );
}

#[test]
fn complete_origin_pair_reaches_every_request_without_entering_output() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let private_session = "origin-session-private";
    let private_daemon = "origin-daemon-private";
    let output = home
        .command()
        .env(protocol::ENV_SESSION_ID, private_session)
        .env(protocol::ENV_DAEMON_ID, private_daemon)
        .args(["session", "screen", SESSION_ID, "--json"])
        .output()
        .expect("run pohunek");
    let requests = fixture.finish();
    assert_json_success(&output);
    assert!(requests.len() >= 2);
    for request in requests {
        assert_eq!(
            request.origin_session_id().map(|id| id.0.as_str()),
            Some(private_session)
        );
        assert_eq!(request.origin_daemon_id(), Some(private_daemon));
    }
    assert!(!utf8(&output.stdout).contains(private_session));
    assert!(!utf8(&output.stdout).contains(private_daemon));
    assert!(!utf8(&output.stderr).contains(private_session));
    assert!(!utf8(&output.stderr).contains(private_daemon));
}

#[test]
fn process_signals_cancel_local_wait_while_wire_timeout_remains_bounded() {
    for (signal, signal_name) in [(libc::SIGINT, "SIGINT"), (libc::SIGTERM, "SIGTERM")] {
        let home = TestHome::new();
        let fixture = FixtureDaemon::start(&home.socket(), Scenario::SlowWait);
        let mut child = home
            .command()
            .args([
                "session",
                "wait",
                SESSION_ID,
                "--state",
                "done",
                "--timeout-ms",
                "8000",
                "--json",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pohunek");
        wait_for_method(&fixture, protocol::method::SESSION_WAIT);
        send_signal(&mut child, signal, signal_name);
        let output = child.wait_with_output().expect("wait for cancelled CLI");
        let requests = fixture.finish();
        assert_json_error(&output, "cancelled");
        let wait_request = requests
            .iter()
            .find(|request| request.method() == protocol::method::SESSION_WAIT)
            .expect("captured wait request");
        assert_eq!(
            wait_request.params()["timeout_ms"],
            protocol::MAX_SESSION_WAIT_MS
        );
    }
}

fn wait_for_method(fixture: &FixtureDaemon, method: &str) {
    let deadline = Instant::now() + REQUEST_CAPTURE_TIMEOUT;
    while Instant::now() < deadline {
        if fixture
            .requests()
            .iter()
            .any(|request| request.method() == method)
        {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("fixture did not receive {method}");
}

fn send_signal(child: &mut Child, signal: libc::c_int, signal_name: &str) {
    let pid = i32::try_from(child.id()).expect("child pid fits i32");
    // SAFETY: `pid` belongs to the child spawned immediately above and the
    // supplied signal is one of the CLI's handled cancellation signals.
    let result = unsafe { libc::kill(pid, signal) };
    assert_eq!(result, 0, "send {signal_name} to CLI child");
}

#[test]
#[cfg(target_os = "linux")]
fn fake_netbird_resolution_reaches_tcp_fixture_with_origin_pair() {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let netbird_ip = "100.64.0.42";
    let tcp_fixture = TcpFixtureDaemon::start(Ipv4Addr::LOCALHOST, Scenario::Success);
    let remote_port = tcp_fixture.address.port();
    let (connect_redirect, connect_capture) = build_connect_redirect(&home);
    let bin = home.root.join("bin");
    fs::create_dir_all(&bin).expect("create fixture bin");
    let netbird = bin.join("netbird");
    fs::write(
        &netbird,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{{\"peers\":{{\"details\":[{{\"fqdn\":\"fixture-remote.netbird.test\",\"netbirdIp\":\"{netbird_ip}\",\"status\":\"Connected\"}}]}}}}'\n"
        ),
    )
    .expect("write fake netbird");
    fs::set_permissions(&netbird, fs::Permissions::from_mode(0o700))
        .expect("make fake netbird executable");
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&inherited_path));
    let path = std::env::join_paths(paths).expect("join PATH");

    let private_session = "remote-origin-session";
    let private_daemon = "remote-origin-daemon";
    let output = home
        .command()
        .env("PATH", path)
        .env("LD_PRELOAD", connect_redirect)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
        .env("POHUNEK_REMOTE_PORT", remote_port.to_string())
        .env(protocol::ENV_SESSION_ID, private_session)
        .env(protocol::ENV_DAEMON_ID, private_daemon)
        .args([
            "--host",
            "fixture-remote",
            "session",
            "screen",
            SESSION_ID,
            "--json",
        ])
        .output()
        .expect("run remote pohunek");
    let requests = tcp_fixture.finish();
    let local_requests = local_fixture.finish();
    let ok = assert_json_success(&output);
    assert_eq!(ok["session_id"], SESSION_ID);
    assert!(requests.len() >= 2, "list and screen reach TCP fixture");
    for request in requests {
        assert_eq!(
            request.origin_session_id().map(|id| id.0.as_str()),
            Some(private_session)
        );
        assert_eq!(request.origin_daemon_id(), Some(private_daemon));
    }
    assert!(
        local_requests.is_empty(),
        "remote target must not use local Unix socket"
    );
    let captured = fs::read_to_string(connect_capture).expect("read intercepted connect targets");
    let expected_target = format!("{netbird_ip}:{remote_port}");
    assert!(
        captured
            .lines()
            .filter(|line| *line == expected_target)
            .count()
            >= 2,
        "both CLI connections must target the fake NetBird address before the test-only redirect"
    );
}

#[cfg(target_os = "linux")]
fn build_connect_redirect(home: &TestHome) -> (PathBuf, PathBuf) {
    // The production resolver must reject loopback as a NetBird peer. This
    // process-local interposer preserves that validation: the CLI resolves and
    // attempts the fixed CGNAT target, the shim records that original target,
    // and only then redirects the syscall to the loopback fixture. Nothing is
    // linked into or enabled by the production binary.
    let source = home.root.join("connect-redirect.c");
    let library = home.root.join("connect-redirect.so");
    let capture = home.root.join("connect-targets.log");
    fs::write(
        &source,
        r#"#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

typedef int (*connect_fn)(int, const struct sockaddr *, socklen_t);

int connect(int fd, const struct sockaddr *address, socklen_t length) {
    static connect_fn real_connect = NULL;
    if (real_connect == NULL) {
        real_connect = (connect_fn)dlsym(RTLD_NEXT, "connect");
    }
    if (address != NULL && address->sa_family == AF_INET &&
        length >= sizeof(struct sockaddr_in)) {
        const struct sockaddr_in *original = (const struct sockaddr_in *)address;
        unsigned int host = ntohl(original->sin_addr.s_addr);
        if ((host & 0xffc00000U) == 0x64400000U) {
            const char *capture = getenv("POHUNEK_TEST_CONNECT_CAPTURE");
            if (capture != NULL) {
                char ip[INET_ADDRSTRLEN];
                char line[96];
                inet_ntop(AF_INET, &original->sin_addr, ip, sizeof(ip));
                int count = snprintf(line, sizeof(line), "%s:%u\n", ip, ntohs(original->sin_port));
                int output = open(capture, O_WRONLY | O_CREAT | O_APPEND, 0600);
                if (output >= 0) {
                    (void)write(output, line, (size_t)count);
                    (void)close(output);
                }
            }
            struct sockaddr_in redirected = *original;
            redirected.sin_addr.s_addr = htonl(0x7f000001U);
            return real_connect(fd, (const struct sockaddr *)&redirected, sizeof(redirected));
        }
    }
    return real_connect(fd, address, length);
}
"#,
    )
    .expect("write connect redirect source");
    let compiled = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2"])
        .arg(&source)
        .args(["-o"])
        .arg(&library)
        .arg("-ldl")
        .output()
        .expect("compile connect redirect");
    assert!(
        compiled.status.success(),
        "compile connect redirect: {}",
        utf8(&compiled.stderr)
    );
    (library, capture)
}
