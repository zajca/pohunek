//! `Client::subscribe_with_options` and the typed `subscription_lagged`
//! accessor against a fixture daemon.
//!
//! The fixture daemon records every request it serves and streams fixed event
//! lines after a subscribe. The scenarios pin that a client sends
//! `heartbeat_ms` only to a daemon that advertises the capability, that a
//! subscriber without options sends the unchanged `null` params, and that a
//! lag notice decodes from the stream, also for a channel this build does not
//! know.

// Rust guideline compliant 2026-11-26

#![cfg(unix)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pohunek_client::protocol::{
    self, ErrorClass, Request, Response, SubscriptionChannel, PROTOCOL_VERSION,
};
use pohunek_client::{Client, ClientError, ClientOptions, OriginSource, SubscribeOptions};
use pohunek_test_support::env::TestEnv;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// Interval the scenarios ask for.
const HEARTBEAT: Duration = Duration::from_millis(1_500);

/// Bounds the capable fixture daemon advertises.
const MIN_MS: u32 = 1_000;
const MAX_MS: u32 = 60_000;

/// Requests the fixture daemon served: method and params, in arrival order.
type Served = Arc<Mutex<Vec<(String, Value)>>>;

fn no_origin_options() -> ClientOptions {
    ClientOptions::default().with_origin_source(OriginSource::Omitted)
}

fn capabilities(heartbeat: bool) -> Value {
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
    if heartbeat {
        value["subscribe_heartbeat"] = json!({ "min_ms": MIN_MS, "max_ms": MAX_MS });
    }
    value
}

/// The event lines the fixture daemon streams after a subscribe.
fn stream_lines() -> Vec<Value> {
    let v = PROTOCOL_VERSION.get();
    vec![
        json!({"v": v, "event": "subscription_lagged", "channel": "session", "skipped": 7}),
        json!({"v": v, "event": "subscription_lagged", "channel": "from_the_future", "skipped": 3}),
        json!({"v": v, "event": "heartbeat"}),
    ]
}

/// Starts a fixture daemon on a fresh socket and returns the requests it serves.
fn spawn_daemon(env: &TestEnv, heartbeat_supported: bool) -> (std::path::PathBuf, Served) {
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
                        protocol::method::HOST_INSPECT => capabilities(heartbeat_supported),
                        protocol::method::SUBSCRIBE => json!({ "subscribed": true }),
                        other => panic!("the client sent an unexpected request: {other}"),
                    };
                    let response = Response::ok(PROTOCOL_VERSION, request.id(), result)
                        .expect("valid fixture response");
                    let mut encoded = serde_json::to_vec(&response).expect("serialize response");
                    encoded.push(b'\n');
                    if write.write_all(&encoded).await.is_err() {
                        return;
                    }
                    if request.method() == protocol::method::SUBSCRIBE {
                        for event in stream_lines() {
                            let mut line = serde_json::to_vec(&event).expect("serialize event");
                            line.push(b'\n');
                            if write.write_all(&line).await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
                }
            });
        }
    });
    (socket, served)
}

async fn connect(socket: &Path) -> Client {
    let mut client = Client::connect_local_with_options(socket, no_origin_options())
        .await
        .expect("connect fixture daemon");
    client.handshake().await.expect("negotiate protocol");
    client
}

fn served_methods(served: &Served) -> Vec<String> {
    served
        .lock()
        .expect("served requests")
        .iter()
        .map(|(method, _params)| method.clone())
        .collect()
}

fn subscribe_params(served: &Served) -> Value {
    served
        .lock()
        .expect("served requests")
        .iter()
        .find(|(method, _params)| method == protocol::method::SUBSCRIBE)
        .map(|(_method, params)| params.clone())
        .expect("the client sent a subscribe request")
}

#[tokio::test]
async fn a_heartbeat_is_requested_only_from_a_daemon_that_advertises_it() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, false);

    let refused = connect(&socket)
        .await
        .subscribe_with_options(SubscribeOptions::with_heartbeat(HEARTBEAT))
        .await
        .expect_err("a daemon without the capability is not asked for a heartbeat");

    let ClientError::Protocol(error) = refused else {
        panic!("expected a protocol error, got {refused:?}");
    };
    assert_eq!(error.class, ErrorClass::Daemon);
    assert_eq!(error.code, "subscribe_heartbeat_unsupported");
    assert!(
        !served_methods(&served).contains(&protocol::method::SUBSCRIBE.to_owned()),
        "nothing was subscribed"
    );
}

#[tokio::test]
async fn a_capable_daemon_receives_the_interval_and_streams_typed_lag_notices() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, true);

    let mut subscription = connect(&socket)
        .await
        .subscribe_with_options(SubscribeOptions::with_heartbeat(HEARTBEAT))
        .await
        .expect("a capable daemon accepts the heartbeat");
    assert_eq!(
        subscribe_params(&served),
        json!({ "heartbeat_ms": HEARTBEAT.as_millis() })
    );

    let lagged = subscription
        .next_event()
        .await
        .expect("read the first event")
        .expect("the stream has events")
        .subscription_lagged()
        .expect("the first event is a lag notice");
    assert_eq!(lagged.channel, SubscriptionChannel::Session);
    assert_eq!(lagged.skipped, 7);

    let unknown = subscription
        .next_event()
        .await
        .expect("read the second event")
        .expect("the stream has more events")
        .subscription_lagged()
        .expect("a notice for an unknown channel still decodes");
    assert_eq!(unknown.channel, SubscriptionChannel::Unknown);
    assert_eq!(unknown.skipped, 3);

    let heartbeat = subscription
        .next_event()
        .await
        .expect("read the third event")
        .expect("the stream has a heartbeat");
    assert_eq!(heartbeat.event(), protocol::event::HEARTBEAT);
    assert!(heartbeat.subscription_lagged().is_none());
}

#[tokio::test]
async fn a_subscriber_without_options_sends_null_params_and_no_inspect() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let (socket, served) = spawn_daemon(&env, true);

    let _subscription = connect(&socket)
        .await
        .subscribe_with_options(SubscribeOptions::default())
        .await
        .expect("subscribe without options");

    assert_eq!(subscribe_params(&served), Value::Null);
    assert!(
        !served_methods(&served).contains(&protocol::method::HOST_INSPECT.to_owned()),
        "no capability probe without an opt-in"
    );
}
