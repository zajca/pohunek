//! Read-only adoption preflight for an upgrade.
//!
//! The running daemon of the previous release owns every live session and
//! holds each worker's single controller slot, so this judgement never
//! connects to a worker, takes no instance lock, and never writes, signals or
//! stops anything. Its evidence is the metadata store (migrated in memory), the
//! worker journals on disk and the process table. Each live session gets the
//! verdict startup reconciliation of this build would reach for it, decided by
//! the functions reconciliation itself uses: the store load, the journal
//! reader, the resume-binding merge, the launch-identity check and the
//! launch-pin resolution resume applies. Evidence that cannot be read counts
//! against the session.
//!
//! When the release carries a `runtime/` directory, the judgement serves the
//! runtimes the release step would leave behind: the planner of that step runs
//! read-only ([`plan_official_runtimes`]) and its planned packages replace the
//! installed ones of their runtime ids. A session resumes and forks through the
//! same version probe resume runs, so a probe the executable fails is judged
//! as the refusal resume would return. Stopped sessions that keep native
//! resume or fork are judged by the same recovery rules.
//!
//! Each at-risk verdict of a loaded record whose code is proven stable across
//! the replacement daemon's startup also carries an acceptance scope (see
//! [`acceptance_scope`]), the digest a later preflight must reproduce for an
//! earlier acceptance to still apply.
//!
//! Evidence only a worker's socket can give (its answering protocol version,
//! the identity it reports) is judged from what its journal recorded of it.
//! The store migration does not consult the runtime registry, so a binding
//! that needs it to complete its launch spec is reported as unverified.

// Rust guideline compliant 2026-10-12

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pohunek_service_config::preflight::{
    PreflightReport, SessionVerdict, StoreReport, StoreState, Verdict, CODE_ADOPTABLE,
    CODE_ADOPTABLE_BY_NEWER_PACKAGE, CODE_CREATE_ROLLED_BACK, CODE_EVIDENCE_UNAVAILABLE,
    CODE_GENERATION_MISSING, CODE_IDENTITY_UNVERIFIED, CODE_INTENT_FINISHED,
    CODE_JOURNAL_AMBIGUOUS, CODE_JOURNAL_MISSING, CODE_JOURNAL_SCHEMA_UNSUPPORTED,
    CODE_JOURNAL_UNREADABLE, CODE_NO_SESSION_RECORD, CODE_PROTOCOL_INCOMPATIBLE,
    CODE_RECORD_UNREADABLE, CODE_RECOVERY_UNMAPPABLE, CODE_RECOVERY_UNVERIFIED,
    CODE_RESUME_RECORD_UNREADABLE, CODE_RUNTIME_IDENTITY_MISMATCH, CODE_RUNTIME_SOURCE_NOT_SERVED,
    CODE_STORE_UNUSABLE, CODE_WORKER_CHILD_MISSING, CODE_WORKER_ENDED, CODE_WORKER_MARKER_LEGACY,
    CODE_WORKER_NOT_ADVANCED, CODE_WORKER_NOT_INITIALIZED, CODE_WORKER_NOT_RUNNING,
    CODE_WORKER_UNVERIFIABLE, REPORT_VERSION,
};
use tracing::warn;

use pohunek_service_config::preflight::{
    ProfileWarning, CODE_PROBE_BUDGET_EXHAUSTED, CODE_PROFILE_WITHOUT_SCREEN_READINESS,
    CODE_REFERENCE_UNVERIFIED, MAX_PROBE_BUDGET_SECS, PROBE_BUDGET,
    PROFILE_WITHOUT_SCREEN_READINESS_DETAIL,
};
use pohunek_service_config::runtimes::{
    CatalogStatus, EnsureReport, Refusal, CODE_RUNTIME_PLAN_UNAVAILABLE,
    CODE_SERVICE_CONFIG_UNREADABLE, ENSURE_REPORT_VERSION,
};
use pohunek_worker_protocol::{negotiate, BaseEnv, Version, VersionRange, SUPPORTED_RANGE};
use protocol::{
    BindingProvenance, LaunchBinding, PackageDigest, PackageIdentity, ProtocolError, RuntimeId,
};

use super::{
    import_native_reference, import_terminal_native_reference, import_worker_identities,
    import_worker_subagents, initial_input_orphaned, merge_persisted_recovery,
    recovery_not_advanced, reported_hook_schema, runtime_identity_conflict, scan_worker_journals,
    validate_identity_claims, IdentityClaims, JournalEvidence, JournalPhase, JournalReject,
    WorkerJournalScan,
};
use crate::agent::host::{
    adopts, adopts_newer, validate_launch_runtime_with_timeout_detailed, BuiltinSource,
    DefinitionError, DefinitionOrigin, LaunchPin, PackageSource, PackageStore, RuntimeDefinition,
    RuntimeHost, Spawn,
};
use crate::agent::{
    parse_head, NativeReferenceProvenance, Pin, ProfileRegistry, ReferenceCheckFailure,
    ReferenceExistence, RefusalPhase, ResolvedAgent, ValidatedLaunchProgram,
};
use crate::capabilities::{unsupported_version_error, LaunchValidationFailure};
use crate::catalog_anchor::CatalogTrust;
use crate::detect::Manifest;
use crate::procwatch::ProcessInspector;
use crate::runtime::environment::effective_variable;
use crate::runtime::lifecycle::IDENTITY_MISMATCH;
use crate::runtime::LaunchEnvironment;
use crate::session::resume::{
    binding_native_launch, binding_program, check_recovery_reference, recovery_contract,
    recovery_definition, reference_existence, session_ref_from_binding,
};
use crate::session::resume::{recovery_profile_in, ProfileChange};
use crate::session::{plan_official_runtimes, EnsureInputs, OfficialPlan, PlannedProfile};
use crate::session::{runtime_hook_schema, HookSchemaLookup};
use crate::session::{DesiredState, Generation, SessionRecord};
use crate::store::{
    may_hold_session, DryRun, DryRunState, RecoveryOutcome, ResumeBinding, Store,
    StoreBodyTooLarge, StoreSchemaError, STORE_SCHEMA_VERSION,
};
use protocol::RuntimeState;

mod acceptance_scope;
use acceptance_scope::{scope_of, ListKind};

/// The installed state the preflight reads.
#[derive(Debug, Clone)]
pub struct PreflightInputs {
    /// The metadata store file.
    pub store_path: PathBuf,
    /// The durable worker journal root.
    pub worker_state_root: PathBuf,
    /// The runtime package store, read when a session needs a hook schema its
    /// journal does not carry; it is never created or changed.
    pub plugins_dir: PathBuf,
}

/// The release step inputs of a preflight of a new release.
#[derive(Debug, Clone)]
pub struct ReleaseInputs {
    /// The release's `runtime/` directory, beside the running executable.
    pub release_dir: PathBuf,
    /// The trust anchor the release catalog is verified against.
    pub catalog_trust: CatalogTrust,
}

/// What the preflight judges against beyond the installed state.
#[derive(Debug, Clone)]
pub struct PreflightOptions {
    /// The release step's inputs. Without them the judgement serves the
    /// installed packages only, and the report carries no plan.
    pub release: Option<ReleaseInputs>,
    /// The host profiles directory. The profile of a session names the
    /// environment its version probe runs under.
    pub profiles_dir: Option<PathBuf>,
    /// The host-state directory the profile revision key is kept in. It is
    /// read, never created: a judgement compares revisions the daemon froze.
    pub host_state_dir: Option<PathBuf>,
    /// The entry of the command. The probe deadline is `entry` plus
    /// `probe_budget`, so the work before the probes counts against it.
    pub entry: Instant,
    /// The time the version probes of one judgement may spend, counted from
    /// `entry`. A probe that would start at or after the deadline is judged
    /// [`CODE_PROBE_BUDGET_EXHAUSTED`].
    pub probe_budget: Duration,
    /// The launch environment of the installed daemon, or the reason the
    /// installed `service.toml` cannot be read. The integration homes are read
    /// with it and the version probe runs under it. An unreadable file refuses
    /// the upgrade, so no verdict is computed under a guessed environment.
    pub launch: Result<LaunchEnvironment, String>,
}

impl Default for PreflightOptions {
    fn default() -> Self {
        Self {
            release: None,
            profiles_dir: None,
            host_state_dir: None,
            entry: Instant::now(),
            probe_budget: PROBE_BUDGET,
            launch: Ok(LaunchEnvironment::subprocess_default()),
        }
    }
}

/// Machine code of a base environment the probe cannot be run under.
const CODE_WORKER_INITIALIZE_INVALID: &str = "worker_initialize_invalid";

/// Machine code of a store written by a newer daemon.
const STORE_NEWER: &str = "store_newer_than_binary";
/// Machine code of a store older than every kept migration.
const STORE_NO_MIGRATION: &str = "store_no_migration_path";
/// Machine code of a store line with an invalid schema version.
const STORE_INVALID_VERSION: &str = "store_invalid_schema_version";
/// Machine code of a migrated store larger than the durable read limit.
const STORE_TOO_LARGE: &str = "store_too_large";
/// Machine code of a store that cannot be read.
const STORE_UNREADABLE: &str = "store_unreadable";

/// Judges every live session of the installed state against this build.
///
/// Reads only; see the module documentation for what the verdicts rest on.
/// The judgement serves the installed packages only and reports no release
/// plan; [`run_with`] judges the runtimes a release step would leave.
#[must_use]
pub fn run(inputs: &PreflightInputs, inspector: &dyn ProcessInspector) -> PreflightReport {
    run_with(inputs, &PreflightOptions::default(), inspector)
}

