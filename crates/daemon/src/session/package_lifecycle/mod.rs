//! The runtime package lifecycle served by the `package.*` methods.
//!
//! Every mutation validates first, records second and reloads third, all under
//! the exclusive package lifecycle guard so a fresh launch can never interleave:
//!
//! 1. the archive is verified and its `runtime.toml` parsed from the archive
//!    entries in memory, so nothing is extracted or recorded for a package the
//!    daemon would refuse to load;
//! 2. trust and the runtime-id claim rules are decided from the descriptor, not
//!    from anything the caller says about the package;
//! 3. the registry records the change and the runtime registry is rebuilt.
//!
//! A package whose integration is not compatible with a version that something
//! still references is installed but never selected ([`selection`]).
//!
//! Reads take no guard: the registry record is replaced atomically, so a read
//! sees one committed state.

// Rust guideline compliant 2026-10-05

mod bind;
mod error;
mod fault;
mod selection;
mod source;
mod trust;

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use package::catalog_state::CatalogState;
use package::registry::{
    InstallRequest, InstallStatus, PackageRecord, PackageSource as RecordSource, RegistryError,
    RegistryState, RetainedDigests,
};
use package::{
    read_archive, read_archive_with_digest, Limits, LocalTrust, LocalTrustError, PackageDigest,
    VerifiedCatalog, MAX_CATALOG_BYTES,
};
use protocol::{
    BindingProvenance, PackageChangeResult, PackageDoctorResult, PackageErrorKind, PackageFinding,
    PackageFindingKind, PackageId, PackageIdentity, PackageInfo, PackageInspectResult,
    PackageInstallParams, PackageInstallResult, PackageInstallStatus, PackageLinkParams,
    PackageListResult, PackageOrigin, PackageRuntimeInfo, PackageSelectParams,
    PackageSelectionBlock, PackageSetEnabledParams, PackageTrust, PackageUninstallParams,
    PackageUninstallResult, ProtocolError,
};
use tracing::warn;

use self::error::{registry_kind, rejection_kind};
use self::fault::fault_of;
use self::selection::{block_of, conflict, declared_by, Integrations};
use self::source::{build_directory, read_archive_file, read_regular_file};
pub use self::trust::HostTrustAnchor;
use super::packages::PackageUninstallError;
use super::SessionRegistry;
use crate::agent::host::{
    decide_claim, definition_from_archive, is_reserved, may_serve_reserved, Authority,
    ClaimRefusal, PackageRejection, PackageReport, PackageStore, RuntimeDefinition, RuntimeHost,
};
use crate::agent::{NativeArg, NativeArgs, NativeSessionLaunch, REFERENCE_PLACEHOLDER};
use crate::catalog_anchor::CatalogTrust;
use crate::integration::{ConfigHomes, RetainedSchemas};

/// What a package method needs from the session registry.
struct Context {
    store: PackageStore,
    runtimes: RuntimeHost,
    trust: CatalogTrust,
    /// Integrations read from installed packages during this request.
    integrations: Mutex<Integrations>,
}

/// An archive that passed every check an install makes before it writes.
struct Prepared {
    bytes: Vec<u8>,
    digest: PackageDigest,
    definition: RuntimeDefinition,
    identity: PackageIdentity,
    authority: Authority,
    source: RecordSource,
    /// The catalog that authorized the package, recorded before the install.
    catalog: Option<VerifiedCatalog>,
}

/// What a caller asked an install to do.
#[derive(Debug, Clone, Copy)]
struct InstallPlan {
    enable: bool,
    select: bool,
    dry_run: bool,
}

/// One installed package with its health.
struct Inspected {
    info: PackageInfo,
    runtime: Option<PackageRuntimeInfo>,
}

impl Context {
    /// The host trust anchor, or why official packages cannot be authorized.
    fn catalog_anchor(&self) -> Result<&HostTrustAnchor, PackageErrorKind> {
        match &self.trust {
            CatalogTrust::Loaded(anchor) => Ok(anchor),
            CatalogTrust::Absent => Err(PackageErrorKind::TrustUnavailable),
            CatalogTrust::Invalid(_fault) => Err(PackageErrorKind::TrustAnchorInvalid),
        }
    }
}

