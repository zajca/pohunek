//! Binary-level contract tests for the session process API.
//!
//! These tests deliberately invoke the built CLI and speak the public wire
//! protocol over a Unix socket. They cover stdout/stderr separation, the
//! versioned process envelope, dedicated wait connections, origin
//! propagation, and argument validation before a daemon connection.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::Shutdown;
#[cfg(target_os = "linux")]
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, CLIENT_PROTOCOL_VERSIONS, PROTOCOL_VERSION};
use serde_json::{json, Value};

#[cfg(target_os = "linux")]
#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[cfg(target_os = "linux")]
#[path = "support/pin_overlay.rs"]
mod pin_overlay;

const SESSION_ID: &str = "s-fixture-1";
const SESSION_NAME: &str = "fixture-session";
const SLOW_RESPONSE_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy)]
enum Scenario {
    Success,
    NativeRecovery,
    GovernanceInspectSuccess,
    GovernanceInspectError,
    DoctorReport,
    Ambiguous,
    OutputRuntimeChanged,
    SlowWait,
    SlowInput,
    PromptAcceptedInput,
    NoReplyToNew,
    WaitTimeout,
    OutputLogSafety,
    RetentionSweepClean,
    RetentionSweepPartialFailure,
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
    env: TestEnv,
    root: PathBuf,
    worker: PathBuf,
}

impl TestHome {
    fn new() -> Self {
        // `TestEnv` keeps the root short and canonical, so the longest worker
        // socket path (`run/pohunek/workers/<session>/control.sock`) stays
        // within the macOS 103-byte `sun_path` limit.
        let env = TestEnv::new().expect("create the hermetic test environment");
        let root = env.root().to_path_buf();
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
        // The daemon creates these directories owner-private, and doctor
        // validates them like startup does: exact mode 0700.
        for directory in PRIVATE_DIRECTORIES {
            fs::set_permissions(
                root.join(directory),
                fs::Permissions::from_mode(PRIVATE_DIR_MODE),
            )
            .expect("make the directory owner-private");
        }
        // Doctor's `worker_executable` check requires an executable
        // `pohunek-sessiond` on macOS; a scripted stand-in satisfies it.
        let worker = root.join("bin-worker");
        pohunek_test_support::fs::write_executable(&worker, b"#!/bin/sh\nexit 0\n")
            .expect("write worker stand-in");
        Self { env, root, worker }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/pohunek/daemon.sock")
    }

    fn command(&self) -> Command {
        let mut command = self.env.command(pohunek_test_support::bin_exe("pohunek"));
        command.env("POHUNEK_WORKER_BIN", &self.worker);
        command
    }
}

/// Doctor check that reports the host's login session rather than the
/// fixture: it may fail on a host without a graphical session.
const HOST_SESSION_CHECK: &str = "launchd_domain";

#[test]
fn doctor_process_skips_untrusted_git_candidates_before_a_trusted_one() {
    let home = TestHome::new();
    let loose_file_dir = home.root.join("loose-file-bin");
    let loose_dir = home.root.join("loose-directory-bin");
    let trusted_dir = home.root.join("trusted-bin");
    for directory in [&loose_file_dir, &loose_dir, &trusted_dir] {
        fs::create_dir(directory).expect("create executable fixture directory");
    }
    let loose_file = loose_file_dir.join("git");
    let loose_directory_file = loose_dir.join("git");
    let trusted_file = trusted_dir.join("git");
    for file in [&loose_file, &loose_directory_file, &trusted_file] {
        pohunek_test_support::fs::write_executable(file, b"#!/bin/sh\nexit 0\n")
            .expect("write executable fixture");
    }
    fs::set_permissions(&loose_file, fs::Permissions::from_mode(0o777))
        .expect("make first executable untrusted");
    fs::set_permissions(&loose_dir, fs::Permissions::from_mode(0o777))
        .expect("make second executable directory untrusted");

    let git_check = |path: &std::ffi::OsStr| {
        let output = home
            .command()
            .args(["doctor", "--json"])
            .env("PATH", path)
            .output()
            .expect("run doctor process");
        let document: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("doctor JSON envelope: {error}\n{}", describe(&output)));
        document["ok"]["checks"]
            .as_array()
            .expect("doctor checks")
            .iter()
            .find(|check| check["name"] == "bin:git")
            .expect("git check")
            .clone()
    };

    for unsafe_only in [&loose_file_dir, &loose_dir] {
        let check = git_check(unsafe_only.as_os_str());
        assert_eq!(check["status"], "fail", "{check}");
    }
    let path = std::env::join_paths([&loose_file_dir, &loose_dir, &trusted_dir])
        .expect("join fixture PATH");
    let check = git_check(&path);
    assert_eq!(check["status"], "ok", "{check}");
    assert!(
        check["detail"]
            .as_str()
            .expect("git detail")
            .contains(&trusted_file.display().to_string()),
        "{check}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn doctor_process_creates_private_probe_directories_without_following_planted_symlinks() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let logs = env.state_home().join("pohunek/logs");
    fs::create_dir_all(&logs).expect("create log probe directory");
    let victim = env.root().join("victim");
    fs::write(&victim, b"precious").expect("write symlink target");
    std::os::unix::fs::symlink(&victim, logs.join(".pohunek-doctor-probe"))
        .expect("plant old predictable probe name");

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["doctor", "--json"])
        .env("PATH", "")
        .output()
        .expect("run doctor process");
    let document: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("doctor JSON envelope: {error}\n{}", describe(&output)));
    let checks = document["ok"]["checks"].as_array().expect("doctor checks");
    let names = checks
        .iter()
        .map(|check| check["name"].as_str().expect("check name"))
        .collect::<Vec<_>>();
    assert!(
        names.starts_with(&[
            "bin:git",
            "bin:codex",
            "bin:claude",
            "socket_dir_writable",
            "state_dir_writable",
            "log_dir_writable",
            "netbird_cli",
            "schema_version",
        ]),
        "{names:?}"
    );
    for name in [
        "socket_dir_writable",
        "state_dir_writable",
        "log_dir_writable",
    ] {
        let check = checks
            .iter()
            .find(|check| check["name"] == name)
            .expect("directory check");
        assert_eq!(check["status"], "ok", "{check}");
    }
    for directory in [
        env.runtime_dir().join("pohunek"),
        env.data_home().join("pohunek"),
    ] {
        let mode = fs::metadata(&directory)
            .expect("doctor created private directory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{}", directory.display());
        assert_eq!(
            fs::read_dir(&directory)
                .expect("read directory after probe")
                .count(),
            0,
            "probe left a file in {}",
            directory.display()
        );
    }
    assert_eq!(fs::read(&victim).expect("read symlink target"), b"precious");
    assert_eq!(
        fs::read_dir(&logs)
            .expect("read log probe directory")
            .count(),
        1,
        "only the planted link may remain"
    );
}

/// Mode of every directory the daemon creates privately.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Fixture directories the daemon would create owner-private: the runtime
/// root, the state root, and the log directory below it.
const PRIVATE_DIRECTORIES: [&str; 3] = ["run/pohunek", "state/pohunek", "state/pohunek/logs"];

#[test]
fn doctor_process_redacts_an_unavailable_local_daemon_in_human_and_json_output() {
    let home = TestHome::new();
    let socket = home.socket();
    let expected_detail =
        "Daemon governance inspection is unavailable; start the local daemon and retry.";

    let json = home
        .command()
        .args(["doctor", "--json"])
        .output()
        .expect("run JSON doctor process");
    let document: Value = serde_json::from_slice(&json.stdout).unwrap_or_else(|error| {
        panic!("doctor JSON envelope: {error}\n{}", describe(&json));
    });
    let checks = document["ok"]["checks"].as_array().expect("doctor checks");
    // The isolated fixture satisfies every required check except the ones that
    // describe the host session itself: on macOS the `gui/<uid>` launchd domain
    // exists only inside a graphical login session.
    let failing = checks
        .iter()
        .filter(|check| check["status"] == "fail")
        .map(|check| check["name"].as_str().unwrap_or("?").to_owned())
        .filter(|name| name != HOST_SESSION_CHECK)
        .collect::<Vec<_>>();
    assert!(
        failing.is_empty(),
        "required doctor checks failed: {failing:?}\n{}",
        describe(&json)
    );
    let unavailable = checks
        .iter()
        .filter(|check| check["name"] == "host_governance_inspection")
        .collect::<Vec<_>>();
    assert_eq!(unavailable.len(), 1, "{}", describe(&json));
    assert_eq!(unavailable[0]["status"], "warn");
    assert_eq!(unavailable[0]["detail"], expected_detail);
    assert_redacted_doctor_output(&utf8(&json.stdout), &socket);

    let human = home
        .command()
        .arg("doctor")
        .output()
        .expect("run doctor process");
    let host_session_failed = checks
        .iter()
        .any(|check| check["status"] == "fail" && check["name"] == HOST_SESSION_CHECK);
    assert_eq!(
        human.status.success(),
        !host_session_failed,
        "human doctor exit status must follow the same checks as JSON\n{}",
        describe(&human)
    );
    let human_stdout = utf8(&human.stdout);
    assert_eq!(
        human_stdout.matches("host_governance_inspection").count(),
        1,
        "human doctor must emit exactly one unavailable-governance check\n{}",
        describe(&human)
    );
    assert!(human_stdout.contains("warn"));
    assert!(human_stdout.contains(expected_detail));
    assert_redacted_doctor_output(&human_stdout, &socket);
}

#[test]
fn doctor_renders_daemon_profile_failures_and_redacts_details_in_human_and_json() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::DoctorReport);
    let json_output = home
        .command()
        .args(["doctor", "--json"])
        .output()
        .expect("run JSON doctor process");
    let document: Value = serde_json::from_slice(&json_output.stdout)
        .unwrap_or_else(|error| panic!("doctor JSON: {error}\n{}", describe(&json_output)));
    let checks = document["ok"]["checks"].as_array().expect("doctor checks");
    let profile = checks
        .iter()
        .find(|check| check["name"] == "agent_profile_bad-profile")
        .expect("CLI exposes daemon profile check");
    assert_eq!(profile["status"], "fail");
    let worker = checks
        .iter()
        .find(|check| check["name"] == "worker_executable")
        .expect("CLI exposes daemon worker readiness check");
    assert_eq!(worker["status"], "fail");
    assert!(!json_output
        .stdout
        .windows(b"PROFILE_SECRET_CANARY".len())
        .any(|window| window == b"PROFILE_SECRET_CANARY"));

    let human = home
        .command()
        .arg("doctor")
        .output()
        .expect("run human doctor process");
    let human_text = utf8(&human.stdout);
    assert!(
        human_text.contains("agent_profile_bad-profile"),
        "{}",
        describe(&human)
    );
    assert!(human_text.contains("fail"), "{}", describe(&human));
    assert!(!human_text.contains("PROFILE_SECRET_CANARY"));
    assert!(
        !human.status.success(),
        "daemon profile failure controls CLI exit status"
    );
    let requests = fixture.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method() == protocol::method::DAEMON_DOCTOR)
            .count(),
        2,
        "JSON and human doctor both call daemon.doctor"
    );
}

