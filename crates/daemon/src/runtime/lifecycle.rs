//! Shared lifecycle engine for durable worker generations.
//!
//! Create, native recovery, stop, and remove drive every worker job through
//! this module, so the durable-worker commit points hold on every native
//! supervisor:
//!
//! 1. the caller validates its intent and persists the preparing record with a
//!    freshly minted [`Generation`] before anything is registered;
//! 2. [`Lifecycle::launch`] registers exactly that generation;
//! 3. it waits for the authenticated worker whose journal names that
//!    generation;
//! 4. the caller initializes that worker, or [`Lifecycle::abandon`] retires
//!    the exact generation;
//! 5. the caller exposes the session only after its runtime commit succeeded.
//!
//! A failure or timeout after registration never retries blindly. The job is
//! inspected: a live worker of the right generation is adopted, a present job
//! that is not ready is watched until its own initialization deadline has
//! provably elapsed and then retired, and an absent job has its definition
//! cleaned up. An inspection failure kills nothing and reports
//! [`SUPERVISION_UNAVAILABLE`] so reconciliation can retry.
//!
//! [`SessionLocks`] serializes every lifecycle operation of one session.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use pohunek_platform::filesystem::TrustedDir;
use pohunek_platform::process::{ProcessIdentity, ProcessInspector, StartIdentity};
use pohunek_platform::supervisor::{
    Error as SupervisorError, JobDefinition, JobLogs, JobSpec, Namespace, RestartPolicy, ServiceId,
    ServiceObservation, ServiceState, WorkerKey,
};
use protocol::{ErrorClass, ProtocolError};
use serde::Deserialize;
use tokio::sync::OwnedMutexGuard;
use tokio::time::Instant;

use super::{Worker, WorkerLauncher};
use crate::store::RuntimeRecord;

// Rust guideline compliant 2026-09-24

/// Reason recorded while the native supervisor cannot be inspected.
///
/// The runtime stays `Reconnecting`; nothing is killed and reconciliation
/// retries the inspection.
pub const SUPERVISION_UNAVAILABLE: &str = "runtime_supervision_unavailable";

/// Reason recorded when a job is present but its worker cannot be proven
/// ended or adopted; the runtime is `Conflict` and nothing is killed.
pub const SUPERVISION_AMBIGUOUS: &str = "runtime_supervision_ambiguous";

/// Reason recorded when a job or worker names another generation or
/// executable than the durable record; the runtime is `Conflict`.
pub const IDENTITY_MISMATCH: &str = "runtime_identity_mismatch";

/// Error code for a new generation whose worker negotiated an outdated protocol.
///
/// New generations must carry the base-environment contract
/// ([`pohunek_worker_protocol::BASE_ENVIRONMENT_VERSION`]); a worker that
/// cannot would give its agent the service manager's environment. Such a
/// generation is never initialized and is retired by exact generation.
pub const WORKER_PROTOCOL_OUTDATED: &str = "worker_protocol_outdated";

/// Worker socket negotiation bound of the dev/test subprocess contract.
///
/// Mirrors the `worker_connect_ms` value `pohunek service install` writes; a
/// direct child binds its socket within milliseconds, so ten seconds only
/// matters on a heavily loaded test runner.
pub const DEV_WORKER_CONNECT: Duration = Duration::from_secs(10);

/// Worker initialization bound of the dev/test subprocess contract.
///
/// Mirrors the `worker_initialize_ms` value `pohunek service install` writes
/// and stays above the worker's own 30 s bootstrap window, so the engine only
/// retires a job whose worker has provably given up.
pub const DEV_WORKER_INITIALIZE: Duration = Duration::from_secs(45);

/// Worker stop grace of the dev/test subprocess contract.
///
/// Mirrors `worker_exit_timeout_ms` of an installed service (systemd's
/// `TimeoutStopSec=30s` and launchd's `ExitTimeOut`).
pub const DEV_WORKER_EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Orphan-sweep grace of the dev/test subprocess contract.
///
/// Mirrors `sweep.grace_ms` of an installed service.
pub const DEV_SWEEP_GRACE: Duration = Duration::from_secs(5);

/// Open-file limit of the dev/test subprocess contract.
///
/// Mirrors `limits.open_files` of an installed service; the subprocess
/// launcher records it in the definition but inherits the caller's limit.
pub const DEV_OPEN_FILES: u64 = 8_192;

