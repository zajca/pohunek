//! Notification hooks delivered through a real worker socket.
//!
//! A real `pohunek-agent-notify.sh` runs inside a real worker's PTY, reaches the
//! worker over `POHUNEK_WORKER_SOCKET_PATH`, and the worker forwards to a daemon
//! socket served by the real request dispatcher.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pohunek_session_worker::{Server, ServerArgs, WorkerConfig};
use pohunek_worker_protocol::{
    Dimensions, Initialize, InitializeLimits, InputPolicy, LaunchIdentity, SecretEnv, SessionId,
    StopPolicy, TransactionId, WorkerId, CURRENT_VERSION, DEFAULT_ENVIRONMENT_ALLOWLIST,
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
    notifications: crate::notifications::NotificationService,
}

impl DaemonStub {
    fn spawn(root: &Path, notification_dir: &Path) -> Self {
        Self::spawn_delaying(root, notification_dir, Duration::ZERO)
    }

    /// Like [`Self::spawn`], but dispatches every request `reply_delay` after
    /// it was received, which holds back the reply the caller waits for.
    fn spawn_delaying(root: &Path, notification_dir: &Path, reply_delay: Duration) -> Self {
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
        let sessions =
            crate::session::SessionRegistry::new(crate::session::SessionRegistryConfig::default());
        let (attention, _task) = crate::notifications::AttentionCoordinator::spawn(
            &crate::shutdown::DaemonShutdown::default(),
            notifications.clone(),
            &sessions,
        );
        let state = Arc::new(
            DaemonState::new(
                HealthInfo::new("test"),
                sessions,
                Arc::new(crate::governance::HostGovernanceService::open_test()),
                crate::test_support::overlay_registry(),
                crate::api::OverlaySource::Default,
            )
            .with_notifications(notifications.clone())
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
                    // timing-allowed: #1345 the injected daemon latency is the scenario input; the test waits on the replies, not on this sleep
                    tokio::time::sleep(reply_delay).await;
                    let Dispatch::Reply(reply) = dispatch_line(&line, &state, None).await else {
                        return;
                    };
                    replies.lock().expect("replies lock").push(reply.clone());
                    let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
                });
            }
        });
        Self {
            lines,
            replies,
            notifications,
        }
    }

    fn records(&self) -> Vec<protocol::NotificationRecord> {
        self.notifications
            .list(protocol::NotificationListParams::default())
            .expect("list the notification records")
            .notifications
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

fn initialize(
    root: &Path,
    script: &str,
    public_protocol_version: u32,
    hook_schema: &str,
) -> Initialize {
    Initialize {
        session_id: SessionId::new(SESSION_ID).expect("session id"),
        daemon_instance_id: Some(
            pohunek_worker_protocol::DaemonId::new("daemon-hook-test").expect("daemon id"),
        ),
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
        base_environment: crate::runtime::environment::base_environment(
            DEFAULT_ENVIRONMENT_ALLOWLIST,
            &crate::test_support::thread_environment_source(),
        )
        .expect("base environment"),
        limits: InitializeLimits::new(1_048_576, 1_048_576, 1_024, 10_000).expect("limits"),
        stop_policy: StopPolicy::new(500).expect("stop policy"),
        input_policy: InputPolicy::new(3000).expect("input policy"),
        hook_protocol_version: CURRENT_VERSION,
        public_protocol_version,
        hook_schema: Some(hook_schema.to_owned()),
        answers_program_status_probe: false,
    }
}

fn notify_asset(agent: &str) -> PathBuf {
    super::runnable_script(agent, "pohunek-agent-notify.sh")
}

/// Hook schema the notification tests launch their worker with.
const NOTIFY_SCHEMA: &str = "identity-subagent-v1";

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
    start_worker_with_schema(root, script, public_protocol_version, NOTIFY_SCHEMA).await
}

/// Starts a real worker initialized with the hook schema `hook_schema`.
async fn start_worker_with_schema(
    root: &Path,
    script: &str,
    public_protocol_version: u32,
    hook_schema: &str,
) -> (
    Worker,
    tokio::task::JoinHandle<Result<(), pohunek_session_worker::WorkerError>>,
    PathBuf,
) {
    start_worker_initialized(
        root,
        initialize(root, script, public_protocol_version, hook_schema),
    )
    .await
}

/// Hook failure trace of the worker started under `root`, which the worker
/// hands to its PTY child as `POHUNEK_HOOK_TRACE_PATH`.
///
/// It sits directly in the scenario's directory: the hooks create the file,
/// never its directory.
fn hook_trace_path(root: &Path) -> PathBuf {
    root.join(pohunek_logging::config::HOOK_TRACE_FILE_NAME)
}

/// Starts a real worker and initializes its runtime with `initialize`.
async fn start_worker_initialized(
    root: &Path,
    initialize: Initialize,
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
        hook_trace_path: hook_trace_path(root),
        config: WorkerConfig::new(),
    })
    .await
    .expect("bind real worker");
    let task = tokio::spawn(server.serve());
    let worker = Worker::connect(&socket_path, SESSION_ID, "daemon-notify")
        .await
        .expect("control handshake");
    worker
        .initialize(initialize)
        .await
        .expect("initialize runtime");
    (worker, task, socket_path)
}

/// What the daemon received and recorded for one hook run.
struct Delivery {
    requests: Vec<Value>,
    replies: Vec<Value>,
    records: Vec<protocol::NotificationRecord>,
}

/// Runs the real notify asset of `agent` inside a real worker initialized with
/// `public_version` and returns the daemon's recorded (request, reply) pair.
async fn deliver_through_worker(agent: &str, args: &str, public_version: u32) -> (Value, Value) {
    let delivery = deliver(agent, args, public_version, Duration::ZERO, 1).await;
    (delivery.requests[0].clone(), delivery.replies[0].clone())
}

/// Runs the real notify asset of `agent` against a daemon that answers
/// `reply_delay` late and waits until it has replied `replies` times.
async fn deliver(
    agent: &str,
    args: &str,
    public_version: u32,
    reply_delay: Duration,
    replies: usize,
) -> Delivery {
    let root = crate::test_support::scoped_dir("ph-notify-worker-");
    let notifications = crate::test_support::scoped_dir("ph-notify-data-");
    let daemon = DaemonStub::spawn_delaying(&root, &notifications, reply_delay);
    // The piped provider payload closes the hook's stdin; `; true` keeps the
    // shell from exec-ing the hook, so the hook's parent is the PTY root the
    // worker attests.
    let script = format!(
        "printf '{{}}' | /bin/sh {} {args}; true",
        notify_asset(agent).display()
    );
    let (worker, task, _socket) = start_worker(&root, &script, public_version).await;

    Box::pin(pohunek_test_support::wait::wait_until(
        "the daemon has replied",
        || async { (daemon.reply_lines().len() >= replies).then_some(()) },
    ))
    .await;

    let delivery = Delivery {
        requests: daemon.request_lines(),
        replies: daemon.reply_lines(),
        records: daemon.records(),
    };
    drop(worker);
    task.abort();
    delivery
}

/// The hook gives the worker `SOCKET_TIMEOUT_SECS` (0.5 s) to answer before it
/// dials the daemon itself; a daemon this late forces that fallback.
const REPLY_DELAY_PAST_HOOK_TIMEOUT: Duration = Duration::from_millis(1_200);

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_that_outwaits_the_worker_falls_back_without_a_second_notification() {
    let delivery = deliver(
        "claude",
        "notification auth_success",
        protocol::PROTOCOL_VERSION.get(),
        REPLY_DELAY_PAST_HOOK_TIMEOUT,
        2,
    )
    .await;
    assert!(
        delivery.requests[0]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("worker-hook:")),
        "the worker's forward arrives first: {:?}",
        delivery.requests
    );
    assert!(
        delivery.requests[1]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("hook:")),
        "the hook dials the daemon itself after the worker timed out: {:?}",
        delivery.requests
    );
    assert_eq!(
        delivery.requests[0]["params"]["source_id"], delivery.requests[1]["params"]["source_id"],
        "both deliveries name the same source"
    );
    let created = delivery
        .replies
        .iter()
        .filter(|reply| reply["ok"]["created"] == json!(true))
        .count();
    assert_eq!(created, 1, "one delivery creates: {:?}", delivery.replies);
    assert_eq!(
        delivery.records.len(),
        1,
        "the store holds one notification: {:?}",
        delivery.records
    );
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

fn state_asset(agent: &str) -> PathBuf {
    super::runnable_script(agent, "pohunek-agent-state.sh")
}

/// Runs the real Claude state asset inside a real worker initialized with
/// `hook_schema`: a subagent start, then an identity report.
///
/// The identity report is admitted by every schema, and the PTY child runs
/// the hooks one after the other, so once the worker shows the identity the
/// subagent start has been decided too. Returns the worker's snapshot at that
/// point.
///
/// The identity hook runs under an intermediate shell, so the agent process it
/// reports is that shell rather than the PTY root: `identity-v1` accepts a
/// report naming the root only from the root itself, and this hook is a child.
async fn report_through_worker(hook_schema: &str) -> pohunek_worker_protocol::InspectSnapshot {
    let root = crate::test_support::scoped_dir("ph-state-worker-");
    let asset = state_asset("claude");
    let script = format!(
        concat!(
            "printf '{{\"agent_id\":\"sub-1\",\"agent_type\":\"Explore\"}}' | /bin/sh {asset} subagent-start; ",
            "printf '{{\"session_id\":\"native-1\"}}' | /bin/sh -c '/bin/sh {asset} session; true'; ",
            "sleep 30"
        ),
        asset = asset.display()
    );
    let (worker, task, _socket) = start_worker_with_schema(
        &root,
        &script,
        protocol::PROTOCOL_VERSION.get(),
        hook_schema,
    )
    .await;
    let snapshot = Box::pin(pohunek_test_support::wait::wait_until(
        "the worker records the identity report",
        || async {
            let snapshot = worker.inspect().await.expect("inspect");
            snapshot.active_identity.is_some().then_some(snapshot)
        },
    ))
    .await;
    worker
        .stop(TransactionId::new("transaction-state-stop").expect("transaction id"))
        .await
        .expect("stop the retained PTY");
    drop(worker);
    task.abort();
    snapshot
}

#[tokio::test(flavor = "multi_thread")]
async fn a_report_of_a_newer_asset_set_is_refused_by_a_session_on_an_older_schema() {
    let older = report_through_worker("identity-v1").await;
    assert!(
        older.subagents.is_empty(),
        "the older schema admits no subagent report: {:?}",
        older.subagents
    );
    assert_eq!(older.hook_schema.as_deref(), Some("identity-v1"));
    assert_eq!(
        older
            .active_identity
            .as_ref()
            .map(|identity| identity.provider.as_str()),
        Some("claude"),
        "the older schema still admits the identity report"
    );

    let current = report_through_worker("identity-subagent-v1").await;
    let [subagent] = current.subagents.as_slice() else {
        panic!("the same hooks reach a session on the newer schema: {current:?}");
    };
    assert_eq!(subagent.id, "sub-1");
    assert_eq!(subagent.provider, "claude");
    assert_eq!(
        current
            .active_identity
            .as_ref()
            .map(|identity| identity.provider.as_str()),
        Some("claude")
    );
}

/// Subagent lifecycle scenarios: real state hooks run inside a real worker's
/// PTY, report provider subagent events through the worker socket, and
/// `Worker::inspect` shows the journaled rows.
///
/// Expected rows come from the Claude Code hook sequences recorded on #883
/// (`compat/claude/subagent-hook-sequences.json`), from the Codex hook schema
/// fixture, and from the subagent rules decided on #883, never from the
/// worker's own output.
mod subagent_sequences {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use pohunek_test_support::wait::wait_until;
    use pohunek_worker_protocol::{
        BackgroundItem, BackgroundKind, BackgroundSnapshot, BackgroundStatus, InspectSnapshot,
        RuntimePhase, SubagentPhase, SubagentSnapshot, TransactionId,
    };
    use serde::Deserialize;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tracing_subscriber::layer::SubscriberExt;

    use super::{initialize, start_worker_initialized, state_asset, DaemonStub, SESSION_ID};
    use crate::runtime::Worker;

    /// Claude Code subagent hook sequences recorded on #883.
    const RECORDED: &str = include_str!("../../../../compat/claude/subagent-hook-sequences.json");

    /// Hook schema of a session whose hooks report subagents.
    const SCHEMA: &str = "identity-subagent-v1";

    /// Provider id the Claude state hook reports.
    const CLAUDE: &str = "claude";

    /// Provider id the Codex state hook reports.
    const CODEX: &str = "codex";

    /// Terminal rows the worker keeps: `TERMINAL_SUBAGENT_CAPACITY` in
    /// `crates/session-worker/src/server.rs`, which is private to the worker.
    const TERMINAL_HISTORY: usize = 32;

    /// One more helper stop than the terminal history holds: if every helper
    /// stop became a terminal row, the oldest real row would be evicted.
    const HELPER_STOPS: usize = TERMINAL_HISTORY + 1;

    /// Most tasks one main-`Stop` snapshot may list: `MAX_SNAPSHOT_TASKS` of
    /// the Claude state hook and the worker (#883 S2 bounds).
    const SNAPSHOT_TASK_BOUND: usize = 256;

    /// Input bound of the Claude state hook's `stop` action:
    /// `MAX_STOP_HOOK_INPUT_BYTES` of `pohunek-agent-state.sh`. The main `Stop`
    /// input carries the final assistant message, so it gets a larger bound
    /// than every other action.
    const STOP_INPUT_BOUND: usize = 1_048_576;

    /// Input bound of every other Claude state hook action and of every Codex
    /// state hook action: `MAX_HOOK_INPUT_BYTES` of both
    /// `pohunek-agent-state.sh` assets.
    const OTHER_INPUT_BOUND: usize = 65_536;

    /// A final assistant message longer than [`OTHER_INPUT_BOUND`] and well
    /// within [`STOP_INPUT_BOUND`]: a valid `Stop` input with it must still
    /// yield a snapshot.
    const LONG_MESSAGE_BYTES: usize = OTHER_INPUT_BOUND * 4;

    /// Rows keyed by provider and subagent id.
    type Rows = BTreeMap<(String, String), SubagentPhase>;

    /// The recorded sequences file.
    #[derive(Debug, Deserialize)]
    struct Recording {
        sequences: BTreeMap<String, Sequence>,
    }

    /// One recorded sequence: hook events in order and the rows they leave.
    #[derive(Debug, Deserialize)]
    struct Sequence {
        events: Vec<Event>,
        checkpoints: Vec<Checkpoint>,
        final_rows: Vec<Row>,
    }

    /// A sanitized provider hook input, named by its `hook_event_name`.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "hook_event_name")]
    enum Event {
        SubagentStart(SubagentEvent),
        SubagentStop(SubagentEvent),
        /// A main-agent turn end with Claude's live background tasks.
        Stop {
            background_tasks: Vec<Value>,
        },
    }

    impl Event {
        /// The subagent a `SubagentStart` or `SubagentStop` names.
        fn subagent(&self) -> &SubagentEvent {
            match self {
                Self::SubagentStart(subagent) | Self::SubagentStop(subagent) => subagent,
                Self::Stop { .. } => panic!("a main-agent Stop names no subagent: {self:?}"),
            }
        }
    }

    /// The subagent fields of a `SubagentStart` or `SubagentStop` input.
    #[derive(Debug, Deserialize)]
    struct SubagentEvent {
        agent_id: String,
        agent_type: String,
    }

    /// Rows expected after the first `after_events` events.
    #[derive(Debug, Deserialize)]
    struct Checkpoint {
        after_events: usize,
        rows: Vec<Row>,
    }

    /// One expected Claude subagent row.
    #[derive(Debug, Deserialize)]
    struct Row {
        agent_id: String,
        lifecycle: SubagentPhase,
    }

    /// One step of the PTY script.
    enum Step {
        /// Runs a state hook `asset` with `action`, `payload` on its stdin.
        Hook {
            asset: PathBuf,
            action: &'static str,
            payload: Value,
        },
        /// Runs a shell command, such as a nested hook or the crafted-claim
        /// client.
        Command(String),
        /// Writes the checkpoint's marker and waits on the gate; the test checks
        /// the worker's state before it releases the gate.
        Checkpoint(Rows),
        /// Runs `steps` in one child shell of the PTY root, which is one agent
        /// process: its hooks report the same identity, and a checkpoint inside
        /// it pauses that same process.
        Process(Vec<Step>),
        /// Runs `steps` in a shell whose parent never reaps it, so the shell is a
        /// zombie once it exits. A launcher process holds it: it waits for the exit
        /// without reaping (`waitid` with `WNOWAIT`), writes the shell's state to
        /// `process-N.state` (N numbers the processes of the replay), and sleeps.
        #[cfg(target_os = "linux")]
        Background(Vec<Step>),
        /// Waits at the final state without recording a checkpoint. A replay
        /// that ends in a process renders its final barrier inside that process,
        /// so the agent is still alive when the test reads the final state.
        Barrier,
    }

    /// A real worker whose PTY script waits on a gate at each checkpoint.
    struct HookRun {
        worker: Worker,
        task: tokio::task::JoinHandle<Result<(), pohunek_session_worker::WorkerError>>,
        /// Write end of the FIFO the PTY script reads one line from per pause.
        gate: std::fs::File,
        /// The scenario directory, where each checkpoint writes its marker.
        root: PathBuf,
        /// Checkpoints whose state the test has already read.
        passed: usize,
        /// The worker's snapshot at each checkpoint, then after the last step.
        snapshots: Vec<InspectSnapshot>,
    }

    impl HookRun {
        /// Waits until the PTY script reaches the next checkpoint and returns
        /// the worker's snapshot there.
        ///
        /// A checkpoint's marker is written only after the hooks before it have
        /// run to completion. Hook completion alone does not prove that the
        /// worker answered: a hook can time out while its claim is still being
        /// committed. Callers inspect the journaled state separately.
        async fn pass_barrier(&mut self) -> InspectSnapshot {
            let marker = self.root.join(format!("barrier-{}", self.passed));
            Box::pin(wait_until("the agent reaches the checkpoint", || async {
                marker.exists().then_some(())
            }))
            .await;
            self.passed += 1;
            self.worker.inspect().await.expect("inspect")
        }

        /// Lets the PTY script continue past its current pause.
        ///
        /// Waits first until the wall clock is past every row timestamp and
        /// the background observation time in `snapshot`, so a transition after
        /// the pause is stamped later than every transition before it.
        async fn release(&mut self, snapshot: &InspectSnapshot) {
            let latest = snapshot
                .subagents
                .iter()
                .map(|row| row.updated_at_ms)
                .chain(
                    snapshot
                        .background
                        .iter()
                        .map(|background| background.observed_at_ms),
                )
                .max()
                .unwrap_or_default();
            Box::pin(wait_until(
                "the wall clock passes the checkpoint's timestamps",
                || async move { (unix_ms() > u128::from(latest)).then_some(()) },
            ))
            .await;
            std::io::Write::write_all(&mut self.gate, b"next\n").expect("release the PTY script");
        }

        /// Stops the PTY, which ends the runtime.
        async fn stop_runtime(&self) {
            self.worker
                .stop(TransactionId::new("transaction-subagent-stop").expect("transaction id"))
                .await
                .expect("stop the PTY");
        }

        /// Disconnects from the worker and ends its task.
        fn close(self) {
            drop(self.worker);
            self.task.abort();
        }

        /// Stops the PTY, then the worker.
        async fn finish(self) {
            self.stop_runtime().await;
            self.close();
        }
    }

