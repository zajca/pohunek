//! `pohunek notifications list` against a notification history larger than one
//! response.
//!
//! A `ControlServer` with a durable notification inbox is served from this test
//! process, as the daemon binary wires it. Records are created through the
//! public `notification.create` method and listed through the `pohunek` binary,
//! so each outcome is what an operator observes.

// Rust guideline compliant 2026-10-14

#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};

use pohunek_client::Client;
use pohunek_daemon::notifications::MAX_BODY_CHARS;
use pohunek_test_support::env::TestEnv;
use protocol::{
    NotificationCreateParams, NotificationId, NotificationKind, NotificationRecord,
    NotificationSeverity, NotificationSource, NotificationStatus, Request, Response,
    MAX_CONTROL_LINE_BYTES, PROTOCOL_VERSION,
};
use serde_json::json;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

#[cfg(target_os = "linux")]
#[path = "support/connect_redirect.rs"]
mod connect_redirect;

use plugin_harness::Harness;

/// Records in the history: with bodies of [`MAX_BODY_CHARS`] they need more
/// than one control line, while a page of the CLI's size still fits one.
const RECORDS: usize = 300;

const _: () = assert!(RECORDS * MAX_BODY_CHARS > MAX_CONTROL_LINE_BYTES);

/// A body character that takes two bytes in UTF-8, which JSON carries as is.
const TWO_BYTE_CHAR: char = '\u{e9}';

/// Creates [`RECORDS`] distinct records whose bodies repeat `body_char`
/// [`MAX_BODY_CHARS`] times, through the public `notification.create` method.
async fn create_history(harness: &Harness, body_char: char) {
    let mut client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");
    for index in 0..RECORDS {
        let created = client
            .create_notification(NotificationCreateParams {
                source: NotificationSource {
                    provider: "paging-scenario".to_owned(),
                    provider_event: "scenario".to_owned(),
                    host_local_source_id: format!("paging-{index}"),
                },
                kind: NotificationKind::Error,
                severity: NotificationSeverity::ActionRequired,
                title: format!("record {index}"),
                body: body_char.to_string().repeat(MAX_BODY_CHARS),
                metadata: BTreeMap::new(),
                session_id: None,
                agent_kind: None,
                source_id: None,
                dedupe_key: None,
                project_id: None,
                retryable_admission: false,
            })
            .await
            .expect("create a notification");
        assert!(created.created, "record {index} is new");
    }
}

#[tokio::test]
async fn a_history_larger_than_one_response_is_listed_completely() {
    let harness = Harness::start_with_debounced_notifications().await;
    create_history(&harness, 'x').await;

    let (code, listed) = harness.json(&["notifications", "list"]).await;

    assert_eq!(code, 0, "{listed}");
    let records = listed["ok"]["notifications"]
        .as_array()
        .expect("notification records");
    assert_eq!(records.len(), RECORDS);
    assert!(
        listed["ok"].get("next_cursor").is_none(),
        "the whole history is listed: {}",
        listed["ok"]["next_cursor"]
    );
}

#[tokio::test]
async fn records_too_large_for_a_full_page_are_listed_in_smaller_pages() {
    // Two-byte bodies make a page of the CLI's size larger than one control
    // line, so the CLI has to ask again for fewer records per page.
    let harness = Harness::start_with_debounced_notifications().await;
    create_history(&harness, TWO_BYTE_CHAR).await;

    let (code, listed) = harness.json(&["notifications", "list"]).await;

    assert_eq!(code, 0, "{listed}");
    let records = listed["ok"]["notifications"]
        .as_array()
        .expect("notification records");
    assert_eq!(records.len(), RECORDS);
}

#[tokio::test]
async fn a_limit_whose_records_exceed_one_response_is_a_typed_error() {
    let harness = Harness::start_with_debounced_notifications().await;
    create_history(&harness, 'x').await;

    let limit = RECORDS.to_string();
    let (code, listed) = harness
        .json(&["notifications", "list", "--limit", &limit])
        .await;

    assert_ne!(code, 0, "{listed}");
    assert_eq!(
        listed["err"]["code"], "notification_list_too_large",
        "{listed}"
    );
    assert!(
        listed["err"]["recover"]
            .as_str()
            .is_some_and(|hint| hint.contains("next_cursor")),
        "{listed}"
    );
}

