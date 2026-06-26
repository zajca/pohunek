//! Materialize the embedded knowledge bundle into a versioned cache directory.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::assistant::embedded_bundle;

const KNOWLEDGE_DIR: &str = "knowledge";
const COMPLETE_MARKER: &str = ".complete";

/// Extract the embedded knowledge bundle into a versioned cache directory.
///
/// On every successful materialization, stale version directories are pruned
/// (best-effort) so the cache does not grow unbounded as the binary version
/// changes. A GC failure never fails materialization.
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
    remove_path_if_exists(&temp)?;
    fs::create_dir_all(&temp)?;
    embedded_bundle().extract(&temp)?;
    fs::write(temp.join(COMPLETE_MARKER), b"complete\n")?;

    match fs::rename(&temp, &target) {
        Ok(()) => {}
        Err(_) if matches!(target_state(&target)?, TargetState::Complete) => {
            remove_path_if_exists(&temp)?;
            return Ok(target);
        }
        Err(error) => {
            let _ = remove_path_if_exists(&temp);
            return Err(error);
        }
    }

    Ok(target)
}

/// Remove stale materialized knowledge versions under the cache knowledge dir.
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

fn temporary_dir(knowledge_dir: &Path, version_hash: &str) -> PathBuf {
    knowledge_dir.join(format!(
        ".tmp-{version_hash}-{}-{}",
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
