//! The daemon's shared handle over its runtime registry.
//!
//! One [`RuntimeHost`] is built when the session registry starts and is
//! cloned wherever a runtime identity has to be resolved: agent-name
//! resolution, launch, resume, fork and the mutation guards. It is the only
//! place that turns an [`RuntimeRef`] into a launchable [`RuntimeDefinition`],
//! so every caller answers an unknown runtime with the same stable error.
//!
//! The registry is a snapshot: every clone of a host observes the same current
//! snapshot, and a resolution hands out an owned definition that stays valid
//! after the snapshot is replaced.

use std::sync::{Arc, Mutex, PoisonError, RwLock};

use package::registry::PackageSource as RecordSource;
use package::registry::RegistryError as PackageRegistryError;
use package::PackageDigest;
use protocol::{
    BindingProvenance, LaunchBinding, PackageId, PackageIdentity, ProtocolError, RuntimeId,
    RuntimeRef,
};
use tracing::{debug, warn};

use super::builtin::BuiltinSource;
use super::claim::{is_reserved, ServedBy};
use super::definition::{DefinitionError, LaunchProgram, RuntimeDefinition};
use super::launch::{adopts, adopts_newer, check_pin, LaunchPin};
use super::package::{PackageRejection, PackageSource, PackageStore, RejectedPackage};
use super::registry::{RegistryError, RuntimeRegistry, RuntimeSource, SourceTrust};

/// Cheap-to-clone handle over the daemon's runtime registry.
///
/// Besides the registry it carries the host's shell command. The shell
/// runtime's definition names the host login shell; the configured shell
/// command is host state that only decides what a bare shell session launches,
/// so its text is neither descriptor data nor validated or digested like
/// descriptor arguments, and it can never make the registry fail to build.
#[derive(Debug, Clone)]
pub struct RuntimeHost {
    state: Arc<RwLock<Arc<HostState>>>,
    shell_command: Arc<ShellLaunch>,
    packages: Option<Arc<Packages>>,
}

/// One consistent view of the host: the registry and what the package layer
/// reported while it was built.
#[derive(Debug)]
struct HostState {
    registry: Arc<RuntimeRegistry>,
    report: Arc<PackageReport>,
}

/// What the package layer reported when the current registry was built.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageReport {
    /// Selected, enabled packages that serve no runtime, with the reason.
    pub rejected: Vec<RejectedPackage>,
    /// Set when the package registry record could not be read. The registry
    /// then holds the built-in runtimes only, and package-pinned sessions
    /// resolve as incompatible.
    pub fault: Option<PackageRegistryError>,
}

impl PackageReport {
    /// Whether the official package of `runtime` was left out of the registry
    /// without loading, so it may serve that official runtime id.
    ///
    /// The package is matched by its official package id. A runtime id
    /// conflict leaves out a package whose root loaded, so it does not count:
    /// its content is not what failed.
    #[must_use]
    pub fn official_unavailable(&self, runtime: &RuntimeId) -> bool {
        let id = protocol::official_package_id(runtime);
        self.rejected.iter().any(|rejected| {
            rejected.official
                && rejected.identity.id.as_str() == id
                && !matches!(rejected.reason, PackageRejection::RuntimeIdConflict)
        })
    }
}

/// The sources a reload rebuilds the registry from, and the store pinned
/// sessions resolve their package from.
#[derive(Debug)]
struct Packages {
    builtin: BuiltinSource,
    /// The installed packages; absent for a read-only overlay over no store.
    source: Option<PackageSource>,
    /// Official definitions that serve their runtime id in place of the
    /// installed selected package of that id. Only the read-only upgrade
    /// preflight sets them, to judge against the state the release step
    /// leaves behind before that step runs.
    planned: Vec<RuntimeDefinition>,
    /// Serializes reloads so a slower rebuild never replaces a newer one.
    reload_gate: Mutex<()>,
}

