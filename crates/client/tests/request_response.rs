use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pohunek_client::protocol::{
    self, ErrorClass, ProtocolError, Request, Response, MAX_CONTROL_LINE_BYTES,
};
use pohunek_client::{next_request_id, Client, ClientError, ClientOptions};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const HOST: &str = "build-box";
static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
enum Reply {
    Line(String),
    Close,
    OversizedLine,
}

#[derive(Debug)]
struct UnixDaemon {
    socket_path: PathBuf,
    request_line: oneshot::Receiver<String>,
    task: JoinHandle<()>,
    _socket_file: SocketFile,
}

#[derive(Debug)]
struct TcpDaemon {
    addr: SocketAddr,
    request_line: oneshot::Receiver<String>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct LateResponseDaemon {
    socket_path: PathBuf,
    first_request_line: oneshot::Receiver<String>,
    second_request_line: oneshot::Receiver<Option<String>>,
    task: JoinHandle<()>,
    _socket_file: SocketFile,
}

#[derive(Debug)]
struct ReusableDaemon {
    socket_path: PathBuf,
    first_request_line: oneshot::Receiver<String>,
    second_request_line: oneshot::Receiver<Option<String>>,
    task: JoinHandle<()>,
    _socket_file: SocketFile,
}

#[derive(Debug)]
struct EchoDaemon {
    socket_path: PathBuf,
    request_line: oneshot::Receiver<String>,
    task: JoinHandle<()>,
    _socket_file: SocketFile,
}

#[derive(Debug)]
struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn request_response_connect_local_sends_json_request_line_and_returns_ok_payload() {
    let (result, request_line) = run_local(Reply::Line(response_ok_line())).await;

    assert_eq!(result.expect("local request succeeds"), ok_payload());
    assert_sent_request(&request_line);
}

#[tokio::test]
async fn request_response_connect_tcp_addr_sends_json_request_line_and_returns_ok_payload() {
    let (result, request_line) = run_remote(Reply::Line(response_ok_line())).await;

    assert_eq!(result.expect("remote request succeeds"), ok_payload());
    assert_sent_request(&request_line);
}

#[tokio::test]
async fn request_response_rejects_a_response_outside_the_selected_protocol_range() {
    let legacy_response = json!({
        "v": 1,
        "id": "req-request-response",
        "ok": { "status": "ok" },
    });
    let (result, _) = run_local(Reply::Line(legacy_response.to_string())).await;

    match result.expect_err("wrong response version must fail") {
        ClientError::Protocol(source) => assert_eq!(source.code, "version_mismatch"),
        other => panic!("expected canonical protocol mismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn remote_wrong_version_success_preserves_host_and_canonical_mismatch() {
    let response = json!({
        "v": 1,
        "id": "req-request-response",
        "ok": { "status": "ok" },
    });
    let (result, _) = run_remote(Reply::Line(response.to_string())).await;

    let error = result.expect_err("wrong remote response version must fail");
    match &error {
        ClientError::RemoteProtocol { host, source } => {
            assert_eq!(host, HOST);
            assert_eq!(source.code, "version_mismatch");
            assert!(source.msg.contains("2..=2"));
            assert!(source.msg.contains("1..=1"));
        }
        other => panic!("expected remote protocol mismatch, got {other:?}"),
    }
    assert_eq!(error.to_protocol_error().code, "version_mismatch");
}

#[tokio::test]
async fn remote_wrong_version_noncanonical_error_becomes_canonical_mismatch() {
    let response = json!({
        "v": 1,
        "id": "req-request-response",
        "err": {
            "class": "daemon",
            "code": "bad_request",
            "msg": "legacy error",
        },
    });
    let (result, _) = run_remote(Reply::Line(response.to_string())).await;

    match result.expect_err("wrong remote error version must fail") {
        ClientError::RemoteProtocol { host, source } => {
            assert_eq!(host, HOST);
            assert_eq!(source.code, "version_mismatch");
            assert!(source.msg.contains("2..=2"));
            assert!(source.msg.contains("1..=1"));
        }
        other => panic!("expected remote protocol mismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn canonical_negotiation_mismatch_is_not_masked_by_response_version_validation() {
    let legacy = protocol::ProtocolVersion::new(1).expect("nonzero legacy version");
    let legacy_range =
        protocol::ProtocolVersionRange::new(legacy, legacy).expect("valid exact legacy range");
    let source =
        ProtocolError::version_mismatch(protocol::SUPPORTED_PROTOCOL_VERSIONS, legacy_range);
    let response = Response::err(legacy, test_request().id(), source.clone())
        .expect("valid canonical mismatch response");
    let line = serde_json::to_string(&response).expect("serialize canonical mismatch");

    let (local, _) = run_local(Reply::Line(line.clone())).await;
    match local.expect_err("local mismatch must fail") {
        ClientError::Protocol(error) => assert_eq!(error, source),
        other => panic!("expected canonical local protocol error, got {other:?}"),
    }

    let (remote, _) = run_remote(Reply::Line(line)).await;
    match remote.expect_err("remote mismatch must fail") {
        ClientError::RemoteProtocol {
            host,
            source: error,
        } => {
            assert_eq!(host, HOST);
            assert_eq!(error, source);
        }
        other => panic!("expected canonical remote protocol error, got {other:?}"),
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the test keeps both accepted connections and their wire assertions adjacent"
)]
async fn waiting_output_uses_a_dedicated_connection() {
    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind unix daemon");
    let socket_file = SocketFile(socket_path.clone());

    let task = tokio::spawn(async move {
        let (first, _) = listener.accept().await.expect("accept primary connection");
        let mut first = BufReader::new(first);
        let mut first_line = String::new();
        first
            .read_line(&mut first_line)
            .await
            .expect("read primary request");
        let first_request: Request =
            serde_json::from_str(trim_line_end(&first_line)).expect("parse primary request");
        let health = Response::ok(
            protocol::PROTOCOL_VERSION,
            first_request.id(),
            json!({
                "status": "ok",
                "daemon_version": "test",
                "protocol_version": protocol::PROTOCOL_VERSION.get(),
            }),
        )
        .expect("create health response");
        first
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&health).expect("serialize health")
                )
                .as_bytes(),
            )
            .await
            .expect("write health response");

        let (second, _) = listener
            .accept()
            .await
            .expect("accept dedicated connection");
        let mut second = BufReader::new(second);
        let mut second_line = String::new();
        second
            .read_line(&mut second_line)
            .await
            .expect("read waiting output request");
        let second_request: Request = serde_json::from_str(trim_line_end(&second_line))
            .expect("parse waiting output request");
        let runtime =
            protocol::SessionRuntimeIdentity::new("runtime-1", protocol::RuntimeGeneration::new(1))
                .expect("valid runtime identity");
        let output = protocol::SessionOutputResult::new(
            protocol::SessionId("s-1".to_owned()),
            runtime,
            protocol::OutputOffset::new(0),
            protocol::OutputOffset::new(0),
            protocol::OutputOffset::new(0),
            protocol::OutputOffset::new(0),
            "",
            None,
            false,
            true,
        )
        .expect("valid empty output result");
        let response = Response::ok(
            protocol::PROTOCOL_VERSION,
            second_request.id(),
            serde_json::to_value(output).expect("serialize output"),
        )
        .expect("create output response");
        second
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&response).expect("serialize output")
                )
                .as_bytes(),
            )
            .await
            .expect("write output response");

        (first_request, second_request)
    });

    let mut client = Client::connect_local(&socket_path)
        .await
        .expect("connect test daemon");
    client.handshake().await.expect("negotiate protocol");
    let runtime =
        protocol::SessionRuntimeIdentity::new("runtime-1", protocol::RuntimeGeneration::new(1))
            .expect("valid runtime identity");
    let params = protocol::SessionOutputParams::new(
        protocol::SessionId("s-1".to_owned()),
        Some(runtime),
        Some(protocol::OutputOffset::new(0)),
        1,
        Some(1),
    )
    .expect("valid waiting output params");
    let output = client
        .session_output(params)
        .await
        .expect("waiting output succeeds");

    assert!(output.timed_out());
    let (first_request, second_request) = task.await.expect("daemon task completed");
    assert_eq!(first_request.method(), protocol::method::DAEMON_HEALTH);
    assert_eq!(second_request.method(), protocol::method::SESSION_OUTPUT);
    drop(socket_file);
}

