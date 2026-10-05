//! The config homes an integration request addresses.
//!
//! A runtime's descriptor declares where its agent keeps its configuration
//! ([`ConfigHome`]). The directory a launch would give the agent is that
//! declaration applied to the environment the launch builds: the daemon's base
//! environment overridden by the host profile's own variables. This module is
//! the single place that answers the question, so an install, a status report,
//! a doctor run and a removal all look at the directory the agent will read.
//!
//! A request selects the runtime's own (profile-less) home, the home of one host
//! profile, or every distinct home of the runtime ([`HomeSelection`]). Selectors
//! that resolve to the same canonical directory share one [`Target`], so a home
//! is never installed into twice by one request.

// Rust guideline compliant 2026-10-05

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use pohunek_worker_protocol::BaseEnv;
use protocol::{ErrorClass, IntegrationHome, ProtocolError, RuntimeRef};

use super::handler::{self, Resolved};
use super::{config_dir_invalid, config_path_kind, ConfigPath};
use crate::agent::host::{ConfigHome, HomeError, RuntimeHost};
use crate::agent::{ProfileRegistry, ResolvedAgent};
use crate::runtime::environment::effective_variable;

/// Which config homes of the selected runtime(s) a request addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HomeSelection {
    /// The home of the runtime launched without a profile.
    #[default]
    Runtime,
    /// The home the named host profile launches with.
    Profile(String),
    /// The runtime's own home and the home of each of its host profiles, one
    /// per distinct directory.
    All,
}

impl HomeSelection {
    /// The selection a request's `profile` and `all_profiles` parameters name.
    ///
    /// # Errors
    ///
    /// A `bad_request` error when both are set: one names a single home and
    /// the other every home.
    pub fn from_params(profile: Option<String>, all_profiles: bool) -> Result<Self, ProtocolError> {
        match (profile, all_profiles) {
            (Some(_), true) => Err(ProtocolError::bad_request(
                "profile and all_profiles cannot be combined",
            )),
            (Some(profile), false) => Ok(Self::Profile(profile)),
            (None, true) => Ok(Self::All),
            (None, false) => Ok(Self::Runtime),
        }
    }
}

/// What resolves the config home of a launch: the host profiles and the base
/// environment the daemon hands every agent.
#[derive(Debug, Clone)]
pub struct ConfigHomes {
    profiles: ProfileRegistry,
    base: BaseEnv,
}

impl ConfigHomes {
    /// Resolves homes against `profiles` and the launch base environment
    /// `base`.
    pub(crate) fn new(profiles: ProfileRegistry, base: BaseEnv) -> Self {
        Self { profiles, base }
    }

    /// The runtimes this host serves.
    pub(super) fn host(&self) -> &RuntimeHost {
        self.profiles.runtimes()
    }

    /// The config home a launch of `resolved` with the profile environment
    /// `profile_env` gives the agent.
    ///
    /// The directory is the one the agent is told; it is canonicalized only to
    /// identify a home that several selectors reach, never to replace it.
    fn home_of(
        &self,
        resolved: &Resolved,
        profile_env: &[(String, String)],
    ) -> Result<Home, ProtocolError> {
        let declared = resolved
            .config_home
            .as_ref()
            .ok_or_else(|| undeclared(&resolved.runtime))?;
        let dir = resolve_declared(declared, &|name| {
            effective_variable(&self.base, profile_env, name)
        })?;
        let canonical = std::fs::canonicalize(&dir).ok();
        Ok(Home { dir, canonical })
    }

    /// The runtimes `agent` selects: that runtime, else every runtime that
    /// names a daemon-run handler and declares a config home.
    fn runtimes(&self, agent: Option<&RuntimeRef>) -> Result<Vec<Resolved>, ProtocolError> {
        match agent {
            Some(agent) => Ok(vec![handler::resolve(self.host(), agent)?]),
            None => Ok(handler::managed(self.host())
                .into_iter()
                .filter(|resolved| resolved.config_home.is_some())
                .collect()),
        }
    }