/// Judges every live session of the installed state against this build and
/// the release step described by `options`.
///
/// Reads only, and never creates the package store. See [`run`].
#[must_use]
pub fn run_with(
    inputs: &PreflightInputs,
    options: &PreflightOptions,
    inspector: &dyn ProcessInspector,
) -> PreflightReport {
    let journals = scan_worker_journals(&inputs.worker_state_root)
        .map_err(|error| format!("the worker journal root cannot be read: {error}"));
    let (runtimes_report, planned, planned_profiles) = match &options.launch {
        Ok(_) => release_plan(inputs, options),
        Err(detail) => (
            Some(refused_plan(CODE_SERVICE_CONFIG_UNREADABLE, detail)),
            Vec::new(),
            Vec::new(),
        ),
    };
    let judged_definitions = planned.clone();
    let (runtimes, runtimes_report) = match read_only_runtimes(&inputs.plugins_dir, planned) {
        Ok(runtimes) => (runtimes, runtimes_report),
        Err(detail) => {
            warn!(
                error = %detail,
                "the installed runtime registry cannot be read; the upgrade is refused"
            );
            let detail = format!(
                "the installed runtime registry cannot be read, so the plan cannot be checked \
                 against it: {detail}"
            );
            (
                RuntimeHost::from_host_environment(),
                Some(with_refusal(
                    runtimes_report,
                    CODE_RUNTIME_PLAN_UNAVAILABLE,
                    &detail,
                )),
            )
        }
    };
    let profiles = judging_profiles(options, &runtimes, &planned_profiles);
    let base_env = match &options.launch {
        Ok(launch) => launch.base().map_err(|error| error.to_string()),
        Err(detail) => Err(detail.clone()),
    };
    let deadline = options
        .entry
        .checked_add(options.probe_budget)
        .unwrap_or(options.entry);
    let probes = ProbeLedger::new(deadline, options.probe_budget);
    let dry = Store::new(inputs.store_path.clone()).dry_run_migration();
    let store_from = dry.as_ref().ok().and_then(|dry| match dry.state {
        DryRunState::WouldMigrate { from } => Some(from),
        DryRunState::Missing | DryRunState::UpToDate => None,
    });
    let judging = Judging {
        runtimes: &runtimes,
        profiles: &profiles,
        base_env: &base_env,
        probes: &probes,
        store_from,
    };
    let profile_warnings =
        judge_profile_warnings(&profiles, &planned_profiles, &judged_definitions);
    let (store, sessions, unmanaged_workers, stopped_sessions) = match dry {
        Ok(dry) => {
            let (sessions, unmanaged) =
                judge_sessions(&dry, journals.as_ref(), inspector, &judging);
            let mut stopped = judge_stopped_sessions(&dry, journals.as_ref().ok(), &judging);
            judge_rejected_records(&dry, &sessions, &mut stopped);
            (store_report(inputs, &dry), sessions, unmanaged, stopped)
        }
        Err(error) => {
            let report = refused_report(inputs, &error);
            let detail = report
                .error
                .clone()
                .unwrap_or_else(|| "the metadata store is unusable".to_owned());
            let sessions = match &journals {
                Ok(journals) => live_journal_sessions(journals, &HashSet::new(), inspector)
                    .into_iter()
                    .map(|session_id| SessionVerdict {
                        session_id,
                        name: None,
                        verdict: Verdict::WouldNotBeAdopted,
                        code: CODE_STORE_UNUSABLE.to_owned(),
                        detail: format!("the new daemon cannot start: {detail}"),
                        acceptance_scope: None,
                    })
                    .collect(),
                Err(detail) => vec![evidence_unavailable(detail)],
            };
            (report, sessions, Vec::new(), Vec::new())
        }
    };
    PreflightReport {
        report_version: REPORT_VERSION,
        daemon_version: crate::DAEMON_VERSION.to_owned(),
        store,
        sessions,
        stopped_sessions,
        unmanaged_workers,
        runtimes: runtimes_report,
        profile_warnings,
    }
}

/// The host profiles a judgement reads: the profile directory over `runtimes`,
/// with the revision key read only, and the profile texts the release step
/// writes in place of the files, which are the texts resume reads after it.
fn judging_profiles(
    options: &PreflightOptions,
    runtimes: &RuntimeHost,
    planned_profiles: &[PlannedProfile],
) -> ProfileRegistry {
    ProfileRegistry::with_runtimes(options.profiles_dir.clone(), runtimes.clone())
        .with_read_only_revision_state_dir(options.host_state_dir.clone())
        .with_planned_texts(
            planned_profiles
                .iter()
                .map(|profile| (profile.name.clone(), profile.text.clone())),
        )
}

/// The host profiles that load with a reduced guarantee on the runtimes the
/// new daemon serves: an override manifest without the screen readiness rule
/// its base declares. Profiles that do not resolve are left to the session
/// verdicts and to doctor, and nothing here refuses the upgrade.
fn judge_profile_warnings(
    profiles: &ProfileRegistry,
    planned_profiles: &[PlannedProfile],
    planned: &[RuntimeDefinition],
) -> Vec<ProfileWarning> {
    let mut warnings = Vec::new();
    for profile in planned_profiles {
        // A profile the plan rewrites is judged as the apply writes it, over the
        // planned definition of its base. The planner validated that text, so a
        // failure here is left to the session verdicts and doctor.
        let Some(definition) = planned
            .iter()
            .find(|definition| definition.runtime_id() == &profile.base)
        else {
            continue;
        };
        if let Ok(agent) =
            profiles.resolve_candidate(&profile.name, &profile.text, &Arc::new(definition.clone()))
        {
            warnings.extend(profile_warning(&agent));
        }
    }
    // A profile the plan does not rewrite is judged as it is on disk, which is
    // the case after the apply, when its profiles are already pinned.
    let enumerated = profiles.enumerate();
    let resolved: HashSet<String> = enumerated.iter().map(|agent| agent.name.clone()).collect();
    for agent in enumerated {
        if planned_profiles
            .iter()
            .any(|profile| profile.name == agent.name)
        {
            continue;
        }
        warnings.extend(profile_warning(&agent));
    }
    // Enumeration skips a profile pinned to a digest the store does not hold
    // yet, although the plan installs that digest. Such a profile is judged over
    // the planned definition of its pin; a pin that is neither installed nor
    // planned resolves nowhere and stays skipped.
    let texts = match profiles.scanned_texts() {
        Ok(texts) => texts,
        Err(error) => {
            warn!(error = %error, "the agent profiles cannot be listed for the profile warnings");
            Vec::new()
        }
    };
    for (name, text) in texts {
        if resolved.contains(&name) || planned_profiles.iter().any(|profile| profile.name == name) {
            continue;
        }
        let Ok(text) = text else { continue };
        let Ok(head) = parse_head(&text) else {
            continue;
        };
        let Some(pin) = head.pin() else { continue };
        let Some(definition) = planned_for_pin(planned, &head.base, &pin) else {
            continue;
        };
        if let Ok(agent) = profiles.resolve_candidate(&name, &text, &Arc::new(definition.clone())) {
            warnings.extend(profile_warning(&agent));
        }
    }
    warnings.sort_by(|left, right| left.profile.cmp(&right.profile));
    warnings
}

/// The planned definition of `base` at exactly the package and digest `pin`
/// names, as the apply installs it.
fn planned_for_pin<'a>(
    planned: &'a [RuntimeDefinition],
    base: &RuntimeId,
    pin: &Pin,
) -> Option<&'a RuntimeDefinition> {
    planned.iter().find(|definition| {
        definition.runtime_id() == base
            && matches!(
                &definition.binding().provenance,
                BindingProvenance::Package { package, package_digest }
                    if package.id == pin.package && package_digest == &pin.digest
            )
    })
}

/// The warning of a resolved profile whose override manifest lacks the screen
/// readiness its base declares.
fn profile_warning(agent: &ResolvedAgent) -> Option<ProfileWarning> {
    if !agent.override_lacks_screen_readiness() {
        return None;
    }
    let manifest = agent.override_manifest_name()?;
    Some(ProfileWarning {
        detail: format!(
            "profile {} with manifest {manifest}: {PROFILE_WITHOUT_SCREEN_READINESS_DETAIL}",
            agent.name
        ),
        profile: agent.name.clone(),
        code: CODE_PROFILE_WITHOUT_SCREEN_READINESS.to_owned(),
    })
}

/// The release step's plan and the definitions of the packages it would
/// install. Without a release `runtime/` directory there is no plan.
fn release_plan(
    inputs: &PreflightInputs,
    options: &PreflightOptions,
) -> (
    Option<EnsureReport>,
    Vec<RuntimeDefinition>,
    Vec<PlannedProfile>,
) {
    let Some(release) = options.release.as_ref() else {
        return (None, Vec::new(), Vec::new());
    };
    if !release.release_dir.is_dir() {
        return (None, Vec::new(), Vec::new());
    }
    // The plan scans the host profiles, so without their directory it cannot
    // say what the release step would do, and the upgrade must not proceed.
    let Some(profiles_dir) = options.profiles_dir.clone() else {
        return (
            Some(refused_plan(
                CODE_RUNTIME_PLAN_UNAVAILABLE,
                "the release runtime plan cannot be computed: the host profiles directory is not known",
            )),
            Vec::new(),
            Vec::new(),
        );
    };
    let ensure = EnsureInputs {
        store_path: inputs.store_path.clone(),
        plugins_dir: inputs.plugins_dir.clone(),
        profiles_dir,
        release_dir: release.release_dir.clone(),
        catalog_trust: release.catalog_trust.clone(),
        dry_run: true,
        daemon_version: crate::DAEMON_VERSION.to_owned(),
        environment: options.launch.clone(),
    };
    match plan_official_runtimes(&ensure) {
        Ok(OfficialPlan {
            report,
            definitions,
            profiles,
        }) => (Some(report), definitions, profiles),
        Err(error) => (
            Some(refused_plan(
                CODE_RUNTIME_PLAN_UNAVAILABLE,
                &format!("the release runtime plan cannot be computed: {error}"),
            )),
            Vec::new(),
            Vec::new(),
        ),
    }
}

/// The report of a release plan that is refused with `code` before any change.
fn refused_plan(code: &str, detail: &str) -> EnsureReport {
    EnsureReport {
        report_version: ENSURE_REPORT_VERSION,
        daemon_version: crate::DAEMON_VERSION.to_owned(),
        dry_run: true,
        catalog: CatalogStatus::Unavailable,
        packages: Vec::new(),
        profiles: Vec::new(),
        integrations: Vec::new(),
        refusals: vec![Refusal {
            subject: "runtime-plan".to_owned(),
            code: code.to_owned(),
            detail: detail.to_owned(),
        }],
        gaps: Vec::new(),
    }
}

