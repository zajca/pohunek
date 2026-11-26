//! A real session whose shell writes fixture bytes to the terminal, read back
//! as the serialized `session.screen` result a client sees.

// Rust guideline compliant 2026-11-26

use std::collections::BTreeMap;
use std::sync::Arc;

use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait::{guard, wait_until};
use pohunek_test_support::worker_binary;
use protocol::{SessionNewParams, ENV_DAEMON_ID, ENV_SESSION_ID};
use serde_json::Value;

#[path = "../../src/procwatch/readable_host.rs"]
mod readable_host;

/// Line the shell prints after the fixture; the screen is read once it shows.
const SENTINEL: &str = "READY-LINKS";

/// Seconds the session's shell stays alive after printing.
const SHELL_LINGER_SECONDS: u32 = 60;

/// Terminal size of a scenario that needs no particular one.
const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// What a scenario varies: terminal size and registry configuration.
pub(crate) struct Setup {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) tweak: Box<dyn Fn(&mut SessionRegistryConfig)>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            cols: DEFAULT_SIZE.0,
            rows: DEFAULT_SIZE.1,
            tweak: Box::new(|_| {}),
        }
    }
}

/// Runs `body` as the session's shell script and returns the `session.screen`
/// result as a client decodes it, taken once the sentinel is on screen.
pub(crate) async fn screen_of_script(body: &str, setup: Setup) -> Value {
    let env = TestEnv::new().expect("create the test environment");
    let mut config = SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            [
                "-c",
                &format!("{body}; printf {SENTINEL}; exec sleep {SHELL_LINGER_SECONDS}"),
            ],
        ),
        ..SessionRegistryConfig::default()
    };
    (setup.tweak)(&mut config);
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
    config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
    config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
    config.supervision = Some(
        worker_environment
            .supervision(worker_binary())
            .with_environment_source(EnvironmentSource::fixed(env.environment().clone())),
    );
    let registry = SessionRegistry::new_with_launcher_and_inspector(
        config,
        launcher,
        Arc::new(readable_host::ReadableHost::new()),
    );
    let created = {
        // A test run inside a pohunek session must not stamp the worker with
        // that session's markers.
        let mut process_env = ProcessEnv::lock();
        process_env.remove(ENV_DAEMON_ID).remove(ENV_SESSION_ID);
        registry
            .create(SessionNewParams {
                client_request_id: None,
                name: Some("screen-links".to_owned()),
                agent: "shell".to_owned(),
                cwd: Some(env.cwd().to_path_buf()),
                cols: setup.cols,
                rows: setup.rows,
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
                secret_env: protocol::SessionSecretEnv::default(),
                context_package_digest: None,
                fetch_ref: None,
                agent_options: BTreeMap::new(),
                skip_repo_hooks: false,
            })
            .await
            .expect("create the session")
    };
    let screen = wait_until("the sentinel on the session screen", || async {
        let screen = registry.screen(&created.id).await.ok()?;
        let value = serde_json::to_value(&screen).expect("serialize the screen");
        value["visible_lines"]
            .as_array()?
            .iter()
            .any(|line| line.as_str().is_some_and(|line| line.contains(SENTINEL)))
            .then_some(value)
    })
    .await;
    guard("stopping the session", Box::pin(registry.stop(&created.id)))
        .await
        .expect("stop the session");
    screen
}

/// Writes `bytes` to the terminal through `cat`, then reads the screen.
pub(crate) async fn screen_of(bytes: &[u8], setup: Setup) -> Value {
    let scratch = TestEnv::new().expect("scratch directory for the fixture");
    let fixture = scratch.root().join("fixture.bin");
    std::fs::write(&fixture, bytes).expect("write the fixture");
    screen_of_script(&format!("cat '{}'", fixture.display()), setup).await
}

/// The `links` array of a screen; empty when the field is absent.
pub(crate) fn links(screen: &Value) -> Vec<Value> {
    screen
        .get("links")
        .map(|links| links.as_array().expect("links is an array").clone())
        .unwrap_or_default()
}
