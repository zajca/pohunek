//! Integration test: the single-instance lock refuses a second holder.
//!
//! Milestone-2 requirement: "a second daemon refuses to start". This exercises
//! the advisory flock directly: the first acquire succeeds and, while held, a
//! second acquire on the same path fails with `AlreadyRunning`. After the first
//! is dropped, acquisition succeeds again.

use pohunek_daemon::error::DaemonError;
use pohunek_daemon::lock::InstanceLock;

/// An owner-private directory and the lock path inside it; the directory is
/// removed when the guard drops.
fn temp_lock() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = pohunek_test_support::tempdir_with_prefix("ph-lock-")
        .expect("create owner-private lock directory");
    let path = dir.path().join("daemon.lock");
    (dir, path)
}

#[test]
fn second_acquire_is_refused_while_held() {
    let (_dir, path) = temp_lock();

    let first = InstanceLock::acquire(&path).expect("first lock acquires");
    assert_eq!(first.path(), path.as_path());

    let second = InstanceLock::acquire(&path);
    assert!(
        matches!(second, Err(DaemonError::AlreadyRunning { .. })),
        "second acquire must be refused while the first is held, got: {second:?}"
    );

    // Releasing the first allows a fresh acquire.
    drop(first);
    let third = InstanceLock::acquire(&path).expect("acquire succeeds after release");
    drop(third);
}
