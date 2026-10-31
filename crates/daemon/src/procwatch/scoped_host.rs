//! Host process table view that confines a cleanup scenario to its own processes,
//! and the reaper that ends them when the scenario's directory goes away.

// Rust guideline compliant 2026-10-13

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[cfg(not(target_os = "linux"))]
use nix::sys::signal::{kill, Signal};
#[cfg(not(target_os = "linux"))]
use nix::unistd::Pid as NativePid;
use pohunek_platform::process::{
    ControlGroupPath, Error, ExitWatch, HostInspector, OwnershipMarkers, Pid, ProcessCoalitions,
    ProcessFact, ProcessIdentity, ProcessInspector, ProcessLineage,
};

/// The real host in which a scenario sees only the processes working in its own
/// directory.
///
/// A runtime sweep decides every same-user process of the host. Some of them
/// (another test's fresh execs, a CI runner's helpers) are neither readable
/// nor attributable by fork lineage while their creator has exited, which makes
/// a cleanup that is complete for the scenario's own processes
/// nondeterministic on a loaded host. This view lists only the processes whose
/// working directory is the scenario's directory or below it, so the scenario's
/// runtime and the bystanders it starts there are exactly the table the sweep
/// decides.
/// Every observation about a process, markers and lineage included, and every
/// signal the sweep sends, goes to the real host unchanged.
#[derive(Debug)]
pub(crate) struct ScopedHost {
    host: HostInspector,
    /// Canonical working directory that selects the scenario's processes.
    scope: PathBuf,
}

impl ScopedHost {
    /// Creates the view over the processes working in `directory`.
    ///
    /// # Panics
    ///
    /// Panics when `directory` cannot be canonicalized, which a scenario's own
    /// freshly created directory always can, and when the test process's own
    /// working directory lies inside it: a process spawned without an explicit
    /// `current_dir` would inherit that directory and be taken for the
    /// scenario's own, although another test started it.
    pub(crate) fn new(directory: &Path) -> Self {
        let scope = std::fs::canonicalize(directory).expect("canonical scenario directory");
        refuse_inherited_scope(&scope);
        Self {
            host: HostInspector::new(),
            scope,
        }
    }

    /// Whether the process works in the scenario's directory or below it.
    ///
    /// A process whose working directory cannot be read has exited or is
    /// between images; it is not listed. The working directory is compared as
    /// the kernel reports it, which is canonical; one that no longer resolves
    /// (a removed directory) is compared by its reported path.
    fn in_scope(&self, pid: Pid) -> bool {
        self.host.cwd(pid).is_ok_and(|cwd| {
            let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
            cwd.starts_with(&self.scope)
        })
    }
}

/// Panics when the working directory of this process lies at or below `scope`.
///
/// A scenario's processes are told apart by their working directory, and a
/// child that is spawned without `current_dir` has this process's. A test
/// therefore never changes the process working directory (the hermetic scan
/// refuses `set_current_dir` in test code; a test of its own process uses
/// `pohunek_test_support::process_env::set_current_dir`).
fn refuse_inherited_scope(scope: &Path) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    assert!(
        !cwd.starts_with(scope),
        "the test process's working directory {} lies inside the scenario scope {}: a process \
         spawned without an explicit current_dir would inherit it and be taken for this \
         scenario's own; do not change the test process's working directory",
        cwd.display(),
        scope.display()
    );
}

impl ProcessInspector for ScopedHost {
    fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, Error> {
        self.host.identity(pid)
    }

    fn is_running(&self, identity: ProcessIdentity) -> Result<bool, Error> {
        self.host.is_running(identity)
    }

    fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, Error> {
        self.host.parent_pid(pid)
    }

    fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, Error> {
        self.host.process(pid)
    }

    fn same_user_processes(&self) -> Result<Vec<ProcessFact>, Error> {
        Ok(self
            .host
            .same_user_processes()?
            .into_iter()
            .filter(|fact| self.in_scope(fact.pid))
            .collect())
    }

    fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, Error> {
        self.host.descendants(root)
    }

    fn descendant_identities(&self, root: ProcessIdentity) -> Result<Vec<ProcessIdentity>, Error> {
        self.host.descendant_identities(root)
    }

