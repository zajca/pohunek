//! Binary-level scenario for the waiting notice of `session fork`, `stop` and
//! `rm`.
//!
//! The built CLI runs against a fixture daemon that holds the reply to the
//! lifecycle request. A plain run prints a waiting notice to stderr once it has
//! waited past the notice delay; a `--json` run prints none. The test releases
//! the reply when the notice appears, so it never sleeps or measures time.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// Mode of the runtime directory the client expects to be owner-private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// A lifecycle command, the request it sends and the reply the fixture holds.
struct Scenario {
    arguments: &'static [&'static str],
    method: &'static str,
    result: Value,
    notice: &'static str,
    human_stdout: &'static str,
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            arguments: &["session", "stop", "s-42"],
            method: protocol::method::SESSION_STOP,
            result: json!({"stopped": true}),
            notice: "waiting for the daemon to stop s-42",
            human_stdout: "session s-42: stopped=true",
        },
        Scenario {
            arguments: &["session", "fork", "s-42"],
            method: protocol::method::SESSION_FORK,
            result: json!({
                "id": "s-43",
                "agent": "shell",
                "agent_base": "shell",
                "capabilities": {"fork": true, "resume": true},
                "cwd": "/workspace/pohunek",
                "pid": 42424,
                "state": "running",
                "state_source": "process",
                "cols": 80,
                "rows": 24,
                "created_at": "2026-07-08T00:00:00Z",
                "updated_at": "2026-07-08T00:01:00Z",
            }),
            notice: "waiting for the daemon to fork s-42",
            human_stdout: "session s-43 forked",
        },
        Scenario {
            arguments: &["session", "rm", "s-42"],
            method: protocol::method::SESSION_REMOVE,
            result: json!({
                "removed": true,
                "stopped": true,
                "worktrees_removed": 0,
                "worktrees_failed": 0
            }),
            notice: "waiting for the daemon to remove s-42",
            human_stdout: "session s-42: removed=true",
        },
    ]
}

#[test]
fn a_long_lifecycle_call_prints_a_waiting_notice_only_without_json() {
    // The scenarios wait out the notice delay in parallel.
    thread::scope(|scope| {
        for scenario in scenarios() {
            scope.spawn(move || run_scenario(&scenario));
        }
    });
}

fn run_scenario(scenario: &Scenario) {
    {
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
        let fixture = Fixture::serve(&root.join("run/pohunek/daemon.sock"), &scenario);

        // The `--json` run starts first, so its notice timer expires before
        // the plain run's: when the plain notice is out, a `--json` notice
        // would be out as well.
        let json_run = spawn(&env, scenario.arguments, true);
        fixture.wait_for_request();
        let plain_run = spawn(&env, scenario.arguments, false);
        fixture.wait_for_request();

        let (plain_stderr, plain_notices) = read_notice(plain_run, scenario.notice);
        fixture.release(2);
        let plain = plain_stderr.finish();
        let json = json_run.wait_with_output().expect("wait for the json run");

        assert!(
            plain.status.success(),
            "{:?}: {plain:?}",
            scenario.arguments
        );
        assert!(
            plain_notices.contains(scenario.notice),
            "{:?}: {plain_notices}",
            scenario.arguments
        );
        assert!(json.status.success(), "{:?}: {json:?}", scenario.arguments);
        assert!(
            json.stderr.is_empty(),
            "{:?}: --json prints no waiting notice: {}",
            scenario.arguments,
            String::from_utf8_lossy(&json.stderr)
        );
        serde_json::from_slice::<Value>(&json.stdout).expect("stdout stays one JSON document");
        assert!(
            String::from_utf8_lossy(&plain.stdout).starts_with(scenario.human_stdout),
            "{:?}: {}",
            scenario.arguments,
            String::from_utf8_lossy(&plain.stdout)
        );
    }
}

