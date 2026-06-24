//! `pohunekd` — the pohunek host daemon binary.
//!
//! Milestone 2: resolve XDG paths, initialize JSON logging, acquire the
//! single-instance lock, bind the control socket (with stale-socket recovery and
//! owner-private permissions), and serve `daemon.health` until SIGINT/SIGTERM.
//!
//! Milestone 11: when NetBird is available and reports a self IP, additionally
//! bind a TCP control listener on that NetBird address and serve it alongside the
//! local Unix socket under one shutdown. NetBird being absent or its local state
//! being unavailable is not an error: the daemon stays local-only and logs why.

use std::net::SocketAddr;
use std::process::ExitCode;

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo, RemoteServer};
use pohunek_daemon::lock::InstanceLock;
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use pohunek_daemon::{logging, DaemonError, Paths, DAEMON_VERSION};

/// File name of the unified metadata store (resume + worktree bindings) under
/// the data dir.
const STORE_NAME: &str = "metadata.jsonl";

/// Subdirectory under the data dir holding per-session git worktrees.
const WORKTREES_SUBDIR: &str = "worktrees";

/// Subdirectory under the data dir holding the append-only event log.
const EVENTS_SUBDIR: &str = "events";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
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
    //    too and is acquired first. The data dir holds the resume-binding store.
    ensure_private_dir(&paths.runtime_dir)?;
    ensure_private_dir(&paths.data_dir)?;

    // 4. Single-instance lock: a second daemon refuses to start.
    let _lock = InstanceLock::acquire(&paths.lock)?;
    info!(lock = %paths.lock.display(), "acquired single-instance lock");

    // 5. Build the session registry with the hook-handshake socket path, the
    //    unified metadata store (resume + worktree bindings in one file), the
    //    worktrees root, and the event-log directory, so spawned agents can
    //    report their native id, captured sessions survive a restart, a
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
        ..SessionRegistryConfig::default()
    };
    let sessions = SessionRegistry::new(config);

    // 6. Start the append-only event log before anything emits, so the resume
    //    events from `load_and_resume` below are captured too. Fail fast if the
    //    log location is unusable.
    sessions
        .spawn_event_log()
        .map_err(|source| DaemonError::Directory {
            path: paths.data_dir.join(EVENTS_SUBDIR),
            source,
        })?;
    sessions.spawn_agent_state_hooks();

    // 7. Bind the control socket (stale-socket recovery + 0600).
    let health = HealthInfo::new(DAEMON_VERSION);
    let state = DaemonState::new(health, sessions.clone());
    let server = ControlServer::bind_with_state(&paths.socket, state).await?;
    info!(socket = %server.socket_path().display(), "ready; serving control protocol");

    // 8. A daemon restart kills live PTYs by design; relaunch the sessions whose
    //    native id was captured. The socket is already bound, so a resumed
    //    agent's hook can re-report. Best-effort: per-session failures are
    //    logged, never fatal.
    sessions.load_and_resume().await;

    // 9. Optionally bind a NetBird TCP control listener alongside the Unix
    //    socket. NetBird absent / not logged in / no self IP => stay local-only.
    let remote_state = DaemonState::new(HealthInfo::new(DAEMON_VERSION), sessions.clone());
    let remote_server = bind_remote_server(remote_state).await;

    // 10. Serve both transports under ONE shutdown signal. A small task awaits
    //     the OS signal once and fans it out to each server via a oneshot so they
    //     stop together.
    let (unix_tx, unix_rx) = oneshot::channel::<()>();
    let (remote_tx, remote_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = unix_tx.send(());
        let _ = remote_tx.send(());
    });

    let unix_serve = server.serve(async move {
        let _ = unix_rx.await;
    });
    match remote_server {
        Some(remote) => {
            let remote_serve = remote.serve(async move {
                let _ = remote_rx.await;
            });
            tokio::join!(unix_serve, remote_serve);
        }
        None => {
            // Drop the unused remote receiver so the fan-out send is a no-op.
            drop(remote_rx);
            unix_serve.await;
        }
    }

    // 11. Flush the append-only event log before exit so events buffered at
    //     shutdown are not lost (bounded so a wedged write cannot hang exit).
    sessions.shutdown_agent_state_hooks().await;
    sessions.shutdown_event_log().await;

    info!("pohunekd stopped");
    Ok(())
}

/// Bind the optional NetBird TCP control listener.
///
/// Queries NetBird state, decides the bind address from it ([`remote_bind_addr`])
/// and the resolved remote port, and binds a [`RemoteServer`] when an address
/// resolves. Returns `None` (daemon stays local-only) when NetBird is
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

    let addr = match remote_bind_addr(&status, port) {
        Some(addr) => addr,
        None => {
            info!("NetBird present but no self IP resolved; serving local-only");
            return None;
        }
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

/// Decide the remote TCP bind address from parsed NetBird state.
///
/// Returns `Some((self_ip, port))` when NetBird reports this host's own IP, else
/// `None`. `NetbirdStatus::self_netbird_ip` already filters to the NetBird CGNAT
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
