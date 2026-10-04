//! Shared XDG path contract for pohunek.
//!
//! Runtime, state, data, cache, and config paths are part of the local
//! daemon/client contract. This crate keeps the layout and fail-fast environment
//! rules in one place while callers map [`PathError`] into their own public error
//! types.

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-10-04

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Application directory under XDG base directories.
pub const APP_DIR: &str = "pohunek";
/// Control socket filename under [`BasePaths::runtime_dir`].
pub const SOCKET_NAME: &str = "daemon.sock";
/// Daemon single-instance lock filename under [`BasePaths::runtime_dir`].
pub const LOCK_NAME: &str = "daemon.lock";
/// Structured log subdirectory under the app state directory.
pub const LOGS_SUBDIR: &str = "logs";
/// Executable subdirectory under an installation prefix.
pub const BIN_SUBDIR: &str = "bin";
/// Assistant knowledge cache subdirectory under the app cache directory.
pub const KNOWLEDGE_CACHE_SUBDIR: &str = "knowledge";
/// Assistant runtime subdirectory under the app runtime directory.
pub const ASSISTANT_RUNTIME_SUBDIR: &str = "assistant";
/// Daemon metadata store filename under [`BasePaths::data_dir`].
pub const METADATA_STORE_NAME: &str = "metadata.jsonl";
/// Infix between a metadata store name and its pre-migration backup suffix.
const SCHEMA_BACKUP_INFIX: &str = ".pre-schema-";
/// Suffix marker of a backup that is still being written.
const SCHEMA_BACKUP_TEMP_MARKER: &str = "tmp";
/// Per-session worker subdirectory under runtime and state directories.
pub const WORKERS_SUBDIR: &str = "workers";
/// Worker control socket filename.
pub const WORKER_SOCKET_NAME: &str = "control.sock";
/// Name prefix of every staged Unix socket bind.
///
/// Daemon and worker listeners bind `<dir>/<prefix><random hex>` and then
/// rename the socket to its published name, so the staged pathname, not the
/// published one, is the longest one `bind(2)` sees. Pass it to
/// `TrustedDir::bind_unix_listener_staged` of `pohunek-platform`.
pub const STAGED_SOCKET_PREFIX: &str = ".s";
/// Hexadecimal characters in the random suffix of a staged socket name.
///
/// Two per random byte of the platform's staging names; `pohunek-platform`
/// asserts the pairing at compile time, so a longer staging name cannot
/// silently outgrow [`validate_staged_socket_path`].
pub const STAGED_SOCKET_SUFFIX_CHARS: usize = 16;
/// Owner-private durable host-state subdirectory under [`BasePaths::state_dir`].
pub const HOST_STATE_SUBDIR: &str = "host";
/// Stable host identity record filename.
pub const HOST_IDENTITY_NAME: &str = "identity.json";
/// Host approval signing secret filename.
pub const HOST_APPROVAL_KEY_NAME: &str = "approval.key";
/// Public host governance record filename.
pub const HOST_GOVERNANCE_NAME: &str = "governance.json";
/// Cross-process host-state lock filename.
pub const HOST_STATE_LOCK_NAME: &str = "state.lock";
/// Owner-private runtime package store subdirectory under
/// [`BasePaths::state_dir`].
///
/// It holds the package registry record and the content-addressed package
/// roots, so it must be an exact `0700` directory owned by the daemon user.
pub const PLUGINS_SUBDIR: &str = "plugins";

/// Subdirectory for launchd definitions (below state) and job logs (below logs).
pub const LAUNCHD_SUBDIR: &str = "launchd";
/// Installation subdirectory holding versioned private executables.
pub const LIBEXEC_SUBDIR: &str = "libexec";
/// Daemon executable name inside a version directory.
pub const DAEMON_EXECUTABLE_NAME: &str = "pohunekd";
/// Session-worker executable name inside a version directory.
pub const WORKER_EXECUTABLE_NAME: &str = "pohunek-sessiond";
/// Upper bound for a version directory name; release versions are far shorter.
pub const MAX_INSTALL_VERSION_BYTES: usize = 64;

/// XDG environment variable carrying the runtime base directory.
pub const XDG_RUNTIME_DIR: &str = "XDG_RUNTIME_DIR";
/// XDG environment variable carrying the data base directory.
pub const XDG_DATA_HOME: &str = "XDG_DATA_HOME";
/// XDG environment variable carrying the state base directory.
pub const XDG_STATE_HOME: &str = "XDG_STATE_HOME";
/// XDG environment variable carrying the cache base directory.
pub const XDG_CACHE_HOME: &str = "XDG_CACHE_HOME";
/// XDG environment variable carrying the config base directory.
pub const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
/// Home directory fallback variable.
pub const HOME: &str = "HOME";

const HOME_DATA_RELATIVE: &[&str] = &[".local", "share"];
const HOME_STATE_RELATIVE: &[&str] = &[".local", "state"];
const HOME_CACHE_RELATIVE: &[&str] = &[".cache"];
const HOME_CONFIG_RELATIVE: &[&str] = &[".config"];

/// Darwin's `sockaddr_un.sun_path` capacity excluding its native terminator.
pub const DARWIN_SOCKET_PATH_MAX_BYTES: usize = 103;
/// Linux's `sockaddr_un.sun_path` capacity excluding its native terminator.
pub const LINUX_SOCKET_PATH_MAX_BYTES: usize = 107;

/// Supported path-resolution platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Platform {
    /// Linux keeps the required `XDG_RUNTIME_DIR` contract.
    Linux,
    /// macOS defaults the runtime root when `XDG_RUNTIME_DIR` is absent.
    MacOs,
}

impl Platform {
    /// Returns the platform selected by the compilation target.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::UnsupportedPlatform`] on unsupported targets.
    pub fn current() -> Result<Self, PathError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Self::Linux)
        }
        #[cfg(target_os = "macos")]
        {
            Ok(Self::MacOs)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err(PathError::UnsupportedPlatform {
                target: std::env::consts::OS.to_owned(),
            })
        }
    }

    /// Returns the maximum encoded pathname bytes for a Unix socket.
    #[must_use]
    pub const fn socket_path_max_bytes(self) -> usize {
        match self {
            Self::Linux => LINUX_SOCKET_PATH_MAX_BYTES,
            Self::MacOs => DARWIN_SOCKET_PATH_MAX_BYTES,
        }
    }
}

