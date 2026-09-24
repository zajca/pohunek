//! Fixture locations shared by the crate's tests.

use std::path::PathBuf;

/// Returns the directory test fixtures create their roots under.
///
/// Darwin's per-user temporary directory lies behind the `/var` symlink, which
/// the trusted filesystem refuses to traverse, and is long enough that a Unix
/// socket beneath it exceeds the 104-byte `sun_path` limit. `/private/tmp` is
/// the resolved, short system temporary root, the base pohunek's own macOS
/// runtime default uses. Elsewhere the standard temporary directory already
/// satisfies both constraints.
pub(crate) fn temp_root() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/private/tmp")
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::temp_dir()
    }
}

/// Returns a fixed worker origin for journals built directly in tests.
pub(crate) fn test_origin() -> crate::WorkerOrigin {
    crate::WorkerOrigin {
        executable: PathBuf::from("/usr/libexec/pohunek-sessiond"),
        version: "0.0.0-test".to_owned(),
        generation: "abcd2345".to_owned(),
    }
}
