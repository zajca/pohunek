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
//! The lock alone cannot keep a change from code that reads the environment
//! without taking it, such as a fixture whose children inherit the process
//! environment. `cargo nextest` runs each test in its own process, where no
//! sibling can see a change; `cargo test` runs the tests of a binary as threads
//! of one process. A change therefore happens only in a process that runs one
//! test: [`ProcessEnv::set`] and [`ProcessEnv::remove`] panic anywhere else,
//! and [`in_own_process`] gives a test such a process under either runner.
//!
//! # Rules for tests
//!
//! - **Prefer injection.** When the code under test can take the value as an
//!   argument, pass it and leave the process environment alone. Reach for this
//!   module only when reading the environment is the behavior being tested.
//! - **Writers run in their own process.** A test that changes the
//!   environment starts with `if !in_own_process() { return; }`.
//! - **Readers hold the guard too.** A test that does not change anything but
//!   whose code reads environment-derived values calls [`ProcessEnv::lock`] and
//!   keeps the returned value alive for the whole test, so no writer can run
//!   concurrently.
//! - **Never hold two at once on one thread.** The lock is not reentrant;
//!   taking it again on the same thread deadlocks. Make every change through
//!   one [`ProcessEnv`] and use [`ProcessEnv::set`] and [`ProcessEnv::remove`]
//!   repeatedly on it.
//! - **Spawn by absolute path.** `Command::new("git")` searches the *parent's*
//!   `PATH` when it spawns, which is a read of the process environment. Build
//!   such a command with [`command`], which resolves the program under the lock
//!   and hands back a command holding the absolute path.
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
//! ```no_run
//! use pohunek_test_support::process_env::{in_own_process, ProcessEnv};
//!
//! # fn a_test() {
//! if !in_own_process() {
//!     return;
//! }
//! let mut env = ProcessEnv::lock();
//! env.set("TEST_SUPPORT_PROCESS_ENV_DOC_EXAMPLE", "value");
//! assert_eq!(
//!     std::env::var("TEST_SUPPORT_PROCESS_ENV_DOC_EXAMPLE").as_deref(),
//!     Ok("value")
//! );
//! drop(env);
//! assert!(std::env::var_os("TEST_SUPPORT_PROCESS_ENV_DOC_EXAMPLE").is_none());
//! # }
//! ```

// Rust guideline compliant 2026-10-14

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The one lock of this test binary's process environment.
static PROCESS_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Set in the child process that [`in_own_process`] runs a test in.
const OWN_PROCESS_CHILD_VAR: &str = "POHUNEK_TEST_OWN_PROCESS_CHILD";

/// Set by `cargo nextest` to how it runs tests.
const NEXTEST_EXECUTION_MODE_VAR: &str = "NEXTEST_EXECUTION_MODE";

/// The [`NEXTEST_EXECUTION_MODE_VAR`] value of a run with one process per test.
const NEXTEST_PROCESS_PER_TEST: &str = "process-per-test";

/// Whether this process runs only the current test: the child of
/// [`in_own_process`], or a `cargo nextest` process per test.
#[must_use]
pub fn runs_in_own_process() -> bool {
    std::env::var_os(OWN_PROCESS_CHILD_VAR).is_some()
        || std::env::var_os(NEXTEST_EXECUTION_MODE_VAR)
            .is_some_and(|mode| mode == NEXTEST_PROCESS_PER_TEST)
}

/// Gives the calling test a process of its own, for a test that changes the
/// process environment or working directory.
///
/// Returns `true` where the test body runs: in a process that runs only this
/// test ([`runs_in_own_process`]). Anywhere else it runs the calling test in a
/// child process of this test binary with `--exact` (and `--include-ignored`,
/// so an ignored test selected with `--ignored` runs too), fails when the child
/// fails or does not run exactly that test, and returns `false`; the caller
/// then returns without running its body. Call it first:
///
/// ```no_run
/// # use pohunek_test_support::process_env::{in_own_process, ProcessEnv};
/// # fn a_test() {
/// if !in_own_process() {
///     return;
/// }
/// let mut env = ProcessEnv::lock();
/// env.set("PATH", "/fixture/bin");
/// # }
/// ```
///
/// # Panics
///
/// Panics when the calling thread has no name (the test harness names each
/// test's thread after the test), when the child cannot be run, or when it
/// does not pass.
#[must_use]
pub fn in_own_process() -> bool {
    if runs_in_own_process() {
        return true;
    }
    let name = std::thread::current()
        .name()
        .expect("the test harness names each test's thread after the test")
        .to_owned();
    // The child writes to files, not pipes: a process the test leaves behind
    // keeps an inherited pipe open, and the verdict waits only for the child
    // itself. The files are read through descriptors of their own, so such a
    // process can neither move the read position nor be moved by it.
    let output_dir = crate::tempdir().expect("child output directory");
    let stdout_path = output_dir.path().join("stdout");
    let stderr_path = output_dir.path().join("stderr");
    let create = |path: &std::path::Path| std::fs::File::create(path).expect("child output file");
    let status = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            &name,
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(OWN_PROCESS_CHILD_VAR, "1")
        .stdin(std::process::Stdio::null())
        .stdout(create(&stdout_path))
        .stderr(create(&stderr_path))
        .status()
        .expect("run the test in its own process");
    let read = |path: &std::path::Path| {
        String::from_utf8_lossy(&std::fs::read(path).expect("read the child output")).into_owned()
    };
    let (stdout, stderr) = (read(&stdout_path), read(&stderr_path));
    assert!(
        status.success() && stdout.contains("1 passed"),
        "{name} failed in its own process: {stdout}{stderr}"
    );
    false
}

