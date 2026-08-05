//! Defines the application log-retention policies and owned file families.

// Rust guideline compliant 2026-07-28

use crate::{Error, Files, Legacy, Policy};

/// Maximum bytes in one daemon log file.
///
/// A 32 MiB file remains practical to inspect while rotating quickly during a
/// runaway logging loop.
pub const DAEMON_MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Maximum daemon files, including the active file.
///
/// Eight files cap the daemon family at 256 MiB while preserving several
/// rotations for diagnosis.
pub const DAEMON_MAX_FILES: usize = 8;

/// Maximum bytes in one session-worker log file.
///
/// Worker logs are numerous, so each file is limited to 4 MiB.
pub const WORKER_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Maximum worker files for one session, including the active file.
///
/// Four files cap every session-owned worker log family at 16 MiB across all
/// worker generations.
pub const WORKER_MAX_FILES: usize = 4;

/// Builds the daemon log policy.
///
/// # Errors
///
/// Returns [`Error::InvalidPolicy`] if an application constant is zero.
pub fn daemon_policy() -> Result<Policy, Error> {
    Policy::new(DAEMON_MAX_FILE_BYTES, DAEMON_MAX_FILES)
}

/// Builds the session-worker log policy.
///
/// # Errors
///
/// Returns [`Error::InvalidPolicy`] if an application constant is zero.
pub fn worker_policy() -> Result<Policy, Error> {
    Policy::new(WORKER_MAX_FILE_BYTES, WORKER_MAX_FILES)
}

/// Builds the daemon-owned file family.
///
/// Legacy daily files used the `pohunekd.log.YYYY-MM-DD` prefix and are pruned
/// during initialization.
///
/// # Errors
///
/// Returns [`Error::InvalidName`] if an application filename is unsafe.
pub fn daemon_files() -> Result<Files, Error> {
    Files::new("pohunekd.jsonl", Legacy::prefix("pohunekd.log."))
}

/// Builds the worker-owned file family for one session.
///
/// All worker generations for a logical session intentionally share this
/// family. The writer's process-safe lock keeps their aggregate retention
/// bounded.
///
/// # Errors
///
/// Returns [`Error::InvalidName`] if `session_id` is not safe in a filename.
pub fn worker_files(session_id: &str) -> Result<Files, Error> {
    Files::new(format!("pohunek-session-{session_id}.jsonl"), Legacy::None)
}
