//! A binary-wide, unwind-safe override of the test process's environment.
//!
//! The process environment is global state shared by every test thread. A test
//! whose subject is code that reads `HOME`, `XDG_*`, `PATH` or another variable
//! in-process has to change that state, and two hazards follow:
//!
//! - **Writer against writer.** Two tests that set the same variable at the
//!   same time clobber each other's values.
//! - **Writer against reader.** A test that only reads a variable through
//!   product code sees a value another test has set. Serializing the writers
//!   among themselves does not help when the reader is in a different module
//!   that holds a different lock, or none.
//!
//! [`ProcessEnv`] closes both with a single `static` lock. A `static` in this
//! crate exists once per final test binary, so every module and every
//! integration-test file linked into that binary contends on the same lock,
//! whatever it is named locally. A lock per module cannot give that guarantee.
//!
//! The runner decides how much the lock matters. `cargo nextest` runs each test
//! in its own process, where no sibling can interfere; `cargo test` runs the
//! tests of a binary as threads of one process, and doctests of a crate run as
//! separate processes but a doctest that mutates the environment still shares
//! it with its own threads. Tests must be correct under the threaded runner too.
//!
//! # Rules for tests
//!
//! - **Prefer injection.** When the code under test can take the value as an
//!   argument, pass it and leave the process environment alone. Reach for this
//!   module only when reading the environment is the behavior being tested.
//! - **Readers hold the guard too.** A test that does not change anything but
//!   whose code reads environment-derived values calls [`ProcessEnv::lock`] and
//!   keeps the returned value alive for the whole test, so no writer can run
//!   concurrently.
//! - **Never hold two at once on one thread.** The lock is not reentrant;
//!   taking it again on the same thread deadlocks. Make every change through
//!   one [`ProcessEnv`] and use [`ProcessEnv::set`] and [`ProcessEnv::remove`]
//!   repeatedly on it.
//! - **Children are immune.** A process started through
//!   [`crate::env::TestEnv::command`] or [`crate::env::TestEnv::tokio_command`]
//!   gets an explicit, scrubbed environment and ignores the changes made here,
//!   so such tests need no guard.
//!
//! # Restoration
//!
//! A [`ProcessEnv`] records the value each touched variable had when it was
//! first touched, including "unset", and restores all of them when it drops,
//! before the lock is released. Restoration also runs while a panic unwinds. A
//! panicking test therefore neither leaves its values behind nor poisons the
//! lock for the tests that follow: the lock ignores poisoning, because the
//! state it protects is restored by the same drop.
//!
//! A test process that is killed or aborts cannot run the drop; there is no
//! cleanup in that case.
//!
//! # Examples
//!
//! ```
//! use pohunek_test_support::process_env::ProcessEnv;
//!
//! let mut env = ProcessEnv::lock();
//! env.set("TEST_SUPPORT_PROCESS_ENV_DOC_EXAMPLE", "value");
//! assert_eq!(
//!     std::env::var("TEST_SUPPORT_PROCESS_ENV_DOC_EXAMPLE").as_deref(),
//!     Ok("value")
//! );
//! drop(env);
//! assert!(std::env::var_os("TEST_SUPPORT_PROCESS_ENV_DOC_EXAMPLE").is_none());
//! ```

// Rust guideline compliant 2026-10-02

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The one lock of this test binary's process environment.
static PROCESS_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Exclusive hold on the process environment with restore-on-drop overrides.
///
/// See the [module documentation](self) for when to use it and why.
#[derive(Debug)]
#[must_use = "the environment is only protected and restored while the value is alive"]
pub struct ProcessEnv {
    /// Variables touched so far with the value each had at its first touch,
    /// in first-touch order.
    saved: Vec<(OsString, Option<OsString>)>,
    /// Declared after `saved` and dropped after [`Drop::drop`] has restored it,
    /// so the lock is released only once the environment is back.
    _guard: MutexGuard<'static, ()>,
}

