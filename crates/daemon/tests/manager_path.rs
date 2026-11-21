//! A daemon started before the service manager's `PATH` gains an agent
//! directory finds the agent without a restart.
//!
//! The manager is a fake whose `PATH` the test changes between requests; the
//! requests go through the daemon's own dispatcher, so the host inventory, the
//! daemon doctor and the agent launch all read the search path of their
//! registry.

// Rust guideline compliant 2026-11-21

mod support;

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pohunek_platform::supervisor::UnitEnvironment;
use protocol::{
    method, DaemonDoctorResult, HostCapabilities, Request, SessionNewParams, ENV_DAEMON_ID,
    ENV_SESSION_ID,
};
use serde_json::Value;

use pohunek_daemon::api::{handle_request, DaemonState, HealthInfo, OverlaySource};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::runtime::{
    FollowGate, ManagerEnvironment, ManagerFuture, ManagerPath, ManagerReadError,
    SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use pohunek_test_support::process_env::{in_own_process, ProcessEnv};
use pohunek_test_support::wait::{wait_until, HANG_GUARD};
use pohunek_test_support::worker_binary;

/// The scenario starts no worker, so the worker executable only has to be a
/// well-formed absolute path.
const WORKER_PLACEHOLDER: &str = "/nonexistent/pohunek-sessiond";

/// The agent program, a bare name, every row looks for.
const AGENT: &str = "claude";

/// The host profile that launches [`AGENT`].
const PROFILE: &str = "fixture";

/// Reuse window of the manager read: zero, so every request reads the manager.
const NO_REUSE: Duration = Duration::ZERO;

/// Reuse window longer than any scenario: a registry reuses its first read.
const LONG_REUSE: Duration = Duration::from_secs(3600);

/// Deadline of one manager read; only the manager that never answers reaches it.
const READ_DEADLINE: Duration = HANG_GUARD;

/// A store location a launched agent must keep as the process had it; the
/// manager interface carries `PATH` and nothing else.
const STORE_VARIABLE: &str = "CLAUDE_CONFIG_DIR";
const STORE_VALUE: &str = "/process/claude-config";

/// A daemon that follows a service manager's job and records no `search_path`.
const FOLLOWING: FollowGate = FollowGate {
    service_managed: true,
    search_path_recorded: false,
};

/// The status of the doctor's `bin:` check once the manager `PATH` holds the
/// agent. The Linux doctor probes the daemon's search path; the macOS doctor
/// probes the process environment, because a macOS daemon never follows a
/// manager.
const DOCTOR_BIN_FOLLOWS_MANAGER: protocol::DoctorStatus = if cfg!(target_os = "linux") {
    protocol::DoctorStatus::Ok
} else {
    protocol::DoctorStatus::Warn
};

/// What the fake manager answers next.
#[derive(Debug, Clone)]
enum Answer {
    Path(OsString),
    NoPath,
    Unreachable,
    /// Never answers; only the read deadline ends the wait.
    Hang,
}

#[derive(Debug, Clone)]
struct FakeManager(Arc<Mutex<Answer>>, Arc<Mutex<UnitEnvironment>>);

impl FakeManager {
    fn answering(answer: Answer) -> Self {
        Self(
            Arc::new(Mutex::new(answer)),
            Arc::new(Mutex::new(UnitEnvironment {
                load_state: "loaded".to_owned(),
                ..UnitEnvironment::default()
            })),
        )
    }

    fn answer(&self, answer: Answer) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = answer;
    }
}

impl ManagerEnvironment for FakeManager {
    fn unit_environment<'a>(&'a self, _unit: &'a str) -> ManagerFuture<'a, UnitEnvironment> {
        let unit = self
            .1
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(unit) })
    }

    fn search_path(&self) -> ManagerFuture<'_, Option<OsString>> {
        let answer = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Box::pin(async move {
            match answer {
                Answer::Path(path) => Ok(Some(path)),
                Answer::NoPath => Ok(None),
                Answer::Unreachable => Err(ManagerReadError::new("the fake manager is down")),
                Answer::Hang => std::future::pending().await,
            }
        })
    }
}

/// Writes an executable `claude` into `directory` that records the `PATH` and
/// the store variable it runs with into `record`.
fn install_agent(directory: &Path, record: &Path) {
    std::fs::create_dir_all(directory).expect("create the agent directory");
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .expect("make the agent directory owner-private");
    let program = directory.join(AGENT);
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n%s\\n' \"$PATH\" \"${STORE_VARIABLE}\" > '{}'\n",
            record.display()
        ),
    )
    .expect("write the agent");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
        .expect("make the agent executable");
}

