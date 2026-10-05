//! Read-only adoption preflight for an upgrade.
//!
//! The running daemon of the previous release owns every live session and
//! holds each worker's single controller slot, so this judgement never
//! connects to a worker, takes no instance lock, and never writes, signals or
//! stops anything. Its evidence is the metadata store (migrated in memory), the
//! worker journals on disk and the process table. Each live session gets the
//! verdict startup reconciliation of this build would reach for it, decided by
//! the functions reconciliation itself uses: the store load, the journal
//! reader, the resume-binding merge and the launch-identity check. Evidence
//! that cannot be read counts against the session.
//!
//! Evidence only a worker's socket can give (its answering protocol version,
//! the identity it reports) is judged from what its journal recorded of it.
//! The runtime registry is not built, so a binding that needs it to complete
//! its launch spec is reported as unverified.

// Rust guideline compliant 2026-06-26

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use pohunek_service_config::preflight::{
    PreflightReport, SessionVerdict, StoreReport, StoreState, Verdict, CODE_ADOPTABLE,
    CODE_CREATE_ROLLED_BACK, CODE_EVIDENCE_UNAVAILABLE, CODE_GENERATION_MISSING,
    CODE_IDENTITY_UNVERIFIED, CODE_INTENT_FINISHED, CODE_JOURNAL_AMBIGUOUS, CODE_JOURNAL_MISSING,
    CODE_JOURNAL_SCHEMA_UNSUPPORTED, CODE_JOURNAL_UNREADABLE, CODE_NO_SESSION_RECORD,
    CODE_PROTOCOL_INCOMPATIBLE, CODE_RECORD_UNREADABLE, CODE_RECOVERY_UNMAPPABLE,
    CODE_RECOVERY_UNVERIFIED, CODE_RESUME_RECORD_UNREADABLE, CODE_RUNTIME_IDENTITY_MISMATCH,
    CODE_STORE_UNUSABLE, CODE_WORKER_CHILD_MISSING, CODE_WORKER_ENDED, CODE_WORKER_NOT_ADVANCED,
    CODE_WORKER_NOT_INITIALIZED, CODE_WORKER_NOT_RUNNING, CODE_WORKER_UNVERIFIABLE, REPORT_VERSION,
};
use pohunek_worker_protocol::{negotiate, Version, VersionRange, SUPPORTED_RANGE};

use super::{
    import_worker_identities, import_worker_subagents, initial_input_orphaned,
    merge_persisted_recovery, recovery_not_advanced, runtime_identity_conflict,
    scan_worker_journals, validate_identity_claims, IdentityClaims, JournalEvidence, JournalPhase,
    JournalReject, WorkerJournalScan,
};
use crate::procwatch::ProcessInspector;
use crate::runtime::lifecycle::IDENTITY_MISMATCH;
use crate::session::{DesiredState, Generation, SessionRecord};
use crate::store::{
    may_hold_session, DryRun, DryRunState, RecoveryOutcome, ResumeBinding, Store,
    StoreBodyTooLarge, StoreSchemaError, STORE_SCHEMA_VERSION,
};
use protocol::RuntimeState;

