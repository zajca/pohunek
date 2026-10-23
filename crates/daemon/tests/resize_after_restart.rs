//! `session.resize` over a worker that outlives the daemon.
//!
//! The worker orders resizes per source and keeps that ordering in memory for
//! its whole life, while each daemon numbers its resizes from the start. A
//! restarted daemon's resize must therefore be applied by the adopted worker
//! whatever the earlier daemon sent.

// Rust guideline compliant 2026-10-23

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use pohunek_daemon::procwatch::HostInspector;
use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::worker_binary;
use protocol::{RuntimeState, SessionInfo, SessionNewParams, ENV_DAEMON_ID, ENV_SESSION_ID};

/// Resizes the first daemon sends. The worker keeps the highest sequence it
/// accepted, so several are needed to outrun the counter of a restarted
/// daemon, which starts again at the beginning.
const RESIZES_BEFORE_RESTART: [(u16, u16); 3] = [(100, 30), (101, 31), (102, 32)];

/// The size the restarted daemon asks for.
const RESIZE_AFTER_RESTART: (u16, u16) = (140, 50);

/// Terminal size the session starts with.
const INITIAL_SIZE: (u16, u16) = (80, 24);

/// File the child writes its terminal size to, below the test root.
const SIZE_LOG: &str = "size.log";

/// File name of the scenario's daemon store, below its state home.
const STORE_FILE: &str = "metadata.jsonl";

/// Writes the terminal size to `$1` on every window-size change and otherwise
/// waits, so the child observes what the PTY was resized to.
const RECORD_SIZE_SHELL: &str = "trap 'stty size > \"$1\"' WINCH; sleep 600 & s=$!; while kill -0 \"$s\" 2>/dev/null; do wait \"$s\"; done";

/// A daemon instance over the scenario's state and worker roots. Its workers
/// belong to `launcher`, so they outlive the instance while the launcher is
/// kept.
fn registry(
    env: &TestEnv,
    size_log: &Path,
    launcher: Arc<SubprocessWorkerLauncher>,
) -> SessionRegistry {
    let worker_environment = SubprocessWorkerEnvironment {
        runtime_home: env.runtime_dir().to_path_buf(),
        state_home: env.state_home().to_path_buf(),
        data_home: env.data_home().to_path_buf(),
        config_home: env.config_home().to_path_buf(),
        cache_home: env.cache_home().to_path_buf(),
        home: env.home().to_path_buf(),
        daemon_socket: env.root().join("daemon.sock"),
    };
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            [
                "-c",
                RECORD_SIZE_SHELL,
                "record-size",
                size_log.to_str().expect("UTF-8 size log path"),
            ],
        ),
        store_path: Some(env.state_home().join(STORE_FILE)),
        worker_runtime_root: Some(worker_environment.runtime_home.join("pohunek/workers")),
        worker_state_root: Some(worker_environment.state_home.join("pohunek/workers")),
        supervision: Some(
            worker_environment
                .supervision(worker_binary())
                .with_environment_source(EnvironmentSource::fixed(env.environment().clone())),
        ),
        ..SessionRegistryConfig::default()
    };
    SessionRegistry::new_with_launcher_and_inspector(
        config,
        launcher,
        Arc::new(HostInspector::new()),
    )
}

/// The size the child last recorded, as `(cols, rows)`.
fn recorded_size(path: &Path) -> Option<(u16, u16)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut fields = text.split_whitespace();
    let rows = fields.next()?.parse().ok()?;
    let cols = fields.next()?.parse().ok()?;
    Some((cols, rows))
}

fn worker_instance_id(session: &SessionInfo) -> Option<String> {
    session
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.worker_instance_id.clone())
}

/// Runs a daemon that creates the session, resizes it
/// [`RESIZES_BEFORE_RESTART`] times and shuts down leaving the worker alive.
///
/// The daemon runs on a runtime of its own that ends with it, which closes its
/// controller connection and releases the lease as a daemon process exit does.
fn first_daemon(
    env: &TestEnv,
    size_log: &Path,
    launcher: &Arc<SubprocessWorkerLauncher>,
) -> (SessionInfo, Option<String>) {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("first daemon runtime");
                runtime.block_on(async {
                    let first = registry(env, size_log, Arc::clone(launcher));
                    let created = create_session(&first, env).await;
                    for (cols, rows) in RESIZES_BEFORE_RESTART {
                        let resized = first
                            .resize(&created.id, cols, rows)
                            .await
                            .expect("the first daemon resizes the session");
                        assert_eq!(
                            (resized.session.cols, resized.session.rows),
                            (cols, rows),
                            "the first daemon's resize is applied"
                        );
                    }
                    let instance = worker_instance_id(
                        &first
                            .inspect(&created.id)
                            .await
                            .expect("inspect the session"),
                    );
                    first.begin_daemon_shutdown();
                    (created, instance)
                })
            })
            .join()
            .expect("first daemon thread")
    })
}

async fn create_session(registry: &SessionRegistry, env: &TestEnv) -> SessionInfo {
    // A test run inside a pohunek session must not stamp the worker with that
    // session's markers.
    let mut process_env = ProcessEnv::lock();
    process_env.remove(ENV_DAEMON_ID).remove(ENV_SESSION_ID);
    registry
        .create(SessionNewParams {
            name: Some("resize-restart".to_owned()),
            agent: "shell".to_owned(),
            cwd: Some(env.cwd().to_path_buf()),
            cols: INITIAL_SIZE.0,
            rows: INITIAL_SIZE.1,
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
}

#[tokio::test]
async fn a_resize_after_a_daemon_restart_reaches_the_surviving_worker() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let env = TestEnv::new().expect("create the test environment");
    let size_log = env.root().join(SIZE_LOG);
    let launcher = Arc::new(SubprocessWorkerLauncher::new());

    let (created, first_instance) = first_daemon(&env, &size_log, &launcher);

    let restarted = registry(&env, &size_log, launcher);
    restarted
        .reconcile_workers()
        .await
        .expect("the restarted daemon adopts the worker");
    let adopted = restarted
        .inspect(&created.id)
        .await
        .expect("inspect the adopted session");
    assert_eq!(
        adopted.runtime.as_ref().map(|runtime| runtime.state),
        Some(RuntimeState::Live),
        "the restarted daemon adopts the live worker"
    );
    assert!(
        first_instance.is_some() && worker_instance_id(&adopted) == first_instance,
        "the adopted worker is the one the first daemon started"
    );

    let (cols, rows) = RESIZE_AFTER_RESTART;
    let resized = restarted
        .resize(&created.id, cols, rows)
        .await
        .expect("the restarted daemon resizes the adopted session");
    assert_eq!(
        (resized.session.cols, resized.session.rows),
        (cols, rows),
        "the resize result reports the requested size"
    );
    let inspected = restarted
        .inspect(&created.id)
        .await
        .expect("inspect the resized session");
    assert_eq!(
        (inspected.cols, inspected.rows),
        (cols, rows),
        "the session record reports the requested size"
    );
    wait_until("the child to observe the new size", || async {
        (recorded_size(&size_log) == Some((cols, rows))).then_some(())
    })
    .await;
    restarted
        .stop(&created.id)
        .await
        .expect("stop the adopted session");
}
