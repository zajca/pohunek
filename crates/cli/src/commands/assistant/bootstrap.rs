//! Local daemon bootstrap for the assistant launch.
//!
//! `pohunek assistant` should work in one command, so on a local target it
//! brings the daemon up itself when it is not already running (unless
//! `--no-start-daemon` is set). Remote targets are never auto-started — they
//! preserve the existing remote session safety model.

use std::time::Duration;

use crate::client::Client;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::is_local_host;

/// How long to wait for a freshly started daemon to begin answering, and how
/// often to re-probe the socket while waiting.
const START_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// Ensure a reachable daemon for `host`, starting the local daemon if needed.
///
/// Returns `true` when this call auto-started the local daemon, `false` when a
/// daemon was already reachable (or the target is remote).
///
/// # Errors
///
/// - Remote target unreachable: surfaced from [`Client::connect`].
/// - Local daemon down and `--no-start-daemon`: [`CliError::DaemonUnreachable`].
/// - Local daemon failed to start or never came up: [`CliError::Spawn`] or a
///   timeout [`CliError::DaemonUnreachable`] with the manual recovery command.
pub(crate) async fn ensure_daemon(
    host: &str,
    paths: &Paths,
    no_start_daemon: bool,
) -> Result<bool, CliError> {
    // Remote targets are never auto-started.
    if !is_local_host(host) {
        return Ok(false);
    }

    match Client::connect(host, paths).await {
        Ok(_) => Ok(false),
        Err(CliError::DaemonUnreachable { .. }) if !no_start_daemon => {
            start_and_wait(host, paths).await?;
            Ok(true)
        }
        Err(err) => Err(err),
    }
}

/// Start the local daemon in the background and poll the socket until it answers
/// or the bounded timeout elapses.
async fn start_and_wait(host: &str, paths: &Paths) -> Result<(), CliError> {
    crate::commands::daemon::start(true)?;

    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if Client::connect(host, paths).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(CliError::DaemonUnreachable {
                socket: paths.socket.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "daemon did not start within the timeout; run `pohunek daemon start --detach` \
                     manually and retry",
                ),
            });
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}
