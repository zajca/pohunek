//! The test-owner reaper: the helper `pohunek-test-reaper <link>` that
//! `TestEnv` arms for its fixture root.
//!
//! The argument is a per-user link to `<root>/owner.lock`, not the root path:
//! tests attribute a process to a fixture root by scanning command lines and
//! environments for the root path, and the helper must not be mistaken for a
//! process of the root it reaps. The helper opens the link (which resolves to
//! the lock file) and blocks on its `flock`. The owner holds that lock for
//! its whole life, and the kernel releases it on any death of the owner
//! (`SIGKILL`, `SIGTERM`, `abort`, `std::process::exit`, nextest termination),
//! so the blocking `flock` returns exactly when the test process is gone
//! without unwinding. The helper then resolves the link back to the root,
//! sweeps it with [`pohunek_test_support::owner::sweep_root`], removes the
//! link, and exits.
//!
//! A missing link means no reaper was armed (or the owner already removed it
//! on a normal end), so there is nothing to sweep and the helper exits at
//! once. The helper writes nothing on any stream: it is spawned with all three
//! standard streams null and an empty environment, and a reaper must never
//! hold the test runner's pipes.

// Rust guideline compliant 2026-11-27

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::ExitCode;

use pohunek_test_support::owner::sweep_root;
use rustix::fs::{flock, FlockOperation};
use rustix::io::Errno;

fn main() -> ExitCode {
    let Some(argument) = std::env::args().nth(1) else {
        return ExitCode::FAILURE;
    };
    let link = PathBuf::from(argument);
    let Ok(lock) = OpenOptions::new().read(true).open(&link) else {
        return ExitCode::SUCCESS;
    };
    // Block until the kernel releases the owner's lock, which happens on any
    // death of the owner process; `EINTR` is retried.
    loop {
        match flock(&lock, FlockOperation::LockExclusive) {
            Ok(()) => break,
            Err(Errno::INTR) => {}
            Err(_) => return ExitCode::FAILURE,
        }
    }
    // The owner is gone. Resolve the link back to `<root>/owner.lock` and
    // sweep the root; a link that no longer resolves means the root is gone.
    let code = match std::fs::canonicalize(&link) {
        Ok(lock_path) => match lock_path.parent() {
            Some(root) => match sweep_root(root) {
                Ok(_) => ExitCode::SUCCESS,
                Err(_) => ExitCode::FAILURE,
            },
            None => ExitCode::SUCCESS,
        },
        Err(_) => ExitCode::SUCCESS,
    };
    let _ = std::fs::remove_file(&link);
    code
}
