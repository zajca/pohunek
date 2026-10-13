//! The offline step `pohunekd ensure-official-runtimes`.
//!
//! The release's own official package archives serve the runtimes the host
//! uses, and the host profiles over those runtimes are pinned to them. The
//! step runs while no daemon runs, so the caller holds the instance locks for
//! an apply. A dry run reads only. Every archive, catalog entry and profile is
//! checked before anything is written: a plan with a refusal changes nothing.
//!
//! Installs go through the same [`commit_install`], selection and bind code the
//! `package.*` methods use, so the trust decisions have one implementation.

// Rust guideline compliant 2026-10-14

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use package::catalog_state::CatalogState;
use package::registry::{RegistryState, RetainedDigests};
use package::{PackageDigest, VerifiedCatalog, MAX_CATALOG_BYTES};
use pohunek_service_config::runtimes::{
    EnsureReport, IntegrationAction, PackageAction, PackageStatus, ProfileAction, ProfileStatus,
    Refusal, UseReason, CODE_PACKAGE_ARCHIVE_INVALID, CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM,
    CODE_PROFILE_NOT_MIGRATABLE, CODE_RELEASE_CATALOG_UNAVAILABLE, ENSURE_REPORT_VERSION,
};
use protocol::{
    IntegrationInstallState, IntegrationStatusParams, PackageBindProfileParams, PackageBindStatus,
    PackageErrorKind, PackageInstallStatus, ProtocolError, RuntimeId, RuntimeRef,
};
use tracing::warn;

use super::bind::bind_profile;
use super::selection::{conflict, declared_by};
use super::source::read_regular_file;
use super::trust;
use super::{
    commit_install, ensure_selectable, prepare_official, prove_loadable, read_state, unix_now,
    Context, HostTrustAnchor, InstallPlan, Prepared,
};
use crate::agent::host::{is_reserved, PackageStore, RuntimeHost};
use crate::agent::{apply_pin, parse_head, Pin, ProfileRegistry};
use crate::catalog_anchor::CatalogTrust;
use crate::session::runtime_host_over;
use crate::store::Store;

/// Name of the signed catalog inside the release's `runtime/` directory.
const CATALOG_FILE: &str = "runtime-catalog.json";

/// Directory of the release's package archives inside `runtime/`.
const PACKAGES_DIR: &str = "packages";

/// Suffix of a release package archive. The archive of a runtime is named
/// after the runtime id, as `xtask` writes it.
const ARCHIVE_SUFFIX: &str = ".tar.zst";

/// Refusal subject of a problem with the release catalog as a whole.
const CATALOG_SUBJECT: &str = "runtime-catalog";

/// The inputs of one run, resolved by the daemon binary.
#[derive(Debug, Clone)]
pub struct EnsureInputs {
    /// The metadata store the sessions are recorded in.
    pub store_path: PathBuf,
    /// The owner-private package store (`plugins_dir`).
    pub plugins_dir: PathBuf,
    /// The host agent profiles directory (`<config>/agents`).
    pub profiles_dir: PathBuf,
    /// The release's `runtime/` directory, beside the running executable.
    pub release_dir: PathBuf,
    /// The trust anchor beside the running executable.
    pub catalog_trust: CatalogTrust,
    /// Whether the run only computes the plan.
    pub dry_run: bool,
    /// Version of the daemon binary that runs the step.
    pub daemon_version: String,
}

