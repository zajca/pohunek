//! launchd worker supervisor, daemon agent, and workflow plugin unit backends.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::super::{
    DaemonSupervisor, Error, ExitEvidence, JobDefinition, JobExit, JobLogs, Namespace, Operation,
    PluginUnitDefinition, PluginUnitKey, PluginUnitKind, PluginUnits, RestartPolicy, ServiceId,
    ServiceObservation, ServiceState, Supervisor, WorkerKey, MAX_JOB_TIMEOUT,
};
use super::discovery::{classify, refuse_rejected, set_aside_loaded};
use super::launchctl::{Launchctl, Status};
use super::plist::{self, Stored, MAX_DEFINITION_BYTES};
use super::registration::{
    self, definition_name, fs_error, remove_file, set_aside_name, DEFINITION_MODE,
};
use super::{run_error, status_error, Presence};
use crate::filesystem::{EntryKind, TrustedDir};
use crate::process::{
    environment_entry_is, DarwinInspector, ExitReportWatch, Pid, ProcessExit, ProcessFact,
    ProcessIdentity, ProcessInspector as _,
};
use rustix::process::{kill_process, Pid as NativePid, Signal};

// Rust guideline compliant 2026-10-13

/// PID of launchd; every job launchd spawns is its direct child.
const LAUNCHD_PID: Pid = 1;

/// Mode of the private worker definition and log directories.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Bits that make a directory unsafe to hold the daemon agent and plugin unit
/// definitions.
///
/// `~/Library/LaunchAgents` is commonly `0755`; only write access for others
/// would let another account substitute the agent.
const AGENTS_DIR_FORBIDDEN_BITS: u32 = 0o022;

/// Suffix of a worker's launchd standard-output log.
const STDOUT_SUFFIX: &str = ".out.log";

/// Suffix of a worker's launchd standard-error log.
const STDERR_SUFFIX: &str = ".err.log";

/// Most entries discovery reads from the private definitions directory.
///
/// Mirrors the systemd backend's unit listing bound. Far above any real
/// session count; more entries mean the directory is not ours to trust.
pub const MAX_DISCOVERED_DEFINITIONS: usize = 4_096;

/// Extra time after a job's `ExitTimeOut` for launchd to reap it and drop the
/// label.
///
/// launchd sends `SIGKILL` when `ExitTimeOut` expires; unregistering follows
/// within milliseconds, and the margin absorbs scheduling delays on loaded
/// hosts. Shorter margins turn a slow reap into a spurious timeout.
const ABSENCE_MARGIN: Duration = Duration::from_secs(5);

/// Delay between `print` probes while waiting for a label to disappear.
///
/// Each probe spawns `launchctl`; 100 ms keeps a 30 s wait under 300 spawns
/// while adding at most a tenth of a second to retirement.
const ABSENCE_POLL: Duration = Duration::from_millis(100);

/// Environment variable launchd sets to the label in every job it spawns.
///
/// A plugin unit's argv need not be unique across installations, so its
/// process is bound to its label through this entry as well; the value is
/// compared in place and never read out.
const SERVICE_NAME_VARIABLE: &str = "XPC_SERVICE_NAME";

/// Worker argument introducing the session ID.
const SESSION_ID_ARGUMENT: &str = "--session-id";

/// Worker argument introducing the runtime generation.
const GENERATION_ARGUMENT: &str = "--worker-generation";

/// Longest wait for the kernel's exit event of a process that is already
/// proven gone.
///
/// The event is queued on the registration when the process ends; the wait only
/// absorbs the gap between the process leaving the process table and the
/// kernel posting the event. It is bounded by the `launchctl` command
/// deadline, and kept short because a caller that reads the exit of a job whose
/// process still runs waits the whole span.
const EXIT_EVENT_GRACE: Duration = Duration::from_secs(2);

/// What the supervisor knows about the end of one worker's main process.
#[derive(Debug)]
enum ExitRecord {
    /// The registration is armed and no event has been read yet.
    Armed(ExitReportWatch),
    /// The event was read; the kernel delivers it only once, so the decoded
    /// result is kept for every later read.
    Ended(ProcessExit),
}

/// Exit records of the workers whose watch is armed, keyed by generation.
type ExitRecords = Mutex<HashMap<ServiceId, Arc<tokio::sync::Mutex<ExitRecord>>>>;

/// Result of [`LaunchdSupervisor::discover_definitions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// Loaded jobs of this namespace.
    pub observations: Vec<ServiceObservation>,
    /// Definition file names of this namespace that were skipped because
    /// they do not parse, their `Label` differs from the file name, or they
    /// are not a private regular file. They are never loaded, and the strict
    /// discovery refuses a result that lists any.
    pub rejected: Vec<String>,
    /// Whether a job may be missing from [`Discovery::observations`]: its
    /// inspection raced, or it is loaded while its only definition is the
    /// set-aside file of an interrupted registration. Only the strict
    /// discovery refuses such a result.
    pub incomplete: bool,
}

/// Supervises the session workers of one installation namespace in
/// `gui/<uid>`.
///
/// Each worker generation is one job labeled
/// `io.github.zajca.pohunek.<ns>.worker.<session-id>.<generation>`, defined by
/// `<definitions>/<label>.plist`, and logging to
/// `<logs>/<label>.{out,err}.log`. The daemon label can never be addressed
/// through this type because [`ServiceId`]s map only to worker labels.
#[derive(Debug)]
pub struct LaunchdSupervisor {
    namespace: Namespace,
    jobs: Jobs,
    definitions: PathBuf,
    logs: PathBuf,
    exits: ExitRecords,
    exit_grace: Duration,
}

impl LaunchdSupervisor {
    /// Creates a supervisor for `namespace` in `gui/<uid>`.
    ///
    /// `definitions` is the private `<state>/pohunek/launchd` directory and
    /// `logs` the `<log_dir>/launchd` directory; both are created `0700` on
    /// first use. Every `launchctl` command ends after `command_deadline`;
    /// `bootout` additionally gets the job's `ExitTimeOut`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDefinition`] for a relative directory or a zero
    /// deadline.
    pub fn new(
        namespace: Namespace,
        uid: u32,
        definitions: PathBuf,
        logs: PathBuf,
        command_deadline: Duration,
    ) -> Result<Self, Error> {
        require_absolute("definitions directory", &definitions)?;
        require_absolute("launchd log directory", &logs)?;
        Ok(Self {
            namespace,
            jobs: Jobs::new(uid, command_deadline)?,
            definitions,
            logs,
            exits: Mutex::new(HashMap::new()),
            exit_grace: command_deadline.min(EXIT_EVENT_GRACE),
        })
    }

    /// Returns the log files a worker definition for `key` must name.
    ///
    /// `start` rejects definitions with other log paths, so retirement always
    /// removes exactly the files launchd wrote.
    #[must_use]
    pub fn worker_logs(&self, key: &WorkerKey) -> JobLogs {
        let label = self.namespace.worker_label(key);
        JobLogs {
            stdout: self.logs.join(format!("{label}{STDOUT_SUFFIX}")),
            stderr: self.logs.join(format!("{label}{STDERR_SUFFIX}")),
        }
    }