#[tokio::test]
async fn waiting_output_uses_a_dedicated_remote_tcp_connection() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind tcp daemon");
    let addr = listener.local_addr().expect("read tcp daemon address");
    let task = tokio::spawn(async move {
        let (first, _) = listener
            .accept()
            .await
            .expect("accept primary tcp connection");
        let mut first = BufReader::new(first);
        let mut first_line = String::new();
        first
            .read_line(&mut first_line)
            .await
            .expect("read handshake");
        let handshake: Request =
            serde_json::from_str(trim_line_end(&first_line)).expect("parse handshake");
        write_health_reply(first.get_mut(), handshake.id()).await;

        let (second, _) = listener
            .accept()
            .await
            .expect("accept dedicated tcp connection");
        let mut second = BufReader::new(second);
        let mut second_line = String::new();
        second
            .read_line(&mut second_line)
            .await
            .expect("read waiting output");
        let output_request: Request =
            serde_json::from_str(trim_line_end(&second_line)).expect("parse waiting output");
        let output = empty_output_result();
        let response = Response::ok(
            protocol::PROTOCOL_VERSION,
            output_request.id(),
            serde_json::to_value(output).expect("serialize output"),
        )
        .expect("create output response");
        second
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&response).expect("serialize response")
                )
                .as_bytes(),
            )
            .await
            .expect("write output response");
        (handshake, output_request)
    });

    let mut client = Client::connect_tcp_addr(HOST, addr)
        .await
        .expect("connect tcp daemon");
    client.handshake().await.expect("negotiate protocol");
    assert!(client
        .session_output(waiting_output_params())
        .await
        .expect("waiting output succeeds")
        .timed_out());
    let (handshake, output) = task.await.expect("daemon task completed");
    assert_eq!(handshake.method(), protocol::method::DAEMON_HEALTH);
    assert_eq!(output.method(), protocol::method::SESSION_OUTPUT);
}

