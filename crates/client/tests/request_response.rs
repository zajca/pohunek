use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use pohunek_client::protocol::{
    self, ErrorClass, ProtocolError, Request, Response, MAX_CONTROL_LINE_BYTES,
};
use pohunek_client::{next_request_id, Client, ClientError, ClientOptions, OriginSource};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::time::{AutoAdvanceInhibitor, TIMER_TICK};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const HOST: &str = "build-box";

/// Options that send no request origin, so a test result never depends on the
/// `POHUNEK_*` variables of the developer's own session.
fn no_origin_options() -> ClientOptions {
    ClientOptions::default().with_origin_source(OriginSource::Omitted)
}
const LEGACY_PROTOCOL_VERSION: u32 = 1;
const DISCOVERED_HOST_PORT: u16 = 18_722;

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

/// A Unix socket path inside a private test root, which is removed with its
/// socket when the value drops.
///
/// [`TestEnv::socket_path`] checks the path against the 104-byte Darwin
/// `sun_path` limit, which a path below macOS's per-user temporary directory
/// would exceed.
#[derive(Debug)]
struct SocketFile {
    path: PathBuf,
    _env: TestEnv,
}

impl SocketFile {
    fn new() -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let path = env
            .socket_path("daemon.sock")
            .expect("socket path fits the sun_path limit");
        Self { path, _env: env }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[tokio::test]
async fn request_response_connect_local_sends_json_request_line_and_returns_ok_payload() {
    let (result, request_line) = run_local(Reply::Line(response_ok_line())).await;

    assert_eq!(result.expect("local request succeeds"), ok_payload());
    assert_sent_request(&request_line);
}

#[tokio::test(flavor = "current_thread")]
async fn omitted_origin_source_ignores_incomplete_process_environment() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let mut process_env = ProcessEnv::lock();
    process_env.set(protocol::ENV_SESSION_ID, "private-session-marker");
    process_env.remove(protocol::ENV_DAEMON_ID);

    let (result, request_line) = run_local(Reply::Line(response_ok_line())).await;
    drop(process_env);

    assert_eq!(
        result.expect("omitted origin ignores the environment"),
        ok_payload()
    );
    assert_sent_request(&request_line);
    assert!(!request_line.contains("private-session-marker"));
}

#[tokio::test]
async fn missing_local_daemon_reports_a_start_recovery_hint() {
    let socket_file = SocketFile::new();
    let Err(error) =
        Client::connect_local_with_options(socket_file.path(), no_origin_options()).await
    else {
        panic!("an absent local daemon must not connect");
    };
    assert!(matches!(error, ClientError::DaemonUnreachable { .. }));

    let structured = error.to_protocol_error();
    assert_eq!(structured.class, ErrorClass::Daemon);
    assert_eq!(structured.code, "daemon_unreachable");
    assert!(structured
        .recover
        .as_deref()
        .is_some_and(|hint| hint.contains("daemon start")));
}

#[tokio::test]
async fn request_response_connect_trusted_tcp_addr_sends_json_request_line_and_returns_ok_payload()
{
    let (result, request_line) = run_remote(Reply::Line(response_ok_line())).await;

    assert_eq!(result.expect("remote request succeeds"), ok_payload());
    assert_sent_request(&request_line);
}

