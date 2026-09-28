//! Real-backend fixture for the supervised lifecycle suite.
//!
//! One [`Installation`] is an isolated pohunek installation below a short,
//! canonical temporary root: its own XDG tree, a versioned binary layout under
//! `<root>/prefix/libexec/pohunek/<version>/`, an explicit `service.toml`, and
//! therefore its own installation namespace. The daemon under test runs as the
//! target's native login job (a systemd user unit or a launchd agent) through
//! the platform `DaemonSupervisor`, exactly as `pohunek service install`
//! registers it, except that the definition directory is private to the test.
//! Workers are the daemon's own per-generation native jobs.
//!
//! Dropping the installation retires every job of its namespace, removes the
//! daemon definition, and kills the probe processes the agent command left.

// Rust guideline compliant 2026-09-24

#[cfg_attr(target_os = "linux", path = "linux.rs")]
#[cfg_attr(target_os = "macos", path = "macos.rs")]
pub(crate) mod backend;

use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pohunek_client::{Client, ClientError, ClientOptions};
use pohunek_daemon::agent::{ForkMode, InputRules, ResumeMode, SessionRefKind};
use pohunek_daemon::store::{
    DesiredState, ResumeBinding, RuntimeRecord, SessionRecord, Store, StoredInputRules,
};
use pohunek_paths::{BasePaths, InstallLayout, PathEnv, Platform};
use pohunek_platform::process::{HostInspector, ProcessIdentity, ProcessInspector as _};
use pohunek_platform::supervisor::{
    DaemonSupervisor, Error as SupervisorError, JobDefinition, JobSpec, Namespace, RestartPolicy,
    ServiceId, ServiceObservation, Supervisor, WorkerKey,
};
use pohunek_service_config::{ConfigSpec, Deadlines, ServiceConfig};
use pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST;
use protocol::{
    method, AgentKind, AttachHeader, CwdSource, RuntimeInventoryResult, RuntimeState,
    SessionAttachParams, SessionCapabilities, SessionId, SessionInfo, SessionNewParams,
    SessionRuntime, SessionState, StateSource,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;

/// Agent profile every test session launches.
pub(crate) const AGENT: &str = "lifecycle";

/// Deadline for any single condition a test waits for.
///
/// Covers a debug-built daemon starting under a loaded CI runner plus the
/// native manager's restart throttle; a condition that needs longer is a bug.
pub(crate) const WAIT: Duration = Duration::from_secs(60);

/// Interval between polls of a waited-for condition.
pub(crate) const POLL: Duration = Duration::from_millis(100);

/// Deadline for one attach stream to deliver an expected marker.
const ATTACH_READ_TIMEOUT: Duration = Duration::from_secs(20);

/// Request deadline of fixture clients; a daemon `session.new` includes the
/// worker's native registration and initialization.
const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// Bound on one call to the native service manager made by the fixture.
pub(crate) const MANAGER_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Daemon start bound in the daemon's job definition, as the installer uses.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);

/// Open-file limit written to `service.toml`.
const OPEN_FILES: u64 = 8_192;

/// Interval at which the agent command prints and records its counter.
///
/// Half a second keeps a 24-row screen holding twelve seconds of history, so
/// a counter produced while the daemon was absent is still on the screen a
/// fresh attach repaints after the daemon returns.
const COUNTER_INTERVAL: &str = "0.5";

/// Deadlines written to `service.toml`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Settings {
    /// `deadlines.worker_connect_ms`.
    pub(crate) worker_connect: Duration,
    /// `deadlines.worker_initialize_ms`.
    pub(crate) worker_initialize: Duration,
    /// `deadlines.launchctl_command_ms`; also bounds one D-Bus call on Linux.
    pub(crate) manager_command: Duration,
    /// `deadlines.worker_exit_timeout_ms`.
    pub(crate) worker_exit: Duration,
    /// `deadlines.daemon_exit_timeout_ms`.
    pub(crate) daemon_exit: Duration,
    /// `deadlines.daemon_restart_throttle_ms`.
    pub(crate) restart_throttle: Duration,
    /// `sweep.grace_ms`.
    pub(crate) sweep_grace: Duration,
}

impl Default for Settings {
    /// Generous worker deadlines and short stop and restart bounds, so a
    /// hangup-ignoring descendant and a killed daemon settle quickly.
    fn default() -> Self {
        Self {
            worker_connect: Duration::from_secs(20),
            worker_initialize: Duration::from_secs(45),
            manager_command: Duration::from_secs(10),
            worker_exit: Duration::from_secs(3),
            daemon_exit: Duration::from_secs(10),
            restart_throttle: Duration::from_secs(1),
            sweep_grace: Duration::from_secs(1),
        }
    }
}

