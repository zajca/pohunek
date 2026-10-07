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
    /// derive pohunek's own config dir.
    pub(crate) config_home: PathBuf,
    /// pohunek's config dir (`<config_home>/pohunek`) — holds `attach.conf`
    /// and the `prompts/*.tmpl` project-action templates.
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
