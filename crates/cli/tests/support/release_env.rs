//! The input contract of the release consumer suite.
//!
//! Every input is an absolute path or a runtime id handed over through the
//! environment. There is no fallback to `CARGO_BIN_EXE_*`, `target/` or the
//! source tree, so a run proves the exact files it was given. A value that is
//! missing, empty, relative or not the kind of file the contract names fails
//! with a message naming the variable.

// Rust guideline compliant 2026-10-08

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use package::ANCHOR_FILE_NAME;
use protocol::RuntimeId;

/// Directory holding the release `pohunek`, `pohunekd` and `pohunek-sessiond`
/// (and, in smoke mode, the archive's trust anchor file).
pub(crate) const BIN_DIR_VAR: &str = "POHUNEK_CONSUMER_BIN_DIR";

/// Runtime id of the one runtime this invocation exercises.
pub(crate) const RUNTIME_VAR: &str = "POHUNEK_CONSUMER_RUNTIME";

/// Package archive (`cargo xtask package build` output) installed by the run.
pub(crate) const PACKAGE_VAR: &str = "POHUNEK_CONSUMER_PACKAGE";

/// Path the consumer report is written to after a successful run.
pub(crate) const REPORT_VAR: &str = "POHUNEK_CONSUMER_REPORT";

/// Optional signed catalog; selects smoke mode when set.
pub(crate) const CATALOG_VAR: &str = "POHUNEK_CONSUMER_CATALOG";

/// The mandatory variables, in the order the contract documents them.
pub(crate) const MANDATORY_VARS: [&str; 4] = [BIN_DIR_VAR, RUNTIME_VAR, PACKAGE_VAR, REPORT_VAR];

/// The executables a release daemon archive ships and a run exercises.
pub(crate) const BINARIES: [&str; 3] = ["pohunek", "pohunekd", "pohunek-sessiond"];

/// Where the catalog and trust anchor come from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    /// A throwaway root, anchor and catalog generated in the test process.
    Row,
    /// The official signed catalog and the anchor shipped in the bin dir.
    Smoke,
}

/// Validated inputs of one run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Inputs {
    pub(crate) bin_dir: PathBuf,
    pub(crate) runtime: String,
    pub(crate) package: PathBuf,
    pub(crate) report: PathBuf,
    pub(crate) catalog: Option<PathBuf>,
}

impl Inputs {
    pub(crate) fn mode(&self) -> Mode {
        if self.catalog.is_some() {
            Mode::Smoke
        } else {
            Mode::Row
        }
    }
}

/// Why the inputs are unusable. Each variant names the offending variable.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum EnvError {
    Missing {
        var: &'static str,
    },
    Empty {
        var: &'static str,
    },
    NotAbsolute {
        var: &'static str,
        value: PathBuf,
    },
    NotDirectory {
        var: &'static str,
        path: PathBuf,
    },
    /// A file that must be a regular file is absent, a link, or another kind.
    NotRegularFile {
        var: &'static str,
        path: PathBuf,
    },
    InvalidRuntime {
        value: String,
    },
    /// The report path (or its temporary sibling) is one of the inputs, by
    /// path, link or alias; writing the report would destroy the input.
    ReportOverlaps {
        input: &'static str,
        path: PathBuf,
    },
    /// The report path cannot be written: its parent is missing or it names a
    /// directory.
    UnwritableReport {
        path: PathBuf,
    },
}