/// Panics unless the calling test runs in its own process.
fn assert_own_process() {
    assert!(
        runs_in_own_process(),
        "a test changes the process environment only in its own process: start it with \
         `if !pohunek_test_support::process_env::in_own_process() {{ return; }}`"
    );
}

/// Changes the process working directory to `dir`.
///
/// The working directory is process-wide and every child that is spawned
/// without an explicit `current_dir` inherits it, so a change is allowed only
/// in a test that runs in its own process (see [`in_own_process`]). Nothing
/// restores it.
///
/// # Panics
///
/// Panics when the test does not run in its own process, and when `dir` cannot
/// be entered.
pub fn set_current_dir(dir: &std::path::Path) {
    assert_own_process();
    std::env::set_current_dir(dir).expect("enter the fixture directory");
}

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
    /// Panics outside a test's own process ([`in_own_process`]), when `key` is
    /// empty or contains `=` or NUL, or when `value` contains NUL, as
    /// [`std::env::set_var`] does.
    pub fn set(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        assert_own_process();
        self.snapshot(key.as_ref());
        std::env::set_var(key, value);
        self
    }

    /// Removes `key` from the environment until this value drops.
    ///
    /// # Panics
    ///
    /// Panics outside a test's own process ([`in_own_process`]), or when `key`
    /// is empty or contains `=` or NUL, as [`std::env::remove_var`] does.
    pub fn remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        assert_own_process();
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

/// Builds a [`Command`] for `program`, resolved against the current `PATH`
/// while holding the environment lock.
///
/// The returned command carries an absolute path and pins the captured `PATH`
/// on the child (removed when it was unset), so neither the spawn nor the
/// child's own lookups observe a `PATH` another test sets between this call
/// and the spawn. A `program` that
/// is not a bare name, or that is not found, is passed through unchanged and
/// fails or resolves at spawn like [`Command::new`].
///
/// Do not call it while holding a [`ProcessEnv`] on the same thread; the lock
/// is not reentrant.
#[must_use]
pub fn command(program: &str) -> Command {
    let path = {
        let _env = ProcessEnv::lock();
        std::env::var_os("PATH")
    };
    command_with_path(program, path)
}

/// [`command`] for a `PATH` already captured under the lock (`None` when it
/// was unset).
fn command_with_path(program: &str, path: Option<std::ffi::OsString>) -> Command {
    let resolved = path.as_deref().and_then(|path| resolve_in(program, path));
    let mut command = Command::new(resolved.unwrap_or_else(|| PathBuf::from(program)));
    match path {
        Some(path) => command.env("PATH", path),
        None => command.env_remove("PATH"),
    };
    command
}

