//! `zagentmeshd` — the zagentmesh host daemon binary.
//!
//! Milestone 2: resolve XDG paths, initialize JSON logging, acquire the
//! single-instance lock, bind the control socket (with stale-socket recovery and
//! owner-private permissions), and serve `daemon.health` until SIGINT/SIGTERM.

use std::process::ExitCode;

use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info};

use zagentmesh_daemon::api::{ControlServer, DaemonState, HealthInfo};
use zagentmesh_daemon::lock::InstanceLock;
use zagentmesh_daemon::session::{SessionRegistry, SessionRegistryConfig};
use zagentmesh_daemon::{logging, DaemonError, Paths, DAEMON_VERSION};

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
            eprintln!("zagentmeshd: fatal: {err}");
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
        "zagentmeshd starting"
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

    // 9. Serve until a termination signal.
    server.serve(shutdown_signal()).await;

    // 10. Flush the append-only event log before exit so events buffered at
    //     shutdown are not lost (bounded so a wedged write cannot hang exit).
    sessions.shutdown_event_log().await;

    info!("zagentmeshd stopped");
    Ok(())
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
