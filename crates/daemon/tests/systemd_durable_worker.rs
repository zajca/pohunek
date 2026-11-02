//! Real-systemd durability of worker generations across daemon restarts.
//!
//! Linux-only: it drives transient user units of the systemd backend. The
//! sessions are Claude sessions launched from the official Claude package,
//! which the fixture installs into the daemon's plugin root, because only
//! that package serves `claude`.

#![cfg(target_os = "linux")]

#[path = "support/official_runtimes.rs"]
mod official_runtimes;

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pohunek_client::{Client, ClientOptions};
use pohunek_daemon::agent::{InputRules, NativeSessionLaunch};
use pohunek_daemon::runtime::Worker;
use pohunek_daemon::store::{
    DesiredState, ResumeBinding, RuntimeRecord, SessionRecord, Store, StoredInputRules,
};
use pohunek_platform::supervisor::systemd::SystemdSupervisor;
use pohunek_platform::supervisor::{
    JobDefinition, JobSpec, Namespace, RestartPolicy, Supervisor, WorkerKey,
};
use pohunek_service_config::{ConfigSpec, Deadlines, ServiceConfig};
use pohunek_worker_protocol::{
    BaseEnv, Dimensions, Initialize, InitializeLimits, InputPolicy, LaunchIdentity, SecretEnv,
    SessionId as WorkerSessionId, StopPolicy, TransactionId, Version,
    DEFAULT_ENVIRONMENT_ALLOWLIST,
};
use protocol::{
    method, AgentActivity, AttachHeader, CwdSource, RuntimeRef, RuntimeState, SessionAttachParams,
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
/// Fixed daemon-issued generation for the fixture's hand-started workers.
const WORKER_GENERATION: &str = "abcd2345";
/// Installation version directory the fixture's `service.toml` activates.
const FIXTURE_VERSION: &str = "0.0.0-e2e";
/// Name prefix of the fixture root directory. Kept short: the worker socket
/// paths below the root are limited to 107 bytes.
const ROOT_PREFIX: &str = "phsd-";
/// Bound on one D-Bus call to the user manager while starting fixture workers.
const SYSTEMD_CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Worker socket negotiation bound written to the fixture's `service.toml`.
const WORKER_CONNECT: Duration = Duration::from_secs(10);
/// Worker initialization bound written to the fixture's `service.toml`.
const WORKER_INITIALIZE: Duration = Duration::from_secs(45);
/// Worker and daemon stop grace written to the fixture's `service.toml`.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Daemon restart throttle written to the fixture's `service.toml`.
const RESTART_THROTTLE: Duration = Duration::from_secs(5);
/// Orphan sweep grace written to the fixture's `service.toml`.
const SWEEP_GRACE: Duration = Duration::from_secs(5);
/// Open-file limit written to the fixture's `service.toml`.
const OPEN_FILES: u64 = 8_192;
/// Scheduling margin on top of the sweep grace and one manager call when the
/// daemon classifies a killed worker.
const LOSS_MARGIN: Duration = Duration::from_secs(15);
/// `MemoryMax` of the running worker unit: below what the worker already
/// holds, so the kernel's OOM killer runs as soon as the limit is written.
const OOM_MEMORY_MAX: &str = "1M";
/// Makes the kernel pick the worker over its own children.
const OOM_SCORE_ADJ_MAX: &str = "1000";
/// `MemoryHigh` of the running worker unit: below what the worker already
/// holds, so every charge is throttled and the unit stalls on memory without
/// the kernel ever reaching an OOM kill.
const OOMD_MEMORY_HIGH: &str = "1M";
/// Memory pressure (share of time stalled) above which `systemd-oomd` kills
/// the units of the sessions slice; low so the throttled worker crosses it.
const OOMD_PRESSURE_LIMIT: &str = "1%";
/// How long the pressure must hold before `systemd-oomd` acts.
const OOMD_PRESSURE_DURATION: &str = "2s";
/// Bound on waiting for `systemd-oomd`, which samples pressure once a second
/// and then waits out [`OOMD_PRESSURE_DURATION`].
const OOMD_TIMEOUT: Duration = Duration::from_secs(120);

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
    let fixture = Fixture::new(&worker_bin);
    fixture.install_claude_package().await;

    fixture.start_worker(&fixture.worker_key).await;
    fixture.start_worker(&fixture.isolation_worker_key).await;
    let worker = fixture.connect_worker("fixture-controller").await;
    let worker_id = worker.worker_id().await;
    let worker_instance_id = worker
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
    fixture.persist_record(
        &worker_id.to_string(),
        &worker_instance_id.to_string(),
        child_pid,
    );
    let isolation_worker = fixture
        .connect_worker_for(
            &fixture.isolation_session_id,
            "fixture-isolation-controller",
        )
        .await;
    let isolation_worker_id = isolation_worker.worker_id().await;
    let isolation_worker_instance_id = isolation_worker
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
        &fixture.isolation_worker_key,
        &isolation_worker_id.to_string(),
        &isolation_worker_instance_id.to_string(),
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
        &worker_instance_id.to_string(),
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
        isolation_worker_instance_id.as_str(),
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
        &worker_instance_id.to_string(),
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
        &worker_instance_id.to_string(),
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
        .call::<method::SessionResume>(SessionId(fixture.session_id.clone()).into())
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
    let recovered_worker_instance_id = recovered_runtime
        .worker_instance_id
        .as_deref()
        .expect("recovered runtime id");
    assert_eq!(recovered.id.0, fixture.session_id);
    assert_eq!(recovered.created_at, original_created_at);
    assert_ne!(recovered.pid, child_pid);
    assert_eq!(
        Fixture::main_pid(&fixture.worker_unit),
        0,
        "recovery must retire the superseded generation"
    );
    assert_ne!(recovered_worker_id, worker_id.as_str());
    assert_ne!(recovered_worker_instance_id, worker_instance_id.as_str());
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
        persisted.runtime.worker_instance_id.as_deref(),
        Some(recovered_worker_instance_id)
    );
    let recovered_generation = persisted
        .runtime
        .generation
        .clone()
        .expect("recovered record names its generation");
    assert_ne!(recovered_generation, WORKER_GENERATION);
    let recovered_key = WorkerKey::new(fixture.session_id.clone(), recovered_generation)
        .expect("recovered generation is valid");
    assert_eq!(
        persisted.runtime.service_id.as_deref(),
        Some(recovered_key.service_id().as_str())
    );
    assert_eq!(
        persisted.runtime.executable.as_deref(),
        Some(fixture.worker_executable.as_path())
    );
    let recovered_unit = fixture.namespace.worker_unit(&recovered_key);
    assert_ne!(
        Fixture::main_pid(&recovered_unit),
        0,
        "the recovered generation runs as its own transient unit"
    );

    let repeated = client
        .call::<method::SessionResume>(SessionId(fixture.session_id.clone()).into())
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
        recovered_worker_instance_id,
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
    let isolation_runtime = lost_isolation.runtime.expect("isolation runtime");
    assert_eq!(isolation_runtime.state, RuntimeState::Lost);
    let isolation_exit = isolation_runtime.exit.expect("the manager's verdict");
    assert_eq!(
        (
            isolation_exit.cause.as_str(),
            isolation_exit.signal,
            isolation_exit.code,
            isolation_exit.oom_killer.as_deref(),
        ),
        ("signal", Some(9), None, None),
        "the manager's verdict on the SIGKILLed worker reaches the session"
    );
    assert!(
        isolation_exit.ended_at.is_some(),
        "the manager reports when the worker ended: {isolation_exit:?}"
    );
    assert_eq!(isolation_runtime.loss_cause, None);
    assert_eq!(
        Fixture::main_pid(&fixture.isolation_worker_unit),
        0,
        "Restart=no must not recreate a killed worker or fake PTY continuity"
    );
    assert_runtime(
        &fixture.inspect().await,
        recovered.pid,
        recovered_worker_id,
        recovered_worker_instance_id,
    );

    client
        .call::<method::SessionStop>(SessionId(fixture.session_id.clone()))
        .await
        .expect("stop recovered session");

    eprintln!(
        "systemd lifecycle passed: worker_pid={worker_unit_pid} child_pid={child_pid} \
         pty_device={} \
         worker_instance_id={worker_instance_id} recovered_worker_instance_id={recovered_worker_instance_id} \
         daemon_pids={daemon_pid},{restarted_daemon_pid},{killed_daemon_pid}",
        pty_device.display()
    );
}