/// Durable worker journal schema this daemon understands.
///
/// Written by `pohunek-sessiond`; a journal with another schema is never used
/// as generation evidence.
pub(crate) const WORKER_JOURNAL_SCHEMA_VERSION: u32 = 4;

/// Largest worker journal read as lifecycle evidence.
///
/// Journals hold bounded identity and subagent metadata far below this; the
/// cap keeps a corrupted file from exhausting daemon memory.
const MAX_WORKER_JOURNAL_BYTES: usize = 1024 * 1024;

/// Interval between socket connection attempts and job inspections.
///
/// Workers bind their socket within milliseconds of starting; 100 ms keeps
/// session creation responsive without busy-polling the service manager.
const SUPERVISION_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Consecutive `Race` answers tolerated from one inspection before the
/// supervisor is treated as unavailable.
///
/// A race means the job changed between two reads; three attempts absorb a
/// job that is still settling while never mistaking a moving job for an
/// absent one.
const MAX_INSPECT_RACES: usize = 3;

/// Names the stdout and stderr files of one worker job.
///
/// The native backend owns the naming (launchd rejects definitions naming other
/// files, so retirement removes exactly what it wrote); backends that keep job
/// output elsewhere, such as the systemd journal, have no naming.
#[derive(Clone)]
pub struct LogNaming(Arc<dyn Fn(&WorkerKey) -> JobLogs + Send + Sync>);

impl LogNaming {
    /// Wraps the backend's naming function.
    pub fn new(naming: impl Fn(&WorkerKey) -> JobLogs + Send + Sync + 'static) -> Self {
        Self(Arc::new(naming))
    }

    /// Returns the log files of `key`'s job.
    #[must_use]
    pub fn logs(&self, key: &WorkerKey) -> JobLogs {
        (self.0)(key)
    }
}

impl std::fmt::Debug for LogNaming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LogNaming(..)")
    }
}

impl PartialEq for LogNaming {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for LogNaming {}

/// Supervision inputs shared by every worker generation of one daemon.
///
/// Production builds it from `service.toml`; the dev/test subprocess mode
/// builds it from the `DEV_*` constants of this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisionConfig {
    /// Installation namespace embedded in native job names.
    pub namespace: Namespace,
    /// Absolute (versioned) `pohunek-sessiond` every new generation runs.
    pub worker_executable: PathBuf,
    /// `service.toml` passed to workers as `--service-config`, when installed.
    pub service_config: Option<PathBuf>,
    /// Daemon control socket passed to workers as `--daemon-socket-path`.
    pub daemon_socket: PathBuf,
    /// Upper bound on a worker's own initialization window; a present job is
    /// retired only after this much time since its process was first seen.
    pub worker_initialize: Duration,
    /// Grace between `SIGTERM` and `SIGKILL` when a worker job is stopped.
    pub worker_exit_timeout: Duration,
    /// Daemon environment names or trailing-`*` prefixes forwarded to agents.
    pub environment_allowlist: Vec<String>,
    /// Delay between `SIGTERM` and `SIGKILL` when sweeping orphaned processes.
    pub sweep_grace: Duration,
    /// Open-file limit applied to worker jobs.
    pub open_files: u64,
    /// Non-secret XDG roots and `HOME` that make the worker resolve the
    /// daemon's own paths.
    pub bootstrap_environment: BTreeMap<String, String>,
    /// Absolute working directory of worker jobs (the user's home).
    pub working_directory: PathBuf,
    /// Backend naming of worker stdout and stderr files; `None` when the
    /// backend keeps job output elsewhere (systemd journal, dev/test children).
    pub worker_logs: Option<LogNaming>,
}

/// Describes an isolated dev/test worker tree for [`super::SubprocessWorkerLauncher`].
///
/// Every base is passed to workers as its XDG variable, so a worker resolves
/// exactly the tree the daemon under test uses.
#[derive(Debug, Clone)]
pub struct SubprocessWorkerEnvironment {
    /// Base for `$XDG_RUNTIME_DIR`.
    pub runtime_home: PathBuf,
    /// Base for `$XDG_STATE_HOME`.
    pub state_home: PathBuf,
    /// Base for `$XDG_DATA_HOME`.
    pub data_home: PathBuf,
    /// Base for `$XDG_CONFIG_HOME`.
    pub config_home: PathBuf,
    /// Base for `$XDG_CACHE_HOME`.
    pub cache_home: PathBuf,
    /// `$HOME` and working directory of the worker.
    pub home: PathBuf,
    /// Actual daemon control socket used by worker-installed hooks.
    pub daemon_socket: PathBuf,
}

