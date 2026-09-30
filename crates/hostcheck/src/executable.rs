//! Executable resolution shared by every host probe.
//!
//! A file counts as an executable only when it is a regular file (after
//! following symlinks) with at least one execute permission bit. Resolution
//! takes the `PATH` value as an argument so tests never mutate the process
//! environment.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

// Rust guideline compliant 2026-09-30

/// Execute permission bits (owner, group, other) of a Unix mode.
///
/// Any of the three makes the file a candidate; the exact effective-user
/// decision is left to the kernel when the program is launched.
const EXECUTE_BITS: u32 = 0o111;

/// Whether `path` is a regular file with an execute bit set.
///
/// Symlinks are followed, so a link to an executable qualifies and a link to a
/// data file or a directory does not.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & EXECUTE_BITS != 0)
}

/// Resolve `program` to an executable file.
///
/// A `program` containing a path separator is checked as given; a bare name is
/// searched in each directory of `path_var` in order. An unset `PATH` resolves
/// nothing for a bare name.
#[must_use]
pub fn resolve_executable(program: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let candidate = PathBuf::from(program);
        return is_executable_file(&candidate).then_some(candidate);
    }
    let path_var = path_var?;
    std::env::split_paths(path_var)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn write_file(dir: &Path, name: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn non_executable_file_on_path_is_not_resolved() {
        let dir = tempfile_dir("non-exec");
        write_file(&dir, "tool", 0o644);
        let path_var = OsString::from(dir.as_os_str());

        assert_eq!(resolve_executable("tool", Some(&path_var)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn executable_file_on_path_is_resolved() {
        let dir = tempfile_dir("exec");
        let tool = write_file(&dir, "tool", 0o755);
        let path_var = OsString::from(dir.as_os_str());

        assert_eq!(resolve_executable("tool", Some(&path_var)), Some(tool));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn earlier_non_executable_entry_does_not_shadow_a_later_executable() {
        let first = tempfile_dir("shadow-a");
        let second = tempfile_dir("shadow-b");
        write_file(&first, "tool", 0o644);
        let tool = write_file(&second, "tool", 0o755);
        let path_var = std::env::join_paths([&first, &second]).unwrap();

        assert_eq!(resolve_executable("tool", Some(&path_var)), Some(tool));
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    #[test]
    fn program_with_separator_is_checked_as_given_and_ignores_path() {
        let dir = tempfile_dir("abs");
        let tool = write_file(&dir, "tool", 0o755);
        let plain = write_file(&dir, "plain", 0o600);

        assert_eq!(resolve_executable(tool.to_str().unwrap(), None), Some(tool));
        assert_eq!(resolve_executable(plain.to_str().unwrap(), None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_and_unset_path_resolve_nothing() {
        let dir = tempfile_dir("dir");
        std::fs::create_dir(dir.join("tool")).unwrap();
        let path_var = OsString::from(dir.as_os_str());

        assert_eq!(resolve_executable("tool", Some(&path_var)), None);
        assert_eq!(resolve_executable("tool", None), None);
        assert_eq!(resolve_executable("", Some(&path_var)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = pohunek_test_support::temp_root().join(format!(
            "pohunek-hostcheck-exec-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
