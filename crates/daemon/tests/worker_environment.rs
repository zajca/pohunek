//! Real-process proof of the worker child-environment contract.
//!
//! A real `pohunek-sessiond` runs with the variables systemd and launchd give a
//! supervised job plus an unrelated variable, and a real PTY child prints its
//! environment. Protocol version seven must hand the child exactly the base
//! environment, the worker's `TERM`, the profile, and the worker's reserved
//! `POHUNEK_*` values. A profile value overrides the base one, while a profile
//! can neither shadow a reserved value nor pass a service-manager variable. The
//! previous protocol version receives the same contract as the current one.
//!
//! Build the worker first: `cargo build -p pohunek-session-worker --bin
//! pohunek-sessiond`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pohunek_daemon::runtime::{DataStream, Worker, WorkerError};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::{wait_until, HANG_GUARD};
use pohunek_test_support::worker_binary;
use pohunek_worker_protocol::{
    is_denylisted, read_frame, write_frame, AttachStart, BaseEnv, CloseReason, ControlCode,
    DaemonId, DataFrame, Dimensions, FrameHeader, FrameKind, Initialize, InitializeLimits,
    InputFragment, InputPolicy, LaunchIdentity, RuntimePhase, SecretBytes, SecretEnv, SessionId,
    StopPolicy, StreamId, TransactionId, Version, WriteId, CURRENT_VERSION, MAX_CONTROL_LINE_BYTES,
    MAX_DATA_PAYLOAD_BYTES, PREVIOUS_VERSION,
};

/// Hang guard for every wait on the worker socket, the child's exit and the
/// streams. The scenarios synchronize on readiness signals, never on this
/// bound; `await_pty_echo` reads live `Output` frames from an observer that
/// attached before the input existed.
const DEADLINE: Duration = HANG_GUARD;
/// Output page size; an environment listing fits in a few pages.
const PAGE_BYTES: u32 = 16 * 1024;
/// Short observation wait, because the child has already exited.
const PAGE_WAIT: Duration = Duration::from_millis(100);
/// Daemon identity presented to the worker.
const DAEMON_ID: &str = "daemon-environment-test";
/// Generation passed on the worker command line.
const GENERATION: &str = "abcd2345";
/// Value of the variable no allowlist names; it must never reach a child.
const UNRELATED_VALUE: &str = "worker-only-value";

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Variables a service manager sets on the worker it supervises.
const SERVICE_MANAGER_VARIABLES: &[(&str, &str)] = &[
    ("WATCHDOG_USEC", "30000000"),
    ("WATCHDOG_PID", "1"),
    ("INVOCATION_ID", "0123456789abcdef0123456789abcdef"),
    ("JOURNAL_STREAM", "8:12345"),
    ("MANAGERPID", "1"),
    ("SYSTEMD_EXEC_PID", "1"),
    ("XPC_SERVICE_NAME", "io.github.zajca.pohunek.test.worker"),
    ("XPC_FLAGS", "0x0"),
    ("__CFBundleIdentifier", "io.github.zajca.pohunek"),
    ("LaunchInstanceID", "00000000-0000-0000-0000-000000000000"),
    ("POHUNEK_BOOTSTRAP_TOKEN", "bootstrap-token"),
    ("POHUNEK_CONTROLLER_TOKEN", "controller-token"),
];

struct Fixture {
    root: PathBuf,
    session_id: String,
    notify: UnixDatagram,
    worker: tokio::process::Child,
    /// Owns `root`, which it removes when the fixture drops.
    _env: TestEnv,
}

impl Fixture {
    /// Starts a real worker whose own environment is fully controlled.
    fn start() -> Self {
        let binary = worker_binary();
        assert!(
            binary.is_file(),
            "{} is missing; build pohunek-sessiond first",
            binary.display()
        );
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let env = TestEnv::new().expect("create the fixture environment");
        let root = env.root().to_path_buf();
        for directory in [
            "runtime/pohunek/workers",
            "state/pohunek/workers",
            "data/pohunek",
            "config/pohunek",
            "cache/pohunek",
            "home",
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join(directory))
                .expect("create fixture directory");
        }
        let notify_path = root.join("notify.sock");
        let notify = UnixDatagram::bind(&notify_path).expect("bind notify socket");
        notify
            .set_read_timeout(Some(DEADLINE))
            .expect("notify read timeout");
        let session_id = format!("s-{}{sequence}", std::process::id());

        let mut command = tokio::process::Command::new(binary);
        command
            .args(["--session-id", &session_id])
            .args(["--worker-generation", GENERATION])
            .arg("--daemon-socket-path")
            .arg(root.join("runtime/pohunek/daemon.sock"))
            .env_clear()
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("HOME", root.join("home"))
            .env("NOTIFY_SOCKET", &notify_path)
            .env("UNRELATED_WORKER_VAR", UNRELATED_VALUE)
            .envs(SERVICE_MANAGER_VARIABLES.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let worker = command.spawn().expect("spawn pohunek-sessiond");
        Self {
            root,
            session_id,
            notify,
            worker,
            _env: env,
        }
    }

    fn wait_ready(&self) {
        // The worker reads NOTIFY_SOCKET from its own environment, proving
        // the service-manager variables really were present in the worker.
        let mut ready = [0_u8; 256];
        let length = self.notify.recv(&mut ready).expect("worker readiness");
        assert!(ready[..length].starts_with(b"READY=1"));
    }

    async fn connect(&mut self, maximum_version: Version) -> Worker {
        self.wait_ready();
        self.connect_controller(maximum_version).await
    }