    fn unix_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after the epoch")
            .as_millis()
    }

    /// `path` as one single-quoted shell word.
    fn quoted(path: &Path) -> String {
        let text = path.display().to_string();
        assert!(!text.contains('\''), "a test path holds no quote: {text}");
        format!("'{text}'")
    }

    /// Renders the PTY script of a replay, numbering the hook inputs and the
    /// checkpoints, and collects the rows each checkpoint expects.
    struct Script<'a> {
        root: &'a Path,
        gate: PathBuf,
        inputs: usize,
        barriers: usize,
        processes: usize,
        checkpoints: Vec<Rows>,
    }

    impl Script<'_> {
        /// The command line that runs `step` in the shell it is rendered for.
        fn command(&mut self, step: Step) -> String {
            match step {
                Step::Hook {
                    asset,
                    action,
                    payload,
                } => {
                    let input = self.root.join(format!("hook-{}.json", self.inputs));
                    self.inputs += 1;
                    std::fs::write(&input, payload.to_string()).expect("write the hook input");
                    format!("/bin/sh {} {action} < {}", quoted(&asset), quoted(&input))
                }
                Step::Command(command) => command,
                Step::Process(steps) => {
                    // A script file keeps nested processes free of quoting
                    // layers: each process is one shell running its own file.
                    let index = self.processes;
                    self.processes += 1;
                    let inner: Vec<String> =
                        steps.into_iter().map(|step| self.command(step)).collect();
                    let file = self.root.join(format!("process-{index}.sh"));
                    std::fs::write(&file, format!("{}\ntrue\n", inner.join("\n")))
                        .expect("write the process script");
                    format!("/bin/sh {}", quoted(&file))
                }
                Step::Checkpoint(rows) => {
                    self.checkpoints.push(rows);
                    self.barrier()
                }
                Step::Barrier => self.barrier(),
                #[cfg(target_os = "linux")]
                Step::Background(steps) => {
                    let index = self.processes;
                    self.processes += 1;
                    let inner: Vec<String> =
                        steps.into_iter().map(|step| self.command(step)).collect();
                    let file = self.root.join(format!("process-{index}.sh"));
                    std::fs::write(&file, format!("{}\ntrue\n", inner.join("\n")))
                        .expect("write the background script");
                    let state = self.root.join(format!("process-{index}.state"));
                    let launcher = self.root.join(format!("launcher-{index}.py"));
                    std::fs::write(
                        &launcher,
                        format!(
                            "import os, subprocess, time\n\
                             owner = subprocess.Popen([\"/bin/sh\", {file:?}])\n\
                             os.waitid(os.P_PID, owner.pid, os.WEXITED | os.WNOWAIT)\n\
                             with open(\"/proc/%d/status\" % owner.pid) as status:\n\
                             \x20   zombie = any(line.startswith(\"State:\") and \"Z\" in line for line in status)\n\
                             with open({state:?}, \"w\") as handle:\n\
                             \x20   handle.write(\"zombie\" if zombie else \"other\")\n\
                             time.sleep(3600)\n",
                            file = file.display().to_string(),
                            state = state.display().to_string(),
                        ),
                    )
                    .expect("write the launcher");
                    format!("python3 -I {} &", quoted(&launcher))
                }
            }
        }

        /// Writes the marker of the next checkpoint, then waits on the gate.
        fn barrier(&mut self) -> String {
            let marker = self.root.join(format!("barrier-{}", self.barriers));
            self.barriers += 1;
            format!(
                "echo done > {}; read -r step < {}",
                quoted(&marker),
                quoted(&self.gate)
            )
        }
    }

    /// Starts a real worker whose PTY runs `steps` with the real hooks, checks
    /// the rows at every checkpoint, and checks `final_rows` after the last
    /// step. The PTY stays up until [`HookRun::finish`].
    async fn replay(root: &Path, steps: Vec<Step>, final_rows: &Rows) -> HookRun {
        replay_checking(root, steps, Some(final_rows)).await
    }

    /// [`replay`], with the rows after the last step checked only when
    /// `final_rows` is given; the caller checks the last snapshot otherwise.
    async fn replay_checking(root: &Path, steps: Vec<Step>, final_rows: Option<&Rows>) -> HookRun {
        replay_with_schema(root, steps, final_rows, SCHEMA).await
    }

    /// [`replay_checking`] for a worker initialized with the hook schema `schema`.
    async fn replay_with_schema(
        root: &Path,
        steps: Vec<Step>,
        final_rows: Option<&Rows>,
        schema: &str,
    ) -> HookRun {
        replay_launched_as(root, steps, final_rows, (schema, "claude")).await
    }

    /// [`replay_with_schema`] for a session launched as the agent `launch_agent`
    /// (the second element), whose runtime is its own base.
    async fn replay_launched_as(
        root: &Path,
        steps: Vec<Step>,
        final_rows: Option<&Rows>,
        (schema, launch_agent): (&str, &str),
    ) -> HookRun {
        let gate_path = root.join("replay.gate");
        nix::unistd::mkfifo(&gate_path, nix::sys::stat::Mode::S_IRWXU).expect("create the gate");
        // Opened read-write so the test never blocks on a reader and the
        // script's read waits for a line rather than for end of file.
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate_path)
            .expect("open the gate");

        // Every hook runs before the script's last command, so the shell never
        // execs it and the hook's parent is the shell that runs it.
        let mut script = Script {
            root,
            gate: gate_path,
            inputs: 0,
            barriers: 0,
            processes: 0,
            checkpoints: Vec::new(),
        };
        let mut steps = steps;
        let trailing_process = matches!(steps.last(), Some(Step::Process(_)));
        if let Some(Step::Process(inner)) = steps.last_mut().filter(|_| trailing_process) {
            inner.push(Step::Barrier);
        }
        let mut lines: Vec<String> = steps.into_iter().map(|step| script.command(step)).collect();
        if !trailing_process {
            lines.push(script.barrier());
        }
        let checkpoints = script.checkpoints;

        let mut launch = initialize(
            root,
            &lines.join("\n"),
            protocol::PROTOCOL_VERSION.get(),
            schema,
        );
        launch_agent.clone_into(&mut launch.launch.agent);
        launch_agent.clone_into(&mut launch.launch.agent_base);
        let (worker, task, _socket) = start_worker_initialized(root, launch).await;
        let mut run = HookRun {
            worker,
            task,
            gate,
            root: root.to_path_buf(),
            passed: 0,
            snapshots: Vec::new(),
        };
        for (index, rows) in checkpoints.iter().enumerate() {
            let snapshot = run.pass_barrier().await;
            assert_eq!(
                &journaled_rows(&snapshot),
                rows,
                "rows at checkpoint {index}: {:?}",
                snapshot.subagents
            );
            run.release(&snapshot).await;
            run.snapshots.push(snapshot);
        }
        let snapshot = run.pass_barrier().await;
        let rows = journaled_rows(&snapshot);
        if let Some(final_rows) = final_rows {
            assert_eq!(
                &rows, final_rows,
                "rows after the last step: {:?}",
                snapshot.subagents
            );
        }
        run.snapshots.push(snapshot);
        run
    }

    /// The journaled rows of `snapshot`, which holds one row per subagent.
    fn journaled_rows(snapshot: &InspectSnapshot) -> Rows {
        let rows: Rows = snapshot
            .subagents
            .iter()
            .map(|row| ((row.provider.clone(), row.id.clone()), row.phase))
            .collect();
        assert_eq!(
            rows.len(),
            snapshot.subagents.len(),
            "one row per subagent: {:?}",
            snapshot.subagents
        );
        rows
    }

    /// The row of subagent `id` in `snapshot`.
    fn row<'a>(snapshot: &'a InspectSnapshot, id: &str) -> &'a SubagentSnapshot {
        snapshot
            .subagents
            .iter()
            .find(|row| row.id == id)
            .unwrap_or_else(|| panic!("no row {id}: {:?}", snapshot.subagents))
    }

    /// The recorded sequence `name`.
    fn recorded(name: &str) -> Sequence {
        let mut recording: Recording =
            serde_json::from_str(RECORDED).expect("the recorded sequences parse");
        recording
            .sequences
            .remove(name)
            .unwrap_or_else(|| panic!("no recorded sequence {name}"))
    }

    /// Recorded Claude rows.
    fn claude_rows(rows: &[Row]) -> Rows {
        rows.iter()
            .map(|row| ((CLAUDE.to_owned(), row.agent_id.clone()), row.lifecycle))
            .collect()
    }

    /// `event` delivered through the real Claude state hook.
    ///
    /// A main-agent `Stop` goes to the hook's `stop` action with every
    /// recorded task field, including the ones the hook must drop.
    fn claude_hook(event: &Event) -> Step {
        let (action, payload) = match event {
            Event::SubagentStart(subagent) => (
                "subagent-start",
                json!({
                    "hook_event_name": "SubagentStart",
                    "agent_id": subagent.agent_id,
                    "agent_type": subagent.agent_type,
                }),
            ),
            Event::SubagentStop(subagent) => (
                "subagent-stop",
                json!({
                    "hook_event_name": "SubagentStop",
                    "agent_id": subagent.agent_id,
                    "agent_type": subagent.agent_type,
                }),
            ),
            Event::Stop { background_tasks } => (
                "stop",
                json!({"hook_event_name": "Stop", "background_tasks": background_tasks}),
            ),
        };
        Step::Hook {
            asset: state_asset(CLAUDE),
            action,
            payload,
        }
    }

    /// `sequence` replayed through the real Claude state hook, with a
    /// checkpoint wherever the recording expects rows between two events.
    fn recorded_steps(sequence: &Sequence) -> Vec<Step> {
        assert!(
            sequence
                .checkpoints
                .iter()
                .all(|checkpoint| (1..sequence.events.len()).contains(&checkpoint.after_events)),
            "every checkpoint sits between two events: {sequence:?}"
        );
        let mut steps = Vec::new();
        for (index, event) in sequence.events.iter().enumerate() {
            steps.extend(
                sequence
                    .checkpoints
                    .iter()
                    .filter(|checkpoint| checkpoint.after_events == index)
                    .map(|checkpoint| Step::Checkpoint(claude_rows(&checkpoint.rows))),
            );
            steps.push(claude_hook(event));
        }
        steps
    }

    /// Replays the recorded sequence `name` and checks every row it expects.
    async fn replay_recorded(prefix: &str, name: &str) -> HookRun {
        let root = crate::test_support::scoped_dir(prefix);
        let sequence = recorded(name);
        replay(
            &root,
            recorded_steps(&sequence),
            &claude_rows(&sequence.final_rows),
        )
        .await
    }

    /// R1: a `SendMessage` resume fires `SubagentStart` again for a
    /// handed-back agent. Its terminal row reopens with a newer revision, keeps
    /// its first start time and clears its finish time, and the resumed run's
    /// stop completes it again (#883 D1, D6).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_resumed_subagent_reopens_its_row_and_completes_again() {
        let sequence = recorded("spawn_handback_resume");
        // The recording resumes the first agent it spawned.
        let resumed = sequence.events[0].subagent().agent_id.clone();
        let run = replay_recorded("ph-subagent-resume-", "spawn_handback_resume").await;

        let [handed_back, reopened, completed] = run.snapshots.as_slice() else {
            panic!("two checkpoints and the final rows: {:?}", run.snapshots);
        };
        let (handed_back, reopened, completed) = (
            row(handed_back, &resumed),
            row(reopened, &resumed),
            row(completed, &resumed),
        );
        assert!(
            reopened.revision > handed_back.revision,
            "{handed_back:?} {reopened:?}"
        );
        assert_eq!(
            reopened.started_at_ms, handed_back.started_at_ms,
            "the reopened row keeps its first start"
        );
        assert_eq!(reopened.finished_at_ms, None, "{reopened:?}");
        assert!(
            reopened.updated_at_ms > handed_back.updated_at_ms,
            "the reopen is a later transition: {handed_back:?} {reopened:?}"
        );
        assert!(
            completed.revision > reopened.revision,
            "{reopened:?} {completed:?}"
        );
        assert!(completed.finished_at_ms.is_some(), "{completed:?}");
        assert_eq!(completed.started_at_ms, handed_back.started_at_ms);
        run.finish().await;
    }

    /// R2: an agent that handed back while its own background shell still ran
    /// is woken by Claude when the shell finishes, with the same id; the row
    /// is running again and completes at the woken run's stop (#883 D1, D6).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_self_woken_subagent_reopens_its_row() {
        let woken_id = recorded("self_wake").events[0].subagent().agent_id.clone();
        let run = replay_recorded("ph-subagent-wake-", "self_wake").await;
        let [handed_back, woken, completed] = run.snapshots.as_slice() else {
            panic!("two checkpoints and the final rows: {:?}", run.snapshots);
        };
        let [handed_back, woken, completed] =
            [handed_back, woken, completed].map(|snapshot| row(snapshot, &woken_id));
        assert!(
            handed_back.revision < woken.revision && woken.revision < completed.revision,
            "{handed_back:?} {woken:?} {completed:?}"
        );
        assert_eq!(woken.started_at_ms, handed_back.started_at_ms);
        run.finish().await;
    }

    /// The live session's `ab6095d32d09fd8cd`: three resumes after four
    /// completions, with a prompt-suggestion helper stop during the last run.
    /// Every resumed run is tracked as running and every completion applies
    /// (#883 D1, D2, D6).
    #[tokio::test(flavor = "multi_thread")]
    async fn every_run_of_a_repeatedly_resumed_subagent_is_tracked() {
        replay_recorded("ph-subagent-live-", "live_resumed_background_agent")
            .await
            .finish()
            .await;
    }

    /// R3: the stop-only helper after every main turn creates no row and does
    /// not change the row count around a real background agent (#883 D2, D6).
    #[tokio::test(flavor = "multi_thread")]
    async fn helper_stops_after_each_turn_create_no_row() {
        replay_recorded("ph-subagent-helper-", "helper_stop_after_each_turn")
            .await
            .finish()
            .await;
    }

    /// More helper stops than the terminal history holds, after a real
    /// subagent completed, leave that real row in place (#883 D2: helper stops
    /// do not evict terminal history). The recorded R3 sequence has three
    /// helper stops, too few for any eviction to show.
    #[tokio::test(flavor = "multi_thread")]
    async fn helper_stops_do_not_evict_terminal_history() {
        let root = crate::test_support::scoped_dir("ph-subagent-evict-");
        let sequence = recorded("helper_stop_after_each_turn");
        let final_rows = claude_rows(&sequence.final_rows);
        // The recorded real agent's start and stop, without the recorded
        // helper stops, so the real row is the only terminal row before the
        // generated helper stops.
        let mut steps: Vec<Step> = sequence
            .events
            .iter()
            .filter(|event| !event.subagent().agent_type.is_empty())
            .map(claude_hook)
            .collect();
        steps.push(Step::Checkpoint(final_rows.clone()));
        // Fresh ids of the recorded helper shape: `a` and 16 hex digits.
        steps.extend((0..HELPER_STOPS).map(|index| {
            claude_hook(&Event::SubagentStop(SubagentEvent {
                agent_id: format!("af{index:015x}"),
                agent_type: String::new(),
            }))
        }));
        replay(&root, steps, &final_rows).await.finish().await;
    }

    /// A start for a subagent that is already running is idempotent: one row,
    /// its revision unchanged (#883 D1).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repeated_start_of_a_running_subagent_is_idempotent() {
        let root = crate::test_support::scoped_dir("ph-subagent-repeat-");
        let sequence = recorded("spawn_handback_resume");
        let start = &sequence.events[0];
        let running = claude_rows(&[Row {
            agent_id: start.subagent().agent_id.clone(),
            lifecycle: SubagentPhase::Running,
        }]);
        let steps = vec![
            claude_hook(start),
            Step::Checkpoint(running.clone()),
            claude_hook(start),
        ];
        let run = replay(&root, steps, &running).await;
        let [first, repeated] = run.snapshots.as_slice() else {
            panic!("one checkpoint and the final rows: {:?}", run.snapshots);
        };
        assert_eq!(
            row(repeated, &start.subagent().agent_id).revision,
            row(first, &start.subagent().agent_id).revision
        );
        run.finish().await;
    }

    /// A Codex `SubagentStop` without a recorded start is a resumed Codex
    /// child, so it records a completed row (#883 D2 Codex rule, #916).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_codex_stop_without_a_recorded_start_records_a_completed_row() {
        let root = crate::test_support::scoped_dir("ph-subagent-codex-");
        let fixture: Value =
            serde_json::from_str(crate::integration::CODEX_SUBAGENT_COMPATIBILITY_FIXTURE)
                .expect("the Codex hook fixture parses");
        let stop = fixture["stop"].clone();
        let id = stop["agent_id"]
            .as_str()
            .expect("the Codex stop names its agent")
            .to_owned();
        let steps = vec![Step::Hook {
            asset: state_asset(CODEX),
            action: "subagent-stop",
            payload: stop,
        }];
        let completed = Rows::from([((CODEX.to_owned(), id), SubagentPhase::Completed)]);
        replay(&root, steps, &completed).await.finish().await;
    }

    /// Codex 0.164 hook sequences captured from the real `codex`
    /// (`compat/codex/subagent-hook-sequences.json`, #916).
    const CODEX_RECORDED: &str =
        include_str!("../../../../compat/codex/subagent-hook-sequences.json");

    /// The id the Codex recordings give their one child.
    const CODEX_CHILD: &str = "agent-child-1";

    /// The subagent lifecycle events of the recorded Codex flow `name`: the
    /// names of its `SubagentStart` and `SubagentStop` inputs in order.
    fn codex_lifecycle(name: &str) -> Vec<String> {
        let recording: Value =
            serde_json::from_str(CODEX_RECORDED).expect("the Codex sequences parse");
        recording["sequences"][name]["events"]
            .as_array()
            .unwrap_or_else(|| panic!("no recorded Codex flow {name}"))
            .iter()
            .map(|event| {
                assert_eq!(event["agent_id"], CODEX_CHILD, "{name}: one recorded child");
                event["hook_event_name"]
                    .as_str()
                    .expect("event name")
                    .to_owned()
            })
            .collect()
    }

    /// `event` (`SubagentStart` or `SubagentStop`) delivered through the real
    /// Codex state hook, with the Codex hook input of the compatibility fixture
    /// naming `id`.
    fn codex_hook(event: &str, id: &str) -> Step {
        let fixture: Value =
            serde_json::from_str(crate::integration::CODEX_SUBAGENT_COMPATIBILITY_FIXTURE)
                .expect("the Codex hook fixture parses");
        let (action, key) = match event {
            "SubagentStart" => ("subagent-start", "start"),
            "SubagentStop" => ("subagent-stop", "stop"),
            other => panic!("not a subagent lifecycle event: {other}"),
        };
        let mut payload = fixture[key].clone();
        payload["agent_id"] = json!(id);
        Step::Hook {
            asset: state_asset(CODEX),
            action,
            payload,
        }
    }

    /// The one Codex child `id` completed.
    fn codex_completed(id: &str) -> Rows {
        Rows::from([((CODEX.to_owned(), id.to_owned()), SubagentPhase::Completed)])
    }

    /// Replays the lifecycle of the recorded Codex flow `name` through the real
    /// state hook in a session launched as `launch_agent`, pausing after the
    /// first stop, and checks the second stop records one more completed turn:
    /// a higher revision, the stop's time as the update and finish time, and
    /// the first start unchanged.
    async fn a_second_codex_stop_is_another_completed_turn(name: &str, launch_agent: &str) {
        let events = codex_lifecycle(name);
        assert_eq!(
            events,
            ["SubagentStart", "SubagentStop", "SubagentStop"],
            "{name}: the recording is a start and two completed turns"
        );
        let root = crate::test_support::scoped_dir("ph-subagent-codex-turn-");
        let steps = vec![
            codex_hook(&events[0], CODEX_CHILD),
            codex_hook(&events[1], CODEX_CHILD),
            Step::Checkpoint(codex_completed(CODEX_CHILD)),
            codex_hook(&events[2], CODEX_CHILD),
        ];
        let run = replay_launched_as(
            &root,
            steps,
            Some(&codex_completed(CODEX_CHILD)),
            (SCHEMA, launch_agent),
        )
        .await;
        let [first_turn, second_turn] = run.snapshots.as_slice() else {
            panic!("one checkpoint and the final rows: {:?}", run.snapshots);
        };
        let (first_turn, second_turn) =
            (row(first_turn, CODEX_CHILD), row(second_turn, CODEX_CHILD));
        assert!(
            second_turn.revision > first_turn.revision,
            "{name}: the second turn is a new revision: {first_turn:?} {second_turn:?}"
        );
        assert_eq!(
            second_turn.started_at_ms, first_turn.started_at_ms,
            "{name}: the row keeps its first start"
        );
        assert!(
            second_turn.updated_at_ms > first_turn.updated_at_ms,
            "{name}: the second stop's arrival time is the update time: {first_turn:?} {second_turn:?}"
        );
        assert!(
            second_turn.finished_at_ms > first_turn.finished_at_ms,
            "{name}: the second turn finishes later than the first: {first_turn:?} {second_turn:?}"
        );
        assert_eq!(
            second_turn.finished_at_ms,
            Some(second_turn.updated_at_ms),
            "{name}: the second turn finishes when it is recorded: {second_turn:?}"
        );
        run.finish().await;
    }

    /// A Codex child that finished its first turn and is given a follow-up
    /// task (`send_input`, or `followup_task` of `multi_agent_v2`) fires a
    /// second `SubagentStop` and no second start. The stop is a completed turn
    /// of the child, so its row takes a new revision and finish time (#916 D1).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_codex_followup_turn_is_recorded_as_another_completed_turn() {
        for name in ["followup_v1", "followup_v2", "resume_after_close"] {
            a_second_codex_stop_is_another_completed_turn(name, "claude").await;
        }
    }

    /// The turn rule belongs to the reporting provider, so a session launched
    /// as a plain shell that hosts Codex in the foreground gets it too (#916 D1).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_shell_session_hosting_codex_records_its_followup_turns() {
        a_second_codex_stop_is_another_completed_turn("followup_v1", "shell").await;
    }

    /// A resumed Codex child fires no start. Its first stop records a completed
    /// row and its next turn's stop is another completed turn of that row, with
    /// a new revision (#916 D1).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_resumed_codex_child_records_each_turn_that_has_no_start() {
        let root = crate::test_support::scoped_dir("ph-subagent-codex-resumed-");
        let steps = vec![
            codex_hook("SubagentStop", CODEX_CHILD),
            Step::Checkpoint(codex_completed(CODEX_CHILD)),
            codex_hook("SubagentStop", CODEX_CHILD),
        ];
        let run = replay(&root, steps, &codex_completed(CODEX_CHILD)).await;
        let [first_turn, second_turn] = run.snapshots.as_slice() else {
            panic!("one checkpoint and the final rows: {:?}", run.snapshots);
        };
        let (first_turn, second_turn) =
            (row(first_turn, CODEX_CHILD), row(second_turn, CODEX_CHILD));
        assert!(
            second_turn.revision > first_turn.revision,
            "{first_turn:?} {second_turn:?}"
        );
        assert!(
            second_turn.updated_at_ms > first_turn.updated_at_ms,
            "the second stop's arrival time is the update time: {first_turn:?} {second_turn:?}"
        );
        assert_eq!(
            second_turn.finished_at_ms,
            Some(second_turn.updated_at_ms),
            "{second_turn:?}"
        );
        run.finish().await;
    }

    /// The one Codex child `id` running.
    fn codex_running(id: &str) -> Rows {
        Rows::from([((CODEX.to_owned(), id.to_owned()), SubagentPhase::Running)])
    }

    /// A Codex child whose turn was interrupted fires no stop for it, and
    /// neither does a child closed while it runs (`interrupt_agent`,
    /// `send_input` with `interrupt`, `close_agent`). Its row stays running
    /// until the runtime ends, which retires it as lost (#916 D2).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_codex_child_without_a_stop_stays_running_until_its_runtime_ends() {
        for name in ["interrupted_v2", "close_running_v1"] {
            let events = codex_lifecycle(name);
            assert_eq!(events, ["SubagentStart"], "{name}: a start and no stop");
            let root = crate::test_support::scoped_dir("ph-subagent-codex-unstopped-");
            let run = replay(
                &root,
                vec![codex_hook(&events[0], CODEX_CHILD)],
                &codex_running(CODEX_CHILD),
            )
            .await;

            run.stop_runtime().await;
            let worker = &run.worker;
            let snapshot = Box::pin(wait_until(
                "the runtime end settles every running row",
                || async move {
                    let snapshot = worker.inspect().await.expect("inspect");
                    snapshot
                        .subagents
                        .iter()
                        .all(|row| row.phase != SubagentPhase::Running)
                        .then_some(snapshot)
                },
            ))
            .await;
            let lost = Rows::from([(
                (CODEX.to_owned(), CODEX_CHILD.to_owned()),
                SubagentPhase::Lost,
            )]);
            assert_eq!(
                journaled_rows(&snapshot),
                lost,
                "{name}: {:?}",
                snapshot.subagents
            );
            run.close();
        }
    }

    /// An interrupted first turn leaves its child running; the stop of the
    /// turn that follows completes it (#916 D2).
    #[tokio::test(flavor = "multi_thread")]
    async fn an_interrupted_codex_child_completes_at_its_later_stop() {
        let events = codex_lifecycle("interrupted_v1");
        assert_eq!(events, ["SubagentStart", "SubagentStop"]);
        let root = crate::test_support::scoped_dir("ph-subagent-codex-interrupted-");
        let steps = vec![
            codex_hook(&events[0], CODEX_CHILD),
            Step::Checkpoint(codex_running(CODEX_CHILD)),
            codex_hook(&events[1], CODEX_CHILD),
        ];
        replay(&root, steps, &codex_completed(CODEX_CHILD))
            .await
            .finish()
            .await;
    }

    /// `input` with a `padding` field that makes its JSON text exactly
    /// `bytes` long. Providers send fields the hook does not read, so the
    /// padding leaves the input valid.
    fn padded_to(input: &Value, bytes: usize) -> Value {
        let mut padded = input.clone();
        padded["padding"] = json!("");
        let unpadded = padded.to_string().len();
        padded["padding"] = json!("x".repeat(
            bytes
                .checked_sub(unpadded)
                .expect("the input is shorter than the target"),
        ));
        assert_eq!(padded.to_string().len(), bytes, "{bytes} bytes of input");
        padded
    }

    /// `provider`'s subagent start `input`, under two ids: once one byte over
    /// [`OTHER_INPUT_BOUND`], which the hook drops whole, and once exactly at
    /// the bound, which it reports.
    async fn start_at_the_input_bound(prefix: &str, provider: &str, input: &Value) {
        /// Subagent whose start input exceeds the bound.
        const OVER: &str = "a7b1c5d9e3f6a0b4c";
        /// Subagent whose start input is exactly at the bound.
        const AT: &str = "a0d4f8b2c6e9a3d7f";
        let root = crate::test_support::scoped_dir(prefix);
        let start = |id: &str, bytes: usize| {
            let mut input = input.clone();
            input["agent_id"] = json!(id);
            Step::Hook {
                asset: state_asset(provider),
                action: "subagent-start",
                payload: padded_to(&input, bytes),
            }
        };
        let steps = vec![
            start(OVER, OTHER_INPUT_BOUND + 1),
            start(AT, OTHER_INPUT_BOUND),
        ];
        let running = Rows::from([((provider.to_owned(), AT.to_owned()), SubagentPhase::Running)]);
        replay(&root, steps, &running).await.finish().await;
    }

    /// The Claude state hook bounds a `subagent-start` input at
    /// `MAX_HOOK_INPUT_BYTES`: a valid start one byte over it creates no row,
    /// one exactly at it creates a running row.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_claude_subagent_start_over_the_input_bound_creates_no_row() {
        let agent_type = recorded("spawn_handback_resume").events[0]
            .subagent()
            .agent_type
            .clone();
        let input =
            json!({"hook_event_name": "SubagentStart", "agent_id": "", "agent_type": agent_type});
        start_at_the_input_bound("ph-subagent-claude-bound-", CLAUDE, &input).await;
    }

    /// A `SubagentStart` whose `agent_type` holds a bidirectional override
    /// creates no row, while the same start with a plain type creates a
    /// running row: the type follows the label rule.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_claude_subagent_start_with_a_bidi_agent_type_creates_no_row() {
        /// Subagent whose type holds U+202E.
        const HOSTILE: &str = "b7a1c5d9e3f6a0b4d";
        /// Subagent whose type is plain.
        const PLAIN: &str = "b0d4f8b2c6e9a3d7e";
        let root = crate::test_support::scoped_dir("ph-subagent-claude-bidi-type-");
        let start = |id: &str, agent_type: &str| Step::Hook {
            asset: state_asset(CLAUDE),
            action: "subagent-start",
            payload: json!({
                "hook_event_name": "SubagentStart",
                "agent_id": id,
                "agent_type": agent_type,
            }),
        };
        let steps = vec![
            start(HOSTILE, "Explore\u{202E}evil"),
            start(PLAIN, "Explore"),
        ];
        let running = Rows::from([(
            (CLAUDE.to_owned(), PLAIN.to_owned()),
            SubagentPhase::Running,
        )]);
        replay(&root, steps, &running).await.finish().await;
    }

    /// The Codex state hook bounds a `subagent-start` input at
    /// `MAX_HOOK_INPUT_BYTES`: a valid start one byte over it creates no row,
    /// one exactly at it creates a running row.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_codex_subagent_start_over_the_input_bound_creates_no_row() {
        let fixture: Value =
            serde_json::from_str(crate::integration::CODEX_SUBAGENT_COMPATIBILITY_FIXTURE)
                .expect("the Codex hook fixture parses");
        start_at_the_input_bound("ph-subagent-codex-bound-", CODEX, &fixture["start"]).await;
    }

    /// The end of the runtime retires a running row as lost.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_subagent_is_lost_when_its_runtime_ends() {
        let root = crate::test_support::scoped_dir("ph-subagent-lost-");
        let sequence = recorded("spawn_handback_resume");
        let start = &sequence.events[0];
        let running = claude_rows(&[Row {
            agent_id: start.subagent().agent_id.clone(),
            lifecycle: SubagentPhase::Running,
        }]);
        let run = replay(&root, vec![claude_hook(start)], &running).await;

        run.stop_runtime().await;
        let worker = &run.worker;
        let snapshot = Box::pin(wait_until(
            "the runtime end settles every running row",
            || async move {
                let snapshot = worker.inspect().await.expect("inspect");
                snapshot
                    .subagents
                    .iter()
                    .all(|row| row.phase != SubagentPhase::Running)
                    .then_some(snapshot)
            },
        ))
        .await;
        let lost = claude_rows(&[Row {
            agent_id: start.subagent().agent_id.clone(),
            lifecycle: SubagentPhase::Lost,
        }]);
        assert_eq!(journaled_rows(&snapshot), lost, "{:?}", snapshot.subagents);
        run.close();
    }

    /// The background tasks of the `index`th event of `sequence`, a main-agent
    /// `Stop`.
    fn stop_tasks(sequence: &Sequence, index: usize) -> &[Value] {
        match &sequence.events[index] {
            Event::Stop { background_tasks } => background_tasks,
            other => panic!("event {index} is not a main-agent Stop: {other:?}"),
        }
    }

    /// The real Claude state hook's `stop` action reading `input` on stdin,
    /// from the file `name` under `root`.
    fn stop_hook_reading(root: &Path, name: &str, input: &[u8]) -> Step {
        let path = root.join(name);
        std::fs::write(&path, input).expect("write the Stop input");
        Step::Command(format!(
            "/bin/sh {} stop < {}",
            quoted(&state_asset(CLAUDE)),
            quoted(&path)
        ))
    }

    /// R4 and the live session: a background agent's stop never reaches the
    /// worker. The next main `Stop` omits it and an earlier one listed it
    /// during its run, so it is settled as completed; the agent the snapshot
    /// lists stays running (#883 D3, D4, D6).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_stop_is_corrected_by_the_next_main_stop() {
        let lost = recorded("lost_stop_corrected_by_snapshot").events[0]
            .subagent()
            .agent_id
            .clone();
        let run =
            replay_recorded("ph-subagent-lost-stop-", "lost_stop_corrected_by_snapshot").await;
        let [listed, _, settled] = run.snapshots.as_slice() else {
            panic!("two checkpoints and the final rows: {:?}", run.snapshots);
        };
        let (listed, settled) = (row(listed, &lost), row(settled, &lost));
        assert!(settled.revision > listed.revision, "{listed:?} {settled:?}");
        assert!(
            settled.updated_at_ms > listed.updated_at_ms,
            "the settlement is a later transition: {listed:?} {settled:?}"
        );
        assert_eq!(settled.finished_at_ms, Some(settled.updated_at_ms));
        assert_eq!(settled.started_at_ms, listed.started_at_ms);
        run.finish().await;
    }

    /// R4: a subagent that no snapshot ever listed is settled by a main `Stop`
    /// that lists only shells, because a shell runs no agent that could hold
    /// it (#883 D4).
    ///
    /// Claude builds a `Stop` listing before the hook reads its clock (S4
    /// evidence, real Claude 2.1.289 and 2.1.296), so the first such `Stop`
    /// cannot prove the subagent had started when the listing was built and
    /// leaves it running; the next one, whose predecessor is newer than the
    /// start, settles it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_never_listed_subagent_is_settled_when_only_shells_are_listed() {
        replay_recorded("ph-subagent-shells-", "never_listed_agent_with_only_shells")
            .await
            .finish()
            .await;
    }

    /// An in-process teammate is listed under a task id that differs from its
    /// agent id, so its record stays running while any teammate is listed,
    /// and is settled by the first main `Stop` that lists none (#883 D4).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_teammate_stays_running_until_no_teammate_is_listed() {
        replay_recorded("ph-subagent-teammate-", "teammate_keeps_unlisted_agents")
            .await
            .finish()
            .await;
    }

    /// A synchronous child of a listed background agent is never listed
    /// itself; it stays running beside its parent until its own stop
    /// (#883 D4).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_synchronous_child_of_a_listed_background_agent_stays_running() {
        replay_recorded("ph-subagent-sync-child-", "sync_child_of_background_agent")
            .await
            .finish()
            .await;
    }

    /// R3 helper stops interleaved with main-`Stop` snapshots create no row,
    /// and neither does a snapshot listing an agent the worker has no record
    /// of (#883 D2, D4, D6).
    #[tokio::test(flavor = "multi_thread")]
    async fn helper_stops_and_snapshots_create_no_extra_rows() {
        replay_recorded("ph-subagent-helper-snapshot-", "helper_and_snapshot")
            .await
            .finish()
            .await;
    }

    /// Main `Stop` inputs that carry no valid snapshot, one per D3 hook rule:
    /// input that is not JSON first, then the inputs commented below in
    /// order. Their only listed tasks are copies of the recorded `shell` task.
    fn invalid_stop_inputs(shell: &Value) -> Vec<Vec<u8>> {
        /// A subagent's own `Stop` input names it; Claude sends it as
        /// `SubagentStop`, never as the main agent's `Stop`.
        const OTHER_SUBAGENT: &str = "a5c9e3b7d1f4a8c2e";
        let entry_without = |field: &str| {
            let mut entry = shell.clone();
            entry
                .as_object_mut()
                .expect("a recorded task is an object")
                .remove(field)
                .unwrap_or_else(|| panic!("the recorded task has a {field}"));
            entry
        };
        let entry_with = |field: &str, value: Value| {
            let mut entry = shell.clone();
            entry[field] = value;
            entry
        };
        let stop = |tasks: Value| json!({"hook_event_name": "Stop", "background_tasks": tasks});
        let oversized = json!({
            "hook_event_name": "Stop",
            "background_tasks": [],
            "last_assistant_message": "x".repeat(STOP_INPUT_BOUND),
        });
        let inputs = [
            // An input over the stop bound.
            oversized,
            // Not an object.
            json!([stop(json!([]))]),
            // A `Stop` that names a subagent.
            json!({
                "hook_event_name": "Stop",
                "agent_id": OTHER_SUBAGENT,
                "agent_type": "general-purpose",
                "background_tasks": [],
            }),
            // No `background_tasks`.
            json!({"hook_event_name": "Stop"}),
            // `background_tasks` is not a list.
            stop(json!({"0": shell})),
            // More entries than the snapshot bound.
            stop(Value::Array(vec![shell.clone(); SNAPSHOT_TASK_BOUND + 1])),
            // An entry without a status.
            stop(json!([shell, entry_without("status"), shell])),
            // An entry with an empty id.
            stop(json!([shell, entry_with("id", json!("")), shell])),
            // An entry whose type is not a string.
            stop(json!([shell, entry_with("type", json!(7)), shell])),
            // An entry that is not an object.
            stop(json!([shell, "b9z3x7c1v", shell])),
        ]
        .map(|input| input.to_string().into_bytes());
        assert!(
            inputs[0].len() > STOP_INPUT_BOUND,
            "the first input exceeds the stop bound"
        );
        // Input that is not JSON.
        let truncated = br#"{"hook_event_name": "Stop", "background_tasks": ["#.to_vec();
        [truncated].into_iter().chain(inputs).collect()
    }

    /// A main `Stop` input without a valid task list yields no snapshot,
    /// never one that lists nothing (#883 D3).
    ///
    /// A valid empty `Stop` first leaves the never-listed subagent running,
    /// because a listing is built before the hook reads its clock (S4
    /// evidence) and no earlier `Stop` proves the subagent predates it; it
    /// gives every later `Stop` that proof. Each invalid input then lists no
    /// task that could host a subagent, so if the hook took it as a snapshot,
    /// or skipped or truncated its entries, the running subagent would be
    /// settled; a checkpoint follows each one. A valid `Stop` whose final
    /// message exceeds every other action's input bound then settles it.
    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_main_stop_inputs_change_nothing() {
        let root = crate::test_support::scoped_dir("ph-subagent-invalid-stop-");
        let sequence = recorded("never_listed_agent_with_only_shells");
        let start = &sequence.events[0];
        let invalid = invalid_stop_inputs(&stop_tasks(&sequence, 1)[0]);
        let running = claude_rows(&[Row {
            agent_id: start.subagent().agent_id.clone(),
            lifecycle: SubagentPhase::Running,
        }]);
        // Checkpoint 0 follows the start and the first valid `Stop`, and
        // checkpoint `n + 1` the `n`th input of `invalid_stop_inputs`.
        let first = json!({"hook_event_name": "Stop", "background_tasks": []});
        let mut steps = vec![
            claude_hook(start),
            stop_hook_reading(&root, "first-stop.json", first.to_string().as_bytes()),
            Step::Checkpoint(running.clone()),
        ];
        for (index, input) in invalid.iter().enumerate() {
            steps.push(stop_hook_reading(
                &root,
                &format!("invalid-stop-{index}.json"),
                input,
            ));
            steps.push(Step::Checkpoint(running.clone()));
        }
        let long = json!({
            "hook_event_name": "Stop",
            "background_tasks": [],
            "last_assistant_message": "x".repeat(LONG_MESSAGE_BYTES),
        });
        steps.push(stop_hook_reading(
            &root,
            "long-stop.json",
            long.to_string().as_bytes(),
        ));
        let completed = claude_rows(&[Row {
            agent_id: start.subagent().agent_id.clone(),
            lifecycle: SubagentPhase::Completed,
        }]);
        replay(&root, steps, &completed).await.finish().await;
    }

    /// A nested Claude's main `Stop` settles only subagents its own process
    /// started: its empty snapshots settle its own subagent and leave the
    /// outer agent's running one alone, which the outer agent's own empty
    /// snapshots then settle (#883 D4).
    ///
    /// A never-listed subagent is settled only by a `Stop` whose reporter has
    /// an earlier `Stop` newer than the subagent's start, because Claude
    /// builds a listing before the hook reads its clock (S4 evidence). Each
    /// agent therefore reports two empty `Stop`s, and the outer agent's first
    /// one leaves its subagent running: the nested agent's `Stop`s are not its
    /// own.
    ///
    /// The nested hooks run under one intermediate shell, which stands in for
    /// the nested Claude: all of its hooks report that shell, while the outer
    /// hooks report the PTY root.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_nested_agents_snapshot_settles_only_its_own_subagents() {
        /// Subagent of the outer agent, of the recorded id shape.
        const OUTER: &str = "a4c8e2b6d0f3a7c1e";
        /// Subagent of the nested agent, of the recorded id shape.
        const NESTED: &str = "a8b2d6f0c4e7a1b5d";
        let root = crate::test_support::scoped_dir("ph-subagent-reporter-");
        let asset = state_asset(CLAUDE);
        let write = |name: &str, payload: Value| {
            let path = root.join(name);
            std::fs::write(&path, payload.to_string()).expect("write the hook input");
            path
        };
        let nested_start = write(
            "nested-start.json",
            json!({"hook_event_name": "SubagentStart", "agent_id": NESTED, "agent_type": "Explore"}),
        );
        let empty_stop = write(
            "empty-stop.json",
            json!({"hook_event_name": "Stop", "background_tasks": []}),
        );
        let nested = format!(
            "/bin/sh -c '/bin/sh \"$0\" subagent-start < \"$1\"; /bin/sh \"$0\" stop < \"$2\"; /bin/sh \"$0\" stop < \"$2\"; true' {} {} {}",
            quoted(&asset),
            quoted(&nested_start),
            quoted(&empty_stop)
        );
        let outer_stop = format!("/bin/sh {} stop < {}", quoted(&asset), quoted(&empty_stop));
        let rows = |outer: SubagentPhase| {
            Rows::from([
                ((CLAUDE.to_owned(), OUTER.to_owned()), outer),
                (
                    (CLAUDE.to_owned(), NESTED.to_owned()),
                    SubagentPhase::Completed,
                ),
            ])
        };
        let steps = vec![
            claude_hook(&Event::SubagentStart(SubagentEvent {
                agent_id: OUTER.to_owned(),
                agent_type: "general-purpose".to_owned(),
            })),
            Step::Command(nested),
            Step::Checkpoint(rows(SubagentPhase::Running)),
            Step::Command(outer_stop.clone()),
            Step::Checkpoint(rows(SubagentPhase::Running)),
            Step::Command(outer_stop),
        ];
        replay(&root, steps, &rows(SubagentPhase::Completed))
            .await
            .finish()
            .await;
    }

    /// Worker instance id [`stop_hook_claims`] launches the hook with.
    const SCRIPTED_INSTANCE: &str = "instance-1";

    /// Runs the real Claude state hook's `stop` action outside a PTY, `input`
    /// on its stdin, against a scripted worker socket that refuses every
    /// claim and the stub daemon socket. Returns the raw claim lines the
    /// worker socket received and the daemon stub.
    async fn stop_hook_claims(input: &Value) -> (Vec<String>, DaemonStub) {
        stop_hook_claims_with(input, &[]).await
    }

    /// [`stop_hook_claims`] with `extra_env` added to the hook's environment.
    async fn stop_hook_claims_with(
        input: &Value,
        extra_env: &[(&str, &str)],
    ) -> (Vec<String>, DaemonStub) {
        let root = crate::test_support::scoped_dir("ph-snapshot-claim-");
        let notifications = crate::test_support::scoped_dir("ph-snapshot-claim-data-");
        let daemon = DaemonStub::spawn(&root, &notifications);
        let worker_socket = root.join("worker.sock");
        let listener = UnixListener::bind(&worker_socket).expect("bind scripted worker");
        let claims = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&claims);
        tokio::spawn(async move {
            while let Ok((stream, _addr)) = listener.accept().await {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                let _ = BufReader::new(read).read_line(&mut line).await;
                received.lock().expect("claims lock").push(line);
                let _ = write.write_all(b"{\"ok\":false}\n").await;
            }
        });

        let mut hook = tokio::process::Command::new("/bin/sh")
            .arg(state_asset(CLAUDE))
            .arg("stop")
            .env_clear()
            .env("PATH", crate::integration::tests::inherited_path())
            .env("TMPDIR", &*root)
            .env("POHUNEK_ENV", "1")
            .env("POHUNEK_SESSION_ID", SESSION_ID)
            .env("POHUNEK_SOCKET_PATH", root.join("daemon.sock"))
            .env(
                "POHUNEK_PROTOCOL_VERSION",
                protocol::PROTOCOL_VERSION.get().to_string(),
            )
            .env("POHUNEK_WORKER_SOCKET_PATH", &worker_socket)
            .env("POHUNEK_WORKER_INSTANCE_ID", SCRIPTED_INSTANCE)
            .env(
                "POHUNEK_BACKGROUND_LABEL_MAX_BYTES",
                pohunek_worker_protocol::BACKGROUND_LABEL_MAX_BYTES.to_string(),
            )
            .env(
                "POHUNEK_BACKGROUND_PROGRAM_MAX_BYTES",
                pohunek_worker_protocol::BACKGROUND_PROGRAM_MAX_BYTES.to_string(),
            )
            .envs(extra_env.iter().copied())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("run the state hook");
        let mut stdin = hook.stdin.take().expect("the hook's stdin");
        stdin
            .write_all(input.to_string().as_bytes())
            .await
            .expect("write the Stop input");
        drop(stdin);
        let status = hook.wait().await.expect("wait for the state hook");
        assert!(status.success(), "the hook exits 0: {status}");
        let claims = claims.lock().expect("claims lock").clone();
        (claims, daemon)
    }

    /// Environment that makes the hook name the provider session id as the
    /// native reference, as a Claude launch does.
    const ID_REFERENCE: [(&str, &str); 1] = [("POHUNEK_NATIVE_REFERENCE_KIND", "id")];

    fn claim_types(claims: &[String]) -> Vec<String> {
        claims
            .iter()
            .map(|line| {
                let claim: Value = serde_json::from_str(line).expect("a JSON claim");
                claim["type"].as_str().expect("a claim type").to_owned()
            })
            .collect()
    }

    /// The end of a turn repeats the identity report before its snapshot, from
    /// the session id and transcript path of the Stop input, so a `SessionStart`
    /// report the agent killed is recovered by the next turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_stop_hook_reports_the_identity_before_the_snapshot() {
        let input = json!({
            "hook_event_name": "Stop",
            "session_id": "native-recovered",
            "transcript_path": "/work/transcript.jsonl",
            "background_tasks": [],
        });
        let (claims, _daemon) = stop_hook_claims_with(&input, &ID_REFERENCE).await;

        assert_eq!(
            claim_types(&claims),
            ["identity_report", "subagent_snapshot"],
            "{claims:?}"
        );
        let report: Value = serde_json::from_str(&claims[0]).expect("the identity claim");
        assert_eq!(report["reference_kind"], "id", "{report}");
        assert_eq!(report["native_reference"], "native-recovered", "{report}");
        assert_eq!(report["provider"], "claude", "{report}");
        assert!(report.get("session_source").is_none(), "{report}");
        // The snapshot is ordered after the identity report, whose acceptance
        // clears the snapshot of a previous owner at the worker's clock.
        let snapshot: Value = serde_json::from_str(&claims[1]).expect("the snapshot claim");
        assert!(
            snapshot["sequence"].as_u64() > report["sequence"].as_u64(),
            "{report} {snapshot}"
        );
    }

    /// A Stop whose task list cannot be used still reports the identity, and a
    /// Stop without a session id sends the snapshot alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_stop_identity_report_does_not_depend_on_the_snapshot() {
        let unusable = json!({
            "hook_event_name": "Stop",
            "session_id": "native-recovered",
            "background_tasks": "not a list",
        });
        let (claims, _daemon) = stop_hook_claims_with(&unusable, &ID_REFERENCE).await;
        assert_eq!(claim_types(&claims), ["identity_report"], "{claims:?}");

        let anonymous = json!({"hook_event_name": "Stop", "background_tasks": []});
        let (claims, _daemon) = stop_hook_claims_with(&anonymous, &ID_REFERENCE).await;
        assert_eq!(claim_types(&claims), ["subagent_snapshot"], "{claims:?}");
    }

    /// The `SessionStart` hook with the worker socket absent and the hook trace
    /// under `root`, reading its input from `stdin`.
    fn session_hook_command(root: &Path, stdin: std::fs::File) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg(state_asset(CLAUDE))
            .arg("session")
            .env_clear()
            .env("PATH", crate::integration::tests::inherited_path())
            .env("TMPDIR", root)
            .env("POHUNEK_ENV", "1")
            .env("POHUNEK_SESSION_ID", SESSION_ID)
            .env(
                "POHUNEK_WORKER_SOCKET_PATH",
                root.join("absent-worker.sock"),
            )
            .env("POHUNEK_WORKER_INSTANCE_ID", SCRIPTED_INSTANCE)
            .env("POHUNEK_HOOK_TRACE_PATH", super::hook_trace_path(root))
            .env("POHUNEK_HOOK_TRACE_MAX_BYTES", "65536")
            .stdin(stdin)
            .process_group(0)
            .kill_on_drop(true);
        command
    }

    /// The reasons of the `SessionStart` trace lines under `root`, in order.
    fn trace_reasons(root: &Path) -> Vec<String> {
        std::fs::read_to_string(super::hook_trace_path(root))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let entry: Value = serde_json::from_str(line).expect("a JSON trace line");
                assert_eq!(entry["action"], "session", "{entry}");
                entry["reason"].as_str().expect("a reason").to_owned()
            })
            .collect()
    }

    /// A `SessionStart` run leaves `hook_started` before it reads its input and
    /// `hook_finished` after every other reason of the run, so a run the agent
    /// kills while it works shows as started without a finish.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_start_hook_traces_its_start_and_its_finish() {
        let root = crate::test_support::scoped_dir("ph-hook-started-");

        // A run that ends normally.
        let input = root.join("session-start.json");
        std::fs::write(
            &input,
            json!({"hook_event_name": "SessionStart", "session_id": "native-1"}).to_string(),
        )
        .expect("write the SessionStart input");
        let status = session_hook_command(&root, std::fs::File::open(&input).expect("input"))
            .status()
            .await
            .expect("run the SessionStart hook");
        assert!(status.success(), "{status}");
        let reasons = trace_reasons(&root);
        assert_eq!(
            reasons.first().map(String::as_str),
            Some("hook_started"),
            "{reasons:?}"
        );
        assert_eq!(
            reasons.last().map(String::as_str),
            Some("hook_finished"),
            "{reasons:?}"
        );
        assert!(
            reasons.len() > 2,
            "the undelivered report is traced between: {reasons:?}"
        );

        // A run that blocks on its input and is killed there.
        std::fs::remove_file(super::hook_trace_path(&root)).expect("reset the trace");
        let gate = root.join("input.fifo");
        nix::unistd::mkfifo(&gate, nix::sys::stat::Mode::S_IRWXU).expect("create the input pipe");
        // Opened read-write, so the hook's read blocks instead of ending.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate)
            .expect("hold the input pipe");
        let mut hook = session_hook_command(&root, held.try_clone().expect("share the pipe"))
            .spawn()
            .expect("start the SessionStart hook");
        pohunek_test_support::wait::wait_until("the hook's start line", || async {
            (trace_reasons(&root) == ["hook_started"]).then_some(())
        })
        .await;
        // The interpreter is a child of the hook's shell, so the whole group goes.
        let group = nix::unistd::Pid::from_raw(
            i32::try_from(hook.id().expect("the hook's pid")).expect("a pid"),
        );
        nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL)
            .expect("kill the SessionStart hook");
        hook.wait().await.expect("reap the SessionStart hook");
        assert_eq!(
            trace_reasons(&root),
            ["hook_started"],
            "a killed run has a start and no finish"
        );
    }

    /// The start markers the `SessionStart` hook left beside the trace under `root`.
    fn start_markers(root: &Path) -> Vec<PathBuf> {
        let prefix = format!("{}.start.", pohunek_logging::config::HOOK_TRACE_FILE_NAME);
        let mut markers: Vec<PathBuf> = std::fs::read_dir(root)
            .expect("read the trace directory")
            .map(|entry| entry.expect("a directory entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .collect();
        markers.sort();
        markers
    }

    /// Starts the `SessionStart` hook with a `python3` stand-in that records
    /// `<name>.started` under `root` and then blocks on a pipe, so the
    /// interpreter never runs the hook. The returned file holds the pipe open.
    fn hook_with_blocked_interpreter(
        root: &Path,
        name: &str,
        trace_path: &Path,
    ) -> (tokio::process::Child, std::fs::File) {
        use std::os::unix::fs::PermissionsExt;

        let gate = root.join(format!("{name}.fifo"));
        nix::unistd::mkfifo(&gate, nix::sys::stat::Mode::S_IRWXU).expect("create the pipe");
        // Opened read-write, so the stand-in's read blocks instead of ending.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate)
            .expect("hold the pipe");
        let shim = root.join(format!("{name}-shim"));
        std::fs::create_dir(&shim).expect("create the shim directory");
        std::fs::write(
            shim.join("python3"),
            format!(
                "#!/bin/sh\n: > '{}'\nread -r _ < '{}'\n",
                root.join(format!("{name}.started")).display(),
                gate.display()
            ),
        )
        .expect("write the interpreter stand-in");
        std::fs::set_permissions(shim.join("python3"), std::fs::Permissions::from_mode(0o755))
            .expect("make the stand-in executable");
        let path = format!(
            "{}:{}",
            shim.display(),
            crate::integration::tests::inherited_path()
        );
        let hook = session_hook_command(root, std::fs::File::open("/dev/null").expect("null"))
            .env("PATH", &path)
            .env("POHUNEK_HOOK_TRACE_PATH", trace_path)
            .spawn()
            .expect("start the SessionStart hook");
        (hook, held)
    }

    /// Kills the hook's whole process group: the interpreter is a child of the
    /// hook's shell.
    async fn kill_hook_group(hook: &mut tokio::process::Child) {
        let group = nix::unistd::Pid::from_raw(
            i32::try_from(hook.id().expect("the hook's pid")).expect("a pid"),
        );
        nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL)
            .expect("kill the SessionStart hook");
        hook.wait().await.expect("reap the SessionStart hook");
    }

    /// A `SessionStart` run killed while its interpreter is still starting
    /// leaves an owner-private start marker, because the shell creates it
    /// before the interpreter starts; a run whose interpreter runs removes its
    /// marker and the markers older than an hour.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_start_hook_killed_before_its_interpreter_runs_leaves_a_marker() {
        use std::os::unix::fs::PermissionsExt;

        let root = crate::test_support::scoped_dir("ph-hook-marker-");
        let (mut hook, held) =
            hook_with_blocked_interpreter(&root, "killed", &super::hook_trace_path(&root));
        // The stand-in runs once the shell is done with the marker.
        pohunek_test_support::wait::wait_until("the interpreter stand-in", || async {
            root.join("killed.started").exists().then_some(())
        })
        .await;
        kill_hook_group(&mut hook).await;
        drop(held);

        let markers = start_markers(&root);
        assert_eq!(markers.len(), 1, "{markers:?}");
        assert_eq!(
            std::fs::read_to_string(&markers[0]).expect("read the marker"),
            format!("{SESSION_ID}\n")
        );
        let mode = std::fs::metadata(&markers[0])
            .expect("marker metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        assert!(trace_reasons(&root).is_empty(), "the interpreter never ran");

        // A run whose interpreter runs removes its own marker and an hour-old
        // one, and keeps a recent one.
        let stale = root.join(format!(
            "{}.start.stale",
            pohunek_logging::config::HOOK_TRACE_FILE_NAME
        ));
        std::fs::write(&stale, "s-old\n").expect("write the stale marker");
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .expect("open the stale marker")
            .set_modified(SystemTime::now() - std::time::Duration::from_hours(2))
            .expect("age the stale marker");
        let status = session_hook_command(&root, std::fs::File::open("/dev/null").expect("null"))
            .status()
            .await
            .expect("run the SessionStart hook");
        assert!(status.success(), "{status}");
        assert_eq!(
            start_markers(&root),
            markers,
            "only the recent marker stays"
        );
    }

    /// The shell stops creating markers at 16, so a host whose interpreter never
    /// starts does not fill the log directory.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_start_markers_are_bounded() {
        let root = crate::test_support::scoped_dir("ph-hook-marker-cap-");
        for index in 0..16 {
            std::fs::write(
                root.join(format!(
                    "{}.start.{index}",
                    pohunek_logging::config::HOOK_TRACE_FILE_NAME
                )),
                "s-1\n",
            )
            .expect("write a marker");
        }
        let before = start_markers(&root);
        let (mut hook, held) =
            hook_with_blocked_interpreter(&root, "marker-cap", &super::hook_trace_path(&root));
        // The shell ran ahead of the interpreter once the stand-in is waiting.
        pohunek_test_support::wait::wait_until("the interpreter stand-in", || async {
            root.join("marker-cap.started").exists().then_some(())
        })
        .await;
        kill_hook_group(&mut hook).await;
        drop(held);
        assert_eq!(start_markers(&root), before, "a 17th marker is not created");
    }

    /// Names planted beside the trace, a FIFO and symlinks (one to a device,
    /// one dangling), neither block the hook nor receive anything: the marker
    /// is a new file under a name the shell does not choose.
    #[tokio::test(flavor = "multi_thread")]
    async fn names_planted_beside_the_trace_do_not_receive_the_start_marker() {
        let root = crate::test_support::scoped_dir("ph-hook-marker-planted-");
        let trace = super::hook_trace_path(&root);
        let planted = |suffix: &str| {
            let mut name = trace.clone().into_os_string();
            name.push(format!(".start.{suffix}"));
            PathBuf::from(name)
        };
        nix::unistd::mkfifo(&planted("fifo"), nix::sys::stat::Mode::S_IRWXU).expect("plant a FIFO");
        std::os::unix::fs::symlink("/dev/null", planted("null")).expect("plant a device link");
        let victim = root.join("victim");
        std::os::unix::fs::symlink(&victim, planted("dangling")).expect("plant a dangling link");

        let (mut hook, held) = hook_with_blocked_interpreter(&root, "planted", &trace);
        pohunek_test_support::wait::wait_until("the interpreter stand-in", || async {
            root.join("planted.started").exists().then_some(())
        })
        .await;
        kill_hook_group(&mut hook).await;
        drop(held);

        assert!(!victim.exists(), "the dangling link was written through");
        let markers = start_markers(&root);
        assert_eq!(
            markers.len(),
            4,
            "three planted names and one marker: {markers:?}"
        );
        let fresh: Vec<_> = markers
            .iter()
            .filter(|path| {
                !path.to_string_lossy().contains(".start.fifo")
                    && !path.to_string_lossy().contains(".start.null")
                    && !path.to_string_lossy().contains(".start.dangling")
            })
            .collect();
        assert_eq!(fresh.len(), 1, "{markers:?}");
        assert_eq!(
            std::fs::read_to_string(fresh[0]).expect("read the marker"),
            format!("{SESSION_ID}\n")
        );
    }

    /// A trace in a directory the user does not own gets no marker, whatever
    /// the directory's permissions allow.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_trace_in_a_directory_of_another_owner_gets_no_start_marker() {
        let root = crate::test_support::scoped_dir("ph-hook-marker-foreign-");
        let trace = PathBuf::from(format!(
            "/dev/shm/ph-hook-foreign-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the clock")
                .as_nanos()
        ));
        let (mut hook, held) = hook_with_blocked_interpreter(&root, "foreign", &trace);
        pohunek_test_support::wait::wait_until("the interpreter stand-in", || async {
            root.join("foreign.started").exists().then_some(())
        })
        .await;
        kill_hook_group(&mut hook).await;
        drop(held);

        let prefix = format!(
            "{}.start.",
            trace
                .file_name()
                .and_then(|name| name.to_str())
                .expect("a name")
        );
        let leftovers: Vec<_> = std::fs::read_dir("/dev/shm")
            .expect("read /dev/shm")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .map(|entry| entry.path())
            .collect();
        for leftover in &leftovers {
            let _ = std::fs::remove_file(leftover);
        }
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// A run with an invalid size bound still removes its marker.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_without_a_trace_bound_removes_its_start_marker() {
        let root = crate::test_support::scoped_dir("ph-hook-marker-nobound-");
        let status = session_hook_command(&root, std::fs::File::open("/dev/null").expect("null"))
            .env("POHUNEK_HOOK_TRACE_MAX_BYTES", "0")
            .status()
            .await
            .expect("run the SessionStart hook");
        assert!(status.success(), "{status}");
        assert!(start_markers(&root).is_empty());
    }

    /// The native session id the worker holds as the session's active identity.
    async fn recorded_reference(worker: &Worker) -> Option<(String, u64)> {
        worker
            .inspect()
            .await
            .expect("inspect the worker")
            .active_identity
            .and_then(|claim| claim.native_reference.map(|id| (id, claim.sequence)))
    }

    /// Runs the real `Stop` hook with a main-agent turn end naming `session_id`.
    fn stop_with_session(asset: &Path, session_id: &str) -> String {
        format!(
            "printf '{{\"session_id\":\"{session_id}\",\"transcript_path\":\"/work/t.jsonl\",\"background_tasks\":[]}}' \
             | /bin/sh -c '/bin/sh {} stop; true'",
            asset.display()
        )
    }

    /// The #1194 scenario through a real worker and the real hook: a
    /// `SessionStart` hook the agent killed before it reported leaves the
    /// session without a native reference; the next `Stop` gives it one; a later
    /// `Stop` of the same id leaves the reference as it is and a `Stop` after a
    /// conversation switch (`/clear`) moves it to the new id.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stop_hook_gives_a_session_the_reference_its_killed_start_hook_never_reported() {
        let root = crate::test_support::scoped_dir("ph-stop-recover-");
        let asset = state_asset(CLAUDE);
        let gate = root.join("start.fifo");
        nix::unistd::mkfifo(&gate, nix::sys::stat::Mode::S_IRWXU).expect("create the pipe");
        // Opened read-write, so the killed hook's input read blocks.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate)
            .expect("hold the pipe");
        let flag = |name: &str| root.join(name);
        let wait_for = |name: &str| {
            format!(
                "while [ ! -e '{}' ]; do sleep 0.02; done; ",
                flag(name).display()
            )
        };
        let script = format!(
            concat!(
                "set -m; ",
                "/bin/sh -c '/bin/sh {asset} session; true' < '{gate}' & hook=$!; ",
                "until grep -q hook_started '{trace}' 2>/dev/null; do sleep 0.02; done; ",
                "kill -9 -- -$hook; : > '{killed}'; ",
                "{wait_first}{first}; : > '{first_done}'; ",
                "{wait_second}{second}; : > '{second_done}'; ",
                "{wait_third}{third}; : > '{third_done}'; ",
                "sleep 30"
            ),
            asset = asset.display(),
            gate = gate.display(),
            trace = super::hook_trace_path(&root).display(),
            killed = flag("killed").display(),
            wait_first = wait_for("go-first"),
            first = stop_with_session(&asset, "native-recovered"),
            first_done = flag("first-done").display(),
            wait_second = wait_for("go-second"),
            second = stop_with_session(&asset, "native-recovered"),
            second_done = flag("second-done").display(),
            wait_third = wait_for("go-third"),
            third = stop_with_session(&asset, "native-cleared"),
            third_done = flag("third-done").display(),
        );
        let mut start = initialize(
            &root,
            &script,
            protocol::PROTOCOL_VERSION.get(),
            "identity-subagent-v1",
        );
        start.launch.reference_kind = Some("id".to_owned());
        let (worker, task, _socket) = start_worker_initialized(&root, start).await;
        let release = |name: &str| std::fs::write(flag(name), b"").expect("release a step");

        // Row 2: the start hook died before it reported, so there is no reference.
        wait_until("the killed start hook", || async {
            flag("killed").exists().then_some(())
        })
        .await;
        assert_eq!(recorded_reference(&worker).await, None);
        assert_eq!(
            trace_reasons(&root),
            ["hook_started"],
            "a start without a finish"
        );

        // The next Stop gives the session its reference.
        release("go-first");
        let (reference, first_sequence) = wait_until("the recovered reference", || async {
            recorded_reference(&worker).await
        })
        .await;
        assert_eq!(reference, "native-recovered");

        // Row 1: a repeated Stop of the same id is accepted and changes nothing else.
        wait_until("the first Stop", || async {
            flag("first-done").exists().then_some(())
        })
        .await;
        release("go-second");
        wait_until("the repeated Stop", || async {
            flag("second-done").exists().then_some(())
        })
        .await;
        let (reference, second_sequence) = recorded_reference(&worker)
            .await
            .expect("the reference stays");
        assert_eq!(reference, "native-recovered");
        assert!(
            second_sequence > first_sequence,
            "the repeat is accepted as newer"
        );

        // Row 5: a Stop after a conversation switch reports the new id.
        release("go-third");
        let (reference, third_sequence) = wait_until("the switched reference", || async {
            recorded_reference(&worker)
                .await
                .filter(|(id, _)| id == "native-cleared")
        })
        .await;
        assert_eq!(reference, "native-cleared");
        assert!(third_sequence > second_sequence);

        worker
            .stop(TransactionId::new("transaction-stop-recover").expect("transaction id"))
            .await
            .expect("stop the retained PTY");
        drop(worker);
        task.abort();
    }

    /// Row 6: a `Stop` whose worker socket is missing is traced, sends nothing
    /// and exits 0.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stop_hook_without_a_worker_is_traced_and_exits_cleanly() {
        let root = crate::test_support::scoped_dir("ph-stop-no-worker-");
        let mut input = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "printf '%s' '{{\"session_id\":\"native-1\",\"background_tasks\":[]}}' | /bin/sh {} stop",
                state_asset(CLAUDE).display()
            ))
            .env_clear()
            .env("PATH", crate::integration::tests::inherited_path())
            .env("TMPDIR", &*root)
            .env("POHUNEK_ENV", "1")
            .env("POHUNEK_SESSION_ID", SESSION_ID)
            .env("POHUNEK_WORKER_SOCKET_PATH", root.join("absent-worker.sock"))
            .env("POHUNEK_WORKER_INSTANCE_ID", SCRIPTED_INSTANCE)
            .env("POHUNEK_HOOK_TRACE_PATH", super::hook_trace_path(&root))
            .env("POHUNEK_HOOK_TRACE_MAX_BYTES", "65536")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run the Stop hook");
        let status = input.wait().expect("wait for the Stop hook");
        assert!(status.success(), "{status}");
        let output = std::fs::read_to_string(super::hook_trace_path(&root)).expect("the trace");
        assert!(output.contains("\"action\":\"stop\""), "{output}");
        assert_eq!(
            output.matches("worker_socket_missing").count(),
            2,
            "the identity report and the snapshot are each traced: {output}"
        );
    }

    /// Secret-shaped command of a background shell: a secret after a variable
    /// assignment, after a flag, and the full command line around them.
    const SECRET: &str = "sk-live-0123456789abcdef";
    const SECRET_COMMAND: &str = "API_KEY=sk-live-0123456789abcdef cd /work && env FOO=bar /usr/bin/cargo test --token=sk-live-0123456789abcdef";

    /// A background subagent listed by a main-agent turn end.
    fn subagent_task(id: &str, description: &str, agent_type: &str) -> Value {
        json!({
            "id": id,
            "type": "subagent",
            "status": "running",
            "description": description,
            "agent_type": agent_type,
        })
    }

    /// A background shell listed by a main-agent turn end.
    fn shell_task(id: &str, description: &str, command: &str) -> Value {
        json!({
            "id": id,
            "type": "shell",
            "status": "running",
            "description": description,
            "command": command,
        })
    }

    /// A background monitor listed by a main-agent turn end.
    fn monitor_task(id: &str, description: &str) -> Value {
        json!({
            "id": id,
            "type": "monitor",
            "status": "running",
            "description": description,
        })
    }

    /// The agent's session identity report from the PTY root, which the hooks
    /// that follow report as. `source` is the session start Claude reports.
    fn session_hook(source: Option<&str>) -> Step {
        let mut payload = json!({
            "hook_event_name": "SessionStart",
            "session_id": "native-background",
        });
        if let Some(source) = source {
            payload["source"] = json!(source);
        }
        Step::Hook {
            asset: state_asset(CLAUDE),
            action: "session",
            payload,
        }
    }

    /// A main-agent turn end listing `tasks`, through the real Claude hook.
    fn stop_with(tasks: Vec<Value>) -> Step {
        claude_hook(&Event::Stop {
            background_tasks: tasks,
        })
    }

    /// One agent process: `steps` run in one child shell of the PTY root, as
    /// Claude runs its hooks, so the identity they report and the claims that
    /// follow it name the same reporter.
    fn turn(steps: Vec<Step>) -> Step {
        Step::Process(steps)
    }

    /// The background snapshot of `snapshot`, which must hold one.
    fn background_of(snapshot: &InspectSnapshot) -> &BackgroundSnapshot {
        snapshot
            .background
            .as_ref()
            .unwrap_or_else(|| panic!("a background snapshot: {snapshot:?}"))
    }

    /// The item of `background` with id `id`.
    fn item<'a>(background: &'a BackgroundSnapshot, id: &str) -> &'a BackgroundItem {
        background
            .items
            .iter()
            .find(|item| item.id == id)
            .unwrap_or_else(|| panic!("no background item {id}: {:?}", background.items))
    }

    /// One event the worker logged, with every field rendered as text.
    #[derive(Debug, Clone)]
    struct LoggedEvent {
        name: String,
        fields: BTreeMap<String, String>,
    }

    /// Captures every event the worker logs on the test thread.
    ///
    /// Scenarios that use it run the worker in process on a current-thread
    /// runtime, so the worker logs on the thread that installed this capture.
    #[derive(Clone, Default)]
    struct EventLog(Arc<Mutex<Vec<LoggedEvent>>>);

    impl EventLog {
        /// Installs this capture as the thread's default subscriber, after a
        /// process-wide anchor dispatcher, as the crafted-claim capture does.
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            static ANCHOR: OnceLock<tracing::Dispatch> = OnceLock::new();
            ANCHOR.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
            tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()))
        }

        /// The fields of every logged event named `name`, in logging order.
        fn named(&self, name: &str) -> Vec<BTreeMap<String, String>> {
            self.0
                .lock()
                .expect("event log")
                .iter()
                .filter(|event| event.name == name)
                .map(|event| event.fields.clone())
                .collect()
        }

        /// Every logged event name, field name and field value, one per line:
        /// the text of every record the worker logged.
        fn rendered(&self) -> String {
            self.0
                .lock()
                .expect("event log")
                .iter()
                .flat_map(|event| {
                    std::iter::once(event.name.clone()).chain(
                        event
                            .fields
                            .iter()
                            .flat_map(|(name, value)| [name.clone(), value.clone()]),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLog {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = FieldText::default();
            event.record(&mut fields);
            self.0.lock().expect("event log").push(LoggedEvent {
                name: event.metadata().name().to_owned(),
                fields: fields.0,
            });
        }
    }

    /// Renders every field of an event as text, including its message.
    #[derive(Default)]
    struct FieldText(BTreeMap<String, String>);

    impl tracing::field::Visit for FieldText {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
    }

    /// The Stop hook forwards each task's id, type and status, with the label
    /// from its description, the program of a shell and the agent type of a
    /// subagent. Nothing else of the task, and nothing of the final message,
    /// reaches the worker socket, and a refused snapshot has no daemon fallback
    /// (#883 D3, #890 D2).
    #[tokio::test(flavor = "multi_thread")]
    async fn the_stop_hook_forwards_ids_types_statuses_and_the_sanitized_label_program_and_agent_type(
    ) {
        /// A final assistant message the claim must not carry.
        const MESSAGE: &str = "Both background tasks are still working.";
        let input = json!({
            "hook_event_name": "Stop",
            "stop_hook_active": false,
            "last_assistant_message": MESSAGE,
            "background_tasks": [
                subagent_task("sub-1", "Port the parser", "general-purpose"),
                shell_task("shell-1", "Test watch", SECRET_COMMAND),
                monitor_task("mon-1", "Watch the CI run"),
            ],
        });
        let (claims, daemon) = stop_hook_claims(&input).await;

        let [line] = claims.as_slice() else {
            panic!("one claim reaches the worker socket: {claims:?}");
        };
        let claim: Value = serde_json::from_str(line).expect("the claim is JSON");
        let keys: BTreeSet<&str> = claim
            .as_object()
            .expect("the claim is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from([
                "type",
                "runtime_id",
                "provider",
                "pid",
                "start_identity",
                "sequence",
                "tasks"
            ]),
            "{claim}"
        );
        assert_eq!(claim["type"], json!("subagent_snapshot"), "{claim}");
        assert_eq!(claim["runtime_id"], json!(SCRIPTED_INSTANCE), "{claim}");
        assert_eq!(claim["provider"], json!(CLAUDE), "{claim}");
        assert_eq!(
            claim["pid"],
            json!(std::process::id()),
            "the agent is the process that runs the hook: {claim}"
        );
        assert!(claim["start_identity"].is_u64(), "{claim}");
        assert!(
            claim["sequence"]
                .as_u64()
                .is_some_and(|sequence| sequence > 0),
            "{claim}"
        );
        assert_eq!(
            claim["tasks"],
            json!([
                {
                    "id": "sub-1",
                    "type": "subagent",
                    "status": "running",
                    "label": "Port the parser",
                    "agent_type": "general-purpose",
                },
                {
                    "id": "shell-1",
                    "type": "shell",
                    "status": "running",
                    "label": "Test watch",
                    "program": "cargo",
                },
                {
                    "id": "mon-1",
                    "type": "monitor",
                    "status": "running",
                    "label": "Watch the CI run",
                },
            ]),
            "{claim}"
        );
        for text in [
            SECRET,
            SECRET_COMMAND,
            "--token",
            "API_KEY",
            "FOO=bar",
            MESSAGE,
        ] {
            assert!(!line.contains(text), "{text:?} is forwarded: {line}");
        }
        assert!(
            daemon.request_lines().is_empty(),
            "the snapshot has no daemon fallback: {:?}",
            daemon.request_lines()
        );
    }

    /// A secret-shaped shell command reaches no record the worker writes or
    /// serves: not the journal, not the inspect response, not any worker log
    /// event, and not the hook trace. The program name, which is allowed, does.
    #[tokio::test]
    async fn a_secret_shaped_shell_command_reaches_no_record() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let root = crate::test_support::scoped_dir("ph-background-secret-");
        let steps = vec![turn(vec![
            session_hook(None),
            stop_with(vec![shell_task(
                "shell-1",
                "Run the workspace tests",
                SECRET_COMMAND,
            )]),
        ])];
        let run = replay_checking(&root, steps, None).await;
        let snapshot = run.snapshots.last().expect("the final snapshot");
        let shell = item(background_of(snapshot), "shell-1");
        assert_eq!(shell.program.as_deref(), Some("cargo"));
        assert_eq!(shell.label.as_deref(), Some("Run the workspace tests"));

        let journal =
            std::fs::read_to_string(root.join("journal/worker.json")).expect("the worker journal");
        let served = serde_json::to_string(snapshot).expect("the inspect snapshot as JSON");
        // The `Debug` output of the snapshot is what logs and debugger views
        // print, so it must keep the label and program out too.
        let rendered = format!("{snapshot:?}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
        assert!(
            !rendered.contains("Run the workspace tests"),
            "the label reaches the Debug output: {rendered}"
        );
        let logged = log.rendered();
        let hook_trace = std::fs::read_to_string(super::hook_trace_path(&root)).unwrap_or_default();
        assert!(journal.contains("cargo"), "the journal keeps the program");
        assert_eq!(log.named("worker.background.applied").len(), 1, "{logged}");
        for (sink, text) in [
            ("journal", journal.as_str()),
            ("inspect response", served.as_str()),
            ("debug output", rendered.as_str()),
            ("worker log", logged.as_str()),
            ("hook trace", hook_trace.as_str()),
        ] {
            for secret in [
                SECRET,
                "--token",
                "API_KEY",
                "FOO=bar",
                "cd /work",
                "/usr/bin/cargo",
            ] {
                assert!(
                    !text.contains(secret),
                    "{secret:?} reaches the {sink}: {text}"
                );
            }
        }
        run.finish().await;
    }

    /// A `Stop` input over the hook's bound is dropped whole, so the last
    /// background snapshot stays as it was, with the same revision and
    /// observation time, and the hook traces its refusal.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_oversized_stop_input_keeps_the_last_background_snapshot() {
        let root = crate::test_support::scoped_dir("ph-background-oversized-");
        let oversized = padded_to(
            &json!({
                "hook_event_name": "Stop",
                "background_tasks": [shell_task("shell-2", "Too long", "cargo build")],
            }),
            STOP_INPUT_BOUND + 1,
        );
        let steps = vec![turn(vec![
            session_hook(None),
            stop_with(vec![shell_task("shell-1", "Test watch", "cargo test")]),
            Step::Checkpoint(Rows::new()),
            Step::Hook {
                asset: state_asset(CLAUDE),
                action: "stop",
                payload: oversized,
            },
        ])];
        let run = replay_checking(&root, steps, None).await;
        let [before, after] = run.snapshots.as_slice() else {
            panic!("one checkpoint and the final rows: {:?}", run.snapshots);
        };
        assert_eq!(background_of(before), background_of(after));
        assert!(
            background_of(after)
                .items
                .iter()
                .all(|item| item.id != "shell-2"),
            "{after:?}"
        );
        let trace = std::fs::read_to_string(super::hook_trace_path(&root))
            .expect("the hook traced its refusal");
        assert!(trace.contains("input_too_large"), "{trace}");
        run.finish().await;
    }

    /// A turn end listing a subagent, a shell and a monitor, then a later turn
    /// end of the same agent process that drops the monitor and adds a shell:
    /// the first snapshot renders each kind with its fields, and the second keeps
    /// the first sightings of the items it still lists, under a new revision.
    /// Background items never create subagent rows.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_end_snapshot_renders_its_items_and_a_later_one_keeps_first_sightings() {
        let root = crate::test_support::scoped_dir("ph-background-snapshot-");
        let steps = vec![turn(vec![
            session_hook(None),
            stop_with(vec![
                subagent_task("sub-1", "Port the parser", "general-purpose"),
                shell_task("shell-1", "Test watch", "cargo test --workspace"),
                monitor_task("mon-1", "Watch the CI run"),
            ]),
            Step::Checkpoint(Rows::new()),
            stop_with(vec![
                subagent_task("sub-1", "Port the parser", "general-purpose"),
                shell_task("shell-1", "Test watch", "cargo test --workspace"),
                shell_task("shell-2", "Clippy watch", "bacon clippy"),
            ]),
        ])];
        let run = replay_checking(&root, steps, None).await;
        let [after_first, after_second] = run.snapshots.as_slice() else {
            panic!("one checkpoint and the final rows: {:?}", run.snapshots);
        };

        let first = background_of(after_first);
        assert_eq!(first.items.len(), 3, "{first:?}");
        let subagent = item(first, "sub-1");
        assert_eq!(subagent.kind, BackgroundKind::Subagent);
        assert_eq!(subagent.status, BackgroundStatus::Running);
        assert_eq!(subagent.label.as_deref(), Some("Port the parser"));
        assert_eq!(subagent.agent_type.as_deref(), Some("general-purpose"));
        assert_eq!(subagent.program, None);
        let shell = item(first, "shell-1");
        assert_eq!(shell.kind, BackgroundKind::Shell);
        assert_eq!(shell.label.as_deref(), Some("Test watch"));
        assert_eq!(shell.program.as_deref(), Some("cargo"));
        assert_eq!(shell.agent_type, None);
        let monitor = item(first, "mon-1");
        assert_eq!(monitor.kind, BackgroundKind::Monitor);
        assert_eq!(monitor.label.as_deref(), Some("Watch the CI run"));
        assert_eq!(monitor.program, None);
        for item in &first.items {
            assert_eq!(item.first_seen_ms, first.observed_at_ms, "{item:?}");
            assert_eq!(item.last_seen_ms, first.observed_at_ms, "{item:?}");
        }

        let second = background_of(after_second);
        assert!(second.revision > first.revision, "{first:?} {second:?}");
        assert!(
            second.observed_at_ms > first.observed_at_ms,
            "{first:?} {second:?}"
        );
        let ids: BTreeSet<&str> = second.items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, BTreeSet::from(["sub-1", "shell-1", "shell-2"]));
        assert_eq!(item(second, "sub-1").first_seen_ms, subagent.first_seen_ms);
        assert_eq!(item(second, "shell-1").first_seen_ms, shell.first_seen_ms);
        let added = item(second, "shell-2");
        assert_eq!(added.first_seen_ms, second.observed_at_ms, "{added:?}");
        assert_eq!(added.program.as_deref(), Some("bacon"));
        assert_eq!(item(second, "shell-1").last_seen_ms, second.observed_at_ms);
        assert!(
            after_second.subagents.is_empty(),
            "{:?}",
            after_second.subagents
        );
        run.finish().await;
    }

    /// Clearing follows the agent process. A compact session of the same
    /// process keeps the background, and a later snapshot of that process keeps
    /// the first sightings. A new process that starts a session clears the
    /// background whatever its source, with the reason `agent_changed`.
    #[tokio::test]
    async fn a_compact_session_keeps_the_background_and_a_new_agent_process_clears_it() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let root = crate::test_support::scoped_dir("ph-background-clear-");
        let tasks = || vec![shell_task("shell-1", "Test watch", "cargo test")];
        let steps = vec![
            turn(vec![
                session_hook(None),
                stop_with(tasks()),
                Step::Checkpoint(Rows::new()),
                session_hook(Some("compact")),
                Step::Checkpoint(Rows::new()),
                stop_with(tasks()),
                Step::Checkpoint(Rows::new()),
            ]),
            turn(vec![
                session_hook(Some("startup")),
                Step::Checkpoint(Rows::new()),
            ]),
            turn(vec![
                session_hook(None),
                stop_with(tasks()),
                Step::Checkpoint(Rows::new()),
            ]),
            turn(vec![
                session_hook(Some("resume")),
                Step::Checkpoint(Rows::new()),
            ]),
        ];
        let run = replay_checking(&root, steps, None).await;
        let [applied, compacted, reapplied, cleared, applied_again, resumed, ..] =
            run.snapshots.as_slice()
        else {
            panic!("six checkpoints and the final rows: {:?}", run.snapshots);
        };
        let first_seen = item(background_of(applied), "shell-1").first_seen_ms;

        let revision = background_of(applied).revision;
        assert_eq!(
            background_of(compacted).revision,
            revision,
            "compact keeps it"
        );
        let kept = background_of(reapplied);
        assert_eq!(kept.revision, revision + 1, "{kept:?}");
        assert_eq!(item(kept, "shell-1").first_seen_ms, first_seen, "{kept:?}");
        assert!(cleared.background.is_none(), "{:?}", cleared.background);
        let again = background_of(applied_again);
        assert_eq!(
            item(again, "shell-1").first_seen_ms,
            again.observed_at_ms,
            "{again:?}"
        );
        assert!(resumed.background.is_none(), "{:?}", resumed.background);
        let reasons: Vec<String> = log
            .named("worker.background.cleared")
            .iter()
            .map(|fields| fields["background.reason"].clone())
            .collect();
        assert_eq!(
            reasons,
            ["agent_changed", "agent_changed"],
            "{}",
            log.rendered()
        );
        run.finish().await;
    }

    /// A new agent process that starts a session with `clear` or `compact`
    /// clears the background of the previous process, with reason
    /// `agent_changed`. The new process's own snapshot then stands alone.
    #[tokio::test]
    async fn a_new_agent_process_clears_the_background_on_clear_or_compact() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let root = crate::test_support::scoped_dir("ph-background-new-process-");
        let tasks = || vec![shell_task("shell-1", "Test watch", "cargo test")];
        let steps = vec![
            turn(vec![
                session_hook(None),
                stop_with(tasks()),
                Step::Checkpoint(Rows::new()),
            ]),
            turn(vec![
                session_hook(Some("clear")),
                Step::Checkpoint(Rows::new()),
            ]),
            turn(vec![
                session_hook(None),
                stop_with(tasks()),
                Step::Checkpoint(Rows::new()),
            ]),
            turn(vec![
                session_hook(Some("compact")),
                Step::Checkpoint(Rows::new()),
            ]),
        ];
        let run = replay_checking(&root, steps, None).await;
        let [applied, cleared, reapplied, compacted, ..] = run.snapshots.as_slice() else {
            panic!("four checkpoints and the final rows: {:?}", run.snapshots);
        };
        assert!(applied.background.is_some(), "{applied:?}");
        assert!(cleared.background.is_none(), "{:?}", cleared.background);
        assert!(reapplied.background.is_some(), "{reapplied:?}");
        assert!(compacted.background.is_none(), "{:?}", compacted.background);
        let reasons: Vec<String> = log
            .named("worker.background.cleared")
            .iter()
            .map(|fields| fields["background.reason"].clone())
            .collect();
        assert_eq!(
            reasons,
            ["agent_changed", "agent_changed"],
            "{}",
            log.rendered()
        );
        run.finish().await;
    }

    /// One agent process lists a shell, checks its background, then ends its
    /// session with a release carrying `reason`. Returns the snapshots at the
    /// checkpoint and after the release.
    async fn released_background(
        prefix: &str,
        reason: Option<&str>,
    ) -> (InspectSnapshot, InspectSnapshot) {
        let root = crate::test_support::scoped_dir(prefix);
        let mut payload =
            json!({"hook_event_name": "SessionEnd", "session_id": "native-background"});
        if let Some(reason) = reason {
            payload["reason"] = json!(reason);
        }
        let steps = vec![turn(vec![
            session_hook(None),
            stop_with(vec![shell_task("shell-1", "Test watch", "cargo test")]),
            Step::Checkpoint(Rows::new()),
            Step::Hook {
                asset: state_asset(CLAUDE),
                action: "release",
                payload,
            },
        ])];
        let mut run = replay_checking(&root, steps, None).await;
        let [held, released] = std::mem::take(&mut run.snapshots)
            .try_into()
            .unwrap_or_else(|snapshots: Vec<InspectSnapshot>| {
                panic!("one checkpoint and the final rows: {snapshots:?}")
            });
        run.finish().await;
        (held, released)
    }

    /// A session release by the active agent clears the background, and the
    /// worker logs the clear with its reason.
    #[tokio::test]
    async fn a_session_release_clears_the_background() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let (held, released) = released_background("ph-background-release-", None).await;
        assert_eq!(background_of(&held).items.len(), 1, "{held:?}");
        assert!(released.background.is_none(), "{:?}", released.background);
        let cleared = log.named("worker.background.cleared");
        assert_eq!(cleared.len(), 1, "{}", log.rendered());
        assert_eq!(cleared[0]["background.reason"], "release");
    }

    /// A release for `prompt_input_exit` ends the session like any release
    /// other than `clear`, so the background is cleared.
    #[tokio::test]
    async fn a_prompt_input_exit_release_clears_the_background() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let (held, released) =
            released_background("ph-background-prompt-exit-", Some("prompt_input_exit")).await;
        assert_eq!(background_of(&held).items.len(), 1, "{held:?}");
        assert!(released.background.is_none(), "{:?}", released.background);
        assert_eq!(
            log.named("worker.background.cleared").len(),
            1,
            "{}",
            log.rendered()
        );
    }

    /// A `/clear` releases the session and starts it again in the same process,
    /// so the release and the new session keep the background. The checkpoint
    /// between them shows the release was accepted.
    #[tokio::test]
    async fn a_clear_release_and_a_clear_session_from_the_same_process_keep_the_background() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let root = crate::test_support::scoped_dir("ph-background-clear-release-");
        let steps = vec![turn(vec![
            session_hook(None),
            stop_with(vec![shell_task("shell-1", "Test watch", "cargo test")]),
            Step::Hook {
                asset: state_asset(CLAUDE),
                action: "release",
                payload: json!({
                    "hook_event_name": "SessionEnd",
                    "session_id": "native-background",
                    "reason": "clear",
                }),
            },
            Step::Checkpoint(Rows::new()),
            session_hook(Some("clear")),
            Step::Checkpoint(Rows::new()),
        ])];
        let run = replay_checking(&root, steps, None).await;
        let [released, restarted, ..] = run.snapshots.as_slice() else {
            panic!("two checkpoints and the final rows: {:?}", run.snapshots);
        };
        assert!(released.active_identity.is_none(), "{released:?}");
        assert!(released.active_identity_release.is_some(), "{released:?}");
        let kept = background_of(released);
        assert_eq!(kept.revision, 1, "{kept:?}");
        assert_eq!(background_of(restarted), kept, "{restarted:?}");
        assert!(
            log.named("worker.background.cleared").is_empty(),
            "{}",
            log.rendered()
        );
        run.finish().await;
    }

    /// A nested Claude started inside the agent reports a snapshot of its own,
    /// which replaces the agent's. Its release clears its snapshot, and the
    /// agent's next turn end restores the agent's background. The nested
    /// session start is another process, so it clears the agent's snapshot
    /// first, with reason `agent_changed`.
    #[tokio::test]
    async fn a_nested_agent_replaces_and_releases_the_background_and_the_agent_restores_it() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let root = crate::test_support::scoped_dir("ph-background-nested-");
        let steps = vec![turn(vec![
            session_hook(None),
            stop_with(vec![shell_task("outer-shell", "Outer watch", "cargo test")]),
            turn(vec![
                session_hook(None),
                stop_with(vec![shell_task("nested-shell", "Nested watch", "bacon")]),
                Step::Checkpoint(Rows::new()),
                Step::Hook {
                    asset: state_asset(CLAUDE),
                    action: "release",
                    payload: json!({"hook_event_name": "SessionEnd", "session_id": "native-background"}),
                },
            ]),
            Step::Checkpoint(Rows::new()),
            stop_with(vec![shell_task(
                "outer-again",
                "Outer again",
                "cargo build",
            )]),
            Step::Checkpoint(Rows::new()),
        ])];
        let run = replay_checking(&root, steps, None).await;
        let [nested, released, restored, ..] = run.snapshots.as_slice() else {
            panic!("three checkpoints and the final rows: {:?}", run.snapshots);
        };
        let ids = |snapshot: &InspectSnapshot| -> Vec<String> {
            background_of(snapshot)
                .items
                .iter()
                .map(|item| item.id.clone())
                .collect()
        };
        assert_eq!(ids(nested), ["nested-shell"], "{nested:?}");
        assert!(released.background.is_none(), "{released:?}");
        assert_eq!(ids(restored), ["outer-again"], "{restored:?}");
        let reasons: Vec<String> = log
            .named("worker.background.cleared")
            .iter()
            .map(|fields| fields["background.reason"].clone())
            .collect();
        assert_eq!(reasons, ["agent_changed", "release"], "{}", log.rendered());
        run.finish().await;
    }

    /// After a `clear` release, whose session start never arrives, the same
    /// process's `prompt_input_exit` release still ends the snapshot's session.
    /// The identity release is not accepted the second time, because the first
    /// one already ended the active identity.
    #[tokio::test]
    async fn a_clear_release_then_a_prompt_input_exit_release_from_the_same_process_clears() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let root = crate::test_support::scoped_dir("ph-background-clear-then-exit-");
        let release = |reason: &str| Step::Hook {
            asset: state_asset(CLAUDE),
            action: "release",
            payload: json!({
                "hook_event_name": "SessionEnd",
                "session_id": "native-background",
                "reason": reason,
            }),
        };
        let steps = vec![turn(vec![
            session_hook(None),
            stop_with(vec![shell_task("shell-1", "Test watch", "cargo test")]),
            release("clear"),
            Step::Checkpoint(Rows::new()),
            release("prompt_input_exit"),
            Step::Checkpoint(Rows::new()),
        ])];
        let run = replay_checking(&root, steps, None).await;
        let [kept, cleared, ..] = run.snapshots.as_slice() else {
            panic!("two checkpoints and the final rows: {:?}", run.snapshots);
        };
        assert_eq!(background_of(kept).items.len(), 1, "{kept:?}");
        assert!(cleared.background.is_none(), "{cleared:?}");
        let reasons: Vec<String> = log
            .named("worker.background.cleared")
            .iter()
            .map(|fields| fields["background.reason"].clone())
            .collect();
        assert_eq!(reasons, ["release"], "{}", log.rendered());
        run.finish().await;
    }

    /// A release from a process that did not report the snapshot leaves it in
    /// place. The owner's snapshot ends only with the owner's release.
    #[tokio::test]
    async fn a_release_from_another_process_keeps_the_owners_background() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let root = crate::test_support::scoped_dir("ph-background-non-owner-");
        let steps = vec![turn(vec![
            session_hook(None),
            stop_with(vec![shell_task("shell-1", "Test watch", "cargo test")]),
            turn(vec![Step::Hook {
                asset: state_asset(CLAUDE),
                action: "release",
                payload: json!({"hook_event_name": "SessionEnd", "session_id": "native-background"}),
            }]),
            Step::Checkpoint(Rows::new()),
        ])];
        let run = replay_checking(&root, steps, None).await;
        let [kept, ..] = run.snapshots.as_slice() else {
            panic!("one checkpoint and the final rows: {:?}", run.snapshots);
        };
        assert_eq!(background_of(kept).items.len(), 1, "{kept:?}");
        assert!(
            log.named("worker.background.cleared").is_empty(),
            "{}",
            log.rendered()
        );
        run.finish().await;
    }

    /// A worker on the identity-only schema refuses a Stop's background
    /// snapshot, so the session shows no background, and the hook records the
    /// refusal in its trace.
    #[tokio::test]
    async fn an_identity_only_schema_refuses_the_background_snapshot() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let root = crate::test_support::scoped_dir("ph-background-identity-only-");
        let steps = vec![turn(vec![
            session_hook(None),
            stop_with(vec![shell_task("shell-1", "Test watch", "cargo test")]),
        ])];
        let run = replay_with_schema(&root, steps, None, "identity-v1").await;
        let snapshot = run.snapshots.last().expect("the final snapshot");
        assert!(snapshot.background.is_none(), "{snapshot:?}");
        assert!(
            log.named("worker.background.applied").is_empty(),
            "{}",
            log.rendered()
        );
        let trace = std::fs::read_to_string(super::hook_trace_path(&root))
            .expect("the hook traced the refused snapshot");
        assert!(trace.contains("worker_refused"), "{trace}");
        run.finish().await;
    }

    /// An agent process that exits but is never reaped stays a zombie, and a
    /// zombie is not a running owner. The root shell is blocked on a checkpoint
    /// while the process is a zombie, so the inspect at that checkpoint finds
    /// the owner exited and clears the snapshot with reason `owner_exited`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_background_owner_left_as_a_zombie_is_cleared() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let root = crate::test_support::scoped_dir("ph-background-zombie-");
        let state = root.join("process-0.state");
        // Builtins only: the root never waits on a child, so it cannot reap the owner.
        let watch = format!("while [ ! -s {} ]; do :; done", quoted(&state));
        let steps = vec![
            Step::Background(vec![
                session_hook(None),
                stop_with(vec![shell_task("shell-1", "Test watch", "cargo test")]),
            ]),
            Step::Command(watch),
            Step::Checkpoint(Rows::new()),
        ];
        let run = replay_checking(&root, steps, None).await;
        let [after_exit, ..] = run.snapshots.as_slice() else {
            panic!("one checkpoint and the final rows: {:?}", run.snapshots);
        };
        let observed = std::fs::read_to_string(&state).expect("the launcher recorded the owner");
        assert_eq!(
            observed.trim(),
            "zombie",
            "the owner was a zombie at the checkpoint"
        );
        assert!(after_exit.background.is_none(), "{after_exit:?}");
        let reasons: Vec<String> = log
            .named("worker.background.cleared")
            .iter()
            .map(|fields| fields["background.reason"].clone())
            .collect();
        assert_eq!(reasons, ["owner_exited"], "{}", log.rendered());
        run.finish().await;
    }

    /// An agent process that ends without a release, as a killed Claude does,
    /// leaves no owner to clear its snapshot. The next inspect finds the owner
    /// gone and clears the snapshot with reason `owner_exited`.
    #[tokio::test]
    async fn a_background_owner_that_exits_without_a_release_is_cleared() {
        let log = EventLog::default();
        let _subscriber = log.install();
        let root = crate::test_support::scoped_dir("ph-background-owner-exit-");
        let steps = vec![
            turn(vec![
                session_hook(None),
                stop_with(vec![shell_task("shell-1", "Test watch", "cargo test")]),
            ]),
            Step::Checkpoint(Rows::new()),
        ];
        let run = replay_checking(&root, steps, None).await;
        let [after_exit, ..] = run.snapshots.as_slice() else {
            panic!("one checkpoint and the final rows: {:?}", run.snapshots);
        };
        assert!(after_exit.background.is_none(), "{after_exit:?}");
        let reasons: Vec<String> = log
            .named("worker.background.cleared")
            .iter()
            .map(|fields| fields["background.reason"].clone())
            .collect();
        assert_eq!(reasons, ["owner_exited"], "{}", log.rendered());
        run.finish().await;
    }

    /// Ending the runtime clears the background, as it settles the subagent rows.
    #[tokio::test(flavor = "multi_thread")]
    async fn ending_the_runtime_clears_the_background() {
        let root = crate::test_support::scoped_dir("ph-background-runtime-end-");
        let steps = vec![turn(vec![
            session_hook(None),
            stop_with(vec![shell_task("shell-1", "Test watch", "cargo test")]),
        ])];
        let run = replay_checking(&root, steps, None).await;
        let before = run.snapshots.last().expect("the final snapshot");
        assert_eq!(background_of(before).items.len(), 1, "{before:?}");

        run.stop_runtime().await;
        let worker = &run.worker;
        let after = Box::pin(wait_until(
            "the runtime end clears the background",
            || async move {
                let snapshot = worker.inspect().await.expect("inspect");
                snapshot.background.is_none().then_some(snapshot)
            },
        ))
        .await;
        assert_eq!(after.phase, RuntimePhase::Exited, "{after:?}");
        run.close();
    }

    /// Ordering scenarios driven by crafted subagent claims with explicit
    /// sequences, so that a claim can reach the worker before a claim it
    /// follows, as hooks of separate processes can.
    ///
    /// Expected rows follow the ordering invariant decided on #883: a record's
    /// phase is the phase implied by the highest-sequence claim the worker has
    /// admitted for that id.
    ///
    /// Linux only: the crafted client reads its start identity from `/proc`.
    #[cfg(target_os = "linux")]
    mod crafted {
        use std::cell::Cell;
        use std::path::{Path, PathBuf};
        use std::sync::{Arc, Mutex, OnceLock};

        use pohunek_worker_protocol::{
            BackgroundKind, SubagentPhase, BACKGROUND_LABEL_MAX_BYTES, BACKGROUND_MAX_ITEMS,
        };
        use serde_json::{json, Value};
        use tracing_subscriber::layer::{Context, SubscriberExt};

        use super::{
            background_of, claude_hook, claude_rows, item, journaled_rows, quoted, recorded,
            replay, replay_checking, row, session_hook, shell_task, state_asset, stop_with, turn,
            Event, EventLog, Row, Rows, Step, SubagentEvent, CLAUDE, CODEX, SNAPSHOT_TASK_BOUND,
            TERMINAL_HISTORY,
        };

        /// Claim type of a subagent start.
        const START: &str = "subagent_start";

        /// Claim type of a subagent stop.
        const STOP: &str = "subagent_stop";

        /// Claim type of a main-`Stop` task snapshot.
        const SNAPSHOT: &str = "subagent_snapshot";

        /// A sequence no hook of these scenarios reaches, so a claim carrying it
        /// is ordered after every real one of the session.
        const FAR_FUTURE_SEQUENCE: u64 = 1 << 62;

        /// Synthetic subagent id of the recorded Claude shape.
        const REORDERED: &str = "a1b3d5f7c9e0a2b4d";

        /// Subagents of one provider the worker runs at once:
        /// `ACTIVE_SUBAGENT_CAPACITY` in `crates/session-worker/src/server.rs`,
        /// which is private to the worker.
        const ACTIVE_CAPACITY: usize = 64;

        /// Longest snapshot task id the worker accepts: `MAX_SUBAGENT_ID_BYTES`.
        const TASK_ID_BOUND: usize = 256;

        /// Longest snapshot task type the worker accepts:
        /// `MAX_SNAPSHOT_TYPE_BYTES`.
        const TASK_TYPE_BOUND: usize = 64;

        /// Longest snapshot task status the worker accepts:
        /// `MAX_SNAPSHOT_STATUS_BYTES`.
        const TASK_STATUS_BOUND: usize = 32;

        /// Reporters whose last snapshot the worker remembers:
        /// `SNAPSHOT_REPORTER_CAPACITY` in `crates/session-worker/src/server.rs`,
        /// which is private to the worker.
        const SNAPSHOT_REPORTERS: usize = 16;

        /// Name of the worker event for a claim that changed no phase.
        const IGNORED_EVENT: &str = "worker.subagent.claim.ignored";

        /// Name of the worker event for a claim that hit a bound.
        const REFUSED_EVENT: &str = "worker.subagent.claim.refused";

        /// Name of the worker event for a running subagent a snapshot
        /// completed.
        const RECONCILED_EVENT: &str = "worker.subagent.reconciled";

        /// Name of the worker event that sums up one applied snapshot.
        const APPLIED_EVENT: &str = "worker.subagent.snapshot.applied";

        /// Crafted-claim client: `<results file> <provider> <plan file>`.
        ///
        /// The plan is a JSON list of `{"type", "sequence"}` claims, with `id`
        /// for a start or a stop and `tasks` for a snapshot, sent one after the
        /// other. Every claim names the client's parent, the PTY root that
        /// runs it, as the reporting agent, the way the real state hook names
        /// the agent that runs it; claims of separate client runs therefore
        /// share one reporter, which snapshot reconciliation is scoped to.
        ///
        /// Each answered claim appends `{"action", "sequence", "id",
        /// "response"}` to the results file, with an empty `id` for a
        /// snapshot; any failure goes to `<results file>.failure`, which the
        /// test reads.
        const CLIENT: &str = r#"
import json
import os
import socket
import sys
import time
from datetime import datetime, timedelta, timezone

results_path, provider, plan_path = sys.argv[1], sys.argv[2], sys.argv[3]

def record_failure(kind, value, trace):
    # The test reads this file, so a claim never fails without a reason.
    import traceback
    with open(results_path + ".failure", "a", encoding="utf-8") as handle:
        handle.write("".join(traceback.format_exception(kind, value, trace)))

sys.excepthook = record_failure

def start_identity(pid):
    with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
        return int(handle.read().rsplit(")", 1)[1].split()[19])

agent_pid = os.getppid()
agent_start = start_identity(agent_pid)
with open(plan_path, encoding="utf-8") as handle:
    plan = json.load(handle)

for claim in plan:
    action, sequence = claim["type"], claim["sequence"]
    if sequence == "now":
        # The worker refuses a sequence ahead of its own reading of this clock,
        # so a claim meant to be ordered after the hooks reads it at send time.
        sequence = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
    request = {
        "type": action,
        "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
        "provider": provider,
        "pid": agent_pid,
        "start_identity": agent_start,
        "sequence": sequence,
    }
    if action == "subagent_start":
        request.update({
            "subagent_id": claim["id"],
            "agent_type": claim.get("agent_type", "general-purpose"),
            "parent_id": claim.get("parent_id"),
        })
    elif action == "subagent_stop":
        request.update({"subagent_id": claim["id"], "outcome": "completed"})
    elif action == "identity_report":
        request.update({
            "expires_at": (datetime.now(timezone.utc) + timedelta(seconds=30)).isoformat().replace("+00:00", "Z"),
            "reference_kind": None,
            "native_reference": None,
        })
    else:
        request["tasks"] = claim["tasks"]
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(os.environ["POHUNEK_WORKER_SOCKET_PATH"])
    client.sendall((json.dumps(request) + "\n").encode())
    received = b""
    while b"\n" not in received:
        chunk = client.recv(4096)
        if not chunk:
            raise RuntimeError(f"worker closed the socket before a full response line: {received!r}")
        received += chunk
    client.close()
    response = json.loads(received.split(b"\n", 1)[0])
    with open(results_path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps({
            "action": action,
            "sequence": sequence,
            "id": claim.get("id", ""),
            "response": response,
        }) + "\n")
"#;

        /// One crafted claim and whether the worker acknowledged it.
        #[derive(Debug, PartialEq, Eq)]
        struct Answer {
            action: String,
            sequence: u64,
            id: String,
            ok: bool,
        }

        impl Answer {
            fn new(action: &str, sequence: u64, id: &str, ok: bool) -> Self {
                Self {
                    action: action.to_owned(),
                    sequence,
                    id: id.to_owned(),
                    ok,
                }
            }
        }

        /// The crafted-claim client written into a scenario's directory.
        struct Client {
            root: PathBuf,
            script: PathBuf,
            results: PathBuf,
            /// Plans written so far, which numbers the next plan file.
            plans: Cell<usize>,
        }

        impl Client {
            fn new(root: &Path) -> Self {
                let script = root.join("crafted-claim.py");
                std::fs::write(&script, CLIENT).expect("write the crafted-claim client");
                Self {
                    root: root.to_path_buf(),
                    script,
                    results: root.join("crafted-claim.results"),
                    plans: Cell::new(0),
                }
            }

            /// A step sending `claims` (claim type, sequence, subagent id) as
            /// `provider` from one process, in order.
            fn step(&self, provider: &str, claims: &[(&str, u64, &str)]) -> Step {
                let claims = claims
                    .iter()
                    .map(|(action, sequence, id)| {
                        json!({"type": action, "sequence": sequence, "id": id})
                    })
                    .collect();
                self.send(provider, &Value::Array(claims))
            }

            /// A step sending one main-`Stop` snapshot of `tasks` as Claude.
            fn snapshot(&self, sequence: u64, tasks: &[Value]) -> Step {
                self.send(
                    CLAUDE,
                    &json!([{"type": SNAPSHOT, "sequence": sequence, "tasks": tasks}]),
                )
            }

            /// A step sending the claims of `plan` as `provider` from one
            /// process, in order.
            fn send(&self, provider: &str, plan: &Value) -> Step {
                let path = self.write_plan(plan);
                Step::Command(format!(
                    "python3 -I {} {} {provider} {}",
                    quoted(&self.script),
                    quoted(&self.results),
                    quoted(&path)
                ))
            }

            /// A step sending, as `provider` from one process, a start and a
            /// stop of every id in `ids`, each start carrying `parent_id` and
            /// `agent_type`.
            fn finished_wide(
                &self,
                provider: &str,
                ids: &[String],
                (parent_id, agent_type): (&str, &str),
            ) -> Step {
                let claims = ids
                    .iter()
                    .zip(((1_u64 << 62)..).step_by(2))
                    .flat_map(|(id, sequence)| {
                        [
                            json!({"type": START, "sequence": sequence, "id": id,
                                "parent_id": parent_id, "agent_type": agent_type}),
                            json!({"type": STOP, "sequence": sequence + 1, "id": id}),
                        ]
                    })
                    .collect();
                self.send(provider, &Value::Array(claims))
            }

            /// A step sending a start of every id in `ids` as `provider` from
            /// one process, each carrying `parent_id` and `agent_type`.
            fn started_wide(
                &self,
                provider: &str,
                ids: &[String],
                (parent_id, agent_type): (&str, &str),
            ) -> Step {
                let claims = ids
                    .iter()
                    .zip((1_u64 << 62)..)
                    .map(|(id, sequence)| {
                        json!({"type": START, "sequence": sequence, "id": id,
                            "parent_id": parent_id, "agent_type": agent_type})
                    })
                    .collect();
                self.send(provider, &Value::Array(claims))
            }

            /// A step sending one main-`Stop` snapshot of `tasks` as Claude
            /// from a client under its own intermediate shell, so the claim
            /// names that shell: a reporter distinct from the PTY root and
            /// from every other such step.
            fn snapshot_from_own_reporter(&self, sequence: u64, tasks: &[Value]) -> Step {
                let path = self
                    .write_plan(&json!([{"type": SNAPSHOT, "sequence": sequence, "tasks": tasks}]));
                Step::Command(format!(
                    "/bin/sh -c 'python3 -I \"$0\" \"$1\" {CLAUDE} \"$2\"; true' {} {} {}",
                    quoted(&self.script),
                    quoted(&self.results),
                    quoted(&path)
                ))
            }

            /// Writes `plan` to the next numbered plan file and returns its path.
            fn write_plan(&self, plan: &Value) -> PathBuf {
                let index = self.plans.replace(self.plans.get() + 1);
                let path = self.root.join(format!("crafted-plan-{index}.json"));
                std::fs::write(&path, plan.to_string()).expect("write the claim plan");
                path
            }

            /// Every answered claim, in the order the worker answered them.
            fn answers(&self) -> Vec<Answer> {
                if let Ok(failure) =
                    std::fs::read_to_string(format!("{}.failure", self.results.display()))
                {
                    panic!("a crafted claim failed: {failure}");
                }
                std::fs::read_to_string(&self.results)
                    .expect("the crafted claims recorded their answers")
                    .lines()
                    .map(|line| {
                        let entry: Value = serde_json::from_str(line).expect("result line is JSON");
                        Answer {
                            action: entry["action"].as_str().expect("claim type").to_owned(),
                            sequence: entry["sequence"].as_u64().expect("sequence"),
                            id: entry["id"].as_str().expect("subagent id").to_owned(),
                            ok: entry["response"]["ok"]
                                .as_bool()
                                .unwrap_or_else(|| panic!("the answer carries ok: {entry}")),
                        }
                    })
                    .collect()
            }
        }

        /// One subagent claim event the worker logged.
        #[derive(Debug, Clone, PartialEq, Eq)]
        struct Logged {
            name: String,
            action: String,
            id: String,
            reason: String,
        }

        impl Logged {
            fn new(name: &str, action: &str, id: &str, reason: &str) -> Self {
                Self {
                    name: name.to_owned(),
                    action: action.to_owned(),
                    id: id.to_owned(),
                    reason: reason.to_owned(),
                }
            }
        }

        /// The counts of one applied snapshot, as the worker logged them.
        #[derive(Debug, Clone, Default, PartialEq, Eq)]
        struct Applied {
            tasks: u64,
            listed: u64,
            terminalized: u64,
            deferred: u64,
            hosts: bool,
        }

        /// Everything [`ClaimLog`] captured.
        #[derive(Default)]
        struct Captured {
            /// Ignored and refused claims.
            claims: Vec<Logged>,
            /// `(subagent id, reason)` of each reconciled subagent.
            reconciled: Vec<(String, String)>,
            /// Each applied snapshot.
            applied: Vec<Applied>,
        }

        /// The worker's subagent claim and snapshot events, captured on the
        /// test thread.
        ///
        /// These scenarios serve the worker in process on a current-thread
        /// runtime, so the worker logs on the thread whose default subscriber
        /// this installs.
        #[derive(Clone, Default)]
        struct ClaimLog(Arc<Mutex<Captured>>);

        impl ClaimLog {
            /// Installs this capture as the thread's default subscriber.
            ///
            /// A process-wide anchor dispatcher is registered first, for the
            /// reason `crate::runtime::lifecycle::tests::LogCapture` gives: with
            /// two registered dispatchers a callsite's interest is combined
            /// over both and each event consults the emitting thread's default.
            fn install(&self) -> tracing::subscriber::DefaultGuard {
                static ANCHOR: OnceLock<tracing::Dispatch> = OnceLock::new();
                ANCHOR.get_or_init(|| {
                    tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new())
                });
                tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()))
            }

            /// Ignored and refused claims, in the order the worker logged them.
            fn events(&self) -> Vec<Logged> {
                self.0.lock().expect("claim log").claims.clone()
            }

            /// `(subagent id, reason)` of each running subagent a snapshot
            /// completed, in the order the worker logged them.
            fn reconciled(&self) -> Vec<(String, String)> {
                self.0.lock().expect("claim log").reconciled.clone()
            }

            /// The counts of each applied snapshot, in order.
            fn applied(&self) -> Vec<Applied> {
                self.0.lock().expect("claim log").applied.clone()
            }
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ClaimLog {
            fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
                let name = event.metadata().name();
                let mut fields = ClaimFields::default();
                event.record(&mut fields);
                let mut captured = self.0.lock().expect("claim log");
                match name {
                    IGNORED_EVENT | REFUSED_EVENT => captured.claims.push(Logged {
                        name: name.to_owned(),
                        action: fields.action,
                        id: fields.id,
                        reason: fields.reason,
                    }),
                    RECONCILED_EVENT => captured.reconciled.push((fields.id, fields.reason)),
                    APPLIED_EVENT => captured.applied.push(fields.applied),
                    _ => {}
                }
            }
        }

        /// The fields of a subagent claim or snapshot event.
        #[derive(Default)]
        struct ClaimFields {
            action: String,
            id: String,
            reason: String,
            applied: Applied,
        }

        impl tracing::field::Visit for ClaimFields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                let slot = match field.name() {
                    "subagent.action" => &mut self.action,
                    "subagent.id" => &mut self.id,
                    "subagent.reason" => &mut self.reason,
                    _ => return,
                };
                value.clone_into(slot);
            }

            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                let slot = match field.name() {
                    "subagent.snapshot.listed" => &mut self.applied.listed,
                    "subagent.snapshot.terminalized" => &mut self.applied.terminalized,
                    "subagent.snapshot.tasks" => &mut self.applied.tasks,
                    "subagent.snapshot.deferred" => &mut self.applied.deferred,
                    _ => return,
                };
                *slot = value;
            }

            fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
                if field.name() == "subagent.snapshot.hosts" {
                    self.applied.hosts = value;
                }
            }

            fn record_debug(
                &mut self,
                _field: &tracing::field::Field,
                _value: &dyn std::fmt::Debug,
            ) {
            }
        }

        /// Rows of one Claude subagent `id` in `lifecycle`.
        fn one(id: &str, lifecycle: SubagentPhase) -> Rows {
            claude_rows(&[Row {
                agent_id: id.to_owned(),
                lifecycle,
            }])
        }

        /// Claude delivers hooks from separate processes, so a stop can reach
        /// the worker before the start it follows. A Claude stop for an unknown
        /// id at sequence 20, then the start for that id at sequence 10,
        /// leaves no row; a later start at sequence 30 creates a running row.
        /// Every claim is acknowledged.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_claude_start_older_than_an_ignored_stop_creates_no_row() {
            let root = crate::test_support::scoped_dir("ph-subagent-reorder-");
            let client = Client::new(&root);
            let steps = vec![
                client.step(CLAUDE, &[(STOP, 20, REORDERED), (START, 10, REORDERED)]),
                Step::Checkpoint(Rows::new()),
                client.step(CLAUDE, &[(START, 30, REORDERED)]),
            ];
            let run = replay(&root, steps, &one(REORDERED, SubagentPhase::Running)).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(STOP, 20, REORDERED, true),
                    Answer::new(START, 10, REORDERED, true),
                    Answer::new(START, 30, REORDERED, true),
                ]
            );
            run.finish().await;
        }

        /// Run 1 is 10..20. The stop of run 2 (40) reaches the worker before
        /// its start (30): the row stays completed, because the highest
        /// admitted claim is the stop at 40.
        #[tokio::test]
        async fn a_stop_ahead_of_its_start_keeps_the_next_run_completed() {
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-stop-ahead-");
            let client = Client::new(&root);
            let completed = one(REORDERED, SubagentPhase::Completed);
            let steps = vec![
                client.step(CLAUDE, &[(START, 10, REORDERED), (STOP, 20, REORDERED)]),
                Step::Checkpoint(completed.clone()),
                client.step(CLAUDE, &[(STOP, 40, REORDERED)]),
                Step::Checkpoint(completed.clone()),
                client.step(CLAUDE, &[(START, 30, REORDERED)]),
            ];
            let run = replay(&root, steps, &completed).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 10, REORDERED, true),
                    Answer::new(STOP, 20, REORDERED, true),
                    Answer::new(STOP, 40, REORDERED, true),
                    Answer::new(START, 30, REORDERED, true),
                ]
            );
            assert_eq!(
                log.events(),
                [
                    Logged::new(IGNORED_EVENT, "stop", REORDERED, "not_running"),
                    Logged::new(IGNORED_EVENT, "start", REORDERED, "stale_sequence"),
                ],
                "the newer stop on the terminal row and the older start are ignored"
            );
            // The stop at 40 advances only the record's sequence, which is not
            // public: the row keeps its revision and transition time.
            let [after_first_stop, after_newer_stop, _] = run.snapshots.as_slice() else {
                panic!("two checkpoints and the final rows: {:?}", run.snapshots);
            };
            let (before, after) = (
                row(after_first_stop, REORDERED),
                row(after_newer_stop, REORDERED),
            );
            assert_eq!(
                (after.revision, after.updated_at_ms),
                (before.revision, before.updated_at_ms),
                "{before:?} {after:?}"
            );
            run.finish().await;
        }

        /// Run 1 starts at 10. The start of run 2 (30) reaches the worker
        /// before the stop of run 1 (20): the row stays running, because the
        /// highest admitted claim is the start at 30; the stop of run 2 (40)
        /// completes it.
        #[tokio::test]
        async fn a_start_ahead_of_the_previous_stop_keeps_the_row_running() {
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-start-ahead-");
            let client = Client::new(&root);
            let steps = vec![
                client.step(
                    CLAUDE,
                    &[
                        (START, 10, REORDERED),
                        (START, 30, REORDERED),
                        (STOP, 20, REORDERED),
                    ],
                ),
                Step::Checkpoint(one(REORDERED, SubagentPhase::Running)),
                client.step(CLAUDE, &[(STOP, 40, REORDERED)]),
            ];
            let run = replay(&root, steps, &one(REORDERED, SubagentPhase::Completed)).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 10, REORDERED, true),
                    Answer::new(START, 30, REORDERED, true),
                    Answer::new(STOP, 20, REORDERED, true),
                    Answer::new(STOP, 40, REORDERED, true),
                ]
            );
            assert_eq!(
                log.events(),
                [
                    Logged::new(IGNORED_EVENT, "start", REORDERED, "duplicate_start"),
                    Logged::new(IGNORED_EVENT, "stop", REORDERED, "stale_sequence"),
                ],
                "the newer start on the running row and the older stop are ignored"
            );
            run.finish().await;
        }

        /// A Codex stop-only row at sequence 20 is journaled as completed; a
        /// Codex start at sequence 10 for it is older, so it is ignored and the
        /// row neither reopens nor changes revision.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_older_start_does_not_reopen_a_journaled_terminal_row() {
            /// Synthetic Codex child thread id.
            const CHILD: &str = "agent-child-reordered";
            let root = crate::test_support::scoped_dir("ph-subagent-stale-start-");
            let client = Client::new(&root);
            let completed = Rows::from([(
                (CODEX.to_owned(), CHILD.to_owned()),
                SubagentPhase::Completed,
            )]);
            let steps = vec![
                client.step(CODEX, &[(STOP, 20, CHILD)]),
                Step::Checkpoint(completed.clone()),
                client.step(CODEX, &[(START, 10, CHILD)]),
            ];
            let run = replay(&root, steps, &completed).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(STOP, 20, CHILD, true),
                    Answer::new(START, 10, CHILD, true),
                ]
            );
            let [journaled, after] = run.snapshots.as_slice() else {
                panic!("one checkpoint and the final rows: {:?}", run.snapshots);
            };
            assert_eq!(row(after, CHILD).revision, row(journaled, CHILD).revision);
            run.finish().await;
        }

        /// A Codex stop older than the recorded claim of a finished child is a
        /// delayed hook of an earlier turn: it changes nothing, and the next
        /// stop in order still records a turn (#916 D1).
        #[tokio::test(flavor = "multi_thread")]
        async fn a_delayed_older_codex_stop_changes_nothing() {
            /// Synthetic Codex child thread id.
            const CHILD: &str = "agent-child-delayed";
            let root = crate::test_support::scoped_dir("ph-subagent-codex-delayed-");
            let client = Client::new(&root);
            let completed = Rows::from([(
                (CODEX.to_owned(), CHILD.to_owned()),
                SubagentPhase::Completed,
            )]);
            let steps = vec![
                client.step(CODEX, &[(START, 10, CHILD), (STOP, 30, CHILD)]),
                Step::Checkpoint(completed.clone()),
                client.step(CODEX, &[(STOP, 20, CHILD)]),
                Step::Checkpoint(completed.clone()),
                client.step(CODEX, &[(STOP, 40, CHILD)]),
            ];
            let run = replay(&root, steps, &completed).await;
            let [after_turn, after_delayed, after_next] = run.snapshots.as_slice() else {
                panic!("two checkpoints and the final rows: {:?}", run.snapshots);
            };
            assert_eq!(
                row(after_delayed, CHILD),
                row(after_turn, CHILD),
                "the delayed stop changes nothing"
            );
            assert!(
                row(after_next, CHILD).revision > row(after_turn, CHILD).revision,
                "the stop in order records a turn"
            );
            run.finish().await;
        }

        /// A turn update makes a finished Codex row the newest terminal row, so
        /// the terminal history drops the oldest other row and keeps the one
        /// that just ran (#916 D1).
        #[tokio::test(flavor = "multi_thread")]
        async fn a_codex_turn_keeps_its_row_in_the_terminal_history() {
            let root = crate::test_support::scoped_dir("ph-subagent-codex-history-");
            let client = Client::new(&root);
            let ids: Vec<String> = (0..=TERMINAL_HISTORY)
                .map(|index| format!("agent-child-{index:03}"))
                .collect();
            let (full, overflow) = ids.split_at(TERMINAL_HISTORY);
            let sequence = |index: usize| u64::try_from(index).expect("index fits u64") * 10 + 10;
            let finish_all: Vec<(&str, u64, &str)> = full
                .iter()
                .enumerate()
                .map(|(index, id)| (STOP, sequence(index), id.as_str()))
                .collect();
            let completed = |ids: &[&String]| -> Rows {
                ids.iter()
                    .map(|id| ((CODEX.to_owned(), (*id).clone()), SubagentPhase::Completed))
                    .collect()
            };
            let oldest = &full[0];
            let steps = vec![
                client.step(CODEX, &finish_all),
                Step::Checkpoint(completed(&full.iter().collect::<Vec<_>>())),
                // The oldest row runs another turn, then one more child ends.
                client.step(CODEX, &[(STOP, sequence(TERMINAL_HISTORY), oldest)]),
                client.step(
                    CODEX,
                    &[(STOP, sequence(TERMINAL_HISTORY + 1), overflow[0].as_str())],
                ),
            ];
            // The second-oldest row is the one dropped, not the row that ran.
            let kept: Vec<&String> = std::iter::once(oldest)
                .chain(full[2..].iter())
                .chain(overflow.iter())
                .collect();
            replay(&root, steps, &completed(&kept)).await.finish().await;
        }

        /// With the active capacity of running subagents reached, one more
        /// start is refused: the hook is answered `ok: false`, no row is
        /// created, and the worker logs the refusal with reason `capacity`.
        #[tokio::test]
        async fn a_start_beyond_the_active_capacity_is_refused() {
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-capacity-");
            let client = Client::new(&root);
            // Fresh ids of the recorded Claude shape: `a` and 16 hex digits.
            let ids: Vec<String> = (0..=ACTIVE_CAPACITY)
                .map(|index| format!("ac{index:015x}"))
                .collect();
            let (running_ids, overflow) = ids.split_at(ACTIVE_CAPACITY);
            let overflow = overflow[0].as_str();
            let sequence = |index: usize| u64::try_from(index).expect("index fits u64") + 1;
            let starts: Vec<(&str, u64, &str)> = running_ids
                .iter()
                .enumerate()
                .map(|(index, id)| (START, sequence(index), id.as_str()))
                .collect();
            let running: Rows = claude_rows(
                &running_ids
                    .iter()
                    .map(|id| Row {
                        agent_id: id.clone(),
                        lifecycle: SubagentPhase::Running,
                    })
                    .collect::<Vec<_>>(),
            );
            let steps = vec![
                client.step(CLAUDE, &starts),
                Step::Checkpoint(running.clone()),
                client.step(CLAUDE, &[(START, sequence(ACTIVE_CAPACITY), overflow)]),
            ];
            let run = replay(&root, steps, &running).await;
            let answers = client.answers();
            let (accepted, refused) = answers.split_at(ACTIVE_CAPACITY);
            assert!(accepted.iter().all(|answer| answer.ok), "{accepted:?}");
            assert_eq!(
                refused,
                [Answer::new(
                    START,
                    sequence(ACTIVE_CAPACITY),
                    overflow,
                    false
                )]
            );
            assert_eq!(
                log.events(),
                [Logged::new(REFUSED_EVENT, "start", overflow, "capacity")]
            );
            run.finish().await;
        }

        /// `text` of exactly `bytes` bytes: `prefix`, then quotes, the widest
        /// accepted character once JSON-escaped.
        fn escaped_text(prefix: &str, bytes: usize) -> String {
            let mut text = prefix.to_owned();
            text.extend(std::iter::repeat_n('"', bytes - text.len()));
            text
        }

        /// The largest subagent state the worker keeps fits what carries it:
        /// every subagent provider at its running capacity, 32 terminal rows,
        /// every id, parent and type at its longest and made of quotes, and a
        /// full background snapshot. The journal stays under its one-MiB read
        /// limit, and the worker answers `inspect` in one control line (#916 D2).
        #[tokio::test]
        async fn the_largest_subagent_state_fits_the_journal_and_the_inspect_line() {
            /// Subagent providers: `SUBAGENT_PROVIDERS` in
            /// `crates/worker-protocol/src/hook_schema.rs`.
            const PROVIDERS: [&str; 3] = ["codex", "claude", "muse"];
            /// Terminal rows the worker keeps: `TERMINAL_SUBAGENT_CAPACITY` in
            /// `crates/session-worker/src/server.rs`.
            const TERMINAL_ROWS: usize = 32;
            /// Terminal rows started before the kept ones are pruned.
            const FINISHED: usize = TERMINAL_ROWS + 8;
            /// Longest parent id: the id bound.
            const PARENT_BOUND: usize = TASK_ID_BOUND;
            /// Longest agent type: `BACKGROUND_AGENT_TYPE_MAX_BYTES`.
            const AGENT_TYPE_BOUND: usize = 128;
            /// Longest background label: `BACKGROUND_LABEL_MAX_BYTES`.
            const LABEL_BOUND: usize = 160;
            /// Most background items one snapshot carries:
            /// `BACKGROUND_MAX_ITEMS`.
            const BACKGROUND_ITEMS: usize = 64;
            /// Journal read limit: `MAX_JOURNAL_BYTES` in
            /// `crates/session-worker/src/journal.rs`.
            const JOURNAL_BOUND: usize = 1024 * 1024;
            /// Sequence of the background snapshot, after every start; the
            /// starts and stops count from `1 << 62`, a 19-digit sequence like a
            /// monotonic clock reading.
            const SNAPSHOT_SEQUENCE: u64 = 1 << 63;

            let root = crate::test_support::scoped_dir("ph-subagent-largest-state-");
            let client = Client::new(&root);
            let wide = |prefix: &str, count: usize| -> Vec<String> {
                (0..count)
                    .map(|index| escaped_text(&format!("{prefix}{index:03}"), TASK_ID_BOUND))
                    .collect()
            };
            let parent = escaped_text("p", PARENT_BOUND);
            let agent_type = escaped_text("t", AGENT_TYPE_BOUND);
            let fields = (parent.as_str(), agent_type.as_str());
            let finished = wide("f", FINISHED);
            let tasks: Vec<Value> = (0..BACKGROUND_ITEMS)
                .map(|index| {
                    json!({
                        "id": escaped_text(&format!("b{index:03}"), TASK_ID_BOUND),
                        "type": "subagent",
                        "status": "running",
                        "label": escaped_text("l", LABEL_BOUND),
                        "agent_type": escaped_text("t", AGENT_TYPE_BOUND),
                    })
                })
                .collect();
            let mut steps = vec![client.finished_wide(CODEX, &finished, fields)];
            let mut running = Rows::new();
            for provider in PROVIDERS {
                let ids = wide(provider, ACTIVE_CAPACITY);
                steps.push(client.started_wide(provider, &ids, fields));
                running.extend(running_of(provider, &ids));
            }
            steps.push(client.snapshot(SNAPSHOT_SEQUENCE, &tasks));
            let run = replay_checking(&root, steps, None).await;

            let snapshot = run.snapshots.last().expect("the final snapshot");
            let rows = journaled_rows(snapshot);
            let running_now: Rows = rows
                .iter()
                .filter(|(_, phase)| **phase == SubagentPhase::Running)
                .map(|(key, phase)| (key.clone(), *phase))
                .collect();
            assert_eq!(running_now, running, "every provider holds its capacity");
            assert_eq!(
                rows.len(),
                PROVIDERS.len() * ACTIVE_CAPACITY + TERMINAL_ROWS,
                "{:?}",
                snapshot.subagents
            );
            let items = &background_of(snapshot).items;
            assert_eq!(items.len(), BACKGROUND_ITEMS);
            assert!(
                items.iter().all(|item| {
                    item.label.as_ref().map(String::len) == Some(LABEL_BOUND)
                        && item.agent_type.as_ref().map(String::len) == Some(AGENT_TYPE_BOUND)
                }),
                "every item keeps its longest label and agent type"
            );

            let inspect_line = serde_json::to_vec(snapshot).expect("the inspect snapshot as JSON");
            assert!(
                inspect_line.len() < pohunek_worker_protocol::MAX_CONTROL_LINE_BYTES,
                "{} inspect bytes",
                inspect_line.len()
            );
            let journal = std::fs::metadata(root.join("journal/worker.json"))
                .expect("the worker journal")
                .len();
            assert!(
                journal < u64::try_from(JOURNAL_BOUND).expect("the bound fits u64"),
                "{journal} journal bytes"
            );
            run.finish().await;
        }

        /// `count` distinct subagent ids that start with `prefix`.
        fn ids_of(prefix: &str, count: usize) -> Vec<String> {
            (0..count)
                .map(|index| format!("{prefix}{index:014x}"))
                .collect()
        }

        /// Start claims, one per id, numbered from `first`.
        fn starts_of(ids: &[String], first: u64) -> Vec<(&'static str, u64, &str)> {
            ids.iter()
                .zip(first..)
                .map(|(id, sequence)| (START, sequence, id.as_str()))
                .collect()
        }

        /// Rows of `provider` running `ids`.
        fn running_of(provider: &str, ids: &[String]) -> Rows {
            ids.iter()
                .map(|id| ((provider.to_owned(), id.clone()), SubagentPhase::Running))
                .collect()
        }

        /// The active capacity counts the running subagents of one provider.
        /// Codex children that never report a stop fill their own bound and
        /// leave Claude's untouched, and Claude's bound still refuses its own
        /// 65th start (#916 D2).
        #[tokio::test]
        async fn the_active_capacity_is_counted_per_provider() {
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-provider-capacity-");
            let client = Client::new(&root);
            let codex = ids_of("c", ACTIVE_CAPACITY);
            let claude = ids_of("a", ACTIVE_CAPACITY + 1);
            let (claude_running, claude_overflow) = claude.split_at(ACTIVE_CAPACITY);
            let mut both = running_of(CODEX, &codex);
            both.extend(running_of(CLAUDE, claude_running));
            let steps = vec![
                client.step(CODEX, &starts_of(&codex, 1)),
                client.step(CLAUDE, &starts_of(claude_running, 1)),
                Step::Checkpoint(both.clone()),
                client.step(
                    CLAUDE,
                    &starts_of(claude_overflow, u64::try_from(ACTIVE_CAPACITY).unwrap() + 1),
                ),
            ];
            let run = replay(&root, steps, &both).await;
            let answers = client.answers();
            let (accepted, refused) = answers.split_at(2 * ACTIVE_CAPACITY);
            assert!(accepted.iter().all(|answer| answer.ok), "{accepted:?}");
            assert_eq!(
                refused,
                [Answer::new(
                    START,
                    u64::try_from(ACTIVE_CAPACITY).unwrap() + 1,
                    &claude_overflow[0],
                    false
                )]
            );
            assert_eq!(
                log.events(),
                [Logged::new(
                    REFUSED_EVENT,
                    "start",
                    &claude_overflow[0],
                    "capacity"
                )]
            );
            run.finish().await;
        }

        /// The Codex start beyond its provider's capacity is refused, and
        /// the stop that follows records the child as completed, as a Codex
        /// stop without a recorded start does (#916 D2).
        #[tokio::test]
        async fn a_codex_start_beyond_its_capacity_is_refused_and_its_stop_is_recorded() {
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-codex-capacity-");
            let client = Client::new(&root);
            let ids = ids_of("c", ACTIVE_CAPACITY + 1);
            let (running_ids, overflow) = ids.split_at(ACTIVE_CAPACITY);
            let overflow = overflow[0].as_str();
            let beyond = u64::try_from(ACTIVE_CAPACITY).unwrap() + 1;
            let running = running_of(CODEX, running_ids);
            let mut settled = running.clone();
            settled.insert(
                (CODEX.to_owned(), overflow.to_owned()),
                SubagentPhase::Completed,
            );
            let steps = vec![
                client.step(CODEX, &starts_of(running_ids, 1)),
                client.step(CODEX, &[(START, beyond, overflow)]),
                Step::Checkpoint(running),
                client.step(CODEX, &[(STOP, beyond + 1, overflow)]),
            ];
            let run = replay(&root, steps, &settled).await;
            let answers = client.answers();
            let (accepted, tail) = answers.split_at(ACTIVE_CAPACITY);
            assert!(accepted.iter().all(|answer| answer.ok), "{accepted:?}");
            assert_eq!(
                tail,
                [
                    Answer::new(START, beyond, overflow, false),
                    Answer::new(STOP, beyond + 1, overflow, true),
                ]
            );
            assert_eq!(
                log.events(),
                [Logged::new(REFUSED_EVENT, "start", overflow, "capacity")]
            );
            run.finish().await;
        }

        /// Main-`Stop` tasks listing the background subagents `ids`.
        fn listed(ids: &[&str]) -> Vec<Value> {
            ids.iter()
                .map(|id| json!({"id": id, "type": "subagent", "status": "running"}))
                .collect()
        }

        /// A snapshot at `sequence` and whether the worker acknowledged it.
        fn snapshot_answer(sequence: u64, ok: bool) -> Answer {
            Answer::new(SNAPSHOT, sequence, "", ok)
        }

        /// Main-`Stop` tasks listing only a background shell, a task type
        /// that runs no agent.
        fn shells() -> Vec<Value> {
            vec![json!({"id": "b5v9n3m7k", "type": "shell", "status": "running"})]
        }

        /// R2 ordering against a main `Stop`: a self-woken subagent's start
        /// (sequence 30) is decided before an older snapshot (sequence 20)
        /// that does not list it yet. The snapshot is acknowledged and leaves
        /// the newer run running (#883 D4, D6).
        ///
        /// Claude builds a listing before the `Stop` hook reads its clock (S4
        /// evidence, real Claude 2.1.289 and 2.1.296), so the empty snapshot
        /// at 40, whose predecessor (20) is older than the start, cannot
        /// prove the run had started when its listing was built and leaves it
        /// running; the snapshot at 50 settles it.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_self_wake_start_after_an_older_snapshot_stays_running() {
            let woken = recorded("self_wake").events[0].subagent().agent_id.clone();
            let root = crate::test_support::scoped_dir("ph-subagent-older-snapshot-");
            let client = Client::new(&root);
            let steps = vec![
                client.step(
                    CLAUDE,
                    &[(START, 5, &woken), (STOP, 10, &woken), (START, 30, &woken)],
                ),
                client.snapshot(20, &[]),
                Step::Checkpoint(one(&woken, SubagentPhase::Running)),
                client.snapshot(40, &[]),
                Step::Checkpoint(one(&woken, SubagentPhase::Running)),
                client.snapshot(50, &[]),
            ];
            let run = replay(&root, steps, &one(&woken, SubagentPhase::Completed)).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 5, &woken, true),
                    Answer::new(STOP, 10, &woken, true),
                    Answer::new(START, 30, &woken, true),
                    snapshot_answer(20, true),
                    snapshot_answer(40, true),
                    snapshot_answer(50, true),
                ]
            );
            run.finish().await;
        }

        /// Main-`Stop` snapshots of one reporter can reach the worker out of
        /// order, since each hook is its own process. A background agent
        /// started at 10 and listed by the snapshot at 30 is not completed by
        /// an older empty snapshot (20) delivered after it: the listing at 30
        /// is newer than that snapshot, so it does not make the agent a known
        /// background agent whose absence at 20 proves it finished, and no
        /// earlier snapshot proves it predates the listing at 20. The row
        /// keeps its revision, and the snapshot at 20 defers it. A later empty
        /// snapshot (40) then settles it as a known background agent
        /// (#883 D4).
        #[tokio::test]
        async fn an_older_empty_snapshot_after_a_newer_listing_keeps_the_agent_running() {
            /// The listed background agent, of the recorded id shape.
            const LISTED: &str = "a6b0d4f8c2e5a9b3d";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-reordered-snapshots-");
            let client = Client::new(&root);
            let running = one(LISTED, SubagentPhase::Running);
            let steps = vec![
                client.step(CLAUDE, &[(START, 10, LISTED)]),
                client.snapshot(30, &listed(&[LISTED])),
                Step::Checkpoint(running.clone()),
                client.snapshot(20, &[]),
                Step::Checkpoint(running),
                client.snapshot(40, &[]),
            ];
            let run = replay(&root, steps, &one(LISTED, SubagentPhase::Completed)).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 10, LISTED, true),
                    snapshot_answer(30, true),
                    snapshot_answer(20, true),
                    snapshot_answer(40, true),
                ]
            );
            let [listed_row, after_older, settled] = run.snapshots.as_slice() else {
                panic!("two checkpoints and the final rows: {:?}", run.snapshots);
            };
            let [listed_row, after_older, settled] =
                [listed_row, after_older, settled].map(|snapshot| row(snapshot, LISTED));
            assert_eq!(
                after_older, listed_row,
                "the older snapshot changes nothing: {listed_row:?} {after_older:?}"
            );
            assert!(settled.revision > listed_row.revision, "{settled:?}");
            assert_eq!(settled.finished_at_ms, Some(settled.updated_at_ms));
            assert_eq!(
                log.reconciled(),
                [(LISTED.to_owned(), "snapshot_absent".to_owned())],
                "only the snapshot at 40 reconciles"
            );
            assert_eq!(
                log.applied(),
                [
                    Applied {
                        tasks: 1,
                        listed: 1,
                        hosts: true,
                        ..Applied::default()
                    },
                    Applied {
                        deferred: 1,
                        ..Applied::default()
                    },
                    Applied {
                        terminalized: 1,
                        ..Applied::default()
                    },
                ]
            );
            assert_eq!(log.events(), [], "no claim is ignored or refused");
            run.finish().await;
        }

        /// A reporter's newer snapshot delivered first (60) is never taken as
        /// the previous snapshot of its older one (45) delivered after it: a
        /// never-listed subagent started at 30 stays running through both
        /// shells-only snapshots, because neither has an older snapshot of
        /// its reporter proving the start predates its listing (S4 evidence:
        /// Claude builds the listing before the `Stop` hook reads its clock).
        /// A later shells-only snapshot (70), whose previous one (60) is
        /// newer than the start, settles it (#883 D4).
        #[tokio::test]
        async fn a_newer_snapshot_delivered_first_proves_nothing_for_an_older_one() {
            /// The never-listed subagent, of the recorded id shape.
            const UNLISTED: &str = "a3e7b1d5f9c2a6e0b";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-reordered-previous-");
            let client = Client::new(&root);
            let running = one(UNLISTED, SubagentPhase::Running);
            let steps = vec![
                client.step(CLAUDE, &[(START, 30, UNLISTED)]),
                client.snapshot(60, &shells()),
                Step::Checkpoint(running.clone()),
                client.snapshot(45, &shells()),
                Step::Checkpoint(running),
                client.snapshot(70, &shells()),
            ];
            let run = replay(&root, steps, &one(UNLISTED, SubagentPhase::Completed)).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 30, UNLISTED, true),
                    snapshot_answer(60, true),
                    snapshot_answer(45, true),
                    snapshot_answer(70, true),
                ]
            );
            let deferred = Applied {
                tasks: 1,
                deferred: 1,
                ..Applied::default()
            };
            assert_eq!(
                log.applied(),
                [
                    deferred.clone(),
                    deferred,
                    Applied {
                        tasks: 1,
                        terminalized: 1,
                        ..Applied::default()
                    },
                ]
            );
            assert_eq!(
                log.reconciled(),
                [(UNLISTED.to_owned(), "snapshot_absent".to_owned())]
            );
            run.finish().await;
        }

        /// The worker remembers the last snapshot of at most
        /// [`SNAPSHOT_REPORTERS`] reporters, and a forgotten reporter's next
        /// snapshot has no previous one: it proves nothing about a
        /// never-listed subagent's start (#883 D4).
        ///
        /// The PTY root starts a subagent at 10 and sends a shells-only
        /// snapshot (20), which defers it and is remembered. Then
        /// [`SNAPSHOT_REPORTERS`] other reporters, each a client under its own
        /// intermediate shell, send one snapshot each, which forgets the
        /// root's. The root's next shells-only snapshot (40) therefore defers
        /// the subagent again, and the one after it (50) settles it.
        #[tokio::test]
        async fn a_forgotten_reporters_snapshot_proves_nothing() {
            /// The never-listed subagent, of the recorded id shape.
            const UNLISTED: &str = "a7c1e5b9d3f6a0c4e";
            /// Sequence of the first snapshot of every other reporter; each
            /// next one is one higher.
            const OTHERS_FROM: u64 = 21;
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-forgotten-reporter-");
            let client = Client::new(&root);
            let running = one(UNLISTED, SubagentPhase::Running);
            let others: Vec<u64> = (OTHERS_FROM..).take(SNAPSHOT_REPORTERS).collect();
            let mut steps = vec![
                client.step(CLAUDE, &[(START, 10, UNLISTED)]),
                client.snapshot(20, &shells()),
            ];
            steps.extend(
                others
                    .iter()
                    .map(|sequence| client.snapshot_from_own_reporter(*sequence, &[])),
            );
            steps.extend([
                Step::Checkpoint(running.clone()),
                client.snapshot(40, &shells()),
                Step::Checkpoint(running),
                client.snapshot(50, &shells()),
            ]);
            let run = replay(&root, steps, &one(UNLISTED, SubagentPhase::Completed)).await;

            let mut expected = vec![
                Answer::new(START, 10, UNLISTED, true),
                snapshot_answer(20, true),
            ];
            expected.extend(
                others
                    .iter()
                    .map(|sequence| snapshot_answer(*sequence, true)),
            );
            expected.extend([snapshot_answer(40, true), snapshot_answer(50, true)]);
            assert_eq!(client.answers(), expected);
            let deferred = Applied {
                tasks: 1,
                deferred: 1,
                ..Applied::default()
            };
            // Another reporter's snapshot does not consider the root's
            // subagent at all.
            let mut applied = vec![deferred.clone()];
            applied.extend(others.iter().map(|_| Applied::default()));
            applied.extend([
                deferred,
                Applied {
                    tasks: 1,
                    terminalized: 1,
                    ..Applied::default()
                },
            ]);
            assert_eq!(log.applied(), applied);
            assert_eq!(
                log.reconciled(),
                [(UNLISTED.to_owned(), "snapshot_absent".to_owned())]
            );
            run.finish().await;
        }

        /// A listing from an earlier run of the same subagent does not make
        /// its current run a known background agent. The run listed at 30
        /// stops at 35, and a self-wake start at 40 opens a new run. The empty
        /// snapshot at 50 defers it: the listing at 30 predates the run, and
        /// the previous snapshot (30) is older than the start, so nothing
        /// proves the run predates the listing at 50 (S4 evidence: Claude
        /// builds the listing before the `Stop` hook reads its clock). The
        /// empty snapshot at 60, whose previous one (50) is newer than the
        /// start, settles it (#883 D1, D4).
        #[tokio::test]
        async fn a_listing_of_an_earlier_run_does_not_settle_a_self_woken_run() {
            /// The self-woken subagent, of the recorded id shape.
            const WOKEN: &str = "a0e4c8f2b6d9a3e7c";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-earlier-run-listing-");
            let client = Client::new(&root);
            let running = one(WOKEN, SubagentPhase::Running);
            let steps = vec![
                client.step(CLAUDE, &[(START, 10, WOKEN)]),
                client.snapshot(30, &listed(&[WOKEN])),
                client.step(CLAUDE, &[(STOP, 35, WOKEN), (START, 40, WOKEN)]),
                Step::Checkpoint(running.clone()),
                client.snapshot(50, &[]),
                Step::Checkpoint(running),
                client.snapshot(60, &[]),
            ];
            let run = replay(&root, steps, &one(WOKEN, SubagentPhase::Completed)).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 10, WOKEN, true),
                    snapshot_answer(30, true),
                    Answer::new(STOP, 35, WOKEN, true),
                    Answer::new(START, 40, WOKEN, true),
                    snapshot_answer(50, true),
                    snapshot_answer(60, true),
                ]
            );
            let [woken, after_empty, _] = run.snapshots.as_slice() else {
                panic!("two checkpoints and the final rows: {:?}", run.snapshots);
            };
            assert_eq!(row(after_empty, WOKEN), row(woken, WOKEN));
            assert_eq!(
                log.applied(),
                [
                    Applied {
                        tasks: 1,
                        listed: 1,
                        hosts: true,
                        ..Applied::default()
                    },
                    Applied {
                        deferred: 1,
                        ..Applied::default()
                    },
                    Applied {
                        terminalized: 1,
                        ..Applied::default()
                    },
                ]
            );
            assert_eq!(
                log.reconciled(),
                [(WOKEN.to_owned(), "snapshot_absent".to_owned())]
            );
            assert_eq!(log.events(), [], "no claim is ignored or refused");
            run.finish().await;
        }

        /// A listing delivered out of order never lowers the newest listing
        /// of a subagent. The background agent started at 10 is listed at
        /// 30, then by a late snapshot at 20, and a late empty snapshot at 25
        /// follows. The newest listing (30) is newer than 25, so it does not
        /// make the agent a known background agent whose absence at 25 proves
        /// it finished, and no older snapshot proves its start predates the
        /// listing at 25: the agent stays running. The empty snapshot at 40
        /// then settles it as a known background agent (#883 D4).
        #[tokio::test]
        async fn a_late_older_listing_does_not_lower_the_newest_listing() {
            /// The listed background agent, of the recorded id shape.
            const LISTED: &str = "a4d8b2f6e0c3a7d1b";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-late-listing-");
            let client = Client::new(&root);
            let running = one(LISTED, SubagentPhase::Running);
            let steps = vec![
                client.step(CLAUDE, &[(START, 10, LISTED)]),
                client.snapshot(30, &listed(&[LISTED])),
                client.snapshot(20, &listed(&[LISTED])),
                Step::Checkpoint(running.clone()),
                client.snapshot(25, &[]),
                Step::Checkpoint(running),
                client.snapshot(40, &[]),
            ];
            let run = replay(&root, steps, &one(LISTED, SubagentPhase::Completed)).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 10, LISTED, true),
                    snapshot_answer(30, true),
                    snapshot_answer(20, true),
                    snapshot_answer(25, true),
                    snapshot_answer(40, true),
                ]
            );
            let [listed_row, after_late, _] = run.snapshots.as_slice() else {
                panic!("two checkpoints and the final rows: {:?}", run.snapshots);
            };
            assert_eq!(row(after_late, LISTED), row(listed_row, LISTED));
            let listing = Applied {
                tasks: 1,
                listed: 1,
                hosts: true,
                ..Applied::default()
            };
            assert_eq!(
                log.applied(),
                [
                    listing.clone(),
                    listing,
                    Applied {
                        deferred: 1,
                        ..Applied::default()
                    },
                    Applied {
                        terminalized: 1,
                        ..Applied::default()
                    },
                ]
            );
            assert_eq!(
                log.reconciled(),
                [(LISTED.to_owned(), "snapshot_absent".to_owned())]
            );
            assert_eq!(log.events(), [], "no claim is ignored or refused");
            run.finish().await;
        }

        /// A snapshot never touches a terminal subagent, even one it lists
        /// as live: an older snapshot (sequence 8) and a newer one (20) list
        /// the subagent completed at 10, and one at 30 lists another agent.
        /// The row stays completed with its revision and timestamps (#883 D4).
        ///
        /// R4 and the S4 evidence show a stopping agent still listed as
        /// running in the main `Stop` after its own `SubagentStop`; a
        /// self-woken run reopens through its own newer `SubagentStart`.
        #[tokio::test]
        async fn a_snapshot_listing_a_completed_subagent_leaves_it_completed() {
            /// The listed subagent, of the recorded id shape.
            const LISTED: &str = "a2c6e0b4d8f1a5c9e";
            /// Another background agent the worker has no record of.
            const OTHER: &str = "a6e0c4b8d2f5a9e3c";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-listed-terminal-");
            let client = Client::new(&root);
            let completed = one(LISTED, SubagentPhase::Completed);
            let steps = vec![
                client.step(CLAUDE, &[(START, 5, LISTED), (STOP, 10, LISTED)]),
                Step::Checkpoint(completed.clone()),
                client.snapshot(8, &listed(&[LISTED])),
                client.snapshot(20, &listed(&[LISTED])),
                client.snapshot(30, &listed(&[OTHER])),
            ];
            let run = replay(&root, steps, &completed).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 5, LISTED, true),
                    Answer::new(STOP, 10, LISTED, true),
                    snapshot_answer(8, true),
                    snapshot_answer(20, true),
                    snapshot_answer(30, true),
                ]
            );
            let [before, after] = run.snapshots.as_slice() else {
                panic!("one checkpoint and the final rows: {:?}", run.snapshots);
            };
            assert_eq!(row(after, LISTED), row(before, LISTED));
            assert_eq!(log.reconciled(), [], "nothing is reconciled");
            let listing_one = Applied {
                tasks: 1,
                hosts: true,
                ..Applied::default()
            };
            assert_eq!(
                log.applied(),
                [listing_one.clone(), listing_one.clone(), listing_one]
            );
            assert_eq!(log.events(), [], "no claim is ignored or refused");
            run.finish().await;
        }

        /// S4 evidence (real Claude 2.1.289 and 2.1.296): a fast background
        /// agent, listed by the main `Stop` at 15 while it ran, sends its own
        /// `SubagentStop` (20) before the next main `Stop` (30), whose listing
        /// was built earlier and still shows it as running. The row stays
        /// completed through that snapshot and the next empty one, with no
        /// running flicker and no new revision (#883 D4).
        #[tokio::test]
        async fn a_fast_background_agent_stays_completed_under_a_lagging_listing() {
            /// The fast background agent, of the recorded id shape.
            const FAST: &str = "a1e5c9f3b7d0a4e8c";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-lagging-listing-");
            let client = Client::new(&root);
            let completed = one(FAST, SubagentPhase::Completed);
            let steps = vec![
                client.step(CLAUDE, &[(START, 10, FAST)]),
                client.snapshot(15, &listed(&[FAST])),
                Step::Checkpoint(one(FAST, SubagentPhase::Running)),
                client.step(CLAUDE, &[(STOP, 20, FAST)]),
                Step::Checkpoint(completed.clone()),
                client.snapshot(30, &listed(&[FAST])),
                Step::Checkpoint(completed.clone()),
                client.snapshot(40, &[]),
            ];
            let run = replay(&root, steps, &completed).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 10, FAST, true),
                    snapshot_answer(15, true),
                    Answer::new(STOP, 20, FAST, true),
                    snapshot_answer(30, true),
                    snapshot_answer(40, true),
                ]
            );
            let [_, stopped, listed_again, last] = run.snapshots.as_slice() else {
                panic!("three checkpoints and the final rows: {:?}", run.snapshots);
            };
            let stopped = row(stopped, FAST);
            assert_eq!(row(listed_again, FAST), stopped);
            assert_eq!(row(last, FAST), stopped);
            assert_eq!(log.reconciled(), [], "nothing is reconciled");
            assert_eq!(
                log.applied(),
                [
                    Applied {
                        tasks: 1,
                        listed: 1,
                        hosts: true,
                        ..Applied::default()
                    },
                    Applied {
                        tasks: 1,
                        hosts: true,
                        ..Applied::default()
                    },
                    Applied::default(),
                ]
            );
            assert_eq!(log.events(), [], "no claim is ignored or refused");
            run.finish().await;
        }

        /// A main `Stop` snapshot (sequence 30) that lists a running subagent
        /// leaves its recorded sequence alone, so the subagent's own stop (20),
        /// delivered after the snapshot, still completes it; the listing
        /// changes no public state. A later stop (40) on the completed row is
        /// ignored as `not_running` (#883 D4, ordering invariant).
        #[tokio::test]
        async fn a_stop_older_than_the_listing_completes_the_subagent() {
            /// The listed subagent, of the recorded id shape.
            const LISTED: &str = "a9c3e7b1d5f8a2c6e";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-listed-older-stop-");
            let client = Client::new(&root);
            let running = one(LISTED, SubagentPhase::Running);
            let completed = one(LISTED, SubagentPhase::Completed);
            let steps = vec![
                client.step(CLAUDE, &[(START, 10, LISTED)]),
                Step::Checkpoint(running.clone()),
                client.snapshot(30, &listed(&[LISTED])),
                Step::Checkpoint(running),
                client.step(CLAUDE, &[(STOP, 20, LISTED)]),
                Step::Checkpoint(completed.clone()),
                client.step(CLAUDE, &[(STOP, 40, LISTED)]),
            ];
            let run = replay(&root, steps, &completed).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 10, LISTED, true),
                    snapshot_answer(30, true),
                    Answer::new(STOP, 20, LISTED, true),
                    Answer::new(STOP, 40, LISTED, true),
                ]
            );
            assert_eq!(
                log.events(),
                [Logged::new(IGNORED_EVENT, "stop", LISTED, "not_running")],
                "only the stop on the completed row is ignored"
            );
            let [started, listed_row, stopped, last] = run.snapshots.as_slice() else {
                panic!("three checkpoints and the final rows: {:?}", run.snapshots);
            };
            let [started, listed_row, stopped, last] =
                [started, listed_row, stopped, last].map(|snapshot| row(snapshot, LISTED));
            assert_eq!(
                listed_row, started,
                "the listing changes no public state: {started:?} {listed_row:?}"
            );
            assert!(stopped.revision > listed_row.revision, "{stopped:?}");
            assert_eq!(last.revision, stopped.revision, "{stopped:?} {last:?}");
            assert_eq!(
                log.reconciled(),
                [],
                "the stop, not the snapshot, completes it"
            );
            assert_eq!(
                log.applied(),
                [Applied {
                    tasks: 1,
                    listed: 1,
                    hosts: true,
                    ..Applied::default()
                }]
            );
            run.finish().await;
        }

        /// A start (30) whose listing may have been built before it: the
        /// first snapshot of its reporter (40) lists only a shell, which runs
        /// no agent, but no earlier snapshot proves the start predates the
        /// listing (S4 evidence: Claude builds the listing before the `Stop`
        /// hook reads its clock), so the subagent is deferred and stays
        /// running. The next shells-only snapshot (50) settles it (#883 D4).
        #[tokio::test]
        async fn a_start_racing_a_shells_only_listing_stays_running_until_the_next_one() {
            /// The racing subagent, of the recorded id shape.
            const RACING: &str = "a4f8b2e6c0d3a7f1b";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-racing-start-");
            let client = Client::new(&root);
            let steps = vec![
                client.step(CLAUDE, &[(START, 30, RACING)]),
                client.snapshot(40, &shells()),
                Step::Checkpoint(one(RACING, SubagentPhase::Running)),
                client.snapshot(50, &shells()),
            ];
            let run = replay(&root, steps, &one(RACING, SubagentPhase::Completed)).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 30, RACING, true),
                    snapshot_answer(40, true),
                    snapshot_answer(50, true),
                ]
            );
            assert_eq!(
                log.reconciled(),
                [(RACING.to_owned(), "snapshot_absent".to_owned())]
            );
            assert_eq!(
                log.applied(),
                [
                    Applied {
                        tasks: 1,
                        deferred: 1,
                        ..Applied::default()
                    },
                    Applied {
                        tasks: 1,
                        terminalized: 1,
                        ..Applied::default()
                    },
                ]
            );
            assert_eq!(log.events(), [], "no claim is ignored or refused");
            run.finish().await;
        }

        /// A known background agent (listed at 30) that a later snapshot (40)
        /// omits is completed, keeping its own recorded sequence (10). A
        /// resume start (35) that falls between the record and that snapshot,
        /// in the window between Claude building the listing and the `Stop`
        /// hook reading its clock (S4 evidence), still reopens it with a new
        /// revision; its stop (50) completes it again (#883 D1, D4).
        #[tokio::test]
        async fn a_settled_background_agent_keeps_its_sequence_for_a_resume() {
            /// The background agent, of the recorded id shape.
            const RESUMED: &str = "a8d2f6b0c4e7a1d5f";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-settled-resume-");
            let client = Client::new(&root);
            let completed = one(RESUMED, SubagentPhase::Completed);
            let steps = vec![
                client.step(CLAUDE, &[(START, 10, RESUMED)]),
                client.snapshot(30, &listed(&[RESUMED])),
                Step::Checkpoint(one(RESUMED, SubagentPhase::Running)),
                client.snapshot(40, &[]),
                Step::Checkpoint(completed.clone()),
                client.step(CLAUDE, &[(START, 35, RESUMED)]),
                Step::Checkpoint(one(RESUMED, SubagentPhase::Running)),
                client.step(CLAUDE, &[(STOP, 50, RESUMED)]),
            ];
            let run = replay(&root, steps, &completed).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(START, 10, RESUMED, true),
                    snapshot_answer(30, true),
                    snapshot_answer(40, true),
                    Answer::new(START, 35, RESUMED, true),
                    Answer::new(STOP, 50, RESUMED, true),
                ]
            );
            let [running, settled, reopened, last] = run.snapshots.as_slice() else {
                panic!("three checkpoints and the final rows: {:?}", run.snapshots);
            };
            let [running, settled, reopened, last] =
                [running, settled, reopened, last].map(|snapshot| row(snapshot, RESUMED));
            assert!(
                running.revision < settled.revision
                    && settled.revision < reopened.revision
                    && reopened.revision < last.revision,
                "{running:?} {settled:?} {reopened:?} {last:?}"
            );
            assert_eq!(settled.finished_at_ms, Some(settled.updated_at_ms));
            assert_eq!(reopened.finished_at_ms, None, "{reopened:?}");
            assert_eq!(reopened.started_at_ms, running.started_at_ms);
            assert_eq!(
                log.reconciled(),
                [(RESUMED.to_owned(), "snapshot_absent".to_owned())]
            );
            assert_eq!(
                log.applied(),
                [
                    Applied {
                        tasks: 1,
                        listed: 1,
                        hosts: true,
                        ..Applied::default()
                    },
                    Applied {
                        terminalized: 1,
                        ..Applied::default()
                    },
                ]
            );
            assert_eq!(log.events(), [], "the resume start is applied, not ignored");
            run.finish().await;
        }

        /// Records left running by lost stops cannot exhaust the subagent
        /// capacity (#883 D4).
        ///
        /// At capacity, a further start is refused and adds no row. The
        /// reporter's first empty main `Stop` snapshot defers every stale
        /// record, because Claude builds a listing before the `Stop` hook
        /// reads its clock (S4 evidence) and no earlier snapshot proves the
        /// records predate it. The next empty snapshot settles all of them,
        /// the terminal history keeps its bound, and the refused subagent
        /// then starts.
        #[tokio::test]
        async fn stale_running_subagents_cannot_exhaust_the_capacity() {
            /// The start beyond the capacity, of the recorded id shape.
            const EXTRA: &str = "ae1f2a3b4c5d6e7f8";
            /// Sequence of the first snapshot, after every start.
            const FIRST_SNAPSHOT: u64 = 900;
            /// Sequence of the snapshot that settles the stale records.
            const SNAPSHOT_SEQUENCE: u64 = 1_000;
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-stale-capacity-");
            let client = Client::new(&root);
            let stale: Vec<String> = (0..ACTIVE_CAPACITY)
                .map(|index| format!("ac{index:015x}"))
                .collect();
            let mut starts: Vec<(&str, u64, &str)> = stale
                .iter()
                .zip(1_u64..)
                .map(|(id, sequence)| (START, sequence, id.as_str()))
                .collect();
            let over = u64::try_from(ACTIVE_CAPACITY + 1).expect("the capacity fits a sequence");
            starts.push((START, over, EXTRA));
            let full: Rows = stale
                .iter()
                .map(|id| ((CLAUDE.to_owned(), id.clone()), SubagentPhase::Running))
                .collect();
            let steps = vec![
                client.step(CLAUDE, &starts),
                Step::Checkpoint(full.clone()),
                client.snapshot(FIRST_SNAPSHOT, &[]),
                Step::Checkpoint(full),
                client.snapshot(SNAPSHOT_SEQUENCE, &[]),
                client.step(CLAUDE, &[(START, SNAPSHOT_SEQUENCE + 1, EXTRA)]),
            ];
            let run = replay_checking(&root, steps, None).await;

            let mut expected: Vec<Answer> = starts
                .iter()
                .map(|(action, sequence, id)| Answer::new(action, *sequence, id, *id != EXTRA))
                .collect();
            expected.extend([
                snapshot_answer(FIRST_SNAPSHOT, true),
                snapshot_answer(SNAPSHOT_SEQUENCE, true),
                Answer::new(START, SNAPSHOT_SEQUENCE + 1, EXTRA, true),
            ]);
            assert_eq!(client.answers(), expected);
            assert_eq!(
                log.events(),
                [Logged::new(REFUSED_EVENT, "start", EXTRA, "capacity")]
            );
            let capacity = u64::try_from(ACTIVE_CAPACITY).expect("the capacity fits u64");
            assert_eq!(
                log.applied(),
                [
                    Applied {
                        deferred: capacity,
                        ..Applied::default()
                    },
                    Applied {
                        terminalized: capacity,
                        ..Applied::default()
                    },
                ]
            );
            let mut reconciled = log.reconciled();
            reconciled.sort();
            let every_stale: Vec<(String, String)> = stale
                .iter()
                .map(|id| (id.clone(), "snapshot_absent".to_owned()))
                .collect();
            assert_eq!(reconciled, every_stale);

            let rows = journaled_rows(run.snapshots.last().expect("the final rows"));
            let running: Vec<&String> = rows
                .iter()
                .filter(|(_, phase)| **phase == SubagentPhase::Running)
                .map(|((_, id), _)| id)
                .collect();
            assert_eq!(running, [EXTRA], "{rows:?}");
            let settled: Vec<(&String, &SubagentPhase)> = rows
                .iter()
                .filter(|((_, id), _)| id != EXTRA)
                .map(|((_, id), phase)| (id, phase))
                .collect();
            assert_eq!(
                settled.len(),
                TERMINAL_HISTORY,
                "the terminal history keeps its bound: {rows:?}"
            );
            assert!(
                settled
                    .iter()
                    .all(|(id, phase)| stale.contains(id) && **phase == SubagentPhase::Completed),
                "every stale record is settled as completed: {rows:?}"
            );
            run.finish().await;
        }

        /// The worker refuses a snapshot whose task list breaks a bound or
        /// holds invalid text as a whole: it answers `ok: false`, settles
        /// nothing and logs no applied snapshot. A valid snapshot of the same
        /// agent then settles the subagent (#883 D4).
        ///
        /// A first valid shells-only snapshot (7) defers the subagent and gives
        /// later snapshots the earlier snapshot that a never-listed subagent's
        /// settlement needs (S4 evidence: Claude builds the listing before the
        /// `Stop` hook reads its clock). Apart from the over-long type, which
        /// is a type that can host agents, each refused list would then settle
        /// the subagent if it were applied, or applied without its invalid
        /// entry.
        #[tokio::test]
        async fn an_invalid_snapshot_task_list_is_refused_whole() {
            /// The running subagent, of the recorded id shape.
            const STALE: &str = "a3d7f1b5c9e2a6d0f";
            /// Sequence of the first valid snapshot.
            const FIRST_VALID: u64 = 7;
            /// Sequence of the first refused snapshot; each next one is one
            /// higher.
            const FIRST_REFUSED: u64 = 10;
            /// Sequence of the valid snapshot that settles the subagent.
            const VALID: u64 = 100;
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-invalid-snapshot-");
            let client = Client::new(&root);
            let task = |id: &str, kind: &str, status: &str| json!({"id": id, "type": kind, "status": status});
            let shell = task("b2n6m0k4j", "shell", "running");
            let with_shell = |entry: Value| json!([shell, entry]);
            // In order: too many tasks, an over-long id, type and status, an
            // empty status, and a control character in an id.
            let invalid = [
                Value::Array(vec![shell.clone(); SNAPSHOT_TASK_BOUND + 1]),
                with_shell(task(&"b".repeat(TASK_ID_BOUND + 1), "shell", "running")),
                with_shell(task(
                    "b8h2g6f0d",
                    &"s".repeat(TASK_TYPE_BOUND + 1),
                    "running",
                )),
                with_shell(task(
                    "b8h2g6f0d",
                    "shell",
                    &"r".repeat(TASK_STATUS_BOUND + 1),
                )),
                with_shell(task("b8h2g6f0d", "shell", "")),
                with_shell(task("b8h2g6f0d\u{7}", "shell", "running")),
            ];
            let refused: Vec<u64> = (FIRST_REFUSED..).take(invalid.len()).collect();
            let plan: Vec<Value> = invalid
                .into_iter()
                .zip(&refused)
                .map(|(tasks, sequence)| {
                    json!({"type": SNAPSHOT, "sequence": sequence, "tasks": tasks})
                })
                .collect();
            let running = one(STALE, SubagentPhase::Running);
            let steps = vec![
                client.step(CLAUDE, &[(START, 5, STALE)]),
                client.snapshot(FIRST_VALID, std::slice::from_ref(&shell)),
                Step::Checkpoint(running.clone()),
                client.send(CLAUDE, &Value::Array(plan)),
                Step::Checkpoint(running),
                client.snapshot(VALID, std::slice::from_ref(&shell)),
            ];
            let run = replay(&root, steps, &one(STALE, SubagentPhase::Completed)).await;
            let mut expected = vec![
                Answer::new(START, 5, STALE, true),
                snapshot_answer(FIRST_VALID, true),
            ];
            expected.extend(
                refused
                    .iter()
                    .map(|sequence| snapshot_answer(*sequence, false)),
            );
            expected.push(snapshot_answer(VALID, true));
            assert_eq!(client.answers(), expected);
            assert_eq!(
                log.applied(),
                [
                    Applied {
                        tasks: 1,
                        deferred: 1,
                        ..Applied::default()
                    },
                    Applied {
                        tasks: 1,
                        terminalized: 1,
                        ..Applied::default()
                    },
                ],
                "only the valid snapshots are applied"
            );
            run.finish().await;
        }

        /// A background snapshot delivered after a newer one is ignored: the
        /// newer one stays, and the worker logs `stale_sequence`.
        #[tokio::test]
        async fn a_stale_background_snapshot_is_ignored() {
            let log = EventLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-background-stale-");
            let client = Client::new(&root);
            let steps = vec![turn(vec![
                    session_hook(None),
                    client.send(
                        CLAUDE,
                        &json!([
                            {"type": SNAPSHOT, "sequence": 200, "tasks": [shell_task("s-new", "Newer", "cargo test")]},
                            {"type": SNAPSHOT, "sequence": 100, "tasks": [shell_task("s-old", "Older", "bacon")]},
                        ]),
                    ),
                ],
            )];
            let run = replay_checking(&root, steps, None).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(SNAPSHOT, 200, "", true),
                    Answer::new(SNAPSHOT, 100, "", true),
                ]
            );
            let background = background_of(run.snapshots.last().expect("the final snapshot"));
            let ids: Vec<&str> = background
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect();
            assert_eq!(ids, ["s-new"], "{background:?}");
            let ignored = log.named("worker.background.ignored");
            assert_eq!(ignored.len(), 1, "{}", log.rendered());
            assert_eq!(
                ignored[0].get("background.reason").map(String::as_str),
                Some("stale_sequence")
            );
            assert_eq!(
                ignored[0]
                    .get("background.sequence.reported")
                    .map(String::as_str),
                Some("100")
            );
            run.finish().await;
        }

        /// Invalid optional fields are dropped by the worker, each counted by
        /// reason, while every task and every valid field of the snapshot apply.
        #[tokio::test]
        async fn invalid_background_fields_are_dropped_and_the_rest_of_the_snapshot_applies() {
            let log = EventLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-background-fields-");
            let client = Client::new(&root);
            let too_long = "x".repeat(BACKGROUND_LABEL_MAX_BYTES + 1);
            let steps = vec![turn(vec![
                session_hook(None),
                client.snapshot(
                    10,
                    &[
                        json!({"id": "c-1", "type": "shell", "status": "running", "label": "bad\u{7}label", "program": "cargo"}),
                        json!({"id": "c-2", "type": "shell", "status": "running", "label": too_long, "program": "bacon"}),
                        json!({"id": "c-3", "type": "shell", "status": "running", "label": "ok label", "program": "cargo", "agent_type": "general-purpose"}),
                        json!({"id": "c-4", "type": "subagent", "status": "running", "label": "ok", "agent_type": "bad\u{1}type"}),
                        json!({"id": "c-5", "type": "shell", "status": "running", "label": 7, "program": "./evil"}),
                        json!({"id": "c-6", "type": "x\u{202E}y", "status": "running"}),
                        json!({"id": "c-7", "type": "subagent", "status": "running", "label": "ok", "agent_type": "x\u{202E}y"}),
                    ],
                ),
                ],
            )];
            let run = replay_checking(&root, steps, None).await;
            assert_eq!(client.answers(), [Answer::new(SNAPSHOT, 10, "", true)]);
            let background = background_of(run.snapshots.last().expect("the final snapshot"));
            assert_eq!(background.items.len(), 7, "{background:?}");
            let kept = item(background, "c-7");
            assert_eq!(
                (kept.kind, kept.agent_type.as_deref()),
                (BackgroundKind::Subagent, None)
            );
            let kept = item(background, "c-6");
            assert_eq!(
                (kept.kind, kept.provider_kind.as_deref()),
                (BackgroundKind::Other, None)
            );
            let kept = item(background, "c-1");
            assert_eq!(
                (kept.label.as_deref(), kept.program.as_deref()),
                (None, Some("cargo"))
            );
            let kept = item(background, "c-2");
            assert_eq!(
                (kept.label.as_deref(), kept.program.as_deref()),
                (None, Some("bacon"))
            );
            let kept = item(background, "c-3");
            assert_eq!(
                (
                    kept.label.as_deref(),
                    kept.program.as_deref(),
                    kept.agent_type.as_deref()
                ),
                (Some("ok label"), Some("cargo"), None)
            );
            let kept = item(background, "c-4");
            assert_eq!(
                (kept.kind, kept.label.as_deref(), kept.agent_type.as_deref()),
                (BackgroundKind::Subagent, Some("ok"), None)
            );
            let kept = item(background, "c-5");
            assert_eq!(
                (kept.label.as_deref(), kept.program.as_deref()),
                (None, None)
            );
            let dropped = log.named("worker.background.fields_dropped");
            assert_eq!(dropped.len(), 1, "{}", log.rendered());
            assert_eq!(
                dropped[0]
                    .get("background.dropped.invalid")
                    .map(String::as_str),
                Some("7")
            );
            assert_eq!(
                dropped[0]
                    .get("background.dropped.kind_mismatch")
                    .map(String::as_str),
                Some("1")
            );
            run.finish().await;
        }

        /// Snapshots are ordered by sequence, not by reporter: a nested process's
        /// newer snapshot replaces the outer agent's, and an older snapshot from
        /// the outer process is then refused as `stale_sequence`. The subagent the
        /// outer agent started keeps its row, because the nested reporter never
        /// owned it. The nested process exits after its claim, so the background
        /// it owned is then cleared as `owner_exited`.
        #[tokio::test]
        async fn a_nested_process_replaces_the_background_by_sequence() {
            let log = EventLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-background-ordering-");
            let client = Client::new(&root);
            let steps = vec![turn(vec![
                session_hook(None),
                stop_with(vec![shell_task("shell-1", "Test watch", "cargo test")]),
                claude_hook(&Event::SubagentStart(SubagentEvent {
                    agent_id: "sub-root".to_owned(),
                    agent_type: "general-purpose".to_owned(),
                })),
                Step::Process(vec![
                    client.send(
                        CLAUDE,
                        &json!([{"type": SNAPSHOT, "sequence": "now", "tasks": [shell_task("nested-1", "Nested", "bacon")]}]),
                    ),
                    Step::Checkpoint(one("sub-root", SubagentPhase::Running)),
                ]),
                client.snapshot(20, &[shell_task("outer-late", "Late", "cargo test")]),
            ])];
            let run = replay_checking(&root, steps, None).await;
            let outcomes: Vec<(String, bool)> = client
                .answers()
                .into_iter()
                .map(|answer| (answer.action, answer.ok))
                .collect();
            assert_eq!(
                outcomes,
                [(SNAPSHOT.to_owned(), true), (SNAPSHOT.to_owned(), true)]
            );
            let [nested, ..] = run.snapshots.as_slice() else {
                panic!("one checkpoint and the final rows: {:?}", run.snapshots);
            };
            let ids = |background: &pohunek_worker_protocol::BackgroundSnapshot| -> Vec<String> {
                background
                    .items
                    .iter()
                    .map(|item| item.id.clone())
                    .collect()
            };
            assert_eq!(ids(background_of(nested)), ["nested-1"], "{nested:?}");
            let last = run.snapshots.last().expect("the final snapshot");
            assert!(last.background.is_none(), "{last:?}");
            let ignored = log.named("worker.background.ignored");
            assert_eq!(ignored.len(), 1, "{}", log.rendered());
            assert_eq!(
                ignored[0].get("background.reason").map(String::as_str),
                Some("stale_sequence")
            );
            assert_eq!(
                ignored[0]
                    .get("background.sequence.reported")
                    .map(String::as_str),
                Some("20")
            );
            let reasons: Vec<String> = log
                .named("worker.background.cleared")
                .iter()
                .map(|fields| fields["background.reason"].clone())
                .collect();
            assert_eq!(reasons, ["owner_exited"], "{}", log.rendered());
            run.finish().await;
        }

        /// A far-future background snapshot applies, and the next normal snapshot
        /// of the same agent, ordered after it, replaces it. The sequence mark is
        /// clamped to the worker's clock, so a claim ahead of the clock cannot lock
        /// the view. Without the clamp the mark would sit at the far-future value
        /// and the normal snapshot would be stale.
        #[tokio::test]
        async fn a_far_future_background_snapshot_applies_and_the_next_normal_one_replaces_it() {
            let log = EventLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-background-far-future-");
            let client = Client::new(&root);
            let steps = vec![turn(vec![
                session_hook(None),
                client.send(
                    CLAUDE,
                    &json!([
                        {"type": SNAPSHOT, "sequence": FAR_FUTURE_SEQUENCE, "tasks": [shell_task("far-shell", "Far", "cargo test")]},
                        {"type": SNAPSHOT, "sequence": "now", "tasks": [shell_task("normal", "Normal", "bacon")]},
                    ]),
                ),
            ])];
            let run = replay_checking(&root, steps, None).await;
            let outcomes: Vec<(String, bool)> = client
                .answers()
                .into_iter()
                .map(|answer| (answer.action, answer.ok))
                .collect();
            assert_eq!(
                outcomes,
                [(SNAPSHOT.to_owned(), true), (SNAPSHOT.to_owned(), true)],
                "{}",
                log.rendered()
            );
            let background = background_of(run.snapshots.last().expect("the final snapshot"));
            let ids: Vec<&str> = background
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect();
            assert_eq!(ids, ["normal"], "{background:?}");
            assert_eq!(background.revision, 2, "{background:?}");
            assert!(
                log.named("worker.background.ignored").is_empty(),
                "{}",
                log.rendered()
            );
            run.finish().await;
        }

        /// A far-future identity report and a far-future subagent start are
        /// admitted, as they were before the worker checked sequences against its
        /// clock. A worker-side refusal would silently drop them, so this scenario
        /// fails if one returns.
        #[tokio::test]
        async fn far_future_identity_and_subagent_claims_are_admitted() {
            let log = EventLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-far-future-admitted-");
            let client = Client::new(&root);
            let steps = vec![turn(vec![
                session_hook(None),
                client.send(
                    CLAUDE,
                    &json!([
                        {"type": START, "sequence": FAR_FUTURE_SEQUENCE, "id": "far-sub"},
                        {"type": "identity_report", "sequence": FAR_FUTURE_SEQUENCE},
                    ]),
                ),
            ])];
            let run = replay_checking(&root, steps, None).await;
            let outcomes: Vec<(String, bool)> = client
                .answers()
                .into_iter()
                .map(|answer| (answer.action, answer.ok))
                .collect();
            assert_eq!(
                outcomes,
                [
                    (START.to_owned(), true),
                    ("identity_report".to_owned(), true)
                ],
                "{}",
                log.rendered()
            );
            let last = run.snapshots.last().expect("the final snapshot");
            assert_eq!(
                row(last, "far-sub").phase,
                SubagentPhase::Running,
                "{last:?}"
            );
            assert!(
                log.named("worker.identity.rejected").is_empty(),
                "{}",
                log.rendered()
            );
            run.finish().await;
        }

        /// A far-future subagent start leaves a record mark no later than its
        /// arrival, so the subagent's genuine stop, taken from the host's
        /// monotonic clock afterwards, is still newer and completes it instead
        /// of being stale until the runtime ends.
        #[tokio::test]
        async fn a_far_future_subagent_start_does_not_freeze_its_record() {
            let log = EventLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-far-future-subagent-");
            let client = Client::new(&root);
            let steps = vec![turn(vec![
                session_hook(None),
                client.send(
                    CLAUDE,
                    &json!([
                        {"type": START, "sequence": FAR_FUTURE_SEQUENCE, "id": "far-sub"},
                        {"type": STOP, "sequence": "now", "id": "far-sub"},
                    ]),
                ),
            ])];
            let run = replay_checking(&root, steps, None).await;
            let outcomes: Vec<(String, bool)> = client
                .answers()
                .into_iter()
                .map(|answer| (answer.action, answer.ok))
                .collect();
            assert_eq!(
                outcomes,
                [(START.to_owned(), true), (STOP.to_owned(), true)],
                "{}",
                log.rendered()
            );
            let last = run.snapshots.last().expect("the final snapshot");
            assert_eq!(
                row(last, "far-sub").phase,
                SubagentPhase::Completed,
                "the genuine stop completes the subagent: {last:?}"
            );
            assert!(
                log.named(IGNORED_EVENT).is_empty(),
                "no claim was stale: {}",
                log.rendered()
            );
            run.finish().await;
        }

        /// A snapshot that began before a release and arrives after it is stale:
        /// the release's clear raised the sequence mark past it. Without that,
        /// the late snapshot would restore the background the release ended.
        #[tokio::test]
        async fn a_late_snapshot_ordered_before_a_release_is_stale() {
            let log = EventLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-background-late-stop-");
            let client = Client::new(&root);
            let steps = vec![turn(vec![
                session_hook(None),
                client.snapshot(100, &[shell_task("first", "First", "cargo test")]),
                Step::Hook {
                    asset: state_asset(CLAUDE),
                    action: "release",
                    payload: json!({"hook_event_name": "SessionEnd", "session_id": "native-background"}),
                },
                client.snapshot(110, &[shell_task("late", "Late", "bacon")]),
            ])];
            let run = replay_checking(&root, steps, None).await;
            let final_state = run.snapshots.last().expect("the final snapshot");
            assert!(final_state.background.is_none(), "{final_state:?}");
            let ignored = log.named("worker.background.ignored");
            assert_eq!(ignored.len(), 1, "{}", log.rendered());
            assert_eq!(
                ignored[0].get("background.reason").map(String::as_str),
                Some("stale_sequence")
            );
            assert_eq!(
                ignored[0]
                    .get("background.sequence.reported")
                    .map(String::as_str),
                Some("110")
            );
            assert_eq!(
                log.named("worker.background.cleared").len(),
                1,
                "{}",
                log.rendered()
            );
            run.finish().await;
        }

        /// A snapshot listing more than `BACKGROUND_MAX_ITEMS` tasks does not
        /// replace the previous background, and the worker logs
        /// `too_many_items`. The reconciliation still applies to it.
        #[tokio::test]
        async fn an_item_list_over_the_bound_keeps_the_previous_background() {
            let log = EventLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-background-bound-");
            let client = Client::new(&root);
            let many: Vec<Value> = (0..=BACKGROUND_MAX_ITEMS)
                .map(|index| shell_task(&format!("many-{index}"), "Watch", "bacon"))
                .collect();
            let steps = vec![turn(vec![
                session_hook(None),
                client.snapshot(10, &[shell_task("keep-1", "Kept", "cargo test")]),
                client.snapshot(20, &many),
            ])];
            let run = replay_checking(&root, steps, None).await;
            assert_eq!(
                client.answers(),
                [
                    Answer::new(SNAPSHOT, 10, "", true),
                    Answer::new(SNAPSHOT, 20, "", true),
                ]
            );
            let background = background_of(run.snapshots.last().expect("the final snapshot"));
            let ids: Vec<&str> = background
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect();
            assert_eq!(ids, ["keep-1"], "{background:?}");
            let ignored = log.named("worker.background.ignored");
            assert_eq!(ignored.len(), 1, "{}", log.rendered());
            assert_eq!(
                ignored[0].get("background.reason").map(String::as_str),
                Some("too_many_items")
            );
            assert_eq!(
                ignored[0].get("background.items").map(String::as_str),
                Some((BACKGROUND_MAX_ITEMS + 1).to_string().as_str())
            );
            run.finish().await;
        }
    }

    /// Hook failure trace scenarios: the real state hooks run inside a real
    /// worker's PTY and append to the trace file the worker hands them as
    /// `POHUNEK_HOOK_TRACE_PATH`.
    ///
    /// Expected lines come from the trace decided on #883 (intake comment,
    /// "D5 diagnostics") and its S3 slice: one JSON line per traced failure
    /// exit, holding `ts`, `session`, `provider`, `action` and `reason` and at
    /// most a short `detail` code, never payload content; the file is
    /// owner-private, never written through a symlink, and rotated once at
    /// `POHUNEK_HOOK_TRACE_MAX_BYTES`. A hook exits 0 without output either
    /// way.
    ///
    /// Two points the decision leaves open are fixed here: a line's `action`
    /// is the hook's action argument, and the `no_snapshot` sub-reason is its
    /// `detail`.
    mod hook_trace {
        use std::collections::BTreeSet;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::path::{Path, PathBuf};

        use serde_json::{json, Value};

        use super::{
            quoted, recorded, replay, state_asset, stop_tasks, Rows, Step, CLAUDE, CODEX,
            OTHER_INPUT_BOUND, SESSION_ID,
        };

        /// Hook action of a subagent start, named as the line's `action`.
        const SUBAGENT_START: &str = "subagent-start";

        /// Hook action of a subagent stop.
        const SUBAGENT_STOP: &str = "subagent-stop";

        /// Hook action of the main agent's `Stop`, named as the line's
        /// `action`.
        const STOP: &str = "stop";

        /// Reason of a claim the worker answered `ok: false`.
        const WORKER_REFUSED: &str = "worker_refused";

        /// Reason of an input over the action's input bound.
        const INPUT_TOO_LARGE: &str = "input_too_large";

        /// Reason of an input that is not a JSON object.
        const INPUT_INVALID: &str = "input_invalid";

        /// Reason of a main `Stop` the hook sends no snapshot for.
        const NO_SNAPSHOT: &str = "no_snapshot";

        /// `no_snapshot` detail of a task list with an invalid entry.
        const TASKS_INVALID: &str = "tasks_invalid";

        /// Reasons the decision allows for a worker socket path that does not
        /// exist: the hook either finds the path missing or fails to connect.
        const SOCKET_ABSENT: &[&str] = &["worker_socket_missing", "worker_unreachable"];

        /// Fields every trace line carries.
        const LINE_FIELDS: [&str; 5] = ["ts", "session", "provider", "action", "reason"];

        /// The one optional field: a short fixed code, never input text.
        const DETAIL: &str = "detail";

        /// Longest trace line with its newline, the bound the hook enforces.
        /// Lines of concurrent hooks stay apart because each is one write
        /// under the hook's exclusive trace lock.
        const LINE_BOUND: usize = 512;

        /// Rotation bound the rotation scenario hands the hook: a longest line
        /// still fits an empty file, and a few real lines fill it.
        const SMALL_MAX_BYTES: usize = LINE_BOUND;

        /// Failures of the rotation scenario: several times the lines that
        /// fit [`SMALL_MAX_BYTES`] twice, so the trace rotates repeatedly and
        /// drops its oldest lines.
        const ROTATION_FAILURES: usize = 20;

        /// Hook input that is not JSON.
        const NOT_JSON: &[u8] = b"{";

        /// A subagent id longer than the worker's `MAX_SUBAGENT_ID_BYTES`
        /// (256). The hooks forward any non-empty id, so the worker refuses
        /// the claim.
        fn overlong_id() -> String {
            format!("a{}", "7f3c9e1b".repeat(33))
        }

        /// What one trace line says.
        struct Expected {
            provider: &'static str,
            action: &'static str,
            /// The line's reason is one of these.
            reasons: &'static [&'static str],
            /// The line's `detail`, when the decision fixes one.
            detail: Option<&'static str>,
        }

        impl Expected {
            const fn new(
                provider: &'static str,
                action: &'static str,
                reasons: &'static [&'static str],
            ) -> Self {
                Self {
                    provider,
                    action,
                    reasons,
                    detail: None,
                }
            }
        }

        /// A step running the state hook `asset` with `action`, `input` on its
        /// stdin, in a subshell that runs `prelude` first. The hook's output
        /// goes to `<name>.out` and its exit status to `<name>.status`.
        ///
        /// The subshell execs the hook, so the hook's parent is still the PTY
        /// root.
        fn traced(
            root: &Path,
            name: &str,
            prelude: &str,
            asset: &Path,
            action: &str,
            input: &[u8],
        ) -> Step {
            let input_path = root.join(format!("{name}.input"));
            std::fs::write(&input_path, input).expect("write the hook input");
            Step::Command(format!(
                "({prelude} exec /bin/sh {asset} {action} < {input} > {out} 2>&1); echo \"$?\" > {status}",
                asset = quoted(asset),
                input = quoted(&input_path),
                out = quoted(&root.join(format!("{name}.out"))),
                status = quoted(&root.join(format!("{name}.status"))),
            ))
        }

        /// Asserts that the hook run `name` exited 0 and printed nothing.
        fn assert_silent(root: &Path, name: &str) {
            let status = std::fs::read_to_string(root.join(format!("{name}.status")))
                .expect("the hook's exit status");
            assert_eq!(status.trim(), "0", "{name}: the hook exits 0");
            let output =
                std::fs::read(root.join(format!("{name}.out"))).expect("the hook's output");
            assert!(
                output.is_empty(),
                "{name}: the hook prints nothing: {:?}",
                String::from_utf8_lossy(&output)
            );
        }

        /// The trace lines of `path`, none when it does not exist. Every line
        /// is complete, within [`LINE_BOUND`], and a JSON value.
        fn trace_lines(path: &Path) -> Vec<Value> {
            let text = match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
                Err(error) => panic!("read {}: {error}", path.display()),
            };
            assert!(
                text.is_empty() || text.ends_with('\n'),
                "every trace line is complete: {text:?}"
            );
            text.lines()
                .map(|line| {
                    assert!(
                        line.len() < LINE_BOUND,
                        "a trace line with its newline stays within the line bound: {line}"
                    );
                    serde_json::from_str(line)
                        .unwrap_or_else(|error| panic!("a trace line is JSON ({error}): {line}"))
                })
                .collect()
        }

        /// Asserts that `line` says what `expected` says, for this session.
        fn assert_line(line: &Value, expected: &Expected) {
            let fields = line
                .as_object()
                .unwrap_or_else(|| panic!("a trace line is an object: {line}"));
            let keys: BTreeSet<&str> = fields.keys().map(String::as_str).collect();
            let required = BTreeSet::from(LINE_FIELDS);
            assert!(keys.is_superset(&required), "{line}");
            assert!(
                keys.difference(&required).all(|key| *key == DETAIL),
                "a line carries no other field: {line}"
            );
            assert_eq!(line["session"], json!(SESSION_ID), "{line}");
            assert_eq!(line["provider"], json!(expected.provider), "{line}");
            assert_eq!(line["action"], json!(expected.action), "{line}");
            let reason = line["reason"].as_str().unwrap_or_default();
            assert!(
                expected.reasons.contains(&reason),
                "reason is one of {:?}: {line}",
                expected.reasons
            );
            let ts = line["ts"]
                .as_str()
                .unwrap_or_else(|| panic!("ts is text: {line}"));
            let at =
                ::time::OffsetDateTime::parse(ts, &::time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|error| panic!("ts is RFC 3339 ({error}): {line}"));
            assert!(at.offset().is_utc(), "ts is UTC: {line}");
            match (expected.detail, line.get(DETAIL)) {
                (Some(detail), found) => assert_eq!(found, Some(&json!(detail)), "{line}"),
                (None, None) => {}
                (None, Some(found)) => assert!(
                    found.as_str().is_some_and(|code| !code.is_empty()
                        && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')),
                    "detail is a short fixed code: {line}"
                ),
            }
        }

        /// Asserts that `path` is a regular file of the current user with mode
        /// 0600.
        fn assert_private(path: &Path) {
            let metadata = std::fs::symlink_metadata(path)
                .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()));
            assert!(
                metadata.file_type().is_file(),
                "{} is a regular file",
                path.display()
            );
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                0o600,
                "{} is owner-private",
                path.display()
            );
            assert_eq!(
                metadata.uid(),
                nix::unistd::getuid().as_raw(),
                "{} is ours",
                path.display()
            );
        }

        /// Whether nothing, not even a dangling symlink, exists at `path`.
        fn absent(path: &Path) -> bool {
            matches!(
                std::fs::symlink_metadata(path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            )
        }

        /// Asserts that the trace text at `path` holds none of `canaries`.
        fn assert_no_leak(path: &Path, canaries: &[String]) {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            for canary in canaries.iter().filter(|canary| !canary.is_empty()) {
                assert!(
                    !text.contains(canary.as_str()),
                    "{canary:?} leaks into the trace: {text}"
                );
            }
        }

        /// The string values of `payload`, nested ones included, except the
        /// event name, the permission mode and a task's type and status: a
        /// trace line holds none of them. Those fields hold short generic
        /// words that are no evidence of a leak.
        fn payload_text(payload: &Value) -> Vec<String> {
            const GENERIC: [&str; 4] = ["hook_event_name", "permission_mode", "type", "status"];
            match payload {
                Value::String(text) => vec![text.clone()],
                Value::Array(items) => items.iter().flat_map(payload_text).collect(),
                Value::Object(fields) => fields
                    .iter()
                    .filter(|(key, _)| !GENERIC.contains(&key.as_str()))
                    .flat_map(|(_, value)| payload_text(value))
                    .collect(),
                _ => Vec::new(),
            }
        }

        /// A Claude `SubagentStart` input naming `agent_id`.
        fn claude_start(agent_id: &str) -> Value {
            json!({
                "hook_event_name": "SubagentStart",
                "agent_id": agent_id,
                "agent_type": "general-purpose",
            })
        }

        /// The recorded Codex hook input `event` (`start` or `stop`).
        fn codex_input(event: &str) -> Value {
            let fixture: Value =
                serde_json::from_str(crate::integration::CODEX_SUBAGENT_COMPATIBILITY_FIXTURE)
                    .expect("the Codex hook fixture parses");
            fixture[event].clone()
        }

        /// `payload` padded past the subagent actions' input bound,
        /// `MAX_HOOK_INPUT_BYTES` of both state hooks.
        fn oversized(mut payload: Value) -> Value {
            payload["padding"] = json!("x".repeat(OTHER_INPUT_BOUND));
            payload
        }

        /// One failure exit and the line it must leave.
        struct Failure {
            name: &'static str,
            prelude: String,
            asset: PathBuf,
            action: &'static str,
            payload: Value,
            expected: Expected,
        }

        /// The failure exits of test item 1, in the order the PTY runs them:
        /// a claim the worker refuses and an input over the bound for both
        /// providers, a main `Stop` with an invalid task list, and a worker
        /// socket that does not exist under `root`.
        fn item_one_failures(root: &Path) -> [Failure; 6] {
            let shell = stop_tasks(&recorded("never_listed_agent_with_only_shells"), 1)[0].clone();
            let mut empty_id = shell.clone();
            empty_id["id"] = json!("");
            let mut codex_refused = codex_input("start");
            codex_refused["agent_id"] = json!(overlong_id());
            let absent_socket = root.join("absent-worker.sock");
            [
                Failure {
                    name: "claude-refused",
                    prelude: String::new(),
                    asset: state_asset(CLAUDE),
                    action: SUBAGENT_START,
                    payload: claude_start(&overlong_id()),
                    expected: Expected::new(CLAUDE, SUBAGENT_START, &[WORKER_REFUSED]),
                },
                Failure {
                    name: "codex-refused",
                    prelude: String::new(),
                    asset: state_asset(CODEX),
                    action: SUBAGENT_START,
                    payload: codex_refused,
                    expected: Expected::new(CODEX, SUBAGENT_START, &[WORKER_REFUSED]),
                },
                Failure {
                    name: "claude-too-large",
                    prelude: String::new(),
                    asset: state_asset(CLAUDE),
                    action: SUBAGENT_START,
                    payload: oversized(claude_start("a5e9c3f7b1d4a8e2c")),
                    expected: Expected::new(CLAUDE, SUBAGENT_START, &[INPUT_TOO_LARGE]),
                },
                Failure {
                    name: "codex-too-large",
                    prelude: String::new(),
                    asset: state_asset(CODEX),
                    action: SUBAGENT_STOP,
                    payload: oversized(codex_input("stop")),
                    expected: Expected::new(CODEX, SUBAGENT_STOP, &[INPUT_TOO_LARGE]),
                },
                Failure {
                    name: "tasks-invalid",
                    prelude: String::new(),
                    asset: state_asset(CLAUDE),
                    action: STOP,
                    payload: json!({
                        "hook_event_name": "Stop",
                        "last_assistant_message": "The build watcher is still running.",
                        "background_tasks": [shell, empty_id],
                    }),
                    expected: Expected {
                        detail: Some(TASKS_INVALID),
                        ..Expected::new(CLAUDE, STOP, &[NO_SNAPSHOT])
                    },
                },
                Failure {
                    name: "socket-absent",
                    prelude: format!(
                        "export {}={};",
                        protocol::ENV_WORKER_SOCKET_PATH,
                        quoted(&absent_socket)
                    ),
                    asset: state_asset(CLAUDE),
                    action: SUBAGENT_START,
                    payload: claude_start("a1f5d9b3e7c2a6f0d"),
                    expected: Expected::new(CLAUDE, SUBAGENT_START, SOCKET_ABSENT),
                },
            ]
        }

        /// Each failure exit of test item 1 writes exactly one line, in the
        /// order the hooks ran. The file is owner-private and holds no input
        /// text, path or environment value.
        #[tokio::test(flavor = "multi_thread")]
        async fn each_failure_exit_writes_one_trace_line() {
            let root = crate::test_support::scoped_dir("ph-trace-failures-");
            let failures = item_one_failures(&root);
            let steps = failures
                .iter()
                .map(|failure| {
                    traced(
                        &root,
                        failure.name,
                        &failure.prelude,
                        &failure.asset,
                        failure.action,
                        failure.payload.to_string().as_bytes(),
                    )
                })
                .collect();
            let run = replay(&root, steps, &Rows::new()).await;
            let instance = run
                .worker
                .worker_instance_id()
                .await
                .expect("worker instance")
                .to_string();
            run.finish().await;

            let trace = super::super::hook_trace_path(&root);
            let lines = trace_lines(&trace);
            assert_eq!(
                lines.len(),
                failures.len(),
                "one line per failure exit: {lines:?}"
            );
            for (line, failure) in lines.iter().zip(&failures) {
                assert_silent(&root, failure.name);
                assert_line(line, &failure.expected);
            }
            assert_private(&trace);
            let mut canaries: Vec<String> = failures
                .iter()
                .flat_map(|failure| payload_text(&failure.payload))
                .collect();
            canaries.extend([root.display().to_string(), instance]);
            assert_no_leak(&trace, &canaries);
        }

        /// Runs real hooks against a bound peer and verifies every explicit ACK.
        ///
        /// The peer has no durability work; row application belongs to the real
        /// worker replay scenarios, not this transport-to-trace assertion.
        async fn assert_acknowledged_hooks(
            root: &Path,
            steps: Vec<Step>,
            expected: &[(&str, &str, Option<&str>)],
        ) {
            let socket = root.join("ack.sock");
            let listener = tokio::net::UnixListener::bind(&socket).expect("bind ACK peer");
            let mut peer = tokio::task::JoinSet::new();
            let request_count = expected.len();
            peer.spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

                let mut requests = Vec::new();
                for _ in 0..request_count {
                    let (stream, _) = listener.accept().await.expect("accept hook");
                    let (read, mut write) = stream.into_split();
                    let mut line = String::new();
                    BufReader::new(read)
                        .read_line(&mut line)
                        .await
                        .expect("read hook request");
                    requests.push(serde_json::from_str::<Value>(&line).expect("JSON request"));
                    write
                        .write_all(b"{\"ok\":true}\n")
                        .await
                        .expect("send explicit ACK");
                }
                requests
            });
            let commands = steps
                .into_iter()
                .map(|step| match step {
                    Step::Command(command) => command,
                    _ => panic!("traced steps are shell commands"),
                })
                .collect::<Vec<_>>()
                .join("\n");
            let output = pohunek_test_support::wait::guard(
                "acknowledged hooks to finish",
                tokio::process::Command::new("/bin/sh")
                    .args(["-c", &commands])
                    .current_dir(root)
                    .env_clear()
                    .env("PATH", crate::integration::tests::inherited_path())
                    .env("TMPDIR", root)
                    .env("POHUNEK_ENV", "1")
                    .env("POHUNEK_SESSION_ID", SESSION_ID)
                    .env(
                        "POHUNEK_PROTOCOL_VERSION",
                        protocol::PROTOCOL_VERSION.get().to_string(),
                    )
                    .env("POHUNEK_WORKER_SOCKET_PATH", &socket)
                    .env("POHUNEK_WORKER_INSTANCE_ID", "runtime-trace-ack")
                    .env(
                        "POHUNEK_HOOK_TRACE_PATH",
                        super::super::hook_trace_path(root),
                    )
                    .env(
                        "POHUNEK_HOOK_TRACE_MAX_BYTES",
                        pohunek_logging::config::HOOK_TRACE_MAX_FILE_BYTES.to_string(),
                    )
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("run hook sequence");
            assert!(output.status.success());
            assert!(output.stdout.is_empty() && output.stderr.is_empty());
            let requests = pohunek_test_support::wait::guard("all hook ACKs", peer.join_next())
                .await
                .expect("peer task exists")
                .expect("peer task succeeds");
            assert_eq!(requests.len(), expected.len());
            for (request, (provider, kind, id)) in requests.iter().zip(expected.iter().copied()) {
                assert_eq!(request["provider"], provider);
                assert_eq!(request["type"], kind);
                assert_eq!(request["runtime_id"], "runtime-trace-ack");
                assert!(request["pid"].as_u64().is_some_and(|pid| pid > 1));
                assert!(request["start_identity"]
                    .as_u64()
                    .is_some_and(|start| start > 0));
                assert!(request["sequence"]
                    .as_u64()
                    .is_some_and(|sequence| sequence > 0));
                if let Some(id) = id {
                    assert_eq!(request["subagent_id"], id);
                } else {
                    assert_eq!(request["tasks"], json!([]));
                }
                if kind == "subagent_stop" {
                    assert_eq!(request["outcome"], "completed");
                }
            }
        }

        /// Test item 2: received acknowledgements write no trace line.
        ///
        /// A protocol peer explicitly acknowledges the five real hook requests.
        /// This tests transport and tracing without requiring a durable worker
        /// commit to complete within the production hook deadline. Real-worker
        /// replay scenarios separately verify admission and journaled rows.
        #[tokio::test(flavor = "multi_thread")]
        async fn acknowledged_claims_write_no_trace_line() {
            let root = crate::test_support::scoped_dir("ph-trace-success-");
            let sequence = recorded("spawn_handback_resume");
            let started = sequence.events[0].subagent();
            let claude_payload = json!({
                "hook_event_name": "SubagentStart",
                "agent_id": started.agent_id,
                "agent_type": started.agent_type,
            });
            let mut claude_stop = claude_payload.clone();
            claude_stop["hook_event_name"] = json!("SubagentStop");
            let codex_id = codex_input("start")["agent_id"]
                .as_str()
                .expect("the Codex start names its agent")
                .to_owned();
            let claude = state_asset(CLAUDE);
            let codex = state_asset(CODEX);
            let steps = vec![
                traced(
                    &root,
                    "claude-start",
                    "",
                    &claude,
                    SUBAGENT_START,
                    claude_payload.to_string().as_bytes(),
                ),
                traced(
                    &root,
                    "claude-stop",
                    "",
                    &claude,
                    SUBAGENT_STOP,
                    claude_stop.to_string().as_bytes(),
                ),
                traced(
                    &root,
                    "codex-start",
                    "",
                    &codex,
                    SUBAGENT_START,
                    codex_input("start").to_string().as_bytes(),
                ),
                traced(
                    &root,
                    "codex-stop",
                    "",
                    &codex,
                    SUBAGENT_STOP,
                    codex_input("stop").to_string().as_bytes(),
                ),
                traced(
                    &root,
                    "main-stop",
                    "",
                    &claude,
                    STOP,
                    json!({"hook_event_name": "Stop", "background_tasks": []})
                        .to_string()
                        .as_bytes(),
                ),
                traced(&root, "control", "", &claude, SUBAGENT_START, NOT_JSON),
            ];
            assert_acknowledged_hooks(
                &root,
                steps,
                &[
                    (CLAUDE, "subagent_start", Some(started.agent_id.as_str())),
                    (CLAUDE, "subagent_stop", Some(started.agent_id.as_str())),
                    (CODEX, "subagent_start", Some(codex_id.as_str())),
                    (CODEX, "subagent_stop", Some(codex_id.as_str())),
                    (CLAUDE, "subagent_snapshot", None),
                ],
            )
            .await;

            for name in [
                "claude-start",
                "claude-stop",
                "codex-start",
                "codex-stop",
                "main-stop",
                "control",
            ] {
                assert_silent(&root, name);
            }
            let lines = trace_lines(&super::super::hook_trace_path(&root));
            let [line] = lines.as_slice() else {
                panic!("only the failure exit writes a line: {lines:?}");
            };
            assert_line(
                line,
                &Expected::new(CLAUDE, SUBAGENT_START, &[INPUT_INVALID]),
            );
        }

        /// Test item 3: a symlink at the trace path is never followed, whether
        /// it points at a file or at nothing: the target stays as it was, the
        /// hook exits 0 without output, and nothing rotates. Once the symlink
        /// is gone, the next failure exit writes its line to a regular file.
        ///
        /// The existing target is an owner-private regular file, a valid
        /// trace in every respect but the link to it.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_symlink_at_the_trace_path_is_never_followed() {
            /// Content of the symlink's existing target.
            const KEPT: &str = "kept\n";
            for existing in [true, false] {
                let root = crate::test_support::scoped_dir("ph-trace-symlink-");
                let trace = super::super::hook_trace_path(&root);
                let target = root.join("symlink-target.jsonl");
                if existing {
                    std::fs::write(&target, KEPT).expect("write the symlink target");
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
                        .expect("make the symlink target owner-private");
                }
                std::os::unix::fs::symlink(&target, &trace).expect("link the trace path");
                let claude = state_asset(CLAUDE);
                let steps = vec![
                    traced(&root, "through-link", "", &claude, SUBAGENT_START, NOT_JSON),
                    Step::Command(format!("rm -f {}", quoted(&trace))),
                    traced(&root, "after-unlink", "", &claude, SUBAGENT_START, NOT_JSON),
                ];
                replay(&root, steps, &Rows::new()).await.finish().await;

                assert_silent(&root, "through-link");
                assert_silent(&root, "after-unlink");
                if existing {
                    assert_eq!(
                        std::fs::read_to_string(&target).expect("read the symlink target"),
                        KEPT,
                        "the existing target is unchanged"
                    );
                } else {
                    assert!(absent(&target), "the dangling target is never created");
                }
                assert!(
                    absent(Path::new(&format!("{}.1", trace.display()))),
                    "nothing rotates through the symlink"
                );
                let lines = trace_lines(&trace);
                let [line] = lines.as_slice() else {
                    panic!("only the failure after the unlink writes a line: {lines:?}");
                };
                assert_line(
                    line,
                    &Expected::new(CLAUDE, SUBAGENT_START, &[INPUT_INVALID]),
                );
                assert_private(&trace);
            }
        }

        /// Test item 4: with a small rotation bound, neither the trace nor its
        /// one rotated file ever exceeds the bound across many failure exits,
        /// the oldest lines are dropped, and exactly one rotated file exists.
        ///
        /// After each failure the PTY script appends the sizes of the trace
        /// and of its rotated file, so the bound is checked at every step.
        #[tokio::test(flavor = "multi_thread")]
        async fn the_trace_rotates_once_within_its_bound() {
            let root = crate::test_support::scoped_dir("ph-trace-rotation-");
            let trace = super::super::hook_trace_path(&root);
            let rotated = PathBuf::from(format!("{}.1", trace.display()));
            let sizes = root.join("trace-sizes");
            let prelude = format!(
                "export {}={SMALL_MAX_BYTES};",
                protocol::ENV_HOOK_TRACE_MAX_BYTES
            );
            let measure = format!(
                "c=0; r=0; [ -f {trace} ] && c=$(wc -c < {trace}); [ -f {rotated} ] && r=$(wc -c < {rotated}); echo \"$c $r\" >> {sizes}",
                trace = quoted(&trace),
                rotated = quoted(&rotated),
                sizes = quoted(&sizes),
            );
            let claude = state_asset(CLAUDE);
            let names: Vec<String> = (0..ROTATION_FAILURES)
                .map(|index| format!("rotate-{index}"))
                .collect();
            let steps = names
                .iter()
                .flat_map(|name| {
                    [
                        traced(&root, name, &prelude, &claude, SUBAGENT_START, NOT_JSON),
                        Step::Command(measure.clone()),
                    ]
                })
                .collect();
            replay(&root, steps, &Rows::new()).await.finish().await;

            for name in &names {
                assert_silent(&root, name);
            }
            let measured: Vec<(usize, usize)> = std::fs::read_to_string(&sizes)
                .expect("the measured sizes")
                .lines()
                .map(|line| {
                    let mut sizes = line
                        .split_whitespace()
                        .map(|size| size.parse::<usize>().expect("a measured size"));
                    (
                        sizes.next().expect("the trace size"),
                        sizes.next().expect("the rotated size"),
                    )
                })
                .collect();
            assert_eq!(measured.len(), ROTATION_FAILURES, "{measured:?}");
            assert!(
                measured
                    .iter()
                    .all(|(current, old)| (1..=SMALL_MAX_BYTES).contains(current) && *old <= SMALL_MAX_BYTES),
                "after every failure the trace holds its line and both files keep the bound: {measured:?}"
            );

            let family: BTreeSet<String> = std::fs::read_dir(&root)
                .expect("list the scenario directory")
                .map(|entry| {
                    entry
                        .expect("a directory entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .filter(|name| name.starts_with(pohunek_logging::config::HOOK_TRACE_FILE_NAME))
                .collect();
            let file_name = pohunek_logging::config::HOOK_TRACE_FILE_NAME;
            assert_eq!(
                family,
                BTreeSet::from([file_name.to_owned(), format!("{file_name}.1")]),
                "the trace and exactly one rotated file"
            );
            let (current, old) = (trace_lines(&trace), trace_lines(&rotated));
            assert!(!old.is_empty(), "the trace rotated");
            assert!(
                current.len() + old.len() < ROTATION_FAILURES,
                "the oldest lines are dropped: {} + {}",
                current.len(),
                old.len()
            );
            for line in current.iter().chain(&old) {
                assert_line(
                    line,
                    &Expected::new(CLAUDE, SUBAGENT_START, &[INPUT_INVALID]),
                );
            }
            assert_private(&trace);
            assert_private(&rotated);
        }

        /// Lock holder: `<trace> <ready fifo> <release fifo>`.
        ///
        /// It creates the trace owner-private, holds an exclusive `flock` on
        /// it, writes `locked` to the ready FIFO, and keeps the lock until it
        /// reads a line from the release FIFO. A failure is written to the
        /// ready FIFO instead, so the PTY script never waits on a dead holder.
        const LOCK_HOLDER: &str = r#"
import fcntl
import os
import sys

trace, ready, release = sys.argv[1:4]
try:
    descriptor = os.open(trace, os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    fcntl.flock(descriptor, fcntl.LOCK_EX)
except BaseException as error:
    with open(ready, "w", encoding="ascii") as handle:
        handle.write(f"failed:{type(error).__name__}\n")
    raise
with open(ready, "w", encoding="ascii") as handle:
    handle.write("locked\n")
with open(release, encoding="ascii") as handle:
    handle.readline()
"#;

        /// A trace another process holds an exclusive lock on is left alone:
        /// a failing Claude or Codex hook exits 0 without output and writes
        /// no line, and once the lock is released the next failure writes
        /// exactly one line.
        ///
        /// The PTY script starts the holder, waits on the ready FIFO until it
        /// holds the lock, runs the failing hooks, then releases the holder
        /// through the release FIFO and waits for it to exit. Each hook must
        /// give up on the lock for the script to reach the next barrier
        /// within the hang guard.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_locked_trace_is_skipped_without_blocking_the_hook() {
            let root = crate::test_support::scoped_dir("ph-trace-locked-");
            let trace = super::super::hook_trace_path(&root);
            let holder = root.join("lock-holder.py");
            std::fs::write(&holder, LOCK_HOLDER).expect("write the lock holder");
            let (ready, release) = (root.join("lock-ready.fifo"), root.join("lock-release.fifo"));
            for fifo in [&ready, &release] {
                nix::unistd::mkfifo(fifo, nix::sys::stat::Mode::S_IRWXU)
                    .expect("create a lock FIFO");
            }
            let (held, holder_status) = (root.join("lock-held"), root.join("lock-holder.status"));
            let start_holder = format!(
                "python3 -I {holder} {trace} {ready} {release} & holder=$!; read -r held < {ready}; echo \"$held\" > {held}",
                holder = quoted(&holder),
                trace = quoted(&trace),
                ready = quoted(&ready),
                release = quoted(&release),
                held = quoted(&held),
            );
            let release_holder = format!(
                "[ \"$held\" = locked ] && echo release > {release}; wait \"$holder\"; echo \"$?\" > {status}",
                release = quoted(&release),
                status = quoted(&holder_status),
            );
            let steps = vec![
                Step::Command(start_holder),
                traced(
                    &root,
                    "claude-locked",
                    "",
                    &state_asset(CLAUDE),
                    SUBAGENT_START,
                    NOT_JSON,
                ),
                traced(
                    &root,
                    "codex-locked",
                    "",
                    &state_asset(CODEX),
                    SUBAGENT_START,
                    NOT_JSON,
                ),
                Step::Command(release_holder),
                traced(
                    &root,
                    "unlocked",
                    "",
                    &state_asset(CLAUDE),
                    SUBAGENT_START,
                    NOT_JSON,
                ),
            ];
            replay(&root, steps, &Rows::new()).await.finish().await;

            assert_eq!(
                std::fs::read_to_string(&held)
                    .expect("the holder's report")
                    .trim(),
                "locked",
                "the holder held the lock while the hooks ran"
            );
            assert_eq!(
                std::fs::read_to_string(&holder_status)
                    .expect("the holder's exit status")
                    .trim(),
                "0"
            );
            for name in ["claude-locked", "codex-locked", "unlocked"] {
                assert_silent(&root, name);
            }
            let lines = trace_lines(&trace);
            let [line] = lines.as_slice() else {
                panic!("only the failure after the release writes a line: {lines:?}");
            };
            assert_line(
                line,
                &Expected::new(CLAUDE, SUBAGENT_START, &[INPUT_INVALID]),
            );
            assert_private(&trace);
        }

        /// Hook action of a submitted prompt, named as the line's `action`.
        const PROMPT_SUBMIT: &str = "prompt-submit";

        /// Reason of a worker that closed the connection without an answer.
        const WORKER_CLOSED: &str = "worker_closed";

        /// A prompt well past the input bound of every other action.
        ///
        /// The prompt-submit action never decodes its input, so the bound does
        /// not apply and the report still goes out.
        fn long_prompt() -> Value {
            json!({
                "hook_event_name": "UserPromptSubmit",
                "prompt": "x".repeat(OTHER_INPUT_BOUND * 3),
            })
        }

        /// The prompt-submit hook is the one hook that runs before every
        /// prompt of every Claude on the host, and its output reaches the
        /// model. Whatever fails, it exits 0 with no output, leaves one trace
        /// line for a failure in a pohunek session, none outside it, and the
        /// Codex hooks have no such action. A worker that predates the report
        /// closes the connection on the unknown request type.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_prompt_submit_hook_is_silent_and_traced_on_every_failure_exit() {
            let root = crate::test_support::scoped_dir("ph-trace-prompt-");
            let old_worker = root.join("old-worker.sock");
            let listener = tokio::net::UnixListener::bind(&old_worker).expect("bind old worker");
            tokio::spawn(async move {
                while let Ok((stream, _addr)) = listener.accept().await {
                    let mut line = String::new();
                    let _ = tokio::io::AsyncBufReadExt::read_line(
                        &mut tokio::io::BufReader::new(stream),
                        &mut line,
                    )
                    .await;
                }
            });
            let absent_socket = root.join("absent-worker.sock");
            let claude = state_asset(CLAUDE);
            let prompt = long_prompt().to_string();
            let steps = vec![
                traced(
                    &root,
                    "refused",
                    "",
                    &claude,
                    PROMPT_SUBMIT,
                    prompt.as_bytes(),
                ),
                traced(
                    &root,
                    "absent",
                    &format!(
                        "export {}={};",
                        protocol::ENV_WORKER_SOCKET_PATH,
                        quoted(&absent_socket)
                    ),
                    &claude,
                    PROMPT_SUBMIT,
                    prompt.as_bytes(),
                ),
                traced(
                    &root,
                    "old-worker",
                    &format!(
                        "export {}={};",
                        protocol::ENV_WORKER_SOCKET_PATH,
                        quoted(&old_worker)
                    ),
                    &claude,
                    PROMPT_SUBMIT,
                    prompt.as_bytes(),
                ),
                traced(
                    &root,
                    "outside-session",
                    &format!("unset {};", protocol::ENV_FLAG),
                    &claude,
                    PROMPT_SUBMIT,
                    prompt.as_bytes(),
                ),
                traced(
                    &root,
                    "codex",
                    "",
                    &state_asset(CODEX),
                    PROMPT_SUBMIT,
                    prompt.as_bytes(),
                ),
            ];
            let run = replay(&root, steps, &Rows::new()).await;
            let snapshot = run.snapshots.last().expect("final snapshot").clone();
            run.finish().await;

            for name in [
                "refused",
                "absent",
                "old-worker",
                "outside-session",
                "codex",
            ] {
                assert_silent(&root, name);
            }
            assert_eq!(snapshot.prompt_accepted_sequence, None);
            let lines = trace_lines(&super::super::hook_trace_path(&root));
            let [refused, absent, closed] = lines.as_slice() else {
                panic!("one line per failure inside the session: {lines:?}");
            };
            assert_line(
                refused,
                &Expected::new(CLAUDE, PROMPT_SUBMIT, &[WORKER_REFUSED]),
            );
            assert_line(absent, &Expected::new(CLAUDE, PROMPT_SUBMIT, SOCKET_ABSENT));
            assert_line(
                closed,
                &Expected::new(CLAUDE, PROMPT_SUBMIT, &[WORKER_CLOSED]),
            );
            let canaries = vec![root.display().to_string(), "x".repeat(64)];
            assert_no_leak(&super::super::hook_trace_path(&root), &canaries);
        }

        /// Test item 5 and the enabling rule: a failure exit writes nothing
        /// when either trace variable is absent, when the path is relative, or
        /// when the bound is not a positive integer, nor outside a pohunek
        /// session; the hook still exits 0 without output. The same failure
        /// with the worker's variables writes the only line.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_failure_without_valid_trace_variables_writes_nothing() {
            /// Relative trace path, resolved against the PTY's working
            /// directory, which is the scenario's directory.
            const RELATIVE: &str = "relative-hooks.jsonl";
            let root = crate::test_support::scoped_dir("ph-trace-disabled-");
            let (path, max) = (
                protocol::ENV_HOOK_TRACE_PATH,
                protocol::ENV_HOOK_TRACE_MAX_BYTES,
            );
            let untraced = [
                ("both-absent", format!("unset {path} {max};")),
                ("path-absent", format!("unset {path};")),
                ("max-absent", format!("unset {max};")),
                ("path-relative", format!("export {path}={RELATIVE};")),
                ("max-zero", format!("export {max}=0;")),
                ("max-not-a-number", format!("export {max}=many;")),
                ("outside-session", format!("unset {};", protocol::ENV_FLAG)),
            ];
            let claude = state_asset(CLAUDE);
            let mut steps: Vec<Step> = untraced
                .iter()
                .map(|(name, prelude)| {
                    traced(&root, name, prelude, &claude, SUBAGENT_START, NOT_JSON)
                })
                .collect();
            steps.push(traced(
                &root,
                "control",
                "",
                &claude,
                SUBAGENT_START,
                NOT_JSON,
            ));
            replay(&root, steps, &Rows::new()).await.finish().await;

            for (name, _) in &untraced {
                assert_silent(&root, name);
            }
            assert_silent(&root, "control");
            assert!(
                absent(&root.join(RELATIVE)),
                "a relative trace path is never written"
            );
            let lines = trace_lines(&super::super::hook_trace_path(&root));
            let [line] = lines.as_slice() else {
                panic!("only the failure with the worker's variables writes a line: {lines:?}");
            };
            assert_line(
                line,
                &Expected::new(CLAUDE, SUBAGENT_START, &[INPUT_INVALID]),
            );
        }
    }
}

