// Rust guideline compliant 2026-10-02

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use pohunek_daemon::procwatch::{
    Error as ProcwatchError, ExitWatch, HostInspector, OwnershipMarkers, Pid, ProcessFact,
    ProcessIdentity, ProcessInspector,
};
use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait::{guard, wait_until};
use pohunek_test_support::worker_binary;
use protocol::{
    event, CwdSource, RuntimeRef, RuntimeState, SessionAttachParams, SessionId, SessionInfo,
    SessionInputParams, SessionNewParams, ENV_DAEMON_ID, ENV_SESSION_ID,
};

// Sessions use the production stop grace, `SessionRegistryConfig::default()`:
// a shorter grace reaches the forced-stop path on a loaded host, and no test
// here is about the grace.
const TEST_COLS: u16 = 80;
const TEST_ROWS: u16 = 24;
/// Poll interval of the managed-session watcher in the pidfd test.
///
/// Short, so that many poll ticks run while the exit watch is the only path
/// that can still clear the agent; the polls cannot clear it, see
/// [`PollBlindInspector`].
const TEST_PROCWATCH_POLL: Duration = Duration::from_millis(100);
/// Seconds the managed shell stays alive after its agent exits.
///
/// Longer than the hang guard, so the session cannot end, and clear the agent
/// with it, before the exit watch has cleared the agent.
const SHELL_LINGER_SECS: u64 = 600;
/// Poll interval for the cwd-tracking integration test.
const CWD_PROCWATCH_POLL: Duration = Duration::from_millis(150);
/// Sweep interval of the external observer in the pidfd test, short for the same
/// reason as [`TEST_PROCWATCH_POLL`].
const EXTERNAL_PROCWATCH_POLL: Duration = Duration::from_millis(100);

/// Clears `POHUNEK_DAEMON_ID`/`POHUNEK_SESSION_ID` for the scope of spawning a
/// worker-backed test session.
///
/// `cargo test` may itself run inside a real pohunek-managed session (e.g. a
/// dev loop invoked from within this very repo's own pohunek session), which
/// sets both on this process's environment. `SubprocessWorkerLauncher::launch`
/// (`crates/daemon/src/runtime/launcher.rs`) spawns the worker subprocess
/// without scrubbing the ambient environment, so those stale markers would
/// otherwise leak all the way down into the freshly spawned worker and the
/// PTY child it forks, stamping the test's own fake agent with a foreign
/// daemon id that `is_foreign_owned_agent`
/// (`crates/daemon/src/session/procwatch.rs`) then correctly refuses to
/// adopt, so it never surfaces as this test's observed agent. Clearing both
/// vars before `registry.create` breaks the leak at its source. The returned
/// value holds the binary-wide environment lock and restores both variables
/// when dropped.
fn without_pohunek_markers() -> ProcessEnv {
    let mut process_env = ProcessEnv::lock();
    process_env.remove(ENV_DAEMON_ID).remove(ENV_SESSION_ID);
    process_env
}

/// What the poll path keeps reading about a process after it died.
#[derive(Debug)]
struct LastSeen {
    identity: ProcessIdentity,
    fact: ProcessFact,
    cwd: PathBuf,
    executable: Option<PathBuf>,
    markers: OwnershipMarkers,
}

/// Process inspector that delegates to the host and can freeze one process in
/// the view of every poll, sweep and rescan.
///
/// After [`PollBlindInspector::freeze`] the process stays listed, running and
/// readable through every method the scans use, with the facts captured at
/// that moment, even once it has exited. No scan, however late or however long
/// it runs, can then notice the death. `exit_watch` still arms a real pidfd, so
/// the only way left for the registry to drop the process is the exit watch
/// that the test is about.
///
/// [`PollBlindInspector::thaw`] ends the freeze: every method then reports host
/// truth, so a later scan sees the process as gone. Every process listing
/// (`same_user_processes`, `descendants`) is counted, which lets a test wait
/// for scans that ran after the thaw.
#[derive(Debug)]
struct PollBlindInspector {
    host: HostInspector,
    frozen: Mutex<Option<LastSeen>>,
    unreadable_pid: Mutex<Option<Pid>>,
    listings: AtomicUsize,
}