/// Live identity of one session's runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// Session identifier.
    pub(crate) session_id: String,
    /// Current worker generation from the durable record.
    pub(crate) generation: String,
    /// Worker executable from the durable record.
    pub(crate) executable: PathBuf,
    /// Worker process as the native manager reports it.
    pub(crate) worker: ProcessIdentity,
    /// PTY child process.
    pub(crate) child: ProcessIdentity,
    /// PTY device of the child.
    pub(crate) tty: String,
    /// Worker identifier the daemon reports.
    pub(crate) worker_id: String,
    /// Runtime identifier the daemon reports.
    pub(crate) runtime_id: String,
}

impl Snapshot {
    /// Service ID of this generation.
    pub(crate) fn service_id(&self) -> ServiceId {
        WorkerKey::new(self.session_id.clone(), self.generation.clone())
            .expect("recorded generation is valid")
            .service_id()
    }
}

/// One isolated installation and its native jobs.
pub(crate) struct Installation {
    /// Canonical temporary root.
    pub(crate) root: PathBuf,
    /// Resolved application paths of the installation.
    pub(crate) paths: BasePaths,
    /// Bootstrap environment given to the daemon job.
    pub(crate) environment: BTreeMap<String, String>,
    /// `service.toml` of the installation.
    pub(crate) config_path: PathBuf,
    /// Active service configuration.
    pub(crate) config: ServiceConfig,
    /// Directory receiving the agent command's probe files.
    pub(crate) probe: PathBuf,
    /// Worker jobs of this namespace, as the daemon's backend sees them.
    pub(crate) workers: Box<dyn Supervisor>,
    /// The daemon's native login job.
    pub(crate) daemon: Box<dyn DaemonSupervisor>,
    inspector: HostInspector,
    /// Jobs planted outside the namespace or the daemon, removed on drop.
    extra: backend::Extra,
    _temporary: tempfile::TempDir,
}

impl Installation {
    /// Lays out an installation of this build's binaries with `settings`.
    pub(crate) async fn new(settings: Settings) -> Self {
        backend::require();
        let (daemon_binary, worker_binary) = binaries();
        // `/var` and `$TMPDIR` are symlinked or long on macOS; trusted
        // directories never follow a symlink and socket paths are bounded.
        let base = std::fs::canonicalize("/tmp").expect("canonical /tmp");
        let temporary = tempfile::Builder::new()
            .prefix("phk")
            .tempdir_in(&base)
            .expect("temporary root");
        let root = temporary.path().to_path_buf();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("private root");
        let dir = |name: &str| {
            let path = root.join(name);
            std::fs::create_dir_all(&path).expect("create fixture directory");
            path
        };
        let environment = BTreeMap::from([
            ("XDG_RUNTIME_DIR".to_owned(), path_string(&dir("run"))),
            ("XDG_STATE_HOME".to_owned(), path_string(&dir("state"))),
            ("XDG_DATA_HOME".to_owned(), path_string(&dir("data"))),
            ("XDG_CACHE_HOME".to_owned(), path_string(&dir("cache"))),
            ("XDG_CONFIG_HOME".to_owned(), path_string(&dir("config"))),
            ("HOME".to_owned(), path_string(&dir("home"))),
        ]);
        let value = |key: &str| environment.get(key).map(Into::into);
        let paths = BasePaths::resolve_for(
            Platform::current().expect("supported platform"),
            uid(),
            &PathEnv {
                xdg_runtime_dir: value("XDG_RUNTIME_DIR"),
                xdg_data_home: value("XDG_DATA_HOME"),
                xdg_state_home: value("XDG_STATE_HOME"),
                xdg_cache_home: value("XDG_CACHE_HOME"),
                xdg_config_home: value("XDG_CONFIG_HOME"),
                home: value("HOME"),
            },
        )
        .expect("resolve installation paths");
        for path in [&paths.runtime_dir, &paths.state_dir, &paths.config_dir] {
            private_dir(path);
        }
        let prefix = root.join("prefix");
        let config = service_config(&paths, &prefix, pohunek_daemon::DAEMON_VERSION, settings);
        let config_path = pohunek_service_config::file_path(&paths);
        config
            .write(&config_path)
            .expect("write service configuration");
        install_version(
            config.layout(),
            config.active_version(),
            &daemon_binary,
            &worker_binary,
        );
        let probe = dir("probe");
        write_agent_profile(&paths, &probe);
        backend::prepare(&paths);
        let namespace = config.namespace();
        let workers = backend::workers(&paths, &namespace).await;
        let daemon = backend::daemon(&root, &namespace).await;
        Self {
            root,
            paths,
            environment,
            config_path,
            config,
            probe,
            workers,
            daemon,
            inspector: HostInspector::new(),
            extra: backend::Extra::default(),
            _temporary: temporary,
        }
    }