/// The installed state the preflight reads.
#[derive(Debug, Clone)]
pub struct PreflightInputs {
    /// The metadata store file.
    pub store_path: PathBuf,
    /// The durable worker journal root.
    pub worker_state_root: PathBuf,
}

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
#[must_use]
pub fn run(inputs: &PreflightInputs, inspector: &dyn ProcessInspector) -> PreflightReport {
    let journals = scan_worker_journals(&inputs.worker_state_root)
        .map_err(|error| format!("the worker journal root cannot be read: {error}"));
    let dry = Store::new(inputs.store_path.clone()).dry_run_migration();
    let (store, sessions, unmanaged_workers) = match dry {
        Ok(dry) => {
            let (sessions, unmanaged) = judge_sessions(&dry, journals.as_ref(), inspector);
            (store_report(inputs, &dry), sessions, unmanaged)
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
                    })
                    .collect(),
                Err(detail) => vec![evidence_unavailable(detail)],
            };
            (report, sessions, Vec::new())
        }
    };
    PreflightReport {
        report_version: REPORT_VERSION,
        daemon_version: crate::DAEMON_VERSION.to_owned(),
        store,
        sessions,
        unmanaged_workers,
    }
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
        let facts = Facts {
            binding: bindings.get(record.session_id.as_str()).copied(),
            recovery: dry
                .recovery
                .get(&record.session_id)
                .copied()
                .unwrap_or(RecoveryOutcome::Preserved),
            resume_unreadable: anonymous_resume
                || unreadable_resume.contains(record.session_id.as_str()),
        };
        let (verdict, code, detail) = judge(record, &facts, scan, inspector);
        verdicts.insert(
            record.session_id.clone(),
            SessionVerdict {
                session_id: record.session_id.clone(),
                name: record.info.name.clone(),
                verdict,
                code: code.to_owned(),
                detail,
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
                },
            );
        }
    }
    journal_gaps(dry, journals, &known, &mut verdicts);
    (verdicts.into_values().collect(), unmanaged)
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
/// A lost runtime keeps the session's `running` state on disk, so the
/// persisted runtime state decides: the upgrade is not what loses it.
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

type Judgement = (Verdict, &'static str, String);

fn not_adopted(code: &'static str, detail: impl Into<String>) -> Judgement {
    (Verdict::WouldNotBeAdopted, code, detail.into())
}

/// Judges one live session record against the journals of its session.
fn judge(
    record: &SessionRecord,
    facts: &Facts<'_>,
    scan: Result<Option<&WorkerJournalScan>, &str>,
    inspector: &dyn ProcessInspector,
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
        return not_adopted(
            CODE_INTENT_FINISHED,
            "the record carries a stop or removal intent the new daemon finishes instead of \
             adopting the runtime",
        );
    }
    let mut candidate = record.clone();
    if let Some(binding) = facts.binding.cloned() {
        if let Err(reason) = merge_persisted_recovery(&mut candidate, binding) {
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
    if let Some(judgement) = judge_process(journal, inspector) {
        return judgement;
    }
    if let Some(judgement) = judge_protocol(journal) {
        return judgement;
    }
    if let Some(judgement) = judge_identity(journal, &mut candidate, inspector) {
        return judgement;
    }
    judge_recovery(facts, &candidate)
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
) -> Option<Judgement> {
    let snapshot = match journal.snapshot() {
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
    // Adoption completes a missing hook schema from the runtime registry,
    // which the preflight cannot build.
    if snapshot.hook_schema.is_none()
        && (snapshot.active_identity.is_some() || snapshot.active_identity_release.is_some())
    {
        return Some(not_adopted(
            CODE_IDENTITY_UNVERIFIED,
            "the worker journaled an active identity without the hook schema that validates \
             it, which only the runtime registry can supply",
        ));
    }
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

/// Whether the session keeps its native recovery.
fn judge_recovery(facts: &Facts<'_>, candidate: &SessionRecord) -> Judgement {
    if facts.recovery == RecoveryOutcome::Lost {
        return (
            Verdict::WouldLoseRecovery,
            CODE_RECOVERY_UNMAPPABLE,
            "the stored native recovery cannot be mapped to the current launch shape, so the \
             session cannot resume natively"
                .to_owned(),
        );
    }
    // Startup skips an unreadable projection and keeps the record's own
    // recovery, so only a record that depends on the projection loses it.
    if facts.resume_unreadable && candidate.recovery.is_none() {
        return (
            Verdict::WouldLoseRecovery,
            CODE_RESUME_RECORD_UNREADABLE,
            "the stored resume record of the session cannot be loaded, so native recovery is \
             not restored from it"
                .to_owned(),
        );
    }
    if facts.recovery == RecoveryOutcome::NeedsRegistry {
        return (
            Verdict::WouldLoseRecovery,
            CODE_RECOVERY_UNVERIFIED,
            "the stored native recovery needs the runtime registry to complete its launch spec; \
             that cannot be verified before the upgrade"
                .to_owned(),
        );
    }
    (
        Verdict::Adoptable,
        CODE_ADOPTABLE,
        "the worker journal, process, protocol range and stored recovery are consistent".to_owned(),
    )
}