#[tokio::test]
async fn cancelling_session_wait_keeps_the_shared_connection_usable() {
    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind unix daemon");
    let socket_file = SocketFile(socket_path.clone());

    let task = tokio::spawn(async move {
        let (first, _) = listener.accept().await.expect("accept primary connection");
        let mut first = BufReader::new(first);
        let mut first_line = String::new();
        first
            .read_line(&mut first_line)
            .await
            .expect("read handshake request");
        let handshake: Request =
            serde_json::from_str(trim_line_end(&first_line)).expect("parse handshake request");
        write_health_reply(first.get_mut(), handshake.id()).await;

        let (second, _) = listener
            .accept()
            .await
            .expect("accept dedicated connection");
        let mut second = BufReader::new(second);
        let mut second_line = String::new();
        second
            .read_line(&mut second_line)
            .await
            .expect("read waiting output request");
        let waiting: Request = serde_json::from_str(trim_line_end(&second_line))
            .expect("parse waiting output request");
        assert_eq!(waiting.method(), protocol::method::SESSION_WAIT);

        let mut discarded = [0_u8; 1];
        assert_eq!(
            second
                .read(&mut discarded)
                .await
                .expect("read cancellation"),
            0
        );

        let mut second_primary_line = String::new();
        first
            .read_line(&mut second_primary_line)
            .await
            .expect("read shared follow-up request");
        let follow_up: Request = serde_json::from_str(trim_line_end(&second_primary_line))
            .expect("parse shared follow-up request");
        write_health_reply(first.get_mut(), follow_up.id()).await;
        follow_up
    });

    let mut client = Client::connect_local(&socket_path)
        .await
        .expect("connect test daemon");
    client.handshake().await.expect("negotiate protocol");
    let params = session_wait_params();
    tokio::select! {
        result = client.session_wait(params) => panic!("session wait unexpectedly completed: {result:?}"),
        () = tokio::time::sleep(Duration::from_millis(10)) => {}
    }

    client
        .call::<protocol::method::DaemonHealth>(())
        .await
        .expect("shared connection remains usable after cancellation");
    let follow_up = task.await.expect("daemon task completed");
    assert_eq!(follow_up.method(), protocol::method::DAEMON_HEALTH);
    drop(socket_file);
}

fn waiting_output_params() -> protocol::SessionOutputParams {
    let runtime =
        protocol::SessionRuntimeIdentity::new("runtime-1", protocol::RuntimeGeneration::new(1))
            .expect("valid runtime identity");
    protocol::SessionOutputParams::new(
        protocol::SessionId("s-1".to_owned()),
        Some(runtime),
        Some(protocol::OutputOffset::new(0)),
        1,
        Some(1_000),
    )
    .expect("valid waiting output params")
}

fn empty_output_result() -> protocol::SessionOutputResult {
    let runtime =
        protocol::SessionRuntimeIdentity::new("runtime-1", protocol::RuntimeGeneration::new(1))
            .expect("valid runtime identity");
    protocol::SessionOutputResult::new(
        protocol::SessionId("s-1".to_owned()),
        runtime,
        protocol::OutputOffset::new(0),
        protocol::OutputOffset::new(0),
        protocol::OutputOffset::new(0),
        protocol::OutputOffset::new(0),
        "",
        None,
        false,
        true,
    )
    .expect("valid empty output result")
}

fn session_wait_params() -> protocol::SessionWaitParams {
    protocol::SessionWaitParams::new(
        protocol::SessionId("s-1".to_owned()),
        None,
        Some("2026-08-04T12:00:00Z".to_owned()),
        None,
        None,
        None,
        None,
        1_000,
    )
    .expect("valid session wait params")
}

async fn write_health_reply<S>(stream: &mut S, id: &str)
where
    S: AsyncWrite + Unpin,
{
    let health = Response::ok(
        protocol::PROTOCOL_VERSION,
        id,
        json!({
            "status": "ok",
            "daemon_version": "test",
            "protocol_version": protocol::PROTOCOL_VERSION.get(),
        }),
    )
    .expect("create health response");
    stream
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&health).expect("serialize health response")
            )
            .as_bytes(),
        )
        .await
        .expect("write health response");
}

#[test]
fn request_response_client_options_default_timeout_matches_convenience_apis() {
    assert_eq!(
        ClientOptions::default().request_timeout,
        Duration::from_secs(5)
    );
    assert_eq!(
        ClientOptions::default().connect_timeout,
        Duration::from_secs(5)
    );
    assert_eq!(
        ClientOptions::default()
            .with_request_timeout(Duration::from_millis(50))
            .request_timeout,
        Duration::from_millis(50)
    );
    assert_eq!(
        ClientOptions::default()
            .with_connect_timeout(Duration::from_millis(75))
            .connect_timeout,
        Duration::from_millis(75)
    );
}

#[test]
fn request_response_sdk_request_ids_are_method_prefixed_and_unique() {
    let first = next_request_id(protocol::method::DAEMON_HEALTH);
    let second = next_request_id(protocol::method::DAEMON_HEALTH);

    assert!(first.starts_with("sdk-daemon.health-"), "id: {first}");
    assert!(second.starts_with("sdk-daemon.health-"), "id: {second}");
    assert_ne!(first, second);
}

#[tokio::test]
async fn request_response_typed_call_sends_method_params_and_decodes_output() {
    let daemon = spawn_unix_echo_daemon(json!([
        {
            "name": "host-a",
            "fqdn": "host-a.example.test",
            "netbird_ip": "100.1.2.3",
            "classification": "candidate"
        }
    ]));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local test daemon");

    let records = client
        .call::<protocol::method::HostDiscover>(protocol::HostDiscoverParams { force: true })
        .await
        .expect("typed call succeeds");

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name.as_deref(), Some("host-a"));
    assert_eq!(records[0].class, protocol::HostClass::Candidate);
    let request_line = daemon
        .request_line
        .await
        .expect("test daemon received a request");
    daemon.task.await.expect("test daemon task completed");
    let request: Request = serde_json::from_str(&request_line).expect("parse request");
    assert_eq!(request.method(), protocol::method::HOST_DISCOVER);
    assert_eq!(request.params(), &json!({"force": true}));
    assert!(
        request.id().starts_with("sdk-host.discover-"),
        "id: {}",
        request.id()
    );
}