    /// Namespace of this installation.
    pub(crate) fn namespace(&self) -> Namespace {
        self.config.namespace()
    }

    /// Records a job planted outside this namespace's daemon for teardown.
    pub(crate) fn extra(&mut self) -> &mut backend::Extra {
        &mut self.extra
    }

    /// Rewrites `service.toml` through `change`; the daemon reads it on start.
    #[cfg_attr(
        target_os = "linux",
        expect(
            dead_code,
            reason = "only the launchd outage scenario rewrites deadlines"
        )
    )]
    pub(crate) fn rewrite_config(&mut self, change: impl FnOnce(&mut ConfigSpec)) {
        let mut spec = self.config.to_spec();
        change(&mut spec);
        self.config = ServiceConfig::new(spec).expect("valid rewritten configuration");
        self.config
            .write(&self.config_path)
            .expect("rewrite service configuration");
    }

    /// The daemon's job definition, as `pohunek service install` renders it.
    pub(crate) fn daemon_definition(&self) -> JobDefinition {
        let deadlines = self.config.deadlines();
        JobDefinition::new(JobSpec {
            executable: self.config.daemon_executable(),
            arguments: vec![
                "--service-config".to_owned(),
                path_string(&self.config_path),
            ],
            environment: self.environment.clone(),
            working_directory: self.config.state_root().to_path_buf(),
            logs: backend::daemon_logs(&self.paths, &self.namespace()),
            start_timeout: DAEMON_START_TIMEOUT,
            exit_timeout: deadlines.daemon_exit_timeout,
            restart: RestartPolicy::OnFailure {
                throttle: deadlines.daemon_restart_throttle,
            },
            open_files: self.config.open_files(),
        })
        .expect("valid daemon definition")
    }

    /// Registers the daemon as a native job and waits until it serves.
    pub(crate) async fn start_daemon(&self) -> ProcessIdentity {
        self.daemon
            .install(&self.daemon_definition())
            .await
            .expect("install the daemon job");
        self.wait_daemon(None).await
    }

    /// Stops the daemon job and removes its definition; workers keep running.
    pub(crate) async fn stop_daemon(&self) {
        let process = self.daemon.inspect().await.ok().and_then(|job| job.process);
        self.daemon
            .uninstall()
            .await
            .expect("uninstall the daemon job");
        if let Some(process) = process {
            self.wait_gone(process, "stopped daemon").await;
        }
    }

    /// Restarts only the daemon through the native manager.
    pub(crate) async fn restart_daemon(&self) -> ProcessIdentity {
        let previous = self.daemon_process().await;
        self.daemon
            .replace(&self.daemon_definition())
            .await
            .expect("restart the daemon job");
        self.wait_daemon(Some(previous)).await
    }

    /// Kills the daemon with `SIGKILL` and waits for the manager to restart it.
    ///
    /// Returns the previous and the restarted daemon process.
    pub(crate) async fn kill_daemon(&self) -> (ProcessIdentity, ProcessIdentity) {
        let previous = self.daemon_process().await;
        signal(previous.pid, nix::sys::signal::Signal::SIGKILL);
        (previous, self.wait_daemon(Some(previous)).await)
    }

    /// Runs the daemon as a direct child process instead of a native job.
    ///
    /// Only for scenarios that must control the daemon's own environment,
    /// which a native job definition restricts to the bootstrap variables.
    /// The child is killed when the returned handle drops.
    #[cfg_attr(
        target_os = "macos",
        expect(
            dead_code,
            reason = "only the systemd outage scenario controls the daemon's bus"
        )
    )]
    pub(crate) async fn spawn_daemon(
        &self,
        environment: &[(&str, String)],
    ) -> tokio::process::Child {
        let log = std::fs::File::create(self.root.join("daemon.log")).expect("daemon log");
        let child = tokio::process::Command::new(self.config.daemon_executable())
            .arg("--service-config")
            .arg(&self.config_path)
            .env_clear()
            .envs(&self.environment)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .envs(environment.iter().map(|(key, value)| (*key, value)))
            .current_dir(self.config.state_root())
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().expect("daemon log handle"))
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn the daemon");
        eventually("daemon socket", || async {
            let mut client = Client::connect_local(&self.paths.socket).await.ok()?;
            client
                .call::<method::SessionList>(protocol::SessionListParams::default())
                .await
                .ok()
        })
        .await;
        child
    }

    /// Current daemon process as its native job reports it.
    pub(crate) async fn daemon_process(&self) -> ProcessIdentity {
        self.daemon
            .inspect()
            .await
            .expect("inspect the daemon job")
            .process
            .expect("the daemon job has a process")
    }

    /// Waits until a daemon process other than `previous` serves requests.
    async fn wait_daemon(&self, previous: Option<ProcessIdentity>) -> ProcessIdentity {
        let process = eventually("daemon job process", || async {
            self.daemon
                .inspect()
                .await
                .ok()
                .and_then(|job| job.process)
                .filter(|process| Some(*process) != previous)
        })
        .await;
        eventually("daemon socket", || async {
            let mut client = Client::connect_local(&self.paths.socket).await.ok()?;
            client
                .call::<method::SessionList>(protocol::SessionListParams::default())
                .await
                .ok()
        })
        .await;
        assert_eq!(
            self.daemon_process().await,
            process,
            "the daemon serving requests is the job's process"
        );
        process
    }

    /// Connects a client to the daemon.
    pub(crate) async fn client(&self) -> Client {
        eventually("daemon client", || async {
            Client::connect_local_with_options(
                &self.paths.socket,
                ClientOptions::default().with_request_timeout(CLIENT_REQUEST_TIMEOUT),
            )
            .await
            .ok()
        })
        .await
    }

    /// Parameters of a `session.new` for the fixture's agent profile.
    pub(crate) fn new_params(&self, name: &str) -> SessionNewParams {
        SessionNewParams {
            agent: AGENT.to_owned(),
            name: Some(name.to_owned()),
            cwd: Some(self.home()),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Creates one session and returns it once its runtime is live.
    pub(crate) async fn new_session(&self, name: &str) -> SessionInfo {
        let created = self
            .client()
            .await
            .call::<method::SessionNew>(self.new_params(name))
            .await
            .expect("create a session")
            .session;
        self.wait_runtime(&created.id.0, RuntimeState::Live).await
    }

    /// Inspects one session.
    pub(crate) async fn inspect(&self, session_id: &str) -> SessionInfo {
        self.try_inspect(session_id)
            .await
            .unwrap_or_else(|error| panic!("inspect {session_id}: {error}"))
    }

    /// Inspects one session, returning the daemon's error.
    pub(crate) async fn try_inspect(&self, session_id: &str) -> Result<SessionInfo, ClientError> {
        self.client()
            .await
            .call::<method::SessionInspect>(SessionId(session_id.to_owned()))
            .await
    }

    /// Lists every session the daemon knows.
    pub(crate) async fn sessions(&self) -> Vec<SessionInfo> {
        self.client()
            .await
            .call::<method::SessionList>(protocol::SessionListParams::default())
            .await
            .expect("list sessions")
    }

    /// Reads the daemon's runtime inventory.
    pub(crate) async fn inventory(&self) -> RuntimeInventoryResult {
        self.client()
            .await
            .call::<method::SessionRuntimeInventory>(())
            .await
            .expect("read the runtime inventory")
    }

    /// Waits until the session's runtime reaches `state`.
    pub(crate) async fn wait_runtime(&self, session_id: &str, state: RuntimeState) -> SessionInfo {
        explained(&format!("{session_id} runtime {state:?}"), || async {
            match self.try_inspect(session_id).await {
                Ok(info) if runtime_state(&info) == Some(state) => Ok(info),
                Ok(info) => Err(format!("runtime {:?}", info.runtime)),
                Err(error) => Err(format!("inspect failed: {error}")),
            }
        })
        .await
    }

    /// Waits until the session's runtime reaches `state` with `reason`.
    pub(crate) async fn wait_reason(
        &self,
        session_id: &str,
        state: RuntimeState,
        reason: &str,
    ) -> SessionInfo {
        explained(
            &format!("{session_id} runtime {state:?} ({reason})"),
            || async {
                match self.try_inspect(session_id).await {
                    Ok(info)
                        if runtime_state(&info) == Some(state)
                            && loss_reason(&info) == Some(reason) =>
                    {
                        Ok(info)
                    }
                    Ok(info) => Err(format!("runtime {:?}", info.runtime)),
                    Err(error) => Err(format!("inspect failed: {error}")),
                }
            },
        )
        .await
    }

    /// Loads the session's durable record.
    pub(crate) fn record(&self, session_id: &str) -> Option<SessionRecord> {
        self.store()
            .load_sessions()
            .expect("load durable records")
            .into_iter()
            .find(|record| record.session_id == session_id)
    }

    /// Rewrites the session's durable record; the daemon must be stopped.
    pub(crate) fn patch_record(&self, session_id: &str, change: impl FnOnce(&mut SessionRecord)) {
        let mut record = self.record(session_id).expect("durable record exists");
        change(&mut record);
        self.store()
            .record_session(&record)
            .expect("rewrite durable record");
    }

    fn store(&self) -> Store {
        Store::new(self.paths.data_dir.join("metadata.jsonl"))
    }

    /// Captures the live identity of one session's runtime.
    pub(crate) async fn snapshot(&self, session_id: &str) -> Snapshot {
        let info = self.wait_runtime(session_id, RuntimeState::Live).await;
        let record = self.record(session_id).expect("durable record");
        let generation = record
            .runtime
            .generation
            .clone()
            .expect("recorded generation");
        let key = WorkerKey::new(session_id, generation.clone()).expect("valid generation");
        let worker = self
            .workers
            .inspect(&key.service_id())
            .await
            .expect("inspect the worker job")
            .process
            .expect("the worker job has a process");
        let runtime = info.runtime.as_ref().expect("session runtime");
        Snapshot {
            session_id: session_id.to_owned(),
            generation,
            executable: record
                .runtime
                .executable
                .clone()
                .expect("recorded executable"),
            worker,
            child: self.identity(info.pid).expect("PTY child is running"),
            tty: tty(info.pid),
            worker_id: runtime.worker_id.clone().expect("worker id"),
            runtime_id: runtime.runtime_id.clone().expect("runtime id"),
        }
    }

    /// Asserts that the session still runs exactly `expected`'s runtime.
    pub(crate) async fn assert_same_runtime(&self, expected: &Snapshot) {
        let current = self.snapshot(&expected.session_id).await;
        assert_eq!(
            &current, expected,
            "{} runtime changed",
            expected.session_id
        );
        assert!(
            self.is_running(expected.worker),
            "{} worker process is gone",
            expected.session_id
        );
    }

    /// Current identity of `pid`, if it runs.
    pub(crate) fn identity(&self, pid: u32) -> Option<ProcessIdentity> {
        self.inspector.identity(pid).expect("inspect a process")
    }

    /// Whether exactly `process` still runs.
    pub(crate) fn is_running(&self, process: ProcessIdentity) -> bool {
        self.inspector
            .is_running(process)
            .expect("inspect a process")
    }

    /// Waits until exactly `process` has exited.
    pub(crate) async fn wait_gone(&self, process: ProcessIdentity, what: &str) {
        eventually(&format!("{what} {} exits", process.pid), || async {
            (!self.is_running(process)).then_some(())
        })
        .await;
    }

    /// The hangup-ignoring descendant the session's agent command started.
    pub(crate) async fn descendant(&self, session_id: &str) -> ProcessIdentity {
        let path = self.probe.join(format!("{session_id}.descendant"));
        let pid = eventually("descendant PID file", || async {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
        })
        .await;
        self.identity(pid).expect("descendant is running")
    }

    /// Last counter value the session's agent command recorded.
    pub(crate) fn counter(&self, session_id: &str) -> u64 {
        std::fs::read_to_string(self.probe.join(format!("{session_id}.counter")))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Worker jobs of this namespace.
    pub(crate) async fn jobs(&self) -> Vec<ServiceObservation> {
        self.workers.discover().await.expect("discover worker jobs")
    }

    /// Service IDs of the namespace's worker jobs.
    pub(crate) async fn job_ids(&self) -> Vec<String> {
        let mut ids = self
            .jobs()
            .await
            .into_iter()
            .map(|job| job.id.to_string())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    /// Whether the backend proves `id` absent.
    pub(crate) async fn job_absent(&self, id: &ServiceId) -> bool {
        matches!(
            self.workers.inspect(id).await,
            Err(SupervisorError::NotFound(_))
        )
    }

    /// Worker journal of one worker, as JSON.
    pub(crate) fn journal_path(&self, session_id: &str, worker_id: &str) -> PathBuf {
        self.paths
            .state_dir
            .join("workers")
            .join(session_id)
            .join(format!("{worker_id}.json"))
    }

    /// Rewrites a worker journal through `change`, keeping its mode.
    pub(crate) fn patch_journal(
        &self,
        session_id: &str,
        worker_id: &str,
        change: impl FnOnce(&mut serde_json::Value),
    ) {
        let path = self.journal_path(session_id, worker_id);
        let mut journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read worker journal"))
                .expect("worker journal is JSON");
        change(&mut journal);
        let staged = path.with_extension("json.patch");
        std::fs::write(
            &staged,
            serde_json::to_vec(&journal).expect("serialize worker journal"),
        )
        .expect("write patched journal");
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))
            .expect("private patched journal");
        std::fs::rename(&staged, &path).expect("replace worker journal");
    }

    /// Persists a logical record the daemon never created, for inputs a
    /// running daemon cannot produce (a resumable binding, a foreign peer).
    pub(crate) fn seed_record(&self, session_id: &str, runtime: RuntimeRecord) {
        let now = "2026-09-24T00:00:00Z".to_owned();
        let live = runtime.state == RuntimeState::Live;
        let info = SessionInfo {
            id: SessionId(session_id.to_owned()),
            external: Some(false),
            capabilities: SessionCapabilities {
                resume: true,
                fork: false,
            },
            name: Some(format!("seeded {session_id}")),
            agent: "claude".to_owned(),
            agent_base: AgentKind::Claude,
            cwd: self.home(),
            cwd_source: Some(CwdSource::Launch),
            pid: 0,
            runtime: Some(SessionRuntime {
                state: runtime.state,
                runtime_generation: protocol::RuntimeGeneration::new(1),
                worker_id: runtime.worker_id.clone(),
                runtime_id: runtime.runtime_id.clone(),
                started_at: Some(now.clone()),
                last_connected_at: Some(now.clone()),
                loss_reason: runtime.reason.clone(),
            }),
            cols: 80,
            rows: 24,
            state: if live {
                SessionState::Running
            } else {
                SessionState::Stopped
            },
            state_source: StateSource::Process,
            activity: None,
            subagents: Vec::new(),
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: Some(format!("native-{session_id}")),
            native_session_path: None,
            project_id: None,
            project_label: None,
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            metadata: BTreeMap::new(),
            created_at: now.clone(),
            updated_at: now,
            exit_code: None,
        };
        self.store()
            .record_session(&SessionRecord {
                schema_version: 1,
                session_id: session_id.to_owned(),
                desired_state: DesiredState::Running,
                transaction: None,
                info,
                recovery: Some(ResumeBinding {
                    session_id: session_id.to_owned(),
                    name: Some(format!("seeded {session_id}")),
                    agent: "claude".to_owned(),
                    agent_base: AgentKind::Claude,
                    cwd: self.home(),
                    cols: 80,
                    rows: 24,
                    native_session_id: Some(format!("native-{session_id}")),
                    native_session_path: None,
                    project_id: None,
                    is_linked_worktree: None,
                    metadata: BTreeMap::new(),
                    program: "/bin/sh".to_owned(),
                    args: vec!["-c".to_owned(), agent_script(&self.probe)],
                    input_rules: StoredInputRules::from(InputRules::unrestricted(
                        false,
                        Duration::ZERO,
                    )),
                    resume_mode: Some(ResumeMode::Flag),
                    ref_kind: Some(SessionRefKind::Id),
                    resumable: true,
                    fork_mode: Some(ForkMode::ClaudeSession),
                    fork_resume_mode: Some(ResumeMode::Flag),
                    fork_ref_kind: Some(SessionRefKind::Id),
                    forkable: false,
                }),
                native_identity_ordering: None,
                runtime,
            })
            .expect("persist seeded record");
    }

    /// Runtime record naming `key` as the current generation.
    pub(crate) fn runtime_record(&self, key: &WorkerKey, state: RuntimeState) -> RuntimeRecord {
        RuntimeRecord {
            state,
            worker_id: None,
            runtime_id: None,
            service_id: Some(key.service_id().to_string()),
            generation: Some(key.generation().to_owned()),
            executable: Some(self.config.worker_executable()),
            reason: None,
        }
    }

    /// Opens an attach stream for one session.
    pub(crate) async fn attach(&self, session_id: &str) -> UnixStream {
        let attach = self
            .client()
            .await
            .call::<method::SessionAttach>(SessionAttachParams {
                session_id: SessionId(session_id.to_owned()),
                initial_dimensions: None,
                origin_session_id: None,
                origin_daemon_id: None,
                origin_worker_id: None,
            })
            .await
            .expect("open an attach stream");
        let mut stream = UnixStream::connect(&self.paths.socket)
            .await
            .expect("connect the raw attach stream");
        let mut header = serde_json::to_vec(&AttachHeader {
            attach: attach.stream_id,
        })
        .expect("serialize the attach header");
        header.push(b'\n');
        stream
            .write_all(&header)
            .await
            .expect("write the attach header");
        stream
    }

    /// Home directory of the installation's user.
    pub(crate) fn home(&self) -> PathBuf {
        self.root.join("home")
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        let namespace = self.namespace();
        let paths = self.paths.clone();
        let root = self.root.clone();
        let extra = std::mem::take(&mut self.extra);
        // The async backends cannot run inside the test's runtime from a
        // synchronous drop, so teardown owns a runtime on its own thread.
        let teardown = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("teardown runtime");
            runtime.block_on(backend::teardown(&root, &paths, &namespace, extra));
        });
        if teardown.join().is_err() {
            eprintln!("teardown of {} panicked", self.root.display());
        }
        self.kill_probes();
    }
}

