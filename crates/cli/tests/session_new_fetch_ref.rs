//! `pohunek session new --fetch-ref/--fetch-commit` against a fixture daemon.
//!
//! The built CLI starts sessions on a fixture daemon that records every
//! request. The scenarios pin what the CLI sends for the extra ref request,
//! what it prints of the fetched ref, and that it refuses what the host
//! cannot honor or the options forbid before it creates a session.

#![cfg(unix)]

// Rust guideline compliant 2026-11-12

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

/// Mode of the runtime and state directories the client expects private.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Id of the session the fixture daemon reports as created.
const SESSION_ID: &str = "s-fetch-1";

/// Extra ref the fixture daemon reports as fetched.
const SOURCE_REF: &str = "refs/pull/7/head";

/// Commit the fixture daemon reports the extra ref peeled to.
const FETCH_COMMIT: &str = "3333333333333333333333333333333333333333";

/// Daemon-chosen destination ref the fixture daemon reports.
const FETCHED_REF: &str = "refs/pohunek/sessions/s-fetch-1/fetched";

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

/// `client_request_id` keys the fixture daemon has seen, as the real daemon
/// remembers them: the first `session.new` under a key creates the session,
/// a repeat returns it with `replayed: true` and none of the first
/// response's outcomes.
#[derive(Default)]
struct Keys {
    seen: Mutex<HashSet<String>>,
}

/// What the fixture daemon advertises and answers.
#[derive(Clone, Copy)]
struct Daemon {
    fetch_ref_supported: bool,
    /// When false, the daemon advertises the capability but omits
    /// `fetched_ref` from the first response (a contract break).
    report_outcome: bool,
}

/// A daemon that advertises the capability and reports the outcome.
const SUPPORTING: Daemon = Daemon {
    fetch_ref_supported: true,
    report_outcome: true,
};

/// A daemon that predates the extra ref and would ignore it.
const IGNORING: Daemon = Daemon {
    fetch_ref_supported: false,
    report_outcome: true,
};

/// A daemon that advertises the capability but omits the outcome.
const SILENT: Daemon = Daemon {
    fetch_ref_supported: true,
    report_outcome: false,
};

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
        "session_wait_supported": true,
        // The replay scenario needs keyed creations; the refusal scenario
        // below predates only the extra ref, never the key.
        "session_new_request_id_supported": true
    });
    if daemon.fetch_ref_supported {
        value["session_new_fetch_ref_supported"] = json!(true);
    }
    value
}

fn session_new_result(request: &Request, daemon: Daemon, keys: &Keys) -> Value {
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
        "updated_at": "2026-07-08T00:01:00Z"
    });
    let replayed = request.params()["client_request_id"]
        .as_str()
        .is_some_and(|key| !keys.seen.lock().expect("keys").insert(key.to_owned()));
    if replayed {
        // A replay carries neither the first response's `fetched_ref` nor
        // `replayed: false`: only the marker.
        value["replayed"] = json!(true);
        return value;
    }
    if daemon.fetch_ref_supported
        && daemon.report_outcome
        && request.params().get("fetch_ref").is_some()
    {
        value["fetched_ref"] = json!({
            "ref": FETCHED_REF,
            "source_ref": SOURCE_REF,
            "commit": FETCH_COMMIT
        });
    }
    value
}

fn result_for(request: &Request, daemon: Daemon, keys: &Keys) -> Value {
    match request.method() {
        protocol::method::DAEMON_HEALTH => json!({
            "status": "ok",
            "daemon_version": "fixture",
            "protocol_version": PROTOCOL_VERSION.get()
        }),
        protocol::method::HOST_INSPECT => capabilities(daemon),
        protocol::method::SESSION_NEW => session_new_result(request, daemon, keys),
        other => panic!("the CLI sent an unexpected request: {other}"),
    }
}

fn serve_connection<S: Read + Write>(stream: S, served: &Served, daemon: Daemon, keys: &Keys) {
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
        let response = Response::ok(
            PROTOCOL_VERSION,
            request.id(),
            result_for(&request, daemon, keys),
        )
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
    serve_local_with_keys(socket, daemon, Arc::new(Keys::default()))
}

fn serve_local_with_keys(socket: &Path, daemon: Daemon, keys: Arc<Keys>) -> Served {
    let served = Served::default();
    let listener = UnixListener::bind(socket).expect("bind the fixture daemon");
    let recorded = Arc::clone(&served);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&recorded);
            let keys = Arc::clone(&keys);
            thread::spawn(move || serve_connection(stream, &served, daemon, &keys));
        }
    });
    served
}

/// Result of one `pohunek session new` run.
struct Run {
    document: Value,
    served: Vec<(String, Value)>,
}

impl Run {
    /// The params of the `session.new` request the daemon received, if any.
    fn session_new(&self) -> Option<&Value> {
        self.served
            .iter()
            .find(|(method, _)| method == protocol::method::SESSION_NEW)
            .map(|(_, params)| params)
    }

    fn error_code(&self) -> &str {
        self.document["err"]["code"].as_str().unwrap_or_default()
    }
}

/// Runs `pohunek session new --agent shell <args> --json` against a local
/// fixture daemon.
fn run_local(args: &[&str], daemon: Daemon) -> Run {
    let env = private_environment();
    let root = env.root().to_path_buf();
    let served = serve_local(&root.join("run/pohunek/daemon.sock"), daemon);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["session", "new", "--agent", "shell"])
        .args(args)
        .arg("--json")
        .output()
        .expect("run pohunek session new");
    let document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "one JSON document ({error}): stdout {}; stderr {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let served = served.lock().expect("served requests").clone();
    Run { document, served }
}

