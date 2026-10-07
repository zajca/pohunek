//! Reaping of `pohunek-sessiond` workers that a fixture root owns.
//!
//! A real daemon starts its session workers in their own process groups and
//! lets them outlive it, so a test that kills the daemon child leaves each
//! worker running, reparented to the init process, with a working directory
//! that no longer exists. [`WorkerGuard`] closes that gap for one fixture root:
//! when it drops, on success and while a panic unwinds, it terminates every
//! worker whose control socket lives below the root and fails the test if there
//! was one.
//!
//! A worker that is a direct child of the test process is not a leak: an
//! in-process registry with a `SubprocessWorkerLauncher` kills and reaps its
//! workers when its runtime drops, which can be after the fixture root does.
//! Only workers of another parent are reported, which in practice are those a
//! killed daemon child left behind.
//!
//! The root is the only marker a worker carries. The subprocess launcher starts
//! workers with a cleared environment, so no variable of the test process
//! reaches them, but their `--daemon-socket-path` argument names a path inside
//! the root. Workers of the host, of another test and of another checkout never
//! match, which keeps the guard safe to run beside live sessions.
//!
//! [`TestEnv`](crate::env::TestEnv) owns a guard for its root. A test that
//! builds its own root, for example with [`crate::tempdir_with_prefix`], wraps
//! it with [`WorkerGuard::watch`] and declares the guard after the root so the
//! guard drops first.
//!
//! # Examples
//!
//! ```
//! use pohunek_test_support::workers::WorkerGuard;
//!
//! let root = pohunek_test_support::tempdir()?;
//! let guard = WorkerGuard::watch(root.path());
//! assert!(guard.survivors().is_empty());
//! # Ok::<(), std::io::Error>(())
//! ```

// Rust guideline compliant 2026-06-26

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use crate::wait::POLL_INTERVAL;

/// File name of the session worker executable.
const WORKER_EXECUTABLE: &str = "pohunek-sessiond";

/// Argument that carries the daemon socket path of a worker.
const SOCKET_PATH_ARGUMENT: &str = "--daemon-socket-path";

/// How long workers get to exit after `SIGTERM` before they are killed.
///
/// A worker that is idle exits on `SIGTERM` within a few poll intervals; the
/// window only has to outlast a loaded host, and expiring it costs nothing
/// but the escalation to `SIGKILL`.
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// How long the process table may keep listing a worker after `SIGKILL`.
///
/// `SIGKILL` cannot be handled, so the process leaves the table as soon as the
/// kernel tears it down; the bound only keeps an unkillable process from
/// hanging the test.
const KILL_SETTLE: Duration = Duration::from_secs(30);

/// A worker process found in the process table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerProcess {
    /// Process ID of the worker.
    pub pid: u32,
    /// Process ID of the worker's parent.
    pub parent_pid: u32,
    /// Value of the worker's `--daemon-socket-path` argument.
    pub socket_path: PathBuf,
}

/// Parses one `ps -o pid=,ppid=,args=` line into a worker, or `None` for any
/// other process.
///
/// The executable is matched by file name, so a worker started from any
/// checkout counts; the root check is the caller's.
fn parse_worker_line(line: &str) -> Option<WorkerProcess> {
    let mut tokens = line.split_whitespace();
    let pid = tokens.next()?.parse().ok()?;
    let parent_pid = tokens.next()?.parse().ok()?;
    let executable = Path::new(tokens.next()?);
    if executable.file_name()? != WORKER_EXECUTABLE {
        return None;
    }
    let socket_path = tokens
        .skip_while(|token| *token != SOCKET_PATH_ARGUMENT)
        .nth(1)?;
    Some(WorkerProcess {
        pid,
        parent_pid,
        socket_path: PathBuf::from(socket_path),
    })
}

/// Returns the workers of `table`, the output of `ps -o pid=,ppid=,args=`,
/// whose socket path is below `root`.
///
/// The comparison is per path component, so `/tmp/pw-s-ab` does not own a
/// worker of `/tmp/pw-s-abc`.
#[must_use]
pub fn workers_in_table(table: &str, root: &Path) -> Vec<WorkerProcess> {
    table
        .lines()
        .filter_map(parse_worker_line)
        .filter(|worker| worker.socket_path.starts_with(root))
        .collect()
}

