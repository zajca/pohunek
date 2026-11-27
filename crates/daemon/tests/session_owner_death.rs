//! A real `pohunekd` child process of a test root is swept when the test
//! process dies without unwinding.
//!
//! The scenario child below starts the real daemon binary as its own child
//! process, creates one real session, reports the root, the daemon PID and
//! every worker and session leader PID with its start identity, and then
//! blocks. The parent `SIGKILL`s the child the way a hung run is killed and
//! asserts, by PID identity, that no `pohunekd`, no `pohunek-sessiond` worker
//! and no session of the root is left: the reaper `TestEnv` armed for the
//! root swept them.

// Rust guideline compliant 2026-11-27

#![cfg(unix)]

use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::channel;

use futures::{SinkExt as _, StreamExt as _};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::owner::{self, HostInspector, ProcessInspector};
use pohunek_test_support::wait::{poll_until, wait_until, HANG_GUARD};
use pohunek_test_support::workers;
use pohunek_test_support::{bin_exe, worker_binary};
use protocol::{method, ProtocolError, Request, Response};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};

/// Directory name the daemon uses below the XDG roots.
const APP_DIR: &str = "pohunek";

/// Bare-word libtest filter that marks a child run as the owner-death
/// scenario, the `hold::HOLD_MARKER` pattern: under `--exact` it matches no
/// test, so a plain `--ignored` sweep of this binary returns at once.
const SCENARIO_MARKER: &str = "owner-death-scenario";

/// The line the child prints after it reported everything.
const READY_LINE: &str = "ready";

/// The session's program: it idles until it is killed.
const SESSION_PROGRAM: &str = "#!/bin/sh\nexec sleep 300\n";

type Control = Framed<UnixStream, LinesCodec>;

/// Writes one report line of the scenario child and flushes it, so the parent
/// reads it while the child blocks.
fn report(line: &str) {
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{line}").expect("write the scenario report");
    stdout.flush().expect("flush the scenario report");
}

/// Sends one request and returns its success value or typed error.
async fn call(
    control: &mut Control,
    id: &str,
    method_name: &str,
    params: Value,
) -> Result<Value, ProtocolError> {
    let request = Request::new(id, method_name, params).expect("valid request");
    control
        .send(serde_json::to_string(&request).expect("serialize request"))
        .await
        .expect("send request");
    let line = pohunek_test_support::wait::guard("the reply", control.next())
        .await
        .expect("a reply line")
        .expect("reply framing ok");
    serde_json::from_str::<Response>(&line)
        .expect("parse the reply")
        .into_result()
}