impl SessionRegistry {
    /// Lists every installed package with its health.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] of a registry that cannot be read.
    pub async fn package_list(&self) -> Result<PackageListResult, PackageErrorKind> {
        let context = self.package_context()?;
        let retained = self.package_retained().await?;
        blocking(move || {
            let state = read_state(&context)?;
            let report = context.runtimes.package_report();
            let packages = state
                .packages()
                .iter()
                .map(|record| inspect_record(&context, &state, record, &retained, &report).info)
                .collect();
            Ok(PackageListResult {
                generation: state.generation(),
                packages,
            })
        })
        .await
    }

    /// Describes one installed package.
    ///
    /// # Errors
    ///
    /// Returns [`PackageErrorKind::NotInstalled`] for an unknown digest.
    pub async fn package_inspect(
        &self,
        digest: PackageDigest,
    ) -> Result<PackageInspectResult, PackageErrorKind> {
        let context = self.package_context()?;
        let retained = self.package_retained().await?;
        blocking(move || {
            let state = read_state(&context)?;
            let record = state
                .package(&digest)
                .ok_or(PackageErrorKind::NotInstalled)?;
            let report = context.runtimes.package_report();
            let inspected = inspect_record(&context, &state, record, &retained, &report);
            Ok(PackageInspectResult {
                package: inspected.info,
                runtime: inspected.runtime,
            })
        })
        .await
    }

    /// Reports every problem of the installed packages, the package roots on
    /// disk without a record and the digests sessions pin without a package.
    ///
    /// With `filter`, only the faults of the versions of that package id are
    /// reported: unregistered roots and unknown pins carry no package id.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] of a registry that cannot be read.
    pub async fn package_doctor(
        &self,
        filter: Option<PackageId>,
    ) -> Result<PackageDoctorResult, PackageErrorKind> {
        let context = self.package_context()?;
        let retained = self.package_retained().await?;
        blocking(move || {
            let state = read_state(&context)?;
            let report = context.runtimes.package_report();
            let mut findings = Vec::new();
            for record in state.packages() {
                if filter
                    .as_ref()
                    .is_some_and(|id| id != &record.identity().id)
                {
                    continue;
                }
                let info = inspect_record(&context, &state, record, &retained, &report).info;
                if let Some(block) = info.selection_blocked {
                    findings.push(PackageFinding {
                        kind: PackageFindingKind::SelectionBlocked,
                        digest: info.digest.clone(),
                        package: Some(info.package.clone()),
                        fault: None,
                        referenced: info.referenced,
                        blocked_by: Some(block.retained),
                    });
                }
                if let Some(fault) = info.fault {
                    findings.push(PackageFinding {
                        kind: PackageFindingKind::Fault,
                        digest: info.digest,
                        package: Some(info.package),
                        fault: Some(fault),
                        referenced: info.referenced,
                        blocked_by: None,
                    });
                }
            }
            if filter.is_none() {
                let unregistered = context
                    .store
                    .registry()
                    .unregistered_roots()
                    .map_err(|error| registry_kind(&error))?;
                for digest in unregistered {
                    let referenced = retained.contains(&digest);
                    findings.push(PackageFinding {
                        kind: PackageFindingKind::UnregisteredRoot,
                        digest,
                        package: None,
                        fault: None,
                        referenced,
                        blocked_by: None,
                    });
                }
                for digest in retained.iter().filter(|d| state.package(d).is_none()) {
                    findings.push(PackageFinding {
                        kind: PackageFindingKind::PinnedNotInstalled,
                        digest: digest.clone(),
                        package: None,
                        fault: None,
                        referenced: true,
                        blocked_by: None,
                    });
                }
            }
            Ok(PackageDoctorResult {
                generation: state.generation(),
                findings,
            })
        })
        .await
    }

