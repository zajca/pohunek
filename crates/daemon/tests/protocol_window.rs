//! Integration test: a real daemon serves the previous public protocol version.
//!
//! A worker-backed daemon is bound on a Unix socket and driven with raw JSON
//! lines of protocol 3 (release v0.33.0): hand-built lines for the shape
//! checks and the lines recorded from the v0.33.0 CLI, TypeScript SDK, Hermes
//! plugin and managed hooks (`tests/fixtures/compat/v3/consumers/`). Responses
//! and events must come back stamped and shaped as protocol 3, while a current
//! client on the same daemon keeps the current shape.

// The daemon's unit tests share this view; it keeps `session.remove`
// independent of the test host's processes whose markers cannot be read.
#[path = "../src/procwatch/readable_host.rs"]
mod readable_host;
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::notifications::{AttentionCoordinator, NotificationService};
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::SessionRegistry;
use pohunek_test_support::wait::{self, wait_until};
use pohunek_test_support::worker_binary;
use protocol::{method, Request, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

/// Protocol version of the recorded fixtures and of the hand-built lines.
const PREVIOUS_VERSION: u32 = 3;

/// Session id the recorded consumer requests address.
const FIXTURE_SESSION_ID: &str = "ses_0123456789abcdef";
/// Worker instance id the recorded consumer requests address.
const FIXTURE_WORKER_INSTANCE_ID: &str = "rt-0123456789abcdef";

/// `max_bytes` of the hand-built `session.output` requests; the parameter is required.
const OUTPUT_MAX_BYTES: u32 = 4096;

/// Error codes that mean the daemon could not read the request's shape.
const SHAPE_ERROR_CODES: &[&str] = &["bad_request", "version_mismatch", "method_not_found"];

/// Consumer recordings: (file name, contents).
const CONSUMER_FIXTURES: &[(&str, &str)] = &[
    (
        "cli.json",
        include_str!("fixtures/compat/v3/consumers/cli.json"),
    ),
    (
        "ts_sdk.json",
        include_str!("fixtures/compat/v3/consumers/ts_sdk.json"),
    ),
    (
        "hermes_plugin.json",
        include_str!("fixtures/compat/v3/consumers/hermes_plugin.json"),
    ),
    (
        "managed_hook.json",
        include_str!("fixtures/compat/v3/consumers/managed_hook.json"),
    ),
];

/// Methods whose recorded public-fallback requests carry an integer `seq`.
///
/// Release v0.33.0 itself rejects that spelling (`ReportSequence` reads a
/// decimal string), so the adapter must leave the rejection as it was.
const INTEGER_SEQ_REJECTIONS: &[&str] =
    &[method::SESSION_REPORT_AGENT, method::SESSION_RELEASE_AGENT];

/// Methods whose recorded requests must succeed against a live session.
const MUST_SUCCEED: &[&str] = &[
    method::DAEMON_HEALTH,
    method::HOST_INSPECT,
    method::SESSION_NEW,
    method::SESSION_LIST,
    method::SESSION_INSPECT,
    method::SESSION_SCREEN,
    method::SESSION_READ,
    method::SESSION_OUTPUT,
    method::SESSION_WAIT,
    method::SESSION_RESIZE,
    method::SESSION_RENAME,
    method::SESSION_SET_METADATA,
    method::SESSION_RUNTIME_INVENTORY,
    method::NOTIFICATION_LIST,
];

type Client = Framed<UnixStream, LinesCodec>;

/// A unique socket path inside a dedicated per-test directory.
struct TestSocket {
    path: PathBuf,
    dir: tempfile::TempDir,
}

impl TestSocket {
    fn new(tag: &str) -> Self {
        let dir = pohunek_test_support::tempdir_with_prefix(&format!("ph-{tag}-"))
            .expect("create test socket dir");
        std::fs::create_dir(dir.path().join("work")).expect("create the session working directory");
        Self {
            path: dir.path().join("daemon.sock"),
            dir,
        }
    }

    fn work_dir(&self) -> PathBuf {
        self.dir.path().join("work")
    }
}

impl std::ops::Deref for TestSocket {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

/// A running worker-backed daemon.
struct Daemon {
    socket: TestSocket,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn start(tag: &str) -> Self {
        let socket = TestSocket::new(tag);
        let (shutdown, handle) = spawn_server(&socket).await;
        Self {
            socket,
            shutdown,
            handle,
        }
    }

    async fn connect(&self) -> Client {
        connect(&self.socket).await
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
    }
}

/// Spawns the control server over a registry backed by a real worker binary.
async fn spawn_server(socket: &Path) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    // Short, because the worker nests its control socket below it.
    let worker_home_dir =
        pohunek_test_support::tempdir_with_prefix("pw-h-").expect("create the worker home");
    let worker_home = worker_home_dir.path().to_path_buf();
    let worker_environment = SubprocessWorkerEnvironment {
        runtime_home: worker_home.join("runtime"),
        state_home: worker_home.join("state"),
        data_home: worker_home.join("data"),
        config_home: worker_home.join("config"),
        cache_home: worker_home.join("cache"),
        home: worker_home.clone(),
        daemon_socket: socket.to_path_buf(),
    };
    let mut config = support::hermetic_registry_config();
    config.socket_path = Some(socket.to_path_buf());
    config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
    config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
    config.supervision = Some(
        worker_environment
            .supervision(worker_binary())
            .with_environment_source(support::hermetic_environment_source()),
    );
    let registry = SessionRegistry::new_with_launcher_and_inspector(
        config,
        Arc::new(SubprocessWorkerLauncher::new()),
        Arc::new(readable_host::ReadableHost::new()),
    );
    let state_root = socket
        .parent()
        .expect("test socket has an isolated parent")
        .to_path_buf();
    let governance = Arc::new(
        HostGovernanceService::open(state_root.join("state"))
            .await
            .expect("open real isolated host-governance service"),
    );
    let notifications = NotificationService::open(&state_root.join("notification-data"))
        .expect("notification service opens");
    let mut policy = pohunek_daemon::notifications::default_policy();
    policy.enabled = protocol::NotificationKindPolicy {
        agent_blocked: true,
        approval_required: true,
        turn_completed: true,
        session_finished: true,
        error: true,
        system: true,
    };
    policy.providers.clear();
    notifications
        .set_policy(policy)
        .expect("enable every notification kind");
    // Session-scoped notifications (approval, turn) are debounced by the coordinator.
    let (attention, _attention_task) = AttentionCoordinator::spawn(notifications.clone());
    let state = DaemonState::new(
        HealthInfo::new("0.0.0"),
        registry,
        governance,
        support::overlay_registry(),
    )
    .with_notifications(notifications)
    .with_attention_coordinator(attention);
    let server = ControlServer::bind_with_state(socket, state)
        .await
        .expect("server binds");
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        // Removed after the server and its registry are gone.
        let _worker_home = worker_home_dir;
        server
            .serve(async move {
                let _ = rx.await;
            })
            .await;
    });
    (tx, handle)
}