/// Why a reload left the current registry in place.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReloadError {
    /// The host was assembled from definitions directly and has no package
    /// store to reload from.
    #[error("the runtime host has no package store to reload")]
    NotReloadable,
    /// The package registry record could not be read.
    #[error("the package registry could not be read: {0}")]
    Packages(PackageRegistryError),
    /// The rebuilt registry was invalid.
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// Adapts the definitions a package load returned to a registry source of the
/// given trust.
#[derive(Debug)]
struct LoadedPackages(SourceTrust, Vec<RuntimeDefinition>);

impl RuntimeSource for LoadedPackages {
    fn trust(&self) -> SourceTrust {
        self.0
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        Ok(self.1.clone())
    }
}

/// Program and arguments a host-shell runtime launches with.
#[derive(Debug)]
struct ShellLaunch {
    program: String,
    args: Vec<String>,
}

impl RuntimeHost {
    /// Wraps an already built registry; a host-shell runtime launches its
    /// definition's program with no extra arguments.
    ///
    /// A host built this way serves only the definitions it was handed and has
    /// no package store: it cannot reload, and a package-origin definition in
    /// it is served as given because there is no root to verify.
    #[must_use]
    pub fn new(registry: RuntimeRegistry) -> Self {
        Self::from_parts(
            HostState {
                registry: Arc::new(registry),
                report: Arc::default(),
            },
            None,
        )
    }

    fn from_parts(state: HostState, packages: Option<Arc<Packages>>) -> Self {
        let program = state
            .registry
            .definitions()
            .find_map(|definition| match definition.program() {
                LaunchProgram::HostShell(program) => Some(program.clone()),
                LaunchProgram::Fixed(_) => None,
            })
            .unwrap_or_default();
        Self {
            state: Arc::new(RwLock::new(Arc::new(state))),
            shell_command: Arc::new(ShellLaunch {
                program,
                args: Vec::new(),
            }),
            packages,
        }
    }

    /// Builds a host serving the built-in runtimes plus the packages `source`
    /// loads.
    ///
    /// A package registry record that cannot be read does not stop the host:
    /// it serves the built-in runtimes and reports the fault in
    /// [`Self::package_report`].
    ///
    /// # Errors
    ///
    /// Returns the registry error when the built-in runtimes themselves are
    /// invalid.
    pub fn with_packages(
        builtin: BuiltinSource,
        source: PackageSource,
    ) -> Result<Self, RegistryError> {
        Self::with_planned_packages(builtin, Some(source), Vec::new())
    }

    /// Builds a host serving the built-in runtimes, the packages `source`
    /// loads (when there is a store) and `planned` in place of the installed
    /// selected official package of each planned runtime id.
    ///
    /// The planned definitions are taken as already verified: they come from
    /// the release archives the release step verifies before it installs them.
    /// A planned digest that is not installed is accepted by an adoption of a
    /// built-in pin, because the registry is what the adoption resolves. Every
    /// other pin still resolves through the store.
    ///
    /// # Errors
    ///
    /// Returns the registry error when the built-in runtimes or the planned
    /// definitions are invalid.
    pub fn with_planned_packages(
        builtin: BuiltinSource,
        source: Option<PackageSource>,
        planned: Vec<RuntimeDefinition>,
    ) -> Result<Self, RegistryError> {
        let packages = Packages {
            builtin,
            source,
            planned,
            reload_gate: Mutex::new(()),
        };
        let state = build_state(&packages)?;
        Ok(Self::from_parts(state, Some(Arc::new(packages))))
    }

    /// Returns a host whose host-shell runtime launches `program` with `args`.
    #[must_use]
    pub fn with_shell_command(mut self, program: impl Into<String>, args: Vec<String>) -> Self {
        self.shell_command = Arc::new(ShellLaunch {
            program: program.into(),
            args,
        });
        self
    }