    /// Runs one package mutation in a task of its own under the exclusive
    /// lifecycle authority.
    ///
    /// The task computes the retained set, runs the blocking work and
    /// reloads, so a caller that stops waiting cannot release the authority
    /// while the change is still in flight.
    async fn package_transaction<T, F>(&self, work: F) -> Result<T, PackageErrorKind>
    where
        T: Send + 'static,
        F: FnOnce(&Context, &RetainedDigests) -> Result<T, PackageErrorKind> + Send + 'static,
    {
        self.package_transaction_excluding(None, work).await
    }

    /// [`Self::package_transaction`] whose retained set leaves out the pin of
    /// the host profile `profile`, because the work replaces that pin.
    async fn package_transaction_excluding<T, F>(
        &self,
        profile: Option<String>,
        work: F,
    ) -> Result<T, PackageErrorKind>
    where
        T: Send + 'static,
        F: FnOnce(&Context, &RetainedDigests) -> Result<T, PackageErrorKind> + Send + 'static,
    {
        let context = self.package_context()?;
        let registry = self.clone();
        tokio::spawn(async move {
            let _exclusive = registry.inner.package_lifecycle.write().await;
            let retained = registry
                .retained_digests_excluding_profile(profile)
                .await
                .map_err(|error| {
                    warn!(%error, "the sessions that pin packages cannot be listed");
                    PackageErrorKind::RegistryFailed
                })?;
            blocking(move || work(&context, &retained)).await
        })
        .await
        .map_err(|_join_error| PackageErrorKind::RegistryFailed)?
    }

    /// Installs a package archive.
    ///
    /// A package already recorded with a verified root is reported as
    /// [`PackageInstallStatus::AlreadyInstalled`] and keeps its recorded
    /// state: `enable`, `select` and the trust only apply to a new record.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] naming the first failed check; on every
    /// error before the registry write nothing was extracted or recorded.
    pub async fn package_install(
        &self,
        params: PackageInstallParams,
    ) -> Result<PackageInstallResult, PackageErrorKind> {
        self.package_transaction(move |context, retained| {
            let prepared = prepare_archive(context, &params.archive_path, &params.trust)?;
            let plan = InstallPlan {
                enable: params.enable,
                select: params.select,
                dry_run: params.dry_run,
            };
            commit_install(context, prepared, plan, retained)
        })
        .await
    }

    /// Copies a developer directory into the package store as a disabled,
    /// unselected package.
    ///
    /// The directory itself is never loaded.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] naming the first failed check.
    pub async fn package_link(
        &self,
        params: PackageLinkParams,
    ) -> Result<PackageInstallResult, PackageErrorKind> {
        self.package_transaction(move |context, retained| {
            let prepared = prepare_directory(&params.directory)?;
            let plan = InstallPlan {
                enable: false,
                select: false,
                dry_run: params.dry_run,
            };
            commit_install(context, prepared, plan, retained)
        })
        .await
    }

    /// Enables or disables a package for fresh launches.
    ///
    /// Enabling proves the package still loads and may serve its runtime id
    /// before anything is recorded.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] naming the first failed check.
    pub async fn package_set_enabled(
        &self,
        params: PackageSetEnabledParams,
    ) -> Result<PackageChangeResult, PackageErrorKind> {
        self.package_transaction(move |context, retained| {
            if params.enabled {
                prove_loadable(context, &params.digest)?;
            } else {
                read_state(context)?
                    .package(&params.digest)
                    .ok_or(PackageErrorKind::NotInstalled)?;
            }
            context
                .store
                .registry()
                .set_enabled(&params.digest, params.enabled)
                .map_err(|error| mutation_kind(context, &error))?;
            reload_and_describe(context, &params.digest, retained)
        })
        .await
    }

    /// Makes a package the one bare requests of its package id resolve to.
    ///
    /// A package whose integration handler or hook schema differs from a
    /// version of the same package that a live, lost or resumable session or a
    /// host profile still references is refused with
    /// [`PackageErrorKind::IntegrationIncompatible`]; it stays installed and the
    /// selection is unchanged.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] naming the first failed check.
    pub async fn package_select(
        &self,
        params: PackageSelectParams,
    ) -> Result<PackageChangeResult, PackageErrorKind> {
        self.package_transaction(move |context, retained| {
            let definition = prove_loadable(context, &params.digest)?;
            ensure_selectable(context, retained, &params.digest, &definition)?;
            context
                .store
                .registry()
                .select(&params.digest)
                .map_err(|error| mutation_kind(context, &error))?;
            reload_and_describe(context, &params.digest, retained)
        })
        .await
    }