#[tokio::test]
async fn integration_status_sdk_helper_sends_typed_read_only_request() {
    let daemon = spawn_unix_echo_daemon(json!({
        "agents": [{
            "agent": "codex",
            "available": false,
            "expected_asset_paths": [
                "/isolated/.codex/pohunek-agent-state.sh",
                "/isolated/.codex/pohunek-agent-notify.sh"
            ],
            "present_asset_paths": [],
            "registration_paths": [
                "/isolated/.codex/hooks.json",
                "/isolated/.codex/config.toml"
            ],
            "installed_version": null,
            "expected_version": 4,
            "state": "not_installed",
            "warnings": ["agent config directory does not exist"]
        }]
    }));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local test daemon");

    let result = client
        .integration_status(protocol::IntegrationStatusParams {
            agent: Some(protocol::AgentKind::Codex),
        })
        .await
        .expect("integration status succeeds");

    assert_eq!(result.agents.len(), 1);
    assert_eq!(result.agents[0].agent, protocol::AgentKind::Codex);
    assert_eq!(
        result.agents[0].state,
        protocol::IntegrationInstallState::NotInstalled
    );
    let request_line = daemon
        .request_line
        .await
        .expect("test daemon received a request");
    daemon.task.await.expect("test daemon task completed");
    let request: Request = serde_json::from_str(&request_line).expect("parse request");
    assert_eq!(request.method(), protocol::method::INTEGRATION_STATUS);
    assert_eq!(request.params(), &json!({"agent": "codex"}));
}

#[tokio::test]
async fn request_response_typed_call_reports_output_deserialization_errors() {
    let daemon = spawn_unix_echo_daemon(json!({
        "status": "ok",
        "daemon_version": "0.15.1",
        "protocol_version": "not-a-number"
    }));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local test daemon");

    match client
        .call::<protocol::method::DaemonHealth>(())
        .await
        .expect_err("invalid typed payload is rejected")
    {
        ClientError::Json(_) => {}
        other => panic!("expected typed decode Json error, got {other:?}"),
    }
    let _request_line = daemon
        .request_line
        .await
        .expect("test daemon received a request");
    daemon.task.await.expect("test daemon task completed");
}

#[tokio::test]
async fn request_response_handshake_returns_daemon_protocol_version() {
    let daemon = spawn_unix_echo_daemon(json!({
        "status": "ok",
        "daemon_version": "0.15.1",
        "protocol_version": protocol::PROTOCOL_VERSION
    }));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local test daemon");

    let version = client.handshake().await.expect("handshake succeeds");

    assert_eq!(version, protocol::PROTOCOL_VERSION);
    let request_line = daemon
        .request_line
        .await
        .expect("test daemon received a request");
    daemon.task.await.expect("test daemon task completed");
    let request: Request = serde_json::from_str(&request_line).expect("parse request");
    assert_eq!(request.method(), protocol::method::DAEMON_HEALTH);
    assert_eq!(request.params(), &Value::Null);
}

#[tokio::test]
async fn request_response_local_daemon_error_maps_to_protocol_and_preserves_code() {
    let source = ProtocolError::bad_request("invalid request from test");

    let (result, _request_line) = run_local(Reply::Line(response_error_line(source.clone()))).await;

    match result.expect_err("local daemon error surfaces") {
        ClientError::Protocol(err) => assert_eq!(err.code, source.code),
        other => panic!("expected local Protocol error, got {other:?}"),
    }
}

#[tokio::test]
async fn request_response_remote_daemon_error_maps_to_remote_protocol_with_source_contract() {
    let source = ProtocolError::new(
        ErrorClass::Runtime,
        "agent_failed",
        "agent failed during test",
        Some("retry the request".to_owned()),
    );

    let (result, _request_line) =
        run_remote(Reply::Line(response_error_line(source.clone()))).await;

    let err = result.expect_err("remote daemon error surfaces");
    match &err {
        ClientError::RemoteProtocol { host, source } => {
            assert_eq!(host, HOST);
            assert_eq!(source.class, ErrorClass::Runtime);
            assert_eq!(source.code, "agent_failed");
            assert_eq!(source.recover.as_deref(), Some("retry the request"));
        }
        other => panic!("expected remote RemoteProtocol error, got {other:?}"),
    }

    let structured = err.to_protocol_error();
    assert_eq!(structured.class, source.class);
    assert_eq!(structured.code, source.code);
    assert_eq!(structured.recover, source.recover);
    assert!(
        structured.msg.contains(HOST),
        "structured message names host: {}",
        structured.msg
    );
}

#[tokio::test]
async fn request_response_closed_connection_before_reply_maps_by_transport() {
    let (local_result, _request_line) = run_local(Reply::Close).await;
    match local_result.expect_err("local close is an error") {
        ClientError::Framing(msg) => assert!(
            msg.contains("closed"),
            "local framing message describes close: {msg}"
        ),
        other => panic!("expected local Framing error, got {other:?}"),
    }

    let (remote_result, _request_line) = run_remote(Reply::Close).await;
    match remote_result.expect_err("remote close is an error") {
        ClientError::RemoteDaemonUnavailable { host } => assert_eq!(host, HOST),
        other => panic!("expected remote RemoteDaemonUnavailable error, got {other:?}"),
    }
}

#[tokio::test]
async fn request_response_garbled_reply_maps_by_transport() {
    let (local_result, _request_line) =
        run_local(Reply::Line("definitely not json".to_owned())).await;
    match local_result.expect_err("local garbled reply is an error") {
        ClientError::Json(_) => {}
        other => panic!("expected local Json error, got {other:?}"),
    }

    let (remote_result, _request_line) =
        run_remote(Reply::Line("definitely not json".to_owned())).await;
    match remote_result.expect_err("remote garbled reply is an error") {
        ClientError::RemoteDaemonUnavailable { host } => assert_eq!(host, HOST),
        other => panic!("expected remote RemoteDaemonUnavailable error, got {other:?}"),
    }
}