#[test]
fn doctor_reports_only_a_safe_netbird_self_address() {
    let home = TestHome::new();
    let bin = home.root.join("doctor-netbird-bin");
    fs::create_dir_all(&bin).expect("create NetBird fixture bin");
    let program = bin.join("netbird");
    for (address, expected_status) in [("100.64.0.10/16", "ok"), ("8.8.8.8", "warn")] {
        pohunek_test_support::fs::write_executable(
            &program,
            format!("#!/bin/sh\nprintf '%s\\n' '{{\"netbirdIp\":\"{address}\"}}'\n"),
        )
        .expect("write NetBird status fixture");
        let output = home
            .command()
            .env("PATH", &bin)
            .args(["doctor", "--json"])
            .output()
            .expect("run doctor CLI");
        let document: Value =
            serde_json::from_slice(&output.stdout).expect("versioned doctor JSON");
        let checks = document["ok"]["checks"].as_array().expect("doctor checks");
        let check = checks
            .iter()
            .find(|check| check["name"] == "netbird_cli")
            .expect("NetBird doctor check");
        assert_eq!(check["status"], expected_status, "address {address}");
        if expected_status == "warn" {
            assert!(!check["detail"]
                .as_str()
                .unwrap_or_default()
                .contains(address));
        }
    }
}

/// Renders a process outcome for assertion messages: status, stdout, stderr.
fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        utf8(&output.stdout),
        utf8(&output.stderr)
    )
}

fn assert_redacted_doctor_output(output: &str, socket: &Path) {
    assert!(!output.contains(&socket.display().to_string()));
    for forbidden in [
        "client error",
        "transport error",
        "connection refused",
        "os error",
    ] {
        assert!(
            !output.to_ascii_lowercase().contains(forbidden),
            "doctor output leaks unavailable-daemon detail: {forbidden}"
        );
    }
}

struct FixtureDaemon {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Request>>>,
    accepted: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
struct TcpFixtureDaemon {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Request>>>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
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
            let mut connections = Vec::new();
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _address)) => {
                        connections.push(stream.try_clone().expect("clone TCP fixture connection"));
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
            // Closing the fixture's end unblocks a handler still waiting for a
            // request line from a client that never sent one.
            for connection in &connections {
                let _closed = connection.shutdown(Shutdown::Both);
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
        join_fixture_thread(
            "TCP fixture daemon to stop",
            self.thread.take().expect("TCP fixture thread"),
        );
        self.requests
            .lock()
            .expect("TCP request capture lock")
            .clone()
    }
}

/// Joins a fixture thread, failing with a named step instead of hanging.
fn join_fixture_thread(what: &str, thread: thread::JoinHandle<()>) {
    pohunek_test_support::wait::poll_until(what, || thread.is_finished().then_some(()));
    thread.join().expect(what);
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
        let accepted = Arc::new(AtomicUsize::new(0));
        let thread_accepted = Arc::clone(&accepted);
        let thread = thread::spawn(move || {
            let mut handlers = Vec::new();
            let mut connections = Vec::new();
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _address)) => {
                        // BSD sockets inherit O_NONBLOCK from the listener on
                        // accept; the handler reads with blocking semantics.
                        stream
                            .set_nonblocking(false)
                            .expect("set fixture connection blocking");
                        connections.push(stream.try_clone().expect("clone fixture connection"));
                        thread_accepted.fetch_add(1, Ordering::Release);
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
            // Closing the fixture's end unblocks a handler still waiting for a
            // request line from a client that never sent one.
            for connection in &connections {
                let _closed = connection.shutdown(Shutdown::Both);
            }
            for handler in handlers {
                handler.join().expect("fixture connection handler");
            }
        });
        Self {
            socket,
            stop,
            requests,
            accepted,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("request capture lock").clone()
    }

    fn finish(mut self) -> Vec<Request> {
        // The accept loop polls a non-blocking listener, so it observes `stop`
        // within one poll interval. Connecting to it as a wake-up would let it
        // accept that connection and then wait on it while this thread waits
        // for the loop to end.
        self.stop.store(true, Ordering::Release);
        join_fixture_thread(
            "fixture daemon to stop",
            self.thread.take().expect("fixture thread"),
        );
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
    // A client whose first request depends on the daemon's protocol version
    // sends `daemon.health` first. The fixture answers it like a daemon and keeps
    // it out of the captured wire, which holds the method requests only.
    let request: Request = loop {
        line.clear();
        if reader.read_line(&mut line).expect("read request") == 0 {
            return;
        }
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse request");
        if request.method() != protocol::method::DAEMON_HEALTH {
            break request;
        }
        let health = Response::ok(
            PROTOCOL_VERSION,
            request.id(),
            json!({
                "status": "ok",
                "daemon_version": "fixture",
                "protocol_version": PROTOCOL_VERSION.get()
            }),
        )
        .expect("valid health response");
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&health).expect("serialize health response")
        )
        .expect("write health response");
    };
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
    if matches!(scenario, Scenario::SlowInput)
        && request.method() == protocol::method::SESSION_INPUT
    {
        thread::sleep(SLOW_RESPONSE_DELAY);
    }
    if matches!(scenario, Scenario::NoReplyToNew)
        && request.method() == protocol::method::SESSION_NEW
    {
        // The creation is never answered: the connection is held until the
        // client hangs up, so only the client's own deadline ends the call.
        let mut rest = String::new();
        while reader.read_line(&mut rest).is_ok_and(|read| read > 0) {
            rest.clear();
        }
        return;
    }
    let response = fixture_response(&request, scenario);
    let mut stream = reader.into_inner();
    let write_result = writeln!(
        stream,
        "{}",
        serde_json::to_string(&response).expect("serialize response")
    );
    if !matches!(scenario, Scenario::SlowWait | Scenario::SlowInput) {
        write_result.expect("write fixture response");
    }
}

fn fixture_response(request: &Request, scenario: Scenario) -> Response {
    if let Some(response) = governance_inspect_response(request, scenario) {
        return response;
    }
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
        protocol::method::SESSION_LIST => session_list(request, scenario),
        protocol::method::DAEMON_DOCTOR if matches!(scenario, Scenario::DoctorReport) => json!({
            "report": {
                "overall": "fail",
                "checks": [{
                    "name": "agent_profile_bad-profile",
                    "status": "fail",
                    "detail": "The profile is malformed; repair its configuration."
                }, {
                    "name": "worker_executable",
                    "status": "fail",
                    "detail": "The active session worker is malformed or incompatible; repair or reinstall the worker binary."
                }]
            }
        }),
        protocol::method::SESSION_INSPECT => {
            session_for_scenario(SESSION_ID, Some(SESSION_NAME), scenario)
        }
        protocol::method::SESSION_FORK => session(SESSION_ID, Some(SESSION_NAME)),
        protocol::method::SESSION_SCREEN => json!({
            "session_id": SESSION_ID,
            "worker_id": "worker-fixture-1",
            "worker_instance_id": "runtime-fixture-1",
            "runtime_generation": "3",
            "watermark": "7",
            "dimensions": {"cols": 80, "rows": 24},
            "cursor": {"row": 1, "col": 4, "visible": true},
            "alternate_screen": false,
            "title": "fixture terminal",
            "visible_lines": ["pohunek fixture"]
        }),
        protocol::method::SESSION_READ => json!({
            "text": "pohunek fixture",
            "source_used": "visible",
            "worker_instance_id": "runtime-fixture-1",
            "runtime_generation": "3",
            "revision": "7",
            "alternate_screen": false,
            "lines_requested": 1,
            "truncated": false
        }),
        protocol::method::SESSION_OUTPUT => output_response(scenario),
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
        | protocol::method::SESSION_SET_METADATA
        | protocol::method::SESSION_RENAME => {
            json!({"session": session(SESSION_ID, Some(SESSION_NAME))})
        }
        protocol::method::SESSION_RUNTIME_INVENTORY => json!({
            "entries": [{
                "runtime_slot": "slot-fixture-1",
                "claimed_session_id": SESSION_ID,
                "worker_id": "worker-fixture-1",
                "worker_instance_id": "runtime-fixture-1",
                "status": "managed"
            }]
        }),
        protocol::method::SESSION_NEW => {
            let mut result = session(SESSION_ID, Some(SESSION_NAME));
            result["applied_input"] = json!(true);
            result
        }
        protocol::method::SESSION_INPUT if matches!(scenario, Scenario::PromptAcceptedInput) => {
            prompt_accepted_input_result()
        }
        protocol::method::SESSION_INPUT => json!({"accepted": true}),
        protocol::method::HOST_INSPECT => host_capabilities(),
        protocol::method::SESSION_RETENTION_SWEEP => retention_sweep(scenario),
        method => panic!("unexpected fixture method: {method}"),
    };
    Response::ok(PROTOCOL_VERSION, request.id(), ok).expect("valid success response")
}

/// The success a daemon answers an opted-in `working` wait with when only the
/// agent's prompt-submit hook acknowledged the input.
fn prompt_accepted_input_result() -> Value {
    json!({
        "accepted": true,
        "submitted": true,
        "prompt_accepted": true,
        "runtime": {"worker_instance_id": "runtime-fixture-1", "runtime_generation": "3"},
        "activity_epoch": "daemon-fixture-epoch"
    })
}

/// Host capabilities that report no runtime, so an explicit `--agent` stays
/// the daemon's decision and the assistant selects it.
fn host_capabilities() -> Value {
    json!({
        "daemon_version": "fixture",
        "protocol_version": PROTOCOL_VERSION.get(),
        "supported_agents": [],
        "runtimes": [],
        "git_available": false,
        "worktree_supported": false,
        "terminal_read_supported": false,
        "output_read_supported": false,
        "session_wait_supported": false
    })
}

