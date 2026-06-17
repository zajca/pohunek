//! Single-instance lock.
//!
//! Prevents two daemons from owning the same state directory (see
//! `docs/architecture.md` "Concurrency and supervision": "a single-instance lock
//! prevents two daemons owning the same state directory"). Implemented as an
//! advisory `flock(LOCK_EX | LOCK_NB)` on a lock file held open for the daemon's
//! lifetime.
//!
//! flock is used (rather than a PID file with `O_EXCL`) because the lock is
//! released automatically by the kernel when the holding process dies — even on
//! a crash — so there is no stale-lock problem to recover from. A second daemon
//! simply fails to acquire it.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::error::DaemonError;

/// An acquired single-instance lock. Drop releases it (the kernel also releases
/// it on process exit).
#[derive(Debug)]
pub struct InstanceLock {
    // The open file must be kept alive for the duration of the lock: closing the
    // fd releases the flock. Held but not otherwise read.
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Try to acquire the single-instance lock at `path`.
    ///
    /// The parent directory must already exist (the daemon creates it first).
    ///
    /// # Errors
    ///
    /// - [`DaemonError::AlreadyRunning`] if another process holds the lock.
    /// - [`DaemonError::Lock`] for other I/O or syscall failures.
    pub fn acquire(path: &Path) -> Result<Self, DaemonError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| DaemonError::Lock {
                path: path.to_path_buf(),
                source,
            })?;

        // SAFETY: `file` owns a valid open fd for the duration of this call.
        // flock with LOCK_NB never blocks; it returns EWOULDBLOCK if contended.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => {
                    Err(DaemonError::AlreadyRunning {
                        lock: path.to_path_buf(),
                    })
                }
                _ => Err(DaemonError::Lock {
                    path: path.to_path_buf(),
                    source: err,
                }),
            };
        }

        Ok(Self {
            _file: file,
            path: path.to_path_buf(),
        })
    }

    /// The lock file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}