/// Environment inputs used by path resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathEnv {
    /// Explicit runtime base directory.
    pub xdg_runtime_dir: Option<OsString>,
    /// Explicit data base directory.
    pub xdg_data_home: Option<OsString>,
    /// Explicit state base directory.
    pub xdg_state_home: Option<OsString>,
    /// Explicit cache base directory.
    pub xdg_cache_home: Option<OsString>,
    /// Explicit config base directory.
    pub xdg_config_home: Option<OsString>,
    /// Home directory used only for documented XDG fallbacks.
    pub home: Option<OsString>,
}

impl PathEnv {
    /// Captures path inputs from the current process environment.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            xdg_runtime_dir: std::env::var_os(XDG_RUNTIME_DIR),
            xdg_data_home: std::env::var_os(XDG_DATA_HOME),
            xdg_state_home: std::env::var_os(XDG_STATE_HOME),
            xdg_cache_home: std::env::var_os(XDG_CACHE_HOME),
            xdg_config_home: std::env::var_os(XDG_CONFIG_HOME),
            home: std::env::var_os(HOME),
        }
    }

    fn value(&self, key: &str) -> Option<&OsStr> {
        match key {
            XDG_RUNTIME_DIR => self.xdg_runtime_dir.as_deref(),
            XDG_DATA_HOME => self.xdg_data_home.as_deref(),
            XDG_STATE_HOME => self.xdg_state_home.as_deref(),
            XDG_CACHE_HOME => self.xdg_cache_home.as_deref(),
            XDG_CONFIG_HOME => self.xdg_config_home.as_deref(),
            HOME => self.home.as_deref(),
            _ => None,
        }
    }
}

/// Unix socket role used in path diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SocketKind {
    /// Public daemon control socket.
    Daemon,
    /// Private per-session worker control socket.
    Worker,
    /// Auxiliary local socket used by lifecycle tooling or tests.
    Auxiliary,
}

impl fmt::Display for SocketKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Daemon => "daemon",
            Self::Worker => "worker",
            Self::Auxiliary => "auxiliary",
        };
        f.write_str(name)
    }
}

/// Invalid environment path reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidPathReason {
    /// The variable is present with an empty value.
    Empty,
    /// The configured path is not absolute.
    NotAbsolute,
    /// The configured path contains a parent component.
    ParentComponent,
    /// The configured path contains a NUL byte.
    ContainsNul,
}

impl fmt::Display for InvalidPathReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::Empty => "must not be empty when present",
            Self::NotAbsolute => "must be an absolute path",
            Self::ParentComponent => "must not contain a parent-directory component",
            Self::ContainsNul => "must not contain a NUL byte",
        };
        f.write_str(reason)
    }
}

/// Path-resolution error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// A required environment variable is missing.
    #[error("required environment variable {var} is not set (no safe default exists)")]
    MissingEnv {
        /// Missing variable name, or an actionable `XDG_* or HOME` pair.
        var: String,
    },
    /// An explicitly configured environment path is malformed.
    #[error("environment variable {var} {reason}")]
    InvalidEnv {
        /// Invalid variable name.
        var: String,
        /// Validation failure.
        reason: InvalidPathReason,
    },
    /// An installation prefix is not an absolute normalized path.
    #[error("installation prefix must be an absolute normalized path: {}", path.display())]
    InvalidInstallPrefix {
        /// Rejected prefix.
        path: PathBuf,
    },
    /// A Unix socket pathname contains an interior NUL byte.
    #[error("{kind} socket path contains a NUL byte: {}", path.display())]
    SocketPathContainsNul {
        /// Socket role.
        kind: SocketKind,
        /// Rejected socket path.
        path: PathBuf,
    },
    /// A Unix socket pathname is not absolute.
    #[error("{kind} socket path must be absolute: {}", path.display())]
    SocketPathNotAbsolute {
        /// Socket role.
        kind: SocketKind,
        /// Rejected socket path.
        path: PathBuf,
    },
    /// A Unix socket pathname contains a parent-directory component.
    #[error(
        "{kind} socket path must not contain a parent-directory component: {}",
        path.display()
    )]
    SocketPathParentComponent {
        /// Socket role.
        kind: SocketKind,
        /// Rejected socket path.
        path: PathBuf,
    },
    /// A Unix socket pathname exceeds the native encoded limit.
    #[error(
        "{kind} socket path is {actual_bytes} bytes but {platform:?} permits at most {max_bytes} bytes including space for the native terminator: {}",
        path.display()
    )]
    SocketPathTooLong {
        /// Socket role.
        kind: SocketKind,
        /// Platform whose ABI limit was applied.
        platform: Platform,
        /// Rejected socket path.
        path: PathBuf,
        /// Actual encoded pathname length.
        actual_bytes: usize,
        /// Maximum encoded pathname length excluding the terminator.
        max_bytes: usize,
    },
    /// The compilation target has no supported path contract.
    #[error("path resolution is unsupported on target {target}")]
    UnsupportedPlatform {
        /// Unsupported target operating system.
        target: String,
    },
}

/// Shared XDG-derived path set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasePaths {
    /// Application runtime root selected by the platform contract.
    pub runtime_dir: PathBuf,
    /// Control Unix socket path.
    pub socket: PathBuf,
    /// Daemon single-instance lock path.
    pub lock: PathBuf,
    /// Structured log directory.
    pub log_dir: PathBuf,
    /// Application state directory.
    pub state_dir: PathBuf,
    /// User data directory.
    pub data_dir: PathBuf,
    /// User cache directory.
    pub cache_dir: PathBuf,
    /// XDG config base directory.
    pub config_home: PathBuf,
    /// App config directory.
    pub config_dir: PathBuf,
    platform: Platform,
}

impl BasePaths {
    /// Resolve all shared pohunek paths from process environment.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] when required configuration is missing, malformed,
    /// or produces an overlong daemon socket path.
    pub fn resolve() -> Result<Self, PathError> {
        let platform = Platform::current()?;
        let effective_uid = nix::unistd::Uid::effective().as_raw();
        Self::resolve_for(platform, effective_uid, &PathEnv::capture())
    }