/// A failure that stops the step before a report exists.
///
/// Every failure is reported without file content or secret values.
#[derive(Debug, thiserror::Error)]
pub enum EnsureError {
    /// The package store cannot be opened.
    #[error("the runtime package store cannot be opened: {0}")]
    Plugins(String),
    /// The metadata store cannot be read.
    #[error("the metadata store cannot be read: {0}")]
    Store(#[source] io::Error),
    /// The host profiles cannot be listed.
    #[error("the agent profiles cannot be listed: {0}")]
    Profiles(#[source] io::Error),
    /// The runtime registry cannot be read or written.
    #[error("the runtime registry failed: {0}")]
    Registry(String),
    /// A host profile could not be pinned after its package was installed.
    #[error("agent profile '{profile}' could not be pinned: {code}")]
    Bind {
        /// Profile that failed.
        profile: String,
        /// Error code of the bind.
        code: String,
    },
}

/// One official runtime the plan installs or keeps.
struct Planned {
    runtime: RuntimeId,
    reasons: Vec<UseReason>,
    prepared: Prepared,
    present: bool,
}

/// One host profile over an official runtime that pins no package.
struct Unpinned {
    name: String,
    base: RuntimeId,
    text: String,
}

/// A host profile to pin to the package planned for its base runtime.
struct Bind {
    name: String,
    base: RuntimeId,
    digest: PackageDigest,
}

/// The official runtimes the host uses, with the reasons, in id order.
#[derive(Default)]
struct Uses(BTreeMap<String, (RuntimeId, Vec<UseReason>)>);

impl Uses {
    fn add(&mut self, runtime: &RuntimeId, reason: UseReason) {
        let entry = self
            .0
            .entry(runtime.as_str().to_owned())
            .or_insert_with(|| (runtime.clone(), Vec::new()));
        if !entry.1.contains(&reason) {
            entry.1.push(reason);
        }
    }

    fn list(&self) -> Vec<(RuntimeId, Vec<UseReason>)> {
        self.0.values().cloned().collect()
    }
}

/// What [`gather`] found.
struct Gathered {
    uses: Uses,
    unpinned: Vec<Unpinned>,
}

/// What the plan reads from the registries and the store.
struct PlanInputs<'a> {
    inputs: &'a EnsureInputs,
    /// The registry as it is; `None` without a package store, where nothing
    /// is installed.
    state: Option<&'a RegistryState>,
    /// The context a conflict check reads installed packages through.
    planning: Option<&'a Context>,
    /// The digests a session or a profile still pins.
    retained: &'a RetainedDigests,
}

/// The release catalog, verified, and where its archives are.
struct Release<'a> {
    anchor: &'a HostTrustAnchor,
    catalog_state: &'a CatalogState,
    catalog: VerifiedCatalog,
    catalog_path: String,
    archive_dir: PathBuf,
}

/// Accumulates the report while the plan is computed.
struct Report {
    version: u32,
    daemon: String,
    dry_run: bool,
    packages: Vec<PackageAction>,
    profiles: Vec<ProfileAction>,
    integrations: Vec<IntegrationAction>,
    refusals: Vec<Refusal>,
}

impl Report {
    fn new(inputs: &EnsureInputs) -> Self {
        Self {
            version: ENSURE_REPORT_VERSION,
            daemon: inputs.daemon_version.clone(),
            dry_run: inputs.dry_run,
            packages: Vec::new(),
            profiles: Vec::new(),
            integrations: Vec::new(),
            refusals: Vec::new(),
        }
    }

    fn refuse(&mut self, subject: &str, code: &str, detail: &str) {
        self.refusals.push(Refusal {
            subject: subject.to_owned(),
            code: code.to_owned(),
            detail: detail.to_owned(),
        });
    }

    /// Ends the report. With a refusal the planned entries stay as the plan
    /// computed them; nothing was applied, and the refusals say why.
    fn finish(self) -> EnsureReport {
        EnsureReport {
            report_version: self.version,
            daemon_version: self.daemon,
            dry_run: self.dry_run,
            packages: self.packages,
            profiles: self.profiles,
            integrations: self.integrations,
            refusals: self.refusals,
        }
    }
}

