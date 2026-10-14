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
use package::verify::VerifyError;
use package::{PackageDigest, VerifiedCatalog, MAX_CATALOG_BYTES};
use pohunek_service_config::runtimes::{
    CatalogStatus, EnsureReport, IntegrationAction, PackageAction, PackageStatus, ProfileAction,
    ProfileStatus, Refusal, UseReason, CODE_PACKAGE_ARCHIVE_INVALID,
    CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM, CODE_PROFILE_NOT_MIGRATABLE,
    CODE_RELEASE_CATALOG_UNAVAILABLE, CODE_SERVICE_CONFIG_UNREADABLE, ENSURE_REPORT_VERSION,
};
use protocol::{
    IntegrationInstallState, IntegrationStatusParams, PackageBindProfileParams, PackageBindStatus,
    PackageErrorKind, PackageId, PackageIdentity, PackageInstallStatus, ProtocolError, RuntimeId,
    RuntimeRef,
};
use tracing::warn;

use super::bind::bind_profile;
use super::error::rejection_kind;
use super::selection::{conflict, declared_by};
use super::source::read_regular_file;
use super::trust;
use super::{
    check_claim, commit_install, ensure_selectable, prepare_official, prove_loadable, read_state,
    unix_now, Context, HostTrustAnchor, InstallPlan, Prepared,
};
use crate::agent::host::{
    is_reserved, PackageRejection, PackageStore, RuntimeDefinition, RuntimeHost,
};
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

/// Refusal subject of an installed `service.toml` that cannot be read.
const SERVICE_CONFIG_SUBJECT: &str = "service-config";

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
    /// The launch environment of the installed daemon, or the reason the
    /// installed `service.toml` cannot be read. An unreadable file refuses the
    /// plan, because the integration homes are read through it.
    pub environment: Result<crate::runtime::LaunchEnvironment, String>,
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

/// What the plan does with one official runtime's package.
enum Action {
    /// The installed package serves the runtime and verifies; nothing to do.
    Present,
    /// The release archive is installed, enabled and selected, or its missing
    /// or altered root is restored.
    Install(Box<Prepared>),
    /// No usable release catalog: the installed package that serves the
    /// runtime keeps serving it.
    Kept,
}

/// The state of an installed release package the plan found.
enum Health {
    /// Enabled, selected, and its root verifies and loads.
    Present,
    /// Not installed, or not serving, or its root is missing: the archive
    /// installs, enables and selects it again.
    Install,
    /// The root is present and fails verification or loading; the reason
    /// names the stable error kind.
    Damaged(PackageErrorKind),
}

/// One official runtime the plan keeps or installs.
struct Planned {
    runtime: RuntimeId,
    reasons: Vec<UseReason>,
    identity: PackageIdentity,
    digest: PackageDigest,
    definition: RuntimeDefinition,
    action: Action,
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
    /// The registry as it is; `None` without a package store, where nothing
    /// is installed.
    state: Option<&'a RegistryState>,
    /// The context a conflict check and a health check read installed packages
    /// through.
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

/// The catalog trust state the plan reports, and the catalog a non-empty plan
/// records.
struct CatalogPlan {
    status: CatalogStatus,
    record: Option<VerifiedCatalog>,
}

/// Accumulates the report while the plan is computed.
struct Report {
    version: u32,
    daemon: String,
    dry_run: bool,
    catalog: CatalogStatus,
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
            catalog: CatalogStatus::Unavailable,
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
            catalog: self.catalog,
            packages: self.packages,
            profiles: self.profiles,
            integrations: self.integrations,
            refusals: self.refusals,
        }
    }
}

/// The plan of one run, as a dry run reports it, and the verified definitions
/// of the packages it would serve: the release archives it installs and the
/// installed packages it keeps.
#[derive(Debug)]
pub struct OfficialPlan {
    /// The plan: the packages and profiles it would change, and the refusals.
    pub report: EnsureReport,
    /// The definitions the planned packages serve, verified exactly as an apply
    /// verifies them.
    pub definitions: Vec<RuntimeDefinition>,
}

