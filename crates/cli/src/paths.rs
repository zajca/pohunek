//! CLI-side path resolution.
//!
//! The CLI must resolve the same control-socket and state paths the daemon uses
//! (see `docs/architecture.md` "Configuration, State, and Log Storage"). Like the
//! daemon, a missing required base directory is a fail-fast error: no silent
//! invented fallbacks (hard project rule).

use std::path::PathBuf;

use pohunek_client::OriginSource;
use pohunek_paths::{PathEnv, Platform};

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
    /// Where daemon connections made with these paths take their request
    /// origin from. [`Self::resolve`] selects the process environment, so the
    /// binary attributes requests to the session it runs in.
    pub(crate) origin_source: OriginSource,
}

impl Paths {
    /// Resolve only the XDG data base directory.
    ///
    /// Shell completion installation uses shell-owned paths adjacent to, rather
    /// than inside, pohunek's data directory.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::MissingEnv`] when neither `XDG_DATA_HOME` nor `HOME`
    /// is available.
    pub(crate) fn data_home_only() -> Result<PathBuf, CliError> {
        pohunek_paths::data_home().map_err(path_error)
    }

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
        Self::resolve_from(&PathEnv::capture())
    }

    /// Resolve CLI paths from explicit environment inputs.
    ///
    /// # Errors
    ///
    /// Same as [`Self::resolve`], judged against `env` instead of the process
    /// environment.
    fn resolve_from(env: &PathEnv) -> Result<Self, CliError> {
        let platform = Platform::current().map_err(path_error)?;
        let effective_uid = nix::unistd::Uid::effective().as_raw();
        let base = pohunek_paths::BasePaths::resolve_for(platform, effective_uid, env)
            .map_err(path_error)?;

        Ok(Self {
            runtime_dir: base.runtime_dir,
            socket: base.socket,
            data_dir: base.data_dir,
            log_dir: base.log_dir,
            cache_dir: base.cache_dir,
            config_home: base.config_home,
            config_dir: base.config_dir,
            origin_source: OriginSource::Environment,
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
        other => CliError::Paths(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use pohunek_paths::APP_DIR;

    /// Returns a short base that is only resolved, never created or read.
    ///
    /// It stays short so the derived daemon socket path fits the 103-byte
    /// `sun_path` limit of macOS.
    fn tmp_base(tag: &str) -> PathBuf {
        Path::new("/work").join(format!("pohunek-cli-paths-{tag}-{}", std::process::id()))
    }

    fn all_present(base: &Path) -> PathEnv {
        PathEnv {
            xdg_runtime_dir: Some(base.join("run").into_os_string()),
            xdg_state_home: Some(base.join("state").into_os_string()),
            xdg_data_home: Some(base.join("data").into_os_string()),
            xdg_config_home: Some(base.join("cfg").into_os_string()),
            xdg_cache_home: Some(base.join("cache").into_os_string()),
            home: Some(base.join("home").into_os_string()),
        }
    }

    #[test]
    fn cache_dir_from_xdg_cache_home() {
        let base = tmp_base("xdg-cache");
        let paths =
            Paths::resolve_from(&all_present(&base)).expect("resolve with all base vars set");
        assert_eq!(paths.cache_dir, base.join("cache").join(APP_DIR));
    }

    #[test]
    fn cache_dir_falls_back_to_home_dot_cache() {
        let base = tmp_base("home-cache");
        let env = PathEnv {
            xdg_cache_home: None,
            ..all_present(&base)
        };
        let paths = Paths::resolve_from(&env).expect("resolve with XDG_CACHE_HOME unset");
        assert_eq!(
            paths.cache_dir,
            base.join("home").join(".cache").join(APP_DIR)
        );
    }
}
