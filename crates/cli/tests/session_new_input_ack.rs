//! `pohunek session new --input-ack-timeout-ms` against a fixture daemon.
//!
//! The built CLI starts sessions on a fixture daemon that records every
//! request. The scenarios pin what the CLI sends, how it reports the
//! acknowledgement of the initial input, and that it refuses a host that
//! cannot report one before it creates a session.

#![cfg(unix)]

// Rust guideline compliant 2026-10-22

use std::fs;
use std::io::{BufRead as _, BufReader, Read, Write};
#[cfg(target_os = "linux")]
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

#[cfg(target_os = "linux")]
#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[cfg(target_os = "linux")]
#[path = "support/pin_overlay.rs"]
mod pin_overlay;

/// Mode of the runtime and state directories the client expects private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Id of the session the fixture daemon reports as created.
const SESSION_ID: &str = "s-ack-1";

/// Commit the fixture daemon reports a remote-mode worktree started at.
const REMOTE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

/// What the fixture daemon advertises and answers.
#[derive(Clone, Copy)]
struct Daemon {
    ack_supported: bool,
    branch_source_supported: bool,
    prompt_delivered: Option<bool>,
    /// Activity the created session reports, when the daemon reports one.
    activity: Option<&'static str>,
}

fn capabilities(daemon: Daemon) -> Value {
    let mut value = json!({
        "daemon_version": "fixture",
        "protocol_version": PROTOCOL_VERSION.get(),
        "supported_agents": ["shell"],
        "runtimes": [],
        "git_available": true,
        "worktree_supported": true,
        "terminal_read_supported": true,
        "output_read_supported": true,
        "session_wait_supported": true
    });
    if daemon.ack_supported {
        value["session_new_input_ack_supported"] = json!(true);
    }
    if daemon.branch_source_supported {
        value["session_new_branch_source_supported"] = json!(true);
    }
    value
}

fn session_new_result(request: &Request, daemon: Daemon) -> Value {
    let mut value = json!({
        "id": SESSION_ID,
        "agent": "shell",
        "agent_base": "shell",
        "capabilities": {"fork": false, "resume": false},
        "cwd": "/project/root",
        "pid": 42424,
        "state": "running",
        "state_source": "process",
        "cols": 80,
        "rows": 24,
        "created_at": "2026-07-08T00:00:00Z",
        "updated_at": "2026-07-08T00:01:00Z",
        "applied_input": true
    });
    if let Some(delivered) = daemon.prompt_delivered {
        value["prompt_delivered"] = json!(delivered);
    }
    if let Some(activity) = daemon.activity {
        value["activity"] = json!(activity);
    }
    if request.params()["branch_source"] == "remote" {
        value["worktree_source"] = json!({"ref": "origin/feat", "commit": REMOTE_COMMIT});
    }
    value
}

fn result_for(request: &Request, daemon: Daemon) -> Value {
    match request.method() {
        protocol::method::DAEMON_HEALTH => json!({
            "status": "ok",
            "daemon_version": "fixture",
            "protocol_version": PROTOCOL_VERSION.get()
        }),
        protocol::method::HOST_INSPECT => capabilities(daemon),
        protocol::method::SESSION_NEW => session_new_result(request, daemon),
        other => panic!("the CLI sent an unexpected request: {other}"),
    }
}

fn serve_connection<S: Read + Write>(stream: S, served: &Served, daemon: Daemon) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse a request");
        served
            .lock()
            .expect("served requests")
            .push((request.method().to_owned(), request.params().clone()));
        let response = Response::ok(PROTOCOL_VERSION, request.id(), result_for(&request, daemon))
            .expect("valid fixture response");
        // The CLI may have gone away already.
        let _ = writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize the response")
        );
    }
}

fn private_environment() -> TestEnv {
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
    env
}

