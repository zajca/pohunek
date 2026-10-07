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
//! The process table is read without running any program: on Linux from
//! `/proc` (NUL-separated `cmdline`, so arguments with spaces stay intact), on
//! other Unix systems from the absolute `/bin/ps`. Neither depends on the
//! mutable `PATH` of the test process, which other tests replace. Each worker
//! is recorded with its start time, and a signal is sent only after the start
//! time of that PID is verified again (through a pidfd on Linux), so a PID that
//! was recycled for an unrelated process is never signalled.
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

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// Absolute path of `ps` on systems without `/proc`.
///
/// Absolute so that the lookup never goes through the test process's `PATH`.
#[cfg(not(target_os = "linux"))]
const PS_PROGRAM: &str = "/bin/ps";

/// A worker process found in the process table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerProcess {
    /// Process ID of the worker.
    pub pid: u32,
    /// Process ID of the worker's parent.
    pub parent_pid: u32,
    /// Opaque start time of the process; a recycled PID has a different one.
    pub start: String,
    /// Value of the worker's `--daemon-socket-path` argument.
    pub socket_path: PathBuf,
}

/// One process of the process table, before it is matched against a root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessEntry {
    pid: u32,
    parent_pid: u32,
    start: String,
    /// Target of the executable link, when it can be read.
    executable: Option<PathBuf>,
    argv: Vec<OsString>,
}

/// Signal the guard sends to a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// `SIGTERM`: lets the worker end its agent and exit.
    Terminate,
    /// `SIGKILL`: for a worker that ignored `Terminate`.
    Kill,
}

/// Returns the worker `entry` describes, or `None` for any other process.
///
/// The executable counts as a worker by the file name of `argv[0]` or of its
/// link target, so a worker started from any checkout matches and one whose
/// binary was replaced on disk still does. The socket path is the argument that
/// follows [`SOCKET_PATH_ARGUMENT`], taken whole so a path with spaces stays one
/// path.
fn as_worker(entry: &ProcessEntry) -> Option<WorkerProcess> {
    let named = |path: &Path| {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.trim_end_matches(" (deleted)") == WORKER_EXECUTABLE)
    };
    let is_worker = entry
        .argv
        .first()
        .is_some_and(|arg0| named(Path::new(arg0)))
        || entry.executable.as_deref().is_some_and(named);
    if !is_worker {
        return None;
    }
    let position = entry
        .argv
        .iter()
        .position(|argument| argument == SOCKET_PATH_ARGUMENT)?;
    let socket_path = PathBuf::from(entry.argv.get(position + 1)?);
    Some(WorkerProcess {
        pid: entry.pid,
        parent_pid: entry.parent_pid,
        start: entry.start.clone(),
        socket_path,
    })
}

/// Returns the workers among `entries` whose socket path is below `root`.
///
/// The comparison is per path component, so `/tmp/pw-s-ab` does not own a
/// worker of `/tmp/pw-s-abc`.
fn workers_among(entries: &[ProcessEntry], root: &Path) -> Vec<WorkerProcess> {
    entries
        .iter()
        .filter_map(as_worker)
        .filter(|worker| worker.socket_path.starts_with(root))
        .collect()
}

/// Extracts the parent PID and the start time from the contents of `/proc/<pid>/stat`.
///
/// The command name is parenthesised and may itself contain spaces and
/// parentheses, so the fields are counted from the last closing parenthesis.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(stat: &str) -> Option<(u32, String)> {
    /// Index of the parent PID after the command name (field 4 of `proc(5)`).
    const PARENT_FIELD: usize = 1;
    /// Index of the start time after the command name (field 22 of `proc(5)`).
    const START_FIELD: usize = 19;
    let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    Some((
        fields.get(PARENT_FIELD)?.parse().ok()?,
        (*fields.get(START_FIELD)?).to_owned(),
    ))
}

/// Reads the process table from `/proc`.
#[cfg(target_os = "linux")]
fn process_table() -> io::Result<Vec<ProcessEntry>> {
    use std::os::unix::ffi::OsStringExt as _;

    let mut entries = Vec::new();
    for directory in std::fs::read_dir("/proc")? {
        let directory = directory?;
        let Some(pid) = directory
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        // A process may exit between the listing and the reads; it is then
        // simply not part of the table.
        let (Ok(stat_line), Ok(cmdline)) = (
            std::fs::read_to_string(directory.path().join("stat")),
            std::fs::read(directory.path().join("cmdline")),
        ) else {
            continue;
        };
        let Some((parent_pid, start)) = parse_stat(&stat_line) else {
            continue;
        };
        let argv = cmdline
            .split_inclusive(|byte| *byte == 0)
            .map(|argument| {
                OsString::from_vec(argument.strip_suffix(&[0]).unwrap_or(argument).to_vec())
            })
            .collect();
        entries.push(ProcessEntry {
            pid,
            parent_pid,
            start,
            executable: std::fs::read_link(directory.path().join("exe")).ok(),
            argv,
        });
    }
    Ok(entries)
}

