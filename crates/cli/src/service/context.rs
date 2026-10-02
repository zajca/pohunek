//! Host facts every `pohunek service` operation starts from.

// Rust guideline compliant 2026-09-30

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pohunek_paths::{BasePaths, PathEnv, HOME, XDG_RUNTIME_DIR};
use pohunek_platform::filesystem::TrustedDir;
use pohunek_platform::shell_env::{
    login_environment, login_environment_with, resolve_search_path, LoginEnvironment,
    LoginEnvironmentError, LoginShellSpec, PathPolicy, SearchPath, DARWIN_FALLBACK_DIRECTORIES,
    PRINTENV_EXECUTABLE,
};
use pohunek_platform::supervisor::{self, Namespace};

use super::error::{fs_error, io_error, Error};
use super::report::{DroppedPath, SearchPathReport};
use super::settings;

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

/// How the installer chooses the daemon job's executable search path.
///
/// The policy itself lives in [`pohunek_platform::shell_env`]; this value only
/// carries the host inputs so tests can drive every tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathDiscovery {
    /// The service manager's own `PATH` is adequate (systemd inherits the
    /// user manager's environment); the job definition carries no `PATH`.
    Unmanaged,
    /// The job receives a `PATH` resolved by the documented tier order.
    Managed {
        /// Login-shell probe to run first, or `None` to use the fallback list.
        login_shell: Option<LoginShellSpec>,
        /// Whether the probe shell is the built-in default because `$SHELL`
        /// was unset.
        shell_defaulted: bool,
        /// Fallback directory table, see [`DARWIN_FALLBACK_DIRECTORIES`].
        fallback_directories: Vec<String>,
    },
}

/// Where the install-time `PATH` discovery policy comes from.
#[derive(Debug, Clone)]
enum DiscoverySource {
    /// A policy fixed by the caller.
    Fixed(PathDiscovery),
    /// A policy built from the environment only when a fresh install asks.
    Environment {
        lookup: fn(&str) -> Option<std::ffi::OsString>,
        managed: bool,
    },
}

