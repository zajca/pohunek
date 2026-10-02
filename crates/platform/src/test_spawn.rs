//! Test helper: run a body while a sibling thread keeps spawning processes.
//!
//! A process spawned while another thread holds a descriptor open gives its
//! child a copy until the child's own `exec`, which is the window the lock and
//! fixture-writing tests need to hit. The spawner thread loops until it is told
//! to stop; the stop signal is set by a drop guard, so it is also set while the
//! body panics. Setting it only on the success path would leave the thread
//! looping, and the scope that joins it would hide the body's failure by never
//! returning.

// Rust guideline compliant 2026-10-02

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use pohunek_test_support::wait::HANG_GUARD;

/// Sets the flag when dropped, including while a panic unwinds.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Runs `body` while a sibling thread spawns `sh` children in a loop, then
/// stops the spawner and returns the body's result.
///
/// `body` starts only after the spawner has completed its first spawn, so even
/// a short body overlaps a spawning thread, and the helper asserts that the
/// spawner completed at least one spawn. The spawner's children start from an
/// empty environment in the private directory `cwd`. A panic in `body` stops
/// the spawner before it propagates. If the spawner fails before its first
/// spawn the wait ends at once, and a spawner that never reports fails the
/// wait at the hang guard.
///
/// # Panics
///
/// Panics when the spawner fails or never completes a spawn.
pub(crate) fn while_a_sibling_spawns<R>(cwd: &Path, body: impl FnOnce() -> R) -> R {
    let stop = AtomicBool::new(false);
    let spawns = AtomicUsize::new(0);
    let result = std::thread::scope(|scope| {
        let (first_spawn, first_spawn_done) = mpsc::sync_channel::<()>(1);
        let (stop, spawns) = (&stop, &spawns);
        scope.spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                Command::new("/bin/sh")
                    .args(["-c", "exit 0"])
                    .env_clear()
                    .current_dir(cwd)
                    .status()
                    .expect("sibling spawn");
                if spawns.fetch_add(1, Ordering::Relaxed) == 0 {
                    // The receiver is gone only when the body already failed.
                    let _ = first_spawn.send(());
                }
            }
        });
        let _stop = StopOnDrop(stop);
        match first_spawn_done.recv_timeout(HANG_GUARD) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the sibling spawner stopped before completing a spawn")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("hang guard elapsed waiting for the sibling spawner's first spawn")
            }
        }
        body()
    });
    assert!(
        spawns.load(Ordering::Relaxed) > 0,
        "no sibling spawn overlapped the body"
    );
    result
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;

    /// Panic message of the body the regression test unwinds with.
    const BODY_FAILURE: &str = "body failed after the spawner started";

    #[test]
    fn the_result_of_the_body_is_returned() {
        let dir = pohunek_test_support::tempdir().expect("fixture directory");
        assert_eq!(while_a_sibling_spawns(dir.path(), || 7), 7);
    }

    #[test]
    fn a_panicking_body_stops_the_spawner_and_returns_its_panic_instead_of_hanging() {
        let dir = pohunek_test_support::tempdir().expect("fixture directory");
        let cwd = dir.path().to_path_buf();
        // The scope runs on its own thread so a hang is observed as a missed
        // deadline instead of blocking this test forever.
        let scope = std::thread::spawn(move || {
            catch_unwind(AssertUnwindSafe(|| {
                while_a_sibling_spawns(&cwd, || panic!("{BODY_FAILURE}"));
            }))
        });
        pohunek_test_support::wait::poll_until("the scope to return after a body panic", || {
            scope.is_finished().then_some(())
        });
        let outcome = scope
            .join()
            .expect("the scope thread does not panic itself");
        let payload = outcome.expect_err("the body's panic reaches the caller");
        assert_eq!(
            payload.downcast_ref::<String>().map(String::as_str),
            Some(BODY_FAILURE)
        );
    }
}
