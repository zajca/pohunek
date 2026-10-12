//! Shared harness of the package lifecycle tests: a real daemon served from
//! this process over a hermetic environment, and the `pohunek` binary run
//! against it.
//!
//! The harness owns no package content; each test file adds its own fixture
//! archives through an `impl Harness` block.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-12

use std::fs;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
    OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
};
use package::registry::{Registry, RegistryState};
use package::{Limits, PackageDigest};
use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo};
use pohunek_daemon::catalog_anchor::CatalogTrust;
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::notifications::{
    AttentionCoordinator, AttentionCoordinatorTask, NotificationService,
};
use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher, WorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_test_support::env::{check_socket_path, TestEnv};
use pohunek_test_support::wait::guard;
use pohunek_test_support::worker_binary;
use serde_json::Value;
use tokio::sync::oneshot;

// The test-host process view: an unrelated process of a loaded test host whose
// ownership markers cannot be read (another test's fresh exec, a non-dumpable
// helper) never makes a session removal refuse.
#[path = "../../../daemon/src/procwatch/readable_host.rs"]
mod readable_host;

/// Mode of every private fixture directory.
pub(crate) const PRIVATE_MODE: u32 = 0o700;

/// Mode of a profile file.
pub(crate) const PROFILE_MODE: u32 = 0o600;

/// Overlay transport with no peers; the registry only has to be non-empty.
#[derive(Debug)]
struct EmptyTransport {
    id: OverlayId,
}

impl OverlayTransport for EmptyTransport {
    fn id(&self) -> &OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, _addr: IpAddr) -> Result<(), BindAddrError> {
        Ok(())
    }

    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
        Box::pin(async { Ok(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)) })
    }

    fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
        let overlay = self.id.clone();
        Box::pin(async move {
            Err(OverlayError::HostUnknown {
                host: host.to_owned(),
                overlay,
            })
        })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        let overlay = self.id.clone();
        Box::pin(async move {
            Err(OverlayError::HostUnknown {
                host: identity.value().to_owned(),
                overlay,
            })
        })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

pub(crate) fn overlay_registry() -> OverlayRegistry {
    let transport = Arc::new(EmptyTransport {
        id: OverlayId::new("test").expect("overlay id"),
    });
    let configured = ConfiguredTransport::new(transport, 18_722).expect("configured overlay");
    OverlayRegistry::new(vec![configured]).expect("registry")
}

/// A package archive on disk with the digest the daemon will see.
pub(crate) struct Built {
    pub(crate) path: PathBuf,
    pub(crate) digest: PackageDigest,
}

/// A real daemon serving the hermetic environment's runtime socket, plus the
/// directories the `pohunek` binary and the daemon share.
///
/// The daemon runs on a thread of its own, on a runtime that is dropped when
/// the daemon stops, as a daemon process's tasks end with it. A daemon started
/// after another on the same root therefore never meets the other's worker
/// connections.
pub(crate) struct Harness {
    pub(crate) env: TestEnv,
    pub(crate) plugins: PathBuf,
    pub(crate) agents: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: std::thread::JoinHandle<()>,
    /// The session registry the server serves.
    pub(crate) sessions: SessionRegistry,
    /// The launcher of this daemon's workers.
    launcher: Arc<dyn WorkerLauncher>,
}

/// What a daemon thread hands back once it serves.
struct Started {
    env: TestEnv,
    plugins: PathBuf,
    agents: PathBuf,
    sessions: SessionRegistry,
    launcher: Arc<dyn WorkerLauncher>,
}

/// What survives a daemon shutdown: the environment, and the launcher whose job
/// records a daemon started later on the same root must observe.
pub(crate) struct Surviving {
    env: TestEnv,
    launcher: Arc<dyn WorkerLauncher>,
}

/// Which notification components the daemon is built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inbox {
    /// No notification inbox.
    Absent,
    /// A durable inbox without the attention debounce coordinator.
    Immediate,
    /// A durable inbox with the attention debounce coordinator the daemon
    /// binary attaches.
    Debounced,
}

impl Harness {
    pub(crate) async fn start() -> Self {
        Self::start_with_trust(CatalogTrust::Absent).await
    }

    /// Starts the daemon with `catalog_trust` as the host's catalog trust
    /// anchor, the way a release daemon holds the one beside its executable.
    pub(crate) async fn start_with_trust(catalog_trust: CatalogTrust) -> Self {
        Self::start_with_trust_and_worker(catalog_trust, worker_binary()).await
    }

    /// Starts the production server with a caller-owned worker executable.
    pub(crate) async fn start_with_worker(worker_executable: PathBuf) -> Self {
        Self::start_with_trust_and_worker(CatalogTrust::Absent, worker_executable).await
    }

