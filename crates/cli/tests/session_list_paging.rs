//! `session.list` against a session set larger than one response.
//!
//! A real daemon is served from this test process. Sessions are started through
//! the public `session.new` method, with metadata that makes each record large
//! where a scenario needs a set beyond one control line. The set is listed
//! through the `pohunek` binary and through raw `session.list` requests, so each
//! outcome is what an operator or a client observes.

// Rust guideline compliant 2026-11-26

#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;

use pohunek_client::Client;
use pohunek_test_support::tempdir;
use protocol::{
    method, Request, Response, SessionNewParams, SessionSecretEnv, MAX_CONTROL_LINE_BYTES,
};
use serde_json::{json, Value};

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;

/// Metadata keys of each large session and the bytes of each value.
/// Four values of 4000 bytes fill the 16 KiB metadata budget of a session.
const METADATA_KEYS: usize = 4;
const METADATA_VALUE_BYTES: usize = 4_000;

/// Large sessions: with records of the largest size the metadata allows, few
/// sessions need more than one control line together.
const LARGE_SESSIONS: usize = 70;

const _: () =
    assert!(LARGE_SESSIONS * METADATA_KEYS * METADATA_VALUE_BYTES > MAX_CONTROL_LINE_BYTES);

/// Sessions of the small set that is read page by page.
const SMALL_SESSIONS: usize = 25;

/// Records of one page in the paging scenario.
const PAGE: usize = 10;

/// Starts `count` shell sessions whose metadata has `keys` values of
/// `value_bytes` bytes and returns their ids in ascending order.
async fn start_sessions(
    harness: &Harness,
    count: usize,
    keys: usize,
    value_bytes: usize,
) -> Vec<String> {
    let cwd = tempdir().expect("create the session working directory");
    let mut client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");
    let mut ids = Vec::new();
    for index in 0..count {
        let metadata: BTreeMap<String, String> = (0..keys)
            .map(|key| (format!("k{key}"), "m".repeat(value_bytes)))
            .collect();
        let created = client
            .call::<method::SessionNew>(SessionNewParams {
                client_request_id: None,
                agent: "shell".to_owned(),
                name: Some(format!("paging-{index}")),
                cwd: Some(cwd.path().to_path_buf()),
                cols: 80,
                rows: 24,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                extended_input_ready_wait: None,
                branch_source: None,
                on_diverged: None,
                input_ack_timeout_ms: None,
                metadata,
                secret_env: SessionSecretEnv::default(),
                context_package_digest: None,
                fetch_ref: None,
                agent_options: BTreeMap::new(),
                skip_repo_hooks: false,
            })
            .await
            .expect("start a session");
        ids.push(created.session.id.0);
    }
    ids.sort();
    ids
}

/// Sends one `session.list` request with `params` as raw JSON and returns the
/// response. The reader accepts a line of any length, so a daemon that ignores
/// the paging parameters is observed rather than refused as a framing error.
fn raw_list(harness: &Harness, params: Value) -> Response {
    let mut stream = UnixStream::connect(harness.socket()).expect("connect to the daemon");
    let request =
        Request::new("paging-scenario", method::SESSION_LIST, params).expect("valid request");
    let mut line = serde_json::to_string(&request).expect("serialize request");
    line.push('\n');
    stream.write_all(line.as_bytes()).expect("send the request");
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .expect("read the response line");
    serde_json::from_str(&reply).expect("parse the response")
}

/// The ids of a page, from a successful `session.list` response.
fn page_ids(response: Response) -> Vec<String> {
    let result = response.into_result().expect("a session.list result");
    result
        .as_array()
        .expect("session records")
        .iter()
        .map(|session| session["id"].as_str().expect("session id").to_owned())
        .collect()
}

#[tokio::test]
async fn a_session_set_larger_than_one_response_is_listed_completely_by_the_cli() {
    let harness = Harness::start().await;
    let ids = start_sessions(
        &harness,
        LARGE_SESSIONS,
        METADATA_KEYS,
        METADATA_VALUE_BYTES,
    )
    .await;

    let (code, listed) = harness.json(&["session", "list"]).await;

    assert_eq!(code, 0, "{listed}");
    let listed_ids: Vec<String> = listed["ok"]
        .as_array()
        .expect("session records")
        .iter()
        .map(|session| session["id"].as_str().expect("session id").to_owned())
        .collect();
    assert_eq!(listed_ids, ids);

    // The same set asked for in one request is a typed error, not a frame the
    // client refuses.
    let response = raw_list(&harness, json!({ "limit": LARGE_SESSIONS }));

    let error = response
        .into_result()
        .expect_err("the whole set does not fit one response");
    assert_eq!(error.code, "session_list_too_large", "{error:?}");
    assert!(
        error
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("cursor")),
        "{error:?}"
    );

    harness.stop().await;
}

#[tokio::test]
async fn limit_and_cursor_walk_the_set_in_id_order() {
    let harness = Harness::start().await;
    let ids = start_sessions(&harness, SMALL_SESSIONS, 0, 0).await;

    let first = page_ids(raw_list(&harness, json!({ "limit": PAGE })));
    assert_eq!(first, ids[..PAGE]);
    let second = page_ids(raw_list(
        &harness,
        json!({ "limit": PAGE, "cursor": first[PAGE - 1] }),
    ));
    assert_eq!(second, ids[PAGE..2 * PAGE]);
    let third = page_ids(raw_list(
        &harness,
        json!({ "limit": PAGE, "cursor": second[PAGE - 1] }),
    ));
    assert_eq!(third, ids[2 * PAGE..]);

    harness.stop().await;
}

#[tokio::test]
async fn a_zero_limit_is_refused() {
    let harness = Harness::start().await;

    let error = raw_list(&harness, json!({ "limit": 0 }))
        .into_result()
        .expect_err("a zero limit is refused");

    assert_eq!(error.code, "bad_request", "{error:?}");

    harness.stop().await;
}
