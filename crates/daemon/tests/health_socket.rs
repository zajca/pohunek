//! Integration test: the daemon's control server answers `daemon.health` over a
//! real Unix socket using newline-delimited JSON.
//!
//! This is the milestone-2 checkpoint ("CLI `doctor` + `daemon start` talk over
//! the socket") exercised at the protocol layer: it binds the actual
//! `ControlServer` on a temp socket, connects a raw client, and verifies the
//! response carries the daemon and protocol versions. It also covers
//! stale-socket recovery and the `method_not_found` path.

// The daemon's unit tests share this view; it keeps `session.remove`
// independent of the test host's processes whose markers cannot be read.
#[path = "health_socket/branch_source.rs"]
mod branch_source;
#[path = "health_socket/native_attestation.rs"]
mod native_attestation;
#[path = "health_socket/native_recovery.rs"]
mod native_recovery;
#[path = "support/official_runtimes.rs"]
mod official_runtimes;
#[path = "health_socket/project_cwd.rs"]
mod project_cwd;
#[path = "../src/procwatch/readable_host.rs"]
mod readable_host;
#[path = "health_socket/required_hooks.rs"]
mod required_hooks;
#[path = "support/root_reporter.rs"]
mod root_reporter;
#[path = "health_socket/session_metadata.rs"]
mod session_metadata;
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures::{FutureExt, SinkExt, StreamExt};
use protocol::{
    event, method, AgentActivity, AttachHeader, ErrorClass, Event, HostDiscoverParams, HostRecord,
    IntegrationDoctorResult, IntegrationInstallState, IntegrationStatusResult,
    IntegrationUninstallResult, IntegrationUninstallState, NotificationCreateParams,
    NotificationCreateResult, NotificationDeleteParams, NotificationDeleteResult, NotificationKind,
    NotificationKindPolicy, NotificationListParams, NotificationListResult, NotificationPolicy,
    NotificationPolicyParams, NotificationPolicyResult, NotificationRetentionParams,
    NotificationRetentionResult, NotificationSeverity, NotificationSource, NotificationStatus,
    NotificationUpdateParams, NotificationUpdateResult, ProcessStartIdentity, ProjectInfo,
    ProjectShowResult, ProjectWorktree, ReportSequence, Request as ProtocolRequest, Response,
    RuntimeId, RuntimeRef, SessionAttachParams, SessionAttachResult, SessionDetachParams,
    SessionDetachResult, SessionId, SessionInfo, SessionInputParams, SessionInputResult,
    SessionInputWait, SessionListFilter, SessionListParams, SessionNewParams, SessionRemoveResult,
    SessionReportAgentParams, SessionReportAgentResult, SessionReportNativeIdParams,
    SessionReportNativeIdResult, SessionResizeParams, SessionResizeResult, SessionState,
    SessionStopResult, SessionWarningKind, StateSource, TerminalDimensions, WorktreeRemoveParams,
    WorktreeRemoveResult, ENV_DAEMON_ID, ENV_SESSION_ID, PROTOCOL_VERSION,
};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo};
use pohunek_daemon::events::{spawn_drain, EventLog};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::notifications::NotificationService;
use pohunek_daemon::procwatch::{HostInspector, ProcessInspector};
use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_daemon::store::{ResumeBinding, Store, WorktreeBinding};
use pohunek_paths::{
    APP_DIR, HOST_APPROVAL_KEY_NAME, HOST_GOVERNANCE_NAME, HOST_IDENTITY_NAME,
    HOST_STATE_LOCK_NAME, HOST_STATE_SUBDIR, LOGS_SUBDIR, PLUGINS_SUBDIR, WORKERS_SUBDIR,
};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait::{self, wait_until, HANG_GUARD};
use pohunek_test_support::workers::WorkerGuard;
use pohunek_test_support::{bin_exe, worker_binary};

// Sessions use the production stop grace, `SessionRegistryConfig::default()`:
// a shorter grace reaches the forced-stop path on a loaded host, and no test
// here is about the grace.
/// Bounds every control request and socket operation in the real-daemon regression.
const DAEMON_CONTROL_REQUEST_TIMEOUT: Duration = HANG_GUARD;
/// Bounds child cleanup so a failed regression test cannot stall the suite indefinitely.
const DAEMON_SHUTDOWN_TIMEOUT: Duration = HANG_GUARD;

struct Request;

impl Request {
    fn make(id: &str, method: &str, params: Value) -> ProtocolRequest {
        ProtocolRequest::new(id, method, params).expect("valid test request")
    }
}

/// Prepends a directory to `PATH` and holds the process-environment lock until
/// dropped.
struct PathGuard {
    _env: ProcessEnv,
}

impl PathGuard {
    fn prepend(path: &Path) -> Self {
        let mut env = ProcessEnv::lock();
        let mut paths = vec![path.to_path_buf()];
        if let Some(old_path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&old_path));
        }
        let joined = std::env::join_paths(paths).expect("join test PATH");
        env.set("PATH", joined);
        Self { _env: env }
    }
}

/// Points every base directory and provider override at a fresh temp root and
/// holds the process-environment lock until dropped.
struct XdgGuard {
    /// Dropped first: restores the variables and releases the lock before the
    /// root below is removed. Tests change further variables through it.
    env: ProcessEnv,
    root: TestDir,
}

impl XdgGuard {
    fn set_all(tag: &str) -> Self {
        Self::set_all_after(tag, |_| {})
    }

    /// Runs `before_isolation` with the lock held, before the isolated
    /// environment is applied.
    fn set_all_after(tag: &str, before_isolation: impl FnOnce(&mut ProcessEnv)) -> Self {
        let mut env = ProcessEnv::lock();
        before_isolation(&mut env);
        let root = temp_dir(tag);
        env.set("XDG_RUNTIME_DIR", root.join("runtime"))
            .set("XDG_STATE_HOME", root.join("state"))
            .set("XDG_DATA_HOME", root.join("data"))
            .set("XDG_CONFIG_HOME", root.join("config"))
            .set("XDG_CACHE_HOME", root.join("cache"))
            .set("HOME", root.join("home"))
            .set("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .set("CODEX_HOME", root.join("home/.codex"));
        Self { env, root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
}

/// A per-test directory, removed with its contents when dropped.
struct TestDir(tempfile::TempDir);

impl std::ops::Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.0.path()
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

/// A unique socket path inside a dedicated per-test directory.
///
/// The server enforces the directory's mode on bind, so the socket must live in
/// a directory we own (not `/tmp` itself, which is root-owned with the sticky
/// bit). This mirrors the real daemon, which always binds inside its own
/// `pohunek` runtime subdir. The directory is removed when the value drops.
struct TestSocket {
    path: PathBuf,
    dir: TestDir,
}

impl std::ops::Deref for TestSocket {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestSocket {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl TestSocket {
    /// A private directory next to the socket, for sessions to work in.
    fn work_dir(&self) -> PathBuf {
        self.dir.join(WORK_DIR_NAME)
    }
}

/// Directory name of [`TestSocket::work_dir`].
const WORK_DIR_NAME: &str = "work";

fn temp_socket(tag: &str) -> TestSocket {
    let dir = temp_dir(tag);
    std::fs::create_dir(dir.join(WORK_DIR_NAME)).expect("create the session working directory");
    TestSocket {
        path: dir.join("daemon.sock"),
        dir,
    }
}

async fn governance_service(socket: &Path) -> Arc<HostGovernanceService> {
    let state_root = socket
        .parent()
        .expect("test socket has an isolated parent")
        .join("state");
    Arc::new(
        HostGovernanceService::open(state_root)
            .await
            .expect("open real isolated host-governance service"),
    )
}

fn temp_dir(tag: &str) -> TestDir {
    // Owner-private, uniquely named, and short enough for the daemon socket
    // below it on macOS.
    TestDir(
        pohunek_test_support::tempdir_with_prefix(&format!("ph-{tag}-"))
            .expect("create test socket dir"),
    )
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write executable test script");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path)
            .expect("test script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod test script");
    }
}

/// Spawn the control server on `socket`, returning a shutdown trigger and the
/// server task handle.
async fn spawn_server(
    socket: &std::path::Path,
    version: &str,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    spawn_server_with_overlay(socket, version, support::overlay_registry()).await
}

async fn spawn_server_with_overlay(
    socket: &std::path::Path,
    version: &str,
    overlay: overlay::OverlayRegistry,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let health = HealthInfo::new(version);
    let server = ControlServer::bind(socket, health, governance_service(socket).await, overlay)
        .await
        .expect("server binds");
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        server
            .serve(async move {
                let _ = rx.await;
            })
            .await;
    });
    (tx, handle)
}

#[tokio::test]
async fn integration_status_accepts_null_at_daemon_boundary() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let ambient = temp_dir("integration-status-ambient-provider-overrides");
    let ambient_claude = ambient.join("claude");
    let ambient_codex = ambient.join("codex");
    std::fs::create_dir_all(&ambient_claude).expect("create ambient Claude override");
    std::fs::create_dir_all(&ambient_codex).expect("create ambient Codex override");
    std::fs::write(ambient_claude.join("sentinel"), "ambient Claude\n")
        .expect("write ambient Claude sentinel");
    std::fs::write(ambient_codex.join("sentinel"), "ambient Codex\n")
        .expect("write ambient Codex sentinel");
    let ambient_before = tree_snapshot(&ambient);
    let claude_override = ambient_claude.clone();
    let codex_override = ambient_codex.clone();
    let xdg = XdgGuard::set_all_after("integration-status-rpc", move |env| {
        env.set("CLAUDE_CONFIG_DIR", claude_override)
            .set("CODEX_HOME", codex_override);
    });
    let claude = xdg.home().join(".claude");
    let codex = xdg.home().join(".codex");
    assert_eq!(
        std::env::var_os("CLAUDE_CONFIG_DIR"),
        Some(claude.clone().into())
    );
    assert_eq!(std::env::var_os("CODEX_HOME"), Some(codex.clone().into()));
    std::fs::create_dir_all(&claude).expect("create isolated Claude config");
    std::fs::create_dir_all(&codex).expect("create isolated Codex config");
    pohunek_daemon::integration::install_claude(&claude).expect("install Claude fixture");
    pohunek_daemon::integration::install_codex(&codex).expect("install Codex fixture");
    let before = tree_snapshot(&xdg.home());
    let socket = temp_socket("integration-status-rpc");
    // The daemon resolves provider homes from its own environment, which the
    // guard above isolates.
    let (shutdown, server, _) = spawn_server_serving_official_runtimes_in(
        &socket,
        "test",
        support::hermetic_registry_config(),
        EnvironmentSource::Process,
    )
    .await;
    let mut framed = connect(&socket).await;
    let request = Request::make(
        "integration-status",
        method::INTEGRATION_STATUS,
        serde_json::Value::Null,
    );
    let payload = ok_payload(exchange(&mut framed, &request).await);

    let result: IntegrationStatusResult =
        serde_json::from_value(payload).expect("deserialize status result");
    assert_eq!(result.agents.len(), 2);
    assert!(result.agents.iter().all(|agent| agent.available));
    assert!(result
        .agents
        .iter()
        .all(|agent| agent.state == IntegrationInstallState::Current));
    assert!(result.agents.iter().all(|agent| agent.warnings.is_empty()));
    assert_eq!(
        tree_snapshot(&xdg.home()),
        before,
        "integration.status must not mutate provider configuration"
    );
    assert_eq!(
        tree_snapshot(&ambient),
        ambient_before,
        "isolated status must not inspect or mutate ambient provider overrides"
    );

    framed.get_mut().shutdown().await.expect("close client");
    shutdown.send(()).expect("send integration status shutdown");
    server.await.expect("integration status server task");
}

#[tokio::test]
async fn integration_status_rejects_unknown_params_at_daemon_boundary() {
    let socket = temp_socket("integration-status-unknown-params");
    let (shutdown, server) = spawn_server(&socket, "test").await;
    let mut framed = connect(&socket).await;
    let request = Request::make(
        "integration-status-unknown-params",
        method::INTEGRATION_STATUS,
        serde_json::json!({ "agent": "codex", "unexpected": true }),
    );

    let response = exchange(&mut framed, &request).await;

    assert_eq!(response.id(), "integration-status-unknown-params");
    assert_eq!(err_payload(response).code, "bad_request");
    framed.get_mut().shutdown().await.expect("close client");
    shutdown.send(()).expect("send integration status shutdown");
    server.await.expect("integration status server task");
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "The RPC scenario drives uninstall and doctor at the daemon boundary in one flow."
)]
async fn integration_uninstall_and_doctor_run_at_daemon_boundary() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut xdg = XdgGuard::set_all("integration-uninstall-doctor-rpc");
    let claude = xdg.home().join(".claude");
    let codex = xdg.home().join(".codex");
    std::fs::create_dir_all(&claude).expect("create isolated Claude config");
    std::fs::create_dir_all(&codex).expect("create isolated Codex config");
    xdg.env
        .set("CLAUDE_CONFIG_DIR", &claude)
        .set("CODEX_HOME", &codex);
    pohunek_daemon::integration::install_claude(&claude).expect("install Claude fixture");
    pohunek_daemon::integration::install_codex(&codex).expect("install Codex fixture");
    let socket = temp_socket("integration-uninstall-doctor-rpc");
    // The daemon resolves provider homes from its own environment, which the
    // guard above isolates.
    let (shutdown, server, _) = spawn_server_serving_official_runtimes_in(
        &socket,
        "test",
        support::hermetic_registry_config(),
        EnvironmentSource::Process,
    )
    .await;
    let mut framed = connect(&socket).await;

    let doctor = Request::make(
        "integration-doctor",
        method::INTEGRATION_DOCTOR,
        serde_json::json!({ "agent": "codex" }),
    );
    let doctor: IntegrationDoctorResult =
        serde_json::from_value(ok_payload(exchange(&mut framed, &doctor).await))
            .expect("deserialize doctor result");
    assert_eq!(doctor.agents.len(), 1);
    assert_eq!(doctor.agents[0].agent, RuntimeRef::codex());
    assert_eq!(
        doctor.agents[0].status.as_ref().expect("status").state,
        IntegrationInstallState::Current
    );

    let all_doctor = Request::make(
        "integration-doctor-all",
        method::INTEGRATION_DOCTOR,
        serde_json::json!({}),
    );
    let all_doctor: IntegrationDoctorResult =
        serde_json::from_value(ok_payload(exchange(&mut framed, &all_doctor).await))
            .expect("deserialize unfiltered doctor result");
    assert_eq!(all_doctor.agents.len(), 2);

    let unknown = Request::make(
        "integration-doctor-unknown",
        method::INTEGRATION_DOCTOR,
        serde_json::json!({ "unexpected": true }),
    );
    assert_eq!(
        err_payload(exchange(&mut framed, &unknown).await).code,
        "bad_request"
    );

    let uninstall = Request::make(
        "integration-uninstall",
        method::INTEGRATION_UNINSTALL,
        serde_json::json!({ "agent": "claude" }),
    );
    let removed_payload = ok_payload(exchange(&mut framed, &uninstall).await);
    assert_eq!(removed_payload["uninstalled"][0]["state"], "removed");
    let removed: IntegrationUninstallResult =
        serde_json::from_value(removed_payload).expect("deserialize uninstall result");
    assert_eq!(removed.uninstalled.len(), 1);
    assert_eq!(
        removed.uninstalled[0].state,
        IntegrationUninstallState::Removed
    );
    assert!(!claude.join("hooks").join("pohunek-agent-state.sh").exists());
    assert!(codex.join("pohunek-agent-state.sh").exists());

    let again = Request::make(
        "integration-uninstall-again",
        method::INTEGRATION_UNINSTALL,
        serde_json::json!({ "agent": "claude" }),
    );
    let again_payload = ok_payload(exchange(&mut framed, &again).await);
    assert_eq!(again_payload["uninstalled"][0]["state"], "not_installed");
    let again: IntegrationUninstallResult =
        serde_json::from_value(again_payload).expect("deserialize repeated uninstall result");
    assert_eq!(
        again.uninstalled[0].state,
        IntegrationUninstallState::NotInstalled
    );

    for (id, params) in [
        ("integration-uninstall-null", serde_json::Value::Null),
        ("integration-uninstall-empty", serde_json::json!({})),
        (
            "integration-uninstall-unknown",
            serde_json::json!({ "agent": "claude", "everything": true }),
        ),
    ] {
        let invalid_request = Request::make(id, method::INTEGRATION_UNINSTALL, params);
        assert_eq!(
            err_payload(exchange(&mut framed, &invalid_request).await).code,
            "bad_request",
            "{id}: an invalid uninstall request must fail"
        );
    }

    framed.get_mut().shutdown().await.expect("close client");
    shutdown.send(()).expect("send integration shutdown");
    server.await.expect("integration server task");
}

fn tree_snapshot(root: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, u32, Vec<u8>)>) {
        use std::os::unix::fs::PermissionsExt;

        let mut children = std::fs::read_dir(path)
            .expect("read snapshot directory")
            .map(|entry| entry.expect("read snapshot entry").path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            let metadata = std::fs::symlink_metadata(&child).expect("snapshot metadata");
            let relative = child.strip_prefix(root).expect("relative snapshot path");
            let content = if metadata.is_file() {
                std::fs::read(&child).expect("snapshot file")
            } else {
                Vec::new()
            };
            entries.push((
                relative.to_path_buf(),
                metadata.permissions().mode(),
                content,
            ));
            if metadata.is_dir() {
                visit(root, &child, entries);
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

/// Build a `SessionRegistry` wired to a real `SubprocessWorkerLauncher` (the
/// built `pohunek-sessiond`), rooted under a unique per-call worker home, so
/// `session.new` can actually launch a durable worker instead of failing with
/// `worker_backend_required`.
///
/// The returned directory is the worker home; the caller keeps it alive for as
/// long as the registry runs.
fn worker_backed_registry(
    socket: &std::path::Path,
    config: SessionRegistryConfig,
) -> (SessionRegistry, tempfile::TempDir) {
    worker_backed_registry_in(socket, config, support::hermetic_environment_source())
}

/// [`worker_backed_registry`] whose agent children start from `environment`.
fn worker_backed_registry_in(
    socket: &std::path::Path,
    mut config: SessionRegistryConfig,
    environment: EnvironmentSource,
) -> (SessionRegistry, tempfile::TempDir) {
    // Short, because the worker nests its control socket below it.
    let worker_home_dir =
        pohunek_test_support::tempdir_with_prefix("pw-h-").expect("create the worker home");
    let worker_home = worker_home_dir.path().to_path_buf();
    let worker_environment = SubprocessWorkerEnvironment {
        runtime_home: worker_home.join("runtime"),
        state_home: worker_home.join("state"),
        data_home: worker_home.join("data"),
        config_home: worker_home.join("config"),
        cache_home: worker_home.join("cache"),
        home: worker_home.clone(),
        daemon_socket: socket.to_path_buf(),
    };
    config.socket_path = Some(socket.to_path_buf());
    config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
    config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
    config.supervision = Some(
        worker_environment
            .supervision(worker_binary())
            .with_environment_source(environment),
    );
    let launcher = Arc::new(SubprocessWorkerLauncher::new());
    let inspector = Arc::new(readable_host::ReadableHost::new());
    let registry = if config.plugins_dir.is_some() {
        SessionRegistry::new_production_with_inspector(config, launcher, inspector)
            .expect("package-backed registry opens the plugin root")
    } else {
        SessionRegistry::new_with_launcher_and_inspector(config, launcher, inspector)
    };
    (registry, worker_home_dir)
}

/// Spawn the control server with a custom shell command.
///
/// Returns the shutdown sender, the server task, and the worker home
/// directory the registry's real workers run under (their journals live
/// below `<worker home>/state/pohunek/workers/<session>`).
async fn spawn_server_with_config(
    socket: &std::path::Path,
    version: &str,
    config: SessionRegistryConfig,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
) {
    let event_log_dir = config
        .event_log_dir
        .clone()
        .map(|dir| (dir, config.event_retention));
    let (registry, worker_home_dir) = worker_backed_registry(socket, config);
    serve_registry(socket, version, event_log_dir, registry, worker_home_dir).await
}

/// [`spawn_server_with_config`] over a plugin root next to `socket` that holds
/// the official Codex, Claude and Hermes packages, installed through
/// `package.install` with a catalog the registry's throwaway trust anchor
/// authorizes.
async fn spawn_server_serving_official_runtimes(
    socket: &std::path::Path,
    version: &str,
    config: SessionRegistryConfig,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
) {
    spawn_server_serving_official_runtimes_in(
        socket,
        version,
        config,
        support::hermetic_environment_source(),
    )
    .await
}

/// [`spawn_server_serving_official_runtimes`] whose agent children, and the
/// provider homes the daemon resolves for them, start from `environment`.
async fn spawn_server_serving_official_runtimes_in(
    socket: &std::path::Path,
    version: &str,
    config: SessionRegistryConfig,
    environment: EnvironmentSource,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
) {
    serve_official_runtimes(socket, version, config, environment, true).await
}

/// [`spawn_server_serving_official_runtimes`] for a daemon that restarts over
/// the plugin directory of an earlier one: the official packages are already
/// installed there, so none is installed again, and startup reconciles its
/// sessions.
async fn respawn_server_serving_official_runtimes(
    socket: &std::path::Path,
    version: &str,
    config: SessionRegistryConfig,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
) {
    serve_official_runtimes(
        socket,
        version,
        config,
        support::hermetic_environment_source(),
        false,
    )
    .await
}

async fn serve_official_runtimes(
    socket: &std::path::Path,
    version: &str,
    config: SessionRegistryConfig,
    environment: EnvironmentSource,
    install: bool,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
) {
    let dir = socket.parent().expect("test socket has an isolated parent");
    let catalog = official_runtimes::OfficialCatalog::new();
    let config = catalog.configure(config, dir.join("plugins"));
    let event_log_dir = config
        .event_log_dir
        .clone()
        .map(|dir| (dir, config.event_retention));
    let (registry, worker_home_dir) = worker_backed_registry_in(socket, config, environment);
    if install {
        catalog.install(&registry, dir).await;
    } else {
        registry
            .reconcile_workers()
            .await
            .expect("startup reconciles the sessions of the earlier daemon");
    }
    serve_registry(socket, version, event_log_dir, registry, worker_home_dir).await
}

/// Serves `registry` on `socket` with a notification inbox, and the event log
/// at `event_log_dir` when one is configured.
async fn serve_registry(
    socket: &std::path::Path,
    version: &str,
    event_log_dir: Option<(PathBuf, pohunek_service_config::EventRetention)>,
    registry: SessionRegistry,
    worker_home_dir: tempfile::TempDir,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
) {
    let worker_home = worker_home_dir.path().to_path_buf();
    let notifications = NotificationService::open(&notification_data_dir(socket))
        .expect("notification service opens");
    if let Some((event_log_dir, retention)) = event_log_dir {
        let log = Arc::new(EventLog::open(&event_log_dir, retention).expect("event log opens"));
        let _session_log = spawn_drain(
            Arc::clone(&log),
            registry.subscribe(),
            tokio_util::sync::CancellationToken::default(),
        );
        let _notification_log = spawn_drain(
            log,
            notifications.subscribe(),
            tokio_util::sync::CancellationToken::default(),
        );
    }
    let state = DaemonState::new(
        HealthInfo::new(version),
        registry,
        governance_service(socket).await,
        support::overlay_registry(),
        pohunek_daemon::api::OverlaySource::Default,
    )
    .with_notifications(notifications);
    let server = ControlServer::bind_with_state(socket, state)
        .await
        .expect("server binds");
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        // Removed after the server and its registry are gone.
        let _worker_home_dir = worker_home_dir;
        server
            .serve(async move {
                let _ = rx.await;
            })
            .await;
    });
    (tx, handle, worker_home)
}

fn notification_data_dir(socket: &std::path::Path) -> PathBuf {
    socket
        .parent()
        .expect("test socket has a parent")
        .join("notification-data")
}

/// Connect a raw line-framed client to `socket`.
async fn connect(socket: &std::path::Path) -> Framed<UnixStream, LinesCodec> {
    // Bind returns before the listener is necessarily accepting in all timing
    // scenarios.
    wait_until("the test socket to accept a connection", || async {
        UnixStream::connect(socket)
            .await
            .ok()
            .map(|stream| Framed::new(stream, LinesCodec::new()))
    })
    .await
}

/// Waits for a spawned daemon to expose its Unix control listener.
async fn wait_for_daemon_socket(
    child: &mut tokio::process::Child,
    socket: &Path,
) -> Framed<UnixStream, LinesCodec> {
    let child = std::cell::RefCell::new(child);
    wait_until("the daemon to expose its Unix control listener", || async {
        if let Some(status) = child.borrow_mut().try_wait().expect("inspect daemon child") {
            panic!("daemon exited before readiness with status {status}");
        }
        UnixStream::connect(socket)
            .await
            .ok()
            .map(|stream| Framed::new(stream, LinesCodec::new()))
    })
    .await
}

fn assert_mode(path: &Path, expected_mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path).expect("inspect private daemon state");
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        expected_mode,
        "unexpected mode for {}",
        path.display()
    );
}

/// Connect a raw client to `socket` for attach-stream tests.
async fn connect_raw(socket: &std::path::Path) -> UnixStream {
    wait_until("the raw test socket to accept a connection", || async {
        UnixStream::connect(socket).await.ok()
    })
    .await
}

/// Send a request line and read one response line.
async fn exchange(
    framed: &mut Framed<UnixStream, LinesCodec>,
    request: &ProtocolRequest,
) -> Response {
    let line = serde_json::to_string(request).expect("serialize request");
    framed.send(line).await.expect("send");
    let reply = framed
        .next()
        .await
        .expect("a response line")
        .expect("response framing ok");
    serde_json::from_str(&reply).expect("parse response")
}

/// Send one raw control line and read one response line.
async fn exchange_line(framed: &mut Framed<UnixStream, LinesCodec>, line: String) -> Response {
    framed.send(line).await.expect("send raw request");
    let reply = framed
        .next()
        .await
        .expect("a response line")
        .expect("response framing ok");
    serde_json::from_str(&reply).expect("parse response")
}

/// Map a base kind to its wire name (the `agent` field is a free string since
/// Part C; the helpers below still take an `RuntimeRef` for convenience).
fn agent_name(agent: &RuntimeRef) -> &'static str {
    match agent.as_wire() {
        RuntimeId::SHELL => "shell",
        RuntimeId::CODEX => "codex",
        RuntimeId::CLAUDE => "claude",
        RuntimeId::HERMES => "hermes",
        _ => "unknown",
    }
}

fn session_params(socket: &TestSocket) -> SessionNewParams {
    session_params_in(socket.work_dir())
}

fn session_params_in(cwd: PathBuf) -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: "shell".to_owned(),
        cwd: Some(cwd),
        cols: 80,
        rows: 24,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        extended_input_ready_wait: None,
        branch_source: None,
        on_diverged: None,
        input_ack_timeout_ms: None,
        metadata: std::collections::BTreeMap::new(),
    }
}

fn session_params_for_agent(agent: &RuntimeRef, cwd: PathBuf) -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: agent_name(agent).to_owned(),
        cwd: Some(cwd),
        cols: 80,
        rows: 24,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        extended_input_ready_wait: None,
        branch_source: None,
        on_diverged: None,
        input_ack_timeout_ms: None,
        metadata: std::collections::BTreeMap::new(),
    }
}

/// `session.new` params binding a worktree for `repo` + `branch`.
fn session_params_for_worktree(
    agent: &RuntimeRef,
    repo: PathBuf,
    branch: &str,
) -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: agent_name(agent).to_owned(),
        cwd: None,
        cols: 80,
        rows: 24,
        project: None,
        repo: Some(repo),
        branch: Some(branch.to_owned()),
        base_branch: None,
        input: None,
        extended_input_ready_wait: None,
        branch_source: None,
        on_diverged: None,
        input_ack_timeout_ms: None,
        metadata: std::collections::BTreeMap::new(),
    }
}

/// Create a worktree-bound session and return the daemon's response.
async fn create_worktree_session(
    framed: &mut Framed<UnixStream, LinesCodec>,
    agent: RuntimeRef,
    repo: PathBuf,
    branch: &str,
) -> Response {
    let req = Request::make(
        "session-new-worktree",
        method::SESSION_NEW,
        serde_json::to_value(session_params_for_worktree(&agent, repo, branch))
            .expect("serialize params"),
    );
    exchange(framed, &req).await
}

