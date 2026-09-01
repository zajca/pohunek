use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pohunek_client::{Client, ClientOptions};
use pohunek_daemon::agent::{ForkMode, InputRules};
use pohunek_daemon::runtime::Worker;
use pohunek_daemon::store::{
    DesiredState, ResumeBinding, RuntimeRecord, SessionRecord, Store, StoredInputRules,
};
use pohunek_worker_protocol::{
    Dimensions, Initialize, InitializeLimits, LaunchIdentity, SecretEnv,
    SessionId as WorkerSessionId, StopPolicy, TransactionId, Version,
};
use protocol::{
    method, AgentActivity, AgentKind, AttachHeader, CwdSource, RuntimeState, SessionAttachParams,
    SessionCapabilities, SessionId, SessionInfo, SessionInputParams, SessionResizeParams,
    SessionRuntime, SessionState, StateSource, PROTOCOL_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const CONNECT_ATTEMPTS: usize = 300;
const CONNECT_DELAY: Duration = Duration::from_millis(50);
/// Allows a debug-built worker to render a snapshot and subsequent live output.
const ATTACH_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Incident-sized historical output that keeps offset zero retained.
const REPLAY_BURST_BYTES: usize = 2_655_396;
/// Visible marker emitted immediately after the historical output burst.
const REPLAY_BURST_MARKER: &[u8] = b"replay-burst-complete";
/// Retains the complete burst to exercise the non-evicted-history regression.
const REPLAY_HISTORY_BYTES: u64 = 3_000_000;

#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a real systemd user manager"]
#[expect(
    clippy::too_many_lines,
    reason = "the end-to-end scenario is intentionally linear so each lifecycle assertion remains ordered"
)]
async fn daemon_restart_and_sigkill_preserve_systemd_worker_runtime() {
    assert_eq!(
        std::env::var("POHUNEK_SYSTEMD_E2E").as_deref(),
        Ok("1"),
        "set POHUNEK_SYSTEMD_E2E=1 explicitly"
    );
    let daemon_bin = required_binary("POHUNEK_DAEMON_BIN");
    let worker_bin = required_binary("POHUNEK_WORKER_BIN");
    let fixture = Fixture::new();

    fixture.start_worker(&worker_bin);
    fixture.start_isolation_worker(&worker_bin);
    let worker = fixture.connect_worker("fixture-controller").await;
    let worker_id = worker.worker_id().await;
    let runtime_id = worker
        .initialize(fixture.initialize(worker_id.clone()))
        .await
        .expect("initialize systemd worker");
    let child_pid = worker
        .inspect()
        .await
        .expect("inspect initialized worker")
        .child_process
        .expect("child process")
        .pid;
    let pty_device = child_tty(child_pid);
    fixture.persist_record(&worker_id.to_string(), &runtime_id.to_string(), child_pid);
    let isolation_worker = fixture
        .connect_worker_for(
            &fixture.isolation_session_id,
            "fixture-isolation-controller",
        )
        .await;
    let isolation_worker_id = isolation_worker.worker_id().await;
    let isolation_runtime_id = isolation_worker
        .initialize(
            fixture.initialize_for(&fixture.isolation_session_id, isolation_worker_id.clone()),
        )
        .await
        .expect("initialize isolation worker");
    let isolation_child_pid = isolation_worker
        .inspect()
        .await
        .expect("inspect isolation worker")
        .child_process
        .expect("isolation child process")
        .pid;
    fixture.persist_record_for(
        &fixture.isolation_session_id,
        &fixture.isolation_worker_unit,
        &isolation_worker_id.to_string(),
        &isolation_runtime_id.to_string(),
        isolation_child_pid,
    );
    worker
        .release_controller()
        .await
        .expect("release bootstrap worker controller");
    isolation_worker
        .release_controller()
        .await
        .expect("release bootstrap isolation worker controller");
    drop(worker);
    drop(isolation_worker);
    tokio::time::sleep(CONNECT_DELAY).await;

    fixture.start_daemon(&daemon_bin);
    let first = fixture.inspect().await;
    eprintln!(
        "initial runtime inventory: {:?}",
        fixture.runtime_inventory().await
    );
    let worker_unit_pid = Fixture::main_pid(&fixture.worker_unit);
    let daemon_pid = Fixture::main_pid(&fixture.daemon_unit);
    assert_runtime(
        &first,
        child_pid,
        &worker_id.to_string(),
        &runtime_id.to_string(),
    );
    assert_eq!(child_tty(child_pid), pty_device);
    let rehydrated_activity = fixture
        .wait_for_activity(&fixture.session_id, AgentActivity::Working)
        .await;
    assert_eq!(rehydrated_activity.pid, child_pid);
    assert_runtime(
        &fixture.inspect_for(&fixture.isolation_session_id).await,
        isolation_child_pid,
        isolation_worker_id.as_str(),
        isolation_runtime_id.as_str(),
    );

    Fixture::systemctl(["restart", fixture.daemon_unit.as_str()]);
    let restarted = fixture.inspect().await;
    let restarted_daemon_pid = Fixture::main_pid(&fixture.daemon_unit);
    assert_ne!(restarted_daemon_pid, daemon_pid);
    assert_eq!(Fixture::main_pid(&fixture.worker_unit), worker_unit_pid);
    assert_runtime(
        &restarted,
        child_pid,
        &worker_id.to_string(),
        &runtime_id.to_string(),
    );
    assert_eq!(child_tty(child_pid), pty_device);
    let mut before_outage_attach = fixture.open_attach(&fixture.session_id).await;
    let before_outage_bytes = read_until_counter(&mut before_outage_attach, 5).await;
    assert_current_terminal_snapshot(&before_outage_bytes);
    let last_counter = extract_counters(&before_outage_bytes)
        .into_iter()
        .max()
        .expect("counter before daemon outage");

    Fixture::systemctl([
        "kill",
        "--kill-whom=main",
        "--signal=KILL",
        fixture.daemon_unit.as_str(),
    ]);
    tokio::time::sleep(Duration::from_millis(750)).await;
    let after_sigkill = fixture.inspect().await;
    let killed_daemon_pid = Fixture::main_pid(&fixture.daemon_unit);
    assert_ne!(killed_daemon_pid, restarted_daemon_pid);
    assert_eq!(Fixture::main_pid(&fixture.worker_unit), worker_unit_pid);
    assert_runtime(
        &after_sigkill,
        child_pid,
        &worker_id.to_string(),
        &runtime_id.to_string(),
    );
    assert_eq!(child_tty(child_pid), pty_device);
    let mut after_outage_attach = fixture.open_attach(&fixture.session_id).await;
    let after_outage_bytes = read_until_counter(&mut after_outage_attach, last_counter + 4).await;
    assert_current_terminal_snapshot(&after_outage_bytes);
    let current = extract_counters(&after_outage_bytes)
        .into_iter()
        .filter(|counter| *counter > last_counter)
        .collect::<Vec<_>>();
    assert!(
        current
            .iter()
            .copied()
            .max()
            .is_some_and(|counter| counter >= last_counter + 4),
        "replacement attach must repaint current state after the outage: {current:?}"
    );

    let mut gap_attach = fixture.open_attach(&fixture.isolation_session_id).await;
    let gap_bytes = read_until_counter(&mut gap_attach, 5).await;
    assert!(
        gap_bytes
            .windows(b"\x1b[2J\x1b[H".len())
            .any(|window| window == b"\x1b[2J\x1b[H"),
        "small-ring fresh attach must receive terminal repaint after an explicit gap"
    );

    let original_created_at = after_sigkill.created_at.clone();
    let mut client = Client::connect_local_with_options(
        &fixture.socket,
        ClientOptions::default().with_request_timeout(Duration::from_secs(15)),
    )
    .await
    .expect("connect final daemon");
    client
        .call::<method::SessionResize>(SessionResizeParams {
            session_id: SessionId(fixture.session_id.clone()),
            cols: 100,
            rows: 30,
        })
        .await
        .expect("resize same runtime after daemon reconnect");
    client
        .call::<method::SessionInput>(SessionInputParams {
            session_id: SessionId(fixture.session_id.clone()),
            text: "post-reconnect-input".to_owned(),
            wait: None,
        })
        .await
        .expect("input same runtime after daemon reconnect");
    let attach = client
        .call::<method::SessionAttach>(SessionAttachParams {
            session_id: SessionId(fixture.session_id.clone()),
            initial_dimensions: None,
            origin_session_id: None,
            origin_daemon_id: None,
            origin_worker_id: None,
        })
        .await
        .expect("open attach stream after daemon reconnect");
    let mut post_reconnect_stream = fixture.redeem_attach(&attach.stream_id).await;
    let delayed_submit_output =
        read_until_marker(&mut post_reconnect_stream, b"input:post-reconnect-input").await;
    assert!(
        delayed_submit_output
            .windows(b"input:post-reconnect-input".len())
            .any(|window| window == b"input:post-reconnect-input"),
        "worker-owned delayed submit must complete after daemon reconnect"
    );
    let post_reconnect = fixture.inspect().await;
    assert_eq!((post_reconnect.cols, post_reconnect.rows), (100, 30));
    assert_eq!(post_reconnect.pid, child_pid);
    client
        .call::<method::SessionStop>(SessionId(fixture.session_id.clone()))
        .await
        .expect("stop session through replacement daemon");
    let recovered_result = client
        .call::<method::SessionResume>(SessionId(fixture.session_id.clone()))
        .await;
    if recovered_result.is_err() {
        fixture.print_status();
    }
    let recovered = recovered_result
        .expect("recover terminal session through native reference")
        .session;
    let recovered_runtime = recovered.runtime.as_ref().expect("recovered runtime");
    let recovered_worker_id = recovered_runtime
        .worker_id
        .as_deref()
        .expect("recovered worker id");
    let recovered_runtime_id = recovered_runtime
        .runtime_id
        .as_deref()
        .expect("recovered runtime id");
    assert_eq!(recovered.id.0, fixture.session_id);
    assert_eq!(recovered.created_at, original_created_at);
    assert_ne!(recovered.pid, child_pid);
    assert_ne!(recovered_worker_id, worker_id.as_str());
    assert_ne!(recovered_runtime_id, runtime_id.as_str());
    let persisted = Store::new(fixture.data_home.join("pohunek/metadata.jsonl"))
        .load_sessions()
        .expect("load recovered logical record")
        .into_iter()
        .find(|record| record.session_id == fixture.session_id)
        .expect("recovered logical record");
    assert_eq!(persisted.transaction, None);
    assert_eq!(persisted.desired_state, DesiredState::Running);
    assert_eq!(
        persisted.runtime.worker_id.as_deref(),
        Some(recovered_worker_id)
    );
    assert_eq!(
        persisted.runtime.runtime_id.as_deref(),
        Some(recovered_runtime_id)
    );

    let repeated = client
        .call::<method::SessionResume>(SessionId(fixture.session_id.clone()))
        .await
        .expect_err("a live recovered runtime is not recoverable again");
    assert_eq!(
        repeated.to_protocol_error().code,
        "session_runtime_not_recoverable"
    );
    let after_repeated = fixture.inspect().await;
    assert_runtime(
        &after_repeated,
        recovered.pid,
        recovered_worker_id,
        recovered_runtime_id,
    );

    Fixture::systemctl([
        "kill",
        "--kill-whom=main",
        "--signal=KILL",
        fixture.isolation_worker_unit.as_str(),
    ]);
    let lost_isolation = fixture
        .wait_for_runtime_state(&fixture.isolation_session_id, RuntimeState::Lost)
        .await;
    assert_eq!(
        lost_isolation.runtime.expect("isolation runtime").state,
        RuntimeState::Lost
    );
    assert_eq!(
        Fixture::main_pid(&fixture.isolation_worker_unit),
        0,
        "Restart=no must not recreate a killed worker or fake PTY continuity"
    );
    assert_runtime(
        &fixture.inspect().await,
        recovered.pid,
        recovered_worker_id,
        recovered_runtime_id,
    );

    client
        .call::<method::SessionStop>(SessionId(fixture.session_id.clone()))
        .await
        .expect("stop recovered session");

    eprintln!(
        "systemd lifecycle passed: worker_pid={worker_unit_pid} child_pid={child_pid} \
         pty_device={} \
         runtime_id={runtime_id} recovered_runtime_id={recovered_runtime_id} \
         daemon_pids={daemon_pid},{restarted_daemon_pid},{killed_daemon_pid}",
        pty_device.display()
    );
}