#[tokio::test]
async fn request_response_rejects_a_response_outside_the_selected_protocol_range() {
    let legacy_response = json!({
        "v": LEGACY_PROTOCOL_VERSION,
        "id": "req-request-response",
        "ok": { "status": "ok" },
    });
    let (result, _) = run_local(Reply::Line(legacy_response.to_string())).await;

    match result.expect_err("wrong response version must fail") {
        ClientError::Protocol(source) => {
            assert_eq!(source, canonical_legacy_protocol_mismatch());
        }
        other => panic!("expected canonical protocol mismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn remote_wrong_version_success_preserves_host_and_canonical_mismatch() {
    let response = json!({
        "v": LEGACY_PROTOCOL_VERSION,
        "id": "req-request-response",
        "ok": { "status": "ok" },
    });
    let (result, _) = run_remote(Reply::Line(response.to_string())).await;

    let error = result.expect_err("wrong remote response version must fail");
    match &error {
        ClientError::RemoteProtocol { host, source } => {
            assert_eq!(host, HOST);
            assert_eq!(source, &canonical_legacy_protocol_mismatch());
        }
        other => panic!("expected remote protocol mismatch, got {other:?}"),
    }
    assert_eq!(error.to_protocol_error().code, "version_mismatch");
}

#[tokio::test]
async fn remote_wrong_version_noncanonical_error_becomes_canonical_mismatch() {
    let response = json!({
        "v": LEGACY_PROTOCOL_VERSION,
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
            assert_eq!(source, canonical_legacy_protocol_mismatch());
        }
        other => panic!("expected remote protocol mismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn canonical_negotiation_mismatch_is_not_masked_by_response_version_validation() {
    let legacy = legacy_protocol_version();
    let source = canonical_legacy_protocol_mismatch();
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
    let socket_file = SocketFile::new();
    let socket_path = socket_file.path().to_path_buf();
    let listener = UnixListener::bind(&socket_path).expect("bind unix daemon");

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

    let mut client = Client::connect_local_with_options(&socket_path, no_origin_options())
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
async fn input_wait_without_timeout_uses_dedicated_connection_and_headroom() {
    let socket_file = SocketFile::new();
    let socket_path = socket_file.path().to_path_buf();
    let listener = UnixListener::bind(&socket_path).expect("bind unix daemon");

    let task = tokio::spawn(async move {
        let (primary, _) = listener.accept().await.expect("accept primary connection");
        let mut primary = BufReader::new(primary);
        let mut handshake_line = String::new();
        primary
            .read_line(&mut handshake_line)
            .await
            .expect("read handshake request");
        let handshake: Request =
            serde_json::from_str(trim_line_end(&handshake_line)).expect("parse handshake request");
        write_health_reply(primary.get_mut(), handshake.id()).await;

        let (dedicated, _) = listener
            .accept()
            .await
            .expect("accept dedicated input wait connection");
        let mut dedicated = BufReader::new(dedicated);
        let mut input_line = String::new();
        dedicated
            .read_line(&mut input_line)
            .await
            .expect("read input wait request");
        let input: Request =
            serde_json::from_str(trim_line_end(&input_line)).expect("parse input wait request");
        tokio::time::sleep(Duration::from_millis(60)).await;
        let response = Response::ok(
            protocol::PROTOCOL_VERSION,
            input.id(),
            json!({
                "accepted": true,
                "activity": "idle",
                "activity_source": "screen",
                "runtime": {
                    "worker_instance_id": "runtime-1",
                    "runtime_generation": "1"
                },
                "activity_epoch": "d-epoch-1",
                "activity_revision": "2",
            }),
        )
        .expect("create input response");
        dedicated
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&response).expect("serialize input response")
                )
                .as_bytes(),
            )
            .await
            .expect("write input response");

        let mut follow_up_line = String::new();
        primary
            .read_line(&mut follow_up_line)
            .await
            .expect("read shared follow-up request");
        let follow_up: Request = serde_json::from_str(trim_line_end(&follow_up_line))
            .expect("parse shared follow-up request");
        write_health_reply(primary.get_mut(), follow_up.id()).await;
        (handshake, input, follow_up)
    });

    let options = no_origin_options().with_request_timeout(Duration::from_millis(20));
    let mut client = Client::connect_local_with_options(&socket_path, options)
        .await
        .expect("connect test daemon");
    client.handshake().await.expect("negotiate protocol");
    let result = client
        .session_input(protocol::SessionInputParams {
            session_id: protocol::SessionId("s-1".to_owned()),
            text: "hello".to_owned(),
            wait: Some(protocol::SessionInputWait {
                until: Some(vec![protocol::AgentActivity::Idle]),
                timeout_ms: None,
                allow_prompt_accepted: None,
            }),
        })
        .await
        .expect("default input wait receives dedicated transport headroom");
    assert_eq!(result.activity, Some(protocol::AgentActivity::Idle));
    client
        .call::<protocol::method::DaemonHealth>(())
        .await
        .expect("shared connection remains usable");

    let (handshake, input, follow_up) = task.await.expect("daemon task completed");
    assert_eq!(handshake.method(), protocol::method::DAEMON_HEALTH);
    assert_eq!(input.method(), protocol::method::SESSION_INPUT);
    assert_eq!(follow_up.method(), protocol::method::DAEMON_HEALTH);
    drop(socket_file);
}

#[tokio::test]
async fn cancelling_session_wait_keeps_the_shared_connection_usable() {
    let socket_file = SocketFile::new();
    let socket_path = socket_file.path().to_path_buf();
    let listener = UnixListener::bind(&socket_path).expect("bind unix daemon");

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

    let mut client = Client::connect_local_with_options(&socket_path, no_origin_options())
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

#[tokio::test(start_paused = true)]
async fn direct_request_timeout_assignment_bounds_session_creation_with_input() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let socket_file = SocketFile::new();
    let listener = UnixListener::bind(socket_file.path()).expect("bind fixture daemon");
    let (request_tx, request_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (primary, _) = listener.accept().await.expect("accept primary connection");
        let mut primary = BufReader::new(primary);
        let mut health_line = String::new();
        primary
            .read_line(&mut health_line)
            .await
            .expect("read health request");
        let health: Request =
            serde_json::from_str(trim_line_end(&health_line)).expect("parse health request");
        write_health_reply(primary.get_mut(), health.id()).await;

        let (dedicated, _) = listener.accept().await.expect("accept creation connection");
        let mut dedicated = BufReader::new(dedicated);
        let mut create_line = String::new();
        dedicated
            .read_line(&mut create_line)
            .await
            .expect("read creation request");
        let create: Request =
            serde_json::from_str(trim_line_end(&create_line)).expect("parse creation request");
        let create_id = create.id().to_owned();
        request_tx.send(create).expect("report creation request");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let response = Response::ok(
            protocol::PROTOCOL_VERSION,
            create_id,
            json!({"applied_input": true}),
        )
        .expect("create fixture response");
        let _ = dedicated
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&response).expect("response JSON")
                )
                .as_bytes(),
            )
            .await;
    });

    let mut options = no_origin_options();
    options.request_timeout = Duration::from_millis(20);
    let mut client = Client::connect_local_with_options(socket_file.path(), options)
        .await
        .expect("connect fixture daemon");
    client.handshake().await.expect("negotiate protocol");
    let params = serde_json::from_value(json!({
        "agent": "shell", "cols": 80, "rows": 24, "input": "hello"
    }))
    .expect("valid creation parameters");
    let creation = tokio::spawn(async move { client.session_new(params).await });
    let request = request_rx.await.expect("creation request reached daemon");
    assert_eq!(request.method(), protocol::method::SESSION_NEW);
    assert_eq!(request.params()["extended_input_ready_wait"], true);
    tokio::time::advance(Duration::from_millis(50) + TIMER_TICK).await;
    let error = creation
        .await
        .expect("creation task completed")
        .expect_err("direct assignment bounds the response wait");
    assert!(
        matches!(error, ClientError::RequestTimeout { timeout, .. } if timeout == Duration::from_millis(20))
    );
    server.await.expect("fixture daemon completed");
    inhibitor.release().await;
}

/// Daemon-side duration of a creation the fixture answers: a slow create hook,
/// far above the client's 5 s default request timeout.
const SLOW_CREATION: Duration = Duration::from_mins(6);

#[tokio::test(start_paused = true)]
async fn session_creation_without_input_outlasts_the_default_request_timeout() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let socket_file = SocketFile::new();
    let listener = UnixListener::bind(socket_file.path()).expect("bind fixture daemon");
    let (request_tx, request_rx) = oneshot::channel();
    let (reply_tx, reply_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (primary, _) = listener.accept().await.expect("accept primary connection");
        let mut primary = BufReader::new(primary);
        let mut health_line = String::new();
        primary
            .read_line(&mut health_line)
            .await
            .expect("read health request");
        let health: Request =
            serde_json::from_str(trim_line_end(&health_line)).expect("parse health request");
        write_health_reply(primary.get_mut(), health.id()).await;

        // The creation may arrive on a dedicated connection or on the primary
        // one; the test asserts which.
        let mut create_line = String::new();
        let (mut stream, dedicated) = tokio::select! {
            accepted = listener.accept() => {
                let (dedicated, _) = accepted.expect("accept creation connection");
                let mut dedicated = BufReader::new(dedicated);
                dedicated
                    .read_line(&mut create_line)
                    .await
                    .expect("read creation request");
                (dedicated, true)
            }
            read = primary.read_line(&mut create_line) => {
                read.expect("read creation request");
                (primary, false)
            }
        };
        let create: Request =
            serde_json::from_str(trim_line_end(&create_line)).expect("parse creation request");
        let create_id = create.id().to_owned();
        request_tx
            .send((create, dedicated))
            .expect("report creation request");
        reply_rx
            .await
            .expect("the test releases the creation reply");
        let response = Response::err(
            protocol::PROTOCOL_VERSION,
            create_id,
            ProtocolError::new(
                ErrorClass::Runtime,
                "fixture_create_finished",
                "the fixture daemon finished the creation",
                None,
            ),
        )
        .expect("create fixture response");
        // The client may have given up already; the reply is best effort.
        let _ = stream
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&response).expect("response JSON")
                )
                .as_bytes(),
            )
            .await;
    });

    let mut client = Client::connect_local_with_options(socket_file.path(), no_origin_options())
        .await
        .expect("connect fixture daemon");
    client.handshake().await.expect("negotiate protocol");
    let params = serde_json::from_value(json!({"agent": "shell", "cols": 80, "rows": 24}))
        .expect("valid creation parameters");
    let creation = tokio::spawn(async move { client.session_new(params).await });
    let (request, dedicated) = request_rx.await.expect("creation request reached daemon");
    assert_eq!(request.method(), protocol::method::SESSION_NEW);
    assert!(request.params().get("extended_input_ready_wait").is_none());
    tokio::time::advance(SLOW_CREATION).await;
    reply_tx.send(()).expect("release the creation reply");

    let error = creation
        .await
        .expect("creation task completed")
        .expect_err("the fixture daemon reports a typed error");
    assert!(
        matches!(&error, ClientError::Protocol(source) if source.code == "fixture_create_finished"),
        "the client waits for the daemon's reply instead of timing out: {error:?}"
    );
    assert!(dedicated, "the creation runs on a dedicated connection");
    server.await.expect("fixture daemon completed");
    inhibitor.release().await;
}