    /// Discovers this namespace's jobs and reports skipped definition files.
    ///
    /// Enumerates the private definitions directory, keeps only
    /// `<label>.plist` files whose label parses as a worker of this namespace,
    /// and inspects each. Files that do not parse, whose `Label` differs from
    /// the file name, or that are not a private regular file are listed in
    /// [`Discovery::rejected`]. Jobs that vanish
    /// are skipped; a job that changes during inspection marks the discovery
    /// [`Discovery::incomplete`] instead of being dropped, because the job
    /// still exists and only [`LaunchdSupervisor::discover_strict`] may
    /// decide what a caller that cannot tolerate an omission does.
    ///
    /// A worker whose `<label>.plist` is missing while an interrupted
    /// registration left its `.<label>.plist.replaced` is probed without
    /// moving either file: a loaded label marks the discovery incomplete, an
    /// absent one runs nothing. A missing definitions directory is an empty
    /// result, because every worker is bootstrapped from a definition `start`
    /// publishes into the directory it creates, and only an uninstall that
    /// retired every worker removes the directory. launchd lists no labels
    /// without parsing `launchctl` output, so the directory is the registry
    /// the systemd backend gets from its manager.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidData`] for more than
    /// [`MAX_DISCOVERED_DEFINITIONS`] entries, and the first inspection
    /// failure other than [`Error::NotFound`] or [`Error::Race`].
    pub async fn discover_definitions(&self) -> Result<Discovery, Error> {
        const OPERATION: &str = "discover";

        let mut discovery = Discovery {
            observations: Vec::new(),
            rejected: Vec::new(),
            incomplete: false,
        };
        let Some(directory) = open_private(&self.definitions, OPERATION)? else {
            return Ok(discovery);
        };
        let names = directory.entry_names().map_err(fs_error(OPERATION))?;
        if names.len() > MAX_DISCOVERED_DEFINITIONS {
            return Err(Error::InvalidData {
                operation: OPERATION,
                detail: format!(
                    "definitions directory holds {} entries; maximum is {MAX_DISCOVERED_DEFINITIONS}",
                    names.len()
                ),
            });
        }
        let entries = classify(&directory, &names, &self.namespace, OPERATION)?;
        discovery.rejected = entries.rejected;
        drop(directory);
        discovery.incomplete = set_aside_loaded(
            &self.jobs.launchctl,
            &self.jobs.domain,
            &self.namespace,
            &entries.set_aside,
            OPERATION,
        )
        .await?;
        for key in entries.candidates {
            match self.inspect_worker(&key.service_id()).await {
                Ok(observation) => discovery.observations.push(observation),
                // The job is not loaded; it cannot reference its version.
                Err(Error::NotFound(_)) => {}
                // The job still exists but its identity is unproven, so a
                // result omitting it is incomplete; only the strict
                // discovery refuses it.
                Err(Error::Race { .. }) => discovery.incomplete = true,
                Err(error) => return Err(error),
            }
        }
        Ok(discovery)
    }

    async fn start_worker(&self, id: &ServiceId, definition: &JobDefinition) -> Result<(), Error> {
        const OPERATION: &str = "start";

        let key = WorkerKey::from_service_id(id)?;
        self.validate_worker(&key, definition)?;
        let label = self.namespace.worker_label(&key);
        let bytes = plist::render(&label, definition)?;
        let directory = TrustedDir::open_or_create_absolute(&self.definitions, PRIVATE_DIR_MODE)
            .map_err(fs_error(OPERATION))?;
        TrustedDir::open_or_create_absolute(&self.logs, PRIVATE_DIR_MODE)
            .map_err(fs_error(OPERATION))?;
        self.jobs
            .register(OPERATION, id, &label, &directory, &bytes)
            .await
    }

    fn validate_worker(&self, key: &WorkerKey, definition: &JobDefinition) -> Result<(), Error> {
        if definition.restart() != RestartPolicy::Never {
            return Err(Error::InvalidDefinition {
                detail: "a session worker must never be restarted by launchd".to_owned(),
            });
        }
        if definition.logs() != Some(&self.worker_logs(key)) {
            return Err(Error::InvalidDefinition {
                detail: "worker logs must be the supervisor's per-label launchd logs".to_owned(),
            });
        }
        let rule = ArgvRule::Worker {
            session_id: key.session_id().to_owned(),
            generation: key.generation().to_owned(),
        };
        if !rule.matches_arguments(definition.arguments()) {
            return Err(Error::InvalidDefinition {
                detail: "worker arguments must carry its session ID and generation".to_owned(),
            });
        }
        Ok(())
    }

    async fn inspect_worker(&self, id: &ServiceId) -> Result<ServiceObservation, Error> {
        const OPERATION: &str = "inspect";

        let key = WorkerKey::from_service_id(id)?;
        let label = self.namespace.worker_label(&key);
        let rule = ArgvRule::Worker {
            session_id: key.session_id().to_owned(),
            generation: key.generation().to_owned(),
        };
        let definitions = &self.definitions;
        self.jobs
            .observe(OPERATION, id, &label, rule, || {
                match open_private(definitions, OPERATION)? {
                    Some(directory) => read_stored(&directory, &definition_name(&label), OPERATION),
                    None => Ok(None),
                }
            })
            .await
    }

