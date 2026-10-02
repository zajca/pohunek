// Rust guideline compliant 2026-09-01

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pohunek_daemon::procwatch::{HostInspector, ProcessInspector};
use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::worker_binary;
use protocol::{
    AgentKind, CwdSource, SessionAttachParams, SessionId, SessionInfo, SessionInputParams,
    SessionNewParams, ENV_DAEMON_ID, ENV_SESSION_ID,
};

// Sessions use the production stop grace, `SessionRegistryConfig::default()`:
// a shorter grace reaches the forced-stop path on a loaded host, and no test
// here is about the grace.
const TEST_COLS: u16 = 80;
const TEST_ROWS: u16 = 24;
/// Poll interval used by the integration test.
///
/// It is intentionally long enough that a clear observed well below this bound
/// proves the pidfd exit path fired instead of waiting for the next poll tick.
const TEST_PROCWATCH_POLL: Duration = Duration::from_secs(2);
/// Upper bound for pidfd-driven release after `kill -9`.
///
/// This is below [`TEST_PROCWATCH_POLL`], so success demonstrates event-driven
/// exit handling rather than poll cleanup.
const EXIT_EVENT_TIMEOUT: Duration = Duration::from_millis(900);
/// Poll interval for the cwd-tracking integration test.
const CWD_PROCWATCH_POLL: Duration = Duration::from_millis(150);
/// Poll interval for the external observer integration test.
const EXTERNAL_PROCWATCH_POLL: Duration = Duration::from_secs(2);
/// Lightweight polling cadence while waiting for external entries.
const EXTERNAL_WAIT_POLL: Duration = Duration::from_millis(20);

static EXTERNAL_ENV_LOCK: Mutex<()> = Mutex::new(());
static POHUNEK_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Clears `POHUNEK_DAEMON_ID`/`POHUNEK_SESSION_ID` for the scope of spawning a
/// worker-backed test session.
///
/// `cargo test` may itself run inside a real pohunek-managed session (e.g. a
/// dev loop invoked from within this very repo's own pohunek session), which
/// sets both on this process's environment. `SubprocessWorkerLauncher::launch`
/// (`crates/daemon/src/runtime/launcher.rs`) spawns the worker subprocess
/// without scrubbing the ambient environment, so those stale markers would
/// otherwise leak all the way down into the freshly spawned worker and the
/// PTY child it forks — stamping the test's own fake agent with a foreign
/// daemon id that `is_foreign_owned_agent`
/// (`crates/daemon/src/session/procwatch.rs`) then correctly refuses to
/// adopt, so it never surfaces as this test's observed agent. Clearing both
/// vars before `registry.create` breaks the leak at its source; mirrors this
/// file's existing `ExternalEnvGuard` pattern for `CLAUDE_CONFIG_DIR`/
/// `CODEX_HOME`.
struct PohunekEnvGuard {
    _lock: MutexGuard<'static, ()>,
    daemon_id: Option<std::ffi::OsString>,
    session_id: Option<std::ffi::OsString>,
}

impl PohunekEnvGuard {
    fn clear() -> Self {
        let lock = POHUNEK_ENV_LOCK.lock().expect("pohunek env lock");
        let daemon_id = std::env::var_os(ENV_DAEMON_ID);
        let session_id = std::env::var_os(ENV_SESSION_ID);
        std::env::remove_var(ENV_DAEMON_ID);
        std::env::remove_var(ENV_SESSION_ID);
        Self {
            _lock: lock,
            daemon_id,
            session_id,
        }
    }
}

