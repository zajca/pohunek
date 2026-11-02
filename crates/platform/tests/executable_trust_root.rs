//! The executable lookup under root-built fixtures on macOS.
//!
//! A root process never trusts an `admin`-group-writable directory, because any
//! admin could then plant a binary that root runs. A non-root process trusts
//! the user's own `admin` directories but refuses a `root:admin` one and any
//! entry another account owns inside them. The scenarios need root to create
//! root-owned and foreign-owned fixtures, so they are ignored in the ordinary
//! run; the macOS lane runs them in its sudo step from the extracted archive.
//! The non-root checks run in a child of the test binary whose uid and gid are
//! dropped to the account that invoked `sudo`.

#![cfg(target_os = "macos")]

// Rust guideline compliant 2026-11-01

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{chown, lchown, symlink, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use pohunek_platform::shell_env::{
    fallback_search_path, is_trusted_executable_file, resolve_executable, trusted_directory,
    ExecutableError, Refusal, SearchPath, TrustError,
};

/// The macOS `admin` group.
const ADMIN_GID: u32 = 80;

/// The macOS `staff` group, a group other than `admin`.
const STAFF_GID: u32 = 20;

/// Why `trusted_directory` refuses an `admin`-group directory the effective
/// user does not own.
const ADMIN_GROUP_NOT_OWNED: &str = "writable by the admin group (gid 80), accepted only in a directory the effective user owns and never for root";

/// Why `trusted_directory` refuses a group-writable directory of another group.
const OTHER_GROUP: &str = "writable by a group other than admin (gid 80)";

/// Why `trusted_directory` refuses a directory whose ancestor is unsafe.
const UNSAFE_ANCESTOR: &str = "a path component is foreign-owned or writable by others";

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
    let admin_entry = admin.to_str().expect("UTF-8");
    let plain_entry = directory(&root, "control", 0o755);
    let plain_entry = plain_entry.to_str().expect("UTF-8");
    let resolved = fallback_search_path(&[plain_entry, admin_entry], None)
        .expect("the control directory is trusted");
    let dropped = resolved
        .untrusted
        .iter()
        .find(|dropped| dropped.entry == admin_entry)
        .expect("the admin directory is reported");
    assert!(
        dropped.reason.contains("never for root"),
        "root is told why the admin group is not accepted: {}",
        dropped.reason
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

/// The variable through which the parent hands the fixture root to its child.
const FIXTURE_ROOT_VARIABLE: &str = "TRUST_FIXTURE_ROOT";

/// The test the unprivileged child runs.
const CHILD_TEST: &str = "an_unprivileged_process_applies_the_admin_group_rule_to_the_fixtures";

/// Parses a numeric id `sudo` hands to the commands it runs.
fn sudo_id(value: Result<String, std::env::VarError>, name: &str) -> u32 {
    value
        .unwrap_or_else(|_| panic!("{name} is unset: run this test through sudo"))
        .parse()
        .unwrap_or_else(|error| panic!("{name} is not a number: {error}"))
}

/// Builds the fixtures as root and checks them from a child process that runs
/// as the account that invoked `sudo`.
#[test]
#[ignore = "native macOS CI runs this test as root to create root-owned fixtures"]
fn root_built_fixtures_are_judged_by_an_unprivileged_process() {
    assert_eq!(rustix::process::geteuid().as_raw(), 0, "test requires root");
    let user = sudo_id(std::env::var("SUDO_UID"), "SUDO_UID");
    let group = sudo_id(std::env::var("SUDO_GID"), "SUDO_GID");
    assert_ne!(user, 0, "the invoking account must not be root");

    let temporary = pohunek_test_support::tempdir().expect("create the fixture root");
    let root = fs::canonicalize(temporary.path()).expect("canonicalize the fixture root");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("set the root mode");

    // A directory the invoking user owns in the admin group, holding a program
    // it owns: the control the exception must accept.
    let owned = directory(&root, "owned", 0o775);
    chown(&owned, Some(user), Some(ADMIN_GID)).expect("hand the directory to the user");
    let owned_program = executable(&owned, "agent");
    chown(&owned_program, Some(user), Some(group)).expect("hand the program to the user");

    // The same shape owned by root: refused for anyone who is not root.
    let root_admin = directory(&root, "root-admin", 0o775);
    chown(&root_admin, Some(0), Some(ADMIN_GID)).expect("set the admin group");
    executable(&root_admin, "agent");

    // A user-owned admin directory holding entries another account owns.
    let shared = directory(&root, "shared", 0o775);
    chown(&shared, Some(user), Some(ADMIN_GID)).expect("hand the directory to the user");
    let link = shared.join("link");
    symlink(&owned_program, &link).expect("create the symlink");
    lchown(&link, Some(FOREIGN_UID), None).expect("hand the symlink to another account");
    let foreign = directory(&shared, "foreign", 0o755);
    chown(&foreign, Some(FOREIGN_UID), Some(ADMIN_GID)).expect("hand the directory away");
    let foreign_program = executable(&foreign, "agent");
    chown(&foreign_program, Some(user), Some(group)).expect("hand the program to the user");

    // A user-owned directory of the `staff` group the group can write: the
    // group is not `admin`, so no site may accept it.
    let staff = directory(&root, "staff", 0o775);
    chown(&staff, Some(user), Some(STAFF_GID)).expect("hand the directory to the user");
    let staff_program = executable(&staff, "agent");
    chown(&staff_program, Some(user), Some(group)).expect("hand the program to the user");

    // User-owned directories that only the ancestor walk can refuse: each is
    // 0755, but the directory above it is group-writable and either not owned
    // by the user or not of the `admin` group.
    let below_root_admin = directory(&root_admin, "below", 0o755);
    chown(&below_root_admin, Some(user), Some(ADMIN_GID)).expect("hand the directory to the user");
    let below_staff = directory(&staff, "below", 0o755);
    chown(&below_staff, Some(user), Some(STAFF_GID)).expect("hand the directory to the user");

    let test_binary = std::env::current_exe().expect("locate the test binary");
    let output = Command::new(test_binary)
        .args(pohunek_test_support::exact_test_filter(
            module_path!(),
            CHILD_TEST,
        ))
        .args(["--ignored", "--nocapture"])
        .env_clear()
        .env(FIXTURE_ROOT_VARIABLE, &root)
        .uid(user)
        .gid(group)
        .output()
        .expect("run the unprivileged child");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the unprivileged child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The checks of [`root_built_fixtures_are_judged_by_an_unprivileged_process`],
/// run as the account that invoked `sudo`. The parent test starts it; started
/// on its own it fails for lack of fixtures.
#[test]
#[ignore = "started by root_built_fixtures_are_judged_by_an_unprivileged_process"]
fn an_unprivileged_process_applies_the_admin_group_rule_to_the_fixtures() {
    assert_ne!(
        rustix::process::geteuid().as_raw(),
        0,
        "the child must not be root"
    );
    let root = PathBuf::from(
        std::env::var_os(FIXTURE_ROOT_VARIABLE).expect("the parent hands over the fixtures"),
    );
    let search = SearchPath::new(vec![root.clone()]).expect("search path");

    let owned = root.join("owned");
    assert!(
        is_trusted_executable_file(&owned.join("agent")),
        "a user-owned admin directory with the user's program is trusted"
    );
    assert!(
        trusted_directory(&owned).is_ok(),
        "and a trusted search directory"
    );

    // The holder check decides first for a program inside a group-writable
    // directory, so its two conditions show as `ChainSharedDirectory`; a site
    // that wrongly accepts would let the ancestor walk answer with
    // `ChainDirectory` instead.
    let root_admin = root.join("root-admin");
    assert!(
        matches!(
            resolve_executable(root_admin.join("agent").as_os_str(), &search),
            Err(ExecutableError::NotExecutable(Refusal::ChainSharedDirectory {
                dir,
                owner: 0
            })) if dir == root_admin
        ),
        "a root-owned admin directory is refused by the holder check"
    );
    assert_eq!(
        trusted_directory(&root_admin),
        Err(TrustError::Refused(ADMIN_GROUP_NOT_OWNED)),
        "and by the search directory check"
    );
    let staff = root.join("staff");
    assert!(
        matches!(
            resolve_executable(staff.join("agent").as_os_str(), &search),
            Err(ExecutableError::NotExecutable(Refusal::ChainSharedDirectory {
                dir,
                ..
            })) if dir == staff
        ),
        "a user-owned group-writable directory of another group is refused by the holder check"
    );
    assert_eq!(
        trusted_directory(&staff),
        Err(TrustError::Refused(OTHER_GROUP)),
        "and by the search directory check"
    );
    // The directory itself is 0755 and owned by the user, so only the
    // ancestor walk refuses it.
    assert_eq!(
        trusted_directory(&root_admin.join("below")),
        Err(TrustError::Refused(UNSAFE_ANCESTOR)),
        "an admin-group ancestor the user does not own is refused by the ancestor walk"
    );
    assert_eq!(
        trusted_directory(&staff.join("below")),
        Err(TrustError::Refused(UNSAFE_ANCESTOR)),
        "a group-writable ancestor of another group is refused by the ancestor walk"
    );

    let shared = root.join("shared");
    assert!(
        matches!(
            resolve_executable(shared.join("link").as_os_str(), &search),
            Err(ExecutableError::NotExecutable(
                Refusal::ChainSharedDirectory {
                    owner: FOREIGN_UID,
                    ..
                }
            ))
        ),
        "a symlink another account owns inside a user-owned admin directory is refused"
    );
    assert!(
        matches!(
            resolve_executable(shared.join("foreign/agent").as_os_str(), &search),
            Err(ExecutableError::NotExecutable(
                Refusal::ChainSharedDirectory {
                    owner: FOREIGN_UID,
                    ..
                }
            ))
        ),
        "a user's program below a directory another account owns is refused"
    );
}