    /// Starts the daemon with a durable notification inbox, so `notification.*`
    /// methods and the notification events of `subscribe` are live.
    ///
    /// No attention debounce coordinator is attached: a create with a
    /// `turn:<id>` or `attention:<id>` dedupe key is refused with
    /// `notifications_not_configured`, and every other create persists at once.
    pub(crate) async fn start_with_notifications() -> Self {
        Self::start_with(CatalogTrust::Absent, worker_binary(), Inbox::Immediate).await
    }

    /// Starts the daemon with a durable notification inbox and the attention
    /// debounce coordinator, so a create with a `turn:<id>` or `attention:<id>`
    /// dedupe key is held pending for the policy debounce window and then
    /// committed, as in the daemon binary.
    pub(crate) async fn start_with_debounced_notifications() -> Self {
        Self::start_with(CatalogTrust::Absent, worker_binary(), Inbox::Debounced).await
    }

    async fn start_with_trust_and_worker(
        catalog_trust: CatalogTrust,
        worker_executable: PathBuf,
    ) -> Self {
        Self::start_with(catalog_trust, worker_executable, Inbox::Absent).await
    }

    async fn start_with(
        catalog_trust: CatalogTrust,
        worker_executable: PathBuf,
        inbox: Inbox,
    ) -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        Self::serve(
            env,
            Arc::new(SubprocessWorkerLauncher::new()),
            DaemonSpec {
                catalog_trust,
                worker_executable,
                inbox,
                durable: false,
                reconcile: false,
            },
        )
        .await
    }

    /// Starts a daemon whose session records are durable under the root, so a
    /// daemon started later on the same root can adopt the workers still running.
    pub(crate) async fn start_durable() -> Self {
        Self::start_durable_with_trust(CatalogTrust::Absent).await
    }

    /// [`Self::start_durable`] with `catalog_trust` as the host's catalog trust
    /// anchor, so an official package can be installed into it.
    pub(crate) async fn start_durable_with_trust(catalog_trust: CatalogTrust) -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        Self::serve(
            env,
            Arc::new(SubprocessWorkerLauncher::new()),
            DaemonSpec {
                catalog_trust,
                worker_executable: worker_binary(),
                inbox: Inbox::Absent,
                durable: true,
                reconcile: false,
            },
        )
        .await
    }

    /// Stops this daemon's server and returns what survives it. The daemon's
    /// workers keep running, as they do when the daemon process exits.
    pub(crate) async fn shut_down(self) -> Surviving {
        self.sessions.begin_daemon_shutdown();
        let env_launcher = self.join().await;
        env_launcher
    }

    /// Restarts the daemon on the same root: the new daemon reconciles the
    /// workers the previous one left running.
    pub(crate) async fn restart(self) -> Self {
        let surviving = self.shut_down().await;
        Self::start_over(surviving).await
    }

    /// Starts a durable daemon over the surviving environment that adopts the
    /// workers already running on its root.
    pub(crate) async fn start_over(surviving: Surviving) -> Self {
        Self::serve(
            surviving.env,
            surviving.launcher,
            DaemonSpec {
                catalog_trust: CatalogTrust::Absent,
                worker_executable: worker_binary(),
                inbox: Inbox::Absent,
                durable: true,
                reconcile: true,
            },
        )
        .await
    }

    /// Signals the server thread to stop and joins it, returning what survives.
    async fn join(self) -> Surviving {
        let Self {
            env,
            shutdown,
            task,
            launcher,
            ..
        } = self;
        let _ = shutdown.send(());
        tokio::task::spawn_blocking(move || task.join())
            .await
            .expect("join the daemon thread")
            .expect("the server thread stops without a panic");
        Surviving { env, launcher }
    }

    /// Serves the daemon of `env` on a thread of its own. A durable daemon keeps
    /// its session records in the root's metadata store; a reconciling one adopts
    /// the surviving workers before its socket accepts a connection.
    async fn serve(env: TestEnv, launcher: Arc<dyn WorkerLauncher>, spec: DaemonSpec) -> Self {
        let (ready, started) = oneshot::channel::<Started>();
        let (shutdown, stop) = oneshot::channel::<()>();
        let task = std::thread::Builder::new()
            .name("harness-daemon".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("the daemon runtime");
                runtime.block_on(Self::run_daemon(env, launcher, spec, ready, stop));
            })
            .expect("spawn the daemon thread");
        let Started {
            env,
            plugins,
            agents,
            sessions,
            launcher,
        } = started.await.expect("the daemon starts");
        Self {
            env,
            plugins,
            agents,
            shutdown,
            task,
            sessions,
            launcher,
        }
    }

    /// Builds and serves one daemon on the calling thread's runtime, reporting
    /// its started state through `ready` and serving until `stop` fires.
    async fn run_daemon(
        env: TestEnv,
        launcher: Arc<dyn WorkerLauncher>,
        spec: DaemonSpec,
        ready: oneshot::Sender<Started>,
        stop: oneshot::Receiver<()>,
    ) {
        let DaemonSpec {
            catalog_trust,
            worker_executable,
            inbox,
            durable,
            reconcile,
        } = spec;
        let dirs = prepare_directories(&env);
        let environment = worker_environment(&env, &dirs.socket);
        let config = registry_config(
            &env,
            &dirs,
            &environment,
            catalog_trust,
            worker_executable,
            durable,
        );
        let registry = SessionRegistry::new_production_with_inspector(
            config,
            Arc::clone(&launcher),
            Arc::new(readable_host::ReadableHost::new()),
        )
        .expect("the production registry opens the plugin root");
        // The event log starts before any session is created or adopted, so
        // every lifecycle event of a durable daemon is recorded.
        if durable {
            registry
                .spawn_event_log()
                .expect("open the daemon event log");
        }
        let log_owner = registry.clone();
        if reconcile {
            Box::pin(registry.reconcile_workers())
                .await
                .expect("adopt the workers the previous daemon left running");
        }
        let sessions = registry.clone();
        let (server, attention) = bind_server(&env, &dirs.socket, registry, inbox).await;
        let started = Started {
            env,
            plugins: dirs.plugins,
            agents: dirs.agents,
            sessions,
            launcher,
        };
        let _ = ready.send(started);
        server
            .serve(async move {
                let _ = stop.await;
            })
            .await;
        if let Some(attention) = attention {
            attention.shutdown().await;
        }
        log_owner.shutdown_event_log().await;
    }

    /// Removes every session the test started, so no durable worker outlives
    /// the test, then stops the server.
    pub(crate) async fn stop(self) {
        let (code, listed) = self.json(&["session", "list"]).await;
        assert_eq!(code, 0, "{listed}");
        let ids: Vec<String> = listed["ok"]
            .as_array()
            .expect("sessions")
            .iter()
            .map(|entry| entry["id"].as_str().expect("session id").to_owned())
            .collect();
        for id in &ids {
            let (code, removed) = self.json(&["session", "rm", id]).await;
            assert_eq!(code, 0, "{removed}");
        }
        let _ = self.shutdown.send(());
        tokio::task::spawn_blocking(move || self.task.join())
            .await
            .expect("join the daemon thread")
            .expect("the server thread stops without a panic");
    }

    /// Runs `pohunek` with the hermetic environment.
    pub(crate) async fn run(&self, arguments: &[&str]) -> Output {
        let mut command = self
            .env
            .tokio_command(pohunek_test_support::bin_exe("pohunek"));
        command.args(arguments);
        guard("the pohunek binary to finish", command.output())
            .await
            .expect("run the pohunek binary")
    }

    /// Runs `pohunek <arguments> --json` and returns the exit code and the
    /// single JSON document on stdout.
    pub(crate) async fn json(&self, arguments: &[&str]) -> (i32, Value) {
        let mut all = arguments.to_vec();
        all.push("--json");
        let output = self.run(&all).await;
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout of {all:?} is one JSON document ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        (output.status.code().expect("exit code"), document)
    }

    /// The daemon's control socket in the hermetic runtime directory.
    pub(crate) fn socket(&self) -> PathBuf {
        self.env.runtime_dir().join("pohunek").join("daemon.sock")
    }

    /// The package registry as the plugin root records it on disk.
    pub(crate) fn registry(&self) -> RegistryState {
        Registry::open_at(&self.plugins, Limits::DEFAULT)
            .expect("open the plugin root")
            .state()
            .expect("read the registry state")
    }

    /// Writes `<config>/pohunek/agents/<name>.toml`.
    pub(crate) fn profile(&self, name: &str, text: &str) -> PathBuf {
        let path = self.agents.join(format!("{name}.toml"));
        fs::write(&path, text).expect("write the profile");
        fs::set_permissions(&path, fs::Permissions::from_mode(PROFILE_MODE)).expect("profile mode");
        path
    }

    /// The directory holding the installed tree of `digest`.
    pub(crate) fn root_of(&self, digest: &PackageDigest) -> PathBuf {
        let hex = digest.as_str().trim_start_matches("sha256:");
        self.plugins.join("packages").join(hex)
    }
}

