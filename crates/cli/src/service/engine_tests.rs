//! Engine transaction tests against an in-memory service manager.
//!
//! The service-manager and daemon doubles exist only to inject failures and
//! interruptions at every journaled step; every filesystem effect (version
//! directories, `service.toml`, the record, the CLI copy, journals) is real
//! and lives below a temporary root.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use pohunek_client::ClientError;
use pohunek_platform::supervisor::{DaemonSupervisor, Operation as Pending, ServiceId, Supervisor};
use pohunek_session_worker::RuntimePhase;
use protocol::{
    AgentKind, DaemonHealthResult, RuntimeGeneration, SessionCapabilities, SessionId,
    SessionRuntime, StateSource,
};

use super::*;
use crate::service::backend::{Call, Control};
use crate::service::context::tests::{context, temp_root};
use crate::service::layout::tests::stage_dir;
use crate::service::layout::CLI_NAME;
use crate::service::usage::tests::{write_journal, write_live_journal, SESSION};

const V1: &str = "1.0.0";
const V2: &str = "2.0.0";
const GENERATION: &str = "abcd2345";

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

#[derive(Debug, Default)]
struct World {
    registered: Option<JobDefinition>,
    running: bool,
    fail_install: bool,
    fail_discover: bool,
    silent_version: Option<String>,
    installs: usize,
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
            if world.fail_install {
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
            Ok(())
        })
    }

    fn replace<'a>(&'a self, definition: &'a JobDefinition) -> Pending<'a, ()> {
        Box::pin(async move {
            let mut world = self.world();
            if world.registered.is_none() {
                return Err(supervisor::Error::NotFound(daemon_id()));
            }
            world.registered = Some(definition.clone());
            world.running = true;
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
                    ServiceState::Stopped
                },
                process: None,
                definition: Some(definition.facts()),
            })
        })
    }

    fn uninstall(&self) -> Pending<'_, ()> {
        Box::pin(async move {
            let mut world = self.world();
            world.registered = None;
            world.running = false;
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
    fn health(&self) -> Call<'_, DaemonHealthResult> {
        Box::pin(async move {
            let (running, silent) = {
                let world = self.world();
                (world.running, world.silent_version.clone())
            };
            match self
                .daemon_version()
                .filter(|version| running && silent.as_ref() != Some(version))
            {
                Some(version) => Ok(DaemonHealthResult {
                    status: "ok".to_owned(),
                    daemon_version: version,
                    protocol_version: protocol::PROTOCOL_VERSION,
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
        agent_base: AgentKind::Shell,
        cwd: PathBuf::from("/tmp"),
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
            runtime_id: None,
            started_at: None,
            last_connected_at: None,
            loss_reason: None,
        }),
    }
}

struct Harness {
    _temp: tempfile::TempDir,
    root: PathBuf,
    context: Context,
    fake: Fake,
    backend: Backend,
}

impl Harness {
    fn new() -> Self {
        let (temp, root) = temp_root();
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
            context,
            fake,
            backend,
        }
    }

    fn engine(&self) -> Engine<'_> {
        let mut engine = Engine::new(&self.context, &self.backend);
        engine.ready_timeout = Duration::from_millis(500);
        engine.stop_timeout = Duration::from_secs(2);
        engine
    }

    fn prefix(&self) -> PathBuf {
        self.root.as_path().join("prefix")
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

#[tokio::test]
async fn a_failed_registration_rolls_everything_back() {
    let harness = Harness::new();
    harness.fake.world().fail_install = true;
    let error = harness.install(V1).await.expect_err("injected failure");
    assert!(matches!(error, Error::Supervisor { .. }), "{error:?}");
    harness.assert_clean();
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
        .expect_err("an untrusted versions directory stops the removal");
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

    std::fs::set_permissions(
        &versions,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("restore versions dir");
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
async fn an_untrusted_unit_directory_fails_before_anything_changes() {
    let harness = Harness::new();
    let open = harness.root.as_path().join("open");
    std::fs::create_dir_all(&open).expect("open dir");
    std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o777))
        .expect("chmod");
    let context = Context::new(
        harness.context.paths().clone(),
        harness.context.uid(),
        None,
        None,
        open.join("systemd/user"),
        PathBuf::from("/usr/bin/pohunek"),
    );
    let error = Engine::new(&context, &harness.backend)
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
    // A stopped job is the only stateless proof of termination; Unknown is
    // launchd's loaded job without a matching process, which may still spawn.
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
    let status = harness
        .engine()
        .status(config.as_ref())
        .await
        .expect("status after release");
    assert!(!status.transaction_in_progress);
    harness.upgrade(V2).await.expect("upgrade after release");
    harness.assert_installed(V2);
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
    let status = harness.engine().status(None).await.expect("status");
    assert!(!status.transaction_in_progress);
    assert_eq!(
        status.pending_transaction.map(|pending| pending.step),
        Some(Step::Config.as_str())
    );
    let report = harness.install(V1).await.expect("resume");
    assert!(report.resumed);
    harness.assert_installed(V1);
}