    /// Acquires the controller lease once the worker is listening. The worker
    /// announces readiness only once, so a reconnect after a release skips
    /// [`Self::wait_ready`].
    async fn connect_controller(&mut self, maximum_version: Version) -> Worker {
        let socket = self
            .root
            .join("runtime/pohunek/workers")
            .join(&self.session_id)
            .join(pohunek_paths::WORKER_SOCKET_NAME);
        let child = std::cell::RefCell::new(&mut self.worker);
        let session_id = &self.session_id;
        wait_until("the worker to accept a controller", || async {
            match Worker::connect_with_range(
                &socket,
                session_id,
                DAEMON_ID,
                PREVIOUS_VERSION,
                maximum_version,
            )
            .await
            {
                Ok(worker) => Some(worker),
                Err(error) => {
                    assert!(
                        child
                            .borrow_mut()
                            .try_wait()
                            .expect("inspect worker")
                            .is_none(),
                        "worker exited before accepting a controller: {error}"
                    );
                    None
                }
            }
        })
        .await
    }

    fn initialize(&self, worker_id: pohunek_worker_protocol::WorkerId) -> Initialize {
        Initialize {
            session_id: SessionId::new(&self.session_id).expect("session id"),
            daemon_instance_id: Some(DaemonId::new(DAEMON_ID).expect("daemon id")),
            transaction_id: TransactionId::new("create-environment").expect("transaction id"),
            expected_worker_id: worker_id,
            launch: LaunchIdentity {
                agent: "codex".to_owned(),
                agent_base: "codex".to_owned(),
                reference_kind: Some("id".to_owned()),
            },
            executable: PathBuf::from("/usr/bin/env"),
            arguments: Vec::new(),
            cwd: self.root.clone(),
            // Wide enough that no environment line wraps in the PTY.
            dimensions: Dimensions::new(1000, 50).expect("dimensions"),
            environment: SecretEnv::new(BTreeMap::from([
                ("PROFILE_TOKEN".to_owned(), "profile-value".to_owned()),
                ("LANG".to_owned(), "C".to_owned()),
                ("NOTIFY_SOCKET".to_owned(), "/run/profile-notify".to_owned()),
                ("WATCHDOG_USEC".to_owned(), "1".to_owned()),
                ("XPC_SERVICE_NAME".to_owned(), "profile".to_owned()),
                ("POHUNEK_SESSION_ID".to_owned(), "s-spoofed".to_owned()),
                ("POHUNEK_DAEMON_ID".to_owned(), "daemon-spoofed".to_owned()),
                (
                    "POHUNEK_HOOK_TRACE_PATH".to_owned(),
                    self.root
                        .join("spoofed-trace.jsonl")
                        .to_str()
                        .expect("UTF-8 fixture path")
                        .to_owned(),
                ),
                ("POHUNEK_HOOK_TRACE_MAX_BYTES".to_owned(), "1".to_owned()),
            ]))
            .expect("profile environment"),
            base_environment: BaseEnv::new(BTreeMap::from([
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                ("HOME".to_owned(), "/home/base".to_owned()),
                ("LANG".to_owned(), "C.UTF-8".to_owned()),
                ("SHELL".to_owned(), "/bin/sh".to_owned()),
            ]))
            .expect("base environment"),
            limits: InitializeLimits::new(1_048_576, 1_048_576, 128, 60_000).expect("limits"),
            stop_policy: StopPolicy::new(500).expect("stop policy"),
            input_policy: InputPolicy::new(3000).expect("input policy"),
            hook_protocol_version: Version::new(1).expect("hook version"),
            public_protocol_version: 7,
            hook_schema: None,
            answers_program_status_probe: false,
        }
    }

    fn socket_path(&self) -> String {
        self.root
            .join("runtime/pohunek/workers")
            .join(&self.session_id)
            .join(pohunek_paths::WORKER_SOCKET_NAME)
            .to_str()
            .expect("UTF-8 fixture path")
            .to_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.worker.start_kill();
    }
}

/// Runs `/usr/bin/env` in the worker's PTY and returns the child environment.
async fn child_environment(worker: &Worker, initialize: Initialize) -> BTreeMap<String, String> {
    worker.initialize(initialize).await.expect("initialize");
    let exited = wait_until("the child to exit", || async {
        let snapshot = worker.inspect().await.expect("inspect worker");
        (snapshot.phase == RuntimePhase::Exited).then_some(snapshot)
    })
    .await;
    assert_eq!(exited.exit.and_then(|exit| exit.code), Some(0));

    let mut output = Vec::new();
    let mut after_offset = None;
    loop {
        let page = worker
            .read_output(after_offset, PAGE_BYTES, PAGE_WAIT)
            .await
            .expect("read child output");
        assert!(page.gap.is_none(), "environment listing must be retained");
        output.extend_from_slice(page.data.expose());
        after_offset = Some(page.next_offset);
        if !page.has_more {
            break;
        }
    }
    String::from_utf8(output)
        .expect("environment listing is UTF-8")
        .split("\r\n")
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line.split_once('=').expect("NAME=value line");
            (name.to_owned(), value.to_owned())
        })
        .collect()
}

