//! Integration test: the daemon's control server answers over a `NetBird` TCP
//! connection with identical protocol and attach semantics to the local Unix
//! socket (milestone 11 "Remote hosts over `NetBird`").
//!
//! A real `TcpListener::bind("127.0.0.1:0")` stands in for the `NetBird` interface
//! (loopback wrapping skips the fail-closed `NetBird` validation via
//! `RemoteServer::from_listener`; the validation itself is asserted separately by
//! `bind_rejects_non_netbird_address`). A `ControlServer` (Unix) is bound on the
//! SAME `DaemonState` so a session created over TCP is daemon-owned and survives
//! a detach. The cases below cover the full lifecycle over TCP, attach/detach
//! over TCP, `host.inspect` over TCP, cross-transport payload parity, and the
//! fail-closed bind.

#[path = "support/official_runtimes.rs"]
mod official_runtimes;
mod support;

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{
    method, AttachHeader, HostCapabilities, Request as ProtocolRequest, Response,
    SessionAttachParams, SessionAttachResult, SessionDetachParams, SessionDetachResult, SessionId,
    SessionInfo, SessionInputParams, SessionInputResult, SessionListFilter, SessionListParams,
    SessionNewParams, SessionState, SessionStopResult, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo, RemoteServer};
use pohunek_daemon::error::DaemonError;
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::procwatch::{HostInspector, ProcessInspector};
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_test_support::worker_binary;

struct Request;

impl Request {
    fn make(id: &str, method: &str, params: Value) -> ProtocolRequest {
        ProtocolRequest::new(id, method, params).expect("valid test request")
    }
}

// Sessions use the production stop grace, `SessionRegistryConfig::default()`:
// a shorter grace reaches the forced-stop path on a loaded host, and no test
// here is about the grace.

/// Directory name, next to the socket, that sessions of a fixture work in.
const WORK_DIR_NAME: &str = "work";

/// The working directory of the sessions served next to `socket`.
fn work_dir(socket: &Path) -> PathBuf {
    socket.with_file_name(WORK_DIR_NAME)
}

/// Creates the private directory a fixture binds its Unix socket in.
///
/// The Unix server enforces its directory's mode on bind, so the socket must
/// live in a directory we own (not `/tmp` itself). The directory is removed
/// when the returned guard drops.
fn temp_dir(tag: &str) -> tempfile::TempDir {
    pohunek_test_support::tempdir_with_prefix(&format!("ph-{tag}-"))
        .expect("create the socket directory")
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

/// Build a shell session config with bounded test stop grace.
fn shell_config() -> SessionRegistryConfig {
    SessionRegistryConfig {
        shell_command: support::hermetic_shell(),
        ..SessionRegistryConfig::default()
    }
}

/// Bind both a TCP `RemoteServer` (loopback stand-in) and a Unix `ControlServer`
/// on the SAME shared state, returning the TCP address, the Unix socket path, a
/// combined shutdown trigger, and the joined server task.
async fn spawn_dual_servers(
    tag: &str,
    version: &str,
    config: SessionRegistryConfig,
) -> (
    SocketAddr,
    PathBuf,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    spawn_dual_servers_with(tag, version, config, false).await
}

/// [`spawn_dual_servers`] whose registry is the production one over a plugin
/// root next to the socket, holding the official Codex, Claude and Hermes
/// packages installed through a catalog its throwaway trust anchor
/// authorizes.
async fn spawn_dual_servers_serving_official_runtimes(
    tag: &str,
    version: &str,
    config: SessionRegistryConfig,
) -> (
    SocketAddr,
    PathBuf,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    spawn_dual_servers_with(tag, version, config, true).await
}

async fn spawn_dual_servers_with(
    tag: &str,
    version: &str,
    mut config: SessionRegistryConfig,
    official_runtimes: bool,
) -> (
    SocketAddr,
    PathBuf,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let socket_dir = temp_dir(tag);
    std::fs::create_dir(socket_dir.path().join(WORK_DIR_NAME))
        .expect("create the session working directory");
    let socket = socket_dir.path().join("daemon.sock");
    // Short, because the worker nests its control socket below it.
    let worker_home_dir =
        pohunek_test_support::tempdir_with_prefix("pw-r-").expect("create the worker home");
    let worker_home = worker_home_dir.path().to_path_buf();
    let worker_environment = SubprocessWorkerEnvironment {
        runtime_home: worker_home.join("runtime"),
        state_home: worker_home.join("state"),
        data_home: worker_home.join("data"),
        config_home: worker_home.join("config"),
        cache_home: worker_home.join("cache"),
        home: worker_home.clone(),
        daemon_socket: socket.clone(),
    };
    config.socket_path = Some(socket.clone());
    config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
    config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
    config.supervision = Some(
        worker_environment
            .supervision(worker_binary())
            .with_environment_source(support::hermetic_environment_source()),
    );
    let registry = if official_runtimes {
        let catalog = official_runtimes::OfficialCatalog::new();
        let config = catalog.configure(config, socket_dir.path().join("plugins"));
        let registry = SessionRegistry::new_production_with_inspector(
            config,
            Arc::new(SubprocessWorkerLauncher::new()),
            Arc::new(HostInspector::new()),
        )
        .expect("the production registry opens the plugin root");
        catalog.install(&registry, socket_dir.path()).await;
        registry
    } else {
        SessionRegistry::new_with_launcher_and_inspector(
            config,
            Arc::new(SubprocessWorkerLauncher::new()),
            Arc::new(HostInspector::new()),
        )
    };
    let state = DaemonState::new(
        HealthInfo::new(version),
        registry,
        governance_service(&socket).await,
        support::overlay_registry(),
        pohunek_daemon::api::OverlaySource::Default,
    );
    let remote_state = state.clone();
    assert!(Arc::ptr_eq(&state.governance, &remote_state.governance));
    let unix = ControlServer::bind_with_state(&socket, state)
        .await
        .expect("unix server binds");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback tcp bind");
    let remote = RemoteServer::from_listener(listener, remote_state);
    let addr = remote.local_addr();

    let (tx, rx) = oneshot::channel::<()>();
    let (unix_rx, remote_rx) = oneshot_fanout(rx);
    let handle = tokio::spawn(async move {
        // Removed after both servers and the registry are gone.
        let _fixture_dirs = (socket_dir, worker_home_dir);
        let unix_serve = unix.serve(async move {
            let _ = unix_rx.await;
        });
        let remote_serve = remote.serve(async move {
            let _ = remote_rx.await;
        });
        tokio::join!(unix_serve, remote_serve);
    });
    (addr, socket, tx, handle)
}
/// Fan one shutdown receiver out to two, mirroring the daemon binary's wiring.
fn oneshot_fanout(rx: oneshot::Receiver<()>) -> (oneshot::Receiver<()>, oneshot::Receiver<()>) {
    let (a_tx, a_rx) = oneshot::channel::<()>();
    let (b_tx, b_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = rx.await;
        let _ = a_tx.send(());
        let _ = b_tx.send(());
    });
    (a_rx, b_rx)
}

