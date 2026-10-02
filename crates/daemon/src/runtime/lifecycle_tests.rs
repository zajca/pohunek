//! Branch coverage for the worker lifecycle engine.
//!
//! [`ScriptedSupervisor`] answers `start` and `inspect` from a script so every
//! reconciliation branch is reachable deterministically. When it wraps a real
//! backend (the in-process worker server or the subprocess launcher),
//! unscripted calls run real workers, which the session registry tests use to
//! drive whole lifecycle transactions.

use std::collections::{HashSet, VecDeque};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;

use pohunek_platform::process::HostInspector;
use pohunek_platform::supervisor::{
    DefinitionFacts, Operation, Supervisor, BOOTSTRAP_ENV_ALLOWLIST,
};
use pohunek_test_support::time::AutoAdvanceInhibitor;
use pohunek_test_support::wait;
use tokio::sync::Notify;

use super::*;
use crate::runtime::InProcessWorkerLauncher;

/// Scripted answer to one `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartStep {
    /// Accept the job; the delegate (when present) starts the worker now.
    Accept,
    /// Accept the job; the delegate starts the worker after this delay.
    AcceptLate(Duration),
    /// Reject the call with an operation error; nothing is registered.
    Fail,
}

/// Scripted answer to one `inspect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InspectStep {
    /// The job exists in `state`, with this test process as its main process
    /// when `process` is set.
    Present {
        /// Reported lifecycle state.
        state: ServiceState,
        /// Whether a main process is reported.
        process: bool,
    },
    /// The job is absent.
    NotFound,
    /// The job changed during the observation.
    Race,
    /// The supervisor cannot be reached.
    Unavailable,
    /// The inspection never completes, as a wedged service manager.
    Block,
}

/// Scripted, sticky answer for one service ID, taking precedence over the
/// inspect script; present jobs are also what `discover` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JobScript {
    /// The job exists with these facts.
    Present {
        /// Reported lifecycle state.
        state: ServiceState,
        /// Reported main process.
        process: Option<ProcessIdentity>,
        /// Reported definition facts.
        definition: Option<DefinitionFacts>,
    },
    /// The supervisor cannot inspect this job.
    Unavailable,
    /// Inspecting or retiring this job panics, standing in for a broken
    /// invariant inside a background lifecycle task.
    Panic,
}

/// One supervisor call, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    Start(ServiceId),
    Inspect(ServiceId),
    Retire(ServiceId),
}

/// Pauses the next matching `start` once, until released.
#[derive(Debug, Clone)]
pub(crate) struct StartGate {
    /// Session whose start is held; `None` holds the next start of any session.
    pub(crate) session_id: Option<String>,
    /// Notified once the held start was entered.
    pub(crate) entered: Arc<Notify>,
    /// Releases the held start.
    pub(crate) release: Arc<Notify>,
}

/// Test supervisor answering from a script, optionally over a real worker.
#[derive(Debug, Default)]
pub(crate) struct ScriptedSupervisor {
    delegate: Option<Arc<dyn Supervisor>>,
    starts: StdMutex<VecDeque<StartStep>>,
    inspects: StdMutex<VecDeque<InspectStep>>,
    calls: StdMutex<Vec<Call>>,
    gate: StdMutex<Option<StartGate>>,
    store: Option<PathBuf>,
    persisted_at_start: StdMutex<Vec<Option<String>>>,
    jobs: StdMutex<HashMap<ServiceId, JobScript>>,
    retire_unavailable: StdMutex<HashSet<ServiceId>>,
    discovery_unavailable: std::sync::atomic::AtomicBool,
}

impl ScriptedSupervisor {
    /// A supervisor without a real worker; unscripted inspections are absent.
    pub(crate) fn scripted() -> Self {
        Self::default()
    }

    /// A supervisor over a real worker backend (the in-process worker server
    /// or the subprocess launcher running the real worker binary).
    pub(crate) fn over(delegate: impl Supervisor + 'static) -> Self {
        Self {
            delegate: Some(Arc::new(delegate)),
            ..Self::default()
        }
    }

    /// Records the stored generation of the started session at every `start`.
    pub(crate) fn recording_store(mut self, store: PathBuf) -> Self {
        self.store = Some(store);
        self
    }

    pub(crate) fn script_starts(&self, steps: impl IntoIterator<Item = StartStep>) {
        self.starts.lock().expect("script").extend(steps);
    }

    pub(crate) fn script_inspects(&self, steps: impl IntoIterator<Item = InspectStep>) {
        self.inspects.lock().expect("script").extend(steps);
    }

    /// Scripts every later `inspect` of `id`; a present job is discovered and
    /// retiring it removes the script.
    pub(crate) fn script_job(&self, id: ServiceId, script: JobScript) {
        self.jobs.lock().expect("jobs").insert(id, script);
    }

    /// Makes the next `retire` of `id` fail as an unreachable manager.
    ///
    /// One-shot: a later retry of the same ID retires normally, so a test can
    /// inject one transient supervisor failure and observe the follow-up.
    pub(crate) fn script_retire_unavailable(&self, id: ServiceId) {
        self.retire_unavailable
            .lock()
            .expect("retire script")
            .insert(id);
    }

    /// Makes `discover` fail as an unreachable manager.
    pub(crate) fn fail_discovery(&self) {
        self.discovery_unavailable.store(true, Ordering::Release);
    }

    pub(crate) fn hold_start(&self, gate: StartGate) {
        *self.gate.lock().expect("gate") = Some(gate);
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls").clone()
    }

    pub(crate) fn retired(&self) -> Vec<ServiceId> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Retire(id) => Some(id),
                Call::Start(_) | Call::Inspect(_) => None,
            })
            .collect()
    }

    pub(crate) fn started(&self) -> Vec<ServiceId> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Start(id) => Some(id),
                Call::Inspect(_) | Call::Retire(_) => None,
            })
            .collect()
    }

    /// Generations the store named for the started session at each `start`.
    pub(crate) fn persisted_at_start(&self) -> Vec<Option<String>> {
        self.persisted_at_start.lock().expect("persisted").clone()
    }

    /// Jobs the delegate currently runs.
    pub(crate) async fn live_jobs(&self) -> Vec<ServiceId> {
        let Some(delegate) = &self.delegate else {
            return Vec::new();
        };
        delegate
            .discover()
            .await
            .expect("discover in-process jobs")
            .into_iter()
            .filter(|observation| observation.state == ServiceState::Running)
            .map(|observation| observation.id)
            .collect()
    }

    fn record(&self, call: Call) {
        self.calls.lock().expect("calls").push(call);
    }

    fn next_start(&self) -> StartStep {
        self.starts
            .lock()
            .expect("script")
            .pop_front()
            .unwrap_or(StartStep::Accept)
    }

    fn next_inspect(&self) -> Option<InspectStep> {
        let mut steps = self.inspects.lock().expect("script");
        if steps.len() > 1 {
            steps.pop_front()
        } else {
            steps.front().copied()
        }
    }

    fn record_persisted(&self, id: &ServiceId) {
        let Some(store) = &self.store else {
            return;
        };
        let key = WorkerKey::from_service_id(id).expect("worker service id");
        let generation = crate::store::Store::new(store.clone())
            .load_sessions()
            .expect("load sessions at start")
            .into_iter()
            .find(|record| record.session_id == key.session_id())
            .and_then(|record| record.runtime.generation);
        self.persisted_at_start
            .lock()
            .expect("persisted")
            .push(generation);
    }
}

