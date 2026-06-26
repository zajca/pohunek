//! Integration test: the single-instance lock refuses a second holder.
//!
//! Milestone-2 requirement: "a second daemon refuses to start". This exercises
//! the advisory flock directly: the first acquire succeeds and, while held, a
//! second acquire on the same path fails with `AlreadyRunning`. After the first
//! is dropped, acquisition succeeds again.

use pohunek_daemon::error::DaemonError;
use pohunek_daemon::lock::InstanceLock;

fn temp_lock(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    p.push(format!(
        "pohunek-test-{tag}-{}-{nanos}.lock",
        std::process::id()
    ));
    p
}

#[test]
fn second_acquire_is_refused_while_held() {
    let path = temp_lock("lock");

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

    let _ = std::fs::remove_file(&path);
}