impl PollBlindInspector {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            host: HostInspector::new(),
            frozen: Mutex::new(None),
            unreadable_pid: Mutex::new(None),
            listings: AtomicUsize::new(0),
        })
    }

    /// Ends the freeze; every method reports host truth from here on.
    fn thaw(&self) {
        *self.lock() = None;
    }

    fn make_markers_unreadable(&self, pid: Pid) {
        *self.unreadable_pid.lock().expect("unreadable PID lock") = Some(pid);
    }

    /// Number of process listings taken so far.
    fn listings(&self) -> usize {
        self.listings.load(Ordering::SeqCst)
    }

    /// Captures `pid` as it is now and keeps reporting it that way.
    fn freeze(&self, pid: Pid) {
        let identity = self
            .host
            .identity(pid)
            .expect("inspect the process to freeze")
            .expect("the process to freeze is alive");
        let last_seen = LastSeen {
            identity,
            fact: self
                .host
                .process(pid)
                .expect("read the process to freeze")
                .expect("the process to freeze is alive"),
            cwd: self.host.cwd(pid).expect("read the process cwd"),
            executable: self
                .host
                .executable(pid)
                .expect("read the process executable"),
            markers: self
                .host
                .ownership_markers(pid)
                .expect("read the process ownership markers"),
        };
        *self.lock() = Some(last_seen);
    }

    fn lock(&self) -> MutexGuard<'_, Option<LastSeen>> {
        self.frozen.lock().expect("frozen process lock")
    }

    /// Runs `view` on the frozen process when `pid` is it.
    fn frozen_view<T>(&self, pid: Pid, view: impl FnOnce(&LastSeen) -> T) -> Option<T> {
        self.lock()
            .as_ref()
            .filter(|last_seen| last_seen.fact.pid == pid)
            .map(view)
    }

    /// Adds the frozen process to `facts` when the host no longer lists it and
    /// `listed` says it belongs in this listing.
    fn with_frozen(
        &self,
        mut facts: Vec<ProcessFact>,
        listed: impl FnOnce(&ProcessFact, &[ProcessFact]) -> bool,
    ) -> Vec<ProcessFact> {
        if let Some(last_seen) = self.lock().as_ref() {
            let missing = facts.iter().all(|fact| fact.pid != last_seen.fact.pid);
            if missing && listed(&last_seen.fact, &facts) {
                facts.push(last_seen.fact.clone());
            }
        }
        facts
    }
}

impl ProcessInspector for PollBlindInspector {
    fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, ProcwatchError> {
        match self.frozen_view(pid, |last_seen| last_seen.identity) {
            Some(identity) => Ok(Some(identity)),
            None => self.host.identity(pid),
        }
    }

    fn is_running(&self, identity: ProcessIdentity) -> Result<bool, ProcwatchError> {
        let frozen = self
            .lock()
            .as_ref()
            .is_some_and(|last_seen| last_seen.identity == identity);
        if frozen {
            Ok(true)
        } else {
            self.host.is_running(identity)
        }
    }

    fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, ProcwatchError> {
        match self.frozen_view(pid, |last_seen| last_seen.fact.ppid) {
            Some(parent) => Ok(Some(parent)),
            None => self.host.parent_pid(pid),
        }
    }

    fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, ProcwatchError> {
        match self.frozen_view(pid, |last_seen| last_seen.fact.clone()) {
            Some(fact) => Ok(Some(fact)),
            None => self.host.process(pid),
        }
    }

    fn same_user_processes(&self) -> Result<Vec<ProcessFact>, ProcwatchError> {
        self.listings.fetch_add(1, Ordering::SeqCst);
        let facts = self.host.same_user_processes()?;
        Ok(self.with_frozen(facts, |_, _| true))
    }

    fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, ProcwatchError> {
        self.listings.fetch_add(1, Ordering::SeqCst);
        let facts = self.host.descendants(root)?;
        Ok(self.with_frozen(facts, |frozen, listed| {
            frozen.ppid == root || listed.iter().any(|fact| fact.pid == frozen.ppid)
        }))
    }

