//! Defines the worker's top-level typed error.

// Rust guideline compliant 2026-07-23

use std::path::PathBuf;

use crate::{ConfigError, JournalError, PtyError};

/// Failure while starting or serving one worker.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// Worker policy is invalid.
    #[error("invalid worker configuration: {0}")]
    Config(#[from] ConfigError),
    /// XDG path resolution failed.
    #[error("failed to resolve worker paths: {0}")]
    Paths(#[from] pohunek_paths::PathError),
    /// A session identifier is unsafe.
    #[error("invalid managed session id `{0}`")]
    InvalidSessionId(String),
    /// A worker identifier is unsafe.
    #[error("invalid worker id `{0}`")]
    InvalidWorkerId(String),
    /// A required filesystem operation failed.
    #[error("worker filesystem operation failed for {}: {source}", path.display())]
    Filesystem {
        /// Affected path.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Bounded structured logging could not be initialized.
    #[error("failed to initialize bounded worker logging: {0}")]
    Logging(#[from] pohunek_logging::Error),
    /// The private Unix endpoint failed.
    #[error("worker socket operation failed for {}: {source}", path.display())]
    Socket {
        /// Affected socket path.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Durable worker state failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// PTY setup or operation failed.
    #[error(transparent)]
    Pty(#[from] PtyError),
    /// Private protocol handling failed.
    #[error("worker protocol failed: {0}")]
    Protocol(String),
    /// No valid controller initialized the worker in time.
    #[error("worker initialization deadline elapsed")]
    InitializeTimeout,
}
