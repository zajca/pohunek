//! The placement escape check against real processes on a real PTY.
//!
//! Each scenario runs a shell script as the PTY child and arms the check the
//! way `Initialize` does. Expected outcomes follow from the shape of the
//! processes the script starts (their session, name and marker), never from
//! the check's own output; the scripts publish files so a scenario inspects
//! only once every process has reached its steady state.
//!
//! A process that must run under the provider's name is the test binary
//! itself, installed under that name and re-run as the holding image
//! (`pohunek_test_support::hold`): one process that ends by itself, with no
//! copy of a system binary.

use std::path::Path;

use pohunek_platform::process::{HostInspector, ProcessInspector as _};
use pohunek_test_support::hold;
use pohunek_test_support::wait::{poll_until, wait_until};
use pohunek_worker_protocol::{
    PlacementOutcome, PlacementReport, ResponseKind, RuntimeScope, SessionId, TransactionId,
    WorkerId, CURRENT_VERSION,
};

use crate::placement::Armed;
use crate::server::Server;
use crate::server::{
    inspect_snapshot, run_placement_check, stop_request, wait_runtime_exit, WireRuntimePhase,
    MAX_PLACEMENT_CHECK_INTERVAL,
};

/// File name of the daemon image in the fixtures of
/// [`super::launch_claim_fixture_env`], whose provider is `claude`.
const CLAIM_DAEMON: &str = "claude-daemon";

/// File name of the daemon image in a runtime started through `Initialize`,
/// whose provider is the `codex` of [`super::environment_initialize`].
const INITIALIZED_DAEMON: &str = "codex-daemon";

/// Substring of the one WARN a generation logs for its first escape.
const ESCAPE_WARNING: &str = "placement_escape:";

/// The generation marker the fixture's processes carry in `scenario`.
///
/// Scenarios share one process table, and under the one-process
/// `cargo test` they also share the test process id, so the marker names the
/// scenario: a daemon one scenario starts never carries another's marker.
fn marker(scenario: &str) -> String {
    format!("runtime-placement-{}-{scenario}", std::process::id())
}

/// The PTY child's environment: the pinned fixture environment plus the
/// generation marker a worker injects into every child.
fn marked_env(scenario: &str) -> Vec<(String, String)> {
    let mut env = crate::test_support::child_env();
    env.push(("POHUNEK_WORKER_INSTANCE_ID".to_owned(), marker(scenario)));
    env
}

/// Arms the check for `pty` as `Initialize` does.
async fn arm(server: &Server, pty: &crate::PtyOwner, scenario: &str) {
    let pty_session = HostInspector::new()
        .process_session(pty.identity().pid)
        .expect("read the PTY child's session");
    assert!(pty_session.is_some(), "the PTY child runs");
    server.shared.state.lock().await.placement.armed = Some(Armed {
        pty_session,
        worker_instance_id: marker(scenario),
        worker_pid: std::process::id(),
    });
}

/// The result `inspect` shows for the latest check, without running one.
async fn latest(server: &Server) -> Option<PlacementReport> {
    let state = server.shared.state.lock().await;
    inspect_snapshot(&server.shared, &state, CURRENT_VERSION)
        .await
        .expect("inspect the worker")
        .placement
}

async fn report(server: &Server) -> PlacementReport {
    run_placement_check(&server.shared).await;
    latest(server)
        .await
        .expect("an armed runtime reports its check")
}

fn pid_in(directory: &Path, file: &str) -> u32 {
    poll_until(&format!("the fixture to publish {file}"), || {
        std::fs::read_to_string(directory.join(file))
            .ok()
            .and_then(|text| text.trim().parse().ok())
    })
}

/// The pid `file` holds once that process runs the image installed as `image`.
///
/// The fixture publishes the pid from the shell that then `exec`s the image,
/// so the pid alone does not say the process is already named for the provider.
fn pid_of_image(directory: &Path, file: &str, image: &str) -> u32 {
    let pid = pid_in(directory, file);
    poll_until(&format!("pid {pid} to run {image}"), || {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .filter(|exe| exe.file_name().is_some_and(|name| name == image))
    });
    pid
}