/// Starts one initialized worker with a persisted session record, then the
/// daemon, and waits until the daemon has adopted the session.
async fn adopt_running_worker(fixture: &Fixture, daemon_bin: &Path) {
    prepare_worker(fixture).await;
    fixture.start_daemon(daemon_bin);
    fixture
        .wait_for_activity(&fixture.session_id, AgentActivity::Working)
        .await;
}

/// Starts one initialized worker unit with a persisted session record and no
/// daemon.
async fn prepare_worker(fixture: &Fixture) {
    fixture.install_claude_package().await;
    fixture.start_worker(&fixture.worker_key).await;
    let worker = fixture.connect_worker("fixture-controller").await;
    let worker_id = worker.worker_id().await;
    let worker_instance_id = worker
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
    fixture.persist_record(
        &worker_id.to_string(),
        &worker_instance_id.to_string(),
        child_pid,
    );
    worker
        .release_controller()
        .await
        .expect("release bootstrap worker controller");
    drop(worker);
    // timing-allowed: #326 lets the worker settle after its controller is released, as the lifecycle scenario does
    tokio::time::sleep(CONNECT_DELAY).await;
}

/// Polls the real daemon until the session's runtime is lost, within `bound`.
async fn wait_for_lost(fixture: &Fixture, bound: Duration, what: &str) -> SessionInfo {
    let deadline = tokio::time::Instant::now() + bound;
    loop {
        let info = fixture.inspect().await;
        if info
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.state == RuntimeState::Lost)
        {
            return info;
        }
        if tokio::time::Instant::now() >= deadline {
            fixture.print_status();
            panic!("{what} was not classified lost in time");
        }
        // timing-allowed: #326 polls the real daemon for the loss, bounded by the explicit deadline above
        tokio::time::sleep(CONNECT_DELAY).await;
    }
}

