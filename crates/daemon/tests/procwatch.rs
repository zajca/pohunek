// Rust guideline compliant 2026-07-07

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pohunek_daemon::procwatch::{LinuxInspector, ProcessInspector};
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use protocol::{
    AgentKind, CwdSource, SessionAttachParams, SessionId, SessionInfo, SessionInputParams,
    SessionNewParams, ENV_DAEMON_ID, ENV_SESSION_ID,
};

const TEST_COLS: u16 = 80;
const TEST_ROWS: u16 = 24;
/// Poll interval used by the integration test.
///
/// It is intentionally long enough that a clear observed well below this bound
/// proves the pidfd exit path fired instead of waiting for the next poll tick.
const TEST_PROCWATCH_POLL: Duration = Duration::from_secs(2);
/// Upper bound for the initial process-discovery wait.
///
/// The watcher ticks immediately after spawn; this timeout leaves room for
/// subprocess-worker spawn plus PTY startup on a heavily loaded parallel test
/// run while still bounding a broken descendant walk. It is a liveness bound
/// only (the event-driven `EXIT_EVENT_TIMEOUT` below is what proves pidfd-driven
/// behavior), so a generous value costs nothing on success and only absorbs
/// scheduling latency when the whole workspace runs concurrently.
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound for pidfd-driven release after `kill -9`.
///
/// This is below [`TEST_PROCWATCH_POLL`], so success demonstrates event-driven
/// exit handling rather than poll cleanup.
const EXIT_EVENT_TIMEOUT: Duration = Duration::from_millis(900);
/// Poll interval for the cwd-tracking integration test.
const CWD_PROCWATCH_POLL: Duration = Duration::from_millis(150);
/// Upper bound for observing a shell `cd` through procwatch.
///
/// A liveness bound: procwatch reflects the new cwd on its next poll, so this
/// only needs to exceed a poll interval plus scheduling latency. Kept generous
/// so a heavily loaded parallel test run cannot starve the poll tick.
const CWD_UPDATE_TIMEOUT: Duration = Duration::from_secs(4);
/// Lightweight inspect polling cadence while waiting for cwd updates.
const CWD_WAIT_POLL: Duration = Duration::from_millis(20);
/// Poll interval for the external observer integration test.
const EXTERNAL_PROCWATCH_POLL: Duration = Duration::from_secs(2);
/// Upper bound for observing an external agent process.
const EXTERNAL_OBSERVE_TIMEOUT: Duration = Duration::from_secs(2);
/// Lightweight polling cadence while waiting for external entries.
const EXTERNAL_WAIT_POLL: Duration = Duration::from_millis(20);

