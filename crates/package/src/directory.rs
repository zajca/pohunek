//! Canonical archive of a package directory.
//!
//! [`build_directory_archive`] walks a developer directory and returns the same
//! deterministic bytes [`crate::build_archive`] produces for that file set. The
//! walk accepts regular files and directories only, charges every file against
//! the archive limits from the bytes actually read, and never follows a symlink
//! that replaces a file between the directory listing and the read.

// Rust guideline compliant 2026-10-04

use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use thiserror::Error;

use crate::{build_archive, ArchiveEntry, ArchiveError, EntryRejection, Limits, MAX_PATH_BYTES};

/// Any execute bit marks the file executable in the canonical archive.
const EXECUTE_BITS: u32 = 0o111;

/// Bytes of a tar block; every file costs one header block plus data padded to
/// a block boundary in the canonical stream.
const TAR_BLOCK_BYTES: u64 = 512;

/// Why a directory could not be turned into an archive.
///
/// Like [`ArchiveError`], no variant carries a path or file content.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum DirectoryError {
    /// The tree holds a symbolic link, device, fifo or socket.
    #[error("the directory holds an unsupported file type")]
    UnsupportedFileType,
    /// A path inside the tree is not valid UTF-8.
    #[error("the directory holds a path that is not valid UTF-8")]
    InvalidPath,
    /// A file stopped being the regular file the walk listed, which means the
    /// tree was modified while it was read.
    #[error("the directory changed while it was read")]
    Changed,
    /// The filesystem failed.
    #[error("filesystem failure while reading the directory: {kind}")]
    Io {
        /// Kind of the operating-system failure.
        kind: io::ErrorKind,
    },
    /// The collected files do not form a valid archive within the limits.
    #[error("directory archive rejected: {0}")]
    Archive(#[source] ArchiveError),
}

impl From<io::Error> for DirectoryError {
    fn from(error: io::Error) -> Self {
        Self::Io { kind: error.kind() }
    }
}

/// Builds the canonical archive of the regular files below `dir`.
///
/// The result depends only on the relative paths, contents and executable bits
/// of the files, never on listing order, timestamps or owners. Limits are
/// enforced during the walk, so an oversized tree is rejected without being
/// buffered.
///
/// # Errors
///
/// Returns [`DirectoryError::UnsupportedFileType`] for a symlink or special
/// file, [`DirectoryError::InvalidPath`] for a non-UTF-8 name,
/// [`DirectoryError::Changed`] when a file is swapped during the walk,
/// [`DirectoryError::Io`] when the filesystem fails and
/// [`DirectoryError::Archive`] when the files violate the archive rules or
/// `limits`.
pub fn build_directory_archive(dir: &Path, limits: &Limits) -> Result<Vec<u8>, DirectoryError> {
    let mut entries = Vec::new();
    let mut budget = Budget {
        limits,
        files: 0,
        expanded: 0,
    };
    collect(dir, dir, &mut budget, &mut entries)?;
    build_archive(&entries, limits).map_err(DirectoryError::Archive)
}

/// Running totals of one directory walk.
///
/// Sizes are charged from the bytes actually read, never from file metadata,
/// and every read is limited by what remains of the aggregate budget, so a file
/// that grows or is replaced during the walk cannot make the builder buffer
/// more than the package limits allow.
struct Budget<'a> {
    limits: &'a Limits,
    files: usize,
    expanded: u64,
}