impl Supervisor for ScriptedSupervisor {
    fn start<'a>(&'a self, id: &'a ServiceId, definition: &'a JobDefinition) -> Operation<'a, ()> {
        Box::pin(async move {
            self.record(Call::Start(id.clone()));
            self.record_persisted(id);
            let gate = {
                let mut slot = self.gate.lock().expect("gate");
                let held = slot.as_ref().is_some_and(|gate| {
                    gate.session_id.as_deref().is_none_or(|session| {
                        WorkerKey::from_service_id(id).is_ok_and(|key| key.session_id() == session)
                    })
                });
                if held {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            match self.next_start() {
                StartStep::Fail => Err(SupervisorError::Operation {
                    operation: "start",
                    source: Box::new(std::io::Error::other("scripted start failure")),
                }),
                StartStep::Accept => match &self.delegate {
                    Some(delegate) => delegate.start(id, definition).await,
                    None => Ok(()),
                },
                StartStep::AcceptLate(delay) => {
                    if let Some(delegate) = self.delegate.clone() {
                        let id = id.clone();
                        let definition = definition.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            // A late start may lose the session socket to a
                            // live generation; the engine must cope either way.
                            let _ = delegate.start(&id, &definition).await;
                        });
                    }
                    Ok(())
                }
            }
        })
    }

    fn discover(&self) -> Operation<'_, Vec<ServiceObservation>> {
        Box::pin(async move {
            if self.discovery_unavailable.load(Ordering::Acquire) {
                return Err(unavailable("discover"));
            }
            let mut observations = match &self.delegate {
                Some(delegate) => delegate.discover().await?,
                None => Vec::new(),
            };
            observations.extend(self.jobs.lock().expect("jobs").iter().filter_map(
                |(id, script)| match script {
                    JobScript::Present { .. } => scripted_observation(id, script),
                    JobScript::Unavailable | JobScript::Panic => None,
                },
            ));
            observations.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(observations)
        })
    }

    fn inspect<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ServiceObservation> {
        Box::pin(async move {
            self.record(Call::Inspect(id.clone()));
            let scripted = self.jobs.lock().expect("jobs").get(id).cloned();
            // The `jobs` lock is released above, so the panic poisons nothing.
            assert!(
                scripted != Some(JobScript::Panic),
                "scripted supervisor panic inspecting {id}"
            );
            if let Some(script) = scripted {
                return scripted_observation(id, &script).ok_or_else(|| unavailable("inspect"));
            }
            match self.next_inspect() {
                Some(InspectStep::Present { state, process }) => Ok(ServiceObservation {
                    id: id.clone(),
                    state,
                    process: process.then(own_process),
                    definition: None,
                }),
                Some(InspectStep::NotFound) => Err(SupervisorError::NotFound(id.clone())),
                Some(InspectStep::Race) => Err(SupervisorError::Race {
                    operation: "inspect",
                }),
                Some(InspectStep::Unavailable) => Err(SupervisorError::Unavailable {
                    operation: "inspect",
                    source: Box::new(std::io::Error::other("scripted manager outage")),
                }),
                Some(InspectStep::Block) => std::future::pending().await,
                None => match &self.delegate {
                    Some(delegate) => delegate.inspect(id).await,
                    None => Err(SupervisorError::NotFound(id.clone())),
                },
            }
        })
    }

    fn retire<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ()> {
        Box::pin(async move {
            self.record(Call::Retire(id.clone()));
            if self
                .retire_unavailable
                .lock()
                .expect("retire script")
                .remove(id)
            {
                return Err(unavailable("retire"));
            }
            let scripted = self.jobs.lock().expect("jobs").remove(id);
            if let Some(script) = scripted {
                return match script {
                    JobScript::Present { .. } => Ok(()),
                    JobScript::Unavailable => Err(unavailable("retire")),
                    JobScript::Panic => panic!("scripted supervisor panic retiring {id}"),
                };
            }
            match &self.delegate {
                Some(delegate) => delegate.retire(id).await,
                None => Err(SupervisorError::NotFound(id.clone())),
            }
        })
    }

    fn exit_status<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, Option<JobExit>> {
        Box::pin(async move {
            match &self.delegate {
                Some(delegate) => delegate.exit_status(id).await,
                None => Ok(None),
            }
        })
    }
}

fn scripted_observation(id: &ServiceId, script: &JobScript) -> Option<ServiceObservation> {
    match script {
        JobScript::Present {
            state,
            process,
            definition,
        } => Some(ServiceObservation {
            id: id.clone(),
            state: *state,
            process: *process,
            definition: definition.clone(),
        }),
        JobScript::Unavailable | JobScript::Panic => None,
    }
}

fn unavailable(operation: &'static str) -> SupervisorError {
    SupervisorError::Unavailable {
        operation,
        source: Box::new(std::io::Error::other("scripted manager outage")),
    }
}

fn own_process() -> ProcessIdentity {
    HostInspector::new()
        .identity(std::process::id())
        .expect("inspect the test process")
        .expect("the test process exists")
}

/// Short, unique, owner-private roots for worker sockets and journals.
#[derive(Debug)]
struct Roots {
    base: PathBuf,
}

impl Roots {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(1);
        // Worker sockets live below the runtime root and `sockaddr_un` paths
        // are short, so the base stays directly under the temp directory.
        let base = pohunek_test_support::temp_root().join(format!(
            "pl-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        for dir in [base.clone(), base.join("r"), base.join("s")] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .expect("create private test root");
        }
        Self { base }
    }
}

impl Drop for Roots {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// One engine under test with its supervisor, roots, and deadlines.
#[derive(Debug)]
struct Harness {
    supervisor: ScriptedSupervisor,
    config: SupervisionConfig,
    inspector: HostInspector,
    connect_deadline: Duration,
    runtime_root: PathBuf,
    state_root: PathBuf,
    home: PathBuf,
    _roots: Roots,
}

impl Harness {
    fn scripted(connect_deadline: Duration, worker_initialize: Duration) -> Self {
        Self::build(false, connect_deadline, worker_initialize)
    }

    fn over_worker(connect_deadline: Duration, worker_initialize: Duration) -> Self {
        Self::build(true, connect_deadline, worker_initialize)
    }

    fn build(delegate: bool, connect_deadline: Duration, worker_initialize: Duration) -> Self {
        let roots = Roots::new();
        let runtime_root = roots.base.join("r");
        let state_root = roots.base.join("s");
        let supervisor = if delegate {
            ScriptedSupervisor::over(InProcessWorkerLauncher::new(
                runtime_root.clone(),
                state_root.clone(),
            ))
        } else {
            ScriptedSupervisor::scripted()
        };
        let mut config = SubprocessWorkerEnvironment {
            runtime_home: runtime_root.clone(),
            state_home: state_root.clone(),
            data_home: state_root.clone(),
            config_home: state_root.clone(),
            cache_home: state_root.clone(),
            home: roots.base.clone(),
            daemon_socket: runtime_root.join("daemon.sock"),
        }
        .supervision(PathBuf::from(
            "/opt/pohunek/libexec/pohunek/1.0.0/pohunek-sessiond",
        ))
        .with_environment_source(crate::test_support::thread_environment_source());
        config.worker_initialize = worker_initialize;
        Self {
            supervisor,
            config,
            inspector: HostInspector::new(),
            connect_deadline,
            runtime_root,
            state_root,
            home: roots.base.clone(),
            _roots: roots,
        }
    }

    fn lifecycle(&self) -> Lifecycle<'_> {
        Lifecycle {
            supervisor: &self.supervisor,
            config: &self.config,
            runtime_root: &self.runtime_root,
            state_root: &self.state_root,
            daemon_instance_id: "lifecycle-test",
            connect_deadline: self.connect_deadline,
            inspector: &self.inspector,
        }
    }

    fn generation(&self, session_id: &str) -> Generation {
        Generation::mint(session_id, self.config.worker_executable.clone()).expect("mint")
    }

    /// Writes a worker journal the engine reads as previous-generation evidence.
    fn write_journal(&self, session_id: &str, worker_id: &str, facts: &serde_json::Value) {
        self.write_journal_bytes(session_id, worker_id, facts.to_string().as_bytes());
    }

    /// Writes raw journal bytes, which need not be a valid journal.
    fn write_journal_bytes(&self, session_id: &str, worker_id: &str, bytes: &[u8]) {
        let directory = self.state_root.join(session_id);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .expect("create journal directory");
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(directory.join(format!("{worker_id}.json")))
            .expect("create journal");
        std::io::Write::write_all(&mut file, bytes).expect("write journal");
    }
}