static EXTERNAL_ENV_LOCK: Mutex<()> = Mutex::new(());
static POHUNEK_ENV_LOCK: Mutex<()> = Mutex::new(());
static WORKER_HOME_COUNTER: AtomicU64 = AtomicU64::new(0);

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
fn worker_backed_registry(mut config: SessionRegistryConfig) -> SessionRegistry {
    let worker_home = std::env::temp_dir().join(format!(
        "pw-p-{}-{}",
        std::process::id(),
        WORKER_HOME_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let worker_environment = SubprocessWorkerEnvironment {
        runtime_home: worker_home.join("runtime"),
        state_home: worker_home.join("state"),
        data_home: worker_home.join("data"),
        config_home: worker_home.join("config"),
        cache_home: worker_home.join("cache"),
        daemon_socket: worker_home.join("daemon.sock"),
    };
    config.worker_runtime_root = Some(worker_environment.runtime_home.join("pohunek/workers"));
    config.worker_state_root = Some(worker_environment.state_home.join("pohunek/workers"));
    let launcher = Arc::new(SubprocessWorkerLauncher::new(
        worker_binary(),
        worker_environment,
    ));
    SessionRegistry::new_with_launcher_and_inspector(
        config,
        launcher,
        Arc::new(LinuxInspector::new()),
    )
}

/// Locate the real `pohunek-sessiond` worker binary built alongside this test.
fn worker_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("POHUNEK_WORKER_BIN") {
        return PathBuf::from(path);
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("daemon crate is inside workspace")
        .to_path_buf();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace.join("target"), PathBuf::from);
    let binary = target.join("debug/pohunek-sessiond");
    assert!(
        binary.is_file(),
        "build the real worker first with `cargo build -p pohunek-session-worker --bin pohunek-sessiond`, or set POHUNEK_WORKER_BIN"
    );
    binary
}

#[tokio::test]
async fn procwatch_auto_reports_and_pidfd_clears_real_child_agent() {
    if !pidfd_is_available() {
        return;
    }

    let dir = temp_dir("procwatch-real-child");
    let fake_codex = dir.join("codex");
    let pid_file = dir.join("agent.pid");
    symlink_sleep_as(&fake_codex);
    let script = format!(
        "{} 60 & echo $! > {}; wait $!; sleep 30",
        fake_codex.display(),
        pid_file.display()
    );
    let registry = worker_backed_registry(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        stop_grace: Duration::from_millis(50),
        procwatch_poll: TEST_PROCWATCH_POLL,
        ..SessionRegistryConfig::default()
    });
    let created = {
        let _env = PohunekEnvGuard::clear();
        registry
            .create(SessionNewParams {
                name: Some("procwatch-real-child".to_owned()),
                agent: "shell".to_owned(),
                cwd: Some(dir.clone()),
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

    let observed =
        wait_for_active_pid(&registry, &created.id, Some(child_pid), OBSERVE_TIMEOUT).await;
    assert_eq!(observed.active_agent.as_deref(), Some("codex"));
    assert_eq!(observed.active_agent_base, Some(AgentKind::Codex));

    kill9(child_pid);
    let started = Instant::now();
    let cleared = wait_for_active_pid(&registry, &created.id, None, EXIT_EVENT_TIMEOUT).await;

    assert_eq!(cleared.active_agent, None);
    assert!(
        started.elapsed() < TEST_PROCWATCH_POLL,
        "active agent cleared only after the poll interval"
    );
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_updates_cwd_after_shell_cd() {
    let start_dir = temp_dir("procwatch-cwd-start");
    let target_dir = temp_dir("procwatch-cwd-target");
    let registry = worker_backed_registry(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<String>()),
        stop_grace: Duration::from_millis(50),
        procwatch_poll: CWD_PROCWATCH_POLL,
        ..SessionRegistryConfig::default()
    });
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

    let updated = wait_for_cwd(&registry, &created.id, &target_dir, CWD_UPDATE_TIMEOUT).await;
    assert_eq!(updated.cwd_source, Some(CwdSource::Procwatch));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn external_observer_reports_fake_agent_and_pidfd_removes_it() {
    if !pidfd_is_available() {
        return;
    }

    let _env = ExternalEnvGuard::set();
    let claude_config = temp_dir("external-claude-config");
    let codex_home = temp_dir("external-codex-home");
    let claude_projects = claude_config.join("projects").join("work");
    let codex_sessions = codex_home.join("sessions");
    fs::create_dir_all(&claude_projects).expect("create claude projects");
    fs::create_dir_all(&codex_sessions).expect("create codex sessions");
    std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config);
    std::env::set_var("CODEX_HOME", &codex_home);

    let work_dir = temp_dir("external-agent-cwd");
    let fake_claude = temp_dir("external-bin").join("claude");
    symlink_sleep_as(&fake_claude);
    let registry = SessionRegistry::new(SessionRegistryConfig {
        observe_external_agents: true,
        procwatch_poll: EXTERNAL_PROCWATCH_POLL,
        ..SessionRegistryConfig::default()
    });
    let mut child = spawn_fake_agent(&fake_claude, &work_dir);
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

    let observed = wait_for_external_pid(&registry, child_pid, EXTERNAL_OBSERVE_TIMEOUT).await;
    assert_eq!(observed.id.0, format!("ext-{child_pid}"));
    assert_eq!(observed.external, Some(true));
    assert_eq!(observed.agent, "claude");
    assert_eq!(observed.agent_base, AgentKind::Claude);
    assert_eq!(observed.native_session_id.as_deref(), Some("native-ext"));
    assert_eq!(
        observed.native_session_path.as_deref(),
        Some(transcript.to_string_lossy().as_ref())
    );

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

fn pidfd_is_available() -> bool {
    let inspector = LinuxInspector::new();
    match inspector.exit_watch(std::process::id()) {
        Ok(_) => true,
        Err(err) if err.raw_os_error() == Some(libc::ENOSYS) => false,
        Err(err) => panic!("pidfd_open failed unexpectedly: {err}"),
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pohunek-{tag}-{}-{}",
        std::process::id(),
        unix_nanos()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos()
}

fn symlink_sleep_as(path: &Path) {
    let sleep = which_sleep();
    std::os::unix::fs::symlink(sleep, path).expect("symlink fake codex");
}

fn spawn_fake_agent(program: &Path, cwd: &Path) -> Child {
    Command::new(program)
        .arg("60")
        .current_dir(cwd)
        // Scrub the pohunek ownership markers the test runner may itself carry
        // (e.g. when the suite runs inside a pohunek-managed session): a marked
        // process is treated as another daemon's agent and would never surface
        // as external. The fake agent models a genuinely external process.
        .env_remove(ENV_DAEMON_ID)
        .env_remove(ENV_SESSION_ID)
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

async fn wait_for_cwd(
    registry: &SessionRegistry,
    id: &SessionId,
    expected: &Path,
    timeout: Duration,
) -> SessionInfo {
    let expected = fs::canonicalize(expected).expect("canonical expected cwd");
    let deadline = Instant::now() + timeout;
    loop {
        let info = registry.inspect(id).await.expect("inspect session");
        if info.cwd == expected {
            return info;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for cwd {}",
            expected.display()
        );
        tokio::time::sleep(CWD_WAIT_POLL).await;
    }
}

async fn wait_for_pid_file(path: &Path) -> u32 {
    for _ in 0..100 {
        if let Ok(contents) = fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {}", path.display());
}

async fn wait_for_active_pid(
    registry: &SessionRegistry,
    id: &SessionId,
    expected: Option<u32>,
    timeout: Duration,
) -> SessionInfo {
    let deadline = Instant::now() + timeout;
    loop {
        let info = registry.inspect(id).await.expect("inspect session");
        if info.active_agent_pid == expected {
            return info;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for active_agent_pid {expected:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_external_pid(
    registry: &SessionRegistry,
    pid: u32,
    timeout: Duration,
) -> SessionInfo {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(info) = registry
            .list()
            .await
            .into_iter()
            .find(|session| session.pid == pid)
        {
            return info;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for external pid {pid}"
        );
        tokio::time::sleep(EXTERNAL_WAIT_POLL).await;
    }
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