/// Launch-identity scenarios whose launch process reports in process from
/// Python: the frozen-root launch rule, and per-process sequence ordering under
/// both launch rules.
///
/// Linux only: on macOS `python3` is an `xcrun` shim or a framework stub that
/// re-executes a different image, so no shebang names the image the root runs.
#[cfg(target_os = "linux")]
mod frozen_root {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use pohunek_session_worker::WorkerConfig;
    use pohunek_worker_protocol::{LaunchIdentity, TransactionId};
    use serde_json::{json, Value};

    use super::{initialize, start_worker_initialized};

    /// Launch root of the frozen-root scenario: an in-process reporter whose
    /// forked children report the root's own process identity.
    ///
    /// Its third argument is how many forked children first claim the launch
    /// identity for their own process: one more than the worker's pending
    /// launch-claim capacity.
    ///
    /// Each report appends `{"step", "response"}` to the results file. Before
    /// its last report, and again before it exits, the reporter waits for one
    /// line on the gate, which the test writes after it changed the entrypoint
    /// file and once it is done. The test prepends the shebang line.
    const FROZEN_ROOT_REPORTER: &str = r#"
import datetime
import json
import os
import socket
import sys

gate_path, results_path, own_claims = sys.argv[1], sys.argv[2], int(sys.argv[3])
failure_path = results_path + ".failure"

def record_failure(kind, value, trace):
    # The test reads this file, so the reporter never dies without a reason.
    import traceback
    with open(failure_path, "w", encoding="utf-8") as handle:
        handle.write("".join(traceback.format_exception(kind, value, trace)))

sys.excepthook = record_failure

def start_identity(pid):
    with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
        return int(handle.read().rsplit(")", 1)[1].split()[19])

root_pid = os.getpid()
root_start = start_identity(root_pid)

def report(step, sequence, reference, own=False):
    pid = os.getpid() if own else root_pid
    expires_at = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=30)
    request = {
        "type": "identity_report",
        "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
        "provider": "hermes",
        "pid": pid,
        "start_identity": start_identity(pid) if own else root_start,
        "sequence": sequence,
        "expires_at": expires_at.isoformat().replace("+00:00", "Z"),
        "reference_kind": "id",
        "native_reference": reference,
    }
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(os.environ["POHUNEK_WORKER_SOCKET_PATH"])
    client.sendall((json.dumps(request) + "\n").encode())
    received = b""
    while b"\n" not in received:
        chunk = client.recv(4096)
        if not chunk:
            raise RuntimeError(f"worker closed the socket before a full response line: {received!r}")
        received += chunk
    client.close()
    response = json.loads(received.split(b"\n", 1)[0])
    with open(results_path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps({"step": step, "response": response}) + "\n")