    /// The program `definition` launches: the configured shell command's
    /// program for a host-shell runtime, the descriptor's program otherwise.
    #[must_use]
    pub fn launch_program<'a>(&'a self, definition: &'a RuntimeDefinition) -> &'a str {
        match definition.program() {
            LaunchProgram::HostShell(_) => &self.shell_command.program,
            LaunchProgram::Fixed(program) => program,
        }
    }

    /// The launch arguments of `definition`: the configured shell command's
    /// arguments for a host-shell runtime, the descriptor's fixed arguments
    /// otherwise.
    #[must_use]
    pub fn launch_args(&self, definition: &RuntimeDefinition) -> Vec<String> {
        match definition.program() {
            LaunchProgram::HostShell(_) => self.shell_command.args.clone(),
            LaunchProgram::Fixed(_) => definition.default_args().to_vec(),
        }
    }

    /// Builds a host serving the built-in runtimes with the host's login shell.
    ///
    /// # Panics
    ///
    /// Panics when an embedded descriptor is invalid; unit tests parse every
    /// embedded descriptor, so this is a build defect and never a runtime
    /// condition. The login shell is sanitized, so it cannot cause it.
    #[must_use]
    pub fn from_host_environment() -> Self {
        let source = BuiltinSource::from_host_environment();
        let registry = RuntimeRegistry::from_sources(&[&source])
            .expect("embedded built-in runtime descriptors are valid");
        Self::new(registry)
    }

    /// The current registry snapshot.
    #[must_use]
    pub fn registry(&self) -> Arc<RuntimeRegistry> {
        Arc::clone(&self.snapshot().registry)
    }

    /// Resolves a runtime identity.
    ///
    /// # Errors
    ///
    /// Returns `runtime_not_installed` when no definition is registered.
    pub fn resolve_id(
        &self,
        runtime_id: &RuntimeId,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        self.registry().resolve(runtime_id).map(Arc::clone)
    }

    /// Resolves the runtime a [`RuntimeRef`] names.
    ///
    /// # Errors
    ///
    /// Returns `agent_kind_unsupported` for a value that is not a valid runtime
    /// id and `runtime_not_installed` for a valid id no definition backs.
    pub fn resolve_ref(
        &self,
        reference: &RuntimeRef,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        self.registry()
            .resolve(reference.launchable()?)
            .map(Arc::clone)
    }

    /// Who serves `runtime_id` in the current registry, if anyone.
    #[must_use]
    pub fn served_by(&self, runtime_id: &RuntimeId) -> Option<ServedBy> {
        let registry = self.registry();
        let definition = registry.resolve(runtime_id).ok()?;
        Some(match &definition.binding().provenance {
            BindingProvenance::Package { package, .. } => ServedBy::Package(package.id.clone()),
            BindingProvenance::Builtin { .. } => ServedBy::Builtin,
        })
    }

    /// What the package layer reported when the current registry was built.
    #[must_use]
    pub fn package_report(&self) -> Arc<PackageReport> {
        Arc::clone(&self.snapshot().report)
    }

    /// The package store, when the host has one.
    #[must_use]
    pub fn package_store(&self) -> Option<&PackageStore> {
        self.packages
            .as_ref()
            .and_then(|packages| packages.source.as_ref())
            .map(PackageSource::store)
    }

    /// Rebuilds the registry from the built-in runtimes and the packages
    /// installed now, and replaces the current registry with it.
    ///
    /// Reload is explicit: nothing watches the package directory. Every
    /// package root is verified again. When the rebuild cannot be trusted the
    /// current registry stays in place.
    ///
    /// # Errors
    ///
    /// Returns [`ReloadError::NotReloadable`] for a host without a package
    /// store, [`ReloadError::Packages`] when the package registry record
    /// cannot be read, and [`ReloadError::Registry`] when the rebuilt
    /// registry is invalid.
    pub fn reload(&self) -> Result<Arc<PackageReport>, ReloadError> {
        let packages = self.packages.as_ref().ok_or(ReloadError::NotReloadable)?;
        let _gate = packages
            .reload_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let state = build_state(packages)?;
        if let Some(fault) = state.report.fault {
            return Err(ReloadError::Packages(fault));
        }
        let report = Arc::clone(&state.report);
        *self.state.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(state);
        if let Some(store) = self.package_store() {
            store.prune_definitions();
        }
        Ok(report)
    }

