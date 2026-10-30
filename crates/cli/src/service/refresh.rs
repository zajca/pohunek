//! `pohunek service refresh-path`: re-records the executable search path.
//!
//! Install records the path a login shell reports in `service.toml`, and
//! upgrades reuse it. A prefix installed later, or a profile link that moved
//! (nix generations), leaves the record behind. This command runs the same
//! bounded discovery, shows the difference and rewrites only the recorded
//! path, atomically. Nothing is stopped or restarted, and the service manager
//! is not contacted.

// Rust guideline compliant 2026-06-26

use pohunek_service_config::ServiceConfig;

use super::context::Context;
use super::error::Error;
use super::record::{Store, TransactionLock};
use super::report::RefreshPathReport;

/// Re-records the search path of the installation `context` locates.
///
/// The recorded path is what `service check` and `service upgrade` judge
/// runtimes with, and what the daemon job gets at its next replacement (a
/// version-changing upgrade); the running daemon keeps its `PATH` until then.
/// With `dry_run` the file is left alone. A path that is already current is a
/// no-op. `inherited` is the transaction lock of a `pohunek service lock`
/// ancestor; without one the command takes the lock itself.
///
/// # Errors
///
/// Returns [`Error::NotInstalled`] without an installation,
/// [`Error::RefreshBlockedByTransaction`] while a transaction record exists,
/// [`Error::SearchPathUnmanaged`] when the host records no path,
/// [`Error::SearchPathRefreshFailed`] when the login shell supplies none (the
/// fallback list never replaces a recorded path), and the configuration and
/// write errors of `service.toml`.
pub(crate) async fn refresh_search_path(
    context: &Context,
    inherited: Option<&TransactionLock>,
    dry_run: bool,
) -> Result<RefreshPathReport, Error> {
    let store = Store::new(context.paths().state_dir.clone());
    let _transaction = match inherited {
        Some(_) => None,
        None => Some(store.lock().await?),
    };
    super::engine::upgrade_preflight(context)?;
    let config_path = context.config_path();
    if !super::engine::file_exists(&config_path)? {
        return Err(Error::NotInstalled { path: config_path });
    }
    if store.load()?.is_some() {
        return Err(Error::RefreshBlockedByTransaction);
    }
    let config = ServiceConfig::load(&config_path)?;
    config.verify_installation(
        context.uid(),
        &context.paths().state_dir,
        &context.paths().runtime_dir,
    )?;
    let (fresh, found) = context.install_search_path().map_err(|error| match error {
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
    let recorded = config.search_path().entries().to_vec();
    let discovered = fresh.entries().to_vec();
    let added: Vec<_> = discovered
        .iter()
        .filter(|entry| !recorded.contains(entry))
        .cloned()
        .collect();
    let removed: Vec<_> = recorded
        .iter()
        .filter(|entry| !discovered.contains(entry))
        .cloned()
        .collect();
    let changed = recorded != discovered;
    let applied = changed && !dry_run;
    if applied {
        config.with_search_path(fresh)?.write(&config_path)?;
    }
    Ok(RefreshPathReport {
        dry_run,
        changed,
        applied,
        recorded,
        discovered,
        added,
        removed,
        shell_used: found.shell_used,
        shell_defaulted: found.shell_defaulted,
        dropped: found.dropped,
        warnings: super::definition::unknown_keys_warning(&config)
            .into_iter()
            .collect(),
    })
}
