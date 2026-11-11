//! The runtime package lifecycle served by the `package.*` methods.
//!
//! Every mutation validates first, records second and reloads third, all under
//! the exclusive package lifecycle guard so a fresh launch can never interleave:
//!
//! 1. the raw archive digest is authorized, by the owner's explicit digest or
//!    by a verified catalog that lists it, before any byte is decompressed;
//!    the archive is then verified and its `runtime.toml` parsed from the
//!    archive entries in memory, so nothing is extracted or recorded for a
//!    package the daemon would refuse to load;
//! 2. trust and the runtime-id claim rules are decided from the descriptor, not
//!    from anything the caller says about the package;
//! 3. the registry records the change and the runtime registry is rebuilt.
//!
//! A package whose integration is not compatible with a version that something
//! still references is installed but never selected ([`selection`]).
//!
//! Reads take no guard: the registry record is replaced atomically, so a read
//! sees one committed state.

// Rust guideline compliant 2026-10-13

mod authority;
mod bind;
pub(crate) mod ensure;
mod error;
mod fault;
mod retention;
mod revocation;
mod selection;
mod source;
mod trust;

pub use self::authority::PackageService;
pub(crate) use self::authority::{LaunchGuard, OwnedLaunchGuard, PackageAuthority};
pub use self::retention::PackageUninstallError;
pub(crate) use self::retention::{installed_digests, RetainedPins};

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use package::catalog_state::CatalogState;
use package::registry::{
    InstallRequest, InstallStatus, PackageRecord, PackageSource as RecordSource, RegistryError,
    RegistryState,
};
use package::revoked_digests::RevokedDigests;
use package::{
    archive_digest, read_archive, read_archive_with_digest, IndexVerifierState, Limits, LocalTrust,
    LocalTrustError, PackageDigest, RootKey, VerifiedCatalog, VerifiedIndex, MAX_CATALOG_BYTES,
    MAX_INDEX_BYTES,
};
use protocol::{
    CommandInfo, PackageChangeResult, PackageDoctorResult, PackageErrorKind, PackageFinding,
    PackageFindingKind, PackageId, PackageIdentity, PackageInfo, PackageInspectResult,
    PackageInstallParams, PackageInstallResult, PackageInstallStatus, PackageLinkParams,
    PackageListResult, PackageOrigin, PackageRuntimeInfo, PackageSelectParams,
    PackageSelectionBlock, PackageSetEnabledParams, PackageTrust, PackageUninstallParams,
    PackageUninstallResult, ProtocolError,
};
use tracing::warn;

use self::error::{registry_kind, rejection_kind};
use self::fault::fault_of;
use self::selection::{block_of, conflict, declared_by, Integration, Integrations};
use self::source::{build_directory, read_archive_file, read_regular_file};
pub use self::trust::HostTrustAnchor;
use crate::agent::host::{
    decide_claim, descriptor_from_archive, is_reserved, may_serve_reserved, Authority,
    ClaimRefusal, ContextDir, ContextError, PackageDescriptor, PackageRejection, PackageReport,
    PackageSource, PackageStore, RuntimeDefinition, RuntimeHost,
};
use crate::agent::{NativeArg, NativeArgs, NativeSessionLaunch, REFERENCE_PLACEHOLDER};
use crate::catalog_anchor::{CatalogTrust, IndexTrust};
use crate::integration::RetainedSchemas;
use crate::session::SessionRegistry;

/// What a package method needs from the authority.
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
    /// The commands the descriptor also exports; empty for a plain runtime
    /// package.
    commands: Vec<CommandInfo>,
    identity: PackageIdentity,
    authority: Authority,
    source: RecordSource,
    /// The catalog that authorized the package, recorded before the install.
    catalog: Option<VerifiedCatalog>,
    /// The plugin index that authorized the package, recorded before the
    /// install.
    index: Option<VerifiedIndex>,
}

/// An archive whose descriptor exports commands only, checked like
/// [`Prepared`].
///
/// It serves no runtime, so the runtime-id claim rules and the signed catalog
/// do not apply to it: it is installed under the owner's exact-digest trust or
/// as a linked directory.
struct CommandsPrepared {
    bytes: Vec<u8>,
    digest: PackageDigest,
    commands: Vec<CommandInfo>,
    identity: PackageIdentity,
    source: RecordSource,
}

/// An archive that passed every check an install makes before it writes, by
/// what its descriptor exports.
enum Candidate {
    /// A runtime package, with or without commands.
    Runtime(Box<Prepared>),
    /// A package that exports commands only.
    Commands(CommandsPrepared),
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
    /// The commands the loaded descriptor exports.
    commands: Vec<CommandInfo>,
    /// Whether the descriptor loaded and exports commands only.
    commands_only: bool,
}

