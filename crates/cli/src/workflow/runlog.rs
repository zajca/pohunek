//! The per-plugin run log, `<plugin state dir>/logs/runs.jsonl`.
//!
//! Every run of an identified plugin appends one JSON record through the
//! bounded, rotated [`pohunek_logging::Writer`], whose size and file count
//! come from the owner knobs. A record holds identities, an outcome, an
//! error code, an exit code or signal, counts and a duration only: no argv
//! value, environment value, context or output byte has a field to land in.

// Rust guideline compliant 2026-10-13

use std::path::Path;

use std::io::ErrorKind;

use pohunek_logging::{Files, Legacy, Policy, Writer};
use pohunek_platform::filesystem::{FileLock, FsError, LockKind, TrustedDir};
use protocol::{PackageDigest, PackageVersion};
use serde::{Deserialize, Serialize};

use super::config::Logs;
use super::Error;

/// Active file name of the run log family.
const FILE_NAME: &str = "runs.jsonl";

/// Subdirectory of the plugin state directory that holds the run log.
const LOGS_DIR: &str = "logs";

/// Mode of the plugin state directory and its `logs` subdirectory.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Mode of the run log files.
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Attempts to take the run log family lock for a read. The writer holds it
/// exclusively only while it appends or rotates one record, a few
/// milliseconds, so this many attempts at [`RUN_LOG_LOCK_RETRY`] apart wait
/// half a second at most before the read reports the log as busy.
const RUN_LOG_LOCK_ATTEMPTS: u32 = 50;

/// Pause between two attempts to take the run log family lock.
const RUN_LOG_LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(10);

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The child exited with status zero.
    Ok,
    /// The child exited non-zero or was killed by a signal it was not sent
    /// by the runner.
    ExitNonzero,
    /// The deadline expired and the runner stopped the group.
    Timeout,
    /// An output stream passed its cap and the runner stopped the group.
    OutputLimit,
    /// The CLI was signalled and the runner stopped the group.
    Interrupted,
    /// The child could not be started.
    SpawnFailed,
    /// The runner could not observe the child or read its output; the group
    /// was killed.
    Failed,
    /// The run was refused before anything was started.
    Refused,
    /// A JSON-mode child exited zero, but its stdout did not end: a process
    /// outside the group held it open.
    OutputIncomplete,
    /// A JSON-mode child exited zero, but its stdout is not exactly one JSON
    /// document.
    OutputInvalid,
    /// A supervised run did not start because an earlier run of the same
    /// job or service still holds its lock.
    OverlapSkipped,
}

impl Outcome {
    /// Whether the outcome counts toward a job's consecutive failures: any
    /// outcome except a success, and an overlap skip, which started no run.
    pub(crate) fn is_failure(self) -> bool {
        !matches!(self, Self::Ok | Self::OverlapSkipped)
    }
}

/// One run record.
#[derive(Debug, Serialize)]
pub(crate) struct Record<'a> {
    /// RFC 3339 UTC time the run started.
    pub(crate) ts: String,
    /// ULID of the run, also in the child's context.
    pub(crate) run_id: &'a str,
    /// Invocation kind: `action` for `workflow run`, `job` or `service` for
    /// a supervised run.
    pub(crate) kind: &'static str,
    /// The declared action name; absent when the name was not verified to be
    /// one the installed manifest declares.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<&'a str>,
    /// Installed plugin version.
    pub(crate) version: &'a PackageVersion,
    /// Installed archive digest.
    pub(crate) digest: &'a PackageDigest,
    /// How the run ended.
    pub(crate) outcome: Outcome,
    /// The stable code of the error the run ended with, when it ended with
    /// one: the same code the CLI reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_code: Option<&'static str>,
    /// The child's exit code, when it exited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    /// The signal that ended the child, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) signal: Option<i32>,
    /// Milliseconds from the start of the run to its end.
    pub(crate) duration_ms: u64,
    /// Stdout bytes delivered, at most the cap.
    pub(crate) stdout_bytes: u64,
    /// Stderr bytes delivered, at most the cap.
    pub(crate) stderr_bytes: u64,
    /// Number of extra command-line arguments given.
    pub(crate) arg_count: usize,
    /// The restart delay a service run waited before it started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backoff_ms: Option<u64>,
}

/// An open run log of one plugin.
#[derive(Debug)]
pub(crate) struct RunLog {
    writer: Writer,
}

impl RunLog {
    /// Opens the run log below `state_dir`, which must exist with mode
    /// `0700`; its `logs` subdirectory is created when missing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RunLog`] when a directory is missing or unsafe, or
    /// the log family cannot be opened.
    pub(crate) fn open(state_dir: &Path, logs: Logs) -> Result<Self, Error> {
        let failed = |detail: String| Error::RunLog { detail };
        let dir = TrustedDir::open_absolute(state_dir, PRIVATE_DIR_MODE)
            .and_then(|state| state.open_or_create_child(LOGS_DIR, PRIVATE_DIR_MODE))
            .map_err(|error| failed(error.to_string()))?;
        let files_count = usize::try_from(logs.run_log_files)
            .map_err(|_too_many| failed("the run log file count is too large".to_owned()))?;
        let policy = Policy::new(logs.run_log_file_bytes, files_count)
            .map_err(|error| failed(error.to_string()))?;
        let files =
            Files::new(FILE_NAME, Legacy::None).map_err(|error| failed(error.to_string()))?;
        let writer =
            Writer::open(dir.path(), files, policy).map_err(|error| failed(error.to_string()))?;
        Ok(Self { writer })
    }

