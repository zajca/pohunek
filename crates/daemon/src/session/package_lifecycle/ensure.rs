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

use super::super::packages::{installed_digests, RetainedPins};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use package::catalog_state::CatalogState;
use package::registry::{PackageSource, RegistryState};
use package::verify::VerifyError;
use package::{PackageDigest, VerifiedCatalog, VerifiedEntry, MAX_CATALOG_BYTES};
use pohunek_service_config::runtimes::{
    CatalogStatus, EnsureReport, Gap, IntegrationAction, PackageAction, PackageStatus,
    ProfileAction, ProfileStatus, Refusal, SelectedPackage, UseReason, CODE_GAP_AGENT_NOT_ON_PATH,
    CODE_GAP_CONFIG_HOME_MISSING, CODE_GAP_CONFIG_HOME_SYMLINK,
    CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING, CODE_GAP_SERVED_BY_OTHER_PACKAGE,
    CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST, CODE_GAP_UPDATE_VERSION_UNORDERED,
    CODE_PACKAGE_ARCHIVE_INVALID, CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM,
    CODE_PINNED_DIFFERS_FROM_KEPT_SELECTION, CODE_PINNED_TARGET_NOT_OFFICIAL,
    CODE_PINNED_TO_NEWER_VERSION, CODE_PINNED_TO_NON_OFFICIAL_PACKAGE,
    CODE_PINNED_TO_OTHER_PACKAGE, CODE_PINNED_TO_SAME_VERSION, CODE_PINNED_VERSION_UNKNOWN,
    CODE_PROFILE_NOT_MIGRATABLE, CODE_RELEASE_CATALOG_UNAVAILABLE, CODE_SERVICE_CONFIG_UNREADABLE,
    ENSURE_REPORT_VERSION,
};
use pohunek_worker_protocol::BaseEnv;
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
    decide_claim, is_reserved, version_order, Authority, PackageRejection, PackageStore,
    RuntimeDefinition, RuntimeHost,
};
use crate::agent::{apply_pin, parse_head, Pin, ProfileRegistry, ValidatedLaunchProgram};
use crate::catalog_anchor::CatalogTrust;
use crate::integration::{config_path_kind, ConfigPath, Home};
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

/// Refusal subject of a release catalog that cannot be used by `--detect`,
/// which plans from the catalog and not from the installed runtimes alone.
const RELEASE_CATALOG_SUBJECT: &str = "release-catalog";

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
    /// The installed package that serves the runtime keeps serving it: the
    /// release has no usable catalog, or the selected version is kept over the
    /// release's.
    Kept,
}

/// The state of an installed release package the plan found.
enum Health {
    /// Enabled, selected, and its root verifies and loads.
    Present,
    /// Not installed, not selected, not enabled, or its root is missing: the
    /// archive installs, enables and selects it again. A selected package the
    /// operator disabled never reaches this plan.
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
    /// The newer official package the registry selects, which installing and
    /// selecting this release package replaces.
    replaces_newer: Option<SelectedPackage>,
    /// The release's version when the entry keeps the selected package (newer
    /// than the release's, or with no order against it) instead of installing
    /// the release's.
    release_version: Option<String>,
}

/// One host profile over an official runtime that pins no package.
struct Unpinned {
    name: String,
    base: RuntimeId,
    text: String,
}

/// One host profile over an official runtime that pins a package.
struct Pinned {
    name: String,
    base: RuntimeId,
    pin: Pin,
    text: String,
}

/// The package pin a repin moves a profile off.
struct Previous {
    digest: PackageDigest,
    version: String,
}

/// A host profile to pin to the package planned for its base runtime: an
/// unpinned profile, or a pinned one that moves from an older official version.
struct Bind {
    name: String,
    base: RuntimeId,
    digest: PackageDigest,
    /// Version of the planned package.
    version: String,
    /// The profile text with its package pin, as the apply writes it.
    text: String,
    /// The pin a repin moves the profile off; `None` for an unpinned profile.
    previous: Option<Previous>,
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

    /// Makes `runtime` one the plan judges, without a reason of its own.
    fn touch(&mut self, runtime: &RuntimeId) {
        self.0
            .entry(runtime.as_str().to_owned())
            .or_insert_with(|| (runtime.clone(), Vec::new()));
    }

    fn list(&self) -> Vec<(RuntimeId, Vec<UseReason>)> {
        self.0.values().cloned().collect()
    }
}

/// A host profile over an official runtime, with the package digest it pins.
struct OfficialProfile {
    name: String,
    base: RuntimeId,
    digest: Option<PackageDigest>,
}

/// What [`gather`] found.
struct Gathered {
    uses: Uses,
    unpinned: Vec<Unpinned>,
    pinned: Vec<Pinned>,
    /// Every host profile over an official runtime, pinned or not.
    official: Vec<OfficialProfile>,
}

/// The packages that host profiles and session records pin, by source.
struct Pins {
    /// Each host profile with the digest it pins.
    profiles: Vec<(String, PackageDigest)>,
    /// The session records, each with the official runtime it launched and the
    /// digest it pins.
    sessions: Vec<SessionRecord>,
}

/// Reads the pins of the host profiles and the session records.
fn read_pins(store_path: &Path, profiles: &ProfileRegistry) -> Result<Pins, EnsureError> {
    Ok(Pins {
        profiles: profiles
            .pinning_profiles(None)
            .map_err(EnsureError::Profiles)?,
        sessions: read_store(store_path)?.records,
    })
}

/// What the plan reads from the registries and the store.
struct PlanInputs<'a> {
    /// The registry as it is; `None` without a package store, where nothing
    /// is installed.
    state: Option<&'a RegistryState>,
    /// The context a conflict check and a health check read installed packages
    /// through.
    planning: Option<&'a Context>,
    /// The digests a session or a profile still pins, and whether a store line
    /// that may pin one cannot be read.
    retained: &'a RetainedPins,
    /// Which profiles and sessions pin them.
    pins: &'a Pins,
    /// The launch base environment the config homes of the planned definitions
    /// are resolved with, as a launch resolves them.
    base: &'a BaseEnv,
}

impl PlanInputs<'_> {
    /// Whether nothing can pin `digest`: no readable record pins it and no
    /// unreadable store line may. Only then is an uninstall of it possible.
    fn removable(&self, digest: &PackageDigest) -> bool {
        !self.retained.known().contains(digest) && !self.retained.is_uncertain()
    }

    /// The sentence that explains a refusal judged over every installed
    /// version, empty while no store line is unreadable.
    fn unreadable_note(&self) -> &'static str {
        if self.retained.is_uncertain() {
            UNREADABLE_PIN_NOTE
        } else {
            ""
        }
    }
}

/// Appended to a refusal that every installed version took part in because a
/// metadata-store line that may pin a package cannot be read.
const UNREADABLE_PIN_NOTE: &str = "; a metadata-store line that cannot be read may pin a version, so every installed version counts: run `pohunek store rejected list`";

/// The step that releases the pin an unreadable metadata-store line may hold,
/// ending in `; then `.
const UNREADABLE_PIN_STEP: &str = "a metadata-store line that cannot be read may pin it, so the registry refuses its removal until the line is set aside: run `pohunek store rejected list`, then `pohunek store rejected set-aside <content-hash>`; then ";

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
    /// Whether the plan detects the runtimes on the search path; a condition
    /// that is a refusal otherwise is then a gap.
    detect: bool,
    catalog: CatalogStatus,
    packages: Vec<PackageAction>,
    profiles: Vec<ProfileAction>,
    integrations: Vec<IntegrationAction>,
    refusals: Vec<Refusal>,
    gaps: Vec<Gap>,
    /// Runtimes the detecting plan dropped: nothing uses them and their
    /// program is off the search path.
    undetected: Vec<String>,
    /// The base of every profile, official or not: a profile over a runtime
    /// keeps that runtime's not-serving gap even when the program is off the
    /// search path.
    profiled: Vec<String>,
}

impl Report {
    fn new(inputs: &EnsureInputs, detect: bool) -> Self {
        Self {
            version: ENSURE_REPORT_VERSION,
            daemon: inputs.daemon_version.clone(),
            dry_run: inputs.dry_run,
            detect,
            catalog: CatalogStatus::Unavailable,
            packages: Vec::new(),
            profiles: Vec::new(),
            integrations: Vec::new(),
            refusals: Vec::new(),
            gaps: Vec::new(),
            undetected: Vec::new(),
            profiled: Vec::new(),
        }
    }

    /// Drops the not-serving gaps of the runtimes the detecting plan dropped,
    /// which a step before the use of the runtimes was known may have reported.
    fn forget_undetected_gaps(&mut self) {
        let undetected = std::mem::take(&mut self.undetected);
        let profiled = std::mem::take(&mut self.profiled);
        self.gaps.retain(|gap| {
            gap.code != CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING
                || !undetected.contains(&gap.runtime)
                || profiled.contains(&gap.runtime)
        });
    }

    fn refuse(&mut self, subject: &str, code: &str, detail: &str) {
        self.refusals.push(Refusal {
            subject: subject.to_owned(),
            code: code.to_owned(),
            detail: detail.to_owned(),
        });
    }

    fn gap(&mut self, runtime: &str, code: &str, detail: &str) {
        self.gaps.push(Gap {
            runtime: runtime.to_owned(),
            code: code.to_owned(),
            detail: detail.to_owned(),
        });
    }

    /// Records a condition as a gap under `--detect`, and as a refusal
    /// otherwise, so the upgrade's plan does not change.
    fn gap_or_refuse(&mut self, subject: &str, gap_code: &str, refusal_code: &str, detail: &str) {
        if self.detect {
            self.gap(subject, gap_code, detail);
        } else {
            self.refuse(subject, refusal_code, detail);
        }
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
            gaps: self.gaps,
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
    /// The host profiles the apply rewrites, with the text it writes for each.
    pub profiles: Vec<PlannedProfile>,
}

/// A host profile the apply rewrites with its package pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedProfile {
    /// Profile name.
    pub name: String,
    /// The runtime the profile is based on, which the planned packages serve.
    pub base: RuntimeId,
    /// The profile file text after the pin is applied.
    pub text: String,
}