/// Computes the plan and, unless it is a dry run or has refusals, applies it.
///
/// # Errors
///
/// Returns [`EnsureError`] when a store cannot be read or a change fails
/// midway. A refusal is a report, not an error: it means nothing changed.
pub fn ensure_official_runtimes(inputs: &EnsureInputs) -> Result<EnsureReport, EnsureError> {
    // The plan is read over the package store only when it already exists: a
    // store is created by an apply that has something to change, never by a
    // plan or a refusal.
    let existing = PackageStore::open_existing(&inputs.plugins_dir)
        .map_err(|error| EnsureError::Plugins(error.to_string()))?;
    let host = match &existing {
        Some(store) => runtime_host_over(store.clone()).map_err(host_error)?,
        None => RuntimeHost::default(),
    };
    let profiles = ProfileRegistry::with_runtimes(Some(inputs.profiles_dir.clone()), host.clone());
    // Without a package store nothing is installed, so the registry is absent
    // rather than empty: the planner then treats every package as not present.
    let (state, catalog_state) = match &existing {
        Some(store) => (
            Some(
                store
                    .registry()
                    .state()
                    .map_err(|error| EnsureError::Registry(error.to_string()))?,
            ),
            store
                .registry()
                .catalog_state()
                .map_err(|error| EnsureError::Registry(error.to_string()))?,
        ),
        None => (None, CatalogState::default()),
    };
    let planning = existing.clone().map(|store| Context {
        store,
        runtimes: host.clone(),
        trust: inputs.catalog_trust.clone(),
        integrations: Mutex::default(),
    });

    let mut report = Report::new(inputs);
    let gathered = gather(inputs, &profiles, &mut report)?;
    let retained = retained_pins(&inputs.store_path, &profiles, None)?;
    let plan = PlanInputs {
        inputs,
        state: state.as_ref(),
        planning: planning.as_ref(),
        retained: &retained,
    };
    let Some(planned) = plan_packages(&gathered.uses, &catalog_state, &plan, &mut report)? else {
        return Ok(report.finish());
    };
    let binds = plan_binds(&profiles, &gathered.unpinned, &planned, &mut report);
    let changes = planned.iter().any(|plan| !plan.present) || !binds.is_empty();
    if inputs.dry_run || !report.refusals.is_empty() || !changes {
        // Without changes every planned package is present, so the status
        // reported for an absent one is only seen on a plan.
        report.packages = planned
            .iter()
            .map(|plan| package_action(plan, PackageStatus::Present, PackageStatus::Install))
            .collect();
        report.profiles = binds
            .iter()
            .map(|bind| profile_action(bind, ProfileStatus::Bind))
            .collect();
        return Ok(report.finish());
    }
    drop(planning);
    apply(inputs, existing, planned, binds, &mut report)?;
    Ok(report.finish())
}

/// Reads the integrations, the host profiles and the sessions, and collects
/// the official runtimes they use.
fn gather(
    inputs: &EnsureInputs,
    profiles: &ProfileRegistry,
    report: &mut Report,
) -> Result<Gathered, EnsureError> {
    let mut uses = Uses::default();
    let homes = integration_homes(profiles)?;
    for runtime in crate::integration::hook_runtime_ids() {
        if integration_present(&homes, &runtime) {
            report.integrations.push(IntegrationAction {
                runtime: runtime.as_str().to_owned(),
            });
            uses.add(&runtime, UseReason::Integration);
        }
    }
    let mut unpinned = Vec::new();
    for (name, text) in profiles.scanned_texts().map_err(EnsureError::Profiles)? {
        // A profile that cannot be read or parsed may be over an official
        // runtime, so it refuses with its reason instead of being skipped. Its
        // detail can quote profile text, so only the error code is reported.
        let source = match text {
            Ok(source) => source,
            Err(error) => {
                report.refuse(
                    &name,
                    CODE_PROFILE_NOT_MIGRATABLE,
                    &format!(
                        "the profile cannot be read ({}); fix or remove `{name}.toml` in the \
                         host profile directory, then run the upgrade again",
                        error.code
                    ),
                );
                continue;
            }
        };
        let head = match parse_head(&source) {
            Ok(head) => head,
            Err(_detail) => {
                report.refuse(
                    &name,
                    CODE_PROFILE_NOT_MIGRATABLE,
                    &format!(
                        "the profile head cannot be parsed; fix or remove `{name}.toml` in the \
                         host profile directory, then run the upgrade again"
                    ),
                );
                continue;
            }
        };
        if !is_official(&head.base) {
            continue;
        }
        // A pinned profile still uses its runtime: the release package must be
        // present for it, and a second run then reports it as present.
        uses.add(&head.base, UseReason::Profile);
        if head.pin().is_none() {
            unpinned.push(Unpinned {
                name,
                base: head.base,
                text: source,
            });
        }
    }
    for runtime in read_store(&inputs.store_path)?.runtimes {
        uses.add(&runtime, UseReason::Session);
    }
    Ok(Gathered { uses, unpinned })
}

