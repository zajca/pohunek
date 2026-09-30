//! Executable resolution shared by every host probe.
//!
//! A file counts as an executable only when it is a regular file (after
//! following symlinks) that the kernel says the effective user can execute:
//! owner, group, ACL and search permission on every parent directory all
//! count. A candidate the user cannot execute is skipped, exactly as a shell
//! skips it, so it neither resolves nor stops the `PATH` search. Resolution
//! takes the `PATH` value as an argument so tests never mutate the process
//! environment.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

// Rust guideline compliant 2026-09-30

/// Whether `path` is a regular file the effective user can execute.
///
/// Uses `faccessat(X_OK, AT_EACCESS)`, the kernel's own answer for the
/// effective user and group. Symlinks are followed, so a link to an executable
/// qualifies and a link to a data file or a directory does not. The file is
/// never run.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
        && rustix::fs::accessat(
            rustix::fs::CWD,
            path,
            rustix::fs::Access::EXEC_OK,
            rustix::fs::AtFlags::EACCESS,
        )
        .is_ok()
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
    use std::os::unix::fs::PermissionsExt as _;

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
    fn execute_permission_is_decided_for_the_effective_user() {
        let dir = tempfile_dir("owner-bits");
        // Owner-only execute: executable for the owner running this test.
        let owner_only = write_file(&dir, "owner-only", 0o100);
        // Group/other execute without the owner bit: the kernel denies the
        // owner, unlike a check of "any execute bit".
        let others_only = write_file(&dir, "others-only", 0o011);

        assert!(is_executable_file(&owner_only));
        if !rustix::process::geteuid().is_root() {
            assert!(!is_executable_file(&others_only));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unexecutable_entry_does_not_stop_the_search() {
        let first = tempfile_dir("eacces-a");
        let second = tempfile_dir("eacces-b");
        write_file(&first, "tool", 0o011);
        let tool = write_file(&second, "tool", 0o755);
        let path_var = std::env::join_paths([&first, &second]).unwrap();

        let resolved = resolve_executable("tool", Some(&path_var));

        if rustix::process::geteuid().is_root() {
            assert!(resolved.is_some());
        } else {
            assert_eq!(resolved, Some(tool));
        }
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
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
