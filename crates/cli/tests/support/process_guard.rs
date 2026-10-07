//! Cleanup of every process a real-agent fixture starts.
//!
//! Shared by the real-agent package tests, which launch an agent through a
//! daemon and must leave nothing behind when an assertion fails.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-06

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pohunek_test_support::wait::poll_until;

/// Terminates every process that belongs to one fixture.
///
/// A process belongs to the fixture when its executable, its working directory
/// or the value of one of its environment variables lies below the fixture's
/// root. That covers the session worker, the agent and any helper the agent
/// detaches, which may outlive its TUI. Dropping the guard reaps them, so an
/// assertion failure or a timed-out wait leaves no process behind; it must be
/// dropped before the directories it names are removed. The scan reads `/proc`, so it finds nothing elsewhere.
pub(crate) struct ProcessGuard {
    root: PathBuf,
}

impl ProcessGuard {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }

    /// Whether `directory`, a `/proc/<pid>` entry, holds a process of the
    /// fixture.
    fn owns(&self, directory: &Path) -> bool {
        let below_root = |link: &str| {
            fs::read_link(directory.join(link)).is_ok_and(|path| path.starts_with(&self.root))
        };
        if below_root("exe") || below_root("cwd") {
            return true;
        }
        fs::read(directory.join("environ")).is_ok_and(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter_map(|entry| {
                    let separator = entry.iter().position(|byte| *byte == b'=')?;
                    Some(&entry[separator + 1..])
                })
                .any(|value| Path::new(OsStr::from_bytes(value)).starts_with(&self.root))
        })
    }

    /// The live processes of the fixture, never the test process itself. A
    /// terminated process that is not yet reaped by its parent has no
    /// executable and does not count.
    pub(crate) fn members(&self) -> Vec<i32> {
        let Ok(processes) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let own = i32::try_from(std::process::id()).ok();
        processes
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
                (Some(pid) != own && self.owns(&entry.path())).then_some(pid)
            })
            .collect()
    }

    /// Kills the fixture's processes until none is left.
    ///
    /// # Panics
    ///
    /// Panics when a process survives until the hang guard of
    /// [`pohunek_test_support::wait::poll_until`] elapses.
    pub(crate) fn reap(&self) {
        poll_until("the fixture processes to end", || {
            let members = self.members();
            for pid in &members {
                // A process that exited since the scan is already gone.
                let _ = kill(Pid::from_raw(*pid), Signal::SIGKILL);
            }
            members.is_empty().then_some(())
        });
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        // A panic escaping a drop that runs during unwinding aborts the run.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.reap())).is_err() {
            eprintln!(
                "fixture processes survived the cleanup below {}",
                self.root.display()
            );
        }
    }
}
