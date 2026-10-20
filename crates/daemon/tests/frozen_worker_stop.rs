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
use pohunek_test_support::wait::{guard, wait_until};
use pohunek_test_support::worker_binary;
use protocol::{SessionId, SessionNewParams, SessionState, ENV_DAEMON_ID, ENV_SESSION_ID};

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
            launcher.clone(),
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
        kill(worker, Signal::SIGSTOP).expect("freeze the worker");
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

    let error = guard("a stop of a frozen worker", frozen.registry.stop(&frozen.id))
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
        frozen.registry.stop(&frozen.id),
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
        frozen.registry.remove(&frozen.id),
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
        frozen.registry.remove(&frozen.id),
    )
    .await
    .expect("a retried removal succeeds once the worker answers");
    assert!(
        frozen.registry.inspect(&frozen.id).await.is_err(),
        "the removed session is gone"
    );
}