    /// Uninstalls a package, or with `remove_modified` removes one whose root
    /// fails verification.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] naming the refusal.
    pub async fn package_uninstall(
        &self,
        params: PackageUninstallParams,
    ) -> Result<PackageUninstallResult, PackageErrorKind> {
        self.remove_package(&params.digest, params.remove_modified)
            .await
            .map_err(|error| match error {
                PackageUninstallError::Registry(error) => registry_kind(&error),
                PackageUninstallError::Reload(error) => {
                    warn!(%error, "the runtime registry was not rebuilt after an uninstall");
                    PackageErrorKind::ReloadFailed
                }
                PackageUninstallError::NoPackageStore | PackageUninstallError::Retention(_) => {
                    PackageErrorKind::RegistryFailed
                }
            })?;
        Ok(PackageUninstallResult {
            digest: params.digest,
            reloaded: true,
        })
    }

    fn package_context(&self) -> Result<Context, PackageErrorKind> {
        let runtimes = self.inner.profiles.runtimes().clone();
        let Some(store) = runtimes.package_store().cloned() else {
            warn!("a package method was called on a host without a package store");
            return Err(PackageErrorKind::RegistryFailed);
        };
        Ok(Context {
            store,
            runtimes,
            trust: self.inner.config.catalog_trust.clone(),
            integrations: Mutex::default(),
        })
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

    /// Runs an integration update under the shared lifecycle guard with the
    /// hook schemas of the package versions that sessions and profile pins
    /// still reference.
    ///
    /// The work runs in a task of its own that owns the guard until the work
    /// ends, so a caller that stops waiting cannot release it while the asset
    /// set is still being written, and a package selection or uninstall cannot
    /// interleave with the activation. A host without a package store has no
    /// package version to retain.
    ///
    /// # Errors
    ///
    /// Returns `package_registry_failed` when the sessions that pin packages
    /// or the registry cannot be read, `integration_install_task_panicked`
    /// when the work panics, and otherwise the error of `work`.
    pub async fn integration_install<T, F>(&self, work: F) -> Result<T, ProtocolError>
    where
        T: Send + 'static,
        F: FnOnce(&RetainedSchemas) -> Result<T, ProtocolError> + Send + 'static,
    {
        let registry = self.clone();
        tokio::spawn(async move {
            let _shared = registry.inner.package_lifecycle.read().await;
            let schemas = if registry.inner.profiles.runtimes().package_store().is_none() {
                RetainedSchemas::default()
            } else {
                let context = registry.package_context()?;
                let retained = registry.package_retained().await?;
                blocking(move || {
                    let state = read_state(&context)?;
                    Ok(selection::retained_schemas(&context, &state, &retained))
                })
                .await?
            };
            tokio::task::spawn_blocking(move || work(&schemas))
                .await
                .map_err(|error| {
                    warn!(%error, "an integration install task failed");
                    ProtocolError::new(
                        protocol::ErrorClass::Daemon,
                        "integration_install_task_panicked",
                        "integration installation task panicked",
                        Some("retry the request; if it repeats, inspect daemon logs".to_owned()),
                    )
                })?
        })
        .await
        .map_err(|_join_error| ProtocolError::from(PackageErrorKind::RegistryFailed))?
    }

    async fn package_retained(&self) -> Result<RetainedDigests, PackageErrorKind> {
        self.retained_package_digests().await.map_err(|error| {
            warn!(%error, "the sessions that pin packages cannot be listed");
            PackageErrorKind::RegistryFailed
        })
    }
}

/// Runs registry and filesystem work off the async runtime.
async fn blocking<T, F>(operation: F) -> Result<T, PackageErrorKind>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PackageErrorKind> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            warn!(%error, "a package task failed");
            PackageErrorKind::RegistryFailed
        })?
}

