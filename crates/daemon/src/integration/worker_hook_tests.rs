//! Notification hooks delivered through a real worker socket.
//!
//! A real `pohunek-agent-notify.sh` runs inside a real worker's PTY, reaches the
//! worker over `POHUNEK_WORKER_SOCKET_PATH`, and the worker forwards to a daemon
//! socket served by the real request dispatcher.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pohunek_session_worker::{Server, ServerArgs, WorkerConfig};
use pohunek_worker_protocol::{
    Dimensions, Initialize, InitializeLimits, LaunchIdentity, SecretEnv, SessionId, StopPolicy,
    TransactionId, WorkerId, CURRENT_VERSION, DEFAULT_ENVIRONMENT_ALLOWLIST,
};
use protocol::method;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::api::{dispatch_line, DaemonState, Dispatch, HealthInfo};
use crate::runtime::Worker;

const SESSION_ID: &str = "s-9101";
const WORKER_ID: &str = "worker-notify";

/// Daemon socket served by the real dispatcher; records every request line.
struct DaemonStub {
    lines: Arc<Mutex<Vec<String>>>,
    replies: Arc<Mutex<Vec<String>>>,
}

impl DaemonStub {
    fn spawn(root: &Path, notification_dir: &Path) -> Self {
        let notifications = crate::notifications::NotificationService::open(notification_dir)
            .expect("notification service");
        let mut policy = crate::notifications::default_policy();
        policy.enabled = protocol::NotificationKindPolicy {
            agent_blocked: true,
            approval_required: true,
            turn_completed: true,
            session_finished: true,
            error: true,
            system: true,
        };
        policy.providers.clear();
        notifications.set_policy(policy).expect("enable all kinds");
        let (attention, _task) =
            crate::notifications::AttentionCoordinator::spawn(notifications.clone());
        let state = Arc::new(
            DaemonState::new(
                HealthInfo::new("test"),
                crate::session::SessionRegistry::new(
                    crate::session::SessionRegistryConfig::default(),
                ),
                Arc::new(crate::governance::HostGovernanceService::open_test()),
                crate::test_support::overlay_registry(),
            )
            .with_notifications(notifications)
            .with_attention_coordinator(attention),
        );
        let lines = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(Vec::new()));
        let listener = UnixListener::bind(root.join("daemon.sock")).expect("bind daemon socket");
        let (serve_state, serve_lines, serve_replies) =
            (Arc::clone(&state), Arc::clone(&lines), Arc::clone(&replies));
        tokio::spawn(async move {
            while let Ok((stream, _addr)) = listener.accept().await {
                let (state, lines, replies) = (
                    Arc::clone(&serve_state),
                    Arc::clone(&serve_lines),
                    Arc::clone(&serve_replies),
                );
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut line = String::new();
                    if BufReader::new(read).read_line(&mut line).await.is_err() {
                        return;
                    }
                    lines.lock().expect("lines lock").push(line.clone());
                    let Dispatch::Reply(reply) = dispatch_line(&line, &state, None).await else {
                        return;
                    };
                    replies.lock().expect("replies lock").push(reply.clone());
                    let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
                });
            }
        });
        Self { lines, replies }
    }

    fn reply_lines(&self) -> Vec<Value> {
        self.replies
            .lock()
            .expect("replies lock")
            .iter()
            .map(|line| serde_json::from_str(line).expect("reply line is JSON"))
            .collect()
    }

    fn request_lines(&self) -> Vec<Value> {
        self.lines
            .lock()
            .expect("lines lock")
            .iter()
            .map(|line| serde_json::from_str(line).expect("request line is JSON"))
            .collect()
    }
}

fn initialize(root: &Path, script: &str, public_protocol_version: u32) -> Initialize {
    Initialize {
        session_id: SessionId::new(SESSION_ID).expect("session id"),
        transaction_id: TransactionId::new("transaction-notify").expect("transaction id"),
        expected_worker_id: WorkerId::new(WORKER_ID).expect("worker id"),
        launch: LaunchIdentity {
            agent: "claude".to_owned(),
            agent_base: "claude".to_owned(),
            reference_kind: None,
        },
        executable: PathBuf::from("/bin/sh"),
        arguments: vec!["-c".to_owned(), script.to_owned()],
        cwd: root.to_path_buf(),
        dimensions: Dimensions::new(80, 24).expect("dimensions"),
        environment: SecretEnv::new(BTreeMap::new()).expect("environment"),
        base_environment: Some(
            crate::runtime::environment::base_environment(
                DEFAULT_ENVIRONMENT_ALLOWLIST,
                &crate::test_support::thread_environment_source(),
            )
            .expect("base environment"),
        ),
        limits: InitializeLimits::new(1_048_576, 1_048_576, 1_024, 10_000).expect("limits"),
        stop_policy: StopPolicy::new(500).expect("stop policy"),
        hook_protocol_version: CURRENT_VERSION,
        public_protocol_version,
    }
}

