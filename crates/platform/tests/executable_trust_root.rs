//! The executable lookup as root on macOS.
//!
//! A root process never trusts an `admin`-group-writable directory, because any
//! admin could then plant a binary that root runs. The scenario needs root to
//! create root-owned fixtures, so it is ignored in the ordinary run; the macOS
//! lane runs it in its sudo step from the extracted archive.

#![cfg(target_os = "macos")]

// Rust guideline compliant 2026-11-01

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{chown, lchown, symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};

use pohunek_platform::shell_env::{
    is_trusted_executable_file, resolve_executable, trusted_directory, ExecutableError, Refusal,
    SearchPath,
};

/// The macOS `admin` group.
const ADMIN_GID: u32 = 80;

/// An account other than root and the effective user.
const FOREIGN_UID: u32 = 1;

fn directory(parent: &Path, name: &str, mode: u32) -> PathBuf {
    let path = parent.join(name);
    fs::create_dir(&path).expect("create the directory");
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set the mode");
    path
}

fn executable(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    pohunek_test_support::fs::write_executable(&path, "#!/bin/sh\n").expect("write the program");
    path
}

#[test]
#[ignore = "native macOS CI runs this test as root to create root-owned fixtures"]
fn root_never_trusts_an_admin_group_writable_directory() {
    assert_eq!(rustix::process::geteuid().as_raw(), 0, "test requires root");
    let temporary = pohunek_test_support::tempdir().expect("create the fixture root");
    let root = fs::canonicalize(temporary.path()).expect("canonicalize the fixture root");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("set the root mode");

    let admin = directory(&root, "admin", 0o775);
    chown(&admin, None, Some(ADMIN_GID)).expect("set the admin group");
    let program = executable(&admin, "agent");
    assert!(
        !is_trusted_executable_file(&program),
        "a root-owned admin-writable directory is refused for root"
    );
    let search = SearchPath::new(vec![admin.clone()]).expect("search path");
    assert!(
        resolve_executable(OsStr::new("agent"), &search).is_err(),
        "the bare name does not resolve in it"
    );
    assert!(
        matches!(
            resolve_executable(program.as_os_str(), &search),
            Err(ExecutableError::NotExecutable(
                Refusal::ChainDirectory { .. } | Refusal::ChainSharedDirectory { .. }
            ))
        ),
        "the configured path names the untrusted directory"
    );
    assert!(
        trusted_directory(&admin).is_err(),
        "the directory is not a trusted search directory"
    );

    let plain = directory(&root, "plain", 0o755);
    let target = executable(&plain, "agent");
    assert!(
        is_trusted_executable_file(&target),
        "the control is trusted"
    );
    let link = plain.join("link");
    symlink(&target, &link).expect("create the symlink");
    lchown(&link, Some(FOREIGN_UID), None).expect("hand the symlink to another account");
    assert!(
        matches!(
            resolve_executable(link.as_os_str(), &search),
            Err(ExecutableError::NotExecutable(Refusal::ChainSymlinkOwner {
                owner: FOREIGN_UID
            }))
        ),
        "a symlink owned by another account is refused"
    );
}