impl fmt::Display for EnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { var } => write!(
                f,
                "{var} is required by the release consumer suite and is not set"
            ),
            Self::Empty { var } => write!(f, "{var} is set but empty"),
            Self::NotAbsolute { var, value } => {
                write!(f, "{var} must be an absolute path, got {}", value.display())
            }
            Self::NotDirectory { var, path } => {
                write!(f, "{var}: {} is not a directory", path.display())
            }
            Self::NotRegularFile { var, path } => write!(
                f,
                "{var}: {} is not a regular file (missing, a link, or another kind of file)",
                path.display()
            ),
            Self::InvalidRuntime { value } => write!(
                f,
                "{RUNTIME_VAR} must be a runtime id (lowercase letters, digits, `-`), got {value:?}"
            ),
            Self::ReportOverlaps { input, path } => write!(
                f,
                "{REPORT_VAR} would overwrite {input}: {} is the same file as the report or its temporary file",
                path.display()
            ),
            Self::UnwritableReport { path } => write!(
                f,
                "{REPORT_VAR}: {} cannot be written (its directory is missing, or it is a directory)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for EnvError {}

/// Reads the contract through `lookup`.
///
/// Returns `Ok(None)` only when no `POHUNEK_CONSUMER_*` variable is set at all,
/// the state of a plain `cargo test`. Once any of them is set, every one of
/// the mandatory variables must be valid.
pub(crate) fn read_inputs(
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Option<Inputs>, EnvError> {
    let any_set = MANDATORY_VARS
        .iter()
        .chain([&CATALOG_VAR])
        .any(|var| lookup(var).is_some());
    if !any_set {
        return Ok(None);
    }
    let bin_dir = absolute(lookup, BIN_DIR_VAR)?;
    let runtime = text(lookup, RUNTIME_VAR)?;
    let package = absolute(lookup, PACKAGE_VAR)?;
    let report = absolute(lookup, REPORT_VAR)?;
    let catalog = match lookup(CATALOG_VAR) {
        Some(_) => Some(absolute(lookup, CATALOG_VAR)?),
        None => None,
    };

    if RuntimeId::parse(&runtime).is_err() {
        return Err(EnvError::InvalidRuntime { value: runtime });
    }
    if !bin_dir.is_dir() {
        return Err(EnvError::NotDirectory {
            var: BIN_DIR_VAR,
            path: bin_dir,
        });
    }
    for name in BINARIES {
        require_regular(BIN_DIR_VAR, &bin_dir.join(name))?;
    }
    if catalog.is_some() {
        require_regular(BIN_DIR_VAR, &bin_dir.join(ANCHOR_FILE_NAME))?;
    }
    require_regular(PACKAGE_VAR, &package)?;
    if let Some(catalog) = &catalog {
        require_regular(CATALOG_VAR, catalog)?;
    }
    let parent_exists = report.parent().is_some_and(Path::is_dir);
    if !parent_exists || report.is_dir() {
        return Err(EnvError::UnwritableReport { path: report });
    }
    Ok(Some(Inputs {
        bin_dir,
        runtime,
        package,
        report,
        catalog,
    }))
}

/// Suffix of the temporary file the report is written through.
pub(crate) const REPORT_PARTIAL_SUFFIX: &str = ".partial";

/// Refuses a report path that is, or aliases, any input.
///
/// Compares the report and its temporary sibling with every input that is set
/// (the three binaries and the anchor of the bin dir, the
/// package, the catalog) by path, by the canonical parent directory plus file
/// name (so a parent-directory symlink cannot hide it), and by device and
/// inode where both files exist (hard links, a symlinked report). A relative
/// path is taken relative to the working directory, so an input that
/// validation will reject later is protected as well. Nothing is created or
/// removed.
///
/// # Errors
///
/// Returns [`EnvError::ReportOverlaps`] naming the overlapped variable.
pub(crate) fn check_report_overlap(
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> Result<(), EnvError> {
    let Some(report) = lookup(REPORT_VAR)
        .filter(|value| !value.is_empty())
        .and_then(|value| std::path::absolute(value).ok())
    else {
        return Ok(());
    };
    let mut partial = report.as_os_str().to_owned();
    partial.push(REPORT_PARTIAL_SUFFIX);
    let written = [report, PathBuf::from(partial)];

    let mut inputs: Vec<(&'static str, PathBuf)> = Vec::new();
    if let Some(dir) = lookup(BIN_DIR_VAR).and_then(rooted) {
        for name in BINARIES.iter().copied().chain([ANCHOR_FILE_NAME]) {
            inputs.push((BIN_DIR_VAR, dir.join(name)));
        }
    }
    for var in [PACKAGE_VAR, CATALOG_VAR] {
        if let Some(path) = lookup(var).and_then(rooted) {
            inputs.push((var, path));
        }
    }
    for (var, input) in &inputs {
        if written.iter().any(|path| same_file(path, input)) {
            return Err(EnvError::ReportOverlaps {
                input: var,
                path: input.clone(),
            });
        }
    }
    Ok(())
}

/// A set, non-empty input as an absolute path; a relative one is taken
/// relative to the working directory, where it would be opened.
fn rooted(value: OsString) -> Option<PathBuf> {
    if value.is_empty() {
        return None;
    }
    std::path::absolute(value).ok()
}

/// The path with its parent directory canonicalized, so two spellings of one
/// directory compare equal even when the file does not exist yet.
fn resolved(path: &Path) -> PathBuf {
    match (path.parent().map(fs::canonicalize), path.file_name()) {
        (Some(Ok(parent)), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    if left == right || resolved(left) == resolved(right) {
        return true;
    }
    match (fs::metadata(left), fs::metadata(right)) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

fn text(lookup: &dyn Fn(&str) -> Option<OsString>, var: &'static str) -> Result<String, EnvError> {
    let value = lookup(var).ok_or(EnvError::Missing { var })?;
    let value = value
        .into_string()
        .map_err(|value| EnvError::InvalidRuntime {
            value: value.to_string_lossy().into_owned(),
        })?;
    if value.is_empty() {
        return Err(EnvError::Empty { var });
    }
    Ok(value)
}

fn absolute(
    lookup: &dyn Fn(&str) -> Option<OsString>,
    var: &'static str,
) -> Result<PathBuf, EnvError> {
    let value = lookup(var).ok_or(EnvError::Missing { var })?;
    if value.is_empty() {
        return Err(EnvError::Empty { var });
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(EnvError::NotAbsolute { var, value: path });
    }
    Ok(path)
}

/// Requires `path` to be a regular file that is not a link, so the bytes the
/// run hashes are the bytes the path names.
fn require_regular(var: &'static str, path: &Path) -> Result<(), EnvError> {
    let regular = fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file());
    if regular {
        Ok(())
    } else {
        Err(EnvError::NotRegularFile {
            var,
            path: path.to_path_buf(),
        })
    }
}
