//! Host facts every `pohunek service` operation starts from.

// Rust guideline compliant 2026-09-24

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pohunek_paths::{BasePaths, PathEnv, HOME, XDG_RUNTIME_DIR};
use pohunek_platform::filesystem::TrustedDir;
use pohunek_platform::supervisor::Namespace;

use super::error::{fs_error, io_error, Error};

/// Mode of the application state and runtime roots.
///
/// The daemon creates both owner-private; the installer creates them the
/// same way so their canonical paths exist before the namespace is derived.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Default installation prefix below `$HOME`.
///
/// `~/.local` is the XDG convention for per-user installs; `~/.local/bin` is
/// on `PATH` in common shells.
pub const DEFAULT_PREFIX: &str = ".local";

/// Where the daemon's persistent service definition lives, relative to its base.
#[cfg(target_os = "linux")]
const SYSTEMD_USER_UNITS: &str = "systemd/user";

/// Where launchd login agents live, relative to `$HOME`.
#[cfg(target_os = "macos")]
const LAUNCH_AGENTS: &str = "Library/LaunchAgents";

/// Paths, identity, and environment of the installing user.
#[derive(Debug, Clone)]
pub struct Context {
    paths: BasePaths,
    uid: u32,
    home: Option<PathBuf>,
    runtime_base: Option<PathBuf>,
    supervisor_dir: PathBuf,
    cli_executable: PathBuf,
}

impl Context {
    /// Creates a context from explicit facts.
    ///
    /// `supervisor_dir` is where the daemon definition goes: the systemd user
    /// unit directory on Linux, `~/Library/LaunchAgents` on macOS.
    /// `runtime_base` is `$XDG_RUNTIME_DIR` when it is set.
    #[must_use]
    pub fn new(
        paths: BasePaths,
        uid: u32,
        home: Option<PathBuf>,
        runtime_base: Option<PathBuf>,
        supervisor_dir: PathBuf,
        cli_executable: PathBuf,
    ) -> Self {
        Self {
            paths,
            uid,
            home,
            runtime_base,
            supervisor_dir,
            cli_executable,
        }
    }

    /// Resolves the context from this process's environment.
    ///
    /// The daemon definition goes to `$XDG_CONFIG_HOME/systemd/user` on Linux
    /// and `$HOME/Library/LaunchAgents` on macOS.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingEnv`] or [`Error::Paths`] when the XDG
    /// environment is incomplete, and [`Error::Io`] when the running
    /// executable cannot be located.
    pub fn resolve() -> Result<Self, Error> {
        let paths = BasePaths::resolve().map_err(path_error)?;
        let env = PathEnv::capture();
        let home = env.home.clone().map(PathBuf::from);
        let runtime_base = env.xdg_runtime_dir.clone().map(PathBuf::from);
        let supervisor_dir = default_supervisor_dir(&paths, home.as_deref())?;
        let cli_executable = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(io_error("locate", "the running pohunek executable"))?;
        Ok(Self::new(
            paths,
            nix::unistd::Uid::effective().as_raw(),
            home,
            runtime_base,
            supervisor_dir,
            cli_executable,
        ))
    }

    /// Returns the shared application paths.
    #[must_use]
    pub fn paths(&self) -> &BasePaths {
        &self.paths
    }

    /// Returns the effective user ID.
    #[must_use]
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Returns the directory receiving the daemon definition.
    #[must_use]
    pub fn supervisor_dir(&self) -> &Path {
        &self.supervisor_dir
    }

    /// Returns the canonical path of the running CLI.
    #[must_use]
    pub fn cli_executable(&self) -> &Path {
        &self.cli_executable
    }

    /// Returns the default prefix, `$HOME/.local`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingEnv`] when `HOME` is unset.
    pub fn default_prefix(&self) -> Result<PathBuf, Error> {
        self.home
            .as_ref()
            .map(|home| home.join(DEFAULT_PREFIX))
            .ok_or_else(|| Error::MissingEnv {
                var: HOME.to_owned(),
            })
    }

