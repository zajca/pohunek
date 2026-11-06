//! The placement escape check against real processes on a real PTY.
//!
//! Each scenario runs a shell script as the PTY child and arms the check the
//! way `Initialize` does. Expected outcomes follow from the shape of the
//! processes the script starts (their session, name and marker), never from
//! the check's own output; the scripts publish files so a scenario inspects
//! only once every process has reached its steady state.

use std::path::Path;

use pohunek_platform::process::{HostInspector, ProcessInspector as _};
use pohunek_test_support::wait::poll_until;
use pohunek_worker_protocol::{PlacementOutcome, PlacementReport, CURRENT_VERSION};

use crate::placement::Armed;
use crate::server::Server;
use crate::server::{inspect_snapshot, run_placement_check, wait_runtime_exit};

/// The generation marker the fixture's processes carry.
///
/// Unique to the test process: scenarios run in parallel and share one process
/// table, so a constant marker would let one scenario's daemon appear in
/// another's check.
fn marker() -> String {
    format!("runtime-placement-{}", std::process::id())
}

/// The PTY child's environment: the pinned fixture environment plus the
/// generation marker a worker injects into every child.
fn marked_env() -> Vec<(String, String)> {
    let mut env = crate::test_support::child_env();
    env.push(("POHUNEK_WORKER_INSTANCE_ID".to_owned(), marker()));
    env
}

/// Arms the check for `pty` as `Initialize` does.
async fn arm(server: &Server, pty: &crate::PtyOwner) {
    let pty_session = HostInspector::new()
        .process_session(pty.identity().pid)
        .expect("read the PTY child's session");
    assert!(pty_session.is_some(), "the PTY child runs");
    server.shared.state.lock().await.placement.armed = Some(Armed {
        pty_session,
        worker_instance_id: marker(),
        worker_pid: std::process::id(),
    });
}

async fn report(server: &Server) -> PlacementReport {
    run_placement_check(&server.shared).await;
    let state = server.shared.state.lock().await;
    inspect_snapshot(&server.shared, &state, CURRENT_VERSION)
        .await
        .expect("inspect the worker")
        .placement
        .expect("an armed runtime reports its check")
}

fn pid_in(directory: &Path, file: &str) -> u32 {
    poll_until(&format!("the fixture to publish {file}"), || {
        std::fs::read_to_string(directory.join(file))
            .ok()
            .and_then(|text| text.trim().parse().ok())
    })
}

fn end(pid: u32) {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(pid).expect("a process id"));
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
}

/// Starts a daemon image named for the provider in a session of its own, the
/// way an agent prewarming a shared host daemon does.
const PREWARM_DAEMON: &str = "cp /bin/sh claude-daemon\n\
    setsid sh -c 'echo $$ > daemon.pid; exec ./claude-daemon -c \"sleep 600; :\"' &\n\
    until [ -e daemon.pid ]; do sleep 0.02; done\n";

#[tokio::test]
async fn a_daemon_that_outlives_the_root_is_reported_at_generation_end() {
    let script = format!("{PREWARM_DAEMON}exit 0\n");
    let (server, pty, directory) =
        super::launch_claim_fixture_env(&script, crate::WorkerConfig::new(), marked_env()).await;
    arm(&server, &pty).await;
    wait_runtime_exit(&pty).await.expect("the root exits");
    let daemon = pid_in(directory.path(), "daemon.pid");

    let found = report(&server).await;
    end(daemon);

    assert_eq!(found.outcome, PlacementOutcome::Escaped);
    assert_eq!(found.processes.len(), 1);
    assert_eq!(found.processes[0].pid, daemon);
    assert_eq!(found.processes[0].comm, "claude-daemon");
}

#[tokio::test]
async fn processes_that_stay_or_are_not_the_generations_are_not_reported() {
    // A tool wrapper in a session of its own that is neither named for the
    // provider nor listening; a provider-named image in another session
    // without this generation's marker; a plain background child.
    let script = "cp /bin/sh claude-daemon\n\
        env -u POHUNEK_WORKER_INSTANCE_ID setsid sh -c 'echo $$ > foreign.pid; exec ./claude-daemon -c \"sleep 600; :\"' &\n\
        setsid sh -c 'echo $$ > wrapper.pid; exec sleep 600' &\n\
        sleep 600 &\n\
        until [ -e foreign.pid ] && [ -e wrapper.pid ]; do sleep 0.02; done\n\
        echo ready > ready\n\
        wait\n";
    let (server, pty, directory) =
        super::launch_claim_fixture_env(script, crate::WorkerConfig::new(), marked_env()).await;
    arm(&server, &pty).await;
    let foreign = pid_in(directory.path(), "foreign.pid");
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
    let (server, pty, _directory) =
        super::launch_claim_fixture_env("read -r line", crate::WorkerConfig::new(), marked_env())
            .await;
    server.shared.state.lock().await.placement.armed = Some(Armed {
        pty_session: None,
        worker_instance_id: marker(),
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
    let (server, pty, _directory) =
        super::launch_claim_fixture_env("read -r line", crate::WorkerConfig::new(), marked_env())
            .await;

    run_placement_check(&server.shared).await;
    let state = server.shared.state.lock().await;
    let snapshot = inspect_snapshot(&server.shared, &state, CURRENT_VERSION)
        .await
        .expect("inspect the worker");
    drop(state);

    assert_eq!(snapshot.placement, None);
    pty.stop(
        "test-cleanup",
        server.shared.config.stop_grace,
        server.shared.config.kill_reap_deadline,
    )
    .await
    .expect("stop the PTY");
}
