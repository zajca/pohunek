//! `zagentmeshd` — the zagentmesh host daemon binary.
//!
//! Milestone 2: resolve XDG paths, initialize JSON logging, acquire the
//! single-instance lock, bind the control socket (with stale-socket recovery and
//! owner-private permissions), and serve `daemon.health` until SIGINT/SIGTERM.

use std::process::ExitCode;

use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info};

use zagentmesh_daemon::api::{ControlServer, HealthInfo};
use zagentmesh_daemon::lock::InstanceLock;
use zagentmesh_daemon::{logging, DaemonError, Paths, DAEMON_VERSION};

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
    //    too and is acquired first.
    ensure_runtime_dir(&paths)?;

    // 4. Single-instance lock: a second daemon refuses to start.
    let _lock = InstanceLock::acquire(&paths.lock)?;
    info!(lock = %paths.lock.display(), "acquired single-instance lock");

    // 5. Bind the control socket (stale-socket recovery + 0600).
    let health = HealthInfo::new(DAEMON_VERSION);
    let server = ControlServer::bind(&paths.socket, health).await?;
    info!(socket = %server.socket_path().display(), "ready; serving control protocol");

    // 6. Serve until a termination signal.
    server.serve(shutdown_signal()).await;

    info!("zagentmeshd stopped");
    Ok(())
}

/// Create the runtime directory with mode 0700 if it does not exist.
fn ensure_runtime_dir(paths: &Paths) -> Result<(), DaemonError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(&paths.runtime_dir).map_err(|source| DaemonError::Directory {
        path: paths.runtime_dir.clone(),
        source,
    })?;
    std::fs::set_permissions(&paths.runtime_dir, std::fs::Permissions::from_mode(0o700)).map_err(
        |source| DaemonError::Directory {
            path: paths.runtime_dir.clone(),
            source,
        },
    )
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
