//! Branch coverage for the worker lifecycle engine.
//!
//! [`ScriptedSupervisor`] answers `start` and `inspect` from a script so every
//! reconciliation branch is reachable deterministically. When it wraps the
//! in-process launcher, unscripted calls run a real worker server, which the
//! session registry tests use to drive whole lifecycle transactions.

use std::collections::VecDeque;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;

use pohunek_platform::process::HostInspector;
use pohunek_platform::supervisor::{
    DefinitionFacts, Operation, Supervisor, BOOTSTRAP_ENV_ALLOWLIST,
};
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
    delegate: Option<Arc<InProcessWorkerLauncher>>,
    starts: StdMutex<VecDeque<StartStep>>,
    inspects: StdMutex<VecDeque<InspectStep>>,
    calls: StdMutex<Vec<Call>>,
    gate: StdMutex<Option<StartGate>>,
    store: Option<PathBuf>,
    persisted_at_start: StdMutex<Vec<Option<String>>>,
}

impl ScriptedSupervisor {
    /// A supervisor without a real worker; unscripted inspections are absent.
    pub(crate) fn scripted() -> Self {
        Self::default()
    }

    /// A supervisor over a real in-process worker server.
    pub(crate) fn over(delegate: InProcessWorkerLauncher) -> Self {
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
            match &self.delegate {
                Some(delegate) => delegate.discover().await,
                None => Ok(Vec::new()),
            }
        })
    }

    fn inspect<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ServiceObservation> {
        Box::pin(async move {
            self.record(Call::Inspect(id.clone()));
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
            match &self.delegate {
                Some(delegate) => delegate.retire(id).await,
                None => Err(SupervisorError::NotFound(id.clone())),
            }
        })
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
        let base = std::env::temp_dir().join(format!(
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
        ));
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
        std::io::Write::write_all(&mut file, facts.to_string().as_bytes()).expect("write journal");
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
    #[cfg(target_os = "macos")]
    assert!(definition.logs().is_some());
    #[cfg(not(target_os = "macos"))]
    assert!(definition.logs().is_none());
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

#[tokio::test]
async fn an_absent_job_after_the_connect_deadline_is_cleaned_up() {
    let harness = Harness::scripted(CONNECT, INITIALIZE);
    harness.supervisor.script_inspects([InspectStep::NotFound]);
    let generation = harness.generation("s-1");

    let failure = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect_err("no worker");

    assert!(
        matches!(&failure, LaunchFailure::Cleaned(error) if error.code == "worker_connect_failed"),
        "{failure:?}"
    );
    assert_eq!(harness.supervisor.retired(), [generation.service_id()]);
}

#[tokio::test]
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

#[tokio::test]
async fn a_job_that_ended_without_a_process_is_retired_immediately() {
    let harness = Harness::scripted(CONNECT, GENEROUS);
    harness.supervisor.script_inspects([InspectStep::Present {
        state: ServiceState::Failed,
        process: false,
    }]);
    let generation = harness.generation("s-1");

    let failure = harness
        .lifecycle()
        .launch(&generation)
        .await
        .expect_err("worker failed");

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
    let harness = Harness::scripted(CONNECT, INITIALIZE);
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

    assert!(matches!(failure, LaunchFailure::Cleaned(_)), "{failure:?}");
    let inspections = harness
        .supervisor
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Inspect(_)))
        .count();
    assert_eq!(inspections, 3);

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
    harness
        .lifecycle()
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

#[tokio::test]
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
