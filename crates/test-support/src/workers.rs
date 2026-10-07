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
use std::sync::{Mutex, PoisonError};
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

/// A process identified by PID and start time, so a recycled PID differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessId {
    /// Process ID.
    pub pid: u32,
    /// Opaque start time of the process.
    pub start: String,
}

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
    /// The worker's descendants and the members of the sessions they lead.
    ///
    /// The PTY workload of a session runs in a session of its own and outlives
    /// a worker that is terminated, so it is stopped before the worker is.
    pub workload: Vec<ProcessId>,
    /// Leaders of the sessions the workload leads: those listed now plus those
    /// the caller retained from earlier scans.
    pub sessions: Vec<ProcessId>,
}

/// One process of the process table, before it is matched against a root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessEntry {
    pid: u32,
    parent_pid: u32,
    /// Session ID; 0 when the platform's process listing does not report it.
    session: u32,
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
#[cfg(any(target_os = "linux", test))]
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
        workload: Vec::new(),
        sessions: Vec::new(),
    })
}

/// Returns the processes that belong to the workload of `worker`: its
/// descendants, and every member of a session that one of them leads.
///
/// A descendant that called `setsid` (the PTY child of a worker) leads a
/// session of its own, and its members stay in it after the intermediate
/// processes exit. The session of the worker itself is never followed: it is
/// shared with the daemon and the test. The worker and this process are never
/// part of the result.
///
/// `retained` lists the leaders of sessions seen as this worker's workload in
/// earlier scans. Their sessions are followed even when no listed descendant
/// leads them any more, which is the case once the intermediate parent exited
/// and init adopted the session leader. A retained leader whose PID now has a
/// different start time is a recycled PID and is ignored. Returns the processes
/// and the leaders of the sessions followed.
fn workload_of(
    entries: &[ProcessEntry],
    worker: &ProcessEntry,
    retained: &[ProcessId],
) -> (Vec<ProcessId>, Vec<ProcessId>) {
    let own_pid = std::process::id();
    let mut members: Vec<&ProcessEntry> = Vec::new();
    let mut frontier = vec![worker.pid];
    while let Some(parent) = frontier.pop() {
        for entry in entries.iter().filter(|entry| entry.parent_pid == parent) {
            if entry.pid != worker.pid
                && entry.pid != own_pid
                && !members.iter().any(|known| known.pid == entry.pid)
            {
                members.push(entry);
                frontier.push(entry.pid);
            }
        }
    }
    let still_theirs = |leader: &&ProcessId| {
        entries
            .iter()
            .find(|entry| entry.pid == leader.pid)
            .is_none_or(|entry| entry.start == leader.start)
    };
    let mut sessions: Vec<u32> = members
        .iter()
        .map(|entry| entry.session)
        .chain(
            retained
                .iter()
                .filter(still_theirs)
                .map(|leader| leader.pid),
        )
        .filter(|session| *session != 0 && *session != worker.session)
        .collect();
    sessions.sort_unstable();
    sessions.dedup();
    let leaders = sessions
        .iter()
        .filter_map(|session| entries.iter().find(|entry| entry.pid == *session))
        .map(|leader| ProcessId {
            pid: leader.pid,
            start: leader.start.clone(),
        })
        .chain(retained.iter().filter(still_theirs).cloned())
        .collect::<Vec<_>>();
    let session_members = entries.iter().filter(|entry| {
        sessions.contains(&entry.session)
            && entry.pid != worker.pid
            && entry.pid != own_pid
            && !members.iter().any(|known| known.pid == entry.pid)
    });
    let session_members: Vec<&ProcessEntry> = session_members.collect();
    let workload = members
        .into_iter()
        .chain(session_members)
        .map(|entry| ProcessId {
            pid: entry.pid,
            start: entry.start.clone(),
        })
        .collect();
    let mut leaders = leaders;
    leaders.sort_by_key(|leader| leader.pid);
    leaders.dedup();
    (workload, leaders)
}