    /// Checks that `definition`, taken from the current registry, may serve a
    /// fresh launch or an integration change right now.
    ///
    /// A built-in definition always passes. A package definition passes only
    /// when its package is still recorded under the same identity, enabled,
    /// and its root verifies against its manifest, so a package modified,
    /// disabled or uninstalled after the registry was built never launches.
    ///
    /// # Errors
    ///
    /// Returns `runtime_not_installed` for a disabled or uninstalled package
    /// and `runtime_incompatible` for a root that fails verification or a
    /// registry that cannot be read.
    pub fn verify_launchable(&self, definition: &RuntimeDefinition) -> Result<(), ProtocolError> {
        let BindingProvenance::Package {
            package,
            package_digest,
        } = &definition.binding().provenance
        else {
            return Ok(());
        };
        let Some(store) = self.package_store() else {
            return Ok(());
        };
        store
            .verify_launchable(package_digest, package)
            .map_err(|reason| refuse(definition.runtime_id(), &reason))
    }

    /// Resolves the definition a session frozen with `pin` may launch from.
    ///
    /// A package pin is resolved from exactly its package digest, never from
    /// the registry's current selection: a newer selected version, a disabled
    /// package or a built-in with the same runtime id does not stand in, and
    /// the package root is verified again. Any other pin resolves through the
    /// registry and is checked by [`check_pin`].
    ///
    /// # Errors
    ///
    /// Returns `agent_kind_unsupported` for a value that is not a runtime id,
    /// `runtime_served_by_package` for a built-in or unpinned snapshot of an
    /// official alias a package serves that does not adopt it,
    /// `runtime_builtin_removed` for a built-in or unpinned snapshot of an
    /// official alias nothing serves, `runtime_not_installed` for any other
    /// runtime or package that is not installed or does not match the pin, and
    /// `runtime_incompatible` for a pinned package whose root fails
    /// verification.
    pub fn resolve_pinned(
        &self,
        reference: &RuntimeRef,
        pin: &LaunchPin,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        let runtime_id = reference.launchable()?;
        if let Some(refusal) = self.removed_builtin(runtime_id, pin) {
            return Err(refusal);
        }
        let (
            Some(store),
            Some(LaunchBinding {
                runtime_id: pinned_id,
                provenance:
                    BindingProvenance::Package {
                        package,
                        package_digest,
                    },
            }),
        ) = (self.package_store(), pin.binding())
        else {
            let definition = self.resolve_ref(reference)?;
            check_pin(pin, &definition)?;
            return self.verified_adopter(pin, definition);
        };
        if pinned_id != runtime_id {
            return Err(ProtocolError::runtime_not_installed(runtime_id));
        }
        // The release step installs exactly the planned archive, and the plan
        // verified it when it was made, so the planned definition of this digest
        // is what the pin resolves to once the step ran.
        if let Some(planned) = self.planned_definition(runtime_id, package_digest, package) {
            return Ok(Arc::new(planned));
        }
        store
            .load_pinned(runtime_id, package_digest, package)
            .map_err(|reason| refuse(runtime_id, &reason))
    }