/// Serves a fake daemon on each of `connections`, a reader and writer pair,
/// until a connection closes without a request. `list` answers each
/// `notification.list` request from the index of its connection and its
/// params, with `None` for a result too long for one control line, which is
/// sent as such an oversized line. Other methods get a health result. Returns
/// the method of every request and the `limit` of every list request.
fn serve_fake_daemon<R, W>(
    connections: impl Iterator<Item = (R, W)> + Send + 'static,
    mut list: impl FnMut(usize, &serde_json::Value) -> Option<serde_json::Value> + Send + 'static,
) -> std::thread::JoinHandle<(Vec<String>, Vec<u64>)>
where
    R: std::io::Read,
    W: std::io::Write,
{
    std::thread::spawn(move || {
        let mut methods = Vec::new();
        let mut limits = Vec::new();
        for (index, (reader, mut writer)) in connections.enumerate() {
            let mut served = false;
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                served = true;
                let request: Request = serde_json::from_str(&line).expect("one request line");
                methods.push(request.method().to_owned());
                let result = if request.method() == protocol::method::NOTIFICATION_LIST {
                    let limit = request.params()["limit"].as_u64().expect("a paged request");
                    limits.push(limit);
                    let Some(result) = list(index, request.params()) else {
                        let oversized = "x".repeat(MAX_CONTROL_LINE_BYTES + 1);
                        let _ = writeln!(writer, "{oversized}");
                        break;
                    };
                    result
                } else {
                    json!({
                        "status": "ok",
                        "daemon_version": "0.34.2",
                        "protocol_version": PROTOCOL_VERSION.get(),
                    })
                };
                let response = Response::ok(PROTOCOL_VERSION, request.id(), result)
                    .expect("a response envelope");
                writeln!(
                    writer,
                    "{}",
                    serde_json::to_string(&response).expect("response JSON")
                )
                .expect("write the response");
            }
            if !served {
                break;
            }
        }
        (methods, limits)
    })
}

/// Answers `notification.list` the way a daemon without the response bound
/// does: a page of more than half the CLI's page size does not fit one control
/// line, and a smaller page is an empty result.
fn unbounded_page(_connection: usize, params: &serde_json::Value) -> Option<serde_json::Value> {
    let limit = params["limit"].as_u64().expect("a paged request");
    (limit <= u64::from(LIST_PAGE_SIZE / 2)).then(|| json!({ "notifications": [] }))
}

/// A notification record with `id`, as a daemon lists it.
fn record(id: &str, created_at: &str) -> serde_json::Value {
    serde_json::to_value(NotificationRecord {
        id: NotificationId(id.to_owned()),
        source: NotificationSource {
            provider: "paging-scenario".to_owned(),
            provider_event: "scenario".to_owned(),
            host_local_source_id: id.to_owned(),
        },
        kind: NotificationKind::Error,
        severity: NotificationSeverity::ActionRequired,
        status: NotificationStatus::Unread,
        title: id.to_owned(),
        body: String::new(),
        metadata: BTreeMap::new(),
        created_at: created_at.to_owned(),
        session_id: None,
        agent_kind: None,
        source_id: None,
        dedupe_key: None,
        project_id: None,
        read_at: None,
        acked_at: None,
        archived_at: None,
        deleted_at: None,
        superseded_by: None,
        turn_started_at: None,
        turn_duration_ms: None,
        session_metadata: BTreeMap::new(),
        hook_admitted: false,
    })
    .expect("record JSON")
}

/// Accepts connections on a Unix listener as reader and writer pairs.
fn unix_connections(listener: UnixListener) -> impl Iterator<Item = (UnixStream, UnixStream)> {
    std::iter::repeat_with(move || {
        let (connection, _) = listener.accept().expect("accept a CLI connection");
        (
            connection.try_clone().expect("clone the connection"),
            connection,
        )
    })
}

/// Binds the daemon socket the CLI in `env` connects to.
fn bind_local_daemon(env: &TestEnv) -> (UnixListener, std::path::PathBuf) {
    let runtime = env.runtime_dir().join("pohunek");
    std::fs::create_dir(&runtime).expect("create daemon socket directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("make socket directory private");
    let socket = runtime.join("daemon.sock");
    (
        UnixListener::bind(&socket).expect("bind daemon socket"),
        socket,
    )
}

/// The page size `pohunek notifications list` requests first (`docs/public-api.md`).
const LIST_PAGE_SIZE: u32 = 200;

#[test]
fn a_page_an_earlier_daemon_cannot_frame_is_asked_again_at_half_the_size() {
    let env = TestEnv::new().expect("private CLI environment");
    let (listener, socket) = bind_local_daemon(&env);
    let server = serve_fake_daemon(unix_connections(listener), unbounded_page);

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["notifications", "list", "--json"])
        .output()
        .expect("run CLI");
    // A connection without a request ends the fixture daemon.
    drop(UnixStream::connect(&socket).expect("end the fixture daemon"));
    let (methods, limits) = server.join().expect("the fixture daemon ends");

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let listed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(listed["ok"]["notifications"], json!([]), "{listed}");
    assert_eq!(
        limits,
        [u64::from(LIST_PAGE_SIZE), u64::from(LIST_PAGE_SIZE / 2)],
        "{methods:?}"
    );
}