/// Reads the process table from `ps`, whose output has lost argument
/// boundaries: a path with spaces splits into several arguments there.
#[cfg(not(target_os = "linux"))]
fn process_table() -> io::Result<Vec<ProcessEntry>> {
    /// Whitespace-separated words of `ps -o lstart=` ("Mon Oct  7 22:06:00 2026").
    const START_WORDS: usize = 5;
    let output = std::process::Command::new(PS_PROGRAM)
        .args(["-axww", "-o", "pid=,ppid=,lstart=,args="])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "`{PS_PROGRAM}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let table = String::from_utf8_lossy(&output.stdout);
    Ok(table
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let parent_pid = words.next()?.parse().ok()?;
            let start = words
                .by_ref()
                .take(START_WORDS)
                .collect::<Vec<_>>()
                .join(" ");
            Some(ProcessEntry {
                pid,
                parent_pid,
                start,
                executable: None,
                argv: words.map(OsString::from).collect(),
            })
        })
        .collect())
}

/// Returns the current start time of `pid`, or `None` when it is gone.
#[cfg(target_os = "linux")]
fn start_of(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&stat).map(|(_, start)| start)
}

/// Returns the current start time of `pid`, or `None` when it is gone.
#[cfg(not(target_os = "linux"))]
fn start_of(pid: u32) -> Option<String> {
    process_table()
        .ok()?
        .into_iter()
        .find(|entry| entry.pid == pid)
        .map(|entry| entry.start)
}

/// Lists the workers whose socket path is below `root` and whose parent is not
/// this process.
///
/// # Errors
///
/// Returns the I/O error when the process table cannot be read.
pub fn try_workers_under(root: &Path) -> io::Result<Vec<WorkerProcess>> {
    let own_pid = std::process::id();
    Ok(workers_among(&process_table()?, root)
        .into_iter()
        .filter(|worker| worker.parent_pid != own_pid)
        .collect())
}

/// Lists the workers whose socket path is below `root` and whose parent is not
/// this process.
///
/// # Panics
///
/// Panics when the process table cannot be read, since no leak check can be
/// made without it.
#[must_use]
pub fn workers_under(root: &Path) -> Vec<WorkerProcess> {
    try_workers_under(root).unwrap_or_else(|error| {
        panic!("cannot read the process table to look for leaked workers: {error}")
    })
}

