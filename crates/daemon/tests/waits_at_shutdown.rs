//! A bounded `session.wait` or `session.output` wait that is in flight when
//! `pohunekd` receives `SIGTERM` is answered with `daemon_shutting_down`
//! before the process exits, instead of ending as an EOF when the runtime
//! drops. A raw attach stream open at `SIGTERM` ends and is recorded as closed.
//!
//! The wait scenarios run the real daemon with a real session and stop it only
//! after its log shows the wait parked, so the reply comes from the wait
//! itself and not from a request that reached the daemon after the signal.

// Rust guideline compliant 2026-10-31

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::{guard, wait_until};
use pohunek_test_support::{bin_exe, worker_binary};
use protocol::{method, ProtocolError, Request, Response, MAX_SESSION_WAIT_MS};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio_util::codec::{Framed, LinesCodec};

/// Directory name the daemon uses below the XDG roots.
const APP_DIR: &str = "pohunek";

/// What the session's program prints once, before it goes quiet.
const MARKER: &str = "session-ready";

/// The session's program: it prints [`MARKER`] and then stays silent, so the
/// output has a known end.
const SESSION_PROGRAM: &str = "#!/bin/sh\nprintf session-ready\nexec sleep 30\n";

/// The log line of a `session.wait` that has taken its waiter slot.
const WAIT_PARKED: &str = "session wait parked";

/// The log line of a `session.output` wait that is about to read.
const OUTPUT_PARKED: &str = "session output wait parked";

type Control = Framed<UnixStream, LinesCodec>;

/// A running daemon with one session.
struct Fixture {
    // Declared before `env` so the daemon is reaped while the root still exists.
    daemon: Child,
    pid: Pid,
    control: Control,
    session: String,
    socket: PathBuf,
    events: PathBuf,
    logs: PathBuf,
    _env: TestEnv,
}

fn private_dir(path: &Path) {
    fs::create_dir_all(path).expect("create directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("make directory private");
}

/// Whether any file below `dir` holds `needle`.
fn any_file_contains(dir: &Path, needle: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            any_file_contains(&path, needle)
        } else {
            fs::read_to_string(&path).is_ok_and(|text| text.contains(needle))
        }
    })
}

impl Fixture {
    async fn start() -> Self {
        let env = TestEnv::new().expect("create short test environment");
        let runtime = env.root().join("r");
        let state_home = env.root().join("s");
        let data_home = env.root().join("data");
        private_dir(&data_home.join(APP_DIR));
        let work = env.root().join("work");
        private_dir(&work);
        let program = env.root().join("session-program");
        fs::write(&program, SESSION_PROGRAM).expect("write the session program");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700))
            .expect("make the session program executable");

        let mut command = env.tokio_command(bin_exe("pohunekd"));
        command
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_DATA_HOME", &data_home)
            .env("SHELL", &program)
            .env(
                "RUST_LOG",
                "pohunek_daemon::session::observation=debug,info",
            )
            .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
            .env("POHUNEK_WORKER_BIN", worker_binary())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut daemon = command.spawn().expect("spawn daemon");
        let pid = Pid::from_raw(
            i32::try_from(daemon.id().expect("daemon pid")).expect("pid fits in i32"),
        );

        let socket = runtime.join(APP_DIR).join("daemon.sock");
        let stream = tokio::select! {
            stream = wait_until("the daemon control socket accepts connections", || async {
                UnixStream::connect(&socket).await.ok()
            }) => stream,
            status = daemon.wait() => panic!("daemon exited before readiness: {status:?}"),
        };
        let mut control = Framed::new(stream, LinesCodec::new());
        let created = call(
            &mut control,
            "new",
            method::SESSION_NEW,
            json!({
                "agent": "shell",
                "cwd": work,
                "cols": 80,
                "rows": 24,
            }),
        )
        .await
        .expect("session.new creates the session");
        let session = created["id"]
            .as_str()
            .expect("the new session has an id")
            .to_owned();
        Self {
            daemon,
            pid,
            control,
            session,
            socket,
            logs: state_home.join(APP_DIR).join("logs"),
            events: data_home.join(APP_DIR).join("events"),
            _env: env,
        }
    }

    /// Waits until the daemon's log shows `line`.
    async fn wait_for_log(&self, line: &str) {
        wait_until("the wait to park in the daemon", || async {
            any_file_contains(&self.logs, line).then_some(())
        })
        .await;
    }

    /// Sends the daemon `SIGTERM`.
    fn stop(&self) {
        kill(self.pid, Signal::SIGTERM).expect("stop the daemon");
    }

    /// Reads the next reply line of the control connection.
    async fn reply(&mut self) -> Result<Value, ProtocolError> {
        let line = guard("the reply to the wait", self.control.next())
            .await
            .expect("a reply line before the connection ends")
            .expect("reply framing ok");
        serde_json::from_str::<Response>(&line)
            .expect("parse the reply")
            .into_result()
    }

    /// The runtime identity and the end offset of the session's output once it
    /// holds [`MARKER`]; the program prints nothing after it.
    async fn output_end(&mut self) -> (Value, Value) {
        let session = self.session.clone();
        let control = tokio::sync::Mutex::new(&mut self.control);
        wait_until("the session output to hold the marker", || async {
            let mut control = control.lock().await;
            let page = call(
                &mut control,
                "tail",
                method::SESSION_OUTPUT,
                json!({ "session_id": session, "max_bytes": 65_536 }),
            )
            .await
            .ok()?;
            let data = base64::engine::general_purpose::STANDARD
                .decode(page["data_base64"].as_str().expect("output data"))
                .expect("output data is base64");
            String::from_utf8_lossy(&data).contains(MARKER).then(|| {
                let runtime = json!({
                    "worker_instance_id": page["worker_instance_id"],
                    "runtime_generation": page["runtime_generation"],
                });
                (runtime, page["runtime_end_offset"].clone())
            })
        })
        .await
    }
}

