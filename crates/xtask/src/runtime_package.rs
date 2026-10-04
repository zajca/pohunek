//! Runtime package archive build and reproducibility check.
//!
//! `package build` turns a package directory into the canonical archive;
//! `package verify` builds it twice from independent directory walks, requires
//! byte equality and re-reads the result with the strict reader.

// Rust guideline compliant 2026-10-04

use std::fs;
use std::path::Path;

use package::directory::{build_directory_archive, DirectoryError};
use package::{read_archive, Limits};

use crate::XtaskError;

/// Builds the canonical archive of `dir` and writes it to `output`.
///
/// `output` must not lie inside `dir`, otherwise a previous archive would
/// become an input of the next build. Returns the package digest.
pub(crate) fn build(dir: &Path, output: &Path) -> Result<String, XtaskError> {
    reject_output_inside(dir, output)?;
    let bytes = build_bytes(dir, &Limits::DEFAULT)?;
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
    let first = build_bytes(dir, &Limits::DEFAULT)?;
    let second = build_bytes(dir, &Limits::DEFAULT)?;
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

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> XtaskError {
    let path = path.to_path_buf();
    move |source| XtaskError::Io { path, source }
}

/// Fails when `output` is a symlink or resolves to a place inside `dir`.
///
/// A symlink is rejected outright, dangling or not: the write would follow it
/// to a destination this check cannot vouch for.
fn reject_output_inside(dir: &Path, output: &Path) -> Result<(), XtaskError> {
    let root = fs::canonicalize(dir).map_err(io_error(dir))?;
    let resolved = match fs::symlink_metadata(output) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(XtaskError::OutputIsSymlink(output.to_path_buf()));
        }
        Ok(_) => fs::canonicalize(output).map_err(io_error(output))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = match output.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            let name = output
                .file_name()
                .ok_or_else(|| XtaskError::InvalidPath(output.to_path_buf()))?;
            fs::canonicalize(parent)
                .map_err(io_error(parent))?
                .join(name)
        }
        Err(error) => return Err(io_error(output)(error)),
    };
    if resolved.starts_with(&root) {
        return Err(XtaskError::OutputInsideInput(output.to_path_buf()));
    }
    Ok(())
}

/// Builds the canonical archive bytes of `dir`, mapping directory failures to
/// [`XtaskError`] values that name `dir`.
fn build_bytes(dir: &Path, limits: &Limits) -> Result<Vec<u8>, XtaskError> {
    build_directory_archive(dir, limits).map_err(|error| match error {
        DirectoryError::UnsupportedFileType => XtaskError::UnsupportedFileType(dir.to_path_buf()),
        DirectoryError::InvalidPath => XtaskError::InvalidPath(dir.to_path_buf()),
        DirectoryError::Io { kind } => XtaskError::Io {
            path: dir.to_path_buf(),
            source: kind.into(),
        },
        DirectoryError::Archive(error) => XtaskError::Package(error),
        // `Changed` and any future variant: the tree is not a stable input.
        other => XtaskError::Usage(format!("package directory is not readable: {other}")),
    })
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
    fn directory_failures_map_to_xtask_errors_naming_the_directory() {
        let dir = package_dir();
        symlink("runtime.toml", dir.path().join("link")).expect("symlink");
        assert!(matches!(
            build_bytes(dir.path(), &Limits::DEFAULT),
            Err(XtaskError::UnsupportedFileType(path)) if path == dir.path()
        ));
        fs::remove_file(dir.path().join("link")).expect("remove");

        fs::write(dir.path().join("with space"), b"x").expect("write");
        assert!(matches!(
            build_bytes(dir.path(), &Limits::DEFAULT),
            Err(XtaskError::Package(_))
        ));

        let missing = dir.path().join("absent");
        assert!(matches!(
            build_bytes(&missing, &Limits::DEFAULT),
            Err(XtaskError::Io { path, .. }) if path == missing
        ));
    }

    #[test]
    fn output_inside_the_input_tree_is_rejected() {
        let dir = package_dir();
        let inside = dir.path().join("out.tar.zst");
        assert!(matches!(
            build(dir.path(), &inside),
            Err(XtaskError::OutputInsideInput(_))
        ));
        assert!(!inside.exists());
        let nested = dir.path().join("detect/../out.tar.zst");
        assert!(matches!(
            build(dir.path(), &nested),
            Err(XtaskError::OutputInsideInput(_))
        ));
    }

    #[test]
    fn output_outside_the_tree_is_accepted_and_rebuilds_identically() {
        let dir = package_dir();
        let out = pohunek_test_support::tempdir().expect("tempdir");
        let path = out.path().join("a.tar.zst");
        let first = build(dir.path(), &path).expect("build");
        assert_eq!(build(dir.path(), &path).expect("rebuild"), first);
    }

    #[test]
    fn output_symlinks_are_rejected_even_when_dangling_into_the_tree() {
        let dir = package_dir();
        let out = pohunek_test_support::tempdir().expect("tempdir");

        let dangling = out.path().join("dangling.tar.zst");
        let target = dir.path().join("future.tar.zst");
        symlink(&target, &dangling).expect("symlink");
        assert!(matches!(
            build(dir.path(), &dangling),
            Err(XtaskError::OutputIsSymlink(_))
        ));
        assert!(!target.exists(), "nothing was written into the tree");

        let existing = out.path().join("existing.tar.zst");
        symlink(dir.path().join("runtime.toml"), &existing).expect("symlink");
        assert!(matches!(
            build(dir.path(), &existing),
            Err(XtaskError::OutputIsSymlink(_))
        ));
        assert_eq!(
            fs::read(dir.path().join("runtime.toml")).expect("read"),
            b"schema = 1\n"
        );
    }
}
