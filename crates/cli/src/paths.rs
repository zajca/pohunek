//! CLI-side path resolution.
//!
//! The CLI must resolve the same control-socket and state paths the daemon uses
//! (see `docs/architecture.md` "Configuration, State, and Log Storage"). Like the
//! daemon, a missing required base directory is a fail-fast error: no silent
//! invented fallbacks (hard project rule).

use std::path::{Component, Path, PathBuf};

use crate::error::CliError;

const APP_DIR: &str = "pohunek";
const SOCKET_NAME: &str = "daemon.sock";

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

        let cache_home = xdg_or_home_relative("XDG_CACHE_HOME", &[".cache"])?;
        let cache_dir = cache_home.join(APP_DIR);

        let config_home = xdg_or_home_relative("XDG_CONFIG_HOME", &[".config"])?;
        let config_dir = config_home.join(APP_DIR);

        Ok(Self {
            runtime_dir,
            socket,
            data_dir,
            log_dir,
            cache_dir,
            config_home,
            config_dir,
        })
    }

    /// Directory the launcher scripts (`pohunek-rofi`, `pohunek-launch-*`,
    /// `lib.sh`) are materialized into by `pohunek setup scripts`. They must be
    /// siblings because the shell launchers source `lib.sh` from their own
    /// directory.
    #[must_use]
    pub(crate) fn launcher_bin_dir(&self) -> PathBuf {
        self.data_dir.join("bin")
    }

    /// The user's sway config dir (`<config_home>/sway`). `pohunek setup sway`
    /// writes a drop-in under `<sway_config_dir>/config.d/`; it never edits the
    /// main sway config.
    #[must_use]
    pub(crate) fn sway_config_dir(&self) -> PathBuf {
        self.config_home.join("sway")
    }

    /// Directory where assistant knowledge bundles are cached.
    #[must_use]
    pub(crate) fn assistant_bundle_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("knowledge")
    }

    /// Runtime directory for assistant material generated for one session or launch.
    #[must_use]
    pub(crate) fn assistant_runtime_dir(&self, session_or_launch_id: &str) -> Option<PathBuf> {
        valid_runtime_id(session_or_launch_id).map(|id| self.runtime_dir.join("assistant").join(id))
    }
}

fn valid_runtime_id(id: &str) -> Option<&Path> {
    let path = Path::new(id);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Some(path),
        _ => None,
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
    let home = std::env::var("HOME").map_err(|_err| CliError::MissingEnv {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn assistant_dirs_have_expected_shape() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("assistant");
        set_all_present(&base);
        let paths = Paths::resolve().expect("resolve with all base vars set");
        assert_eq!(
            paths.assistant_bundle_cache_dir(),
            base.join("cache").join(APP_DIR).join("knowledge")
        );
        assert_eq!(
            paths.assistant_runtime_dir("launch-123"),
            Some(
                base.join("run")
                    .join(APP_DIR)
                    .join("assistant")
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
}