impl Installation {
    /// Kills every hangup-ignoring descendant still running.
    fn kill_probes(&self) {
        let Ok(entries) = std::fs::read_dir(&self.probe) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("descendant") {
                continue;
            }
            let Some(pid) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
            else {
                continue;
            };
            // Only a `sleep` still carrying this installation's probe marker
            // is ours; a reused PID is left alone.
            let ours = self
                .inspector
                .process(pid)
                .ok()
                .flatten()
                .is_some_and(|fact| fact.comm.contains("sleep"));
            if ours {
                signal(pid, nix::sys::signal::Signal::SIGKILL);
            }
        }
    }
}

/// Polls `probe` until it yields a value, failing after [`WAIT`].
pub(crate) async fn eventually<T, F, Fut>(what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(POLL).await;
    }
}

/// Polls `probe` until it succeeds, failing after [`WAIT`] with the last
/// explanation it gave.
pub(crate) async fn explained<T, F, Fut>(what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let deadline = Instant::now() + WAIT;
    loop {
        match probe().await {
            Ok(value) => return value,
            Err(last) => assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; last observed: {last}"
            ),
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Asserts that `probe` holds for the whole of `period`.
pub(crate) async fn stays<F, Fut>(what: &str, period: Duration, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let end = Instant::now() + period;
    while Instant::now() < end {
        assert!(probe().await, "{what} stopped holding");
        tokio::time::sleep(POLL).await;
    }
}

/// Runtime state of a session, if it has a runtime.
pub(crate) fn runtime_state(info: &SessionInfo) -> Option<RuntimeState> {
    info.runtime.as_ref().map(|runtime| runtime.state)
}

/// Loss reason of a session's runtime.
pub(crate) fn loss_reason(info: &SessionInfo) -> Option<&str> {
    info.runtime
        .as_ref()
        .and_then(|runtime| runtime.loss_reason.as_deref())
}

/// Effective user ID of the test.
pub(crate) fn uid() -> u32 {
    nix::unistd::Uid::effective().as_raw()
}

/// Sends `signal` to `pid`; an already exited process is fine.
pub(crate) fn signal(pid: u32, signal: nix::sys::signal::Signal) {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(pid).expect("PID fits i32"));
    match nix::sys::signal::kill(pid, signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(error) => panic!("signal {pid}: {error}"),
    }
}

/// PTY device name of `pid`, as `ps` reports it.
pub(crate) fn tty(pid: u32) -> String {
    let output = std::process::Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    let tty = String::from_utf8(output.stdout)
        .expect("ps output is UTF-8")
        .trim()
        .to_owned();
    assert!(
        !tty.is_empty() && tty != "?" && tty != "??",
        "process {pid} has no controlling terminal"
    );
    tty
}

/// Reads an attach stream until `found` accepts the collected output.
pub(crate) async fn read_until(
    stream: &mut UnixStream,
    what: &str,
    found: impl Fn(&[u8]) -> bool,
) -> Vec<u8> {
    tokio::time::timeout(ATTACH_READ_TIMEOUT, async {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).await.expect("read attach output");
            assert!(count > 0, "attach closed before {what}");
            output.extend_from_slice(&buffer[..count]);
            if found(&output) {
                return output;
            }
        }
    })
    .await
    .unwrap_or_else(|_elapsed| panic!("attach did not deliver {what}"))
}