/// A worker unit OOM-killed under real memory pressure is a failed session
/// whose runtime says why: the manager's verdict is read before the daemon
/// retires the failed unit.
#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a real systemd user manager"]
async fn an_oom_killed_systemd_worker_is_a_failed_session_with_its_cause() {
    assert_eq!(
        std::env::var("POHUNEK_SYSTEMD_E2E").as_deref(),
        Ok("1"),
        "set POHUNEK_SYSTEMD_E2E=1 explicitly"
    );
    require_memory_controller();
    let daemon_bin = required_binary("POHUNEK_DAEMON_BIN");
    let worker_bin = required_binary("POHUNEK_WORKER_BIN");
    let fixture = Fixture::new(&worker_bin);
    adopt_running_worker(&fixture, &daemon_bin).await;

    let unit_pid = Fixture::main_pid(&fixture.worker_unit);
    assert_ne!(unit_pid, 0, "the worker unit runs");
    std::fs::write(format!("/proc/{unit_pid}/oom_score_adj"), OOM_SCORE_ADJ_MAX)
        .expect("make the worker the preferred OOM victim");
    Fixture::systemctl([
        "set-property",
        "--runtime",
        fixture.worker_unit.as_str(),
        format!("MemoryMax={OOM_MEMORY_MAX}").as_str(),
        "MemorySwapMax=0",
    ]);

    let lost = wait_for_lost(
        &fixture,
        SWEEP_GRACE + SYSTEMD_CALL_TIMEOUT + LOSS_MARGIN,
        "the OOM-killed worker",
    )
    .await;
    assert_eq!(lost.state, protocol::SessionState::Failed);
    assert_eq!(lost.activity, None);
    let runtime = lost.runtime.expect("lost runtime");
    let exit = runtime.exit.as_ref().expect("the manager's verdict");
    assert_eq!(exit.cause, "oom_kill", "{runtime:?}");
    assert_eq!(
        exit.oom_killer.as_deref(),
        Some("kernel"),
        "the kernel killed the worker: {runtime:?}"
    );
    assert!(
        exit.memory_peak_bytes.is_some_and(|peak| peak > 0),
        "the unit's peak memory is reported: {runtime:?}"
    );
    assert!(exit.ended_at.is_some(), "{runtime:?}");
    assert_eq!(runtime.loss_cause, None, "an exit explains the loss itself");
}

/// A worker unit killed by `systemd-oomd` is a failed session whose runtime
/// names `systemd_oomd` as the killer: the kill ends the unit with the
/// `oom-kill` result, so only the manager's kill counters tell it from the
/// kernel's.
#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager with the memory controller, and a running systemd-oomd"]
async fn a_systemd_oomd_killed_worker_is_a_failed_session_with_its_killer() {
    assert_eq!(
        std::env::var("POHUNEK_SYSTEMD_E2E").as_deref(),
        Ok("1"),
        "set POHUNEK_SYSTEMD_E2E=1 explicitly"
    );
    require_memory_controller();
    require_oomd();
    let daemon_bin = required_binary("POHUNEK_DAEMON_BIN");
    let worker_bin = required_binary("POHUNEK_WORKER_BIN");
    let fixture = Fixture::new(&worker_bin);
    adopt_running_worker(&fixture, &daemon_bin).await;

    // The slice is the monitored cgroup, the worker unit its candidate;
    // MemoryHigh throttles the worker so the kernel never kills it.
    Fixture::systemctl([
        "set-property",
        "--runtime",
        fixture.namespace.sessions_slice().as_str(),
        "ManagedOOMMemoryPressure=kill",
        format!("ManagedOOMMemoryPressureLimit={OOMD_PRESSURE_LIMIT}").as_str(),
        format!("ManagedOOMMemoryPressureDurationSec={OOMD_PRESSURE_DURATION}").as_str(),
    ]);
    Fixture::systemctl([
        "set-property",
        "--runtime",
        fixture.worker_unit.as_str(),
        format!("MemoryHigh={OOMD_MEMORY_HIGH}").as_str(),
    ]);

    // Input keeps the throttled worker allocating until `systemd-oomd` acts.
    let deadline = tokio::time::Instant::now() + OOMD_TIMEOUT;
    let lost = loop {
        let info = fixture.inspect().await;
        if info
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.state == RuntimeState::Lost)
        {
            break info;
        }
        if tokio::time::Instant::now() >= deadline {
            fixture.print_status();
            panic!("the worker was not killed by systemd-oomd in time");
        }
        if let Ok(mut client) = Client::connect_local(&fixture.socket).await {
            let _ = client
                .call::<method::SessionInput>(SessionInputParams {
                    session_id: SessionId(fixture.session_id.clone()),
                    text: "pressure".to_owned(),
                    wait: None,
                })
                .await;
        }
        // timing-allowed: #326 polls the real daemon while systemd-oomd samples pressure, bounded by the explicit deadline above
        tokio::time::sleep(CONNECT_DELAY).await;
    };

    assert_eq!(lost.state, protocol::SessionState::Failed);
    let runtime = lost.runtime.expect("lost runtime");
    let exit = runtime.exit.as_ref().expect("the manager's verdict");
    assert_eq!(exit.cause, "oom_kill", "{runtime:?}");
    assert_eq!(
        exit.oom_killer.as_deref(),
        Some("systemd_oomd"),
        "systemd-oomd killed the worker, the kernel did not: {runtime:?}"
    );
    assert!(exit.memory_peak_bytes.is_some(), "{runtime:?}");
    assert!(exit.ended_at.is_some(), "{runtime:?}");
    assert_eq!(runtime.loss_cause, None, "an exit explains the loss itself");
}

