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

use package::registry::RegistryError as PackageRegistryError;
use protocol::{BindingProvenance, LaunchBinding, ProtocolError, RuntimeId, RuntimeRef};
use tracing::warn;

use super::builtin::BuiltinSource;
use super::claim::ServedBy;
use super::definition::{DefinitionError, LaunchProgram, RuntimeDefinition};
use super::launch::{check_pin, LaunchPin};
use super::package::{PackageSource, PackageStore, RejectedPackage};
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

/// The sources a reload rebuilds the registry from, and the store pinned
/// sessions resolve their package from.
#[derive(Debug)]
struct Packages {
    builtin: BuiltinSource,
    source: PackageSource,
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

/// Adapts the definitions a package load returned to a registry source.
#[derive(Debug)]
struct LoadedPackages(Vec<RuntimeDefinition>);

impl RuntimeSource for LoadedPackages {
    fn trust(&self) -> SourceTrust {
        SourceTrust::External
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        Ok(self.0.clone())
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
        let packages = Packages {
            builtin,
            source,
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
            .map(|packages| packages.source.store())
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
    /// `runtime_not_installed` for a runtime or package that is not
    /// installed or does not match the pin, and `runtime_incompatible` for a
    /// pinned package whose root fails verification.
    pub fn resolve_pinned(
        &self,
        reference: &RuntimeRef,
        pin: &LaunchPin,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        let runtime_id = reference.launchable()?;
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
            return Ok(definition);
        };
        if pinned_id != runtime_id {
            return Err(ProtocolError::runtime_not_installed(runtime_id));
        }
        store
            .load_pinned(runtime_id, package_digest, package)
            .map(Arc::new)
            .map_err(|reason| refuse(runtime_id, &reason))
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
    /// Returns `agent_kind_unsupported` for a value that is not a runtime id
    /// and `runtime_not_installed` when neither condition holds.
    pub fn ensure_session_runtime(
        &self,
        reference: &RuntimeRef,
        pin: &LaunchPin,
    ) -> Result<(), ProtocolError> {
        let runtime_id = reference.launchable()?;
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
    /// observation: detection rules and input framing.
    ///
    /// A package pin resolves from exactly its digest, verified again, so a
    /// disabled package or another selected version never changes how a
    /// session is observed. Every other pin resolves through the registry.
    ///
    /// # Errors
    ///
    /// Returns the stable protocol error of [`Self::resolve_pinned`] or
    /// [`Self::resolve_ref`].
    pub fn definition_for_pin(
        &self,
        reference: &RuntimeRef,
        pin: &LaunchPin,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        let packaged = matches!(
            pin.binding().map(|binding| &binding.provenance),
            Some(BindingProvenance::Package { .. })
        );
        if packaged && self.package_store().is_some() {
            self.resolve_pinned(reference, pin)
        } else {
            self.resolve_ref(reference)
        }
    }

    /// The verified definition of the package `pin` froze, or `None` for a
    /// built-in or unpinned runtime and for a package that cannot be resolved
    /// from its digest now.
    #[must_use]
    pub fn pinned_package_definition(
        &self,
        reference: &RuntimeRef,
        pin: &LaunchPin,
    ) -> Option<Arc<RuntimeDefinition>> {
        let packaged = matches!(
            pin.binding().map(|binding| &binding.provenance),
            Some(BindingProvenance::Package { .. })
        );
        if packaged {
            self.definition_for_pin(reference, pin).ok()
        } else {
            None
        }
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
    let (definitions, report) = match packages.source.load_report() {
        Ok(load) => (
            load.definitions,
            PackageReport {
                rejected: load.rejected,
                fault: None,
            },
        ),
        Err(fault) => (
            Vec::new(),
            PackageReport {
                rejected: Vec::new(),
                fault: Some(fault),
            },
        ),
    };
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
    let registry =
        RuntimeRegistry::from_sources(&[&packages.builtin, &LoadedPackages(definitions)])?;
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
