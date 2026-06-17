//! Typed daemon errors.
//!
//! These cover daemon startup/runtime failures (path resolution, socket setup,
//! single-instance lock, framing). They map to the `configuration`/`daemon`
//! error classes from `docs/architecture.md` "Error Handling". Per the project
//! rule, no runtime-fallible path uses `unwrap()`; everything returns one of
//! these.

use std::io;
use std::path::PathBuf;

/// Daemon-level error.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// A required environment variable is missing. Fail fast, never invent a
    /// path (project rule: no silent invented defaults).
    #[error("required environment variable {var} is not set (no safe default exists)")]
    MissingEnv {
        /// The missing variable name.
        var: String,
    },

    /// Another daemon already holds the single-instance lock.
    #[error("another zagentmesh daemon is already running (lock held: {lock})")]
    AlreadyRunning {
        /// The lock file path.
        lock: PathBuf,
    },

    /// Failed to create or set permissions on a directory.
    #[error("failed to prepare directory {path}: {source}")]
    Directory {
        /// The directory path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// Failed to bind, configure, or operate the Unix socket.
    #[error("socket error at {path}: {source}")]
    Socket {
        /// The socket path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// Failed to acquire or operate the single-instance lock file.
    #[error("lock error at {path}: {source}")]
    Lock {
        /// The lock file path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// Generic I/O error not tied to a specific resource above.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}