impl Budget<'_> {
    /// Accounts for one more file at a path of `path_len` bytes.
    fn admit(&mut self, path_len: usize) -> Result<(), DirectoryError> {
        if self.files >= self.limits.max_files {
            return Err(DirectoryError::Archive(ArchiveError::TooManyFiles {
                limit: self.limits.max_files,
            }));
        }
        if path_len > self.limits.max_path_bytes.min(MAX_PATH_BYTES) {
            return Err(DirectoryError::Archive(ArchiveError::Entry {
                index: self.files,
                reason: EntryRejection::PathTooLong,
            }));
        }
        self.files += 1;
        Ok(())
    }

    /// Reads one admitted file from `source`, charging its canonical size.
    ///
    /// At most `min(per-file limit, remaining aggregate)` plus one byte is
    /// buffered. The two-block end marker is not counted here; the archive
    /// builder applies the exact aggregate limit.
    fn read_charged(&mut self, source: impl Read) -> Result<Vec<u8>, DirectoryError> {
        let index = self.files.saturating_sub(1);
        let remaining = self
            .limits
            .max_expanded_bytes
            .saturating_sub(self.expanded)
            .saturating_sub(TAR_BLOCK_BYTES);
        let limit = self.limits.max_file_bytes.min(remaining);
        let mut contents = Vec::new();
        source
            .take(limit.saturating_add(1))
            .read_to_end(&mut contents)?;
        let len = u64::try_from(contents.len()).unwrap_or(u64::MAX);
        if len > limit {
            return Err(DirectoryError::Archive(
                if limit < self.limits.max_file_bytes {
                    ArchiveError::ExpandedTooLarge {
                        limit: self.limits.max_expanded_bytes,
                    }
                } else {
                    ArchiveError::Entry {
                        index,
                        reason: EntryRejection::FileTooLarge,
                    }
                },
            ));
        }
        self.expanded = self.expanded.saturating_add(
            len.div_ceil(TAR_BLOCK_BYTES)
                .saturating_mul(TAR_BLOCK_BYTES)
                .saturating_add(TAR_BLOCK_BYTES),
        );
        Ok(contents)
    }
}

/// Collects regular files below `dir`; symlinks and special files are errors.
fn collect(
    root: &Path,
    dir: &Path,
    budget: &mut Budget<'_>,
    entries: &mut Vec<ArchiveEntry>,
) -> Result<(), DirectoryError> {
    for item in fs::read_dir(dir)? {
        let path = item?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            collect(root, &path, budget, entries)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_cause| DirectoryError::InvalidPath)?;
            let name = relative
                .components()
                .map(|component| component.as_os_str().to_str())
                .collect::<Option<Vec<&str>>>()
                .ok_or(DirectoryError::InvalidPath)?
                .join("/");
            budget.admit(name.len())?;
            let (contents, executable) = read_listed_file(&path, &metadata, budget)?;
            entries.push(ArchiveEntry {
                path: name,
                contents,
                executable,
            });
        } else {
            return Err(DirectoryError::UnsupportedFileType);
        }
    }
    Ok(())
}

/// Opens `path` and reads it, provided it is still the file `listed` described.
///
/// The open follows symlinks, so identity is checked on the opened descriptor
/// before a byte is read; a file replaced by a symlink after the listing is
/// reported as [`DirectoryError::Changed`] instead of being followed.
fn read_listed_file(
    path: &Path,
    listed: &fs::Metadata,
    budget: &mut Budget<'_>,
) -> Result<(Vec<u8>, bool), DirectoryError> {
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    if !is_same_regular_file(listed, &opened) {
        return Err(DirectoryError::Changed);
    }
    let contents = budget.read_charged(file)?;
    Ok((contents, opened.permissions().mode() & EXECUTE_BITS != 0))
}

