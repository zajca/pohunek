//! A host-wide lock between test processes of separate test runs.
//!
//! A nextest test group serializes its members within one run only. Several
//! runs on one host (two developers' or agents' gates, a local run beside an
//! IDE run) are separate processes of the same user, so a group cannot keep
//! their members apart. [`HostLock`] does: it is an exclusive `flock` on a file
//! under a fixed, per-user directory in `/tmp`, shared by every run of this
//! user whatever its `TMPDIR`. Runs that see separate `/tmp` mounts (a private
//! `/tmp` sandbox) do not share it.
//!
//! The lock is advisory and released by the kernel when the holding process
//! ends, so a test that crashes or is killed never leaves it held.
//!
//! It stays host-wide on purpose: what it protects, a sweep of the whole
//! same-user process table, is shared by every run of the user, so a lock per
//! worktree or per target directory would not keep the runs apart.
//!
//! A waiter therefore waits as long as other runs keep making progress. It
//! blocks in the kernel, so it takes the lock as soon as the holder releases
//! it, and it reports the holder (pid, test, and the test executable, which
//! names the worktree) every [`PROGRESS_REPORT_INTERVAL`]. It fails only when
//! one holder keeps the lock longer than [`HOLDER_STALL_GUARD`], when the wait
//! takes longer than [`TOTAL_WAIT_GUARD`] in all, or when the recorded holder
//! process is gone and the lock stays held for [`HANG_GUARD`]. A lock cannot
//! be stale: the kernel releases it with the last descriptor of its holder, so
//! a lock held after its recorded holder ended is held by a process that
//! inherited that descriptor, and is not taken over.
//!
//! # Examples
//!
//! ```no_run
//! use pohunek_test_support::host_lock::{HostLock, UNREADABLE_HOLDERS};
//!
//! let _held = HostLock::acquire(UNREADABLE_HOLDERS, "my_test");
//! // ... the part of the test that must not overlap another run's ...
//! ```

// Rust guideline compliant 2026-10-14

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use rustix::fs::{flock, FlockOperation};
use rustix::io::Errno;

use crate::wait::HANG_GUARD;

/// The lock of tests that start, or need a sweep untouched by, a same-user
/// process whose ownership markers the kernel hides.
///
/// A worker confirms a forced output close by sweeping the whole same-user
/// process table. On Linux it cannot rule out a process with a hidden (non-
/// dumpable) environment that started after the worker, so one test's escaped
/// non-dumpable holder keeps another run's forced close unconfirmed. Every test
/// of the `unreadable-holders` nextest group holds this lock for its whole run;
/// the name matches that group.
pub const UNREADABLE_HOLDERS: &str = "unreadable-holders";

/// How often a waiter reports who holds the lock.
pub const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(30);

/// Longest one holder may keep the lock before a waiter calls it hung.
///
/// A holder is one test, and every wait inside a test is bounded by
/// [`HANG_GUARD`]; a test that holds the lock for ten of those is hung, while
/// the slowest holder seen (about a minute under load) stays far below it.
pub const HOLDER_STALL_GUARD: Duration = HANG_GUARD.saturating_mul(10);

/// Longest a waiter waits in all, however many holders come and go.
///
/// Holders that keep changing are other runs making progress, and a whole
/// gate run of the group takes minutes. Waiting longer than thirty
/// [`HANG_GUARD`]s means the lock is never free for this run, so the run fails
/// instead of hanging.
pub const TOTAL_WAIT_GUARD: Duration = HANG_GUARD.saturating_mul(30);

/// When a waiter reports progress and when it gives up.
#[derive(Debug, Clone, Copy)]
struct WaitPolicy {
    /// Interval of the progress reports, and of the hang checks.
    report_interval: Duration,
    /// Longest one recorded holder may keep the lock.
    holder_stall: Duration,
    /// Longest the lock may stay held after its recorded holder is gone.
    orphaned_hold: Duration,
    /// Longest the whole wait may take.
    total_wait: Duration,
}

impl WaitPolicy {
    const HOST: Self = Self {
        report_interval: PROGRESS_REPORT_INTERVAL,
        holder_stall: HOLDER_STALL_GUARD,
        orphaned_hold: HANG_GUARD,
        total_wait: TOTAL_WAIT_GUARD,
    };
}

/// Parent of the per-user lock directory.
///
/// A fixed path, not [`std::env::temp_dir`]: runs on one host set `TMPDIR`
/// differently, and the lock only works when they all open the same file.
/// `/tmp` exists on every supported host.
const LOCK_PARENT: &str = "/tmp";

