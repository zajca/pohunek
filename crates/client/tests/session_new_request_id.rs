//! `Client::session_new` with `client_request_id` against a fixture daemon.
//!
//! The fixture daemon records every request it serves. The scenarios pin that a
//! client sends the key only to a daemon that advertises
//! `session_new_request_id_supported`, that the refusal for one that does not
//! is distinct, and that a replayed result decodes.

// Rust guideline compliant 2026-11-01

#![cfg(unix)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use pohunek_client::protocol::{
    self, ErrorClass, Request, Response, SessionNewParams, PROTOCOL_VERSION,
};
use pohunek_client::{Client, ClientError, ClientOptions, OriginSource};
use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// The key every scenario sends.
const REQUEST_ID: &str = "req-fixture-1";

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

fn no_origin_options() -> ClientOptions {
    ClientOptions::default().with_origin_source(OriginSource::Omitted)
}

fn capabilities(request_id_supported: bool) -> Value {
    let mut value = json!({
        "daemon_version": "fixture",
        "protocol_version": PROTOCOL_VERSION.get(),
        "supported_agents": ["shell"],
        "runtimes": [],
        "git_available": true,
        "worktree_supported": true,
        "terminal_read_supported": true,
        "output_read_supported": true,
        "session_wait_supported": true
    });
    if request_id_supported {
        value["session_new_request_id_supported"] = json!(true);
    }
    value
}

/// The `session.new` result of a replay: the session and `replayed: true`.
fn replayed_result() -> Value {
    json!({
        "id": "s-keyed-1",
        "agent": "shell",
        "agent_base": "shell",
        "capabilities": {"fork": false, "resume": false},
        "cwd": "/project/root",
        "pid": 42424,
        "state": "running",
        "state_source": "process",
        "cols": 80,
        "rows": 24,
        "created_at": "2026-11-01T00:00:00Z",
        "updated_at": "2026-11-01T00:01:00Z",
        "replayed": true
    })
}

/// Starts a fixture daemon on a fresh socket and returns the requests it serves.
fn spawn_daemon(env: &TestEnv, request_id_supported: bool) -> (std::path::PathBuf, Served) {
    let socket = env
        .socket_path("daemon.sock")
        .expect("socket path fits the sun_path limit");
    let listener = UnixListener::bind(&socket).expect("bind fixture daemon");
    let served: Served = Arc::default();
    let record = Arc::clone(&served);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _address)) = listener.accept().await else {
                return;
            };
            let record = Arc::clone(&record);
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Request = serde_json::from_str(&line).expect("parse a request");
                    record
                        .lock()
                        .expect("served requests")
                        .push((request.method().to_owned(), request.params().clone()));
                    let result = match request.method() {
                        protocol::method::DAEMON_HEALTH => json!({
                            "status": "ok",
                            "daemon_version": "fixture",
                            "protocol_version": PROTOCOL_VERSION.get()
                        }),
                        protocol::method::HOST_INSPECT => capabilities(request_id_supported),
                        protocol::method::SESSION_NEW => replayed_result(),
                        other => panic!("the client sent an unexpected request: {other}"),
                    };
                    let response = Response::ok(PROTOCOL_VERSION, request.id(), result)
                        .expect("valid fixture response");
                    let mut encoded = serde_json::to_vec(&response).expect("serialize response");
                    encoded.push(b'\n');
                    if write.write_all(&encoded).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (socket, served)
}

fn params(request_id: Option<&str>) -> SessionNewParams {
    let mut params: SessionNewParams = serde_json::from_value(json!({
        "agent": "shell", "cols": 80, "rows": 24
    }))
    .expect("valid creation parameters");
    params.client_request_id = request_id.map(str::to_owned);
    params
}

async fn connect(socket: &Path) -> Client {
    let mut client = Client::connect_local_with_options(socket, no_origin_options())
        .await
        .expect("connect fixture daemon");
    client.handshake().await.expect("negotiate protocol");
    client
}

fn methods(served: &Served) -> Vec<String> {
    served
        .lock()
        .expect("served requests")
        .iter()
        .map(|(method, _params)| method.clone())
        .collect()
}

#[tokio::test]
async fn a_daemon_without_the_flag_is_refused_before_session_new_is_sent() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, false);
    let mut client = connect(&socket).await;

    let error = client
        .session_new(params(Some(REQUEST_ID)))
        .await
        .expect_err("a daemon that would drop the key is refused");

    let ClientError::Protocol(refusal) = &error else {
        panic!("expected a protocol error, got {error:?}");
    };
    assert_eq!(refusal.code, "session_new_request_id_unsupported");
    assert_eq!(refusal.class, ErrorClass::Daemon);
    assert!(
        !methods(&served)
            .iter()
            .any(|method| method == protocol::method::SESSION_NEW),
        "no session.new reached the daemon: {:?}",
        methods(&served)
    );
}

#[tokio::test]
async fn a_daemon_with_the_flag_receives_the_key_and_a_replay_decodes() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, true);
    let mut client = connect(&socket).await;

    let created = client
        .session_new(params(Some(REQUEST_ID)))
        .await
        .expect("a daemon that honors client_request_id answers");

    assert_eq!(created.session.id.0, "s-keyed-1");
    assert_eq!(created.replayed, Some(true));
    let served = served.lock().expect("served requests");
    let (_method, sent) = served
        .iter()
        .find(|(method, _params)| method == protocol::method::SESSION_NEW)
        .expect("session.new reached the daemon");
    assert_eq!(sent["client_request_id"], json!(REQUEST_ID));
}

#[tokio::test]
async fn a_request_without_a_key_does_not_probe_the_capability() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, false);
    let mut client = connect(&socket).await;

    client
        .session_new(params(None))
        .await
        .expect("an unkeyed create needs no capability");

    assert!(
        !methods(&served)
            .iter()
            .any(|method| method == protocol::method::HOST_INSPECT),
        "no host.inspect probe: {:?}",
        methods(&served)
    );
}
