//! `pohunekd` — the pohunek host daemon binary.
//!
//! Milestone 2: resolve XDG paths, initialize JSON logging, acquire the
//! single-instance lock, bind the control socket (with stale-socket recovery and
//! owner-private permissions), and serve `daemon.health` until SIGINT/SIGTERM.
//!
//! The daemon binds one TCP control listener for each configured overlay and
//! serves them alongside the local Unix socket under one shutdown. A provider
//! being temporarily unavailable is not an error: the daemon stays reachable
//! through healthy transports while retrying that listener.

use std::future::Future;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use overlay::{ConfiguredTransport, OverlayRegistry};
use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo, RemoteServer};
use pohunek_daemon::discovery::DiscoveryCache;
use pohunek_daemon::events::{spawn_drain, EventLog};
use pohunek_daemon::lock::InstanceLock;
use pohunek_daemon::notifications::{
    AttentionCoordinator, NotificationProjector, NotificationRetentionTask, NotificationService,
    NOTIFICATIONS_SUBDIR,
};
use pohunek_daemon::runtime::{
    SubprocessWorkerEnvironment, SubprocessWorkerLauncher, UnitTemplate, WorkerLauncher,
    DEFAULT_WORKER_UNIT_TEMPLATE,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use pohunek_daemon::{logging, DaemonError, Paths, DAEMON_VERSION};

/// File name of the unified logical-session metadata store under the data dir.
const STORE_NAME: &str = "metadata.jsonl";

/// Subdirectory under the data dir holding per-session git worktrees.
const WORKTREES_SUBDIR: &str = "worktrees";

/// Subdirectory under the data dir holding the append-only event log.
const EVENTS_SUBDIR: &str = "events";
/// Per-session worker socket root under the owner-private runtime directory.
const WORKERS_SUBDIR: &str = pohunek_paths::WORKERS_SUBDIR;

/// Env var enabling opt-in observation of agents outside pohunek-owned PTYs.
const OBSERVE_EXTERNAL_AGENTS_ENV: &str = "POHUNEK_OBSERVE_EXTERNAL_AGENTS";
/// Optional worker template override for isolated systemd integration tests.
const WORKER_UNIT_TEMPLATE_ENV: &str = "POHUNEK_WORKER_UNIT_TEMPLATE";
/// Selects how the daemon activates durable session workers: `systemd` (default)
/// or `subprocess`. `subprocess` spawns `pohunek-sessiond` as a direct child, for
/// headless environments (CI, containers) with no systemd user manager.
const WORKER_LAUNCHER_ENV: &str = "POHUNEK_WORKER_LAUNCHER";
/// Overrides the durable worker binary path used by the subprocess launcher.
/// When unset, the daemon uses the `pohunek-sessiond` co-located next to itself.
const WORKER_BIN_ENV: &str = "POHUNEK_WORKER_BIN";
/// Durable worker binary name, expected next to the daemon executable.
const WORKER_BINARY_NAME: &str = "pohunek-sessiond";

/// Maximum time to let event-log drains flush on daemon shutdown.
///
/// Event log writes are local owner-private JSONL appends and should finish
/// quickly. Bounding shutdown prevents a wedged filesystem from hanging the
/// daemon indefinitely while still giving buffered audit/debug events a chance
/// to reach disk.
const EVENT_LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Initial delay between attempts to enable an optional remote listener.
///
/// An overlay can report incomplete state while its local service and interface
/// are starting. Five seconds avoids repeatedly querying the provider while
/// making a newly ready host discoverable without an operator restart.
const REMOTE_BIND_INITIAL_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Longest delay between remote-listener retry attempts.
///
/// A local-only daemon is valid when an overlay is not ready, so retries back
/// off to avoid continuously querying its state and filling logs. Five minutes
/// still recovers reasonably quickly after a delayed login or interface setup.
const REMOTE_BIND_MAX_RETRY_INTERVAL: Duration = Duration::from_mins(5);

#[tokio::main]
async fn main() -> ExitCode {
    // The startup future is large (reconciliation, listeners, event logs); box
    // it so it lives on the heap instead of inflating the `main` task frame.
    match Box::pin(run()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Logging may not be initialized yet (e.g. missing env), so also
            // print to stderr to guarantee the operator sees the failure.
            eprintln!("pohunekd: fatal: {err}");
            error!(error = %err, "daemon exited with error");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), DaemonError> {
    // 1. Resolve all paths up front; fail fast on missing required env.
    let paths = Paths::resolve()?;

    // 2. Initialize structured logging to the state log dir.
    let _log_guard = logging::init(&paths.log_dir)?;
    info!(
        daemon_version = DAEMON_VERSION,
        runtime_dir = %paths.runtime_dir.display(),
        log_dir = %paths.log_dir.display(),
        "pohunekd starting"
    );

    // 3. Ensure the runtime dir exists (0700) before taking the lock in it.
    //    The control server enforces the same on bind, but the lock lives there
    //    too and is acquired first. The data dir holds logical session state.
    ensure_private_dir(&paths.runtime_dir)?;
    ensure_private_dir(&paths.data_dir)?;
    ensure_private_dir(&paths.runtime_dir.join(WORKERS_SUBDIR))?;
    ensure_private_dir(&paths.state_dir.join(WORKERS_SUBDIR))?;

    // 4. Single-instance lock: a second daemon refuses to start.
    let _lock = InstanceLock::acquire(&paths.lock)?;
    info!(lock = %paths.lock.display(), "acquired single-instance lock");

    // 5. Build the session registry with the hook-handshake socket path, the
    //    unified metadata store (logical sessions and related bindings), the
    //    worktrees root, and the event-log directory, so spawned agents can
    //    report native identity, logical sessions survive a restart, a
    //    repo+branch session binds a dedicated worktree, and the lifecycle is
    //    recorded to the append-only event log.
    let config = SessionRegistryConfig {
        socket_path: Some(paths.socket.clone()),
        store_path: Some(paths.data_dir.join(STORE_NAME)),
        worktree_root: Some(paths.data_dir.join(WORKTREES_SUBDIR)),
        event_log_dir: Some(paths.data_dir.join(EVENTS_SUBDIR)),
        log_dir: Some(paths.log_dir.clone()),
        // Slice 0 owns this line: B3 (host-global hooks) and C1 (agent profiles)
        // read it through SessionRegistry::config_dir() / derive off it — they must
        // NOT re-add config_dir here.
        config_dir: Some(paths.config_dir.clone()),
        // Part C: host agent profiles live under <config_dir>/agents.
        agents_dir: Some(paths.config_dir.join("agents")),
        observe_external_agents: env_bool(OBSERVE_EXTERNAL_AGENTS_ENV)?,
        worker_runtime_root: Some(paths.runtime_dir.join(WORKERS_SUBDIR)),
        worker_state_root: Some(paths.state_dir.join(WORKERS_SUBDIR)),
        worker_unit_template: worker_unit_template()?,
        ..SessionRegistryConfig::default()
    };
    let sessions = build_session_registry(config, &paths)?;
    let notifications =
        NotificationService::open(&paths.data_dir).map_err(|source| DaemonError::Directory {
            path: paths.data_dir.join(NOTIFICATIONS_SUBDIR),
            source: std::io::Error::other(source),
        })?;

    // 6. Start the append-only event log before anything emits, so worker
    //    reconciliation events are captured too. The log drains
    //    both session and notification control-plane events into one file.
    let event_logs = spawn_event_logs(
        &paths.data_dir.join(EVENTS_SUBDIR),
        &sessions,
        &notifications,
    )?;
    let notification_retention = NotificationRetentionTask::spawn(notifications.clone());
    // Spawn the debounce coordinator: it owns the lifecycle of session-scoped
    // agent_blocked/approval_required and turn_completed notifications, holding
    // them for the policy debounce window so transient signals never surface.
    // Both producers (the notification.create handler and the projector) route
    // debounced notifications through its clonable command handle.
    let (attention_coordinator, attention_task) =
        AttentionCoordinator::spawn(notifications.clone());
    let notification_projector = NotificationProjector::spawn(
        &sessions,
        notifications.clone(),
        attention_coordinator.clone(),
    );
    sessions.spawn_agent_state_hooks();

    // 7. Adopt exact surviving worker runtimes before exposing the public API.
    //    Reconciliation never invokes provider-native resume.
    Box::pin(sessions.reconcile_workers())
        .await
        .map_err(DaemonError::Reconcile)?;

    // 8. Bind the control socket (stale-socket recovery + 0600).
    let registry = netbird::configured_registry()?;
    let health = HealthInfo::new(DAEMON_VERSION);
    let discovery = DiscoveryCache::new(registry.clone());
    let state = DaemonState::new_with_discovery(health, sessions.clone(), discovery.clone())
        .with_notifications(notifications.clone())
        .with_attention_coordinator(attention_coordinator.clone());
    let server = ControlServer::bind_with_state(&paths.socket, state).await?;
    info!(socket = %server.socket_path().display(), "ready; serving control protocol");

    // 9. Keep attempting to bind every configured overlay listener alongside
    //    the Unix socket. Providers may become ready after this daemon starts.
    let remote_state = DaemonState::new_with_discovery(
        HealthInfo::new(DAEMON_VERSION),
        sessions.clone(),
        discovery,
    )
    .with_notifications(notifications.clone())
    .with_attention_coordinator(attention_coordinator.clone());
    // Reconciliation and every required local listener are ready. A manual
    // foreground launch has no NOTIFY_SOCKET and this is a no-op.
    pohunek_daemon::notify::ready()?;

    // 10. Serve both transports under ONE shutdown signal. A small task awaits
    //     the OS signal once and cancels each server, including a remote
    //     supervisor that is waiting to retry its bind.
    let shutdown = CancellationToken::new();
    let shutdown_sessions = sessions.clone();
    let shutdown_token = shutdown.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_sessions.begin_daemon_shutdown();
        shutdown_token.cancel();
    });

    let unix_shutdown = shutdown.clone();
    let unix_serve = server.serve(async move {
        unix_shutdown.cancelled().await;
    });
    let remote_serve = serve_remote(remote_state, &registry, shutdown);
    tokio::join!(unix_serve, remote_serve);

    // 11. Flush the append-only event log before exit so events buffered at
    //     shutdown are not lost (bounded so a wedged write cannot hang exit).
    //     Stop the projector before the coordinator so any last defers/resolves
    //     it drains are still accepted, then drop the coordinator's pending
    //     (in-memory, deliberately not persisted).
    notification_retention.shutdown().await;
    notification_projector.shutdown().await;
    attention_task.shutdown().await;
    sessions.shutdown_agent_state_hooks().await;
    shutdown_event_logs(event_logs).await;

    info!("pohunekd stopped");
    Ok(())
}