    fn cwd(&self, pid: Pid) -> Result<PathBuf, ProcwatchError> {
        match self.frozen_view(pid, |last_seen| last_seen.cwd.clone()) {
            Some(cwd) => Ok(cwd),
            None => self.host.cwd(pid),
        }
    }

    fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, ProcwatchError> {
        match self.frozen_view(pid, |last_seen| last_seen.executable.clone()) {
            Some(executable) => Ok(executable),
            None => self.host.executable(pid),
        }
    }

    fn exit_watch(&self, identity: ProcessIdentity) -> Result<ExitWatch, ProcwatchError> {
        self.host.exit_watch(identity)
    }

    fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, ProcwatchError> {
        if *self.unreadable_pid.lock().expect("unreadable PID lock") == Some(pid) {
            return Err(ProcwatchError::Unobservable {
                operation: "read_ownership_markers",
            });
        }
        match self.frozen_view(pid, |last_seen| last_seen.markers.clone()) {
            Some(markers) => Ok(markers),
            None => self.host.ownership_markers(pid),
        }
    }

    fn foreground_process_group(&self, root_pid: Pid) -> Result<Option<Pid>, ProcwatchError> {
        self.host.foreground_process_group(root_pid)
    }
}

/// Build a `SessionRegistry` wired to a real `SubprocessWorkerLauncher` (the
/// built `pohunek-sessiond`), rooted under a unique per-call worker home, so
/// `registry.create` can actually launch a durable worker instead of failing
/// with `worker_backend_required`.
///
/// This file drives `SessionRegistry` directly with no `ControlServer`
/// (unlike `health_socket.rs`'s integration tests), so the worker's
/// `--daemon-socket-path` points at an unbound placeholder path: harmless,
/// since it is only used for the worker's own best-effort hook handshake,
/// which none of these tests exercise. The worker child inherits this
/// process's real `PATH`, so `sh`/`sleep`/`stty` resolve normally; this file
/// never narrows `PATH` (unlike `health_socket.rs`'s `PathGuard`), so there is
/// no PATH-isolation race to guard against here.
///
/// The worker's directories are the private ones of `env`, whose short root
/// leaves room for the worker's nested control socket.
fn worker_backed_registry(
    env: &TestEnv,
    config: SessionRegistryConfig,
    inspector: Arc<dyn ProcessInspector>,
) -> SessionRegistry {
    worker_backed_registry_with_launcher(
        env,
        config,
        inspector,
        Arc::new(SubprocessWorkerLauncher::new()),
    )
}

fn worker_backed_registry_with_launcher(
    env: &TestEnv,
    mut config: SessionRegistryConfig,
    inspector: Arc<dyn ProcessInspector>,
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
    config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
    config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
    config.supervision = Some(
        worker_environment
            .supervision(worker_binary())
            .with_environment_source(EnvironmentSource::fixed(env.environment().clone())),
    );
    SessionRegistry::new_with_launcher_and_inspector(config, launcher, inspector)
}