/// Installs, enables and selects the planned packages, then pins the profiles.
///
/// The package store is created here, once the plan has no refusal and has a
/// change to make.
fn apply(
    inputs: &EnsureInputs,
    existing: Option<PackageStore>,
    planned: Vec<Planned>,
    binds: Vec<Bind>,
    report: &mut Report,
) -> Result<(), EnsureError> {
    let store = match existing {
        Some(store) => store,
        None => PackageStore::open(&inputs.plugins_dir)
            .map_err(|error| EnsureError::Plugins(error.to_string()))?,
    };
    let host = runtime_host_over(store.clone()).map_err(host_error)?;
    let profiles = ProfileRegistry::with_runtimes(Some(inputs.profiles_dir.clone()), host.clone());
    let context = Context {
        store,
        runtimes: host,
        trust: inputs.catalog_trust.clone(),
        integrations: Mutex::default(),
    };
    for plan in planned {
        let action = package_action(&plan, PackageStatus::Present, PackageStatus::Installed);
        if !plan.present {
            install(&context, plan.prepared, &inputs.store_path, &profiles)?;
        }
        report.packages.push(action);
    }
    for bind in binds {
        let mut retained = profiles
            .pinned_digests_excluding(Some(&bind.name))
            .map_err(EnsureError::Profiles)?;
        retained.extend(read_store(&inputs.store_path)?.pins);
        let params = PackageBindProfileParams {
            profile: bind.name.clone(),
            digest: Some(bind.digest.clone()),
            dry_run: false,
        };
        let result = bind_profile(&context, &profiles, &retained, &params).map_err(|kind| {
            EnsureError::Bind {
                profile: bind.name.clone(),
                code: ProtocolError::from(kind).code,
            }
        })?;
        if result.status != PackageBindStatus::Bound {
            return Err(EnsureError::Bind {
                profile: bind.name,
                code: "profile_not_bound".to_owned(),
            });
        }
        report
            .profiles
            .push(profile_action(&bind, ProfileStatus::Bound));
    }
    Ok(())
}

/// Whether `runtime` is an official runtime id: reserved, and not the shell.
fn is_official(runtime: &RuntimeId) -> bool {
    is_reserved(runtime) && runtime.as_str() != RuntimeId::SHELL
}

/// The official runtime a session record was launched from, if any.
fn session_runtime(binding: &crate::store::ResumeBinding) -> Option<RuntimeId> {
    let runtime = match binding.launch_binding.binding() {
        Some(launch) => match &launch.provenance {
            protocol::BindingProvenance::Builtin { .. } => launch.runtime_id.clone(),
            protocol::BindingProvenance::Package { .. } => return None,
        },
        None => match &binding.agent_base {
            RuntimeRef::Id(runtime) => runtime.clone(),
            RuntimeRef::Historical(_) => return None,
        },
    };
    is_official(&runtime).then_some(runtime)
}

/// What the metadata store's records use and pin.
struct StoreView {
    /// The official runtime of every session record, whatever its state.
    runtimes: Vec<RuntimeId>,
    /// The package digests the records pin.
    pins: Vec<PackageDigest>,
}

/// Reads the metadata store through the startup migration's dry run, so records
/// of an older schema are judged the way the new daemon loads them after its
/// migration. The store is never written, and a store of a newer schema is an
/// error.
fn read_store(store_path: &Path) -> Result<StoreView, EnsureError> {
    let dry = Store::new(store_path.to_path_buf())
        .dry_run_migration()
        .map_err(EnsureError::Store)?;
    let bindings: Vec<&crate::store::ResumeBinding> = dry
        .sessions
        .iter()
        .filter_map(|session| session.recovery.as_ref())
        .chain(dry.resume.iter())
        .collect();
    let runtimes = bindings
        .iter()
        .filter_map(|binding| session_runtime(binding))
        .collect();
    let pins = bindings
        .iter()
        .filter_map(|binding| match binding.launch_binding.binding() {
            Some(launch) => match &launch.provenance {
                protocol::BindingProvenance::Package { package_digest, .. } => {
                    Some(package_digest.clone())
                }
                protocol::BindingProvenance::Builtin { .. } => None,
            },
            None => None,
        })
        .collect();
    Ok(StoreView { runtimes, pins })
}

