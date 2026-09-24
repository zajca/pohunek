//! launchd worker supervisor and daemon agent backends.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::super::{
    DaemonSupervisor, Error, JobDefinition, JobLogs, Namespace, Operation, RestartPolicy,
    ServiceId, ServiceObservation, ServiceState, Supervisor, WorkerKey, MAX_JOB_TIMEOUT,
};
use super::launchctl::{Launchctl, Status};
use super::plist::{self, Stored, MAX_DEFINITION_BYTES};
use super::{presence, run_error, status_error, Presence};
use crate::filesystem::{EntryKind, FsError, RemoveOutcome, StageOutcome, TrustedDir};
use crate::process::{DarwinInspector, Pid, ProcessFact, ProcessIdentity, ProcessInspector as _};
use rustix::process::{kill_process, Pid as NativePid, Signal};

// Rust guideline compliant 2026-09-24

/// PID of launchd; every job launchd spawns is its direct child.
const LAUNCHD_PID: Pid = 1;

/// Mode of the private worker definition and log directories.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Mode of every definition file; launchd refuses group- or world-writable plists.
const DEFINITION_MODE: u32 = 0o600;

/// Bits that make a directory unsafe to hold the daemon agent definition.
///
/// `~/Library/LaunchAgents` is commonly `0755`; only write access for others
/// would let another account substitute the agent.
const AGENTS_DIR_FORBIDDEN_BITS: u32 = 0o022;

/// File-name suffix of every definition.
const DEFINITION_SUFFIX: &str = ".plist";

/// Suffix of a worker's launchd standard-output log.
const STDOUT_SUFFIX: &str = ".out.log";

/// Suffix of a worker's launchd standard-error log.
const STDERR_SUFFIX: &str = ".err.log";

/// Prefix of the temporary name an atomic definition write starts from.
///
/// Starts with `.` and never ends in [`DEFINITION_SUFFIX`], so discovery never
/// mistakes an interrupted write for a definition.
const TEMPORARY_PREFIX: &str = ".pohunek-launchd-";

/// Prefix of the quarantine name a removed file passes through.
const REMOVAL_PREFIX: &str = ".pohunek-launchd-removed-";

/// Random bytes in a temporary definition name.
const TEMPORARY_RANDOM_BYTES: usize = 8;

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

/// Mount point of the boot volume group's data volume.
///
/// Since macOS 10.15 the home directories on the boot disk live on this
/// volume (firmlinked into `/Users`), while `/` is the sealed system volume.
const BOOT_DATA_VOLUME: &str = "/System/Volumes/Data";

/// Worker argument introducing the session ID.
const SESSION_ID_ARGUMENT: &str = "--session-id";

/// Worker argument introducing the runtime generation.
const GENERATION_ARGUMENT: &str = "--worker-generation";

/// launchd refused a definition stored outside the boot volume group.
///
/// launchd does not load job definitions from external volumes. The backend
/// reports this instead of silently storing definitions somewhere else; it is
/// the source of an [`Error::Unavailable`] from `start` or `install`.
#[derive(Debug, thiserror::Error)]
#[error(
    "launchd refused the definition directory {path} because it is not on the boot volume",
    path = .path.display()
)]
pub struct ExternalVolume {
    /// The definition directory.
    pub path: PathBuf,
}