    fn exit_records(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<ServiceId, Arc<tokio::sync::Mutex<ExitRecord>>>> {
        // The map holds no invariant a panicking holder could break.
        self.exits.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn watch_worker_exit(&self, id: &ServiceId, process: ProcessIdentity) -> Result<(), Error> {
        const OPERATION: &str = "watch_exit";

        WorkerKey::from_service_id(id)?;
        if self.exit_records().contains_key(id) {
            return Ok(());
        }
        let watch = DarwinInspector::new()
            .exit_report(process)
            .map_err(|error| process_error(OPERATION, error))?;
        self.exit_records()
            .entry(id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(ExitRecord::Armed(watch))));
        Ok(())
    }

    /// Reads how the worker's main process ended from its armed watch.
    ///
    /// `None` when no watch was armed or no exit event arrived within the
    /// grace period. An absent label is [`Error::NotFound`] and drops the
    /// record.
    async fn worker_exit(
        &self,
        id: &ServiceId,
        operation: &'static str,
    ) -> Result<Option<ProcessExit>, Error> {
        let key = WorkerKey::from_service_id(id)?;
        let label = self.namespace.worker_label(&key);
        if self.jobs.presence(operation, &label).await? == Presence::Absent {
            self.exit_records().remove(id);
            return Err(Error::NotFound(id.clone()));
        }
        let Some(record) = self.exit_records().get(id).cloned() else {
            return Ok(None);
        };
        let mut record = record.lock().await;
        if let ExitRecord::Armed(watch) = &*record {
            match tokio::time::timeout(self.exit_grace, watch.report()).await {
                Ok(Ok(exit)) => *record = ExitRecord::Ended(exit),
                Ok(Err(error)) => return Err(process_error(operation, error)),
                Err(_elapsed) => return Ok(None),
            }
        }
        Ok(match &*record {
            ExitRecord::Ended(exit) => Some(*exit),
            ExitRecord::Armed(_) => None,
        })
    }

    async fn retire_worker(&self, id: &ServiceId) -> Result<(), Error> {
        let retired = self.end_worker(id).await;
        // A job that is still loaded keeps its record for a later read.
        if matches!(retired, Ok(()) | Err(Error::NotFound(_))) {
            self.exit_records().remove(id);
        }
        retired
    }

    async fn end_worker(&self, id: &ServiceId) -> Result<(), Error> {
        const OPERATION: &str = "retire";

        let key = WorkerKey::from_service_id(id)?;
        let label = self.namespace.worker_label(&key);
        let file_name = definition_name(&label);
        let directory = open_private(&self.definitions, OPERATION)?;
        // A definition an interrupted `start` left set aside is settled first,
        // so its stored `ExitTimeOut` bounds the wait and no file survives.
        if let Some(directory) = &directory {
            registration::recover(directory, &label, OPERATION)?;
        }
        let stored = directory
            .as_ref()
            .and_then(|directory| usable_stored(directory, &file_name, OPERATION))
            .filter(|stored| stored.label == label);
        let rule = ArgvRule::Worker {
            session_id: key.session_id().to_owned(),
            generation: key.generation().to_owned(),
        };
        let before = self
            .jobs
            .end_job(OPERATION, &label, stored.as_ref(), rule)
            .await?;
        if let Some(directory) = &directory {
            remove_file(directory, &file_name, OPERATION)?;
        }
        if let Some(logs) = open_private(&self.logs, OPERATION)? {
            remove_file(&logs, &format!("{label}{STDOUT_SUFFIX}"), OPERATION)?;
            remove_file(&logs, &format!("{label}{STDERR_SUFFIX}"), OPERATION)?;
        }
        match before {
            Presence::Loaded => Ok(()),
            Presence::Absent => Err(Error::NotFound(id.clone())),
        }
    }
}

impl Supervisor for LaunchdSupervisor {
    /// Writes `<label>.plist` and bootstraps it.
    ///
    /// The label is probed with `print` first; a loaded label is
    /// [`Error::AlreadyRegistered`] and its definition file is left untouched.
    /// A leftover file of an absent label is moved aside and the label probed
    /// again before the new definition is published without replacing any
    /// file, so a job loaded concurrently keeps the definition it was loaded
    /// from. The leftover is discarded only once the new definition is kept;
    /// when `bootstrap` proves nothing was loaded, the new file is removed and
    /// the leftover returns to its name. A definition an interrupted `start`
    /// left set aside is settled first; see [`registration::recover`].
    fn start<'a>(&'a self, id: &'a ServiceId, definition: &'a JobDefinition) -> Operation<'a, ()> {
        Box::pin(self.start_worker(id, definition))
    }

    /// Returns the lenient discovery for reconciliation.
    ///
    /// A raced job is omitted, because discovery is repeated anyway; the
    /// destructive [`Supervisor::discover_strict`] refuses such a result
    /// instead.
    fn discover(&self) -> Operation<'_, Vec<ServiceObservation>> {
        Box::pin(async move {
            let discovery = self.discover_definitions().await?;
            super::super::warn_rejected("launchd", &discovery.rejected);
            Ok(discovery.observations)
        })
    }

    /// Returns the destructive discovery: a raced job still exists but its
    /// identity is unproven, a loaded job whose only definition is set aside
    /// cannot be inspected until its registration settles, and a rejected
    /// definition of this namespace may belong to a loaded job whose process
    /// is never matched, so a result that may omit any of them fails instead
    /// of letting a caller delete the version it may execute. A race or a
    /// set-aside loaded job is [`Error::Race`], a rejected definition
    /// [`Error::InvalidData`].
    fn discover_strict(&self) -> Operation<'_, Vec<ServiceObservation>> {
        Box::pin(async move {
            let discovery = self.discover_definitions().await?;
            if discovery.incomplete {
                return Err(Error::Race {
                    operation: "discover",
                });
            }
            super::super::warn_rejected("launchd", &discovery.rejected);
            refuse_rejected("discover", &discovery.rejected)?;
            Ok(discovery.observations)
        })
    }

    /// Combines `print` presence, the private definition, and process facts.
    ///
    /// A loaded job with a matching process is [`ServiceState::Running`]; a
    /// loaded job without one is [`ServiceState::Unknown`] with no process.
    /// launchd keeps a `RunAtLoad` job without `KeepAlive` loaded after its
    /// process exits and exposes no further state without parsing `print`
    /// output, so that one observation covers a job not spawned yet, a
    /// process that does not match the definition, and an exited process.
    /// It is never proof that the job ended; see the module documentation.
    fn inspect<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ServiceObservation> {
        Box::pin(self.inspect_worker(id))
    }

    /// Boots the job out, waits until its label is absent, ends the rest of
    /// its process group, then removes its definition and launchd logs.
    ///
    /// launchd sends the main process `SIGTERM`, then `SIGKILL` after
    /// `ExitTimeOut`. With `AbandonProcessGroup=false` it only sends `SIGTERM`
    /// to the remaining process group once the main process is gone, so a
    /// member that ignores it would outlive the job (observed on macOS 15).
    /// Members captured before `bootout` therefore get `SIGTERM` and, once
    /// `ExitTimeOut` has passed since `bootout` started, `SIGKILL`: the same
    /// bound systemd's `KillMode=control-group` applies. Members forked after
    /// the capture, and processes that left the group, are left to the
    /// daemon's ownership-marker sweep. An absent label still has leftover
    /// files, including a definition an interrupted `start` left set aside,
    /// removed and returns [`Error::NotFound`]. Several processes
    /// matching the definition are [`Error::InvalidData`] before anything is
    /// booted out or removed, because the group to end is unknown.
    fn retire<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ()> {
        Box::pin(self.retire_worker(id))
    }

    /// Reports how the worker's main process ended, from the kernel watch
    /// [`Supervisor::watch_exit`] armed.
    ///
    /// launchd keeps no exit record that is readable without parsing `print`
    /// output, so only a process that ended while the watch was armed has one.
    /// A clean exit is `Code(0)`; a memory kill by the kernel is the signal
    /// that ended the process.
    fn exit_status<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, Option<JobExit>> {
        Box::pin(async move { Ok(self.worker_exit(id, "exit_status").await?.map(job_exit)) })
    }

    /// Reports the end of a failed worker in the systemd vocabulary:
    /// `exit-code`, `signal`, `core-dump`, or `oom-kill` for a kill under
    /// memory pressure (jetsam). A clean exit is no evidence.
    fn exit_evidence<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, Option<ExitEvidence>> {
        Box::pin(async move {
            Ok(self
                .worker_exit(id, "exit_evidence")
                .await?
                .and_then(evidence))
        })
    }

    /// Arms the kernel exit watch for the worker's main process.
    ///
    /// A second call for a generation that has a record keeps it: the watch
    /// of an exited process cannot be armed again, and the queued event must
    /// not be lost.
    fn watch_exit<'a>(&'a self, id: &'a ServiceId, process: ProcessIdentity) -> Operation<'a, ()> {
        Box::pin(std::future::ready(self.watch_worker_exit(id, process)))
    }
}

fn job_exit(exit: ProcessExit) -> JobExit {
    match exit {
        ProcessExit::Code(code) => JobExit::Code(code),
        ProcessExit::Signal { signal, .. } | ProcessExit::MemoryPressure { signal } => {
            JobExit::Signal(signal)
        }
    }
}

fn evidence(exit: ProcessExit) -> Option<ExitEvidence> {
    let result = match exit {
        ProcessExit::Code(0) => return None,
        ProcessExit::Code(_) => "exit-code",
        ProcessExit::Signal {
            core_dumped: true, ..
        } => "core-dump",
        ProcessExit::Signal { .. } => "signal",
        ProcessExit::MemoryPressure { .. } => "oom-kill",
    };
    Some(ExitEvidence::new(result.to_owned(), Some(job_exit(exit))))
}

/// Manages the daemon login agent of one installation namespace.
///
/// The agent is labeled `io.github.zajca.pohunek.<ns>.daemon` and defined by
/// `<agents>/<label>.plist`; production passes `~/Library/LaunchAgents` so
/// launchd loads it again at login. No operation here addresses a worker label.
#[derive(Debug)]
pub struct LaunchdDaemon {
    id: ServiceId,
    jobs: Jobs,
    agents: PathBuf,
}

impl LaunchdDaemon {
    /// Creates the daemon agent manager for `namespace` in `gui/<uid>`.
    ///
    /// `agents` is the `LaunchAgents` directory; `install` creates it `0700`
    /// when missing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDefinition`] for a relative directory or a zero
    /// deadline.
    pub fn new(
        namespace: &Namespace,
        uid: u32,
        agents: PathBuf,
        command_deadline: Duration,
    ) -> Result<Self, Error> {
        require_absolute("LaunchAgents directory", &agents)?;
        Ok(Self {
            id: ServiceId::parse(namespace.daemon_label())?,
            jobs: Jobs::new(uid, command_deadline)?,
            agents,
        })
    }

