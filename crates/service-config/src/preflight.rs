//! The report `pohunekd upgrade-preflight` prints and `pohunek service
//! check|upgrade` reads.
//!
//! The new daemon binary judges, from the installed state on disk, whether it
//! would adopt every live session the running daemon owns. The report is the
//! one contract between that binary and the installer, so both link these
//! types instead of mirroring them. Reason codes are stable machine strings:
//! the ones the daemon's reconciliation already uses keep their spelling, the
//! rest are the `CODE_*` constants below.

// Rust guideline compliant 2026-10-12

use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::runtimes::{EnsureReport, Refusal};

/// Argument that adds the release runtime plan to the judgement of
/// `pohunekd upgrade-preflight`. Without it the judgement serves the installed
/// packages only and the report carries no plan.
///
/// Only the upgrade's own gates pass it. A rollback runs the previous release's
/// binary without it, because that binary never applies the plan.
pub const RELEASE_PLAN_FLAG: &str = "--release-plan";

/// Deadline the installer gives one `pohunekd upgrade-preflight` run when the
/// probe budget is the default [`PROBE_BUDGET`].
///
/// A longer probe budget raises the deadline by the same amount, see
/// [`preflight_timeout`].
pub const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default time one preflight run may spend on version probes and reference
/// checks, counted from the entry of the command.
///
/// The probe deadline is the entry of `pohunekd upgrade-preflight` plus this
/// budget, so the journal scan, the release plan and the registry load count
/// against it too. A session whose probe would start at or after the deadline
/// is judged `would_lose_recovery` with [`CODE_PROBE_BUDGET_EXHAUSTED`]; a
/// stopped session whose native reference is not checked before the deadline
/// is judged `would_lose_recovery` with [`CODE_REFERENCE_UNVERIFIED`]. The
/// judgement stays pessimistic instead of running past the installer deadline.
/// The operator raises the budget with `--preflight-budget` up to
/// [`MAX_PROBE_BUDGET`].
pub const PROBE_BUDGET: Duration = Duration::from_secs(20);

/// Smallest and largest probe budget an operator may pass, in whole seconds.
pub const MIN_PROBE_BUDGET_SECS: u64 = 1;
/// See [`MIN_PROBE_BUDGET_SECS`].
pub const MAX_PROBE_BUDGET_SECS: u64 = 300;

/// Time reserved after the probe deadline for the rest of the judgement and for
/// the report to be printed. A probe that started before the deadline ends by
/// its own bounded timeout, so the run ends within this margin of the deadline.
pub const REPORT_MARGIN: Duration = Duration::from_secs(5);

/// The installer's deadline for one preflight run with `budget` as its probe
/// budget: the default deadline, or the budget plus the report margin when that
/// is longer.
#[must_use]
pub fn preflight_timeout(budget: Duration) -> Duration {
    PREFLIGHT_TIMEOUT.max(budget.saturating_add(REPORT_MARGIN))
}

// The default probe deadline and the report margin must end before the default
// installer deadline.
const _: () =
    assert!(PROBE_BUDGET.as_secs() + REPORT_MARGIN.as_secs() < PREFLIGHT_TIMEOUT.as_secs());

/// Argument that passes the probe budget, in whole seconds, to the daemon's
/// `upgrade-preflight` run.
pub const PROBE_BUDGET_FLAG: &str = "--probe-budget-secs";

/// Machine code of a session whose version probe the probe budget did not
/// reach.
pub const CODE_PROBE_BUDGET_EXHAUSTED: &str = "probe_budget_exhausted";

/// Machine code of a stopped session whose native reference the probe budget
/// did not reach. The reference is unverified, so it is never adoptable and is
/// never reported as already not resumable.
pub const CODE_REFERENCE_UNVERIFIED: &str = "reference_unverified_within_budget";

/// Machine code of a host profile whose override manifest has no idle rule on
/// a screen region, over a base that declares `input.ready = "screen_idle"`.
/// The profile still loads, with bracketed-paste readiness.
pub const CODE_PROFILE_WITHOUT_SCREEN_READINESS: &str = "profile_without_screen_readiness";

