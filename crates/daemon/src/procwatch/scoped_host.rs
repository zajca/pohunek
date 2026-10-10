//! Host process table view that confines a cleanup scenario to its own processes.

// Rust guideline compliant 2026-10-10

use std::path::{Path, PathBuf};

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
/// working directory is the scenario's directory, so the scenario's runtime and
/// the bystanders it starts there are exactly the table the sweep decides.
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

    /// Whether the process works in the scenario's directory.
    ///
    /// A process whose working directory cannot be read has exited or is
    /// between images; it is not listed.
    fn in_scope(&self, pid: Pid) -> bool {
        self.host
            .cwd(pid)
            .is_ok_and(|cwd| std::fs::canonicalize(cwd).is_ok_and(|cwd| cwd == self.scope))
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
