//! A daemon started before the service manager's `PATH` gains an agent
//! directory finds the agent without a restart.
//!
//! The manager is a fake whose `PATH` the test changes between requests; the
//! requests go through the daemon's own dispatcher, so the host inventory, the
//! daemon doctor and the agent launch all read the one search path.

// Rust guideline compliant 2026-11-21

mod support;

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use protocol::{DaemonDoctorResult, HostCapabilities, Request, method};
use serde_json::Value;

use pohunek_daemon::api::{handle_request, DaemonState, HealthInfo, OverlaySource};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::runtime::{
    ManagerEnvironment, ManagerFuture, ManagerPath, ManagerReadError, SubprocessWorkerEnvironment,
    SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use pohunek_test_support::process_env::{in_own_process, ProcessEnv};

/// The scenario starts no worker, so the worker executable only has to be a
/// well-formed absolute path.
const WORKER_PLACEHOLDER: &str = "/nonexistent/pohunek-sessiond";

/// The agent program, a bare name, every row looks for.
const AGENT: &str = "fixture-agent";

/// The host profile that launches [`AGENT`].
const PROFILE: &str = "fixture";

/// Reuse window of the manager read: zero, so every request reads the manager.
const NO_REUSE: Duration = Duration::ZERO;

/// What the fake manager answers next.
#[derive(Debug, Clone)]
enum Answer {
    Path(OsString),
    NoPath,
    Unreachable,
}

#[derive(Debug, Clone)]
struct FakeManager(Arc<Mutex<Answer>>);

impl FakeManager {
    fn answer(&self, answer: Answer) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = answer;
    }
}

impl ManagerEnvironment for FakeManager {
    fn search_path(&self) -> ManagerFuture<'_, Option<OsString>> {
        let answer = self.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
        Box::pin(async move {
            match answer {
                Answer::Path(path) => Ok(Some(path)),
                Answer::NoPath => Ok(None),
                Answer::Unreachable => Err(ManagerReadError::new("the fake manager is down")),
            }
        })
    }
}

/// Writes an executable `claude` into `directory`.
fn install_agent(directory: &Path) {
    std::fs::create_dir_all(directory).expect("create the agent directory");
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .expect("make the agent directory owner-private");
    let program = directory.join(AGENT);
    std::fs::write(&program, "#!/bin/sh\nexit 0\n").expect("write the agent");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
        .expect("make the agent executable");
}

async fn state(root: &Path, manager: &FakeManager) -> DaemonState {
    let socket = root.join("daemon.sock");
    let environment = SubprocessWorkerEnvironment {
        runtime_home: root.join("worker/runtime"),
        state_home: root.join("worker/state"),
        data_home: root.join("worker/data"),
        config_home: root.join("worker/config"),
        cache_home: root.join("worker/cache"),
        home: root.join("worker"),
        daemon_socket: socket.clone(),
    };
    let agents = root.join("agents");
    std::fs::create_dir(&agents).expect("create the agents directory");
    std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700))
        .expect("make the agents directory owner-private");
    std::fs::write(
        agents.join(format!("{PROFILE}.toml")),
        format!("base = \"shell\"\nprogram = \"{AGENT}\"\n"),
    )
    .expect("write the profile");
    let config = SessionRegistryConfig {
        agents_dir: Some(agents),
        socket_path: Some(socket),
        worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
        worker_state_root: Some(environment.state_home.join("pohunek/workers")),
        supervision: Some(
            environment
                .supervision(PathBuf::from(WORKER_PLACEHOLDER))
                .with_environment_source(support::hermetic_environment_source())
                .with_manager_path(ManagerPath::new(Arc::new(manager.clone()), NO_REUSE)),
        ),
        ..support::hermetic_registry_config()
    };
    let registry = SessionRegistry::new_production(config, Arc::new(SubprocessWorkerLauncher::new()))
        .expect("the production registry opens");
    let governance = Arc::new(
        HostGovernanceService::open(root.join("state"))
            .await
            .expect("open the host-governance service"),
    );
    DaemonState::new(
        HealthInfo::new("0.0.0-test"),
        registry,
        governance,
        support::overlay_registry(),
        OverlaySource::Default,
    )
}

