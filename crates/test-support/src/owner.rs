//! The owner reaper of a fixture root: the process that sweeps a test root
//! when the test process dies without unwinding.
//!
//! `Drop` cleans up only while the test process unwinds. A test process that
//! is killed (`SIGKILL`, `SIGTERM`, `abort`, nextest termination, an operator
//! kill) runs no `Drop`, and the durable `pohunek-sessiond` workers it left
//! keep their sessions alive. [`OwnerReaper`] closes that gap for one fixture
//! root: the test process creates `<root>/owner.lock` and holds an exclusive
//! `flock` on it, links the lock into the per-user lock directory, and arms a
//! helper process ([`crate::reaper_binary`]) on the link. The kernel releases
//! a `flock` on any death of the holder, so the helper wakes exactly when the
//! test process is gone, whatever killed it, resolves the link back to the
//! root, and sweeps the root with [`sweep_root`]. No polling, no parent-death
//! signal, nothing platform-specific: the same code is correct on Linux and
//! macOS.
//!
//! The sweep can only signal a process whose working directory is the root or
//! below it, or a transitive descendant of such a process, taken from one
//! snapshot of the same-user process table. That is the criterion the daemon
//! crate's scenario reapers already use, extended with the descendant closure
//! so a session leader whose working directory lies outside the root (a
//! session started in its own temporary directory) is still covered. Every
//! signal is preceded by a start-identity re-check, so a recycled PID is never
//! signalled, and an implausible root (not absolute, fewer than two components,
//! or `/`, `/tmp`, `/var/tmp` or `$HOME` themselves) is refused. A production
//! daemon's workers never
//! work under a test root, so the criterion cannot select them.
//!
//! On the normal path [`OwnerReaper::drop`] kills the helper before releasing
//! the lock, so a test that returns pays no sweep and no process-table scan.
//!
//! # Examples
//!
//! ```no_run
//! use pohunek_test_support::owner::OwnerReaper;
//!
//! let root = pohunek_test_support::tempdir()?;
//! let reaper = OwnerReaper::arm(root.path())?;
//! // ... the test runs; dropping `reaper` stops the helper first ...
//! # Ok::<(), std::io::Error>(())
//! ```

// Rust guideline compliant 2026-11-27

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::fs::{flock, FlockOperation};
use rustix::process::{kill_process, Pid, Signal};

use crate::wait::POLL_INTERVAL;

pub use pohunek_platform::process::{
    HostInspector, ProcessIdentity, ProcessInspector, StartIdentity,
};

/// File name of the owner lock inside a fixture root.
pub const OWNER_LOCK_FILE: &str = "owner.lock";

/// Mode of the owner lock file: the owner's only.
///
/// Set with an explicit `chmod` after creation, because the mode passed to
/// `open` is filtered by the process umask, which could clear the owner bits.
const PRIVATE_FILE_MODE: u32 = 0o600;

/// How long the reaper keeps rescanning the process table after it signalled
/// the scoped processes.
///
/// `SIGKILL` cannot be handled, so a signalled process leaves the table as the
/// kernel tears it down; the bound only keeps an unkillable process from
/// hanging the reaper, and matches the settle bound the worker guard uses.
const SWEEP_DEADLINE: Duration = Duration::from_secs(30);

/// The held owner lock and the armed reaper child of one fixture root.
///
/// Created by [`OwnerReaper::arm`]; dropping the value kills the helper and
/// reaps it before the lock is released, so the helper never acquires the lock
/// on the normal path.
#[derive(Debug)]
pub struct OwnerReaper {
    // Held only for its drop side effect: the kernel releases the `flock` when
    // the last descriptor closes, which is what wakes the helper.
    _lock: File,
    child: Child,
    // The per-user link the helper was given; removed on drop and after a sweep.
    link: PathBuf,
}

impl OwnerReaper {
    /// Arms the reaper for `root`.
    ///
    /// Creates `<root>/owner.lock`, takes the exclusive `flock` on it in this
    /// process, links it as `reaper-<pid>-<root name>.lock` in the per-user
    /// lock directory, and spawns `pohunek-test-reaper <link>` in its own
    /// process group, with all three standard streams null, an empty
    /// environment, and the working directory `/`.
    ///
    /// The helper is given the root only through the link, never through its
    /// command line or environment: tests attribute a process to a fixture
    /// root by scanning command lines and environments for the root path, and
    /// the helper must not be mistaken for a process of the root it reaps (it
    /// lives until this process ends, so a survivor scan would wait for it).
    /// An empty environment is also the hermetic choice: the helper outlives
    /// this process and must not carry any of its variables. A terminal
    /// signal to the test's foreground group does not reach the helper's own
    /// group, and the null streams mean it never holds the test runner's pipes.
    ///
    /// The helper blocks on the lock the link resolves to and sweeps the root
    /// once this process releases it, which the kernel does on any death of
    /// this process.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the lock file cannot be created or locked,
    /// when the reaper binary is missing (the error names
    /// `cargo build -p pohunek-test-support --bin pohunek-test-reaper`), or
    /// when the helper cannot be spawned.
    pub fn arm(root: &Path) -> io::Result<Self> {
        let lock_path = root.join(OWNER_LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&lock_path)?;
        std::fs::set_permissions(
            &lock_path,
            std::fs::Permissions::from_mode(PRIVATE_FILE_MODE),
        )?;
        // A fresh root is never locked by anyone else, so a contended lock is
        // an error, not a wait.
        match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(error) => {
                return Err(io::Error::other(format!(
                    "the owner lock {} is already held: {error}",
                    lock_path.display()
                )));
            }
        }
        let link = reaper_link_path(root);
        // A stale link of a recycled pid and root name is replaced; the name
        // carries the root's random directory name, so two live roots never
        // share one.
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&lock_path, &link)?;
        let spawn: io::Result<std::process::Child> = (|| {
            let mut command = Command::new(crate::try_reaper_binary()?);
            command
                .arg(&link)
                .env_clear()
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
            command.spawn()
        })();
        let child = match spawn {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_file(&link);
                return Err(error);
            }
        };
        Ok(Self {
            _lock: lock,
            child,
            link,
        })
    }
}