    /// The newer official package a relaunch of a session frozen with the
    /// package `pin` may adopt instead of its exact digest, if there is one.
    ///
    /// The candidate is what the registry serves for the runtime id now (the
    /// planned overlay included), and it must pass [`adopts_newer`]. Both
    /// registry records must come from the official catalog, which is the trust
    /// decision: the frozen digest is recorded with source `official`, and the
    /// candidate is either a planned release package or recorded `official`,
    /// enabled and selected. A package installed by explicit digest or linked
    /// may declare the same package and runtime id, and never adopts.
    /// The frozen package must still load, because the candidate is compared
    /// with it: the same integration handler and hook schema, and the same
    /// native reference existence check. The candidate's root is verified
    /// again. Every other case yields `None`, and the session resolves its exact
    /// digest through [`Self::resolve_pinned`].
    ///
    /// Only a relaunch and the recovery judgements call this; a live session is
    /// observed through its frozen digest. `session list` judges every listed
    /// session, so the reasons a candidate does not adopt are logged at debug
    /// level, and the exact-digest resolution reports its own refusal.
    #[must_use]
    pub fn newer_adopter(
        &self,
        reference: &RuntimeRef,
        pin: &LaunchPin,
    ) -> Option<Arc<RuntimeDefinition>> {
        let store = self.package_store()?;
        let LaunchBinding {
            runtime_id,
            provenance:
                BindingProvenance::Package {
                    package: frozen_package,
                    package_digest: frozen_digest,
                },
        } = pin.binding()?
        else {
            return None;
        };
        if reference.launchable().ok()? != runtime_id {
            return None;
        }
        let candidate = self.resolve_ref(reference).ok()?;
        if !adopts_newer(pin, &candidate) {
            return None;
        }
        let BindingProvenance::Package {
            package,
            package_digest,
        } = &candidate.binding().provenance
        else {
            return None;
        };
        let state = store.registry().state().ok()?;
        let official =
            |record: &package::registry::PackageRecord| record.source() == RecordSource::Official;
        if !state.package(frozen_digest).is_some_and(official) {
            return None;
        }
        // A planned release package counts as official only while the registry
        // holds no record of its digest under another trust source.
        let planned =
            self.is_planned(package_digest) && state.package(package_digest).is_none_or(official);
        let served = planned
            || (state.package(package_digest).is_some_and(|record| {
                official(record) && record.enabled() && record.identity() == package
            }) && state.selected(&package.id) == Some(package_digest));
        if !served {
            return None;
        }
        let frozen = match store.load_pinned(runtime_id, frozen_digest, frozen_package) {
            Ok(frozen) => frozen,
            Err(reason) => {
                debug!(
                    runtime = %runtime_id,
                    reason = %reason,
                    "the frozen package of a session does not load, so a newer version cannot adopt it"
                );
                return None;
            }
        };
        let same_integration = frozen.integration_handler() == candidate.integration_handler()
            && frozen.hook_schema() == candidate.hook_schema()
            && frozen.hook_existence() == candidate.hook_existence();
        if !same_integration {
            debug!(
                runtime = %runtime_id,
                package = %package.id,
                "a newer package version changes the integration or the native reference check, so it does not adopt sessions of the older version"
            );
            return None;
        }
        if planned {
            return Some(candidate);
        }
        match store.load_pinned(runtime_id, package_digest, package) {
            Ok(verified) => Some(verified),
            Err(reason) => {
                debug!(
                    runtime = %runtime_id,
                    reason = %reason,
                    "the selected newer package does not verify, so it does not adopt sessions"
                );
                None
            }
        }
    }

    /// Whether a host profile pinned to `digest` of package `package` names a
    /// strictly newer official version of the official package the session
    /// binding `frozen` was launched from.
    ///
    /// Both registry records must be official and of the same package id. A
    /// release step repins a profile so; a session that the newer version does
    /// not adopt keeps relaunching from its frozen digest under that profile.
    #[must_use]
    pub fn pins_newer_official(
        &self,
        frozen: &LaunchBinding,
        package: &PackageId,
        digest: &PackageDigest,
    ) -> bool {
        let (
            Some(store),
            BindingProvenance::Package {
                package: frozen_package,
                package_digest: frozen_digest,
            },
        ) = (self.package_store(), &frozen.provenance)
        else {
            return false;
        };
        let Ok(state) = store.registry().state() else {
            return false;
        };
        let official =
            |record: &package::registry::PackageRecord| record.source() == RecordSource::Official;
        let Some(launched) = state
            .package(frozen_digest)
            .filter(|record| official(record))
        else {
            return false;
        };
        // A digest the planned release step installs is an official release
        // package the plan verified, unless the registry already holds it under
        // another trust source; an installed one must be recorded official.
        let installed = state.package(digest);
        let pinned = match self.planned_identity(digest) {
            Some(planned) if installed.is_none_or(official) => planned,
            Some(_) => return false,
            None => match installed.filter(|record| official(record)) {
                Some(record) => record.identity().clone(),
                None => return false,
            },
        };
        launched.identity() == frozen_package
            && pinned.id == frozen_package.id
            && *package == frozen_package.id
            && super::launch::version_order(&launched.identity().version, &pinned.version)
                == Some(std::cmp::Ordering::Less)
    }