struct Fixture {
    root: PathBuf,
    runtime_home: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    state_home: PathBuf,
    session_id: String,
    worker_unit: String,
    isolation_session_id: String,
    isolation_worker_unit: String,
    daemon_unit: String,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let unique = format!(
            "{}{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time after epoch")
                .subsec_nanos()
        );
        let root = std::env::temp_dir().join(format!("pohunek-systemd-e2e-{unique}"));
        let runtime_home = root.join("runtime");
        let config_home = root.join("config");
        let data_home = root.join("data");
        let state_home = root.join("state");
        for path in [&runtime_home, &config_home, &data_home, &state_home] {
            std::fs::create_dir_all(path).expect("create fixture XDG directory");
        }
        let session_id = format!("s-{unique}");
        let isolation_session_id = format!("s-{unique}9");
        Self {
            socket: runtime_home.join("pohunek/daemon.sock"),
            worker_unit: format!("pohunek-session@{session_id}.service"),
            isolation_worker_unit: format!("pohunek-session@{isolation_session_id}.service"),
            daemon_unit: format!("pohunek-rfc-daemon-{unique}.service"),
            root,
            runtime_home,
            config_home,
            data_home,
            state_home,
            session_id,
            isolation_session_id,
        }
    }

    fn start_worker(&self, worker_bin: &Path) {
        self.systemd_run(
            &self.worker_unit,
            worker_bin,
            &["--session-id", self.session_id.as_str()],
            false,
        );
    }

    fn start_isolation_worker(&self, worker_bin: &Path) {
        self.systemd_run(
            &self.isolation_worker_unit,
            worker_bin,
            &["--session-id", self.isolation_session_id.as_str()],
            false,
        );
    }

    fn start_daemon(&self, daemon_bin: &Path) {
        self.systemd_run(&self.daemon_unit, daemon_bin, &[], true);
    }

    fn systemd_run(&self, unit: &str, binary: &Path, arguments: &[&str], restart: bool) {
        let restart_policy = if restart { "on-failure" } else { "no" };
        let mut command = Command::new("systemd-run");
        command
            .args(["--user", "--unit", unit])
            .args(["--property", "Type=notify"])
            .args(["--property", "NotifyAccess=main"])
            .args(["--property", &format!("Restart={restart_policy}")])
            .args(["--property", "RestartSec=100ms"])
            .args(["--property", "KillMode=control-group"])
            .arg(format!(
                "--setenv=XDG_RUNTIME_DIR={}",
                self.runtime_home.display()
            ))
            .arg(format!(
                "--setenv=XDG_CONFIG_HOME={}",
                self.config_home.display()
            ))
            .arg(format!(
                "--setenv=XDG_DATA_HOME={}",
                self.data_home.display()
            ))
            .arg(format!(
                "--setenv=XDG_STATE_HOME={}",
                self.state_home.display()
            ))
            .arg("--setenv=POHUNEK_OBSERVE_EXTERNAL_AGENTS=0")
            .arg(binary)
            .args(arguments);
        let output = command.output().expect("run transient systemd unit");
        assert_success(&output);
    }

    async fn connect_worker(&self, daemon_id: &str) -> Worker {
        self.connect_worker_for(&self.session_id, daemon_id).await
    }

    async fn connect_worker_for(&self, session_id: &str, daemon_id: &str) -> Worker {
        let socket = self
            .runtime_home
            .join("pohunek/workers")
            .join(session_id)
            .join(pohunek_paths::WORKER_SOCKET_NAME);
        for _ in 0..CONNECT_ATTEMPTS {
            if let Ok(worker) = Worker::connect(&socket, session_id, daemon_id).await {
                return worker;
            }
            tokio::time::sleep(CONNECT_DELAY).await;
        }
        self.print_status();
        panic!("worker socket did not become ready: {}", socket.display());
    }

    fn initialize(&self, worker_id: pohunek_worker_protocol::WorkerId) -> Initialize {
        self.initialize_for(&self.session_id, worker_id)
    }

    fn initialize_for(
        &self,
        session_id: &str,
        worker_id: pohunek_worker_protocol::WorkerId,
    ) -> Initialize {
        Initialize {
            session_id: WorkerSessionId::new(session_id).expect("session id"),
            transaction_id: TransactionId::new(format!("create-{session_id}"))
                .expect("transaction id"),
            expected_worker_id: worker_id,
            launch: LaunchIdentity {
                agent: "claude".to_owned(),
                agent_base: "claude".to_owned(),
                reference_kind: Some("id".to_owned()),
            },
            executable: PathBuf::from("/bin/sh"),
            arguments: vec![
                "-c".to_owned(),
                format!(
                    "printf '\\033]0;working\\007'; \
                 head -c {REPLAY_BURST_BYTES} /dev/zero | tr '\\000' 'R'; \
                 printf '\\n{}\\n'; \
                 (n=0; while :; do n=$((n+1)); printf 'counter:%04d\\n' \"$n\"; \
                 sleep 0.1; done) & \
                 while IFS= read -r line; do printf 'input:%s\\n' \"$line\"; done",
                    std::str::from_utf8(REPLAY_BURST_MARKER).expect("replay marker is UTF-8")
                ),
            ],
            cwd: self.root.clone(),
            dimensions: Dimensions::new(80, 24).expect("dimensions"),
            environment: SecretEnv::new(BTreeMap::new()).expect("environment"),
            limits: InitializeLimits::new(
                if session_id == self.isolation_session_id {
                    128
                } else {
                    REPLAY_HISTORY_BYTES
                },
                100_000,
                128,
                60_000,
            )
            .expect("limits"),
            stop_policy: StopPolicy::new(250).expect("stop policy"),
            hook_protocol_version: Version::new(1).expect("hook protocol version"),
            public_protocol_version: PROTOCOL_VERSION.get(),
        }
    }

    fn persist_record(&self, worker_id: &str, runtime_id: &str, child_pid: u32) {
        self.persist_record_for(
            &self.session_id,
            &self.worker_unit,
            worker_id,
            runtime_id,
            child_pid,
        );
    }

    fn persist_record_for(
        &self,
        session_id: &str,
        worker_unit: &str,
        worker_id: &str,
        runtime_id: &str,
        child_pid: u32,
    ) {
        let now = "2026-07-23T00:00:00Z".to_owned();
        let info = SessionInfo {
            id: SessionId(session_id.to_owned()),
            external: Some(false),
            capabilities: SessionCapabilities {
                resume: true,
                fork: true,
            },
            name: Some("systemd durability e2e".to_owned()),
            agent: "claude".to_owned(),
            agent_base: AgentKind::Claude,
            cwd: self.root.clone(),
            cwd_source: Some(CwdSource::Launch),
            pid: child_pid,
            runtime: Some(SessionRuntime {
                state: RuntimeState::Live,
                runtime_generation: protocol::RuntimeGeneration::new(1),
                worker_id: Some(worker_id.to_owned()),
                runtime_id: Some(runtime_id.to_owned()),
                started_at: Some(now.clone()),
                last_connected_at: Some(now.clone()),
                loss_reason: None,
            }),
            cols: 80,
            rows: 24,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: Some("native-systemd-recovery".to_owned()),
            native_session_path: None,
            project_id: None,
            project_label: None,
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            metadata: BTreeMap::new(),
            created_at: now.clone(),
            updated_at: now,
            exit_code: None,
        };
        let store = Store::new(self.data_home.join("pohunek/metadata.jsonl"));
        store
            .record_session(&SessionRecord {
                schema_version: 1,
                session_id: session_id.to_owned(),
                desired_state: DesiredState::Running,
                transaction: None,
                info,
                recovery: Some(ResumeBinding {
                    session_id: session_id.to_owned(),
                    name: Some("systemd durability e2e".to_owned()),
                    agent: "claude".to_owned(),
                    agent_base: AgentKind::Claude,
                    cwd: self.root.clone(),
                    cols: 80,
                    rows: 24,
                    native_session_id: Some("native-systemd-recovery".to_owned()),
                    native_session_path: None,
                    project_id: None,
                    is_linked_worktree: None,
                    metadata: BTreeMap::new(),
                    program: "/bin/sh".to_owned(),
                    args: vec!["-c".to_owned(), "while :; do sleep 1; done".to_owned()],
                    input_rules: StoredInputRules::from(InputRules::unrestricted(
                        false,
                        Duration::from_millis(150),
                    )),
                    resume_mode: Some(pohunek_daemon::agent::ResumeMode::Flag),
                    ref_kind: Some(pohunek_daemon::agent::SessionRefKind::Id),
                    resumable: true,
                    fork_mode: Some(ForkMode::ClaudeSession),
                    fork_resume_mode: Some(pohunek_daemon::agent::ResumeMode::Flag),
                    fork_ref_kind: Some(pohunek_daemon::agent::SessionRefKind::Id),
                    forkable: true,
                }),
                native_identity_ordering: None,
                runtime: RuntimeRecord {
                    state: RuntimeState::Live,
                    worker_id: Some(worker_id.to_owned()),
                    runtime_id: Some(runtime_id.to_owned()),
                    unit_name: Some(worker_unit.to_owned()),
                    reason: None,
                },
            })
            .expect("persist logical session");
    }

    async fn inspect(&self) -> SessionInfo {
        self.inspect_for(&self.session_id).await
    }

    async fn runtime_inventory(&self) -> protocol::RuntimeInventoryResult {
        let mut client = Client::connect_local(&self.socket)
            .await
            .expect("connect runtime inventory client");
        client
            .call::<method::SessionRuntimeInventory>(())
            .await
            .expect("inspect runtime inventory")
    }

    async fn inspect_for(&self, session_id: &str) -> SessionInfo {
        for _ in 0..CONNECT_ATTEMPTS {
            if let Ok(mut client) = Client::connect_local(&self.socket).await {
                if let Ok(info) = client
                    .call::<method::SessionInspect>(SessionId(session_id.to_owned()))
                    .await
                {
                    return info;
                }
            }
            tokio::time::sleep(CONNECT_DELAY).await;
        }
        self.print_status();
        panic!("daemon did not expose reconciled session");
    }

    async fn wait_for_runtime_state(
        &self,
        session_id: &str,
        expected: RuntimeState,
    ) -> SessionInfo {
        for _ in 0..CONNECT_ATTEMPTS {
            let info = self.inspect_for(session_id).await;
            if info
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.state == expected)
            {
                return info;
            }
            tokio::time::sleep(CONNECT_DELAY).await;
        }
        self.print_status();
        panic!("session {session_id} did not reach runtime state {expected:?}");
    }

    async fn wait_for_activity(&self, session_id: &str, expected: AgentActivity) -> SessionInfo {
        for _ in 0..CONNECT_ATTEMPTS {
            let info = self.inspect_for(session_id).await;
            if info.activity == Some(expected) {
                return info;
            }
            tokio::time::sleep(CONNECT_DELAY).await;
        }
        self.print_status();
        panic!("session {session_id} did not reach activity {expected:?}");
    }

    async fn open_attach(&self, session_id: &str) -> UnixStream {
        let mut client = Client::connect_local(&self.socket)
            .await
            .expect("connect attach control");
        let attach = client
            .call::<method::SessionAttach>(SessionAttachParams {
                session_id: SessionId(session_id.to_owned()),
                initial_dimensions: None,
                origin_session_id: None,
                origin_daemon_id: None,
                origin_worker_id: None,
            })
            .await
            .expect("open attach");
        self.redeem_attach(&attach.stream_id).await
    }

    async fn redeem_attach(&self, stream_id: &str) -> UnixStream {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .expect("connect raw attach");
        let header = serde_json::to_vec(&AttachHeader {
            attach: stream_id.to_owned(),
        })
        .expect("serialize attach header");
        stream
            .write_all(&header)
            .await
            .expect("write attach header");
        stream
            .write_all(b"\n")
            .await
            .expect("terminate attach header");
        stream
    }

    fn main_pid(unit: &str) -> u32 {
        let output = Self::systemctl_output(["show", unit, "-p", "MainPID", "--value"]);
        String::from_utf8(output.stdout)
            .expect("main PID utf8")
            .trim()
            .parse()
            .expect("numeric main PID")
    }

    fn systemctl<const N: usize>(arguments: [&str; N]) {
        let output = Self::systemctl_output(arguments);
        assert_success(&output);
    }

    fn systemctl_output<const N: usize>(arguments: [&str; N]) -> Output {
        Command::new("systemctl")
            .arg("--user")
            .args(arguments)
            .output()
            .expect("run systemctl")
    }

    fn print_status(&self) {
        let _ = Command::new("systemctl")
            .args([
                "--user",
                "status",
                self.worker_unit.as_str(),
                self.daemon_unit.as_str(),
                "--no-pager",
            ])
            .status();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for unit in [
            &self.daemon_unit,
            &self.worker_unit,
            &self.isolation_worker_unit,
        ] {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", unit])
                .status();
            let _ = Command::new("systemctl")
                .args(["--user", "reset-failed", unit])
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn required_binary(name: &str) -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os(name)
            .unwrap_or_else(|| panic!("{name} must point to a built test binary")),
    );
    assert!(path.is_absolute(), "{name} must be absolute");
    assert!(path.is_file(), "{name} does not exist: {}", path.display());
    path
}