def from_descendant(step, sequence, reference, own=False):
    # A forked child inherits the worker socket path and instance id, and its
    # report names the root's pid and start identity, or with `own` its own.
    child = os.fork()
    if child == 0:
        try:
            report(step, sequence, reference, own)
        except BaseException:
            record_failure(*sys.exc_info())
            os._exit(1)
        os._exit(0)
    _, status = os.waitpid(child, 0)
    if os.waitstatus_to_exitcode(status) != 0:
        raise RuntimeError(f"descendant step {step} failed")

# Distinct descendants claim for themselves before the root, each with a
# sequence below the root's, more of them than the pending capacity holds.
for index in range(own_claims):
    from_descendant(f"own_{index}", index + 1, f"own-{index}", own=True)
# The descendant claims for the root first and with a far higher sequence than
# the root's, so neither ordering nor arrival order can favor the root.
from_descendant("descendant_first", 1_000_000, "descendant-first")
report("root_first", 1_000, "root-first")
from_descendant("descendant_update", 2_000_000, "descendant-update")
report("root_update", 2_000, "root-update")
with open(gate_path, encoding="ascii") as gate:
    gate.readline()
report("root_after_touch", 3_000, "root-after-touch")
with open(gate_path, encoding="ascii") as gate:
    gate.readline()
