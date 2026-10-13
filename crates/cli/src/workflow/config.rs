//! Owner limits of workflow plugin runs, read from `<config>/pohunek/workflow.toml`.
//!
//! The file is optional: when it or its directory is absent, every knob has
//! its documented default. The directory, when it exists, must be one no
//! other account can write, whether or not the file is there, because another
//! account could otherwise plant the file. When the file exists it must be a
//! regular file owned by the user with mode `0600`, and
//! parsing is strict: unknown tables or keys, wrong types and out-of-range
//! values fail with [`Error::ConfigInvalid`] before anything runs. Tables of
//! later workflow slices are unknown keys here.

// Rust guideline compliant 2026-10-13

use std::io::ErrorKind;
use std::path::Path;

use pohunek_platform::filesystem::{FsError, TrustedDir};
use serde::Deserialize;

use super::Error;

/// File name of the knobs file in pohunek's config directory.
pub(crate) const FILE_NAME: &str = "workflow.toml";

/// Largest accepted knobs file, checked before parsing.
///
/// The same ceiling as a plugin manifest; the file holds a few integers.
const MAX_FILE_BYTES: usize = 64 * 1024;

/// Required mode of the knobs file: owner read and write only, like
/// `service.toml`, because the file sets safety limits.
const FILE_MODE: u32 = 0o600;

/// Directory mode bits that would let another account replace the file.
const FORBIDDEN_DIRECTORY_BITS: u32 = 0o022;

/// Hard ceiling of `limits.max_context_bytes`: 96 KiB.
///
/// The context travels as one environment string, and Linux refuses a
/// string longer than `MAX_ARG_STRLEN` (128 KiB) at `exec`; the margin keeps
/// the variable name and the rest of the environment below that bound.
pub(crate) const MAX_CONTEXT_BYTES: u64 = 96 * 1024;

/// Ceiling of any action timeout: one day. An interactive run that needs
/// longer is a hang, not a workflow step.
const MAX_ACTION_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

/// Ceiling of `limits.kill_grace_ms`: one minute between `SIGTERM` and
/// `SIGKILL` is ample for a plugin to clean up.
const MAX_KILL_GRACE_MS: u64 = 60 * 1000;

/// Ceiling of the per-stream output caps: 1 GiB. A captured stream is held
/// in memory, so the cap must stay addressable on every host.
const MAX_STREAM_BYTES: u64 = 1024 * 1024 * 1024;

/// Floor of `logs.run_log_file_bytes`: 4 KiB holds several run records, so a
/// record never exceeds an empty file.
const MIN_RUN_LOG_FILE_BYTES: u64 = 4 * 1024;

/// Ceiling of `logs.run_log_file_bytes`: 64 MiB, twice the daemon log file.
const MAX_RUN_LOG_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Ceiling of `logs.run_log_files`, the active file included.
const MAX_RUN_LOG_FILES: u64 = 64;

/// Default action deadline: one minute covers an interactive step.
const DEFAULT_ACTION_TIMEOUT_MS: u64 = 60 * 1000;

/// Default ceiling a manifest `timeout_ms` may request: ten minutes, the
/// platform's longest supervised job deadline (`MAX_JOB_TIMEOUT`).
const DEFAULT_ACTION_TIMEOUT_MAX_MS: u64 = 10 * 60 * 1000;

/// Default time between `SIGTERM` and `SIGKILL`: five seconds.
const DEFAULT_KILL_GRACE_MS: u64 = 5 * 1000;

/// Default stdout cap: 16 MiB, far above a report or JSON document and far
/// below what floods a terminal for minutes.
const DEFAULT_MAX_STDOUT_BYTES: u64 = 16 * 1024 * 1024;

/// Default stderr cap: 1 MiB of diagnostics.
const DEFAULT_MAX_STDERR_BYTES: u64 = 1024 * 1024;

/// Default context cap: 64 KiB, below [`MAX_CONTEXT_BYTES`].
const DEFAULT_MAX_CONTEXT_BYTES: u64 = 64 * 1024;

/// Default run log file size: 1 MiB holds thousands of run records.
const DEFAULT_RUN_LOG_FILE_BYTES: u64 = 1024 * 1024;

/// Default run log file count, the active file included: 4 MiB per plugin.
const DEFAULT_RUN_LOG_FILES: u64 = 4;

/// Run limits, the `[limits]` table.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Limits {
    /// Deadline of an action that declares no `timeout_ms`.
    pub(crate) action_timeout_default_ms: u64,
    /// Largest `timeout_ms` an action may declare; larger ones are refused.
    pub(crate) action_timeout_max_ms: u64,
    /// Time between `SIGTERM` and `SIGKILL` when a run is stopped; zero
    /// sends `SIGKILL` at once.
    pub(crate) kill_grace_ms: u64,
    /// Bytes of stdout a run may write.
    pub(crate) max_stdout_bytes: u64,
    /// Bytes of stderr a run may write.
    pub(crate) max_stderr_bytes: u64,
    /// Bytes of `POHUNEK_PLUGIN_CONTEXT`; a larger context refuses the run.
    pub(crate) max_context_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            action_timeout_default_ms: DEFAULT_ACTION_TIMEOUT_MS,
            action_timeout_max_ms: DEFAULT_ACTION_TIMEOUT_MAX_MS,
            kill_grace_ms: DEFAULT_KILL_GRACE_MS,
            max_stdout_bytes: DEFAULT_MAX_STDOUT_BYTES,
            max_stderr_bytes: DEFAULT_MAX_STDERR_BYTES,
            max_context_bytes: DEFAULT_MAX_CONTEXT_BYTES,
        }
    }
}

