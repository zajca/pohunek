//! Fixture roots for tests that build trusted directories or Unix sockets.
//!
//! Pohunek's trusted filesystem layer refuses to traverse a symlinked path
//! component, and a Unix socket path must fit the platform's `sun_path`
//! (104 bytes on Darwin, 108 on Linux, both including the terminating NUL).
//! The standard temporary directory violates both on macOS: it is a per-user
//! directory under `/var/folders/...`, where `/var` is a symlink to
//! `/private/var`, and the prefix alone takes about 50 bytes. Tests that put
//! a trusted root or a socket under a fixture directory therefore take that
//! directory from this crate instead of from [`std::env::temp_dir`].
//!
//! [`temp_root`] names the base directory; [`tempdir`] and [`tempdir_with_prefix`]
//! create a private, uniquely named directory under it that is removed when
//! the returned guard drops.
//!
//! This crate is a development dependency only; production code never picks
//! its paths from here.
//!
//! # Examples
//!
//! ```
//! let fixture = pohunek_test_support::tempdir()?;
//! assert!(fixture.path().starts_with(pohunek_test_support::temp_root()));
//! # Ok::<(), std::io::Error>(())
//! ```

// Rust guideline compliant 2026-09-24

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

/// Resolved, short system temporary directory on macOS.
///
/// `/tmp` is a symlink to this directory on macOS. pohunek's own macOS
/// runtime default (`/private/tmp/pohunek-<uid>`) uses the same base, so
/// fixtures under it face the same trusted-filesystem checks as production.
#[cfg(target_os = "macos")]
const MACOS_TEMP_ROOT: &str = "/private/tmp";

/// Name prefix of the directories [`tempdir`] creates.
///
/// Kept short because every byte of a fixture root counts against the
/// `sun_path` limit of the sockets tests bind beneath it.
const DEFAULT_PREFIX: &str = "ph-";

/// Mode of every fixture directory.
///
/// The trusted filesystem layer rejects group- or world-accessible private
/// roots, so fixtures start owner-only like production state directories.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Returns the base directory test fixtures create their roots under.
///
/// On macOS this is `/private/tmp`: symlink-free and short enough for socket
/// paths nested several levels below it. Elsewhere it is
/// [`std::env::temp_dir`], which already satisfies both constraints.
#[must_use]
pub fn temp_root() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from(MACOS_TEMP_ROOT)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::temp_dir()
    }
}

/// Creates a private fixture directory under [`temp_root`].
///
/// This is the drop-in replacement for [`tempfile::tempdir`]. The directory is
/// owner-only (`0700`) and is removed with its contents when the returned
/// guard drops.
///
/// # Errors
///
/// Returns the I/O error when the directory cannot be created.
pub fn tempdir() -> std::io::Result<tempfile::TempDir> {
    tempdir_with_prefix(DEFAULT_PREFIX)
}

/// Creates a private fixture directory under [`temp_root`] named `prefix` plus
/// a random suffix.
///
/// Use a short `prefix`: it counts against the socket path limit of anything
/// bound beneath the directory.
///
/// # Errors
///
/// Returns the I/O error when the directory cannot be created.
pub fn tempdir_with_prefix(prefix: &str) -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .permissions(std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
        .tempdir_in(temp_root())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{temp_root, tempdir, tempdir_with_prefix};

    /// Darwin's `sun_path` capacity, including the terminating NUL.
    #[cfg(target_os = "macos")]
    const DARWIN_SUN_PATH_CAPACITY: usize = 104;

    /// Longest suffix the daemon tests append below a fixture root for a
    /// socket, e.g. `/runtime/pohunek/workers/<session-id>/control.sock`.
    #[cfg(target_os = "macos")]
    const NESTED_SOCKET_SUFFIX: usize = 64;

    #[test]
    fn temp_root_has_no_symlinked_component() {
        let root = temp_root();
        let canonical = std::fs::canonicalize(&root).expect("canonicalize temp root");
        assert_eq!(root, canonical);
    }

    #[test]
    fn fixture_is_private_and_removed_on_drop() {
        let fixture = tempdir().expect("create fixture");
        let path = fixture.path().to_path_buf();
        assert!(path.starts_with(temp_root()));
        let mode = std::fs::metadata(&path)
            .expect("stat fixture")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, super::PRIVATE_DIR_MODE);
        drop(fixture);
        assert!(!path.exists());
    }

    #[test]
    fn fixture_uses_the_requested_prefix() {
        let fixture = tempdir_with_prefix("ph-prefix-").expect("create fixture");
        let name = fixture
            .path()
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("utf-8 fixture name");
        assert!(name.starts_with("ph-prefix-"), "{name}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_fixture_leaves_room_for_nested_sockets() {
        let fixture = tempdir().expect("create fixture");
        let length = fixture.path().as_os_str().len();
        assert!(
            length + NESTED_SOCKET_SUFFIX < DARWIN_SUN_PATH_CAPACITY,
            "{length}"
        );
    }
}