/// Computes the plan without changing anything, whatever `inputs.dry_run` says.
///
/// # Errors
///
/// Returns [`EnsureError`] when a store cannot be read. A refusal is a report.
pub fn plan_official_runtimes(inputs: &EnsureInputs) -> Result<OfficialPlan, EnsureError> {
    plan_with(inputs, false)
}

/// Computes the plan of [`plan_official_runtimes`] and also plans every runtime
/// of the release catalog whose program is on the search path, as the
/// `--detect` setup plan does. Nothing is changed.
///
/// Runtimes that cannot be planned are reported as gaps, not refusals, except
/// where the archive or the catalog fails its checks.
///
/// # Errors
///
/// Returns [`EnsureError`] when a store cannot be read. A refusal is a report.
pub fn detect_official_runtimes(inputs: &EnsureInputs) -> Result<OfficialPlan, EnsureError> {
    plan_with(inputs, true)
}

fn plan_with(inputs: &EnsureInputs, detect: bool) -> Result<OfficialPlan, EnsureError> {
    let computed = compute(inputs, detect)?;
    let definitions = computed
        .planned
        .iter()
        .map(|plan| plan.definition.clone())
        .collect();
    let profiles = computed
        .binds
        .iter()
        .map(|bind| PlannedProfile {
            name: bind.name.clone(),
            base: bind.base.clone(),
            text: bind.text.clone(),
        })
        .collect();
    Ok(OfficialPlan {
        report: computed.dry_report(true),
        definitions,
        profiles,
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
    let computed = compute(inputs, false)?;
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
        // The kept profiles are already in the report, as the planning found them.
        report
            .profiles
            .extend(binds.iter().map(|bind| profile_action(bind, false)));
        report.finish()
    }
}

/// Reads the sources and plans every change, without writing.
fn compute(inputs: &EnsureInputs, detect: bool) -> Result<Computed, EnsureError> {
    let launch = match &inputs.environment {
        Ok(launch) => launch,
        Err(detail) => {
            let mut report = Report::new(inputs, detect);
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

    let mut report = Report::new(inputs, detect);
    let base = launch
        .base()
        .map_err(|error| EnsureError::Registry(error.to_string()))?;
    let retained = retained_pins(&inputs.store_path, &profiles, None)?;
    let pins = read_pins(&inputs.store_path, &profiles)?;
    let plan = PlanInputs {
        state: state.as_ref(),
        planning: planning.as_ref(),
        retained: &retained,
        pins: &pins,
        base: &base,
    };
    let release = load_release(inputs, &catalog_state);
    let managed = managed_runtimes(release.as_ref(), &plan, &mut report)?;
    let Gathered {
        mut uses,
        unpinned,
        pinned,
        official,
    } = gather(inputs, launch, &profiles, &managed, &mut report)?;
    for runtime in &managed {
        uses.add(runtime, UseReason::Installed);
    }
    let catalog = catalog_plan(release.as_ref(), &catalog_state);
    report.catalog = catalog.status;
    if detect {
        detect_targets(release.as_ref(), &mut uses, &mut report)?;
    }
    let planned = plan_packages(&uses, release.as_ref(), &plan, &mut report)?;
    report.forget_undetected_gaps();
    settle_same_version_gaps(&mut report, &planned, &unpinned);
    let binds = plan_binds(
        &profiles,
        &plan,
        &inputs.store_path,
        &unpinned,
        &pinned,
        &planned,
        &mut report,
    )?;
    if detect {
        let integrations =
            detected_integrations(&profiles, &base, &planned, &binds, &official, &mut report);
        report.integrations = integrations;
    }
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
    managed: &[RuntimeId],
    report: &mut Report,
) -> Result<Gathered, EnsureError> {
    let mut uses = Uses::default();
    let mut unpinned = Vec::new();
    let mut pinned = Vec::new();
    // Every profile over an official base, pinned or not, with its base.
    let mut official_profiles: Vec<OfficialProfile> = Vec::new();
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
        report.profiled.push(head.base.as_str().to_owned());
        let managed_base = managed.contains(&head.base);
        if !is_official(&head.base) && !managed_base {
            continue;
        }
        // A pinned profile still uses its runtime: the release package must be
        // present for it, and a second run then reports it as present.
        uses.add(&head.base, UseReason::Profile);
        official_profiles.push(OfficialProfile {
            name: name.clone(),
            base: head.base.clone(),
            digest: head.pin().map(|pin| pin.digest),
        });
        match head.pin() {
            // An unpinned profile over a managed runtime follows the selection
            // of its package; the step binds only the reserved runtimes.
            None if managed_base => {}
            None => unpinned.push(Unpinned {
                name,
                base: head.base,
                text: source,
            }),
            Some(pin) => pinned.push(Pinned {
                name,
                base: head.base,
                pin,
                text: source,
            }),
        }
    }
    let homes = integration_homes(profiles, launch)?;
    for runtime in crate::integration::hook_runtime_ids() {
        for profile in integration_homes_of(&homes, &runtime, &official_profiles) {
            // A detecting plan lists the homes its hooks would reach instead,
            // see `detected_integrations`.
            if !report.detect {
                report.integrations.push(IntegrationAction {
                    runtime: runtime.as_str().to_owned(),
                    profile: profile.clone(),
                });
            }
            uses.add(&runtime, UseReason::Integration);
        }
    }
    let store = read_store(&inputs.store_path)?;
    for runtime in store.runtimes() {
        uses.add(runtime, UseReason::Session);
    }
    Ok(Gathered {
        uses,
        unpinned,
        pinned,
        official: official_profiles,
    })
}

/// The config homes of `runtime` that hold its daemon-installed hooks, as the
/// profile each one is selected by (`None` for the runtime's default home).
///
/// A profile home that resolves to the default home's directory is the same
/// home and is reported once, under the default entry.
fn integration_homes_of(
    homes: &crate::integration::ConfigHomes,
    runtime: &RuntimeId,
    official_profiles: &[OfficialProfile],
) -> Vec<Option<String>> {
    let default = inspect_home(homes, runtime, None);
    let mut found = Vec::new();
    if default.holds_hooks() {
        found.push(None);
    }
    for profile in official_profiles {
        let name = &profile.name;
        if &profile.base != runtime {
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
        report.profiles.push(profile_action(&bind, true));
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

/// One session record of the metadata store.
struct SessionRecord {
    /// The record's official runtime, whatever its state.
    runtime: Option<RuntimeId>,
    /// The package digest the record pins.
    pin: Option<PackageDigest>,
}

/// What the metadata store's records use and pin.
struct StoreView {
    records: Vec<SessionRecord>,
    /// Lines that may pin a package and no loader accepts. They stay in the
    /// store and are never searched for a digest, so the pins above are
    /// incomplete while one exists.
    unreadable_lines: usize,
}

impl StoreView {
    /// The official runtime of every record.
    fn runtimes(&self) -> impl Iterator<Item = &RuntimeId> {
        self.records
            .iter()
            .filter_map(|record| record.runtime.as_ref())
    }

    /// The package digests the records pin.
    fn pins(&self) -> impl Iterator<Item = &PackageDigest> {
        self.records.iter().filter_map(|record| record.pin.as_ref())
    }
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
    let records = bindings
        .iter()
        .map(|binding| SessionRecord {
            runtime: session_runtime(binding),
            pin: match binding
                .launch_binding
                .binding()
                .map(|launch| &launch.provenance)
            {
                Some(protocol::BindingProvenance::Package { package_digest, .. }) => {
                    Some(package_digest.clone())
                }
                Some(protocol::BindingProvenance::Builtin { .. }) | None => None,
            },
        })
        .collect();
    let unreadable_lines = dry.rejected.iter().filter(|line| line.may_pin()).count();
    Ok(StoreView {
        records,
        unreadable_lines,
    })
}

/// Digests every installed package a session or profile still pins, excluding
/// the pin of the profile `skip` when one is given. The offline step reads the
/// same sources as the daemon's `retained_package_pins`, which an uninstall
/// judges by: a store line that may pin a package and cannot be read is never
/// searched for a digest, and while one exists the gate set also holds every
/// installed digest.
pub(in crate::session) fn retained_pins(
    store_path: &Path,
    profiles: &ProfileRegistry,
    skip: Option<&str>,
) -> Result<RetainedPins, EnsureError> {
    let mut retained = profiles
        .pinned_digests_excluding(skip)
        .map_err(EnsureError::Profiles)?;
    let store = read_store(store_path)?;
    retained.extend(store.pins().cloned());
    let installed = if store.unreadable_lines > 0 {
        warn!(
            unreadable_lines = store.unreadable_lines,
            "package checks judge every installed package: the metadata store holds a line that may pin a package and cannot be read"
        );
        installed_digests(profiles.runtimes())
            .map_err(|error| EnsureError::Registry(error.to_string()))?
    } else {
        Vec::new()
    };
    Ok(RetainedPins::with_unreadable(
        retained,
        store.unreadable_lines,
        installed,
    ))
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

/// The `plugin update` command that installs the release's package of
/// `runtime` from the extracted release archive.
fn update_command(package: &PackageId, runtime: &RuntimeId) -> String {
    format!(
        "pohunek plugin update {package} runtime/{PACKAGES_DIR}/{runtime}{ARCHIVE_SUFFIX} \
         --catalog runtime/{CATALOG_FILE} --yes"
    )
}

/// The `plugin install` command that installs the release's package of
/// `runtime` from the extracted release archive, enabled and selected.
fn install_command(runtime: &RuntimeId) -> String {
    format!(
        "pohunek plugin install runtime/{PACKAGES_DIR}/{runtime}{ARCHIVE_SUFFIX} \
         --catalog runtime/{CATALOG_FILE} --yes"
    )
}

/// The runtimes that are not reserved (Pi) whose official package the step
/// keeps current: one is installed from the official catalog, selected and
/// enabled, and the release catalog lists a package of the same id for it.
///
/// The step never installs a package that is absent and never selects or
/// enables one the operator turned off. An official package that is installed
/// but not serving, or that the release does not list for this platform, is
/// reported as a gap with the command that updates, enables or diagnoses it by
/// hand, by its state. Without a usable
/// release catalog every such serving package is kept as it is.
fn managed_runtimes(
    release: Option<&Release<'_>>,
    plan: &PlanInputs<'_>,
    report: &mut Report,
) -> Result<Vec<RuntimeId>, EnsureError> {
    let (Some(state), Some(context)) = (plan.state, plan.planning) else {
        return Ok(Vec::new());
    };
    let platform = trust::host_platform().map_err(kind_error)?;
    let core = trust::core_version().map_err(kind_error)?;
    let listed = |runtime: &RuntimeId| {
        release
            .into_iter()
            .flat_map(|release| release.catalog.entries())
            .filter(|entry| {
                entry.runtime_id() == runtime
                    && entry.supports_platform(&platform)
                    && entry.supports_core(&core)
            })
            .collect::<Vec<_>>()
    };
    let mut serving: BTreeMap<String, (RuntimeId, PackageId)> = BTreeMap::new();
    for record in state.packages() {
        let id = &record.identity().id;
        if record.source() != PackageSource::Official
            || !record.enabled()
            || state.selected(id) != Some(record.digest())
        {
            continue;
        }
        // The descriptor names the runtime. A root that does not load still
        // serves the runtime the release catalog lists the package for, so the
        // plan reports its damage instead of skipping it.
        let runtime = context
            .store
            .read_definition(record.digest(), record.identity())
            .map(|definition| definition.runtime_id().clone())
            .ok()
            .or_else(|| {
                release.and_then(|release| {
                    release
                        .catalog
                        .entries()
                        .iter()
                        .find(|entry| entry.package_id() == id)
                        .map(|entry| entry.runtime_id().clone())
                })
            });
        if let Some(runtime) = runtime.filter(|runtime| !is_reserved(runtime)) {
            serving.insert(runtime.as_str().to_owned(), (runtime, id.clone()));
        }
    }
    let Some(release) = release else {
        return Ok(serving.into_values().map(|(runtime, _)| runtime).collect());
    };
    let mut managed = Vec::new();
    for (runtime, package) in serving.values() {
        match listed(runtime).as_slice() {
            [entry] if entry.package_id() == package => managed.push(runtime.clone()),
            _ => report.gap(
                runtime.as_str(),
                CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM,
                &format!(
                    "the release catalog lists no single package {package} of this runtime for \
                     this platform, so the installed package stays as it is; update it from a \
                     release that lists it: {}",
                    update_command(package, runtime)
                ),
            ),
        }
    }
    for entry in release.catalog.entries() {
        let runtime = entry.runtime_id();
        if is_reserved(runtime)
            || !entry.supports_platform(&platform)
            || !entry.supports_core(&core)
            || serving.contains_key(runtime.as_str())
        {
            continue;
        }
        let installed = state.packages().iter().any(|record| {
            record.source() == PackageSource::Official
                && record.identity().id == *entry.package_id()
        });
        if installed {
            let detail = unserved_package_detail(plan, state, entry);
            report.gap(
                runtime.as_str(),
                CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING,
                &detail,
            );
        }
    }
    Ok(managed)
}

/// The gap detail for the release catalog's `entry`, whose official package is
/// installed but neither selected nor enabled: the state of a disabled
/// selection, the hint of a recorded holder, or the update that selects it.
fn unserved_package_detail(
    plan: &PlanInputs<'_>,
    state: &RegistryState,
    entry: &VerifiedEntry,
) -> String {
    let runtime = entry.runtime_id();
    if let Some(disabled) = disabled_selected_package(state, entry.package_id()) {
        return disabled_package_detail(
            entry.package_id(),
            &disabled,
            disabled_state(plan, &disabled, runtime),
        );
    }
    let blocked = Blocked {
        id: entry.package_id(),
        version: entry.version().as_str(),
        digest: entry.digest(),
        runtime,
    };
    match held_selection(plan, &blocked) {
        Some((holder, held)) if !matches!(held, Held::Serving) => {
            held_hint(plan, &blocked, &holder, &held, false, &[]).detail
        }
        _ => format!(
            "an official package of this runtime is installed but not selected and enabled, so \
             the upgrade leaves it as you set it; update it with: {}",
            update_command(entry.package_id(), runtime)
        ),
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
            None => plan_installed(plan, runtime, reasons, report, false),
        };
        if let Some(entry) = entry {
            planned.push(entry);
        }
    }
    Ok(planned)
}

/// Plans a runtime over the installed package that serves it, which keeps
/// serving it when its root verifies: the release has no usable catalog, or
/// the selected version is kept (`kept_selection`). A runtime nothing serves,
/// or whose package does not verify, is refused.
fn plan_installed(
    plan: &PlanInputs<'_>,
    runtime: RuntimeId,
    reasons: Vec<UseReason>,
    report: &mut Report,
    kept_selection: bool,
) -> Option<Planned> {
    // The refusal names why the installed package is the one that plans.
    let why = if kept_selection {
        "the selected package is kept"
    } else {
        "this release has no usable runtime catalog"
    };
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
                    "{why}, and no installed package serves this runtime ({}); install the \
                     official package from a release that carries its catalog",
                    error.code
                ),
            );
            return None;
        }
        None => {
            report.refuse(
                runtime.as_str(),
                CODE_RELEASE_CATALOG_UNAVAILABLE,
                &format!(
                    "{why}, and no installed package serves this runtime; install the official \
                     package from a release that carries its catalog"
                ),
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
                &format!(
                    "{why}, and only a built-in serves this runtime; install the official package \
                     from a release that carries its catalog"
                ),
            );
            return None;
        }
    };
    match health_of(plan, &digest) {
        Health::Present => {}
        Health::Damaged(kind) => {
            refuse_damaged(report, runtime.as_str(), kind, &identity.id, &digest);
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
        replaces_newer: None,
        release_version: None,
    })
}

/// Plans one official runtime with a usable release catalog: its one release
/// package for this platform, verified, and whether the host already has it
/// selected and healthy.
fn plan_runtime(
    plan: &PlanInputs<'_>,
    release: &Release<'_>,
    runtime: RuntimeId,
    mut reasons: Vec<UseReason>,
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
        report.gap_or_refuse(
            &subject,
            CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM,
            CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM,
            "the release catalog lists no single package of this runtime for this platform",
        );
        return Ok(None);
    }
    let Some(prepared) = prepare_release_archive(release, &runtime, report) else {
        return Ok(None);
    };
    if prepared.definition.runtime_id() != &runtime {
        report.refuse(
            &subject,
            CODE_PACKAGE_ARCHIVE_INVALID,
            "the release archive serves another runtime than the catalog lists it for",
        );
        return Ok(None);
    }
    let reserved = is_reserved(&runtime);
    match decide_selection(plan, &prepared, &subject, &mut reasons, report, reserved) {
        Selection::Leave => return Ok(None),
        Selection::Keep { reason } => {
            let kept = KeptSelection { reason, reserved };
            return Ok(plan_kept_selection(
                plan, &prepared, &subject, runtime, reasons, kept, report,
            ));
        }
        Selection::Replace => {}
    }
    if report.detect
        && !detect_on_search_path(
            report,
            &subject,
            prepared.definition.program().as_str(),
            &mut reasons,
        )
    {
        return Ok(None);
    }
    let health = health_of(plan, &prepared.digest);
    if let Health::Damaged(kind) = health {
        refuse_prepared_damaged(report, &subject, kind, &prepared);
        return Ok(None);
    }
    if !matches!(health, Health::Present) {
        if let Some(kind) = claim_refusal(plan, &prepared) {
            report_claim_refusal(report, &subject, kind, RELEASE_CLAIM_DETAIL);
            return Ok(None);
        }
        // The apply judges the retained set only where it selects the package:
        // a package that is already the selected version is only repaired, and
        // a new or unselected one is selected over every retained digest.
        if selection_pending(plan, &prepared.identity.id, &prepared.digest)
            && coexistence_blocked(plan, &prepared)
        {
            report.refuse(
                &subject,
                &ProtocolError::from(PackageErrorKind::IntegrationIncompatible).code,
                &format!(
                    "a retained version of this package has an incompatible integration{}",
                    plan.unreadable_note()
                ),
            );
            return Ok(None);
        }
    }
    if report.detect {
        config_home_gap(plan, &prepared.definition, report);
    }
    let identity = prepared.identity.clone();
    let digest = prepared.digest.clone();
    let definition = prepared.definition.clone();
    let replaces_newer = replaces_newer(plan, &prepared);
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
        replaces_newer,
        release_version: None,
    }))
}

