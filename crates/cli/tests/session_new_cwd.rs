//! `pohunek session new --cwd` against a fixture daemon.
//!
//! The built CLI starts sessions on a fixture daemon that records every
//! request. The scenarios pin what the CLI sends for a requested directory,
//! and that it refuses what the host cannot honor before it creates a session.

#![cfg(unix)]

// Rust guideline compliant 2026-10-22

use std::fs;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
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
const SESSION_ID: &str = "s-cwd-1";

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

fn capabilities(cwd_supported: bool) -> Value {
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
    if cwd_supported {
        value["session_new_cwd_supported"] = json!(true);
    }
    value
}

fn session(cwd: &str) -> Value {
    json!({
        "id": SESSION_ID,
        "agent": "shell",
        "agent_base": "shell",
        "capabilities": {"fork": false, "resume": false},
        "cwd": cwd,
        "pid": 42424,
        "state": "running",
        "state_source": "process",
        "cols": 80,
        "rows": 24,
        "created_at": "2026-07-08T00:00:00Z",
        "updated_at": "2026-07-08T00:01:00Z"
    })
}

fn result_for(request: &Request, cwd_supported: bool) -> Value {
    match request.method() {
        protocol::method::DAEMON_HEALTH => json!({
            "status": "ok",
            "daemon_version": "fixture",
            "protocol_version": PROTOCOL_VERSION.get()
        }),
        protocol::method::HOST_INSPECT => capabilities(cwd_supported),
        protocol::method::SESSION_NEW => {
            let cwd = request.params()["cwd"].as_str().unwrap_or("/project/root");
            session(cwd)
        }
        other => panic!("the CLI sent an unexpected request: {other}"),
    }
}