/// Sends `stop` to `worker` only if the process with its PID still has the
/// start time recorded for it; returns whether a signal was sent.
///
/// On Linux a pidfd is opened first and the start time is checked afterwards,
/// so the signal goes to the process that was verified even if the PID is
/// recycled in between.
fn signal_worker(worker: &WorkerProcess, stop: Stop) -> bool {
    use rustix::process::{Pid, Signal};

    let Some(pid) = i32::try_from(worker.pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    let signal = match stop {
        Stop::Terminate => Signal::TERM,
        Stop::Kill => Signal::KILL,
    };
    #[cfg(target_os = "linux")]
    {
        let Ok(pidfd) = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
        else {
            return false;
        };
        if start_of(worker.pid).as_deref() != Some(worker.start.as_str()) {
            return false;
        }
        rustix::process::pidfd_send_signal(&pidfd, signal).is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        start_of(worker.pid).as_deref() == Some(worker.start.as_str())
            && rustix::process::kill_process(pid, signal).is_ok()
    }
}

/// Waits until no worker is below `root`, up to `limit`; returns whether none is.
///
/// A table that cannot be read counts as "not yet", so the wait ends at `limit`.
fn wait_for_none(root: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if try_workers_under(root).is_ok_and(|workers| workers.is_empty()) {
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
/// [`KILL_SETTLE`]. Every signal goes through [`signal_worker`], so only a
/// process that still matches what was listed is signalled.
///
/// # Errors
///
/// Returns the I/O error when the process table cannot be read.
pub fn try_reap_workers_under(root: &Path) -> io::Result<Vec<WorkerProcess>> {
    let found = try_workers_under(root)?;
    if found.is_empty() {
        return Ok(found);
    }
    for worker in &found {
        signal_worker(worker, Stop::Terminate);
    }
    if !wait_for_none(root, TERMINATE_GRACE) {
        for worker in try_workers_under(root)? {
            signal_worker(&worker, Stop::Kill);
        }
        let _ = wait_for_none(root, KILL_SETTLE);
    }
    Ok(found)
}

/// Terminates every worker below `root` and returns those that were running.
///
/// # Panics
///
/// Panics when the process table cannot be read.
#[must_use]
pub fn reap_workers_under(root: &Path) -> Vec<WorkerProcess> {
    try_reap_workers_under(root).unwrap_or_else(|error| {
        panic!("cannot read the process table to reap leaked workers: {error}")
    })
}

/// Terminates the workers below one fixture root when it drops, and fails the
/// test if there were any.
///
/// Reaping happens on success and while a panic unwinds. The failure is raised
/// only when the thread is not already panicking, so the original assertion is
/// the one that is reported; a process table that cannot be read fails the test
/// the same way and is ignored while unwinding.
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
        let leaked = match try_reap_workers_under(&self.root) {
            Ok(leaked) => leaked,
            Err(error) => {
                assert!(
                    std::thread::panicking(),
                    "cannot read the process table to check for leaked workers below {}: {error}",
                    self.root.display()
                );
                return;
            }
        };
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
    use std::ffi::OsString;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::CommandExt as _;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};

    use super::{
        as_worker, parse_stat, reap_workers_under, signal_worker, workers_among, workers_under,
        ProcessEntry, Stop, WorkerGuard, WorkerProcess,
    };
    use crate::process_env::ProcessEnv;
    use crate::tempdir;
    use crate::wait::poll_until;

    /// Shell body of a stand-in worker: idles until `SIGTERM`, then ends its
    /// child and exits.
    const FAKE_WORKER_SCRIPT: &str = "sleep 120 & child=$!; trap 'kill $child' TERM; wait";

    /// Shell body that starts a stand-in worker in the background and returns,
    /// so the worker's parent is no longer this process: `$0` is the worker
    /// executable, `$1` its script, `$2` its socket path.
    const DETACH_SCRIPT: &str =
        "( \"$0\" -c \"$1\" --daemon-socket-path \"$2\" ) >/dev/null 2>&1 &";

    fn entry(executable: &str, arguments: &[&str]) -> ProcessEntry {
        ProcessEntry {
            pid: 10,
            parent_pid: 1,
            start: "100".to_owned(),
            executable: None,
            argv: std::iter::once(executable)
                .chain(arguments.iter().copied())
                .map(OsString::from)
                .collect(),
        }
    }

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

    /// Starts a stand-in worker named `executable` whose parent has exited, as
    /// a daemon's worker is after the daemon is killed, with its control socket
    /// at `socket`; returns it once the process table lists it below `root`.
    fn spawn_orphan_at(root: &Path, executable: &Path, socket: &Path) -> WorkerProcess {
        std::fs::create_dir_all(executable.parent().expect("executable directory"))
            .expect("create executable directory");
        symlink("/bin/sh", executable).expect("name the stand-in worker executable");
        Command::new("/bin/sh")
            .args(["-c", DETACH_SCRIPT])
            .arg(executable)
            .arg(FAKE_WORKER_SCRIPT)
            .arg(socket)
            .status()
            .expect("detach stand-in worker");
        poll_until("the detached stand-in worker appears", || {
            workers_under(root).into_iter().next()
        })
    }

    fn spawn_orphaned_worker(root: &Path) -> WorkerProcess {
        spawn_orphan_at(
            root,
            &root.join("pohunek-sessiond"),
            &root.join("r/pohunek/daemon.sock"),
        )
    }

    #[test]
    fn a_worker_yields_its_pids_start_and_socket_path() {
        let worker = as_worker(&entry(
            "/w/target/debug/pohunek-sessiond",
            &[
                "--session-id",
                "s-1",
                "--daemon-socket-path",
                "/tmp/pw-s-abc/d.sock",
            ],
        ))
        .expect("worker");
        assert_eq!(
            worker,
            WorkerProcess {
                pid: 10,
                parent_pid: 1,
                start: "100".to_owned(),
                socket_path: "/tmp/pw-s-abc/d.sock".into(),
            }
        );
    }

    #[test]
    fn spaces_in_the_executable_and_the_socket_path_are_kept() {
        let worker = as_worker(&entry(
            "/home/a b/target/pohunek-sessiond",
            &[
                "--daemon-socket-path",
                "/tmp/pw s/r/daemon.sock",
                "--session-id",
                "s-1",
            ],
        ))
        .expect("worker");
        assert_eq!(worker.socket_path, Path::new("/tmp/pw s/r/daemon.sock"));
        let entries = [entry(
            "/home/a b/target/pohunek-sessiond",
            &["--daemon-socket-path", "/tmp/pw s/r/daemon.sock"],
        )];
        assert_eq!(workers_among(&entries, Path::new("/tmp/pw s")).len(), 1);
        assert!(workers_among(&entries, Path::new("/tmp/pw")).is_empty());
    }

    #[test]
    fn other_processes_and_a_worker_without_a_socket_are_ignored() {
        assert_eq!(as_worker(&entry("/usr/bin/pohunekd", &["--foo"])), None);
        assert_eq!(
            as_worker(&entry("/w/pohunek-sessiond", &["--session-id", "s-1"])),
            None
        );
        assert_eq!(
            as_worker(&entry("/w/pohunek-sessiond", &["--daemon-socket-path"])),
            None
        );
        assert_eq!(
            as_worker(&entry(
                "grep",
                &["pohunek-sessiond", "--daemon-socket-path", "/tmp/x"]
            )),
            None
        );
    }

    #[test]
    fn a_replaced_binary_is_still_a_worker_by_its_executable_link() {
        let mut process = entry(
            "pohunek-sessiond",
            &["--daemon-socket-path", "/tmp/x/d.sock"],
        );
        process.argv[0] = OsString::from("renamed");
        process.executable = Some(PathBuf::from("/w/target/debug/pohunek-sessiond (deleted)"));
        assert!(as_worker(&process).is_some());
    }

    #[test]
    fn a_root_owns_only_the_workers_below_it() {
        let inside = entry(
            "/w/pohunek-sessiond",
            &["--daemon-socket-path", "/tmp/pw-s-abc/d.sock"],
        );
        let sibling = entry(
            "/w/pohunek-sessiond",
            &["--daemon-socket-path", "/tmp/pw-s-abcd/d.sock"],
        );
        let entries = [inside, sibling];
        assert_eq!(workers_among(&entries, Path::new("/tmp/pw-s-abc")).len(), 1);
        assert!(workers_among(&entries, Path::new("/tmp/pw-s-ab")).is_empty());
    }

    #[test]
    fn stat_fields_are_counted_after_the_last_parenthesis() {
        let stat = "42 (we ird) name) S 7 42 42 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 987654 1 2";
        assert_eq!(parse_stat(stat), Some((7, "987654".to_owned())));
        assert_eq!(parse_stat("42 (short) S 7"), None);
    }

    #[test]
    fn a_direct_child_worker_is_not_a_leak() {
        let root = tempdir().expect("root");
        let mut worker = spawn_child_worker(root.path());
        let guard = WorkerGuard::watch(root.path());
        assert!(guard.survivors().is_empty());
        drop(guard);
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(i32::try_from(worker.id()).expect("pid")).expect("pid"),
            rustix::process::Signal::TERM,
        )
        .expect("end the child worker");
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
        assert_eq!(reaped.iter().map(|w| w.pid).collect::<Vec<_>>(), [mine.pid]);
        assert!(workers_under(owned.path()).is_empty());

        let survivors = workers_under(other.path());
        assert_eq!(
            survivors.iter().map(|w| w.pid).collect::<Vec<_>>(),
            [theirs.pid]
        );
        assert_eq!(reap_workers_under(other.path()).len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_worker_whose_paths_contain_spaces_is_found_and_reaped() {
        let root = tempdir().expect("root");
        let worker = spawn_orphan_at(
            root.path(),
            &root.path().join("bin dir/pohunek-sessiond"),
            &root.path().join("run dir/pohunek/daemon.sock"),
        );
        assert_eq!(
            worker.socket_path,
            root.path().join("run dir/pohunek/daemon.sock")
        );
        assert_eq!(reap_workers_under(root.path()).len(), 1);
        assert!(workers_under(root.path()).is_empty());
    }

    #[test]
    fn a_process_that_no_longer_matches_its_recorded_start_is_not_signalled() {
        let root = tempdir().expect("root");
        let worker = spawn_orphaned_worker(root.path());
        let recycled = WorkerProcess {
            start: format!("{}-recycled", worker.start),
            ..worker.clone()
        };
        assert!(!signal_worker(&recycled, Stop::Kill));
        assert_eq!(
            workers_under(root.path()).len(),
            1,
            "the process still runs"
        );
        assert!(signal_worker(&worker, Stop::Terminate));
        let _ = reap_workers_under(root.path());
    }

    #[test]
    fn the_scan_does_not_depend_on_the_path_of_the_test_process() {
        let root = tempdir().expect("root");
        let _worker = spawn_orphaned_worker(root.path());
        let mut env = ProcessEnv::lock();
        env.set("PATH", "/test-support-overridden-path");
        assert_eq!(workers_under(root.path()).len(), 1);
        assert_eq!(reap_workers_under(root.path()).len(), 1);
        drop(env);
        assert!(workers_under(root.path()).is_empty());
    }

    #[test]
    fn a_guard_that_reaps_a_worker_fails_the_test_naming_it() {
        let root = tempdir().expect("root");
        let worker = spawn_orphaned_worker(root.path());
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _guard = WorkerGuard::watch(root.path());
        }));
        let payload = outcome.expect_err("a leaked worker fails the test");
        let message = payload.downcast_ref::<String>().expect("formatted panic");
        assert!(message.contains(&worker.pid.to_string()), "{message}");
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
