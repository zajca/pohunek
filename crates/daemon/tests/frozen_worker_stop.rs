//! A stop or removal whose worker stops answering ends with a typed error
//! instead of waiting for the caller's lifecycle budget.
//!
//! The scenario freezes a real `pohunek-sessiond` with `SIGSTOP` while its
//! control socket stays open, as a worker stuck in uninterruptible I/O is.
//! The daemon bounds its wait for the worker's stop answer by the stop policy
//! it sent plus the worker's terminal-commit wait, so the call returns a typed
//! runtime error, the session stays listed and stoppable, and a retry after
//! `SIGCONT` succeeds.

// Rust guideline compliant 2026-10-20

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pohunek_daemon::procwatch::HostInspector;
use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::time::AutoAdvanceInhibitor;
use pohunek_test_support::wait::{guard, wait_until};
use pohunek_test_support::worker_binary;
use protocol::{
    SessionId, SessionInputParams, SessionNewParams, SessionState, ENV_DAEMON_ID, ENV_SESSION_ID,
};

/// Seconds the session's root sleeps if the stop never ends it.
const ROOT_SLEEP_SECONDS: u32 = 30;

/// Stop grace of the scenario: short, so the bound the daemon derives from it
/// stays far below the hang guard.
const STOP_GRACE: Duration = Duration::from_millis(100);

/// Post-kill reap deadline of the scenario; never shorter than the grace.
const KILL_REAP_DEADLINE: Duration = Duration::from_millis(200);

/// Terminal-commit wait the daemon assumes for the worker. The frozen worker
/// never reaches its own wait, so this value only sizes the daemon's bound.
const TERMINAL_COMMIT_WAIT: Duration = Duration::from_millis(300);

/// Margin the daemon adds to the worker's stop bounds.
const WORKER_STOP_HEADROOM: Duration = Duration::from_millis(300);

/// How long a stop waits for an earlier exchange on the frozen worker's
/// control connection (a periodic inspection already stuck on it).
const EXCHANGE_WAIT: Duration = Duration::from_millis(500);

/// How long a stop waits behind an input write holding the control connection,
/// well above [`EXCHANGE_WAIT`] so the scenarios can tell the two apart.
const INPUT_STOP_WAIT: Duration = Duration::from_secs(5);

/// How long the worker waits for the agent to read a byte before it fails an
/// input; the scenarios' root never reads, so a large input holds the control
/// connection for this long.
const INPUT_STALL: Duration = Duration::from_secs(4);

/// Bytes of an input too large for the terminal's line buffer, so the worker
/// blocks delivering it.
const BLOCKING_INPUT_BYTES: usize = 200 * 1024;

/// Scheduler yields that let spawned tasks reach their next await.
const SETTLE_YIELDS: usize = 256;

/// Clock steps a scenario takes before it concludes a task will never finish.
const ADVANCE_STEPS_LIMIT: usize = 10_000;

/// Text the typed error carries.
const NO_ANSWER: &str = "did not answer the stop";

struct Frozen {
    env: TestEnv,
    launcher: Arc<SubprocessWorkerLauncher>,
    registry: SessionRegistry,
    id: SessionId,
    worker: Pid,
}

impl Frozen {
    /// Starts a session on a real worker and freezes the worker.
    async fn start() -> Self {
        let frozen = Self::start_running().await;
        kill(frozen.worker, Signal::SIGSTOP).expect("freeze the worker");
        frozen
    }

