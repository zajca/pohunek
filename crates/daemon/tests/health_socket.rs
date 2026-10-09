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
#[path = "../src/procwatch/readable_host.rs"]
mod readable_host;
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{
    event, method, AgentActivity, AttachHeader, ErrorClass, Event, HostDiscoverParams, HostRecord,
    IntegrationDoctorResult, IntegrationInstallState, IntegrationStatusResult,
    IntegrationUninstallResult, IntegrationUninstallState, NotificationCreateParams,
    NotificationCreateResult, NotificationDeleteParams, NotificationDeleteResult, NotificationKind,
    NotificationKindPolicy, NotificationListParams, NotificationListResult, NotificationPolicy,
    NotificationPolicyParams, NotificationPolicyResult, NotificationRetentionParams,
    NotificationRetentionResult, NotificationSeverity, NotificationSource, NotificationStatus,
    NotificationUpdateParams, NotificationUpdateResult, ProcessStartIdentity, ReportSequence,
    Request as ProtocolRequest, Response, RuntimeId, RuntimeRef, SessionAttachParams,
    SessionAttachResult, SessionDetachParams, SessionDetachResult, SessionId, SessionInfo,
    SessionInputParams, SessionInputResult, SessionListFilter, SessionListParams, SessionNewParams,
    SessionRemoveResult, SessionReportAgentParams, SessionReportAgentResult,
    SessionReportNativeIdParams, SessionReportNativeIdResult, SessionResizeParams,
    SessionResizeResult, SessionState, SessionStopResult, StateSource, TerminalDimensions,
    WorktreeRemoveParams, WorktreeRemoveResult, PROTOCOL_VERSION,
};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo};
use pohunek_daemon::events::{spawn_drain, EventLog};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::notifications::NotificationService;
use pohunek_daemon::procwatch::{HostInspector, ProcessInspector};
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_daemon::store::{ResumeBinding, Store, WorktreeBinding};
use pohunek_paths::{
    APP_DIR, HOST_APPROVAL_KEY_NAME, HOST_GOVERNANCE_NAME, HOST_IDENTITY_NAME,
    HOST_STATE_LOCK_NAME, HOST_STATE_SUBDIR, LOGS_SUBDIR, WORKERS_SUBDIR,
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
    let health = HealthInfo::new(version);
    let server = ControlServer::bind(
        socket,
        health,
        governance_service(socket).await,
        support::overlay_registry(),
    )
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
    let (shutdown, server) = spawn_server(&socket, "test").await;
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
async fn integration_uninstall_and_doctor_run_at_daemon_boundary() {
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
    let (shutdown, server) = spawn_server(&socket, "test").await;
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
    mut config: SessionRegistryConfig,
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
            .with_environment_source(support::hermetic_environment_source()),
    );
    let launcher = Arc::new(SubprocessWorkerLauncher::new());
    let registry = SessionRegistry::new_with_launcher_and_inspector(
        config,
        launcher,
        Arc::new(readable_host::ReadableHost::new()),
    );
    (registry, worker_home_dir)
}