async fn connect(socket: &Path) -> Client {
    wait_until("the test socket to accept a connection", || async {
        UnixStream::connect(socket)
            .await
            .ok()
            .map(|stream| Framed::new(stream, LinesCodec::new()))
    })
    .await
}

/// Sends one raw JSON line and reads one response line as JSON.
async fn exchange(client: &mut Client, line: &Value) -> Value {
    client
        .send(serde_json::to_string(line).expect("serialize request line"))
        .await
        .expect("send request line");
    next_line(client).await
}

async fn next_line(client: &mut Client) -> Value {
    let reply = wait::guard("a response or event line", client.next())
        .await
        .expect("a line")
        .expect("line framing ok");
    serde_json::from_str(&reply).expect("the daemon writes JSON lines")
}

/// A hand-built request line of the previous protocol version.
#[expect(
    clippy::needless_pass_by_value,
    reason = "test helper takes the json! literal by value to keep call sites terse"
)]
fn previous_request(id: &str, method: &str, params: Value) -> Value {
    json!({
        "v": {"minimum": PREVIOUS_VERSION, "maximum": PREVIOUS_VERSION},
        "id": id,
        "method": method,
        "params": params,
    })
}

/// The success payload of a response stamped with the previous version.
fn previous_ok(response: &Value) -> &Value {
    assert_eq!(response["v"], PREVIOUS_VERSION, "{response}");
    assert!(response.get("err").is_none(), "{response}");
    &response["ok"]
}