/// Lists the workers whose socket path is below `root` and whose parent is not
/// this process, from the live process table.
///
/// # Panics
///
/// Panics when `ps` cannot be run or does not exit successfully, since no
/// leak check can be made without the process table.
#[must_use]
pub fn workers_under(root: &Path) -> Vec<WorkerProcess> {
    // `-ww` lifts the column cap of macOS `ps`; Linux `ps` writing to a pipe
    // never truncates.
    let output = Command::new("ps")
        .args(["-axww", "-o", "pid=,ppid=,args="])
        .output()
        .unwrap_or_else(|error| panic!("cannot run `ps` to look for leaked workers: {error}"));
    assert!(
        output.status.success(),
        "`ps` failed while looking for leaked workers: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let own_pid = std::process::id();
    workers_in_table(&String::from_utf8_lossy(&output.stdout), root)
        .into_iter()
        .filter(|worker| worker.parent_pid != own_pid)
        .collect()
}

/// Sends `signal` to `pid`; a process that already exited is fine.
fn send(pid: u32, signal: Signal) {
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    // Any other failure (for example `EPERM` for a recycled PID of another
    // user) leaves the process in the table, and the caller's rescan reports it.
    let _ = kill(Pid::from_raw(raw), signal);
}

/// Waits until no worker is below `root`, up to `limit`; returns whether none is.
fn wait_for_none(root: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if workers_under(root).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Terminates every worker below `root` and returns those that were running.
///
/// Sends `SIGTERM`, then `SIGKILL` to whatever is still listed after
/// [`TERMINATE_GRACE`], and returns once the process table shows none or after
/// [`KILL_SETTLE`].
#[must_use]
pub fn reap_workers_under(root: &Path) -> Vec<WorkerProcess> {
    let found = workers_under(root);
    if found.is_empty() {
        return found;
    }
    for worker in &found {
        send(worker.pid, Signal::SIGTERM);
    }
    if !wait_for_none(root, TERMINATE_GRACE) {
        for worker in workers_under(root) {
            send(worker.pid, Signal::SIGKILL);
        }
        let _ = wait_for_none(root, KILL_SETTLE);
    }
    found
}

/// Terminates the workers below one fixture root when it drops, and fails the
/// test if there were any.
///
/// Reaping happens on success and while a panic unwinds. The failure is raised
/// only when the thread is not already panicking, so the original assertion is
/// the one that is reported.
#[derive(Debug)]
pub struct WorkerGuard {
    root: PathBuf,
}

impl WorkerGuard {
    /// Watches workers whose control socket is below `root`.
    ///
    /// `root` must be the canonical path the daemon under test was given, since
    /// the match is on the path as it appears in the worker's arguments.
    #[must_use]
    pub fn watch(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the workers below the root that are running now.
    ///
    /// # Panics
    ///
    /// Panics when the process table cannot be read.
    #[must_use]
    pub fn survivors(&self) -> Vec<WorkerProcess> {
        workers_under(&self.root)
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let leaked = reap_workers_under(&self.root);
        if leaked.is_empty() || std::thread::panicking() {
            return;
        }
        let pids = leaked
            .iter()
            .map(|worker| worker.pid.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        panic!(
            "test left {} pohunek-sessiond worker(s) running below {} (pids {pids}); they were terminated. \
             Stop the daemon with `session.remove` for every session it created, or end its workers before the test returns",
            leaked.len(),
            self.root.display(),
        );
    }
}

#[cfg(test)]
mod tests {
    use nix::sys::signal::Signal;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::CommandExt as _;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    use super::{
        parse_worker_line, reap_workers_under, workers_in_table, workers_under, WorkerGuard,
        WorkerProcess,
    };
    use crate::tempdir;
    use crate::wait::poll_until;

    const WORKER: &str = "/w/target/debug/pohunek-sessiond --session-id s-1 --worker-generation g \
                          --daemon-socket-path /tmp/pw-s-abc/r/pohunek/daemon.sock";

    /// Shell body of a stand-in worker: idles until `SIGTERM`, then ends its
    /// child and exits.
    const FAKE_WORKER_SCRIPT: &str = "sleep 120 & child=$!; trap 'kill $child' TERM; wait";

    /// Shell body that starts a stand-in worker in the background and returns,
    /// so the worker's parent is no longer this process: `$0` is the worker
    /// executable, `$1` its script, `$2` its socket path.
    const DETACH_SCRIPT: &str =
        "( \"$0\" -c \"$1\" --daemon-socket-path \"$2\" ) >/dev/null 2>&1 &";

    /// Starts a stand-in worker that is a direct child of this process, with
    /// its control socket below `root`.
    fn spawn_child_worker(root: &Path) -> Child {
        Command::new("/bin/sh")
            .arg0("/fake/target/debug/pohunek-sessiond")
            .args(["-c", FAKE_WORKER_SCRIPT, "--daemon-socket-path"])
            .arg(root.join("r/pohunek/daemon.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn stand-in worker")
    }

    /// Starts a stand-in worker whose parent has exited, as a daemon's worker is
    /// after the daemon is killed, and returns its PID once the process table
    /// lists it.
    fn spawn_orphaned_worker(root: &Path) -> u32 {
        let executable = root.join("pohunek-sessiond");
        symlink("/bin/sh", &executable).expect("name the stand-in worker executable");
        Command::new("/bin/sh")
            .args(["-c", DETACH_SCRIPT])
            .arg(&executable)
            .arg(FAKE_WORKER_SCRIPT)
            .arg(root.join("r/pohunek/daemon.sock"))
            .status()
            .expect("detach stand-in worker");
        poll_until("the detached stand-in worker appears", || {
            workers_under(root).first().map(|worker| worker.pid)
        })
    }

    #[test]
    fn a_worker_line_yields_its_pids_and_socket_path() {
        let worker = parse_worker_line(&format!("  4242 7 {WORKER}")).expect("worker line");
        assert_eq!(
            worker,
            WorkerProcess {
                pid: 4242,
                parent_pid: 7,
                socket_path: "/tmp/pw-s-abc/r/pohunek/daemon.sock".into(),
            }
        );
    }

    #[test]
    fn other_processes_and_a_worker_without_a_socket_are_ignored() {
        assert_eq!(parse_worker_line("1 1 /usr/bin/pohunekd --foo"), None);
        assert_eq!(
            parse_worker_line("2 1 /w/pohunek-sessiond --session-id s-1"),
            None
        );
        assert_eq!(
            parse_worker_line("3 1 grep pohunek-sessiond --daemon-socket-path /tmp/x"),
            None
        );
        assert_eq!(parse_worker_line("not-a-pid 1 /w/pohunek-sessiond"), None);
    }

    #[test]
    fn a_root_owns_only_the_workers_below_it() {
        let table = format!(
            "10 1 {WORKER}\n11 1 {}\n12 1 /usr/bin/sleep 1\n",
            WORKER.replace("pw-s-abc", "pw-s-abcd")
        );
        let owned = workers_in_table(&table, Path::new("/tmp/pw-s-abc"));
        assert_eq!(owned.iter().map(|w| w.pid).collect::<Vec<_>>(), [10]);
        assert!(workers_in_table(&table, Path::new("/tmp/pw-s-ab")).is_empty());
    }

    #[test]
    fn a_direct_child_worker_is_not_a_leak() {
        let root = tempdir().expect("root");
        let mut worker = spawn_child_worker(root.path());
        let guard = WorkerGuard::watch(root.path());
        assert!(guard.survivors().is_empty());
        drop(guard);
        super::send(worker.id(), Signal::SIGTERM);
        worker.wait().expect("reap the child worker");
    }

    #[test]
    fn reaping_ends_the_workers_of_one_root_and_leaves_other_roots_alone() {
        let (owned, other) = (
            tempdir().expect("owned root"),
            tempdir().expect("other root"),
        );
        let mine = spawn_orphaned_worker(owned.path());
        let theirs = spawn_orphaned_worker(other.path());

        let reaped = reap_workers_under(owned.path());
        assert_eq!(reaped.iter().map(|w| w.pid).collect::<Vec<_>>(), [mine]);
        assert!(workers_under(owned.path()).is_empty());

        let survivors = workers_under(other.path());
        assert_eq!(
            survivors.iter().map(|w| w.pid).collect::<Vec<_>>(),
            [theirs]
        );
        assert_eq!(reap_workers_under(other.path()).len(), 1);
    }

    #[test]
    fn a_guard_that_reaps_a_worker_fails_the_test_naming_it() {
        let root = tempdir().expect("root");
        let pid = spawn_orphaned_worker(root.path());
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _guard = WorkerGuard::watch(root.path());
        }));
        let payload = outcome.expect_err("a leaked worker fails the test");
        let message = payload.downcast_ref::<String>().expect("formatted panic");
        assert!(message.contains(&pid.to_string()), "{message}");
        assert!(message.contains("pohunek-sessiond"), "{message}");
        assert!(workers_under(root.path()).is_empty());
    }

    #[test]
    fn a_guard_without_workers_is_silent() {
        let root = tempdir().expect("root");
        drop(WorkerGuard::watch(root.path()));
    }

    #[test]
    fn a_guard_reaps_during_unwinding_and_keeps_the_original_failure() {
        let root = tempdir().expect("root");
        spawn_orphaned_worker(root.path());
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _guard = WorkerGuard::watch(root.path());
            panic!("original failure");
        }));
        let payload = outcome.expect_err("the closure panics");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"original failure"));
        assert!(workers_under(root.path()).is_empty());
    }
}
