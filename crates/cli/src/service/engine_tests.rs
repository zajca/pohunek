//! Engine transaction tests against an in-memory service manager.
//!
//! The service-manager and daemon doubles exist only to inject failures and
//! interruptions at every journaled step; every filesystem effect (version
//! directories, `service.toml`, the record, the CLI copy, journals) is real
//! and lives below a temporary root.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

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
use pohunek_service_config::preflight::{StoreState, Verdict};
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
use crate::service::preflight::Judging;
use crate::service::usage::tests::{
    own_identity, spawn_when_exec_free, write_journal, write_live_journal, write_outdated_journal,
    SESSION,
};

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
const STEPS: [Step; 7] = [
    Step::Binaries,
    Step::Config,
    Step::Definition,
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
    fn judge<'a>(&'a self, context: &'a Context, _daemon: &'a Path) -> Judging<'a> {
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

/// An engine over `context` and `backend` that observes only this test's
/// own processes and, on Linux, verifies units with [`ACCEPTING_VERIFIER`].
fn engine_over<'a>(context: &'a Context, backend: &'a Backend) -> Engine<'a> {
    let mut engine = Engine::new(context, backend);
    engine.inspector = Box::new(OwnProcesses::default());
    let engine = engine.with_adoption_preflight(Box::new(InProcessPreflight::default()));
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
    /// Daemon definition replacements, each of which restarts the daemon.
    replaces: usize,
    sessions: Vec<SessionInfo>,
    workers: Vec<ServiceObservation>,
    /// Registered jobs whose observation raced; the lenient discovery omits
    /// them the way the systemd backend does, the strict one refuses.
    raced_workers: Vec<ServiceObservation>,
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
            let world = self.world();
            if world.fail_discover {
                return Err(supervisor::Error::Operation {
                    operation: "discover",
                    source: "injected failure".into(),
                });
            }
            if !world.raced_workers.is_empty() {
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
        native_session_id: None,
        native_session_path: None,
        active_agent: None,
        active_agent_base: None,
        active_agent_pid: None,
        active_agent_session_id: None,
        active_agent_session_path: None,
        project_id: None,
        project_label: None,
        metadata: BTreeMap::new(),
        is_linked_worktree: None,
        repo: None,
        branch: None,
        worktree_path: None,
        warnings: Vec::new(),
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
        }),
    }
}

struct Harness {
    _temp: tempfile::TempDir,
    root: PathBuf,
    prefix: PathBuf,
    context: Context,
    fake: Fake,
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
        let context = context(&root);
        let fake = Fake::default();
        let backend = Backend::new(
            Box::new(fake.clone()),
            Box::new(fake.clone()),
            Box::new(fake.clone()),
        );
        Self {
            _temp: temp,
            root,
            prefix,
            context,
            fake,
            backend,
        }
    }

    fn engine(&self) -> Engine<'_> {
        let mut engine = engine_over(&self.context, &self.backend);
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
        "metadata.jsonl.tmp.1.2",
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
            format!("daemon socket is served by pid {FOREIGN_PID}; the supervised job has no process"),
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
    once_free("the transaction lock", || harness.upgrade(V2))
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

    let Error::UpgradeAtRisk { sessions } = &error else {
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

    let Error::UpgradeAtRisk { sessions } = &error else {
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
    fn judge<'a>(&'a self, context: &'a Context, daemon: &'a Path) -> Judging<'a> {
        let call = self.inner.calls.load(Ordering::SeqCst);
        if call >= self.fail_from {
            return Box::pin(async move {
                Err(Error::UpgradePreflightFailed {
                    binary: daemon.to_path_buf(),
                    detail: "injected".to_owned(),
                })
            });
        }
        self.inner.judge(context, daemon)
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
    fn judge<'a>(&'a self, _context: &'a Context, daemon: &'a Path) -> Judging<'a> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Err(Error::UpgradePreflightFailed {
                binary: daemon.to_path_buf(),
                detail: "must not be consulted".to_owned(),
            })
        })
    }
}