/// The runtime definitions the preflight reads: the built-in runtimes, the
/// installed packages when the package store exists, and the `planned` official
/// definitions in place of the installed packages of their runtime ids. Nothing
/// is created.
///
/// # Errors
///
/// Returns the reason the installed packages or the registry built from them
/// cannot be read. The caller reports it as a refusal: a judgement over the
/// built-in runtimes alone is not the state the new daemon starts with.
fn read_only_runtimes(
    plugins_dir: &Path,
    planned: Vec<RuntimeDefinition>,
) -> Result<RuntimeHost, String> {
    let store = PackageStore::open_existing(plugins_dir)
        .map_err(|error| format!("the runtime package store cannot be opened: {error}"))?;
    RuntimeHost::with_planned_packages(
        BuiltinSource::from_host_environment(),
        store.map(PackageSource::new),
        planned,
    )
    .map_err(|error| format!("the runtime registry cannot be built: {error}"))
}

/// Adds a refusal to the plan report, or starts a report that only refuses.
fn with_refusal(report: Option<EnsureReport>, code: &str, detail: &str) -> EnsureReport {
    let mut report = report.unwrap_or_else(|| refused_plan(code, detail));
    if report.refusals.iter().all(|refusal| refusal.code != code) {
        report.refusals.push(Refusal {
            subject: "runtime-plan".to_owned(),
            code: code.to_owned(),
            detail: detail.to_owned(),
        });
    }
    report
}

fn store_report(inputs: &PreflightInputs, dry: &DryRun) -> StoreReport {
    let (state, schema_from) = match dry.state {
        DryRunState::Missing => (StoreState::Missing, None),
        DryRunState::UpToDate => (StoreState::UpToDate, Some(STORE_SCHEMA_VERSION)),
        DryRunState::WouldMigrate { from } => (StoreState::WouldMigrate, Some(from)),
    };
    StoreReport {
        path: inputs.store_path.clone(),
        state,
        schema_from,
        schema_to: STORE_SCHEMA_VERSION,
        records: dry.records,
        error_code: None,
        error: None,
    }
}

fn refused_report(inputs: &PreflightInputs, error: &io::Error) -> StoreReport {
    let (code, schema_from) = match StoreSchemaError::from_io(error) {
        Some(StoreSchemaError::NewerThanBinary { found, .. }) => (STORE_NEWER, Some(*found)),
        Some(StoreSchemaError::NoMigrationPath { found, .. }) => (STORE_NO_MIGRATION, Some(*found)),
        Some(StoreSchemaError::InvalidVersion { .. }) => (STORE_INVALID_VERSION, None),
        None if StoreBodyTooLarge::from_io(error).is_some() => (STORE_TOO_LARGE, None),
        // `MigrationRequired` is only returned by loads of a store that is
        // not migrated, which a dry run never performs.
        Some(_) | None => (STORE_UNREADABLE, None),
    };
    StoreReport {
        path: inputs.store_path.clone(),
        state: StoreState::Refused,
        schema_from,
        schema_to: STORE_SCHEMA_VERSION,
        records: 0,
        error_code: Some(code.to_owned()),
        error: Some(error.to_string()),
    }
}

/// The verdict of every session that may own a live runtime, ordered by
/// session id, and the live journaled workers no record accounts for.
///
/// Such a worker is unmanaged before and after the upgrade, so it is only
/// listed, unless a store line that names no session could not be read: that
/// line may be the worker's own record.
fn judge_sessions(
    dry: &DryRun,
    journals: Result<&HashMap<String, WorkerJournalScan>, &String>,
    inspector: &dyn ProcessInspector,
    judging: &Judging<'_>,
) -> (Vec<SessionVerdict>, Vec<String>) {
    let bindings: HashMap<&str, &ResumeBinding> = dry
        .resume
        .iter()
        .map(|binding| (binding.session_id.as_str(), binding))
        .collect();
    // A skipped line that names no session may be the separate recovery of any
    // live session, so each one that depends on it is judged as if it were.
    let anonymous_resume = dry.rejected.iter().any(|line| {
        line.session_id.is_none() && matches!(line.kind.as_deref(), None | Some("resume"))
    });
    let unreadable_resume: HashSet<&str> = dry
        .rejected
        .iter()
        .filter(|line| line.kind.as_deref() == Some("resume"))
        .filter_map(|line| line.session_id.as_deref())
        .collect();
    let mut verdicts = BTreeMap::new();
    let mut known = HashSet::new();
    for record in &dry.sessions {
        known.insert(record.session_id.clone());
        if !record.info.may_own_runtime() || runtime_ended(record) {
            continue;
        }
        let scan = match journals {
            Ok(journals) => Ok(journals.get(&record.session_id)),
            Err(detail) => Err(detail.as_str()),
        };
        let facts = facts_of(dry, record, &bindings, &unreadable_resume, anonymous_resume);
        let ((verdict, code, detail), candidate) = judge(record, &facts, scan, inspector, judging);
        let acceptance_scope = scope_of(ListKind::Live, &candidate, verdict, &code);
        verdicts.insert(
            record.session_id.clone(),
            SessionVerdict {
                session_id: record.session_id.clone(),
                name: record.info.name.clone(),
                verdict,
                code,
                detail,
                acceptance_scope,
            },
        );
    }
    for line in &dry.rejected {
        if !line.may_own_runtime {
            continue;
        }
        let session_id = line
            .session_id
            .clone()
            .unwrap_or_else(|| "unknown".to_owned());
        known.insert(session_id.clone());
        verdicts.insert(
            session_id.clone(),
            SessionVerdict {
                session_id,
                name: None,
                verdict: Verdict::WouldNotBeAdopted,
                code: CODE_RECORD_UNREADABLE.to_owned(),
                detail: format!(
                    "the session record cannot be loaded and is skipped at startup: {}",
                    line.reason
                ),
                acceptance_scope: None,
            },
        );
    }
    let mut unmanaged = Vec::new();
    if let Ok(journals) = journals {
        for session_id in live_journal_sessions(journals, &known, inspector) {
            if !anonymous_loss(dry) {
                unmanaged.push(session_id);
                continue;
            }
            verdicts.insert(
                session_id.clone(),
                SessionVerdict {
                    session_id,
                    name: None,
                    verdict: Verdict::WouldNotBeAdopted,
                    code: CODE_NO_SESSION_RECORD.to_owned(),
                    detail: "a live worker journal names a session the store has no record of, \
                             and a store line that names no session cannot be read; that line \
                             may be this session's record"
                        .to_owned(),
                    acceptance_scope: None,
                },
            );
        }
    }
    journal_gaps(dry, journals, &known, &mut verdicts);
    (verdicts.into_values().collect(), unmanaged)
}

/// The store-side evidence of one session record.
fn facts_of<'a>(
    dry: &DryRun,
    record: &SessionRecord,
    bindings: &HashMap<&str, &'a ResumeBinding>,
    unreadable_resume: &HashSet<&str>,
    anonymous_resume: bool,
) -> Facts<'a> {
    Facts {
        binding: bindings.get(record.session_id.as_str()).copied(),
        recovery: dry
            .recovery
            .get(&record.session_id)
            .copied()
            .unwrap_or(RecoveryOutcome::Preserved),
        resume_unreadable: anonymous_resume
            || unreadable_resume.contains(record.session_id.as_str()),
    }
}

/// The verdict of every session whose runtime has ended and that keeps native
/// resume or fork, ordered by session id.
///
/// A stopped session has no worker to adopt, so only its recovery is judged:
/// resume and fork must still resolve through the runtime source the new
/// daemon serves, under the same rules as a live session's recovery.
fn judge_stopped_sessions(
    dry: &DryRun,
    journals: Option<&HashMap<String, WorkerJournalScan>>,
    judging: &Judging<'_>,
) -> Vec<SessionVerdict> {
    let bindings: HashMap<&str, &ResumeBinding> = dry
        .resume
        .iter()
        .map(|binding| (binding.session_id.as_str(), binding))
        .collect();
    let anonymous_resume = dry.rejected.iter().any(|line| {
        line.session_id.is_none() && matches!(line.kind.as_deref(), None | Some("resume"))
    });
    let unreadable_resume: HashSet<&str> = dry
        .rejected
        .iter()
        .filter(|line| line.kind.as_deref() == Some("resume"))
        .filter_map(|line| line.session_id.as_deref())
        .collect();
    let mut verdicts = Vec::new();
    for record in &dry.sessions {
        let stopped = record.info.external != Some(true)
            && (record.desired_state == DesiredState::Stopped
                || !record.info.may_own_runtime()
                || runtime_ended(record));
        // Reconciliation finishes a removal intent instead of resuming the
        // session, so its recovery is not one an upgrade must keep.
        if !stopped || record.desired_state == DesiredState::Removed {
            continue;
        }
        let facts = facts_of(dry, record, &bindings, &unreadable_resume, anonymous_resume);
        let (judgement, candidate) = judge_stopped(
            record,
            &facts,
            journals.and_then(|journals| journals.get(&record.session_id)),
            judging,
        );
        let Some((verdict, code, detail)) = judgement else {
            continue;
        };
        let acceptance_scope = scope_of(ListKind::Stopped, &candidate, verdict, &code);
        verdicts.push(SessionVerdict {
            session_id: record.session_id.clone(),
            name: record.info.name.clone(),
            verdict,
            code,
            detail,
            acceptance_scope,
        });
    }
    fill_unverified_counts(&mut verdicts, judging.probes.budget);
    verdicts.sort_by(|left: &SessionVerdict, right| left.session_id.cmp(&right.session_id));
    verdicts
}

/// Judges one stopped session. A session without native resume or fork is still
/// judged on its migration outcome: the previous release may have resumed it
/// from the stored reference, and startup can fail to repair that reference.
/// Any other session without a recovery binding, or without either capability,
/// has no recovery to lose and gives `None`.
fn judge_stopped(
    record: &SessionRecord,
    facts: &Facts<'_>,
    journals: Option<&WorkerJournalScan>,
    judging: &Judging<'_>,
) -> (Option<Judgement>, SessionRecord) {
    let mut candidate = record.clone();
    let judgement = judge_stopped_candidate(record, &mut candidate, facts, journals, judging);
    (judgement, candidate)
}