fn read_state(context: &Context) -> Result<RegistryState, PackageErrorKind> {
    context
        .store
        .registry()
        .state()
        .map_err(|error| registry_kind(&error))
}

fn unix_now() -> Result<u64, PackageErrorKind> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|error| {
            warn!(%error, "the host clock is before the Unix epoch");
            PackageErrorKind::RegistryFailed
        })
}

/// The error kind of a failed registry mutation.
///
/// A commit whose durability is uncertain may have changed the record, so the
/// runtime registry is rebuilt before the error is returned.
fn mutation_kind(context: &Context, error: &RegistryError) -> PackageErrorKind {
    if matches!(
        error,
        RegistryError::CommitUncertain | RegistryError::RemovalIncomplete
    ) {
        if let Err(reload) = context.runtimes.reload() {
            warn!(%reload, "the runtime registry was not rebuilt after an uncertain commit");
        }
    }
    registry_kind(error)
}

fn origin_of(source: RecordSource) -> PackageOrigin {
    match source {
        RecordSource::Official => PackageOrigin::Official,
        RecordSource::Link => PackageOrigin::Link,
        // A source this build does not know is the least trusted origin.
        _ => PackageOrigin::ExplicitDigest,
    }
}

fn authority_of(source: RecordSource) -> Authority {
    match source {
        RecordSource::Official => Authority::Official,
        _ => Authority::Local,
    }
}

/// The tokens of an argument template, with the reference slot spelled as the
/// placeholder the owner reads.
fn template_tokens(args: &NativeArgs) -> Vec<String> {
    args.as_slice()
        .iter()
        .map(|arg| match arg {
            NativeArg::Literal(value) => value.clone(),
            NativeArg::Reference => REFERENCE_PLACEHOLDER.to_owned(),
        })
        .collect()
}

fn runtime_info(definition: &RuntimeDefinition) -> PackageRuntimeInfo {
    PackageRuntimeInfo {
        runtime_id: definition.runtime_id().clone(),
        display_name: definition.display_name().to_owned(),
        program: definition.program().as_str().to_owned(),
        args: definition.default_args().to_vec(),
        launch_args: definition
            .native()
            .and_then(NativeSessionLaunch::assigned)
            .map(|assigned| template_tokens(assigned.launch_args())),
        resume_args: definition
            .native()
            .map(|native| template_tokens(native.resume_args())),
        fork_args: definition
            .native()
            .and_then(NativeSessionLaunch::fork_args)
            .map(template_tokens),
        prompt_argument: definition.prompt_arg(),
        version_probe: definition
            .version_probe_parser()
            .map(|parser| parser.as_str().to_owned()),
        resumable: definition.native().is_some(),
        forkable: definition
            .native()
            .is_some_and(|native| native.fork_args().is_some()),
        integration_handler: definition
            .integration_handler()
            .map(|handler| handler.as_str().to_owned()),
        hook_schema: definition.hook_schema().map(|schema| schema.id.to_owned()),
    }
}

/// Builds the wire description of a recorded package, verifying its root and
/// loading its descriptor to fill the runtime and the fault.
fn inspect_record(
    context: &Context,
    state: &RegistryState,
    record: &PackageRecord,
    retained: &RetainedDigests,
    report: &PackageReport,
) -> Inspected {
    let digest = record.digest();
    let (runtime, fault, selection_blocked) =
        match context.store.read_definition(digest, record.identity()) {
            Ok(definition) => {
                let fault = if is_reserved(definition.runtime_id())
                    && !may_serve_reserved(record, &definition)
                {
                    fault_of(&PackageRejection::ReservedRuntimeId)
                } else if report.rejected.iter().any(|rejected| {
                    &rejected.digest == digest
                        && matches!(rejected.reason, PackageRejection::RuntimeIdConflict)
                }) {
                    fault_of(&PackageRejection::RuntimeIdConflict)
                } else {
                    None
                };
                let blocked = block_of(context, state, retained, record, &declared_by(&definition));
                (Some(runtime_info(&definition)), fault, blocked)
            }
            Err(rejection) => (None, fault_of(&rejection), None),
        };
    Inspected {
        info: PackageInfo {
            digest: digest.clone(),
            package: record.identity().clone(),
            origin: origin_of(record.source()),
            enabled: record.enabled(),
            selected: state.selected(&record.identity().id) == Some(digest),
            installed_at_unix_seconds: record.installed_at_unix_seconds(),
            runtime_id: runtime.as_ref().map(|runtime| runtime.runtime_id.clone()),
            fault,
            referenced: retained.contains(digest),
            selection_blocked,
        },
        runtime,
    }
}