/// Human text of [`CODE_PROFILE_WITHOUT_SCREEN_READINESS`], shared by the
/// doctor check and the preflight warning so both name the same fix.
pub const PROFILE_WITHOUT_SCREEN_READINESS_DETAIL: &str = "The override manifest has no idle rule on a screen region, so the sessions write the initial prompt once bracketed paste is on instead of waiting for the prompt to render, and the agent may drop it; add an idle rule on a screen region to the override, such as the package's live_prompt_box rule on prompt_box_body.";

/// Version of the report layout; a reader refuses any other value.
///
/// Stays at 1 while the layout only gains fields, because the rollback gate
/// parses reports of older daemons, which ignore fields they do not know.
pub const REPORT_VERSION: u32 = 1;

/// Name of the acceptance-scope scheme of [`SessionVerdict::acceptance_scope`],
/// the prefix before the digest: `v1:` followed by the lowercase hex SHA-256
/// of a domain-separated, length-prefixed encoding.
///
/// The digest covers the facts a durable acceptance of an at-risk verdict
/// stays bound to, taken from the record the judgement ended on (after the
/// resume-binding merge and the identities the judgement imported, so the
/// replacement daemon's startup reproduces it):
/// - the list (live or stopped), the verdict and its code;
/// - the session id, the runtime generation and the worker instance;
/// - the agent base, the launch pin (including the package or descriptor
///   digest), and digests of the native session id and path, so no raw
///   reference leaves the daemon;
/// - whether the session can resume and fork.
///
/// It leaves out everything startup rewrites (runtime state and reason,
/// transactions, identity ordering, process ids), the human `detail` and
/// `name`, and any environment or credential value.
///
/// The scope is absent for a verdict that is not at risk, for every verdict
/// without a loaded record, and for each code a differential case does not
/// prove stable across startup; the budget-limited codes
/// [`CODE_PROBE_BUDGET_EXHAUSTED`] and [`CODE_REFERENCE_UNVERIFIED`] and the
/// unprovable-process code [`CODE_WORKER_UNVERIFIABLE`] are never eligible.
///
/// The scheme is a cross-release contract: a daemon of a later release must
/// reproduce the digest of an earlier one for identical facts. A change to the
/// facts or their encoding needs a new scheme name, and a reader treats an
/// unknown scheme, and a verdict without a scope, as not accepted.
pub const ACCEPTANCE_SCOPE_V1: &str = "v1";