"#;

    /// The image the kernel runs for `python3`: the absolute interpreter a
    /// frozen-root entrypoint's shebang must name.
    fn python_image() -> String {
        let output = std::process::Command::new("python3")
            .args(["-c", "import os; print(os.path.realpath('/proc/self/exe'))"])
            .output()
            .expect("run python3");
        assert!(output.status.success(), "python3 failed: {output:?}");
        let image = String::from_utf8(output.stdout).expect("UTF-8 interpreter path");
        let image = image.trim_end().to_owned();
        assert!(
            Path::new(&image).is_absolute() && !image.contains(char::is_whitespace),
            "the interpreter cannot be named by a single-path shebang: {image:?}"
        );
        image
    }

    /// Reads the reporter's recorded responses by step name.
    fn frozen_root_results(results: &Path) -> BTreeMap<String, Value> {
        if let Ok(failure) = std::fs::read_to_string(format!("{}.failure", results.display())) {
            panic!("the frozen-root reporter failed: {failure}");
        }
        std::fs::read_to_string(results)
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let mut entry: Value = serde_json::from_str(line).expect("result line is JSON");
                let step = entry["step"].as_str().expect("step name").to_owned();
                (step, entry["response"].take())
            })
            .collect()
    }

    /// Waits until the reporter recorded `count` responses and returns them.
    async fn frozen_root_responses(results: &Path, count: usize) -> BTreeMap<String, Value> {
        Box::pin(pohunek_test_support::wait::wait_until(
            "the frozen-root reporter to record its responses",
            || async {
                let recorded = frozen_root_results(results);
                (recorded.len() >= count).then_some(recorded)
            },
        ))
        .await
    }

    /// A real worker whose PTY root runs a reporter script.
    struct FrozenRoot {
        worker: crate::runtime::Worker,
        task: tokio::task::JoinHandle<Result<(), pohunek_session_worker::WorkerError>>,
        /// Write end of the FIFO the reporter reads one line from per pause.
        gate: std::fs::File,
        /// The reporter script the PTY root runs.
        entrypoint: PathBuf,
        /// File the reporter appends `{"step", "response"}` lines to.
        results: PathBuf,
    }

    /// Starts a Hermes-launched `identity-v1` worker whose entrypoint is
    /// `body` behind an absolute `python3` shebang, run as
    /// `<entrypoint> <gate> <results> <arguments>...`.
    async fn start_frozen_root(root: &Path, body: &str, arguments: &[String]) -> FrozenRoot {
        let entrypoint = root.join("frozen-root-reporter");
        std::fs::write(&entrypoint, format!("#!{}\n{body}", python_image()))
            .expect("write the entrypoint script");
        std::fs::set_permissions(
            &entrypoint,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("make the entrypoint executable");
        let mut launch = initialize(root, "", protocol::PROTOCOL_VERSION.get(), "identity-v1");
        launch.launch = LaunchIdentity {
            agent: "hermes".to_owned(),
            agent_base: "hermes".to_owned(),
            reference_kind: None,
        };
        launch.executable.clone_from(&entrypoint);
        start_reporter_root(root, launch, entrypoint, None, arguments).await
    }

    /// Starts a Claude-launched `identity-subagent-v1` worker whose PTY root is
    /// a copy of the `python3` image named `claude` running `body`, as
    /// `claude <script> <gate> <results> <arguments>...`.
    ///
    /// The copy makes the root the process the named-for-provider launch rule
    /// designates; a symlink would not, because the kernel reports the image
    /// it resolves to.
    async fn start_named_root(root: &Path, body: &str, arguments: &[String]) -> FrozenRoot {
        let image = root.join("claude");
        std::fs::copy(python_image(), &image).expect("copy the interpreter image");
        let script = root.join("named-root-reporter.py");
        std::fs::write(&script, body).expect("write the reporter script");
        let mut launch = initialize(
            root,
            "",
            protocol::PROTOCOL_VERSION.get(),
            "identity-subagent-v1",
        );
        launch.executable = image;
        start_reporter_root(root, launch, script.clone(), Some(&script), arguments).await
    }

    /// Starts a real worker for `launch` whose PTY root is run with `script`
    /// (when its executable is an interpreter), the gate and results paths,
    /// then `arguments`.
    async fn start_reporter_root(
        root: &Path,
        mut launch: pohunek_worker_protocol::Initialize,
        entrypoint: PathBuf,
        script: Option<&Path>,
        arguments: &[String],
    ) -> FrozenRoot {
        let gate_path = root.join("frozen-root.gate");
        nix::unistd::mkfifo(&gate_path, nix::sys::stat::Mode::S_IRWXU).expect("create the gate");
        // Opened read-write so the test never blocks on a reader and the reporter's
        // read waits for a line rather than for end of file.
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate_path)
            .expect("open the gate");
        let results = root.join("frozen-root.results");

        launch.arguments = script
            .map(|script| script.display().to_string())
            .into_iter()
            .chain([
                gate_path.display().to_string(),
                results.display().to_string(),
            ])
            .chain(arguments.iter().cloned())
            .collect();
        let (worker, task, _socket) = start_worker_initialized(root, launch).await;
        FrozenRoot {
            worker,
            task,
            gate,
            entrypoint,
            results,
        }
    }

    /// Under `identity-v1` only the PTY root itself, while it still runs the frozen
    /// entrypoint, sets or updates the launch identity and its native reference.
    ///
    /// The root is a real script with an absolute shebang that reports in process
    /// through the worker socket, as the pinned Hermes plugin does. Its forked
    /// descendants inherit the socket and report the root's exact process
    /// identity, which the kernel peer credentials expose as a different process.
    /// Before that, more descendants than the worker keeps pending claims for
    /// claim the launch identity for themselves, and none of them is kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_frozen_root_sets_or_updates_the_launch_identity() {
        let root = crate::test_support::scoped_dir("ph-frozen-root-");
        let own_claims = WorkerConfig::new().pending_launch_claims + 1;
        let FrozenRoot {
            worker,
            task,
            mut gate,
            entrypoint,
            results,
        } = start_frozen_root(&root, FROZEN_ROOT_REPORTER, &[own_claims.to_string()]).await;

        let responses = frozen_root_responses(&results, own_claims + 4).await;
        // A descendant's own claim can never qualify under the frozen-root rule,
        // so none of them occupies the bounded pending capacity.
        for index in 0..own_claims {
            assert_eq!(
                responses[&format!("own_{index}")],
                json!({"ok": true, "launch_identity_accepted": false, "launch_identity_status": "rejected"}),
                "{responses:?}"
            );
        }
        assert_eq!(
            responses["descendant_first"]["ok"],
            json!(false),
            "a descendant cannot report the root's identity: {responses:?}"
        );
        assert_eq!(
            responses["root_first"],
            json!({"ok": true, "launch_identity_accepted": true, "launch_identity_status": "accepted"}),
            "the root's own report establishes the launch identity: {responses:?}"
        );
        assert_eq!(
            responses["descendant_update"]["ok"],
            json!(false),
            "a descendant cannot update the root's reference: {responses:?}"
        );
        // The launch identity is immutable: a later reference of the same root is
        // journaled while its claim to establish the launch identity is rejected.
        assert_eq!(
            responses["root_update"],
            json!({"ok": true, "launch_identity_accepted": false, "launch_identity_status": "rejected"}),
            "{responses:?}"
        );

        let snapshot = worker.inspect().await.expect("inspect");
        let root_process = snapshot.child_process.expect("the PTY root");
        let launch_identity = snapshot.launch_identity.expect("the launch identity");
        assert_eq!(launch_identity.process, root_process);
        assert_eq!(launch_identity.native_reference, "root-first");
        let reference = snapshot.native_reference.expect("the journaled reference");
        assert_eq!(reference.process, root_process);
        assert_eq!(
            (reference.native_reference.as_str(), reference.sequence),
            ("root-update", 2_000),
            "the root's later reference is journaled"
        );

        // Any change to the frozen entrypoint advances its status-change time, so
        // the root no longer runs the image the worker froze.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&entrypoint)
            .expect("open the entrypoint")
            .set_modified(std::time::SystemTime::now())
            .expect("touch the entrypoint");
        std::io::Write::write_all(&mut gate, b"touched\n").expect("release the reporter");
        let responses = frozen_root_responses(&results, own_claims + 5).await;
        assert_eq!(
            responses["root_after_touch"]["launch_identity_status"],
            json!("rejected"),
            "{responses:?}"
        );
        let snapshot = worker.inspect().await.expect("inspect");
        let reference = snapshot.native_reference.expect("the journaled reference");
        assert_eq!(
            reference.native_reference, "root-update",
            "a report after the entrypoint changed is not journaled"
        );
        assert_eq!(
            snapshot
                .launch_identity
                .map(|identity| identity.native_reference),
            Some("root-first".to_owned())
        );

        std::io::Write::write_all(&mut gate, b"done\n").expect("release the reporter");
        worker
            .stop(TransactionId::new("transaction-frozen-stop").expect("transaction id"))
            .await
            .expect("stop the PTY");
        drop(worker);
        task.abort();
    }

    /// Launch root that reports through the core-owned Hermes reporter, with a
    /// nested Claude identity hook between its two reports.
    ///
    /// Arguments after the gate and results paths: the reporter module and the
    /// Claude state hook. The hook runs under an intermediate shell, so it
    /// reports that shell as a nested agent rather than the root. Each step
    /// appends `{"step", "response": {"failures"}}`, the reporter's count of
    /// reports the worker did not accept.
    const SHARED_CLOCK_REPORTER: &str = r#"
