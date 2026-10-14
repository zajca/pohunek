//! The `PATH` the systemd user manager gives its units.
//!
//! The daemon's systemd user unit records no `PATH` when the install chose
//! unmanaged discovery, so the daemon inherits the user manager's `PATH`. A
//! preflight or runtime step that must judge with the daemon's `PATH` reads
//! the manager's `Environment` property through the platform's D-Bus client.
//! A manager that cannot answer is an error: the caller never falls back to an
//! empty `PATH` or to the CLI's own.

// Rust guideline compliant 2026-10-14

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;

use pohunek_platform::shell_env::SearchPath;
use pohunek_platform::supervisor::SEARCH_PATH_VARIABLE;

use super::error::Error;

/// Reads the environment the systemd user manager gives its units.
pub trait UserManager: Debug + Send + Sync {
    /// Returns the `KEY=VALUE` entries of the manager's environment, or the
    /// reason they cannot be read.
    fn environment(&self)
        -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + '_>>;
}

/// Reads the environment through the platform's D-Bus client on Linux.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemdUserManager;

impl UserManager for SystemdUserManager {
    #[cfg(target_os = "linux")]
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + '_>> {
        use super::settings;

        Box::pin(async {
            pohunek_platform::supervisor::systemd::user_manager_environment(
                settings::MANAGER_ENVIRONMENT_TIMEOUT,
            )
            .await
            .map_err(|error| error.to_string())
        })
    }

    /// The systemd user manager exists only on Linux.
    #[cfg(not(target_os = "linux"))]
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + '_>> {
        Box::pin(std::future::ready(Err(
            "the systemd user manager exists only on Linux".to_owned(),
        )))
    }
}

/// Reads the `PATH` of the systemd user manager through `manager`.
///
/// The value is validated like the login-shell `PATH` of an install: every
/// directory must be trusted, and none may be dropped.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] when the environment cannot be read,
/// has no `PATH` entry, or names a `PATH` that is not a usable trusted search
/// path.
pub async fn user_manager_search_path(manager: &dyn UserManager) -> Result<SearchPath, Error> {
    let failure = |detail: String| Error::ManagerEnvironment { detail };
    let environment = manager.environment().await.map_err(failure)?;
    search_path_of(&environment).map_err(failure)
}

/// Parses the `PATH` entry of a manager environment.
fn search_path_of(environment: &[String]) -> Result<SearchPath, String> {
    let prefix = format!("{SEARCH_PATH_VARIABLE}=");
    let value = environment
        .iter()
        .find_map(|entry| entry.strip_prefix(prefix.as_str()))
        .ok_or_else(|| "it sets no PATH entry".to_owned())?;
    let sanitized = SearchPath::sanitize(value, true)
        .map_err(|error| format!("its PATH is not a usable search path: {error}"))?;
    if !sanitized.untrusted.is_empty() {
        return Err(format!(
            "its PATH names {} untrusted director{}",
            sanitized.untrusted.len(),
            if sanitized.untrusted.len() == 1 {
                "y"
            } else {
                "ies"
            }
        ));
    }
    Ok(sanitized.path)
}