fn output_response(scenario: Scenario) -> Value {
    if matches!(scenario, Scenario::OutputLogSafety) {
        let output = format!("{OUTPUT_LOG_LINE_ONE}\n{OUTPUT_LOG_LINE_TWO}");
        let end = output.len().to_string();
        json!({
            "session_id": SESSION_ID,
            "worker_instance_id": "runtime-fixture-1",
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
            "worker_instance_id": "runtime-fixture-1",
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

/// A sweep result whose only difference between scenarios is whether it reports a
/// partial failure, which is what the exit status has to reflect.
fn retention_sweep(scenario: Scenario) -> Value {
    let partial = matches!(scenario, Scenario::RetentionSweepPartialFailure);
    json!({
        "dry_run": false,
        "examined": 3,
        "eligible": 1,
        "held": 1,
        "removed": 1,
        "worktrees_cleaned": u32::from(!partial),
        "worktrees_failed": u32::from(partial),
        "failed": u32::from(partial),
        "sessions": []
    })
}

fn governance_inspect_response(request: &Request, scenario: Scenario) -> Option<Response> {
    if request.method() != protocol::method::HOST_GOVERNANCE_INSPECT {
        return None;
    }
    match scenario {
        Scenario::GovernanceInspectSuccess => Some(
            Response::ok(PROTOCOL_VERSION, request.id(), governance_status())
                .expect("valid governance inspection response"),
        ),
        Scenario::GovernanceInspectError => Some(
            Response::err(
                PROTOCOL_VERSION,
                request.id(),
                protocol::ProtocolError::new(
                    protocol::ErrorClass::Daemon,
                    "host_governance_unavailable",
                    "safe governance inspection is unavailable",
                    None,
                ),
            )
            .expect("valid governance inspection error"),
        ),
        _ => None,
    }
}

fn governance_status() -> Value {
    json!({
        "host_id": governance_identifier("host_"),
        "enrollment": {
            "relay_id": governance_identifier("relay_"),
            "status": "quarantined",
            "revision": "1"
        },
        "owner": {
            "kind": "team",
            "id": governance_identifier("team_")
        },
        "owner_revision": "1",
        "quarantine": "projection_conflict",
        "approval_key_reference": governance_identifier("approval_key_")
    })
}

fn governance_identifier(prefix: &str) -> String {
    format!("{prefix}{}", "A".repeat(43))
}

fn session_list(request: &Request, scenario: Scenario) -> Value {
    if matches!(scenario, Scenario::NoReplyToNew) {
        return recorded_creation_list(request);
    }
    if matches!(scenario, Scenario::Ambiguous) {
        json!([
            session("s-ambiguous-2", Some("ambiguous")),
            session("s-ambiguous-1", Some("ambiguous"))
        ])
    } else {
        json!([session_for_scenario(
            SESSION_ID,
            Some(SESSION_NAME),
            scenario
        )])
    }
}

/// The caller key an assistant launch of the request-form scenarios carries,
/// as `--meta launch=req-7f3a`.
const LAUNCH_KEY: (&str, &str) = ("launch", "req-7f3a");

/// The list of a daemon that recorded the unanswered creation under the
/// caller's key, filtered with the protocol's own filter semantics.
fn recorded_creation_list(request: &Request) -> Value {
    let mut recorded = session(SESSION_ID, None);
    recorded["metadata"] = json!({LAUNCH_KEY.0: LAUNCH_KEY.1});
    let recorded: protocol::SessionInfo =
        serde_json::from_value(recorded).expect("a valid recorded session");
    let params: protocol::SessionListParams =
        serde_json::from_value(request.params().clone()).expect("valid list params");
    let listed = params
        .filters
        .iter()
        .all(|filter| filter.matches(&recorded))
        .then_some(recorded);
    serde_json::to_value(listed.into_iter().collect::<Vec<_>>()).expect("serialize the list")
}

fn session_for_scenario(id: &str, name: Option<&str>, scenario: Scenario) -> Value {
    let mut value = session(id, name);
    if matches!(scenario, Scenario::NativeRecovery) {
        value["agent"] = json!("claude");
        value["agent_base"] = json!("claude");
        value["native_session_id"] = json!("native-fixture-conversation");
        value["native_last_activity_at"] = json!("2023-11-14T22:13:20Z");
    }
    value
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
        "updated_at": "2026-07-08T00:01:00Z",
        "runtime": {
            "state": "live",
            "runtime_generation": "3",
            "worker_id": "worker-fixture-1",
            "worker_instance_id": "runtime-fixture-1"
        }
    });
    if let Some(name) = name {
        value["name"] = json!(name);
    }
    value
}

#[test]
fn human_session_views_show_the_exact_recovery_target_and_transcript_time() {
    for (args, expected_method) in [
        (
            vec!["session", "inspect", SESSION_ID],
            protocol::method::SESSION_INSPECT,
        ),
        (vec!["session", "list"], protocol::method::SESSION_LIST),
    ] {
        let home = TestHome::new();
        let fixture = FixtureDaemon::start(&home.socket(), Scenario::NativeRecovery);
        let (output, requests) = run(&home, fixture, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).expect("human output is UTF-8");
        assert!(text.contains("native-fixture-conversation"), "{text}");
        assert!(text.contains("2023-11-14T22:13:20Z"), "{text}");
        if expected_method == protocol::method::SESSION_INSPECT {
            assert!(text.contains("native_binding"), "{text}");
            assert!(!text.contains("resumable"), "{text}");
        }
        assert_eq!(
            requests.last().expect("view request").method(),
            expected_method
        );
    }
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
    assert_eq!(
        document["protocol"]["minimum"],
        CLIENT_PROTOCOL_VERSIONS.minimum().get()
    );
    assert_eq!(
        document["protocol"]["maximum"],
        CLIENT_PROTOCOL_VERSIONS.maximum().get()
    );
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
    assert_eq!(
        document["protocol"]["minimum"],
        CLIENT_PROTOCOL_VERSIONS.minimum().get()
    );
    assert_eq!(
        document["protocol"]["maximum"],
        CLIENT_PROTOCOL_VERSIONS.maximum().get()
    );
    assert_eq!(document["err"]["code"], expected_code);
    assert!(document.get("ok").is_none());
    document
}

#[test]
fn host_governance_inspect_process_renders_safe_human_and_json_statuses() {
    let human = run_governance_inspect(&["host", "governance", "inspect", "local"]);
    assert!(
        human.output.status.success(),
        "human error: {}",
        utf8(&human.output.stderr)
    );
    let human_stdout = utf8(&human.output.stdout);
    assert!(human.output.stderr.is_empty());
    assert!(human_stdout.contains("host local governance"));
    assert!(human_stdout.contains("host_id:                host_"));
    assert!(human_stdout.contains("approval_key_reference: approval_key_"));
    assert!(human_stdout.contains("relay_id:               relay_"));
    assert!(human_stdout.contains("owner:                  team team_"));
    assert!(human_stdout.contains("owner_revision:         1"));
    assert!(human_stdout.contains("quarantine:             ProjectionConflict"));
    assert_safe_governance_output(&human_stdout);
    assert_governance_inspect_request(&human.requests);

    let json = run_governance_inspect(&["host", "governance", "inspect", "local", "--json"]);
    let status = assert_json_success(&json.output);
    assert_eq!(status["host_id"], governance_identifier("host_"));
    assert_eq!(
        status["enrollment"]["relay_id"],
        governance_identifier("relay_")
    );
    assert_eq!(status["enrollment"]["status"], "quarantined");
    assert_eq!(status["enrollment"]["revision"], "1");
    assert_eq!(
        status["owner"],
        json!({"kind": "team", "id": governance_identifier("team_")})
    );
    assert_eq!(status["owner_revision"], "1");
    assert_eq!(status["quarantine"], "projection_conflict");
    assert_eq!(
        status["approval_key_reference"],
        governance_identifier("approval_key_")
    );
    assert_safe_governance_output(&utf8(&json.output.stdout));
    assert_governance_inspect_request(&json.requests);
}

#[test]
fn host_governance_inspect_process_renders_daemon_error_without_status_payload() {
    let human = run_governance_inspect_error(&["host", "governance", "inspect", "local"]);
    assert_eq!(human.output.status.code(), Some(1));
    assert!(human.output.stdout.is_empty());
    let human_stderr = utf8(&human.output.stderr);
    assert!(human_stderr.contains("safe governance inspection is unavailable"));
    assert_safe_governance_output(&human_stderr);
    assert_governance_inspect_request(&human.requests);

    let json = run_governance_inspect_error(&["host", "governance", "inspect", "local", "--json"]);
    let document = assert_json_error(&json.output, "host_governance_unavailable");
    assert_eq!(
        document["err"]["msg"],
        "safe governance inspection is unavailable"
    );
    assert_safe_governance_output(&utf8(&json.output.stdout));
    assert_governance_inspect_request(&json.requests);
}

struct GovernanceInspectRun {
    output: Output,
    requests: Vec<Request>,
}

fn run_governance_inspect(args: &[&str]) -> GovernanceInspectRun {
    run_governance_inspect_scenario(args, Scenario::GovernanceInspectSuccess)
}

fn run_governance_inspect_error(args: &[&str]) -> GovernanceInspectRun {
    run_governance_inspect_scenario(args, Scenario::GovernanceInspectError)
}

fn run_governance_inspect_scenario(args: &[&str], scenario: Scenario) -> GovernanceInspectRun {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), scenario);
    let (output, requests) = run(&home, fixture, args);
    GovernanceInspectRun { output, requests }
}

fn assert_governance_inspect_request(requests: &[Request]) {
    assert_eq!(
        requests.len(),
        1,
        "governance inspection sends one RPC request"
    );
    let request = requests.first().expect("governance inspection request");
    assert_eq!(request.method(), protocol::method::HOST_GOVERNANCE_INSPECT);
    assert_eq!(request.params(), &Value::Null);
}