fn contains_key(value: &Value, wanted: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key(wanted) || object.values().any(|inner| contains_key(inner, wanted))
        }
        Value::Array(items) => items.iter().any(|inner| contains_key(inner, wanted)),
        _ => false,
    }
}

/// Asserts that no protocol 4 spelling of the worker instance key leaks.
fn assert_previous_spelling(value: &Value) {
    for key in ["worker_instance_id", "previous_worker_instance_id"] {
        assert!(!contains_key(value, key), "`{key}` leaked into {value}");
    }
}

/// Replaces recorded placeholder values with the live session's identity.
fn substitute(value: &Value, live: &LiveSession, work_dir: &Path) -> Value {
    match value {
        Value::String(text) if text == FIXTURE_SESSION_ID => json!(live.session_id),
        Value::String(text) if text == FIXTURE_WORKER_INSTANCE_ID => json!(live.worker_instance_id),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, inner)| {
                    // The recorded launch directories are host paths; a session
                    // launched by a replayed request starts in the test's own.
                    let replaced = match key.as_str() {
                        "runtime_generation" => json!(live.runtime_generation),
                        "cwd" => json!(work_dir),
                        _ => substitute(inner, live, work_dir),
                    };
                    (key.clone(), replaced)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|inner| substitute(inner, live, work_dir))
                .collect(),
        ),
        other => other.clone(),
    }
}

struct LiveSession {
    session_id: String,
    worker_instance_id: String,
    runtime_generation: String,
}