fn assert_runtime(info: &SessionInfo, child_pid: u32, worker_id: &str, runtime_id: &str) {
    assert_eq!(info.pid, child_pid, "session {} PID", info.id.0);
    let runtime = info.runtime.as_ref().expect("session runtime");
    assert_eq!(
        runtime.state,
        RuntimeState::Live,
        "session {} runtime: {runtime:?}",
        info.id.0
    );
    assert_eq!(
        runtime.worker_id.as_deref(),
        Some(worker_id),
        "session {} worker ID",
        info.id.0
    );
    assert_eq!(
        runtime.runtime_id.as_deref(),
        Some(runtime_id),
        "session {} runtime ID",
        info.id.0
    );
}

fn child_tty(child_pid: u32) -> PathBuf {
    std::fs::read_link(format!("/proc/{child_pid}/fd/0")).expect("read child PTY device")
}

async fn read_until_counter(stream: &mut UnixStream, target: u64) -> Vec<u8> {
    tokio::time::timeout(ATTACH_READ_TIMEOUT, async {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).await.expect("read attach output");
            assert!(count > 0, "attach closed before counter {target}");
            output.extend_from_slice(&buffer[..count]);
            if extract_counters(&output)
                .into_iter()
                .any(|counter| counter >= target)
            {
                return output;
            }
        }
    })
    .await
    .expect("counter arrives before timeout")
}