#[tokio::test]
async fn current_worker_child_gets_only_base_term_profile_and_identity() {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(CURRENT_VERSION).await;
    assert_eq!(worker.selected_version().await, CURRENT_VERSION);
    let worker_id = worker.worker_id().await;
    assert!(pohunek_paths::valid_worker_id(worker_id.as_str()).is_some());
    let initialize = fixture.initialize(worker_id.clone());

    let environment = child_environment(&worker, initialize).await;

    let worker_instance_id = worker
        .worker_instance_id()
        .await
        .expect("runtime id")
        .to_string();
    let socket_path = fixture.socket_path();
    let daemon_socket = fixture.root.join("runtime/pohunek/daemon.sock");
    // The shared hook failure trace lives in the pohunek log directory,
    // `$XDG_STATE_HOME/pohunek/logs`.
    let hook_trace = fixture.root.join("state/pohunek/logs/pohunek-hooks.jsonl");
    let hook_trace_max_bytes = pohunek_logging::config::HOOK_TRACE_MAX_FILE_BYTES.to_string();
    let background_label_max_bytes =
        pohunek_worker_protocol::BACKGROUND_LABEL_MAX_BYTES.to_string();
    let background_program_max_bytes =
        pohunek_worker_protocol::BACKGROUND_PROGRAM_MAX_BYTES.to_string();
    let expected = BTreeMap::from([
        ("HOME", "/home/base"),
        ("LANG", "C"),
        ("PATH", "/usr/bin:/bin"),
        ("SHELL", "/bin/sh"),
        ("TERM", "xterm-256color"),
        ("PROFILE_TOKEN", "profile-value"),
        ("POHUNEK_ENV", "1"),
        ("POHUNEK_DAEMON_ID", DAEMON_ID),
        (
            "POHUNEK_HOOK_TRACE_PATH",
            hook_trace.to_str().expect("UTF-8 fixture path"),
        ),
        (
            "POHUNEK_HOOK_TRACE_MAX_BYTES",
            hook_trace_max_bytes.as_str(),
        ),
        (
            "POHUNEK_BACKGROUND_LABEL_MAX_BYTES",
            background_label_max_bytes.as_str(),
        ),
        (
            "POHUNEK_BACKGROUND_PROGRAM_MAX_BYTES",
            background_program_max_bytes.as_str(),
        ),
        ("POHUNEK_NATIVE_REFERENCE_KIND", "id"),
        ("POHUNEK_PROTOCOL_VERSION", "7"),
        ("POHUNEK_WORKER_INSTANCE_ID", worker_instance_id.as_str()),
        ("POHUNEK_SESSION_ID", fixture.session_id.as_str()),
        (
            "POHUNEK_SOCKET_PATH",
            daemon_socket.to_str().expect("UTF-8 fixture path"),
        ),
        ("POHUNEK_WORKER_HOOK_PROTOCOL_VERSION", "1"),
        ("POHUNEK_WORKER_ID", worker_id.as_str()),
        ("POHUNEK_WORKER_SOCKET_PATH", socket_path.as_str()),
    ])
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect::<BTreeMap<_, _>>();
    assert_eq!(environment, expected);
}

#[tokio::test]
async fn previous_version_child_gets_the_base_environment_without_service_manager_variables() {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(PREVIOUS_VERSION).await;
    assert_eq!(worker.selected_version().await, PREVIOUS_VERSION);
    let initialize = fixture.initialize(worker.worker_id().await);

    let environment = child_environment(&worker, initialize).await;

    assert!(
        !environment.contains_key("UNRELATED_WORKER_VAR"),
        "a child receives the base environment, not the worker environment"
    );
    assert_eq!(
        environment.get("HOME").map(String::as_str),
        Some("/home/base")
    );
    assert_eq!(
        environment.get("TERM").map(String::as_str),
        Some("xterm-256color")
    );
    assert_eq!(
        environment.get("PROFILE_TOKEN").map(String::as_str),
        Some("profile-value")
    );
    assert_eq!(
        environment.get("POHUNEK_SESSION_ID"),
        Some(&fixture.session_id)
    );
    for name in environment.keys() {
        assert!(!is_denylisted(name), "{name} leaked to the child");
    }
    assert!(!environment.contains_key("NOTIFY_SOCKET"));
}

/// Input stall bound the silent-child scenarios initialize with. It is short
/// so the stall arrives quickly, and the write waits for it on a child that
/// never reads.
// timing-allowed: #1592 the worker's input stall policy is the behavior under test
const SILENT_CHILD_STALL_MS: u64 = 300;

/// Lines in the oversized write, about 134 KB in all. A PTY whose program does
/// not read holds only a few KiB, so a write this large always stays pending
/// until the child reads. On Linux the capacity is limited by the PTY's flip
/// buffer limit (8 KiB, set by `tty_buffer_set_limit` in `drivers/tty/pty.c`;
/// the 640 KiB `TTYB_DEFAULT_MEM_LIMIT` of `tty_buffer.c` does not apply to
/// PTYs) and the 4 KiB line-discipline queue (`N_TTY_BUF_SIZE`); a buffer
/// without a flag area can hold up to twice its accounted size. One write of
/// this blob was accepted up to 15,360 bytes canonical and 13,824 raw on
/// 7.2.9 with 4 KiB pages. macOS `ptcwrite` (xnu `bsd/kern/tty_dev.c`) blocks
/// at `TTYHOG - 2` = 1022 bytes (`bsd/sys/tty.h`).
const OVERSIZED_LINES: usize = 1_700;

/// Bytes per oversized write line.
const OVERSIZED_LINE_BYTES: usize = 78;

/// Bound the oversized write must exceed: 96 KiB, several times the measured
/// figures above and above the worst case for a kernel with 64 KiB pages
/// (`TTY_BUFFER_PAGE` about 32 KiB), which would hold at most about 84 KiB
/// even with the twofold slack. The 134 KB write stays larger.
const PTY_UNREAD_INPUT_CAPACITY: usize = 96 * 1024;