fn notify_asset(agent: &str) -> PathBuf {
    pohunek_test_support::manifest_dir()
        .join("src/integration/assets/")
        .join(agent)
        .join("pohunek-agent-notify.sh")
}

/// Starts a real worker whose PTY child runs `script`, wired to the stub daemon.
async fn start_worker(
    root: &Path,
    script: &str,
    public_protocol_version: u32,
) -> (
    Worker,
    tokio::task::JoinHandle<Result<(), pohunek_session_worker::WorkerError>>,
    PathBuf,
) {
    let socket_path = root.join("socket/worker.sock");
    let server = Server::bind(ServerArgs {
        session_id: SESSION_ID.to_owned(),
        worker_id: WORKER_ID.to_owned(),
        generation: "abcd2345".to_owned(),
        socket_path: socket_path.clone(),
        journal_path: root.join("journal/worker.json"),
        daemon_socket_path: root.join("daemon.sock"),
        config: WorkerConfig::new(),
    })
    .await
    .expect("bind real worker");
    let task = tokio::spawn(server.serve());
    let worker = Worker::connect(&socket_path, SESSION_ID, "daemon-notify")
        .await
        .expect("control handshake");
    worker
        .initialize(initialize(root, script, public_protocol_version))
        .await
        .expect("initialize runtime");
    (worker, task, socket_path)
}