/// What happens to the selected version of a runtime: under `--detect` the
/// release package's search-path gate comes first for a runtime that would be
/// left untouched, then [`selection_outcome`] decides.
fn decide_selection(
    plan: &PlanInputs<'_>,
    prepared: &Prepared,
    subject: &str,
    reasons: &mut Vec<UseReason>,
    report: &mut Report,
    reserved: bool,
) -> Selection {
    if report.detect
        && !passes_detect_gate_before_leaving(plan, prepared, subject, reasons, report, reserved)
    {
        return Selection::Leave;
    }
    selection_outcome(plan, prepared, subject, report, reserved, reasons)
}

/// Under `--detect`, a runtime the plan would leave untouched (its package is
/// disabled, or its unordered selection does not load or is altered) is first judged by the
/// release package's search-path gate, as a replacing plan is: a runtime
/// nothing uses and whose program is off the search path is dropped, not
/// reported or refused. `false` drops the runtime.
fn passes_detect_gate_before_leaving(
    plan: &PlanInputs<'_>,
    prepared: &Prepared,
    subject: &str,
    reasons: &mut Vec<UseReason>,
    report: &mut Report,
    reserved: bool,
) -> bool {
    let left = disabled_package(plan, prepared).is_some()
        || (disabled_release_package(plan, prepared, reserved).is_some()
            && (!selection_serves(plan, prepared)
                || damaged_release_package(plan, prepared, reserved).is_some()))
        || matches!(
            unordered_selection(plan, prepared),
            Some((Serving::Broken | Serving::Damaged(_), _))
        )
        || held_leaves(plan, prepared, reserved);
    !left
        || detect_on_search_path(
            report,
            subject,
            prepared.definition.program().as_str(),
            reasons,
        )
}