impl SubprocessWorkerEnvironment {
    /// Builds the dev/test supervision contract for this tree.
    ///
    /// Uses the `DEV_*` constants and
    /// [`pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST`]; no
    /// `service.toml` is passed to workers.
    #[must_use]
    pub fn supervision(&self, worker_executable: PathBuf) -> SupervisionConfig {
        let uid = nix::unistd::Uid::effective().as_raw();
        let bootstrap_environment = [
            ("XDG_RUNTIME_DIR", &self.runtime_home),
            ("XDG_STATE_HOME", &self.state_home),
            ("XDG_DATA_HOME", &self.data_home),
            ("XDG_CONFIG_HOME", &self.config_home),
            ("XDG_CACHE_HOME", &self.cache_home),
            ("HOME", &self.home),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_string_lossy().into_owned()))
        .collect();
        SupervisionConfig {
            namespace: Namespace::derive(
                uid,
                &self.state_home.join(pohunek_paths::APP_DIR),
                &self.runtime_home.join(pohunek_paths::APP_DIR),
            ),
            worker_executable,
            service_config: None,
            daemon_socket: self.daemon_socket.clone(),
            worker_initialize: DEV_WORKER_INITIALIZE,
            worker_exit_timeout: DEV_WORKER_EXIT_TIMEOUT,
            environment_allowlist: pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            sweep_grace: DEV_SWEEP_GRACE,
            open_files: DEV_OPEN_FILES,
            bootstrap_environment,
            working_directory: self.home.clone(),
            worker_logs: None,
        }
    }
}

/// One worker generation: its native job key and the executable it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    key: WorkerKey,
    executable: PathBuf,
}

impl Generation {
    /// Draws a fresh daemon-issued generation for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns `invalid_worker_identity` for a session ID that cannot name a
    /// worker, or `worker_generation_failed` when the OS entropy source fails.
    pub fn mint(session_id: &str, executable: PathBuf) -> Result<Self, ProtocolError> {
        let mut entropy = [0_u8; pohunek_paths::WORKER_GENERATION_ENTROPY_BYTES];
        getrandom::getrandom(&mut entropy).map_err(|error| {
            lifecycle_error(
                "worker_generation_failed",
                format!("failed to draw a worker generation: {error}"),
            )
        })?;
        let key = WorkerKey::new(session_id, pohunek_paths::encode_worker_generation(entropy))
            .map_err(|error| lifecycle_error("invalid_worker_identity", error.to_string()))?;
        Ok(Self { key, executable })
    }

    /// Reads the generation a durable runtime record names.
    ///
    /// Returns `Ok(None)` when the record names no job.
    ///
    /// # Errors
    ///
    /// Returns [`IDENTITY_MISMATCH`] when the fields are partial, malformed, or
    /// disagree with each other or with `session_id`.
    pub fn from_record(
        session_id: &str,
        record: &RuntimeRecord,
    ) -> Result<Option<Self>, ProtocolError> {
        let (service_id, generation, executable) = match (
            record.service_id.as_deref(),
            record.generation.as_deref(),
            record.executable.as_ref(),
        ) {
            (None, None, None) => return Ok(None),
            (Some(service_id), Some(generation), Some(executable)) => {
                (service_id, generation, executable)
            }
            _ => {
                return Err(lifecycle_error(
                    IDENTITY_MISMATCH,
                    format!("session {session_id} records a partial worker generation"),
                ))
            }
        };
        let key = ServiceId::parse(service_id)
            .and_then(|id| WorkerKey::from_service_id(&id))
            .map_err(|error| lifecycle_error(IDENTITY_MISMATCH, error.to_string()))?;
        if key.session_id() != session_id || key.generation() != generation {
            return Err(lifecycle_error(
                IDENTITY_MISMATCH,
                format!("session {session_id} records a foreign worker service {service_id}"),
            ));
        }
        Ok(Some(Self {
            key,
            executable: executable.clone(),
        }))
    }

    /// Writes this generation into a durable runtime record.
    pub fn record_into(&self, record: &mut RuntimeRecord) {
        record.service_id = Some(self.key.service_id().to_string());
        record.generation = Some(self.key.generation().to_owned());
        record.executable = Some(self.executable.clone());
    }