    /// Starts a session on a real worker that answers.
    async fn start_running() -> Self {
        let env = TestEnv::new().expect("create the test environment");
        let config = SessionRegistryConfig {
            shell_command: ShellCommand::new(
                "/bin/sh",
                ["-c", &format!("exec sleep {ROOT_SLEEP_SECONDS}")],
            ),
            stop_grace: STOP_GRACE,
            kill_reap_deadline: KILL_REAP_DEADLINE,
            terminal_commit_wait: TERMINAL_COMMIT_WAIT,
            worker_stop_headroom: WORKER_STOP_HEADROOM,
            worker_exchange_wait: EXCHANGE_WAIT,
            worker_input_stop_wait: INPUT_STOP_WAIT,
            input_write_stall_timeout: INPUT_STALL,
            ..SessionRegistryConfig::default()
        };
        let launcher = Arc::new(SubprocessWorkerLauncher::new());
        let worker_environment = SubprocessWorkerEnvironment {
            runtime_home: env.runtime_dir().to_path_buf(),
            state_home: env.state_home().to_path_buf(),
            data_home: env.data_home().to_path_buf(),
            config_home: env.config_home().to_path_buf(),
            cache_home: env.cache_home().to_path_buf(),
            home: env.home().to_path_buf(),
            daemon_socket: env.root().join("daemon.sock"),
        };
        let mut config = config;
        config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
        config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
        config.supervision = Some(
            worker_environment
                .supervision(worker_binary())
                .with_environment_source(EnvironmentSource::fixed(env.environment().clone())),
        );
        let registry = SessionRegistry::new_with_launcher_and_inspector(
            config,
            Arc::<SubprocessWorkerLauncher>::clone(&launcher),
            Arc::new(HostInspector::new()),
        );
        let created = {
            // A test run inside a pohunek session must not stamp the worker
            // with that session's markers.
            let mut process_env = ProcessEnv::lock();
            process_env.remove(ENV_DAEMON_ID).remove(ENV_SESSION_ID);
            registry
                .create(SessionNewParams {
                    name: Some("frozen-worker".to_owned()),
                    agent: "shell".to_owned(),
                    cwd: Some(env.cwd().to_path_buf()),
                    cols: 80,
                    rows: 24,
                    project: None,
                    repo: None,
                    branch: None,
                    base_branch: None,
                    input: None,
                    extended_input_ready_wait: None,
                    branch_source: None,
                    on_diverged: None,
                    input_ack_timeout_ms: None,
                    metadata: BTreeMap::new(),
                })
                .await
                .expect("create the session")
        };
        let worker = launcher
            .worker_process_id(&created.id.0)
            .await
            .expect("the session runs on a worker process");
        let worker = Pid::from_raw(i32::try_from(worker).expect("pid fits i32"));
        Self {
            env,
            launcher,
            registry,
            id: created.id,
            worker,
        }
    }

    fn thaw(&self) {
        kill(self.worker, Signal::SIGCONT).expect("thaw the worker");
    }

    async fn state(&self) -> SessionState {
        self.registry
            .inspect(&self.id)
            .await
            .expect("the session stays listed")
            .state
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        // A failing scenario must not leave a stopped worker behind.
        let _ = kill(self.worker, Signal::SIGCONT);
        let _ = (&self.env, &self.launcher);
    }
}