/// Paths, identity, and environment of the installing user.
#[derive(Debug, Clone)]
pub struct Context {
    paths: BasePaths,
    uid: u32,
    home: Option<PathBuf>,
    runtime_base: Option<PathBuf>,
    supervisor_dir: PathBuf,
    cli_executable: PathBuf,
    path_discovery: DiscoverySource,
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
            path_discovery: DiscoverySource::Fixed(PathDiscovery::Unmanaged),
        }
    }

    /// Returns this context with `path_discovery` choosing the daemon `PATH`.
    #[must_use]
    pub fn with_path_discovery(mut self, path_discovery: PathDiscovery) -> Self {
        self.path_discovery = DiscoverySource::Fixed(path_discovery);
        self
    }

    /// Returns this context reading the discovery environment through `lookup`
    /// when a fresh install needs it.
    ///
    /// Nothing is read or validated before [`Self::install_search_path`], so an
    /// unusable variable never blocks a command that uses the recorded
    /// configuration. `managed` selects login-shell discovery; without it the
    /// service manager's own `PATH` applies.
    #[must_use]
    pub fn with_environment_discovery(
        mut self,
        lookup: fn(&str) -> Option<std::ffi::OsString>,
        managed: bool,
    ) -> Self {
        self.path_discovery = DiscoverySource::Environment { lookup, managed };
        self
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
        )
        // Login-shell probing applies on macOS only; systemd hands the user
        // manager's environment to the daemon job. The target is chosen at run
        // time, not with `#[cfg]`, so every branch is type-checked everywhere.
        .with_environment_discovery(|name| std::env::var_os(name), cfg!(target_os = "macos")))
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

    /// Resolves the executable search path a fresh install records.
    ///
    /// Empty for [`PathDiscovery::Unmanaged`]. Otherwise the tier order of
    /// [`pohunek_platform::shell_env`] applies; a failed login-shell probe
    /// falls back to the directory list. The report carries the source, the
    /// failure, and every refused directory, so the caller can show them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SearchPath`] when no tier yields a usable directory.
    pub fn install_search_path(&self) -> Result<(SearchPath, SearchPathReport), Error> {
        let discovery = match &self.path_discovery {
            DiscoverySource::Fixed(discovery) => discovery.clone(),
            DiscoverySource::Environment { lookup, managed } => {
                if *managed {
                    managed_discovery_from_env(lookup)?
                } else {
                    PathDiscovery::Unmanaged
                }
            }
        };
        let PathDiscovery::Managed {
            login_shell,
            shell_defaulted,
            fallback_directories,
        } = &discovery
        else {
            return Ok((
                SearchPath::empty(),
                SearchPathReport {
                    source: "unmanaged",
                    entries: Vec::new(),
                    shell_used: None,
                    shell_defaulted: false,
                    login_shell_failure: None,
                    dropped: Vec::new(),
                },
            ));
        };
        let fallback: Vec<&str> = fallback_directories.iter().map(String::as_str).collect();
        let resolution = resolve_search_path(&PathPolicy {
            configured: None,
            login_shell: login_shell.as_ref(),
            fallback_directories: &fallback,
            home: self.home.as_deref(),
        })?;
        let report = SearchPathReport {
            source: resolution.source.as_str(),
            entries: resolution.path.entries().to_vec(),
            shell_used: resolution.shell,
            shell_defaulted: *shell_defaulted && login_shell.is_some(),
            login_shell_failure: resolution
                .login_shell_failure
                .map(|error| error.to_string()),
            dropped: resolution
                .untrusted
                .into_iter()
                .map(|dropped| DroppedPath {
                    path: dropped.entry,
                    reason: dropped.reason,
                })
                .collect(),
        };
        Ok((resolution.path, report))
    }

    /// Returns the bootstrap environment for the daemon's job definition.
    ///
    /// Every XDG root is passed explicitly, so the daemon resolves exactly the
    /// paths this CLI resolved regardless of the service manager's own
    /// environment. `XDG_RUNTIME_DIR` and `HOME` are passed only when set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NonUtf8Env`] for a root that is not UTF-8. Job
    /// definitions carry UTF-8 values only, and a daemon missing that
    /// variable would fall back to `HOME` and open other files than this
    /// CLI's, so the root is refused instead of left out.
    pub fn bootstrap_environment(&self) -> Result<BTreeMap<String, String>, Error> {
        let roots: [(&'static str, Option<&Path>); 6] = [
            (XDG_RUNTIME_DIR, self.runtime_base.as_deref()),
            (pohunek_paths::XDG_STATE_HOME, self.paths.state_dir.parent()),
            (pohunek_paths::XDG_DATA_HOME, self.paths.data_dir.parent()),
            (pohunek_paths::XDG_CACHE_HOME, self.paths.cache_dir.parent()),
            (
                pohunek_paths::XDG_CONFIG_HOME,
                Some(self.paths.config_home.as_path()),
            ),
            (HOME, self.home.as_deref()),
        ];
        let mut environment = BTreeMap::new();
        for (var, path) in roots {
            let Some(path) = path else { continue };
            let value = path.to_str().ok_or_else(|| Error::NonUtf8Env {
                var,
                path: path.to_path_buf(),
            })?;
            environment.insert(var.to_owned(), value.to_owned());
        }
        Ok(environment)
    }

    /// Returns the bootstrap environment after checking the daemon accepts it.
    ///
    /// The daemon validates every bootstrap variable with the job-definition
    /// rules and starts each session worker in `HOME`, refusing to become
    /// ready without an existing directory there. Install and upgrade apply
    /// the same rules before their first effect, so a registered daemon never
    /// fails on them.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::bootstrap_environment`],
    /// [`Error::MissingEnv`] without `HOME`, and [`Error::UnusableEnv`] for a
    /// value the job-definition rules reject or a `HOME` that is not an
    /// existing directory.
    pub fn service_environment(&self) -> Result<BTreeMap<String, String>, Error> {
        let environment = self.bootstrap_environment()?;
        for (var, value) in &environment {
            supervisor::validate_bootstrap_variable(var, value)
                .map_err(|source| unusable(var, Path::new(value), source.to_string()))?;
        }
        let home = environment
            .get(HOME)
            .map(Path::new)
            .ok_or_else(|| Error::MissingEnv {
                var: HOME.to_owned(),
            })?;
        supervisor::validate_working_directory(home)
            .map_err(|source| unusable(HOME, home, source.to_string()))?;
        // `is_dir` follows symlinks, exactly like the daemon's check.
        if !home.is_dir() {
            return Err(unusable(
                HOME,
                home,
                "it is not an existing directory".to_owned(),
            ));
        }
        Ok(environment)
    }
}