/// Mode of the lock directory and its files: the owner's only.
const PRIVATE_DIR_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Mode bits that would let another user replace the directory's entries.
const FOREIGN_WRITE_BITS: u32 = 0o022;

/// A held host-wide lock; dropping it releases the lock.
#[derive(Debug)]
pub struct HostLock {
    _file: File,
}

impl HostLock {
    /// Blocks until this process holds the host-wide lock `name`.
    ///
    /// `holder` names the test in the lock file, so a waiter can say who holds
    /// it. While another process holds it, this reports that holder on stderr
    /// every [`PROGRESS_REPORT_INTERVAL`].
    ///
    /// # Panics
    ///
    /// Panics when the lock directory is not a private directory of the current
    /// user, when the lock file cannot be opened or written, when one holder
    /// keeps the lock longer than [`HOLDER_STALL_GUARD`], when the wait takes
    /// longer than [`TOTAL_WAIT_GUARD`], or when the recorded holder process is
    /// gone and the lock stays held for [`HANG_GUARD`]; the message names the
    /// holder.
    #[must_use]
    pub fn acquire(name: &str, holder: &str) -> Self {
        Self::acquire_with(name, holder, WaitPolicy::HOST, &mut |line| {
            eprintln!("{line}");
        })
    }

    fn acquire_with(
        name: &str,
        holder: &str,
        policy: WaitPolicy,
        report: &mut dyn FnMut(&str),
    ) -> Self {
        let path = lock_dir().join(format!("{name}.lock"));
        // Built before locking, so the record follows the lock at once.
        let record = holder_record(holder);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc_o_nofollow())
            .open(&path)
            .unwrap_or_else(|error| panic!("open the host lock {}: {error}", path.display()));
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK | Errno::INTR) => {
                wait_for_lock(&path, &mut file, policy, report);
            }
            Err(error) => panic!("lock the host lock {}: {error}", path.display()),
        }
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| file.write_all(record.as_bytes()))
            .unwrap_or_else(|error| panic!("record the holder of {}: {error}", path.display()));
        Self { _file: file }
    }
}

/// Waits until `file`, whose lock another process holds, is locked.
///
/// The lock is taken by a blocking `flock` on a second descriptor of the same
/// open file, so this process holds it once that call returns. A waiter that
/// gives up leaves that call running; it releases the lock as soon as it gets
/// it, since nothing else keeps the open file then.
fn wait_for_lock(path: &Path, file: &mut File, policy: WaitPolicy, report: &mut dyn FnMut(&str)) {
    let blocking = file
        .try_clone()
        .unwrap_or_else(|error| panic!("open a second handle on {}: {error}", path.display()));
    let (locked_tx, locked_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let locked = loop {
            match flock(&blocking, FlockOperation::LockExclusive) {
                Err(Errno::INTR) => {}
                other => break other,
            }
        };
        // A waiter that gave up no longer listens.
        let _ = locked_tx.send(locked);
    });
    let started = Instant::now();
    let mut current: Option<(String, Instant)> = None;
    loop {
        match locked_rx.recv_timeout(policy.report_interval) {
            Ok(Ok(())) => return,
            Ok(Err(error)) => panic!("lock the host lock {}: {error}", path.display()),
            Err(RecvTimeoutError::Disconnected) => {
                panic!(
                    "the lock thread of {} ended without a result",
                    path.display()
                )
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        let holder = current_holder(file);
        let since = match &current {
            Some((known, since)) if *known == holder => *since,
            _ => Instant::now(),
        };
        current = Some((holder.clone(), since));
        report(&format!(
            "waiting {:?} for the host lock {}, held by {holder} for {:?}",
            started.elapsed(),
            path.display(),
            since.elapsed()
        ));
        assert!(
            since.elapsed() < policy.holder_stall,
            "the host lock {} is still held after {:?} by one holder, which is hung: {holder}",
            path.display(),
            since.elapsed()
        );
        assert!(
            started.elapsed() < policy.total_wait,
            "the host lock {} was never free for {:?}; it is now held by: {holder}",
            path.display(),
            started.elapsed()
        );
        assert!(
            holder_is_alive(&holder) || since.elapsed() < policy.orphaned_hold,
            "the host lock {} is still held {:?} after its recorded holder ended; \
             another process keeps its descriptor: {holder}",
            path.display(),
            since.elapsed()
        );
    }
}

/// The record of this process as the lock's holder: its pid, the test
/// `holder`, and the test executable, which names the worktree it was built in.
fn holder_record(holder: &str) -> String {
    let executable = std::env::current_exe().map_or_else(
        |error| format!("unknown ({error})"),
        |path| path.display().to_string(),
    );
    format!(
        "pid={} test={holder} executable={executable}",
        std::process::id()
    )
}

/// Whether the process a holder record names still exists.
///
/// A record without a readable pid counts as alive, so only a holder known to
/// be gone shortens the wait.
fn holder_is_alive(record: &str) -> bool {
    let Some(pid) = record
        .strip_prefix("pid=")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|pid| pid.parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw)
    else {
        return true;
    };
    !matches!(rustix::process::test_kill_process(pid), Err(Errno::SRCH))
}

