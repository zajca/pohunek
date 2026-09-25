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
use std::time::Duration;

use pohunek_platform::supervisor::Namespace;
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
            transaction_in_progress: store.in_progress()?,
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

/// Connects the backend of the recorded installation, or of an interrupted
/// install that has not registered anything yet.
///
/// With `service.toml` the backend is built from the verified installation:
/// [`verified_connection`] confirms the recorded user and canonical roots
/// describe this process, and the namespace and `launchctl` deadline come
/// from the recording, never from the current environment alone. A changed
/// `XDG_STATE_HOME` or `XDG_RUNTIME_DIR` would otherwise point the backend at
/// another installation's jobs while `service.toml` still names the original
/// one. Without `service.toml` only a pre-registration install connects,
/// against the namespace this process derives: nothing of that transaction
/// can have been registered yet.
async fn connect_installed(context: &Context) -> Result<Backend, Error> {
    let config_path = context.config_path();
    if exists(&config_path)? {
        let (namespace, deadline) = verified_connection(context)?;
        return Backend::connect(context, &namespace, deadline).await;
    }
    let pending = record::Store::new(context.paths().state_dir.clone()).load()?;
    if pending.is_none() {
        return Err(Error::NotInstalled { path: config_path });
    }
    Backend::connect(context, &context.namespace()?, settings::LAUNCHCTL_COMMAND).await
}

/// Verifies the installation recorded at `context.config_path()` against the
/// running process and returns the namespace and `launchctl` deadline its
/// backend must use.
///
/// The caller has checked that the file exists.
///
/// # Errors
///
/// Returns [`Error::Config`] when the file cannot be read or its recorded
/// namespace inputs differ from this process's user or canonical roots.
fn verified_connection(context: &Context) -> Result<(Namespace, Duration), Error> {
    let config = ServiceConfig::load(&context.config_path())?;
    config.verify_installation(
        context.uid(),
        &context.paths().state_dir,
        &context.paths().runtime_dir,
    )?;
    Ok((config.namespace(), config.deadlines().launchctl_command))
}