fn judge_stopped_candidate(
    record: &SessionRecord,
    candidate: &mut SessionRecord,
    facts: &Facts<'_>,
    journals: Option<&WorkerJournalScan>,
    judging: &Judging<'_>,
) -> Option<Judgement> {
    if let Some(binding) = facts.binding.cloned() {
        if let Err(reason) = merge_persisted_recovery(candidate, binding) {
            // Reconciliation refuses a contradiction only for a running record;
            // a stopped record keeps its own recovery, which resume then uses.
            if candidate.desired_state == DesiredState::Running {
                return Some((
                    Verdict::WouldLoseRecovery,
                    reason.to_owned(),
                    "the record contradicts its stored resume binding, so native recovery is not \
                     restored from it"
                        .to_owned(),
                ));
            }
            *candidate = record.clone();
        }
    }
    // A replayed stop imports the worker's final native reference before it
    // retains recovery. Judge that same reference rather than stale metadata.
    if record.desired_state == DesiredState::Stopped {
        project_stopped_reference(candidate, journals, judging.runtimes);
    }
    if candidate.desired_state == DesiredState::Stopped
        && candidate.recovery.is_none()
        && (facts.resume_unreadable || facts.recovery != RecoveryOutcome::Preserved)
    {
        return recovery_loss(facts, candidate, judging);
    }
    candidate.recovery.as_ref()?;
    let capabilities = &candidate.info.capabilities;
    if !capabilities.resume && !capabilities.fork {
        return match facts.recovery {
            RecoveryOutcome::Lost | RecoveryOutcome::NeedsRegistry => {
                recovery_loss(facts, candidate, judging)
            }
            RecoveryOutcome::Preserved => None,
        };
    }
    Some(match recovery_loss(facts, candidate, judging) {
        Some(loss) => loss,
        None => judge_reference(candidate, judging)
            .unwrap_or_else(|| adoptable_verdict(candidate, judging)),
    })
}

/// Projects only the reference a bound worker retains on stop. Invalid or
/// ambiguous evidence is refused separately by the runtime intent judgement;
/// it must never replace the stored recovery reference here.
fn project_stopped_reference(
    record: &mut SessionRecord,
    scan: Option<&WorkerJournalScan>,
    runtimes: &RuntimeHost,
) {
    let Some(scan) = scan else {
        return;
    };
    if !scan.rejected.is_empty() {
        return;
    }
    let Ok(Some(generation)) = Generation::from_record(&record.session_id, &record.runtime) else {
        return;
    };
    let scoped = scan.scoped_to(Some(&generation));
    let [journal] = scoped.evidence.as_slice() else {
        return;
    };
    if runtime_identity_conflict(
        record,
        &journal.worker_id,
        journal.worker_instance_id.as_deref(),
    ) {
        return;
    }
    let Ok(mut snapshot) = journal.snapshot() else {
        return;
    };
    if snapshot.hook_schema.is_none() {
        match record_hook_schema(record, runtimes) {
            HookSchemaLookup::Schema(schema) => snapshot.hook_schema = Some(schema.id.to_owned()),
            HookSchemaLookup::NoIntegration => {}
            HookSchemaLookup::Unresolved(_) => return,
        }
    }
    import_terminal_native_reference(
        record,
        &snapshot,
        reported_hook_schema(snapshot.hook_schema.as_deref()),
    );
}

/// Resolves the schema startup supplies to a worker that journaled none.
fn record_hook_schema(record: &SessionRecord, runtimes: &RuntimeHost) -> HookSchemaLookup {
    let pin = record
        .recovery
        .as_ref()
        .map(|binding| binding.launch_binding.clone())
        .unwrap_or_default();
    runtime_hook_schema(runtimes, &record.info.agent_base, &pin)
}

/// The verdict of a stopped session whose recovery the new daemon keeps, naming
/// the package versions when a newer official package version adopts it.
fn adoptable_verdict(candidate: &SessionRecord, judging: &Judging<'_>) -> Judgement {
    let adopted = candidate.recovery.as_ref().and_then(|binding| {
        let definition = recovery_definition(judging.profiles, binding).ok()?;
        if !adopts_newer(&binding.launch_binding, &definition) {
            return None;
        }
        let frozen = package_identity(binding.launch_binding.binding()?)?;
        let served = package_identity(definition.binding())?;
        Some((frozen.clone(), served.clone()))
    });
    match adopted {
        Some((frozen, served)) => (
            Verdict::Adoptable,
            CODE_ADOPTABLE_BY_NEWER_PACKAGE.to_owned(),
            format!(
                "the stored native recovery resumes and forks through {} {}, which adopts the \
                 session launched from {}; the session is moved to the newer version on its next \
                 resume or fork",
                served.id, served.version, frozen.version
            ),
        ),
        None => (
            Verdict::Adoptable,
            CODE_ADOPTABLE.to_owned(),
            "the stored native recovery resumes and forks through the runtime source this daemon \
             serves"
                .to_owned(),
        ),
    }
}

/// The package identity of a package-pinned binding.
fn package_identity(binding: &LaunchBinding) -> Option<&PackageIdentity> {
    match &binding.provenance {
        BindingProvenance::Package { package, .. } => Some(package),
        BindingProvenance::Builtin { .. } => None,
    }
}

/// The v0.34.2 built-in runtimes whose native reference check is compiled in as
/// a golden copy: the runtime id, its `runtime.toml`, and its `detect.toml`.
/// Hermes is not listed: its golden names a version probe parser this daemon
/// does not compile in, so a Hermes reference over an adopted package stays
/// fail-closed.
const V034_GOLDENS: [(&str, &str, &str); 2] = [
    (
        RuntimeId::CLAUDE,
        include_str!("../../../../../compat/claude/builtin-v0.34/runtime.toml"),
        include_str!("../../../../../compat/claude/builtin-v0.34/detect.toml"),
    ),
    (
        RuntimeId::CODEX,
        include_str!("../../../../../compat/codex/builtin-v0.34/runtime.toml"),
        include_str!("../../../../../compat/codex/builtin-v0.34/detect.toml"),
    ),
];

/// What the v0.34.2 golden of a runtime declares for its native reference check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GoldenCheck {
    /// No golden is compiled in for the runtime, or it does not parse.
    Missing,
    /// The golden's existence declaration, `None` when it declares none.
    Declared(Option<ReferenceExistence>),
}

/// The native reference existence check v0.34.2 declared for `runtime`, parsed
/// from its golden with the descriptor parser.
pub(crate) fn golden_check(runtime: &str) -> GoldenCheck {
    let Some((_, descriptor, detect)) = V034_GOLDENS.iter().find(|(id, _, _)| *id == runtime)
    else {
        return GoldenCheck::Missing;
    };
    let parsed = RuntimeDefinition::from_toml(
        descriptor,
        |package| DefinitionOrigin::Builtin {
            package: Some(package),
        },
        |name| {
            if name != runtime {
                return Err(DefinitionError::UnknownManifest);
            }
            Manifest::parse_str(detect)
                .ok()
                .map(Arc::new)
                .ok_or(DefinitionError::UnknownManifest)
        },
    );
    match parsed {
        Ok(definition) => GoldenCheck::Declared(definition.hook_existence().cloned()),
        Err(_) => GoldenCheck::Missing,
    }
}

/// The digest of a package-pinned binding, if it is pinned to one.
fn package_digest(binding: &LaunchBinding) -> Option<&PackageDigest> {
    match &binding.provenance {
        BindingProvenance::Package { package_digest, .. } => Some(package_digest),
        BindingProvenance::Builtin { .. } => None,
    }
}

/// Whether the running release's native reference check for a reference is the
/// one the upgrade serves.
enum CheckIdentity {
    /// The check is the same on both sides of the upgrade.
    Unchanged,
    /// The check cannot be shown identical; the reason is the verdict's detail.
    Unproven(String),
}

/// The identity of the native reference check of `binding`, judged against the
/// `definition` the upgrade serves.
///
/// An assigned reference is frozen in its launch, so its check is the session's
/// own. A reported reference reads the check of the running release's
/// definition, which is provable identical only in two cases: a package pinned
/// to the session's digest, which content addressing fixes, or a built-in that
/// the upgrade adopts from a v0.34.x store, whose check must equal the v0.34.2
/// golden. Every other reported reference is unproven.
fn reported_check_identity(
    binding: &ResumeBinding,
    definition: &RuntimeDefinition,
    judging: &Judging<'_>,
) -> CheckIdentity {
    if !matches!(
        binding.native_reference_provenance,
        NativeReferenceProvenance::Reported
    ) {
        return CheckIdentity::Unchanged;
    }
    let pinned = match &binding.launch_binding {
        LaunchPin::Pinned(frozen) => package_digest(frozen),
        LaunchPin::Unpinned => None,
    };
    if pinned.is_some() && pinned == package_digest(definition.binding()) {
        return CheckIdentity::Unchanged;
    }
    // A newer package version adopts a session only with the frozen digest's
    // native reference check (`RuntimeHost::newer_adopter`).
    if adopts_newer(&binding.launch_binding, definition) {
        return CheckIdentity::Unchanged;
    }
    if !adopts(&binding.launch_binding, definition) {
        return CheckIdentity::Unproven(
            "the recovery runs a built-in runtime that the upgrade does not replace with a \
             package pinned to its digest, so the running release's native reference check \
             cannot be shown identical"
                .to_owned(),
        );
    }
    if judging.store_from != Some(STORE_SCHEMA_V034) {
        return CheckIdentity::Unproven(format!(
            "the store's oldest schema is {}, not {STORE_SCHEMA_V034} as a v0.34.x daemon \
             writes it, so the adopted package's native reference check cannot be shown \
             identical to the one the running release checks",
            judging
                .store_from
                .map_or_else(|| "current".to_owned(), |from| from.to_string())
        ));
    }
    let runtime = definition.binding().runtime_id.as_str();
    match golden_check(runtime) {
        GoldenCheck::Missing => CheckIdentity::Unproven(format!(
            "no v0.34.2 golden of runtime {runtime} is compiled in to compare the adopted \
             package's native reference check with"
        )),
        GoldenCheck::Declared(golden) if golden.as_ref() == definition.hook_existence() => {
            CheckIdentity::Unchanged
        }
        GoldenCheck::Declared(_) => CheckIdentity::Unproven(
            "the adopted package's native reference check differs from the v0.34.2 golden, so \
             the package changed its check"
                .to_owned(),
        ),
    }
}