/// Returns the workers among `entries` whose socket path is below `root`.
///
/// The comparison is per path component, so `/tmp/pw-s-ab` does not own a
/// worker of `/tmp/pw-s-abc`.
fn workers_among(
    entries: &[ProcessEntry],
    root: &Path,
    retained: &[ProcessId],
) -> Vec<WorkerProcess> {
    entries
        .iter()
        .filter_map(|entry| matched_worker(entry, root).map(|worker| (entry, worker)))
        .map(|(entry, worker)| {
            let (workload, sessions) = workload_of(entries, entry, retained);
            WorkerProcess {
                workload,
                sessions,
                ..worker
            }
        })
        .collect()
}

/// Returns the worker `entry` describes when its socket is below `root`.
#[cfg(target_os = "linux")]
fn matched_worker(entry: &ProcessEntry, root: &Path) -> Option<WorkerProcess> {
    as_worker(entry).filter(|worker| worker.socket_path.starts_with(root))
}

/// Returns the worker `entry` describes when its socket is below `root`.
///
/// `ps` has lost the argument boundaries, so the whole command line is matched
/// as text (see [`command_is_worker_below`]); the socket path reported is the root.
#[cfg(not(target_os = "linux"))]
fn matched_worker(entry: &ProcessEntry, root: &Path) -> Option<WorkerProcess> {
    let command = entry.argv.first()?.to_str()?;
    command_is_worker_below(command, root).then(|| WorkerProcess {
        pid: entry.pid,
        parent_pid: entry.parent_pid,
        start: entry.start.clone(),
        socket_path: root.to_path_buf(),
        workload: Vec::new(),
        sessions: Vec::new(),
    })
}

/// Whether `command`, the whole command column of `ps`, is a worker whose
/// control socket is below `root`.
///
/// Plain substring matching, so spaces in the executable or in `root` do not
/// matter: the command names the worker executable and contains
/// `--daemon-socket-path <root>` followed by a path separator, which keeps
/// `/tmp/pw-s-ab` from owning a worker of `/tmp/pw-s-abc`.
#[cfg(any(not(target_os = "linux"), test))]
fn command_is_worker_below(command: &str, root: &Path) -> bool {
    let names_worker = command.starts_with(WORKER_EXECUTABLE)
        || command.contains(&format!("/{WORKER_EXECUTABLE}"));
    let marker = format!("{SOCKET_PATH_ARGUMENT} {}", root.display());
    names_worker
        && command
            .match_indices(&marker)
            .any(|(at, _)| command[at + marker.len()..].starts_with('/'))
}

/// Fields of `/proc/<pid>/stat` the process table uses.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
struct Stat {
    parent_pid: u32,
    session: u32,
    start: String,
}

/// Extracts the parent PID, session and start time from the contents of `/proc/<pid>/stat`.
///
/// The command name is parenthesised and may itself contain spaces and
/// parentheses, so the fields are counted from the last closing parenthesis.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(stat: &str) -> Option<Stat> {
    /// Index of the parent PID after the command name (field 4 of `proc(5)`).
    const PARENT_FIELD: usize = 1;
    /// Index of the session ID after the command name (field 6 of `proc(5)`).
    const SESSION_FIELD: usize = 3;
    /// Index of the start time after the command name (field 22 of `proc(5)`).
    const START_FIELD: usize = 19;
    let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    Some(Stat {
        parent_pid: fields.get(PARENT_FIELD)?.parse().ok()?,
        session: fields.get(SESSION_FIELD)?.parse().ok()?,
        start: (*fields.get(START_FIELD)?).to_owned(),
    })
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
        let Some(Stat {
            parent_pid,
            session,
            start,
        }) = parse_stat(&stat_line)
        else {
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
            session,
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
                // `ps` has no portable session column; without one only
                // descendants are part of a worker's workload.
                session: 0,
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
    parse_stat(&stat).map(|stat| stat.start)
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
    workers_retaining(root, &[])
}