impl PackageAuthority {
    fn context(&self) -> Result<Context, PackageErrorKind> {
        let runtimes = self.runtimes.clone();
        let Some(store) = runtimes.package_store().cloned() else {
            warn!("a package method was called on a host without a package store");
            return Err(PackageErrorKind::RegistryFailed);
        };
        Ok(Context {
            store,
            runtimes,
            trust: self.trust.clone(),
            integrations: Mutex::default(),
        })
    }
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

    /// The host trust anchor and the root that endorses plugin index keys, or
    /// why index installs are unavailable.
    fn index_endorser(&self) -> Result<(&HostTrustAnchor, &RootKey), PackageErrorKind> {
        let anchor = self.catalog_anchor()?;
        match anchor.index() {
            IndexTrust::Loaded(endorser) => Ok((anchor, endorser.as_ref())),
            IndexTrust::Absent => Err(PackageErrorKind::TrustUnavailable),
            IndexTrust::Invalid(fault) => {
                warn!(%fault, "a plugin index install was refused: the channel cannot be trusted");
                Err(PackageErrorKind::TrustAnchorInvalid)
            }
        }
    }
}

impl PackageService {
    /// The verified files of the context package `id`: the selected, enabled
    /// package of that id.
    ///
    /// # Errors
    ///
    /// The [`ContextError`] of the package source, or
    /// [`ContextError::NotInstalled`] on a host without a package store.
    pub async fn context_dir(&self, id: PackageId) -> Result<ContextDir, ContextError> {
        let Some(store) = self.authority.runtimes.package_store().cloned() else {
            return Err(ContextError::NotInstalled);
        };
        tokio::task::spawn_blocking(move || PackageSource::new(store).context_dir(&id))
            .await
            .map_err(|error| {
                warn!(%error, "a context package lookup task failed");
                ContextError::LookupFailed
            })?
    }

    /// The verified files of the context package `id` at `digest`: an
    /// installed, enabled package of that id, selected or not.
    ///
    /// # Errors
    ///
    /// The [`ContextError`] of the package source, or
    /// [`ContextError::NotInstalled`] on a host without a package store.
    pub async fn context_dir_at(
        &self,
        id: PackageId,
        digest: PackageDigest,
    ) -> Result<ContextDir, ContextError> {
        self.authority.context_dir_at(id, digest).await
    }

