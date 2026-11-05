//! The session side of the package lifecycle: the digests live sessions and
//! profiles pin, and the re-derivation of adopted sessions after the runtime
//! registry was rebuilt.
//!
//! The package service calls into it for the retained digests, the adopted-pin
//! refresh and the transaction spawner.

// Rust guideline compliant 2026-10-05

use std::io;
use std::sync::Arc;

use package::registry::RetainedDigests;
use package::PackageDigest;
use protocol::{BindingProvenance, RuntimeRef};

use super::{host, ProtocolError, SessionEntry, SessionId, SessionRegistry};
use crate::agent::host::{LaunchPin, RuntimeDefinition, RuntimeHost};
use crate::agent::{ProfileRegistry, ResolvedAgent};
use crate::detect::DetectorConfig;
use crate::integration::ConfigHomes;
use crate::packages::{installed_digests, LaunchGuard, RetainedPins};
use tracing::warn;

impl SessionRegistry {
    /// Verifies that `definition` may serve a fresh launch and holds the shared
    /// package lifecycle lock for as long as the returned guard lives.
    ///
    /// # Errors
    ///
    /// The errors of [`crate::packages::PackageAuthority::guard_launch`].
    pub(super) async fn guard_package_launch(
        &self,
        definition: &host::RuntimeDefinition,
    ) -> Result<LaunchGuard<'_>, ProtocolError> {
        self.inner
            .packages
            .guard_launch(definition, &self.inner.profiles)
            .await
    }

    /// What the package command service needs from this registry: the package
    /// store, the `PATH` every agent launch sees, and the daemon shutdown
    /// signal.
    ///
    /// The search path is the launch base environment's, so an interpreter a
    /// command program names with `#!/usr/bin/env` resolves as it does for an
    /// agent. A base environment that cannot be formed leaves `search_path`
    /// as an error and every run unavailable.
    pub(crate) fn package_command_host(&self) -> crate::package_command::CommandHost {
        let search_path = self.integration_homes().map(|homes| homes.search_path());
        crate::package_command::CommandHost {
            store: self.inner.profiles.runtimes().package_store().cloned(),
            search_path,
            shutdown: self.inner.daemon_shutdown.clone(),
        }
    }

    /// What resolves the config home a launch of a runtime gives its agent: the
    /// host profiles and the base environment this registry hands every agent.
    ///
    /// A registry without a worker backend launches no agent; its base
    /// environment is the default selection of the daemon's own.
    ///
    /// # Errors
    ///
    /// Returns `worker_initialize_invalid` when the configured allowlist or
    /// the daemon's environment cannot form a base environment.
    pub(crate) fn integration_homes(&self) -> Result<ConfigHomes, ProtocolError> {
        let (allowlist, source) = match self.inner.config.supervision.as_ref() {
            Some(supervision) => (
                supervision.environment_allowlist.clone(),
                supervision.environment_source.clone(),
            ),
            None => (
                pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
                crate::runtime::EnvironmentSource::Process,
            ),
        };
        let base = crate::runtime::environment::base_environment(&allowlist, &source).map_err(
            |error| {
                ProtocolError::new(
                    protocol::ErrorClass::Runtime,
                    "worker_initialize_invalid",
                    error.to_string(),
                    None,
                )
            },
        )?;
        Ok(ConfigHomes::new(self.inner.profiles.clone(), base))
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
    pub(crate) async fn refresh_adopted_pins(&self) {
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
            entry
                .default_detector_config
                .launch_name
                .clone_from(&entry.launch_name);
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
    /// sessions. Only the pins that are known are returned: a record that cannot
    /// be interpreted contributes nothing here, so a gate must use
    /// [`Self::retained_package_pins`], whose gate set covers every installed
    /// digest in that case.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the durable store cannot be read.
    pub async fn retained_package_digests(&self) -> io::Result<RetainedDigests> {
        Ok(self.retained_package_pins().await?.into_known())
    }

    /// The retained digests for display and for gates.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::retained_package_digests`].
    pub(crate) async fn retained_package_pins(&self) -> io::Result<RetainedPins> {
        self.retained_digests_excluding_profile(None).await
    }

    /// [`Self::retained_package_digests`] without the pin of the host profile
    /// `profile`, for a change that replaces that pin.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::retained_package_digests`].
    pub(crate) async fn retained_digests_excluding_profile(
        &self,
        profile: Option<String>,
    ) -> io::Result<RetainedPins> {
        let mut retained = RetainedDigests::new();
        let mut unreadable_lines = 0;
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
            unreadable_lines = pinned.unreadable_lines;
            retained.extend(pinned.digests);
        }
        let sessions = self.inner.sessions.lock().await;
        for entry in sessions.values() {
            retained.extend(pinned_digest(&entry.snapshot.launch_binding));
        }
        drop(sessions);
        let installed = if unreadable_lines > 0 {
            tracing::debug!(
                unreadable_lines,
                "package gates judge every installed package: the metadata store holds a line that may pin a package and cannot be read"
            );
            let runtimes = self.inner.profiles.runtimes().clone();
            tokio::task::spawn_blocking(move || installed_digests(&runtimes))
                .await
                .map_err(|join_error| io::Error::other(join_error.to_string()))??
        } else {
            Vec::new()
        };
        Ok(RetainedPins::with_unreadable(
            retained,
            unreadable_lines,
            installed,
        ))
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
