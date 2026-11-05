//! Retention-checked package removal.
//!
//! A package digest stays referenced while any live, lost or resumable session
//! was launched from it, so uninstall never retires content a session can
//! still resume from. Fresh launches hold a shared guard from the moment their
//! package is verified until their durable record exists; uninstall takes the
//! exclusive guard, so a launch can never pin a digest between the retained
//! set being computed and the package being removed.

// Rust guideline compliant 2026-10-05

use std::io;
use std::sync::Arc;

use package::registry::{RegistryError, RetainedDigests};
use package::PackageDigest;
use thiserror::Error;

use super::PackageService;
use crate::agent::host::{PackageReport, ReloadError, RuntimeHost};

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
    /// A store line that may pin the package cannot be read, so the removal
    /// cannot be proven safe.
    #[error("an unreadable metadata-store line may reference the package")]
    RetentionUncertain,
    /// The package registry refused: still referenced, not installed, a
    /// modified root, or a registry fault.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// The package was uninstalled but the runtime registry could not be
    /// rebuilt; the previous registry stays in place until the next reload.
    #[error("the package was uninstalled but the runtime registry was not rebuilt: {0}")]
    Reload(#[source] ReloadError),
}

/// The package digests the daemon retains, for display and for gates.
///
/// An unreadable store line that may pin a package never shrinks a gate's set:
/// [`Self::gate`] widens to every installed digest while one exists, and only
/// gates see it.
#[derive(Debug, Clone, Default)]
pub(crate) struct RetainedPins {
    /// Digests a profile, a readable durable record or a live session pins.
    known: RetainedDigests,
    /// [`Self::known`], plus every installed digest while a store line that may
    /// pin a package cannot be read.
    gate: RetainedDigests,
    /// Whether such a line exists.
    uncertain: bool,
}

impl RetainedPins {
    /// The known digests of a read that found `unreadable_lines` store lines
    /// that may pin a package and cannot be read. While one exists the gate
    /// also holds every digest in `installed`, so an unreadable line never
    /// shrinks what a gate judges.
    #[must_use]
    pub(crate) fn with_unreadable(
        known: RetainedDigests,
        unreadable_lines: usize,
        installed: impl IntoIterator<Item = PackageDigest>,
    ) -> Self {
        let uncertain = unreadable_lines > 0;
        let mut gate = known.clone();
        if uncertain {
            gate.extend(installed);
        }
        Self {
            known,
            gate,
            uncertain,
        }
    }

    /// The digests that are provably pinned.
    #[must_use]
    pub(crate) fn known(&self) -> &RetainedDigests {
        &self.known
    }

    /// The provably pinned digests, by value.
    #[must_use]
    pub(crate) fn into_known(self) -> RetainedDigests {
        self.known
    }

    /// The set every gate judges.
    #[must_use]
    pub(crate) fn gate(&self) -> &RetainedDigests {
        &self.gate
    }

    /// Whether an unreadable store line widened [`Self::gate`].
    #[must_use]
    pub(crate) fn is_uncertain(&self) -> bool {
        self.uncertain
    }
}

/// The digest of every installed package; empty without a package store.
///
/// # Errors
///
/// Returns an error when the package registry state cannot be read.
pub(crate) fn installed_digests(runtimes: &RuntimeHost) -> io::Result<Vec<PackageDigest>> {
    runtimes
        .package_store()
        .map(|store| {
            store.registry().state().map(|state| {
                state
                    .packages()
                    .iter()
                    .map(|record| record.digest().clone())
                    .collect::<Vec<_>>()
            })
        })
        .transpose()
        .map(Option::unwrap_or_default)
        .map_err(|error| io::Error::other(error.to_string()))
}

impl PackageService {
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
        self.authority.runtimes.reload()
    }

    /// Uninstalls the package `digest` unless a session still references it,
    /// then rebuilds the runtime registry.
    ///
    /// # Errors
    ///
    /// Returns [`PackageUninstallError::Registry`] with
    /// [`RegistryError::StillReferenced`] while a session references the
    /// digest, [`PackageUninstallError::RetentionUncertain`] while an
    /// unreadable store line may reference it, and the other variants for a missing store, an unreadable
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
        let runtimes = self.authority.runtimes.clone();
        let store = runtimes
            .package_store()
            .ok_or(PackageUninstallError::NoPackageStore)?
            .clone();
        let sessions = self.sessions.clone();
        let authority = Arc::clone(&self.authority);
        let digest = digest.clone();
        // The whole transaction runs in a task of its own: the exclusive
        // authority is held by the task that computes the retained set,
        // removes the package and reloads, so a caller that stops waiting
        // cannot release it while the removal is still in flight.
        self.sessions
            .spawn_transaction("package_uninstall", async move {
                let _exclusive = authority.lifecycle.write().await;
                let pins = sessions
                    .retained_package_pins()
                    .await
                    .map_err(PackageUninstallError::Retention)?;
                let removal = tokio::task::spawn_blocking(move || {
                    let registry = store.registry();
                    let result = if remove_modified {
                        registry.remove_modified(&digest, pins.gate())
                    } else {
                        registry.uninstall(&digest, pins.gate())
                    };
                    match result {
                        Err(RegistryError::StillReferenced)
                            if pins.is_uncertain() && !pins.known().contains(&digest) =>
                        {
                            Err(PackageUninstallError::RetentionUncertain)
                        }
                        Err(error) => Err(PackageUninstallError::Registry(error)),
                        Ok(_generation) => Ok(()),
                    }
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
                    Ok(())
                        | Err(PackageUninstallError::Registry(
                            RegistryError::RemovalIncomplete | RegistryError::CommitUncertain
                        ))
                );
                let reload = committed.then(|| runtimes.reload());
                sessions.refresh_adopted_pins().await;
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
}