/// Sends one request and returns its success value or typed error.
async fn call(
    control: &mut Control,
    id: &str,
    method: &str,
    params: Value,
) -> Result<Value, ProtocolError> {
    send(control, id, method, params).await;
    let line = guard("the reply", control.next())
        .await
        .expect("a reply line")
        .expect("reply framing ok");
    serde_json::from_str::<Response>(&line)
        .expect("parse the reply")
        .into_result()
}

async fn send(control: &mut Control, id: &str, method: &str, params: Value) {
    let request = Request::new(id, method, params).expect("valid request");
    control
        .send(serde_json::to_string(&request).expect("serialize request"))
        .await
        .expect("send request");
}

/// Asserts that the parked wait ended with `daemon_shutting_down` and that the
/// daemon then exited.
async fn assert_answered_then_exited(mut fixture: Fixture) {
    let error = fixture
        .reply()
        .await
        .expect_err("the wait ends with an error");
    assert_eq!(error.code, "daemon_shutting_down", "{error:?}");
    let status = guard("the stopping daemon exits", fixture.daemon.wait())
        .await
        .expect("wait for the daemon");
    assert!(status.success(), "the daemon exits cleanly: {status:?}");
}

#[tokio::test]
async fn a_session_wait_in_flight_is_answered_when_the_daemon_stops() {
    let mut fixture = Fixture::start().await;
    send(
        &mut fixture.control,
        "wait",
        method::SESSION_WAIT,
        json!({
            "session_id": fixture.session,
            "states": ["failed"],
            "timeout_ms": MAX_SESSION_WAIT_MS,
        }),
    )
    .await;
    fixture.wait_for_log(WAIT_PARKED).await;

    fixture.stop();

    assert_answered_then_exited(fixture).await;
}

#[tokio::test]
async fn a_session_output_wait_in_flight_is_answered_when_the_daemon_stops() {
    let mut fixture = Fixture::start().await;
    let (runtime, offset) = fixture.output_end().await;
    send(
        &mut fixture.control,
        "output",
        method::SESSION_OUTPUT,
        json!({
            "session_id": fixture.session,
            "runtime": runtime,
            "after_offset": offset,
            "max_bytes": 4_096,
            "wait_ms": MAX_SESSION_WAIT_MS,
        }),
    )
    .await;
    fixture.wait_for_log(OUTPUT_PARKED).await;

    fixture.stop();

    assert_answered_then_exited(fixture).await;
}

/// Whether the daemon's event log below `events` holds a line of `event`
/// for the attach stream `stream_id`.
fn attach_event_recorded(events: &Path, event: &str, stream_id: &str) -> bool {
    fn scan(dir: &Path, event: &str, stream_id: &str) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|entry| {
            let path = entry.path();
            if path.is_dir() {
                scan(&path, event, stream_id)
            } else {
                fs::read_to_string(&path).is_ok_and(|text| {
                    text.lines()
                        .any(|line| line.contains(event) && line.contains(stream_id))
                })
            }
        })
    }
    scan(events, event, stream_id)
}

#[tokio::test]
async fn an_attach_stream_open_when_the_daemon_stops_is_ended_and_recorded_as_closed() {
    let mut fixture = Fixture::start().await;
    let attached = call(
        &mut fixture.control,
        "attach",
        method::SESSION_ATTACH,
        json!({ "session_id": fixture.session }),
    )
    .await
    .expect("session.attach opens the stream");
    let stream_id = attached["stream_id"]
        .as_str()
        .expect("the attach result has a stream id")
        .to_owned();
    let mut raw = UnixStream::connect(&fixture.socket)
        .await
        .expect("connect the raw stream");
    raw.write_all(format!("{}\n", json!({ "attach": stream_id })).as_bytes())
        .await
        .expect("send the attach header");
    wait_until("the attach stream to be bound", || async {
        attach_event_recorded(&fixture.events, "attach_opened", &stream_id).then_some(())
    })
    .await;

    fixture.stop();

    // The raw stream ends; `attach_closed` below tells that the daemon ended
    // the bridge and not only that the process exited.
    let mut buffer = [0_u8; 4_096];
    loop {
        let read = guard("the attach stream to end", raw.read(&mut buffer))
            .await
            .unwrap_or(0);
        if read == 0 {
            break;
        }
    }
    let status = guard("the stopping daemon exits", fixture.daemon.wait())
        .await
        .expect("wait for the daemon");
    assert!(status.success(), "the daemon exits cleanly: {status:?}");
    assert!(
        attach_event_recorded(&fixture.events, "attach_closed", &stream_id),
        "the closed attach stream is recorded"
    );
}