/// Fails with a stated precondition unless `systemd-oomd` runs on the host
/// and the kernel exposes memory pressure, which it samples.
fn require_oomd() {
    let output = Command::new("systemctl")
        .args(["is-active", "systemd-oomd"])
        .output()
        .expect("run systemctl");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "active",
        "precondition: systemd-oomd is not running; install and start it \
         (`systemctl enable --now systemd-oomd`) before running this scenario"
    );
    assert!(
        Path::new("/proc/pressure/memory").exists(),
        "precondition: the kernel exposes no memory pressure (PSI), which systemd-oomd samples"
    );
}

/// A worker unit an operator reset (`reset-failed`) while the user manager
/// stayed alive leaves no verdict and no explanation: the loss stays plain.
#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a real systemd user manager"]
async fn a_reset_worker_unit_under_a_live_manager_is_lost_without_an_explanation() {
    assert_eq!(
        std::env::var("POHUNEK_SYSTEMD_E2E").as_deref(),
        Ok("1"),
        "set POHUNEK_SYSTEMD_E2E=1 explicitly"
    );
    let daemon_bin = required_binary("POHUNEK_DAEMON_BIN");
    let worker_bin = required_binary("POHUNEK_WORKER_BIN");
    let fixture = Fixture::new(&worker_bin);
    prepare_worker(&fixture).await;

    // The unit ends and is reset before any daemon runs, so no verdict is
    // left to read.
    Fixture::systemctl([
        "kill",
        "--kill-whom=main",
        "--signal=KILL",
        fixture.worker_unit.as_str(),
    ]);
    let deadline = tokio::time::Instant::now() + SYSTEMD_CALL_TIMEOUT;
    while Fixture::main_pid(&fixture.worker_unit) != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the killed worker unit never ended"
        );
        // timing-allowed: #326 polls the real manager for the unit's end, bounded by the explicit deadline above
        tokio::time::sleep(CONNECT_DELAY).await;
    }
    Fixture::systemctl(["reset-failed", fixture.worker_unit.as_str()]);
    fixture.start_daemon(&daemon_bin);

    let lost = wait_for_lost(
        &fixture,
        SWEEP_GRACE + SYSTEMD_CALL_TIMEOUT + LOSS_MARGIN,
        "the reset worker",
    )
    .await;
    let runtime = lost.runtime.expect("lost runtime");
    assert_eq!(runtime.exit, None, "{runtime:?}");
    assert_eq!(
        runtime.loss_cause, None,
        "the user manager that held the worker still runs: {runtime:?}"
    );
}