    fn label(&self) -> &str {
        self.id.as_str()
    }

    fn validate(definition: &JobDefinition) -> Result<(), Error> {
        match definition.restart() {
            RestartPolicy::OnFailure { .. } => Ok(()),
            RestartPolicy::Never => Err(Error::InvalidDefinition {
                detail: "the daemon agent must restart after a failure".to_owned(),
            }),
            RestartPolicy::Always { .. } => Err(Error::InvalidDefinition {
                detail: "the daemon agent must stay stopped after a clean exit".to_owned(),
            }),
        }
    }

    async fn install_agent(&self, definition: &JobDefinition) -> Result<(), Error> {
        const OPERATION: &str = "install";

        Self::validate(definition)?;
        let bytes = plist::render(self.label(), definition)?;
        let directory = create_agents(&self.agents, OPERATION)?;
        self.jobs
            .register(OPERATION, &self.id, self.label(), &directory, &bytes)
            .await
    }

    async fn replace_agent(&self, definition: &JobDefinition) -> Result<(), Error> {
        const OPERATION: &str = "replace";

        Self::validate(definition)?;
        let bytes = plist::render(self.label(), definition)?;
        let directory = open_agents(&self.agents, OPERATION)?
            .ok_or_else(|| Error::NotFound(self.id.clone()))?;
        // An interrupted replacement may have stopped the agent and left its
        // definition set aside; putting it back lets the replacement resume.
        registration::recover(&directory, self.label(), OPERATION)?;
        registration::require_definition(&directory, &self.id, self.label(), OPERATION)?;
        let exit_timeout =
            stored_exit_timeout(&directory, &definition_name(self.label()), OPERATION);
        self.jobs
            .unload(OPERATION, self.label(), exit_timeout)
            .await?;
        self.jobs
            .register(OPERATION, &self.id, self.label(), &directory, &bytes)
            .await
    }

    async fn inspect_agent(&self) -> Result<ServiceObservation, Error> {
        const OPERATION: &str = "inspect_daemon";

        let file_name = definition_name(self.label());
        self.jobs
            .observe(
                OPERATION,
                &self.id,
                self.label(),
                ArgvRule::Exact,
                || match open_agents(&self.agents, OPERATION)? {
                    Some(directory) => read_stored(&directory, &file_name, OPERATION),
                    None => Ok(None),
                },
            )
            .await
    }

    async fn uninstall_agent(&self) -> Result<(), Error> {
        const OPERATION: &str = "uninstall";

        let file_name = definition_name(self.label());
        let directory = open_agents(&self.agents, OPERATION)?;
        // No definition an interrupted registration left set aside survives.
        if let Some(directory) = &directory {
            registration::recover(directory, self.label(), OPERATION)?;
        }
        let exit_timeout = match &directory {
            Some(directory) => stored_exit_timeout(directory, &file_name, OPERATION),
            None => MAX_JOB_TIMEOUT,
        };
        self.jobs
            .unload(OPERATION, self.label(), exit_timeout)
            .await?;
        if let Some(directory) = &directory {
            remove_file(directory, &file_name, OPERATION)?;
        }
        Ok(())
    }
}

impl DaemonSupervisor for LaunchdDaemon {
    /// Writes the agent definition and bootstraps it.
    ///
    /// A loaded daemon label is [`Error::AlreadyRegistered`] and its file is
    /// left untouched. A leftover agent file is replaced only once the new
    /// definition is kept, as in [`Supervisor::start`].
    fn install<'a>(&'a self, definition: &'a JobDefinition) -> Operation<'a, ()> {
        Box::pin(self.install_agent(definition))
    }

    /// Boots the agent out, waits until it is absent, then writes and
    /// bootstraps the new definition.
    ///
    /// A missing agents directory or agent file is [`Error::NotFound`] and
    /// leaves launchd untouched; an agent file that is not a private regular
    /// file is refused the same way.
    ///
    /// The previous agent file stays set aside until `bootstrap` returns.
    /// When launchd proves it loaded nothing (such as a missing `gui/<uid>`
    /// domain), the new file is removed and the previous one returns to its
    /// name, so launchd still loads the previous agent at the next login.
    ///
    /// A replacement interrupted after `bootout` is resumed by calling this
    /// again: an agent file left set aside returns to its name first when the
    /// agent file is missing, and is discarded when it is not.
    fn replace<'a>(&'a self, definition: &'a JobDefinition) -> Operation<'a, ()> {
        Box::pin(self.replace_agent(definition))
    }

    /// Matches the agent process by executable and exact arguments.
    fn inspect(&self) -> Operation<'_, ServiceObservation> {
        Box::pin(self.inspect_agent())
    }

    /// Boots the agent out and removes its definition, including one an
    /// interrupted registration left set aside.
    fn uninstall(&self) -> Operation<'_, ()> {
        Box::pin(self.uninstall_agent())
    }
}

/// Installs the persistent workflow plugin units of one installation
/// namespace in `gui/<uid>`.
///
/// Each unit is one job labeled
/// `io.github.zajca.pohunek.<ns>.wf.<plugin-hash>.<kind>.<name>` and defined by
/// `<agents>/<label>.plist`; production passes `~/Library/LaunchAgents`, so
/// launchd loads every installed unit again at login. Definitions are
/// written, replaced, and removed through the same crash-safe registration
/// as the daemon agent ([`registration::register`], [`registration::recover`]).
/// Errors about one unit report its label as the service ID.
///
/// A periodic job is loaded with `StartInterval` and `RunAtLoad=false`, so
/// its first run starts one interval after it is loaded and later runs follow
/// that cadence; launchd skips a firing while a run is active, so runs never
/// overlap. A service is loaded with `KeepAlive=true` and its restart delay as
/// `ThrottleInterval`. launchd has no key bounding a run, so the definition's
/// start timeout is not rendered and the unit's executable bounds its own
/// run; `ExitTimeOut` bounds a stop. See the `plist` module for every key.
///
/// A unit's process is matched by launchd parentage, executable, exact argv,
/// and launchd's `XPC_SERVICE_NAME` entry naming its label, so units of two
/// installations running the same argv are never mistaken for each other; a
/// process whose environment cannot be read is never matched. The worker
/// label grammar never matches these labels, so worker discovery neither
/// adopts, sweeps, nor reports them, even in a shared directory.
#[derive(Debug)]
pub struct LaunchdPluginUnits {
    namespace: Namespace,
    jobs: Jobs,
    agents: PathBuf,
}

impl LaunchdPluginUnits {
    /// Creates the plugin unit installer for `namespace` in `gui/<uid>`.
    ///
    /// `agents` is the `LaunchAgents` directory; `install` creates it `0700`
    /// when missing, and every operation refuses an existing one that the
    /// user does not own or that group or others can write, as for the daemon
    /// agent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDefinition`] for a relative directory or a zero
    /// deadline.
    pub fn new(
        namespace: Namespace,
        uid: u32,
        agents: PathBuf,
        command_deadline: Duration,
    ) -> Result<Self, Error> {
        require_absolute("LaunchAgents directory", &agents)?;
        Ok(Self {
            namespace,
            jobs: Jobs::new(uid, command_deadline)?,
            agents,
        })
    }

    /// Returns the label of `key` and the service ID errors report it under.
    fn label(&self, key: &PluginUnitKey) -> (String, ServiceId) {
        let label = self.namespace.plugin_label(key);
        let id = ServiceId::parse(label.as_str())
            .expect("plugin labels are bounded and use the service ID alphabet");
        (label, id)
    }