#[tokio::test]
async fn request_response_oversized_line_maps_by_transport() {
    let (local_result, _request_line) = run_local(Reply::OversizedLine).await;
    match local_result.expect_err("local oversized reply is an error") {
        ClientError::Framing(msg) => assert!(
            msg.contains("maximum length"),
            "local framing message describes limit: {msg}"
        ),
        other => panic!("expected local Framing error, got {other:?}"),
    }

    let (remote_result, _request_line) = run_remote(Reply::OversizedLine).await;
    match remote_result.expect_err("remote oversized reply is an error") {
        ClientError::RemoteDaemonUnavailable { host } => assert_eq!(host, HOST),
        other => panic!("expected remote RemoteDaemonUnavailable error, got {other:?}"),
    }
}

#[tokio::test]
async fn request_response_timeout_poisons_connection_before_late_response_can_be_reused() {
    let first_request = request_with_id("req-timeout-1");
    let second_request = request_with_id("req-timeout-2");
    let daemon = spawn_late_response_daemon(
        response_ok_line_for(&first_request, json!({"request": 1})),
        Duration::from_millis(60),
    );
    let options = ClientOptions::default().with_request_timeout(Duration::from_millis(20));
    let mut client = Client::connect_local_with_options(&daemon.socket_path, options)
        .await
        .expect("connect local test daemon");

    match client
        .request(&first_request)
        .await
        .expect_err("first request times out")
    {
        ClientError::RequestTimeout { host, timeout } => {
            assert_eq!(host, None);
            assert_eq!(timeout, Duration::from_millis(20));
        }
        other => panic!("expected typed request timeout, got {other:?}"),
    }
    assert_sent_specific_request(
        &daemon
            .first_request_line
            .await
            .expect("daemon saw first request"),
        &first_request,
    );

    tokio::time::sleep(Duration::from_millis(80)).await;

    match client
        .request(&second_request)
        .await
        .expect_err("timed-out connection must be poisoned")
    {
        ClientError::Framing(msg) => assert!(
            msg.contains("unusable"),
            "poisoned connection message is clear: {msg}"
        ),
        other => panic!("expected poisoned Framing error, got {other:?}"),
    }

    let maybe_second_request = daemon
        .second_request_line
        .await
        .expect("daemon reports whether it saw a second request");
    assert_eq!(
        maybe_second_request, None,
        "poisoned client must fail before sending a second request"
    );
    daemon.task.await.expect("late response daemon completed");
}

#[tokio::test]
async fn request_response_id_mismatch_maps_by_transport() {
    let wrong_id_reply = response_ok_line_for(&request_with_id("wrong-response-id"), ok_payload());

    let (local_result, _request_line) = run_local(Reply::Line(wrong_id_reply.clone())).await;
    match local_result.expect_err("local id mismatch is an error") {
        ClientError::Framing(msg) => assert!(
            msg.contains("response id"),
            "local id mismatch message is clear: {msg}"
        ),
        other => panic!("expected local Framing error, got {other:?}"),
    }

    let (remote_result, _request_line) = run_remote(Reply::Line(wrong_id_reply)).await;
    match remote_result.expect_err("remote id mismatch is an error") {
        ClientError::RemoteDaemonUnavailable { host } => assert_eq!(host, HOST),
        other => panic!("expected remote RemoteDaemonUnavailable error, got {other:?}"),
    }
}

#[tokio::test]
async fn request_response_id_mismatch_poisons_connection_before_it_can_be_reused() {
    let first_request = request_with_id("req-id-mismatch-1");
    let second_request = request_with_id("req-id-mismatch-2");
    let wrong_id_reply = response_ok_line_for(&request_with_id("wrong-response-id"), ok_payload());
    let daemon = spawn_reusable_daemon(wrong_id_reply);
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local reusable daemon");

    match client
        .request(&first_request)
        .await
        .expect_err("first request sees id mismatch")
    {
        ClientError::Framing(msg) => assert!(
            msg.contains("response id"),
            "id mismatch message is clear: {msg}"
        ),
        other => panic!("expected local Framing error, got {other:?}"),
    }
    assert_sent_specific_request(
        &daemon
            .first_request_line
            .await
            .expect("daemon saw first request"),
        &first_request,
    );

    match client
        .request(&second_request)
        .await
        .expect_err("id-mismatched connection must be poisoned")
    {
        ClientError::Framing(msg) => assert!(
            msg.contains("unusable"),
            "poisoned connection message is clear: {msg}"
        ),
        other => panic!("expected poisoned Framing error, got {other:?}"),
    }

    let maybe_second_request = daemon
        .second_request_line
        .await
        .expect("daemon reports whether it saw a second request");
    assert_eq!(
        maybe_second_request, None,
        "poisoned client must fail before sending a second request"
    );
    daemon.task.await.expect("reusable daemon completed");
}

#[tokio::test]
async fn request_response_create_notification_sends_method_and_returns_typed_result() {
    let params = sample_create_params();
    let expected = protocol::NotificationCreateResult {
        created: true,
        record: sample_notification_record(),
    };
    let daemon = spawn_echo_ok_daemon(serde_json::to_value(&expected).expect("serialize result"));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local echo daemon");

    let result = client
        .create_notification(params.clone())
        .await
        .expect("create_notification succeeds");

    assert_eq!(result, expected);
    let sent = parse_sent_request(&daemon.request_line.await.expect("daemon saw request"));
    assert_eq!(sent.method(), protocol::method::NOTIFICATION_CREATE);
    assert_eq!(
        sent.params(),
        &serde_json::to_value(&params).expect("serialize params")
    );
    daemon.task.await.expect("echo daemon completed");
}