const _: () = assert!(
    OVERSIZED_LINES * (OVERSIZED_LINE_BYTES + 1) > PTY_UNREAD_INPUT_CAPACITY
        && OVERSIZED_LINES * (OVERSIZED_LINE_BYTES + 1) <= MAX_DATA_PAYLOAD_BYTES
        && OVERSIZED_LINES * (OVERSIZED_LINE_BYTES + 1) <= MAX_CONTROL_LINE_BYTES / 4,
    "the oversized write must exceed what a PTY holds and fit one data frame and one control line (a byte array, about four JSON bytes per input byte)"
);

/// Initializes the fixture's worker with a child that never reads its
/// terminal and the short input stall bound.
async fn initialize_silent_child(fixture: &Fixture, worker: &Worker) {
    let mut initialize = fixture.initialize(worker.worker_id().await);
    initialize.executable = PathBuf::from("/bin/sh");
    initialize.arguments = vec!["-c".to_owned(), "exec sleep 3600".to_owned()];
    initialize.input_policy =
        InputPolicy::new(SILENT_CHILD_STALL_MS).expect("silent child input policy");
    worker
        .initialize(initialize)
        .await
        .expect("initialize the silent child");
}

/// One fragment carrying more bytes than the child's PTY can queue unread.
fn oversized_text() -> Vec<u8> {
    vec!["x".repeat(OVERSIZED_LINE_BYTES); OVERSIZED_LINES]
        .join("\n")
        .into_bytes()
}

fn oversized_plan() -> Vec<InputFragment> {
    vec![InputFragment {
        bytes: SecretBytes::new(oversized_text()),
        delay_after_ms: 0,
    }]
}

#[tokio::test]
async fn current_version_controller_sees_a_stalled_write_as_input_stalled() {
    assert_stalled_write_is_input_stalled(CURRENT_VERSION).await;
}

#[tokio::test]
async fn previous_version_controller_sees_a_stalled_write_as_input_stalled() {
    assert_stalled_write_is_input_stalled(PREVIOUS_VERSION).await;
}

async fn assert_stalled_write_is_input_stalled(version: Version) {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(version).await;
    assert_eq!(worker.selected_version().await, version);
    initialize_silent_child(&fixture, &worker).await;

    let error = worker
        .write(oversized_plan())
        .await
        .expect_err("a write the silent child never reads must stall");
    assert!(
        matches!(
            error,
            WorkerError::Rejected {
                code: ControlCode::InputStalled,
                ..
            }
        ),
        "{error:?}"
    );
    worker
        .stop(TransactionId::new("stall-cleanup").expect("transaction id"))
        .await
        .expect("stop the silent child after the stall");
}

/// Initializes the fixture's worker with a working directory that does not
/// exist under the fixture root.
async fn initialize_into_missing_cwd(fixture: &Fixture, worker: &Worker) -> WorkerError {
    let mut initialize = fixture.initialize(worker.worker_id().await);
    initialize.cwd = fixture.root.join("missing-worktree");
    worker
        .initialize(initialize)
        .await
        .expect_err("a missing working directory must refuse the launch")
}

#[tokio::test]
async fn current_version_initialize_into_a_missing_cwd_reports_launch_cwd_missing() {
    assert_missing_cwd_is_launch_cwd_missing(CURRENT_VERSION).await;
}

#[tokio::test]
async fn previous_version_initialize_into_a_missing_cwd_reports_launch_cwd_missing() {
    assert_missing_cwd_is_launch_cwd_missing(PREVIOUS_VERSION).await;
}

async fn assert_missing_cwd_is_launch_cwd_missing(version: Version) {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(version).await;
    assert_eq!(worker.selected_version().await, version);

    let error = initialize_into_missing_cwd(&fixture, &worker).await;
    assert!(
        matches!(
            error,
            WorkerError::Rejected {
                code: ControlCode::LaunchCwdMissing,
                ..
            }
        ),
        "{error:?}"
    );
    // The fixture does not expose the spawned child, so the inspect snapshot
    // stands in: a refused launch leaves no child process and no running phase.
    let snapshot = worker.inspect().await.expect("inspect refused worker");
    assert_eq!(snapshot.child_process, None, "no child may be started");
    assert_ne!(snapshot.phase, RuntimePhase::Running);
}

/// Child script that leaves its terminal unread until the gate file named by
/// `$1` exists, then drains stdin for good.
const GATED_DRAIN_SCRIPT: &str = "while [ ! -e \"$1\" ]; do sleep 0.05; done; exec cat >/dev/null";

/// Attach stream that carries the stalled input.
const ATTACH_STREAM_ID: &str = "attach-stall";

/// Attach stream that observes the PTY echo while the input is pending.
const OBSERVER_STREAM_ID: &str = "attach-observer";

