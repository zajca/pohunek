//! The kit error: the `xtask` failure surface the thin wrappers share.
//!
//! Every variant keeps the exact message `xtask` prints today, so moving a
//! command into the kit never changes what an operator reads.

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

use thiserror::Error;

/// Process exit code of a plugin proof that ran and failed, so a workflow
/// tells a failed proof from a refused invocation. Same value as
/// `PROVE_FAILED_EXIT_CODE` in `xtask`.
pub const PROVE_FAILED_EXIT_CODE: i32 = 1;

/// Process exit code of a plugin index document that fails verification, so a
/// workflow tells a refused document from a refused invocation. Same value as
/// `INDEX_REFUSED_EXIT_CODE` in `xtask`.
pub const INDEX_REFUSED_EXIT_CODE: i32 = 1;

/// Process exit code of a refused kit invocation. Same value as
/// `FAILURE_EXIT_CODE` in `xtask`.
pub const FAILURE_EXIT_CODE: i32 = 2;

/// Shell convention for a child killed by a signal: 128 plus the signal number.
const SIGNAL_EXIT_BASE: i32 = 128;

/// Every failure a kit command reports.
#[derive(Debug, Error)]
pub enum Error {
    /// A refused invocation or refused content; the text names it.
    #[error("{0}")]
    Usage(String),
    /// A file could not be read or written.
    #[error("failed to access `{path}`: {source}", path = .path.display())]
    Io { path: PathBuf, source: io::Error },
    /// A directory entry is not a regular file.
    #[error("unsupported file type in `{}`", .0.display())]
    UnsupportedFileType(PathBuf),
    /// JSON serialization failed.
    #[error("failed to serialize json: {0}")]
    Json(#[from] serde_json::Error),
    /// A package archive was refused by the strict reader.
    #[error("package archive: {0}")]
    Package(#[from] package::ArchiveError),
    /// A catalog could not be built, signed or verified.
    #[error("catalog: {0}")]
    Catalog(#[from] package::CatalogError),
    /// A trust anchor could not be built or read.
    #[error("trust anchor: {0}")]
    Anchor(#[from] package::AnchorFileError),
    /// A plugin index document failed the daemon's verification rules.
    #[error("{0}")]
    Index(#[from] package::IndexError),
    /// The archive output path lies inside the package directory it packs.
    #[error(
        "archive output `{}` is inside the package directory",
        .0.display()
    )]
    OutputInsideInput(PathBuf),
    /// The archive output path is a symbolic link.
    #[error("output `{}` is a symbolic link", .0.display())]
    OutputIsSymlink(PathBuf),
    /// A path cannot be represented as a deterministic relative path.
    #[error(
        "path `{}` cannot be represented as a deterministic relative path",
        .0.display()
    )]
    InvalidPath(PathBuf),
    /// A delegated command ran and exited unsuccessfully.
    #[error("`{command}` failed with {status}")]
    ChildExit { command: String, status: ExitStatus },
    /// A plugin proof ran and failed at a step; its evidence was written.
    #[error("{0}")]
    Prove(#[from] crate::prove::Verdict),
}

impl Error {
    /// Process exit code that reports this error. Same mapping as
    /// `XtaskError::exit_code`: a failed delegated command propagates its own
    /// exit code, a failed proof exits 1, every other error exits 2.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::ChildExit { status, .. } => status.code().unwrap_or_else(|| {
                use std::os::unix::process::ExitStatusExt as _;
                SIGNAL_EXIT_BASE + status.signal().unwrap_or(0)
            }),
            Self::Index(_) => INDEX_REFUSED_EXIT_CODE,
            Self::Prove(_) => PROVE_FAILED_EXIT_CODE,
            _ => FAILURE_EXIT_CODE,
        }
    }
}

/// Refuses with [`Error::Usage`] naming `what`.
pub fn refuse(what: impl Into<String>) -> Error {
    Error::Usage(what.into())
}