/// Initializes a throwaway git repo on branch `main` with one commit in the
/// working directory of a hermetic environment.
///
/// Git runs with the scrubbed environment, so host `GIT_*` variables and the
/// developer's global configuration cannot reach it.
fn init_git_repo() -> TestEnv {
    let env = TestEnv::new().expect("create the repository environment");
    let git = |args: &[&str]| {
        let out = env
            .command("git")
            .args(args)
            .output()
            .expect("run git in the repository environment");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["-c", "init.defaultBranch=main", "init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(env.cwd().join("README.md"), "init\n").expect("write README");
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    env
}

fn ok_payload(response: Response) -> Value {
    response
        .into_result()
        .unwrap_or_else(|error| panic!("expected ok, got error: {error}"))
}

fn err_payload(response: Response) -> protocol::ProtocolError {
    response.into_result().expect_err("expected error response")
}

async fn create_session(
    framed: &mut Framed<UnixStream, LinesCodec>,
    socket: &TestSocket,
) -> SessionInfo {
    let req = Request::make(
        "session-new",
        method::SESSION_NEW,
        serde_json::to_value(session_params(socket)).expect("serialize params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("session info")
}

async fn create_session_with_params(
    framed: &mut Framed<UnixStream, LinesCodec>,
    params: SessionNewParams,
) -> Response {
    let req = Request::make(
        "session-new-custom",
        method::SESSION_NEW,
        serde_json::to_value(params).expect("serialize params"),
    );
    exchange(framed, &req).await
}

async fn create_session_with_agent(
    framed: &mut Framed<UnixStream, LinesCodec>,
    agent: RuntimeRef,
    cwd: PathBuf,
) -> Response {
    let req = Request::make(
        "session-new-agent",
        method::SESSION_NEW,
        serde_json::to_value(session_params_for_agent(&agent, cwd)).expect("serialize params"),
    );
    exchange(framed, &req).await
}

async fn attach_session(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
) -> SessionAttachResult {
    attach_session_with_dimensions(framed, id, None).await
}

async fn attach_session_with_dimensions(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
    initial_dimensions: Option<TerminalDimensions>,
) -> SessionAttachResult {
    let req = Request::make(
        "session-attach",
        method::SESSION_ATTACH,
        serde_json::to_value(SessionAttachParams {
            session_id: id.clone(),
            initial_dimensions,
            origin_session_id: None,
            origin_daemon_id: None,
            origin_worker_id: None,
        })
        .expect("serialize attach params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("attach result")
}

async fn detach_stream(
    framed: &mut Framed<UnixStream, LinesCodec>,
    stream_id: &str,
) -> SessionDetachResult {
    let req = Request::make(
        "session-detach",
        method::SESSION_DETACH,
        serde_json::to_value(SessionDetachParams {
            stream_id: stream_id.to_owned(),
        })
        .expect("serialize detach params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("detach result")
}

async fn resize_session(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
    cols: u16,
    rows: u16,
) -> SessionResizeResult {
    let req = Request::make(
        "session-resize",
        method::SESSION_RESIZE,
        serde_json::to_value(SessionResizeParams {
            session_id: id.clone(),
            cols,
            rows,
        })
        .expect("serialize resize params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("resize result")
}

async fn input_session(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
    text: &str,
) -> SessionInputResult {
    let req = Request::make(
        "session-input",
        method::SESSION_INPUT,
        serde_json::to_value(SessionInputParams {
            session_id: id.clone(),
            text: text.to_owned(),
            wait: None,
        })
        .expect("serialize input params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("input result")
}

fn notification_params(session_id: Option<SessionId>) -> NotificationCreateParams {
    NotificationCreateParams {
        source: NotificationSource {
            provider: "codex".to_owned(),
            provider_event: "PermissionRequest".to_owned(),
            host_local_source_id: "codex-hook-1".to_owned(),
        },
        kind: NotificationKind::ApprovalRequired,
        severity: NotificationSeverity::ActionRequired,
        title: "Approval required".to_owned(),
        body: "Codex is waiting for owner approval.".to_owned(),
        metadata: BTreeMap::from([
            ("provider".to_owned(), "codex".to_owned()),
            ("provider_event".to_owned(), "PermissionRequest".to_owned()),
        ]),
        session_id,
        agent_kind: None,
        source_id: Some("permission-request-1".to_owned()),
        dedupe_key: Some("session:s-1:attention".to_owned()),
        project_id: None,
        retryable_admission: false,
    }
}

async fn create_notification(
    framed: &mut Framed<UnixStream, LinesCodec>,
    params: NotificationCreateParams,
) -> NotificationCreateResult {
    let req = Request::make(
        "notification-create",
        method::NOTIFICATION_CREATE,
        serde_json::to_value(params).expect("serialize notification params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await))
        .expect("notification.create result")
}

async fn update_notification(
    framed: &mut Framed<UnixStream, LinesCodec>,
    params: NotificationUpdateParams,
) -> NotificationUpdateResult {
    let req = Request::make(
        "notification-update",
        method::NOTIFICATION_UPDATE,
        serde_json::to_value(params).expect("serialize notification update"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await))
        .expect("notification.update result")
}

async fn delete_notification(
    framed: &mut Framed<UnixStream, LinesCodec>,
    params: NotificationDeleteParams,
) -> NotificationDeleteResult {
    let req = Request::make(
        "notification-delete",
        method::NOTIFICATION_DELETE,
        serde_json::to_value(params).expect("serialize notification delete"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await))
        .expect("notification.delete result")
}

async fn list_notifications(
    framed: &mut Framed<UnixStream, LinesCodec>,
    params: NotificationListParams,
) -> NotificationListResult {
    let req = Request::make(
        "notification-list",
        method::NOTIFICATION_LIST,
        serde_json::to_value(params).expect("serialize list params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await))
        .expect("notification.list result")
}

async fn get_notification_policy(
    framed: &mut Framed<UnixStream, LinesCodec>,
) -> NotificationPolicyResult {
    let req = Request::make(
        "notification-policy-get",
        method::NOTIFICATION_POLICY_GET,
        Value::Null,
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await))
        .expect("notification.policy.get result")
}

async fn set_notification_policy(
    framed: &mut Framed<UnixStream, LinesCodec>,
    policy: NotificationPolicy,
) -> NotificationPolicyResult {
    let req = Request::make(
        "notification-policy-set",
        method::NOTIFICATION_POLICY_SET,
        serde_json::to_value(NotificationPolicyParams { policy }).expect("serialize policy params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await))
        .expect("notification.policy.set result")
}

fn all_enabled_notification_policy() -> NotificationPolicy {
    NotificationPolicy {
        attention_dedupe_window_secs: 42,
        attention_debounce_secs: 5,
        enabled: NotificationKindPolicy {
            agent_blocked: true,
            approval_required: true,
            turn_completed: true,
            session_finished: true,
            error: true,
            system: true,
        },
        providers: BTreeMap::new(),
        retention: protocol::NotificationRetentionPolicy::default(),
    }
}

async fn read_file_until(path: &Path, marker: &[u8]) -> Vec<u8> {
    let _report = ReceivedOnPanic(path);
    wait::guard("file marker arrives before timeout", async {
        loop {
            if let Ok(bytes) = tokio::fs::read(path).await {
                if bytes.windows(marker.len()).any(|window| window == marker) {
                    return bytes;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
}

/// Prints what the file at its path holds when the wait for a marker panics,
/// so a missed delivery shows what did arrive.
struct ReceivedOnPanic<'a>(&'a Path);

impl Drop for ReceivedOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            match std::fs::read(self.0) {
                Ok(bytes) => eprintln!(
                    "{} held {} bytes: {:?}",
                    self.0.display(),
                    bytes.len(),
                    String::from_utf8_lossy(&bytes)
                ),
                Err(error) => eprintln!("{} is unreadable: {error}", self.0.display()),
            }
        }
    }
}

async fn open_attach_stream(socket: &std::path::Path, stream_id: &str) -> UnixStream {
    let mut raw = connect_raw(socket).await;
    let header = serde_json::to_string(&AttachHeader {
        attach: stream_id.to_owned(),
    })
    .expect("serialize attach header");
    raw.write_all(header.as_bytes())
        .await
        .expect("send attach header");
    raw.write_all(b"\n").await.expect("terminate attach header");
    raw
}

async fn read_until_marker(stream: &mut UnixStream, marker: &[u8]) -> Vec<u8> {
    wait::guard("marker arrives before timeout", async {
        let mut collected = Vec::new();
        let mut buf = [0_u8; 1024];
        loop {
            let n = stream.read(&mut buf).await.expect("read raw stream");
            assert_ne!(n, 0, "raw stream closed before marker arrived");
            collected.extend_from_slice(&buf[..n]);
            if collected
                .windows(marker.len())
                .any(|window| window == marker)
            {
                return collected;
            }
        }
    })
    .await
}

async fn assert_raw_stream_closes(stream: &mut UnixStream) {
    wait::guard("raw stream closes before timeout", async {
        let mut buf = [0_u8; 256];
        loop {
            let n = stream.read(&mut buf).await.expect("read raw stream");
            if n == 0 {
                return;
            }
        }
    })
    .await;
}

async fn inspect_session(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
) -> SessionInfo {
    let req = Request::make(
        "session-inspect",
        method::SESSION_INSPECT,
        serde_json::to_value(id).expect("serialize id"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("session info")
}

async fn wait_for_state(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
    state: SessionState,
) -> SessionInfo {
    let framed = tokio::sync::Mutex::new(framed);
    wait_until("the session to reach the awaited state", || async {
        let mut framed = framed.lock().await;
        let info = inspect_session(&mut framed, id).await;
        (info.state == state).then_some(info)
    })
    .await
}

/// Subscribes a new connection to the daemon's events.
async fn subscribe_events(socket: &Path) -> Framed<UnixStream, LinesCodec> {
    let mut subscriber = connect(socket).await;
    let request = Request::make("subscribe-events", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &request).await;
    assert!(ack.is_ok(), "subscribe should ack");
    subscriber
}

/// Waits for the attach lifecycle event `expected` of the stream `stream_id`:
/// `attach_opened` once the daemon has bound the raw stream, `attach_closed`
/// once it has deregistered it.
async fn wait_for_attach_event(
    subscriber: &mut Framed<UnixStream, LinesCodec>,
    expected: &str,
    stream_id: &str,
) {
    wait::guard("the attach lifecycle event", async {
        loop {
            let line = subscriber
                .next()
                .await
                .expect("a streamed event line")
                .expect("event framing ok");
            let streamed: Event = serde_json::from_str(&line).expect("parse event");
            if streamed.event() == expected
                && streamed.payload()["stream_id"].as_str() == Some(stream_id)
            {
                return;
            }
        }
    })
    .await;
}

async fn wait_for_agent_state_event(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
    activity: AgentActivity,
    source: StateSource,
) -> Event {
    let expected_activity = serde_json::to_value(activity).expect("serialize activity");
    let expected_source = serde_json::to_value(source).expect("serialize source");
    let deadline = tokio::time::Instant::now() + HANG_GUARD;
    let mut seen = Vec::new();
    loop {
        let now = tokio::time::Instant::now();
        assert!(
            now < deadline,
            "agent_state event did not arrive before timeout; expected activity={expected_activity} source={expected_source}; seen={seen:?}"
        );

        let line = tokio::time::timeout(deadline - now, framed.next())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "agent_state event did not arrive before timeout; expected activity={expected_activity} source={expected_source}; seen={seen:?}"
                )
            })
            .expect("a streamed event line")
            .expect("event framing ok");
        let streamed: Event = serde_json::from_str(&line).expect("parse event");
        if streamed.event() == event::AGENT_STATE
            && streamed.payload()["session_id"].as_str() == Some(id.0.as_str())
        {
            seen.push(streamed.payload().clone());
            if streamed.payload()["activity"] == expected_activity
                && streamed.payload()["source"] == expected_source
            {
                return streamed;
            }
        }
    }
}

async fn wait_for_notification_event(
    framed: &mut Framed<UnixStream, LinesCodec>,
    expected_event: &str,
    id: &protocol::NotificationId,
    expected_status: Option<NotificationStatus>,
) -> Event {
    let expected_status = expected_status
        .map(serde_json::to_value)
        .transpose()
        .expect("serialize status");
    let deadline = tokio::time::Instant::now() + HANG_GUARD;
    let mut seen = Vec::new();
    loop {
        let now = tokio::time::Instant::now();
        assert!(
            now < deadline,
            "notification event did not arrive before timeout; expected event={expected_event} id={}; seen={seen:?}",
            id.0
        );

        let line = tokio::time::timeout(deadline - now, framed.next())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "notification event did not arrive before timeout; expected event={expected_event} id={}; seen={seen:?}",
                    id.0
                )
            })
            .expect("a streamed event line")
            .expect("event framing ok");
        let streamed: Event = serde_json::from_str(&line).expect("parse event");
        if streamed.event() != expected_event {
            continue;
        }
        seen.push(streamed.payload().clone());
        if expected_event == event::NOTIFICATION_DELETED {
            if streamed.payload()["notification_id"].as_str() == Some(id.0.as_str()) {
                return streamed;
            }
            continue;
        }
        let record = &streamed.payload()["record"];
        if record["id"].as_str() != Some(id.0.as_str()) {
            continue;
        }
        if expected_status
            .as_ref()
            .is_some_and(|status| record["status"] != *status)
        {
            continue;
        }
        return streamed;
    }
}

/// Build a stub agent script that logs its argv and then idles.
/// A fake `runtime` agent that answers the official package's version probe,
/// logs its arguments to `argv_log` and idles.
fn process_start_identity(pid: u32) -> ProcessStartIdentity {
    let identity = HostInspector::new()
        .identity(pid)
        .expect("inspect child process identity")
        .expect("child process is live");
    ProcessStartIdentity::new(identity.start_identity.get())
}

async fn wait_for_persisted_resume_and_worktree(
    store: &Store,
    id: &SessionId,
) -> (Vec<ResumeBinding>, Vec<WorktreeBinding>) {
    wait_until("the resume and worktree bindings to persist", || async {
        let resume = store.load_resume().expect("load resume");
        let worktrees = store.load_worktrees().expect("load worktrees");
        let resume_bound = resume
            .iter()
            .any(|binding| binding.session_id == id.0.as_str());
        let worktree_bound = worktrees
            .iter()
            .any(|binding| binding.session_id == id.0.as_str());
        (resume_bound && worktree_bound).then_some((resume, worktrees))
    })
    .await
}

/// Closes the control connection, terminates the real daemon child and reaps
/// it, each step bounded; returns the exit status.
async fn stop_real_daemon(
    mut client: Framed<UnixStream, LinesCodec>,
    child: &mut tokio::process::Child,
) -> std::process::ExitStatus {
    tokio::time::timeout(DAEMON_CONTROL_REQUEST_TIMEOUT, client.get_mut().shutdown())
        .await
        .expect("close daemon health client before control timeout")
        .expect("close daemon health client");

    child.start_kill().expect("terminate real daemon");
    tokio::time::timeout(DAEMON_SHUTDOWN_TIMEOUT, child.wait())
        .await
        .expect("real daemon stops before cleanup timeout")
        .expect("wait for real daemon")
}

/// Opens a file-backed descriptor that lacks close-on-exec, inside `root`.
///
/// The regression is the boundary a real worker keeps: this sentinel exists so
/// the boundary has something to fail on. The test and the real daemon inherit
/// it by construction; without the launcher's isolation the worker would too.
///
/// Returns the file, whose drop closes the descriptor at the end of the test,
/// and the path every executor's descriptor link would name while it is open.
#[cfg(target_os = "linux")]
fn inheritable_fd_sentinel(root: &Path) -> (std::fs::File, PathBuf) {
    use rustix::io::{fcntl_getfd, fcntl_setfd, FdFlags};

    // Above the daemon's lowered soft limit, so the fallback must enumerate existing FDs.
    const SENTINEL_DESCRIPTOR_MIN: i32 = 512;
    let path = root.join("fd-sentinel");
    let original = std::fs::File::create(&path).expect("create the sentinel file");
    let file = std::fs::File::from(
        rustix::io::fcntl_dupfd_cloexec(&original, SENTINEL_DESCRIPTOR_MIN)
            .expect("duplicate sentinel above the daemon soft limit"),
    );
    fcntl_setfd(&file, FdFlags::empty()).expect("clear close-on-exec on the sentinel");
    assert!(
        fcntl_getfd(&file)
            .expect("read the sentinel flags")
            .is_empty(),
        "the sentinel must lack close-on-exec for the regression to be meaningful"
    );
    (file, path)
}

/// Reads the link target of every descriptor the live process `pid` holds.
///
/// Ignores descriptors closed during the scan; other inspection errors fail
/// the scenario instead of claiming the sentinel is absent.
#[cfg(target_os = "linux")]
fn descriptor_link_targets(pid: u32) -> std::io::Result<Vec<PathBuf>> {
    let mut targets = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
        match std::fs::read_link(entry?.path()) {
            Ok(target) => targets.push(target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(targets)
}

#[test]
fn daemon_startup_rejects_missing_config_home_and_home() {
    let env = TestEnv::new().expect("create isolated daemon environment");
    let output = env
        .command(bin_exe("pohunekd"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HOME")
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .output()
        .expect("run real daemon");

    assert!(
        !output.status.success(),
        "daemon must refuse missing path roots"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("XDG_CONFIG_HOME or HOME"),
        "daemon must name the missing configuration root: {stderr}"
    );
}

#[tokio::test]
async fn daemon_startup_creates_private_host_state_from_ordinary_xdg_state_home() {
    daemon_startup_with_descriptor_limit(false).await;
}

#[tokio::test]
async fn daemon_launches_a_real_worker_with_an_unlimited_soft_descriptor_limit() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let env = TestEnv::new().expect("create isolated limit probe environment");
    let mut probe = env.command("/usr/bin/true");
    configure_descriptor_limit(&mut probe, true);
    match probe.status() {
        Ok(status) => assert!(
            status.success(),
            "unlimited descriptor limit probe succeeds"
        ),
        #[cfg(target_os = "linux")]
        Err(error) if matches!(error.raw_os_error(), Some(libc::EPERM | libc::EINVAL)) => {
            eprintln!("unsupported: Linux refuses an unlimited RLIMIT_NOFILE: {error}");
            return;
        }
        Err(error) => panic!("set unlimited descriptor limit in the child: {error}"),
    }
    daemon_startup_with_descriptor_limit(true).await;
}

fn configure_descriptor_limit(command: &mut std::process::Command, unlimited: bool) {
    use std::os::unix::process::CommandExt;

    // Keep ample runtime headroom while retaining the already-open sentinel above this limit.
    const DAEMON_SOFT_DESCRIPTOR_LIMIT: u64 = 256;
    let limit = if unlimited {
        rustix::process::Rlimit {
            current: None,
            maximum: None,
        }
    } else {
        rustix::process::Rlimit {
            current: Some(DAEMON_SOFT_DESCRIPTOR_LIMIT),
            maximum: rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum,
        }
    };
    #[expect(
        unsafe_code,
        reason = "pre_exec changes the child limit without touching the test process"
    )]
    // SAFETY: the closure captures only scalar limits and calls async-signal-safe setrlimit.
    unsafe {
        command.pre_exec(move || {
            rustix::process::setrlimit(rustix::process::Resource::Nofile, limit)
                .map_err(std::io::Error::from)
        });
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "This real-daemon regression keeps bounded startup, worker lifecycle, permission, and cleanup assertions in one auditable sequence."
)]
async fn daemon_startup_with_descriptor_limit(unlimited: bool) {
    use std::os::unix::fs::PermissionsExt;

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut xdg = XdgGuard::set_all("daemon-startup-private-state");
    // The real worker nests its control socket below the XDG runtime root, and
    // `sockaddr_un` imposes a small platform path bound. Keep this fixture short
    // while retaining an isolated complete XDG environment.
    let root_dir =
        pohunek_test_support::tempdir_with_prefix("pw-s-").expect("create short isolated XDG root");
    let root = root_dir.path().to_path_buf();
    // Declared after `root_dir` and before the daemon child: the daemon is
    // killed first, then any worker still below the root is reaped, then the
    // root is removed. The guard reports its survivors while the regression
    // below needs the real worker's identity, and owns the teardown itself.
    let workers = WorkerGuard::watch(&root);
    // On Linux the worker's descriptor table is the regression's boundary. The
    // sentinel file backs a deliberately inheritable descriptor held by this
    // test for the whole run: without the launcher's isolation the real daemon
    // inherits it, and without the sentinel the worker-side assertion below
    // could pass vacuously.
    #[cfg(target_os = "linux")]
    let (sentinel, sentinel_path) = inheritable_fd_sentinel(&root);
    let runtime_home = root.join("r");
    let data_home = root.join("d");
    let state_home = root.join("s");
    let cache_home = root.join("c");
    let config_home = root.join("g");
    let home = root.join("h");
    xdg.env
        .set("XDG_RUNTIME_DIR", &runtime_home)
        .set("XDG_DATA_HOME", &data_home)
        .set("XDG_STATE_HOME", &state_home)
        .set("XDG_CACHE_HOME", &cache_home)
        .set("XDG_CONFIG_HOME", &config_home)
        .set("HOME", &home)
        .set("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .set("CODEX_HOME", home.join(".codex"));
    std::fs::create_dir_all(&home).expect("create isolated home, the worker working directory");
    std::fs::create_dir_all(&state_home).expect("create ordinary XDG state home");
    std::fs::set_permissions(&state_home, std::fs::Permissions::from_mode(0o755))
        .expect("make XDG state home ordinary owner-readable");
    assert_mode(&state_home, 0o755);

    let daemon_env = TestEnv::new().expect("create the daemon child environment");
    let mut command = daemon_env.tokio_command(bin_exe("pohunekd"));
    command
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("HOME", &home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .env("POHUNEK_WORKER_BIN", worker_binary())
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if unlimited || cfg!(target_os = "linux") {
        configure_descriptor_limit(command.as_std_mut(), unlimited);
    }
    let mut child = command.spawn().expect("spawn real daemon");
    let socket = runtime_home.join(APP_DIR).join("daemon.sock");
    let mut client = wait_for_daemon_socket(&mut child, &socket).await;

    // Non-vacuity of the boundary below: the sentinel is still clear and
    // deliberately inherited by both the test process and the real daemon, so
    // only the launcher's isolation can keep it out of the worker.
    #[cfg(target_os = "linux")]
    {
        assert!(
            rustix::io::fcntl_getfd(&sentinel)
                .expect("read the sentinel flags")
                .is_empty(),
            "the sentinel must lack close-on-exec for the regression to be meaningful"
        );
        for (process, pid) in [
            ("the test process", std::process::id()),
            ("the real daemon", child.id().expect("running daemon pid")),
        ] {
            let targets = descriptor_link_targets(pid)
                .unwrap_or_else(|error| panic!("read the descriptor table of {process}: {error}"));
            assert!(
                targets.iter().any(|target| target == &sentinel_path),
                "{process} must hold the deliberately inheritable sentinel descriptor: {targets:?}"
            );
        }
    }
    let response = tokio::time::timeout(
        DAEMON_CONTROL_REQUEST_TIMEOUT,
        exchange(
            &mut client,
            &Request::make("startup-private-state", method::DAEMON_HEALTH, Value::Null),
        ),
    )
    .await
    .expect("daemon.health returns before control timeout");
    assert!(response.is_ok(), "daemon health succeeds after bootstrap");

    let doctor_response = tokio::time::timeout(
        DAEMON_CONTROL_REQUEST_TIMEOUT,
        exchange(
            &mut client,
            &Request::make(
                "startup-private-state-daemon-doctor",
                method::DAEMON_DOCTOR,
                Value::Null,
            ),
        ),
    )
    .await
    .expect("daemon.doctor returns before the control timeout");
    let doctor: protocol::DaemonDoctorResult =
        serde_json::from_value(ok_payload(doctor_response)).expect("typed daemon doctor report");
    let worker_check = doctor
        .report
        .checks
        .iter()
        .find(|check| check.name == "worker_executable")
        .expect("daemon reports worker readiness");
    assert_eq!(worker_check.status, protocol::DoctorStatus::Ok);
    assert!(worker_check.detail.contains("readable journal schema"));
    assert!(
        !worker_check.detail.contains('/'),
        "detail remains path-redacted"
    );
    assert!(
        data_home.join(APP_DIR).join("data.lock").is_file(),
        "the running daemon owns the data authority lock in the XDG data root"
    );

    let session_request = Request::make(
        "startup-private-state-session-new",
        method::SESSION_NEW,
        serde_json::to_value(session_params_in(home.clone()))
            .expect("serialize bounded shell session request"),
    );
    let created: SessionInfo = serde_json::from_value(ok_payload(
        tokio::time::timeout(
            DAEMON_CONTROL_REQUEST_TIMEOUT,
            exchange(&mut client, &session_request),
        )
        .await
        .expect("session.new returns before control timeout"),
    ))
    .expect("deserialize worker-backed session");
    let runtime = created.runtime.as_ref().expect("session reports a runtime");
    assert!(
        runtime.worker_id.is_some(),
        "session uses a real worker process"
    );
    assert!(
        runtime.worker_instance_id.is_some(),
        "session reports a worker runtime"
    );

    let list_request = Request::make(
        "startup-private-state-session-list",
        method::SESSION_LIST,
        serde_json::to_value(SessionListParams::default()).expect("serialize session list request"),
    );
    let sessions: Vec<SessionInfo> = serde_json::from_value(ok_payload(
        tokio::time::timeout(
            DAEMON_CONTROL_REQUEST_TIMEOUT,
            exchange(&mut client, &list_request),
        )
        .await
        .expect("session.list returns before control timeout"),
    ))
    .expect("deserialize worker-backed session list");
    assert!(
        sessions.iter().any(|session| session.id == created.id
            && session
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.worker_id.is_some())),
        "listed session retains its real worker runtime"
    );

    // The boundary of #302: the real worker's post-exec descriptor table holds
    // no link to the sentinel file. The worker's process is not hard-coded; it
    // is the survivor the guard lists while the session still runs, and the
    // guard retains its teardown role below.
    #[cfg(target_os = "linux")]
    {
        let worker_pid = wait_until(
            "the real worker to be listed below the guarded root",
            || {
                let survivor = workers
                    .survivors()
                    .into_iter()
                    .next()
                    .map(|worker| worker.pid);
                std::future::ready(survivor)
            },
        )
        .await;
        let inspector = HostInspector::new();
        let worker_identity = inspector
            .identity(worker_pid)
            .expect("inspect the real worker")
            .expect("the real worker is live before descriptor inspection");
        let worker_targets = descriptor_link_targets(worker_pid)
            .expect("read the real worker's descriptor table while it runs");
        assert!(
            !worker_targets.iter().any(|target| target == &sentinel_path),
            "the real worker inherited a descriptor above 2: {worker_targets:?}"
        );
        assert_eq!(
            inspector
                .identity(worker_pid)
                .expect("reinspect the real worker"),
            Some(worker_identity),
            "the same real worker remains live after descriptor inspection"
        );
    };

    let stop_request = Request::make(
        "startup-private-state-session-stop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize worker-backed session id"),
    );
    let _: SessionStopResult = serde_json::from_value(ok_payload(
        tokio::time::timeout(
            DAEMON_CONTROL_REQUEST_TIMEOUT,
            exchange(&mut client, &stop_request),
        )
        .await
        .expect("session.stop returns before control timeout"),
    ))
    .expect("deserialize worker-backed session stop result");

    // A stopped session keeps its worker until the session is removed; the
    // daemon is killed below and would leave that worker running.
    let remove_request = Request::make(
        "startup-private-state-session-remove",
        method::SESSION_REMOVE,
        serde_json::to_value(&created.id).expect("serialize worker-backed session id"),
    );
    let removed: SessionRemoveResult = serde_json::from_value(ok_payload(
        tokio::time::timeout(
            DAEMON_CONTROL_REQUEST_TIMEOUT,
            exchange(&mut client, &remove_request),
        )
        .await
        .expect("session.remove returns before control timeout"),
    ))
    .expect("deserialize worker-backed session remove result");
    assert!(removed.removed, "the stopped session is removed");

    let status = stop_real_daemon(client, &mut child).await;
    assert!(!status.success(), "test termination stops the real daemon");

    let app_state = state_home.join(APP_DIR);
    let host_state = app_state.join(HOST_STATE_SUBDIR);
    assert_mode(&app_state, 0o700);
    assert_mode(&app_state.join(LOGS_SUBDIR), 0o700);
    assert_mode(&app_state.join(WORKERS_SUBDIR), 0o700);
    assert_mode(&host_state, 0o700);
    for name in [
        HOST_IDENTITY_NAME,
        HOST_APPROVAL_KEY_NAME,
        HOST_GOVERNANCE_NAME,
        HOST_STATE_LOCK_NAME,
    ] {
        assert_mode(&host_state.join(name), 0o600);
    }
    drop(workers);
}

#[tokio::test]
async fn health_returns_versions() {
    let socket = temp_socket("health");
    let (shutdown, handle) = spawn_server(&socket, "9.9.9-test").await;

    let mut client = connect(&socket).await;
    let req = Request::make("t-1", method::DAEMON_HEALTH, Value::Null);
    let resp = exchange(&mut client, &req).await;

    assert_eq!(resp.version(), PROTOCOL_VERSION);
    assert_eq!(resp.id(), "t-1");
    let ok = ok_payload(resp);
    assert_eq!(ok["status"], Value::from("ok"));
    assert_eq!(ok["daemon_version"], Value::from("9.9.9-test"));
    assert_eq!(ok["protocol_version"], Value::from(PROTOCOL_VERSION.get()));

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn public_bind_serves_host_discover_with_supplied_registry() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut origin_env = ProcessEnv::lock();
    origin_env.remove(ENV_DAEMON_ID).remove(ENV_SESSION_ID);
    let socket = temp_socket("host-discover-public-bind");
    let (shutdown, handle) = spawn_server(&socket, "9.9.9-test").await;

    let mut client = connect(&socket).await;
    let req = Request::make(
        "host-discover",
        method::HOST_DISCOVER,
        serde_json::to_value(HostDiscoverParams { force: true }).expect("params serialize"),
    );
    let resp = exchange(&mut client, &req).await;
    let records: Vec<HostRecord> =
        serde_json::from_value(ok_payload(resp)).expect("host records deserialize");
    assert!(records.is_empty());

    let default_req = Request::make(
        "host-discover-default",
        method::HOST_DISCOVER,
        serde_json::json!({}),
    );
    let default_resp = exchange(&mut client, &default_req).await;
    let default_records: Vec<HostRecord> =
        serde_json::from_value(ok_payload(default_resp)).expect("default host records deserialize");
    assert!(default_records.is_empty());

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn host_discover_lists_this_machine_as_one_local_record() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut origin_env = ProcessEnv::lock();
    origin_env.remove(ENV_DAEMON_ID).remove(ENV_SESSION_ID);
    let socket = temp_socket("host-discover-local-record");
    let (shutdown, handle) = spawn_server_with_overlay(
        &socket,
        "9.9.9-test",
        support::overlay_registry_with_own_name("self-box.overlay.test"),
    )
    .await;

    let mut client = connect(&socket).await;
    let req = Request::make(
        "host-discover-local",
        method::HOST_DISCOVER,
        serde_json::to_value(HostDiscoverParams { force: true }).expect("params serialize"),
    );
    let payload = ok_payload(exchange(&mut client, &req).await);
    let records: Vec<HostRecord> =
        serde_json::from_value(payload.clone()).expect("host records deserialize");
    assert_eq!(records.len(), 1, "{payload}");
    let local = &records[0];
    assert!(local.local);
    assert_eq!(local.name.as_deref(), Some("self-box"));
    assert_eq!(local.fqdn.as_deref(), Some("self-box.overlay.test"));
    assert_eq!(local.overlay, "test");
    assert_eq!(local.peer_id, None);
    assert_eq!(local.class, protocol::HostClass::Candidate);
    // The wire shape carries `local` only for this machine.
    assert_eq!(payload[0]["local"], Value::Bool(true));

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn public_socket_rejects_invalid_origin_envelopes_without_leaking_values() {
    let socket = temp_socket("invalid-origin-envelope");
    let (shutdown, handle) = spawn_server(&socket, "test").await;
    let mut client = connect(&socket).await;
    let base = serde_json::to_value(Request::make(
        "origin-envelope",
        method::DAEMON_HEALTH,
        Value::Null,
    ))
    .expect("serialize base request");

    for (field, value) in [
        ("origin_session_id", "s-origin"),
        ("origin_daemon_id", "d-origin"),
    ] {
        let mut invalid = base.clone();
        invalid[field] = Value::from(value);
        let response = exchange_line(
            &mut client,
            serde_json::to_string(&invalid).expect("serialize invalid origin request"),
        )
        .await;
        assert_eq!(response.id(), "invalid-request");
        let error = err_payload(response);
        assert_eq!(error.code, "bad_request");
        assert!(error.msg.contains("origin markers must be absent together"));
        assert!(!error.msg.contains(value));
    }

    let secret = "origin-secret/value";
    let mut unsafe_origin = base;
    unsafe_origin["origin_session_id"] = Value::from("s-origin");
    unsafe_origin["origin_daemon_id"] = Value::from(secret);
    let response = exchange_line(
        &mut client,
        serde_json::to_string(&unsafe_origin).expect("serialize unsafe origin request"),
    )
    .await;
    assert_eq!(response.id(), "invalid-request");
    let error = err_payload(response);
    assert_eq!(error.code, "bad_request");
    assert!(!error.msg.contains(secret));

    let healthy = Request::make("after-invalid-origin", method::DAEMON_HEALTH, Value::Null);
    assert_eq!(
        ok_payload(exchange(&mut client, &healthy).await)["status"],
        "ok"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn host_discover_with_the_whole_window_is_served_on_a_connection_frozen_on_the_current_version(
) {
    let socket = temp_socket("host-discover-window-after-freeze");
    let (shutdown, handle) = spawn_server(&socket, "9.9.9-test").await;
    let mut client = connect(&socket).await;
    let request = |id: &str, method: &str, params: Value, minimum: u32| -> protocol::Request {
        serde_json::from_value(serde_json::json!({
            "v": {"minimum": minimum, "maximum": PROTOCOL_VERSION.get()},
            "id": id,
            "method": method,
            "params": params,
        }))
        .expect("valid request")
    };

    // The connection freezes on the current version with a pinned request.
    let health = request(
        "pinned",
        method::DAEMON_HEALTH,
        Value::Null,
        PROTOCOL_VERSION.get(),
    );
    let ok = ok_payload(exchange(&mut client, &health).await);
    assert_eq!(ok["protocol_version"], Value::from(PROTOCOL_VERSION.get()));

    // A later discovery that advertises the whole window is still served, at the
    // frozen version.
    let discover = request(
        "windowed",
        method::HOST_DISCOVER,
        serde_json::to_value(HostDiscoverParams { force: true }).expect("params"),
        protocol::MIN_PROTOCOL_VERSION.get(),
    );
    let response = exchange(&mut client, &discover).await;
    assert_eq!(response.version(), PROTOCOL_VERSION);
    let records: Vec<HostRecord> =
        serde_json::from_value(ok_payload(response)).expect("host records deserialize");
    assert!(records.is_empty());

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn unknown_method_returns_typed_error() {
    let socket = temp_socket("unknown");
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;

    let mut client = connect(&socket).await;
    let req = Request::make("t-2", "no.such.method", Value::Null);
    let resp = exchange(&mut client, &req).await;

    assert_eq!(resp.id(), "t-2");
    assert_eq!(err_payload(resp).code, "method_not_found");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn host_governance_inspect_requires_null_and_reports_never_enrolled_absence() {
    let socket = temp_socket("host-governance-inspect");
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;
    let mut client = connect(&socket).await;

    let explicit = Request::make(
        "governance-explicit-null",
        method::HOST_GOVERNANCE_INSPECT,
        Value::Null,
    );
    let explicit_payload = ok_payload(exchange(&mut client, &explicit).await);

    let mut missing_wire = serde_json::to_value(Request::make(
        "governance-missing-params",
        method::HOST_GOVERNANCE_INSPECT,
        Value::Null,
    ))
    .expect("serialize missing-params request");
    missing_wire
        .as_object_mut()
        .expect("request is an object")
        .remove("params");
    let missing_payload = ok_payload(
        exchange_line(
            &mut client,
            serde_json::to_string(&missing_wire).expect("encode missing-params request"),
        )
        .await,
    );

    for payload in [&explicit_payload, &missing_payload] {
        assert!(payload["host_id"].is_string());
        assert!(payload["approval_key_reference"].is_string());
        assert_eq!(payload["enrollment"], Value::Null);
        assert_eq!(payload["owner"], Value::Null);
        assert_eq!(payload["owner_revision"], Value::Null);
        assert_eq!(payload["quarantine"], Value::Null);
    }
    assert_eq!(
        explicit_payload, missing_payload,
        "the established omitted-params normalization must match explicit null"
    );

    for (index, params) in [
        serde_json::json!({}),
        serde_json::json!([]),
        serde_json::json!("unexpected"),
        serde_json::json!(1),
        serde_json::json!(true),
    ]
    .into_iter()
    .enumerate()
    {
        let request_id = format!("governance-invalid-{index}");
        let response = exchange(
            &mut client,
            &Request::make(&request_id, method::HOST_GOVERNANCE_INSPECT, params),
        )
        .await;
        assert_eq!(err_payload(response).code, "bad_request");
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn session_wait_with_an_activity_this_daemon_does_not_define_is_refused() {
    // A client that sends an activity from a newer protocol build must get the
    // typed refusal, not a wait that silently never matches.
    let socket = temp_socket("wait-activity");
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;
    let mut client = connect(&socket).await;

    for (index, activity) in ["an_activity_from_a_newer_daemon", "unknown"]
        .into_iter()
        .enumerate()
    {
        let request_id = format!("wait-activity-{index}");
        let response = exchange(
            &mut client,
            &Request::make(
                &request_id,
                method::SESSION_WAIT,
                serde_json::json!({
                    "session_id": "s-1",
                    "activities": [activity],
                    "timeout_ms": 1_000,
                }),
            ),
        )
        .await;
        assert_eq!(response.id(), request_id);
        assert_eq!(err_payload(response).code, "bad_request");
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn session_list_with_malformed_filter_returns_typed_error() {
    // A foreign client (e.g. a shell script or another CLI) can talk JSON to the
    // daemon directly, bypassing clap's value parser. An unknown filter key or an
    // out-of-range value must yield a typed usage error at the daemon boundary,
    // NOT a silently-empty list (Slice A: "typed usage error, not silent empty").
    let socket = temp_socket("bad-filter");
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;
    let mut client = connect(&socket).await;

    // Unknown filter key.
    let unknown_key = Request::make(
        "bad-filter-key",
        method::SESSION_LIST,
        serde_json::json!({ "filters": [{ "key": "cwd", "value": "/workspace" }] }),
    );
    let response = exchange(&mut client, &unknown_key).await;
    assert_eq!(response.id(), "bad-filter-key");
    assert_eq!(err_payload(response).code, "bad_request");

    // Known key, value outside the closed state enum.
    let bad_value = Request::make(
        "bad-filter-value",
        method::SESSION_LIST,
        serde_json::json!({ "filters": [{ "key": "state", "value": "paused" }] }),
    );
    let response = exchange(&mut client, &bad_value).await;
    assert_eq!(response.id(), "bad-filter-value");
    assert_eq!(err_payload(response).code, "bad_request");

    let oversized_key = Request::make(
        "bad-filter-metadata-key",
        method::SESSION_LIST,
        serde_json::json!({
            "filters": [{ "key": "metadata_has", "value": { "key": "x".repeat(65) } }]
        }),
    );
    assert_eq!(
        err_payload(exchange(&mut client, &oversized_key).await).code,
        "bad_request"
    );

    let oversized_value = Request::make(
        "bad-filter-metadata-value",
        method::SESSION_LIST,
        serde_json::json!({
            "filters": [{
                "key": "metadata_eq",
                "value": { "key": "tag", "value": "x".repeat(4097) }
            }]
        }),
    );
    assert_eq!(
        err_payload(exchange(&mut client, &oversized_value).await).code,
        "bad_request"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn session_list_filters_metadata_at_the_socket_boundary() {
    let socket = temp_socket("session-list-metadata");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut client = connect(&socket).await;

    let mut tagged_params = session_params(&socket);
    tagged_params.metadata = BTreeMap::from([
        ("ticket".to_owned(), "ABC-1".to_owned()),
        ("team".to_owned(), "runtime".to_owned()),
    ]);
    let tagged: SessionInfo = serde_json::from_value(ok_payload(
        create_session_with_params(&mut client, tagged_params).await,
    ))
    .expect("tagged session");
    let untagged = create_session(&mut client, &socket).await;

    let cases = [
        (
            "metadata-exact",
            vec![SessionListFilter::MetadataEq {
                key: "ticket".to_owned(),
                value: "ABC-1".to_owned(),
            }],
            vec![tagged.id.clone()],
        ),
        (
            "metadata-presence",
            vec![SessionListFilter::MetadataHas {
                key: "team".to_owned(),
            }],
            vec![tagged.id.clone()],
        ),
        (
            "metadata-and",
            vec![
                SessionListFilter::MetadataHas {
                    key: "ticket".to_owned(),
                },
                SessionListFilter::MetadataEq {
                    key: "team".to_owned(),
                    value: "runtime".to_owned(),
                },
                SessionListFilter::Id(tagged.id.0.clone()),
            ],
            vec![tagged.id.clone()],
        ),
        (
            "metadata-no-match",
            vec![SessionListFilter::MetadataEq {
                key: "ticket".to_owned(),
                value: "ABC-2".to_owned(),
            }],
            Vec::new(),
        ),
    ];
    for (id, filters, expected) in cases {
        let request = Request::make(
            id,
            method::SESSION_LIST,
            serde_json::to_value(SessionListParams { filters }).expect("serialize filters"),
        );
        let sessions: Vec<SessionInfo> =
            serde_json::from_value(ok_payload(exchange(&mut client, &request).await))
                .expect("filtered list");
        assert_eq!(
            sessions
                .iter()
                .map(|session| &session.id)
                .collect::<Vec<_>>(),
            expected.iter().collect::<Vec<_>>(),
            "filter scenario {id}"
        );
    }
    assert_ne!(tagged.id, untagged.id);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn attach_reporting_its_own_session_as_origin_is_rejected_over_the_socket() {
    // End-to-end: the CLI sets `origin_session_id`/`origin_worker_id` from the
    // PTY env when it runs inside a session. An attach whose origin matches the
    // target session AND its durable worker would loop the PTY's output into its
    // own input, so the daemon must reject it at the wire boundary with a typed,
    // stable error — proving the handler threads the origin through to the guard
    // (not just the unit path). `origin_worker_id` (not `origin_daemon_id`) is
    // authoritative for worker-backed sessions (see
    // `SessionAttachParams::origin_worker_id`), so the test reads the created
    // session's own worker id back off the wire to build the self-feeding request.
    let socket = temp_socket("attach-self-feedback");
    let (shutdown, handle, _) =
        spawn_server_with_config(&socket, "0.0.0", support::hermetic_registry_config()).await;
    let mut control = connect(&socket).await;

    let created = create_session(&mut control, &socket).await;
    let worker_id = created
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.worker_id.clone())
        .expect("worker-backed session reports a worker id");

    let self_attach = Request::make(
        "attach-self",
        method::SESSION_ATTACH,
        serde_json::json!({
            "session_id": created.id,
            "origin_session_id": created.id,
            "origin_worker_id": worker_id,
        }),
    );
    let response = exchange(&mut control, &self_attach).await;
    assert_eq!(response.id(), "attach-self");
    let error = err_payload(response);
    assert_eq!(error.code, "attach_self_feedback");
    assert!(
        error.recover.is_some(),
        "self-feedback error must carry a recovery hint: {error:?}"
    );

    // Same session id reported from a DIFFERENT worker (a colliding id or a stale
    // env from a prior process): no loop, so it must be accepted.
    let other_worker = Request::make(
        "attach-other-worker",
        method::SESSION_ATTACH,
        serde_json::json!({
            "session_id": created.id,
            "origin_session_id": created.id,
            "origin_worker_id": "some-other-worker",
        }),
    );
    let ok = ok_payload(exchange(&mut control, &other_worker).await);
    assert!(
        ok.get("stream_id").and_then(Value::as_str).is_some(),
        "a matching session id on a different worker must still attach: {ok:?}"
    );

    // An attach from a different terminal (no origin reported) still works.
    let plain_attach = Request::make(
        "attach-plain",
        method::SESSION_ATTACH,
        serde_json::json!({ "session_id": created.id }),
    );
    let ok = ok_payload(exchange(&mut control, &plain_attach).await);
    assert!(
        ok.get("stream_id").and_then(Value::as_str).is_some(),
        "a plain attach must still mint a stream id: {ok:?}"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Programs a worker-backed session needs on the daemon's `PATH`; no agent
/// binary is among them.
const AGENTLESS_PATH_TOOLS: [&str; 1] = ["sh"];

/// Owner-only name of the directory holding the child daemon's `PATH`.
const AGENTLESS_PATH_DIR: &str = "bin";

/// Creates an owner-only directory under `env`'s private root that holds links
/// to [`AGENTLESS_PATH_TOOLS`] only, resolved from the test's own `PATH`.
///
/// The daemon rejects group- or world-writable `PATH` components, hence the
/// explicit mode; the directory is removed with `env`.
fn agentless_path_dir(env: &TestEnv) -> PathBuf {
    use std::os::unix::fs::{symlink, DirBuilderExt as _};

    let dir = env.root().join(AGENTLESS_PATH_DIR);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .expect("create the agent-free PATH directory");
    let host_path = std::env::var_os("PATH").expect("the test process has a PATH");
    for tool in AGENTLESS_PATH_TOOLS {
        let real = std::env::split_paths(&host_path)
            .map(|candidate| candidate.join(tool))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| panic!("`{tool}` is not on the test PATH"));
        symlink(&real, dir.join(tool)).expect("link a tool into the agent-free PATH directory");
    }
    dir
}

#[tokio::test]
async fn session_new_for_missing_agent_binary_returns_typed_error() {
    assert_session_new_reports_missing_binary(RuntimeRef::claude(), "claude").await;
}

/// A runtime that declares a version probe reports a missing executable with
/// the same typed error as one that does not: the probe only judges an
/// executable that exists.
#[tokio::test]
async fn session_new_for_missing_probed_agent_binary_returns_typed_error() {
    assert_session_new_reports_missing_binary(RuntimeRef::hermes(), "hermes").await;
}

/// Starts a real daemon whose `PATH` holds no agent, asks it for an `agent`
/// session and asserts the typed, recoverable `agent_binary_missing` refusal
/// naming `binary`, with no session left behind.
///
/// The daemon serves `codex` and `claude` from the official packages, whose
/// descriptors declare a version probe.
async fn assert_session_new_reports_missing_binary(agent: RuntimeRef, binary: &str) {
    // The daemon runs as a child with a `PATH` of its own, so no agent binary
    // is resolvable whatever the host has installed and no process-wide
    // variable changes.
    let env = TestEnv::new().expect("create the daemon child environment");
    official_runtimes::OfficialCatalog::new()
        .preinstall(
            &env.state_home().join(APP_DIR).join(PLUGINS_SUBDIR),
            env.root(),
        )
        .await;
    let path_dir = agentless_path_dir(&env);
    let mut child = env
        .tokio_command(bin_exe("pohunekd"))
        .env("PATH", &path_dir)
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .env("POHUNEK_WORKER_BIN", worker_binary())
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn real daemon");
    let socket = env.runtime_dir().join(APP_DIR).join("daemon.sock");
    let mut client = wait_for_daemon_socket(&mut child, &socket).await;

    // `session.new` must fail with the typed, recoverable
    // `agent_binary_missing` error (stable code a script can branch on)
    // rather than a generic spawn failure or a version-probe verdict.
    let resp = tokio::time::timeout(
        DAEMON_CONTROL_REQUEST_TIMEOUT,
        create_session_with_agent(&mut client, agent.clone(), env.cwd().to_path_buf()),
    )
    .await
    .expect("session.new returns before control timeout");

    let err = err_payload(resp);
    assert_eq!(err.class, ErrorClass::Runtime, "{agent}: {err:?}");
    assert_eq!(err.code, "agent_binary_missing", "{agent}: {err:?}");
    assert!(
        err.msg.contains(binary),
        "error must name the missing binary: {err:?}"
    );
    assert!(
        err.recover.is_some(),
        "missing-binary error must carry a recover hint: {err:?}"
    );

    let list = Request::make(
        "missing-binary-session-list",
        method::SESSION_LIST,
        serde_json::to_value(SessionListParams::default()).expect("serialize session list request"),
    );
    let sessions: Vec<SessionInfo> = serde_json::from_value(ok_payload(
        tokio::time::timeout(DAEMON_CONTROL_REQUEST_TIMEOUT, exchange(&mut client, &list))
            .await
            .expect("session.list returns before control timeout"),
    ))
    .expect("session list");
    assert!(
        sessions.is_empty(),
        "{agent}: no session is created: {sessions:?}"
    );

    // The child is reaped before `env` drops and removes the PATH directory.
    stop_real_daemon(client, &mut child).await;
}

/// Block until `socket` genuinely refuses connections, bounded by a timeout.
///
/// The daemon's own stale-socket recovery (`recover_stale_socket`) decides
/// "stale vs. live" by trying to connect: a refused connection means no
/// listener is behind the path. Dropping a `std::os::unix::net::UnixListener`
/// closes its file descriptor, but under heavy parallel load the kernel can
/// take a small, nonzero amount of time to fully tear down the listening
/// socket's backlog; a `connect()` racing that teardown can transiently
/// succeed, which the daemon then (correctly, if misleadingly) reports as "a
/// live daemon is already listening". Waiting here for a genuine refusal
/// before invoking the daemon's own recovery path removes that race without
/// touching the daemon's connect-based liveness check itself.
async fn wait_until_connect_refused(socket: &Path) {
    wait::guard(
        "dropped listener never stopped accepting connections",
        async {
            loop {
                if UnixStream::connect(socket).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        },
    )
    .await;
}

#[tokio::test]
async fn stale_socket_is_recovered_on_bind() {
    let socket = temp_socket("stale");
    // Create a stale socket file with no listener behind it.
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind stale");
    std::fs::set_permissions(
        &socket,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )
    .expect("make stale socket owner-private");
    drop(listener);
    assert!(socket.exists(), "stale socket file should exist");
    // See `wait_until_connect_refused`: give the kernel a moment to fully tear
    // down the just-dropped listener before asserting on stale-socket recovery,
    // so this test exercises genuine staleness rather than racing the teardown.
    wait_until_connect_refused(&socket).await;

    // Binding again must succeed by removing the stale socket.
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;

    let mut client = connect(&socket).await;
    let req = Request::make("t-3", method::DAEMON_HEALTH, Value::Null);
    let resp = exchange(&mut client, &req).await;
    assert!(resp.is_ok(), "health works after recovery");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn hostile_socket_entry_is_rejected_without_mutation() {
    let socket = temp_socket("hostile-socket-entry");
    std::fs::write(&socket, b"owner data").expect("create hostile regular file");
    std::fs::set_permissions(
        &socket,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )
    .expect("make hostile file private");

    let error = ControlServer::bind(
        &socket,
        HealthInfo::new("0.0.0"),
        governance_service(&socket).await,
        support::overlay_registry(),
    )
    .await
    .expect_err("regular file must not be treated as a stale socket");

    assert!(error.to_string().contains("expected Socket"));
    assert_eq!(
        std::fs::read(&socket).expect("hostile entry remains"),
        b"owner data"
    );
}

#[tokio::test]
async fn wrong_mode_stale_socket_is_rejected_without_removal() {
    let socket = temp_socket("wrong-mode-stale-socket");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind stale socket");
    std::fs::set_permissions(
        &socket,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o660),
    )
    .expect("widen stale socket mode");
    drop(listener);
    wait_until_connect_refused(&socket).await;

    ControlServer::bind(
        &socket,
        HealthInfo::new("0.0.0"),
        governance_service(&socket).await,
        support::overlay_registry(),
    )
    .await
    .expect_err("wrong-mode stale socket must fail closed");

    assert!(socket.exists(), "unsafe stale socket must remain untouched");
}

#[tokio::test]
async fn session_lifecycle_over_socket() {
    let socket = temp_socket("session-lifecycle");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut client = connect(&socket).await;
    let created = create_session(&mut client, &socket).await;
    assert_eq!(created.agent, "shell");
    assert_eq!(created.state, SessionState::Running);
    assert_eq!(created.cols, 80);
    assert_eq!(created.rows, 24);
    assert!(created.pid > 0);

    let list_req = Request::make("session-list", method::SESSION_LIST, Value::Null);
    let list: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut client, &list_req).await))
            .expect("session list");
    assert!(
        list.iter()
            .any(|session| session.id == created.id && session.state == SessionState::Running),
        "created session should appear in list: {list:?}"
    );

    let second = create_session(&mut client, &socket).await;
    let filtered_list_req = Request::make(
        "session-list-filtered",
        method::SESSION_LIST,
        serde_json::to_value(SessionListParams {
            filters: vec![
                SessionListFilter::State(SessionState::Running),
                SessionListFilter::Id(created.id.0.clone()),
            ],
        })
        .expect("serialize list params"),
    );
    let filtered: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut client, &filtered_list_req).await))
            .expect("filtered session list");
    assert_eq!(
        filtered
            .iter()
            .map(|session| &session.id)
            .collect::<Vec<_>>(),
        vec![&created.id],
        "local filtered list must return only the exact AND match, not {second:?}: {filtered:?}"
    );

    let inspected = inspect_session(&mut client, &created.id).await;
    assert_eq!(inspected.id, created.id);
    assert_eq!(inspected.cwd, created.cwd);
    assert_eq!(inspected.pid, created.pid);

    let stop_req = Request::make(
        "session-stop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let stopped: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut client, &stop_req).await))
            .expect("stop result");
    assert!(stopped.stopped);

    let stopped_info = inspect_session(&mut client, &created.id).await;
    assert_eq!(stopped_info.state, SessionState::Stopped);

    let list_after_stop_req =
        Request::make("session-list-after-stop", method::SESSION_LIST, Value::Null);
    let list_after_stop: Vec<SessionInfo> = serde_json::from_value(ok_payload(
        exchange(&mut client, &list_after_stop_req).await,
    ))
    .expect("session list after stop");
    assert!(
        list_after_stop
            .iter()
            .any(|session| session.id == created.id && session.state == SessionState::Stopped),
        "stopped session should be reflected in list: {list_after_stop:?}"
    );
    let stop_second_req = Request::make(
        "session-stop-second",
        method::SESSION_STOP,
        serde_json::to_value(&second.id).expect("serialize second id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut client, &stop_second_req).await))
            .expect("stop second result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn session_input_writes_text_to_shell_pty() {
    let socket = temp_socket("session-input-shell");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            [
                "-c",
                "IFS= read -r line; printf 'got:%s\\n' \"$line\"; sleep 30",
            ],
        ),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut client = connect(&socket).await;
    let created = create_session(&mut client, &socket).await;
    let input = input_session(&mut client, &created.id, "hello from control").await;
    assert!(input.accepted);

    let attach = attach_session(&mut client, &created.id).await;
    let mut raw = open_attach_stream(&socket, &attach.stream_id).await;
    let output = read_until_marker(&mut raw, b"got:hello from control").await;
    assert!(
        output
            .windows(b"got:hello from control".len())
            .any(|window| window == b"got:hello from control"),
        "input output should be visible in the attach snapshot: {output:?}"
    );

    let _ = detach_stream(&mut client, &attach.stream_id).await;
    let stop_req = Request::make(
        "session-input-stop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut client, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Bytes pasted into the attach stream: far more than the terminal's input
/// queue and the socket buffers between the client and the worker hold.
const LARGE_PASTE_BYTES: usize = 1024 * 1024;

/// Bytes per pasted line, newline included, so a canonical-mode reader
/// receives the paste line by line.
const PASTE_LINE_BYTES: usize = 64;

/// Bytes the child prints while the paste is pending: more than the socket
/// buffers between the worker and the client hold.
const PENDING_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

/// Line the child prints after its output, while the paste is still pending.
const PENDING_OUTPUT_MARKER: &[u8] = b"pending-output-done";

/// Line the child prints once it has read the whole paste.
const PASTE_DRAINED_MARKER: &[u8] = b"paste-drained";

/// Reads `stream` until `marker` has arrived, keeping only the bytes a
/// marker split across reads needs.
async fn read_past_marker<R>(stream: &mut R, marker: &[u8], what: &str)
where
    R: tokio::io::AsyncRead + Unpin,
{
    wait::guard(what, async {
        let mut tail = Vec::new();
        let mut buf = vec![0_u8; 64 * 1024];
        loop {
            let n = stream.read(&mut buf).await.expect("read raw stream");
            assert_ne!(n, 0, "raw stream closed before {what}");
            tail.extend_from_slice(&buf[..n]);
            if tail.windows(marker.len()).any(|window| window == marker) {
                return;
            }
            let keep = tail.len().min(marker.len() - 1);
            tail.drain(..tail.len() - keep);
        }
    })
    .await;
}

#[tokio::test]
async fn an_attached_client_keeps_receiving_output_while_its_large_paste_waits_for_the_child() {
    let socket = temp_socket("attach-large-paste");
    let gates = temp_dir("attach-large-paste-gates");
    let output_gate = gates.join("output");
    let drain_gate = gates.join("drain");
    let script = format!(
        "while [ ! -e '{output_gate}' ]; do sleep 0.05; done; \
         head -c {PENDING_OUTPUT_BYTES} /dev/zero | tr '\\0' y; \
         printf '\\n%s\\n' '{pending}'; \
         while [ ! -e '{drain_gate}' ]; do sleep 0.05; done; \
         head -c {LARGE_PASTE_BYTES} >/dev/null; \
         printf '%s\\n' '{drained}'; sleep 30",
        output_gate = output_gate.display(),
        drain_gate = drain_gate.display(),
        pending = std::str::from_utf8(PENDING_OUTPUT_MARKER).expect("ASCII marker"),
        drained = std::str::from_utf8(PASTE_DRAINED_MARKER).expect("ASCII marker"),
    );
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut client = connect(&socket).await;
    let created = create_session(&mut client, &socket).await;
    let attach = attach_session(&mut client, &created.id).await;
    let raw = open_attach_stream(&socket, &attach.stream_id).await;
    let (mut raw_read, mut raw_write) = tokio::io::split(raw);

    // The paste is written from its own task, as a terminal sends it while it
    // keeps drawing; it completes only once the child reads it.
    let mut line = vec![b'x'; PASTE_LINE_BYTES - 1];
    line.push(b'\n');
    let paste = line.repeat(LARGE_PASTE_BYTES / PASTE_LINE_BYTES);
    let pasting = tokio::spawn(async move {
        raw_write.write_all(&paste).await.expect("write the paste");
        raw_write
    });

    // The child does not read yet, so the paste stays pending in the worker
    // while the child prints more than every buffer on the way holds.
    std::fs::File::create(&output_gate).expect("open the output gate");
    read_past_marker(
        &mut raw_read,
        PENDING_OUTPUT_MARKER,
        "the child's output arrives while the paste is pending",
    )
    .await;
    assert!(!pasting.is_finished(), "the paste is still pending");

    std::fs::File::create(&drain_gate).expect("open the drain gate");
    read_past_marker(
        &mut raw_read,
        PASTE_DRAINED_MARKER,
        "the child reads the whole paste",
    )
    .await;
    let _raw_write = wait::guard("the paste completes", pasting)
        .await
        .expect("paste task");

    let _ = detach_stream(&mut client, &attach.stream_id).await;
    let stop_req = Request::make(
        "attach-large-paste-stop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut client, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn session_new_with_input_writes_text_to_shell_pty() {
    let socket = temp_socket("session-new-input-shell");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            [
                "-c",
                "IFS= read -r line; printf 'got:%s\\n' \"$line\"; sleep 30",
            ],
        ),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut client = connect(&socket).await;
    let mut params = session_params(&socket);
    params.input = Some("hello from create".to_owned());
    let ok = ok_payload(create_session_with_params(&mut client, params).await);
    assert!(
        !ok.as_object()
            .expect("session.new response object")
            .contains_key("accepted"),
        "session.new must keep returning SessionInfo, not SessionInputResult: {ok}"
    );
    let created: SessionInfo = serde_json::from_value(ok).expect("session info");
    assert_eq!(created.agent, "shell");
    assert_eq!(created.state, SessionState::Running);

    let attach = attach_session(&mut client, &created.id).await;
    let mut raw = open_attach_stream(&socket, &attach.stream_id).await;
    let output = read_until_marker(&mut raw, b"got:hello from create").await;
    assert!(
        output
            .windows(b"got:hello from create".len())
            .any(|window| window == b"got:hello from create"),
        "create-time input output should be visible in the attach snapshot: {output:?}"
    );

    let _ = detach_stream(&mut client, &attach.stream_id).await;
    let stop_req = Request::make(
        "session-new-input-stop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut client, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Startup grace for the silent-agent scenarios. Short, so the stall path is
/// reached at once; the agent never publishes readiness, so the grace always
/// runs to its end.
const SILENT_AGENT_STARTUP_GRACE: Duration = Duration::from_millis(50);

/// Stall bound for the silent-agent scenarios. It stays below the client's
/// five-second request timeout, so a fire-and-forget caller receives the typed
/// error; the agent never reads, so any nonzero bound stalls.
const SILENT_AGENT_STALL_TIMEOUT: Duration = Duration::from_millis(300);

/// Upper bound on the daemon shutdown after a stall. The shutdown must not
/// wait for the stalled write, so this guard fails the test rather than hang.
const STALL_SCENARIO_SHUTDOWN_GUARD: Duration = Duration::from_secs(30);

/// Lines in the oversized input, about 134 KB in all. A PTY whose program
/// does not read holds only a few KiB: on Linux the tty flip buffer limit
/// (8 KiB, `tty_buffer_set_limit` in `drivers/tty/pty.c`) plus the 4 KiB
/// line-discipline queue (`N_TTY_BUF_SIZE`), and an input that fits is
/// accepted at once. The input is far larger, so the write always stalls.
const OVERSIZED_INPUT_LINES: usize = 1_700;

/// Bytes per oversized input line.
const OVERSIZED_INPUT_LINE_BYTES: usize = 78;

/// Bound the oversized input must exceed: 96 KiB, above what a Linux PTY holds
/// for a program that does not read, even on a kernel with 64 KiB pages
/// (`TTY_BUFFER_PAGE` about 32 KiB), which would hold at most about 84 KiB
/// with the twofold buffer slack. The 134 KB input stays larger.
const PTY_UNREAD_INPUT_CAPACITY: usize = 96 * 1024;

const _: () = assert!(
    OVERSIZED_INPUT_LINES * (OVERSIZED_INPUT_LINE_BYTES + 1) > PTY_UNREAD_INPUT_CAPACITY
        && OVERSIZED_INPUT_LINES * (OVERSIZED_INPUT_LINE_BYTES + 1)
            <= protocol::MAX_SESSION_INPUT_BYTES,
    "the oversized input must exceed what a PTY holds and fit one session input"
);

/// A child that runs `sleep` and never reads its terminal.
fn silent_agent_config() -> SessionRegistryConfig {
    SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exec sleep 3600"]),
        initial_input_startup_grace: SILENT_AGENT_STARTUP_GRACE,
        input_write_stall_timeout: SILENT_AGENT_STALL_TIMEOUT,
        ..SessionRegistryConfig::default()
    }
}

fn oversized_input() -> String {
    vec!["x".repeat(OVERSIZED_INPUT_LINE_BYTES); OVERSIZED_INPUT_LINES].join("\n")
}

#[tokio::test]
async fn session_new_rolls_back_when_the_agent_never_reads_its_initial_input() {
    assert_stalled_create_rolls_back("session-new-input-stalled", None).await;
}

#[tokio::test]
async fn session_new_with_an_ack_window_rolls_back_when_the_agent_never_reads_its_input() {
    assert_stalled_create_rolls_back("session-new-ack-stalled", Some(1_000)).await;
}

async fn assert_stalled_create_rolls_back(tag: &str, input_ack_timeout_ms: Option<u32>) {
    let socket = temp_socket(tag);
    let (shutdown, handle, _) =
        spawn_server_with_config(&socket, "0.0.0", silent_agent_config()).await;

    let mut client = connect(&socket).await;
    let mut params = session_params(&socket);
    params.input = Some(oversized_input());
    params.input_ack_timeout_ms = input_ack_timeout_ms;
    let error = err_payload(create_session_with_params(&mut client, params).await);
    assert_eq!(error.code, "session_input_stalled");
    assert_eq!(error.class, ErrorClass::Runtime);

    let list_request = Request::make(
        "session-new-stalled-list",
        method::SESSION_LIST,
        serde_json::to_value(SessionListParams::default()).expect("serialize session list request"),
    );
    let sessions: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut client, &list_request).await))
            .expect("session list");
    assert!(
        sessions.is_empty(),
        "a create whose initial input stalled must be rolled back: {sessions:?}"
    );

    let _ = shutdown.send(());
    tokio::time::timeout(STALL_SCENARIO_SHUTDOWN_GUARD, handle)
        .await
        .expect("daemon shutdown completes after the rolled-back create")
        .expect("server task joins");
}

#[tokio::test]
async fn session_input_to_an_agent_that_never_reads_fails_with_a_typed_stall() {
    let socket = temp_socket("session-input-stalled");
    let (shutdown, handle, _) =
        spawn_server_with_config(&socket, "0.0.0", silent_agent_config()).await;

    let mut client = connect(&socket).await;
    let created = create_session(&mut client, &socket).await;
    let input_request = Request::make(
        "session-input-stalled",
        method::SESSION_INPUT,
        serde_json::to_value(SessionInputParams {
            session_id: created.id.clone(),
            text: oversized_input(),
            wait: None,
        })
        .expect("serialize input params"),
    );
    let error = err_payload(exchange(&mut client, &input_request).await);
    assert_eq!(error.code, "session_input_stalled");
    assert_eq!(error.class, ErrorClass::Runtime);

    // The stalled write must leave the control connection usable, so the
    // stop of the same session still succeeds over it.
    let stop_req = Request::make(
        "session-input-stalled-stop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut client, &stop_req).await))
            .expect("stop result after a stalled input");

    let _ = shutdown.send(());
    tokio::time::timeout(STALL_SCENARIO_SHUTDOWN_GUARD, handle)
        .await
        .expect("daemon shutdown completes after a stalled input")
        .expect("server task joins");
}

/// Launcher for a fake agent that runs `script` under Python without a version
/// probe. The profiles using it are based on `shell`, whose readiness is the
/// bracketed-paste rule these tests exercise; a Claude base would require the
/// prompt box, which the fake does not render.
fn python_launcher(script: &Path) -> String {
    format!(
        "#!/bin/sh\nexec /usr/bin/python3 '{}' \"$@\"\n",
        script.display()
    )
}

/// Covers delayed reader startup while remaining below the configured
/// five-second fallback.
const INITIAL_READER_OBSERVATION_WINDOW: Duration = Duration::from_secs(3);

#[expect(
    clippy::too_many_lines,
    reason = "the real worker setup, public observation, and cleanup share one lifecycle fixture"
)]
async fn initial_input_waits_for_reader(warning_first: bool, stale_mode_first: bool) {
    let socket = temp_socket(if warning_first {
        "initial-input-warning"
    } else {
        "initial-input-silent"
    });
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let reader_socket = socket.dir.join("reader.sock");
    let reader = UnixListener::bind(&reader_socket).expect("bind fake reader gate");
    let received = socket.dir.join("received.bin");
    let script = agents_dir.join("reader.py");
    let fake = agents_dir.join("reader");
    let fake_source = format!(
        r#"import os
import select
import socket
import termios
import tty

gate = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
gate.connect({reader_socket:?})
tty.setraw(0)
if {warning_first}:
    os.write(1, b"Startup warning: checking workspace\r\n")
if {stale_mode_first}:
    os.write(1, b"\x1b[?1;2004h\x1b[?1;2004l")
gate.sendall(b"s")
while True:
    ready, _, _ = select.select([gate, 0], [], [])
    if gate in ready and gate.recv(1):
        break
    if 0 in ready:
        os.read(0, 4096)
        gate.sendall(b"e")
termios.tcflush(0, termios.TCIFLUSH)
if {stale_mode_first}:
    os.write(1, b"\x1b[?1;20")
    gate.sendall(b"p")
    gate.recv(1)
    os.write(1, b"04h\xe2\x9d\xaf ")
else:
    os.write(1, b"\x1b[?1;2004h\xe2\x9d\xaf ")
body = bytearray()
while len(body) < len(b"hello reader"):
    body.extend(os.read(0, len(b"hello reader") - len(body)))
submit = os.read(0, 1)
with open({received:?}, "wb") as capture:
    capture.write(body + submit)
gate.sendall(b"d")
gate.recv(1)
ready, _, _ = select.select([0], [], [], 0)
if ready:
    with open({received:?}, "ab") as capture:
        capture.write(os.read(0, 4096))
gate.sendall(b"f")
while True:
    os.read(0, 1)
"#,
        reader_socket = reader_socket.display().to_string(),
        received = received.display().to_string(),
        warning_first = if warning_first { "True" } else { "False" },
        stale_mode_first = if stale_mode_first { "True" } else { "False" },
    );
    std::fs::write(&script, fake_source).expect("write reader script");
    write_executable(&fake, &python_launcher(&script));
    std::fs::write(
        agents_dir.join("readershell.toml"),
        format!(
            "base = \"shell\"\nprogram = {}\n",
            toml::Value::String(fake.display().to_string())
        ),
    )
    .expect("write reader profile");
    let config_dir = socket.dir.join("config");
    std::fs::create_dir_all(config_dir.join("hooks")).expect("create session hook directory");
    let hook_started = socket.dir.join("hook-started");
    write_executable(
        &config_dir.join("hooks/session-start"),
        &format!("#!/bin/sh\nprintf ready > '{}'\n", hook_started.display()),
    );
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        config_dir: Some(config_dir),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;

    let mut client = connect(&socket).await;
    let mut params = session_params(&socket);
    "readershell".clone_into(&mut params.agent);
    params.input = Some("hello reader".to_owned());
    params.extended_input_ready_wait = Some(true);
    let mut create =
        tokio::spawn(async move { create_session_with_params(&mut client, params).await });
    // A refused `session.new` never starts the fake agent, so its answer ends
    // the wait for the gate instead of leaving it pending.
    let (mut gate, _) = tokio::select! {
        accepted = reader.accept() => accepted.expect("fake agent connects to gate"),
        refused = &mut create => panic!("session.new answered before the fake agent started: {refused:?}"),
    };
    let mut started = [0];
    gate.read_exact(&mut started)
        .await
        .expect("fake agent starts");
    assert_eq!(started, [b's']);

    let mut observer = connect(&socket).await;
    let list = Request::make("reader-session-list", method::SESSION_LIST, Value::Null);
    let id = wait::guard("fake agent is registered", async {
        loop {
            let sessions: Vec<SessionInfo> =
                serde_json::from_value(ok_payload(exchange(&mut observer, &list).await))
                    .expect("list running reader sessions");
            if let Some(session) = sessions
                .iter()
                .find(|session| session.agent == "readershell")
            {
                break session.id.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    wait_until("session.start hook entered", || async {
        hook_started.exists().then_some(())
    })
    .await;
    if warning_first {
        wait::guard("startup warning reaches public output", async {
            loop {
                let output = Request::make(
                    "reader-output",
                    method::SESSION_OUTPUT,
                    serde_json::json!({"session_id": id, "max_bytes": 1024}),
                );
                let value = ok_payload(exchange(&mut observer, &output).await);
                if value["next_offset"]
                    .as_str()
                    .and_then(|offset| offset.parse::<u64>().ok())
                    .is_some_and(|offset| offset > 0)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    let mut early_marker = [0];
    // timing-allowed: #496 The real worker needs wall time to process PTY input; virtual time cannot drive its socket.
    let early_input = match tokio::time::timeout(
        INITIAL_READER_OBSERVATION_WINDOW,
        gate.read_exact(&mut early_marker),
    )
    .await
    {
        Ok(Ok(_)) => {
            assert_eq!(early_marker, [b'e']);
            true
        }
        Err(_) => false,
        Ok(Err(error)) => panic!("early-input gate read failed: {error}"),
    };
    gate.write_all(b"g")
        .await
        .expect("release fake input reader");
    if stale_mode_first {
        let mut partial_marker = [0];
        gate.read_exact(&mut partial_marker)
            .await
            .expect("fake agent wrote the partial CSI");
        assert_eq!(partial_marker, [b'p']);
        wait::guard("partial CSI reaches public output", async {
            loop {
                let output = Request::make(
                    "reader-partial-output",
                    method::SESSION_OUTPUT,
                    serde_json::json!({"session_id": id, "max_bytes": 1024}),
                );
                let value = ok_payload(exchange(&mut observer, &output).await);
                let expected = b"\x1b[?1;2004h\x1b[?1;2004l\x1b[?1;20".len() as u64;
                if value["next_offset"]
                    .as_str()
                    .and_then(|offset| offset.parse::<u64>().ok())
                    .is_some_and(|offset| offset >= expected)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        gate.write_all(b"c")
            .await
            .expect("complete the fragmented CSI");
    }

    let response = create.await.expect("session.new task");
    let created: SessionInfo = serde_json::from_value(ok_payload(response)).expect("session info");
    if !early_input {
        let mut received_marker = [0];
        gate.read_exact(&mut received_marker)
            .await
            .expect("fake agent accepted the framed input");
        assert_eq!(received_marker, [b'd']);
        gate.write_all(b"f")
            .await
            .expect("inspect the fake reader queue after create");
        gate.read_exact(&mut received_marker)
            .await
            .expect("fake agent inspected remaining input");
        assert_eq!(received_marker, [b'f']);
        let accepted = tokio::fs::read(&received)
            .await
            .expect("read accepted input");
        assert_eq!(accepted, b"hello reader\r");
    }

    let mut control = connect(&socket).await;
    let stop_req = Request::make(
        "stop-reader-agent",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");
    let _ = shutdown.send(());
    let _ = handle.await;
    assert!(
        !early_input,
        "initial input reached the fake agent before it enabled its reader"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_new_initial_input_waits_past_startup_warning() {
    initial_input_waits_for_reader(true, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_new_initial_input_waits_for_silent_reader() {
    initial_input_waits_for_reader(false, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn session_new_initial_input_waits_after_combined_mode_disable_and_fragmented_enable() {
    initial_input_waits_for_reader(false, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn default_sdk_client_waits_for_initial_input_without_reader_signal() {
    let socket = temp_socket("initial-input-default-client");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let received = socket.dir.join("received.bin");
    let script = agents_dir.join("silent-reader.py");
    let fake = agents_dir.join("silent-reader");
    std::fs::write(
        &script,
        format!(
            r#"import os
import tty

tty.setraw(0)
body = bytearray()
while len(body) < len(b"hello reader\r"):
    body.extend(os.read(0, len(b"hello reader\r") - len(body)))
with open({received:?}, "wb") as capture:
    capture.write(body)
while True:
    os.read(0, 1)
"#,
            received = received.display().to_string(),
        ),
    )
    .expect("write silent reader script");
    write_executable(&fake, &python_launcher(&script));
    std::fs::write(
        agents_dir.join("silentreader.toml"),
        format!(
            "base = \"shell\"\nprogram = {}\n",
            toml::Value::String(fake.display().to_string())
        ),
    )
    .expect("write silent reader profile");
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut client = pohunek_client::Client::connect_local(&socket.path)
        .await
        .expect("connect default SDK client");
    let mut params = session_params(&socket);
    params.agent = "silentreader".to_owned();
    params.input = Some("hello reader".to_owned());

    let created = client
        .call::<protocol::method::SessionNew>(params)
        .await
        .expect("default client waits through reader grace");
    assert_eq!(created.applied_input, Some(true));
    wait_until("silent reader received the initial input", || async {
        std::fs::read(&received)
            .ok()
            .filter(|bytes| bytes.as_slice() == b"hello reader\r")
    })
    .await;
    assert_eq!(
        std::fs::read(&received).expect("captured input"),
        b"hello reader\r"
    );

    let mut observer = connect(&socket).await;
    let list = Request::make(
        "default-client-session-list",
        method::SESSION_LIST,
        Value::Null,
    );
    let sessions: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut observer, &list).await))
            .expect("list sessions");
    assert_eq!(
        sessions
            .iter()
            .filter(|session| session.agent == "silentreader")
            .count(),
        1,
        "a request timeout must not leave a detached session for a retry"
    );
    let stop = Request::make(
        "stop-silent-reader",
        method::SESSION_STOP,
        serde_json::to_value(&created.session.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut observer, &stop).await))
            .expect("stop result");
    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn codex_stub_session_publishes_blocked_and_receives_bracketed_input() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let bin_dir = temp_dir("codex-stub-bin");
    let cwd = temp_dir("codex-stub-cwd");
    let input_log_dir = temp_dir("codex-stub-input");
    let input_log = input_log_dir.join("input.bin");
    let cwd_log_dir = temp_dir("codex-stub-pwd");
    let cwd_log = cwd_log_dir.join("pwd.txt");
    write_executable(
        &bin_dir.join("codex"),
        &format!(
            "#!/bin/sh\n{}pwd > \"{}\"\n/bin/sleep 0.2\nprintf '\\033]2;Action Required\\007'\nIFS= read -r line\nprintf '%s' \"$line\" > \"{}\"\n/bin/sleep 30\n",
            official_runtimes::sh_version_answer("codex"),
            cwd_log.display(),
            input_log.display()
        ),
    );

    let socket = temp_socket("codex-stub");
    let (shutdown, handle, _) = spawn_server_serving_official_runtimes(
        &socket,
        "0.0.0",
        support::hermetic_registry_config(),
    )
    .await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make("subscribe-codex-stub", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let created: SessionInfo = {
        let _path = PathGuard::prepend(&bin_dir);
        serde_json::from_value(ok_payload(
            create_session_with_agent(&mut control, RuntimeRef::codex(), cwd.to_path_buf()).await,
        ))
        .expect("codex session info")
    };
    assert_eq!(created.agent, "codex");

    let streamed = wait_for_agent_state_event(
        &mut subscriber,
        &created.id,
        AgentActivity::Blocked,
        StateSource::OscTitle,
    )
    .await;
    assert_eq!(streamed.payload()["activity"], Value::from("blocked"));
    assert_eq!(streamed.payload()["source"], Value::from("osc_title"));

    let input = input_session(&mut control, &created.id, "run tests").await;
    assert!(input.accepted);
    let bytes = read_file_until(&input_log, b"\x1b[200~run tests\x1b[201~").await;
    assert_eq!(bytes, b"\x1b[200~run tests\x1b[201~");

    let launched_cwd = tokio::fs::read_to_string(&cwd_log)
        .await
        .expect("read cwd log");
    assert_eq!(launched_cwd.trim(), cwd.display().to_string());

    let stop_req = Request::make(
        "session-stop-codex-stub",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn claude_stub_session_publishes_screen_blocked_and_receives_plain_input() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let bin_dir = temp_dir("claude-stub-bin");
    let cwd = temp_dir("claude-stub-cwd");
    let input_log_dir = temp_dir("claude-stub-input");
    let input_log = input_log_dir.join("input.bin");
    write_executable(
        &bin_dir.join("claude"),
        &format!(
            "#!/bin/sh\n{}/bin/sleep 0.2\nprintf '\\033[2J\\033[HReview\\r\\n────\\r\\nenter to select\\r\\nesc to cancel\\r\\n↑/↓ to navigate\\r\\n'\nIFS= read -r line\nprintf '%s' \"$line\" > \"{}\"\n/bin/sleep 30\n",
            official_runtimes::sh_version_answer("claude"),
            input_log.display()
        ),
    );

    let socket = temp_socket("claude-stub");
    let (shutdown, handle, _) = spawn_server_serving_official_runtimes(
        &socket,
        "0.0.0",
        support::hermetic_registry_config(),
    )
    .await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make("subscribe-claude-stub", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let created: SessionInfo = {
        let _path = PathGuard::prepend(&bin_dir);
        serde_json::from_value(ok_payload(
            create_session_with_agent(&mut control, RuntimeRef::claude(), cwd.to_path_buf()).await,
        ))
        .expect("claude session info")
    };
    assert_eq!(created.agent, "claude");

    let streamed = wait_for_agent_state_event(
        &mut subscriber,
        &created.id,
        AgentActivity::Blocked,
        StateSource::Screen,
    )
    .await;
    assert_eq!(streamed.payload()["activity"], Value::from("blocked"));
    assert_eq!(streamed.payload()["source"], Value::from("screen"));

    let input = input_session(&mut control, &created.id, "hello Claude").await;
    assert!(input.accepted);
    let bytes = read_file_until(&input_log, b"hello Claude").await;
    assert_eq!(bytes, b"hello Claude");

    let stop_req = Request::make(
        "session-stop-claude-stub",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// How long a `session.new --input` for a stub of a `screen_idle` runtime waits
/// for a prompt that the stub never renders. The stub enables bracketed paste
/// at once, so the grace covers the window in which bracketed paste alone would
/// release the input of a runtime without that declaration, and it stays short
/// so the refusal path finishes quickly.
const UNRENDERED_PROMPT_GRACE: Duration = Duration::from_millis(1_500);

/// Prompt box the Claude stub renders, shaped like the idle screen in
/// `compat/claude/screens/idle.txt`: a rule, the `❯` line, and a rule. The
/// Claude manifest's `live_prompt_box` rule matches the `❯` line between the
/// rules.
const CLAUDE_PROMPT_BOX_SCREEN: &str = "\x1b[2J\x1b[H\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{276f} \r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\n? for shortcuts\r\n";

// Real Codex 0.160.0 recordings, replayed by an external runtime through a
// real PTY and the official package. The stale title is an adversary, not
// something the held dialogs emitted. No credentials or model are involved.
fn codex_dialog_capture(surface: &str, columns: u16) -> Vec<u8> {
    let capture: Value = serde_json::from_str(
        &std::fs::read_to_string(
            pohunek_test_support::workspace_root()
                .join("compat/codex/screens/startup-dialogs")
                .join(format!("{surface}-{columns}.json")),
        )
        .expect("read captured dialog"),
    )
    .expect("parse captured dialog");
    let raw = capture["raw_hex"].as_str().expect("raw PTY bytes");
    (0..raw.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&raw[offset..offset + 2], 16).expect("hex byte"))
        .collect()
}

async fn assert_codex_input_delivered_once(received: &Path) {
    let bytes = read_file_until(received, b"hello Codex").await;
    assert_eq!(
        bytes
            .windows(b"hello Codex".len())
            .filter(|window| *window == b"hello Codex")
            .count(),
        1,
        "task delivered once after dismissal"
    );
}

async fn codex_captured_startup_dialog(
    surface: &str,
    columns: u16,
    stale_title: bool,
    dismiss: bool,
) {
    let socket = temp_socket("codex-startup-dialog");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let received = socket.dir.join("received.bin");
    let script = socket.dir.join("codex-startup-dialog.py");
    let bytes = codex_dialog_capture(surface, columns);
    let release_path = socket.dir.join("release.sock");
    let release = dismiss
        .then(|| tokio::net::UnixListener::bind(&release_path).expect("bind release barrier"));
    codex_startup_dialog_stub(
        &script,
        &received,
        stale_title,
        release.as_ref().map(|_| release_path.as_path()),
        &bytes,
    );
    write_official_profile(&agents_dir, "codex", "codexstartupdialog", &script);
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        initial_input_startup_grace: CODEX_POPUP_GRACE,
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make(
        "subscribe-codex-startup-dialog",
        method::SUBSCRIBE,
        Value::Null,
    );
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let mut params = session_params(&socket);
    params.cols = columns;
    params.rows = 40;
    "codexstartupdialog".clone_into(&mut params.agent);
    params.input = Some("hello Codex".to_owned());
    let mut create =
        tokio::spawn(async move { create_session_with_params(&mut control, params).await });
    let published = tokio::select! {
        event = wait_for_any_agent_state_event(
            &mut subscriber,
            AgentActivity::Blocked,
            StateSource::Screen,
        ) => event,
        answered = &mut create => panic!(
            "session.new answered before {surface}-{columns} was classified: {answered:?}"
        ),
    };
    assert_eq!(published.payload()["activity"], Value::from("blocked"));
    assert_eq!(published.payload()["source"], Value::from("screen"));

    if let Some(release) = release {
        assert!(
            std::fs::read(&received).unwrap_or_default().is_empty(),
            "held dialog received task bytes"
        );
        let (mut barrier, _) = release.accept().await.expect("runtime awaits dismissal");
        barrier.write_all(b"release").await.expect("dismiss dialog");
        let created: SessionInfo =
            serde_json::from_value(ok_payload(create.await.expect("create task joins")))
                .expect("created session");
        assert_codex_input_delivered_once(&received).await;
        let mut observer = connect(&socket).await;
        let request = Request::make(
            "dialog-stop",
            method::SESSION_STOP,
            serde_json::to_value(&created.id).expect("session id"),
        );
        let _: SessionStopResult =
            serde_json::from_value(ok_payload(exchange(&mut observer, &request).await))
                .expect("stop session");
    } else {
        let error = err_payload(create.await.expect("create task joins"));
        assert_eq!(error.code, "session_input_not_ready");

        let mut observer = connect(&socket).await;
        let list = Request::make(
            "codex-startup-dialog-list",
            method::SESSION_LIST,
            Value::Null,
        );
        let sessions: Vec<SessionInfo> =
            serde_json::from_value(ok_payload(exchange(&mut observer, &list).await))
                .expect("list sessions");
        assert!(sessions.is_empty(), "the refused session was rolled back");
        let captured = std::fs::read(&received).unwrap_or_default();
        assert!(
            captured.is_empty(),
            "the stub received input while the startup dialog held the screen"
        );
    }
    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn codex_captured_login_80_holds_initial_input() {
    codex_captured_startup_dialog("login", 80, true, false).await;
    codex_captured_startup_dialog("login", 80, false, false).await;
    codex_captured_startup_dialog("login", 80, true, true).await;
}

#[tokio::test]
async fn codex_captured_login_160_holds_initial_input() {
    codex_captured_startup_dialog("login", 160, true, false).await;
    codex_captured_startup_dialog("login", 160, false, false).await;
    codex_captured_startup_dialog("login", 160, true, true).await;
}

#[tokio::test]
async fn codex_captured_api_entry_80_holds_initial_input() {
    codex_captured_startup_dialog("api-entry", 80, true, false).await;
    codex_captured_startup_dialog("api-entry", 80, false, false).await;
    codex_captured_startup_dialog("api-entry", 80, true, true).await;
}

#[tokio::test]
async fn codex_captured_api_entry_160_holds_initial_input() {
    codex_captured_startup_dialog("api-entry", 160, true, false).await;
    codex_captured_startup_dialog("api-entry", 160, false, false).await;
    codex_captured_startup_dialog("api-entry", 160, true, true).await;
}

#[tokio::test]
async fn codex_captured_settings_80_holds_initial_input() {
    codex_captured_startup_dialog("settings", 80, true, false).await;
    codex_captured_startup_dialog("settings", 80, false, false).await;
    codex_captured_startup_dialog("settings", 80, true, true).await;
}

#[tokio::test]
async fn codex_captured_settings_160_holds_initial_input() {
    codex_captured_startup_dialog("settings", 160, true, false).await;
    codex_captured_startup_dialog("settings", 160, false, false).await;
    codex_captured_startup_dialog("settings", 160, true, true).await;
}

#[tokio::test]
async fn codex_captured_trust_20_holds_initial_input() {
    codex_captured_startup_dialog("trust", 20, true, false).await;
    codex_captured_startup_dialog("trust", 20, false, false).await;
    codex_captured_startup_dialog("trust", 20, true, true).await;
}

#[tokio::test]
async fn codex_captured_trust_30_holds_initial_input() {
    codex_captured_startup_dialog("trust", 30, true, false).await;
    codex_captured_startup_dialog("trust", 30, false, false).await;
    codex_captured_startup_dialog("trust", 30, true, true).await;
}

#[tokio::test]
async fn codex_captured_trust_80_holds_initial_input() {
    codex_captured_startup_dialog("trust", 80, true, false).await;
    codex_captured_startup_dialog("trust", 80, false, false).await;
    codex_captured_startup_dialog("trust", 80, true, true).await;
}

#[tokio::test]
async fn codex_captured_trust_160_holds_initial_input() {
    codex_captured_startup_dialog("trust", 160, true, false).await;
    codex_captured_startup_dialog("trust", 160, false, false).await;
    codex_captured_startup_dialog("trust", 160, true, true).await;
}

/// Writes a host profile `name` over the pinned official Claude package whose
/// program is a launcher for `script`. A profile-backed launch writes the
/// initial input once readiness is observed; an unprofiled `claude` launch
/// passes the prompt as an argument (`prompt_arg`) and never waits.
fn write_claude_profile(agents_dir: &Path, name: &str, script: &Path) {
    write_official_profile(agents_dir, "claude", name, script);
}

/// A Claude stub that enables bracketed paste and never renders the prompt
/// box records every byte it reads into `received`.
fn claude_stub_without_prompt_box(script: &Path, received: &Path) {
    std::fs::write(
        script,
        format!(
            r#"import os
import termios
import tty

# The daemon writes the prompt once bracketed paste is on, or after its startup
# grace when this script starts slowly. Raw mode is set without flushing
# (TCSANOW, not tty.setraw's TCSAFLUSH), so a prompt that arrived before it is
# still read.
tty.setraw(0, termios.TCSANOW)
with open({received:?}, "wb") as capture:
    os.write(1, b"\x1b[?2004h")
    while True:
        chunk = os.read(0, 4096)
        if not chunk:
            break
        capture.write(chunk)
        capture.flush()
"#,
            received = received.display().to_string(),
        ),
    )
    .expect("write Claude stub without prompt box");
}

#[tokio::test]
async fn claude_initial_input_is_not_sent_before_the_prompt_box_renders() {
    let socket = temp_socket("claude-no-prompt");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let received = socket.dir.join("received.bin");
    let script = socket.dir.join("claude-no-prompt.py");
    claude_stub_without_prompt_box(&script, &received);
    write_claude_profile(&agents_dir, "claudenoprompt", &script);
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        initial_input_startup_grace: UNRENDERED_PROMPT_GRACE,
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let mut params = session_params(&socket);
    "claudenoprompt".clone_into(&mut params.agent);
    params.input = Some("hello Claude".to_owned());
    let response = create_session_with_params(&mut control, params).await;
    let error = err_payload(response);
    assert_eq!(error.code, "session_input_not_ready");

    let list = Request::make("claude-no-prompt-list", method::SESSION_LIST, Value::Null);
    let sessions: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut control, &list).await))
            .expect("list sessions");
    assert!(sessions.is_empty(), "the refused session was rolled back");
    let captured = std::fs::read(&received).unwrap_or_default();
    assert!(
        captured.is_empty(),
        "the stub received input before its prompt box rendered"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn claude_initial_input_is_delivered_once_the_prompt_box_renders() {
    let socket = temp_socket("claude-prompt");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let received = socket.dir.join("received.bin");
    let script = socket.dir.join("claude-prompt.py");
    // Hex escapes keep the screen's bytes exact inside the Python literal.
    let screen_hex = CLAUDE_PROMPT_BOX_SCREEN
        .bytes()
        .map(|byte| format!("\\x{byte:02x}"))
        .collect::<Vec<_>>()
        .concat();
    std::fs::write(
        &script,
        format!(
            r#"import os
import tty

os.write(1, b"\x1b[?2004h")
os.write(1, b"{screen_hex}")
tty.setraw(0)
with open({received:?}, "wb") as capture:
    while True:
        chunk = os.read(0, 4096)
        if not chunk:
            break
        capture.write(chunk)
        capture.flush()
"#,
            received = received.display().to_string(),
        ),
    )
    .expect("write Claude stub with prompt box");
    write_claude_profile(&agents_dir, "claudeprompt", &script);
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let mut params = session_params(&socket);
    "claudeprompt".clone_into(&mut params.agent);
    params.input = Some("hello Claude".to_owned());
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_session_with_params(&mut control, params).await,
    ))
    .expect("claude session info");
    let bytes = read_file_until(&received, b"hello Claude").await;
    assert_eq!(
        bytes
            .windows(b"hello Claude".len())
            .filter(|window| *window == b"hello Claude")
            .count(),
        1,
        "the initial input is delivered exactly once"
    );

    let stop_req = Request::make(
        "claude-prompt-stop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Writes `agents_dir/manifests/<name>.toml` with `manifest` and a Claude
/// profile `name` over `script` that selects it as its override manifest.
fn write_claude_profile_with_manifest(
    agents_dir: &Path,
    name: &str,
    script: &Path,
    manifest: &str,
) {
    let manifests_dir = agents_dir.join("manifests");
    std::fs::create_dir_all(&manifests_dir).expect("create manifests directory");
    std::fs::write(manifests_dir.join(format!("{name}.toml")), manifest)
        .expect("write override manifest");
    write_claude_profile(agents_dir, name, script);
    let profile = agents_dir.join(format!("{name}.toml"));
    let body = std::fs::read_to_string(&profile).expect("read profile");
    std::fs::write(&profile, format!("{body}manifest = \"{name}\"\n"))
        .expect("declare override manifest");
}

/// A Claude-based profile whose override manifest declares no idle rule on a
/// screen region loads, and its sessions write the initial input as soon as
/// bracketed paste is on, because screen readiness could never be observed.
#[tokio::test]
async fn claude_profile_without_a_screen_idle_rule_delivers_input_on_bracketed_paste() {
    let socket = temp_socket("claude-profile-no-idle");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let variants = [
        (
            "blockedonly",
            "[[rules]]\n\
             id = \"custom-blocked\"\n\
             state = \"blocked\"\n\
             priority = 1\n\
             region = \"whole_recent\"\n\
             contains = \"custom blocker\"\n",
        ),
        ("emptyrules", "rules = []\n"),
    ];
    let mut captures = Vec::new();
    for (name, manifest) in variants {
        let received = socket.dir.join(format!("{name}-received.bin"));
        let script = socket.dir.join(format!("{name}.py"));
        claude_stub_without_prompt_box(&script, &received);
        write_claude_profile_with_manifest(&agents_dir, name, &script, manifest);
        captures.push((name, received));
    }
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;

    for (name, received) in captures {
        let mut params = session_params(&socket);
        name.clone_into(&mut params.agent);
        params.input = Some("hello Claude".to_owned());
        let created: SessionInfo = serde_json::from_value(ok_payload(
            create_session_with_params(&mut control, params).await,
        ))
        .unwrap_or_else(|err| panic!("profile {name} session info: {err}"));
        let bytes = read_file_until(&received, b"hello Claude").await;
        assert_eq!(
            bytes
                .windows(b"hello Claude".len())
                .filter(|window| *window == b"hello Claude")
                .count(),
            1,
            "profile {name} receives the initial input exactly once"
        );
        let stop_req = Request::make(
            "claude-profile-no-idle-stop",
            method::SESSION_STOP,
            serde_json::to_value(&created.id).expect("serialize id"),
        );
        let _: SessionStopResult =
            serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
                .expect("stop result");
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A Claude-based profile whose override manifest declares an idle rule on a
/// screen region keeps waiting for that rule to match before it writes the
/// initial input.
#[tokio::test]
async fn claude_profile_with_a_screen_idle_rule_waits_for_screen_readiness() {
    let socket = temp_socket("claude-profile-idle");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let received = socket.dir.join("received.bin");
    let script = socket.dir.join("claude-profile-idle.py");
    claude_stub_without_prompt_box(&script, &received);
    write_claude_profile_with_manifest(
        &agents_dir,
        "idlerule",
        &script,
        "[[rules]]\n\
         id = \"custom_prompt_box\"\n\
         state = \"idle\"\n\
         priority = 950\n\
         region = \"prompt_box_body\"\n\
         line_regex = ['^\\s*\u{276f}']\n",
    );
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        initial_input_startup_grace: UNRENDERED_PROMPT_GRACE,
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let mut params = session_params(&socket);
    "idlerule".clone_into(&mut params.agent);
    params.input = Some("hello Claude".to_owned());
    let error = err_payload(create_session_with_params(&mut control, params).await);
    assert_eq!(error.code, "session_input_not_ready");
    let captured = std::fs::read(&received).unwrap_or_default();
    assert!(
        captured.is_empty(),
        "the stub received input before its prompt box rendered"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The startup update popup Codex 0.160 draws over the whole screen when it
/// finds a newer release: the heading with both releases, the release-notes
/// line, the three numbered choices and the footer, measured at 100 columns
/// from the real popup capture this regression mirrors.
const CODEX_UPDATE_POPUP_SCREEN: &str = "\x1b[2J\x1b[H\r\n  Update available \u{b7} 0.160.0 \u{2192} 0.162.1\r\n  Release notes: https://github.com/openai/codex/releases/latest\r\n\r\n\u{203a} 1. Update now (runs `npm install -g @openai/codex`)\r\n  2. Skip\r\n  3. Skip until next version\r\n\r\n  enter continue \u{b7} esc skip\r\n";

/// The provisional composer Codex 0.160 draws while it loads, before it sets a
/// terminal title and before (or instead of) the update popup. No rule of the
/// Codex manifest matches it.
const CODEX_PROVISIONAL_COMPOSER_SCREEN: &str = "\x1b[2J\x1b[H  >_ OpenAI Codex (v0.160.0)\r\n     loading\r\n\r\n  Your keyboard has entered the chat.\r\n\r\n\u{203a} Ask Codex to do anything\r\n";

/// The Python line that sets the idle terminal title Codex sets once its
/// composer reads input, measured from the real captures (`ESC ] 0 ; work BEL`).
const CODEX_IDLE_TITLE_PY: &str = r#"os.write(1, b"\x1b]0;work\x07")"#;

/// Grace for the popup scenarios whose create must survive past the debounced
/// blocked publication of the popup (the production startup grace of 3 s plus
/// its confirmations) before the test releases or judges it.
const CODEX_POPUP_GRACE: Duration = Duration::from_secs(8);

/// Writes a Codex stub that replays the measured startup sequence: raw mode
/// with `TCSANOW` (no flush) before any output, bracketed paste on at once,
/// then [`CODEX_UPDATE_POPUP_SCREEN`] over the whole screen, and, with
/// `stale_title`, a synthetic adversarial idle-looking title `work` while the
/// popup still holds — the real popup set no title, so the stale title is the
/// case the rule must survive, not one the capture observed.
///
/// With a `release` socket the stub then waits for one byte (the test's
/// release of the popup), redraws [`CODEX_PROVISIONAL_COMPOSER_SCREEN`], sets
/// the idle title again, and records every byte it reads into `received`.
/// Without one the popup never goes and the stub records from the start,
/// mirroring a popup the operator never dismisses.
fn codex_update_popup_stub(
    script: &Path,
    received: &Path,
    stale_title: bool,
    release: Option<&Path>,
) {
    codex_startup_dialog_stub(
        script,
        received,
        stale_title,
        release,
        CODEX_UPDATE_POPUP_SCREEN.as_bytes(),
    );
}

fn codex_startup_dialog_stub(
    script: &Path,
    received: &Path,
    stale_title: bool,
    release: Option<&Path>,
    screen: &[u8],
) {
    let hex = |screen: &[u8]| {
        screen
            .iter()
            .map(|byte| format!("\\x{byte:02x}"))
            .collect::<Vec<_>>()
            .concat()
    };
    let stale_title_line = if stale_title {
        format!("{CODEX_IDLE_TITLE_PY}\n")
    } else {
        String::new()
    };
    let release_block = release.map_or_else(String::new, |release| {
        format!(
            "release = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\nrelease.connect({release_display:?})\nrelease.recv(1)\n",
            release_display = release.display().to_string(),
        )
    });
    let redraw_block = release.map_or_else(String::new, |_| {
        format!(
            "os.write(1, b\"{}\")\n{CODEX_IDLE_TITLE_PY}\n",
            hex(CODEX_PROVISIONAL_COMPOSER_SCREEN.as_bytes())
        )
    });
    std::fs::write(
        script,
        format!(
            r#"import os
import socket
import termios
import tty

# Raw mode before any output, with TCSANOW: a mode switch that late-flushes
# could drop the fixed prompt the daemon writes once readiness shows.
tty.setraw(0, termios.TCSANOW)
os.write(1, b"\x1b[?2004h")
os.write(1, b"{popup_hex}")
{stale_title_line}{release_block}{redraw_block}with open({received:?}, "wb") as capture:
    while True:
        chunk = os.read(0, 4096)
        if not chunk:
            break
        capture.write(chunk)
        capture.flush()
"#,
            popup_hex = hex(screen),
            received = received.display().to_string(),
        ),
    )
    .expect("write Codex update-popup stub");
}

/// A `session.new --input` for a profile over the official Codex package whose
/// stub draws the startup update popup and never lets it go is refused with
/// `session_input_not_ready`, rolled back, and writes nothing: the popup holds
/// the screen and no readiness evidence may release the prompt into it.
#[tokio::test]
async fn codex_initial_input_is_refused_while_the_startup_update_popup_holds_the_screen() {
    codex_popup_is_refused(None).await;
}

/// A profile override without idle evidence keeps Codex's strict policy.
#[tokio::test]
async fn codex_initial_input_is_refused_with_an_override_without_idle_evidence() {
    codex_popup_is_refused(Some("rules = []\n")).await;
}

async fn codex_popup_is_refused(override_manifest: Option<&str>) {
    let socket = temp_socket("codex-popup");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let received = socket.dir.join("received.bin");
    let script = socket.dir.join("codex-popup.py");
    codex_update_popup_stub(&script, &received, false, None);
    write_official_profile(&agents_dir, "codex", "codexpopup", &script);
    if let Some(manifest) = override_manifest {
        let manifests = agents_dir.join("manifests");
        std::fs::create_dir(&manifests).expect("create override manifest directory");
        std::fs::write(manifests.join("popup.toml"), manifest).expect("write override manifest");
        let profile = agents_dir.join("codexpopup.toml");
        let body = std::fs::read_to_string(&profile).expect("read Codex profile");
        std::fs::write(profile, format!("{body}manifest = \"popup\"\n"))
            .expect("declare override manifest");
    }
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        initial_input_startup_grace: UNRENDERED_PROMPT_GRACE,
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let mut params = session_params(&socket);
    params.cols = 100;
    "codexpopup".clone_into(&mut params.agent);
    params.input = Some("hello Codex".to_owned());
    let error = err_payload(create_session_with_params(&mut control, params).await);
    assert_eq!(error.code, "session_input_not_ready");

    let list = Request::make("codex-popup-list", method::SESSION_LIST, Value::Null);
    let sessions: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut control, &list).await))
            .expect("list sessions");
    assert!(sessions.is_empty(), "the refused session was rolled back");
    let captured = std::fs::read(&received).unwrap_or_default();
    assert!(
        captured.is_empty(),
        "the stub received input while the update popup held the screen"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Waits for the first `agent_state` event that reports `activity` from
/// `source` for any session and returns it. The popup regressions run one
/// session at a time and its create is still waiting for readiness when the
/// popup is classified, so the event cannot name a session the test knows.
async fn wait_for_any_agent_state_event(
    framed: &mut Framed<UnixStream, LinesCodec>,
    activity: AgentActivity,
    source: StateSource,
) -> Event {
    let expected_activity = serde_json::to_value(activity).expect("serialize activity");
    let expected_source = serde_json::to_value(source).expect("serialize source");
    let deadline = tokio::time::Instant::now() + HANG_GUARD;
    let mut seen = Vec::new();
    loop {
        let now = tokio::time::Instant::now();
        assert!(
            now < deadline,
            "agent_state event did not arrive before timeout; expected activity={expected_activity} source={expected_source}; seen={seen:?}"
        );
        let line = tokio::time::timeout(deadline - now, framed.next())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "agent_state event did not arrive before timeout; expected activity={expected_activity} source={expected_source}; seen={seen:?}"
                )
            })
            .expect("a streamed event line")
            .expect("event framing ok");
        let streamed: Event = serde_json::from_str(&line).expect("parse event");
        if streamed.event() == event::AGENT_STATE {
            seen.push(streamed.payload().clone());
            if streamed.payload()["activity"] == expected_activity
                && streamed.payload()["source"] == expected_source
            {
                return streamed;
            }
        }
    }
}

/// The same refusal with a synthetic adversarial idle-looking terminal title
/// (`work`) set while the popup holds — the measured popup set no title, so
/// this is the case the rule must survive, not one the capture observed: the
/// popup blocker outranks the title's idle evidence, so the stale title must
/// not release the prompt. The test first waits for the daemon to classify
/// the popup as blocked, which is what makes the later refusal a popup
/// verdict.
#[tokio::test]
async fn codex_initial_input_is_refused_while_a_stale_idle_title_survives_under_the_update_popup() {
    let socket = temp_socket("codex-popup-stale");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let received = socket.dir.join("received.bin");
    let script = socket.dir.join("codex-popup-stale.py");
    codex_update_popup_stub(&script, &received, true, None);
    write_official_profile(&agents_dir, "codex", "codexpopupstale", &script);
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        initial_input_startup_grace: CODEX_POPUP_GRACE,
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make(
        "subscribe-codex-popup-stale",
        method::SUBSCRIBE,
        Value::Null,
    );
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let mut params = session_params(&socket);
    params.cols = 100;
    "codexpopupstale".clone_into(&mut params.agent);
    params.input = Some("hello Codex".to_owned());
    let mut create =
        tokio::spawn(async move { create_session_with_params(&mut control, params).await });
    let published = tokio::select! {
        event = wait_for_any_agent_state_event(
            &mut subscriber,
            AgentActivity::Blocked,
            StateSource::Screen,
        ) => event,
        answered = &mut create => panic!(
            "session.new answered before the popup was classified: {answered:?}"
        ),
    };
    assert_eq!(published.payload()["activity"], Value::from("blocked"));
    assert_eq!(published.payload()["source"], Value::from("screen"));

    let error = err_payload(create.await.expect("create task joins"));
    assert_eq!(error.code, "session_input_not_ready");

    let mut observer = connect(&socket).await;
    let list = Request::make("codex-popup-stale-list", method::SESSION_LIST, Value::Null);
    let sessions: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut observer, &list).await))
            .expect("list sessions");
    assert!(sessions.is_empty(), "the refused session was rolled back");
    let captured = std::fs::read(&received).unwrap_or_default();
    assert!(
        captured.is_empty(),
        "the stub received input while the popup outranked its stale idle title"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A popup the test dismisses through the stub's release socket, followed by
/// the real idle title, delivers the initial input exactly once: the strict
/// wait ends on the first eligible idle evidence, not on the popup's
/// disappearance alone.
#[tokio::test]
async fn codex_initial_input_is_delivered_once_the_update_popup_clears_and_the_title_turns_idle() {
    let socket = temp_socket("codex-popup-release");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let received = socket.dir.join("received.bin");
    let script = socket.dir.join("codex-popup-release.py");
    let release_path = socket.dir.join("codex-popup-release.sock");
    codex_update_popup_stub(&script, &received, true, Some(&release_path));
    write_official_profile(&agents_dir, "codex", "codexpopuprelease", &script);
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        initial_input_startup_grace: CODEX_POPUP_GRACE,
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let release = tokio::net::UnixListener::bind(&release_path).expect("bind the release listener");

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make(
        "subscribe-codex-popup-release",
        method::SUBSCRIBE,
        Value::Null,
    );
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let mut params = session_params(&socket);
    params.cols = 100;
    "codexpopuprelease".clone_into(&mut params.agent);
    params.input = Some("hello Codex".to_owned());
    let mut create =
        tokio::spawn(async move { create_session_with_params(&mut control, params).await });
    let published = tokio::select! {
        event = wait_for_any_agent_state_event(
            &mut subscriber,
            AgentActivity::Blocked,
            StateSource::Screen,
        ) => event,
        answered = &mut create => panic!(
            "session.new answered before the popup was classified: {answered:?}"
        ),
    };
    assert_eq!(published.payload()["activity"], Value::from("blocked"));

    // The daemon holds the popup as blocked. Dismissing it hands the stub the
    // release byte, after which it redraws the composer and sets the idle
    // title.
    let (mut release_stream, _) = release
        .accept()
        .await
        .expect("the stub awaits the popup release");
    release_stream
        .write_all(b"release")
        .await
        .expect("release the popup");

    let created: SessionInfo =
        serde_json::from_value(ok_payload(create.await.expect("create task joins")))
            .expect("codex session info once the popup cleared");
    let bytes = read_file_until(&received, b"hello Codex").await;
    assert_eq!(
        bytes
            .windows(b"hello Codex".len())
            .filter(|window| *window == b"hello Codex")
            .count(),
        1,
        "the initial input is delivered exactly once"
    );

    let mut observer = connect(&socket).await;
    let stop_req = Request::make(
        "codex-popup-release-stop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut observer, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Writes a host profile `name` over the pinned official `runtime` package
/// whose program is a launcher for `script`.
fn write_official_profile(agents_dir: &Path, runtime: &str, name: &str, script: &Path) {
    let stub = agents_dir.join(format!("{name}-agent"));
    write_executable(
        &stub,
        &official_runtimes::python_agent_launcher(runtime, script),
    );
    std::fs::write(
        agents_dir.join(format!("{name}.toml")),
        format!(
            "base = \"{runtime}\"\n{}program = {}\n",
            official_runtimes::profile_pin(runtime),
            toml::Value::String(stub.display().to_string())
        ),
    )
    .expect("write official runtime stub profile");
}

/// The idle classic prompt of Hermes, which its manifest's `idle_prompt` rule
/// matches on the bottom screen lines.
const HERMES_IDLE_PROMPT_SCREEN: &str = "\x1b[2J\x1b[HWelcome to Hermes Agent!\r\n\r\n\u{276f} ";

/// The longest the Hermes stub waits for queued input before it flushes: past
/// the bound in which an older client's initial input is written to an agent
/// that shows no readiness, like a Hermes whose start outlasts that bound.
fn hermes_stub_flush_delay() -> Duration {
    pohunek_service_config::COMPAT_INITIAL_INPUT_STARTUP_GRACE * 2
}

/// A Hermes stub that, like Hermes's startup terminal query, enters cbreak
/// mode, waits until input is queued or [`hermes_stub_flush_delay`] passes, and
/// restores its terminal mode with `TCSAFLUSH`, which discards every byte
/// queued so far; only then does it render its idle prompt, enable bracketed
/// paste and record every byte it reads into `received`.
fn hermes_stub_flushing_at_startup(script: &Path, received: &Path) {
    let screen_hex = HERMES_IDLE_PROMPT_SCREEN
        .bytes()
        .map(|byte| format!("\\x{byte:02x}"))
        .collect::<Vec<_>>()
        .concat();
    std::fs::write(
        script,
        format!(
            r#"import os
import select
import termios
import tty

attributes = termios.tcgetattr(0)
tty.setcbreak(0, termios.TCSANOW)
select.select([0], [], [], {delay})
termios.tcsetattr(0, termios.TCSAFLUSH, attributes)
os.write(1, b"{screen_hex}")
os.write(1, b"\x1b[?2004h")
tty.setraw(0, termios.TCSANOW)
with open({received:?}, "wb") as capture:
    while True:
        chunk = os.read(0, 4096)
        if not chunk:
            break
        capture.write(chunk)
        capture.flush()
"#,
            delay = hermes_stub_flush_delay().as_secs_f64(),
            received = received.display().to_string(),
        ),
    )
    .expect("write Hermes stub flushing at startup");
}

/// Hermes declares that only its rendered prompt shows readiness, so its
/// initial input waits past the startup flush for both an older client and one
/// that opts into the extended wait, and arrives once.
#[tokio::test]
async fn hermes_initial_input_waits_past_its_startup_input_flush() {
    let socket = temp_socket("hermes-flush");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let clients = [("older", None), ("extended", Some(true))];
    for (client, _) in clients {
        let script = socket.dir.join(format!("hermes-flush-{client}.py"));
        hermes_stub_flushing_at_startup(&script, &socket.dir.join(format!("{client}.bin")));
        write_official_profile(
            &agents_dir,
            "hermes",
            &format!("hermesflush{client}"),
            &script,
        );
    }
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    for (client, extended_input_ready_wait) in clients {
        let mut params = session_params(&socket);
        params.agent = format!("hermesflush{client}");
        params.input = Some("hello Hermes".to_owned());
        params.extended_input_ready_wait = extended_input_ready_wait;
        let created: SessionInfo = serde_json::from_value(ok_payload(
            create_session_with_params(&mut control, params).await,
        ))
        .expect("hermes session info");
        let bytes =
            read_file_until(&socket.dir.join(format!("{client}.bin")), b"hello Hermes").await;
        assert_eq!(
            bytes
                .windows(b"hello Hermes".len())
                .filter(|window| *window == b"hello Hermes")
                .count(),
            1,
            "the {client} client's initial input is delivered exactly once"
        );
        let stop_req = Request::make(
            &format!("hermes-flush-stop-{client}"),
            method::SESSION_STOP,
            serde_json::to_value(&created.id).expect("serialize id"),
        );
        let _: SessionStopResult =
            serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
                .expect("stop result");
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The Hermes package declares a startup grace longer than the host's, so a
/// Hermes whose prompt renders after the host's grace has passed still gets
/// the initial input of a client that opted into the extended wait, once. An
/// older client keeps the host's grace, and its launch is refused at that
/// grace with nothing written.
#[tokio::test]
async fn hermes_initial_input_waits_for_its_declared_startup_grace() {
    let socket = temp_socket("hermes-slow-start");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let received = socket.dir.join("received.bin");
    let script = socket.dir.join("hermes-slow-start.py");
    let screen_hex = HERMES_IDLE_PROMPT_SCREEN
        .bytes()
        .map(|byte| format!("\\x{byte:02x}"))
        .collect::<Vec<_>>()
        .concat();
    std::fs::write(
        &script,
        format!(
            r#"import os
import termios
import time
import tty

time.sleep({delay})
os.write(1, b"{screen_hex}")
os.write(1, b"\x1b[?2004h")
tty.setraw(0, termios.TCSANOW)
with open({received:?}, "wb") as capture:
    while True:
        chunk = os.read(0, 4096)
        if not chunk:
            break
        capture.write(chunk)
        capture.flush()
"#,
            delay = (UNRENDERED_PROMPT_GRACE * 2).as_secs_f64(),
            received = received.display().to_string(),
        ),
    )
    .expect("write slowly starting Hermes stub");
    write_official_profile(&agents_dir, "hermes", "hermesslowstart", &script);
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        initial_input_startup_grace: UNRENDERED_PROMPT_GRACE,
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let mut params = session_params(&socket);
    "hermesslowstart".clone_into(&mut params.agent);
    params.input = Some("hello Hermes".to_owned());
    params.extended_input_ready_wait = Some(true);
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_session_with_params(&mut control, params).await,
    ))
    .expect("hermes session info");
    let bytes = read_file_until(&received, b"hello Hermes").await;
    assert_eq!(
        bytes
            .windows(b"hello Hermes".len())
            .filter(|window| *window == b"hello Hermes")
            .count(),
        1,
        "the initial input is delivered exactly once"
    );
    let stop_req = Request::make(
        "hermes-slow-start-stop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");
    std::fs::remove_file(&received).expect("clear the first session's capture");

    let mut params = session_params(&socket);
    "hermesslowstart".clone_into(&mut params.agent);
    params.input = Some("hello Hermes".to_owned());
    let error = err_payload(create_session_with_params(&mut control, params).await);
    assert_eq!(error.code, "session_input_not_ready");
    let captured = std::fs::read(&received).unwrap_or_default();
    assert!(
        captured.is_empty(),
        "the older client's input was written into a prompt that had not rendered"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Pi declares that only its idle editor frame shows readiness, so a Pi that
/// enables bracketed paste and never draws that frame gets no initial input:
/// an older client's launch is refused and rolled back as for Claude.
#[tokio::test]
async fn pi_initial_input_is_not_sent_before_its_editor_frame_renders() {
    let socket = temp_socket("pi-no-frame");
    let agents_dir = socket.dir.join("agents");
    std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
    let received = socket.dir.join("received.bin");
    let script = socket.dir.join("pi-no-frame.py");
    std::fs::write(
        &script,
        format!(
            r#"import os
import termios
import tty

os.write(1, b"\x1b[?2004h")
tty.setraw(0, termios.TCSANOW)
with open({received:?}, "wb") as capture:
    while True:
        chunk = os.read(0, 4096)
        if not chunk:
            break
        capture.write(chunk)
        capture.flush()
"#,
            received = received.display().to_string(),
        ),
    )
    .expect("write Pi stub without editor frame");
    write_official_profile(&agents_dir, "pi", "pinoframe", &script);
    let config = SessionRegistryConfig {
        agents_dir: Some(agents_dir),
        host_state_dir: Some(socket.dir.join("host-state")),
        initial_input_startup_grace: UNRENDERED_PROMPT_GRACE,
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let mut params = session_params(&socket);
    "pinoframe".clone_into(&mut params.agent);
    params.input = Some("hello Pi".to_owned());
    let error = err_payload(create_session_with_params(&mut control, params).await);
    assert_eq!(error.code, "session_input_not_ready");

    let list = Request::make("pi-no-frame-list", method::SESSION_LIST, Value::Null);
    let sessions: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut control, &list).await))
            .expect("list sessions");
    assert!(sessions.is_empty(), "the refused session was rolled back");
    let captured = std::fs::read(&received).unwrap_or_default();
    assert!(
        captured.is_empty(),
        "the stub received input before its editor frame rendered"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Waited input through the public API under the given submit delays: the
/// daemon delivers body and Enter for a zero and for a nonzero delay alike.
///
/// With `delivered`, the capture is read once the fake agent has read the whole
/// input, which it may still be doing when the wait times out.
async fn waited_input_with_host_submit_delay(
    runtime: RuntimeRef,
    host_delay: Duration,
    override_delay: Option<Duration>,
    delivered: bool,
) -> (Response, Option<Vec<u8>>) {
    let socket = temp_socket("host-submit-delay");
    let bin_dir = socket.dir.join("bin");
    std::fs::create_dir(&bin_dir).expect("fake-agent bin directory");
    let agent_name = if runtime == RuntimeRef::claude() {
        "claude"
    } else {
        "codex"
    };
    let received = socket.dir.join("waited-input.bin");
    let ready = socket.dir.join("waited-input-ready");
    let script = socket.dir.join("waited-input.py");
    std::fs::write(
        &script,
        format!(
            r#"import os
import tty

tty.setraw(0)
body = bytearray()
# The capture exists before the ready marker, so a test that reads it as soon
# as the input was refused finds it empty rather than missing.
with open({received:?}, "wb") as capture:
    with open({ready:?}, "wb") as marker:
        marker.write(b"ready")
    while len(body) < len(b"hello\r"):
        chunk = os.read(0, len(b"hello\r") - len(body))
        body.extend(chunk)
        capture.write(chunk)
        capture.flush()
os.write(1, b"\x1b]2;Action Required\x07")
while True:
    os.read(0, 1)
"#,
            received = received.display().to_string(),
            ready = ready.display().to_string()
        ),
    )
    .expect("write waited-input script");
    write_executable(
        &bin_dir.join(agent_name),
        &official_runtimes::python_agent_launcher(agent_name, &script),
    );
    let mut config = support::hermetic_registry_config();
    config.host_submit_delay = Some(host_delay);
    if let Some(delay) = override_delay {
        config.submit_delay_overrides.insert(
            RuntimeId::parse(agent_name).expect("known runtime id"),
            delay,
        );
    }
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let created: SessionInfo = {
        let _path = PathGuard::prepend(&bin_dir);
        serde_json::from_value(ok_payload(
            create_session_with_agent(&mut control, runtime, socket.work_dir()).await,
        ))
        .expect("fake-agent session")
    };
    wait_until("fake waited-input agent is reading", || async {
        ready.exists().then_some(())
    })
    .await;
    let request = Request::make(
        "waited-host-input",
        method::SESSION_INPUT,
        serde_json::to_value(SessionInputParams {
            session_id: created.id.clone(),
            text: "hello".to_owned(),
            wait: Some(SessionInputWait {
                until: Some(vec![AgentActivity::Blocked]),
                timeout_ms: Some(2_000),
                allow_prompt_accepted: None,
            }),
        })
        .expect("waited input params"),
    );
    let response = exchange(&mut control, &request).await;
    if delivered {
        wait_until("fake waited-input agent read the whole input", || async {
            let captured = tokio::fs::read(&received).await.ok()?;
            (captured.len() >= b"hello\r".len()).then_some(())
        })
        .await;
    }
    let captured = tokio::fs::read(received).await.ok();
    let stop = Request::make(
        "stop-host-input",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize session id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop).await))
            .expect("stop fake agent");
    let _ = shutdown.send(());
    let _ = handle.await;
    (response, captured)
}

#[tokio::test]
async fn host_zero_submit_delay_passes_wait_framing_for_opted_in_claude() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let (response, captured) =
        waited_input_with_host_submit_delay(RuntimeRef::claude(), Duration::ZERO, None, true).await;
    let error = response
        .into_result()
        .expect_err("the fake agent leaves the activity wait incomplete");
    assert_eq!(error.code, "session_input_timeout");
    assert!(
        !error.msg.contains("Enter was written"),
        "a zero-delay profile settles in one plan: {}",
        error.msg
    );
    assert_eq!(captured, Some(b"hello\r".to_vec()));
}

#[tokio::test]
async fn host_zero_submit_delay_does_not_override_non_opted_in_codex() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let (response, captured) =
        waited_input_with_host_submit_delay(RuntimeRef::codex(), Duration::ZERO, None, true).await;
    let error = response
        .into_result()
        .expect_err("the fake agent leaves the activity wait incomplete");
    assert_eq!(error.code, "session_input_timeout");
    assert!(
        error.msg.contains("Enter was written"),
        "Codex keeps its delayed, daemon-paced submit: {}",
        error.msg
    );
    // Codex frames the body as a bracketed paste, so the fake agent's first
    // six bytes are the paste start.
    assert_eq!(captured, Some(b"\x1b[200~".to_vec()));
}

#[tokio::test]
async fn per_runtime_submit_delay_override_wins_over_host_zero() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let (response, captured) = waited_input_with_host_submit_delay(
        RuntimeRef::claude(),
        Duration::ZERO,
        Some(Duration::from_millis(40)),
        true,
    )
    .await;
    let error = response
        .into_result()
        .expect_err("the fake agent leaves the activity wait incomplete");
    assert_eq!(error.code, "session_input_timeout");
    assert!(
        error.msg.contains("Enter was written"),
        "the per-runtime delay wins over host zero: {}",
        error.msg
    );
    assert_eq!(captured, Some(b"hello\r".to_vec()));
}

#[tokio::test]
async fn session_survives_requesting_client_exit() {
    let socket = temp_socket("session-client-independence");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let created = {
        let mut first_client = connect(&socket).await;
        create_session(&mut first_client, &socket).await
    };

    let mut fresh_client = connect(&socket).await;
    let list_req = Request::make("session-list-fresh", method::SESSION_LIST, Value::Null);
    let list: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut fresh_client, &list_req).await))
            .expect("session list");
    assert!(
        list.iter()
            .any(|session| session.id == created.id && session.state == SessionState::Running),
        "fresh client should see daemon-owned session: {list:?}"
    );

    let stop_req = Request::make(
        "session-stop-fresh",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut fresh_client, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn session_exit_detection_reports_done_and_failed() {
    let success_socket = temp_socket("session-exit-success");
    let success_config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exit 0"]),
        ..SessionRegistryConfig::default()
    };
    let (success_shutdown, success_handle, _) =
        spawn_server_with_config(&success_socket, "0.0.0", success_config).await;
    let mut success_client = connect(&success_socket).await;
    let success = create_session(&mut success_client, &success_socket).await;
    let done = wait_for_state(&mut success_client, &success.id, SessionState::Done).await;
    assert_eq!(done.exit_code, Some(0));
    let _ = success_shutdown.send(());
    let _ = success_handle.await;

    let failure_socket = temp_socket("session-exit-failure");
    let failure_config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exit 7"]),
        ..SessionRegistryConfig::default()
    };
    let (failure_shutdown, failure_handle, _) =
        spawn_server_with_config(&failure_socket, "0.0.0", failure_config).await;
    let mut failure_client = connect(&failure_socket).await;
    let failure = create_session(&mut failure_client, &failure_socket).await;
    let failed = wait_for_state(&mut failure_client, &failure.id, SessionState::Failed).await;
    assert_eq!(failed.exit_code, Some(7));
    let _ = failure_shutdown.send(());
    let _ = failure_handle.await;
}

#[tokio::test]
async fn subscribe_streams_session_created_event() {
    let socket = temp_socket("subscribe-events");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    // A subscriber connection: send `subscribe`, expect an OK ack, then keep the
    // connection open to receive unsolicited event lines.
    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make("subscribe-1", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert_eq!(ack.id(), "subscribe-1");
    assert_eq!(ok_payload(ack)["subscribed"], Value::from(true));

    // A second, independent connection creates a session.
    let mut creator = connect(&socket).await;
    let created = create_session(&mut creator, &socket).await;

    // The subscriber must receive a `session_created` event for that session.
    // Bound the read so the test cannot hang if streaming is broken.
    let event_line = wait::guard("the session_created event", subscriber.next())
        .await
        .expect("a streamed event line")
        .expect("event framing ok");
    let streamed: Event = serde_json::from_str(&event_line).expect("parse event");

    assert_eq!(streamed.event(), event::SESSION_CREATED);
    assert_eq!(
        streamed.payload()["session"]["id"],
        Value::from(created.id.0.as_str()),
        "streamed event should carry the created session id"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn notification_api_crud_policy_methods_work() {
    let socket = temp_socket("notification-api");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;

    let created = create_notification(&mut control, notification_params(None)).await;
    assert!(created.created);
    assert_eq!(created.record.status, NotificationStatus::Unread);

    let listed = list_notifications(&mut control, NotificationListParams::default()).await;
    assert_eq!(listed.notifications.len(), 1);
    assert_eq!(listed.notifications[0].id, created.record.id);

    let read = update_notification(
        &mut control,
        NotificationUpdateParams {
            id: created.record.id.clone(),
            status: NotificationStatus::Read,
        },
    )
    .await;
    assert_eq!(read.record.status, NotificationStatus::Read);
    assert!(read.record.read_at.is_some());

    let got_policy = get_notification_policy(&mut control).await;
    assert!(got_policy.policy.enabled.agent_blocked);

    let replacement_policy = all_enabled_notification_policy();
    let set_policy = set_notification_policy(&mut control, replacement_policy.clone()).await;
    assert_eq!(set_policy.policy, replacement_policy);

    let deleted = delete_notification(
        &mut control,
        NotificationDeleteParams {
            id: created.record.id.clone(),
        },
    )
    .await;
    assert!(deleted.deleted);
    assert_eq!(deleted.id, created.record.id);
    let deleted_again = delete_notification(
        &mut control,
        NotificationDeleteParams {
            id: created.record.id.clone(),
        },
    )
    .await;
    assert!(
        !deleted_again.deleted,
        "notification.delete is idempotent for already-deleted records"
    );

    let default_list = list_notifications(&mut control, NotificationListParams::default()).await;
    assert!(
        default_list.notifications.is_empty(),
        "default list excludes deleted records"
    );
    let deleted_list = list_notifications(
        &mut control,
        NotificationListParams {
            status: Some(NotificationStatus::Deleted),
            ..NotificationListParams::default()
        },
    )
    .await;
    assert_eq!(deleted_list.notifications.len(), 1);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn notification_retention_prune_dry_run_and_apply_methods_work() {
    let socket = temp_socket("notification-retention");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;

    let read_record = create_notification(&mut control, notification_params(None)).await;
    let _ = update_notification(
        &mut control,
        NotificationUpdateParams {
            id: read_record.record.id.clone(),
            status: NotificationStatus::Read,
        },
    )
    .await;
    let retention_dry_run_req = Request::make(
        "notification-retention-dry-run",
        method::NOTIFICATION_RETENTION_PRUNE,
        serde_json::to_value(NotificationRetentionParams {
            dry_run: true,
            status: Some(NotificationStatus::Read),
            before: Some("2999-01-01T00:00:00Z".to_owned()),
            limit: None,
        })
        .expect("serialize retention params"),
    );
    let dry_run: NotificationRetentionResult = serde_json::from_value(ok_payload(
        exchange(&mut control, &retention_dry_run_req).await,
    ))
    .expect("notification.retention.prune dry-run result");
    assert!(dry_run.dry_run);
    assert_eq!(dry_run.pruned, vec![read_record.record.id.clone()]);

    let mut archived_params = notification_params(None);
    archived_params.source.host_local_source_id = "codex-hook-2".to_owned();
    archived_params.source_id = Some("permission-request-2".to_owned());
    archived_params.dedupe_key = Some("session:s-2:attention".to_owned());
    let archived_record = create_notification(&mut control, archived_params).await;
    let archived = update_notification(
        &mut control,
        NotificationUpdateParams {
            id: archived_record.record.id.clone(),
            status: NotificationStatus::Archived,
        },
    )
    .await;
    assert_eq!(archived.record.status, NotificationStatus::Archived);

    let retention_apply_req = Request::make(
        "notification-retention-apply",
        method::NOTIFICATION_RETENTION_PRUNE,
        serde_json::to_value(NotificationRetentionParams {
            dry_run: false,
            status: Some(NotificationStatus::Archived),
            before: Some("2999-01-01T00:00:00Z".to_owned()),
            limit: None,
        })
        .expect("serialize retention apply params"),
    );
    let applied: NotificationRetentionResult = serde_json::from_value(ok_payload(
        exchange(&mut control, &retention_apply_req).await,
    ))
    .expect("notification.retention.prune apply result");
    assert!(!applied.dry_run);
    assert_eq!(applied.pruned, vec![archived_record.record.id.clone()]);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn notification_update_returns_typed_errors_for_missing_invalid_and_malformed_requests() {
    let socket = temp_socket("notification-errors");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;

    let missing_req = Request::make(
        "notification-update-missing",
        method::NOTIFICATION_UPDATE,
        serde_json::to_value(NotificationUpdateParams {
            id: protocol::NotificationId("n-missing".to_owned()),
            status: NotificationStatus::Read,
        })
        .expect("serialize missing update"),
    );
    let missing = err_payload(exchange(&mut control, &missing_req).await);
    assert_eq!(missing.class, ErrorClass::Runtime);
    assert_eq!(missing.code, "notification_not_found");

    let created = create_notification(&mut control, notification_params(None)).await;
    let _ = update_notification(
        &mut control,
        NotificationUpdateParams {
            id: created.record.id.clone(),
            status: NotificationStatus::Read,
        },
    )
    .await;
    let invalid_req = Request::make(
        "notification-update-invalid-transition",
        method::NOTIFICATION_UPDATE,
        serde_json::to_value(NotificationUpdateParams {
            id: created.record.id.clone(),
            status: NotificationStatus::Unread,
        })
        .expect("serialize invalid transition"),
    );
    let invalid = err_payload(exchange(&mut control, &invalid_req).await);
    assert_eq!(invalid.class, ErrorClass::Runtime);
    assert_eq!(invalid.code, "invalid_notification_transition");

    let _ = delete_notification(
        &mut control,
        NotificationDeleteParams {
            id: created.record.id.clone(),
        },
    )
    .await;
    let update_deleted_req = Request::make(
        "notification-update-deleted",
        method::NOTIFICATION_UPDATE,
        serde_json::to_value(NotificationUpdateParams {
            id: created.record.id.clone(),
            status: NotificationStatus::Archived,
        })
        .expect("serialize deleted update"),
    );
    let update_deleted = err_payload(exchange(&mut control, &update_deleted_req).await);
    assert_eq!(update_deleted.code, "invalid_notification_transition");

    let malformed_list_req = Request::make(
        "notification-list-malformed",
        method::NOTIFICATION_LIST,
        serde_json::json!({ "created_after": "not-rfc3339" }),
    );
    let malformed = err_payload(exchange(&mut control, &malformed_list_req).await);
    assert_eq!(malformed.class, ErrorClass::Runtime);
    assert_eq!(malformed.code, "invalid_notification_timestamp");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn notification_create_enriches_live_session_context() {
    let socket = temp_socket("notification-session-context");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let session = create_session(&mut control, &socket).await;

    let created =
        create_notification(&mut control, notification_params(Some(session.id.clone()))).await;

    assert_eq!(created.record.session_id, Some(session.id));
    assert_eq!(created.record.agent_kind, Some(RuntimeRef::shell()));

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn notification_create_keeps_missing_session_reference() {
    let socket = temp_socket("notification-missing-session");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let missing = SessionId("s-missing".to_owned());

    let created =
        create_notification(&mut control, notification_params(Some(missing.clone()))).await;

    assert_eq!(created.record.session_id, Some(missing));
    assert_eq!(created.record.agent_kind, None);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn subscribe_streams_notification_created_event() {
    let socket = temp_socket("notification-created-event");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make(
        "subscribe-notification-created",
        method::SUBSCRIBE,
        Value::Null,
    );
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let created = create_notification(&mut control, notification_params(None)).await;
    let streamed = wait_for_notification_event(
        &mut subscriber,
        event::NOTIFICATION_CREATED,
        &created.record.id,
        Some(NotificationStatus::Unread),
    )
    .await;

    assert_eq!(
        streamed.payload()["record"]["title"],
        Value::from("Approval required")
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn subscribe_streams_notification_updated_events_for_read_ack_and_archive() {
    let socket = temp_socket("notification-updated-events");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let created = create_notification(&mut control, notification_params(None)).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make(
        "subscribe-notification-updated",
        method::SUBSCRIBE,
        Value::Null,
    );
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    for status in [
        NotificationStatus::Read,
        NotificationStatus::Acknowledged,
        NotificationStatus::Archived,
    ] {
        let updated = update_notification(
            &mut control,
            NotificationUpdateParams {
                id: created.record.id.clone(),
                status,
            },
        )
        .await;
        assert_eq!(updated.record.status, status);
        let streamed = wait_for_notification_event(
            &mut subscriber,
            event::NOTIFICATION_UPDATED,
            &created.record.id,
            Some(status),
        )
        .await;
        assert_eq!(
            streamed.payload()["record"]["id"],
            Value::from(created.record.id.0.as_str())
        );
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn subscribe_streams_notification_deleted_event() {
    let socket = temp_socket("notification-deleted-event");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let created = create_notification(&mut control, notification_params(None)).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make(
        "subscribe-notification-deleted",
        method::SUBSCRIBE,
        Value::Null,
    );
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let deleted = delete_notification(
        &mut control,
        NotificationDeleteParams {
            id: created.record.id.clone(),
        },
    )
    .await;
    assert!(deleted.deleted);
    let streamed = wait_for_notification_event(
        &mut subscriber,
        event::NOTIFICATION_DELETED,
        &created.record.id,
        None,
    )
    .await;

    assert_eq!(
        streamed.payload()["notification_id"],
        Value::from(created.record.id.0.as_str())
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn report_agent_api_records_active_agent_and_streams_report_state() {
    let socket = temp_socket("report-agent-api");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make("subscribe-report-agent", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let created = create_session(&mut control, &socket).await;
    let report_req = Request::make(
        "session-report-agent",
        method::SESSION_REPORT_AGENT,
        serde_json::to_value(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(ReportSequence::new(1)),
            pid: None,
            agent_session_id: None,
            agent_session_path: None,
            pid_start_identity: None,
            monotonic_sequence: None,
            retryable_admission: false,
        })
        .expect("serialize report-agent params"),
    );
    let result: SessionReportAgentResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &report_req).await))
            .expect("report-agent result");
    assert!(result.recorded);

    let streamed = wait_for_agent_state_event(
        &mut subscriber,
        &created.id,
        AgentActivity::Blocked,
        StateSource::Report,
    )
    .await;
    assert_eq!(streamed.payload()["source"], Value::from("report"));

    let stop_req = Request::make(
        "session-stop-report-agent",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn attach_raw_stream_round_trips_resizes_detaches_and_reattaches() {
    let socket = temp_socket("attach-roundtrip");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control, &socket).await;

    let attach = attach_session(&mut control, &created.id).await;
    assert!(!attach.stream_id.is_empty());

    let mut raw = connect_raw(&socket).await;
    let header = serde_json::to_string(&AttachHeader {
        attach: attach.stream_id.clone(),
    })
    .expect("serialize attach header");
    let mut attach_prelude_and_input = header.into_bytes();
    attach_prelude_and_input
        .extend_from_slice(b"\nprintf 'm4-leftover:%s\\n' '{\"looks\":\"json\"}'\n");
    raw.write_all(&attach_prelude_and_input)
        .await
        .expect("send attach header and input in one socket write");

    let output = read_until_marker(&mut raw, br#"m4-leftover:{"looks":"json"}"#).await;
    assert!(
        output
            .windows(br#"m4-leftover:{"looks":"json"}"#.len())
            .any(|window| window == br#"m4-leftover:{"looks":"json"}"#),
        "raw output should contain the JSON-looking marker: {}",
        String::from_utf8_lossy(&output)
    );

    raw.write_all(b"printf 'm4-bin:\\377END\\n'\n")
        .await
        .expect("send binary-safe marker command");
    let output = read_until_marker(&mut raw, b"m4-bin:\xffEND").await;
    assert!(
        output
            .windows(b"m4-bin:\xffEND".len())
            .any(|window| window == b"m4-bin:\xffEND"),
        "raw output should contain non-UTF-8/control bytes: {output:?}"
    );

    let resize = resize_session(&mut control, &created.id, 120, 40).await;
    assert_eq!(resize.session.cols, 120);
    assert_eq!(resize.session.rows, 40);
    let inspected = inspect_session(&mut control, &created.id).await;
    assert_eq!(inspected.cols, 120);
    assert_eq!(inspected.rows, 40);

    let detached = detach_stream(&mut control, &attach.stream_id).await;
    assert!(detached.detached);
    assert_raw_stream_closes(&mut raw).await;

    let survived = inspect_session(&mut control, &created.id).await;
    assert_eq!(survived.state, SessionState::Running);

    let reattach = attach_session(&mut control, &created.id).await;
    assert_ne!(reattach.stream_id, attach.stream_id);
    let mut raw_again = open_attach_stream(&socket, &reattach.stream_id).await;
    raw_again
        .write_all(b"printf 'm4-reattach\n'\n")
        .await
        .expect("send input after reattach");
    let output = read_until_marker(&mut raw_again, b"m4-reattach").await;
    assert!(
        output
            .windows(b"m4-reattach".len())
            .any(|window| window == b"m4-reattach"),
        "reattach stream should receive PTY output: {}",
        String::from_utf8_lossy(&output)
    );

    let stop_req = Request::make(
        "session-stop-after-attach",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");
    let stopped = inspect_session(&mut control, &created.id).await;
    assert_eq!(stopped.state, SessionState::Stopped);
    assert_eq!(stopped.activity, None);
    assert_eq!(stopped.state_source, StateSource::Process);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn reattach_starts_from_current_snapshot_after_historical_resizes() {
    let socket = temp_socket("attach-current-snapshot");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control, &socket).await;

    // Produce full-screen output at several historical geometries. Replaying
    // these bytes into a differently sized client is the regression: cursor
    // movement from the old grids corrupts the reconstructed screen.
    let first = attach_session(&mut control, &created.id).await;
    let mut raw = open_attach_stream(&socket, &first.stream_id).await;
    resize_session(&mut control, &created.id, 120, 40).await;
    raw.write_all(b"printf '\\033[2J\\033[H%s%s\\n' 'WIDE-HISTORICAL-' 'STATE'\n")
        .await
        .expect("send wide terminal state");
    let _ = read_until_marker(&mut raw, b"WIDE-HISTORICAL-STATE").await;
    resize_session(&mut control, &created.id, 60, 12).await;
    raw.write_all(b"printf '\\033[2J\\033[H%s%s\\n' 'FINAL-SNAPSHOT-' 'STATE'\n")
        .await
        .expect("send final terminal state");
    let live_output = read_until_marker(&mut raw, b"FINAL-SNAPSHOT-STATE").await;
    assert!(
        live_output
            .windows(b"FINAL-SNAPSHOT-STATE".len())
            .any(|window| window == b"FINAL-SNAPSHOT-STATE"),
        "first attach should receive live output: {}",
        String::from_utf8_lossy(&live_output)
    );

    // Detach the first stream and confirm it closes.
    let detached = detach_stream(&mut control, &first.stream_id).await;
    assert!(detached.detached);
    assert_raw_stream_closes(&mut raw).await;

    // A differently sized reattach starts from one repaint of current state,
    // never the retained raw history from incompatible geometries.
    let second = attach_session_with_dimensions(
        &mut control,
        &created.id,
        Some(TerminalDimensions::new(100, 30).expect("dimensions")),
    )
    .await;
    assert_ne!(second.stream_id, first.stream_id);
    let mut raw_again = open_attach_stream(&socket, &second.stream_id).await;
    let snapshot = read_until_marker(&mut raw_again, b"FINAL-SNAPSHOT-STATE").await;
    let resized = inspect_session(&mut control, &created.id).await;
    assert_eq!((resized.cols, resized.rows), (100, 30));
    assert!(
        snapshot
            .windows(b"\x1b[2J\x1b[H".len())
            .any(|window| window == b"\x1b[2J\x1b[H"),
        "fresh attach must begin with a full repaint: {}",
        String::from_utf8_lossy(&snapshot)
    );
    assert!(
        !snapshot
            .windows(b"WIDE-HISTORICAL-STATE".len())
            .any(|window| window == b"WIDE-HISTORICAL-STATE"),
        "fresh attach must not replay bytes from the historical wide grid: {}",
        String::from_utf8_lossy(&snapshot)
    );

    let stop_req = Request::make(
        "session-stop-after-current-snapshot",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn multiple_attach_clients_receive_output_and_disconnect_independently() {
    let socket = temp_socket("attach-multi");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut subscriber = subscribe_events(&socket).await;
    let mut control = connect(&socket).await;
    let created = create_session(&mut control, &socket).await;

    let first = attach_session(&mut control, &created.id).await;
    let second = attach_session(&mut control, &created.id).await;
    // Each stream is served by its own task, so the two opened events can
    // arrive in either order; the wait skips other events, so each stream is
    // confirmed open before the next one is opened.
    let mut raw_one = open_attach_stream(&socket, &first.stream_id).await;
    wait_for_attach_event(&mut subscriber, event::ATTACH_OPENED, &first.stream_id).await;
    let mut raw_two = open_attach_stream(&socket, &second.stream_id).await;
    wait_for_attach_event(&mut subscriber, event::ATTACH_OPENED, &second.stream_id).await;

    raw_one
        .write_all(b"printf 'm4-multi\n'\n")
        .await
        .expect("send first multi-client marker");

    let one_output = read_until_marker(&mut raw_one, b"m4-multi").await;
    let two_output = read_until_marker(&mut raw_two, b"m4-multi").await;
    assert!(one_output
        .windows(b"m4-multi".len())
        .any(|window| window == b"m4-multi"));
    assert!(two_output
        .windows(b"m4-multi".len())
        .any(|window| window == b"m4-multi"));

    drop(raw_one);
    raw_two
        .write_all(b"printf 'm4-still-attached\n'\n")
        .await
        .expect("send marker after dropping first attach");
    let two_output = read_until_marker(&mut raw_two, b"m4-still-attached").await;
    assert!(two_output
        .windows(b"m4-still-attached".len())
        .any(|window| window == b"m4-still-attached"));

    let stopped = Request::make(
        "session-stop-after-multi-attach",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stopped).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn dropping_an_attach_connection_detaches_without_stopping_the_session() {
    // Closing an attach window kills `pohunek attach`, which drops the attach
    // socket WITHOUT sending an explicit `session.detach` (the client installs no
    // SIGHUP handler). The daemon must treat the dropped stream as a detach and
    // keep the session running — the spec's top-risk guarantee (Slice D:
    // closing a window detaches, never stops; the deselected session stays live).
    let socket = temp_socket("attach-drop-detaches");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut subscriber = subscribe_events(&socket).await;
    let mut control = connect(&socket).await;
    let created = create_session(&mut control, &socket).await;

    // Attach, then drop the raw stream the way a closed window / SIGHUP would —
    // no `session.detach` is ever sent.
    let attach = attach_session(&mut control, &created.id).await;
    let raw = open_attach_stream(&socket, &attach.stream_id).await;
    wait_for_attach_event(&mut subscriber, event::ATTACH_OPENED, &attach.stream_id).await;
    drop(raw);
    // The daemon's attach bridge observes the EOF and deregisters the stream.
    wait_for_attach_event(&mut subscriber, event::ATTACH_CLOSED, &attach.stream_id).await;

    // The session must still be running and listed: a dropped attach is a detach,
    // not a stop.
    let inspected = inspect_session(&mut control, &created.id).await;
    assert_eq!(
        inspected.state,
        SessionState::Running,
        "dropping an attach connection must not stop the session"
    );
    let list_req = Request::make("list-after-attach-drop", method::SESSION_LIST, Value::Null);
    let list: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut control, &list_req).await))
            .expect("session list");
    assert!(
        list.iter()
            .any(|s| s.id == created.id && s.state == SessionState::Running),
        "session must still be listed running after the attach drop: {list:?}"
    );

    // And it remains attachable: re-attach and exchange a marker, proving the
    // same live session survived the window close.
    let reattach = attach_session(&mut control, &created.id).await;
    let mut raw2 = open_attach_stream(&socket, &reattach.stream_id).await;
    wait_for_attach_event(&mut subscriber, event::ATTACH_OPENED, &reattach.stream_id).await;
    raw2.write_all(b"printf 're-attached\n'\n")
        .await
        .expect("send marker after reattach");
    let output = read_until_marker(&mut raw2, b"re-attached").await;
    assert!(
        output
            .windows(b"re-attached".len())
            .any(|w| w == b"re-attached"),
        "re-attached stream should receive live output from the surviving session"
    );

    let stop_req = Request::make(
        "stop-after-attach-drop",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn detector_publishes_osc_title_activity_while_attach_receives_output() {
    let socket = temp_socket("detector-osc-attach");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            [
                "-c",
                "stty -echo; read trigger; printf '\\033]0;working\\007m5-detector-attach\\n'; sleep 30",
            ],
        ),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut subscriber = subscribe_events(&socket).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control, &socket).await;
    let attach = attach_session(&mut control, &created.id).await;
    let mut raw = open_attach_stream(&socket, &attach.stream_id).await;
    // The terminal buffers the trigger line until the shell reads it.
    wait_for_attach_event(&mut subscriber, event::ATTACH_OPENED, &attach.stream_id).await;
    raw.write_all(b"\n")
        .await
        .expect("trigger detector marker command");

    let raw_output = read_until_marker(&mut raw, b"m5-detector-attach").await;
    assert!(
        raw_output
            .windows(b"m5-detector-attach".len())
            .any(|window| window == b"m5-detector-attach"),
        "raw attach stream should receive shell output: {}",
        String::from_utf8_lossy(&raw_output)
    );

    let streamed = wait_for_agent_state_event(
        &mut subscriber,
        &created.id,
        AgentActivity::Working,
        StateSource::OscTitle,
    )
    .await;
    assert_eq!(streamed.payload()["activity"], Value::from("working"));
    assert_eq!(streamed.payload()["source"], Value::from("osc_title"));

    let inspected = inspect_session(&mut control, &created.id).await;
    assert_eq!(inspected.activity, Some(AgentActivity::Working));
    assert_eq!(inspected.state_source, StateSource::OscTitle);

    let stop_req = Request::make(
        "session-stop-after-detector",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");
    let stopped = inspect_session(&mut control, &created.id).await;
    assert_eq!(stopped.state, SessionState::Stopped);
    assert_eq!(stopped.activity, None);
    assert_eq!(stopped.state_source, StateSource::Process);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn detector_tick_publishes_debounced_static_osc_title_activity() {
    let socket = temp_socket("detector-osc-debounce");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            ["-c", "sleep 0.2; printf '\\033]0;blocked\\007'; sleep 30"],
        ),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make(
        "subscribe-detector-debounce",
        method::SUBSCRIBE,
        Value::Null,
    );
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let created = create_session(&mut control, &socket).await;

    let streamed = wait_for_agent_state_event(
        &mut subscriber,
        &created.id,
        AgentActivity::Blocked,
        StateSource::OscTitle,
    )
    .await;
    assert_eq!(streamed.payload()["activity"], Value::from("blocked"));
    assert_eq!(streamed.payload()["source"], Value::from("osc_title"));

    let inspected = inspect_session(&mut control, &created.id).await;
    assert_eq!(inspected.activity, Some(AgentActivity::Blocked));
    assert_eq!(inspected.state_source, StateSource::OscTitle);

    let stop_req = Request::make(
        "session-stop-after-detector-debounce",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
            .expect("stop result");

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Milestone-8 checkpoint: two sessions on one repository with different
/// branches get two distinct worktrees and each launches inside its own.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "The two-session worktree scenario is clearer as one sequence."
)]
async fn two_sessions_on_one_repo_get_distinct_worktrees() {
    let repo = init_git_repo();
    let worktree_root = temp_dir("two-wt-root");
    let store_path_dir = temp_dir("two-wt-store");
    let store_path = store_path_dir.join("metadata.jsonl");
    let socket = temp_socket("two-worktrees");

    let config = SessionRegistryConfig {
        // Each shell records its working directory into its own worktree, which
        // proves the process was launched *inside* the bound tree.
        shell_command: ShellCommand::new("/bin/sh", ["-c", "pwd > pohunek-pwd.txt; exec sleep 30"]),
        worktree_root: Some(worktree_root.to_path_buf()),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let alpha: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(
            &mut control,
            RuntimeRef::shell(),
            repo.cwd().to_path_buf(),
            "feature/alpha",
        )
        .await,
    ))
    .expect("alpha session info");
    let beta: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(
            &mut control,
            RuntimeRef::shell(),
            repo.cwd().to_path_buf(),
            "feature/beta",
        )
        .await,
    ))
    .expect("beta session info");

    let alpha_path = alpha.worktree_path.clone().expect("alpha worktree path");
    let beta_path = beta.worktree_path.clone().expect("beta worktree path");

    // Two distinct trees — no shared working tree.
    assert_ne!(
        alpha_path, beta_path,
        "two branches must not share a worktree"
    );
    assert!(alpha_path.starts_with(&worktree_root));
    assert!(beta_path.starts_with(&worktree_root));

    // Each session was launched in its own worktree (cwd == bound worktree).
    assert_eq!(alpha.cwd, alpha_path);
    assert_eq!(beta.cwd, beta_path);
    assert_eq!(alpha.branch.as_deref(), Some("feature/alpha"));
    assert_eq!(beta.branch.as_deref(), Some("feature/beta"));

    // Both are real git worktrees (a `.git` file pointer, not a directory).
    for path in [&alpha_path, &beta_path] {
        let git_pointer = std::fs::read_to_string(path.join(".git"))
            .unwrap_or_else(|err| panic!("read {}/.git: {err}", path.display()));
        assert!(
            git_pointer.trim_start().starts_with("gitdir:"),
            "{} must be a git worktree",
            path.display()
        );
    }

    // The shell in each worktree wrote its cwd there: the file lands in the
    // session's own tree, proving the process ran inside it.
    for path in [&alpha_path, &beta_path] {
        let marker = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("worktree dir name")
            .as_bytes()
            .to_vec();
        let recorded = read_file_until(&path.join("pohunek-pwd.txt"), &marker).await;
        assert!(
            recorded
                .windows(marker.len())
                .any(|w| w == marker.as_slice()),
            "pwd recorded in {} must contain the worktree dir name",
            path.display()
        );
    }

    // The unified metadata store persisted both worktree bindings.
    let bindings = std::fs::read_to_string(&store_path).expect("read metadata store");
    assert_eq!(
        bindings
            .lines()
            .filter(|l| l.contains("\"kind\":\"worktree\""))
            .count(),
        2,
        "two worktree bindings persisted: {bindings}"
    );

    for id in [&alpha.id, &beta.id] {
        let remove_req = Request::make(
            "session-remove-worktree",
            method::SESSION_REMOVE,
            serde_json::to_value(id).expect("serialize id"),
        );
        let removed: SessionRemoveResult =
            serde_json::from_value(ok_payload(exchange(&mut control, &remove_req).await))
                .expect("remove result");
        assert!(removed.removed);
    }
    assert!(
        !alpha_path.exists(),
        "session.remove removes its owned worktree"
    );
    assert!(
        !beta_path.exists(),
        "session.remove removes its owned worktree"
    );
    let cleaned_store = std::fs::read_to_string(&store_path).expect("read cleaned metadata store");
    assert!(
        !cleaned_store.contains("\"kind\":\"worktree\""),
        "session.remove drops exact worktree ownership bindings: {cleaned_store}"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Registers `repo` as a project and returns its raw `project.show` result.
async fn show_project_of(framed: &mut Framed<UnixStream, LinesCodec>, repo: &Path) -> Value {
    let add = Request::make(
        "owner-project-add",
        method::PROJECT_ADD,
        serde_json::json!({ "path": repo }),
    );
    let project: ProjectInfo =
        serde_json::from_value(ok_payload(exchange(framed, &add).await)).expect("project info");
    let show = Request::make(
        "owner-project-show",
        method::PROJECT_SHOW,
        serde_json::json!({ "reference": project.id }),
    );
    ok_payload(exchange(framed, &show).await)
}

/// Returns the typed worktree entry at `path` and its raw JSON object.
fn worktree_entry(shown: &Value, path: &Path) -> (ProjectWorktree, Value) {
    let canonical = path.canonicalize().expect("canonical worktree path");
    let typed: ProjectShowResult =
        serde_json::from_value(shown.clone()).expect("project show result");
    let index = typed
        .worktrees
        .iter()
        .position(|worktree| worktree.path == canonical || worktree.path == path)
        .unwrap_or_else(|| {
            panic!(
                "project.show lists the worktree {}: {shown}",
                path.display()
            )
        });
    (
        typed.worktrees[index].clone(),
        shown["worktrees"][index].clone(),
    )
}

/// `project.show` names the session a worktree was created for after that
/// session stops, while `session_id` names only a live session in it.
#[tokio::test]
async fn a_stopped_session_still_owns_its_worktree_in_project_show() {
    let repo = init_git_repo();
    let worktree_root = temp_dir("owner-wt-root");
    let store_path = temp_dir("owner-wt-store").join("metadata.jsonl");
    let socket = temp_socket("owner-worktree");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exec sleep 30"]),
        worktree_root: Some(worktree_root.to_path_buf()),
        store_path: Some(store_path),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let session: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(
            &mut control,
            RuntimeRef::shell(),
            repo.cwd().to_path_buf(),
            "feature/owner",
        )
        .await,
    ))
    .expect("session info");
    let path = session.worktree_path.clone().expect("worktree path");

    let (live, _) = worktree_entry(&show_project_of(&mut control, repo.cwd()).await, &path);
    assert!(live.owned, "pohunek owns the session's worktree: {live:?}");
    assert_eq!(
        live.owner_session_id.as_deref(),
        Some(session.id.0.as_str())
    );
    assert_eq!(live.session_id.as_deref(), Some(session.id.0.as_str()));

    stop_session(&mut control, &session.id).await;
    wait_for_state(&mut control, &session.id, SessionState::Stopped).await;

    let shown = show_project_of(&mut control, repo.cwd()).await;
    let (stopped, raw) = worktree_entry(&shown, &path);
    assert!(
        stopped.owned,
        "the stopped session still owns its worktree: {raw}"
    );
    assert_eq!(
        stopped.owner_session_id.as_deref(),
        Some(session.id.0.as_str()),
        "the owner survives the stop: {raw}"
    );
    assert!(
        raw.get("session_id").is_none(),
        "a worktree without a live session omits session_id: {raw}"
    );
    let unowned = shown["worktrees"]
        .as_array()
        .expect("worktrees array")
        .iter()
        .find(|entry| entry["owned"] == false)
        .unwrap_or_else(|| panic!("the repository's own checkout is listed: {shown}"));
    assert!(
        unowned.get("owner_session_id").is_none(),
        "an unowned worktree omits owner_session_id: {unowned}"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Script of the temporary-directory scenario: it fills `$TMPDIR` with what
/// agents leave there (a read-only and an unreadable directory, a hardlink, a
/// FIFO) and then
/// reports the value it saw in `tmpdir.txt` of its working directory.
const TMPDIR_SCRIPT: &str = concat!(
    "mkdir -p \"$TMPDIR/ro/inner\" && : > \"$TMPDIR/ro/inner/file\" && ",
    ": > \"$TMPDIR/data\" && ln \"$TMPDIR/data\" \"$TMPDIR/hardlink\" && ",
    "mkfifo \"$TMPDIR/fifo\" && chmod 0555 \"$TMPDIR/ro\" && ",
    "mkdir \"$TMPDIR/locked\" && : > \"$TMPDIR/locked/file\" && chmod 0000 \"$TMPDIR/locked\" && ",
    "printf '%s|' \"$TMPDIR\" > tmpdir.partial && mv tmpdir.partial tmpdir.txt; ",
    "exec sleep 30",
);

/// Starts a session of `agent` in `cwd` and returns it with the `TMPDIR` its
/// child saw.
async fn session_with_reported_tmpdir(
    framed: &mut Framed<UnixStream, LinesCodec>,
    agent: &str,
    cwd: &Path,
) -> (SessionInfo, PathBuf) {
    let params = SessionNewParams {
        agent: agent.to_owned(),
        ..session_params_in(cwd.to_path_buf())
    };
    let session: SessionInfo =
        serde_json::from_value(ok_payload(create_session_with_params(framed, params).await))
            .expect("session info");
    let report = read_file_until(&cwd.join("tmpdir.txt"), b"|").await;
    let reported = String::from_utf8(report).expect("TMPDIR is UTF-8");
    (session, PathBuf::from(reported.trim_end_matches('|')))
}

async fn remove_session(framed: &mut Framed<UnixStream, LinesCodec>, id: &SessionId) {
    let request = Request::make(
        "session-remove-tmpdir",
        method::SESSION_REMOVE,
        serde_json::to_value(id).expect("serialize id"),
    );
    let removed: SessionRemoveResult =
        serde_json::from_value(ok_payload(exchange(framed, &request).await))
            .expect("remove result");
    assert!(removed.removed);
}

/// Each session gets its own private `TMPDIR`, kept across a stop and deleted
/// with everything an agent left in it when the session is removed. A profile
/// that sets `TMPDIR` keeps its own value.
#[tokio::test]
async fn every_session_gets_a_private_tmpdir_that_its_removal_deletes() {
    use std::os::unix::fs::PermissionsExt;

    let socket = temp_socket("session-tmpdir");
    let tmp_root = socket.dir.join("cache").join("session-tmp");
    let profile_tmpdir = temp_dir("profile-tmpdir");
    let agents = socket.dir.join("agents");
    std::fs::create_dir(&agents).expect("create agent profiles");
    std::fs::write(
        agents.join("own-tmpdir.toml"),
        format!(
            "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", {script:?}]\n\n[env]\nTMPDIR = {dir:?}\n",
            script = TMPDIR_SCRIPT,
            dir = profile_tmpdir.display().to_string(),
        ),
    )
    .expect("write the profile with its own TMPDIR");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", TMPDIR_SCRIPT]),
        session_tmp_root: Some(tmp_root.clone()),
        agents_dir: Some(agents),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;

    let cwds: Vec<_> = ["alpha", "beta", "profile"]
        .into_iter()
        .map(|name| {
            let cwd = socket.dir.join(name);
            std::fs::create_dir(&cwd).expect("create the session's working directory");
            cwd
        })
        .collect();
    let (alpha, alpha_tmp) = session_with_reported_tmpdir(&mut control, "shell", &cwds[0]).await;
    let (beta, beta_tmp) = session_with_reported_tmpdir(&mut control, "shell", &cwds[1]).await;
    let (own, own_tmp) = session_with_reported_tmpdir(&mut control, "own-tmpdir", &cwds[2]).await;

    assert_eq!(alpha_tmp, tmp_root.join(&alpha.id.0));
    assert_eq!(beta_tmp, tmp_root.join(&beta.id.0));
    for dir in [&alpha_tmp, &beta_tmp] {
        let metadata = std::fs::symlink_metadata(dir).expect("the session's TMPDIR exists");
        assert!(metadata.is_dir());
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            0o700,
            "{}",
            dir.display()
        );
    }
    assert_eq!(
        own_tmp,
        profile_tmpdir.to_path_buf(),
        "the profile's TMPDIR wins"
    );
    assert!(
        !tmp_root.join(&own.id.0).exists(),
        "a session with the profile's TMPDIR gets no private directory"
    );

    stop_session(&mut control, &beta.id).await;
    wait_for_state(&mut control, &beta.id, SessionState::Stopped).await;
    assert!(
        beta_tmp.join("data").exists(),
        "a stop keeps the session's TMPDIR"
    );

    for session in [&alpha, &beta, &own] {
        remove_session(&mut control, &session.id).await;
    }
    assert!(
        !alpha_tmp.exists(),
        "the removal deletes the session's TMPDIR"
    );
    assert!(
        !beta_tmp.exists(),
        "the removal deletes the stopped session's TMPDIR"
    );
    let left: Vec<_> = std::fs::read_dir(&tmp_root)
        .expect("list the TMPDIR root")
        .map(|entry| entry.expect("TMPDIR root entry").file_name())
        .collect();
    assert!(
        left.is_empty(),
        "nothing stays below the TMPDIR root: {left:?}"
    );
    assert!(
        profile_tmpdir.join("data").exists(),
        "the profile's own TMPDIR is never deleted"
    );
    for dir in ["ro", "locked"] {
        std::fs::set_permissions(
            profile_tmpdir.join(dir),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("let the fixture delete the profile's TMPDIR");
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Daemon startup deletes the private `TMPDIR` of every session without a
/// record and the quarantined leftovers of an interrupted deletion, keeps the
/// directory of a stopped session, and leaves an entry it cannot delete, such
/// as a symlink, in place without following it or failing the startup.
#[tokio::test]
async fn startup_deletes_the_tmpdirs_of_sessions_without_a_record() {
    use std::os::unix::fs::PermissionsExt;

    const SCRIPT: &str = concat!(
        ": > \"$TMPDIR/data\" && ",
        "printf '%s|' \"$TMPDIR\" > tmpdir.partial && mv tmpdir.partial tmpdir.txt; ",
        "exec sleep 30",
    );
    let socket = temp_socket("session-tmpdir-orphans");
    let tmp_root = socket.dir.join("cache").join("session-tmp");
    let store_path = socket.dir.join("metadata.jsonl");
    let outside = temp_dir("orphan-link-target");
    std::fs::write(outside.join("kept"), b"").expect("write the symlink target's file");
    let config = || SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", SCRIPT]),
        session_tmp_root: Some(tmp_root.clone()),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };

    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config()).await;
    let mut control = connect(&socket).await;
    let cwd = socket.dir.join("stopped");
    std::fs::create_dir(&cwd).expect("create the session's working directory");
    let (stopped, stopped_tmp) = session_with_reported_tmpdir(&mut control, "shell", &cwd).await;
    stop_session(&mut control, &stopped.id).await;
    wait_for_state(&mut control, &stopped.id, SessionState::Stopped).await;
    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;

    let orphan = tmp_root.join("orphaned-session");
    std::fs::create_dir_all(orphan.join("ro").join("inner")).expect("create the orphan's tree");
    std::fs::write(orphan.join("ro").join("inner").join("file"), b"").expect("orphan file");
    std::fs::set_permissions(orphan.join("ro"), std::fs::Permissions::from_mode(0o555))
        .expect("make the orphan's directory read-only");
    let leftover = tmp_root.join(".pohunek-session-tmp-leftover");
    std::fs::create_dir(&leftover).expect("create the quarantined leftover");
    std::fs::write(leftover.join("file"), b"").expect("leftover file");
    let link = tmp_root.join("linked");
    std::os::unix::fs::symlink(&*outside, &link).expect("link out of the TMPDIR root");

    let (restarted, _worker_home) = worker_backed_registry(&socket, config());
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciles despite an entry it cannot delete");

    assert!(!orphan.exists(), "the orphan's TMPDIR is deleted");
    assert!(!leftover.exists(), "the quarantined leftover is deleted");
    assert!(
        stopped_tmp.join("data").exists(),
        "a stopped session keeps its TMPDIR"
    );
    assert!(
        std::fs::symlink_metadata(&link).is_ok_and(|metadata| metadata.is_symlink()),
        "an entry that is not a directory stays"
    );
    assert!(
        outside.join("kept").exists(),
        "the symlink's target is never followed"
    );
    assert_eq!(
        restarted
            .inspect(&stopped.id)
            .await
            .expect("the stopped session is still listed")
            .state,
        SessionState::Stopped
    );
}

/// Environment flag that marks the re-executed child of the wide-tree scenario.
const WIDE_TMPDIR_CHILD: &str = "POHUNEK_HEALTH_SOCKET_WIDE_TMPDIR_CHILD";
/// Descriptor limit the wide-tree scenario runs its daemon under.
const WIDE_TMPDIR_FD_LIMIT: u64 = 256;
/// Directories in the read-only directory of the wide-tree scenario, more
/// than [`WIDE_TMPDIR_FD_LIMIT`].
const WIDE_TMPDIR_ENTRIES: usize = 600;

/// A read-only directory with more subdirectories than the daemon may hold
/// descriptors is still deleted with the session, because the cleanup holds
/// descriptors bounded by the tree's depth.
///
/// The scenario re-executes itself so the lowered descriptor limit stays in
/// that child process.
#[tokio::test]
async fn a_wide_read_only_tmpdir_is_deleted_under_a_low_descriptor_limit() {
    use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};

    const TEST_NAME: &str = "a_wide_read_only_tmpdir_is_deleted_under_a_low_descriptor_limit";
    if std::env::var_os(WIDE_TMPDIR_CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(pohunek_test_support::exact_test_filter(
                module_path!(),
                TEST_NAME,
            ))
            .arg("--nocapture")
            .env(WIDE_TMPDIR_CHILD, "1")
            .output()
            .expect("run the re-executed child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "child failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("wide-tmpdir-ok"),
            "child did not run its body:\n{stdout}"
        );
        return;
    }

    let socket = temp_socket("wide-tmpdir");
    let tmp_root = socket.dir.join("cache").join("session-tmp");
    let script = format!(
        concat!(
            "mkdir \"$TMPDIR/wide\" && i=0 && ",
            "while [ $i -lt {entries} ]; do mkdir \"$TMPDIR/wide/d$i\" || exit 1; i=$((i+1)); done && ",
            "chmod 0555 \"$TMPDIR/wide\" && ",
            "printf '%s|' \"$TMPDIR\" > tmpdir.partial && mv tmpdir.partial tmpdir.txt; ",
            "exec sleep 30",
        ),
        entries = WIDE_TMPDIR_ENTRIES,
    );
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        session_tmp_root: Some(tmp_root.clone()),
        ..support::hermetic_registry_config()
    };
    let hard = getrlimit(Resource::Nofile).maximum;
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(WIDE_TMPDIR_FD_LIMIT),
            maximum: hard,
        },
    )
    .expect("lower the descriptor limit");
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = socket.dir.join("wide-cwd");
    std::fs::create_dir(&cwd).expect("create the session's working directory");
    let (session, tmpdir) = session_with_reported_tmpdir(&mut control, "shell", &cwd).await;
    assert_eq!(tmpdir, tmp_root.join(&session.id.0));

    remove_session(&mut control, &session.id).await;
    assert!(
        !tmpdir.exists(),
        "the removal deletes the wide read-only tree"
    );
    let left: Vec<_> = std::fs::read_dir(&tmp_root)
        .expect("list the TMPDIR root")
        .map(|entry| entry.expect("TMPDIR root entry").file_name())
        .collect();
    assert!(
        left.is_empty(),
        "nothing stays below the TMPDIR root: {left:?}"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
    println!("wide-tmpdir-ok");
}

/// A private `TMPDIR` path that is not UTF-8 cannot reach the agent, so the
/// launch is refused before any worker starts instead of exporting a changed
/// path.
#[tokio::test]
async fn a_tmpdir_root_that_is_not_utf8_refuses_the_launch() {
    use std::os::unix::ffi::OsStrExt;

    let socket = temp_socket("session-tmpdir-bytes");
    let tmp_root = socket
        .dir
        .join(std::ffi::OsStr::from_bytes(b"session-tmp-\xff"));
    let started = socket.dir.join("agent-started");
    let script = format!("touch '{}'; exec sleep 30", started.display());
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        session_tmp_root: Some(tmp_root.clone()),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;

    let error =
        err_payload(create_session_with_params(&mut control, session_params(&socket)).await);
    assert_eq!(error.code, "session_tmpdir_failed", "{error}");
    assert_eq!(error.class, ErrorClass::Runtime);
    assert!(!started.exists(), "the agent never starts");
    assert!(
        !tmp_root.exists(),
        "nothing is created below the refused root"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The directory holding the stores' `TMPDIR` roots must be private like the
/// roots themselves: a group-readable one refuses the launch before any worker
/// starts.
#[tokio::test]
async fn a_group_readable_directory_of_tmpdir_roots_refuses_the_launch() {
    use std::os::unix::fs::PermissionsExt as _;

    let socket = temp_socket("session-tmpdir-roots-mode");
    let store_roots = socket.dir.join("session-tmp");
    std::fs::create_dir(&store_roots).expect("create the directory of store roots");
    std::fs::set_permissions(&store_roots, std::fs::Permissions::from_mode(0o750))
        .expect("make it group-readable");
    let started = socket.dir.join("agent-started");
    let script = format!("touch '{}'; exec sleep 30", started.display());
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        session_tmp_root: Some(store_roots.join("0123456789abcdef")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;

    let error =
        err_payload(create_session_with_params(&mut control, session_params(&socket)).await);
    assert_eq!(error.code, "session_tmpdir_unsafe", "{error}");
    assert_eq!(error.class, ErrorClass::Runtime);
    assert!(!started.exists(), "the agent never starts");
    assert_eq!(
        std::fs::read_dir(&store_roots)
            .expect("list the directory of store roots")
            .count(),
        0,
        "no store root is created below it"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A `TMPDIR` root reached through a symlink is refused before any worker
/// starts, and no session is left behind.
#[tokio::test]
async fn a_symlinked_tmpdir_root_refuses_the_launch() {
    let socket = temp_socket("session-tmpdir-symlink");
    let elsewhere = socket.dir.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("create the symlink target");
    let tmp_root = socket.dir.join("session-tmp");
    std::os::unix::fs::symlink(&elsewhere, &tmp_root).expect("plant the symlinked root");
    let started = socket.dir.join("agent-started");
    let script = format!("touch '{}'; exec sleep 30", started.display());
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        session_tmp_root: Some(tmp_root),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;

    let error =
        err_payload(create_session_with_params(&mut control, session_params(&socket)).await);
    assert_eq!(error.code, "session_tmpdir_unsafe", "{error}");
    assert_eq!(error.class, ErrorClass::Runtime);
    let list = Request::make("tmpdir-symlink-list", method::SESSION_LIST, Value::Null);
    let sessions: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut control, &list).await))
            .expect("list sessions");
    assert!(
        sessions.is_empty(),
        "the refused session is not listed: {sessions:?}"
    );
    assert!(!started.exists(), "the agent never starts");
    assert_eq!(
        std::fs::read_dir(&elsewhere)
            .expect("list the symlink target")
            .count(),
        0,
        "nothing is created through the symlink"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Writes a host-global `post-create` hook into `config_dir` and returns the
/// config directory for [`SessionRegistryConfig::config_dir`].
fn host_post_create_hook(socket: &TestSocket, body: &str) -> PathBuf {
    let config_dir = socket.dir.join("config");
    std::fs::create_dir_all(config_dir.join("hooks")).expect("create host hook directory");
    write_executable(&config_dir.join("hooks/post-create"), body);
    config_dir
}

/// A failing `post-create` hook is a non-fatal `hook` warning: the session is
/// created, its worktree is kept, and its agent runs inside that worktree.
#[tokio::test]
async fn a_failing_post_create_hook_keeps_the_session_in_its_worktree() {
    let repo = init_git_repo();
    let worktree_root = temp_dir("hook-fail-wt-root");
    let socket = temp_socket("hook-fail-worktree");
    let config_dir = host_post_create_hook(&socket, "#!/bin/sh\nexit 1\n");
    let store_path = temp_dir("hook-fail-store").join("metadata.jsonl");
    let pwd_file = socket.dir.join("agent-pwd.txt");
    let script = format!("pwd -P > '{}'; exec sleep 30", pwd_file.display());
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        worktree_root: Some(worktree_root.to_path_buf()),
        store_path: Some(store_path),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(
            &mut control,
            RuntimeRef::shell(),
            repo.cwd().to_path_buf(),
            "b",
        )
        .await,
    ))
    .expect("session info");
    let worktree = created.worktree_path.clone().expect("worktree bound");
    assert_eq!(created.cwd, worktree);
    assert_eq!(created.branch.as_deref(), Some("b"));

    let [warning] = created.warnings.as_slice() else {
        panic!("exactly one hook warning expected: {:?}", created.warnings);
    };
    assert_eq!(warning.kind, SessionWarningKind::Hook);
    assert!(
        warning.message.contains("post-create"),
        "warning names the failing event: {}",
        warning.message
    );

    // The agent reports its own working directory. Comparing against the
    // canonical path keeps the check valid where `/var` is a symlink to `/private/var`.
    let expected = std::fs::canonicalize(&worktree).expect("canonical worktree path");
    let recorded = read_file_until(&pwd_file, expected.to_string_lossy().as_bytes()).await;
    assert_eq!(
        String::from_utf8_lossy(&recorded).trim_end(),
        expected.to_string_lossy(),
        "the agent ran inside its worktree"
    );

    let remove_req = Request::make(
        "hook-fail-remove",
        method::SESSION_REMOVE,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let removed: SessionRemoveResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &remove_req).await))
            .expect("remove result");
    assert!(removed.removed);

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Writes executable host-global hooks named in `hooks` into `config_dir` and
/// returns the config directory for [`SessionRegistryConfig::config_dir`].
fn host_hooks(socket: &TestSocket, hooks: &[(&str, &str)]) -> PathBuf {
    let config_dir = socket.dir.join("config");
    std::fs::create_dir_all(config_dir.join("hooks")).expect("create host hook directory");
    for (name, body) in hooks {
        write_executable(&config_dir.join("hooks").join(name), body);
    }
    config_dir
}

/// Creates a worktree session on a fresh repository, removes it, and returns
/// the `session.remove` result. `hooks` are the host hooks installed first.
async fn remove_worktree_session_with_hooks(
    label: &str,
    hooks: impl FnOnce(&TestSocket) -> Vec<(&'static str, String)>,
) -> (SessionRemoveResult, TestSocket, PathBuf) {
    let repo = init_git_repo();
    let worktree_root = temp_dir(&format!("{label}-wt-root"));
    let socket = temp_socket(label);
    let hook_bodies = hooks(&socket);
    let hook_refs = hook_bodies
        .iter()
        .map(|(name, body)| (*name, body.as_str()))
        .collect::<Vec<_>>();
    let config_dir = host_hooks(&socket, &hook_refs);
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exec sleep 30"]),
        worktree_root: Some(worktree_root.to_path_buf()),
        store_path: Some(temp_dir(&format!("{label}-store")).join("metadata.jsonl")),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(
            &mut control,
            RuntimeRef::shell(),
            repo.cwd().to_path_buf(),
            "b",
        )
        .await,
    ))
    .expect("session info");
    let worktree = created.worktree_path.clone().expect("worktree bound");
    let remove = Request::make(
        &format!("{label}-remove"),
        method::SESSION_REMOVE,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let removed: SessionRemoveResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &remove).await))
            .expect("remove result");
    let _ = shutdown.send(());
    let _ = handle.await;
    (removed, socket, worktree)
}

/// `pre-remove` runs inside the worktree before it is deleted and `post-remove`
/// runs after it is gone; a clean removal reports no warnings.
#[tokio::test]
async fn session_remove_runs_pre_remove_in_the_worktree_and_reports_a_clean_result() {
    let (removed, socket, worktree) = remove_worktree_session_with_hooks("rm-hooks-ok", |socket| {
        let pre = socket.dir.join("pre-remove-ran");
        let post = socket.dir.join("post-remove-ran");
        vec![
            (
                "pre-remove",
                format!("#!/bin/sh\npwd -P > '{}'\n", pre.display()),
            ),
            (
                "post-remove",
                format!("#!/bin/sh\npwd -P > '{}'\n", post.display()),
            ),
        ]
    })
    .await;

    assert!(removed.removed);
    assert_eq!(removed.worktrees_removed, 1);
    assert!(removed.warnings.is_empty(), "{:?}", removed.warnings);
    assert!(!worktree.exists(), "the worktree is deleted");
    let pre = std::fs::read_to_string(socket.dir.join("pre-remove-ran")).expect("pre-remove ran");
    assert!(
        pre.trim_end().ends_with(
            worktree
                .file_name()
                .and_then(|name| name.to_str())
                .expect("worktree directory name")
        ),
        "pre-remove ran inside the worktree: {pre}"
    );
    assert!(
        socket.dir.join("post-remove-ran").exists(),
        "post-remove ran after the worktree was removed"
    );
}

/// A failing `pre-remove` hook does not block the removal: the result carries a
/// `hook` warning that names the event, since the session record is gone.
#[tokio::test]
async fn a_failing_pre_remove_hook_is_reported_in_the_session_remove_result() {
    let (removed, _socket, worktree) = remove_worktree_session_with_hooks("rm-hooks-fail", |_| {
        vec![("pre-remove", "#!/bin/sh\nexit 1\n".to_owned())]
    })
    .await;

    assert!(removed.removed);
    assert_eq!(removed.worktrees_removed, 1);
    assert!(
        !worktree.exists(),
        "the failed hook does not keep the worktree"
    );
    let [warning] = removed.warnings.as_slice() else {
        panic!("exactly one hook warning expected: {:?}", removed.warnings);
    };
    assert_eq!(warning.kind, SessionWarningKind::Hook);
    assert!(
        warning.message.contains("pre-remove"),
        "warning names the failing event: {}",
        warning.message
    );
}

/// Bytes the chatty failing hook writes to stderr before its last line, far
/// more than the default tail and more than one pipe buffer, so the capture
/// must both drain concurrently and cut.
const CHATTY_HOOK_STDERR_BYTES: usize = 200 * 1024;

/// A failing `post-create` hook's stderr tail reaches the `session.new` warning,
/// cut to the configured size, marked, and stripped of control characters, while
/// its stdout never does; the stored session keeps the warning without the tail.
#[tokio::test]
async fn a_failing_hook_reports_its_stderr_tail_only_in_the_session_new_result() {
    let repo = init_git_repo();
    let worktree_root = temp_dir("hook-tail-wt-root");
    let socket = temp_socket("hook-tail");
    let config_dir = host_post_create_hook(
        &socket,
        &format!(
            "#!/bin/sh\n\
             echo 'stdout is never reported'\n\
             head -c {CHATTY_HOOK_STDERR_BYTES} /dev/zero | tr '\\0' 'x' >&2\n\
             printf '\\n\\033[31mFINAL ERROR: npm install failed\\033[0m\\r\\n' >&2\n\
             exit 7\n"
        ),
    );
    let store_path = temp_dir("hook-tail-store").join("metadata.jsonl");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exec sleep 30"]),
        worktree_root: Some(worktree_root.to_path_buf()),
        store_path: Some(store_path),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    };
    let tail_bytes = usize::try_from(config.hook_stderr_tail_bytes).expect("tail size");
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(
            &mut control,
            RuntimeRef::shell(),
            repo.cwd().to_path_buf(),
            "b",
        )
        .await,
    ))
    .expect("session info");
    let [warning] = created.warnings.as_slice() else {
        panic!("exactly one hook warning expected: {:?}", created.warnings);
    };
    assert_eq!(warning.kind, SessionWarningKind::Hook);
    let detail = warning.detail.as_deref().expect("hook warning detail");
    let (head, tail) = detail
        .split_once("; stderr tail:\n")
        .unwrap_or_else(|| panic!("the detail carries a stderr tail: {detail}"));
    assert!(head.contains("exited with status"), "{head}");
    assert!(
        tail.starts_with('[') && tail.contains("earlier bytes omitted]\n"),
        "the cut is marked: {}",
        &tail[..tail.len().min(80)]
    );
    assert!(
        tail.ends_with("FINAL ERROR: npm install failed"),
        "the tail ends with the hook's last line, without its escape sequences: {}",
        &tail[tail.len().saturating_sub(80)..]
    );
    assert!(
        !tail
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "control characters other than LF and TAB are stripped"
    );
    assert!(!detail.contains("stdout is never reported"));
    let (_marker, text) = tail.split_once('\n').expect("marker line");
    assert!(
        text.len() <= tail_bytes,
        "the tail is bounded by the configured size: {} > {tail_bytes}",
        text.len()
    );

    let list = Request::make("hook-tail-list", method::SESSION_LIST, Value::Null);
    let sessions: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut control, &list).await))
            .expect("list sessions");
    let listed = sessions
        .iter()
        .find(|session| session.id == created.id)
        .expect("the session is listed");
    let [stored] = listed.warnings.as_slice() else {
        panic!("the listed session keeps its hook warning: {listed:?}");
    };
    assert_eq!(
        stored.detail.as_deref(),
        Some(head),
        "the stored warning has no tail"
    );

    let remove_req = Request::make(
        "hook-tail-remove",
        method::SESSION_REMOVE,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let removed: SessionRemoveResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &remove_req).await))
            .expect("remove result");
    assert!(removed.removed);

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// How long the descendant a failing hook leaves behind keeps the hook's stderr
/// open. It is still alive when `session.new` returns unless the create waited
/// for the pipe's EOF; the test kills it at the end.
const HOOK_DESCENDANT_HOLD: Duration = Duration::from_secs(600);

/// A failing hook whose background descendants keep its stderr open does not
/// hold up `session.new`, and a descendant that writes to that stderr after the
/// hook exited keeps running while the daemon does.
#[tokio::test]
async fn a_hook_descendant_holding_stderr_neither_delays_the_create_nor_dies() {
    let repo = init_git_repo();
    let worktree_root = temp_dir("hook-hold-wt-root");
    let socket = temp_socket("hook-hold");
    let late_writer_done = socket.dir.join("late-writer-done");
    let holder_pid = socket.dir.join("holder.pid");
    let config_dir = host_post_create_hook(
        &socket,
        &format!(
            "#!/bin/sh\n\
             sleep {hold} &\n\
             echo $! > '{holder}'\n\
             (sleep 2; echo late >&2; touch '{done}') &\n\
             echo 'setup failed' >&2\n\
             exit 3\n",
            hold = HOOK_DESCENDANT_HOLD.as_secs(),
            holder = holder_pid.display(),
            done = late_writer_done.display(),
        ),
    );
    let store_path = temp_dir("hook-hold-store").join("metadata.jsonl");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exec sleep 30"]),
        worktree_root: Some(worktree_root.to_path_buf()),
        store_path: Some(store_path),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(
            &mut control,
            RuntimeRef::shell(),
            repo.cwd().to_path_buf(),
            "b",
        )
        .await,
    ))
    .expect("session info");
    // The holder keeps the hook's stderr open until it exits, so a create that
    // waited for EOF would return only after it is gone.
    let holder = std::fs::read_to_string(&holder_pid)
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw)
        .expect("the hook recorded its background holder");
    assert!(
        rustix::process::test_kill_process(holder).is_ok(),
        "session.new returned while the hook's descendant still holds its stderr"
    );
    let [warning] = created.warnings.as_slice() else {
        panic!("exactly one hook warning expected: {:?}", created.warnings);
    };
    let detail = warning.detail.as_deref().expect("hook warning detail");
    assert!(
        detail.ends_with("; stderr tail:\nsetup failed"),
        "the tail holds what the hook printed before it exited: {detail}"
    );

    // Without a reader the late write would raise SIGPIPE and the subshell
    // would die before touching its marker.
    wait_until("the late stderr writer finishes", || {
        let done = late_writer_done.exists();
        async move { done.then_some(()) }
    })
    .await;

    let _ = rustix::process::kill_process(holder, rustix::process::Signal::KILL);
    let remove_req = Request::make(
        "hook-hold-remove",
        method::SESSION_REMOVE,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let removed: SessionRemoveResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &remove_req).await))
            .expect("remove result");
    assert!(removed.removed);

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// `hooks.stderr_tail_bytes = 0` keeps a failing hook's stderr out of the
/// warning entirely.
#[tokio::test]
async fn a_zero_stderr_tail_size_reports_no_hook_output() {
    let repo = init_git_repo();
    let worktree_root = temp_dir("hook-notail-wt-root");
    let socket = temp_socket("hook-notail");
    let config_dir = host_post_create_hook(&socket, "#!/bin/sh\necho 'secret-ish' >&2\nexit 1\n");
    let store_path = temp_dir("hook-notail-store").join("metadata.jsonl");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exec sleep 30"]),
        worktree_root: Some(worktree_root.to_path_buf()),
        store_path: Some(store_path),
        config_dir: Some(config_dir),
        hook_stderr_tail_bytes: 0,
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(
            &mut control,
            RuntimeRef::shell(),
            repo.cwd().to_path_buf(),
            "b",
        )
        .await,
    ))
    .expect("session info");
    let [warning] = created.warnings.as_slice() else {
        panic!("exactly one hook warning expected: {:?}", created.warnings);
    };
    let detail = warning.detail.as_deref().expect("hook warning detail");
    assert!(
        !detail.contains("stderr tail") && !detail.contains("secret-ish"),
        "{detail}"
    );

    let remove_req = Request::make(
        "hook-notail-remove",
        method::SESSION_REMOVE,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let removed: SessionRemoveResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &remove_req).await))
            .expect("remove result");
    assert!(removed.removed);

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A worktree that disappears between its bind and the agent spawn fails the
/// create with `session_cwd_missing`: no session is left behind, no worktree
/// binding is kept, and the agent never starts (in `$HOME` or anywhere else).
#[tokio::test]
async fn a_worktree_removed_before_the_agent_starts_fails_closed() {
    let repo = init_git_repo();
    let worktree_root = temp_dir("cwd-missing-wt-root");
    let store_path_dir = temp_dir("cwd-missing-store");
    let store_path = store_path_dir.join("metadata.jsonl");
    let socket = temp_socket("cwd-missing-worktree");
    // The hook deletes the worktree it runs in, reproducing a worktree that
    // vanishes between the bind and the spawn.
    let config_dir = host_post_create_hook(&socket, "#!/bin/sh\nrm -rf \"$(pwd -P)\"\nexit 0\n");
    let started = socket.dir.join("agent-started");
    let script = format!("touch '{}'; exec sleep 30", started.display());
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        worktree_root: Some(worktree_root.to_path_buf()),
        store_path: Some(store_path.clone()),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let error = err_payload(
        create_worktree_session(
            &mut control,
            RuntimeRef::shell(),
            repo.cwd().to_path_buf(),
            "b",
        )
        .await,
    );
    assert_eq!(error.code, "session_cwd_missing");
    assert_eq!(error.class, ErrorClass::Runtime);

    let list = Request::make("cwd-missing-list", method::SESSION_LIST, Value::Null);
    let sessions: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut control, &list).await))
            .expect("list sessions");
    assert!(
        sessions.is_empty(),
        "the refused session must not stay listed: {sessions:?}"
    );

    assert!(
        !started.exists(),
        "the agent must not start when its worktree is missing"
    );

    // A store that was never written has no bindings at all.
    let bindings = std::fs::read_to_string(&store_path).unwrap_or_default();
    assert!(
        !bindings.contains("\"kind\":\"worktree\""),
        "the refused create drops its worktree binding: {bindings}"
    );

    // `project.show` lists the worktrees git reports for the repository. None
    // may remain under the worktree root.
    let added = Request::make(
        "cwd-missing-project-add",
        method::PROJECT_ADD,
        serde_json::json!({ "path": repo.cwd() }),
    );
    let project: ProjectInfo =
        serde_json::from_value(ok_payload(exchange(&mut control, &added).await))
            .expect("project info");
    let show = Request::make(
        "cwd-missing-project-show",
        method::PROJECT_SHOW,
        serde_json::json!({ "reference": project.id }),
    );
    let shown: ProjectShowResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &show).await))
            .expect("project show result");
    assert!(
        !shown.worktrees.is_empty(),
        "project.show lists the repository's own worktree"
    );
    assert!(
        shown
            .worktrees
            .iter()
            .all(|worktree| !worktree.path.starts_with(&worktree_root)),
        "no worktree may stay listed under the root: {:?}",
        shown.worktrees
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The daemon opens its event journal with the retention of its configuration:
/// session traffic rotates the journal and every file stays within the bound.
#[tokio::test]
async fn event_log_rotates_within_the_configured_retention() {
    const MAX_FILE_BYTES: u64 = 16 * 1024;
    const MAX_FILES: u32 = 3;
    let events_parent = temp_dir("eventlog-retention");
    let events_dir = events_parent.join("events");
    let socket = temp_socket("eventlog-retention");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exec sleep 30"]),
        event_log_dir: Some(events_dir.clone()),
        event_retention: pohunek_service_config::EventRetention {
            max_file_bytes: MAX_FILE_BYTES,
            max_files: MAX_FILES,
            max_age: Duration::from_hours(24),
        },
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    for _ in 0..40 {
        let created = create_session(&mut control, &socket).await;
        let _ = resize_session(&mut control, &created.id, 100, 40).await;
    }

    let rotated = events_dir.join("events.jsonl.1");
    wait_until("the journal to rotate", || async {
        tokio::fs::metadata(&rotated).await.ok()
    })
    .await;
    // Stopping the server does not quiesce the journal writer: the harness
    // drains hold a token that is never cancelled and the writer queue may
    // still hold events, so a rotation can rename a file during the listing
    // below. The `NotFound` arm tolerates that; keep it.
    let _ = shutdown.send(());
    let _ = handle.await;
    let mut journal_files = 0;
    for entry in std::fs::read_dir(&events_dir).expect("read events dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("events.jsonl")
            && Path::new(&name).extension().is_none_or(|ext| ext != "lock")
        {
            journal_files += 1;
            // The journal writer may still rotate; a file renamed away between
            // the listing and the stat is not a bound violation.
            match entry.metadata() {
                Ok(metadata) => assert!(
                    metadata.len() <= MAX_FILE_BYTES,
                    "{name} exceeds the configured bound"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("stat {name}: {error}"),
            }
        }
    }
    assert!(journal_files <= MAX_FILES, "{journal_files} journal files");
}

/// Milestone-9 checkpoint (event log): the daemon's append-only event log records
/// the session lifecycle and NEVER contains raw terminal output.
#[tokio::test]
async fn event_log_records_lifecycle_and_never_terminal_bytes() {
    const SENTINEL: &str = "SENTINEL_TERMINAL_OUTPUT_DEADBEEF";
    let events_parent = temp_dir("eventlog-data");
    let events_dir = events_parent.join("events");
    let socket = temp_socket("eventlog");

    // The shell prints a unique sentinel to its PTY; that raw output must never
    // reach the event log, which records only structured control events.
    let shell_cmd = format!("printf '{SENTINEL}\\n'; exec sleep 30");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c".to_owned(), shell_cmd]),
        event_log_dir: Some(events_dir.clone()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control, &socket).await;
    // Drive a second lifecycle event, then stop to produce session_stopped.
    let _ = resize_session(&mut control, &created.id, 100, 40).await;
    let stop_req = Request::make(
        "stop-eventlog",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _ = exchange(&mut control, &stop_req).await;

    // Wait until the drain records the stop (the final lifecycle event).
    let log_path = events_dir.join("events.jsonl");
    let bytes = read_file_until(&log_path, b"session_stopped").await;
    let text = String::from_utf8_lossy(&bytes);

    // Every non-empty line is exactly one JSON event carrying a protocol version.
    let mut saw_created = false;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let parsed: Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("invalid event line {line:?}: {err}"));
        assert!(
            parsed.get("v").is_some(),
            "event line carries a protocol version: {line}"
        );
        assert!(
            parsed.get("event").is_some(),
            "event line carries an event name: {line}"
        );
        if parsed["event"].as_str() == Some(event::SESSION_CREATED) {
            saw_created = true;
        }
    }
    assert!(saw_created, "event log must record session_created: {text}");
    assert!(
        !text.contains(SENTINEL),
        "event log must never contain raw terminal output: {text}"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Notification events are structured control-plane events, so the append-only
/// event log records them just like session lifecycle events.
#[tokio::test]
async fn event_log_records_notification_control_events() {
    let events_parent = temp_dir("notification-eventlog-data");
    let events_dir = events_parent.join("events");
    let socket = temp_socket("notification-eventlog");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        event_log_dir: Some(events_dir.clone()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created = create_notification(&mut control, notification_params(None)).await;

    let log_path = events_dir.join("events.jsonl");
    let bytes = read_file_until(&log_path, event::NOTIFICATION_CREATED.as_bytes()).await;
    let text = String::from_utf8_lossy(&bytes);
    let mut saw_created = false;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let parsed: Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("invalid event line {line:?}: {err}"));
        if parsed["event"].as_str() == Some(event::NOTIFICATION_CREATED)
            && parsed["record"]["id"].as_str() == Some(created.record.id.0.as_str())
        {
            saw_created = true;
        }
    }
    assert!(
        saw_created,
        "event log must record notification_created for {}: {text}",
        created.record.id.0
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A worktree-bound session records its worktree and explicit native-recovery
/// metadata in the same atomic store.
#[tokio::test]
async fn worktree_session_persists_recovery_and_worktree_metadata() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let agent = RuntimeRef::claude();
    let bin_name = "claude";
    let native_id = "native-claude-wt-1";

    let repo = init_git_repo();
    let bin_dir = temp_dir("wt-resume-bin");
    let data_dir = temp_dir("wt-resume-state");
    let store_path = data_dir.join("metadata.jsonl");
    let worktree_root = data_dir.join("worktrees");
    // The agent's own process is the PTY root, so it is the process that reports the native id.
    let mut root_agent =
        root_reporter::RootReporter::install(&bin_dir, bin_name, bin_name, &data_dir);

    let socket = temp_socket("wt-resume");
    let config = SessionRegistryConfig {
        shell_command: support::hermetic_shell(),
        store_path: Some(store_path.clone()),
        worktree_root: Some(worktree_root.clone()),
        ..SessionRegistryConfig::default()
    };

    let _path = PathGuard::prepend(&bin_dir);

    // --- Create phase: report a native id after the session commit. ---
    let (shutdown, handle, _) =
        spawn_server_serving_official_runtimes(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(&mut control, agent, repo.cwd().to_path_buf(), "feat/x").await,
    ))
    .expect("worktree stub session info");
    let worktree_path = created.worktree_path.clone().expect("worktree bound");
    assert_eq!(created.branch.as_deref(), Some("feat/x"));
    assert_eq!(created.cwd, worktree_path);

    let worker_instance_id = created
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.worker_instance_id.as_deref())
        .expect("created worktree session exposes its runtime id");
    let report_req = Request::make(
        "wt-resume-report-native-id",
        method::SESSION_REPORT_NATIVE_ID,
        serde_json::to_value(
            SessionReportNativeIdParams::new(
                created.id.clone(),
                worker_instance_id,
                bin_name,
                created.pid,
                process_start_identity(created.pid),
                ReportSequence::new(1),
                (OffsetDateTime::now_utc() + time::Duration::seconds(30))
                    .format(&Rfc3339)
                    .expect("format native identity expiry"),
                native_id,
                None,
            )
            .expect("valid worktree native identity claim"),
        )
        .expect("serialize worktree report-native-id params"),
    );
    let reported: SessionReportNativeIdResult = serde_json::from_value(ok_payload(
        root_agent
            .send(
                "worktree-report",
                root_reporter::Via::Root,
                &socket,
                &report_req,
            )
            .await,
    ))
    .expect("worktree report-native-id result");
    assert!(reported.recorded, "worktree native id must be recorded");

    let captured = inspect_session(&mut control, &created.id).await;
    assert_eq!(captured.native_session_id.as_deref(), Some(native_id));

    // Both records coexist in one unified metadata file while recovery remains
    // eligible.
    let store = Store::new(store_path.clone());
    let (resume_before, worktrees_before) =
        wait_for_persisted_resume_and_worktree(&store, &created.id).await;
    assert_eq!(
        resume_before.len(),
        1,
        "resume binding persisted: {resume_before:?}"
    );
    assert_eq!(
        worktrees_before.len(),
        1,
        "worktree binding persisted: {worktrees_before:?}"
    );
    assert_eq!(resume_before[0].session_id, created.id.0);
    assert_eq!(worktrees_before[0].session_id, created.id.0);

    let stop_req = Request::make(
        "session-stop-wt-resume",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _ = exchange(&mut control, &stop_req).await;
    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Sends `worktree.remove` for `path` over the control socket.
async fn remove_worktree(framed: &mut Framed<UnixStream, LinesCodec>, path: &Path) -> Response {
    let request = Request::make(
        "worktree-remove",
        method::WORKTREE_REMOVE,
        serde_json::to_value(WorktreeRemoveParams {
            path: path.to_path_buf(),
        })
        .expect("serialize worktree.remove params"),
    );
    exchange(framed, &request).await
}

/// `worktree.remove` is a public wire method with no in-repo client, so its
/// success and fail-closed paths are pinned over the real control socket: an
/// owned stopped worktree is removed, a live one is refused with
/// `worktree_in_use`, an unowned path (the main checkout) is refused with
/// `worktree_not_owned`, and malformed params are a `bad_request`.
#[tokio::test]
async fn worktree_remove_over_the_socket_succeeds_and_fails_closed() {
    let repo = init_git_repo();
    let worktree_root = temp_dir("wt-remove-root");
    let store_dir = temp_dir("wt-remove-store");
    let socket = temp_socket("wt-remove");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exec sleep 30"]),
        worktree_root: Some(worktree_root.to_path_buf()),
        store_path: Some(store_dir.join("metadata.jsonl")),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;

    let mut sessions = Vec::new();
    for branch in ["feature/remove-stopped", "feature/remove-live"] {
        let info: SessionInfo = serde_json::from_value(ok_payload(
            create_worktree_session(
                &mut control,
                RuntimeRef::shell(),
                repo.cwd().to_path_buf(),
                branch,
            )
            .await,
        ))
        .expect("worktree session info");
        sessions.push(info);
    }
    let live = sessions.pop().expect("live session");
    let stopped = sessions.pop().expect("stopped session");
    let stopped_path = stopped
        .worktree_path
        .clone()
        .expect("stopped worktree path");
    let live_path = live.worktree_path.clone().expect("live worktree path");

    // A live session keeps its worktree: refused, directory untouched.
    let refused = err_payload(remove_worktree(&mut control, &live_path).await);
    assert_eq!(refused.code, "worktree_in_use");
    assert!(
        live_path.exists(),
        "a live session's worktree stays on disk"
    );

    // The main checkout has no worktree binding: refused, checkout untouched.
    let unowned = err_payload(remove_worktree(&mut control, repo.cwd()).await);
    assert_eq!(unowned.code, "worktree_not_owned");
    assert!(
        repo.cwd().join("README.md").is_file(),
        "the main checkout is untouched"
    );

    // Params that do not deserialize are rejected before any filesystem work.
    let malformed = Request::make(
        "worktree-remove-malformed",
        method::WORKTREE_REMOVE,
        serde_json::json!({ "path": 42 }),
    );
    let invalid = err_payload(exchange(&mut control, &malformed).await);
    assert_eq!(invalid.code, "bad_request");
    assert!(live_path.exists() && stopped_path.exists());

    // Once the session is stopped its owned worktree is removed.
    let stop_request = Request::make(
        "worktree-remove-stop",
        method::SESSION_STOP,
        serde_json::to_value(&stopped.id).expect("serialize id"),
    );
    let stop_result: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &stop_request).await))
            .expect("stop result");
    assert!(stop_result.stopped);
    let removed: WorktreeRemoveResult = serde_json::from_value(ok_payload(
        remove_worktree(&mut control, &stopped_path).await,
    ))
    .expect("worktree.remove result");
    assert!(removed.removed);
    assert!(!stopped_path.exists(), "the worktree directory is gone");
    assert!(live_path.exists(), "the other worktree is untouched");

    let stop_live = Request::make(
        "worktree-remove-stop-live",
        method::SESSION_STOP,
        serde_json::to_value(&live.id).expect("serialize id"),
    );
    let _ = exchange(&mut control, &stop_live).await;
    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Timeout of a wait held across a hangup: the largest `session.wait` accepts,
/// so the release on hangup is as far as possible from the release at the
/// wait's own deadline, and short enough that a missing release fails fast.
const HELD_WAIT_TIMEOUT_MS: u32 = 8_000;

/// Timeout of a waiter-slot probe: admitted, it returns `timeout` at once; with
/// the slot taken, it returns `session_waiter_limit_reached` without waiting.
const SLOT_PROBE_TIMEOUT_MS: u32 = 1;

/// Registry configuration with one observation waiter and one turn waiter per
/// session, so a single held wait fills a session's cap in its pool.
fn single_waiter_registry_config() -> SessionRegistryConfig {
    SessionRegistryConfig {
        observation_session_waiters: 1,
        turn_session_waiters: 1,
        ..support::hermetic_registry_config()
    }
}

/// Builds one request line of a bounded wait: `(id, session, timeout_ms)`.
type WaitLine = fn(&str, &SessionId, u32) -> String;

/// A `session.wait` line that a live shell session never satisfies on its own:
/// it ends at `timeout_ms` or when the session is stopped.
fn stop_wait_line(id: &str, session_id: &SessionId, timeout_ms: u32) -> String {
    let request = Request::make(
        id,
        method::SESSION_WAIT,
        serde_json::json!({
            "session_id": session_id,
            "states": [SessionState::Stopped],
            "timeout_ms": timeout_ms,
        }),
    );
    serde_json::to_string(&request).expect("serialize session.wait")
}

/// A `session.wait_turn` line that a shell session never satisfies: a shell
/// records no `turn_completed`, so it ends at `timeout_ms`, or with
/// `targets_ended` once the session is stopped.
fn turn_wait_line(id: &str, session_id: &SessionId, timeout_ms: u32) -> String {
    let request = Request::make(
        id,
        method::SESSION_WAIT_TURN,
        serde_json::json!({
            "session_ids": [session_id],
            "until": ["turn_completed"],
            "timeout_ms": timeout_ms,
        }),
    );
    serde_json::to_string(&request).expect("serialize session.wait_turn")
}

/// Asks on a fresh connection whether `session_id`'s only waiter slot in the
/// pool of `wait_line`'s method is taken.
async fn waiter_slot_taken(socket: &Path, session_id: &SessionId, wait_line: WaitLine) -> bool {
    let mut probe = connect(socket).await;
    let response = exchange_line(
        &mut probe,
        wait_line("slot-probe", session_id, SLOT_PROBE_TIMEOUT_MS),
    )
    .await;
    match response.into_result() {
        Ok(result) => {
            assert_eq!(result["reason"], "timeout", "an admitted probe times out");
            false
        }
        Err(error) => {
            assert_eq!(error.code, "session_waiter_limit_reached");
            true
        }
    }
}

/// Opens a connection whose `wait_line` wait owns `session_id`'s only waiter
/// slot in that method's pool.
///
/// The wait is confirmed in flight when a probe is refused. A probe admitted
/// while the wait is still registering can make the daemon refuse the wait
/// instead; that refusal is read and the wait is sent again.
async fn hold_only_waiter_slot(
    socket: &Path,
    session_id: &SessionId,
    id: &str,
    wait_line: WaitLine,
) -> Framed<UnixStream, LinesCodec> {
    let line = wait_line(id, session_id, HELD_WAIT_TIMEOUT_MS);
    let holder = tokio::sync::Mutex::new(connect(socket).await);
    holder
        .lock()
        .await
        .send(line.clone())
        .await
        .expect("send the held wait");
    wait_until(
        "the held wait to own the session's only waiter slot",
        || async {
            if waiter_slot_taken(socket, session_id, wait_line).await {
                return Some(());
            }
            let mut holder = holder.lock().await;
            if let Some(reply) = holder.next().now_or_never() {
                let reply = reply
                    .expect("the held wait connection stays open")
                    .expect("response framing ok");
                let response: Response = serde_json::from_str(&reply).expect("parse response");
                assert_eq!(
                    err_payload(response).code,
                    "session_waiter_limit_reached",
                    "the held wait only ends early when a probe owned the slot"
                );
                holder
                    .send(line.clone())
                    .await
                    .expect("send the held wait again");
            }
            None
        },
    )
    .await;
    holder.into_inner()
}

async fn stop_session(framed: &mut Framed<UnixStream, LinesCodec>, session_id: &SessionId) {
    let request = Request::make(
        "session-stop",
        method::SESSION_STOP,
        serde_json::to_value(session_id).expect("serialize id"),
    );
    let stopped: SessionStopResult =
        serde_json::from_value(ok_payload(exchange(framed, &request).await)).expect("stop result");
    assert!(stopped.stopped);
}

/// Proves that a client hanging up during a `wait_line` wait frees its waiter
/// slot before the wait's deadline.
///
/// The witness wait on another session owns its slot before the held wait
/// starts, with the same timeout, so its deadline comes first. A slot freed only
/// at the held wait's deadline would let the witness reply before a new wait is
/// admitted.
async fn assert_hangup_frees_waiter_slot(tag: &str, wait_line: WaitLine) {
    let socket = temp_socket(tag);
    let (shutdown, handle, _) =
        spawn_server_with_config(&socket, "0.0.0", single_waiter_registry_config()).await;
    let mut control = connect(&socket).await;
    let held_session = create_session(&mut control, &socket).await;
    let witness_session = create_session(&mut control, &socket).await;

    let mut witness =
        hold_only_waiter_slot(&socket, &witness_session.id, "witness-wait", wait_line).await;
    let held = hold_only_waiter_slot(&socket, &held_session.id, "held-wait", wait_line).await;
    drop(held);

    tokio::select! {
        biased;
        reply = witness.next() => panic!(
            "the witness wait ended before the closed connection's waiter slot was free: {reply:?}"
        ),
        () = wait_until("a new wait to be admitted after the client hung up", || async {
            (!waiter_slot_taken(&socket, &held_session.id, wait_line).await).then_some(())
        }) => {}
    }

    stop_session(&mut control, &held_session.id).await;
    stop_session(&mut control, &witness_session.id).await;
    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A client that hangs up during `session.wait` frees its observation waiter
/// slot before the wait's deadline.
#[tokio::test]
async fn session_wait_hangup_releases_the_waiter_slot_before_its_deadline() {
    assert_hangup_frees_waiter_slot("wait-hangup", stop_wait_line).await;
}

/// A client that hangs up during `session.wait_turn` frees its turn waiter
/// slot before the wait's deadline.
#[tokio::test]
async fn session_wait_turn_hangup_releases_the_turn_waiter_slot_before_its_deadline() {
    assert_hangup_frees_waiter_slot("turn-hangup", turn_wait_line).await;
}

/// A request line pipelined behind an in-flight `session.wait` is answered
/// after the wait's own reply, on the same connection.
#[tokio::test]
async fn session_wait_answers_a_pipelined_request_after_the_wait() {
    let socket = temp_socket("wait-pipeline");
    let (shutdown, handle, _) =
        spawn_server_with_config(&socket, "0.0.0", single_waiter_registry_config()).await;
    let mut control = connect(&socket).await;
    let session = create_session(&mut control, &socket).await;

    let mut held = hold_only_waiter_slot(&socket, &session.id, "held-wait", stop_wait_line).await;
    let pipelined = Request::make("pipelined-health", method::DAEMON_HEALTH, Value::Null);
    held.send(serde_json::to_string(&pipelined).expect("serialize health"))
        .await
        .expect("send the pipelined request");
    stop_session(&mut control, &session.id).await;

    let mut replies = Vec::new();
    for _ in 0..2 {
        let reply = wait::guard("a reply on the pipelined connection", held.next())
            .await
            .expect("a response line")
            .expect("response framing ok");
        replies.push(serde_json::from_str::<Response>(&reply).expect("parse response"));
    }
    assert_eq!(
        replies.iter().map(Response::id).collect::<Vec<_>>(),
        ["held-wait", "pipelined-health"],
        "replies keep request order"
    );
    let mut replies = replies.into_iter();
    let wait = ok_payload(replies.next().expect("the wait reply"));
    assert_eq!(wait["reason"], "state_matched");
    assert_eq!(wait["session"]["state"], "stopped");
    let health = ok_payload(replies.next().expect("the pipelined reply"));
    assert_eq!(health["status"], "ok");

    let _ = shutdown.send(());
    let _ = handle.await;
}
