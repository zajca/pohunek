//! Canonical archive of a package directory.
//!
//! [`build_directory_archive`] walks a developer directory and returns the same
//! deterministic bytes [`crate::build_archive`] produces for that file set. The
//! walk accepts regular files and directories only, charges every file against
//! the archive limits from the bytes actually read. The walk holds a descriptor
//! per directory and opens every entry relative to it without following
//! symlinks and without blocking, then checks type and identity on the opened
//! descriptor, so an entry swapped for a symlink or a fifo after it was listed
//! is reported as changed and never followed or waited on.

// Rust guideline compliant 2026-10-04

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{fstat, openat, statat, AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use thiserror::Error;

use crate::{build_archive, ArchiveEntry, ArchiveError, EntryRejection, Limits, MAX_PATH_BYTES};

/// Any execute bit marks the file executable in the canonical archive.
const EXECUTE_BITS: rustix::fs::RawMode = 0o111;

/// Flags that open a directory without following a symlink at its name.
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Flags that open a regular file without following a symlink at its name and
/// without blocking on a fifo or device swapped in after the listing.
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

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
        directories: 0,
        expanded: 0,
    };
    let root = rustix::fs::open(dir, DIRECTORY_FLAGS, Mode::empty()).map_err(open_error)?;
    collect(&root, "", &mut budget, &mut entries)?;
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
    /// Directory entries seen across the whole tree. Directories hold no file
    /// by themselves, so without this count a tree of empty directories would
    /// cost unbounded filesystem work while passing every other limit.
    directories: usize,
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

/// What the listing saw at one name: its type and identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Listed {
    kind: FileType,
    device: rustix::fs::Dev,
    inode: u64,
}

/// Maps an open failure to the typed error; a name that stopped being the
/// listed type (a symlink where `O_NOFOLLOW` forbids one, a non-directory
/// where one was listed) means the tree changed while it was read.
fn open_error(errno: Errno) -> DirectoryError {
    if matches!(errno, Errno::LOOP | Errno::NOTDIR | Errno::NXIO) {
        DirectoryError::Changed
    } else {
        DirectoryError::Io {
            kind: io::Error::from(errno).kind(),
        }
    }
}

fn identity(stat: &Stat) -> Listed {
    Listed {
        kind: FileType::from_raw_mode(stat.st_mode),
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}

/// The type of the entry `name` below `dir`.
///
/// The type a directory listing reports is used when it is known; some
/// filesystems report `DT_UNKNOWN`, and then the entry is inspected through the
/// directory descriptor without following it, so a subdirectory is never
/// charged against the file budget.
fn entry_kind(
    dir: &OwnedFd,
    name: &std::ffi::CStr,
    reported: FileType,
) -> Result<FileType, DirectoryError> {
    if reported != FileType::Unknown {
        return Ok(reported);
    }
    statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| FileType::from_raw_mode(stat.st_mode))
        .map_err(open_error)
}

/// Inspects `name` below `dir` without following it.
fn list_entry(dir: &OwnedFd, name: &str) -> Result<Listed, DirectoryError> {
    statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| identity(&stat))
        .map_err(open_error)
}

/// Opens the directory `name` listed below `dir`, provided it is still that
/// directory.
fn open_listed_dir(dir: &OwnedFd, name: &str, listed: Listed) -> Result<OwnedFd, DirectoryError> {
    let child = openat(dir, name, DIRECTORY_FLAGS, Mode::empty()).map_err(open_error)?;
    let opened = identity(&fstat(&child).map_err(open_error)?);
    if opened == listed {
        Ok(child)
    } else {
        Err(DirectoryError::Changed)
    }
}

