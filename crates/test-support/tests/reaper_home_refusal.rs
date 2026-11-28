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
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use pohunek_test_support::owner::{OwnerReaper, REAPER_HOME_ENV};
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

/// Marker argument that selects the child scenario run of this binary, so a
/// plain `--ignored` sweep of this binary returns at once.
const SCENARIO_MARKER: &str = "arm-home-scenario";

/// Child scenario: `HOME` is the root, so [`OwnerReaper::arm`] hands the
/// helper that home through `POHUNEK_REAPER_HOME`; the child then exits
/// without unwinding, the kernel releases the owner lock, and the helper
/// sweeps (or refuses) the root.
#[test]
#[ignore = "child process of the armed-reaper scenario"]
fn arm_home_scenario() {
    let arguments: Vec<String> = std::env::args().collect();
    if !arguments.iter().any(|argument| argument == SCENARIO_MARKER) {
        return;
    }
    let root = PathBuf::from(std::env::var_os("HOME").expect("HOME is the scenario root"));
    let reaper = OwnerReaper::arm(&root).expect("arm the reaper on the home root");
    // Exiting must not run the drop that kills the helper before it can sweep.
    std::mem::forget(reaper);
    std::process::exit(0);
}

/// End-to-end proof through [`OwnerReaper::arm`]: a child whose `HOME` is the
/// root arms the reaper on that root and exits without unwinding; once the
/// armed helper has finished (its link is gone), the sleeper under the root
/// survives, because the sweep root resolves to the armed home and is refused.
#[test]
fn an_armed_reaper_refuses_the_home_root_when_its_owner_dies() {
    // The scenario root doubles as the child's HOME.
    let root = tempdir().expect("scenario root doubling as HOME");
    let root = std::fs::canonicalize(root.path()).expect("canonical scenario root");

    // A sleeper under the root: the sweep criterion selects it, so an
    // accepted root would kill it.
    let mut sleeper = Command::new("sleep")
        .arg("300")
        .current_dir(&root)
        .spawn()
        .expect("spawn the sleeper under the root");
    let sleeper_pid = sleeper.id();
    let sleeper_start = poll_until("the sleeper to run", || workers::try_start_of(sleeper_pid));

    // The scenario child runs with HOME=root, arms the reaper on it, and
    // exits without unwinding.
    let exe = std::env::current_exe().expect("path of the running test binary");
    let [exact, test] =
        pohunek_test_support::exact_test_filter(module_path!(), "arm_home_scenario");
    let mut child = Command::new(exe)
        .arg(exact)
        .arg(test)
        .arg("--ignored")
        .arg(SCENARIO_MARKER)
        .env("HOME", &root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the scenario child");
    let child_pid = child.id();
    let status = child.wait().expect("wait for the scenario child");
    assert_eq!(
        status.code(),
        Some(0),
        "the child exited without unwinding: {status:?}"
    );

    // The armed helper finishes when it has swept (or refused) the root and
    // removed its link: `/tmp/pohunek-test-locks-<uid>/reaper-<pid>-<name>.lock`.
    let uid = rustix::process::geteuid().as_raw();
    let root_name = root
        .file_name()
        .expect("the root has a directory name")
        .to_string_lossy()
        .into_owned();
    let link = Path::new("/tmp")
        .join(format!("pohunek-test-locks-{uid}"))
        .join(format!("reaper-{child_pid}-{root_name}.lock"));
    poll_until("the armed helper to finish and remove its link", || {
        std::fs::symlink_metadata(&link).is_err().then_some(())
    });

    // The home root was refused, so the sleeper under it survives.
    assert!(
        sleeper.try_wait().expect("poll the sleeper").is_none(),
        "the sleeper under the refused home root survives"
    );
    assert_eq!(
        workers::try_start_of(sleeper_pid).as_deref(),
        Some(sleeper_start.as_str()),
        "the survivor keeps its start identity"
    );
    sleeper.kill().expect("stop the sleeper");
    let _ = sleeper.wait();
}