/// The store schema a v0.34.0 to v0.34.2 daemon writes.
const STORE_SCHEMA_V034: u32 = 5;

/// The native reference check resume runs after [`judge_launch`] passed: the
/// stopped session's conversation must still exist where its launch looks for
/// it. Resume refuses a missing or relocated reference before any launch, with
/// the code and message this returns.
///
/// The verdict is [`Verdict::AlreadyNotResumable`] only when the upgrade keeps
/// the check: an assigned reference, or a reported reference whose check
/// [`reported_check_identity`] proves identical. A reported reference whose
/// check cannot be proven identical is [`Verdict::WouldLoseRecovery`] with the
/// reason. `None` when the reference verifies, or when no check is declared.
fn judge_reference(candidate: &SessionRecord, judging: &Judging<'_>) -> Option<Judgement> {
    let binding = candidate.recovery.as_ref()?;
    let capabilities = &candidate.info.capabilities;
    if !capabilities.resume && !capabilities.fork {
        return None;
    }
    // judge_pin and judge_launch refuse a definition, profile or base
    // environment that does not resolve before this check is reached, so the
    // lookups below succeed for every session that gets here.
    let fork = !capabilities.resume;
    let definition = recovery_definition(judging.profiles, binding).ok()?;
    let profile = recovery_profile_in(
        judging.profiles,
        binding,
        Some(&definition),
        ProfileChange::Refuse,
        fork,
    )
    .ok()?;
    let base = judging.base_env.as_ref().ok()?;
    let identity = reported_check_identity(binding, &definition, judging);
    let launch = binding_native_launch(binding, &definition);
    let existence = reference_existence(binding, launch.as_ref(), &definition)?;
    let session_ref = match session_ref_from_binding(launch.as_ref()?.reference_kind(), binding) {
        Ok(session_ref) => session_ref,
        Err(error) => {
            return Some(match identity {
                CheckIdentity::Unchanged => (
                    Verdict::AlreadyNotResumable,
                    error.code.clone(),
                    format!(
                        "resume refuses the session's native reference ({}: {}) before any \
                         launch, and the upgrade does not change it",
                        error.code, error.msg
                    ),
                ),
                CheckIdentity::Unproven(reason) => (
                    Verdict::WouldLoseRecovery,
                    error.code.clone(),
                    format!(
                        "resume refuses the session's native reference ({}: {}) before any \
                         launch, and {reason}; restore the conversation, or upgrade with \
                         --accept-runtime-loss",
                        error.code, error.msg
                    ),
                ),
            });
        }
    };
    // The reference scan is not a version probe, so it runs under the same
    // deadline as the probes: once the budget is spent, a reference that was
    // not checked is unverified, never adoptable and never already not resumable.
    // An unchecked reference reads nothing, so resume's judgement holds whatever
    // the budget is.
    if existence.scans_store() && judging.probes.remaining().is_zero() {
        return Some((
            Verdict::WouldLoseRecovery,
            CODE_REFERENCE_UNVERIFIED.to_owned(),
            unverified_detail(0, judging.probes.budget),
        ));
    }
    let failure = check_recovery_reference(
        &existence,
        &session_ref,
        base,
        profile.env.as_slice(),
        &binding.args,
        &binding.cwd,
    )
    .err()?;
    let error = existence.refusal(failure, RefusalPhase::Recovery);
    Some(match identity {
        CheckIdentity::Unproven(reason) => (
            Verdict::WouldLoseRecovery,
            error.code.clone(),
            format!(
                "resume refuses the session's native reference ({}: {}) before any launch, and \
                 {reason}; restore the conversation, or upgrade with --accept-runtime-loss",
                error.code, error.msg
            ),
        ),
        CheckIdentity::Unchanged if stored_reference_fact(failure) => (
            Verdict::AlreadyNotResumable,
            error.code.clone(),
            format!(
                "resume refuses the session's native reference ({}: {}) before any launch on \
                 the running release too, so the upgrade does not lose its recovery",
                error.code, error.msg
            ),
        ),
        CheckIdentity::Unchanged => (
            Verdict::WouldLoseRecovery,
            error.code.clone(),
            format!(
                "resume refuses the session's native reference ({}: {}) before any launch, but \
                 the preflight cannot show that the running release refuses it too; restore the \
                 store, or upgrade with --accept-runtime-loss",
                error.code, error.msg
            ),
        ),
    })
}

/// The detail of a stopped session whose native reference the budget did not
/// reach. `count` is the number of such sessions in the report.
fn unverified_detail(count: usize, budget: Duration) -> String {
    format!(
        "the native reference of this session was not checked because the preflight budget of \
         {} seconds was spent first; {count} stopped session(s) in this report are unverified \
         this way. Run `pohunek service check|upgrade --preflight-budget <SECONDS>` with a \
         budget up to {} seconds, or upgrade with --accept-runtime-loss",
        budget.as_secs(),
        MAX_PROBE_BUDGET_SECS,
    )
}

/// Writes the number of unverified sessions into each of their details, so the
/// count is the same for every session and in both report formats.
fn fill_unverified_counts(verdicts: &mut [SessionVerdict], budget: Duration) {
    let count = verdicts
        .iter()
        .filter(|verdict| verdict.code == CODE_REFERENCE_UNVERIFIED)
        .count();
    for verdict in verdicts
        .iter_mut()
        .filter(|verdict| verdict.code == CODE_REFERENCE_UNVERIFIED)
    {
        verdict.detail = unverified_detail(count, budget);
    }
}

/// Whether a failed reference check reports a fact of the stored data itself,
/// independent of the daemon version: a conversation absent from the declared
/// store, or a stored reference that is not a plain identifier. Relocation
/// and settings checks depend on the daemon's detection rules, and the
/// store-condition failures depend on the host's state at the time of the
/// check, so the running release may decide differently and they are not
/// judged as already not resumable.
fn stored_reference_fact(failure: ReferenceCheckFailure) -> bool {
    matches!(
        failure,
        ReferenceCheckFailure::Missing | ReferenceCheckFailure::InvalidReference
    )
}

/// Judges every record the loader skipped that names a session no other verdict
/// covers, and that cannot own a live runtime: startup skips it, so the session
/// loses its recovery. A live skipped record is already judged by
/// [`judge_sessions`], as not adopted.
fn judge_rejected_records(
    dry: &DryRun,
    sessions: &[SessionVerdict],
    stopped: &mut Vec<SessionVerdict>,
) {
    let mut covered: HashSet<String> = sessions
        .iter()
        .chain(stopped.iter())
        .map(|verdict| verdict.session_id.clone())
        .collect();
    for line in &dry.rejected {
        let Some(session_id) = line.session_id.as_deref() else {
            continue;
        };
        let names_recovery =
            may_hold_session(line.kind.as_deref()) || line.kind.as_deref() == Some("resume");
        if line.may_own_runtime
            || line.removal_intent
            || !names_recovery
            || !covered.insert(session_id.to_owned())
        {
            continue;
        }
        stopped.push(SessionVerdict {
            session_id: session_id.to_owned(),
            name: None,
            verdict: Verdict::WouldLoseRecovery,
            code: CODE_RECORD_UNREADABLE.to_owned(),
            detail: format!(
                "the record cannot be loaded and is skipped at startup, so the session's native \
                 recovery is lost: {}",
                line.reason
            ),
            acceptance_scope: None,
        });
    }
    stopped.sort_by(|left, right| left.session_id.cmp(&right.session_id));
}

/// Lists what unreadable journal evidence may hide: the whole root failing to
/// scan, and, while a store line that names no session cannot be read, journal
/// files the reader rejects.
fn journal_gaps(
    dry: &DryRun,
    journals: Result<&HashMap<String, WorkerJournalScan>, &String>,
    known: &HashSet<String>,
    verdicts: &mut BTreeMap<String, SessionVerdict>,
) {
    match journals {
        Ok(journals) if anonymous_loss(dry) => {
            // The unreadable store line may hold a session whose journal is
            // unreadable too, which leaves no live worker to see.
            for (session_id, scan) in journals {
                let Some((file, reject)) = scan.rejected.first() else {
                    continue;
                };
                if known.contains(session_id) || verdicts.contains_key(session_id) {
                    continue;
                }
                let (code, detail) = reject_reason(file, reject);
                verdicts.insert(
                    session_id.clone(),
                    SessionVerdict {
                        session_id: session_id.clone(),
                        name: None,
                        verdict: Verdict::WouldNotBeAdopted,
                        code: code.to_owned(),
                        detail: format!(
                            "{detail}, and a store line that names no session cannot be read; \
                             that line may be this session's record"
                        ),
                        acceptance_scope: None,
                    },
                );
            }
        }
        Ok(_) => {}
        Err(detail) => {
            verdicts.insert(JOURNAL_ROOT.to_owned(), evidence_unavailable(detail));
        }
    }
}

/// Whether a store line that names no session cannot be read, so it may be any
/// session's record.
fn anonymous_loss(dry: &DryRun) -> bool {
    dry.rejected
        .iter()
        .any(|line| line.session_id.is_none() && may_hold_session(line.kind.as_deref()))
}

/// Stands in for the sessions of a worker journal root that cannot be read.
const JOURNAL_ROOT: &str = "(worker journals)";

/// The entry of a journal root whose scan failed: with no journals to see, no
/// session can be called adoptable, even an empty one.
fn evidence_unavailable(detail: &str) -> SessionVerdict {
    SessionVerdict {
        session_id: JOURNAL_ROOT.to_owned(),
        name: None,
        verdict: Verdict::WouldNotBeAdopted,
        code: CODE_EVIDENCE_UNAVAILABLE.to_owned(),
        detail: format!("{detail}, so the live workers cannot be told"),
        acceptance_scope: None,
    }
}

/// The stable code and text of a journal file the reader rejects.
fn reject_reason(file: &str, reject: &JournalReject) -> (&'static str, String) {
    let name = if file.is_empty() { "directory" } else { file };
    match reject {
        JournalReject::UnsupportedSchema(found) => (
            CODE_JOURNAL_SCHEMA_UNSUPPORTED,
            format!("worker journal {name} has schema {found}, which this daemon does not read"),
        ),
        JournalReject::Unreadable | JournalReject::Mismatched => (
            CODE_JOURNAL_UNREADABLE,
            format!("worker journal {name} cannot be read or does not match its path"),
        ),
    }
}