/// Connect deadline of the timeout branches; short so they stay fast.
const CONNECT: Duration = Duration::from_millis(150);
/// Initialize window of the timeout branches; longer than [`CONNECT`] so the
/// retirement provably waits for it.
const INITIALIZE: Duration = Duration::from_millis(400);
/// Deadline that a healthy in-process worker never approaches.
const GENEROUS: Duration = Duration::from_secs(10);

fn journal(
    session_id: &str,
    worker_id: &str,
    generation: &str,
    phase: &str,
    pid: u32,
) -> serde_json::Value {
    let start_identity = HostInspector::new()
        .identity(pid)
        .ok()
        .flatten()
        .map_or(1, |identity| identity.start_identity.get());
    serde_json::json!({
        "schema_version": WORKER_JOURNAL_SCHEMA_VERSION,
        "session_id": session_id,
        "worker_id": worker_id,
        "generation": generation,
        "worker_pid": pid,
        "worker_start_identity": start_identity.to_string(),
        "phase": phase,
    })
}

#[test]
fn minted_generations_are_valid_distinct_and_named_by_the_definition() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    let first = harness.generation("s-1");
    let second = harness.generation("s-1");
    assert_ne!(first.generation(), second.generation());
    assert!(pohunek_paths::valid_worker_generation(first.generation()).is_some());
    assert_eq!(
        first.service_id().as_str(),
        format!("s-1.{}", first.generation())
    );

    let definition = job_definition(&harness.config, &first).expect("valid definition");
    assert_eq!(definition.executable(), first.executable());
    assert_eq!(
        definition.arguments(),
        [
            "--session-id".to_owned(),
            "s-1".to_owned(),
            "--worker-generation".to_owned(),
            first.generation().to_owned(),
            "--daemon-socket-path".to_owned(),
            harness.config.daemon_socket.display().to_string(),
        ]
    );
    assert!(definition
        .environment()
        .keys()
        .all(|key| BOOTSTRAP_ENV_ALLOWLIST.contains(&key.as_str())));
    assert_eq!(definition.working_directory(), harness.home);
    assert_eq!(definition.restart(), RestartPolicy::Never);
    assert_eq!(definition.start_timeout(), INITIALIZE);
    assert_eq!(definition.exit_timeout(), DEV_WORKER_EXIT_TIMEOUT);
    assert_eq!(definition.open_files(), DEV_OPEN_FILES);
    assert!(
        definition.logs().is_none(),
        "dev/test jobs name no log files"
    );
    assert_eq!(
        definition.facts(),
        DefinitionFacts {
            executable: first.executable().to_path_buf(),
            arguments: definition.arguments().to_vec(),
        }
    );
}

#[test]
fn installed_definitions_pass_the_service_config() {
    let mut harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.config.service_config = Some(PathBuf::from("/home/u/.config/pohunek/service.toml"));
    let generation = harness.generation("s-1");
    let definition = job_definition(&harness.config, &generation).expect("valid definition");
    let arguments = definition.arguments();
    let flag = arguments
        .iter()
        .position(|argument| argument == "--service-config")
        .expect("service config flag");
    assert_eq!(arguments[flag + 1], "/home/u/.config/pohunek/service.toml");
}

#[test]
fn definitions_name_exactly_the_backend_log_files() {
    let mut harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.config.worker_logs = Some(LogNaming::new(|key: &WorkerKey| JobLogs {
        stdout: PathBuf::from(format!("/logs/{key}.out.log")),
        stderr: PathBuf::from(format!("/logs/{key}.err.log")),
    }));
    let generation = harness.generation("s-1");

    let definition = job_definition(&harness.config, &generation).expect("valid definition");

    assert_eq!(
        definition.logs(),
        Some(&JobLogs {
            stdout: PathBuf::from(format!("/logs/{}.out.log", generation.key())),
            stderr: PathBuf::from(format!("/logs/{}.err.log", generation.key())),
        })
    );
}

#[test]
fn records_round_trip_and_reject_foreign_or_partial_generations() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    let generation = harness.generation("s-7");
    let mut record = RuntimeRecord {
        state: protocol::RuntimeState::Starting,
        worker_id: None,
        runtime_id: None,
        service_id: None,
        generation: None,
        executable: None,
        reason: None,
    };
    assert_eq!(
        Generation::from_record("s-7", &record).expect("empty"),
        None
    );

    generation.record_into(&mut record);
    assert_eq!(
        Generation::from_record("s-7", &record).expect("round trip"),
        Some(generation.clone())
    );
    assert_eq!(
        Generation::from_record("s-8", &record)
            .expect_err("foreign session")
            .code,
        IDENTITY_MISMATCH
    );

    let mut partial = record.clone();
    partial.executable = None;
    assert_eq!(
        Generation::from_record("s-7", &partial)
            .expect_err("partial generation")
            .code,
        IDENTITY_MISMATCH
    );

    let mut mismatched = record;
    mismatched.generation = Some("zzzz2345".to_owned());
    assert_eq!(
        Generation::from_record("s-7", &mismatched)
            .expect_err("service id and generation disagree")
            .code,
        IDENTITY_MISMATCH
    );
}

#[tokio::test]
async fn launch_returns_the_worker_of_exactly_the_started_generation() {
    let harness = Harness::over_worker(GENEROUS, GENEROUS);
    let generation = harness.generation("s-1");

    let worker = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect("launched generation connects");

    let journal = read_journal(
        &harness.state_root,
        "s-1",
        worker.worker_id().await.as_str(),
    )
    .expect("worker journal");
    assert_eq!(journal.generation, generation.generation());
    assert_eq!(harness.supervisor.started(), [generation.service_id()]);
    assert!(harness.supervisor.retired().is_empty());
    harness
        .lifecycle()
        .retire(&generation)
        .await
        .expect("retire test worker");
}

#[tokio::test]
async fn start_failure_with_an_absent_job_cleans_only_its_definition() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_starts([StartStep::Fail]);
    harness.supervisor.script_inspects([InspectStep::NotFound]);
    let generation = harness.generation("s-1");

    let failure = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect_err("failed start");

    assert!(
        matches!(&failure, LaunchFailure::Cleaned(error) if error.code == "worker_manager_unavailable"),
        "{failure:?}"
    );
    assert_eq!(
        harness.supervisor.calls(),
        [
            Call::Start(generation.service_id()),
            Call::Inspect(generation.service_id()),
            Call::Retire(generation.service_id()),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn an_absent_job_ends_the_connect_wait_and_is_cleaned_up() {
    let harness = Harness::scripted(GENEROUS, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::NotFound]);
    let generation = harness.generation("s-1");
    let started = Instant::now();

    let failure = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect_err("no worker");

    assert!(
        started.elapsed() < GENEROUS,
        "an absent job cannot produce a worker; waited {:?}",
        started.elapsed()
    );
    assert!(
        matches!(
            &failure,
            LaunchFailure::Cleaned(error) if error.code == WORKER_EXITED_BEFORE_READY
                && error.msg.contains("no longer registered")
        ),
        "{failure:?}"
    );
    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);
}

#[tokio::test(start_paused = true)]
async fn a_present_job_is_retired_by_exact_generation_only_after_its_initialize_deadline() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::Present {
        state: ServiceState::Running,
        process: true,
    }]);
    let generation = harness.generation("s-1");
    let started = Instant::now();

    let failure = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect_err("worker never connects");

    assert!(
        started.elapsed() >= CONNECT + INITIALIZE,
        "retired after {:?}, before the connect deadline plus the worker's own initialize window",
        started.elapsed()
    );
    assert!(
        matches!(&failure, LaunchFailure::Cleaned(error) if error.code == "worker_connect_failed"),
        "{failure:?}"
    );
    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);
}