fn env_bool(var: &str) -> Result<bool, DaemonError> {
    let Some(value) = std::env::var_os(var) else {
        return Ok(false);
    };
    let Some(value) = value.to_str() else {
        return Err(DaemonError::InvalidEnv {
            var: var.to_owned(),
            value: "<non-utf8>".to_owned(),
            expected: "true/false, 1/0, yes/no, or on/off",
        });
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        _ => Err(DaemonError::InvalidEnv {
            var: var.to_owned(),
            value: value.to_owned(),
            expected: "true/false, 1/0, yes/no, or on/off",
        }),
    }
}

fn worker_unit_template() -> Result<UnitTemplate, DaemonError> {
    let value = std::env::var(WORKER_UNIT_TEMPLATE_ENV)
        .unwrap_or_else(|_| DEFAULT_WORKER_UNIT_TEMPLATE.to_owned());
    UnitTemplate::parse(&value).map_err(|_template_error| DaemonError::InvalidEnv {
        var: WORKER_UNIT_TEMPLATE_ENV.to_owned(),
        value,
        expected: "an ASCII systemd template like pohunek-session@.service",
    })
}

/// Durable worker activation backend selected by [`WORKER_LAUNCHER_ENV`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerLauncherMode {
    /// Native systemd user-manager units (production default).
    Systemd,
    /// Direct child `pohunek-sessiond` processes (headless / CI).
    Subprocess,
}