fn end(pid: u32) {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(pid).expect("a process id"));
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
}

/// Installs the holding image as `name` below `directory` and returns the
/// shell words that run it, redirections included.
fn install_image(directory: &Path, name: &str) -> String {
    let path = directory.join(name);
    hold::install_at(&path).expect("install the holding image");
    hold::shell_words_for(&path)
}

/// Starts a daemon image named for the provider in a session of its own, the
/// way an agent prewarming a shared host daemon does, and publishes its pid.
fn prewarm_daemon(image: &str) -> String {
    format!(
        "setsid sh -c \"echo \\$\\$ > daemon.pid; exec {image}\" &\n\
         until [ -e daemon.pid ]; do sleep 0.02; done\n"
    )
}

#[tokio::test]
async fn a_daemon_that_outlives_the_root_is_reported_at_generation_end() {
    let images = pohunek_test_support::tempdir().expect("image directory");
    let script = format!(
        "{}exit 0\n",
        prewarm_daemon(&install_image(images.path(), CLAIM_DAEMON))
    );
    let scenario = "generation-end";
    let (server, pty, directory) =
        super::launch_claim_fixture_env(&script, crate::WorkerConfig::new(), marked_env(scenario))
            .await;
    arm(&server, &pty, scenario).await;
    wait_runtime_exit(&pty).await.expect("the root exits");
    let daemon = pid_of_image(directory.path(), "daemon.pid", CLAIM_DAEMON);

    let found = report(&server).await;
    end(daemon);

    assert_eq!(found.outcome, PlacementOutcome::Escaped);
    assert_eq!(found.processes.len(), 1);
    assert_eq!(found.processes[0].pid, daemon);
    assert_eq!(found.processes[0].comm, CLAIM_DAEMON);
}

#[tokio::test]
async fn processes_that_stay_or_are_not_the_generations_are_not_reported() {
    // A tool wrapper in a session of its own that is neither named for the
    // provider nor listening; a provider-named image in another session
    // without this generation's marker; a plain background child.
    let images = pohunek_test_support::tempdir().expect("image directory");
    let image = install_image(images.path(), CLAIM_DAEMON);
    let script = format!(
        "env -u POHUNEK_WORKER_INSTANCE_ID setsid sh -c \"echo \\$\\$ > foreign.pid; exec {image}\" &\n\
         setsid sh -c 'echo $$ > wrapper.pid; exec sleep 600' &\n\
         sleep 600 &\n\
         until [ -e foreign.pid ] && [ -e wrapper.pid ]; do sleep 0.02; done\n\
         echo ready > ready\n\
         wait\n"
    );
    let scenario = "stay-or-foreign";
    let (server, pty, directory) =
        super::launch_claim_fixture_env(&script, crate::WorkerConfig::new(), marked_env(scenario))
            .await;
    arm(&server, &pty, scenario).await;
    let foreign = pid_of_image(directory.path(), "foreign.pid", CLAIM_DAEMON);
    let wrapper = pid_in(directory.path(), "wrapper.pid");
    poll_until("the fixture to settle", || {
        directory.path().join("ready").exists().then_some(())
    });

    let found = report(&server).await;
    end(foreign);
    end(wrapper);

    assert_eq!(
        found.outcome,
        PlacementOutcome::Clean,
        "foreign={foreign} wrapper={wrapper} {found:?}"
    );
    assert!(found.processes.is_empty());
    pty.stop(
        "test-cleanup",
        server.shared.config.stop_grace,
        server.shared.config.kill_reap_deadline,
    )
    .await
    .expect("stop the PTY");
}

#[tokio::test]
async fn a_check_that_cannot_read_the_ptys_session_is_unobservable_not_clean() {
    let scenario = "unreadable-session";
    let (server, pty, _directory) = super::launch_claim_fixture_env(
        "read -r line",
        crate::WorkerConfig::new(),
        marked_env(scenario),
    )
    .await;
    server.shared.state.lock().await.placement.armed = Some(Armed {
        pty_session: None,
        worker_instance_id: marker(scenario),
        worker_pid: std::process::id(),
    });

    let found = report(&server).await;

    assert_eq!(found.outcome, PlacementOutcome::Unobservable);
    pty.stop(
        "test-cleanup",
        server.shared.config.stop_grace,
        server.shared.config.kill_reap_deadline,
    )
    .await
    .expect("stop the PTY");
}

