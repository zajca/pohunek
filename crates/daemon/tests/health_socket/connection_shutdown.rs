//! Control connections when the daemon stops.
//!
//! The scenarios serve the control socket (and, for the last one, a remote
//! listener) the way `pohunekd` does: the connections are tracked by a
//! [`ConnectionTasks`] that stops with the signal and is awaited by a
//! [`DaemonShutdown`], and the signal is raised as `main` raises it, by
//! `begin_daemon_shutdown` on the registry and a cancelled token.

// Rust guideline compliant 2026-10-31

use std::fs;
use std::sync::Mutex;

use nix::unistd::mkfifo;
use pohunek_daemon::api::{ConnectionTasks, OverlaySource, RemoteServer};
use pohunek_daemon::shutdown::{DaemonShutdown, DaemonShutdownReport, ShutdownPhase, ShutdownPlan};
use pohunek_test_support::wait::guard;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::fmt::MakeWriter;

use super::*;

/// A shell that stays alive for the whole scenario.
const IDLE_SHELL: &str = "exec sleep 30";

/// The metadata key the pipelined request tries to set.
const PIPELINED_KEY: &str = "pipelined.key";

/// The log line of a `session.wait` that has taken its waiter slot.
const WAIT_PARKED: &str = "session wait parked";

/// The bound of the drivers phase in the scenarios that outlive it, on the
/// paused clock.
const DRIVER_SHUTDOWN: Duration = Duration::from_secs(1);

/// Runs the shutdown while the paused clock advances past its bounds: the
/// request blocked on the FIFO keeps the runtime from advancing it on its own.
async fn run_past_the_bounds(
    tasks: &DaemonShutdown,
    registry: &SessionRegistry,
) -> DaemonShutdownReport {
    let run = tasks.run(registry);
    tokio::pin!(run);
    loop {
        tokio::select! {
            biased;
            report = &mut run => return report,
            () = tokio::time::advance(DRIVER_SHUTDOWN) => {}
        }
    }
}

/// The tracked server of one transport and the way to stop it.
struct Stopping {
    registry: SessionRegistry,
    signal: CancellationToken,
    tasks: DaemonShutdown,
    server: tokio::task::JoinHandle<()>,
    // Removed after the server and its registry are gone.
    _worker_home: tempfile::TempDir,
}

/// A registry config with a metadata store, which `project.add` needs.
fn stored_config(socket: &TestSocket) -> SessionRegistryConfig {
    SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        store_path: Some(socket.dir.join("metadata.jsonl")),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    }
}

impl Stopping {
    async fn start(socket: &TestSocket, plan: ShutdownPlan) -> Self {
        let (registry, worker_home) = worker_backed_registry(socket, stored_config(socket));
        let state = DaemonState::new(
            HealthInfo::new("0.0.0"),
            registry.clone(),
            governance_service(socket).await,
            support::overlay_registry(),
            OverlaySource::Default,
        );
        let server = ControlServer::bind_with_state(socket, state)
            .await
            .expect("server binds");
        let signal = CancellationToken::new();
        let tasks = DaemonShutdown::new(plan);
        let connections = ConnectionTasks::new("control_connections", &signal);
        connections.register_shutdown_step(&tasks);
        let accepting = signal.clone();
        let server = tokio::spawn(server.serve(connections, async move {
            accepting.cancelled().await;
        }));
        Self {
            registry,
            signal,
            tasks,
            server,
            _worker_home: worker_home,
        }
    }

    /// Raises the stop signal as `pohunekd` does.
    fn raise_signal(&self) {
        self.registry.begin_daemon_shutdown();
        self.signal.cancel();
    }
}

async fn send(control: &mut Framed<UnixStream, LinesCodec>, request: &ProtocolRequest) {
    control
        .send(serde_json::to_string(request).expect("serialize request"))
        .await
        .expect("send request");
}

/// The next reply line of `control`, or `None` at the end of the connection.
async fn next_reply(control: &mut Framed<UnixStream, LinesCodec>) -> Option<Response> {
    let line = guard("the next line of the connection", control.next()).await?;
    Some(serde_json::from_str(&line.expect("reply framing ok")).expect("parse the reply"))
}

