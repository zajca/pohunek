//! The processes of a test root end when the test process dies without
//! unwinding.
//!
//! A scenario that starts a real daemon and real sessions stops them only at
//! the end of the scenario; a test process that is killed (`SIGKILL`,
//! `SIGTERM`, nextest termination) or exits without unwinding runs no `Drop`,
//! and the durable `pohunek-sessiond` workers it left keep their sessions
//! alive. `TestEnv` arms a `pohunek-test-reaper` helper for its root, which
//! sweeps the root when the owner lock the test process holds is released by
//! the kernel.
//!
//! Each scenario below re-runs this test binary as a child process that serves
//! a real daemon thread and two real shell sessions (one with a working
//! directory under the root, one outside it), reports the root and the PID and
//! start identity of every worker and session leader, and then blocks. The
//! parent kills the child the way a hung run is killed and asserts, by PID
//! identity, that nothing of the root is left.

// Rust guideline compliant 2026-11-27

#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::channel;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pohunek_client::Client;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::owner;
use pohunek_test_support::process_env::{in_own_process, ProcessEnv};
use pohunek_test_support::wait::{poll_until, HANG_GUARD};
use pohunek_test_support::workers;
use protocol::{method, SessionNewParams, SessionSecretEnv};

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;

/// Bare-word libtest filter that marks a child run as the owner-death
/// scenario, the `hold::HOLD_MARKER` pattern: under `--exact` it matches no
/// test, so a plain `--ignored` sweep of this binary returns at once.
const SCENARIO_MARKER: &str = "owner-death-scenario";

/// Second marker word: the child ends itself with `std::process::exit` after
/// reporting, instead of blocking on standard input.
const EXIT_MARKER: &str = "owner-death-exit";

/// The line the child prints after it reported everything.
const READY_LINE: &str = "ready";

/// Writes one report line of the scenario child and flushes it, so the parent
/// reads it while the child blocks.
fn report(line: &str) {
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{line}").expect("write the scenario report");
    stdout.flush().expect("flush the scenario report");
}

/// Starts one shell session through `client` and returns its id.
async fn start_session(client: &mut Client, cwd: PathBuf, name: &str) -> String {
    let created = client
        .call::<method::SessionNew>(SessionNewParams {
            client_request_id: None,
            agent: "shell".to_owned(),
            name: Some(name.to_owned()),
            cwd: Some(cwd),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            extended_input_ready_wait: None,
            branch_source: None,
            on_diverged: None,
            input_ack_timeout_ms: None,
            metadata: BTreeMap::new(),
            secret_env: SessionSecretEnv::default(),
            context_package_digest: None,
            fetch_ref: None,
            agent_options: BTreeMap::new(),
            skip_repo_hooks: false,
        })
        .await
        .expect("start a session");
    created.session.id.0
}

