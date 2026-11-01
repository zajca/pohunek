//! A client decodes the results and errors of a daemon that sent keys this
//! build does not know: the call succeeds with the keys it knows, and an error
//! response keeps its typed class, code, message and recovery hint.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use pohunek_client::protocol::{self, ErrorClass, SessionId};
use pohunek_client::{Client, ClientError, ClientOptions, OriginSource};
use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::task::JoinHandle;

const HOST: &str = "build-box";
const UNKNOWN_KEY: &str = "added_by_a_newer_daemon";

/// What the scripted daemon answers for one method.
#[derive(Clone)]
enum Outcome {
    Ok(Value),
    Err(Value),
}

/// A daemon that answers each method with its scripted outcome on every
/// connection, including the dedicated ones a wait opens.
struct ScriptedDaemon {
    task: JoinHandle<()>,
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn options() -> ClientOptions {
    ClientOptions::default().with_origin_source(OriginSource::Omitted)
}

fn script(outcomes: &[(&str, Outcome)]) -> Arc<HashMap<String, Outcome>> {
    let mut script: HashMap<String, Outcome> = outcomes
        .iter()
        .map(|(method, outcome)| ((*method).to_owned(), outcome.clone()))
        .collect();
    script.insert(
        protocol::method::DAEMON_HEALTH.to_owned(),
        Outcome::Ok(json!({
            "status": "ok",
            "daemon_version": "test",
            "protocol_version": protocol::PROTOCOL_VERSION.get(),
        })),
    );
    Arc::new(script)
}

async fn serve_connection<S>(stream: S, script: Arc<HashMap<String, Outcome>>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let request: protocol::Request =
            serde_json::from_str(line.trim_end()).expect("parse request line");
        let outcome = script
            .get(request.method())
            .unwrap_or_else(|| panic!("no scripted answer for {}", request.method()));
        let mut response = json!({
            "v": protocol::PROTOCOL_VERSION.get(),
            "id": request.id(),
        });
        match outcome {
            Outcome::Ok(value) => response["ok"] = value.clone(),
            Outcome::Err(value) => response["err"] = value.clone(),
        }
        let stream = reader.get_mut();
        stream
            .write_all(format!("{response}\n").as_bytes())
            .await
            .expect("write response");
    }
}

/// Connects a client over a Unix socket (`remote == false`) or loopback TCP
/// (`remote == true`) to a daemon that answers from `script`.
async fn connect(
    script: Arc<HashMap<String, Outcome>>,
    remote: bool,
) -> (Client, ScriptedDaemon, Option<TestEnv>) {
    let (client, task, env) = if remote {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp daemon");
        let addr = listener.local_addr().expect("tcp daemon address");
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept tcp client");
                tokio::spawn(serve_connection(stream, Arc::clone(&script)));
            }
        });
        let client = Client::connect_trusted_tcp_addr_with_options(HOST, addr, options())
            .await
            .expect("connect tcp daemon");
        (client, task, None)
    } else {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let socket = env
            .socket_path("daemon.sock")
            .expect("socket path fits the sun_path limit");
        let listener = UnixListener::bind(&socket).expect("bind unix daemon");
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept unix client");
                tokio::spawn(serve_connection(stream, Arc::clone(&script)));
            }
        });
        let client = Client::connect_local_with_options(&socket, options())
            .await
            .expect("connect unix daemon");
        (client, task, Some(env))
    };
    let mut client = client;
    client.handshake().await.expect("negotiate protocol");
    (client, ScriptedDaemon { task }, env)
}

fn session_id() -> SessionId {
    SessionId("s-1".to_owned())
}

fn with_unknown_key(mut value: Value) -> Value {
    value[UNKNOWN_KEY] = json!({"nested": true});
    value
}

fn session_info() -> Value {
    json!({
        "id": "s-1",
        "agent": "codex",
        "agent_base": "codex",
        "capabilities": {"fork": true, "resume": true},
        "cwd": "/workspace/pohunek",
        "pid": 42424,
        "state": "running",
        "state_source": "process",
        "cols": 80,
        "rows": 24,
        "created_at": "2026-07-08T00:00:00Z",
        "updated_at": "2026-07-08T00:01:00Z",
    })
}

