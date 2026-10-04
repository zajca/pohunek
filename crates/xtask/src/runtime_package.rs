//! Runtime package archive build and reproducibility check.
//!
//! `package build` turns a package directory into the canonical archive;
//! `package verify` builds it twice from independent directory walks, requires
//! byte equality and re-reads the result with the strict reader.

// Rust guideline compliant 2026-10-04

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use package::{
    build_archive, read_archive, ArchiveEntry, ArchiveError, EntryRejection, Limits, MAX_PATH_BYTES,
};

use crate::XtaskError;

/// Any execute bit marks the file executable in the canonical archive.
const EXECUTE_BITS: u32 = 0o111;

/// Bytes of a tar block; every file costs one header block plus data padded to
/// a block boundary in the canonical stream.
const TAR_BLOCK_BYTES: u64 = 512;

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
    fn admit(&mut self, path_len: usize) -> Result<(), XtaskError> {
        if self.files >= self.limits.max_files {
            return Err(XtaskError::Package(ArchiveError::TooManyFiles {
                limit: self.limits.max_files,
            }));
        }
        if path_len > self.limits.max_path_bytes.min(MAX_PATH_BYTES) {
            return Err(XtaskError::Package(ArchiveError::Entry {
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
    fn read_charged(&mut self, source: impl Read) -> Result<Vec<u8>, XtaskError> {
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
            .read_to_end(&mut contents)
            .map_err(|source| XtaskError::Io {
                path: PathBuf::new(),
                source,
            })?;
        let len = u64::try_from(contents.len()).unwrap_or(u64::MAX);
        if len > limit {
            return Err(XtaskError::Package(if limit < self.limits.max_file_bytes {
                ArchiveError::ExpandedTooLarge {
                    limit: self.limits.max_expanded_bytes,
                }
            } else {
                ArchiveError::Entry {
                    index,
                    reason: EntryRejection::FileTooLarge,
                }
            }));
        }
        self.expanded = self.expanded.saturating_add(
            len.div_ceil(TAR_BLOCK_BYTES)
                .saturating_mul(TAR_BLOCK_BYTES)
                .saturating_add(TAR_BLOCK_BYTES),
        );
        Ok(contents)
    }
}

fn build_bytes(dir: &Path, limits: &Limits) -> Result<Vec<u8>, XtaskError> {
    let mut entries = Vec::new();
    let mut budget = Budget {
        limits,
        files: 0,
        expanded: 0,
    };
    collect(dir, dir, &mut budget, &mut entries)?;
    build_archive(&entries, limits).map_err(XtaskError::Package)
}

/// Collects regular files below `dir`; symlinks and special files are errors.
fn collect(
    root: &Path,
    dir: &Path,
    budget: &mut Budget<'_>,
    entries: &mut Vec<ArchiveEntry>,
) -> Result<(), XtaskError> {
    for item in fs::read_dir(dir).map_err(io_error(dir))? {
        let path = item.map_err(io_error(dir))?.path();
        let metadata = fs::symlink_metadata(&path).map_err(io_error(&path))?;
        if metadata.is_dir() {
            collect(root, &path, budget, entries)?;
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
            budget.admit(name.len())?;
            let file = fs::File::open(&path).map_err(io_error(&path))?;
            let contents = budget.read_charged(file).map_err(|error| match error {
                XtaskError::Io { source, .. } => XtaskError::Io {
                    path: path.clone(),
                    source,
                },
                other => other,
            })?;
            entries.push(ArchiveEntry {
                path: name,
                contents,
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
        let bytes = build_bytes(dir.path(), &Limits::DEFAULT).expect("build");
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
            build_bytes(dir.path(), &Limits::DEFAULT),
            Err(XtaskError::UnsupportedFileType(_))
        ));
    }

    #[test]
    fn invalid_package_paths_are_reported_as_archive_errors() {
        let dir = package_dir();
        fs::write(dir.path().join("with space"), b"x").expect("write");
        assert!(matches!(
            build_bytes(dir.path(), &Limits::DEFAULT),
            Err(XtaskError::Package(_))
        ));
    }

    #[test]
    fn oversized_file_is_rejected_from_metadata_without_reading() {
        let dir = package_dir();
        let big = fs::File::create(dir.path().join("big")).expect("create");
        big.set_len(Limits::DEFAULT.max_file_bytes + 1)
            .expect("sparse");
        assert!(matches!(
            build_bytes(dir.path(), &Limits::DEFAULT),
            Err(XtaskError::Package(ArchiveError::Entry {
                reason: EntryRejection::FileTooLarge,
                ..
            }))
        ));
    }

    fn budget(limits: &Limits) -> Budget<'_> {
        Budget {
            limits,
            files: 1,
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
                .read_charged(std::io::repeat(1).take(1024))
                .expect("at the limit")
                .len(),
            1024
        );
        assert_eq!(budget.expanded, 512 + 1024);
        assert!(matches!(
            budget.read_charged(std::io::repeat(1)),
            Err(XtaskError::Package(ArchiveError::Entry {
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
            .read_charged(std::io::repeat(1).take(4096))
            .expect("first");
        budget
            .read_charged(std::io::repeat(1).take(4096))
            .expect("second");
        // 100 bytes remain after a third header: a 4096-byte file must fail
        // after buffering only 101 bytes, even from an endless source.
        assert!(matches!(
            budget.read_charged(std::io::repeat(1)),
            Err(XtaskError::Package(ArchiveError::ExpandedTooLarge { .. }))
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
            build_bytes(dir.path(), &limits),
            Err(XtaskError::Package(ArchiveError::ExpandedTooLarge { .. }))
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
        assert!(matches!(
            build_bytes(dir.path(), &few),
            Err(XtaskError::Package(ArchiveError::TooManyFiles { limit: 2 }))
        ));
        let short = Limits {
            max_path_bytes: 0,
            ..Limits::DEFAULT
        };
        assert!(matches!(
            build_bytes(dir.path(), &short),
            Err(XtaskError::Package(ArchiveError::Entry {
                reason: EntryRejection::PathTooLong,
                ..
            }))
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