impl ProcessEnv {
    /// Takes the binary-wide environment lock, blocking until it is free.
    ///
    /// The returned value changes nothing by itself; it is how a test that only
    /// reads environment-derived values keeps writers out. A lock left behind
    /// by a panicking holder is taken over, not reported as an error.
    pub fn lock() -> Self {
        Self {
            saved: Vec::new(),
            _guard: PROCESS_ENV_LOCK
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// Sets `key` to `value` until this value drops.
    ///
    /// Calling it again for the same `key` replaces the value; the original is
    /// still the one restored.
    ///
    /// # Panics
    ///
    /// Panics when `key` is empty or contains `=` or NUL, or when `value`
    /// contains NUL, as [`std::env::set_var`] does.
    pub fn set(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.snapshot(key.as_ref());
        std::env::set_var(key, value);
        self
    }

    /// Removes `key` from the environment until this value drops.
    ///
    /// # Panics
    ///
    /// Panics when `key` is empty or contains `=` or NUL, as
    /// [`std::env::remove_var`] does.
    pub fn remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.snapshot(key.as_ref());
        std::env::remove_var(key);
        self
    }

    /// Records the current value of `key` unless it was touched before.
    fn snapshot(&mut self, key: &OsStr) {
        if self.saved.iter().all(|(saved_key, _)| saved_key != key) {
            self.saved.push((key.to_owned(), std::env::var_os(key)));
        }
    }
}

impl Drop for ProcessEnv {
    fn drop(&mut self) {
        for (key, original) in self.saved.drain(..).rev() {
            match original {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;

    use super::ProcessEnv;
    use crate::wait::{poll_until, HANG_GUARD};

    const KEY_UNSET: &str = "TEST_SUPPORT_PROCESS_ENV_UNSET";
    const KEY_SET: &str = "TEST_SUPPORT_PROCESS_ENV_SET";
    const KEY_NESTED: &str = "TEST_SUPPORT_PROCESS_ENV_NESTED";
    const KEY_PANIC: &str = "TEST_SUPPORT_PROCESS_ENV_PANIC";
    const KEY_CONTEND: &str = "TEST_SUPPORT_PROCESS_ENV_CONTEND";
    const KEY_READ_ONLY: &str = "TEST_SUPPORT_PROCESS_ENV_READ_ONLY";

    fn var(key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    #[test]
    fn variable_unset_before_is_unset_after() {
        let mut env = ProcessEnv::lock();
        env.remove(KEY_UNSET);
        env.set(KEY_UNSET, "temporary");
        assert_eq!(var(KEY_UNSET).as_deref(), Some("temporary"));
        drop(env);
        let _env = ProcessEnv::lock();
        assert_eq!(var(KEY_UNSET), None);
    }

    #[test]
    fn variable_set_before_keeps_its_value_after_set_and_remove() {
        let mut env = ProcessEnv::lock();
        std::env::set_var(KEY_SET, "preexisting");
        env.set(KEY_SET, "changed");
        env.remove(KEY_SET);
        assert_eq!(var(KEY_SET), None);
        drop(env);
        let _env = ProcessEnv::lock();
        assert_eq!(var(KEY_SET).as_deref(), Some("preexisting"));
        std::env::remove_var(KEY_SET);
    }

    #[test]
    fn overriding_the_same_key_twice_restores_the_original() {
        let mut env = ProcessEnv::lock();
        std::env::set_var(KEY_NESTED, "baseline");
        env.set(KEY_NESTED, "first").set(KEY_NESTED, "second");
        assert_eq!(var(KEY_NESTED).as_deref(), Some("second"));
        drop(env);
        let _env = ProcessEnv::lock();
        assert_eq!(var(KEY_NESTED).as_deref(), Some("baseline"));
        std::env::remove_var(KEY_NESTED);
    }

    #[test]
    fn panic_restores_the_environment_and_does_not_poison_the_lock() {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut env = ProcessEnv::lock();
            env.set(KEY_PANIC, "leaked");
            panic!("simulated test failure while holding the override");
        }));
        assert!(result.is_err());
        let env = ProcessEnv::lock();
        assert_eq!(var(KEY_PANIC), None);
        drop(env);
    }

    #[test]
    fn lock_without_changes_leaves_the_environment_untouched() {
        std::env::set_var(KEY_READ_ONLY, "kept");
        let env = ProcessEnv::lock();
        assert_eq!(var(KEY_READ_ONLY).as_deref(), Some("kept"));
        drop(env);
        let _env = ProcessEnv::lock();
        assert_eq!(var(KEY_READ_ONLY).as_deref(), Some("kept"));
        std::env::remove_var(KEY_READ_ONLY);
    }

    #[test]
    fn a_second_thread_blocks_until_the_first_drops_and_sees_the_restored_value() {
        let mut env = ProcessEnv::lock();
        env.set(KEY_CONTEND, "held");
        let released = Arc::new(AtomicBool::new(false));
        let attempting = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let contender = {
            let released = Arc::clone(&released);
            let attempting = Arc::clone(&attempting);
            std::thread::spawn(move || {
                attempting.store(true, Ordering::SeqCst);
                let _env = ProcessEnv::lock();
                sender
                    .send((released.load(Ordering::SeqCst), var(KEY_CONTEND)))
                    .expect("receiver is alive");
            })
        };
        poll_until("the contending thread to start locking", || {
            attempting.load(Ordering::SeqCst).then_some(())
        });
        released.store(true, Ordering::SeqCst);
        drop(env);
        let (saw_release, seen) = receiver
            .recv_timeout(HANG_GUARD)
            .expect("the contending thread acquires the lock after the drop");
        contender.join().expect("contending thread");
        assert!(
            saw_release,
            "the lock was granted before the holder dropped"
        );
        assert_eq!(seen, None, "the holder's value leaked past its drop");
    }
}