/// The session is adopted with its native recovery intact.
pub const CODE_ADOPTABLE: &str = "adoptable";
/// The stopped session resumes and forks through a newer version of the
/// official package it was launched from, which adopts it; the detail names
/// both versions.
pub const CODE_ADOPTABLE_BY_NEWER_PACKAGE: &str = "adoptable_by_newer_package";
/// The session record is skipped when the store loads.
pub const CODE_RECORD_UNREADABLE: &str = "record_unreadable";
/// A live worker names no session record, and a store line that names no
/// session could not be read, so it may be that session's.
pub const CODE_NO_SESSION_RECORD: &str = "worker_without_session_record";
/// The record names no worker generation.
pub const CODE_GENERATION_MISSING: &str = "worker_generation_missing";
/// A journal of the session has an unsupported schema.
pub const CODE_JOURNAL_SCHEMA_UNSUPPORTED: &str = "worker_journal_schema_unsupported";
/// A journal of the session cannot be read or does not match its path.
pub const CODE_JOURNAL_UNREADABLE: &str = "worker_journal_unreadable";
/// No journal names the record's generation.
pub const CODE_JOURNAL_MISSING: &str = "worker_journal_missing";
/// Several journals name the record's generation.
pub const CODE_JOURNAL_AMBIGUOUS: &str = "worker_journal_ambiguous";
/// The journal of the record's generation records an ended worker.
pub const CODE_WORKER_ENDED: &str = "worker_already_ended";
/// The journal's worker process is gone.
pub const CODE_WORKER_NOT_RUNNING: &str = "worker_not_running";
/// The process table could not prove whether the worker runs.
pub const CODE_WORKER_UNVERIFIABLE: &str = "worker_process_unverifiable";
/// The journal names another worker or runtime instance than the record.
pub const CODE_RUNTIME_IDENTITY_MISMATCH: &str = "runtime_identity_mismatch";
/// The record's recovery waits for a worker other than the live one.
pub const CODE_WORKER_NOT_ADVANCED: &str = "worker_generation_not_advanced";
/// The worker never finished initializing, so the new daemon imports it as ended.
pub const CODE_WORKER_NOT_INITIALIZED: &str = "worker_not_initialized";
/// The worker journals cannot be read at all, so no live worker can be told.
pub const CODE_EVIDENCE_UNAVAILABLE: &str = "evidence_unavailable";
/// The record is a create whose initial input was never delivered; startup
/// rolls it back.
pub const CODE_CREATE_ROLLED_BACK: &str = "create_rolled_back";
/// The record carries a stop or removal intent startup finishes.
pub const CODE_INTENT_FINISHED: &str = "stop_or_removal_intent_pending";
/// The worker journaled an identity whose hook schema only the runtime
/// registry can supply.
pub const CODE_IDENTITY_UNVERIFIED: &str = "worker_identity_unverified";
/// The live worker's journal records no PTY child.
pub const CODE_WORKER_CHILD_MISSING: &str = "worker_child_missing";
/// The worker's protocol range shares no version with the new daemon.
pub const CODE_PROTOCOL_INCOMPATIBLE: &str = "worker_protocol_incompatible";
/// The worker's journaled release predates `POHUNEK_WORKER_INSTANCE_ID`, or
/// the journal names no release, so its children carry no worker-instance
/// marker the new daemon reads. Startup reports it with the same reason.
pub const CODE_WORKER_MARKER_LEGACY: &str = "worker_marker_legacy";
/// The native recovery binding cannot be mapped to the current launch shape.
pub const CODE_RECOVERY_UNMAPPABLE: &str = "native_recovery_unmappable";
/// The native recovery binding needs the runtime registry to be completed.
pub const CODE_RECOVERY_UNVERIFIED: &str = "native_recovery_unverified";
/// The session's launch pin names a runtime source the new daemon does not
/// serve: a built-in runtime it does not ship or an official package serves
/// in its place, or a package digest that is not installed. The worker is
/// adopted, but resume and fork are refused, because no other source stands
/// in for the pinned one. While the session's runtime resolves nowhere (a
/// built-in it does not ship and no package serves, or a runtime that is not
/// installed), every interaction with the session is refused too, whether or
/// not it can resume.
pub const CODE_RUNTIME_SOURCE_NOT_SERVED: &str = "runtime_source_not_served";
/// The metadata store is unusable, so the new daemon cannot start.
pub const CODE_STORE_UNUSABLE: &str = "store_unusable";
/// The separately stored resume record of the session is skipped when the
/// store loads.
pub const CODE_RESUME_RECORD_UNREADABLE: &str = "resume_record_unreadable";

/// A host profile the new daemon would load with a reduced guarantee. It never
/// refuses the upgrade and is not part of the session verdicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileWarning {
    /// Name of the host profile.
    pub profile: String,
    /// Stable machine reason.
    pub code: String,
    /// Human explanation naming the profile, its manifest and the fix.
    pub detail: String,
}

/// What the new daemon would do with one live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Verdict {
    /// The worker is adopted and the session keeps its native recovery.
    Adoptable,
    /// The worker is adopted but the session can no longer resume natively.
    WouldLoseRecovery,
    /// The worker is not adopted: its runtime is lost, quarantined or left
    /// unmanaged.
    WouldNotBeAdopted,
    /// The stopped session's native reference is missing or relocated, and
    /// the upgrade does not change that: resume refuses it on the running
    /// release as well, so the upgrade loses no recovery.
    AlreadyNotResumable,
}