#[tokio::test(start_paused = true)]
async fn a_job_that_ended_without_a_process_is_retired_immediately() {
    let harness = Harness::scripted(GENEROUS, GENEROUS);
    harness.supervisor.script_inspects([InspectStep::Present {
        state: ServiceState::Failed,
        process: false,
    }]);
    let generation = harness.generation("s-1");
    let started = Instant::now();

    let failure = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect_err("worker failed");

    assert!(
        started.elapsed() < GENEROUS,
        "an ended job ends the connect wait; waited {:?}",
        started.elapsed()
    );
    assert!(
        matches!(
            &failure,
            LaunchFailure::Cleaned(error) if error.code == WORKER_EXITED_BEFORE_READY
                && error.msg.contains("state Failed")
        ),
        "{failure:?}"
    );
    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);
}

#[tokio::test(start_paused = true)]
async fn a_loaded_job_without_a_process_is_retired_only_after_its_initialize_deadline() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::Present {
        state: ServiceState::Unknown,
        process: false,
    }]);
    let generation = harness.generation("s-1");
    let started = Instant::now();

    let failure = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect_err("worker never connects");

    assert!(
        started.elapsed() >= INITIALIZE,
        "a job without a process may still spawn one; retired after {:?}",
        started.elapsed()
    );
    assert!(matches!(failure, LaunchFailure::Cleaned(_)), "{failure:?}");
    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);
}

#[tokio::test]
async fn an_unavailable_inspection_kills_nothing() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness
        .supervisor
        .script_inspects([InspectStep::Unavailable]);
    let generation = harness.generation("s-1");

    let failure = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect_err("supervisor outage");

    assert!(
        matches!(&failure, LaunchFailure::Unavailable(error) if error.code == SUPERVISION_UNAVAILABLE),
        "{failure:?}"
    );
    assert!(harness.supervisor.retired().is_empty());
}

#[tokio::test]
async fn races_are_retried_and_never_read_as_absent() {
    // Each race retry waits one poll interval, so the connect deadline must
    // outlast both retries for the settled absence to be observed.
    let harness = Harness::scripted(GENEROUS, INITIALIZE);
    harness.supervisor.script_inspects([
        InspectStep::Race,
        InspectStep::Race,
        InspectStep::NotFound,
    ]);
    let generation = harness.generation("s-1");

    let failure = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect_err("job settles absent");

    assert!(
        matches!(&failure, LaunchFailure::Cleaned(error) if error.code == WORKER_EXITED_BEFORE_READY),
        "{failure:?}"
    );
    let inspections = harness
        .supervisor
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Inspect(_)))
        .count();
    // Three while the socket is awaited (two races, then the absence) and
    // one more while the ended job is settled.
    assert_eq!(inspections, 4);

    let racing = Harness::scripted(CONNECT, INITIALIZE);
    racing.supervisor.script_inspects([InspectStep::Race]);
    let failure = racing
        .lifecycle()
        .launch(&racing.generation("s-2"))
        .await
        .expect_err("job never settles");
    assert!(
        matches!(&failure, LaunchFailure::Unavailable(error) if error.code == SUPERVISION_UNAVAILABLE),
        "{failure:?}"
    );
    assert!(racing.supervisor.retired().is_empty());
}

#[tokio::test]
async fn a_late_worker_of_the_right_generation_is_adopted() {
    let harness = Harness::over_worker(CONNECT, GENEROUS);
    harness
        .supervisor
        .script_starts([StartStep::AcceptLate(CONNECT * 2)]);
    harness.supervisor.script_inspects([InspectStep::Present {
        state: ServiceState::Running,
        process: true,
    }]);
    let generation = harness.generation("s-1");

    let worker = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect("late worker is adopted");

    let journal = read_journal(
        &harness.state_root,
        "s-1",
        worker.worker_id().await.as_str(),
    )
    .expect("worker journal");
    assert_eq!(journal.generation, generation.generation());
    assert!(harness.supervisor.retired().is_empty());
    harness
        .lifecycle()
        .retire(&generation)
        .await
        .expect("retire test worker");
}

#[tokio::test]
async fn a_worker_serving_another_generation_is_never_adopted() {
    let harness = Harness::over_worker(CONNECT, INITIALIZE);
    let running = harness.generation("s-1");
    // The healthy first generation gets the dev/test connect contract: its
    // budget covers the worker's durable startup I/O inside `start`. Only
    // `other` is meant to exhaust the short deadline.
    Lifecycle {
        connect_deadline: DEV_WORKER_CONNECT,
        ..harness.lifecycle()
    }
    .launch(&running)
    .await
    .expect("first generation runs");
    let other = harness.generation("s-1");
    // The job of `other` is reported present, but the session socket keeps
    // serving the running generation, whose journal names another token.
    harness
        .supervisor
        .script_starts([StartStep::AcceptLate(GENEROUS)]);
    harness.supervisor.script_inspects([InspectStep::Present {
        state: ServiceState::Running,
        process: true,
    }]);

    let failure = harness
        .lifecycle()
        .launch(&other)
        .await
        .expect_err("the socket serves another generation");

    assert!(matches!(failure, LaunchFailure::Cleaned(_)), "{failure:?}");
    assert_eq!(harness.supervisor.retired(), [other.service_id()]);
    harness
        .lifecycle()
        .retire(&running)
        .await
        .expect("retire test worker");
}

#[tokio::test]
async fn abandon_reports_unavailable_when_the_job_cannot_be_retired() {
    #[derive(Debug)]
    struct Unretirable;

    impl Supervisor for Unretirable {
        fn start<'a>(&'a self, _: &'a ServiceId, _: &'a JobDefinition) -> Operation<'a, ()> {
            Box::pin(async { Ok(()) })
        }

        fn discover(&self) -> Operation<'_, Vec<ServiceObservation>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn inspect<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ServiceObservation> {
            Box::pin(async move { Err(SupervisorError::NotFound(id.clone())) })
        }

        fn retire<'a>(&'a self, _: &'a ServiceId) -> Operation<'a, ()> {
            Box::pin(async {
                Err(SupervisorError::Timeout {
                    operation: "retire",
                })
            })
        }
    }

    let harness = Harness::scripted(CONNECT, INITIALIZE);
    let supervisor = Unretirable;
    let lifecycle = Lifecycle {
        supervisor: &supervisor,
        ..harness.lifecycle()
    };
    let cause = lifecycle_error("worker_initialize_failed", "scripted");

    let failure = lifecycle.abandon(&harness.generation("s-1"), cause).await;

    assert!(
        matches!(&failure, LaunchFailure::Unavailable(error) if error.code == SUPERVISION_UNAVAILABLE),
        "{failure:?}"
    );
}

fn previous(harness: &Harness, worker_id: Option<&str>) -> (Generation, PreviousGeneration) {
    let generation = harness.generation("s-1");
    let previous = PreviousGeneration {
        session_id: "s-1".to_owned(),
        generation: Some(generation.clone()),
        worker_id: worker_id.map(ToOwned::to_owned),
    };
    (generation, previous)
}

#[tokio::test]
async fn recovery_refuses_while_the_previous_generation_runs() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::Present {
        state: ServiceState::Running,
        process: true,
    }]);
    let (generation, previous) = previous(&harness, Some("worker-live"));
    harness.write_journal(
        "s-1",
        "worker-live",
        &journal(
            "s-1",
            "worker-live",
            generation.generation(),
            "live",
            std::process::id(),
        ),
    );

    let refusal = harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect_err("previous generation is live");

    assert!(
        matches!(&refusal, PreviousLive::Ambiguous(error) if error.code == SUPERVISION_AMBIGUOUS),
        "{refusal:?}"
    );
    assert!(harness.supervisor.retired().is_empty());
}