/// The `NetBird` address the fake `netbird` reports for the remote host; the
/// connect interposer sends it to the loopback fixture daemon.
#[cfg(target_os = "linux")]
const REMOTE_NETBIRD_IP: &str = "100.64.0.42";

#[test]
#[cfg(target_os = "linux")]
fn a_page_an_earlier_remote_daemon_cannot_frame_is_asked_again_at_half_the_size() {
    let env = TestEnv::new().expect("private CLI environment");
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind TCP fixture daemon");
    let address = listener.local_addr().expect("TCP fixture address");
    let server = serve_fake_daemon(
        std::iter::repeat_with(move || {
            let (connection, _) = listener.accept().expect("accept a CLI connection");
            (
                connection.try_clone().expect("clone the connection"),
                connection,
            )
        }),
        unbounded_page,
    );
    let (connect_redirect, connect_capture) = connect_redirect::build(env.home());
    let bin = env.home().join("netbird-bin");
    std::fs::create_dir(&bin).expect("create fake netbird directory");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{{\"peers\":{{\"details\":[{{\"fqdn\":\"fixture-remote.netbird.test\",\"netbirdIp\":\"{REMOTE_NETBIRD_IP}\",\"status\":\"Connected\"}}]}}}}'\n"
        ),
    )
    .expect("write fake netbird");

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &connect_redirect)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &connect_capture)
        .env("POHUNEK_REMOTE_PORT", address.port().to_string())
        .args([
            "--host",
            "fixture-remote",
            "notifications",
            "list",
            "--json",
        ])
        .output()
        .expect("run CLI");
    // A connection without a request ends the fixture daemon.
    drop(std::net::TcpStream::connect(address).expect("end the fixture daemon"));
    let (methods, limits) = server.join().expect("the fixture daemon ends");

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let listed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(listed["ok"]["notifications"], json!([]), "{listed}");
    assert_eq!(
        limits,
        [u64::from(LIST_PAGE_SIZE), u64::from(LIST_PAGE_SIZE / 2)],
        "{methods:?}"
    );
    let captured = std::fs::read_to_string(&connect_capture).expect("read connect targets");
    let remote = format!("{REMOTE_NETBIRD_IP}:{}", address.port());
    assert!(
        !captured.is_empty() && captured.lines().all(|line| line == remote),
        "every connection goes to the remote host: {captured}"
    );
}

#[test]
fn a_listing_asked_again_on_a_new_connection_starts_from_the_first_page() {
    // The first connection reaches a daemon that cannot frame its second page;
    // the next connection reaches another daemon, as a remote host name
    // resolved again may. A cursor of the first daemon must not page the second.
    let env = TestEnv::new().expect("private CLI environment");
    let (listener, socket) = bind_local_daemon(&env);
    let server = serve_fake_daemon(unix_connections(listener), |connection, params| {
        let first_page = params.get("cursor").is_none_or(serde_json::Value::is_null);
        match (connection, first_page) {
            (0, true) => Some(json!({
                "notifications": [record("first-daemon", "2026-10-14T10:00:00Z")],
                "next_cursor": "first-daemon-cursor",
            })),
            (0, false) => None,
            (_, true) => Some(json!({
                "notifications": [record("second-daemon-newer", "2026-10-14T09:00:00Z")],
                "next_cursor": "second-daemon-cursor",
            })),
            (_, false) => Some(json!({
                "notifications": [record("second-daemon-older", "2026-10-14T08:00:00Z")],
            })),
        }
    });

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args(["notifications", "list", "--json"])
        .output()
        .expect("run CLI");
    // A connection without a request ends the fixture daemon.
    drop(UnixStream::connect(&socket).expect("end the fixture daemon"));
    let (methods, _limits) = server.join().expect("the fixture daemon ends");

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let listed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON document");
    let mut ids: Vec<_> = listed["ok"]["notifications"]
        .as_array()
        .expect("notification records")
        .iter()
        .map(|record| record["id"].as_str().expect("record id").to_owned())
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        ["second-daemon-newer", "second-daemon-older"],
        "{listed} {methods:?}"
    );
}