/// How the selected version of a runtime stays.
#[derive(Clone)]
struct KeptSelection {
    /// Why the selection stays.
    reason: KeepReason,
    /// The runtime is reserved.
    reserved: bool,
}

/// Plans a runtime whose selected version stays. A reserved runtime goes
/// through the checks of a detecting plan as for the release's package, judged
/// against the kept package's own program and config home; the unordered and
/// the same-version gaps are reported only when the keep is planned. The entry names the version the
/// release offers.
fn plan_kept_selection(
    plan: &PlanInputs<'_>,
    prepared: &Prepared,
    subject: &str,
    runtime: RuntimeId,
    reasons: Vec<UseReason>,
    kept: KeptSelection,
    report: &mut Report,
) -> Option<Planned> {
    let mut entry = plan_installed(plan, runtime, reasons, report, true)?;
    if report.detect && kept.reserved {
        if !detect_on_search_path(
            report,
            subject,
            entry.definition.program().as_str(),
            &mut entry.reasons,
        ) {
            return None;
        }
        config_home_gap(plan, &entry.definition, report);
    }
    entry.release_version = Some(prepared.identity.version.as_str().to_owned());
    match kept.reason {
        KeepReason::Unordered => report.gap(
            subject,
            CODE_GAP_UPDATE_VERSION_UNORDERED,
            "the selected version cannot be ordered against the release's, so it is kept",
        ),
        KeepReason::SameVersionOtherDigest(detail) => {
            report.gap(subject, CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST, &detail);
        }
        KeepReason::Ordered => {}
    }
    Some(entry)
}

/// Prepares the release archive of `runtime` that the catalog authorizes. A
/// failed check is a refusal of the plan.
fn prepare_release_archive(
    release: &Release<'_>,
    runtime: &RuntimeId,
    report: &mut Report,
) -> Option<Prepared> {
    let archive = release
        .archive_dir
        .join(format!("{}{ARCHIVE_SUFFIX}", runtime.as_str()));
    let archive_text = archive.to_string_lossy().into_owned();
    match prepare_official(
        release.anchor,
        release.catalog_state,
        &archive_text,
        &release.catalog_path,
    ) {
        Ok(prepared) => Some(prepared),
        Err(kind) => {
            report.refuse(
                runtime.as_str(),
                CODE_PACKAGE_ARCHIVE_INVALID,
                &kind_refusal_detail(kind),
            );
            None
        }
    }
}

/// Under `--detect`, marks the runtime detected when its `program` is on the
/// search path, and gaps it otherwise. Returns whether the runtime is still
/// planned: a runtime the host uses keeps its planned package, while one it
/// does not use is not installed for an agent that is not there.
fn detect_on_search_path(
    report: &mut Report,
    subject: &str,
    program: &str,
    reasons: &mut Vec<UseReason>,
) -> bool {
    if ValidatedLaunchProgram::resolve(program).is_some() {
        reasons.push(UseReason::Detected);
        return true;
    }
    report.gap(
        subject,
        CODE_GAP_AGENT_NOT_ON_PATH,
        &format!(
            "`{program}` is not on the search path; put the agent on it, then run setup again"
        ),
    );
    let planned = !reasons.is_empty();
    if !planned {
        report.undetected.push(subject.to_owned());
    }
    planned
}

/// Reports a release package the claim rules refuse: a conflict with another
/// package that serves the runtime id is a gap under `--detect`, every other
/// refusal stays a refusal.
fn report_claim_refusal(report: &mut Report, subject: &str, kind: PackageErrorKind, detail: &str) {
    let refusal_code = ProtocolError::from(kind).code;
    if kind == PackageErrorKind::RuntimeConflict {
        report.gap_or_refuse(
            subject,
            CODE_GAP_SERVED_BY_OTHER_PACKAGE,
            &refusal_code,
            detail,
        );
    } else {
        report.refuse(subject, &refusal_code, detail);
    }
}

/// Adds every runtime of the release catalog for this platform and core to
/// `uses`, so `--detect` judges each one. Without a usable catalog nothing can
/// be detected, which is a refusal.
fn detect_targets(
    release: Option<&Release<'_>>,
    uses: &mut Uses,
    report: &mut Report,
) -> Result<(), EnsureError> {
    let Some(release) = release else {
        report.refuse(
            RELEASE_CATALOG_SUBJECT,
            CODE_RELEASE_CATALOG_UNAVAILABLE,
            "this release has no usable runtime catalog, so the runtimes cannot be detected; \
             install the release again from a build that carries its runtime catalog",
        );
        return Ok(());
    };
    let platform = trust::host_platform().map_err(kind_error)?;
    let core = trust::core_version().map_err(kind_error)?;
    for entry in release.catalog.entries() {
        if entry.supports_platform(&platform) && entry.supports_core(&core) {
            uses.touch(entry.runtime_id());
        }
    }
    Ok(())
}

/// The installer's code for a config path it refuses, as `integration.install`
/// reports it. Only a normalized absolute path to a real directory is accepted.
const CODE_HOME_PATH_REFUSED: &str = "integration_path_untrusted";

/// How the installer's own path walk classifies a planned config home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeAdmission {
    /// A real directory the installer walks; hooks can be installed into it.
    Admitted,
    /// Not there yet; the agent creates it on its first start.
    Missing,
    /// A symlink, which the installer never follows.
    Symlink,
    /// Not a directory, or not inspectable; the installer refuses the path.
    Refused,
}

/// Classifies `home` with the classifier the installer uses. Only a
/// [`ConfigPath::Directory`] spelled the way the trusted walk accepts is
/// admitted into planning and identity deduplication, so a path the installer
/// refuses never shadows another home. The spelling is checked first because
/// [`config_path_kind`] collapses `//`, `/./` and a trailing `/`.
fn admit_home(home: &Home) -> HomeAdmission {
    if !pohunek_platform::filesystem::is_normalized_absolute_path(&home.dir) {
        return HomeAdmission::Refused;
    }
    match config_path_kind(&home.dir) {
        ConfigPath::Directory => HomeAdmission::Admitted,
        ConfigPath::Absent => HomeAdmission::Missing,
        ConfigPath::Symlink => HomeAdmission::Symlink,
        ConfigPath::Other | ConfigPath::Unreadable => HomeAdmission::Refused,
    }
}

/// Reports a planned config home that is not admitted, with its one next step.
/// An admitted home reports nothing.
fn report_home_gap(
    report: &mut Report,
    runtime: &str,
    profile: Option<&str>,
    program: &str,
    home: &Home,
    admission: HomeAdmission,
) {
    let owner = match profile {
        Some(name) => format!("the config home of profile `{name}`"),
        None => "its config home".to_owned(),
    };
    let dir = home.dir.display();
    let (code, detail) = match admission {
        HomeAdmission::Admitted => return,
        HomeAdmission::Missing => (
            CODE_GAP_CONFIG_HOME_MISSING,
            format!(
                "{owner} {dir} does not exist; start `{program}` once so it creates the home, \
                 then run setup again"
            ),
        ),
        HomeAdmission::Symlink => (
            CODE_GAP_CONFIG_HOME_SYMLINK,
            format!(
                "{owner} {dir} is a symlink; hooks are installed only into a real directory, so \
                 replace it with one, then run setup again"
            ),
        ),
        HomeAdmission::Refused => (
            CODE_HOME_PATH_REFUSED,
            format!(
                "{owner} {dir} is refused by the installer, which takes only a normalized absolute \
                 path to a real directory; fix it, then run setup again"
            ),
        ),
    };
    report.gap(runtime, code, &detail);
}

/// Gaps a runtime whose declared config home is not admitted. The package is
/// still planned and the home is never created: the step only reads the home.
fn config_home_gap(plan: &PlanInputs<'_>, definition: &RuntimeDefinition, report: &mut Report) {
    let Some(declared) = definition.config_home() else {
        return;
    };
    let runtime = definition.runtime_id().as_str();
    match Home::resolve(declared, plan.base, &[]) {
        Ok(home) => {
            let admission = admit_home(&home);
            report_home_gap(
                report,
                runtime,
                None,
                definition.program().as_str(),
                &home,
                admission,
            );
        }
        Err(error) => report.gap(
            runtime,
            CODE_GAP_CONFIG_HOME_MISSING,
            &format!(
                "its config home cannot be resolved ({}); set {} or HOME, then run setup again",
                error.code,
                declared.env()
            ),
        ),
    }
}

