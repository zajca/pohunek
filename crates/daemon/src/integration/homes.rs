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

// Rust guideline compliant 2026-10-12

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use pohunek_worker_protocol::BaseEnv;
use protocol::{ErrorClass, IntegrationHome, IntegrationSelector, ProtocolError, RuntimeRef};

use super::handler::{self, Resolved};
use super::{config_dir_invalid, config_path_kind, ConfigPath};
use crate::agent::host::{ConfigHome, HomeError, RuntimeHost, Transcripts};
use crate::agent::ProfileEnv;
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
        Home::resolve(declared, &self.base, profile_env)
    }

    /// The config homes the host's agents launch with, for runtimes that
    /// declare one.
    ///
    /// One entry per runtime's own home (in runtime id order) and per host
    /// profile (in name order), each resolved exactly as a launch resolves it. A home that cannot be
    /// resolved is reported with the profile that names it, never skipped
    /// silently, so a caller decides what a missing home means. Entries are not
    /// deduplicated: profiles that share a directory each appear.
    pub(crate) fn launch_homes(&self) -> Vec<LaunchHome> {
        let mut homes = Vec::new();
        let registry = self.host().registry();
        let mut definitions: Vec<_> = registry.definitions().collect();
        definitions.sort_by(|a, b| a.runtime_id().as_str().cmp(b.runtime_id().as_str()));
        for definition in definitions {
            let Some(declared) = definition.config_home() else {
                continue;
            };
            homes.push(LaunchHome {
                runtime: RuntimeRef::from(definition.runtime_id().clone()),
                profile: None,
                transcripts: definition.transcripts().cloned(),
                home: Home::resolve(declared, &self.base, &[]),
            });
        }
        for agent in self.profiles.enumerate() {
            let (Some(declared), Some(profile)) =
                (agent.definition.config_home(), agent.profile.as_ref())
            else {
                continue;
            };
            homes.push(LaunchHome {
                runtime: RuntimeRef::from(agent.base.clone()),
                profile: Some(agent.name.clone()),
                transcripts: agent.definition.transcripts().cloned(),
                home: Home::resolve(declared, &self.base, &profile.env),
            });
        }
        homes
    }

    /// The runtimes `agent` selects: that runtime, else every runtime that
    /// names a daemon-run handler and declares a config home. With
    /// [`Reach::Existing`] a named official alias nothing serves resolves to
    /// its retired built-in ([`handler::retired`]).
    fn runtimes(
        &self,
        agent: Option<&RuntimeRef>,
        reach: Reach,
    ) -> Result<Vec<Resolved>, ProtocolError> {
        match agent {
            Some(agent) => Ok(vec![match handler::resolve(self.host(), agent) {
                Ok(resolved) => resolved,
                Err(error) => match reach {
                    Reach::Existing => handler::retired(self.host(), agent).ok_or(error)?,
                    Reach::Served => return Err(error),
                },
            }]),
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
            selector: None,
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
    /// daemon-run handler. With [`Reach::Existing`] a profile over an official
    /// alias nothing serves resolves to the alias's retired built-in under the
    /// profile's own environment ([`Self::retired_profile`]); otherwise the
    /// profile's resolution error.
    fn profile(
        &self,
        agent: Option<&RuntimeRef>,
        name: &str,
        reach: Reach,
    ) -> Result<(Resolved, Vec<(String, String)>), ProtocolError> {
        let resolved_agent = match (self.profiles.resolve_agent(name), reach) {
            (Ok(resolved_agent), _) => resolved_agent,
            (Err(error), Reach::Existing) => {
                return self.retired_profile(agent, name)?.ok_or(error);
            }
            (Err(error), Reach::Served) => return Err(error),
        };
        let Some(profile) = resolved_agent.profile.as_ref() else {
            return Err(crate::agent::agent_profile_not_found(name));
        };
        let base = resolved_agent.definition.runtime_id().as_str();
        if let Some(agent) = agent.filter(|agent| agent.as_wire() != base) {
            return Err(profile_runtime_mismatch(name, base, agent));
        }
        self.host().verify_launchable(&resolved_agent.definition)?;
        let resolved = handler::resolved(&resolved_agent.definition, RuntimeRef::from_wire(base))
            .ok_or_else(|| handler::not_installable(base))?;
        Ok((resolved, profile.env.as_slice().to_vec()))
    }

    /// The profile `name` whose base is an official alias nothing serves, as
    /// the alias's retired built-in under the profile's own environment.
    ///
    /// Such a profile never resolves, but the hooks a release that compiled
    /// the built-in in installed into its home stay there. `None` when no
    /// profile file has that name or its base resolves to no retired
    /// built-in.
    ///
    /// # Errors
    ///
    /// The load error of an unsafe or malformed profile file, and
    /// `integration_profile_runtime_mismatch` when its base is not `agent`.
    fn retired_profile(
        &self,
        agent: Option<&RuntimeRef>,
        name: &str,
    ) -> Result<Option<(Resolved, ProfileEnv)>, ProtocolError> {
        let Some((base, env)) = self.profiles.unresolved_base_env(name)? else {
            return Ok(None);
        };
        let Some(resolved) = handler::retired(self.host(), &RuntimeRef::from(base.clone())) else {
            return Ok(None);
        };
        if let Some(agent) = agent.filter(|agent| agent.as_wire() != base.as_str()) {
            return Err(profile_runtime_mismatch(name, base.as_str(), agent));
        }
        Ok(Some((resolved, env)))
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
        self.targets_reaching(agent, selection, Reach::Served)
    }

    /// [`Self::targets`] for inspecting or removing hooks that already exist:
    /// a named official alias nothing serves addresses the config home of its
    /// retired built-in, so the hooks that built-in installed can be reported
    /// and removed without a package.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::targets`].
    pub(super) fn existing_targets(
        &self,
        agent: Option<&RuntimeRef>,
        selection: &HomeSelection,
    ) -> Result<Vec<Target>, ProtocolError> {
        self.targets_reaching(agent, selection, Reach::Existing)
    }

    fn targets_reaching(
        &self,
        agent: Option<&RuntimeRef>,
        selection: &HomeSelection,
        reach: Reach,
    ) -> Result<Vec<Target>, ProtocolError> {
        let targets = match selection {
            HomeSelection::Runtime => self
                .runtimes(agent, reach)?
                .into_iter()
                .map(|resolved| self.target(resolved, None, false))
                .collect(),
            HomeSelection::Profile(name) => {
                let (resolved, env) = self.profile(agent, name, reach)?;
                vec![self.target(resolved, Some((name, &env)), true)]
            }
            HomeSelection::All => {
                let mut targets = Vec::new();
                for resolved in self.runtimes(agent, reach)? {
                    targets.push(self.target(resolved.clone(), None, true));
                    if reach == Reach::Existing
                        && self.host().served_by(&resolved.runtime_id).is_none()
                    {
                        // A retired built-in: none of its profiles resolves,
                        // so each is addressed by its own environment.
                        for (name, env) in
                            self.profiles.unresolved_profiles_on(&resolved.runtime_id)
                        {
                            targets.push(self.target(resolved.clone(), Some((&name, &env)), true));
                        }
                        continue;
                    }
                    let profiles = self.profiles_of(resolved.runtime_id.as_str());
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
        let mut merged = merge_shared(targets);
        for target in &mut merged {
            let selector = target
                .scope
                .as_ref()
                .map_or(IntegrationSelector::Default, |name| {
                    IntegrationSelector::Profile { name: name.clone() }
                });
            if let Some(label) = target.label.as_mut() {
                label.selector = Some(selector);
            }
        }
        Ok(merged)
    }
}

/// `integration_profile_runtime_mismatch`: the profile `name` extends `base`,
/// not the runtime `agent` the request names.
fn profile_runtime_mismatch(name: &str, base: &str, agent: &RuntimeRef) -> ProtocolError {
    ProtocolError::new(
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
    )
}

/// Which runtimes a request may address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Only runtimes the host serves: an install acts for a launchable runtime.
    Served,
    /// Also a named official alias nothing serves, through its retired
    /// built-in: inspection and removal of hooks that already exist.
    Existing,
}

/// One config home a launch gives an agent, and whom it belongs to.
#[derive(Debug)]
pub(crate) struct LaunchHome {
    /// The runtime the agent runs.
    pub(crate) runtime: RuntimeRef,
    /// The host profile that launches the agent; `None` for the runtime's own
    /// home.
    pub(crate) profile: Option<String>,
    /// The transcript tree below the home and its parser, as the runtime's
    /// descriptor declares them; `None` when it declares none.
    pub(crate) transcripts: Option<Transcripts>,
    /// The resolved home, or why the launch environment names none.
    pub(crate) home: Result<Home, ProtocolError>,
}

/// A config directory and its canonical identity.
#[derive(Debug, Clone)]
pub(crate) struct Home {
    /// The directory the agent is told, as the environment names it.
    pub(crate) dir: PathBuf,
    /// The directory with every symlink resolved, when it exists.
    canonical: Option<PathBuf>,
}

impl Home {
    /// The home `declared` names in the environment a launch builds: the
    /// profile's variables over the daemon's base environment.
    ///
    /// # Errors
    ///
    /// `missing_env` when neither the declared variable nor `HOME` is set and a
    /// configuration error when the chosen value is not an absolute UTF-8 path.
    pub(crate) fn resolve(
        declared: &ConfigHome,
        base: &BaseEnv,
        profile_env: &[(String, String)],
    ) -> Result<Self, ProtocolError> {
        let dir = resolve_declared(declared, &|name| {
            effective_variable(base, profile_env, name)
        })?;
        let canonical = std::fs::canonicalize(&dir).ok();
        Ok(Self { dir, canonical })
    }

    /// What two homes must share to be the same directory: the canonical
    /// directory when it exists, else the path with its `.` components
    /// removed; an unresolved path with a `..` is never folded into another.
    pub(crate) fn identity(&self) -> PathBuf {
        self.canonical
            .clone()
            .unwrap_or_else(|| lexically_normalized(&self.dir))
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
            .map(|home| (target.resolved.handler.id(), home.identity()));
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

/// `path` with every `.` component removed, without touching the filesystem.
///
/// A `..` stays: after a missing or symlinked component it does not name the
/// lexical parent, so folding it could give two different directories one
/// identity.
fn lexically_normalized(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| *component != Component::CurDir)
        .collect()
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
        let host = crate::agent::host::fixture::official_host();
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