/// Reads an observer stream until its output contains `marker`. The PTY echoes
/// each accepted input byte, so an echoed line proves the attach write was
/// accepted and now holds the execution lock.
async fn await_pty_echo(observer: &mut DataStream, marker: &[u8]) -> Vec<u8> {
    let mut echoed = Vec::new();
    let outcome = tokio::time::timeout(DEADLINE, async {
        while !echoed.windows(marker.len()).any(|window| window == marker) {
            let frame = read_frame(&mut observer.stream)
                .await
                .expect("read observer frame")
                .expect("observer stream stays open");
            match frame.header().kind {
                FrameKind::Output { .. } | FrameKind::TerminalSnapshot { .. } => {
                    echoed.extend_from_slice(frame.payload());
                }
                FrameKind::Error { .. } | FrameKind::Close { .. } | FrameKind::Exit { .. } => {
                    panic!(
                        "observer stream ended before the echo: {:?}",
                        frame.header().kind
                    )
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(
        outcome.is_ok(),
        "the attach write is not echoed by the PTY: the observer saw {} bytes, tail {:?}",
        echoed.len(),
        String::from_utf8_lossy(&echoed[echoed.len().saturating_sub(120)..])
    );
    echoed
}

#[tokio::test]
async fn a_stalled_attach_write_keeps_its_stream_while_control_input_fails_with_input_stalled() {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(CURRENT_VERSION).await;
    assert_eq!(worker.selected_version().await, CURRENT_VERSION);
    let gate = fixture.root.join("gate");
    let mut initialize = fixture.initialize(worker.worker_id().await);
    initialize.executable = PathBuf::from("/bin/sh");
    initialize.arguments = vec![
        "-c".to_owned(),
        GATED_DRAIN_SCRIPT.to_owned(),
        "sh".to_owned(),
        gate.to_str().expect("UTF-8 fixture path").to_owned(),
    ];
    initialize.input_policy =
        InputPolicy::new(SILENT_CHILD_STALL_MS).expect("gated child input policy");
    worker
        .initialize(initialize)
        .await
        .expect("initialize the gated child");

    // The echo is observed on a second attach stream, as another client of
    // the session sees it. The observer attaches before any input exists, so
    // the echo is produced after its watermark and arrives as live `Output`
    // frames. A stream holds the dimension ordering guard until it is
    // dropped, and a later attach waits for that guard, so release it first.
    let mut observer = worker
        .open_attach(
            StreamId::new(OBSERVER_STREAM_ID).expect("observer stream id"),
            AttachStart { dimensions: None },
        )
        .await
        .expect("open observer attach stream");
    observer.dimension_update = None;

    let stream_id = StreamId::new(ATTACH_STREAM_ID).expect("stream id");
    let mut attach = worker
        .open_attach(stream_id.clone(), AttachStart { dimensions: None })
        .await
        .expect("open attach stream");

    let attach_write_id = WriteId::new(format!("attach-{ATTACH_STREAM_ID}-1")).expect("write id");
    let text = oversized_text();
    let attach_input = DataFrame::new(
        FrameHeader {
            version: attach.version,
            stream_id: stream_id.clone(),
            worker_instance_id: attach.worker_instance_id.clone(),
            kind: FrameKind::Input {
                write_id: attach_write_id.clone(),
            },
        },
        text.clone(),
    )
    .expect("attach input frame");
    write_frame(&mut attach.stream, &attach_input)
        .await
        .expect("send attach input");

    await_pty_echo(&mut observer, &[b'x'; OVERSIZED_LINE_BYTES]).await;

    let error = worker
        .write(oversized_plan())
        .await
        .expect_err("a control write must not run behind the stalled attach write");
    assert!(
        matches!(
            error,
            WorkerError::Rejected {
                code: ControlCode::InputStalled,
                ..
            }
        ),
        "{error:?}"
    );

    // Opening the gate lets the child drain its input, so the pending attach
    // write completes and its acknowledgement arrives on the same stream.
    std::fs::File::create(&gate).expect("create gate file");
    let (acked, bytes_written) = tokio::time::timeout(DEADLINE, async {
        loop {
            let frame = read_frame(&mut attach.stream)
                .await
                .expect("read attach frame")
                .expect("attach stream stays open until its write is acknowledged");
            match frame.header().kind.clone() {
                FrameKind::InputAck {
                    write_id,
                    bytes_written,
                } => return (write_id, bytes_written),
                FrameKind::Error { error } => {
                    panic!("the attach stream failed instead of acknowledging: {error:?}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the attach write is acknowledged after the child drains");
    assert_eq!(acked, attach_write_id);
    assert_eq!(
        bytes_written,
        u64::try_from(text.len()).expect("text length")
    );

    drop(attach);
    drop(observer);
    worker
        .stop(TransactionId::new("stall-cleanup").expect("transaction id"))
        .await
        .expect("stop the gated child");
}

/// Child script that prints [`CHILD_OUTPUT_MARKER`] once the gate file named
/// by `$1` exists, then leaves its terminal unread until the gate file named
/// by `$2` exists, and then drains stdin for good.
const GATED_OUTPUT_THEN_DRAIN_SCRIPT: &str = "while [ ! -e \"$1\" ]; do sleep 0.05; done; \
     printf 'child-output-marker\\n'; \
     while [ ! -e \"$2\" ]; do sleep 0.05; done; exec cat >/dev/null";

/// Line the gated child prints while the attach write is still pending.
const CHILD_OUTPUT_MARKER: &[u8] = b"child-output-marker";

/// Delay of a control plan's body fragment. It never elapses: the scenario
/// ends the worker while the plan is still inside it.
const PLAN_HOLD_MS: u64 = 3_600_000;

/// Attach stream that types while a control plan's delay runs.
const INTERLEAVING_STREAM_ID: &str = "attach-interleaving";

/// Attach stream whose own input write is pending while it reads output.
const WRITING_STREAM_ID: &str = "attach-writing";

/// Reads the attach stream until its output contains `marker`, failing if the
/// stream acknowledges its pending write first.
async fn await_output_before_ack(attach: &mut DataStream, marker: &[u8], what: &str) {
    let mut output = Vec::new();
    tokio::time::timeout(DEADLINE, async {
        while !output.windows(marker.len()).any(|window| window == marker) {
            let frame = read_frame(&mut attach.stream)
                .await
                .expect("read attach frame")
                .expect("attach stream stays open");
            match frame.header().kind {
                FrameKind::Output { .. } | FrameKind::TerminalSnapshot { .. } => {
                    output.extend_from_slice(frame.payload());
                }
                FrameKind::InputAck { .. } => {
                    panic!("the write was acknowledged before the child read it ({what})")
                }
                FrameKind::Error { .. } | FrameKind::Close { .. } | FrameKind::Exit { .. } => {
                    panic!(
                        "attach stream ended before {what}: {:?}",
                        frame.header().kind
                    )
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_elapsed| panic!("the writing attach stream receives {what}"));
}

#[tokio::test]
async fn an_attach_stream_receives_output_while_its_own_input_waits_for_the_child_to_read() {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(CURRENT_VERSION).await;
    let output_gate = fixture.root.join("output-gate");
    let drain_gate = fixture.root.join("drain-gate");
    let mut initialize = fixture.initialize(worker.worker_id().await);
    initialize.executable = PathBuf::from("/bin/sh");
    initialize.arguments = vec![
        "-c".to_owned(),
        GATED_OUTPUT_THEN_DRAIN_SCRIPT.to_owned(),
        "sh".to_owned(),
        output_gate.to_str().expect("UTF-8 fixture path").to_owned(),
        drain_gate.to_str().expect("UTF-8 fixture path").to_owned(),
    ];
    worker
        .initialize(initialize)
        .await
        .expect("initialize the gated child");

    let stream_id = StreamId::new(WRITING_STREAM_ID).expect("stream id");
    let mut attach = worker
        .open_attach(stream_id.clone(), AttachStart { dimensions: None })
        .await
        .expect("open attach stream");
    let write_id = WriteId::new(format!("attach-{WRITING_STREAM_ID}-1")).expect("write id");
    let text = oversized_text();
    let input = DataFrame::new(
        FrameHeader {
            version: attach.version,
            stream_id: stream_id.clone(),
            worker_instance_id: attach.worker_instance_id.clone(),
            kind: FrameKind::Input {
                write_id: write_id.clone(),
            },
        },
        text.clone(),
    )
    .expect("attach input frame");
    write_frame(&mut attach.stream, &input)
        .await
        .expect("send attach input");

    // The input exceeds the terminal's input queue and the child does not
    // read it, so the write stays pending. The stream still shows the echo of
    // what the terminal accepted, and output the child produces meanwhile.
    await_output_before_ack(&mut attach, &[b'x'; OVERSIZED_LINE_BYTES], "the input echo").await;
    std::fs::File::create(&output_gate).expect("create the output gate");
    await_output_before_ack(&mut attach, CHILD_OUTPUT_MARKER, "the child's output").await;

    std::fs::File::create(&drain_gate).expect("create the drain gate");
    let (acked, bytes_written) = tokio::time::timeout(DEADLINE, async {
        loop {
            let frame = read_frame(&mut attach.stream)
                .await
                .expect("read attach frame")
                .expect("attach stream stays open until its write is acknowledged");
            match frame.header().kind.clone() {
                FrameKind::InputAck {
                    write_id,
                    bytes_written,
                } => return (write_id, bytes_written),
                FrameKind::Error { error } => {
                    panic!("the attach stream failed instead of acknowledging: {error:?}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the attach write is acknowledged after the child drains");
    assert_eq!(acked, write_id);
    assert_eq!(
        bytes_written,
        u64::try_from(text.len()).expect("text length")
    );

    drop(attach);
    worker
        .stop(TransactionId::new("writing-stream-cleanup").expect("transaction id"))
        .await
        .expect("stop the gated child");
}

#[tokio::test]
async fn an_attach_write_lands_between_the_fragments_of_a_delayed_control_plan() {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(CURRENT_VERSION).await;
    let mut initialize = fixture.initialize(worker.worker_id().await);
    initialize.executable = PathBuf::from("/bin/cat");
    worker
        .initialize(initialize)
        .await
        .expect("initialize the echoing child");

    let mut observer = worker
        .open_attach(
            StreamId::new(OBSERVER_STREAM_ID).expect("observer stream id"),
            AttachStart { dimensions: None },
        )
        .await
        .expect("open observer attach stream");
    observer.dimension_update = None;
    let stream_id = StreamId::new(INTERLEAVING_STREAM_ID).expect("stream id");
    let mut attach = worker
        .open_attach(stream_id.clone(), AttachStart { dimensions: None })
        .await
        .expect("open attach stream");

    let plan = tokio::spawn({
        let worker = worker.clone();
        async move {
            worker
                .write(vec![
                    InputFragment {
                        bytes: SecretBytes::new(b"plan-body".to_vec()),
                        delay_after_ms: PLAN_HOLD_MS,
                    },
                    InputFragment {
                        bytes: SecretBytes::new(b"\r".to_vec()),
                        delay_after_ms: 0,
                    },
                ])
                .await
        }
    });
    let body_echo = await_pty_echo(&mut observer, b"plan-body").await;

    // The plan is inside its delay: its body is echoed and its Enter is not
    // yet written. An attach write must reach the PTY now instead of queueing
    // behind the plan.
    let attach_input = DataFrame::new(
        FrameHeader {
            version: attach.version,
            stream_id: stream_id.clone(),
            worker_instance_id: attach.worker_instance_id.clone(),
            kind: FrameKind::Input {
                write_id: WriteId::new(format!("attach-{INTERLEAVING_STREAM_ID}-1"))
                    .expect("write id"),
            },
        },
        b"attach-mark".to_vec(),
    )
    .expect("attach input frame");
    write_frame(&mut attach.stream, &attach_input)
        .await
        .expect("send attach input");
    // The PTY echoes bytes in the order it accepts them. The plan's Enter
    // waits behind a delay that never ends, so an attach write is echoed
    // directly after the body.
    let mut echoed = body_echo;
    echoed.extend(await_pty_echo(&mut observer, b"attach-mark").await);
    assert!(
        echoed
            .windows(b"plan-bodyattach-mark".len())
            .any(|window| window == b"plan-bodyattach-mark"),
        "the attach write did not land between the plan's fragments: {:?}",
        String::from_utf8_lossy(&echoed)
    );

    // The plan is still inside its delay and holds the control connection
    // until the worker ends with the fixture.
    plan.abort();
    drop(attach);
    drop(observer);
}

/// Line the attach client types while it reads none of the output.
const UNREAD_KEYSTROKE: &str = "keystroke-while-output-unread";

/// Line typed after the client resumed reading; its echo is live output.
const RESYNCED_KEYSTROKE: &str = "keystroke-after-resync";

/// Bytes the child prints once told to start: four times the subscriber queue
/// the fixture grants (1 MiB) and many times the socket and PTY buffers
/// between the child and the client, so the output path is blocked when the
/// client types.
const UNREAD_FLOOD_BYTES: usize = 4 * 1024 * 1024;

/// Releases the flooding child, once the attach stream is open, so the stream
/// is subscribed before the child prints its first byte. A non-blocking open
/// of a FIFO fails with `ENXIO` until the child opens it for reading, so the wait for the
/// child is bounded by the hang guard.
async fn start_flooding(start: &Path) {
    let mut fifo = wait_until("the child to wait for the start line", || async {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(start)
        {
            Ok(fifo) => Some(fifo),
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => None,
            Err(error) => panic!("open the start FIFO: {error}"),
        }
    })
    .await;
    fifo.write_all(b"go\n").expect("release the flooding child");
}

/// Starts a child that waits for the returned `start` FIFO to be written,
/// floods its output, signals through the returned `flooded` file and copies
/// its input into the returned `typed` file.
async fn initialize_flooding_child(
    fixture: &Fixture,
    worker: &Worker,
) -> (PathBuf, PathBuf, PathBuf) {
    let sink = fixture.root.join("typed");
    let flooded = fixture.root.join("flooded");
    let start = fixture.root.join("start");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&start)
            .status()
            .expect("run mkfifo")
            .success(),
        "create the start FIFO"
    );
    let mut initialize = fixture.initialize(worker.worker_id().await);
    initialize.executable = PathBuf::from("/bin/sh");
    initialize.arguments = vec![
        "-c".to_owned(),
        format!(
            "read _ < \"$3\"; {{ head -c {UNREAD_FLOOD_BYTES} /dev/zero | tr '\\0' y; : > \"$2\"; }} & exec cat > \"$1\""
        ),
        "sh".to_owned(),
        sink.to_str().expect("UTF-8 fixture path").to_owned(),
        flooded.to_str().expect("UTF-8 fixture path").to_owned(),
        start.to_str().expect("UTF-8 fixture path").to_owned(),
    ];
    worker
        .initialize(initialize)
        .await
        .expect("initialize the flooding child");
    (sink, flooded, start)
}

fn attach_input_frame(attach: &DataStream, sequence: u32, text: &str) -> DataFrame {
    DataFrame::new(
        FrameHeader {
            version: attach.version,
            stream_id: StreamId::new(ATTACH_STREAM_ID).expect("stream id"),
            worker_instance_id: attach.worker_instance_id.clone(),
            kind: FrameKind::Input {
                write_id: WriteId::new(format!("attach-{ATTACH_STREAM_ID}-{sequence}"))
                    .expect("write id"),
            },
        },
        format!("{text}\n").into_bytes(),
    )
    .expect("attach input frame")
}

/// Frames kept in [`ResyncProgress`] for the timeout report.
const RESYNC_TRACE_FRAMES: usize = 12;

/// What the client that resumed reading has seen of the gap, the repaint and
/// the live output, reported when the wait for the echo times out.
#[derive(Default)]
struct ResyncProgress {
    frames: u64,
    gaps: u32,
    repainted: bool,
    resync_keystroke_sent: bool,
    snapshot_bytes: usize,
    output_bytes: usize,
    live: Vec<u8>,
    last_frames: std::collections::VecDeque<String>,
}

impl ResyncProgress {
    fn record(&mut self, frame: &DataFrame) {
        self.frames += 1;
        match frame.header().kind {
            FrameKind::TerminalSnapshot { .. } => self.snapshot_bytes += frame.payload().len(),
            FrameKind::Output { .. } => self.output_bytes += frame.payload().len(),
            _ => {}
        }
        if self.last_frames.len() == RESYNC_TRACE_FRAMES {
            self.last_frames.pop_front();
        }
        self.last_frames.push_back(format!(
            "{:?} ({} payload bytes)",
            frame.header().kind,
            frame.payload().len()
        ));
    }
}

impl std::fmt::Display for ResyncProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "frames read {}, gaps {}, repainted {}, resync keystroke sent {}, snapshot bytes {}, \
             output bytes {}, live bytes after the repaint {}, last frames {:?}",
            self.frames,
            self.gaps,
            self.repainted,
            self.resync_keystroke_sent,
            self.snapshot_bytes,
            self.output_bytes,
            self.live.len(),
            self.last_frames
        )
    }
}

#[tokio::test]
async fn keystrokes_reach_the_child_while_the_attach_stream_reads_no_output_and_resyncs_after() {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(CURRENT_VERSION).await;
    let (sink, flooded, start) = initialize_flooding_child(&fixture, &worker).await;

    let mut attach = worker
        .open_attach(
            StreamId::new(ATTACH_STREAM_ID).expect("stream id"),
            AttachStart { dimensions: None },
        )
        .await
        .expect("open attach stream");
    start_flooding(&start).await;

    // The client reads nothing, so every buffer between the child and it fills
    // while the child prints.
    wait_until("the child to finish its output", || async {
        flooded.exists().then_some(())
    })
    .await;
    let keystroke = attach_input_frame(&attach, 1, UNREAD_KEYSTROKE);
    write_frame(&mut attach.stream, &keystroke)
        .await
        .expect("send attach input");
    wait_until("the child to receive the typed line", || async {
        std::fs::read_to_string(&sink)
            .ok()
            .filter(|typed| typed.contains(UNREAD_KEYSTROKE))
    })
    .await;

    // Reading resumes: the lost output is reported as a gap that a repaint
    // follows, and live output continues after it. The repaint is over once a
    // live `Output` frame carrying the echo of a fresh keystroke arrives.
    let mut progress = ResyncProgress::default();
    let resynced = tokio::time::timeout(DEADLINE, async {
        loop {
            let frame = read_frame(&mut attach.stream)
                .await
                .expect("read attach frame")
                .expect("attach stream stays open");
            progress.record(&frame);
            match frame.header().kind {
                FrameKind::Gap { .. } => progress.gaps += 1,
                FrameKind::TerminalSnapshot { .. } if progress.gaps > 0 && !progress.repainted => {
                    progress.repainted = true;
                    let keystroke = attach_input_frame(&attach, 2, RESYNCED_KEYSTROKE);
                    write_frame(&mut attach.stream, &keystroke)
                        .await
                        .expect("send the keystroke typed after the resync");
                    progress.resync_keystroke_sent = true;
                }
                FrameKind::Output { .. } if progress.repainted => {
                    progress.live.extend_from_slice(frame.payload());
                    if progress
                        .live
                        .windows(RESYNCED_KEYSTROKE.len())
                        .any(|window| window == RESYNCED_KEYSTROKE.as_bytes())
                    {
                        return;
                    }
                }
                FrameKind::Error { .. } | FrameKind::Close { .. } | FrameKind::Exit { .. } => {
                    panic!(
                        "attach stream ended before live output resumed: {:?}; {progress}",
                        frame.header().kind
                    )
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(
        resynced.is_ok(),
        "a gap, its repaint and live output did not reach the client that resumed reading \
         within the deadline: {progress}"
    );

    drop(attach);
    worker
        .stop(TransactionId::new("unread-output-cleanup").expect("transaction id"))
        .await
        .expect("stop the flooding child");
}

#[tokio::test]
async fn a_lease_release_closes_an_attach_stream_whose_client_reads_no_output() {
    let mut fixture = Fixture::start();
    let worker = fixture.connect(CURRENT_VERSION).await;
    let (_sink, flooded, start) = initialize_flooding_child(&fixture, &worker).await;

    let mut attach = worker
        .open_attach(
            StreamId::new(ATTACH_STREAM_ID).expect("stream id"),
            AttachStart { dimensions: None },
        )
        .await
        .expect("open attach stream");
    start_flooding(&start).await;
    wait_until("the child to finish its output", || async {
        flooded.exists().then_some(())
    })
    .await;
    worker
        .release_controller()
        .await
        .expect("release the controller lease");

    // The close frame is queued behind the output the client did not read, so
    // it arrives once reading resumes.
    tokio::time::timeout(DEADLINE, async {
        loop {
            let frame = read_frame(&mut attach.stream)
                .await
                .expect("read attach frame")
                .expect("stream stays open until its close frame");
            if let FrameKind::Close { reason } = frame.header().kind.clone() {
                assert_eq!(reason, CloseReason::LeaseReleased);
                break;
            }
        }
        assert!(
            read_frame(&mut attach.stream)
                .await
                .expect("read after the close frame")
                .is_none(),
            "the stream ends after its close frame"
        );
    })
    .await
    .expect("a close frame follows the queued output and ends the stream");
}

#[tokio::test]
async fn worker_socket_rejects_a_protocol_range_above_its_window() {
    let above = Version::new(CURRENT_VERSION.get() + 1).expect("next version is nonzero");
    assert_disjoint_range_rejected(above).await;
}

/// A daemon that only speaks protocol 6, the version that left the window, is
/// refused: the window is the previous and the current version.
#[tokio::test]
async fn worker_socket_rejects_a_daemon_that_only_speaks_protocol_six() {
    let dropped = Version::new(6).expect("version six is nonzero");
    assert!(dropped < PREVIOUS_VERSION);
    assert_disjoint_range_rejected(dropped).await;
}

async fn assert_disjoint_range_rejected(next_version: Version) {
    let mut fixture = Fixture::start();
    fixture.wait_ready();
    let socket = fixture.socket_path();
    let session_id = fixture.session_id.clone();
    let child = std::cell::RefCell::new(&mut fixture.worker);
    let error = wait_until("the worker to reject a disjoint protocol range", || async {
        match Worker::connect_with_range(
            &socket,
            &session_id,
            DAEMON_ID,
            next_version,
            next_version,
        )
        .await
        {
            Err(WorkerError::Socket { .. }) => {
                assert!(
                    child
                        .borrow_mut()
                        .try_wait()
                        .expect("inspect worker")
                        .is_none(),
                    "worker exited before accepting a controller"
                );
                None
            }
            Err(error) => Some(error),
            Ok(_) => panic!("a worker accepted a version outside its supported range"),
        }
    })
    .await;
    assert!(
        matches!(
            error,
            WorkerError::Rejected {
                code: ControlCode::WorkerProtocolIncompatible,
                ..
            }
        ),
        "{error:?}"
    );
}