/// Computes the plan without changing anything, whatever `inputs.dry_run` says.
///
/// # Errors
///
/// Returns [`EnsureError`] when a store cannot be read. A refusal is a report.
pub fn plan_official_runtimes(inputs: &EnsureInputs) -> Result<OfficialPlan, EnsureError> {
    let computed = compute(inputs)?;
    let definitions = computed
        .planned
        .iter()
        .map(|plan| plan.definition.clone())
        .collect();
    Ok(OfficialPlan {
        report: computed.dry_report(true),
        definitions,
    })
}

/// Computes the plan and, unless it is a dry run, has refusals or has nothing
/// to change, applies it.
///
/// # Errors
///
/// Returns [`EnsureError`] when a store cannot be read or a change fails
/// midway. A refusal is a report, not an error: it means nothing changed.
pub fn ensure_official_runtimes(inputs: &EnsureInputs) -> Result<EnsureReport, EnsureError> {
    let computed = compute(inputs)?;
    if inputs.dry_run || !computed.report.refusals.is_empty() || !computed.has_changes() {
        return Ok(computed.dry_report(inputs.dry_run));
    }
    let Computed {
        mut report,
        planned,
        binds,
        catalog_record,
        existing,
    } = computed;
    apply(
        inputs,
        existing,
        planned,
        binds,
        catalog_record,
        &mut report,
    )?;
    Ok(report.finish())
}

/// What [`compute`] found: the plan, before anything is applied.
struct Computed {
    report: Report,
    /// The planned packages, empty when the plan cannot be made at all.
    planned: Vec<Planned>,
    /// The profiles to pin to the planned packages.
    binds: Vec<Bind>,
    /// The catalog a non-empty plan records.
    catalog_record: Option<VerifiedCatalog>,
    /// The package store when it exists; an apply creates it otherwise.
    existing: Option<PackageStore>,
}

impl Computed {
    fn has_changes(&self) -> bool {
        self.planned
            .iter()
            .any(|plan| matches!(plan.action, Action::Install(_)))
            || !self.binds.is_empty()
            || self.catalog_record.is_some()
    }

    /// The report of the plan. Without changes every planned package is kept
    /// as it is, so the status reported for an installed one is only seen on a
    /// plan.
    fn dry_report(self, dry_run: bool) -> EnsureReport {
        let Computed {
            mut report,
            planned,
            binds,
            ..
        } = self;
        report.dry_run = dry_run;
        report.packages = planned
            .iter()
            .map(|plan| package_action(plan, false))
            .collect();
        report.profiles = binds
            .iter()
            .map(|bind| profile_action(bind, ProfileStatus::Bind))
            .collect();
        report.finish()
    }
}

/// Reads the sources and plans every change, without writing.
fn compute(inputs: &EnsureInputs) -> Result<Computed, EnsureError> {
    let launch = match &inputs.environment {
        Ok(launch) => launch,
        Err(detail) => {
            let mut report = Report::new(inputs);
            report.refuse(
                SERVICE_CONFIG_SUBJECT,
                CODE_SERVICE_CONFIG_UNREADABLE,
                detail,
            );
            return Ok(Computed {
                report,
                planned: Vec::new(),
                binds: Vec::new(),
                catalog_record: None,
                existing: None,
            });
        }
    };
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
    let gathered = gather(inputs, launch, &profiles, &mut report)?;
    let retained = retained_pins(&inputs.store_path, &profiles, None)?;
    let plan = PlanInputs {
        state: state.as_ref(),
        planning: planning.as_ref(),
        retained: &retained,
    };
    let release = load_release(inputs, &catalog_state);
    let catalog = catalog_plan(release.as_ref(), &catalog_state);
    report.catalog = catalog.status;
    let planned = plan_packages(&gathered.uses, release.as_ref(), &plan, &mut report)?;
    let binds = plan_binds(
        &profiles,
        &plan,
        &inputs.store_path,
        &gathered.unpinned,
        &planned,
        &mut report,
    )?;
    Ok(Computed {
        report,
        planned,
        binds,
        catalog_record: catalog.record,
        existing,
    })
}