/// Connect a line-framed client over TCP.
async fn connect_tcp(addr: SocketAddr) -> Framed<TcpStream, LinesCodec> {
    for _ in 0..50 {
        if let Ok(stream) = TcpStream::connect(addr).await {
            return Framed::new(stream, LinesCodec::new());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to test tcp addr {addr}");
}

/// Connect a line-framed client over the Unix socket.
async fn connect_unix(socket: &std::path::Path) -> Framed<UnixStream, LinesCodec> {
    for _ in 0..50 {
        if let Ok(stream) = UnixStream::connect(socket).await {
            return Framed::new(stream, LinesCodec::new());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("could not connect to test socket {}", socket.display());
}

/// Open a raw (unframed) TCP attach stream and send the attach prelude line.
async fn open_attach_stream_tcp(addr: SocketAddr, stream_id: &str) -> TcpStream {
    let mut raw = TcpStream::connect(addr).await.expect("raw tcp connect");
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

/// Send a request line over a generic line-framed client and read one response.
async fn exchange<S>(framed: &mut Framed<S, LinesCodec>, request: &ProtocolRequest) -> Response
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let line = serde_json::to_string(request).expect("serialize request");
    framed.send(line).await.expect("send");
    let reply = framed
        .next()
        .await
        .expect("a response line")
        .expect("response framing ok");
    serde_json::from_str(&reply).expect("parse response")
}

fn ok_payload(response: Response) -> Value {
    response
        .into_result()
        .unwrap_or_else(|error| panic!("expected ok, got error: {error}"))
}

fn session_params(socket: &Path) -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: "shell".to_owned(),
        cwd: Some(work_dir(socket)),
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

async fn create_session<S>(framed: &mut Framed<S, LinesCodec>, socket: &Path) -> SessionInfo
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let req = Request::make(
        "session-new",
        method::SESSION_NEW,
        serde_json::to_value(session_params(socket)).expect("serialize params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("session info")
}

async fn create_session_with_params<S>(
    framed: &mut Framed<S, LinesCodec>,
    params: SessionNewParams,
) -> Response
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let req = Request::make(
        "session-new-custom",
        method::SESSION_NEW,
        serde_json::to_value(params).expect("serialize params"),
    );
    exchange(framed, &req).await
}

async fn inspect_session<S>(framed: &mut Framed<S, LinesCodec>, id: &SessionId) -> SessionInfo
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let req = Request::make(
        "session-inspect",
        method::SESSION_INSPECT,
        serde_json::to_value(id).expect("serialize id"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("session info")
}

async fn attach_session<S>(
    framed: &mut Framed<S, LinesCodec>,
    id: &SessionId,
) -> SessionAttachResult
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let req = Request::make(
        "session-attach",
        method::SESSION_ATTACH,
        serde_json::to_value(SessionAttachParams {
            session_id: id.clone(),
            initial_dimensions: None,
            origin_session_id: None,
            origin_daemon_id: None,
            origin_worker_id: None,
        })
        .expect("serialize attach params"),
    );
    serde_json::from_value(ok_payload(exchange(framed, &req).await)).expect("attach result")
}

async fn detach_stream<S>(
    framed: &mut Framed<S, LinesCodec>,
    stream_id: &str,
) -> SessionDetachResult
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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

async fn input_session<S>(
    framed: &mut Framed<S, LinesCodec>,
    id: &SessionId,
    text: &str,
) -> SessionInputResult
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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

/// Read from a raw TCP stream until `marker` appears or a timeout elapses.
async fn read_until_marker(stream: &mut TcpStream, marker: &[u8]) -> Vec<u8> {
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

/// Drain a raw TCP stream until it closes (read returns 0) or a timeout elapses.
async fn assert_raw_stream_closes(stream: &mut TcpStream) {
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

#[tokio::test]
async fn session_lifecycle_over_tcp() {
    let (addr, socket, shutdown, handle) =
        spawn_dual_servers("remote-lifecycle", "0.0.0", shell_config()).await;

    let mut client = connect_tcp(addr).await;
    let created = create_session(&mut client, &socket).await;
    assert_eq!(created.agent, "shell");
    assert_eq!(created.state, SessionState::Running);
    assert!(created.pid > 0);

    let list_req = Request::make("session-list", method::SESSION_LIST, Value::Null);
    let list: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut client, &list_req).await))
            .expect("session list");
    assert!(
        list.iter()
            .any(|session| session.id == created.id && session.state == SessionState::Running),
        "created session should appear in list over TCP: {list:?}"
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
        "TCP filtered list must return only the exact AND match, not {second:?}: {filtered:?}"
    );

    let inspected = inspect_session(&mut client, &created.id).await;
    assert_eq!(inspected.id, created.id);
    assert_eq!(inspected.pid, created.pid);

    let input = input_session(&mut client, &created.id, "echo over tcp").await;
    assert!(input.accepted);

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
async fn session_new_with_input_over_tcp_writes_text_to_shell_pty() {
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            [
                "-c",
                "IFS= read -r line; printf 'got:%s\\n' \"$line\"; /bin/sleep 30",
            ],
        ),
        ..SessionRegistryConfig::default()
    };
    let (addr, socket, shutdown, handle) =
        spawn_dual_servers("remote-session-new-input", "0.0.0", config).await;

    let mut control = connect_tcp(addr).await;
    let mut params = session_params(&socket);
    params.input = Some("hello from tcp create".to_owned());
    let ok = ok_payload(create_session_with_params(&mut control, params).await);
    assert!(
        !ok.as_object()
            .expect("session.new response object")
            .contains_key("accepted"),
        "session.new must keep returning SessionInfo, not SessionInputResult: {ok}"
    );
    let created: SessionInfo = serde_json::from_value(ok).expect("session info");
    assert_eq!(created.agent, "shell");
    assert_eq!(created.state, SessionState::Running);

    let attach = attach_session(&mut control, &created.id).await;
    let mut raw = open_attach_stream_tcp(addr, &attach.stream_id).await;
    let output = read_until_marker(&mut raw, b"got:hello from tcp create").await;
    assert!(
        output
            .windows(b"got:hello from tcp create".len())
            .any(|window| window == b"got:hello from tcp create"),
        "create-time input should reach a TCP-created session: {}",
        String::from_utf8_lossy(&output)
    );

    let detached = detach_stream(&mut control, &attach.stream_id).await;
    assert!(detached.detached);
    let stop_req = Request::make(
        "session-new-input-stop",
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
async fn attach_over_tcp_round_trips_and_detach_keeps_session_running() {
    let (addr, socket, shutdown, handle) =
        spawn_dual_servers("remote-attach", "0.0.0", shell_config()).await;

    let mut control = connect_tcp(addr).await;
    let created = create_session(&mut control, &socket).await;

    let attach = attach_session(&mut control, &created.id).await;
    assert!(!attach.stream_id.is_empty());

    // Second TCP connection carries the attach byte stream.
    let mut raw = open_attach_stream_tcp(addr, &attach.stream_id).await;
    raw.write_all(b"printf 'remote-attach-mark\\n'\n")
        .await
        .expect("write input over raw attach stream");
    let output = read_until_marker(&mut raw, b"remote-attach-mark").await;
    assert!(
        output
            .windows(b"remote-attach-mark".len())
            .any(|window| window == b"remote-attach-mark"),
        "attach stream over TCP should receive live PTY output: {}",
        String::from_utf8_lossy(&output)
    );

    // Detach over the control connection; the raw stream must close, but the
    // daemon-owned session must keep running (detach != stop).
    let detached = detach_stream(&mut control, &attach.stream_id).await;
    assert!(detached.detached);
    assert_raw_stream_closes(&mut raw).await;

    let survived = inspect_session(&mut control, &created.id).await;
    assert_eq!(
        survived.state,
        SessionState::Running,
        "detach must not kill the daemon-owned process"
    );

    let stop_req = Request::make(
        "session-stop-after-attach",
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
async fn host_inspect_over_tcp_returns_capabilities() {
    let (addr, _socket, shutdown, handle) = spawn_dual_servers_serving_official_runtimes(
        "remote-inspect",
        "7.7.7-test",
        shell_config(),
    )
    .await;

    let mut client = connect_tcp(addr).await;
    let req = Request::make("host-inspect", method::HOST_INSPECT, Value::Null);
    let caps: HostCapabilities =
        serde_json::from_value(ok_payload(exchange(&mut client, &req).await))
            .expect("host capabilities");

    assert_eq!(caps.daemon_version, "7.7.7-test");
    assert_eq!(caps.protocol_version, PROTOCOL_VERSION);
    assert_eq!(
        caps.supported_agents,
        vec!["shell", "codex", "claude", "hermes", "pi"]
    );
    assert_eq!(caps.worktree_supported, caps.git_available);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn host_governance_inspect_has_identical_unix_and_tcp_dispatch() {
    let (addr, socket, shutdown, handle) =
        spawn_dual_servers("remote-governance-inspect", "0.0.0", shell_config()).await;

    let mut tcp_client = connect_tcp(addr).await;
    let mut unix_client = connect_unix(&socket).await;
    let request = Request::make(
        "governance-inspect",
        method::HOST_GOVERNANCE_INSPECT,
        Value::Null,
    );
    let tcp_payload = ok_payload(exchange(&mut tcp_client, &request).await);
    let unix_payload = ok_payload(exchange(&mut unix_client, &request).await);

    assert_eq!(
        tcp_payload, unix_payload,
        "both owner transports must read the same shared governance service"
    );
    assert!(tcp_payload["host_id"].is_string());
    assert!(tcp_payload["approval_key_reference"].is_string());
    assert_eq!(tcp_payload["enrollment"], Value::Null);
    assert_eq!(tcp_payload["owner"], Value::Null);

    let invalid = Request::make(
        "governance-invalid",
        method::HOST_GOVERNANCE_INSPECT,
        serde_json::json!({}),
    );
    let tcp_error = exchange(&mut tcp_client, &invalid)
        .await
        .into_result()
        .expect_err("tcp rejects non-null governance params");
    let unix_error = exchange(&mut unix_client, &invalid)
        .await
        .into_result()
        .expect_err("unix rejects non-null governance params");
    assert_eq!(tcp_error, unix_error);
    assert_eq!(tcp_error.code, "bad_request");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn daemon_doctor_has_identical_unix_and_tcp_dispatch() {
    let _process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    let (addr, socket, shutdown, handle) =
        spawn_dual_servers("remote-daemon-doctor", "0.0.0", shell_config()).await;

    let mut tcp_client = connect_tcp(addr).await;
    let mut unix_client = connect_unix(&socket).await;
    let request = Request::make("daemon-doctor", method::DAEMON_DOCTOR, Value::Null);
    let tcp_payload = ok_payload(exchange(&mut tcp_client, &request).await);
    let unix_payload = ok_payload(exchange(&mut unix_client, &request).await);

    assert_eq!(tcp_payload, unix_payload);
    let checks = tcp_payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array");
    assert!(checks
        .iter()
        .any(|check| check["name"] == "host_identity_stable"));

    let mut omitted_wire = serde_json::to_value(Request::make(
        "daemon-doctor-omitted",
        method::DAEMON_DOCTOR,
        Value::Null,
    ))
    .expect("serialize daemon doctor request");
    omitted_wire
        .as_object_mut()
        .expect("request object")
        .remove("params");
    let omitted: ProtocolRequest =
        serde_json::from_value(omitted_wire).expect("omitted params normalize to null");
    let tcp_omitted = ok_payload(exchange(&mut tcp_client, &omitted).await);
    let unix_omitted = ok_payload(exchange(&mut unix_client, &omitted).await);
    assert_eq!(tcp_omitted, tcp_payload);
    assert_eq!(unix_omitted, unix_payload);

    let invalid = Request::make(
        "daemon-doctor-invalid",
        method::DAEMON_DOCTOR,
        serde_json::json!({}),
    );
    let tcp_error = exchange(&mut tcp_client, &invalid)
        .await
        .into_result()
        .expect_err("tcp rejects daemon doctor params");
    let unix_error = exchange(&mut unix_client, &invalid)
        .await
        .into_result()
        .expect_err("unix rejects daemon doctor params");
    assert_eq!(tcp_error, unix_error);
    assert_eq!(tcp_error.code, "bad_request");

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Writes two Claude profiles whose override manifests differ in whether they
/// declare an idle rule on a screen region, over a program that answers the
/// version probe.
fn write_claude_override_profiles(root: &Path, agents: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    // A Claude profile waits for screen readiness unless its override manifest
    // lacks an idle rule on a screen region.
    let manifests = agents.join("manifests");
    std::fs::create_dir(&manifests).expect("create manifests directory");
    std::fs::write(manifests.join("norule.toml"), "rules = []\n")
        .expect("write override manifest without an idle rule");
    std::fs::write(
        manifests.join("withrule.toml"),
        "[[rules]]\n\
         id = \"custom_prompt_box\"\n\
         state = \"idle\"\n\
         priority = 950\n\
         region = \"prompt_box_body\"\n\
         line_regex = ['^\\s*\u{276f}']\n",
    )
    .expect("write override manifest with an idle rule");
    let claude_program = root.join("claude-version-stub");
    std::fs::write(
        &claude_program,
        format!(
            "#!/bin/sh\n{}exit 0\n",
            official_runtimes::sh_version_answer("claude")
        ),
    )
    .expect("write Claude version stub");
    std::fs::set_permissions(&claude_program, std::fs::Permissions::from_mode(0o755))
        .expect("make Claude version stub executable");
    for (name, manifest) in [("claude-norule", "norule"), ("claude-withrule", "withrule")] {
        std::fs::write(
            agents.join(format!("{name}.toml")),
            format!(
                "base = \"claude\"\n{}program = {}\nmanifest = \"{manifest}\"\n",
                official_runtimes::profile_pin("claude"),
                toml::Value::String(claude_program.display().to_string())
            ),
        )
        .expect("write Claude profile with an override manifest");
    }
}

#[tokio::test]
async fn daemon_doctor_reports_profile_readiness_and_redacts_profile_content() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let agents_root = pohunek_test_support::tempdir().expect("create profile fixture root");
    let mut process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    process_env.set("PATH", agents_root.path());
    let agents = agents_root.path().join("agents");
    std::fs::create_dir(&agents).expect("create agents directory");
    std::fs::write(
        agents.join("healthy.toml"),
        "base = \"shell\"\nprogram = \"/bin/sh\"\n[env]\nAPI_KEY = \"PROFILE_SECRET_CANARY\"\n",
    )
    .expect("write healthy profile");
    std::fs::write(
        agents.join("malformed.toml"),
        "base = \"shell\"\n[env]\nAPI_KEY = 12345\n",
    )
    .expect("write malformed profile");
    std::fs::write(
        agents.join("missing-executable.toml"),
        "base = \"shell\"\nprogram = \"pohunek-missing-executable-canary\"\n",
    )
    .expect("write profile with missing executable");
    std::fs::write(
        agents.join("pinned-runtime.toml"),
        "base = \"pi\"\npackage = \"acme.runtime.pi\"\ndigest = \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n",
    )
    .expect("write profile with an unavailable pinned runtime");
    write_claude_override_profiles(agents_root.path(), &agents);
    let config = SessionRegistryConfig {
        agents_dir: Some(agents),
        ..shell_config()
    };
    // Codex is served, so its optional runtime check warns about the executable
    // missing from `PATH`.
    let (_addr, socket, shutdown, handle) =
        spawn_dual_servers_serving_official_runtimes("doctor-profile-readiness", "0.0.0", config)
            .await;
    let mut client = connect_unix(&socket).await;
    let response = exchange(
        &mut client,
        &Request::make("doctor-profiles", method::DAEMON_DOCTOR, Value::Null),
    )
    .await;
    let payload = ok_payload(response);
    let checks = payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array");
    let find = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("doctor reports {name}: {payload}"))
    };
    assert_eq!(find("agent_profile_healthy")["status"], "ok");
    assert_eq!(find("agent_profile_malformed")["status"], "fail");
    assert!(find("agent_profile_malformed")["detail"]
        .as_str()
        .expect("redacted detail")
        .contains("repair"));
    assert_eq!(find("agent_profile_missing-executable")["status"], "fail");
    assert_eq!(find("agent_profile_pinned-runtime")["status"], "fail");
    assert!(find("agent_profile_pinned-runtime")["detail"]
        .as_str()
        .expect("pinned runtime detail")
        .contains("pinned runtime is unavailable"));
    assert_eq!(find("agent_profile_claude-norule")["status"], "warn");
    assert!(find("agent_profile_claude-norule")["detail"]
        .as_str()
        .expect("readiness detail")
        .contains("prompt_box_body"));
    assert_eq!(find("agent_profile_claude-withrule")["status"], "ok");
    assert_eq!(find("agent_runtime_codex")["status"], "warn");
    assert!(find("agent_runtime_codex")["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("executable is missing")));
    let rendered = serde_json::to_string(&payload).expect("serialize doctor payload");
    assert!(!rendered.contains("PROFILE_SECRET_CANARY"));
    assert!(!rendered.contains("pohunek-missing-executable-canary"));

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "The RPC scenario verifies several redaction and provider states together."
)]
async fn daemon_doctor_reports_redacted_provider_integration_readiness() {
    use std::os::unix::fs::PermissionsExt as _;

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("create integration fixture root");
    let mut process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    process_env.set("PATH", root.path());
    process_env.set("HOME", root.path());

    let agents = root.path().join("agents");
    std::fs::create_dir(&agents).expect("create agents directory");
    let shared_home = root.path().join("shared-codex-home");
    let symlink_home = root.path().join("a-link-codex-home");
    let different_home = root.path().join("different-codex-home");
    let malformed_home = root.path().join("malformed-codex-home");
    let incomplete_home = root.path().join("incomplete-codex-home");
    std::fs::create_dir(&shared_home).expect("create shared Codex home");
    std::os::unix::fs::symlink(&shared_home, &symlink_home).expect("symlink shared Codex home");
    std::fs::create_dir(&different_home).expect("create separate Codex home");
    std::fs::create_dir(&malformed_home).expect("create malformed Codex home");
    std::fs::write(malformed_home.join("config.toml"), "= invalid\n")
        .expect("write malformed provider config");
    std::fs::create_dir(&incomplete_home).expect("create incomplete Codex home");
    std::fs::write(
        incomplete_home.join(".pohunek-integration-displaced-fixture"),
        "left by an earlier incomplete install",
    )
    .expect("write quarantine fixture");
    for (name, home) in [
        ("a-link", &symlink_home),
        ("b-real", &shared_home),
        ("different", &different_home),
        ("malformed", &malformed_home),
        ("incomplete", &incomplete_home),
    ] {
        std::fs::write(
            agents.join(format!("{name}.toml")),
            format!(
                "base = \"codex\"\n{}[env]\nCODEX_HOME = \"{}\"\nAPI_KEY = \"DOCTOR_INTEGRATION_SECRET_CANARY\"\n",
                official_runtimes::profile_pin("codex"),
                home.display()
            ),
        )
        .expect("write Codex profile");
    }
    std::fs::write(
        agents.join("hermes-work.toml"),
        format!(
            "base = \"hermes\"\n{}program = \"/bin/sh\"\n",
            official_runtimes::profile_pin("hermes")
        ),
    )
    .expect("write Hermes profile");

    let config = SessionRegistryConfig {
        agents_dir: Some(agents),
        ..shell_config()
    };
    let (addr, socket, shutdown, handle) = spawn_dual_servers_serving_official_runtimes(
        "doctor-provider-integrations",
        "0.0.0",
        config,
    )
    .await;
    let mut local = connect_unix(&socket).await;
    let params = protocol::IntegrationInstallParams {
        agent: Some(protocol::RuntimeRef::codex()),
        profile: Some("b-real".to_owned()),
        all_profiles: false,
    };
    let install_payload = ok_payload(
        exchange(
            &mut local,
            &Request::make(
                "install-profile-integration",
                method::INTEGRATION_INSTALL,
                serde_json::to_value(params).expect("serialize install params"),
            ),
        )
        .await,
    );
    let installed_hook = install_payload["installed"][0]["hook_path"]
        .as_str()
        .expect("installed hook path")
        .to_owned();

    let local_payload = ok_payload(
        exchange(
            &mut local,
            &Request::make(
                "doctor-local-integrations",
                method::DAEMON_DOCTOR,
                Value::Null,
            ),
        )
        .await,
    );
    assert_eq!(local_payload["report"]["overall"], "fail");
    let checks = local_payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array");
    let find = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("doctor reports {name}: {local_payload}"))
    };
    assert_eq!(
        find("agent_integration_codex_profile_b-real")["status"],
        "ok"
    );
    assert_eq!(
        find("agent_integration_codex_profile_b-real")
            .as_object()
            .expect("stable DoctorCheck object")
            .len(),
        3
    );
    assert!(!checks
        .iter()
        .any(|check| check["name"] == "agent_integration_codex_profile_shared"));
    assert_eq!(
        find("agent_integration_codex_profile_different")["status"],
        "warn"
    );
    assert_eq!(
        find("agent_integration_codex_profile_malformed")["status"],
        "fail"
    );
    assert_eq!(
        find("agent_integration_codex_profile_incomplete")["status"],
        "fail"
    );
    assert_eq!(
        find("agent_integration_hermes_profile_hermes-work")["status"],
        "warn"
    );

    let rendered = serde_json::to_string(&local_payload).expect("serialize local doctor payload");
    assert!(!rendered.contains("DOCTOR_INTEGRATION_SECRET_CANARY"));
    assert!(!rendered.contains(shared_home.to_string_lossy().as_ref()));
    assert!(!rendered.contains(different_home.to_string_lossy().as_ref()));

    let alias_doctor = ok_payload(
        exchange(
            &mut local,
            &Request::make(
                "doctor-profile-alias-selector",
                method::INTEGRATION_DOCTOR,
                serde_json::json!({ "agent": "codex", "all_profiles": true }),
            ),
        )
        .await,
    );
    let alias_home = alias_doctor["agents"]
        .as_array()
        .expect("integration doctor agents")
        .iter()
        .find(|agent| agent["home"]["profiles"] == serde_json::json!(["a-link", "b-real"]))
        .expect("the aliased home is grouped");
    assert_eq!(alias_home["home"]["selector"]["name"], "b-real");

    std::fs::set_permissions(&installed_hook, std::fs::Permissions::from_mode(0o700))
        .expect("make the installed hook owner-only executable");
    let benign_mode_payload = ok_payload(
        exchange(
            &mut local,
            &Request::make(
                "doctor-benign-hook-mode",
                method::DAEMON_DOCTOR,
                Value::Null,
            ),
        )
        .await,
    );
    let benign_mode = benign_mode_payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array")
        .iter()
        .find(|check| check["name"] == "agent_integration_codex_profile_b-real")
        .expect("integration check");
    assert_eq!(benign_mode["status"], "warn");
    assert!(benign_mode["detail"]
        .as_str()
        .expect("detail")
        .contains("pohunek integration install --agent codex --profile b-real"));

    std::fs::set_permissions(&installed_hook, std::fs::Permissions::from_mode(0o777))
        .expect("make the installed hook writable by other users");
    let unsafe_mode_payload = ok_payload(
        exchange(
            &mut local,
            &Request::make(
                "doctor-unsafe-hook-mode",
                method::DAEMON_DOCTOR,
                Value::Null,
            ),
        )
        .await,
    );
    let unsafe_mode = unsafe_mode_payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array")
        .iter()
        .find(|check| check["name"] == "agent_integration_codex_profile_b-real")
        .expect("integration check");
    assert_eq!(unsafe_mode["status"], "fail");
    std::fs::set_permissions(&installed_hook, std::fs::Permissions::from_mode(0o755))
        .expect("restore the installer mode before the content-drift case");

    let shared_root =
        pohunek_platform::filesystem::TrustedDir::open_absolute_owner_safe(&shared_home, 0o022)
            .expect("open shared home");
    let lock = shared_root
        .lock_existing_file(
            ".pohunek-integration.lock",
            0o600,
            pohunek_platform::filesystem::LockKind::Exclusive,
        )
        .expect("lock probe file")
        .expect("install created its lock file");
    let locked_payload = ok_payload(
        exchange(
            &mut local,
            &Request::make(
                "doctor-locked-integration",
                method::DAEMON_DOCTOR,
                Value::Null,
            ),
        )
        .await,
    );
    let locked = locked_payload["report"]["checks"]
        .as_array()
        .expect("locked doctor checks array")
        .iter()
        .find(|check| check["name"] == "agent_integration_codex_profile_b-real")
        .expect("locked integration check");
    assert_eq!(locked["status"], "warn");
    assert!(locked["detail"]
        .as_str()
        .expect("busy detail")
        .contains("busy"));
    drop(lock);

    std::fs::write(&installed_hook, "# outdated integration fixture\n")
        .expect("change installed hook");
    let outdated_payload = ok_payload(
        exchange(
            &mut local,
            &Request::make(
                "doctor-outdated-integration",
                method::DAEMON_DOCTOR,
                Value::Null,
            ),
        )
        .await,
    );
    let outdated = outdated_payload["report"]["checks"]
        .as_array()
        .expect("outdated doctor checks array")
        .iter()
        .find(|check| check["name"] == "agent_integration_codex_profile_b-real")
        .expect("outdated integration check");
    assert_eq!(outdated["status"], "warn");
    assert!(outdated["detail"]
        .as_str()
        .expect("outdated detail")
        .contains("outdated"));

    let mut remote = connect_tcp(addr).await;
    let remote_payload = ok_payload(
        exchange(
            &mut remote,
            &Request::make(
                "doctor-remote-integrations",
                method::DAEMON_DOCTOR,
                Value::Null,
            ),
        )
        .await,
    );
    let remote_checks = remote_payload["report"]["checks"]
        .as_array()
        .expect("remote doctor checks array");
    assert!(remote_checks
        .iter()
        .any(|check| check["name"] == "agent_integration_codex"));
    assert!(!remote_checks.iter().any(|check| {
        check["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("agent_integration_codex_profile_"))
    }));
    let remote_rendered =
        serde_json::to_string(&remote_payload).expect("serialize remote doctor payload");
    assert!(!remote_rendered.contains("DOCTOR_INTEGRATION_SECRET_CANARY"));
    assert!(!remote_rendered.contains(shared_home.to_string_lossy().as_ref()));

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn daemon_doctor_bounds_oversized_profile_home_inspection() {
    const PROFILE_SCAN_LIMIT: usize = 4096;

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("create oversized profile fixture root");
    let mut process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    process_env.set("PATH", root.path());
    let agents = root.path().join("agents");
    std::fs::create_dir(&agents).expect("create agents directory");
    let config = SessionRegistryConfig {
        agents_dir: Some(agents.clone()),
        ..shell_config()
    };
    let (_addr, socket, shutdown, handle) =
        spawn_dual_servers_serving_official_runtimes("doctor-oversized-profiles", "0.0.0", config)
            .await;
    // Written once the packages are installed: a package install reads the
    // profile directory, which is past its scan limit from here on.
    let profile = format!(
        "base = \"codex\"\n{}",
        official_runtimes::profile_pin("codex")
    );
    for index in 0..=PROFILE_SCAN_LIMIT {
        std::fs::write(agents.join(format!("profile-{index:04}.toml")), &profile)
            .expect("write profile entry");
    }
    let mut client = connect_unix(&socket).await;
    let payload = ok_payload(
        exchange(
            &mut client,
            &Request::make(
                "doctor-oversized-profiles",
                method::DAEMON_DOCTOR,
                Value::Null,
            ),
        )
        .await,
    );
    let report = &payload["report"];
    assert_eq!(report["overall"], "fail");
    let checks = report["checks"].as_array().expect("doctor checks array");
    let profiles = checks
        .iter()
        .find(|check| check["name"] == "agent_profiles")
        .expect("oversized profile scan failure");
    assert_eq!(profiles["status"], "fail");
    assert_eq!(
        profiles["detail"],
        "the configured agents directory has too many entries to inspect"
    );
    assert!(
        checks.len() < 64,
        "doctor response was not bounded: {} checks",
        checks.len()
    );
    assert!(checks
        .iter()
        .any(|check| check["name"] == "agent_integration_codex"));
    assert!(!checks.iter().any(|check| {
        check["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("agent_integration_codex_profile_"))
    }));

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "One RPC scenario compares profile results below, at, and above the directory-entry bound."
)]
async fn daemon_doctor_counts_non_profile_entries_at_profile_scan_boundary() {
    use std::os::unix::fs::PermissionsExt as _;

    const PROFILE_SCAN_LIMIT: usize = 4096;

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("create profile boundary fixture root");
    let mut process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    process_env.set("PATH", root.path());
    let agents = root.path().join("agents");
    std::fs::create_dir(&agents).expect("create agents directory");
    std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700))
        .expect("make agents directory private");
    std::fs::write(
        agents.join("healthy.toml"),
        "base = \"shell\"\nprogram = \"/bin/sh\"\n",
    )
    .expect("write healthy profile");
    std::fs::write(agents.join("malformed.toml"), "base = [\n").expect("write malformed profile");
    let config = SessionRegistryConfig {
        agents_dir: Some(agents.clone()),
        ..shell_config()
    };
    let (_addr, socket, shutdown, handle) =
        spawn_dual_servers("doctor-profile-boundary", "0.0.0", config).await;
    let mut client = connect_unix(&socket).await;
    let mut reports = Vec::new();
    let mut previous_count = 2;
    for entry_count in [2, PROFILE_SCAN_LIMIT, PROFILE_SCAN_LIMIT + 1] {
        // Every directory entry counts toward the bound, including non-profile files.
        for index in previous_count..entry_count {
            std::fs::write(agents.join(format!("ignored-{index:04}.txt")), "")
                .expect("write non-profile entry");
        }
        previous_count = entry_count;
        let payload = ok_payload(
            exchange(
                &mut client,
                &Request::make(
                    &format!("doctor-profile-boundary-{entry_count}"),
                    method::DAEMON_DOCTOR,
                    Value::Null,
                ),
            )
            .await,
        );
        let mut checks = payload["report"]["checks"]
            .as_array()
            .expect("doctor checks array")
            .clone();
        checks.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
        reports.push(checks);
    }
    assert_eq!(
        reports[0], reports[1],
        "non-profile entries preserve checks at the limit"
    );
    let profiles: Vec<_> = reports[1]
        .iter()
        .filter(|check| {
            check["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("agent_profile_"))
        })
        .collect();
    assert_eq!(profiles.len(), 2);
    for (profile, name, status, detail) in [
        (profiles[0], "healthy", "ok", "Configured profile and runtime are ready."),
        (profiles[1], "malformed", "fail", "The profile is malformed or cannot be resolved securely; repair its file or permissions."),
    ] {
        assert_eq!(profile["name"], format!("agent_profile_{name}"));
        assert_eq!(profile["status"], status);
        assert_eq!(profile["detail"], detail);
    }
    assert!(!reports[1]
        .iter()
        .any(|check| check["name"] == "agent_profiles"));
    let overflow = reports[2]
        .iter()
        .find(|check| check["name"] == "agent_profiles")
        .expect("limit plus one reports overflow");
    assert_eq!(overflow["status"], "fail");
    assert_eq!(
        overflow["detail"],
        "the configured agents directory has too many entries to inspect"
    );
    assert!(reports[2].len() < 64, "overflow response stays bounded");
    assert!(!reports[2].iter().any(|check| {
        check["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("agent_profile_"))
    }));
    for check in &reports[2] {
        match check["name"].as_str().expect("check name") {
            "agent_profiles" => {}
            "agent_manifest_overrides" => {
                assert_eq!(check["status"], "fail");
                assert_eq!(
                    check["detail"],
                    "configured agent profiles cannot be inspected"
                );
            }
            _ => assert!(
                reports[1].contains(check),
                "overflow preserves generic checks: {check}"
            ),
        }
    }
    assert!(reports[2]
        .iter()
        .any(|check| check["name"] == "agent_manifest_overrides"));

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn daemon_doctor_reports_insecure_profile_directory_without_reading_it() {
    use std::os::unix::fs::PermissionsExt as _;

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("create insecure profile fixture root");
    let mut process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    process_env.set("PATH", root.path());
    let agents = root.path().join("agents");
    std::fs::create_dir(&agents).expect("create agents directory");
    std::fs::write(
        agents.join("unsafe.toml"),
        "base = \"shell\"\n[env]\nAPI_KEY = \"INSECURE_PROFILE_SECRET_CANARY\"\n",
    )
    .expect("write unsafe profile canary");
    std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o777))
        .expect("make configured directory insecure");
    let config = SessionRegistryConfig {
        agents_dir: Some(agents.clone()),
        ..shell_config()
    };
    let (_addr, socket, shutdown, handle) =
        spawn_dual_servers("doctor-insecure-profile", "0.0.0", config).await;
    let mut client = connect_unix(&socket).await;
    let payload = ok_payload(
        exchange(
            &mut client,
            &Request::make("doctor-insecure", method::DAEMON_DOCTOR, Value::Null),
        )
        .await,
    );
    let checks = payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array");
    let profiles = checks
        .iter()
        .find(|check| check["name"] == "agent_profiles")
        .expect("directory security check");
    assert_eq!(profiles["status"], "fail");
    assert_eq!(
        profiles["detail"],
        "the configured agents directory is not owner-secure"
    );
    let rendered = serde_json::to_string(&payload).expect("serialize doctor payload");
    assert!(!rendered.contains("INSECURE_PROFILE_SECRET_CANARY"));
    assert!(!rendered.contains(agents.to_string_lossy().as_ref()));

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn daemon_doctor_uses_daemon_path_for_executable_and_profile_path_for_probe_child() {
    use std::os::unix::fs::PermissionsExt as _;

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("create path fixture root");
    let mut process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    let daemon_path = root.path().join("daemon-bin");
    let profile_path = root.path().join("profile-bin");
    let agents = root.path().join("agents");
    std::fs::create_dir_all(&daemon_path).expect("create daemon PATH");
    std::fs::create_dir_all(&profile_path).expect("create profile PATH");
    std::fs::create_dir(&agents).expect("create agents directory");
    process_env.set("PATH", &daemon_path);

    let banner = official_runtimes::supported_banner("codex");
    std::fs::write(
        profile_path.join("profile-only-runtime"),
        format!("#!/bin/sh\nprintf '%s\\n' '{banner}'\n"),
    )
    .expect("write executable present only on profile PATH");
    std::fs::set_permissions(
        profile_path.join("profile-only-runtime"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("make profile-only runtime executable");
    std::fs::write(
        agents.join("profile-only.toml"),
        format!(
            "base = \"codex\"\n{}program = \"profile-only-runtime\"\n[env]\nPATH = \"{}\"\n",
            official_runtimes::profile_pin("codex"),
            profile_path.display()
        ),
    )
    .expect("write profile whose executable is missing from daemon PATH");

    let runtime_script = daemon_path.join("runtime-script");
    std::fs::write(
        &runtime_script,
        format!("#!/usr/bin/env sh\nprintf '%s\\n' '{banner}'\n"),
    )
    .expect("write runtime script with PATH-resolved interpreter");
    std::fs::set_permissions(&runtime_script, std::fs::Permissions::from_mode(0o755))
        .expect("make runtime script executable");
    std::fs::write(
        agents.join("profile-path-probe.toml"),
        format!(
            "base = \"codex\"\n{}program = \"{}\"\n[env]\nPATH = \"/bin\"\n",
            official_runtimes::profile_pin("codex"),
            runtime_script.display()
        ),
    )
    .expect("write profile with an interpreter supplied by effective launch PATH");

    let config = SessionRegistryConfig {
        agents_dir: Some(agents),
        ..shell_config()
    };
    let (_addr, socket, shutdown, handle) =
        spawn_dual_servers_serving_official_runtimes("doctor-path-readiness", "0.0.0", config)
            .await;
    let mut client = connect_unix(&socket).await;
    let payload = ok_payload(
        exchange(
            &mut client,
            &Request::make("doctor-paths", method::DAEMON_DOCTOR, Value::Null),
        )
        .await,
    );
    let checks = payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array");
    let find = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("doctor reports {name}: {payload}"))
    };
    assert_eq!(find("agent_profile_profile-only")["status"], "fail");
    assert!(find("agent_profile_profile-only")["detail"]
        .as_str()
        .expect("missing executable detail")
        .contains("executable is missing"));
    assert_eq!(find("agent_profile_profile-path-probe")["status"], "ok");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn daemon_doctor_bounds_aggregate_version_probes_across_slow_profiles() {
    use std::os::unix::fs::PermissionsExt as _;

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("create slow profile fixture root");
    let mut process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    process_env.set("PATH", root.path());
    let agents = root.path().join("agents");
    std::fs::create_dir(&agents).expect("create agents directory");
    let probe_count = root.path().join("probe-count");
    let slow_runtime = root.path().join("slow-runtime");
    std::fs::write(
        &slow_runtime,
        format!(
            "#!/bin/sh\nprintf x >> '{}'\n/bin/sleep 20\nprintf '%s\\n' '{}'\n",
            probe_count.display(),
            official_runtimes::supported_banner("hermes")
        ),
    )
    .expect("write blocking version-probe executable");
    std::fs::set_permissions(&slow_runtime, std::fs::Permissions::from_mode(0o755))
        .expect("make slow runtime executable");
    std::os::unix::fs::symlink(&slow_runtime, root.path().join("hermes"))
        .expect("expose the valid slow runtime as the optional Hermes provider");
    for index in 0..6 {
        std::fs::write(
            agents.join(format!("slow-{index}.toml")),
            format!(
                "base = \"hermes\"\n{}program = \"{}\"\n[env]\nPATH = \"/bin\"\n",
                official_runtimes::profile_pin("hermes"),
                slow_runtime.display()
            ),
        )
        .expect("write slow profile");
    }
    let config = SessionRegistryConfig {
        agents_dir: Some(agents),
        ..shell_config()
    };
    let (_addr, socket, shutdown, handle) =
        spawn_dual_servers_serving_official_runtimes("doctor-probe-budget", "0.0.0", config).await;
    let mut client = connect_unix(&socket).await;
    let payload = ok_payload(
        exchange(
            &mut client,
            &Request::make("doctor-probe-budget", method::DAEMON_DOCTOR, Value::Null),
        )
        .await,
    );
    let checks = payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array");
    let find = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("doctor reports {name}: {payload}"))
    };
    assert!(
        find("agent_profile_slow-1")["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("probe budget was exhausted")),
        "a timeout under the shortened aggregate budget must not be called an unsupported version: {payload}"
    );
    assert_eq!(find("agent_profile_slow-1")["status"], "fail");
    assert_eq!(find("agent_runtime_hermes")["status"], "warn");
    assert!(find("agent_runtime_hermes")["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("probe budget was exhausted")));
    let launches = std::fs::read_to_string(&probe_count).expect("read version-probe count");
    assert_eq!(
        launches.len(),
        2,
        "aggregate budget permits only two full probes"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// Opens the write end of the FIFO at `path` without waiting for a reader.
///
/// While the returned file is open, a reader's open completes at once and its
/// read blocks; dropping it, also while a failure unwinds or when this process
/// exits, hands every reader end of file.
fn open_fifo_writer(path: &Path) -> std::fs::File {
    use std::os::unix::fs::OpenOptionsExt as _;

    // A non-blocking read end lets the write end open without blocking; only
    // the write end is kept.
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .expect("open the FIFO for reading");
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the FIFO for writing");
    drop(reader);
    writer
}

#[tokio::test]
async fn daemon_doctor_version_probe_leaves_no_descendant_behind() {
    use std::os::unix::fs::PermissionsExt as _;

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("create probe fixture root");
    let mut process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    process_env.set("PATH", root.path());
    let agents = root.path().join("agents");
    std::fs::create_dir(&agents).expect("create agents directory");
    let descendants = root.path().join("descendants");
    std::fs::create_dir(&descendants).expect("create descendant pid directory");
    let hold = root.path().join("hold");
    nix::unistd::mkfifo(&hold, nix::sys::stat::Mode::S_IRWXU).expect("create the hold FIFO");
    let _hold_writer = open_fifo_writer(&hold);
    // Each probe starts a descendant in its process group. The descendant opens
    // the FIFO read-write, so the read-only open after it cannot block, drops
    // that write end, records its pid, and blocks reading: a shell builtin step
    // that ends only once this test's write end closes, also when it unwinds or
    // exits. The probe prints its version and exits only after that pid is
    // recorded, so its group still holds a live descendant when it exits.
    let runtime = root.path().join("forking-runtime");
    std::fs::write(
        &runtime,
        format!(
            "#!/bin/sh\n\
             sh -c 'exec 3<> \"{hold}\" 4< \"{hold}\" 3>&-; echo $$ > \"{descendants}/$$\"; read held <&4' &\n\
             until [ -s \"{descendants}/$!\" ]; do :; done\n\
             printf '%s\\n' '{banner}'\n",
            descendants = descendants.display(),
            hold = hold.display(),
            banner = official_runtimes::supported_banner("hermes"),
        ),
    )
    .expect("write forking version-probe executable");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))
        .expect("make forking runtime executable");
    std::fs::write(
        agents.join("forking.toml"),
        format!(
            "base = \"hermes\"\n{}program = \"{}\"\n[env]\nPATH = \"/bin\"\n",
            official_runtimes::profile_pin("hermes"),
            runtime.display()
        ),
    )
    .expect("write forking profile");
    let config = SessionRegistryConfig {
        agents_dir: Some(agents),
        ..shell_config()
    };
    let (_addr, socket, shutdown, handle) =
        spawn_dual_servers_serving_official_runtimes("doctor-probe-group", "0.0.0", config).await;
    let mut client = connect_unix(&socket).await;
    let payload = ok_payload(
        exchange(
            &mut client,
            &Request::make("doctor-probe-group", method::DAEMON_DOCTOR, Value::Null),
        )
        .await,
    );
    // A ready profile proves the probe exited and printed its version rather
    // than running into the probe timeout, whose cleanup ends the group anyway.
    let profile = payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array")
        .iter()
        .find(|check| check["name"] == "agent_profile_forking")
        .unwrap_or_else(|| panic!("doctor reports the forking profile: {payload}"));
    assert_eq!(profile["status"], "ok", "{payload}");

    let pids: Vec<u32> = std::fs::read_dir(&descendants)
        .expect("read descendant pids")
        .map(|entry| {
            entry
                .expect("read descendant pid entry")
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
                .expect("a numeric descendant pid")
        })
        .collect();
    assert!(
        !pids.is_empty(),
        "the doctor must run the profile's version probe: {payload}"
    );
    let inspector = HostInspector::new();
    pohunek_test_support::wait::wait_until("the probe's descendants to be terminated", || {
        let ended = pids.iter().all(|pid| {
            inspector
                .identity(*pid)
                .expect("inspect a probe descendant")
                .is_none_or(|identity| {
                    !inspector
                        .is_running(identity)
                        .expect("inspect a probe descendant")
                })
        });
        async move { ended.then_some(()) }
    })
    .await;

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn daemon_doctor_truncates_oversized_reports_below_control_line_limit() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("create large profile fixture root");
    let mut process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    process_env.set("PATH", root.path());
    let agents = root.path().join("agents");
    std::fs::create_dir(&agents).expect("create agents directory");
    for index in 0..4096 {
        let name = format!("p{index:04}-{}", "x".repeat(235));
        std::fs::write(agents.join(format!("{name}.toml")), "base = \"shell\"\n")
            .expect("write large-report profile");
    }
    let config = SessionRegistryConfig {
        agents_dir: Some(agents),
        ..shell_config()
    };
    let (_addr, socket, shutdown, handle) =
        spawn_dual_servers("doctor-report-limit", "0.0.0", config).await;
    let mut client = connect_unix(&socket).await;
    let response = exchange(
        &mut client,
        &Request::make("doctor-report-limit", method::DAEMON_DOCTOR, Value::Null),
    )
    .await;
    let response_size = serde_json::to_vec(&response)
        .expect("serialize response envelope")
        .len();
    assert!(
        response_size <= protocol::MAX_CONTROL_LINE_BYTES,
        "response is {response_size} bytes"
    );
    let payload = ok_payload(response);
    assert_eq!(payload["report"]["overall"], "fail");
    let checks = payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array");
    let truncated = checks
        .iter()
        .find(|check| check["name"] == "doctor_checks_truncated")
        .expect("explicit report truncation failure");
    assert_eq!(truncated["status"], "fail");

    let _ = shutdown.send(());
    let _ = handle.await;
}

fn write_manifest_doctor_fixture(root: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;

    let agents = root.join("agents");
    let manifests = agents.join("manifests");
    std::fs::create_dir_all(&manifests).expect("create manifest directory");
    std::fs::write(
        agents.join("empty-override.toml"),
        "base = \"shell\"\nmanifest = \"empty\"\n",
    )
    .expect("write profile with empty override");
    std::fs::write(
        agents.join("process-only-override.toml"),
        "base = \"shell\"\nmanifest = \"process-only\"\n",
    )
    .expect("write profile with process-only override");
    std::fs::write(
        agents.join("missing-override.toml"),
        "base = \"shell\"\nmanifest = \"missing\"\n",
    )
    .expect("write profile with missing override");
    std::fs::write(
        agents.join("malformed-override.toml"),
        "base = \"shell\"\nmanifest = \"malformed\"\n",
    )
    .expect("write profile with malformed override");
    std::fs::write(
        agents.join("unsafe-override.toml"),
        "base = \"shell\"\nmanifest = \"unsafe\"\n",
    )
    .expect("write profile with unsafe override");
    std::fs::write(
        manifests.join("empty.toml"),
        "# An empty rule list intentionally disables detection.\nrules = []\n",
    )
    .expect("write empty manifest");
    std::fs::write(
        manifests.join("process-only.toml"),
        "[process]\ncomm = [\"^codex$\"]\n",
    )
    .expect("write process-only manifest");
    std::fs::write(
        manifests.join("malformed.toml"),
        "[[rules]]\nid = \"MANIFEST_SECRET_CANARY\"\n",
    )
    .expect("write malformed manifest canary");
    std::fs::write(
        manifests.join("unused.toml"),
        "[[rules]]\nid = \"MANIFEST_UNUSED_CANARY\"\n",
    )
    .expect("write malformed unused manifest canary");
    let outside = root.join("outside.toml");
    std::fs::write(&outside, "[[rules]]\nid = \"MANIFEST_UNSAFE_CANARY\"\n")
        .expect("write outside manifest canary");
    std::os::unix::fs::symlink(&outside, manifests.join("unsafe.toml"))
        .expect("link manifest outside the trusted tree");
    std::os::unix::fs::symlink(
        root.join("DANGLING_MANIFEST_SECRET_CANARY"),
        manifests.join("dangling.toml"),
    )
    .expect("link an unreferenced manifest to a missing target");
    std::fs::set_permissions(
        manifests.join("empty.toml"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("keep empty manifest owner-only");
    (agents, outside)
}

#[tokio::test]
async fn daemon_doctor_reports_embedded_and_override_manifest_health_redacted() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("create manifest fixture root");
    let mut process_env = pohunek_test_support::process_env::ProcessEnv::lock();
    process_env.set("PATH", root.path());
    let (agents, outside) = write_manifest_doctor_fixture(root.path());
    let config = SessionRegistryConfig {
        agents_dir: Some(agents),
        ..shell_config()
    };
    let (_addr, socket, shutdown, handle) =
        spawn_dual_servers("doctor-manifest-readiness", "0.0.0", config).await;
    let mut client = connect_unix(&socket).await;
    let payload = ok_payload(
        exchange(
            &mut client,
            &Request::make("doctor-manifests", method::DAEMON_DOCTOR, Value::Null),
        )
        .await,
    );
    let checks = payload["report"]["checks"]
        .as_array()
        .expect("doctor checks array");
    let find = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("doctor reports {name}: {payload}"))
    };
    assert_eq!(find("agent_manifest_embedded_shell")["status"], "ok");
    // Codex, Claude, Hermes and Pi are served only by their official packages,
    // so the daemon embeds no manifest of theirs to check.
    for runtime in ["codex", "claude", "hermes", "pi"] {
        let name = format!("agent_manifest_embedded_{runtime}");
        assert!(
            checks.iter().all(|check| check["name"] != name.as_str()),
            "{payload}"
        );
    }
    assert_eq!(find("agent_manifest_override_empty")["status"], "ok");
    assert!(find("agent_manifest_override_empty")["detail"]
        .as_str()
        .expect("empty override detail")
        .contains("disables detection"));
    assert_eq!(find("agent_manifest_override_process-only")["status"], "ok");
    let process_only_detail = find("agent_manifest_override_process-only")["detail"]
        .as_str()
        .expect("process-only override detail");
    assert!(process_only_detail.contains("takes precedence"));
    assert!(!process_only_detail.contains("disables detection"));
    for name in ["missing", "malformed", "unsafe"] {
        assert_eq!(
            find(&format!("agent_manifest_override_{name}"))["status"],
            "fail"
        );
        assert_eq!(
            find(&format!("agent_profile_{name}-override"))["status"],
            "fail"
        );
    }
    assert_eq!(find("agent_manifest_override_unused")["status"], "warn");
    assert_eq!(find("agent_manifest_override_dangling")["status"], "warn");
    let rendered = serde_json::to_string(&payload).expect("serialize doctor payload");
    assert!(!rendered.contains("MANIFEST_SECRET_CANARY"));
    assert!(!rendered.contains("MANIFEST_UNUSED_CANARY"));
    assert!(!rendered.contains("MANIFEST_UNSAFE_CANARY"));
    assert!(!rendered.contains("DANGLING_MANIFEST_SECRET_CANARY"));
    assert!(!rendered.contains(outside.to_string_lossy().as_ref()));

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn daemon_health_payload_is_identical_over_unix_and_tcp() {
    let (addr, socket, shutdown, handle) =
        spawn_dual_servers("remote-parity", "5.5.5-test", shell_config()).await;

    let mut tcp_client = connect_tcp(addr).await;
    let mut unix_client = connect_unix(&socket).await;

    let health_req = Request::make("health", method::DAEMON_HEALTH, Value::Null);
    let tcp_ok = ok_payload(exchange(&mut tcp_client, &health_req).await);
    let unix_ok = ok_payload(exchange(&mut unix_client, &health_req).await);

    assert_eq!(
        tcp_ok, unix_ok,
        "daemon.health payload must be transport-agnostic"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn version_mismatch_over_tcp_is_rejected_with_the_daemon_version() {
    // The cross-crate contract `host discover` depends on (finding #1): a real
    // daemon rejects a protocol-incompatible client at negotiation — BEFORE the
    // health handler runs — with a `version_mismatch` ERROR whose envelope `v`
    // carries the DAEMON's own version. The CLI's `classify_response` keys off
    // that envelope `v` to report VersionMismatch instead of Unreachable, so this
    // pins the daemon end of that contract over the real TCP wire.
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("remote-version-mismatch", "0.0.0", shell_config()).await;

    let mut client = connect_tcp(addr).await;
    // Speak a protocol version one higher than the daemon — an incompatible peer.
    let unsupported = PROTOCOL_VERSION.get() + 1;
    let request: ProtocolRequest = serde_json::from_value(serde_json::json!({
        "v": {"minimum": unsupported, "maximum": unsupported},
        "id": "skew",
        "method": method::DAEMON_HEALTH,
        "params": null
    }))
    .expect("valid unsupported request range");

    let response = exchange(&mut client, &request).await;
    assert_eq!(
        response.version(),
        PROTOCOL_VERSION,
        "the rejection envelope must carry the daemon's own version"
    );
    let err = response
        .into_result()
        .expect_err("an incompatible client must be rejected");
    assert_eq!(err.code, "version_mismatch", "stable code");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn a_client_of_the_previous_protocol_is_served_in_its_own_version_over_tcp() {
    // The overlay listener shares the daemon's dispatch, so a client of the
    // previous public protocol is answered at its own version, not rejected.
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("remote-previous-protocol-client", "0.0.0", shell_config()).await;

    let mut client = connect_tcp(addr).await;
    let previous = MIN_PROTOCOL_VERSION.get();
    assert_eq!(previous + 1, PROTOCOL_VERSION.get());
    let request: ProtocolRequest = serde_json::from_value(serde_json::json!({
        "v": {"minimum": previous, "maximum": previous},
        "id": "previous-client",
        "method": method::SESSION_LIST,
        "params": {}
    }))
    .expect("valid previous-version request range");

    let response = exchange(&mut client, &request).await;
    assert_eq!(response.version().get(), previous);
    let listed = response
        .into_result()
        .expect("a client of the previous protocol is accepted");
    assert!(listed.is_array(), "{listed}");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn a_client_older_than_the_window_is_rejected_with_the_typed_version_error() {
    // A client two public versions back gets the typed mismatch error and the
    // daemon's own version before any method runs, never a payload it could
    // misread.
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("remote-protocol-below-window", "0.0.0", shell_config()).await;

    let mut client = connect_tcp(addr).await;
    let below = MIN_PROTOCOL_VERSION.get() - 1;
    let request: ProtocolRequest = serde_json::from_value(serde_json::json!({
        "v": {"minimum": below, "maximum": below},
        "id": "old-client",
        "method": method::SESSION_LIST,
        "params": {}
    }))
    .expect("valid below-window request range");

    let response = exchange(&mut client, &request).await;
    assert_eq!(response.version(), PROTOCOL_VERSION);
    let err = response
        .into_result()
        .expect_err("a client below the window must be rejected");
    assert_eq!(err.code, "version_mismatch");
    assert!(err.recover.is_some(), "the error carries a recovery hint");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn bind_rejects_non_netbird_address() {
    // The daemon must reject every non-NetBird category before opening a socket.
    let root = temp_dir("remote-bind-rejection");
    let root = root.path();
    let state = DaemonState::new(
        HealthInfo::new("0.0.0"),
        SessionRegistry::new(support::hermetic_registry_config()),
        Arc::new(
            HostGovernanceService::open(root.join("state"))
                .await
                .expect("open real isolated host-governance service"),
        ),
        support::overlay_registry(),
        pohunek_daemon::api::OverlaySource::Default,
    );
    let transport = netbird::NetbirdTransport::new();
    for literal in [
        "0.0.0.0",
        "127.0.0.1",
        "::",
        "::1",
        "100.63.255.255",
        "100.128.0.0",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.1",
        "8.8.8.8",
        "2001:db8::1",
        "::ffff:100.64.0.1",
    ] {
        let ip: IpAddr = literal.parse().expect("valid IP literal");
        let result =
            RemoteServer::bind(SocketAddr::new(ip, 18722), state.clone(), &transport).await;
        match result {
            Err(DaemonError::OverlayBind { addr: rejected, .. }) => {
                assert_eq!(rejected, ip);
            }
            Err(other) => panic!("expected OverlayBind for {literal}, got: {other}"),
            Ok(_) => panic!("expected bind to reject {literal}"),
        }
    }
}

/// A provider whose range check accepts every address and whose reported
/// listener address the scenario sets, so only the core checks of
/// `RemoteServer::bind_with` can refuse a bind.
#[derive(Debug)]
struct ScriptedOverlay {
    id: overlay::OverlayId,
    listener: std::sync::Mutex<Result<IpAddr, overlay::OverlayError>>,
}

impl ScriptedOverlay {
    fn reporting(address: IpAddr) -> Self {
        let id = overlay::OverlayId::new("scripted").expect("overlay id");
        Self {
            id,
            listener: std::sync::Mutex::new(Ok(address)),
        }
    }

    fn report(&self, listener: Result<IpAddr, overlay::OverlayError>) {
        *self.listener.lock().expect("listener lock") = listener;
    }
}

impl overlay::OverlayTransport for ScriptedOverlay {
    fn id(&self) -> &overlay::OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, _addr: IpAddr) -> Result<(), overlay::BindAddrError> {
        Ok(())
    }

    fn listener_addr(&self) -> overlay::OverlayFuture<'_, IpAddr> {
        let listener = self.listener.lock().expect("listener lock").clone();
        Box::pin(async move { listener })
    }

    fn resolve_peer<'a>(
        &'a self,
        host: &'a str,
    ) -> overlay::OverlayFuture<'a, overlay::ResolvedPeer> {
        let error = overlay::OverlayError::HostUnknown {
            host: host.to_owned(),
            overlay: self.id.clone(),
        };
        Box::pin(async move { Err(error) })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a overlay::ExternalIdentity,
    ) -> overlay::OverlayFuture<'a, overlay::ResolvedPeer> {
        let error = overlay::OverlayError::HostUnknown {
            host: identity.value().to_owned(),
            overlay: self.id.clone(),
        };
        Box::pin(async move { Err(error) })
    }

    fn discover_peers(&self) -> overlay::OverlayFuture<'_, Vec<overlay::DiscoveredPeer>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// What `RemoteServer::bind_with` did for one target: the outcome and whether
/// the socket opener was reached.
async fn bind_scripted(
    transport: &ScriptedOverlay,
    target: SocketAddr,
) -> (Result<RemoteServer, DaemonError>, bool) {
    let root = temp_dir("remote-bind-check");
    let state = DaemonState::new(
        HealthInfo::new("0.0.0"),
        SessionRegistry::new(support::hermetic_registry_config()),
        Arc::new(
            HostGovernanceService::open(root.path().join("state"))
                .await
                .expect("open real isolated host-governance service"),
        ),
        support::overlay_registry(),
    );
    // The overlay address is not bindable here; a loopback socket stands in.
    let stand_in = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the stand-in listener");
    let opened = std::sync::atomic::AtomicBool::new(false);
    let result = RemoteServer::bind_with(target, state, transport, |_| async {
        opened.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(pohunek_daemon::api::OpenedListener::os_bound(stand_in))
    })
    .await;
    (result, opened.load(std::sync::atomic::Ordering::SeqCst))
}

/// The listener binds only the address its overlay reports at bind time.
#[tokio::test]
async fn bind_requires_the_address_the_overlay_reports_now() {
    let address: IpAddr = "100.64.0.7".parse().expect("overlay address");
    let moved: IpAddr = "100.64.0.8".parse().expect("moved overlay address");
    let target = SocketAddr::new(address, 18722);
    let transport = ScriptedOverlay::reporting(address);

    let (result, opened) = bind_scripted(&transport, target).await;
    result.expect("the reported address binds");
    assert!(opened, "the opener is reached for the reported address");

    // The provider's state changed after the caller queried it: the range
    // check still passes, the equality does not.
    transport.report(Ok(moved));
    let (result, opened) = bind_scripted(&transport, target).await;
    match result {
        Err(DaemonError::OverlayBind { addr, reason }) => {
            assert_eq!(addr, address);
            assert!(reason.contains(&moved.to_string()), "{reason}");
        }
        other => panic!(
            "a stale address must be refused, got {:?}",
            other.map(|_| ())
        ),
    }
    assert!(!opened, "no socket is opened for a stale address");

    // A provider that reports no address cannot vouch for any bind.
    for failure in [
        overlay::OverlayError::ListenerAddressMissing(transport.id.clone()),
        overlay::OverlayError::CliMissing(transport.id.clone()),
        overlay::OverlayError::StateUnavailable {
            overlay: transport.id.clone(),
            detail: "down".to_owned(),
        },
    ] {
        transport.report(Err(failure));
        let (result, opened) = bind_scripted(&transport, target).await;
        assert!(
            matches!(result, Err(DaemonError::OverlayBind { .. })),
            "an address-less provider refuses the bind"
        );
        assert!(!opened);
    }
}

/// Unspecified, loopback, link-local, multicast and broadcast targets are
/// refused by the core even when the provider accepts and reports them.
#[tokio::test]
async fn bind_refuses_non_unicast_targets_whatever_the_provider_says() {
    for literal in [
        "0.0.0.0",
        "::",
        "127.0.0.1",
        "::1",
        "169.254.1.1",
        "fe80::1",
        "224.0.0.1",
        "ff02::1",
        "255.255.255.255",
        "::ffff:127.0.0.1",
        "::ffff:0.0.0.0",
    ] {
        let address: IpAddr = literal.parse().expect("valid IP literal");
        let transport = ScriptedOverlay::reporting(address);
        let (result, opened) = bind_scripted(&transport, SocketAddr::new(address, 18722)).await;
        match result {
            Err(DaemonError::OverlayBind { addr, .. }) => assert_eq!(addr, address, "{literal}"),
            other => panic!("{literal} must be refused, got {:?}", other.map(|_| ())),
        }
        assert!(!opened, "{literal}: no socket is opened");
    }
}

/// Daemon log lines captured in memory, shared by the keepalive scenarios.
static CAPTURED_LOG: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Appends what the daemon logs to [`CAPTURED_LOG`].
struct CapturedLogWriter;

impl std::io::Write for CapturedLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURED_LOG
            .lock()
            .expect("the log capture lock is never poisoned")
            .push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Routes the daemon's debug-level logs into [`CAPTURED_LOG`] for this process.
fn capture_daemon_log() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(|| CapturedLogWriter)
            .init();
    });
}

/// The captured log lines that mention `needle`.
fn captured_log_lines(needle: &str) -> Vec<String> {
    CAPTURED_LOG
        .lock()
        .expect("the log capture lock is never poisoned")
        .lines()
        .filter(|line| line.contains(needle))
        .map(str::to_owned)
        .collect()
}

/// Connects a client to `addr` and returns the daemon's log line about that
/// connection's keepalive, found by the client's own address.
async fn keepalive_log_for_new_connection(
    addr: SocketAddr,
    message: &str,
) -> (Framed<TcpStream, LinesCodec>, String) {
    let client = connect_tcp(addr).await;
    let peer = client
        .get_ref()
        .local_addr()
        .expect("the client socket has a local address");
    let peer_field = format!("peer={peer}");
    let line =
        pohunek_test_support::wait::wait_until("the keepalive log line of the client", || {
            let found = captured_log_lines(&peer_field)
                .into_iter()
                .find(|line| line.contains(message));
            async move { found }
        })
        .await;
    (client, line)
}

/// Every accepted overlay connection reports the documented keepalive defaults.
#[tokio::test]
async fn remote_keepalive_applies_the_documented_defaults() {
    capture_daemon_log();
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("keepalive-default", "0.0.0", shell_config()).await;

    let (_client, line) = keepalive_log_for_new_connection(addr, "remote keepalive applied").await;

    for field in [
        "keepalive.enabled=true",
        "keepalive.idle_s=30",
        "keepalive.interval_s=10",
        "keepalive.count=3",
    ] {
        assert!(line.contains(field), "expected {field} in: {line}");
    }
    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The keepalive of `service.toml` reaches the socket of an accepted connection.
#[tokio::test]
async fn remote_keepalive_applies_the_configured_values() {
    capture_daemon_log();
    let config = SessionRegistryConfig {
        remote_keepalive: pohunek_service_config::KeepaliveTiming {
            idle: Duration::from_secs(45),
            interval: Duration::from_secs(15),
            count: 5,
        },
        ..shell_config()
    };
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("keepalive-configured", "0.0.0", config).await;

    let (_client, line) = keepalive_log_for_new_connection(addr, "remote keepalive applied").await;

    for field in [
        "keepalive.enabled=true",
        "keepalive.idle_s=45",
        "keepalive.interval_s=15",
        "keepalive.count=5",
    ] {
        assert!(line.contains(field), "expected {field} in: {line}");
    }
    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A socket-option error is logged with the peer and the errno and does not
/// stop the daemon from serving the connection.
///
/// The idle time exceeds the largest value the Linux kernel accepts
/// (32767 s), which `service.toml` validation never lets through.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn remote_keepalive_setup_failure_keeps_serving_the_connection() {
    capture_daemon_log();
    let config = SessionRegistryConfig {
        remote_keepalive: pohunek_service_config::KeepaliveTiming {
            idle: Duration::from_secs(40_000),
            interval: Duration::from_secs(10),
            count: 3,
        },
        ..shell_config()
    };
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("keepalive-failure", "0.0.0", config).await;

    let (mut client, line) =
        keepalive_log_for_new_connection(addr, "remote keepalive setup failed").await;

    assert!(line.contains("WARN"), "the failure is a warning: {line}");
    assert!(
        line.contains(&format!("errno={}", libc::EINVAL)),
        "the warning names the errno: {line}"
    );
    let health = Request::make("health", method::DAEMON_HEALTH, Value::Null);
    let payload = ok_payload(exchange(&mut client, &health).await);
    assert_eq!(payload["daemon_version"], "0.0.0");
    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A `session.wait_turn` request line that a live shell session never satisfies.
fn held_turn_wait_line(session_id: &SessionId, timeout_ms: u32, id: &str) -> String {
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

/// Whether `session_id`'s only turn waiter slot is taken, asked on a fresh
/// connection with a probe that returns at once either way.
async fn turn_slot_taken(addr: SocketAddr, session_id: &SessionId) -> bool {
    let mut probe = connect_tcp(addr).await;
    let request: ProtocolRequest =
        serde_json::from_str(&held_turn_wait_line(session_id, 1, "slot-probe"))
            .expect("parse the probe request");
    match exchange(&mut probe, &request).await.into_result() {
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

/// A peer that resets its connection during `session.wait_turn` frees the turn
/// waiter slot long before the wait's own deadline.
///
/// The reset surfaces on the daemon as the read error that a failed keepalive
/// probe also produces.
#[tokio::test]
async fn wait_turn_over_tcp_releases_its_slot_when_the_peer_resets() {
    /// Far beyond the harness hang guard, so only a cancelled wait frees the slot.
    const HELD_WAIT_TIMEOUT_MS: u32 = 600_000;
    let config = SessionRegistryConfig {
        turn_session_waiters: 1,
        ..shell_config()
    };
    let (addr, socket, shutdown, handle) =
        spawn_dual_servers("keepalive-reset", "0.0.0", config).await;
    let mut control = connect_unix(&socket).await;
    let session = create_session(&mut control, &socket).await;

    let line = held_turn_wait_line(&session.id, HELD_WAIT_TIMEOUT_MS, "held-wait");
    let holder = tokio::sync::Mutex::new(connect_tcp(addr).await);
    holder
        .lock()
        .await
        .send(line.clone())
        .await
        .expect("send the held wait");
    pohunek_test_support::wait::wait_until("the held wait to own the turn slot", || async {
        if turn_slot_taken(addr, &session.id).await {
            return Some(());
        }
        // A probe that held the slot while the wait arrived makes the daemon
        // refuse the wait; it is sent again.
        let mut holder = holder.lock().await;
        if let Some(reply) = futures::FutureExt::now_or_never(holder.next()) {
            let reply = reply
                .expect("the held wait connection stays open")
                .expect("response framing ok");
            let response: Response = serde_json::from_str(&reply).expect("parse response");
            assert_eq!(
                response.into_result().expect_err("a refused wait").code,
                "session_waiter_limit_reached"
            );
            holder
                .send(line.clone())
                .await
                .expect("resend the held wait");
        }
        None
    })
    .await;

    let stream = holder.into_inner().into_inner();
    socket2::SockRef::from(&stream)
        .set_linger(Some(Duration::ZERO))
        .expect("a zero linger turns the close into a reset");
    drop(stream);

    pohunek_test_support::wait::wait_until(
        "the reset connection's turn slot to be released",
        || async { (!turn_slot_taken(addr, &session.id).await).then_some(()) },
    )
    .await;

    let _ = shutdown.send(());
    let _ = handle.await;
}