impl Verdict {
    /// Whether the upgrade must be refused unless the operator accepts loss.
    #[must_use]
    pub const fn at_risk(self) -> bool {
        !matches!(self, Self::Adoptable | Self::AlreadyNotResumable)
    }

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Adoptable => "adoptable",
            Self::WouldLoseRecovery => "would_lose_recovery",
            Self::WouldNotBeAdopted => "would_not_be_adopted",
            Self::AlreadyNotResumable => "already_not_resumable",
        }
    }
}

impl Display for Verdict {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The adoption evidence of one live session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionVerdict {
    /// Logical session id; for a worker without a record, the journal's.
    pub session_id: String,
    /// Owner-set display name, when the record has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What would happen to the session.
    pub verdict: Verdict,
    /// Stable machine reason.
    pub code: String,
    /// Human explanation naming the evidence.
    pub detail: String,
    /// Digest of the facts a durable acceptance of this at-risk verdict is
    /// bound to; see [`ACCEPTANCE_SCOPE_V1`]. Absent when nothing can durably
    /// accept the verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_scope: Option<String>,
}

/// What the store dry run found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StoreState {
    /// There is no store file yet.
    Missing,
    /// Every record has the current schema; startup writes nothing.
    UpToDate,
    /// Startup rewrites the store at the current schema and keeps a backup.
    WouldMigrate,
    /// The daemon refuses to start with this store.
    Refused,
}

/// The dry-run migration of the metadata store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreReport {
    /// The store file.
    pub path: PathBuf,
    /// Outcome of the dry run.
    pub state: StoreState,
    /// Oldest schema found; `None` without records.
    pub schema_from: Option<u32>,
    /// Schema the new daemon writes.
    pub schema_to: u32,
    /// Records the migration carries over.
    pub records: usize,
    /// Machine code of the refusal, for [`StoreState::Refused`].
    pub error_code: Option<String>,
    /// Human text of the refusal, for [`StoreState::Refused`].
    pub error: Option<String>,
}

/// The preflight of one installed state against one daemon binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightReport {
    /// Must equal [`REPORT_VERSION`].
    pub report_version: u32,
    /// Version of the daemon that judged.
    pub daemon_version: String,
    /// Dry-run store migration.
    pub store: StoreReport,
    /// Every live session, ordered by id.
    pub sessions: Vec<SessionVerdict>,
    /// Sessions whose runtime has ended and that keep native resume or fork,
    /// ordered by id. Each verdict says whether the session still resumes or
    /// forks through the runtime source the new daemon serves.
    #[serde(default)]
    pub stopped_sessions: Vec<SessionVerdict>,
    /// Sessions whose worker journal is live but which the store has no
    /// record of, ordered by id. The running daemon does not manage them
    /// either, so the upgrade leaves them as they are.
    #[serde(default)]
    pub unmanaged_workers: Vec<String>,
    /// The release step's plan against this host: the packages and profiles
    /// it changes, and its refusals. Absent when the release carries no
    /// `runtime/` directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtimes: Option<EnsureReport>,
    /// Host profiles the new daemon loads with a reduced guarantee, ordered by
    /// name. They never refuse the upgrade and do not change any verdict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profile_warnings: Vec<ProfileWarning>,
}

impl PreflightReport {
    /// The live and stopped sessions whose verdict is at risk: every verdict
    /// except [`Verdict::Adoptable`] and [`Verdict::AlreadyNotResumable`].
    pub fn at_risk(&self) -> impl Iterator<Item = &SessionVerdict> {
        self.sessions
            .iter()
            .chain(&self.stopped_sessions)
            .filter(|session| session.verdict.at_risk())
    }

    /// The reasons the release step refuses its plan, which the upgrade must
    /// not override: the plan changes nothing while any remains.
    pub fn plan_refusals(&self) -> impl Iterator<Item = &Refusal> {
        self.runtimes
            .iter()
            .flat_map(|runtimes| runtimes.refusals.iter())
    }

    /// Whether the daemon would refuse to start with the store.
    #[must_use]
    pub fn store_refused(&self) -> bool {
        self.store.state == StoreState::Refused
    }
}