/// Reads the profiles, the integrations and the sessions, and collects the
/// official runtimes they use.
fn gather(
    inputs: &EnsureInputs,
    launch: &crate::runtime::LaunchEnvironment,
    profiles: &ProfileRegistry,
    report: &mut Report,
) -> Result<Gathered, EnsureError> {
    let mut uses = Uses::default();
    let mut unpinned = Vec::new();
    // Every profile over an official base, pinned or not, with its base.
    let mut official_profiles: Vec<(String, RuntimeId)> = Vec::new();
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
        official_profiles.push((name.clone(), head.base.clone()));
        if head.pin().is_none() {
            unpinned.push(Unpinned {
                name,
                base: head.base,
                text: source,
            });
        }
    }
    let homes = integration_homes(profiles, launch)?;
    for runtime in crate::integration::hook_runtime_ids() {
        for profile in integration_homes_of(&homes, &runtime, &official_profiles) {
            report.integrations.push(IntegrationAction {
                runtime: runtime.as_str().to_owned(),
                profile: profile.clone(),
            });
            uses.add(&runtime, UseReason::Integration);
        }
    }
    for runtime in read_store(&inputs.store_path)?.runtimes {
        uses.add(&runtime, UseReason::Session);
    }
    Ok(Gathered { uses, unpinned })
}

/// The config homes of `runtime` that hold its daemon-installed hooks, as the
/// profile each one is selected by (`None` for the runtime's default home).
///
/// A profile home that resolves to the default home's directory is the same
/// home and is reported once, under the default entry.
fn integration_homes_of(
    homes: &crate::integration::ConfigHomes,
    runtime: &RuntimeId,
    official_profiles: &[(String, RuntimeId)],
) -> Vec<Option<String>> {
    let default = inspect_home(homes, runtime, None);
    let mut found = Vec::new();
    if default.holds_hooks() {
        found.push(None);
    }
    for (name, base) in official_profiles {
        if base != runtime {
            continue;
        }
        let home = inspect_home(homes, runtime, Some(name));
        if !home.holds_hooks() {
            continue;
        }
        if default.same_home_as(&home) {
            continue;
        }
        found.push(Some(name.clone()));
    }
    found
}

/// What the managed hooks of one config home look like.
enum Inspection {
    /// Managed assets or registrations are present (even if outdated).
    Installed(Vec<String>),
    /// No managed asset is present; the expected paths name where they go.
    NotInstalled(Vec<String>),
    /// The home could not be inspected.
    Unreadable,
}

impl Inspection {
    /// A home that cannot be inspected counts as holding hooks: the post-ready
    /// reinstall then runs and reports its own failure, where an absent answer
    /// would hide the hooks from the operator.
    fn holds_hooks(&self) -> bool {
        !matches!(self, Self::NotInstalled(_))
    }

    /// Whether both inspections name the same managed asset paths, which means
    /// both selections resolve to one home directory.
    fn same_home_as(&self, other: &Self) -> bool {
        match (self.expected_paths(), other.expected_paths()) {
            (Some(mine), Some(theirs)) => !mine.is_empty() && mine == theirs,
            _ => false,
        }
    }

    fn expected_paths(&self) -> Option<&[String]> {
        match self {
            Self::Installed(paths) | Self::NotInstalled(paths) => Some(paths),
            Self::Unreadable => None,
        }
    }
}

/// Inspects the managed hooks of `runtime` in the config home its `profile`
/// selects, or in its default home when `profile` is `None`.
fn inspect_home(
    homes: &crate::integration::ConfigHomes,
    runtime: &RuntimeId,
    profile: Option<&str>,
) -> Inspection {
    let params = IntegrationStatusParams {
        agent: Some(RuntimeRef::from(runtime.clone())),
        profile: profile.map(str::to_owned),
        all_profiles: false,
    };
    match crate::integration::status_in(homes, params) {
        Ok(result) => match result.agents.into_iter().next() {
            Some(agent) if agent.state != IntegrationInstallState::NotInstalled => {
                Inspection::Installed(agent.expected_asset_paths)
            }
            Some(agent) => Inspection::NotInstalled(agent.expected_asset_paths),
            None => Inspection::NotInstalled(Vec::new()),
        },
        Err(error) => {
            warn!(
                runtime = %runtime,
                profile = ?profile,
                code = %error.code,
                detail = %error.msg,
                "the integration hooks cannot be inspected; they are listed for reinstall"
            );
            Inspection::Unreadable
        }
    }
}

