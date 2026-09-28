//! Host process table view that tests use to keep removal sweeps hermetic.
//!
//! The daemon's unit tests declare this module under `cfg(test)`, and daemon
//! integration tests include the same file by path, so it names its types
//! through `pohunek_platform` rather than through the daemon crate.

// Rust guideline compliant 2026-09-28

use std::path::PathBuf;

use pohunek_platform::process::{
    Error, ExitWatch, HostInspector, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity,
    ProcessInspector,
};

/// The host process table in which a process whose ownership markers cannot be
/// read counts as exited.
///
/// A removal and a lost-runtime cleanup sweep every same-user process marked
/// with the runtime. A process whose markers cannot be read (a non-dumpable
/// helper, a process between images) may carry that marker, so the sweep
/// stays unconfirmed unless the process provably started before the runtime's
/// worker. Unrelated processes of a loaded test host (other tests' fresh
/// execs, the CI runner's non-dumpable helpers) start after the worker, which
/// would make every such cleanup and every removal nondeterministic. This view
/// reports those marker reads as a race, which the sweep treats as a process
/// that exited. Every other observation, and every signal the sweep sends,
/// goes to the real host.
#[derive(Debug, Default)]
pub(crate) struct ReadableHost {
    host: HostInspector,
}

impl ReadableHost {
    /// Creates the view over the real host process table.
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

impl ProcessInspector for ReadableHost {
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
        self.host.same_user_processes()
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
        match self.host.ownership_markers(pid) {
            Err(Error::PermissionDenied { .. } | Error::Unobservable { .. }) => Err(Error::Race {
                operation: "readable_host_markers",
            }),
            markers => markers,
        }
    }

    fn foreground_process_group(&self, root_pid: Pid) -> Result<Option<Pid>, Error> {
        self.host.foreground_process_group(root_pid)
    }
}
