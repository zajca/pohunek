//! A client whose request timed out, or whose reply did not match, opens a
//! fresh connection on its next call.
//!
//! The fixture daemon is driven line by line on a paused clock: the test
//! decides which connection a reply is written to and when, so a stale reply
//! can be staged precisely and no scenario waits for real time.

use std::path::Path;
use std::time::Duration;

use pohunek_client::{Client, ClientError, ClientOptions, OriginSource};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::time::{AutoAdvanceInhibitor, TIMER_TICK};
use protocol::{Request, Response};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Response timeout of the scenarios; the clock is paused, so it is virtual.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(20);

/// Package digest whose request depends on the protocol version, so a fresh
/// connection probes the daemon before sending it.
const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

struct Fixture {
    listener: UnixListener,
    path: std::path::PathBuf,
    _env: TestEnv,
}

impl Fixture {
    fn start() -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let path = env
            .socket_path("daemon.sock")
            .expect("socket path fits the sun_path limit");
        let listener = UnixListener::bind(&path).expect("bind the fixture daemon");
        Self {
            listener,
            path,
            _env: env,
        }
    }

    async fn connect(&self) -> Client {
        let options = ClientOptions::default()
            .with_origin_source(OriginSource::Omitted)
            .with_request_timeout(REQUEST_TIMEOUT);
        Client::connect_local_with_options(&self.path, options)
            .await
            .expect("connect to the fixture daemon")
    }

    async fn accept(&self) -> Connection {
        let (stream, _) = self.listener.accept().await.expect("accept a connection");
        Connection {
            stream: BufReader::new(stream),
        }
    }
}

struct Connection {
    stream: BufReader<UnixStream>,
}

impl Connection {
    async fn read_request(&mut self) -> Request {
        let mut line = String::new();
        let read = self
            .stream
            .read_line(&mut line)
            .await
            .expect("read a request line");
        assert!(
            read > 0,
            "the client closed the connection before a request"
        );
        serde_json::from_str(line.trim_end()).expect("parse a request line")
    }

    /// Whether the client closed this connection without sending anything.
    async fn is_closed_by_client(&mut self) -> bool {
        let mut line = String::new();
        self.stream
            .read_line(&mut line)
            .await
            .expect("read the closed connection")
            == 0
    }

    async fn reply(&mut self, request: &Request, ok: Value) {
        self.reply_with_id(request.id(), ok).await;
    }

    async fn reply_with_id(&mut self, id: &str, ok: Value) {
        let response =
            Response::ok(protocol::PROTOCOL_VERSION, id, ok).expect("valid fixture response");
        let mut encoded = serde_json::to_vec(&response).expect("encode the response");
        encoded.push(b'\n');
        // The client may have dropped the connection already.
        let _ = self.stream.get_mut().write_all(&encoded).await;
    }
}

fn health(id: &str) -> Request {
    Request::new(id, protocol::method::DAEMON_HEALTH, Value::Null).expect("valid health request")
}

/// Runs `request` on a task that hands the client back, so the test can move
/// the paused clock while the call waits.
fn spawn_request(
    mut client: Client,
    request: Request,
) -> tokio::task::JoinHandle<(Client, Result<Value, ClientError>)> {
    tokio::spawn(async move {
        let result = client.request(&request).await;
        (client, result)
    })
}

async fn time_out(call: tokio::task::JoinHandle<(Client, Result<Value, ClientError>)>) -> Client {
    tokio::time::advance(REQUEST_TIMEOUT + TIMER_TICK).await;
    let (client, result) = call.await.expect("the call task joins");
    assert!(
        matches!(result, Err(ClientError::RequestTimeout { .. })),
        "{result:?}"
    );
    client
}

fn assert_socket_gone(path: &Path) {
    assert!(!path.exists(), "the fixture socket was removed");
}

#[tokio::test(start_paused = true)]
async fn the_call_after_a_timeout_succeeds_with_its_own_reply() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let fixture = Fixture::start();
    let client = fixture.connect().await;
    let mut first_connection = fixture.accept().await;
    let first = health("req-1");
    let call = spawn_request(client, first.clone());
    assert_eq!(first_connection.read_request().await.id(), first.id());

    let mut client = time_out(call).await;
    let second = health("req-2");
    let call = tokio::spawn(async move {
        let result = client.request(&second).await;
        (client, result)
    });
    let mut second_connection = fixture.accept().await;
    let seen = second_connection.read_request().await;
    // The timed-out request is not sent again; the connection carries only
    // the new call.
    assert_eq!(seen.id(), "req-2");
    second_connection.reply(&seen, json!({"call": 2})).await;

    let (_client, result) = call.await.expect("the call task joins");
    assert_eq!(
        result.expect("the second call succeeds"),
        json!({"call": 2})
    );
    inhibitor.release().await;
}