/// Digests every installed package a session or profile still pins, excluding
/// the pin of the profile `skip` when one is given.
fn retained_pins(
    store_path: &Path,
    profiles: &ProfileRegistry,
    skip: Option<&str>,
) -> Result<RetainedDigests, EnsureError> {
    let mut retained = profiles
        .pinned_digests_excluding(skip)
        .map_err(EnsureError::Profiles)?;
    retained.extend(read_store(store_path)?.pins);
    Ok(retained)
}

/// Verifies the release catalog for the plan.
///
/// Returns `None` after the refusal is recorded when the catalog cannot be
/// trusted or read.
fn load_release<'a>(
    inputs: &EnsureInputs,
    anchor: &'a HostTrustAnchor,
    catalog_state: &'a CatalogState,
    report: &mut Report,
) -> Result<Option<Release<'a>>, EnsureError> {
    let catalog_path = inputs.release_dir.join(CATALOG_FILE);
    let catalog_text = catalog_path.to_string_lossy().into_owned();
    let now = unix_now().map_err(kind_error)?;
    let verified = read_regular_file(&catalog_text, catalog_bytes_limit())
        .map_err(|_failure| "the release catalog cannot be read".to_owned())
        .and_then(|bytes| {
            trust::verify_catalog_document(&bytes, anchor, catalog_state, now)
                .map_err(kind_refusal_detail)
        });
    match verified {
        Ok(catalog) => Ok(Some(Release {
            anchor,
            catalog_state,
            catalog,
            catalog_path: catalog_text,
            archive_dir: inputs.release_dir.join(PACKAGES_DIR),
        })),
        Err(detail) => {
            report.refuse(CATALOG_SUBJECT, CODE_RELEASE_CATALOG_UNAVAILABLE, &detail);
            Ok(None)
        }
    }
}

/// Plans the package of every used runtime.
///
/// Returns `None` when the plan cannot be made at all, after the refusals have
/// been recorded.
fn plan_packages(
    uses: &Uses,
    catalog_state: &CatalogState,
    plan: &PlanInputs<'_>,
    report: &mut Report,
) -> Result<Option<Vec<Planned>>, EnsureError> {
    let runtimes = uses.list();
    if runtimes.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let anchor = match &plan.inputs.catalog_trust {
        CatalogTrust::Loaded(anchor) => anchor,
        CatalogTrust::Absent | CatalogTrust::Invalid(_) => {
            for (runtime, _) in &runtimes {
                report.refuse(
                    runtime.as_str(),
                    CODE_RELEASE_CATALOG_UNAVAILABLE,
                    "this build has no usable trust anchor beside the daemon executable",
                );
            }
            return Ok(None);
        }
    };
    let Some(release) = load_release(plan.inputs, anchor, catalog_state, report)? else {
        return Ok(None);
    };
    let mut planned = Vec::new();
    for (runtime, reasons) in runtimes {
        if let Some(entry) = plan_runtime(plan, &release, runtime, reasons, report)? {
            planned.push(entry);
        }
    }
    Ok(Some(planned))
}