#[tokio::test]
async fn procwatch_auto_reports_and_pidfd_clears_real_child_agent() {
    if !native_exit_watch_is_available() {
        return;
    }

    let env = TestEnv::new().expect("create the test environment");
    let dir = env.cwd();
    let fake_codex = dir.join("codex");
    let pid_file = dir.join("agent.pid");
    symlink_sleep_as(&fake_codex);
    let script = format!(
        "{} 60 & echo $! > {}; wait $!; sleep {SHELL_LINGER_SECS}",
        fake_codex.display(),
        pid_file.display()
    );
    let poll_blind = PollBlindInspector::new();
    let registry = worker_backed_registry(
        &env,
        SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
            procwatch_poll: TEST_PROCWATCH_POLL,
            ..SessionRegistryConfig::default()
        },
        Arc::clone(&poll_blind) as Arc<dyn ProcessInspector>,
    );
    let created = {
        let _process_env = without_pohunek_markers();
        registry
            .create(SessionNewParams {
                name: Some("procwatch-real-child".to_owned()),
                agent: "shell".to_owned(),
                cwd: Some(dir.to_path_buf()),
                cols: TEST_COLS,
                rows: TEST_ROWS,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                metadata: BTreeMap::new(),
            })
            .await
            .expect("create shell session")
    };
    let child_pid = wait_for_pid_file(&pid_file).await;
    let inspector = HostInspector::new();
    let child = inspector
        .process(child_pid)
        .expect("inspect fake agent")
        .expect("fake agent remains live");
    let foreground_group = inspector
        .foreground_process_group(created.pid)
        .expect("inspect root foreground group");
    assert_eq!(child.pgid, created.pid);
    assert_eq!(foreground_group, Some(created.pid));

    let observed = wait_for_observed_pid(&registry, &created.id, child_pid).await;
    assert_eq!(observed.active_agent.as_deref(), Some("codex"));
    assert_eq!(observed.active_agent_base, Some(RuntimeRef::codex()));

    // Every poll keeps seeing the agent alive, so only the pidfd exit watch can
    // clear it.
    poll_blind.freeze(child_pid);
    let mut events = registry.subscribe();
    kill9(child_pid);
    let cleared = wait_for_cleared_agent(&created.id, &mut events).await;
    assert_eq!(cleared.active_agent, None);

    // The cleared event arrived while every poll still reported the agent alive,
    // so the exit watch produced it. From here the host truth (the agent is
    // gone) applies, and a later scan must keep the agent cleared.
    poll_blind.thaw();
    wait_for_scans_after_thaw(&poll_blind).await;
    let after_scan = registry
        .inspect(&created.id)
        .await
        .expect("inspect session");
    assert_eq!(after_scan.active_agent_pid, None);
    assert_eq!(after_scan.active_agent, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_keeps_worker_agent_after_daemon_restart() {
    let env = TestEnv::new().expect("create the test environment");
    let dir = env.cwd();
    let fake_codex = dir.join("codex");
    let pid_file = dir.join("agent.pid");
    symlink_sleep_as(&fake_codex);
    let script = format!(
        "{} {SHELL_LINGER_SECS} & echo $! > {}; wait $!",
        fake_codex.display(),
        pid_file.display()
    );
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        store_path: Some(env.state_home().join("metadata.jsonl")),
        procwatch_poll: TEST_PROCWATCH_POLL,
        ..SessionRegistryConfig::default()
    };
    let launcher = Arc::new(SubprocessWorkerLauncher::new());
    let (created, child_pid, first_daemon_id) = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("first daemon runtime");
                runtime.block_on(async {
                    let first = worker_backed_registry_with_launcher(
                        &env,
                        config.clone(),
                        Arc::new(HostInspector::new()),
                        Arc::clone(&launcher),
                    );
                    let created = {
                        let _process_env = without_pohunek_markers();
                        first
                            .create(SessionNewParams {
                                name: Some("procwatch-restart-agent".to_owned()),
                                agent: "shell".to_owned(),
                                cwd: Some(dir.to_path_buf()),
                                cols: TEST_COLS,
                                rows: TEST_ROWS,
                                project: None,
                                repo: None,
                                branch: None,
                                base_branch: None,
                                input: None,
                                metadata: BTreeMap::new(),
                            })
                            .await
                            .expect("create shell session")
                    };
                    let child_pid = wait_for_pid_file(&pid_file).await;
                    wait_for_observed_pid(&first, &created.id, child_pid).await;
                    let daemon_id = first.daemon_instance_id().to_owned();
                    first.begin_daemon_shutdown();
                    (created, child_pid, daemon_id)
                })
            })
            .join()
            .expect("first daemon thread")
    });

    let inspector = PollBlindInspector::new();
    let second = worker_backed_registry_with_launcher(
        &env,
        config,
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
        launcher,
    );
    assert_ne!(second.daemon_instance_id(), first_daemon_id);
    second.reconcile_workers().await.expect("adopt live worker");
    let reconciled = second
        .inspect(&created.id)
        .await
        .expect("inspect adopted session");
    assert_eq!(
        reconciled.runtime.as_ref().map(|runtime| runtime.state),
        Some(RuntimeState::Live),
        "a restarted daemon must adopt the live worker"
    );
    let adopted = wait_until("the restarted daemon to observe the agent", || async {
        let info = second.inspect(&created.id).await.expect("inspect session");
        (info.active_agent_pid == Some(child_pid)).then_some(info)
    })
    .await;
    assert_eq!(adopted.active_agent.as_deref(), Some("codex"));
    second
        .stop(&created.id)
        .await
        .expect("stop adopted session");
    assert_unreadable_bystander_liveness(&second, &inspector, &created.id, dir).await;
}