fn assert_safe_governance_output(output: &str) {
    for forbidden in [
        "seed",
        "nonce",
        "proposal",
        "signature",
        "session",
        "terminal",
        "path",
    ] {
        assert!(
            !output.to_ascii_lowercase().contains(forbidden),
            "governance output leaked {forbidden}"
        );
    }
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

const PLUGIN_COMMAND_CASES: &[(&[&str], &str)] = &[
    (
        &["session", "new", "--agent", "shell", "--json"],
        protocol::method::SESSION_NEW,
    ),
    (
        &["session", "list", "--json"],
        protocol::method::SESSION_LIST,
    ),
    (
        &["session", "inspect", SESSION_ID, "--json"],
        protocol::method::SESSION_INSPECT,
    ),
    (
        &["session", "fork", SESSION_ID, "--json"],
        protocol::method::SESSION_FORK,
    ),
    (
        &["session", "screen", SESSION_ID, "--json"],
        protocol::method::SESSION_SCREEN,
    ),
    (
        &["session", "screen", "local/s-fixture-1", "--json"],
        protocol::method::SESSION_SCREEN,
    ),
    (
        &["--host", "", "session", "screen", SESSION_ID, "--json"],
        protocol::method::SESSION_SCREEN,
    ),
    (
        &["session", "read", SESSION_ID, "--json"],
        protocol::method::SESSION_READ,
    ),
    (
        &["session", "input", SESSION_ID, "hello", "--json"],
        protocol::method::SESSION_INPUT,
    ),
    (
        &[
            "session",
            "output",
            SESSION_ID,
            "--worker-instance-id",
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
        &["session", "rename", SESSION_ID, "renamed", "--json"],
        protocol::method::SESSION_RENAME,
    ),
    (
        &["session", "runtime-inventory", "--json"],
        protocol::method::SESSION_RUNTIME_INVENTORY,
    ),
];

#[test]
fn plugin_commands_have_binary_level_json_and_wire_parity() {
    for (args, expected_method) in PLUGIN_COMMAND_CASES {
        let home = TestHome::new();
        let fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
        let (output, requests) = run(&home, fixture, args);
        let ok = assert_json_success(&output);
        assert!(!ok.is_null(), "success payload for {expected_method}");
        assert_typed_session_json(expected_method, &ok);
        let command = requests.last().expect("captured command request");
        assert_eq!(command.method(), *expected_method);
        if matches!(
            *expected_method,
            protocol::method::SESSION_NEW | protocol::method::SESSION_RUNTIME_INVENTORY
        ) {
            assert_eq!(requests.len(), 1, "command needs no name resolution");
        } else {
            assert_eq!(
                requests.first().expect("captured list request").method(),
                protocol::method::SESSION_LIST
            );
        }
        if !matches!(
            *expected_method,
            protocol::method::SESSION_NEW
                | protocol::method::SESSION_LIST
                | protocol::method::SESSION_RUNTIME_INVENTORY
        ) {
            // The canned fixture response names `SESSION_ID` whatever the
            // request carried, so only the captured params show the parsed target.
            assert_eq!(requested_session_id(command), SESSION_ID, "{args:?}");
        }
    }
}

/// The session a captured command request names, decoded from the typed
/// params of its method.
fn requested_session_id(request: &Request) -> String {
    fn decode<T: serde::de::DeserializeOwned>(request: &Request) -> T {
        serde_json::from_value(request.params().clone())
            .unwrap_or_else(|error| panic!("{} typed params: {error}", request.method()))
    }
    let session_id = match request.method() {
        protocol::method::SESSION_INSPECT => decode::<protocol::SessionId>(request),
        protocol::method::SESSION_FORK => decode::<protocol::SessionForkParams>(request).session_id,
        protocol::method::SESSION_SCREEN => decode::<protocol::SessionScreenParams>(request)
            .session_id()
            .clone(),
        protocol::method::SESSION_READ => decode::<protocol::SessionReadParams>(request)
            .session_id()
            .clone(),
        protocol::method::SESSION_INPUT => {
            decode::<protocol::SessionInputParams>(request).session_id
        }
        protocol::method::SESSION_OUTPUT => decode::<protocol::SessionOutputParams>(request)
            .session_id()
            .clone(),
        protocol::method::SESSION_WAIT => decode::<protocol::SessionWaitParams>(request)
            .session_id()
            .clone(),
        protocol::method::SESSION_RESUME => decode::<protocol::SessionResumeParams>(request)
            .session_id()
            .clone(),
        protocol::method::SESSION_RESIZE => {
            decode::<protocol::SessionResizeParams>(request).session_id
        }
        protocol::method::SESSION_SET_METADATA => {
            decode::<protocol::SessionSetMetadataParams>(request).session_id
        }
        protocol::method::SESSION_RENAME => {
            decode::<protocol::SessionRenameParams>(request).session_id
        }
        other => panic!("{other} names no single session"),
    };
    session_id.0
}

fn assert_typed_session_json(method: &str, ok: &Value) {
    match method {
        protocol::method::SESSION_NEW
        | protocol::method::SESSION_INSPECT
        | protocol::method::SESSION_FORK => {
            let _: protocol::SessionInfo = serde_json::from_value(ok.clone())
                .unwrap_or_else(|error| panic!("{method} typed JSON: {error}"));
            assert_eq!(ok["runtime"]["runtime_id"], "runtime-fixture-1");
        }
        protocol::method::SESSION_LIST => {
            let _: Vec<protocol::SessionInfo> =
                serde_json::from_value(ok.clone()).expect("typed session list JSON");
            assert_eq!(ok[0]["runtime"]["runtime_id"], "runtime-fixture-1");
        }
        protocol::method::SESSION_WAIT => {
            let _: protocol::SessionWaitResult =
                serde_json::from_value(ok.clone()).expect("typed session wait JSON");
            assert_eq!(ok["session"]["runtime"]["runtime_id"], "runtime-fixture-1");
        }
        protocol::method::SESSION_RESUME
        | protocol::method::SESSION_RESIZE
        | protocol::method::SESSION_SET_METADATA
        | protocol::method::SESSION_RENAME => {
            match method {
                protocol::method::SESSION_RESUME => {
                    let _: protocol::SessionResumeResult =
                        serde_json::from_value(ok.clone()).expect("typed session resume JSON");
                }
                protocol::method::SESSION_RESIZE => {
                    let _: protocol::SessionResizeResult =
                        serde_json::from_value(ok.clone()).expect("typed session resize JSON");
                }
                protocol::method::SESSION_SET_METADATA => {
                    let _: protocol::SessionSetMetadataResult =
                        serde_json::from_value(ok.clone()).expect("typed session metadata JSON");
                }
                protocol::method::SESSION_RENAME => {
                    let _: protocol::SessionRenameResult =
                        serde_json::from_value(ok.clone()).expect("typed session rename JSON");
                }
                _ => unreachable!(),
            }
            assert_eq!(ok["session"]["runtime"]["runtime_id"], "runtime-fixture-1");
        }
        protocol::method::SESSION_SCREEN => {
            let _: protocol::SessionScreenResult =
                serde_json::from_value(ok.clone()).expect("strict typed session screen JSON");
            assert!(ok.get("runtime_id").is_none());
        }
        protocol::method::SESSION_READ => {
            let _: protocol::SessionReadResult =
                serde_json::from_value(ok.clone()).expect("strict typed session read JSON");
            assert!(ok.get("runtime_id").is_none());
        }
        protocol::method::SESSION_OUTPUT => {
            let _: protocol::SessionOutputResult =
                serde_json::from_value(ok.clone()).expect("strict typed session output JSON");
            assert!(ok.get("runtime_id").is_none());
        }
        protocol::method::SESSION_INPUT => {
            let _: protocol::SessionInputResult =
                serde_json::from_value(ok.clone()).expect("strict typed session input JSON");
            assert!(ok["runtime"].get("runtime_id").is_none());
        }
        protocol::method::SESSION_RUNTIME_INVENTORY => {
            let _: protocol::RuntimeInventoryResult =
                serde_json::from_value(ok.clone()).expect("typed runtime inventory JSON");
        }
        method => panic!("unexpected tested method: {method}"),
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
            "--worker-instance-id",
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
        let mut child = SpawnedCli::spawn(home.command().args([
            "session",
            "wait",
            SESSION_ID,
            "--state",
            "done",
            "--timeout-ms",
            "8000",
            "--json",
        ]));
        wait_for_method(&fixture, protocol::method::SESSION_WAIT);
        child.signal(signal, signal_name);
        let output = child.finish(&format!("session wait cancelled by {signal_name}"));
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

#[test]
fn waited_input_opts_in_to_the_prompt_hook_and_reports_which_evidence_answered() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::PromptAcceptedInput);
    let (output, requests) = run(
        &home,
        fixture,
        &[
            "session",
            "input",
            SESSION_ID,
            "go",
            "--until",
            "working",
            "--timeout",
            "2000",
            "--json",
        ],
    );

    let ok = assert_json_success(&output);
    assert_eq!(ok["prompt_accepted"], true);
    assert!(
        ok.get("activity").is_none(),
        "no activity is invented: {ok}"
    );
    let _: protocol::SessionInputResult =
        serde_json::from_value(ok).expect("strict typed session input JSON");
    let sent = requests
        .iter()
        .find(|request| request.method() == protocol::method::SESSION_INPUT)
        .expect("captured input request");
    assert_eq!(sent.params()["wait"]["until"], json!(["working"]));
    assert_eq!(sent.params()["wait"]["allow_prompt_accepted"], true);

    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::PromptAcceptedInput);
    let (output, _requests) = run(
        &home,
        fixture,
        &[
            "session",
            "input",
            SESSION_ID,
            "go",
            "--until",
            "working",
            "--timeout",
            "2000",
        ],
    );
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(
        utf8(&output.stdout),
        format!(
            "session {SESSION_ID}: input accepted submitted=true prompt_accepted=true source=prompt-submit-hook\n"
        )
    );
}

#[test]
fn process_signals_report_unknown_delivery_for_waited_input_without_retry_hint() {
    for (signal, signal_name) in [(libc::SIGINT, "SIGINT"), (libc::SIGTERM, "SIGTERM")] {
        let home = TestHome::new();
        let fixture = FixtureDaemon::start(&home.socket(), Scenario::SlowInput);
        let mut child = SpawnedCli::spawn(home.command().args([
            "session",
            "input",
            SESSION_ID,
            "approval response",
            "--until",
            "idle",
            "--timeout",
            "8000",
            "--json",
        ]));
        wait_for_method(&fixture, protocol::method::SESSION_INPUT);
        child.signal(signal, signal_name);
        let output = child.finish(&format!("session input interrupted by {signal_name}"));
        let requests = fixture.finish();
        let document = assert_json_error(&output, "session_input_interrupted");
        assert!(document["err"].get("recover").is_none());
        assert!(document["err"]["msg"]
            .as_str()
            .expect("typed message")
            .contains("delivery outcome is unknown"));
        assert!(requests
            .iter()
            .any(|request| request.method() == protocol::method::SESSION_INPUT));
    }
}

#[test]
fn fixture_finish_returns_while_an_accepted_connection_is_idle() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    // The connection never sends a request line, so its handler stays blocked
    // reading it until the fixture closes its end.
    let idle = UnixStream::connect(home.socket()).expect("connect idle client");
    pohunek_test_support::wait::poll_until("the fixture to accept the idle client", || {
        (fixture.accepted.load(Ordering::Acquire) > 0).then_some(())
    });
    let requests = fixture.finish();
    assert!(requests.is_empty());
    drop(idle);
}

fn wait_for_method(fixture: &FixtureDaemon, method: &str) {
    pohunek_test_support::wait::poll_until(&format!("the fixture to receive {method}"), || {
        fixture
            .requests()
            .iter()
            .any(|request| request.method() == method)
            .then_some(())
    });
}

/// A spawned CLI child that is killed and reaped when the test unwinds.
struct SpawnedCli {
    child: Child,
    stdout: Option<thread::JoinHandle<Vec<u8>>>,
    stderr: Option<thread::JoinHandle<Vec<u8>>>,
}

impl SpawnedCli {
    /// Spawns `command` with piped stdout and stderr drained on helper threads.
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pohunek");
        let stdout = drain(child.stdout.take().expect("piped stdout"));
        let stderr = drain(child.stderr.take().expect("piped stderr"));
        Self {
            child,
            stdout: Some(stdout),
            stderr: Some(stderr),
        }
    }

    fn signal(&mut self, signal: libc::c_int, signal_name: &str) {
        send_signal(&mut self.child, signal, signal_name);
    }

    /// Waits for the child to exit and returns its output, failing with a named
    /// step if the child or its pipes stay open past the hang guard.
    fn finish(mut self, what: &str) -> Output {
        let status =
            pohunek_test_support::wait::poll_until(&format!("the CLI to exit: {what}"), || {
                self.child.try_wait().expect("poll CLI exit")
            });
        Output {
            status,
            stdout: join_drain(self.stdout.take().expect("stdout reader"), what),
            stderr: join_drain(self.stderr.take().expect("stderr reader"), what),
        }
    }
}

impl Drop for SpawnedCli {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _killed = self.child.kill();
            let _reaped = self.child.wait();
        }
    }
}

fn drain(mut pipe: impl std::io::Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _read = pipe.read_to_end(&mut bytes);
        bytes
    })
}