    /// Resolves paths from explicit platform, identity, and environment inputs.
    ///
    /// This pure resolver allows callers to test every supported platform without
    /// mutating process environment or identity.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] for missing or malformed configuration and for an
    /// overlong daemon socket pathname.
    pub fn resolve_for(
        platform: Platform,
        effective_uid: u32,
        env: &PathEnv,
    ) -> Result<Self, PathError> {
        let runtime_dir = resolve_runtime_dir(platform, effective_uid, env)?;
        let socket = runtime_dir.join(SOCKET_NAME);
        validate_socket_path(&socket, platform, SocketKind::Daemon)?;
        let lock = runtime_dir.join(LOCK_NAME);
        let data_dir = resolve_xdg_or_home(env, XDG_DATA_HOME, HOME_DATA_RELATIVE)?.join(APP_DIR);
        let state_dir =
            resolve_xdg_or_home(env, XDG_STATE_HOME, HOME_STATE_RELATIVE)?.join(APP_DIR);
        let log_dir = state_dir.join(LOGS_SUBDIR);
        let cache_dir =
            resolve_xdg_or_home(env, XDG_CACHE_HOME, HOME_CACHE_RELATIVE)?.join(APP_DIR);
        let config_home = resolve_xdg_or_home(env, XDG_CONFIG_HOME, HOME_CONFIG_RELATIVE)?;
        let config_dir = config_home.join(APP_DIR);

        Ok(Self {
            runtime_dir,
            socket,
            lock,
            log_dir,
            state_dir,
            data_dir,
            cache_dir,
            config_home,
            config_dir,
            platform,
        })
    }

    /// Directory where assistant knowledge bundles are cached.
    #[must_use]
    pub fn assistant_bundle_cache_dir(&self) -> PathBuf {
        self.cache_dir.join(KNOWLEDGE_CACHE_SUBDIR)
    }

    /// Runtime directory for assistant material generated for one launch/session.
    #[must_use]
    pub fn assistant_runtime_dir(&self, launch_or_session_id: &str) -> Option<PathBuf> {
        valid_path_id(launch_or_session_id)
            .map(|id| self.runtime_dir.join(ASSISTANT_RUNTIME_SUBDIR).join(id))
    }

    /// Returns the owner-private root for live worker sockets.
    #[must_use]
    pub fn worker_runtime_root(&self) -> PathBuf {
        self.runtime_dir.join(WORKERS_SUBDIR)
    }

    /// Returns the owner-private root for durable worker journals.
    #[must_use]
    pub fn worker_state_root(&self) -> PathBuf {
        self.state_dir.join(WORKERS_SUBDIR)
    }

    /// Resolves one managed session's worker runtime directory.
    #[must_use]
    pub fn worker_runtime_dir(&self, session_id: &str) -> Option<PathBuf> {
        valid_worker_session_id(session_id).map(|id| self.worker_runtime_root().join(id))
    }

    /// Resolves one managed session's worker control socket.
    ///
    /// Returns `Ok(None)` for a session ID that cannot name a worker.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] when the published socket or the longest staged
    /// bind path beside it exceeds the platform limit
    /// ([`worker_socket_path`]).
    pub fn worker_socket(&self, session_id: &str) -> Result<Option<PathBuf>, PathError> {
        worker_socket_path(&self.worker_runtime_root(), session_id, self.platform)
    }

    /// Resolves one worker's durable journal path.
    #[must_use]
    pub fn worker_journal(&self, session_id: &str, worker_id: &str) -> Option<PathBuf> {
        let session_id = valid_worker_session_id(session_id)?;
        let worker_id = valid_worker_id(worker_id)?;
        Some(
            self.worker_state_root()
                .join(session_id)
                .join(worker_id)
                .with_extension("json"),
        )
    }

    /// Returns the owner-private directory holding launchd worker definitions.
    ///
    /// Worker job definitions live here rather than in `~/Library/LaunchAgents`
    /// so launchd never resurrects a worker at login.
    #[must_use]
    pub fn launchd_definitions_dir(&self) -> PathBuf {
        self.state_dir.join(LAUNCHD_SUBDIR)
    }

    /// Returns the directory receiving launchd job stdout and stderr files.
    #[must_use]
    pub fn launchd_log_dir(&self) -> PathBuf {
        self.log_dir.join(LAUNCHD_SUBDIR)
    }

    /// Returns the owner-private durable host-state directory.
    #[must_use]
    pub fn host_state_dir(&self) -> PathBuf {
        self.state_dir.join(HOST_STATE_SUBDIR)
    }

    /// Returns the owner-private runtime package store directory.
    #[must_use]
    pub fn plugins_dir(&self) -> PathBuf {
        self.state_dir.join(PLUGINS_SUBDIR)
    }

    /// Returns the stable host identity record path.
    #[must_use]
    pub fn host_identity_path(&self) -> PathBuf {
        self.host_state_dir().join(HOST_IDENTITY_NAME)
    }

    /// Returns the host approval secret key path.
    #[must_use]
    pub fn host_approval_key_path(&self) -> PathBuf {
        self.host_state_dir().join(HOST_APPROVAL_KEY_NAME)
    }

    /// Returns the durable host governance record path.
    #[must_use]
    pub fn host_governance_path(&self) -> PathBuf {
        self.host_state_dir().join(HOST_GOVERNANCE_NAME)
    }

    /// Returns the cross-process host-state lock path.
    #[must_use]
    pub fn host_state_lock_path(&self) -> PathBuf {
        self.host_state_dir().join(HOST_STATE_LOCK_NAME)
    }
}

/// Versioned executable layout below one installation prefix.
///
/// Binaries live in `<prefix>/libexec/pohunek/<version>/` so a live worker keeps
/// the exact executable its definition names while a newer version installs
/// beside it. The CLI stays on `PATH` through `<prefix>/bin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallLayout {
    prefix: PathBuf,
}

impl InstallLayout {
    /// Creates the layout rooted at an absolute normalized installation prefix.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::InvalidInstallPrefix`] when `prefix` fails
    /// [`is_normalized_absolute`].
    pub fn new(prefix: impl Into<PathBuf>) -> Result<Self, PathError> {
        let prefix = prefix.into();
        if !is_normalized_absolute(&prefix) {
            return Err(PathError::InvalidInstallPrefix { path: prefix });
        }
        Ok(Self { prefix })
    }

    /// Returns the installation prefix.
    #[must_use]
    pub fn prefix(&self) -> &Path {
        &self.prefix
    }