/// Creates a shell session as a protocol 3 client and reads its identity back
/// in the protocol 3 spelling.
async fn create_previous_session(client: &mut Client, daemon: &Daemon) -> (LiveSession, Value) {
    let response = exchange(
        client,
        &previous_request(
            "new-1",
            method::SESSION_NEW,
            json!({"agent": "shell", "cwd": daemon.socket.work_dir(), "cols": 80, "rows": 24}),
        ),
    )
    .await;
    let session = previous_ok(&response).clone();
    assert_previous_spelling(&session);
    let runtime = &session["runtime"];
    let live = LiveSession {
        session_id: session["id"].as_str().expect("session id").to_owned(),
        worker_instance_id: runtime["runtime_id"]
            .as_str()
            .expect("a protocol 3 session carries `runtime.runtime_id`")
            .to_owned(),
        runtime_generation: runtime["runtime_generation"]
            .as_str()
            .expect("runtime generation")
            .to_owned(),
    };
    (live, session)
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one linear scenario shares a single worker-backed session"
)]
async fn previous_version_client_drives_a_worker_backed_session_in_the_previous_shape() {
    let daemon = Daemon::start("window-shape").await;
    let mut client = daemon.connect().await;

    let health = exchange(
        &mut client,
        &previous_request("health-1", method::DAEMON_HEALTH, Value::Null),
    )
    .await;
    assert_eq!(
        previous_ok(&health)["protocol_version"],
        PREVIOUS_VERSION,
        "the handshake of a protocol 3 SDK compares this field with its own version"
    );

    let (live, created) = create_previous_session(&mut client, &daemon).await;
    assert_eq!(created["agent"], "shell");

    let listed = exchange(
        &mut client,
        &previous_request("list-1", method::SESSION_LIST, json!({})),
    )
    .await;
    let listed = previous_ok(&listed);
    assert_previous_spelling(listed);
    let entry = listed
        .as_array()
        .expect("session list")
        .iter()
        .find(|session| session["id"] == live.session_id)
        .expect("the created session is listed");
    assert_eq!(entry["runtime"]["runtime_id"], live.worker_instance_id);

    let inspected = exchange(
        &mut client,
        &previous_request("inspect-1", method::SESSION_INSPECT, json!(live.session_id)),
    )
    .await;
    assert_eq!(
        previous_ok(&inspected)["runtime"]["runtime_id"],
        live.worker_instance_id
    );

    for (id, name, params) in [
        (
            "screen-1",
            method::SESSION_SCREEN,
            json!({"session_id": live.session_id}),
        ),
        (
            "read-1",
            method::SESSION_READ,
            json!({"session_id": live.session_id}),
        ),
        (
            "output-1",
            method::SESSION_OUTPUT,
            json!({"session_id": live.session_id, "max_bytes": OUTPUT_MAX_BYTES}),
        ),
    ] {
        let response = exchange(&mut client, &previous_request(id, name, params)).await;
        let result = previous_ok(&response);
        assert_eq!(result["runtime_id"], live.worker_instance_id, "{name}");
        assert_eq!(
            result["runtime_generation"], live.runtime_generation,
            "{name}"
        );
        assert_previous_spelling(result);
    }

    let continued = exchange(
        &mut client,
        &previous_request(
            "output-2",
            method::SESSION_OUTPUT,
            json!({
                "session_id": live.session_id,
                "runtime": {
                    "runtime_id": live.worker_instance_id,
                    "runtime_generation": live.runtime_generation,
                },
                "after_offset": "0",
                "max_bytes": OUTPUT_MAX_BYTES,
            }),
        ),
    )
    .await;
    assert_eq!(
        previous_ok(&continued)["runtime_id"],
        live.worker_instance_id,
        "a protocol 3 runtime cursor is accepted and answered in the same spelling"
    );

    let replaced = exchange(
        &mut client,
        &previous_request(
            "wait-1",
            method::SESSION_WAIT,
            json!({
                "session_id": live.session_id,
                "runtime": {"runtime_id": "another-worker-instance", "runtime_generation": "1"},
                "timeout_ms": 1000,
            }),
        ),
    )
    .await;
    let waited = previous_ok(&replaced);
    assert_eq!(waited["reason"], "runtime_changed");
    assert_eq!(
        waited["session"]["runtime"]["runtime_id"],
        live.worker_instance_id
    );
    assert_previous_spelling(waited);

    let inventory = exchange(
        &mut client,
        &previous_request(
            "inventory-1",
            method::SESSION_RUNTIME_INVENTORY,
            Value::Null,
        ),
    )
    .await;
    // A managed worker is not an inventory entry (only unadopted and quarantined
    // endpoints are), so the entry rename is covered by the golden fixtures.
    assert!(previous_ok(&inventory)["entries"].is_array(), "{inventory}");
    assert_previous_spelling(&inventory);

    let current_spelling = exchange(
        &mut client,
        &previous_request(
            "output-3",
            method::SESSION_OUTPUT,
            json!({
                "session_id": live.session_id,
                "runtime": {
                    "worker_instance_id": live.worker_instance_id,
                    "runtime_generation": live.runtime_generation,
                },
                "max_bytes": OUTPUT_MAX_BYTES,
            }),
        ),
    )
    .await;
    assert_eq!(current_spelling["v"], PREVIOUS_VERSION);
    assert_eq!(
        current_spelling["err"]["code"], "bad_request",
        "the protocol 4 spelling is not part of a protocol 3 request: {current_spelling}"
    );

    let stopped = exchange(
        &mut client,
        &previous_request("stop-1", method::SESSION_STOP, json!(live.session_id)),
    )
    .await;
    assert_eq!(previous_ok(&stopped)["stopped"], true);

    daemon.stop().await;
}