/// Daemon-side duration of a resume the fixture answers: a cleanup sweep that
/// waits out its grace, above the client's 5 s default request timeout.
const SLOW_RESUME: Duration = Duration::from_secs(30);

/// Serves one `session.resume`: see [`spawn_lifecycle_daemon`].
fn spawn_resume_daemon(
    listener: UnixListener,
    request_tx: oneshot::Sender<(Request, bool)>,
    reply: oneshot::Receiver<()>,
) -> JoinHandle<()> {
    spawn_lifecycle_daemon(listener, request_tx, reply, resumed_session_result())
}

/// The `session.resume` result the fixture answers.
fn resumed_session_result() -> Value {
    json!({"session": session_info_json("s-resume")})
}

/// A running session summary as the fixture daemon reports it.
fn session_info_json(id: &str) -> Value {
    json!({
        "id": id,
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

/// Serves one lifecycle request: a health reply on the shared connection, then
/// the request on whichever connection it arrives, answered with `result` once
/// the test releases `reply`. `request_tx` reports the request and whether it
/// arrived on a dedicated connection.
fn spawn_lifecycle_daemon(
    listener: UnixListener,
    request_tx: oneshot::Sender<(Request, bool)>,
    reply: oneshot::Receiver<()>,
    result: Value,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (primary, _) = listener.accept().await.expect("accept primary connection");
        let mut primary = BufReader::new(primary);
        let mut health_line = String::new();
        primary
            .read_line(&mut health_line)
            .await
            .expect("read health request");
        let health: Request =
            serde_json::from_str(trim_line_end(&health_line)).expect("parse health request");
        write_health_reply(primary.get_mut(), health.id()).await;

        let mut resume_line = String::new();
        let (mut stream, dedicated) = tokio::select! {
            accepted = listener.accept() => {
                let (dedicated, _) = accepted.expect("accept resume connection");
                let mut dedicated = BufReader::new(dedicated);
                dedicated
                    .read_line(&mut resume_line)
                    .await
                    .expect("read resume request");
                (dedicated, true)
            }
            read = primary.read_line(&mut resume_line) => {
                read.expect("read resume request");
                (primary, false)
            }
        };
        let resume: Request =
            serde_json::from_str(trim_line_end(&resume_line)).expect("parse resume request");
        let resume_id = resume.id().to_owned();
        request_tx
            .send((resume, dedicated))
            .expect("report resume request");
        reply.await.expect("the test releases the resume reply");
        let response = Response::ok(protocol::PROTOCOL_VERSION, resume_id, result)
            .expect("lifecycle fixture response");
        // The client may have given up already; the reply is best effort.
        let _ = stream
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&response).expect("response JSON")
                )
                .as_bytes(),
            )
            .await;
    })
}

#[tokio::test(start_paused = true)]
async fn session_resume_outlasts_the_default_request_timeout() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let socket_file = SocketFile::new();
    let listener = UnixListener::bind(socket_file.path()).expect("bind fixture daemon");
    let (request_tx, request_rx) = oneshot::channel();
    let (reply_tx, reply_rx) = oneshot::channel::<()>();
    let server = spawn_resume_daemon(listener, request_tx, reply_rx);

    let mut client = Client::connect_local_with_options(socket_file.path(), no_origin_options())
        .await
        .expect("connect fixture daemon");
    client.handshake().await.expect("negotiate protocol");
    let session_id = protocol::SessionId("s-resume".to_owned());
    let resume = tokio::spawn(async move { client.session_resume(session_id).await });
    let (request, dedicated) = request_rx.await.expect("resume request reached daemon");
    assert_eq!(request.method(), protocol::method::SESSION_RESUME);
    assert!(dedicated, "the resume runs on a dedicated connection");
    tokio::time::advance(SLOW_RESUME).await;
    reply_tx.send(()).expect("release the resume reply");

    let resumed = resume
        .await
        .expect("resume task completed")
        .expect("the client waits for the daemon's reply instead of timing out");
    assert_eq!(resumed.session.id.0, "s-resume");
    server.await.expect("fixture daemon completed");
    inhibitor.release().await;
}

/// Daemon-side duration of an acknowledged creation that used its whole
/// budget: the creation budget, then the largest delivery bound and the largest
/// acknowledgement window the daemon accepts, less one second of the client's
/// transport headroom.
fn slow_acknowledged_creation() -> Duration {
    Duration::from_millis(u64::from(pohunek_client::SESSION_NEW_REQUEST_BUDGET_MS))
        + Duration::from_millis(u64::from(protocol::MAX_SESSION_WAIT_MS) * 2)
}