fn assert_shutting_down(reply: Option<Response>, what: &str) {
    let error = reply
        .unwrap_or_else(|| panic!("{what}: the connection ended without a reply"))
        .into_result()
        .expect_err("the request is refused");
    assert_eq!(error.code, "daemon_shutting_down", "{what}: {error:?}");
}

/// A `session.wait` in flight and a mutating request pipelined behind it are
/// both answered with `daemon_shutting_down` at the signal; the second is never
/// dispatched, so its edit is not made, and the connection then ends.
#[tokio::test]
async fn a_request_pipelined_behind_a_wait_is_refused_when_the_daemon_stops() {
    let captured = Captured::default();
    let _logs = capture_logs(&captured);
    let socket = temp_socket("conn-pipelined");
    let stopping = Stopping::start(&socket, ShutdownPlan::default()).await;
    let mut control = connect(&socket).await;
    let session = create_session(&mut control, &socket).await;
    send(
        &mut control,
        &Request::make(
            "wait",
            method::SESSION_WAIT,
            serde_json::json!({
                "session_id": session.id,
                "states": ["failed"],
                "timeout_ms": 8_000,
            }),
        ),
    )
    .await;
    send(
        &mut control,
        &Request::make(
            "metadata",
            method::SESSION_SET_METADATA,
            serde_json::json!({
                "session_id": session.id,
                "metadata": { PIPELINED_KEY: "1" },
            }),
        ),
    )
    .await;
    // The wait is parked in the daemon, and the second line is queued behind it.
    wait_until("the wait to park in the daemon", || async {
        captured.text().contains(WAIT_PARKED).then_some(())
    })
    .await;

    stopping.raise_signal();

    assert_shutting_down(next_reply(&mut control).await, "the wait");
    assert_shutting_down(next_reply(&mut control).await, "the pipelined request");
    assert!(
        next_reply(&mut control).await.is_none(),
        "the connection ends after the replies"
    );
    let listed = stopping.registry.list().await;
    let metadata = &listed
        .iter()
        .find(|info| info.id == session.id)
        .expect("the session is listed")
        .metadata;
    assert!(
        !metadata.contains_key(PIPELINED_KEY),
        "the pipelined request was dispatched: {metadata:?}"
    );
    let report = stopping.tasks.run(&stopping.registry).await;
    assert!(report.is_quiescent(), "{report:?}");
    let _ = stopping.server.await;
}

/// An idle connection and an event subscription are at EOF at the signal, and
/// the daemon's shutdown then finds no connection running.
#[tokio::test]
async fn idle_connections_and_subscriptions_end_with_the_daemon() {
    let socket = temp_socket("conn-idle");
    let stopping = Stopping::start(&socket, ShutdownPlan::default()).await;
    let mut idle = connect(&socket).await;
    let mut subscriber = connect(&socket).await;
    let ack = exchange(
        &mut subscriber,
        &Request::make("subscribe", method::SUBSCRIBE, serde_json::json!({})),
    )
    .await;
    assert!(ack.is_ok(), "{ack:?}");
    // Both connections are served once the daemon answers on a third.
    let mut probe = connect(&socket).await;
    let health = exchange(
        &mut probe,
        &Request::make("health", method::DAEMON_HEALTH, serde_json::json!({})),
    )
    .await;
    assert!(health.is_ok(), "{health:?}");

    stopping.raise_signal();

    assert!(
        next_reply(&mut idle).await.is_none(),
        "the idle connection is at EOF at the signal"
    );
    assert!(
        next_reply(&mut subscriber).await.is_none(),
        "the subscription is at EOF at the signal"
    );
    let report = stopping.tasks.run(&stopping.registry).await;
    assert!(report.is_quiescent(), "{report:?}");
    let _ = stopping.server.await;
}

/// The lines a log subscriber captured.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log capture lock")).into_owned()
    }
}

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::Write::write(&mut *self.0.lock().expect("log capture lock"), buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Routes the debug logs of this test's thread into `captured`.
fn capture_logs(captured: &Captured) -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    )
}