/// Applies the runtime-id claim rules to `definition` against the runtimes
/// served right now.
fn check_claim(
    runtimes: &RuntimeHost,
    definition: &RuntimeDefinition,
    package: &PackageId,
    authority: Authority,
) -> Result<(), PackageErrorKind> {
    let runtime_id = definition.runtime_id();
    decide_claim(
        runtime_id,
        package,
        authority,
        runtimes.served_by(runtime_id).as_ref(),
    )
    .map_err(|refusal| match refusal {
        ClaimRefusal::NotClaimable => PackageErrorKind::RuntimeNotClaimable,
        ClaimRefusal::Conflict => PackageErrorKind::RuntimeConflict,
    })
}

/// Proves that the installed package `digest` still loads and may serve its
/// runtime id, so enabling or selecting it cannot create a conflict, and
/// returns the definition it loaded.
fn prove_loadable(
    context: &Context,
    digest: &PackageDigest,
) -> Result<RuntimeDefinition, PackageErrorKind> {
    let state = read_state(context)?;
    let record = state
        .package(digest)
        .ok_or(PackageErrorKind::NotInstalled)?;
    let definition = context
        .store
        .read_definition(digest, record.identity())
        .map_err(|rejection| {
            warn!(reason = %rejection, "an installed package cannot be loaded");
            rejection_kind(&rejection)
        })?;
    check_claim(
        &context.runtimes,
        &definition,
        &record.identity().id,
        authority_of(record.source()),
    )?;
    Ok(definition)
}

/// Refuses the selection of the installed package `digest` while a retained
/// reference uses a version of the same package with another integration.
///
/// Selecting the package that is already selected changes nothing and is
/// always accepted. Must run under the exclusive lifecycle authority, with
/// `retained` computed under it.
fn ensure_selectable(
    context: &Context,
    retained: &RetainedDigests,
    digest: &PackageDigest,
    definition: &RuntimeDefinition,
) -> Result<(), PackageErrorKind> {
    let state = read_state(context)?;
    let record = state
        .package(digest)
        .ok_or(PackageErrorKind::NotInstalled)?;
    let id = &record.identity().id;
    if state.selected(id) == Some(digest) {
        return Ok(());
    }
    match conflict(
        context,
        &state,
        retained,
        (digest, id),
        &declared_by(definition),
    ) {
        None => Ok(()),
        Some(blocking) => {
            warn!(
                package = %id,
                retained = %blocking,
                "a package is not selected: its integration is not compatible with a retained version"
            );
            Err(PackageErrorKind::IntegrationIncompatible)
        }
    }
}

/// Rebuilds the runtime registry and describes the package as it is now.
fn reload_and_describe(
    context: &Context,
    digest: &PackageDigest,
    retained: &RetainedDigests,
) -> Result<PackageChangeResult, PackageErrorKind> {
    let report = context.runtimes.reload().map_err(|error| {
        warn!(%error, "the runtime registry was not rebuilt after a package change");
        PackageErrorKind::ReloadFailed
    })?;
    let state = read_state(context)?;
    let record = state
        .package(digest)
        .ok_or(PackageErrorKind::NotInstalled)?;
    let info = inspect_record(context, &state, record, retained, &report).info;
    Ok(PackageChangeResult {
        package: info,
        reloaded: true,
    })
}

/// The identity a descriptor declares.
fn declared_identity(definition: &RuntimeDefinition) -> Result<PackageIdentity, PackageErrorKind> {
    match &definition.binding().provenance {
        BindingProvenance::Package { package, .. } => Ok(package.clone()),
        BindingProvenance::Builtin { .. } => Err(PackageErrorKind::DescriptorInvalid),
    }
}