/// Sessions outside `known` with a worker journal whose worker may still run.
fn live_journal_sessions(
    journals: &HashMap<String, WorkerJournalScan>,
    known: &HashSet<String>,
    inspector: &dyn ProcessInspector,
) -> Vec<String> {
    let mut sessions: Vec<String> = journals
        .iter()
        .filter(|(session_id, _scan)| !known.contains(*session_id))
        .filter(|(_session_id, scan)| {
            scan.evidence
                .iter()
                .any(|journal| !is_final(journal) && may_run(journal, inspector))
        })
        .map(|(session_id, _scan)| session_id.clone())
        .collect();
    sessions.sort();
    sessions
}

/// Whether the record already classifies the runtime as ended.
///
/// A lost runtime may keep the session's `running` state on disk (a loss that
/// was not proven ended), so the persisted runtime state decides: the upgrade
/// is not what loses it.
fn runtime_ended(record: &SessionRecord) -> bool {
    let ended = |state| matches!(state, RuntimeState::Lost | RuntimeState::Terminal);
    ended(record.runtime.state)
        || record
            .info
            .runtime
            .as_ref()
            .is_some_and(|runtime| ended(runtime.state))
}

/// Whether the journal's phase says its worker no longer owns a PTY child.
fn is_final(journal: &JournalEvidence) -> bool {
    matches!(
        journal.phase,
        JournalPhase::Terminal | JournalPhase::NeverInitialized | JournalPhase::Faulted
    )
}

/// Whether the journaled worker may still run; unprovable counts as running.
fn may_run(journal: &JournalEvidence, inspector: &dyn ProcessInspector) -> bool {
    journal.worker().identity().map_or(true, |identity| {
        inspector.is_running(identity).unwrap_or(true)
    })
}

/// Store-side evidence about one session beyond its record.
struct Facts<'a> {
    /// The separately stored resume binding.
    binding: Option<&'a ResumeBinding>,
    /// What the migration does to the session's native recovery.
    recovery: crate::store::RecoveryOutcome,
    /// Whether the session's resume line is skipped at load.
    resume_unreadable: bool,
}

type Judgement = (Verdict, String, String);

fn not_adopted(code: impl Into<String>, detail: impl Into<String>) -> Judgement {
    (Verdict::WouldNotBeAdopted, code.into(), detail.into())
}

/// What the preflight judges against: the runtimes the new daemon serves, the
/// profiles over them, and the base environment a launch inherits.
struct Judging<'a> {
    /// The schema of the store as the startup migration finds it, when the
    /// store is older than this release. A v0.34.x daemon writes schema 5.
    store_from: Option<u32>,
    runtimes: &'a RuntimeHost,
    profiles: &'a ProfileRegistry,
    /// The base environment, or the reason it cannot be built.
    base_env: &'a Result<BaseEnv, String>,
    /// The version probes this judgement has run or may still run.
    probes: &'a ProbeLedger,
}

/// The version probes of one judgement: the budget they share, and the outcome
/// of each distinct probe, so an executable that stalls costs one probe however
/// many sessions launch it.
struct ProbeLedger {
    deadline: Instant,
    /// The budget the deadline was computed from, for the messages that name it.
    budget: Duration,
    outcomes: RefCell<HashMap<ProbeKey, ProbeOutcome>>,
}

/// The outcome of a probe that finished: `Ok` when the executable passes, or
/// the refusal's code and detail.
type ProbeOutcome = Result<(), (String, String)>;

/// What one probe ran against: the program as the launch names it, the `PATH`
/// it runs under, and the executable of the package whose probe it is.
#[derive(PartialEq, Eq, Hash)]
struct ProbeKey {
    program: String,
    launch_path: Option<OsString>,
    probed: String,
}

impl ProbeLedger {
    fn new(deadline: Instant, budget: Duration) -> Self {
        Self {
            deadline,
            budget,
            outcomes: RefCell::new(HashMap::new()),
        }
    }

    /// The time left before the probe deadline, as of now.
    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// The outcome of the version probe one session needs.
enum Probe {
    /// The executable passes the probe of its runtime.
    Supported,
    /// The executable does not pass: resume refuses it with `code`.
    Refused { code: String, detail: String },
    /// The probe budget is spent before this probe could finish.
    BudgetExhausted,
}

/// Runs the version probe of `program` under `launch_path` for `definition`,
/// as resume validates the program it relaunches, unless the same probe
/// already has an outcome in `ledger` or the budget is spent.
fn run_probe(
    definition: &RuntimeDefinition,
    program: &str,
    launch_path: Option<&OsStr>,
    ledger: &ProbeLedger,
) -> Probe {
    let key = ProbeKey {
        program: program.to_owned(),
        launch_path: launch_path.map(OsStr::to_os_string),
        probed: probed_identity(definition),
    };
    if let Some(outcome) = ledger.outcomes.borrow().get(&key) {
        return match outcome {
            Ok(()) => Probe::Supported,
            Err((code, detail)) => Probe::Refused {
                code: code.clone(),
                detail: detail.clone(),
            },
        };
    }
    // Resolution alone runs no probe, so only a declared probe waits on the
    // budget; a program that does not resolve is refused as resume refuses it.
    let remaining = ledger.remaining();
    if definition.version_probe_parser().is_some() && remaining.is_zero() {
        if ValidatedLaunchProgram::resolve(program).is_none() {
            let error = ProtocolError::agent_binary_missing(&reported_program(definition, program));
            return Probe::Refused {
                code: error.code,
                detail: error.msg,
            };
        }
        return Probe::BudgetExhausted;
    }
    let result = validate_launch_runtime_with_timeout_detailed(
        definition,
        Spawn::Program(program),
        launch_path,
        remaining,
    );
    let outcome = match result {
        Ok(_) => Ok(()),
        // Only a probe the shortened budget cut short is budget exhaustion; a
        // probe that ran its full deadline is an executable that does not pass.
        Err(LaunchValidationFailure::ProbeTimedOut) => return Probe::BudgetExhausted,
        Err(LaunchValidationFailure::Missing) => {
            let error = ProtocolError::agent_binary_missing(&reported_program(definition, program));
            Err((error.code, error.msg))
        }
        Err(LaunchValidationFailure::Refused(refusal)) => {
            let error = unsupported_version_error(refusal);
            Err((error.code, error.msg))
        }
        Err(LaunchValidationFailure::Unsupported) => {
            let error = ProtocolError::agent_runtime_unsupported();
            Err((error.code, error.msg))
        }
    };
    ledger.outcomes.borrow_mut().insert(key, outcome.clone());
    match outcome {
        Ok(()) => Probe::Supported,
        Err((code, detail)) => Probe::Refused { code, detail },
    }
}

/// What a probe of `definition` is keyed by beyond its program: the executable
/// of the package that declares it.
fn probed_identity(definition: &RuntimeDefinition) -> String {
    match &definition.binding().provenance {
        BindingProvenance::Package { package_digest, .. } => package_digest.as_str().to_owned(),
        BindingProvenance::Builtin { .. } => format!("builtin:{}", definition.runtime_id()),
    }
}

/// The name a missing-executable refusal reports for `program`: its file name,
/// as the launch validation names it.
fn reported_program(definition: &RuntimeDefinition, program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_else(|| definition.runtime_id().as_str())
        .to_owned()
}

/// Judges one live session record against the journals of its session.
///
/// Returns the candidate the judgement ended on: the record after the resume
/// binding merge and every identity the judgement imported, or the record
/// itself when the judgement stopped before that.
fn judge(
    record: &SessionRecord,
    facts: &Facts<'_>,
    scan: Result<Option<&WorkerJournalScan>, &str>,
    inspector: &dyn ProcessInspector,
    judging: &Judging<'_>,
) -> (Judgement, SessionRecord) {
    let mut candidate = record.clone();
    let judgement = judge_candidate(record, &mut candidate, facts, scan, inspector, judging);
    (judgement, candidate)
}

fn judge_candidate(
    record: &SessionRecord,
    candidate: &mut SessionRecord,
    facts: &Facts<'_>,
    scan: Result<Option<&WorkerJournalScan>, &str>,
    inspector: &dyn ProcessInspector,
    judging: &Judging<'_>,
) -> Judgement {
    // Startup rolls back a create whose initial input a previous daemon never
    // delivered; no new daemon instance can own that marker.
    if initial_input_orphaned(record, "") {
        return not_adopted(
            CODE_CREATE_ROLLED_BACK,
            "the record is a create whose initial input was never delivered, which startup \
             rolls back",
        );
    }
    // A stop or removal the owner asked for is finished at startup, not adopted.
    if record.desired_state != DesiredState::Running {
        return judge_intent(record, candidate, scan, inspector, judging);
    }
    if let Some(binding) = facts.binding.cloned() {
        if let Err(reason) = merge_persisted_recovery(candidate, binding) {
            return not_adopted(
                reason,
                "the record contradicts its stored resume binding, so the runtime is quarantined",
            );
        }
    }
    let generation = match Generation::from_record(&record.session_id, &record.runtime) {
        Ok(Some(generation)) => generation,
        Ok(None) => {
            return not_adopted(
                CODE_GENERATION_MISSING,
                "the record names no worker generation, so no journal can bind a live worker to it",
            );
        }
        Err(error) => {
            return not_adopted(IDENTITY_MISMATCH, error.msg);
        }
    };
    let scan = match scan {
        Ok(scan) => scan.cloned().unwrap_or_default(),
        Err(detail) => return not_adopted(CODE_JOURNAL_UNREADABLE, detail),
    };
    let scoped = scan.scoped_to(Some(&generation));
    let journal = match select_journal(record, &scan, &scoped) {
        Ok(journal) => journal,
        Err(judgement) => return judgement,
    };
    if runtime_identity_conflict(
        record,
        &journal.worker_id,
        journal.worker_instance_id.as_deref(),
    ) {
        return not_adopted(
            CODE_RUNTIME_IDENTITY_MISMATCH,
            format!(
                "the journal of generation {} names worker {} (runtime {:?}), which is not the \
                 runtime the record binds",
                generation.generation(),
                journal.worker_id,
                journal.worker_instance_id
            ),
        );
    }
    if recovery_not_advanced(record, &journal.worker_id) {
        return not_adopted(
            CODE_WORKER_NOT_ADVANCED,
            "the record's recovery waits for a new worker generation but the journal names the \
             worker it replaces",
        );
    }
    // A worker that has not initialized is imported as ended; a starting or
    // live one is adopted only with its PTY child.
    match journal.phase {
        JournalPhase::Bootstrap => {
            return not_adopted(
                CODE_WORKER_NOT_INITIALIZED,
                "the worker journal records a worker that never initialized, so the runtime is \
                 imported as ended",
            );
        }
        JournalPhase::Starting | JournalPhase::Live if journal.child.is_none() => {
            return not_adopted(
                CODE_WORKER_CHILD_MISSING,
                "the worker journal records no PTY child, so the runtime has nothing to adopt",
            );
        }
        _ => {}
    }
    judge_adoption(facts, journal, candidate, inspector, judging)
}

/// Finishing an owner intent is expected, but never authorizes a foreign or
/// unprovable process. Missing or ended workers need no adoption; a live one
/// still needs the identity evidence reconciliation uses before stopping it.
fn judge_intent(
    record: &SessionRecord,
    candidate: &mut SessionRecord,
    scan: Result<Option<&WorkerJournalScan>, &str>,
    inspector: &dyn ProcessInspector,
    judging: &Judging<'_>,
) -> Judgement {
    let finished = || {
        (
            Verdict::Adoptable,
            CODE_INTENT_FINISHED.to_owned(),
            "the owner requested a stop or removal, which startup finishes instead of adopting \
             the runtime; any retained native recovery is judged separately"
                .to_owned(),
        )
    };
    let generation = match Generation::from_record(&record.session_id, &record.runtime) {
        Ok(generation) => generation,
        Err(error) => return not_adopted(IDENTITY_MISMATCH, error.msg),
    };
    let scan = match scan {
        Ok(Some(scan)) => scan,
        Ok(None) => return finished(),
        Err(detail) => return not_adopted(CODE_JOURNAL_UNREADABLE, detail),
    };
    if let Some((file, reject)) = scan.rejected.first() {
        let (code, detail) = reject_reason(file, reject);
        return not_adopted(code, detail);
    }
    let Some(generation) = generation else {
        return not_adopted(
            CODE_GENERATION_MISSING,
            "the requested stop or removal has journal evidence but no bound generation",
        );
    };
    let scoped = scan.scoped_to(Some(&generation));
    let journal = match scoped.evidence.as_slice() {
        [] => return finished(),
        [journal] => journal,
        _ => {
            return not_adopted(
                CODE_JOURNAL_AMBIGUOUS,
                "several worker journals name the record's generation",
            );
        }
    };
    if runtime_identity_conflict(
        record,
        &journal.worker_id,
        journal.worker_instance_id.as_deref(),
    ) {
        return not_adopted(
            CODE_RUNTIME_IDENTITY_MISMATCH,
            "the worker journal contradicts the runtime the stop or removal intent binds",
        );
    }
    if is_final(journal) {
        return finished();
    }
    if let Some(judgement) = judge_process(journal, inspector) {
        return if judgement.1 == CODE_WORKER_NOT_RUNNING {
            finished()
        } else {
            judgement
        };
    }
    if let Some(judgement) = judge_marker(journal) {
        return judgement;
    }
    if let Some(judgement) = judge_protocol(journal) {
        return judgement;
    }
    judge_identity(journal, candidate, inspector, judging.runtimes).unwrap_or_else(finished)
}

/// The live-worker evidence after the record's identity and generation are
/// bound to the journal: the process, marker, protocol, identity and runtime
/// source, then the native recovery.
fn judge_adoption(
    facts: &Facts<'_>,
    journal: &JournalEvidence,
    candidate: &mut SessionRecord,
    inspector: &dyn ProcessInspector,
    judging: &Judging<'_>,
) -> Judgement {
    if let Some(judgement) = judge_process(journal, inspector) {
        return judgement;
    }
    if let Some(judgement) = judge_marker(journal) {
        return judgement;
    }
    if let Some(judgement) = judge_protocol(journal) {
        return judgement;
    }
    if let Some(judgement) = judge_identity(journal, candidate, inspector, judging.runtimes) {
        return judgement;
    }
    if let Some(judgement) = judge_runtime_source(candidate, judging.runtimes) {
        return judgement;
    }
    match recovery_loss(facts, candidate, judging) {
        Some(loss) => loss,
        None => (
            Verdict::Adoptable,
            CODE_ADOPTABLE.to_owned(),
            "the worker journal, process, protocol range and stored recovery are consistent"
                .to_owned(),
        ),
    }
}

/// Picks the one live journal of the record's generation.
fn select_journal<'a>(
    record: &SessionRecord,
    scan: &WorkerJournalScan,
    scoped: &'a WorkerJournalScan,
) -> Result<&'a JournalEvidence, Judgement> {
    let live: Vec<&JournalEvidence> = scoped
        .evidence
        .iter()
        .filter(|journal| !is_final(journal))
        .collect();
    match live.as_slice() {
        [journal] => Ok(journal),
        [_, _, ..] => Err(not_adopted(
            CODE_JOURNAL_AMBIGUOUS,
            "several worker journals name the record's generation",
        )),
        [] if !scoped.evidence.is_empty() => Err(not_adopted(
            CODE_WORKER_ENDED,
            "the worker journal of the record's generation records an ended worker",
        )),
        [] => Err(missing_journal(record, scan)),
    }
}