    /// Ends the loaded job of `label`, if any, with its process group.
    ///
    /// The stored definition bounds the wait and identifies the main process;
    /// an unusable one is never fatal, because a damaged unit must still stop.
    async fn end(
        &self,
        operation: &'static str,
        label: &str,
        directory: Option<&TrustedDir>,
    ) -> Result<Presence, Error> {
        let stored = directory
            .and_then(|directory| usable_stored(directory, &definition_name(label), operation))
            .filter(|stored| stored.label == label);
        self.jobs
            .end_job(operation, label, stored.as_ref(), labeled(label))
            .await
    }

    async fn install_unit(
        &self,
        key: &PluginUnitKey,
        definition: &PluginUnitDefinition,
    ) -> Result<(), Error> {
        const OPERATION: &str = "install_plugin_unit";

        if definition.kind() != key.kind() {
            return Err(Error::InvalidDefinition {
                detail: "the definition's kind differs from the unit key's".to_owned(),
            });
        }
        let (label, id) = self.label(key);
        let bytes = plist::render_plugin(&label, definition)?;
        let directory = create_agents(&self.agents, OPERATION)?;
        registration::recover(&directory, &label, OPERATION)?;
        // A replaced unit is stopped first, so no process or run of the
        // previous definition outlives the rewrite.
        self.end(OPERATION, &label, Some(&directory)).await?;
        self.jobs
            .register(OPERATION, &id, &label, &directory, &bytes)
            .await
    }

    /// Opens the agents directory and settles an interrupted registration of
    /// `label`; a missing directory or definition is [`Error::NotFound`].
    fn installed(
        &self,
        label: &str,
        id: &ServiceId,
        operation: &'static str,
    ) -> Result<TrustedDir, Error> {
        let directory =
            open_agents(&self.agents, operation)?.ok_or_else(|| Error::NotFound(id.clone()))?;
        registration::recover(&directory, label, operation)?;
        registration::require_definition(&directory, id, label, operation)?;
        Ok(directory)
    }

    async fn start_unit(&self, key: &PluginUnitKey) -> Result<(), Error> {
        const OPERATION: &str = "start_plugin_unit";

        let (label, id) = self.label(key);
        let directory = self.installed(&label, &id, OPERATION)?;
        if self.jobs.presence(OPERATION, &label).await? == Presence::Loaded {
            return Ok(());
        }
        // Only a definition this backend wrote for this label is loaded.
        let stored = read_stored(&directory, &definition_name(&label), OPERATION)?
            .ok_or_else(|| Error::NotFound(id.clone()))?;
        if stored.label != label {
            return Err(Error::InvalidData {
                operation: OPERATION,
                detail: "definition label differs from its file name".to_owned(),
            });
        }
        registration::load(
            &self.jobs.launchctl,
            &self.jobs.domain,
            OPERATION,
            &id,
            &label,
            &directory,
        )
        .await
    }

    async fn stop_unit(&self, key: &PluginUnitKey) -> Result<(), Error> {
        const OPERATION: &str = "stop_plugin_unit";

        let (label, id) = self.label(key);
        let directory = self.installed(&label, &id, OPERATION)?;
        self.end(OPERATION, &label, Some(&directory)).await?;
        Ok(())
    }

    async fn remove_unit(&self, key: &PluginUnitKey) -> Result<(), Error> {
        const OPERATION: &str = "remove_plugin_unit";

        let (label, _) = self.label(key);
        let directory = open_agents(&self.agents, OPERATION)?;
        // No definition an interrupted registration left set aside survives.
        if let Some(directory) = &directory {
            registration::recover(directory, &label, OPERATION)?;
        }
        self.end(OPERATION, &label, directory.as_ref()).await?;
        if let Some(directory) = &directory {
            remove_file(directory, &definition_name(&label), OPERATION)?;
        }
        Ok(())
    }

    async fn unit_state(&self, key: &PluginUnitKey) -> Result<ServiceState, Error> {
        const OPERATION: &str = "plugin_unit_state";

        let (label, id) = self.label(key);
        let file_name = definition_name(&label);
        let (stored, set_aside) = match open_agents(&self.agents, OPERATION)? {
            Some(directory) => (
                read_stored(&directory, &file_name, OPERATION)?,
                directory
                    .entry_identity(set_aside_name(&label), EntryKind::RegularFile)
                    .map_err(fs_error(OPERATION))?
                    .is_some(),
            ),
            None => (None, false),
        };
        let presence = self.jobs.presence(OPERATION, &label).await?;
        let Some(stored) = stored else {
            // A loaded label or a set-aside definition without the definition
            // itself is an interrupted registration, which the next `start`,
            // `stop`, `install`, or `remove` settles; this read never does.
            return match (presence, set_aside) {
                (Presence::Absent, false) => Err(Error::NotFound(id)),
                (Presence::Loaded, _) | (Presence::Absent, true) => Ok(ServiceState::Unknown),
            };
        };
        if stored.label != label {
            return Err(Error::InvalidData {
                operation: OPERATION,
                detail: "definition label differs from its file name".to_owned(),
            });
        }
        if presence == Presence::Absent {
            return Ok(ServiceState::Stopped);
        }
        if key.kind() == PluginUnitKind::Job {
            // A loaded job's schedule is armed, whether or not a run is
            // active now.
            return Ok(ServiceState::Running);
        }
        let directory = &self.agents;
        let observation = self
            .jobs
            .observe(
                OPERATION,
                &id,
                &label,
                labeled(&label),
                || match open_agents(directory, OPERATION)? {
                    Some(directory) => read_stored(&directory, &file_name, OPERATION),
                    None => Ok(None),
                },
            )
            .await;
        match observation {
            // Without a matching process the service is between two spawns,
            // throttled, or not spawned yet; launchd tells them apart only
            // in `print` text.
            Ok(observation) => Ok(observation.state),
            Err(Error::NotFound(_)) => Ok(ServiceState::Stopped),
            Err(Error::Race { .. }) => Ok(ServiceState::Unknown),
            Err(error) => Err(error),
        }
    }
}

impl PluginUnits for LaunchdPluginUnits {
    /// Writes `<label>.plist` and bootstraps it, after booting out and ending
    /// the job of a replaced definition.
    ///
    /// The kind is checked and the definition rendered before anything is
    /// written. A leftover definition stays set aside until `bootstrap`
    /// decides which file remains, as for the daemon agent.
    fn install<'a>(
        &'a self,
        key: &'a PluginUnitKey,
        definition: &'a PluginUnitDefinition,
    ) -> Operation<'a, ()> {
        Box::pin(self.install_unit(key, definition))
    }

    /// Bootstraps the installed definition; a loaded label is already
    /// started.
    fn start<'a>(&'a self, key: &'a PluginUnitKey) -> Operation<'a, ()> {
        Box::pin(self.start_unit(key))
    }

    /// Boots the job out and ends its process group, keeping the definition,
    /// so launchd loads it again at the next login.
    ///
    /// `bootout` unloads a job's schedule together with its run, so no new
    /// run starts in between.
    fn stop<'a>(&'a self, key: &'a PluginUnitKey) -> Operation<'a, ()> {
        Box::pin(self.stop_unit(key))
    }

    /// Boots the job out, ends its process group, and removes its definition,
    /// including one an interrupted registration left set aside.
    fn remove<'a>(&'a self, key: &'a PluginUnitKey) -> Operation<'a, ()> {
        Box::pin(self.remove_unit(key))
    }

    /// Combines the stored definition, `print` presence, and, for a service,
    /// process facts.
    ///
    /// | Definition | Label | Kind | Result |
    /// |---|---|---|---|
    /// | missing, nothing set aside | absent | any | [`Error::NotFound`] |
    /// | missing, set aside by an interrupted registration | absent | any | [`ServiceState::Unknown`] |
    /// | missing | loaded | any | [`ServiceState::Unknown`] |
    /// | present | absent | any | [`ServiceState::Stopped`] |
    /// | present | loaded | job | [`ServiceState::Running`] (schedule armed) |
    /// | present | loaded | service, matching process | [`ServiceState::Running`] |
    /// | present | loaded | service, no matching process or a race | [`ServiceState::Unknown`] |
    ///
    /// launchd reports an exit status only in `print` text, which is never
    /// parsed, so [`ServiceState::Failed`] is never reported.
    fn state<'a>(&'a self, key: &'a PluginUnitKey) -> Operation<'a, ServiceState> {
        Box::pin(self.unit_state(key))
    }
}