/// Creates a private directory.
fn private_dir(path: &Path) {
    fs::create_dir_all(path).expect("create directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("make directory private");
}

/// The scenario child: a real `pohunekd` process with one real session, a
/// report of the root and of every process of the root, and then a block on
/// standard input until the parent kills this process.
#[tokio::test]
#[ignore = "child process of the owner-death scenario"]
async fn scenario_child_serving_daemon() {
    let arguments: Vec<String> = std::env::args().collect();
    if !arguments.iter().any(|argument| argument == SCENARIO_MARKER) {
        return;
    }
    report("child-started");
    let env = TestEnv::new().expect("create the test environment");
    let root = env.root().to_path_buf();
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

    // No `kill_on_drop`: the daemon must survive this process being killed,
    // which is exactly the leak the reaper closes.
    let mut command = env.command(bin_exe("pohunekd"));
    command
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("SHELL", &program)
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .env("POHUNEK_WORKER_BIN", worker_binary())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let daemon = command.spawn().expect("spawn the daemon");
    let daemon_pid = daemon.id();
    // The daemon must outlive this process: the parent kills this process and
    // the reaper sweeps the daemon, so the child is never waited on here.
    std::mem::forget(daemon);
    report(&format!("root={}", root.display()));
    report(&format!("daemon={daemon_pid}"));

    let socket = runtime.join(APP_DIR).join("daemon.sock");
    let stream = wait_until("the daemon control socket accepts connections", || async {
        UnixStream::connect(&socket).await.ok()
    })
    .await;
    let mut control = Framed::new(stream, LinesCodec::new());
    call(
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

    let workers = poll_until("the scenario's worker and session leader to run", || {
        let workers = workers::try_workers_under(&root).ok()?;
        workers
            .iter()
            .any(|worker| !worker.sessions.is_empty())
            .then_some(workers)
    });
    for worker in &workers {
        report(&format!("worker={}:{}", worker.pid, worker.start));
        for session in &worker.sessions {
            report(&format!("session={}:{}", session.pid, session.start));
        }
    }
    report(READY_LINE);
    // Block until the parent ends this process or closes standard input. The
    // daemon child is left running on purpose: the parent kills this process
    // and the reaper must sweep the daemon too.
    let mut stdin = std::io::stdin();
    let mut buffer = [0_u8; 64];
    loop {
        match stdin.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// The parsed report of the scenario child.
struct Report {
    root: PathBuf,
    daemon: u32,
    processes: Vec<(String, u32, String)>,
}

/// Spawns the scenario child and reads its report until [`READY_LINE`].
///
/// A child that never reports fails with the hang guard instead of hanging
/// the test.
fn spawn_scenario_child() -> (Child, Report) {
    let exe = std::env::current_exe().expect("path of the running test binary");
    let [exact, test] =
        pohunek_test_support::exact_test_filter(module_path!(), "scenario_child_serving_daemon");
    // The child's stderr goes to a file, so a child that never reports leaves
    // its panic or hang reason in the parent's failure message.
    let stderr_dir = pohunek_test_support::tempdir().expect("stderr dir for the scenario child");
    let stderr_path = stderr_dir.path().join("stderr");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create the child stderr file");
    let mut child = Command::new(exe)
        .arg(exact)
        .arg(test)
        .arg("--ignored")
        .arg(SCENARIO_MARKER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn the scenario child");
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
    let mut root = None;
    let mut daemon = None;
    let mut processes = Vec::new();
    // The child's report lines reach this pipe live, but its panic text is
    // captured by the test harness and only flushed on exit, so keep the
    // recent lines to show where a child that never reports got stuck.
    let mut recent: Vec<String> = Vec::new();
    loop {
        let line = rx.recv_timeout(HANG_GUARD).unwrap_or_else(|error| {
            let stderr_tail = std::fs::read_to_string(&stderr_path).unwrap_or_default();
            let stdout_tail = recent
                .iter()
                .rev()
                .take(30)
                .rev()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "the scenario child reports before the hang guard: {error}\nchild stdout tail:\n{stdout_tail}\nchild stderr tail: {}",
                stderr_tail.chars().take(2000).collect::<String>()
            )
        });
        if recent.len() >= 50 {
            recent.remove(0);
        }
        recent.push(line.clone());
        if line == READY_LINE {
            break;
        }
        if let Some(path) = line.strip_prefix("root=") {
            root = Some(PathBuf::from(path));
            continue;
        }
        if let Some(pid) = line.strip_prefix("daemon=") {
            daemon = Some(pid.parse().expect("numeric pid"));
            continue;
        }
        for kind in ["worker", "session"] {
            if let Some(rest) = line.strip_prefix(&format!("{kind}=")) {
                let (pid, start) = rest.split_once(':').expect("pid:start");
                processes.push((
                    kind.to_owned(),
                    pid.parse().expect("numeric pid"),
                    start.to_owned(),
                ));
            }
        }
    }
    let report = Report {
        root: root.expect("the child reports its root"),
        daemon: daemon.expect("the child reports its daemon"),
        processes,
    };
    (child, report)
}

/// A test process `SIGKILL`ed while its root runs a real `pohunekd`, one
/// worker and one session leaves no process of its root: the reaper swept the
/// daemon, the worker and the session leader.
#[test]
fn a_sigkilled_scenario_leaves_no_daemon_worker_or_session_of_its_root() {
    let (mut child, report) = spawn_scenario_child();
    child.kill().expect("SIGKILL the scenario child");
    let status = child.wait().expect("wait for the scenario child");
    assert_eq!(
        status.signal(),
        Some(9),
        "the child died of SIGKILL: {status:?}"
    );

    let inspector = HostInspector::new();
    // The daemon is scoped by its working directory under the root.
    poll_until("the killed scenario's pohunekd to end", || {
        matches!(inspector.identity(report.daemon), Ok(None)).then_some(())
    });
    poll_until(
        "no pohunek-sessiond worker of the killed scenario's root is left",
        || {
            workers::try_workers_under(&report.root)
                .ok()
                .filter(Vec::is_empty)
                .map(|_| ())
        },
    );
    for (kind, pid, start) in &report.processes {
        poll_until(
            &format!("the {kind} {pid} of the killed scenario to end"),
            || {
                // The child reported the start identity in the process
                // table's own spelling, so compare it in that spelling.
                let gone = match workers::try_start_of(*pid) {
                    None => true,
                    Some(current) => current != *start,
                };
                gone.then_some(())
            },
        );
    }
    poll_until("no process of the killed scenario's root is left", || {
        owner::try_scoped_identities(&report.root)
            .ok()
            .filter(Vec::is_empty)
            .map(|_| ())
    });
    // The killed child never dropped its root; remove it here.
    let _ = fs::remove_dir_all(&report.root);
}
