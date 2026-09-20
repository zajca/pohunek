//! Shared XDG path contract for pohunek.
//!
//! Runtime, state, data, cache, and config paths are part of the local
//! daemon/client contract. This crate keeps the layout and fail-fast environment
//! rules in one place while callers map [`PathError`] into their own public error
//! types.

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-09-19

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
/// Launcher script subdirectory under the app data directory.
pub const BIN_SUBDIR: &str = "bin";
/// Sway config directory name under `XDG_CONFIG_HOME`.
pub const SWAY_CONFIG_DIR: &str = "sway";
/// Assistant knowledge cache subdirectory under the app cache directory.
pub const KNOWLEDGE_CACHE_SUBDIR: &str = "knowledge";
/// Assistant runtime subdirectory under the app runtime directory.
pub const ASSISTANT_RUNTIME_SUBDIR: &str = "assistant";
/// Per-session worker subdirectory under runtime and state directories.
pub const WORKERS_SUBDIR: &str = "workers";
/// Worker control socket filename.
pub const WORKER_SOCKET_NAME: &str = "control.sock";
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

    /// Directory containing materialized launcher scripts.
    #[must_use]
    pub fn launcher_bin_dir(&self) -> PathBuf {
        self.data_dir.join(BIN_SUBDIR)
    }

    /// User sway config directory.
    #[must_use]
    pub fn sway_config_dir(&self) -> PathBuf {
        self.config_home.join(SWAY_CONFIG_DIR)
    }

    /// Directory where assistant knowledge bundles are cached.
    #[must_use]
    pub fn assistant_bundle_cache_dir(&self) -> PathBuf {
        self.cache_dir.join(KNOWLEDGE_CACHE_SUBDIR)
    }

    /// Runtime directory for assistant material generated for one launch/session.
    #[must_use]
    pub fn assistant_runtime_dir(&self, launch_or_session_id: &str) -> Option<PathBuf> {
        valid_runtime_id(launch_or_session_id)
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
    pub fn worker_socket(&self, session_id: &str) -> Result<Option<PathBuf>, PathError> {
        let Some(socket) = self
            .worker_runtime_dir(session_id)
            .map(|dir| dir.join(WORKER_SOCKET_NAME))
        else {
            return Ok(None);
        };
        validate_socket_path(&socket, self.platform, SocketKind::Worker)?;
        Ok(Some(socket))
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

    /// Returns the owner-private durable host-state directory.
    #[must_use]
    pub fn host_state_dir(&self) -> PathBuf {
        self.state_dir.join(HOST_STATE_SUBDIR)
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

/// Return a path view of a safe one-component runtime id.
#[must_use]
pub fn valid_runtime_id(id: &str) -> Option<&Path> {
    let path = Path::new(id);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Some(path),
        _ => None,
    }
}

/// Validates a managed worker session ID.
///
/// Worker units are instantiated only for daemon-issued numeric or ULID session
/// IDs. Restricting the grammar keeps paths and systemd instance names
/// interchangeable without escaping.
#[must_use]
pub fn valid_worker_session_id(id: &str) -> Option<&str> {
    let suffix = id.strip_prefix("s-")?;
    let numeric = !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit());
    let ulid = suffix.len() == 26 && suffix.bytes().all(|byte| {
        byte.is_ascii_digit()
            || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
    });
    (numeric || ulid).then_some(id)
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

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const VARS: [&str; 6] = [
        XDG_RUNTIME_DIR,
        XDG_STATE_HOME,
        XDG_DATA_HOME,
        XDG_CONFIG_HOME,
        XDG_CACHE_HOME,
        HOME,
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
            let saved = VARS
                .iter()
                .map(|&key| (key, std::env::var(key).ok()))
                .collect();
            Self { _lock: lock, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn tmp_base(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pohunek-paths-{tag}-{}", std::process::id()))
    }

    fn set_all_present(base: &Path) {
        std::env::set_var(XDG_RUNTIME_DIR, base.join("run"));
        std::env::set_var(XDG_STATE_HOME, base.join("state"));
        std::env::set_var(XDG_DATA_HOME, base.join("data"));
        std::env::set_var(XDG_CONFIG_HOME, base.join("cfg"));
        std::env::set_var(XDG_CACHE_HOME, base.join("cache"));
        std::env::set_var(HOME, base.join("home"));
    }

    #[test]
    fn resolves_full_base_path_set() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("full");
        set_all_present(&base);

        let paths = BasePaths::resolve().expect("resolve paths");

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
    fn falls_back_to_home_for_cache_home() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("cache-home");
        set_all_present(&base);
        std::env::remove_var(XDG_CACHE_HOME);

        let paths = BasePaths::resolve().expect("resolve paths");

        assert_eq!(
            paths.cache_dir,
            base.join("home").join(".cache").join(APP_DIR)
        );
    }

    #[test]
    fn require_env_rejects_missing_and_empty_values() {
        let _env = EnvGuard::acquire();
        std::env::remove_var(XDG_RUNTIME_DIR);
        assert!(matches!(
            require_env(XDG_RUNTIME_DIR),
            Err(PathError::MissingEnv { var }) if var == XDG_RUNTIME_DIR
        ));
        std::env::set_var(XDG_RUNTIME_DIR, "");
        assert!(matches!(
            require_env(XDG_RUNTIME_DIR),
            Err(PathError::MissingEnv { var }) if var == XDG_RUNTIME_DIR
        ));
    }

    #[test]
    fn xdg_or_home_relative_reports_actionable_missing_pair() {
        let _env = EnvGuard::acquire();
        std::env::remove_var(XDG_CONFIG_HOME);
        std::env::remove_var(HOME);

        let err = config_home().expect_err("missing config env fails");

        assert!(matches!(
            err,
            PathError::MissingEnv { var } if var == "XDG_CONFIG_HOME or HOME"
        ));
    }

    #[test]
    fn assistant_runtime_dir_rejects_unsafe_ids() {
        let _env = EnvGuard::acquire();
        let base = tmp_base("assistant-runtime");
        set_all_present(&base);
        let paths = BasePaths::resolve().expect("resolve paths");

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
        let _env = EnvGuard::acquire();
        let base = tmp_base("worker-paths");
        set_all_present(&base);
        let paths = BasePaths::resolve().expect("resolve paths");

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
        let _env = EnvGuard::acquire();
        let base = tmp_base("host-state");
        set_all_present(&base);
        let paths = BasePaths::resolve().expect("resolve paths");
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
}