/// [`try_workers_under`] that also follows the sessions in `retained`.
fn workers_retaining(root: &Path, retained: &[ProcessId]) -> io::Result<Vec<WorkerProcess>> {
    let own_pid = std::process::id();
    Ok(workers_among(&process_table()?, root, retained)
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

/// Sends `stop` to the process `pid` only if it still has the start time
/// recorded for it; returns whether a signal was sent.
///
/// On Linux a pidfd is opened first and the start time is checked afterwards,
/// so the signal goes to the process that was verified even if the PID is
/// recycled in between.
fn signal_process(pid: u32, start: &str, stop: Stop) -> bool {
    use rustix::process::{Pid, Signal};

    let Some(target) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    let signal = match stop {
        Stop::Terminate => Signal::TERM,
        Stop::Kill => Signal::KILL,
    };
    #[cfg(target_os = "linux")]
    {
        let Ok(pidfd) = rustix::process::pidfd_open(target, rustix::process::PidfdFlags::empty())
        else {
            return false;
        };
        if start_of(pid).as_deref() != Some(start) {
            return false;
        }
        rustix::process::pidfd_send_signal(&pidfd, signal).is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        start_of(pid).as_deref() == Some(start)
            && rustix::process::kill_process(target, signal).is_ok()
    }
}

/// Sends `stop` to `worker` under the identity check of [`signal_process`].
fn signal_worker(worker: &WorkerProcess, stop: Stop) -> bool {
    signal_process(worker.pid, &worker.start, stop)
}

/// Kills the workload of `worker`, each process under the identity check of
/// [`signal_process`].
///
/// The workload is killed before the worker: a worker that is signalled first
/// exits without ending the session it started, and the workload then outlives
/// the fixture root. `SIGKILL` is used because a workload can ignore `SIGHUP`
/// and `SIGTERM`.
fn stop_workload(worker: &WorkerProcess) {
    for process in &worker.workload {
        signal_process(process.pid, &process.start, Stop::Kill);
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
/// Kills each worker's workload, sends the worker `SIGTERM`, then `SIGKILL` to
/// whatever is still listed after
/// [`TERMINATE_GRACE`], and returns once the process table shows none or after
/// [`KILL_SETTLE`]. Every signal goes through [`signal_worker`], so only a
/// process that still matches what was listed is signalled.
///
/// # Errors
///
/// Returns the I/O error when the process table cannot be read.
pub fn try_reap_workers_under(root: &Path) -> io::Result<Vec<WorkerProcess>> {
    reap_retaining(root, &mut Vec::new())
}

/// Adds the session leaders of the workloads of `workers` to `retained`.
fn retain_sessions(retained: &mut Vec<ProcessId>, workers: &[WorkerProcess]) {
    retained.extend(
        workers
            .iter()
            .flat_map(|worker| worker.sessions.iter().cloned()),
    );
    retained.sort_by_key(|leader| leader.pid);
    retained.dedup();
}

/// [`try_reap_workers_under`] that follows the sessions in `retained` and adds
/// the ones it finds to it.
fn reap_retaining(root: &Path, retained: &mut Vec<ProcessId>) -> io::Result<Vec<WorkerProcess>> {
    let found = workers_retaining(root, retained)?;
    if found.is_empty() {
        return Ok(found);
    }
    retain_sessions(retained, &found);
    for worker in &found {
        stop_workload(worker);
        signal_worker(worker, Stop::Terminate);
    }
    if !wait_for_none(root, TERMINATE_GRACE) {
        for worker in workers_retaining(root, retained)? {
            stop_workload(&worker);
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
    /// Sessions seen as the workload of a worker in an earlier scan.
    retained_sessions: Mutex<Vec<ProcessId>>,
}

impl WorkerGuard {
    /// Watches workers whose control socket is below `root`.
    ///
    /// `root` must be the canonical path the daemon under test was given, since
    /// the match is on the path as it appears in the worker's arguments.
    #[must_use]
    pub fn watch(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            retained_sessions: Mutex::new(Vec::new()),
        }
    }

    /// Returns the workers below the root that are running now.
    ///
    /// The sessions their workloads lead are remembered, so the guard still
    /// stops them if the intermediate parent exits before the guard drops.
    ///
    /// # Panics
    ///
    /// Panics when the process table cannot be read.
    #[must_use]
    pub fn survivors(&self) -> Vec<WorkerProcess> {
        let mut retained = self
            .retained_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let workers = workers_retaining(&self.root, &retained).unwrap_or_else(|error| {
            panic!("cannot read the process table to look for leaked workers: {error}")
        });
        retain_sessions(&mut retained, &workers);
        workers
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let retained = self
            .retained_sessions
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let leaked = match reap_retaining(&self.root, retained) {
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
        as_worker, command_is_worker_below, parse_stat, reap_workers_under, signal_worker,
        workers_among, workers_under, workload_of, ProcessEntry, ProcessId, Stat, Stop,
        WorkerGuard, WorkerProcess,
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
            session: 0,
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
    fn spawn_orphan_at(
        root: &Path,
        executable: &Path,
        socket: &Path,
        script: &str,
    ) -> WorkerProcess {
        std::fs::create_dir_all(executable.parent().expect("executable directory"))
            .expect("create executable directory");
        symlink("/bin/sh", executable).expect("name the stand-in worker executable");
        Command::new("/bin/sh")
            .args(["-c", DETACH_SCRIPT])
            .arg(executable)
            .arg(script)
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
            FAKE_WORKER_SCRIPT,
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
                workload: Vec::new(),
                sessions: Vec::new(),
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
        assert_eq!(
            workers_among(&entries, Path::new("/tmp/pw s"), &[]).len(),
            1
        );
        assert!(workers_among(&entries, Path::new("/tmp/pw"), &[]).is_empty());
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
        assert_eq!(
            workers_among(&entries, Path::new("/tmp/pw-s-abc"), &[]).len(),
            1
        );
        assert!(workers_among(&entries, Path::new("/tmp/pw-s-ab"), &[]).is_empty());
    }

    #[test]
    fn stat_fields_are_counted_after_the_last_parenthesis() {
        let stat = "42 (we ird) name) S 7 42 42 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 987654 1 2";
        assert_eq!(
            parse_stat(stat),
            Some(Stat {
                parent_pid: 7,
                session: 42,
                start: "987654".to_owned()
            })
        );
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
            FAKE_WORKER_SCRIPT,
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

    fn process_in_table(pid: u32, parent_pid: u32, session: u32) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent_pid,
            session,
            start: format!("s{pid}"),
            executable: None,
            argv: Vec::new(),
        }
    }

    #[test]
    fn the_workload_is_the_descendants_and_the_sessions_they_lead() {
        let worker = process_in_table(100, 1, 50);
        let entries = [
            worker.clone(),
            process_in_table(101, 100, 50),
            // PTY child: its own session, with a straggler reparented to init.
            process_in_table(102, 100, 102),
            process_in_table(103, 102, 102),
            process_in_table(104, 1, 102),
            // Descendant of a descendant.
            process_in_table(105, 101, 50),
            // Unrelated: the worker's own session, init's child, another session.
            process_in_table(106, 1, 50),
            process_in_table(107, 1, 107),
            process_in_table(std::process::id(), 100, 102),
        ];
        let mut pids: Vec<u32> = workload_of(&entries, &worker, &[])
            .0
            .iter()
            .map(|process| process.pid)
            .collect();
        pids.sort_unstable();
        assert_eq!(pids, [101, 102, 103, 104, 105]);
    }

    /// Stand-in worker body whose workload ignores `SIGHUP` and `SIGTERM`, lives
    /// in a session of its own and records its PID in `pid_file`.
    #[cfg(target_os = "linux")]
    fn resistant_workload_script(pid_file: &Path) -> String {
        format!(
            "setsid -w sh -c 'trap \"\" HUP TERM; echo $$ > {}; sleep 613; true' & wait",
            pid_file.display()
        )
    }

    #[cfg(target_os = "linux")]
    fn process_exists(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|stat| !stat.contains(") Z "))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_signal_resistant_workload_in_its_own_session_is_stopped_with_its_worker() {
        let root = tempdir().expect("root");
        let pid_file = root.path().join("workload.pid");
        let worker = spawn_orphan_at(
            root.path(),
            &root.path().join("pohunek-sessiond"),
            &root.path().join("r/pohunek/daemon.sock"),
            &resistant_workload_script(&pid_file),
        );
        let workload: u32 = poll_until("the workload records its PID", || {
            std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
        });
        poll_until("the worker lists its workload", || {
            workers_under(root.path())
                .first()
                .filter(|listed| {
                    listed
                        .workload
                        .iter()
                        .any(|process| process.pid == workload)
                })
                .map(|_| ())
        });
        assert!(process_exists(workload));

        assert_eq!(reap_workers_under(root.path()).len(), 1);
        poll_until("the workload ends", || {
            (!process_exists(workload) && !process_exists(worker.pid)).then_some(())
        });
    }

    #[test]
    fn a_ps_command_is_matched_as_text_so_spaces_do_not_matter() {
        let command = "/Users/a b/target/debug/pohunek-sessiond --session-id s-1 \
                       --daemon-socket-path /private/tmp/pw s/r/pohunek/daemon.sock";
        assert!(command_is_worker_below(
            command,
            Path::new("/private/tmp/pw s")
        ));
        assert!(!command_is_worker_below(
            command,
            Path::new("/private/tmp/pw")
        ));
        assert!(!command_is_worker_below(
            command,
            Path::new("/private/tmp/other")
        ));
        assert!(!command_is_worker_below(
            "grep pohunek-sessiond --daemon-socket-path /private/tmp/pw s/x",
            Path::new("/private/tmp/pw s")
        ));
    }

    #[test]
    fn a_retained_session_is_followed_after_its_leader_was_reparented() {
        let worker = process_in_table(100, 1, 50);
        // The intermediate parent exited: the leader's parent is init.
        let entries = [
            worker.clone(),
            process_in_table(102, 1, 102),
            process_in_table(103, 102, 102),
        ];
        assert!(workload_of(&entries, &worker, &[]).0.is_empty());
        let leader = ProcessId {
            pid: 102,
            start: "s102".to_owned(),
        };
        let (workload, sessions) = workload_of(&entries, &worker, std::slice::from_ref(&leader));
        let mut pids: Vec<u32> = workload.iter().map(|process| process.pid).collect();
        pids.sort_unstable();
        assert_eq!(pids, [102, 103]);
        assert_eq!(sessions, [leader]);

        // The same PID with another start time is a recycled one: not followed.
        let recycled = ProcessId {
            pid: 102,
            start: "other".to_owned(),
        };
        assert!(workload_of(&entries, &worker, &[recycled]).0.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_workload_whose_intermediate_parent_exited_is_still_stopped() {
        let root = tempdir().expect("root");
        let pid_file = root.path().join("workload.pid");
        // worker -> intermediate shell -> session leader (`setsid` runs `inner.sh`
        // in place; the trailing `true` keeps the intermediate shell alive).
        let inner = root.path().join("inner.sh");
        std::fs::write(
            &inner,
            format!(
                "trap '' HUP TERM; echo $$ > {}; sleep 613; true",
                pid_file.display()
            ),
        )
        .expect("write the workload script");
        let worker = spawn_orphan_at(
            root.path(),
            &root.path().join("pohunek-sessiond"),
            &root.path().join("r/pohunek/daemon.sock"),
            // The idle child keeps the worker alive once the intermediate shell ends.
            &format!(
                "sleep 613 & sh -c 'setsid sh {}; true' & wait",
                inner.display()
            ),
        );
        let leader: u32 = poll_until("the workload records its PID", || {
            std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
        });
        let guard = WorkerGuard::watch(root.path());
        poll_until("the guard sees the workload", || {
            guard
                .survivors()
                .iter()
                .any(|listed| listed.workload.iter().any(|process| process.pid == leader))
                .then_some(())
        });

        // End the process between the worker and the session leader.
        let parent_of = |pid: u32| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| parse_stat(&stat))
                .map(|stat| stat.parent_pid)
        };
        let intermediate = parent_of(leader).expect("the leader has a parent");
        assert_ne!(intermediate, worker.pid);
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(i32::try_from(intermediate).expect("pid")).expect("pid"),
            rustix::process::Signal::KILL,
        )
        .expect("end the intermediate parent");
        poll_until("the leader is reparented", || {
            (parent_of(leader) != Some(intermediate)).then_some(())
        });
        let listed = workers_under(root.path());
        assert!(
            !listed[0]
                .workload
                .iter()
                .any(|process| process.pid == leader),
            "without the retained session the leader is no longer linked to the worker"
        );

        let outcome = catch_unwind(AssertUnwindSafe(|| drop(guard)));
        assert!(outcome.is_err(), "the leaked worker fails the test");
        poll_until("the workload ends", || {
            (!process_exists(leader) && !process_exists(worker.pid)).then_some(())
        });
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