#[tokio::test]
async fn an_unarmed_runtime_reports_no_placement() {
    let (server, pty, _directory) = super::launch_claim_fixture_env(
        "read -r line",
        crate::WorkerConfig::new(),
        marked_env("unarmed"),
    )
    .await;

    run_placement_check(&server.shared).await;
    let snapshot = latest(&server).await;

    assert_eq!(snapshot, None);
    pty.stop(
        "test-cleanup",
        server.shared.config.stop_grace,
        server.shared.config.kill_reap_deadline,
    )
    .await
    .expect("stop the PTY");
}

/// A runtime started through `Initialize` with its placement check armed.
///
/// The check interval is the longest the worker accepts, so the periodic task
/// only runs its first, immediate check; every later check is one a scenario
/// asked for or one the worker runs itself at the end of the generation.
struct StartedRuntime {
    server: Server,
    connection: crate::server::Connection,
    scope: RuntimeScope,
    directory: tempfile::TempDir,
    /// Keeps the installed images until the scenario ends.
    _images: tempfile::TempDir,
}

/// Starts a runtime whose root first waits for the scenario's `go` file, then
/// prewarms the provider-named daemon in a session of its own and runs
/// `after`.
///
/// Returns once the immediate check ran and found nothing, so a later
/// `Escaped` result can only come from a check made after the daemon exists.
async fn initialized_runtime(after: &str) -> StartedRuntime {
    let images = pohunek_test_support::tempdir().expect("image directory");
    let directory = pohunek_test_support::tempdir().expect("fixture directory");
    let script = format!(
        "until [ -e go ]; do sleep 0.02; done\n{}{after}\n",
        prewarm_daemon(&install_image(images.path(), INITIALIZED_DAEMON))
    );
    let server = super::environment_fixture(directory.path(), "abcd2345")
        .await
        .expect("bind worker");
    let mut initialize = super::environment_initialize(
        super::base_environment(&[("PATH", crate::test_support::CHILD_PATH)]),
        &[],
    );
    initialize.executable = std::path::PathBuf::from("/bin/sh");
    initialize.arguments = vec!["-c".to_owned(), script];
    initialize.cwd = directory.path().to_path_buf();
    initialize.placement_check_interval_ms = Some(
        u64::try_from(MAX_PLACEMENT_CHECK_INTERVAL.as_millis()).expect("an interval in millis"),
    );
    let (connection, lease_id) = super::controller_connection(&server);
    let initialized =
        crate::server::initialize_request(&server.shared, &connection, &lease_id, initialize)
            .await
            .expect("initialize the runtime");
    let ResponseKind::Initialized {
        worker_instance_id, ..
    } = initialized
    else {
        panic!("initialize answers Initialized: {initialized:?}");
    };
    let scope = RuntimeScope {
        lease_id,
        session_id: SessionId::new("s-114").expect("session id"),
        worker_id: WorkerId::new("worker-environment").expect("worker id"),
        worker_instance_id,
    };
    let runtime = StartedRuntime {
        server,
        connection,
        scope,
        directory,
        _images: images,
    };
    let first = wait_until("the immediate check to run", || async {
        latest(&runtime.server).await
    })
    .await;
    assert_eq!(first.outcome, PlacementOutcome::Clean, "{first:?}");
    runtime
}

impl StartedRuntime {
    /// Lets the root start its daemon and returns the daemon's pid.
    fn release_daemon(&self) -> u32 {
        std::fs::write(self.directory.path().join("go"), b"").expect("release the root");
        pid_of_image(self.directory.path(), "daemon.pid", INITIALIZED_DAEMON)
    }

    async fn stop(&self) -> ResponseKind {
        stop_request(
            &self.server.shared,
            &self.connection,
            &self.scope,
            TransactionId::new("stop-placement").expect("transaction id"),
        )
        .await
        .expect("stop the runtime")
    }
}