    fn cwd(&self, pid: Pid) -> Result<PathBuf, Error> {
        self.host.cwd(pid)
    }

    fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, Error> {
        self.host.executable(pid)
    }

    fn exit_watch(&self, identity: ProcessIdentity) -> Result<ExitWatch, Error> {
        self.host.exit_watch(identity)
    }

    fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, Error> {
        self.host.ownership_markers(pid)
    }

    fn lineage(&self, pid: Pid) -> Result<Option<ProcessLineage>, Error> {
        self.host.lineage(pid)
    }

    fn coalitions(&self, pid: Pid) -> Result<Option<ProcessCoalitions>, Error> {
        self.host.coalitions(pid)
    }

    fn control_group(&self, pid: Pid) -> Result<Option<ControlGroupPath>, Error> {
        self.host.control_group(pid)
    }

    fn foreground_process_group(&self, root_pid: Pid) -> Result<Option<Pid>, Error> {
        self.host.foreground_process_group(root_pid)
    }
}

/// File name of the session worker executable.
const WORKER_EXECUTABLE: &str = "pohunek-sessiond";

/// Kills every process working in a scenario's directory (or below it) when it
/// drops, on success and while a panic unwinds, so a runtime the scenario left
/// alive does not outlive it, and returns only once none of them runs.
///
/// The scenario's processes are found by their working directory, so the
/// reaper must drop while that directory exists, and the scope is fixed when the
/// reaper is created. A process the scenario started with another working
/// directory is outside its reach.
///
/// Dropping never panics: a reaper can drop at thread exit, where an unwind
/// aborts the test process. A process that survives `SIGKILL` for the whole
/// hang guard is reported on standard error instead.
#[derive(Debug)]
pub(crate) struct ScopeReaper {
    host: ScopedHost,
    /// Whether session workers are left to the fixture's `WorkerGuard`.
    spares_workers: bool,
}

impl ScopeReaper {
    /// Creates the reaper of the processes working in `directory`.
    ///
    /// # Panics
    ///
    /// Panics when `directory` cannot be canonicalized, which a scenario's own
    /// freshly created directory always can, and when the test process's own
    /// working directory lies inside it: a process spawned without an explicit
    /// `current_dir` would inherit that directory and be taken for the
    /// scenario's own, although another test started it.
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            host: ScopedHost::new(directory),
            spares_workers: false,
        }
    }

    /// Like [`Self::new`], but leaves every `pohunek-sessiond` process alone.
    ///
    /// For a root whose `TestEnv` carries a `WorkerGuard`: ending a leaked worker
    /// here would hide it from the guard, which fails the test that left it.
    /// The worker's own workload is still ended.
    #[allow(
        dead_code,
        reason = "only the supervised-lifecycle suite, which includes this file, spares workers; `expect` would fail in that crate"
    )]
    pub(crate) fn sparing_workers(directory: &Path) -> Self {
        let mut reaper = Self::new(directory);
        reaper.spares_workers = true;
        reaper
    }

    /// Whether `fact` is a session worker this reaper leaves alone.
    ///
    /// A worker names its executable in the first argument, or in the second
    /// when an interpreter runs it, which is how a stand-in worker script runs.
    fn is_spared(&self, fact: &ProcessFact) -> bool {
        self.spares_workers
            && fact.cmdline.iter().take(2).any(|argument| {
                Path::new(argument)
                    .file_name()
                    .is_some_and(|name| name == WORKER_EXECUTABLE)
            })
    }

    /// Identities of the scenario's processes that still run.
    ///
    /// # Errors
    ///
    /// Returns the inspection failure: a process table or liveness read that
    /// failed proves nothing about whether the processes exited. A process that
    /// exits during the read is not running.
    pub(crate) fn running(&self) -> Result<Vec<ProcessIdentity>, Error> {
        let mut running = Vec::new();
        for fact in self.host.same_user_processes()? {
            let identity = fact.identity();
            if identity.pid == std::process::id() || self.is_spared(&fact) {
                continue;
            }
            match self.host.is_running(identity) {
                Ok(true) => running.push(identity),
                Ok(false) => {}
                Err(error) if error.is_race() => {}
                Err(error) => return Err(error),
            }
        }
        Ok(running)
    }

    /// PIDs of the processes that still run in the scenario's directory.
    ///
    /// # Panics
    ///
    /// Panics when the process table cannot be inspected.
    pub(crate) fn pids(&self) -> BTreeSet<u32> {
        self.running()
            .expect("inspect the scenario's processes")
            .into_iter()
            .map(|identity| identity.pid)
            .collect()
    }

    /// Signals every running process of the scenario with `SIGKILL`, each only
    /// while its PID still names the listed process.
    fn kill_running(&self) -> Result<Vec<ProcessIdentity>, Error> {
        let running = self.running()?;
        for identity in &running {
            kill_if_current(&self.host, *identity);
        }
        Ok(running)
    }
}