/// Runs the real notify asset of `agent` inside a real worker initialized with
/// `public_version` and returns the daemon's recorded (request, reply) pair.
async fn deliver_through_worker(agent: &str, args: &str, public_version: u32) -> (Value, Value) {
    let root = crate::test_support::scoped_dir("ph-notify-worker-");
    let notifications = crate::test_support::scoped_dir("ph-notify-data-");
    let daemon = DaemonStub::spawn(&root, &notifications);
    // The piped provider payload closes the hook's stdin; `; true` keeps the
    // shell from exec-ing the hook, so the hook's parent is the PTY root the
    // worker attests.
    let script = format!(
        "printf '{{}}' | /bin/sh {} {args}; true",
        notify_asset(agent).display()
    );
    let (worker, task, _socket) = start_worker(&root, &script, public_version).await;

    Box::pin(pohunek_test_support::wait::wait_until(
        "the worker forwards the notification",
        || async { (!daemon.reply_lines().is_empty()).then_some(()) },
    ))
    .await;

    let requests = daemon.request_lines();
    assert_eq!(
        requests.len(),
        1,
        "exactly one request reaches the daemon: {requests:?}"
    );
    let replies = daemon.reply_lines();
    drop(worker);
    task.abort();
    (requests[0].clone(), replies[0].clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn notify_hook_delivers_through_the_worker_socket_to_a_created_notification() {
    let current = protocol::PROTOCOL_VERSION.get();
    let (request, reply) =
        deliver_through_worker("claude", "notification auth_success", current).await;
    assert_eq!(request["method"], json!(method::NOTIFICATION_CREATE));
    assert!(
        request["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("worker-hook:")),
        "the create must be forwarded by the worker, not dialed by the hook: {request}"
    );
    assert_eq!(request["params"]["session_id"], json!(SESSION_ID));
    assert_eq!(reply["ok"]["created"], json!(true), "{reply}");
    assert_eq!(reply["ok"]["record"]["agent_kind"], json!("claude"));
    assert_eq!(reply["ok"]["record"]["session_id"], json!(SESSION_ID));
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_forwards_notifications_in_the_version_its_session_launched_with() {
    const PREVIOUS: u32 = 3;
    let cases = [
        ("claude", "notification auth_success"),
        ("claude", "stop"),
        ("codex", "permission_request"),
        ("codex", "stop"),
    ];
    for (agent, args) in cases {
        let (request, reply) = deliver_through_worker(agent, args, PREVIOUS).await;
        assert_eq!(
            request["v"],
            json!({"minimum": PREVIOUS, "maximum": PREVIOUS}),
            "{agent} {args}: {request}"
        );
        assert!(
            request["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("worker-hook:")),
            "{agent} {args}: {request}"
        );
        assert_eq!(reply["v"], json!(PREVIOUS), "{agent} {args}: {reply}");
        assert_eq!(
            reply["ok"]["created"],
            json!(true),
            "{agent} {args}: {reply}"
        );
        assert_eq!(reply["ok"]["record"]["session_id"], json!(SESSION_ID));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_rejects_a_notification_claim_from_outside_the_session() {
    let root = crate::test_support::scoped_dir("ph-notify-spoof-");
    let notifications = crate::test_support::scoped_dir("ph-notify-spoof-data-");
    let daemon = DaemonStub::spawn(&root, &notifications);
    let (worker, task, socket) =
        start_worker(&root, "sleep 30", protocol::PROTOCOL_VERSION.get()).await;
    let instance = worker
        .worker_instance_id()
        .await
        .expect("worker instance")
        .to_string();

    let claim = json!({
        "type": "notification_create",
        "runtime_id": instance,
        "provider": "claude",
        "pid": std::process::id(),
        "start_identity": 1,
        "sequence": 1,
        "params": {"title": "spoofed"},
    });
    let mut stream = UnixStream::connect(&socket).await.expect("connect worker");
    stream
        .write_all(format!("{claim}\n").as_bytes())
        .await
        .expect("send claim");
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .await
        .expect("worker reply");
    let reply: Value = serde_json::from_str(&reply).expect("reply is JSON");

    assert_eq!(reply["ok"], json!(false));
    assert!(daemon.request_lines().is_empty(), "nothing is forwarded");
    drop(worker);
    task.abort();
}

/// Runs the real notify asset with a worker socket that answers `reply`
/// (or never exists when `None`) and the stub daemon socket as fallback.
async fn run_hook_with_worker_reply(
    name: &str,
    reply: Option<&'static str>,
    agent: &str,
    args: &[&str],
    public_version: u32,
) -> (Vec<Value>, Vec<Value>) {
    let root = crate::test_support::scoped_dir(name);
    let notifications = crate::test_support::scoped_dir("ph-notify-fallback-data-");
    let daemon = DaemonStub::spawn(&root, &notifications);
    let worker_socket = root.join("worker.sock");
    if let Some(reply) = reply {
        let listener = UnixListener::bind(&worker_socket).expect("bind scripted worker");
        tokio::spawn(async move {
            while let Ok((stream, _addr)) = listener.accept().await {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                let _ = BufReader::new(read).read_line(&mut line).await;
                let _ = write.write_all(reply.as_bytes()).await;
            }
        });
    }
    let asset = notify_asset(agent);
    let daemon_socket = root.join("daemon.sock");
    let output = tokio::process::Command::new("/bin/sh")
        .arg(&asset)
        .args(args)
        .env_clear()
        .env("PATH", super::tests::inherited_path())
        .env("TMPDIR", &*root)
        .env("POHUNEK_ENV", "1")
        .env("POHUNEK_SESSION_ID", SESSION_ID)
        .env("POHUNEK_SOCKET_PATH", &daemon_socket)
        .env("POHUNEK_PROTOCOL_VERSION", public_version.to_string())
        .env("POHUNEK_WORKER_SOCKET_PATH", &worker_socket)
        .env("POHUNEK_WORKER_INSTANCE_ID", "instance-1")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("run notify hook");
    assert!(output.status.success(), "hook must exit 0");
    (daemon.request_lines(), daemon.reply_lines())
}

#[tokio::test(flavor = "multi_thread")]
async fn notify_hook_falls_back_to_the_daemon_when_the_worker_refuses_or_is_absent() {
    for (name, reply) in [
        ("ph-notify-refused-", Some("{\"ok\":false}\n")),
        ("ph-notify-unknown-", Some("not json\n")),
        ("ph-notify-absent-", None),
    ] {
        let (requests, _) = run_hook_with_worker_reply(
            name,
            reply,
            "claude",
            &["notification", "auth_success"],
            protocol::PROTOCOL_VERSION.get(),
        )
        .await;
        assert_eq!(requests.len(), 1, "{name}: {requests:?}");
        assert!(
            requests[0]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("hook:claude:")),
            "{name}: the hook dials the daemon itself: {}",
            requests[0]
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn prebump_notify_hooks_fall_back_to_the_daemon_in_their_launch_version() {
    const PREVIOUS: u32 = 3;
    let cases: [(&str, &[&str]); 2] = [
        ("claude", &["notification", "auth_success"]),
        ("codex", &["permission_request"]),
    ];
    for (agent, args) in cases {
        let (requests, replies) =
            run_hook_with_worker_reply("ph-notify-prebump-", None, agent, args, PREVIOUS).await;
        assert_eq!(requests.len(), 1, "{agent}: {requests:?}");
        assert_eq!(
            requests[0]["v"],
            json!({"minimum": PREVIOUS, "maximum": PREVIOUS}),
            "{agent}: {}",
            requests[0]
        );
        assert_eq!(replies[0]["v"], json!(PREVIOUS), "{agent}: {}", replies[0]);
        assert_eq!(
            replies[0]["ok"]["created"],
            json!(true),
            "{agent}: {}",
            replies[0]
        );
    }
}