async fn call<T: serde::de::DeserializeOwned>(state: &DaemonState, method: &str) -> T {
    let request = Request::new("manager-path", method, Value::Null).expect("valid request");
    let value = handle_request(&request, state)
        .await
        .into_result()
        .unwrap_or_else(|error| panic!("{method} failed: {error}"));
    serde_json::from_value(value).unwrap_or_else(|error| panic!("{method} result: {error}"))
}

/// The path `host.inspect` reports for the agent, `None` when it is missing.
async fn inspected(state: &DaemonState) -> Option<String> {
    let capabilities: HostCapabilities = call(state, method::HOST_INSPECT).await;
    let runtime = capabilities
        .runtimes
        .into_iter()
        .find(|runtime| runtime.agent == PROFILE)
        .expect("the agent is inventoried");
    assert_eq!(runtime.available, runtime.path.is_some(), "{runtime:?}");
    runtime.path
}

/// The status of the daemon doctor's `bin:` check of the agent.
async fn doctor_status(state: &DaemonState) -> protocol::DoctorStatus {
    let doctor: DaemonDoctorResult = call(state, method::DAEMON_DOCTOR).await;
    doctor
        .report
        .checks
        .into_iter()
        .find(|check| check.name == format!("agent_profile_{PROFILE}"))
        .expect("the doctor checks the agent")
        .status
}

/// One scenario, in order, because the search path is process-wide state.
#[tokio::test]
async fn a_daemon_started_before_the_manager_path_gains_an_agent_dir_finds_it() {
    if !in_own_process() {
        return;
    }
    let root = pohunek_test_support::tempdir().expect("scratch directory");
    let process_dir = root.path().join("process-bin");
    let manager_dir = root.path().join("manager-bin");
    std::fs::create_dir_all(&process_dir).expect("process directory");
    std::fs::set_permissions(&process_dir, std::fs::Permissions::from_mode(0o700))
        .expect("owner-private process directory");
    install_agent(&manager_dir);
    let mut process = ProcessEnv::lock();
    process.set("PATH", &process_dir);

    let manager = FakeManager(Arc::new(Mutex::new(Answer::Path(process_dir.clone().into()))));
    let state = state(root.path(), &manager).await;

    // R2: the daemon started without the agent directory.
    assert_eq!(inspected(&state).await, None);
    assert_eq!(doctor_status(&state).await, protocol::DoctorStatus::Fail);

    // The desktop session imports its environment: the next request sees it.
    let with_agent = std::env::join_paths([&manager_dir, &process_dir]).expect("join PATH");
    manager.answer(Answer::Path(with_agent));
    let found = inspected(&state).await.expect("the agent is found");
    assert_eq!(Path::new(&found), manager_dir.join(AGENT));
    assert_eq!(doctor_status(&state).await, protocol::DoctorStatus::Ok);

    // R3: the directory leaves the manager's PATH again.
    manager.answer(Answer::Path(process_dir.clone().into()));
    assert_eq!(inspected(&state).await, None);

    // R4: an unreachable manager, or one without a PATH, leaves the process
    // PATH, which here has no agent.
    manager.answer(Answer::Path(
        std::env::join_paths([&manager_dir]).expect("join PATH"),
    ));
    assert!(inspected(&state).await.is_some());
    manager.answer(Answer::Unreachable);
    assert_eq!(inspected(&state).await, None);
    manager.answer(Answer::Path(
        std::env::join_paths([&manager_dir]).expect("join PATH"),
    ));
    assert!(inspected(&state).await.is_some());
    manager.answer(Answer::NoPath);
    assert_eq!(inspected(&state).await, None);

    // The process PATH itself stays what the process started with.
    assert_eq!(
        std::env::var_os("PATH").as_deref(),
        Some(OsStr::new(&process_dir))
    );
}
