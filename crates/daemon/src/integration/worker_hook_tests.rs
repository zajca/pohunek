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
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use pohunek_test_support::wait::wait_until;
    use pohunek_worker_protocol::{
        InspectSnapshot, SubagentPhase, SubagentSnapshot, TransactionId,
    };
    use serde::Deserialize;
    use serde_json::{json, Value};

    use super::{start_worker_with_schema, state_asset};
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

    /// A sanitized provider hook input.
    #[derive(Debug, Deserialize)]
    struct Event {
        hook_event_name: String,
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
        /// Runs a shell command, such as the crafted-claim client, which
        /// only Linux runs.
        #[cfg(target_os = "linux")]
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
                #[cfg(target_os = "linux")]
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
        assert_eq!(
            &journaled_rows(&snapshot),
            final_rows,
            "rows after the last step: {:?}",
            snapshot.subagents
        );
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
    fn claude_hook(event: &Event) -> Step {
        let action = match event.hook_event_name.as_str() {
            "SubagentStart" => "subagent-start",
            "SubagentStop" => "subagent-stop",
            other => panic!("the replay has no state hook action for recorded {other}"),
        };
        Step::Hook {
            asset: state_asset(CLAUDE),
            action,
            payload: json!({
                "hook_event_name": event.hook_event_name,
                "agent_id": event.agent_id,
                "agent_type": event.agent_type,
            }),
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
        let resumed = sequence.events[0].agent_id.clone();
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
        let woken_id = recorded("self_wake").events[0].agent_id.clone();
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
            .filter(|event| !event.agent_type.is_empty())
            .map(claude_hook)
            .collect();
        steps.push(Step::Checkpoint(final_rows.clone()));
        // Fresh ids of the recorded helper shape: `a` and 16 hex digits.
        steps.extend((0..HELPER_STOPS).map(|index| {
            claude_hook(&Event {
                hook_event_name: "SubagentStop".to_owned(),
                agent_id: format!("af{index:015x}"),
                agent_type: String::new(),
            })
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
            agent_id: start.agent_id.clone(),
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
            row(repeated, &start.agent_id).revision,
            row(first, &start.agent_id).revision
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

    /// The end of the runtime retires a running row as lost.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_subagent_is_lost_when_its_runtime_ends() {
        let root = crate::test_support::scoped_dir("ph-subagent-lost-");
        let sequence = recorded("spawn_handback_resume");
        let start = &sequence.events[0];
        let running = claude_rows(&[Row {
            agent_id: start.agent_id.clone(),
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
            agent_id: start.agent_id.clone(),
            lifecycle: SubagentPhase::Lost,
        }]);
        assert_eq!(journaled_rows(&snapshot), lost, "{:?}", snapshot.subagents);
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
        use std::path::{Path, PathBuf};
        use std::sync::{Arc, Mutex, OnceLock};

        use pohunek_worker_protocol::SubagentPhase;
        use serde_json::Value;
        use tracing_subscriber::layer::{Context, SubscriberExt};

        use super::{claude_rows, quoted, replay, row, Row, Rows, Step, CLAUDE, CODEX};

        /// Claim type of a subagent start.
        const START: &str = "subagent_start";

        /// Claim type of a subagent stop.
        const STOP: &str = "subagent_stop";

        /// Synthetic subagent id of the recorded Claude shape.
        const REORDERED: &str = "a1b3d5f7c9e0a2b4d";

        /// Subagents the worker runs at once: `ACTIVE_SUBAGENT_CAPACITY` in
        /// `crates/session-worker/src/server.rs`, which is private to the worker.
        const ACTIVE_CAPACITY: usize = 64;

        /// Name of the worker event for a claim that changed no phase.
        const IGNORED_EVENT: &str = "worker.subagent.claim.ignored";

        /// Name of the worker event for a claim that hit a bound.
        const REFUSED_EVENT: &str = "worker.subagent.claim.refused";

        /// Crafted-claim client: `<results file> <provider>` and then claims
        /// as `<claim type>:<sequence>:<subagent id>`, sent one after the other
        /// by the process that runs the client, which reports itself.
        ///
        /// Each answered claim appends `{"action", "sequence", "id",
        /// "response"}` to the results file; any failure goes to
        /// `<results file>.failure`, which the test reads.
        const CLIENT: &str = r#"
import json
import os
import socket
import sys

results_path, provider = sys.argv[1], sys.argv[2]

def record_failure(kind, value, trace):
    # The test reads this file, so a claim never fails without a reason.
    import traceback
    with open(results_path + ".failure", "a", encoding="utf-8") as handle:
        handle.write("".join(traceback.format_exception(kind, value, trace)))

sys.excepthook = record_failure

def start_identity(pid):
    with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
        return int(handle.read().rsplit(")", 1)[1].split()[19])

pid = os.getpid()
for claim in sys.argv[3:]:
    action, sequence, subagent_id = claim.split(":")
    request = {
        "type": action,
        "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
        "provider": provider,
        "pid": pid,
        "start_identity": start_identity(pid),
        "sequence": int(sequence),
        "subagent_id": subagent_id,
    }
    if action == "subagent_start":
        request.update({"agent_type": "general-purpose", "parent_id": None})
    else:
        request["outcome"] = "completed"
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
            "sequence": int(sequence),
            "id": subagent_id,
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
            script: PathBuf,
            results: PathBuf,
        }

        impl Client {
            fn new(root: &Path) -> Self {
                let script = root.join("crafted-claim.py");
                std::fs::write(&script, CLIENT).expect("write the crafted-claim client");
                Self {
                    script,
                    results: root.join("crafted-claim.results"),
                }
            }

            /// A step sending `claims` (claim type, sequence, subagent id) as
            /// `provider` from one process, in order.
            fn step(&self, provider: &str, claims: &[(&str, u64, &str)]) -> Step {
                let claims = claims
                    .iter()
                    .map(|(action, sequence, id)| format!("{action}:{sequence}:{id}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                Step::Command(format!(
                    "python3 -I {} {} {provider} {claims}",
                    quoted(&self.script),
                    quoted(&self.results)
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

        /// The worker's subagent claim events, captured on the test thread.
        ///
        /// These scenarios serve the worker in process on a current-thread
        /// runtime, so the worker logs on the thread whose default subscriber
        /// this installs.
        #[derive(Clone, Default)]
        struct ClaimLog(Arc<Mutex<Vec<Logged>>>);

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

            fn events(&self) -> Vec<Logged> {
                self.0.lock().expect("claim log").clone()
            }
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ClaimLog {
            fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
                let name = event.metadata().name();
                if name != IGNORED_EVENT && name != REFUSED_EVENT {
                    return;
                }
                let mut fields = ClaimFields::default();
                event.record(&mut fields);
                self.0.lock().expect("claim log").push(Logged {
                    name: name.to_owned(),
                    action: fields.action,
                    id: fields.id,
                    reason: fields.reason,
                });
            }
        }

        /// The string fields of a subagent claim event.
        #[derive(Default)]
        struct ClaimFields {
            action: String,
            id: String,
            reason: String,
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