/// Installs, enables and selects the planned package, records the release
/// catalog, and pins the profiles.
///
/// The package store is created here, once the plan has no refusal and has a
/// change to make.
fn apply(
    inputs: &EnsureInputs,
    existing: Option<PackageStore>,
    planned: Vec<Planned>,
    binds: Vec<Bind>,
    record: Option<VerifiedCatalog>,
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
    // The catalog trust state is recorded before any package, as an install
    // does: a later failure leaves nothing installed under a stale watermark.
    if let Some(catalog) = record {
        context
            .store
            .registry()
            .record_catalog(&catalog)
            .map_err(|error| EnsureError::Registry(error.to_string()))?;
        report.catalog = CatalogStatus::Recorded;
    }
    for plan in planned {
        let entry = package_action(&plan, true);
        if let Action::Install(prepared) = plan.action {
            install(&context, *prepared, &inputs.store_path, &profiles)?;
        }
        report.packages.push(entry);
    }
    for bind in binds {
        let retained = retained_pins(&inputs.store_path, &profiles, Some(&bind.name))?;
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

/// The official runtime a session record was launched from, if any. A session
/// launched from an installed package counts too: its frozen digest stays
/// retained and is never rebound.
fn session_runtime(binding: &crate::store::ResumeBinding) -> Option<RuntimeId> {
    let runtime = match binding.launch_binding.binding() {
        Some(launch) => launch.runtime_id.clone(),
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
/// `None` when there is no trust anchor, or the catalog is missing, unreadable
/// or refused by the verifier (including a catalog older than the recorded
/// watermark). The reason is logged; the plan then keeps the installed packages
/// and refuses only the runtimes that have none.
fn load_release<'a>(
    inputs: &'a EnsureInputs,
    catalog_state: &'a CatalogState,
) -> Option<Release<'a>> {
    let CatalogTrust::Loaded(anchor) = &inputs.catalog_trust else {
        return None;
    };
    let catalog_path = inputs.release_dir.join(CATALOG_FILE);
    let catalog_text = catalog_path.to_string_lossy().into_owned();
    let now = match unix_now() {
        Ok(now) => now,
        Err(kind) => {
            warn!(code = %ProtocolError::from(kind).code, "the release catalog is not usable");
            return None;
        }
    };
    let verified = read_regular_file(&catalog_text, catalog_bytes_limit())
        .map_err(|_failure| PackageErrorKind::SourceUnreadable)
        .and_then(|bytes| trust::verify_catalog_document(&bytes, anchor, catalog_state, now));
    match verified {
        Ok(catalog) => Some(Release {
            anchor,
            catalog_state,
            catalog,
            catalog_path: catalog_text,
            archive_dir: inputs.release_dir.join(PACKAGES_DIR),
        }),
        Err(kind) => {
            warn!(
                code = %ProtocolError::from(kind).code,
                "the release catalog is not usable"
            );
            None
        }
    }
}

/// The catalog trust state of a usable release catalog against the recorded
/// one: `current` when the recorded watermark is the catalog's sequence and the
/// catalog's revoked keys are already recorded, otherwise `record`.
fn catalog_plan(release: Option<&Release<'_>>, state: &CatalogState) -> CatalogPlan {
    let Some(release) = release else {
        return CatalogPlan {
            status: CatalogStatus::Unavailable,
            record: None,
        };
    };
    let current = state.high_water() == Some(release.catalog.sequence())
        && release
            .catalog
            .revoked_key_ids()
            .is_subset(state.revoked_key_ids());
    if current {
        CatalogPlan {
            status: CatalogStatus::Current,
            record: None,
        }
    } else {
        CatalogPlan {
            status: CatalogStatus::Record,
            record: Some(release.catalog.clone()),
        }
    }
}

/// Plans the package of every used runtime.
fn plan_packages(
    uses: &Uses,
    release: Option<&Release<'_>>,
    plan: &PlanInputs<'_>,
    report: &mut Report,
) -> Result<Vec<Planned>, EnsureError> {
    let mut planned = Vec::new();
    for (runtime, reasons) in uses.list() {
        let entry = match release {
            Some(release) => plan_runtime(plan, release, runtime, reasons, report)?,
            None => plan_installed(plan, runtime, reasons, report),
        };
        if let Some(entry) = entry {
            planned.push(entry);
        }
    }
    Ok(planned)
}

/// Plans a runtime without a usable release catalog: the installed package
/// that serves it keeps serving it, when its root verifies. A runtime nothing
/// serves, or whose package does not verify, is refused.
fn plan_installed(
    plan: &PlanInputs<'_>,
    runtime: RuntimeId,
    reasons: Vec<UseReason>,
    report: &mut Report,
) -> Option<Planned> {
    let served = plan
        .planning
        .map(|context| context.runtimes.resolve_id(&runtime));
    let definition = match served {
        Some(Ok(definition)) => definition,
        Some(Err(error)) => {
            report.refuse(
                runtime.as_str(),
                CODE_RELEASE_CATALOG_UNAVAILABLE,
                &format!(
                    "this release has no usable runtime catalog, and no installed package serves \
                     this runtime ({}); install the official package from a release that carries \
                     its catalog",
                    error.code
                ),
            );
            return None;
        }
        None => {
            report.refuse(
                runtime.as_str(),
                CODE_RELEASE_CATALOG_UNAVAILABLE,
                "this release has no usable runtime catalog, and no installed package serves this \
                 runtime; install the official package from a release that carries its catalog",
            );
            return None;
        }
    };
    let (identity, digest) = match definition.binding().provenance.clone() {
        protocol::BindingProvenance::Package {
            package,
            package_digest,
        } => (package, package_digest),
        protocol::BindingProvenance::Builtin { .. } => {
            report.refuse(
                runtime.as_str(),
                CODE_RELEASE_CATALOG_UNAVAILABLE,
                "this release has no usable runtime catalog, and only a built-in serves this \
                 runtime; install the official package from a release that carries its catalog",
            );
            return None;
        }
    };
    match health_of(plan, &digest) {
        Health::Present => {}
        Health::Damaged(kind) => {
            refuse_damaged(report, runtime.as_str(), kind, &digest);
            return None;
        }
        Health::Install => {
            report.refuse(
                runtime.as_str(),
                CODE_RELEASE_CATALOG_UNAVAILABLE,
                "the installed package serving this runtime does not verify, and no release \
                 catalog restores it; restore the release catalog, or remove the package",
            );
            return None;
        }
    }
    Some(Planned {
        runtime,
        reasons,
        identity,
        digest,
        definition: (*definition).clone(),
        action: Action::Kept,
    })
}

/// Plans one official runtime with a usable release catalog: its one release
/// package for this platform, verified, and whether the host already has it
/// selected and healthy.
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
    let health = health_of(plan, &prepared.digest);
    if let Health::Damaged(kind) = health {
        refuse_damaged(report, &subject, kind, &prepared.digest);
        return Ok(None);
    }
    if !matches!(health, Health::Present) {
        if let Some(kind) = claim_refusal(plan, &prepared) {
            report.refuse(
                &subject,
                &ProtocolError::from(kind).code,
                "the release package cannot take this runtime id alongside the installed runtimes",
            );
            return Ok(None);
        }
        // The apply judges the retained set only where it selects the package:
        // a package that is already the selected version is only enabled, and
        // a new or unselected one is selected over every retained digest.
        if selection_pending(plan, &prepared.identity.id, &prepared.digest)
            && coexistence_blocked(plan, &prepared)
        {
            report.refuse(
                &subject,
                &ProtocolError::from(PackageErrorKind::IntegrationIncompatible).code,
                "a retained version of this package has an incompatible integration",
            );
            return Ok(None);
        }
    }
    let identity = prepared.identity.clone();
    let digest = prepared.digest.clone();
    let definition = prepared.definition.clone();
    let action = if matches!(health, Health::Present) {
        Action::Present
    } else {
        Action::Install(Box::new(prepared))
    };
    Ok(Some(Planned {
        runtime,
        reasons,
        identity,
        digest,
        definition,
        action,
    }))
}

/// Whether the apply selects `prepared`: it is not the selected version of its
/// package yet, so the selecting call judges the retained set.
fn selection_pending(plan: &PlanInputs<'_>, id: &PackageId, digest: &PackageDigest) -> bool {
    plan.state.and_then(|state| state.selected(id)) != Some(digest)
}

/// Whether a retained version of the package `prepared` would make a profile
/// bind refuse it (`ensure_coexists`). The same test runs in the plan, for
/// installed packages too, so a dry run refuses what an apply would.
fn coexistence_blocked(plan: &PlanInputs<'_>, prepared: &Prepared) -> bool {
    let (Some(context), Some(state)) = (plan.planning, plan.state) else {
        return false;
    };
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
            runtime = %prepared.definition.runtime_id(),
            "the release package cannot be selected next to a retained version"
        );
    }
    blocking.is_some()
}