/// Explains why no journal names the record's generation: the file of the
/// record's worker is rejected, or there is none.
fn missing_journal(record: &SessionRecord, scan: &WorkerJournalScan) -> Judgement {
    let expected = record.runtime.worker_id.as_deref();
    let rejected = scan.rejected.iter().find(|(file, _reject)| {
        expected.is_none_or(|worker_id| file.strip_suffix(".json") == Some(worker_id))
            || file.is_empty()
    });
    match rejected {
        Some((file, JournalReject::UnsupportedSchema(found))) => not_adopted(
            CODE_JOURNAL_SCHEMA_UNSUPPORTED,
            format!(
                "worker journal {file} has schema {found}, which this daemon does not read, so \
                 its worker cannot be bound to the record"
            ),
        ),
        Some((file, JournalReject::Unreadable | JournalReject::Mismatched)) => not_adopted(
            CODE_JOURNAL_UNREADABLE,
            format!(
                "worker journal {} cannot be read or does not match its path, so its worker \
                 cannot be bound to the record",
                if file.is_empty() { "directory" } else { file }
            ),
        ),
        None => not_adopted(
            CODE_JOURNAL_MISSING,
            "no worker journal names the record's generation",
        ),
    }
}

/// The process-table evidence: the journaled worker must provably run.
fn judge_process(journal: &JournalEvidence, inspector: &dyn ProcessInspector) -> Option<Judgement> {
    let identity = match journal.worker().identity() {
        Ok(identity) => identity,
        Err(detail) => return Some(not_adopted(CODE_WORKER_UNVERIFIABLE, detail)),
    };
    match inspector.is_running(identity) {
        Ok(true) => None,
        Ok(false) => Some(not_adopted(
            CODE_WORKER_NOT_RUNNING,
            format!(
                "worker process {} named by the journal is not running, so the runtime is already gone",
                identity.pid
            ),
        )),
        Err(error) => Some(not_adopted(
            CODE_WORKER_UNVERIFIABLE,
            format!(
                "worker process {} cannot be inspected: {error}",
                identity.pid
            ),
        )),
    }
}

/// The journaled worker release must mark its children with the
/// worker-instance marker; startup refuses any other live worker.
fn judge_marker(journal: &JournalEvidence) -> Option<Judgement> {
    (!journal.marks_worker_instance()).then(|| {
        not_adopted(
            CODE_WORKER_MARKER_LEGACY,
            format!(
                "the worker journal names release {}, which predates the worker-instance marker, \
                 so the new daemon cannot attribute the runtime's processes and does not adopt \
                 it; stop the session with the running release first, or upgrade with \
                 --accept-runtime-loss",
                journal.release()
            ),
        )
    })
}

/// The worker's identity claims and subagent state, judged as adoption
/// judges the snapshot the live worker would report.
///
/// The claimed processes must still be in the PTY tree
/// ([`validate_identity_claims`]), and the claims must import into the record
/// ([`import_worker_identities`], [`import_worker_subagents`]). A process
/// table that cannot be read fails closed.
fn judge_identity(
    journal: &JournalEvidence,
    candidate: &mut SessionRecord,
    inspector: &dyn ProcessInspector,
    runtimes: &RuntimeHost,
) -> Option<Judgement> {
    let mut snapshot = match journal.snapshot() {
        Ok(snapshot) => snapshot,
        Err(reason) => {
            return Some(not_adopted(
                reason,
                "the worker journal holds an identity that cannot be read",
            ));
        }
    };
    let claims = IdentityClaims::from_snapshot(&snapshot);
    if claims.has_claims() {
        if let Err(failure) = validate_identity_claims(inspector, &claims) {
            let (reason, _retryable) = failure.reason_and_retryability();
            return Some(not_adopted(
                reason,
                "the launch or active identity process the journal names is not in the \
                 worker's PTY tree, or the process table cannot prove it is",
            ));
        }
    }
    // A worker that journals no schema is validated with the schema its
    // runtime declares, as adoption does.
    if snapshot.hook_schema.is_none()
        && (snapshot.active_identity.is_some() || snapshot.active_identity_release.is_some())
    {
        match record_hook_schema(candidate, runtimes) {
            HookSchemaLookup::Schema(schema) => snapshot.hook_schema = Some(schema.id.to_owned()),
            HookSchemaLookup::NoIntegration => {}
            HookSchemaLookup::Unresolved(error) => {
                return Some(not_adopted(
                    CODE_IDENTITY_UNVERIFIED,
                    format!(
                        "the worker journaled an active identity without a hook schema, and \
                         this daemon serves no \
                         definition of the session's launch pin to supply one ({}: {}); the \
                         session's runtime source is not served or cannot be read, so the \
                         identity cannot be verified",
                        error.code, error.msg
                    ),
                ));
            }
        }
    }
    // Startup imports the journaled native reference before the identities, so
    // the candidate holds the reference the replacement daemon persists.
    import_native_reference(candidate, &snapshot);
    if let Err(reason) = import_worker_identities(candidate, &snapshot) {
        return Some(not_adopted(
            reason,
            "the identity the worker journaled contradicts the stored record",
        ));
    }
    import_worker_subagents(&snapshot).err().map(|reason| {
        not_adopted(
            reason,
            "the subagent state the worker journaled cannot be imported",
        )
    })
}

