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
        let sessions =
            crate::session::SessionRegistry::new(crate::session::SessionRegistryConfig::default());
        let (attention, _task) =
            crate::notifications::AttentionCoordinator::spawn(notifications.clone(), &sessions);
        let state = Arc::new(
            DaemonState::new(
                HealthInfo::new("test"),
                sessions,
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
        hook_schema: Some(hook_schema.to_owned()),
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
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use pohunek_test_support::wait::wait_until;
    use pohunek_worker_protocol::{
        InspectSnapshot, SubagentPhase, SubagentSnapshot, TransactionId,
    };
    use serde::Deserialize;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use super::{start_worker_with_schema, state_asset, DaemonStub, SESSION_ID};
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
        /// Reports an identity barrier and waits on the gate; the test checks
        /// these rows before it releases the gate.
        Checkpoint(Rows),
    }

    /// A real worker whose PTY script waits on a gate after its last step.
    struct HookRun {
        worker: Worker,
        task: tokio::task::JoinHandle<Result<(), pohunek_session_worker::WorkerError>>,
        /// Write end of the FIFO the PTY script reads one line from per pause.
        gate: std::fs::File,
        /// Sequence of the last identity barrier the worker recorded.
        barrier: Option<u64>,
        /// The worker's snapshot at each checkpoint, then after the last step.
        snapshots: Vec<InspectSnapshot>,
    }

    impl HookRun {
        /// Waits until the worker records an identity barrier newer than the
        /// last one and returns the snapshot at that point.
        ///
        /// The PTY script runs every hook to completion before it reports the
        /// barrier, so every earlier claim has been decided by then.
        async fn pass_barrier(&mut self) -> InspectSnapshot {
            let worker = &self.worker;
            let previous = self.barrier;
            let snapshot = Box::pin(wait_until(
                "the worker records the identity barrier",
                || async move {
                    let snapshot = worker.inspect().await.expect("inspect");
                    let sequence = snapshot.active_identity.as_ref()?.sequence;
                    previous
                        .is_none_or(|previous| sequence > previous)
                        .then_some(snapshot)
                },
            ))
            .await;
            self.barrier = snapshot
                .active_identity
                .as_ref()
                .map(|identity| identity.sequence);
            snapshot
        }

        /// Lets the PTY script continue past its current pause.
        ///
        /// Waits first until the wall clock is past every row timestamp in
        /// `snapshot`, so a transition after the pause is stamped later than
        /// every transition before it.
        async fn release(&mut self, snapshot: &InspectSnapshot) {
            let latest = snapshot
                .subagents
                .iter()
                .map(|row| row.updated_at_ms)
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

    /// Starts a real worker whose PTY runs `steps` with the real hooks, checks
    /// the rows at every checkpoint, and checks `final_rows` after the last
    /// step. The PTY stays up until [`HookRun::finish`].
    async fn replay(root: &Path, steps: Vec<Step>, final_rows: &Rows) -> HookRun {
        replay_checking(root, steps, Some(final_rows)).await
    }

    /// [`replay`], with the rows after the last step checked only when
    /// `final_rows` is given; the caller checks the last snapshot otherwise.
    async fn replay_checking(root: &Path, steps: Vec<Step>, final_rows: Option<&Rows>) -> HookRun {
        let gate_path = root.join("replay.gate");
        nix::unistd::mkfifo(&gate_path, nix::sys::stat::Mode::S_IRWXU).expect("create the gate");
        // Opened read-write so the test never blocks on a reader and the
        // script's read waits for a line rather than for end of file.
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate_path)
            .expect("open the gate");
        let barrier_input = root.join("barrier.json");
        std::fs::write(&barrier_input, br#"{"session_id":"barrier"}"#)
            .expect("write the barrier input");
        // The identity hook runs under an intermediate shell, so it reports that
        // shell rather than the PTY root, as in `report_through_worker`.
        let barrier = format!(
            "/bin/sh -c '/bin/sh \"$0\" session; true' {} < {}",
            quoted(&state_asset(CLAUDE)),
            quoted(&barrier_input)
        );
        let pause = format!("read -r step < {}", quoted(&gate_path));

        // Every hook runs before the script's last command, so the shell never
        // execs it and the hook's parent is the PTY root the worker attests.
        let mut lines = Vec::new();
        let mut checkpoints = Vec::new();
        for (index, step) in steps.into_iter().enumerate() {
            match step {
                Step::Hook {
                    asset,
                    action,
                    payload,
                } => {
                    let input = root.join(format!("hook-{index}.json"));
                    std::fs::write(&input, payload.to_string()).expect("write the hook input");
                    lines.push(format!(
                        "/bin/sh {} {action} < {}",
                        quoted(&asset),
                        quoted(&input)
                    ));
                }
                Step::Command(command) => lines.push(command),
                Step::Checkpoint(rows) => {
                    lines.extend([barrier.clone(), pause.clone()]);
                    checkpoints.push(rows);
                }
            }
        }
        lines.extend([barrier, pause]);

        let (worker, task, _socket) = start_worker_with_schema(
            root,
            &lines.join("\n"),
            protocol::PROTOCOL_VERSION.get(),
            SCHEMA,
        )
        .await;
        let mut run = HookRun {
            worker,
            task,
            gate,
            barrier: None,
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
    /// Each invalid input lists no task that could host a subagent, so if the
    /// hook took it as a snapshot, or skipped or truncated its entries, the
    /// running subagent would be settled; a checkpoint follows each one. A
    /// valid `Stop` whose final message exceeds every other action's input
    /// bound then settles the subagent.
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
        // Checkpoint 0 follows the start, and checkpoint `n + 1` the `n`th
        // input of `invalid_stop_inputs`.
        let mut steps = vec![claude_hook(start), Step::Checkpoint(running.clone())];
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
    /// started: its empty snapshot settles its own subagent and leaves the
    /// outer agent's running one alone, which the outer agent's own empty
    /// snapshot then settles (#883 D4).
    ///
    /// The nested hooks run under one intermediate shell, which stands in for
    /// the nested Claude: both of its hooks report that shell, while the
    /// outer hooks report the PTY root.
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
            "/bin/sh -c '/bin/sh \"$0\" subagent-start < \"$1\"; /bin/sh \"$0\" stop < \"$2\"; true' {} {} {}",
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

    /// The `stop` action sends each recorded task as its `id`, `type` and
    /// `status` alone, in order, with the hook's sequence and the agent
    /// process that runs the hook, to the worker socket only: a refused
    /// snapshot reaches no daemon (#883 D3).
    #[tokio::test(flavor = "multi_thread")]
    async fn the_stop_hook_forwards_only_task_ids_types_and_statuses() {
        /// A final assistant message the claim must not carry.
        const MESSAGE: &str = "Both background agents are still working.";
        let sequence = recorded("lost_stop_corrected_by_snapshot");
        let tasks = stop_tasks(&sequence, 1);
        let input = json!({
            "hook_event_name": "Stop",
            "stop_hook_active": false,
            "last_assistant_message": MESSAGE,
            "background_tasks": tasks,
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
        let expected: Vec<Value> = tasks
            .iter()
            .map(|task| json!({"id": task["id"], "type": task["type"], "status": task["status"]}))
            .collect();
        assert_eq!(claim["tasks"], Value::Array(expected), "{claim}");
        let dropped: Vec<&str> = tasks
            .iter()
            .flat_map(|task| ["description", "command", "agent_type"].map(|field| &task[field]))
            .filter_map(Value::as_str)
            .chain([MESSAGE])
            .collect();
        assert!(
            dropped.iter().any(|text| text.contains("cargo")),
            "the recorded tasks carry a shell command: {dropped:?}"
        );
        for text in dropped {
            assert!(!line.contains(text), "{text:?} is forwarded: {line}");
        }
        assert!(
            daemon.request_lines().is_empty(),
            "the snapshot has no daemon fallback: {:?}",
            daemon.request_lines()
        );
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

        use pohunek_worker_protocol::SubagentPhase;
        use serde_json::{json, Value};
        use tracing_subscriber::layer::{Context, SubscriberExt};

        use super::{
            claude_rows, journaled_rows, quoted, recorded, replay, replay_checking, row, Row, Rows,
            Step, CLAUDE, CODEX, SNAPSHOT_TASK_BOUND, TERMINAL_HISTORY,
        };

        /// Claim type of a subagent start.
        const START: &str = "subagent_start";

        /// Claim type of a subagent stop.
        const STOP: &str = "subagent_stop";

        /// Claim type of a main-`Stop` task snapshot.
        const SNAPSHOT: &str = "subagent_snapshot";

        /// Synthetic subagent id of the recorded Claude shape.
        const REORDERED: &str = "a1b3d5f7c9e0a2b4d";

        /// Subagents the worker runs at once: `ACTIVE_SUBAGENT_CAPACITY` in
        /// `crates/session-worker/src/server.rs`, which is private to the worker.
        const ACTIVE_CAPACITY: usize = 64;

        /// Longest snapshot task id the worker accepts: `MAX_SUBAGENT_ID_BYTES`.
        const TASK_ID_BOUND: usize = 256;

        /// Longest snapshot task type the worker accepts:
        /// `MAX_SNAPSHOT_TYPE_BYTES`.
        const TASK_TYPE_BOUND: usize = 64;

        /// Longest snapshot task status the worker accepts:
        /// `MAX_SNAPSHOT_STATUS_BYTES`.
        const TASK_STATUS_BOUND: usize = 32;

        /// Name of the worker event for a claim that changed no phase.
        const IGNORED_EVENT: &str = "worker.subagent.claim.ignored";

        /// Name of the worker event for a claim that hit a bound.
        const REFUSED_EVENT: &str = "worker.subagent.claim.refused";

        /// Name of the worker event for a subagent a snapshot completed or
        /// reopened.
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
    request = {
        "type": action,
        "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
        "provider": provider,
        "pid": agent_pid,
        "start_identity": agent_start,
        "sequence": sequence,
    }
    if action == "subagent_start":
        request.update({"subagent_id": claim["id"], "agent_type": "general-purpose", "parent_id": None})
    elif action == "subagent_stop":
        request.update({"subagent_id": claim["id"], "outcome": "completed"})
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
                let index = self.plans.replace(self.plans.get() + 1);
                let path = self.root.join(format!("crafted-plan-{index}.json"));
                std::fs::write(&path, plan.to_string()).expect("write the claim plan");
                Step::Command(format!(
                    "python3 -I {} {} {provider} {}",
                    quoted(&self.script),
                    quoted(&self.results),
                    quoted(&path)
                ))
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
            listed: u64,
            terminalized: u64,
            reopened: u64,
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

            /// `(subagent id, reason)` of each subagent a snapshot completed
            /// or reopened, in the order the worker logged them.
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
                    "subagent.snapshot.reopened" => &mut self.applied.reopened,
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

        /// R2 ordering against a main `Stop`: a self-woken subagent's start
        /// (sequence 30) is decided before an older snapshot (sequence 20)
        /// that does not list it yet. The snapshot is acknowledged and leaves
        /// the newer run running; a later empty snapshot (sequence 40) settles
        /// it (#883 D4, D6).
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
                ]
            );
            run.finish().await;
        }

        /// A terminal subagent that a newer main `Stop` snapshot lists as live
        /// is reopened: its stop at sequence 10 lost the race with a self-wake
        /// start that the snapshot at sequence 20 already shows. A snapshot at
        /// sequence 30 that lists another background agent but not this one
        /// settles it again, because a snapshot listed it during its current
        /// run (#883 D4).
        ///
        /// An older snapshot (sequence 8) that still lists the subagent after
        /// its stop was decided reopens nothing: R4 shows a stopping agent
        /// listed as running in its own stop, so a main `Stop` taken before
        /// that stop can list it too.
        ///
        /// The worker logs the reopen as `snapshot_listed`, the settlement as
        /// `snapshot_absent`, and the counts of each snapshot (#883 D5).
        #[tokio::test]
        async fn a_snapshot_listing_a_completed_subagent_reopens_it() {
            /// The listed subagent, of the recorded id shape.
            const LISTED: &str = "a2c6e0b4d8f1a5c9e";
            /// Another background agent the worker has no record of.
            const OTHER: &str = "a6e0c4b8d2f5a9e3c";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-reopen-listed-");
            let client = Client::new(&root);
            let steps = vec![
                client.step(CLAUDE, &[(START, 5, LISTED), (STOP, 10, LISTED)]),
                client.snapshot(8, &listed(&[LISTED])),
                Step::Checkpoint(one(LISTED, SubagentPhase::Completed)),
                client.snapshot(20, &listed(&[LISTED])),
                Step::Checkpoint(one(LISTED, SubagentPhase::Running)),
                client.snapshot(30, &listed(&[OTHER])),
            ];
            let run = replay(&root, steps, &one(LISTED, SubagentPhase::Completed)).await;
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
            let [completed, reopened, settled] = run.snapshots.as_slice() else {
                panic!("two checkpoints and the final rows: {:?}", run.snapshots);
            };
            let [completed, reopened, settled] =
                [completed, reopened, settled].map(|snapshot| row(snapshot, LISTED));
            assert!(
                completed.revision < reopened.revision && reopened.revision < settled.revision,
                "{completed:?} {reopened:?} {settled:?}"
            );
            assert_eq!(reopened.finished_at_ms, None, "{reopened:?}");
            assert_eq!(reopened.started_at_ms, completed.started_at_ms);
            assert!(
                reopened.updated_at_ms > completed.updated_at_ms,
                "{completed:?} {reopened:?}"
            );
            assert!(settled.finished_at_ms.is_some(), "{settled:?}");
            assert_eq!(
                log.reconciled(),
                [
                    (LISTED.to_owned(), "snapshot_listed".to_owned()),
                    (LISTED.to_owned(), "snapshot_absent".to_owned()),
                ]
            );
            // Every snapshot lists a subagent, a task type that can host
            // agents.
            assert_eq!(
                log.applied(),
                [
                    Applied {
                        hosts: true,
                        ..Applied::default()
                    },
                    Applied {
                        reopened: 1,
                        hosts: true,
                        ..Applied::default()
                    },
                    Applied {
                        terminalized: 1,
                        hosts: true,
                        ..Applied::default()
                    },
                ]
            );
            assert_eq!(log.events(), [], "no claim is ignored or refused");
            run.finish().await;
        }

        /// A snapshot that lists a terminal subagent while
        /// [`ACTIVE_CAPACITY`] listed subagents run cannot reopen it: the
        /// subagent stays completed, the snapshot is still acknowledged, and
        /// the worker logs a refused `snapshot` claim for it with reason
        /// `capacity`, beside the snapshot's counts (#883 D4, D5).
        #[tokio::test]
        async fn a_snapshot_reopen_beyond_the_active_capacity_is_refused() {
            /// The terminal subagent the snapshot lists, of the recorded id
            /// shape.
            const LISTED: &str = "a5e9c3f7b1d4a8e2c";
            /// Sequence of the snapshot, after every start and stop.
            const SNAPSHOT_SEQUENCE: u64 = 1_000;
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-reopen-capacity-");
            let client = Client::new(&root);
            let running_ids: Vec<String> = (0..ACTIVE_CAPACITY)
                .map(|index| format!("ad{index:015x}"))
                .collect();
            let mut claims = vec![(START, 1, LISTED), (STOP, 2, LISTED)];
            claims.extend(
                running_ids
                    .iter()
                    .zip(3_u64..)
                    .map(|(id, sequence)| (START, sequence, id.as_str())),
            );
            let mut rows: Rows = running_ids
                .iter()
                .map(|id| ((CLAUDE.to_owned(), id.clone()), SubagentPhase::Running))
                .collect();
            rows.insert(
                (CLAUDE.to_owned(), LISTED.to_owned()),
                SubagentPhase::Completed,
            );
            let mut every: Vec<&str> = running_ids.iter().map(String::as_str).collect();
            every.push(LISTED);
            let steps = vec![
                client.step(CLAUDE, &claims),
                Step::Checkpoint(rows.clone()),
                client.snapshot(SNAPSHOT_SEQUENCE, &listed(&every)),
            ];
            let run = replay(&root, steps, &rows).await;

            let mut expected: Vec<Answer> = claims
                .iter()
                .map(|(action, sequence, id)| Answer::new(action, *sequence, id, true))
                .collect();
            expected.push(snapshot_answer(SNAPSHOT_SEQUENCE, true));
            assert_eq!(client.answers(), expected);
            assert_eq!(
                log.events(),
                [Logged::new(REFUSED_EVENT, "snapshot", LISTED, "capacity")]
            );
            assert_eq!(log.reconciled(), [], "nothing is completed or reopened");
            assert_eq!(
                log.applied(),
                [Applied {
                    listed: u64::try_from(ACTIVE_CAPACITY).expect("the capacity fits u64"),
                    hosts: true,
                    ..Applied::default()
                }]
            );
            run.finish().await;
        }

        /// A running subagent that a main `Stop` snapshot (sequence 30) lists
        /// takes the snapshot's sequence, so a delayed stop between its start
        /// (10) and the snapshot (20) is stale and leaves it running; the
        /// listing changes no public state. A newer stop (40) completes it
        /// (#883 D4, ordering invariant).
        #[tokio::test]
        async fn a_listed_subagent_ignores_a_stop_older_than_the_listing() {
            /// The listed subagent, of the recorded id shape.
            const LISTED: &str = "a9c3e7b1d5f8a2c6e";
            let log = ClaimLog::default();
            let _subscriber = log.install();
            let root = crate::test_support::scoped_dir("ph-subagent-listed-stale-stop-");
            let client = Client::new(&root);
            let running = one(LISTED, SubagentPhase::Running);
            let steps = vec![
                client.step(CLAUDE, &[(START, 10, LISTED)]),
                Step::Checkpoint(running.clone()),
                client.snapshot(30, &listed(&[LISTED])),
                client.step(CLAUDE, &[(STOP, 20, LISTED)]),
                Step::Checkpoint(running),
                client.step(CLAUDE, &[(STOP, 40, LISTED)]),
            ];
            let run = replay(&root, steps, &one(LISTED, SubagentPhase::Completed)).await;
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
                [Logged::new(IGNORED_EVENT, "stop", LISTED, "stale_sequence")],
                "the stop older than the listing is ignored as stale"
            );
            let [started, listed_row, completed] = run.snapshots.as_slice() else {
                panic!("two checkpoints and the final rows: {:?}", run.snapshots);
            };
            let [started, listed_row, completed] =
                [started, listed_row, completed].map(|snapshot| row(snapshot, LISTED));
            assert_eq!(
                listed_row.revision, started.revision,
                "the listing changes no public state: {started:?} {listed_row:?}"
            );
            assert!(completed.revision > listed_row.revision, "{completed:?}");
            run.finish().await;
        }

        /// Records left running by lost stops cannot exhaust the subagent
        /// capacity (#883 D4).
        ///
        /// At capacity, a further start is refused and adds no row. One empty
        /// main `Stop` snapshot settles every stale record, the terminal
        /// history keeps its bound, and the refused subagent then starts.
        #[tokio::test(flavor = "multi_thread")]
        async fn stale_running_subagents_cannot_exhaust_the_capacity() {
            /// The start beyond the capacity, of the recorded id shape.
            const EXTRA: &str = "ae1f2a3b4c5d6e7f8";
            /// Sequence of the snapshot, after every start.
            const SNAPSHOT_SEQUENCE: u64 = 1_000;
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
                snapshot_answer(SNAPSHOT_SEQUENCE, true),
                Answer::new(START, SNAPSHOT_SEQUENCE + 1, EXTRA, true),
            ]);
            assert_eq!(client.answers(), expected);

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
        /// holds invalid text as a whole: it answers `ok: false` and settles
        /// nothing. A valid snapshot of the same agent then settles the
        /// subagent (#883 D4).
        ///
        /// Apart from the over-long type, which is a type that can host
        /// agents, each refused list would settle the subagent if it were
        /// applied, or applied without its invalid entry.
        #[tokio::test(flavor = "multi_thread")]
        async fn an_invalid_snapshot_task_list_is_refused_whole() {
            /// The running subagent, of the recorded id shape.
            const STALE: &str = "a3d7f1b5c9e2a6d0f";
            /// Sequence of the first refused snapshot; each next one is one
            /// higher.
            const FIRST_REFUSED: u64 = 10;
            /// Sequence of the valid snapshot, after every refused one.
            const VALID: u64 = 100;
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
                .map(|(tasks, sequence)| json!({"type": SNAPSHOT, "sequence": sequence, "tasks": tasks}))
                .collect();
            let steps = vec![
                client.step(CLAUDE, &[(START, 5, STALE)]),
                client.send(CLAUDE, &Value::Array(plan)),
                Step::Checkpoint(one(STALE, SubagentPhase::Running)),
                client.snapshot(VALID, std::slice::from_ref(&shell)),
            ];
            let run = replay(&root, steps, &one(STALE, SubagentPhase::Completed)).await;
            let mut expected = vec![Answer::new(START, 5, STALE, true)];
            expected.extend(
                refused
                    .iter()
                    .map(|sequence| snapshot_answer(*sequence, false)),
            );
            expected.push(snapshot_answer(VALID, true));
            assert_eq!(client.answers(), expected);
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

        use pohunek_worker_protocol::SubagentPhase;
        use serde_json::{json, Value};

        use super::{
            claude_rows, quoted, recorded, replay, state_asset, stop_tasks, Row, Rows, Step,
            CLAUDE, CODEX, OTHER_INPUT_BOUND, SESSION_ID,
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

        /// Test item 2: claims the worker acknowledges write no line. Claude
        /// and Codex subagent starts and stops and a valid main `Stop`
        /// snapshot run first; one failure exit after them leaves the only
        /// line.
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
            let mut completed = claude_rows(&[Row {
                agent_id: started.agent_id.clone(),
                lifecycle: SubagentPhase::Completed,
            }]);
            completed.insert((CODEX.to_owned(), codex_id), SubagentPhase::Completed);
            replay(&root, steps, &completed).await.finish().await;

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

/// Launch-identity scenarios of the frozen-root launch rule.
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

    /// A real worker whose PTY root is a reporter script under `identity-v1`.
    struct FrozenRoot {
        worker: crate::runtime::Worker,
        task: tokio::task::JoinHandle<Result<(), pohunek_session_worker::WorkerError>>,
        /// Write end of the FIFO the reporter reads one line from per pause.
        gate: std::fs::File,
        /// The frozen entrypoint script.
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

        let mut launch = initialize(root, "", protocol::PROTOCOL_VERSION.get(), "identity-v1");
        launch.launch = LaunchIdentity {
            agent: "hermes".to_owned(),
            agent_base: "hermes".to_owned(),
            reference_kind: None,
        };
        launch.executable.clone_from(&entrypoint);
        launch.arguments = [
            gate_path.display().to_string(),
            results.display().to_string(),
        ]
        .into_iter()
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
}
