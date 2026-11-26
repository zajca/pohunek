//! `Client::project_list` against a daemon that predates paging.
//!
//! Such a daemon ignores `limit` and `cursor` and answers the whole current set
//! to every `project.list` request. The fixture daemon plays one scripted set
//! per request, so the set can change between the walk's requests. The client
//! must return the current set once, without duplicates.

#![cfg(unix)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pohunek_client::protocol::{self, ProjectListParams, Request, Response, PROTOCOL_VERSION};
use pohunek_client::{Client, ClientOptions, OriginSource};
use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// Projects the client asks for per page, and so the size of a set that a
/// daemon ignoring `limit` makes look like one full page.
const CLIENT_PAGE_SIZE: usize = 200;

/// The sets the fixture daemon answers, one per `project.list` request.
type Script = Arc<Mutex<VecDeque<Vec<String>>>>;

/// A project record; `entry` is an id, or `id@label` for a project whose label
/// differs from its id.
fn project(entry: &str) -> Value {
    let (id, label) = entry.split_once('@').unwrap_or((entry, entry));
    json!({
        "id": id,
        "label": label,
        "repo_root": format!("/repos/{id}"),
        "git_common_dir": format!("/repos/{id}/.git"),
        "source": "manual",
        "is_bare": false,
        "added_at": "2026-11-26T00:00:00Z",
        "last_used_at": "2026-11-26T00:00:00Z"
    })
}

/// Serves `script` to `project.list` requests; a request after the script ends
/// repeats its last set.
fn spawn_daemon(env: &TestEnv, script: Vec<Vec<String>>) -> std::path::PathBuf {
    let socket = env.socket_path("old-projects").expect("fixture socket");
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
                        protocol::method::PROJECT_LIST => {
                            let mut script = script.lock().expect("script");
                            let ids = if script.len() > 1 {
                                script.pop_front().expect("a scripted set")
                            } else {
                                script.front().cloned().expect("a scripted set")
                            };
                            Value::Array(ids.iter().map(|id| project(id)).collect())
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
        .map(|n| format!("p-{n:05}"))
        .collect()
}

#[tokio::test]
async fn a_set_that_changes_between_requests_is_listed_once_from_its_current_state() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let first = ids(0, CLIENT_PAGE_SIZE);
    // Two projects removed from the front and one registered at the end.
    let mut second = ids(2, CLIENT_PAGE_SIZE - 2);
    second.push(format!("p-{CLIENT_PAGE_SIZE:05}"));
    let socket = spawn_daemon(&env, vec![first, second.clone()]);
    let mut client = connect(&socket).await;

    let listed: Vec<String> = client
        .project_list(ProjectListParams::default())
        .await
        .expect("list the projects")
        .into_iter()
        .map(|project| project.id)
        .collect();

    assert_eq!(listed, second);
}

#[tokio::test]
async fn a_project_renamed_past_the_cursor_between_pages_is_listed_once() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    // The second page answers a project the first page already listed, now
    // under a label that sorts after the cursor, as a paging daemon does when
    // a project is renamed between two requests.
    let first = ids(0, CLIENT_PAGE_SIZE);
    let renamed = vec!["p-00005@zz".to_owned()];
    let socket = spawn_daemon(&env, vec![first.clone(), renamed]);
    let mut client = connect(&socket).await;

    let listed: Vec<String> = client
        .project_list(ProjectListParams::default())
        .await
        .expect("list the projects")
        .into_iter()
        .map(|project| project.id)
        .collect();

    assert_eq!(listed, first);
}