fn spawn(env: &TestEnv, arguments: &[&str], json: bool) -> Child {
    let mut command: Command = env.command(pohunek_test_support::bin_exe("pohunek"));
    command.args(arguments);
    if json {
        command.arg("--json");
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pohunek")
}

/// stderr of the plain run, read until its waiting notice appeared.
struct PlainRun {
    child: Child,
    rest: mpsc::Receiver<Vec<u8>>,
}

impl PlainRun {
    fn finish(mut self) -> std::process::Output {
        let status = self.child.wait().expect("wait for the plain run");
        let mut stdout = Vec::new();
        self.child
            .stdout
            .take()
            .expect("piped stdout")
            .read_to_end(&mut stdout)
            .expect("read stdout");
        let stderr = self.rest.recv().expect("remaining stderr");
        std::process::Output {
            status,
            stdout,
            stderr,
        }
    }
}

/// Reads stderr line by line until a line contains `notice`; returns the run
/// and the text read so far.
fn read_notice(mut child: Child, notice: &str) -> (PlainRun, String) {
    let stderr = child.stderr.take().expect("piped stderr");
    let (seen_tx, seen_rx) = mpsc::channel::<String>();
    let (rest_tx, rest) = mpsc::channel::<Vec<u8>>();
    let notice = notice.to_owned();
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut seen = String::new();
        let mut line = String::new();
        let mut announced = false;
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            seen.push_str(&line);
            if !announced && seen.contains(&notice) {
                announced = true;
                let _ = seen_tx.send(seen.clone());
            }
        }
        let _ = rest_tx.send(seen.into_bytes());
    });
    let seen = seen_rx
        .recv_timeout(pohunek_test_support::wait::HANG_GUARD)
        .expect("the plain run prints its waiting notice");
    (PlainRun { child, rest }, seen)
}

/// Fixture daemon holding the reply to the scenario's lifecycle request.
struct Fixture {
    requests: mpsc::Receiver<()>,
    releases: mpsc::Sender<()>,
}

impl Fixture {
    fn serve(socket: &Path, scenario: &Scenario) -> Self {
        let listener = UnixListener::bind(socket).expect("bind the fixture daemon");
        let (request_tx, requests) = mpsc::channel();
        let (releases, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let method = scenario.method;
        let result = scenario.result.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let stream = stream.expect("accept a fixture connection");
                let request_tx = request_tx.clone();
                let release_rx = Arc::clone(&release_rx);
                let result = result.clone();
                thread::spawn(move || {
                    serve_connection(stream, method, &result, &request_tx, &release_rx);
                });
            }
        });
        Self { requests, releases }
    }

    fn wait_for_request(&self) {
        self.requests
            .recv_timeout(pohunek_test_support::wait::HANG_GUARD)
            .expect("the CLI sent the lifecycle request");
    }

    fn release(&self, replies: usize) {
        for _ in 0..replies {
            self.releases.send(()).expect("release a reply");
        }
    }
}

fn serve_connection(
    stream: UnixStream,
    method: &str,
    result: &Value,
    request_tx: &mpsc::Sender<()>,
    release_rx: &Mutex<mpsc::Receiver<()>>,
) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse a request");
        let value = if request.method() == protocol::method::DAEMON_HEALTH {
            json!({
                "status": "ok",
                "daemon_version": "fixture",
                "protocol_version": PROTOCOL_VERSION.get()
            })
        } else if request.method() == protocol::method::SESSION_LIST {
            // Target resolution finds no session by that name and keeps the id.
            json!([])
        } else {
            assert_eq!(
                request.method(),
                method,
                "the CLI sent an unexpected request"
            );
            let _ = request_tx.send(());
            release_rx
                .lock()
                .expect("release queue")
                .recv()
                .expect("the test releases the reply");
            result.clone()
        };
        let response =
            Response::ok(PROTOCOL_VERSION, request.id(), value).expect("valid fixture response");
        // The CLI may have gone away already.
        let _ = writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize the response")
        );
    }
}