fn serve_local(socket: &Path, daemon: Daemon) -> Served {
    let served = Served::default();
    let listener = UnixListener::bind(socket).expect("bind the fixture daemon");
    let recorded = Arc::clone(&served);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&recorded);
            thread::spawn(move || serve_connection(stream, &served, daemon));
        }
    });
    served
}

/// Result of one `pohunek session new` run.
struct Run {
    output: Output,
    served: Vec<(String, Value)>,
}

impl Run {
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    fn document(&self) -> Value {
        serde_json::from_slice(&self.output.stdout).unwrap_or_else(|error| {
            panic!(
                "one JSON document ({error}): stdout {}; stderr {}",
                self.stdout(),
                String::from_utf8_lossy(&self.output.stderr)
            )
        })
    }

    fn session_new(&self) -> Option<&Value> {
        self.served
            .iter()
            .find(|(method, _)| method == protocol::method::SESSION_NEW)
            .map(|(_, params)| params)
    }
}

/// Runs `pohunek session new --agent shell --input hello <args>` against a
/// local fixture daemon.
fn run_local(args: &[&str], daemon: Daemon) -> Run {
    run_local_with(&["--input", "hello"], None, args, daemon)
}

/// Runs `pohunek session new --agent shell <input_args> <args>` against a local
/// fixture daemon, feeding `stdin` to the command when given.
fn run_local_with(input_args: &[&str], stdin: Option<&str>, args: &[&str], daemon: Daemon) -> Run {
    let env = private_environment();
    let root = env.root().to_path_buf();
    let served = serve_local(&root.join("run/pohunek/daemon.sock"), daemon);
    let mut command = env.command(pohunek_test_support::bin_exe("pohunek"));
    command
        .args(["session", "new", "--agent", "shell"])
        .args(input_args)
        .args(args);
    let output = match stdin {
        None => command.output().expect("run pohunek session new"),
        Some(text) => {
            let mut child = command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn pohunek session new");
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(text.as_bytes())
                .expect("write the prompt to stdin");
            child.wait_with_output().expect("collect pohunek output")
        }
    };
    let served = served.lock().expect("served requests").clone();
    Run { output, served }
}

const ACK_DAEMON: Daemon = Daemon {
    ack_supported: true,
    branch_source_supported: true,
    prompt_delivered: Some(true),
    activity: None,
};

#[test]
fn an_acknowledged_start_reports_prompt_delivered_in_json() {
    for delivered in [true, false] {
        let run = run_local(
            &["--input-ack-timeout-ms", "8000", "--json"],
            Daemon {
                prompt_delivered: Some(delivered),
                ..ACK_DAEMON
            },
        );

        let document = run.document();
        assert_eq!(document["ok"]["prompt_delivered"], delivered, "{document}");
        assert_eq!(document["ok"]["id"], SESSION_ID, "{document}");
        let params = run.session_new().expect("session.new was sent");
        assert_eq!(params["input_ack_timeout_ms"], 8000);
        assert_eq!(params["input"], "hello");
    }
}

#[test]
fn an_acknowledged_start_accepts_the_prompt_from_stdin() {
    let run = run_local_with(
        &["--input-stdin"],
        Some("from stdin"),
        &["--input-ack-timeout-ms", "250", "--json"],
        ACK_DAEMON,
    );

    let document = run.document();
    assert_eq!(document["ok"]["prompt_delivered"], true, "{document}");
    let params = run.session_new().expect("session.new was sent");
    assert_eq!(params["input"], "from stdin");
    assert_eq!(params["input_ack_timeout_ms"], 250);
}

#[test]
fn an_acknowledged_start_prints_the_outcome_for_humans() {
    for (delivered, line) in [
        (true, "prompt delivered: yes"),
        (false, "prompt delivered: no"),
    ] {
        let run = run_local(
            &["--input-ack-timeout-ms", "100"],
            Daemon {
                prompt_delivered: Some(delivered),
                ..ACK_DAEMON
            },
        );

        assert!(run.output.status.success(), "{}", run.stdout());
        assert!(run.stdout().contains(line), "{}", run.stdout());
    }
}