#[tokio::test]
async fn recovery_refuses_while_the_previous_worker_process_runs_outside_the_job() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::NotFound]);
    let (generation, previous) = previous(&harness, Some("worker-orphan"));
    harness.write_journal(
        "s-1",
        "worker-orphan",
        &journal(
            "s-1",
            "worker-orphan",
            generation.generation(),
            "live",
            std::process::id(),
        ),
    );

    let refusal = harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect_err("previous worker process is alive");

    assert!(matches!(refusal, PreviousLive::Ambiguous(_)), "{refusal:?}");
    assert!(harness.supervisor.retired().is_empty());
}

#[tokio::test]
async fn recovery_refuses_when_the_previous_generation_cannot_be_inspected() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness
        .supervisor
        .script_inspects([InspectStep::Unavailable]);
    let (_, previous) = previous(&harness, None);

    let refusal = harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect_err("supervisor outage");

    assert!(
        matches!(&refusal, PreviousLive::Unavailable(error) if error.code == SUPERVISION_UNAVAILABLE),
        "{refusal:?}"
    );
    assert!(harness.supervisor.retired().is_empty());
}

#[tokio::test]
async fn recovery_refuses_a_journal_of_another_generation() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::NotFound]);
    let (_, previous) = previous(&harness, Some("worker-other"));
    harness.write_journal(
        "s-1",
        "worker-other",
        &journal(
            "s-1",
            "worker-other",
            "zzzz2345",
            "terminal",
            std::process::id(),
        ),
    );

    let refusal = harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect_err("journal names another generation");

    assert!(
        matches!(&refusal, PreviousLive::Ambiguous(error) if error.code == IDENTITY_MISMATCH),
        "{refusal:?}"
    );
    assert!(harness.supervisor.retired().is_empty());
}

#[tokio::test]
async fn recovery_refuses_a_previous_worker_whose_journal_is_corrupted() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::NotFound]);
    let (_, previous) = previous(&harness, Some("worker-corrupt"));
    harness.write_journal_bytes("s-1", "worker-corrupt", b"{\"schema_version\": 4, trunc");

    let refusal = harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect_err("a corrupted journal proves nothing");

    assert!(
        matches!(&refusal, PreviousLive::Ambiguous(error) if error.code == SUPERVISION_AMBIGUOUS),
        "{refusal:?}"
    );
    assert!(harness.supervisor.retired().is_empty());
    assert!(
        harness.supervisor.calls().is_empty(),
        "the job is not inspected before the journal is proven readable"
    );
}

#[tokio::test]
async fn recovery_refuses_a_previous_journal_of_another_worker() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::NotFound]);
    let (generation, previous) = previous(&harness, Some("worker-mine"));
    harness.write_journal(
        "s-1",
        "worker-mine",
        &journal(
            "s-1",
            "worker-foreign",
            generation.generation(),
            "live",
            std::process::id(),
        ),
    );

    let refusal = harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect_err("a journal of another worker proves nothing");

    assert!(
        matches!(&refusal, PreviousLive::Ambiguous(error) if error.code == SUPERVISION_AMBIGUOUS),
        "{refusal:?}"
    );
    assert!(harness.supervisor.retired().is_empty());
}

#[tokio::test]
async fn recovery_retires_a_previous_generation_whose_worker_never_journaled() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness
        .supervisor
        .script_inspects([InspectStep::NotFound, InspectStep::NotFound]);
    let (generation, previous) = previous(&harness, Some("worker-silent"));

    harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect("a missing journal leaves nothing to cross-check");
    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);

    harness.write_journal(
        "s-1",
        "worker-sibling",
        &journal(
            "s-1",
            "worker-sibling",
            generation.generation(),
            "terminal",
            std::process::id(),
        ),
    );
    harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect("a missing journal next to another worker's journal is still missing");
    assert_eq!(
        harness.supervisor.retired(),
        [generation.service_id(), generation.service_id()]
    );
}

/// PID of a process that has exited and been reaped.
fn exited_pid() -> u32 {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn short-lived process");
    let pid = child.id();
    child.wait().expect("reap short-lived process");
    pid
}

/// A loaded job without a process, as launchd reports one.
const LOADED_WITHOUT_PROCESS: InspectStep = InspectStep::Present {
    state: ServiceState::Unknown,
    process: false,
};

#[tokio::test]
async fn recovery_retires_a_loaded_job_without_a_process_once_its_journaled_worker_is_gone() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([LOADED_WITHOUT_PROCESS]);
    let (generation, previous) = previous(&harness, Some("worker-gone"));
    harness.write_journal(
        "s-1",
        "worker-gone",
        &journal(
            "s-1",
            "worker-gone",
            generation.generation(),
            "live",
            exited_pid(),
        ),
    );

    harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect("the journaled worker is proven gone");

    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);
}

#[tokio::test]
async fn recovery_refuses_a_loaded_job_without_a_process_while_its_journaled_worker_runs() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([LOADED_WITHOUT_PROCESS]);
    let (generation, previous) = previous(&harness, Some("worker-hidden"));
    harness.write_journal(
        "s-1",
        "worker-hidden",
        &journal(
            "s-1",
            "worker-hidden",
            generation.generation(),
            "live",
            std::process::id(),
        ),
    );

    let refusal = harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect_err("the journaled worker still runs");

    assert!(
        matches!(&refusal, PreviousLive::Ambiguous(error) if error.code == SUPERVISION_AMBIGUOUS),
        "{refusal:?}"
    );
    assert!(harness.supervisor.retired().is_empty());
}

#[tokio::test]
async fn recovery_refuses_a_loaded_job_without_a_process_or_a_journal() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([LOADED_WITHOUT_PROCESS]);
    let (_, previous) = previous(&harness, None);

    let refusal = harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect_err("nothing proves the job ended");

    assert!(
        matches!(&refusal, PreviousLive::Ambiguous(error) if error.code == SUPERVISION_AMBIGUOUS),
        "{refusal:?}"
    );
    assert!(harness.supervisor.retired().is_empty());
}

#[tokio::test]
async fn recovery_retires_a_previous_worker_that_only_retains_terminal_output() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::Present {
        state: ServiceState::Running,
        process: true,
    }]);
    let (generation, previous) = previous(&harness, Some("worker-done"));
    harness.write_journal(
        "s-1",
        "worker-done",
        &journal(
            "s-1",
            "worker-done",
            generation.generation(),
            "terminal",
            std::process::id(),
        ),
    );

    harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect("terminal previous generation is retired");

    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);
}

/// Scripts the previous generation's job as running with `definition`, with
/// this test process as its main process.
fn script_previous_job(harness: &Harness, generation: &Generation, definition: DefinitionFacts) {
    harness.supervisor.script_job(
        generation.service_id(),
        JobScript::Present {
            state: ServiceState::Running,
            process: Some(own_process()),
            definition: Some(definition),
        },
    );
}

/// Writes a terminal journal of `generation` whose worker is this process.
fn write_terminal_journal(harness: &Harness, generation: &Generation, worker_id: &str) {
    harness.write_journal(
        "s-1",
        worker_id,
        &journal(
            "s-1",
            worker_id,
            generation.generation(),
            "terminal",
            std::process::id(),
        ),
    );
}

#[tokio::test]
async fn recovery_refuses_a_foreign_job_under_the_previous_service_id() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    let (generation, previous) = previous(&harness, Some("worker-done"));
    let other = harness.generation("s-1");
    let own = job_definition(&harness.config, &generation)
        .expect("valid definition")
        .facts();
    let foreign_definitions = [
        job_definition(&harness.config, &other)
            .expect("valid definition")
            .facts(),
        DefinitionFacts {
            executable: PathBuf::from("/usr/bin/unrelated"),
            ..own
        },
    ];
    write_terminal_journal(&harness, &generation, "worker-done");

    for definition in foreign_definitions {
        script_previous_job(&harness, &generation, definition);

        let refusal = harness
            .lifecycle()
            .retire_previous(&previous)
            .await
            .expect_err("the job is not the previous generation's");

        assert!(
            matches!(&refusal, PreviousLive::Ambiguous(error) if error.code == IDENTITY_MISMATCH),
            "{refusal:?}"
        );
        assert!(harness.supervisor.retired().is_empty());
    }
}