    /// The package identity of the planned definition of exactly `digest`, when
    /// the planned overlay holds it.
    fn planned_identity(&self, digest: &PackageDigest) -> Option<PackageIdentity> {
        self.packages
            .as_ref()?
            .planned
            .iter()
            .find_map(|definition| match &definition.binding().provenance {
                BindingProvenance::Package {
                    package,
                    package_digest,
                } if package_digest == digest => Some(package.clone()),
                _ => None,
            })
    }

    /// The planned definition of exactly `digest` under `identity` for
    /// `runtime_id`, when the planned overlay holds it.
    fn planned_definition(
        &self,
        runtime_id: &RuntimeId,
        digest: &PackageDigest,
        identity: &PackageIdentity,
    ) -> Option<RuntimeDefinition> {
        self.packages
            .as_ref()?
            .planned
            .iter()
            .find_map(|definition| {
                let matches = definition.runtime_id() == runtime_id
                    && matches!(
                        &definition.binding().provenance,
                        BindingProvenance::Package { package, package_digest }
                            if package == identity && package_digest == digest
                    );
                matches.then(|| definition.clone())
            })
    }

    /// Verifies the package root of a definition that adopts a built-in `pin`.
    ///
    /// The registry is built once, so an adopting package whose root changed or
    /// disappeared afterwards is served from its exact digest again, as a
    /// package pin is. Other definitions are returned as given.
    fn verified_adopter(
        &self,
        pin: &LaunchPin,
        definition: Arc<RuntimeDefinition>,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        let (
            Some(store),
            BindingProvenance::Package {
                package,
                package_digest,
            },
        ) = (self.package_store(), &definition.binding().provenance)
        else {
            return Ok(definition);
        };
        if !adopts(pin, &definition) || self.is_planned(package_digest) {
            return Ok(definition);
        }
        let runtime_id = definition.runtime_id();
        store
            .load_pinned(runtime_id, package_digest, package)
            .map_err(|reason| refuse(runtime_id, &reason))
    }

    /// Resolves the definition a host profile binding `package` at `digest`
    /// launches from.
    ///
    /// The definition comes from exactly that digest, never from the
    /// registry's current selection, so selecting or updating another version
    /// does not change what the profile launches. Enablement is not checked
    /// here: a fresh launch passes [`Self::verify_launchable`], while an
    /// existing session of the profile keeps resolving. A host without a
    /// package store serves its definitions as given, so the registry's
    /// definition must carry the same package id and digest.
    ///
    /// # Errors
    ///
    /// Returns `runtime_not_installed` for a digest that is not installed and
    /// `runtime_incompatible` for a digest of another package or runtime and
    /// for a root that fails verification.
    pub fn resolve_profile_pin(
        &self,
        runtime_id: &RuntimeId,
        package: &PackageId,
        digest: &PackageDigest,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        if let Some(store) = self.package_store() {
            return store
                .load_bound(runtime_id, digest, package)
                .map_err(|reason| refuse(runtime_id, &reason));
        }
        let definition = self.resolve_id(runtime_id)?;
        match &definition.binding().provenance {
            BindingProvenance::Package {
                package: served,
                package_digest,
            } if served.id == *package && package_digest == digest => Ok(definition),
            _ => Err(ProtocolError::runtime_incompatible(runtime_id)),
        }
    }