    /// Returns `service.toml`'s path.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        pohunek_service_config::file_path(&self.paths)
    }

    /// Returns the canonical application state and runtime roots.
    ///
    /// Missing roots are created owner-private first, exactly as the daemon
    /// would, so the namespace never depends on whether the daemon ran yet.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when a root or ancestor is unsafe.
    pub fn roots(&self) -> Result<(PathBuf, PathBuf), Error> {
        Ok((
            canonical_root(&self.paths.state_dir)?,
            canonical_root(&self.paths.runtime_dir)?,
        ))
    }

    /// Derives this installation's namespace from the UID and canonical roots.
    ///
    /// # Errors
    ///
    /// See [`Self::roots`].
    pub fn namespace(&self) -> Result<Namespace, Error> {
        let (state, runtime) = self.roots()?;
        Ok(Namespace::derive(self.uid, &state, &runtime))
    }

    /// Returns the bootstrap environment for the daemon's job definition.
    ///
    /// Every XDG root is passed explicitly, so the daemon resolves exactly the
    /// paths this CLI resolved regardless of the service manager's own
    /// environment. `XDG_RUNTIME_DIR` and `HOME` are passed only when set.
    #[must_use]
    pub fn bootstrap_environment(&self) -> BTreeMap<String, String> {
        let mut environment = BTreeMap::new();
        let mut insert = |key: &str, path: Option<&Path>| {
            if let Some(value) = path.and_then(Path::to_str) {
                environment.insert(key.to_owned(), value.to_owned());
            }
        };
        insert(XDG_RUNTIME_DIR, self.runtime_base.as_deref());
        insert(pohunek_paths::XDG_STATE_HOME, self.paths.state_dir.parent());
        insert(pohunek_paths::XDG_DATA_HOME, self.paths.data_dir.parent());
        insert(pohunek_paths::XDG_CACHE_HOME, self.paths.cache_dir.parent());
        insert(
            pohunek_paths::XDG_CONFIG_HOME,
            Some(self.paths.config_home.as_path()),
        );
        insert(HOME, self.home.as_deref());
        environment
    }
}

#[cfg(target_os = "linux")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "one signature for both targets; macOS needs HOME"
)]
fn default_supervisor_dir(paths: &BasePaths, _home: Option<&Path>) -> Result<PathBuf, Error> {
    Ok(paths.config_home.join(SYSTEMD_USER_UNITS))
}

#[cfg(target_os = "macos")]
fn default_supervisor_dir(_paths: &BasePaths, home: Option<&Path>) -> Result<PathBuf, Error> {
    home.map(|home| home.join(LAUNCH_AGENTS))
        .ok_or_else(|| Error::MissingEnv {
            var: HOME.to_owned(),
        })
}

fn canonical_root(path: &Path) -> Result<PathBuf, Error> {
    TrustedDir::open_or_create_absolute(path, PRIVATE_DIR_MODE)
        .map_err(|source| fs_error("prepare application directory", source))?;
    std::fs::canonicalize(path).map_err(io_error("canonicalize", path))
}

fn path_error(error: pohunek_paths::PathError) -> Error {
    match error {
        pohunek_paths::PathError::MissingEnv { var } => Error::MissingEnv { var },
        other => Error::Paths(other),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::ffi::OsString;

    use pohunek_paths::Platform;

    use super::*;

    /// Resolves application paths below `root` without touching the process environment.
    pub(crate) fn paths(root: &Path) -> BasePaths {
        let env = PathEnv {
            xdg_runtime_dir: Some(OsString::from(root.join("run"))),
            xdg_data_home: Some(OsString::from(root.join("data"))),
            xdg_state_home: Some(OsString::from(root.join("state"))),
            xdg_cache_home: Some(OsString::from(root.join("cache"))),
            xdg_config_home: Some(OsString::from(root.join("config"))),
            home: Some(OsString::from(root.join("home"))),
        };
        BasePaths::resolve_for(
            Platform::current().expect("supported platform"),
            nix::unistd::Uid::effective().as_raw(),
            &env,
        )
        .expect("resolve test paths")
    }

    /// Builds a context entirely below `root`.
    pub(crate) fn context(root: &Path) -> Context {
        Context::new(
            paths(root),
            nix::unistd::Uid::effective().as_raw(),
            Some(root.join("home")),
            Some(root.join("run")),
            root.join("units"),
            PathBuf::from("/usr/bin/pohunek"),
        )
    }

    #[test]
    fn bootstrap_environment_names_every_root_explicitly() {
        let root = pohunek_test_support::tempdir().expect("temp dir");
        let context = context(root.path());
        let environment = context.bootstrap_environment();
        let path = |name: &str| root.path().join(name).to_str().expect("utf-8").to_owned();
        assert_eq!(
            environment,
            BTreeMap::from([
                ("HOME".to_owned(), path("home")),
                ("XDG_CACHE_HOME".to_owned(), path("cache")),
                ("XDG_CONFIG_HOME".to_owned(), path("config")),
                ("XDG_DATA_HOME".to_owned(), path("data")),
                ("XDG_RUNTIME_DIR".to_owned(), path("run")),
                ("XDG_STATE_HOME".to_owned(), path("state")),
            ])
        );
    }

    #[test]
    fn namespace_is_deterministic_and_depends_on_the_roots() {
        let first = pohunek_test_support::tempdir().expect("temp dir");
        let second = pohunek_test_support::tempdir().expect("temp dir");
        let a = context(first.path()).namespace().expect("namespace");
        let again = context(first.path()).namespace().expect("namespace");
        let b = context(second.path()).namespace().expect("namespace");
        assert_eq!(a, again);
        assert_ne!(a, b);
        assert!(first.path().join("state/pohunek").is_dir());
        assert!(first.path().join("run/pohunek").is_dir());
    }
}