async fn read_until_marker(stream: &mut UnixStream, marker: &[u8]) -> Vec<u8> {
    tokio::time::timeout(ATTACH_READ_TIMEOUT, async {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).await.expect("read attach output");
            assert!(count > 0, "attach closed before marker");
            output.extend_from_slice(&buffer[..count]);
            if output.windows(marker.len()).any(|window| window == marker) {
                return output;
            }
        }
    })
    .await
    .expect("marker arrives before timeout")
}

fn assert_current_terminal_snapshot(bytes: &[u8]) {
    assert!(
        bytes
            .windows(b"\x1b[2J\x1b[H".len())
            .any(|window| window == b"\x1b[2J\x1b[H"),
        "fresh attach must start with a complete terminal repaint"
    );
    let marker_start = bytes
        .windows(REPLAY_BURST_MARKER.len())
        .position(|window| window == REPLAY_BURST_MARKER)
        .expect("fresh attach snapshot must contain the visible completion marker");
    let after_marker = marker_start + REPLAY_BURST_MARKER.len();
    assert!(
        !bytes[after_marker..]
            .windows(REPLAY_BURST_MARKER.len())
            .any(|window| window == REPLAY_BURST_MARKER),
        "fresh attach snapshot must paint the burst marker exactly once"
    );
}

fn extract_counters(bytes: &[u8]) -> Vec<u64> {
    let marker = b"counter:";
    let mut counters = Vec::new();
    let mut offset = 0;
    while let Some(relative) = bytes[offset..]
        .windows(marker.len())
        .position(|window| window == marker)
    {
        let start = offset + relative + marker.len();
        let end = bytes[start..]
            .iter()
            .position(|byte| !byte.is_ascii_digit())
            .map_or(bytes.len(), |relative_end| start + relative_end);
        if end > start {
            if let Ok(value) = std::str::from_utf8(&bytes[start..end])
                .unwrap_or_default()
                .parse()
            {
                counters.push(value);
            }
        }
        offset = end.max(start + 1);
        if offset >= bytes.len() {
            break;
        }
    }
    counters
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
