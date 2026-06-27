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

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{
    method, AttachHeader, HostCapabilities, ProtocolVersion, Request, Response,
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
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temp directory inside the test temp root.
///
/// The Unix server enforces its directory's mode on bind, so the socket must
/// live in a directory we own (not `/tmp` itself). Mirrors `health_socket.rs`.
fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pohunek-test-{tag}-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn temp_socket(tag: &str) -> PathBuf {
    temp_dir(tag).join("daemon.sock")
}

/// Build a shell session config with a short stop grace, like the Unix tests.
fn shell_config() -> SessionRegistryConfig {
    SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        stop_grace: Duration::from_millis(50),
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
    let state = DaemonState::new(HealthInfo::new(version), SessionRegistry::new(config));

    let socket = temp_socket(tag);
    let unix = ControlServer::bind_with_state(&socket, state.clone())
        .await
        .expect("unix server binds");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback tcp bind");
    let remote = RemoteServer::from_listener(listener, state);
    let addr = remote.local_addr();

    let (tx, rx) = oneshot::channel::<()>();
    let (unix_rx, remote_rx) = oneshot_fanout(rx);
    let handle = tokio::spawn(async move {
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
async fn exchange<S>(framed: &mut Framed<S, LinesCodec>, request: &Request) -> Response
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
    match response {
        Response::Ok { ok, .. } => ok,
        Response::Err { err, .. } => panic!("expected ok, got error: {err}"),
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
        metadata: std::collections::BTreeMap::new(),
    }
}

async fn create_session<S>(framed: &mut Framed<S, LinesCodec>) -> SessionInfo
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let req = Request::new(
        "session-new",
        method::SESSION_NEW,
        serde_json::to_value(session_params()).expect("serialize params"),
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
    let req = Request::new(
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
    let req = Request::new(
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

async fn detach_stream<S>(
    framed: &mut Framed<S, LinesCodec>,
    stream_id: &str,
) -> SessionDetachResult
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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

async fn input_session<S>(
    framed: &mut Framed<S, LinesCodec>,
    id: &SessionId,
    text: &str,
) -> SessionInputResult
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("remote-lifecycle", "0.0.0", shell_config()).await;

    let mut client = connect_tcp(addr).await;
    let created = create_session(&mut client).await;
    assert_eq!(created.agent, "shell");
    assert_eq!(created.state, SessionState::Running);
    assert!(created.pid > 0);

    let list_req = Request::new("session-list", method::SESSION_LIST, Value::Null);
    let list: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(&mut client, &list_req).await))
            .expect("session list");
    assert!(
        list.iter()
            .any(|session| session.id == created.id && session.state == SessionState::Running),
        "created session should appear in list over TCP: {list:?}"
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
        "TCP filtered list must return only the exact AND match, not {second:?}: {filtered:?}"
    );

    let inspected = inspect_session(&mut client, &created.id).await;
    assert_eq!(inspected.id, created.id);
    assert_eq!(inspected.pid, created.pid);

    let input = input_session(&mut client, &created.id, "echo over tcp").await;
    assert!(input.accepted);

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
async fn session_new_with_input_over_tcp_writes_text_to_shell_pty() {
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
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("remote-session-new-input", "0.0.0", config).await;

    let mut control = connect_tcp(addr).await;
    let mut params = session_params();
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
    let stop_req = Request::new(
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
    let (addr, _socket, shutdown, handle) =
        spawn_dual_servers("remote-attach", "0.0.0", shell_config()).await;

    let mut control = connect_tcp(addr).await;
    let created = create_session(&mut control).await;

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

    let stop_req = Request::new(
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
    let req = Request::new("host-inspect", method::HOST_INSPECT, Value::Null);
    let caps: HostCapabilities =
        serde_json::from_value(ok_payload(exchange(&mut client, &req).await))
            .expect("host capabilities");

    assert_eq!(caps.daemon_version, "7.7.7-test");
    assert_eq!(caps.protocol_version, PROTOCOL_VERSION);
    assert_eq!(caps.supported_agents, vec!["shell", "codex", "claude"]);
    assert_eq!(caps.worktree_supported, caps.git_available);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn daemon_health_payload_is_identical_over_unix_and_tcp() {
    let (addr, socket, shutdown, handle) =
        spawn_dual_servers("remote-parity", "5.5.5-test", shell_config()).await;

    let mut tcp_client = connect_tcp(addr).await;
    let mut unix_client = connect_unix(&socket).await;

    let health_req = Request::new("health", method::DAEMON_HEALTH, Value::Null);
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
    let mut request = Request::new("skew", method::DAEMON_HEALTH, Value::Null);
    request.v = ProtocolVersion(PROTOCOL_VERSION.get() + 1);

    match exchange(&mut client, &request).await {
        Response::Err { v, err, .. } => {
            assert_eq!(
                v, PROTOCOL_VERSION,
                "the rejection envelope must carry the daemon's own version"
            );
            assert_eq!(err.code, "version_mismatch", "stable code");
        }
        Response::Ok { .. } => panic!("an incompatible client must be rejected, not served"),
    }

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn bind_rejects_non_netbird_address() {
    // A loopback address is never a NetBird address; bind must fail closed BEFORE
    // opening any socket. Deterministic without a NetBird interface present.
    let state = DaemonState::new(
        HealthInfo::new("0.0.0"),
        SessionRegistry::new(SessionRegistryConfig::default()),
    );
    let addr: SocketAddr = "127.0.0.1:18722".parse().expect("parse loopback addr");

    let result = RemoteServer::bind(addr, state).await;
    match result {
        Err(DaemonError::NetbirdBind { addr: rejected, .. }) => {
            assert_eq!(rejected.to_string(), "127.0.0.1");
        }
        Err(other) => panic!("expected NetbirdBind error, got: {other}"),
        Ok(_) => panic!("expected bind to fail closed on a non-NetBird address"),
    }
}