/// How the registry under test starts agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launches {
    /// No agent starts; the base environment is a fixed one.
    Never,
    /// A real worker starts the agent on the daemon's process environment.
    Really,
}

async fn state(
    root: &Path,
    manager: &FakeManager,
    launches: Launches,
    max_age: Duration,
) -> DaemonState {
    state_configured(root, manager, launches, max_age, |_config| {}).await
}

/// [`state`] with `configure` applied to the registry configuration last.
async fn state_configured(
    root: &Path,
    manager: &FakeManager,
    launches: Launches,
    max_age: Duration,
    configure: impl FnOnce(&mut SessionRegistryConfig),
) -> DaemonState {
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
    for directory in [
        "worker/runtime/pohunek/workers",
        "worker/state/pohunek/workers",
        "worker/data/pohunek",
        "worker/config/pohunek",
        "worker/cache/pohunek",
    ] {
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
            .recursive(true)
            .create(root.join(directory))
            .expect("create the owner-private worker directory");
    }
    let agents = root.join("agents");
    std::fs::create_dir(&agents).expect("create the agents directory");
    std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700))
        .expect("make the agents directory owner-private");
    std::fs::write(
        agents.join(format!("{PROFILE}.toml")),
        format!("base = \"shell\"\nprogram = \"{AGENT}\"\n"),
    )
    .expect("write the profile");
    let manager_path = ManagerPath::new(Arc::new(manager.clone()), max_age, READ_DEADLINE);
    let supervision = match launches {
        Launches::Never => environment
            .supervision(PathBuf::from(WORKER_PLACEHOLDER))
            .with_environment_source(support::hermetic_environment_source()),
        Launches::Really => {
            let mut supervision = environment.supervision(worker_binary());
            supervision.environment_allowlist =
                ["PATH", "HOME", STORE_VARIABLE].map(str::to_owned).to_vec();
            supervision
        }
    };
    let mut config = SessionRegistryConfig {
        agents_dir: Some(agents),
        host_state_dir: Some(root.join("host-state")),
        socket_path: Some(socket),
        worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
        worker_state_root: Some(environment.state_home.join("pohunek/workers")),
        supervision: Some(supervision.with_manager_path(manager_path)),
        ..support::hermetic_registry_config()
    };
    configure(&mut config);
    let registry =
        SessionRegistry::new_production(config, Arc::new(SubprocessWorkerLauncher::new()))
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

/// The statuses of the daemon doctor's `bin:` check of the agent and of the
/// check of the profile that launches it.
async fn doctor_status(state: &DaemonState) -> [protocol::DoctorStatus; 2] {
    let doctor: DaemonDoctorResult = call(state, method::DAEMON_DOCTOR).await;
    let status = |name: String| {
        doctor
            .report
            .checks
            .iter()
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("the doctor reports {name}"))
            .status
    };
    [
        status(format!("bin:{AGENT}")),
        status(format!("agent_profile_{PROFILE}")),
    ]
}

/// A daemon started while the manager `PATH` has no agent directory.
///
/// The scenarios set the process `PATH`, so each runs in its own process.
struct Started {
    root: tempfile::TempDir,
    process_dir: PathBuf,
    manager_dir: PathBuf,
    manager: FakeManager,
    state: DaemonState,
    _process: ProcessEnv,
}

impl Started {
    async fn new() -> Self {
        Self::with(Launches::Never).await
    }

    async fn with(launches: Launches) -> Self {
        let root = pohunek_test_support::tempdir().expect("scratch directory");
        let process_dir = root.path().join("process-bin");
        let manager_dir = root.path().join("manager-bin");
        std::fs::create_dir_all(&process_dir).expect("process directory");
        std::fs::set_permissions(&process_dir, std::fs::Permissions::from_mode(0o700))
            .expect("owner-private process directory");
        install_agent(&manager_dir, &root.path().join("launched.txt"));
        let mut process = ProcessEnv::lock();
        process
            .set("PATH", &process_dir)
            .set(STORE_VARIABLE, STORE_VALUE)
            .remove(ENV_DAEMON_ID)
            .remove(ENV_SESSION_ID);
        let manager = FakeManager::answering(Answer::Path(process_dir.clone().into()));
        let state = state(root.path(), &manager, launches, NO_REUSE).await;
        Self {
            root,
            process_dir,
            manager_dir,
            manager,
            state,
            _process: process,
        }
    }