/// Subscribes this thread to the worker's warnings; the events a scenario's
/// tasks emit on its current-thread runtime land in the returned capture.
fn capture_warnings() -> (super::EventCapture, tracing::subscriber::DefaultGuard) {
    let capture = super::EventCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer({
            let capture = capture.clone();
            move || capture.clone()
        })
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    (capture, tracing::subscriber::set_default(subscriber))
}

fn escape_warnings(capture: &super::EventCapture) -> usize {
    String::from_utf8_lossy(&capture.0.lock().expect("event capture"))
        .matches(ESCAPE_WARNING)
        .count()
}

#[tokio::test]
async fn an_escape_is_warned_once_per_generation_however_often_it_is_checked() {
    let (capture, _subscriber) = capture_warnings();
    let runtime = initialized_runtime("read -r line").await;
    let daemon = runtime.release_daemon();

    for tick in 0..3 {
        let found = report(&runtime.server).await;
        assert_eq!(found.outcome, PlacementOutcome::Escaped, "tick {tick}");
        assert_eq!(found.processes.len(), 1, "tick {tick}");
        assert_eq!(found.processes[0].pid, daemon, "tick {tick}");
    }
    assert_eq!(escape_warnings(&capture), 1);

    // The check the stop runs finds the same daemon and logs nothing more.
    assert!(matches!(runtime.stop().await, ResponseKind::Stopped { .. }));
    end(daemon);
    let after = latest(&runtime.server).await.expect("the stop keeps it");
    assert_eq!(after.outcome, PlacementOutcome::Escaped);
    assert_eq!(escape_warnings(&capture), 1);
}

#[tokio::test]
async fn a_daemon_left_at_a_stop_is_reported_by_the_check_the_stop_runs() {
    let (capture, _subscriber) = capture_warnings();
    // On the stop's signal the root ends and reaps the daemon, so the check
    // that runs when the runtime ends finds nothing: only the check the stop
    // itself runs, before it signals, can have seen the daemon.
    let runtime = initialized_runtime(
        "trap 'kill -KILL $(cat daemon.pid); wait $(cat daemon.pid); while kill -0 $(cat daemon.pid) 2>/dev/null; do sleep 0.02; done; exit 0' HUP TERM\n\
         : > trapped\nsleep 600 &\nwait $!",
    )
    .await;
    runtime.release_daemon();
    assert_eq!(escape_warnings(&capture), 0, "no check saw the daemon yet");
    poll_until("the root to install its trap", || {
        runtime
            .directory
            .path()
            .join("trapped")
            .exists()
            .then_some(())
    });

    assert!(matches!(runtime.stop().await, ResponseKind::Stopped { .. }));

    // `Stopped` follows the terminal commit, which is written after the check
    // that ends the runtime. A trap that outran the stop grace would leave the
    // daemon for that check to report, so a clean commit proves the root
    // reaped the daemon and the one WARN came from the stop's own check.
    assert_eq!(
        latest(&runtime.server).await.map(|report| report.outcome),
        Some(PlacementOutcome::Clean),
        "the root killed the daemon within the stop grace"
    );
    assert_eq!(escape_warnings(&capture), 1);
}

#[tokio::test]
async fn a_daemon_left_after_the_root_exits_is_reported_when_the_runtime_ends() {
    let (capture, _subscriber) = capture_warnings();
    let runtime = initialized_runtime("until [ -e end ]; do sleep 0.02; done\nexit 0").await;
    let daemon = runtime.release_daemon();
    std::fs::write(runtime.directory.path().join("end"), b"").expect("let the root exit");

    wait_until("the runtime to end", || async {
        (runtime.server.shared.state.lock().await.phase == WireRuntimePhase::Exited).then_some(())
    })
    .await;
    end(daemon);

    let found = latest(&runtime.server).await.expect("the end's check");
    assert_eq!(found.outcome, PlacementOutcome::Escaped);
    assert_eq!(found.processes.len(), 1);
    assert_eq!(found.processes[0].pid, daemon);
    assert_eq!(escape_warnings(&capture), 1);
}
