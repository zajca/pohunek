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

/// Cross-process lock filename protecting the daemon-owned data directory.
const DATA_AUTHORITY_LOCK_NAME: &str = "data.lock";

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
        Self::resolve_from(&pohunek_paths::PathEnv::capture())
    }

    /// Resolves all daemon paths from explicit environment inputs.
    ///
    /// # Errors
    ///
    /// Same as [`Paths::resolve`].
    pub(crate) fn resolve_from(env: &pohunek_paths::PathEnv) -> Result<Self, DaemonError> {
        let platform = pohunek_paths::Platform::current().map_err(path_error)?;
        let effective_uid = nix::unistd::Uid::effective().as_raw();
        let base = pohunek_paths::BasePaths::resolve_for(platform, effective_uid, env)
            .map_err(path_error)?;

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

    /// Returns the owner-private durable host-state directory.
    #[must_use]
    pub fn host_state_dir(&self) -> PathBuf {
        self.state_dir.join(pohunek_paths::HOST_STATE_SUBDIR)
    }

    /// Returns the stable host identity record path.
    #[must_use]
    pub fn host_identity_path(&self) -> PathBuf {
        self.host_state_dir()
            .join(pohunek_paths::HOST_IDENTITY_NAME)
    }

    /// Returns the host approval signing secret path.
    #[must_use]
    pub fn host_approval_key_path(&self) -> PathBuf {
        self.host_state_dir()
            .join(pohunek_paths::HOST_APPROVAL_KEY_NAME)
    }

    /// Returns the durable host governance record path.
    #[must_use]
    pub fn host_governance_path(&self) -> PathBuf {
        self.host_state_dir()
            .join(pohunek_paths::HOST_GOVERNANCE_NAME)
    }

    /// Returns the cross-process host-state lock path.
    #[must_use]
    pub fn host_state_lock_path(&self) -> PathBuf {
        self.host_state_dir()
            .join(pohunek_paths::HOST_STATE_LOCK_NAME)
    }

    /// Returns the cross-process data-authority lock path.
    #[must_use]
    pub fn data_authority_lock_path(&self) -> PathBuf {
        self.data_dir.join(DATA_AUTHORITY_LOCK_NAME)
    }
}

fn path_error(err: pohunek_paths::PathError) -> DaemonError {
    match err {
        pohunek_paths::PathError::MissingEnv { var } => DaemonError::MissingEnv { var },
        other => DaemonError::Paths(other),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pohunek_paths::{PathEnv, APP_DIR, ASSISTANT_RUNTIME_SUBDIR, KNOWLEDGE_CACHE_SUBDIR};
    use pohunek_test_support::process_env::ProcessEnv;

    use super::*;

    fn tmp_base(tag: &str) -> PathBuf {
        pohunek_test_support::temp_root()
            .join(format!("pohunek-paths-{tag}-{}", std::process::id()))
    }

    /// Every base variable the resolver reads, set to paths below `base`, so a
    /// test can drop one variable without tripping an unrelated fail-fast.
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
    fn config_dir_from_xdg_config_home() {
        let base = tmp_base("xdg");
        let env = all_present(&base);
        let paths = Paths::resolve_from(&env).expect("resolve with all base vars set");
        assert_eq!(paths.config_dir, base.join("cfg").join(APP_DIR));
    }

    #[test]
    fn config_dir_falls_back_to_home_dot_config() {
        let base = tmp_base("home");
        let mut env = all_present(&base);
        env.xdg_config_home = None;
        let paths = Paths::resolve_from(&env).expect("resolve with XDG_CONFIG_HOME unset");
        assert_eq!(
            paths.config_dir,
            base.join("home").join(".config").join(APP_DIR)
        );
    }

    #[test]
    fn cache_dir_from_xdg_cache_home() {
        let base = tmp_base("xdg-cache");
        let env = all_present(&base);
        let paths = Paths::resolve_from(&env).expect("resolve with all base vars set");
        assert_eq!(paths.cache_dir, base.join("cache").join(APP_DIR));
    }

    #[test]
    fn cache_dir_falls_back_to_home_dot_cache() {
        let base = tmp_base("home-cache");
        let mut env = all_present(&base);
        env.xdg_cache_home = None;
        let paths = Paths::resolve_from(&env).expect("resolve with XDG_CACHE_HOME unset");
        assert_eq!(
            paths.cache_dir,
            base.join("home").join(".cache").join(APP_DIR)
        );
    }

    #[test]
    fn assistant_dirs_have_expected_shape() {
        let base = tmp_base("assistant");
        let env = all_present(&base);
        let paths = Paths::resolve_from(&env).expect("resolve with all base vars set");
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
        let base = tmp_base("assistant-unsafe");
        let env = all_present(&base);
        let paths = Paths::resolve_from(&env).expect("resolve with all base vars set");

        for id in ["", "/work/launch", "../launch", "launch/child", "launch/.."] {
            assert_eq!(
                paths.assistant_runtime_dir(id),
                None,
                "unsafe id should be rejected: {id:?}"
            );
        }
    }

    #[test]
    fn missing_config_home_and_home_fails_fast() {
        let base = tmp_base("missing");
        let mut env = all_present(&base);
        // XDG_STATE_HOME/XDG_DATA_HOME stay set so the earlier steps do not need
        // HOME; only the config step must fail, and with the actionable message.
        env.xdg_config_home = None;
        env.home = None;
        match Paths::resolve_from(&env) {
            Err(DaemonError::MissingEnv { var }) => {
                assert_eq!(var, "XDG_CONFIG_HOME or HOME");
            }
            other => panic!("expected MissingEnv, got {other:?}"),
        }
    }

    #[test]
    fn resolve_reads_the_process_environment() {
        let base = tmp_base("process-env");
        let mut env = ProcessEnv::lock();
        env.set("XDG_RUNTIME_DIR", base.join("run"))
            .set("XDG_STATE_HOME", base.join("state"))
            .set("XDG_DATA_HOME", base.join("data"))
            .set("XDG_CONFIG_HOME", base.join("cfg"))
            .set("XDG_CACHE_HOME", base.join("cache"))
            .set("HOME", base.join("home"));

        let paths = Paths::resolve().expect("resolve from the process environment");

        assert_eq!(paths.config_dir, base.join("cfg").join(APP_DIR));
        assert_eq!(paths.cache_dir, base.join("cache").join(APP_DIR));
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

    #[test]
    fn host_state_paths_have_the_canonical_layout() {
        let base = tmp_base("host-state");
        let env = all_present(&base);
        let paths = Paths::resolve_from(&env).expect("resolve paths");
        let host = base
            .join("state")
            .join(APP_DIR)
            .join(pohunek_paths::HOST_STATE_SUBDIR);

        assert_eq!(paths.host_state_dir(), host);
        assert_eq!(
            paths.host_identity_path(),
            host.join(pohunek_paths::HOST_IDENTITY_NAME)
        );
        assert_eq!(
            paths.host_approval_key_path(),
            host.join(pohunek_paths::HOST_APPROVAL_KEY_NAME)
        );
        assert_eq!(
            paths.host_governance_path(),
            host.join(pohunek_paths::HOST_GOVERNANCE_NAME)
        );
        assert_eq!(
            paths.host_state_lock_path(),
            host.join(pohunek_paths::HOST_STATE_LOCK_NAME)
        );
        assert_eq!(
            paths.data_authority_lock_path(),
            base.join("data")
                .join(APP_DIR)
                .join(DATA_AUTHORITY_LOCK_NAME)
        );
    }
}