    /// Starts the agent through `session.new` and returns the `PATH` and the
    /// store variable the agent process recorded.
    async fn launched(&self) -> (Vec<PathBuf>, String) {
        let params = SessionNewParams {
            client_request_id: None,
            name: None,
            agent: PROFILE.to_owned(),
            cwd: Some(self.root.path().to_path_buf()),
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
            metadata: std::collections::BTreeMap::new(),
            secret_env: protocol::SessionSecretEnv::default(),
            context_package_digest: None,
            fetch_ref: None,
            agent_options: std::collections::BTreeMap::new(),
            skip_repo_hooks: false,
        };
        let request = Request::new(
            "manager-path",
            method::SESSION_NEW,
            serde_json::to_value(params).expect("session.new params"),
        )
        .expect("valid request");
        handle_request(&request, &self.state)
            .await
            .into_result()
            .unwrap_or_else(|error| panic!("session.new failed: {error}"));
        let record = self.root.path().join("launched.txt");
        let recorded = wait_until("the agent to record its environment", || async {
            let text = tokio::fs::read_to_string(&record).await.ok()?;
            (text.matches('\n').count() == 2).then_some(text)
        })
        .await;
        let (path, store) = recorded.split_once('\n').expect("two recorded lines");
        (
            std::env::split_paths(path).collect(),
            store.trim_end().to_owned(),
        )
    }

    /// The manager's `PATH` with the agent directory in front.
    fn with_agent(&self) -> OsString {
        std::env::join_paths([&self.manager_dir, &self.process_dir]).expect("join PATH")
    }

    /// The agent path `host.inspect` reports, which must be the manager's.
    async fn found(&self) -> String {
        let found = inspected(&self.state).await.expect("the agent is found");
        assert_eq!(Path::new(&found), self.manager_dir.join(AGENT));
        found
    }
}

/// R2: the daemon started before the manager `PATH` gained the agent
/// directory; the next request finds the agent, and the doctor agrees.
#[tokio::test]
async fn a_daemon_started_before_the_manager_path_gains_an_agent_dir_finds_it() {
    if !in_own_process() {
        return;
    }
    let started = Started::new().await;
    assert_eq!(inspected(&started.state).await, None);
    assert_eq!(
        doctor_status(&started.state).await,
        [protocol::DoctorStatus::Warn, protocol::DoctorStatus::Fail]
    );

    started.manager.answer(Answer::Path(started.with_agent()));

    started.found().await;
    assert_eq!(
        doctor_status(&started.state).await,
        [DOCTOR_BIN_FOLLOWS_MANAGER, protocol::DoctorStatus::Ok]
    );
}

/// R2 at the launch: `session.new` resolves the agent on the manager's `PATH`
/// and the agent runs with that `PATH`.
///
/// R7: only `PATH` follows the manager; the agent keeps the process's value of
/// every other variable.
#[tokio::test]
async fn a_session_started_after_the_manager_path_gains_an_agent_dir_runs_the_agent() {
    if !in_own_process() {
        return;
    }
    let started = Started::with(Launches::Really).await;
    started.manager.answer(Answer::Path(started.with_agent()));

    let (path, store) = started.launched().await;

    assert_eq!(
        path,
        std::env::split_paths(&started.with_agent()).collect::<Vec<_>>(),
        "the agent runs with the manager's PATH"
    );
    assert_eq!(
        store, STORE_VALUE,
        "the store variable keeps the process value"
    );
}

/// R3: the agent directory leaves the manager `PATH` again; the daemon
/// follows.
#[tokio::test]
async fn an_agent_dir_the_manager_path_loses_is_missing_again() {
    if !in_own_process() {
        return;
    }
    let started = Started::new().await;
    started.manager.answer(Answer::Path(started.with_agent()));
    started.found().await;

    started
        .manager
        .answer(Answer::Path(started.process_dir.clone().into()));

    assert_eq!(inspected(&started.state).await, None);
}