/// Builds the session registry with the durable-worker launcher selected by
/// [`WORKER_LAUNCHER_ENV`]: the systemd user manager by default, or a direct
/// `pohunek-sessiond` child process for headless environments (CI, containers)
/// that have no systemd user manager or session D-Bus.
fn build_session_registry(
    config: SessionRegistryConfig,
    paths: &Paths,
) -> Result<SessionRegistry, DaemonError> {
    match worker_launcher_mode()? {
        WorkerLauncherMode::Systemd => {
            SessionRegistry::new_production(config).map_err(DaemonError::Reconcile)
        }
        WorkerLauncherMode::Subprocess => Ok(SessionRegistry::new_with_launcher_and_inspector(
            config,
            subprocess_worker_launcher(paths)?,
            Arc::new(pohunek_daemon::procwatch::LinuxInspector::new()),
        )),
    }
}

fn worker_launcher_mode() -> Result<WorkerLauncherMode, DaemonError> {
    let Some(value) = std::env::var_os(WORKER_LAUNCHER_ENV) else {
        return Ok(WorkerLauncherMode::Systemd);
    };
    match value
        .to_str()
        .map(|raw| raw.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("" | "systemd") => Ok(WorkerLauncherMode::Systemd),
        Some("subprocess") => Ok(WorkerLauncherMode::Subprocess),
        Some(other) => Err(DaemonError::InvalidEnv {
            var: WORKER_LAUNCHER_ENV.to_owned(),
            value: other.to_owned(),
            expected: "systemd or subprocess",
        }),
        None => Err(DaemonError::InvalidEnv {
            var: WORKER_LAUNCHER_ENV.to_owned(),
            value: "<non-utf8>".to_owned(),
            expected: "systemd or subprocess",
        }),
    }
}