/// Which process arguments identify a job's process.
#[derive(Debug, Clone)]
enum ArgvRule {
    /// A worker: argv carries `--session-id <id>` and
    /// `--worker-generation <generation>` as adjacent pairs.
    Worker {
        session_id: String,
        generation: String,
    },
    /// The daemon: argv after the executable equals the stored arguments,
    /// which name the installation's service configuration.
    Exact,
    /// A plugin unit: argv after the executable equals the stored arguments
    /// and the process environment's [`SERVICE_NAME_VARIABLE`] is `label`.
    Labeled { label: String },
}

/// Returns the rule binding a plugin unit's process to `label`.
fn labeled(label: &str) -> ArgvRule {
    ArgvRule::Labeled {
        label: label.to_owned(),
    }
}

impl ArgvRule {
    fn matches_arguments(&self, arguments: &[String]) -> bool {
        match self {
            Self::Worker {
                session_id,
                generation,
            } => {
                has_pair(arguments, SESSION_ID_ARGUMENT, session_id)
                    && has_pair(arguments, GENERATION_ARGUMENT, generation)
            }
            Self::Exact | Self::Labeled { .. } => true,
        }
    }
}

fn has_pair(arguments: &[String], flag: &str, value: &str) -> bool {
    arguments
        .windows(2)
        .any(|pair| pair[0] == flag && pair[1] == value)
}

/// Process facts a job's process must match.
#[derive(Debug)]
struct Expected {
    executable: PathBuf,
    /// `executable` with symlinks resolved, which is what the kernel reports.
    canonical: Option<PathBuf>,
    arguments: Vec<String>,
    rule: ArgvRule,
}

impl Expected {
    fn new(stored: &Stored, rule: ArgvRule) -> Self {
        Self {
            canonical: std::fs::canonicalize(&stored.facts.executable).ok(),
            executable: stored.facts.executable.clone(),
            arguments: stored.facts.arguments.clone(),
            rule,
        }
    }

    fn matches(&self, fact: &ProcessFact) -> bool {
        if fact.ppid != LAUNCHD_PID {
            return false;
        }
        let Some((_, arguments)) = fact.cmdline.split_first() else {
            return false;
        };
        match &self.rule {
            ArgvRule::Worker { .. } => self.rule.matches_arguments(arguments),
            ArgvRule::Exact | ArgvRule::Labeled { .. } => arguments == self.arguments.as_slice(),
        }
    }

    /// Whether `path` is the job's executable, or that file in the version
    /// directory a repair moved aside ([`pohunek_paths::is_set_aside_copy`]).
    fn matches_executable(&self, path: &Path) -> bool {
        path == self.executable
            || self.canonical.as_deref() == Some(path)
            || self
                .canonical
                .as_deref()
                .is_some_and(|canonical| pohunek_paths::is_set_aside_copy(canonical, path))
    }
}

/// launchctl access shared by both backends for one `gui/<uid>` domain.
#[derive(Debug)]
struct Jobs {
    domain: String,
    launchctl: Launchctl,
    inspector: DarwinInspector,
}

impl Jobs {
    fn new(uid: u32, command_deadline: Duration) -> Result<Self, Error> {
        if command_deadline.is_zero() {
            return Err(Error::InvalidDefinition {
                detail: "launchctl command deadline must be positive".to_owned(),
            });
        }
        Ok(Self {
            domain: format!("gui/{uid}"),
            launchctl: Launchctl::new(command_deadline),
            inspector: DarwinInspector::new(),
        })
    }

    fn target(&self, label: &str) -> String {
        format!("{}/{label}", self.domain)
    }

    async fn presence(&self, operation: &'static str, label: &str) -> Result<Presence, Error> {
        registration::probe(&self.launchctl, &self.domain, label, operation).await
    }

    /// Writes `<label>.plist` into `directory` and bootstraps it; see
    /// [`registration::register`].
    async fn register(
        &self,
        operation: &'static str,
        id: &ServiceId,
        label: &str,
        directory: &TrustedDir,
        bytes: &[u8],
    ) -> Result<(), Error> {
        registration::register(
            &self.launchctl,
            &self.domain,
            operation,
            id,
            label,
            directory,
            bytes,
        )
        .await
    }

    /// Boots `label` out and waits until `print` reports it absent.
    ///
    /// Returns whether the label was loaded when `bootout` ran.
    async fn unload(
        &self,
        operation: &'static str,
        label: &str,
        exit_timeout: Duration,
    ) -> Result<Presence, Error> {
        let target = self.target(label);
        // `bootout` waits for the job to exit, which launchd bounds by
        // `ExitTimeOut` before it sends `SIGKILL`.
        let completion = self
            .launchctl
            .run_within(
                "bootout",
                &[OsStr::new(&target)],
                self.launchctl.deadline() + exit_timeout,
            )
            .await
            .map_err(|error| run_error(operation, error))?;
        let before = match completion.status {
            Status::Success => Presence::Loaded,
            Status::NoSuchProcess | Status::NoSuchService => Presence::Absent,
            Status::InputOutput | Status::InProgress | Status::NoSuchDomain | Status::Unmapped => {
                return Err(status_error(operation, &self.domain, &completion));
            }
        };
        let deadline = Instant::now() + exit_timeout + ABSENCE_MARGIN;
        while self.presence(operation, label).await? == Presence::Loaded {
            if Instant::now() >= deadline {
                return Err(Error::Timeout { operation });
            }
            tokio::time::sleep(ABSENCE_POLL).await;
        }
        Ok(before)
    }

    /// Boots `label` out and ends the rest of its process group, waiting
    /// until both are gone.
    ///
    /// launchd sends the main process `SIGTERM`, then `SIGKILL` after
    /// `ExitTimeOut`. With `AbandonProcessGroup=false` it only sends `SIGTERM`
    /// to the remaining process group once the main process is gone, so a
    /// member that ignores it would outlive the job (observed on macOS 15).
    /// The group of the process matching `stored` is therefore captured
    /// before `bootout`, and its members get `SIGTERM` and, once `ExitTimeOut`
    /// has passed since `bootout` started, `SIGKILL`. Without a usable
    /// `stored` definition the wait is bounded by [`MAX_JOB_TIMEOUT`] and no
    /// group is captured. Returns whether the label was loaded.
    ///
    /// A process launchd spawns between the capture and `bootout` (an
    /// interval firing or a `KeepAlive` respawn) is not captured: once
    /// `bootout` returns, its main process is gone, so its group can no
    /// longer be found, and a member that ignores launchd's `SIGTERM`
    /// survives. A job's executable that ends its own children on
    /// `SIGTERM` closes that window.
    async fn end_job(
        &self,
        operation: &'static str,
        label: &str,
        stored: Option<&Stored>,
        rule: ArgvRule,
    ) -> Result<Presence, Error> {
        let exit_timeout = stored.map_or(MAX_JOB_TIMEOUT, |stored| {
            stored.exit_timeout.min(MAX_JOB_TIMEOUT)
        });
        let members = match stored {
            Some(stored) => {
                self.group_members(operation, Expected::new(stored, rule))
                    .await?
            }
            None => Vec::new(),
        };
        let stopping = Instant::now();
        let before = self.unload(operation, label, exit_timeout).await?;
        self.end_group(operation, &members, stopping + exit_timeout)
            .await?;
        Ok(before)
    }