/// Which daemon a harness serves: the production configuration it is built
/// with, and whether its session records persist and adopt surviving workers.
struct DaemonSpec {
    catalog_trust: CatalogTrust,
    worker_executable: PathBuf,
    inbox: Inbox,
    durable: bool,
    reconcile: bool,
}

/// The directories a daemon of the harness uses, all under its environment.
struct DaemonDirs {
    socket: PathBuf,
    plugins: PathBuf,
    agents: PathBuf,
}

/// Creates the runtime and agents directories with private modes and returns
/// the paths the daemon serves from.
fn prepare_directories(env: &TestEnv) -> DaemonDirs {
    let runtime = env.runtime_dir().join("pohunek");
    fs::create_dir_all(&runtime).expect("create the runtime directory");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(PRIVATE_MODE))
        .expect("private runtime directory");
    let socket = runtime.join("daemon.sock");
    check_socket_path(&socket).expect("the socket path fits sun_path");
    let agents = env.config_home().join("pohunek/agents");
    fs::create_dir_all(&agents).expect("create the agents directory");
    fs::set_permissions(&agents, fs::Permissions::from_mode(PRIVATE_MODE))
        .expect("private agents directory");
    DaemonDirs {
        socket,
        plugins: env.state_home().join("pohunek/plugins"),
        agents,
    }
}

