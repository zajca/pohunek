//! The handshake file the worker writes for hooks that run without the
//! `POHUNEK_*` environment.
//!
//! An agent can start a hook environment that lacks the variables the worker
//! put into its PTY (Muse does after `/clear`). The daemon names a file in the
//! session's launch-input directory; the worker writes the routing values a hook
//! needs into it after it chose its instance id and before it starts the agent.
//! The file holds no secret: every value is also in the agent's own environment.

// Rust guideline compliant 2026-10-27

use std::path::Path;

use pohunek_platform::filesystem::TrustedDir;
use serde::Serialize;
use thiserror::Error;

/// Mode of the session directory that holds the file.
const DIRECTORY_MODE: u32 = 0o700;

/// Mode of the handshake file: the owner reads and writes.
const FILE_MODE: u32 = 0o600;

/// Layout version of the file content; a reader refuses any other value.
const FORMAT_VERSION: u32 = 1;

/// Why the handshake file was not written.
#[derive(Debug, Error)]
pub(crate) enum HandshakeError {
    /// The target is not a launch-input file beside the worker's own journal.
    #[error("handshake file {0} is not a launch-input file of this session")]
    NotSessionFile(String),
    /// The directory cannot be opened or the file cannot be replaced.
    #[error("handshake file cannot be written: {0}")]
    Write(String),
    /// The content cannot be encoded.
    #[error("handshake content cannot be encoded: {0}")]
    Encode(#[from] serde_json::Error),
}

/// The routing values a hook needs to reach this worker.
#[derive(Debug, Serialize)]
pub(crate) struct Handshake<'a> {
    /// Layout version.
    version: u32,
    /// Durable logical session.
    session_id: &'a str,
    /// Instance id the worker attests hook reports against.
    worker_instance_id: &'a str,
    /// Owner-private worker socket.
    worker_socket_path: &'a str,
    /// Shared hook failure trace.
    hook_trace_path: &'a str,
    /// Size at which the hook trace rotates.
    hook_trace_max_bytes: u64,
    /// Kind of native session reference the runtime reports, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    native_reference_kind: Option<&'a str>,
}

impl<'a> Handshake<'a> {
    /// The handshake of the worker instance `worker_instance_id`.
    pub(crate) fn new(
        session_id: &'a str,
        worker_instance_id: &'a str,
        worker_socket_path: &'a str,
        hook_trace_path: &'a str,
        hook_trace_max_bytes: u64,
        native_reference_kind: Option<&'a str>,
    ) -> Self {
        Self {
            version: FORMAT_VERSION,
            session_id,
            worker_instance_id,
            worker_socket_path,
            hook_trace_path,
            hook_trace_max_bytes,
            native_reference_kind,
        }
    }

    /// Writes the handshake to `target`, replacing any earlier copy atomically.
    ///
    /// `target` must be a launch-input file in the directory that holds
    /// `journal_path`, so the daemon cannot make the worker write elsewhere.
    pub(crate) fn write(&self, target: &Path, journal_path: &Path) -> Result<(), HandshakeError> {
        let refused = || HandshakeError::NotSessionFile(target.display().to_string());
        let parent = target.parent().ok_or_else(refused)?;
        let name = target
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(refused)?;
        let is_final_name = pohunek_paths::is_launch_input_entry(name) && !name.starts_with('.');
        if !is_final_name || journal_path.parent() != Some(parent) {
            return Err(refused());
        }
        let stem = name
            .strip_prefix(pohunek_paths::LAUNCH_INPUT_PREFIX)
            .ok_or_else(refused)?;
        let bytes = serde_json::to_vec(self)?;
        let directory = TrustedDir::open_absolute(parent, DIRECTORY_MODE)
            .map_err(|error| HandshakeError::Write(error.to_string()))?;
        directory
            .replace_file(
                name,
                pohunek_paths::launch_input_temp_name(stem),
                &bytes,
                FILE_MODE,
            )
            .map_err(|error| HandshakeError::Write(error.to_string()))
    }
}