/// Result of [`LaunchdSupervisor::discover_definitions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// Loaded jobs of this namespace.
    pub observations: Vec<ServiceObservation>,
    /// Definition file names of this namespace that were skipped because
    /// they do not parse or their `Label` differs from the file name. They
    /// are never loaded.
    pub rejected: Vec<String>,
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
    /// and inspects each. Files that do not parse or whose `Label` differs from
    /// the file name are listed in [`Discovery::rejected`]. Jobs that vanish or
    /// change during inspection are skipped.
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
        let mut candidates = Vec::new();
        for name in names {
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(label) = name.strip_suffix(DEFINITION_SUFFIX) else {
                continue;
            };
            let Ok(key) = self.namespace.parse_worker_label(label) else {
                continue;
            };
            match directory.read_file(name, DEFINITION_MODE, MAX_DEFINITION_BYTES) {
                Ok(bytes) => match plist::parse(&bytes) {
                    Ok(stored) if stored.label == label => candidates.push(key),
                    Ok(_) | Err(_) => discovery.rejected.push(name.to_owned()),
                },
                // Retired between enumeration and read.
                Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {}
                // Not a file this backend wrote.
                Err(
                    FsError::UnsafeType { .. }
                    | FsError::UnsafeOwner { .. }
                    | FsError::UnsafeMode { .. }
                    | FsError::UnsafeAcl { .. }
                    | FsError::UnsafeLinkCount { .. }
                    | FsError::FileTooLarge { .. },
                ) => discovery.rejected.push(name.to_owned()),
                Err(error) => return Err(fs_error(OPERATION)(error)),
            }
        }
        drop(directory);
        for key in candidates {
            match self.inspect_worker(&key.service_id()).await {
                Ok(observation) => discovery.observations.push(observation),
                Err(Error::NotFound(_) | Error::Race { .. }) => {}
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

    async fn retire_worker(&self, id: &ServiceId) -> Result<(), Error> {
        const OPERATION: &str = "retire";

        let key = WorkerKey::from_service_id(id)?;
        let label = self.namespace.worker_label(&key);
        let file_name = definition_name(&label);
        let directory = open_private(&self.definitions, OPERATION)?;
        let stored = directory
            .as_ref()
            .and_then(|directory| usable_stored(directory, &file_name, OPERATION))
            .filter(|stored| stored.label == label);
        let exit_timeout = stored.as_ref().map_or(MAX_JOB_TIMEOUT, |stored| {
            stored.exit_timeout.min(MAX_JOB_TIMEOUT)
        });
        let members = match &stored {
            Some(stored) => {
                let rule = ArgvRule::Worker {
                    session_id: key.session_id().to_owned(),
                    generation: key.generation().to_owned(),
                };
                self.jobs
                    .group_members(OPERATION, Expected::new(stored, rule))
                    .await?
            }
            None => Vec::new(),
        };
        let stopping = Instant::now();
        let before = self.jobs.unload(OPERATION, &label, exit_timeout).await?;
        self.jobs
            .end_group(OPERATION, &members, stopping + exit_timeout)
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
    /// Only after the label is proven absent is the definition atomically
    /// written (a leftover file of an absent label is replaced) and
    /// bootstrapped. A concurrent `start` of the very same generation between
    /// the probe and the write is the only way to overwrite a loaded job's
    /// file, and it would write identical bytes.
    fn start<'a>(&'a self, id: &'a ServiceId, definition: &'a JobDefinition) -> Operation<'a, ()> {
        Box::pin(self.start_worker(id, definition))
    }

    /// Returns [`Discovery::observations`] and logs a warning per
    /// [`Discovery::rejected`] file name.
    fn discover(&self) -> Operation<'_, Vec<ServiceObservation>> {
        Box::pin(async move {
            let discovery = self.discover_definitions().await?;
            super::super::warn_rejected("launchd", &discovery.rejected);
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
    /// files removed and returns [`Error::NotFound`].
    fn retire<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ()> {
        Box::pin(self.retire_worker(id))
    }
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
    /// `agents` is the `LaunchAgents` directory, created `0700` when missing.
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
        }
    }

    /// Opens an existing agents directory; a missing one is `None`.
    fn open_agents(&self, operation: &'static str) -> Result<Option<TrustedDir>, Error> {
        match TrustedDir::open_absolute_owner_safe(&self.agents, AGENTS_DIR_FORBIDDEN_BITS) {
            Ok(directory) => Ok(Some(directory)),
            Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => Ok(None),
            Err(error) => Err(fs_error(operation)(error)),
        }
    }

    /// Opens the agents directory, creating a missing one `0700`.
    fn create_agents(&self, operation: &'static str) -> Result<TrustedDir, Error> {
        match self.open_agents(operation)? {
            Some(directory) => Ok(directory),
            None => TrustedDir::open_or_create_absolute(&self.agents, PRIVATE_DIR_MODE)
                .map_err(fs_error(operation)),
        }
    }

    async fn install_agent(&self, definition: &JobDefinition) -> Result<(), Error> {
        const OPERATION: &str = "install";

        Self::validate(definition)?;
        let bytes = plist::render(self.label(), definition)?;
        let directory = self.create_agents(OPERATION)?;
        self.jobs
            .register(OPERATION, &self.id, self.label(), &directory, &bytes)
            .await
    }

    async fn replace_agent(&self, definition: &JobDefinition) -> Result<(), Error> {
        const OPERATION: &str = "replace";

        Self::validate(definition)?;
        let bytes = plist::render(self.label(), definition)?;
        let directory = self.create_agents(OPERATION)?;
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
                || match self.open_agents(OPERATION)? {
                    Some(directory) => read_stored(&directory, &file_name, OPERATION),
                    None => Ok(None),
                },
            )
            .await
    }

    async fn uninstall_agent(&self) -> Result<(), Error> {
        const OPERATION: &str = "uninstall";

        let file_name = definition_name(self.label());
        let directory = self.open_agents(OPERATION)?;
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
    /// left untouched.
    fn install<'a>(&'a self, definition: &'a JobDefinition) -> Operation<'a, ()> {
        Box::pin(self.install_agent(definition))
    }

    /// Boots the agent out, waits until it is absent, then writes and
    /// bootstraps the new definition.
    fn replace<'a>(&'a self, definition: &'a JobDefinition) -> Operation<'a, ()> {
        Box::pin(self.replace_agent(definition))
    }

    /// Matches the agent process by executable and exact arguments.
    fn inspect(&self) -> Operation<'_, ServiceObservation> {
        Box::pin(self.inspect_agent())
    }

    fn uninstall(&self) -> Operation<'_, ()> {
        Box::pin(self.uninstall_agent())
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
    /// The daemon: argv after the executable equals the stored arguments.
    Exact,
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
            Self::Exact => true,
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
            ArgvRule::Exact => arguments == self.arguments.as_slice(),
        }
    }

    fn matches_executable(&self, path: &Path) -> bool {
        path == self.executable || self.canonical.as_deref() == Some(path)
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
        let target = self.target(label);
        let completion = self
            .launchctl
            .run("print", &[OsStr::new(&target)])
            .await
            .map_err(|error| run_error(operation, error))?;
        presence(operation, &self.domain, &completion)
    }

    /// Writes `<label>.plist` into `directory` and bootstraps it.
    async fn register(
        &self,
        operation: &'static str,
        id: &ServiceId,
        label: &str,
        directory: &TrustedDir,
        bytes: &[u8],
    ) -> Result<(), Error> {
        if self.presence(operation, label).await? == Presence::Loaded {
            return Err(Error::AlreadyRegistered(id.clone()));
        }
        let file_name = definition_name(label);
        write_definition(directory, &file_name, bytes, operation)?;
        let path = directory.path().join(&file_name);
        let completion = self
            .launchctl
            .run("bootstrap", &[OsStr::new(&self.domain), path.as_os_str()])
            .await
            .map_err(|error| run_error(operation, error))?;
        match completion.status {
            Status::Success => Ok(()),
            // `EIO` covers both an already loaded label and a definition
            // launchd refused to read; `print` tells them apart.
            Status::InputOutput => {
                if self.presence(operation, label).await? == Presence::Loaded {
                    return Err(Error::AlreadyRegistered(id.clone()));
                }
                remove_file(directory, &file_name, operation)?;
                if !on_boot_volume(directory, operation)? {
                    return Err(Error::Unavailable {
                        operation,
                        source: Box::new(ExternalVolume {
                            path: directory.path().to_path_buf(),
                        }),
                    });
                }
                Err(status_error(operation, &self.domain, &completion))
            }
            Status::NoSuchDomain => {
                remove_file(directory, &file_name, operation)?;
                Err(status_error(operation, &self.domain, &completion))
            }
            // The label's state is unknown, so its definition stays for
            // discovery and retirement to reconcile.
            Status::NoSuchProcess
            | Status::InProgress
            | Status::NoSuchService
            | Status::Unmapped => Err(status_error(operation, &self.domain, &completion)),
        }
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
        let stored = stored()?.ok_or_else(|| Error::InvalidData {
            operation,
            detail: "loaded job has no private definition".to_owned(),
        })?;
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
    /// leader, so the group ID is the main PID. Without exactly one main
    /// process nothing is captured.
    async fn group_members(
        &self,
        operation: &'static str,
        expected: Expected,
    ) -> Result<Vec<ProcessIdentity>, Error> {
        let inspector = self.inspector;
        self.blocking(operation, move || {
            let facts = inspector.same_user_processes()?;
            let [main] = scan(inspector, &facts, &expected)?[..] else {
                return Ok(Vec::new());
            };
            Ok(facts
                .iter()
                .filter(|fact| fact.pgid == main.pid)
                .map(ProcessFact::identity)
                .collect())
        })
        .await
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
        match matches.as_slice() {
            [] => Ok(None),
            [identity] => Ok(Some(*identity)),
            _ => Err(Error::InvalidData {
                operation,
                detail: format!("{} processes match one job", matches.len()),
            }),
        }
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
        // An `exec` between the table read and the path read would pair one
        // image's path with another's facts, so the identity must still hold.
        if expected.matches_executable(&executable)
            && inspector.identity(fact.pid)? == Some(fact.identity())
        {
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

fn fs_error(operation: &'static str) -> impl Fn(FsError) -> Error {
    move |error| Error::Operation {
        operation,
        source: Box::new(error),
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

fn definition_name(label: &str) -> String {
    format!("{label}{DEFINITION_SUFFIX}")
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

/// Atomically writes a `0600` definition through a random temporary name.
fn write_definition(
    directory: &TrustedDir,
    file_name: &str,
    bytes: &[u8],
    operation: &'static str,
) -> Result<(), Error> {
    let mut random = [0_u8; TEMPORARY_RANDOM_BYTES];
    getrandom::getrandom(&mut random).map_err(|error| Error::Unavailable {
        operation,
        source: Box::new(std::io::Error::other(error.to_string())),
    })?;
    let mut temporary = String::from(TEMPORARY_PREFIX);
    for byte in random {
        use std::fmt::Write as _;
        write!(temporary, "{byte:02x}").expect("writing hexadecimal to String cannot fail");
    }
    temporary.push_str(".tmp");
    directory
        .replace_file(file_name, &temporary, bytes, DEFINITION_MODE)
        .map_err(|error| Error::Operation {
            operation,
            source: Box::new(error),
        })
}

/// Removes one regular file bound to the inode that was inspected.
fn remove_file(
    directory: &TrustedDir,
    file_name: &str,
    operation: &'static str,
) -> Result<(), Error> {
    let Some(identity) = directory
        .entry_identity(file_name, EntryKind::RegularFile)
        .map_err(fs_error(operation))?
    else {
        return Ok(());
    };
    match directory
        .stage_random(file_name, REMOVAL_PREFIX, identity)
        .map_err(fs_error(operation))?
    {
        StageOutcome::Staged(entry) => match entry.remove().map_err(fs_error(operation))? {
            RemoveOutcome::Removed | RemoveOutcome::Missing => Ok(()),
            _ => Err(Error::Race { operation }),
        },
        StageOutcome::Missing => Ok(()),
        _ => Err(Error::Race { operation }),
    }
}

/// Returns whether `directory` is on the boot volume group.
fn on_boot_volume(directory: &TrustedDir, operation: &'static str) -> Result<bool, Error> {
    let io = |source: std::io::Error| Error::Operation {
        operation,
        source: Box::new(source),
    };
    let device = directory
        .try_clone_descriptor()
        .map_err(fs_error(operation))?
        .metadata()
        .map_err(io)?
        .dev();
    for root in ["/", BOOT_DATA_VOLUME] {
        match std::fs::metadata(root) {
            Ok(metadata) if metadata.dev() == device => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
    }
    Ok(false)
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
    fn private_directories_are_on_the_boot_volume() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = std::fs::canonicalize(root.path()).expect("canonical temporary directory");
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("private mode");
        let directory = TrustedDir::open_absolute(&path, 0o700).expect("private directory");
        assert!(on_boot_volume(&directory, "test").expect("volume check"));
    }
}