/// Opens the regular file `name` listed below `dir` and reads it, provided it
/// is still that file.
///
/// The open neither follows a symlink nor blocks, and type and identity are
/// checked on the opened descriptor before a byte is read.
fn read_listed_file(
    dir: &OwnedFd,
    name: &str,
    listed: Listed,
    budget: &mut Budget<'_>,
) -> Result<(Vec<u8>, bool), DirectoryError> {
    let fd = openat(dir, name, FILE_FLAGS, Mode::empty()).map_err(open_error)?;
    let stat = fstat(&fd).map_err(open_error)?;
    if identity(&stat) != listed {
        return Err(DirectoryError::Changed);
    }
    let contents = budget.read_charged(File::from(fd))?;
    let executable = stat.st_mode & EXECUTE_BITS != 0;
    Ok((contents, executable))
}

/// Collects regular files below the directory `dir`, whose path relative to
/// the package root is `prefix`; symlinks and special files are errors.
fn collect(
    dir: &OwnedFd,
    prefix: &str,
    budget: &mut Budget<'_>,
    entries: &mut Vec<ArchiveEntry>,
) -> Result<(), DirectoryError> {
    let listing = Dir::read_from(dir).map_err(open_error)?;
    let mut names = Vec::new();
    // Names are buffered only up to what the archive could still admit, so a
    // directory with millions of entries is rejected as soon as the budget is
    // exceeded instead of being held in memory first. Files are charged
    // against the remaining file budget; subdirectories hold no file by
    // themselves and are bounded by the whole-archive file limit.
    let file_room = budget.limits.max_files.saturating_sub(budget.files);
    let mut files_seen = 0_usize;
    for item in listing {
        let item = item.map_err(open_error)?;
        let bytes = item.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let kind = entry_kind(dir, item.file_name(), item.file_type())?;
        let too_many = if kind == FileType::Directory {
            budget.directories += 1;
            budget.directories > budget.limits.max_files
        } else {
            files_seen += 1;
            files_seen > file_room
        };
        if too_many {
            return Err(DirectoryError::Archive(ArchiveError::TooManyFiles {
                limit: budget.limits.max_files,
            }));
        }
        let name = std::str::from_utf8(bytes).map_err(|_cause| DirectoryError::InvalidPath)?;
        names.push(name.to_owned());
    }
    for name in names {
        let listed = list_entry(dir, &name)?;
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        match listed.kind {
            FileType::Directory => {
                // A directory deeper than the path limit cannot hold a file,
                // which also bounds the descriptors the walk keeps open.
                if relative.len() > budget.limits.max_path_bytes.min(MAX_PATH_BYTES) {
                    return Err(DirectoryError::Archive(ArchiveError::Entry {
                        index: budget.files,
                        reason: EntryRejection::PathTooLong,
                    }));
                }
                let child = open_listed_dir(dir, &name, listed)?;
                collect(&child, &relative, budget, entries)?;
            }
            FileType::RegularFile => {
                budget.admit(relative.len())?;
                let (contents, executable) = read_listed_file(dir, &name, listed, budget)?;
                entries.push(ArchiveEntry {
                    path: relative,
                    contents,
                    executable,
                });
            }
            _ => return Err(DirectoryError::UnsupportedFileType),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    fn budget(limits: &Limits) -> Budget<'_> {
        Budget {
            limits,
            files: 1,
            directories: 0,
            expanded: 0,
        }
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

    fn open_root(path: &Path) -> OwnedFd {
        rustix::fs::open(path, DIRECTORY_FLAGS, Mode::empty()).expect("open the root")
    }

    #[test]
    fn a_file_swapped_for_a_symlink_after_the_listing_is_reported_as_changed() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let secret = dir.path().join("secret");
        fs::write(dir.path().join("victim"), b"listed\n").expect("write");
        fs::write(&secret, b"followed\n").expect("write");
        let root = open_root(dir.path());
        let listed = list_entry(&root, "victim").expect("list");
        fs::remove_file(dir.path().join("victim")).expect("remove");
        symlink(&secret, dir.path().join("victim")).expect("symlink");

        let limits = Limits::DEFAULT;
        let mut budget = budget(&limits);
        assert_eq!(
            read_listed_file(&root, "victim", listed, &mut budget),
            Err(DirectoryError::Changed)
        );
        assert_eq!(budget.expanded, 0, "nothing was read from the target");
    }

    #[test]
    fn a_file_swapped_for_a_fifo_after_the_listing_is_reported_without_blocking() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        fs::write(dir.path().join("victim"), b"listed\n").expect("write");
        let root = open_root(dir.path());
        let listed = list_entry(&root, "victim").expect("list");
        fs::remove_file(dir.path().join("victim")).expect("remove");
        nix::unistd::mkfifo(
            &dir.path().join("victim"),
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("mkfifo");

        let limits = Limits::DEFAULT;
        let mut budget = budget(&limits);
        // The open must return at once: a blocking open would hang this test.
        assert_eq!(
            read_listed_file(&root, "victim", listed, &mut budget),
            Err(DirectoryError::Changed)
        );
    }

    #[test]
    fn a_file_swapped_for_a_symlink_to_a_fifo_is_reported_without_blocking() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        fs::write(dir.path().join("victim"), b"listed\n").expect("write");
        let root = open_root(dir.path());
        let listed = list_entry(&root, "victim").expect("list");
        fs::remove_file(dir.path().join("victim")).expect("remove");
        nix::unistd::mkfifo(
            &dir.path().join("pipe"),
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("mkfifo");
        symlink(dir.path().join("pipe"), dir.path().join("victim")).expect("symlink");

        let limits = Limits::DEFAULT;
        let mut budget = budget(&limits);
        assert_eq!(
            read_listed_file(&root, "victim", listed, &mut budget),
            Err(DirectoryError::Changed)
        );
    }

    #[test]
    fn a_directory_swapped_for_a_symlink_after_the_listing_is_not_followed() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let outside = pohunek_test_support::tempdir().expect("tempdir");
        fs::write(outside.path().join("secret"), b"external\n").expect("write");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::write(dir.path().join("sub/inner"), b"listed\n").expect("write");
        let root = open_root(dir.path());
        let listed = list_entry(&root, "sub").expect("list");
        fs::remove_dir_all(dir.path().join("sub")).expect("remove");
        symlink(outside.path(), dir.path().join("sub")).expect("symlink");

        assert_eq!(
            open_listed_dir(&root, "sub", listed).err(),
            Some(DirectoryError::Changed)
        );
    }

    #[test]
    fn a_directory_replaced_by_another_directory_after_the_listing_is_changed() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        let root = open_root(dir.path());
        let listed = list_entry(&root, "sub").expect("list");
        // The listed directory stays alive under another name, so the
        // replacement cannot be handed its inode.
        fs::rename(dir.path().join("sub"), dir.path().join("kept")).expect("rename");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        assert_eq!(
            open_listed_dir(&root, "sub", listed).err(),
            Some(DirectoryError::Changed)
        );
    }

    #[test]
    fn an_unknown_reported_type_is_resolved_through_the_descriptor() {
        use std::ffi::CString;
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::write(dir.path().join("file"), b"x").expect("write");
        symlink("file", dir.path().join("link")).expect("symlink");
        let root = open_root(dir.path());
        let kind =
            |name: &str, reported| entry_kind(&root, &CString::new(name).expect("name"), reported);
        assert_eq!(kind("sub", FileType::Unknown), Ok(FileType::Directory));
        assert_eq!(kind("file", FileType::Unknown), Ok(FileType::RegularFile));
        assert_eq!(kind("link", FileType::Unknown), Ok(FileType::Symlink));
        // A reported type is trusted without touching the filesystem.
        assert_eq!(kind("absent", FileType::Directory), Ok(FileType::Directory));
        assert_eq!(
            kind("absent", FileType::Unknown),
            Err(DirectoryError::Io {
                kind: io::ErrorKind::NotFound
            })
        );
    }
}