#[tokio::test]
async fn request_response_list_notifications_sends_method_and_returns_typed_result() {
    let params = protocol::NotificationListParams {
        status: Some(protocol::NotificationStatus::Unread),
        ..protocol::NotificationListParams::default()
    };
    let expected = protocol::NotificationListResult {
        notifications: vec![sample_notification_record()],
        next_cursor: Some("cursor-1".to_owned()),
    };
    let daemon = spawn_echo_ok_daemon(serde_json::to_value(&expected).expect("serialize result"));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local echo daemon");

    let result = client
        .list_notifications(params.clone())
        .await
        .expect("list_notifications succeeds");

    assert_eq!(result, expected);
    let sent = parse_sent_request(&daemon.request_line.await.expect("daemon saw request"));
    assert_eq!(sent.method(), protocol::method::NOTIFICATION_LIST);
    assert_eq!(
        sent.params(),
        &serde_json::to_value(&params).expect("serialize params")
    );
    daemon.task.await.expect("echo daemon completed");
}

#[tokio::test]
async fn request_response_update_notification_sends_method_and_returns_typed_result() {
    let params = protocol::NotificationUpdateParams {
        id: protocol::NotificationId("n-1".to_owned()),
        status: protocol::NotificationStatus::Read,
    };
    let expected = protocol::NotificationUpdateResult {
        record: sample_notification_record(),
    };
    let daemon = spawn_echo_ok_daemon(serde_json::to_value(&expected).expect("serialize result"));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local echo daemon");

    let result = client
        .update_notification(params.clone())
        .await
        .expect("update_notification succeeds");

    assert_eq!(result, expected);
    let sent = parse_sent_request(&daemon.request_line.await.expect("daemon saw request"));
    assert_eq!(sent.method(), protocol::method::NOTIFICATION_UPDATE);
    assert_eq!(
        sent.params(),
        &serde_json::to_value(&params).expect("serialize params")
    );
    daemon.task.await.expect("echo daemon completed");
}

#[tokio::test]
async fn request_response_delete_notification_sends_method_and_returns_typed_result() {
    let params = protocol::NotificationDeleteParams {
        id: protocol::NotificationId("n-1".to_owned()),
    };
    let expected = protocol::NotificationDeleteResult {
        id: protocol::NotificationId("n-1".to_owned()),
        deleted: true,
    };
    let daemon = spawn_echo_ok_daemon(serde_json::to_value(&expected).expect("serialize result"));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local echo daemon");

    let result = client
        .delete_notification(params.clone())
        .await
        .expect("delete_notification succeeds");

    assert_eq!(result, expected);
    let sent = parse_sent_request(&daemon.request_line.await.expect("daemon saw request"));
    assert_eq!(sent.method(), protocol::method::NOTIFICATION_DELETE);
    assert_eq!(
        sent.params(),
        &serde_json::to_value(&params).expect("serialize params")
    );
    daemon.task.await.expect("echo daemon completed");
}

#[tokio::test]
async fn request_response_get_notification_policy_sends_null_params_and_returns_policy() {
    let expected = protocol::NotificationPolicyResult {
        policy: sample_policy(),
    };
    let daemon = spawn_echo_ok_daemon(serde_json::to_value(&expected).expect("serialize result"));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local echo daemon");

    let result = client
        .get_notification_policy()
        .await
        .expect("get_notification_policy succeeds");

    assert_eq!(result, expected);
    let sent = parse_sent_request(&daemon.request_line.await.expect("daemon saw request"));
    assert_eq!(sent.method(), protocol::method::NOTIFICATION_POLICY_GET);
    assert_eq!(sent.params(), &Value::Null);
    daemon.task.await.expect("echo daemon completed");
}

#[tokio::test]
async fn request_response_set_notification_policy_sends_method_and_returns_policy() {
    let params = protocol::NotificationPolicyParams {
        policy: sample_policy(),
    };
    let expected = protocol::NotificationPolicyResult {
        policy: sample_policy(),
    };
    let daemon = spawn_echo_ok_daemon(serde_json::to_value(&expected).expect("serialize result"));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local echo daemon");

    let result = client
        .set_notification_policy(params.clone())
        .await
        .expect("set_notification_policy succeeds");

    assert_eq!(result, expected);
    let sent = parse_sent_request(&daemon.request_line.await.expect("daemon saw request"));
    assert_eq!(sent.method(), protocol::method::NOTIFICATION_POLICY_SET);
    assert_eq!(
        sent.params(),
        &serde_json::to_value(&params).expect("serialize params")
    );
    daemon.task.await.expect("echo daemon completed");
}

#[tokio::test]
async fn request_response_prune_notifications_sends_method_and_returns_typed_result() {
    let params = protocol::NotificationRetentionParams {
        dry_run: true,
        status: Some(protocol::NotificationStatus::Archived),
        before: Some("2026-01-01T00:00:00Z".to_owned()),
        limit: Some(10),
    };
    let expected = protocol::NotificationRetentionResult {
        dry_run: true,
        pruned: vec![protocol::NotificationId("n-1".to_owned())],
    };
    let daemon = spawn_echo_ok_daemon(serde_json::to_value(&expected).expect("serialize result"));
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local echo daemon");

    let result = client
        .prune_notifications(params.clone())
        .await
        .expect("prune_notifications succeeds");

    assert_eq!(result, expected);
    let sent = parse_sent_request(&daemon.request_line.await.expect("daemon saw request"));
    assert_eq!(
        sent.method(),
        protocol::method::NOTIFICATION_RETENTION_PRUNE
    );
    assert_eq!(
        sent.params(),
        &serde_json::to_value(&params).expect("serialize params")
    );
    daemon.task.await.expect("echo daemon completed");
}

