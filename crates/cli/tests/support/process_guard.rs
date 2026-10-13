//! Cleanup of every process a real-agent fixture starts.
//!
//! Shared by the real-agent package tests, which launch an agent through a
//! daemon and must leave nothing behind when an assertion fails.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-13

#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt as _;

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
/// dropped before the directories it names are removed. On Linux, the scan reads
/// `/proc`. On macOS, the host inspector provides process information.
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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

    /// The live processes of the fixture, never the test process itself. A
    /// terminated process that is not yet reaped by its parent has no
    /// executable and does not count.
    ///
    /// Membership is determined by whether the process's executable path or
    /// working directory lies below the fixture root. The platform inspector
    /// exposes only the allowlisted ownership markers of another process's
    /// environment, so membership is decided by executable and working directory.
    /// A detached helper whose executable and working directory both lie outside
    /// the root is not found (#1010).
    #[cfg(target_os = "macos")]
    pub(crate) fn members(&self) -> Vec<i32> {
        self.scan()
            .expect("inspect same-user processes")
            .into_iter()
            .filter_map(|identity| i32::try_from(identity.pid).ok())
            .collect()
    }

    /// Scans for member processes and returns their identities. Returns an error
    /// if the scan itself fails; per-process read failures (executable/cwd unreadable)
    /// are silently ignored as "not a member".
    #[cfg(target_os = "macos")]
    fn scan(
        &self,
    ) -> Result<Vec<pohunek_platform::process::ProcessIdentity>, pohunek_platform::process::Error>
    {
        use pohunek_platform::process::{HostInspector, ProcessInspector as _};

        let inspector = HostInspector::new();
        let processes = inspector.same_user_processes()?;
        let own = std::process::id();

        let canonical_root = fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let below_root = |path: &Path| -> bool {
            fs::canonicalize(path).is_ok_and(|canonical| canonical.starts_with(&canonical_root))
        };

        let mut members = Vec::new();
        for fact in processes {
            if fact.pid == own {
                continue;
            }

            let belongs_by_exe = inspector
                .executable(fact.pid)
                .ok()
                .flatten()
                .is_some_and(|exe| below_root(&exe));

            let belongs_by_cwd = inspector.cwd(fact.pid).is_ok_and(|cwd| below_root(&cwd));

            if belongs_by_exe || belongs_by_cwd {
                members.push(fact.identity());
            }
        }

        Ok(members)
    }

    /// Kills the fixture's processes until none is left.
    ///
    /// # Panics
    ///
    /// Panics when a process survives until the hang guard of
    /// [`pohunek_test_support::wait::poll_until`] elapses.
    #[cfg(target_os = "linux")]
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

    /// Kills the fixture's processes until none is left.
    ///
    /// The signal is sent only to a process whose identity still matches the scan,
    /// to handle PID reuse. A process that exited since the scan is already gone.
    ///
    /// # Panics
    ///
    /// Panics when a process survives until the hang guard of
    /// [`pohunek_test_support::wait::poll_until`] elapses.
    #[cfg(target_os = "macos")]
    pub(crate) fn reap(&self) {
        use pohunek_platform::process::{HostInspector, ProcessInspector as _};

        poll_until("the fixture processes to end", || {
            // An inspection failure keeps polling until the hang guard panics.
            let Ok(identities) = self.scan() else {
                return None;
            };
            let inspector = HostInspector::new();
            for identity in &identities {
                // Re-check the process identity before sending the signal.
                if let Ok(Some(current)) = inspector.identity(identity.pid) {
                    if current == *identity {
                        // Convert u32 pid to i32 for nix; skip if it doesn't fit.
                        if let Ok(pid_i32) = i32::try_from(identity.pid) {
                            let _ = kill(Pid::from_raw(pid_i32), Signal::SIGKILL);
                        }
                    }
                }
            }
            identities.is_empty().then_some(())
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

/// A spawned child that is killed and waited for when dropped, also while a
/// panic unwinds.
///
/// For helpers [`ProcessGuard`] cannot find: a process that hides its
/// executable, working directory and environment (non-dumpable) is invisible to
/// process inspection, and dropping a bare [`std::process::Child`] leaves the
/// process running. Create it right after the spawn, before any fallible step.
pub(crate) struct ChildGuard {
    child: std::process::Child,
}

impl ChildGuard {
    pub(crate) fn new(child: std::process::Child) -> Self {
        Self { child }
    }

    /// The child's process id.
    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // The child may have exited already; a failed kill or wait leaves
        // nothing further to do.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
