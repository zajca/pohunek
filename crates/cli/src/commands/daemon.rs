//! `pohunek daemon start` — launch the host daemon by hand.
//!
//! Two worker modes:
//! - service (default): run the installed, versioned `pohunekd` with
//!   `--service-config <config>/pohunek/service.toml`, so workers become
//!   native service jobs exactly as under the login service. Without
//!   `service.toml` this fails fast and points at `pohunek service install`.
//! - `--dev-subprocess`: run the `pohunekd` next to this CLI (or on `PATH`)
//!   with `POHUNEK_WORKER_LAUNCHER=subprocess` and no service configuration,
//!   for development and tests.
//!
//! Either mode runs in the foreground by default (the process is replaced by
//! `pohunekd`, so logs stream to the terminal and Ctrl-C stops it) or in the
//! background with `--detach`. A daemon binary is never guessed: a missing
//! one is a clear error.

// Rust guideline compliant 2026-09-24

use std::path::{Path, PathBuf};
use std::process::Command;

use pohunek_service_config::ServiceConfig;

use crate::error::CliError;

/// The daemon binary name.
const DAEMON_BIN: &str = "pohunekd";

/// Daemon argument naming the service configuration file.
const SERVICE_CONFIG_FLAG: &str = "--service-config";

/// Environment variable selecting the daemon's worker launcher.
const WORKER_LAUNCHER_VAR: &str = "POHUNEK_WORKER_LAUNCHER";

/// Launcher value that runs workers as plain subprocesses (dev/test only).
const SUBPROCESS_LAUNCHER: &str = "subprocess";

/// How the started daemon launches workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Native service jobs configured by `service.toml`.
    Service,
    /// Plain subprocesses, for development and tests.
    DevSubprocess,
}

/// Run `daemon start`.
///
/// In foreground mode this does not return on success (the process is replaced).
/// In detach mode it returns after spawning.
///
/// # Errors
///
/// Returns [`CliError::ServiceNotInstalled`] in service mode without
/// `service.toml`, [`CliError::Service`] for an invalid one, and
/// [`CliError::Spawn`] if the daemon binary cannot be located or launched.
pub(crate) fn start(detach: bool, mode: Mode) -> Result<(), CliError> {
    let mut command = daemon_command(mode)?;
    if detach {
        let bin = PathBuf::from(command.get_program());
        // Background spawn: detach stdio so the CLI can return. The daemon writes
        // its own JSON logs to the state log dir regardless.
        let child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| CliError::Spawn(format!("{}: {e}", bin.display())))?;
        println!("started {} in background (pid {})", DAEMON_BIN, child.id());
        Ok(())
    } else {
        foreground_exec(command)
    }
}

/// Builds the daemon command line for `mode`.
fn daemon_command(mode: Mode) -> Result<Command, CliError> {
    match mode {
        Mode::Service => {
            let paths = pohunek_paths::BasePaths::resolve().map_err(CliError::Paths)?;
            let config_path = pohunek_service_config::file_path(&paths);
            if !config_path.exists() {
                return Err(CliError::ServiceNotInstalled { path: config_path });
            }
            let config = ServiceConfig::load(&config_path)
                .map_err(|error| CliError::Service(error.into()))?;
            Ok(service_command(&config.daemon_executable(), &config_path))
        }
        Mode::DevSubprocess => Ok(dev_command(&locate_daemon()?)),
    }
}

fn service_command(daemon: &Path, config_path: &Path) -> Command {
    let mut command = Command::new(daemon);
    command.arg(SERVICE_CONFIG_FLAG).arg(config_path);
    command
}

fn dev_command(daemon: &Path) -> Command {
    let mut command = Command::new(daemon);
    command.env(WORKER_LAUNCHER_VAR, SUBPROCESS_LAUNCHER);
    command
}

/// Replace the current process with the daemon (foreground).
fn foreground_exec(mut command: Command) -> Result<(), CliError> {
    use std::os::unix::process::CommandExt;

    // `exec` returns only if it fails; on success the image is replaced and this
    // function never returns.
    let err = command.exec();
    Err(CliError::Spawn(format!(
        "exec {}: {err}",
        Path::new(command.get_program()).display()
    )))
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn service_mode_passes_only_the_service_config() {
        let command = service_command(
            Path::new("/p/libexec/pohunek/1.0.0/pohunekd"),
            Path::new("/c/pohunek/service.toml"),
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("--service-config"),
                OsStr::new("/c/pohunek/service.toml")
            ]
        );
        assert_eq!(command.get_envs().count(), 0);
    }

    #[test]
    fn dev_subprocess_mode_selects_the_subprocess_launcher_without_config() {
        let command = dev_command(Path::new("/build/pohunekd"));
        assert_eq!(command.get_args().count(), 0);
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [(
                OsStr::new("POHUNEK_WORKER_LAUNCHER"),
                Some(OsStr::new("subprocess"))
            )]
        );
    }
}