/// The config homes `--detect` lists: for each planned runtime whose hooks the
/// setup installs, its default home and each host profile's home, when admitted
/// (see [`admit_home`]). Each distinct directory appears once. Pi declares no
/// hooks, so it has none.
///
/// A profile is resolved the way `integration.install` will see it after the
/// plan applies: a profile the plan binds, or whose pin is the planned package,
/// is built on the planned definition; any other profile is resolved through the
/// installed package it names. A profile that does not resolve is a gap and
/// never enters the identity deduplication.
fn detected_integrations(
    profiles: &ProfileRegistry,
    base: &BaseEnv,
    planned: &[Planned],
    binds: &[Bind],
    official: &[OfficialProfile],
    report: &mut Report,
) -> Vec<IntegrationAction> {
    let hook_runtimes = crate::integration::hook_runtime_ids();
    let mut seen: Vec<(String, PathBuf)> = Vec::new();
    let mut actions = Vec::new();
    for entry in planned {
        if !hook_runtimes.contains(&entry.runtime) {
            continue;
        }
        let runtime = entry.runtime.as_str();
        let program = entry.definition.program().as_str();
        let Some(declared) = entry.definition.config_home() else {
            continue;
        };
        if let Ok(home) = Home::resolve(declared, base, &[]) {
            if matches!(admit_home(&home), HomeAdmission::Admitted) {
                seen.push((runtime.to_owned(), home.identity()));
                actions.push(IntegrationAction {
                    runtime: runtime.to_owned(),
                    profile: None,
                });
            }
        }
        let definition = Arc::new(entry.definition.clone());
        for official_profile in official.iter().filter(|p| p.base == entry.runtime) {
            let name = official_profile.name.as_str();
            let planned_package = binds.iter().any(|bind| bind.name == name)
                || official_profile.digest.as_ref() == Some(&entry.digest);
            let resolved = if planned_package {
                profiles.resolve_agent_pinned(name, &definition)
            } else {
                profiles.resolve_agent(name)
            };
            let agent = match resolved {
                Ok(agent) => agent,
                Err(error) => {
                    report.gap(
                        runtime,
                        &error.code,
                        &format!(
                            "profile `{name}` cannot be served by the package it names ({}); \
                             repair or re-pin the profile, then run setup again",
                            error.code
                        ),
                    );
                    continue;
                }
            };
            let Some(resolved_profile) = agent.profile.as_ref() else {
                continue;
            };
            let home = match Home::resolve(declared, base, &resolved_profile.env) {
                Ok(home) => home,
                Err(error) => {
                    report.gap(
                        runtime,
                        CODE_GAP_CONFIG_HOME_MISSING,
                        &format!(
                            "the config home of profile `{name}` cannot be resolved ({}); fix \
                             the profile's environment, then run setup again",
                            error.code
                        ),
                    );
                    continue;
                }
            };
            let admission = admit_home(&home);
            if !matches!(admission, HomeAdmission::Admitted) {
                report_home_gap(report, runtime, Some(name), program, &home, admission);
                continue;
            }
            let key = (runtime.to_owned(), home.identity());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            actions.push(IntegrationAction {
                runtime: runtime.to_owned(),
                profile: Some(name.to_owned()),
            });
        }
    }
    actions
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
        plan.retained.gate(),
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

/// Plans the pins of the host profiles over the planned runtimes.
///
/// An unpinned profile is bound to its planned package. A profile pinned to an
/// older official version of that package is repinned to it, and any other
/// pinned profile is kept and reported with its reason. Each rewritten profile
/// is validated against the planned definition the way a bind validates it,
/// and runs the coexistence check a bind runs.
fn plan_binds(
    profiles: &ProfileRegistry,
    plan: &PlanInputs<'_>,
    store_path: &Path,
    unpinned: &[Unpinned],
    pinned: &[Pinned],
    planned: &[Planned],
    report: &mut Report,
) -> Result<Vec<Bind>, EnsureError> {
    let mut repins = Vec::new();
    for profile in pinned {
        match classify_pin(plan, profile, planned) {
            Pinning::Untouched => {}
            Pinning::Keep(action) => report.profiles.push(action),
            Pinning::Repin { entry, previous } => repins.push((profile, entry, previous)),
        }
    }
    let targets: Vec<&str> = unpinned
        .iter()
        .filter(|profile| planned.iter().any(|entry| entry.runtime == profile.base))
        .map(|profile| profile.name.as_str())
        .chain(repins.iter().map(|(profile, _, _)| profile.name.as_str()))
        .collect();
    // The apply rewrites the agents directory through the owner-safety check
    // this opens it with, so a directory that fails it is refused here.
    if !targets.is_empty() && profiles.open_for_bind().is_err() {
        for name in targets {
            report.refuse(
                name,
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
        let Some(text) = rewrite_pin(
            profiles,
            plan,
            store_path,
            &profile.name,
            &profile.text,
            entry,
            report,
        )?
        else {
            continue;
        };
        binds.push(bind_of(&profile.name, entry, text, None));
    }
    for (profile, entry, previous) in repins {
        let Some(text) = rewrite_pin(
            profiles,
            plan,
            store_path,
            &profile.name,
            &profile.text,
            entry,
            report,
        )?
        else {
            continue;
        };
        binds.push(bind_of(&profile.name, entry, text, Some(previous)));
    }
    Ok(binds)
}

/// The bind of the profile `name` to the planned package `entry`.
fn bind_of(name: &str, entry: &Planned, text: String, previous: Option<Previous>) -> Bind {
    Bind {
        name: name.to_owned(),
        base: entry.runtime.clone(),
        digest: entry.digest.clone(),
        version: entry.identity.version.as_str().to_owned(),
        text,
        previous,
    }
}

/// What the plan does with a host profile that is pinned to a package.
enum Pinning<'a> {
    /// The profile already has the planned package, or no package is planned
    /// for its base runtime, or the release package is not planned as one to
    /// move to; it has no entry.
    Untouched,
    /// The profile moves to the planned package `entry`, off `previous`.
    Repin {
        entry: &'a Planned,
        previous: Previous,
    },
    /// The profile keeps its pin, and the action names the reason.
    Keep(ProfileAction),
}

/// Judges a pinned profile against the package planned for its base runtime.
///
/// A repin needs the registry record of the profile's digest to be an official
/// package with the planned package's id, and the planned package to be a
/// strictly newer official version of it. Every other pin is kept, and the
/// first failing check is the reason. Nothing here refuses the plan.
fn classify_pin<'a>(
    plan: &PlanInputs<'_>,
    profile: &Pinned,
    planned: &'a [Planned],
) -> Pinning<'a> {
    let Some(entry) = planned.iter().find(|entry| entry.runtime == profile.base) else {
        return Pinning::Untouched;
    };
    // A kept package is the installed one that serves the runtime, not a
    // release package to move to: no profile is repinned to it.
    if entry.digest == profile.pin.digest {
        return Pinning::Untouched;
    }
    let keep = |reason: &str, previous_version: Option<&str>| {
        Pinning::Keep(kept_action(profile, previous_version, reason))
    };
    let kept_selection = matches!(entry.action, Action::Kept);
    // Only a kept selection that has no order against the release's version is
    // reported: the pin stays and differs from what serves the runtime.
    if kept_selection
        && !entry.release_version.as_deref().is_some_and(|release| {
            semver::Version::parse(entry.identity.version.as_str()).is_err()
                || semver::Version::parse(release).is_err()
        })
    {
        return Pinning::Untouched;
    }
    let Some(record) = plan
        .state
        .and_then(|state| state.package(&profile.pin.digest))
    else {
        return keep(CODE_PINNED_VERSION_UNKNOWN, None);
    };
    let version = record.identity().version.as_str().to_owned();
    if record.source() != PackageSource::Official {
        return keep(CODE_PINNED_TO_NON_OFFICIAL_PACKAGE, Some(&version));
    }
    // The profile names its package as well as the digest; a pin whose
    // package key and digest record disagree is anomalous and kept.
    if record.identity().id != entry.identity.id || profile.pin.package != entry.identity.id {
        return keep(CODE_PINNED_TO_OTHER_PACKAGE, Some(&version));
    }
    if kept_selection {
        return keep(CODE_PINNED_DIFFERS_FROM_KEPT_SELECTION, Some(&version));
    }
    let target_is_official = plan
        .state
        .and_then(|state| state.package(&entry.digest))
        .is_none_or(|target| target.source() == PackageSource::Official);
    if !target_is_official {
        return keep(CODE_PINNED_TARGET_NOT_OFFICIAL, Some(&version));
    }
    match version_order(&record.identity().version, &entry.identity.version) {
        Some(Ordering::Less) => Pinning::Repin {
            entry,
            previous: Previous {
                digest: profile.pin.digest.clone(),
                version,
            },
        },
        Some(Ordering::Greater) => keep(CODE_PINNED_TO_NEWER_VERSION, Some(&version)),
        Some(Ordering::Equal) => keep(CODE_PINNED_TO_SAME_VERSION, Some(&version)),
        None => keep(CODE_PINNED_VERSION_UNKNOWN, Some(&version)),
    }
}

/// Rewrites the profile `name` with its pin to the planned package `entry`, and
/// validates it the way a bind does. `None` after a refusal of the plan.
fn rewrite_pin(
    profiles: &ProfileRegistry,
    plan: &PlanInputs<'_>,
    store_path: &Path,
    name: &str,
    text: &str,
    entry: &Planned,
    report: &mut Report,
) -> Result<Option<String>, EnsureError> {
    // The reference this profile replaces is not a reference to compare
    // with, so the set is the one the apply's bind uses.
    let retained = retained_pins(store_path, profiles, Some(name))?;
    if let Some(kind) = bind_coexistence(plan, entry, &retained) {
        report.refuse(
            name,
            &ProtocolError::from(kind).code,
            &format!(
                "an installed version of this package is not compatible with a retained version{}",
                plan.unreadable_note()
            ),
        );
        return Ok(None);
    }
    let pin = Pin {
        package: entry.identity.id.clone(),
        digest: entry.digest.clone(),
    };
    let rewritten = match apply_pin(text, &pin) {
        Ok(rewritten) => rewritten,
        Err(_detail) => {
            report.refuse(
                name,
                CODE_PROFILE_NOT_MIGRATABLE,
                "the profile cannot be rewritten with its package pin",
            );
            return Ok(None);
        }
    };
    if let Err(error) =
        profiles.validate_candidate(name, &rewritten, &Arc::new(entry.definition.clone()))
    {
        report.refuse(
            name,
            CODE_PROFILE_NOT_MIGRATABLE,
            &format!("the pinned profile is not accepted: {}", error.code),
        );
        return Ok(None);
    }
    Ok(Some(rewritten))
}