#[tokio::test(start_paused = true)]
async fn session_creation_with_the_largest_input_ack_outlasts_the_creation_budget() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let socket_file = SocketFile::new();
    let listener = UnixListener::bind(socket_file.path()).expect("bind fixture daemon");
    let (request_tx, request_rx) = oneshot::channel();
    let (reply_tx, reply_rx) = oneshot::channel::<()>();
    let mut result = session_info_json("s-ack");
    result["applied_input"] = json!(true);
    result["prompt_delivered"] = json!(false);
    let server = spawn_lifecycle_daemon(listener, request_tx, reply_rx, result);

    let mut client = Client::connect_local_with_options(socket_file.path(), no_origin_options())
        .await
        .expect("connect fixture daemon");
    client.handshake().await.expect("negotiate protocol");
    let params = serde_json::from_value(json!({
        "agent": "shell", "cols": 80, "rows": 24, "input": "hello",
        "input_ack_timeout_ms": protocol::MAX_SESSION_WAIT_MS
    }))
    .expect("valid creation parameters");
    let creation = tokio::spawn(async move { client.session_new(params).await });
    let (request, dedicated) = request_rx.await.expect("creation request reached daemon");
    assert_eq!(request.method(), protocol::method::SESSION_NEW);
    assert!(dedicated, "the creation runs on a dedicated connection");
    // Past the creation budget plus the transport headroom, so a client that
    // did not add the delivery bound and the window would have given up.
    tokio::time::advance(
        slow_acknowledged_creation()
            .checked_sub(Duration::from_secs(1))
            .expect("the acknowledged creation outlasts one second"),
    )
    .await;
    reply_tx.send(()).expect("release the creation reply");

    let created = creation
        .await
        .expect("creation task completed")
        .expect("the client waits for the daemon's reply instead of timing out");
    assert_eq!(created.prompt_delivered, Some(false));
    server.await.expect("fixture daemon completed");
    inhibitor.release().await;
}

#[tokio::test(start_paused = true)]
async fn session_resume_past_its_deadline_reports_an_unknown_outcome() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let socket_file = SocketFile::new();
    let listener = UnixListener::bind(socket_file.path()).expect("bind fixture daemon");
    let (request_tx, request_rx) = oneshot::channel();
    let (reply_tx, reply_rx) = oneshot::channel::<()>();
    let server = spawn_resume_daemon(listener, request_tx, reply_rx);

    let explicit = Duration::from_secs(2);
    let mut client = Client::connect_local_with_options(
        socket_file.path(),
        no_origin_options().with_request_timeout(explicit),
    )
    .await
    .expect("connect fixture daemon");
    client.handshake().await.expect("negotiate protocol");
    let session_id = protocol::SessionId("s-resume".to_owned());
    let resume = tokio::spawn(async move { client.session_resume(session_id).await });
    request_rx.await.expect("resume request reached daemon");
    tokio::time::advance(explicit + TIMER_TICK).await;

    let error = resume
        .await
        .expect("resume task completed")
        .expect_err("the explicit deadline elapses before the daemon answers");
    assert!(
        matches!(&error, ClientError::SessionResumeOutcomeUnknown { timeout, .. } if *timeout == explicit),
        "{error:?}"
    );
    let structured = error.to_protocol_error();
    assert_eq!(structured.code, "session_resume_outcome_unknown");
    assert!(
        structured.recover.as_deref().is_some_and(
            |hint| hint.contains("may still complete") && hint.contains("session inspect")
        ),
        "{structured:?}"
    );
    reply_tx.send(()).expect("release the resume reply");
    server.await.expect("fixture daemon completed");
    inhibitor.release().await;
}

/// Daemon-side duration of a fork, stop or remove the fixture answers, above
/// the client's 5 s default request timeout.
const SLOW_LIFECYCLE: Duration = Duration::from_secs(6);

/// A fixture daemon serving one lifecycle request, held until the test
/// releases its reply.
struct LifecycleFixture {
    request_rx: oneshot::Receiver<(Request, bool)>,
    reply_tx: Option<oneshot::Sender<()>>,
    server: JoinHandle<()>,
    inhibitor: AutoAdvanceInhibitor,
    _socket_file: SocketFile,
}

impl LifecycleFixture {
    /// Starts the fixture and connects a client with `options` to it.
    async fn start(result: Value, options: ClientOptions) -> (Client, Self) {
        let inhibitor = AutoAdvanceInhibitor::new();
        let socket_file = SocketFile::new();
        let listener = UnixListener::bind(socket_file.path()).expect("bind fixture daemon");
        let (request_tx, request_rx) = oneshot::channel();
        let (reply_tx, reply_rx) = oneshot::channel::<()>();
        let server = spawn_lifecycle_daemon(listener, request_tx, reply_rx, result);
        let mut client = Client::connect_local_with_options(socket_file.path(), options)
            .await
            .expect("connect fixture daemon");
        client.handshake().await.expect("negotiate protocol");
        (
            client,
            Self {
                request_rx,
                reply_tx: Some(reply_tx),
                server,
                inhibitor,
                _socket_file: socket_file,
            },
        )
    }

    /// Waits for the lifecycle request; the flag is whether it arrived on a
    /// dedicated connection.
    async fn request(&mut self) -> (Request, bool) {
        (&mut self.request_rx)
            .await
            .expect("the lifecycle request reached the daemon")
    }

    /// Lets the daemon answer.
    fn release(&mut self) {
        self.reply_tx
            .take()
            .expect("the reply is released once")
            .send(())
            .expect("release the reply");
    }

    /// Ends the fixture after the daemon answered.
    async fn finish(self) {
        self.server.await.expect("fixture daemon completed");
        self.inhibitor.release().await;
    }
}

fn fork_params() -> protocol::SessionForkParams {
    protocol::SessionForkParams {
        session_id: protocol::SessionId("s-source".to_owned()),
        name: None,
        cwd_mode: protocol::ForkCwdMode::Same,
        cols: 80,
        rows: 24,
        accept_profile_change: false,
    }
}

fn source_session() -> protocol::SessionId {
    protocol::SessionId("s-source".to_owned())
}

fn removed_result() -> Value {
    json!({"removed": true, "stopped": true, "worktrees_removed": 0, "worktrees_failed": 0})
}

fn explicit_options(timeout: Duration) -> ClientOptions {
    no_origin_options().with_request_timeout(timeout)
}

