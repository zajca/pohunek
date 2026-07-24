//! Shared XDG path contract for pohunek.
//!
//! Runtime, state, data, cache, and config paths are part of the local
//! daemon/client contract. This crate keeps the layout and fail-fast environment
//! rules in one place while callers map [`PathError`] into their own public error
//! types.

#![forbid(unsafe_code)]

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

/// Path-resolution error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// A required environment variable is missing or empty.
    #[error("required environment variable {var} is not set (no safe default exists)")]
    MissingEnv {
        /// Missing variable name, or an actionable `XDG_* or HOME` pair.
        var: String,
    },
}

/// Shared XDG-derived path set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasePaths {
    /// `$XDG_RUNTIME_DIR/pohunek`.
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
}

impl BasePaths {
    /// Resolve all shared pohunek paths from process environment.
    ///
    /// # Errors
    ///
    /// Returns [`PathError::MissingEnv`] when `XDG_RUNTIME_DIR` is missing, or
    /// when neither a relevant XDG variable nor `HOME` is available for a base
    /// directory with a documented home fallback.
    pub fn resolve() -> Result<Self, PathError> {
        let runtime_dir = runtime_dir()?;
        let socket = runtime_dir.join(SOCKET_NAME);
        let lock = runtime_dir.join(LOCK_NAME);
        let data_dir = data_home()?.join(APP_DIR);
        let state_dir = state_home()?.join(APP_DIR);
        let log_dir = state_dir.join(LOGS_SUBDIR);
        let cache_dir = cache_home()?.join(APP_DIR);
        let config_home = config_home()?;
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
    #[must_use]
    pub fn worker_socket(&self, session_id: &str) -> Option<PathBuf> {
        self.worker_runtime_dir(session_id)
            .map(|dir| dir.join(WORKER_SOCKET_NAME))
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
}

/// Resolve `$XDG_RUNTIME_DIR/pohunek`.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when `XDG_RUNTIME_DIR` is missing or empty.
pub fn runtime_dir() -> Result<PathBuf, PathError> {
    Ok(PathBuf::from(require_env(XDG_RUNTIME_DIR)?).join(APP_DIR))
}

/// Resolve the local control socket path.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when `XDG_RUNTIME_DIR` is missing or empty.
pub fn socket_path() -> Result<PathBuf, PathError> {
    Ok(runtime_dir()?.join(SOCKET_NAME))
}

/// Resolve the XDG data base directory.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when neither `XDG_DATA_HOME` nor `HOME`
/// resolves to a non-empty value.
pub fn data_home() -> Result<PathBuf, PathError> {
    xdg_or_home_relative(XDG_DATA_HOME, HOME_DATA_RELATIVE)
}

/// Resolve the XDG state base directory.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when neither `XDG_STATE_HOME` nor `HOME`
/// resolves to a non-empty value.
pub fn state_home() -> Result<PathBuf, PathError> {
    xdg_or_home_relative(XDG_STATE_HOME, HOME_STATE_RELATIVE)
}

/// Resolve the XDG cache base directory.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when neither `XDG_CACHE_HOME` nor `HOME`
/// resolves to a non-empty value.
pub fn cache_home() -> Result<PathBuf, PathError> {
    xdg_or_home_relative(XDG_CACHE_HOME, HOME_CACHE_RELATIVE)
}

/// Resolve the XDG config base directory.
///
/// # Errors
///
/// Returns [`PathError::MissingEnv`] when neither `XDG_CONFIG_HOME` nor `HOME`
/// resolves to a non-empty value.
pub fn config_home() -> Result<PathBuf, PathError> {
    xdg_or_home_relative(XDG_CONFIG_HOME, HOME_CONFIG_RELATIVE)
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
    if let Ok(value) = std::env::var(key) {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    let home = match require_env(HOME) {
        Ok(home) => home,
        Err(PathError::MissingEnv { .. }) => {
            return Err(PathError::MissingEnv {
                var: format!("{key} or {HOME}"),
            });
        }
    };
    let mut path = PathBuf::from(home);
    for segment in home_relative {
        path.push(segment);
    }
    Ok(path)
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
/// Worker units are instantiated only for the daemon-issued `s-<number>` ID
/// space. Restricting the grammar keeps paths and systemd instance names
/// interchangeable without escaping.
#[must_use]
pub fn valid_worker_session_id(id: &str) -> Option<&str> {
    let suffix = id.strip_prefix("s-")?;
    (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())).then_some(id)
}

/// Validates an opaque worker ID used as a filename.
///
/// IDs use a conservative ASCII grammar and are bounded so they remain one
/// safe path component on every supported Linux filesystem.
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
            paths.worker_socket("s-42"),
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

        for invalid in ["", "42", "s-", "s-a", "../s-1", "s-1/other"] {
            assert_eq!(paths.worker_socket(invalid), None);
        }
        for invalid in ["", "../worker", "worker/name", "worker.name"] {
            assert_eq!(paths.worker_journal("s-42", invalid), None);
        }
    }
}