/// Whether `haystack` contains `needle`.
pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Counter values printed as `counter:NNNN` in terminal output.
pub(crate) fn counters(bytes: &[u8]) -> Vec<u64> {
    const MARKER: &[u8] = b"counter:";
    let mut values = Vec::new();
    let mut offset = 0;
    while let Some(relative) = bytes[offset..]
        .windows(MARKER.len())
        .position(|window| window == MARKER)
    {
        let start = offset + relative + MARKER.len();
        let end = bytes[start..]
            .iter()
            .position(|byte| !byte.is_ascii_digit())
            .map_or(bytes.len(), |length| start + length);
        if let Some(value) = std::str::from_utf8(&bytes[start..end])
            .ok()
            .and_then(|digits| digits.parse().ok())
        {
            values.push(value);
        }
        offset = end.max(start);
        if offset >= bytes.len() {
            break;
        }
    }
    values
}

/// Absolute path as a UTF-8 string.
pub(crate) fn path_string(path: &Path) -> String {
    path.to_str().expect("fixture paths are UTF-8").to_owned()
}

/// Creates `path` and its missing parents with mode `0700`.
fn private_dir(path: &Path) {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .expect("create private directory");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .expect("private directory mode");
}

/// Daemon and worker binaries of this build.
///
/// `POHUNEK_DAEMON_BIN` and `POHUNEK_WORKER_BIN` name them explicitly (the
/// Linux CI job exports both); otherwise the daemon is this package's binary
/// and the worker is the `pohunek-sessiond` built into the same target
/// directory, which the macOS CI job builds before the test step.
fn binaries() -> (PathBuf, PathBuf) {
    let daemon = std::env::var_os("POHUNEK_DAEMON_BIN").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_pohunekd")),
        PathBuf::from,
    );
    let worker = std::env::var_os("POHUNEK_WORKER_BIN").map_or_else(
        || {
            PathBuf::from(env!("CARGO_BIN_EXE_pohunekd"))
                .with_file_name(pohunek_paths::WORKER_EXECUTABLE_NAME)
        },
        PathBuf::from,
    );
    for (name, path) in [("daemon", &daemon), ("worker", &worker)] {
        assert!(
            path.is_absolute() && path.is_file(),
            "the {name} binary {} is missing; build it with \
             `cargo build -p pohunek-daemon -p pohunek-session-worker --bins`",
            path.display()
        );
    }
    (daemon, worker)
}