fn serve_connection<S: Read + Write>(stream: S, served: &Served, cwd_supported: bool) {
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
            result_for(&request, cwd_supported),
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

fn serve_local(socket: &Path, cwd_supported: bool) -> Served {
    let served = Served::default();
    let listener = UnixListener::bind(socket).expect("bind the fixture daemon");
    let recorded = Arc::clone(&served);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&recorded);
            thread::spawn(move || serve_connection(stream, &served, cwd_supported));
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
/// fixture daemon whose `host.inspect` reports `cwd_supported`.
fn run_local(args: &[&str], cwd_supported: bool) -> Run {
    let env = private_environment();
    let root = env.root().to_path_buf();
    let served = serve_local(&root.join("run/pohunek/daemon.sock"), cwd_supported);
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

#[test]
fn a_project_session_sends_the_requested_directory() {
    let run = run_local(&["--project", "demo", "--cwd", "/srv/elsewhere"], true);

    assert_eq!(
        run.document["ok"]["cwd"], "/srv/elsewhere",
        "{}",
        run.document
    );
    let params = run.session_new().expect("session.new was sent");
    assert_eq!(params["cwd"], "/srv/elsewhere");
    assert_eq!(params["project"], "demo");
}

#[test]
fn a_project_session_without_a_directory_sends_no_directory() {
    let run = run_local(&["--project", "demo"], false);

    let params = run.session_new().expect("session.new was sent");
    assert!(
        params.get("cwd").is_none(),
        "the CLI's own directory is not a request: {params}"
    );
    assert!(
        run.served
            .iter()
            .all(|(method, _)| method != protocol::method::HOST_INSPECT),
        "no capability check without a requested directory: {:?}",
        run.served
    );
}

#[test]
fn a_relative_directory_is_made_absolute_against_the_cli_directory() {
    let run = run_local(&["--project", "demo", "--cwd", "nested/dir"], true);

    let params = run.session_new().expect("session.new was sent");
    let sent = PathBuf::from(params["cwd"].as_str().expect("an absolute cwd"));
    assert!(sent.is_absolute(), "{sent:?}");
    assert!(sent.ends_with("nested/dir"), "{sent:?}");
}

/// Runs `session new` without a directory from `dir`, a path below the
/// environment root, against a fixture daemon, and returns the `cwd` it sent.
fn implicit_directory_sent_from(env: &TestEnv, dir: &Path) -> PathBuf {
    implicit_directory_sent_with(env, dir, &[]).0
}

/// [`implicit_directory_sent_from`] with extra environment variables set.
fn implicit_directory_sent_with(
    env: &TestEnv,
    dir: &Path,
    vars: &[(&str, &Path)],
) -> (PathBuf, String) {
    let served = serve_local(&env.root().join("run/pohunek/daemon.sock"), false);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .current_dir(dir)
        .envs(vars.iter().copied())
        .args(["session", "new", "--agent", "shell", "--json"])
        .output()
        .expect("run pohunek session new");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let served = served.lock().expect("served requests").clone();
    let (_, params) = served
        .iter()
        .find(|(method, _)| method == protocol::method::SESSION_NEW)
        .expect("session.new was sent");
    (
        PathBuf::from(params["cwd"].as_str().expect("an implicit cwd")),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn an_implicit_start_inside_a_work_tree_sends_the_checkout_root() {
    let env = private_environment();
    let repo = env.root().join("repo");
    let nested = repo.join("nested");
    fs::create_dir_all(&nested).expect("create the nested directory");
    let init = env
        .command("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(&repo)
        .output()
        .expect("run git init");
    assert!(init.status.success(), "git init failed");

    let sent = implicit_directory_sent_from(&env, &nested);

    assert_eq!(
        sent.canonicalize().expect("canonical sent cwd"),
        repo.canonicalize().expect("canonical repository root"),
        "standing in a subdirectory still starts the session at the checkout root"
    );
}

#[test]
fn an_implicit_start_ignores_git_redirection_variables() {
    let env = private_environment();
    let repo = env.root().join("repo");
    let nested = repo.join("nested");
    let other = env.root().join("other");
    fs::create_dir_all(&nested).expect("create the nested directory");
    fs::create_dir_all(&other).expect("create the other directory");
    let init = env
        .command("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(&repo)
        .output()
        .expect("run git init");
    assert!(init.status.success(), "git init failed");

    let (sent, _) = implicit_directory_sent_with(&env, &nested, &[("GIT_WORK_TREE", &other)]);

    assert_eq!(
        sent.canonicalize().expect("canonical sent cwd"),
        repo.canonicalize().expect("canonical repository root"),
        "a redirecting GIT_WORK_TREE does not move the session out of its checkout"
    );
}

#[test]
fn an_implicit_start_outside_a_work_tree_sends_the_cli_directory() {
    let env = private_environment();
    let plain = env.root().join("plain");
    fs::create_dir_all(&plain).expect("create the plain directory");

    let (sent, stderr) =
        implicit_directory_sent_with(&env, &plain, &[("GIT_CEILING_DIRECTORIES", env.root())]);

    assert_eq!(
        sent.canonicalize().expect("canonical sent cwd"),
        plain.canonicalize().expect("canonical plain directory")
    );
    assert!(
        stderr.is_empty(),
        "outside a work tree is not a failure: {stderr}"
    );
}

#[test]
fn an_implicit_start_warns_when_the_work_tree_root_cannot_be_found() {
    let env = private_environment();
    let repo = env.root().join("repo");
    let nested = repo.join("nested");
    let no_git = env.root().join("no-git");
    fs::create_dir_all(&nested).expect("create the nested directory");
    fs::create_dir_all(&no_git).expect("create the empty PATH directory");
    let init = env
        .command("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(&repo)
        .output()
        .expect("run git init");
    assert!(init.status.success(), "git init failed");

    let (sent, stderr) = implicit_directory_sent_with(&env, &nested, &[("PATH", &no_git)]);

    assert_eq!(
        sent.canonicalize().expect("canonical sent cwd"),
        nested.canonicalize().expect("canonical nested directory"),
        "without git the session starts where the CLI stands"
    );
    assert!(
        stderr.contains("could not find the git work tree root"),
        "{stderr}"
    );
}

#[test]
fn a_daemon_that_ignores_the_directory_is_refused_before_the_session_is_created() {
    let run = run_local(&["--project", "demo", "--cwd", "/srv/elsewhere"], false);

    assert_eq!(
        run.error_code(),
        "session_new_cwd_unsupported",
        "{}",
        run.document
    );
    let message = run.document["err"]["msg"].as_str().unwrap_or_default();
    assert!(message.contains("session_new_cwd_supported"), "{message}");
    assert!(message.contains("daemon"), "{message}");
    assert!(
        run.session_new().is_none(),
        "no session may start in an unrequested directory: {:?}",
        run.served
    );
}

#[test]
fn a_repo_directory_is_refused_by_a_daemon_that_ignores_it() {
    let run = run_local(&["--repo", "/srv/repo", "--cwd", "/srv/elsewhere"], false);

    assert_eq!(
        run.error_code(),
        "session_new_cwd_unsupported",
        "{}",
        run.document
    );
    assert!(run.session_new().is_none(), "{:?}", run.served);
}

#[test]
fn a_directory_without_a_project_is_refused_by_a_daemon_that_ignores_it() {
    let run = run_local(&["--cwd", "/srv/repo/nested"], false);

    assert_eq!(
        run.error_code(),
        "session_new_cwd_unsupported",
        "{}",
        run.document
    );
    assert!(run.session_new().is_none(), "{:?}", run.served);
}

#[test]
fn a_directory_with_a_worktree_is_refused_before_any_connection() {
    let run = run_local(
        &[
            "--project",
            "demo",
            "--branch",
            "feature/x",
            "--cwd",
            "/srv/elsewhere",
        ],
        true,
    );

    assert_eq!(run.error_code(), "cwd_with_worktree", "{}", run.document);
    assert!(run.served.is_empty(), "{:?}", run.served);
}

#[cfg(target_os = "linux")]
mod remote {
    use std::net::{Ipv4Addr, TcpListener};

    use super::*;

    /// Peer the name `peer-a` selects.
    const PEER_IP: &str = "100.64.0.11";

    fn run_remote(args: &[&str]) -> Run {
        let env = private_environment();
        let root = env.root().to_path_buf();
        let (library, capture) = connect_redirect::build(&root);
        let status = pin_overlay::status(
            None,
            &[pin_overlay::Peer::new(
                "peer-a.netbird.test",
                PEER_IP,
                "key-first",
            )],
        );
        let path = pin_overlay::install(&root.join("bin"), &capture, &status, &status);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let served = Served::default();
        let recorded = Arc::clone(&served);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let stream = stream.expect("accept a fixture connection");
                let served = Arc::clone(&recorded);
                thread::spawn(move || serve_connection(stream, &served, true));
            }
        });
        let output = env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .env("PATH", &path)
            .env("LD_PRELOAD", &library)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
            .env("POHUNEK_REMOTE_PORT", port.to_string())
            .args(["--host", "peer-a", "session", "new", "--agent", "shell"])
            .args(args)
            .args(["--yes", "--json"])
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

    #[test]
    fn a_remote_project_session_sends_the_directory_to_the_host() {
        let run = run_remote(&["--project", "demo", "--cwd", "/srv/elsewhere"]);

        let params = run.session_new().expect("session.new reached the host");
        assert_eq!(params["cwd"], "/srv/elsewhere", "{}", run.document);
        assert_eq!(params["project"], "demo");
    }

    #[test]
    fn a_relative_remote_directory_is_refused_before_any_connection() {
        let run = run_remote(&["--project", "demo", "--cwd", "relative/dir"]);

        assert_eq!(
            run.error_code(),
            "remote_cwd_not_absolute",
            "{}",
            run.document
        );
        assert!(run.served.is_empty(), "{:?}", run.served);
    }
}