/// Classifies the installed release package `digest` against the registry.
///
/// A recorded package whose root is missing is restored from the archive. A
/// root that is present but altered is not: the install refuses it, so the
/// plan reports it as damaged before anything changes.
fn health_of(plan: &PlanInputs<'_>, digest: &PackageDigest) -> Health {
    match (plan.planning, plan.state) {
        (Some(context), Some(state)) => match state.package(digest) {
            None => Health::Install,
            Some(record) => match context.store.read_definition(digest, record.identity()) {
                Ok(_definition) => {
                    let serving = record.enabled()
                        && state.selected(&record.identity().id) == Some(digest)
                        && prove_loadable(context, digest).is_ok();
                    if serving {
                        Health::Present
                    } else {
                        Health::Install
                    }
                }
                Err(PackageRejection::Root(VerifyError::RootMissing)) => Health::Install,
                Err(rejection) => Health::Damaged(rejection_kind(&rejection)),
            },
        },
        _ => Health::Install,
    }
}

/// Plans the pins of the unpinned profiles, validating each rewritten profile
/// against the planned package definition the way a bind does, and running the
/// coexistence check a bind runs.
fn plan_binds(
    profiles: &ProfileRegistry,
    plan: &PlanInputs<'_>,
    store_path: &Path,
    unpinned: &[Unpinned],
    planned: &[Planned],
    report: &mut Report,
) -> Result<Vec<Bind>, EnsureError> {
    let targets: Vec<&Unpinned> = unpinned
        .iter()
        .filter(|profile| planned.iter().any(|entry| entry.runtime == profile.base))
        .collect();
    // The apply rewrites the agents directory through the owner-safety check
    // this opens it with, so a directory that fails it is refused here.
    if !targets.is_empty() && profiles.open_for_bind().is_err() {
        for target in targets {
            report.refuse(
                &target.name,
                CODE_PROFILE_NOT_MIGRATABLE,
                "the agent profile directory cannot be opened for a rewrite; fix its owner and \
                 permissions, then run the upgrade again",
            );
        }
        return Ok(Vec::new());
    }
    let mut binds = Vec::new();
    for profile in unpinned {
        let Some(entry) = planned.iter().find(|entry| entry.runtime == profile.base) else {
            continue;
        };
        // The reference this profile replaces is not a reference to compare
        // with, so the set is the one the apply's bind uses.
        let retained = retained_pins(store_path, profiles, Some(&profile.name))?;
        if let Some(kind) = bind_coexistence(plan, entry, &retained) {
            report.refuse(
                &profile.name,
                &ProtocolError::from(kind).code,
                "an installed version of this package is not compatible with a retained version",
            );
            continue;
        }
        let pin = Pin {
            package: entry.identity.id.clone(),
            digest: entry.digest.clone(),
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
            &Arc::new(entry.definition.clone()),
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
            digest: entry.digest.clone(),
        });
    }
    Ok(binds)
}