/// Removal remains fail-closed for a live unreadable process, but an exited
/// process with the same unreadable markers no longer blocks the session.
async fn assert_unreadable_bystander_liveness(
    registry: &SessionRegistry,
    inspector: &PollBlindInspector,
    session_id: &SessionId,
    cwd: &Path,
) {
    let mut bystander = ReapOnDrop(
        Command::new("/bin/sleep")
            .arg(SHELL_LINGER_SECS.to_string())
            .current_dir(cwd)
            .env_clear()
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn unrelated process"),
    );
    let host = HostInspector::new();
    let identity = host
        .identity(bystander.0.id())
        .expect("inspect unrelated process")
        .expect("unrelated process is alive");
    assert!(host.is_running(identity).expect("inspect live bystander"));
    inspector.make_markers_unreadable(bystander.0.id());
    let blocked = registry.remove(session_id).await;
    bystander.0.kill().expect("terminate unrelated process");
    let blocked = blocked.expect_err("a live unreadable process must keep cleanup unconfirmed");
    assert_eq!(blocked.code, "runtime_supervision_ambiguous");
    assert!(
        blocked.msg.contains(&bystander.0.id().to_string()),
        "{blocked:?}"
    );

    wait_until("unrelated process to exit without being reaped", || async {
        (!host
            .is_running(identity)
            .expect("inspect unrelated process liveness"))
        .then_some(())
    })
    .await;
    registry
        .remove(session_id)
        .await
        .expect("remove adopted session");
}

/// A failed assertion must not leave the test's long-running bystander alive.
struct ReapOnDrop(Child);

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn procwatch_skips_nested_worker_agent_with_same_session_marker() {
    let env = TestEnv::new().expect("create the test environment");
    let dir = env.cwd();
    let fake_codex = dir.join("codex");
    let pid_file = dir.join("nested-agent.pid");
    let missing_session_pid_file = dir.join("missing-session-agent.pid");
    symlink_sleep_as(&fake_codex);
    let script = format!(
        "POHUNEK_DAEMON_ID=daemon-nested POHUNEK_WORKER_INSTANCE_ID=runtime-nested \
         {} {SHELL_LINGER_SECS} & echo $! > {}; \
         (unset POHUNEK_SESSION_ID; \
         POHUNEK_DAEMON_ID=daemon-nested exec {} {SHELL_LINGER_SECS}) \
         & echo $! > {}; wait",
        fake_codex.display(),
        pid_file.display(),
        fake_codex.display(),
        missing_session_pid_file.display()
    );
    let inspector = PollBlindInspector::new();
    let registry = worker_backed_registry(
        &env,
        SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
            procwatch_poll: TEST_PROCWATCH_POLL,
            ..SessionRegistryConfig::default()
        },
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let created = {
        let _process_env = without_pohunek_markers();
        registry
            .create(SessionNewParams {
                name: Some("procwatch-nested-worker".to_owned()),
                agent: "shell".to_owned(),
                cwd: Some(dir.to_path_buf()),
                cols: TEST_COLS,
                rows: TEST_ROWS,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                metadata: BTreeMap::new(),
            })
            .await
            .expect("create shell session")
    };
    let nested_pid = wait_for_pid_file(&pid_file).await;
    let markers = wait_until("the nested agent to exec with worker markers", || async {
        inspector
            .ownership_markers(nested_pid)
            .ok()
            .filter(|markers| markers.worker_instance_id.as_deref() == Some("runtime-nested"))
    })
    .await;
    assert_eq!(markers.session_id.as_deref(), Some(created.id.0.as_str()));
    assert_eq!(
        markers.worker_instance_id.as_deref(),
        Some("runtime-nested")
    );
    let missing_session_pid = wait_for_pid_file(&missing_session_pid_file).await;
    wait_until("the agent to exec with hostile markers", || async {
        inspector
            .ownership_markers(missing_session_pid)
            .ok()
            .filter(|markers| {
                markers.session_id.is_none()
                    && markers.daemon_id.as_deref() == Some("daemon-nested")
                    && markers.worker_instance_id.as_deref()
                        == created
                            .runtime
                            .as_ref()
                            .and_then(|runtime| runtime.worker_instance_id.as_deref())
            })
    })
    .await;

    let listed_at = inspector.listings();
    wait_until("procwatch to scan the nested agent", || async {
        (inspector.listings() >= listed_at + 2).then_some(())
    })
    .await;
    let observed = registry
        .inspect(&created.id)
        .await
        .expect("inspect session");
    assert_eq!(
        observed.active_agent_pid, None,
        "nested worker agent was adopted"
    );
    registry.stop(&created.id).await.expect("stop session");
    registry.remove(&created.id).await.expect("remove session");
}

