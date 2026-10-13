//! The per-plugin run log, `<plugin state dir>/logs/runs.jsonl`.
//!
//! Every run of an identified plugin appends one JSON record through the
//! bounded, rotated [`pohunek_logging::Writer`], whose size and file count
//! come from the owner knobs. A record holds identities, an outcome, an
//! error code, an exit code or signal, counts and a duration only: no argv
//! value, environment value, context or output byte has a field to land in.

// Rust guideline compliant 2026-10-13

use std::path::Path;

use pohunek_logging::{Files, Legacy, Policy, Writer};
use pohunek_platform::filesystem::TrustedDir;
use protocol::{PackageDigest, PackageVersion};
use serde::Serialize;

use super::config::Logs;
use super::Error;

/// Active file name of the run log family.
const FILE_NAME: &str = "runs.jsonl";

/// Subdirectory of the plugin state directory that holds the run log.
const LOGS_DIR: &str = "logs";

/// Mode of the plugin state directory and its `logs` subdirectory.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
}

/// One run record.
#[derive(Debug, Serialize)]
pub(crate) struct Record<'a> {
    /// RFC 3339 UTC time the run started.
    pub(crate) ts: String,
    /// ULID of the run, also in the child's context.
    pub(crate) run_id: &'a str,
    /// Invocation kind; `action` for `workflow run`.
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
