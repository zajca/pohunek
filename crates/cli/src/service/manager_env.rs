//! The environment the service manager gives the daemon job.
//!
//! The daemon's systemd user unit records no `PATH` when the install chose
//! unmanaged discovery, so the daemon inherits the user manager's `PATH`. The
//! same manager environment carries the allowlisted variables a daemon reads,
//! such as a store location, so a preflight judges with them too. A manager
//! that cannot answer is an error: the caller never falls back to an empty
//! `PATH` or to the CLI's own environment. Values are never part of an error.

// Rust guideline compliant 2026-10-14

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;

use pohunek_platform::supervisor::SEARCH_PATH_VARIABLE;
use pohunek_worker_protocol::BaseEnv;

use super::error::Error;

/// Why the service manager's environment was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentFailure {
    /// No service manager answers: its bus cannot be reached. The job then has
    /// no manager environment to inherit from, and the caller decides what that
    /// means for its own step.
    Unreachable(String),
    /// The manager answered, but its answer cannot be used.
    Unusable(String),
}

/// Reads the environment the systemd user manager gives its units.
pub trait UserManager: Debug + Send + Sync {
    /// Returns the `KEY=VALUE` entries of the manager's environment, or why they
    /// cannot be read.
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, EnvironmentFailure>> + Send + '_>>;
}

/// Reads the environment through the platform's D-Bus client on Linux.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemdUserManager;

impl UserManager for SystemdUserManager {
    #[cfg(target_os = "linux")]
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, EnvironmentFailure>> + Send + '_>> {
        use super::settings;

        Box::pin(async {
            pohunek_platform::supervisor::systemd::user_manager_environment(
                settings::MANAGER_ENVIRONMENT_TIMEOUT,
            )
            .await
            .map_err(|error| {
                // Only an absent bus means no manager is reachable; a bus that
                // refuses the client is an answer that cannot be used.
                let detail = error.to_string();
                if error.is_no_bus() {
                    EnvironmentFailure::Unreachable(detail)
                } else {
                    EnvironmentFailure::Unusable(detail)
                }
            })
        })
    }

    /// The systemd user manager exists only on Linux.
    #[cfg(not(target_os = "linux"))]
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, EnvironmentFailure>> + Send + '_>> {
        Box::pin(std::future::ready(Err(EnvironmentFailure::Unreachable(
            "the systemd user manager exists only on Linux".to_owned(),
        ))))
    }
}

/// What the systemd user manager gives the daemon job, read once.
#[derive(Clone, PartialEq, Eq)]
pub struct UserManagerEnvironment {
    /// The manager's `PATH`, byte for byte, when it sets one. The daemon job
    /// runs with exactly this value.
    pub search_path: Option<String>,
    /// The manager's variables the allowlist selects, `PATH` excluded.
    pub variables: BTreeMap<String, String>,
}