/// The scenario child: a real daemon served from this process, two shell
/// sessions, a report of the root and of every worker and session leader, and
/// then a block on standard input until the parent kills this process.
#[tokio::test]
#[ignore = "child process of the owner-death scenarios"]
async fn scenario_child_serving_workers() {
    let arguments: Vec<String> = std::env::args().collect();
    if !arguments.iter().any(|argument| argument == SCENARIO_MARKER) {
        return;
    }
    report("child-started");
    let harness = Harness::start().await;
    let root = harness.env.root().to_path_buf();
    report(&format!("root={}", root.display()));
    report(&format!("socket={}", harness.socket().display()));
    // One session with a working directory under the root, one outside it: the
    // session leader of the second is still a descendant of its worker.
    let outside = pohunek_test_support::tempdir().expect("session cwd outside the root");
    report(&format!("outside={}", outside.path().display()));
    let mut client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");
    start_session(&mut client, harness.env.cwd().to_path_buf(), "inside").await;
    start_session(&mut client, outside.path().to_path_buf(), "outside").await;
    let workers = poll_until("the scenario's workers and session leaders to run", || {
        let workers = workers::try_child_workers_under(&root).ok()?;
        let sessions: usize = workers.iter().map(|worker| worker.sessions.len()).sum();
        (sessions >= 2).then_some(workers)
    });
    for worker in &workers {
        report(&format!("worker={}:{}", worker.pid, worker.start));
        for session in &worker.sessions {
            report(&format!("session={}:{}", session.pid, session.start));
        }
    }
    report(READY_LINE);
    if arguments.iter().any(|argument| argument == EXIT_MARKER) {
        std::process::exit(0);
    }
    // Block until the parent ends this process or closes standard input.
    let mut stdin = std::io::stdin();
    let mut buffer = [0_u8; 64];
    loop {
        match stdin.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// A report line of the child, parsed: `root=<path>` or `<kind>=<pid>:<start>`.
struct Report {
    root: PathBuf,
    outside: Option<PathBuf>,
    processes: Vec<(String, u32, String)>,
}

/// Spawns the scenario child and reads its report until [`READY_LINE`].
///
/// A child that never reports fails with the hang guard instead of hanging
/// the test.
fn spawn_scenario_child(extra: &[&str]) -> (Child, Report) {
    let exe = std::env::current_exe().expect("path of the running test binary");
    let [exact, test] =
        pohunek_test_support::exact_test_filter(module_path!(), "scenario_child_serving_workers");
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
        .args(extra)
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
    let mut outside = None;
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
        if let Some(path) = line.strip_prefix("outside=") {
            outside = Some(PathBuf::from(path));
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
        outside,
        processes,
    };
    (child, report)
}

/// Asserts that nothing of the killed scenario's root is left: no worker of
/// the root, no reported process alive under its recorded identity, and no
/// process scoped by the root in the reaper's own criterion.
fn assert_root_reaped(root: &Path, processes: &[(String, u32, String)]) {
    poll_until(
        "no pohunek-sessiond worker of the killed scenario's root is left",
        || {
            workers::try_workers_under(root)
                .ok()
                .filter(Vec::is_empty)
                .map(|_| ())
        },
    );
    for (kind, pid, start) in processes {
        poll_until(
            &format!("the {kind} {pid} of the killed scenario to end"),
            || {
                // The child reported the start identity in the process
                // table's own spelling, so compare it in that spelling.
                let gone = match workers::try_start_of(*pid) {
                    None => true,
                    Some(current) => &current != start,
                };
                gone.then_some(())
            },
        );
    }
    poll_until("no process of the killed scenario's root is left", || {
        owner::try_scoped_identities(root)
            .ok()
            .filter(Vec::is_empty)
            .map(|_| ())
    });
    // The killed child never dropped its root; remove it here.
    let _ = std::fs::remove_dir_all(root);
}

/// Runs the scenario child, ends it the way `end` ends it, asserts the reaper
/// swept the root, and returns the child's exit status.
fn end_and_assert_reaped(end: impl FnOnce(&mut Child), extra: &[&str]) -> ExitStatus {
    let (mut child, report) = spawn_scenario_child(extra);
    end(&mut child);
    let status = child.wait().expect("wait for the scenario child");
    assert_root_reaped(&report.root, &report.processes);
    // The killed child never dropped the outside tempdir; remove it here.
    if let Some(outside) = &report.outside {
        let _ = std::fs::remove_dir_all(outside);
    }
    status
}

/// A test process `SIGKILL`ed with a live daemon thread, two workers and two
/// sessions leaves no worker or session of its root.
#[test]
fn a_sigkilled_scenario_leaves_no_worker_or_session_of_its_root() {
    let status = end_and_assert_reaped(
        |child| {
            child.kill().expect("SIGKILL the scenario child");
        },
        &[],
    );
    assert_eq!(
        status.signal(),
        Some(9),
        "the child died of SIGKILL: {status:?}"
    );
}

/// A test process `SIGTERM`ed, which is what nextest's `terminate-after` sends,
/// leaves no worker or session of its root.
#[test]
fn a_terminated_scenario_leaves_no_worker_or_session_of_its_root() {
    let status = end_and_assert_reaped(
        |child| {
            let pid = Pid::from_raw(i32::try_from(child.id()).expect("pid fits in i32"));
            kill(pid, Signal::SIGTERM).expect("SIGTERM the scenario child");
        },
        &[],
    );
    assert_eq!(
        status.signal(),
        Some(15),
        "the child died of SIGTERM: {status:?}"
    );
}

/// A test process that exits without unwinding (`std::process::exit`) leaves
/// no worker or session of its root.
#[test]
fn an_exiting_scenario_leaves_no_worker_or_session_of_its_root() {
    let status = end_and_assert_reaped(|_| {}, &[EXIT_MARKER]);
    assert_eq!(status.code(), Some(0), "the child exited without unwinding");
}

/// A missing reaper binary fails `TestEnv::new` fast, naming the build command.
#[test]
fn a_missing_reaper_binary_fails_testenv_creation_fast() {
    if !in_own_process() {
        return;
    }
    let mut environment = ProcessEnv::lock();
    environment.set("POHUNEK_REAPER_BIN", "/nonexistent/pohunek-test-reaper");
    let error = TestEnv::new().expect_err("a missing reaper binary fails TestEnv::new");
    let message = format!("{error}");
    assert!(
        message.contains("cargo build -p pohunek-test-support --bin pohunek-test-reaper"),
        "{message}"
    );
}