fn runtime_identity() -> Value {
    json!({"worker_instance_id": "runtime-1", "runtime_generation": "2"})
}

fn with_runtime(mut value: Value) -> Value {
    let identity = runtime_identity();
    value["worker_instance_id"] = identity["worker_instance_id"].clone();
    value["runtime_generation"] = identity["runtime_generation"].clone();
    value
}

fn read_params() -> protocol::SessionReadParams {
    protocol::SessionReadParams::new(session_id(), None, None, None).expect("valid read params")
}

fn wait_params() -> protocol::SessionWaitParams {
    protocol::SessionWaitParams::new(
        session_id(),
        None,
        Some("2026-08-04T12:00:00Z".to_owned()),
        None,
        None,
        None,
        None,
        protocol::MAX_SESSION_WAIT_MS,
    )
    .expect("valid session wait params")
}

fn wait_turn_params() -> protocol::SessionWaitTurnParams {
    protocol::SessionWaitTurnParams::new(
        vec![session_id()],
        vec![protocol::SessionTurnEventKind::TurnCompleted],
        None,
        Some(protocol::MAX_SESSION_WAIT_MS),
    )
    .expect("valid turn wait params")
}

fn error_with_unknown_key() -> Value {
    with_unknown_key(json!({
        "class": "runtime",
        "code": "agent_failed",
        "msg": "agent failed during test",
        "recover": "retry the request",
    }))
}

fn assert_typed_error(error: &protocol::ProtocolError) {
    assert_eq!(error.class, ErrorClass::Runtime);
    assert_eq!(error.code, "agent_failed");
    assert_eq!(error.msg, "agent failed during test");
    assert_eq!(error.recover.as_deref(), Some("retry the request"));
}

