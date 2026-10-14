//! `cargo xtask docs log`: the unreleased knowledge-log fragments on disk.
//!
//! The rules of a fragment and the order of the entries live in
//! [`knowledge::log`]; this module reads and writes the files of a source
//! bundle with them:
//!
//! - `fold --version <X.Y.Z>` writes the fragments into `log.md` under a
//!   `## <X.Y.Z>` heading and removes them, as `scripts/release` does in the
//!   release commit.
//! - `migrate` turns every `## Unreleased (...)` section of `log.md` into a
//!   fragment, for a branch that still edits the shared log.
//! - [`assemble_into`] gives `docs build` the log with the fragments in place.
//!
//! `--dry-run` prints what would change and writes nothing.

// Rust guideline compliant 2026-10-14

use std::fs;
use std::path::{Path, PathBuf};

use knowledge::log::{self, LogError, FRAGMENT_DIR, LOG_FILE};

use crate::XtaskError;

/// What `fold` did, or would do on a dry run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldSummary {
    /// The fragments folded, in the order of the release section.
    pub folded: Vec<String>,
}

/// What `migrate` did, or would do on a dry run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateSummary {
    /// The fragment files written, in the order of their sections in the log.
    pub written: Vec<String>,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> XtaskError + '_ {
    move |source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Folds the fragments of `source_dir` into its `log.md` under `## <version>`
/// and removes them.
///
/// The log is written before any fragment is removed, so an interrupted fold
/// leaves fragments that are also in the log, never an entry in neither; a
/// repeated fold then only removes them (see [`log::fold`]).
///
/// # Errors
///
/// Returns [`XtaskError::KnowledgeLog`] for a fragment that breaks the rules or
/// a log without a title, and [`XtaskError::Io`] when a file cannot be read,
/// written or removed.
pub fn fold(source_dir: &Path, version: &str, dry_run: bool) -> Result<FoldSummary, XtaskError> {
    let fragment_dir = source_dir.join(FRAGMENT_DIR);
    let fragments = log::read_fragments(&fragment_dir).map_err(XtaskError::KnowledgeLog)?;
    let log_path = source_dir.join(LOG_FILE);
    let current = fs::read_to_string(&log_path).map_err(io(&log_path))?;
    log::check_log(&log_path, &current).map_err(XtaskError::KnowledgeLog)?;
    let folded = log::fold(&current, &fragments, version).map_err(XtaskError::KnowledgeLog)?;
    let summary = FoldSummary {
        folded: fragments
            .iter()
            .map(|fragment| fragment.file_name().to_owned())
            .collect(),
    };
    if dry_run || fragments.is_empty() {
        return Ok(summary);
    }
    replace_file(&log_path, &folded)?;
    for fragment in &fragments {
        let path = fragment_dir.join(fragment.file_name());
        fs::remove_file(&path).map_err(io(&path))?;
    }
    remove_if_empty(&fragment_dir)?;
    Ok(summary)
}

/// Removes `dir` when nothing is left in it.
fn remove_if_empty(dir: &Path) -> Result<(), XtaskError> {
    let mut entries = fs::read_dir(dir).map_err(io(dir))?;
    if entries.next().is_none() {
        fs::remove_dir(dir).map_err(io(dir))?;
    }
    Ok(())
}