import importlib.util
import json
import os
import subprocess
import sys

gate_path, results_path, reporter_path, claude_hook = sys.argv[1:5]
failure_path = results_path + ".failure"

def record_failure(kind, value, trace):
    # The test reads this file, so the reporter never dies without a reason.
    import traceback
    with open(failure_path, "w", encoding="utf-8") as handle:
        handle.write("".join(traceback.format_exception(kind, value, trace)))

sys.excepthook = record_failure

spec = importlib.util.spec_from_file_location("pohunek_reporter", reporter_path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
# The reporter's longest hook deadline keeps a slow host from turning a worker
# answer into a recorded failure.
reporter = module.Reporter("hermes", {**os.environ, "POHUNEK_HOOK_TIMEOUT_MS": "1000"})

def mark(step):
    with open(results_path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps({"step": step, "response": {"failures": reporter.failures}}) + "\n")

reporter.identity("root-first")
mark("root_first")
subprocess.run(
    ["/bin/sh", "-c", '/bin/sh "$0" session; true', claude_hook],
    input=b'{"session_id":"nested-native"}',
    check=True,
)
mark("nested")
with open(gate_path, encoding="ascii") as gate:
    gate.readline()
reporter.identity("root-second")
mark("root_second")
with open(gate_path, encoding="ascii") as gate:
    gate.readline()
"#;

    /// A nested agent's identity report does not make the root's later report
    /// stale: every in-repo reporter stamps worker claims from the same
    /// system-wide monotonic clock.
    ///
    /// The root reports through the core-owned Hermes reporter, and the real
    /// Claude state hook reports a nested agent between its two reports.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_nested_agent_report_does_not_make_the_roots_later_report_stale() {
        let root = crate::test_support::scoped_dir("ph-shared-clock-");
        let reporter = pohunek_test_support::workspace_root()
            .join("crates/cli/src/hermes_integration/assets/pohunek/_pohunek_reporter.py");
        let FrozenRoot {
            worker,
            task,
            mut gate,
            results,
            ..
        } = start_frozen_root(
            &root,
            SHARED_CLOCK_REPORTER,
            &[
                reporter.display().to_string(),
                super::state_asset("claude").display().to_string(),
            ],
        )
        .await;

        let responses = frozen_root_responses(&results, 2).await;
        assert_eq!(
            responses["root_first"]["failures"],
            json!(0),
            "{responses:?}"
        );
        let snapshot = worker.inspect().await.expect("inspect");
        let first = snapshot.native_reference.expect("the root's reference");
        assert_eq!(first.native_reference, "root-first");
        let nested = snapshot.active_identity.expect("the nested agent's claim");
        assert_eq!(nested.provider, "claude", "the nested agent is active");
        assert_ne!(Some(nested.process), snapshot.child_process);
        assert!(nested.sequence > first.sequence, "{nested:?} {first:?}");

        std::io::Write::write_all(&mut gate, b"report\n").expect("release the reporter");
        let responses = frozen_root_responses(&results, 3).await;
        assert_eq!(
            responses["root_second"]["failures"],
            json!(0),
            "the root's later report is accepted: {responses:?}"
        );
        let snapshot = worker.inspect().await.expect("inspect");
        let reference = snapshot.native_reference.expect("the root's reference");
        assert_eq!(reference.native_reference, "root-second");
        assert_eq!(
            snapshot.active_identity.map(|active| active.provider),
            Some("hermes".to_owned())
        );

        std::io::Write::write_all(&mut gate, b"done\n").expect("release the reporter");
        worker
            .stop(TransactionId::new("transaction-clock-stop").expect("transaction id"))
            .await
            .expect("stop the PTY");
        drop(worker);
        task.abort();
    }

    /// Launch root whose forked descendants each report their own process as a
    /// nested Claude agent with the largest sequence the wire admits.
    ///
    /// Arguments after the gate and results paths: the launch provider the
    /// root reports as. The root stamps its reports from `CLOCK_MONOTONIC`, as
    /// the shipped hooks do. Each report appends `{"step", "response"}` to the
    /// results file; the reporter waits for one gate line before its last
    /// report and again before it exits.
    const OWN_SEQUENCE_REPORTER: &str = r#"
