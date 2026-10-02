//! Materialize the embedded knowledge bundle into a versioned cache directory.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::assistant::embedded_bundle;

const KNOWLEDGE_DIR: &str = "knowledge";
const COMPLETE_MARKER: &str = ".complete";

/// Extract the embedded knowledge bundle into a versioned cache directory.
///
/// Concurrent calls for the same version, from threads or processes, are safe:
/// each builds the bundle in its own exclusively created temp dir and renames
/// it into place, so the version directory only ever appears complete.
///
/// On every successful materialization, stale version directories are pruned
/// (best-effort) so the cache does not grow unbounded as the binary version
/// changes. A GC failure never fails materialization.
///
/// # Errors
///
/// Returns an [`io::Error`] if `version_hash` is not a single path segment, or
/// if extracting the embedded bundle into the cache directory fails.
pub fn materialize(cache_dir: impl AsRef<Path>, version_hash: &str) -> io::Result<PathBuf> {
    validate_version_hash(version_hash)?;

    let knowledge_dir = cache_dir.as_ref().join(KNOWLEDGE_DIR);
    let target = extract_into_knowledge_dir(&knowledge_dir, version_hash)?;
    let _ = gc_in_knowledge_dir(&knowledge_dir, version_hash);
    Ok(target)
}

fn extract_into_knowledge_dir(knowledge_dir: &Path, version_hash: &str) -> io::Result<PathBuf> {
    let target = knowledge_dir.join(version_hash);
    if matches!(target_state(&target)?, TargetState::Complete) {
        return Ok(target);
    }

    fs::create_dir_all(knowledge_dir)?;
    let temp = temporary_dir(knowledge_dir, version_hash);
    publish_through(&temp, &target)?;
    Ok(target)
}

/// Build the bundle in `temp` and rename it onto `target`.
///
/// `temp` is created exclusively, so a directory owned by a concurrent
/// materializer is never reused or removed; a name clash fails closed with
/// [`io::ErrorKind::AlreadyExists`]. The temp dir is removed on any later error.
fn publish_through(temp: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir(temp)?;
    let result = fill_and_rename(temp, target);
    if result.is_err() {
        // The original error is the one worth reporting; a leftover temp dir is
        // skipped by GC and harmless to later materializations.
        let _ = remove_path_if_exists(temp);
    }
    result
}

fn fill_and_rename(temp: &Path, target: &Path) -> io::Result<()> {
    embedded_bundle().extract(temp)?;
    fs::write(temp.join(COMPLETE_MARKER), b"complete\n")?;

    match fs::rename(temp, target) {
        Ok(()) => Ok(()),
        // A concurrent materializer published the same version first.
        Err(_) if matches!(target_state(target)?, TargetState::Complete) => {
            remove_path_if_exists(temp)
        }
        Err(error) => Err(error),
    }
}

/// Remove stale materialized knowledge versions under the cache knowledge dir.
///
/// # Errors
///
/// Returns an [`io::Error`] if `keep` is not a single path segment, or if a
/// stale version directory cannot be removed.
pub fn gc(cache_dir: impl AsRef<Path>, keep: &str) -> io::Result<()> {
    validate_version_hash(keep)?;
    gc_in_knowledge_dir(&cache_dir.as_ref().join(KNOWLEDGE_DIR), keep)
}

fn gc_in_knowledge_dir(knowledge_dir: &Path, keep: &str) -> io::Result<()> {
    if !knowledge_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(knowledge_dir)? {
        let entry = entry?;
        if entry.file_name() == OsStr::new(keep) {
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".tmp-"))
        {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        }
    }

    Ok(())
}

fn validate_version_hash(version_hash: &str) -> io::Result<()> {
    let mut components = Path::new(version_hash).components();
    let valid =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "knowledge version hash must be a single path segment",
        ))
    }
}

/// Name a per-attempt temp dir unique within the host and process.
///
/// Threads of one process share the pid and can read the same clock
/// nanosecond, so the process-wide sequence keeps their names apart. The
/// sequence only makes clashes rare; exclusive creation in [`publish_through`]
/// keeps a clash safe, so correctness does not depend on this static.
fn temporary_dir(knowledge_dir: &Path, version_hash: &str) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    knowledge_dir.join(format!(
        ".tmp-{version_hash}-{}-{}-{sequence}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after unix epoch")
            .as_nanos()
    ))
}

enum TargetState {
    Missing,
    IncompleteDir,
    Complete,
}

fn target_state(target: &Path) -> io::Result<TargetState> {
    let metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(TargetState::Missing),
        Err(error) => return Err(error),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "knowledge version directory must not be a symlink",
        ));
    }
    if !file_type.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "knowledge version path must be a directory",
        ));
    }

    let marker = target.join(COMPLETE_MARKER);
    match fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            let marker_type = metadata.file_type();
            if marker_type.is_symlink() {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "knowledge complete marker must not be a symlink",
                ))
            } else if marker_type.is_file() {
                Ok(TargetState::Complete)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "knowledge complete marker must be a file",
                ))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TargetState::IncompleteDir),
        Err(error) => Err(error),
    }
}

fn remove_path_if_exists(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_leaves_temp_dir_owned_by_another_materializer_untouched() {
        let guard = pohunek_test_support::tempdir_with_prefix("knowledge-unit-foreign-temp-")
            .expect("create scratch dir");
        let knowledge_dir = guard.path();
        let target = knowledge_dir.join("sha256:test-foreign-temp");
        let temp = knowledge_dir.join(".tmp-sha256:test-foreign-temp-1-2-3");
        fs::create_dir(&temp).expect("create foreign temp dir");
        fs::write(temp.join("in-flight.md"), "partial").expect("write in-flight file");

        let error =
            publish_through(&temp, &target).expect_err("foreign temp dir must not be reused");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(temp.join("in-flight.md")).expect("in-flight file remains"),
            "partial"
        );
        assert!(!temp.join(COMPLETE_MARKER).exists());
        assert!(matches!(target_state(&target), Ok(TargetState::Missing)));
    }

    #[test]
    fn temporary_dir_names_differ_within_one_process() {
        let knowledge_dir = Path::new("knowledge");

        let first = temporary_dir(knowledge_dir, "sha256:same");
        let second = temporary_dir(knowledge_dir, "sha256:same");

        assert_ne!(first, second);
    }
}