/// Builds a subprocess launcher rooted in the daemon's own XDG base directories,
/// so a spawned worker resolves the exact runtime/state paths the daemon expects.
fn subprocess_worker_launcher(paths: &Paths) -> Result<Arc<dyn WorkerLauncher>, DaemonError> {
    let environment = SubprocessWorkerEnvironment {
        runtime_home: xdg_base(&paths.runtime_dir)?,
        state_home: xdg_base(&paths.state_dir)?,
        data_home: xdg_base(&paths.data_dir)?,
        config_home: paths.config_home.clone(),
        cache_home: xdg_base(&paths.cache_dir)?,
        daemon_socket: paths.socket.clone(),
    };
    Ok(Arc::new(SubprocessWorkerLauncher::new(
        resolve_worker_binary()?,
        environment,
    )))
}

/// Resolves the durable worker binary: [`WORKER_BIN_ENV`] when set, otherwise the
/// `pohunek-sessiond` co-located next to the running daemon executable.
fn resolve_worker_binary() -> Result<std::path::PathBuf, DaemonError> {
    if let Some(path) = std::env::var_os(WORKER_BIN_ENV) {
        return Ok(std::path::PathBuf::from(path));
    }
    let exe = std::env::current_exe()?;
    exe.parent()
        .map(|dir| dir.join(WORKER_BINARY_NAME))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "daemon executable has no parent directory to locate the worker binary",
            )
            .into()
        })
}

/// Recovers the XDG base directory that produced a `<base>/pohunek` daemon path,
/// so the worker (which re-appends `pohunek/...`) lands on the same tree.
fn xdg_base(app_scoped_dir: &std::path::Path) -> Result<std::path::PathBuf, DaemonError> {
    app_scoped_dir
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "daemon path {} has no XDG base parent",
                    app_scoped_dir.display()
                ),
            )
            .into()
        })
}

#[derive(Debug)]
struct EventLogDrains {
    shutdown: CancellationToken,
    handles: Vec<JoinHandle<()>>,
}

fn spawn_event_logs(
    events_dir: &std::path::Path,
    sessions: &SessionRegistry,
    notifications: &NotificationService,
) -> Result<EventLogDrains, DaemonError> {
    let log = Arc::new(
        EventLog::open(events_dir).map_err(|source| DaemonError::Directory {
            path: events_dir.to_path_buf(),
            source,
        })?,
    );
    let shutdown = CancellationToken::new();
    let handles = vec![
        spawn_drain(Arc::clone(&log), sessions.subscribe(), shutdown.clone()),
        spawn_drain(log, notifications.subscribe(), shutdown.clone()),
    ];
    Ok(EventLogDrains { shutdown, handles })
}

async fn shutdown_event_logs(drains: EventLogDrains) {
    drains.shutdown.cancel();
    for handle in drains.handles {
        if tokio::time::timeout(EVENT_LOG_FLUSH_TIMEOUT, handle)
            .await
            .is_err()
        {
            warn!("event-log drain did not finish flushing within the shutdown timeout");
        }
    }
}