/// A kept create whose agent waits on a dialog tells the user where to answer
/// it; an undelivered prompt on any other activity, and a delivered prompt on
/// a blocked one, carry no such line.
#[test]
fn an_undelivered_prompt_behind_a_dialog_names_where_to_answer_it() {
    let line = "waiting on a dialog: see `pohunek session screen s-ack-1`, answer it with `pohunek attach s-ack-1`";
    let acknowledged: &[&str] = &["--input-ack-timeout-ms", "100"];
    // The daemon keeps a blocked create and answers `prompt_delivered: false`
    // whether or not the caller asked for the acknowledgement.
    let plain: &[&str] = &[];
    for (args, delivered, activity, expected) in [
        (acknowledged, false, Some("blocked"), true),
        (acknowledged, false, Some("idle"), false),
        (acknowledged, false, None, false),
        (acknowledged, true, Some("blocked"), false),
        (plain, false, Some("blocked"), true),
        (plain, false, Some("idle"), false),
        (plain, true, Some("blocked"), false),
    ] {
        let run = run_local(
            args,
            Daemon {
                prompt_delivered: Some(delivered),
                activity,
                ..ACK_DAEMON
            },
        );

        assert!(run.output.status.success(), "{}", run.stdout());
        assert_eq!(
            run.stdout().contains(line),
            expected,
            "args={args:?} delivered={delivered} activity={activity:?}: {}",
            run.stdout()
        );
    }
}

/// A kept create on another host names the session as `<host>/<id>`: a bare id
/// would select the local daemon.
#[cfg(target_os = "linux")]
#[test]
fn a_dialog_line_for_a_remote_session_names_the_host() {
    let env = private_environment();
    let root = env.root().to_path_buf();
    let (library, capture) = connect_redirect::build(&root);
    let status = pin_overlay::status(
        None,
        &[pin_overlay::Peer::new(
            "peer-a.netbird.test",
            "100.64.0.11",
            "key-first",
        )],
    );
    let path = pin_overlay::install(&root.join("bin"), &capture, &status, &status);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let served = Served::default();
    let recorded = Arc::clone(&served);
    let daemon = Daemon {
        prompt_delivered: Some(false),
        activity: Some("blocked"),
        ..ACK_DAEMON
    };
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&recorded);
            thread::spawn(move || serve_connection(stream, &served, daemon));
        }
    });

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &path)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", port.to_string())
        .args([
            "--host",
            "peer-a",
            "session",
            "new",
            "--agent",
            "shell",
            "--project",
            "demo",
            "--input",
            "hello",
            "--yes",
        ])
        .output()
        .expect("run pohunek session new");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "waiting on a dialog: see `pohunek session screen peer-a/s-ack-1`, answer it with `pohunek attach peer-a/s-ack-1`"
        ),
        "{stdout}"
    );
}

#[test]
fn a_start_without_the_flag_reports_no_outcome_and_checks_no_capability() {
    let run = run_local(&["--json"], ACK_DAEMON);

    let document = run.document();
    assert!(
        document["ok"].get("prompt_delivered").is_none(),
        "{document}"
    );
    let params = run.session_new().expect("session.new was sent");
    assert!(params.get("input_ack_timeout_ms").is_none(), "{params}");
    assert!(
        run.served
            .iter()
            .all(|(method, _)| method != protocol::method::HOST_INSPECT),
        "no capability check without the flag: {:?}",
        run.served
    );
}

#[test]
fn a_host_without_the_capability_is_refused_before_a_session_is_created() {
    let run = run_local(
        &["--input-ack-timeout-ms", "500", "--json"],
        Daemon {
            ack_supported: false,
            branch_source_supported: true,
            prompt_delivered: None,
            activity: None,
        },
    );

    let document = run.document();
    assert_eq!(
        document["err"]["code"], "session_new_input_ack_unsupported",
        "{document}"
    );
    assert!(run.session_new().is_none(), "no session was requested");
}