    /// Observes one loaded job from `print`, its stored definition, and the
    /// process table, rejecting a job that changed during the observation.
    async fn observe(
        &self,
        operation: &'static str,
        id: &ServiceId,
        label: &str,
        rule: ArgvRule,
        stored: impl FnOnce() -> Result<Option<Stored>, Error>,
    ) -> Result<ServiceObservation, Error> {
        if self.presence(operation, label).await? == Presence::Absent {
            return Err(Error::NotFound(id.clone()));
        }
        let Some(stored) = stored()? else {
            // A retire boots the job out before it removes the definition, so
            // a job that vanished after the first probe is gone, not invalid.
            if self.presence(operation, label).await? == Presence::Absent {
                return Err(Error::NotFound(id.clone()));
            }
            return Err(Error::InvalidData {
                operation,
                detail: "loaded job has no private definition".to_owned(),
            });
        };
        if stored.label != label {
            return Err(Error::InvalidData {
                operation,
                detail: "definition label differs from its file name".to_owned(),
            });
        }
        let expected = Expected::new(&stored, rule);
        let process = self.find_process(operation, expected).await?;
        if self.presence(operation, label).await? == Presence::Absent {
            return Err(Error::Race { operation });
        }
        if let Some(identity) = process {
            if self
                .inspector
                .identity(identity.pid)
                .map_err(|error| process_error(operation, error))?
                != Some(identity)
            {
                return Err(Error::Race { operation });
            }
        }
        Ok(ServiceObservation {
            id: id.clone(),
            // A loaded label without a matching process is not proven ended.
            state: if process.is_some() {
                ServiceState::Running
            } else {
                ServiceState::Unknown
            },
            process,
            definition: Some(stored.facts),
        })
    }

    /// Runs blocking process inspection off the async executor.
    async fn blocking<T: Send + 'static>(
        &self,
        operation: &'static str,
        work: impl FnOnce() -> Result<T, crate::process::Error> + Send + 'static,
    ) -> Result<T, Error> {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|error| Error::Unavailable {
                operation,
                source: Box::new(error),
            })?
            .map_err(|error| process_error(operation, error))
    }

    /// Captures the process group of the single process matching `expected`.
    ///
    /// launchd makes each job's main process a session and process-group
    /// leader, so the group ID is the main PID. Without a main process nothing
    /// is captured; several matching processes are [`Error::InvalidData`], as
    /// in [`Jobs::observe`], because the job's group is then unknown.
    async fn group_members(
        &self,
        operation: &'static str,
        expected: Expected,
    ) -> Result<Vec<ProcessIdentity>, Error> {
        let inspector = self.inspector;
        let (facts, matches) = self
            .blocking(operation, move || {
                let facts = inspector.same_user_processes()?;
                let matches = scan(inspector, &facts, &expected)?;
                Ok((facts, matches))
            })
            .await?;
        let Some(main) = single_process(operation, &matches)? else {
            return Ok(Vec::new());
        };
        Ok(facts
            .iter()
            .filter(|fact| fact.pgid == main.pid)
            .map(ProcessFact::identity)
            .collect())
    }

    /// Ends captured process-group members that outlived their job.
    ///
    /// Survivors get `SIGTERM`, then `SIGKILL` at `kill_at`, and must be gone
    /// within [`ABSENCE_MARGIN`] after that.
    async fn end_group(
        &self,
        operation: &'static str,
        members: &[ProcessIdentity],
        kill_at: Instant,
    ) -> Result<(), Error> {
        if self.signal_running(operation, members, Signal::TERM)? == 0 {
            return Ok(());
        }
        while Instant::now() < kill_at {
            tokio::time::sleep(ABSENCE_POLL).await;
            if self.running(operation, members)? == 0 {
                return Ok(());
            }
        }
        self.signal_running(operation, members, Signal::KILL)?;
        let deadline = Instant::now() + ABSENCE_MARGIN;
        while self.running(operation, members)? > 0 {
            if Instant::now() >= deadline {
                return Err(Error::Timeout { operation });
            }
            tokio::time::sleep(ABSENCE_POLL).await;
        }
        Ok(())
    }

    /// Counts the members that are still running.
    fn running(
        &self,
        operation: &'static str,
        members: &[ProcessIdentity],
    ) -> Result<usize, Error> {
        let mut running = 0;
        for member in members {
            if self
                .inspector
                .is_running(*member)
                .map_err(|error| process_error(operation, error))?
            {
                running += 1;
            }
        }
        Ok(running)
    }

    /// Signals every member that is still running; returns how many were.
    ///
    /// Darwin has no process descriptors, so the start identity is checked
    /// immediately before `kill`; PID reuse within that window would need the
    /// whole PID space to wrap between two system calls.
    fn signal_running(
        &self,
        operation: &'static str,
        members: &[ProcessIdentity],
        signal: Signal,
    ) -> Result<usize, Error> {
        let mut signalled = 0;
        for member in members {
            if !self
                .inspector
                .is_running(*member)
                .map_err(|error| process_error(operation, error))?
            {
                continue;
            }
            let pid = i32::try_from(member.pid)
                .ok()
                .and_then(NativePid::from_raw)
                .ok_or_else(|| Error::InvalidData {
                    operation,
                    detail: format!("process id {} is out of range", member.pid),
                })?;
            match kill_process(pid, signal) {
                Ok(()) => signalled += 1,
                Err(rustix::io::Errno::SRCH) => {}
                Err(error) => {
                    return Err(Error::Operation {
                        operation,
                        source: Box::new(std::io::Error::from(error)),
                    });
                }
            }
        }
        Ok(signalled)
    }

    /// Finds the single same-user process launchd spawned for `expected`.
    async fn find_process(
        &self,
        operation: &'static str,
        expected: Expected,
    ) -> Result<Option<ProcessIdentity>, Error> {
        let inspector = self.inspector;
        let matches = self
            .blocking(operation, move || {
                scan(inspector, &inspector.same_user_processes()?, &expected)
            })
            .await?;
        single_process(operation, &matches)
    }
}

/// Returns the only process matching one job, rejecting ambiguous matches.
fn single_process(
    operation: &'static str,
    matches: &[ProcessIdentity],
) -> Result<Option<ProcessIdentity>, Error> {
    match matches {
        [] => Ok(None),
        [identity] => Ok(Some(*identity)),
        _ => Err(Error::InvalidData {
            operation,
            detail: format!("{} processes match one job", matches.len()),
        }),
    }
}