/// The worker protocol range the journal recorded must share a version with
/// this daemon.
fn judge_protocol(journal: &JournalEvidence) -> Option<Judgement> {
    let range = journal
        .protocol_minimum
        .zip(journal.protocol_maximum)
        .and_then(|(minimum, maximum)| {
            VersionRange::new(
                Version::new(u32::from(minimum)).ok()?,
                Version::new(u32::from(maximum)).ok()?,
            )
            .ok()
        });
    let Some(range) = range else {
        return Some(not_adopted(
            CODE_PROTOCOL_INCOMPATIBLE,
            "the worker journal records no valid private protocol range",
        ));
    };
    negotiate(SUPPORTED_RANGE, range).err().map(|error| {
        not_adopted(
            CODE_PROTOCOL_INCOMPATIBLE,
            format!("the worker speaks a private protocol this daemon cannot: {error}"),
        )
    })
}

/// The reason the session loses its native recovery, or `None` when it keeps
/// it: the stored outcome of the migration, an unreadable resume record, the
/// registry-dependent launch spec, the launch pin and the version probe.
fn recovery_loss(
    facts: &Facts<'_>,
    candidate: &SessionRecord,
    judging: &Judging<'_>,
) -> Option<Judgement> {
    if facts.recovery == RecoveryOutcome::Lost {
        return Some((
            Verdict::WouldLoseRecovery,
            CODE_RECOVERY_UNMAPPABLE.to_owned(),
            "the stored native recovery cannot be mapped to the current launch shape, so the \
             session cannot resume natively"
                .to_owned(),
        ));
    }
    // Startup skips an unreadable projection and keeps the record's own
    // recovery, so only a record that depends on the projection loses it.
    if facts.resume_unreadable && candidate.recovery.is_none() {
        return Some((
            Verdict::WouldLoseRecovery,
            CODE_RESUME_RECORD_UNREADABLE.to_owned(),
            "the stored resume record of the session cannot be loaded, so native recovery is \
             not restored from it"
                .to_owned(),
        ));
    }
    if facts.recovery == RecoveryOutcome::NeedsRegistry {
        return Some((
            Verdict::WouldLoseRecovery,
            CODE_RECOVERY_UNVERIFIED.to_owned(),
            "the stored native recovery needs the runtime registry to complete its launch spec; \
             that cannot be verified before the upgrade"
                .to_owned(),
        ));
    }
    judge_pin(candidate, judging.profiles).or_else(|| judge_launch(candidate, judging))
}

/// The launch resume runs before it relaunches a session: the host profile the
/// session was launched from, resolved and checked against its frozen revision
/// by the function resume itself runs, then the executable validation resume
/// runs for the frozen program. A declared version probe runs under the
/// environment that profile gives the agent; a runtime without one is only
/// checked for an executable.
///
/// Only a session that resumes or forks reaches it. A profile refusal carries
/// resume's own code, and so does a probe refusal, so the verdict names the
/// refusal resume would return.
fn judge_launch(candidate: &SessionRecord, judging: &Judging<'_>) -> Option<Judgement> {
    let binding = candidate.recovery.as_ref()?;
    let capabilities = &candidate.info.capabilities;
    if !capabilities.resume && !capabilities.fork {
        return None;
    }
    let definition = recovery_definition(judging.profiles, binding);
    let mut profile_env = None;
    for (fork, required) in [(false, capabilities.resume), (true, capabilities.fork)] {
        if !required {
            continue;
        }
        match recovery_profile_in(
            judging.profiles,
            binding,
            definition.as_ref().ok(),
            ProfileChange::Refuse,
            fork,
        ) {
            Ok(profile) => {
                profile_env.get_or_insert(profile.env);
            }
            Err(error) => {
                return Some((
                    Verdict::WouldLoseRecovery,
                    error.code.clone(),
                    format!(
                        "resume refuses the session's host profile ({}: {}), so the session \
                         cannot resume natively; restore the profile it was launched from, or \
                         upgrade with --accept-runtime-loss",
                        error.code, error.msg
                    ),
                ));
            }
        }
    }
    let profile_env = profile_env.unwrap_or_default();
    let definition = definition.ok()?;
    // Resume builds the base environment for every relaunch and validates the
    // frozen executable for every relaunch; only a declared version probe reads
    // the `PATH` the agent launches under.
    let base = match judging.base_env {
        Ok(base) => base,
        Err(detail) => {
            return Some((
                Verdict::WouldLoseRecovery,
                CODE_WORKER_INITIALIZE_INVALID.to_owned(),
                format!(
                    "the launch environment cannot be built ({detail}), so resume cannot probe \
                     the agent it would run"
                ),
            ));
        }
    };
    let launch_path = definition
        .version_probe_parser()
        .is_some()
        .then(|| effective_variable(base, profile_env.as_slice(), "PATH"))
        .flatten();
    let program = binding_program(binding, &definition);
    match run_probe(
        &definition,
        &program,
        launch_path.as_deref(),
        judging.probes,
    ) {
        Probe::Supported => None,
        Probe::Refused { code, detail } => Some((
            Verdict::WouldLoseRecovery,
            code.clone(),
            format!(
                "the agent the session launches from does not pass the version probe of its \
                 runtime ({code}: {detail}), so resume and fork refuse it; install a supported \
                 release of the agent, or upgrade with --accept-runtime-loss"
            ),
        )),
        Probe::BudgetExhausted => Some((
            Verdict::WouldLoseRecovery,
            CODE_PROBE_BUDGET_EXHAUSTED.to_owned(),
            format!(
                "the version probes of this upgrade's judgement used their total budget of {} \
                 seconds before this session's agent was probed, so its recovery cannot be \
                 verified; stop the agent that hangs its version probe, run `pohunek service \
                 check|upgrade --preflight-budget <SECONDS>` with a larger budget, or upgrade \
                 with --accept-runtime-loss",
                judging.probes.budget.as_secs()
            ),
        )),
    }
}

/// Every interaction with an adopted session needs its runtime source served.
///
/// Input, attach, resize, rename and metadata changes all pass the mutation
/// guard (`RuntimeHost::ensure_session_runtime`), which refuses a session whose
/// runtime resolves nowhere: a built-in of an official alias this binary does
/// not compile in while no package serves the alias, or a runtime that is not
/// installed. Such a session is judged with the guard itself, whatever its
/// resume and fork capabilities. Its worker is adopted, so the verdict is
/// `would_lose_recovery` with the code of an unserved runtime source, like a
/// session whose pin only resume and fork refuse, and the detail names what
/// is refused beyond them.
fn judge_runtime_source(candidate: &SessionRecord, runtimes: &RuntimeHost) -> Option<Judgement> {
    let pin = candidate
        .recovery
        .as_ref()
        .map(|binding| binding.launch_binding.clone())
        .unwrap_or_default();
    let agent_base = &candidate.info.agent_base;
    let error = runtimes.ensure_session_runtime(agent_base, &pin).err()?;
    let runtime = agent_base.as_wire();
    let outcome = match agent_base.launchable() {
        // Nothing serves the alias, so only the official package of the built-in's
        // package identity can serve the session (`launch::adopts`).
        Ok(runtime_id) if error.code == "runtime_builtin_removed" => format!(
            "every interaction with the session (input, attach, resize, rename, metadata) is \
             refused until you {}; the official package then adopts the session, and resume \
             and fork run through it when it honors the launch the session froze (otherwise \
             they answer `runtime_served_by_package` and the work continues in a new session \
             started with `pohunek session new --agent {runtime}`)",
            protocol::official_package_install_step(runtime_id)
        ),
        _ => format!(
            "every interaction with the session (input, attach, resize, rename, metadata) and \
             resume and fork are refused until the runtime it was launched from is installed \
             and enabled again; otherwise the session cannot be resumed through another \
             runtime source, so continue in a new session started with `pohunek session new \
             --agent {runtime}` once this daemon serves that runtime"
        ),
    };
    Some((
        Verdict::WouldLoseRecovery,
        CODE_RUNTIME_SOURCE_NOT_SERVED.to_owned(),
        format!(
            "the session was launched from a runtime source this daemon does not serve ({}: \
             {}), so the worker is adopted but {outcome}, or upgrade with \
             --accept-runtime-loss",
            error.code, error.msg
        ),
    ))
}

/// The session's launch pin must name a runtime source the new daemon serves:
/// resume and fork resolve exactly that source, and nothing else stands in.
///
/// The pin is resolved as resume resolves it ([`recovery_definition`]), against
/// the built-in runtimes of this binary and the packages installed now, so a
/// package digest that is not installed and a package that does not honor the
/// frozen launch each lose the session's recovery, and a fork the package
/// does not honor is judged the same way ([`recovery_contract`]). A session
/// without native resume or fork has no recovery to lose. A runtime source no
/// interaction is served from is judged first, by [`judge_runtime_source`].
fn judge_pin(candidate: &SessionRecord, profiles: &ProfileRegistry) -> Option<Judgement> {
    let binding = candidate.recovery.as_ref()?;
    let capabilities = &candidate.info.capabilities;
    if !capabilities.resume && !capabilities.fork {
        return None;
    }
    let error = match recovery_definition(profiles, binding) {
        Err(error) => error,
        Ok(definition) if capabilities.fork => {
            recovery_contract(binding, &definition, true).err()?
        }
        Ok(_) => return None,
    };
    let runtime = binding.agent_base.as_wire();
    Some((
        Verdict::WouldLoseRecovery,
        CODE_RUNTIME_SOURCE_NOT_SERVED.to_owned(),
        format!(
            "the session was launched from a runtime source this daemon does not serve ({}: \
             {}), so the worker is adopted but its recovery is refused; a session cannot \
             be resumed through another runtime source, so finish the work in the live session and \
             continue in a new session started with `pohunek session new --agent {runtime}` \
             while this daemon serves that runtime, or upgrade with --accept-runtime-loss",
            error.code, error.msg
        ),
    ))
}
