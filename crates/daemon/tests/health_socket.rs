//! Integration test: the daemon's control server answers `daemon.health` over a
//! real Unix socket using newline-delimited JSON.
//!
//! This is the milestone-2 checkpoint ("CLI `doctor` + `daemon start` talk over
//! the socket") exercised at the protocol layer: it binds the actual
//! `ControlServer` on a temp socket, connects a raw client, and verifies the
//! response carries the daemon and protocol versions. It also covers
//! stale-socket recovery and the `method_not_found` path.

mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{
    event, method, AgentActivity, AgentKind, AssistantMaterializeParams,
    AssistantMaterializeResult, AttachHeader, ErrorClass, Event, HostDiscoverParams, HostRecord,
    IntegrationInstallState, IntegrationStatusResult, NotificationCreateParams,
    NotificationCreateResult, NotificationDeleteParams, NotificationDeleteResult, NotificationKind,
    NotificationKindPolicy, NotificationListParams, NotificationListResult, NotificationPolicy,
    NotificationPolicyParams, NotificationPolicyResult, NotificationRetentionParams,
    NotificationRetentionResult, NotificationSeverity, NotificationSource, NotificationStatus,
    NotificationUpdateParams, NotificationUpdateResult, ProcessStartIdentity, ReportSequence,
    Request as ProtocolRequest, Response, SessionAttachParams, SessionAttachResult,
    SessionDetachParams, SessionDetachResult, SessionId, SessionInfo, SessionInputParams,
    SessionInputResult, SessionListFilter, SessionListParams, SessionNewParams,
    SessionRemoveResult, SessionReportAgentParams, SessionReportAgentResult,
    SessionReportNativeIdParams, SessionReportNativeIdResult, SessionResizeParams,
    SessionResizeResult, SessionState, SessionStopResult, StateSource, TerminalDimensions,
    PROTOCOL_VERSION,
};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex, MutexGuard};
use tokio_util::codec::{Framed, LinesCodec};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo};
use pohunek_daemon::events::{spawn_drain, EventLog};
use pohunek_daemon::notifications::NotificationService;
use pohunek_daemon::procwatch::LinuxInspector;
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_daemon::store::{ResumeBinding, Store, WorktreeBinding};

static PATH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static XDG_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Request;

impl Request {
    fn make(id: &str, method: &str, params: Value) -> ProtocolRequest {
        ProtocolRequest::new(id, method, params).expect("valid test request")
    }
}

struct PathGuard {
    _guard: MutexGuard<'static, ()>,
    old_path: Option<OsString>,
}

impl PathGuard {
    async fn prepend(path: &Path) -> Self {
        let guard = PATH_LOCK.lock().await;
        let old_path = std::env::var_os("PATH");
        let mut paths = vec![path.to_path_buf()];
        if let Some(old_path) = &old_path {
            paths.extend(std::env::split_paths(old_path));
        }
        let joined = std::env::join_paths(paths).expect("join test PATH");
        std::env::set_var("PATH", joined);
        Self {
            _guard: guard,
            old_path,
        }
    }

    /// Replace PATH with an isolated directory that contains NONE of the agent
    /// binaries, so agent resolution deterministically fails regardless of what
    /// is installed on the host (claude/codex may well be on the developer's
    /// PATH).
    ///
    /// `PATH` is a process-wide environment variable (`std::env::set_var` has no
    /// thread/task scoping), so while it is replaced here every OTHER concurrent
    /// test's `fork`/`exec` of a worker-backed session's shell inherits this same
    /// isolated `PATH` — `PATH_LOCK` only serializes this swap against other
    /// PATH-*mutating* tests, not against ordinary session creation happening
    /// elsewhere in the suite. The rest of the suite resolves a handful of tools
    /// through PATH (`git`, `python3`, `sh` for stub agents; `sleep`/`stty` inside
    /// worker-backed shell test commands such as `"sleep 30"` or `"stty -echo"`);
    /// the isolated dir is seeded with symlinks to all of those, resolved from the
    /// current PATH, so replacing PATH cannot starve a sibling test of them (a
    /// missing `sleep` here previously surfaced as a sibling's shell exiting with
    /// code 127 and its session flipping to `Failed` mid-test). PATH is restored
    /// on drop.
    async fn isolated_without_agents(tag: &str) -> Self {
        let guard = PATH_LOCK.lock().await;
        let old_path = std::env::var_os("PATH");
        let dir = temp_dir(tag);
        if let Some(old_path) = &old_path {
            for tool in ["git", "python3", "sh", "sleep", "stty"] {
                if let Some(real) = which_in(old_path, tool) {
                    let _ = std::os::unix::fs::symlink(&real, dir.join(tool));
                }
            }
        }
        std::env::set_var("PATH", &dir);
        Self {
            _guard: guard,
            old_path,
        }
    }
}

