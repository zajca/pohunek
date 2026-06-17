//! Typed CLI errors.
//!
//! These cover client-side failures: missing env, daemon-unreachable, framing,
//! protocol errors returned by the daemon, and version mismatch. They are
//! rendered for humans on stderr and, where a command supports `--json`, can be
//! surfaced as structured error output.

use std::io;
use std::path::PathBuf;

use protocol::ProtocolError;

/// CLI error.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// A required environment variable is missing (fail fast, no invented path).
    #[error("required environment variable {var} is not set (no safe default exists)")]
    MissingEnv {
        /// The missing variable name.
        var: String,
    },

    /// The daemon socket could not be reached (likely not running).
    #[error("cannot reach the daemon at {socket}: {source}\nhint: start it with `zagentmesh daemon start`")]
    DaemonUnreachable {
        /// The socket path that was dialed.
        socket: PathBuf,
        /// Underlying connection error.
        #[source]
        source: io::Error,
    },

    /// A control line could not be framed/parsed.
    #[error("protocol framing error: {0}")]
    Framing(String),

    /// The daemon returned a typed protocol error.
    #[error("daemon error: {0}")]
    Protocol(#[from] ProtocolError),

    /// A remote (non-local) target was requested before Phase 2 transport
    /// exists. Parsing is host-aware now; execution is local-only.
    #[error("remote target '{host}' is not supported yet (Phase 1 is local-only)")]
    RemoteNotSupported {
        /// The requested host name.
        host: String,
    },

    /// Failed to spawn the daemon process.
    #[error("failed to start daemon: {0}")]
    Spawn(String),

    /// Generic I/O error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// JSON (de)serialization error at the client edge.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