fn join_drain(reader: thread::JoinHandle<Vec<u8>>, what: &str) -> Vec<u8> {
    pohunek_test_support::wait::poll_until(&format!("the CLI pipes to close: {what}"), || {
        reader.is_finished().then_some(())
    });
    reader.join().expect("CLI pipe reader")
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
    let (connect_redirect, connect_capture) = connect_redirect::build(&home.root);
    let bin = home.root.join("bin");
    fs::create_dir_all(&bin).expect("create fixture bin");
    let netbird = bin.join("netbird");
    pohunek_test_support::fs::write_file(
        &netbird,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{{\"peers\":{{\"details\":[{{\"fqdn\":\"fixture-remote.netbird.test\",\"netbirdIp\":\"{netbird_ip}\",\"publicKey\":\"a/real+netbird@key==\",\"status\":\"Connected\"}}]}}}}'\n"
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
    for selector in ["fixture-remote", "FIXTURE-REMOTE.NETBIRD.TEST", netbird_ip] {
        let output = home
            .command()
            .env("PATH", &path)
            .env("LD_PRELOAD", &connect_redirect)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
            .env("POHUNEK_REMOTE_PORT", remote_port.to_string())
            .env(protocol::ENV_SESSION_ID, private_session)
            .env(protocol::ENV_DAEMON_ID, private_daemon)
            .args([
                "--host", selector, "session", "screen", SESSION_ID, "--json",
            ])
            .output()
            .expect("run remote pohunek");
        let ok = assert_json_success(&output);
        assert_eq!(ok["session_id"], SESSION_ID, "selector {selector}");
    }
    let peer = pohunek_client::ExternalIdentity::peer_id("a/real+netbird@key==")
        .expect("stable peer identity")
        .selector();
    let route = pohunek_client::remote_host_with_port(&format!("netbird:{peer}"), remote_port)
        .expect("canonical remote route");
    let target = format!("{route}/{SESSION_ID}");
    let output = home
        .command()
        .env("PATH", &path)
        .env("LD_PRELOAD", &connect_redirect)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
        .env("POHUNEK_REMOTE_PORT", remote_port.to_string())
        .env(protocol::ENV_SESSION_ID, private_session)
        .env(protocol::ENV_DAEMON_ID, private_daemon)
        .args(["session", "screen", &target, "--json"])
        .output()
        .expect("run CLI with a provider-qualified target");
    let ok = assert_json_success(&output);
    assert_eq!(ok["session_id"], SESSION_ID);

    let requests = tcp_fixture.finish();
    let local_requests = local_fixture.finish();
    assert!(requests.len() >= 8, "every selector reaches TCP fixture");
    let screens = requests
        .iter()
        .filter(|request| request.method() == protocol::method::SESSION_SCREEN)
        .map(requested_session_id)
        .collect::<Vec<_>>();
    assert_eq!(
        screens, [SESSION_ID; 4],
        "every selector and the provider-qualified target name the session"
    );
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
            >= 8,
        "every CLI connection must target the fake NetBird address before the test-only redirect"
    );
}

/// This machine's own `NetBird` name in the self-name scenarios' fake status.
#[cfg(target_os = "linux")]
const SELF_FQDN: &str = "fixture-self.netbird.test";
/// This machine's own `NetBird` IP in the self-name scenarios' fake status.
#[cfg(target_os = "linux")]
const SELF_NETBIRD_IP: &str = "100.64.0.7";

/// A fake `netbird` whose status names this machine at the root, with one
/// peer that shares its short name on another domain, so the bare short name
/// is ambiguous. Every run appends a line to the call log.
#[cfg(target_os = "linux")]
struct SelfNetbird {
    path: std::ffi::OsString,
    program: PathBuf,
    calls: PathBuf,
}

/// How long a stalled fake `netbird` runs: far beyond the client's 5 s connect
/// timeout, so only a lookup bounded by that timeout finishes before it.
#[cfg(target_os = "linux")]
const STALLED_NETBIRD: Duration = Duration::from_secs(300);

#[cfg(target_os = "linux")]
impl SelfNetbird {
    fn install(root: &Path) -> Self {
        let bin = root.join("self-netbird-bin");
        fs::create_dir_all(&bin).expect("create fixture bin");
        let netbird = bin.join("netbird");
        let calls = root.join("netbird-calls");
        pohunek_test_support::fs::write_file(
            &netbird,
            format!(
                "#!/bin/sh\necho call >> '{calls}'\nprintf '%s\\n' '{{\"fqdn\":\"{SELF_FQDN}\",\"netbirdIp\":\"{SELF_NETBIRD_IP}\",\"peers\":{{\"details\":[{{\"fqdn\":\"fixture-remote.netbird.test\",\"netbirdIp\":\"100.64.0.42\",\"publicKey\":\"remote-key\",\"status\":\"Connected\"}},{{\"fqdn\":\"fixture-self.lan.test\",\"netbirdIp\":\"100.64.0.43\",\"publicKey\":\"twin-key\",\"status\":\"Connected\"}}]}}}}'\n",
                calls = calls.display(),
            ),
        )
        .expect("write fake netbird");
        fs::set_permissions(&netbird, fs::Permissions::from_mode(0o700))
            .expect("make fake netbird executable");
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&inherited_path));
        let path = std::env::join_paths(paths).expect("join PATH");
        Self {
            path,
            program: netbird,
            calls,
        }
    }

    /// Makes every later `netbird` run hang for [`STALLED_NETBIRD`].
    fn stall(&self) {
        pohunek_test_support::fs::write_file(
            &self.program,
            format!("#!/bin/sh\nexec sleep {}\n", STALLED_NETBIRD.as_secs()),
        )
        .expect("write stalled netbird");
        fs::set_permissions(&self.program, fs::Permissions::from_mode(0o700))
            .expect("make stalled netbird executable");
    }

    fn call_count(&self) -> usize {
        fs::read_to_string(&self.calls).map_or(0, |calls| calls.lines().count())
    }
}

