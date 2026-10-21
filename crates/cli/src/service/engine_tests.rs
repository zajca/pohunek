//! Engine transaction tests against an in-memory service manager.
//!
//! The service-manager and daemon doubles exist only to inject failures and
//! interruptions at every journaled step; every filesystem effect (version
//! directories, `service.toml`, the record, the CLI copy, journals) is real
//! and lives below a temporary root.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
#[cfg(target_os = "macos")]
use std::time::SystemTime;

use package::ANCHOR_FILE_NAME;
use pohunek_client::ClientError;
use pohunek_daemon::catalog_anchor::{load_from_directory, CatalogTrust};
use pohunek_daemon::session::upgrade_preflight::PreflightInputs;
use pohunek_daemon::store::{
    DesiredState, ProjectRecord, RuntimeRecord, SessionRecord, Store as DaemonStore,
};
use pohunek_paths::BasePaths;
use pohunek_platform::process::{
    Error as ProcessError, ExitWatch, OwnershipMarkers, ProcessFact, ProcessIdentity, StartIdentity,
};
use pohunek_platform::supervisor::{DaemonSupervisor, Operation as Pending, ServiceId, Supervisor};
use pohunek_service_config::preflight::{SessionVerdict, StoreState, Verdict};
use pohunek_service_config::runtimes::{
    CatalogStatus, IntegrationAction, PackageAction, PackageStatus, ProfileAction, ProfileStatus,
    Refusal, UseReason, CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM, ENSURE_REPORT_VERSION,
};
use pohunek_session_worker::RuntimePhase;
use pohunek_test_support::wait::wait_until;
use protocol::{
    DaemonHealthResult, ProjectSource, RuntimeGeneration, RuntimeRef, RuntimeState,
    SessionCapabilities, SessionId, SessionRuntime, SessionState, StateSource,
};

use super::*;
use crate::service::backend::{Call, Control, Health};
use crate::service::context::tests::{context, temp_root};
use crate::service::definition::initial_config;
use crate::service::inherited::Token;
use crate::service::layout::tests::{anchor_bytes, stage_dir, write_anchor};
use crate::service::layout::CLI_NAME;
#[cfg(target_os = "macos")]
use crate::service::manager_env::{ActiveJob, LaunchdJob};
use crate::service::manager_env::{EnvironmentFailure, UserManager};
use crate::service::preflight::{Judging, ReleasePlan};
use crate::service::runtimes::Running;
use crate::service::usage::tests::{
    own_identity, spawn_when_exec_free, write_journal, write_live_journal, write_outdated_journal,
    SESSION,
};
use crate::service::VERSION;
use pohunek_platform::supervisor::UnitEnvironment;

#[cfg(target_os = "linux")]
#[path = "engine_tests_unit_environment.rs"]
mod unit_environment;

const V1: &str = "1.0.0";
const V2: &str = "2.0.0";
/// A supervised version older than and unrelated to the installed one.
const OLDER: &str = "0.9.0";
/// A supervised version newer than and unrelated to the installed one.
const V3: &str = "3.0.0";
const GENERATION: &str = "abcd2345";
/// Main process of the fake supervised daemon job while it runs.
const DAEMON_PID: Pid = 4_242;
/// A process of the same build that is not the supervised daemon job.
const FOREIGN_PID: Pid = 7_777;

/// Every step after which a transaction can be interrupted.
const STEPS: [Step; 8] = [
    Step::Binaries,
    Step::Config,
    Step::Definition,
    Step::Runtimes,
    Step::Registering,
    Step::Registered,
    Step::Ready,
    Step::Cli,
];

/// How the fake service manager answers a daemon registration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Registration {
    /// The job is registered and the call succeeds.
    #[default]
    Succeeds,
    /// The call fails and registers nothing.
    Fails,
    /// The job is registered but the call reports failure, like a D-Bus or
    /// `launchctl` timeout after the service manager accepted the job.
    FailsAfterRegistering,
}

/// PIDs of the stand-in processes tests started through [`spawn_tracked`].
static TRACKED: Mutex<Vec<Pid>> = Mutex::new(Vec::new());

/// Spawns a stand-in process that [`OwnProcesses`] lists in its process table.
fn spawn_tracked(command: &mut std::process::Command) -> std::process::Child {
    let child = spawn_when_exec_free(command).expect("spawn stand-in");
    TRACKED.lock().expect("tracked lock").push(child.id());
    child
}

/// Process observer whose process table is this test process plus the
/// stand-ins registered with [`spawn_tracked`].
///
/// No verdict depends on unrelated host processes, and the host process table
/// is never enumerated. Identity lookups answer for this process only; every
/// other PID, such as the one a stale journal names, is an exited process.
#[derive(Debug, Default)]
struct OwnProcesses(HostInspector);

impl OwnProcesses {
    fn own_pid() -> Pid {
        std::process::id()
    }
}

impl ProcessInspector for OwnProcesses {
    fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, ProcessError> {
        if pid == Self::own_pid() {
            self.0.identity(pid)
        } else {
            Ok(None)
        }
    }

    fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, ProcessError> {
        self.0.parent_pid(pid)
    }

    fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, ProcessError> {
        self.0.process(pid)
    }

    fn same_user_processes(&self) -> Result<Vec<ProcessFact>, ProcessError> {
        let mut processes: Vec<ProcessFact> =
            self.0.process(Self::own_pid())?.into_iter().collect();
        let tracked = TRACKED.lock().expect("tracked lock").clone();
        for pid in tracked {
            processes.extend(self.0.process(pid)?);
        }
        Ok(processes)
    }

    fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, ProcessError> {
        self.0.descendants(root)
    }

    fn cwd(&self, pid: Pid) -> Result<PathBuf, ProcessError> {
        self.0.cwd(pid)
    }

    fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, ProcessError> {
        self.0.executable(pid)
    }

    fn exit_watch(&self, identity: ProcessIdentity) -> Result<ExitWatch, ProcessError> {
        self.0.exit_watch(identity)
    }

    fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, ProcessError> {
        self.0.ownership_markers(pid)
    }

    fn foreground_process_group(&self, root_pid: Pid) -> Result<Option<Pid>, ProcessError> {
        self.0.foreground_process_group(root_pid)
    }
}

/// Program standing in for `systemd-analyze` in transactions that are not
/// about unit verification: it accepts any units, so the verdict never
/// depends on the host's user systemd.
#[cfg(target_os = "linux")]
const ACCEPTING_VERIFIER: &str = "/bin/true";

/// Judges adoption with the daemon's own rules, in this process, over the
/// store and journals below the context's paths.
///
/// The staged `pohunekd` of these tests only prints its version, so the
/// rules run through the daemon library instead of a child process; the
/// process runner has its own tests against the real binary. Process facts
/// come from [`OwnProcesses`].
#[derive(Debug, Default)]
struct InProcessPreflight {
    calls: Arc<AtomicUsize>,
}

impl AdoptionPreflight for InProcessPreflight {
    fn judge<'a>(
        &'a self,
        context: &'a Context,
        _dir: &'a VerifiedVersionDir,
        _search_path: &'a str,
        _variables: &'a BTreeMap<String, String>,
        _probe_budget: Option<Duration>,
        _plan: ReleasePlan,
    ) -> Judging<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let paths = context.paths();
            let inputs = PreflightInputs {
                store_path: paths.data_dir.join(pohunek_paths::METADATA_STORE_NAME),
                worker_state_root: paths.worker_state_root(),
                plugins_dir: paths.plugins_dir(),
            };
            Ok(pohunek_daemon::session::upgrade_preflight::run(
                &inputs,
                &OwnProcesses::default(),
            ))
        })
    }
}

/// Judges with [`InProcessPreflight`], then reports the runtime plan and the
/// stopped sessions it was given, the fields a release daemon reports.
#[derive(Debug, Default)]
struct ScriptedPreflight {
    inner: InProcessPreflight,
    runtimes: Option<EnsureReport>,
    stopped: Vec<SessionVerdict>,
    /// The `PATH` of every judgment, in call order.
    search_paths: Arc<Mutex<Vec<String>>>,
    /// The release-plan choice of every judgment, in call order.
    plans: Arc<Mutex<Vec<ReleasePlan>>>,
    /// The manager variables of every judgment, in call order.
    variables: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
}

impl AdoptionPreflight for ScriptedPreflight {
    fn judge<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a VerifiedVersionDir,
        search_path: &'a str,
        variables: &'a BTreeMap<String, String>,
        probe_budget: Option<Duration>,
        plan: ReleasePlan,
    ) -> Judging<'a> {
        Box::pin(async move {
            self.search_paths
                .lock()
                .expect("search path lock")
                .push(search_path.to_owned());
            self.plans.lock().expect("plan lock").push(plan);
            self.variables
                .lock()
                .expect("variables lock")
                .push(variables.clone());
            let mut report: PreflightReport = self
                .inner
                .judge(context, dir, search_path, variables, probe_budget, plan)
                .await?;
            report.runtimes = self.runtimes.clone();
            report.stopped_sessions = self.stopped.clone();
            Ok(report)
        })
    }
}

/// Stands in for the release's `pohunekd ensure-official-runtimes`.
///
/// A plan returns the scripted report; an apply records whether the daemon
/// was still registered when it ran, writes the state files a real apply
/// changes, and returns the scripted outcome or fails when asked to.
#[derive(Debug, Clone)]
struct RuntimeFake(Arc<Mutex<RuntimeScript>>, Fake);

#[derive(Debug, Default)]
struct RuntimeScript {
    /// The plan reported by a dry run.
    plan: Option<EnsureReport>,
    /// Whether an apply fails after it changed the state files.
    fail_apply: bool,
    /// Whether an apply reports that its daemon's exit is not confirmed.
    unconfirmed_apply: bool,
    /// Every call in order: `plan`, or `apply registered=<bool>`.
    calls: Vec<String>,
    /// The `PATH` each call ran with, in call order.
    search_paths: Vec<String>,
    /// The daemon job removals made before each dry run, in order.
    removals_at_plan: Vec<usize>,
}

impl RuntimeFake {
    fn new(fake: &Fake) -> Self {
        Self(Arc::default(), fake.clone())
    }

    fn script(&self) -> MutexGuard<'_, RuntimeScript> {
        self.0.lock().expect("runtime script lock")
    }

    fn plan(&self, plan: EnsureReport) {
        self.script().plan = Some(plan);
    }

    fn calls(&self) -> Vec<String> {
        self.script().calls.clone()
    }

    /// Whether an apply has run.
    fn applied(&self) -> bool {
        self.script()
            .calls
            .iter()
            .any(|call| call.starts_with("apply"))
    }

    /// The daemon job removals before the last two dry runs.
    fn last_two_removal_counts(&self) -> (usize, usize) {
        let counts = &self.script().removals_at_plan;
        let last = counts.len().saturating_sub(2);
        (counts[last], counts[counts.len() - 1])
    }
}

/// The report of a plan or an apply, with `packages` and `profiles`.
fn runtime_report(
    dry_run: bool,
    packages: Vec<PackageAction>,
    profiles: Vec<ProfileAction>,
    refusals: Vec<Refusal>,
) -> EnsureReport {
    EnsureReport {
        report_version: ENSURE_REPORT_VERSION,
        daemon_version: V2.to_owned(),
        dry_run,
        packages,
        catalog: CatalogStatus::Current,
        profiles,
        integrations: vec![IntegrationAction {
            runtime: "claude".to_owned(),
            profile: None,
        }],
        refusals,
        gaps: Vec::new(),
    }
}

fn package(status: PackageStatus) -> PackageAction {
    PackageAction {
        runtime: "claude".to_owned(),
        package: "pohunek.runtime.claude".to_owned(),
        version: V2.to_owned(),
        digest: "de".repeat(32),
        status,
        reasons: vec![UseReason::Profile],
        replaces_newer: None,
        release_version: None,
    }
}

fn profile(status: ProfileStatus) -> ProfileAction {
    ProfileAction {
        name: "work".to_owned(),
        base: "claude".to_owned(),
        digest: "de".repeat(32),
        status,
        previous_digest: None,
        previous_version: None,
        version: None,
        kept_reason: None,
    }
}

/// The plan of a host that needs the package and one profile pin.
fn installing_plan() -> EnsureReport {
    runtime_report(
        true,
        vec![package(PackageStatus::Install)],
        vec![profile(ProfileStatus::Bind)],
        Vec::new(),
    )
}

impl RuntimeStep for RuntimeFake {
    fn run<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a VerifiedVersionDir,
        dry_run: bool,
        search_path: &'a str,
    ) -> Running<'a> {
        Box::pin(async move {
            self.script().search_paths.push(search_path.to_owned());
            if dry_run {
                let removals = self.1.world().uninstalls;
                let mut script = self.script();
                script.calls.push("plan".to_owned());
                script.removals_at_plan.push(removals);
                return Ok(script.plan.clone().unwrap_or_else(|| {
                    runtime_report(
                        true,
                        vec![package(PackageStatus::Present)],
                        Vec::new(),
                        Vec::new(),
                    )
                }));
            }
            let registered = self.1.world().registered.is_some();
            let (fail, unconfirmed) = {
                let mut script = self.script();
                script.calls.push(format!("apply registered={registered}"));
                (script.fail_apply, script.unconfirmed_apply)
            };
            // What a real apply writes: the registry, a new catalog state, and the
            // pin of the profile.
            let plugins = context.paths().plugins_dir();
            write_private(&plugins.join("registry.json"), APPLIED_REGISTRY);
            write_private(&plugins.join("catalog-state.json"), "applied state");
            let agents = context.paths().config_dir.join("agents");
            if agents.join("work.toml").is_file() {
                pohunek_test_support::fs::write_file(agents.join("work.toml"), "applied profile")
                    .expect("profile");
            }
            if unconfirmed {
                return Err(Error::UpgradeRuntimesUnconfirmed {
                    binary: dir.daemon(),
                    detail: "the scripted daemon's exit was not confirmed".to_owned(),
                });
            }
            if fail {
                return Err(Error::UpgradeRuntimesFailed {
                    binary: dir.daemon(),
                    detail: "the fake apply failed after writing".to_owned(),
                });
            }
            Ok(runtime_report(
                false,
                vec![package(PackageStatus::Installed)],
                vec![profile(ProfileStatus::Bound)],
                Vec::new(),
            ))
        })
    }
}

/// Reinstalls nothing, so the tests never touch the test host's integrations.
#[derive(Debug)]
struct NoIntegrations;

impl IntegrationStep for NoIntegrations {
    fn reinstall<'a>(
        &'a self,
        _env: &'a IntegrationEnv,
        _actions: &'a [IntegrationAction],
        _search_path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<IntegrationReinstall>> + Send + 'a>> {
        Box::pin(async { Vec::new() })
    }
}

/// An engine over `context` and `backend` that observes only this test's
/// own processes and, on Linux, verifies units with [`ACCEPTING_VERIFIER`].
fn engine_over<'a>(context: &'a Context, backend: &'a Backend) -> Engine<'a> {
    let mut engine = Engine::new(context, backend);
    engine.inspector = Box::new(OwnProcesses::default());
    let engine = engine
        .with_adoption_preflight(Box::new(InProcessPreflight::default()))
        .with_integration_step(Box::new(NoIntegrations));
    #[cfg(target_os = "linux")]
    let engine =
        engine.with_unit_verifier(UnitVerifier::default().with_program(ACCEPTING_VERIFIER));
    engine
}

#[derive(Debug, Default)]
struct World {
    registered: Option<JobDefinition>,
    running: bool,
    registration: Registration,
    /// Process serving the socket instead of the supervised job's process.
    socket_pid: Option<Pid>,
    /// Version served by another process holding the daemon socket.
    socket_version: Option<String>,
    /// The running job reports no main process.
    processless: bool,
    /// State an inactive job is observed in; `Stopped` when unset. The launchd
    /// backend reports `Unknown` (no process) for every loaded, inactive label.
    inactive_state: Option<ServiceState>,
    fail_discover: bool,
    silent_version: Option<String>,
    /// Version whose daemon definition `replace` refuses without effect.
    refused_replace: Option<String>,
    /// Version whose daemon installation is refused during rollback.
    refused_install: Option<String>,
    /// A daemon store write made when the replacement starts.
    store_on_replace: Option<PathBuf>,
    /// A final store write made while the replacement stops.
    store_on_uninstall: Option<PathBuf>,
    installs: usize,
    /// Daemon job removals, each of which stops the daemon.
    uninstalls: usize,
    /// Daemon definition replacements, each of which restarts the daemon.
    replaces: usize,
    sessions: Vec<SessionInfo>,
    workers: Vec<ServiceObservation>,
    /// Registered jobs whose observation raced; the lenient discovery omits
    /// them the way the systemd backend does, the strict one refuses.
    raced_workers: Vec<ServiceObservation>,
    /// Strict discoveries that still race before one completes, as when a
    /// worker job is being unloaded while it is inspected.
    transient_races: u32,
}

#[derive(Debug, Clone, Default)]
struct Fake(Arc<Mutex<World>>);

impl Fake {
    fn world(&self) -> MutexGuard<'_, World> {
        self.0.lock().expect("world lock")
    }

    fn seed(&self, sessions: Vec<SessionInfo>, workers: Vec<ServiceObservation>) {
        let mut world = self.world();
        world.sessions = sessions;
        world.workers = workers;
    }

    fn daemon_version(&self) -> Option<String> {
        let world = self.world();
        world
            .registered
            .as_ref()
            .map(|definition| version_of(definition.executable()))
    }
}

fn version_of(executable: &Path) -> String {
    executable
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .expect("versioned executable")
        .to_owned()
}

fn daemon_id() -> ServiceId {
    ServiceId::parse("pohunek-test-daemon.service").expect("daemon id")
}

impl DaemonSupervisor for Fake {
    fn install<'a>(&'a self, definition: &'a JobDefinition) -> Pending<'a, ()> {
        Box::pin(async move {
            let mut world = self.world();
            world.installs += 1;
            if world.refused_install.as_deref() == Some(&*version_of(definition.executable())) {
                return Err(supervisor::Error::Operation {
                    operation: "install",
                    source: "injected restore failure".into(),
                });
            }
            if world.registration == Registration::Fails {
                return Err(supervisor::Error::Operation {
                    operation: "install",
                    source: "injected failure".into(),
                });
            }
            if world.registered.is_some() && world.running {
                return Err(supervisor::Error::AlreadyRegistered(daemon_id()));
            }
            world.registered = Some(definition.clone());
            world.running = true;
            if world.registration == Registration::FailsAfterRegistering {
                return Err(supervisor::Error::Operation {
                    operation: "install",
                    source: "injected timeout after registration".into(),
                });
            }
            Ok(())
        })
    }

    fn replace<'a>(&'a self, definition: &'a JobDefinition) -> Pending<'a, ()> {
        Box::pin(async move {
            let mut world = self.world();
            world.replaces += 1;
            if world.registered.is_none() {
                return Err(supervisor::Error::NotFound(daemon_id()));
            }
            if world.refused_replace.as_deref() == Some(&*version_of(definition.executable())) {
                return Err(supervisor::Error::Operation {
                    operation: "replace",
                    source: "injected failure".into(),
                });
            }
            world.registered = Some(definition.clone());
            world.running = true;
            if let Some(path) = world.store_on_replace.take() {
                write_new_store(&path);
            }
            Ok(())
        })
    }

    fn inspect(&self) -> Pending<'_, ServiceObservation> {
        Box::pin(async move {
            let world = self.world();
            let definition = world
                .registered
                .as_ref()
                .ok_or_else(|| supervisor::Error::NotFound(daemon_id()))?;
            Ok(ServiceObservation {
                id: daemon_id(),
                state: if world.running {
                    ServiceState::Running
                } else {
                    world.inactive_state.unwrap_or(ServiceState::Stopped)
                },
                process: (world.running && !world.processless).then_some(ProcessIdentity {
                    pid: DAEMON_PID,
                    start_identity: StartIdentity::new(1),
                }),
                definition: Some(definition.facts()),
            })
        })
    }

    fn uninstall(&self) -> Pending<'_, ()> {
        Box::pin(async move {
            let mut world = self.world();
            world.uninstalls += 1;
            world.registered = None;
            world.running = false;
            if let Some(path) = world.store_on_uninstall.take() {
                write_new_store(&path);
            }
            Ok(())
        })
    }
}

impl Supervisor for Fake {
    fn start<'a>(&'a self, id: &'a ServiceId, _definition: &'a JobDefinition) -> Pending<'a, ()> {
        Box::pin(async move {
            self.world()
                .workers
                .push(worker(id.clone(), ServiceState::Running));
            Ok(())
        })
    }

    fn discover(&self) -> Pending<'_, Vec<ServiceObservation>> {
        Box::pin(async move {
            let world = self.world();
            if world.fail_discover {
                return Err(supervisor::Error::Operation {
                    operation: "discover",
                    source: "injected failure".into(),
                });
            }
            Ok(world.workers.clone())
        })
    }

    /// The destructive discovery: refuses while an observation raced, the
    /// way the real backends refuse an incomplete job list. An injected
    /// discovery failure fails both discoveries.
    fn discover_strict(&self) -> Pending<'_, Vec<ServiceObservation>> {
        Box::pin(async move {
            let mut world = self.world();
            if world.fail_discover {
                return Err(supervisor::Error::Operation {
                    operation: "discover",
                    source: "injected failure".into(),
                });
            }
            if !world.raced_workers.is_empty() || world.transient_races > 0 {
                world.transient_races = world.transient_races.saturating_sub(1);
                return Err(supervisor::Error::Race {
                    operation: "discover",
                });
            }
            Ok(world.workers.clone())
        })
    }

    fn inspect<'a>(&'a self, id: &'a ServiceId) -> Pending<'a, ServiceObservation> {
        Box::pin(async move {
            self.world()
                .workers
                .iter()
                .find(|worker| &worker.id == id)
                .cloned()
                .ok_or_else(|| supervisor::Error::NotFound(id.clone()))
        })
    }

    fn retire<'a>(&'a self, id: &'a ServiceId) -> Pending<'a, ()> {
        Box::pin(async move {
            let mut world = self.world();
            let before = world.workers.len();
            world.workers.retain(|worker| &worker.id != id);
            if world.workers.len() == before {
                Err(supervisor::Error::NotFound(id.clone()))
            } else {
                Ok(())
            }
        })
    }
}

impl Control for Fake {
    fn health(&self) -> Call<'_, Health> {
        Box::pin(async move {
            let (running, silent, socket_pid, socket_version) = {
                let world = self.world();
                (
                    world.running,
                    world.silent_version.clone(),
                    world.socket_pid,
                    world.socket_version.clone(),
                )
            };
            match self
                .daemon_version()
                .filter(|version| running && silent.as_ref() != Some(version))
            {
                Some(version) => Ok(Health {
                    result: DaemonHealthResult {
                        status: "ok".to_owned(),
                        daemon_version: socket_version.unwrap_or(version),
                        protocol_version: protocol::PROTOCOL_VERSION,
                    },
                    pid: socket_pid.unwrap_or(DAEMON_PID),
                }),
                None => Err(unreachable()),
            }
        })
    }

    fn sessions(&self) -> Call<'_, Vec<SessionInfo>> {
        Box::pin(async move {
            if self.world().running {
                Ok(self.world().sessions.clone())
            } else {
                Err(unreachable())
            }
        })
    }

    fn stop<'a>(&'a self, id: &'a str) -> Call<'a, ()> {
        Box::pin(async move {
            let mut world = self.world();
            for session in world
                .sessions
                .iter_mut()
                .filter(|session| session.id.0 == id)
            {
                session.state = SessionState::Stopped;
                session.runtime = None;
            }
            // The stopped worker exits, and its transient job disappears.
            world.workers.retain(|worker| {
                WorkerKey::from_service_id(&worker.id).map_or(true, |key| key.session_id() != id)
            });
            Ok(())
        })
    }
}

fn unreachable() -> ClientError {
    ClientError::DaemonUnreachable {
        socket: PathBuf::from("/run/pohunek/daemon.sock"),
        source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
    }
}

fn worker(id: ServiceId, state: ServiceState) -> ServiceObservation {
    ServiceObservation {
        id,
        state,
        process: None,
        definition: None,
    }
}

fn worker_id(session: &str) -> ServiceId {
    WorkerKey::new(session, GENERATION)
        .expect("worker key")
        .service_id()
}

fn session(id: &str, name: Option<&str>) -> SessionInfo {
    SessionInfo {
        name: name.map(str::to_owned),
        id: SessionId(id.to_owned()),
        external: Some(false),
        capabilities: SessionCapabilities::default(),
        agent: "shell".to_owned(),
        agent_base: RuntimeRef::shell(),
        cwd: PathBuf::from("/work"),
        cwd_source: None,
        pid: 1,
        cols: 80,
        rows: 24,
        state: SessionState::Running,
        state_source: StateSource::Process,
        activity: None,
        subagents: Vec::new(),
        background: None,
        native_session_id: None,
        native_session_path: None,
        active_agent: None,
        active_agent_base: None,
        active_agent_pid: None,
        active_agent_session_id: None,
        active_agent_session_path: None,
        native_last_activity_at: None,
        project_id: None,
        project_label: None,
        metadata: BTreeMap::new(),
        is_linked_worktree: None,
        repo: None,
        branch: None,
        worktree_path: None,
        warnings: Vec::new(),
        capability_refusals: None,
        output_force_closed: false,
        created_at: "2026-09-24T00:00:00Z".to_owned(),
        updated_at: "2026-09-24T00:00:00Z".to_owned(),
        exit_code: None,
        runtime: Some(SessionRuntime {
            state: RuntimeState::Live,
            runtime_generation: RuntimeGeneration::new(1),
            worker_id: Some("w-1".to_owned()),
            worker_instance_id: None,
            started_at: None,
            last_connected_at: None,
            loss_reason: None,
            exit: None,
        }),
    }
}

struct Harness {
    _temp: tempfile::TempDir,
    root: PathBuf,
    prefix: PathBuf,
    context: Context,
    fake: Fake,
    runtimes: RuntimeFake,
    backend: Backend,
}

impl Harness {
    fn new() -> Self {
        let (temp, root) = temp_root();
        let prefix = root.join("prefix");
        Self::build(temp, root, prefix)
    }

    /// A second installation with its own XDG roots, and therefore its own
    /// namespace and service manager, that installs into `other`'s prefix.
    fn sharing_prefix(other: &Self) -> Self {
        let (temp, root) = temp_root();
        let harness = Self::build(temp, root, other.prefix());
        assert_ne!(
            harness.context.namespace().expect("namespace"),
            other.context.namespace().expect("namespace")
        );
        harness
    }

    fn build(temp: tempfile::TempDir, root: PathBuf, prefix: PathBuf) -> Self {
        let manager_bin = root.join(USER_MANAGER_BIN);
        std::fs::create_dir_all(&manager_bin).expect("manager bin dir");
        let context = context(&root).with_user_manager(Arc::new(FakeUserManager(Ok(vec![
            "HOME=/home/test".to_owned(),
            format!("PATH={}", manager_bin.display()),
        ]))));
        // A launchd job records the PATH its install discovered; without a login
        // shell the managed discovery records its fallback directories.
        #[cfg(target_os = "macos")]
        let context =
            context.with_path_discovery(crate::service::context::PathDiscovery::Managed {
                login_shell: None,
                shell_defaulted: false,
                fallback_directories: vec!["/usr/bin".to_owned(), "/bin".to_owned()],
            });
        let fake = Fake::default();
        let backend = Backend::new(
            Box::new(fake.clone()),
            Box::new(fake.clone()),
            Box::new(fake.clone()),
        );
        let runtimes = RuntimeFake::new(&fake);
        Self {
            _temp: temp,
            root,
            prefix,
            context,
            fake,
            runtimes,
            backend,
        }
    }

    fn engine(&self) -> Engine<'_> {
        let mut engine = engine_over(&self.context, &self.backend)
            .with_runtime_step(Box::new(self.runtimes.clone()));
        engine.ready_timeout = Duration::from_millis(500);
        engine.stop_timeout = Duration::from_secs(2);
        engine
    }

    fn prefix(&self) -> PathBuf {
        self.prefix.clone()
    }

    fn layout(&self) -> InstallLayout {
        InstallLayout::new(self.prefix()).expect("layout")
    }

    fn staged(&self, version: &str) -> PathBuf {
        stage_dir(self.root.as_path(), version)
    }

    async fn install(&self, version: &str) -> Result<InstallReport, Error> {
        self.engine()
            .install(&self.staged(version), &self.prefix(), version)
            .await
    }

    async fn upgrade(&self, version: &str) -> Result<UpgradeReport, Error> {
        self.engine().upgrade(&self.staged(version), version).await
    }

    /// Upgrades with the prepared `engine`.
    async fn upgrade_with(
        &self,
        engine: Engine<'_>,
        version: &str,
    ) -> Result<UpgradeReport, Error> {
        engine.upgrade(&self.staged(version), version).await
    }

    /// Upgrades with `--accept-runtime-loss`.
    async fn upgrade_accepting_loss(&self, version: &str) -> Result<UpgradeReport, Error> {
        self.engine()
            .with_runtime_loss_accepted(true)
            .upgrade(&self.staged(version), version)
            .await
    }

    fn config(&self) -> Option<ServiceConfig> {
        load_config(&self.context.config_path()).expect("load config")
    }

    fn pending(&self) -> Option<Record> {
        self.engine().store.load().expect("load record")
    }

    /// Asserts a completed installation of `version` and nothing else pending.
    fn assert_installed(&self, version: &str) {
        let config = self.config().expect("service.toml exists");
        assert_eq!(config.active_version(), version);
        assert_eq!(self.fake.daemon_version().as_deref(), Some(version));
        assert!(self.fake.world().running);
        assert!(self
            .layout()
            .daemon_executable(version)
            .expect("daemon path")
            .is_file());
        let cli = std::fs::read(self.layout().bin_dir().join(CLI_NAME)).expect("installed CLI");
        let staged = std::fs::read(self.staged(version).join(CLI_NAME)).expect("staged CLI");
        assert_eq!(cli, staged, "bin/pohunek is the {version} CLI");
        assert_eq!(self.pending(), None, "no transaction is pending");
    }

    /// Asserts that nothing of an installation remains.
    fn assert_clean(&self) {
        assert_eq!(self.config(), None, "service.toml is gone");
        assert!(self.fake.world().registered.is_none(), "no daemon job");
        assert!(self.fake.world().workers.is_empty(), "no worker job");
        assert!(
            layout::installed_versions(&self.layout())
                .expect("versions")
                .is_empty(),
            "no version directory"
        );
        assert_eq!(self.pending(), None, "no transaction is pending");
    }
}

#[test]
fn the_test_process_table_holds_this_process_and_its_tracked_stand_ins() {
    let mut child = spawn_tracked(std::process::Command::new("sleep").arg("30"));
    let observer = OwnProcesses::default();
    let pids: Vec<Pid> = observer
        .same_user_processes()
        .expect("process table")
        .iter()
        .map(|process| process.pid)
        .collect();
    let tracked = TRACKED.lock().expect("tracked lock").clone();
    let own = observer.identity(OwnProcesses::own_pid());
    let foreign = observer.identity(child.id());
    child.kill().expect("kill stand-in");
    child.wait().expect("reap stand-in");

    assert!(pids.contains(&OwnProcesses::own_pid()));
    assert!(pids.contains(&child.id()));
    // Stand-ins of concurrent tests may be listed; nothing else is.
    assert!(
        pids.iter()
            .all(|pid| *pid == OwnProcesses::own_pid() || tracked.contains(pid)),
        "{pids:?}"
    );
    assert!(!pids.contains(&1), "init is not in the table");
    assert!(
        !pids.contains(&std::os::unix::process::parent_id()),
        "the parent of the test process is not in the table"
    );
    assert!(own.expect("inspect").is_some(), "this process is running");
    assert_eq!(foreign.expect("inspect"), None, "any other PID has exited");
}

#[tokio::test]
async fn install_status_upgrade_and_uninstall_round_trip() {
    let harness = Harness::new();
    let installed = harness.install(V1).await.expect("install");
    assert!(!installed.resumed);
    assert_eq!(
        installed.namespace,
        harness.context.namespace().expect("namespace").as_str()
    );
    harness.assert_installed(V1);
    let definition = harness.fake.world().registered.clone().expect("definition");
    assert_eq!(
        definition.arguments(),
        [
            "--service-config".to_owned(),
            harness.context.config_path().display().to_string()
        ]
    );

    let config = harness.config();
    let status = harness
        .engine()
        .status(config.as_ref())
        .await
        .expect("status");
    assert!(status.installed);
    assert_eq!(status.active_version.as_deref(), Some(V1));
    assert_eq!(
        status.daemon.as_ref().map(|daemon| daemon.state),
        Some("running")
    );
    assert_eq!(status.versions.len(), 1);
    assert!(status.versions[0].active);
    let json = serde_json::to_value(&status).expect("status JSON");
    for key in [
        "installed",
        "config_path",
        "namespace",
        "prefix",
        "active_version",
        "daemon",
        "versions",
        "workers",
        "unreadable_journals",
        "pending_transaction",
    ] {
        assert!(json.get(key).is_some(), "status JSON lacks {key}: {json}");
    }

    assert!(matches!(
        harness.install(V1).await,
        Err(Error::AlreadyInstalled { .. })
    ));

    let upgraded = harness.upgrade(V2).await.expect("upgrade");
    assert_eq!(upgraded.from_version, V1);
    assert!(!upgraded.unchanged);
    assert_eq!(upgraded.removed_versions, [V1]);
    harness.assert_installed(V2);

    let again = harness.upgrade(V2).await.expect("repeat upgrade");
    assert!(again.unchanged);

    let removed = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall");
    assert!(!removed.purged);
    assert!(removed
        .removed
        .contains(&harness.layout().bin_dir().join(CLI_NAME)));
    harness.assert_clean();
    assert!(!harness.layout().bin_dir().join(CLI_NAME).exists());
    assert!(matches!(
        harness
            .engine()
            .uninstall(UninstallOptions::default())
            .await,
        Err(Error::NotInstalled { .. })
    ));
}

#[tokio::test]
async fn an_install_interrupted_after_every_step_resumes_to_one_installation() {
    for step in STEPS {
        let harness = Harness::new();
        let mut engine = harness.engine();
        engine.interrupt_after = Some(step);
        let error = engine
            .install(&harness.staged(V1), &harness.prefix(), V1)
            .await
            .expect_err("interrupted");
        assert!(
            matches!(error, Error::Interrupted(at) if at == step),
            "{error:?}"
        );
        assert!(harness.pending().is_some(), "{step:?} leaves the record");

        let report = harness.install(V1).await.expect("resume");
        assert!(report.resumed, "{step:?} resumes");
        harness.assert_installed(V1);
        assert!(
            harness.fake.world().installs <= 1,
            "{step:?}: the daemon job is installed at most once"
        );
    }
}

/// A context whose login shell reports `dir` as the whole `PATH`.
fn discovering(
    context: &Context,
    root: &std::path::Path,
    name: &str,
    dir: &std::path::Path,
) -> Context {
    crate::service::context::tests::make_dirs(root, dir);
    let shell = root.join(name);
    pohunek_test_support::fs::write_executable(
        &shell,
        format!(
            "#!/bin/sh\nPATH='{}'; export PATH\nexec /bin/sh -c \"$3\"\n",
            dir.display()
        ),
    )
    .expect("write shell");
    let discovery = crate::service::context::managed_path_discovery(Some(shell), Vec::new())
        .expect("discovery");
    let crate::service::context::PathDiscovery::Managed {
        login_shell,
        shell_defaulted,
        ..
    } = discovery
    else {
        panic!("managed discovery");
    };
    // No fallback directories, so the recorded path is what the shell printed.
    context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Managed {
            login_shell,
            shell_defaulted,
            fallback_directories: Vec::new(),
        })
}

#[tokio::test]
async fn a_resumed_install_keeps_the_path_recorded_in_service_toml() {
    let mut harness = Harness::new();
    let first = harness.root.join("first-bin");
    let second = harness.root.join("second-bin");
    harness.context = discovering(&harness.context, &harness.root, "shell-a", &first);
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registering);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");

    harness.context = discovering(&harness.context, &harness.root, "shell-b", &second);
    let report = harness.install(V1).await.expect("resume");
    assert!(report.resumed);
    let recorded = harness
        .config()
        .expect("config")
        .search_path()
        .to_env_value();
    assert_eq!(recorded, first.display().to_string());
    let registered = harness.fake.world().registered.clone().expect("registered");
    assert_eq!(registered.environment().get("PATH"), Some(&recorded));
}

/// A context whose discovery-only variables are unusable.
fn with_hostile_discovery_environment(context: &Context) -> Context {
    context.clone().with_environment_discovery(
        |name| match name {
            "SHELL" | "ZDOTDIR" | "XDG_CONFIG_HOME" => Some("relative/path".into()),
            _ => None,
        },
        true,
    )
}

#[tokio::test]
async fn unusable_discovery_variables_fail_only_a_fresh_install_before_any_effect() {
    // A fresh install needs the discovery environment, so it fails first.
    let mut harness = Harness::new();
    harness.context = with_hostile_discovery_environment(&harness.context);
    let error = harness.install(V1).await.expect_err("fresh install");
    assert_eq!(error.code(), "service_environment_invalid");
    assert!(harness.config().is_none(), "no config was written");
    assert!(harness.pending().is_none(), "no transaction started");
    assert_eq!(harness.fake.world().installs, 0, "no job was registered");

    // Every other command uses the recorded configuration and never reads it.
    let mut harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.context = with_hostile_discovery_environment(&harness.context);
    let config = harness.config();
    harness
        .engine()
        .status(config.as_ref())
        .await
        .expect("status ignores the discovery environment");
    harness.upgrade(V2).await.expect("upgrade ignores it");
    harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall ignores it");

    // A resumed install reads the recorded config instead of discovering.
    let mut harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registering);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    harness.context = with_hostile_discovery_environment(&harness.context);
    let report = harness.install(V1).await.expect("resume ignores it");
    assert!(report.resumed);
}

#[tokio::test]
async fn a_hostile_discovery_environment_fails_install_and_check_before_a_foreign_rollback() {
    let mut harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Binaries);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    let before = harness.pending().expect("a foreign pending install");
    let config_before = harness.config();

    harness.context = with_hostile_discovery_environment(&harness.context);
    // `service check` and install agree on the hostile environment.
    let check = crate::service::check_with(
        &harness.context,
        &harness.backend,
        Some(harness.prefix()),
        V2,
        false,
    )
    .await
    .expect_err("check fails");
    let error = harness.install(V2).await.expect_err("install fails");
    assert_eq!(check.code(), "service_environment_invalid");
    assert_eq!(error.code(), check.code());
    // The pending transaction was neither rolled back nor touched.
    let after = harness.pending().expect("the record survives");
    assert_eq!((after.version, after.step), (before.version, before.step));
    assert_eq!(harness.config(), config_before);
    assert_eq!(harness.fake.world().installs, 0);
}

#[tokio::test]
async fn an_existing_installation_wins_over_discovery_and_never_starts_the_probe() {
    let mut harness = Harness::new();
    harness.install(V1).await.expect("install");
    let marker = harness.root.join("probe-ran");
    let shell = harness.root.join("marker-shell");
    pohunek_test_support::fs::write_executable(
        &shell,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .expect("write shell");
    let discovery = crate::service::context::managed_path_discovery(Some(shell), Vec::new())
        .expect("discovery");
    let probing = harness.context.clone().with_path_discovery(discovery);
    let hostile = with_hostile_discovery_environment(&harness.context);
    for context in [probing, hostile] {
        harness.context = context;
        let error = harness.install(V1).await.expect_err("already installed");
        assert_eq!(error.code(), "service_already_installed");
        let checked = crate::service::check_with(
            &harness.context,
            &harness.backend,
            Some(harness.prefix()),
            V2,
            false,
        )
        .await;
        // `check` would upgrade; it never discovers for an installed service.
        assert!(checked.is_ok(), "{checked:?}");
    }
    assert!(!marker.exists(), "the login shell must never start");
}

#[tokio::test]
async fn check_and_install_agree_on_the_config_of_a_resumed_install() {
    for variant in ["missing", "prefix", "version", "valid"] {
        let harness = Harness::new();
        let mut engine = harness.engine();
        engine.interrupt_after = Some(Step::Registering);
        engine
            .install(&harness.staged(V1), &harness.prefix(), V1)
            .await
            .expect_err("interrupted");
        let path = harness.context.config_path();
        match variant {
            "missing" => std::fs::remove_file(&path).expect("remove config"),
            "prefix" | "version" => {
                let mut spec = harness.config().expect("config").to_spec();
                if variant == "prefix" {
                    spec.prefix = harness.root.join("elsewhere");
                } else {
                    spec.active_version = "9.9.9".to_owned();
                }
                ServiceConfig::new(spec)
                    .expect("valid edited config")
                    .write(&path)
                    .expect("write");
            }
            _ => {}
        }
        let checked = crate::service::check_with(
            &harness.context,
            &harness.backend,
            Some(harness.prefix()),
            V1,
            false,
        )
        .await;
        let installed = harness.install(V1).await;
        match (&checked, &installed) {
            (Ok(_), Ok(_)) => assert_eq!(variant, "valid"),
            (Err(check), Err(install)) => {
                assert_ne!(variant, "valid");
                assert_eq!(check.code(), install.code(), "{variant}");
                assert_eq!(check.to_string(), install.to_string(), "{variant}");
            }
            other => panic!("{variant}: check and install disagree: {other:?}"),
        }
    }
}

#[tokio::test]
async fn rolling_back_an_interrupted_upgrade_needs_no_discovery() {
    for hostile in [true, false] {
        let mut harness = Harness::new();
        harness.install(V1).await.expect("install");
        let mut engine = harness.engine();
        engine.interrupt_after = Some(Step::Binaries);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted upgrade");
        assert!(harness.pending().is_some(), "an upgrade record is pending");

        let marker = harness.root.join("probe-ran");
        let shell = harness.root.join("marker-shell");
        pohunek_test_support::fs::write_executable(
            &shell,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .expect("write shell");
        harness.context = if hostile {
            with_hostile_discovery_environment(&harness.context)
        } else {
            let discovery =
                crate::service::context::managed_path_discovery(Some(shell), Vec::new())
                    .expect("discovery");
            harness.context.clone().with_path_discovery(discovery)
        };
        // `check` agrees: no discovery for the rollback of an upgrade.
        let checked = crate::service::check_with(
            &harness.context,
            &harness.backend,
            Some(harness.prefix()),
            V1,
            false,
        )
        .await;
        assert!(!matches!(&checked, Err(error) if error.code() == "service_environment_invalid"));
        // The install rolls the upgrade back, then finds the installation.
        let error = harness.install(V1).await.expect_err("already installed");
        assert_eq!(error.code(), "service_already_installed");
        assert!(harness.pending().is_none(), "the rollback completed");
        assert!(!marker.exists(), "no profile code ran");
    }
}

#[tokio::test]
async fn check_and_install_agree_when_a_hostile_environment_meets_an_orphan_job() {
    let mut harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Binaries);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    let before = harness.pending().expect("a foreign pending install");
    // An orphan job of this namespace, plus an unusable discovery environment.
    let config =
        crate::service::definition::initial_config(&harness.context, &harness.prefix(), V1)
            .expect("config");
    let definition =
        crate::service::definition::daemon_definition(&harness.context, &config).expect("job");
    harness.fake.world().registered = Some(definition);
    harness.fake.world().running = true;
    harness.context = with_hostile_discovery_environment(&harness.context);

    let checked = crate::service::check_with(
        &harness.context,
        &harness.backend,
        Some(harness.prefix()),
        V2,
        false,
    )
    .await
    .expect_err("check fails");
    let installed = harness.install(V2).await.expect_err("install fails");
    // The rollback of an interrupted install discovers before the job check.
    assert_eq!(installed.code(), "service_environment_invalid");
    assert_eq!(checked.code(), installed.code());
    let after = harness.pending().expect("the transaction is untouched");
    assert_eq!((after.version, after.step), (before.version, before.step));
}

#[tokio::test]
async fn a_resume_refuses_a_service_toml_of_another_installation() {
    for (key, edit) in [("prefix", 0_u8), ("active_version", 1_u8)] {
        let harness = Harness::new();
        let mut engine = harness.engine();
        engine.interrupt_after = Some(Step::Registering);
        engine
            .install(&harness.staged(V1), &harness.prefix(), V1)
            .await
            .expect_err("interrupted");
        let mut spec = harness.config().expect("config").to_spec();
        if edit == 0 {
            spec.prefix = harness.root.join("elsewhere");
        } else {
            spec.active_version = "9.9.9".to_owned();
        }
        let edited = ServiceConfig::new(spec).expect("valid edited config");
        edited.write(&harness.context.config_path()).expect("write");
        let before = std::fs::read(harness.context.config_path()).expect("read");

        let error = harness.install(V1).await.expect_err("mismatch");
        assert!(
            matches!(&error, Error::ResumeConfigMismatch { key: found, .. } if *found == key),
            "{key}: {error:?}"
        );
        assert_eq!(error.code(), "service_resume_config_mismatch");
        // Nothing was overwritten and the record stays for an operator.
        assert_eq!(
            std::fs::read(harness.context.config_path()).expect("read"),
            before,
            "{key}"
        );
        assert!(harness.pending().is_some(), "{key}");
    }
}

#[tokio::test]
async fn an_install_interrupted_before_registration_rolls_back_without_orphans() {
    for step in [Step::Binaries, Step::Config, Step::Definition] {
        let harness = Harness::new();
        let mut engine = harness.engine();
        engine.interrupt_after = Some(step);
        engine
            .install(&harness.staged(V1), &harness.prefix(), V1)
            .await
            .expect_err("interrupted");
        assert_eq!(
            harness.pending().expect("record left behind").step,
            step,
            "{step:?}"
        );

        let report = harness
            .engine()
            .uninstall(UninstallOptions::default())
            .await
            .expect("uninstall rolls back");
        let rolled_back = report.rolled_back.expect("rollback reported");
        assert_eq!(rolled_back.step, step.as_str());
        harness.assert_clean();
    }
}

/// Leaves an install of `V1` pending at `config`, which any other command
/// would roll back, and returns its record.
async fn pending_install_at_config(harness: &Harness) -> Record {
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Config);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    harness.pending().expect("record left behind")
}

/// Asserts that the pending install `record` and everything it wrote remain.
fn assert_pending_untouched(harness: &Harness, record: &Record) {
    assert_eq!(harness.pending().as_ref(), Some(record), "record untouched");
    assert_eq!(
        harness
            .config()
            .expect("service.toml kept")
            .active_version(),
        V1
    );
    assert!(harness
        .layout()
        .daemon_executable(V1)
        .expect("daemon path")
        .is_file());
    assert!(harness.fake.world().registered.is_none());
}

#[tokio::test]
async fn an_unnormalized_prefix_is_refused_before_a_pending_install_is_rolled_back() {
    let harness = Harness::new();
    let record = pending_install_at_config(&harness).await;
    let prefix = harness.prefix().display().to_string();
    let (parent, name) = prefix.rsplit_once('/').expect("absolute prefix");
    for spelling in [
        format!("{prefix}/."),
        format!("{prefix}/"),
        format!("{parent}//{name}"),
        format!("{parent}/./{name}"),
    ] {
        let error = harness
            .engine()
            .install(&harness.staged(V2), Path::new(&spelling), V2)
            .await
            .expect_err("unnormalized prefix");
        assert!(
            matches!(&error, Error::InvalidPath { flag: "--prefix", path } if path == Path::new(&spelling)),
            "{spelling}: {error:?}"
        );
        assert_pending_untouched(&harness, &record);
    }
}

#[tokio::test]
async fn a_root_the_daemon_definition_cannot_carry_is_refused_before_any_rollback() {
    use std::os::unix::ffi::OsStrExt as _;

    let harness = Harness::new();
    let record = pending_install_at_config(&harness).await;
    let mut home = harness.root.as_os_str().to_owned();
    home.push(std::ffi::OsStr::from_bytes(b"/home-\xff"));
    let broken = Context::new(
        harness.context.paths().clone(),
        harness.context.uid(),
        Some(PathBuf::from(home)),
        Some(harness.root.join("run")),
        harness.context.supervisor_dir().to_path_buf(),
        harness.context.cli_executable().to_path_buf(),
    );
    let engine = engine_over(&broken, &harness.backend);

    let error = engine
        .install(&harness.staged(V2), &harness.prefix(), V2)
        .await
        .expect_err("non-UTF-8 HOME");
    assert!(
        matches!(&error, Error::NonUtf8Env { var: "HOME", .. }),
        "{error:?}"
    );
    assert_pending_untouched(&harness, &record);

    let error = engine
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("non-UTF-8 HOME");
    assert!(
        matches!(&error, Error::NonUtf8Env { var: "HOME", .. }),
        "{error:?}"
    );
    assert_pending_untouched(&harness, &record);

    let error = engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("non-UTF-8 HOME");
    assert!(
        matches!(&error, Error::NonUtf8Env { var: "HOME", .. }),
        "{error:?}"
    );
    assert_pending_untouched(&harness, &record);
}

/// Builds `harness`'s context with `home` as `HOME`.
fn context_with_home(harness: &Harness, home: Option<PathBuf>) -> Context {
    Context::new(
        harness.context.paths().clone(),
        harness.context.uid(),
        home,
        Some(harness.root.join("run")),
        harness.context.supervisor_dir().to_path_buf(),
        harness.context.cli_executable().to_path_buf(),
    )
}

/// `HOME` values the daemon refuses as its workers' working directory.
fn unusable_homes(harness: &Harness) -> Vec<(&'static str, Option<PathBuf>)> {
    let file = harness.root.join("home-file");
    std::fs::write(&file, "").expect("write a regular file");
    let home = harness.root.join("home").display().to_string();
    let (parent, name) = home.rsplit_once('/').expect("absolute home");
    vec![
        ("missing", None),
        ("relative", Some(PathBuf::from("home"))),
        ("regular file", Some(file)),
        ("nonexistent", Some(harness.root.join("no-such-home"))),
        (
            "unnormalized",
            Some(PathBuf::from(format!("{parent}/./{name}"))),
        ),
    ]
}

fn assert_unusable_home(case: &str, error: &Error) {
    match error {
        Error::MissingEnv { var } if case == "missing" => assert_eq!(var, "HOME"),
        Error::UnusableEnv { var, .. } if case != "missing" => {
            assert_eq!(var, "HOME", "{case}");
            assert_eq!(error.code(), "service_environment_invalid", "{case}");
        }
        other => panic!("{case}: unexpected error {other:?}"),
    }
    assert!(
        error.hint().is_some_and(|hint| hint.contains("HOME")),
        "{case}: {error:?}"
    );
}

#[tokio::test]
async fn an_unusable_home_is_refused_before_a_fresh_install_changes_anything() {
    let harness = Harness::new();
    for (case, home) in unusable_homes(&harness) {
        let context = context_with_home(&harness, home);
        let error = engine_over(&context, &harness.backend)
            .install(&harness.staged(V1), &harness.prefix(), V1)
            .await
            .expect_err("unusable HOME");
        assert_unusable_home(case, &error);
        assert!(!harness.layout().versions_dir().exists(), "{case}");
        assert_eq!(harness.fake.world().installs, 0, "{case}");
        harness.assert_clean();
    }
}

#[tokio::test]
async fn an_unusable_home_is_refused_before_any_rollback() {
    let harness = Harness::new();
    let record = pending_install_at_config(&harness).await;
    for (case, home) in unusable_homes(&harness) {
        let context = context_with_home(&harness, home);
        let engine = engine_over(&context, &harness.backend);
        let error = engine
            .install(&harness.staged(V2), &harness.prefix(), V2)
            .await
            .expect_err("unusable HOME");
        assert_unusable_home(case, &error);
        assert_pending_untouched(&harness, &record);

        let error = engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("unusable HOME");
        assert_unusable_home(case, &error);
        assert_pending_untouched(&harness, &record);
    }

    // Removing an installation needs no worker working directory.
    let context = context_with_home(&harness, None);
    let report = engine_over(&context, &harness.backend)
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall without HOME");
    assert!(report.rolled_back.is_some());
    harness.assert_clean();
}

/// Writes the durable metadata `--purge` removes, plus a worktree it keeps.
fn seed_durable_metadata(paths: &BasePaths) {
    std::fs::create_dir_all(paths.data_dir.join("events")).expect("events");
    std::fs::create_dir_all(paths.data_dir.join("worktrees/repo")).expect("worktrees");
    std::fs::write(paths.data_dir.join("metadata.jsonl"), "{}\n").expect("store");
    std::fs::create_dir_all(paths.host_state_dir()).expect("host state");
    // The daemon and the installer keep both roots owner-private.
    for root in [&paths.data_dir, &paths.state_dir] {
        std::fs::set_permissions(root, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("make root private");
    }
}

/// Backups a schema migration leaves next to the store, plus lookalikes the
/// purge must keep.
fn seed_schema_backups(paths: &BasePaths) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let store = std::ffi::OsStr::new(pohunek_paths::METADATA_STORE_NAME);
    let backups = vec![
        paths
            .data_dir
            .join(pohunek_paths::schema_backup_name(store, 1)),
        paths
            .data_dir
            .join(pohunek_paths::schema_backup_name(store, 42)),
        paths
            .data_dir
            .join(pohunek_paths::schema_backup_temp_name(store, 4242, 0)),
    ];
    assert_eq!(
        backups,
        [
            "metadata.jsonl.pre-schema-1",
            "metadata.jsonl.pre-schema-42",
            "metadata.jsonl.pre-schema-tmp.4242.0",
        ]
        .map(|name| paths.data_dir.join(name))
    );
    for backup in &backups {
        std::fs::write(backup, "{\"kind\":\"resume\"}\n").expect("schema backup");
    }
    let lookalikes = [
        "metadata.jsonl.tmp.01.2",
        "metadata.jsonl.pre-schema-",
        "metadata.jsonl.pre-schema-01",
        "metadata.jsonl.pre-schema--1",
        "metadata.jsonl.pre-schema-1.bak",
        "metadata.jsonl.pre-schema-tmp",
        "metadata.jsonl.pre-schema-tmp.1",
        "metadata.jsonl.pre-schema-tmp.1.2.3",
        "metadata.jsonl.pre-schema-tmp.x.2",
        "other.jsonl.pre-schema-1",
        "xmetadata.jsonl.pre-schema-1",
    ]
    .map(|name| paths.data_dir.join(name))
    .to_vec();
    for lookalike in &lookalikes {
        std::fs::write(lookalike, "keep").expect("unrelated file");
    }
    (backups, lookalikes)
}

fn assert_purged(paths: &BasePaths) {
    assert!(!paths.data_dir.join("metadata.jsonl").exists());
    assert!(!paths.data_dir.join("events").exists());
    assert!(!paths.worker_state_root().exists());
    assert!(!paths.host_state_dir().exists());
    assert!(paths.data_dir.join("worktrees/repo").is_dir());
}

const PURGE: UninstallOptions = UninstallOptions {
    stop_sessions: false,
    purge: true,
};

#[tokio::test]
async fn purge_follows_the_rollback_of_an_unregistered_install() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    write_journal(
        &paths,
        "s-5",
        "w-5",
        Path::new("/elsewhere/pohunek-sessiond"),
        RuntimePhase::Terminal,
        1,
    );
    let record = pending_install_at_config(&harness).await;

    let report = harness.engine().uninstall(PURGE).await.expect("uninstall");
    assert_eq!(
        report.rolled_back.map(|pending| pending.step),
        Some(record.step.as_str())
    );
    assert!(report.purged, "the report says the metadata is purged");
    assert_purged(&paths);
    harness.assert_clean();
}

#[tokio::test]
async fn purge_removes_the_schema_migration_backups_with_the_store() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    let (backups, lookalikes) = seed_schema_backups(&paths);
    let _record = pending_install_at_config(&harness).await;

    let report = harness.engine().uninstall(PURGE).await.expect("uninstall");

    assert!(report.purged);
    assert_purged(&paths);
    for backup in &backups {
        assert!(!backup.exists(), "{} survives the purge", backup.display());
        assert!(
            report.removed.contains(backup),
            "{} is reported as removed",
            backup.display()
        );
    }
    for lookalike in &lookalikes {
        assert_eq!(
            std::fs::read_to_string(lookalike).expect("unrelated file survives"),
            "keep",
            "{} is not a schema backup",
            lookalike.display()
        );
        assert!(!report.removed.contains(lookalike));
    }
}

/// Purge consumes the archived manifest, without deleting unrelated migrations.
#[tokio::test]
async fn purge_removes_imported_legacy_manifest_archives() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    let migrations = paths.data_dir.join("migrations");
    std::fs::create_dir(&migrations).expect("migrations");
    std::fs::set_permissions(
        &migrations,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private migrations");
    let archive =
        migrations.join("durable-session-workers.imported-0123456789abcdef-fedcba9876543210.json");
    std::fs::write(&archive, "synthetic legacy sessions").expect("archive");
    let short_archive = migrations.join("durable-session-workers.imported--0123456789abcdef.json");
    std::fs::write(&short_archive, "already migrated archive").expect("short archive");
    let lookalike =
        migrations.join("durable-session-workers.imported-0123456789abcdef0-fedcba9876543210.json");
    std::fs::write(&lookalike, "unrelated").expect("archive lookalike");
    let unrelated = migrations.join("durable-session-workers.json");
    std::fs::write(&unrelated, "pending manifest").expect("unrelated migration");
    let _record = pending_install_at_config(&harness).await;

    let report = harness.engine().uninstall(PURGE).await.expect("purge");
    assert!(report.purged);
    assert!(!archive.exists(), "imported session archive survives purge");
    assert!(report.removed.contains(&archive));
    assert!(!short_archive.exists());
    assert!(report.removed.contains(&short_archive));
    assert_eq!(
        std::fs::read_to_string(&lookalike).expect("lookalike"),
        "unrelated"
    );
    assert!(report.kept_purge_entries.contains(&lookalike));
    assert!(report.kept_purge_entries.contains(&unrelated));
    assert!(report
        .kept_purge_entries
        .contains(&paths.data_dir.join("worktrees")));
    assert_eq!(
        std::fs::read_to_string(&unrelated).expect("pending manifest"),
        "pending manifest"
    );
    assert_purged(&paths);
    let retry = harness.engine().uninstall(PURGE).await;
    assert!(matches!(retry, Err(Error::NotInstalled { .. })));
    assert_eq!(
        std::fs::read_to_string(&unrelated).expect("retained after retry"),
        "pending manifest"
    );
}

/// Atomic store copies from crashed writers are session metadata too.
#[tokio::test]
async fn purge_removes_crashed_store_temps() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    let temporary = paths.data_dir.join("metadata.jsonl.tmp.4242.0");
    std::fs::write(&temporary, "synthetic store copy").expect("store temp");
    let second = paths
        .data_dir
        .join("metadata.jsonl.tmp.4294967295.18446744073709551615");
    std::fs::write(&second, "store copy").expect("second temp");
    let lookalikes = [
        "metadata.jsonl.tmp.01.2",
        "metadata.jsonl.tmp.1.2.3",
        "metadata.jsonl.tmp.4294967296.0",
        "metadata.jsonl.tmp.1.18446744073709551616",
        "other.jsonl.tmp.1.2",
    ];
    for name in lookalikes {
        std::fs::write(paths.data_dir.join(name), "keep").expect("lookalike");
    }
    let _record = pending_install_at_config(&harness).await;

    let report = harness.engine().uninstall(PURGE).await.expect("purge");
    assert!(report.purged);
    assert!(!temporary.exists(), "crashed store temp survives purge");
    assert!(report.removed.contains(&temporary));
    assert!(!second.exists());
    assert!(report.removed.contains(&second));
    for name in lookalikes {
        let path = paths.data_dir.join(name);
        assert_eq!(std::fs::read_to_string(&path).expect("lookalike"), "keep");
        assert!(report.kept_purge_entries.contains(&path));
    }
    assert_purged(&paths);
}

/// A recognized temp-name symlink is unlinked without touching its target.
#[tokio::test]
async fn purge_unlinks_a_store_temp_symlink_and_preserves_its_referent() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    let referent = paths.data_dir.join("worktrees/repo/keep");
    std::fs::write(&referent, "owner work").expect("referent");
    let temporary = paths.data_dir.join("metadata.jsonl.tmp.4242.0");
    std::os::unix::fs::symlink(&referent, &temporary).expect("temp symlink");
    let _record = pending_install_at_config(&harness).await;

    let report = harness.engine().uninstall(PURGE).await.expect("purge");
    assert!(report.purged);
    assert!(
        temporary.symlink_metadata().is_err(),
        "temp symlink survives purge"
    );
    assert!(report.removed.contains(&temporary));
    assert_eq!(
        std::fs::read_to_string(&referent).expect("referent"),
        "owner work"
    );
    assert_purged(&paths);
}

/// Purge must still emit successful CLI JSON when retained Unix names are not UTF-8.
#[tokio::test]
async fn purge_reports_non_utf8_retained_names_as_escaped_json_strings() {
    use std::os::unix::ffi::OsStrExt as _;

    for in_migrations in [false, true] {
        let harness = Harness::new();
        let paths = harness.context.paths().clone();
        seed_durable_metadata(&paths);
        let parent = if in_migrations {
            let migrations = paths.data_dir.join("migrations");
            std::fs::create_dir(&migrations).expect("migrations");
            std::fs::set_permissions(
                &migrations,
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )
            .expect("private migrations");
            migrations
        } else {
            paths.data_dir.clone()
        };
        let mut names: Vec<(&[u8], &str)> = vec![
            (b"keep-\\xff", r"keep-\\xff"),
            (b"keep-\n", r"keep-\n"),
            ("keep-é".as_bytes(), r"keep-\xc3\xa9"),
        ];
        let non_utf8 = parent.join(std::ffi::OsStr::from_bytes(b"keep-\xff"));
        match std::fs::write(&non_utf8, "owner work") {
            Ok(()) => names.push((b"keep-\xff", r"keep-\xff")),
            Err(error) => {
                #[cfg(target_os = "macos")]
                {
                    // Native APFS rejects this name before a purge can ever observe it.
                    assert_eq!(error.raw_os_error(), Some(nix::libc::EILSEQ));
                    non_utf8
                        .symlink_metadata()
                        .expect_err("a rejected filename has no directory entry");
                    eprintln!("filesystem explicitly rejected the non-UTF-8 fixture with EILSEQ; supported retained names still exercise purge and JSON");
                }
                #[cfg(not(target_os = "macos"))]
                panic!("non-UTF-8 fixture must be supported on this filesystem: {error}");
            }
        }
        for (name, _) in &names {
            std::fs::write(parent.join(std::ffi::OsStr::from_bytes(name)), "owner work")
                .expect("retained file");
        }
        let _record = pending_install_at_config(&harness).await;

        let report = harness.engine().uninstall(PURGE).await.expect("purge");
        assert!(report.purged);
        assert_purged(&paths);
        harness.assert_clean();
        let json = crate::commands::render_json(&report).expect("successful purge must serialize");
        let document: serde_json::Value = serde_json::from_str(&json).expect("CLI JSON document");
        let entries = document["ok"]["kept_purge_entries"]
            .as_array()
            .expect("retained paths");
        for (name, escaped) in &names {
            let path = parent.join(std::ffi::OsStr::from_bytes(name));
            assert_eq!(
                std::fs::read_to_string(path).expect("retained file"),
                "owner work"
            );
            let expected = format!("{}/{escaped}", parent.display());
            assert!(entries.contains(&serde_json::Value::String(expected)));
        }
    }
}

#[tokio::test]
async fn purge_never_follows_a_symlinked_migrations_directory() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    let owner = paths.data_dir.join("worktrees/repo");
    let archive =
        owner.join("durable-session-workers.imported-0123456789abcdef-fedcba9876543210.json");
    std::fs::write(&archive, "owner work").expect("referent");
    std::os::unix::fs::symlink(&owner, paths.data_dir.join("migrations")).expect("migrations link");
    let _record = pending_install_at_config(&harness).await;
    harness
        .engine()
        .uninstall(PURGE)
        .await
        .expect_err("symlinked parent is refused");
    assert_eq!(
        std::fs::read_to_string(&archive).expect("owner archive"),
        "owner work"
    );
}

#[tokio::test]
async fn purge_refuses_a_directory_at_a_recognized_store_temp_name() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    let directory = paths.data_dir.join("metadata.jsonl.tmp.4242.0");
    std::fs::create_dir(&directory).expect("wrong type");
    std::fs::write(directory.join("keep"), "owner work").expect("owner file");
    let _record = pending_install_at_config(&harness).await;
    harness
        .engine()
        .uninstall(PURGE)
        .await
        .expect_err("directory is not a temp file");
    assert_eq!(
        std::fs::read_to_string(directory.join("keep")).expect("owner file"),
        "owner work"
    );
}

#[tokio::test]
async fn purge_refuses_a_symlinked_schema_backup_without_following_it() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    let referent = paths.data_dir.join("worktrees/repo/keep");
    std::fs::write(&referent, "owner work").expect("referent");
    let link = paths.data_dir.join("metadata.jsonl.pre-schema-1");
    std::os::unix::fs::symlink(&referent, &link).expect("symlink");
    let _record = pending_install_at_config(&harness).await;

    harness
        .engine()
        .uninstall(PURGE)
        .await
        .expect_err("a symlinked backup fails the purge closed");

    assert_eq!(
        std::fs::read_to_string(&referent).expect("referent"),
        "owner work"
    );
}

#[tokio::test]
async fn purge_after_a_rollback_keeps_the_record_until_nothing_blocks_it() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    let worker_exe = PathBuf::from("/elsewhere/pohunek-sessiond");
    write_live_journal(&paths, "s-6", "w-6", &worker_exe);
    pending_install_at_config(&harness).await;

    let error = harness
        .engine()
        .uninstall(PURGE)
        .await
        .expect_err("a live worker blocks the purge");
    assert!(matches!(&error, Error::OrphanWorkers { .. }), "{error:?}");
    assert_eq!(harness.config(), None, "the rollback removed service.toml");
    assert!(
        harness.pending().expect("record kept").rolling_back,
        "a rerun still finds the installation"
    );
    assert!(paths.data_dir.join("metadata.jsonl").is_file());

    write_journal(&paths, "s-6", "w-6", &worker_exe, RuntimePhase::Terminal, 1);
    let report = harness.engine().uninstall(PURGE).await.expect("rerun");
    assert!(report.rolled_back.is_some());
    assert!(report.purged);
    assert_purged(&paths);
    harness.assert_clean();
}

#[tokio::test]
async fn purge_after_a_rollback_refuses_a_registered_daemon_job() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    pending_install_at_config(&harness).await;
    assert!(harness.fake.world().registered.is_none());
    // A daemon job registered outside this transaction.
    let definition = daemon_definition(
        &harness.context,
        &initial_config(&harness.context, &harness.prefix(), V1).expect("config"),
    )
    .expect("definition");
    {
        let mut world = harness.fake.world();
        world.registered = Some(definition);
        world.running = true;
    };

    let error = harness
        .engine()
        .uninstall(PURGE)
        .await
        .expect_err("a daemon job may serve the store");
    assert!(
        matches!(&error, Error::DaemonJobPresent { .. }),
        "{error:?}"
    );
    assert!(paths.data_dir.join("metadata.jsonl").is_file());
    assert!(
        harness.pending().is_some(),
        "a rerun still finds the installation"
    );
}

#[tokio::test]
async fn purge_without_an_installation_purges_nothing() {
    let harness = Harness::new();
    let paths = harness.context.paths().clone();
    seed_durable_metadata(&paths);
    let error = harness
        .engine()
        .uninstall(PURGE)
        .await
        .expect_err("nothing installed");
    assert!(matches!(&error, Error::NotInstalled { .. }), "{error:?}");
    assert!(paths.data_dir.join("metadata.jsonl").is_file());
    assert!(paths.data_dir.join("events").is_dir());
    assert!(paths.host_state_dir().is_dir());
}

#[tokio::test]
async fn an_install_interrupted_at_or_after_registration_is_removed_by_the_safe_uninstall() {
    for step in [Step::Registering, Step::Registered, Step::Ready, Step::Cli] {
        let harness = Harness::new();
        let mut engine = harness.engine();
        engine.interrupt_after = Some(step);
        engine
            .install(&harness.staged(V1), &harness.prefix(), V1)
            .await
            .expect_err("interrupted");
        assert_eq!(
            harness.pending().expect("record left behind").step,
            step,
            "{step:?}"
        );

        // No session is live, so the safe uninstall removes the installation
        // the record describes and consumes the record with it instead of
        // rolling the transaction back.
        let report = harness
            .engine()
            .uninstall(UninstallOptions::default())
            .await
            .expect("uninstall");
        assert_eq!(report.rolled_back, None, "{step:?}");
        harness.assert_clean();
    }
}

#[tokio::test]
async fn an_upgrade_refuses_a_pending_install_and_install_finishes_it() {
    // From `config` on, service.toml exists, so an installer choosing by that
    // file alone would call `upgrade`; from `registered` on, a daemon runs.
    for step in [Step::Config, Step::Registered, Step::Ready] {
        let harness = Harness::new();
        let mut engine = harness.engine();
        engine.interrupt_after = Some(step);
        engine
            .install(&harness.staged(V1), &harness.prefix(), V1)
            .await
            .expect_err("interrupted");
        let record = harness.pending().expect("record left behind");
        let config = std::fs::read(harness.context.config_path()).expect("service.toml");
        let registered = harness.fake.world().registered.clone();
        let running = harness.fake.world().running;

        let error = harness.upgrade(V2).await.expect_err("upgrade refuses");
        assert!(
            matches!(
                &error,
                Error::PendingInstall { version, step: at } if version == V1 && *at == step.as_str()
            ),
            "{step:?}: {error:?}"
        );
        assert_eq!(error.code(), "service_install_pending");
        assert!(error.hint().is_some());
        assert_eq!(harness.pending(), Some(record), "{step:?}: record kept");
        assert_eq!(
            std::fs::read(harness.context.config_path()).expect("service.toml kept"),
            config,
            "{step:?}: service.toml untouched"
        );
        assert_eq!(
            harness.fake.world().registered,
            registered,
            "{step:?}: daemon job untouched"
        );
        assert_eq!(harness.fake.world().running, running);
        assert_eq!(
            layout::installed_versions(&harness.layout()).expect("versions"),
            [V1],
            "{step:?}: nothing staged for the refused upgrade"
        );

        let report = harness.install(V1).await.expect("install resumes");
        assert!(report.resumed, "{step:?} resumes");
        assert_eq!(report.rolled_back, None);
        harness.assert_installed(V1);
        assert!(
            harness.fake.world().installs <= 1,
            "{step:?}: the daemon job is installed at most once"
        );
    }
}

/// Value that stands in for a secret a hand-edited `service.toml` holds under
/// a key this version does not know.
const UNKNOWN_KEY_SECRET: &str = "PH-UNKNOWN-sentinel-8d41-c3e7";

/// Adds one unknown key to `[limits]` and one unknown section to the
/// installed `service.toml`, as a newer release or an operator would.
fn add_unknown_keys(harness: &Harness) {
    let path = harness.context.config_path();
    let text = std::fs::read_to_string(&path).expect("read service.toml");
    let text = text.replacen(
        "[limits]\n",
        &format!("[limits]\nfuture_limit = \"{UNKNOWN_KEY_SECRET}\"\n"),
        1,
    );
    assert!(
        text.contains("future_limit"),
        "the fixture has a [limits] table"
    );
    // Writing in place keeps the owner-private mode of the existing file.
    std::fs::write(
        &path,
        format!("{text}\n[future]\nname = \"{UNKNOWN_KEY_SECRET}\"\n"),
    )
    .expect("write service.toml");
}

fn assert_names_unknown_keys(warnings: &[String], context: &str) {
    let joined = warnings.join("\n");
    assert!(
        joined.contains("limits.future_limit"),
        "{context}: {joined}"
    );
    assert!(joined.contains("future.name"), "{context}: {joined}");
    assert!(!joined.contains(UNKNOWN_KEY_SECRET), "{context}: {joined}");
}

#[tokio::test]
async fn service_commands_report_unknown_keys() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");

    let clean = harness.config().expect("config");
    let status = harness
        .engine()
        .status(Some(&clean))
        .await
        .expect("clean status");
    assert!(status.warnings.is_empty(), "{:?}", status.warnings);
    let clean_check = crate::service::check_with(
        &harness.context,
        &harness.backend,
        Some(harness.prefix()),
        V2,
        false,
    )
    .await
    .expect("clean check");
    assert!(
        clean_check.warnings.is_empty(),
        "{:?}",
        clean_check.warnings
    );

    add_unknown_keys(&harness);
    let config = harness.config().expect("config with unknown keys");
    let status = harness
        .engine()
        .status(Some(&config))
        .await
        .expect("status");
    assert_names_unknown_keys(&status.warnings, "status");
    let json = serde_json::to_string(&status).expect("status JSON");
    assert!(
        json.contains("future_limit") && !json.contains(UNKNOWN_KEY_SECRET),
        "{json}"
    );

    let checked = crate::service::check_with(
        &harness.context,
        &harness.backend,
        Some(harness.prefix()),
        V2,
        false,
    )
    .await
    .expect("check");
    assert_names_unknown_keys(&checked.warnings, "check");
    let json = serde_json::to_string(&checked).expect("check JSON");
    assert!(!json.contains(UNKNOWN_KEY_SECRET), "{json}");

    let unchanged = harness.upgrade(V1).await.expect("same-version upgrade");
    assert!(unchanged.unchanged);
    assert_names_unknown_keys(&unchanged.warnings, "same-version upgrade");
}

#[tokio::test]
async fn an_upgrade_keeps_unknown_keys_it_does_not_own() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    add_unknown_keys(&harness);

    let report = harness.upgrade(V2).await.expect("upgrade");

    assert_names_unknown_keys(&report.warnings, "upgrade");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("kept")),
        "{:?}",
        report.warnings
    );
    let json = serde_json::to_string(&report).expect("upgrade JSON");
    assert!(!json.contains(UNKNOWN_KEY_SECRET), "{json}");
    harness.assert_installed(V2);
    let text = std::fs::read_to_string(harness.context.config_path()).expect("read service.toml");
    assert!(
        text.contains(&format!("future_limit = \"{UNKNOWN_KEY_SECRET}\"")),
        "{text}"
    );
    assert!(text.contains("[future]"), "{text}");
    let reloaded = harness.config().expect("the new daemon's file loads");
    assert_eq!(reloaded.active_version(), V2);
    assert_eq!(reloaded.unknown_keys().len(), 3);
}

#[tokio::test]
async fn a_rollback_restores_the_source_config_with_its_unknown_keys() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    add_unknown_keys(&harness);
    let source = std::fs::read_to_string(harness.context.config_path()).expect("read");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Config);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after the config write");
    let written = harness.config().expect("config after the interrupted step");
    assert_eq!(written.active_version(), V2);

    // Asking for the active version rolls the interrupted upgrade back first.
    let report = harness.upgrade(V1).await.expect("roll back");

    assert!(report.unchanged);
    harness.assert_installed(V1);
    let restored = std::fs::read_to_string(harness.context.config_path()).expect("read");
    let table = |text: &str| -> toml::Table { toml::from_str(text).expect("TOML") };
    assert_eq!(table(&restored), table(&source), "{restored}");
    assert_eq!(harness.config().expect("loads").unknown_keys().len(), 3);
}

/// Appends a compact array to `service.toml` so the file stays within the load
/// bound while its rendering, which spreads the array out, does not.
fn grow_rendering_past_the_bound(harness: &Harness) {
    let path = harness.context.config_path();
    let text = std::fs::read_to_string(&path).expect("read service.toml");
    let elements = (pohunek_service_config::MAX_CONFIG_BYTES - text.len() - 64) / 2;
    let grown = format!(
        "{text}\n[future]\nbig = [{}]\n",
        vec!["0"; elements].join(",")
    );
    assert!(grown.len() <= pohunek_service_config::MAX_CONFIG_BYTES);
    std::fs::write(&path, grown).expect("write service.toml");
    assert!(
        harness.config().expect("still loads").to_toml().len()
            > pohunek_service_config::MAX_CONFIG_BYTES,
        "the fixture must outgrow the bound"
    );
}

#[tokio::test]
async fn an_upgrade_whose_rendering_outgrows_the_bound_is_refused_before_any_effect() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    grow_rendering_past_the_bound(&harness);
    let before = std::fs::read(harness.context.config_path()).expect("read");
    harness.runtimes.script().calls.clear();
    let uninstalls = harness.fake.world().uninstalls;

    // An interruption at the first checkpoint would end the upgrade there if
    // the refusal did not come first.
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Binaries);
    let error = engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("a rendering above the bound is refused");

    assert!(
        matches!(
            error,
            Error::Config(pohunek_service_config::ConfigError::TooLarge { .. })
        ),
        "{error:?}"
    );
    // `service check` judges the same rendering and reports the same refusal.
    let checked = check_local_report(&harness.context, None, V2, false)
        .expect_err("the check refuses what the upgrade refuses");
    assert!(
        matches!(
            checked,
            Error::Config(pohunek_service_config::ConfigError::TooLarge { .. })
        ),
        "{checked:?}"
    );
    assert!(harness.pending().is_none(), "no transaction was started");
    assert!(
        harness.runtimes.calls().is_empty(),
        "the refusal comes before any effect: {:?}",
        harness.runtimes.calls()
    );
    assert_eq!(harness.fake.world().uninstalls, uninstalls);
    assert_eq!(
        std::fs::read(harness.context.config_path()).expect("read"),
        before
    );
    assert_eq!(
        layout::installed_versions(&harness.layout()).expect("versions"),
        [V1],
        "nothing was published"
    );
}

#[tokio::test]
async fn a_rollback_after_the_daemon_stop_does_not_rewrite_an_untouched_config() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::StopDaemon);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after the daemon stop");
    // The operator grows the file while the upgrade is pending; its
    // rendering now exceeds the bound, so a re-render cannot be written.
    grow_rendering_past_the_bound(&harness);
    let before = std::fs::read(harness.context.config_path()).expect("read");

    // The check of the same request passes: it rewrites nothing.
    check_local_report(&harness.context, None, V1, false)
        .expect("the check of a rollback that leaves the file alone passes");
    let report = harness.upgrade(V1).await.expect("the rollback succeeds");

    assert!(report.unchanged);
    assert_eq!(
        report.rolled_back.map(|pending| pending.step),
        Some(Step::StopDaemon.as_str())
    );
    assert!(
        harness.pending().is_none(),
        "the journal is not left rolling back"
    );
    harness.assert_installed(V1);
    assert_eq!(
        std::fs::read(harness.context.config_path()).expect("read"),
        before,
        "the file is restored by not touching it"
    );
}

#[tokio::test]
async fn a_check_refuses_a_rollback_that_has_to_rewrite_an_oversized_config() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Config);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after the config write");
    grow_rendering_past_the_bound(&harness);

    let checked = check_local_report(&harness.context, None, V1, false)
        .expect_err("the rollback must rewrite the file, which no longer fits");
    assert!(
        matches!(
            checked,
            Error::Config(pohunek_service_config::ConfigError::TooLarge { .. })
        ),
        "{checked:?}"
    );
    // The command refuses the same way.
    let error = harness
        .upgrade(V1)
        .await
        .expect_err("the rollback is refused");
    assert!(
        matches!(
            error,
            Error::Config(pohunek_service_config::ConfigError::TooLarge { .. })
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_upgrade_interrupted_after_every_step_resumes_or_rolls_back() {
    for step in STEPS {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let mut engine = harness.engine();
        engine.interrupt_after = Some(step);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted");
        let report = harness.upgrade(V2).await.expect("resume");
        assert!(report.resumed, "{step:?} resumes");
        harness.assert_installed(V2);

        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let mut engine = harness.engine();
        engine.interrupt_after = Some(step);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted");
        // Asking for the active version rolls the other upgrade back first.
        let report = harness.upgrade(V1).await.expect("roll back");
        assert!(report.unchanged);
        assert_eq!(
            report.rolled_back.map(|pending| pending.step),
            Some(step.as_str())
        );
        harness.assert_installed(V1);
        assert_eq!(
            layout::installed_versions(&harness.layout()).expect("versions"),
            [V1],
            "{step:?}: the half-installed version is removed"
        );
    }
}

/// The registry record an apply leaves behind: schema 1 with one committed
/// change and no package, the fields `RegistryState` deserializes.
const APPLIED_REGISTRY: &str = r#"{"schema":1,"generation":1,"packages":[],"selected":{}}"#;

/// Creates the directories below `dir` that do not exist yet with mode `0700`,
/// as the daemon creates its state directories.
fn private_dirs(dir: &Path) {
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .recursive(true)
        .create(dir)
        .expect("private state dir");
}

/// Writes a state file owner-private, in a directory the daemon would create.
fn write_private(path: &Path, bytes: &str) {
    private_dirs(path.parent().expect("parent"));
    std::fs::write(path, bytes).expect("state file");
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("mode");
}

/// The state of an installed host with a package store and one profile, as
/// the daemon leaves it: the plugin root opened by the package store (which
/// creates its `packages` directory privately) and no registry record yet,
/// which the store reads as its empty state.
fn seed_runtime_state(harness: &Harness) {
    let plugins = harness.context.paths().plugins_dir();
    package::registry::Registry::open_at(&plugins, package::Limits::DEFAULT)
        .expect("the daemon opens its package store");
    write_private(
        &harness
            .context
            .paths()
            .config_dir
            .join("agents")
            .join("work.toml"),
        "profile before",
    );
}

/// The registry record and the profile text; `None` for a registry the
/// store has not written.
fn runtime_state(harness: &Harness) -> (Option<String>, String) {
    let registry =
        std::fs::read_to_string(harness.context.paths().plugins_dir().join("registry.json")).ok();
    let profile = std::fs::read_to_string(
        harness
            .context
            .paths()
            .config_dir
            .join("agents")
            .join("work.toml"),
    )
    .expect("profile");
    (registry, profile)
}

#[tokio::test]
async fn an_upgrade_whose_runtime_plan_is_refused_stops_nothing_and_changes_nothing() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    // The install runs the runtime step too; the calls under test start here.
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    harness.runtimes.plan(runtime_report(
        true,
        vec![package(PackageStatus::Install)],
        Vec::new(),
        vec![Refusal {
            subject: "claude".to_owned(),
            code: CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM.to_owned(),
            detail: "the release lists no package for this platform".to_owned(),
        }],
    ));

    let error = harness.upgrade(V2).await.expect_err("the plan is refused");
    let Error::UpgradeRuntimesRefused { refusals } = &error else {
        panic!("a refused plan is typed: {error:?}");
    };
    assert_eq!(refusals[0].code, CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM);
    assert_eq!(error.code(), "service_upgrade_runtimes_refused");
    assert_eq!(harness.runtimes.calls(), ["plan"], "no apply ran");
    assert!(harness.fake.world().running, "the old daemon still runs");
    assert_eq!(runtime_state(&harness), (None, "profile before".to_owned()));
    assert!(!harness
        .context
        .paths()
        .state_dir
        .join(record::RUNTIME_BACKUP_DIR_NAME)
        .exists());
    harness.assert_installed(V1);
}

/// The directory the fake user manager reports as its `PATH`.
const USER_MANAGER_BIN: &str = "manager-bin";

/// Stands in for the systemd user manager's `Environment` property.
#[derive(Debug, Clone)]
struct FakeUserManager(Result<Vec<String>, String>);

impl UserManager for FakeUserManager {
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, EnvironmentFailure>> + Send + '_>> {
        let value = self.0.clone().map_err(EnvironmentFailure::Unreachable);
        Box::pin(async move { value })
    }

    fn unit_environment<'a>(
        &'a self,
        _unit: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UnitEnvironment, EnvironmentFailure>> + Send + 'a>>
    {
        Box::pin(async {
            Ok(UnitEnvironment {
                load_state: "loaded".to_owned(),
                ..UnitEnvironment::default()
            })
        })
    }
}

/// A launchd user-domain configuration that sets `PATH` to `value`, as
/// `launchctl config user path` writes it.
#[cfg(target_os = "macos")]
fn launchd_domain_xml(value: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>PathEnvironmentVariable</key><string>{value}</string></dict></plist>\n"
    )
}

/// Stands in for launchd's active state of the daemon job. The state can change
/// while a test runs, as a job that is unloaded mid-upgrade does.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct FakeLaunchd {
    state: Mutex<Result<ActiveJob, String>>,
    /// How many times the job's state was asked for.
    job_reads: AtomicUsize,
    /// How many times the domain alone was asked for.
    domain_reads: AtomicUsize,
    /// Whether launchd has no user domain, as without a GUI session.
    no_domain: bool,
    /// The boot identity the host reports.
    boot: Mutex<Result<String, String>>,
    /// When the host reports it booted.
    boot_time: Mutex<Result<SystemTime, String>>,
}

#[cfg(target_os = "macos")]
impl FakeLaunchd {
    fn new(value: Result<ActiveJob, String>) -> Self {
        Self {
            state: Mutex::new(value),
            job_reads: AtomicUsize::new(0),
            domain_reads: AtomicUsize::new(0),
            no_domain: false,
            boot: Mutex::new(Ok(FIRST_BOOT.to_owned())),
            boot_time: Mutex::new(Ok(boot_time())),
        }
    }

    /// Changes the boot time the host reports.
    fn set_boot_time(&self, value: Result<SystemTime, String>) {
        *self.boot_time.lock().expect("boot time") = value;
    }

    /// Changes the boot identity the host reports, as a reboot does.
    fn set_boot(&self, value: Result<String, String>) {
        *self.boot.lock().expect("boot identity") = value;
    }

    /// The fake of a launchd with no user domain: no GUI session.
    fn without_domain() -> Self {
        Self {
            no_domain: true,
            ..Self::new(Ok(ActiveJob::default()))
        }
    }

    fn set(&self, value: Result<ActiveJob, String>) {
        *self.state.lock().expect("launchd state") = value;
    }
}

#[cfg(target_os = "macos")]
impl LaunchdJob for FakeLaunchd {
    fn active_path<'a>(
        &'a self,
        _uid: u32,
        _label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ActiveJob, String>> + Send + 'a>> {
        self.job_reads.fetch_add(1, Ordering::SeqCst);
        let value = self.state.lock().expect("launchd state").clone();
        Box::pin(async move { value })
    }

    fn domain_environment(&self, _uid: u32) -> crate::service::manager_env::DomainEnvironment<'_> {
        self.domain_reads.fetch_add(1, Ordering::SeqCst);
        let value = if self.no_domain {
            Ok(None)
        } else {
            self.state
                .lock()
                .expect("launchd state")
                .clone()
                .map(|job| job.domain_environment)
        };
        Box::pin(async move { value })
    }

    fn boot_identity(&self) -> Result<String, String> {
        self.boot.lock().expect("boot identity").clone()
    }

    fn boot_time(&self) -> Result<SystemTime, String> {
        self.boot_time.lock().expect("boot time").clone()
    }
}

/// A launchd that reports `loaded` while the fake service manager holds the
/// daemon job and `unloaded` once the job is removed, as a real stop does.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct FlipLaunchd {
    fake: Fake,
    loaded: ActiveJob,
    unloaded: ActiveJob,
    boot: String,
}

#[cfg(target_os = "macos")]
impl FlipLaunchd {
    fn state(&self) -> ActiveJob {
        if self.fake.world().registered.is_some() {
            self.loaded.clone()
        } else {
            self.unloaded.clone()
        }
    }
}

#[cfg(target_os = "macos")]
impl LaunchdJob for FlipLaunchd {
    fn active_path<'a>(
        &'a self,
        _uid: u32,
        _label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ActiveJob, String>> + Send + 'a>> {
        let value = self.state();
        Box::pin(async move { Ok(value) })
    }

    fn domain_environment(&self, _uid: u32) -> crate::service::manager_env::DomainEnvironment<'_> {
        let value = self.state().domain_environment;
        Box::pin(async move { Ok(value) })
    }

    fn boot_identity(&self) -> Result<String, String> {
        Ok(self.boot.clone())
    }

    fn boot_time(&self) -> Result<SystemTime, String> {
        Ok(boot_time())
    }
}

/// The boot identity a fake launchd host starts with.
#[cfg(target_os = "macos")]
const FIRST_BOOT: &str = "6D0CBE3B-5C31-4E4B-A1B0-0F2B6A4D5A11";
/// When the fake launchd host booted: far enough ahead that a file written by a
/// test predates the boot unless the test dates it after.
#[cfg(target_os = "macos")]
fn boot_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000)
}
/// The boot identity after a reboot.
#[cfg(target_os = "macos")]
const SECOND_BOOT: &str = "A94F0E57-77C2-4B61-9D3F-2C6E8B1A7F42";

/// On macOS, a recorded-`PATH` job without a launchd user domain (no GUI session,
/// for example an upgrade over SSH) is degraded like a bus that cannot be
/// reached: the warning says the store variables were not applied.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_recorded_path_job_without_a_launchd_domain_warns_like_an_absent_bus() {
    let mut harness = Harness::new();
    let fake = Arc::new(FakeLaunchd::without_domain());
    harness.context = harness
        .context
        .clone()
        .with_launchd_job(Arc::clone(&fake) as Arc<dyn LaunchdJob>);
    let config = crate::service::definition::config_with_search_path(
        &harness.context,
        Path::new("/opt/pohunek"),
        crate::service::VERSION,
        pohunek_platform::shell_env::SearchPath::new(vec![std::path::PathBuf::from("/usr/bin")])
            .expect("a trusted directory"),
    )
    .expect("the recorded configuration");

    let job = crate::service::engine::job_path(&harness.context, &config, None)
        .await
        .expect("a recorded PATH judges without a domain");

    assert!(job.variables.is_empty(), "no domain, no variables");
    assert!(job.manager_unreachable.is_some(), "{job:?}");
    let warnings = crate::service::engine::job_warnings(&harness.context, &job);
    assert!(
        warnings.iter().any(
            |warning| warning.contains("judged at the default store locations")
                && warning.contains("GUI session")
        ),
        "the warning names the macOS remedy: {warnings:?}"
    );
}

/// On macOS a recorded-`PATH` job reads only the launchd domain: asking for the
/// job would derive the namespace, which creates the state and runtime roots.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_recorded_path_job_on_macos_reads_the_domain_and_never_the_job() {
    let mut harness = Harness::new();
    let (allowed_name, allowed_value) = ALLOWLISTED_MANAGER_VARIABLE;
    let fake = Arc::new(FakeLaunchd::new(Ok(ActiveJob {
        domain: None,
        loaded_default: None,
        domain_environment: Some(vec![(allowed_name.to_owned(), allowed_value.to_owned())]),
    })));
    harness.context = harness
        .context
        .clone()
        .with_launchd_job(Arc::clone(&fake) as Arc<dyn LaunchdJob>);
    let config = crate::service::definition::config_with_search_path(
        &harness.context,
        Path::new("/opt/pohunek"),
        crate::service::VERSION,
        pohunek_platform::shell_env::SearchPath::new(vec![std::path::PathBuf::from("/usr/bin")])
            .expect("a trusted directory"),
    )
    .expect("the recorded configuration");

    let job = crate::service::engine::job_path(&harness.context, &config, None)
        .await
        .expect("the recorded PATH judges");

    assert_eq!(
        fake.job_reads.load(Ordering::SeqCst),
        0,
        "the job is never asked for"
    );
    assert_eq!(fake.domain_reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        job.variables.get(allowed_name).map(String::as_str),
        Some(allowed_value)
    );
}

/// A loaded job whose `default environment` sets `PATH` to `default`.
#[cfg(target_os = "macos")]
fn loaded_job(default: impl Into<String>) -> ActiveJob {
    ActiveJob {
        domain: None,
        loaded_default: Some(default.into()),
        domain_environment: Some(Vec::new()),
    }
}

/// A job that is not loaded, in a domain that sets no `PATH`.
#[cfg(target_os = "macos")]
fn not_loaded() -> ActiveJob {
    ActiveJob {
        domain_environment: Some(Vec::new()),
        ..ActiveJob::default()
    }
}

/// A domain that sets `domain`, with the job loaded at `default` or not.
#[cfg(target_os = "macos")]
fn domain_over(domain: &str, default: Option<&str>) -> ActiveJob {
    ActiveJob {
        domain: Some(domain.to_owned()),
        loaded_default: default.map(str::to_owned),
        domain_environment: Some(Vec::new()),
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_loaded_job_without_a_recorded_path_judges_with_the_path_launchd_reports() {
    const DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
    let mut harness = Harness::new();
    harness.context = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged)
        .with_launchd_job(Arc::new(FakeLaunchd::new(Ok(loaded_job(
            DEFAULT_PATH.to_owned(),
        )))));

    harness.install(V1).await.expect("install");

    assert!(harness.config().expect("config").search_path().is_empty());
    let judged = harness.runtimes.script().search_paths.clone();
    assert!(!judged.is_empty(), "the runtime step ran");
    assert!(
        judged.iter().all(|path| path == DEFAULT_PATH),
        "every spawn gets the PATH launchd reports for the job: {judged:?}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_job_that_is_not_loaded_falls_back_to_the_persisted_domain_configuration() {
    const DOMAIN_PATH: &str = "/opt/pohunek-test/bin:/usr/bin:/bin";
    let mut harness = Harness::new();
    let domain = harness.root.join("user.plist");
    std::fs::write(&domain, launchd_domain_xml(DOMAIN_PATH)).expect("domain configuration");
    harness.context = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged)
        .with_launchd_domain_config(&domain)
        .with_launchd_job(Arc::new(FakeLaunchd::new(Ok(not_loaded()))));

    harness.install(V1).await.expect("install");

    let judged = harness.runtimes.script().search_paths.clone();
    assert!(!judged.is_empty(), "the runtime step ran");
    assert!(
        judged.iter().all(|path| path == DOMAIN_PATH),
        "a job that is down judges with the best evidence available: {judged:?}"
    );
}

/// On macOS a job without a recorded `PATH` and without a launchd user domain
/// (no GUI session, for example a check over SSH) takes its `PATH` from the
/// persisted configuration and no variables. It is degraded like a recorded-`PATH`
/// job without a domain: the warning names the remedy, and a gate that judged a
/// stopped session refuses.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_job_without_a_recorded_path_or_a_launchd_domain_is_judged_without_its_variables() {
    const DOMAIN_PATH: &str = "/opt/pohunek-test/bin:/usr/bin:/bin";
    let mut harness = Harness::new();
    let domain = harness.root.join("user.plist");
    std::fs::write(&domain, launchd_domain_xml(DOMAIN_PATH)).expect("domain configuration");
    harness.context = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged)
        .with_launchd_domain_config(&domain)
        .with_launchd_job(Arc::new(FakeLaunchd::without_domain()));
    harness.install(V1).await.expect("install");
    let config = harness.config().expect("config");
    assert!(config.search_path().is_empty());

    let job = crate::service::engine::job_path(&harness.context, &config, None)
        .await
        .expect("the persisted configuration gives the PATH");
    assert_eq!(job.value, DOMAIN_PATH);
    assert!(job.variables.is_empty(), "no domain, no variables");
    assert!(job.manager_unreachable.is_some(), "{job:?}");
    let warnings = crate::service::engine::job_warnings(&harness.context, &job);
    assert!(
        warnings.iter().any(
            |warning| warning.contains("judged at the default store locations")
                && warning.contains("GUI session")
        ),
        "{warnings:?}"
    );

    let refused = harness
        .engine()
        .with_adoption_preflight(preflight_with_a_stopped_session())
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("a stopped session judged without the domain's variables refuses");
    assert!(
        matches!(refused, Error::ManagerVariablesUnread { .. }),
        "{refused:?}"
    );
    harness.assert_installed(V1);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_unrecognized_launchd_report_refuses_a_job_without_a_recorded_path() {
    let mut harness = Harness::new();
    harness.context = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged)
        .with_launchd_job(Arc::new(FakeLaunchd::new(Err(
            "unrecognized launchctl print output: `launchctl changed its output`".to_owned(),
        ))));

    let error = harness
        .install(V1)
        .await
        .expect_err("the job's PATH cannot be read");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    assert!(harness.pending().is_none(), "no transaction was journaled");
    assert!(harness.config().is_none(), "no configuration was kept");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_upgrade_judges_and_runs_with_the_user_managers_path_when_the_job_records_none() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let manager_bin = harness.root.join(USER_MANAGER_BIN).display().to_string();
    assert!(harness.config().expect("config").search_path().is_empty());
    harness.runtimes.script().search_paths.clear();
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade");

    let judged = judged.lock().expect("search path lock").clone();
    assert_eq!(judged, [manager_bin.clone(), manager_bin.clone()]);
    let runtime_paths = harness.runtimes.script().search_paths.clone();
    assert!(!runtime_paths.is_empty(), "the runtime step ran");
    assert!(
        runtime_paths.iter().all(|path| *path == manager_bin),
        "{runtime_paths:?}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_unreadable_user_manager_path_fails_the_upgrade_before_any_effect() {
    let mut harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    let unreadable = FakeUserManager(Err("Failed to connect to bus".to_owned()));
    harness.context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(unreadable));

    let error = harness
        .upgrade(V2)
        .await
        .expect_err("the daemon's PATH cannot be read");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    assert!(
        error.to_string().contains("Failed to connect to bus"),
        "{error}"
    );
    assert!(harness.pending().is_none(), "no transaction was journaled");
    assert_eq!(
        harness.fake.world().replaces,
        0,
        "the daemon was not replaced"
    );
    assert!(harness.runtimes.calls().is_empty(), "no runtime step ran");
    harness.assert_installed(V1);

    let no_path = FakeUserManager(Ok(vec!["HOME=/home/test".to_owned()]));
    harness.context = harness.context.clone().with_user_manager(Arc::new(no_path));
    let error = harness
        .upgrade(V2)
        .await
        .expect_err("an environment without PATH is refused");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    assert!(harness.pending().is_none(), "no transaction was journaled");
    harness.assert_installed(V1);
}

#[tokio::test]
async fn a_refused_runtime_plan_stops_the_upgrade_before_any_effect_even_when_accepted() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    let preflight = ScriptedPreflight {
        runtimes: Some(runtime_report(
            true,
            vec![package(PackageStatus::Install)],
            Vec::new(),
            vec![Refusal {
                subject: "claude".to_owned(),
                code: CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM.to_owned(),
                detail: "the release lists no package for this platform".to_owned(),
            }],
        )),
        ..ScriptedPreflight::default()
    };

    let error = harness
        .engine()
        .with_runtime_loss_accepted(true)
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("a refused plan is not overridable");

    assert_eq!(error.code(), "service_upgrade_runtimes_refused");
    assert!(harness.runtimes.calls().is_empty(), "no runtime step ran");
    assert!(harness.pending().is_none(), "no transaction was journaled");
    assert!(
        !layout::version_exists(&harness.layout(), V2).expect("version probe"),
        "no version directory was published"
    );
    assert_eq!(
        harness.fake.world().replaces,
        0,
        "the daemon was not replaced"
    );
    assert!(harness.fake.world().running, "the old daemon still runs");
    harness.assert_installed(V1);
}

#[tokio::test]
async fn a_stopped_session_at_risk_needs_the_flag_like_a_live_one() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let stopped = SessionVerdict {
        session_id: OTHER_SESSION.to_owned(),
        name: None,
        verdict: Verdict::WouldLoseRecovery,
        code: "native_recovery_unmappable".to_owned(),
        detail: "the stopped session's recovery cannot be mapped".to_owned(),
        acceptance_scope: None,
    };
    let scripted = || ScriptedPreflight {
        stopped: vec![stopped.clone()],
        ..ScriptedPreflight::default()
    };

    let error = harness
        .engine()
        .with_adoption_preflight(Box::new(scripted()))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("the stopped session would lose its recovery");
    let Error::UpgradeAtRisk {
        sessions,
        stopped_sessions,
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert!(sessions.is_empty());
    assert_eq!(stopped_sessions, std::slice::from_ref(&stopped));
    assert!(harness.pending().is_none(), "no transaction was journaled");
    harness.assert_installed(V1);

    let report = harness
        .engine()
        .with_runtime_loss_accepted(true)
        .with_adoption_preflight(Box::new(scripted()))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("the flag accepts the stopped session's loss");
    assert!(report.accepted_runtime_loss);
    harness.assert_installed(V2);
}

/// The `PATH` a job without its own gets from its service manager now: the
/// systemd user manager's on Linux, launchd's active state on macOS.
fn set_job_path(harness: &mut Harness, path: &str) {
    harness.context = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged);
    #[cfg(target_os = "linux")]
    {
        harness.context = harness
            .context
            .clone()
            .with_user_manager(Arc::new(FakeUserManager(Ok(vec![format!("PATH={path}")]))));
    }
    #[cfg(target_os = "macos")]
    {
        harness.context = harness
            .context
            .clone()
            .with_launchd_job(Arc::new(FakeLaunchd::new(Ok(loaded_job(path.to_owned())))));
    }
}

const JOURNALED_PATH: &str = "/opt/journaled/bin:/usr/bin";
const CURRENT_PATH: &str = "/opt/current/bin:/usr/bin";

/// Installs V1, starts an upgrade to V2 that stops after registering the new
/// daemon, and returns the harness with that journal pending.
async fn interrupted_upgrade_journal(harness: &mut Harness) {
    set_job_path(harness, JOURNALED_PATH);
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after the registration");
}

/// A manager variable the default allowlist selects, and one it does not.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const ALLOWLISTED_MANAGER_VARIABLE: (&str, &str) = ("LC_CTYPE", "C.UTF-8");
#[cfg(any(target_os = "linux", target_os = "macos"))]
const UNLISTED_MANAGER_VARIABLE: (&str, &str) = ("CLAUDE_CONFIG_DIR", "/manager/unlisted-store");

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_upgrade_judges_with_the_allowlisted_manager_variables_and_no_others() {
    let mut harness = Harness::new();
    set_job_path(&mut harness, CURRENT_PATH);
    let (allowed_name, allowed_value) = ALLOWLISTED_MANAGER_VARIABLE;
    let (unlisted_name, unlisted_value) = UNLISTED_MANAGER_VARIABLE;
    harness.context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(FakeUserManager(Ok(vec![
            format!("PATH={CURRENT_PATH}"),
            format!("{allowed_name}={allowed_value}"),
            format!("{unlisted_name}={unlisted_value}"),
        ]))));
    harness.install(V1).await.expect("install");
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.variables);

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("the upgrade judges and applies");

    let judged = judged.lock().expect("variables lock").clone();
    assert!(!judged.is_empty(), "the upgrade judged");
    for variables in &judged {
        assert_eq!(
            variables.get(allowed_name).map(String::as_str),
            Some(allowed_value),
            "an allowlisted manager variable reaches the judgement"
        );
        assert!(
            !variables.contains_key(unlisted_name),
            "a manager variable outside the allowlist never reaches the judgement"
        );
        assert!(
            !variables.contains_key("PATH"),
            "PATH is carried by the search path, not the variables"
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_upgrade_judges_with_the_allowlisted_launchd_domain_variables_and_no_others() {
    let mut harness = Harness::new();
    let (allowed_name, allowed_value) = ALLOWLISTED_MANAGER_VARIABLE;
    let (unlisted_name, unlisted_value) = UNLISTED_MANAGER_VARIABLE;
    harness.context = harness
        .context
        .clone()
        .with_launchd_job(Arc::new(FakeLaunchd::new(Ok(ActiveJob {
            domain: Some(CURRENT_PATH.to_owned()),
            loaded_default: None,
            domain_environment: Some(vec![
                ("PATH".to_owned(), CURRENT_PATH.to_owned()),
                (allowed_name.to_owned(), allowed_value.to_owned()),
                (unlisted_name.to_owned(), unlisted_value.to_owned()),
            ]),
        }))));
    harness.install(V1).await.expect("install");
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.variables);

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("the upgrade judges and applies");

    let judged = judged.lock().expect("variables lock").clone();
    assert!(!judged.is_empty(), "the upgrade judged");
    for variables in &judged {
        assert_eq!(
            variables.get(allowed_name).map(String::as_str),
            Some(allowed_value)
        );
        assert!(!variables.contains_key(unlisted_name));
        assert!(!variables.contains_key("PATH"));
    }
}

/// The debug form of the job's launch facts and of launchd's active state names
/// the variables and never prints their values.
#[test]
fn the_debug_forms_of_the_job_environment_name_the_variables_and_never_their_values() {
    const SENTINEL: &str = "/sentinel/secret-store-value";
    let variables = BTreeMap::from([("CLAUDE_CONFIG_DIR".to_owned(), SENTINEL.to_owned())]);
    let job = JobPath {
        value: CURRENT_PATH.to_owned(),
        source: PathSource::Recorded,
        path_drop_in: None,
        loaded_default: None,
        variables,
        manager_unreachable: None,
    };
    let active = crate::service::manager_env::ActiveJob {
        domain: None,
        loaded_default: None,
        domain_environment: Some(vec![("CLAUDE_CONFIG_DIR".to_owned(), SENTINEL.to_owned())]),
    };
    for debug in [format!("{job:?}"), format!("{active:?}")] {
        assert!(debug.contains("CLAUDE_CONFIG_DIR"), "{debug}");
        assert!(!debug.contains(SENTINEL), "{debug}");
    }
}

/// A user manager that answers with the given failure.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct FailingManager(crate::service::manager_env::EnvironmentFailure);

#[cfg(target_os = "linux")]
impl UserManager for FailingManager {
    fn environment(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<Vec<String>, crate::service::manager_env::EnvironmentFailure>,
                > + Send
                + '_,
        >,
    > {
        let failure = self.0.clone();
        Box::pin(async move { Err(failure) })
    }

    fn unit_environment<'a>(
        &'a self,
        _unit: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<
                        pohunek_platform::supervisor::UnitEnvironment,
                        crate::service::manager_env::EnvironmentFailure,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let failure = self.0.clone();
        Box::pin(async move { Err(failure) })
    }
}

/// A job that records its `PATH` is judged with the daemon unit's settings too,
/// so a manager that cannot be reached or answers unusably refuses the plan and
/// the judgement alike, and nothing falls back to the recorded `PATH` alone.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_recorded_path_without_a_bus_is_refused() {
    use crate::service::manager_env::EnvironmentFailure;
    use pohunek_platform::shell_env::SearchPath;

    let mut harness = Harness::new();
    let search_path =
        SearchPath::new(vec![std::path::PathBuf::from("/usr/bin")]).expect("a trusted directory");
    let config = crate::service::definition::config_with_search_path(
        &harness.context,
        Path::new("/opt/pohunek"),
        crate::service::VERSION,
        search_path,
    )
    .expect("the recorded configuration");

    for failure in [
        EnvironmentFailure::Unreachable("no session bus".to_owned()),
        EnvironmentFailure::Unusable("the answer is not a list".to_owned()),
    ] {
        let detail = match &failure {
            EnvironmentFailure::Unreachable(detail) | EnvironmentFailure::Unusable(detail) => {
                detail.clone()
            }
        };
        harness.context = harness
            .context
            .clone()
            .with_user_manager(Arc::new(FailingManager(failure)));
        let judged = crate::service::engine::job_path(&harness.context, &config, None)
            .await
            .expect_err("the judgement is refused");
        let planned = crate::service::engine::job_search_path(&harness.context, &config, None)
            .await
            .expect_err("the plan is refused");
        for error in [judged, planned] {
            assert_eq!(error.code(), "service_manager_environment_unavailable");
            assert!(error.to_string().contains(&detail), "{error}");
        }
    }
}

/// The bus answers but the unit's properties cannot be read: the recorded `PATH`
/// is refused, whatever the unit may hold.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn unreadable_unit_properties_refuse_a_recorded_path() {
    use crate::service::manager_env::EnvironmentFailure;
    use pohunek_platform::shell_env::SearchPath;

    let harness = Harness::new();
    let search_path =
        SearchPath::new(vec![std::path::PathBuf::from("/usr/bin")]).expect("a trusted directory");
    let config = crate::service::definition::config_with_search_path(
        &harness.context,
        Path::new("/opt/pohunek"),
        crate::service::VERSION,
        search_path,
    )
    .expect("the recorded configuration");
    let context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(UnitManager {
            unit: Err(EnvironmentFailure::Unusable(
                "the property read was rejected".to_owned(),
            )),
        }));
    let error = crate::service::engine::job_path(&context, &config, None)
        .await
        .expect_err("unreadable properties are refused");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    assert!(
        error.to_string().contains("the property read was rejected"),
        "{error}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_rollback_judges_with_the_manager_path_current_at_rollback() {
    let mut harness = Harness::new();
    interrupted_upgrade_journal(&mut harness).await;
    // The manager's environment changes after the upgrade and before the rollback.
    set_job_path(&mut harness, CURRENT_PATH);
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V1), V1)
        .await
        .expect("the pending upgrade is rolled back");

    let judged = judged.lock().expect("search path lock").clone();
    assert!(!judged.is_empty(), "the rollback judged");
    assert!(
        judged.iter().all(|path| path == CURRENT_PATH),
        "the rollback judges with the manager's PATH as the restored job gets it: {judged:?}"
    );
    // The restored job records no PATH, so it gets the manager's environment.
    let registered = harness
        .fake
        .world()
        .registered
        .clone()
        .expect("the restored daemon is registered");
    assert_eq!(registered.environment().get("PATH"), None);
    harness.assert_installed(V1);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_rollback_judges_with_the_domain_path_over_the_journaled_default() {
    const DOMAIN_PATH: &str = "/opt/domain/bin:/usr/bin";
    let mut harness = Harness::new();
    interrupted_upgrade_journal(&mut harness).await;
    harness.context = harness
        .context
        .clone()
        .with_launchd_job(Arc::new(FakeLaunchd::new(Ok(domain_over(
            DOMAIN_PATH,
            Some(CURRENT_PATH),
        )))));
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V1), V1)
        .await
        .expect("the pending upgrade is rolled back");

    let judged = judged.lock().expect("search path lock").clone();
    assert!(
        !judged.is_empty() && judged.iter().all(|path| path == DOMAIN_PATH),
        "the domain's PATH is what the job gets now: {judged:?}"
    );
    harness.assert_installed(V1);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_job_unloaded_before_the_runtime_step_resolves_the_journaled_default_not_the_plist() {
    const JOURNALED: &str = "/opt/journaled/bin:/usr/bin";
    const PERSISTED: &str = "/opt/persisted/bin:/usr/bin";
    let launchd = Arc::new(FakeLaunchd::new(Ok(loaded_job(JOURNALED))));
    let mut harness = Harness::new();
    let domain = harness.root.join("user.plist");
    std::fs::write(&domain, launchd_domain_xml(PERSISTED)).expect("domain configuration");
    harness.context = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged)
        .with_launchd_job(Arc::clone(&launchd) as Arc<dyn LaunchdJob>)
        .with_launchd_domain_config(&domain);
    harness.install(V1).await.expect("install");
    harness.runtimes.script().search_paths.clear();
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);
    let mut engine = harness
        .engine()
        .with_adoption_preflight(Box::new(preflight));
    // The early gate reads the loaded default and journals it; the job is unloaded
    // before the swap, so the late gate and the runtime step are not given a live
    // default by launchd.
    let unloaded = Arc::clone(&launchd);
    engine.before_swap = Some(SwapHook(Box::new(move || {
        unloaded.set(Ok(not_loaded()));
    })));

    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade");

    assert_eq!(
        judged.lock().expect("search path lock").clone(),
        [JOURNALED.to_owned(), JOURNALED.to_owned()],
        "the early and the late gate judge with the journaled default"
    );
    let runtime = harness.runtimes.script().search_paths.clone();
    assert!(!runtime.is_empty(), "the runtime step ran");
    assert!(
        runtime.iter().all(|path| path == JOURNALED),
        "the runtime step does not fall back to the plist: {runtime:?}"
    );
    harness.assert_installed(V2);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_journal_without_a_launchd_default_takes_the_persisted_plist_and_warns() {
    const PERSISTED: &str = "/opt/persisted/bin:/usr/bin";
    let mut harness = Harness::new();
    interrupted_upgrade_journal(&mut harness).await;
    // A journal that does not hold the launchd default.
    let journal = harness.context.paths().state_dir.join(record::FILE_NAME);
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).expect("journal")).expect("journal JSON");
    document
        .as_object_mut()
        .expect("journal object")
        .remove("launchd_default_path");
    std::fs::write(
        &journal,
        serde_json::to_vec(&document).expect("journal bytes"),
    )
    .expect("rewrite journal");
    let domain = harness.root.join("user.plist");
    std::fs::write(&domain, launchd_domain_xml(PERSISTED)).expect("domain configuration");
    // The persisted file is newer than the boot, so a change may be pending.
    let launchd = FakeLaunchd::new(Ok(not_loaded()));
    booted_an_hour_ago(&launchd);
    harness.context = harness
        .context
        .clone()
        .with_launchd_job(Arc::new(launchd))
        .with_launchd_domain_config(&domain);
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);

    let report = harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V1), V1)
        .await
        .expect("the pending upgrade is rolled back");

    let judged = judged.lock().expect("search path lock").clone();
    assert!(
        !judged.is_empty() && judged.iter().all(|path| path == PERSISTED),
        "the persisted configuration is the last evidence: {judged:?}"
    );
    let rolled_back = report.rolled_back.expect("the rollback is reported");
    assert_eq!(rolled_back.warnings.len(), 1, "{:?}", rolled_back.warnings);
    assert!(
        rolled_back.warnings[0].contains("persisted launchd configuration was used"),
        "{:?}",
        rolled_back.warnings
    );
    harness.assert_installed(V1);
}

/// The persisted user-domain `PATH` a not-loaded job gets at the early gate.
#[cfg(target_os = "macos")]
const PERSISTED_PATH: &str = "/opt/persisted/bin:/usr/bin";

/// A V1 install whose daemon job records no `PATH`, with a launchd user-domain
/// configuration directory in the test root; returns the file's path.
#[cfg(target_os = "macos")]
async fn unrecorded_install_with_domain_config(harness: &mut Harness) -> std::path::PathBuf {
    let config_dir = harness.root.join("launchd-domain");
    std::fs::create_dir_all(&config_dir).expect("configuration directory");
    let domain = config_dir.join("user.plist");
    harness.context = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged)
        .with_launchd_job(Arc::new(FakeLaunchd::new(Ok(loaded_job(CURRENT_PATH)))))
        .with_launchd_domain_config(&domain);
    harness.install(V1).await.expect("install");
    domain
}

/// Upgrades an installed job that launchd has not loaded and no journal holds a
/// default for; returns the result and the `PATH` of every judgement.
#[cfg(target_os = "macos")]
async fn upgrade_of_an_unloaded_job(
    harness: &mut Harness,
    domain: &std::path::Path,
    persisted: Option<&str>,
    dated: impl FnOnce(&std::path::Path),
    host: impl FnOnce(&FakeLaunchd),
) -> (Result<UpgradeReport, Error>, Vec<String>) {
    if let Some(value) = persisted {
        std::fs::write(domain, launchd_domain_xml(value)).expect("domain configuration");
    }
    dated(domain);
    let launchd = FakeLaunchd::new(Ok(not_loaded()));
    host(&launchd);
    harness.context = harness.context.clone().with_launchd_job(Arc::new(launchd));
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);
    let result = harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await;
    let judged = judged.lock().expect("search path lock").clone();
    (result, judged)
}

/// Dates the configuration directory before the boot, as a file of an old
/// configuration, or the directory an absent one would have been removed from.
#[cfg(target_os = "macos")]
fn dated_before_boot(domain: &std::path::Path) {
    let target = if domain.exists() {
        domain
    } else {
        domain.parent().expect("configuration directory")
    };
    date(target, Modified::BeforeBoot).expect("date");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_fresh_upgrade_of_an_unloaded_job_without_persisted_path_uses_the_standard_path() {
    let mut harness = Harness::new();
    let domain = unrecorded_install_with_domain_config(&mut harness).await;
    let (result, judged) =
        upgrade_of_an_unloaded_job(&mut harness, &domain, None, dated_before_boot, |_| {}).await;
    let report = result.expect("upgrade");
    assert!(
        !judged.is_empty()
            && judged
                .iter()
                .all(|path| path == pohunek_platform::supervisor::launchd::STANDARD_PATH),
        "{judged:?}"
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    harness.assert_installed(V2);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_persisted_path_older_than_the_boot_is_active() {
    let mut harness = Harness::new();
    let domain = unrecorded_install_with_domain_config(&mut harness).await;
    let (result, judged) = upgrade_of_an_unloaded_job(
        &mut harness,
        &domain,
        Some(PERSISTED_PATH),
        dated_before_boot,
        |_| {},
    )
    .await;
    let report = result.expect("upgrade");
    assert!(
        !judged.is_empty() && judged.iter().all(|path| path == PERSISTED_PATH),
        "{judged:?}"
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    harness.assert_installed(V2);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_possibly_pending_persisted_path_refuses_the_forward_gate() {
    let mut harness = Harness::new();
    let domain = unrecorded_install_with_domain_config(&mut harness).await;
    let (result, judged) = upgrade_of_an_unloaded_job(
        &mut harness,
        &domain,
        Some(PERSISTED_PATH),
        |domain| date(domain, Modified::AfterBoot).expect("date"),
        |_| {},
    )
    .await;
    let error = result.expect_err("a change may be pending");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    let text = error.to_string();
    assert!(
        text.contains("launchctl setenv PATH") && text.contains("reboot"),
        "{text}"
    );
    assert!(!text.contains(PERSISTED_PATH), "values stay out: {text}");
    assert!(judged.is_empty(), "nothing was judged: {judged:?}");
    assert_eq!(harness.pending(), None, "no journal was written");
    harness.assert_installed(V1);
}

/// The pending upgrade of [`interrupted_upgrade_journal`], whose journal holds no
/// launchd default, rolled back on a host whose launchd does not have the job
/// loaded; `persisted` is dated by `dated`, and `host` shapes the fake host.
#[cfg(target_os = "macos")]
async fn rollback_without_a_journaled_default(
    harness: &mut Harness,
    dated: impl FnOnce(&std::path::Path),
    host: impl FnOnce(&FakeLaunchd),
) -> (Result<UpgradeReport, Error>, Vec<String>) {
    let domain = harness.root.join("user.plist");
    std::fs::write(&domain, launchd_domain_xml(PERSISTED_PATH)).expect("domain configuration");
    harness.context = harness.context.clone().with_launchd_domain_config(&domain);
    interrupted_upgrade_journal(harness).await;
    let journal = harness.context.paths().state_dir.join(record::FILE_NAME);
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).expect("journal")).expect("journal JSON");
    document
        .as_object_mut()
        .expect("journal object")
        .remove("launchd_default_path");
    std::fs::write(
        &journal,
        serde_json::to_vec(&document).expect("journal bytes"),
    )
    .expect("rewrite journal");
    dated(&domain);
    let launchd = FakeLaunchd::new(Ok(not_loaded()));
    host(&launchd);
    harness.context = harness.context.clone().with_launchd_job(Arc::new(launchd));
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);
    let result = harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V1), V1)
        .await;
    let judged = judged.lock().expect("search path lock").clone();
    (result, judged)
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_rollback_uses_an_active_persisted_path_without_warning() {
    let mut harness = Harness::new();
    let (result, judged) = rollback_without_a_journaled_default(
        &mut harness,
        |domain| date(domain, Modified::BeforeBoot).expect("date"),
        |_| {},
    )
    .await;
    let report = result.expect("the pending upgrade is rolled back");
    assert!(
        !judged.is_empty() && judged.iter().all(|path| path == PERSISTED_PATH),
        "{judged:?}"
    );
    let rolled_back = report.rolled_back.expect("the rollback is reported");
    assert!(
        rolled_back.warnings.is_empty(),
        "{:?}",
        rolled_back.warnings
    );
    harness.assert_installed(V1);
}

/// An upgrade interrupted after `interrupted_after`, whose journal holds the
/// launchd default, on a host whose launchd no longer has the job loaded and
/// whose `user.plist` was written after the boot; returns the preflight to
/// install and the recorder of the `PATH`s it judges.
#[cfg(target_os = "macos")]
async fn interrupted_upgrade_with_a_pending_plist(
    harness: &mut Harness,
    interrupted_after: Step,
) -> (ScriptedPreflight, Arc<Mutex<Vec<String>>>) {
    set_job_path(harness, JOURNALED_PATH);
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(interrupted_after);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted");
    let domain = harness.root.join("user.plist");
    std::fs::write(&domain, launchd_domain_xml(PERSISTED_PATH)).expect("domain configuration");
    date(&domain, Modified::AfterBoot).expect("date");
    harness.context = harness
        .context
        .clone()
        .with_launchd_job(Arc::new(FakeLaunchd::new(Ok(not_loaded()))))
        .with_launchd_domain_config(&domain);
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);
    (preflight, judged)
}

/// The adoption gate of a resumed upgrade judges with the launchd default the
/// transaction journaled, as its later gates do, not with the pending plist.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_resume_judges_the_adoption_gate_with_the_journaled_default() {
    let mut harness = Harness::new();
    let (preflight, judged) =
        interrupted_upgrade_with_a_pending_plist(&mut harness, Step::Definition).await;

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("the interrupted upgrade resumes");

    let judged = judged.lock().expect("search path lock").clone();
    assert!(
        !judged.is_empty() && judged.iter().all(|path| path == JOURNALED_PATH),
        "{judged:?}"
    );
    harness.assert_installed(V2);
}

/// The same for the rollback of a pending upgrade that another version's upgrade
/// runs first.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_rollback_to_another_version_judges_the_adoption_gate_with_the_journaled_default() {
    let mut harness = Harness::new();
    let (preflight, judged) =
        interrupted_upgrade_with_a_pending_plist(&mut harness, Step::Registered).await;

    // Once the rollback is done the new transaction has no journaled default, so
    // the upgrade to the third version may be refused by a later gate; the gate
    // that precedes the rollback is under test.
    let result = harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V3), V3)
        .await;

    let judged = judged.lock().expect("search path lock").clone();
    assert_eq!(
        judged.first().map(String::as_str),
        Some(JOURNALED_PATH),
        "{judged:?}"
    );
    match result {
        Ok(_) => harness.assert_installed(V3),
        Err(error) => {
            assert_eq!(error.code(), "service_manager_environment_unavailable");
            harness.assert_installed(V1);
        }
    }
}

/// `service check` judges the adoption gate of a resumed upgrade, and of one that
/// rolls another back first, with the launchd default the pending transaction
/// journaled, as the upgrade does.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_check_judges_a_pending_transaction_with_its_journaled_default() {
    for (interrupted_after, version) in [(Step::Definition, V2), (Step::Registered, V3)] {
        let mut harness = Harness::new();
        let (preflight, judged) =
            interrupted_upgrade_with_a_pending_plist(&mut harness, interrupted_after).await;

        crate::service::check_with_preflight(
            &harness.context,
            &harness.staged(version),
            &preflight,
            version,
            false,
        )
        .await
        .expect("the check judges with the journaled default");

        let judged = judged.lock().expect("search path lock").clone();
        assert!(
            !judged.is_empty() && judged.iter().all(|path| path == JOURNALED_PATH),
            "{version}: {judged:?}"
        );
    }
}

/// Without a journaled default a possibly pending plist refuses the forward
/// upgrade that would roll the pending one back first, before any effect.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_rollback_to_another_version_without_a_journaled_default_refuses_before_any_effect() {
    let mut harness = Harness::new();
    let (preflight, judged) =
        interrupted_upgrade_with_a_pending_plist(&mut harness, Step::Registered).await;
    let journal = harness.context.paths().state_dir.join(record::FILE_NAME);
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).expect("journal")).expect("journal JSON");
    document
        .as_object_mut()
        .expect("journal object")
        .remove("launchd_default_path");
    std::fs::write(
        &journal,
        serde_json::to_vec(&document).expect("journal bytes"),
    )
    .expect("rewrite journal");
    let error = harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V3), V3)
        .await
        .expect_err("a change may be pending");

    assert_eq!(error.code(), "service_manager_environment_unavailable");
    assert!(
        judged.lock().expect("search path lock").is_empty(),
        "nothing was judged"
    );
    assert!(
        harness.pending().is_some(),
        "the pending upgrade is untouched"
    );
}

/// A rollback goes on with launchd's standard `PATH` as a stand-in when the
/// persisted configuration cannot be read; a forward gate refuses.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_unreadable_persisted_configuration_does_not_strand_a_rollback() {
    use std::os::unix::fs::PermissionsExt;

    let unreadable = |domain: &std::path::Path| {
        date(domain, Modified::BeforeBoot).expect("date");
        std::fs::set_permissions(domain, std::fs::Permissions::from_mode(0o000))
            .expect("permissions");
    };

    let mut harness = Harness::new();
    let (result, judged) =
        rollback_without_a_journaled_default(&mut harness, unreadable, |_| {}).await;
    let report = result.expect("the pending upgrade is rolled back");
    assert!(
        !judged.is_empty()
            && judged
                .iter()
                .all(|path| path == pohunek_platform::supervisor::launchd::STANDARD_PATH),
        "{judged:?}"
    );
    let rolled_back = report.rolled_back.expect("the rollback is reported");
    assert_eq!(rolled_back.warnings.len(), 1, "{:?}", rolled_back.warnings);
    harness.assert_installed(V1);

    let mut harness = Harness::new();
    let domain = unrecorded_install_with_domain_config(&mut harness).await;
    let (result, judged) = upgrade_of_an_unloaded_job(
        &mut harness,
        &domain,
        Some(PERSISTED_PATH),
        unreadable,
        |_| {},
    )
    .await;
    let error = result.expect_err("a forward gate refuses");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    assert!(judged.is_empty(), "nothing was judged: {judged:?}");
    harness.assert_installed(V1);
}

/// A boot time that cannot be read, or a configuration changed within the margin
/// of the boot, may hide a pending change: a forward gate refuses, and a
/// rollback goes on with the persisted value and says so.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_unreadable_boot_or_file_time_is_treated_as_pending() {
    let unreadable_boot =
        |launchd: &FakeLaunchd| launchd.set_boot_time(Err("sysctl failed".to_owned()));
    let within_margin = |domain: &std::path::Path| {
        let when = boot_time() - crate::service::settings::LAUNCHD_CONFIG_BOOT_MARGIN / 2;
        std::fs::File::open(domain)
            .and_then(|file| file.set_modified(when))
            .expect("date");
    };

    for host_unreadable in [true, false] {
        let host = |launchd: &FakeLaunchd| {
            if host_unreadable {
                unreadable_boot(launchd);
            }
        };
        let dated = |domain: &std::path::Path| {
            if host_unreadable {
                dated_before_boot(domain);
            } else {
                within_margin(domain);
            }
        };

        let mut harness = Harness::new();
        let domain = unrecorded_install_with_domain_config(&mut harness).await;
        let (result, _) =
            upgrade_of_an_unloaded_job(&mut harness, &domain, Some(PERSISTED_PATH), dated, host)
                .await;
        let error = result.expect_err("a forward gate refuses");
        assert_eq!(error.code(), "service_manager_environment_unavailable");
        harness.assert_installed(V1);

        let mut harness = Harness::new();
        let (result, judged) =
            rollback_without_a_journaled_default(&mut harness, dated, host).await;
        let report = result.expect("a rollback goes on");
        assert!(
            !judged.is_empty() && judged.iter().all(|path| path == PERSISTED_PATH),
            "{judged:?}"
        );
        let warnings = report.rolled_back.expect("rolled back").warnings;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("persisted launchd configuration was used"));
    }
}

#[tokio::test]
async fn the_runtime_judgment_and_plan_run_with_the_path_of_the_installed_job() {
    let mut harness = Harness::new();
    let dir = harness.root.join("job-tools");
    harness.context = discovering(&harness.context, &harness.root, "job-shell", &dir);
    harness.install(V1).await.expect("install");
    let expected = harness
        .config()
        .expect("config")
        .search_path()
        .to_env_value();
    assert_eq!(expected, dir.display().to_string());
    harness.runtimes.script().search_paths.clear();
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade");

    // The early gate of the upgrade and the late gate before the daemon swap.
    let judged = judged.lock().expect("search path lock").clone();
    assert_eq!(
        judged,
        [expected.clone(), expected.clone()],
        "every judgment ran with the job PATH"
    );
    let runtime_paths = harness.runtimes.script().search_paths.clone();
    assert!(!runtime_paths.is_empty(), "the runtime step ran");
    assert!(
        runtime_paths.iter().all(|path| *path == expected),
        "every runtime call ran with the job PATH: {runtime_paths:?}"
    );
}

#[tokio::test]
async fn only_the_upgrades_own_gates_judge_the_release_plan() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    harness.runtimes.plan(installing_plan());
    let preflight = ScriptedPreflight::default();
    let plans = Arc::clone(&preflight.plans);

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade");

    // The early gate and the late gate before the swap judge the plan; the
    // post-apply judgement runs after the apply, when the plan is already the
    // installed state, so it judges the installed packages only.
    assert_eq!(
        plans.lock().expect("plan lock").clone(),
        [
            ReleasePlan::Included,
            ReleasePlan::Included,
            ReleasePlan::Omitted,
        ]
    );
}

#[tokio::test]
async fn an_upgrade_stops_the_daemon_and_backs_up_before_it_applies_the_runtime_plan() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    // The install runs the runtime step too; the calls under test start here.
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    harness.runtimes.plan(installing_plan());

    let report = harness.upgrade(V2).await.expect("upgrade");
    assert_eq!(
        harness.runtimes.calls(),
        ["plan", "apply registered=false", "plan"],
        "the daemon is stopped when the apply runs, and the post-ready plan follows"
    );
    let (before, after) = harness.runtimes.last_two_removal_counts();
    assert_eq!(after, before + 1, "the apply stops the daemon once");
    let runtimes = report.runtimes.expect("the apply is reported");
    assert!(!runtimes.dry_run);
    assert_eq!(runtimes.packages[0].status, PackageStatus::Installed);
    assert_eq!(
        runtime_state(&harness),
        (
            Some(APPLIED_REGISTRY.to_owned()),
            "applied profile".to_owned()
        )
    );
    assert!(!harness
        .context
        .paths()
        .state_dir
        .join(record::RUNTIME_BACKUP_DIR_NAME)
        .exists());
    harness.assert_installed(V2);
}

#[tokio::test]
async fn a_failed_runtime_apply_restores_the_state_it_changed_and_the_previous_daemon() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    // The install runs the runtime step too; the calls under test start here.
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    harness.runtimes.plan(installing_plan());
    harness.runtimes.script().fail_apply = true;

    let error = harness.upgrade(V2).await.expect_err("the apply fails");
    assert!(
        matches!(error, Error::UpgradeRuntimesFailed { .. }),
        "{error:?}"
    );
    assert_eq!(
        runtime_state(&harness),
        (None, "profile before".to_owned()),
        "the registry and the profile are restored byte for byte"
    );
    assert!(
        !harness
            .context
            .paths()
            .plugins_dir()
            .join("catalog-state.json")
            .exists(),
        "a file the apply created is removed"
    );
    assert_eq!(harness.pending(), None);
    harness.assert_installed(V1);
    assert_eq!(
        layout::installed_versions(&harness.layout()).expect("versions"),
        [V1]
    );
}

/// A registry that selects an older official Pi package, and a Pi profile
/// pinned to it, as an operator who installed Pi by hand leaves them.
const PI_REGISTRY_BEFORE: &str = concat!(
    r#"{"schema":1,"generation":2,"packages":[{"digest":"sha256:"#,
    "1111111111111111111111111111111111111111111111111111111111111111",
    r#"","identity":{"id":"pohunek.runtime.pi","version":"1.0.2"},"source":"official","#,
    r#""manifest":"2222222222222222222222222222222222222222222222222222222222222222","#,
    r#""installed_at_unix_seconds":1,"enabled":true}],"selected":{"pohunek.runtime.pi":"sha256:"#,
    "1111111111111111111111111111111111111111111111111111111111111111",
    r#""}}"#
);

#[tokio::test]
async fn a_rolled_back_upgrade_restores_the_pi_selection_and_pins() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    let plugins = harness.context.paths().plugins_dir();
    write_private(&plugins.join("registry.json"), PI_REGISTRY_BEFORE);
    let pinned = "base = \"pi\"\npackage = \"pohunek.runtime.pi\"\ndigest = \"sha256:1111111111111111111111111111111111111111111111111111111111111111\"\n";
    write_private(&profile_path(&harness), pinned);
    harness.runtimes.plan(installing_plan());
    harness.runtimes.script().fail_apply = true;

    let error = harness.upgrade(V2).await.expect_err("the apply fails");
    assert!(
        matches!(error, Error::UpgradeRuntimesFailed { .. }),
        "{error:?}"
    );
    assert_eq!(
        runtime_state(&harness),
        (Some(PI_REGISTRY_BEFORE.to_owned()), pinned.to_owned()),
        "the Pi selection and the Pi profile are restored byte for byte"
    );
    harness.assert_installed(V1);
}

/// Writes a state file with exactly `mode`, as an editor or a copy leaves it.
fn write_with_mode(path: &Path, bytes: &str, mode: u32) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    std::fs::write(path, bytes).expect("state file");
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .expect("mode");
}

fn profile_path(harness: &Harness) -> PathBuf {
    harness
        .context
        .paths()
        .config_dir
        .join("agents")
        .join("work.toml")
}

fn mode_of(path: &Path) -> u32 {
    std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(path).expect("metadata").permissions(),
    ) & 0o777
}

#[tokio::test]
async fn a_group_readable_profile_upgrades_and_a_failed_apply_restores_its_mode() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    // An editor's umask leaves a profile readable by others; the daemon
    // accepts it, so the backup must too.
    write_with_mode(&profile_path(&harness), "profile before", 0o644);
    harness.runtimes.plan(installing_plan());
    harness.runtimes.script().fail_apply = true;

    let error = harness.upgrade(V2).await.expect_err("the apply fails");
    assert!(
        matches!(error, Error::UpgradeRuntimesFailed { .. }),
        "{error:?}"
    );
    assert_eq!(runtime_state(&harness), (None, "profile before".to_owned()));
    assert_eq!(
        mode_of(&profile_path(&harness)),
        0o644,
        "the mode is restored"
    );
    harness.assert_installed(V1);
}

#[tokio::test]
async fn a_group_readable_profile_is_applied_by_an_upgrade() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    write_with_mode(&profile_path(&harness), "profile before", 0o644);
    harness.runtimes.plan(installing_plan());

    harness.upgrade(V2).await.expect("upgrade");
    assert_eq!(runtime_state(&harness).1, "applied profile");
    harness.assert_installed(V2);
}

#[tokio::test]
async fn a_registry_the_package_store_would_not_read_refuses_the_upgrade_before_any_effect() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    write_with_mode(
        &harness.context.paths().plugins_dir().join("registry.json"),
        "before",
        0o644,
    );
    harness.runtimes.plan(installing_plan());

    // The plan is judged against the registry first, so the unreadable record
    // refuses before the backup, the stop, or any apply.
    let error = harness
        .upgrade(V2)
        .await
        .expect_err("the registry is refused");
    let Error::UpgradeRuntimesRefused { refusals } = &error else {
        panic!("a registry of another mode is refused: {error:?}");
    };
    assert_eq!(refusals[0].code, "runtime_plan_unavailable");
    assert!(
        refusals[0].detail.contains("owner-private"),
        "{}",
        refusals[0].detail
    );
    assert_eq!(
        harness.runtimes.calls(),
        Vec::<String>::new(),
        "no plan ran"
    );
    harness.assert_installed(V1);
    assert_eq!(
        mode_of(&harness.context.paths().plugins_dir().join("registry.json")),
        0o644
    );
}

/// What the post-apply judgment of a test host reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum PostApply {
    /// Every live session stays adoptable.
    #[default]
    Clean,
    /// One live session would lose its recovery.
    AtRisk,
    /// The release's preflight cannot produce a report.
    CannotRun,
}

/// The adoption preflight of a test host. A judgment made before the runtime
/// apply reads the real store; a judgment made after it is the post-apply
/// judgment, which the test scripts.
#[derive(Debug, Clone)]
struct PostApplyPreflight {
    runtimes: RuntimeFake,
    post: PostApply,
    judged_after_apply: Arc<AtomicUsize>,
}

impl AdoptionPreflight for PostApplyPreflight {
    fn judge<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a VerifiedVersionDir,
        search_path: &'a str,
        variables: &'a BTreeMap<String, String>,
        probe_budget: Option<Duration>,
        plan: ReleasePlan,
    ) -> Judging<'a> {
        Box::pin(async move {
            // Only the new release's judgment after the apply is scripted; the
            // rollback judges the previous release's daemon as it stands.
            let new_release = dir.path().components().any(|part| part.as_os_str() == V2);
            if !self.runtimes.applied() || !new_release || self.post == PostApply::Clean {
                return InProcessPreflight::default()
                    .judge(context, dir, search_path, variables, probe_budget, plan)
                    .await;
            }
            self.judged_after_apply.fetch_add(1, Ordering::SeqCst);
            if self.post == PostApply::CannotRun {
                return Err(Error::UpgradePreflightFailed {
                    binary: dir.daemon(),
                    detail: "the scripted post-apply judgment produced no report".to_owned(),
                });
            }
            let mut report = InProcessPreflight::default()
                .judge(context, dir, search_path, variables, probe_budget, plan)
                .await?;
            report.sessions.push(SessionVerdict {
                session_id: "s-after-apply".to_owned(),
                name: None,
                verdict: Verdict::WouldLoseRecovery,
                code: "runtime_source_not_served".to_owned(),
                detail: "the applied package no longer serves the native contract".to_owned(),
                acceptance_scope: None,
            });
            Ok(report)
        })
    }
}

/// Upgrades `harness` to V2 with `plan`, judged after an apply as `post`.
/// Returns the result and how many judgments followed the apply.
async fn upgrade_judged_after_apply(
    harness: &Harness,
    plan: EnsureReport,
    post: PostApply,
    accept: bool,
) -> (Result<UpgradeReport, Error>, Arc<AtomicUsize>) {
    let judged_after_apply = Arc::new(AtomicUsize::new(0));
    harness.runtimes.plan(plan);
    let result = harness
        .engine()
        .with_adoption_preflight(Box::new(PostApplyPreflight {
            runtimes: harness.runtimes.clone(),
            post,
            judged_after_apply: Arc::clone(&judged_after_apply),
        }))
        .with_runtime_loss_accepted(accept)
        .upgrade(&harness.staged(V2), V2)
        .await;
    (result, judged_after_apply)
}

#[tokio::test]
async fn a_session_at_risk_after_the_apply_restores_the_backup_and_refuses() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);

    let (result, judged) =
        upgrade_judged_after_apply(&harness, installing_plan(), PostApply::AtRisk, false).await;
    let error = result.expect_err("the post-apply verdict refuses");
    assert_eq!(
        error.code(),
        "service_upgrade_sessions_at_risk",
        "{error:?}"
    );
    assert_eq!(
        judged.load(Ordering::SeqCst),
        1,
        "the apply was judged once more"
    );
    assert_eq!(
        runtime_state(&harness),
        (None, "profile before".to_owned()),
        "the apply is rolled back"
    );
    harness.assert_installed(V1);
}

#[tokio::test]
async fn a_session_at_risk_after_the_apply_proceeds_with_accepted_runtime_loss() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);

    let (result, _) =
        upgrade_judged_after_apply(&harness, installing_plan(), PostApply::AtRisk, true).await;
    let report = result.expect("the accepted loss proceeds");
    assert!(report.accepted_runtime_loss);
    assert_eq!(
        runtime_state(&harness),
        (
            Some(APPLIED_REGISTRY.to_owned()),
            "applied profile".to_owned()
        )
    );
    harness.assert_installed(V2);
}

#[tokio::test]
async fn a_post_apply_preflight_that_cannot_run_restores_the_backup_and_fails() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);

    let (result, _) =
        upgrade_judged_after_apply(&harness, installing_plan(), PostApply::CannotRun, true).await;
    let error = result.expect_err("a preflight that cannot run is never nothing at risk");
    assert_eq!(
        error.code(),
        "service_upgrade_preflight_failed",
        "{error:?}"
    );
    assert_eq!(runtime_state(&harness), (None, "profile before".to_owned()));
    harness.assert_installed(V1);
}

/// Records the integration homes an upgrade asked to reinstall, and the
/// search path of each reinstall.
#[derive(Debug, Clone, Default)]
struct RecordingIntegrations(Arc<Mutex<Vec<IntegrationAction>>>, Arc<Mutex<Vec<String>>>);

impl IntegrationStep for RecordingIntegrations {
    fn reinstall<'a>(
        &'a self,
        _env: &'a IntegrationEnv,
        actions: &'a [IntegrationAction],
        search_path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<IntegrationReinstall>> + Send + 'a>> {
        self.0
            .lock()
            .expect("record lock")
            .extend(actions.iter().cloned());
        self.1
            .lock()
            .expect("search path lock")
            .push(search_path.to_owned());
        Box::pin(async { Vec::new() })
    }
}

#[tokio::test]
async fn an_apply_whose_daemon_exit_is_not_confirmed_keeps_the_transaction() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    harness.runtimes.plan(installing_plan());
    harness.runtimes.script().unconfirmed_apply = true;

    let error = harness
        .upgrade(V2)
        .await
        .expect_err("the exit is not confirmed");
    assert_eq!(
        error.code(),
        "service_upgrade_runtimes_unconfirmed",
        "{error:?}"
    );
    // Nothing is restored and no previous daemon starts while the process may
    // still write: the transaction stays pending with its backup and stopped daemon.
    assert!(harness.pending().is_some(), "the transaction is kept");
    assert!(
        harness
            .context
            .paths()
            .state_dir
            .join(record::RUNTIME_BACKUP_DIR_NAME)
            .exists(),
        "the backup is kept for the next command"
    );
    assert_eq!(
        harness.fake.daemon_version(),
        None,
        "no daemon was restarted"
    );
}

/// Holds the daemon data authority lock of `harness`, as a running daemon does.
fn hold_data_authority(harness: &Harness) -> pohunek_platform::filesystem::AdvisoryLock {
    let path = harness.context.paths().data_authority_lock_path();
    let parent = path.parent().expect("data dir").to_path_buf();
    std::fs::create_dir_all(&parent).expect("data dir");
    std::fs::set_permissions(&parent, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .expect("data dir mode");
    let directory = pohunek_platform::filesystem::TrustedDir::open_absolute(&parent, 0o700)
        .expect("open data dir");
    directory
        .acquire_lock(path.file_name().expect("lock name"), 0o600)
        .expect("hold the data authority lock")
}

#[tokio::test]
async fn a_held_data_authority_lock_keeps_a_pending_rollback_and_its_backup() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    harness.runtimes.plan(installing_plan());
    harness.runtimes.script().fail_apply = true;
    let authority = hold_data_authority(&harness);

    // The apply fails and the rollback cannot restore while the lock is held.
    let error = harness.upgrade(V2).await.expect_err("the rollback is held");
    assert_eq!(
        error.code(),
        "service_upgrade_runtimes_unconfirmed",
        "{error:?}"
    );
    assert!(harness.pending().is_some(), "the transaction is kept");
    assert!(
        harness
            .context
            .paths()
            .state_dir
            .join(record::RUNTIME_BACKUP_DIR_NAME)
            .exists(),
        "the backup is kept"
    );
    assert_eq!(
        runtime_state(&harness).0.as_deref(),
        Some(APPLIED_REGISTRY),
        "nothing was restored while the lock was held"
    );
    assert_eq!(
        harness.fake.daemon_version(),
        None,
        "no previous daemon started"
    );

    // A later command, with the lock free, finishes the pending rollback.
    drop(authority);
    let report = harness
        .upgrade(V1)
        .await
        .expect("the pending rollback runs");
    assert!(report.unchanged);
    assert_eq!(runtime_state(&harness), (None, "profile before".to_owned()));
    harness.assert_installed(V1);
}

/// A pending rollback that restores a runtime backup resolves the job's `PATH`
/// before it stops the daemon, so a drop-in the removed unit could not
/// reproduce is refused with the daemon still up.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_rollback_with_a_runtime_backup_resolves_the_path_before_it_stops_the_daemon() {
    let mut harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    harness.runtimes.plan(installing_plan());
    harness.runtimes.script().fail_apply = true;
    let authority = hold_data_authority(&harness);
    harness.upgrade(V2).await.expect_err("the rollback is held");
    drop(authority);

    let (_keep, unit) = unit_with_files(
        b"[Service]\nExecStart=/bin/true\n",
        &[b"[Service]\nEnvironment=PATH=%h/bin\n"],
        &["PATH=/expanded/bin"],
    );
    harness.context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(UnitManager { unit: Ok(unit) }));
    let uninstalls = harness.fake.world().uninstalls;
    let error = harness
        .upgrade(V1)
        .await
        .expect_err("a specifier is not reproducible");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    assert_eq!(
        harness.fake.world().uninstalls,
        uninstalls,
        "nothing stopped"
    );
    assert!(harness.pending().is_some(), "the transaction is kept");
}

#[tokio::test]
async fn a_catalog_record_is_a_plan_of_its_own_and_is_applied_with_a_stop_and_backup() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    let mut plan = runtime_report(
        true,
        vec![package(PackageStatus::Present)],
        Vec::new(),
        Vec::new(),
    );
    plan.catalog = CatalogStatus::Record;
    harness.runtimes.plan(plan);

    harness.upgrade(V2).await.expect("upgrade");
    assert_eq!(
        harness.runtimes.calls(),
        ["plan", "apply registered=false", "plan"],
        "a catalog-only plan stops the daemon and applies"
    );
    assert_eq!(
        runtime_state(&harness),
        (
            Some(APPLIED_REGISTRY.to_owned()),
            "applied profile".to_owned()
        ),
        "the apply ran and the apply's state is in place"
    );
    harness.assert_installed(V2);
}

#[tokio::test]
async fn each_listed_hook_home_is_reinstalled_in_its_own_profile() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    let mut plan = runtime_report(
        true,
        vec![package(PackageStatus::Present)],
        Vec::new(),
        Vec::new(),
    );
    plan.integrations = vec![
        IntegrationAction {
            runtime: "claude".to_owned(),
            profile: None,
        },
        IntegrationAction {
            runtime: "claude".to_owned(),
            profile: Some("work".to_owned()),
        },
    ];
    harness.runtimes.plan(plan);
    let recorded = RecordingIntegrations::default();

    harness
        .engine()
        .with_integration_step(Box::new(recorded.clone()))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade");
    let homes = recorded.0.lock().expect("record lock").clone();
    assert_eq!(
        homes
            .iter()
            .map(|home| home.profile.clone())
            .collect::<Vec<_>>(),
        [None, Some("work".to_owned())],
        "the profile home is reinstalled in its profile"
    );
}

/// The integrations an upgrade reinstalls, the Hermes plugin among them, get
/// the daemon job's `PATH`, the one the runtime plan ran with, and never the
/// `PATH` of the CLI that runs the upgrade.
#[tokio::test]
async fn an_upgrade_reinstalls_the_integrations_with_the_job_path() {
    let mut harness = Harness::new();
    set_job_path(&mut harness, CURRENT_PATH);
    harness.install(V1).await.expect("install");
    harness.runtimes.script().search_paths.clear();
    assert_ne!(
        std::env::var_os("PATH").as_deref(),
        Some(std::ffi::OsStr::new(CURRENT_PATH)),
        "the CLI's PATH must differ from the job's"
    );
    let recorded = RecordingIntegrations::default();

    harness
        .engine()
        .with_integration_step(Box::new(recorded.clone()))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade");

    let reinstalled = recorded.1.lock().expect("search path lock").clone();
    assert_eq!(
        reinstalled,
        [CURRENT_PATH],
        "one reinstall, with the job's PATH"
    );
    let planned = harness.runtimes.script().search_paths.clone();
    assert_eq!(
        planned.last().map(String::as_str),
        Some(CURRENT_PATH),
        "the plan the reinstall acts on ran with the same PATH: {planned:?}"
    );
}

#[tokio::test]
async fn an_empty_plan_needs_no_post_apply_judgment() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.runtimes.script().calls.clear();
    let plan = runtime_report(
        true,
        vec![package(PackageStatus::Present)],
        Vec::new(),
        Vec::new(),
    );

    let (result, judged) =
        upgrade_judged_after_apply(&harness, plan, PostApply::AtRisk, false).await;
    result.expect("nothing was applied, so nothing is judged again");
    assert_eq!(
        judged.load(Ordering::SeqCst),
        0,
        "no apply, no judgment after it"
    );
}

#[tokio::test]
async fn an_empty_runtime_plan_applies_nothing() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    // The install runs the runtime step too; the calls under test start here.
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    harness.runtimes.plan(runtime_report(
        true,
        vec![package(PackageStatus::Present)],
        Vec::new(),
        Vec::new(),
    ));

    harness.upgrade(V2).await.expect("upgrade");
    assert_eq!(harness.runtimes.calls(), ["plan", "plan"]);
    let (before, after) = harness.runtimes.last_two_removal_counts();
    assert_eq!(before, after, "the daemon is not removed for an empty plan");
    assert_eq!(runtime_state(&harness), (None, "profile before".to_owned()));
    harness.assert_installed(V2);
}

#[tokio::test]
async fn an_upgrade_interrupted_after_its_runtime_apply_resumes_without_applying_again() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    // The install runs the runtime step too; the calls under test start here.
    harness.runtimes.script().calls.clear();
    seed_runtime_state(&harness);
    harness.runtimes.plan(installing_plan());
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Runtimes);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted");
    assert_eq!(harness.runtimes.calls().len(), 2, "plan and apply ran once");

    let report = harness.upgrade(V2).await.expect("resume");
    assert!(report.resumed);
    assert_eq!(
        harness.runtimes.calls()[2..],
        ["plan".to_owned()],
        "the resumed transaction does not apply again"
    );
    assert_eq!(
        runtime_state(&harness),
        (
            Some(APPLIED_REGISTRY.to_owned()),
            "applied profile".to_owned()
        )
    );
    harness.assert_installed(V2);
}

/// Asserts that `error` keeps a pending install of `V1` at `step`.
fn assert_incomplete(harness: &Harness, error: &Error, step: Step) {
    let Error::InstallIncomplete {
        version,
        prefix,
        step: at,
        ..
    } = error
    else {
        panic!("unexpected error {error:?}");
    };
    assert_eq!(version, V1);
    assert_eq!(prefix, &harness.prefix());
    assert_eq!(*at, step.as_str());
    assert_eq!(error.code(), "service_install_incomplete");
    let hint = error.hint().expect("hint");
    assert!(hint.contains("pohunek service install"), "{hint}");
    assert!(hint.contains("pohunek service uninstall"), "{hint}");
    assert_eq!(
        harness.pending().expect("record kept").step,
        step,
        "the record stays for a resume or an uninstall"
    );
    assert_eq!(
        harness
            .config()
            .expect("service.toml kept")
            .active_version(),
        V1
    );
}

/// The install fails closed when the unit verifier is absent or rejects the
/// units: nothing is registered and no installation is left behind.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_install_fails_closed_when_the_unit_verifier_is_missing_or_rejects() {
    let harness = Harness::new();
    let missing = harness.root.join("no-such-verifier");
    let error = harness
        .engine()
        .with_unit_verifier(UnitVerifier::default().with_program(missing))
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("a missing verifier refuses the install");
    assert!(matches!(error, Error::VerifierMissing), "{error:?}");
    assert!(harness.fake.world().registered.is_none());
    harness.assert_clean();

    let error = harness
        .engine()
        .with_unit_verifier(UnitVerifier::default().with_program("/bin/false"))
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("a rejecting verifier refuses the install");
    assert!(matches!(error, Error::UnitVerification { .. }), "{error:?}");
    assert!(harness.fake.world().registered.is_none());
    harness.assert_clean();
}

#[tokio::test]
async fn a_failed_registration_keeps_the_install_pending_until_uninstall() {
    let harness = Harness::new();
    harness.fake.world().registration = Registration::Fails;
    let error = harness.install(V1).await.expect_err("injected failure");
    assert_incomplete(&harness, &error, Step::Registering);
    let Error::InstallIncomplete { original, .. } = &error else {
        unreachable!("checked above");
    };
    assert!(
        matches!(**original, Error::Supervisor { .. }),
        "{original:?}"
    );
    assert!(harness.fake.world().registered.is_none());

    harness.fake.world().registration = Registration::Succeeds;
    let report = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall");
    assert_eq!(report.rolled_back, None);
    harness.assert_clean();
}

#[tokio::test]
async fn a_registration_that_fails_after_registering_keeps_the_live_daemon_for_uninstall() {
    let harness = Harness::new();
    harness.fake.world().registration = Registration::FailsAfterRegistering;
    harness.fake.seed(
        vec![session(SESSION, Some("review"))],
        vec![worker(worker_id(SESSION), ServiceState::Running)],
    );
    let error = harness.install(V1).await.expect_err("ambiguous failure");
    harness.fake.world().registration = Registration::Succeeds;
    assert_incomplete(&harness, &error, Step::Registering);
    // Nothing was rolled back: the job the call really registered still runs
    // with its live session.
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
    assert!(harness.fake.world().running);
    assert_eq!(
        harness.fake.world().sessions[0].state,
        SessionState::Running
    );
    assert!(!harness.layout().bin_dir().join(CLI_NAME).exists());

    let error = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("refused");
    assert!(matches!(error, Error::LiveSessions { .. }), "{error:?}");
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
    assert!(harness.fake.world().running);
    assert!(
        harness.config().is_some(),
        "service.toml survives the refusal"
    );
    assert_eq!(
        harness.pending().expect("record stays pending").step,
        Step::Registering
    );

    let report = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect("uninstall with --stop-sessions");
    assert_eq!(report.stopped_sessions, [SESSION]);
    harness.assert_clean();
}

#[tokio::test]
async fn a_registration_that_fails_after_registering_is_finished_by_rerunning_install() {
    let harness = Harness::new();
    harness.fake.world().registration = Registration::FailsAfterRegistering;
    let error = harness.install(V1).await.expect_err("ambiguous failure");
    harness.fake.world().registration = Registration::Succeeds;
    assert_incomplete(&harness, &error, Step::Registering);

    let report = harness.install(V1).await.expect("resume");
    assert!(report.resumed);
    assert_eq!(report.rolled_back, None);
    harness.assert_installed(V1);
    assert_eq!(
        harness.fake.world().installs,
        1,
        "the resume replaces the registered job instead of installing twice"
    );
}

#[tokio::test]
async fn a_pending_install_past_registration_is_never_rolled_back() {
    let harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    let record = harness.pending().expect("record");

    let error = harness
        .engine()
        .rollback(&record)
        .await
        .expect_err("invariant");
    assert!(matches!(error, Error::Record { .. }), "{error:?}");
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
    assert!(harness.fake.world().running);
    assert!(harness.config().is_some());
}

#[tokio::test]
async fn readiness_requires_the_supervised_job_to_serve_the_socket() {
    for (processless, expected) in [
        (
            false,
            format!(
                "daemon socket is served by pid {FOREIGN_PID}; the supervised job runs pid {DAEMON_PID}"
            ),
        ),
        (
            true,
            format!(
                "daemon socket is served by pid {FOREIGN_PID}; the supervised job has no process"
            ),
        ),
    ] {
        let harness = Harness::new();
        {
            let mut world = harness.fake.world();
            world.socket_pid = Some(FOREIGN_PID);
            world.processless = processless;
        };
        let error = harness.install(V1).await.expect_err("not ready");
        assert_incomplete(&harness, &error, Step::Registered);
        let Error::InstallIncomplete { original, .. } = &error else {
            unreachable!("checked above");
        };
        let Error::DaemonNotReady { version, last, .. } = &**original else {
            panic!("unexpected error {original:?}");
        };
        assert_eq!(version, V1);
        let last = last.as_deref().expect("last observation");
        assert!(last.contains(&expected), "{last}");
        assert!(
            !harness.layout().bin_dir().join(CLI_NAME).exists(),
            "the CLI copy is not installed"
        );

        // Once the supervised job serves the socket itself, the resume is ready.
        {
            let mut world = harness.fake.world();
            world.socket_pid = None;
            world.processless = false;
        };
        let report = harness.install(V1).await.expect("resume");
        assert!(report.resumed);
        harness.assert_installed(V1);
    }
}

#[tokio::test]
async fn an_upgrade_is_not_ready_while_another_process_serves_the_socket() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.world().socket_pid = Some(FOREIGN_PID);
    let error = harness.upgrade(V2).await.expect_err("not ready");
    // The rollback waits for the restored daemon the same way, so it cannot
    // finish while the foreign process holds the socket either.
    let Error::RollbackFailed { original, rollback } = &error else {
        panic!("unexpected error {error:?}");
    };
    assert!(
        matches!(&**original, Error::DaemonNotReady { version, .. } if version == V2),
        "{original:?}"
    );
    assert!(
        matches!(&**rollback, Error::DaemonNotReady { version, .. } if version == V1),
        "{rollback:?}"
    );
    let cli = std::fs::read(harness.layout().bin_dir().join(CLI_NAME)).expect("installed CLI");
    let staged = std::fs::read(harness.staged(V1).join(CLI_NAME)).expect("staged CLI");
    assert_eq!(cli, staged, "the V2 CLI copy is not installed");

    // Once the supervised job serves the socket itself, asking for the
    // active version finishes the rollback, and a fresh upgrade is ready.
    harness.fake.world().socket_pid = None;
    let report = harness.upgrade(V1).await.expect("finish the rollback");
    assert!(report.unchanged);
    assert!(report.rolled_back.is_some());
    harness.assert_installed(V1);
    harness.upgrade(V2).await.expect("upgrade");
    harness.assert_installed(V2);
}

/// Fails an upgrade at readiness and the stopped daemon's restoration.
async fn fail_upgrade_and_its_rollback(harness: &Harness) {
    {
        let mut world = harness.fake.world();
        world.silent_version = Some(V2.to_owned());
        world.refused_install = Some(V1.to_owned());
    };
    let error = harness.upgrade(V2).await.expect_err("not ready");
    let Error::RollbackFailed { original, rollback } = &error else {
        panic!("unexpected error {error:?}");
    };
    assert!(
        matches!(&**original, Error::DaemonNotReady { version, .. } if version == V2),
        "{original:?}"
    );
    assert!(
        matches!(&**rollback, Error::Supervisor { .. }),
        "{rollback:?}"
    );
    let pending = harness.pending().expect("record kept");
    assert_eq!(pending.step, Step::Registered);
    assert!(pending.rolling_back, "the begun rollback is journaled");
    assert_eq!(
        harness.config().expect("config").active_version(),
        V1,
        "the rollback restored service.toml before it failed"
    );
    assert!(
        harness.fake.daemon_version().is_none(),
        "new daemon is stopped"
    );
    let mut world = harness.fake.world();
    world.silent_version = None;
    world.refused_install = None;
}

#[tokio::test]
async fn an_upgrade_whose_rollback_failed_is_rolled_back_before_the_same_upgrade_reruns() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    fail_upgrade_and_its_rollback(&harness).await;

    // Resuming would skip the `config` step whose effect the rollback undid,
    // registering the V2 daemon with a service.toml that names V1.
    let report = harness.upgrade(V2).await.expect("upgrade reruns");
    assert!(!report.resumed, "a rolling-back record is never resumed");
    assert_eq!(report.from_version, V1);
    assert_eq!(
        report.rolled_back.map(|pending| pending.step),
        Some(Step::Registered.as_str())
    );
    harness.assert_installed(V2);
    assert_eq!(
        layout::installed_versions(&harness.layout()).expect("versions"),
        [V2],
        "service.toml names the only remaining version"
    );
}

#[tokio::test]
async fn an_upgrade_whose_rollback_failed_is_finished_by_asking_for_the_previous_version() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    fail_upgrade_and_its_rollback(&harness).await;

    let report = harness.upgrade(V1).await.expect("finish the rollback");
    assert!(report.unchanged);
    assert!(report.rolled_back.is_some());
    harness.assert_installed(V1);
    assert_eq!(
        layout::installed_versions(&harness.layout()).expect("versions"),
        [V1],
        "the version the rollback abandoned is removed"
    );
}

#[tokio::test]
async fn an_install_whose_rollback_died_is_started_over_instead_of_resumed() {
    let harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Config);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    // A rollback that died right after removing service.toml.
    let mut record = harness.pending().expect("record left behind");
    record.rolling_back = true;
    harness.engine().store.save(&record).expect("save record");
    assert!(remove_config(&harness.context.config_path()).expect("remove service.toml"));

    let report = harness.install(V1).await.expect("install starts over");
    assert!(!report.resumed, "a rolling-back record is never resumed");
    assert_eq!(
        report.rolled_back.map(|pending| pending.step),
        Some(Step::Config.as_str())
    );
    harness.assert_installed(V1);
}

#[tokio::test]
async fn a_daemon_that_never_answers_rolls_the_upgrade_back_to_the_previous_version() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.world().silent_version = Some(V2.to_owned());
    let error = harness.upgrade(V2).await.expect_err("not ready");
    assert!(
        matches!(&error, Error::DaemonNotReady { version, .. } if version == V2),
        "{error:?}"
    );
    harness.fake.world().silent_version = None;
    let config = harness.config().expect("config restored");
    assert_eq!(config.active_version(), V1);
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
    assert_eq!(
        layout::installed_versions(&harness.layout()).expect("versions"),
        [V1]
    );
    assert_eq!(harness.pending(), None);
}

#[tokio::test]
async fn uninstall_refuses_live_sessions_and_changes_nothing() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.seed(
        vec![session(SESSION, Some("review")), session("s-7", None)],
        vec![
            worker(worker_id(SESSION), ServiceState::Running),
            worker(worker_id("s-9"), ServiceState::Running),
        ],
    );
    let error = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("refused");
    let Error::LiveSessions { sessions, workers } = &error else {
        panic!("unexpected error {error:?}");
    };
    assert_eq!(
        sessions.iter().map(ToString::to_string).collect::<Vec<_>>(),
        [format!("{SESSION} (review)"), "s-7".to_owned()]
    );
    assert_eq!(workers, &[worker_id("s-9").to_string()]);
    harness.assert_installed(V1);
    assert_eq!(harness.fake.world().workers.len(), 2);
}

#[tokio::test]
async fn uninstall_with_stop_sessions_stops_retires_and_removes_everything() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let worker_exe = harness.layout().worker_executable(V1).expect("worker");
    // A lingering worker whose runtime already ended is retired, not stopped.
    write_journal(
        harness.context.paths(),
        "s-5",
        "w-5",
        &worker_exe,
        RuntimePhase::Terminal,
        1,
    );
    harness.fake.seed(
        vec![session(SESSION, Some("review"))],
        vec![
            worker(worker_id(SESSION), ServiceState::Running),
            worker(worker_id("s-5"), ServiceState::Running),
        ],
    );
    let report = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect("uninstall");
    assert_eq!(report.stopped_sessions, [SESSION]);
    assert_eq!(report.retired_workers, [worker_id("s-5").to_string()]);
    harness.assert_clean();
    assert!(
        harness
            .context
            .paths()
            .worker_state_root()
            .join("s-5")
            .is_dir(),
        "journals are kept without --purge"
    );
}

#[tokio::test]
async fn stop_sessions_times_out_while_a_worker_stays_live() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let worker_exe = harness.layout().worker_executable(V1).expect("worker");
    write_live_journal(harness.context.paths(), "s-6", "w-6", &worker_exe);
    let error = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect_err("worker never stops");
    assert!(matches!(error, Error::StopTimeout { .. }), "{error:?}");
    harness.assert_installed(V1);
}

#[tokio::test]
async fn uninstall_starts_a_stopped_daemon_to_list_sessions() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.world().running = false;
    let report = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall");
    assert!(report.started_daemon);
    harness.assert_clean();
}

#[tokio::test]
async fn uninstall_refuses_live_sessions_while_a_pending_install_has_registered_its_daemon() {
    let harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registering);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    // The interrupted register call landed before the crash: the daemon job
    // exists and runs the version `service.toml` names.
    let config = harness.config().expect("service.toml exists");
    let definition = daemon_definition(&harness.context, &config).expect("definition");
    harness
        .backend
        .daemon()
        .install(&definition)
        .await
        .expect("register landed");
    harness.fake.seed(
        vec![session(SESSION, Some("review"))],
        vec![worker(worker_id(SESSION), ServiceState::Running)],
    );

    let error = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("refused");
    let Error::LiveSessions { sessions, .. } = &error else {
        panic!("unexpected error {error:?}");
    };
    assert_eq!(sessions.len(), 1);
    // Nothing was rolled back: the daemon job, `service.toml`, and the
    // version directory all survive for the session-checked rerun.
    assert_eq!(
        harness
            .config()
            .expect("service.toml survives")
            .active_version(),
        V1
    );
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
    assert!(harness.fake.world().running);
    assert!(harness
        .layout()
        .daemon_executable(V1)
        .expect("daemon path")
        .is_file());
    assert_eq!(
        harness.pending().expect("record stays pending").step,
        Step::Registering
    );

    let report = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect("uninstall with --stop-sessions");
    assert_eq!(report.stopped_sessions, [SESSION]);
    harness.assert_clean();
}

#[tokio::test]
async fn uninstall_of_an_upgrade_interrupted_immediately_before_replacement_stops_sessions_through_the_previous_daemon(
) {
    // The interruption lands after `service.toml` already names the new
    // version but before the service-manager register call: the previous
    // daemon still runs and owns the store readership, and doing nothing to
    // it is the only safe removal.
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registering);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted before the service-manager call");
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
    assert!(harness.fake.world().running);
    assert_eq!(harness.fake.world().replaces, 0, "no replacement call");
    let config = harness.config().expect("service.toml exists");
    assert_eq!(
        config.active_version(),
        V2,
        "service.toml names the new version"
    );
    assert_eq!(
        harness.pending().expect("record stays pending").step,
        Step::Registering
    );
    harness.fake.seed(
        vec![session(SESSION, Some("review"))],
        vec![worker(worker_id(SESSION), ServiceState::Running)],
    );

    let error = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("refused");
    let Error::LiveSessions { sessions, .. } = &error else {
        panic!("unexpected error {error:?}");
    };
    assert_eq!(sessions.len(), 1);
    // Nothing was replaced or unlocked: the previous daemon keeps running,
    // `service.toml`, and the record all survive for the rerun.
    assert_eq!(harness.fake.world().replaces, 0);
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
    assert_eq!(
        harness
            .config()
            .expect("service.toml survives")
            .active_version(),
        V2
    );
    assert_eq!(
        harness.pending().expect("record stays pending").step,
        Step::Registering
    );

    let report = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect("uninstall with --stop-sessions");
    assert_eq!(report.stopped_sessions, [SESSION]);
    assert_eq!(
        harness.fake.world().replaces,
        0,
        "the previous daemon is never replaced"
    );
    harness.assert_clean();
}

#[tokio::test]
async fn uninstall_refuses_a_supervised_daemon_of_a_foreign_or_older_version_before_any_change() {
    // The job runs a daemon neither the configuration nor a pending
    // transaction proves belongs to this installation: the removal must
    // refuse before it asks for or stops a session. A completed install has
    // the daemon job registered and running its version.
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let config = harness.config().expect("service.toml exists");
    let definition = harness.fake.world().registered.clone().expect("definition");
    harness.fake.seed(
        vec![session(SESSION, Some("review"))],
        vec![worker(worker_id(SESSION), ServiceState::Running)],
    );
    for foreign in [OLDER, V3] {
        let foreign_config = with_version(&config, foreign).expect("with_version");
        harness.fake.world().registered =
            Some(daemon_definition(&harness.context, &foreign_config).expect("definition"));

        let error = harness
            .engine()
            .uninstall(UninstallOptions {
                stop_sessions: true,
                purge: false,
            })
            .await
            .expect_err("refused");
        let Error::SupervisedVersion { served, accepted } = &error else {
            panic!("unexpected error {error:?}");
        };
        assert_eq!(served, foreign);
        assert_eq!(*accepted, [V1]);
        assert_eq!(error.code(), "service_daemon_version_unexpected");
        // The refusal happened without touching a session: nothing stopped
        // and nothing was removed.
        assert_eq!(harness.fake.world().sessions.len(), 1);
        assert!(harness.fake.world().running);
        assert!(harness.config().is_some());
        assert_eq!(harness.fake.world().workers.len(), 1);
    }
    // The original supervised daemon still serves, and the uninstall
    // proceeds through it unchanged.
    harness.fake.world().registered = Some(definition);
    let report = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect("uninstall");
    assert_eq!(report.stopped_sessions, [SESSION]);
    harness.assert_clean();
}

#[tokio::test]
async fn uninstall_refuses_an_unexpected_supervised_version_even_without_live_sessions() {
    // No session and no worker may make the mismatch harmless: nothing here
    // proves the unexpected daemon belongs to this installation, so the
    // uninstall refuses before its first destructive call.
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let config = harness.config().expect("service.toml exists");
    let foreign = daemon_definition(
        &harness.context,
        &with_version(&config, V3).expect("with_version"),
    )
    .expect("definition");
    harness.fake.world().registered = Some(foreign);

    let error = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: true,
        })
        .await
        .expect_err("refused");
    let Error::SupervisedVersion { served, accepted } = &error else {
        panic!("unexpected error {error:?}");
    };
    assert_eq!(served, V3);
    assert_eq!(*accepted, [V1]);
    // Nothing was removed: the supervised job, the daemon it runs (through
    // `--purge` the durable metadata must also have survived), `service.toml`,
    // and the version directory all stay.
    assert!(harness.fake.world().running);
    assert!(harness.fake.world().registered.is_some());
    assert!(harness.fake.world().workers.is_empty());
    assert!(harness.fake.world().sessions.is_empty());
    assert!(harness.config().is_some());
    assert_eq!(
        layout::installed_versions(&harness.layout())
            .expect("versions")
            .len(),
        1
    );
    assert_eq!(
        harness.engine().store.load().expect("load record"),
        None,
        "the purge ran nowhere: the daemon job alone still serves the store"
    );
}

#[tokio::test]
async fn uninstall_does_not_attribute_a_foreign_socket_version_to_the_supervised_job() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.seed(
        vec![session(SESSION, Some("review"))],
        vec![worker(worker_id(SESSION), ServiceState::Running)],
    );
    {
        let mut world = harness.fake.world();
        world.socket_pid = Some(FOREIGN_PID);
        world.socket_version = Some(V3.to_owned());
    };

    let error = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: true,
        })
        .await
        .expect_err("foreign process does not prove a supervised version");
    let Error::DaemonNotReady { last, .. } = &error else {
        panic!("unexpected error {error:?}");
    };
    assert!(
        last.as_deref()
            .is_some_and(|detail| detail.contains(&format!("daemon socket is served by pid {FOREIGN_PID}; the supervised job runs pid {DAEMON_PID}"))),
        "{last:?}"
    );
    assert_eq!(harness.fake.world().sessions.len(), 1);
    assert_eq!(harness.fake.world().workers.len(), 1);
    assert!(harness.fake.world().running);
    assert_eq!(harness.config().expect("config").active_version(), V1);

    {
        let mut world = harness.fake.world();
        world.socket_pid = None;
        world.socket_version = None;
    };
    let report = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: true,
        })
        .await
        .expect("supervised daemon serves the socket");
    assert_eq!(report.stopped_sessions, [SESSION]);
    harness.assert_clean();
}

#[tokio::test]
async fn uninstall_of_a_pending_install_at_the_cli_step_removes_the_installed_cli() {
    let harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Cli);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    harness.fake.seed(
        vec![session(SESSION, None)],
        vec![worker(worker_id(SESSION), ServiceState::Running)],
    );
    let cli = harness.layout().bin_dir().join(CLI_NAME);
    assert!(cli.is_file(), "the interrupted install installed the CLI");

    let error = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("refused");
    let Error::LiveSessions { .. } = &error else {
        panic!("unexpected error {error:?}");
    };
    assert!(cli.is_file(), "the refusal removes nothing");

    let report = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect("uninstall with --stop-sessions");
    assert_eq!(report.stopped_sessions, [SESSION]);
    harness.assert_clean();
    assert!(!cli.exists(), "the installed CLI copy is removed");
}

#[tokio::test]
async fn uninstall_refuses_a_processless_unknown_worker_job_until_its_journal_proves_the_end() {
    // launchd reports a loaded job without a matching process as Unknown;
    // such a job may still be waiting to spawn, so a non-final or missing
    // journal never proves the runtime ended.
    for journal in [None, Some(RuntimePhase::Live)] {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        if let Some(phase) = journal {
            write_journal(
                harness.context.paths(),
                SESSION,
                "w-1",
                &harness.layout().worker_executable(V1).expect("worker"),
                phase,
                1,
            );
        }
        harness.fake.seed(
            Vec::new(),
            vec![worker(worker_id(SESSION), ServiceState::Unknown)],
        );

        let error = harness
            .engine()
            .uninstall(UninstallOptions::default())
            .await
            .expect_err("refused");
        let Error::LiveSessions { sessions, workers } = &error else {
            panic!("unexpected error {error:?}");
        };
        assert!(sessions.is_empty());
        assert_eq!(workers, &[worker_id(SESSION).to_string()]);
        harness.assert_installed(V1);
    }

    // A final journal is the authoritative proof: the job retires.
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    write_journal(
        harness.context.paths(),
        SESSION,
        "w-1",
        &harness.layout().worker_executable(V1).expect("worker"),
        RuntimePhase::Terminal,
        1,
    );
    harness.fake.seed(
        Vec::new(),
        vec![worker(worker_id(SESSION), ServiceState::Unknown)],
    );
    let report = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall");
    assert_eq!(report.retired_workers, [worker_id(SESSION).to_string()]);
    harness.assert_clean();
}

#[tokio::test]
async fn purge_removes_durable_metadata_but_never_worktrees() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let paths = harness.context.paths().clone();
    std::fs::create_dir_all(paths.data_dir.join("events")).expect("events");
    std::fs::create_dir_all(paths.data_dir.join("worktrees/repo")).expect("worktrees");
    std::fs::write(paths.data_dir.join("metadata.jsonl"), "{}\n").expect("store");
    std::fs::create_dir_all(paths.host_state_dir()).expect("host state");
    write_journal(
        &paths,
        "s-5",
        "w-5",
        Path::new("/elsewhere/pohunek-sessiond"),
        RuntimePhase::Terminal,
        1,
    );
    let report = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: false,
            purge: true,
        })
        .await
        .expect("uninstall");
    assert!(report.purged);
    assert!(!paths.data_dir.join("metadata.jsonl").exists());
    assert!(!paths.data_dir.join("events").exists());
    assert!(!paths.worker_state_root().exists());
    assert!(!paths.host_state_dir().exists());
    assert!(paths.data_dir.join("worktrees/repo").is_dir());
}

#[tokio::test]
async fn upgrade_keeps_a_version_a_live_journal_references() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let worker_exe = harness.layout().worker_executable(V1).expect("worker");
    write_live_journal(harness.context.paths(), SESSION, "w-1", &worker_exe);
    let report = harness.upgrade(V2).await.expect("upgrade");
    assert!(report.removed_versions.is_empty());
    let kept = report
        .kept_versions
        .iter()
        .find(|kept| kept.version == V1)
        .expect("the referenced version is kept");
    assert!(kept.reason.contains(SESSION), "{kept:?}");
    assert!(worker_exe.is_file(), "the live worker's binary survives");
}

/// A worker job registered from `executable` that has neither executed nor
/// written a journal yet.
fn pending_worker(session: &str, state: ServiceState, executable: PathBuf) -> ServiceObservation {
    ServiceObservation {
        definition: Some(pohunek_platform::supervisor::DefinitionFacts {
            executable,
            arguments: Vec::new(),
        }),
        ..worker(worker_id(session), state)
    }
}

#[tokio::test]
async fn upgrade_keeps_a_version_a_registered_worker_job_will_execute() {
    for state in [ServiceState::Starting, ServiceState::Unknown] {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let worker_exe = harness.layout().worker_executable(V1).expect("worker");
        harness.fake.seed(
            Vec::new(),
            vec![pending_worker(SESSION, state, worker_exe.clone())],
        );

        let report = harness.upgrade(V2).await.expect("upgrade");
        assert!(report.removed_versions.is_empty(), "{state:?}");
        let kept = report
            .kept_versions
            .iter()
            .find(|kept| kept.version == V1)
            .expect("the job's version is kept");
        assert!(
            kept.reason.contains(&worker_id(SESSION).to_string()),
            "{state:?}: {kept:?}"
        );
        assert!(worker_exe.is_file(), "{state:?}: the job can still exec");
    }
}

#[tokio::test]
async fn rerunning_the_active_upgrade_removes_a_version_whose_worker_job_ended() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let worker_exe = harness.layout().worker_executable(V1).expect("worker");
    harness.fake.seed(
        Vec::new(),
        vec![pending_worker(
            SESSION,
            ServiceState::Starting,
            worker_exe.clone(),
        )],
    );
    let upgraded = harness.upgrade(V2).await.expect("upgrade");
    assert!(upgraded.kept_versions.iter().any(|kept| kept.version == V1));

    // While the job is still registered, the rerun keeps its version too.
    let restarts = harness.fake.world().replaces;
    let still_kept = harness.upgrade(V2).await.expect("repeat upgrade");
    assert!(still_kept.unchanged);
    assert!(still_kept.removed_versions.is_empty());
    assert!(still_kept
        .kept_versions
        .iter()
        .any(|kept| kept.version == V1));
    assert!(worker_exe.is_file());

    harness.fake.seed(Vec::new(), Vec::new());
    let collected = harness.upgrade(V2).await.expect("repeat upgrade");
    assert!(collected.unchanged);
    assert_eq!(collected.removed_versions, [V1]);
    assert!(
        collected
            .kept_versions
            .iter()
            .all(|kept| kept.version != V1),
        "{collected:?}"
    );
    assert_eq!(collected.gc_error, None);
    assert!(!worker_exe.exists(), "the unreferenced version is removed");
    assert_eq!(
        harness.fake.world().replaces,
        restarts,
        "an unchanged upgrade never restarts the daemon"
    );
    harness.assert_installed(V2);
}

#[tokio::test]
async fn upgrade_keeps_every_version_when_worker_jobs_cannot_be_discovered() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.world().fail_discover = true;

    let report = harness.upgrade(V2).await.expect("upgrade");
    assert!(report.removed_versions.is_empty());
    let kept = report
        .kept_versions
        .iter()
        .find(|kept| kept.version == V1)
        .expect("the old version is kept");
    assert!(kept.reason.contains("injected failure"), "{kept:?}");
    harness.fake.world().fail_discover = false;
    harness.assert_installed(V2);
}

#[tokio::test]
async fn upgrade_keeps_every_version_when_worker_discovery_is_incomplete() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let worker_exe = harness.layout().worker_executable(V1).expect("worker");
    // A registered not-yet-spawned worker whose observation races is omitted
    // from the lenient discovery; the strict one must refuse the result so
    // GC keeps the version the job may still execute.
    harness.fake.world().raced_workers.push(pending_worker(
        SESSION,
        ServiceState::Starting,
        worker_exe.clone(),
    ));

    let report = harness.upgrade(V2).await.expect("upgrade");
    assert!(report.removed_versions.is_empty());
    let kept = report
        .kept_versions
        .iter()
        .find(|kept| kept.version == V1)
        .expect("the raced job's version is kept");
    assert!(kept.reason.contains("changed during"), "{kept:?}");
    assert!(worker_exe.is_file(), "the job can still exec");
}

#[tokio::test]
async fn upgrade_discovers_workers_again_when_a_job_is_unloaded_during_the_inspection() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.fake.world().transient_races = 2;

    let report = harness.upgrade(V2).await.expect("upgrade");
    assert_eq!(harness.fake.world().transient_races, 0);
    assert_eq!(report.removed_versions, [V1]);
    harness.assert_installed(V2);
}

#[tokio::test]
async fn a_rolled_back_upgrade_keeps_a_version_a_worker_job_registered_from() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let worker_exe = harness.layout().worker_executable(V2).expect("worker");
    harness.fake.seed(
        Vec::new(),
        vec![pending_worker(
            SESSION,
            ServiceState::Starting,
            worker_exe.clone(),
        )],
    );
    harness.fake.world().silent_version = Some(V2.to_owned());

    harness.upgrade(V2).await.expect_err("not ready");
    harness.fake.world().silent_version = None;
    assert_eq!(harness.config().expect("config").active_version(), V1);
    assert_eq!(
        layout::installed_versions(&harness.layout()).expect("versions"),
        [V1, V2],
        "the version the job execs survives the rollback"
    );
    assert!(worker_exe.is_file());
}

#[tokio::test]
async fn uninstall_removes_a_version_whose_non_final_journal_names_an_exited_worker() {
    for purge in [false, true] {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let worker_exe = harness.layout().worker_executable(V1).expect("worker");
        write_journal(
            harness.context.paths(),
            SESSION,
            "w-1",
            &worker_exe,
            RuntimePhase::Live,
            1,
        );

        let report = harness
            .engine()
            .uninstall(UninstallOptions {
                stop_sessions: false,
                purge,
            })
            .await
            .expect("uninstall");
        assert!(report.kept_versions.is_empty(), "purge={purge}: {report:?}");
        harness.assert_clean();
    }
}

#[tokio::test]
async fn a_failed_uninstall_keeps_service_toml_so_a_rerun_finishes_it() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    // The versions directory is verified before anything is removed, so the
    // CLI copy's directory is what fails once the daemon job is gone.
    let bin = harness.layout().bin_dir();
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o777))
        .expect("loosen bin dir");

    let error = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("an untrusted bin directory stops the removal");
    assert!(
        matches!(error, Error::UntrustedDirectory { .. }),
        "{error:?}"
    );
    assert!(
        harness.config().is_some(),
        "service.toml survives the failure"
    );
    assert!(
        harness.fake.world().registered.is_none(),
        "the daemon job is gone"
    );

    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .expect("restore bin dir");
    let report = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("rerun finishes the uninstall");
    assert!(report.removed.contains(&harness.context.config_path()));
    harness.assert_clean();
    assert!(!harness.layout().bin_dir().join(CLI_NAME).exists());
}

#[tokio::test]
async fn an_uninstall_that_died_before_removing_service_toml_is_finished_by_a_rerun() {
    let harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    // The state an uninstall of that pending install leaves when it dies
    // right after clearing the record: the daemon job and the version
    // directory are gone, and only `service.toml` remains.
    harness
        .backend
        .daemon()
        .uninstall()
        .await
        .expect("daemon removed");
    assert!(layout::remove_version(&harness.layout(), V1).expect("remove version"));
    harness.engine().store.clear().expect("record cleared");
    assert!(harness.config().is_some());

    let report = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("rerun finishes the uninstall");
    assert!(report.removed.contains(&harness.context.config_path()));
    harness.assert_clean();
    harness.install(V1).await.expect("reinstall");
    harness.assert_installed(V1);
}

#[tokio::test]
async fn a_registered_pending_install_without_service_toml_is_uninstalled_through_the_session_check(
) {
    let harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    std::fs::remove_file(harness.context.config_path()).expect("remove service.toml");
    harness.fake.seed(
        vec![session(SESSION, None)],
        vec![worker(worker_id(SESSION), ServiceState::Running)],
    );

    // The daemon the record registered may own live sessions, so the missing
    // `service.toml` does not make the installation absent.
    let error = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("live sessions refuse the uninstall");
    assert!(matches!(error, Error::LiveSessions { .. }), "{error:?}");
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
    assert!(harness.pending().is_some(), "the record stays pending");

    let report = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect("uninstall with --stop-sessions");
    assert_eq!(report.stopped_sessions, [SESSION]);
    harness.assert_clean();
    harness.install(V1).await.expect("reinstall");
    harness.upgrade(V2).await.expect("upgrade");
    harness.assert_installed(V2);
}

#[tokio::test]
async fn an_outdated_journal_blocks_uninstall_only_while_its_worker_may_run() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let (pid, start) = own_identity();
    let journal = write_outdated_journal(
        harness.context.paths(),
        SESSION,
        "w-1",
        "live",
        Some((pid, &start)),
    );

    for stop_sessions in [false, true] {
        let error = harness
            .engine()
            .uninstall(UninstallOptions {
                stop_sessions,
                purge: false,
            })
            .await
            .expect_err("a possibly live outdated worker blocks");
        let Error::OutdatedJournals { workers } = &error else {
            panic!("unexpected error {error:?}");
        };
        assert_eq!(
            workers,
            &[OutdatedWorker {
                path: journal.clone(),
                schema_version: 3,
                pid: Some(pid),
            }]
        );
        assert!(error.hint().is_some());
        assert!(harness.config().is_some(), "nothing was removed");
    }

    // The worker has exited: its outdated journal proves nothing is live.
    write_outdated_journal(
        harness.context.paths(),
        SESSION,
        "w-1",
        "live",
        Some((pid, "1")),
    );
    harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall");
    harness.assert_clean();
}

/// Asserts that `error` refuses the prefix `holder` owns.
fn assert_owned_by(error: &Error, holder: &Harness) {
    let namespace = holder.context.namespace().expect("namespace");
    assert!(
        matches!(error, Error::PrefixOwned { owner, prefix, record }
            if owner == namespace.as_str()
                && prefix == &holder.prefix()
                && record == &layout::owner_record(&holder.layout())),
        "{error:?}"
    );
    assert_eq!(error.code(), "service_prefix_owned");
}

/// Two installations with different XDG roots never share a prefix: the
/// second install is refused before it stages anything, and the owner's
/// uninstall releases the prefix only after removing its own files.
#[tokio::test]
async fn a_prefix_serves_one_namespace_until_its_uninstall_releases_it() {
    let first = Harness::new();
    first.install(V1).await.expect("first install");
    let record = layout::owner_record(&first.layout());
    assert!(record.is_file(), "the install claims the prefix");

    let second = Harness::sharing_prefix(&first);
    let error = second.install(V2).await.expect_err("shared prefix");
    assert_owned_by(&error, &first);
    assert!(second.config().is_none(), "{error:?}");
    assert!(second.pending().is_none(), "{error:?}");
    assert!(second.fake.world().registered.is_none(), "{error:?}");
    assert_eq!(
        layout::installed_versions(&first.layout()).expect("versions"),
        [V1],
        "nothing of the second installation is staged or published"
    );
    first.assert_installed(V1);

    first.upgrade(V2).await.expect("the owner still upgrades");
    let report = first
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("owner uninstall");
    assert!(report.removed.contains(&record));
    first.assert_clean();
    assert!(!record.exists(), "the uninstall releases the prefix");

    second
        .install(V1)
        .await
        .expect("install into the released prefix");
    second.assert_installed(V1);
    second
        .upgrade(V2)
        .await
        .expect("upgrade in the released prefix");
    second.assert_installed(V2);
}

/// A prefix another namespace claimed is never cleaned by this one: upgrade,
/// its version collection, and uninstall are all refused before anything is
/// removed, so the owner's binaries and CLI copy survive.
#[tokio::test]
async fn a_prefix_claimed_by_another_namespace_is_never_cleaned_by_this_one() {
    let first = Harness::new();
    first.install(V1).await.expect("first install");
    // An installation made before prefixes had owners: the record is gone,
    // and the second installation claims the prefix with its own install.
    let record = layout::owner_record(&first.layout());
    std::fs::remove_file(&record).expect("remove record");
    let second = Harness::sharing_prefix(&first);
    second
        .install(V2)
        .await
        .expect("second install claims the prefix");
    second.assert_installed(V2);

    let error = first.upgrade(V2).await.expect_err("upgrade");
    assert_owned_by(&error, &second);
    let error = first
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("uninstall");
    assert_owned_by(&error, &second);
    let error = first
        .engine()
        .collect_garbage(&first.layout(), V1)
        .await
        .expect_err("collection");
    assert_owned_by(&error, &second);

    assert_eq!(
        first.fake.daemon_version().as_deref(),
        Some(V1),
        "the refused uninstall kept its daemon job"
    );
    assert!(first.config().is_some());
    assert_eq!(
        layout::installed_versions(&second.layout()).expect("versions"),
        [V1, V2],
        "no version directory was removed"
    );
    second.assert_installed(V2);
}

#[tokio::test]
async fn an_install_that_fails_or_rolls_back_gives_the_prefix_up() {
    let first = Harness::new();
    let from = first.staged(V1);
    layout::tests::write_fake(&from, CLI_NAME, "0.9.0");
    let error = first
        .engine()
        .install(&from, &first.prefix(), V1)
        .await
        .expect_err("mismatched binaries");
    assert!(matches!(error, Error::VersionMismatch { .. }), "{error:?}");
    assert!(!layout::owner_record(&first.layout()).exists());

    let mut engine = first.engine();
    engine.interrupt_after = Some(Step::Config);
    engine
        .install(&first.staged(V1), &first.prefix(), V1)
        .await
        .expect_err("interrupted");
    assert!(layout::owner_record(&first.layout()).is_file());
    first
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall rolls the install back");
    first.assert_clean();
    assert!(!layout::owner_record(&first.layout()).exists());

    let second = Harness::sharing_prefix(&first);
    second
        .install(V1)
        .await
        .expect("install into the free prefix");
    second.assert_installed(V1);
}

/// The anchor lives and dies with its version directory: the installed
/// version's daemon loads it, garbage collection after an upgrade removes the
/// old directory with its anchor, and uninstall removes the rest.
#[tokio::test]
async fn install_upgrade_and_uninstall_carry_the_anchor_with_the_version_directory() {
    let harness = Harness::new();
    let (first, second) = (anchor_bytes(1), anchor_bytes(2));
    write_anchor(&harness.staged(V1), &first);
    write_anchor(&harness.staged(V2), &second);

    harness.install(V1).await.expect("install");
    let installed = harness.layout().version_dir(V1).expect("version dir");
    let trust = load_from_directory(&installed);
    assert!(matches!(trust, CatalogTrust::Loaded(_)), "{trust:?}");
    assert_eq!(
        std::fs::read(installed.join(ANCHOR_FILE_NAME)).expect("anchor"),
        first
    );

    harness.upgrade(V2).await.expect("upgrade");
    harness.assert_installed(V2);
    let upgraded = harness.layout().version_dir(V2).expect("version dir");
    let trust = load_from_directory(&upgraded);
    assert!(matches!(trust, CatalogTrust::Loaded(_)), "{trust:?}");
    assert_eq!(
        std::fs::read(upgraded.join(ANCHOR_FILE_NAME)).expect("anchor"),
        second
    );
    assert!(!installed.exists(), "garbage collection removed {V1}");

    harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall");
    harness.assert_clean();
}

#[tokio::test]
async fn a_corrupt_staged_anchor_fails_the_install_and_gives_the_prefix_up() {
    let harness = Harness::new();
    write_anchor(&harness.staged(V1), b"not an anchor");
    let error = harness.install(V1).await.expect_err("corrupt anchor");
    assert!(matches!(error, Error::StagedAnchor { .. }), "{error:?}");
    assert!(!layout::owner_record(&harness.layout()).exists());
    harness.assert_clean();
}

/// The prefix is verified before the uninstall touches anything, so an
/// untrusted versions directory leaves the whole installation in place.
#[tokio::test]
async fn an_untrusted_versions_directory_fails_the_uninstall_before_any_change() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let versions = harness.layout().versions_dir();
    std::fs::set_permissions(
        &versions,
        std::os::unix::fs::PermissionsExt::from_mode(0o777),
    )
    .expect("loosen versions dir");
    let error = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("untrusted");
    assert!(
        matches!(&error, Error::UntrustedDirectory { path, .. } if path == &versions),
        "{error:?}"
    );
    std::fs::set_permissions(
        &versions,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("restore versions dir");
    harness.assert_installed(V1);
}

/// The in-use protection matches executables by their canonical path, which
/// equals the configured prefix only while no component of it is a symlink.
/// Every version-directory operation walks the prefix without following
/// symlinks, so a prefix reached through one is refused before any deletion.
#[tokio::test]
async fn a_prefix_reached_through_a_symlink_is_refused_before_any_version_is_deleted() {
    let harness = Harness::new();
    let linked = harness.root.as_path().join("linked");
    std::fs::create_dir(&linked).expect("linked dir");
    std::os::unix::fs::symlink(&linked, harness.root.as_path().join("link")).expect("symlink");
    let through_link = harness.root.as_path().join("link/prefix");
    let error = harness
        .engine()
        .install(&harness.staged(V1), &through_link, V1)
        .await
        .expect_err("install through a symlinked prefix");
    assert!(harness.config().is_none(), "{error:?}");
    assert!(harness.pending().is_none(), "{error:?}");

    // An installation whose prefix becomes a symlink afterwards: a process
    // running from the moved version directory reports the canonical path,
    // which lies outside the configured prefix.
    harness.install(V1).await.expect("install");
    let moved = harness.root.as_path().join("moved");
    std::fs::rename(harness.prefix(), &moved).expect("move prefix");
    std::os::unix::fs::symlink(&moved, harness.prefix()).expect("symlink prefix");
    let sleeper = moved.join("libexec/pohunek").join(V1).join("sleeper");
    std::fs::copy("/bin/sleep", &sleeper).expect("copy sleep");
    let mut child = spawn_tracked(std::process::Command::new(&sleeper).arg("30"));

    let upgrade = harness.upgrade(V2).await;
    let uninstall = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await;
    child.kill().expect("kill stand-in");
    child.wait().expect("reap stand-in");
    upgrade.expect_err("upgrade through a symlinked prefix");
    uninstall.expect_err("uninstall through a symlinked prefix");
    assert!(sleeper.is_file(), "the in-use version directory survives");
    assert!(moved
        .join("libexec/pohunek")
        .join(V1)
        .join("pohunekd")
        .is_file());
}

#[tokio::test]
async fn an_untrusted_unit_directory_fails_before_anything_changes() {
    let harness = Harness::new();
    let open = harness.root.as_path().join("open");
    std::fs::create_dir_all(&open).expect("open dir");
    std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o777))
        .expect("chmod");
    let context = Context::new(
        harness.context.paths().clone(),
        harness.context.uid(),
        Some(harness.root.join("home")),
        Some(harness.root.join("run")),
        open.join("systemd/user"),
        PathBuf::from("/usr/bin/pohunek"),
    );
    let error = engine_over(&context, &harness.backend)
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("untrusted");
    let Error::UntrustedDirectory { path, .. } = &error else {
        panic!("unexpected error {error:?}");
    };
    assert_eq!(path, &open);
    assert!(error
        .to_string()
        .contains(&format!("chmod go-w {}", open.display())));
    assert!(!harness.layout().versions_dir().exists());
    harness.assert_clean();
}

#[test]
fn live_session_classification_covers_runtime_and_logical_state() {
    let mut live = session("s-1", None);
    assert!(is_live(&live));
    live.state = SessionState::Done;
    assert!(is_live(&live), "a live runtime keeps the session live");
    live.runtime = None;
    assert!(!is_live(&live));
    let mut external = session("s-2", None);
    external.external = Some(true);
    assert!(!is_live(&external));
}

#[test]
fn job_alive_treats_launchd_unknown_as_potentially_alive() {
    // Only a stopped or failed job proves termination without further
    // evidence; Unknown is launchd's loaded job without a matching process,
    // which may still spawn.
    assert!(!job_alive(&worker(
        worker_id(SESSION),
        ServiceState::Stopped
    )));
    assert!(!job_alive(&worker(
        worker_id(SESSION),
        ServiceState::Failed
    )));
    for state in [
        ServiceState::Starting,
        ServiceState::Running,
        ServiceState::Stopping,
        ServiceState::Unknown,
    ] {
        assert!(
            job_alive(&worker(worker_id(SESSION), state)),
            "{state:?} may still own a PTY"
        );
    }
}

#[tokio::test]
async fn a_second_command_is_refused_while_a_transaction_holds_the_lock() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let held = harness.engine().store.lock().await.expect("hold the lock");

    let refused = harness.upgrade(V2).await.expect_err("upgrade while locked");
    assert!(
        matches!(refused, Error::TransactionInProgress { .. }),
        "{refused:?}"
    );
    assert_eq!(refused.code(), "service_transaction_in_progress");
    let refused = harness
        .engine()
        .uninstall(UninstallOptions::default())
        .await
        .expect_err("uninstall while locked");
    assert!(
        matches!(refused, Error::TransactionInProgress { .. }),
        "{refused:?}"
    );
    let config = harness.config();
    let status = harness
        .engine()
        .status(config.as_ref())
        .await
        .expect("status while locked");
    assert!(status.transaction_in_progress);
    harness.assert_installed(V1);

    drop(held);
    let status = status_once_free(&harness, config.as_ref()).await;
    assert!(!status.transaction_in_progress);
    Box::pin(once_free("the transaction lock", || harness.upgrade(V2)))
        .await
        .expect("upgrade after release");
    harness.assert_installed(V2);
}

/// The `launchd_job` doctor check for the status the engine reports.
async fn doctor_status(harness: &Harness, reachable: bool) -> protocol::DoctorStatus {
    use crate::commands::doctor::launchd_job_check;

    let config = harness.config();
    let status = harness
        .engine()
        .status(config.as_ref())
        .await
        .expect("status");
    launchd_job_check(Ok(status), reachable, hostcheck::Supervision::Native).status
}

#[tokio::test]
async fn doctor_reads_the_daemon_job_from_the_status_the_engine_reports() {
    use protocol::DoctorStatus;

    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    assert_eq!(doctor_status(&harness, true).await, DoctorStatus::Ok);

    // A loaded job that lost its process is `unknown` under launchd: it is a
    // failure only when the daemon does not answer either.
    harness.fake.world().running = false;
    harness.fake.world().inactive_state = Some(ServiceState::Unknown);
    assert_eq!(doctor_status(&harness, false).await, DoctorStatus::Fail);
    assert_eq!(doctor_status(&harness, true).await, DoctorStatus::Warn);
}

#[tokio::test]
async fn a_record_left_by_a_crashed_holder_is_resumed_under_a_fresh_lock() {
    let harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Config);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    // The interrupted engine released its lock like a crashed process would.
    let status = status_once_free(&harness, None).await;
    assert!(!status.transaction_in_progress);
    assert_eq!(
        status.pending_transaction.map(|pending| pending.step),
        Some(Step::Config.as_str())
    );
    let report = harness.install(V1).await.expect("resume");
    assert!(report.resumed);
    harness.assert_installed(V1);
}

/// Publishes `holder`'s lock the way `pohunek service lock` does and adopts
/// it with the token, as the command it runs would.
fn inherit(harness: &Harness, holder: &TransactionLock) -> (record::Handoff, TransactionLock) {
    let store = harness.engine().store;
    let handoff = store.hand_off(holder).expect("publish the holder record");
    let adopted = store
        .adopt(handoff.token(), record::Adoption::Relay)
        .expect("adopt the held lock");
    (handoff, adopted)
}

#[tokio::test]
async fn transactions_run_under_an_inherited_lock_while_others_are_refused() {
    let harness = Harness::new();
    let holder = harness.engine().store.lock().await.expect("hold the lock");
    let (handoff, adopted) = inherit(&harness, &holder);
    let inherited = || {
        let lock = harness
            .engine()
            .store
            .adopt(handoff.token(), record::Adoption::Transaction)
            .expect("adopt the held lock");
        harness.engine().with_inherited_lock(lock)
    };

    inherited()
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect("install under the inherited lock");
    harness.assert_installed(V1);

    // Another transaction without the token stays refused for as long as
    // the holder holds the lock.
    let refused = harness.upgrade(V2).await.expect_err("upgrade beside it");
    assert_eq!(refused.code(), "service_transaction_in_progress");

    inherited()
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade under the inherited lock");
    harness.assert_installed(V2);
    inherited()
        .uninstall(UninstallOptions::default())
        .await
        .expect("uninstall under the inherited lock");
    harness.assert_clean();

    // A nested handoff passes the same token on and publishes nothing.
    let nested = harness
        .engine()
        .store
        .hand_off(&inherited_lock(&harness, &handoff))
        .expect("hand the adopted lock on");
    assert!(nested.token().matches(handoff.token()));
    nested.release().expect("nothing to remove");

    let token = handoff.token().clone();
    handoff.release().expect("remove the holder record");
    drop(holder);
    assert_no_holder_once_free(&harness.engine().store, &token).await;
    // An adopter that outlives its holder still keeps every other
    // transaction out until it ends.
    let refused = harness
        .install(V1)
        .await
        .expect_err("an adopter still runs");
    assert_eq!(refused.code(), "service_transaction_in_progress");
    drop(adopted);
    once_free("the adopters' locks", || harness.install(V1))
        .await
        .expect("install after the release");
}

#[tokio::test]
async fn adopted_transactions_of_one_holder_run_one_at_a_time() {
    let harness = Harness::new();
    let holder = harness.engine().store.lock().await.expect("hold the lock");
    let store = harness.engine().store;
    let handoff = store.hand_off(&holder).expect("publish the holder record");
    let first = store
        .adopt(handoff.token(), record::Adoption::Transaction)
        .expect("the first adopter");
    let refused = store
        .adopt(handoff.token(), record::Adoption::Transaction)
        .expect_err("a second adopted transaction");
    assert_eq!(refused.code(), "service_transaction_in_progress");

    // A relay, such as a nested `service lock`, runs no transaction itself
    // and adopts beside it.
    drop(
        store
            .adopt(handoff.token(), record::Adoption::Relay)
            .expect("a relay"),
    );

    drop(first);
    let _next = adopt_once_free(&store, handoff.token(), record::Adoption::Transaction).await;
    drop(handoff);
    drop(holder);
}

fn inherited_lock(harness: &Harness, handoff: &record::Handoff) -> TransactionLock {
    harness
        .engine()
        .store
        .adopt(handoff.token(), record::Adoption::Relay)
        .expect("adopt the held lock")
}

/// Runs `operation` until it is no longer refused for a held transaction lock.
///
/// A dropped lock guard closes its descriptor at once, but a process another
/// test thread spawned while it was open keeps a copy, and with it the
/// `flock`, until that process's own `exec`. Probing the lock for freedom
/// first would open one more descriptor that a spawn can inherit, so the real
/// operation is retried on its expected transient refusal instead; any other
/// outcome is returned as it is. Expiry of the hang guard means the lock was
/// never released.
async fn once_free<T, Op, Fut>(what: &str, operation: Op) -> Result<T, Error>
where
    Op: Fn() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    let operation = &operation;
    wait_until(what, || async move {
        match operation().await {
            Err(Error::TransactionInProgress { .. }) => None,
            result => Some(result),
        }
    })
    .await
}

/// Reads the engine status once the transaction lock no longer reports as held.
async fn status_once_free(harness: &Harness, config: Option<&ServiceConfig>) -> StatusReport {
    wait_until("the transaction lock", || async {
        let status = harness.engine().status(config).await.expect("status");
        (!status.transaction_in_progress).then_some(status)
    })
    .await
}

/// Adopts the lock as soon as the previous adopter's descriptors are gone.
async fn adopt_once_free(
    store: &record::Store,
    token: &Token,
    adoption: record::Adoption,
) -> TransactionLock {
    wait_until("the adopted transaction lock", || async {
        match store.adopt(token, adoption) {
            Ok(lock) => Some(lock),
            Err(Error::TransactionInProgress { .. }) => None,
            Err(error) => panic!("adopt: {error:?}"),
        }
    })
    .await
}

/// The refusal of a token while no process holds the transaction lock.
const NO_HOLDER: &str = "no process holds the transaction lock";

/// The refusal of a token while the lock is held but no holder record exists.
const NO_RECORD: &str = "no `pohunek service lock` holds the lock";

/// Asserts that adopting `token` is refused with [`NO_HOLDER`] once the
/// holder's descriptors are gone, for a state directory without a holder
/// record.
///
/// While a copy of the dropped lock descriptor lingers in a spawned process,
/// the lock still looks held, and with no record to validate the adoption is
/// refused with [`NO_RECORD`] instead. That refusal is retried until the lock
/// is free; every other outcome is final.
async fn assert_no_holder_once_free(store: &record::Store, token: &Token) {
    let error = wait_until("the transaction lock", || async {
        match store.adopt(token, record::Adoption::Transaction) {
            Err(Error::InheritedLock { detail }) if detail == NO_RECORD => None,
            result => Some(result),
        }
    })
    .await;
    assert_refused(error, NO_HOLDER);
}

fn assert_refused(result: Result<TransactionLock, Error>, detail: &str) {
    let error = result.expect_err("refused");
    assert!(
        matches!(&error, Error::InheritedLock { detail: actual } if actual == detail),
        "{error:?}"
    );
    assert_eq!(error.code(), "service_inherited_lock_invalid");
    assert!(
        error.to_string().contains("POHUNEK_SERVICE_LOCK_TOKEN"),
        "{error}"
    );
}

/// Opens a second descriptor on the transaction lock file and locks it, as a
/// sibling's pre-`exec` child holds a copy of a dropped guard's descriptor.
fn lingering_copy(harness: &Harness) -> std::fs::File {
    let copy = std::fs::File::options()
        .read(true)
        .write(true)
        .open(harness.context.paths().state_dir.join(record::LOCK_NAME))
        .expect("open the lock file");
    copy.try_lock().expect("hold a copy of the lock");
    copy
}

/// A copy of the lock descriptor held elsewhere, as a sibling's pre-`exec`
/// child holds it, keeps the lock looking held after its owner dropped it. The
/// helper rides out that refusal and still asserts the final one.
#[tokio::test]
async fn a_lingering_copy_of_the_lock_is_waited_out_before_the_final_refusal() {
    let harness = Harness::new();
    let store = harness.engine().store;
    let token = Token::generate().expect("token");
    drop(store.lock().await.expect("create the state directory"));
    let lingering = lingering_copy(&harness);

    // The helper is polled first, so its first adoption meets the held lock;
    // the second future confirms that refusal, then ends the copy's life.
    let release = async {
        assert_refused(
            store.adopt(&token, record::Adoption::Transaction),
            NO_RECORD,
        );
        drop(lingering);
    };
    tokio::join!(assert_no_holder_once_free(&store, &token), release);
}

/// The status helper keeps reading until the lingering copy is gone and
/// returns the report that shows the lock free.
#[tokio::test]
async fn the_status_is_reread_while_a_lingering_copy_holds_the_lock() {
    let harness = Harness::new();
    drop(
        harness
            .engine()
            .store
            .lock()
            .await
            .expect("create the state directory"),
    );
    let lingering = lingering_copy(&harness);

    let release = async {
        let status = harness.engine().status(None).await.expect("status");
        assert!(status.transaction_in_progress);
        drop(lingering);
    };
    let (status, ()) = tokio::join!(status_once_free(&harness, None), release);
    assert!(!status.transaction_in_progress);
}

fn in_progress() -> Error {
    Error::TransactionInProgress {
        path: PathBuf::from("service-install.lock"),
    }
}

/// An operation is retried on the lock refusal alone and its result is the
/// first outcome that is not that refusal.
#[tokio::test(start_paused = true)]
async fn an_operation_is_retried_only_while_the_lock_refuses_it() {
    let attempts = std::cell::Cell::new(0_u32);
    let outcome = once_free("a released lock", || async {
        attempts.set(attempts.get() + 1);
        if attempts.get() < 3 {
            Err(in_progress())
        } else {
            Ok(attempts.get())
        }
    })
    .await;
    assert_eq!(outcome.expect("free on the third attempt"), 3);

    let attempts = std::cell::Cell::new(0_u32);
    let outcome = once_free("a released lock", || async {
        attempts.set(attempts.get() + 1);
        Err::<(), _>(Error::InheritedLock {
            detail: NO_HOLDER.to_owned(),
        })
    })
    .await;
    assert_refused(outcome.map(|()| unreachable!("refused")), NO_HOLDER);
    assert_eq!(attempts.get(), 1, "another refusal is final");
}

/// Writes a holder record naming `pid` with start identity `start`.
fn write_holder(harness: &Harness, pid: u32, start: u64, token: &Token) {
    let path = harness.context.paths().state_dir.join(record::HOLDER_NAME);
    let holder = serde_json::json!({
        "schema_version": 1,
        "pid": pid,
        "start_identity": start,
        "token": token.as_str(),
    });
    std::fs::write(&path, holder.to_string()).expect("write holder record");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("chmod holder record");
}

#[tokio::test]
async fn a_token_that_proves_no_live_holder_is_refused() {
    let harness = Harness::new();
    let store = harness.engine().store;
    let token = Token::generate().expect("token");

    // No state directory, then no lock held.
    assert_refused(
        store.adopt(&token, record::Adoption::Transaction),
        "the state directory does not exist",
    );
    drop(store.lock().await.expect("create the state directory"));
    assert_no_holder_once_free(&store, &token).await;

    // The lock is held, but by a transaction that published no record.
    let holder = store.lock().await.expect("hold the lock");
    assert_refused(
        store.adopt(&token, record::Adoption::Transaction),
        NO_RECORD,
    );

    // A record of this holder with another token.
    let handoff = store.hand_off(&holder).expect("publish");
    assert_refused(
        store.adopt(&token, record::Adoption::Transaction),
        "the token is not the lock holder's",
    );
    handoff.release().expect("remove the record");

    // The recorded holder no longer runs: an exited process, and this
    // process with another start identity, as after PID reuse.
    let own = pohunek_platform::process::ProcessInspector::identity(
        &pohunek_platform::process::HostInspector::new(),
        std::process::id(),
    )
    .expect("inspect")
    .expect("own identity");
    let mut exited = std::process::Command::new("true").spawn().expect("spawn");
    let exited_pid = exited.id();
    exited.wait().expect("reap");
    for (pid, start) in [
        (exited_pid, own.start_identity.get()),
        (own.pid, own.start_identity.get() + 1),
    ] {
        write_holder(&harness, pid, start, &token);
        assert_refused(
            store.adopt(&token, record::Adoption::Transaction),
            "the recorded lock holder no longer runs",
        );
    }

    // The live holder with the right token adopts.
    write_holder(&harness, own.pid, own.start_identity.get(), &token);
    store
        .adopt(&token, record::Adoption::Transaction)
        .expect("the live holder's token adopts");

    // A record that is not one.
    let path = harness.context.paths().state_dir.join(record::HOLDER_NAME);
    std::fs::write(&path, "{}").expect("overwrite");
    assert_refused(
        store.adopt(&token, record::Adoption::Transaction),
        "the lock holder record is malformed",
    );
    drop(holder);
}

/// The local part of `pohunek service check`.
fn check_local_report(
    context: &Context,
    prefix: Option<PathBuf>,
    version: &str,
    locked: bool,
) -> Result<crate::service::report::CheckReport, Error> {
    super::super::check_local(context, prefix, version, locked).map(|checked| checked.report)
}

/// Asserts that `check` changed nothing an install would create.
fn assert_unchanged(harness: &Harness) {
    assert!(!harness.prefix().exists(), "the prefix was created");
    assert!(
        !harness.context.config_path().exists(),
        "service.toml was written"
    );
    assert!(harness.pending().is_none(), "a record was written");
    assert_eq!(
        harness.fake.world().installs,
        0,
        "the daemon was registered"
    );
}

#[tokio::test]
async fn check_passes_a_fresh_install_without_changing_anything() {
    let harness = Harness::new();
    let checked = super::super::check_with(
        &harness.context,
        &harness.backend,
        Some(harness.prefix()),
        V1,
        false,
    )
    .await
    .expect("pass with no daemon job");
    assert_eq!(checked.operation, "install");
    let report =
        check_local_report(&harness.context, Some(harness.prefix()), V1, false).expect("pass");
    assert_eq!(report.operation, "install");
    assert_eq!(report.version, V1);
    assert_eq!(report.prefix, harness.prefix());
    assert_eq!(
        report.namespace,
        harness.context.namespace().expect("namespace").as_str()
    );
    assert_eq!(report.pending_transaction, None);
    assert_eq!(report.pending_action, None);
    assert!(!report.locked);
    assert_unchanged(&harness);

    // Without --prefix the check covers the default `$HOME/.local`.
    let report = check_local_report(&harness.context, None, V1, true).expect("pass");
    assert_eq!(
        report.prefix,
        harness.context.default_prefix().expect("default")
    );
    assert!(report.locked);
}

/// Installs `V1` into `prefix` with `context` in place of the harness's.
async fn install_with(
    harness: &Harness,
    context: &Context,
    prefix: &Path,
) -> Result<InstallReport, Error> {
    engine_over(context, &harness.backend)
        .install(&harness.staged(V1), prefix, V1)
        .await
}

#[tokio::test]
async fn check_refuses_exactly_what_install_refuses_before_its_first_effect() {
    let harness = Harness::new();
    let check = |context: &Context, prefix: &Path| {
        check_local_report(context, Some(prefix.to_path_buf()), V1, false)
    };

    for (case, home) in unusable_homes(&harness) {
        let context = context_with_home(&harness, home);
        let checked = check(&context, &harness.prefix()).expect_err(case);
        assert_unusable_home(case, &checked);
        let installed = install_with(&harness, &context, &harness.prefix())
            .await
            .expect_err(case);
        assert_eq!(checked.code(), installed.code(), "{case}");
    }

    let unnormalized = PathBuf::from(format!("{}/.", harness.prefix().display()));
    let checked = check(&harness.context, &unnormalized).expect_err("unnormalized");
    assert!(
        matches!(
            &checked,
            Error::InvalidPath {
                flag: "--prefix",
                ..
            }
        ),
        "{checked:?}"
    );
    let installed = install_with(&harness, &harness.context, &unnormalized)
        .await
        .expect_err("same");
    assert_eq!(checked.code(), installed.code());

    // A group-writable directory install would write into.
    let shared = harness.root.as_path().join("shared");
    std::fs::create_dir(&shared).expect("shared dir");
    std::fs::set_permissions(&shared, std::os::unix::fs::PermissionsExt::from_mode(0o775))
        .expect("chmod");
    let checked = check(&harness.context, &shared.join("prefix")).expect_err("untrusted");
    assert!(
        matches!(&checked, Error::UntrustedDirectory { path, .. } if path == &shared),
        "{checked:?}"
    );
    let installed = install_with(&harness, &harness.context, &shared.join("prefix"))
        .await
        .expect_err("untrusted");
    assert_eq!(checked.code(), installed.code());
    assert_eq!(checked.to_string(), installed.to_string());
    harness.assert_clean();
    assert_unchanged(&harness);
}

#[tokio::test]
async fn check_covers_every_directory_below_the_prefix_the_transaction_writes() {
    let harness = Harness::new();
    let layout = harness.layout();
    for directory in [
        layout.bin_dir(),
        layout
            .versions_dir()
            .parent()
            .expect("libexec")
            .to_path_buf(),
        layout.versions_dir(),
    ] {
        std::fs::create_dir_all(&directory).expect("create directory");
        std::fs::set_permissions(
            &directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o777),
        )
        .expect("loosen");
        let error = check_local_report(&harness.context, Some(harness.prefix()), V1, false)
            .expect_err("untrusted");
        assert!(
            matches!(&error, Error::UntrustedDirectory { path, .. } if path == &directory),
            "{}: {error:?}",
            directory.display()
        );
        let installed = harness
            .install(V1)
            .await
            .expect_err("install refuses it too");
        assert_eq!(installed.to_string(), error.to_string());
        std::fs::set_permissions(
            &directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("restore");
    }
    assert!(harness.pending().is_none());
    assert_eq!(harness.fake.world().installs, 0);
}

#[tokio::test]
async fn check_reports_how_a_pending_transaction_would_be_handled() {
    let harness = Harness::new();
    let record = pending_install_at_config(&harness).await;
    let check =
        |version| check_local_report(&harness.context, Some(harness.prefix()), version, false);

    let resume = check(V1).expect("same install resumes");
    assert_eq!(resume.operation, "install");
    assert_eq!(resume.pending_action, Some("resume"));
    assert_eq!(
        resume.pending_transaction.map(|pending| pending.step),
        Some(Step::Config.as_str())
    );
    let roll_back = check(V2).expect("another install rolls it back");
    assert_eq!(roll_back.operation, "install");
    assert_eq!(roll_back.pending_action, Some("roll_back"));
    assert_pending_untouched(&harness, &record);

    // A pending install past registration refuses another install.
    let mut advanced = record.clone();
    advanced.step = Step::Registering;
    harness
        .engine()
        .store
        .save(&advanced)
        .expect("advance record");
    let error = check(V2).expect_err("pending registered install");
    assert_eq!(error.code(), "service_install_pending");
}

#[tokio::test]
async fn check_covers_an_upgrade_of_the_recorded_installation() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let report = check_local_report(&harness.context, None, V2, false).expect("upgrade passes");
    assert_eq!(report.operation, "upgrade");
    assert_eq!(report.prefix, harness.prefix());
    assert_eq!(report.version, V2);
    let same = check_local_report(&harness.context, Some(harness.prefix()), V2, false)
        .expect("the installed prefix passes");
    assert_eq!(same, report);

    let other = harness.root.as_path().join("other");
    let error = check_local_report(&harness.context, Some(other.clone()), V2, false)
        .expect_err("another prefix");
    assert!(
        matches!(&error, Error::PrefixMismatch { requested, installed }
            if requested == &other && installed == &harness.prefix()),
        "{error:?}"
    );
    assert_eq!(error.code(), "service_prefix_mismatch");

    // The recorded installation must describe this process, as upgrade
    // verifies it.
    let moved = Context::new(
        crate::service::context::tests::paths(&harness.root.join("moved")),
        harness.context.uid(),
        Some(harness.root.join("home")),
        Some(harness.root.join("run")),
        harness.context.supervisor_dir().to_path_buf(),
        harness.context.cli_executable().to_path_buf(),
    );
    std::fs::create_dir_all(moved.config_path().parent().expect("config dir")).expect("config dir");
    std::fs::copy(harness.context.config_path(), moved.config_path()).expect("copy service.toml");
    moved.roots().expect("the moved roots resolve");
    let error = check_local_report(&moved, None, V2, false).expect_err("moved roots");
    assert!(
        matches!(
            &error,
            Error::Config(pohunek_service_config::ConfigError::NamespaceMismatch { .. })
        ),
        "{error:?}"
    );
    assert_eq!(error.code(), "service_config_invalid");
    harness.assert_installed(V1);
}

#[tokio::test]
async fn check_refuses_a_stale_daemon_job_like_install_does() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    // `service.toml` is gone while the daemon job stays registered.
    std::fs::remove_file(harness.context.config_path()).expect("remove service.toml");
    let checked = super::super::check_local(&harness.context, Some(harness.prefix()), V2, true)
        .expect("the local checks pass");
    assert!(checked.fresh_install);
    let error = super::super::check_with(
        &harness.context,
        &harness.backend,
        Some(harness.prefix()),
        V2,
        true,
    )
    .await
    .expect_err("a registered daemon job");
    assert_eq!(error.code(), "service_daemon_job_present");
    let installed = harness
        .install(V2)
        .await
        .expect_err("install refuses it too");
    assert_eq!(installed.to_string(), error.to_string());

    // A resumed install keeps its own daemon job, so the check skips it.
    let harness = Harness::new();
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .install(&harness.staged(V1), &harness.prefix(), V1)
        .await
        .expect_err("interrupted");
    let report = super::super::check_with(
        &harness.context,
        &harness.backend,
        Some(harness.prefix()),
        V1,
        true,
    )
    .await
    .expect("the resume passes");
    assert_eq!(report.pending_action, Some("resume"));
}

#[tokio::test]
async fn check_refuses_a_prefix_another_namespace_owns() {
    let first = Harness::new();
    first.install(V1).await.expect("first install");
    let second = Harness::sharing_prefix(&first);
    let error = check_local_report(&second.context, Some(second.prefix()), V2, false)
        .expect_err("shared prefix");
    assert_owned_by(&error, &first);
    let installed = second
        .install(V2)
        .await
        .expect_err("install refuses it too");
    assert_owned_by(&installed, &first);
}

/// Second logical session of the preflight tests.
const OTHER_SESSION: &str = "s-01KYAPVPFVHD56Z69B9CX3XWN3";
/// Third logical session of the preflight tests.
const THIRD_SESSION: &str = "s-01KYAPVPFVHD56Z69B9CX3XWN4";

fn store_path(harness: &Harness) -> PathBuf {
    harness
        .context
        .paths()
        .data_dir
        .join(pohunek_paths::METADATA_STORE_NAME)
}

/// Writes a live session through the daemon's own store and the live journal
/// of its worker, the way a running daemon of the installed version leaves them.
fn seed_live_session(harness: &Harness, session_id: &str, worker: &str) {
    let executable = harness.layout().worker_executable(V1).expect("worker");
    seed_live_session_at(harness.context.paths(), &executable, session_id, worker);
}

/// Like [`seed_live_session`] for state below `paths`.
fn seed_live_session_at(paths: &BasePaths, executable: &Path, session_id: &str, worker: &str) {
    let mut info = session(session_id, None);
    info.runtime.as_mut().expect("runtime").worker_id = Some(worker.to_owned());
    let record = SessionRecord {
        schema_version: pohunek_daemon::store::STORE_SCHEMA_VERSION,
        session_id: session_id.to_owned(),
        desired_state: DesiredState::Running,
        transaction: None,
        info,
        recovery: None,
        active_agent_origin: None,
        native_identity_ordering: None,
        runtime: RuntimeRecord {
            state: RuntimeState::Live,
            worker_id: Some(worker.to_owned()),
            worker_instance_id: None,
            service_id: Some(worker_id(session_id).to_string()),
            generation: Some(GENERATION.to_owned()),
            executable: Some(executable.to_path_buf()),
            reason: None,
        },
    };
    DaemonStore::new(paths.data_dir.join(pohunek_paths::METADATA_STORE_NAME))
        .record_session(&record)
        .expect("record session");
    write_live_journal(paths, session_id, worker, executable);
}

/// Rewrites the stored line of `session_id` as a schema-1 line `edit` changed.
fn edit_stored_line(harness: &Harness, session_id: &str, edit: impl Fn(&mut serde_json::Value)) {
    let path = store_path(harness);
    let body = std::fs::read_to_string(&path).expect("read store");
    let mut out = String::new();
    for line in body.lines() {
        let mut value: serde_json::Value = serde_json::from_str(line).expect("store line");
        if value["session_id"] == session_id {
            edit(&mut value);
            value["schema_version"] = 1.into();
        }
        out.push_str(&value.to_string());
        out.push('\n');
    }
    std::fs::write(&path, out).expect("write store");
}

/// A legacy (v0.33.0) recovery binding whose resume mode no release maps.
fn unmappable_legacy_recovery(session_id: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": session_id,
        "agent": "shell",
        "agent_base": "shell",
        "cwd": "/work",
        "cols": 80,
        "rows": 24,
        "native_session_id": "native-1",
        "program": "sh",
        "args": [],
        "input_rules": { "bracketed_paste": false, "submit_delay_ms": 150 },
        "resume_mode": "no-such-mode",
        "ref_kind": "id",
        "resumable": true
    })
}

/// Names of the version directories below the installation's versions directory.
fn version_dirs(harness: &Harness) -> Vec<String> {
    layout::installed_versions(&harness.layout())
        .expect("versions")
        .into_iter()
        .collect()
}

#[tokio::test]
async fn an_upgrade_refuses_live_sessions_whose_records_cannot_be_migrated_and_the_flag_proceeds() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    for (session_id, worker) in [
        (SESSION, "w-1"),
        (OTHER_SESSION, "w-2"),
        (THIRD_SESSION, "w-3"),
    ] {
        seed_live_session(&harness, session_id, worker);
    }
    // The first record cannot be loaded at all, the second loads but its
    // recovery cannot be mapped to a launch spec.
    edit_stored_line(&harness, SESSION, |line| {
        line.as_object_mut().expect("record").remove("runtime");
    });
    edit_stored_line(&harness, OTHER_SESSION, |line| {
        line["recovery"] = unmappable_legacy_recovery(OTHER_SESSION);
    });
    let store_before = std::fs::read(store_path(&harness)).expect("read store");

    let error = harness
        .upgrade(V2)
        .await
        .expect_err("the upgrade is refused");

    let Error::UpgradeAtRisk { sessions, .. } = &error else {
        panic!("{error:?}");
    };
    let listed: Vec<_> = sessions
        .iter()
        .map(|session| {
            (
                session.session_id.as_str(),
                session.verdict,
                session.code.as_str(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (SESSION, Verdict::WouldNotBeAdopted, "record_unreadable"),
            (
                OTHER_SESSION,
                Verdict::WouldLoseRecovery,
                "native_recovery_unmappable"
            ),
        ]
    );
    let message = error.to_string();
    assert!(
        message.contains(SESSION) && message.contains(OTHER_SESSION),
        "{message}"
    );
    assert!(!message.contains(THIRD_SESSION), "{message}");
    assert!(
        message.contains("2 live session(s), 0 stopped session(s)"),
        "{message}"
    );
    assert!(message.contains(&format!("live {SESSION}:")), "{message}");
    assert_eq!(error.code(), "service_upgrade_sessions_at_risk");
    harness.assert_installed(V1);
    assert_eq!(
        version_dirs(&harness),
        [V1],
        "a refusal leaves no version behind"
    );
    assert_eq!(
        std::fs::read(store_path(&harness)).expect("read store"),
        store_before,
        "a refusal never writes the store"
    );

    let report = harness
        .upgrade_accepting_loss(V2)
        .await
        .expect("the flag proceeds");

    assert!(report.accepted_runtime_loss);
    let preflight = report.preflight.expect("the preflight ran");
    assert_eq!(preflight.store.state, StoreState::WouldMigrate);
    assert_eq!(preflight.sessions.len(), 3);
    assert_eq!(
        preflight
            .sessions
            .iter()
            .find(|session| session.session_id == THIRD_SESSION)
            .map(|session| session.verdict),
        Some(Verdict::Adoptable)
    );
    harness.assert_installed(V2);
}

#[tokio::test]
async fn a_clean_upgrade_with_live_sessions_proceeds_and_reports_them_adoptable() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    seed_live_session(&harness, SESSION, "w-1");
    seed_live_session(&harness, OTHER_SESSION, "w-2");

    let report = harness.upgrade(V2).await.expect("a clean upgrade proceeds");

    assert!(!report.accepted_runtime_loss);
    let preflight = report.preflight.expect("the preflight ran");
    assert_eq!(preflight.store.state, StoreState::UpToDate);
    let verdicts: Vec<_> = preflight
        .sessions
        .iter()
        .map(|session| (session.session_id.as_str(), session.verdict))
        .collect();
    assert_eq!(
        verdicts,
        [
            (SESSION, Verdict::Adoptable),
            (OTHER_SESSION, Verdict::Adoptable)
        ]
    );
    harness.assert_installed(V2);
}

#[tokio::test]
async fn a_store_the_new_daemon_cannot_start_with_is_refused_even_with_the_flag() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    seed_live_session(&harness, SESSION, "w-1");
    edit_stored_line(&harness, SESSION, |line| {
        line["schema_version"] = 99.into();
    });
    // `edit_stored_line` stamps schema 1 after the edit; restore the future schema.
    let path = store_path(&harness);
    let body = std::fs::read_to_string(&path).expect("read store");
    std::fs::write(
        &path,
        body.replace("\"schema_version\":1", "\"schema_version\":99"),
    )
    .expect("write store");

    for result in [
        harness.upgrade(V2).await,
        harness.upgrade_accepting_loss(V2).await,
    ] {
        let error = result.expect_err("the store is refused");
        assert!(
            matches!(error, Error::UpgradeStoreUnusable { .. }),
            "{error:?}"
        );
        assert_eq!(error.code(), "service_upgrade_store_unusable");
    }
    harness.assert_installed(V1);
}

#[tokio::test]
async fn the_adoption_preflight_is_skipped_when_no_daemon_swap_starts() {
    let harness = Harness::new();
    harness.install(V2).await.expect("install");
    seed_live_session(&harness, SESSION, "w-1");
    edit_stored_line(&harness, SESSION, |line| {
        line.as_object_mut().expect("record").remove("runtime");
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = harness
        .engine()
        .with_adoption_preflight(Box::new(InProcessPreflight {
            calls: Arc::clone(&calls),
        }));

    let report = engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("rerunning the active version swaps nothing");

    assert!(report.unchanged);
    assert!(report.preflight.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// A check of a recorded-`PATH` job with no reachable bus is refused before it
/// judges a session, with the cause named.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_check_without_a_bus_is_refused_before_it_judges() {
    let harness = harness_without_a_bus().await;
    seed_live_session(&harness, SESSION, "w-1");
    edit_stored_line(&harness, SESSION, |line| {
        line.as_object_mut().expect("record").remove("runtime");
    });
    let daemon_dir = harness.staged(V2);

    let refused = crate::service::check_with_preflight(
        &harness.context,
        &daemon_dir,
        &InProcessPreflight::default(),
        V2,
        false,
    )
    .await
    .expect_err("no judgement is possible without the manager");
    assert_eq!(refused.code(), "service_manager_environment_unavailable");
    assert!(refused.to_string().contains("no session bus"), "{refused}");
}

/// A harness whose daemon job is installed with a recorded `PATH` while the
/// user manager answers, and whose user manager then has no reachable bus.
#[cfg(target_os = "linux")]
async fn harness_without_a_bus() -> Harness {
    let mut harness = Harness::new();
    harness.context = harness.context.clone().with_path_discovery(
        crate::service::context::PathDiscovery::Managed {
            login_shell: None,
            shell_defaulted: false,
            fallback_directories: vec!["/usr/bin".to_owned()],
        },
    );
    harness.install(V1).await.expect("install");
    harness.context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(FailingManager(EnvironmentFailure::Unreachable(
            "no session bus".to_owned(),
        ))));
    harness
}

/// A preflight that reports one adoptable stopped session, whose native
/// reference check reads a store location.
fn preflight_with_a_stopped_session() -> Box<ScriptedPreflight> {
    Box::new(ScriptedPreflight {
        stopped: vec![SessionVerdict {
            session_id: OTHER_SESSION.to_owned(),
            name: None,
            verdict: Verdict::Adoptable,
            code: "adoptable".to_owned(),
            detail: "the stopped session's conversation is in its store".to_owned(),
            acceptance_scope: None,
        }],
        ..ScriptedPreflight::default()
    })
}

/// An upgrade of a recorded-`PATH` job whose manager cannot be reached is
/// refused before any effect, with or without a stopped session, and accepting
/// runtime loss does not change that: the loss it accepts is a judgement the
/// manager's variables could change, and here there is no judgement at all.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_upgrade_without_a_bus_is_refused_before_any_effect() {
    let harness = harness_without_a_bus().await;

    for accept in [false, true] {
        for preflight in [
            preflight_with_a_stopped_session(),
            Box::<ScriptedPreflight>::default(),
        ] {
            let refused = harness
                .engine()
                .with_runtime_loss_accepted(accept)
                .with_adoption_preflight(preflight)
                .upgrade(&harness.staged(V2), V2)
                .await
                .expect_err("no judgement is possible without the manager");
            assert_eq!(refused.code(), "service_manager_environment_unavailable");
            assert!(refused.to_string().contains("no session bus"), "{refused}");
            assert_eq!(harness.pending(), None, "no journal was written");
            harness.assert_installed(V1);
        }
    }
}

/// A rollback whose manager cannot be reached keeps its transaction.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_rollback_without_a_bus_keeps_its_transaction() {
    let mut harness = harness_without_a_bus().await;
    harness.context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(FakeUserManager(Ok(vec![format!(
            "PATH={}",
            harness.root.join(USER_MANAGER_BIN).display()
        )]))));
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after the registration");
    let pending = harness.pending().expect("the interrupted upgrade is kept");
    harness.context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(FailingManager(EnvironmentFailure::Unreachable(
            "no session bus".to_owned(),
        ))));

    for accept in [false, true] {
        let refused = harness
            .engine()
            .with_runtime_loss_accepted(accept)
            .with_adoption_preflight(preflight_with_a_stopped_session())
            .upgrade(&harness.staged(V1), V1)
            .await
            .expect_err("no judgement is possible without the manager");
        assert_eq!(refused.code(), "service_manager_environment_unavailable");
        assert_eq!(harness.pending(), Some(pending.clone()));
        assert_eq!(harness.config().expect("config").active_version(), V2);
    }
}

#[tokio::test]
async fn check_asks_the_preflight_of_an_upgrade_and_honours_the_flag() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    seed_live_session(&harness, SESSION, "w-1");
    edit_stored_line(&harness, SESSION, |line| {
        line.as_object_mut().expect("record").remove("runtime");
    });
    let preflight = InProcessPreflight::default();
    let daemon_dir = harness.staged(V2);
    // The checked version is the staged one, so `check` probes that daemon.
    let refused =
        crate::service::check_with_preflight(&harness.context, &daemon_dir, &preflight, V2, false)
            .await
            .expect_err("the check refuses what the upgrade refuses");
    assert!(
        matches!(refused, Error::UpgradeAtRisk { .. }),
        "{refused:?}"
    );

    let accepted =
        crate::service::check_with_preflight(&harness.context, &daemon_dir, &preflight, V2, true)
            .await
            .expect("the flag passes the check");

    assert_eq!(accepted.operation, "upgrade");
    assert!(accepted.accepted_runtime_loss);
    assert_eq!(accepted.preflight.expect("preflight").sessions.len(), 1);
    harness.assert_installed(V1);
}

#[tokio::test]
async fn a_session_created_between_staging_and_the_swap_is_caught_by_the_late_preflight() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    // The host is empty when the upgrade starts; the old daemon then accepts a
    // session whose worker the new daemon could not adopt.
    let paths = harness.context.paths().clone();
    let executable = harness.layout().worker_executable(V1).expect("worker");
    let mut engine = harness.engine();
    engine.before_swap = Some(SwapHook(Box::new(move || {
        seed_live_session_at(&paths, &executable, SESSION, "w-1");
        let store = paths.data_dir.join(pohunek_paths::METADATA_STORE_NAME);
        let body = std::fs::read_to_string(&store).expect("read store");
        let mut line: serde_json::Value = serde_json::from_str(body.trim()).expect("line");
        line.as_object_mut().expect("record").remove("runtime");
        std::fs::write(&store, format!("{line}\n")).expect("write store");
    })));

    let error = engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("the late preflight refuses");

    let Error::UpgradeAtRisk { sessions, .. } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(sessions[0].session_id, SESSION);
    harness.assert_installed(V1);
    assert_eq!(
        version_dirs(&harness),
        [V1],
        "the refused swap is rolled back"
    );
}

#[tokio::test]
async fn a_resumed_upgrade_that_still_has_to_swap_the_daemon_is_judged_again() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Binaries);
    let interrupted = engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted before the config step");
    assert!(
        matches!(interrupted, Error::Interrupted(Step::Binaries)),
        "{interrupted:?}"
    );
    // The old daemon keeps running and gains a session nobody consented to lose.
    seed_live_session(&harness, SESSION, "w-1");
    edit_stored_line(&harness, SESSION, |line| {
        line.as_object_mut().expect("record").remove("runtime");
    });

    let error = harness.upgrade(V2).await.expect_err("the resume is judged");
    assert!(matches!(error, Error::UpgradeAtRisk { .. }), "{error:?}");

    let checked = crate::service::check_with_preflight(
        &harness.context,
        &harness.staged(V2),
        &InProcessPreflight::default(),
        V2,
        false,
    )
    .await;
    // The refused resume rolled the transaction back, so check sees a fresh upgrade.
    assert!(
        matches!(checked, Err(Error::UpgradeAtRisk { .. })),
        "{checked:?}"
    );
}

#[tokio::test]
async fn an_upgrade_interrupted_before_the_service_manager_call_is_judged_again_on_resume() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registering);
    let interrupted = engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after recording the registration");
    assert!(
        matches!(interrupted, Error::Interrupted(Step::Registering)),
        "{interrupted:?}"
    );
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
    // The old daemon is still running and gains a session the new one would lose.
    seed_live_session(&harness, SESSION, "w-1");
    edit_stored_line(&harness, SESSION, |line| {
        line.as_object_mut().expect("record").remove("runtime");
    });

    let checked = crate::service::check_with_preflight(
        &harness.context,
        &harness.staged(V2),
        &InProcessPreflight::default(),
        V2,
        false,
    )
    .await;
    assert!(
        matches!(checked, Err(Error::UpgradeAtRisk { .. })),
        "{checked:?}"
    );
    let error = harness.upgrade(V2).await.expect_err("the resume is judged");
    assert!(matches!(error, Error::UpgradeAtRisk { .. }), "{error:?}");
    assert_eq!(
        harness.fake.daemon_version().as_deref(),
        Some(V1),
        "no replacement"
    );
}

/// Judges adoption in this process until its `fail_from`-th call, then fails
/// as a daemon that gives no report does.
#[derive(Debug)]
struct FailingFrom {
    inner: InProcessPreflight,
    fail_from: usize,
}

impl AdoptionPreflight for FailingFrom {
    fn judge<'a>(
        &'a self,
        context: &'a Context,
        dir: &'a VerifiedVersionDir,
        search_path: &'a str,
        variables: &'a BTreeMap<String, String>,
        probe_budget: Option<Duration>,
        plan: ReleasePlan,
    ) -> Judging<'a> {
        let call = self.inner.calls.load(Ordering::SeqCst);
        if call >= self.fail_from {
            return Box::pin(async move {
                Err(Error::UpgradePreflightFailed {
                    binary: dir.daemon(),
                    detail: "injected".to_owned(),
                })
            });
        }
        self.inner
            .judge(context, dir, search_path, variables, probe_budget, plan)
    }
}

/// Seeds a live session the new daemon would not adopt.
fn seed_unadoptable_session(harness: &Harness) {
    seed_live_session(harness, SESSION, "w-1");
    edit_stored_line(harness, SESSION, |line| {
        line.as_object_mut().expect("record").remove("runtime");
    });
}

/// Asserts the previous daemon was never replaced or restarted.
fn assert_daemon_untouched(harness: &Harness, replaces_before: usize) {
    assert_eq!(
        harness.fake.world().replaces,
        replaces_before,
        "no replacement call"
    );
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V1));
}

#[tokio::test]
async fn a_refused_resume_at_registering_never_touches_the_daemon_and_stays_resumable() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registering);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted before the service-manager call");
    let replaces = harness.fake.world().replaces;
    let paths = harness.context.paths().clone();
    let executable = harness.layout().worker_executable(V1).expect("worker");
    let mut resumed = harness.engine();
    resumed.before_swap = Some(SwapHook(Box::new(move || {
        seed_live_session_at(&paths, &executable, SESSION, "w-1");
        let store = paths.data_dir.join(pohunek_paths::METADATA_STORE_NAME);
        let body = std::fs::read_to_string(&store).expect("read store");
        let mut line: serde_json::Value = serde_json::from_str(body.trim()).expect("line");
        line.as_object_mut().expect("record").remove("runtime");
        std::fs::write(&store, format!("{line}\n")).expect("write store");
    })));

    let error = resumed
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("the final gate refuses");

    assert!(matches!(error, Error::UpgradeAtRisk { .. }), "{error:?}");
    assert_daemon_untouched(&harness, replaces);
    let pending = harness.pending().expect("the transaction is kept");
    assert_eq!(pending.step, Step::Registering);

    let report = harness
        .upgrade_accepting_loss(V2)
        .await
        .expect("a later resume with the flag proceeds");
    assert!(report.accepted_runtime_loss);
    assert!(report.resumed);
    harness.assert_installed(V2);
}

#[tokio::test]
async fn no_gate_refusal_before_the_swap_restarts_the_daemon() {
    // Early refusal: the sessions are at risk when the upgrade starts.
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    seed_unadoptable_session(&harness);
    let replaces = harness.fake.world().replaces;
    harness.upgrade(V2).await.expect_err("early refusal");
    assert_daemon_untouched(&harness, replaces);
    harness.assert_installed(V1);

    // Store the new daemon cannot start with, found at the final gate.
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let replaces = harness.fake.world().replaces;
    let path = store_path(&harness);
    let mut engine = harness.engine();
    engine.before_swap = Some(SwapHook(Box::new(move || {
        let directory = path.parent().expect("store directory");
        std::fs::create_dir_all(directory).expect("data directory");
        std::fs::set_permissions(
            directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("private data directory");
        std::fs::write(&path, "{\"kind\":\"project\",\"schema_version\":99}\n")
            .expect("write future store");
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("private store");
    })));
    let error = engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("late refusal of the store");
    assert!(
        matches!(error, Error::UpgradeStoreUnusable { .. }),
        "{error:?}"
    );
    assert_daemon_untouched(&harness, replaces);
    harness.assert_installed(V1);

    // A preflight that gives no report, early and then late.
    for fail_from in [0, 1] {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let replaces = harness.fake.world().replaces;
        let engine = harness
            .engine()
            .with_adoption_preflight(Box::new(FailingFrom {
                inner: InProcessPreflight::default(),
                fail_from,
            }));
        let error = engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("no report");
        assert!(
            matches!(error, Error::UpgradePreflightFailed { .. }),
            "{error:?}"
        );
        assert_daemon_untouched(&harness, replaces);
        harness.assert_installed(V1);
    }
}

#[tokio::test]
async fn rollback_refusal_distinguishes_live_and_stopped_session_risks() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after replacement");
    seed_unadoptable_session(&harness);
    let stopped = SessionVerdict {
        session_id: OTHER_SESSION.to_owned(),
        name: None,
        verdict: Verdict::WouldLoseRecovery,
        code: "native_recovery_unmappable".to_owned(),
        detail: "the stopped session's recovery cannot be mapped".to_owned(),
        acceptance_scope: None,
    };
    let error = harness
        .engine()
        .with_adoption_preflight(Box::new(ScriptedPreflight {
            stopped: vec![stopped],
            ..ScriptedPreflight::default()
        }))
        .upgrade(&harness.staged(V1), V1)
        .await
        .expect_err("both groups need explicit consent");
    assert_eq!(error.code(), "service_rollback_sessions_at_risk");
    let message = error.to_string();
    assert!(
        message.contains("1 live session(s), 1 stopped session(s)"),
        "{message}"
    );
    assert!(message.contains(&format!("live {SESSION}:")), "{message}");
    assert!(
        message.contains(&format!("stopped {OTHER_SESSION}:")),
        "{message}"
    );
    assert_eq!(
        harness.pending().expect("record kept").step,
        Step::Registered
    );
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V2));
}

#[tokio::test]
async fn cancellation_checks_the_previous_daemon_when_it_may_have_been_replaced() {
    for interrupted_at in [Step::Binaries, Step::Registering, Step::Registered] {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let mut engine = harness.engine();
        engine.interrupt_after = Some(interrupted_at);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted");
        // A session becomes unadoptable while the upgrade is interrupted.
        seed_unadoptable_session(&harness);
        let calls = Arc::new(AtomicUsize::new(0));
        if interrupted_at == Step::Binaries {
            let report = harness
                .engine()
                .with_adoption_preflight(Box::new(CountingFailure(Arc::clone(&calls))))
                .upgrade(&harness.staged(V1), V1)
                .await
                .expect("the old daemon still runs before registration");
            assert!(report.preflight.is_none());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            harness.assert_installed(V1);
            continue;
        }
        let refused = harness
            .upgrade(V1)
            .await
            .expect_err("rollback needs consent");
        assert!(
            matches!(refused, Error::RollbackAtRisk { .. }),
            "{refused:?}"
        );
        assert_eq!(harness.pending().expect("record kept").step, interrupted_at);
        let report = harness
            .upgrade_accepting_loss(V1)
            .await
            .expect("accepted rollback completes");
        assert!(
            report.unchanged && report.rolled_back.is_some(),
            "{report:?}"
        );
        assert!(report.accepted_runtime_loss);
        let preflight = report.preflight.expect("rollback preflight");
        assert_eq!(preflight.daemon_version, crate::service::VERSION);
        assert_eq!(preflight.sessions.len(), 1);
        assert!(
            report
                .rolled_back
                .expect("rolled back")
                .accepted_runtime_loss
        );
        harness.assert_installed(V1);
    }
}

/// Records a store the previous schema-1 daemon must not load.
fn write_new_store(path: &Path) {
    let directory = path.parent().expect("store parent");
    std::fs::create_dir_all(directory).expect("create data directory");
    std::fs::set_permissions(
        directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private data directory");
    let project = ProjectRecord {
        git_common_dir: directory.join("repository/.git"),
        repo_root: directory.join("repository"),
        custom_name: None,
        origin_url: None,
        default_base_branch: None,
        is_bare: false,
        source: ProjectSource::Manual,
        added_at: "2026-10-09T00:00:00Z".to_owned(),
        last_used_at: "2026-10-09T00:00:00Z".to_owned(),
    };
    DaemonStore::new(path.to_path_buf())
        .record_project(&project)
        .expect("write current-schema project record");
}

fn write_legacy_store_bytes(path: &Path, bytes: &[u8]) {
    let directory = path.parent().expect("store parent");
    std::fs::create_dir_all(directory).expect("create data directory");
    std::fs::set_permissions(
        directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private data directory");
    std::fs::write(path, bytes).expect("write legacy store fixture");
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("private store file");
}

#[tokio::test]
async fn cancellation_keeps_new_daemon_when_previous_reader_cannot_read_store() {
    // Both released readers refuse any store a newer schema stamped.
    let previous_list = ["0.33.0", "0.33.1"];
    for previous in previous_list {
        for accepted in [false, true] {
            let harness = Harness::new();
            harness
                .install(previous)
                .await
                .expect("install previous release");
            let mut engine = harness.engine();
            engine.interrupt_after = Some(Step::Registered);
            engine
                .upgrade(&harness.staged(V2), V2)
                .await
                .expect_err("interrupted after replacement");
            write_new_store(&store_path(&harness));
            let before = harness.pending().expect("pending upgrade");
            let replaces = harness.fake.world().replaces;
            let checked = crate::service::check_with_preflight(
                &harness.context,
                &harness.staged(previous),
                &InProcessPreflight::default(),
                previous,
                accepted,
            )
            .await
            .expect_err("read-only check rejects the same old reader");
            assert_eq!(checked.code(), "service_rollback_store_unusable");
            let result = harness
                .engine()
                .with_runtime_loss_accepted(accepted)
                .upgrade(&harness.staged(previous), previous)
                .await;
            let error = result.expect_err("old reader cannot load schema 4");
            assert_eq!(error.code(), "service_rollback_store_unusable");
            assert_eq!(harness.pending(), Some(before));
            assert_eq!(harness.fake.world().replaces, replaces);
            assert_eq!(harness.fake.daemon_version().as_deref(), Some(V2));
            assert_eq!(harness.config().expect("config").active_version(), V2);
        }
    }
}

#[tokio::test]
async fn downgrade_accepts_a_previous_release_store_record() {
    // A project record written by either legacy release, with no live worker
    // to gate. Both readers keep it.
    const LEGACY_PROJECT: &str = r#"{"kind":"project","git_common_dir":"/workspace/project/.git","repo_root":"/workspace/project","custom_name":"Fixture Project","origin_url":"https://github.com/example/repo.git","is_bare":false,"source":"auto","added_at":"2026-06-19T00:00:00Z","last_used_at":"2026-06-19T00:00:00Z"}"#;
    for previous in ["0.33.0", "0.33.1"] {
        let harness = Harness::new();
        harness
            .install(previous)
            .await
            .expect("install previous release");
        let mut engine = harness.engine();
        engine.interrupt_after = Some(Step::Registered);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted after replacement");
        write_legacy_store_bytes(
            &store_path(&harness),
            format!("{LEGACY_PROJECT}\n").as_bytes(),
        );

        let report = harness
            .upgrade(previous)
            .await
            .expect("previous reader accepts schema-1 project record");
        let preflight = report.preflight.expect("rollback preflight");
        assert_eq!(preflight.store.state, StoreState::UpToDate);
        assert_eq!(preflight.store.schema_from, Some(1));
        assert_eq!(preflight.store.records, 1);
        harness.assert_installed(previous);
    }
}

#[tokio::test]
async fn downgrade_refuses_malformed_or_unknown_store_records_before_any_effect() {
    // Both released readers drop a malformed or unknown-kind line silently
    // and rewrite the store without it.
    for previous in ["0.33.0", "0.33.1"] {
        for bytes in [
            b"{invalid json}\n".as_slice(),
            b"{\"kind\":\"unknown\"}\n".as_slice(),
        ] {
            let harness = Harness::new();
            harness
                .install(previous)
                .await
                .expect("install previous release");
            let mut engine = harness.engine();
            engine.interrupt_after = Some(Step::Registered);
            engine
                .upgrade(&harness.staged(V2), V2)
                .await
                .expect_err("interrupted after replacement");
            write_legacy_store_bytes(&store_path(&harness), bytes);
            let before = harness.pending().expect("pending upgrade");
            let (installs, replaces) = {
                let world = harness.fake.world();
                (world.installs, world.replaces)
            };

            let checked = crate::service::check_with_preflight(
                &harness.context,
                &harness.staged(previous),
                &InProcessPreflight::default(),
                previous,
                true,
            )
            .await
            .expect_err("read-only check refuses invalid store");
            assert_eq!(checked.code(), "service_rollback_store_unusable");
            assert!(
                matches!(
                    &checked,
                    Error::RollbackStoreUnusable { code, .. } if code == "legacy_store_invalid"
                ),
                "{previous}: {checked:?}"
            );
            let error = harness
                .upgrade_accepting_loss(previous)
                .await
                .expect_err("rollback refuses invalid store");
            assert_eq!(error.code(), "service_rollback_store_unusable");
            assert!(
                matches!(
                    &error,
                    Error::RollbackStoreUnusable { code, .. } if code == "legacy_store_invalid"
                ),
                "{previous}: {error:?}"
            );
            assert_eq!(harness.pending(), Some(before));
            let after = {
                let world = harness.fake.world();
                (world.installs, world.replaces, world.running)
            };
            assert_eq!(after.0, installs);
            assert_eq!(after.1, replaces);
            assert!(after.2, "refusal leaves the new daemon running");
            assert_eq!(harness.fake.daemon_version().as_deref(), Some(V2));
            assert_eq!(harness.config().expect("config").active_version(), V2);
        }
    }
}

/// A whole schema-1 store the v0.33.1 reader keeps: one record of every kind.
const LEGACY_STORE_ALL_KINDS: &str = concat!(
    r#"{"kind":"session","schema_version":1,"session_id":"s-leg","desired_state":"running","#,
    r#""info":{"id":"s-leg","external":false,"agent":"shell","agent_base":"shell","cwd":"/work","#,
    r#""pid":412,"cols":80,"rows":24,"state":"running","state_source":"process","#,
    r#""created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"},"#,
    r#""runtime":{"state":"live","worker_id":"w-1","service_id":"s-leg.abcd2345"}}"#,
    "\n",
    r#"{"kind":"resume","session_id":"s-leg","agent":"shell","cwd":"/work","cols":80,"rows":24,"#,
    r#""native_session_id":"native-1"}"#,
    "\n",
    r#"{"kind":"worktree","session_id":"s-leg","repository":"/repo/main","branch":"main","#,
    r#""base_branch":"trunk","branch_slug":"main","path":"/repo/wt","agent":"shell","#,
    r#""status":"active","created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"}"#,
    "\n",
    r#"{"kind":"project","git_common_dir":"/workspace/project/.git","repo_root":"/workspace/project","#,
    r#""custom_name":"Fixture Project","origin_url":"https://github.com/example/repo.git","#,
    r#""is_bare":false,"source":"auto","added_at":"2026-06-19T00:00:00Z","#,
    r#""last_used_at":"2026-06-19T00:00:00Z"}"#,
    "\n",
);

/// Leaves the service pending at `registered` (the daemon already runs the
/// new version), with the store migrated past schema 1, so the previous
/// release's reader refuses the store and any rollback to it.
///
/// Nothing rewrites the store during a pending transaction, so the test
/// simulates the new daemon's startup migration by writing it through the
/// daemon's own current-schema writer.
async fn pending_registered_upgrade_with_migrated_store(harness: &Harness) -> Record {
    const PREVIOUS: &str = "0.33.1";
    harness
        .install(PREVIOUS)
        .await
        .expect("install previous release");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after replacement");
    write_new_store(&store_path(harness));
    harness.pending().expect("pending upgrade")
}

#[tokio::test]
async fn an_upgrade_accepts_a_whole_schema1_store_with_a_record_of_every_kind() {
    // Previously covered only a project record; this also pins the valid
    // session, resume, and worktree shapes both released readers' frozen
    // mirrors admit (the fixture line shapes both releases wrote).
    for previous in ["0.33.0", "0.33.1"] {
        let harness = Harness::new();
        harness
            .install(previous)
            .await
            .expect("install previous release");
        let mut engine = harness.engine();
        engine.interrupt_after = Some(Step::Registered);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted after replacement");
        write_legacy_store_bytes(&store_path(&harness), LEGACY_STORE_ALL_KINDS.as_bytes());

        let report = harness
            .upgrade(previous)
            .await
            .expect("previous reader keeps every schema-1 record");
        let preflight = report.preflight.expect("rollback preflight");
        assert_eq!(preflight.store.state, StoreState::UpToDate);
        assert_eq!(preflight.store.schema_from, Some(1));
        assert_eq!(preflight.store.records, 4);
        harness.assert_installed(previous);
    }
}

/// Schema-1 records with known kinds that both released readers deserialize
/// past the syntax checks, then silently skip: a missing required field
/// fails the readers' deserialization, and an unsupported agent kind fails
/// their (different) persistence rules in both.
const TYPED_MALFORMED_STORES: &[(&str, &str)] = &[
    // A session record without its required runtime binding.
    (
        "session without runtime",
        concat!(
            r#"{"kind":"session","schema_version":1,"session_id":"s-leg","desired_state":"running","#,
            r#""info":{"id":"s-leg","agent":"shell","agent_base":"shell","cwd":"/work","pid":412,"cols":80,"rows":24,"state":"running","state_source":"process","created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"}}"#,
        ),
    ),
    // A session whose summary agent kind is not a runtime id: the reader
    // drops even a fully typed record over this semantic rule.
    (
        "session with a historical agent kind",
        concat!(
            r#"{"kind":"session","schema_version":1,"session_id":"s-leg","desired_state":"running","#,
            r#""info":{"id":"s-leg","agent":"mystery","agent_base":"Legacy Agent Friend","cwd":"/work","pid":412,"cols":80,"rows":24,"state":"running","state_source":"process","created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"},"#,
            r#""runtime":{"state":"live"}}"#,
        ),
    ),
    // A resume record whose agent base cannot be recorded nor derived.
    (
        "resume without a recordable agent base",
        r#"{"kind":"resume","session_id":"s-leg","agent":"mystery","cwd":"/work","cols":80,"rows":24}"#,
    ),
    // A worktree record with an unrecognized status.
    (
        "worktree with an unknown status",
        concat!(
            r#"{"kind":"worktree","session_id":"s-leg","repository":"/repo/main","branch":"main","#,
            r#""base_branch":"trunk","branch_slug":"main","path":"/repo/wt","status":"merged-forever","#,
            r#""created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"}"#,
        ),
    ),
    // A project record whose identity fields are all missing.
    ("project without identity fields", r#"{"kind":"project"}"#),
];

#[tokio::test]
async fn a_downgrade_refuses_typed_malformed_records_of_every_kind() {
    // Each fixture fails either reader: the missing field fails both
    // deserializations, the historical agent kind fails both persistence
    // rules (v0.33.0's known-kind check and v0.33.1's runtime-id grammar),
    // and the shared worktree/project shapes fail both.
    for previous in ["0.33.0", "0.33.1"] {
        for (name, line) in TYPED_MALFORMED_STORES {
            let harness = Harness::new();
            harness
                .install(previous)
                .await
                .expect("install previous release");
            let mut engine = harness.engine();
            engine.interrupt_after = Some(Step::Registered);
            engine
                .upgrade(&harness.staged(V2), V2)
                .await
                .expect_err("interrupted after replacement");
            write_legacy_store_bytes(&store_path(&harness), format!("{line}\n").as_bytes());
            let before = harness.pending().expect("pending upgrade");
            let (installs, replaces) = {
                let world = harness.fake.world();
                (world.installs, world.replaces)
            };
            let store_before = std::fs::read(store_path(&harness)).expect("read store");

            let checked = crate::service::check_with_preflight(
                &harness.context,
                &harness.staged(previous),
                &InProcessPreflight::default(),
                previous,
                true,
            )
            .await
            .expect_err("read-only check refuses what the reader would skip");
            assert_eq!(checked.code(), "service_rollback_store_unusable", "{name}");
            assert!(
                matches!(
                    &checked,
                    Error::RollbackStoreUnusable { code, .. } if code == "legacy_store_invalid"
                ),
                "{previous} {name}: {checked:?}"
            );

            let error = harness
                .upgrade_accepting_loss(previous)
                .await
                .expect_err("rollback refuses what the reader would skip");
            assert_eq!(error.code(), "service_rollback_store_unusable", "{name}");
            assert!(
                matches!(
                    &error,
                    Error::RollbackStoreUnusable { code, .. } if code == "legacy_store_invalid"
                ),
                "{previous} {name}: {error:?}"
            );
            // Nothing changed: the record survives for the operator, the new
            // daemon still runs, and the store is untouched.
            assert_eq!(harness.pending(), Some(before), "{name}");
            let after = {
                let world = harness.fake.world();
                (world.installs, world.replaces, world.running)
            };
            assert_eq!(after.0, installs, "{name}");
            assert_eq!(after.1, replaces, "{name}");
            assert!(after.2, "{name}: the new daemon still runs");
            assert_eq!(harness.fake.daemon_version().as_deref(), Some(V2));
            assert_eq!(
                std::fs::read(store_path(&harness)).expect("read store"),
                store_before,
                "{name}: the store is untouched"
            );
            assert_eq!(
                harness.config().expect("config").active_version(),
                V2,
                "{name}"
            );
        }
    }
}

/// Leaves a pending upgrade whose store holds nothing but one schema-1
/// `line`, with the new daemon registered and no live worker.
async fn pending_upgrade_with_legacy_store_line(harness: &Harness, previous: &str, line: &str) {
    harness
        .install(previous)
        .await
        .expect("install previous release");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after replacement");
    write_legacy_store_bytes(&store_path(harness), format!("{line}\n").as_bytes());
}

/// Pins a schema-1 store `line` as refused by a rollback to `previous`,
/// judged by that release's frozen reader: the read-only check refuses, and
/// the rollback refuses with and without runtime-loss consent, all before
/// any effect. `verdict` names the fragment the refusal detail carries, to
/// tell the reader's skip rule apart from the relaunch-rewrite loss.
async fn pending_store_line_refuses(
    harness: &Harness,
    previous: &str,
    name: &str,
    line: &str,
    verdict: &str,
) {
    pending_upgrade_with_legacy_store_line(harness, previous, line).await;
    let before = harness.pending().expect("pending upgrade");
    let (installs, replaces) = {
        let world = harness.fake.world();
        (world.installs, world.replaces)
    };
    let store_before = std::fs::read(store_path(harness)).expect("read store");

    let checked = crate::service::check_with_preflight(
        &harness.context,
        &harness.staged(previous),
        &InProcessPreflight::default(),
        previous,
        true,
    )
    .await
    .expect_err("read-only check refuses what the reader would lose");
    assert_eq!(
        checked.code(),
        "service_rollback_store_unusable",
        "{previous} {name}"
    );
    assert!(
        matches!(
            &checked,
            Error::RollbackStoreUnusable { code, detail, .. }
                if code == "legacy_store_invalid" && detail.contains(verdict)
        ),
        "{previous} {name}: {checked:?}"
    );

    let error = harness
        .upgrade(previous)
        .await
        .expect_err("rollback refuses what the reader would lose, consent or not");
    assert_eq!(
        error.code(),
        "service_rollback_store_unusable",
        "{previous} {name}"
    );
    assert!(
        matches!(
            &error,
            Error::RollbackStoreUnusable { code, detail, .. }
                if code == "legacy_store_invalid" && detail.contains(verdict)
        ),
        "{previous} {name}: {error:?}"
    );

    let error = harness
        .upgrade_accepting_loss(previous)
        .await
        .expect_err("rollback refuses what the reader would lose, consent or not");
    assert_eq!(
        error.code(),
        "service_rollback_store_unusable",
        "{previous} {name}"
    );
    assert!(
        matches!(
            &error,
            Error::RollbackStoreUnusable { code, detail, .. }
                if code == "legacy_store_invalid" && detail.contains(verdict)
        ),
        "{previous} {name}: {error:?}"
    );

    // Nothing changed: the pending transaction is kept, the new daemon
    // still runs its recorded config, and the store is untouched for the
    // operator.
    assert_eq!(harness.pending(), Some(before), "{previous} {name}");
    let after = {
        let world = harness.fake.world();
        (world.installs, world.replaces, world.running)
    };
    assert_eq!(after.0, installs, "{previous} {name}");
    assert_eq!(after.1, replaces, "{previous} {name}");
    assert!(after.2, "{previous} {name}: the new daemon still runs");
    assert_eq!(
        harness.fake.daemon_version().as_deref(),
        Some(V2),
        "{previous} {name}"
    );
    assert_eq!(
        harness.config().expect("config").active_version(),
        V2,
        "{previous} {name}"
    );
    assert_eq!(
        std::fs::read(store_path(harness)).expect("read store"),
        store_before,
        "{previous} {name}: the store is untouched"
    );
}

/// Pins a schema-1 store `line` as kept by a rollback to `previous`, judged
/// by that release's frozen reader: the pending upgrade rolls back to
/// `previous` and its reader keeps the whole store.
async fn pending_store_line_keeps(harness: &Harness, previous: &str, line: &str) {
    pending_upgrade_with_legacy_store_line(harness, previous, line).await;
    let report = harness
        .upgrade(previous)
        .await
        .expect("the previous reader keeps the record");
    let preflight = report.preflight.expect("rollback preflight");
    assert_eq!(preflight.store.state, StoreState::UpToDate, "{previous}");
    assert_eq!(preflight.store.records, 1, "{previous}");
    harness.assert_installed(previous);
}

/// A resume line whose `resume_mode` the v0.33.0 reader requires to be one
/// of its relaunch modes and skips the line over: a record the v0.33.1
/// reader deserializes and still rewrites the relaunch snapshot away from.
const V0_33_0_UNKNOWN_RESUME_MODE: &str = r#"{"kind":"resume","session_id":"s-mode","agent":"claude","agent_base":"claude","cwd":"/work","cols":80,"rows":24,"native_session_id":"native-1","resume_mode":"no-such-mode"}"#;

/// A resume line whose launch spec has no reference placeholder.
const V0_33_1_REFERENCE_LESS_LAUNCH: &str = r#"{"kind":"resume","session_id":"s-launch","agent":"claude","agent_base":"claude","cwd":"/work","cols":80,"rows":24,"native_session_id":"native-1","native_launch":{"reference_kind":"id","resume_args":[]}}"#;

/// A resume line whose launch pin names a digest the strict digest grammar
/// rejects.
const V0_33_1_UNDIGESTED_LAUNCH_PIN: &str = r#"{"kind":"resume","session_id":"s-pin","agent":"codex","agent_base":"codex","cwd":"/work","cols":80,"rows":24,"native_session_id":"native-2","launch_binding":{"runtime_id":"codex","provenance":{"kind":"package","package":{"id":"codex","version":"1.2.3"},"package_digest":"nothex"}}}"#;

/// A resume line whose launch spec names a reference kind the readers never
/// spell.
const V0_33_1_UNKNOWN_REFERENCE_KIND: &str = r#"{"kind":"resume","session_id":"s-ref","agent":"claude","agent_base":"claude","cwd":"/work","cols":80,"rows":24,"native_session_id":"native-3","native_launch":{"reference_kind":"nonsense","resume_args":["--resume","{reference}"]}}"#;

/// Schema-1 lines the v0.33.0 reader would silently drop but the v0.33.1
/// reader keeps: the resume mode is a required enum at v0.33.0 and an
/// ignored field at v0.33.1, and a grammar-valid custom agent base is a
/// runtime id at v0.33.1 but an unknown `AgentKind` at v0.33.0.
const V0_33_0_DROPS: &[(&str, &str)] = &[
    (
        "resume with an unknown resume_mode",
        V0_33_0_UNKNOWN_RESUME_MODE,
    ),
    (
        "resume with a grammar-valid custom agent base",
        r#"{"kind":"resume","session_id":"s-base","agent":"claude-sonnet","agent_base":"claude-sonnet","cwd":"/work","cols":80,"rows":24,"native_session_id":"native-1"}"#,
    ),
    (
        "session with a grammar-valid custom agent base",
        concat!(
            r#"{"kind":"session","schema_version":1,"session_id":"s-base","desired_state":"stopped","#,
            r#""info":{"id":"s-base","agent":"claude-sonnet","agent_base":"claude-sonnet","cwd":"/work","pid":42,"cols":80,"rows":24,"state":"stopped","state_source":"process","created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"},"#,
            r#""runtime":{"state":"lost"}}"#,
        ),
    ),
    (
        "session with a grammar-valid custom nested agent base",
        concat!(
            r#"{"kind":"session","schema_version":1,"session_id":"s-nested","desired_state":"stopped","#,
            r#""info":{"id":"s-nested","agent":"claude","agent_base":"claude","cwd":"/work","pid":43,"cols":80,"rows":24,"state":"stopped","state_source":"process","#,
            r#""active_agent":"sonnet","active_agent_base":"claude-sonnet","created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"},"#,
            r#""runtime":{"state":"lost"}}"#,
        ),
    ),
    (
        "session with a grammar-valid custom recovery base",
        concat!(
            r#"{"kind":"session","schema_version":1,"session_id":"s-recovery","desired_state":"stopped","#,
            r#""info":{"id":"s-recovery","agent":"claude","agent_base":"claude","cwd":"/work","pid":44,"cols":80,"rows":24,"state":"stopped","state_source":"process","created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"},"#,
            r#""recovery":{"session_id":"s-recovery","agent":"claude-sonnet","agent_base":"claude-sonnet","cwd":"/work","cols":80,"rows":24,"native_session_id":"native-2"},"#,
            r#""runtime":{"state":"lost"}}"#,
        ),
    ),
];

/// Schema-1 lines the v0.33.1 reader would silently drop: v0.33.1 validates
/// the native launch snapshot its recovery bindings carry. The v0.33.0
/// reader deserializes each of these lines and refuses it under the same
/// gate for silently rewriting that snapshot away.
const V0_33_1_DROPS: &[(&str, &str)] = &[
    (
        "resume with a reference-less native launch",
        V0_33_1_REFERENCE_LESS_LAUNCH,
    ),
    (
        "resume with an undigested launch binding",
        V0_33_1_UNDIGESTED_LAUNCH_PIN,
    ),
    (
        "resume with an unknown native reference kind",
        V0_33_1_UNKNOWN_REFERENCE_KIND,
    ),
];

/// Schema-1 recovery bindings in the v0.33.1 relaunch-snapshot spellings
/// that the v0.33.0 reader parses, keeps, and still rewrites without the
/// snapshot — the valid snapshot and the lines v0.33.1 itself drops alike:
/// the preservation rule refuses a rollback that would silently lose the
/// native recovery on restore.
const V0_33_1_REWRITE_LOSS: &[(&str, &str)] = &[
    (
        "resume with a native launch snapshot",
        V0_33_1_LAUNCH_SNAPSHOT,
    ),
    (
        "session with a native launch recovery",
        V0_33_1_RECOVERY_LAUNCH,
    ),
    (
        "resume with a reference-less native launch",
        V0_33_1_REFERENCE_LESS_LAUNCH,
    ),
    (
        "resume with an undigested launch binding",
        V0_33_1_UNDIGESTED_LAUNCH_PIN,
    ),
    (
        "resume with an unknown native reference kind",
        V0_33_1_UNKNOWN_REFERENCE_KIND,
    ),
];

/// Schema-1 recovery bindings in the v0.33.0 relaunch-snapshot spellings
/// that the v0.33.1 reader parses, keeps, and still rewrites without the
/// snapshot, with no `native_launch` to keep the relaunch shape the reader
/// drops: the preservation rule refuses a rollback that would silently lose
/// the relaunch modes.
const V0_33_0_REWRITE_LOSS: &[(&str, &str)] = &[
    ("resume with relaunch modes", V0_33_0_MODE_SNAPSHOT),
    ("session with a relaunch recovery", V0_33_0_RECOVERY_MODES),
    (
        "resume with an unknown resume_mode",
        V0_33_0_UNKNOWN_RESUME_MODE,
    ),
];

/// A recovery binding in the v0.33.0 release's own relaunch snapshot
/// spellings: the v0.33.0 reader deserializes and keeps every field, while
/// the v0.33.1 reader has no `native_launch` to compensate for the relaunch
/// argv modes it silently rewrites away.
const V0_33_0_MODE_SNAPSHOT: &str = r#"{"kind":"resume","session_id":"s-modes","agent":"claude","agent_base":"claude","cwd":"/work","cols":80,"rows":24,"native_session_path":"/w/transcript.jsonl","ref_kind":"path","resumable":true,"resume_mode":"subcommand","forkable":true,"fork_mode":"claude_session","fork_resume_mode":"flag","fork_ref_kind":"id"}"#;

/// A session record whose nested `recovery` binding carries the v0.33.0
/// release's own relaunch snapshot spellings: kept by the v0.33.0 reader and
/// rewritten without the snapshot by the v0.33.1 reader.
const V0_33_0_RECOVERY_MODES: &str = concat!(
    r#"{"kind":"session","schema_version":1,"session_id":"s-modes","desired_state":"stopped","#,
    r#""info":{"id":"s-modes","agent":"claude","agent_base":"claude","cwd":"/work","pid":46,"cols":80,"rows":24,"state":"stopped","state_source":"process","native_session_path":"/w/transcript.jsonl","created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"},"#,
    r#""recovery":{"session_id":"s-modes","agent":"claude","agent_base":"claude","cwd":"/work","cols":80,"rows":24,"native_session_path":"/w/transcript.jsonl","ref_kind":"path","resumable":true,"resume_mode":"subcommand","forkable":true,"fork_mode":"claude_session","fork_resume_mode":"flag","fork_ref_kind":"id"},"#,
    r#""runtime":{"state":"lost"}}"#,
);

/// A recovery binding in the v0.33.1 release's own relaunch snapshot
/// spellings: the v0.33.0 reader deserializes it and still rewrites the
/// native launch spec and its launch pin away, so the rollback refuses it
/// instead of losing native recovery on restore.
const V0_33_1_LAUNCH_SNAPSHOT: &str = concat!(
    r#"{"kind":"resume","session_id":"s-launch","agent":"claude","agent_base":"claude","cwd":"/work","cols":80,"rows":24,"native_session_id":"native-1","#,
    r#""native_launch":{"reference_kind":"id","resume_args":[{"literal":"--resume"},"reference"]},"#,
    r#""launch_binding":{"runtime_id":"claude","provenance":{"kind":"builtin","descriptor_digest":"#,
    r#""sha256:0000000000000000000000000000000000000000000000000000000000000000"}}}"#,
);

/// A session record whose nested `recovery` binding carries the v0.33.1
/// release's own relaunch snapshot spellings: kept by the v0.33.1 reader and
/// rewritten without the native launch snapshot by the v0.33.0 reader.
const V0_33_1_RECOVERY_LAUNCH: &str = concat!(
    r#"{"kind":"session","schema_version":1,"session_id":"s-native","desired_state":"stopped","#,
    r#""info":{"id":"s-native","agent":"claude","agent_base":"claude","cwd":"/work","pid":45,"cols":80,"rows":24,"state":"stopped","state_source":"process","native_session_id":"native-2","created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"},"#,
    r#""recovery":{"session_id":"s-native","agent":"claude","agent_base":"claude","cwd":"/work","cols":80,"rows":24,"native_session_id":"native-2","#,
    r#""native_launch":{"reference_kind":"id","resume_args":[{"literal":"--resume"},"reference"]}},"#,
    r#""runtime":{"state":"lost"}}"#,
);

#[tokio::test]
async fn a_downgrade_is_judged_by_the_previous_release_reader_alone() {
    // The two released readers disagree on what a schema-1 record must
    // carry and which relaunch snapshot its recovery bindings preserve, so
    // each release's rollback is judged against that release's own reader.
    // With no live workers, a store line the previous reader would silently
    // rewrite away — a line it skips, or a relaunch snapshot it drops from a
    // recovery binding it keeps — refuses the read-only check and the
    // rollback before any effect, with and without runtime-loss consent; a
    // line its reader keeps rolls back.
    for (previous, refusals, verdict) in [
        ("0.33.0", V0_33_0_DROPS, "would be skipped"),
        ("0.33.1", V0_33_1_DROPS, "would be skipped"),
        (
            "0.33.0",
            V0_33_1_REWRITE_LOSS,
            "would drop its relaunch snapshot",
        ),
        (
            "0.33.1",
            V0_33_0_REWRITE_LOSS,
            "would drop its relaunch snapshot",
        ),
    ] {
        for (name, line) in refusals {
            let harness = Harness::new();
            pending_store_line_refuses(&harness, previous, name, line, verdict).await;
        }
    }

    // Where its own reader keeps the record, the rollback proceeds: the
    // v0.33.1 reader keeps every agent kind v0.33.0 drops at its runtime-id
    // spellings (the mode line refused above is not among them, rewritten
    // without its relaunch snapshot), and each reader keeps the relaunch
    // snapshot it froze itself, at a resume line and nested in a session
    // record alike.
    for (_, line) in &V0_33_0_DROPS[1..] {
        let harness = Harness::new();
        pending_store_line_keeps(&harness, "0.33.1", line).await;
    }
    for (previous, snapshots) in [
        ("0.33.0", [V0_33_0_MODE_SNAPSHOT, V0_33_0_RECOVERY_MODES]),
        ("0.33.1", [V0_33_1_LAUNCH_SNAPSHOT, V0_33_1_RECOVERY_LAUNCH]),
    ] {
        for snapshot in snapshots {
            let harness = Harness::new();
            pending_store_line_keeps(&harness, previous, snapshot).await;
        }
    }
}

#[tokio::test]
async fn uninstall_of_a_pending_schema_changing_upgrade_never_restores_the_previous_daemon() {
    // With session consent the removal proceeds through the session-checked
    // path, even though the rollback gate of this pending upgrade refuses the
    // migrated store: the previous release's reader reads nothing the new
    // daemon migrated, and restoring it would lose anything the migration
    // wrote. The uninstall therefore consumes the record without ever
    // contacting the previous reader or restarting the previous daemon.
    let harness = Harness::new();
    harness.fake.seed(
        vec![session(SESSION, Some("review"))],
        vec![worker(worker_id(SESSION), ServiceState::Running)],
    );
    pending_registered_upgrade_with_migrated_store(&harness).await;
    let (installs, replaces) = {
        let world = harness.fake.world();
        (world.installs, world.replaces)
    };

    let report = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: true,
        })
        .await
        .expect("uninstall removes the pending upgrade and the installation");
    assert_eq!(report.stopped_sessions, [SESSION]);
    assert!(report.purged);
    assert!(report.rolled_back.is_none(), "no rollback ran");
    harness.assert_clean();
    assert!(
        !harness
            .context
            .paths()
            .data_dir
            .join("metadata.jsonl")
            .exists(),
        "--purge removes the migrated store"
    );
    let after = {
        let world = harness.fake.world();
        (world.installs, world.replaces)
    };
    assert_eq!(after.0, installs, "the previous daemon was never restored");
    assert_eq!(after.1, replaces, "no replacement beyond the pending one");
    assert_eq!(harness.pending(), None, "the record is consumed");
}

#[tokio::test]
async fn uninstall_without_session_consent_still_refuses_a_live_pending_upgrade() {
    // Without consent the uninstall fails closed before touching anything.
    let harness = Harness::new();
    harness.fake.seed(
        vec![session(SESSION, Some("review"))],
        vec![worker(worker_id(SESSION), ServiceState::Running)],
    );
    let before = pending_registered_upgrade_with_migrated_store(&harness).await;

    let error = harness
        .engine()
        .uninstall(UninstallOptions {
            stop_sessions: false,
            purge: true,
        })
        .await
        .expect_err("live sessions refuse the uninstall");
    let Error::LiveSessions { sessions, .. } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, SESSION);
    assert_eq!(error.code(), "service_live_sessions");
    // The pending upgrade and the installation are untouched.
    assert_eq!(
        harness.pending(),
        Some(before),
        "the record survives for a consented rerun"
    );
    assert_eq!(harness.fake.daemon_version().as_deref(), Some(V2));
    assert_eq!(
        harness.config().expect("config").active_version(),
        V2,
        "the pending upgrade is not rolled back"
    );
    assert!(
        harness
            .context
            .paths()
            .data_dir
            .join("metadata.jsonl")
            .exists(),
        "the purged store is untouched"
    );
}

#[tokio::test]
async fn downgrade_refuses_an_untrusted_store_even_with_runtime_loss_consent() {
    for previous in ["0.33.0", "0.33.1"] {
        let harness = Harness::new();
        harness
            .install(previous)
            .await
            .expect("install previous release");
        let mut engine = harness.engine();
        engine.interrupt_after = Some(Step::Registered);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted after replacement");
        let store = store_path(&harness);
        write_new_store(&store);
        std::fs::set_permissions(&store, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .expect("make store untrusted");
        let before = harness.pending().expect("pending upgrade");
        let error = harness
            .upgrade_accepting_loss(previous)
            .await
            .expect_err("runtime-loss consent cannot override untrusted store");
        assert_eq!(error.code(), "service_rollback_store_unusable");
        assert!(error.to_string().contains("unreadable"));
        assert_eq!(harness.pending(), Some(before));
        assert_eq!(harness.fake.daemon_version().as_deref(), Some(V2));
    }
}

#[tokio::test]
async fn readiness_failure_keeps_transaction_when_previous_reader_cannot_read_store() {
    // Either legacy release refuses the schema-4 store the ready-not daemon
    // wrote during its replacement, so the failed upgrade keeps its record.
    for previous in ["0.33.0", "0.33.1"] {
        let harness = Harness::new();
        harness
            .install(previous)
            .await
            .expect("install previous release");
        {
            let mut world = harness.fake.world();
            world.silent_version = Some(V2.to_owned());
            world.store_on_replace = Some(store_path(&harness));
        };
        let error = harness
            .upgrade(V2)
            .await
            .expect_err("new daemon is not ready");
        let Error::RollbackFailed { original, rollback } = error else {
            panic!("unexpected failure {error:?}");
        };
        assert!(matches!(*original, Error::DaemonNotReady { .. }));
        assert_eq!(rollback.code(), "service_rollback_store_unusable");
        assert!(rollback.to_string().contains("schema"));
        assert_eq!(harness.fake.daemon_version().as_deref(), Some(V2));
        assert_eq!(harness.config().expect("config").active_version(), V2);
        assert_eq!(
            harness.pending().expect("transaction kept").step,
            Step::Registered
        );
    }
}

#[tokio::test]
async fn downgrade_rechecks_store_after_stopping_the_new_daemon() {
    for previous in ["0.33.0", "0.33.1"] {
        let harness = Harness::new();
        harness
            .install(previous)
            .await
            .expect("install previous release");
        let mut engine = harness.engine();
        engine.interrupt_after = Some(Step::Registered);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted after replacement");
        let installs = harness.fake.world().installs;
        harness.fake.world().store_on_uninstall = Some(store_path(&harness));

        let error = harness
            .upgrade_accepting_loss(previous)
            .await
            .expect_err("a final new-daemon write must be refused");
        assert_eq!(error.code(), "service_rollback_store_unusable");
        assert!(harness.pending().expect("transaction kept").rolling_back);
        assert_eq!(harness.fake.world().installs, installs);
        assert_eq!(harness.fake.daemon_version(), None);
        assert_eq!(harness.config().expect("config").active_version(), V2);
    }
}

#[tokio::test]
async fn accepted_legacy_downgrade_reports_its_preflight_and_runtime_loss() {
    for previous in ["0.33.0", "0.33.1"] {
        let harness = Harness::new();
        harness
            .install(previous)
            .await
            .expect("install previous release");
        let mut engine = harness.engine();
        engine.interrupt_after = Some(Step::Registered);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted after replacement");
        let executable = harness.layout().worker_executable(V2).expect("worker path");
        write_live_journal(harness.context.paths(), SESSION, "w-1", &executable);

        let refused = harness
            .upgrade(previous)
            .await
            .expect_err("live worker needs consent");
        assert_eq!(refused.code(), "service_rollback_sessions_at_risk");
        let report = harness
            .upgrade_accepting_loss(previous)
            .await
            .expect("accepted downgrade");
        assert!(report.unchanged);
        assert!(report.accepted_runtime_loss);
        let preflight = report.preflight.expect("rollback preflight");
        assert_eq!(preflight.daemon_version, previous);
        assert_eq!(preflight.store.state, StoreState::Missing);
        assert_eq!(preflight.sessions.len(), 1);
        assert_eq!(
            preflight.sessions[0].code,
            "legacy_worker_adoption_unverified"
        );
        let rolled_back = report.rolled_back.expect("rollback summary");
        assert!(rolled_back.accepted_runtime_loss);
        assert_eq!(rolled_back.preflight, Some(preflight));
        harness.assert_installed(previous);
    }
}

#[tokio::test]
async fn legacy_downgrade_refuses_an_incomplete_worker_inventory_before_stopping_the_daemon() {
    for previous in ["0.33.0", "0.33.1"] {
        let harness = Harness::new();
        harness
            .install(previous)
            .await
            .expect("install previous release");
        let mut engine = harness.engine();
        engine.interrupt_after = Some(Step::Registered);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted after replacement");
        let pending = harness.pending().expect("pending upgrade");
        let worker_exe = harness.layout().worker_executable(V2).expect("worker");
        harness.fake.world().raced_workers.push(pending_worker(
            SESSION,
            ServiceState::Starting,
            worker_exe,
        ));
        let (installs, replaces) = {
            let world = harness.fake.world();
            (world.installs, world.replaces)
        };

        for accept_runtime_loss in [false, true] {
            let error = if accept_runtime_loss {
                harness.upgrade_accepting_loss(previous).await
            } else {
                harness.upgrade(previous).await
            }
            .expect_err("incomplete inventory refuses even with runtime-loss consent");
            assert_eq!(error.code(), "service_rollback_preflight_failed");
            assert!(error.to_string().contains("changed during"), "{error:?}");
            assert_eq!(harness.pending(), Some(pending.clone()));
            assert_eq!(harness.config().expect("config").active_version(), V2);
            assert_eq!(harness.fake.daemon_version().as_deref(), Some(V2));
            assert!(harness.fake.world().running);
            assert_eq!(harness.fake.world().installs, installs);
            assert_eq!(harness.fake.world().replaces, replaces);
        }

        harness.fake.world().raced_workers.clear();
        let report = harness
            .upgrade(previous)
            .await
            .expect("complete inventory permits rollback");
        assert!(report.rolled_back.is_some());
        harness.assert_installed(previous);
    }
}

/// A preflight that fails the test when it is consulted.
#[derive(Debug)]
struct CountingFailure(Arc<AtomicUsize>);

impl AdoptionPreflight for CountingFailure {
    fn judge<'a>(
        &'a self,
        _context: &'a Context,
        dir: &'a VerifiedVersionDir,
        _search_path: &'a str,
        _variables: &'a BTreeMap<String, String>,
        _probe_budget: Option<Duration>,
        _plan: ReleasePlan,
    ) -> Judging<'a> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Err(Error::UpgradePreflightFailed {
                binary: dir.daemon(),
                detail: "must not be consulted".to_owned(),
            })
        })
    }
}

// Durable acceptance of upgrade risk (the accepted-risk ledger).

const SCOPE_A: &str = "v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SCOPE_A_CHANGED: &str = "v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A stopped session at risk whose verdict the daemon bound to `scope`.
fn stopped_at_risk(session: &str, code: &str, scope: Option<&str>) -> SessionVerdict {
    SessionVerdict {
        session_id: session.to_owned(),
        name: None,
        verdict: Verdict::WouldLoseRecovery,
        code: code.to_owned(),
        detail: "the runtime source is not served".to_owned(),
        acceptance_scope: scope.map(ToOwned::to_owned),
    }
}

fn risk_a() -> SessionVerdict {
    stopped_at_risk(OTHER_SESSION, "runtime_source_not_served", Some(SCOPE_A))
}

fn risk_b() -> SessionVerdict {
    stopped_at_risk(THIRD_SESSION, "runtime_source_not_served", Some(SCOPE_A))
}

fn judging(stopped: &[SessionVerdict]) -> Box<ScriptedPreflight> {
    Box::new(ScriptedPreflight {
        stopped: stopped.to_vec(),
        ..ScriptedPreflight::default()
    })
}

impl Harness {
    /// Upgrades to `version` over `stopped` with the given flag.
    async fn upgrade_over(
        &self,
        version: &str,
        stopped: &[SessionVerdict],
        accept: bool,
    ) -> Result<UpgradeReport, Error> {
        self.engine()
            .with_runtime_loss_accepted(accept)
            .with_adoption_preflight(judging(stopped))
            .upgrade(&self.staged(version), version)
            .await
    }

    fn ledger_bytes(&self) -> Option<Vec<u8>> {
        self.engine().store.read_ledger().expect("read the ledger")
    }

    fn ledger(&self) -> Option<Ledger> {
        self.ledger_bytes()
            .map(|bytes| serde_json::from_slice(&bytes).expect("a valid ledger"))
    }

    /// Replaces the ledger file with `edit` applied to the current one.
    fn edit_ledger(&self, edit: impl FnOnce(&mut serde_json::Value)) {
        let mut value: serde_json::Value =
            serde_json::from_slice(&self.ledger_bytes().expect("a ledger")).expect("ledger json");
        edit(&mut value);
        std::fs::write(
            self.engine().store.ledger_path(),
            serde_json::to_vec(&value).expect("serialize"),
        )
        .expect("write the ledger");
    }

    /// Every file of the state directory with its bytes and mode.
    fn state_snapshot(&self) -> std::collections::BTreeMap<PathBuf, (u32, Vec<u8>)> {
        use std::os::unix::fs::PermissionsExt as _;

        fn walk(path: &Path, into: &mut std::collections::BTreeMap<PathBuf, (u32, Vec<u8>)>) {
            let metadata = std::fs::symlink_metadata(path).expect("stat");
            let bytes = if metadata.is_file() {
                std::fs::read(path).expect("read")
            } else {
                Vec::new()
            };
            into.insert(path.to_path_buf(), (metadata.permissions().mode(), bytes));
            if metadata.is_dir() {
                for entry in std::fs::read_dir(path).expect("list") {
                    walk(&entry.expect("entry").path(), into);
                }
            }
        }
        let mut into = std::collections::BTreeMap::new();
        walk(&self.context.paths().state_dir, &mut into);
        into
    }
}

/// Installs V1 and upgrades to V2 accepting `risk_a`, leaving its ledger.
async fn harness_with_accepted_risk() -> Harness {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let report = harness
        .upgrade_over(V2, &[risk_a()], true)
        .await
        .expect("the flag accepts the risk");
    assert!(report.accepted_runtime_loss);
    assert_eq!(report.acceptance.durable_acceptance, [risk_a()]);
    harness
}

#[tokio::test]
async fn an_accepted_risk_is_not_asked_about_again_at_the_next_upgrade() {
    let harness = harness_with_accepted_risk().await;
    let ledger = harness.ledger().expect("the upgrade left a ledger");
    assert_eq!(ledger.applied_version, V2);
    assert_eq!(ledger.previous_version.as_deref(), Some(V1));
    assert_eq!(ledger.entries.len(), 1);
    assert_eq!(ledger.entries[0].session_id, OTHER_SESSION);
    assert_eq!(ledger.entries[0].scope, SCOPE_A);

    let report = harness
        .upgrade_over(V3, &[risk_a()], false)
        .await
        .expect("the identical risk was accepted at the previous upgrade");

    assert!(!report.accepted_runtime_loss, "the flag was not needed");
    assert_eq!(report.acceptance.previously_accepted, [risk_a()]);
    assert_eq!(
        report.acceptance.previously_accepted_at.as_deref(),
        Some(V2)
    );
    harness.assert_installed(V3);
    let carried = harness.ledger().expect("the ledger is replaced");
    assert_eq!(carried.applied_version, V3);
    assert_eq!(carried.entries, ledger.entries);
}

#[tokio::test]
async fn a_new_risk_beside_an_accepted_one_refuses_and_lists_only_the_new_one() {
    let harness = harness_with_accepted_risk().await;

    let error = harness
        .upgrade_over(V3, &[risk_a(), risk_b()], false)
        .await
        .expect_err("the second session was never accepted");

    let Error::UpgradeAtRisk {
        sessions,
        stopped_sessions,
        acceptance,
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert!(sessions.is_empty());
    assert_eq!(stopped_sessions, &[risk_b()]);
    assert_eq!(acceptance.previously_accepted, [risk_a()]);
    assert!(
        error
            .to_string()
            .contains("previously accepted at upgrade to"),
        "{error}"
    );
    harness.assert_installed(V2);
}

#[tokio::test]
async fn a_risk_whose_scope_changed_is_asked_about_again() {
    let harness = harness_with_accepted_risk().await;
    let changed = stopped_at_risk(
        OTHER_SESSION,
        "runtime_source_not_served",
        Some(SCOPE_A_CHANGED),
    );

    let error = harness
        .upgrade_over(V3, std::slice::from_ref(&changed), false)
        .await
        .expect_err("the daemon bound the verdict to other facts");

    let Error::UpgradeAtRisk {
        stopped_sessions, ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(stopped_sessions, &[changed]);
    harness.assert_installed(V2);
}

#[tokio::test]
async fn a_check_with_the_flag_changes_nothing_on_disk() {
    let harness = harness_with_accepted_risk().await;
    let before = harness.state_snapshot();

    let checked = crate::service::check_with_preflight(
        &harness.context,
        &harness.staged(V3),
        &ScriptedPreflight {
            stopped: vec![risk_a(), risk_b()],
            ..ScriptedPreflight::default()
        },
        V3,
        true,
    )
    .await
    .expect("the flag accepts the new risk in a check");

    assert_eq!(checked.acceptance.previously_accepted, [risk_a()]);
    assert_eq!(checked.acceptance.durable_acceptance, [risk_b()]);
    assert_eq!(harness.state_snapshot(), before, "a check writes no ledger");
}

#[tokio::test]
async fn a_refused_or_failed_upgrade_leaves_the_earlier_ledger_untouched() {
    let harness = harness_with_accepted_risk().await;
    let before = harness.ledger_bytes();

    harness
        .upgrade_over(V3, &[risk_a(), risk_b()], false)
        .await
        .expect_err("refused at the gate");
    assert_eq!(harness.ledger_bytes(), before, "a refusal writes nothing");

    // The new daemon definition is refused after the gate passed with the
    // flag, so the transaction rolls back.
    harness.fake.world().refused_replace = Some(V3.to_owned());
    harness
        .upgrade_over(V3, &[risk_a(), risk_b()], true)
        .await
        .expect_err("the replacement fails and rolls back");
    assert_eq!(harness.pending(), None);
    harness.assert_installed(V2);
    assert_eq!(harness.ledger_bytes(), before, "a rollback writes nothing");
}

#[tokio::test]
async fn an_interrupted_accepting_upgrade_resumes_to_the_same_ledger() {
    let reference = harness_with_accepted_risk().await;
    let expected = reference.ledger().expect("a ledger");

    for interrupted_at in [Step::Registering, Step::Registered, Step::Ready, Step::Cli] {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let mut engine = harness
            .engine()
            .with_runtime_loss_accepted(true)
            .with_adoption_preflight(judging(&[risk_a()]));
        engine.interrupt_after = Some(interrupted_at);
        engine
            .upgrade(&harness.staged(V2), V2)
            .await
            .expect_err("interrupted");
        assert_eq!(
            harness.ledger_bytes(),
            None,
            "{interrupted_at:?}: not final"
        );
        let journaled = harness
            .pending()
            .expect("the record is kept")
            .accepted_risks
            .expect("the gate journaled its accepted set");
        assert_eq!(journaled.entries.len(), 1, "{interrupted_at:?}");

        harness
            .upgrade_over(V2, &[risk_a()], true)
            .await
            .expect("the resume completes");

        let mut ledger = harness.ledger().expect("the resume wrote the ledger");
        // Each harness has its own roots and clock.
        ledger.applied_at.clone_from(&expected.applied_at);
        ledger.namespace.clone_from(&expected.namespace);
        ledger.store_path.clone_from(&expected.store_path);
        assert_eq!(ledger, expected, "{interrupted_at:?}");
    }

    // A crash after the ledger write and before the record is cleared repeats
    // the same replace.
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness
        .engine()
        .with_runtime_loss_accepted(true)
        .with_adoption_preflight(judging(&[risk_a()]));
    engine.interrupt_after = Some(Step::Cli);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted");
    let record = harness.pending().expect("the record is kept");
    harness
        .engine()
        .write_ledger(&record)
        .expect("the write that precedes the clear");
    let written = harness.ledger_bytes();
    harness
        .upgrade_over(V2, &[risk_a()], true)
        .await
        .expect("the resume completes");
    assert_eq!(harness.ledger_bytes(), written);
}

#[tokio::test]
async fn a_ledger_of_a_rolled_back_transaction_is_inert() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    harness.upgrade_over(V2, &[], false).await.expect("upgrade");
    let mut engine = harness
        .engine()
        .with_runtime_loss_accepted(true)
        .with_adoption_preflight(judging(&[risk_a()]));
    engine.interrupt_after = Some(Step::Cli);
    engine
        .upgrade(&harness.staged(V3), V3)
        .await
        .expect_err("interrupted at the last step");
    let record = harness.pending().expect("the record is kept");
    harness
        .engine()
        .write_ledger(&record)
        .expect("the ledger is written before the clear");
    assert_eq!(harness.ledger().expect("a ledger").applied_version, V3);

    // Another version rolls the Ready record back to V2, which the ledger no
    // longer names, and then judges the risk without it.
    let error = harness
        .upgrade_over("4.0.0", &[risk_a()], false)
        .await
        .expect_err("the ledger names V3, which the rollback undid");

    let Error::UpgradeAtRisk {
        stopped_sessions,
        warnings,
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(stopped_sessions, &[risk_a()]);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("accepted-risk ledger")),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn a_verdict_without_a_scope_is_a_one_time_acceptance_and_not_recorded() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let unscoped = stopped_at_risk(OTHER_SESSION, "probe_budget_exhausted", None);
    let scoped = risk_b();

    let report = harness
        .upgrade_over(V2, &[unscoped.clone(), scoped.clone()], true)
        .await
        .expect("the flag accepts both");

    assert_eq!(report.acceptance.one_time_acceptance, [unscoped]);
    assert_eq!(report.acceptance.durable_acceptance, [scoped]);
    let ledger = harness.ledger().expect("a ledger");
    assert_eq!(ledger.entries.len(), 1);
    assert_eq!(ledger.entries[0].session_id, THIRD_SESSION);
    let error = harness
        .upgrade_over(
            V3,
            &[stopped_at_risk(
                OTHER_SESSION,
                "probe_budget_exhausted",
                None,
            )],
            false,
        )
        .await
        .expect_err("a one-time acceptance is asked about again");
    assert!(matches!(error, Error::UpgradeAtRisk { .. }), "{error:?}");
}

#[tokio::test]
async fn a_scope_of_an_unknown_scheme_or_malformed_digest_is_never_recorded_or_honoured() {
    let uppercase = format!("v1:{}", "A".repeat(64));
    let short = format!("v1:{}", "a".repeat(63));
    let unknown_scheme = format!("v2:{}", "a".repeat(64));
    let not_hex = format!("v1:{}", "g".repeat(64));
    for scope in [
        &uppercase,
        &short,
        &unknown_scheme,
        &not_hex,
        &String::new(),
    ] {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let risk = stopped_at_risk(OTHER_SESSION, "runtime_source_not_served", Some(scope));

        let report = harness
            .upgrade_over(V2, std::slice::from_ref(&risk), true)
            .await
            .expect("the flag accepts it once");
        assert_eq!(
            report.acceptance.one_time_acceptance,
            std::slice::from_ref(&risk),
            "{scope}"
        );
        assert!(report.acceptance.durable_acceptance.is_empty(), "{scope}");
        assert!(
            harness
                .ledger()
                .is_none_or(|ledger| ledger.entries.is_empty()),
            "{scope}"
        );

        // Even a ledger that names the scope verbatim does not cover it.
        harness.edit_ledger(|ledger| {
            ledger["entries"] = serde_json::json!([{
                "session_id": OTHER_SESSION,
                "list": "stopped",
                "verdict": "would_lose_recovery",
                "code": "runtime_source_not_served",
                "scope": scope,
            }]);
        });
        let error = harness
            .upgrade_over(V3, &[risk], false)
            .await
            .expect_err("an unsupported scope is asked about again");
        assert!(
            matches!(error, Error::UpgradeAtRisk { .. }),
            "{scope}: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_ledger_for_other_facts_is_ignored_with_a_warning_and_refuses() {
    type Edit = fn(&mut serde_json::Value);
    let edits: [(&str, Edit); 5] = [
        ("version", |ledger| {
            ledger["applied_version"] = "9.9.9".into();
        }),
        ("namespace", |ledger| ledger["namespace"] = "another".into()),
        ("store path", |ledger| {
            ledger["store_path"] = "/elsewhere/metadata.jsonl".into();
        }),
        ("scope scheme", |ledger| {
            ledger["scope_scheme"] = "v2".into();
        }),
        ("schema", |ledger| {
            ledger["schema_version"] = 2.into();
            ledger["unknown_future_field"] = true.into();
        }),
    ];
    for (name, edit) in edits {
        let harness = harness_with_accepted_risk().await;
        harness.edit_ledger(edit);

        let error = harness
            .upgrade_over(V3, &[risk_a()], false)
            .await
            .expect_err(name);

        let Error::UpgradeAtRisk {
            stopped_sessions,
            warnings,
            ..
        } = &error
        else {
            panic!("{name}: {error:?}");
        };
        assert_eq!(stopped_sessions, &[risk_a()], "{name}");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("accepted-risk ledger")),
            "{name}: {warnings:?}"
        );
        harness.assert_installed(V2);

        // With the flag the upgrade and the check pass, and both still say why
        // the ledger did not apply.
        let checked = crate::service::check_with_preflight(
            &harness.context,
            &harness.staged(V3),
            &ScriptedPreflight {
                stopped: vec![risk_a()],
                ..ScriptedPreflight::default()
            },
            V3,
            true,
        )
        .await
        .expect(name);
        assert!(
            checked
                .warnings
                .iter()
                .any(|warning| warning.contains("accepted-risk ledger")),
            "{name}: {:?}",
            checked.warnings
        );
        let report = harness
            .upgrade_over(V3, &[risk_a()], true)
            .await
            .expect(name);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("accepted-risk ledger")),
            "{name}: {:?}",
            report.warnings
        );
        assert_eq!(report.acceptance.durable_acceptance, [risk_a()], "{name}");
    }
}

#[tokio::test]
async fn a_corrupt_or_unsafe_ledger_is_a_typed_error_that_names_the_file() {
    use std::os::unix::fs::PermissionsExt as _;

    let corrupt = harness_with_accepted_risk().await;
    std::fs::write(corrupt.engine().store.ledger_path(), b"{ not json")
        .expect("corrupt the ledger");
    let error = corrupt
        .upgrade_over(V3, &[risk_a()], true)
        .await
        .expect_err("a corrupt ledger is not guessed at");
    assert_eq!(error.code(), "service_accepted_risks_invalid");
    assert!(
        error.to_string().contains("service-accepted-risks.json"),
        "{error}"
    );
    corrupt.assert_installed(V2);

    let unsafe_mode = harness_with_accepted_risk().await;
    std::fs::set_permissions(
        unsafe_mode.engine().store.ledger_path(),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("loosen the mode");
    let error = unsafe_mode
        .upgrade_over(V3, &[risk_a()], true)
        .await
        .expect_err("a ledger others can read is refused");
    assert_eq!(error.code(), "service_accepted_risks_invalid");
}

#[tokio::test]
async fn a_schema_4_record_resumes_and_loads_without_an_accepted_set() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted");
    let path = harness.engine().store.path();
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read the record"))
            .expect("record json");
    value["schema_version"] = 4.into();
    value
        .as_object_mut()
        .expect("object")
        .remove("accepted_risks");
    std::fs::write(&path, serde_json::to_vec(&value).expect("serialize"))
        .expect("write the schema 4 record");
    assert_eq!(harness.pending().expect("loads").accepted_risks, None);

    harness
        .upgrade(V2)
        .await
        .expect("the schema 4 record resumes");

    harness.assert_installed(V2);
    assert_eq!(harness.ledger_bytes(), None, "no gate ran, so no ledger");
}

#[tokio::test]
async fn the_rollback_gate_never_consults_the_ledger() {
    let harness = harness_with_accepted_risk().await;
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .with_runtime_loss_accepted(true)
        .with_adoption_preflight(judging(&[risk_a()]))
        .upgrade(&harness.staged(V3), V3)
        .await
        .expect_err("interrupted after replacement");

    let error = harness
        .engine()
        .with_adoption_preflight(judging(&[risk_a()]))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("the previous release emits no scope, so nothing is accepted");

    assert_eq!(error.code(), "service_rollback_sessions_at_risk");
}

/// The ways an installed version directory can stop being what publishing made.
#[derive(Debug, Clone, Copy)]
enum Tamper {
    GroupWritableDirectory,
    GroupWritableDaemon,
    GroupWritableWorker,
    SymlinkedDaemon,
    HardLinkedDaemon,
    MissingWorker,
    WritableAnchor,
    WritableRuntimeEntry,
    SymlinkedRuntimeEntry,
}

impl Tamper {
    const ALL: [Self; 9] = [
        Self::GroupWritableDirectory,
        Self::GroupWritableDaemon,
        Self::GroupWritableWorker,
        Self::SymlinkedDaemon,
        Self::HardLinkedDaemon,
        Self::MissingWorker,
        Self::WritableAnchor,
        Self::WritableRuntimeEntry,
        Self::SymlinkedRuntimeEntry,
    ];

    /// Applies the change to `dir` and returns the path verification must name.
    fn apply(self, dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let chmod = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        };
        let daemon = dir.join(pohunek_paths::DAEMON_EXECUTABLE_NAME);
        let worker = dir.join(pohunek_paths::WORKER_EXECUTABLE_NAME);
        match self {
            Self::GroupWritableDirectory => {
                chmod(dir, 0o775);
                dir.to_path_buf()
            }
            Self::GroupWritableDaemon => {
                chmod(&daemon, 0o775);
                daemon
            }
            Self::GroupWritableWorker => {
                chmod(&worker, 0o775);
                worker
            }
            Self::SymlinkedDaemon => {
                let prefix = dir.ancestors().nth(3).expect("the prefix");
                let elsewhere = prefix.join("daemon.elsewhere");
                std::fs::rename(&daemon, &elsewhere).expect("move the daemon away");
                std::os::unix::fs::symlink(&elsewhere, &daemon).expect("link the daemon");
                daemon
            }
            Self::HardLinkedDaemon => {
                let prefix = dir.ancestors().nth(3).expect("the prefix");
                std::fs::hard_link(&daemon, prefix.join("daemon.link")).expect("link the daemon");
                daemon
            }
            Self::MissingWorker => {
                std::fs::remove_file(&worker).expect("remove the worker");
                worker
            }
            Self::WritableAnchor => {
                let anchor = dir.join(package::ANCHOR_FILE_NAME);
                std::fs::write(&anchor, b"anchor").expect("write the anchor");
                chmod(&anchor, 0o666);
                anchor
            }
            Self::WritableRuntimeEntry => {
                let runtime = dir.join("runtime");
                std::fs::create_dir(&runtime).expect("runtime directory");
                chmod(&runtime, 0o755);
                let entry = runtime.join("entry");
                std::fs::write(&entry, b"entry").expect("write the entry");
                chmod(&entry, 0o666);
                entry
            }
            Self::SymlinkedRuntimeEntry => {
                let runtime = dir.join("runtime");
                std::fs::create_dir(&runtime).expect("runtime directory");
                chmod(&runtime, 0o755);
                let entry = runtime.join("entry");
                std::os::unix::fs::symlink(&daemon, &entry).expect("link the entry");
                entry
            }
        }
    }
}

fn assert_untrusted_version(error: &Error, named: &Path) {
    // A rollback that cannot remove the tampered directory still reports the
    // refusal that started it.
    let error = match error {
        Error::RollbackFailed { original, .. } => original,
        other => other,
    };
    assert_eq!(error.code(), "service_version_untrusted", "{error:?}");
    assert!(
        matches!(error, Error::UntrustedVersion { path, .. } if path == named),
        "the refusal names {}: {error:?}",
        named.display()
    );
}

#[tokio::test]
async fn an_upgrade_runs_nothing_from_a_new_version_directory_that_stopped_being_trusted() {
    for tamper in Tamper::ALL {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        harness.runtimes.script().calls.clear();
        let preflight = ScriptedPreflight::default();
        let judged = Arc::clone(&preflight.search_paths);
        let mut engine = harness
            .engine()
            .with_adoption_preflight(Box::new(preflight));
        let version_dir = harness.layout().version_dir(V2).expect("version dir");
        let named = Arc::new(Mutex::new(PathBuf::new()));
        let recorded = Arc::clone(&named);
        engine.before_swap = Some(SwapHook(Box::new(move || {
            *recorded.lock().expect("named path") = tamper.apply(&version_dir);
        })));

        let error = harness
            .upgrade_with(engine, V2)
            .await
            .expect_err("an untrusted version directory is refused");

        let named = named.lock().expect("named path").clone();
        assert_untrusted_version(&error, &named);
        assert!(
            harness.runtimes.calls().is_empty(),
            "{tamper:?}: no runtime step ran"
        );
        assert_eq!(
            judged.lock().expect("search path lock").len(),
            1,
            "{tamper:?}: only the gate on the staged source judged"
        );
    }
}

#[tokio::test]
async fn a_rollback_judges_nothing_from_a_previous_version_directory_that_stopped_being_trusted() {
    for tamper in Tamper::ALL {
        let mut harness = Harness::new();
        interrupted_upgrade_journal(&mut harness).await;
        let named = tamper.apply(&harness.layout().version_dir(V1).expect("version dir"));
        let preflight = ScriptedPreflight::default();
        let judged = Arc::clone(&preflight.search_paths);

        let error = harness
            .engine()
            .with_adoption_preflight(Box::new(preflight))
            .upgrade(&harness.staged(V1), V1)
            .await
            .expect_err("an untrusted previous version is not run");

        assert_untrusted_version(&error, &named);
        assert!(
            judged.lock().expect("search path lock").is_empty(),
            "{tamper:?}: the rollback gate ran nothing"
        );
    }
}

#[tokio::test]
async fn a_check_of_a_pending_rollback_runs_nothing_from_a_previous_version_directory_that_stopped_being_trusted(
) {
    for tamper in Tamper::ALL {
        let mut harness = Harness::new();
        interrupted_upgrade_journal(&mut harness).await;
        let named = tamper.apply(&harness.layout().version_dir(V1).expect("version dir"));
        let preflight = ScriptedPreflight::default();
        let judged = Arc::clone(&preflight.search_paths);

        let error = super::super::check_with_preflight(
            &harness.context,
            &harness.staged(V1),
            &preflight,
            V1,
            false,
        )
        .await
        .expect_err("an untrusted previous version is not run");

        assert_untrusted_version(&error, &named);
        assert!(
            judged.lock().expect("search path lock").is_empty(),
            "{tamper:?}: the check ran nothing"
        );
    }
}

#[tokio::test]
async fn an_upgrade_or_check_from_a_retained_version_directory_verifies_it_first() {
    for tamper in Tamper::ALL {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let retained = harness.layout().version_dir(V1).expect("version dir");
        let named = tamper.apply(&retained);
        let preflight = ScriptedPreflight::default();
        let judged = Arc::clone(&preflight.search_paths);

        let upgrade = harness
            .engine()
            .with_adoption_preflight(Box::new(preflight))
            .upgrade(&retained, V2)
            .await
            .expect_err("an untrusted retained directory is refused");
        assert_untrusted_version(&upgrade, &named);

        let check = super::super::check_with_preflight(
            &harness.context,
            &retained,
            &ScriptedPreflight::default(),
            V2,
            false,
        )
        .await
        .expect_err("an untrusted retained directory is refused");
        assert_untrusted_version(&check, &named);
        assert!(
            judged.lock().expect("search path lock").is_empty(),
            "{tamper:?}: nothing was judged"
        );
    }
}

#[tokio::test]
async fn a_release_directory_that_resolves_into_a_version_directory_is_judged_as_that_directory_or_refused(
) {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let versions = harness.layout().versions_dir();
    let version_dir = harness.layout().version_dir(V1).expect("version dir");
    std::fs::create_dir(version_dir.join("nested")).expect("nested directory");
    let aliases = harness.root.join("aliases");
    std::fs::create_dir(&aliases).expect("aliases");
    let cases = [
        ("a link to a nested directory", version_dir.join("nested")),
        ("a link to the versions directory", versions.clone()),
    ];
    for (label, target) in cases {
        let alias = aliases.join(label.replace(' ', "-"));
        std::os::unix::fs::symlink(&target, &alias).expect("alias");
        let preflight = ScriptedPreflight::default();
        let judged = Arc::clone(&preflight.search_paths);

        let error = harness
            .engine()
            .with_adoption_preflight(Box::new(preflight))
            .upgrade(&alias, V2)
            .await
            .expect_err("the alias is not a version directory");

        assert_eq!(
            error.code(),
            "service_version_untrusted",
            "{label}: {error:?}"
        );
        assert!(
            judged.lock().expect("search path lock").is_empty(),
            "{label}: nothing was judged"
        );
    }

    let alias = aliases.join("exact");
    std::os::unix::fs::symlink(&version_dir, &alias).expect("alias");
    let verified = layout::operator_release(&harness.layout(), &alias).expect("an exact alias");
    assert_eq!(
        verified.path(),
        version_dir,
        "the alias resolves to the version directory"
    );
}

/// Makes every binary of `dir` leave a marker when it runs.
fn mark_runs(dir: &Path, marker: &Path) {
    for binary in layout::BINARIES {
        pohunek_test_support::fs::write_executable(
            dir.join(binary),
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .expect("marking binary");
    }
}

#[tokio::test]
async fn an_upgrade_without_a_daemon_swap_runs_nothing_from_an_untrusted_source_version_directory()
{
    for tamper in Tamper::ALL {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let source = harness.layout().version_dir(V1).expect("version dir");
        let marker = harness.root.join("ran");
        mark_runs(&source, &marker);
        let named = tamper.apply(&source);

        let error = harness
            .engine()
            .upgrade(&source, V1)
            .await
            .expect_err("an untrusted source is refused");

        assert_untrusted_version(&error, &named);
        assert!(!marker.exists(), "{tamper:?}: a source binary ran");
    }
}

#[tokio::test]
async fn an_install_from_an_untrusted_version_directory_runs_nothing_from_it() {
    for tamper in Tamper::ALL {
        let harness = Harness::new();
        let source = harness.layout().version_dir(V1).expect("version dir");
        crate::service::context::tests::make_dirs(&harness.root, &source);
        let marker = harness.root.join("ran");
        mark_runs(&source, &marker);
        let named = tamper.apply(&source);

        let error = harness
            .engine()
            .install(&source, &harness.prefix(), V1)
            .await
            .expect_err("an untrusted source is refused");

        assert_untrusted_version(&error, &named);
        assert!(!marker.exists(), "{tamper:?}: a source binary ran");
    }
}

#[tokio::test]
async fn an_upgrade_from_another_retained_version_directory_verifies_it_first() {
    for tamper in Tamper::ALL {
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let retained = harness.layout().version_dir("0.9.0").expect("version dir");
        crate::service::context::tests::make_dirs(&harness.root, &retained);
        for binary in layout::BINARIES {
            crate::service::layout::tests::write_fake(&retained, binary, "0.9.0");
        }
        let named = tamper.apply(&retained);
        let preflight = ScriptedPreflight::default();
        let judged = Arc::clone(&preflight.search_paths);

        let error = harness
            .engine()
            .with_adoption_preflight(Box::new(preflight))
            .upgrade(&retained, V2)
            .await
            .expect_err("an untrusted retained directory is refused");

        assert_untrusted_version(&error, &named);
        assert!(
            judged.lock().expect("search path lock").is_empty(),
            "{tamper:?}: nothing was judged"
        );
    }
}

#[tokio::test]
async fn an_intact_retained_version_directory_passes_the_verification_and_fails_later_on_its_content(
) {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let retained = harness.layout().version_dir("0.9.0").expect("version dir");
    crate::service::context::tests::make_dirs(&harness.root, &retained);
    for binary in layout::BINARIES {
        crate::service::layout::tests::write_fake(&retained, binary, "0.9.0");
    }

    let error = harness
        .engine()
        .upgrade(&retained, V2)
        .await
        .expect_err("the old release does not report the new version");

    assert_ne!(error.code(), "service_version_untrusted", "{error:?}");
}

#[tokio::test]
async fn a_source_that_is_not_exactly_a_version_directory_is_refused_before_anything_runs() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let versions = harness.layout().versions_dir();
    let version_dir = harness.layout().version_dir(V1).expect("version dir");
    std::fs::create_dir(version_dir.join("nested")).expect("nested directory");
    let scratch = versions.join(".staging-left-over");
    std::fs::create_dir(&scratch).expect("staging-shaped directory");
    std::fs::set_permissions(
        &scratch,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod");
    let aliases = harness.root.join("aliases");
    std::fs::create_dir(&aliases).expect("aliases");
    let dangling = aliases.join("dangling");
    std::os::unix::fs::symlink(harness.root.join("missing"), &dangling).expect("dangling alias");
    let looping = aliases.join("looping");
    std::os::unix::fs::symlink(&looping, &looping).expect("looping alias");
    let sources = [
        ("the versions directory", versions.clone()),
        ("a nested directory", version_dir.join("nested")),
        ("a staging-shaped directory", scratch),
        ("a dangling link", dangling),
        ("a looping link", looping),
    ];
    for (label, source) in sources {
        for install in [false, true] {
            let preflight = ScriptedPreflight::default();
            let judged = Arc::clone(&preflight.search_paths);
            let engine = harness
                .engine()
                .with_adoption_preflight(Box::new(preflight));

            let error = if install {
                engine
                    .install(&source, &harness.prefix(), V1)
                    .await
                    .map(drop)
            } else {
                engine.upgrade(&source, V2).await.map(drop)
            }
            .expect_err("not a version directory");

            let error = match &error {
                Error::RollbackFailed { original, .. } => original,
                other => other,
            };
            assert!(
                matches!(
                    error,
                    Error::UntrustedVersion { .. } | Error::AlreadyInstalled { .. }
                ),
                "{label} (install={install}): {error:?}"
            );
            assert!(
                judged.lock().expect("search path lock").is_empty(),
                "{label}: nothing was judged"
            );
        }
    }
}

#[tokio::test]
async fn a_check_classifies_the_source_in_every_adoption_mode() {
    for tamper in Tamper::ALL {
        // Same version: no daemon swap, so no adoption gate.
        let harness = Harness::new();
        harness.install(V1).await.expect("install");
        let source = harness.layout().version_dir(V1).expect("version dir");
        let named = tamper.apply(&source);
        let error = super::super::check_with_preflight(
            &harness.context,
            &source,
            &ScriptedPreflight::default(),
            V1,
            false,
        )
        .await
        .expect_err("an untrusted source is refused");
        assert_untrusted_version(&error, &named);

        // Fresh install from a retained prefix.
        let harness = Harness::new();
        let prefix = harness.context.default_prefix().expect("default prefix");
        let source = InstallLayout::new(prefix)
            .expect("layout")
            .version_dir(V1)
            .expect("version dir");
        crate::service::context::tests::make_dirs(&harness.root, &source);
        for binary in layout::BINARIES {
            crate::service::layout::tests::write_fake(&source, binary, V1);
        }
        let named = tamper.apply(&source);
        let error = super::super::check_with_preflight(
            &harness.context,
            &source,
            &ScriptedPreflight::default(),
            V1,
            false,
        )
        .await
        .expect_err("an untrusted source is refused");
        assert_untrusted_version(&error, &named);
    }
}

#[tokio::test]
async fn a_link_chain_through_the_versions_directory_is_verified_hop_by_hop_on_every_command() {
    let harness = Harness::new();
    harness.install(V1).await.expect("install");
    let versions = harness.layout().versions_dir();
    let version_dir = harness.layout().version_dir(V1).expect("version dir");
    let aliases = harness.root.join("aliases");
    std::fs::create_dir(&aliases).expect("aliases");
    let link = |target: &Path, name: &str| {
        let alias = aliases.join(name);
        std::os::unix::fs::symlink(target, &alias).expect("link");
        alias
    };

    // A version directory that is itself a link to a directory outside.
    let outside = harness.root.join("outside");
    std::fs::create_dir(&outside).expect("outside directory");
    let marker = harness.root.join("ran");
    mark_runs(&outside, &marker);
    std::os::unix::fs::symlink(&outside, versions.join("9.9.9")).expect("version link");
    let to_outward = link(&versions.join("9.9.9"), "to-outward-version");
    let two_hops = link(&to_outward, "two-hops");

    // A link to the versions directory, then a nested path and a tampered version.
    let dir_link = link(&versions, "dir-link");
    let nested_dir = version_dir.join("nested");
    std::fs::create_dir(&nested_dir).expect("nested directory");
    let nested = dir_link.join(V1).join("nested");

    // Loops and chains beyond the hop limit.
    let loop_a = aliases.join("loop-a");
    let loop_b = aliases.join("loop-b");
    std::os::unix::fs::symlink(&loop_b, &loop_a).expect("loop a");
    std::os::unix::fs::symlink(&loop_a, &loop_b).expect("loop b");
    let mut previous = version_dir.clone();
    for index in 0..=crate::service::settings::MAX_SOURCE_LINK_HOPS {
        previous = link(&previous, &format!("chain-{index}"));
    }

    let refused = [
        ("an alias to a version link pointing outside", to_outward),
        ("a chain of aliases to a version link", two_hops),
        ("a path below a link to the versions directory", nested),
        ("a link loop", loop_a),
        ("a chain beyond the hop limit", previous),
    ];
    for (label, source) in &refused {
        let upgrade = harness.engine().upgrade(source, V2).await.expect_err(label);
        let install = harness
            .engine()
            .install(source, &harness.prefix(), V1)
            .await
            .expect_err(label);
        let check = super::super::check_with_preflight(
            &harness.context,
            source,
            &ScriptedPreflight::default(),
            V2,
            false,
        )
        .await
        .expect_err(label);
        for error in [upgrade, install, check] {
            assert_eq!(
                error.code(),
                "service_version_untrusted",
                "{label}: {error:?}"
            );
        }
    }
    assert!(!marker.exists(), "nothing ran from the outside directory");

    // A tampered version reached through the directory link is refused too.
    let named = Tamper::GroupWritableDirectory.apply(&version_dir);
    let through = dir_link.join(V1);
    let error = harness
        .engine()
        .upgrade(&through, V2)
        .await
        .expect_err("tampered");
    assert_untrusted_version(&error, &named);
    std::fs::set_permissions(
        &version_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("restore the mode");

    // An intact chain resolves to the canonical version directory.
    let chain = link(&link(&version_dir, "first"), "second");
    let verified = layout::operator_release(&harness.layout(), &chain).expect("an intact chain");
    assert_eq!(verified.path(), version_dir);
    let through = layout::operator_release(&harness.layout(), &dir_link.join(V1))
        .expect("through the directory link");
    assert_eq!(through.path(), version_dir);

    // A chain that never touches the versions directory is the operator's own.
    let elsewhere = harness.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    let own = layout::operator_release(&harness.layout(), &link(&elsewhere, "own"))
        .expect("an operator directory");
    assert_ne!(own.path(), version_dir);
}

/// Counts the plans the setup asks the release daemon for.
#[derive(Debug, Default)]
struct CountingPlanner(AtomicUsize);

impl crate::commands::setup_host::ReleasePlanner for CountingPlanner {
    fn plan<'a>(
        &'a self,
        _context: &'a Context,
        _dir: &'a VerifiedVersionDir,
        _search_path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<EnsureReport, Error>> + Send + 'a>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(runtime_report(
                true,
                vec![package(PackageStatus::Present)],
                Vec::new(),
                Vec::new(),
            ))
        })
    }
}

#[tokio::test]
async fn setup_plans_nothing_from_an_installed_version_directory_that_stopped_being_trusted() {
    for tamper in Tamper::ALL {
        let harness = Harness::new();
        harness.install(VERSION).await.expect("install");
        let named = tamper.apply(&harness.layout().version_dir(VERSION).expect("version dir"));
        let planner = CountingPlanner::default();

        let error =
            crate::commands::setup_host::plan(&harness.context, &harness.staged(VERSION), &planner)
                .await
                .expect_err("an untrusted version directory is refused");

        assert_eq!(error.code(), "service_version_untrusted", "{tamper:?}");
        assert!(
            error.to_string().contains(&named.display().to_string()),
            "{tamper:?}: the refusal names {}: {error}",
            named.display()
        );
        assert_eq!(
            planner.0.load(Ordering::SeqCst),
            0,
            "{tamper:?}: the release daemon was not asked for a plan"
        );
    }
}

#[tokio::test]
async fn setup_applies_nothing_when_the_version_directory_stopped_being_trusted_after_the_preview()
{
    for tamper in Tamper::ALL {
        let harness = Harness::new();
        harness.install(VERSION).await.expect("install");
        let planner = CountingPlanner::default();
        let host_plan =
            crate::commands::setup_host::plan(&harness.context, &harness.staged(VERSION), &planner)
                .await
                .expect("a trusted directory is planned");
        assert_eq!(planner.0.load(Ordering::SeqCst), 1);
        let named = tamper.apply(&harness.layout().version_dir(VERSION).expect("version dir"));

        let error = crate::commands::setup_host::apply(&harness.context, host_plan, &planner)
            .await
            .expect_err("an untrusted version directory is refused");

        assert_eq!(error.code(), "service_version_untrusted", "{tamper:?}");
        assert!(
            error.to_string().contains(&named.display().to_string()),
            "{tamper:?}: the refusal names {}: {error}",
            named.display()
        );
        assert_eq!(
            planner.0.load(Ordering::SeqCst),
            1,
            "{tamper:?}: the apply asked for no further plan"
        );
    }
}

/// A user manager that reports `PATH` and a configurable daemon unit.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct UnitManager {
    unit: Result<UnitEnvironment, EnvironmentFailure>,
}

#[cfg(target_os = "linux")]
impl UserManager for UnitManager {
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, EnvironmentFailure>> + Send + '_>> {
        Box::pin(async { Ok(vec![format!("PATH={MANAGER_PATH}")]) })
    }

    fn unit_environment<'a>(
        &'a self,
        _unit: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UnitEnvironment, EnvironmentFailure>> + Send + 'a>>
    {
        let unit = self.unit.clone();
        Box::pin(async move { unit })
    }
}

/// A user manager that reports the daemon unit as `present` while the fake
/// service manager holds the daemon job, and as `removed` once it does not.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct LifecycleUnitManager {
    fake: Fake,
    present: UnitEnvironment,
    removed: UnitEnvironment,
}

#[cfg(target_os = "linux")]
impl UserManager for LifecycleUnitManager {
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, EnvironmentFailure>> + Send + '_>> {
        Box::pin(async { Ok(vec![format!("PATH={MANAGER_PATH}")]) })
    }

    fn unit_environment<'a>(
        &'a self,
        _unit: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UnitEnvironment, EnvironmentFailure>> + Send + 'a>>
    {
        let unit = if self.fake.world().registered.is_some() {
            self.present.clone()
        } else {
            self.removed.clone()
        };
        Box::pin(async move { Ok(unit) })
    }
}

/// The `PATH` the fake user manager itself reports.
#[cfg(target_os = "linux")]
const MANAGER_PATH: &str = "/manager/bin";

/// A loaded daemon unit with no environment settings.
#[cfg(target_os = "linux")]
fn loaded_unit() -> UnitEnvironment {
    UnitEnvironment {
        load_state: "loaded".to_owned(),
        ..UnitEnvironment::default()
    }
}

/// Judges the `PATH` of a daemon job that records none, with `unit` as the
/// manager's view of the daemon unit.
#[cfg(target_os = "linux")]
async fn unrecorded_job_path(
    unit: Result<UnitEnvironment, EnvironmentFailure>,
) -> Result<crate::service::engine::JobPath, Error> {
    let harness = Harness::new();
    let context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(UnitManager { unit }));
    // The daemon unit's name derives from the application roots.
    context.namespace().expect("the roots exist");
    crate::service::engine::recorded_job_search_path(
        &context,
        &pohunek_platform::shell_env::SearchPath::empty(),
        None,
    )
    .await
}

/// The refusal text of `result`, which must be a manager-environment error that
/// names a cause and the way to fix it.
#[cfg(target_os = "linux")]
fn refusal(result: Result<crate::service::engine::JobPath, Error>) -> String {
    let error = result.expect_err("the effective PATH cannot be determined");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    error.to_string()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_unit_without_environment_settings_gets_the_managers_path() {
    let job = unrecorded_job_path(Ok(loaded_unit())).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_drop_in_environment_path_replaces_the_managers_path_and_the_last_one_wins() {
    let (_dir, drop_in) =
        environment_file("[Service]\nEnvironment=\"OTHER=1\" PATH=/drop-in/bin\n");
    let unit = UnitEnvironment {
        drop_in_paths: vec![drop_in],
        environment: vec![
            "PATH=/first".to_owned(),
            "OTHER=1".to_owned(),
            "PATH=/drop-in/bin".to_owned(),
        ],
        ..loaded_unit()
    };
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/drop-in/bin");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_unit_changed_on_disk_since_it_was_loaded_is_refused_only_with_drop_ins() {
    // Only the unit file changed: the install replaces it, so the snapshot holds.
    let unit = UnitEnvironment {
        need_daemon_reload: true,
        ..loaded_unit()
    };
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);

    for unit in [
        UnitEnvironment {
            need_daemon_reload: true,
            drop_in_paths: vec!["/nonexistent/loaded.conf".to_owned()],
            ..loaded_unit()
        },
        UnitEnvironment {
            need_daemon_reload: true,
            drop_ins_on_disk: vec!["/home/u/.config/systemd/user/service.d/x.conf".to_owned()],
            ..loaded_unit()
        },
    ] {
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains("systemctl --user daemon-reload"), "{text}");
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_drop_in_with_spaces_around_the_equals_sign_still_sets_the_path() {
    let (_dir, drop_in) = environment_file("[Service]\nEnvironment = PATH=/drop-in/bin\n");
    let unit = UnitEnvironment {
        drop_in_paths: vec![drop_in],
        environment: vec!["PATH=/drop-in/bin".to_owned()],
        ..loaded_unit()
    };
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/drop-in/bin");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_drop_in_that_cannot_be_read_is_refused() {
    let unit = UnitEnvironment {
        drop_in_paths: vec!["/nonexistent/p.conf".to_owned()],
        environment: vec!["PATH=/unit-file/bin".to_owned()],
        ..loaded_unit()
    };
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(
        text.contains("/nonexistent/p.conf") && text.contains("readable regular file"),
        "{text}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn unit_environment_without_a_path_keeps_the_managers_path() {
    let unit = UnitEnvironment {
        environment: vec!["PATHOLOGY=1".to_owned(), "OTHER=/x".to_owned()],
        ..loaded_unit()
    };
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);
}

/// Writes `content` to a file in a scratch directory and returns both.
#[cfg(target_os = "linux")]
fn environment_file(content: &str) -> (tempfile::TempDir, String) {
    let dir = pohunek_test_support::tempdir().expect("scratch directory");
    let file = dir.path().join("job.env");
    std::fs::write(&file, content).expect("environment file");
    let path = file.display().to_string();
    (dir, path)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_environment_file_without_a_path_line_keeps_the_managers_path() {
    let (_dir, file) = environment_file("# comment\nSTORE=/s\nMY_PATH=/not-it\n");
    let unit = UnitEnvironment {
        environment_files: vec![(file, false)],
        ..loaded_unit()
    };
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_environment_file_that_sets_path_is_refused_with_the_file_and_the_fix() {
    for content in [
        "PATH=/file/bin\n",
        "A=1\n  PATH = \"/x:/y\"\n",
        "A='x\ny'\nPATH=/file/bin\n",
        "OTHER=it's\nPATH=/file/bin\nNEXT=it's\n",
        "OTHER=\"a b\" c\nPATH=/file/bin\n",
    ] {
        let (_dir, file) = environment_file(content);
        let unit = UnitEnvironment {
            environment_files: vec![(file.clone(), false)],
            ..loaded_unit()
        };
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains(&file), "{text}");
        assert!(text.contains("Environment= line of a drop-in"), "{text}");
        assert!(!text.contains("/file/bin"), "values stay out: {text}");
    }
}

/// A PATH-looking line inside a quoted or continued value, or in a comment, is
/// not an assignment of `PATH`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn path_text_inside_values_and_comments_of_an_environment_file_is_not_an_assignment() {
    for content in [
        "OTHER='first\nPATH=/not-path\nlast'\n",
        "OTHER=\"a\nPATH=/not-path\"\nSTORE=/s\n",
        "OTHER=a\\\nPATH=/not-path\nSTORE=/s\n",
        "# PATH=/commented\n; PATH=/commented\nSTORE=/s\n",
        "OTHER=\"a b\" c\nSTORE=/s\n",
        "OTHER=it's \"x\nSTORE=/s\n",
    ] {
        let (_dir, file) = environment_file(content);
        let unit = UnitEnvironment {
            environment_files: vec![(file, false)],
            ..loaded_unit()
        };
        let job = unrecorded_job_path(Ok(unit)).await.expect("path");
        assert_eq!(job.value, MANAGER_PATH, "{content:?}");
    }
    let (_dir, file) = environment_file("OTHER='never closed\nPATH=/x\n");
    let unit = UnitEnvironment {
        environment_files: vec![(file, false)],
        ..loaded_unit()
    };
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(text.contains("quoting that is not followed"), "{text}");
}

/// An `EnvironmentFile=` that is not a plain readable file never blocks or
/// exhausts the check.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_environment_file_that_is_a_fifo_a_device_or_too_large_is_refused() {
    let dir = pohunek_test_support::tempdir().expect("scratch directory");
    let fifo = dir.path().join("job.fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).expect("fifo");
    let link = dir.path().join("zero.link");
    std::os::unix::fs::symlink("/dev/zero", &link).expect("symlink");
    let big = dir.path().join("big.env");
    std::fs::write(
        &big,
        vec![
            b'#';
            usize::try_from(crate::service::settings::UNIT_SETTINGS_FILE_LIMIT).expect("fits") + 1
        ],
    )
    .expect("large file");
    for file in [fifo, link, big] {
        let unit = UnitEnvironment {
            environment_files: vec![(file.display().to_string(), false)],
            ..loaded_unit()
        };
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains("readable regular file"), "{text}");
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_optional_environment_file_that_is_missing_has_no_effect() {
    let unit = UnitEnvironment {
        environment_files: vec![("/nonexistent/pohunek-job.env".to_owned(), true)],
        ..loaded_unit()
    };
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_required_environment_file_that_is_missing_or_a_wildcard_is_refused() {
    let missing = "/nonexistent/pohunek-job.env";
    let unit = UnitEnvironment {
        environment_files: vec![(missing.to_owned(), false)],
        ..loaded_unit()
    };
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(
        text.contains(missing) && text.contains("prefix the path with `-`"),
        "{text}"
    );

    let unit = UnitEnvironment {
        environment_files: vec![("/etc/pohunek/*.env".to_owned(), true)],
        ..loaded_unit()
    };
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(
        text.contains("/etc/pohunek/*.env") && text.contains("plain paths"),
        "{text}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn pass_environment_and_unset_environment_naming_path_are_refused() {
    let unit = UnitEnvironment {
        pass_environment: vec!["HOME".to_owned(), "PATH".to_owned()],
        ..loaded_unit()
    };
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(
        text.contains("PassEnvironment=") && text.contains("systemctl --user cat"),
        "{text}"
    );

    for name in ["PATH", "PATH=/x"] {
        let unit = UnitEnvironment {
            unset_environment: vec![name.to_owned()],
            ..loaded_unit()
        };
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains("UnsetEnvironment="), "{text}");
    }
    let unit = UnitEnvironment {
        pass_environment: vec!["PATHS".to_owned()],
        unset_environment: vec!["OTHER".to_owned()],
        ..loaded_unit()
    };
    unrecorded_job_path(Ok(unit))
        .await
        .expect("other names do not matter");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_unit_not_yet_written_without_drop_ins_is_judged_by_the_manager() {
    let fresh = UnitEnvironment {
        load_state: "not-found".to_owned(),
        ..UnitEnvironment::default()
    };
    let job = unrecorded_job_path(Ok(fresh.clone())).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_masked_or_broken_unit_is_refused_and_unreadable_properties_fail_closed() {
    for state in ["masked", "error", "bad-setting"] {
        let unit = UnitEnvironment {
            load_state: state.to_owned(),
            ..UnitEnvironment::default()
        };
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(
            text.contains(state) && text.contains("systemctl --user status"),
            "{text}"
        );
    }
    let text = refusal(
        unrecorded_job_path(Err(EnvironmentFailure::Unusable(
            "the property read was rejected".to_owned(),
        )))
        .await,
    );
    assert!(text.contains("the property read was rejected"), "{text}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_upgrade_judges_and_runs_with_the_drop_in_path_not_the_managers() {
    let mut harness = Harness::new();
    harness.install(V1).await.expect("install");
    let (_dir, drop_in) = environment_file("[Service]\nEnvironment=PATH=/drop-in/bin\n");
    let unit = UnitEnvironment {
        drop_in_paths: vec![drop_in],
        environment: vec!["PATH=/drop-in/bin".to_owned()],
        ..loaded_unit()
    };
    harness.context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(UnitManager { unit: Ok(unit) }));
    harness.runtimes.script().search_paths.clear();
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade");

    let judged = judged.lock().expect("search path lock").clone();
    assert!(!judged.is_empty() && judged.iter().all(|path| path == "/drop-in/bin"));
    let runtime_paths = harness.runtimes.script().search_paths.clone();
    assert!(!runtime_paths.is_empty(), "the runtime step ran");
    assert!(
        runtime_paths.iter().all(|path| path == "/drop-in/bin"),
        "{runtime_paths:?}"
    );
}

/// A rollback and a config migration remove the daemon unit and judge again
/// before installing it back: with a drop-in `PATH` the unit file never held,
/// each step resolves it from the surviving drop-in and completes.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_rollback_whose_unit_was_removed_still_judges_by_the_surviving_drop_in() {
    let mut harness = Harness::new();
    interrupted_upgrade_journal(&mut harness).await;
    let (_keep, present) = unit_with_files(
        b"[Service]\nExecStart=/bin/true\n",
        &[b"[Service]\nEnvironment=PATH=/drop-in/bin\n"],
        &["PATH=/drop-in/bin"],
    );
    let (_keep_removed, removed) = orphan_drop_ins(&[(
        "a.service.d",
        "10-path.conf",
        b"[Service]\nEnvironment=PATH=/drop-in/bin\n",
    )]);
    harness.context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(LifecycleUnitManager {
            fake: harness.fake.clone(),
            present,
            removed,
        }));
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);
    let uninstalls = harness.fake.world().uninstalls;

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V1), V1)
        .await
        .expect("the pending upgrade is rolled back");

    let judged = judged.lock().expect("search path lock").clone();
    assert!(!judged.is_empty() && judged.iter().all(|path| path == "/drop-in/bin"));
    assert!(
        harness.fake.world().uninstalls > uninstalls,
        "the daemon job was removed during the rollback"
    );
    harness.assert_installed(V1);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_upgrade_refuses_before_any_effect_when_a_unit_override_cannot_be_resolved() {
    let mut harness = Harness::new();
    harness.install(V1).await.expect("install");
    let unit = UnitEnvironment {
        pass_environment: vec!["PATH".to_owned()],
        ..loaded_unit()
    };
    harness.context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(UnitManager { unit: Ok(unit) }));
    harness.runtimes.script().calls.clear();

    let error = harness.upgrade(V2).await.expect_err("PassEnvironment=PATH");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    assert!(harness.pending().is_none(), "no transaction was journaled");
    assert_eq!(harness.fake.world().replaces, 0);
    assert!(harness.runtimes.calls().is_empty(), "no runtime step ran");
    harness.assert_installed(V1);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn the_upgrade_journals_the_boot_identity_with_the_launchd_default() {
    let mut harness = Harness::new();
    interrupted_upgrade_journal(&mut harness).await;
    let record = harness
        .pending()
        .expect("the interrupted upgrade is pending");
    assert_eq!(record.launchd_default_path.as_deref(), Some(JOURNALED_PATH));
    assert_eq!(record.launchd_boot_identity.as_deref(), Some(FIRST_BOOT));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_unreadable_boot_identity_journals_the_default_without_a_marker() {
    let mut harness = Harness::new();
    let launchd = FakeLaunchd::new(Ok(loaded_job(JOURNALED_PATH.to_owned())));
    launchd.set_boot(Err("sysctl failed".to_owned()));
    harness.context = harness
        .context
        .clone()
        .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged)
        .with_launchd_job(Arc::new(launchd));
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Definition);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted before the swap");
    let record = harness.pending().expect("pending");
    assert_eq!(record.launchd_default_path.as_deref(), Some(JOURNALED_PATH));
    assert_eq!(record.launchd_boot_identity, None);
}

/// Without a readable boot identity the journal cannot prove the loaded default
/// after the stop; the persisted configuration is taken for it only when it
/// predates the boot and agrees, and anything else is refused while the job
/// still runs.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_unreadable_boot_identity_refuses_before_the_swap_unless_the_persisted_path_proves_the_default(
) {
    for (persisted, modified, accepted) in [
        (PENDING_PATH, Modified::BeforeBoot, false),
        (JOURNALED_PATH, Modified::AfterBoot, false),
        (JOURNALED_PATH, Modified::BeforeBoot, true),
    ] {
        let mut harness = Harness::new();
        let launchd = FakeLaunchd::new(Ok(loaded_job(JOURNALED_PATH.to_owned())));
        launchd.set_boot(Err("sysctl failed".to_owned()));
        let domain = harness.root.join("user.plist");
        std::fs::write(&domain, launchd_domain_xml(persisted)).expect("domain configuration");
        date(&domain, modified).expect("date the configuration");
        harness.context = harness
            .context
            .clone()
            .with_path_discovery(crate::service::context::PathDiscovery::Unmanaged)
            .with_launchd_domain_config(&domain)
            .with_launchd_job(Arc::new(launchd));
        harness.install(V1).await.expect("install");
        let replaces = harness.fake.world().replaces;
        let result = harness.upgrade(V2).await;
        if accepted {
            result.expect("the persisted configuration proves the default");
        } else {
            let error = result.expect_err("the PATH after the stop cannot be told");
            assert_eq!(error.code(), "service_manager_environment_unavailable");
            assert_eq!(harness.fake.world().replaces, replaces, "no replacement");
        }
    }
}

/// A job that records its `PATH` never depends on the launchd default, so an
/// unreadable boot identity does not stop its upgrade.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_recorded_path_job_is_not_refused_for_an_unreadable_boot_identity() {
    let mut harness = Harness::new();
    let launchd = FakeLaunchd::new(Ok(loaded_job(JOURNALED_PATH.to_owned())));
    launchd.set_boot(Err("sysctl failed".to_owned()));
    // The harness records the PATH its install discovers.
    harness.context = harness.context.clone().with_launchd_job(Arc::new(launchd));
    harness.install(V1).await.expect("install");
    assert!(!harness.config().expect("config").search_path().is_empty());
    harness.upgrade(V2).await.expect("upgrade");
    harness.assert_installed(V2);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_loaded_job_ignores_the_journal_and_its_boot_marker() {
    let mut harness = Harness::new();
    interrupted_upgrade_journal(&mut harness).await;
    let launchd = FakeLaunchd::new(Ok(loaded_job(CURRENT_PATH.to_owned())));
    launchd.set_boot(Ok(SECOND_BOOT.to_owned()));
    harness.context = harness.context.clone().with_launchd_job(Arc::new(launchd));
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);
    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V1), V1)
        .await
        .expect("rolled back");
    let judged = judged.lock().expect("search path lock").clone();
    assert!(judged.iter().all(|path| path == CURRENT_PATH), "{judged:?}");
}

/// A preview on a host without the application roots still asks for the unit of
/// the namespace its roots will have, and creates no directory.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn judging_the_path_before_the_application_roots_exist_creates_nothing() {
    let harness = Harness::new();
    let context = harness
        .context
        .clone()
        .with_user_manager(Arc::new(UnitManager {
            unit: Ok(UnitEnvironment {
                pass_environment: vec!["PATH".to_owned()],
                ..loaded_unit()
            }),
        }));
    let (state, runtime) = (
        context.paths().state_dir.clone(),
        context.paths().runtime_dir.clone(),
    );
    // The namespace of the roots as they exist now.
    let before = context.namespace().expect("the roots exist");
    for root in [&state, &runtime] {
        std::fs::remove_dir_all(root).expect("remove root");
    }
    assert_eq!(
        context.namespace_without_creating().expect("derives"),
        before,
        "the namespace does not depend on the roots existing"
    );
    let text = refusal(
        crate::service::engine::recorded_job_search_path(
            &context,
            &pohunek_platform::shell_env::SearchPath::empty(),
            None,
        )
        .await,
    );
    assert!(
        text.contains("PassEnvironment="),
        "the unit was judged: {text}"
    );
    assert!(!state.exists() && !runtime.exists(), "nothing was created");
}

/// When the persisted user-domain configuration last changed, relative to the boot.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy)]
enum Modified {
    BeforeBoot,
    AfterBoot,
    /// The modification time was set long ago; the status change time is now.
    Preserved,
    /// A link made now to a target whose times are long ago.
    Linked,
    /// A link to a target modified after the boot, the link itself made before.
    LinkedTargetAfterBoot,
    /// The configuration directory is gone; the nearest directory above it
    /// changed before the boot.
    DirectoryGoneBeforeBoot,
    /// The same, after the boot.
    DirectoryGoneAfterBoot,
}

/// Dates `path` (a file or a directory) as `modified` says.
#[cfg(target_os = "macos")]
fn date(path: &std::path::Path, modified: Modified) -> std::io::Result<()> {
    let when = match modified {
        Modified::BeforeBoot | Modified::DirectoryGoneBeforeBoot => {
            boot_time() - Duration::from_secs(3600)
        }
        Modified::AfterBoot
        | Modified::DirectoryGoneAfterBoot
        | Modified::LinkedTargetAfterBoot => boot_time() + Duration::from_secs(3600),
        Modified::Preserved | Modified::Linked => {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1000)
        }
    };
    std::fs::File::open(path)?.set_modified(when)
}

/// A host whose boot was an hour ago, so a file written now is newer than it.
#[cfg(target_os = "macos")]
fn booted_an_hour_ago(launchd: &FakeLaunchd) {
    launchd.set_boot(Ok(SECOND_BOOT.to_owned()));
    launchd.set_boot_time(Ok(SystemTime::now() - Duration::from_secs(3600)));
}

/// The pending upgrade of [`interrupted_upgrade_journal`], rolled back on a host
/// whose launchd does not have the job loaded.
///
/// `plist_at_journal` is the persisted user-domain `PATH` when the upgrade
/// journals; `plist_now` is what it holds at the rollback (`None`: the file was
/// removed) and `modified` when it, or its directory, last changed. Returns the
/// paths the rollback's gates judged.
#[cfg(target_os = "macos")]
async fn rollback_after_journal(
    harness: &mut Harness,
    plist_at_journal: &str,
    plist_now: Option<&str>,
    modified: Modified,
    edit_journal: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
    host: impl FnOnce(&FakeLaunchd),
) -> Result<Vec<String>, Error> {
    let config_dir = harness.root.join("launchd-domain");
    std::fs::create_dir_all(&config_dir).expect("configuration directory");
    let domain = config_dir.join("user.plist");
    std::fs::write(&domain, launchd_domain_xml(plist_at_journal)).expect("domain configuration");
    harness.context = harness.context.clone().with_launchd_domain_config(&domain);
    interrupted_upgrade_journal(harness).await;
    let journal = harness.context.paths().state_dir.join(record::FILE_NAME);
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).expect("journal")).expect("journal JSON");
    edit_journal(document.as_object_mut().expect("journal object"));
    std::fs::write(
        &journal,
        serde_json::to_vec(&document).expect("journal bytes"),
    )
    .expect("rewrite journal");
    if let Some(value) = plist_now {
        std::fs::write(&domain, launchd_domain_xml(value)).expect("domain");
        if matches!(modified, Modified::Linked | Modified::LinkedTargetAfterBoot) {
            let target = config_dir.join("target.plist");
            std::fs::rename(&domain, &target).expect("move the configuration");
            date(&target, modified).expect("date the target");
            std::os::unix::fs::symlink(&target, &domain).expect("link the configuration");
        } else {
            date(&domain, modified).expect("date the configuration");
        }
    } else {
        std::fs::remove_file(&domain).expect("remove the configuration");
        if matches!(
            modified,
            Modified::DirectoryGoneBeforeBoot | Modified::DirectoryGoneAfterBoot
        ) {
            std::fs::remove_dir_all(&config_dir).expect("remove the directory");
            date(&harness.root, modified).expect("date the ancestor");
        } else {
            date(&config_dir, modified).expect("date the directory");
        }
    }
    let launchd = FakeLaunchd::new(Ok(not_loaded()));
    host(&launchd);
    harness.context = harness.context.clone().with_launchd_job(Arc::new(launchd));
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);
    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V1), V1)
        .await?;
    let judged = judged.lock().expect("search path lock").clone();
    Ok(judged)
}

/// A host that rebooted since the upgrade journaled.
#[cfg(target_os = "macos")]
fn rebooted(launchd: &FakeLaunchd) {
    launchd.set_boot(Ok(SECOND_BOOT.to_owned()));
}

/// The journal of a pending upgrade holds the persisted `PATH` next to the default.
#[cfg(target_os = "macos")]
const PENDING_PATH: &str = "/opt/pending/bin:/usr/bin";

#[cfg(target_os = "macos")]
fn assert_judged(judged: &Result<Vec<String>, Error>, expected: &str) {
    let judged = judged.as_ref().expect("rolled back");
    assert!(
        !judged.is_empty() && judged.iter().all(|path| path == expected),
        "{judged:?}"
    );
}

#[cfg(target_os = "macos")]
fn assert_rollback_refused(result: &Result<Vec<String>, Error>, reason: &str) {
    let error = result
        .as_ref()
        .expect_err("which PATH is active cannot be told");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    let text = error.to_string();
    assert!(
        text.contains(reason) && text.contains("launchctl setenv PATH"),
        "{text}"
    );
    assert!(
        !text.contains(PENDING_PATH) && !text.contains(JOURNALED_PATH),
        "values stay out: {text}"
    );
}

/// A reboot activated a pending configuration that was then changed again: the
/// journal can no longer tell which default the loaded job has, but the job can.
/// The rollback journals what the loaded job reports before it stops it, so the
/// gate after the stop judges that default instead of refusing with the daemon down.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_rollback_journals_the_loaded_default_before_it_stops_the_job() {
    const LATER: &str = "/opt/later/bin:/usr/bin";
    let mut harness = Harness::new();
    let domain = harness.root.join("user.plist");
    std::fs::write(&domain, launchd_domain_xml(PENDING_PATH)).expect("domain configuration");
    harness.context = harness.context.clone().with_launchd_domain_config(&domain);
    interrupted_upgrade_journal(&mut harness).await;
    std::fs::write(&domain, launchd_domain_xml(LATER)).expect("domain configuration");
    harness.context = harness
        .context
        .clone()
        .with_launchd_job(Arc::new(FlipLaunchd {
            fake: harness.fake.clone(),
            loaded: loaded_job(LATER),
            unloaded: not_loaded(),
            boot: SECOND_BOOT.to_owned(),
        }));
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);
    let uninstalls = harness.fake.world().uninstalls;

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V1), V1)
        .await
        .expect("rolled back");

    assert!(
        harness.fake.world().uninstalls > uninstalls,
        "the job was stopped"
    );
    let judged = judged.lock().expect("search path lock").clone();
    assert!(
        !judged.is_empty() && judged.iter().all(|path| path == LATER),
        "{judged:?}"
    );
    harness.assert_installed(V1);
}

/// A swap whose apply fails rolls back with the job unloaded: the default the
/// loaded job had at the swap is journaled first, so that rollback completes.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_failed_apply_rolls_back_with_the_default_the_loaded_job_had() {
    const LATER: &str = "/opt/later/bin:/usr/bin";
    let mut harness = Harness::new();
    let domain = harness.root.join("user.plist");
    std::fs::write(&domain, launchd_domain_xml(PENDING_PATH)).expect("domain configuration");
    harness.context = harness.context.clone().with_launchd_domain_config(&domain);
    set_job_path(&mut harness, JOURNALED_PATH);
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Definition);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted before the runtime step");
    std::fs::write(&domain, launchd_domain_xml(LATER)).expect("domain configuration");
    harness.context = harness
        .context
        .clone()
        .with_launchd_job(Arc::new(FlipLaunchd {
            fake: harness.fake.clone(),
            loaded: loaded_job(LATER),
            unloaded: not_loaded(),
            boot: SECOND_BOOT.to_owned(),
        }));
    seed_runtime_state(&harness);
    harness.runtimes.plan(installing_plan());
    harness.runtimes.script().fail_apply = true;

    let error = harness
        .engine()
        .with_adoption_preflight(Box::new(ScriptedPreflight::default()))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("the apply fails");

    assert_ne!(
        error.code(),
        "service_manager_environment_unavailable",
        "the rollback was not refused with the job down: {error}"
    );
    assert!(harness.pending().is_none(), "the rollback finished");
    harness.assert_installed(V1);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn the_same_boot_keeps_the_journaled_default_over_a_pending_persisted_change() {
    // Also the case of a clock change: the boot session is the same.
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        Some(PENDING_PATH),
        Modified::AfterBoot,
        |_| {},
        |_| {},
    )
    .await;
    assert_judged(&judged, JOURNALED_PATH);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn the_same_boot_does_not_need_a_readable_persisted_configuration() {
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        Some("<not a property list"),
        Modified::AfterBoot,
        |_| {},
        |_| {},
    )
    .await;
    assert_judged(&judged, JOURNALED_PATH);
}

/// The configuration scheduled `PATH` C, a reboot applied it, and it was then
/// changed back to the journaled B: the file's value equals the journal's, but it
/// was written after the boot, so launchd still applies C.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_configuration_changed_back_to_the_journaled_value_after_a_reboot_is_refused() {
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        Some(JOURNALED_PATH),
        Modified::AfterBoot,
        |_| {},
        rebooted,
    )
    .await;
    assert_rollback_refused(&judged, "modified after the host booted");

    // B journaled, C scheduled and applied by the reboot, B written again: the
    // file equals what it held at the journal write, which proves nothing.
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        JOURNALED_PATH,
        Some(JOURNALED_PATH),
        Modified::AfterBoot,
        |_| {},
        rebooted,
    )
    .await;
    assert_rollback_refused(&judged, "modified after the host booted");
}

/// The same value in a file that predates the boot is the one launchd applied.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_configuration_that_predates_the_boot_is_the_active_default() {
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        JOURNALED_PATH,
        Some(JOURNALED_PATH),
        Modified::BeforeBoot,
        |_| {},
        rebooted,
    )
    .await;
    assert_judged(&judged, JOURNALED_PATH);

    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        Some(PENDING_PATH),
        Modified::BeforeBoot,
        |_| {},
        rebooted,
    )
    .await;
    assert_judged(&judged, PENDING_PATH);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_persisted_change_or_removal_after_the_boot_is_refused() {
    const LATER: &str = "/opt/later/bin:/usr/bin";
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        Some(LATER),
        Modified::AfterBoot,
        |_| {},
        rebooted,
    )
    .await;
    assert_rollback_refused(&judged, "modified after the host booted");

    // A removal is dated by the directory that held the file.
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        None,
        Modified::AfterBoot,
        |_| {},
        rebooted,
    )
    .await;
    assert_rollback_refused(&judged, "modified after the host booted");

    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        None,
        Modified::BeforeBoot,
        |_| {},
        rebooted,
    )
    .await;
    assert_judged(
        &judged,
        pohunek_platform::supervisor::launchd::STANDARD_PATH,
    );
}

/// The configuration is dated by the later of its modification and status change
/// time, a change of a link's target is seen through the link, and a missing
/// directory is dated by the nearest one that exists.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_persisted_configuration_is_dated_by_its_latest_change() {
    // A modification time set long ago does not hide a write after the boot.
    for modified in [Modified::Preserved, Modified::Linked] {
        let mut harness = Harness::new();
        let judged = rollback_after_journal(
            &mut harness,
            PENDING_PATH,
            Some(PENDING_PATH),
            modified,
            |_| {},
            booted_an_hour_ago,
        )
        .await;
        assert_rollback_refused(&judged, "modified after the host booted");
    }
    // A target changed after the boot, behind a link made earlier.
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        Some(PENDING_PATH),
        Modified::LinkedTargetAfterBoot,
        |_| {},
        rebooted,
    )
    .await;
    assert_rollback_refused(&judged, "modified after the host booted");
    // The same files are older than a boot that happened after them.
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        Some(PENDING_PATH),
        Modified::Preserved,
        |_| {},
        rebooted,
    )
    .await;
    assert_judged(&judged, PENDING_PATH);

    // A removed directory is dated by the nearest one above it.
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        None,
        Modified::DirectoryGoneBeforeBoot,
        |_| {},
        rebooted,
    )
    .await;
    assert_judged(
        &judged,
        pohunek_platform::supervisor::launchd::STANDARD_PATH,
    );
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        None,
        Modified::DirectoryGoneAfterBoot,
        |_| {},
        rebooted,
    )
    .await;
    assert_rollback_refused(&judged, "modified after the host booted");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_configuration_or_boot_time_that_cannot_be_dated_is_refused() {
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        Some(PENDING_PATH),
        Modified::BeforeBoot,
        |_| {},
        |launchd| {
            rebooted(launchd);
            launchd.set_boot_time(Err("sysctl failed".to_owned()));
        },
    )
    .await;
    assert_rollback_refused(&judged, "boot time cannot be read");
}

/// Without a boot marker the time rule applies too, and the persisted value
/// must equal the journaled one.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_journal_without_a_boot_marker_is_used_only_when_the_file_predates_the_boot_and_agrees() {
    let drop_marker = |journal: &mut serde_json::Map<String, serde_json::Value>| {
        journal.remove("launchd_boot_identity");
    };
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        JOURNALED_PATH,
        Some(JOURNALED_PATH),
        Modified::BeforeBoot,
        drop_marker,
        rebooted,
    )
    .await;
    assert_judged(&judged, JOURNALED_PATH);

    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        JOURNALED_PATH,
        Some(JOURNALED_PATH),
        Modified::AfterBoot,
        drop_marker,
        rebooted,
    )
    .await;
    assert_rollback_refused(&judged, "modified after the host booted");

    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        Some(PENDING_PATH),
        Modified::BeforeBoot,
        drop_marker,
        |_| {},
    )
    .await;
    assert_rollback_refused(&judged, "no boot marker");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn an_unreadable_current_boot_identity_is_judged_like_a_missing_marker() {
    let unreadable = |launchd: &FakeLaunchd| launchd.set_boot(Err("sysctl failed".to_owned()));
    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        JOURNALED_PATH,
        Some(JOURNALED_PATH),
        Modified::BeforeBoot,
        |_| {},
        unreadable,
    )
    .await;
    assert_judged(&judged, JOURNALED_PATH);

    let mut harness = Harness::new();
    let judged = rollback_after_journal(
        &mut harness,
        PENDING_PATH,
        Some(PENDING_PATH),
        Modified::BeforeBoot,
        |_| {},
        unreadable,
    )
    .await;
    assert_rollback_refused(&judged, "current boot identity cannot be read");
}

/// A unit as the manager reports it: the unit file at `FragmentPath` with the
/// given text, one drop-in per text, and the merged `Environment` list.
/// Returns the scratch directories to keep alive.
#[cfg(target_os = "linux")]
fn unit_with_files(
    unit_file: &[u8],
    drop_ins: &[&[u8]],
    environment: &[&str],
) -> (Vec<tempfile::TempDir>, UnitEnvironment) {
    let mut guards = Vec::new();
    let mut write = |name: String, content: &[u8]| {
        let dir = pohunek_test_support::tempdir().expect("scratch directory");
        let file = dir.path().join(name);
        std::fs::write(&file, content).expect("setting file");
        let path = file.display().to_string();
        guards.push(dir);
        path
    };
    let fragment_path = write("setting.conf".to_owned(), unit_file);
    // The manager reports one file per drop-in name, so the names differ.
    let drop_in_paths = drop_ins
        .iter()
        .enumerate()
        .map(|(index, content)| write(format!("{index:02}-setting.conf"), content))
        .collect();
    (
        guards,
        UnitEnvironment {
            fragment_path,
            drop_in_paths,
            environment: environment
                .iter()
                .map(|entry| (*entry).to_owned())
                .collect(),
            ..loaded_unit()
        },
    )
}

/// The install replaces the unit file and keeps its drop-ins: a `PATH` that only
/// the unit file sets does not reach the job, and one that a drop-in also sets
/// does.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_path_set_only_by_the_replaced_unit_file_does_not_apply() {
    let unit_file = b"[Service]\nEnvironment=PATH=/unit-file/bin\n";
    let (_keep, unit) = unit_with_files(unit_file, &[], &["PATH=/unit-file/bin"]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH, "no drop-in");

    let (_keep, unit) = unit_with_files(
        unit_file,
        &[b"[Service]\nEnvironment=PATH=/drop-in/bin\n"],
        &["PATH=/unit-file/bin", "PATH=/drop-in/bin"],
    );
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/drop-in/bin");
}

/// With the unit file also setting `PATH`, a drop-in that no reading recognizes
/// as keeping it cannot be attributed: refuse instead of guessing.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_merged_path_no_drop_in_claims_is_refused_when_the_unit_file_sets_it_too() {
    let unit_file: &[u8] = b"[Service]\nEnvironment=PATH=/unit-file/bin\n";
    for drop_in in [
        &b"[Unit]\nEnvironment=PATH=/ignored/bin\n"[..],
        &b"[Service]\nEnvironment=STORE=/x\n"[..],
        &b"# Environment=PATH=/commented\n[Service]\nEnvironment=OTHER=v \\\n# PATH=/commented\n  FOO=bar\n"[..],
    ] {
        let (_keep, unit) = unit_with_files(unit_file, &[drop_in], &["PATH=/unit-file/bin"]);
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains("could be recognized as keeping it"), "{text}");
        assert!(!text.contains("/unit-file/bin"), "values stay out: {text}");
    }
}

/// Forms of a drop-in `PATH` that only a different reader sees: refuse (unit
/// file also sets it) or use the merged value (it does not).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_drop_in_path_in_an_unreadable_form_is_refused_or_attributed_by_the_unit_file() {
    let unit_file: &[u8] = b"[Service]\nEnvironment=PATH=/unit-file/bin\n";
    for drop_in in [
        &b"[Service]\nEnvironment=\"PATH\\x3d/drop-in/bin\"\n"[..],
        &b"[Service]\nEnvironment=MYPATH=/x\n"[..],
        &b"[Service]\nEnvironment=\"PATH=/ignored\n"[..],
        &b"[Service]\nEnvironment=PATH=/\xff/bin\n"[..],
    ] {
        let (_keep, unit) = unit_with_files(unit_file, &[drop_in], &["PATH=/unit-file/bin"]);
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains("in a form not read here"), "{text}");
        assert!(!text.contains("/unit-file/bin"), "values stay out: {text}");
    }
    // A removed unit would resolve from the drop-in alone, which cannot read this
    // form, so it is refused upfront even though the unit file sets no PATH.
    let (_keep, unit) = unit_with_files(
        b"[Service]\nExecStart=/bin/true\n",
        &[&b"[Service]\nEnvironment=\"PATH\\x3d/drop-in/bin\"\n"[..]],
        &["PATH=/drop-in/bin"],
    );
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(text.contains("in a form not read here"), "{text}");
}

/// An escaped variable name in the replaced unit file is a `PATH` the install
/// removes: the merged value is not a drop-in's.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_escaped_path_name_in_the_replaced_unit_file_does_not_apply() {
    let unit_file: &[u8] = b"[Service]\nEnvironment=\"\\x50ATH=/unit-file/bin\"\n";
    let (_keep, unit) = unit_with_files(unit_file, &[], &["PATH=/unit-file/bin"]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH, "no drop-in keeps it");

    let (_keep, unit) = unit_with_files(
        unit_file,
        &[b"[Service]\nEnvironment=PATH=/drop-in/bin\n"],
        &["PATH=/unit-file/bin", "PATH=/drop-in/bin"],
    );
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/drop-in/bin");
}

/// `ExecSearchPath=` appends across files, so a merged list that a unit file and
/// a drop-in both feed cannot be split into the part the install keeps.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_exec_search_path_fed_by_the_unit_file_and_a_drop_in_is_refused() {
    let (_keep, mut unit) = unit_with_files(
        b"[Service]\nExecSearchPath=/unit-file/bin\n",
        &[b"[Service]\nExecSearchPath=/drop-in/bin\n"],
        &[],
    );
    unit.exec_search_path = vec!["/unit-file/bin".to_owned(), "/drop-in/bin".to_owned()];
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(text.contains("ExecSearchPath"), "{text}");
    assert!(text.contains("both its unit file"), "{text}");
    assert!(!text.contains("/unit-file/bin"), "values stay out: {text}");
}

/// A drop-in saved with a byte order mark or CR line ends is read as systemd
/// reads it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_drop_in_with_a_byte_order_mark_or_carriage_returns_still_sets_the_path() {
    let unit_file: &[u8] = b"[Service]\nEnvironment=PATH=/unit-file/bin\n";
    for drop_in in [
        &b"\xef\xbb\xbf[Service]\nEnvironment=PATH=/drop-in/bin\n"[..],
        &b"[Service]\rEnvironment=PATH=/drop-in/bin\r"[..],
        &b"[Service]\r\nEnvironment=PATH=/drop-in/bin\r\n"[..],
        &b"[Service]\0Environment=PATH=/drop-in/bin\0"[..],
    ] {
        let (_keep, unit) = unit_with_files(
            unit_file,
            &[drop_in],
            &["PATH=/unit-file/bin", "PATH=/drop-in/bin"],
        );
        let job = unrecorded_job_path(Ok(unit)).await.expect("path");
        assert_eq!(job.value, "/drop-in/bin", "{drop_in:?}");
    }
}

/// `ExecSearchPath=` replaces the manager's `PATH`; an `Environment=` `PATH`
/// replaces it in turn.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn exec_search_path_from_a_drop_in_sets_the_job_path_below_environment() {
    let drop_in: &[u8] = b"[Service]\nExecSearchPath=/opt/tools/bin:/opt/more/bin\n";
    let (_keep, mut unit) = unit_with_files(b"[Service]\nExecStart=/bin/true\n", &[drop_in], &[]);
    unit.exec_search_path = vec!["/opt/tools/bin".to_owned(), "/opt/more/bin".to_owned()];
    let job = unrecorded_job_path(Ok(unit.clone())).await.expect("path");
    assert_eq!(job.value, "/opt/tools/bin:/opt/more/bin");

    // Environment=PATH wins over ExecSearchPath.
    let env_drop_in: &[u8] = b"[Service]\nEnvironment=PATH=/env/bin\n";
    let (_keep2, mut both) = unit_with_files(
        b"[Service]\nExecStart=/bin/true\n",
        &[drop_in, env_drop_in],
        &["PATH=/env/bin"],
    );
    both.exec_search_path = unit.exec_search_path.clone();
    let job = unrecorded_job_path(Ok(both)).await.expect("path");
    assert_eq!(job.value, "/env/bin");

    // An ExecSearchPath that only the replaced unit file sets does not apply.
    let (_keep3, mut file_only) =
        unit_with_files(b"[Service]\nExecSearchPath=/opt/tools/bin\n", &[], &[]);
    file_only.exec_search_path = vec!["/opt/tools/bin".to_owned()];
    let job = unrecorded_job_path(Ok(file_only)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);
}

/// A unit with no unit file whose drop-in files exist on disk only: `(directory
/// tag, file name, text)` per file.
#[cfg(target_os = "linux")]
fn orphan_drop_ins(files: &[(&str, &str, &[u8])]) -> (Vec<tempfile::TempDir>, UnitEnvironment) {
    let mut guards = Vec::new();
    let mut on_disk = Vec::new();
    for (tag, name, text) in files {
        let dir = pohunek_test_support::tempdir().expect("scratch directory");
        let directory = dir.path().join(tag);
        std::fs::create_dir_all(&directory).expect("drop-in directory");
        let path = directory.join(name);
        std::fs::write(&path, text).expect("drop-in");
        on_disk.push(path.display().to_string());
        guards.push(dir);
    }
    (
        guards,
        UnitEnvironment {
            load_state: "not-found".to_owned(),
            drop_ins_on_disk: on_disk,
            ..UnitEnvironment::default()
        },
    )
}

/// Rollback and migration remove the unit and judge again before reinstalling it:
/// the drop-ins that survive are what the new job gets, so a missing unit with
/// plain drop-ins resolves instead of blocking the recovery.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_removed_unit_with_surviving_drop_ins_is_judged_by_them() {
    let (_keep, unit) = orphan_drop_ins(&[
        (
            "a.service.d",
            "10-first.conf",
            b"[Service]\nEnvironment=PATH=/first/bin\n",
        ),
        (
            "a-.service.d",
            "20-second.conf",
            b"[Service]\nEnvironment=OTHER=1 PATH=/second/bin\n",
        ),
    ]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/second/bin", "the later file name wins");

    let (_keep, unit) = orphan_drop_ins(&[
        (
            "a.service.d",
            "10-first.conf",
            b"[Service]\nEnvironment=PATH=/first/bin\n",
        ),
        ("a.service.d", "20-reset.conf", b"[Service]\nEnvironment=\n"),
    ]);
    // The `PATH` reader follows the reset; the job as a whole is refused, because
    // the reset also removes the bootstrap environment the unit file gives it.
    let value = crate::service::manager_env::effective_search_path(
        Some(MANAGER_PATH.to_owned()),
        &unit,
        "a.service",
        &mut crate::service::manager_env::UnitFileReader::default(),
    )
    .await
    .expect("path");
    assert_eq!(
        value.as_deref(),
        Some(MANAGER_PATH),
        "an empty assignment resets"
    );
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(text.contains("resets Environment="), "{text}");

    let (_keep, unit) = orphan_drop_ins(&[(
        "a.service.d",
        "10-other.conf",
        b"[Service]\nEnvironment=STORE=/x\n",
    )]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH, "no PATH in the drop-ins");
}

/// A name two same-named directories hold resolves by lookup priority, and a
/// link to `/dev/null` that wins disables the name.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_drop_in_name_in_two_lookup_directories_resolves_by_priority() {
    let real: &[u8] = b"[Service]\nEnvironment=PATH=/vendor/bin\n";
    let other: &[u8] = b"[Service]\nEnvironment=PATH=/other/bin\n";

    // Two real files: the higher-priority (first) directory wins.
    let (_keep, unit) = orphan_drop_ins(&[
        ("a.service.d", "10-p.conf", other),
        ("a.service.d", "10-p.conf", real),
    ]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/other/bin");

    // A mask in the higher-priority directory disables the vendor drop-in.
    let (_keep, unit) = orphan_drop_ins(&[
        ("a.service.d", "10-p.conf", b""),
        ("a.service.d", "10-p.conf", real),
    ]);
    std::fs::remove_file(&unit.drop_ins_on_disk[0]).expect("remove placeholder");
    std::os::unix::fs::symlink("/dev/null", &unit.drop_ins_on_disk[0]).expect("mask");
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH, "the masked name does not apply");

    // A mask below a real file does not hide it.
    let (_keep, unit) = orphan_drop_ins(&[
        ("a.service.d", "10-p.conf", real),
        ("a.service.d", "10-p.conf", b""),
    ]);
    std::fs::remove_file(&unit.drop_ins_on_disk[1]).expect("remove placeholder");
    std::os::unix::fs::symlink("/dev/null", &unit.drop_ins_on_disk[1]).expect("mask");
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/vendor/bin");
}

/// The manager keeps a drop-in masked by a link to `/dev/null` in its list; a
/// loaded unit's resolution treats it as disabled instead of refusing it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_masked_drop_in_of_a_loaded_unit_is_disabled_not_refused() {
    let (keep, mut unit) = unit_with_files(
        b"[Service]\nExecStart=/bin/true\n",
        &[b"[Service]\nEnvironment=PATH=/drop-in/bin\n"],
        &["PATH=/drop-in/bin"],
    );
    let mask_dir = pohunek_test_support::tempdir().expect("scratch directory");
    let mask = mask_dir.path().join("00-vendor.conf");
    std::os::unix::fs::symlink("/dev/null", &mask).expect("mask");
    unit.drop_in_paths.push(mask.display().to_string());
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/drop-in/bin");
    drop((keep, mask_dir));
}

/// `PassEnvironment=` and `UnsetEnvironment=` refuse only for the name `PATH`,
/// quoted or not; a variable whose name contains it is not `PATH`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn pass_and_unset_environment_are_matched_by_the_exact_name() {
    for line in [
        "UnsetEnvironment=PYTHONPATH",
        "PassEnvironment=LD_LIBRARY_PATH PATHS",
        "UnsetEnvironment=\"MYPATH\" OTHER",
    ] {
        let text = format!("[Service]\n{line}\nEnvironment=PATH=/a/bin\n");
        let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "p.conf", text.as_bytes())]);
        let job = unrecorded_job_path(Ok(unit))
            .await
            .expect("a lookalike is not PATH");
        assert_eq!(job.value, "/a/bin", "{line}");
    }
    for line in [
        "UnsetEnvironment=P\"ATH\"",
        "PassEnvironment=\"PATH\"",
        "UnsetEnvironment=PATH=/x",
        "PassEnvironment=PA\\x54H",
        "PassEnvironment=%j",
    ] {
        let text = format!("[Service]\n{line}\n");
        let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "p.conf", text.as_bytes())]);
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(
            text.contains("p.conf") && text.contains("Environment="),
            "{line}: {text}"
        );
    }
}

/// The drop-in settings that resolve while the unit is loaded resolve after its
/// removal too: `ExecSearchPath=` and an `EnvironmentFile=` without `PATH`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_removed_unit_keeps_resolving_exec_search_path_and_environment_files() {
    let (_keep, unit) = orphan_drop_ins(&[
        (
            "a.service.d",
            "10-a.conf",
            b"[Service]\nExecSearchPath=/opt/a/bin:/opt/b/bin\n",
        ),
        (
            "a.service.d",
            "20-b.conf",
            b"[Service]\nExecSearchPath=/opt/c/bin\n",
        ),
    ]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/opt/a/bin:/opt/b/bin:/opt/c/bin");

    let (_keep, unit) = orphan_drop_ins(&[
        (
            "a.service.d",
            "10-a.conf",
            b"[Service]\nExecSearchPath=/opt/a/bin\n",
        ),
        (
            "a.service.d",
            "20-b.conf",
            b"[Service]\nEnvironment=PATH=/env/bin\n",
        ),
    ]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/env/bin", "Environment=PATH wins");

    let (_dir, file) = environment_file("FOO=1\n");
    let line = format!("[Service]\nEnvironmentFile=-{file}\n");
    let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", line.as_bytes())]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);

    let (_dir, file) = environment_file("PATH=/file/bin\n");
    let line = format!("[Service]\nEnvironmentFile={file}\n");
    let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", line.as_bytes())]);
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(text.contains("sets PATH"), "{text}");
}

/// systemd ignores a relative `EnvironmentFile=` (pinned on the real manager), so
/// a removed unit's drop-in with one gives no file to read; a non-normalized
/// absolute path is refused, since what systemd reads for it is not followed.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn environment_file_paths_of_a_removed_unit_are_ignored_when_relative_and_refused_when_not_normalized(
) {
    for line in [
        "EnvironmentFile=relative.env",
        "EnvironmentFile=-also-relative.env",
    ] {
        let text = format!("[Service]\n{line}\n");
        let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", text.as_bytes())]);
        let job = unrecorded_job_path(Ok(unit)).await.expect("path");
        assert_eq!(job.value, MANAGER_PATH, "{line}");
    }
    for line in [
        "EnvironmentFile=/nonexistent/../pohunek-test.env",
        "EnvironmentFile=-/nonexistent/./pohunek-test.env",
        "EnvironmentFile=/nonexistent//pohunek-test.env",
        "EnvironmentFile=/nonexistent/",
    ] {
        let text = format!("[Service]\n{line}\n");
        let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", text.as_bytes())]);
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains("not a normalized path"), "{line}: {text}");
    }
}

/// A run of backslashes at the end of a line continues it only when its length
/// is odd (pinned on the real manager), and a comment line ending in a backslash
/// swallows nothing, so a removed unit's drop-in is read by that rule.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_removed_units_drop_in_with_backslashes_is_read_by_the_rule() {
    for (content, expected) in [
        // Odd run: the next line is more words of the same assignment.
        (
            "[Service]\nEnvironment=FIRST=1 \\\nPATH=/hidden/bin\n",
            "/hidden/bin",
        ),
        // Even run: the line ends, the next one is a line of its own.
        (
            "[Service]\nEnvironment=FIRST=1 \\\\\nPATH=/hidden/bin\n",
            MANAGER_PATH,
        ),
        ("[Service]\nEnvironment=PATH=/a/bin \\\n", "/a/bin"),
        ("[Service]\nEnvironment=PATH=/a/bin \\\\\n", "/a/bin"),
        // A comment ending in a backslash swallows nothing.
        ("[Service]\n# note \\\nEnvironment=PATH=/a/bin\n", "/a/bin"),
        // A header joined into a continuation is text of the continued line.
        (
            "[Unit]\nDescription=x \\\n[Service]\nEnvironment=PATH=/a/bin\n",
            MANAGER_PATH,
        ),
        // The continuation of an assignment goes on across a comment line.
        (
            "[Service]\nEnvironment=FIRST=1 \\\n# note\nPATH=/c/bin\n",
            "/c/bin",
        ),
    ] {
        let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", content.as_bytes())]);
        let job = unrecorded_job_path(Ok(unit)).await.expect(content);
        assert_eq!(job.value, expected, "{content:?}");
    }
}

/// A loaded unit whose drop-in uses backslash continuations, escaped backslashes
/// or a comment ending in a backslash is resolved from the manager's merged
/// `PATH`, not refused: the rule is the one systemd applies (pinned on the real
/// manager).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_loaded_unit_with_backslash_forms_is_resolved_not_refused() {
    for (content, merged, expected) in [
        (
            "[Service]\nEnvironment=FIRST=1 \\\nPATH=/hidden/bin\n",
            vec!["FIRST=1", "PATH=/hidden/bin"],
            "/hidden/bin",
        ),
        (
            "[Service]\nEnvironment=FIRST=1 \\\\\nPATH=/hidden/bin\n",
            vec!["FIRST=1"],
            MANAGER_PATH,
        ),
        (
            "[Service]\n# note \\\nEnvironment=PATH=/a/bin\n",
            vec!["PATH=/a/bin"],
            "/a/bin",
        ),
        (
            "[Service]\nEnvironment=PATH=/a/bin \\\\\n",
            vec!["PATH=/a/bin"],
            "/a/bin",
        ),
    ] {
        let (_dir, drop_in) = environment_file(content);
        let unit = UnitEnvironment {
            drop_in_paths: vec![drop_in],
            environment: merged.into_iter().map(str::to_owned).collect(),
            ..loaded_unit()
        };
        let job = unrecorded_job_path(Ok(unit)).await.expect(content);
        assert_eq!(job.value, expected, "{content:?}");
    }
}

/// An empty or non-normalized entry of `ExecSearchPath=` is ignored or simplified
/// by systemd, dropping the entries after it (pinned on the real manager), so a
/// removed unit's drop-in with one is refused.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_empty_exec_search_path_entry_of_a_removed_unit_is_refused() {
    let component = format!("/x:/{}:/y", "a".repeat(256));
    let entry = format!("/x:/{}", vec!["a".repeat(100); 41].join("/"));
    for value in [
        component.as_str(),
        entry.as_str(),
        "/a/bin::/b/bin",
        ":/a/bin",
        "/a/bin:",
        "/d/../e:/f",
        "/g//h/:/i/./j",
        "relative/bin",
    ] {
        let text = format!("[Service]\nExecSearchPath={value}\n");
        let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", text.as_bytes())]);
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(
            text.contains("not a normalized absolute path or too long"),
            "{value}: {text}"
        );
    }
}

/// Quotes in `ExecSearchPath=` stay literal characters for systemd, a section
/// header that is never closed ends what systemd reads of the file, and a byte
/// order mark after the start is dropped wherever it is (all pinned on the real
/// manager): a removed unit's drop-in with one is refused.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn drop_in_syntax_systemd_reads_differently_is_refused_for_a_removed_unit() {
    for (content, reason) in [
        (
            &b"[Service]\nExecSearchPath=\"/q/bin\"\n"[..],
            "ExecSearchPath",
        ),
        (
            b"[Service]\nEnvironment=PATH=/a\n[Bogus\nEnvironment=PATH=/b\n",
            "without closing it",
        ),
        (
            b"[Service]\nExecSearchPath=/x/bin\n[Service] # note\nExecSearchPath=/y/bin\n",
            "without closing it",
        ),
        (
            b"[Service]\nEnvironment=FIRST=1\n\xEF\xBB\xBFEnvironment=PATH=/bom\n",
            "byte order mark",
        ),
        (
            b"[Service]\nEnvironment=PATH=/a\n[Unit]\x0C\nEnvironment=PATH=/b\n",
            "without closing it",
        ),
        (
            b"[Service]\nEnvironment=PATH=/a\nEnvironment=PATH=/b FOO=\xEF\xBF\xBE\n",
            "in a form not read here",
        ),
        (
            b"[Service]\nExecSearchPath=/x\nExecSearchPath=/a/\xF4\x8F\xBF\xBF:/z\n",
            "in a form not read here",
        ),
    ] {
        let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", content)]);
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains(reason), "{content:?}: {text}");
    }
    // A byte order mark at the start of the file is dropped by systemd too.
    let (_keep, unit) = orphan_drop_ins(&[(
        "a.service.d",
        "10-a.conf",
        b"\xEF\xBB\xBF[Service]\nEnvironment=PATH=/bom/bin\n",
    )]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/bom/bin");
}

/// A specifier in the name of an `Environment=` word is expanded by systemd
/// (`%i` to nothing for a unit that is not a template), so it can spell `PATH`:
/// a removed unit's drop-in with one is refused, the specifier named.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_specifier_in_an_environment_name_of_a_removed_unit_is_refused() {
    for line in [
        "Environment=%iPATH=/x",
        "Environment=P%IATH=/x FOO=1",
        "Environment=\"%iPATH=/x\"",
        "Environment=FOO=1 %",
    ] {
        let text = format!("[Service]\n{line}\n");
        let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", text.as_bytes())]);
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains("uses the specifier %"), "{line}: {text}");
        assert!(!text.contains('\0'), "{line}: {text:?}");
    }
}

/// A no-break space is not a word separator for systemd, so it keeps two
/// assignments in one value.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_no_break_space_does_not_split_an_environment_value() {
    let (_keep, unit) = orphan_drop_ins(&[(
        "a.service.d",
        "10-a.conf",
        "[Service]\nEnvironment=PATH=/first\u{a0}PATH=/second\n".as_bytes(),
    )]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/first\u{a0}PATH=/second");
}

/// A drop-in linked to `/dev/null` is a disabled one; an environment file path
/// is the whole value, spaces included.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_removed_unit_skips_masked_drop_ins_and_reads_a_spaced_environment_file() {
    let (keep, mut unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", b"[Service]\n")]);
    let masked = std::path::PathBuf::from(&unit.drop_ins_on_disk[0]);
    std::fs::remove_file(&masked).expect("remove");
    std::os::unix::fs::symlink("/dev/null", &masked).expect("mask");
    let job = unrecorded_job_path(Ok(unit.clone())).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);
    unit.drop_ins_on_disk.clear();
    drop(keep);

    let dir = pohunek_test_support::tempdir().expect("scratch directory");
    let spaced = dir.path().join("service vars");
    std::fs::write(&spaced, "FOO=1\n").expect("environment file");
    let text = format!("[Service]\nEnvironmentFile={}\n", spaced.display());
    let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", text.as_bytes())]);
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);
}

/// The values of a specifier cannot be read for a unit the manager has not
/// loaded, so none is expanded: the drop-in is refused with the key and the
/// specifier named.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_removed_unit_refuses_every_specifier_in_its_drop_ins() {
    for (line, specifier, key) in [
        ("Environment=PATH=%h/bin", "%h", "Environment"),
        ("Environment=PATH=/run/%t/bin", "%t", "Environment"),
        ("Environment=PATH=/opt/100%%", "%%", "Environment"),
        ("Environment=PATH=%Z/bin", "%Z", "Environment"),
        ("ExecSearchPath=/opt/%H/bin", "%H", "ExecSearchPath"),
    ] {
        let text = format!("[Service]\n{line}\n");
        let (_keep, unit) = orphan_drop_ins(&[("a.service.d", "10-a.conf", text.as_bytes())]);
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(
            text.contains(specifier) && text.contains(&format!("in {key}=")),
            "{text}"
        );
    }
}

/// The loaded gate refuses, before anything is stopped, a drop-in the removed
/// unit's resolution could not read: a specifier the manager expands for the
/// loaded unit (`%h`, `%Z`) cannot be reproduced after a rollback or migration
/// removed the unit, so the recovery would strand the daemon.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_loaded_gate_validates_the_removed_resolution_before_anything_is_stopped() {
    for specifier in ["%Z", "%h"] {
        let mut harness = Harness::new();
        harness.install(V1).await.expect("install");
        let line = format!("[Service]\nEnvironment=PATH={specifier}/bin\n");
        let (_keep, unit) = unit_with_files(
            b"[Service]\nExecStart=/bin/true\n",
            &[line.as_bytes()],
            &["PATH=/expanded/bin"],
        );
        harness.context = harness
            .context
            .clone()
            .with_user_manager(Arc::new(UnitManager { unit: Ok(unit) }));
        harness.runtimes.script().calls.clear();
        let uninstalls = harness.fake.world().uninstalls;
        let error = harness
            .upgrade(V2)
            .await
            .expect_err("a specifier is not reproducible");
        assert_eq!(error.code(), "service_manager_environment_unavailable");
        assert!(error.to_string().contains(specifier), "{error}");
        assert!(harness.pending().is_none(), "no transaction was journaled");
        assert_eq!(
            harness.fake.world().uninstalls,
            uninstalls,
            "nothing stopped"
        );
        assert_eq!(harness.fake.world().replaces, 0);
        harness.assert_installed(V1);
    }

    // A same-named drop-in shadowed in another directory is refused too.
    let (_keep, mut unit) = unit_with_files(
        b"[Service]\nExecStart=/bin/true\n",
        &[b"[Service]\nEnvironment=PATH=/a/bin\n"],
        &["PATH=/a/bin"],
    );
    let shadow = pohunek_test_support::tempdir().expect("scratch directory");
    let shadowed = shadow.path().join("00-setting.conf");
    std::fs::write(&shadowed, "[Service]\nEnvironment=PATH=/b/bin\n").expect("shadowed drop-in");
    unit.drop_ins_on_disk = vec![
        unit.drop_in_paths[0].clone(),
        shadowed.display().to_string(),
    ];
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(text.contains("two directories"), "{text}");
}

/// What cannot be read from drop-ins of an unloaded unit is refused with the
/// file and the fix.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_removed_unit_with_drop_ins_that_cannot_be_read_is_refused() {
    for (name, text, expect) in [
        (
            "p.conf",
            &b"[Service]\nExecSearchPath=/a:/b /c\n"[..],
            "ExecSearchPath",
        ),
        (
            "p.conf",
            &b"[Service]\nExecSearchPath=relative/bin\n"[..],
            "ExecSearchPath",
        ),
        (
            "p.conf",
            &b"[Service]\nPassEnvironment=PATH\n"[..],
            "PassEnvironment=",
        ),
        (
            "p.conf",
            &b"[Service]\nEnvironment=\"\\x50ATH=/a\"\n"[..],
            "form not read here",
        ),
    ] {
        let (_keep, unit) = orphan_drop_ins(&[("a.service.d", name, text)]);
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains(expect) && text.contains("p.conf"), "{text}");
    }
    let (_keep, unit) = orphan_drop_ins(&[
        ("a.service.d", "p.conf", b"[Service]\nEnvironment=PATH=/a\n"),
        ("b.service.d", "p.conf", b"[Service]\nEnvironment=PATH=/b\n"),
    ]);
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(text.contains("two directories"), "{text}");
}

/// A stale snapshot next to drop-ins refuses with the reload fix.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_stale_snapshot_next_to_drop_ins_on_disk_asks_for_a_reload() {
    let stale = UnitEnvironment {
        need_daemon_reload: true,
        drop_ins_on_disk: vec!["/etc/systemd/user/pohunek-.service.d/p.conf".to_owned()],
        ..loaded_unit()
    };
    let text = refusal(unrecorded_job_path(Ok(stale)).await);
    assert!(text.contains("systemctl --user daemon-reload"), "{text}");
}

/// A merged `PATH` of a stale snapshot whose unit file no longer sets it, with
/// no drop-in, is not attributed to a drop-in.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_stale_fragment_only_path_without_drop_ins_is_not_attributed_to_one() {
    let (_keep, mut unit) =
        unit_with_files(b"[Service]\nExecStart=/bin/true\n", &[], &["PATH=/old/bin"]);
    unit.need_daemon_reload = true;
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, MANAGER_PATH);
}

/// CRLF is one line end: a continuation across it joins the next line, and an
/// assignment after it is a line of its own.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn crlf_line_ends_keep_a_continuation_intact() {
    let unit_file: &[u8] =
        b"[Service]\r\nEnvironment=OTHER=1 \\\r\n  MORE=2\r\nEnvironment=PATH=/unit-file/bin\r\n";
    let (_keep, unit) = unit_with_files(
        unit_file,
        &[b"[Service]\r\nEnvironment=PATH=/drop-in/bin\r\n"],
        &["PATH=/drop-in/bin"],
    );
    let job = unrecorded_job_path(Ok(unit)).await.expect("path");
    assert_eq!(job.value, "/drop-in/bin");
    // The continued assignment of the unit file carries PATH, which the install
    // removes: an unrelated drop-in does not claim it.
    let continued: &[u8] = b"[Service]\r\nEnvironment=OTHER=1 \\\r\nPATH=/unit-file/bin\r\n";
    let (_keep, unit) = unit_with_files(
        continued,
        &[b"[Service]\r\nEnvironment=STORE=/x\r\n"],
        &["PATH=/unit-file/bin"],
    );
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(text.contains("could be recognized as keeping it"), "{text}");

    // is refused instead of attributed.
    let (_keep, unit) = unit_with_files(
        unit_file,
        &[b"[Service]\r\nEnvironment=\"PATH\\x3d/x\"\r\n"],
        &["PATH=/unit-file/bin"],
    );
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(text.contains("in a form not read here"), "{text}");
}

/// systemd ends an `EnvironmentFile=` line at `\r` as well as `\n`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn carriage_returns_end_environment_file_lines_like_newlines() {
    for content in [
        "FOO=1\rPATH=/file/bin\n",
        "# note\rPATH=/file/bin\n",
        "FOO=1\r\nPATH=/file/bin\r\n",
    ] {
        let (_dir, file) = environment_file(content);
        let unit = UnitEnvironment {
            environment_files: vec![(file, false)],
            ..loaded_unit()
        };
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(text.contains("sets PATH"), "{content:?}: {text}");
    }
}

/// An assignment without a name is not followed past its continuation: refuse.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_environment_file_line_without_a_name_is_refused() {
    let (_dir, file) = environment_file("=\\\nPATH=/actual/bin\n");
    let unit = UnitEnvironment {
        environment_files: vec![(file, false)],
        ..loaded_unit()
    };
    let text = refusal(unrecorded_job_path(Ok(unit)).await);
    assert!(text.contains("EnvironmentFile"), "{text}");
}

/// Another quoted segment after a closed one is not followed: refuse.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_environment_file_with_successive_quoted_segments_is_refused() {
    for content in [
        "OTHER='first' 'second\nX=\"still-other'\nPATH=/actual/bin\nY=\"\"\n",
        "OTHER=\"a\"\"b\"\nSTORE=/s\n",
    ] {
        let (_dir, file) = environment_file(content);
        let unit = UnitEnvironment {
            environment_files: vec![(file, false)],
            ..loaded_unit()
        };
        let text = refusal(unrecorded_job_path(Ok(unit)).await);
        assert!(
            text.contains("quoting that is not followed"),
            "{content:?}: {text}"
        );
    }
}