    /// Returns `<prefix>/bin`, the directory holding the `pohunek` CLI.
    #[must_use]
    pub fn bin_dir(&self) -> PathBuf {
        self.prefix.join(BIN_SUBDIR)
    }

    /// Returns `<prefix>/libexec/pohunek`, the parent of every version directory.
    #[must_use]
    pub fn versions_dir(&self) -> PathBuf {
        self.prefix.join(LIBEXEC_SUBDIR).join(APP_DIR)
    }

    /// Returns one version directory, or `None` for an unsafe version string.
    #[must_use]
    pub fn version_dir(&self, version: &str) -> Option<PathBuf> {
        valid_install_version(version).map(|version| self.versions_dir().join(version))
    }

    /// Returns the versioned daemon executable path.
    #[must_use]
    pub fn daemon_executable(&self, version: &str) -> Option<PathBuf> {
        self.version_dir(version)
            .map(|dir| dir.join(DAEMON_EXECUTABLE_NAME))
    }

    /// Returns the versioned session-worker executable path.
    #[must_use]
    pub fn worker_executable(&self, version: &str) -> Option<PathBuf> {
        self.version_dir(version)
            .map(|dir| dir.join(WORKER_EXECUTABLE_NAME))
    }
}

/// Returns whether `path` is absolute and spelled exactly as its components.
///
/// Rejects `.`, `..`, repeated or trailing separators, and NUL bytes, so the
/// accepted spelling is the only one: a path recorded in `service.toml` or a
/// job definition compares equal to every later spelling of the same path.
#[must_use]
pub fn is_normalized_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path_bytes(path).contains(&0)
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
        && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
}

/// Validates an installed version directory name.
///
/// Versions are one path component of at most [`MAX_INSTALL_VERSION_BYTES`]
/// ASCII alphanumerics, `.`, `+`, or `-`, and never `.` or `..`.
#[must_use]
pub fn valid_install_version(version: &str) -> Option<&str> {
    (!version.is_empty()
        && version.len() <= MAX_INSTALL_VERSION_BYTES
        && !matches!(version, "." | "..")
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-')))
    .then_some(version)
}

/// Resolves the platform application runtime root.
///
/// # Errors
///
/// Returns [`PathError`] when the platform runtime configuration is unavailable
/// or malformed.
pub fn runtime_dir() -> Result<PathBuf, PathError> {
    let platform = Platform::current()?;
    let effective_uid = nix::unistd::Uid::effective().as_raw();
    resolve_runtime_dir(platform, effective_uid, &PathEnv::capture())
}

/// Resolve the local control socket path.
///
/// # Errors
///
/// Returns [`PathError`] when the platform runtime configuration is unavailable,
/// malformed, or produces an overlong daemon socket path.
pub fn socket_path() -> Result<PathBuf, PathError> {
    let platform = Platform::current()?;
    let path = runtime_dir()?.join(SOCKET_NAME);
    validate_socket_path(&path, platform, SocketKind::Daemon)?;
    Ok(path)
}

/// Resolve the XDG data base directory.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when neither `XDG_DATA_HOME` nor `HOME`
/// resolves to a non-empty value.
pub fn data_home() -> Result<PathBuf, PathError> {
    resolve_xdg_or_home(&PathEnv::capture(), XDG_DATA_HOME, HOME_DATA_RELATIVE)
}

/// Resolve the XDG state base directory.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when neither `XDG_STATE_HOME` nor `HOME`
/// resolves to a non-empty value.
pub fn state_home() -> Result<PathBuf, PathError> {
    resolve_xdg_or_home(&PathEnv::capture(), XDG_STATE_HOME, HOME_STATE_RELATIVE)
}

/// Resolve the XDG cache base directory.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when neither `XDG_CACHE_HOME` nor `HOME`
/// resolves to a non-empty value.
pub fn cache_home() -> Result<PathBuf, PathError> {
    resolve_xdg_or_home(&PathEnv::capture(), XDG_CACHE_HOME, HOME_CACHE_RELATIVE)
}

/// Resolve the XDG config base directory.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when neither `XDG_CONFIG_HOME` nor `HOME`
/// resolves to a non-empty value.
pub fn config_home() -> Result<PathBuf, PathError> {
    resolve_xdg_or_home(&PathEnv::capture(), XDG_CONFIG_HOME, HOME_CONFIG_RELATIVE)
}

/// Read a required environment variable or fail fast.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when `key` is missing or empty.
pub fn require_env(key: &str) -> Result<String, PathError> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(PathError::MissingEnv {
            var: key.to_owned(),
        }),
    }
}

/// Resolve an XDG base dir: use `$key` if set and non-empty, otherwise
/// `$HOME` joined with `home_relative`.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when neither source resolves.
pub fn xdg_or_home_relative(key: &str, home_relative: &[&str]) -> Result<PathBuf, PathError> {
    if let Some(value) = std::env::var_os(key) {
        return validate_env_path(key, &value);
    }
    resolve_xdg_or_home(&PathEnv::capture(), key, home_relative)
}

/// Validates one Unix socket path against a platform's encoded ABI limit.
///
/// # Errors
///
/// Returns [`PathError`] when the path is nonabsolute, contains a parent
/// component or NUL, or leaves no room for the native terminator.
pub fn validate_socket_path(
    path: impl AsRef<Path>,
    platform: Platform,
    kind: SocketKind,
) -> Result<(), PathError> {
    let path = path.as_ref();
    if !path.is_absolute() {
        return Err(PathError::SocketPathNotAbsolute {
            kind,
            path: path.to_path_buf(),
        });
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(PathError::SocketPathParentComponent {
            kind,
            path: path.to_path_buf(),
        });
    }
    let bytes = path_bytes(path);
    if bytes.contains(&0) {
        return Err(PathError::SocketPathContainsNul {
            kind,
            path: path.to_path_buf(),
        });
    }
    let max_bytes = platform.socket_path_max_bytes();
    if bytes.len() > max_bytes {
        return Err(PathError::SocketPathTooLong {
            kind,
            platform,
            path: path.to_path_buf(),
            actual_bytes: bytes.len(),
            max_bytes,
        });
    }
    Ok(())
}