async fn run_local(reply: Reply) -> (Result<Value, ClientError>, String) {
    let daemon = spawn_unix_daemon(reply);
    let mut client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local test daemon");
    let result = client.request(&test_request()).await;
    let request_line = daemon
        .request_line
        .await
        .expect("test daemon received a request");
    daemon.task.await.expect("test daemon task completed");
    (result, request_line)
}

async fn run_remote(reply: Reply) -> (Result<Value, ClientError>, String) {
    let daemon = spawn_tcp_daemon(reply).await;
    let mut client = Client::connect_tcp_addr(HOST, daemon.addr)
        .await
        .expect("connect tcp test daemon");
    let result = client.request(&test_request()).await;
    let request_line = daemon
        .request_line
        .await
        .expect("test daemon received a request");
    daemon.task.await.expect("test daemon task completed");
    (result, request_line)
}

fn spawn_unix_daemon(reply: Reply) -> UnixDaemon {
    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind unix test daemon");
    let (request_tx, request_line) = oneshot::channel();

    let task = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept unix client");
        handle_connection(stream, request_tx, reply).await;
    });

    UnixDaemon {
        socket_path: socket_path.clone(),
        request_line,
        task,
        _socket_file: SocketFile(socket_path),
    }
}

fn spawn_unix_echo_daemon(ok: Value) -> UnixDaemon {
    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind unix echo test daemon");
    let (request_tx, request_line) = oneshot::channel();

    let task = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept unix client");
        handle_echo_connection(stream, request_tx, ok).await;
    });

    UnixDaemon {
        socket_path: socket_path.clone(),
        request_line,
        task,
        _socket_file: SocketFile(socket_path),
    }
}

async fn spawn_tcp_daemon(reply: Reply) -> TcpDaemon {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind tcp test daemon");
    let addr = listener.local_addr().expect("read tcp test daemon address");
    let (request_tx, request_line) = oneshot::channel();

    let task = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept tcp client");
        handle_connection(stream, request_tx, reply).await;
    });

    TcpDaemon {
        addr,
        request_line,
        task,
    }
}

fn spawn_late_response_daemon(reply_line: String, delay: Duration) -> LateResponseDaemon {
    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind unix test daemon");
    let (first_request_tx, first_request_line) = oneshot::channel();
    let (second_request_tx, second_request_line) = oneshot::channel();

    let task = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept unix client");
        handle_late_response_connection(
            stream,
            first_request_tx,
            second_request_tx,
            reply_line,
            delay,
        )
        .await;
    });

    LateResponseDaemon {
        socket_path: socket_path.clone(),
        first_request_line,
        second_request_line,
        task,
        _socket_file: SocketFile(socket_path),
    }
}

fn spawn_reusable_daemon(first_reply_line: String) -> ReusableDaemon {
    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind unix reusable test daemon");
    let (first_request_tx, first_request_line) = oneshot::channel();
    let (second_request_tx, second_request_line) = oneshot::channel();

    let task = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept unix client");
        handle_reusable_connection(
            stream,
            first_request_tx,
            second_request_tx,
            first_reply_line,
        )
        .await;
    });

    ReusableDaemon {
        socket_path: socket_path.clone(),
        first_request_line,
        second_request_line,
        task,
        _socket_file: SocketFile(socket_path),
    }
}

fn spawn_echo_ok_daemon(ok_payload: Value) -> EchoDaemon {
    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind unix echo test daemon");
    let (request_tx, request_line) = oneshot::channel();

    let task = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept unix client");
        handle_echo_connection(stream, request_tx, ok_payload).await;
    });

    EchoDaemon {
        socket_path: socket_path.clone(),
        request_line,
        task,
        _socket_file: SocketFile(socket_path),
    }
}

async fn handle_echo_connection<S>(stream: S, request_tx: oneshot::Sender<String>, ok: Value)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .expect("read request line");
    let request: Request =
        serde_json::from_str(trim_line_end(&request_line)).expect("parse request line");
    request_tx
        .send(trim_line_end(&request_line).to_owned())
        .expect("send request line to test");

    let line = response_ok_line_for(&request, ok);
    let mut stream = reader.into_inner();
    stream
        .write_all(line.as_bytes())
        .await
        .expect("write echo reply line");
    stream
        .write_all(b"\n")
        .await
        .expect("write echo reply newline");
    stream.shutdown().await.expect("close echo reply stream");
}

async fn handle_connection<S>(stream: S, request_tx: oneshot::Sender<String>, reply: Reply)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .expect("read request line");
    request_tx
        .send(trim_line_end(&request_line).to_owned())
        .expect("send request line to test");

    let mut stream = reader.into_inner();
    match reply {
        Reply::Line(line) => {
            stream
                .write_all(line.as_bytes())
                .await
                .expect("write reply line");
            stream.write_all(b"\n").await.expect("write reply newline");
            stream.shutdown().await.expect("close reply stream");
        }
        Reply::Close => {}
        Reply::OversizedLine => {
            let line = vec![b'a'; MAX_CONTROL_LINE_BYTES + 1];
            stream
                .write_all(&line)
                .await
                .expect("write oversized reply");
            stream
                .write_all(b"\n")
                .await
                .expect("write oversized reply newline");
            stream.shutdown().await.expect("close reply stream");
        }
    }
}