/// Verifies and authorizes an archive file under `trust` without writing.
fn prepare_archive(
    context: &Context,
    path: &str,
    trust: &PackageTrust,
) -> Result<Prepared, PackageErrorKind> {
    let limits = Limits::DEFAULT;
    let anchor = match trust {
        PackageTrust::Catalog { .. } => Some(context.catalog_anchor()?),
        PackageTrust::ExplicitDigest { .. } => None,
    };
    let bytes = read_archive_file(path, &limits)?;
    let archive = match trust {
        PackageTrust::ExplicitDigest { digest } => {
            read_archive_with_digest(&bytes, digest, &limits)
        }
        PackageTrust::Catalog { .. } => read_archive(&bytes, &limits),
    }
    .map_err(|error| {
        warn!(%error, "a package archive was refused");
        PackageErrorKind::ArchiveInvalid
    })?;
    let digest = archive.digest().clone();
    let definition = definition_from_archive(archive.entries(), &digest).map_err(|rejection| {
        warn!(reason = %rejection, "a package descriptor was refused");
        rejection_kind(&rejection)
    })?;
    let identity = declared_identity(&definition)?;

    let (authority, source, catalog) = match (trust, anchor) {
        (PackageTrust::ExplicitDigest { digest: pinned }, _) => {
            LocalTrust::ExplicitDigest(pinned.clone())
                .authorize(definition.runtime_id(), &digest)
                .map_err(|error| match error {
                    LocalTrustError::ReservedRuntime => PackageErrorKind::RuntimeNotClaimable,
                    LocalTrustError::DigestMismatch => PackageErrorKind::ArchiveInvalid,
                })?;
            (Authority::Local, RecordSource::ExplicitDigest, None)
        }
        (PackageTrust::Catalog { catalog_path }, Some(anchor)) => {
            let max = u64::try_from(MAX_CATALOG_BYTES).unwrap_or(u64::MAX);
            let document = read_regular_file(catalog_path, max)
                .map_err(|_failure| PackageErrorKind::SourceUnreadable)?;
            let state: CatalogState = context
                .store
                .registry()
                .catalog_state()
                .map_err(|error| registry_kind(&error))?;
            let verified = trust::verify_catalog_document(&document, anchor, &state, unix_now()?)?;
            trust::authorize_official(
                &verified,
                &identity,
                definition.runtime_id(),
                &digest,
                &trust::core_version()?,
                &trust::host_platform()?,
            )?;
            (Authority::Official, RecordSource::Official, Some(verified))
        }
        (PackageTrust::Catalog { .. }, None) => return Err(PackageErrorKind::TrustUnavailable),
    };
    Ok(Prepared {
        bytes,
        digest,
        definition,
        identity,
        authority,
        source,
        catalog,
    })
}

/// Archives and validates a developer directory.
fn prepare_directory(directory: &str) -> Result<Prepared, PackageErrorKind> {
    let limits = Limits::DEFAULT;
    let bytes = build_directory(directory, &limits)?;
    let archive = read_archive(&bytes, &limits).map_err(|error| {
        warn!(%error, "a linked package directory produced an invalid archive");
        PackageErrorKind::ArchiveInvalid
    })?;
    let digest = archive.digest().clone();
    let definition = definition_from_archive(archive.entries(), &digest).map_err(|rejection| {
        warn!(reason = %rejection, "a linked package descriptor was refused");
        rejection_kind(&rejection)
    })?;
    let identity = declared_identity(&definition)?;
    Ok(Prepared {
        bytes,
        digest,
        definition,
        identity,
        authority: Authority::Local,
        source: RecordSource::Link,
        catalog: None,
    })
}