/// Repeatedly bind and serve one optional overlay TCP control listener.
///
/// A `NetBird` daemon may start after `pohunekd`, or temporarily emit an
/// incomplete status snapshot while it initializes. Retrying keeps the local
/// control plane available immediately while allowing remote discovery to
/// recover without an operator restart.
async fn serve_remote_with_retry<F, Fut>(
    mut bind: F,
    shutdown: CancellationToken,
    retry_interval: Duration,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = RemoteBind>,
{
    let mut retry_interval = retry_interval;
    loop {
        let attempt = tokio::select! {
            () = shutdown.cancelled() => return,
            attempt = bind() => attempt,
        };
        match attempt {
            RemoteBind::Bound(remote) => {
                remote
                    .serve(async move {
                        shutdown.cancelled().await;
                    })
                    .await;
                return;
            }
            RemoteBind::Disabled => {
                shutdown.cancelled().await;
                return;
            }
            RemoteBind::Retry => {}
        }

        debug!(
            retry_after_secs = retry_interval.as_secs(),
            "remote control listener unavailable; retrying after delay"
        );
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(retry_interval) => {}
        }
        retry_interval = next_retry_interval(retry_interval);
    }
}

/// Increase a retry delay without exceeding the local-only polling ceiling.
#[must_use]
fn next_retry_interval(current: Duration) -> Duration {
    current
        .saturating_mul(2)
        .min(REMOTE_BIND_MAX_RETRY_INTERVAL)
}

/// Serve the remote transport when its port configuration is valid.
async fn serve_remote(state: DaemonState, registry: &OverlayRegistry, shutdown: CancellationToken) {
    let mut tasks = Vec::with_capacity(registry.entries().len());
    for configured in registry.entries() {
        let configured = configured.clone();
        let state = state.clone();
        let shutdown = shutdown.clone();
        tasks.push(tokio::spawn(async move {
            serve_remote_with_retry(
                || bind_remote_server(state.clone(), configured.clone()),
                shutdown,
                REMOTE_BIND_INITIAL_RETRY_INTERVAL,
            )
            .await;
        }));
    }

    if tasks.is_empty() {
        shutdown.cancelled().await;
        return;
    }
    for task in tasks {
        let _ = task.await;
    }
}

/// Bind one overlay's TCP control listener once.
///
/// Queries the overlay for its explicit listener address and binds a
/// [`RemoteServer`] on that overlay's configured port. Missing CLI disables
/// that listener; unavailable state, a missing self address, and bind failures
/// are retried by the caller.
async fn bind_remote_server(state: DaemonState, configured: ConfiguredTransport) -> RemoteBind {
    let overlay_id = configured.id().clone();
    let port = configured.port();
    let transport = Arc::clone(configured.transport());
    let address = match transport.listener_addr().await {
        Ok(address) => address,
        Err(overlay::OverlayError::CliMissing(_)) => {
            info!(overlay = %overlay_id, port, "overlay CLI missing; listener disabled");
            return RemoteBind::Disabled;
        }
        Err(error) => {
            info!(overlay = %overlay_id, port, reason = %error, "overlay state unavailable; serving local-only");
            return RemoteBind::Retry;
        }
    };
    let addr = std::net::SocketAddr::new(address, port);

    match RemoteServer::bind(addr, state, transport.as_ref()).await {
        Ok(remote) => {
            info!(overlay = %overlay_id, addr = %remote.local_addr(), "serving control protocol over overlay");
            RemoteBind::Bound(remote)
        }
        Err(err) => {
            // Fail-closed validation or an OS bind error: log and stay local-only
            // rather than aborting the whole daemon.
            warn!(reason = %err, "remote control listener not bound; serving local-only");
            RemoteBind::Retry
        }
    }
}

/// Result of one remote-listener bind attempt.
enum RemoteBind {
    /// The listener is bound and ready to serve.
    Bound(RemoteServer),
    /// The condition may recover without restarting the daemon.
    Retry,
    /// The configuration cannot enable remote transport during this run.
    Disabled,
}