/// Reads who holds the lock, for a progress report or a failure message.
fn current_holder(file: &mut File) -> String {
    let mut holder = String::new();
    match file
        .seek(SeekFrom::Start(0))
        .and_then(|_| file.read_to_string(&mut holder))
    {
        Ok(_) if !holder.is_empty() => holder,
        Ok(_) => "an unnamed holder".to_owned(),
        Err(error) => format!("unknown ({error})"),
    }
}

/// Returns the per-user lock directory, creating it private when missing.
///
/// # Panics
///
/// Panics when the path exists but is a symlink, not a directory, owned by
/// another user, or writable by group or others, since such a directory could
/// let another user swap the lock files.
fn lock_dir() -> PathBuf {
    let uid = rustix::process::geteuid().as_raw();
    let dir = Path::new(LOCK_PARENT).join(format!("pohunek-test-locks-{uid}"));
    match DirBuilder::new().mode(PRIVATE_DIR_MODE).create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => panic!("create the host lock directory {}: {error}", dir.display()),
    }
    let metadata = std::fs::symlink_metadata(&dir).unwrap_or_else(|error| {
        panic!("inspect the host lock directory {}: {error}", dir.display())
    });
    assert!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & FOREIGN_WRITE_BITS == 0,
        "the host lock directory {} must be a private directory of uid {uid}",
        dir.display()
    );
    dir
}