import datetime
import json
import os
import socket
import sys
import time

gate_path, results_path, launch_provider = sys.argv[1:4]
failure_path = results_path + ".failure"
# The largest `sequence` the worker's u64 field accepts.
MAX_SEQUENCE = 2**64 - 1

def record_failure(kind, value, trace):
    # The test reads this file, so the reporter never dies without a reason.
    import traceback
    with open(failure_path, "w", encoding="utf-8") as handle:
        handle.write("".join(traceback.format_exception(kind, value, trace)))

sys.excepthook = record_failure

def start_identity(pid):
    with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
        return int(handle.read().rsplit(")", 1)[1].split()[19])

def report(step, provider, sequence, reference):
    pid = os.getpid()
    expires_at = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=30)
    request = {
        "type": "identity_report",
        "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
        "provider": provider,
        "pid": pid,
        "start_identity": start_identity(pid),
        "sequence": sequence,
        "expires_at": expires_at.isoformat().replace("+00:00", "Z"),
        "reference_kind": "id",
        "native_reference": reference,
    }
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(os.environ["POHUNEK_WORKER_SOCKET_PATH"])
    client.sendall((json.dumps(request) + "\n").encode())
    received = b""
    while b"\n" not in received:
        chunk = client.recv(4096)
        if not chunk:
            raise RuntimeError(f"worker closed the socket before a full response line: {received!r}")
        received += chunk
    client.close()
    response = json.loads(received.split(b"\n", 1)[0])
    with open(results_path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps({"step": step, "response": response}) + "\n")

