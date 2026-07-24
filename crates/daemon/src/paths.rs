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
//! - cache dir:       `$XDG_CACHE_HOME` or `~/.cache` + `/pohunek`
//! - config dir:      `$XDG_CONFIG_HOME` or `~/.config` + `/pohunek`
//!   (host-default templates/actions/prompts, hooks/, agents/ profiles)

use std::path::PathBuf;

use crate::error::DaemonError;

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
    /// The user state directory containing logs and durable worker journals.
    pub state_dir: PathBuf,
    /// The user data directory (state.db / events / worktrees in later milestones).
    pub data_dir: PathBuf,
    /// The user cache directory.
    pub cache_dir: PathBuf,
    /// The XDG config base (`$XDG_CONFIG_HOME` or `$HOME/.config`).
    pub config_home: PathBuf,
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
        let base = pohunek_paths::BasePaths::resolve().map_err(path_error)?;

        Ok(Self {
            runtime_dir: base.runtime_dir,
            socket: base.socket,
            lock: base.lock,
            log_dir: base.log_dir,
            state_dir: base.state_dir,
            data_dir: base.data_dir,
            cache_dir: base.cache_dir,
            config_home: base.config_home,
            config_dir: base.config_dir,
        })
    }

    /// Directory the launcher scripts are materialized into by `pohunek setup scripts`.
    #[must_use]
    pub fn launcher_bin_dir(&self) -> PathBuf {
        self.data_dir.join(pohunek_paths::BIN_SUBDIR)
    }

    /// The user's sway config dir (`<config_home>/sway`).
    #[must_use]
    pub fn sway_config_dir(&self) -> PathBuf {
        self.config_home.join(pohunek_paths::SWAY_CONFIG_DIR)
    }

    /// Directory where assistant knowledge bundles are cached.
    #[must_use]
    pub fn assistant_bundle_cache_dir(&self) -> PathBuf {
        self.cache_dir.join(pohunek_paths::KNOWLEDGE_CACHE_SUBDIR)
    }

    /// Runtime directory for assistant material generated for one session or launch.
    #[must_use]
    pub fn assistant_runtime_dir(&self, session_or_launch_id: &str) -> Option<PathBuf> {
        pohunek_paths::valid_runtime_id(session_or_launch_id).map(|id| {
            self.runtime_dir
                .join(pohunek_paths::ASSISTANT_RUNTIME_SUBDIR)
                .join(id)
        })
    }
}

fn path_error(err: pohunek_paths::PathError) -> DaemonError {
    match err {
        pohunek_paths::PathError::MissingEnv { var } => DaemonError::MissingEnv { var },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pohunek_paths::{APP_DIR, ASSISTANT_RUNTIME_SUBDIR, KNOWLEDGE_CACHE_SUBDIR};

    use super::*;

    use crate::test_support::XDG_ENV_LOCK;

    const VARS: [&str; 6] = [
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
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
            let lock = XDG_ENV_LOCK
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
        std::env::set_var("XDG_CACHE_HOME", base.join("cache"));
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
    fn cache_dir_from_xdg_cache_home() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("xdg-cache");
        set_all_present(&base);
        let paths = Paths::resolve().expect("resolve with all base vars set");
        assert_eq!(paths.cache_dir, base.join("cache").join(APP_DIR));
    }

    #[test]
    fn cache_dir_falls_back_to_home_dot_cache() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("home-cache");
        set_all_present(&base);
        std::env::remove_var("XDG_CACHE_HOME");
        let paths = Paths::resolve().expect("resolve with XDG_CACHE_HOME unset");
        assert_eq!(
            paths.cache_dir,
            base.join("home").join(".cache").join(APP_DIR)
        );
    }

    #[test]
    fn assistant_dirs_have_expected_shape() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("assistant");
        set_all_present(&base);
        let paths = Paths::resolve().expect("resolve with all base vars set");
        assert_eq!(
            paths.assistant_bundle_cache_dir(),
            base.join("cache")
                .join(APP_DIR)
                .join(KNOWLEDGE_CACHE_SUBDIR)
        );
        assert_eq!(
            paths.assistant_runtime_dir("launch-123"),
            Some(
                base.join("run")
                    .join(APP_DIR)
                    .join(ASSISTANT_RUNTIME_SUBDIR)
                    .join("launch-123")
            )
        );
    }

    #[test]
    fn assistant_runtime_dir_rejects_unsafe_ids() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("assistant-unsafe");
        set_all_present(&base);
        let paths = Paths::resolve().expect("resolve with all base vars set");

        for id in ["", "/tmp/launch", "../launch", "launch/child", "launch/.."] {
            assert_eq!(
                paths.assistant_runtime_dir(id),
                None,
                "unsafe id should be rejected: {id:?}"
            );
        }
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