/// Plans one official runtime: its one release package for this platform,
/// verified, and whether the host already has it selected.
fn plan_runtime(
    plan: &PlanInputs<'_>,
    release: &Release<'_>,
    runtime: RuntimeId,
    reasons: Vec<UseReason>,
    report: &mut Report,
) -> Result<Option<Planned>, EnsureError> {
    let subject = runtime.as_str().to_owned();
    let platform = trust::host_platform().map_err(kind_error)?;
    let core = trust::core_version().map_err(kind_error)?;
    let listed = release
        .catalog
        .entries()
        .iter()
        .filter(|entry| {
            entry.runtime_id() == &runtime
                && entry.supports_platform(&platform)
                && entry.supports_core(&core)
        })
        .count();
    if listed != 1 {
        report.refuse(
            &subject,
            CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM,
            "the release catalog lists no single package of this runtime for this platform",
        );
        return Ok(None);
    }
    let archive = release
        .archive_dir
        .join(format!("{}{ARCHIVE_SUFFIX}", runtime.as_str()));
    let archive_text = archive.to_string_lossy().into_owned();
    let prepared = match prepare_official(
        release.anchor,
        release.catalog_state,
        &archive_text,
        &release.catalog_path,
    ) {
        Ok(prepared) => prepared,
        Err(kind) => {
            report.refuse(
                &subject,
                CODE_PACKAGE_ARCHIVE_INVALID,
                &kind_refusal_detail(kind),
            );
            return Ok(None);
        }
    };
    if prepared.definition.runtime_id() != &runtime {
        report.refuse(
            &subject,
            CODE_PACKAGE_ARCHIVE_INVALID,
            "the release archive serves another runtime than the catalog lists it for",
        );
        return Ok(None);
    }
    let present = plan.state.is_some_and(|state| {
        state.package(&prepared.digest).is_some_and(|record| {
            record.enabled() && state.selected(&record.identity().id) == Some(&prepared.digest)
        })
    });
    // The bind of a profile refuses a package the coexistence rules reject
    // (`ensure_coexists`); the same test runs here, for present packages too,
    // so a dry run refuses what an apply would.
    if let (Some(context), Some(state)) = (plan.planning, plan.state) {
        let integration = declared_by(&prepared.definition);
        let blocking = conflict(
            context,
            state,
            plan.retained,
            (&prepared.digest, &prepared.identity.id),
            &integration,
        );
        if blocking.is_some() {
            warn!(
                runtime = %runtime,
                "the release package cannot be selected next to a retained version"
            );
            report.refuse(
                &subject,
                &ProtocolError::from(PackageErrorKind::IntegrationIncompatible).code,
                "a retained version of this package has an incompatible integration",
            );
            return Ok(None);
        }
    }
    Ok(Some(Planned {
        runtime,
        reasons,
        prepared,
        present,
    }))
}

/// Plans the pins of the unpinned profiles, validating each rewritten profile
/// against the planned package definition the way a bind does.
fn plan_binds(
    profiles: &ProfileRegistry,
    unpinned: &[Unpinned],
    planned: &[Planned],
    report: &mut Report,
) -> Vec<Bind> {
    let mut binds = Vec::new();
    for profile in unpinned {
        let Some(plan) = planned.iter().find(|plan| plan.runtime == profile.base) else {
            continue;
        };
        let pin = Pin {
            package: plan.prepared.identity.id.clone(),
            digest: plan.prepared.digest.clone(),
        };
        let rewritten = match apply_pin(&profile.text, &pin) {
            Ok(rewritten) => rewritten,
            Err(_detail) => {
                report.refuse(
                    &profile.name,
                    CODE_PROFILE_NOT_MIGRATABLE,
                    "the profile cannot be rewritten with its package pin",
                );
                continue;
            }
        };
        if let Err(error) = profiles.validate_candidate(
            &profile.name,
            &rewritten,
            &Arc::new(plan.prepared.definition.clone()),
        ) {
            report.refuse(
                &profile.name,
                CODE_PROFILE_NOT_MIGRATABLE,
                &format!("the pinned profile is not accepted: {}", error.code),
            );
            continue;
        }
        binds.push(Bind {
            name: profile.name.clone(),
            base: profile.base.clone(),
            digest: plan.prepared.digest.clone(),
        });
    }
    binds
}

