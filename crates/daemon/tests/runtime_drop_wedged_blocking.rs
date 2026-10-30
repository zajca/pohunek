//! A blocking task that never returns cannot keep a stopping `pohunekd` alive.
//!
//! The scenario runs the real daemon and wedges a blocking task on its runtime
//! from outside: `project.add` detects the repository with `git rev-parse`, and
//! the repository's `include.path` names a FIFO nobody writes, so git blocks
//! opening its configuration. On `SIGTERM` the daemon must exit while that git
//! process is still blocked, which only a bounded runtime drop allows: an
//! unbounded drop waits for the blocking task, and the daemon's own git
//! detection timeout kills git first, so the daemon would exit after git died.

// Rust guideline compliant 2026-10-30

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use futures::SinkExt as _;
use nix::sys::signal::{kill, Signal};
use nix::unistd::{mkfifo, Pid};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::{guard, wait_until};
use pohunek_test_support::{bin_exe, worker_binary};
use protocol::{method, Request};
use serde_json::json;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};

/// Directory name the daemon uses below the XDG roots.
const APP_DIR: &str = "pohunek";

/// Text of the warning a drop that reached its bound logs.
const DROP_WARNING: &str = "daemon runtime drop reached its bound";

/// Pids of the processes whose command line holds every one of `needles`.
fn processes_running(needles: &[&str]) -> Vec<Pid> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(raw) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw);
        if needles.iter().all(|needle| cmdline.contains(needle)) {
            found.push(Pid::from_raw(pid));
        }
    }
    found
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

/// Kills the git processes the scenario blocked, on every exit of the test;
/// a git blocked opening a FIFO nobody writes would otherwise outlive it.
struct ReapGit(String);

impl Drop for ReapGit {
    fn drop(&mut self) {
        for pid in processes_running(&["rev-parse", &self.0]) {
            let _ = kill(pid, Signal::SIGKILL);
        }
    }
}

fn private_dir(path: &Path) {
    fs::create_dir_all(path).expect("create directory");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("make directory private");
}

#[tokio::test]
async fn a_wedged_blocking_task_does_not_keep_a_stopping_daemon_alive() {
    let env = TestEnv::new().expect("create short test environment");
    let runtime = env.root().join("r");
    let state_home = env.root().join("s");
    let data_home = env.root().join("data");
    private_dir(&data_home.join(APP_DIR));

    let repo: PathBuf = env.root().join("repo");
    fs::create_dir(&repo).expect("create repository directory");
    let init = env
        .command("git")
        .args(["-C"])
        .arg(&repo)
        .args(["init", "-q"])
        .status()
        .expect("run git init");
    assert!(init.success(), "git init");
    let fifo = env.root().join("config.fifo");
    mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).expect("create fifo");
    let configured = env
        .command("git")
        .args(["-C"])
        .arg(&repo)
        .args(["config", "include.path"])
        .arg(&fifo)
        .status()
        .expect("run git config");
    assert!(configured.success(), "git config include.path");

    let mut daemon = env.tokio_command(bin_exe("pohunekd"));
    daemon
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
    let mut daemon = daemon.spawn().expect("spawn daemon");
    let daemon_pid =
        Pid::from_raw(i32::try_from(daemon.id().expect("daemon pid")).expect("pid fits in i32"));

    let socket = runtime.join(APP_DIR).join("daemon.sock");
    let stream = tokio::select! {
        stream = wait_until("the daemon control socket accepts connections", || async {
            UnixStream::connect(&socket).await.ok()
        }) => stream,
        status = daemon.wait() => panic!("daemon exited before readiness: {status:?}"),
    };
    let mut control = Framed::new(stream, LinesCodec::new());
    let request =
        Request::new("wedge", method::PROJECT_ADD, json!({ "path": repo })).expect("valid request");
    control
        .send(serde_json::to_string(&request).expect("serialize request"))
        .await
        .expect("send project.add");

    // The request is in flight on a blocking thread once git is blocked on the FIFO.
    let repo_text = repo.display().to_string();
    let _reap = ReapGit(repo_text.clone());
    wait_until("git blocked on the configuration FIFO", || async {
        processes_running(&["rev-parse", &repo_text])
            .first()
            .copied()
    })
    .await;

    kill(daemon_pid, Signal::SIGTERM).expect("stop the daemon");
    let status = guard("the stopping daemon exits", daemon.wait())
        .await
        .expect("wait for the daemon");

    // Any blocked git counts: the daemon's own detection retries with another
    // git after its timeout kills the first, so the first pid may be gone.
    let still_blocked = !processes_running(&["rev-parse", &repo_text]).is_empty();
    assert!(status.success(), "the daemon exits cleanly: {status:?}");
    assert!(
        still_blocked,
        "the daemon exited after its blocked git ended: the runtime drop waited for it"
    );
    let logs = state_home.join(APP_DIR).join("logs");
    assert!(
        any_file_contains(&logs, DROP_WARNING),
        "the drop warning is in the daemon's log files"
    );
}