/// Runs `pohunek session new --agent shell <args>` (no `--json`) against a
/// fixture daemon and returns the process output with the requests served.
fn run_plain(args: &[&str]) -> (std::process::Output, Vec<(String, Value)>) {
    let env = private_environment();
    let root = env.root().to_path_buf();
    let served = serve_local(&root.join("run/pohunek/daemon.sock"), SUPPORTING);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["session", "new", "--agent", "shell"])
        .args(args)
        .output()
        .expect("run pohunek session new");
    let served = served.lock().expect("served requests").clone();
    (output, served)
}

fn fetch_ref_args() -> Vec<&'static str> {
    vec![
        "--repo",
        "/srv/repo",
        "--branch",
        "feat",
        "--fetch-ref",
        SOURCE_REF,
        "--fetch-commit",
        FETCH_COMMIT,
    ]
}

#[test]
fn fetch_ref_sends_the_extra_ref_and_reports_it_in_json() {
    let args = fetch_ref_args();
    let run = run_local(&args, SUPPORTING);

    let params = run.session_new().expect("session.new was sent");
    assert_eq!(
        params["fetch_ref"],
        json!({"source_ref": SOURCE_REF, "commit": FETCH_COMMIT}),
        "{params}"
    );
    assert_eq!(
        run.document["ok"]["fetched_ref"],
        json!({
            "ref": FETCHED_REF,
            "source_ref": SOURCE_REF,
            "commit": FETCH_COMMIT
        }),
        "{}",
        run.document
    );
    let inspect = run
        .served
        .iter()
        .find(|(method, _)| method == protocol::method::HOST_INSPECT)
        .expect("host.inspect was sent");
    assert_eq!(inspect.1, json!({"skip_runtime_probes": true}));
}

#[test]
fn fetch_ref_prints_the_destination_for_humans() {
    let args = fetch_ref_args();
    let (output, _) = run_plain(&args);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "fetched: {SOURCE_REF} -> {FETCHED_REF} at {FETCH_COMMIT}"
        )),
        "missing the fetched ref line in {stdout}"
    );
}

#[test]
fn fetch_ref_needs_its_commit_and_a_branch() {
    for args in [
        vec![
            "--repo",
            "/srv/repo",
            "--branch",
            "feat",
            "--fetch-ref",
            SOURCE_REF,
        ],
        vec![
            "--repo",
            "/srv/repo",
            "--branch",
            "feat",
            "--fetch-commit",
            FETCH_COMMIT,
        ],
        vec![
            "--repo",
            "/srv/repo",
            "--fetch-ref",
            SOURCE_REF,
            "--fetch-commit",
            FETCH_COMMIT,
        ],
    ] {
        let (output, served) = run_plain(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?} must be refused");
        assert!(served.is_empty(), "{args:?} reached the daemon");
    }
}

#[test]
fn fetch_ref_is_refused_by_a_daemon_that_ignores_it() {
    let args = fetch_ref_args();
    let run = run_local(&args, IGNORING);

    assert_eq!(
        run.error_code(),
        "session_new_fetch_ref_unsupported",
        "{}",
        run.document
    );
    assert_eq!(run.document["err"]["class"], "daemon", "{}", run.document);
    assert!(
        run.document["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("fetch_ref")),
        "{}",
        run.document
    );
    assert!(
        run.document["err"]["recover"]
            .as_str()
            .is_some_and(|recover| recover.contains("upgrade the daemon")),
        "{}",
        run.document
    );
    assert!(run.session_new().is_none(), "no session.new was sent");
}

#[test]
fn a_replay_reports_no_fetched_ref_and_stays_green() {
    let keys = Arc::new(Keys::default());
    let env = private_environment();
    let root = env.root().to_path_buf();
    let served = serve_local_with_keys(
        &root.join("run/pohunek/daemon.sock"),
        SUPPORTING,
        Arc::clone(&keys),
    );
    let session_new = |extra: &[&str]| {
        let mut args = fetch_ref_args();
        args.extend(["--request-id", "fetch-1", "--json"]);
        args.extend(extra.iter().copied());
        env.command(pohunek_test_support::bin_exe("pohunek"))
            .args(["session", "new", "--agent", "shell"])
            .args(&args)
            .output()
            .expect("run pohunek session new")
    };
    let first = session_new(&[]);
    let second = session_new(&[]);

    assert!(first.status.success(), "{first:?}");
    assert!(second.status.success(), "{second:?}");
    let (first, second) = (
        serde_json::from_slice::<Value>(&first.stdout).expect("one JSON document"),
        serde_json::from_slice::<Value>(&second.stdout).expect("one JSON document"),
    );
    assert!(
        first["ok"].get("replayed").is_none() && first["ok"].get("fetched_ref").is_some(),
        "{first}"
    );
    assert_eq!(second["ok"]["replayed"], true, "{second}");
    assert!(
        second["ok"].get("fetched_ref").is_none() && second.get("err").is_none(),
        "{second}"
    );
    let served = served.lock().expect("served requests").clone();
    let creates: Vec<&Value> = served
        .iter()
        .filter(|(method, _)| method == protocol::method::SESSION_NEW)
        .map(|(_, params)| params)
        .collect();
    assert_eq!(creates.len(), 2, "{creates:?}");
    assert!(
        creates
            .iter()
            .all(|params| params["client_request_id"] == "fetch-1"),
        "{creates:?}"
    );
}

#[test]
fn a_missing_fetched_outcome_fails_loudly() {
    let args = fetch_ref_args();
    let run = run_local(&args, SILENT);

    assert_eq!(
        run.error_code(),
        "fetch_ref_outcome_missing",
        "{}",
        run.document
    );
    assert_eq!(
        run.document["err"]["details"]["session_id"], SESSION_ID,
        "{}",
        run.document
    );
    assert!(
        run.session_new().is_some(),
        "the session was created; only the outcome report is unusable"
    );
}