/// Whether `opened` is a regular file with the device and inode of `listed`.
fn is_same_regular_file(listed: &fs::Metadata, opened: &fs::Metadata) -> bool {
    opened.is_file() && listed.dev() == opened.dev() && listed.ino() == opened.ino()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::read_archive;

    fn package_dir() -> tempfile::TempDir {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("detect")).expect("mkdir");
        fs::write(dir.path().join("runtime.toml"), b"schema = 1\n").expect("write");
        fs::write(dir.path().join("detect/default.toml"), b"[detect]\n").expect("write");
        dir
    }

    fn budget(limits: &Limits) -> Budget<'_> {
        Budget {
            limits,
            files: 1,
            expanded: 0,
        }
    }

    #[test]
    fn the_archive_equals_the_one_built_from_the_same_files() {
        let dir = package_dir();
        let bytes = build_directory_archive(dir.path(), &Limits::DEFAULT).expect("build");
        let expected = build_archive(
            &[
                ArchiveEntry {
                    path: "detect/default.toml".to_owned(),
                    contents: b"[detect]\n".to_vec(),
                    executable: false,
                },
                ArchiveEntry {
                    path: "runtime.toml".to_owned(),
                    contents: b"schema = 1\n".to_vec(),
                    executable: false,
                },
            ],
            &Limits::DEFAULT,
        )
        .expect("expected archive");
        assert_eq!(bytes, expected);
    }

    #[test]
    fn executable_bit_is_carried_into_the_archive() {
        let dir = package_dir();
        let script = dir.path().join("hook.sh");
        fs::write(&script, b"#!/bin/sh\n").expect("write");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("chmod");
        let bytes = build_directory_archive(dir.path(), &Limits::DEFAULT).expect("build");
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
        assert_eq!(
            build_directory_archive(dir.path(), &Limits::DEFAULT),
            Err(DirectoryError::UnsupportedFileType)
        );
    }

    #[test]
    fn non_utf8_names_are_rejected() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;
        let dir = package_dir();
        fs::write(dir.path().join(OsStr::from_bytes(b"bad\xff")), b"x").expect("write");
        assert_eq!(
            build_directory_archive(dir.path(), &Limits::DEFAULT),
            Err(DirectoryError::InvalidPath)
        );
    }

    #[test]
    fn a_missing_directory_is_an_io_error() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        assert_eq!(
            build_directory_archive(&dir.path().join("absent"), &Limits::DEFAULT),
            Err(DirectoryError::Io {
                kind: io::ErrorKind::NotFound
            })
        );
    }

    #[test]
    fn invalid_package_paths_are_reported_as_archive_errors() {
        let dir = package_dir();
        fs::write(dir.path().join("with space"), b"x").expect("write");
        assert!(matches!(
            build_directory_archive(dir.path(), &Limits::DEFAULT),
            Err(DirectoryError::Archive(_))
        ));
    }

    #[test]
    fn oversized_file_is_rejected_without_buffering_more_than_the_limit() {
        let dir = package_dir();
        let big = fs::File::create(dir.path().join("big")).expect("create");
        big.set_len(Limits::DEFAULT.max_file_bytes + 1)
            .expect("sparse");
        assert!(matches!(
            build_directory_archive(dir.path(), &Limits::DEFAULT),
            Err(DirectoryError::Archive(ArchiveError::Entry {
                reason: EntryRejection::FileTooLarge,
                ..
            }))
        ));
    }

    #[test]
    fn read_is_charged_from_bytes_read_not_metadata() {
        // The source yields far more than any metadata would have promised.
        let limits = Limits {
            max_file_bytes: 1024,
            ..Limits::DEFAULT
        };
        let mut budget = budget(&limits);
        assert_eq!(
            budget
                .read_charged(io::repeat(1).take(1024))
                .expect("at the limit")
                .len(),
            1024
        );
        assert_eq!(budget.expanded, 512 + 1024);
        assert!(matches!(
            budget.read_charged(io::repeat(1)),
            Err(DirectoryError::Archive(ArchiveError::Entry {
                reason: EntryRejection::FileTooLarge,
                ..
            }))
        ));
    }

    #[test]
    fn reads_are_limited_by_the_remaining_aggregate_budget() {
        // Each read is valid for the per-file limit; the aggregate is not.
        let limits = Limits {
            max_file_bytes: 4096,
            max_expanded_bytes: 2 * (512 + 4096) + 512 + 100,
            ..Limits::DEFAULT
        };
        let mut budget = budget(&limits);
        budget
            .read_charged(io::repeat(1).take(4096))
            .expect("first");
        budget
            .read_charged(io::repeat(1).take(4096))
            .expect("second");
        // 100 bytes remain after a third header: a 4096-byte file must fail
        // after buffering only 101 bytes, even from an endless source.
        assert!(matches!(
            budget.read_charged(io::repeat(1)),
            Err(DirectoryError::Archive(
                ArchiveError::ExpandedTooLarge { .. }
            ))
        ));
    }

    #[test]
    fn aggregate_size_is_limited_before_files_are_read() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        for name in ["a", "b", "c"] {
            let file = fs::File::create(dir.path().join(name)).expect("create");
            file.set_len(4096).expect("sparse");
        }
        // Three files cost 3 x (512 + 4096) bytes of tar stream.
        let limits = Limits {
            max_expanded_bytes: 3 * (512 + 4096) - 1,
            ..Limits::DEFAULT
        };
        assert!(matches!(
            build_directory_archive(dir.path(), &limits),
            Err(DirectoryError::Archive(
                ArchiveError::ExpandedTooLarge { .. }
            ))
        ));
    }

    #[test]
    fn file_count_and_path_length_are_limited_during_traversal() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        for name in ["a", "b", "c"] {
            fs::write(dir.path().join(name), b"x").expect("write");
        }
        let few = Limits {
            max_files: 2,
            ..Limits::DEFAULT
        };
        assert_eq!(
            build_directory_archive(dir.path(), &few),
            Err(DirectoryError::Archive(ArchiveError::TooManyFiles {
                limit: 2
            }))
        );
        let short = Limits {
            max_path_bytes: 0,
            ..Limits::DEFAULT
        };
        assert!(matches!(
            build_directory_archive(dir.path(), &short),
            Err(DirectoryError::Archive(ArchiveError::Entry {
                reason: EntryRejection::PathTooLong,
                ..
            }))
        ));
    }

    #[test]
    fn a_file_swapped_for_a_symlink_after_the_listing_is_reported_as_changed() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let victim = dir.path().join("victim");
        let secret = dir.path().join("secret");
        fs::write(&victim, b"listed\n").expect("write");
        fs::write(&secret, b"followed\n").expect("write");
        let listed = fs::symlink_metadata(&victim).expect("metadata");
        fs::remove_file(&victim).expect("remove");
        symlink(&secret, &victim).expect("symlink");

        let limits = Limits::DEFAULT;
        let mut budget = Budget {
            limits: &limits,
            files: 1,
            expanded: 0,
        };
        assert_eq!(
            read_listed_file(&victim, &listed, &mut budget),
            Err(DirectoryError::Changed)
        );
        assert_eq!(budget.expanded, 0, "nothing was read from the target");
    }

    #[test]
    fn an_unchanged_file_is_read_with_its_executable_bit() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let path = dir.path().join("hook");
        fs::write(&path, b"#!/bin/sh\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod");
        let listed = fs::symlink_metadata(&path).expect("metadata");
        let limits = Limits::DEFAULT;
        let mut budget = budget(&limits);
        assert_eq!(
            read_listed_file(&path, &listed, &mut budget),
            Ok((b"#!/bin/sh\n".to_vec(), true))
        );
    }

    #[test]
    fn identity_requires_the_same_regular_file() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::write(&first, b"a").expect("write");
        fs::write(&second, b"a").expect("write");
        let first_meta = fs::metadata(&first).expect("metadata");
        let second_meta = fs::metadata(&second).expect("metadata");
        assert!(is_same_regular_file(&first_meta, &first_meta));
        assert!(!is_same_regular_file(&first_meta, &second_meta));
        let dir_meta = fs::metadata(dir.path()).expect("metadata");
        assert!(!is_same_regular_file(&dir_meta, &dir_meta));
    }
}
