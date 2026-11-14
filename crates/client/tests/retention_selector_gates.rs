//! `Client::session_retention_sweep` against a fixture daemon: a selector
//! travels only to a daemon that advertises
//! `session_retention_selector_supported`.
//!
//! The refusal for a daemon without the flag is distinct, carries the
//! selector nowhere, and never sends `session.retention.sweep`.

#![cfg(unix)]

use std::sync::{Arc, Mutex};

use pohunek_client::protocol::{
    self, ErrorClass, Request, Response, SessionRetentionParams, PROTOCOL_VERSION,
};
use pohunek_client::{Client, ClientError, ClientOptions, OriginSource};
use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

fn no_origin_options() -> ClientOptions {
    ClientOptions::default().with_origin_source(OriginSource::Omitted)
}

fn capabilities(selector_supported: bool) -> Value {
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
    if selector_supported {
        value["session_retention_selector_supported"] = json!(true);
    }
    value
}

fn sweep_result() -> Value {
    json!({
        "dry_run": true,
        "examined": 1,
        "eligible": 0,
        "held": 0,
        "removed": 0,
        "worktrees_cleaned": 0,
        "worktrees_failed": 0,
        "failed": 0,
        "sessions": []
    })
}

fn spawn_daemon(env: &TestEnv, selector_supported: bool) -> (std::path::PathBuf, Served) {
    let socket = env
        .socket_path("selector-sweep-gate")
        .expect("fixture socket");
    let served: Served = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(&socket).expect("bind fixture socket");
    let accepted = Arc::clone(&served);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let peer = Arc::clone(&accepted);
            tokio::spawn(async move {
                let (reader, mut write) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Request = match serde_json::from_str(&line) {
                        Ok(request) => request,
                        Err(_) => return,
                    };
                    peer.lock()
                        .expect("served requests")
                        .push((request.method().to_owned(), request.params().clone()));
                    let result = match request.method() {
                        protocol::method::DAEMON_HEALTH => json!({
                            "status": "ok",
                            "daemon_version": "fixture",
                            "protocol_version": PROTOCOL_VERSION.get()
                        }),
                        protocol::method::HOST_INSPECT => capabilities(selector_supported),
                        protocol::method::SESSION_RETENTION_SWEEP => sweep_result(),
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

fn selector_params() -> SessionRetentionParams {
    SessionRetentionParams {
        dry_run: true,
        limit: None,
        spawned_by: Some(pohunek_client::protocol::SessionId("s-parent".to_owned())),
        session_ids: None,
    }
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
async fn a_daemon_without_the_flag_is_refused_before_the_sweep_is_sent() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, false);
    let mut client = connect(&socket).await;

    let error = client
        .session_retention_sweep(selector_params())
        .await
        .expect_err("a daemon that would ignore the selector is refused");

    let ClientError::Protocol(refusal) = &error else {
        panic!("expected a protocol error, got {error:?}");
    };
    assert_eq!(refusal.code, "session_retention_selector_unsupported");
    assert_eq!(refusal.class, ErrorClass::Daemon);
    assert!(
        !methods(&served)
            .iter()
            .any(|method| method == protocol::method::SESSION_RETENTION_SWEEP),
        "no sweep reached the daemon: {:?}",
        methods(&served)
    );
}

#[tokio::test]
async fn a_daemon_with_the_flag_receives_the_selector_with_the_request() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, true);
    let mut client = connect(&socket).await;

    let result = client
        .session_retention_sweep(selector_params())
        .await
        .expect("a daemon that honors the selector runs the sweep");

    assert_eq!(result.examined, 1);
    let served = served.lock().expect("served requests");
    let (_method, params) = served
        .iter()
        .find(|(method, _params)| method == protocol::method::SESSION_RETENTION_SWEEP)
        .expect("the sweep reached the daemon");
    assert_eq!(params["spawned_by"], json!("s-parent"));
}

#[tokio::test]
async fn a_generic_call_takes_the_gated_sweep_path() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, true);
    let mut client = connect(&socket).await;

    let result = client
        .call::<protocol::method::SessionRetentionSweep>(selector_params())
        .await
        .expect("the generic call routes to the gated helper");

    assert_eq!(result.examined, 1);
    assert!(
        methods(&served).contains(&protocol::method::HOST_INSPECT.to_owned()),
        "the generic call probes the capability first: {:?}",
        methods(&served)
    );
}

#[tokio::test]
async fn a_request_without_a_selector_does_not_probe_the_capability() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, false);
    let mut client = connect(&socket).await;

    client
        .session_retention_sweep(SessionRetentionParams::default())
        .await
        .expect("a plain sweep needs no capability");

    assert!(
        !methods(&served)
            .iter()
            .any(|method| method == protocol::method::HOST_INSPECT),
        "no capability probe for a selector-free sweep: {:?}",
        methods(&served)
    );
}
