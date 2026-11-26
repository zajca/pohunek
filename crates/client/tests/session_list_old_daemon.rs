//! `Client::session_list` against a daemon that predates paging.
//!
//! Such a daemon ignores `limit` and `cursor` and answers the whole current set
//! to every `session.list` request. The fixture daemon plays one scripted set
//! per request, so a set can change between the walk's requests. The client must
//! return the current set once, without duplicates, and without an error when
//! sessions were only removed.

#![cfg(unix)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use pohunek_client::protocol::{self, Request, Response, SessionListParams, PROTOCOL_VERSION};
use pohunek_client::{Client, ClientOptions, OriginSource};
use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// Sessions the client asks for per page, and so the size of a set that a
/// daemon ignoring `limit` makes look like one full page.
const CLIENT_PAGE_SIZE: usize = 200;

/// The sets the fixture daemon answers, one per `session.list` request.
type Script = Arc<Mutex<VecDeque<Vec<String>>>>;

fn session(id: &str) -> Value {
    json!({
        "id": id,
        "agent": "shell",
        "agent_base": "shell",
        "capabilities": {"fork": false, "resume": false},
        "cols": 80,
        "rows": 24,
        "cwd": "/project/root",
        "cwd_source": "launch",
        "created_at": "2026-11-26T00:00:00Z",
        "updated_at": "2026-11-26T00:00:00Z",
        "external": false,
        "pid": 42424,
        "state": "running",
        "state_source": "process"
    })
}

/// Serves `script` to `session.list` requests; a request after the script ends
/// repeats its last set.
fn spawn_daemon(env: &TestEnv, script: Vec<Vec<String>>) -> std::path::PathBuf {
    let socket = env.socket_path("old-list").expect("fixture socket");
    let script: Script = Arc::new(Mutex::new(script.into()));
    let listener = UnixListener::bind(&socket).expect("bind fixture socket");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let script = Arc::clone(&script);
            tokio::spawn(async move {
                let (reader, mut write) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(request) = serde_json::from_str::<Request>(&line) else {
                        return;
                    };
                    let result = match request.method() {
                        protocol::method::DAEMON_HEALTH => json!({
                            "status": "ok",
                            "daemon_version": "fixture",
                            "protocol_version": PROTOCOL_VERSION.get()
                        }),
                        protocol::method::SESSION_LIST => {
                            let mut script = script.lock().expect("script");
                            let ids = if script.len() > 1 {
                                script.pop_front().expect("a scripted set")
                            } else {
                                script.front().cloned().expect("a scripted set")
                            };
                            Value::Array(ids.iter().map(|id| session(id)).collect())
                        }
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
    socket
}

async fn connect(socket: &Path) -> Client {
    let options = ClientOptions::default().with_origin_source(OriginSource::Omitted);
    let mut client = Client::connect_local_with_options(socket, options)
        .await
        .expect("connect fixture daemon");
    client.handshake().await.expect("negotiate protocol");
    client
}

/// `count` ids in ascending order starting at `first`.
fn ids(first: usize, count: usize) -> Vec<String> {
    (first..first + count)
        .map(|n| format!("s-{n:05}"))
        .collect()
}

async fn listed(script: Vec<Vec<String>>) -> Vec<String> {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let socket = spawn_daemon(&env, script);
    let mut client = connect(&socket).await;
    client
        .session_list(SessionListParams::default())
        .await
        .expect("list the sessions")
        .into_iter()
        .map(|session| session.id.0)
        .collect()
}

fn page() -> usize {
    CLIENT_PAGE_SIZE
}

#[tokio::test]
async fn a_set_that_changes_between_requests_is_listed_once_from_its_current_state() {
    let first = ids(0, page());
    // Two sessions removed from the front and one created at the end.
    let mut second = ids(2, page() - 2);
    second.push(format!("s-{:05}", page()));

    let listed = listed(vec![first, second.clone()]).await;

    assert_eq!(listed, second);
}

#[tokio::test]
async fn a_set_that_only_lost_sessions_between_requests_lists_without_error() {
    let first = ids(0, page());
    let second = ids(0, page() - 2);

    let listed = listed(vec![first, second.clone()]).await;

    assert_eq!(listed, second);
}

#[tokio::test]
async fn an_unchanged_set_of_exactly_one_page_is_listed_once() {
    let set = ids(0, page());

    let listed = listed(vec![set.clone(), set.clone()]).await;

    assert_eq!(listed, set);
}