    /// Returns the managed session ID.
    #[must_use]
    pub fn session_id(&self) -> &str {
        self.key.session_id()
    }

    /// Returns the generation token.
    #[must_use]
    pub fn generation(&self) -> &str {
        self.key.generation()
    }

    /// Returns the backend-neutral service ID `<session-id>.<generation>`.
    #[must_use]
    pub fn service_id(&self) -> ServiceId {
        self.key.service_id()
    }

    /// Returns the executable the job definition names.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Returns the native job key.
    #[must_use]
    pub fn key(&self) -> &WorkerKey {
        &self.key
    }
}

/// Builds the explicit job definition of one worker generation.
///
/// # Errors
///
/// Returns [`SupervisorError::InvalidDefinition`] when a configured path or
/// bound violates the target-neutral job contract.
pub fn job_definition(
    config: &SupervisionConfig,
    generation: &Generation,
) -> Result<JobDefinition, SupervisorError> {
    let mut arguments = vec![
        "--session-id".to_owned(),
        generation.session_id().to_owned(),
        "--worker-generation".to_owned(),
        generation.generation().to_owned(),
    ];
    if let Some(service_config) = &config.service_config {
        arguments.push("--service-config".to_owned());
        arguments.push(path_argument(service_config)?);
    }
    arguments.push("--daemon-socket-path".to_owned());
    arguments.push(path_argument(&config.daemon_socket)?);
    JobDefinition::new(JobSpec {
        executable: generation.executable.clone(),
        arguments,
        environment: config.bootstrap_environment.clone(),
        working_directory: config.working_directory.clone(),
        logs: config
            .worker_logs
            .as_ref()
            .map(|naming| naming.logs(generation.key())),
        start_timeout: config.worker_initialize,
        exit_timeout: config.worker_exit_timeout,
        restart: RestartPolicy::Never,
        open_files: config.open_files,
    })
}

fn path_argument(path: &Path) -> Result<String, SupervisorError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| SupervisorError::InvalidDefinition {
            detail: format!("path {} is not valid UTF-8", path.display()),
        })
}

/// Serializes lifecycle operations per session.
///
/// Create, recover, stop, and remove of one session hold its lock for their
/// whole transaction; different sessions never contend. Entries are weak, so
/// the map only holds sessions with an operation in flight.
#[derive(Debug, Default)]
pub(crate) struct SessionLocks {
    locks: std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

/// Exclusive lifecycle authority over one session until dropped.
#[derive(Debug)]
pub(crate) struct LifecycleGuard {
    _guard: OwnedMutexGuard<()>,
}

impl SessionLocks {
    /// Waits for exclusive lifecycle authority over `session_id`.
    pub(crate) async fn acquire(&self, session_id: &str) -> LifecycleGuard {
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .expect("session lifecycle lock map is never poisoned");
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(session_id).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(session_id.to_owned(), Arc::downgrade(&lock));
                lock
            }
        };
        LifecycleGuard {
            _guard: lock.lock_owned().await,
        }
    }

    /// Returns how many sessions currently have a lock entry.
    #[cfg(test)]
    pub(crate) fn tracked(&self) -> usize {
        let mut locks = self
            .locks
            .lock()
            .expect("session lifecycle lock map is never poisoned");
        locks.retain(|_, lock| lock.strong_count() > 0);
        locks.len()
    }
}

/// Outcome of a lifecycle step that could not produce a usable worker.
#[derive(Debug)]
pub enum LaunchFailure {
    /// Nothing of the generation is left: the job was never registered, is
    /// proven absent, or was retired by exact generation.
    Cleaned(ProtocolError),
    /// The supervisor could not be inspected or asked to retire the job, so
    /// the job was left untouched for reconciliation.
    Unavailable(ProtocolError),
}

impl LaunchFailure {
    /// Returns the error reported to the caller.
    #[must_use]
    pub fn error(&self) -> &ProtocolError {
        match self {
            Self::Cleaned(error) | Self::Unavailable(error) => error,
        }
    }

    /// Consumes the failure and returns its error.
    #[must_use]
    pub fn into_error(self) -> ProtocolError {
        match self {
            Self::Cleaned(error) | Self::Unavailable(error) => error,
        }
    }
}

