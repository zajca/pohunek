//! `pohunek service refresh-path`: re-records the executable search path and
//! replaces the daemon job, so the running daemon gets it.
//!
//! Install records the path a login shell reports in `service.toml`, and
//! upgrades reuse it. A prefix installed later, or a profile link that moved
//! (nix generations), leaves the record behind, and the daemon keeps running
//! with the `PATH` its job was registered with. The command runs the same
//! bounded discovery, shows the difference, rewrites only the recorded path
//! (atomically) and replaces the daemon job in one journaled transaction
//! ([`super::engine::Engine::refresh_path`]); it either does both or changes
//! nothing. Worker jobs are never touched.
//!
//! The login shell runs here, before any transaction lock is taken, so a slow
//! shell never keeps another service command waiting. Everything that depends
//! on the recorded state is decided later, under the lock, against the freshly
//! loaded file.

// Rust guideline compliant 2026-06-26

use std::ffi::OsStr;
use std::path::PathBuf;

use pohunek_platform::shell_env::{resolve_executable, SearchPath};
use pohunek_service_config::{ConfigError, ServiceConfig, SCHEMA_VERSION};

use super::context::Context;
use super::error::Error;
use super::report::SearchPathReport;

/// The search path a fresh login-shell discovery reports.
#[derive(Debug, Clone)]
pub(crate) struct Discovered {
    /// The validated directories, in search order.
    pub(crate) search_path: SearchPath,
    /// How they were obtained: the shell tried and the directories dropped.
    pub(crate) found: SearchPathReport,
}

/// Runs the bounded login-shell discovery of the installation `context` locates.
///
/// Takes no lock and contacts no service manager.
///
/// # Errors
///
/// Returns [`Error::NotInstalled`] without an installation,
/// [`Error::SearchPathUnmanaged`] when the host records no path, and
/// [`Error::SearchPathRefreshFailed`] when the login shell supplies none (the
/// fallback list never replaces a recorded path).
pub(crate) fn discover(context: &Context) -> Result<Discovered, Error> {
    // A host without an installation never runs the login shell; the check is
    // repeated under the lock.
    let config_path = context.config_path();
    if !super::engine::file_exists(&config_path)? {
        return Err(Error::NotInstalled { path: config_path });
    }
    let (search_path, found) = context.install_search_path().map_err(|error| match error {
        Error::SearchPath(reason) => Error::SearchPathRefreshFailed {
            detail: reason.to_string(),
        },
        other => other,
    })?;
    if found.source == "unmanaged" {
        return Err(Error::SearchPathUnmanaged);
    }
    if found.source != "login_shell" {
        return Err(Error::SearchPathRefreshFailed {
            detail: found
                .login_shell_failure
                .unwrap_or_else(|| "no login shell was asked".to_owned()),
        });
    }
    Ok(Discovered { search_path, found })
}

/// Loads the installed `service.toml` of the current schema, with its exact
/// bytes, after proving it describes this installation.
///
/// # Errors
///
/// Returns [`Error::NotInstalled`] without an installation,
/// [`Error::RefreshConfigSchema`] for a file of another schema, and the
/// configuration errors of the read and of the namespace verification.
pub(crate) fn load_installed_config(context: &Context) -> Result<(ServiceConfig, Vec<u8>), Error> {
    let config_path = context.config_path();
    if !super::engine::file_exists(&config_path)? {
        return Err(Error::NotInstalled { path: config_path });
    }
    // One trusted read supplies the typed values and the exact bytes. A file of
    // another schema may not parse as the older one the upgrade reader accepts,
    // so a failure is classified by the strict reader, which names the schema.
    let source = match ServiceConfig::load_upgrade_source(&config_path) {
        Ok(source) => source,
        Err(ConfigError::UnsupportedSchema { found }) => {
            return Err(Error::RefreshConfigSchema {
                found,
                current: SCHEMA_VERSION,
            });
        }
        Err(error) => {
            return Err(match ServiceConfig::load(&config_path) {
                Err(ConfigError::UnsupportedSchema { found }) => Error::RefreshConfigSchema {
                    found,
                    current: SCHEMA_VERSION,
                },
                _ => error.into(),
            });
        }
    };
    if source.source_schema_version != SCHEMA_VERSION {
        return Err(Error::RefreshConfigSchema {
            found: i64::from(source.source_schema_version),
            current: SCHEMA_VERSION,
        });
    }
    let bytes = source.original_bytes().to_vec();
    source.config.verify_installation(
        context.uid(),
        &context.paths().state_dir,
        &context.paths().runtime_dir,
    )?;
    Ok((source.config, bytes))
}

/// The agent programs `pohunek doctor` probes by name.
pub(crate) const PROBED_AGENTS: [&str; 2] = ["claude", "codex"];

/// An agent program only the login shell's search path resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoginPathOnlyAgent {
    /// The program name.
    pub(crate) name: &'static str,
    /// Where the login shell's path finds it.
    pub(crate) found: PathBuf,
}

/// The agent programs the login shell's path resolves and the recorded
/// search path does not.
///
/// `None` is no verdict: a host that records no path, no installation or one
/// `service.toml` cannot describe (including a runtime root that does not exist
/// yet), a transaction record that is pending (the file and the daemon job may
/// then disagree), an empty recorded path, or a login shell that fails or
/// hangs. `Some` lists the programs only the login shell reaches, empty when
/// every program it reaches resolves through the recorded path too.
/// Runs the bounded discovery without a lock and contacts no service manager.
pub(crate) fn agents_behind_recorded_path(context: &Context) -> Option<Vec<LoginPathOnlyAgent>> {
    if !context.records_search_path() {
        return None;
    }
    let (config, _) = load_installed_config(context).ok()?;
    if !matches!(
        super::record::Store::new(context.paths().state_dir.clone()).load(),
        Ok(None)
    ) {
        return None;
    }
    if config.search_path().entries().is_empty() {
        return None;
    }
    let discovered = discover(context).ok()?;
    Some(agents_only_in(
        config.search_path(),
        &discovered.search_path,
    ))
}

fn agents_only_in(recorded: &SearchPath, login: &SearchPath) -> Vec<LoginPathOnlyAgent> {
    PROBED_AGENTS
        .into_iter()
        .filter_map(|name| {
            if resolve_executable(OsStr::new(name), recorded).is_ok() {
                return None;
            }
            let found = resolve_executable(OsStr::new(name), login).ok()?;
            Some(LoginPathOnlyAgent { name, found })
        })
        .collect()
}