/// Applies the claim rules, then records the package and rebuilds the runtime
/// registry; a dry run stops after the rules.
fn commit_install(
    context: &Context,
    prepared: Prepared,
    plan: InstallPlan,
    retained: &RetainedDigests,
) -> Result<PackageInstallResult, PackageErrorKind> {
    check_claim(
        &context.runtimes,
        &prepared.definition,
        &prepared.identity.id,
        prepared.authority,
    )?;
    let runtime = runtime_info(&prepared.definition);
    let now = unix_now()?;
    // A package whose integration is incompatible with a retained version is
    // installed without being selected, so the install itself never fails on
    // it and the selected version keeps serving.
    let held_back = conflict(
        context,
        &read_state(context)?,
        retained,
        (&prepared.digest, &prepared.identity.id),
        &declared_by(&prepared.definition),
    );
    let select = plan.select && held_back.is_none();
    if plan.dry_run {
        return Ok(PackageInstallResult {
            status: PackageInstallStatus::Preview,
            package: PackageInfo {
                digest: prepared.digest.clone(),
                package: prepared.identity,
                origin: origin_of(prepared.source),
                enabled: plan.enable,
                selected: select,
                installed_at_unix_seconds: now,
                runtime_id: Some(runtime.runtime_id.clone()),
                fault: None,
                referenced: retained.contains(&prepared.digest),
                selection_blocked: held_back.map(|retained| PackageSelectionBlock {
                    reason: protocol::PackageSelectionBlockReason::IncompatibleWithRetained,
                    retained,
                }),
            },
            runtime,
            reloaded: false,
        });
    }
    if let Some(blocking) = &held_back {
        if plan.select {
            warn!(
                package = %prepared.identity.id,
                retained = %blocking,
                "a package is installed but not selected: its integration is not compatible with a retained version"
            );
        }
    }

    let registry = context.store.registry();
    // The catalog's sequence and revocations are persisted before anything is
    // published: the update only tightens the trust state, so a failure or a
    // crash after it leaves nothing installed under a stale watermark, while
    // the reverse order would leave a package authorized by state that was
    // never recorded.
    if let Some(catalog) = prepared.catalog.as_ref() {
        registry
            .record_catalog(catalog)
            .map_err(|error| registry_kind(&error))?;
    }
    let report = registry
        .install(&InstallRequest {
            archive: &prepared.bytes,
            expected: &prepared.digest,
            identity: prepared.identity.clone(),
            source: prepared.source,
            enabled: plan.enable,
            select,
            installed_at_unix_seconds: now,
        })
        .map_err(|error| mutation_kind(context, &error))?;
    let status = match report.status {
        InstallStatus::Installed => PackageInstallStatus::Installed,
        InstallStatus::AlreadyInstalled => PackageInstallStatus::AlreadyInstalled,
        InstallStatus::RootRestored => PackageInstallStatus::RootRestored,
        _ => return Err(PackageErrorKind::RegistryFailed),
    };
    let reloaded = context.runtimes.reload().map_err(|error| {
        warn!(%error, "the runtime registry was not rebuilt after an install");
        PackageErrorKind::ReloadFailed
    })?;
    if status == PackageInstallStatus::Installed {
        if let Some(rejected) = reloaded
            .rejected
            .iter()
            .find(|rejected| rejected.digest == prepared.digest)
        {
            // Validation makes this unreachable for a package the daemon
            // checked; a root that changed between the check and the load is
            // disabled again rather than left recorded and enabled.
            warn!(reason = %rejected.reason, "a freshly installed package was refused by the loader");
            let kind = rejection_kind(&rejected.reason);
            let _ = registry.set_enabled(&prepared.digest, false);
            let _ = context.runtimes.reload();
            return Err(kind);
        }
    }
    let state = read_state(context)?;
    let record = state
        .package(&prepared.digest)
        .ok_or(PackageErrorKind::NotInstalled)?;
    let inspected = inspect_record(context, &state, record, retained, &reloaded);
    Ok(PackageInstallResult {
        status,
        package: inspected.info,
        runtime,
        reloaded: true,
    })
}

#[cfg(test)]
mod bind_tests;
#[cfg(test)]
mod catalog_tests;
#[cfg(test)]
mod official_alias_tests;
#[cfg(test)]
mod selection_tests;
#[cfg(test)]
mod test_fixture;
#[cfg(test)]
mod tests;
