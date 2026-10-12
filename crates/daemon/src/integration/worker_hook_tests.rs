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