/// Sends `SIGKILL` to `identity` only while its PID still names that process.
///
/// A process that exited, or whose PID now names another process, is left
/// alone. On Linux the signal goes through a pidfd opened before the identity
/// check, so it cannot reach a process that reused the PID after the check.
/// Elsewhere a PID reused between the check and `kill` is a microsecond-wide
/// window, and the start-identity check keeps it from covering any process
/// that existed before the check.
pub(crate) fn kill_if_current(host: &dyn ProcessInspector, identity: ProcessIdentity) {
    let Some(pid) = i32::try_from(identity.pid).ok().filter(|raw| *raw > 0) else {
        return;
    };
    #[cfg(target_os = "linux")]
    {
        use rustix::process::{pidfd_open, pidfd_send_signal, PidfdFlags, Signal};

        // The pidfd pins the process it opened; the identity check after
        // opening it proves the pidfd names `identity`.
        let Some(Ok(pidfd)) =
            rustix::process::Pid::from_raw(pid).map(|pid| pidfd_open(pid, PidfdFlags::empty()))
        else {
            return;
        };
        if is_current(host, identity) {
            let _signalled = pidfd_send_signal(&pidfd, Signal::KILL);
        }
    }
    #[cfg(not(target_os = "linux"))]
    if is_current(host, identity) {
        let _signalled = kill(NativePid::from_raw(pid), Signal::SIGKILL);
    }
}

/// Whether the PID of `identity` still names that process.
fn is_current(host: &dyn ProcessInspector, identity: ProcessIdentity) -> bool {
    matches!(host.identity(identity.pid), Ok(Some(current)) if current == identity)
}

impl Drop for ScopeReaper {
    /// Signals every running process of the scenario again until a listing
    /// finds none: a process that forked before it was signalled shows up in
    /// the next listing. A failed inspection is retried and never counts as an
    /// exit, so a persistent failure ends in the hang guard instead of a
    /// teardown that reports success; the hang guard is reported on standard
    /// error and never unwinds.
    fn drop(&mut self) {
        let reaped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pohunek_test_support::wait::poll_until("the scenario's processes to exit", || {
                self.kill_running().ok()?.is_empty().then_some(())
            });
        }));
        if reaped.is_err() {
            eprintln!("the scenario's processes did not exit after SIGKILL");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scope that holds the process working directory is refused and the
    /// message names both; a scope elsewhere is accepted.
    #[test]
    fn a_scope_that_holds_the_process_working_directory_is_refused() {
        if !pohunek_test_support::process_env::in_own_process() {
            return;
        }
        let inside = pohunek_test_support::tempdir().expect("scope directory");
        let elsewhere = pohunek_test_support::tempdir().expect("another directory");
        let _accepted = ScopeReaper::new(inside.path());
        pohunek_test_support::process_env::set_current_dir(inside.path());

        let refused = std::panic::catch_unwind(|| ScopeReaper::new(inside.path()))
            .expect_err("a scope holding the working directory is refused");
        let message = refused
            .downcast_ref::<String>()
            .cloned()
            .expect("a formatted panic message");
        let canonical = std::fs::canonicalize(inside.path()).expect("canonical scope");
        assert!(
            message.contains(&format!("working directory {}", canonical.display()))
                && message.contains(&format!("scenario scope {}", canonical.display())),
            "{message}"
        );

        let _accepted = ScopeReaper::new(elsewhere.path());
    }
}
