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
use std::time::Instant;

use rustix::fs::{flock, FlockOperation};
use rustix::io::Errno;

use crate::wait::{HANG_GUARD, POLL_INTERVAL};

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
    /// `holder` names the test in the lock file, so a run that waits too long
    /// can say who held it.
    ///
    /// # Panics
    ///
    /// Panics when the lock directory is not a private directory of the current
    /// user, when the lock file cannot be opened or written, or when another
    /// process still holds the lock after [`HANG_GUARD`]; the message names that
    /// holder.
    #[must_use]
    pub fn acquire(name: &str, holder: &str) -> Self {
        let path = lock_dir().join(format!("{name}.lock"));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc_o_nofollow())
            .open(&path)
            .unwrap_or_else(|error| panic!("open the host lock {}: {error}", path.display()));
        let started = Instant::now();
        loop {
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(Errno::WOULDBLOCK | Errno::INTR) => {}
                Err(error) => panic!("lock the host lock {}: {error}", path.display()),
            }
            assert!(
                started.elapsed() < HANG_GUARD,
                "the host lock {} is still held after {:?} by: {}",
                path.display(),
                started.elapsed(),
                current_holder(&mut file)
            );
            std::thread::sleep(POLL_INTERVAL);
        }
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| write!(file, "pid={} test={holder}", std::process::id()))
            .unwrap_or_else(|error| panic!("record the holder of {}: {error}", path.display()));
        Self { _file: file }
    }
}

/// Reads who holds the lock, for a timeout message.
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