/// The coexistence refusal a bind of `entry` meets, as `ensure_coexists` judges
/// it. An installed, selected package is judged here; a package the apply still
/// installs is judged by [`coexistence_blocked`] before the bind is planned.
fn bind_coexistence(
    plan: &PlanInputs<'_>,
    entry: &Planned,
    retained: &RetainedPins,
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
        retained.gate(),
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

/// [`refuse_damaged`] for the installed root of the release package `prepared`.
fn refuse_prepared_damaged(
    report: &mut Report,
    subject: &str,
    kind: PackageErrorKind,
    prepared: &Prepared,
) {
    refuse_damaged(
        report,
        subject,
        kind,
        &prepared.identity.id,
        &prepared.digest,
    );
}

/// Refuses a release package whose installed root is present but damaged, with
/// the operator's next step.
fn refuse_damaged(
    report: &mut Report,
    subject: &str,
    kind: PackageErrorKind,
    package: &PackageId,
    digest: &PackageDigest,
) {
    report.refuse(
        subject,
        &ProtocolError::from(kind).code,
        &format!(
            "the installed package root of this runtime is altered or damaged; remove it with \
             `pohunek plugin uninstall {package} --digest {} --remove-modified --yes`, then \
             run the upgrade again",
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
        ensure_selectable(context, &retained, &digest, &declared_by(&definition))
            .map_err(kind_error)?;
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

/// The entry of one planned or applied pin: a bind, or a repin when the profile
/// moves off a previous pin.
fn profile_action(bind: &Bind, applied: bool) -> ProfileAction {
    let status = match (bind.previous.is_some(), applied) {
        (false, false) => ProfileStatus::Bind,
        (false, true) => ProfileStatus::Bound,
        (true, false) => ProfileStatus::Repin,
        (true, true) => ProfileStatus::Repinned,
    };
    ProfileAction {
        name: bind.name.clone(),
        base: bind.base.as_str().to_owned(),
        digest: bind.digest.as_str().to_owned(),
        status,
        previous_digest: bind
            .previous
            .as_ref()
            .map(|previous| previous.digest.as_str().to_owned()),
        previous_version: bind
            .previous
            .as_ref()
            .map(|previous| previous.version.clone()),
        version: Some(bind.version.clone()),
        kept_reason: None,
    }
}

/// The entry of a profile that keeps its pin, with the reason it is kept.
fn kept_action(profile: &Pinned, previous_version: Option<&str>, reason: &str) -> ProfileAction {
    ProfileAction {
        name: profile.name.clone(),
        base: profile.base.as_str().to_owned(),
        digest: profile.pin.digest.as_str().to_owned(),
        status: ProfileStatus::Kept,
        previous_digest: None,
        previous_version: previous_version.map(str::to_owned),
        version: None,
        kept_reason: Some(reason.to_owned()),
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
        replaces_newer: plan.replaces_newer.clone(),
        release_version: plan.release_version.clone(),
    }
}

/// The official package the registry selects over `prepared` when that package
/// is a strictly newer version of the same package. Installing and selecting
/// `prepared` replaces it.
fn replaces_newer(plan: &PlanInputs<'_>, prepared: &Prepared) -> Option<SelectedPackage> {
    let state = plan.state?;
    let selected = state.selected(&prepared.identity.id)?;
    if selected == &prepared.digest {
        return None;
    }
    let record = state.package(selected)?;
    if record.source() != PackageSource::Official {
        return None;
    }
    (version_order(&record.identity().version, &prepared.identity.version)
        == Some(Ordering::Greater))
    .then(|| SelectedPackage {
        digest: selected.as_str().to_owned(),
        version: record.identity().version.as_str().to_owned(),
    })
}

/// The selected official version of package `id` when the operator disabled
/// it. Whatever its version, the operator is the one who leaves a selected
/// package disabled: an install records the enabled flag and the selection in
/// one registry commit. The one other writer is `commit_install`, which
/// disables a package the loader rejects right after its install; the plan
/// reports that package by the state of its root, as for any disabled package.
fn disabled_selected_package(state: &RegistryState, id: &PackageId) -> Option<PackageDigest> {
    let selected = state.selected(id)?;
    let record = state.package(selected)?;
    (record.source() == PackageSource::Official && !record.enabled()).then(|| selected.clone())
}

/// [`disabled_selected_package`] over the plan's registry and the prepared
/// release package's id.
fn disabled_package(plan: &PlanInputs<'_>, prepared: &Prepared) -> Option<PackageDigest> {
    disabled_selected_package(plan.state?, &prepared.identity.id)
}

/// Why a refused release package cannot take its runtime id.
const RELEASE_CLAIM_DETAIL: &str =
    "the release package cannot take this runtime id alongside the installed runtimes";

/// The release's own package when it is recorded, official and disabled, and
/// the selection would otherwise be replaced by it: an enabled selection that
/// is kept is not affected. The operator leaves a package disabled and
/// unselected (`plugin disable`, `plugin install --no-enable`); the
/// loader-rejection disable of `commit_install` is reported by the state of the
/// root, as for any disabled package.
fn disabled_release_package(
    plan: &PlanInputs<'_>,
    prepared: &Prepared,
    reserved: bool,
) -> Option<PackageDigest> {
    if unordered_selection(plan, prepared).is_some()
        || (!reserved && replaces_newer(plan, prepared).is_some())
    {
        return None;
    }
    let record = plan.state?.package(&prepared.digest)?;
    (record.source() == PackageSource::Official && !record.enabled())
        .then(|| prepared.digest.clone())
}

/// The failure of the root of the disabled release package when it is present
/// but altered or damaged.
fn damaged_release_package(
    plan: &PlanInputs<'_>,
    prepared: &Prepared,
    reserved: bool,
) -> Option<PackageErrorKind> {
    let digest = disabled_release_package(plan, prepared, reserved)?;
    match disabled_state(plan, &digest, prepared.definition.runtime_id()) {
        DisabledState::Damaged(kind) => Some(kind),
        _ => None,
    }
}

/// Whether an enabled official selection other than the release's package
/// serves the runtime: it is recorded, enabled, and its root verifies and loads.
fn selection_serves(plan: &PlanInputs<'_>, prepared: &Prepared) -> bool {
    let Some(state) = plan.state else {
        return false;
    };
    let Some(selected) = state
        .selected(&prepared.identity.id)
        .filter(|selected| **selected != prepared.digest)
    else {
        return false;
    };
    state
        .package(selected)
        .is_some_and(|record| record.source() == PackageSource::Official && record.enabled())
        && matches!(health_of(plan, selected), Health::Present)
}

/// Whether `plugin enable` succeeds for a disabled package.
#[derive(Clone, Copy)]
enum DisabledState {
    /// Its root loads and it may serve its runtime id.
    Enableable,
    /// Its root is missing.
    RootMissing,
    /// Its root is present and fails verification or loading.
    Damaged(PackageErrorKind),
    /// Another package serves its runtime id.
    Claimed(PackageErrorKind),
}

/// The state of the disabled package `digest`, judged as `plugin enable` does.
fn disabled_state(
    plan: &PlanInputs<'_>,
    digest: &PackageDigest,
    runtime: &RuntimeId,
) -> DisabledState {
    let (Some(context), Some(record)) = (
        plan.planning,
        plan.state.and_then(|state| state.package(digest)),
    ) else {
        return DisabledState::Enableable;
    };
    match context.store.read_definition(digest, record.identity()) {
        Ok(_definition) => match prove_loadable(context, digest) {
            Ok(_definition) => DisabledState::Enableable,
            Err(kind) => DisabledState::Claimed(kind),
        },
        // A missing root cannot name its runtime, so the runtime the release
        // lists for the package stands for it: restoring the root does not help
        // while another package serves the id.
        Err(PackageRejection::Root(VerifyError::RootMissing)) => match decide_claim(
            runtime,
            &record.identity().id,
            Authority::Official,
            context.runtimes.served_by(runtime).as_ref(),
        ) {
            Ok(()) => DisabledState::RootMissing,
            Err(_refusal) => DisabledState::Claimed(PackageErrorKind::RuntimeConflict),
        },
        Err(rejection) => DisabledState::Damaged(rejection_kind(&rejection)),
    }
}

/// A digest without its `sha256:` prefix, as the `--digest` selector spells it.
fn selector_digest(digest: &PackageDigest) -> String {
    let text = digest.as_str();
    text.strip_prefix(DIGEST_PREFIX).unwrap_or(text).to_owned()
}

/// The gap detail for a disabled package the upgrade leaves as the operator
/// set it. `plugin update` would re-enable it, or refuse it as a downgrade, so
/// the hint is `plugin enable`, after the repair its state needs.
fn disabled_package_detail(
    package: &PackageId,
    digest: &PackageDigest,
    state: DisabledState,
) -> String {
    let selector = selector_digest(digest);
    let enable = format!("pohunek plugin enable {package} --digest {selector}");
    match state {
        DisabledState::Enableable => format!(
            "the package is disabled, so the upgrade leaves it as you set it; enable it with: \
             {enable}"
        ),
        DisabledState::RootMissing => format!(
            "the package is disabled and its root is missing, so the upgrade leaves it; restore \
             it with `pohunek plugin install` of that version's archive, then enable it with: \
             {enable}"
        ),
        DisabledState::Damaged(_) | DisabledState::Claimed(_) => "the package is disabled and \
             cannot be enabled as it is, so the upgrade leaves it; `pohunek plugin doctor` names \
             the cause"
            .to_owned(),
    }
}

/// Prefix of a package digest; the `--digest` selector spells it without.
const DIGEST_PREFIX: &str = "sha256:";

/// What the plan does with the selected official version of a runtime.
enum Selection {
    /// The release's package is planned over it.
    Replace,
    /// The selected package stays and serves the runtime, for `reason`.
    Keep { reason: KeepReason },
    /// The runtime is not planned: the selection is disabled or does not load.
    Leave,
}

/// Decides what happens to the selected official version of a runtime.
///
/// A selected package the operator disabled is never replaced, selected or
/// enabled, whatever its version, and neither is a disabled release package
/// that a replacement would enable: the runtime is left as set, with a gap. A version that
/// cannot be ordered against the release's is never replaced either, for any
/// runtime: it is kept and reported when it serves, and left, with a gap, when
/// it cannot load. A newer ordered version is kept only for a runtime that is
/// not reserved (Pi); a reserved runtime's is replaced and reported through
/// `replaces_newer`, unless the registry holds the release package's id and
/// version under another digest: the release's archive cannot be installed
/// then, so the serving selection is kept with a gap.
fn selection_outcome(
    plan: &PlanInputs<'_>,
    prepared: &Prepared,
    subject: &str,
    report: &mut Report,
    reserved: bool,
    reasons: &[UseReason],
) -> Selection {
    let id = &prepared.identity.id;
    if let Some(digest) = disabled_package(plan, prepared) {
        leave_disabled(plan, report, subject, prepared, &digest);
        return Selection::Leave;
    }
    match unordered_selection(plan, prepared) {
        Some((Serving::Loadable, _)) => {
            return Selection::Keep {
                reason: KeepReason::Unordered,
            }
        }
        Some((Serving::Damaged(kind), selected)) => {
            refuse_damaged(report, subject, kind, id, &selected);
            return Selection::Leave;
        }
        Some((Serving::Broken, _)) => {
            report_not_serving(
                report,
                subject,
                format!(
                    "the selected version cannot be ordered against the release's and its \
                     package root is missing or does not load, so the upgrade leaves it; \
                     restore it with `pohunek plugin install` of that version's archive, or \
                     select another installed version with: pohunek plugin select {id} \
                     --digest <digest>"
                ),
            );
            return Selection::Leave;
        }
        None => {}
    }
    if let Some(digest) = disabled_release_package(plan, prepared, reserved) {
        if selection_serves(plan, prepared) {
            // The enabled selection keeps serving; the release's package stays
            // as the operator set it. An altered root of that package is
            // refused with its uninstall command, as for any release package.
            if let Some(kind) = damaged_release_package(plan, prepared, reserved) {
                refuse_damaged(report, subject, kind, id, &digest);
                return Selection::Leave;
            }
            let state = disabled_state(plan, &digest, prepared.definition.runtime_id());
            let detail = format!(
                "the selected version keeps serving, and the release's package is disabled, so \
                 the upgrade does not update it; {}",
                disabled_package_detail(id, &digest, state)
            );
            report.gap(subject, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING, &detail);
            return Selection::Keep {
                reason: KeepReason::Ordered,
            };
        }
        leave_disabled(plan, report, subject, prepared, &digest);
        return Selection::Leave;
    }
    if !reserved && replaces_newer(plan, prepared).is_some() {
        return Selection::Keep {
            reason: KeepReason::Ordered,
        };
    }
    let blocked = Blocked::of(prepared);
    let Some((holder, held)) = held_selection(plan, &blocked) else {
        return Selection::Replace;
    };
    if matches!(held, Held::Serving) {
        return Selection::Keep {
            reason: KeepReason::SameVersionOtherDigest(same_version_detail(
                plan, prepared, &holder, reserved,
            )),
        };
    }
    let hint = held_hint(plan, &blocked, &holder, &held, reserved, reasons);
    match hint.refusal {
        Some(kind) => {
            // The refusal supersedes the gap that `managed_runtimes` reported.
            report.gaps.retain(|gap| {
                gap.runtime != subject || gap.code != CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING
            });
            report.refuse(subject, &ProtocolError::from(kind).code, &hint.detail);
        }
        None => report_not_serving(report, subject, hint.detail),
    }
    Selection::Leave
}

/// The release package that a recorded holder blocks.
struct Blocked<'a> {
    id: &'a PackageId,
    version: &'a str,
    digest: &'a PackageDigest,
    runtime: &'a RuntimeId,
}

impl<'a> Blocked<'a> {
    fn of(prepared: &'a Prepared) -> Self {
        Self {
            id: &prepared.identity.id,
            version: prepared.identity.version.as_str(),
            digest: &prepared.digest,
            runtime: prepared.definition.runtime_id(),
        }
    }
}

/// What the selection of a package id is when the registry holds the release
/// package's id and version under another digest, the holder.
enum Held {
    /// An enabled official selection serves the runtime: it is kept.
    Serving,
    /// No version of the package is selected.
    NoSelection,
    /// The selected package is not an enabled official one.
    NotOfficial(PackageDigest),
    /// The enabled official selection's root is missing or does not load.
    Broken(PackageDigest),
    /// The package's root is present and altered or damaged; the package is the
    /// enabled official selection, or the unselected holder.
    Damaged(PackageErrorKind, PackageDigest),
}

/// The digest the registry records under the release package's id and version
/// when it is not the release's digest, whatever its source, flags and
/// selection: the registry keeps one digest per identity.
fn holder_of(plan: &PlanInputs<'_>, blocked: &Blocked<'_>) -> Option<PackageDigest> {
    plan.state?
        .packages()
        .iter()
        .find(|record| {
            record.identity().id == *blocked.id
                && record.identity().version.as_str() == blocked.version
                && record.digest() != blocked.digest
        })
        .map(|record| record.digest().clone())
}

/// The holder of the release package's identity and the state of the selection
/// beside it; `None` when the registry holds no such digest, so the release's
/// archive can be installed.
fn held_selection(plan: &PlanInputs<'_>, blocked: &Blocked<'_>) -> Option<(PackageDigest, Held)> {
    let holder = holder_of(plan, blocked)?;
    let state = plan.state?;
    let held = match state.selected(blocked.id) {
        // The registry refuses the plain uninstall of an unselected holder whose
        // root is altered.
        None => match health_of(plan, &holder) {
            Health::Damaged(kind) => Held::Damaged(kind, holder.clone()),
            Health::Present | Health::Install => Held::NoSelection,
        },
        Some(selected) if selected == blocked.digest => return None,
        Some(selected) => match state.package(selected) {
            Some(record) if record.source() == PackageSource::Official && record.enabled() => {
                match health_of(plan, selected) {
                    Health::Present => Held::Serving,
                    Health::Install => Held::Broken(selected.clone()),
                    Health::Damaged(kind) => Held::Damaged(kind, selected.clone()),
                }
            }
            // The registry refuses the plain uninstall of the altered root of a
            // selected holder whatever its source.
            _ if *selected == holder => match health_of(plan, selected) {
                Health::Damaged(kind) => Held::Damaged(kind, selected.clone()),
                Health::Present | Health::Install => Held::NotOfficial(selected.clone()),
            },
            _ => Held::NotOfficial(selected.clone()),
        },
    };
    Some((holder, held))
}

/// Whether [`selection_outcome`] reaches the holder check and leaves the runtime
/// untouched there.
fn held_leaves(plan: &PlanInputs<'_>, prepared: &Prepared, reserved: bool) -> bool {
    unordered_selection(plan, prepared).is_none()
        && (reserved || replaces_newer(plan, prepared).is_none())
        && matches!(
            held_selection(plan, &Blocked::of(prepared)),
            Some((
                _,
                Held::NoSelection | Held::NotOfficial(_) | Held::Broken(_) | Held::Damaged(..)
            ))
        )
}

/// How a runtime left or refused beside a holder is reported.
struct HeldHint {
    /// The refusal code kind; `None` for the gap `official_package_not_serving`.
    refusal: Option<PackageErrorKind>,
    detail: String,
}

/// The one hint for a runtime that the holder of the release's package keeps
/// from being installed.
///
/// The obstacle is the digest that stops the runtime serving: the selected
/// package, or the holder when nothing is selected. The hint names, in order,
/// the release of a pin on the obstacle (an edit, since no command unpins), the
/// removal of the obstacle, and what follows it: the next upgrade, or for a Pi
/// that keeps no enabled official selection the install of the release's
/// package by hand. A reserved runtime that no profile, session or hook uses
/// gets no command, because the plain upgrade would not plan it again.
fn held_hint(
    plan: &PlanInputs<'_>,
    blocked: &Blocked<'_>,
    holder: &PackageDigest,
    held: &Held,
    reserved: bool,
    reasons: &[UseReason],
) -> HeldHint {
    let id = blocked.id;
    let same = format!(
        "the release's package has the same version ({}) as an installed one under another \
         archive",
        blocked.version
    );
    let used = reasons.iter().any(|reason| {
        matches!(
            reason,
            UseReason::Profile | UseReason::Session | UseReason::Integration
        )
    });
    let named = !reserved || used;
    let direct = |cause: &str| {
        let lead = format!(
            "{same}, and {cause}, so nothing is installed and the runtime is left as it is"
        );
        let detail = if named {
            let clause = removal_clause(plan, blocked, holder, holder, false, reserved, reasons);
            format!("{lead}{clause}")
        } else {
            format!("{lead}; the fix is a release that gives the package a new version")
        };
        HeldHint {
            refusal: None,
            detail,
        }
    };
    let beside = |tail: String| {
        let mut detail = format!("{same}, so it cannot be installed, and {tail}");
        if named && plan.removable(holder) {
            let altered = matches!(health_of(plan, holder), Health::Damaged(_));
            let command = uninstall_command(id, holder, altered);
            let follow = follow_up(plan, blocked, holder, holder, reserved, reasons)
                .map(|step| format!(", then {step}"))
                .unwrap_or_default();
            let _ = write!(
                detail,
                "; or remove the same-version installed one with `{command}`{follow}"
            );
        }
        HeldHint {
            refusal: None,
            detail,
        }
    };
    match held {
        Held::Serving => direct("an enabled official selection serves"),
        Held::Damaged(kind, obstacle) => {
            let clause = removal_clause(plan, blocked, holder, obstacle, true, reserved, reasons);
            HeldHint {
                refusal: Some(*kind),
                detail: format!(
                    "the installed package root of this runtime is altered or damaged{clause}"
                ),
            }
        }
        Held::NoSelection => direct("no version of the package is selected"),
        Held::Broken(selected) if selected == holder => {
            direct("the selected package's root is missing or does not load")
        }
        Held::NotOfficial(selected) if selected == holder => {
            direct("the selected package is not an official one")
        }
        Held::Broken(_) => beside(format!(
            "the selected version's package root is missing or does not load, so the upgrade \
             leaves it; restore it by installing that version's archive again, or select \
             another installed version with: pohunek plugin select {id} --digest <digest>"
        )),
        Held::NotOfficial(_) => beside(format!(
            "the selected package is not an enabled official one, so the upgrade leaves it; \
             select an official version with: pohunek plugin select {id} --digest <digest>"
        )),
    }
}

/// `pohunek plugin uninstall` of `digest`, with `--remove-modified` for a root
/// that is altered.
fn uninstall_command(id: &PackageId, digest: &PackageDigest, altered: bool) -> String {
    let modified = if altered { " --remove-modified" } else { "" };
    format!(
        "pohunek plugin uninstall {id} --digest {}{modified} --yes",
        selector_digest(digest)
    )
}

/// The clause that releases a pin on the obstacle, removes it and says what
/// follows, starting with `; `.
fn removal_clause(
    plan: &PlanInputs<'_>,
    blocked: &Blocked<'_>,
    holder: &PackageDigest,
    obstacle: &PackageDigest,
    altered: bool,
    reserved: bool,
    reasons: &[UseReason],
) -> String {
    let pin = pin_release(plan, obstacle).unwrap_or_default();
    let command = uninstall_command(blocked.id, obstacle, altered);
    let follow = follow_up(plan, blocked, obstacle, holder, reserved, reasons)
        .map(|step| format!(", then {step}"))
        .unwrap_or_default();
    format!("; {pin}remove it with `{command}`{follow}")
}

/// Who pins `digest` and how to release the pin, with the trailing `; then `;
/// `None` when nothing pins it. There is no command that unpins: a profile is
/// edited, and a session pin goes with the sessions.
fn pin_release(plan: &PlanInputs<'_>, digest: &PackageDigest) -> Option<String> {
    if !plan.retained.known().contains(digest) {
        return plan
            .retained
            .is_uncertain()
            .then(|| UNREADABLE_PIN_STEP.to_owned());
    }
    let profiles: Vec<String> = plan
        .pins
        .profiles
        .iter()
        .filter(|(_, pinned)| pinned == digest)
        .map(|(name, _)| format!("{name}.toml"))
        .collect();
    let session = plan
        .pins
        .sessions
        .iter()
        .any(|record| record.pin.as_ref() == Some(digest));
    let mut who = Vec::new();
    let mut exit = Vec::new();
    if !profiles.is_empty() {
        let names = profiles
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        who.push(format!("the host profile {names}"));
        exit.push(format!(
            "remove the `package` and `digest` lines from {names} in the host profile directory"
        ));
    }
    if session {
        who.push("a session record".to_owned());
        exit.push("remove the sessions launched from it".to_owned());
    }
    if who.is_empty() {
        who.push("a host profile or session".to_owned());
        exit.push("release the pin".to_owned());
    }
    let verb = if profiles.len() + usize::from(session) > 1 {
        "pin"
    } else {
        "pins"
    };
    Some(format!(
        "{} {verb} it, so the registry refuses its removal until the pin is released: {}; then ",
        who.join(" and "),
        exit.join(" and ")
    ))
}

/// What follows the removal of `removed`: the next upgrade, or for a Pi that
/// keeps no enabled official selection the install of the release's package by
/// hand. `None` when the plain upgrade would not plan the runtime again.
fn follow_up(
    plan: &PlanInputs<'_>,
    blocked: &Blocked<'_>,
    removed: &PackageDigest,
    holder: &PackageDigest,
    reserved: bool,
    reasons: &[UseReason],
) -> Option<String> {
    let upgrade = "run the upgrade again".to_owned();
    if reserved {
        // Detection alone is a use only for a runtime nothing else uses, so the
        // three modes name the same step.
        let used_after = reasons.iter().any(|reason| match reason {
            UseReason::Profile | UseReason::Integration => true,
            // Another record of the runtime that does not pin the package keeps
            // it used after the pinning sessions are gone.
            UseReason::Session => plan.pins.sessions.iter().any(|record| {
                record.runtime.as_ref() == Some(blocked.runtime)
                    && record.pin.as_ref() != Some(removed)
            }),
            UseReason::Detected | UseReason::Installed => false,
        });
        let detected_only = reasons
            .iter()
            .all(|reason| matches!(reason, UseReason::Detected));
        return (used_after || detected_only).then_some(upgrade);
    }
    // The holder stays recorded, so the next upgrade names the next step.
    if removed != holder {
        return Some(upgrade);
    }
    let state = plan.state?;
    let kept = state.selected(blocked.id).is_some_and(|selected| {
        selected != removed
            && state.package(selected).is_some_and(|record| {
                record.source() == PackageSource::Official && record.enabled()
            })
    });
    if kept {
        return Some(upgrade);
    }
    let other_version = state
        .packages()
        .iter()
        .any(|record| record.identity().id == *blocked.id && record.digest() != removed);
    let command = if other_version {
        update_command(blocked.id, blocked.runtime)
    } else {
        install_command(blocked.runtime)
    };
    Some(format!("install the release's package with `{command}`"))
}

/// Why a selected package stays instead of being replaced.
#[derive(Clone)]
enum KeepReason {
    /// Its version is ordered against the release's and is not older, or the
    /// release's own package is disabled.
    Ordered,
    /// Its version has no order against the release's.
    Unordered,
    /// The registry holds the release package's id and version under another
    /// digest, which `package.install` refuses to sit beside; the gap detail.
    SameVersionOtherDigest(String),
}

/// The gap detail for a release package whose id and version the registry
/// holds under the digest `holder`.
///
/// The uninstall command is named only where the registry accepts it for the
/// selected holder of a reserved runtime: its package verifies and no session or
/// host profile pins it. [`settle_same_version_gaps`] withdraws the command where
/// the upgrade itself pins the holder or does not plan the runtime again.
fn same_version_detail(
    plan: &PlanInputs<'_>,
    prepared: &Prepared,
    holder: &PackageDigest,
    reserved: bool,
) -> String {
    let version = prepared.identity.version.as_str();
    let removable = reserved
        && plan
            .state
            .and_then(|state| state.selected(&prepared.identity.id))
            == Some(holder)
        && plan.removable(holder)
        && matches!(health_of(plan, holder), Health::Present);
    if removable {
        return format!(
            "the release's package has the same version ({version}) as the installed one under \
             another archive, so the selected package is kept; remove the installed one with \
             `pohunek plugin uninstall {} --digest {} --yes`, then run the upgrade again",
            prepared.identity.id,
            selector_digest(holder)
        );
    }
    same_version_plain_detail(version, reserved)
}

/// The same-version gap detail that names no command: no command releases the
/// pin on the holder, or the upgrade does not install the package again.
fn same_version_plain_detail(version: &str, reserved: bool) -> String {
    let pi = if reserved {
        ""
    } else {
        "; the upgrade does not install a Pi package again once none is installed"
    };
    format!(
        "the release's package has the same version ({version}) as an installed one under \
         another archive, so the selected package keeps serving and nothing is installed; the \
         fix is a release that gives the package a new version{pi}"
    )
}

/// Withdraws the uninstall command from a same-version gap whose runtime the
/// next plain upgrade does not install again, or whose holder this plan pins: a
/// runtime planned only by `--detect`, or one with an unpinned profile that the
/// plan binds to the kept package.
fn settle_same_version_gaps(report: &mut Report, planned: &[Planned], unpinned: &[Unpinned]) {
    for entry in planned {
        let runtime = entry.runtime.as_str();
        let used = entry.reasons.iter().any(|reason| {
            matches!(
                reason,
                UseReason::Profile | UseReason::Session | UseReason::Integration
            )
        });
        let bound = unpinned.iter().any(|profile| profile.base == entry.runtime);
        if used && !bound {
            continue;
        }
        let Some(version) = entry.release_version.as_deref() else {
            continue;
        };
        for gap in report.gaps.iter_mut().filter(|gap| {
            gap.runtime == runtime && gap.code == CODE_GAP_UPDATE_SAME_VERSION_OTHER_DIGEST
        }) {
            gap.detail = same_version_plain_detail(version, is_reserved(&entry.runtime));
        }
    }
}

/// Reports a runtime whose disabled package `digest` the upgrade leaves as the
/// operator set it, with what the package's state needs before it is enabled.
fn leave_disabled(
    plan: &PlanInputs<'_>,
    report: &mut Report,
    subject: &str,
    prepared: &Prepared,
    digest: &PackageDigest,
) {
    let id = &prepared.identity.id;
    let state = disabled_state(plan, digest, prepared.definition.runtime_id());
    if matches!(state, DisabledState::Damaged(_) | DisabledState::Claimed(_)) {
        // The refusal or the claim gap below supersedes the gap that
        // `managed_runtimes` reported for the package.
        report.gaps.retain(|gap| {
            gap.runtime != subject || gap.code != CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING
        });
    }
    match state {
        DisabledState::Damaged(kind) => refuse_damaged(report, subject, kind, id, digest),
        DisabledState::Claimed(kind) => report_claim_refusal(
            report,
            subject,
            kind,
            "the disabled package cannot be enabled while another package serves this runtime id",
        ),
        state => report_not_serving(report, subject, disabled_package_detail(id, digest, state)),
    }
}

/// Reports a runtime the upgrade leaves as it is because its package is not
/// serving, and lists none of its integrations.
fn report_not_serving(report: &mut Report, subject: &str, detail: String) {
    // `managed_runtimes` may already report a non-reserved runtime's package;
    // this detail replaces it.
    match report
        .gaps
        .iter_mut()
        .find(|gap| gap.runtime == subject && gap.code == CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING)
    {
        Some(gap) => gap.detail = detail,
        None => report.gap(subject, CODE_GAP_OFFICIAL_PACKAGE_NOT_SERVING, &detail),
    }
    report
        .integrations
        .retain(|action| action.runtime != subject);
}

/// The state of a selected official version that has no order against the
/// prepared one.
enum Serving {
    /// Enabled and loadable.
    Loadable,
    /// Enabled but not serving: its root is missing or does not load.
    Broken,
    /// Enabled, and its root is present but altered or damaged.
    Damaged(PackageErrorKind),
}

/// The state of the enabled selected official version of the package when it
/// differs from the prepared one and the two versions have no order, with its
/// digest; `None` otherwise.
fn unordered_selection(
    plan: &PlanInputs<'_>,
    prepared: &Prepared,
) -> Option<(Serving, PackageDigest)> {
    let state = plan.state?;
    let selected = state.selected(&prepared.identity.id)?;
    if selected == &prepared.digest {
        return None;
    }
    let record = state.package(selected)?;
    if record.source() != PackageSource::Official
        || version_order(&record.identity().version, &prepared.identity.version).is_some()
    {
        return None;
    }
    if !record.enabled() {
        return None;
    }
    let serving = match health_of(plan, selected) {
        Health::Present => Serving::Loadable,
        Health::Install => Serving::Broken,
        Health::Damaged(kind) => Serving::Damaged(kind),
    };
    Some((serving, selected.clone()))
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