#[tokio::test]
async fn a_stop_of_a_frozen_worker_is_a_typed_error_and_retryable() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let frozen = Frozen::start().await;

    let error = guard(
        "a stop of a frozen worker",
        Box::pin(frozen.registry.stop(&frozen.id)),
    )
    .await
    .expect_err("a frozen worker cannot answer the stop");

    assert_eq!(error.code, "worker_operation_failed", "{error:?}");
    assert!(error.msg.contains(NO_ANSWER), "{error:?}");
    assert_eq!(
        frozen.state().await,
        SessionState::Running,
        "the session is still live and listed"
    );

    frozen.thaw();
    guard(
        "a retried stop after the worker thawed",
        Box::pin(frozen.registry.stop(&frozen.id)),
    )
    .await
    .expect("a retried stop succeeds once the worker answers");
    wait_until("the session to end stopped", || async {
        (frozen.state().await == SessionState::Stopped).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_remove_of_a_frozen_worker_is_a_typed_error_and_keeps_the_record() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let frozen = Frozen::start().await;

    let error = guard(
        "a removal of a frozen worker",
        Box::pin(frozen.registry.remove(&frozen.id)),
    )
    .await
    .expect_err("a frozen worker cannot answer the stop inside the removal");

    assert_eq!(error.code, "worker_operation_failed", "{error:?}");
    assert!(error.msg.contains(NO_ANSWER), "{error:?}");
    assert!(
        frozen.registry.inspect(&frozen.id).await.is_ok(),
        "the record is kept"
    );

    frozen.thaw();
    guard(
        "a retried removal after the worker thawed",
        Box::pin(frozen.registry.remove(&frozen.id)),
    )
    .await
    .expect("a retried removal succeeds once the worker answers");
    assert!(
        frozen.registry.inspect(&frozen.id).await.is_err(),
        "the removed session is gone"
    );
}

/// Lets spawned tasks run until they wait on something that needs time or the
/// frozen worker.
async fn settle() {
    for _ in 0..SETTLE_YIELDS {
        tokio::task::yield_now().await;
    }
}

/// Starts an input the session's root never reads, so the worker blocks
/// delivering it and the input holds the control connection until the stall
/// timeout fails it.
async fn hold_the_connection_with_input(
    frozen: &Frozen,
) -> tokio::task::JoinHandle<Result<protocol::SessionInputResult, protocol::ProtocolError>> {
    let registry = frozen.registry.clone();
    let id = frozen.id.clone();
    let input = tokio::spawn(async move {
        registry
            .input(SessionInputParams {
                session_id: id,
                text: "x".repeat(BLOCKING_INPUT_BYTES),
                wait: None,
            })
            .await
    });
    // The input holds the connection once the daemon counts it; settling by
    // yields alone could let a later stop reach the worker first.
    wait_until("the input to hold the control connection", || async {
        (frozen.registry.input_writes_in_flight(&frozen.id).await == 1).then_some(())
    })
    .await;
    input
}

/// Advances the paused clock by `step` until `task` finishes, handing the
/// scheduler to the blocking pool between steps so work the task still waits
/// for on other threads can complete.
async fn advance_until_finished<T>(what: &str, task: &tokio::task::JoinHandle<T>, step: Duration) {
    for _ in 0..ADVANCE_STEPS_LIMIT {
        if task.is_finished() {
            return;
        }
        tokio::time::advance(step).await;
        settle().await;
        tokio::task::spawn_blocking(std::thread::yield_now)
            .await
            .expect("the blocking pool yields");
    }
    panic!("{what} did not finish within {ADVANCE_STEPS_LIMIT} clock steps");
}

#[tokio::test]
async fn a_stop_behind_an_input_outlasts_the_exchange_wait_and_proceeds_once_it_ends() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let frozen = Frozen::start_running().await;
    let input = hold_the_connection_with_input(&frozen).await;
    let registry = frozen.registry.clone();
    let id = frozen.id.clone();
    let stopping = tokio::spawn(async move { registry.stop(&id).await });
    settle().await;

    // The exchange wait elapses twice on the virtual clock while the input
    // still holds the connection; the stop keeps waiting.
    tokio::time::pause();
    let inhibitor = AutoAdvanceInhibitor::new();
    for _ in 0..3 {
        tokio::time::advance(EXCHANGE_WAIT).await;
        settle().await;
        assert!(!stopping.is_finished(), "the stop waits behind the input");
    }

    // The input ends on the worker (acknowledged or failed by its stall
    // policy) and the stop runs.
    let _outcome = guard("the blocked input", input)
        .await
        .expect("input task joins");
    guard("the stop behind the input", stopping)
        .await
        .expect("stop task joins")
        .expect("the stop proceeds once the input ended");
    inhibitor.release().await;
}

#[tokio::test]
async fn a_worker_frozen_during_an_input_ends_the_stop_at_the_input_wait() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let frozen = Frozen::start_running().await;
    let input = hold_the_connection_with_input(&frozen).await;
    kill(frozen.worker, Signal::SIGSTOP).expect("freeze the worker mid-input");
    let registry = frozen.registry.clone();
    let id = frozen.id.clone();
    let stopping = tokio::spawn(async move { registry.stop(&id).await });
    settle().await;

    tokio::time::pause();
    let inhibitor = AutoAdvanceInhibitor::new();
    advance_until_finished("the stop behind a frozen input", &stopping, EXCHANGE_WAIT).await;
    let error = guard("the stop behind a frozen input", stopping)
        .await
        .expect("stop task joins")
        .expect_err("a frozen worker cannot finish the input the stop waits behind");

    assert_eq!(error.code, "worker_operation_failed", "{error:?}");
    assert!(error.msg.contains(NO_ANSWER), "{error:?}");
    assert!(error.msg.contains("input write"), "{error:?}");
    frozen.thaw();
    let _ = input.await;
    inhibitor.release().await;
}