#[test]
#[cfg(target_os = "linux")]
fn the_machines_own_overlay_name_selects_the_local_daemon() {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let tcp_fixture = TcpFixtureDaemon::start(Ipv4Addr::LOCALHOST, Scenario::Success);
    let remote_port = tcp_fixture.address.port();
    let (connect_redirect, connect_capture) = connect_redirect::build(&home.root);
    let netbird = SelfNetbird::install(&home.root);
    let run = |args: &[&str]| {
        home.command()
            .env("PATH", &netbird.path)
            .env("LD_PRELOAD", &connect_redirect)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
            .env("POHUNEK_REMOTE_PORT", remote_port.to_string())
            .args(args)
            .output()
            .expect("run pohunek")
    };
    let screen =
        |host: &str, target: &str| run(&["--host", host, "session", "screen", target, "--json"]);

    // Literal `local` never consults the overlay.
    let ok = assert_json_success(&screen("local", SESSION_ID));
    assert_eq!(ok["session_id"], SESSION_ID);
    assert_eq!(netbird.call_count(), 0, "`local` must not run netbird");

    // The own fqdn, its upper-case spelling, the own IP and a target host all
    // reach the local daemon.
    let self_target = format!("{SELF_FQDN}/{SESSION_ID}");
    for output in [
        screen(SELF_FQDN, SESSION_ID),
        screen("FIXTURE-SELF.NETBIRD.TEST", SESSION_ID),
        screen(SELF_NETBIRD_IP, SESSION_ID),
        run(&["session", "screen", &self_target, "--json"]),
    ] {
        let ok = assert_json_success(&output);
        assert_eq!(ok["session_id"], SESSION_ID);
    }

    // A short name this machine shares with a peer is ambiguous, never a
    // silent choice; an unknown name keeps its typed error.
    let ambiguous = assert_json_error(
        &screen("fixture-self", SESSION_ID),
        "overlay_host_ambiguous",
    );
    assert_eq!(
        ambiguous["err"]["msg"],
        "host 'fixture-self' matches 2 hosts on overlay netbird: \
         this machine (fixture-self.netbird.test), peer fixture-self.lan.test"
    );
    let recover = ambiguous["err"]["recover"].as_str().expect("recover text");
    assert!(
        recover.contains(": local, ") && recover.contains("netbird:peer~"),
        "{recover}"
    );
    assert_json_error(&screen("fixture-nobody", SESSION_ID), "host_unknown");
    // An explicit target host overrides that `--host`, so it is never resolved.
    let ok = assert_json_success(&screen("fixture-self", &format!("local/{SESSION_ID}")));
    assert_eq!(ok["session_id"], SESSION_ID);

    // A peer is still dialed over the overlay.
    let ok = assert_json_success(&screen("fixture-remote", SESSION_ID));
    assert_eq!(ok["session_id"], SESSION_ID);

    let local_screens = local_fixture
        .finish()
        .iter()
        .filter(|request| request.method() == protocol::method::SESSION_SCREEN)
        .count();
    assert_eq!(
        local_screens, 6,
        "`local`, every own-name selector and the overriding target use the local socket"
    );
    let remote_screens = tcp_fixture
        .finish()
        .iter()
        .filter(|request| request.method() == protocol::method::SESSION_SCREEN)
        .count();
    assert_eq!(
        remote_screens, 1,
        "only the peer selector dials the overlay"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn host_commands_treat_the_machines_own_overlay_name_as_local() {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::GovernanceInspectSuccess);
    let tcp_fixture = TcpFixtureDaemon::start(Ipv4Addr::LOCALHOST, Scenario::Success);
    let remote_port = tcp_fixture.address.port();
    let (connect_redirect, connect_capture) = connect_redirect::build(&home.root);
    let netbird = SelfNetbird::install(&home.root);
    let run = |args: &[&str]| {
        home.command()
            .env("PATH", &netbird.path)
            .env("LD_PRELOAD", &connect_redirect)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
            .env("POHUNEK_REMOTE_PORT", remote_port.to_string())
            .args(args)
            .output()
            .expect("run pohunek")
    };

    // A positional host that names this machine inspects the local daemon.
    assert_json_success(&run(&[
        "host",
        "governance",
        "inspect",
        SELF_FQDN,
        "--json",
    ]));

    // A command that ignores `--host` never resolves it.
    let calls = netbird.call_count();
    let hermes = run(&[
        "--host",
        "fixture-self",
        "integration",
        "status",
        "--agent",
        "hermes",
        "--json",
    ]);
    assert!(
        !utf8(&hermes.stdout).contains("overlay_host_ambiguous"),
        "{}",
        utf8(&hermes.stdout)
    );
    assert_eq!(
        netbird.call_count(),
        calls,
        "the ignored `--host` ran netbird"
    );

    // `host list` ignores `--host`, so an ambiguous one does not stop it. It
    // shows this machine first, named by its own overlay name and routed as
    // `local`.
    let listed = run(&["--host", "fixture-self", "host", "list", "--refresh"]);
    assert!(
        listed.status.success(),
        "host list: {}",
        utf8(&listed.stderr)
    );
    let table = utf8(&listed.stdout);
    let first_row = table.lines().nth(1).expect("a row below the header");
    assert_eq!(
        first_row.split_whitespace().take(3).collect::<Vec<_>>(),
        ["fixture-self", "local", "local"],
        "{table}"
    );

    // With a stuck overlay CLI, the cached records, this machine's included,
    // print without waiting for it.
    netbird.stall();
    let started = std::time::Instant::now();
    let listed = run(&["host", "list"]);
    assert!(
        started.elapsed() < STALLED_NETBIRD,
        "host list waited for the stalled netbird"
    );
    assert!(
        listed.status.success(),
        "host list: {}",
        utf8(&listed.stderr)
    );
    let table = utf8(&listed.stdout);
    assert!(
        table.lines().any(|row| row.split_whitespace().take(3).eq([
            "fixture-self",
            "local",
            "local"
        ])),
        "{table}"
    );
    assert!(
        table.lines().any(|row| row.starts_with("fixture-remote")),
        "{table}"
    );

    let local_inspections = local_fixture
        .finish()
        .iter()
        .filter(|request| request.method() == protocol::method::HOST_GOVERNANCE_INSPECT)
        .count();
    assert_eq!(
        local_inspections, 1,
        "the own name inspects the local daemon"
    );
    drop(tcp_fixture.finish());
}

#[test]
#[cfg(target_os = "linux")]
fn stable_netbird_peer_identity_follows_its_new_address_but_not_a_reassigned_one() {
    let home = TestHome::new();
    let tcp_fixture = TcpFixtureDaemon::start(Ipv4Addr::LOCALHOST, Scenario::Success);
    let remote_port = tcp_fixture.address.port();
    let (connect_redirect, connect_capture) = connect_redirect::build(&home.root);
    let bin = home.root.join("identity-netbird-bin");
    fs::create_dir_all(&bin).expect("create NetBird fixture bin");
    let netbird = bin.join("netbird");
    let selector = pohunek_client::ExternalIdentity::peer_id("stable/key+=")
        .expect("stable peer identity")
        .selector();
    let host = format!("netbird:{selector}");

    let write_status = |peer_id: &str, address: &str| {
        pohunek_test_support::fs::write_executable(
            &netbird,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{{\"peers\":[{{\"publicKey\":\"{peer_id}\",\"fqdn\":\"remote.example\",\"netbirdIp\":\"{address}\"}}]}}'\n"
            ),
        )
        .expect("write NetBird status fixture");
    };
    let run = || {
        home.command()
            .env("PATH", &bin)
            .env("LD_PRELOAD", &connect_redirect)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
            .env("POHUNEK_REMOTE_PORT", remote_port.to_string())
            .args(["--host", &host, "session", "screen", SESSION_ID, "--json"])
            .output()
            .expect("run remote CLI")
    };

    for address in ["100.64.0.42", "100.64.0.43"] {
        write_status("stable/key+=", address);
        let output = run();
        assert_json_success(&output);
        let captured = fs::read_to_string(&connect_capture).expect("read connect targets");
        assert!(
            captured
                .lines()
                .any(|line| line == format!("{address}:{remote_port}")),
            "stable identity must route to its current address: {captured}"
        );
    }

    let before = fs::read_to_string(&connect_capture).expect("read previous connect targets");
    write_status("different-key", "100.64.0.42");
    let rejected = run();
    assert_json_error(&rejected, "host_unknown");
    assert_eq!(
        fs::read_to_string(&connect_capture).expect("read connect targets after reassignment"),
        before,
        "the old peer identity must not follow an address reassigned to another peer"
    );
    assert!(!tcp_fixture.finish().is_empty());
}

/// Installs a scripted `netbird` binary whose real `status` probe reports a
/// single connected peer named `fixture-remote.netbird.test` at `netbird_ip`.
/// The script records each invocation in `calls`, so a test can prove at the
/// process boundary whether the CLI ever probed the provider. The child's
/// `PATH` contains only the fixture binary directory, so a host `netbird`
/// cannot interfere with the evidence.
#[cfg(target_os = "linux")]
fn install_fake_netbird(command: &mut Command, home: &TestHome, netbird_ip: &str, calls: &Path) {
    let bin = home.root.join(NETBIRD_FIXTURE_PREFIX);
    fs::create_dir_all(&bin).expect("create fixture netbird bin");
    let netbird = bin.join("netbird");
    pohunek_test_support::fs::write_file(
        &netbird,
        format!(
            "#!/bin/sh\nprintf 'invoked\\n' >> \"$POHUNEK_TEST_NETBIRD_CALLS\"\nprintf '%s\\n' '{{\"peers\":{{\"details\":[{{\"fqdn\":\"fixture-remote.netbird.test\",\"netbirdIp\":\"{netbird_ip}\",\"status\":\"Connected\"}}]}}}}'\n"
        ),
    )
    .expect("write fake netbird");
    fs::set_permissions(&netbird, fs::Permissions::from_mode(0o700))
        .expect("make fake netbird executable");
    command.env("POHUNEK_TEST_NETBIRD_CALLS", calls);
    command.env("PATH", &bin);
}

/// Directory below the test home where the fixture `netbird` lives.
#[cfg(target_os = "linux")]
const NETBIRD_FIXTURE_PREFIX: &str = "netbird-fixture-bin";

/// Arguments that address a session through a remote host, in both modes.
#[cfg(target_os = "linux")]
fn remote_screen_args(json: bool) -> Vec<&'static str> {
    if json {
        vec![
            "--host",
            "fixture-remote",
            "session",
            "screen",
            SESSION_ID,
            "--json",
        ]
    } else {
        vec!["--host", "fixture-remote", "session", "screen", SESSION_ID]
    }
}

#[test]
#[cfg(target_os = "linux")]
fn remote_port_override_reaches_the_connect_target() {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let (connect_redirect, connect_capture) = connect_redirect::build(&home.root);
    let calls = home.root.join("netbird-calls-valid.log");
    // One real CLI process per configured value; the capture log records the
    // raw `connect()` target after provider resolution and before redirect,
    // so each distinct value proves the override reached the transport.
    for (configured, expected_port) in [(" 9000 ", "9000"), ("65535", "65535"), ("1", "1")] {
        let mut command = home.command();
        install_fake_netbird(&mut command, &home, "100.64.0.42", &calls);
        command
            .env("LD_PRELOAD", &connect_redirect)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
            .env("POHUNEK_REMOTE_PORT", configured)
            .args(remote_screen_args(true));
        let _output = command.output().expect("run remote pohunek");
        // The provider-targeted port cannot be a listener in the test
        // environment, so the process may exit nonzero; what the scenario
        // pins is only that the configured value reached the real syscalls.
        let captured = fs::read_to_string(&connect_capture).unwrap_or_default();
        assert!(
            captured
                .lines()
                .any(|line| line == format!("100.64.0.42:{expected_port}")),
            "configured POHUNEK_REMOTE_PORT={configured:?} must reach connect: capture\n{captured}"
        );
    }
    local_fixture.finish();
}

#[test]
#[cfg(target_os = "linux")]
fn remote_port_default_is_used_when_the_override_is_unset() {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let (connect_redirect, connect_capture) = connect_redirect::build(&home.root);
    let calls = home.root.join("netbird-calls-default.log");
    let mut command = home.command();
    install_fake_netbird(&mut command, &home, "100.64.0.42", &calls);
    command
        .env("LD_PRELOAD", connect_redirect)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
        .args(remote_screen_args(true));
    let _output = command.output().expect("run remote pohunek");
    let captured = fs::read_to_string(&connect_capture).unwrap_or_default();
    assert!(
        captured
            .lines()
            .any(|line| line == format!("100.64.0.42:{}", netbird::DEFAULT_REMOTE_PORT)),
        "without POHUNEK_REMOTE_PORT the CLI must dial the default remote port: capture\n{captured}"
    );
    local_fixture.finish();
}

#[test]
#[cfg(target_os = "linux")]
fn invalid_remote_port_fails_fast_before_probing_the_provider() {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let calls = home.root.join("netbird-calls-invalid.log");
    for invalid in ["", "   ", "abc", "0", "1.5", "65536", "99999"] {
        let mut command = home.command();
        install_fake_netbird(&mut command, &home, "100.64.0.42", &calls);
        command
            .env("POHUNEK_REMOTE_PORT", invalid)
            .args(remote_screen_args(true));
        let output = command.output().expect("run JSON pohunek with a bad port");
        let document = assert_json_error(&output, "netbird_configuration_invalid");
        let message = document["err"]["msg"].as_str().expect("error message");
        assert!(
            message.contains(netbird::REMOTE_PORT_ENV),
            "the message must name the misconfigured variable: {message}"
        );
        assert!(
            message.contains("expected a port number in 1..=65535"),
            "the message must state the accepted range: {message}"
        );
    }
    // Fail fast means the provider was never consulted and nothing dialed.
    assert!(
        !calls.exists(),
        "an invalid port must fail before the netbird probe"
    );
    local_fixture.finish();
}

#[test]
#[cfg(target_os = "linux")]
fn invalid_remote_port_fails_loudly_in_human_mode() {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let calls = home.root.join("netbird-calls-human.log");
    let mut command = home.command();
    install_fake_netbird(&mut command, &home, "100.64.0.42", &calls);
    command
        .env("POHUNEK_REMOTE_PORT", "99999")
        .args(remote_screen_args(false));
    let output = command.output().expect("run human-mode pohunek");
    assert!(
        !output.status.success(),
        "a misconfigured port must not exit zero"
    );
    let combined = format!("{}{}", utf8(&output.stdout), utf8(&output.stderr));
    assert!(
        combined.contains(netbird::REMOTE_PORT_ENV),
        "the human failure must name POHUNEK_REMOTE_PORT: {combined}"
    );
    assert!(
        !calls.exists(),
        "an invalid port must fail before the netbird probe"
    );
    local_fixture.finish();
}

#[test]
#[cfg(target_os = "linux")]
fn non_unicode_remote_port_value_is_a_user_visible_failure() {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    // An environment value that is raw bytes, not UTF-8, must surface as a
    // configuration error rather than crash the lookup.
    let raw_value = std::ffi::OsString::from_vec(vec![0xff, 0xfe]);
    let mut command = home.command();
    command
        .env(netbird::REMOTE_PORT_ENV, raw_value)
        .args(remote_screen_args(true));
    let output = command.output().expect("run JSON pohunek");
    let document = assert_json_error(&output, "netbird_configuration_invalid");
    let message = document["err"]["msg"].as_str().expect("error message");
    assert!(
        message.contains("value is not valid Unicode"),
        "the message must explain the non-Unicode value: {message}"
    );
    local_fixture.finish();
}

#[test]
fn session_retention_sweep_exits_zero_when_nothing_failed() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::RetentionSweepClean);
    let (output, requests) = run(
        &home,
        fixture,
        &["session", "retention", "sweep", "--apply"],
    );

    assert!(
        output.status.success(),
        "a sweep with no failure exits zero: {}",
        utf8(&output.stderr)
    );
    assert_eq!(
        requests.last().expect("sweep request").method(),
        protocol::method::SESSION_RETENTION_SWEEP
    );
}

