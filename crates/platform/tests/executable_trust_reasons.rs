//! The reasons the search-path policy gives for refusing a directory on macOS.

#![cfg(target_os = "macos")]

// Rust guideline compliant 2026-11-01

use std::fs;
use std::os::unix::fs::{chown, PermissionsExt as _};
use std::path::{Path, PathBuf};

use pohunek_platform::shell_env::{
    fallback_search_path, resolve_executable_in_path_value, ExecutableError, Refusal,
};

/// The macOS `admin` group.
const ADMIN_GID: u32 = 80;

/// The macOS `staff` group, a group other than `admin`.
const STAFF_GID: u32 = 20;

fn directory(parent: &Path, name: &str, gid: u32, mode: u32) -> PathBuf {
    let path = parent.join(name);
    fs::create_dir(&path).expect("create the directory");
    chown(&path, None, Some(gid)).expect("set the group");
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("set the mode");
    path
}

#[test]
fn a_refused_directory_names_the_write_right_that_made_it_unsafe() {
    let temporary = pohunek_test_support::tempdir().expect("create the fixture root");
    let root = fs::canonicalize(temporary.path()).expect("canonicalize the fixture root");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("set the root mode");

    let trusted = directory(&root, "trusted", STAFF_GID, 0o755);
    let admin = directory(&root, "admin", ADMIN_GID, 0o775);
    let staff = directory(&root, "staff", STAFF_GID, 0o775);
    let world = directory(&root, "world", ADMIN_GID, 0o777);
    let entries = [&trusted, &admin, &staff, &world].map(|path| path.to_str().expect("UTF-8"));

    let resolved = fallback_search_path(&entries, None).expect("one directory is trusted");
    assert_eq!(
        resolved.path.entries(),
        [trusted, admin.clone()],
        "the user's admin-group directory is kept"
    );
    let reason_of = |path: &Path| {
        resolved
            .untrusted
            .iter()
            .find(|dropped| Path::new(&dropped.entry) == path)
            .unwrap_or_else(|| panic!("{} is reported: {:?}", path.display(), resolved.untrusted))
            .reason
    };
    assert_eq!(
        reason_of(&staff),
        "writable by a group other than admin (gid 80)"
    );
    assert_eq!(reason_of(&world), "writable by others");
}

/// A candidate in a directory of another group is named with its path and the
/// failed check, whatever else the host has installed: the lookup sees only
/// the fixture's `PATH`.
#[test]
fn a_candidate_in_a_directory_of_another_group_is_named() {
    let temporary = pohunek_test_support::tempdir().expect("create the fixture root");
    let root = fs::canonicalize(temporary.path()).expect("canonicalize the fixture root");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("set the root mode");

    let staff = directory(&root, "staff", STAFF_GID, 0o775);
    let candidate = staff.join("tailscale");
    pohunek_test_support::fs::write_executable(&candidate, "#!/bin/sh\n")
        .expect("write the candidate");

    let error = resolve_executable_in_path_value(
        std::ffi::OsStr::new("tailscale"),
        Some(staff.as_os_str()),
    )
    .expect_err("the candidate is refused");
    assert!(
        matches!(
            &error,
            ExecutableError::Refused {
                candidate: found,
                refusal: Refusal::ChainSharedDirectory { .. } | Refusal::ChainDirectory { .. },
            } if *found == candidate
        ),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains(&candidate.display().to_string()) && message.contains("not trusted"),
        "{message}"
    );
}
