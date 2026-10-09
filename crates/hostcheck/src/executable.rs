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
