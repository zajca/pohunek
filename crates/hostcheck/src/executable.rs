//! Executable resolution shared by every host probe.
//!
//! Every probe delegates to `pohunek_platform::shell_env`, the resolver the
//! daemon spawns programs with, so the doctor, the capability snapshot, and the
//! spawn path always agree. A candidate counts only when it is a regular file
//! (after following symlinks) owned by the effective user or root, writable by
//! neither group nor others, in directories only the owner or root can change,
//! and executable for the effective user. A candidate that fails is skipped, so
//! it neither resolves nor stops the `PATH` search; relative and empty `PATH`
//! entries are skipped too. Resolution takes the `PATH` value as an argument so
//! tests never mutate the process environment.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use pohunek_platform::shell_env;

// Rust guideline compliant 2026-10-01

/// Whether `path` is an executable the daemon would run.
///
/// The shared trusted-executable check: see the module documentation. The file
/// is never run.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    shell_env::is_trusted_executable_file(path)
}

/// Resolve `program` to an executable file.
///
/// A `program` containing a path separator must be absolute and is checked as
/// given; a bare name is searched in each absolute directory of `path_var` in
/// order. An unset `PATH` resolves nothing for a bare name.
#[must_use]
pub fn resolve_executable(program: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    shell_env::resolve_executable_in_path_value(OsStr::new(program), path_var).ok()
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
        let dir_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-non-exec-")
            .expect("private fixture directory");
        let dir = dir_guard.path();
        write_file(dir, "tool", 0o644);
        let path_var = OsString::from(dir.as_os_str());

        assert_eq!(resolve_executable("tool", Some(&path_var)), None);
    }

    #[test]
    fn executable_file_on_path_is_resolved() {
        let dir_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-exec-")
            .expect("private fixture directory");
        let dir = dir_guard.path();
        let tool = write_file(dir, "tool", 0o755);
        let path_var = OsString::from(dir.as_os_str());

        assert_eq!(resolve_executable("tool", Some(&path_var)), Some(tool));
    }

    #[test]
    fn execute_permission_is_decided_for_the_effective_user() {
        let dir_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-owner-bits-")
            .expect("private fixture directory");
        let dir = dir_guard.path();
        // Owner-only read and execute: executable for the owner running this
        // test. (An execute-only file cannot be opened for inspection and is
        // skipped like any candidate the shared check cannot vouch for.)
        let owner_only = write_file(dir, "owner-only", 0o500);
        // Group/other execute without the owner bit: the kernel denies the
        // owner, unlike a check of "any execute bit".
        let others_only = write_file(dir, "others-only", 0o011);

        assert!(is_executable_file(&owner_only));
        if !rustix::process::geteuid().is_root() {
            assert!(!is_executable_file(&others_only));
        }
    }

    #[test]
    fn an_unexecutable_entry_does_not_stop_the_search() {
        let first_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-eacces-a-")
            .expect("private fixture directory");
        let first = first_guard.path();
        let second_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-eacces-b-")
            .expect("private fixture directory");
        let second = second_guard.path();
        write_file(first, "tool", 0o011);
        let tool = write_file(second, "tool", 0o755);
        let path_var = std::env::join_paths([first, second]).unwrap();

        let resolved = resolve_executable("tool", Some(&path_var));

        if rustix::process::geteuid().is_root() {
            assert!(resolved.is_some());
        } else {
            assert_eq!(resolved, Some(tool));
        }
    }

    #[test]
    fn earlier_non_executable_entry_does_not_shadow_a_later_executable() {
        let first_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-shadow-a-")
            .expect("private fixture directory");
        let first = first_guard.path();
        let second_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-shadow-b-")
            .expect("private fixture directory");
        let second = second_guard.path();
        write_file(first, "tool", 0o644);
        let tool = write_file(second, "tool", 0o755);
        let path_var = std::env::join_paths([first, second]).unwrap();

        assert_eq!(resolve_executable("tool", Some(&path_var)), Some(tool));
    }

    #[test]
    fn program_with_separator_is_checked_as_given_and_ignores_path() {
        let dir_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-abs-")
            .expect("private fixture directory");
        let dir = dir_guard.path();
        let tool = write_file(dir, "tool", 0o755);
        let plain = write_file(dir, "plain", 0o600);

        assert_eq!(resolve_executable(tool.to_str().unwrap(), None), Some(tool));
        assert_eq!(resolve_executable(plain.to_str().unwrap(), None), None);
    }

    #[test]
    fn directory_and_unset_path_resolve_nothing() {
        let dir_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-dir-")
            .expect("private fixture directory");
        let dir = dir_guard.path();
        std::fs::create_dir(dir.join("tool")).unwrap();
        let path_var = OsString::from(dir.as_os_str());

        assert_eq!(resolve_executable("tool", Some(&path_var)), None);
        assert_eq!(resolve_executable("tool", None), None);
        assert_eq!(resolve_executable("", Some(&path_var)), None);
    }

    #[test]
    fn a_writable_file_or_directory_never_resolves_and_a_safe_later_one_wins() {
        let loose_file_dir_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-loose-file-")
            .expect("private fixture directory");
        let loose_file_dir = loose_file_dir_guard.path();
        let loose_dir_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-loose-dir-")
            .expect("private fixture directory");
        let loose_dir = loose_dir_guard.path();
        let safe_dir_guard = pohunek_test_support::tempdir_with_prefix("ph-hc-safe-")
            .expect("private fixture directory");
        let safe_dir = safe_dir_guard.path();
        write_file(loose_file_dir, "git", 0o777);
        std::fs::set_permissions(loose_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        write_file(loose_dir, "git", 0o755);
        let safe = write_file(safe_dir, "git", 0o755);
        for unsafe_only in [loose_file_dir, loose_dir] {
            let path_var = OsString::from(unsafe_only.as_os_str());
            assert_eq!(resolve_executable("git", Some(&path_var)), None);
            assert!(!is_executable_file(&unsafe_only.join("git")));
            assert_ne!(
                crate::binary_with_path("git", true, Some(&path_var), "").status,
                crate::DoctorStatus::Ok
            );
        }
        let path_var = std::env::join_paths([loose_file_dir, loose_dir, safe_dir]).unwrap();
        assert_eq!(
            resolve_executable("git", Some(&path_var)),
            Some(safe.clone())
        );
        let check = crate::binary_with_path("git", true, Some(&path_var), "");
        assert_eq!(check.status, crate::DoctorStatus::Ok);
        assert!(
            check.detail.contains(&safe.display().to_string()),
            "{check:?}"
        );
    }

    /// The doctor accepts stock macOS tools; a failure prints the typed reason.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_doctor_accepts_stock_macos_tools() {
        let search = std::env::join_paths(["/usr/bin", "/bin"]).unwrap();
        for name in ["sh", "false", "zsh"] {
            let check = crate::binary_with_path(name, true, Some(&search), "");
            let reason = pohunek_platform::shell_env::resolve_executable(
                OsStr::new(&format!("/bin/{name}")),
                &pohunek_platform::shell_env::SearchPath::empty(),
            );
            assert_eq!(
                check.status,
                crate::DoctorStatus::Ok,
                "bin:{name}: {check:?} (/bin/{name}: {reason:?})"
            );
        }
    }
}