#[tokio::test]
async fn a_current_client_on_the_same_daemon_keeps_the_current_shape() {
    let daemon = Daemon::start("window-current").await;
    let mut previous = daemon.connect().await;
    let (live, _) = create_previous_session(&mut previous, &daemon).await;

    let mut current = daemon.connect().await;
    let request = Request::new(
        "inspect-current",
        method::SESSION_INSPECT,
        json!(live.session_id),
    )
    .expect("valid request");
    let response = exchange(
        &mut current,
        &serde_json::to_value(&request).expect("serialize request"),
    )
    .await;
    assert_eq!(response["v"], PROTOCOL_VERSION.get());
    assert_eq!(
        response["ok"]["runtime"]["worker_instance_id"],
        live.worker_instance_id
    );
    assert!(!contains_key(&response, "runtime_id"), "{response}");

    daemon.stop().await;
}

#[tokio::test]
async fn subscribers_of_both_versions_each_receive_their_own_event_shape() {
    let daemon = Daemon::start("window-events").await;
    let mut previous_events = daemon.connect().await;
    let ack = exchange(
        &mut previous_events,
        &previous_request("subscribe-previous", method::SUBSCRIBE, Value::Null),
    )
    .await;
    assert_eq!(ack["v"], PREVIOUS_VERSION);
    let mut current_events = daemon.connect().await;
    let request =
        Request::new("subscribe-current", method::SUBSCRIBE, Value::Null).expect("valid request");
    let ack = exchange(
        &mut current_events,
        &serde_json::to_value(&request).expect("serialize request"),
    )
    .await;
    assert_eq!(ack["v"], PROTOCOL_VERSION.get());

    let mut driver = daemon.connect().await;
    let (live, _) = create_previous_session(&mut driver, &daemon).await;

    let previous_event = next_event_with_runtime(&mut previous_events).await;
    assert_eq!(previous_event["v"], PREVIOUS_VERSION);
    assert_eq!(
        previous_event["session"]["runtime"]["runtime_id"],
        live.worker_instance_id
    );
    assert_previous_spelling(&previous_event);

    let current_event = next_event_with_runtime(&mut current_events).await;
    assert_eq!(current_event["v"], PROTOCOL_VERSION.get());
    assert_eq!(
        current_event["session"]["runtime"]["worker_instance_id"],
        live.worker_instance_id
    );
    assert!(
        !contains_key(&current_event, "runtime_id"),
        "{current_event}"
    );

    daemon.stop().await;
}

/// Reads events until one carries a session with a worker runtime.
async fn next_event_with_runtime(subscriber: &mut Client) -> Value {
    wait::guard("an event whose session carries a runtime", async {
        loop {
            let event = next_line(subscriber).await;
            if event["session"]["runtime"].is_object() {
                return event;
            }
        }
    })
    .await
}

#[tokio::test]
async fn invalid_input_on_a_previous_version_connection_is_answered_in_that_version() {
    let daemon = Daemon::start("window-invalid").await;
    let mut client = daemon.connect().await;
    let health = exchange(
        &mut client,
        &previous_request("health-1", method::DAEMON_HEALTH, Value::Null),
    )
    .await;
    assert_eq!(previous_ok(&health)["protocol_version"], PREVIOUS_VERSION);

    for line in ["", "not json", "{\"v\":1}"] {
        client
            .send(line.to_owned())
            .await
            .expect("send invalid input");
        let reply = next_line(&mut client).await;
        assert_eq!(reply["v"], PREVIOUS_VERSION, "{line:?}: {reply}");
        assert_eq!(reply["err"]["code"], "bad_request", "{line:?}: {reply}");
    }

    // Before anything is negotiated the daemon's current version is stamped.
    let mut fresh = daemon.connect().await;
    fresh
        .send("not json".to_owned())
        .await
        .expect("send invalid input");
    let reply = next_line(&mut fresh).await;
    assert_eq!(reply["v"], PROTOCOL_VERSION.get(), "{reply}");
    daemon.stop().await;
}