#[test]
fn session_retention_sweep_exits_nonzero_on_a_partial_failure() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::RetentionSweepPartialFailure);
    let (output, _requests) = run(
        &home,
        fixture,
        &["session", "retention", "sweep", "--apply"],
    );

    assert!(
        !output.status.success(),
        "a cron job must see a partial failure in the exit status: {}",
        utf8(&output.stdout)
    );
    // The counters still reach stdout so the operator learns what went wrong.
    let stdout = utf8(&output.stdout);
    assert!(stdout.contains("failed=1"), "{stdout}");
    assert!(stdout.contains("worktrees_failed=1"), "{stdout}");
}

fn assert_cli_usage_error(args: &[&str], message: &str) {
    let home = TestHome::new();
    let output = home.command().args(args).output().expect("run pohunek");
    assert_eq!(output.status.code(), Some(2), "{}", utf8(&output.stderr));
    assert!(output.stdout.is_empty(), "a usage error has no stdout");
    assert!(
        utf8(&output.stderr).contains(message),
        "{}",
        utf8(&output.stderr)
    );
    assert!(
        !home.socket().exists(),
        "argument validation must not start or contact a daemon"
    );
}

#[test]
fn session_policy_set_requires_a_field_on_the_cli() {
    assert_cli_usage_error(
        &["session", "policy", "set"],
        "required arguments were not provided",
    );
}

#[test]
fn session_policy_set_rejects_conflicting_flags_on_the_cli() {
    assert_cli_usage_error(
        &["session", "policy", "set", "--enabled", "--disabled"],
        "cannot be used with",
    );
}

#[test]
fn session_retention_sweep_requires_an_explicit_mode_on_the_cli() {
    assert_cli_usage_error(
        &["session", "retention", "sweep"],
        "required arguments were not provided",
    );
    assert_cli_usage_error(
        &["session", "retention", "sweep", "--dry-run", "--apply"],
        "cannot be used with",
    );
}

#[test]
fn notification_retention_prune_requires_an_explicit_mode_on_the_cli() {
    assert_cli_usage_error(
        &["notifications", "retention", "prune"],
        "required arguments were not provided",
    );
}

/// The request line of an assistant opening prompt.
fn assistant_request_line(request: &Request) -> String {
    request.params()["input"]
        .as_str()
        .expect("the opening prompt is the initial input")
        .lines()
        .find(|line| line.starts_with("request: "))
        .expect("the opening prompt names the request")
        .to_owned()
}

/// Runs `pohunek` in `home` with `stdin` written to its standard input.
fn run_piping_stdin(home: &TestHome, args: &[&str], stdin: &str) -> Output {
    let mut child = home
        .command()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pohunek");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin.as_bytes())
        .expect("write the request to stdin");
    child.wait_with_output().expect("wait for pohunek")
}

/// Assistant flags shared by the request-form scenarios.
const ASSISTANT_ARGS: [&str; 7] = [
    "assistant",
    "--agent",
    "codex",
    "--no-snapshot",
    "--meta",
    "launch=req-7f3a",
    "--json",
];

/// The request words of the request-form scenarios.
const ASSISTANT_REQUEST: [&str; 4] = ["review", "the", "failing", "test"];

#[test]
fn an_assistant_request_from_stdin_launches_like_the_positional_form() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);

    let positional = home
        .command()
        .args(ASSISTANT_ARGS)
        .args(ASSISTANT_REQUEST)
        .output()
        .expect("run pohunek");
    let stdin_args = [ASSISTANT_ARGS.as_slice(), &["--request-stdin"]].concat();
    assert!(
        stdin_args
            .iter()
            .all(|arg| !ASSISTANT_REQUEST.contains(arg)),
        "the stdin form keeps the request out of argv"
    );
    let from_stdin = run_piping_stdin(
        &home,
        &stdin_args,
        &format!("\n  {}  \n", ASSISTANT_REQUEST.join(" ")),
    );

    let created = assert_json_success(&positional);
    assert_eq!(created["session"]["id"], SESSION_ID, "{created}");
    assert_eq!(assert_json_success(&from_stdin), created);
    let creations = fixture
        .finish()
        .into_iter()
        .filter(|request| request.method() == protocol::method::SESSION_NEW)
        .collect::<Vec<_>>();
    assert_eq!(creations.len(), 2, "one creation per launch");
    for creation in &creations {
        assert_eq!(
            assistant_request_line(creation),
            "request: review the failing test"
        );
        assert_eq!(creation.params()["metadata"], json!({"launch": "req-7f3a"}));
    }
}

#[test]
fn an_unusable_assistant_stdin_request_is_refused_before_any_daemon_request() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let stdin_args = [ASSISTANT_ARGS.as_slice(), &["--request-stdin"]].concat();

    let empty = assert_json_error(
        &run_piping_stdin(&home, &stdin_args, " \n\t\n"),
        "cli_usage",
    );
    assert!(
        empty["err"]["msg"]
            .as_str()
            .expect("typed message")
            .contains("the assistant request is empty"),
        "{empty}"
    );
    assert_json_error(
        &run_piping_stdin(&home, &stdin_args, "stop\u{7}"),
        "cli_usage",
    );
    let both = run_piping_stdin(
        &home,
        &[stdin_args.as_slice(), &["review"]].concat(),
        "review",
    );
    assert_eq!(both.status.code(), Some(2), "{both:?}");
    // Stdin holds the request, so a remote launch cannot prompt and needs
    // `--yes` even on the human path.
    let remote = run_piping_stdin(
        &home,
        &[
            "--host",
            "fixture-remote",
            "assistant",
            "--project",
            "ui",
            "--request-stdin",
        ],
        "review",
    );
    assert_eq!(remote.status.code(), Some(1), "{remote:?}");
    assert!(
        utf8(&remote.stderr).contains("--yes"),
        "{}",
        utf8(&remote.stderr)
    );

    assert!(
        fixture.finish().is_empty(),
        "a refused request reaches no daemon"
    );
}

#[test]
fn a_remote_session_new_from_stdin_without_yes_is_refused_before_any_daemon_request() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let remote_args = [
        "--host",
        "fixture-remote",
        "session",
        "new",
        "--agent",
        "codex",
        "--project",
        "ui",
        "--input-stdin",
    ];

    // Stdin holds the input, so the prompt would read EOF: both the machine and
    // the human path fail with `confirmation_required` instead of a decline.
    let machine = run_piping_stdin(
        &home,
        &[remote_args.as_slice(), &["--json"]].concat(),
        "review",
    );
    let document = assert_json_error(&machine, "confirmation_required");
    assert_eq!(document["err"]["class"], "configuration", "{document}");
    let human = run_piping_stdin(&home, &remote_args, "review");
    assert_eq!(human.status.code(), Some(1), "{human:?}");
    assert!(
        utf8(&human.stderr).contains("--yes"),
        "{}",
        utf8(&human.stderr)
    );
    assert!(
        !utf8(&human.stderr).contains("was not confirmed"),
        "{}",
        utf8(&human.stderr)
    );

    assert!(
        fixture.finish().is_empty(),
        "a refused request reaches no daemon"
    );
}

/// Creation deadline of the unanswered-creation scenario: the fixture never
/// replies, so any deadline expires, and a short one keeps the scenario fast.
const UNANSWERED_CREATION_DEADLINE_MS: &str = "50";

/// The caller protocol of "Creation outcome" in `docs/public-api.md`: after an
/// unknown outcome, look the launch key up and retry only when nothing matches.
#[test]
fn an_assistant_launch_with_an_unknown_outcome_is_found_by_its_key_instead_of_retried() {
    let home = TestHome::new();
    let fixture = FixtureDaemon::start(&home.socket(), Scenario::NoReplyToNew);
    let launch = || {
        run_piping_stdin(
            &home,
            &[
                ASSISTANT_ARGS.as_slice(),
                &[
                    "--request-stdin",
                    "--request-timeout-ms",
                    UNANSWERED_CREATION_DEADLINE_MS,
                ],
            ]
            .concat(),
            "review the failing test",
        )
    };
    let lookup = |key: &str| {
        let output = home
            .command()
            .args(["session", "list", "--meta-eq", key, "--json"])
            .output()
            .expect("run pohunek");
        assert_json_success(&output)
    };

    assert_json_error(&launch(), "request_timeout");
    // The client may give up before the fixture has read the request.
    wait_for_method(&fixture, protocol::method::SESSION_NEW);
    assert_eq!(
        lookup("launch=another-launch"),
        json!([]),
        "the lookup matches the key, not any session"
    );
    let found = lookup(&format!("{}={}", LAUNCH_KEY.0, LAUNCH_KEY.1));
    let found = found.as_array().expect("a session list");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["id"], SESSION_ID);
    assert_eq!(found[0]["metadata"][LAUNCH_KEY.0], LAUNCH_KEY.1);
    // The caller retries only when nothing matches. The launch is found, so
    // no second creation is sent.

    let creations = fixture
        .finish()
        .into_iter()
        .filter(|request| request.method() == protocol::method::SESSION_NEW)
        .collect::<Vec<_>>();
    assert_eq!(creations.len(), 1, "one creation: the CLI never resends it");
    assert_eq!(
        creations[0].params()["metadata"],
        json!({LAUNCH_KEY.0: LAUNCH_KEY.1}),
        "the caller's key travels with the one creation"
    );
}

/// Peer the name `peer-a` selects before the first connection of an operation.
#[cfg(target_os = "linux")]
const PIN_FIRST_IP: &str = "100.64.0.11";

/// Peer the name `peer-a` selects once the first connection was dialed.
#[cfg(target_os = "linux")]
const PIN_SECOND_IP: &str = "100.64.0.12";

/// Address the first peer moves to in the address-change scenario.
#[cfg(target_os = "linux")]
const PIN_MOVED_IP: &str = "100.64.0.13";

/// Remote port the environment configures when a scenario names the port in
/// the route; no daemon listens there, so a pin that loses the route's port
/// fails to connect.
#[cfg(target_os = "linux")]
const PIN_WRONG_PORT: &str = "1";

/// `after` table of [`run_pinned`] that makes every lookup after the first
/// dialed connection hang.
#[cfg(target_os = "linux")]
const PIN_STALL: &str = "stall";

/// How long a stalled fake `netbird` hangs: beyond the client's 5 s connect
/// timeout, so only a bounded lookup finishes.
#[cfg(target_os = "linux")]
const PIN_STALL_SECONDS: u32 = 30;

/// Placeholder of the fixture daemon's port in a host selector template.
#[cfg(target_os = "linux")]
const PORT_PLACEHOLDER: &str = "{port}";

#[cfg(target_os = "linux")]
fn pin_first_peer(fqdn: &'static str, ip: &'static str) -> pin_overlay::Peer {
    pin_overlay::Peer::new(fqdn, ip, "key-first")
}

#[cfg(target_os = "linux")]
fn pin_second_peer(fqdn: &'static str) -> pin_overlay::Peer {
    pin_overlay::Peer::new(fqdn, PIN_SECOND_IP, "key-second")
}

/// The table before the first connection: `peer-a` is the first peer.
#[cfg(target_os = "linux")]
fn pin_before() -> String {
    pin_overlay::status(
        None,
        &[
            pin_first_peer("peer-a.netbird.test", PIN_FIRST_IP),
            pin_second_peer("peer-y.netbird.test"),
        ],
    )
}