/// The first executable regular file named `program` in an absolute entry of
/// `path`, or `None` for an empty name, a name containing `/`, or no match.
fn resolve_in(program: &str, path: &OsStr) -> Option<PathBuf> {
    if program.is_empty() || program.contains('/') {
        return None;
    }
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;

    use std::os::unix::ffi::OsStrExt as _;

    use super::ProcessEnv;
    use crate::wait::{poll_until, HANG_GUARD};

    const KEY_UNSET: &str = "TEST_SUPPORT_PROCESS_ENV_UNSET";
    const KEY_SET: &str = "TEST_SUPPORT_PROCESS_ENV_SET";
    const KEY_PANIC: &str = "TEST_SUPPORT_PROCESS_ENV_PANIC";
    const KEY_CONTEND: &str = "TEST_SUPPORT_PROCESS_ENV_CONTEND";
    const KEY_GUARD: &str = "TEST_SUPPORT_PROCESS_ENV_GUARD";

    /// The test path of [`write_probe`] in this crate's test binary.
    const WRITE_PROBE: &str = "process_env::tests::write_probe";

    /// Set only by [`a_write_runs_only_in_a_tests_own_process`] for the probe
    /// it runs, so a run of the ignored tests leaves the probe idle.
    const WRITE_PROBE_VAR: &str = "TEST_SUPPORT_PROCESS_ENV_WRITE_PROBE";

    fn var(key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    /// Changes the environment wherever [`a_write_runs_only_in_a_tests_own_process`]
    /// runs it, in a process shared with other tests; anywhere else it does
    /// nothing.
    #[test]
    #[ignore = "run by a_write_runs_only_in_a_tests_own_process"]
    fn write_probe() {
        if std::env::var_os(WRITE_PROBE_VAR).is_none() {
            return;
        }
        let mut env = ProcessEnv::lock();
        env.set(KEY_GUARD, "probe");
    }

    #[test]
    fn a_write_runs_only_in_a_tests_own_process() {
        // Without the own-process marker or nextest's process per test, the
        // probe stands for a test that shares its process with others.
        let shared = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                WRITE_PROBE,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(WRITE_PROBE_VAR, "1")
            .env_remove(super::OWN_PROCESS_CHILD_VAR)
            .env_remove(super::NEXTEST_EXECUTION_MODE_VAR)
            .output()
            .expect("run the write probe");
        let shared_output = format!(
            "{}{}",
            String::from_utf8_lossy(&shared.stdout),
            String::from_utf8_lossy(&shared.stderr)
        );
        assert!(
            !shared.status.success()
                && shared_output
                    .contains("changes the process environment only in its own process"),
            "a write in a shared process must panic: {shared_output}"
        );

        if !super::in_own_process() {
            return;
        }
        let mut env = ProcessEnv::lock();
        env.set(KEY_GUARD, "own process");
        assert_eq!(var(KEY_GUARD).as_deref(), Some("own process"));
        drop(env);
        let _env = ProcessEnv::lock();
        assert_eq!(var(KEY_GUARD), None);
    }

    #[test]
    fn variable_unset_before_is_unset_after() {
        if !super::in_own_process() {
            return;
        }
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
        if !super::in_own_process() {
            return;
        }
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
    fn panic_restores_the_environment_and_does_not_poison_the_lock() {
        if !super::in_own_process() {
            return;
        }
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
    fn a_second_thread_blocks_until_the_first_drops_and_sees_the_restored_value() {
        if !super::in_own_process() {
            return;
        }
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

    #[test]
    fn resolve_in_takes_the_first_executable_in_an_absolute_entry() {
        let first = crate::tempdir().expect("first dir");
        let second = crate::tempdir().expect("second dir");
        crate::fs::write_file(first.path().join("tool"), "not executable").expect("plain file");
        crate::fs::write_executable(second.path().join("tool"), "#!/bin/sh\n").expect("tool");
        let path = std::env::join_paths([first.path(), second.path()]).expect("join");
        assert_eq!(
            super::resolve_in("tool", &path),
            Some(second.path().join("tool"))
        );
    }

    #[test]
    fn resolve_in_skips_relative_entries_and_refuses_paths_and_empty_names() {
        let dir = crate::tempdir().expect("fixture dir");
        crate::fs::write_executable(dir.path().join("tool"), "#!/bin/sh\n").expect("tool");
        let path = std::env::join_paths([
            std::path::PathBuf::from("relative"),
            dir.path().to_path_buf(),
        ])
        .expect("join");
        assert_eq!(
            super::resolve_in("tool", &path),
            Some(dir.path().join("tool"))
        );
        assert_eq!(super::resolve_in("sub/tool", &path), None);
        assert_eq!(super::resolve_in("", &path), None);
        assert_eq!(super::resolve_in("missing", &path), None);
    }

    #[test]
    fn command_hands_back_an_absolute_program_for_a_name_on_path() {
        let command = super::command("sh");
        assert!(std::path::Path::new(command.get_program()).is_absolute());
    }

    #[test]
    fn command_keeps_the_captured_path_when_another_test_changes_it_before_the_spawn() {
        if !super::in_own_process() {
            return;
        }
        let captured = {
            let _env = ProcessEnv::lock();
            // Readers hold the lock so a concurrent writer's override is never captured.
            std::env::var_os("PATH").expect("the test process has a PATH")
        };
        let mut command = super::command("sh");
        command.args(["-c", "printf %s \"$PATH\""]);
        let output = {
            let mut env = ProcessEnv::lock();
            env.set("PATH", "/test-support-overridden-path");
            command.output().expect("run sh")
        };
        assert!(output.status.success());
        assert_eq!(std::ffi::OsStr::from_bytes(&output.stdout), captured);
    }

    #[test]
    fn a_command_built_from_an_unset_path_keeps_it_unset_when_another_test_sets_it() {
        if !super::in_own_process() {
            return;
        }
        let mut command = super::command_with_path("/bin/sh", None);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "PATH" && value.is_none()),
            "the command removes PATH from the child environment"
        );
        // `sh` substitutes its own default PATH when none is inherited, so the
        // check is that the override never reaches the child.
        command.args(["-c", "printf %s \"$PATH\""]);
        let output = {
            let mut env = ProcessEnv::lock();
            env.set("PATH", "/test-support-overridden-path");
            command.output().expect("run sh")
        };
        assert!(output.status.success());
        assert_ne!(output.stdout, b"/test-support-overridden-path");
    }
}