#[tokio::test]
async fn methods_added_after_the_previous_version_are_unknown_to_it() {
    let daemon = Daemon::start("window-introduced").await;
    let mut previous = daemon.connect().await;
    for name in protocol::compat::introduced_methods(MIN_PROTOCOL_VERSION) {
        let response = exchange(
            &mut previous,
            &previous_request("introduced-1", name, json!({})),
        )
        .await;
        assert_eq!(response["v"], PREVIOUS_VERSION, "{name}: {response}");
        assert_eq!(response["err"]["code"], "method_not_found", "{name}");
        assert!(response.get("ok").is_none(), "{name}: {response}");
    }

    // The same method is served on a current connection.
    let mut current = daemon.connect().await;
    let request =
        Request::new("package-list", method::PACKAGE_LIST, json!({})).expect("valid request");
    let response = exchange(
        &mut current,
        &serde_json::to_value(&request).expect("serialize request"),
    )
    .await;
    assert_eq!(response["v"], PROTOCOL_VERSION.get());
    assert_ne!(response["err"]["code"], "method_not_found", "{response}");
    daemon.stop().await;
}

#[tokio::test]
async fn a_client_two_versions_back_is_rejected_with_a_typed_mismatch() {
    let daemon = Daemon::start("window-below").await;
    let mut client = daemon.connect().await;
    let below = MIN_PROTOCOL_VERSION.get() - 1;
    let response = exchange(
        &mut client,
        &json!({
            "v": {"minimum": below, "maximum": below},
            "id": "below-1",
            "method": method::DAEMON_HEALTH,
            "params": null,
        }),
    )
    .await;
    assert_eq!(response["err"]["code"], "version_mismatch", "{response}");
    daemon.stop().await;
}

#[tokio::test]
async fn recorded_previous_release_consumer_requests_are_accepted() {
    let daemon = Daemon::start("window-consumers").await;
    let mut driver = daemon.connect().await;
    let (live, _) = create_previous_session(&mut driver, &daemon).await;
    let work_dir = daemon.socket.work_dir();

    let mut accepted = 0;
    let mut stops = Vec::new();
    for (file, raw) in CONSUMER_FIXTURES {
        let document: Value = serde_json::from_str(raw).expect("consumer fixture is JSON");
        assert_eq!(document["release"], "v0.33.0", "{file}");
        for recorded in document["requests"].as_array().expect("recorded requests") {
            let label = recorded["label"].as_str().expect("label");
            let sent = substitute(&recorded["line"], &live, &work_dir);
            let name = sent["method"].as_str().expect("method").to_owned();
            if name == method::SESSION_STOP {
                stops.push((format!("{file}: {label}"), sent));
                continue;
            }
            if sent["v"].is_u64() {
                // The recorded notify hooks send a bare integer `v`, a legacy
                // envelope no protocol 2 or later daemon reads.
                let response = exchange(&mut driver, &sent).await;
                assert_eq!(response["err"]["code"], "bad_request", "{file}: {label}");
                continue;
            }
            let mut client = daemon.connect().await;
            let response = exchange(&mut client, &sent).await;
            assert_accepted(file, label, &name, &response);
            accepted += 1;
        }
    }
    assert!(
        accepted >= 40,
        "only {accepted} recorded requests were replayed"
    );

    for (label, sent) in stops {
        let mut client = daemon.connect().await;
        let response = exchange(&mut client, &sent).await;
        assert_accepted("stop", &label, method::SESSION_STOP, &response);
    }
    daemon.stop().await;
}

/// A recorded request is accepted when it is answered in protocol 3 and is not
/// refused for its shape; the listed methods must also succeed.
fn assert_accepted(file: &str, label: &str, name: &str, response: &Value) {
    let context = format!("{file}: {label}: {response}");
    assert_eq!(response["v"], PREVIOUS_VERSION, "{context}");
    assert_previous_spelling(response);
    if let Some(error) = response.get("err") {
        let code = error["code"].as_str().expect("error code");
        let message = error["msg"].as_str().expect("error message");
        let integer_seq = INTEGER_SEQ_REJECTIONS.contains(&name)
            && message.contains("invalid type: integer")
            && message.contains("expected a string");
        assert!(
            integer_seq || !SHAPE_ERROR_CODES.contains(&code),
            "the daemon refused the shape: {context}"
        );
    }
    if MUST_SUCCEED.contains(&name) {
        assert!(response.get("ok").is_some(), "{context}");
    }
}

