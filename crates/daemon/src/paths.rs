//! Filesystem path resolution for the daemon.
//!
//! All paths come from XDG base directories (see `docs/architecture.md`
//! "Configuration, State, and Log Storage"). Per the hard project rule, a
//! missing required base directory is a fail-fast error: we never invent a
//! silent fallback path.
//!
//! Resolved paths (Linux-first):
//! - control socket:  `$XDG_RUNTIME_DIR/pohunek/daemon.sock`   (dir 0700)
//! - single-instance lock: `$XDG_RUNTIME_DIR/pohunek/daemon.lock`
//! - logs:            `$XDG_STATE_HOME` or `~/.local/state` + `/pohunek/logs`
//! - data dir:        `$XDG_DATA_HOME`  or `~/.local/share` + `/pohunek`
//!   (state.db, events/, worktrees/ live here in later milestones)
//! - config dir:      `$XDG_CONFIG_HOME` or `~/.config` + `/pohunek`
//!   (host-default templates/actions/prompts, hooks/, agents/ profiles)

use std::path::PathBuf;

use crate::error::DaemonError;

/// Subdirectory/name constants. Centralized so there are no scattered string
/// literals for the on-disk layout.
const APP_DIR: &str = "pohunek";
const SOCKET_NAME: &str = "daemon.sock";
const LOCK_NAME: &str = "daemon.lock";
const LOGS_SUBDIR: &str = "logs";

/// Resolved set of daemon paths.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `$XDG_RUNTIME_DIR/pohunek` — owner-private (0700) runtime dir.
    pub runtime_dir: PathBuf,
    /// The control Unix socket path.
    pub socket: PathBuf,
    /// The single-instance lock file path.
    pub lock: PathBuf,
    /// The structured-log directory.
    pub log_dir: PathBuf,
    /// The user data directory (state.db / events / worktrees in later milestones).
    pub data_dir: PathBuf,
    /// The host config directory (`$XDG_CONFIG_HOME/pohunek` or `~/.config/pohunek`).
    /// Home of host-default templates/actions/prompts, lifecycle hooks, and agent
    /// profiles. The daemon reads (never writes) this tree as the host-default layer.
    pub config_dir: PathBuf,
}

impl Paths {
    /// Resolve all daemon paths from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::MissingEnv`] when `XDG_RUNTIME_DIR` is unset (it is
    /// required and has no safe invented default), or when neither
    /// `XDG_STATE_HOME`/`XDG_DATA_HOME`/`XDG_CONFIG_HOME` nor `HOME` is available to
    /// derive the log/data/config directories.
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

        // Config dir: prefer XDG_CONFIG_HOME, else ~/.config. One of the two must
        // resolve; otherwise fail fast (no silent default).
        let config_home = xdg_or_home_relative("XDG_CONFIG_HOME", &[".config"])?;
        let config_dir = config_home.join(APP_DIR);

        Ok(Self {
            runtime_dir,
            socket,
            lock,
            log_dir,
            data_dir,
            config_dir,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // Env-var resolution is process-global; serialize the env-mutating tests and
    // restore every touched var on drop so they stay hermetic under the parallel
    // runner (mirrors the PATH_LOCK guard in `tests/health_socket.rs`).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const VARS: [&str; 5] = [
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "HOME",
    ];

    /// Holds `ENV_LOCK` for the test's duration and restores every snapshotted var
    /// on drop, so a panic mid-test cannot leak mutated env into a sibling.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn acquire() -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let saved = VARS.iter().map(|&k| (k, std::env::var(k).ok())).collect();
            Self { _lock: lock, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn tmp_base(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pohunek-paths-{tag}-{}", std::process::id()))
    }

    /// Set every base var the resolver reads to temp paths, so a test can exercise
    /// one variable in isolation without tripping an unrelated fail-fast.
    fn set_all_present(base: &Path) {
        std::env::set_var("XDG_RUNTIME_DIR", base.join("run"));
        std::env::set_var("XDG_STATE_HOME", base.join("state"));
        std::env::set_var("XDG_DATA_HOME", base.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", base.join("cfg"));
        std::env::set_var("HOME", base.join("home"));
    }

    #[test]
    fn config_dir_from_xdg_config_home() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("xdg");
        set_all_present(&base);
        let paths = Paths::resolve().expect("resolve with all base vars set");
        assert_eq!(paths.config_dir, base.join("cfg").join(APP_DIR));
    }

    #[test]
    fn config_dir_falls_back_to_home_dot_config() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("home");
        set_all_present(&base);
        std::env::remove_var("XDG_CONFIG_HOME");
        let paths = Paths::resolve().expect("resolve with XDG_CONFIG_HOME unset");
        assert_eq!(
            paths.config_dir,
            base.join("home").join(".config").join(APP_DIR)
        );
    }

    #[test]
    fn missing_config_home_and_home_fails_fast() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("missing");
        set_all_present(&base);
        // XDG_STATE_HOME/XDG_DATA_HOME stay set so the earlier steps do not need
        // HOME; only the config step must fail, and with the actionable message.
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("HOME");
        match Paths::resolve() {
            Err(DaemonError::MissingEnv { var }) => {
                assert_eq!(var, "XDG_CONFIG_HOME or HOME");
            }
            other => panic!("expected MissingEnv, got {other:?}"),
        }
    }

    #[test]
    fn default_session_config_has_no_config_dir() {
        // Pins the new field is opt-in: every `..SessionRegistryConfig::default()`
        // construction across the crate keeps compiling with `config_dir = None`.
        assert_eq!(
            crate::session::SessionRegistryConfig::default().config_dir,
            None
        );
    }
}