#[tokio::test]
async fn recovery_refuses_a_previous_job_whose_process_is_not_the_journaled_worker() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    let (generation, previous) = previous(&harness, Some("worker-done"));
    let definition = job_definition(&harness.config, &generation)
        .expect("valid definition")
        .facts();
    script_previous_job(&harness, &generation, definition);
    harness.write_journal(
        "s-1",
        "worker-done",
        &journal(
            "s-1",
            "worker-done",
            generation.generation(),
            "terminal",
            exited_pid(),
        ),
    );

    let refusal = harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect_err("the job's process is not the journaled worker");

    assert!(
        matches!(&refusal, PreviousLive::Ambiguous(error) if error.code == IDENTITY_MISMATCH),
        "{refusal:?}"
    );
    assert!(harness.supervisor.retired().is_empty());
}

#[tokio::test]
async fn recovery_retires_a_previous_job_that_matches_its_generation() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    let (generation, previous) = previous(&harness, Some("worker-done"));
    let definition = job_definition(&harness.config, &generation)
        .expect("valid definition")
        .facts();
    script_previous_job(&harness, &generation, definition);
    write_terminal_journal(&harness, &generation, "worker-done");

    harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect("the job is exactly the previous generation");

    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);
}

#[tokio::test]
async fn recovery_cleans_up_an_ended_previous_generation() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::Present {
        state: ServiceState::Failed,
        process: false,
    }]);
    let (generation, previous) = previous(&harness, None);

    harness
        .lifecycle()
        .retire_previous(&previous)
        .await
        .expect("ended previous generation is cleaned up");

    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);
}

#[tokio::test(start_paused = true)]
async fn one_session_is_serialized_and_different_sessions_are_not() {
    let locks = SessionLocks::default();
    let first = locks.acquire("s-1").await;

    let other = tokio::time::timeout(Duration::from_secs(1), locks.acquire("s-2"))
        .await
        .expect("another session is not blocked");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), locks.acquire("s-1"))
            .await
            .is_err(),
        "a second operation on the same session must wait"
    );

    drop(first);
    let second = tokio::time::timeout(Duration::from_secs(1), locks.acquire("s-1"))
        .await
        .expect("the session is released");
    drop((second, other));
    assert_eq!(locks.tracked(), 0, "released sessions leave no lock entry");
}

/// Serves one controller connection as a worker that only speaks version five.
///
/// Returns the request kinds received after the controller lease, so a test
/// can prove the daemon never sent `Initialize`.
fn serve_version_five_worker(
    socket: &Path,
    session_id: &str,
    worker_id: &str,
) -> tokio::task::JoinHandle<Vec<String>> {
    serve_gated_version_five_worker(socket, session_id, worker_id, None)
}

/// Holds a fake worker's `ControllerAcquired` reply until released.
#[derive(Debug, Clone, Default)]
struct AcquireGate {
    /// Notified once the controller request arrived.
    entered: Arc<Notify>,
    /// Releases the held reply.
    release: Arc<Notify>,
}

/// [`serve_version_five_worker`] whose controller reply waits for `gate`.
fn serve_gated_version_five_worker(
    socket: &Path,
    session_id: &str,
    worker_id: &str,
    gate: Option<AcquireGate>,
) -> tokio::task::JoinHandle<Vec<String>> {
    serve_fake_version_five_worker(socket, session_id, worker_id, gate, 0, None).0
}

/// Serves controller connections as a version-five worker, rejecting the
/// controller request of the first `busy` connections with
/// `ControllerBusy`.
///
/// Returns the task, which ends with the request kinds received after the
/// lease of the first served connection, and the count of busy rejections.
/// When `refusal_closed` is set, the worker sends one unit on it each time the
/// daemon has closed a connection after receiving a busy rejection.
fn serve_fake_version_five_worker(
    socket: &Path,
    session_id: &str,
    worker_id: &str,
    gate: Option<AcquireGate>,
    busy: usize,
    refusal_closed: Option<tokio::sync::mpsc::UnboundedSender<()>>,
) -> (tokio::task::JoinHandle<Vec<String>>, Arc<AtomicU64>) {
    use pohunek_worker_protocol::{
        Capability, ControlError, ControlMessage, ControlReader, ControlResponse, ControlWriter,
        LeaseChallenge, LeaseId, ProcessIdentity as WireProcess, RequestKind, ResponseKind,
        RuntimePhase, SessionId, Version, VersionRange, WorkerId,
    };

    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(socket.parent().expect("socket directory"))
        .expect("create socket directory");
    let listener = tokio::net::UnixListener::bind(socket).expect("bind fake worker");
    let session_id = SessionId::new(session_id).expect("session id");
    let worker_id = WorkerId::new(worker_id).expect("worker id");
    let rejections = Arc::new(AtomicU64::new(0));
    let rejected = Arc::clone(&rejections);
    let task = tokio::spawn(async move {
        let mut connection = 0_usize;
        loop {
            let (stream, _) = listener.accept().await.expect("accept controller");
            let reject_busy = connection < busy;
            connection = connection.saturating_add(1);
            let (read_half, write_half) = stream.into_split();
            let mut reader = ControlReader::new(read_half);
            let mut writer = ControlWriter::new(write_half);
            let five = Version::new(5).expect("version five");
            let mut after_lease = Vec::new();
            let mut refused = false;
            // A client that saw a rejection closes its connection, which
            // ends this loop with an error or EOF.
            while let Ok(Some(message)) = reader.read::<ControlMessage>().await {
                let ControlMessage::Request(request) = message else {
                    panic!("daemon sent a non-request message");
                };
                let kind = match request.kind {
                    RequestKind::Negotiate { .. } => ResponseKind::Negotiated {
                        selected_version: five,
                        supported_range: VersionRange::new(Version::new(4).expect("v4"), five)
                            .expect("range"),
                        session_id: session_id.clone(),
                        worker_id: worker_id.clone(),
                        runtime_id: None,
                        worker_process: WireProcess {
                            pid: std::process::id(),
                            start_identity: 1,
                        },
                        phase: RuntimePhase::Uninitialized,
                        capabilities: vec![Capability::AtomicReplay],
                        challenge: LeaseChallenge::new("challenge-v5").expect("challenge"),
                    },
                    RequestKind::AcquireController { .. } if reject_busy => {
                        rejected.fetch_add(1, Ordering::Relaxed);
                        refused = true;
                        ResponseKind::Error {
                            error: ControlError {
                                code: ControlCode::ControllerBusy,
                                message: "another controller holds the lease".to_owned(),
                                retryable: true,
                            },
                        }
                    }
                    RequestKind::AcquireController { .. } => {
                        if let Some(gate) = &gate {
                            gate.entered.notify_one();
                            gate.release.notified().await;
                        }
                        ResponseKind::ControllerAcquired {
                            lease_id: LeaseId::new("lease-v5").expect("lease"),
                            capabilities: Vec::new(),
                        }
                    }
                    other => {
                        after_lease.push(format!("{other:?}"));
                        continue;
                    }
                };
                if writer
                    .write(&ControlMessage::Response(ControlResponse {
                        request_id: request.request_id,
                        kind,
                    }))
                    .await
                    .is_err()
                    || writer.flush().await.is_err()
                {
                    break;
                }
            }
            if refused {
                if let Some(closed) = &refusal_closed {
                    // The receiver is gone once the test stopped listening.
                    let _ = closed.send(());
                }
            }
            if !reject_busy {
                return after_lease;
            }
        }
    });
    (task, rejections)
}