/// The coexistence refusal a bind of `entry` meets, as `ensure_coexists` judges
/// it. An installed, selected package is judged here; a package the apply still
/// installs is judged by [`coexistence_blocked`] before the bind is planned.
fn bind_coexistence(
    plan: &PlanInputs<'_>,
    entry: &Planned,
    retained: &RetainedDigests,
) -> Option<PackageErrorKind> {
    // A pending selection is judged over the global set by the install-time
    // check, which covers this set: the global set is a superset of it.
    if selection_pending(plan, &entry.identity.id, &entry.digest) {
        return None;
    }
    let (Some(context), Some(state)) = (plan.planning, plan.state) else {
        return None;
    };
    // The apply restores a missing root of the selected candidate before it
    // binds, so the candidate's integration is the one its archive declares,
    // which is the one its restored root reads. The retained versions are read
    // as they are, a missing root stays unresolvable.
    let integration = declared_by(&entry.definition);
    conflict(
        context,
        state,
        retained,
        (&entry.digest, &entry.identity.id),
        &integration,
    )
    .map(|_| PackageErrorKind::IntegrationIncompatible)
}

/// The claim refusal an install of `prepared` meets against the installed
/// runtimes, as the install judges it.
fn claim_refusal(plan: &PlanInputs<'_>, prepared: &Prepared) -> Option<PackageErrorKind> {
    let context = plan.planning?;
    check_claim(
        &context.runtimes,
        &prepared.definition,
        &prepared.identity.id,
        prepared.authority,
    )
    .err()
}

