//! Resolution of an agent executable against a resolved search path.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::search_path::SearchPath;

/// Reports why a program name did not resolve to an executable.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ExecutableError {
    /// The name is empty or holds a NUL byte.
    #[error("program name is empty or contains a NUL byte")]
    InvalidName,
    /// A name with a `/` must be absolute; a relative path would depend on
    /// the launcher's working directory.
    #[error("a program path containing `/` must be absolute")]
    RelativePath,
    /// The absolute path is not an executable regular file.
    #[error("the configured executable is not an executable file")]
    NotExecutable,
    /// No searched directory holds an executable of that name.
    #[error("no executable of that name exists in the search path")]
    NotFound,
}

/// Resolves `program` to an executable file.
///
/// A name containing `/` is a configured executable: it must be absolute and
/// executable, and `search` is not consulted. A bare name is looked up in the
/// directories of `search` in order, and the first executable regular file
/// wins. Symlinks resolve like `execve` resolves them.
///
/// # Examples
///
/// ```
/// use std::ffi::OsStr;
/// use pohunek_platform::shell_env::{resolve_executable, SearchPath};
///
/// let search = SearchPath::new(vec!["/bin".into(), "/usr/bin".into()])?;
/// let sh = resolve_executable(OsStr::new("sh"), &search)?;
/// assert!(sh.is_absolute());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// Returns [`ExecutableError`] for an invalid or relative name, a configured
/// path that is not executable, and a bare name found nowhere.
pub fn resolve_executable(
    program: &OsStr,
    search: &SearchPath,
) -> Result<PathBuf, ExecutableError> {
    let bytes = program.as_bytes();
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(ExecutableError::InvalidName);
    }
    if bytes.contains(&b'/') {
        let path = Path::new(program);
        if !path.is_absolute() {
            return Err(ExecutableError::RelativePath);
        }
        return if is_executable_file(path) {
            Ok(path.to_path_buf())
        } else {
            Err(ExecutableError::NotExecutable)
        };
    }
    search
        .entries()
        .iter()
        .map(|directory| directory.join(program))
        .find(|candidate| is_executable_file(candidate))
        .ok_or(ExecutableError::NotFound)
}

/// Whether `path` is a regular file the effective user can execute.
///
/// Asks the kernel with `faccessat(X_OK, AT_EACCESS)`, so owner, group, ACL,
/// and parent-directory search permission all count. Symlinks are followed.
/// A candidate the user cannot execute is skipped, as a shell skips it.
pub(super) fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
        && rustix::fs::accessat(
            rustix::fs::CWD,
            path,
            rustix::fs::Access::EXEC_OK,
            rustix::fs::AtFlags::EACCESS,
        )
        .is_ok()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[test]
    fn a_bare_name_resolves_in_search_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first bin");
        let second = dir.path().join("zwei\u{e9}");
        fs::create_dir_all(&first).expect("first");
        fs::create_dir_all(&second).expect("second");
        let winner = executable(&first, "agent");
        executable(&second, "agent");
        let only_second = executable(&second, "only-second");
        let search = SearchPath::new(vec![first, second]).expect("search path");
        assert_eq!(
            resolve_executable(OsStr::new("agent"), &search).expect("agent"),
            winner
        );
        assert_eq!(
            resolve_executable(OsStr::new("only-second"), &search).expect("only-second"),
            only_second
        );
    }

    #[test]
    fn names_with_spaces_quotes_and_unicode_resolve_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in [
            "my agent",
            "it's",
            "agent \"x\"",
            "\u{65e5}\u{672c}\u{8a9e}",
            "$(id)",
        ] {
            let path = executable(dir.path(), name);
            let search = SearchPath::new(vec![dir.path().to_path_buf()]).expect("search");
            assert_eq!(
                resolve_executable(OsStr::new(name), &search).expect(name),
                path
            );
        }
    }

    #[test]
    fn a_configured_absolute_path_skips_the_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = executable(dir.path(), "agent");
        let search = SearchPath::empty();
        assert_eq!(
            resolve_executable(path.as_os_str(), &search).expect("absolute"),
            path
        );
    }

    #[test]
    fn a_relative_path_with_a_slash_is_refused() {
        let error =
            resolve_executable(OsStr::new("./agent"), &SearchPath::empty()).expect_err("relative");
        assert_eq!(error, ExecutableError::RelativePath);
        let error = resolve_executable(OsStr::new("bin/agent"), &SearchPath::empty())
            .expect_err("relative");
        assert_eq!(error, ExecutableError::RelativePath);
    }

    #[test]
    fn non_executable_and_missing_programs_are_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plain = dir.path().join("plain");
        fs::write(&plain, "data").expect("plain");
        let search = SearchPath::new(vec![dir.path().to_path_buf()]).expect("search");
        assert_eq!(
            resolve_executable(plain.as_os_str(), &search),
            Err(ExecutableError::NotExecutable)
        );
        assert_eq!(
            resolve_executable(OsStr::new("plain"), &search),
            Err(ExecutableError::NotFound)
        );
        assert_eq!(
            resolve_executable(OsStr::new(""), &search),
            Err(ExecutableError::InvalidName)
        );
        // A directory named like the program is not an executable file.
        fs::create_dir(dir.path().join("dir")).expect("dir");
        assert_eq!(
            resolve_executable(OsStr::new("dir"), &search),
            Err(ExecutableError::NotFound)
        );
    }

    #[test]
    fn a_file_the_user_cannot_execute_is_skipped_along_the_search() {
        // Root passes the execute check for any file with an execute bit.
        if rustix::process::geteuid().is_root() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir_all(&first).expect("first");
        fs::create_dir_all(&second).expect("second");
        // Executable by group and others only: the owner is refused.
        let unusable = first.join("agent");
        fs::write(&unusable, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&unusable, fs::Permissions::from_mode(0o011)).expect("chmod");
        let usable = executable(&second, "agent");
        let search = SearchPath::new(vec![first, second]).expect("search");
        assert_eq!(
            resolve_executable(OsStr::new("agent"), &search).expect("agent"),
            usable
        );
        assert_eq!(
            resolve_executable(unusable.as_os_str(), &search),
            Err(ExecutableError::NotExecutable)
        );
    }

    #[test]
    fn owner_only_execute_permission_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent");
        fs::write(&path, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod");
        let search = SearchPath::new(vec![dir.path().to_path_buf()]).expect("search");
        assert_eq!(
            resolve_executable(OsStr::new("agent"), &search).expect("agent"),
            path
        );
    }
}