#[tokio::test]
async fn a_new_generation_below_the_base_environment_protocol_is_retired_uninitialized() {
    let harness = Harness::scripted(GENEROUS, INITIALIZE);
    let generation = harness.generation("s-1");
    harness.write_journal(
        "s-1",
        "worker-v5",
        &journal(
            "s-1",
            "worker-v5",
            generation.generation(),
            "bootstrap",
            std::process::id(),
        ),
    );
    // The job runs while its worker (the fake) serves the socket.
    harness.supervisor.script_inspects([InspectStep::Present {
        state: ServiceState::Running,
        process: true,
    }]);
    let fake = serve_version_five_worker(
        &harness
            .runtime_root
            .join("s-1")
            .join(pohunek_paths::WORKER_SOCKET_NAME),
        "s-1",
        "worker-v5",
    );

    let failure = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect_err("a version-five worker must not start a new generation");

    assert!(
        matches!(&failure, LaunchFailure::Cleaned(error) if error.code == WORKER_PROTOCOL_OUTDATED),
        "{failure:?}"
    );
    assert_eq!(harness.supervisor.started(), [generation.service_id()]);
    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);
    let after_lease = tokio::time::timeout(GENEROUS, fake)
        .await
        .expect("controller connection closed")
        .expect("fake worker task");
    assert!(
        after_lease.is_empty(),
        "no request may follow the lease, got {after_lease:?}"
    );
}

#[tokio::test]
async fn a_live_version_five_worker_stays_adoptable_by_reconnection() {
    let harness = Harness::scripted(GENEROUS, INITIALIZE);
    let socket = harness
        .runtime_root
        .join("s-1")
        .join(pohunek_paths::WORKER_SOCKET_NAME);
    let fake = serve_version_five_worker(&socket, "s-1", "worker-v5");

    // Reconciliation reconnects through the worker client directly, never
    // through `Lifecycle::launch`, so the version floor does not apply.
    let worker = Worker::connect_discovered(&socket, "daemon-after-upgrade")
        .await
        .expect("reconnect to a live version-five worker");

    assert_eq!(
        worker.selected_version().await,
        pohunek_worker_protocol::Version::new(5).expect("version five")
    );
    drop(worker);
    fake.await.expect("fake worker task");
}

/// Connect deadline that a worker which already exited must never wait for.
const NEVER: Duration = Duration::from_secs(600);

/// Fatal line the exiting fake worker writes to stderr.
const FAKE_WORKER_FATAL: &str = "pohunek-sessiond: fatal: scripted startup failure";

/// Exit code of the exiting fake worker.
const FAKE_WORKER_EXIT_CODE: i32 = 3;

/// Writes an executable that prints [`FAKE_WORKER_FATAL`] to stderr and
/// exits with [`FAKE_WORKER_EXIT_CODE`] before binding any socket.
pub(crate) fn write_exiting_worker(directory: &Path) -> PathBuf {
    let path = directory.join("exiting-sessiond");
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(&path)
        .expect("create fake worker");
    std::io::Write::write_all(
        &mut file,
        format!("#!/bin/sh\necho '{FAKE_WORKER_FATAL}' >&2\nexit {FAKE_WORKER_EXIT_CODE}\n")
            .as_bytes(),
    )
    .expect("write fake worker");
    file.sync_all().expect("flush fake worker");
    drop(file);
    path
}

/// Collects formatted log output of the test's default subscriber.
#[derive(Debug, Clone, Default)]
pub(crate) struct LogCapture(Arc<StdMutex<Vec<u8>>>);

impl LogCapture {
    /// Installs a capturing subscriber as this thread's default.
    pub(crate) fn install(&self) -> tracing::subscriber::DefaultGuard {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(self.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    /// Returns everything logged so far.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log capture")).into_owned()
    }
}

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log capture").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn a_subprocess_worker_that_exits_at_once_ends_the_wait_with_its_status_and_stderr() {
    let logs = LogCapture::default();
    let _subscriber = logs.install();
    let roots = Roots::new();
    let runtime_root = roots.base.join("r");
    let state_root = roots.base.join("s");
    let executable = write_exiting_worker(&roots.base);
    let config = SubprocessWorkerEnvironment {
        runtime_home: runtime_root.clone(),
        state_home: state_root.clone(),
        data_home: state_root.clone(),
        config_home: state_root.clone(),
        cache_home: state_root.clone(),
        home: roots.base.clone(),
        daemon_socket: runtime_root.join("daemon.sock"),
    }
    .supervision(executable.clone())
    .with_environment_source(crate::test_support::thread_environment_source());
    let supervisor = ScriptedSupervisor::over(crate::runtime::SubprocessWorkerLauncher::new());
    let inspector = HostInspector::new();
    let lifecycle = Lifecycle {
        supervisor: &supervisor,
        config: &config,
        runtime_root: &runtime_root,
        state_root: &state_root,
        daemon_instance_id: "lifecycle-test",
        connect_deadline: NEVER,
        inspector: &inspector,
    };
    let generation = Generation::mint("s-1", executable).expect("mint");

    let failure = tokio::time::timeout(GENEROUS, lifecycle.launch(&generation))
        .await
        .expect("an exited worker ends the wait long before the connect deadline")
        .expect_err("the worker never serves");

    assert!(
        matches!(
            &failure,
            LaunchFailure::Cleaned(error) if error.code == WORKER_EXITED_BEFORE_READY
                && error.msg.contains(&format!("exit code {FAKE_WORKER_EXIT_CODE}"))
        ),
        "{failure:?}"
    );
    assert_eq!(supervisor.retired(), [generation.service_id()]);
    let logged = logs.text();
    assert!(
        logged.contains(FAKE_WORKER_FATAL) && logged.contains("stderr.bytes"),
        "the worker's fatal message reaches the daemon log: {logged}"
    );
}

#[tokio::test]
async fn a_worker_socket_that_cannot_be_bound_is_refused_before_its_job_starts() {
    let harness = Harness::scripted(GENEROUS, INITIALIZE);
    let limit = Platform::current()
        .expect("supported test platform")
        .socket_path_max_bytes();
    let published_tail = format!("/s-1/{}", pohunek_paths::WORKER_SOCKET_NAME);
    let base = harness.runtime_root.as_os_str().len();
    // The published socket sits exactly at the limit, so only the longer
    // staged bind path is over it.
    let runtime_root = harness
        .runtime_root
        .join("p".repeat(limit - base - published_tail.len() - 1));
    let lifecycle = Lifecycle {
        runtime_root: &runtime_root,
        ..harness.lifecycle()
    };
    let generation = harness.generation("s-1");

    let failure = lifecycle
        .launch(&generation)
        .await
        .expect_err("the worker could never bind its socket");

    let staged = pohunek_paths::longest_staged_socket_path(&runtime_root.join("s-1"));
    assert!(
        matches!(
            &failure,
            LaunchFailure::Cleaned(error) if error.code == WORKER_SOCKET_PATH_INVALID
                && error.class == ErrorClass::Configuration
                && error.msg.contains(&staged.display().to_string())
                && error.msg.contains(&limit.to_string())
        ),
        "{failure:?}"
    );
    assert!(harness.supervisor.calls().is_empty(), "no job was started");
}

#[tokio::test]
async fn a_worker_is_adopted_while_its_job_inspection_is_wedged() {
    let harness = Harness::over_worker(GENEROUS, GENEROUS);
    harness
        .supervisor
        .script_starts([StartStep::AcceptLate(CONNECT * 2)]);
    harness.supervisor.script_inspects([InspectStep::Block]);
    let generation = harness.generation("s-1");

    let worker = tokio::time::timeout(GENEROUS, harness.lifecycle().launch(&generation))
        .await
        .expect("a wedged inspection must not stall the socket attempts")
        .expect("the late worker is adopted");

    let journal = read_journal(
        &harness.state_root,
        "s-1",
        worker.worker_id().await.as_str(),
    )
    .expect("worker journal");
    assert_eq!(journal.generation, generation.generation());
    assert!(harness.supervisor.retired().is_empty());
    harness
        .lifecycle()
        .retire(&generation)
        .await
        .expect("retire test worker");
}