// Variables are printed by name only: their values may be secrets.
impl Debug for UserManagerEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserManagerEnvironment")
            .field("search_path", &self.search_path)
            .field("variables", &self.variables.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Reads the environment of the systemd user manager through `manager` once.
///
/// # Errors
///
/// Returns the manager's [`EnvironmentFailure`].
pub async fn read_user_manager_entries(
    manager: &dyn UserManager,
) -> Result<Vec<String>, EnvironmentFailure> {
    manager.environment().await
}

/// Reads the environment of the systemd user manager through `manager` once,
/// and selects its variables with `allowlist`. Every failure to read it is an
/// error: a caller that may go on without the manager uses
/// [`read_user_manager_entries`] instead.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] when the environment cannot be read or
/// an allowlisted variable cannot be carried by a job environment.
pub async fn user_manager_environment(
    manager: &dyn UserManager,
    allowlist: &[String],
) -> Result<UserManagerEnvironment, Error> {
    let entries = read_user_manager_entries(manager)
        .await
        .map_err(|failure| {
            let (EnvironmentFailure::Unreachable(detail) | EnvironmentFailure::Unusable(detail)) =
                failure;
            Error::ManagerEnvironment { detail }
        })?;
    selected_environment(&entries, allowlist)
}

/// Selects the `PATH` and the allowlisted variables of the manager's `entries`.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] when an allowlisted variable cannot be
/// carried by a job environment.
pub fn selected_environment(
    entries: &[String],
    allowlist: &[String],
) -> Result<UserManagerEnvironment, Error> {
    let prefix = format!("{SEARCH_PATH_VARIABLE}=");
    let search_path = entries
        .iter()
        .find_map(|entry| entry.strip_prefix(prefix.as_str()))
        .map(carry)
        .transpose()
        .map_err(|detail| Error::ManagerEnvironment { detail })?;
    let pairs = entries.iter().filter_map(|entry| {
        entry
            .split_once('=')
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
    });
    Ok(UserManagerEnvironment {
        search_path,
        variables: allowlisted_variables(pairs, allowlist)?,
    })
}

/// Selects the variables of a service manager environment that `allowlist`
/// names, with the job's own `PATH` left out.
///
/// The selection is the one a daemon applies to its launch environment, so a
/// variable the daemon would not inherit is never passed to a preflight. Errors
/// name variables, never their values.
///
/// # Errors
///
/// Returns [`Error::ManagerEnvironment`] for an invalid pattern or a selected
/// variable that a base environment cannot hold.
pub fn allowlisted_variables(
    entries: impl IntoIterator<Item = (String, String)>,
    allowlist: &[String],
) -> Result<BTreeMap<String, String>, Error> {
    let vars = entries
        .into_iter()
        .filter(|(name, _)| name != SEARCH_PATH_VARIABLE)
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
    let base =
        BaseEnv::from_allowlist(allowlist, vars).map_err(|error| Error::ManagerEnvironment {
            detail: format!("the service manager's environment cannot be carried: {error}"),
        })?;
    Ok(base.into_inner())
}

/// The `PATH` value of a job, if a process environment can carry it.
///
/// A value with a NUL byte cannot be placed in a process environment. Every
/// other byte sequence is passed as it is.
///
/// # Errors
///
/// Returns a message for a value with an interior NUL byte.
pub(crate) fn carry(value: &str) -> Result<String, String> {
    if value.contains('\0') {
        return Err(
            "its PATH contains a NUL byte, which a process environment cannot carry".to_owned(),
        );
    }
    Ok(value.to_owned())
}

/// What launchd reports for the daemon job's `PATH`, from both of its sources.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ActiveJob {
    /// The domain's `PATH`, set with `launchctl setenv`, when the domain has one.
    pub domain: Option<String>,
    /// The `default environment` `PATH` of the loaded job, when it is loaded.
    pub loaded_default: Option<String>,
    /// The domain's `environment` variables, `PATH` included, in launchd's order.
    pub domain_environment: Vec<(String, String)>,
}

// The domain's variables are printed by name only: their values may be secrets.
impl Debug for ActiveJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self
            .domain_environment
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        f.debug_struct("ActiveJob")
            .field("domain", &self.domain)
            .field("loaded_default", &self.loaded_default)
            .field("domain_environment", &names)
            .finish_non_exhaustive()
    }
}

/// Reads launchd's active state of the daemon job.
pub trait LaunchdJob: Debug + Send + Sync {
    /// Returns the active `PATH` of job `label` of `gui/<uid>`, or the reason it
    /// cannot be read.
    fn active_path<'a>(
        &'a self,
        uid: u32,
        label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ActiveJob, String>> + Send + 'a>>;
}

/// Reads the active state through `launchctl`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemLaunchd;

impl LaunchdJob for SystemLaunchd {
    #[cfg(target_os = "macos")]
    fn active_path<'a>(
        &'a self,
        uid: u32,
        label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ActiveJob, String>> + Send + 'a>> {
        Box::pin(async move {
            let sources = pohunek_platform::supervisor::launchd::job_path_sources(
                uid,
                label,
                super::settings::LAUNCHCTL_COMMAND,
            )
            .await
            .map_err(|error| error.to_string())?;
            Ok(ActiveJob {
                domain: sources.domain,
                loaded_default: sources.loaded_default,
                domain_environment: sources.domain_environment,
            })
        })
    }

    /// launchd exists only on macOS.
    #[cfg(not(target_os = "macos"))]
    fn active_path<'a>(
        &'a self,
        _uid: u32,
        _label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ActiveJob, String>> + Send + 'a>> {
        Box::pin(std::future::ready(Err(
            "launchd exists only on macOS".to_owned()
        )))
    }
}
