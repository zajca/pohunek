//! The report `pohunekd upgrade-preflight` prints and `pohunek service
//! check|upgrade` reads.
//!
//! The new daemon binary judges, from the installed state on disk, whether it
//! would adopt every live session the running daemon owns. The report is the
//! one contract between that binary and the installer, so both link these
//! types instead of mirroring them. Reason codes are stable machine strings:
//! the ones the daemon's reconciliation already uses keep their spelling, the
//! rest are the `CODE_*` constants below.

// Rust guideline compliant 2026-06-26

use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Version of the report layout; a reader refuses any other value.
pub const REPORT_VERSION: u32 = 1;

/// The session is adopted with its native recovery intact.
pub const CODE_ADOPTABLE: &str = "adoptable";
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
/// The native recovery binding cannot be mapped to the current launch shape.
pub const CODE_RECOVERY_UNMAPPABLE: &str = "native_recovery_unmappable";
/// The native recovery binding needs the runtime registry to be completed.
pub const CODE_RECOVERY_UNVERIFIED: &str = "native_recovery_unverified";
/// The metadata store is unusable, so the new daemon cannot start.
pub const CODE_STORE_UNUSABLE: &str = "store_unusable";
/// The separately stored resume record of the session is skipped when the
/// store loads.
pub const CODE_RESUME_RECORD_UNREADABLE: &str = "resume_record_unreadable";

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
}

impl Verdict {
    /// Whether the upgrade must be refused unless the operator accepts loss.
    #[must_use]
    pub const fn at_risk(self) -> bool {
        !matches!(self, Self::Adoptable)
    }

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Adoptable => "adoptable",
            Self::WouldLoseRecovery => "would_lose_recovery",
            Self::WouldNotBeAdopted => "would_not_be_adopted",
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
    /// Sessions whose worker journal is live but which the store has no
    /// record of, ordered by id. The running daemon does not manage them
    /// either, so the upgrade leaves them as they are.
    #[serde(default)]
    pub unmanaged_workers: Vec<String>,
}

impl PreflightReport {
    /// The sessions whose verdict is not [`Verdict::Adoptable`].
    pub fn at_risk(&self) -> impl Iterator<Item = &SessionVerdict> {
        self.sessions
            .iter()
            .filter(|session| session.verdict.at_risk())
    }

    /// Whether the daemon would refuse to start with the store.
    #[must_use]
    pub fn store_refused(&self) -> bool {
        self.store.state == StoreState::Refused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_use_stable_snake_case_spellings() {
        for verdict in [
            Verdict::Adoptable,
            Verdict::WouldLoseRecovery,
            Verdict::WouldNotBeAdopted,
        ] {
            let encoded = serde_json::to_string(&verdict).expect("serialize verdict");
            assert_eq!(encoded, format!("\"{}\"", verdict.as_str()));
            assert_eq!(verdict.to_string(), verdict.as_str());
        }
    }

    #[test]
    fn only_an_adoptable_verdict_is_not_at_risk() {
        assert!(!Verdict::Adoptable.at_risk());
        assert!(Verdict::WouldLoseRecovery.at_risk());
        assert!(Verdict::WouldNotBeAdopted.at_risk());
    }

    #[test]
    fn a_report_round_trips_and_lists_only_risky_sessions() {
        let session = |id: &str, verdict| SessionVerdict {
            session_id: id.to_owned(),
            name: None,
            verdict,
            code: CODE_ADOPTABLE.to_owned(),
            detail: String::new(),
        };
        let report = PreflightReport {
            report_version: REPORT_VERSION,
            daemon_version: "1.0.0".to_owned(),
            store: StoreReport {
                path: PathBuf::from("/data/metadata.jsonl"),
                state: StoreState::UpToDate,
                schema_from: Some(2),
                schema_to: 2,
                records: 1,
                error_code: None,
                error: None,
            },
            sessions: vec![
                session("a", Verdict::Adoptable),
                session("b", Verdict::WouldNotBeAdopted),
            ],
            unmanaged_workers: Vec::new(),
        };
        let decoded: PreflightReport =
            serde_json::from_str(&serde_json::to_string(&report).expect("serialize report"))
                .expect("deserialize report");
        assert_eq!(decoded, report);
        let risky: Vec<_> = decoded.at_risk().map(|s| s.session_id.as_str()).collect();
        assert_eq!(risky, ["b"]);
        assert!(!decoded.store_refused());
    }
}
