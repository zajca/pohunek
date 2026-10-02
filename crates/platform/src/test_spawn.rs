//! Test helper: run a body while a sibling thread keeps spawning processes.
//!
//! A process spawned while another thread holds a descriptor open gives its
//! child a copy until the child's own `exec`, which is the window the lock and
//! fixture-writing tests need to hit. The spawner thread loops until it is told
//! to stop; the stop signal is set by a drop guard, so it is also set while the
//! body panics. Setting it only on the success path would leave the thread
//! looping, and the scope that joins it would hide the body's failure by never
//! returning.
//!
//! Tests built on this helper are stress tests: the counters show that the
//! spawner ran while the body repeated its operation, but which operation a
//! given fork lands in is up to the scheduler. The deterministic regression
//! tests for inherited lock descriptors hand the copy over explicitly
//! (`filesystem::tests::dropping_a_lock_releases_it_while_a_duplicate_descriptor_stays_open`
//! and `..._while_a_child_process_holds_an_inherited_copy`).

// Rust guideline compliant 2026-10-02

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, PoisonError};
use std::time::Instant;

use pohunek_test_support::wait::HANG_GUARD;

/// Sets the flag when dropped, including while a panic unwinds.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Error of the spawner's failed spawn, shared with the body's handle.
///
/// The spawner stops after recording it, so the waiting methods of [`Sibling`]
/// fail with the original cause instead of waiting for spawns that never come.
#[derive(Default)]
struct SpawnerFailure(Mutex<Option<String>>);

impl SpawnerFailure {
    fn record(&self, message: String) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(message);
    }

    fn message(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Spawns that must complete during a body that repeats its operation through
/// [`Sibling::repeat_while_spawning`].
///
/// A few completed spawns show the spawner kept running while the body
/// repeated its operation, rather than one spawn that may have completed
/// between two cycles.
const MIN_OVERLAPPING_SPAWNS: usize = 3;

/// Handle given to the body of [`while_a_sibling_spawns`].
pub(crate) struct Sibling<'a> {
    spawns: &'a AtomicUsize,
    failure: &'a SpawnerFailure,
    at_start: usize,
}

impl Sibling<'_> {
    /// Returns how many spawns completed since the body started.
    pub(crate) fn spawns_since_start(&self) -> usize {
        self.spawns.load(Ordering::Relaxed) - self.at_start
    }

    /// Fails at once with the spawner's error when its spawn failed.
    #[track_caller]
    fn assert_spawner_running(&self) {
        if let Some(message) = self.failure.message() {
            panic!("the sibling spawner failed: {message}");
        }
    }

    /// Runs `cycle` with the cycle number until at least `min_cycles` cycles ran
    /// and the spawner completed [`MIN_OVERLAPPING_SPAWNS`] spawns since the
    /// body started, so the operation keeps repeating while the spawner runs.
    /// Whether a fork lands while the operation holds its descriptor is up to
    /// the scheduler; repetition makes it likely, not certain.
    ///
    /// # Panics
    ///
    /// Panics with the spawner's error as soon as its spawn failed, and naming
    /// the spawner when it completes too few spawns within the hang guard.
    pub(crate) fn repeat_while_spawning(&self, min_cycles: usize, mut cycle: impl FnMut(usize)) {
        let started = Instant::now();
        let mut number = 0;
        while number < min_cycles || self.spawns_since_start() < MIN_OVERLAPPING_SPAWNS {
            self.assert_spawner_running();
            assert!(
                started.elapsed() < HANG_GUARD,
                "hang guard elapsed: the sibling spawner completed {} of {MIN_OVERLAPPING_SPAWNS} spawns",
                self.spawns_since_start()
            );
            cycle(number);
            number += 1;
        }
    }

    /// Blocks until the spawner completes one more spawn than it had when this
    /// was called, so a short body can stay active across a spawn.
    ///
    /// The wait is bounded by the hang guard and fails with a message naming the
    /// spawn. It fails at once with the spawner's error when its spawn failed.
    pub(crate) fn wait_for_next_spawn(&self) {
        let target = self.spawns.load(Ordering::Relaxed) + 1;
        pohunek_test_support::wait::poll_until("the next sibling spawn", || {
            self.assert_spawner_running();
            (self.spawns.load(Ordering::Relaxed) >= target).then_some(())
        });
    }
}

