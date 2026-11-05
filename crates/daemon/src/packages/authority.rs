//! The owner of the runtime package lifecycle.

// Rust guideline compliant 2026-11-05

use std::sync::Arc;

use protocol::{BindingProvenance, ProtocolError};
use tokio::sync::{OwnedRwLockReadGuard, RwLock, RwLockReadGuard};

use crate::agent::host::{RuntimeDefinition, RuntimeHost};
use crate::agent::ProfileRegistry;
use crate::catalog_anchor::CatalogTrust;
use crate::session::SessionRegistry;

/// Owns the package registry handle, the catalog trust and the lifecycle lock.
///
/// The lock is shared by every fresh launch from verification until its durable
/// record exists and exclusive for a package mutation, so a launch cannot pin a
/// package digest between the retained set being computed and the package
/// being removed.
#[derive(Debug)]
pub struct PackageAuthority {
    pub(super) lifecycle: Arc<RwLock<()>>,
    pub(super) trust: CatalogTrust,
    pub(super) runtimes: RuntimeHost,
}

/// A fresh launch's hold on the lifecycle lock; a package mutation waits for
/// it to drop.
#[derive(Debug)]
#[must_use = "dropping the guard releases the lifecycle lock"]
pub struct LaunchGuard<'a>(
    #[expect(dead_code, reason = "held for its drop")] RwLockReadGuard<'a, ()>,
);

/// A launch's hold on the lifecycle lock that a detached task can own.
#[derive(Debug)]
#[must_use = "dropping the guard releases the lifecycle lock"]
pub struct OwnedLaunchGuard(
    #[expect(dead_code, reason = "held for its drop")] OwnedRwLockReadGuard<()>,
);

/// The authority paired with the session side it works against: the value the
/// `package.*` methods run on.
#[derive(Debug, Clone)]
pub struct PackageService {
    pub(super) authority: Arc<PackageAuthority>,
    pub(super) sessions: SessionRegistry,
}

impl PackageAuthority {
    /// The authority over `runtimes`, authorizing official packages by `trust`.
    #[must_use]
    pub fn new(trust: CatalogTrust, runtimes: RuntimeHost) -> Self {
        Self {
            lifecycle: Arc::new(RwLock::new(())),
            trust,
            runtimes,
        }
    }

    /// What the host trusts to authorize official packages.
    #[must_use]
    pub fn catalog_trust(&self) -> &CatalogTrust {
        &self.trust
    }

    /// The lifecycle lock, for scenarios that observe who holds it.
    #[cfg(test)]
    pub(crate) fn lifecycle(&self) -> &Arc<RwLock<()>> {
        &self.lifecycle
    }

    /// The service the `package.*` methods run on, against `sessions`.
    pub(crate) fn service(self: &Arc<Self>, sessions: SessionRegistry) -> PackageService {
        PackageService {
            authority: Arc::clone(self),
            sessions,
        }
    }

    /// Holds the shared lifecycle lock until the returned guard drops, for a
    /// path that resolves a pin itself and persists a record in a detached task.
    pub async fn hold_launch(&self) -> OwnedLaunchGuard {
        OwnedLaunchGuard(Arc::clone(&self.lifecycle).read_owned().await)
    }

    /// Verifies that `definition` may serve a fresh launch and holds the
    /// shared lifecycle guard for as long as the returned value lives.
    ///
    /// # Errors
    ///
    /// Returns the stable protocol error of
    /// [`RuntimeHost::verify_launchable`], or `runtime_package_changed`
    /// when the definition is no longer the selected version of its package.
    pub(crate) async fn guard_launch(
        &self,
        definition: &RuntimeDefinition,
        profiles: &ProfileRegistry,
    ) -> Result<LaunchGuard<'_>, ProtocolError> {
        let guard = self.lifecycle.read().await;
        self.runtimes.verify_launchable(definition)?;
        self.ensure_launch_is_current(definition, profiles).await?;
        Ok(LaunchGuard(guard))
    }

    /// Verifies, under the shared lifecycle guard, that a launch resolved
    /// before the guard was taken still launches the version that serves its
    /// runtime now.
    ///
    /// A package selection commits under the exclusive guard after proving
    /// that no referenced version is incompatible with the selected one, so a
    /// launch that resolved the previously selected version and pinned it
    /// after the selection would reference exactly such a version. A digest
    /// that a host profile pins launches whatever is selected.
    async fn ensure_launch_is_current(
        &self,
        definition: &RuntimeDefinition,
        profiles: &ProfileRegistry,
    ) -> Result<(), ProtocolError> {
        let BindingProvenance::Package {
            package,
            package_digest,
        } = &definition.binding().provenance
        else {
            return Ok(());
        };
        let Some(store) = self.runtimes.package_store() else {
            return Ok(());
        };
        let incompatible = || ProtocolError::runtime_incompatible(definition.runtime_id());
        let state = store.registry().state().map_err(|_error| incompatible())?;
        if state.selected(&package.id) == Some(package_digest) {
            return Ok(());
        }
        let profiles = profiles.clone();
        let pinned = tokio::task::spawn_blocking(move || profiles.pinned_digests())
            .await
            .map_err(|_error| incompatible())?
            .map_err(|_error| incompatible())?;
        if pinned.contains(package_digest) {
            Ok(())
        } else {
            Err(ProtocolError::runtime_package_changed(
                definition.runtime_id(),
            ))
        }
    }
}
