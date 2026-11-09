//! Integration test: a real daemon serves exactly the current public protocol
//! version.
//!
//! A worker-backed daemon is bound on a Unix socket and driven with raw JSON
//! lines. The window holds protocol 5 only, so a client that announces an
//! earlier version is refused at the negotiation boundary, and the notification
//! methods protocol 5 removed answer `method_not_found`.

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
use pohunek_daemon::shutdown::DaemonShutdown;
use pohunek_test_support::wait::{self, wait_until};
use pohunek_test_support::worker_binary;
use protocol::{method, Request, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

/// Methods no version of the window serves any more.
const REMOVED_METHODS: &[&str] = &[
    "session.fork",
    "session.runtime_inventory",
    "session.policy.get",
    "session.policy.set",
    "session.idle_stop.get",
    "session.idle_stop.set",
    "store.rejected_list",
    "store.rejected_set_aside",
    "notification.update",
    "notification.delete",
    "notification.policy.get",
    "notification.policy.set",
    "notification.retention.prune",
];

type Client = Framed<UnixStream, LinesCodec>;

/// A unique socket path inside a dedicated per-test directory.
struct TestSocket {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TestSocket {
    fn new(tag: &str) -> Self {
        let dir = pohunek_test_support::tempdir_with_prefix(&format!("ph-{tag}-"))
            .expect("create test socket dir");
        std::fs::create_dir(dir.path().join("work")).expect("create the session working directory");
        Self {
            path: dir.path().join("daemon.sock"),
            _dir: dir,
        }
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
        let (shutdown, handle, _daemon_instance_id) = spawn_server(&socket).await;
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

async fn spawn_server(socket: &Path) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>, String) {
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
    let daemon_instance_id = registry.daemon_instance_id().to_owned();
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
    // Session-scoped notifications (approval, turn) are debounced by the coordinator.
    let (attention, _attention_task) =
        AttentionCoordinator::spawn(&DaemonShutdown::default(), notifications.clone(), &registry);
    let state = DaemonState::new(
        HealthInfo::new("0.0.0"),
        registry,
        governance,
        support::overlay_registry(),
        pohunek_daemon::api::OverlaySource::Default,
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
            .serve(
                pohunek_daemon::api::ConnectionTasks::new(
                    "control_connections",
                    &tokio_util::sync::CancellationToken::new(),
                ),
                async move {
                    let _ = rx.await;
                },
            )
            .await;
    });
    (tx, handle, daemon_instance_id)
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

/// A request line announcing exactly `version`.
fn request_at(version: u32, id: &str, method: &str, params: &Value) -> Value {
    json!({
        "v": {"minimum": version, "maximum": version},
        "id": id,
        "method": method,
        "params": params,
    })
}

#[tokio::test]
async fn the_current_version_is_served_and_stamped() {
    let daemon = Daemon::start("window-current").await;
    let mut client = daemon.connect().await;
    let health = exchange(
        &mut client,
        &request_at(
            PROTOCOL_VERSION.get(),
            "health",
            method::DAEMON_HEALTH,
            &Value::Null,
        ),
    )
    .await;
    assert_eq!(health["v"], PROTOCOL_VERSION.get(), "{health}");
    assert_eq!(
        health["ok"]["protocol_version"],
        PROTOCOL_VERSION.get(),
        "{health}"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_version_outside_the_window_is_refused_and_invalid_input_is_a_bad_request() {
    let daemon = Daemon::start("window-outside").await;
    let mut client = daemon.connect().await;

    // The window is the current version alone: the previous protocol and one
    // beyond the current are both refused at the negotiation boundary.
    assert_eq!(MIN_PROTOCOL_VERSION, PROTOCOL_VERSION);
    for (id, outside) in [
        ("previous", PROTOCOL_VERSION.get() - 1),
        ("below-window", PROTOCOL_VERSION.get() - 2),
        ("above-window", PROTOCOL_VERSION.get() + 1),
    ] {
        let line = request_at(outside, id, method::DAEMON_HEALTH, &Value::Null);
        let reply = exchange(&mut client, &line).await;
        assert_eq!(reply["id"], id, "{line}: {reply}");
        assert_eq!(reply["err"]["code"], "version_mismatch", "{line}: {reply}");
    }

    for line in ["", "not json", "{\"v\":1}"] {
        client
            .send(line.to_owned())
            .await
            .expect("send invalid input");
        let reply = next_line(&mut client).await;
        assert_eq!(reply["err"]["code"], "bad_request", "{line:?}: {reply}");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn removed_methods_are_unknown() {
    let daemon = Daemon::start("window-removed").await;
    let mut current = daemon.connect().await;
    for name in REMOVED_METHODS {
        let request = Request::new("removed-current", *name, json!({})).expect("valid request");
        let response = exchange(
            &mut current,
            &serde_json::to_value(&request).expect("serialize request"),
        )
        .await;
        assert_eq!(response["v"], PROTOCOL_VERSION.get(), "{name}: {response}");
        assert_eq!(response["err"]["code"], "method_not_found", "{name}");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn the_kept_notification_methods_keep_their_shape() {
    let daemon = Daemon::start("window-kept").await;
    let mut client = daemon.connect().await;
    let create = Request::new(
        "create",
        method::NOTIFICATION_CREATE,
        json!({
            "source": {
                "provider": "scenario-tool",
                "provider_event": "PermissionRequest",
                "host_local_source_id": "scenario-1"
            },
            "kind": "approval_required",
            "severity": "action_required",
            "title": "Approval required",
            "body": "A tool waits for approval."
        }),
    )
    .expect("valid request");
    let created = exchange(
        &mut client,
        &serde_json::to_value(&create).expect("serialize request"),
    )
    .await;
    assert_eq!(created["ok"]["created"], true, "{created}");
    assert_eq!(created["ok"]["record"]["kind"], "approval_required");

    let list = Request::new("list", method::NOTIFICATION_LIST, json!({})).expect("valid request");
    let listed = exchange(
        &mut client,
        &serde_json::to_value(&list).expect("serialize request"),
    )
    .await;
    assert_eq!(
        listed["ok"]["notifications"][0]["id"], created["ok"]["record"]["id"],
        "{listed}"
    );
    daemon.stop().await;
}