    /// Checks that a mutation of a live session launched with `pin` may
    /// proceed.
    ///
    /// The session's runtime must resolve in the current registry, or its pin
    /// must name a package the registry still records: disabling a package or
    /// selecting another version retires it for fresh launches only, and a
    /// running session stays controllable. The package root is not verified
    /// here; launching and integration changes verify it.
    ///
    /// # Errors
    ///
    /// Returns `agent_kind_unsupported` for a value that is not a runtime id,
    /// `runtime_builtin_removed` for a built-in or unpinned snapshot of an
    /// official alias nothing serves, and otherwise `runtime_not_installed`
    /// when neither condition holds.
    pub fn ensure_session_runtime(
        &self,
        reference: &RuntimeRef,
        pin: &LaunchPin,
    ) -> Result<(), ProtocolError> {
        let runtime_id = reference.launchable()?;
        if let Some(refusal) = self.removed_builtin(runtime_id, pin) {
            return Err(refusal);
        }
        let resolved = self.resolve_id(runtime_id).map(|_definition| ());
        let (Err(_), Some(store), Some(binding)) = (&resolved, self.package_store(), pin.binding())
        else {
            return resolved;
        };
        match &binding.provenance {
            BindingProvenance::Package {
                package,
                package_digest,
            } if binding.runtime_id == *runtime_id
                && store.is_recorded(package_digest, package) =>
            {
                Ok(())
            }
            _ => resolved,
        }
    }

    /// The definition that describes a session launched with `pin` for
    /// observation: detection rules, input framing and hook schema.
    ///
    /// It is the definition [`Self::resolve_pinned`] resumes the session from,
    /// so a session is observed only through the runtime source its pin
    /// names. A package pin resolves from exactly its digest, verified again.
    /// A built-in pin resolves to a built-in of the same package identity, or
    /// to the official package of that identity that adopts it. A pin this
    /// daemon does not serve (a built-in it does not ship, a built-in an
    /// official package of another identity replaced, or a package digest
    /// that is not installed) is an error, and callers fall back to the
    /// launch-time facts the session persisted and the generic shell manifest,
    /// never to whatever serves the runtime id now.
    ///
    /// # Errors
    ///
    /// Returns the stable protocol error of [`Self::resolve_pinned`].
    pub fn definition_for_pin(
        &self,
        reference: &RuntimeRef,
        pin: &LaunchPin,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        self.resolve_pinned(reference, pin)
    }

    /// The verified definition the launch provenance of `pin` names for a
    /// live session, or `None` when this daemon does not serve that runtime
    /// source (see [`Self::definition_for_pin`]).
    ///
    /// Callbacks, process recognition and detection of the session use it
    /// instead of resolving its runtime id against whatever the registry
    /// serves now; without it, nothing is attributed to the session's own
    /// runtime through the registry.
    #[must_use]
    pub fn pinned_definition(
        &self,
        reference: &RuntimeRef,
        pin: &LaunchPin,
    ) -> Option<Arc<RuntimeDefinition>> {
        self.definition_for_pin(reference, pin).ok()
    }

