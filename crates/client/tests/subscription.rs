use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use pohunek_client::protocol::{self, ErrorClass, ProtocolError, Request, Response};
use pohunek_client::{Client, ClientError};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const HOST: &str = "build-box";

static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct UnixSubscriptionDaemon {
    socket_path: PathBuf,
    request_line: oneshot::Receiver<String>,
    task: JoinHandle<()>,
    _socket_file: SocketFile,
}

#[derive(Debug)]
struct TcpSubscriptionDaemon {
    addr: SocketAddr,
    request_line: oneshot::Receiver<String>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn subscription_connect_local_returns_event_lines_until_close() {
    let request = subscribe_request("subscribe-events");
    let events = [
        json!({"v": 1, "event": "agent_state", "session_id": "s-1", "activity": "working"}),
        json!({"v": 1, "event": "attach_opened", "session_id": "s-1", "stream_id": "a-1"}),
    ];
    let event_lines: Vec<String> = events
        .iter()
        .map(|event| serde_json::to_string(event).expect("serialize event"))
        .collect();
    let daemon = spawn_unix_subscription_daemon(
        response_ok_line_for(&request, json!({"subscribed": true})),
        event_lines.clone(),
    );

    let client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local subscription test daemon");
    let mut subscription = client
        .subscribe(&request)
        .await
        .expect("subscribe succeeds");
    assert_sent_request(
        &daemon
            .request_line
            .await
            .expect("daemon received subscription request"),
        &request,
    );

    assert_eq!(
        subscription.next_line().await.expect("first event"),
        Some(event_lines[0].clone())
    );
    assert_eq!(
        subscription.next_line().await.expect("second event"),
        Some(event_lines[1].clone())
    );
    assert_eq!(subscription.next_line().await.expect("closed stream"), None);
    daemon
        .task
        .await
        .expect("subscription daemon task completed");
}

#[tokio::test]
async fn subscription_local_ack_error_maps_to_protocol_and_preserves_code() {
    let request = subscribe_request("subscribe-local-error");
    let source = ProtocolError::bad_request("subscription rejected by local daemon");
    let daemon =
        spawn_unix_subscription_daemon(response_error_line_for(&request, source.clone()), vec![]);

    let client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local subscription test daemon");
    let err = client
        .subscribe(&request)
        .await
        .expect_err("subscription ack error surfaces");

    match err {
        ClientError::Protocol(err) => assert_eq!(err.code, source.code),
        other => panic!("expected local Protocol error, got {other:?}"),
    }
    assert_sent_request(
        &daemon
            .request_line
            .await
            .expect("daemon received subscription request"),
        &request,
    );
    daemon
        .task
        .await
        .expect("subscription daemon task completed");
}

#[tokio::test]
async fn subscription_remote_ack_error_maps_to_remote_protocol_and_preserves_code() {
    let request = subscribe_request("subscribe-remote-error");
    let source = ProtocolError::new(
        ErrorClass::Runtime,
        "subscription_denied",
        "subscription rejected by remote daemon",
        Some("retry later".to_owned()),
    );
    let daemon =
        spawn_tcp_subscription_daemon(response_error_line_for(&request, source.clone()), vec![])
            .await;

    let client = Client::connect_tcp_addr(HOST, daemon.addr)
        .await
        .expect("connect tcp subscription test daemon");
    let err = client
        .subscribe(&request)
        .await
        .expect_err("subscription ack error surfaces");

    match &err {
        ClientError::RemoteProtocol { host, source } => {
            assert_eq!(host, HOST);
            assert_eq!(source.class, ErrorClass::Runtime);
            assert_eq!(source.code, "subscription_denied");
            assert_eq!(source.recover.as_deref(), Some("retry later"));
        }
        other => panic!("expected remote RemoteProtocol error, got {other:?}"),
    }
    let structured = err.to_protocol_error();
    assert_eq!(structured.class, source.class);
    assert_eq!(structured.code, source.code);
    assert_eq!(structured.recover, source.recover);
    assert_sent_request(
        &daemon
            .request_line
            .await
            .expect("daemon received subscription request"),
        &request,
    );
    daemon
        .task
        .await
        .expect("subscription daemon task completed");
}

#[tokio::test]
async fn subscription_ack_response_id_mismatch_is_rejected() {
    let request = subscribe_request("subscribe-id-mismatch");
    let daemon = spawn_unix_subscription_daemon(
        response_ok_line_for(
            &Request::new(
                "wrong-subscribe-id",
                protocol::method::SUBSCRIBE,
                Value::Null,
            ),
            json!({"subscribed": true}),
        ),
        vec![],
    );

    let client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local subscription test daemon");
    match client
        .subscribe(&request)
        .await
        .expect_err("subscription ack id mismatch is rejected")
    {
        ClientError::Framing(msg) => assert!(
            msg.contains("response id"),
            "id mismatch message is clear: {msg}"
        ),
        other => panic!("expected local Framing error, got {other:?}"),
    }
    assert_sent_request(
        &daemon
            .request_line
            .await
            .expect("daemon received subscription request"),
        &request,
    );
    daemon
        .task
        .await
        .expect("subscription daemon task completed");
}

#[tokio::test]
async fn subscription_next_event_decodes_notification_created() {
    let request = subscribe_request("subscribe-notification-created");
    let record = sample_notification_record();
    let event = protocol::Event::new(
        protocol::event::NOTIFICATION_CREATED,
        json!({ "record": serde_json::to_value(&record).expect("serialize record") }),
    );
    let event_line = serde_json::to_string(&event).expect("serialize event");
    let daemon = spawn_unix_subscription_daemon(
        response_ok_line_for(&request, json!({"subscribed": true})),
        vec![event_line],
    );

    let client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local subscription test daemon");
    let mut subscription = client
        .subscribe(&request)
        .await
        .expect("subscribe succeeds");
    assert_sent_request(
        &daemon
            .request_line
            .await
            .expect("daemon received subscription request"),
        &request,
    );

    let decoded = subscription
        .next_event()
        .await
        .expect("event decodes")
        .expect("event is present");
    assert_eq!(decoded, event);
    assert_eq!(decoded.event, protocol::event::NOTIFICATION_CREATED);
    let payload: protocol::NotificationCreatedEvent =
        serde_json::from_value(decoded.payload).expect("decode notification_created payload");
    assert_eq!(payload.record, record);

    assert!(subscription
        .next_event()
        .await
        .expect("closed stream yields Ok(None)")
        .is_none());
    daemon
        .task
        .await
        .expect("subscription daemon task completed");
}

#[tokio::test]
async fn subscription_next_event_malformed_json_returns_typed_error() {
    let request = subscribe_request("subscribe-notification-malformed");
    let daemon = spawn_unix_subscription_daemon(
        response_ok_line_for(&request, json!({"subscribed": true})),
        vec!["definitely not json".to_owned()],
    );

    let client = Client::connect_local(&daemon.socket_path)
        .await
        .expect("connect local subscription test daemon");
    let mut subscription = client
        .subscribe(&request)
        .await
        .expect("subscribe succeeds");
    assert_sent_request(
        &daemon
            .request_line
            .await
            .expect("daemon received subscription request"),
        &request,
    );

    match subscription
        .next_event()
        .await
        .expect_err("malformed event line surfaces a typed error")
    {
        ClientError::Json(_) => {}
        other => panic!("expected local Json error, got {other:?}"),
    }
    daemon
        .task
        .await
        .expect("subscription daemon task completed");
}

fn spawn_unix_subscription_daemon(
    ack_line: String,
    event_lines: Vec<String>,
) -> UnixSubscriptionDaemon {
    let socket_path = unique_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind unix subscription test daemon");
    let (request_tx, request_line) = oneshot::channel();

    let task = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept unix client");
        handle_subscription_connection(stream, request_tx, ack_line, event_lines).await;
    });

    UnixSubscriptionDaemon {
        socket_path: socket_path.clone(),
        request_line,
        task,
        _socket_file: SocketFile(socket_path),
    }
}

