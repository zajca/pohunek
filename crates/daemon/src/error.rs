//! Typed daemon errors.
//!
//! These cover daemon startup/runtime failures (path resolution, socket setup,
//! single-instance lock, framing). They map to the `configuration`/`daemon`
//! error classes from `docs/architecture.md` "Error Handling". Per the project
//! rule, no runtime-fallible path uses `unwrap()`; everything returns one of
//! these.

use std::io;
use std::net::IpAddr;
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

    /// An environment variable was present but not one of the accepted values.
    #[error("environment variable {var} has invalid value {value:?}; expected {expected}")]
    InvalidEnv {
        /// The invalid variable name.
        var: String,
        /// The invalid raw value.
        value: String,
        /// Human-readable accepted value set.
        expected: &'static str,
    },

    /// Another daemon already holds the single-instance lock.
    #[error("another pohunek daemon is already running (lock held: {lock})")]
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

    /// Bounded structured logging could not be initialized.
    #[error("failed to initialize bounded structured logging: {0}")]
    Logging(#[from] pohunek_logging::Error),

    /// The global tracing subscriber was already initialized.
    #[error("failed to install structured logging subscriber: {0}")]
    LoggingSubscriber(String),

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

    /// The resolved `NetBird` bind address was rejected (fails closed). The remote
    /// TCP control listener only ever binds an address inside the `NetBird` CGNAT
    /// range; any other address is refused before a socket is opened.
    #[error("refusing to bind control listener to non-NetBird address {addr}: {reason}")]
    NetbirdBind {
        /// The rejected bind address.
        addr: IpAddr,
        /// Why the address was rejected (from the `NetBird` validator).
        reason: String,
    },

    /// Durable logical sessions could not be loaded for startup reconciliation.
    #[error("session worker reconciliation failed: {0}")]
    Reconcile(#[source] protocol::ProtocolError),

    /// Generic I/O error not tied to a specific resource above.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}