#[tokio::test(start_paused = true)]
async fn a_late_reply_to_the_timed_out_request_never_reaches_the_next_call() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let fixture = Fixture::start();
    let client = fixture.connect().await;
    let mut first_connection = fixture.accept().await;
    let first = health("req-late-1");
    let call = spawn_request(client, first.clone());
    first_connection.read_request().await;

    let mut client = time_out(call).await;
    let call = tokio::spawn(async move {
        let result = client.request(&health("req-late-2")).await;
        (client, result)
    });
    let mut second_connection = fixture.accept().await;
    let seen = second_connection.read_request().await;
    // The daemon answers the first request only now, on the old connection,
    // and the second request afterwards on its own.
    first_connection
        .reply(&first, json!({"call": "stale"}))
        .await;
    second_connection
        .reply(&seen, json!({"call": "fresh"}))
        .await;

    let (_client, result) = call.await.expect("the call task joins");
    assert_eq!(
        result.expect("the second call succeeds"),
        json!({"call": "fresh"})
    );
    assert!(
        first_connection.is_closed_by_client().await,
        "the old connection was closed"
    );
    inhibitor.release().await;
}

#[tokio::test(start_paused = true)]
async fn a_reply_with_another_id_is_followed_by_a_working_call() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let fixture = Fixture::start();
    let client = fixture.connect().await;
    let mut first_connection = fixture.accept().await;
    let call = spawn_request(client, health("req-mismatch-1"));
    first_connection.read_request().await;
    first_connection
        .reply_with_id("an-unrelated-id", json!({"call": "wrong"}))
        .await;
    let (mut client, result) = call.await.expect("the call task joins");
    assert!(matches!(result, Err(ClientError::Framing(_))), "{result:?}");

    let call = tokio::spawn(async move {
        let result = client.request(&health("req-mismatch-2")).await;
        (client, result)
    });
    let mut second_connection = fixture.accept().await;
    let seen = second_connection.read_request().await;
    second_connection.reply(&seen, json!({"call": 2})).await;
    let (_client, result) = call.await.expect("the call task joins");
    assert_eq!(result.expect("the next call succeeds"), json!({"call": 2}));
    inhibitor.release().await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_reopen_is_typed_and_the_next_call_tries_again() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let fixture = Fixture::start();
    let client = fixture.connect().await;
    let mut first_connection = fixture.accept().await;
    let call = spawn_request(client, health("req-gone-1"));
    first_connection.read_request().await;
    let mut client = time_out(call).await;

    // The daemon is gone when the next call wants a connection.
    let Fixture {
        listener,
        path,
        _env,
    } = fixture;
    drop(listener);
    std::fs::remove_file(&path).expect("remove the fixture socket");
    assert_socket_gone(&path);
    let error = client
        .request(&health("req-gone-2"))
        .await
        .expect_err("no daemon to reconnect to");
    assert!(
        matches!(error, ClientError::DaemonUnreachable { .. }),
        "{error:?}"
    );

    // The daemon is back; the same client reconnects on its next call.
    let listener = UnixListener::bind(&path).expect("bind the fixture daemon again");
    let call = tokio::spawn(async move {
        let result = client.request(&health("req-gone-3")).await;
        (client, result)
    });
    let (stream, _) = listener.accept().await.expect("accept the new connection");
    let mut connection = Connection {
        stream: BufReader::new(stream),
    };
    let seen = connection.read_request().await;
    connection.reply(&seen, json!({"call": 3})).await;
    let (_client, result) = call.await.expect("the call task joins");
    assert_eq!(result.expect("the next call succeeds"), json!({"call": 3}));
    inhibitor.release().await;
}

#[tokio::test(start_paused = true)]
async fn a_reopened_connection_learns_the_daemon_version_again() {
    let inhibitor = AutoAdvanceInhibitor::new();
    let fixture = Fixture::start();
    let client = fixture.connect().await;
    let mut first_connection = fixture.accept().await;
    let uninstall = |id: &str| {
        Request::new(
            id,
            protocol::method::PACKAGE_UNINSTALL,
            json!({"digest": DIGEST}),
        )
        .expect("valid uninstall request")
    };
    // The first connection selects the current version, then the request is
    // never answered.
    let call = spawn_request(client, uninstall("req-version-1"));
    let probe = first_connection.read_request().await;
    assert_eq!(probe.method(), protocol::method::DAEMON_HEALTH);
    first_connection
        .reply(
            &probe,
            json!({
                "status": "ok",
                "daemon_version": "test",
                "protocol_version": protocol::PROTOCOL_VERSION.get(),
            }),
        )
        .await;
    assert_eq!(
        first_connection.read_request().await.method(),
        protocol::method::PACKAGE_UNINSTALL
    );
    let mut client = time_out(call).await;

    let call = tokio::spawn(async move {
        let result = client.request(&uninstall("req-version-2")).await;
        (client, result)
    });
    let mut second_connection = fixture.accept().await;
    // Nothing selected on the old connection is inherited: the new one probes.
    let probe = second_connection.read_request().await;
    assert_eq!(probe.method(), protocol::method::DAEMON_HEALTH);
    drop(second_connection);
    let (_client, result) = call.await.expect("the call task joins");
    assert!(result.is_err(), "{result:?}");
    inhibitor.release().await;
}