async fn spawn_tcp_subscription_daemon(
    ack_line: String,
    event_lines: Vec<String>,
) -> TcpSubscriptionDaemon {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind tcp subscription test daemon");
    let addr = listener
        .local_addr()
        .expect("read tcp subscription test daemon address");
    let (request_tx, request_line) = oneshot::channel();

    let task = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept tcp client");
        handle_subscription_connection(stream, request_tx, ack_line, event_lines).await;
    });

    TcpSubscriptionDaemon {
        addr,
        request_line,
        task,
    }
}

async fn handle_subscription_connection<S>(
    stream: S,
    request_tx: oneshot::Sender<String>,
    ack_line: String,
    event_lines: Vec<String>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .expect("read subscription request line");
    request_tx
        .send(trim_line_end(&request_line).to_owned())
        .expect("send subscription request line to test");

    let mut stream = reader.into_inner();
    stream
        .write_all(ack_line.as_bytes())
        .await
        .expect("write subscription ack line");
    stream
        .write_all(b"\n")
        .await
        .expect("write subscription ack newline");

    for line in event_lines {
        stream
            .write_all(line.as_bytes())
            .await
            .expect("write subscription event line");
        stream
            .write_all(b"\n")
            .await
            .expect("write subscription event newline");
    }
    stream.shutdown().await.expect("close subscription stream");
}

fn subscribe_request(id: &str) -> Request {
    Request::new(id, protocol::method::SUBSCRIBE, Value::Null)
}

fn response_ok_line_for(request: &Request, ok: Value) -> String {
    serde_json::to_string(&Response::ok(request.id.clone(), ok)).expect("serialize ok response")
}

fn response_error_line_for(request: &Request, err: ProtocolError) -> String {
    serde_json::to_string(&Response::err(request.id.clone(), err)).expect("serialize err response")
}

fn assert_sent_request(request_line: &str, expected: &Request) {
    let request: Request = serde_json::from_str(request_line).expect("parse request line");
    assert_eq!(&request, expected);
}

fn sample_notification_record() -> protocol::NotificationRecord {
    protocol::NotificationRecord {
        id: protocol::NotificationId("n-1".to_owned()),
        source: protocol::NotificationSource {
            provider: "codex".to_owned(),
            provider_event: "permission_request".to_owned(),
            host_local_source_id: "src-1".to_owned(),
        },
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

fn trim_line_end(line: &str) -> &str {
    line.trim_end_matches('\n').trim_end_matches('\r')
}

fn unique_socket_path() -> PathBuf {
    let id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "pohunek-client-subscription-{}-{id}.sock",
        process::id()
    ))
}