/// Selects the launchd children in `facts` matching `expected`.
fn scan(
    inspector: DarwinInspector,
    facts: &[ProcessFact],
    expected: &Expected,
) -> Result<Vec<ProcessIdentity>, crate::process::Error> {
    let mut matches = Vec::new();
    for fact in facts {
        if !expected.matches(fact) {
            continue;
        }
        let Some(executable) = inspector.executable(fact.pid)? else {
            continue;
        };
        if !expected.matches_executable(&executable) {
            continue;
        }
        if let ArgvRule::Labeled { label } = &expected.rule {
            match environment_entry_is(fact.pid, SERVICE_NAME_VARIABLE, label) {
                Ok(true) => {}
                // Another job's process, possibly of another installation
                // running the same argv, or one whose environment cannot be
                // read: that never proves the process is this job's, so it is
                // neither reported nor signalled as the job.
                Ok(false) | Err(crate::process::Error::Unobservable { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
        // An `exec` between the table read and the path and environment reads
        // would pair one image's evidence with another's facts, so the
        // identity must still hold.
        if inspector.identity(fact.pid)? == Some(fact.identity()) {
            matches.push(fact.identity());
        }
    }
    Ok(matches)
}

fn process_error(operation: &'static str, error: crate::process::Error) -> Error {
    if error.is_race() {
        Error::Race { operation }
    } else {
        Error::Unavailable {
            operation,
            source: Box::new(error),
        }
    }
}

fn require_absolute(field: &str, path: &Path) -> Result<(), Error> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(Error::InvalidDefinition {
            detail: format!("{field} must be absolute"),
        })
    }
}

/// Opens an existing `LaunchAgents` directory; a missing one is `None`.
fn open_agents(agents: &Path, operation: &'static str) -> Result<Option<TrustedDir>, Error> {
    match TrustedDir::open_absolute_owner_safe(agents, AGENTS_DIR_FORBIDDEN_BITS) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => Ok(None),
        Err(error) => Err(fs_error(operation)(error)),
    }
}

/// Opens the `LaunchAgents` directory, creating a missing one `0700`.
fn create_agents(agents: &Path, operation: &'static str) -> Result<TrustedDir, Error> {
    match open_agents(agents, operation)? {
        Some(directory) => Ok(directory),
        None => TrustedDir::open_or_create_absolute(agents, PRIVATE_DIR_MODE)
            .map_err(fs_error(operation)),
    }
}

/// Opens an existing private directory; a missing one is `None`.
fn open_private(path: &Path, operation: &'static str) -> Result<Option<TrustedDir>, Error> {
    match TrustedDir::open_absolute(path, PRIVATE_DIR_MODE) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => Ok(None),
        Err(error) => Err(fs_error(operation)(error)),
    }
}

/// Reads and parses a stored definition; a missing file is `None`.
///
/// A file that is not a usable definition is [`Error::InvalidData`].
fn read_stored(
    directory: &TrustedDir,
    file_name: &str,
    operation: &'static str,
) -> Result<Option<Stored>, Error> {
    let bytes = match directory.read_file(file_name, DEFINITION_MODE, MAX_DEFINITION_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => return Ok(None),
        Err(error) => return Err(fs_error(operation)(error)),
    };
    plist::parse(&bytes)
        .map(Some)
        .map_err(|error| Error::InvalidData {
            operation,
            detail: error.to_string(),
        })
}

/// Returns a stored definition, or `None` when it is missing or unusable.
///
/// Unloading must never depend on a readable definition: a job whose file was
/// damaged still has to be retired, bounded by [`MAX_JOB_TIMEOUT`].
fn usable_stored(
    directory: &TrustedDir,
    file_name: &str,
    operation: &'static str,
) -> Option<Stored> {
    read_stored(directory, file_name, operation).ok().flatten()
}

/// Returns the stored `ExitTimeOut`, or [`MAX_JOB_TIMEOUT`] when the
/// definition is missing or unusable.
///
/// The largest timeout any valid definition can carry bounds the wait without
/// guessing a shorter one.
fn stored_exit_timeout(
    directory: &TrustedDir,
    file_name: &str,
    operation: &'static str,
) -> Duration {
    usable_stored(directory, file_name, operation).map_or(MAX_JOB_TIMEOUT, |stored| {
        stored.exit_timeout.min(MAX_JOB_TIMEOUT)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(ppid: Pid, cmdline: &[&str]) -> ProcessFact {
        ProcessFact {
            pid: 4_242,
            pgid: 4_242,
            ppid,
            start_identity: crate::process::StartIdentity::new(1),
            comm: "pohunek-sessiond".to_owned(),
            cmdline: cmdline.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    fn worker() -> Expected {
        Expected {
            executable: PathBuf::from("/opt/pohunek/bin/pohunek-sessiond"),
            canonical: Some(PathBuf::from("/private/opt/pohunek/bin/pohunek-sessiond")),
            arguments: Vec::new(),
            rule: ArgvRule::Worker {
                session_id: "s-42".to_owned(),
                generation: "abcd2345".to_owned(),
            },
        }
    }

    #[test]
    fn worker_processes_need_launchd_parent_and_both_argument_pairs() {
        let expected = worker();
        let exe = "/opt/pohunek/bin/pohunek-sessiond";
        assert!(expected.matches(&fact(
            1,
            &[
                exe,
                "--session-id",
                "s-42",
                "--worker-generation",
                "abcd2345"
            ]
        )));
        assert!(!expected.matches(&fact(
            77,
            &[
                exe,
                "--session-id",
                "s-42",
                "--worker-generation",
                "abcd2345"
            ]
        )));
        assert!(!expected.matches(&fact(
            1,
            &[
                exe,
                "--session-id",
                "s-42",
                "--worker-generation",
                "abcd2346"
            ]
        )));
        assert!(!expected.matches(&fact(1, &[exe, "--session-id", "s-42"])));
        // The executable slot never satisfies an argument pair.
        assert!(!expected.matches(&fact(
            1,
            &["--session-id", "s-42", "--worker-generation", "abcd2345"]
        )));
        assert!(!expected.matches(&fact(1, &[])));
        assert!(expected.matches_executable(Path::new("/opt/pohunek/bin/pohunek-sessiond")));
        assert!(expected.matches_executable(Path::new("/private/opt/pohunek/bin/pohunek-sessiond")));
        assert!(!expected.matches_executable(Path::new("/bin/sh")));
        // The version directory a repair moved aside is still the job's image.
        let (versions, aside) = (
            "/private/opt/pohunek/libexec/pohunek",
            ".pohunek_refused-1.0.0-0123456789abcdef",
        );
        let expected = Expected {
            canonical: Some(PathBuf::from(format!("{versions}/1.0.0/pohunek-sessiond"))),
            ..expected
        };
        assert!(
            expected.matches_executable(Path::new(&format!("{versions}/{aside}/pohunek-sessiond")))
        );
        assert!(!expected.matches_executable(Path::new(&format!(
            "{versions}/.pohunek_refused-2.0.0-0123456789abcdef/pohunek-sessiond"
        ))));
    }

    #[test]
    fn daemon_processes_need_exact_arguments() {
        let expected = Expected {
            executable: PathBuf::from("/opt/pohunek/bin/pohunekd"),
            canonical: None,
            arguments: vec!["--service-config".to_owned(), "/x/service.toml".to_owned()],
            rule: ArgvRule::Exact,
        };
        let exe = "/opt/pohunek/bin/pohunekd";
        assert!(expected.matches(&fact(1, &[exe, "--service-config", "/x/service.toml"])));
        assert!(!expected.matches(&fact(
            1,
            &[exe, "--service-config", "/x/service.toml", "--extra"]
        )));
        assert!(!expected.matches(&fact(1, &[exe])));
    }

    #[test]
    fn ambiguous_process_matches_are_invalid_data() {
        let first = fact(1, &[]).identity();
        let second = ProcessFact {
            pid: 4_243,
            pgid: 4_243,
            ..fact(1, &[])
        }
        .identity();
        assert_eq!(single_process("retire", &[]).expect("no match"), None);
        assert_eq!(
            single_process("retire", &[first]).expect("one match"),
            Some(first)
        );
        assert!(matches!(
            single_process("retire", &[first, second]),
            Err(Error::InvalidData {
                operation: "retire",
                ..
            })
        ));
    }
}
