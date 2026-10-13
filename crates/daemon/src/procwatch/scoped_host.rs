//! Host process table view that confines a cleanup scenario to its own processes,
//! and the reaper that ends them when the scenario's directory goes away.

// Rust guideline compliant 2026-10-12

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid as NativePid;
use pohunek_platform::process::{
    Error, ExitWatch, HostInspector, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity,
    ProcessInspector, ProcessLineage,
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
    /// freshly created directory always can.
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            host: HostInspector::new(),
            scope: std::fs::canonicalize(directory).expect("canonical scenario directory"),
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
    /// freshly created directory always can.
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
            let Ok(raw) = i32::try_from(identity.pid) else {
                continue;
            };
            if matches!(self.host.identity(identity.pid), Ok(Some(current)) if current == *identity)
            {
                let _signalled = kill(NativePid::from_raw(raw), Signal::SIGKILL);
            }
        }
        Ok(running)
    }
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