async fn handle_late_response_connection<S>(
    stream: S,
    first_request_tx: oneshot::Sender<String>,
    second_request_tx: oneshot::Sender<Option<String>>,
    reply_line: String,
    delay: Duration,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut first_request_line = String::new();
    reader
        .read_line(&mut first_request_line)
        .await
        .expect("read first request line");
    first_request_tx
        .send(trim_line_end(&first_request_line).to_owned())
        .expect("send first request line to test");

    tokio::time::sleep(delay).await;

    let stream = reader.get_mut();
    stream
        .write_all(reply_line.as_bytes())
        .await
        .expect("write late reply line");
    stream
        .write_all(b"\n")
        .await
        .expect("write late reply newline");

    let mut second_request_line = String::new();
    let second_request = match tokio::time::timeout(
        Duration::from_millis(80),
        reader.read_line(&mut second_request_line),
    )
    .await
    {
        Ok(Ok(0)) | Err(_) => None,
        Ok(Ok(_bytes)) => Some(trim_line_end(&second_request_line).to_owned()),
        Ok(Err(err)) => panic!("read second request line failed: {err}"),
    };
    second_request_tx
        .send(second_request)
        .expect("send second request result to test");
}

async fn handle_reusable_connection<S>(
    stream: S,
    first_request_tx: oneshot::Sender<String>,
    second_request_tx: oneshot::Sender<Option<String>>,
    first_reply_line: String,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut first_request_line = String::new();
    reader
        .read_line(&mut first_request_line)
        .await
        .expect("read first request line");
    first_request_tx
        .send(trim_line_end(&first_request_line).to_owned())
        .expect("send first request line to test");

    let stream = reader.get_mut();
    stream
        .write_all(first_reply_line.as_bytes())
        .await
        .expect("write first reply line");
    stream
        .write_all(b"\n")
        .await
        .expect("write first reply newline");

    let mut second_request_line = String::new();
    let second_request = match tokio::time::timeout(
        Duration::from_millis(80),
        reader.read_line(&mut second_request_line),
    )
    .await
    {
        Ok(Ok(0)) | Err(_) => None,
        Ok(Ok(_bytes)) => Some(trim_line_end(&second_request_line).to_owned()),
        Ok(Err(err)) => panic!("read second request line failed: {err}"),
    };
    second_request_tx
        .send(second_request)
        .expect("send second request result to test");
}

fn test_request() -> Request {
    request_with_id("req-request-response")
}

fn request_with_id(id: &str) -> Request {
    Request::new(id, protocol::method::DAEMON_HEALTH, json!({"ping": true}))
        .expect("valid test request")
}

fn ok_payload() -> Value {
    json!({"status": "ok"})
}

fn response_ok_line() -> String {
    response_ok_line_for(&test_request(), ok_payload())
}

fn response_ok_line_for(request: &Request, ok: Value) -> String {
    serde_json::to_string(
        &Response::ok(protocol::PROTOCOL_VERSION, request.id(), ok).expect("valid test response"),
    )
    .expect("serialize ok response")
}

fn response_error_line(err: ProtocolError) -> String {
    serde_json::to_string(
        &Response::err(protocol::PROTOCOL_VERSION, test_request().id(), err)
            .expect("valid test response"),
    )
    .expect("serialize err response")
}

fn assert_sent_request(request_line: &str) {
    assert_sent_specific_request(request_line, &test_request());
}

fn assert_sent_specific_request(request_line: &str, expected: &Request) {
    let request: Request = serde_json::from_str(request_line).expect("parse request line");
    assert_eq!(&request, expected);
}

fn parse_sent_request(request_line: &str) -> Request {
    serde_json::from_str(request_line).expect("parse request line")
}

fn sample_notification_source() -> protocol::NotificationSource {
    protocol::NotificationSource {
        provider: "codex".to_owned(),
        provider_event: "permission_request".to_owned(),
        host_local_source_id: "src-1".to_owned(),
    }
}

fn sample_create_params() -> protocol::NotificationCreateParams {
    protocol::NotificationCreateParams {
        source: sample_notification_source(),
        kind: protocol::NotificationKind::ApprovalRequired,
        severity: protocol::NotificationSeverity::ActionRequired,
        title: "Approval required".to_owned(),
        body: "Codex is waiting for approval".to_owned(),
        session_id: None,
        agent_kind: None,
        source_id: None,
        dedupe_key: Some("dedupe-1".to_owned()),
        project_id: None,
        metadata: BTreeMap::new(),
    }
}

fn sample_notification_record() -> protocol::NotificationRecord {
    protocol::NotificationRecord {
        id: protocol::NotificationId("n-1".to_owned()),
        source: sample_notification_source(),
        kind: protocol::NotificationKind::ApprovalRequired,
        severity: protocol::NotificationSeverity::ActionRequired,
        status: protocol::NotificationStatus::Unread,
        title: "Approval required".to_owned(),
        body: "Codex is waiting for approval".to_owned(),
        metadata: BTreeMap::new(),
        created_at: "2026-07-03T00:00:00Z".to_owned(),
        session_id: None,
        agent_kind: None,
        source_id: None,
        dedupe_key: Some("dedupe-1".to_owned()),
        project_id: None,
        read_at: None,
        acked_at: None,
        archived_at: None,
        deleted_at: None,
        superseded_by: None,
    }
}

fn sample_policy() -> protocol::NotificationPolicy {
    protocol::NotificationPolicy {
        attention_dedupe_window_secs: 30,
        attention_debounce_secs: 5,
        enabled: protocol::NotificationKindPolicy {
            agent_blocked: true,
            approval_required: true,
            turn_completed: false,
            session_finished: false,
            error: true,
            system: false,
        },
        providers: BTreeMap::new(),
        retention: protocol::NotificationRetentionPolicy::default(),
    }
}

fn trim_line_end(line: &str) -> &str {
    line.trim_end_matches('\n').trim_end_matches('\r')
}

fn unique_socket_path() -> PathBuf {
    let id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "pohunek-client-request-response-{}-{id}.sock",
        process::id()
    ))
}
