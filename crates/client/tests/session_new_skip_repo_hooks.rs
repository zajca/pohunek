//! `Client::session_new` with `skip_repo_hooks` against a fixture daemon.
//!
//! The fixture daemon records every request it serves. The scenarios pin that
//! a client sends the switch only to a daemon that advertises
//! `session_new_skip_repo_hooks_supported`, because an older daemon would
//! drop it and run the repository's hooks, and that the refusal for such a
//! daemon is distinct from the refusal for `agent_options`.

// Rust guideline compliant 2026-11-15

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

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

fn no_origin_options() -> ClientOptions {
    ClientOptions::default().with_origin_source(OriginSource::Omitted)
}

fn capabilities(skip_repo_hooks_supported: bool) -> Value {
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
    if skip_repo_hooks_supported {
        value["session_new_skip_repo_hooks_supported"] = json!(true);
    }
    value
}

/// The `session.new` result: the session echoes the switch it was created
/// with, as the real daemon records it.
fn created_result(request: &Request) -> Value {
    let mut value = json!({
        "id": "s-skip-1",
        "agent": "work",
        "agent_base": "shell",
        "capabilities": {"fork": false, "resume": false},
        "cwd": "/project/root",
        "pid": 42424,
        "state": "running",
        "state_source": "process",
        "cols": 80,
        "rows": 24,
        "created_at": "2026-11-15T00:00:00Z",
        "updated_at": "2026-11-15T00:00:01Z"
    });
    if request.params()["skip_repo_hooks"] == json!(true) {
        value["skip_repo_hooks"] = json!(true);
    }
    value
}

/// Starts a fixture daemon on a fresh socket and returns the requests it serves.
fn spawn_daemon(env: &TestEnv, skip_repo_hooks_supported: bool) -> (std::path::PathBuf, Served) {
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
                        protocol::method::HOST_INSPECT => capabilities(skip_repo_hooks_supported),
                        protocol::method::SESSION_NEW => created_result(&request),
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

fn params(skip_repo_hooks: bool) -> SessionNewParams {
    let mut params: SessionNewParams = serde_json::from_value(json!({
        "agent": "work", "cols": 80, "rows": 24
    }))
    .expect("valid creation parameters");
    params.skip_repo_hooks = skip_repo_hooks;
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

/// The params of the `session.new` the daemon served, if any.
fn sent_session_new(served: &Served) -> Option<Value> {
    served
        .lock()
        .expect("served requests")
        .iter()
        .find(|(method, _params)| method == protocol::method::SESSION_NEW)
        .map(|(_method, params)| params.clone())
}

#[tokio::test]
async fn a_daemon_without_the_flag_is_refused_before_session_new_is_sent() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, false);
    let mut client = connect(&socket).await;

    let error = client
        .session_new(params(true))
        .await
        .expect_err("a daemon that would run the repository's hooks is refused");

    let ClientError::Protocol(refusal) = &error else {
        panic!("expected a protocol error, got {error:?}");
    };
    assert_eq!(refusal.code, "session_new_skip_repo_hooks_unsupported");
    assert_eq!(refusal.class, ErrorClass::Daemon);
    assert!(
        methods(&served).contains(&protocol::method::HOST_INSPECT.to_owned()),
        "the capability was probed: {:?}",
        methods(&served)
    );
    assert!(
        sent_session_new(&served).is_none(),
        "no session.new reached the daemon: {:?}",
        methods(&served)
    );
}

#[tokio::test]
async fn a_daemon_with_the_flag_receives_the_switch_and_the_result_decodes() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, true);
    let mut client = connect(&socket).await;

    let created = client
        .session_new(params(true))
        .await
        .expect("a daemon that honors skip_repo_hooks answers");

    assert_eq!(created.session.id.0, "s-skip-1");
    assert!(
        created.session.skip_repo_hooks,
        "the session records that its repository hooks were skipped"
    );
    let sent = sent_session_new(&served).expect("session.new reached the daemon");
    assert_eq!(sent["skip_repo_hooks"], json!(true), "{sent}");
}

#[tokio::test]
async fn a_switch_left_false_needs_no_capability_and_is_absent_on_the_wire() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, false);
    let mut client = connect(&socket).await;

    let created = client
        .session_new(params(false))
        .await
        .expect("a create without skip_repo_hooks needs no capability");

    assert!(!created.session.skip_repo_hooks);
    assert!(
        !methods(&served).contains(&protocol::method::HOST_INSPECT.to_owned()),
        "no host.inspect probe: {:?}",
        methods(&served)
    );
    let sent = sent_session_new(&served).expect("session.new reached the daemon");
    assert!(
        sent.get("skip_repo_hooks").is_none(),
        "the false switch is omitted from the params: {sent}"
    );
}