/// Spawn the control server with a custom shell command.
async fn spawn_server_with_config(
    socket: &std::path::Path,
    version: &str,
    config: SessionRegistryConfig,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let event_log_dir = config.event_log_dir.clone();
    let (registry, worker_home) = worker_backed_registry(socket, config);
    let notifications = NotificationService::open(&notification_data_dir(socket))
        .expect("notification service opens");
    if let Some(event_log_dir) = event_log_dir {
        let log = Arc::new(EventLog::open(&event_log_dir).expect("event log opens"));
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
    )
    .with_notifications(notifications);
    let server = ControlServer::bind_with_state(socket, state)
        .await
        .expect("server binds");
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        // Removed after the server and its registry are gone.
        let _worker_home = worker_home;
        server
            .serve(async move {
                let _ = rx.await;
            })
            .await;
    });
    (tx, handle)
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
fn stub_agent_script(argv_log: &Path) -> String {
    format!(
        "#!/bin/sh\n\
printf '%s\\n' \"$*\" >> '{argv}'\n\
/bin/sleep 30\n",
        argv = argv_log.display(),
    )
}

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
#[expect(
    clippy::too_many_lines,
    reason = "This real-daemon regression keeps bounded startup, worker lifecycle, permission, and cleanup assertions in one auditable sequence."
)]
async fn daemon_startup_creates_private_host_state_from_ordinary_xdg_state_home() {
    use std::os::unix::fs::PermissionsExt;

    let mut xdg = XdgGuard::set_all("daemon-startup-private-state");
    // The real worker nests its control socket below the XDG runtime root, and
    // `sockaddr_un` imposes a small platform path bound. Keep this fixture short
    // while retaining an isolated complete XDG environment.
    let root_dir =
        pohunek_test_support::tempdir_with_prefix("pw-s-").expect("create short isolated XDG root");
    let root = root_dir.path().to_path_buf();
    // Declared after `root_dir` and before the daemon child: the daemon is
    // killed first, then any worker still below the root is reaped, then the
    // root is removed.
    let _workers = WorkerGuard::watch(&root);
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
    let mut child = daemon_env
        .tokio_command(bin_exe("pohunekd"))
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
        .kill_on_drop(true)
        .spawn()
        .expect("spawn real daemon");
    let socket = runtime_home.join(APP_DIR).join("daemon.sock");
    let mut client = wait_for_daemon_socket(&mut child, &socket).await;
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
    let (shutdown, handle) =
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
    // The daemon runs as a child with a `PATH` of its own, so no agent binary
    // is resolvable whatever the host has installed and no process-wide
    // variable changes.
    let env = TestEnv::new().expect("create the daemon child environment");
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

    // `session.new --agent claude` must fail with the typed, recoverable
    // `agent_binary_missing` error (stable code a script can branch on)
    // rather than a generic spawn failure.
    let resp = tokio::time::timeout(
        DAEMON_CONTROL_REQUEST_TIMEOUT,
        create_session_with_agent(&mut client, RuntimeRef::claude(), env.cwd().to_path_buf()),
    )
    .await
    .expect("session.new returns before control timeout");

    let err = err_payload(resp);
    assert_eq!(err.class, ErrorClass::Runtime);
    assert_eq!(err.code, "agent_binary_missing");
    assert!(
        err.msg.contains("claude"),
        "error must name the missing binary: {err:?}"
    );
    assert!(
        err.recover.is_some(),
        "missing-binary error must carry a recover hint: {err:?}"
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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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

#[tokio::test]
async fn codex_stub_session_publishes_blocked_and_receives_bracketed_input() {
    let bin_dir = temp_dir("codex-stub-bin");
    let cwd = temp_dir("codex-stub-cwd");
    let input_log_dir = temp_dir("codex-stub-input");
    let input_log = input_log_dir.join("input.bin");
    let cwd_log_dir = temp_dir("codex-stub-pwd");
    let cwd_log = cwd_log_dir.join("pwd.txt");
    write_executable(
        &bin_dir.join("codex"),
        &format!(
            "#!/bin/sh\npwd > \"{}\"\n/bin/sleep 0.2\nprintf '\\033]2;Action Required\\007'\nIFS= read -r line\nprintf '%s' \"$line\" > \"{}\"\n/bin/sleep 30\n",
            cwd_log.display(),
            input_log.display()
        ),
    );

    let socket = temp_socket("codex-stub");
    let (shutdown, handle) =
        spawn_server_with_config(&socket, "0.0.0", support::hermetic_registry_config()).await;

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
    let bin_dir = temp_dir("claude-stub-bin");
    let cwd = temp_dir("claude-stub-cwd");
    let input_log_dir = temp_dir("claude-stub-input");
    let input_log = input_log_dir.join("input.bin");
    write_executable(
        &bin_dir.join("claude"),
        &format!(
            "#!/bin/sh\n/bin/sleep 0.2\nprintf '\\033[2J\\033[HReview\\r\\n────\\r\\nenter to select\\r\\nesc to cancel\\r\\n↑/↓ to navigate\\r\\n'\nIFS= read -r line\nprintf '%s' \"$line\" > \"{}\"\n/bin/sleep 30\n",
            input_log.display()
        ),
    );

    let socket = temp_socket("claude-stub");
    let (shutdown, handle) =
        spawn_server_with_config(&socket, "0.0.0", support::hermetic_registry_config()).await;

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

#[tokio::test]
async fn session_survives_requesting_client_exit() {
    let socket = temp_socket("session-client-independence");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (success_shutdown, success_handle) =
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
    let (failure_shutdown, failure_handle) =
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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

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
    let agent = RuntimeRef::claude();
    let bin_name = "claude";
    let native_id = "native-claude-wt-1";

    let repo = init_git_repo();
    let bin_dir = temp_dir("wt-resume-bin");
    let data_dir = temp_dir("wt-resume-state");
    let store_path = data_dir.join("metadata.jsonl");
    let worktree_root = data_dir.join("worktrees");
    let argv_log_dir = temp_dir("wt-resume-argv");
    let argv_log = argv_log_dir.join("argv.log");
    write_executable(&bin_dir.join(bin_name), &stub_agent_script(&argv_log));

    let socket = temp_socket("wt-resume");
    let config = SessionRegistryConfig {
        shell_command: support::hermetic_shell(),
        store_path: Some(store_path.clone()),
        worktree_root: Some(worktree_root.clone()),
        ..SessionRegistryConfig::default()
    };

    let _path = PathGuard::prepend(&bin_dir);

    // --- Create phase: report a native id after the session commit. ---
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
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
    let reported: SessionReportNativeIdResult =
        serde_json::from_value(ok_payload(exchange(&mut control, &report_req).await))
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
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
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