    /// A target for `resolved` launched with the environment of a profile.
    fn target(
        &self,
        resolved: Resolved,
        profile: Option<(&str, &[(String, String)])>,
        labeled: bool,
    ) -> Target {
        let env = profile.map_or(&[][..], |(_name, env)| env);
        let home = self.home_of(&resolved, env);
        let label = labeled.then(|| IntegrationHome {
            profiles: profile
                .map(|(name, _env)| name.to_owned())
                .into_iter()
                .collect(),
            bare: profile.is_none(),
        });
        Target {
            resolved,
            home,
            label,
            scope: profile.map(|(name, _env)| name.to_owned()),
        }
    }

    /// The host profile `name` as a resolved runtime plus its environment.
    ///
    /// # Errors
    ///
    /// `agent_profile_not_found` when no profile file has that name,
    /// `integration_profile_runtime_mismatch` when it extends another runtime
    /// than `agent`, and `agent_not_installable` when its runtime has no
    /// daemon-run handler.
    fn profile(
        &self,
        agent: Option<&RuntimeRef>,
        name: &str,
    ) -> Result<(Resolved, Vec<(String, String)>), ProtocolError> {
        let resolved_agent = self.profiles.resolve_agent(name)?;
        let Some(profile) = resolved_agent.profile.as_ref() else {
            return Err(crate::agent::agent_profile_not_found(name));
        };
        let base = resolved_agent.definition.runtime_id().as_str();
        if let Some(agent) = agent.filter(|agent| agent.as_wire() != base) {
            return Err(ProtocolError::new(
                ErrorClass::Configuration,
                "integration_profile_runtime_mismatch",
                format!(
                    "profile `{name}` extends runtime `{base}`, not `{}`",
                    agent.as_wire()
                ),
                Some(format!(
                    "pass --agent {base}, or a profile of {}",
                    agent.as_wire()
                )),
            ));
        }
        self.host().verify_launchable(&resolved_agent.definition)?;
        let resolved = handler::resolved(&resolved_agent.definition, RuntimeRef::from_wire(base))
            .ok_or_else(|| handler::not_installable(base))?;
        Ok((resolved, profile.env.clone()))
    }

    /// The host profiles that extend `runtime_id`, sorted by name.
    fn profiles_of(&self, runtime_id: &str) -> Vec<ResolvedAgent> {
        self.profiles
            .enumerate()
            .into_iter()
            .filter(|agent| agent.definition.runtime_id().as_str() == runtime_id)
            .collect()
    }

    /// The homes `selection` addresses for the selected runtime(s), one target
    /// per distinct directory.
    ///
    /// A home that cannot be resolved stays a target carrying its error, so a
    /// read-only report can describe it and a removal can name it.
    ///
    /// # Errors
    ///
    /// `agent_not_installable` for a runtime without a daemon-run handler, or
    /// the errors of an unusable profile selection.
    pub(super) fn targets(
        &self,
        agent: Option<&RuntimeRef>,
        selection: &HomeSelection,
    ) -> Result<Vec<Target>, ProtocolError> {
        let targets = match selection {
            HomeSelection::Runtime => self
                .runtimes(agent)?
                .into_iter()
                .map(|resolved| self.target(resolved, None, false))
                .collect(),
            HomeSelection::Profile(name) => {
                let (resolved, env) = self.profile(agent, name)?;
                vec![self.target(resolved, Some((name, &env)), true)]
            }
            HomeSelection::All => {
                let mut targets = Vec::new();
                for resolved in self.runtimes(agent)? {
                    let profiles = self.profiles_of(resolved.runtime_id.as_str());
                    targets.push(self.target(resolved.clone(), None, true));
                    for profile in profiles {
                        let Some(env) = profile.profile.as_ref().map(|launch| launch.env.clone())
                        else {
                            continue;
                        };
                        let runtime = RuntimeRef::from_wire(resolved.runtime_id.as_str());
                        let Some(from_profile) = handler::resolved(&profile.definition, runtime)
                        else {
                            continue;
                        };
                        let mut target =
                            self.target(from_profile, Some((&profile.name, &env)), true);
                        if let Err(error) = self.host().verify_launchable(&profile.definition) {
                            target.home = Err(error);
                        }
                        targets.push(target);
                    }
                }
                targets
            }
        };
        Ok(merge_shared(targets))
    }
}

