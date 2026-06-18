//! Integration test: the daemon's control server answers `daemon.health` over a
//! real Unix socket using newline-delimited JSON.
//!
//! This is the milestone-2 checkpoint ("CLI `doctor` + `daemon start` talk over
//! the socket") exercised at the protocol layer: it binds the actual
//! `ControlServer` on a temp socket, connects a raw client, and verifies the
//! response carries the daemon and protocol versions. It also covers
//! stale-socket recovery and the `method_not_found` path.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use protocol::{
    AgentActivity, AgentKind, AttachHeader, Event, PROTOCOL_VERSION, Request, Response,
    SessionAttachParams, SessionAttachResult, SessionDetachParams, SessionDetachResult, SessionId,
    SessionInfo, SessionNewParams, SessionResizeParams, SessionResizeResult, SessionState,
    SessionStopResult, StateSource, event, method,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

use zagentmesh_daemon::api::{ControlServer, DaemonState, HealthInfo};
use zagentmesh_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};

/// A unique temp socket path inside a dedicated per-test directory.
///
/// The server enforces the directory's mode on bind, so the socket must live in
/// a directory we own (not `/tmp` itself, which is root-owned with the sticky
/// bit). This mirrors the real daemon, which always binds inside its own
/// `zagentmesh` runtime subdir.
fn temp_socket(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "zagentmesh-test-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test socket dir");
    dir.join("daemon.sock")
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

fn session_params() -> SessionNewParams {
    SessionNewParams {
        agent: AgentKind::Shell,
        cwd: Some(std::env::temp_dir()),
        cols: 80,
        rows: 24,
    }
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

async fn attach_session(
    framed: &mut Framed<UnixStream, LinesCodec>,
    id: &SessionId,
) -> SessionAttachResult {
    let req = Request::new(
        "session-attach",
        method::SESSION_ATTACH,
        serde_json::to_value(SessionAttachParams {
            session_id: id.clone(),
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
async fn stale_socket_is_recovered_on_bind() {
    let socket = temp_socket("stale");
    // Create a stale socket file with no listener behind it.
    {
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind stale");
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
    assert_eq!(created.agent, AgentKind::Shell);
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
    assert!(
        one_output
            .windows(b"m4-multi".len())
            .any(|window| window == b"m4-multi")
    );
    assert!(
        two_output
            .windows(b"m4-multi".len())
            .any(|window| window == b"m4-multi")
    );

    drop(raw_one);
    raw_two
        .write_all(b"printf 'm4-still-attached\n'\n")
        .await
        .expect("send marker after dropping first attach");
    let two_output = read_until_marker(&mut raw_two, b"m4-still-attached").await;
    assert!(
        two_output
            .windows(b"m4-still-attached".len())
            .any(|window| window == b"m4-still-attached")
    );

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