#[tokio::test]
async fn a_local_error_response_with_an_unknown_key_is_the_typed_error() {
    let script = script(&[(
        protocol::method::SESSION_READ,
        Outcome::Err(error_with_unknown_key()),
    )]);
    let (mut client, _daemon, _env) = connect(script, false).await;

    match client
        .session_read(read_params())
        .await
        .expect_err("the daemon answered with an error")
    {
        ClientError::Protocol(error) => assert_typed_error(&error),
        other => panic!("expected the typed local error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_remote_error_response_with_an_unknown_key_is_the_typed_error() {
    let script = script(&[(
        protocol::method::SESSION_READ,
        Outcome::Err(error_with_unknown_key()),
    )]);
    let (mut client, _daemon, _env) = connect(script, true).await;

    match client
        .session_read(read_params())
        .await
        .expect_err("the daemon answered with an error")
    {
        ClientError::RemoteProtocol { host, source } => {
            assert_eq!(host, HOST);
            assert_typed_error(&source);
        }
        other => panic!("expected the typed remote error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_turn_wait_result_with_unknown_keys_decodes() {
    let result = with_unknown_key(json!({
        "reason": "event",
        "event": with_unknown_key(json!({
            "session_id": "s-1",
            "kind": "turn_completed",
            "at": "2026-08-04T12:00:00Z",
        })),
        "cursor": "t1.0.1.d-fixture",
        "progress": [with_unknown_key(json!({
            "session_id": "s-1",
            "state": "running",
            "activity": "working",
            "output_advanced": true,
        }))],
    }));
    let script = script(&[(protocol::method::SESSION_WAIT_TURN, Outcome::Ok(result))]);
    let (mut client, _daemon, _env) = connect(script, false).await;

    let decoded = client
        .session_wait_turn(wait_turn_params())
        .await
        .expect("a result with unknown keys decodes");

    assert_eq!(decoded.reason, protocol::SessionWaitTurnReason::Event);
    let event = decoded.event.expect("the event is kept");
    assert_eq!(event.kind, protocol::SessionTurnEventKind::TurnCompleted);
    assert_eq!(decoded.progress.len(), 1);
    assert!(decoded.progress[0].output_advanced);
}

#[tokio::test]
async fn session_read_screen_detection_and_wait_results_with_unknown_keys_decode() {
    let read = with_unknown_key(with_runtime(json!({
        "text": "one\ntwo",
        "source_used": "visible",
        "revision": "9",
        "alternate_screen": false,
        "lines_requested": 20,
        "truncated": false,
    })));
    let screen = with_unknown_key(with_runtime(json!({
        "session_id": "s-1",
        "worker_id": "worker-1",
        "watermark": "9",
        "dimensions": {"cols": 120, "rows": 40},
        "cursor": with_unknown_key(json!({"row": 3, "col": 7, "visible": true})),
        "alternate_screen": false,
        "visible_lines": ["one", "two"],
    })));
    let detection = with_unknown_key(json!({
        "session_id": "s-1",
        "supported_regions": ["osc_title"],
        "previews": [with_unknown_key(json!({
            "kind": "top_non_empty_lines",
            "region": "top_non_empty_lines(8)",
            "text": "Do you trust the contents of this directory?",
        }))],
    }));
    let wait = with_unknown_key(json!({
        "reason": "timeout",
        "session": session_info(),
        "terminal_watermark": "7",
        "output_offset": "8",
    }));
    let script = script(&[
        (protocol::method::SESSION_READ, Outcome::Ok(read)),
        (protocol::method::SESSION_SCREEN, Outcome::Ok(screen)),
        (protocol::method::SESSION_DETECTION, Outcome::Ok(detection)),
        (protocol::method::SESSION_WAIT, Outcome::Ok(wait)),
    ]);
    let (mut client, _daemon, _env) = connect(script, false).await;

    let read = client
        .session_read(read_params())
        .await
        .expect("session.read decodes");
    assert_eq!(read.text, "one\ntwo");

    let screen = client
        .session_screen(protocol::SessionScreenParams::new(session_id()))
        .await
        .expect("session.screen decodes");
    assert_eq!(screen.cursor.row, 3);
    assert_eq!(screen.visible_lines, ["one", "two"]);

    let detection = client
        .session_detection(protocol::SessionDetectionParams::new(session_id()))
        .await
        .expect("session.detection decodes");
    assert_eq!(detection.previews.len(), 1);

    let wait = client
        .session_wait(wait_params())
        .await
        .expect("session.wait decodes");
    assert_eq!(wait.reason, protocol::SessionWaitReason::Timeout);
}

#[tokio::test]
async fn a_session_output_result_with_unknown_keys_in_the_result_and_the_gap_decodes() {
    let output = with_unknown_key(with_runtime(json!({
        "session_id": "s-1",
        "history_start_offset": "10",
        "start_offset": "10",
        "next_offset": "10",
        "runtime_end_offset": "10",
        "data_base64": "",
        "gap": with_unknown_key(json!({"start_offset": "0", "end_offset": "10"})),
        "has_more": false,
        "timed_out": false,
    })));
    let script = script(&[(protocol::method::SESSION_OUTPUT, Outcome::Ok(output))]);
    let (mut client, _daemon, _env) = connect(script, false).await;

    let params = protocol::SessionOutputParams::new(session_id(), None, None, 4_096, None)
        .expect("valid output params");
    let decoded = client
        .session_output(params)
        .await
        .expect("session.output decodes");

    assert!(decoded.gap().is_some(), "the gap is kept");
}

#[tokio::test]
async fn a_session_list_with_an_unknown_capability_key_lists_every_session() {
    let mut newer = session_info();
    newer["capabilities"][UNKNOWN_KEY] = json!(true);
    let mut plain = session_info();
    plain["id"] = json!("s-2");
    let script = script(&[(
        protocol::method::SESSION_LIST,
        Outcome::Ok(json!([newer, plain])),
    )]);
    let (mut client, _daemon, _env) = connect(script, false).await;

    let sessions = client
        .call::<protocol::method::SessionList>(protocol::SessionListParams::default())
        .await
        .expect("the list decodes");

    let ids: Vec<&str> = sessions
        .iter()
        .map(|session| session.id.0.as_str())
        .collect();
    assert_eq!(ids, ["s-1", "s-2"]);
}
