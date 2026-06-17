//! CLI-side path resolution.
//!
//! The CLI must resolve the same control-socket and state paths the daemon uses
//! (see `docs/architecture.md` "Configuration, State, and Log Storage"). Like the
//! daemon, a missing required base directory is a fail-fast error: no silent
//! invented fallbacks (hard project rule).

use std::path::PathBuf;

use crate::error::CliError;

const APP_DIR: &str = "zagentmesh";
const SOCKET_NAME: &str = "daemon.sock";

/// Resolved CLI paths.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    /// `$XDG_RUNTIME_DIR/zagentmesh` runtime dir.
    pub(crate) runtime_dir: PathBuf,
    /// The control Unix socket path.
    pub(crate) socket: PathBuf,
    /// The user data directory (state.db / events / worktrees).
    pub(crate) data_dir: PathBuf,
    /// The structured-log directory.
    pub(crate) log_dir: PathBuf,
}

impl Paths {
    /// Resolve CLI paths from the environment, failing fast on missing required
    /// variables.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::MissingEnv`] when `XDG_RUNTIME_DIR` is unset, or when
    /// neither the relevant XDG var nor `HOME` is available.
    pub(crate) fn resolve() -> Result<Self, CliError> {
        let runtime_base = require_env("XDG_RUNTIME_DIR")?;
        let runtime_dir = PathBuf::from(runtime_base).join(APP_DIR);
        let socket = runtime_dir.join(SOCKET_NAME);

        let data_home = xdg_or_home_relative("XDG_DATA_HOME", &[".local", "share"])?;
        let data_dir = data_home.join(APP_DIR);

        let state_home = xdg_or_home_relative("XDG_STATE_HOME", &[".local", "state"])?;
        let log_dir = state_home.join(APP_DIR).join("logs");

        Ok(Self {
            runtime_dir,
            socket,
            data_dir,
            log_dir,
        })
    }
}

fn require_env(key: &str) -> Result<String, CliError> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(CliError::MissingEnv {
            var: key.to_owned(),
        }),
    }
}

fn xdg_or_home_relative(key: &str, home_relative: &[&str]) -> Result<PathBuf, CliError> {
    if let Ok(v) = std::env::var(key) {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = std::env::var("HOME").map_err(|_| CliError::MissingEnv {
        var: format!("{key} or HOME"),
    })?;
    if home.is_empty() {
        return Err(CliError::MissingEnv {
            var: format!("{key} or HOME"),
        });
    }
    let mut p = PathBuf::from(home);
    for seg in home_relative {
        p.push(seg);
    }
    Ok(p)
}
