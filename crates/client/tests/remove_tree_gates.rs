//! `Client::session_remove_tree` against a fixture daemon: the request
//! travels only to a daemon that advertises `session_remove_tree_supported`.
//!
//! The refusal for a daemon without the flag is distinct, carries the tree
//! request nowhere, and never sends `session.remove_tree`.

#![cfg(unix)]

use std::sync::{Arc, Mutex};

use pohunek_client::protocol::{
    self, ErrorClass, Request, Response, SessionRemoveTreeParams, PROTOCOL_VERSION,
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

fn capabilities(tree_supported: bool) -> Value {
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
    if tree_supported {
        value["session_remove_tree_supported"] = json!(true);
    }
    value
}

fn tree_result() -> Value {
    json!({
        "removed": 1,
        "kept": 0,
        "refused": 0,
        "sessions": [
            {
                "session_id": "s-root",
                "depth": 0,
                "outcome": "removed",
                "keep_reason": null,
                "accepted_unconfirmed": false
            }
        ]
    })
}

fn spawn_daemon(env: &TestEnv, tree_supported: bool) -> (std::path::PathBuf, Served) {
    let socket = env.socket_path("remove-tree-gate").expect("fixture socket");
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
                        protocol::method::HOST_INSPECT => capabilities(tree_supported),
                        protocol::method::SESSION_REMOVE_TREE => tree_result(),
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

fn tree_params() -> SessionRemoveTreeParams {
    SessionRemoveTreeParams {
        session_id: pohunek_client::protocol::SessionId("s-root".to_owned()),
        accept_unconfirmed_cleanup: false,
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
async fn a_daemon_without_the_flag_is_refused_before_the_tree_request_is_sent() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, false);
    let mut client = connect(&socket).await;

    let error = client
        .session_remove_tree(tree_params())
        .await
        .expect_err("a daemon without remove_tree support is refused");

    let ClientError::Protocol(refusal) = &error else {
        panic!("expected a protocol error, got {error:?}");
    };
    assert_eq!(refusal.code, "session_remove_tree_unsupported");
    assert_eq!(refusal.class, ErrorClass::Daemon);
    assert!(
        !methods(&served)
            .iter()
            .any(|method| method == protocol::method::SESSION_REMOVE_TREE),
        "no tree request reached the daemon: {:?}",
        methods(&served)
    );
}

#[tokio::test]
async fn a_generic_call_takes_the_gated_remove_tree_path() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, true);
    let mut client = connect(&socket).await;

    let result = client
        .call::<protocol::method::SessionRemoveTree>(tree_params())
        .await
        .expect("the generic call routes to the gated helper");

    assert_eq!(result.removed, 1);
    assert_eq!(
        methods(&served),
        vec![
            protocol::method::DAEMON_HEALTH.to_owned(),
            protocol::method::HOST_INSPECT.to_owned(),
            protocol::method::SESSION_REMOVE_TREE.to_owned(),
        ],
        "the generic call probes the capability first"
    );
}

#[tokio::test]
async fn a_daemon_with_the_flag_receives_the_tree_request() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, true);
    let mut client = connect(&socket).await;

    let result = client
        .session_remove_tree(tree_params())
        .await
        .expect("a capable daemon serves the tree request");

    assert_eq!(result.removed, 1);
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(
        methods(&served),
        vec![
            protocol::method::DAEMON_HEALTH.to_owned(),
            protocol::method::HOST_INSPECT.to_owned(),
            protocol::method::SESSION_REMOVE_TREE.to_owned(),
        ]
    );
}