#[tokio::test]
async fn procwatch_updates_cwd_after_shell_cd() {
    let env = TestEnv::new().expect("create the test environment");
    let start_dir = env.cwd().to_path_buf();
    let target_dir = env.data_home().join("cd-target");
    fs::create_dir(&target_dir).expect("create the cd target");
    let registry = worker_backed_registry(
        &env,
        SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<String>()),
            procwatch_poll: CWD_PROCWATCH_POLL,
            ..SessionRegistryConfig::default()
        },
        Arc::new(HostInspector::new()),
    );
    let created = registry
        .create(SessionNewParams {
            name: Some("procwatch-cwd".to_owned()),
            agent: "shell".to_owned(),
            cwd: Some(start_dir),
            cols: TEST_COLS,
            rows: TEST_ROWS,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            metadata: BTreeMap::new(),
        })
        .await
        .expect("create shell session");

    registry
        .input(SessionInputParams {
            session_id: created.id.clone(),
            text: format!("cd {}", target_dir.display()),
            wait: None,
        })
        .await
        .expect("send cd command");

    let updated = wait_for_cwd(&registry, &created.id, &target_dir).await;
    assert_eq!(updated.cwd_source, Some(CwdSource::Procwatch));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the end-to-end external-session contract is clearer in one lifecycle test"
)]
async fn external_observer_reports_fake_agent_and_pidfd_removes_it() {
    if !native_exit_watch_is_available() {
        return;
    }

    let mut process_env = ProcessEnv::lock();
    let env = TestEnv::new().expect("create the test environment");
    // The observer watches the config homes a launch gives its agents: with
    // the default allowlist that is `$HOME/.claude` and `$HOME/.codex`.
    let claude_config = env.home().join(".claude");
    let codex_home = env.home().join(".codex");
    let claude_projects = claude_config.join("projects").join("work");
    let codex_sessions = codex_home.join("sessions");
    fs::create_dir_all(&claude_projects).expect("create claude projects");
    fs::create_dir_all(&codex_sessions).expect("create codex sessions");
    process_env.set("HOME", env.home());

    let work_dir = env.cwd().to_path_buf();
    let bin_dir = env.data_home().join("bin");
    fs::create_dir(&bin_dir).expect("create the fake agent directory");
    let fake_claude = bin_dir.join("claude");
    symlink_sleep_as(&fake_claude);
    let inspector = PollBlindInspector::new();
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            observe_external_agents: true,
            procwatch_poll: EXTERNAL_PROCWATCH_POLL,
            ..SessionRegistryConfig::default()
        },
        Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
    );
    let mut child = spawn_fake_agent(&env, &fake_claude);
    let child_pid = child.id();
    let transcript = claude_projects.join("session.jsonl");
    fs::write(
        &transcript,
        format!(
            "{{\"session_id\":\"native-ext\",\"cwd\":\"{}\",\"transcript_path\":\"{}\"}}\n",
            work_dir.display(),
            transcript.display()
        ),
    )
    .expect("write transcript");

    let observed = wait_for_external_pid(&registry, child_pid).await;
    assert_eq!(observed.id.0, format!("ext-{child_pid}"));
    assert_eq!(observed.external, Some(true));
    assert_eq!(observed.agent, "claude");
    assert_eq!(observed.agent_base, RuntimeRef::claude());
    assert_eq!(observed.native_session_id.as_deref(), Some("native-ext"));
    assert_eq!(
        observed.native_session_path.as_deref(),
        Some(transcript.to_string_lossy().as_ref())
    );

    assert_external_detection_unavailable(&registry, &observed.id).await;

    let attached = registry
        .attach(&SessionAttachParams {
            session_id: observed.id.clone(),
            initial_dimensions: None,
            origin_session_id: None,
            origin_daemon_id: None,
            origin_worker_id: None,
        })
        .await
        .expect_err("external sessions cannot be attached");
    assert_eq!(attached.code, "session_external_read_only");
    let input = registry
        .input(SessionInputParams {
            session_id: observed.id.clone(),
            text: "hello".to_owned(),
            wait: None,
        })
        .await
        .expect_err("external sessions cannot receive input");
    assert_eq!(input.code, "session_external_read_only");
    let resized = registry
        .resize(&observed.id, 100, 30)
        .await
        .expect_err("external sessions cannot be resized");
    assert_eq!(resized.code, "session_external_read_only");
    let stopped = registry
        .stop(&observed.id)
        .await
        .expect_err("external sessions cannot be stopped");
    assert_eq!(stopped.code, "session_external_read_only");
    let removed = registry
        .remove(&observed.id)
        .await
        .expect_err("external sessions cannot be removed");
    assert_eq!(removed.code, "session_external_read_only");
    let renamed = registry
        .rename(&observed.id, Some("external".to_owned()))
        .await
        .expect_err("external sessions cannot be renamed");
    assert_eq!(renamed.code, "session_external_read_only");
    let metadata = registry
        .set_metadata(&observed.id, BTreeMap::new())
        .await
        .expect_err("external sessions cannot store metadata");
    assert_eq!(metadata.code, "session_external_read_only");
    let resumed = registry
        .resume(&observed.id)
        .await
        .expect_err("external sessions cannot be resumed");
    assert_eq!(resumed.code, "session_external_read_only");

    // Every sweep keeps seeing the agent alive, so only the pidfd exit watch can
    // remove the entry.
    inspector.freeze(child_pid);
    let mut events = registry.subscribe();
    kill9(child_pid);
    wait_for_external_gone(&observed.id, &mut events).await;
    let _ = child.wait();

    // The removal event arrived while every sweep still listed the agent, so the
    // exit watch produced it. A later sweep, on host truth, must not list it
    // again.
    inspector.thaw();
    wait_for_scans_after_thaw(&inspector).await;
    assert!(
        registry
            .list()
            .await
            .into_iter()
            .all(|session| session.id != observed.id),
        "a sweep after the exit re-listed the external agent"
    );
}

