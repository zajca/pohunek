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
use crate::service::usage::tests::{write_journal, SESSION};

const V1: &str = "1.0.0";
const V2: &str = "2.0.0";
const GENERATION: &str = "abcd2345";

/// Every step after which a transaction can be interrupted.
const STEPS: [Step; 6] = [
    Step::Binaries,
    Step::Config,
    Step::Definition,
    Step::Registered,
    Step::Ready,
    Step::Cli,
];

#[derive(Debug, Default)]
struct World {
    registered: Option<JobDefinition>,
    running: bool,
    fail_install: bool,
    silent_version: Option<String>,
    installs: usize,
    sessions: Vec<SessionInfo>,
    workers: Vec<ServiceObservation>,
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
        Box::pin(async move { Ok(self.world().workers.clone()) })
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
async fn an_install_interrupted_after_every_step_rolls_back_without_orphans() {
    for step in STEPS {
        let harness = Harness::new();
        let mut engine = harness.engine();
        engine.interrupt_after = Some(step);
        engine
            .install(&harness.staged(V1), &harness.prefix(), V1)
            .await
            .expect_err("interrupted");

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
    // The worker behind this journal is this test process: provably alive.
    let start = pohunek_platform::process::ProcessInspector::identity(
        &pohunek_platform::process::HostInspector::new(),
        std::process::id(),
    )
    .expect("identity")
    .expect("own process");
    write_journal(
        harness.context.paths(),
        "s-6",
        "w-6",
        &worker_exe,
        RuntimePhase::Live,
        std::process::id(),
    );
    rewrite_start_identity(&harness, "s-6", "w-6", &start.start_identity.to_string());
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

fn rewrite_start_identity(harness: &Harness, session: &str, worker: &str, identity: &str) {
    let path = harness
        .context
        .paths()
        .worker_journal(session, worker)
        .expect("journal path");
    let journal = pohunek_session_worker::Journal::new(&path);
    let mut record = journal.load().expect("load journal");
    record.worker_start_identity = identity.to_owned();
    journal.write(&record).expect("rewrite journal");
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
    write_journal(
        harness.context.paths(),
        SESSION,
        "w-1",
        &worker_exe,
        RuntimePhase::Live,
        1,
    );
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
