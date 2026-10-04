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
//! Reads take no guard: the registry record is replaced atomically, so a read
//! sees one committed state.

// Rust guideline compliant 2026-10-04

mod bind;
mod error;
mod fault;
mod source;
mod trust;

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
    PackageSetEnabledParams, PackageTrust, PackageUninstallParams, PackageUninstallResult,
};
use tracing::warn;

use self::error::{registry_kind, rejection_kind};
use self::fault::fault_of;
use self::source::{build_directory, read_archive_file, read_regular_file};
pub use self::trust::HostTrustAnchor;
use super::packages::PackageUninstallError;
use super::SessionRegistry;
use crate::agent::host::{
    decide_claim, definition_from_archive, is_reserved, Authority, ClaimRefusal, PackageRejection,
    PackageReport, PackageStore, RuntimeDefinition, RuntimeHost,
};
use crate::agent::{NativeArg, NativeArgs, NativeSessionLaunch, REFERENCE_PLACEHOLDER};

/// What a package method needs from the session registry.
struct Context {
    store: PackageStore,
    runtimes: RuntimeHost,
    anchor: Option<HostTrustAnchor>,
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
                if let Some(fault) = info.fault {
                    findings.push(PackageFinding {
                        kind: PackageFindingKind::Fault,
                        digest: info.digest,
                        package: Some(info.package),
                        fault: Some(fault),
                        referenced: info.referenced,
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
                    });
                }
                for digest in retained.iter().filter(|d| state.package(d).is_none()) {
                    findings.push(PackageFinding {
                        kind: PackageFindingKind::PinnedNotInstalled,
                        digest: digest.clone(),
                        package: None,
                        fault: None,
                        referenced: true,
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
        let context = self.package_context()?;
        let registry = self.clone();
        tokio::spawn(async move {
            let _exclusive = registry.inner.package_lifecycle.write().await;
            let retained = registry.package_retained().await?;
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
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] naming the first failed check.
    pub async fn package_select(
        &self,
        params: PackageSelectParams,
    ) -> Result<PackageChangeResult, PackageErrorKind> {
        self.package_transaction(move |context, retained| {
            prove_loadable(context, &params.digest)?;
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
            anchor: self.inner.config.catalog_trust_anchor.clone(),
        })
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
    let (runtime, fault) = match context.store.read_definition(digest, record.identity()) {
        Ok(definition) => {
            let fault = if is_reserved(definition.runtime_id()) {
                fault_of(&PackageRejection::ReservedRuntimeId)
            } else if report.rejected.iter().any(|rejected| {
                &rejected.digest == digest
                    && matches!(rejected.reason, PackageRejection::RuntimeIdConflict)
            }) {
                fault_of(&PackageRejection::RuntimeIdConflict)
            } else {
                None
            };
            (Some(runtime_info(&definition)), fault)
        }
        Err(rejection) => (None, fault_of(&rejection)),
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
        },
        runtime,
    }
}

/// Applies the runtime-id claim rules to `definition` against the runtimes
/// served right now.
///
/// The runtime registry reserves the shell and the official aliases for
/// built-in runtimes and its loader refuses them for packages, so an id the
/// policy would allow an official package is still a conflict while the
/// registry reserves it.
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
    })?;
    if is_reserved(runtime_id) {
        return Err(PackageErrorKind::RuntimeConflict);
    }
    Ok(())
}

/// Proves that the installed package `digest` still loads and may serve its
/// runtime id, so enabling or selecting it cannot create a conflict.
fn prove_loadable(context: &Context, digest: &PackageDigest) -> Result<(), PackageErrorKind> {
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
    )
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
        PackageTrust::Catalog { .. } => Some(
            context
                .anchor
                .as_ref()
                .ok_or(PackageErrorKind::TrustUnavailable)?,
        ),
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
    if plan.dry_run {
        return Ok(PackageInstallResult {
            status: PackageInstallStatus::Preview,
            package: PackageInfo {
                digest: prepared.digest.clone(),
                package: prepared.identity,
                origin: origin_of(prepared.source),
                enabled: plan.enable,
                selected: plan.select,
                installed_at_unix_seconds: now,
                runtime_id: Some(runtime.runtime_id.clone()),
                fault: None,
                referenced: retained.contains(&prepared.digest),
            },
            runtime,
            reloaded: false,
        });
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
            select: plan.select,
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
mod test_fixture;
#[cfg(test)]
mod tests;