/// Create a directory (and parents) with mode 0700 if it does not exist.
fn ensure_private_dir(dir: &std::path::Path) -> Result<(), DaemonError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir).map_err(|source| DaemonError::Directory {
        path: dir.to_path_buf(),
        source,
    })?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        DaemonError::Directory {
            path: dir.to_path_buf(),
            source,
        }
    })
}

/// Resolve when SIGINT or SIGTERM is received.
async fn shutdown_signal() {
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(err) => {
            error!(error = %err, "failed to install SIGINT handler");
            return;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(err) => {
            error!(error = %err, "failed to install SIGTERM handler");
            return;
        }
    };
    tokio::select! {
        _ = sigint.recv() => info!("received SIGINT"),
        _ = sigterm.recv() => info!("received SIGTERM"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_util::sync::CancellationToken;

    use pohunek_daemon::api::{DaemonState, HealthInfo};
    use pohunek_daemon::session::SessionRegistry;
    use protocol::{method, Request, Response};
    use serde_json::Value;

    use super::{
        next_retry_interval, serve_remote_with_retry, RemoteBind, REMOTE_BIND_MAX_RETRY_INTERVAL,
    };

    #[test]
    fn remote_retry_interval_caps_at_configured_maximum() {
        assert_eq!(
            next_retry_interval(REMOTE_BIND_MAX_RETRY_INTERVAL),
            REMOTE_BIND_MAX_RETRY_INTERVAL
        );
    }

    #[tokio::test(start_paused = true)]
    async fn remote_supervisor_retries_after_transient_bind_failures() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let bind_attempts = Arc::clone(&attempts);
        let shutdown = CancellationToken::new();
        let supervisor_shutdown = shutdown.clone();
        let retry_interval = Duration::from_secs(1);

        let supervisor = tokio::spawn(serve_remote_with_retry(
            move || {
                bind_attempts.fetch_add(1, Ordering::Relaxed);
                std::future::ready(RemoteBind::Retry)
            },
            supervisor_shutdown,
            retry_interval,
        ));

        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::Relaxed), 1);

        tokio::time::advance(retry_interval).await;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::Relaxed), 2);

        shutdown.cancel();
        supervisor
            .await
            .expect("remote supervisor exits after shutdown");
    }

    #[tokio::test(start_paused = true)]
    async fn remote_supervisor_serves_after_a_transient_bind_failure() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback remote listener");
        let addr = listener
            .local_addr()
            .expect("read loopback listener address");
        let state = DaemonState::new(HealthInfo::new("test"), SessionRegistry::default());
        let mut remote = Some(pohunek_daemon::api::RemoteServer::from_listener(
            listener, state,
        ));
        let attempts = Arc::new(AtomicUsize::new(0));
        let bind_attempts = Arc::clone(&attempts);
        let shutdown = CancellationToken::new();
        let supervisor_shutdown = shutdown.clone();
        let retry_interval = Duration::from_secs(1);

        let supervisor = tokio::spawn(serve_remote_with_retry(
            move || {
                let attempt = bind_attempts.fetch_add(1, Ordering::Relaxed);
                let result = if attempt == 0 {
                    RemoteBind::Retry
                } else {
                    RemoteBind::Bound(remote.take().expect("only one successful bind"))
                };
                std::future::ready(result)
            },
            supervisor_shutdown,
            retry_interval,
        ));

        tokio::task::yield_now().await;
        tokio::time::advance(retry_interval).await;
        tokio::task::yield_now().await;
        let mut stream = TcpStream::connect(addr)
            .await
            .expect("remote listener accepts a connection after retry");
        let request = Request::new("health", method::DAEMON_HEALTH, Value::Null)
            .expect("valid remote health request");
        let request = serde_json::to_string(&request).expect("serialize remote health request");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write remote health request");
        stream
            .write_all(b"\n")
            .await
            .expect("terminate remote health request");
        let mut response = [0_u8; 1024];
        let read = stream
            .read(&mut response)
            .await
            .expect("read remote health response");
        let response = std::str::from_utf8(&response[..read]).expect("health response is UTF-8");
        let response: Response = serde_json::from_str(response).expect("parse health response");
        assert!(response.is_ok());
        assert_eq!(attempts.load(Ordering::Relaxed), 2);

        shutdown.cancel();
        supervisor
            .await
            .expect("remote supervisor exits after shutdown");
    }
}