/// The table after the first connection: the name `peer-a` moved to the
/// second peer, and the first peer was renamed.
#[cfg(target_os = "linux")]
fn pin_after_rename() -> String {
    pin_overlay::status(
        None,
        &[
            pin_first_peer("peer-x-old.netbird.test", PIN_FIRST_IP),
            pin_second_peer("peer-a.netbird.test"),
        ],
    )
}

/// What an operation observed: the CLI output, the `NetBird` addresses it
/// dialed in order, and the requests the fixture daemon served.
#[cfg(target_os = "linux")]
struct PinnedRun {
    output: Output,
    dialed: Vec<String>,
    requests: Vec<Request>,
}

/// Runs `pohunek --host <host> <args>` against a fake `NetBird` whose table
/// changes from `before` to `after` once the first connection was dialed.
/// `host` may contain [`PORT_PLACEHOLDER`]; `env_port` is the configured
/// remote port, the fixture daemon's own port when `None`.
#[cfg(target_os = "linux")]
fn run_pinned(
    host: &str,
    args: &[&str],
    before: &str,
    after: &str,
    env_port: Option<&str>,
) -> PinnedRun {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let tcp_fixture = TcpFixtureDaemon::start(Ipv4Addr::LOCALHOST, Scenario::Success);
    let fixture_port = tcp_fixture.address.port().to_string();
    let (connect_redirect, connect_capture) = connect_redirect::build(&home.root);
    let bin = home.root.join("bin");
    let path = if after == PIN_STALL {
        pin_overlay::install_stalling(&bin, &connect_capture, before, PIN_STALL_SECONDS)
    } else {
        pin_overlay::install(&bin, &connect_capture, before, after)
    };
    let output = home
        .command()
        .env("PATH", &path)
        .env("LD_PRELOAD", &connect_redirect)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
        .env("POHUNEK_REMOTE_PORT", env_port.unwrap_or(&fixture_port))
        .arg("--host")
        .arg(host.replace(PORT_PLACEHOLDER, &fixture_port))
        .args(args)
        .output()
        .expect("run pinned pohunek");
    let requests = tcp_fixture.finish();
    assert!(
        local_fixture.finish().is_empty(),
        "a remote operation never uses the local socket"
    );
    PinnedRun {
        output,
        dialed: pin_overlay::dialed_ips(&connect_capture),
        requests,
    }
}

/// Commands that resolve the session on one connection and act on another.
#[cfg(target_os = "linux")]
const PINNED_SESSION_COMMANDS: [&[&str]; 2] = [
    &["session", "output", SESSION_ID, "--json"],
    &[
        "session",
        "wait",
        SESSION_ID,
        "--state",
        "done",
        "--timeout-ms",
        "200",
        "--json",
    ],
];

#[cfg(target_os = "linux")]
fn command_method(command: &[&str]) -> &'static str {
    match command[1] {
        "output" => protocol::method::SESSION_OUTPUT,
        "wait" => protocol::method::SESSION_WAIT,
        other => panic!("unexpected pinned command {other}"),
    }
}

/// Selector forms that pin to the first peer: a short name, a
/// provider-qualified name and a name with the daemon port in the route.
#[cfg(target_os = "linux")]
const PIN_SELECTORS: [(&str, Option<&str>); 3] = [
    ("peer-a", None),
    ("netbird:peer-a", None),
    ("peer-a@{port}", Some(PIN_WRONG_PORT)),
];

#[test]
#[cfg(target_os = "linux")]
fn session_output_and_wait_stay_on_the_peer_the_name_first_selected() {
    for command in PINNED_SESSION_COMMANDS {
        for (selector, env_port) in PIN_SELECTORS {
            let run = run_pinned(
                selector,
                command,
                &pin_before(),
                &pin_after_rename(),
                env_port,
            );
            assert_json_success(&run.output);
            assert!(
                run.dialed.len() >= 2,
                "{selector} {command:?}: one connection resolves the session, one acts: {:?}",
                run.dialed
            );
            assert!(
                run.dialed.iter().all(|ip| ip == PIN_FIRST_IP),
                "{selector} {command:?}: every connection reaches the first peer: {:?}",
                run.dialed
            );
            assert!(
                run.requests
                    .iter()
                    .any(|request| request.method() == command_method(command)),
                "{selector} {command:?}: the action reached the first peer's daemon"
            );
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn a_pinned_peer_that_changes_address_is_followed_by_identity() {
    let moved = pin_overlay::status(
        None,
        &[
            pin_first_peer("peer-x-old.netbird.test", PIN_MOVED_IP),
            pin_second_peer("peer-a.netbird.test"),
        ],
    );
    for command in PINNED_SESSION_COMMANDS {
        let run = run_pinned("peer-a", command, &pin_before(), &moved, None);
        assert_json_success(&run.output);
        assert_eq!(
            run.dialed.first().map(String::as_str),
            Some(PIN_FIRST_IP),
            "{command:?}"
        );
        assert!(
            run.dialed.iter().skip(1).all(|ip| ip == PIN_MOVED_IP),
            "{command:?}: the later connection follows the pinned peer to its new address: {:?}",
            run.dialed
        );
        assert!(
            !run.dialed.iter().any(|ip| ip == PIN_SECOND_IP),
            "{command:?}"
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn a_pinned_peer_that_vanishes_fails_closed_under_the_typed_name() {
    let vanished = pin_overlay::status(None, &[pin_second_peer("peer-a.netbird.test")]);
    for command in PINNED_SESSION_COMMANDS {
        let run = run_pinned("peer-a", command, &pin_before(), &vanished, None);
        let document = assert_json_error(&run.output, "host_unknown");
        let message = document["err"]["msg"].as_str().expect("error message");
        assert!(message.contains("peer-a"), "{command:?}: {message}");
        assert!(!message.contains("peer~"), "{command:?}: {message}");
        assert!(
            run.dialed.iter().all(|ip| ip == PIN_FIRST_IP),
            "{command:?}: the second peer is never contacted: {:?}",
            run.dialed
        );
        assert!(
            !run.requests
                .iter()
                .any(|request| request.method() == command_method(command)),
            "{command:?}: no action reached any daemon"
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn a_host_without_a_peer_id_or_fqdn_cannot_be_pinned() {
    let unnamed = pin_overlay::status(
        None,
        &[pin_overlay::Peer {
            fqdn: None,
            ip: "100.64.0.9",
            key: None,
        }],
    );
    for command in PINNED_SESSION_COMMANDS {
        let run = run_pinned("100.64.0.9", command, &unnamed, &unnamed, None);
        let document = assert_json_error(&run.output, "host_identity_unpinnable");
        assert!(
            document["err"]["msg"]
                .as_str()
                .is_some_and(|message| message.contains("100.64.0.9")),
            "{document}"
        );
        assert!(run.dialed.is_empty(), "{command:?}: {:?}", run.dialed);
        assert!(run.requests.is_empty(), "{command:?}: no daemon was asked");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn an_ambiguous_name_is_refused_before_any_connection() {
    let ambiguous = pin_overlay::status(
        None,
        &[
            pin_first_peer("build.one.netbird.test", PIN_FIRST_IP),
            pin_second_peer("build.two.netbird.test"),
        ],
    );
    for command in PINNED_SESSION_COMMANDS {
        let run = run_pinned("build", command, &ambiguous, &ambiguous, None);
        assert_json_error(&run.output, "overlay_host_ambiguous");
        assert!(run.dialed.is_empty(), "{command:?}: {:?}", run.dialed);
        assert!(run.requests.is_empty(), "{command:?}");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn pinned_operations_of_the_machines_own_overlay_name_stay_on_the_local_daemon() {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let tcp_fixture = TcpFixtureDaemon::start(Ipv4Addr::LOCALHOST, Scenario::Success);
    let remote_port = tcp_fixture.address.port();
    let (connect_redirect, connect_capture) = connect_redirect::build(&home.root);
    let netbird = SelfNetbird::install(&home.root);
    for command in PINNED_SESSION_COMMANDS {
        for host in ["local", SELF_FQDN] {
            let output = home
                .command()
                .env("PATH", &netbird.path)
                .env("LD_PRELOAD", &connect_redirect)
                .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
                .env("POHUNEK_REMOTE_PORT", remote_port.to_string())
                .args(["--host", host])
                .args(command)
                .output()
                .expect("run pohunek");
            assert_json_success(&output);
        }
    }
    let local_requests = local_fixture.finish();
    for command in PINNED_SESSION_COMMANDS {
        assert!(
            local_requests
                .iter()
                .any(|request| request.method() == command_method(command)),
            "{command:?} reached the local daemon"
        );
    }
    assert!(
        tcp_fixture.finish().is_empty(),
        "an own-name operation never dials the overlay"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn a_stalled_lookup_of_a_pinned_peer_fails_under_the_typed_name() {
    for command in PINNED_SESSION_COMMANDS {
        let run = run_pinned("peer-a", command, &pin_before(), PIN_STALL, None);
        let document = assert_json_error(&run.output, "remote_discovery_failed");
        let message = document["err"]["msg"].as_str().expect("error message");
        assert!(message.contains("peer-a"), "{command:?}: {message}");
        assert!(!message.contains("peer~"), "{command:?}: {message}");
        assert!(
            run.dialed.iter().all(|ip| ip == PIN_FIRST_IP),
            "{command:?}: {:?}",
            run.dialed
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn the_names_of_the_local_record_select_the_local_daemon() {
    let home = TestHome::new();
    let local_fixture = FixtureDaemon::start(&home.socket(), Scenario::Success);
    let tcp_fixture = TcpFixtureDaemon::start(Ipv4Addr::LOCALHOST, Scenario::Success);
    let remote_port = tcp_fixture.address.port();
    let (connect_redirect, connect_capture) = connect_redirect::build(&home.root);
    let netbird = SelfNetbird::install(&home.root);
    let run = |args: &[&str]| {
        home.command()
            .env("PATH", &netbird.path)
            .env("LD_PRELOAD", &connect_redirect)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
            .env("POHUNEK_REMOTE_PORT", remote_port.to_string())
            .args(args)
            .output()
            .expect("run pohunek")
    };
    let records = assert_json_success(&run(&["host", "discover", "--refresh", "--json"]));
    let local = records
        .as_array()
        .expect("host records")
        .iter()
        .find(|record| record["local"] == true)
        .expect("the local record");
    // A consumer feeds the record's own names back as `--host`; a release
    // that predates the record maps them to the local daemon the same way.
    for field in ["fqdn", "address"] {
        let host = local[field].as_str().expect("record field");
        let ok = assert_json_success(&run(&[
            "--host", host, "session", "screen", SESSION_ID, "--json",
        ]));
        assert_eq!(ok["session_id"], SESSION_ID, "{field}");
    }
    let screens = |requests: Vec<Request>| {
        requests
            .iter()
            .filter(|request| request.method() == protocol::method::SESSION_SCREEN)
            .count()
    };
    assert_eq!(
        screens(local_fixture.finish()),
        2,
        "both reached the local daemon"
    );
    assert_eq!(
        screens(tcp_fixture.finish()),
        0,
        "neither dialed the overlay"
    );
}