fn native_exit_watch_is_available() -> bool {
    let inspector = HostInspector::new();
    let identity = inspector
        .identity(std::process::id())
        .expect("inspect test process")
        .expect("test process exists");
    match inspector.exit_watch(identity) {
        Ok(_) => true,
        Err(pohunek_daemon::procwatch::Error::Unavailable { .. }) => false,
        Err(err) => panic!("native exit watch failed unexpectedly: {err}"),
    }
}

fn symlink_sleep_as(path: &Path) {
    let sleep = which_sleep();
    std::os::unix::fs::symlink(sleep, path).expect("symlink fake codex");
}

/// Starts a fake agent as a genuinely external process: it runs in the
/// scrubbed environment of `env`, so it carries none of the pohunek ownership
/// markers the test runner may itself have (a marked process is treated as
/// another daemon's agent and would never surface as external).
fn spawn_fake_agent(env: &TestEnv, program: &Path) -> Child {
    env.command(program)
        .arg("60")
        .spawn()
        .expect("spawn fake agent")
}

fn which_sleep() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("sleep"))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| PathBuf::from("/bin/sleep"))
}

async fn wait_for_cwd(registry: &SessionRegistry, id: &SessionId, expected: &Path) -> SessionInfo {
    let expected = fs::canonicalize(expected).expect("canonical expected cwd");
    wait_until("the session cwd to follow the shell cd", || async {
        let info = registry.inspect(id).await.expect("inspect session");
        (info.cwd == expected).then_some(info)
    })
    .await
}

