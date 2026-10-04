//! Bounded reads of the files a package method names.

// Rust guideline compliant 2026-10-04

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use package::directory::{build_directory_archive, DirectoryError};
use package::Limits;
use protocol::PackageErrorKind;
use tracing::warn;

/// Why a source file could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadFailure {
    /// The path is not an absolute path to a readable regular file.
    Unreadable,
    /// The file is larger than the bound.
    TooLarge,
}

/// Reads the regular file at the absolute path `path`, at most `max_bytes`.
///
/// The file is opened without following a final symbolic link and without
/// blocking, and the opened handle is checked to be a regular file, so a FIFO
/// or device can neither hang the handler nor feed it unbounded data.
pub(super) fn read_regular_file(path: &str, max_bytes: u64) -> Result<Vec<u8>, ReadFailure> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(ReadFailure::Unreadable);
    }
    let file = open_regular(path).map_err(|_error| ReadFailure::Unreadable)?;
    let length = file
        .metadata()
        .map_err(|_error| ReadFailure::Unreadable)?
        .len();
    if length > max_bytes {
        return Err(ReadFailure::TooLarge);
    }
    let mut bytes = Vec::new();
    // One byte past the bound detects a file that grew after the length check.
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_error| ReadFailure::Unreadable)?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > max_bytes) {
        return Err(ReadFailure::TooLarge);
    }
    Ok(bytes)
}

fn open_regular(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(std::io::Error::from(std::io::ErrorKind::InvalidInput))
    }
}

/// Reads an archive file.
pub(super) fn read_archive_file(path: &str, limits: &Limits) -> Result<Vec<u8>, PackageErrorKind> {
    read_regular_file(path, limits.max_compressed_bytes).map_err(|failure| match failure {
        ReadFailure::Unreadable => PackageErrorKind::SourceUnreadable,
        ReadFailure::TooLarge => PackageErrorKind::ArchiveInvalid,
    })
}

/// Builds the canonical archive of the developer directory at the absolute
/// path `directory`.
pub(super) fn build_directory(
    directory: &str,
    limits: &Limits,
) -> Result<Vec<u8>, PackageErrorKind> {
    let path = Path::new(directory);
    if !path.is_absolute() {
        return Err(PackageErrorKind::SourceUnreadable);
    }
    build_directory_archive(path, limits).map_err(|error| {
        warn!(%error, "a package directory cannot be archived");
        match error {
            DirectoryError::Io { .. } | DirectoryError::Changed => {
                PackageErrorKind::SourceUnreadable
            }
            _ => PackageErrorKind::ArchiveInvalid,
        }
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn a_regular_file_is_read_up_to_the_bound() {
        let dir = pohunek_test_support::tempdir().expect("private test directory");
        let file = dir.path().join("a.bin");
        std::fs::File::create(&file)
            .and_then(|mut file| file.write_all(b"12345"))
            .expect("write");
        let path = file.to_str().expect("utf-8 path");
        assert_eq!(read_regular_file(path, 5).expect("fits"), b"12345");
        assert_eq!(read_regular_file(path, 4), Err(ReadFailure::TooLarge));
    }

    #[test]
    fn relative_missing_linked_and_special_paths_are_unreadable() {
        let dir = pohunek_test_support::tempdir().expect("private test directory");
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"x").expect("write");
        let link = dir.path().join("link");
        symlink(&file, &link).expect("symlink");
        for path in [
            "relative.bin".to_owned(),
            dir.path().join("missing").display().to_string(),
            link.display().to_string(),
            dir.path().display().to_string(),
        ] {
            assert_eq!(
                read_regular_file(&path, 16),
                Err(ReadFailure::Unreadable),
                "{path}"
            );
        }
    }

    #[test]
    fn a_relative_package_directory_is_unreadable() {
        assert_eq!(
            build_directory("relative", &Limits::DEFAULT),
            Err(PackageErrorKind::SourceUnreadable)
        );
    }
}
