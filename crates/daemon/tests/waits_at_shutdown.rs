//! A bounded `session.wait` or `session.output` wait that is in flight when
//! `pohunekd` receives `SIGTERM` is answered with `daemon_shutting_down`
//! before the process exits, instead of ending as an EOF when the runtime
//! drops.
//!
//! Both scenarios run the real daemon with a real shell session, send the wait
//! with a bound far above the test's patience, and stop the daemon while the
//! request is in flight.

// Rust guideline compliant 2026-10-31

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use futures::{SinkExt as _, StreamExt as _};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::{guard, wait_until};
use pohunek_test_support::{bin_exe, worker_binary};
use protocol::{method, ProtocolError, Request, Response};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio_util::codec::{Framed, LinesCodec};

/// Directory name the daemon uses below the XDG roots.
const APP_DIR: &str = "pohunek";

/// Bound of the waits under test; the replies must come far sooner.
const WAIT_BOUND_MS: u32 = 8_000;

/// The longest the shutdown reply may take.
const REPLY_CEILING: Duration = Duration::from_secs(4);

/// Quiet period that marks the shell's output as settled.
const QUIET_MS: u32 = 1_500;

type Control = Framed<UnixStream, LinesCodec>;

/// A running daemon with one shell session.
struct Fixture {
    // Declared before `env` so the daemon is reaped while the root still exists.
    daemon: Child,
    pid: Pid,
    control: Control,
    session: String,
    _env: TestEnv,
}

fn private_dir(path: &Path) {
    fs::create_dir_all(path).expect("create directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("make directory private");
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

        let mut command = env.tokio_command(bin_exe("pohunekd"));
        command
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_DATA_HOME", &data_home)
            .env("SHELL", "/bin/sh")
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
        .expect("session.new creates the shell session");
        let session = created["id"]
            .as_str()
            .expect("the new session has an id")
            .to_owned();
        Self {
            daemon,
            pid,
            control,
            session,
            _env: env,
        }
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

    /// The identity and end offset of the session's output once it is quiet.
    async fn settled_output(&mut self) -> (Value, Value) {
        let mut cursor: Option<(Value, Value)> = None;
        loop {
            let mut params = json!({
                "session_id": self.session,
                "max_bytes": 65_536,
            });
            if let Some((runtime, offset)) = &cursor {
                params["runtime"] = runtime.clone();
                params["after_offset"] = offset.clone();
                params["wait_ms"] = json!(QUIET_MS);
            }
            let page = call(&mut self.control, "settle", method::SESSION_OUTPUT, params)
                .await
                .expect("session.output reads the retained output");
            let runtime = json!({
                "worker_instance_id": page["worker_instance_id"],
                "runtime_generation": page["runtime_generation"],
            });
            let next = page["next_offset"].clone();
            if cursor.is_some() && page["timed_out"] == json!(true) {
                return (runtime, next);
            }
            cursor = Some((runtime, next));
        }
    }
}

/// Sends one request and returns its success value or typed error.
async fn call(
    control: &mut Control,
    id: &str,
    method: &str,
    params: Value,
) -> Result<Value, ProtocolError> {
    let request = Request::new(id, method, params).expect("valid request");
    control
        .send(serde_json::to_string(&request).expect("serialize request"))
        .await
        .expect("send request");
    let line = guard("the reply", control.next())
        .await
        .expect("a reply line")
        .expect("reply framing ok");
    serde_json::from_str::<Response>(&line)
        .expect("parse the reply")
        .into_result()
}

/// Asserts that the in-flight wait ended with `daemon_shutting_down`, before
/// the daemon exited and long before the wait's own bound.
async fn assert_answered_before_exit(mut fixture: Fixture, sent_at: Instant) {
    let outcome = fixture.reply().await;
    let waited = sent_at.elapsed();
    let error = outcome.expect_err("the wait ends with an error");
    assert_eq!(error.code, "daemon_shutting_down", "{error:?}");
    assert!(
        waited < REPLY_CEILING,
        "the reply took {waited:?}, so it is not the shutdown but the wait's own bound"
    );
    let status = guard("the stopping daemon exits", fixture.daemon.wait())
        .await
        .expect("wait for the daemon");
    assert!(status.success(), "the daemon exits cleanly: {status:?}");
}

#[tokio::test]
async fn a_session_wait_in_flight_is_answered_when_the_daemon_stops() {
    let mut fixture = Fixture::start().await;
    let request = Request::new(
        "wait",
        method::SESSION_WAIT,
        json!({
            "session_id": fixture.session,
            "states": ["failed"],
            "timeout_ms": WAIT_BOUND_MS,
        }),
    )
    .expect("valid request");
    let sent_at = Instant::now();
    fixture
        .control
        .send(serde_json::to_string(&request).expect("serialize request"))
        .await
        .expect("send session.wait");
    // The wait is registered once it has taken its waiter slot, which it does
    // before it sleeps; a short settle keeps the signal behind the request.
    tokio::time::sleep(Duration::from_millis(500)).await;

    fixture.stop();

    assert_answered_before_exit(fixture, sent_at).await;
}

#[tokio::test]
async fn a_session_output_wait_in_flight_is_answered_when_the_daemon_stops() {
    let mut fixture = Fixture::start().await;
    let (runtime, offset) = fixture.settled_output().await;
    let request = Request::new(
        "output",
        method::SESSION_OUTPUT,
        json!({
            "session_id": fixture.session,
            "runtime": runtime,
            "after_offset": offset,
            "max_bytes": 4_096,
            "wait_ms": WAIT_BOUND_MS,
        }),
    )
    .expect("valid request");
    let sent_at = Instant::now();
    fixture
        .control
        .send(serde_json::to_string(&request).expect("serialize request"))
        .await
        .expect("send session.output");
    tokio::time::sleep(Duration::from_millis(500)).await;

    fixture.stop();

    assert_answered_before_exit(fixture, sent_at).await;
}