/// A socket the hooks dial that relays every request to the real daemon and
/// records each (request, response) exchange.
struct RecordingRelay {
    path: PathBuf,
    exchanges: Arc<std::sync::Mutex<Vec<(Value, Value)>>>,
}

impl RecordingRelay {
    fn start(daemon: &Daemon) -> Self {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

        let path = daemon.socket.dir.path().join("relay.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind relay socket");
        let upstream = daemon.socket.path.clone();
        let exchanges = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&exchanges);
        tokio::spawn(async move {
            while let Ok((stream, _addr)) = listener.accept().await {
                let (upstream, recorded) = (upstream.clone(), Arc::clone(&recorded));
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut request = String::new();
                    if BufReader::new(read).read_line(&mut request).await.is_err() {
                        return;
                    }
                    let Ok(mut client) = connect_once(&upstream).await else {
                        return;
                    };
                    let reply = exchange(
                        &mut client,
                        &serde_json::from_str(&request).expect("request JSON"),
                    )
                    .await;
                    recorded.lock().expect("relay lock").push((
                        serde_json::from_str(&request).expect("request JSON"),
                        reply.clone(),
                    ));
                    let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
                });
            }
        });
        Self { path, exchanges }
    }

    fn exchanges(&self) -> Vec<(Value, Value)> {
        self.exchanges.lock().expect("relay lock").clone()
    }
}

async fn connect_once(socket: &Path) -> std::io::Result<Client> {
    UnixStream::connect(socket)
        .await
        .map(|stream| Framed::new(stream, LinesCodec::new()))
}

/// Runs one real managed hook asset with the launch environment of a session
/// that baked `version` as `POHUNEK_PROTOCOL_VERSION` and returns its exit status.
async fn run_prebump_hook(
    relay: &RecordingRelay,
    live: &LiveSession,
    version: u32,
    agent: &str,
    script: &str,
    args: &[&str],
    stdin: &Value,
) -> std::process::ExitStatus {
    use tokio::io::AsyncWriteExt as _;

    // The Codex script ships as a template; the recorded rendering is
    // byte-identical to what the handler installs (pinned by a daemon unit test).
    let integration = pohunek_test_support::manifest_dir().join("src/integration");
    let asset = if agent == "codex" {
        integration
            .join("reporter_golden")
            .join(format!("codex-{script}"))
    } else {
        integration.join("assets").join(agent).join(script)
    };
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .arg(&asset)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("test PATH"))
        .env("TMPDIR", relay.path.parent().expect("relay dir"))
        .env("POHUNEK_ENV", "1")
        .env("POHUNEK_SOCKET_PATH", &relay.path)
        .env("POHUNEK_PROTOCOL_VERSION", version.to_string())
        .env("POHUNEK_SESSION_ID", &live.session_id)
        .env("POHUNEK_RUNTIME_ID", &live.worker_instance_id)
        .stdin(std::process::Stdio::piped());
    let mut child = command.spawn().expect("spawn managed hook");
    let mut pipe = child.stdin.take().expect("hook stdin");
    pipe.write_all(stdin.to_string().as_bytes())
        .await
        .expect("write hook input");
    drop(pipe);
    wait::guard("the managed hook to exit", child.wait())
        .await
        .expect("hook exit status")
}

/// Protocol version of the current release.
const CURRENT_VERSION: u32 = PROTOCOL_VERSION.get();

/// Asserts the exchange was stamped and answered in `version` and succeeded.
fn assert_hook_exchange(version: u32, request: &Value, reply: &Value) {
    let context = format!("{request} => {reply}");
    assert_eq!(
        request["v"],
        json!({"minimum": version, "maximum": version}),
        "{context}"
    );
    assert_eq!(reply["v"], version, "{context}");
    assert!(reply.get("ok").is_some(), "{context}");
    assert_previous_spelling_if(version, reply);
}