    /// Lists every installed package with its health.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageErrorKind`] of a registry that cannot be read.
    pub async fn package_list(&self) -> Result<PackageListResult, PackageErrorKind> {
        let context = self.authority.context()?;
        let retained = self.retained().await?;
        blocking(move || {
            let state = read_state(&context)?;
            let report = context.runtimes.package_report();
            let revoked = revocation::revoked_set(&context).ok();
            let packages = state
                .packages()
                .iter()
                .map(|record| {
                    inspect_record(
                        &context,
                        &state,
                        record,
                        &retained,
                        &report,
                        revoked.as_ref(),
                    )
                    .info
                })
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
        let context = self.authority.context()?;
        let retained = self.retained().await?;
        blocking(move || {
            let state = read_state(&context)?;
            let record = state
                .package(&digest)
                .ok_or(PackageErrorKind::NotInstalled)?;
            let report = context.runtimes.package_report();
            let inspected = inspect_one(&context, &state, record, &retained, &report);
            Ok(PackageInspectResult {
                package: inspected.info,
                runtime: inspected.runtime,
                commands: inspected.commands,
                commands_only: inspected.commands_only,
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
    /// Returns the [`PackageErrorKind`] of a registry that cannot be read, and
    /// [`PackageErrorKind::RevocationsUnreadable`] when the file of revoked
    /// digests cannot be.
    pub async fn package_doctor(
        &self,
        filter: Option<PackageId>,
    ) -> Result<PackageDoctorResult, PackageErrorKind> {
        let context = self.authority.context()?;
        let retained = self.retained().await?;
        let mut result = blocking(move || {
            let state = read_state(&context)?;
            let report = context.runtimes.package_report();
            // An unreadable revocation file is a doctor error of its own: it
            // refuses every catalog-authorized package.
            let revoked = revocation::revoked_set(&context)?;
            let mut findings = Vec::new();
            for record in state.packages() {
                if filter
                    .as_ref()
                    .is_some_and(|id| id != &record.identity().id)
                {
                    continue;
                }
                let info =
                    inspect_record(&context, &state, record, &retained, &report, Some(&revoked))
                        .info;
                if let Some(block) = info.selection_blocked {
                    findings.push(PackageFinding {
                        kind: PackageFindingKind::SelectionBlocked,
                        digest: info.digest.clone(),
                        package: Some(info.package.clone()),
                        fault: None,
                        referenced: info.referenced,
                        blocked_by: Some(block.retained),
                        running_sessions: Vec::new(),
                    });
                }
                if info.revoked {
                    findings.push(PackageFinding {
                        kind: PackageFindingKind::Revoked,
                        digest: info.digest.clone(),
                        package: Some(info.package.clone()),
                        fault: None,
                        referenced: info.referenced,
                        blocked_by: None,
                        running_sessions: Vec::new(),
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
                        running_sessions: Vec::new(),
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
                    let referenced = retained.known().contains(&digest);
                    findings.push(PackageFinding {
                        kind: PackageFindingKind::UnregisteredRoot,
                        digest,
                        package: None,
                        fault: None,
                        referenced,
                        blocked_by: None,
                        running_sessions: Vec::new(),
                    });
                }
                for digest in retained
                    .known()
                    .iter()
                    .filter(|d| state.package(d).is_none())
                {
                    findings.push(PackageFinding {
                        kind: PackageFindingKind::PinnedNotInstalled,
                        digest: digest.clone(),
                        package: None,
                        fault: None,
                        referenced: true,
                        blocked_by: None,
                        running_sessions: Vec::new(),
                    });
                }
            }
            Ok(PackageDoctorResult {
                generation: state.generation(),
                findings,
            })
        })
        .await?;
        self.attach_running_sessions(&mut result).await;
        Ok(result)
    }

    /// Names, on each `revoked` finding, the sessions that still run from its
    /// digest.
    async fn attach_running_sessions(&self, result: &mut PackageDoctorResult) {
        let revoked: Vec<PackageDigest> = result
            .findings
            .iter()
            .filter(|finding| finding.kind == PackageFindingKind::Revoked)
            .map(|finding| finding.digest.clone())
            .collect();
        if revoked.is_empty() {
            return;
        }
        let running = self.sessions.running_on_packages(&revoked).await;
        for finding in &mut result.findings {
            if finding.kind == PackageFindingKind::Revoked {
                finding.running_sessions = running
                    .iter()
                    .filter(|(digest, _)| *digest == finding.digest)
                    .map(|(_, id)| id.clone())
                    .collect();
            }
        }
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
        F: FnOnce(&Context, &RetainedPins) -> Result<T, PackageErrorKind> + Send + 'static,
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
        F: FnOnce(&Context, &RetainedPins) -> Result<T, PackageErrorKind> + Send + 'static,
    {
        let context = self.authority.context()?;
        let sessions = self.sessions.clone();
        let authority = Arc::clone(&self.authority);
        self.sessions
            .spawn_transaction("package_transaction", async move {
                let _exclusive = authority.lifecycle.write().await;
                let retained = sessions
                    .retained_digests_excluding_profile(profile)
                    .await
                    .map_err(|error| {
                        warn!(%error, "the sessions that pin packages cannot be listed");
                        PackageErrorKind::RegistryFailed
                    })?;
                let uncertain = retained.is_uncertain();
                let outcome = blocking(move || work(&context, &retained)).await;
                if uncertain && outcome.is_err() {
                    warn_refused_under_uncertainty("a package change");
                }
                // Every transaction may have rebuilt the registry, so live adopted
                // sessions follow whichever package serves their runtime now.
                sessions.refresh_adopted_pins().await;
                outcome
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
            let candidate = prepare_archive(context, &params.archive_path, &params.trust)?;
            require_commands_opt_in(&candidate, params.accept_commands)?;
            let plan = InstallPlan {
                enable: params.enable,
                select: params.select,
                dry_run: params.dry_run,
            };
            commit_candidate(context, candidate, plan, retained)
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
            let candidate = prepare_directory(&params.directory)?;
            require_commands_opt_in(&candidate, params.accept_commands)?;
            let plan = InstallPlan {
                enable: false,
                select: false,
                dry_run: params.dry_run,
            };
            commit_candidate(context, candidate, plan, retained)
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
                revocation::require_not_revoked(context, &read_state(context)?, &params.digest)?;
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
            let integration = prove_loadable(context, &params.digest)?
                .as_ref()
                .map_or(Integration::Known(None), declared_by);
            revocation::require_not_revoked(context, &read_state(context)?, &params.digest)?;
            ensure_selectable(context, retained, &params.digest, &integration)?;
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
                PackageUninstallError::RetentionUncertain => {
                    warn!("a package is not removed: an unreadable metadata-store line may reference it");
                    PackageErrorKind::RetentionUncertain
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
        let sessions = self.sessions.clone();
        let authority = Arc::clone(&self.authority);
        self.sessions
            .spawn_transaction("integration_install", async move {
                let _shared = authority.lifecycle.read().await;
                let mut uncertain = false;
                let schemas = if authority.runtimes.package_store().is_none() {
                    RetainedSchemas::default()
                } else {
                    let context = authority.context()?;
                    let retained = retained(&sessions).await?;
                    uncertain = retained.is_uncertain();
                    blocking(move || {
                        let state = read_state(&context)?;
                        Ok(selection::retained_schemas(
                            &context,
                            &state,
                            retained.gate(),
                        ))
                    })
                    .await?
                };
                let outcome = tokio::task::spawn_blocking(move || work(&schemas))
                    .await
                    .map_err(|error| {
                        warn!(%error, "an integration install task failed");
                        ProtocolError::new(
                            protocol::ErrorClass::Daemon,
                            "integration_install_task_panicked",
                            "integration installation task panicked",
                            Some(
                                "retry the request; if it repeats, inspect daemon logs".to_owned(),
                            ),
                        )
                    })?;
                if uncertain && outcome.is_err() {
                    warn_refused_under_uncertainty("an integration install");
                }
                outcome
            })
            .await
            .map_err(|_join_error| ProtocolError::from(PackageErrorKind::RegistryFailed))?
    }

    async fn retained(&self) -> Result<RetainedPins, PackageErrorKind> {
        retained(&self.sessions).await
    }
}

/// The digests the sessions and profiles pin, as a package error on failure.
async fn retained(sessions: &SessionRegistry) -> Result<RetainedPins, PackageErrorKind> {
    sessions.retained_package_pins().await.map_err(|error| {
        warn!(%error, "the sessions that pin packages cannot be listed");
        PackageErrorKind::RegistryFailed
    })
}

/// Logs that `change` was refused while an unreadable metadata-store line may
/// pin a package.
fn warn_refused_under_uncertainty(change: &str) {
    warn!(
        "{change} was refused while an unreadable metadata-store line may pin a package; see the metadata-store warning for the line and the remedy"
    );
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
        prompt_argument: definition.prompt_arg(),
        version_probe: definition
            .version_probe_parser()
            .map(|parser| parser.as_str().to_owned()),
        resumable: definition.native().is_some(),
        forkable: false,
        integration_handler: definition
            .integration_handler()
            .map(|handler| handler.as_str().to_owned()),
        hook_schema: definition.hook_schema().map(|schema| schema.id.to_owned()),
    }
}

/// Builds the wire description of a recorded package, verifying its root and
/// loading its descriptor to fill the runtime and the fault.
///
/// `revoked` is the set the caller read once for the whole call; `None` (an
/// unreadable file) flags nothing, so a listing stays readable.
fn inspect_record(
    context: &Context,
    state: &RegistryState,
    record: &PackageRecord,
    retained: &RetainedPins,
    report: &PackageReport,
    revoked: Option<&RevokedDigests>,
) -> Inspected {
    let digest = record.digest();
    let mut commands = Vec::new();
    let mut commands_only = false;
    let (runtime, fault, selection_blocked) =
        match context.store.read_descriptor(digest, record.identity()) {
            Ok(descriptor) => {
                commands = descriptor.commands().to_vec();
                commands_only = descriptor.runtime().is_none();
                match descriptor.runtime() {
                    Some(definition) => {
                        let fault = if is_reserved(definition.runtime_id())
                            && !may_serve_reserved(record, definition)
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
                        let blocked = block_of(
                            context,
                            state,
                            retained.gate(),
                            record,
                            &declared_by(definition),
                        );
                        (Some(runtime_info(definition)), fault, blocked)
                    }
                    // A package that exports commands only serves no runtime and
                    // declares no integration: it is healthy and never blocked.
                    None => (None, None, None),
                }
            }
            Err(rejection) => (None, fault_of(&rejection), None),
        };
    let revoked = revoked.is_some_and(|set| revocation::is_revoked(set, record));
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
            referenced: retained.known().contains(digest),
            selection_blocked,
            revoked,
        },
        runtime,
        commands,
        commands_only,
    }
}

/// [`inspect_record`] for a call that describes a single record: it reads the
/// revoked digests itself.
fn inspect_one(
    context: &Context,
    state: &RegistryState,
    record: &PackageRecord,
    retained: &RetainedPins,
    report: &PackageReport,
) -> Inspected {
    let revoked = revocation::revoked_set(context).ok();
    inspect_record(context, state, record, retained, report, revoked.as_ref())
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

/// Proves that the installed package `digest` still loads and, when it
/// exports a runtime, may serve its runtime id, so enabling or selecting it
/// cannot create a conflict, and returns the runtime it loaded.
///
/// A package that exports commands only has no runtime to claim: it returns
/// `None` once its root and descriptor verify.
fn prove_loadable(
    context: &Context,
    digest: &PackageDigest,
) -> Result<Option<RuntimeDefinition>, PackageErrorKind> {
    let state = read_state(context)?;
    let record = state
        .package(digest)
        .ok_or(PackageErrorKind::NotInstalled)?;
    let descriptor = context
        .store
        .read_descriptor(digest, record.identity())
        .map_err(|rejection| {
            warn!(reason = %rejection, "an installed package cannot be loaded");
            rejection_kind(&rejection)
        })?;
    let Some(definition) = descriptor.into_runtime() else {
        return Ok(None);
    };
    check_claim(
        &context.runtimes,
        &definition,
        &record.identity().id,
        authority_of(record.source()),
    )?;
    Ok(Some(definition))
}

/// Refuses the selection of the installed package `digest` while a retained
/// reference uses a version of the same package with another integration.
///
/// Selecting the package that is already selected changes nothing and is
/// always accepted. Must run under the exclusive lifecycle authority, with
/// `retained` computed under it.
fn ensure_selectable(
    context: &Context,
    retained: &RetainedPins,
    digest: &PackageDigest,
    integration: &Integration,
) -> Result<(), PackageErrorKind> {
    let state = read_state(context)?;
    let record = state
        .package(digest)
        .ok_or(PackageErrorKind::NotInstalled)?;
    let id = &record.identity().id;
    if state.selected(id) == Some(digest) {
        return Ok(());
    }
    match conflict(context, &state, retained.gate(), (digest, id), integration) {
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
    retained: &RetainedPins,
) -> Result<PackageChangeResult, PackageErrorKind> {
    let report = context.runtimes.reload().map_err(|error| {
        warn!(%error, "the runtime registry was not rebuilt after a package change");
        PackageErrorKind::ReloadFailed
    })?;
    let state = read_state(context)?;
    let record = state
        .package(digest)
        .ok_or(PackageErrorKind::NotInstalled)?;
    let info = inspect_one(context, &state, record, retained, &report).info;
    Ok(PackageChangeResult {
        package: info,
        reloaded: true,
    })
}

/// Refuses a package that declares commands unless the request opted in.
///
/// A client that does not send `accept_commands` cannot show a command's
/// program, arguments and exposure for consent, and an older client drops the
/// `commands` of the result without a trace. The check runs on the validated
/// archive before any write, so a dry run is refused too.
fn require_commands_opt_in(candidate: &Candidate, accept: bool) -> Result<(), PackageErrorKind> {
    let declares = match candidate {
        Candidate::Runtime(prepared) => !prepared.commands.is_empty(),
        Candidate::Commands(prepared) => !prepared.commands.is_empty(),
    };
    if declares && !accept {
        warn!("a package that declares commands was refused: the request did not opt in");
        return Err(PackageErrorKind::CommandsClientUnsupported);
    }
    Ok(())
}

/// Verifies and authorizes an archive file under `trust` without writing.
///
/// The raw archive digest is authorized before the archive is decompressed,
/// so bytes that neither the owner's digest nor a verified catalog vouches for
/// never reach the zstd decoder.
fn prepare_archive(
    context: &Context,
    path: &str,
    trust: &PackageTrust,
) -> Result<Candidate, PackageErrorKind> {
    match trust {
        PackageTrust::ExplicitDigest { digest } => prepare_local(path, digest),
        PackageTrust::Catalog { catalog_path } => {
            let anchor = context.catalog_anchor()?;
            let state = context
                .store
                .registry()
                .catalog_state()
                .map_err(|error| registry_kind(&error))?;
            let prepared = prepare_official(anchor, &state, path, catalog_path)?;
            // A digest an earlier catalog revoked stays revoked when a later
            // catalog lists it again.
            if revocation::revoked_set(context)?.contains(&prepared.digest) {
                warn!("a package archive was refused: a recorded catalog revoked its digest");
                return Err(PackageErrorKind::Revoked);
            }
            Ok(Candidate::Runtime(Box::new(prepared)))
        }
        PackageTrust::Index { index_path } => {
            let (anchor, endorser) = context.index_endorser()?;
            let registry = context.store.registry();
            let catalog_state = registry
                .catalog_state()
                .map_err(|error| registry_kind(&error))?;
            let index_state = registry
                .index_state()
                .map_err(|error| registry_kind(&error))?;
            let prepared = prepare_indexed(
                &IndexTrustInputs {
                    anchor,
                    endorser,
                    catalog_state: &catalog_state,
                    index_state: &index_state,
                },
                path,
                index_path,
            )?;
            if revocation::revoked_set(context)?.contains(&prepared.digest) {
                warn!("a package archive was refused: a recorded catalog revoked its digest");
                return Err(PackageErrorKind::Revoked);
            }
            Ok(Candidate::Runtime(Box::new(prepared)))
        }
    }
}

/// Prepares an archive the owner trusts by its exact digest `pinned`.
fn prepare_local(path: &str, pinned: &PackageDigest) -> Result<Candidate, PackageErrorKind> {
    let limits = Limits::DEFAULT;
    let bytes = read_archive_file(path, &limits)?;
    let (digest, descriptor) = read_authorized(&bytes, pinned, &limits)?;
    let identity = descriptor.identity().clone();
    let (definition, commands) = descriptor.into_parts();
    let Some(definition) = definition else {
        // The archive was read against `pinned`, so the exact-digest trust is
        // already satisfied; there is no runtime id to check.
        return Ok(Candidate::Commands(CommandsPrepared {
            bytes,
            digest,
            commands,
            identity,
            source: RecordSource::ExplicitDigest,
        }));
    };
    LocalTrust::ExplicitDigest(pinned.clone())
        .authorize(definition.runtime_id(), &digest)
        .map_err(|error| match error {
            LocalTrustError::ReservedRuntime => PackageErrorKind::RuntimeNotClaimable,
            LocalTrustError::DigestMismatch => PackageErrorKind::ArchiveInvalid,
        })?;
    Ok(Candidate::Runtime(Box::new(Prepared {
        bytes,
        digest,
        definition,
        commands,
        identity,
        authority: Authority::Local,
        source: RecordSource::ExplicitDigest,
        catalog: None,
        index: None,
    })))
}

/// Prepares an archive that the signed catalog at `catalog_path` authorizes as
/// official.
///
/// The verified catalog must list the raw archive digest before the archive is
/// decoded; the identity the decoded descriptor declares must then match a
/// listed entry that supports this core version and platform.
fn prepare_official(
    anchor: &HostTrustAnchor,
    state: &CatalogState,
    path: &str,
    catalog_path: &str,
) -> Result<Prepared, PackageErrorKind> {
    let limits = Limits::DEFAULT;
    let bytes = read_archive_file(path, &limits)?;
    let max = u64::try_from(MAX_CATALOG_BYTES).unwrap_or(u64::MAX);
    let document = read_regular_file(catalog_path, max)
        .map_err(|_failure| PackageErrorKind::SourceUnreadable)?;
    let verified = trust::verify_catalog_document(&document, anchor, state, unix_now()?)?;
    let raw = archive_digest(&bytes).map_err(|error| {
        warn!(%error, "a package archive digest cannot be computed");
        PackageErrorKind::ArchiveInvalid
    })?;
    trust::require_listed(&verified, &raw)?;
    let (digest, descriptor) = read_authorized(&bytes, &raw, &limits)?;
    let identity = descriptor.identity().clone();
    let (definition, commands) = descriptor.into_parts();
    // The catalog authorizes a package for the runtime it serves, so a package
    // that exports commands only cannot be catalog-authorized.
    let Some(definition) = definition else {
        warn!(
            "a package that exports commands only was refused: a catalog entry binds a runtime id"
        );
        return Err(PackageErrorKind::Untrusted);
    };
    // The release install paths (`ensure`, `setup host`) have no consent step
    // that shows a command, so an official package declaring commands is not
    // installable until they do.
    if !commands.is_empty() {
        warn!("an official package that declares commands was refused: no consent step shows them");
        return Err(PackageErrorKind::Untrusted);
    }
    trust::authorize_official(
        &verified,
        &identity,
        definition.runtime_id(),
        &digest,
        &trust::core_version()?,
        &trust::host_platform()?,
    )?;
    Ok(Prepared {
        bytes,
        digest,
        definition,
        commands,
        identity,
        authority: Authority::Official,
        source: RecordSource::Official,
        catalog: Some(verified),
        index: None,
    })
}

/// What an index install verifies against.
struct IndexTrustInputs<'a> {
    anchor: &'a HostTrustAnchor,
    /// The anchor root the channel names as the index endorser.
    endorser: &'a RootKey,
    catalog_state: &'a CatalogState,
    index_state: &'a IndexVerifierState,
}

/// Prepares an archive that the signed plugin index at `index_path`
/// authorizes as official.
///
/// The verified index must list the raw archive digest before the archive is
/// decoded; the identity the decoded descriptor declares must then match the
/// listed entry. A package that exports commands, alone or beside a runtime,
/// is refused: the index install paths have no consent step that shows a
/// command.
fn prepare_indexed(
    inputs: &IndexTrustInputs<'_>,
    path: &str,
    index_path: &str,
) -> Result<Prepared, PackageErrorKind> {
    let limits = Limits::DEFAULT;
    let bytes = read_archive_file(path, &limits)?;
    let max = u64::try_from(MAX_INDEX_BYTES).unwrap_or(u64::MAX);
    let document = read_regular_file(index_path, max)
        .map_err(|_failure| PackageErrorKind::SourceUnreadable)?;
    let verified = trust::verify_index_document(
        &document,
        inputs.endorser,
        inputs.anchor,
        inputs.catalog_state,
        inputs.index_state,
        unix_now()?,
    )?;
    let raw = archive_digest(&bytes).map_err(|error| {
        warn!(%error, "a package archive digest cannot be computed");
        PackageErrorKind::ArchiveInvalid
    })?;
    trust::require_indexed(&verified, &raw)?;
    let (digest, descriptor) = read_authorized(&bytes, &raw, &limits)?;
    let identity = descriptor.identity().clone();
    let (definition, commands) = descriptor.into_parts();
    let Some(definition) = definition else {
        warn!(
            "a package that exports commands only was refused: an index entry binds a runtime id"
        );
        return Err(PackageErrorKind::Untrusted);
    };
    if !commands.is_empty() {
        warn!("an indexed package that declares commands was refused: no consent step shows them");
        return Err(PackageErrorKind::Untrusted);
    }
    trust::authorize_indexed(
        &verified,
        &identity,
        definition.runtime_id(),
        &digest,
        &trust::core_version()?,
        &trust::host_platform()?,
    )?;
    Ok(Prepared {
        bytes,
        digest,
        definition,
        commands,
        identity,
        authority: Authority::Official,
        source: RecordSource::Official,
        catalog: None,
        index: Some(verified),
    })
}

/// Reads archive `bytes` whose digest `authorized` a trust source already
/// accepted, and parses the descriptor they carry.
///
/// The digest is compared before decompression, so bytes that differ from the
/// authorized digest are refused without being decoded.
fn read_authorized(
    bytes: &[u8],
    authorized: &PackageDigest,
    limits: &Limits,
) -> Result<(PackageDigest, PackageDescriptor), PackageErrorKind> {
    let archive = read_archive_with_digest(bytes, authorized, limits).map_err(|error| {
        warn!(%error, "a package archive was refused");
        PackageErrorKind::ArchiveInvalid
    })?;
    let digest = archive.digest().clone();
    let descriptor = descriptor_from_archive(archive.entries(), &digest).map_err(|rejection| {
        warn!(reason = %rejection, "a package descriptor was refused");
        rejection_kind(&rejection)
    })?;
    Ok((digest, descriptor))
}

/// Archives and validates a developer directory.
fn prepare_directory(directory: &str) -> Result<Candidate, PackageErrorKind> {
    let limits = Limits::DEFAULT;
    let bytes = build_directory(directory, &limits)?;
    let archive = read_archive(&bytes, &limits).map_err(|error| {
        warn!(%error, "a linked package directory produced an invalid archive");
        PackageErrorKind::ArchiveInvalid
    })?;
    let digest = archive.digest().clone();
    let descriptor = descriptor_from_archive(archive.entries(), &digest).map_err(|rejection| {
        warn!(reason = %rejection, "a linked package descriptor was refused");
        rejection_kind(&rejection)
    })?;
    let identity = descriptor.identity().clone();
    let (definition, commands) = descriptor.into_parts();
    let Some(definition) = definition else {
        return Ok(Candidate::Commands(CommandsPrepared {
            bytes,
            digest,
            commands,
            identity,
            source: RecordSource::Link,
        }));
    };
    Ok(Candidate::Runtime(Box::new(Prepared {
        bytes,
        digest,
        definition,
        commands,
        identity,
        authority: Authority::Local,
        source: RecordSource::Link,
        catalog: None,
        index: None,
    })))
}

/// What an install records and reports, whatever the package exports.
struct Install {
    bytes: Vec<u8>,
    digest: PackageDigest,
    identity: PackageIdentity,
    source: RecordSource,
    /// The catalog that authorized the package, recorded before the install.
    catalog: Option<VerifiedCatalog>,
    /// The plugin index that authorized the package, recorded before the
    /// install.
    index: Option<VerifiedIndex>,
    /// The integration the package declares; none for a package without a
    /// runtime.
    integration: Integration,
    /// The runtime the descriptor declares, when it exports one.
    runtime: Option<PackageRuntimeInfo>,
    commands: Vec<CommandInfo>,
}

/// Installs a checked archive by what its descriptor exports.
fn commit_candidate(
    context: &Context,
    candidate: Candidate,
    plan: InstallPlan,
    retained: &RetainedPins,
) -> Result<PackageInstallResult, PackageErrorKind> {
    match candidate {
        Candidate::Runtime(prepared) => commit_install(context, *prepared, plan, retained),
        Candidate::Commands(prepared) => install_package(
            context,
            Install {
                bytes: prepared.bytes,
                digest: prepared.digest,
                identity: prepared.identity,
                source: prepared.source,
                catalog: None,
                index: None,
                integration: Integration::Known(None),
                runtime: None,
                commands: prepared.commands,
            },
            plan,
            retained,
        ),
    }
}

/// Applies the claim rules, then records the package and rebuilds the runtime
/// registry; a dry run stops after the rules.
fn commit_install(
    context: &Context,
    prepared: Prepared,
    plan: InstallPlan,
    retained: &RetainedPins,
) -> Result<PackageInstallResult, PackageErrorKind> {
    check_claim(
        &context.runtimes,
        &prepared.definition,
        &prepared.identity.id,
        prepared.authority,
    )?;
    install_package(
        context,
        Install {
            runtime: Some(runtime_info(&prepared.definition)),
            integration: declared_by(&prepared.definition),
            commands: prepared.commands,
            bytes: prepared.bytes,
            digest: prepared.digest,
            identity: prepared.identity,
            source: prepared.source,
            catalog: prepared.catalog,
            index: prepared.index,
        },
        plan,
        retained,
    )
}

/// Disables the just-installed package `digest` and fails when the rebuilt
/// runtime registry left it out.
///
/// Validation makes this unreachable for a package the daemon checked; a root
/// that changed between the check and the load is disabled again rather than
/// left recorded and enabled.
fn refuse_loader_rejection(
    context: &Context,
    digest: &PackageDigest,
    reloaded: &PackageReport,
) -> Result<(), PackageErrorKind> {
    let Some(rejected) = reloaded
        .rejected
        .iter()
        .find(|rejected| &rejected.digest == digest)
    else {
        return Ok(());
    };
    warn!(reason = %rejected.reason, "a freshly installed package was refused by the loader");
    let kind = rejection_kind(&rejected.reason);
    let _ = context.store.registry().set_enabled(digest, false);
    let _ = context.runtimes.reload();
    Err(kind)
}

/// Records the package and rebuilds the runtime registry; a dry run stops
/// before the first write.
fn install_package(
    context: &Context,
    install: Install,
    plan: InstallPlan,
    retained: &RetainedPins,
) -> Result<PackageInstallResult, PackageErrorKind> {
    let Install {
        bytes,
        digest,
        identity,
        source,
        catalog,
        index,
        integration,
        runtime,
        commands,
    } = install;
    let now = unix_now()?;
    // A package whose integration is incompatible with a retained version is
    // installed without being selected, so the install itself never fails on
    // it and the selected version keeps serving.
    let held_back = conflict(
        context,
        &read_state(context)?,
        retained.gate(),
        (&digest, &identity.id),
        &integration,
    );
    let select = plan.select && held_back.is_none();
    if plan.dry_run {
        return Ok(PackageInstallResult {
            status: PackageInstallStatus::Preview,
            package: PackageInfo {
                referenced: retained.known().contains(&digest),
                digest,
                package: identity,
                origin: origin_of(source),
                enabled: plan.enable,
                selected: select,
                installed_at_unix_seconds: now,
                runtime_id: runtime.as_ref().map(|runtime| runtime.runtime_id.clone()),
                fault: None,
                revoked: false,
                selection_blocked: held_back.map(|retained| PackageSelectionBlock {
                    reason: protocol::PackageSelectionBlockReason::IncompatibleWithRetained,
                    retained,
                }),
            },
            runtime,
            commands,
            reloaded: false,
        });
    }
    if let Some(blocking) = &held_back {
        if plan.select {
            warn!(
                package = %identity.id,
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
    if let Some(catalog) = catalog.as_ref() {
        registry
            .record_catalog(catalog)
            .map_err(|error| registry_kind(&error))?;
    }
    // The index marks and revocations are persisted for the same reason and
    // before the install too: they only tighten, so a crash after this write
    // leaves the package absent under raised marks, and the same document
    // installs on the next attempt.
    if let Some(index) = index.as_ref() {
        registry
            .record_index(index)
            .map_err(|error| registry_kind(&error))?;
    }
    let report = registry
        .install(&InstallRequest {
            archive: &bytes,
            expected: &digest,
            identity: identity.clone(),
            source,
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
        refuse_loader_rejection(context, &digest, &reloaded)?;
    }
    let state = read_state(context)?;
    let record = state
        .package(&digest)
        .ok_or(PackageErrorKind::NotInstalled)?;
    let inspected = inspect_one(context, &state, record, retained, &reloaded);
    Ok(PackageInstallResult {
        status,
        package: inspected.info,
        runtime,
        commands,
        reloaded: true,
    })
}