def from_descendant(step, reference):
    # Each fork is a distinct process that reports itself, which the kernel
    # peer credentials confirm.
    child = os.fork()
    if child == 0:
        try:
            report(step, "claude", MAX_SEQUENCE, reference)
        except BaseException:
            record_failure(*sys.exc_info())
            os._exit(1)
        os._exit(0)
    _, status = os.waitpid(child, 0)
    if os.waitstatus_to_exitcode(status) != 0:
        raise RuntimeError(f"descendant step {step} failed")

def from_root(step, reference):
    report(step, launch_provider, time.clock_gettime_ns(time.CLOCK_MONOTONIC), reference)

from_descendant("descendant_before", "nested-before")
from_root("root_first", "root-first")
from_descendant("descendant_after", "nested-after")
with open(gate_path, encoding="ascii") as gate:
    gate.readline()
from_root("root_update", "root-update")
with open(gate_path, encoding="ascii") as gate:
    gate.readline()
"#;

    /// A descendant's own report at the largest sequence never makes the
    /// launch root's later reports stale.
    ///
    /// Identity reports are ordered per reporting process, so the root's first
    /// report still establishes the launch identity after a descendant's, and
    /// its later report still updates the journaled native reference. Across
    /// processes the latest accepted report is the active identity.
    async fn assert_own_sequence_never_starves_the_root(root_process: FrozenRoot) {
        let FrozenRoot {
            worker,
            task,
            mut gate,
            results,
            ..
        } = root_process;

        let responses = frozen_root_responses(&results, 3).await;
        assert_eq!(
            responses["descendant_before"]["ok"],
            json!(true),
            "a descendant reports its own process: {responses:?}"
        );
        assert_eq!(
            responses["root_first"],
            json!({"ok": true, "launch_identity_accepted": true, "launch_identity_status": "accepted"}),
            "the root's first report establishes the launch identity: {responses:?}"
        );
        assert_eq!(
            responses["descendant_after"]["ok"],
            json!(true),
            "another descendant reports its own process: {responses:?}"
        );
        let snapshot = worker.inspect().await.expect("inspect");
        let root_identity = snapshot.child_process.expect("the PTY root");
        assert_eq!(
            snapshot
                .launch_identity
                .as_ref()
                .map(|launch| (launch.process, launch.native_reference.as_str())),
            Some((root_identity, "root-first"))
        );
        let nested = snapshot.active_identity.expect("the latest accepted claim");
        assert_eq!(
            (nested.provider.as_str(), nested.sequence),
            ("claude", u64::MAX),
            "the descendant's later claim is the active identity"
        );
        assert_ne!(nested.process, root_identity);

        std::io::Write::write_all(&mut gate, b"report\n").expect("release the reporter");
        let responses = frozen_root_responses(&results, 4).await;
        assert_eq!(
            responses["root_update"]["ok"],
            json!(true),
            "the root's later report is accepted: {responses:?}"
        );
        let snapshot = worker.inspect().await.expect("inspect");
        let reference = snapshot.native_reference.expect("the journaled reference");
        assert_eq!(
            (reference.process, reference.native_reference.as_str()),
            (root_identity, "root-update"),
            "the root's later report updates its native reference"
        );
        let active = snapshot.active_identity.expect("the root's claim");
        assert_eq!(active.process, root_identity, "the root is active again");

        std::io::Write::write_all(&mut gate, b"done\n").expect("release the reporter");
        worker
            .stop(TransactionId::new("transaction-own-sequence-stop").expect("transaction id"))
            .await
            .expect("stop the PTY");
        drop(worker);
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_descendants_largest_sequence_never_starves_the_frozen_root() {
        let root = crate::test_support::scoped_dir("ph-own-sequence-frozen-");
        let reporter =
            start_frozen_root(&root, OWN_SEQUENCE_REPORTER, &["hermes".to_owned()]).await;
        assert_own_sequence_never_starves_the_root(reporter).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_descendants_largest_sequence_never_starves_the_named_launch_root() {
        let root = crate::test_support::scoped_dir("ph-own-sequence-named-");
        let reporter = start_named_root(&root, OWN_SEQUENCE_REPORTER, &["claude".to_owned()]).await;
        assert_own_sequence_never_starves_the_root(reporter).await;
    }

    /// Claude launch root that reports itself and a background snapshot from
    /// `CLOCK_MONOTONIC`, waits for one gate line, takes a second snapshot's
    /// sequence, has a forked descendant report its own process from a clock
    /// of its own, then delivers that second snapshot, and waits for a second
    /// gate line before it exits. The argument after the gate and results
    /// paths is `below` (the descendant's clock reads far below the root's)
    /// or `above` (it reads just above the root's first snapshot and below
    /// the second).
    const BACKGROUND_SWITCH: &str = r#"
import datetime
import json
import os
import socket
import sys
import time

gate_path, results_path, mode = sys.argv[1:4]
failure_path = results_path + ".failure"
# The descendant's own clock in `below` mode: far below any CLOCK_MONOTONIC
# reading of the root. In `above` mode it reads just above the root's first
# snapshot and below the snapshot the root delivers late.
DESCENDANT_SEQUENCE_BELOW = 10

def record_failure(kind, value, trace):
    # The test reads this file, so the reporter never dies without a reason.
    import traceback
    with open(failure_path, "w", encoding="utf-8") as handle:
        handle.write("".join(traceback.format_exception(kind, value, trace)))

sys.excepthook = record_failure

def start_identity(pid):
    with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
        return int(handle.read().rsplit(")", 1)[1].split()[19])

def send(step, request):
    pid = os.getpid()
    request = {
        "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
        "provider": "claude",
        "pid": pid,
        "start_identity": start_identity(pid),
        **request,
    }
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(os.environ["POHUNEK_WORKER_SOCKET_PATH"])
    client.sendall((json.dumps(request) + "\n").encode())
    received = b""
    while b"\n" not in received:
        chunk = client.recv(4096)
        if not chunk:
            raise RuntimeError(f"worker closed the socket before a full response line: {received!r}")
        received += chunk
    client.close()
    response = json.loads(received.split(b"\n", 1)[0])
    with open(results_path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps({"step": step, "response": response}) + "\n")

def report(step, sequence):
    expires_at = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=30)
    send(step, {
        "type": "identity_report",
        "sequence": sequence,
        "expires_at": expires_at.isoformat().replace("+00:00", "Z"),
        "reference_kind": "id",
        "native_reference": step.replace("_", "-"),
    })

def now():
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC)

report("root_first", now())
first_snapshot = now()
send("root_snapshot", {
    "type": "subagent_snapshot",
    "sequence": first_snapshot,
    "tasks": [{"id": "shell-1", "type": "shell", "status": "running", "label": "Test watch", "program": "cargo"}],
})
with open(gate_path, encoding="ascii") as gate:
    gate.readline()
# A snapshot the root takes before the switch and delivers after it.
pending = max(now(), first_snapshot + 2)
descendant_sequence = DESCENDANT_SEQUENCE_BELOW if mode == "below" else first_snapshot + 1
child = os.fork()
if child == 0:
    try:
        report("descendant_low", descendant_sequence)
    except BaseException:
        record_failure(*sys.exc_info())
        os._exit(1)
    os._exit(0)
_, status = os.waitpid(child, 0)
if os.waitstatus_to_exitcode(status) != 0:
    raise RuntimeError("the descendant report failed")
send("root_late_snapshot", {
    "type": "subagent_snapshot",
    "sequence": pending,
    "tasks": [{"id": "shell-1", "type": "shell", "status": "running", "label": "Test watch", "program": "cargo"}],
})
with open(gate_path, encoding="ascii") as gate:
    gate.readline()
"#;

    /// A process whose own clock reads below the background owner's still
    /// clears the owner's snapshot once its report is accepted as the active
    /// identity: sequences of different processes are never compared. An owner
    /// snapshot taken before that report but delivered after it stays stale.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_accepted_report_of_another_process_clears_the_background_whatever_its_clock() {
        assert_switch_clears_the_background("below").await;
    }

    /// The same holds when the other process's clock reads above the stored
    /// background mark but below the owner's late snapshot: the clear raises
    /// the mark to the worker's own clock, not to that process's sequence.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_late_owner_snapshot_stays_stale_after_a_switch_ordered_above_the_mark() {
        assert_switch_clears_the_background("above").await;
    }

    /// Runs [`BACKGROUND_SWITCH`] in `mode` and checks the background is
    /// cleared and stays cleared.
    async fn assert_switch_clears_the_background(mode: &str) {
        let root = crate::test_support::scoped_dir(&format!("ph-background-switch-{mode}-"));
        let FrozenRoot {
            worker,
            task,
            mut gate,
            results,
            ..
        } = start_named_root(&root, BACKGROUND_SWITCH, &[mode.to_owned()]).await;

        let responses = frozen_root_responses(&results, 2).await;
        assert_eq!(responses["root_first"]["ok"], json!(true), "{responses:?}");
        assert_eq!(
            responses["root_snapshot"]["ok"],
            json!(true),
            "{responses:?}"
        );
        let snapshot = worker.inspect().await.expect("inspect");
        assert!(
            snapshot.background.is_some(),
            "the root's snapshot is applied"
        );

        std::io::Write::write_all(&mut gate, b"report\n").expect("release the reporter");
        let responses = frozen_root_responses(&results, 4).await;
        assert_eq!(
            responses["descendant_low"]["ok"],
            json!(true),
            "the descendant's own first report is accepted: {responses:?}"
        );
        // The late snapshot is admitted for its subagents; only its background
        // is stale, which the inspect below shows.
        assert!(
            responses.contains_key("root_late_snapshot"),
            "{responses:?}"
        );
        let snapshot = worker.inspect().await.expect("inspect");
        let root_identity = snapshot.child_process.expect("the PTY root");
        let active = snapshot.active_identity.expect("the descendant's claim");
        assert_ne!(active.process, root_identity, "the descendant is active");
        assert!(
            snapshot.background.is_none(),
            "the previous owner's snapshot is cleared and its late snapshot is stale: {:?}",
            snapshot.background
        );

        std::io::Write::write_all(&mut gate, b"done\n").expect("release the reporter");
        worker
            .stop(
                TransactionId::new(format!("transaction-background-switch-{mode}"))
                    .expect("transaction id"),
            )
            .await
            .expect("stop the PTY");
        drop(worker);
        task.abort();
    }

    /// Launch root that drives live descendant reporters, each a forked
    /// process that stays alive and reports its own process as a nested Claude
    /// agent on command.
    ///
    /// Arguments after the gate and results paths: the scenario
    /// (`late_claim`, `full_table`, `zombie_table` or `launch_beyond_bound`)
    /// and the worker's identity-reporter bound; `launch_beyond_bound` also
    /// takes the image the launch process runs. Except in
    /// `launch_beyond_bound` the root first reports as Hermes, stamped from
    /// `CLOCK_MONOTONIC`. Each report appends `{"step", "response"}` to the
    /// results file; the reporter waits for one gate line before it exits.
    const LIVE_REPORTERS: &str = r#"
import datetime
import json
import os
import socket
import sys
import time

gate_path, results_path, scenario, bound = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
failure_path = results_path + ".failure"

def record_failure(kind, value, trace):
    # The test reads this file, so the reporter never dies without a reason.
    import traceback
    with open(failure_path, "w", encoding="utf-8") as handle:
        handle.write("".join(traceback.format_exception(kind, value, trace)))

sys.excepthook = record_failure

def start_identity(pid):
    with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
        return int(handle.read().rsplit(")", 1)[1].split()[19])

def send(step, request):
    pid = os.getpid()
    request = {
        "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
        "pid": pid,
        "start_identity": start_identity(pid),
        **request,
    }
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(os.environ["POHUNEK_WORKER_SOCKET_PATH"])
    client.sendall((json.dumps(request) + "\n").encode())
    received = b""
    while b"\n" not in received:
        chunk = client.recv(4096)
        if not chunk:
            raise RuntimeError(f"worker closed the socket before a full response line: {received!r}")
        received += chunk
    client.close()
    response = json.loads(received.split(b"\n", 1)[0])
    with open(results_path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps({"step": step, "response": response}) + "\n")

def report(step, provider, sequence, reference=True):
    expires_at = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=30)
    request = {
        "type": "identity_report",
        "provider": provider,
        "sequence": sequence,
        "expires_at": expires_at.isoformat().replace("+00:00", "Z"),
    }
    if reference:
        request["reference_kind"] = "id"
        request["native_reference"] = step.replace("_", "-")
    send(step, request)

def release(step, provider, sequence):
    send(step, {"type": "identity_release", "provider": provider, "sequence": sequence})

class Live:
    """A forked descendant that stays alive and reports itself on command."""

    def __init__(self):
        commands_read, commands_write = os.pipe()
        acks_read, acks_write = os.pipe()
        pid = os.fork()
        if pid == 0:
            os.close(commands_write)
            os.close(acks_read)
            commands = os.fdopen(commands_read, "r", encoding="ascii")
            acks = os.fdopen(acks_write, "w", encoding="ascii")
            try:
                for line in commands:
                    kind, step, sequence = line.split()
                    if kind == "exit":
                        break
                    if kind == "report":
                        report(step, "claude", int(sequence))
                    elif kind == "bare":
                        report(step, "claude", int(sequence), reference=False)
                    else:
                        release(step, "claude", int(sequence))
                    acks.write("ok\n")
                    acks.flush()
            except BaseException:
                record_failure(*sys.exc_info())
                os._exit(1)
            os._exit(0)
        os.close(commands_read)
        os.close(acks_write)
        self.pid = pid
        self.commands = os.fdopen(commands_write, "w", encoding="ascii")
        self.acks = os.fdopen(acks_read, "r", encoding="ascii")

    def run(self, kind, step, sequence):
        self.commands.write(f"{kind} {step} {sequence}\n")
        self.commands.flush()
        if self.acks.readline() != "ok\n":
            raise RuntimeError(f"live reporter step {step} failed")

    def exit(self):
        # An explicit command, because later forks hold this pipe open too.
        self.commands.write("exit - 0\n")
        self.commands.flush()
        _, status = os.waitpid(self.pid, 0)
        if os.waitstatus_to_exitcode(status) != 0:
            raise RuntimeError("live reporter failed")

    def exit_unreaped(self):
        """Ends the process but leaves it a zombie until `reap`."""
        self.commands.write("exit - 0\n")
        self.commands.flush()
        # WNOWAIT returns once the process has exited without reaping it.
        os.waitid(os.P_PID, self.pid, os.WEXITED | os.WNOWAIT)

    def reap(self):
        _, status = os.waitpid(self.pid, 0)
        if os.waitstatus_to_exitcode(status) != 0:
            raise RuntimeError("live reporter failed")

def from_root(step):
    report(step, "hermes", time.clock_gettime_ns(time.CLOCK_MONOTONIC))

live = []
zombies = []
if scenario == "launch_process":
    # The launch process the root started: it reports with a native
    # reference, acknowledges on stdout and lives until stdin closes.
    report("launch_report", "claude", 400)
    sys.stdout.write("ok\n")
    sys.stdout.flush()
    sys.stdin.read()
    sys.exit(0)
if scenario != "launch_beyond_bound":
    from_root("root_first")
if scenario == "late_claim":
    released = Live()
    live.append(released)
    released.run("report", "released_claim", 100)
    released.run("release", "released_release", 110)
    other = Live()
    live.append(other)
    other.run("report", "other_claim", 200)
    for index in range(bound):
        filler = Live()
        live.append(filler)
        filler.run("report", f"filler_{index}", 300)
    released.run("report", "released_late", 105)
elif scenario == "full_table":
    for index in range(bound):
        filler = Live()
        live.append(filler)
        filler.run("report", f"filler_{index}", 300)
    extra = Live()
    live.append(extra)
    extra.run("report", "extra_refused", 300)
    from_root("root_update")
    live.pop(0).exit()
    extra.run("report", "extra_after_exit", 301)
elif scenario == "zombie_table":
    for index in range(bound):
        filler = Live()
        live.append(filler)
        filler.run("report", f"filler_{index}", 300)
    extra = Live()
    live.append(extra)
    extra.run("report", "extra_refused", 300)
    zombie = live.pop(0)
    zombie.exit_unreaped()
    zombies.append(zombie)
    extra.run("report", "extra_after_zombie", 301)
elif scenario == "launch_beyond_bound":
    # Reports without a native reference never claim the launch identity, so
    # these fill the bound while no launch process exists yet.
    for index in range(bound):
        filler = Live()
        live.append(filler)
        filler.run("bare", f"filler_{index}", 300)
    import subprocess
    launch_image = sys.argv[5]
    launch = subprocess.Popen(
        [launch_image, sys.argv[0], gate_path, results_path, "launch_process", str(bound)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    if launch.stdout.readline() != "ok\n":
        raise RuntimeError("the launch process did not report")
    extra = Live()
    live.append(extra)
    extra.run("bare", "extra_refused", 300)
else:
    raise RuntimeError(f"unknown scenario {scenario}")
with open(gate_path, encoding="ascii") as gate:
    gate.readline()
for process in live:
    process.exit()
for process in zombies:
    process.reap()
if scenario == "launch_beyond_bound":
    launch.stdin.close()
    if launch.wait() != 0:
        raise RuntimeError("the launch process failed")
"#;

    /// A released process that is still alive cannot become active again
    /// through its own older claim delivered late, however many other
    /// processes report in between: the worker keeps a live process's
    /// sequence instead of evicting it, so the late claim is stale.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_released_process_stays_released_under_reporter_pressure() {
        let root = crate::test_support::scoped_dir("ph-live-released-");
        let bound = WorkerConfig::new().identity_reporters;
        let FrozenRoot {
            worker,
            task,
            mut gate,
            results,
            ..
        } = start_frozen_root(
            &root,
            LIVE_REPORTERS,
            &["late_claim".to_owned(), bound.to_string()],
        )
        .await;

        let responses = frozen_root_responses(&results, bound + 5).await;
        for step in [
            "root_first",
            "released_claim",
            "released_release",
            "other_claim",
        ] {
            assert_eq!(responses[step]["ok"], json!(true), "{step}: {responses:?}");
        }
        assert_eq!(
            responses["released_late"]["ok"],
            json!(false),
            "the released process's older claim is stale: {responses:?}"
        );
        let snapshot = worker.inspect().await.expect("inspect");
        assert_ne!(
            snapshot
                .active_identity
                .and_then(|active| active.native_reference),
            Some("released-late".to_owned()),
            "the released process is not active again"
        );

        std::io::Write::write_all(&mut gate, b"done\n").expect("release the reporter");
        worker
            .stop(TransactionId::new("transaction-live-released-stop").expect("transaction id"))
            .await
            .expect("stop the PTY");
        drop(worker);
        task.abort();
    }

    /// When live reporting processes fill the worker's bound, a report from a
    /// further process is refused while the launch root's reports are still
    /// accepted, and room returns once a reporter exits.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_reporter_table_refuses_a_new_process_but_never_the_root() {
        let root = crate::test_support::scoped_dir("ph-full-reporters-");
        let bound = WorkerConfig::new().identity_reporters;
        let FrozenRoot {
            worker,
            task,
            mut gate,
            results,
            ..
        } = start_frozen_root(
            &root,
            LIVE_REPORTERS,
            &["full_table".to_owned(), bound.to_string()],
        )
        .await;

        let responses = frozen_root_responses(&results, bound + 4).await;
        for index in 0..bound {
            assert_eq!(
                responses[&format!("filler_{index}")]["ok"],
                json!(true),
                "{responses:?}"
            );
        }
        assert_eq!(
            responses["extra_refused"]["ok"],
            json!(false),
            "a further live process is refused at the bound: {responses:?}"
        );
        assert_eq!(
            responses["root_update"]["ok"],
            json!(true),
            "the root is never refused for the bound: {responses:?}"
        );
        assert_eq!(
            responses["extra_after_exit"]["ok"],
            json!(true),
            "an exited reporter's mark makes room: {responses:?}"
        );
        let snapshot = worker.inspect().await.expect("inspect");
        let reference = snapshot.native_reference.expect("the journaled reference");
        assert_eq!(reference.native_reference, "root-update");

        std::io::Write::write_all(&mut gate, b"done\n").expect("release the reporter");
        worker
            .stop(TransactionId::new("transaction-full-reporters-stop").expect("transaction id"))
            .await
            .expect("stop the PTY");
        drop(worker);
        task.abort();
    }

    /// An exited reporter its parent has not reaped yet can never report
    /// again, so its mark makes room in a full reporter table.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_zombie_reporter_makes_room_in_a_full_reporter_table() {
        let root = crate::test_support::scoped_dir("ph-zombie-reporters-");
        let bound = WorkerConfig::new().identity_reporters;
        let FrozenRoot {
            worker,
            task,
            mut gate,
            results,
            ..
        } = start_frozen_root(
            &root,
            LIVE_REPORTERS,
            &["zombie_table".to_owned(), bound.to_string()],
        )
        .await;

        let responses = frozen_root_responses(&results, bound + 3).await;
        assert_eq!(
            responses["extra_refused"]["ok"],
            json!(false),
            "a further live process is refused at the bound: {responses:?}"
        );
        assert_eq!(
            responses["extra_after_zombie"]["ok"],
            json!(true),
            "an unreaped exited reporter's mark makes room: {responses:?}"
        );

        std::io::Write::write_all(&mut gate, b"done\n").expect("release the reporter");
        worker
            .stop(TransactionId::new("transaction-zombie-reporters-stop").expect("transaction id"))
            .await
            .expect("stop the PTY");
        drop(worker);
        task.abort();
    }

    /// Before the launch identity exists, a full reporter table does not keep
    /// the named-for-provider launch process from establishing it, even when
    /// that process is a descendant of the PTY root; a further reporter is
    /// still refused afterwards.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_reporter_table_admits_the_launch_process_descendant() {
        let root = crate::test_support::scoped_dir("ph-launch-beyond-bound-");
        let bound = WorkerConfig::new().identity_reporters;
        // The PTY root runs the interpreter under its own name, so only the
        // copy named `claude` it starts is the designated launch process.
        let launch_image = root.join("claude");
        std::fs::copy(python_image(), &launch_image).expect("copy the interpreter image");
        let script = root.join("launcher-reporter.py");
        std::fs::write(&script, LIVE_REPORTERS).expect("write the reporter script");
        let mut launch = initialize(
            &root,
            "",
            protocol::PROTOCOL_VERSION.get(),
            "identity-subagent-v1",
        );
        launch.executable = PathBuf::from(python_image());
        let FrozenRoot {
            worker,
            task,
            mut gate,
            results,
            ..
        } = start_reporter_root(
            &root,
            launch,
            script.clone(),
            Some(&script),
            &[
                "launch_beyond_bound".to_owned(),
                bound.to_string(),
                launch_image.display().to_string(),
            ],
        )
        .await;

        let responses = frozen_root_responses(&results, bound + 2).await;
        for index in 0..bound {
            assert_eq!(
                responses[&format!("filler_{index}")]["ok"],
                json!(true),
                "{responses:?}"
            );
        }
        assert_eq!(
            responses["launch_report"],
            json!({"ok": true, "launch_identity_accepted": true, "launch_identity_status": "accepted"}),
            "the launch process establishes the launch identity: {responses:?}"
        );
        assert_eq!(
            responses["extra_refused"]["ok"],
            json!(false),
            "the bound still refuses a further process: {responses:?}"
        );
        let snapshot = worker.inspect().await.expect("inspect");
        let pty_root = snapshot.child_process.expect("the PTY root");
        let launch = snapshot.launch_identity.expect("the launch identity");
        assert_ne!(
            launch.process, pty_root,
            "the launch process is a descendant"
        );
        assert_eq!(launch.native_reference, "launch-report");

        std::io::Write::write_all(&mut gate, b"done\n").expect("release the reporter");
        worker
            .stop(
                TransactionId::new("transaction-launch-beyond-bound-stop").expect("transaction id"),
            )
            .await
            .expect("stop the PTY");
        drop(worker);
        task.abort();
    }
}