/// Installs, enables and selects the planned package, under the registry's own
/// lock for each change.
///
/// An absent record is installed with both flags set. A record that is kept
/// has only what is not yet so changed: a disabled or unselected package is
/// enabled or selected, never reinstalled.
fn install(
    context: &Context,
    prepared: Prepared,
    store_path: &Path,
    profiles: &ProfileRegistry,
) -> Result<(), EnsureError> {
    let retained = retained_pins(store_path, profiles, None)?;
    let state = read_state(context).map_err(kind_error)?;
    let digest = prepared.digest.clone();
    let Some(record) = state.package(&digest) else {
        let result = commit_install(
            context,
            prepared,
            InstallPlan {
                enable: true,
                select: true,
                dry_run: false,
            },
            &retained,
        )
        .map_err(kind_error)?;
        if result.status != PackageInstallStatus::Installed {
            return Err(EnsureError::Registry(
                "the release package was not newly installed".to_owned(),
            ));
        }
        return Ok(());
    };
    if !record.enabled() {
        prove_loadable(context, &digest).map_err(kind_error)?;
        context
            .store
            .registry()
            .set_enabled(&digest, true)
            .map_err(|error| EnsureError::Registry(error.to_string()))?;
    }
    if state.selected(&record.identity().id) != Some(&digest) {
        ensure_selectable(context, &retained, &digest, &prepared.definition).map_err(kind_error)?;
        context
            .store
            .registry()
            .select(&digest)
            .map_err(|error| EnsureError::Registry(error.to_string()))?;
    }
    context.runtimes.reload().map_err(|error| {
        warn!(%error, "the runtime registry was not rebuilt after the release install");
        EnsureError::Registry("the runtime registry was not rebuilt".to_owned())
    })?;
    Ok(())
}

/// The profile status of one planned or applied pin.
fn profile_action(bind: &Bind, status: ProfileStatus) -> ProfileAction {
    ProfileAction {
        name: bind.name.clone(),
        base: bind.base.as_str().to_owned(),
        digest: bind.digest.as_str().to_owned(),
        status,
    }
}

/// The package status of one planned runtime.
fn package_action(plan: &Planned, present: PackageStatus, changed: PackageStatus) -> PackageAction {
    PackageAction {
        runtime: plan.runtime.as_str().to_owned(),
        package: plan.prepared.identity.id.as_str().to_owned(),
        version: plan.prepared.identity.version.as_str().to_owned(),
        digest: plan.prepared.digest.as_str().to_owned(),
        status: if plan.present { present } else { changed },
        reasons: plan.reasons.clone(),
    }
}

/// The base environment the integration status reads the config homes with,
/// as the daemon hands it to agents when it runs without a supervisor.
fn integration_homes(
    profiles: &ProfileRegistry,
) -> Result<crate::integration::ConfigHomes, EnsureError> {
    let allowlist: Vec<String> = pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let base = crate::runtime::environment::base_environment(
        &allowlist,
        &crate::runtime::EnvironmentSource::Process,
    )
    .map_err(|error| EnsureError::Registry(error.to_string()))?;
    Ok(crate::integration::ConfigHomes::new(profiles.clone(), base))
}

/// Whether the daemon-installed hooks of `runtime` are present in its home.
///
/// A home that cannot be inspected counts as present: the post-ready reinstall
/// then runs and reports its own failure, where an absent answer would hide the
/// hooks from the operator. The inspection error is logged.
fn integration_present(homes: &crate::integration::ConfigHomes, runtime: &RuntimeId) -> bool {
    let params = IntegrationStatusParams {
        agent: Some(RuntimeRef::from(runtime.clone())),
        profile: None,
        all_profiles: false,
    };
    match crate::integration::status_in(homes, params) {
        Ok(result) => result
            .agents
            .iter()
            .any(|agent| agent.state != IntegrationInstallState::NotInstalled),
        Err(error) => {
            warn!(
                runtime = %runtime,
                code = %error.code,
                detail = %error.msg,
                "the integration hooks cannot be inspected; they are listed for reinstall"
            );
            true
        }
    }
}

/// The largest catalog the step reads, as the install path bounds it.
fn catalog_bytes_limit() -> u64 {
    u64::try_from(MAX_CATALOG_BYTES).unwrap_or(u64::MAX)
}

/// The error of a package step that failed with `kind`.
fn kind_error(kind: PackageErrorKind) -> EnsureError {
    EnsureError::Registry(ProtocolError::from(kind).code)
}

/// The error of a package store whose runtime host cannot be built.
fn host_error(error: ProtocolError) -> EnsureError {
    EnsureError::Plugins(error.code)
}

/// The refusal detail of a package check that failed with `kind`. It names
/// the wire code only, never the archive or catalog content.
fn kind_refusal_detail(kind: PackageErrorKind) -> String {
    format!(
        "the package check failed: {}",
        ProtocolError::from(kind).code
    )
}
