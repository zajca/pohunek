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
    resolve_in(
        program,
        search.entries().iter().map(PathBuf::as_path),
        Owners::current(),
    )
}

/// Resolves `program` against the raw value of an environment `PATH`.
///
/// Same rules as [`resolve_executable`], for a caller that holds the inherited
/// `PATH` (the daemon's). Entries that are empty or relative are skipped, never
/// resolved against the working directory, and the search continues with the
/// rest, so a trailing colon or a `.` entry costs nothing. The value has no
/// length bound. An absent `PATH` resolves no bare name.
///
/// # Errors
///
/// See [`resolve_executable`].
pub fn resolve_executable_in_path_value(
    program: &OsStr,
    path: Option<&OsStr>,
) -> Result<PathBuf, ExecutableError> {
    let entries: Vec<PathBuf> = path
        .map(|value| {
            std::env::split_paths(value)
                .filter(|entry| entry.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    resolve_in(
        program,
        entries.iter().map(PathBuf::as_path),
        Owners::current(),
    )
}

fn resolve_in<'a>(
    program: &OsStr,
    directories: impl Iterator<Item = &'a Path>,
    owners: Owners,
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
        return if is_trusted_executable(path, owners) {
            Ok(path.to_path_buf())
        } else {
            Err(ExecutableError::NotExecutable)
        };
    }
    directories
        .map(|directory| directory.join(program))
        .find(|candidate| is_trusted_executable(candidate, owners))
        .ok_or(ExecutableError::NotFound)
}

/// Users whose files may be executed: the effective user and root.
#[derive(Debug, Clone, Copy)]
struct Owners {
    effective: u32,
    root: u32,
}

impl Owners {
    fn current() -> Self {
        Self {
            effective: rustix::process::geteuid().as_raw(),
            root: 0,
        }
    }

    const fn allows(self, uid: u32) -> bool {
        uid == self.effective || uid == self.root
    }
}

/// Whether `path` is an executable the daemon may run as the owner.
///
/// The decision is bound to the opened file: symlinks are resolved first, then
/// the final file is opened with `O_NOFOLLOW | O_CLOEXEC` and its descriptor is
/// inspected. It must be a regular file owned by the effective user or root,
/// writable by neither group nor others, and (on macOS) free of an ACL that
/// grants anything, using the platform's own ACL check for trusted entries.
/// Finally the kernel must agree the effective user can execute it
/// (`faccessat(X_OK, AT_EACCESS)`). A symlink's own owner is irrelevant once its
/// target is validated. An untrusted candidate is skipped, like one the shell
/// cannot execute. An executable the user cannot read cannot be opened and is
/// skipped as well. The path is returned as found, not canonicalized, so a
/// multi-call binary keeps the name it was invoked by; group-writable
/// executables (an admin-group Intel Homebrew) are out of scope.
///
/// A path-based exec cannot be made atomic with the check, so the directories
/// the lookup passes through must also be owner-controlled
/// ([`lookup_chain_is_owner_controlled`]): then nobody else can rename or
/// retarget an entry after the check.
pub(super) fn is_executable_file(path: &Path) -> bool {
    is_trusted_executable(path, Owners::current())
}

/// The public form of the candidate check every resolver here applies.
///
/// For callers that probe one known path (the doctor, the CLI's daemon
/// lookup) and must agree with the spawn path; the rule is that of
/// [`resolve_executable`].
#[must_use]
pub fn is_trusted_executable_file(path: &Path) -> bool {
    is_executable_file(path)
}

/// Whether every directory the lookup of `path` passes through is
/// owner-controlled, symlink targets included.
///
/// The path is resolved component by component, and each directory searched on
/// the way (the canonical directory holding the next entry, including the final
/// file's directory) must pass the platform's trusted-ancestor policy
/// ([`TrustedDir::open_absolute_ancestor`](crate::filesystem::TrustedDir)):
/// effective-user-owned components are not group- or world-writable and
/// root-owned ones are non-writable or sticky, so no other account can rename
/// an entry on either the lexical or the canonical chain.
///
/// Every symlink on the chain must be owned by `owners`, and a directory that
/// others can write (a sticky `/tmp`) may hold a chain entry only when that
/// entry is owned by `owners` too: sticky protects an entry from everyone but
/// its owner, so an entry another account created there is the attack itself.
fn lookup_chain_is_owner_controlled(path: &Path, owners: Owners) -> bool {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::Component;

    /// Symlinks followed before the lookup is treated as a loop.
    const MAX_LINKS: usize = 40;
    /// Group and other write permission bits.
    const WRITABLE_BY_OTHERS: u32 = 0o022;
    /// The sticky bit.
    const STICKY: u32 = 0o1000;

    let mut pending: VecDeque<OsString> = path
        .components()
        .filter_map(|component| match component {
            Component::RootDir | Component::CurDir => None,
            other => Some(other.as_os_str().to_owned()),
        })
        .collect();
    let mut directory = PathBuf::from("/");
    let mut searched: Vec<PathBuf> = Vec::new();
    let mut links = 0_usize;
    while let Some(name) = pending.pop_front() {
        if name == ".." {
            directory.pop();
            continue;
        }
        if !searched.contains(&directory) {
            searched.push(directory.clone());
        }
        let next = directory.join(&name);
        let Ok(metadata) = std::fs::symlink_metadata(&next) else {
            return false;
        };
        let owned = owners.allows(metadata.uid());
        let Ok(holder) = std::fs::metadata(&directory) else {
            return false;
        };
        let holder_mode = holder.mode();
        if holder_mode & WRITABLE_BY_OTHERS != 0 && !(holder_mode & STICKY != 0 && owned) {
            return false;
        }
        if !metadata.file_type().is_symlink() {
            directory = next;
            continue;
        }
        if !owned {
            return false;
        }
        links += 1;
        let Ok(target) = std::fs::read_link(&next) else {
            return false;
        };
        if links > MAX_LINKS {
            return false;
        }
        if target.is_absolute() {
            directory = PathBuf::from("/");
        }
        for component in target.components().rev() {
            match component {
                Component::RootDir | Component::CurDir => {}
                other => pending.push_front(other.as_os_str().to_owned()),
            }
        }
    }
    // The filesystem root has no components for the ancestor policy to judge;
    // renaming below it needs write access to a directory that is judged.
    searched
        .iter()
        .filter(|dir| dir.as_path() != Path::new("/"))
        .all(|dir| crate::filesystem::TrustedDir::open_absolute_ancestor(dir).is_ok())
}

fn is_trusted_executable(path: &Path, owners: Owners) -> bool {
    use rustix::fs::{FileType, Mode, OFlags};

    let Ok(target) = std::fs::canonicalize(path) else {
        return false;
    };
    // A FIFO without a writer or a device node must neither block the lookup
    // nor be opened at all, so non-regular files are skipped on a plain `stat`,
    // and the open itself is non-blocking.
    if !std::fs::metadata(&target).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let Ok(descriptor) = rustix::fs::open(
        &target,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return false;
    };
    let file = std::fs::File::from(descriptor);
    let Ok(stat) = rustix::fs::fstat(&file) else {
        return false;
    };
    let mode = Mode::from_raw_mode(stat.st_mode);
    FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
        && owners.allows(stat.st_uid)
        && !mode.intersects(Mode::WGRP | Mode::WOTH)
        && crate::filesystem::validate_private_acl(&file, &target).is_ok()
        && lookup_chain_is_owner_controlled(path, owners)
        && rustix::fs::accessat(
            rustix::fs::CWD,
            &target,
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
    use crate::shell_env::test_support::{fixture, make_dir};

    fn executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[test]
    fn a_bare_name_resolves_in_search_order() {
        let dir = fixture();
        let first = dir.path().join("first bin");
        let second = dir.path().join("zwei\u{e9}");
        make_dir(dir.path(), &first);
        make_dir(dir.path(), &second);
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
        let dir = fixture();
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
        let dir = fixture();
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
        let dir = fixture();
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
        let dir = fixture();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        make_dir(dir.path(), &first);
        make_dir(dir.path(), &second);
        let usable = executable(&second, "agent");
        let search = SearchPath::new(vec![first.clone(), second]).expect("search");
        // No execute bit at all: refused for every user, root included.
        let no_bits = first.join("agent");
        fs::write(&no_bits, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&no_bits, fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(
            resolve_executable(OsStr::new("agent"), &search).expect("agent"),
            usable
        );
        assert_eq!(
            resolve_executable(no_bits.as_os_str(), &search),
            Err(ExecutableError::NotExecutable)
        );
        // Executable by group and others only: the owner is refused by the
        // kernel, but root may run any file that has an execute bit.
        let others_only = first.join("others-only");
        fs::write(&others_only, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&others_only, fs::Permissions::from_mode(0o011)).expect("chmod");
        let owner_refused = !rustix::process::geteuid().is_root();
        assert_eq!(
            resolve_executable(others_only.as_os_str(), &search).is_err(),
            owner_refused
        );
    }

    #[test]
    fn owner_only_execute_permission_counts() {
        let dir = fixture();
        let path = dir.path().join("agent");
        fs::write(&path, "#!/bin/sh\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod");
        let search = SearchPath::new(vec![dir.path().to_path_buf()]).expect("search");
        assert_eq!(
            resolve_executable(OsStr::new("agent"), &search).expect("agent"),
            path
        );
    }

    #[test]
    fn an_environment_path_skips_empty_and_relative_entries_and_keeps_searching() {
        let dir = fixture();
        let bin = dir.path().join("bin");
        make_dir(dir.path(), &bin);
        let agent = executable(&bin, "agent");
        let cwd_copy = executable(dir.path(), "agent");
        assert_ne!(cwd_copy, agent);
        let absolute = bin.display().to_string();
        for path in [
            format!("{absolute}:"),
            format!(":{absolute}"),
            format!("::{absolute}"),
            format!("relative-bin:.:{absolute}"),
            format!("./node_modules/.bin:{absolute}:"),
        ] {
            assert_eq!(
                resolve_executable_in_path_value(OsStr::new("agent"), Some(OsStr::new(&path))),
                Ok(agent.clone()),
                "{path}"
            );
        }
        // Only relative or empty entries: nothing resolves, even though the
        // working directory could hold the program.
        for path in ["", ":", ".", "relative-bin", "./bin:"] {
            assert_eq!(
                resolve_executable_in_path_value(OsStr::new("agent"), Some(OsStr::new(path))),
                Err(ExecutableError::NotFound),
                "{path:?}"
            );
        }
        assert_eq!(
            resolve_executable_in_path_value(OsStr::new("agent"), None),
            Err(ExecutableError::NotFound)
        );
    }

    #[test]
    fn an_environment_path_has_no_length_bound() {
        let dir = fixture();
        let bin = dir.path().join("bin");
        make_dir(dir.path(), &bin);
        let agent = executable(&bin, "agent");
        let filler = format!("/{}", "x".repeat(200));
        let mut entries: Vec<String> = vec![filler; 40];
        entries.push(bin.display().to_string());
        let path = entries.join(":");
        assert!(path.len() > crate::shell_env::MAX_SEARCH_PATH_BYTES);
        assert_eq!(
            resolve_executable_in_path_value(OsStr::new("agent"), Some(OsStr::new(&path))),
            Ok(agent)
        );
    }

    fn with_mode(dir: &Path, name: &str, mode: u32) -> PathBuf {
        let path = executable(dir, name);
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
        path
    }

    #[test]
    fn a_writable_candidate_is_skipped_and_a_later_trusted_one_wins() {
        let dir = fixture();
        let loose = dir.path().join("loose");
        let tight = dir.path().join("tight");
        make_dir(dir.path(), &loose);
        make_dir(dir.path(), &tight);
        let trusted = executable(&tight, "agent");
        let search = SearchPath::new(vec![loose.clone(), tight]).expect("search");
        for mode in [0o777, 0o775, 0o757, 0o722] {
            with_mode(&loose, "agent", mode);
            assert_eq!(
                resolve_executable(OsStr::new("agent"), &search),
                Ok(trusted.clone()),
                "mode {mode:o}"
            );
            // A configured absolute path gets the same verdict.
            assert_eq!(
                resolve_executable(loose.join("agent").as_os_str(), &search),
                Err(ExecutableError::NotExecutable),
                "mode {mode:o}"
            );
        }
        // Tight modes are accepted, with or without the read bits for others.
        for mode in [0o755, 0o700, 0o750, 0o711] {
            let path = with_mode(&loose, "agent", mode);
            assert_eq!(
                resolve_executable(OsStr::new("agent"), &search),
                Ok(path),
                "mode {mode:o}"
            );
        }
    }

    #[test]
    fn a_foreign_owned_candidate_is_skipped() {
        let dir = fixture();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        make_dir(dir.path(), &first);
        make_dir(dir.path(), &second);
        let mine = executable(&first, "agent");
        let other = executable(&second, "agent");
        let dirs = [first.as_path(), second.as_path()];
        // The real owner of both files is neither of these ids, which stand in
        // for "another account" since a test cannot chown.
        let strangers = Owners {
            effective: u32::MAX - 1,
            root: u32::MAX - 2,
        };
        assert_eq!(
            resolve_in(OsStr::new("agent"), dirs.into_iter(), strangers),
            Err(ExecutableError::NotFound)
        );
        assert_eq!(
            resolve_in(mine.as_os_str(), std::iter::empty(), strangers),
            Err(ExecutableError::NotExecutable)
        );
        // Owned by the effective user, the first one wins again.
        let me = Owners {
            effective: rustix::process::geteuid().as_raw(),
            root: u32::MAX - 2,
        };
        assert_eq!(
            resolve_in(OsStr::new("agent"), dirs.into_iter(), me),
            Ok(mine)
        );
        assert_ne!(other, dir.path());
    }

    #[test]
    fn a_symlink_is_judged_by_its_target() {
        let dir = fixture();
        let cellar = dir.path().join("Cellar");
        let bin = dir.path().join("bin");
        make_dir(dir.path(), &cellar);
        make_dir(dir.path(), &bin);
        let real = executable(&cellar, "tool");
        let link = bin.join("tool");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let search = SearchPath::new(vec![bin.clone()]).expect("search");
        // The link itself is returned, like a shell would, after the target passed.
        assert_eq!(
            resolve_executable(OsStr::new("tool"), &search),
            Ok(link.clone())
        );
        fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).expect("chmod");
        assert_eq!(
            resolve_executable(OsStr::new("tool"), &search),
            Err(ExecutableError::NotFound)
        );
        // A dangling link is no executable either.
        fs::remove_file(&real).expect("remove target");
        assert_eq!(
            resolve_executable(OsStr::new("tool"), &search),
            Err(ExecutableError::NotFound)
        );
    }

    #[test]
    fn a_directory_or_special_file_is_never_an_executable() {
        let dir = fixture();
        make_dir(dir.path(), &dir.path().join("agent"));
        let search = SearchPath::new(vec![dir.path().to_path_buf()]).expect("search");
        assert_eq!(
            resolve_executable(OsStr::new("agent"), &search),
            Err(ExecutableError::NotFound)
        );
    }

    /// A world-writable directory (not sticky): anyone may rename its entries.
    fn wild_dir(root: &Path, name: &str) -> PathBuf {
        let wild = root.join(name);
        make_dir(root, &wild);
        fs::set_permissions(&wild, fs::Permissions::from_mode(0o777)).expect("chmod");
        wild
    }

    #[test]
    fn a_trusted_file_in_a_directory_others_can_write_is_refused() {
        let dir = fixture();
        let wild = wild_dir(dir.path(), "wild");
        let tight = dir.path().join("tight");
        make_dir(dir.path(), &tight);
        // The file is 0755 and ours; only its directory is the problem.
        executable(&wild, "agent");
        let trusted = executable(&tight, "agent");
        let search = SearchPath::new(vec![wild.clone(), tight]).expect("search");
        assert_eq!(
            resolve_executable(OsStr::new("agent"), &search),
            Ok(trusted)
        );
        assert_eq!(
            resolve_executable(wild.join("agent").as_os_str(), &search),
            Err(ExecutableError::NotExecutable)
        );
        // A directory below a writable one is refused as well.
        let below = wild.join("below");
        make_dir(dir.path(), &below);
        executable(&below, "agent");
        assert_eq!(
            resolve_executable(below.join("agent").as_os_str(), &search),
            Err(ExecutableError::NotExecutable)
        );
    }

    #[test]
    fn a_symlink_whose_lookup_directory_is_writable_is_refused() {
        let dir = fixture();
        let cellar = dir.path().join("Cellar");
        make_dir(dir.path(), &cellar);
        let real = executable(&cellar, "tool");
        let wild = wild_dir(dir.path(), "wild");
        std::os::unix::fs::symlink(&real, wild.join("tool")).expect("symlink");
        let search = SearchPath::new(vec![wild]).expect("search");
        // The target is trusted; the directory holding the link is not.
        assert_eq!(
            resolve_executable(OsStr::new("tool"), &search),
            Err(ExecutableError::NotFound)
        );
    }

    #[test]
    fn a_trusted_chain_under_a_sticky_root_owned_temporary_root_is_accepted() {
        // `fixture` lives in a private 0700 directory below the sticky,
        // world-writable, root-owned temporary root.
        let dir = fixture();
        let bin = dir.path().join("bin");
        make_dir(dir.path(), &bin);
        let agent = executable(&bin, "agent");
        let search = SearchPath::new(vec![bin]).expect("search");
        assert_eq!(resolve_executable(OsStr::new("agent"), &search), Ok(agent));
    }

    #[test]
    fn a_foreign_symlink_or_entry_in_a_sticky_directory_breaks_the_chain() {
        let dir = fixture();
        let real = executable(dir.path(), "real");
        let sticky = dir.path().join("sticky");
        make_dir(dir.path(), &sticky);
        fs::set_permissions(&sticky, fs::Permissions::from_mode(0o1777)).expect("chmod");
        let link = sticky.join("agent");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        // The owner ids stand in for "another account", since a test cannot chown.
        let strangers = Owners {
            effective: u32::MAX - 1,
            root: u32::MAX - 2,
        };
        assert!(!lookup_chain_is_owner_controlled(&link, strangers));
        // A plain entry another account put into the sticky directory is the
        // same attack.
        assert!(!lookup_chain_is_owner_controlled(
            &sticky.join("plain"),
            strangers
        ));
        // A symlink in a private directory is judged by its owner alone.
        let private = dir.path().join("private");
        make_dir(dir.path(), &private);
        let own = private.join("agent");
        std::os::unix::fs::symlink(&real, &own).expect("symlink");
        assert!(!lookup_chain_is_owner_controlled(&own, strangers));
        let me = Owners {
            effective: rustix::process::geteuid().as_raw(),
            root: u32::MAX - 2,
        };
        assert!(lookup_chain_is_owner_controlled(&own, me));
    }

    #[test]
    fn a_homebrew_style_relative_symlink_is_accepted() {
        let dir = fixture();
        let cellar = dir.path().join("Cellar/x/1.0/bin");
        let bin = dir.path().join("bin");
        make_dir(dir.path(), &cellar);
        make_dir(dir.path(), &bin);
        executable(&cellar, "x");
        std::os::unix::fs::symlink("../Cellar/x/1.0/bin/x", bin.join("x")).expect("symlink");
        let search = SearchPath::new(vec![bin.clone()]).expect("search");
        assert_eq!(
            resolve_executable(OsStr::new("x"), &search),
            Ok(bin.join("x"))
        );
    }

    #[test]
    fn a_fifo_without_a_writer_neither_blocks_nor_shadows_a_later_executable() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = fixture();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        make_dir(dir.path(), &first);
        make_dir(dir.path(), &second);
        let fifo = first.join("agent");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o755))
            .expect("mkfifo");
        let later = executable(&second, "agent");
        let search = SearchPath::new(vec![first, second]).expect("search");
        // A helper thread bounds the test: a blocking open would hang it.
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            drop(sender.send(resolve_executable(OsStr::new("agent"), &search)));
        });
        let resolved = receiver
            .recv_timeout(Duration::from_secs(20))
            .expect("the lookup returned instead of blocking on the FIFO");
        assert_eq!(resolved, Ok(later));
    }
}