/// Two registries of one process follow their own managers: each resolves
/// agents on the `PATH` of its manager, whatever the other registry read in
/// between. The long reuse window makes the second request of a registry reuse
/// its earlier read, so the answer comes from the registry's own state.
#[tokio::test]
async fn two_registries_in_one_process_resolve_on_their_own_manager_path() {
    if !in_own_process() {
        return;
    }
    let process_dir_root = pohunek_test_support::tempdir().expect("scratch directory");
    let process_dir = process_dir_root.path().join("process-bin");
    std::fs::create_dir_all(&process_dir).expect("process directory");
    std::fs::set_permissions(&process_dir, std::fs::Permissions::from_mode(0o700))
        .expect("owner-private process directory");
    let mut process = ProcessEnv::lock();
    process
        .set("PATH", &process_dir)
        .remove(ENV_DAEMON_ID)
        .remove(ENV_SESSION_ID);

    let mut daemons = Vec::new();
    for name in ["first", "second"] {
        let root = pohunek_test_support::tempdir().expect("scratch directory");
        let manager_dir = root.path().join("manager-bin");
        install_agent(&manager_dir, &root.path().join("launched.txt"));
        let manager_path = std::env::join_paths([&manager_dir, &process_dir]).expect("join PATH");
        let manager = FakeManager::answering(Answer::Path(manager_path));
        let state = state(root.path(), &manager, Launches::Never, LONG_REUSE).await;
        daemons.push((name, root, manager_dir, state));
    }

    for index in [0, 1, 0, 1, 0] {
        let (name, _root, manager_dir, state) = &daemons[index];
        let found = inspected(state)
            .await
            .unwrap_or_else(|| panic!("the {name} registry finds its agent"));
        assert_eq!(
            Path::new(&found),
            manager_dir.join(AGENT),
            "the {name} registry resolves on its own manager's PATH"
        );
    }
}

/// Writes an executable `git` into `directory` and makes the directory
/// owner-private, so the trusted-executable policy accepts it.
fn install_git(directory: &Path) {
    std::fs::create_dir_all(directory).expect("create the git directory");
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .expect("make the git directory owner-private");
    let program = directory.join("git");
    std::fs::write(&program, "#!/bin/sh\nexit 0\n").expect("write git");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
        .expect("make git executable");
}

/// `git_available` of the flags-only `host.inspect` answer.
async fn git_available(state: &DaemonState) -> bool {
    let request = Request::new(
        "manager-path",
        method::HOST_INSPECT,
        serde_json::json!({ "skip_runtime_probes": true }),
    )
    .expect("valid request");
    let value = handle_request(&request, state)
        .await
        .into_result()
        .unwrap_or_else(|error| panic!("host.inspect failed: {error}"));
    serde_json::from_value::<HostCapabilities>(value)
        .expect("host.inspect result")
        .git_available
}

/// C1: each registry judges `git` on its own manager's `PATH`. Registry A's
/// manager holds a `git`; B's manager and the process `PATH` hold none. The long
/// reuse window makes A's later requests reuse its earlier read, so only a
/// state shared between the registries could turn the answer.
#[tokio::test]
async fn two_registries_judge_git_on_their_own_manager_path() {
    if !in_own_process() {
        return;
    }
    let scratch = pohunek_test_support::tempdir().expect("scratch directory");
    let process_dir = scratch.path().join("process-bin");
    std::fs::create_dir_all(&process_dir).expect("process directory");
    std::fs::set_permissions(&process_dir, std::fs::Permissions::from_mode(0o700))
        .expect("owner-private process directory");
    let mut process = ProcessEnv::lock();
    process
        .set("PATH", &process_dir)
        .remove(ENV_DAEMON_ID)
        .remove(ENV_SESSION_ID);

    let mut daemons = Vec::new();
    for (name, has_git) in [("first", true), ("second", false)] {
        let root = pohunek_test_support::tempdir().expect("scratch directory");
        let manager_dir = root.path().join("manager-bin");
        std::fs::create_dir_all(&manager_dir).expect("manager directory");
        if has_git {
            install_git(&manager_dir);
        }
        let manager_path = std::env::join_paths([&manager_dir, &process_dir]).expect("join PATH");
        let manager = FakeManager::answering(Answer::Path(manager_path));
        let state = state(root.path(), &manager, Launches::Never, LONG_REUSE).await;
        daemons.push((name, has_git, root, state));
    }

    for index in [0, 1, 0, 1, 0] {
        let (name, has_git, _root, state) = &daemons[index];
        assert_eq!(
            git_available(state).await,
            *has_git,
            "the {name} registry judges git on its own manager's PATH"
        );
    }
}

/// The first directory of the process `PATH` that holds `program`.
fn directory_of(program: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("the test runs with a PATH"))
        .find(|directory| directory.join(program).is_file())
        .unwrap_or_else(|| panic!("{program} is on the process PATH"))
}

