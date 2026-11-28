//! Integration test: the reaper helper refuses a sweep root that resolves to
//! the owner's home, with the home armed through `POHUNEK_REAPER_HOME`.
//!
//! The helper is spawned with an empty environment (`env_clear`), so `$HOME`
//! is unset there and the home refusal can only fire from
//! [`pohunek_test_support::owner::REAPER_HOME_ENV`]. The scenario runs the real
//! `pohunek-test-reaper` binary end to end: the root's owner lock is held,
//! the helper blocks on it, the lock is released, and the helper resolves its
//! link argument back to the root and sweeps it.

#![cfg(unix)]

use std::fs::OpenOptions;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use pohunek_test_support::owner::REAPER_HOME_ENV;
use pohunek_test_support::wait::poll_until;
use pohunek_test_support::{reaper_binary, tempdir, workers};
use rustix::fs::{flock, FlockOperation};

/// Holds the owner lock of `root`, spawns the real reaper helper on a link to
/// that lock with an empty environment except `REAPER_HOME_ENV=home`, then
/// releases the lock and returns the helper's exit status.
fn reaped_status(root: &Path, home: &Path) -> ExitStatus {
    let lock_path = root.join(pohunek_test_support::owner::OWNER_LOCK_FILE);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .expect("create the owner lock");
    flock(&lock, FlockOperation::LockExclusive).expect("take the owner lock");
    // The helper's argument is a link to the lock outside the root, so tests
    // never attribute the helper itself to the root it reaps.
    let link_dir = tempdir().expect("link directory");
    let link = link_dir.path().join("reaper-link");
    symlink(&lock_path, &link).expect("link to the owner lock");

    let mut reaper = Command::new(reaper_binary())
        .arg(&link)
        .env_clear()
        .env(REAPER_HOME_ENV, home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the reaper helper");
    // Releasing the owner lock is the readiness signal the helper waits on:
    // it wakes and sweeps as soon as the lock is free, whenever it gets there.
    drop(lock);

    poll_until("the reaper helper to exit", || {
        reaper.try_wait().expect("poll the reaper")
    })
}

#[test]
fn the_reaper_refuses_a_sweep_root_that_resolves_to_the_armed_home() {
    // The private home doubles as the sweep root: the helper's link resolves
    // to `<home>/owner.lock`, so the swept root is the home itself.
    let home = tempdir().expect("private home as the sweep root");

    // A sleeper whose working directory is under the root, so the sweep
    // criterion selects it: an accepted root would signal it.
    let mut sleeper = Command::new("sleep")
        .arg("300")
        .current_dir(home.path())
        .spawn()
        .expect("spawn the sleeper under the root");
    let sleeper_pid = sleeper.id();
    let sleeper_start = poll_until("the sleeper to run", || workers::try_start_of(sleeper_pid));

    let status = reaped_status(home.path(), home.path());

    assert_eq!(
        status.code(),
        Some(1),
        "the home sweep root is refused: {status:?}"
    );
    assert_eq!(
        workers::try_start_of(sleeper_pid).as_deref(),
        Some(sleeper_start.as_str()),
        "the refused root's sleeper survives under its start identity"
    );
    sleeper.kill().expect("stop the sleeper");
    let _ = sleeper.wait();
}