/// Runs `body` while a sibling thread spawns `sh` children in a loop, then
/// stops the spawner and returns the body's result.
///
/// The spawner's children start from an empty environment in the private
/// directory `cwd`. `body` starts after the spawner completed its first spawn.
/// When `body` returns normally, the helper asserts that the spawner completed
/// at least one more spawn while `body` ran; a body that repeats an operation
/// uses [`Sibling::repeat_while_spawning`] and a body too short for either calls
/// [`Sibling::wait_for_next_spawn`]. A panic in `body` stops the spawner and
/// propagates without that assertion.
///
/// # Panics
///
/// Panics with the spawner's error when a spawn fails: before the first spawn
/// the wait ends at once, and afterwards the waiting methods of [`Sibling`] end
/// at once as does the check after `body` returns. Also panics when the spawner
/// never completes a spawn (the wait ends at the hang guard) or when no spawn
/// completed while `body` ran.
pub(crate) fn while_a_sibling_spawns<R>(cwd: &Path, body: impl FnOnce(&Sibling<'_>) -> R) -> R {
    let stop = AtomicBool::new(false);
    let spawns = AtomicUsize::new(0);
    let failure = SpawnerFailure::default();
    std::thread::scope(|scope| {
        let (first_spawn, first_spawn_done) = mpsc::sync_channel::<()>(1);
        let (stop, spawns, failure) = (&stop, &spawns, &failure);
        scope.spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let spawned = Command::new("/bin/sh")
                    .args(["-c", "exit 0"])
                    .env_clear()
                    .current_dir(cwd)
                    .status();
                if let Err(error) = spawned {
                    failure.record(format!("sibling spawn failed: {error}"));
                    return;
                }
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
                let cause = failure.message().unwrap_or_default();
                panic!("the sibling spawner stopped before completing a spawn: {cause}")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("hang guard elapsed waiting for the sibling spawner's first spawn")
            }
        }
        let at_start = spawns.load(Ordering::Relaxed);
        let sibling = Sibling {
            spawns,
            failure,
            at_start,
        };
        let result = body(&sibling);
        sibling.assert_spawner_running();
        assert!(
            spawns.load(Ordering::Relaxed) > at_start,
            "no sibling spawn completed while the body ran"
        );
        result
    })
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use std::time::Duration;

    use super::*;

    /// Longest a body may take to see a spawner failure.
    ///
    /// Far below [`HANG_GUARD`], so a failure that is only noticed at the hang
    /// guard is told apart from one reported at once, and generous enough for a
    /// loaded runner to schedule one failing spawn.
    const FAILURE_SURFACING_CEILING: Duration = Duration::from_secs(30);

    /// Panic message of the body the regression test unwinds with.
    const BODY_FAILURE: &str = "body failed after the spawner started";

    #[test]
    fn the_result_of_the_body_is_returned() {
        let dir = pohunek_test_support::tempdir().expect("fixture directory");
        assert_eq!(
            while_a_sibling_spawns(dir.path(), |sibling| {
                sibling.wait_for_next_spawn();
                7
            }),
            7
        );
    }

    #[test]
    fn a_panicking_body_stops_the_spawner_and_returns_its_panic_instead_of_hanging() {
        let dir = pohunek_test_support::tempdir().expect("fixture directory");
        let cwd = dir.path().to_path_buf();
        // The scope runs on its own thread so a hang is observed as a missed
        // deadline instead of blocking this test forever.
        let scope = std::thread::spawn(move || {
            catch_unwind(AssertUnwindSafe(|| {
                while_a_sibling_spawns(&cwd, |_sibling| panic!("{BODY_FAILURE}"));
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

    /// Removes the spawner's directory after the handshake, runs `wait` as the
    /// body, and returns the panic message and how long the body took to fail.
    fn failure_message_of_a_spawner_that_fails_after_its_first_spawn(
        wait: impl FnOnce(&Sibling<'_>),
    ) -> (String, Duration) {
        let dir = pohunek_test_support::tempdir().expect("fixture directory");
        let cwd = dir.path().to_path_buf();
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            while_a_sibling_spawns(&cwd, |sibling| {
                // The handshake is complete, so every spawn from here on that
                // starts after the removal fails.
                std::fs::remove_dir(&cwd).expect("remove the spawner's directory");
                wait(sibling);
            });
        }));
        let elapsed = started.elapsed();
        let payload = outcome.expect_err("the spawner's failure reaches the caller");
        let message = payload
            .downcast_ref::<String>()
            .expect("the failure message is formatted")
            .clone();
        (message, elapsed)
    }

    /// Error text of a spawn in a directory that does not exist.
    fn missing_directory_spawn_error() -> String {
        let dir = pohunek_test_support::tempdir().expect("fixture directory");
        let missing = dir.path().join("missing");
        let error = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .env_clear()
            .current_dir(&missing)
            .status()
            .expect_err("a missing directory cannot be entered");
        format!("the sibling spawner failed: sibling spawn failed: {error}")
    }

    #[test]
    fn a_spawner_failing_after_its_first_spawn_fails_a_repeating_body_at_once() {
        // The cycle count is unreachable, so only the failure ends the loop.
        let (message, elapsed) =
            failure_message_of_a_spawner_that_fails_after_its_first_spawn(|sibling| {
                sibling.repeat_while_spawning(usize::MAX, |_| {});
            });
        assert_eq!(message, missing_directory_spawn_error());
        assert!(
            elapsed < FAILURE_SURFACING_CEILING,
            "the failure took {elapsed:?}"
        );
    }

    #[test]
    fn a_spawner_failing_after_its_first_spawn_fails_a_waiting_body_at_once() {
        // A spawn in flight during the removal may still complete and end one
        // wait, so the body waits until the failure ends it.
        let (message, elapsed) =
            failure_message_of_a_spawner_that_fails_after_its_first_spawn(|sibling| loop {
                sibling.wait_for_next_spawn();
            });
        assert_eq!(message, missing_directory_spawn_error());
        assert!(
            elapsed < FAILURE_SURFACING_CEILING,
            "the failure took {elapsed:?}"
        );
    }
}