/// A config directory and its canonical identity.
#[derive(Debug, Clone)]
pub(super) struct Home {
    /// The directory the agent is told, as the environment names it.
    pub(super) dir: PathBuf,
    /// The directory with every symlink resolved, when it exists.
    canonical: Option<PathBuf>,
}

impl Home {
    /// What two homes must share to be the same directory.
    fn identity(&self) -> &Path {
        self.canonical.as_deref().unwrap_or(&self.dir)
    }

    /// Whether the directory itself is a symlink, which no handler writes
    /// through.
    fn is_symlink(&self) -> bool {
        config_path_kind(&self.dir) == ConfigPath::Symlink
    }
}

/// One config home a request acts on.
#[derive(Debug)]
pub(super) struct Target {
    /// The runtime, handler and hook schema that act on the home.
    pub(super) resolved: Resolved,
    /// The resolved directory, or why the launch environment names none.
    pub(super) home: Result<Home, ProtocolError>,
    /// Which selectors reach the home; present only on a profile-aware
    /// request.
    pub(super) label: Option<IntegrationHome>,
    /// The profile a recovery command names to reach `home`; `None` when the
    /// runtime's own home is the one the command reaches.
    scope: Option<String>,
}

impl Target {
    /// The config directory, or the resolution error.
    pub(super) fn dir(&self) -> Result<&Path, ProtocolError> {
        self.dir_ref().map_err(Clone::clone)
    }

    /// The config directory, or a reference to the resolution error.
    pub(super) fn dir_ref(&self) -> Result<&Path, &ProtocolError> {
        self.home.as_ref().map(|home| home.dir.as_path())
    }

    /// The profile a recovery command must name to reach this home, when the
    /// request selected profiles: the selector that owns the directory the
    /// target keeps, so the command never resolves to a directory the handler
    /// refuses.
    pub(super) fn scope_profile(&self) -> Option<&str> {
        self.label.as_ref()?;
        self.scope.as_deref()
    }
}

/// Collapses targets that act on one directory through one handler.
///
/// The first target keeps the attribution; the selectors of the others join its
/// label. When the first target's directory is a symlink the handler refuses,
/// the first one that is not takes over the directory, so the real home is
/// still written.
fn merge_shared(targets: Vec<Target>) -> Vec<Target> {
    let mut merged: Vec<Target> = Vec::new();
    let mut seen: BTreeMap<(&'static str, PathBuf), usize> = BTreeMap::new();
    for target in targets {
        let key = target
            .home
            .as_ref()
            .ok()
            .map(|home| (target.resolved.handler.id(), home.identity().to_path_buf()));
        let Some(key) = key else {
            merged.push(target);
            continue;
        };
        let Some(&index) = seen.get(&key) else {
            seen.insert(key, merged.len());
            merged.push(target);
            continue;
        };
        let kept = &mut merged[index];
        if let (Some(kept_label), Some(label)) = (kept.label.as_mut(), target.label.as_ref()) {
            kept_label.bare |= label.bare;
            kept_label.profiles.extend(label.profiles.iter().cloned());
            kept_label.profiles.sort();
            kept_label.profiles.dedup();
        }
        let kept_is_symlink = kept.home.as_ref().is_ok_and(Home::is_symlink);
        let new_is_symlink = target.home.as_ref().is_ok_and(Home::is_symlink);
        if kept_is_symlink && !new_is_symlink {
            kept.home = target.home;
            kept.scope = target.scope;
        }
    }
    merged
}

/// `agent_config_home_undeclared`: the runtime's descriptor has no
/// `[config_home]`, so its integration has no directory to act on.
fn undeclared(runtime: &RuntimeRef) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Configuration,
        "agent_config_home_undeclared",
        format!(
            "{} declares no [config_home], so its integration has no directory to act on",
            runtime.as_wire()
        ),
        Some("declare [config_home] env and default in the runtime descriptor".to_owned()),
    )
}