/// Builds the managed discovery policy for a user whose login shell is `shell`.
///
/// An unset `shell` selects the platform default login shell, which the report
/// says. `environment` holds the non-secret variables the shell needs to find
/// its startup files.
///
/// # Errors
///
/// Returns [`Error::UnusableEnv`] for a `shell` that is not absolute and
/// [`Error::NonUtf8Env`] for one that is not UTF-8: a set but unusable `$SHELL`
/// is refused before any install effect instead of replaced by a guess, and the
/// report that names the shell must stay serializable.
pub fn managed_path_discovery(
    shell: Option<PathBuf>,
    environment: Vec<(String, String)>,
) -> Result<PathDiscovery, Error> {
    let login = login_environment_with(shell, environment).map_err(login_environment_error)?;
    Ok(managed_discovery(login))
}

/// The managed policy for an already validated probe environment.
fn managed_discovery(login: LoginEnvironment) -> PathDiscovery {
    PathDiscovery::Managed {
        login_shell: Some(LoginShellSpec {
            shell: login.shell,
            printenv: PathBuf::from(PRINTENV_EXECUTABLE),
            environment: login.variables,
            timeout: settings::LOGIN_SHELL_TIMEOUT,
            max_output_bytes: settings::LOGIN_SHELL_OUTPUT,
        }),
        shell_defaulted: login.shell_defaulted,
        fallback_directories: DARWIN_FALLBACK_DIRECTORIES
            .iter()
            .map(|directory| (*directory).to_owned())
            .collect(),
    }
}

/// Maps a probe-environment failure to the service error of the same meaning.
fn login_environment_error(error: LoginEnvironmentError) -> Error {
    match error {
        LoginEnvironmentError::NonUtf8 { var, value } => Error::NonUtf8Env { var, path: value },
        LoginEnvironmentError::NotAbsolute { var, value } => {
            unusable(var, &value, "it is not an absolute path".to_owned())
        }
        LoginEnvironmentError::NulByte { var } => {
            unusable(var, Path::new(""), "it contains a NUL byte".to_owned())
        }
        other => unusable("login shell environment", Path::new(""), other.to_string()),
    }
}