/// Returns the longest pathname a staged socket bind in `dir` can use.
///
/// Staged names are [`STAGED_SOCKET_PREFIX`] followed by
/// [`STAGED_SOCKET_SUFFIX_CHARS`] hexadecimal characters.
#[must_use]
pub fn longest_staged_socket_path(dir: &Path) -> PathBuf {
    let mut name = String::with_capacity(STAGED_SOCKET_PREFIX.len() + STAGED_SOCKET_SUFFIX_CHARS);
    name.push_str(STAGED_SOCKET_PREFIX);
    name.extend(std::iter::repeat_n('0', STAGED_SOCKET_SUFFIX_CHARS));
    dir.join(name)
}

/// Validates a socket's published path and the longest staged bind path in
/// its directory.
///
/// A listener binds a staged name before publishing the socket, so a
/// published path that fits can still be unbindable when the staged name is
/// longer than the published file name.
///
/// # Errors
///
/// Returns [`PathError`] for the published path as
/// [`validate_socket_path`] does, then [`PathError::SocketPathTooLong`]
/// naming the staged path when only that one exceeds the limit.
pub fn validate_staged_socket_path(
    path: impl AsRef<Path>,
    platform: Platform,
    kind: SocketKind,
) -> Result<(), PathError> {
    let path = path.as_ref();
    validate_socket_path(path, platform, kind)?;
    // An absolute path without a parent is the root itself; its staged
    // sibling is then measured below the root.
    let dir = path.parent().unwrap_or(path);
    validate_socket_path(longest_staged_socket_path(dir), platform, kind)
}

/// Resolves one managed session's worker control socket below
/// `runtime_root`, the directory [`BasePaths::worker_runtime_root`] names.
///
/// The worker binds its socket through a staged name, so this validates the
/// published path and the longest staged bind path exactly as the worker
/// does before binding ([`validate_staged_socket_path`]). The daemon uses it
/// to refuse a session whose worker could never bind.
///
/// Returns `Ok(None)` for a session ID that cannot name a worker.
///
/// # Errors
///
/// Returns [`PathError`] when either path exceeds the platform limit or is
/// otherwise not a valid socket path.
pub fn worker_socket_path(
    runtime_root: &Path,
    session_id: &str,
    platform: Platform,
) -> Result<Option<PathBuf>, PathError> {
    let Some(session_id) = valid_worker_session_id(session_id) else {
        return Ok(None);
    };
    let socket = runtime_root.join(session_id).join(WORKER_SOCKET_NAME);
    validate_staged_socket_path(&socket, platform, SocketKind::Worker)?;
    Ok(Some(socket))
}

fn resolve_runtime_dir(
    platform: Platform,
    effective_uid: u32,
    env: &PathEnv,
) -> Result<PathBuf, PathError> {
    match env.xdg_runtime_dir.as_deref() {
        Some(value) => validate_env_path(XDG_RUNTIME_DIR, value).map(|base| base.join(APP_DIR)),
        None if platform == Platform::MacOs => Ok(PathBuf::from(format!(
            "/private/tmp/{APP_DIR}-{effective_uid}"
        ))),
        None => Err(PathError::MissingEnv {
            var: XDG_RUNTIME_DIR.to_owned(),
        }),
    }
}

fn resolve_xdg_or_home(
    env: &PathEnv,
    key: &str,
    home_relative: &[&str],
) -> Result<PathBuf, PathError> {
    if let Some(value) = env.value(key) {
        return validate_env_path(key, value);
    }
    let Some(home) = env.home.as_deref() else {
        return Err(PathError::MissingEnv {
            var: format!("{key} or {HOME}"),
        });
    };
    let mut path = validate_env_path(HOME, home)?;
    path.extend(home_relative);
    Ok(path)
}

fn validate_env_path(key: &str, value: &OsStr) -> Result<PathBuf, PathError> {
    if value.is_empty() {
        return Err(invalid_env(key, InvalidPathReason::Empty));
    }
    let path = PathBuf::from(value);
    if path_bytes(&path).contains(&0) {
        return Err(invalid_env(key, InvalidPathReason::ContainsNul));
    }
    if !path.is_absolute() {
        return Err(invalid_env(key, InvalidPathReason::NotAbsolute));
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(invalid_env(key, InvalidPathReason::ParentComponent));
    }
    Ok(path)
}

fn invalid_env(key: &str, reason: InvalidPathReason) -> PathError {
    PathError::InvalidEnv {
        var: key.to_owned(),
        reason,
    }
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt as _;

    path.as_os_str().as_bytes()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

/// Return a path view of an identifier that is exactly one normal path component.
#[must_use]
pub fn valid_path_id(id: &str) -> Option<&Path> {
    let path = Path::new(id);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Some(path),
        _ => None,
    }
}

/// Prefix of every managed worker session ID.
pub const WORKER_SESSION_ID_PREFIX: &str = "s-";

/// Most digits a numeric worker session ID may carry.
///
/// Numeric IDs were issued from a `u64` counter, whose largest value has 20
/// decimal digits. The bound keeps every valid session ID short enough for the
/// service identifiers, launchd labels, and systemd unit names derived from it.
const MAX_NUMERIC_SESSION_DIGITS: usize = 20;

/// Characters of a Crockford base32 ULID.
const ULID_LEN: usize = 26;

/// Longest valid managed worker session ID in bytes.
///
/// Supervisor backends build names from a session ID plus a
/// [`WORKER_GENERATION_LEN`] token and rely on this bound to keep them within
/// their own length limits.
pub const MAX_WORKER_SESSION_ID_BYTES: usize = WORKER_SESSION_ID_PREFIX.len()
    + if MAX_NUMERIC_SESSION_DIGITS > ULID_LEN {
        MAX_NUMERIC_SESSION_DIGITS
    } else {
        ULID_LEN
    };

/// Validates a managed worker session ID.
///
/// Worker units are instantiated only for daemon-issued numeric or ULID session
/// IDs. Restricting the grammar keeps paths and systemd instance names
/// interchangeable without escaping, and every accepted ID is at most
/// [`MAX_WORKER_SESSION_ID_BYTES`] long.
#[must_use]
pub fn valid_worker_session_id(id: &str) -> Option<&str> {
    let suffix = id.strip_prefix(WORKER_SESSION_ID_PREFIX)?;
    let numeric = (1..=MAX_NUMERIC_SESSION_DIGITS).contains(&suffix.len())
        && suffix.bytes().all(|byte| byte.is_ascii_digit());
    let ulid = suffix.len() == ULID_LEN && suffix.bytes().all(|byte| {
        byte.is_ascii_digit()
            || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
    });
    (numeric || ulid).then_some(id)
}