/// The directories the workers of the daemon at `socket` run and keep state in.
fn worker_environment(env: &TestEnv, socket: &Path) -> SubprocessWorkerEnvironment {
    let worker_home = env.root().join("worker");
    SubprocessWorkerEnvironment {
        runtime_home: worker_home.join("runtime"),
        state_home: worker_home.join("state"),
        data_home: worker_home.join("data"),
        config_home: worker_home.join("config"),
        cache_home: worker_home.join("cache"),
        home: worker_home,
        daemon_socket: socket.to_path_buf(),
    }
}

/// The registry configuration of a daemon of the harness.
fn registry_config(
    env: &TestEnv,
    dirs: &DaemonDirs,
    environment: &SubprocessWorkerEnvironment,
    catalog_trust: CatalogTrust,
    worker_executable: PathBuf,
    durable: bool,
) -> SessionRegistryConfig {
    SessionRegistryConfig {
        socket_path: Some(dirs.socket.clone()),
        plugins_dir: Some(dirs.plugins.clone()),
        agents_dir: Some(dirs.agents.clone()),
        // A profile-backed launch freezes a keyed revision, which needs the
        // host's revision key directory.
        host_state_dir: Some(env.root().join("host-state")),
        catalog_trust,
        worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
        worker_state_root: Some(environment.state_home.join("pohunek/workers")),
        supervision: Some(
            environment
                .supervision(worker_executable)
                .with_environment_source(EnvironmentSource::fixed(env.environment().clone())),
        ),
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        store_path: durable.then(|| metadata_store(env)),
        event_log_dir: durable.then(|| env.root().join("events")),
        ..SessionRegistryConfig::default()
    }
}

/// The metadata store of a durable daemon, in a directory only its owner reads.
fn metadata_store(env: &TestEnv) -> PathBuf {
    let directory = env.root().join("metadata");
    fs::create_dir_all(&directory).expect("create the metadata directory");
    fs::set_permissions(&directory, fs::Permissions::from_mode(PRIVATE_MODE))
        .expect("private metadata directory");
    directory.join("metadata.jsonl")
}

/// Wraps `registry` in the daemon's state and binds the control socket at
/// `socket`. Returns the server and the attention coordinator the inbox runs.
async fn bind_server(
    env: &TestEnv,
    socket: &Path,
    registry: SessionRegistry,
    inbox: Inbox,
) -> (ControlServer, Option<AttentionCoordinatorTask>) {
    let governance = Arc::new(
        HostGovernanceService::open(env.root().join("governance"))
            .await
            .expect("open the host-governance service"),
    );
    let notifications = (inbox != Inbox::Absent).then(|| {
        NotificationService::open(&env.root().join("daemon-data"))
            .expect("open the notification inbox")
    });
    let (coordinator, attention) = match (&notifications, inbox) {
        (Some(notifications), Inbox::Debounced) => {
            let (coordinator, task) = AttentionCoordinator::spawn(notifications.clone(), &registry);
            (Some(coordinator), Some(task))
        }
        _ => (None, None),
    };
    let mut state = DaemonState::new(
        HealthInfo::new("0.0.0-test"),
        registry,
        governance,
        overlay_registry(),
    );
    if let Some(notifications) = notifications {
        state = state.with_notifications(notifications);
    }
    if let Some(coordinator) = coordinator {
        state = state.with_attention_coordinator(coordinator);
    }
    let server = ControlServer::bind_with_state(socket, state)
        .await
        .expect("the control server binds");
    (server, attention)
}

pub(crate) fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

pub(crate) fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub(crate) fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
