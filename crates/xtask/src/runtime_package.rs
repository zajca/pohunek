//! Runtime package archive build and reproducibility check.
//!
//! `package build` turns a package directory into the canonical archive;
//! `package verify` builds it twice from independent directory walks, requires
//! byte equality and re-reads the result with the strict reader.

// Rust guideline compliant 2026-10-04

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use package::{build_archive, read_archive, ArchiveEntry, Limits};

use crate::XtaskError;

/// Any execute bit marks the file executable in the canonical archive.
const EXECUTE_BITS: u32 = 0o111;

/// Builds the canonical archive of `dir` and writes it to `output`.
///
/// Returns the package digest.
pub(crate) fn build(dir: &Path, output: &Path) -> Result<String, XtaskError> {
    let bytes = build_bytes(dir)?;
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .map_err(XtaskError::Package)?
        .digest()
        .to_string();
    fs::write(output, &bytes).map_err(|source| XtaskError::Io {
        path: output.to_path_buf(),
        source,
    })?;
    Ok(digest)
}

/// Builds the archive of `dir` twice and requires identical bytes.
///
/// Returns the package digest.
pub(crate) fn verify(dir: &Path) -> Result<String, XtaskError> {
    let first = build_bytes(dir)?;
    let second = build_bytes(dir)?;
    if first != second {
        return Err(XtaskError::Usage(
            "package verify failed: two builds of the same directory differ".to_string(),
        ));
    }
    Ok(read_archive(&first, &Limits::DEFAULT)
        .map_err(XtaskError::Package)?
        .digest()
        .to_string())
}

fn build_bytes(dir: &Path) -> Result<Vec<u8>, XtaskError> {
    let mut entries = Vec::new();
    collect(dir, dir, &mut entries)?;
    build_archive(&entries, &Limits::DEFAULT).map_err(XtaskError::Package)
}

/// Collects regular files below `dir`; symlinks and special files are errors.
fn collect(root: &Path, dir: &Path, entries: &mut Vec<ArchiveEntry>) -> Result<(), XtaskError> {
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| XtaskError::Io { path, source }
    };
    for item in fs::read_dir(dir).map_err(io_error(dir))? {
        let path = item.map_err(io_error(dir))?.path();
        let metadata = fs::symlink_metadata(&path).map_err(io_error(&path))?;
        if metadata.is_dir() {
            collect(root, &path, entries)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_cause| XtaskError::InvalidPath(path.clone()))?;
            let name = relative
                .components()
                .map(|component| component.as_os_str().to_str())
                .collect::<Option<Vec<&str>>>()
                .ok_or_else(|| XtaskError::InvalidPath(path.clone()))?
                .join("/");
            entries.push(ArchiveEntry {
                path: name,
                contents: fs::read(&path).map_err(io_error(&path))?,
                executable: metadata.permissions().mode() & EXECUTE_BITS != 0,
            });
        } else {
            return Err(XtaskError::UnsupportedFileType(path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn package_dir() -> tempfile::TempDir {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("detect")).expect("mkdir");
        fs::write(dir.path().join("runtime.toml"), b"schema = 1\n").expect("write");
        fs::write(dir.path().join("detect/default.toml"), b"[detect]\n").expect("write");
        dir
    }

    #[test]
    fn build_writes_a_readable_deterministic_archive() {
        let dir = package_dir();
        let out = pohunek_test_support::tempdir().expect("tempdir");
        let first = out.path().join("first.tar.zst");
        let second = out.path().join("second.tar.zst");
        let digest = build(dir.path(), &first).expect("build");
        assert_eq!(build(dir.path(), &second).expect("build"), digest);
        assert_eq!(
            fs::read(&first).expect("read"),
            fs::read(&second).expect("read")
        );

        let archive = read_archive(&fs::read(&first).expect("read"), &Limits::DEFAULT)
            .expect("archive reads back");
        let paths: Vec<&str> = archive.entries().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["detect/default.toml", "runtime.toml"]);
        assert_eq!(archive.digest().as_str(), digest);
    }

    #[test]
    fn verify_reports_the_digest_of_a_reproducible_build() {
        let dir = package_dir();
        let out = pohunek_test_support::tempdir().expect("tempdir");
        let archive = out.path().join("a.tar.zst");
        assert_eq!(
            verify(dir.path()).expect("verify"),
            build(dir.path(), &archive).expect("build")
        );
    }

    #[test]
    fn executable_bit_is_carried_into_the_archive() {
        let dir = package_dir();
        let script = dir.path().join("hook.sh");
        fs::write(&script, b"#!/bin/sh\n").expect("write");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("chmod");
        let bytes = build_bytes(dir.path()).expect("build");
        let archive = read_archive(&bytes, &Limits::DEFAULT).expect("read");
        let hook = archive
            .entries()
            .iter()
            .find(|entry| entry.path == "hook.sh")
            .expect("hook");
        assert!(hook.executable);
    }

    #[test]
    fn symlinks_are_rejected() {
        let dir = package_dir();
        symlink("runtime.toml", dir.path().join("link")).expect("symlink");
        assert!(matches!(
            build_bytes(dir.path()),
            Err(XtaskError::UnsupportedFileType(_))
        ));
    }

    #[test]
    fn invalid_package_paths_are_reported_as_archive_errors() {
        let dir = package_dir();
        fs::write(dir.path().join("with space"), b"x").expect("write");
        assert!(matches!(
            build_bytes(dir.path()),
            Err(XtaskError::Package(_))
        ));
    }
}