/// A repository with one commit on `main`, made before the scenario replaces
/// the process `PATH`.
fn repository() -> pohunek_test_support::env::TestEnv {
    let env = pohunek_test_support::env::TestEnv::new().expect("repository environment");
    let git = |args: &[&str]| {
        let output = env
            .git_command()
            .args(args)
            .output()
            .expect("run git in the repository environment");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["-c", "init.defaultBranch=main", "init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(env.cwd().join("README.md"), "init\n").expect("write README");
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    env
}

/// C2: the `post-create` hook of a worktree session runs with the `PATH` of the
/// registry that created it, whatever another registry of the process read
/// since. Both managers carry the `git` and `sh` the create needs and differ in
/// one more directory, so a hook that took the other registry's value records a
/// different `PATH`.
#[tokio::test]
async fn a_worktree_hook_runs_on_its_own_registrys_manager_path() {
    if !in_own_process() {
        return;
    }
    let repo = repository();
    let tools = [directory_of("git"), directory_of("sh")];
    let scratch = pohunek_test_support::tempdir().expect("scratch directory");
    let process_dir = scratch.path().join("process-bin");
    std::fs::create_dir_all(&process_dir).expect("process directory");
    std::fs::set_permissions(&process_dir, std::fs::Permissions::from_mode(0o700))
        .expect("owner-private process directory");
    let mut process = ProcessEnv::lock();
    process
        .set("PATH", &process_dir)
        .remove(ENV_DAEMON_ID)
        .remove(ENV_SESSION_ID);

    let mut daemons = Vec::new();
    for name in ["first", "second"] {
        let root = pohunek_test_support::tempdir().expect("scratch directory");
        let manager_dir = root.path().join("manager-bin");
        install_agent(&manager_dir, &root.path().join("launched.txt"));
        let manager_path = std::env::join_paths(
            std::iter::once(manager_dir.clone())
                .chain(tools.iter().cloned())
                .chain([process_dir.clone()]),
        )
        .expect("join PATH");
        let manager = FakeManager::answering(Answer::Path(manager_path.clone()));
        let config_dir = root.path().join("config");
        std::fs::create_dir_all(config_dir.join("hooks")).expect("host hook directory");
        let recorded = root.path().join("hook-path.txt");
        let hook = config_dir.join("hooks/post-create");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf '%s' \"$PATH\" > '{}'\n",
                recorded.display()
            ),
        )
        .expect("write the hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
            .expect("make the hook executable");
        let worktrees = root.path().join("worktrees");
        let store = root.path().join("metadata.jsonl");
        let state = state_configured(
            root.path(),
            &manager,
            Launches::Really,
            LONG_REUSE,
            |config| {
                config.config_dir = Some(config_dir);
                config.worktree_root = Some(worktrees);
                config.store_path = Some(store);
            },
        )
        .await;
        daemons.push((name, manager_path, recorded, root, state));
    }

    // Each registry reads its manager once; the second read is the one the
    // first registry's create must not see.
    for (_name, _path, _recorded, _root, state) in &daemons {
        git_available(state).await;
    }
    let (name, manager_path, recorded, _root, state) = &daemons[0];
    let params = SessionNewParams {
        client_request_id: None,
        name: None,
        agent: PROFILE.to_owned(),
        cwd: None,
        cols: 80,
        rows: 24,
        project: None,
        repo: Some(repo.cwd().to_path_buf()),
        branch: Some("c2".to_owned()),
        base_branch: None,
        input: None,
        extended_input_ready_wait: None,
        branch_source: None,
        on_diverged: None,
        input_ack_timeout_ms: None,
        metadata: std::collections::BTreeMap::new(),
        secret_env: protocol::SessionSecretEnv::default(),
        context_package_digest: None,
        fetch_ref: None,
        agent_options: std::collections::BTreeMap::new(),
        skip_repo_hooks: false,
    };
    let request = Request::new(
        "manager-path",
        method::SESSION_NEW,
        serde_json::to_value(params).expect("session.new params"),
    )
    .expect("valid request");
    handle_request(&request, state)
        .await
        .into_result()
        .unwrap_or_else(|error| panic!("session.new on the {name} registry failed: {error}"));

    let hook_path = std::fs::read_to_string(recorded).expect("the hook ran and recorded its PATH");
    assert_eq!(
        OsString::from(hook_path),
        *manager_path,
        "the {name} registry's hook runs on its own manager's PATH"
    );
}

/// R4: a manager that cannot answer, or that has no `PATH`, leaves the `PATH`
/// the daemon process started with; the process environment itself is never
/// changed.
#[tokio::test]
async fn a_manager_that_does_not_answer_leaves_the_process_path() {
    if !in_own_process() {
        return;
    }
    let started = Started::new().await;
    install_git(&started.manager_dir);
    for silent in [Answer::Unreachable, Answer::NoPath] {
        started.manager.answer(Answer::Path(started.with_agent()));
        started.found().await;
        assert!(git_available(&started.state).await);

        started.manager.answer(silent);

        assert_eq!(inspected(&started.state).await, None);
        assert!(!git_available(&started.state).await);
    }
    assert_eq!(
        std::env::var_os("PATH").as_deref(),
        Some(OsStr::new(&started.process_dir))
    );
}

/// R4: a manager that never answers is given up on at the read deadline and
/// the daemon returns to its process `PATH`.
#[tokio::test(start_paused = true)]
async fn a_manager_that_never_answers_leaves_the_process_path() {
    if !in_own_process() {
        return;
    }
    let started = Started::new().await;
    started.manager.answer(Answer::Path(started.with_agent()));
    started.found().await;

    started.manager.answer(Answer::Hang);

    assert_eq!(inspected(&started.state).await, None);
}

/// A unit whose manager-side settings are `unit`.
fn loaded() -> UnitEnvironment {
    UnitEnvironment {
        load_state: "loaded".to_owned(),
        ..UnitEnvironment::default()
    }
}

/// Whether a daemon with `gate` whose unit holds `unit` follows the manager.
async fn follows(gate: FollowGate, unit: UnitEnvironment) -> bool {
    let manager = FakeManager::answering(Answer::NoPath);
    *manager.1.lock().expect("unit settings") = unit;
    ManagerPath::follow(
        Arc::new(manager),
        "unit.service",
        gate,
        NO_REUSE,
        READ_DEADLINE,
    )
    .await
    .is_some()
}

/// R5: a daemon follows the manager only when it runs under a service
/// manager and records no `search_path`.
#[tokio::test]
async fn a_daemon_that_is_not_a_service_job_or_records_a_search_path_is_not_followed() {
    assert!(follows(FOLLOWING, loaded()).await, "a following daemon");
    assert!(
        !follows(
            FollowGate {
                service_managed: false,
                ..FOLLOWING
            },
            loaded()
        )
        .await,
        "the daemon is not a service manager's job"
    );
    assert!(
        !follows(
            FollowGate {
                search_path_recorded: true,
                ..FOLLOWING
            },
            loaded()
        )
        .await,
        "the service configuration records a search_path"
    );
}

/// R5: the manager is followed only when the daemon's unit cannot decide its
/// own `PATH`.
#[tokio::test]
async fn a_unit_that_decides_its_own_path_is_not_followed() {
    let follow = |unit: UnitEnvironment| follows(FOLLOWING, unit);

    assert!(follow(loaded()).await, "an inheriting unit");
    assert!(
        follow(UnitEnvironment {
            environment: vec!["PATHEXT=/x".to_owned(), "CLAUDE_CONFIG_DIR=/c".to_owned()],
            ..loaded()
        })
        .await,
        "other variables do not matter"
    );
    let deciding = [
        (
            "Environment=PATH=",
            UnitEnvironment {
                environment: vec!["PATH=/opt/bin".to_owned()],
                ..loaded()
            },
        ),
        (
            "ExecSearchPath=",
            UnitEnvironment {
                exec_search_path: vec!["/opt/bin".to_owned()],
                ..loaded()
            },
        ),
        (
            "EnvironmentFile=",
            UnitEnvironment {
                environment_files: vec![("/etc/pohunek.env".to_owned(), true)],
                ..loaded()
            },
        ),
        (
            "PassEnvironment=PATH",
            UnitEnvironment {
                pass_environment: vec!["PATH".to_owned()],
                ..loaded()
            },
        ),
        (
            "UnsetEnvironment=PATH",
            UnitEnvironment {
                unset_environment: vec!["PATH".to_owned()],
                ..loaded()
            },
        ),
        (
            "stale until a daemon-reload",
            UnitEnvironment {
                need_daemon_reload: true,
                ..loaded()
            },
        ),
        ("not loaded", UnitEnvironment::default()),
    ];
    for (setting, unit) in deciding {
        assert!(!follow(unit).await, "{setting}");
    }
}
