//! `Client` attention-isolation gates against a fixture daemon.
//!
//! The fixture daemon records every request it serves. The scenarios pin that
//! a client sends the reserved `pohunek.attention.children` flag only to a
//! daemon that advertises `session_attention_isolation_supported`, that the
//! refusal for one that does not is distinct, and that clearing the key is
//! always sent.

// Rust guideline compliant 2026-11-13

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pohunek_client::protocol::{
    self, ErrorClass, Request, Response, SessionNewParams, SessionSetMetadataParams,
    PROTOCOL_VERSION,
};
use pohunek_client::{Client, ClientError, ClientOptions, OriginSource};
use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// Reserved flag every scenario sets.
const ATTENTION_KEY: &str = "pohunek.attention.children";
/// The only accepted flag value.
const ISOLATE: &str = "isolate";

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

fn no_origin_options() -> ClientOptions {
    ClientOptions::default().with_origin_source(OriginSource::Omitted)
}

fn capabilities(isolation_supported: bool) -> Value {
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
    if isolation_supported {
        value["session_attention_isolation_supported"] = json!(true);
    }
    value
}

/// The `session.new` result: the flagged session.
fn created_result() -> Value {
    json!({
        "id": "s-isol-1",
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
        "updated_at": "2026-11-01T00:00:01Z",
        "metadata": {ATTENTION_KEY: ISOLATE}
    })
}

/// The `session.set_metadata` result: the session with merged metadata.
fn metadata_result() -> Value {
    json!({
        "session": {
            "id": "s-isol-1",
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
            "updated_at": "2026-11-01T00:00:01Z"
        }
    })
}

/// Starts a fixture daemon on a fresh socket and returns the requests it serves.
fn spawn_daemon(env: &TestEnv, isolation_supported: bool) -> (std::path::PathBuf, Served) {
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
                        protocol::method::HOST_INSPECT => capabilities(isolation_supported),
                        protocol::method::SESSION_NEW => created_result(),
                        protocol::method::SESSION_SET_METADATA => metadata_result(),
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

fn new_params() -> SessionNewParams {
    let mut params: SessionNewParams = serde_json::from_value(json!({
        "agent": "shell", "cols": 80, "rows": 24
    }))
    .expect("valid creation parameters");
    params
        .metadata
        .insert(ATTENTION_KEY.to_owned(), ISOLATE.to_owned());
    params
}

fn set_params(metadata: BTreeMap<String, Option<String>>) -> SessionSetMetadataParams {
    SessionSetMetadataParams {
        session_id: protocol::SessionId("s-isol-1".to_owned()),
        metadata,
    }
}

fn set_flag() -> BTreeMap<String, Option<String>> {
    BTreeMap::from([(ATTENTION_KEY.to_owned(), Some(ISOLATE.to_owned()))])
}

fn clear_flag() -> BTreeMap<String, Option<String>> {
    BTreeMap::from([(ATTENTION_KEY.to_owned(), None)])
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
        .session_new(new_params())
        .await
        .expect_err("a daemon that would ignore the flag is refused");

    let ClientError::Protocol(refusal) = &error else {
        panic!("expected a protocol error, got {error:?}");
    };
    assert_eq!(refusal.code, "session_attention_isolation_unsupported");
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
async fn a_daemon_without_the_flag_is_refused_before_set_metadata_is_sent() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, false);
    let mut client = connect(&socket).await;

    let error = client
        .session_set_metadata(set_params(set_flag()))
        .await
        .expect_err("a daemon that would ignore the flag is refused");

    let ClientError::Protocol(refusal) = &error else {
        panic!("expected a protocol error, got {error:?}");
    };
    assert_eq!(refusal.code, "session_attention_isolation_unsupported");
    assert!(
        !methods(&served)
            .iter()
            .any(|method| method == protocol::method::SESSION_SET_METADATA),
        "no session.set_metadata reached the daemon: {:?}",
        methods(&served)
    );
}

#[tokio::test]
async fn clearing_the_key_is_sent_without_a_probe() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, false);
    let mut client = connect(&socket).await;

    client
        .session_set_metadata(set_params(clear_flag()))
        .await
        .expect("clearing the key needs no capability");

    assert!(
        !methods(&served)
            .iter()
            .any(|method| method == protocol::method::HOST_INSPECT),
        "no host.inspect probe: {:?}",
        methods(&served)
    );
    assert!(
        methods(&served)
            .iter()
            .any(|method| method == protocol::method::SESSION_SET_METADATA),
        "session.set_metadata reached the daemon: {:?}",
        methods(&served)
    );
}

#[tokio::test]
async fn a_daemon_with_the_flag_receives_session_new() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, true);
    let mut client = connect(&socket).await;

    let created = client
        .session_new(new_params())
        .await
        .expect("a daemon that honors isolation answers");

    assert_eq!(created.session.id.0, "s-isol-1");
    let served = served.lock().expect("served requests");
    let (_method, sent) = served
        .iter()
        .find(|(method, _params)| method == protocol::method::SESSION_NEW)
        .expect("session.new reached the daemon");
    assert_eq!(sent["metadata"][ATTENTION_KEY], json!(ISOLATE));
}