fn assert_previous_spelling_if(version: u32, reply: &Value) {
    if version == PREVIOUS_VERSION {
        assert_previous_spelling(reply);
    }
}

#[tokio::test]
async fn prebump_state_hooks_report_through_the_daemon_socket() {
    let daemon = Daemon::start("prebump-state").await;
    let relay = RecordingRelay::start(&daemon);
    let mut driver = daemon.connect().await;
    let (live, _) = create_previous_session(&mut driver, &daemon).await;
    let input = json!({"session_id": "native-1", "transcript_path": "/work/t.jsonl"});

    for agent in ["claude", "codex"] {
        let mut outcomes = Vec::new();
        for version in [PREVIOUS_VERSION, CURRENT_VERSION] {
            let before = relay.exchanges().len();
            // Only the Claude state hook has a release action.
            let actions: &[&str] = if agent == "claude" {
                &["session", "release"]
            } else {
                &["session"]
            };
            for action in actions.iter().copied() {
                let status = run_prebump_hook(
                    &relay,
                    &live,
                    version,
                    agent,
                    "pohunek-agent-state.sh",
                    &[action],
                    &input,
                )
                .await;
                assert!(status.success(), "{agent} {action} v{version}");
            }
            let exchanges = relay.exchanges().split_off(before);
            let methods: Vec<_> = exchanges
                .iter()
                .map(|(request, _)| request["method"].as_str().expect("method").to_owned())
                .collect();
            let mut expected = vec![
                method::SESSION_REPORT_AGENT,
                method::SESSION_REPORT_NATIVE_ID,
            ];
            if agent == "claude" {
                expected.push(method::SESSION_RELEASE_AGENT);
            }
            assert_eq!(methods, expected, "{agent} v{version}");
            for (request, reply) in &exchanges {
                assert_hook_exchange(version, request, reply);
            }
            let native = &exchanges[1].0["params"];
            let key = if version == PREVIOUS_VERSION {
                "runtime_id"
            } else {
                "worker_instance_id"
            };
            assert_eq!(
                native[key],
                json!(live.worker_instance_id),
                "{agent} v{version}"
            );
            outcomes.push(
                exchanges
                    .iter()
                    .map(|(_, reply)| reply["ok"].clone())
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(
            outcomes[0], outcomes[1],
            "{agent}: a version 3 session is served the same outcomes as a current one"
        );
    }
    daemon.stop().await;
}

#[tokio::test]
async fn prebump_notify_hooks_create_notifications_through_the_daemon_socket() {
    let daemon = Daemon::start("prebump-notify").await;
    let relay = RecordingRelay::start(&daemon);
    let mut driver = daemon.connect().await;
    let (live, _) = create_previous_session(&mut driver, &daemon).await;

    let cases: [(&str, &[&str]); 4] = [
        ("claude", &["notification", "auth_success"]),
        ("claude", &["stop"]),
        ("codex", &["permission_request"]),
        ("codex", &["stop"]),
    ];
    for (agent, args) in cases {
        let before = relay.exchanges().len();
        let status = run_prebump_hook(
            &relay,
            &live,
            PREVIOUS_VERSION,
            agent,
            "pohunek-agent-notify.sh",
            args,
            &json!({"hook_event_id": "evt-1"}),
        )
        .await;
        assert!(status.success(), "{agent} {args:?}");
        let exchanges = relay.exchanges().split_off(before);
        assert_eq!(exchanges.len(), 1, "{agent} {args:?}: {exchanges:?}");
        let (request, reply) = &exchanges[0];
        assert_eq!(request["method"], json!(method::NOTIFICATION_CREATE));
        assert_hook_exchange(PREVIOUS_VERSION, request, reply);
        assert_eq!(
            reply["ok"]["created"],
            json!(true),
            "{agent} {args:?}: {reply}"
        );
        assert_eq!(reply["ok"]["record"]["session_id"], json!(live.session_id));
        assert_eq!(reply["ok"]["record"]["agent_kind"], json!(agent));
    }
    daemon.stop().await;
}
