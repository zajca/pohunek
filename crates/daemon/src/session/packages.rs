//! Runtime package lifecycle on the session registry: explicit reload and
//! retention-checked uninstall.
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
use protocol::{BindingProvenance, RuntimeRef};
use thiserror::Error;

use super::{host, ProtocolError, SessionEntry, SessionId, SessionRegistry};
use crate::agent::host::{LaunchPin, PackageReport, ReloadError, RuntimeDefinition, RuntimeHost};
use crate::agent::{ProfileRegistry, ResolvedAgent};

use crate::detect::DetectorConfig;
use tracing::warn;

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

    /// Re-derives the pinned definition and the running detector configuration
    /// of each live session launched from a built-in runtime an official
    /// package may adopt, after the runtime registry was rebuilt.
    ///
    /// The runtime sources are resolved off the sessions lock, once per pass.
    /// The running configuration is then decided under the lock, from the
    /// session's active agent at that time: the same rule the launch, report
    /// and observation paths use. An active agent that changed while the
    /// sources resolved is collected again, a bounded number of times.
    pub(super) async fn refresh_adopted_pins(&self) {
        let mut only: Option<Vec<SessionId>> = None;
        for _attempt in 0..REFRESH_ATTEMPTS {
            let candidates = self.collect_adopted(only.as_deref()).await;
            if candidates.is_empty() {
                return;
            }
            let runtimes = self.inner.profiles.runtimes().clone();
            let profiles = self.inner.profiles.clone();
            let resolved = match tokio::task::spawn_blocking(move || {
                candidates
                    .into_iter()
                    .map(|candidate| {
                        let resolution = resolve_adopted(&runtimes, &profiles, &candidate);
                        (candidate, resolution)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            {
                Ok(resolved) => resolved,
                Err(error) => {
                    warn!(%error, "the adopted sessions' runtime sources were not resolved");
                    return;
                }
            };
            #[cfg(test)]
            self.hold_refresh().await;
            let stale = self.apply_adopted(resolved).await;
            if stale.is_empty() {
                return;
            }
            only = Some(stale);
        }
        warn!(
            "the running detector of sessions whose active agent kept changing was not refreshed"
        );
    }

    /// The adopted sessions to refresh, optionally only those `only` names.
    async fn collect_adopted(&self, only: Option<&[SessionId]>) -> Vec<AdoptedCandidate> {
        let sessions = self.inner.sessions.lock().await;
        sessions
            .iter()
            .filter(|(id, _)| only.is_none_or(|ids| ids.contains(id)))
            .filter(|(_, entry)| {
                entry
                    .snapshot
                    .launch_binding
                    .binding()
                    .is_some_and(|binding| {
                        matches!(
                            binding.provenance,
                            protocol::BindingProvenance::Builtin {
                                package: Some(_),
                                ..
                            }
                        )
                    })
            })
            .map(|(id, entry)| AdoptedCandidate {
                id: id.clone(),
                agent: entry.info.agent.clone(),
                base: entry.info.agent_base.clone(),
                pin: entry.snapshot.launch_binding.clone(),
                active: active_snapshot(entry),
            })
            .collect()
    }

    /// Applies resolved sources under the lock and returns the sessions whose
    /// active agent changed in a way this resolution does not cover.
    async fn apply_adopted(
        &self,
        resolved: Vec<(AdoptedCandidate, AdoptedResolution)>,
    ) -> Vec<SessionId> {
        let mut sessions = self.inner.sessions.lock().await;
        let mut stale = Vec::new();
        for (candidate, resolution) in resolved {
            let Some(entry) = sessions.get_mut(&candidate.id) else {
                continue;
            };
            // A changed pin is refreshed by the transaction that changed it.
            if entry.snapshot.launch_binding != candidate.pin {
                continue;
            }
            let previous = std::mem::replace(&mut entry.pinned, resolution.pinned);
            rebind_observed_agents(entry, previous.as_ref());
            entry.default_detector_config = resolution.default.clone();
            let current = active_snapshot(entry);
            if current == candidate.active {
                match candidate.active {
                    None => super::send_detector_config(entry, resolution.default),
                    Some(ActiveSnapshot {
                        own_runtime: true, ..
                    }) => {
                        let config = resolution.active.unwrap_or(resolution.default);
                        super::send_detector_config(entry, config);
                    }
                    // A nested agent of another runtime owns its configuration.
                    Some(_) => {}
                }
            } else if current.is_none() {
                // The agent ended: the default is the running configuration.
                super::send_detector_config(entry, resolution.default);
            } else {
                stale.push(candidate.id);
            }
        }
        stale
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
        self.retained_digests_excluding_profile(None).await
    }

    /// [`Self::retained_package_digests`] without the pin of the host profile
    /// `profile`, for a change that replaces that pin.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::retained_package_digests`].
    pub(super) async fn retained_digests_excluding_profile(
        &self,
        profile: Option<String>,
    ) -> io::Result<RetainedDigests> {
        let mut retained = RetainedDigests::new();
        let profiles = self.inner.profiles.clone();
        retained.extend(
            tokio::task::spawn_blocking(move || {
                profiles.pinned_digests_excluding(profile.as_deref())
            })
            .await
            .map_err(|join_error| io::Error::other(join_error.to_string()))??
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
        self.spawn_transaction("package_uninstall", async move {
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
            registry.refresh_adopted_pins().await;
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
    /// [`host::RuntimeHost::verify_launchable`], or `runtime_package_changed`
    /// when the definition is no longer the selected version of its package.
    pub(super) async fn guard_package_launch(
        &self,
        definition: &host::RuntimeDefinition,
    ) -> Result<tokio::sync::RwLockReadGuard<'_, ()>, ProtocolError> {
        let guard = self.inner.package_lifecycle.read().await;
        self.inner
            .profiles
            .runtimes()
            .verify_launchable(definition)?;
        self.ensure_launch_is_current(definition).await?;
        Ok(guard)
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
        definition: &host::RuntimeDefinition,
    ) -> Result<(), ProtocolError> {
        let BindingProvenance::Package {
            package,
            package_digest,
        } = &definition.binding().provenance
        else {
            return Ok(());
        };
        let runtimes = self.inner.profiles.runtimes();
        let Some(store) = runtimes.package_store() else {
            return Ok(());
        };
        let incompatible = || ProtocolError::runtime_incompatible(definition.runtime_id());
        let state = store.registry().state().map_err(|_error| incompatible())?;
        if state.selected(&package.id) == Some(package_digest) {
            return Ok(());
        }
        let profiles = self.inner.profiles.clone();
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

/// The package digest a launch pin froze, if it froze one.
fn pinned_digest(pin: &host::LaunchPin) -> Option<PackageDigest> {
    match pin.binding().map(|binding| &binding.provenance) {
        Some(BindingProvenance::Package { package_digest, .. }) => Some(package_digest.clone()),
        _ => None,
    }
}

/// How many times a refresh collects again the sessions whose active agent
/// changed while the runtime sources resolved.
const REFRESH_ATTEMPTS: usize = 3;

/// The active agent of an adopted session at collection time: its name, whether
/// it runs the session's own runtime, and whether procwatch observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveSnapshot {
    agent: String,
    own_runtime: bool,
    observed: bool,
}

fn active_snapshot(entry: &SessionEntry) -> Option<ActiveSnapshot> {
    entry.active_agent.as_ref().map(|active| ActiveSnapshot {
        agent: active.agent.clone(),
        own_runtime: entry.info.active_agent_base.as_ref() == Some(&entry.info.agent_base),
        observed: active.observed,
    })
}

/// An adopted session collected under the lock, resolved off it.
struct AdoptedCandidate {
    id: SessionId,
    agent: String,
    base: RuntimeRef,
    pin: LaunchPin,
    active: Option<ActiveSnapshot>,
}

/// What an adopted session runs under once its pin is re-derived.
struct AdoptedResolution {
    pinned: Option<Arc<RuntimeDefinition>>,
    /// The session's own default: the profile override, else the pinned manifest.
    default: DetectorConfig,
    /// The own-runtime active agent's configuration: an observed agent runs the
    /// pinned definition as procwatch applies it, a reported one resolves its
    /// name as its report resolves it. `None` when it does not resolve or is not
    /// of the own runtime.
    active: Option<DetectorConfig>,
}

/// Moves the observations the replaced pin `previous` matched onto the
/// session's current pin, so a later transition never applies the replaced
/// definition's rules. Without a current pin the session's own runtime is no
/// longer attributed to it, and those observations are dropped. A changed pin
/// also asks procwatch to observe again.
fn rebind_observed_agents(entry: &mut SessionEntry, previous: Option<&Arc<RuntimeDefinition>>) {
    if super::same_pin(entry.pinned.as_ref(), previous) {
        return;
    }
    entry.procwatch_rescan.request();
    let Some(previous) = previous else {
        return;
    };
    let current = entry.pinned.clone();
    entry.observed_agents.retain_mut(|observed| {
        if !Arc::ptr_eq(&observed.definition.0, previous) {
            return true;
        }
        match &current {
            Some(current) => {
                observed.definition = super::MatchedDefinition(Arc::clone(current));
                true
            }
            None => false,
        }
    });
}

/// Resolves an adopted session once. Loading the definition verifies the
/// package root, so this runs off the async runtime.
fn resolve_adopted(
    runtimes: &RuntimeHost,
    profiles: &ProfileRegistry,
    candidate: &AdoptedCandidate,
) -> AdoptedResolution {
    let definition = runtimes.definition_for_pin(&candidate.base, &candidate.pin);
    let manifest_override = resolve_named(profiles, &candidate.agent, &definition)
        .and_then(|resolved| resolved.profile.and_then(|profile| profile.manifest));
    let default = DetectorConfig::for_resolution(
        manifest_override,
        definition.as_ref().ok().map(Arc::as_ref),
    );
    let active = match &candidate.active {
        Some(ActiveSnapshot {
            own_runtime: true,
            observed: true,
            ..
        }) => definition
            .as_ref()
            .ok()
            .map(|pinned| DetectorConfig::for_definition(pinned)),
        Some(ActiveSnapshot {
            agent,
            own_runtime: true,
            observed: false,
        }) => resolve_named(profiles, agent, &definition)
            .map(|resolved| super::detector_config_for_resolved_agent(&resolved)),
        _ => None,
    };
    AdoptedResolution {
        pinned: definition.ok(),
        default,
        active,
    }
}

/// Resolves `agent` as a report or launch of a session pinned to `definition`
/// resolves it: against the pinned definition when there is one.
fn resolve_named(
    profiles: &ProfileRegistry,
    agent: &str,
    definition: &Result<Arc<RuntimeDefinition>, ProtocolError>,
) -> Option<ResolvedAgent> {
    match definition {
        Ok(pinned) => profiles.resolve_agent_pinned(agent, pinned),
        Err(_unresolved) => profiles.resolve_agent(agent),
    }
    .ok()
}