#[tokio::test(start_paused = true)]
async fn session_fork_outlasts_the_default_request_timeout() {
    let (mut client, mut fixture) =
        LifecycleFixture::start(session_info_json("s-forked"), no_origin_options()).await;
    let fork = tokio::spawn(async move { client.session_fork(fork_params()).await });
    let (request, dedicated) = fixture.request().await;
    assert_eq!(request.method(), protocol::method::SESSION_FORK);
    assert!(dedicated, "the fork runs on a dedicated connection");
    tokio::time::advance(SLOW_LIFECYCLE).await;
    fixture.release();
    let forked = fork
        .await
        .expect("fork task completed")
        .expect("the client waits for the daemon's reply instead of timing out");
    assert_eq!(forked.session.id.0, "s-forked");
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn generic_fork_call_takes_the_dedicated_budget() {
    let (mut client, mut fixture) =
        LifecycleFixture::start(session_info_json("s-forked"), no_origin_options()).await;
    let fork = tokio::spawn(async move {
        client
            .call::<protocol::method::SessionFork>(fork_params())
            .await
    });
    let (_request, dedicated) = fixture.request().await;
    assert!(
        dedicated,
        "the generic fork call runs on a dedicated connection"
    );
    tokio::time::advance(SLOW_LIFECYCLE).await;
    fixture.release();
    let forked = fork
        .await
        .expect("fork task completed")
        .expect("the generic call waits for the daemon's reply instead of timing out");
    assert_eq!(forked.session.id.0, "s-forked");
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn session_fork_past_its_deadline_reports_an_unknown_outcome() {
    let explicit = Duration::from_millis(200);
    let (mut client, mut fixture) =
        LifecycleFixture::start(session_info_json("s-forked"), explicit_options(explicit)).await;
    let fork = tokio::spawn(async move { client.session_fork(fork_params()).await });
    fixture.request().await;
    tokio::time::advance(explicit + TIMER_TICK).await;

    let error = fork
        .await
        .expect("fork task completed")
        .expect_err("the explicit deadline elapses before the daemon answers");
    assert!(
        matches!(&error, ClientError::SessionForkOutcomeUnknown { timeout, .. } if *timeout == explicit),
        "{error:?}"
    );
    let structured = error.to_protocol_error();
    assert_eq!(structured.code, "session_fork_outcome_unknown");
    assert_eq!(structured.class, ErrorClass::Transport);
    let hint = structured.recover.as_deref().expect("a recover hint");
    assert!(
        hint.contains("may still complete") && hint.contains("session list"),
        "{hint}"
    );
    assert!(
        hint.contains("second") && hint.contains("child"),
        "a retried fork creates another child, which the hint must say: {hint}"
    );
    fixture.release();
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn session_fork_explicit_timeout_wins_over_the_lifecycle_budget() {
    let explicit = Duration::from_secs(2);
    let (mut client, mut fixture) =
        LifecycleFixture::start(session_info_json("s-forked"), explicit_options(explicit)).await;
    let fork = tokio::spawn(async move { client.session_fork(fork_params()).await });
    fixture.request().await;
    tokio::time::advance(explicit + TIMER_TICK).await;

    let error = fork
        .await
        .expect("fork task completed")
        .expect_err("the daemon answers after the explicit deadline");
    assert!(
        matches!(&error, ClientError::SessionForkOutcomeUnknown { timeout, .. } if *timeout == explicit),
        "{error:?}"
    );
    fixture.release();
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn session_stop_outlasts_the_default_request_timeout() {
    let (mut client, mut fixture) =
        LifecycleFixture::start(json!({"stopped": true}), no_origin_options()).await;
    let stop = tokio::spawn(async move { client.session_stop(source_session()).await });
    let (request, dedicated) = fixture.request().await;
    assert_eq!(request.method(), protocol::method::SESSION_STOP);
    assert!(dedicated, "the stop runs on a dedicated connection");
    tokio::time::advance(SLOW_LIFECYCLE).await;
    fixture.release();
    let stopped = stop
        .await
        .expect("stop task completed")
        .expect("the client waits for the daemon's reply instead of timing out");
    assert!(stopped.stopped);
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn generic_stop_call_takes_the_dedicated_budget() {
    let (mut client, mut fixture) =
        LifecycleFixture::start(json!({"stopped": true}), no_origin_options()).await;
    let stop = tokio::spawn(async move {
        client
            .call::<protocol::method::SessionStop>(source_session())
            .await
    });
    let (_request, dedicated) = fixture.request().await;
    assert!(
        dedicated,
        "the generic stop call runs on a dedicated connection"
    );
    tokio::time::advance(SLOW_LIFECYCLE).await;
    fixture.release();
    stop.await
        .expect("stop task completed")
        .expect("the generic call waits for the daemon's reply instead of timing out");
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn session_stop_past_its_deadline_reports_an_unknown_outcome() {
    let explicit = Duration::from_millis(300);
    let (mut client, mut fixture) =
        LifecycleFixture::start(json!({"stopped": true}), explicit_options(explicit)).await;
    let stop = tokio::spawn(async move { client.session_stop(source_session()).await });
    fixture.request().await;
    tokio::time::advance(explicit + TIMER_TICK).await;

    let error = stop
        .await
        .expect("stop task completed")
        .expect_err("the explicit deadline elapses before the daemon answers");
    assert!(
        matches!(&error, ClientError::SessionStopOutcomeUnknown { timeout, .. } if *timeout == explicit),
        "{error:?}"
    );
    let structured = error.to_protocol_error();
    assert_eq!(structured.code, "session_stop_outcome_unknown");
    assert_eq!(structured.class, ErrorClass::Transport);
    let hint = structured.recover.as_deref().expect("a recover hint");
    assert!(
        hint.contains("may still complete") && hint.contains("session inspect"),
        "{hint}"
    );
    fixture.release();
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn session_remove_outlasts_the_default_request_timeout() {
    let (mut client, mut fixture) =
        LifecycleFixture::start(removed_result(), no_origin_options()).await;
    let remove = tokio::spawn(async move { client.session_remove(source_session()).await });
    let (request, dedicated) = fixture.request().await;
    assert_eq!(request.method(), protocol::method::SESSION_REMOVE);
    assert!(dedicated, "the remove runs on a dedicated connection");
    tokio::time::advance(SLOW_LIFECYCLE).await;
    fixture.release();
    let removed = remove
        .await
        .expect("remove task completed")
        .expect("the client waits for the daemon's reply instead of timing out");
    assert!(removed.removed);
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn session_remove_accepting_unconfirmed_has_the_same_budget() {
    let (mut client, mut fixture) =
        LifecycleFixture::start(removed_result(), no_origin_options()).await;
    let remove = tokio::spawn(async move {
        client
            .session_remove_accepting_unconfirmed(source_session())
            .await
    });
    let (request, dedicated) = fixture.request().await;
    assert_eq!(
        request.method(),
        protocol::method::SESSION_REMOVE_ACCEPTING_UNCONFIRMED
    );
    assert!(
        dedicated,
        "the consenting remove runs on a dedicated connection"
    );
    tokio::time::advance(SLOW_LIFECYCLE).await;
    fixture.release();
    let removed = remove
        .await
        .expect("remove task completed")
        .expect("the client waits for the daemon's reply instead of timing out");
    assert!(removed.removed);
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn session_remove_past_its_deadline_reports_an_unknown_outcome() {
    let explicit = Duration::from_millis(200);
    for consenting in [false, true] {
        let (mut client, mut fixture) =
            LifecycleFixture::start(removed_result(), explicit_options(explicit)).await;
        let remove = tokio::spawn(async move {
            if consenting {
                client
                    .session_remove_accepting_unconfirmed(source_session())
                    .await
            } else {
                client.session_remove(source_session()).await
            }
        });
        fixture.request().await;
        tokio::time::advance(explicit + TIMER_TICK).await;

        let error = remove
            .await
            .expect("remove task completed")
            .expect_err("the explicit deadline elapses before the daemon answers");
        assert!(
            matches!(&error, ClientError::SessionRemoveOutcomeUnknown { timeout, .. } if *timeout == explicit),
            "consenting={consenting}: {error:?}"
        );
        let structured = error.to_protocol_error();
        assert_eq!(structured.code, "session_remove_outcome_unknown");
        assert_eq!(structured.class, ErrorClass::Transport);
        fixture.release();
        fixture.finish().await;
    }
}

/// Daemon-side duration of each turn wait the fixture answers, far above the
/// client's 5 s default request timeout and the 30 min default ceiling.
const TURN_WAIT_FIXTURE_DELAY: Duration = Duration::from_mins(45);

/// Turn waits the fixture daemon answers: typed helper and generic call, each
/// with an explicit and an omitted timeout.
const TURN_WAIT_FIXTURE_CALLS: usize = 4;

/// A wait request a fixture daemon received, answered once the test sends on
/// `reply`.
type HeldWait = (Request, oneshot::Sender<()>);

/// Serves the turn-wait scenario: a health reply on the shared connection,
/// then [`TURN_WAIT_FIXTURE_CALLS`] dedicated turn waits, each held until the
/// test releases it. Each dedicated request goes to `request_tx`; a request on
/// the shared connection goes to `stray_tx`.
fn spawn_turn_wait_daemon(
    listener: UnixListener,
    request_tx: tokio::sync::mpsc::UnboundedSender<HeldWait>,
    stray_tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (primary, _) = listener.accept().await.expect("accept primary connection");
        let mut primary = BufReader::new(primary);
        let mut health_line = String::new();
        primary
            .read_line(&mut health_line)
            .await
            .expect("read health request");
        let health: Request =
            serde_json::from_str(trim_line_end(&health_line)).expect("parse health request");
        write_health_reply(primary.get_mut(), health.id()).await;
        // A turn wait sent on the shared connection is reported, so a call
        // that skips the dedicated connection fails at once.
        tokio::spawn(async move {
            let mut stray = String::new();
            if primary
                .read_line(&mut stray)
                .await
                .is_ok_and(|read| read > 0)
            {
                let _ = stray_tx.send(stray);
            }
        });
        for _ in 0..TURN_WAIT_FIXTURE_CALLS {
            let (dedicated, _) = listener
                .accept()
                .await
                .expect("accept turn wait connection");
            let mut dedicated = BufReader::new(dedicated);
            let mut wait_line = String::new();
            dedicated
                .read_line(&mut wait_line)
                .await
                .expect("read turn wait request");
            let wait: Request =
                serde_json::from_str(trim_line_end(&wait_line)).expect("parse turn wait request");
            let wait_id = wait.id().to_owned();
            let (reply, release) = oneshot::channel();
            request_tx
                .send((wait, reply))
                .expect("report turn wait request");
            release.await.expect("the test releases the turn wait");
            let response = Response::ok(
                protocol::PROTOCOL_VERSION,
                wait_id,
                json!({
                    "reason": "timeout",
                    "cursor": "t1.0.1.d-fixture",
                    "progress": [{
                        "session_id": "s-1",
                        "state": "running",
                        "activity": "working",
                        "output_advanced": false
                    }]
                }),
            )
            .expect("create turn wait response");
            dedicated
                .get_mut()
                .write_all(
                    format!(
                        "{}\n",
                        serde_json::to_string(&response).expect("response JSON")
                    )
                    .as_bytes(),
                )
                .await
                .expect("write turn wait response");
        }
    })
}

#[tokio::test(start_paused = true)]
async fn turn_wait_deadline_covers_the_requested_and_the_daemon_default_duration() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let socket_file = SocketFile::new();
    let listener = UnixListener::bind(socket_file.path()).expect("bind fixture daemon");
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stray_tx, mut stray_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = spawn_turn_wait_daemon(listener, request_tx, stray_tx);

    let mut client = Client::connect_local_with_options(socket_file.path(), no_origin_options())
        .await
        .expect("connect fixture daemon");
    client.handshake().await.expect("negotiate protocol");
    let wait_minutes = u32::try_from(TURN_WAIT_FIXTURE_DELAY.as_millis()).expect("fits u32");
    // The typed helper and the generic typed call, each with an explicit and
    // an omitted timeout.
    for (generic, timeout_ms) in [
        (false, Some(wait_minutes)),
        (false, None),
        (true, Some(wait_minutes)),
        (true, None),
    ] {
        let params = protocol::SessionWaitTurnParams::new(
            vec![protocol::SessionId("s-1".to_owned())],
            vec![protocol::SessionTurnEventKind::TurnCompleted],
            None,
            timeout_ms,
        )
        .expect("valid turn wait params");
        let waiting = tokio::spawn(async move {
            let result = if generic {
                client
                    .call::<protocol::method::SessionWaitTurn>(params)
                    .await
            } else {
                client.session_wait_turn(params).await
            };
            (client, result)
        });
        let (request, reply) = tokio::select! {
            request = request_rx.recv() => request.expect("turn wait reached the daemon"),
            stray = stray_rx.recv() => panic!(
                "the turn wait (generic: {generic}) used the shared connection: {stray:?}"
            ),
        };
        assert_eq!(request.method(), protocol::method::SESSION_WAIT_TURN);
        assert_eq!(
            request.params().get("timeout_ms").and_then(Value::as_u64),
            timeout_ms.map(u64::from)
        );
        // The daemon-side wait lasts this long before it answers.
        tokio::time::advance(TURN_WAIT_FIXTURE_DELAY + TIMER_TICK).await;
        reply.send(()).expect("release the turn wait");
        let (returned, result) = waiting.await.expect("turn wait task joins");
        client = returned;
        let result = result.expect("the dedicated deadline outlasts the daemon-side wait");
        assert_eq!(result.reason, protocol::SessionWaitTurnReason::Timeout);
    }
    server.await.expect("fixture daemon completed");
    drop(socket_file);
    inhibitor.release().await;
}

/// Longer than the client's 5 s default request timeout and within the
/// protocol's wait ceiling, so only a connection sized to the wait outlasts it.
const GENERIC_WAIT_MS: u32 = protocol::MAX_SESSION_WAIT_MS;

/// Waits the generic-call fixture daemon answers: `session.wait` and a waiting
/// `session.output`.
const GENERIC_WAIT_FIXTURE_CALLS: usize = 2;

/// Serves the generic-wait scenario: a health reply on the shared connection,
/// then [`GENERIC_WAIT_FIXTURE_CALLS`] dedicated waits, each held until the
/// test releases it. Each dedicated request goes to `request_tx`; a request on
/// the shared connection goes to `stray_tx`.
fn spawn_generic_wait_daemon(
    listener: UnixListener,
    request_tx: tokio::sync::mpsc::UnboundedSender<HeldWait>,
    stray_tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (primary, _) = listener.accept().await.expect("accept primary connection");
        let mut primary = BufReader::new(primary);
        let mut health_line = String::new();
        primary
            .read_line(&mut health_line)
            .await
            .expect("read health request");
        let health: Request =
            serde_json::from_str(trim_line_end(&health_line)).expect("parse health request");
        write_health_reply(primary.get_mut(), health.id()).await;
        // A wait sent on the shared connection is reported, so a call that
        // skips the dedicated connection fails at once.
        tokio::spawn(async move {
            let mut stray = String::new();
            if primary
                .read_line(&mut stray)
                .await
                .is_ok_and(|read| read > 0)
            {
                let _ = stray_tx.send(stray);
            }
        });
        for _ in 0..GENERIC_WAIT_FIXTURE_CALLS {
            let (dedicated, _) = listener.accept().await.expect("accept wait connection");
            let mut dedicated = BufReader::new(dedicated);
            let mut wait_line = String::new();
            dedicated
                .read_line(&mut wait_line)
                .await
                .expect("read wait request");
            let wait: Request =
                serde_json::from_str(trim_line_end(&wait_line)).expect("parse wait request");
            let wait_id = wait.id().to_owned();
            let result = if wait.method() == protocol::method::SESSION_WAIT {
                generic_wait_result()
            } else {
                generic_output_result()
            };
            let (reply, release) = oneshot::channel();
            request_tx.send((wait, reply)).expect("report wait request");
            release.await.expect("the test releases the wait");
            let response = Response::ok(protocol::PROTOCOL_VERSION, wait_id, result)
                .expect("create wait response");
            dedicated
                .get_mut()
                .write_all(
                    format!(
                        "{}\n",
                        serde_json::to_string(&response).expect("response JSON")
                    )
                    .as_bytes(),
                )
                .await
                .expect("write wait response");
        }
    })
}

/// A `session.wait` result for session `s-1` whose wait timed out.
fn generic_wait_result() -> Value {
    json!({
        "reason": "timeout",
        "session": {
            "id": "s-1",
            "agent": "shell",
            "agent_base": "shell",
            "cwd": "/work",
            "pid": 4242,
            "cols": 80,
            "rows": 24,
            "state": "running",
            "state_source": "process",
            "created_at": "2026-08-04T12:00:00Z",
            "updated_at": "2026-08-04T12:00:00Z"
        }
    })
}

/// An empty `session.output` result for session `s-1` whose wait timed out.
fn generic_output_result() -> Value {
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
    serde_json::to_value(output).expect("serialize output result")
}

#[tokio::test(start_paused = true)]
async fn generic_waits_outlast_the_default_request_timeout_on_dedicated_connections() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let socket_file = SocketFile::new();
    let listener = UnixListener::bind(socket_file.path()).expect("bind fixture daemon");
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stray_tx, mut stray_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = spawn_generic_wait_daemon(listener, request_tx, stray_tx);

    let mut client = Client::connect_local_with_options(socket_file.path(), no_origin_options())
        .await
        .expect("connect fixture daemon");
    client.handshake().await.expect("negotiate protocol");
    let runtime =
        protocol::SessionRuntimeIdentity::new("runtime-1", protocol::RuntimeGeneration::new(1))
            .expect("valid runtime identity");
    let wait = protocol::SessionWaitParams::new(
        protocol::SessionId("s-1".to_owned()),
        None,
        Some("2026-08-04T12:00:00Z".to_owned()),
        None,
        None,
        None,
        None,
        GENERIC_WAIT_MS,
    )
    .expect("valid session wait params");
    let output = protocol::SessionOutputParams::new(
        protocol::SessionId("s-1".to_owned()),
        Some(runtime),
        Some(protocol::OutputOffset::new(0)),
        1,
        Some(GENERIC_WAIT_MS),
    )
    .expect("valid waiting output params");
    for method in [
        protocol::method::SESSION_WAIT,
        protocol::method::SESSION_OUTPUT,
    ] {
        let wait = wait.clone();
        let output = output.clone();
        let waiting = tokio::spawn(async move {
            let timed_out = if method == protocol::method::SESSION_WAIT {
                client
                    .call::<protocol::method::SessionWait>(wait)
                    .await
                    .map(|result| result.reason == protocol::SessionWaitReason::Timeout)
            } else {
                client
                    .call::<protocol::method::SessionOutput>(output)
                    .await
                    .map(|result| result.timed_out())
            };
            (client, timed_out)
        });
        let (request, reply) = tokio::select! {
            request = request_rx.recv() => request.expect("the wait reached the daemon"),
            stray = stray_rx.recv() => panic!("{method} used the shared connection: {stray:?}"),
        };
        assert_eq!(request.method(), method);
        // The daemon-side wait lasts its full duration before it answers.
        tokio::time::advance(Duration::from_millis(u64::from(GENERIC_WAIT_MS)) + TIMER_TICK).await;
        reply.send(()).expect("release the wait");
        let (returned, timed_out) = waiting.await.expect("wait task joins");
        client = returned;
        assert!(
            timed_out.expect("the dedicated deadline outlasts the daemon-side wait"),
            "{method}"
        );
    }
    server.await.expect("fixture daemon completed");
    drop(socket_file);
    inhibitor.release().await;
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
            "address": "100.1.2.3",
            "port": DISCOVERED_HOST_PORT,
            "overlay": "netbird",
            "peer_id": "peer-host-a",
            "classification": "candidate"
        }
    ]));
    let mut client = Client::connect_local_with_options(&daemon.socket_path, no_origin_options())
        .await
        .expect("connect local test daemon");

    let records = client
        .call::<protocol::method::HostDiscover>(protocol::HostDiscoverParams { force: true })
        .await
        .expect("typed call succeeds");

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name.as_deref(), Some("host-a"));
    assert_eq!(records[0].fqdn.as_deref(), Some("host-a.example.test"));
    assert_eq!(records[0].address.as_deref(), Some("100.1.2.3"));
    assert_eq!(records[0].port, DISCOVERED_HOST_PORT);
    assert_eq!(records[0].overlay, "netbird");
    assert_eq!(records[0].peer_id.as_deref(), Some("peer-host-a"));
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
async fn request_response_typed_call_reports_output_deserialization_errors() {
    let daemon = spawn_unix_echo_daemon(json!({
        "status": "ok",
        "daemon_version": "0.15.1",
        "protocol_version": "not-a-number"
    }));
    let mut client = Client::connect_local_with_options(&daemon.socket_path, no_origin_options())
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
    let mut client = Client::connect_local_with_options(&daemon.socket_path, no_origin_options())
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
    assert!(structured.msg.contains("agent failed during test"));
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

async fn run_local(reply: Reply) -> (Result<Value, ClientError>, String) {
    let daemon = spawn_unix_daemon(reply);
    let mut client = Client::connect_local_with_options(&daemon.socket_path, no_origin_options())
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
    let mut client =
        Client::connect_trusted_tcp_addr_with_options(HOST, daemon.addr, no_origin_options())
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
    let socket_file = SocketFile::new();
    let socket_path = socket_file.path().to_path_buf();
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
        _socket_file: socket_file,
    }
}

