//! CLI-side path resolution.
//!
//! The CLI must resolve the same control-socket and state paths the daemon uses
//! (see `docs/architecture.md` "Configuration, State, and Log Storage"). Like the
//! daemon, a missing required base directory is a fail-fast error: no silent
//! invented fallbacks (hard project rule).

use std::path::PathBuf;

use crate::error::CliError;

/// Resolved CLI paths.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    /// `$XDG_RUNTIME_DIR/pohunek` runtime dir.
    pub(crate) runtime_dir: PathBuf,
    /// The control Unix socket path.
    pub(crate) socket: PathBuf,
    /// The user data directory (state.db / events / worktrees).
    pub(crate) data_dir: PathBuf,
    /// The structured-log directory.
    pub(crate) log_dir: PathBuf,
    /// The user cache directory.
    pub(crate) cache_dir: PathBuf,
    /// The XDG config base (`$XDG_CONFIG_HOME` or `$HOME/.config`). Used to
    /// derive both pohunek's own config dir and the sway config dir.
    pub(crate) config_home: PathBuf,
    /// pohunek's config dir (`<config_home>/pohunek`) — holds `launcher.conf`
    /// and `prompts/*.tmpl` consumed by the launcher scripts.
    pub(crate) config_dir: PathBuf,
}

impl Paths {
    /// Resolve only the pohunek cache directory.
    ///
    /// Standalone host discovery deliberately does not need a runtime directory
    /// or local control socket, so it must not require `XDG_RUNTIME_DIR`.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::MissingEnv`] when neither `XDG_CACHE_HOME` nor `HOME`
    /// is available.
    pub(crate) fn cache_dir_only() -> Result<PathBuf, CliError> {
        pohunek_paths::cache_home()
            .map(|path| path.join(pohunek_paths::APP_DIR))
            .map_err(path_error)
    }

    /// Resolve CLI paths from the environment, failing fast on missing required
    /// variables.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::MissingEnv`] when `XDG_RUNTIME_DIR` is unset, or when
    /// neither the relevant XDG var nor `HOME` is available.
    pub(crate) fn resolve() -> Result<Self, CliError> {
        let base = pohunek_paths::BasePaths::resolve().map_err(path_error)?;

        Ok(Self {
            runtime_dir: base.runtime_dir,
            socket: base.socket,
            data_dir: base.data_dir,
            log_dir: base.log_dir,
            cache_dir: base.cache_dir,
            config_home: base.config_home,
            config_dir: base.config_dir,
        })
    }

    /// Directory the launcher scripts (`pohunek-rofi`, `pohunek-launch-*`,
    /// `lib.sh`) are materialized into by `pohunek setup scripts`. They must be
    /// siblings because the shell launchers source `lib.sh` from their own
    /// directory.
    #[must_use]
    pub(crate) fn launcher_bin_dir(&self) -> PathBuf {
        self.data_dir.join(pohunek_paths::BIN_SUBDIR)
    }

    /// The user's sway config dir (`<config_home>/sway`). `pohunek setup sway`
    /// writes a drop-in under `<sway_config_dir>/config.d/`; it never edits the
    /// main sway config.
    #[must_use]
    pub(crate) fn sway_config_dir(&self) -> PathBuf {
        self.config_home.join(pohunek_paths::SWAY_CONFIG_DIR)
    }

    /// One-time legacy-to-worker migration manifest.
    #[must_use]
    pub(crate) fn worker_migration_manifest(&self) -> PathBuf {
        self.data_dir
            .join("migrations")
            .join("durable-session-workers.json")
    }
}

fn path_error(err: pohunek_paths::PathError) -> CliError {
    match err {
        pohunek_paths::PathError::MissingEnv { var } => CliError::MissingEnv { var },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use pohunek_paths::APP_DIR;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const VARS: [&str; 6] = [
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "HOME",
    ];

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
        std::env::temp_dir().join(format!("pohunek-cli-paths-{tag}-{}", std::process::id()))
    }

    fn set_all_present(base: &Path) {
        std::env::set_var("XDG_RUNTIME_DIR", base.join("run"));
        std::env::set_var("XDG_STATE_HOME", base.join("state"));
        std::env::set_var("XDG_DATA_HOME", base.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", base.join("cfg"));
        std::env::set_var("XDG_CACHE_HOME", base.join("cache"));
        std::env::set_var("HOME", base.join("home"));
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
}
