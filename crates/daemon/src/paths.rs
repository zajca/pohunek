//! Filesystem path resolution for the daemon.
//!
//! All paths come from XDG base directories (see `docs/architecture.md`
//! "Configuration, State, and Log Storage"). Per the hard project rule, a
//! missing required base directory is a fail-fast error: we never invent a
//! silent fallback path.
//!
//! Resolved paths (Linux-first):
//! - control socket:  `$XDG_RUNTIME_DIR/zagentmesh/daemon.sock`   (dir 0700)
//! - single-instance lock: `$XDG_RUNTIME_DIR/zagentmesh/daemon.lock`
//! - logs:            `$XDG_STATE_HOME` or `~/.local/state` + `/zagentmesh/logs`
//! - data dir:        `$XDG_DATA_HOME`  or `~/.local/share` + `/zagentmesh`
//!   (state.db, events/, worktrees/ live here in later milestones)

use std::path::PathBuf;

use crate::error::DaemonError;

/// Subdirectory/name constants. Centralized so there are no scattered string
/// literals for the on-disk layout.
const APP_DIR: &str = "zagentmesh";
const SOCKET_NAME: &str = "daemon.sock";
const LOCK_NAME: &str = "daemon.lock";
const LOGS_SUBDIR: &str = "logs";

/// Resolved set of daemon paths.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `$XDG_RUNTIME_DIR/zagentmesh` — owner-private (0700) runtime dir.
    pub runtime_dir: PathBuf,
    /// The control Unix socket path.
    pub socket: PathBuf,
    /// The single-instance lock file path.
    pub lock: PathBuf,
    /// The structured-log directory.
    pub log_dir: PathBuf,
    /// The user data directory (state.db / events / worktrees in later milestones).
    pub data_dir: PathBuf,
}

impl Paths {
    /// Resolve all daemon paths from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::MissingEnv`] when `XDG_RUNTIME_DIR` is unset (it is
    /// required and has no safe invented default), or when neither
    /// `XDG_STATE_HOME`/`XDG_DATA_HOME` nor `HOME` is available to derive the
    /// log/data directories.
    pub fn resolve() -> Result<Self, DaemonError> {
        // XDG_RUNTIME_DIR is mandatory: it is the only correct home for an
        // owner-private socket, and inventing e.g. /tmp would weaken the
        // single-user security model. Fail fast instead.
        let runtime_base = require_env("XDG_RUNTIME_DIR")?;
        let runtime_dir = PathBuf::from(runtime_base).join(APP_DIR);
        let socket = runtime_dir.join(SOCKET_NAME);
        let lock = runtime_dir.join(LOCK_NAME);

        // Logs: prefer XDG_STATE_HOME, else ~/.local/state. One of the two must
        // resolve; otherwise fail fast.
        let state_home = xdg_or_home_relative("XDG_STATE_HOME", &[".local", "state"])?;
        let log_dir = state_home.join(APP_DIR).join(LOGS_SUBDIR);

        // Data dir: prefer XDG_DATA_HOME, else ~/.local/share.
        let data_home = xdg_or_home_relative("XDG_DATA_HOME", &[".local", "share"])?;
        let data_dir = data_home.join(APP_DIR);

        Ok(Self {
            runtime_dir,
            socket,
            lock,
            log_dir,
            data_dir,
        })
    }
}

/// Read a required environment variable or fail fast.
fn require_env(key: &str) -> Result<String, DaemonError> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(DaemonError::MissingEnv {
            var: key.to_owned(),
        }),
    }
}

/// Resolve an XDG base dir: use `$key` if set and non-empty, otherwise
/// `$HOME` joined with `home_relative`. Fails if neither is available.
fn xdg_or_home_relative(key: &str, home_relative: &[&str]) -> Result<PathBuf, DaemonError> {
    if let Ok(v) = std::env::var(key) {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = require_env("HOME").map_err(|_| DaemonError::MissingEnv {
        // Report the more actionable variable: the user needs HOME (or the XDG
        // var) so the daemon can locate its state directory.
        var: format!("{key} or HOME"),
    })?;
    let mut p = PathBuf::from(home);
    for seg in home_relative {
        p.push(seg);
    }
    Ok(p)
}