/// `O_NOFOLLOW`, so a symlink planted at a lock path is refused.
fn libc_o_nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW
        .bits()
        .try_into()
        .expect("O_NOFOLLOW fits an i32")
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};
    use std::thread::JoinHandle;

    use super::*;

    /// Progress interval of the test waiters: short, so a scenario sees many
    /// reports at once.
    const TEST_REPORT_INTERVAL: Duration = Duration::from_millis(50);

    /// Holder bound of the scenarios that must not trip it: each holder keeps
    /// the lock for about [`REPORTS_PER_HOLDER`] reports, twenty times less,
    /// so a delayed handover on a loaded host stays within it.
    const TEST_HOLDER_STALL: Duration = Duration::from_secs(2);

    /// Times the scenario hands the lock record to a new holder; together
    /// they hold the lock longer than [`TEST_HOLDER_STALL`].
    const HOLDER_HANDOVERS: usize = 30;

    /// Reports seen per holder before the next one takes over.
    const REPORTS_PER_HOLDER: usize = 2;

    /// Whole-wait bound of the scenario that trips it: shorter than
    /// [`TEST_HOLDER_STALL`], so it trips first while holders keep changing.
    const TEST_TOTAL_WAIT: Duration = Duration::from_secs(1);

    /// A lock name no other test or run uses, removed when dropped.
    struct TestLock(String);

    impl TestLock {
        fn new(scenario: &str) -> Self {
            Self(format!("host-lock-test-{scenario}-{}", std::process::id()))
        }
    }

    impl TestLock {
        /// Replaces the recorded holder through a handle of the test's own;
        /// the record is plain file content, not part of the lock.
        fn record(&self, record: &str) {
            let mut file = OpenOptions::new()
                .write(true)
                .open(lock_dir().join(format!("{}.lock", self.0)))
                .expect("open the test lock file");
            file.set_len(0)
                .and_then(|()| file.seek(SeekFrom::Start(0)))
                .and_then(|_| file.write_all(record.as_bytes()))
                .expect("write the holder record");
        }
    }

    impl Drop for TestLock {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(lock_dir().join(format!("{}.lock", self.0)));
        }
    }

    /// Starts a waiter for `name` under `policy`; its reports arrive on the
    /// returned channel, and the thread returns once it holds the lock.
    fn spawn_waiter(name: &str, policy: WaitPolicy) -> (JoinHandle<HostLock>, Receiver<String>) {
        let (report_tx, report_rx) = mpsc::channel();
        let name = name.to_owned();
        let waiter = std::thread::spawn(move || {
            HostLock::acquire_with(&name, "waiter", policy, &mut |line| {
                let _ = report_tx.send(line.to_owned());
            })
        });
        (waiter, report_rx)
    }

    /// Waits for the next report that names `holder`.
    fn next_report_naming(reports: &Receiver<String>, holder: &str) -> String {
        loop {
            let line = reports
                .recv_timeout(HANG_GUARD)
                .expect("the waiter reports while it waits");
            if line.contains(holder) {
                return line;
            }
        }
    }

    fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_waiter_outlasts_holders_that_keep_making_progress_and_reports_each() {
        let lock = TestLock::new("progress");
        let held = HostLock::acquire(&lock.0, "holder-0");
        let policy = WaitPolicy {
            report_interval: TEST_REPORT_INTERVAL,
            holder_stall: TEST_HOLDER_STALL,
            orphaned_hold: HANG_GUARD,
            total_wait: HANG_GUARD,
        };
        let (waiter, reports) = spawn_waiter(&lock.0, policy);

        for handover in 0..HOLDER_HANDOVERS {
            let holder = format!("holder-{handover}");
            for _ in 0..REPORTS_PER_HOLDER {
                next_report_naming(&reports, &holder);
            }
            lock.record(&format!(
                "pid={} test=holder-{}",
                std::process::id(),
                handover + 1
            ));
        }
        drop(held);

        let acquired = waiter.join().expect("the waiter gets the lock");
        drop(acquired);
    }

    #[test]
    fn a_lock_never_free_for_the_whole_wait_fails_the_waiter() {
        let lock = TestLock::new("total");
        let held = HostLock::acquire(&lock.0, "holder-0");
        let policy = WaitPolicy {
            report_interval: TEST_REPORT_INTERVAL,
            holder_stall: TEST_HOLDER_STALL,
            orphaned_hold: HANG_GUARD,
            total_wait: TEST_TOTAL_WAIT,
        };
        let (waiter, reports) = spawn_waiter(&lock.0, policy);

        // Holders keep changing, each well within its own bound, until the
        // waiter gives up on the whole wait.
        let mut handover = 0;
        while !waiter.is_finished() {
            let holder = format!("holder-{handover}");
            if reports
                .recv_timeout(HANG_GUARD)
                .is_ok_and(|line| line.contains(&holder))
            {
                handover += 1;
                lock.record(&format!(
                    "pid={} test=holder-{handover}",
                    std::process::id()
                ));
            }
        }
        let failure = waiter
            .join()
            .expect_err("the waiter gives up on the whole wait");
        drop(held);

        let message = panic_text(failure.as_ref());
        assert!(message.contains("was never free"), "{message}");
    }

    #[test]
    fn a_holder_that_keeps_the_lock_too_long_fails_the_waiter_with_its_name() {
        let lock = TestLock::new("stall");
        let held = HostLock::acquire(&lock.0, "the-hung-holder");
        let policy = WaitPolicy {
            report_interval: TEST_REPORT_INTERVAL,
            holder_stall: TEST_REPORT_INTERVAL.saturating_mul(2),
            orphaned_hold: HANG_GUARD,
            total_wait: HANG_GUARD,
        };
        let (waiter, _reports) = spawn_waiter(&lock.0, policy);

        let failure = waiter
            .join()
            .expect_err("the waiter gives up on a hung holder");
        drop(held);

        let message = panic_text(failure.as_ref());
        assert!(message.contains("which is hung"), "{message}");
        assert!(message.contains("test=the-hung-holder"), "{message}");
    }

    #[test]
    fn a_lock_held_after_its_recorded_holder_ended_fails_the_waiter() {
        let lock = TestLock::new("orphan");
        let held = HostLock::acquire(&lock.0, "placeholder");
        let mut ended = std::process::Command::new("true")
            .spawn()
            .expect("start a short-lived process");
        let ended_pid = ended.id();
        ended.wait().expect("the short-lived process ends");
        lock.record(&format!("pid={ended_pid} test=the-ended-holder"));
        let policy = WaitPolicy {
            report_interval: TEST_REPORT_INTERVAL,
            holder_stall: HANG_GUARD,
            orphaned_hold: TEST_REPORT_INTERVAL.saturating_mul(2),
            total_wait: HANG_GUARD,
        };
        let (waiter, _reports) = spawn_waiter(&lock.0, policy);

        let failure = waiter
            .join()
            .expect_err("the waiter gives up on an orphaned lock");
        drop(held);

        let message = panic_text(failure.as_ref());
        assert!(
            message.contains("after its recorded holder ended"),
            "{message}"
        );
        assert!(message.contains(&format!("pid={ended_pid}")), "{message}");
    }
}
