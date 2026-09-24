//! Installs, upgrades, uninstalls, and reports the native pohunek service.
//!
//! This is the engine behind `pohunek service install|upgrade|uninstall|status`.
//! It installs versioned binaries under `<prefix>/libexec/pohunek/<version>/`,
//! writes the owner-private `service.toml`, and registers the daemon as a
//! systemd user unit (Linux) or a launchd login agent (macOS) through the
//! platform supervisor backends. Install and upgrade are journaled
//! transactions; see [`engine`] for the exact steps, resume, and rollback
//! rules, and [`report`] for the `--json` result shapes.
//!
//! The functions here resolve the host [`Context`], connect the native
//! [`Backend`], and run the [`Engine`]. Tests and alternative front ends can
//! assemble those pieces explicitly instead, for example to point the
//! systemd backend at another unit directory.

// Rust guideline compliant 2026-09-24

use std::path::{Path, PathBuf};

use pohunek_service_config::ServiceConfig;

pub mod backend;
pub mod context;
pub mod definition;
pub mod engine;
pub mod error;
pub mod layout;
pub mod record;
pub mod report;
pub mod settings;
pub mod usage;
#[cfg(target_os = "linux")]
pub mod verify;

#[doc(inline)]
pub use backend::Backend;
#[doc(inline)]
pub use context::Context;
#[doc(inline)]
pub use engine::{Engine, UninstallOptions};
#[doc(inline)]
pub use error::Error;

/// Version of this CLI and of the binaries it installs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Installs this CLI's version from `from` below `prefix`.
///
/// `from` defaults to the directory of the running `pohunek`, and `prefix`
/// to `$HOME/.local`.
///
/// # Errors
///
/// Returns [`Error`] for invalid paths, an existing installation, failed
/// verification, or a failed transaction step.
pub async fn install(
    from: Option<PathBuf>,
    prefix: Option<PathBuf>,
) -> Result<report::InstallReport, Error> {
    let context = Context::resolve()?;
    let from = staged_dir(&context, from)?;
    let prefix = match prefix {
        Some(prefix) => absolute("--prefix", prefix)?,
        None => context.default_prefix()?,
    };
    let namespace = context.namespace()?;
    let backend = Backend::connect(&context, &namespace, settings::LAUNCHCTL_COMMAND).await?;
    Engine::new(&context, &backend)
        .install(&from, &prefix, VERSION)
        .await
}

/// Upgrades the installation to this CLI's version from `from`.
///
/// # Errors
///
/// Returns [`Error::NotInstalled`] without an installation, and the same
/// errors as [`install`] otherwise.
pub async fn upgrade(from: Option<PathBuf>) -> Result<report::UpgradeReport, Error> {
    let context = Context::resolve()?;
    let from = staged_dir(&context, from)?;
    let backend = connect_installed(&context).await?;
    Engine::new(&context, &backend)
        .upgrade(&from, VERSION)
        .await
}

/// Uninstalls the service.
///
/// # Errors
///
/// Returns [`Error::NotInstalled`] without an installation,
/// [`Error::LiveSessions`] while sessions are live and `--stop-sessions` is
/// absent, and backend errors otherwise.
pub async fn uninstall(options: UninstallOptions) -> Result<report::UninstallReport, Error> {
    let context = Context::resolve()?;
    let backend = connect_installed(&context).await?;
    Engine::new(&context, &backend).uninstall(options).await
}

/// Reports the installation.
///
/// Without `service.toml` the report says so and no service manager is
/// contacted.
///
/// # Errors
///
/// Returns [`Error`] for an unreadable `service.toml` or unsafe directories.
pub async fn status() -> Result<report::StatusReport, Error> {
    let context = Context::resolve()?;
    let config_path = context.config_path();
    if !exists(&config_path)? {
        let store = record::Store::new(context.paths().state_dir.clone());
        return Ok(report::StatusReport {
            installed: false,
            config_path,
            namespace: None,
            prefix: None,
            active_version: None,
            daemon: None,
            daemon_error: None,
            versions: Vec::new(),
            workers: Vec::new(),
            workers_error: None,
            unreadable_journals: Vec::new(),
            pending_transaction: store.load()?.map(|pending| report::PendingReport {
                operation: pending.operation.as_str(),
                version: pending.version,
                step: pending.step.as_str(),
            }),
        });
    }
    let config = ServiceConfig::load(&config_path)?;
    let backend = Backend::connect(
        &context,
        &config.namespace(),
        config.deadlines().launchctl_command,
    )
    .await?;
    Engine::new(&context, &backend).status(Some(&config)).await
}

fn staged_dir(context: &Context, from: Option<PathBuf>) -> Result<PathBuf, Error> {
    match from {
        Some(from) => absolute("--from", from),
        None => Ok(context
            .cli_executable()
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)),
    }
}

fn absolute(flag: &'static str, path: PathBuf) -> Result<PathBuf, Error> {
    use std::path::Component;

    let normalized = path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)));
    if normalized {
        Ok(path)
    } else {
        Err(Error::InvalidPath { flag, path })
    }
}

/// Connects the backend of an installation or of an interrupted install.
///
/// The configured `launchctl` deadline applies once `service.toml` exists.
async fn connect_installed(context: &Context) -> Result<Backend, Error> {
    let config_path = context.config_path();
    let pending = record::Store::new(context.paths().state_dir.clone()).load()?;
    let deadline = if exists(&config_path)? {
        ServiceConfig::load(&config_path)?
            .deadlines()
            .launchctl_command
    } else if pending.is_some() {
        settings::LAUNCHCTL_COMMAND
    } else {
        return Err(Error::NotInstalled { path: config_path });
    };
    Backend::connect(context, &context.namespace()?, deadline).await
}

fn exists(path: &Path) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(_metadata) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error::io_error("inspect", path)(error)),
    }
}