fn spawn_unix_echo_daemon(ok: Value) -> UnixDaemon {
    let socket_file = SocketFile::new();
    let socket_path = socket_file.path().to_path_buf();
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
        _socket_file: socket_file,
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

fn test_request() -> Request {
    request_with_id("req-request-response")
}

fn legacy_protocol_version() -> protocol::ProtocolVersion {
    protocol::ProtocolVersion::new(LEGACY_PROTOCOL_VERSION).expect("nonzero legacy version")
}

fn canonical_legacy_protocol_mismatch() -> ProtocolError {
    let legacy = legacy_protocol_version();
    let legacy_range =
        protocol::ProtocolVersionRange::new(legacy, legacy).expect("valid exact legacy range");
    ProtocolError::version_mismatch(protocol::CLIENT_PROTOCOL_VERSIONS, legacy_range)
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

fn trim_line_end(line: &str) -> &str {
    line.trim_end_matches('\n').trim_end_matches('\r')
}

fn command_run_params(timeout_ms: Option<u32>) -> protocol::CommandRunParams {
    protocol::CommandRunParams {
        package: protocol::PackageId::parse("acme.fleet-state").expect("package id"),
        command: protocol::CommandName::parse("host-state").expect("command name"),
        timeout_ms,
        max_output_bytes: None,
    }
}

fn command_run_result() -> Value {
    json!({
        "package": { "id": "acme.fleet-state", "version": "1.0.0" },
        "command": "host-state",
        "digest": format!("sha256:{}", "a".repeat(64)),
        "output": "state\n",
        "duration_ms": 30_000
    })
}

#[tokio::test(start_paused = true)]
async fn command_run_outlasts_the_default_request_timeout() {
    let (mut client, mut fixture) =
        LifecycleFixture::start(command_run_result(), no_origin_options()).await;
    let run = tokio::spawn(async move { client.command_run(command_run_params(None)).await });
    let (request, dedicated) = fixture.request().await;
    assert_eq!(request.method(), protocol::method::COMMAND_RUN);
    assert!(dedicated, "a command runs on a dedicated connection");
    // A run may take up to the protocol ceiling, far past the 5 second
    // default response deadline of a shared connection.
    tokio::time::advance(Duration::from_millis(u64::from(
        protocol::MAX_COMMAND_TIMEOUT_MS,
    )))
    .await;
    fixture.release();
    let result = run
        .await
        .expect("command task completed")
        .expect("the client waits for the daemon's reply instead of timing out");
    assert_eq!(result.output, "state\n");
    fixture.finish().await;
}

/// A daemon of the current protocol version that predates `command.run`: it
/// answers the version probe and refuses every other method as unknown.
async fn serve_without_command_run<S>(stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut stream = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match stream.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let request: Request =
            serde_json::from_str(trim_line_end(&line)).expect("parse request line");
        if request.method() == protocol::method::DAEMON_HEALTH {
            write_health_reply(stream.get_mut(), request.id()).await;
            continue;
        }
        let response = Response::err(
            protocol::PROTOCOL_VERSION,
            request.id(),
            ProtocolError::method_not_found(request.method()),
        )
        .expect("method_not_found response");
        stream
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&response).expect("serialize response")
                )
                .as_bytes(),
            )
            .await
            .expect("write method_not_found");
    }
}