/// Builds the managed policy from an environment lookup.
///
/// The environment, its validation, and the failures are those of
/// [`login_environment`], which the GUI shares. The lookup is injected so the
/// validation is type-checked and tested on every target.
fn managed_discovery_from_env(
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<PathDiscovery, Error> {
    login_environment(lookup)
        .map(managed_discovery)
        .map_err(login_environment_error)
}

/// Returns where the daemon definition goes for `paths` and `home`.
///
/// This is the systemd user unit directory `$XDG_CONFIG_HOME/systemd/user`
/// on Linux and `$HOME/Library/LaunchAgents` on macOS, as
/// [`Context::resolve`] picks it.
///
/// # Errors
///
/// Returns [`Error::MissingEnv`] on macOS when `home` is `None`.
#[cfg(target_os = "linux")]
pub fn default_supervisor_dir(paths: &BasePaths, _home: Option<&Path>) -> Result<PathBuf, Error> {
    Ok(paths.config_home.join(SYSTEMD_USER_UNITS))
}

/// Returns where the daemon definition goes for `paths` and `home`.
///
/// See the Linux variant; on macOS this is `$HOME/Library/LaunchAgents`.
///
/// # Errors
///
/// Returns [`Error::MissingEnv`] when `home` is `None`.
#[cfg(target_os = "macos")]
pub fn default_supervisor_dir(_paths: &BasePaths, home: Option<&Path>) -> Result<PathBuf, Error> {
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

fn unusable(var: &str, path: &Path, detail: String) -> Error {
    Error::UnusableEnv {
        var: var.to_owned(),
        path: path.to_path_buf(),
        detail,
    }
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

    /// Creates a temporary root and returns it with its canonical path.
    ///
    /// macOS temporary directories live below the `/var` symlink, which the
    /// trusted-directory checks and process executable paths never traverse.
    pub(crate) fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let root = pohunek_test_support::tempdir().expect("temp dir");
        let path = std::fs::canonicalize(root.path()).expect("canonical temp dir");
        (root, path)
    }

    /// Creates `path` below `root`, every new component with mode 0755
    /// regardless of the process umask, as the path trust checks require.
    pub(crate) fn make_dirs(root: &Path, path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let mut current = root.to_path_buf();
        for component in path.strip_prefix(root).expect("below root").components() {
            current.push(component);
            if !current.exists() {
                std::fs::create_dir(&current).expect("create directory");
                std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod directory");
            }
        }
    }

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

    /// Builds a context entirely below `root`, creating its `HOME`.
    pub(crate) fn context(root: &Path) -> Context {
        std::fs::create_dir_all(root.join("home")).expect("create test HOME");
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
    fn profile_selectors_reach_the_probe_and_must_be_absolute_utf8() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let env = |pairs: Vec<(&'static str, OsString)>| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        let PathDiscovery::Managed {
            login_shell: Some(spec),
            ..
        } = managed_discovery_from_env(env(vec![
            ("ZDOTDIR", OsString::from("/home/u/.config/zsh")),
            ("XDG_CONFIG_HOME", OsString::from("")),
        ]))
        .expect("discovery")
        else {
            panic!("managed discovery");
        };
        // An empty selector counts as unset, as shells treat it.
        assert_eq!(
            spec.environment,
            [
                ("ZDOTDIR".to_owned(), "/home/u/.config/zsh".to_owned()),
                ("SHELL".to_owned(), "/bin/zsh".to_owned()),
            ]
        );
        for (var, value, code) in [
            (
                "ZDOTDIR",
                OsString::from("relative/zsh"),
                "service_environment_invalid",
            ),
            (
                "XDG_CONFIG_HOME",
                OsString::from("cfg"),
                "service_environment_invalid",
            ),
            (
                "ZDOTDIR",
                OsString::from_vec(b"/z\xff".to_vec()),
                "service_environment_not_utf8",
            ),
        ] {
            let error = managed_discovery_from_env(env(vec![(var, value)])).expect_err(var);
            assert_eq!(error.code(), code, "{var}");
        }
    }

    #[test]
    fn a_custom_profile_location_supplies_a_prefix_outside_the_fallback_list() {
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt as _;

        let (_root, root) = temp_root();
        let root = root.as_path();
        let zdotdir = root.join("zdotdir");
        let prefix = root.join("nix/profile/bin");
        make_dirs(root, &zdotdir);
        make_dirs(root, &prefix);
        // A fake zsh that reads its PATH from `$ZDOTDIR/path`, as a profile
        // under `ZDOTDIR` would set it, and ignores everything else.
        std::fs::write(zdotdir.join("path"), prefix.display().to_string()).expect("profile");
        let shell = root.join("fake-zsh");
        std::fs::write(
            &shell,
            "#!/bin/sh\n[ -n \"$ZDOTDIR\" ] && PATH=\"$(cat \"$ZDOTDIR/path\")\"; export PATH\nexec /bin/sh -c \"$3\"\n",
        )
        .expect("shell");
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let shell_value = OsString::from(shell.as_os_str());
        let zdotdir_value = OsString::from(zdotdir.as_os_str());
        let PathDiscovery::Managed {
            login_shell,
            shell_defaulted,
            ..
        } = managed_discovery_from_env(|name| match name {
            "SHELL" => Some(shell_value.clone()),
            "ZDOTDIR" => Some(zdotdir_value.clone()),
            _ => None,
        })
        .expect("discovery")
        else {
            panic!("managed discovery");
        };
        let context = context(root).with_path_discovery(PathDiscovery::Managed {
            login_shell,
            shell_defaulted,
            fallback_directories: Vec::new(),
        });
        let (path, report) = context.install_search_path().expect("resolve");
        assert_eq!(path.entries(), [prefix]);
        assert_eq!(report.source, "login_shell");
    }

    #[test]
    fn a_non_utf8_shell_or_login_variable_fails_before_any_effect() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let non_utf8 = OsString::from_vec(b"/bin/\xff".to_vec());
        for var in ["SHELL", "HOME", "USER", "LOGNAME"] {
            let bad = non_utf8.clone();
            let error = managed_discovery_from_env(|name| {
                if name == var {
                    Some(bad.clone())
                } else if name == "SHELL" {
                    Some(OsString::from("/bin/zsh"))
                } else {
                    None
                }
            })
            .expect_err(var);
            assert!(
                matches!(&error, Error::NonUtf8Env { var: found, .. } if *found == var),
                "{var}: {error:?}"
            );
            assert_eq!(error.code(), "service_environment_not_utf8");
        }
        // Valid values pass through and an unset SHELL selects the default.
        let PathDiscovery::Managed {
            login_shell: Some(spec),
            shell_defaulted,
            ..
        } = managed_discovery_from_env(|name| (name == "HOME").then(|| OsString::from("/home/u")))
            .expect("discovery")
        else {
            panic!("managed discovery");
        };
        assert!(shell_defaulted);
        assert_eq!(
            spec.environment,
            [
                ("HOME".to_owned(), "/home/u".to_owned()),
                // The defaulted shell is passed on as `$SHELL` too.
                ("SHELL".to_owned(), "/bin/zsh".to_owned()),
            ]
        );
    }

    #[test]
    fn bootstrap_environment_names_every_root_explicitly() {
        let (_root, root) = temp_root();
        let context = context(root.as_path());
        let environment = context.bootstrap_environment().expect("utf-8 roots");
        let path = |name: &str| {
            root.as_path()
                .join(name)
                .to_str()
                .expect("utf-8")
                .to_owned()
        };
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
    fn bootstrap_environment_refuses_a_root_that_is_not_utf8() {
        use std::os::unix::ffi::OsStrExt as _;

        let (_root, root) = temp_root();
        let mut data_home = root.as_os_str().to_owned();
        data_home.push(std::ffi::OsStr::from_bytes(b"/data-\xff"));
        let mut env = PathEnv {
            xdg_runtime_dir: Some(OsString::from(root.join("run"))),
            xdg_data_home: Some(data_home.clone()),
            xdg_state_home: Some(OsString::from(root.join("state"))),
            xdg_cache_home: Some(OsString::from(root.join("cache"))),
            xdg_config_home: Some(OsString::from(root.join("config"))),
            home: Some(OsString::from(root.join("home"))),
        };
        let resolve = |env: &PathEnv| {
            BasePaths::resolve_for(
                Platform::current().expect("supported platform"),
                nix::unistd::Uid::effective().as_raw(),
                env,
            )
            .expect("the path contract accepts non-UTF-8 roots")
        };
        let context_of = |paths: BasePaths, home: PathBuf| {
            Context::new(
                paths,
                nix::unistd::Uid::effective().as_raw(),
                Some(home),
                Some(root.join("run")),
                root.join("units"),
                PathBuf::from("/usr/bin/pohunek"),
            )
        };

        let error = context_of(resolve(&env), root.join("home"))
            .bootstrap_environment()
            .expect_err("a non-UTF-8 XDG_DATA_HOME is refused");
        let Error::NonUtf8Env { var, path } = &error else {
            panic!("unexpected error {error:?}");
        };
        assert_eq!(*var, "XDG_DATA_HOME");
        assert_eq!(path.as_os_str(), data_home.as_os_str());
        assert_eq!(error.code(), "service_environment_not_utf8");

        env.xdg_data_home = Some(OsString::from(root.join("data")));
        let mut home = root.as_os_str().to_owned();
        home.push(std::ffi::OsStr::from_bytes(b"/home-\xfe"));
        let error = context_of(resolve(&env), PathBuf::from(home))
            .bootstrap_environment()
            .expect_err("a non-UTF-8 HOME is refused");
        assert!(
            matches!(&error, Error::NonUtf8Env { var, .. } if *var == "HOME"),
            "{error:?}"
        );
    }

    #[test]
    fn namespace_is_deterministic_and_depends_on_the_roots() {
        let (_first, first) = temp_root();
        let (_second, second) = temp_root();
        let a = context(&first).namespace().expect("namespace");
        let again = context(&first).namespace().expect("namespace");
        let b = context(&second).namespace().expect("namespace");
        assert_eq!(a, again);
        assert_ne!(a, b);
        assert!(first.join("state/pohunek").is_dir());
        assert!(first.join("run/pohunek").is_dir());
    }
}