/// Why a previous generation could not be proven ended.
#[derive(Debug)]
pub enum PreviousLive {
    /// The supervisor could not be inspected; nothing was touched.
    Unavailable(ProtocolError),
    /// The previous generation may still own a live PTY.
    Ambiguous(ProtocolError),
}

impl PreviousLive {
    /// Consumes the refusal and returns its error.
    #[must_use]
    pub fn into_error(self) -> ProtocolError {
        match self {
            Self::Unavailable(error) | Self::Ambiguous(error) => error,
        }
    }
}

/// Evidence the durable record holds about a generation being replaced.
#[derive(Debug, Clone)]
pub struct PreviousGeneration {
    /// Session whose generation is being replaced.
    pub session_id: String,
    /// Job of the previous generation, when the record names one.
    pub generation: Option<Generation>,
    /// Worker identifier whose journal belongs to that generation.
    pub worker_id: Option<String>,
}

/// Lifecycle engine bound to one registry's supervisor and paths.
#[derive(Debug, Clone, Copy)]
pub struct Lifecycle<'a> {
    /// Native (or dev/test) supervisor.
    pub supervisor: &'a dyn WorkerLauncher,
    /// Supervision inputs.
    pub config: &'a SupervisionConfig,
    /// Root of the per-session worker sockets.
    pub runtime_root: &'a Path,
    /// Root of the per-session worker journals.
    pub state_root: &'a Path,
    /// Daemon instance that claims the controller lease.
    pub daemon_instance_id: &'a str,
    /// Bound on waiting for a new generation's authenticated socket.
    pub connect_deadline: Duration,
    /// Process observer used to prove a previous worker ended.
    pub inspector: &'a dyn ProcessInspector,
}