/// The user manager that held the worker is killed and started again while
/// the session runs; the restarted daemon finds the worker gone with no
/// verdict and names the manager as the cause.
///
/// It kills the CI runner's own user manager through `sudo`, so it runs only
/// where `POHUNEK_PRIVILEGED_E2E=1` and passwordless `sudo` are given; a
/// local run adds `--skip privileged_`.
#[tokio::test]
#[ignore = "requires POHUNEK_SYSTEMD_E2E=1, POHUNEK_PRIVILEGED_E2E=1 and passwordless sudo (CI only)"]
async fn privileged_a_killed_user_manager_is_the_cause_of_a_lost_runtime() {
    assert_eq!(
        std::env::var("POHUNEK_SYSTEMD_E2E").as_deref(),
        Ok("1"),
        "set POHUNEK_SYSTEMD_E2E=1 explicitly"
    );
    assert_eq!(
        std::env::var("POHUNEK_PRIVILEGED_E2E").as_deref(),
        Ok("1"),
        "set POHUNEK_PRIVILEGED_E2E=1 explicitly; this scenario kills the user manager"
    );
    let daemon_bin = required_binary("POHUNEK_DAEMON_BIN");
    let worker_bin = required_binary("POHUNEK_WORKER_BIN");
    let fixture = Fixture::new(&worker_bin);
    adopt_running_worker(&fixture, &daemon_bin).await;

    let uid = nix::unistd::Uid::effective().as_raw();
    let manager_unit = format!("user@{uid}.service");
    let sudo_systemctl = |arguments: &[&str]| {
        let output = Command::new("sudo")
            .args(["--non-interactive", "systemctl"])
            .args(arguments)
            .output()
            .expect("run sudo systemctl");
        assert_success(&output);
    };
    sudo_systemctl(&["kill", "--signal=KILL", manager_unit.as_str()]);
    // The kill is asynchronous: start only once the manager unit has ended.
    let deadline = tokio::time::Instant::now() + SYSTEMD_CALL_TIMEOUT;
    loop {
        let active = Command::new("systemctl")
            .args(["is-active", manager_unit.as_str()])
            .output()
            .expect("run systemctl is-active");
        if String::from_utf8_lossy(&active.stdout).trim() != "active" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the killed user manager unit never ended"
        );
        // timing-allowed: #326 polls the real system manager for the unit's end, bounded by the explicit deadline above
        tokio::time::sleep(CONNECT_DELAY).await;
    }
    sudo_systemctl(&["start", manager_unit.as_str()]);
    // A runner whose manager starts `degraded` is as usable as a `running` one.
    let deadline = tokio::time::Instant::now() + SYSTEMD_CALL_TIMEOUT;
    loop {
        let state = Fixture::systemctl_output(["is-system-running"]);
        let state = String::from_utf8_lossy(&state.stdout).trim().to_owned();
        if matches!(state.as_str(), "running" | "degraded") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the user manager did not come back (state {state:?})"
        );
        // timing-allowed: #326 polls the restarted real manager, bounded by the explicit deadline above
        tokio::time::sleep(CONNECT_DELAY).await;
    }
    fixture.start_daemon(&daemon_bin);

    let lost = wait_for_lost(
        &fixture,
        SWEEP_GRACE + SYSTEMD_CALL_TIMEOUT + LOSS_MARGIN,
        "the worker of the killed user manager",
    )
    .await;
    let runtime = lost.runtime.expect("lost runtime");
    assert_eq!(
        runtime.loss_cause.as_deref(),
        Some("user_manager_lost"),
        "{runtime:?}"
    );
    assert_eq!(runtime.exit, None, "no unit survived to give a verdict");
}

/// The OOM scenario needs the user manager to enforce `MemoryMax`, which
/// requires the memory controller to be delegated to it. A runner without it
/// fails here with the reason instead of skipping.
fn require_memory_controller() {
    let uid = nix::unistd::Uid::effective().as_raw();
    let controllers = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/cgroup.controllers"
    ));
    let delegated = std::fs::read_to_string(&controllers).unwrap_or_default();
    assert!(
        delegated.split_whitespace().any(|name| name == "memory"),
        "precondition: the systemd user manager does not have the memory controller \
         delegated ({} lists: {delegated:?}); delegate it (for example \
         `Delegate=cpu memory pids` for user@.service) before running this scenario",
        controllers.display()
    );
}

/// Fails with the offending path when a fixture session's worker socket, or
/// the staged bind path beside it, exceeds the platform limit.
///
/// The worker refuses to start in that case; checking before any unit starts
/// reports the path instead of a worker that never became ready.
fn assert_worker_sockets_fit<'a>(
    runtime_root: &Path,
    session_ids: impl IntoIterator<Item = &'a String>,
) {
    let platform = pohunek_paths::Platform::current().expect("supported platform");
    let workers_root = runtime_root.join(pohunek_paths::WORKERS_SUBDIR);
    for id in session_ids {
        pohunek_paths::worker_socket_path(&workers_root, id, platform)
            .unwrap_or_else(|error| {
                panic!("the fixture's worker socket path does not fit: {error}")
            })
            .expect("the fixture session id names a worker");
    }
}

struct Fixture {
    root: PathBuf,
    runtime_home: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    state_home: PathBuf,
    session_id: String,
    worker_key: WorkerKey,
    worker_unit: String,
    isolation_session_id: String,
    isolation_worker_key: WorkerKey,
    isolation_worker_unit: String,
    daemon_unit: String,
    socket: PathBuf,
    namespace: Namespace,
    service_config: PathBuf,
    worker_executable: PathBuf,
    /// The program the persisted sessions were launched with: a fake `claude`
    /// that answers the official package's version probe and otherwise runs
    /// its arguments with `/bin/sh`.
    claude_program: PathBuf,
}

/// Creates a private Claude transcript for a seeded native reference.
fn seed_transcript(home: &Path, reference: &str) {
    let transcript_dir = home.join(".claude/projects/fixture");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&transcript_dir)
        .expect("create private Claude transcript directory");
    let transcript = transcript_dir.join(format!("{reference}.jsonl"));
    std::fs::write(&transcript, "{}\n").expect("write fixture Claude transcript");
    std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o600))
        .expect("private Claude transcript");
}

/// Writes a fake `claude` below `root` that answers the official Claude
/// package's version probe and otherwise runs its arguments with `/bin/sh`,
/// and returns its path.
fn write_fake_claude(root: &Path) -> PathBuf {
    let program = root.join("bin").join("claude");
    std::fs::create_dir_all(program.parent().expect("the fake claude directory"))
        .expect("create the fake claude directory");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\n{}exec /bin/sh \"$@\"\n",
            official_runtimes::sh_version_answer("claude")
        ),
    )
    .expect("write the fake claude");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
        .expect("make the fake claude executable");
    program
}