#[tokio::test]
async fn a_current_protocol_daemon_without_command_run_is_command_unsupported() {
    let socket_file = SocketFile::new();
    let listener = UnixListener::bind(socket_file.path()).expect("bind fixture daemon");
    let local = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept local client");
            tokio::spawn(serve_without_command_run(stream));
        }
    });
    let mut client = Client::connect_local_with_options(socket_file.path(), no_origin_options())
        .await
        .expect("connect fixture daemon");
    let error = client
        .command_run(command_run_params(None))
        .await
        .expect_err("the daemon has no command.run");
    match error {
        ClientError::Protocol(source) => {
            assert_eq!(source.code, "command_unsupported");
            assert_eq!(source.class, ErrorClass::Daemon);
        }
        other => panic!("expected a protocol error, got {other:?}"),
    }
    // The generic typed call takes the same path.
    let error = client
        .call::<protocol::method::CommandRun>(command_run_params(None))
        .await
        .expect_err("the daemon has no command.run");
    assert!(
        matches!(&error, ClientError::Protocol(source) if source.code == "command_unsupported"),
        "{error:?}"
    );
    local.abort();

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind tcp fixture daemon");
    let addr = listener.local_addr().expect("tcp fixture address");
    let remote = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept remote client");
            tokio::spawn(serve_without_command_run(stream));
        }
    });
    let mut client = Client::connect_trusted_tcp_addr_with_options(HOST, addr, no_origin_options())
        .await
        .expect("connect tcp fixture daemon");
    let error = client
        .command_run(command_run_params(None))
        .await
        .expect_err("the remote daemon has no command.run");
    match error {
        ClientError::RemoteProtocol { host, source } => {
            assert_eq!(host, HOST);
            assert_eq!(source.code, "command_unsupported");
        }
        other => panic!("expected a remote protocol error, got {other:?}"),
    }
    remote.abort();
}