    /// Appends `record` as one line.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RunLog`] when the record cannot be written.
    pub(crate) fn append(&mut self, record: &Record<'_>) -> Result<(), Error> {
        let mut line = serde_json::to_vec(record).map_err(|error| Error::RunLog {
            detail: error.to_string(),
        })?;
        line.push(b'\n');
        // The writer takes each `write` whole as one event and rotates only
        // between events, so this is one call and the record and its newline
        // stay in one file.
        std::io::Write::write_all(&mut self.writer, &line).map_err(|error| Error::RunLog {
            detail: error.to_string(),
        })
    }
}

/// One run record read back from the run log.
///
/// Only the fields status needs are read; a record's other fields, which
/// hold no value a run was given, are ignored.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ReadRecord {
    /// RFC 3339 UTC time the run started.
    pub(crate) ts: String,
    /// Invocation kind as the record names it.
    pub(crate) kind: String,
    /// The declared name, when the record has one.
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// Archive digest the run used.
    pub(crate) digest: PackageDigest,
    /// How the run ended.
    pub(crate) outcome: Outcome,
    /// The child's exit code, when it exited.
    #[serde(default)]
    pub(crate) exit_code: Option<i32>,
    /// The signal that ended the child, when one did.
    #[serde(default)]
    pub(crate) signal: Option<i32>,
    /// The stable code of the error the run ended with, when it ended with one.
    #[serde(default)]
    pub(crate) error_code: Option<String>,
}

/// Reads the retained run records below `state_dir`, newest first.
///
/// The active file and its rotations are read, at most `logs.run_log_files`
/// of them and each at most `logs.run_log_file_bytes`: the bounds the writer
/// keeps the family within, so a status read never grows past them. The files
/// are read under a shared hold of the writer's family lock, so a rotation
/// never moves a file between two reads and no record is read twice or
/// missed. A line that does not parse is skipped. A missing log directory
/// holds no records.
///
/// # Errors
///
/// Returns [`Error::RunLog`] when a directory is unsafe, the family lock stays
/// held by a writer, a file is larger than its bound, or a file cannot be read.
pub(crate) fn recent(state_dir: &Path, logs: Logs) -> Result<Vec<ReadRecord>, Error> {
    let failed = |detail: String| Error::RunLog { detail };
    let Some(dir) = open_logs_dir(state_dir)? else {
        return Ok(Vec::new());
    };
    let max_bytes = usize::try_from(logs.run_log_file_bytes)
        .map_err(|_too_large| failed("the run log file size is too large".to_owned()))?;
    let files = usize::try_from(logs.run_log_files)
        .map_err(|_too_many| failed("the run log file count is too large".to_owned()))?;
    let _family = lock_family(&dir)?;
    let mut records = Vec::new();
    for index in 0..files {
        // The writer keeps the newest records in the active file and moves
        // older ones to `.1`, `.2`, and so on.
        let name = if index == 0 {
            FILE_NAME.to_owned()
        } else {
            format!("{FILE_NAME}.{index}")
        };
        let bytes = match dir.read_file(&name, PRIVATE_FILE_MODE, max_bytes) {
            Ok(bytes) => bytes,
            Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => continue,
            Err(error) => return Err(failed(error.to_string())),
        };
        records.extend(
            bytes
                .split(|byte| *byte == b'\n')
                .rev()
                .filter_map(|line| serde_json::from_slice::<ReadRecord>(line).ok()),
        );
    }
    Ok(records)
}

/// Takes a shared hold of the run log family lock the writer serializes on,
/// retrying while a writer holds it.
fn lock_family(dir: &TrustedDir) -> Result<FileLock, Error> {
    let name = format!("{FILE_NAME}.lock");
    let mut attempt = 1;
    loop {
        match dir.lock_file(&name, PRIVATE_FILE_MODE, LockKind::Shared) {
            Ok(lock) => return Ok(lock),
            Err(FsError::LockContended { .. }) if attempt < RUN_LOG_LOCK_ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(RUN_LOG_LOCK_RETRY);
            }
            Err(FsError::LockContended { .. }) => {
                return Err(Error::RunLog {
                    detail: "the run log is being written; retry".to_owned(),
                });
            }
            Err(error) => {
                return Err(Error::RunLog {
                    detail: error.to_string(),
                })
            }
        }
    }
}

/// Opens the `logs` directory below `state_dir`; `None` when either is missing.
fn open_logs_dir(state_dir: &Path) -> Result<Option<TrustedDir>, Error> {
    let failed = |detail: String| Error::RunLog { detail };
    let state = match TrustedDir::open_absolute(state_dir, PRIVATE_DIR_MODE) {
        Ok(state) => state,
        Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => return Ok(None),
        Err(error) => return Err(failed(error.to_string())),
    };
    match state.open_child(LOGS_DIR, PRIVATE_DIR_MODE) {
        Ok(dir) => Ok(Some(dir)),
        Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => Ok(None),
        Err(error) => Err(failed(error.to_string())),
    }
}