#[test]
fn the_flag_needs_an_initial_input_and_a_value_in_range() {
    let env = private_environment();
    let socket = env.root().join("run/pohunek/daemon.sock");
    let served = serve_local(&socket, ACK_DAEMON);
    for args in [
        vec!["--input-ack-timeout-ms", "100"],
        vec!["--input", "hello", "--input-ack-timeout-ms", "0"],
        vec!["--input", "hello", "--input-ack-timeout-ms", "8001"],
    ] {
        let output = env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .args(["session", "new", "--agent", "shell"])
            .args(&args)
            .output()
            .expect("run pohunek session new");
        assert!(!output.status.success(), "{args:?} must be refused");
    }
    assert!(
        served.lock().expect("served requests").is_empty(),
        "a usage error contacts no daemon"
    );
}

/// Arguments that start the session from `origin/feat` and ask for the
/// acknowledgement, both capability-gated through one `host.inspect`.
const FROM_REMOTE_ACK_ARGS: [&str; 7] = [
    "--repo",
    "/srv/repo",
    "--branch",
    "feat",
    "--from-remote",
    "--input-ack-timeout-ms",
    "250",
];

fn host_inspect_calls(run: &Run) -> usize {
    run.served
        .iter()
        .filter(|(method, _)| method == protocol::method::HOST_INSPECT)
        .count()
}

#[test]
fn from_remote_with_an_acknowledgement_reports_both_outcomes_in_json() {
    let mut args = FROM_REMOTE_ACK_ARGS.to_vec();
    args.push("--json");
    let run = run_local(&args, ACK_DAEMON);

    let document = run.document();
    assert_eq!(document["ok"]["prompt_delivered"], true, "{document}");
    assert_eq!(
        document["ok"]["worktree_source"],
        json!({"ref": "origin/feat", "commit": REMOTE_COMMIT}),
        "{document}"
    );
    let params = run.session_new().expect("session.new was sent");
    assert_eq!(params["branch_source"], "remote");
    assert_eq!(params["input_ack_timeout_ms"], 250);
    assert_eq!(params["input"], "hello");
    assert_eq!(host_inspect_calls(&run), 1, "{:?}", run.served);
    // The gate needs only the capability flags, so it asks the daemon to skip
    // its runtime inventory.
    let inspect = run
        .served
        .iter()
        .find(|(method, _)| method == protocol::method::HOST_INSPECT)
        .expect("host.inspect was sent");
    assert_eq!(inspect.1, json!({"skip_runtime_probes": true}));
}

#[test]
fn from_remote_with_an_acknowledgement_prints_both_outcomes_for_humans() {
    let run = run_local(&FROM_REMOTE_ACK_ARGS, ACK_DAEMON);

    assert!(run.output.status.success(), "{}", run.stdout());
    for line in [
        format!("worktree source: origin/feat at {REMOTE_COMMIT}"),
        "prompt delivered: yes".to_owned(),
    ] {
        assert!(run.stdout().contains(&line), "{line:?} in {}", run.stdout());
    }
    assert_eq!(host_inspect_calls(&run), 1, "{:?}", run.served);
}

#[test]
fn a_host_lacking_either_capability_is_refused_before_a_session_is_created() {
    for (daemon, code) in [
        (
            Daemon {
                branch_source_supported: false,
                ..ACK_DAEMON
            },
            "session_new_branch_source_unsupported",
        ),
        (
            Daemon {
                ack_supported: false,
                ..ACK_DAEMON
            },
            "session_new_input_ack_unsupported",
        ),
    ] {
        let mut args = FROM_REMOTE_ACK_ARGS.to_vec();
        args.push("--json");
        let run = run_local(&args, daemon);

        assert_eq!(run.document()["err"]["code"], code, "{}", run.stdout());
        assert!(run.session_new().is_none(), "no session was requested");
        assert_eq!(host_inspect_calls(&run), 1, "{:?}", run.served);
    }
}