/// Builds the installation's `service.toml`.
fn service_config(
    paths: &BasePaths,
    prefix: &Path,
    version: &str,
    settings: Settings,
) -> ServiceConfig {
    ServiceConfig::new(ConfigSpec {
        prefix: prefix.to_path_buf(),
        active_version: version.to_owned(),
        uid: uid(),
        state_root: std::fs::canonicalize(&paths.state_dir).expect("canonical state root"),
        runtime_root: std::fs::canonicalize(&paths.runtime_dir).expect("canonical runtime root"),
        deadlines: Deadlines {
            worker_connect: settings.worker_connect,
            worker_initialize: settings.worker_initialize,
            launchctl_command: settings.manager_command,
            worker_exit_timeout: settings.worker_exit,
            daemon_exit_timeout: settings.daemon_exit,
            daemon_restart_throttle: settings.restart_throttle,
        },
        environment_allowlist: DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect(),
        sweep_grace: settings.sweep_grace,
        open_files: OPEN_FILES,
    })
    .expect("valid service configuration")
}

/// Copies the binaries into `<prefix>/libexec/pohunek/<version>/`.
pub(crate) fn install_version(layout: &InstallLayout, version: &str, daemon: &Path, worker: &Path) {
    let directory = layout.version_dir(version).expect("valid version");
    std::fs::create_dir_all(&directory).expect("create version directory");
    for (source, target) in [
        (
            daemon,
            layout.daemon_executable(version).expect("daemon path"),
        ),
        (
            worker,
            layout.worker_executable(version).expect("worker path"),
        ),
    ] {
        std::fs::copy(source, &target).expect("install versioned binary");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("executable binary");
    }
}