/// Returns the longest valid managed worker session ID.
///
/// All-zero digits are valid in both the numeric and the ULID form, so the ID
/// has exactly [`MAX_WORKER_SESSION_ID_BYTES`] bytes. Path-length checks use it
/// to size the longest worker socket path.
#[must_use]
pub fn longest_worker_session_id() -> String {
    let digits = MAX_WORKER_SESSION_ID_BYTES - WORKER_SESSION_ID_PREFIX.len();
    format!("{WORKER_SESSION_ID_PREFIX}{}", "0".repeat(digits))
}

/// Validates an opaque worker ID used as a filename.
///
/// IDs use a conservative ASCII grammar and are bounded so they remain one
/// safe path component on every supported filesystem.
#[must_use]
pub fn valid_worker_id(id: &str) -> Option<&str> {
    const MAX_WORKER_ID_BYTES: usize = 96;

    (!id.is_empty()
        && id.len() <= MAX_WORKER_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then_some(id)
}

/// Length of a daemon-issued worker generation token.
///
/// A generation names one worker process of a session in service-manager
/// labels and unit names. Eight base32 characters carry 40 random bits, enough
/// that two generations of one session never collide in practice, while the
/// full launchd label stays short.
pub const WORKER_GENERATION_LEN: usize = 8;

/// Random bytes encoded into one worker generation token.
///
/// Five bytes are exactly the 40 bits that [`WORKER_GENERATION_LEN`] base32
/// characters represent, so every token is fully random.
pub const WORKER_GENERATION_ENTROPY_BYTES: usize = 5;

/// RFC 4648 base32 alphabet in lowercase, as used by generation tokens.
///
/// Lowercase letters and digits `2`-`7` are valid unescaped in launchd labels,
/// systemd unit names, and file names on every supported filesystem.
const GENERATION_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Bits encoded by one base32 character.
const BASE32_BITS: u32 = 5;

/// Validates a daemon-issued worker generation token.
///
/// A token is exactly [`WORKER_GENERATION_LEN`] characters of the lowercase
/// RFC 4648 base32 alphabet `[a-z2-7]`.
#[must_use]
pub fn valid_worker_generation(value: &str) -> Option<&str> {
    (value.len() == WORKER_GENERATION_LEN
        && value
            .bytes()
            .all(|byte| GENERATION_ALPHABET.contains(&byte)))
    .then_some(value)
}

/// Encodes random bytes as a worker generation token.
///
/// The caller supplies operating-system entropy; the encoding itself is pure
/// so that its output grammar is testable. The result always satisfies
/// [`valid_worker_generation`].
#[must_use]
pub fn encode_worker_generation(entropy: [u8; WORKER_GENERATION_ENTROPY_BYTES]) -> String {
    let bits = entropy
        .iter()
        .fold(0_u64, |bits, byte| (bits << 8) | u64::from(*byte));
    (0..WORKER_GENERATION_LEN)
        .rev()
        .map(|index| {
            let shift = u32::try_from(index).expect("generation length fits u32") * BASE32_BITS;
            let symbol = usize::try_from((bits >> shift) & 0x1f).expect("5-bit value fits usize");
            char::from(GENERATION_ALPHABET[symbol])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn longest_worker_session_id_is_valid_and_maximal() {
        let id = super::longest_worker_session_id();

        assert_eq!(id.len(), MAX_WORKER_SESSION_ID_BYTES);
        assert!(id.starts_with(WORKER_SESSION_ID_PREFIX));
        assert!(valid_worker_session_id(&id).is_some());
    }

    use super::*;

    use pohunek_test_support::process_env::ProcessEnv;

    // Keep synthetic runtime paths below the strictest supported Unix-socket limit.
    const TEST_BASE_ROOT: &str = "/work";
    const CUSTOM_DATA_HOME: &str = "CUSTOM_DATA_HOME";

    fn tmp_base(tag: &str) -> PathBuf {
        Path::new(TEST_BASE_ROOT).join(format!("p-{}-{tag}", std::process::id()))
    }

    /// A fully populated path environment below `base`.
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

    /// Resolves `env` for the host platform without reading the process environment.
    fn resolve_in(env: &PathEnv) -> Result<BasePaths, PathError> {
        BasePaths::resolve_for(
            Platform::current().expect("supported platform"),
            nix::unistd::Uid::effective().as_raw(),
            env,
        )
    }

    #[test]
    fn resolves_full_base_path_set() {
        let base = tmp_base("full");

        let paths = resolve_in(&all_present(&base)).expect("resolve paths");

        assert_eq!(paths.runtime_dir, base.join("run").join(APP_DIR));
        assert_eq!(
            paths.socket,
            base.join("run").join(APP_DIR).join(SOCKET_NAME)
        );
        assert_eq!(paths.lock, base.join("run").join(APP_DIR).join(LOCK_NAME));
        assert_eq!(
            paths.log_dir,
            base.join("state").join(APP_DIR).join(LOGS_SUBDIR)
        );
        assert_eq!(paths.state_dir, base.join("state").join(APP_DIR));
        assert_eq!(paths.data_dir, base.join("data").join(APP_DIR));
        assert_eq!(paths.cache_dir, base.join("cache").join(APP_DIR));
        assert_eq!(paths.config_home, base.join("cfg"));
        assert_eq!(paths.config_dir, base.join("cfg").join(APP_DIR));
    }

    #[test]
    fn resolve_reads_the_process_environment() {
        let base = tmp_base("process-env");
        let mut env = ProcessEnv::lock();
        env.set(XDG_RUNTIME_DIR, base.join("run"))
            .set(XDG_STATE_HOME, base.join("state"))
            .set(XDG_DATA_HOME, base.join("data"))
            .set(XDG_CONFIG_HOME, base.join("cfg"))
            .set(XDG_CACHE_HOME, base.join("cache"))
            .set(HOME, base.join("home"));

        let captured = PathEnv::capture();
        let paths = BasePaths::resolve().expect("resolve paths");

        assert_eq!(captured, all_present(&base));
        assert_eq!(paths, resolve_in(&captured).expect("resolve captured"));
        assert_eq!(config_home().expect("config home"), base.join("cfg"));
    }

    #[test]
    fn falls_back_to_home_for_cache_home() {
        let base = tmp_base("cache-home");
        let env = PathEnv {
            xdg_cache_home: None,
            ..all_present(&base)
        };

        let paths = resolve_in(&env).expect("resolve paths");

        assert_eq!(
            paths.cache_dir,
            base.join("home").join(".cache").join(APP_DIR)
        );
    }

    #[test]
    fn require_env_rejects_missing_and_empty_values() {
        let mut env = ProcessEnv::lock();
        env.remove(XDG_RUNTIME_DIR);
        assert!(matches!(
            require_env(XDG_RUNTIME_DIR),
            Err(PathError::MissingEnv { var }) if var == XDG_RUNTIME_DIR
        ));
        env.set(XDG_RUNTIME_DIR, "");
        assert!(matches!(
            require_env(XDG_RUNTIME_DIR),
            Err(PathError::MissingEnv { var }) if var == XDG_RUNTIME_DIR
        ));
    }

    #[test]
    fn xdg_or_home_relative_reports_actionable_missing_pair() {
        let env = PathEnv::default();

        let err = resolve_xdg_or_home(&env, XDG_CONFIG_HOME, HOME_CONFIG_RELATIVE)
            .expect_err("missing config env fails");

        assert!(matches!(
            err,
            PathError::MissingEnv { var } if var == "XDG_CONFIG_HOME or HOME"
        ));
    }

    #[test]
    fn xdg_or_home_relative_honors_an_arbitrary_environment_key() {
        let mut env = ProcessEnv::lock();
        let custom = tmp_base("custom-data-home");
        env.set(CUSTOM_DATA_HOME, &custom);

        assert_eq!(
            xdg_or_home_relative(CUSTOM_DATA_HOME, &["fallback"])
                .expect("resolve custom environment key"),
            custom
        );
    }

    #[test]
    fn assistant_runtime_dir_rejects_unsafe_ids() {
        let base = tmp_base("assistant-runtime");
        let paths = resolve_in(&all_present(&base)).expect("resolve paths");

        assert_eq!(
            paths.assistant_runtime_dir("launch-1"),
            Some(
                base.join("run")
                    .join(APP_DIR)
                    .join(ASSISTANT_RUNTIME_SUBDIR)
                    .join("launch-1")
            )
        );
        for id in ["", ".", "..", "nested/id", "/absolute"] {
            assert_eq!(paths.assistant_runtime_dir(id), None, "id: {id}");
        }
    }

    #[test]
    fn worker_paths_accept_only_managed_safe_ids() {
        let base = tmp_base("worker-paths");
        let paths = resolve_in(&all_present(&base)).expect("resolve paths");

        assert_eq!(
            paths.worker_socket("s-42").expect("socket path length"),
            Some(
                base.join("run")
                    .join(APP_DIR)
                    .join(WORKERS_SUBDIR)
                    .join("s-42")
                    .join(WORKER_SOCKET_NAME)
            )
        );
        assert_eq!(
            paths.worker_journal("s-42", "w-runtime_1"),
            Some(
                base.join("state")
                    .join(APP_DIR)
                    .join(WORKERS_SUBDIR)
                    .join("s-42")
                    .join("w-runtime_1.json")
            )
        );
        assert!(paths
            .worker_socket("s-01KYAPVPFVHD56Z69B9CX3XWN2")
            .expect("socket path length")
            .is_some());

        for invalid in [
            "",
            "42",
            "s-",
            "s-a",
            "s-01kyapvpfvhd56z69b9cx3xwn2",
            "s-01KYAPVPFVHD56Z69B9CX3XWNI",
            "../s-1",
            "s-1/other",
        ] {
            assert_eq!(
                paths
                    .worker_socket(invalid)
                    .expect("invalid ID has no path"),
                None
            );
        }
        for invalid in ["", "../worker", "worker/name", "worker.name"] {
            assert_eq!(paths.worker_journal("s-42", invalid), None);
        }
    }

    #[test]
    fn host_state_paths_have_the_canonical_layout() {
        let base = tmp_base("host-state");
        let paths = resolve_in(&all_present(&base)).expect("resolve paths");
        let host = base.join("state").join(APP_DIR).join(HOST_STATE_SUBDIR);

        assert_eq!(paths.host_state_dir(), host);
        assert_eq!(paths.host_identity_path(), host.join(HOST_IDENTITY_NAME));
        assert_eq!(
            paths.host_approval_key_path(),
            host.join(HOST_APPROVAL_KEY_NAME)
        );
        assert_eq!(
            paths.host_governance_path(),
            host.join(HOST_GOVERNANCE_NAME)
        );
        assert_eq!(
            paths.host_state_lock_path(),
            host.join(HOST_STATE_LOCK_NAME)
        );
    }

    #[test]
    fn plugins_dir_is_a_state_subdirectory() {
        let base = tmp_base("plugins");
        let paths = resolve_in(&all_present(&base)).expect("resolve paths");

        assert_eq!(
            paths.plugins_dir(),
            base.join("state").join(APP_DIR).join(PLUGINS_SUBDIR)
        );
        assert_ne!(paths.plugins_dir(), paths.host_state_dir());
    }

    #[test]
    fn worker_session_ids_are_bounded() {
        assert_eq!(u64::MAX.to_string().len(), MAX_NUMERIC_SESSION_DIGITS);
        let longest_numeric = format!("s-{}", u64::MAX);
        assert_eq!(
            valid_worker_session_id(&longest_numeric),
            Some(longest_numeric.as_str())
        );
        let ulid = "s-01KYAPVPFVHD56Z69B9CX3XWN2";
        assert_eq!(valid_worker_session_id(ulid), Some(ulid));
        assert_eq!(ulid.len(), MAX_WORKER_SESSION_ID_BYTES);

        let over_long = format!("s-{}", "9".repeat(MAX_NUMERIC_SESSION_DIGITS + 1));
        assert_eq!(valid_worker_session_id(&over_long), None);
        assert_eq!(
            valid_worker_session_id(&format!("s-{}", "1".repeat(150))),
            None
        );
    }

    #[test]
    fn worker_generation_accepts_only_eight_lowercase_base32_characters() {
        for valid in ["abcd2345", "aaaaaaaa", "77777777", "zzzzzzzz"] {
            assert_eq!(valid_worker_generation(valid), Some(valid));
        }
        for invalid in [
            "",
            "abcd234",
            "abcd23456",
            "ABCD2345",
            "abcd2301",
            "abcd-345",
            "abcd 345",
            "abcd23\u{e9}",
        ] {
            assert_eq!(valid_worker_generation(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn install_layout_versions_executables_below_libexec() {
        let layout = InstallLayout::new("/home/u/.local").expect("absolute prefix");
        assert_eq!(layout.bin_dir(), Path::new("/home/u/.local/bin"));
        assert_eq!(
            layout.versions_dir(),
            Path::new("/home/u/.local/libexec/pohunek")
        );
        assert_eq!(
            layout.daemon_executable("0.31.6"),
            Some(PathBuf::from(
                "/home/u/.local/libexec/pohunek/0.31.6/pohunekd"
            ))
        );
        assert_eq!(
            layout.worker_executable("0.32.0-rc.1+g1"),
            Some(PathBuf::from(
                "/home/u/.local/libexec/pohunek/0.32.0-rc.1+g1/pohunek-sessiond"
            ))
        );
        for unsafe_version in ["", ".", "..", "../x", "a/b", "1 2", &"1".repeat(65)] {
            assert_eq!(layout.version_dir(unsafe_version), None, "{unsafe_version}");
        }
        for prefix in [
            "relative",
            "/home/u/../x",
            "",
            "/home/u/.local/.",
            "/home/u/./.local",
            "/home//u/.local",
            "/home/u/.local/",
            "/home/u/\0/.local",
        ] {
            assert!(
                matches!(
                    InstallLayout::new(prefix),
                    Err(PathError::InvalidInstallPrefix { .. })
                ),
                "{prefix:?}"
            );
        }
    }

    #[test]
    fn normalized_absolute_paths_have_exactly_one_spelling() {
        for accepted in ["/", "/home/u/.local", "/a/b.c/..d"] {
            assert!(is_normalized_absolute(Path::new(accepted)), "{accepted:?}");
        }
        for rejected in [
            "",
            "relative/x",
            "/a/..",
            "/a/.",
            "/./a",
            "//a",
            "/a//b",
            "/a/",
            "/a\0b",
        ] {
            assert!(!is_normalized_absolute(Path::new(rejected)), "{rejected:?}");
        }
    }

    #[test]
    fn worker_generation_encoding_is_rfc4648_base32() {
        // RFC 4648 section 10 test vector: BASE32("fooba") = "MZXW6YTB".
        assert_eq!(encode_worker_generation(*b"fooba"), "mzxw6ytb");
        assert_eq!(encode_worker_generation([0; 5]), "aaaaaaaa");
        assert_eq!(encode_worker_generation([0xff; 5]), "77777777");
        for entropy in [[0x12, 0x34, 0x56, 0x78, 0x9a], [1, 2, 3, 4, 5]] {
            let token = encode_worker_generation(entropy);
            assert_eq!(valid_worker_generation(&token), Some(token.as_str()));
        }
    }
}

/// Name of the pre-migration backup of `store` taken at schema `schema`.
///
/// The daemon writes it next to the store; `is_schema_backup_artifact` is its
/// exact inverse, which keeps cleanup in step with the writer.
#[must_use]
pub fn schema_backup_name(store: &OsStr, schema: u32) -> OsString {
    let mut name = store.to_os_string();
    name.push(format!("{SCHEMA_BACKUP_INFIX}{schema}"));
    name
}

/// Name a pre-migration backup of `store` carries while it is being written.
///
/// `unique` makes concurrent writers' names distinct (process id and a counter).
#[must_use]
pub fn schema_backup_temp_name(store: &OsStr, process: u32, sequence: u64) -> OsString {
    let mut name = store.to_os_string();
    name.push(format!(
        "{SCHEMA_BACKUP_INFIX}{SCHEMA_BACKUP_TEMP_MARKER}.{process}.{sequence}"
    ));
    name
}

/// Whether `name` is exactly a backup, finished or in progress, of `store`.
///
/// Matches `<store>.pre-schema-<canonical decimal>` and
/// `<store>.pre-schema-tmp.<decimal>.<decimal>`; nothing else, so cleanup never
/// touches an unrelated file that merely starts with the store name.
#[must_use]
pub fn is_schema_backup_artifact(store: &str, name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(store)
        .and_then(|tail| tail.strip_prefix(SCHEMA_BACKUP_INFIX))
    else {
        return false;
    };
    let canonical_decimal = |part: &str| {
        part.parse::<u64>()
            .is_ok_and(|number| number.to_string() == part)
    };
    if canonical_decimal(rest) {
        return true;
    }
    let mut parts = rest.split('.');
    parts.next() == Some(SCHEMA_BACKUP_TEMP_MARKER)
        && parts.next().is_some_and(canonical_decimal)
        && parts.next().is_some_and(canonical_decimal)
        && parts.next().is_none()
}

#[cfg(test)]
mod schema_backup_tests {
    use std::ffi::OsStr;

    use super::{
        is_schema_backup_artifact, schema_backup_name, schema_backup_temp_name, METADATA_STORE_NAME,
    };

    #[test]
    fn the_writer_names_match_the_cleanup_grammar() {
        let store = OsStr::new(METADATA_STORE_NAME);
        for name in [
            schema_backup_name(store, 1),
            schema_backup_name(store, 42),
            schema_backup_temp_name(store, 4242, 0),
        ] {
            let name = name.to_str().expect("utf-8 name");
            assert!(
                is_schema_backup_artifact(METADATA_STORE_NAME, name),
                "{name}"
            );
        }
    }

    #[test]
    fn unrelated_names_are_not_backup_artifacts() {
        for name in [
            "metadata.jsonl",
            "metadata.jsonl.tmp.1.2",
            "metadata.jsonl.pre-schema-",
            "metadata.jsonl.pre-schema-01",
            "metadata.jsonl.pre-schema--1",
            "metadata.jsonl.pre-schema-1.bak",
            "metadata.jsonl.pre-schema-tmp",
            "metadata.jsonl.pre-schema-tmp.1",
            "metadata.jsonl.pre-schema-tmp.1.2.3",
            "metadata.jsonl.pre-schema-tmp.x.2",
            "other.jsonl.pre-schema-1",
            "xmetadata.jsonl.pre-schema-1",
        ] {
            assert!(
                !is_schema_backup_artifact(METADATA_STORE_NAME, name),
                "{name}"
            );
        }
    }
}