/// Refuses a release package whose installed root is present but damaged, with
/// the operator's next step.
fn refuse_damaged(
    report: &mut Report,
    subject: &str,
    kind: PackageErrorKind,
    digest: &PackageDigest,
) {
    report.refuse(
        subject,
        &ProtocolError::from(kind).code,
        &format!(
            "the installed package root of this runtime is altered or damaged; remove it with \
             `pohunek plugin uninstall --digest {} --remove-modified --yes`, then run the \
             upgrade again",
            digest.as_str()
        ),
    );
}

/// Installs, enables and selects the planned package, under the registry's own
/// lock for each change.
///
/// An absent record is installed with both flags set. A record that is kept
/// keeps its recorded flags; its root is restored from the archive when it is
/// missing or altered, and only what is not yet so is enabled or selected.
fn install(
    context: &Context,
    prepared: Prepared,
    store_path: &Path,
    profiles: &ProfileRegistry,
) -> Result<(), EnsureError> {
    let retained = retained_pins(store_path, profiles, None)?;
    let state = read_state(context).map_err(kind_error)?;
    let digest = prepared.digest.clone();
    let definition = prepared.definition.clone();
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
    if prove_loadable(context, &digest).is_err() {
        // The record keeps its flags: the install restores only the root.
        let result = commit_install(
            context,
            prepared,
            InstallPlan {
                enable: false,
                select: false,
                dry_run: false,
            },
            &retained,
        )
        .map_err(kind_error)?;
        if result.status != PackageInstallStatus::RootRestored {
            return Err(EnsureError::Registry(
                "the release package root was not restored".to_owned(),
            ));
        }
    }
    if !record.enabled() {
        prove_loadable(context, &digest).map_err(kind_error)?;
        context
            .store
            .registry()
            .set_enabled(&digest, true)
            .map_err(|error| EnsureError::Registry(error.to_string()))?;
    }
    if state.selected(&record.identity().id) != Some(&digest) {
        ensure_selectable(context, &retained, &digest, &definition).map_err(kind_error)?;
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

/// The package status of one planned runtime, after the apply when `applied`.
fn package_action(plan: &Planned, applied: bool) -> PackageAction {
    let status = match &plan.action {
        Action::Present => PackageStatus::Present,
        Action::Kept => PackageStatus::Kept,
        Action::Install(_) if applied => PackageStatus::Installed,
        Action::Install(_) => PackageStatus::Install,
    };
    PackageAction {
        runtime: plan.runtime.as_str().to_owned(),
        package: plan.identity.id.as_str().to_owned(),
        version: plan.identity.version.as_str().to_owned(),
        digest: plan.digest.as_str().to_owned(),
        status,
        reasons: plan.reasons.clone(),
    }
}

/// The base environment the integration status reads the config homes with:
/// the environment a launch of the installed daemon inherits.
fn integration_homes(
    profiles: &ProfileRegistry,
    launch: &crate::runtime::LaunchEnvironment,
) -> Result<crate::integration::ConfigHomes, EnsureError> {
    let base = launch
        .base()
        .map_err(|error| EnsureError::Registry(error.to_string()))?;
    Ok(crate::integration::ConfigHomes::new(profiles.clone(), base))
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