/// The per-user link path the reaper of `root` is given.
///
/// In the shared per-user lock directory, named after the owner pid and the
/// root's directory name, so it never contains the root path itself.
fn reaper_link_path(root: &Path) -> PathBuf {
    let name = root.file_name().map_or_else(
        || "root".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    crate::host_lock::lock_dir().join(format!("reaper-{}-{name}.lock", std::process::id()))
}

impl Drop for OwnerReaper {
    /// Kills the helper and reaps it before the lock field drops, so a test
    /// that ends normally never pays a sweep, and removes the helper's link.
    /// Never panics: a drop also runs while a panic unwinds.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.link);
    }
}

/// Sweeps `root`: signals every same-user process whose working directory is
/// the root or below it, and every transitive descendant of such a process,
/// with `SIGKILL`, rescanning until none is left or [`SWEEP_DEADLINE`] passes.
///
/// Returns how many signals were sent. The root is canonicalized first and
/// refused when it is not absolute, has fewer than two path components, or is
/// `/`, `/tmp`, `/var/tmp` or `$HOME` themselves, so a wrong argument can
/// never select a directory whose death would take unrelated processes down.
/// Each signal is preceded by a start-identity
/// re-check of the PID, so a process that reused the PID after the snapshot is
/// never signalled, and the reaper never signals itself.
///
/// # Errors
///
/// Returns an [`io::ErrorKind::InvalidInput`] error for an implausible root,
/// and the I/O error when the process table cannot be read.
pub fn sweep_root(root: &Path) -> io::Result<usize> {
    let root = canonical_root(root)?;
    let host = HostInspector::new();
    let deadline = Instant::now() + SWEEP_DEADLINE;
    let mut signalled = 0;
    loop {
        // A table that cannot be read right now counts as "not yet": the sweep
        // rescans until the deadline instead of giving up on a transient error.
        let scoped = match try_scoped_identities(&root) {
            Ok(scoped) => scoped,
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(error);
                }
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
        };
        if scoped.is_empty() {
            return Ok(signalled);
        }
        for identity in &scoped {
            // Fail closed: an inspection error is not evidence that the
            // process is the one the snapshot listed, so it is not signalled.
            if !matches!(host.is_running(*identity), Ok(true)) {
                continue;
            }
            if let Some(pid) = i32::try_from(identity.pid).ok().and_then(Pid::from_raw) {
                if kill_process(pid, Signal::KILL).is_ok() {
                    signalled += 1;
                }
            }
        }
        if Instant::now() >= deadline {
            return Ok(signalled);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Returns the identities of the same-user processes scoped by `root` in one
/// snapshot of the process table: those whose working directory is the root or
/// below it, plus every transitive descendant of them.
///
/// The reaper's own process is never included. A process whose working
/// directory cannot be read has exited or is between images and is not
/// listed.
///
/// # Errors
///
/// Returns an [`io::ErrorKind::InvalidInput`] error for an implausible root,
/// and the I/O error when the process table cannot be read.
pub fn try_scoped_identities(root: &Path) -> io::Result<Vec<ProcessIdentity>> {
    let root = canonical_root(root)?;
    let host = HostInspector::new();
    let snapshot = host
        .same_user_processes()
        .map_err(|error| io::Error::other(format!("read the process table: {error}")))?;
    let own_pid = std::process::id();
    let mut scoped: Vec<ProcessIdentity> = Vec::new();
    // Only the scoped processes and the reaper itself are in `seen`; a process
    // that is not scoped must stay eligible for the descendant closure below.
    let mut seen: HashSet<u32> = HashSet::from([own_pid]);
    let mut queue: Vec<u32> = Vec::new();
    for fact in &snapshot {
        if fact.pid == own_pid {
            continue;
        }
        let in_scope = host
            .cwd(fact.pid)
            .ok()
            .map(|cwd| std::fs::canonicalize(&cwd).unwrap_or(cwd))
            .is_some_and(|cwd| cwd.starts_with(&root));
        if in_scope {
            seen.insert(fact.pid);
            scoped.push(fact.identity());
            queue.push(fact.pid);
        }
    }
    // The descendant closure of the marked processes, from the same snapshot:
    // a session leader whose own working directory lies outside the root is
    // still a descendant of a worker whose working directory is inside it.
    while let Some(parent) = queue.pop() {
        for fact in &snapshot {
            if fact.ppid == parent && seen.insert(fact.pid) {
                scoped.push(fact.identity());
                queue.push(fact.pid);
            }
        }
    }
    Ok(scoped)
}

/// Canonicalizes `root` and refuses an implausible sweep root.
fn canonical_root(root: &Path) -> io::Result<PathBuf> {
    if !root.is_absolute() || root.components().count() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to sweep the implausible root {} (not absolute or fewer than two components)",
                root.display()
            ),
        ));
    }
    let canonical = std::fs::canonicalize(root).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot resolve the sweep root {}: {error}", root.display()),
        )
    })?;
    let mut refused = vec![
        PathBuf::from("/"),
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        refused.push(PathBuf::from(home));
    }
    if refused.contains(&canonical) || canonical.components().count() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to sweep the implausible root {} (resolves to {})",
                root.display(),
                canonical.display()
            ),
        ));
    }
    Ok(canonical)
}