/// Resolve `name` to its first existing entry across the directories in a PATH
/// value. A dependency-free `which` for [`PathGuard::isolated_without_agents`].
fn which_in(path_var: &OsString, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }
}

struct XdgGuard {
    _guard: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<String>)>,
    root: PathBuf,
}

impl XdgGuard {
    async fn set_all(tag: &str) -> Self {
        Self::set_all_after(tag, || {}).await
    }

    async fn set_all_after(tag: &str, before_isolation: impl FnOnce()) -> Self {
        let guard = XDG_LOCK.lock().await;
        let vars = [
            "XDG_RUNTIME_DIR",
            "XDG_STATE_HOME",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "HOME",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
        ];
        let saved = vars
            .iter()
            .map(|&key| (key, std::env::var(key).ok()))
            .collect::<Vec<_>>();
        before_isolation();
        let root = temp_dir(tag);
        std::env::set_var("XDG_RUNTIME_DIR", root.join("runtime"));
        std::env::set_var("XDG_STATE_HOME", root.join("state"));
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("CLAUDE_CONFIG_DIR", root.join("home/.claude"));
        std::env::set_var("CODEX_HOME", root.join("home/.codex"));
        Self {
            _guard: guard,
            saved,
            root,
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
}

impl Drop for XdgGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// A unique temp socket path inside a dedicated per-test directory.
///
/// The server enforces the directory's mode on bind, so the socket must live in
/// a directory we own (not `/tmp` itself, which is root-owned with the sticky
/// bit). This mirrors the real daemon, which always binds inside its own
/// `pohunek` runtime subdir.
fn temp_socket(tag: &str) -> std::path::PathBuf {
    temp_dir(tag).join("daemon.sock")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pohunek-test-{tag}-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test socket dir");
    dir
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
    let server = ControlServer::bind(socket, health, support::overlay_registry())
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
    let xdg = XdgGuard::set_all_after("integration-status-rpc", move || {
        std::env::set_var("CLAUDE_CONFIG_DIR", claude_override);
        std::env::set_var("CODEX_HOME", codex_override);
    })
    .await;
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
fn worker_backed_registry(
    socket: &std::path::Path,
    mut config: SessionRegistryConfig,
) -> SessionRegistry {
    let worker_home = std::env::temp_dir().join(format!(
        "pw-h-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let worker_environment = SubprocessWorkerEnvironment {
        runtime_home: worker_home.join("runtime"),
        state_home: worker_home.join("state"),
        data_home: worker_home.join("data"),
        config_home: worker_home.join("config"),
        cache_home: worker_home.join("cache"),
        daemon_socket: socket.to_path_buf(),
    };
    config.socket_path = Some(socket.to_path_buf());
    config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
    config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
    let launcher = Arc::new(SubprocessWorkerLauncher::new(
        worker_binary(),
        worker_environment,
    ));
    SessionRegistry::new_with_launcher_and_inspector(
        config,
        launcher,
        Arc::new(LinuxInspector::new()),
    )
}

/// Spawn the control server with a custom shell command.
async fn spawn_server_with_config(
    socket: &std::path::Path,
    version: &str,
    config: SessionRegistryConfig,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let event_log_dir = config.event_log_dir.clone();
    let registry = worker_backed_registry(socket, config);
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
        support::overlay_registry(),
    )
    .with_notifications(notifications);
    let server = ControlServer::bind_with_state(socket, state)
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

fn worker_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("POHUNEK_WORKER_BIN") {
        return PathBuf::from(path);
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("daemon crate is inside workspace")
        .to_path_buf();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace.join("target"), PathBuf::from);
    let binary = target.join("debug/pohunek-sessiond");
    assert!(
        binary.is_file(),
        "build the real worker first with `cargo build -p pohunek-session-worker --bin pohunek-sessiond`, or set POHUNEK_WORKER_BIN"
    );
    binary
}

fn notification_data_dir(socket: &std::path::Path) -> PathBuf {
    socket
        .parent()
        .expect("test socket has a parent")
        .join("notification-data")
}

/// Connect a raw line-framed client to `socket`.
async fn connect(socket: &std::path::Path) -> Framed<UnixStream, LinesCodec> {
    // Brief retry: bind returns before the listener is necessarily accepting in
    // all timing scenarios.
    for _ in 0..50 {
        if let Ok(stream) = UnixStream::connect(socket).await {
            return Framed::new(stream, LinesCodec::new());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to test socket {}", socket.display());
}

/// Connect a raw client to `socket` for attach-stream tests.
async fn connect_raw(socket: &std::path::Path) -> UnixStream {
    for _ in 0..50 {
        if let Ok(stream) = UnixStream::connect(socket).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect raw test socket {}", socket.display());
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

/// Map a base kind to its wire name (the `agent` field is a free string since
/// Part C; the helpers below still take an `AgentKind` for convenience).
fn agent_name(agent: &AgentKind) -> &'static str {
    match agent {
        AgentKind::Shell => "shell",
        AgentKind::Codex => "codex",
        AgentKind::Claude => "claude",
        AgentKind::Hermes => "hermes",
        AgentKind::Unknown(_) => "unknown",
    }
}

fn session_params() -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: "shell".to_owned(),
        cwd: Some(std::env::temp_dir()),
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

fn session_params_for_agent(agent: &AgentKind, cwd: PathBuf) -> SessionNewParams {
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
fn session_params_for_worktree(agent: &AgentKind, repo: PathBuf, branch: &str) -> SessionNewParams {
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
    agent: AgentKind,
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

/// Initialize a throwaway git repo on branch `main` with one commit.
fn init_git_repo(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    let init = std::process::Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(&dir)
        .output()
        .expect("git init");
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    for args in [
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["config", "commit.gpgsign", "false"],
    ] {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(&args)
            .output()
            .expect("git config");
        assert!(out.status.success(), "git {args:?} failed");
    }
    std::fs::write(dir.join("README.md"), "init\n").expect("write README");
    for args in [vec!["add", "."], vec!["commit", "-q", "-m", "init"]] {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(&args)
            .output()
            .expect("git commit");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    dir
}

fn ok_payload(response: Response) -> Value {
    response
        .into_result()
        .unwrap_or_else(|error| panic!("expected ok, got error: {error}"))
}

fn err_payload(response: Response) -> protocol::ProtocolError {
    response.into_result().expect_err("expected error response")
}

async fn create_session(framed: &mut Framed<UnixStream, LinesCodec>) -> SessionInfo {
    let req = Request::make(
        "session-new",
        method::SESSION_NEW,
        serde_json::to_value(session_params()).expect("serialize params"),
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
    agent: AgentKind,
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
    tokio::time::timeout(Duration::from_secs(5), async {
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
    .expect("file marker arrives before timeout")
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
    tokio::time::timeout(Duration::from_secs(5), async {
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
    .expect("marker arrives before timeout")
}

async fn assert_raw_stream_closes(stream: &mut UnixStream) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = [0_u8; 256];
        loop {
            let n = stream.read(&mut buf).await.expect("read raw stream");
            if n == 0 {
                return;
            }
        }
    })
    .await
    .expect("raw stream closes before timeout");
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
    for _ in 0..100 {
        let info = inspect_session(framed, id).await;
        if info.state == state {
            return info;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session {} did not reach state {state:?}", id.0);
}

async fn wait_for_agent_state_event(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
    activity: AgentActivity,
    source: StateSource,
) -> Event {
    let expected_activity = serde_json::to_value(activity).expect("serialize activity");
    let expected_source = serde_json::to_value(source).expect("serialize source");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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
    let stat =
        std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("read child process identity");
    let fields = stat
        .rsplit_once(") ")
        .expect("process stat contains command terminator")
        .1
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    let start_identity = fields
        .get(19)
        .expect("process stat contains start identity")
        .parse()
        .expect("process start identity is numeric");
    ProcessStartIdentity::new(start_identity)
}

async fn wait_for_persisted_resume_and_worktree(
    store: &Store,
    id: &SessionId,
) -> (Vec<ResumeBinding>, Vec<WorktreeBinding>) {
    for _ in 0..100 {
        let resume = store.load_resume().expect("load resume");
        let worktrees = store.load_worktrees().expect("load worktrees");
        if resume
            .iter()
            .any(|binding| binding.session_id == id.0.as_str())
            && worktrees
                .iter()
                .any(|binding| binding.session_id == id.0.as_str())
        {
            return (resume, worktrees);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let resume = store.load_resume().expect("load resume after timeout");
    let worktrees = store
        .load_worktrees()
        .expect("load worktrees after timeout");
    panic!(
        "resume and worktree bindings did not persist for {}: resume={resume:?}, worktrees={worktrees:?}",
        id.0
    );
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
async fn assistant_materialize_returns_readable_paths_over_socket() {
    let _env = XdgGuard::set_all("assistant-materialize-socket").await;
    let socket = temp_socket("assistant-materialize");
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;

    let mut client = connect(&socket).await;
    let params = AssistantMaterializeParams {
        snapshot: r#"{"source":"socket"}"#.to_owned(),
    };
    let req = Request::make(
        "assistant-materialize-socket",
        method::ASSISTANT_MATERIALIZE,
        serde_json::to_value(params).expect("params serialize"),
    );
    let resp = exchange(&mut client, &req).await;

    let result: AssistantMaterializeResult =
        serde_json::from_value(ok_payload(resp)).expect("result deserializes");
    assert!(Path::new(&result.bundle_path).join("index.md").is_file());
    assert_eq!(
        std::fs::read_to_string(&result.snapshot_path).expect("snapshot"),
        r#"{"source":"socket"}"#
    );
    assert!(result.content_hash.starts_with("sha256:"));
    assert!(!result.concepts.is_empty());

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
async fn session_list_with_malformed_filter_returns_typed_error() {
    // A foreign client (e.g. the rofi script or another CLI) can talk JSON to the
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
        spawn_server_with_config(&socket, "0.0.0", SessionRegistryConfig::default()).await;
    let mut control = connect(&socket).await;

    let created = create_session(&mut control).await;
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

#[tokio::test]
async fn session_new_for_missing_agent_binary_returns_typed_error() {
    let cwd = temp_dir("missing-agent-cwd");
    let socket = temp_socket("missing-agent");
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;

    let mut control = connect(&socket).await;
    // Claude is resolved from PATH at launch. With PATH isolated so no `claude`
    // binary is present, `session.new --agent claude` must fail with the typed,
    // recoverable `agent_binary_missing` error (stable code a script can branch
    // on) rather than a generic spawn failure.
    let resp = {
        let _path = PathGuard::isolated_without_agents("missing-agent-path").await;
        create_session_with_agent(&mut control, AgentKind::Claude, cwd).await
    };

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

    let _ = shutdown.send(());
    let _ = handle.await;
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
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if UnixStream::connect(socket).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("dropped listener never stopped accepting connections");
}

#[tokio::test]
async fn stale_socket_is_recovered_on_bind() {
    let socket = temp_socket("stale");
    // Create a stale socket file with no listener behind it.
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind stale");
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
async fn session_lifecycle_over_socket() {
    let socket = temp_socket("session-lifecycle");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut client = connect(&socket).await;
    let created = create_session(&mut client).await;
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

    let second = create_session(&mut client).await;
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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut client = connect(&socket).await;
    let created = create_session(&mut client).await;
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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut client = connect(&socket).await;
    let mut params = session_params();
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
    let input_log = temp_dir("codex-stub-input").join("input.bin");
    let cwd_log = temp_dir("codex-stub-pwd").join("pwd.txt");
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
        spawn_server_with_config(&socket, "0.0.0", SessionRegistryConfig::default()).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make("subscribe-codex-stub", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let created: SessionInfo = {
        let _path = PathGuard::prepend(&bin_dir).await;
        serde_json::from_value(ok_payload(
            create_session_with_agent(&mut control, AgentKind::Codex, cwd.clone()).await,
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
    let input_log = temp_dir("claude-stub-input").join("input.bin");
    write_executable(
        &bin_dir.join("claude"),
        &format!(
            "#!/bin/sh\n/bin/sleep 0.2\nprintf '\\033[2J\\033[HReview\\r\\n────\\r\\nenter to select\\r\\nesc to cancel\\r\\n↑/↓ to navigate\\r\\n'\nIFS= read -r line\nprintf '%s' \"$line\" > \"{}\"\n/bin/sleep 30\n",
            input_log.display()
        ),
    );

    let socket = temp_socket("claude-stub");
    let (shutdown, handle) =
        spawn_server_with_config(&socket, "0.0.0", SessionRegistryConfig::default()).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make("subscribe-claude-stub", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let created: SessionInfo = {
        let _path = PathGuard::prepend(&bin_dir).await;
        serde_json::from_value(ok_payload(
            create_session_with_agent(&mut control, AgentKind::Claude, cwd).await,
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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let created = {
        let mut first_client = connect(&socket).await;
        create_session(&mut first_client).await
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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (success_shutdown, success_handle) =
        spawn_server_with_config(&success_socket, "0.0.0", success_config).await;
    let mut success_client = connect(&success_socket).await;
    let success = create_session(&mut success_client).await;
    let done = wait_for_state(&mut success_client, &success.id, SessionState::Done).await;
    assert_eq!(done.exit_code, Some(0));
    let _ = success_shutdown.send(());
    let _ = success_handle.await;

    let failure_socket = temp_socket("session-exit-failure");
    let failure_config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exit 7"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (failure_shutdown, failure_handle) =
        spawn_server_with_config(&failure_socket, "0.0.0", failure_config).await;
    let mut failure_client = connect(&failure_socket).await;
    let failure = create_session(&mut failure_client).await;
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
        stop_grace: Duration::from_millis(50),
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
    let created = create_session(&mut creator).await;

    // The subscriber must receive a `session_created` event for that session.
    // Bound the read so the test cannot hang if streaming is broken.
    let event_line = tokio::time::timeout(Duration::from_secs(5), subscriber.next())
        .await
        .expect("event arrives before timeout")
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
        stop_grace: Duration::from_millis(50),
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
        stop_grace: Duration::from_millis(50),
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
        stop_grace: Duration::from_millis(50),
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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let session = create_session(&mut control).await;

    let created =
        create_notification(&mut control, notification_params(Some(session.id.clone()))).await;

    assert_eq!(created.record.session_id, Some(session.id));
    assert_eq!(created.record.agent_kind, Some(AgentKind::Shell));

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn notification_create_keeps_missing_session_reference() {
    let socket = temp_socket("notification-missing-session");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
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
        stop_grace: Duration::from_millis(50),
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
        stop_grace: Duration::from_millis(50),
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
        stop_grace: Duration::from_millis(50),
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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make("subscribe-report-agent", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let created = create_session(&mut control).await;
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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control).await;

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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control).await;

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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control).await;

    let first = attach_session(&mut control, &created.id).await;
    let second = attach_session(&mut control, &created.id).await;
    let mut raw_one = open_attach_stream(&socket, &first.stream_id).await;
    let mut raw_two = open_attach_stream(&socket, &second.stream_id).await;

    tokio::time::sleep(Duration::from_millis(50)).await;
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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control).await;

    // Attach, then drop the raw stream the way a closed window / SIGHUP would —
    // no `session.detach` is ever sent.
    let attach = attach_session(&mut control, &created.id).await;
    let raw = open_attach_stream(&socket, &attach.stream_id).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(raw);
    // Let the daemon's attach bridge observe the EOF and deregister the stream.
    tokio::time::sleep(Duration::from_millis(100)).await;

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
    tokio::time::sleep(Duration::from_millis(50)).await;
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
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::make("subscribe-detector", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(ack.is_ok(), "subscribe should ack");

    let mut control = connect(&socket).await;
    let created = create_session(&mut control).await;
    let attach = attach_session(&mut control, &created.id).await;
    let mut raw = open_attach_stream(&socket, &attach.stream_id).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
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
        stop_grace: Duration::from_millis(50),
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
    let created = create_session(&mut control).await;

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
async fn two_sessions_on_one_repo_get_distinct_worktrees() {
    let repo = init_git_repo("two-wt-repo");
    let worktree_root = temp_dir("two-wt-root");
    let store_path = temp_dir("two-wt-store").join("metadata.jsonl");
    let socket = temp_socket("two-worktrees");

    let config = SessionRegistryConfig {
        // Each shell records its working directory into its own worktree, which
        // proves the process was launched *inside* the bound tree.
        shell_command: ShellCommand::new("/bin/sh", ["-c", "pwd > pohunek-pwd.txt; exec sleep 30"]),
        stop_grace: Duration::from_millis(50),
        worktree_root: Some(worktree_root.clone()),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let alpha: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(
            &mut control,
            AgentKind::Shell,
            repo.clone(),
            "feature/alpha",
        )
        .await,
    ))
    .expect("alpha session info");
    let beta: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(&mut control, AgentKind::Shell, repo.clone(), "feature/beta").await,
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
    let events_dir = temp_dir("eventlog-data").join("events");
    let socket = temp_socket("eventlog");

    // The shell prints a unique sentinel to its PTY; that raw output must never
    // reach the event log, which records only structured control events.
    let shell_cmd = format!("printf '{SENTINEL}\\n'; exec sleep 30");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c".to_owned(), shell_cmd]),
        stop_grace: Duration::from_millis(50),
        event_log_dir: Some(events_dir.clone()),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control).await;
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
    let events_dir = temp_dir("notification-eventlog-data").join("events");
    let socket = temp_socket("notification-eventlog");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
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
    let agent = AgentKind::Claude;
    let bin_name = "claude";
    let native_id = "native-claude-wt-1";

    let repo = init_git_repo("wt-resume-repo");
    let bin_dir = temp_dir("wt-resume-bin");
    let data_dir = temp_dir("wt-resume-state");
    let store_path = data_dir.join("metadata.jsonl");
    let worktree_root = data_dir.join("worktrees");
    let argv_log = temp_dir("wt-resume-argv").join("argv.log");
    write_executable(&bin_dir.join(bin_name), &stub_agent_script(&argv_log));

    let socket = temp_socket("wt-resume");
    let config = SessionRegistryConfig {
        store_path: Some(store_path.clone()),
        worktree_root: Some(worktree_root.clone()),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };

    let _path = PathGuard::prepend(&bin_dir).await;

    // --- Create phase: report a native id after the session commit. ---
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(&mut control, agent, repo.clone(), "feat/x").await,
    ))
    .expect("worktree stub session info");
    let worktree_path = created.worktree_path.clone().expect("worktree bound");
    assert_eq!(created.branch.as_deref(), Some("feat/x"));
    assert_eq!(created.cwd, worktree_path);

    let runtime_id = created
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.runtime_id.as_deref())
        .expect("created worktree session exposes its runtime id");
    let report_req = Request::make(
        "wt-resume-report-native-id",
        method::SESSION_REPORT_NATIVE_ID,
        serde_json::to_value(
            SessionReportNativeIdParams::new(
                created.id.clone(),
                runtime_id,
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
