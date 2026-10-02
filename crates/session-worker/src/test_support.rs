//! Fixture values shared by the crate's tests.

use std::path::PathBuf;

/// `PATH` of every PTY child a test starts.
///
/// Pinned instead of inherited so a fixture script resolves `sleep`, `yes` and
/// `setsid` from the system directories, never from a developer's toolchain
/// directories. Both directories exist on Linux and macOS.
pub(crate) const CHILD_PATH: &str = "/usr/bin:/bin";

/// Returns the complete environment of a PTY child started with
/// [`crate::EnvBase::Empty`].
///
/// Nothing from the test process leaks into the child: only the pinned
/// `PATH` and a `SHELL` naming the POSIX shell the fixtures run under.
pub(crate) fn child_env() -> Vec<(String, String)> {
    vec![
        ("PATH".to_owned(), CHILD_PATH.to_owned()),
        ("SHELL".to_owned(), "/bin/sh".to_owned()),
    ]
}

/// Returns a private working directory for a PTY child, removed on drop.
pub(crate) fn child_cwd() -> tempfile::TempDir {
    pohunek_test_support::tempdir().expect("child working directory")
}

/// Returns a fixed worker origin for journals built directly in tests.
pub(crate) fn test_origin() -> crate::WorkerOrigin {
    crate::WorkerOrigin {
        executable: PathBuf::from("/usr/libexec/pohunek-sessiond"),
        version: "0.0.0-test".to_owned(),
        generation: "abcd2345".to_owned(),
    }
}
