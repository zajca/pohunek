//! `pohunekd` — the pohunek host daemon binary.
//!
//! Milestone 2: resolve XDG paths, initialize JSON logging, acquire the
//! single-instance lock, bind the control socket (with stale-socket recovery and
//! owner-private permissions), and serve `daemon.health` until SIGINT/SIGTERM.
//!
//! Milestone 11: when `NetBird` is available and reports a self IP, additionally
//! bind a TCP control listener on that `NetBird` address and serve it alongside the
//! local Unix socket under one shutdown. `NetBird` being absent or its local state
//! being unavailable is not an error: the daemon stays local-only and logs why.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo, RemoteServer};
use pohunek_daemon::discovery::DiscoveryCache;
use pohunek_daemon::events::{spawn_drain, EventLog};
use pohunek_daemon::lock::InstanceLock;
use pohunek_daemon::notifications::{
    AttentionCoordinator, NotificationProjector, NotificationService, NOTIFICATIONS_SUBDIR,
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
    sessions
        .reconcile_workers()
        .await
        .map_err(DaemonError::Reconcile)?;

    // 8. Bind the control socket (stale-socket recovery + 0600).
    let health = HealthInfo::new(DAEMON_VERSION);
    let discovery = DiscoveryCache::default();
    let state = DaemonState::new_with_discovery(health, sessions.clone(), discovery.clone())
        .with_notifications(notifications.clone())
        .with_attention_coordinator(attention_coordinator.clone());
    let server = ControlServer::bind_with_state(&paths.socket, state).await?;
    info!(socket = %server.socket_path().display(), "ready; serving control protocol");

    // 9. Optionally bind a NetBird TCP control listener alongside the Unix
    //    socket. NetBird absent / not logged in / no self IP => stay local-only.
    let remote_state = DaemonState::new_with_discovery(
        HealthInfo::new(DAEMON_VERSION),
        sessions.clone(),
        discovery,
    )
    .with_notifications(notifications.clone())
    .with_attention_coordinator(attention_coordinator.clone());
    let remote_server = bind_remote_server(remote_state).await;

    // Reconciliation and every required local listener are ready. A manual
    // foreground launch has no NOTIFY_SOCKET and this is a no-op.
    pohunek_daemon::notify::ready()?;

    // 10. Serve both transports under ONE shutdown signal. A small task awaits
    //     the OS signal once and fans it out to each server via a oneshot so they
    //     stop together.
    let (unix_tx, unix_rx) = oneshot::channel::<()>();
    let (remote_tx, remote_rx) = oneshot::channel::<()>();
    let shutdown_sessions = sessions.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_sessions.begin_daemon_shutdown();
        let _ = unix_tx.send(());
        let _ = remote_tx.send(());
    });

    let unix_serve = server.serve(async move {
        let _ = unix_rx.await;
    });
    if let Some(remote) = remote_server {
        let remote_serve = remote.serve(async move {
            let _ = remote_rx.await;
        });
        tokio::join!(unix_serve, remote_serve);
    } else {
        // Drop the unused remote receiver so the fan-out send is a no-op.
        drop(remote_rx);
        unix_serve.await;
    }

    // 11. Flush the append-only event log before exit so events buffered at
    //     shutdown are not lost (bounded so a wedged write cannot hang exit).
    //     Stop the projector before the coordinator so any last defers/resolves
    //     it drains are still accepted, then drop the coordinator's pending
    //     (in-memory, deliberately not persisted).
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

/// Bind the optional `NetBird` TCP control listener.
///
/// Queries `NetBird` state, decides the bind address from it ([`remote_bind_addr`])
/// and the resolved remote port, and binds a [`RemoteServer`] when an address
/// resolves. Returns `None` (daemon stays local-only) when `NetBird` is
/// unavailable, reports no self IP, the remote port is misconfigured, or the bind
/// itself fails. Every local-only path logs the reason; a bind failure is logged
/// but never fatal, so the local control plane always comes up.
async fn bind_remote_server(state: DaemonState) -> Option<RemoteServer> {
    let status = match netbird::run_status() {
        Ok(status) => status,
        Err(err) => {
            info!(reason = %err, "NetBird state unavailable; serving local-only");
            return None;
        }
    };

    let port = match netbird::remote_port() {
        Ok(port) => port,
        Err(err) => {
            warn!(reason = %err, "invalid remote port configuration; serving local-only");
            return None;
        }
    };

    let Some(addr) = remote_bind_addr(&status, port) else {
        info!("NetBird present but no self IP resolved; serving local-only");
        return None;
    };

    match RemoteServer::bind(addr, state).await {
        Ok(remote) => {
            info!(addr = %remote.local_addr(), "serving control protocol over NetBird");
            Some(remote)
        }
        Err(err) => {
            // Fail-closed validation or an OS bind error: log and stay local-only
            // rather than aborting the whole daemon.
            warn!(reason = %err, "remote control listener not bound; serving local-only");
            None
        }
    }
}

/// Decide the remote TCP bind address from parsed `NetBird` state.
///
/// Returns `Some((self_ip, port))` when `NetBird` reports this host's own IP, else
/// `None`. `NetbirdStatus::self_netbird_ip` already filters to the `NetBird` CGNAT
/// range, and the authoritative fail-closed gate is [`RemoteServer::bind`]'s
/// validation, so this helper does not re-validate the range (which would only
/// risk diverging from that single source of truth). Pure and unit-tested.
#[must_use]
fn remote_bind_addr(status: &netbird::NetbirdStatus, port: u16) -> Option<SocketAddr> {
    status.self_netbird_ip().map(|ip| SocketAddr::new(ip, port))
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
    use super::remote_bind_addr;

    /// A remote bind port distinct from the production default, so the test
    /// asserts the helper threads the passed-in port rather than a constant.
    const TEST_PORT: u16 = 19000;

    #[test]
    fn remote_bind_addr_is_none_when_self_ip_absent() {
        // Empty status: no root netbirdIp and no localPeerState => no self IP.
        let status = netbird::parse_status("{}").expect("parse empty status");
        assert!(remote_bind_addr(&status, TEST_PORT).is_none());
    }

    #[test]
    fn remote_bind_addr_is_none_when_offline_without_self_ip() {
        // Daemon present but not connected and reporting no self IP.
        let status = netbird::parse_status(
            r#"{"daemonStatus":"NeedsLogin","peers":{"connected":0,"total":0,"details":[]}}"#,
        )
        .expect("parse offline status");
        assert!(remote_bind_addr(&status, TEST_PORT).is_none());
    }

    #[test]
    fn remote_bind_addr_uses_self_netbird_ip_and_port() {
        let status =
            netbird::parse_status(r#"{"netbirdIp":"100.92.10.20","daemonStatus":"Connected"}"#)
                .expect("parse current status");
        let addr = remote_bind_addr(&status, TEST_PORT).expect("self IP resolves to a bind addr");
        assert_eq!(addr.ip().to_string(), "100.92.10.20");
        assert_eq!(addr.port(), TEST_PORT);
    }
}
