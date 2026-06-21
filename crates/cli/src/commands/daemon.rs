//! `pohunek daemon start` — launch the host daemon.
//!
//! Per `docs/plan-phase-1.md` "Build Order" step 2: `daemon start` runs the
//! daemon. Two modes:
//! - foreground (default): replace the CLI process with `pohunekd` so logs
//!   stream to the terminal and Ctrl-C stops it. This is the natural mode for
//!   `systemd --user` (it execs the daemon as the service process) and for
//!   manual debugging.
//! - `--detach`: spawn `pohunekd` in the background and return immediately.
//!
//! The daemon binary is located next to the CLI binary first (the normal install
//! layout), then on `PATH`. We never invent a path: if it cannot be found, fail
//! fast with a clear error.

use std::path::PathBuf;
use std::process::Command;

use crate::error::CliError;

/// The daemon binary name.
const DAEMON_BIN: &str = "pohunekd";

/// Run `daemon start`.
///
/// In foreground mode this does not return on success (the process is replaced).
/// In detach mode it returns after spawning.
///
/// # Errors
///
/// Returns [`CliError::Spawn`] if the daemon binary cannot be located or
/// launched.
pub(crate) fn start(detach: bool) -> Result<(), CliError> {
    let bin = locate_daemon()?;

    if detach {
        // Background spawn: detach stdio so the CLI can return. The daemon writes
        // its own JSON logs to the state log dir regardless.
        let child = Command::new(&bin)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| CliError::Spawn(format!("{}: {e}", bin.display())))?;
        println!("started {} in background (pid {})", DAEMON_BIN, child.id());
        Ok(())
    } else {
        // Foreground: replace this process image with the daemon so signals and
        // stdio belong to it directly. `exec` only returns on error.
        foreground_exec(&bin)
    }
}

/// Replace the current process with the daemon binary (foreground).
fn foreground_exec(bin: &PathBuf) -> Result<(), CliError> {
    use std::os::unix::process::CommandExt;

    // `exec` returns only if it fails; on success the image is replaced and this
    // function never returns.
    let err = Command::new(bin).exec();
    Err(CliError::Spawn(format!("exec {}: {err}", bin.display())))
}

/// Locate the `pohunekd` binary: sibling of the running CLI, then `PATH`.
fn locate_daemon() -> Result<PathBuf, CliError> {
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let sibling = dir.join(DAEMON_BIN);
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }
    if let Some(found) = which_on_path(DAEMON_BIN) {
        return Ok(found);
    }
    Err(CliError::Spawn(format!(
        "could not find '{DAEMON_BIN}' next to the CLI or on PATH"
    )))
}

/// Minimal dependency-free `which` (same approach as the doctor command).
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