impl Lifecycle<'_> {
    /// Registers `generation` and returns its authenticated worker.
    ///
    /// Commit points 2 and 3: the job is started, then the worker is accepted
    /// only when its journal names exactly this generation and it negotiated
    /// at least [`pohunek_worker_protocol::BASE_ENVIRONMENT_VERSION`]. On
    /// failure the job is reconciled as described in the module
    /// documentation, so an `Err` never leaves an untracked live job behind
    /// unless it is [`LaunchFailure::Unavailable`]. Reconnecting to an already
    /// running generation does not go through here, so a live version-five
    /// worker kept across an upgrade stays adoptable.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchFailure`] classifying what is left of the generation;
    /// a worker below the base-environment protocol is never initialized and
    /// is abandoned with [`WORKER_PROTOCOL_OUTDATED`].
    pub async fn launch(&self, generation: &Generation) -> Result<Worker, LaunchFailure> {
        let worker = self.start_and_connect(generation).await?;
        let version = worker.selected_version().await;
        if version < pohunek_worker_protocol::BASE_ENVIRONMENT_VERSION {
            tracing::warn!(
                session_id = generation.session_id(),
                worker.generation = generation.generation(),
                worker.protocol = %version,
                "new worker generation negotiated an outdated protocol; retiring it"
            );
            drop(worker);
            let cause = lifecycle_error(
                WORKER_PROTOCOL_OUTDATED,
                format!(
                    "worker generation {} negotiated protocol {version}; new generations \
                     require {}",
                    generation.service_id(),
                    pohunek_worker_protocol::BASE_ENVIRONMENT_VERSION
                ),
            );
            return Err(self.abandon(generation, cause).await);
        }
        Ok(worker)
    }

    /// Commit points 2 and 3 without the protocol check of [`Self::launch`].
    async fn start_and_connect(&self, generation: &Generation) -> Result<Worker, LaunchFailure> {
        let definition = job_definition(self.config, generation).map_err(|error| {
            LaunchFailure::Cleaned(lifecycle_error(
                "worker_definition_invalid",
                error.to_string(),
            ))
        })?;
        let id = generation.service_id();
        let started = Instant::now();
        let failure = match self.supervisor.start(&id, &definition).await {
            Ok(()) => {
                let deadline = started + self.connect_deadline;
                match self.connect_until(generation, deadline).await {
                    Some(worker) => return Ok(worker),
                    None => lifecycle_error(
                        "worker_connect_failed",
                        format!(
                            "worker generation {id} did not accept connections within {:?}",
                            self.connect_deadline
                        ),
                    ),
                }
            }
            Err(
                error @ (SupervisorError::InvalidDefinition { .. }
                | SupervisorError::InvalidServiceId(_)),
            ) => {
                return Err(LaunchFailure::Cleaned(lifecycle_error(
                    "worker_definition_invalid",
                    error.to_string(),
                )));
            }
            Err(error) => lifecycle_error(
                "worker_manager_unavailable",
                format!("failed to start worker generation {id}: {error}"),
            ),
        };
        tracing::warn!(
            session_id = generation.session_id(),
            worker.generation = generation.generation(),
            error = %failure,
            "worker generation is not ready; reconciling its job"
        );
        self.settle(generation, started, failure).await
    }

    /// Retires a generation whose worker connected but could not be used.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchFailure::Unavailable`] carrying `cause` when the
    /// supervisor could not retire the job; otherwise
    /// [`LaunchFailure::Cleaned`] with `cause`.
    pub async fn abandon(&self, generation: &Generation, cause: ProtocolError) -> LaunchFailure {
        match self.retire(generation).await {
            Ok(()) => LaunchFailure::Cleaned(cause),
            Err(error) => {
                tracing::warn!(
                    session_id = generation.session_id(),
                    worker.generation = generation.generation(),
                    error = %error,
                    "failed to retire an abandoned worker generation"
                );
                LaunchFailure::Unavailable(supervision_unavailable(generation, &error))
            }
        }
    }

    /// Stops and unregisters exactly `generation`; an absent job is success.
    ///
    /// # Errors
    ///
    /// Returns the supervisor error for any other failure.
    pub async fn retire(&self, generation: &Generation) -> Result<(), SupervisorError> {
        match self.supervisor.retire(&generation.service_id()).await {
            Ok(()) | Err(SupervisorError::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Proves the previous generation ended, then retires its job.
    ///
    /// A previous worker whose journal is terminal only retains its final
    /// output, so it is retired first. Otherwise the job must be absent or
    /// ended without a process, and the journal's worker process must not be
    /// running. Native recovery calls this before minting a new generation, so
    /// two generations of one session never run at once (they would also
    /// contend for the per-session socket).
    ///
    /// A missing journal means the previous worker never journaled, so there
    /// is no process to cross-check. A journal that exists but cannot be read,
    /// decoded, or matched to its path proves nothing about that process and
    /// is refused as ambiguous.
    ///
    /// # Errors
    ///
    /// Returns [`PreviousLive`] when the previous generation cannot be proven
    /// ended; nothing is started or killed in that case.
    pub async fn retire_previous(&self, previous: &PreviousGeneration) -> Result<(), PreviousLive> {
        let journal = match previous.worker_id.as_deref() {
            None => None,
            Some(worker_id) => match read_journal(self.state_root, &previous.session_id, worker_id)
            {
                Ok(journal) => Some(journal),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    tracing::warn!(
                        session_id = %previous.session_id,
                        worker.id = worker_id,
                        error = %error,
                        "previous worker journal is unreadable; recovery is refused"
                    );
                    return Err(PreviousLive::Ambiguous(lifecycle_error(
                        SUPERVISION_AMBIGUOUS,
                        format!(
                            "previous worker {worker_id} of session {} has an unreadable journal: {error}",
                            previous.session_id
                        ),
                    )));
                }
            },
        };
        let runtime_ended = journal.as_ref().is_some_and(JournalFacts::runtime_ended);
        let Some(generation) = &previous.generation else {
            return self.prove_worker_process_ended(journal.as_ref(), runtime_ended);
        };
        if let Some(journal) = &journal {
            if journal.generation != generation.generation() {
                return Err(PreviousLive::Ambiguous(lifecycle_error(
                    IDENTITY_MISMATCH,
                    format!(
                        "previous worker journal names generation {} instead of {}",
                        journal.generation,
                        generation.generation()
                    ),
                )));
            }
        }
        let observation = self.inspect(generation).await.map_err(|error| {
            PreviousLive::Unavailable(supervision_unavailable(generation, &error))
        })?;
        let job_live = observation.as_ref().is_some_and(observation_live);
        if job_live && !runtime_ended {
            return Err(PreviousLive::Ambiguous(lifecycle_error(
                SUPERVISION_AMBIGUOUS,
                format!(
                    "previous worker generation {} is still running",
                    generation.service_id()
                ),
            )));
        }
        if !job_live {
            self.prove_worker_process_ended(journal.as_ref(), runtime_ended)?;
        }
        self.retire(generation)
            .await
            .map_err(|error| PreviousLive::Unavailable(supervision_unavailable(generation, &error)))
    }

    fn prove_worker_process_ended(
        &self,
        journal: Option<&JournalFacts>,
        runtime_ended: bool,
    ) -> Result<(), PreviousLive> {
        if runtime_ended {
            return Ok(());
        }
        let Some(journal) = journal else {
            return Ok(());
        };
        let identity = ProcessIdentity {
            pid: journal.worker_pid,
            start_identity: journal
                .worker_start_identity
                .parse::<StartIdentity>()
                .map_err(|error| {
                    PreviousLive::Ambiguous(lifecycle_error(IDENTITY_MISMATCH, error.to_string()))
                })?,
        };
        match self.inspector.is_running(identity) {
            Ok(false) => Ok(()),
            Ok(true) => Err(PreviousLive::Ambiguous(lifecycle_error(
                SUPERVISION_AMBIGUOUS,
                format!(
                    "previous worker process {} of session {} is still running",
                    journal.worker_pid, journal.session_id
                ),
            ))),
            Err(error) => Err(PreviousLive::Ambiguous(lifecycle_error(
                SUPERVISION_AMBIGUOUS,
                format!("previous worker process cannot be inspected: {error}"),
            ))),
        }
    }

    /// Connects to the generation's socket until `deadline`.
    async fn connect_until(&self, generation: &Generation, deadline: Instant) -> Option<Worker> {
        loop {
            if let Some(worker) = self.try_connect(generation, deadline).await {
                return Some(worker);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            tokio::time::sleep(SUPERVISION_POLL_INTERVAL.min(deadline - now)).await;
        }
    }

    /// Makes one bounded attempt to adopt the generation's worker.
    async fn try_connect(&self, generation: &Generation, deadline: Instant) -> Option<Worker> {
        let socket = self
            .runtime_root
            .join(generation.session_id())
            .join(pohunek_paths::WORKER_SOCKET_NAME);
        let budget = deadline
            .saturating_duration_since(Instant::now())
            .max(SUPERVISION_POLL_INTERVAL);
        let worker = match tokio::time::timeout(
            budget,
            Worker::connect(&socket, generation.session_id(), self.daemon_instance_id),
        )
        .await
        {
            Ok(Ok(worker)) => worker,
            Ok(Err(error)) => {
                tracing::debug!(
                    session_id = generation.session_id(),
                    error = %error,
                    "worker socket not ready yet"
                );
                return None;
            }
            Err(_elapsed) => return None,
        };
        let worker_id = worker.worker_id().await;
        match read_journal(self.state_root, generation.session_id(), worker_id.as_str()) {
            Ok(journal) if journal.generation == generation.generation() => Some(worker),
            Ok(journal) => {
                tracing::warn!(
                    session_id = generation.session_id(),
                    worker.generation = generation.generation(),
                    journal.generation = %journal.generation,
                    "worker socket is served by another generation"
                );
                None
            }
            Err(error) => {
                tracing::warn!(
                    session_id = generation.session_id(),
                    error = %error,
                    "connected worker has no readable journal"
                );
                None
            }
        }
    }

    /// Reconciles a generation after a failed or timed-out start.
    async fn settle(
        &self,
        generation: &Generation,
        started: Instant,
        cause: ProtocolError,
    ) -> Result<Worker, LaunchFailure> {
        let mut first_process: Option<(ProcessIdentity, Instant)> = None;
        loop {
            let observation = match self.inspect(generation).await {
                Ok(observation) => observation,
                Err(error) => {
                    return Err(LaunchFailure::Unavailable(supervision_unavailable(
                        generation, &error,
                    )));
                }
            };
            let Some(observation) = observation.filter(observation_live) else {
                // Absent, or ended without a process: nothing can become ready,
                // so only the definition is left to clean up.
                return Err(self.abandon(generation, cause).await);
            };
            let now = Instant::now();
            let give_up_at = match observation.process {
                Some(process) => {
                    let seen = match first_process {
                        Some((identity, seen)) if identity == process => seen,
                        _ => {
                            first_process = Some((process, now));
                            now
                        }
                    };
                    seen + self.config.worker_initialize
                }
                None => started + self.config.worker_initialize,
            };
            if observation.process.is_some() {
                if let Some(worker) = self.try_connect(generation, give_up_at).await {
                    tracing::info!(
                        session_id = generation.session_id(),
                        worker.generation = generation.generation(),
                        "adopted a late worker generation"
                    );
                    return Ok(worker);
                }
            }
            let now = Instant::now();
            if now >= give_up_at {
                return Err(self.abandon(generation, cause).await);
            }
            tokio::time::sleep(SUPERVISION_POLL_INTERVAL.min(give_up_at - now)).await;
        }
    }

    /// Inspects the generation's job; `Ok(None)` means proven absent.
    ///
    /// A `Race` is retried a bounded number of times and never read as absent.
    ///
    /// # Errors
    ///
    /// Returns the supervisor error for anything but a proven absence,
    /// including a `Race` that persisted through every retry.
    pub(crate) async fn inspect(
        &self,
        generation: &Generation,
    ) -> Result<Option<ServiceObservation>, SupervisorError> {
        self.inspect_service(&generation.service_id()).await
    }

    /// Inspects the job named `id`; `Ok(None)` means proven absent.
    ///
    /// A `Race` is retried a bounded number of times and never read as absent.
    ///
    /// # Errors
    ///
    /// Returns the supervisor error for anything but a proven absence,
    /// including a `Race` that persisted through every retry.
    pub(crate) async fn inspect_service(
        &self,
        id: &ServiceId,
    ) -> Result<Option<ServiceObservation>, SupervisorError> {
        let mut races = 0;
        loop {
            match self.supervisor.inspect(id).await {
                Ok(observation) => return Ok(Some(observation)),
                Err(SupervisorError::NotFound(_)) => return Ok(None),
                Err(SupervisorError::Race { operation }) if races < MAX_INSPECT_RACES => {
                    races += 1;
                    tracing::debug!(
                        service_id = %id,
                        operation,
                        races,
                        "worker job changed during inspection; retrying"
                    );
                    tokio::time::sleep(SUPERVISION_POLL_INTERVAL).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// Whether an observed job may still have or produce a worker process.
pub(crate) fn observation_live(observation: &ServiceObservation) -> bool {
    observation.process.is_some()
        || matches!(
            observation.state,
            ServiceState::Starting | ServiceState::Running | ServiceState::Unknown
        )
}

fn supervision_unavailable(generation: &Generation, error: &SupervisorError) -> ProtocolError {
    lifecycle_error(
        SUPERVISION_UNAVAILABLE,
        format!(
            "worker generation {} cannot be supervised: {error}",
            generation.service_id()
        ),
    )
}

fn lifecycle_error(code: &str, message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorClass::Runtime, code, message, None)
}

/// Worker journal facts the lifecycle engine relies on.
#[derive(Debug, Clone, Deserialize)]
struct JournalFacts {
    schema_version: u32,
    session_id: String,
    worker_id: String,
    generation: String,
    worker_pid: u32,
    worker_start_identity: String,
    phase: JournalPhase,
}

impl JournalFacts {
    /// Whether the journal proves the PTY runtime is no longer running.
    fn runtime_ended(&self) -> bool {
        matches!(
            self.phase,
            JournalPhase::Terminal | JournalPhase::NeverInitialized
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalPhase {
    Bootstrap,
    Starting,
    Live,
    Terminal,
    NeverInitialized,
    Faulted,
}

/// Reads one worker journal through the owner-private state tree.
fn read_journal(
    state_root: &Path,
    session_id: &str,
    worker_id: &str,
) -> std::io::Result<JournalFacts> {
    if pohunek_paths::valid_worker_session_id(session_id).is_none()
        || pohunek_paths::valid_worker_id(worker_id).is_none()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "worker journal identity is malformed",
        ));
    }
    let bytes = TrustedDir::open_absolute(state_root, 0o700)
        .and_then(|root| root.open_child(session_id, 0o700))
        .and_then(|session| {
            session.read_file(format!("{worker_id}.json"), 0o600, MAX_WORKER_JOURNAL_BYTES)
        })
        // The OS error kind is kept so a missing journal stays `NotFound`.
        .map_err(|error| {
            std::io::Error::new(error.io_kind().unwrap_or(std::io::ErrorKind::Other), error)
        })?;
    let journal = serde_json::from_slice::<JournalFacts>(&bytes)?;
    if journal.schema_version != WORKER_JOURNAL_SCHEMA_VERSION
        || journal.session_id != session_id
        || journal.worker_id != worker_id
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "worker journal identity does not match its path",
        ));
    }
    Ok(journal)
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
pub(crate) mod tests;