/// The config directory `declared` names under `lookup`, as a typed error for
/// a value the agent could not use.
pub(super) fn resolve_declared(
    declared: &ConfigHome,
    lookup: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, ProtocolError> {
    declared.resolve(lookup).map_err(|error| match error {
        HomeError::Unset { .. } => ProtocolError::new(
            ErrorClass::Configuration,
            "missing_env",
            error.to_string(),
            None,
        ),
        HomeError::Invalid { variable } => config_dir_invalid(&variable),
    })
}

/// Entry points of the lifecycle that resolve homes against this test
/// process's environment.
///
/// A test names the config directories with `ProcessEnv`, so the base
/// environment forwards the config-home variables the host's descriptors
/// declare on top of the default allowlist, as a daemon configured to forward
/// them would.
#[cfg(test)]
pub(crate) mod process_environment {
    use pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST;
    use protocol::{
        IntegrationInstallResult, IntegrationStatusParams, IntegrationStatusResult,
        IntegrationUninstallResult,
    };

    use super::{ConfigHomes, HomeSelection};
    use crate::agent::host::RuntimeHost;
    use crate::agent::ProfileRegistry;
    use crate::integration::handler::{self, RetainedSchemas};
    use crate::runtime::environment::{base_environment, EnvironmentSource};
    use protocol::{ProtocolError, RuntimeRef};
    use std::path::PathBuf;

    /// Homes over `host` with no host profiles and a base environment taken
    /// from this process.
    #[must_use]
    pub fn config_homes_for_tests(host: &RuntimeHost) -> ConfigHomes {
        let registry = host.registry();
        let forwarded = registry
            .definitions()
            .filter_map(|definition| definition.config_home())
            .map(|declared| declared.env().to_owned());
        let allowlist: Vec<String> = DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .map(|name| (*name).to_owned())
            .chain(forwarded)
            .collect();
        let base = base_environment(&allowlist, &EnvironmentSource::Process)
            .expect("the test process environment fits a base environment");
        ConfigHomes::new(ProfileRegistry::with_runtimes(None, host.clone()), base)
    }

    /// [`handler::install_in`] for the runtime's own home.
    pub fn install_for_retained(
        host: &RuntimeHost,
        agent: Option<&RuntimeRef>,
        retained: &RetainedSchemas,
    ) -> Result<IntegrationInstallResult, ProtocolError> {
        handler::install_in(
            &config_homes_for_tests(host),
            agent,
            &HomeSelection::Runtime,
            retained,
        )
    }

    /// [`install_for_retained`] with no retained package version.
    pub fn install_for(
        host: &RuntimeHost,
        agent: Option<&RuntimeRef>,
    ) -> Result<IntegrationInstallResult, ProtocolError> {
        install_for_retained(host, agent, &RetainedSchemas::default())
    }

    /// [`handler::status_in`] over `host`.
    pub fn status_for(
        host: &RuntimeHost,
        params: IntegrationStatusParams,
    ) -> Result<IntegrationStatusResult, ProtocolError> {
        handler::status_in(&config_homes_for_tests(host), params)
    }

    /// [`crate::integration::uninstall_in`] for the runtime's own home.
    #[cfg(unix)]
    pub fn uninstall_for(
        host: &RuntimeHost,
        agent: &RuntimeRef,
    ) -> Result<IntegrationUninstallResult, ProtocolError> {
        crate::integration::uninstall_in(
            &config_homes_for_tests(host),
            agent,
            &HomeSelection::Runtime,
        )
    }

    /// The config directory of `runtime` as its descriptor and this process's
    /// environment name it.
    pub(crate) fn runtime_config_dir(runtime: &RuntimeRef) -> Result<PathBuf, ProtocolError> {
        let host = crate::agent::host::fixture::builtin_host();
        let homes = config_homes_for_tests(&host);
        let mut targets = homes.targets(Some(runtime), &HomeSelection::Runtime)?;
        targets
            .pop()
            .expect("a runtime selection has one target")
            .dir()
            .map(std::path::Path::to_path_buf)
    }
}

#[cfg(all(test, unix))]
mod tests;
