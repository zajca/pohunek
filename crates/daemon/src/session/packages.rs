//! Runtime package lifecycle on the session registry: explicit reload and
//! retention-checked uninstall.
//!
//! A package digest stays referenced while any live, lost or resumable session
//! was launched from it, so uninstall never retires content a session can
//! still resume from. Fresh launches hold a shared guard from the moment their
//! package is verified until their durable record exists; uninstall takes the
//! exclusive guard, so a launch can never pin a digest between the retained
//! set being computed and the package being removed.

// Rust guideline compliant 2026-10-04

use std::io;
use std::sync::Arc;

use package::registry::{RegistryError, RetainedDigests};
use package::PackageDigest;
use protocol::BindingProvenance;
use thiserror::Error;

use super::{host, ProtocolError, SessionRegistry};
use crate::agent::host::{PackageReport, ReloadError};

/// Why a package could not be uninstalled.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PackageUninstallError {
    /// The registry's runtime host has no package store.
    #[error("the runtime host has no package store")]
    NoPackageStore,
    /// The durable sessions that may reference the package could not be read.
    #[error("the sessions that may reference the package cannot be listed: {0}")]
    Retention(#[source] io::Error),
    /// The package registry refused: still referenced, not installed, a
    /// modified root, or a registry fault.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// The package was uninstalled but the runtime registry could not be
    /// rebuilt; the previous registry stays in place until the next reload.
    #[error("the package was uninstalled but the runtime registry was not rebuilt: {0}")]
    Reload(#[source] ReloadError),
}

impl SessionRegistry {
    /// Rebuilds the runtime registry from the packages installed now.
    ///
    /// Reload is explicit: installing, enabling, disabling or selecting a
    /// package changes nothing until it is called. Every enabled package root
    /// is verified again; the previous registry stays in place when the
    /// rebuild fails.
    ///
    /// # Errors
    ///
    /// Returns the [`ReloadError`] of the runtime host.
    pub fn reload_runtimes(&self) -> Result<Arc<PackageReport>, ReloadError> {
        self.inner.profiles.runtimes().reload()
    }

    /// Package digests that a live, lost or resumable session was launched
    /// from, or that a host profile pins.
    ///
    /// The set is the union of the digests host profiles pin, the durable
    /// records (logical sessions and resume bindings) and the sessions held in
    /// memory, so a registry without persistence still protects its live
    /// sessions. The durable scan is strict: a record that cannot be
    /// interpreted may pin a package, so it is an error rather than an
    /// omission.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the durable store cannot be read or holds a
    /// record that cannot be interpreted.
    pub async fn retained_package_digests(&self) -> io::Result<RetainedDigests> {
        let mut retained = RetainedDigests::new();
        let profiles = self.inner.profiles.clone();
        retained.extend(
            tokio::task::spawn_blocking(move || profiles.pinned_digests())
                .await
                .map_err(|join_error| io::Error::other(join_error.to_string()))?
                .iter()
                .cloned(),
        );
        if let Some(store) = self.inner.store.clone() {
            let pinned = tokio::task::spawn_blocking(move || store.pinned_package_digests())
                .await
                .map_err(|join_error| io::Error::other(join_error.to_string()))??;
            retained.extend(pinned);
        }
        let sessions = self.inner.sessions.lock().await;
        for entry in sessions.values() {
            retained.extend(pinned_digest(&entry.snapshot.launch_binding));
        }
        Ok(retained)
    }

    /// Uninstalls the package `digest` unless a session still references it,
    /// then rebuilds the runtime registry.
    ///
    /// # Errors
    ///
    /// Returns [`PackageUninstallError::Registry`] with
    /// [`RegistryError::StillReferenced`] while a session references the
    /// digest, and the other variants for a missing store, an unreadable
    /// session store, a registry refusal or a failed rebuild.
    pub async fn uninstall_package(
        &self,
        digest: &PackageDigest,
    ) -> Result<(), PackageUninstallError> {
        self.remove_package(digest, false).await
    }

    /// Removes the package `digest` unless a session still references it, then
    /// rebuilds the runtime registry.
    ///
    /// With `remove_modified` the package must fail root verification, which a
    /// normal uninstall refuses to touch; a root that verifies is refused with
    /// [`RegistryError::RootIntact`].
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::uninstall_package`].
    pub async fn remove_package(
        &self,
        digest: &PackageDigest,
        remove_modified: bool,
    ) -> Result<(), PackageUninstallError> {
        let runtimes = self.inner.profiles.runtimes().clone();
        let store = runtimes
            .package_store()
            .ok_or(PackageUninstallError::NoPackageStore)?
            .clone();
        let registry = self.clone();
        let digest = digest.clone();
        // The whole transaction runs in a task of its own: the exclusive
        // authority is held by the task that computes the retained set,
        // removes the package and reloads, so a caller that stops waiting
        // cannot release it while the removal is still in flight.
        tokio::spawn(async move {
            let _exclusive = registry.inner.package_lifecycle.write().await;
            let retained = registry
                .retained_package_digests()
                .await
                .map_err(PackageUninstallError::Retention)?;
            let removal = tokio::task::spawn_blocking(move || {
                let registry = store.registry();
                if remove_modified {
                    registry.remove_modified(&digest, &retained)
                } else {
                    registry.uninstall(&digest, &retained)
                }
                .map(|_generation| ())
            })
            .await
            .map_err(|join_error| {
                PackageUninstallError::Retention(io::Error::other(join_error.to_string()))
            })?;
            // The record is already replaced when the root could not be fully
            // deleted or the commit's durability is uncertain, so the runtime
            // registry must follow it before the error is returned.
            let committed = matches!(
                removal,
                Ok(()) | Err(RegistryError::RemovalIncomplete | RegistryError::CommitUncertain)
            );
            let reload = committed.then(|| runtimes.reload());
            removal?;
            match reload {
                Some(Err(error)) => Err(PackageUninstallError::Reload(error)),
                _ => Ok(()),
            }
        })
        .await
        .map_err(|join_error| {
            PackageUninstallError::Retention(io::Error::other(join_error.to_string()))
        })?
    }

    /// Verifies that `definition` may serve a fresh launch and holds the
    /// shared lifecycle guard for as long as the returned value lives.
    ///
    /// # Errors
    ///
    /// Returns the stable protocol error of
    /// [`host::RuntimeHost::verify_launchable`].
    pub(super) async fn guard_package_launch(
        &self,
        definition: &host::RuntimeDefinition,
    ) -> Result<tokio::sync::RwLockReadGuard<'_, ()>, ProtocolError> {
        let guard = self.inner.package_lifecycle.read().await;
        self.inner
            .profiles
            .runtimes()
            .verify_launchable(definition)?;
        Ok(guard)
    }
}

/// The package digest a launch pin froze, if it froze one.
fn pinned_digest(pin: &host::LaunchPin) -> Option<PackageDigest> {
    match pin.binding().map(|binding| &binding.provenance) {
        Some(BindingProvenance::Package { package_digest, .. }) => Some(package_digest.clone()),
        _ => None,
    }
}