async fn wait_for_pid_file(path: &Path) -> u32 {
    wait_until("the fake agent pid file", || async {
        fs::read_to_string(path)
            .ok()
            .and_then(|contents| contents.trim().parse::<u32>().ok())
    })
    .await
}

/// Waits until the registry reports `pid` as the active agent of the session.
async fn wait_for_observed_pid(
    registry: &SessionRegistry,
    id: &SessionId,
    pid: u32,
) -> SessionInfo {
    wait_until("the watcher to observe the agent", || async {
        let info = registry.inspect(id).await.expect("inspect session");
        (info.active_agent_pid == Some(pid)).then_some(info)
    })
    .await
}

/// Waits for the lifecycle event that reports no active agent for session `id`.
///
/// The proof comes from the event itself, never from a state read that a later
/// scan could overwrite; `events` must be subscribed before the exit is
/// triggered.
async fn wait_for_cleared_agent(
    id: &SessionId,
    events: &mut tokio::sync::broadcast::Receiver<protocol::Event>,
) -> SessionInfo {
    guard("the event reporting no active agent", async {
        loop {
            let info = next_session_event(events, event::SESSION_UPDATED).await;
            if info.id == *id && info.active_agent_pid.is_none() {
                return info;
            }
        }
    })
    .await
}

/// Waits until the process listings show that a complete scan ran on host truth
/// after [`PollBlindInspector::thaw`].
///
/// Each session runs its scans one after another, and a scan takes one listing.
/// Listing number 1 after the thaw is therefore a post-thaw scan, and listing
/// number 2 starting means that scan has finished.
async fn wait_for_scans_after_thaw(inspector: &PollBlindInspector) {
    let thawed_at = inspector.listings();
    wait_until("a procwatch scan to complete after the thaw", || async {
        (inspector.listings() >= thawed_at + 2).then_some(())
    })
    .await;
}

/// Waits for the next event named `name` and returns the session it carries.
///
/// A lagged receiver panics: a skipped event could be the one being awaited.
async fn next_session_event(
    events: &mut tokio::sync::broadcast::Receiver<protocol::Event>,
    name: &str,
) -> SessionInfo {
    loop {
        match events.recv().await {
            Ok(event) if event.event() == name => {
                let session = event
                    .payload()
                    .get("session")
                    .cloned()
                    .expect("the lifecycle event carries a session");
                return serde_json::from_value(session).expect("decode the event session");
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                panic!("the event receiver lagged and skipped {skipped} events")
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                panic!("the registry event channel closed")
            }
        }
    }
}

async fn wait_for_external_pid(registry: &SessionRegistry, pid: u32) -> SessionInfo {
    wait_until("the external observer to list the fake agent", || async {
        registry
            .list()
            .await
            .into_iter()
            .find(|session| session.pid == pid)
    })
    .await
}

async fn assert_external_detection_unavailable(registry: &SessionRegistry, id: &SessionId) {
    let error = registry
        .detection(id)
        .await
        .expect_err("external sessions have no managed detector");
    assert_eq!(
        error,
        protocol::ProtocolError::session_has_no_managed_terminal()
    );
}

/// Waits for the removal event of the external session `id`.
///
/// The proof comes from the event itself, never from a listing that a later
/// sweep could overwrite; `events` must be subscribed before the exit is
/// triggered.
async fn wait_for_external_gone(
    id: &SessionId,
    events: &mut tokio::sync::broadcast::Receiver<protocol::Event>,
) {
    guard("the removal event of the external session", async {
        loop {
            let removed = next_session_event(events, event::SESSION_REMOVED).await;
            if removed.id == *id {
                return;
            }
        }
    })
    .await;
}

fn kill9(pid: u32) {
    let status = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -9 {pid} failed");
}