    /// The hook schema of a session pinned to a package whose descriptor
    /// predates `[integration] hook_schema`, resolved from the handler it
    /// names.
    ///
    /// A live worker of the previous release journals no schema, and its
    /// pinned descriptor no longer parses as a definition. The verified
    /// descriptor's handler resolves to a schema only when exactly one
    /// registered schema is driven by it; every other case yields `None`.
    #[must_use]
    pub fn legacy_hook_schema(
        &self,
        pin: &LaunchPin,
    ) -> Option<&'static pohunek_worker_protocol::HookSchema> {
        let BindingProvenance::Package {
            package,
            package_digest,
        } = &pin.binding()?.provenance
        else {
            return None;
        };
        let handler = self
            .package_store()?
            .read_legacy_integration_handler(package_digest, package)
            .ok()??;
        pohunek_worker_protocol::hook_schema_for_handler(handler.as_str())
    }

    /// Whether `digest` is the archive digest of a planned definition.
    fn is_planned(&self, digest: &PackageDigest) -> bool {
        self.packages.as_ref().is_some_and(|packages| {
            packages.planned.iter().any(|definition| {
                matches!(
                    &definition.binding().provenance,
                    BindingProvenance::Package { package_digest, .. } if package_digest == digest
                )
            })
        })
    }

    /// The refusal of a session launched from the built-in runtime of the
    /// official alias `runtime_id` while nothing serves that alias: this
    /// daemon does not compile the built-in in and no official package is
    /// installed, enabled and selected for it.
    ///
    /// A session was launched from the built-in when its pin names it, or when
    /// it carries no pin, which only a built-in serves (see [`check_pin`]). A
    /// package serving the alias is checked against the pin, so the refusal
    /// only applies while nothing serves the alias.
    fn removed_builtin(&self, runtime_id: &RuntimeId, pin: &LaunchPin) -> Option<ProtocolError> {
        let built_in = match pin {
            LaunchPin::Unpinned => true,
            LaunchPin::Pinned(binding) => {
                binding.runtime_id == *runtime_id
                    && matches!(binding.provenance, BindingProvenance::Builtin { .. })
            }
        };
        // The shell is reserved too, but every daemon compiles it in.
        let removed =
            built_in && is_reserved(runtime_id) && self.registry().resolve(runtime_id).is_err();
        removed.then(|| ProtocolError::runtime_builtin_removed(runtime_id))
    }

    fn snapshot(&self) -> Arc<HostState> {
        Arc::clone(&self.state.read().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Logs the typed cause of a refused package runtime and returns the stable
/// protocol error.
fn refuse(runtime_id: &RuntimeId, reason: &super::package::PackageRejection) -> ProtocolError {
    warn!(
        runtime = %runtime_id,
        reason = %reason,
        "a package runtime was refused"
    );
    reason.to_protocol(runtime_id)
}

/// Builds one registry snapshot from the built-in runtimes and the packages
/// installed now.
fn build_state(packages: &Packages) -> Result<HostState, RegistryError> {
    let loaded = match &packages.source {
        Some(source) => source.load_report(),
        None => Ok(super::package::PackageLoad::default()),
    };
    let (definitions, mut official, report) = match loaded {
        Ok(load) => (
            load.definitions,
            load.official,
            PackageReport {
                rejected: load.rejected,
                fault: None,
            },
        ),
        Err(fault) => (
            Vec::new(),
            Vec::new(),
            PackageReport {
                rejected: Vec::new(),
                fault: Some(fault),
            },
        ),
    };
    if !packages.planned.is_empty() {
        official.retain(|installed| {
            packages
                .planned
                .iter()
                .all(|planned| planned.runtime_id() != installed.runtime_id())
        });
        official.extend(packages.planned.iter().cloned());
    }
    for rejected in &report.rejected {
        warn!(
            package = %rejected.identity.id,
            version = %rejected.identity.version,
            digest = %rejected.digest,
            reason = %rejected.reason,
            "an installed package serves no runtime"
        );
    }
    if let Some(fault) = &report.fault {
        warn!(
            reason = %fault,
            "the package registry cannot be read; serving built-in runtimes only"
        );
    }
    let registry = RuntimeRegistry::from_sources(&[
        &packages.builtin,
        &LoadedPackages(SourceTrust::External, definitions),
        &LoadedPackages(SourceTrust::Official, official),
    ])?;
    Ok(HostState {
        registry: Arc::new(registry),
        report: Arc::new(report),
    })
}

impl Default for RuntimeHost {
    fn default() -> Self {
        Self::from_host_environment()
    }
}