fn exists(path: &Path) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(_metadata) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error::io_error("inspect", path)(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use pohunek_paths::{BasePaths, PathEnv, Platform};
    use pohunek_service_config::ConfigError;

    use super::*;
    use crate::service::context::tests::temp_root;
    use crate::service::definition::initial_config;

    /// Resolves paths from an XDG layout whose state and runtime roots a test
    /// can move independently; the config home pins where `service.toml` is
    /// read from.
    fn moved_paths(
        root: &Path,
        config_home: &Path,
        state_home: &Path,
        runtime_dir: &Path,
    ) -> BasePaths {
        let env = PathEnv {
            xdg_runtime_dir: Some(OsString::from(runtime_dir)),
            xdg_data_home: Some(OsString::from(root.join("data"))),
            xdg_state_home: Some(OsString::from(state_home)),
            xdg_cache_home: Some(OsString::from(root.join("cache"))),
            xdg_config_home: Some(OsString::from(config_home)),
            home: Some(OsString::from(root.join("home"))),
        };
        BasePaths::resolve_for(
            Platform::current().expect("supported platform"),
            nix::unistd::Uid::effective().as_raw(),
            &env,
        )
        .expect("resolve test paths")
    }

    fn context_of(
        root: &Path,
        config_home: &Path,
        state_home: &Path,
        runtime_dir: &Path,
        uid: u32,
    ) -> Context {
        Context::new(
            moved_paths(root, config_home, state_home, runtime_dir),
            uid,
            Some(root.join("home")),
            Some(runtime_dir.to_path_buf()),
            root.join("units"),
            PathBuf::from("/usr/bin/pohunek"),
        )
    }

    /// Writes the `service.toml` an install of `context` would record.
    fn install_config(context: &Context) {
        let config = initial_config(context, &context.default_prefix().expect("home"), "1.0.0")
            .expect("valid config");
        config.write(&context.config_path()).expect("write config");
    }

    /// The installed context and the canonical roots it recorded.
    fn installed() -> (tempfile::TempDir, PathBuf, Context, PathBuf) {
        let (temp, root) = temp_root();
        let context = context_of(
            &root,
            &root.join("config"),
            &root.join("state"),
            &root.join("run"),
            nix::unistd::Uid::effective().as_raw(),
        );
        install_config(&context);
        let state_root = context.roots().expect("roots").0;
        (temp, root, context, state_root)
    }

    #[test]
    fn verified_connection_accepts_the_environment_the_installation_records() {
        let (_temp, _root, context, _state) = installed();
        let (namespace, deadline) = verified_connection(&context).expect("verified");
        assert_eq!(namespace, context.namespace().expect("namespace"));
        assert_eq!(
            deadline,
            ServiceConfig::load(&context.config_path())
                .expect("config")
                .deadlines()
                .launchctl_command
        );
    }

    #[test]
    fn verified_connection_fails_closed_when_xdg_state_home_changed() {
        let (_temp, root, _context, state_root) = installed();
        let (moved_temp, moved) = temp_root();

        // Only XDG_STATE_HOME moves; the config home still locates
        // `service.toml`.
        let changed = context_of(
            &root,
            &root.join("config"),
            &moved.join("state"),
            &root.join("run"),
            nix::unistd::Uid::effective().as_raw(),
        );
        changed.roots().expect("the moved roots resolve");
        let error = verified_connection(&changed).expect_err("mismatch");
        let Error::Config(ConfigError::NamespaceMismatch {
            key,
            recorded: old,
            actual,
        }) = &error
        else {
            panic!("unexpected error {error:?}");
        };
        assert_eq!(*key, "namespace.state_root");
        assert_eq!(old, &state_root.display().to_string());
        assert_eq!(actual, &moved.join("state/pohunek").display().to_string());
        drop(moved_temp);
        assert_eq!(error.code(), "service_config_invalid");
    }

    #[test]
    fn verified_connection_fails_closed_when_xdg_runtime_dir_changed() {
        let (_temp, root, _context, _state) = installed();
        let (moved_temp, moved) = temp_root();

        // Only XDG_RUNTIME_DIR moves; the daemon socket the backend would
        // address lives under the moved runtime root, not the recorded one.
        let changed = context_of(
            &root,
            &root.join("config"),
            &root.join("state"),
            &moved.join("run"),
            nix::unistd::Uid::effective().as_raw(),
        );
        changed.roots().expect("the moved roots resolve");
        let error = verified_connection(&changed).expect_err("mismatch");
        assert!(
            matches!(
                &error,
                Error::Config(ConfigError::NamespaceMismatch { key, .. }) if *key == "namespace.runtime_root"
            ),
            "{error:?}"
        );
        drop(moved_temp);
    }

    #[test]
    fn verified_connection_fails_closed_for_another_user() {
        let (_temp, root, _context, _state) = installed();
        let foreign = context_of(
            &root,
            &root.join("config"),
            &root.join("state"),
            &root.join("run"),
            nix::unistd::Uid::effective().as_raw() + 1,
        );
        let error = verified_connection(&foreign).expect_err("mismatch");
        assert!(
            matches!(
                &error,
                Error::Config(ConfigError::NamespaceMismatch { key, .. }) if *key == "namespace.uid"
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn connect_installed_refuses_to_connect_when_the_roots_changed() {
        let (_temp, root, _context, _state) = installed();
        let (moved_temp, moved) = temp_root();
        let changed = context_of(
            &root,
            &root.join("config"),
            &moved.join("state"),
            &root.join("run"),
            nix::unistd::Uid::effective().as_raw(),
        );
        changed.roots().expect("the moved roots resolve");

        // The mismatch fails before any service manager is contacted, so the
        // assertion holds with and without a session bus.
        let error = connect_installed(&changed).await.expect_err("refused");
        assert!(
            matches!(&error, Error::Config(ConfigError::NamespaceMismatch { .. })),
            "{error:?}"
        );
        drop(moved_temp);
    }
}