/// Run log retention, the `[logs]` table.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Logs {
    /// Bytes of one `runs.jsonl` file before it rotates.
    pub(crate) run_log_file_bytes: u64,
    /// Run log files kept per plugin, the active file included.
    pub(crate) run_log_files: u64,
}

impl Default for Logs {
    fn default() -> Self {
        Self {
            run_log_file_bytes: DEFAULT_RUN_LOG_FILE_BYTES,
            run_log_files: DEFAULT_RUN_LOG_FILES,
        }
    }
}

/// Every owner knob of workflow runs.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Knobs {
    /// The `[limits]` table.
    pub(crate) limits: Limits,
    /// The `[logs]` table.
    pub(crate) logs: Logs,
}

/// Reads the knobs file at `path`; an absent file or directory yields the
/// defaults.
///
/// # Errors
///
/// Returns [`Error::ConfigInvalid`] when the file is unsafe, unreadable,
/// too large, not TOML of the schema, or holds an out-of-range value. The
/// message names a line, a key and its allowed range, never file content.
pub(crate) fn load(path: &Path) -> Result<Knobs, Error> {
    let invalid = |reason: String| Error::ConfigInvalid {
        path: path.display().to_string(),
        reason,
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(invalid("the path has no file name".to_owned()));
    };
    let bytes = match TrustedDir::open_absolute_owner_safe(parent, FORBIDDEN_DIRECTORY_BITS)
        .and_then(|dir| dir.read_file(name, FILE_MODE, MAX_FILE_BYTES))
    {
        Ok(bytes) => bytes,
        Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => return Ok(Knobs::default()),
        Err(error) => return Err(invalid(unsafe_reason(&error))),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_not_utf8| invalid("the file is not UTF-8".to_owned()))?;
    // Parser messages can quote file values, so only the line survives.
    let knobs: Knobs = toml_edit::de::from_str(text).map_err(|error| {
        let line = error
            .span()
            .and_then(|span| text.get(..span.start))
            .map(|prefix| prefix.matches('\n').count() + 1);
        invalid(format!(
            "the file does not match the knobs schema (syntax, unknown key or wrong type){}",
            line.map(|line| format!(" at line {line}"))
                .unwrap_or_default()
        ))
    })?;
    check_ranges(&knobs).map_err(invalid)?;
    Ok(knobs)
}

/// Checks every knob against its documented range.
fn check_ranges(knobs: &Knobs) -> Result<(), String> {
    let limits = &knobs.limits;
    let logs = &knobs.logs;
    let ranges = [
        (
            "limits.action_timeout_max_ms",
            limits.action_timeout_max_ms,
            1,
            MAX_ACTION_TIMEOUT_MS,
        ),
        (
            "limits.action_timeout_default_ms",
            limits.action_timeout_default_ms,
            1,
            limits.action_timeout_max_ms,
        ),
        (
            "limits.kill_grace_ms",
            limits.kill_grace_ms,
            0,
            MAX_KILL_GRACE_MS,
        ),
        (
            "limits.max_stdout_bytes",
            limits.max_stdout_bytes,
            1,
            MAX_STREAM_BYTES,
        ),
        (
            "limits.max_stderr_bytes",
            limits.max_stderr_bytes,
            1,
            MAX_STREAM_BYTES,
        ),
        (
            "limits.max_context_bytes",
            limits.max_context_bytes,
            1,
            MAX_CONTEXT_BYTES,
        ),
        (
            "logs.run_log_file_bytes",
            logs.run_log_file_bytes,
            MIN_RUN_LOG_FILE_BYTES,
            MAX_RUN_LOG_FILE_BYTES,
        ),
        (
            "logs.run_log_files",
            logs.run_log_files,
            1,
            MAX_RUN_LOG_FILES,
        ),
    ];
    for (key, value, minimum, maximum) in ranges {
        if !(minimum..=maximum).contains(&value) {
            return Err(format!("{key} must be in {minimum}..={maximum}"));
        }
    }
    Ok(())
}

/// Describes why the knobs could not be read safely, by cause only.
fn unsafe_reason(error: &FsError) -> String {
    match error {
        FsError::FileTooLarge { .. } => format!("the file exceeds {MAX_FILE_BYTES} bytes"),
        FsError::Io { .. } => format!(
            "the file or its directory cannot be read: {}",
            error.io_kind().unwrap_or(ErrorKind::Other)
        ),
        _ => format!(
            "the directory must be owned by you and not writable by other accounts, even \
             without a {FILE_NAME}, and {FILE_NAME} must be a regular file owned by you with \
             mode {FILE_MODE:o}"
        ),
    }
}