/// A repository whose configuration includes a FIFO: `git` blocks opening it
/// until the test holds the write end, so `project.add` on the repository stays
/// in flight. Returns the repository and the FIFO.
fn wedged_repository(root: &Path) -> (PathBuf, PathBuf) {
    let repo = root.join("wedged-repo");
    fs::create_dir(&repo).expect("create the repository directory");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    let fifo = root.join("config.fifo");
    mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).expect("create the FIFO");
    git(&["config", "include.path", fifo.to_str().expect("utf-8 path")]);
    (repo, fifo)
}

/// Opens the write end of `fifo`, which completes once `git` has opened the read
/// end and so is blocked on it. The returned file keeps `git` blocked.
async fn hold_fifo_open(fifo: PathBuf) -> fs::File {
    tokio::task::spawn_blocking(move || {
        fs::OpenOptions::new()
            .write(true)
            .open(fifo)
            .expect("open the FIFO's write end")
    })
    .await
    .expect("the FIFO opener finishes")
}

fn add_project_request(repo: &Path) -> ProtocolRequest {
    Request::make(
        "add",
        method::PROJECT_ADD,
        serde_json::json!({ "path": repo }),
    )
}

/// A request that outlives the drivers bound is named in the report under its
/// transport and in the log under its method, and is left running.
#[tokio::test(start_paused = true)]
async fn a_request_that_outlives_the_bound_is_named_by_its_transport() {
    let captured = Captured::default();
    let _logs = capture_logs(&captured);
    let socket = temp_socket("conn-straggler");
    let plan = ShutdownPlan {
        driver_shutdown: DRIVER_SHUTDOWN,
        ..ShutdownPlan::default()
    };
    let stopping = Stopping::start(&socket, plan).await;
    let (repo, fifo) = wedged_repository(&socket.dir);
    let mut control = connect(&socket).await;
    send(&mut control, &add_project_request(&repo)).await;
    let _wedge = hold_fifo_open(fifo).await;

    stopping.raise_signal();
    let report = run_past_the_bounds(&stopping.tasks, &stopping.registry).await;

    assert_eq!(
        report.still_running(),
        [(ShutdownPhase::Drivers, "control_connections")],
        "{report:?}"
    );
    let logs = captured.text();
    assert!(
        logs.contains("connections were still open when the shutdown bound passed")
            && logs.contains("request project.add"),
        "the straggler is named in the log: {logs}"
    );
    let _ = stopping.server.await;
}

/// The same over a remote listener, whose connections are a tracker of their
/// own.
#[tokio::test(start_paused = true)]
async fn a_remote_request_that_outlives_the_bound_is_named_by_its_transport() {
    let socket = temp_socket("conn-remote-straggler");
    let (registry, _worker_home) = worker_backed_registry(&socket, stored_config(&socket));
    let state = DaemonState::new(
        HealthInfo::new("0.0.0"),
        registry.clone(),
        governance_service(&socket).await,
        support::overlay_registry(),
        OverlaySource::Default,
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the remote listener");
    let address = listener.local_addr().expect("listener address");
    let remote = RemoteServer::from_listener(listener, state);
    let signal = CancellationToken::new();
    let tasks = DaemonShutdown::new(ShutdownPlan {
        driver_shutdown: DRIVER_SHUTDOWN,
        ..ShutdownPlan::default()
    });
    let connections = ConnectionTasks::new("remote_connections", &signal);
    connections.register_shutdown_step(&tasks);
    let accepting = signal.clone();
    let server = tokio::spawn(remote.serve(connections, async move {
        accepting.cancelled().await;
    }));
    let (repo, fifo) = wedged_repository(&socket.dir);
    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to the remote listener");
    let mut client = Framed::new(stream, LinesCodec::new());
    client
        .send(serde_json::to_string(&add_project_request(&repo)).expect("serialize request"))
        .await
        .expect("send project.add");
    let _wedge = hold_fifo_open(fifo).await;

    registry.begin_daemon_shutdown();
    signal.cancel();
    let report = run_past_the_bounds(&tasks, &registry).await;

    assert_eq!(
        report.still_running(),
        [(ShutdownPhase::Drivers, "remote_connections")],
        "{report:?}"
    );
    let _ = server.await;
}