#[tokio::test(start_paused = true)]
async fn a_wedged_job_inspection_never_extends_the_connect_deadline() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::Block]);
    let generation = harness.generation("s-1");
    let started = Instant::now();
    let deadline = started + CONNECT;

    let outcome = tokio::time::timeout(
        GENEROUS,
        harness.lifecycle().connect_until(&generation, deadline),
    )
    .await
    .expect("the wait ends at the connect deadline");

    assert!(matches!(outcome, Err(NotReady::Elapsed)), "{outcome:?}");
    assert_eq!(
        Instant::now(),
        deadline,
        "the wait ends exactly at the connect deadline"
    );
}

#[tokio::test]
async fn an_end_report_never_cancels_a_connect_the_worker_is_answering() {
    let harness = Harness::scripted(GENEROUS, INITIALIZE);
    // The job reads as absent from the first inspection on.
    harness.supervisor.script_inspects([InspectStep::NotFound]);
    let generation = harness.generation("s-1");
    harness.write_journal(
        "s-1",
        "worker-v5",
        &journal(
            "s-1",
            "worker-v5",
            generation.generation(),
            "bootstrap",
            std::process::id(),
        ),
    );
    let gate = AcquireGate::default();
    // The fake accepts one connection only, so a replacement attempt after a
    // cancelled one could never be adopted.
    let fake = serve_gated_version_five_worker(
        &harness
            .runtime_root
            .join("s-1")
            .join(pohunek_paths::WORKER_SOCKET_NAME),
        "s-1",
        "worker-v5",
        Some(gate.clone()),
    );

    let lifecycle = harness.lifecycle();
    let connecting = lifecycle.connect_until(&generation, Instant::now() + GENEROUS);
    tokio::pin!(connecting);
    tokio::select! {
        outcome = &mut connecting => panic!("the held controller reply cannot complete: {outcome:?}"),
        () = gate.entered.notified() => {}
    }
    assert!(
        harness
            .supervisor
            .calls()
            .contains(&Call::Inspect(generation.service_id())),
        "the job end was reported while the controller reply is held"
    );
    gate.release.notify_one();

    let worker = connecting
        .await
        .expect("the answering worker is adopted despite the end report");
    assert_eq!(worker.worker_id().await.as_str(), "worker-v5");
    drop(worker);
    tokio::time::timeout(GENEROUS, fake)
        .await
        .expect("controller connection closed")
        .expect("fake worker task");
}

#[tokio::test(start_paused = true)]
async fn a_job_end_at_the_connect_deadline_outranks_the_deadline() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    let live = InspectStep::Present {
        state: ServiceState::Running,
        process: true,
    };
    // Inspections run at 0 and 100 ms, then at the 150 ms deadline, which is
    // also when the last socket attempt fails.
    harness
        .supervisor
        .script_inspects([live, live, InspectStep::NotFound]);
    let generation = harness.generation("s-1");
    let deadline = Instant::now() + CONNECT;

    let outcome = harness
        .lifecycle()
        .connect_until(&generation, deadline)
        .await;

    assert_eq!(
        Instant::now(),
        deadline,
        "both outcomes fall on the deadline"
    );
    assert!(
        matches!(outcome, Err(NotReady::Ended(JobEnd::Absent))),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_busy_controller_lease_is_retried_until_the_worker_is_adopted() {
    let harness = Harness::scripted(GENEROUS, INITIALIZE);
    let generation = harness.generation("s-1");
    harness.write_journal(
        "s-1",
        "worker-v5",
        &journal(
            "s-1",
            "worker-v5",
            generation.generation(),
            "bootstrap",
            std::process::id(),
        ),
    );
    let (fake, rejections) = serve_fake_version_five_worker(
        &harness
            .runtime_root
            .join("s-1")
            .join(pohunek_paths::WORKER_SOCKET_NAME),
        "s-1",
        "worker-v5",
        None,
        1,
        None,
    );

    // One attempt, as after a job-end report, must outlast the busy lease.
    let worker = harness
        .lifecycle()
        .try_connect(&generation, Instant::now() + GENEROUS, None)
        .await
        .expect("the worker is adopted once its lease is free");

    assert_eq!(worker.worker_id().await.as_str(), "worker-v5");
    assert_eq!(rejections.load(Ordering::Relaxed), 1);
    drop(worker);
    tokio::time::timeout(GENEROUS, fake)
        .await
        .expect("controller connection closed")
        .expect("fake worker task");
}

/// The busy-lease retry timeline runs on a paused clock that moves only by the
/// test's `advance` calls. The fake worker reports each refusal once the
/// daemon has closed that connection, which in the daemon's single task
/// happens strictly before it arms the retry sleep. The test therefore
/// advances by one retry step only while the retry sleep is armed, so every
/// retry slot before the deadline holds exactly one refusal and the attempt
/// ends at the deadline whatever the host load, the socket latency or the
/// scheduling.
#[tokio::test(start_paused = true)]
async fn a_controller_lease_busy_until_the_deadline_ends_with_the_deadline() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    let generation = harness.generation("s-1");
    let (refusal_closed, mut closed_refusals) = tokio::sync::mpsc::unbounded_channel();
    let (fake, rejections) = serve_fake_version_five_worker(
        &harness
            .runtime_root
            .join("s-1")
            .join(pohunek_paths::WORKER_SOCKET_NAME),
        "s-1",
        "worker-v5",
        None,
        usize::MAX,
        Some(refusal_closed),
    );
    let lifecycle = harness.lifecycle();
    let deadline = Instant::now() + CONNECT;
    let attempt = async {
        let outcome = lifecycle.try_connect(&generation, deadline, None).await;
        (outcome, Instant::now())
    };
    let timeline = async {
        while Instant::now() < deadline {
            wait::guard("the busy refusal of a retry slot", closed_refusals.recv())
                .await
                .expect("the fake worker outlives the attempt");
            tokio::time::advance(SUPERVISION_POLL_INTERVAL.min(deadline - Instant::now())).await;
        }
        // The clock is at the deadline. Any further wait of the attempt now
        // auto-advances the clock past it, so an attempt that outlives its
        // deadline fails the equality below instead of hanging.
        drop(inhibitor);
    };

    let ((outcome, ended), ()) =
        wait::guard("a busy lease never extends the attempt deadline", async {
            tokio::join!(attempt, timeline)
        })
        .await;

    assert!(
        outcome.is_none(),
        "a lease busy until the deadline adopts nothing"
    );
    assert_eq!(
        ended, deadline,
        "the attempt ends exactly at its deadline while the lease stays busy"
    );
    // Every attempt that starts before the deadline is refused once; the one
    // that would start at the deadline is cut off before it is answered.
    let retry_slots = CONNECT
        .as_millis()
        .div_ceil(SUPERVISION_POLL_INTERVAL.as_millis());
    assert_eq!(
        u128::from(rejections.load(Ordering::Relaxed)),
        retry_slots,
        "the busy lease was retried every poll interval within the deadline"
    );
    fake.abort();
}

#[tokio::test]
async fn an_ended_job_stops_the_busy_lease_retries_after_the_first_refusal() {
    let harness = Harness::scripted(NEVER, INITIALIZE);
    // The job reads as absent from the first inspection on.
    harness.supervisor.script_inspects([InspectStep::NotFound]);
    let generation = harness.generation("s-1");
    let (fake, rejections) = serve_fake_version_five_worker(
        &harness
            .runtime_root
            .join("s-1")
            .join(pohunek_paths::WORKER_SOCKET_NAME),
        "s-1",
        "worker-v5",
        None,
        usize::MAX,
        None,
    );

    let outcome = tokio::time::timeout(
        GENEROUS,
        harness
            .lifecycle()
            .connect_until(&generation, Instant::now() + NEVER),
    )
    .await
    .expect("an ended job does not wait out a busy lease until the connect deadline");

    assert!(
        matches!(outcome, Err(NotReady::Ended(JobEnd::Absent))),
        "{outcome:?}"
    );
    assert_eq!(
        rejections.load(Ordering::Relaxed),
        1,
        "the attempt in flight completed, and no retry followed the end report"
    );
    fake.abort();
}
