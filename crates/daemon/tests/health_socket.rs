//! Integration test: the daemon's control server answers `daemon.health` over a
//! real Unix socket using newline-delimited JSON.
//!
//! This is the milestone-2 checkpoint ("CLI `doctor` + `daemon start` talk over
//! the socket") exercised at the protocol layer: it binds the actual
//! `ControlServer` on a temp socket, connects a raw client, and verifies the
//! response carries the daemon and protocol versions. It also covers
//! stale-socket recovery and the `method_not_found` path.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{
    event, method, AgentActivity, AgentKind, AttachHeader, ErrorClass, Event, Request, Response,
    SessionAttachParams, SessionAttachResult, SessionDetachParams, SessionDetachResult, SessionId,
    SessionInfo, SessionInputParams, SessionInputResult, SessionListFilter, SessionListParams,
    SessionNewParams, SessionResizeParams, SessionResizeResult, SessionState, SessionStopResult,
    StateSource, PROTOCOL_VERSION,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex, MutexGuard};
use tokio_util::codec::{Framed, LinesCodec};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_daemon::store::Store;

static PATH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    /// The rest of the suite runs concurrently and resolves a few tools through
    /// PATH (`git`, `python3`, `sh`); the isolated dir is seeded with symlinks to
    /// those, resolved from the current PATH, so replacing PATH cannot starve a
    /// sibling test of them. PATH is restored on drop; PATH_LOCK serializes this
    /// against the other PATH-mutating tests.
    async fn isolated_without_agents(tag: &str) -> Self {
        let guard = PATH_LOCK.lock().await;
        let old_path = std::env::var_os("PATH");
        let dir = temp_dir(tag);
        if let Some(old_path) = &old_path {
            for tool in ["git", "python3", "sh"] {
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
        .map(|d| d.as_nanos())
        .unwrap_or(0);
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
    let server = ControlServer::bind(socket, health)
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

/// Spawn the control server owning an explicit, pre-built registry.
///
/// Used by the resume round-trip test, which needs a handle to the registry to
/// call `load_and_resume` after a simulated restart.
async fn spawn_server_owned(
    socket: &std::path::Path,
    registry: SessionRegistry,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let state = DaemonState::new(HealthInfo::new("0.0.0"), registry);
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

/// Spawn the control server with a custom shell command.
async fn spawn_server_with_config(
    socket: &std::path::Path,
    version: &str,
    config: SessionRegistryConfig,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let state = DaemonState::new(HealthInfo::new(version), SessionRegistry::new(config));
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
async fn exchange(framed: &mut Framed<UnixStream, LinesCodec>, request: &Request) -> Response {
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
fn agent_name(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Shell => "shell",
        AgentKind::Codex => "codex",
        AgentKind::Claude => "claude",
    }
}

fn session_params() -> SessionNewParams {
    SessionNewParams {
        agent: "shell".to_owned(),
        cwd: Some(std::env::temp_dir()),
        cols: 80,
        rows: 24,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
    }
}

fn session_params_for_agent(agent: AgentKind, cwd: PathBuf) -> SessionNewParams {
    SessionNewParams {
        agent: agent_name(agent).to_owned(),
        cwd: Some(cwd),
        cols: 80,
        rows: 24,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
    }
}

/// `session.new` params binding a worktree for `repo` + `branch`.
fn session_params_for_worktree(agent: AgentKind, repo: PathBuf, branch: &str) -> SessionNewParams {
    SessionNewParams {
        agent: agent_name(agent).to_owned(),
        cwd: None,
        cols: 80,
        rows: 24,
        project: None,
        repo: Some(repo),
        branch: Some(branch.to_owned()),
        base_branch: None,
        input: None,
    }
}

/// Create a worktree-bound session and return the daemon's response.
async fn create_worktree_session(
    framed: &mut Framed<UnixStream, LinesCodec>,
    agent: AgentKind,
    repo: PathBuf,
    branch: &str,
) -> Response {
    let req = Request::new(
        "session-new-worktree",
        method::SESSION_NEW,
        serde_json::to_value(session_params_for_worktree(agent, repo, branch))
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
    match response {
        Response::Ok { ok, .. } => ok,
        Response::Err { err, .. } => panic!("expected ok, got error: {err}"),
    }
}

async fn create_session(framed: &mut Framed<UnixStream, LinesCodec>) -> SessionInfo {
    let req = Request::new(
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
    let req = Request::new(
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
    let req = Request::new(
        "session-new-agent",
        method::SESSION_NEW,
        serde_json::to_value(session_params_for_agent(agent, cwd)).expect("serialize params"),
    );
    exchange(framed, &req).await
}

async fn attach_session(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
) -> SessionAttachResult {
    let req = Request::new(
        "session-attach",
        method::SESSION_ATTACH,
        serde_json::to_value(SessionAttachParams {
            session_id: id.clone(),
            origin_session_id: None,
            origin_daemon_id: None,
        })
        .expect("serialize attach params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("attach result")
}

async fn detach_stream(
    framed: &mut Framed<UnixStream, LinesCodec>,
    stream_id: &str,
) -> SessionDetachResult {
    let req = Request::new(
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
    let req = Request::new(
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
    let req = Request::new(
        "session-input",
        method::SESSION_INPUT,
        serde_json::to_value(SessionInputParams {
            session_id: id.clone(),
            text: text.to_owned(),
        })
        .expect("serialize input params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("input result")
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
    let req = Request::new(
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
        if streamed.event == event::AGENT_STATE
            && streamed.payload["session_id"].as_str() == Some(id.0.as_str())
        {
            seen.push(streamed.payload.clone());
            if streamed.payload["activity"] == expected_activity
                && streamed.payload["source"] == expected_source
            {
                return streamed;
            }
        }
    }
}

/// Python reporter the stub agents run on launch to simulate the `SessionStart`
/// hook: it reads the daemon-injected handshake env and fires one
/// `session.report_native_id` RPC. Kept as a separate file (not a heredoc) so
/// the stub shell script stays trivial to template.
const STUB_REPORTER_PY: &str = r#"import json
import os
import socket

session_id = os.environ.get("POHUNEK_SESSION_ID")
socket_path = os.environ.get("POHUNEK_SOCKET_PATH")
protocol_raw = os.environ.get("POHUNEK_PROTOCOL_VERSION")
native = os.environ.get("POHUNEK_STUB_NATIVE_ID")
agent = os.environ.get("POHUNEK_STUB_AGENT")

if not (session_id and socket_path and protocol_raw and native and agent):
    raise SystemExit(0)

request = {
    "v": int(protocol_raw),
    "id": "stub-report",
    "method": "session.report_native_id",
    "params": {
        "session_id": session_id,
        "agent": agent,
        "native_session_id": native,
    },
}

try:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(1.0)
    client.connect(socket_path)
    client.sendall((json.dumps(request) + "\n").encode())
    try:
        client.recv(4096)
    except Exception:
        pass
    client.close()
except Exception:
    pass
"#;

/// Build a stub agent script that logs its argv (one line per launch), fires the
/// native-id report via [`STUB_REPORTER_PY`] using the injected handshake env,
/// then idles. The argv log lets the test assert the resume argv after restart.
fn stub_agent_script(argv_log: &Path, reporter_py: &Path, agent: &str, native_id: &str) -> String {
    format!(
        "#!/bin/sh\n\
printf '%s\\n' \"$*\" >> '{argv}'\n\
if [ \"${{POHUNEK_ENV:-}}\" = \"1\" ] && command -v python3 >/dev/null 2>&1; then\n\
  POHUNEK_STUB_NATIVE_ID='{native}' POHUNEK_STUB_AGENT='{agent}' python3 '{reporter}' || true\n\
fi\n\
/bin/sleep 30\n",
        argv = argv_log.display(),
        native = native_id,
        agent = agent,
        reporter = reporter_py.display(),
    )
}

/// Poll `inspect` until the session reports the expected captured native id.
async fn wait_for_native_id(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
    native_id: &str,
) -> SessionInfo {
    for _ in 0..250 {
        let info = inspect_session(framed, id).await;
        if info.native_session_id.as_deref() == Some(native_id) {
            return info;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "native id {native_id} was not captured for session {}",
        id.0
    );
}

/// End-to-end: a stub agent installs (simulated) its hook by reporting a native
/// id on launch; the binding survives a simulated daemon kill + registry
/// rebuild against the same state dir; the relaunched stub receives the resume
/// argv. Exercised for both Claude (`--resume <id>`) and Codex (`resume <id>`).
async fn assert_resume_round_trip(
    agent: AgentKind,
    bin_name: &str,
    native_id: &str,
    expected_resume_argv: &str,
) {
    let bin_dir = temp_dir(&format!("{bin_name}-resume-bin"));
    let cwd = temp_dir(&format!("{bin_name}-resume-cwd"));
    let state_dir = temp_dir(&format!("{bin_name}-resume-state"));
    let store_path = state_dir.join("metadata.jsonl");
    let argv_log = temp_dir(&format!("{bin_name}-resume-argv")).join("argv.log");
    let reporter_py = temp_dir(&format!("{bin_name}-resume-reporter")).join("reporter.py");
    write_executable(&reporter_py, STUB_REPORTER_PY);
    write_executable(
        &bin_dir.join(bin_name),
        &stub_agent_script(&argv_log, &reporter_py, bin_name, native_id),
    );

    let socket = temp_socket(&format!("{bin_name}-resume"));
    let make_config = || SessionRegistryConfig {
        socket_path: Some(socket.clone()),
        store_path: Some(store_path.clone()),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };

    // Hold the PATH override across both the create and resume phases so the
    // stub binary resolves at launch and at resume.
    let _path = PathGuard::prepend(&bin_dir).await;

    // --- Create phase: launch the agent; its hook reports a native id. ---
    let registry = SessionRegistry::new(make_config());
    let (shutdown, handle) = spawn_server_owned(&socket, registry).await;
    let mut control = connect(&socket).await;
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_session_with_agent(&mut control, agent, cwd.clone()).await,
    ))
    .expect("stub session info");
    assert_eq!(created.agent, agent_name(agent));

    // The stub fires the report RPC (proving the handshake env reached it); wait
    // until the daemon records the native id on the session.
    let captured = wait_for_native_id(&mut control, &created.id, native_id).await;
    assert_eq!(captured.native_session_id.as_deref(), Some(native_id));

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;

    // --- The binding survived the kill: persisted to the resume store. ---
    assert!(
        store_path.is_file(),
        "resume store must persist across the daemon kill"
    );

    // --- Restart: rebuild a fresh registry against the SAME state dir. ---
    let registry = SessionRegistry::new(make_config());
    let (shutdown, handle) = spawn_server_owned(&socket, registry.clone()).await;
    registry.load_and_resume().await;

    // The relaunched stub logged the resume argv built by the M6 builder.
    let logged = read_file_until(&argv_log, expected_resume_argv.as_bytes()).await;
    assert!(
        logged
            .windows(expected_resume_argv.len())
            .any(|window| window == expected_resume_argv.as_bytes()),
        "resumed stub did not receive resume argv {expected_resume_argv:?}; argv log: {}",
        String::from_utf8_lossy(&logged)
    );

    let stop_req = Request::new(
        "session-stop-resume",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let mut cleanup = connect(&socket).await;
    let _ = exchange(&mut cleanup, &stop_req).await;
    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn claude_session_captures_native_id_and_resumes_after_restart() {
    assert_resume_round_trip(
        AgentKind::Claude,
        "claude",
        "native-claude-1",
        "--resume native-claude-1",
    )
    .await;
}

#[tokio::test]
async fn codex_session_captures_native_id_and_resumes_after_restart() {
    assert_resume_round_trip(
        AgentKind::Codex,
        "codex",
        "native-codex-1",
        "resume native-codex-1",
    )
    .await;
}

#[tokio::test]
async fn health_returns_versions() {
    let socket = temp_socket("health");
    let (shutdown, handle) = spawn_server(&socket, "9.9.9-test").await;

    let mut client = connect(&socket).await;
    let req = Request::new("t-1", method::DAEMON_HEALTH, Value::Null);
    let resp = exchange(&mut client, &req).await;

    match resp {
        Response::Ok { v, id, ok } => {
            assert_eq!(v, PROTOCOL_VERSION);
            assert_eq!(id, "t-1");
            assert_eq!(ok["status"], Value::from("ok"));
            assert_eq!(ok["daemon_version"], Value::from("9.9.9-test"));
            assert_eq!(ok["protocol_version"], Value::from(PROTOCOL_VERSION.get()));
        }
        Response::Err { err, .. } => panic!("expected ok, got error: {err}"),
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn unknown_method_returns_typed_error() {
    let socket = temp_socket("unknown");
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;

    let mut client = connect(&socket).await;
    let req = Request::new("t-2", "no.such.method", Value::Null);
    let resp = exchange(&mut client, &req).await;

    match resp {
        Response::Err { id, err, .. } => {
            assert_eq!(id, "t-2");
            assert_eq!(err.code, "method_not_found");
        }
        Response::Ok { .. } => panic!("expected an error for an unknown method"),
    }

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
    let unknown_key = Request::new(
        "bad-filter-key",
        method::SESSION_LIST,
        serde_json::json!({ "filters": [{ "key": "cwd", "value": "/workspace" }] }),
    );
    match exchange(&mut client, &unknown_key).await {
        Response::Err { id, err, .. } => {
            assert_eq!(id, "bad-filter-key");
            assert_eq!(err.code, "bad_request");
        }
        Response::Ok { ok, .. } => {
            panic!("an unknown filter key must be a typed usage error, got ok: {ok:?}")
        }
    }

    // Known key, value outside the closed state enum.
    let bad_value = Request::new(
        "bad-filter-value",
        method::SESSION_LIST,
        serde_json::json!({ "filters": [{ "key": "state", "value": "paused" }] }),
    );
    match exchange(&mut client, &bad_value).await {
        Response::Err { id, err, .. } => {
            assert_eq!(id, "bad-filter-value");
            assert_eq!(err.code, "bad_request");
        }
        Response::Ok { ok, .. } => {
            panic!("an out-of-range filter value must be a typed usage error, got ok: {ok:?}")
        }
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn attach_reporting_its_own_session_as_origin_is_rejected_over_the_socket() {
    // End-to-end: the CLI sets `origin_session_id`/`origin_daemon_id` from the
    // PTY env when it runs inside a session. An attach whose origin matches the
    // target session AND this daemon instance would loop the PTY's output into its
    // own input, so the daemon must reject it at the wire boundary with a typed,
    // stable error — proving the handler threads the origin through to the guard
    // (not just the unit path). Own the registry so the test knows the instance id
    // the CLI would have inherited via POHUNEK_DAEMON_ID.
    let socket = temp_socket("attach-self-feedback");
    let registry = SessionRegistry::new(SessionRegistryConfig::default());
    let daemon_id = registry.daemon_instance_id().to_owned();
    let (shutdown, handle) = spawn_server_owned(&socket, registry).await;
    let mut control = connect(&socket).await;

    let created = create_session(&mut control).await;

    let self_attach = Request::new(
        "attach-self",
        method::SESSION_ATTACH,
        serde_json::json!({
            "session_id": created.id,
            "origin_session_id": created.id,
            "origin_daemon_id": daemon_id,
        }),
    );
    match exchange(&mut control, &self_attach).await {
        Response::Err { id, err, .. } => {
            assert_eq!(id, "attach-self");
            assert_eq!(err.code, "attach_self_feedback");
            assert!(
                err.recover.is_some(),
                "self-feedback error must carry a recovery hint: {err:?}"
            );
        }
        Response::Ok { ok, .. } => {
            panic!("a self-feeding attach must be a typed error, got ok: {ok:?}")
        }
    }

    // Same session id reported from a DIFFERENT daemon instance (a colliding id or
    // a stale env from a prior process): no loop, so it must be accepted.
    let other_daemon = Request::new(
        "attach-other-daemon",
        method::SESSION_ATTACH,
        serde_json::json!({
            "session_id": created.id,
            "origin_session_id": created.id,
            "origin_daemon_id": "some-other-daemon",
        }),
    );
    let ok = ok_payload(exchange(&mut control, &other_daemon).await);
    assert!(
        ok.get("stream_id").and_then(Value::as_str).is_some(),
        "a matching id on a different daemon instance must still attach: {ok:?}"
    );

    // An attach from a different terminal (no origin reported) still works.
    let plain_attach = Request::new(
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

    match resp {
        Response::Err { err, .. } => {
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
        }
        Response::Ok { .. } => panic!("expected agent_binary_missing error, got ok"),
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn stale_socket_is_recovered_on_bind() {
    let socket = temp_socket("stale");
    // Create a stale socket file with no listener behind it.
    {
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind stale");
        drop(listener);
    }
    assert!(socket.exists(), "stale socket file should exist");

    // Binding again must succeed by removing the stale socket.
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;

    let mut client = connect(&socket).await;
    let req = Request::new("t-3", method::DAEMON_HEALTH, Value::Null);
    let resp = exchange(&mut client, &req).await;
    assert!(
        matches!(resp, Response::Ok { .. }),
        "health works after recovery"
    );

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

    let list_req = Request::new("session-list", method::SESSION_LIST, Value::Null);
    let list: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut client, &list_req).await))
            .expect("session list");
    assert!(
        list.iter()
            .any(|session| session.id == created.id && session.state == SessionState::Running),
        "created session should appear in list: {list:?}"
    );

    let second = create_session(&mut client).await;
    let filtered_list_req = Request::new(
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

    let stop_req = Request::new(
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
        Request::new("session-list-after-stop", method::SESSION_LIST, Value::Null);
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
    let stop_second_req = Request::new(
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
        "input output should be replayed to attach stream: {output:?}"
    );

    let _ = detach_stream(&mut client, &attach.stream_id).await;
    let stop_req = Request::new(
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
        "create-time input output should be replayed to attach stream: {output:?}"
    );

    let _ = detach_stream(&mut client, &attach.stream_id).await;
    let stop_req = Request::new(
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
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::new("subscribe-codex-stub", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(matches!(ack, Response::Ok { .. }), "subscribe should ack");

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
    assert_eq!(streamed.payload["activity"], Value::from("blocked"));
    assert_eq!(streamed.payload["source"], Value::from("osc_title"));

    let input = input_session(&mut control, &created.id, "run tests").await;
    assert!(input.accepted);
    let bytes = read_file_until(&input_log, b"\x1b[200~run tests\x1b[201~").await;
    assert_eq!(bytes, b"\x1b[200~run tests\x1b[201~");

    let launched_cwd = tokio::fs::read_to_string(&cwd_log)
        .await
        .expect("read cwd log");
    assert_eq!(launched_cwd.trim(), cwd.display().to_string());

    let stop_req = Request::new(
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
    let (shutdown, handle) = spawn_server(&socket, "0.0.0").await;

    let mut subscriber = connect(&socket).await;
    let subscribe_req = Request::new("subscribe-claude-stub", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(matches!(ack, Response::Ok { .. }), "subscribe should ack");

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
    assert_eq!(streamed.payload["activity"], Value::from("blocked"));
    assert_eq!(streamed.payload["source"], Value::from("screen"));

    let input = input_session(&mut control, &created.id, "hello Claude").await;
    assert!(input.accepted);
    let bytes = read_file_until(&input_log, b"hello Claude").await;
    assert_eq!(bytes, b"hello Claude");

    let stop_req = Request::new(
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
    let list_req = Request::new("session-list-fresh", method::SESSION_LIST, Value::Null);
    let list: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut fresh_client, &list_req).await))
            .expect("session list");
    assert!(
        list.iter()
            .any(|session| session.id == created.id && session.state == SessionState::Running),
        "fresh client should see daemon-owned session: {list:?}"
    );

    let stop_req = Request::new(
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
    let subscribe_req = Request::new("subscribe-1", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    match ack {
        Response::Ok { id, ok, .. } => {
            assert_eq!(id, "subscribe-1");
            assert_eq!(ok["subscribed"], Value::from(true));
        }
        Response::Err { err, .. } => panic!("expected subscribe ack, got error: {err}"),
    }

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

    assert_eq!(streamed.event, event::SESSION_CREATED);
    assert_eq!(
        streamed.payload["session"]["id"],
        Value::from(created.id.0.as_str()),
        "streamed event should carry the created session id"
    );

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

    let stop_req = Request::new(
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
async fn reattach_replays_recent_output_history() {
    let socket = temp_socket("attach-history-replay");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };
    let (shutdown, handle) = spawn_server_with_config(&socket, "0.0.0", config).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control).await;

    // First attach: produce output that should later be replayed on reattach.
    let first = attach_session(&mut control, &created.id).await;
    let mut raw = open_attach_stream(&socket, &first.stream_id).await;
    raw.write_all(b"printf 'HISTORY-REPLAY-MARK\\n'\n")
        .await
        .expect("send history marker command");
    let live_output = read_until_marker(&mut raw, b"HISTORY-REPLAY-MARK").await;
    assert!(
        live_output
            .windows(b"HISTORY-REPLAY-MARK".len())
            .any(|window| window == b"HISTORY-REPLAY-MARK"),
        "first attach should receive live output: {}",
        String::from_utf8_lossy(&live_output)
    );

    // Detach the first stream and confirm it closes.
    let detached = detach_stream(&mut control, &first.stream_id).await;
    assert!(detached.detached);
    assert_raw_stream_closes(&mut raw).await;

    // Reattach WITHOUT sending any input. The marker produced before detach
    // must arrive purely from the replayed history buffer.
    let second = attach_session(&mut control, &created.id).await;
    assert_ne!(second.stream_id, first.stream_id);
    let mut raw_again = open_attach_stream(&socket, &second.stream_id).await;
    let replayed = read_until_marker(&mut raw_again, b"HISTORY-REPLAY-MARK").await;
    assert!(
        replayed
            .windows(b"HISTORY-REPLAY-MARK".len())
            .any(|window| window == b"HISTORY-REPLAY-MARK"),
        "reattach should replay prior output without new input: {}",
        String::from_utf8_lossy(&replayed)
    );

    let stop_req = Request::new(
        "session-stop-after-history-replay",
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

    let stopped = Request::new(
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
    let list_req = Request::new("list-after-attach-drop", method::SESSION_LIST, Value::Null);
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

    let stop_req = Request::new(
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
    let subscribe_req = Request::new("subscribe-detector", method::SUBSCRIBE, Value::Null);
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(matches!(ack, Response::Ok { .. }), "subscribe should ack");

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
    assert_eq!(streamed.payload["activity"], Value::from("working"));
    assert_eq!(streamed.payload["source"], Value::from("osc_title"));

    let inspected = inspect_session(&mut control, &created.id).await;
    assert_eq!(inspected.activity, Some(AgentActivity::Working));
    assert_eq!(inspected.state_source, StateSource::OscTitle);

    let stop_req = Request::new(
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
    let subscribe_req = Request::new(
        "subscribe-detector-debounce",
        method::SUBSCRIBE,
        Value::Null,
    );
    let ack = exchange(&mut subscriber, &subscribe_req).await;
    assert!(matches!(ack, Response::Ok { .. }), "subscribe should ack");

    let mut control = connect(&socket).await;
    let created = create_session(&mut control).await;

    let streamed = wait_for_agent_state_event(
        &mut subscriber,
        &created.id,
        AgentActivity::Blocked,
        StateSource::OscTitle,
    )
    .await;
    assert_eq!(streamed.payload["activity"], Value::from("blocked"));
    assert_eq!(streamed.payload["source"], Value::from("osc_title"));

    let inspected = inspect_session(&mut control, &created.id).await;
    assert_eq!(inspected.activity, Some(AgentActivity::Blocked));
    assert_eq!(inspected.state_source, StateSource::OscTitle);

    let stop_req = Request::new(
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
        let stop_req = Request::new(
            "session-stop-worktree",
            method::SESSION_STOP,
            serde_json::to_value(id).expect("serialize id"),
        );
        let _: SessionStopResult =
            serde_json::from_value(ok_payload(exchange(&mut control, &stop_req).await))
                .expect("stop result");
    }

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
    let registry = SessionRegistry::new(config);
    registry.spawn_event_log().expect("start event log");
    let (shutdown, handle) = spawn_server_owned(&socket, registry).await;

    let mut control = connect(&socket).await;
    let created = create_session(&mut control).await;
    // Drive a second lifecycle event, then stop to produce session_stopped.
    let _ = resize_session(&mut control, &created.id, 100, 40).await;
    let stop_req = Request::new(
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

/// Milestone-9 checkpoint (unified store survives a restart): a worktree-bound
/// stub agent records BOTH a worktree binding and a resume binding in one
/// metadata file; after a simulated daemon kill + registry rebuild against the
/// same data dir, both bindings survive, the session relaunches via its resume
/// argv, and its worktree metadata (repo/branch/worktree_path) is restored.
#[tokio::test]
async fn worktree_session_resumes_with_metadata_after_restart() {
    let agent = AgentKind::Claude;
    let bin_name = "claude";
    let native_id = "native-claude-wt-1";
    let expected_resume_argv = "--resume native-claude-wt-1";

    let repo = init_git_repo("wt-resume-repo");
    let bin_dir = temp_dir("wt-resume-bin");
    let data_dir = temp_dir("wt-resume-state");
    let store_path = data_dir.join("metadata.jsonl");
    let worktree_root = data_dir.join("worktrees");
    let argv_log = temp_dir("wt-resume-argv").join("argv.log");
    let reporter_py = temp_dir("wt-resume-reporter").join("reporter.py");
    write_executable(&reporter_py, STUB_REPORTER_PY);
    write_executable(
        &bin_dir.join(bin_name),
        &stub_agent_script(&argv_log, &reporter_py, bin_name, native_id),
    );

    let socket = temp_socket("wt-resume");
    let make_config = || SessionRegistryConfig {
        socket_path: Some(socket.clone()),
        store_path: Some(store_path.clone()),
        worktree_root: Some(worktree_root.clone()),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    };

    let _path = PathGuard::prepend(&bin_dir).await;

    // --- Create phase: a worktree-bound stub agent reports its native id. ---
    let registry = SessionRegistry::new(make_config());
    let (shutdown, handle) = spawn_server_owned(&socket, registry).await;
    let mut control = connect(&socket).await;
    let created: SessionInfo = serde_json::from_value(ok_payload(
        create_worktree_session(&mut control, agent, repo.clone(), "feat/x").await,
    ))
    .expect("worktree stub session info");
    let worktree_path = created.worktree_path.clone().expect("worktree bound");
    assert_eq!(created.branch.as_deref(), Some("feat/x"));
    assert_eq!(created.cwd, worktree_path);

    let captured = wait_for_native_id(&mut control, &created.id, native_id).await;
    assert_eq!(captured.native_session_id.as_deref(), Some(native_id));

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;

    // --- Both records survived the kill in ONE unified metadata file. ---
    let store = Store::new(store_path.clone());
    let resume_before = store.load_resume().expect("load resume");
    let worktrees_before = store.load_worktrees().expect("load worktrees");
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
    let repository = worktrees_before[0].repository.clone();

    // --- Restart: rebuild a fresh registry against the SAME data dir. ---
    let registry = SessionRegistry::new(make_config());
    let (shutdown, handle) = spawn_server_owned(&socket, registry.clone()).await;
    registry.load_and_resume().await;

    // The relaunched stub logged the resume argv built by the M6 builder.
    let logged = read_file_until(&argv_log, expected_resume_argv.as_bytes()).await;
    assert!(
        logged
            .windows(expected_resume_argv.len())
            .any(|window| window == expected_resume_argv.as_bytes()),
        "resumed stub did not receive resume argv {expected_resume_argv:?}; argv log: {}",
        String::from_utf8_lossy(&logged)
    );

    // The resumed session restored its worktree metadata from the unified store.
    let mut control = connect(&socket).await;
    let resumed = wait_for_state(&mut control, &created.id, SessionState::Running).await;
    assert_eq!(
        resumed.worktree_path.as_deref(),
        Some(worktree_path.as_path())
    );
    assert_eq!(resumed.branch.as_deref(), Some("feat/x"));
    assert_eq!(resumed.repo.as_deref(), Some(repository.as_path()));
    assert_eq!(resumed.native_session_id.as_deref(), Some(native_id));

    // Both bindings still present after the restart.
    assert_eq!(store.load_resume().expect("resume after").len(), 1);
    assert_eq!(store.load_worktrees().expect("worktrees after").len(), 1);

    let stop_req = Request::new(
        "session-stop-wt-resume",
        method::SESSION_STOP,
        serde_json::to_value(&created.id).expect("serialize id"),
    );
    let _ = exchange(&mut control, &stop_req).await;
    let _ = shutdown.send(());
    let _ = handle.await;
}