impl Fixture {
    /// Lays out an isolated installation: XDG tree, versioned worker binary,
    /// and an explicit `service.toml` whose namespace names the fixture's
    /// transient units.
    #[expect(
        clippy::too_many_lines,
        reason = "one literal spells every ConfigSpec field of the test installation"
    )]
    fn new(worker_bin: &Path) -> Self {
        // One number names the fixture: the pid in the high bits and the
        // sub-second nanoseconds (below 2^30) in the low bits. The session ids
        // and unit names spell it in decimal; the root directory spells it in
        // hex, so the worker socket paths below it stay short.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .subsec_nanos();
        let token = (u64::from(std::process::id()) << 30) | u64::from(nanos);
        let unique = token.to_string();
        let root = pohunek_test_support::temp_root().join(format!("{ROOT_PREFIX}{token:x}"));
        let runtime_home = root.join("runtime");
        let config_home = root.join("config");
        let data_home = root.join("data");
        let state_home = root.join("state");
        for path in [&runtime_home, &config_home, &data_home, &state_home] {
            std::fs::create_dir_all(path).expect("create fixture XDG directory");
        }
        seed_transcript(&root, "native-systemd-recovery");
        for path in [
            runtime_home.join("pohunek"),
            state_home.join("pohunek"),
            config_home.join("pohunek"),
        ] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .expect("create private application directory");
        }
        let state_root =
            std::fs::canonicalize(state_home.join("pohunek")).expect("canonical state root");
        let runtime_root =
            std::fs::canonicalize(runtime_home.join("pohunek")).expect("canonical runtime root");
        let prefix = std::fs::canonicalize(&root)
            .expect("canonical fixture root")
            .join("prefix");
        let service = ServiceConfig::new(ConfigSpec {
            prefix,
            active_version: FIXTURE_VERSION.to_owned(),
            uid: nix::unistd::Uid::effective().as_raw(),
            state_root,
            runtime_root,
            deadlines: Deadlines {
                worker_connect: WORKER_CONNECT,
                worker_initialize: WORKER_INITIALIZE,
                launchctl_command: SYSTEMD_CALL_TIMEOUT,
                worker_exit_timeout: EXIT_TIMEOUT,
                daemon_exit_timeout: EXIT_TIMEOUT,
                daemon_restart_throttle: RESTART_THROTTLE,
                registry_shutdown: pohunek_service_config::DEFAULT_REGISTRY_SHUTDOWN,
                create_drain: pohunek_service_config::DEFAULT_CREATE_DRAIN,
                driver_shutdown: pohunek_service_config::DEFAULT_DRIVER_SHUTDOWN,
                event_log_flush: pohunek_service_config::DEFAULT_EVENT_LOG_FLUSH,
            },
            environment_allowlist: DEFAULT_ENVIRONMENT_ALLOWLIST
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            search_path: pohunek_platform::shell_env::SearchPath::default(),
            sweep_grace: SWEEP_GRACE,
            input_timing: pohunek_service_config::DEFAULT_INPUT_TIMING,
            wait_timing: pohunek_service_config::DEFAULT_WAIT_TIMING,
            hook_stderr_tail_bytes: pohunek_service_config::DEFAULT_HOOK_STDERR_TAIL_BYTES,
            hook_required: Vec::new(),
            event_retention: pohunek_service_config::DEFAULT_EVENT_RETENTION,
            inventory_probe: pohunek_service_config::DEFAULT_INVENTORY_PROBE,
            worker_journal_max_age: pohunek_service_config::DEFAULT_WORKER_JOURNAL_MAX_AGE,
            open_files: OPEN_FILES,
        })
        .expect("valid fixture service configuration");
        let service_config = config_home.join("pohunek/service.toml");
        service
            .write(&service_config)
            .expect("write fixture service configuration");
        let worker_executable = service.worker_executable();
        std::fs::create_dir_all(
            worker_executable
                .parent()
                .expect("versioned worker has a directory"),
        )
        .expect("create versioned worker directory");
        std::fs::copy(worker_bin, &worker_executable).expect("install versioned worker binary");
        let namespace = service.namespace();
        let session_id = format!("s-{unique}");
        let isolation_session_id = format!("s-{unique}9");
        assert_worker_sockets_fit(
            &runtime_home.join("pohunek"),
            [&session_id, &isolation_session_id],
        );
        let worker_key = WorkerKey::new(session_id.clone(), WORKER_GENERATION).expect("worker key");
        let isolation_worker_key = WorkerKey::new(isolation_session_id.clone(), WORKER_GENERATION)
            .expect("isolation worker key");
        let claude_program = write_fake_claude(&root);
        Self {
            socket: runtime_home.join("pohunek/daemon.sock"),
            worker_unit: namespace.worker_unit(&worker_key),
            isolation_worker_unit: namespace.worker_unit(&isolation_worker_key),
            daemon_unit: format!("pohunek-rfc-daemon-{unique}.service"),
            root,
            runtime_home,
            config_home,
            data_home,
            state_home,
            session_id,
            worker_key,
            isolation_session_id,
            isolation_worker_key,
            namespace,
            service_config,
            worker_executable,
            claude_program,
        }
    }

    /// Installs the official Claude package into the daemon's plugin root
    /// before the daemon starts, so the daemon serves `claude` from it.
    async fn install_claude_package(&self) {
        official_runtimes::OfficialCatalog::new()
            .preinstall(&self.state_home.join("pohunek/plugins"), &self.root)
            .await;
    }

    /// Registers one worker generation through the transient-unit backend,
    /// exactly as the daemon's lifecycle engine would.
    async fn start_worker(&self, key: &WorkerKey) {
        let supervisor = SystemdSupervisor::connect(self.namespace.clone(), SYSTEMD_CALL_TIMEOUT)
            .await
            .expect("connect the systemd user manager");
        let definition = JobDefinition::new(JobSpec {
            executable: self.worker_executable.clone(),
            arguments: vec![
                "--session-id".to_owned(),
                key.session_id().to_owned(),
                "--worker-generation".to_owned(),
                key.generation().to_owned(),
                "--service-config".to_owned(),
                path_string(&self.service_config),
                "--daemon-socket-path".to_owned(),
                path_string(&self.socket),
            ],
            environment: self.bootstrap_environment(),
            working_directory: self.root.clone(),
            logs: None,
            start_timeout: WORKER_INITIALIZE,
            exit_timeout: EXIT_TIMEOUT,
            restart: RestartPolicy::Never,
            open_files: OPEN_FILES,
        })
        .expect("valid worker definition");
        supervisor
            .start(&key.service_id(), &definition)
            .await
            .expect("start transient worker unit");
    }

    fn bootstrap_environment(&self) -> BTreeMap<String, String> {
        [
            ("XDG_RUNTIME_DIR", &self.runtime_home),
            ("XDG_CONFIG_HOME", &self.config_home),
            ("XDG_DATA_HOME", &self.data_home),
            ("XDG_STATE_HOME", &self.state_home),
            ("HOME", &self.root),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), path_string(value)))
        .collect()
    }

    fn start_daemon(&self, daemon_bin: &Path) {
        let mut command = Command::new("systemd-run");
        command
            .args(["--user", "--unit", self.daemon_unit.as_str()])
            .args(["--property", "Type=notify"])
            .args(["--property", "NotifyAccess=main"])
            .args(["--property", "Restart=on-failure"])
            .args(["--property", "RestartSec=100ms"])
            .args(["--property", "KillMode=control-group"]);
        for (key, value) in self.bootstrap_environment() {
            command.arg(format!("--setenv={key}={value}"));
        }
        command
            .arg("--setenv=POHUNEK_OBSERVE_EXTERNAL_AGENTS=0")
            .arg(daemon_bin)
            .arg("--service-config")
            .arg(&self.service_config);
        let output = command.output().expect("run transient daemon unit");
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
            daemon_instance_id: Some(
                pohunek_worker_protocol::DaemonId::new("daemon-systemd-test").expect("daemon id"),
            ),
            transaction_id: TransactionId::new(format!("create-{session_id}"))
                .expect("transaction id"),
            expected_worker_id: worker_id,
            launch: LaunchIdentity {
                agent: "claude".to_owned(),
                agent_base: "claude".to_owned(),
                reference_kind: Some("id".to_owned()),
            },
            executable: PathBuf::from("/bin/sh"),
            // The counter rewrites one line instead of printing one per tick:
            // an attach snapshot repaints only the visible screen, so a
            // scrolling counter would push the burst marker off it within a
            // few seconds, however long the test then takes to attach.
            arguments: vec![
                "-c".to_owned(),
                format!(
                    "printf '\\033]0;working\\007'; \
                 head -c {REPLAY_BURST_BYTES} /dev/zero | tr '\\000' 'R'; \
                 printf '\\n{}\\n'; \
                 (n=0; while :; do n=$((n+1)); printf '\\rcounter:%04d' \"$n\"; \
                 sleep 0.1; done) & \
                 while IFS= read -r line; do printf 'input:%s\\n' \"$line\"; done",
                    std::str::from_utf8(REPLAY_BURST_MARKER).expect("replay marker is UTF-8")
                ),
            ],
            cwd: self.root.clone(),
            dimensions: Dimensions::new(80, 24).expect("dimensions"),
            environment: SecretEnv::new(BTreeMap::new()).expect("environment"),
            base_environment: BaseEnv::from_allowlist(
                DEFAULT_ENVIRONMENT_ALLOWLIST,
                std::env::vars_os(),
            )
            .expect("base environment"),
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
            input_policy: InputPolicy::new(3000).expect("input policy"),
            hook_protocol_version: Version::new(1).expect("hook protocol version"),
            public_protocol_version: PROTOCOL_VERSION.get(),
            hook_schema: None,
            answers_program_status_probe: false,
        }
    }

    fn persist_record(&self, worker_id: &str, worker_instance_id: &str, child_pid: u32) {
        self.persist_record_for(
            &self.session_id,
            &self.worker_key,
            worker_id,
            worker_instance_id,
            child_pid,
        );
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one persisted session record literal"
    )]
    fn persist_record_for(
        &self,
        session_id: &str,
        worker_key: &WorkerKey,
        worker_id: &str,
        worker_instance_id: &str,
        child_pid: u32,
    ) {
        let now = "2026-07-23T00:00:00Z".to_owned();
        let info = SessionInfo {
            spawned_by: None,
            id: SessionId(session_id.to_owned()),
            external: Some(false),
            capabilities: SessionCapabilities {
                resume: true,
                fork: true,
                ..Default::default()
            },
            name: Some("systemd durability e2e".to_owned()),
            agent: "claude".to_owned(),
            agent_base: RuntimeRef::claude(),
            cwd: self.root.clone(),
            cwd_source: Some(CwdSource::Launch),
            pid: child_pid,
            runtime: Some(SessionRuntime {
                state: RuntimeState::Live,
                runtime_generation: protocol::RuntimeGeneration::new(1),
                worker_id: Some(worker_id.to_owned()),
                worker_instance_id: Some(worker_instance_id.to_owned()),
                started_at: Some(now.clone()),
                last_connected_at: Some(now.clone()),
                loss_reason: None,
                exit: None,
                loss_cause: None,
            }),
            cols: 80,
            rows: 24,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            subagents: Vec::new(),
            background: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_last_activity_at: None,
            native_session_id: Some("native-systemd-recovery".to_owned()),
            native_session_path: None,
            project_id: None,
            project_label: None,
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            capability_refusals: None,
            output_force_closed: false,
            metadata: BTreeMap::new(),
            created_at: now.clone(),
            updated_at: now,
            exit_code: None,
        };
        let store = Store::new(self.data_home.join("pohunek/metadata.jsonl"));
        store
            .record_session(&SessionRecord {
                create_request: None,
                schema_version: 1,
                session_id: session_id.to_owned(),
                desired_state: DesiredState::Running,
                transaction: None,
                info,
                recovery: Some(ResumeBinding {
                    spawned_by: None,
                    session_id: session_id.to_owned(),
                    name: Some("systemd durability e2e".to_owned()),
                    agent: "claude".to_owned(),
                    agent_base: RuntimeRef::claude(),
                    cwd: self.root.clone(),
                    cols: 80,
                    rows: 24,
                    native_session_id: Some("native-systemd-recovery".to_owned()),
                    native_session_path: None,
                    project_id: None,
                    is_linked_worktree: None,
                    metadata: BTreeMap::new(),
                    program: path_string(&self.claude_program),
                    args: vec!["-c".to_owned(), "while :; do sleep 1; done".to_owned()],
                    input_rules: StoredInputRules::from(InputRules::unrestricted(
                        false,
                        Duration::from_millis(150),
                    )),
                    native_launch: Some(
                        NativeSessionLaunch::from_templates(
                            pohunek_daemon::agent::SessionRefKind::Id,
                            &["--resume", "{reference}"],
                            Some(&["--resume", "{reference}", "--fork-session"]),
                        )
                        .expect("valid launch templates"),
                    ),
                    launch_binding: pohunek_daemon::agent::host::LaunchPin::Pinned(Box::new(
                        official_runtimes::package_binding("claude"),
                    )),
                    native_reference_provenance:
                        pohunek_daemon::agent::NativeReferenceProvenance::default(),
                    profile_revision: None,
                    native_launch_unresolved: false,
                }),
                active_agent_origin: None,
                native_identity_ordering: None,
                runtime: RuntimeRecord {
                    state: RuntimeState::Live,
                    worker_id: Some(worker_id.to_owned()),
                    worker_instance_id: Some(worker_instance_id.to_owned()),
                    service_id: Some(worker_key.service_id().to_string()),
                    generation: Some(worker_key.generation().to_owned()),
                    executable: Some(self.worker_executable.clone()),
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
                self.worker_units_pattern().as_str(),
                self.daemon_unit.as_str(),
                "--no-pager",
            ])
            .status();
    }

    /// Matches every worker generation of the fixture's namespace, including
    /// generations the daemon started during recovery.
    fn worker_units_pattern(&self) -> String {
        self.namespace.worker_unit_pattern()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for unit in [
            self.daemon_unit.clone(),
            self.worker_units_pattern(),
            self.namespace.sessions_slice(),
            // Parent slice systemd creates implicitly for the dashed sessions slice.
            format!("pohunek-{}.slice", self.namespace.as_str()),
        ] {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", unit.as_str()])
                .status();
            let _ = Command::new("systemctl")
                .args(["--user", "reset-failed", unit.as_str()])
                .status();
        }
        // Drops the runtime properties a scenario set on the sessions slice.
        let _ = Command::new("systemctl")
            .args(["--user", "revert", self.namespace.sessions_slice().as_str()])
            .status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn path_string(path: &Path) -> String {
    path.to_str().expect("fixture paths are UTF-8").to_owned()
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

fn assert_runtime(info: &SessionInfo, child_pid: u32, worker_id: &str, worker_instance_id: &str) {
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
        runtime.worker_instance_id.as_deref(),
        Some(worker_instance_id),
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
