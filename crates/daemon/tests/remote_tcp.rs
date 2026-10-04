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

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{
    method, AttachHeader, HostCapabilities, Request as ProtocolRequest, Response,
    SessionAttachParams, SessionAttachResult, SessionDetachParams, SessionDetachResult, SessionId,
    SessionInfo, SessionInputParams, SessionInputResult, SessionListFilter, SessionListParams,
    SessionNewParams, SessionState, SessionStopResult, PROTOCOL_VERSION,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo, RemoteServer};
use pohunek_daemon::error::DaemonError;
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::procwatch::HostInspector;
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
    mut config: SessionRegistryConfig,
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
    let registry = SessionRegistry::new_with_launcher_and_inspector(
        config,
        Arc::new(SubprocessWorkerLauncher::new()),
        Arc::new(HostInspector::new()),
    );
    let state = DaemonState::new(
        HealthInfo::new(version),
        registry,
        governance_service(&socket).await,
        support::overlay_registry(),
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
                "IFS= read -r line; printf 'got:%s\\n' \"$line\"; sleep 30",
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
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("remote-inspect", "7.7.7-test", shell_config()).await;

    let mut client = connect_tcp(addr).await;
    let req = Request::make("host-inspect", method::HOST_INSPECT, Value::Null);
    let caps: HostCapabilities =
        serde_json::from_value(ok_payload(exchange(&mut client, &req).await))
            .expect("host capabilities");

    assert_eq!(caps.daemon_version, "7.7.7-test");
    assert_eq!(caps.protocol_version, PROTOCOL_VERSION);
    assert_eq!(
        caps.supported_agents,
        vec!["shell", "codex", "claude", "hermes"]
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
async fn a_protocol_three_client_is_rejected_with_the_typed_version_error() {
    // A client still speaking the previous public protocol (`runtime_id`
    // spelling) gets the typed mismatch error and the daemon's own version
    // before any method runs, never a payload it could misread.
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("remote-protocol-three-client", "0.0.0", shell_config()).await;

    let mut client = connect_tcp(addr).await;
    let previous = PROTOCOL_VERSION.get() - 1;
    assert_eq!(previous, 3);
    let request: ProtocolRequest = serde_json::from_value(serde_json::json!({
        "v": {"minimum": previous, "maximum": previous},
        "id": "old-client",
        "method": method::SESSION_LIST,
        "params": {}
    }))
    .expect("valid previous-version request range");

    let response = exchange(&mut client, &request).await;
    assert_eq!(response.version(), PROTOCOL_VERSION);
    let err = response
        .into_result()
        .expect_err("a protocol 3 client must be rejected");
    assert_eq!(err.code, "version_mismatch");
    assert!(err.recover.is_some(), "the error carries a recovery hint");

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn bind_rejects_non_netbird_address() {
    // A loopback address is never a NetBird address; bind must fail closed BEFORE
    // opening any socket. Deterministic without a NetBird interface present.
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
    );
    let addr: SocketAddr = "127.0.0.1:18722".parse().expect("parse loopback addr");

    let transport = netbird::NetbirdTransport::new();
    let result = RemoteServer::bind(addr, state, &transport).await;
    match result {
        Err(DaemonError::OverlayBind { addr: rejected, .. }) => {
            assert_eq!(rejected.to_string(), "127.0.0.1");
        }
        Err(other) => panic!("expected OverlayBind error, got: {other}"),
        Ok(_) => panic!("expected bind to fail closed on a non-overlay address"),
    }
}
