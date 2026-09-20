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

use std::io;
use std::path::{Path, PathBuf};

use pohunek_platform::filesystem::{AdvisoryLock, FsError, TrustedDir};

use crate::error::DaemonError;

/// An acquired single-instance lock. Drop releases it (the kernel also releases
/// it on process exit).
#[derive(Debug)]
pub struct InstanceLock {
    // The retained directory and marker descriptors keep both advisory locks
    // alive until this value is dropped.
    _lock: AdvisoryLock,
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
        let parent = path.parent().ok_or_else(|| DaemonError::Lock {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "lock path has no parent"),
        })?;
        let name = path.file_name().ok_or_else(|| DaemonError::Lock {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "lock path has no filename"),
        })?;
        let directory =
            TrustedDir::open_absolute(parent, 0o700).map_err(|source| DaemonError::Lock {
                path: path.to_path_buf(),
                source: io::Error::other(source),
            })?;
        let lock = directory
            .acquire_lock(name, 0o600)
            .map_err(|source| match source {
                FsError::LockContended { .. } => DaemonError::AlreadyRunning {
                    lock: path.to_path_buf(),
                },
                other => DaemonError::Lock {
                    path: path.to_path_buf(),
                    source: io::Error::other(other),
                },
            })?;

        Ok(Self {
            _lock: lock,
            path: path.to_path_buf(),
        })
    }

    /// The lock file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}