/// Writes the `lifecycle` agent profile running [`agent_script`].
fn write_agent_profile(paths: &BasePaths, probe: &Path) {
    let directory = paths.config_dir.join("agents");
    private_dir(&directory);
    let profile = format!(
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", {}]\n",
        toml_string(&agent_script(probe))
    );
    let path = directory.join(format!("{AGENT}.toml"));
    std::fs::write(&path, profile).expect("write agent profile");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("private agent profile");
}

/// Agent command of every test session.
///
/// It starts a descendant that ignores `SIGHUP` and `SIGTERM` (so neither the
/// PTY hangup nor the sweep's `SIGTERM` ends it) and records its PID, prints
/// and records a counter every [`COUNTER_INTERVAL`], and echoes each input
/// line. `POHUNEK_SESSION_ID` is part of the worker-provided child identity.
pub(crate) fn agent_script(probe: &Path) -> String {
    let probe = path_string(probe);
    format!(
        "p='{probe}'; s=\"$POHUNEK_SESSION_ID\"; \
         (trap '' HUP TERM; exec sleep 100000) & echo $! > \"$p/$s.descendant\"; \
         (n=0; while :; do n=$((n+1)); printf 'counter:%04d\\n' \"$n\"; \
         echo \"$n\" > \"$p/$s.counter\"; sleep {COUNTER_INTERVAL}; done) & \
         while IFS= read -r line; do printf 'input:%s\\n' \"$line\"; done"
    )
}

/// Encodes `value` as a TOML basic string.
fn toml_string(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() + 2);
    encoded.push('"');
    for character in value.chars() {
        match character {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            other => encoded.push(other),
        }
    }
    encoded.push('"');
    encoded
}