/// Moves every `## Unreleased (...)` section of the `log.md` of `source_dir`
/// into a fragment of its own.
///
/// The fragments are written before the log loses the sections, so an
/// interrupted migration leaves an entry in both places, never in neither.
///
/// # Errors
///
/// Returns [`XtaskError::KnowledgeLog`] for a section whose fragment could not
/// be named or would break the rules, and [`XtaskError::Io`] when a file cannot
/// be read or written, or a fragment of the same name already exists.
pub fn migrate(source_dir: &Path, dry_run: bool) -> Result<MigrateSummary, XtaskError> {
    let fragment_dir = source_dir.join(FRAGMENT_DIR);
    let log_path = source_dir.join(LOG_FILE);
    let current = fs::read_to_string(&log_path).map_err(io(&log_path))?;
    let taken = existing_names(&fragment_dir)?;
    let (kept, extracted) =
        log::split_unreleased(&current, &taken).map_err(XtaskError::KnowledgeLog)?;
    let summary = MigrateSummary {
        written: extracted
            .iter()
            .map(|fragment| fragment.file_name.clone())
            .collect(),
    };
    if dry_run || extracted.is_empty() {
        return Ok(summary);
    }
    fs::create_dir_all(&fragment_dir).map_err(io(&fragment_dir))?;
    let mut written: Vec<PathBuf> = Vec::new();
    let outcome = extracted
        .iter()
        .try_for_each(|fragment| {
            let path = fragment_dir.join(&fragment.file_name);
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(io(&path))?;
            written.push(path.clone());
            std::io::Write::write_all(&mut file, fragment.content.as_bytes()).map_err(io(&path))
        })
        .and_then(|()| replace_file(&log_path, &kept));
    if let Err(error) = outcome {
        // The log is replaced only as a whole, so it still holds every section
        // and the fragments written so far are removed; a repeated migration
        // then names them the same way.
        for path in &written {
            if let Err(cleanup) = fs::remove_file(path) {
                return Err(XtaskError::Usage(format!(
                    "{error}; removing the partial fragment `{}` failed too: {cleanup}",
                    path.display()
                )));
            }
        }
        return Err(error);
    }
    Ok(summary)
}

/// Replaces `path` with `content` through a sibling temporary file this run
/// creates, so a failed write leaves the previous file whole and no existing
/// file is overwritten on the way.
fn replace_file(path: &Path, content: &str) -> Result<(), XtaskError> {
    let file_name = path
        .file_name()
        .ok_or_else(|| XtaskError::InvalidPath(path.to_path_buf()))?;
    let temporary = path.with_file_name(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(io(&temporary))?;
    let written = std::io::Write::write_all(&mut file, content.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, path))
        .map_err(io(path));
    if written.is_err() {
        // Best effort: the returned error already reports the failed write,
        // and the file removed is the one this run created.
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// File names already present in the fragment directory.
fn existing_names(fragment_dir: &Path) -> Result<Vec<String>, XtaskError> {
    let entries = match fs::read_dir(fragment_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io(fragment_dir)(error)),
    };
    entries
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(io(fragment_dir))
        })
        .collect()
}

/// Writes `bundle_dir/log.md` as the `log.md` of `source_dir` with its
/// fragments as unreleased sections. Without a log or fragments nothing is
/// written.
///
/// # Errors
///
/// Returns [`XtaskError::KnowledgeLog`] for a fragment that breaks the rules,
/// and [`XtaskError::Io`] when a file cannot be read or written.
pub(crate) fn assemble_into(source_dir: &Path, bundle_dir: &Path) -> Result<(), XtaskError> {
    let fragments =
        log::read_fragments(&source_dir.join(FRAGMENT_DIR)).map_err(XtaskError::KnowledgeLog)?;
    let log_path = source_dir.join(LOG_FILE);
    if fragments.is_empty() || !log_path.is_file() {
        return Ok(());
    }
    let current = fs::read_to_string(&log_path).map_err(io(&log_path))?;
    let assembled = log::assemble(&current, &fragments).map_err(XtaskError::KnowledgeLog)?;
    let destination = bundle_dir.join(LOG_FILE);
    fs::write(&destination, assembled).map_err(io(&destination))
}

/// Whether `relative`, a path inside a source bundle, is a log fragment or the
/// fragment directory, which reach a built bundle only through its log.
pub(crate) fn is_fragment_path(relative: &Path) -> bool {
    relative.starts_with(FRAGMENT_DIR)
}

/// The committed log of `source_dir` carries no unreleased section, and its
/// fragments follow the rules. Returns the number of fragments.
///
/// # Errors
///
/// Returns a message naming the first [`LogError`] or the unreadable log.
pub(crate) fn check(source_dir: &Path) -> Result<usize, String> {
    let log_path: PathBuf = source_dir.join(LOG_FILE);
    let current = fs::read_to_string(&log_path)
        .map_err(|error| format!("`{}`: {error}", log_path.display()))?;
    log::check_log(&log_path, &current).map_err(|error: LogError| error.to_string())?;
    log::read_fragments(&source_dir.join(FRAGMENT_DIR))
        .map(|fragments| fragments.len())
        .map_err(|error| error.to_string())
}