impl Drop for PohunekEnvGuard {
    fn drop(&mut self) {
        restore_env(ENV_DAEMON_ID, self.daemon_id.clone());
        restore_env(ENV_SESSION_ID, self.session_id.clone());
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
fn worker_backed_registry(env: &TestEnv, mut config: SessionRegistryConfig) -> SessionRegistry {
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
    let launcher = Arc::new(SubprocessWorkerLauncher::new());
    SessionRegistry::new_with_launcher_and_inspector(
        config,
        launcher,
        Arc::new(HostInspector::new()),
    )
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
        "{} 60 & echo $! > {}; wait $!; sleep 30",
        fake_codex.display(),
        pid_file.display()
    );
    let registry = worker_backed_registry(
        &env,
        SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
            procwatch_poll: TEST_PROCWATCH_POLL,
            ..SessionRegistryConfig::default()
        },
    );
    let created = {
        let _env = PohunekEnvGuard::clear();
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
    assert_eq!(observed.active_agent_base, Some(AgentKind::Codex));

    kill9(child_pid);
    let started = Instant::now();
    let cleared = wait_for_cleared_agent(&registry, &created.id, EXIT_EVENT_TIMEOUT).await;

    assert_eq!(cleared.active_agent, None);
    assert!(
        started.elapsed() < TEST_PROCWATCH_POLL,
        "active agent cleared only after the poll interval"
    );
    let _ = registry.stop(&created.id).await;
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

    let _env = ExternalEnvGuard::set();
    let env = TestEnv::new().expect("create the test environment");
    let claude_config = env.config_home().join("claude");
    let codex_home = env.config_home().join("codex");
    let claude_projects = claude_config.join("projects").join("work");
    let codex_sessions = codex_home.join("sessions");
    fs::create_dir_all(&claude_projects).expect("create claude projects");
    fs::create_dir_all(&codex_sessions).expect("create codex sessions");
    std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config);
    std::env::set_var("CODEX_HOME", &codex_home);

    let work_dir = env.cwd().to_path_buf();
    let bin_dir = env.data_home().join("bin");
    fs::create_dir(&bin_dir).expect("create the fake agent directory");
    let fake_claude = bin_dir.join("claude");
    symlink_sleep_as(&fake_claude);
    let registry = SessionRegistry::new(SessionRegistryConfig {
        observe_external_agents: true,
        procwatch_poll: EXTERNAL_PROCWATCH_POLL,
        ..SessionRegistryConfig::default()
    });
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
    assert_eq!(observed.agent_base, AgentKind::Claude);
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

    kill9(child_pid);
    let started = Instant::now();
    wait_for_external_gone(&registry, child_pid, EXIT_EVENT_TIMEOUT).await;
    assert!(
        started.elapsed() < EXTERNAL_PROCWATCH_POLL,
        "external agent disappeared only after the poll interval"
    );
    let _ = child.wait();
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

/// Waits until the registry reports no active agent, within `timeout`.
///
/// The bound is the contract under test: the release must come from the exit
/// watch, which is faster than the poll interval.
async fn wait_for_cleared_agent(
    registry: &SessionRegistry,
    id: &SessionId,
    timeout: Duration,
) -> SessionInfo {
    let deadline = Instant::now() + timeout;
    loop {
        let info = registry.inspect(id).await.expect("inspect session");
        if info.active_agent_pid.is_none() {
            return info;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the active agent to clear"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
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

async fn wait_for_external_gone(registry: &SessionRegistry, pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let present = registry
            .list()
            .await
            .into_iter()
            .any(|session| session.pid == pid);
        if !present {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for external pid {pid} to disappear"
        );
        tokio::time::sleep(EXTERNAL_WAIT_POLL).await;
    }
}

fn kill9(pid: u32) {
    let status = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -9 {pid} failed");
}

struct ExternalEnvGuard {
    _lock: MutexGuard<'static, ()>,
    claude_config_dir: Option<std::ffi::OsString>,
    codex_home: Option<std::ffi::OsString>,
}

impl ExternalEnvGuard {
    fn set() -> Self {
        let lock = EXTERNAL_ENV_LOCK.lock().expect("external env lock");
        let claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");
        let codex_home = std::env::var_os("CODEX_HOME");
        Self {
            _lock: lock,
            claude_config_dir,
            codex_home,
        }
    }
}

impl Drop for ExternalEnvGuard {
    fn drop(&mut self) {
        restore_env("CLAUDE_CONFIG_DIR", self.claude_config_dir.clone());
        restore_env("CODEX_HOME", self.codex_home.clone());
    }
}

fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
    if let Some(value) = value {
        std::env::set_var(key, value);
    } else {
        std::env::remove_var(key);
    }
}
