//! Startup adoption of durable session workers.

use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pohunek_platform::{filesystem::TrustedDir, process::BootIdentity, supervisor::WorkerKey};
use pohunek_worker_protocol::{
    AncestryMatcher, ControlCode, InspectSnapshot, PtyRelation, ReleasedIdentityClaim,
    RuntimePhase, SubagentField,
};
use protocol::{
    AgentActivity, RuntimeInventoryEntry, RuntimeInventoryEvent, RuntimeInventoryStatus,
    RuntimeRef, SessionRuntimeIdentity, SubagentInfo, SubagentLifecycle, SubagentRevision,
    SubagentStateEvent, UnconfirmedProcess,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::conflict_stop::is_conflict_proven_removal;
use super::supervision::{
    classify_unreachable, describe_unreadable_candidates, job_identity_mismatch, observe,
    record_accepted_process, Cleanup, JobEvidence, JournalWorker, Unreachable, WorkerBounds,
    CREATE_COMPENSATION_PENDING, MAX_ACCEPTED_UNCONFIRMED_PROCESSES, RUNTIME_LOST,
    RUNTIME_LOST_CLEANUP_UNCONFIRMED, UNREADABLE_CANDIDATES_RECOVER, UNSUPERVISED_WORKER,
};
use super::{
    current_time_millis, event, event_payload, identity_claim_expiry_is_valid, mpsc,
    preserve_durable_worker_metadata, preserve_newer_updated_at, runtime_error, sort_subagents,
    terminalize_running_subagents, timestamp_now, watch, ActiveAgentReport, CancellationToken,
    DesiredState, DetectorConfig, DetectorConfigUpdate, DetectorInputs, DetectorScope, Mutex,
    Notify, ObservedAgent, ProtocolError, ResumeSnapshot, RuntimeHandle, RuntimeState,
    RuntimeWatchIdentity, SessionEntry, SessionId, SessionRecord, SessionRef, SessionRefKind,
    SessionRegistry, SessionRuntime, SessionState, StateSource, UnconfirmedCleanup, Worker,
    WorkerError, WorkerMetadataApplyOutcome, WORKER_CONNECT_RETRY,
};
use crate::agent::NativeReferenceProvenance;
use crate::procwatch::ProcessInspector;
use crate::runtime::lifecycle::{
    journal_schema_supported, observation_live, require_supported_journal_schema, Lifecycle,
    UnsupportedJournalSchema, IDENTITY_MISMATCH, SUPERVISION_AMBIGUOUS, SUPERVISION_UNAVAILABLE,
};
use crate::session::target::open_detector_output;
use crate::store::{ResumeBinding, SessionWriteOutcome};

// Rust guideline compliant 2026-10-05

pub mod upgrade_preflight;

/// Discovery reason of a worker socket that does not answer.
const UNREACHABLE_SOCKET: &str = "worker_unavailable";

/// Reason of a worker whose id or runtime instance differs from the record's.
pub(super) const RUNTIME_IDENTITY_MISMATCH: &str = "runtime_identity_mismatch";

/// Reason of a recovery that found the worker it was meant to replace.
pub(super) const WORKER_GENERATION_NOT_ADVANCED: &str = "worker_generation_not_advanced";

/// Whether the worker named by `worker_id` and `worker_instance_id` is not the
/// runtime the record binds; ids the record does not carry never conflict.
pub(super) fn runtime_identity_conflict(
    record: &SessionRecord,
    worker_id: &str,
    worker_instance_id: Option<&str>,
) -> bool {
    record
        .runtime
        .worker_id
        .as_deref()
        .is_some_and(|expected| expected != worker_id)
        || record
            .runtime
            .worker_instance_id
            .as_deref()
            .zip(worker_instance_id)
            .is_some_and(|(expected, actual)| expected != actual)
}

/// Whether the record's recovery is still waiting for a worker other than
/// `worker_id`.
pub(super) fn recovery_not_advanced(record: &SessionRecord, worker_id: &str) -> bool {
    record.transaction.as_ref().is_some_and(|transaction| {
        transaction.kind == crate::store::TransactionKind::Recover
            && transaction.previous_worker_id.as_deref() == Some(worker_id)
    })
}

/// Maximum worker journal accepted during daemon reconciliation.
const MAX_WORKER_JOURNAL_BYTES: usize = 1024 * 1024;

/// Inventory reason of a legacy resume binding that startup found with no
/// logical record and no migration manifest to import it from.
pub(crate) const MIGRATION_MANIFEST_MISSING: &str = "migration_manifest_missing";

#[derive(Debug, Clone)]
struct DiscoveredWorker {
    slot: String,
    worker: Worker,
    snapshot: InspectSnapshot,
}

/// Whether classifying an unreachable running worker finished its watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LossClassification {
    /// The runtime was classified, or another operation already replaced it.
    Settled,
    /// Nothing was decided; the watcher keeps reconnecting.
    Pending,
}

/// What the per-session worker socket yielded during reconciliation.
#[derive(Debug)]
enum SocketEvidence {
    /// No socket exists for the session.
    Absent,
    /// Exactly one authenticated worker answered on the session's socket.
    Worker(Box<DiscoveredWorker>),
    /// A socket answered but cannot be adopted (incompatible protocol or a
    /// mismatched identity); its worker is left alive.
    Unusable(RuntimeState, &'static str),
    /// Several live workers claim the session.
    Multiple,
    /// The runtime root could not be enumerated, or the session's socket did
    /// not answer before the connect deadline, so whether a worker still
    /// serves the session is unknown.
    Unknown(String),
}

/// Why a discovered worker socket yielded no inspected worker.
#[derive(Debug)]
enum ProbeFailure {
    /// The connection or the inspection failed with this error.
    Failed(WorkerError),
    /// The socket did not answer before the connect deadline; carries the
    /// detail reported as unknown evidence.
    Unresponsive(String),
}

/// Workers found by startup discovery.
#[derive(Debug, Default)]
struct WorkerDiscovery {
    /// Inspected workers, keyed by the session each claims.
    workers: HashMap<String, Vec<DiscoveredWorker>>,
    /// Inventory of every answering socket.
    inventory: Vec<RuntimeInventoryEntry>,
    /// Runtime slots whose socket did not answer before the connect deadline,
    /// with the detail reported as unknown evidence.
    unresponsive: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct JournalEvidence {
    schema_version: u32,
    session_id: String,
    worker_id: String,
    /// Daemon-issued generation the worker was started as.
    generation: String,
    /// Absolute executable the worker runs.
    executable: PathBuf,
    worker_pid: u32,
    worker_start_identity: String,
    boot_identity: String,
    /// Kernel creation number of the worker, valid within `boot_identity`;
    /// absent for a worker on a host without process lineage and for a worker
    /// of the previous release.
    #[serde(default)]
    worker_spawn_id: Option<String>,
    #[serde(rename = "runtime_id")]
    worker_instance_id: Option<String>,
    child: Option<JournalChild>,
    cols: Option<u16>,
    rows: Option<u16>,
    phase: JournalPhase,
    outcome: Option<JournalOutcome>,
    #[serde(default)]
    subagents: Vec<pohunek_worker_protocol::SubagentSnapshot>,
    /// Hook schema id the worker journaled; absent for a worker that predates
    /// schema delivery.
    #[serde(default)]
    hook_schema: Option<String>,
    /// Lowest private-protocol version the worker serves.
    #[serde(default)]
    protocol_minimum: Option<u16>,
    /// Highest private-protocol version the worker serves.
    #[serde(default)]
    protocol_maximum: Option<u16>,
    /// Immutable launch identity the worker accepted, when it accepted one.
    #[serde(default)]
    launch_identity: Option<JournalLaunchIdentity>,
    /// Latest active identity claim the worker journaled.
    #[serde(default)]
    active_identity: Option<JournalActiveIdentity>,
    /// Latest accepted release of an active identity.
    #[serde(default)]
    active_identity_release: Option<JournalReleasedIdentity>,
    /// Latest native reference the verified launch process reported.
    #[serde(default)]
    native_reference_claim: Option<JournalNativeReference>,
}

/// The latest native reference a worker journaled for its launch process.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct JournalNativeReference {
    provider: String,
    process: JournalProcess,
    sequence: u64,
    reference_kind: String,
    native_reference: String,
}

/// The recovery facts of a worker's accepted launch identity.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct JournalLaunchIdentity {
    provider: String,
    reference_kind: String,
    native_reference: String,
    process: Option<JournalProcess>,
}

impl JournalEvidence {
    /// Returns the worker process facts this journal records.
    pub(super) fn worker(&self) -> JournalWorker<'_> {
        JournalWorker {
            session_id: &self.session_id,
            worker_id: &self.worker_id,
            pid: self.worker_pid,
            start_identity: &self.worker_start_identity,
            boot_identity: &self.boot_identity,
            spawn_id: self.worker_spawn_id.as_deref(),
            worker_instance_id: self.worker_instance_id.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct JournalChild {
    pid: u32,
    /// Platform start identity, as the worker recorded it.
    #[serde(default)]
    start_identity: String,
}

/// A process a journaled identity names.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct JournalProcess {
    pid: u32,
    #[serde(default)]
    start_identity: String,
}

impl JournalProcess {
    /// The process as the worker reports it over its socket; `None` for a
    /// start identity that is not a number.
    fn wire(&self) -> Option<pohunek_worker_protocol::ProcessIdentity> {
        Some(pohunek_worker_protocol::ProcessIdentity {
            pid: self.pid,
            start_identity: self.start_identity.parse().ok()?,
        })
    }
}

/// The live active identity claim a worker journaled.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct JournalActiveIdentity {
    provider: String,
    process: JournalProcess,
    sequence: u64,
    expires_at: String,
    reference_kind: Option<String>,
    native_reference: Option<String>,
}

/// The release of an active identity a worker journaled.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct JournalReleasedIdentity {
    provider: String,
    process: JournalProcess,
    sequence: u64,
}

impl JournalEvidence {
    /// The inspection snapshot this journal stands for, as the live worker
    /// would report it, or the reason it cannot be built.
    ///
    /// The upgrade preflight cannot ask the worker, so it judges the snapshot
    /// its journal implies with the functions adoption applies to the real one.
    pub(super) fn snapshot(&self) -> Result<InspectSnapshot, &'static str> {
        const UNREADABLE: &str = "worker_journal_unreadable";
        let wire = |process: &JournalProcess| process.wire().ok_or(UNREADABLE);
        let child_process = match &self.child {
            Some(child) => Some(wire(&JournalProcess {
                pid: child.pid,
                start_identity: child.start_identity.clone(),
            })?),
            None => None,
        };
        let launch_identity = match &self.launch_identity {
            Some(claim) => Some(pohunek_worker_protocol::ReportedLaunchIdentity {
                provider: claim.provider.clone(),
                process: wire(claim.process.as_ref().ok_or(UNREADABLE)?)?,
                reference_kind: claim.reference_kind.clone(),
                native_reference: claim.native_reference.clone(),
            }),
            None => None,
        };
        // The worker reports an active claim only while it has not expired.
        let active_identity = match &self.active_identity {
            Some(claim) if identity_claim_expiry_is_valid(&claim.expires_at) => {
                Some(pohunek_worker_protocol::ActiveIdentityClaim {
                    provider: claim.provider.clone(),
                    process: wire(&claim.process)?,
                    sequence: claim.sequence,
                    expires_at: claim.expires_at.clone(),
                    reference_kind: claim.reference_kind.clone(),
                    native_reference: claim.native_reference.clone(),
                })
            }
            _ => None,
        };
        let active_identity_release = match &self.active_identity_release {
            Some(release) => Some(ReleasedIdentityClaim {
                provider: release.provider.clone(),
                process: wire(&release.process)?,
                sequence: release.sequence,
            }),
            None => None,
        };
        let native_reference = match &self.native_reference_claim {
            Some(claim) => Some(pohunek_worker_protocol::ReportedNativeReference {
                provider: claim.provider.clone(),
                process: wire(&claim.process)?,
                sequence: claim.sequence,
                reference_kind: claim.reference_kind.clone(),
                native_reference: claim.native_reference.clone(),
            }),
            None => None,
        };
        let phase = match self.phase {
            JournalPhase::Bootstrap => RuntimePhase::Uninitialized,
            JournalPhase::Starting => RuntimePhase::Starting,
            JournalPhase::Live => RuntimePhase::Running,
            JournalPhase::Terminal => RuntimePhase::Exited,
            JournalPhase::NeverInitialized | JournalPhase::Faulted => RuntimePhase::Faulted,
        };
        Ok(InspectSnapshot {
            session_id: pohunek_worker_protocol::SessionId::new(&self.session_id)
                .map_err(|_invalid| UNREADABLE)?,
            worker_id: pohunek_worker_protocol::WorkerId::new(&self.worker_id)
                .map_err(|_invalid| UNREADABLE)?,
            worker_instance_id: self
                .worker_instance_id
                .as_deref()
                .map(pohunek_worker_protocol::WorkerInstanceId::new)
                .transpose()
                .map_err(|_invalid| UNREADABLE)?,
            phase,
            worker_process: wire(&JournalProcess {
                pid: self.worker_pid,
                start_identity: self.worker_start_identity.clone(),
            })?,
            child_process,
            dimensions: None,
            history_start_offset: 0,
            next_offset: 0,
            exit: None,
            launch_identity,
            active_identity,
            active_identity_release,
            native_reference,
            subagents: self.subagents.clone(),
            hook_schema: self.hook_schema.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalPhase {
    Bootstrap,
    Starting,
    Live,
    Terminal,
    NeverInitialized,
    Faulted,
}

#[derive(Debug, Clone, Deserialize)]
struct JournalOutcome {
    exit_code: Option<i32>,
    signal: Option<String>,
    success: bool,
}

#[derive(Debug)]
struct WorkerIdentityProjection {
    active: Option<ActiveAgentReport>,
    release: Option<ReleasedIdentityClaim>,
    privately_reported: bool,
}

impl WorkerIdentityProjection {
    fn unreported() -> Self {
        Self {
            active: None,
            release: None,
            privately_reported: false,
        }
    }
}

impl SessionRegistry {
    /// The hook schema id `snapshot` is validated with: the one the worker
    /// journaled, else the schema of the session's pinned runtime.
    ///
    /// A worker that predates schema delivery journals none, so the daemon
    /// re-projects its claims with the schema its runtime resolves to now.
    fn effective_hook_schema_id(
        &self,
        record: &SessionRecord,
        snapshot: &InspectSnapshot,
    ) -> Option<String> {
        if snapshot.hook_schema.is_some() {
            return snapshot.hook_schema.clone();
        }
        let pin = record
            .recovery
            .as_ref()
            .map(|binding| binding.launch_binding.clone())
            .unwrap_or_default();
        self.session_hook_schema(&record.info.agent_base, &pin)
            .map(|schema| schema.id.to_owned())
    }

    /// The hook schema a snapshot a worker of `record` carries is validated
    /// with: the schema id the journal names must resolve, and a worker that
    /// journaled none is validated with the schema of the record's own
    /// runtime.
    pub(super) fn record_hook_schema(
        &self,
        record: &SessionRecord,
        journaled: Option<&str>,
    ) -> Result<Option<&'static pohunek_worker_protocol::HookSchema>, &'static str> {
        reported_hook_schema(journaled).map(|reported| {
            reported.or_else(|| {
                let pin = record
                    .recovery
                    .as_ref()
                    .map(|binding| binding.launch_binding.clone())
                    .unwrap_or_default();
                self.session_hook_schema(&record.info.agent_base, &pin)
            })
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one locked projection keeps identity and subagent snapshot updates atomic"
    )]
    pub(super) async fn apply_worker_metadata_snapshot(
        &self,
        id: &SessionId,
        snapshot: &InspectSnapshot,
    ) -> WorkerMetadataApplyOutcome {
        #[cfg(test)]
        if snapshot.phase == RuntimePhase::Running
            && self
                .inner
                .worker_imports_blocked
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return WorkerMetadataApplyOutcome::Retryable(super::WorkerMetadataRetryCause::Commit);
        }
        let expected_worker_id = snapshot.worker_id.to_string();
        let expected_worker_instance_id = snapshot
            .worker_instance_id
            .as_ref()
            .map(ToString::to_string);
        let mut memory_base = {
            let sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get(id) else {
                return WorkerMetadataApplyOutcome::Discarded;
            };
            let record = Self::session_record(id, entry, entry.desired_state, None);
            if !worker_metadata_record_is_current(
                &record,
                &expected_worker_id,
                expected_worker_instance_id.as_deref(),
            ) {
                return WorkerMetadataApplyOutcome::Discarded;
            }
            record
        };
        let projected = InspectSnapshot {
            hook_schema: self.effective_hook_schema_id(&memory_base, snapshot),
            ..snapshot.clone()
        };
        let snapshot = &projected;
        let mut outcome = WorkerMetadataApplyOutcome::Applied;
        let mut identities_accepted = true;
        if snapshot.launch_identity.is_some() || snapshot.active_identity.is_some() {
            if let Err(failure) =
                validate_worker_identity_processes(&*self.inner.inspector, snapshot)
            {
                let (reason, retryable) = failure.reason_and_retryability();
                if retryable {
                    tracing::debug!(session_id = %id.0, reason, "worker identity process validation is retryable");
                } else {
                    tracing::warn!(session_id = %id.0, reason, "rejected worker identity process claim");
                }
                identities_accepted = false;
                outcome = if retryable {
                    WorkerMetadataApplyOutcome::Retryable(
                        super::WorkerMetadataRetryCause::IdentityValidation,
                    )
                } else {
                    WorkerMetadataApplyOutcome::Discarded
                };
            }
        }
        let subagents = match import_worker_subagents(snapshot) {
            Ok(subagents) => subagents,
            Err(reason) => {
                tracing::warn!(session_id = %id.0, reason, "rejected worker subagent snapshot");
                return WorkerMetadataApplyOutcome::Discarded;
            }
        };
        let durable_base = match self.load_durable_session_record(id).await {
            Ok(Some(record)) => record,
            Ok(None) if self.inner.store.is_none() => memory_base.clone(),
            Ok(None) => return WorkerMetadataApplyOutcome::Discarded,
            Err(error) => {
                tracing::debug!(session_id = %id.0, error = %error, "worker metadata store load is retryable");
                return WorkerMetadataApplyOutcome::Retryable(
                    super::WorkerMetadataRetryCause::Commit,
                );
            }
        };
        if !worker_metadata_record_is_current(
            &durable_base,
            &expected_worker_id,
            expected_worker_instance_id.as_deref(),
        ) {
            return WorkerMetadataApplyOutcome::Discarded;
        }
        let mut candidate = memory_base.clone();
        preserve_durable_worker_metadata(&durable_base, &mut candidate);
        // The journaled reference is trusted by its generation binding, not by
        // the liveness of the processes the identity claims name.
        import_native_reference(&mut candidate, snapshot);
        let projection = if identities_accepted {
            match import_worker_identities(&mut candidate, snapshot) {
                Ok(projection) => projection,
                Err(reason) => {
                    tracing::warn!(session_id = %id.0, reason, "rejected worker identity snapshot");
                    identities_accepted = false;
                    outcome = WorkerMetadataApplyOutcome::Discarded;
                    WorkerIdentityProjection::unreported()
                }
            }
        } else {
            WorkerIdentityProjection::unreported()
        };
        candidate.info.subagents.clone_from(&subagents);
        if candidate != durable_base {
            candidate.info.updated_at = timestamp_now();
        }
        preserve_newer_updated_at(&memory_base.info.updated_at, &mut candidate.info.updated_at);
        if candidate != durable_base {
            if let Err(error) = self
                .write_session_record_if_current(durable_base, candidate.clone())
                .await
            {
                tracing::debug!(session_id = %id.0, error = %error, "worker metadata store write did not commit");
                return metadata_write_failure_outcome(&error);
            }
        }

        let current_after_store = {
            let sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get(id) else {
                return WorkerMetadataApplyOutcome::Discarded;
            };
            let current = Self::session_record(id, entry, entry.desired_state, None);
            if !worker_metadata_record_is_current(
                &current,
                &expected_worker_id,
                expected_worker_instance_id.as_deref(),
            ) {
                return WorkerMetadataApplyOutcome::Discarded;
            }
            current
        };
        if current_after_store != memory_base {
            let durable_before_rebase = candidate.clone();
            candidate = current_after_store.clone();
            preserve_durable_worker_metadata(&durable_before_rebase, &mut candidate);
            if candidate != durable_before_rebase {
                if let Err(error) = self
                    .write_session_record_if_current(durable_before_rebase, candidate.clone())
                    .await
                {
                    tracing::debug!(session_id = %id.0, error = %error, "rebased worker metadata store write did not commit");
                    return metadata_write_failure_outcome(&error);
                }
            }
            memory_base = current_after_store;
        }

        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                return WorkerMetadataApplyOutcome::Discarded;
            };
            let current = Self::session_record(id, entry, entry.desired_state, None);
            // The narrow projection may rebase once across a benign live-state
            // change, but another concurrent change keeps the snapshot retryable.
            if current != memory_base {
                return WorkerMetadataApplyOutcome::Retryable(
                    super::WorkerMetadataRetryCause::Commit,
                );
            }
            let subagent_events = subagents
                .iter()
                .filter(|subagent| {
                    entry.info.subagents.iter().find(|current| {
                        current.provider == subagent.provider && current.id == subagent.id
                    }) != Some(*subagent)
                })
                .cloned()
                .collect::<Vec<_>>();
            let mut changed = apply_identity_projection(entry, &candidate, &projection, snapshot);
            if entry.info.subagents != subagents {
                entry.info.subagents = subagents;
                changed = true;
            }
            entry
                .last_native_report
                .clone_from(&candidate.native_identity_ordering);
            if changed {
                let mut updated_at = candidate.info.updated_at.clone();
                preserve_newer_updated_at(&entry.info.updated_at, &mut updated_at);
                entry.info.updated_at = updated_at;
            }
            (changed, entry.info.clone(), subagent_events)
        };
        if updated.0 {
            self.persist_resume_binding(id).await;
            self.emit(event::SESSION_UPDATED, &updated.1);
            let runtime = updated.1.runtime.as_ref().and_then(|runtime| {
                runtime
                    .worker_instance_id
                    .as_ref()
                    .and_then(|worker_instance_id| {
                        SessionRuntimeIdentity::new(
                            worker_instance_id.clone(),
                            runtime.runtime_generation,
                        )
                        .ok()
                    })
            });
            for subagent in updated.2 {
                let event = crate::events::event(
                    event::SUBAGENT_STATE,
                    event_payload(SubagentStateEvent {
                        session_id: id.clone(),
                        subagent,
                        runtime: runtime.clone(),
                    }),
                );
                let _ = self.inner.events.send(event);
            }
        } else if identities_accepted
            && (snapshot.launch_identity.is_some() || snapshot.active_identity.is_some())
        {
            // A concurrent applier may have committed the same identity to
            // memory and still be writing its resume binding. Persisting here
            // queues behind that write on the persist lock, so an applied
            // snapshot always leaves the binding matching the record.
            self.persist_resume_binding(id).await;
        }
        if identities_accepted {
            WorkerMetadataApplyOutcome::Applied
        } else {
            outcome
        }
    }

    /// Loads logical records and adopts their exact surviving workers.
    ///
    /// Evidence is the native supervisor's jobs, the worker sockets, and the
    /// worker journals, joined on each record's current generation; every
    /// record is classified under its session's lifecycle lock. Ended jobs of
    /// stale generations are retired, and sessions whose supervisor cannot be
    /// inspected are retried in the background.
    ///
    /// This never invokes provider-native resume. An absent, conflicting, or
    /// incompatible runtime remains visible with an explicit runtime state.
    ///
    /// # Errors
    ///
    /// Returns a store error when logical records cannot be loaded. Individual
    /// worker failures are classified per session and do not abort startup.
    pub async fn reconcile_workers(&self) -> Result<(), ProtocolError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(());
        };
        let (manifest, records, resume_bindings) = load_durable_state(store).await?;
        let unmigrated = if manifest == LegacyManifest::Missing && records.is_empty() {
            unmigrated_legacy_bindings(&resume_bindings)
        } else {
            Vec::new()
        };
        self.inner
            .legacy_migration_pending
            .store(!unmigrated.is_empty(), std::sync::atomic::Ordering::Release);
        let mut resume_bindings = resume_bindings
            .into_iter()
            .map(|binding| (binding.session_id.clone(), binding))
            .collect::<HashMap<_, _>>();

        let lifecycle = self.lifecycle().ok();
        let (mut discovered, socket_failure) = match self.discover_workers(&records).await {
            Ok(discovered) => (discovered, None),
            Err(detail) => (WorkerDiscovery::default(), Some(detail)),
        };
        let mut inventory = std::mem::take(&mut discovered.inventory);
        inventory.extend(unmigrated);
        let mut journals = self.discover_worker_journals().await;
        let stale_journal = |key: &WorkerKey| match &journals {
            Ok(journals) => journals.get(key.session_id()).map_or(Ok(None), |scan| {
                scan.journal_of_generation(key.generation())
                    .map(|journal| journal.map(JournalEvidence::worker))
            }),
            Err(detail) => Err(detail.clone()),
        };
        inventory.extend(
            self.retire_stale_jobs(lifecycle.as_ref(), &records, stale_journal)
                .await,
        );
        for entry in inventory
            .iter()
            .filter(|entry| entry.status != RuntimeInventoryStatus::Managed)
        {
            let event = crate::events::event(
                event::SESSION_RUNTIME_DISCOVERED,
                event_payload(RuntimeInventoryEvent {
                    entry: entry.clone(),
                }),
            );
            let _ = self.inner.events.send(event);
        }
        *self.inner.runtime_inventory.lock().await = inventory;

        for mut record in records {
            let id = SessionId(record.session_id.clone());
            let _guard = self.lock_lifecycle(&id).await;
            let binding = resume_bindings.remove(&record.session_id);
            if let Err(reason) = self.merge_resume_binding(&mut record, binding) {
                if !self
                    .insert_unavailable_record(record, RuntimeState::Conflict, reason)
                    .await
                {
                    self.schedule_supervision_retry(&id);
                }
                continue;
            }
            let socket = self
                .startup_socket_evidence(
                    &record.session_id,
                    socket_failure.as_deref(),
                    &mut discovered,
                )
                .await;
            let scan = match &mut journals {
                Ok(journals) => Ok(journals.remove(&record.session_id).unwrap_or_default()),
                Err(detail) => Err(detail.clone()),
            };
            if Box::pin(self.reconcile_logical(
                record,
                socket,
                scan.as_ref().map_err(String::as_str),
                lifecycle.as_ref(),
                false,
            ))
            .await
            {
                self.schedule_supervision_retry(&id);
            }
        }
        Ok(())
    }

    /// Classifies the socket evidence startup discovery found for a session,
    /// taking its candidates out of `discovered`; `failure` is why the
    /// runtime root could not be enumerated at all.
    async fn startup_socket_evidence(
        &self,
        session_id: &str,
        failure: Option<&str>,
        discovered: &mut WorkerDiscovery,
    ) -> SocketEvidence {
        let unresponsive = discovered.unresponsive.get(session_id);
        if let Some(detail) = failure.or(unresponsive.map(String::as_str)) {
            return SocketEvidence::Unknown(detail.to_owned());
        }
        let mut candidates = discovered.workers.remove(session_id).unwrap_or_default();
        match candidates.as_slice() {
            [candidate] if candidate.slot == session_id => {
                SocketEvidence::Worker(Box::new(candidates.remove(0)))
            }
            [] => match self.inventory_state_for_slot(session_id).await {
                Some((state, reason)) => SocketEvidence::Unusable(state, reason),
                None => SocketEvidence::Absent,
            },
            _ => SocketEvidence::Multiple,
        }
    }

    /// Repairs the native recovery of `record` and merges its persisted
    /// resume `binding`.
    ///
    /// Returns the reason when the binding contradicts the record, which
    /// quarantines adoption. A record that carries a stop or removal intent
    /// adopts nothing, so a contradiction does not stop it from being finished
    /// from the record and the worker evidence alone.
    fn merge_resume_binding(
        &self,
        record: &mut SessionRecord,
        mut binding: Option<ResumeBinding>,
    ) -> Result<(), &'static str> {
        self.repair_native_recovery(record, binding.as_mut());
        let Some(binding) = binding else {
            return Ok(());
        };
        let merged = merge_persisted_recovery(record, binding);
        if record.desired_state == DesiredState::Running {
            merged
        } else {
            Ok(())
        }
    }

    /// Reads the persisted resume binding of `session_id`.
    async fn load_resume_binding(
        &self,
        session_id: &str,
    ) -> Result<Option<ResumeBinding>, ProtocolError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(None);
        };
        let wanted = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            store.load_resume().map(|bindings| {
                bindings
                    .into_iter()
                    .rev()
                    .find(|binding| binding.session_id == wanted)
            })
        })
        .await
        .map_err(|_join_error| {
            runtime_error(
                "session_store_failed",
                format!("resume binding read task panicked for {session_id}"),
            )
        })?
        .map_err(|error| {
            runtime_error(
                "session_store_failed",
                format!("failed to read the resume binding of {session_id}: {error}"),
            )
        })
    }

    /// Re-reconciles one durable record from fresh evidence.
    ///
    /// Used by the supervision retry, with the session's lifecycle lock held.
    /// Returns whether the supervisor is still unavailable; the in-memory entry
    /// is left untouched in that case.
    pub(super) async fn reconcile_single_session(&self, mut record: SessionRecord) -> bool {
        // The persisted binding is judged again on every pass, so a
        // quarantine whose classification could not be written stays in force.
        let binding = match self.load_resume_binding(&record.session_id).await {
            Ok(binding) => binding,
            Err(error) => {
                tracing::warn!(session_id = %record.session_id, error = %error, "the resume binding cannot be read; the session stays pending");
                return true;
            }
        };
        if let Err(reason) = self.merge_resume_binding(&mut record, binding) {
            return !self
                .insert_unavailable_record(record, RuntimeState::Conflict, reason)
                .await;
        }
        let lifecycle = self.lifecycle().ok();
        let socket = self.session_socket_evidence(&record).await;
        let scan = self
            .discover_worker_journals()
            .await
            .map(|mut journals| journals.remove(&record.session_id).unwrap_or_default());
        Box::pin(self.reconcile_logical(
            record,
            socket,
            scan.as_ref().map_err(String::as_str),
            lifecycle.as_ref(),
            true,
        ))
        .await
    }

    /// Collects the socket evidence of one session from every worker socket.
    ///
    /// The session's own slot decides what answers there; every other slot is
    /// probed too, because a worker anywhere under the runtime root can claim
    /// the session. Any such claim is [`SocketEvidence::Multiple`] whatever
    /// the own slot shows, exactly as startup discovery treats duplicate
    /// claims. A slot held by the controller of another session is not
    /// probed: its claim is that session's.
    ///
    /// Every connection and inspection is bounded by the connect deadline; a
    /// session slot that does not answer in time is unknown evidence, so
    /// nothing is touched and the session stays pending.
    async fn session_socket_evidence(&self, record: &SessionRecord) -> SocketEvidence {
        let Some(runtime_root) = self.inner.config.worker_runtime_root.clone() else {
            return SocketEvidence::Absent;
        };
        let slots = match tokio::task::spawn_blocking(move || discover_runtime_slots(&runtime_root))
            .await
        {
            Ok(Ok(slots)) => slots,
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "failed to enumerate durable worker runtime root");
                return SocketEvidence::Unknown(format!(
                    "worker runtime root cannot be enumerated: {error}"
                ));
            }
            Err(error) => {
                tracing::warn!(error = %error, "durable worker discovery task panicked");
                return SocketEvidence::Unknown(format!(
                    "worker runtime root discovery failed: {error}"
                ));
            }
        };
        let held = self.controlled_worker_sockets().await;
        let mut own = SocketEvidence::Absent;
        for (slot, socket) in slots {
            if slot == record.session_id {
                own = self.own_slot_evidence(record, slot, &socket).await;
                continue;
            }
            match held.iter().find(|(path, _)| *path == socket) {
                Some((_, owner)) if *owner != record.session_id => continue,
                Some(_) => return SocketEvidence::Multiple,
                None => {}
            }
            // A slot that fails or stays silent proves no claim, as in
            // startup discovery, where it is inventoried or ignored.
            if let Ok((_worker, snapshot)) = self.connect_and_inspect(&socket).await {
                if snapshot.session_id.as_str() == record.session_id {
                    return SocketEvidence::Multiple;
                }
            }
        }
        own
    }

    /// Worker sockets some registered session's controller holds, with the
    /// session each belongs to.
    async fn controlled_worker_sockets(&self) -> Vec<(PathBuf, String)> {
        self.inner
            .sessions
            .lock()
            .await
            .iter()
            .filter_map(|(id, entry)| match &entry.runtime {
                super::RuntimeHandle::Worker(worker) => {
                    Some((worker.socket_path().to_path_buf(), id.0.clone()))
                }
                super::RuntimeHandle::Unavailable(_) => None,
            })
            .collect()
    }

    /// Connects the worker socket of the session's own slot and classifies
    /// what answers there.
    async fn own_slot_evidence(
        &self,
        record: &SessionRecord,
        slot: String,
        socket: &Path,
    ) -> SocketEvidence {
        match self.connect_and_inspect(socket).await {
            Ok((worker, snapshot)) => {
                if snapshot.session_id.as_str() != slot
                    || persisted_identity_mismatch(record, &snapshot)
                {
                    SocketEvidence::Unusable(RuntimeState::Conflict, IDENTITY_MISMATCH)
                } else {
                    SocketEvidence::Worker(Box::new(DiscoveredWorker {
                        slot,
                        worker,
                        snapshot,
                    }))
                }
            }
            Err(ProbeFailure::Unresponsive(detail)) => SocketEvidence::Unknown(detail),
            Err(ProbeFailure::Failed(error)) => {
                // Same classification startup discovery applies to its inventory.
                match classify_connect_error(&error) {
                    (RuntimeState::Incompatible, reason) => {
                        SocketEvidence::Unusable(RuntimeState::Incompatible, reason)
                    }
                    (RuntimeState::Lost, _) => SocketEvidence::Absent,
                    _ => SocketEvidence::Unusable(RuntimeState::Conflict, IDENTITY_MISMATCH),
                }
            }
        }
    }

    /// Classifies one durable record from its socket, journal, and job evidence.
    ///
    /// The record's current generation binds the evidence: a socket counts only
    /// when its worker's journal names that generation, terminal journals of
    /// other generations are history, and only that generation's job is
    /// inspected. Returns whether the session needs the background re-check:
    /// its supervisor was unavailable, its generation is ambiguous, a worker
    /// that answers or a job the supervisor shows keeps it in conflict, the
    /// job of its proven-ended generation could not be retired, or its
    /// durable removal intent could not be finished. In `retry` mode an
    /// unchanged outcome leaves the current entry untouched.
    ///
    /// A removal intent is finished, not imported, once the generation is
    /// proven ended with no worker answering (an exact terminal journal or an
    /// ended job) or its answering worker confirms the stop. A stop intent
    /// (or committed stop) over a generation whose job ended and whose worker
    /// is gone ends `stopped` ([`Self::settle_ended_stop`]), never `lost`.
    ///
    /// `scan` is `Err` when the journals could not be scanned. Missing socket
    /// or journal evidence decides nothing: the session is ambiguous and
    /// re-checked, because only a successful scan without a journal of the
    /// generation proves that its worker never journaled.
    #[expect(
        clippy::too_many_lines,
        reason = "the classification table stays in one place so every row is visible together"
    )]
    async fn reconcile_logical(
        &self,
        record: SessionRecord,
        socket: SocketEvidence,
        scan: Result<&WorkerJournalScan, &str>,
        lifecycle: Option<&Lifecycle<'_>>,
        retry: bool,
    ) -> bool {
        let mut record = match self.roll_back_undelivered_create(record).await {
            UndeliveredCreate::Proceed(record) => record,
            UndeliveredCreate::Pending(record) => {
                self.hold_pending_unpersisted(record, retry).await;
                return true;
            }
            UndeliveredCreate::Gone(id) => {
                let _evicted = self.remove_session_entry(&id).await;
                return false;
            }
        };
        let generation = match super::Generation::from_record(&record.session_id, &record.runtime) {
            Ok(generation) => generation,
            Err(error) => {
                tracing::debug!(session_id = %record.session_id, error = %error, "record names a malformed worker generation");
                return !self
                    .insert_unavailable_record(record, RuntimeState::Conflict, IDENTITY_MISMATCH)
                    .await;
            }
        };
        let complete = match (&socket, scan) {
            (SocketEvidence::Unknown(detail), _) => Err(detail.as_str()),
            (_, scan) => scan,
        };
        let scan = match complete {
            Ok(scan) => scan,
            Err(detail) => {
                tracing::debug!(session_id = %record.session_id, detail, "worker evidence is incomplete; nothing is touched");
                self.mark_ambiguous(record, retry).await;
                return true;
            }
        };
        match socket {
            SocketEvidence::Multiple => {
                self.mark_pending(
                    record,
                    RuntimeState::Conflict,
                    "multiple_worker_candidates",
                    retry,
                )
                .await;
                return true;
            }
            SocketEvidence::Worker(candidate) => {
                let bound = generation.as_ref().and_then(|generation| {
                    scan.evidence
                        .iter()
                        .find(|journal| journal.worker_id == candidate.snapshot.worker_id.as_str())
                        .filter(|journal| journal.generation == generation.generation())
                        .map(|journal| (generation, journal))
                });
                let Some((generation, journal)) = bound else {
                    tracing::debug!(
                        session_id = %record.session_id,
                        worker_id = %candidate.snapshot.worker_id,
                        "live worker is not bound to the record's generation"
                    );
                    self.mark_pending(record, RuntimeState::Conflict, IDENTITY_MISMATCH, retry)
                        .await;
                    return true;
                };
                match observe(lifecycle, generation).await {
                    JobEvidence::Present(observation) => {
                        if let Some(detail) =
                            job_identity_mismatch(&observation, generation, Some(&journal.worker()))
                        {
                            tracing::debug!(session_id = %record.session_id, detail, "live worker's job does not match its generation");
                            self.mark_pending(
                                record,
                                RuntimeState::Conflict,
                                IDENTITY_MISMATCH,
                                retry,
                            )
                            .await;
                            return true;
                        }
                    }
                    // The authenticated worker proves its generation through
                    // its journal; a job the supervisor cannot show does not
                    // make its live PTY any less adoptable.
                    JobEvidence::Absent => {
                        self.note_unsupervised_worker(
                            &record.session_id,
                            &candidate.snapshot,
                            &generation.service_id(),
                        )
                        .await;
                    }
                    JobEvidence::Unavailable(detail) => {
                        tracing::warn!(session_id = %record.session_id, detail, "adopting a live worker without supervisor evidence");
                    }
                }
                return Box::pin(self.reconcile_record(
                    record,
                    Some((candidate.worker, candidate.snapshot)),
                    retry,
                ))
                .await;
            }
            SocketEvidence::Absent | SocketEvidence::Unusable(..) | SocketEvidence::Unknown(_) => {}
        }

        let creating = uncommitted_create(&record);
        let scoped = scan.scoped_to(generation.as_ref());
        match classify_terminal_journals(&scoped, &record) {
            // The journal proves the generation ended and no worker answers,
            // so the removal intent is finished; the finalizer still proves
            // every worker journaled for the generation gone.
            TerminalJournalClassification::Exact(_)
                if record.desired_state == DesiredState::Removed
                    && generation.is_some()
                    && matches!(socket, SocketEvidence::Absent) =>
            {
                return Box::pin(self.finish_removal_intent(
                    record,
                    generation.as_ref(),
                    lifecycle,
                    retry,
                ))
                .await;
            }
            // The create never committed and its runtime ended, so it is
            // compensated instead of imported as a terminal session.
            TerminalJournalClassification::Exact(_)
                if creating && matches!(socket, SocketEvidence::Absent) =>
            {
                return self
                    .settle_ended_create(record, generation.as_ref(), lifecycle, retry)
                    .await;
            }
            TerminalJournalClassification::Exact(evidence) => {
                let session_id = record.session_id.clone();
                // A pending stop ends only with confirmed cleanup, whichever
                // way the worker's end is proven.
                if is_pending_stop(&record)
                    && matches!(socket, SocketEvidence::Absent)
                    && !self
                        .stop_cleanup_confirmed(&record.session_id, generation.as_ref())
                        .await
                {
                    self.mark_pending(record, RuntimeState::Conflict, SUPERVISION_AMBIGUOUS, retry)
                        .await;
                    return true;
                }
                if !self.import_terminal_journal(record, *evidence).await {
                    return true;
                }
                // The imported outcome is recorded first; the job of the
                // proven-terminal generation otherwise stays registered (a
                // launchd `RunAtLoad` job stays loaded after its worker exits).
                // A worker that still answers keeps its final output until it
                // exits, and a later reconciliation retires the job then.
                if !matches!(socket, SocketEvidence::Absent) {
                    return false;
                }
                return !self
                    .retire_terminal_generation(&session_id, generation.as_ref(), lifecycle)
                    .await;
            }
            TerminalJournalClassification::Conflict => {
                return !self
                    .insert_unavailable_record(
                        record,
                        RuntimeState::Conflict,
                        "worker_journal_identity_mismatch",
                    )
                    .await;
            }
            TerminalJournalClassification::Absent => {}
        }
        if let SocketEvidence::Unusable(state, reason) = socket {
            // A conflicting worker still answers there, so its end is watched.
            let watched = state == RuntimeState::Conflict;
            self.mark_pending(record, state, reason, retry).await;
            return watched;
        }
        let id = SessionId(record.session_id.clone());
        let Some(generation) = generation else {
            if creating {
                self.compensate_abandoned_create(&id).await;
                return false;
            }
            return !self
                .insert_unavailable_record(record, RuntimeState::Lost, "worker_unavailable")
                .await;
        };

        let live_journals = scoped
            .evidence
            .iter()
            .filter(|journal| journal.phase != JournalPhase::Terminal)
            .collect::<Vec<_>>();
        let journal = match live_journals.as_slice() {
            [] => None,
            [journal] => Some(*journal),
            [_, _, ..] => {
                tracing::debug!(session_id = %id.0, "several journals claim the record's generation");
                self.mark_ambiguous(record, retry).await;
                return true;
            }
        };
        if let (Some(journal), Some(expected)) = (journal, record.runtime.worker_id.as_deref()) {
            if journal.worker_id != expected {
                return !self
                    .insert_unavailable_record(record, RuntimeState::Conflict, IDENTITY_MISMATCH)
                    .await;
            }
        }
        let job = observe(lifecycle, &generation).await;
        if creating && scoped.evidence.is_empty() {
            if let JobEvidence::Present(observation) = &job {
                if observation_live(observation)
                    && job_identity_mismatch(observation, &generation, None).is_none()
                {
                    // No worker of this generation ever journaled, so the
                    // create that registered it died with its daemon.
                    tracing::info!(session_id = %id.0, service_id = %generation.service_id(), "settling the live job of an abandoned create");
                    self.mark_ambiguous(record, retry).await;
                    self.settle_abandoned_create(id, generation);
                    return false;
                }
            }
        }
        // A worker that cannot be reached may still have journaled a switch the
        // daemon never imported; its own generation's journal is the evidence.
        // A previous-release worker keeps that switch only as its active claim,
        // which the terminal import applies when no reference is journaled.
        if let Some(snapshot) = journal.and_then(|journal| journal.snapshot().ok()) {
            let schema = self.record_hook_schema(&record, snapshot.hook_schema.as_deref());
            import_terminal_native_reference(&mut record, &snapshot, schema);
        }
        let worker = journal.map(JournalEvidence::worker);
        match classify_unreachable(
            &job,
            &generation,
            worker.as_ref(),
            self.inner.inspector.as_ref(),
        ) {
            Unreachable::Unavailable(detail) => {
                tracing::debug!(session_id = %id.0, detail, "worker supervisor is unavailable; nothing is touched");
                if !retry || self.classification_unpersisted(&record.session_id) {
                    // An uncommitted classification leaves the session pending
                    // all the same.
                    let _persisted = self
                        .insert_unavailable_record(
                            record,
                            RuntimeState::Reconnecting,
                            SUPERVISION_UNAVAILABLE,
                        )
                        .await;
                }
                true
            }
            Unreachable::Ambiguous(detail) => {
                tracing::debug!(session_id = %id.0, detail, "worker generation is ambiguous; nothing is touched");
                self.mark_ambiguous(record, retry).await;
                true
            }
            Unreachable::Mismatch(detail) => {
                tracing::debug!(session_id = %id.0, detail, "worker job identity mismatch; failing closed");
                self.mark_pending(record, RuntimeState::Conflict, IDENTITY_MISMATCH, retry)
                    .await;
                true
            }
            Unreachable::Ended {
                journaled,
                worker_instance_id,
                bounds,
            } => {
                // The removal finalizer sweeps every runtime of the
                // generation itself and finishes only on a confirmed sweep.
                if !creating && record.desired_state == DesiredState::Removed {
                    return Box::pin(self.finish_removal_intent(
                        record,
                        Some(&generation),
                        lifecycle,
                        retry,
                    ))
                    .await;
                }
                // `worker_instance_id` is set only for a journaled worker proven gone;
                // a generation that never journaled has nothing proven to
                // sweep, so only its job is retired.
                let cleanup = match worker_instance_id.as_deref() {
                    Some(worker_instance_id) => {
                        self.sweep_lost_runtime(&id.0, worker_instance_id, bounds)
                            .await
                    }
                    None => Cleanup::Complete,
                };
                let retired = match lifecycle {
                    Some(lifecycle) => lifecycle.retire(&generation).await,
                    None => Ok(()),
                };
                if let Err(error) = &retired {
                    tracing::warn!(
                        session_id = %id.0,
                        service_id = %generation.service_id(),
                        error = %error,
                        "failed to retire the ended worker generation"
                    );
                }
                if creating {
                    // The abandoned create is compensated only once its job
                    // is gone, so the deleted record never hides a job.
                    if retired.is_ok() {
                        self.compensate_abandoned_create(&id).await;
                        return false;
                    }
                    if !retry || self.classification_unpersisted(&record.session_id) {
                        // An uncommitted classification leaves the session
                        // pending all the same.
                        let _persisted = self
                            .insert_unavailable_record(
                                record,
                                RuntimeState::Reconnecting,
                                SUPERVISION_UNAVAILABLE,
                            )
                            .await;
                    }
                    return true;
                }
                // `Lost` counts as ended, so a later removal skips retirement:
                // it is published only once the job is gone, and a failed
                // retirement is re-checked until it succeeds.
                if retired.is_err() {
                    self.mark_pending(
                        record,
                        RuntimeState::Reconnecting,
                        SUPERVISION_UNAVAILABLE,
                        retry,
                    )
                    .await;
                    return true;
                }
                // A stop the user asked for ended the runtime on purpose:
                // the outcome is a stop, never a loss.
                if record.desired_state == DesiredState::Stopped {
                    return Box::pin(self.settle_ended_stop(record, Some(&generation), retry))
                        .await;
                }
                let reason = match (journaled, cleanup) {
                    (false, _) => "worker_unavailable",
                    (true, Cleanup::Complete) => RUNTIME_LOST,
                    (true, Cleanup::Unconfirmed) => RUNTIME_LOST_CLEANUP_UNCONFIRMED,
                };
                !self
                    .insert_unavailable_record(record, RuntimeState::Lost, reason)
                    .await
            }
        }
    }

    /// Whether the marked processes of every runtime of `generation` are
    /// proven gone, the cleanup a stop requires before it can end.
    ///
    /// The sweep covers only runtimes the journals of the generation prove
    /// and never accepts unreadable-marker processes. Without a generation
    /// there is nothing to sweep.
    async fn stop_cleanup_confirmed(
        &self,
        session_id: &str,
        generation: Option<&super::Generation>,
    ) -> bool {
        if generation.is_none() {
            return true;
        }
        match self
            .sweep_removed_runtimes(
                &SessionId(session_id.to_owned()),
                generation,
                None,
                UnconfirmedCleanup::Refuse,
            )
            .await
        {
            Ok(_accepted) => true,
            Err(error) => {
                tracing::warn!(session_id, error = %error, "the stop waits for its cleanup to be confirmed");
                false
            }
        }
    }

    /// Settles the stop of a runtime proven ended: its worker gone and its
    /// job retired by the caller.
    ///
    /// A runtime ended through its supervisor (the stop of a conflicted
    /// runtime retires the job) leaves a live-phase journal, so no terminal
    /// journal can prove the stop. The record's own stop evidence does: a
    /// stop intent that was not committed yet is finished as `stopped`, and a
    /// stop that was committed is kept as it is. Cleanup that is not confirmed
    /// ([`Self::stop_cleanup_confirmed`]) decides nothing; the session stays
    /// pending and the re-check sweeps again. Returns whether the session
    /// needs that re-check.
    ///
    /// The caller imported the generation journal's native reference into
    /// `record` before classifying the unreachable worker, so the stop keeps
    /// a switch the dead worker journaled.
    async fn settle_ended_stop(
        &self,
        mut record: SessionRecord,
        generation: Option<&super::Generation>,
        retry: bool,
    ) -> bool {
        if !self
            .stop_cleanup_confirmed(&record.session_id, generation)
            .await
        {
            self.mark_pending(record, RuntimeState::Conflict, SUPERVISION_AMBIGUOUS, retry)
                .await;
            return true;
        }
        let id = SessionId(record.session_id.clone());
        record.transaction = None;
        record.info.state = SessionState::Stopped;
        record.info.state_source = StateSource::Process;
        record.info.activity = None;
        record.info.active_agent = None;
        record.info.active_agent_base = None;
        record.info.active_agent_pid = None;
        record.info.active_agent_session_id = None;
        record.info.active_agent_session_path = None;
        if !self
            .insert_unavailable_record(record, RuntimeState::Terminal, "")
            .await
        {
            return true;
        }
        self.persist_resume_binding(&id).await;
        false
    }

    /// Turns a committed create whose initial input was never delivered into
    /// a removal intent, which the rest of reconciliation finishes exactly as
    /// an interrupted `session rm`.
    ///
    /// Only a marker another daemon instance wrote (or one naming no
    /// instance) is orphaned: that instance held the input in memory and is
    /// gone, so the session cannot get it and must not keep running without
    /// it. A marker of this instance belongs to a create still delivering
    /// its input, which reconciliation treats as the running session it is.
    ///
    /// The conversion starts from the current durable record and is persisted
    /// conditionally on it; only an applied intent proceeds to the removal.
    /// A record that changed meanwhile, or a write that failed, stops nothing
    /// and cleans nothing: the session stays pending for the retry, which
    /// re-reads it.
    async fn roll_back_undelivered_create(&self, record: SessionRecord) -> UndeliveredCreate {
        if !self.orphaned_initial_input(&record) {
            return UndeliveredCreate::Proceed(record);
        }
        let id = SessionId(record.session_id.clone());
        let durable = match self.load_durable_session_record(&id).await {
            Ok(Some(durable)) => durable,
            Ok(None) => return UndeliveredCreate::Gone(id),
            Err(error) => {
                tracing::warn!(session_id = %id.0, error = %error, "cannot read the record of an undelivered create; it stays pending");
                return UndeliveredCreate::Pending(record);
            }
        };
        if !self.orphaned_initial_input(&durable) {
            // Settled by another writer since the evidence was read.
            return UndeliveredCreate::Pending(durable);
        }
        #[cfg(test)]
        self.hold_undelivered_conversion(&id).await;
        tracing::warn!(session_id = %id.0, "rolling back a create whose initial input was never delivered");
        let mut intent = durable.clone();
        intent.desired_state = DesiredState::Removed;
        intent.transaction = Some(crate::store::SessionTransaction {
            id: format!("remove-create-{}", id.0),
            kind: crate::store::TransactionKind::Remove,
            phase: "requested".to_owned(),
            previous_worker_id: None,
            previous_worker_instance_id: None,
            daemon_instance_id: None,
        });
        match self
            .write_session_record_if_current(durable.clone(), intent.clone())
            .await
        {
            Ok(()) => UndeliveredCreate::Proceed(intent),
            Err(error) => {
                tracing::warn!(
                    session_id = %id.0,
                    error = %error,
                    "the removal intent of an undelivered create was not persisted; nothing is stopped"
                );
                UndeliveredCreate::Pending(durable)
            }
        }
    }

    /// Whether `record` is a running create whose `initial_input` marker was
    /// written by another daemon instance, or names none.
    fn orphaned_initial_input(&self, record: &SessionRecord) -> bool {
        initial_input_orphaned(record, self.daemon_instance_id())
    }

    /// Parks an undelivered-create conversion before its conditional write
    /// when a test armed the hold.
    #[cfg(test)]
    async fn hold_undelivered_conversion(&self, id: &SessionId) {
        let gate = self
            .inner
            .undelivered_conversion_hold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_if(|gate| gate.session_id.as_deref().is_none_or(|held| held == id.0));
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
    }

    /// Shows `record` as `Conflict` with `runtime_supervision_ambiguous`
    /// without persisting it, and leaves it for the supervision retry.
    ///
    /// Used where the durable record is newer than this snapshot or could
    /// not be written, so the snapshot must not overwrite it.
    async fn hold_pending_unpersisted(&self, record: SessionRecord, retry: bool) {
        let id = SessionId(record.session_id.clone());
        if retry
            && self
                .inner
                .sessions
                .lock()
                .await
                .get(&id)
                .is_some_and(|entry| {
                    entry.info.runtime.as_ref().is_some_and(|runtime| {
                        runtime.state == RuntimeState::Conflict
                            && runtime.loss_reason.as_deref() == Some(SUPERVISION_AMBIGUOUS)
                    })
                })
        {
            return;
        }
        let (_record, entry) =
            self.unavailable_entry(record, RuntimeState::Conflict, SUPERVISION_AMBIGUOUS);
        let info = entry.info.clone();
        self.install_session_entry(&id, entry).await;
        warn_unavailable(
            &id.0,
            info_worker_id(&info),
            RuntimeState::Conflict,
            SUPERVISION_AMBIGUOUS,
            "",
        );
        self.emit(event::SESSION_RUNTIME_CONFLICT, &info);
    }

    /// Shows `record` as `Conflict` with `runtime_supervision_ambiguous`.
    ///
    /// A retry that finds the same ambiguity leaves the entry, and its
    /// subscribers, untouched.
    async fn mark_ambiguous(&self, record: SessionRecord, retry: bool) {
        self.mark_pending(record, RuntimeState::Conflict, SUPERVISION_AMBIGUOUS, retry)
            .await;
    }

    /// Shows `record` as `state` with `reason` while it waits for the
    /// background re-check.
    ///
    /// A retry that finds the entry already showing the same state and
    /// reason leaves it, and its subscribers, untouched. Callers keep the
    /// session pending, so an uncommitted classification is re-checked.
    async fn mark_pending(
        &self,
        record: SessionRecord,
        state: RuntimeState,
        reason: &str,
        retry: bool,
    ) {
        let id = SessionId(record.session_id.clone());
        if retry && !self.classification_unpersisted(&id.0) {
            let unchanged = self
                .inner
                .sessions
                .lock()
                .await
                .get(&id)
                .is_some_and(|entry| {
                    entry.info.runtime.as_ref().is_some_and(|runtime| {
                        runtime.state == state && runtime.loss_reason.as_deref() == Some(reason)
                    })
                });
            if unchanged {
                return;
            }
        }
        let _persisted = self.insert_unavailable_record(record, state, reason).await;
    }

    /// Records that a live worker was adopted while its job is proven absent.
    ///
    /// The native manager has lost track of a worker that still owns a PTY,
    /// so the anomaly is logged as a structured warning and noted on the
    /// session's inventory entry, which is re-announced to subscribers.
    async fn note_unsupervised_worker(
        &self,
        session_id: &str,
        snapshot: &InspectSnapshot,
        service_id: &pohunek_platform::supervisor::ServiceId,
    ) {
        // A re-check that meets the same unsupervised worker again reports
        // nothing new.
        let noted = self
            .inner
            .runtime_inventory
            .lock()
            .await
            .iter()
            .any(|entry| {
                entry.runtime_slot == session_id
                    && entry.status == RuntimeInventoryStatus::Managed
                    && entry.reason.as_deref() == Some(UNSUPERVISED_WORKER)
                    && entry.worker_id.as_deref() == Some(snapshot.worker_id.as_str())
                    && entry.worker_instance_id.as_deref()
                        == snapshot
                            .worker_instance_id
                            .as_ref()
                            .map(pohunek_worker_protocol::WorkerInstanceId::as_str)
            });
        if noted {
            return;
        }
        tracing::warn!(
            name: "reconcile.adopt.job_absent",
            session_id,
            service_id = %service_id,
            worker_id = %snapshot.worker_id,
            "adopting a live worker whose supervisor job is absent"
        );
        let entry = {
            let mut inventory = self.inner.runtime_inventory.lock().await;
            let position = inventory.iter().position(|entry| {
                entry.runtime_slot == session_id && entry.status == RuntimeInventoryStatus::Managed
            });
            let entry = if let Some(position) = position {
                &mut inventory[position]
            } else {
                inventory.push(RuntimeInventoryEntry {
                    runtime_slot: session_id.to_owned(),
                    claimed_session_id: Some(session_id.to_owned()),
                    worker_id: Some(snapshot.worker_id.to_string()),
                    worker_instance_id: snapshot
                        .worker_instance_id
                        .as_ref()
                        .map(ToString::to_string),
                    status: RuntimeInventoryStatus::Managed,
                    reason: None,
                });
                inventory.sort_by(|left, right| left.runtime_slot.cmp(&right.runtime_slot));
                inventory
                    .iter_mut()
                    .find(|entry| {
                        entry.runtime_slot == session_id
                            && entry.status == RuntimeInventoryStatus::Managed
                    })
                    .expect("the managed entry was just inserted")
            };
            entry.reason = Some(UNSUPERVISED_WORKER.to_owned());
            entry.clone()
        };
        let event = crate::events::event(
            event::SESSION_RUNTIME_DISCOVERED,
            event_payload(RuntimeInventoryEvent { entry }),
        );
        let _ = self.inner.events.send(event);
    }

    /// Classifies a running session whose worker control connection is gone.
    ///
    /// Runs the unreachable-worker rows of reconciliation for the exact
    /// generation the entry names, under the session's lifecycle lock: a
    /// proven crash sweeps the runtime's marked processes, retires the job,
    /// and is `Lost` only once the job is retired; a job or process that may
    /// still be live is `Conflict` and re-checked in the background; an
    /// unavailable supervisor, or a job it could not retire, leaves the
    /// runtime `Reconnecting` and schedules the background retry. With
    /// `proven_only`, anything short of a proven crash is left `Pending` so
    /// the watcher keeps reconnecting.
    pub(super) async fn classify_lost_worker(
        &self,
        id: &SessionId,
        expected: &super::RuntimeWatchIdentity,
        proven_only: bool,
    ) -> LossClassification {
        let _guard = self.lock_lifecycle(id).await;
        let job = {
            let sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get(id) else {
                return LossClassification::Settled;
            };
            if !expected.matches(entry) || entry.stopping {
                return LossClassification::Settled;
            }
            entry.job.clone()
        };
        let lifecycle = self.lifecycle().ok();
        let decision = self
            .classify_running_generation(id, expected, job.as_ref(), lifecycle.as_ref())
            .await;
        if proven_only
            && !matches!(
                decision,
                Unreachable::Ended {
                    journaled: true,
                    ..
                }
            )
        {
            return LossClassification::Pending;
        }
        let (state, reason) = match decision {
            Unreachable::Unavailable(detail) => {
                tracing::debug!(session_id = %id.0, detail, "worker supervisor is unavailable; nothing is touched");
                (RuntimeState::Reconnecting, SUPERVISION_UNAVAILABLE)
            }
            Unreachable::Ambiguous(detail) => {
                tracing::debug!(session_id = %id.0, detail, "unreachable worker may still be live; nothing is touched");
                (RuntimeState::Conflict, SUPERVISION_AMBIGUOUS)
            }
            Unreachable::Mismatch(detail) => {
                tracing::debug!(session_id = %id.0, detail, "unreachable worker identity mismatch; failing closed");
                (RuntimeState::Conflict, IDENTITY_MISMATCH)
            }
            Unreachable::Ended {
                journaled,
                worker_instance_id,
                bounds,
            } => {
                // Only a journaled worker whose process is proven gone proves
                // which runtime died. Without a journal nothing proves what
                // ran, so no process is swept; the ended job is still retired.
                let cleanup = if journaled {
                    let worker_instance_id =
                        worker_instance_id.unwrap_or_else(|| expected.worker_instance_id.clone());
                    self.sweep_lost_runtime(&id.0, &worker_instance_id, bounds)
                        .await
                } else {
                    Cleanup::Complete
                };
                let retired = match (lifecycle.as_ref(), job.as_ref()) {
                    (Some(lifecycle), Some(generation)) => {
                        lifecycle.retire(generation).await.map_err(|error| {
                            tracing::warn!(
                                session_id = %id.0,
                                service_id = %generation.service_id(),
                                error = %error,
                                "failed to retire the crashed worker generation"
                            );
                        })
                    }
                    _ => Ok(()),
                };
                // `Lost` counts as ended, so a later removal skips retirement:
                // until the job is retired the session stays unavailable and
                // the background re-check reaches this classification again.
                if retired.is_err() {
                    (RuntimeState::Reconnecting, SUPERVISION_UNAVAILABLE)
                } else {
                    let reason = match (journaled, cleanup) {
                        (false, _) => "worker_unavailable",
                        (true, Cleanup::Complete) => RUNTIME_LOST,
                        (true, Cleanup::Unconfirmed) => RUNTIME_LOST_CLEANUP_UNCONFIRMED,
                    };
                    (RuntimeState::Lost, reason)
                }
            }
        };
        match self
            .mark_worker_unavailable(id, expected, state, reason)
            .await
        {
            super::RuntimeTransitionOutcome::Applied(_) => {
                // A conflicted runtime is watched too: its worker or job may
                // end without anything else noticing.
                if state == RuntimeState::Conflict || reason == SUPERVISION_UNAVAILABLE {
                    self.schedule_supervision_retry(id);
                }
                LossClassification::Settled
            }
            super::RuntimeTransitionOutcome::IdentityMismatch => LossClassification::Settled,
            super::RuntimeTransitionOutcome::RetryablePersistenceFailure(_)
            | super::RuntimeTransitionOutcome::RetryableConcurrentChange => {
                LossClassification::Pending
            }
        }
    }

    /// Decides what the unreachable worker of a running entry is.
    async fn classify_running_generation(
        &self,
        id: &SessionId,
        expected: &super::RuntimeWatchIdentity,
        job: Option<&super::Generation>,
        lifecycle: Option<&Lifecycle<'_>>,
    ) -> Unreachable {
        let Some(generation) = job else {
            return Unreachable::Mismatch("the session names no worker generation".to_owned());
        };
        let scan = match self.discover_worker_journals().await {
            Ok(mut journals) => journals.remove(&id.0).unwrap_or_default(),
            Err(detail) => return Unreachable::Ambiguous(detail),
        };
        // An unreadable journal of the session could be this worker's, so
        // its absence below would prove nothing.
        if scan.conflict {
            return Unreachable::Ambiguous(format!(
                "session {} has an unreadable or mismatched worker journal",
                id.0
            ));
        }
        let journal = scan
            .evidence
            .iter()
            .find(|journal| journal.worker_id == expected.worker_id);
        if let Some(journal) = journal {
            if journal.generation != generation.generation() {
                return Unreachable::Mismatch(format!(
                    "worker journal names generation {} instead of {}",
                    journal.generation,
                    generation.generation()
                ));
            }
        }
        let evidence = observe(lifecycle, generation).await;
        let worker = journal.map(JournalEvidence::worker);
        classify_unreachable(
            &evidence,
            generation,
            worker.as_ref(),
            self.inner.inspector.as_ref(),
        )
    }

    /// Compensates a create that never produced a usable runtime: removes
    /// its worktree and binding, then its record.
    ///
    /// Callers prove the create's generation ended first (never started, or
    /// retired by exact generation), since removing the worktree deletes the
    /// checkout. The record goes last and stays while any checkout of the
    /// session is still on disk or its binding cannot be dropped, so it keeps
    /// guarding the rollback. A compensation that cannot finish leaves the
    /// session listed as `reconnecting` with
    /// [`CREATE_COMPENSATION_PENDING`], and the supervision retry repeats it
    /// in the running daemon ([`Self::try_compensate_create`]); a later
    /// reconciliation of the preparing record repeats it as well, and a
    /// repeat finds nothing left to remove.
    ///
    /// Returns whether the create is fully compensated.
    pub(super) async fn compensate_abandoned_create(&self, id: &SessionId) -> bool {
        if self.try_compensate_create(id).await {
            return true;
        }
        self.defer_create_compensation(id).await;
        false
    }

    /// Makes one attempt at [`Self::compensate_abandoned_create`].
    ///
    /// A listed entry of the create leaves the registry with its record, and
    /// subscribers see it removed.
    ///
    /// Returns whether the create is fully compensated; `false` keeps the
    /// record and any entry untouched.
    pub(super) async fn try_compensate_create(&self, id: &SessionId) -> bool {
        if !self.cleanup_bound_worktree(id).await {
            tracing::warn!(
                session_id = %id.0,
                "abandoned create keeps its record until its worktree and binding are removed"
            );
            return false;
        }
        if let Err(error) = self.delete_session_record(id).await {
            tracing::warn!(
                session_id = %id.0,
                error = %error,
                "failed to compensate abandoned preparing session"
            );
            return false;
        }
        let removed = self.remove_session_entry(id).await;
        if let Some(entry) = removed {
            self.emit(event::SESSION_REMOVED, &entry.info);
        }
        true
    }

    /// Lists a create whose compensation did not finish as
    /// [`CREATE_COMPENSATION_PENDING`] and hands it to the supervision retry.
    ///
    /// An entry already showing that reason is left, and its subscribers,
    /// untouched. A record that cannot be loaded or classified stays on disk
    /// for the next startup reconciliation.
    async fn defer_create_compensation(&self, id: &SessionId) {
        let listed = self
            .inner
            .sessions
            .lock()
            .await
            .get(id)
            .is_some_and(|entry| {
                entry.info.runtime.as_ref().is_some_and(|runtime| {
                    runtime.state == RuntimeState::Reconnecting
                        && runtime.loss_reason.as_deref() == Some(CREATE_COMPENSATION_PENDING)
                })
            });
        if !listed {
            match self.load_durable_session_record(id).await {
                Ok(Some(record)) => {
                    // The retry is scheduled below whether or not the listing
                    // was committed.
                    let _persisted = self
                        .insert_unavailable_record(
                            record,
                            RuntimeState::Reconnecting,
                            CREATE_COMPENSATION_PENDING,
                        )
                        .await;
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(
                        session_id = %id.0,
                        error = %error,
                        "unfinished create record cannot be loaded; the next start compensates it"
                    );
                    return;
                }
            }
        }
        self.schedule_supervision_retry(id);
    }

    /// Settles a preparing create whose runtime is proven ended: its child
    /// exited (a worker may still retain the final output) or its terminal
    /// journal says so.
    ///
    /// The job of `generation` is retired first, so no runtime of the create
    /// can still use its checkout; the worktree and binding are compensated
    /// next and the record goes last ([`Self::compensate_abandoned_create`]).
    /// A retirement the supervisor cannot confirm keeps the session
    /// `runtime_supervision_unavailable` for the supervision retry, and an
    /// incomplete compensation leaves it `create_compensation_pending` for the
    /// same retry, which schedules itself.
    ///
    /// Returns whether the session needs the background re-check.
    async fn settle_ended_create(
        &self,
        record: SessionRecord,
        generation: Option<&super::Generation>,
        lifecycle: Option<&Lifecycle<'_>>,
        retry: bool,
    ) -> bool {
        let id = SessionId(record.session_id.clone());
        if !self
            .retire_terminal_generation(&id.0, generation, lifecycle)
            .await
        {
            if !retry || self.classification_unpersisted(&record.session_id) {
                // An uncommitted classification leaves the session pending
                // all the same.
                let _persisted = self
                    .insert_unavailable_record(
                        record,
                        RuntimeState::Reconnecting,
                        SUPERVISION_UNAVAILABLE,
                    )
                    .await;
            }
            return true;
        }
        self.compensate_abandoned_create(&id).await;
        false
    }

    /// Collects the worker processes a removal of `id` must prove gone.
    ///
    /// Reads every journal of the session from a fresh scan, the same one
    /// reconciliation uses, independent of which worker the record names:
    /// a preparing record names no worker, and several journals may claim
    /// one generation. Each journal of `generation` whose runtime has not
    /// ended contributes its worker process. A worker journaled under
    /// another generation must already be gone, since retiring `generation`
    /// cannot stop it. A worker journals before it owns a PTY, so a
    /// successful scan without such journals proves no PTY to orphan.
    ///
    /// # Errors
    ///
    /// Returns `runtime_supervision_ambiguous` when the journals cannot be
    /// scanned or one of them is unreadable, or a foreign-generation worker
    /// cannot be inspected, and `runtime_identity_mismatch` when a journal
    /// records a malformed worker identity or a worker of another generation
    /// still runs.
    pub(super) async fn removal_worker_processes(
        &self,
        id: &SessionId,
        generation: &super::Generation,
    ) -> Result<Vec<crate::procwatch::ProcessIdentity>, ProtocolError> {
        let ambiguous = |detail: String| {
            ProtocolError::new(
                protocol::ErrorClass::Runtime,
                SUPERVISION_AMBIGUOUS,
                format!("session {} runtime cannot be released: {detail}", id.0),
                None,
            )
        };
        let mismatch = |detail: String| {
            ProtocolError::new(
                protocol::ErrorClass::Runtime,
                IDENTITY_MISMATCH,
                format!("session {} runtime cannot be released: {detail}", id.0),
                None,
            )
        };
        let scan = self
            .discover_worker_journals()
            .await
            .map_err(ambiguous)?
            .remove(&id.0)
            .unwrap_or_default();
        if scan.conflict {
            return Err(ambiguous(
                "the session has an unreadable or mismatched worker journal".to_owned(),
            ));
        }
        let mut workers = Vec::new();
        for journal in scan.evidence.iter().filter(|journal| {
            !matches!(
                journal.phase,
                JournalPhase::Terminal | JournalPhase::NeverInitialized
            )
        }) {
            let identity = journal.worker().identity().map_err(mismatch)?;
            if journal.generation == generation.generation() {
                workers.push(identity);
                continue;
            }
            match self.inner.inspector.is_running(identity) {
                Ok(false) => {}
                Ok(true) => {
                    return Err(mismatch(format!(
                        "worker {} of generation {} is still running",
                        journal.worker_id, journal.generation
                    )));
                }
                Err(error) => {
                    return Err(ambiguous(format!(
                        "worker {} of generation {} cannot be inspected: {error}",
                        journal.worker_id, journal.generation
                    )));
                }
            }
        }
        Ok(workers)
    }

    /// Proves that the runtime a conflicted session records is the one a stop
    /// may end: the journal of the recorded generation names the recorded
    /// worker, and the job the supervisor shows under the generation's service
    /// id is that worker's.
    ///
    /// A stop acts on a service id, which a foreign or rewritten job can hold,
    /// so nothing is retired on unproven identity.
    ///
    /// # Errors
    ///
    /// Returns `runtime_supervision_ambiguous` when the journals cannot be
    /// scanned or one is unreadable, `runtime_supervision_unavailable` when
    /// the job cannot be inspected, and `runtime_identity_mismatch` when no
    /// journal of the generation names the recorded worker, the journal does
    /// not name the recorded runtime (`worker_instance_id`), or the job is not
    /// that worker's.
    pub(super) async fn prove_conflicted_runtime(
        &self,
        id: &SessionId,
        generation: &super::Generation,
        worker_id: &str,
        worker_instance_id: Option<&str>,
        lifecycle: &Lifecycle<'_>,
    ) -> Result<(), ProtocolError> {
        let refusal = |code: &str, detail: String| {
            ProtocolError::new(
                protocol::ErrorClass::Runtime,
                code,
                format!("session {} cannot be stopped: {detail}", id.0),
                None,
            )
        };
        let scan = self
            .discover_worker_journals()
            .await
            .map_err(|detail| refusal(SUPERVISION_AMBIGUOUS, detail))?
            .remove(&id.0)
            .unwrap_or_default();
        let journal = scan
            .journal_of_generation(generation.generation())
            .map_err(|detail| refusal(SUPERVISION_AMBIGUOUS, detail))?
            .filter(|journal| journal.worker_id == worker_id)
            .ok_or_else(|| {
                refusal(
                    IDENTITY_MISMATCH,
                    format!(
                        "no worker journal of generation {} names worker {worker_id}",
                        generation.generation()
                    ),
                )
            })?;
        // The stop sweeps the runtime the journal proves; a recorded runtime
        // the journal does not name could belong to any other session.
        if let Some(recorded) = worker_instance_id {
            if journal.worker().worker_instance_id != Some(recorded) {
                return Err(refusal(
                    IDENTITY_MISMATCH,
                    format!(
                        "the record names runtime {recorded}, which the journal of worker {worker_id} does not"
                    ),
                ));
            }
        }
        match lifecycle.inspect(generation).await {
            Ok(Some(observation)) => {
                if let Some(detail) =
                    job_identity_mismatch(&observation, generation, Some(&journal.worker()))
                {
                    return Err(refusal(IDENTITY_MISMATCH, detail));
                }
            }
            // No job holds the worker, so retiring cannot end it: a worker
            // that still runs is refused before anything is written.
            Ok(None) => {
                let identity = journal
                    .worker()
                    .identity()
                    .map_err(|detail| refusal(IDENTITY_MISMATCH, detail))?;
                match self.inner.inspector.is_running(identity) {
                    Ok(false) => {}
                    Ok(true) => {
                        return Err(refusal(
                            SUPERVISION_AMBIGUOUS,
                            format!(
                                "no supervisor job holds worker process {}, which still runs",
                                identity.pid
                            ),
                        ));
                    }
                    Err(error) => {
                        return Err(refusal(
                            SUPERVISION_AMBIGUOUS,
                            format!(
                                "worker process {} cannot be inspected: {error}",
                                identity.pid
                            ),
                        ));
                    }
                }
            }
            Err(error) => {
                return Err(refusal(
                    SUPERVISION_UNAVAILABLE,
                    format!(
                        "job {} cannot be inspected: {error}",
                        generation.service_id()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Retires `generation` for the removal of `id` and proves every worker
    /// that may still own one of its PTYs gone.
    ///
    /// Removal deletes the only record of the generation, so the workers are
    /// collected from a fresh journal scan ([`Self::removal_worker_processes`])
    /// and must be gone after the job is retired
    /// ([`Lifecycle::retire_for_removal`]).
    ///
    /// # Errors
    ///
    /// Returns `runtime_supervision_unavailable` when the supervisor cannot
    /// retire the job, and `runtime_supervision_ambiguous` or
    /// `runtime_identity_mismatch` when a worker is not proven gone.
    pub(super) async fn retire_generation_for_removal(
        &self,
        id: &SessionId,
        generation: &super::Generation,
        lifecycle: &Lifecycle<'_>,
    ) -> Result<(), ProtocolError> {
        let workers = self.removal_worker_processes(id, generation).await?;
        lifecycle
            .retire_for_removal(generation, &workers)
            .await
            .map_err(crate::runtime::lifecycle::PreviousLive::into_error)
    }

    /// Confirms cleanup of the exact journaled runtime before native recovery.
    ///
    /// The record's runtime id is not a signal permit by itself: only a
    /// matching journal of its generation proves which markers may be swept.
    ///
    /// # Errors
    ///
    /// Returns `runtime_supervision_ambiguous` when journal evidence is missing
    /// or inconsistent, or when the marker sweep cannot confirm cleanup.
    pub(super) async fn confirm_recovery_cleanup(
        &self,
        id: &SessionId,
        generation: Option<&super::Generation>,
        worker_id: Option<&str>,
        worker_instance_id: Option<&str>,
    ) -> Result<(), ProtocolError> {
        let refusal = |detail: String| {
            ProtocolError::new(
                protocol::ErrorClass::Runtime,
                SUPERVISION_AMBIGUOUS,
                format!("session {} cannot be recovered: {detail}", id.0),
                Some("inspect and end the previous runtime's marked processes, then retry session.resume".to_owned()),
            )
        };
        let (Some(generation), Some(worker_id), Some(worker_instance_id)) =
            (generation, worker_id, worker_instance_id)
        else {
            return Err(refusal(
                "the previous runtime has no complete journal identity".to_owned(),
            ));
        };
        let scan = self
            .discover_worker_journals()
            .await
            .map_err(refusal)?
            .remove(&id.0)
            .unwrap_or_default();
        let journal = scan
            .journal_of_generation(generation.generation())
            .map_err(refusal)?
            .filter(|journal| {
                journal.worker_id == worker_id
                    && journal.worker().worker_instance_id == Some(worker_instance_id)
            })
            .ok_or_else(|| {
                refusal(
                    "the recorded worker and runtime do not match a journal of their generation"
                        .to_owned(),
                )
            })?;
        let worker = journal.worker();
        worker.identity().map_err(refusal)?;
        let outcome = self
            .sweep_lost_runtime_detailed(&id.0, worker_instance_id, worker.bounds())
            .await;
        if outcome.cleanup == Cleanup::Unconfirmed {
            return Err(refusal(
                "marked process cleanup is still unconfirmed".to_owned(),
            ));
        }
        Ok(())
    }

    /// Proves that no marked process of a runtime the removal of `id`
    /// forgets is left.
    ///
    /// Removal deletes the only record of the session's runtimes, and a
    /// descendant that left the worker's process group (launchd kills only
    /// the group) is reaped by nothing but the marker sweep. Every runtime
    /// the record names (`worker_instance_id`) or a journal of `generation` records,
    /// terminal journals included, is swept. A journaled runtime is bounded
    /// by its worker's start identity. A runtime only the record names also
    /// requires a matching session marker, because its runtime marker alone
    /// could belong to another session. Callers sweep only
    /// once every worker of the generation is proven gone, so no live
    /// worker is still starting marked processes.
    ///
    /// Under [`UnconfirmedCleanup::Accept`] a runtime whose sweep is
    /// unconfirmed solely because of unreadable-marker processes does not
    /// fail the sweep: those processes are returned (all runtimes
    /// concatenated, each with the runtime that first listed it) and never
    /// signalled.
    ///
    /// # Errors
    ///
    /// Returns `runtime_supervision_ambiguous` when the journals cannot be
    /// scanned or one of them is unreadable, or a sweep cannot confirm that
    /// every marked process exited and `cleanup` does not cover why. The
    /// caller keeps the record and its removal intent.
    pub(super) async fn sweep_removed_runtimes(
        &self,
        id: &SessionId,
        generation: Option<&super::Generation>,
        worker_instance_id: Option<&str>,
        cleanup: UnconfirmedCleanup,
    ) -> Result<Vec<(UnconfirmedProcess, String)>, ProtocolError> {
        let ambiguous = |detail: String| {
            ProtocolError::new(
                protocol::ErrorClass::Runtime,
                SUPERVISION_AMBIGUOUS,
                format!("session {} runtime cannot be released: {detail}", id.0),
                None,
            )
        };
        // The marker requirement applies only to a runtime no journal names.
        let mut runtimes: Vec<(String, WorkerBounds, bool)> = Vec::new();
        if let Some(generation) = generation {
            let scan = self
                .discover_worker_journals()
                .await
                .map_err(ambiguous)?
                .remove(&id.0)
                .unwrap_or_default();
            if scan.conflict {
                return Err(ambiguous(
                    "the session has an unreadable or mismatched worker journal".to_owned(),
                ));
            }
            for journal in scan
                .evidence
                .iter()
                .filter(|journal| journal.generation == generation.generation())
            {
                let worker = journal.worker();
                let Some(journaled) = worker.worker_instance_id else {
                    continue;
                };
                // A malformed start identity proves no bystander foreign, so
                // the sweep then treats every unreadable marker as the
                // runtime's own.
                let bounds = worker.bounds();
                match runtimes.iter_mut().find(|(known, _, _)| known == journaled) {
                    // Several workers journaling one runtime cannot tell
                    // which one started it, so a fact bounds it only when
                    // every one of them agrees on it.
                    Some((_, known, _)) => *known = known.agreeing(bounds),
                    None => runtimes.push((journaled.to_owned(), bounds, false)),
                }
            }
        }
        if let Some(worker_instance_id) = worker_instance_id {
            if !runtimes
                .iter()
                .any(|(known, _, _)| known == worker_instance_id)
            {
                runtimes.push((worker_instance_id.to_owned(), WorkerBounds::NONE, true));
            }
        }
        // Each accepted process with the runtime that first listed it. The
        // caller logs them once the removal has completed.
        let mut accepted: Vec<(UnconfirmedProcess, String)> = Vec::new();
        for (worker_instance_id, bounds, requires_session_marker) in runtimes {
            let outcome = if requires_session_marker {
                self.sweep_unjournaled_removed_runtime_detailed(&id.0, &worker_instance_id)
                    .await
            } else {
                self.sweep_lost_runtime_detailed(&id.0, &worker_instance_id, bounds)
                    .await
            };
            if outcome.cleanup != Cleanup::Unconfirmed {
                continue;
            }
            if cleanup == UnconfirmedCleanup::Accept && outcome.only_unreadable_candidates_blocked()
            {
                for candidate in &outcome.unreadable {
                    record_accepted_process(&mut accepted, candidate, &worker_instance_id);
                    // Stops at the first excess process so the work stays
                    // bounded however many candidates a host has.
                    if accepted.len() > MAX_ACCEPTED_UNCONFIRMED_PROCESSES {
                        let mut error = ambiguous(format!(
                            "more than the {MAX_ACCEPTED_UNCONFIRMED_PROCESSES} unreadable processes one removal can accept may belong to its runtimes"
                        ));
                        error.recover = Some(UNREADABLE_CANDIDATES_RECOVER.to_owned());
                        return Err(error);
                    }
                }
                continue;
            }
            let mut error = ambiguous(format!(
                "processes of runtime {worker_instance_id} are not proven gone"
            ));
            if outcome.only_unreadable_candidates_blocked() {
                error.msg = format!(
                    "{}; processes whose environment cannot be read may belong to it: {}",
                    error.msg,
                    describe_unreadable_candidates(&outcome.unreadable)
                );
                error.recover = Some(UNREADABLE_CANDIDATES_RECOVER.to_owned());
            }
            return Err(error);
        }
        Ok(accepted)
    }

    /// Finishes the durable removal intent of `record` once its runtime is
    /// proven ended or stopped.
    ///
    /// The exact recorded generation is retired and its workers proven gone,
    /// then [`Self::release_removed_session`] sweeps the session's runtimes
    /// and deletes everything the session owns, the durable record last. The
    /// sweep always refuses on unconfirmed cleanup
    /// ([`UnconfirmedCleanup::Refuse`]): only an explicit `session.remove`
    /// call consents to it.
    /// Without a supervisor or a recorded generation there is no job to
    /// retire. Returns whether the removal needs the background re-check: a
    /// generation the supervisor cannot retire keeps the session
    /// `runtime_supervision_unavailable`, a worker or a marked process not
    /// proven gone keeps it `runtime_supervision_ambiguous`, and a failed
    /// cleanup keeps it `runtime_supervision_unavailable`; each re-check
    /// reaches this finalizer again from fresh evidence. A worker of another
    /// generation that still runs is an identity conflict and is not retried.
    ///
    /// A Remove intent with the conflict-proven transaction phase
    /// (`PROVEN_CONFLICT_REMOVAL_PHASE`, a conflict removal whose identity
    /// was proven once, then interrupted) is re-proven
    /// ([`Self::prove_conflicted_runtime`]) before any retirement: the job
    /// under the service ID may have changed while the daemon was down, and a
    /// foreign or replaced job is never retired.
    async fn finish_removal_intent(
        &self,
        record: SessionRecord,
        generation: Option<&super::Generation>,
        lifecycle: Option<&Lifecycle<'_>>,
        retry: bool,
    ) -> bool {
        let id = SessionId(record.session_id.clone());
        if let (Some(generation), Some(lifecycle)) = (generation, lifecycle) {
            if let Err(error) = self
                .retire_removal_intent(&id, &record, generation, lifecycle)
                .await
            {
                return self.settle_failed_removal(record, &error, retry).await;
            }
        }
        let worker_instance_id = record.runtime.worker_instance_id.clone();
        match self
            .release_removed_session(
                &id,
                generation,
                worker_instance_id.as_deref(),
                UnconfirmedCleanup::Refuse,
            )
            .await
        {
            Ok(_released) => false,
            Err(error) => {
                tracing::warn!(session_id = %id.0, error = %error, "failed to finish reconciled removal");
                let (state, reason) = if error.code == SUPERVISION_AMBIGUOUS {
                    (RuntimeState::Conflict, SUPERVISION_AMBIGUOUS)
                } else {
                    (RuntimeState::Reconnecting, SUPERVISION_UNAVAILABLE)
                };
                self.mark_pending(record, state, reason, retry).await;
                true
            }
        }
    }

    /// Re-proves a conflict-proven Remove intent's recorded identity before
    /// its retirement, then retires its recorded generation.
    ///
    /// The re-proof ([`Self::prove_conflicted_runtime`]) judges the recorded
    /// worker ID and runtime against a fresh journal scan and the job the
    /// supervisor currently shows under the service ID, as a live removal
    /// does, so a foreign or replaced job is never retired.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::prove_conflicted_runtime`] and
    /// [`Self::retire_generation_for_removal`], and
    /// `runtime_identity_mismatch` when the intent names no complete worker
    /// identity.
    async fn retire_removal_intent(
        &self,
        id: &SessionId,
        record: &SessionRecord,
        generation: &super::Generation,
        lifecycle: &Lifecycle<'_>,
    ) -> Result<(), ProtocolError> {
        if is_conflict_proven_removal(record) {
            Box::pin(self.reprove_conflict_removal_intent(id, record, generation, lifecycle))
                .await?;
        }
        if let Err(error) =
            Box::pin(self.retire_generation_for_removal(id, generation, lifecycle)).await
        {
            tracing::warn!(
                session_id = %id.0,
                service_id = %generation.service_id(),
                error = %error,
                "removal intent waits for its generation to be retired"
            );
            return Err(error);
        }
        Ok(())
    }

    /// Re-proves a conflict-proven Remove intent's recorded identity before
    /// its retirement.
    ///
    /// The recorded worker ID and runtime are judged against a fresh journal
    /// scan and the job the supervisor currently shows under the service ID,
    /// as [`Self::prove_conflicted_runtime`] does for a live removal.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::prove_conflicted_runtime`], and
    /// `runtime_identity_mismatch` when the record names no worker.
    async fn reprove_conflict_removal_intent(
        &self,
        id: &SessionId,
        record: &SessionRecord,
        generation: &super::Generation,
        lifecycle: &Lifecycle<'_>,
    ) -> Result<(), ProtocolError> {
        let worker_id = record.runtime.worker_id.as_deref();
        let worker_instance_id = record.runtime.worker_instance_id.as_deref();
        Box::pin(async {
            match (worker_id, worker_instance_id) {
                (Some(worker_id), Some(worker_instance_id)) => {
                    self.prove_conflicted_runtime(
                        id,
                        generation,
                        worker_id,
                        Some(worker_instance_id),
                        lifecycle,
                    )
                    .await
                }
                // The proof pinned a recorded worker; a record without one can no
                // longer be proven.
                _ => Err(ProtocolError::new(
                    protocol::ErrorClass::Runtime,
                    IDENTITY_MISMATCH,
                    format!(
                        "session {} cannot be removed: its removal intent names no complete worker identity",
                        id.0
                    ),
                    None,
                )),
            }
        })
        .await
    }

    /// Shows `record` after a failed removal retirement or identity proof.
    ///
    /// Identity conflicts are not retried; everything else waits for a
    /// re-check that reaches the finalizer again from fresh evidence.
    async fn settle_failed_removal(
        &self,
        record: SessionRecord,
        error: &ProtocolError,
        retry: bool,
    ) -> bool {
        let (state, reason) = match error.code.as_str() {
            SUPERVISION_UNAVAILABLE => (RuntimeState::Reconnecting, SUPERVISION_UNAVAILABLE),
            SUPERVISION_AMBIGUOUS => (RuntimeState::Conflict, SUPERVISION_AMBIGUOUS),
            _ => {
                return !self
                    .insert_unavailable_record(record, RuntimeState::Conflict, IDENTITY_MISMATCH)
                    .await;
            }
        };
        self.mark_pending(record, state, reason, retry).await;
        true
    }

    /// Reads the journal of exactly `key`'s generation from a fresh scan.
    ///
    /// # Errors
    ///
    /// Returns why the journal cannot be decided: the scan failed, a journal
    /// file of the session is unreadable, or several journals name the
    /// generation.
    pub(super) async fn generation_journal(
        &self,
        key: &WorkerKey,
    ) -> Result<Option<JournalEvidence>, String> {
        let journals = self.discover_worker_journals().await?;
        let Some(scan) = journals.get(key.session_id()) else {
            return Ok(None);
        };
        scan.journal_of_generation(key.generation())
            .map(Option::<&JournalEvidence>::cloned)
    }

    /// The snapshot the journal of exactly `record`'s runtime generation
    /// stands for, or `None` when that journal is absent, ambiguous or
    /// unreadable.
    pub(super) async fn generation_journal_snapshot(
        &self,
        record: &SessionRecord,
    ) -> Option<InspectSnapshot> {
        self.generation_journal_snapshot_checked(record)
            .await
            .ok()
            .flatten()
    }

    /// Reads an exact generation without treating failed evidence as absence.
    ///
    /// Explicit recovery must propagate the reason; terminal import may use
    /// [`Self::generation_journal_snapshot`] to leave a target unchanged.
    pub(super) async fn generation_journal_snapshot_checked(
        &self,
        record: &SessionRecord,
    ) -> Result<Option<InspectSnapshot>, String> {
        let Some(generation) = record.runtime.generation.as_deref() else {
            return Ok(None);
        };
        let journals = self.discover_worker_journals().await?;
        let Some(scan) = journals.get(record.session_id.as_str()) else {
            return Ok(None);
        };
        scan.journal_of_generation(generation)?
            .map(|journal| journal.snapshot().map_err(str::to_owned))
            .transpose()
    }

    /// Whether this generation journal carries an unpromoted native claim.
    /// An unreadable or ambiguous journal is never classified as absent.
    ///
    /// The claim is read from the durable journal itself, not from the
    /// snapshot it builds: the snapshot filters an active claim whose lease
    /// has expired, while the report the agent made stays in the journal. A
    /// diagnostic must not turn the verification failure it expired from into
    /// a claim of absence, so an expired unpromoted claim still reads as
    /// present. The snapshot is still built first, so malformed evidence
    /// remains an error instead of a false absence.
    pub(super) async fn generation_has_native_claim(
        &self,
        record: &SessionRecord,
    ) -> Result<bool, String> {
        let Some(generation) = record.runtime.generation.as_deref() else {
            return Ok(false);
        };
        let journals = self.discover_worker_journals().await?;
        let Some(scan) = journals.get(record.session_id.as_str()) else {
            return Ok(false);
        };
        let Some(journal) = scan.journal_of_generation(generation)? else {
            return Ok(false);
        };
        journal.snapshot().map_err(str::to_owned)?;
        Ok(journal
            .active_identity
            .as_ref()
            .is_some_and(|claim| claim.native_reference.is_some())
            || journal.native_reference_claim.is_some())
    }

    /// Scans every session's worker journals.
    /// Scans every session's worker journals.
    ///
    /// Returns why the scan failed instead of an empty map: a failed scan is
    /// no proof that any worker never journaled.
    async fn discover_worker_journals(&self) -> Result<HashMap<String, WorkerJournalScan>, String> {
        let Some(state_root) = self.inner.config.worker_state_root.clone() else {
            return Ok(HashMap::new());
        };
        match tokio::task::spawn_blocking(move || scan_worker_journals(&state_root)).await {
            Ok(Ok(journals)) => Ok(journals),
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "failed to scan durable worker journals");
                Err(format!("worker journals cannot be scanned: {error}"))
            }
            Err(error) => {
                tracing::warn!(error = %error, "durable worker journal scan task panicked");
                Err(format!("worker journal scan failed: {error}"))
            }
        }
    }

    /// Retires the job of a generation whose terminal journal proves it ended.
    ///
    /// Returns whether nothing is left to retry: the job was retired (an
    /// absent job counts), or there is no generation or supervisor to retire
    /// it through. A failed retirement is logged and returns `false`, so the
    /// caller keeps the session for the background re-check.
    async fn retire_terminal_generation(
        &self,
        session_id: &str,
        generation: Option<&super::Generation>,
        lifecycle: Option<&Lifecycle<'_>>,
    ) -> bool {
        let (Some(generation), Some(lifecycle)) = (generation, lifecycle) else {
            return true;
        };
        match lifecycle.retire(generation).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    service_id = %generation.service_id(),
                    error = %error,
                    "failed to retire the terminal worker generation"
                );
                false
            }
        }
    }

    /// Re-attempts the retirement of a terminal record's generation.
    ///
    /// Used by the supervision retry, with the session's lifecycle lock held,
    /// for a session whose terminal entry kept its generation's job. The
    /// generation is retired only while its journal still proves it terminal
    /// exactly as reconciliation did and no worker answers on the session's
    /// socket; the entry itself is never changed. Returns whether the session
    /// no longer needs a retry: incomplete evidence or a failed retirement
    /// keeps it pending, while evidence that proves nothing leaves the job to
    /// the next startup reconciliation.
    pub(super) async fn retry_terminal_retirement(&self, record: &SessionRecord) -> bool {
        // A malformed generation was already classified; nothing names a job.
        let Ok(Some(generation)) =
            super::Generation::from_record(&record.session_id, &record.runtime)
        else {
            return true;
        };
        let Ok(lifecycle) = self.lifecycle() else {
            return true;
        };
        match self.session_socket_evidence(record).await {
            SocketEvidence::Absent => {}
            SocketEvidence::Unknown(_) => return false,
            SocketEvidence::Worker(_) | SocketEvidence::Unusable(..) | SocketEvidence::Multiple => {
                return true;
            }
        }
        let scan = match self.discover_worker_journals().await {
            Ok(mut journals) => journals.remove(&record.session_id).unwrap_or_default(),
            Err(_) => return false,
        };
        let scoped = scan.scoped_to(Some(&generation));
        if !matches!(
            classify_terminal_journals(&scoped, record),
            TerminalJournalClassification::Exact(_)
        ) {
            tracing::warn!(
                session_id = %record.session_id,
                service_id = %generation.service_id(),
                "terminal generation is no longer proven by its journal; its job is left to the next reconciliation"
            );
            return true;
        }
        self.retire_terminal_generation(&record.session_id, Some(&generation), Some(&lifecycle))
            .await
    }

    /// Records the terminal outcome the journal proves; returns whether it was
    /// committed.
    async fn import_terminal_journal(
        &self,
        mut record: SessionRecord,
        evidence: JournalEvidence,
    ) -> bool {
        let runtime_generation = record
            .info
            .runtime
            .as_ref()
            .map_or(protocol::RuntimeGeneration::new(1), |runtime| {
                runtime.runtime_generation
            });
        record.transaction = None;
        let schema = self
            .record_hook_schema(&record, evidence.hook_schema.as_deref())
            .and_then(|schema| map_worker_subagents(&evidence.subagents, schema));
        match schema {
            Ok(subagents) => record.info.subagents = subagents,
            Err(reason) => {
                tracing::warn!(session_id = %record.session_id, reason, "rejected terminal worker subagent journal");
            }
        }
        match evidence.snapshot() {
            Ok(snapshot) => {
                let schema = self.record_hook_schema(&record, snapshot.hook_schema.as_deref());
                import_terminal_native_reference(&mut record, &snapshot, schema);
            }
            Err(reason) => {
                tracing::warn!(session_id = %record.session_id, reason, "unreadable terminal worker journal; its native reference is not imported");
            }
        }
        record.info.pid = evidence.child.as_ref().map_or(0, |child| child.pid);
        if let Some(cols) = evidence.cols {
            record.info.cols = cols;
        }
        if let Some(rows) = evidence.rows {
            record.info.rows = rows;
        }
        let stopped_by_intent = record.desired_state != DesiredState::Running;
        if let Some(outcome) = evidence.outcome {
            record.info.exit_code = outcome.exit_code;
            record.info.state = if stopped_by_intent {
                SessionState::Stopped
            } else if outcome.success && outcome.signal.is_none() {
                SessionState::Done
            } else {
                SessionState::Failed
            };
        } else {
            record.info.state = if stopped_by_intent {
                SessionState::Stopped
            } else {
                SessionState::Failed
            };
        }
        record.info.state_source = StateSource::Process;
        record.runtime.state = RuntimeState::Terminal;
        record.runtime.worker_id = Some(evidence.worker_id.clone());
        record
            .runtime
            .worker_instance_id
            .clone_from(&evidence.worker_instance_id);
        record.runtime.reason = None;
        record.info.runtime = Some(SessionRuntime {
            state: RuntimeState::Terminal,
            runtime_generation,
            worker_id: Some(evidence.worker_id),
            worker_instance_id: evidence.worker_instance_id,
            started_at: record
                .info
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.started_at.clone()),
            last_connected_at: None,
            loss_reason: None,
        });
        self.insert_unavailable_record(record, RuntimeState::Terminal, "worker_journal_terminal")
            .await
    }

    /// Classifies the session's socket from the startup inventory.
    ///
    /// A socket nothing answers on (a crashed worker leaves its socket file
    /// behind) is no evidence either way; the supervisor and journal decide.
    async fn inventory_state_for_slot(&self, slot: &str) -> Option<(RuntimeState, &'static str)> {
        self.inner
            .runtime_inventory
            .lock()
            .await
            .iter()
            .filter(|entry| entry.reason.as_deref() != Some(UNREACHABLE_SOCKET))
            .find(|entry| {
                entry.runtime_slot == slot
                    || entry
                        .claimed_session_id
                        .as_deref()
                        .is_some_and(|id| id == slot)
            })
            .map(|entry| match entry.status {
                RuntimeInventoryStatus::Incompatible => {
                    (RuntimeState::Incompatible, "worker_protocol_incompatible")
                }
                RuntimeInventoryStatus::Conflict
                | RuntimeInventoryStatus::IdentityMismatch
                | RuntimeInventoryStatus::Orphaned => (RuntimeState::Conflict, IDENTITY_MISMATCH),
                RuntimeInventoryStatus::Managed => {
                    (RuntimeState::Reconnecting, "worker_unavailable")
                }
            })
    }

    /// Reconciles `record` with the worker that answers on its socket.
    ///
    /// Returns whether the session needs the background re-check: its durable
    /// intent (removal, stop, or an abandoned create's settlement) is not
    /// finished yet.
    async fn reconcile_record(
        &self,
        record: SessionRecord,
        candidate: Option<(Worker, InspectSnapshot)>,
        retry: bool,
    ) -> bool {
        let id = SessionId(record.session_id.clone());
        let Some((worker, snapshot)) = candidate else {
            return !self
                .insert_unavailable_record(record, RuntimeState::Lost, "worker_unavailable")
                .await;
        };

        if runtime_identity_conflict(
            &record,
            snapshot.worker_id.as_str(),
            snapshot
                .worker_instance_id
                .as_ref()
                .map(pohunek_worker_protocol::WorkerInstanceId::as_str),
        ) {
            self.mark_pending(
                record,
                RuntimeState::Conflict,
                RUNTIME_IDENTITY_MISMATCH,
                retry,
            )
            .await;
            return true;
        }
        if recovery_not_advanced(&record, snapshot.worker_id.as_str()) {
            return !self
                .insert_unavailable_record(
                    record,
                    RuntimeState::Lost,
                    WORKER_GENERATION_NOT_ADVANCED,
                )
                .await;
        }

        if record.desired_state == DesiredState::Removed {
            return Box::pin(self.finish_reconciled_removal(worker, record, retry)).await;
        }

        // A worker still `Stopping` may own its PTY through the stop grace or
        // wait for its terminal journal commit, so a stop intent is replayed
        // there too; the replay returns only once the outcome is committed.
        let replays_stop = record.desired_state == DesiredState::Stopped
            && snapshot.phase == RuntimePhase::Stopping;
        if !replays_stop
            && !matches!(
                snapshot.phase,
                RuntimePhase::Running | RuntimePhase::Starting
            )
        {
            if uncommitted_create(&record) {
                // The worker only retains the final output of a create that
                // never committed; the session is settled without it. The
                // caller bound the worker to the record's generation.
                drop(worker);
                let generation =
                    super::Generation::from_record(&record.session_id, &record.runtime)
                        .ok()
                        .flatten();
                let lifecycle = self.lifecycle().ok();
                return self
                    .settle_ended_create(record, generation.as_ref(), lifecycle.as_ref(), retry)
                    .await;
            }
            return !self.import_ended_worker(record, &snapshot).await;
        }

        if record.desired_state == DesiredState::Stopped {
            return Box::pin(self.finish_reconciled_stop(&id, worker, record, retry)).await;
        }

        Box::pin(self.adopt_live_record(id, worker, record, snapshot, retry)).await
    }

    /// Records the outcome of an answering worker whose runtime is no longer
    /// live: `terminal` with its exit when it exited, `lost` otherwise, with
    /// its final subagent snapshot.
    async fn import_ended_worker(
        &self,
        mut record: SessionRecord,
        snapshot: &InspectSnapshot,
    ) -> bool {
        let projected = InspectSnapshot {
            hook_schema: self.effective_hook_schema_id(&record, snapshot),
            ..snapshot.clone()
        };
        let snapshot = &projected;
        let state = if snapshot.phase == RuntimePhase::Exited {
            RuntimeState::Terminal
        } else {
            RuntimeState::Lost
        };
        match import_worker_subagents(snapshot) {
            Ok(subagents) if !subagents.is_empty() || record.info.subagents.is_empty() => {
                record.info.subagents = subagents;
            }
            Ok(_) => {}
            Err(reason) => {
                return self
                    .insert_unavailable_record(record, RuntimeState::Conflict, reason)
                    .await;
            }
        }
        terminalize_running_subagents(&mut record.info.subagents, current_time_millis());
        import_terminal_native_reference(
            &mut record,
            snapshot,
            reported_hook_schema(snapshot.hook_schema.as_deref()),
        );
        if let Some(exit) = &snapshot.exit {
            record.info.exit_code = exit.code;
            record.info.state = if exit.stopped_by_user {
                SessionState::Stopped
            } else if exit.code == Some(0) && exit.signal.is_none() {
                SessionState::Done
            } else {
                SessionState::Failed
            };
            record.info.state_source = StateSource::Process;
        }
        self.insert_unavailable_record(record, state, "worker_runtime_terminal")
            .await
    }

    /// Connects every worker socket under the runtime root.
    ///
    /// Returns why the runtime root could not be enumerated instead of an
    /// empty result: a failed enumeration is no proof that a socket is absent.
    /// Each socket is bounded by the connect deadline; one that does not
    /// answer in time is reported as unresponsive rather than absent.
    async fn discover_workers(&self, records: &[SessionRecord]) -> Result<WorkerDiscovery, String> {
        let Some(runtime_root) = self.inner.config.worker_runtime_root.clone() else {
            return Ok(WorkerDiscovery::default());
        };
        let slots = match tokio::task::spawn_blocking(move || discover_runtime_slots(&runtime_root))
            .await
        {
            Ok(Ok(slots)) => slots,
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "failed to enumerate durable worker runtime root");
                return Err(format!("worker runtime root cannot be enumerated: {error}"));
            }
            Err(error) => {
                tracing::warn!(error = %error, "durable worker discovery task panicked");
                return Err(format!("worker runtime root discovery failed: {error}"));
            }
        };

        let mut candidates = Vec::new();
        let mut inventory = Vec::new();
        let mut unresponsive = HashMap::new();
        for (slot, socket) in slots {
            match self.connect_and_inspect(&socket).await {
                Ok((worker, snapshot)) => candidates.push(DiscoveredWorker {
                    slot,
                    worker,
                    snapshot,
                }),
                Err(ProbeFailure::Failed(error)) => {
                    // A refused or vanished endpoint is no identity evidence:
                    // supervisor and journal proof decide the session, so no
                    // contradictory inventory entry exists. Other failures
                    // (unreadable or unowned path, real protocol/identity
                    // mismatch) still quarantine.
                    if no_listener_on_socket(&error) {
                        tracing::debug!(slot = %slot, error = %error, "no durable worker listens");
                    } else {
                        inventory.push(discovery_failure_entry(slot, &error));
                    }
                }
                // Nothing proves who answers there, so the slot is not
                // inventoried; the session it names stays pending.
                Err(ProbeFailure::Unresponsive(detail)) => {
                    unresponsive.insert(slot, detail);
                }
            }
        }

        let logical: HashMap<&str, &SessionRecord> = records
            .iter()
            .map(|record| (record.session_id.as_str(), record))
            .collect();
        let mut claim_counts = HashMap::<String, usize>::new();
        for candidate in &candidates {
            *claim_counts
                .entry(candidate.snapshot.session_id.to_string())
                .or_default() += 1;
        }

        let mut grouped = HashMap::<String, Vec<DiscoveredWorker>>::new();
        for candidate in candidates {
            let claimed = candidate.snapshot.session_id.to_string();
            let record = logical.get(claimed.as_str()).copied();
            let status = if candidate.slot != claimed {
                RuntimeInventoryStatus::IdentityMismatch
            } else if claim_counts.get(&claimed).copied().unwrap_or_default() > 1 {
                RuntimeInventoryStatus::Conflict
            } else if let Some(record) = record {
                if persisted_identity_mismatch(record, &candidate.snapshot) {
                    RuntimeInventoryStatus::IdentityMismatch
                } else {
                    RuntimeInventoryStatus::Managed
                }
            } else {
                RuntimeInventoryStatus::Orphaned
            };
            let reason = match status {
                RuntimeInventoryStatus::Managed => None,
                RuntimeInventoryStatus::Orphaned => Some("logical_session_missing".to_owned()),
                RuntimeInventoryStatus::Conflict => Some("multiple_worker_candidates".to_owned()),
                RuntimeInventoryStatus::Incompatible => {
                    Some("worker_protocol_incompatible".to_owned())
                }
                RuntimeInventoryStatus::IdentityMismatch => {
                    Some("runtime_identity_mismatch".to_owned())
                }
            };
            inventory.push(RuntimeInventoryEntry {
                runtime_slot: candidate.slot.clone(),
                claimed_session_id: Some(claimed.clone()),
                worker_id: Some(candidate.snapshot.worker_id.to_string()),
                worker_instance_id: candidate
                    .snapshot
                    .worker_instance_id
                    .as_ref()
                    .map(ToString::to_string),
                status,
                reason,
            });
            if status == RuntimeInventoryStatus::Managed {
                grouped.entry(claimed).or_default().push(candidate);
            }
        }
        inventory.sort_by(|left, right| left.runtime_slot.cmp(&right.runtime_slot));
        Ok(WorkerDiscovery {
            workers: grouped,
            inventory,
            unresponsive,
        })
    }

    /// Connects a discovered worker socket and inspects its worker.
    ///
    /// Negotiation, controller acquisition (including waiting out a previous
    /// daemon's lease), and the inspection share one absolute deadline of
    /// `worker_connect_deadline`, so a socket that accepts and then stops
    /// answering cannot stall reconciliation or the lifecycle lock its caller
    /// holds.
    async fn connect_and_inspect(
        &self,
        socket_path: &Path,
    ) -> Result<(Worker, InspectSnapshot), ProbeFailure> {
        let deadline = tokio::time::Instant::now() + self.inner.config.worker_connect_deadline;
        let worker = self
            .connect_discovered_worker(socket_path, deadline)
            .await?;
        match tokio::time::timeout_at(deadline, worker.inspect()).await {
            Ok(Ok(snapshot)) => Ok((worker, snapshot)),
            Ok(Err(error)) => Err(ProbeFailure::Failed(error)),
            Err(_elapsed) => Err(self.unresponsive(socket_path)),
        }
    }

    /// Connects a discovered worker socket before `deadline`.
    ///
    /// A `ControllerBusy` rejection is retried until `deadline`; when the
    /// deadline cuts a retry short, that rejection is the result.
    async fn connect_discovered_worker(
        &self,
        socket_path: &Path,
        deadline: tokio::time::Instant,
    ) -> Result<Worker, ProbeFailure> {
        let mut busy = None;
        loop {
            let attempt = tokio::time::timeout_at(
                deadline,
                Worker::connect_discovered(socket_path, self.daemon_instance_id()),
            )
            .await;
            match attempt {
                Ok(Ok(worker)) => return Ok(worker),
                Ok(Err(
                    error @ WorkerError::Rejected {
                        code: ControlCode::ControllerBusy,
                        ..
                    },
                )) => {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        return Err(ProbeFailure::Failed(error));
                    }
                    tracing::debug!(
                        path = %socket_path.display(),
                        error = %error,
                        "waiting for the previous daemon discovery lease to close"
                    );
                    busy = Some(error);
                    tokio::time::sleep(WORKER_CONNECT_RETRY.min(deadline - now)).await;
                }
                Ok(Err(error)) => return Err(ProbeFailure::Failed(error)),
                Err(_elapsed) => {
                    return Err(
                        busy.map_or_else(|| self.unresponsive(socket_path), ProbeFailure::Failed)
                    );
                }
            }
        }
    }

    /// Reports a worker socket that did not answer within the deadline.
    fn unresponsive(&self, socket_path: &Path) -> ProbeFailure {
        let deadline = self.inner.config.worker_connect_deadline;
        tracing::warn!(
            path = %socket_path.display(),
            deadline_ms = deadline.as_millis(),
            "durable worker socket did not answer before the connect deadline; nothing is touched"
        );
        ProbeFailure::Unresponsive(format!(
            "worker socket did not answer within {}ms",
            deadline.as_millis()
        ))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "worker adoption reconstructs one complete observer-backed session entry"
    )]
    /// Adopts the live `worker` as the session's runtime.
    ///
    /// Returns whether the session needs the background re-check: a worker
    /// that answers but cannot be adopted leaves the session `Conflict`, and
    /// the re-check classifies it from fresh evidence once the worker or its
    /// job is gone.
    async fn adopt_live_record(
        &self,
        id: SessionId,
        worker: Worker,
        mut record: SessionRecord,
        snapshot: pohunek_worker_protocol::InspectSnapshot,
        retry: bool,
    ) -> bool {
        let snapshot = InspectSnapshot {
            hook_schema: self.effective_hook_schema_id(&record, &snapshot),
            ..snapshot
        };
        let native_recovery = record.transaction.as_ref().and_then(|transaction| {
            (transaction.kind == crate::store::TransactionKind::Recover)
                .then(|| transaction.previous_worker_instance_id.clone())
        });
        let Some(child) = snapshot.child_process else {
            return !self
                .insert_unavailable_record(record, RuntimeState::Lost, "worker_child_missing")
                .await;
        };
        let job = match super::Generation::from_record(&record.session_id, &record.runtime) {
            Ok(job) => job,
            Err(error) => {
                return !self
                    .insert_unavailable_record(record, RuntimeState::Conflict, error.code.as_str())
                    .await;
            }
        };
        let mut retry_identity = false;
        let mut root_missing_during_drain = false;
        if snapshot.launch_identity.is_some() || snapshot.active_identity.is_some() {
            if let Err(failure) =
                validate_worker_identity_processes(&*self.inner.inspector, &snapshot)
            {
                let (reason, retryable) = failure.reason_and_retryability();
                if failure.is_missing_root() {
                    tracing::debug!(
                        session_id = %id.0,
                        reason,
                        "preserving durable identity while an adopted worker drains its PTY"
                    );
                    root_missing_during_drain = true;
                } else if retryable {
                    tracing::debug!(
                        session_id = %id.0,
                        reason,
                        "deferring startup worker identity validation"
                    );
                    retry_identity = true;
                } else {
                    self.mark_pending(record, RuntimeState::Conflict, reason, retry)
                        .await;
                    return true;
                }
            }
        }
        import_native_reference(&mut record, &snapshot);
        let identity_projection = if root_missing_during_drain || retry_identity {
            clear_active_identity(&mut record.info);
            WorkerIdentityProjection::unreported()
        } else {
            match import_worker_identities(&mut record, &snapshot) {
                Ok(projection) => projection,
                Err(reason) => {
                    self.mark_pending(record, RuntimeState::Conflict, reason, retry)
                        .await;
                    return true;
                }
            }
        };
        record.info.subagents = match import_worker_subagents(&snapshot) {
            Ok(subagents) => subagents,
            Err(reason) => {
                self.mark_pending(record, RuntimeState::Conflict, reason, retry)
                    .await;
                return true;
            }
        };
        let active_agent = identity_projection
            .active
            .or_else(|| active_report_from_info(&record.info));
        let detector_output = match open_detector_output(&worker, &id).await {
            Ok(output) => output,
            Err(error) => {
                return !self
                    .insert_unavailable_record(
                        record,
                        RuntimeState::Reconnecting,
                        error.code.as_str(),
                    )
                    .await;
            }
        };
        let now = timestamp_now();
        let worker_instance_id = snapshot
            .worker_instance_id
            .as_ref()
            .map(ToString::to_string);
        let runtime_generation = record
            .info
            .runtime
            .as_ref()
            .map_or(protocol::RuntimeGeneration::new(1), |runtime| {
                runtime.runtime_generation
            });
        record.info.pid = child.pid;
        if let Some(dimensions) = snapshot.dimensions {
            record.info.cols = dimensions.columns();
            record.info.rows = dimensions.rows();
        }
        record.info.state = SessionState::Running;
        record.info.state_source = StateSource::Process;
        record.info.runtime = Some(SessionRuntime {
            state: RuntimeState::Live,
            runtime_generation,
            worker_id: Some(snapshot.worker_id.to_string()),
            worker_instance_id: worker_instance_id.clone(),
            started_at: record
                .info
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.started_at.clone())
                .or_else(|| Some(record.info.created_at.clone())),
            last_connected_at: Some(now.clone()),
            loss_reason: None,
        });
        record.info.updated_at = now;
        record.transaction = None;
        record.runtime.state = RuntimeState::Live;
        record.runtime.worker_id = Some(snapshot.worker_id.to_string());
        record.runtime.worker_instance_id = worker_instance_id;
        record.runtime.reason = None;

        let recovery = record.recovery.clone();
        let input_rules = recovery.as_ref().map_or_else(
            || {
                super::input_rules_for_agent(
                    self.inner.profiles.runtimes(),
                    &record.info.agent_base,
                    &self.inner.config,
                )
            },
            |binding| {
                crate::agent::recovered_input_rules(
                    self.inner.profiles.runtimes(),
                    &binding.agent_base,
                    &binding.launch_binding,
                    binding.input_rules,
                )
            },
        );
        let snapshot = recovery
            .as_ref()
            .map_or_else(ResumeSnapshot::empty, ResumeSnapshot::from_binding);
        let pin = recovery
            .as_ref()
            .map(|binding| binding.launch_binding.clone())
            .unwrap_or_default();
        let manifest_override = match self
            .inner
            .profiles
            .runtimes()
            .definition_for_pin(&record.info.agent_base, &pin)
        {
            Ok(pinned) => self
                .inner
                .profiles
                .resolve_agent_pinned(&record.info.agent, &pinned),
            Err(_unresolved) => self.inner.profiles.resolve_agent(&record.info.agent),
        }
        .ok()
        .and_then(|resolved| resolved.profile.and_then(|profile| profile.manifest));
        let default_detector_config = DetectorConfig::for_pinned(
            self.inner.profiles.runtimes(),
            &record.info.agent_base,
            &pin,
            manifest_override,
        );
        let detector_cancel = CancellationToken::new();
        let procwatch_cancel = CancellationToken::new();
        let runtime_watch_cancel = CancellationToken::new();
        let procwatch_rescan = Arc::new(Notify::new());
        let (detector_resize, detector_resize_rx) =
            watch::channel((record.info.rows, record.info.cols));
        let (detector_config, detector_config_rx) = watch::channel(DetectorConfigUpdate {
            generation: 0,
            config: default_detector_config.clone(),
        });
        let (detector_preview, detector_preview_rx) = mpsc::channel(1);
        let (input_ready_tx, input_ready) = watch::channel(false);
        let info = record.info.clone();
        let entry = SessionEntry {
            info: info.clone(),
            activity_revision: 0,
            activity_evidence: VecDeque::new(),
            input_gate: Arc::new(Mutex::new(())),
            runtime: RuntimeHandle::Worker(worker.clone()),
            job,
            desired_state: DesiredState::Running,
            detector_cancel: detector_cancel.clone(),
            detector_resize,
            detector_config,
            detector_preview,
            input_ready,
            default_detector_config,
            pinned: self
                .inner
                .profiles
                .runtimes()
                .pinned_definition(&record.info.agent_base, &pin),
            procwatch_cancel: procwatch_cancel.clone(),
            runtime_watch_cancel: runtime_watch_cancel.clone(),
            procwatch_rescan: Arc::clone(&procwatch_rescan),
            stopping: false,
            stop_transaction_id: None,
            input_rules,
            snapshot,
            active_agent: active_agent.clone(),
            foreground_process_group: None,
            last_agent_report: active_agent,
            last_native_report: record.native_identity_ordering.clone(),
            observed_agents: Vec::<ObservedAgent>::new(),
            cwd_observed_at: crate::time::now(),
            initial_input_owner: initial_input_owner(&record),
        };
        let listed = record.clone();
        if let Err(error) = self.write_session_record(record).await {
            tracing::warn!(session_id = %id.0, error = %error, "failed to commit reconciled worker");
            Box::pin(self.list_pending(&listed)).await;
            return true;
        }
        self.install_session_entry(&id, entry).await;
        let expected = RuntimeWatchIdentity::from_info(&info)
            .expect("reconciled live runtime has a complete watcher identity");
        self.spawn_detector(DetectorInputs {
            scope: DetectorScope {
                id: id.clone(),
                runtime: expected.clone(),
            },
            output: detector_output,
            initial_size: (info.rows, info.cols),
            cancel: detector_cancel,
            resize: detector_resize_rx,
            config: detector_config_rx,
            preview: detector_preview_rx,
            input_ready: input_ready_tx,
        });
        if !root_missing_during_drain {
            self.spawn_procwatch(
                id.clone(),
                expected.clone(),
                crate::procwatch::ProcessIdentity {
                    pid: child.pid,
                    start_identity: crate::procwatch::StartIdentity::new(child.start_identity),
                },
                procwatch_cancel,
                procwatch_rescan,
            );
        }
        self.spawn_worker_exit_watcher(id, worker, expected, runtime_watch_cancel);
        if let Some(previous_worker_instance_id) = native_recovery {
            self.emit_native_recovered(&info, previous_worker_instance_id);
        } else {
            self.emit(event::SESSION_RUNTIME_RECONNECTED, &info);
        }
        false
    }

    /// Persists `record` as classified `state` for `reason`, shows it, and
    /// announces it.
    ///
    /// Logs the classification's one WARN ([`warn_unavailable`]) once it is
    /// applied.
    ///
    /// Returns whether the classification was committed. A failed write
    /// changes nothing the registry shows, except that a session without an
    /// entry is listed as `runtime_supervision_unavailable` in memory, so it
    /// is never absent from the registry; the caller keeps the session
    /// pending for the re-check.
    #[must_use = "an uncommitted classification must keep the session pending"]
    pub(super) async fn insert_unavailable_record(
        &self,
        record: SessionRecord,
        state: RuntimeState,
        reason: &str,
    ) -> bool {
        let id = SessionId(record.session_id.clone());
        let (classified, entry) = self.unavailable_entry(record.clone(), state, reason);
        let info = entry.info.clone();
        if let Err(error) = self.write_session_record(classified).await {
            tracing::warn!(session_id = %id.0, error = %error, "failed to persist runtime classification");
            Box::pin(self.list_pending(&record)).await;
            return false;
        }
        // A placeholder that already logged this very classification does not
        // log it again once the write succeeds.
        let logged = self
            .inner
            .unpersisted_classifications
            .lock()
            .expect("unpersisted classifications are never poisoned")
            .get(&id.0)
            .is_some_and(|(shown, shown_reason)| *shown == state && shown_reason == reason);
        self.install_session_entry(&id, entry).await;
        if !logged {
            warn_unavailable(&id.0, info_worker_id(&info), state, reason, "");
        }
        let event_name = match state {
            RuntimeState::Conflict => event::SESSION_RUNTIME_CONFLICT,
            RuntimeState::Lost | RuntimeState::Incompatible => event::SESSION_RUNTIME_LOST,
            RuntimeState::Starting
            | RuntimeState::Live
            | RuntimeState::Reconnecting
            | RuntimeState::Terminal => event::SESSION_UPDATED,
        };
        self.emit(event_name, &info);
        true
    }

    /// Lists a session without an entry as `runtime_supervision_unavailable`
    /// in memory, so a classification that could not be committed never
    /// leaves it absent from the registry and the re-check finds it.
    async fn list_pending(&self, record: &SessionRecord) {
        let id = SessionId(record.session_id.clone());
        if self.inner.sessions.lock().await.contains_key(&id) {
            return;
        }
        let (_, placeholder) = self.unavailable_entry(
            record.clone(),
            RuntimeState::Reconnecting,
            SUPERVISION_UNAVAILABLE,
        );
        let info = placeholder.info.clone();
        let shown = (
            RuntimeState::Reconnecting,
            SUPERVISION_UNAVAILABLE.to_owned(),
        );
        if !self
            .install_placeholder_entry(&id, placeholder, shown)
            .await
        {
            return;
        }
        warn_unavailable(
            &id.0,
            info_worker_id(&info),
            RuntimeState::Reconnecting,
            SUPERVISION_UNAVAILABLE,
            "",
        );
    }

    /// Whether the classification of `id` is shown only by a placeholder.
    fn classification_unpersisted(&self, id: &str) -> bool {
        self.inner
            .unpersisted_classifications
            .lock()
            .expect("unpersisted classifications are never poisoned")
            .contains_key(id)
    }

    /// Classifies `record` as `state` for `reason` and builds the in-memory
    /// entry that shows it, returning the classified record with it.
    fn unavailable_entry(
        &self,
        mut record: SessionRecord,
        state: RuntimeState,
        reason: &str,
    ) -> (SessionRecord, SessionEntry) {
        let now = timestamp_now();
        let runtime = record.info.runtime.get_or_insert(SessionRuntime {
            state,
            runtime_generation: protocol::RuntimeGeneration::new(1),
            worker_id: record.runtime.worker_id.clone(),
            worker_instance_id: record.runtime.worker_instance_id.clone(),
            started_at: None,
            last_connected_at: None,
            loss_reason: Some(reason.to_owned()),
        });
        runtime.state = state;
        runtime.loss_reason = (state != RuntimeState::Terminal).then(|| reason.to_owned());
        record.runtime.state = state;
        record.runtime.reason.clone_from(&runtime.loss_reason);
        if matches!(state, RuntimeState::Lost | RuntimeState::Terminal) {
            terminalize_running_subagents(&mut record.info.subagents, current_time_millis());
        }
        record.info.updated_at = now;
        let recovery = record.recovery.clone();
        let input_rules = recovery.as_ref().map_or_else(
            || {
                super::input_rules_for_agent(
                    self.inner.profiles.runtimes(),
                    &record.info.agent_base,
                    &self.inner.config,
                )
            },
            |binding| {
                crate::agent::recovered_input_rules(
                    self.inner.profiles.runtimes(),
                    &binding.agent_base,
                    &binding.launch_binding,
                    binding.input_rules,
                )
            },
        );
        let relaunch = recovery
            .as_ref()
            .map_or_else(ResumeSnapshot::empty, ResumeSnapshot::from_binding);
        let pin = recovery
            .as_ref()
            .map(|binding| binding.launch_binding.clone())
            .unwrap_or_default();
        let default_detector_config = DetectorConfig::for_pinned(
            self.inner.profiles.runtimes(),
            &record.info.agent_base,
            &pin,
            None,
        );
        let (detector_resize, _) = watch::channel((record.info.rows, record.info.cols));
        let (detector_config, _) = watch::channel(DetectorConfigUpdate {
            generation: 0,
            config: default_detector_config.clone(),
        });
        let (detector_preview, _) = mpsc::channel::<super::DetectionPreviewRequest>(1);
        let (_, input_ready) = watch::channel(false);
        let info = record.info.clone();
        let entry = SessionEntry {
            info: info.clone(),
            activity_revision: 0,
            activity_evidence: VecDeque::new(),
            input_gate: Arc::new(Mutex::new(())),
            runtime: RuntimeHandle::Unavailable(state),
            // A malformed job reference is not adoptable evidence; the runtime is
            // already classified unavailable, so the entry keeps no job.
            job: super::Generation::from_record(&record.session_id, &record.runtime)
                .ok()
                .flatten(),
            desired_state: record.desired_state,
            detector_cancel: CancellationToken::new(),
            detector_resize,
            detector_config,
            detector_preview,
            input_ready,
            default_detector_config,
            pinned: self
                .inner
                .profiles
                .runtimes()
                .pinned_definition(&record.info.agent_base, &pin),
            procwatch_cancel: CancellationToken::new(),
            runtime_watch_cancel: CancellationToken::new(),
            procwatch_rescan: Arc::new(Notify::new()),
            stopping: false,
            stop_transaction_id: None,
            input_rules,
            snapshot: relaunch,
            active_agent: None,
            foreground_process_group: None,
            last_agent_report: None,
            last_native_report: record.native_identity_ordering.clone(),
            observed_agents: Vec::new(),
            cwd_observed_at: crate::time::now(),
            initial_input_owner: initial_input_owner(&record),
        };
        (record, entry)
    }

    /// Replays the durable stop intent of `record` through its answering
    /// worker.
    ///
    /// Only a stop that returns the runtime's terminal outcome proves the PTY
    /// ended; the record is then committed `stopped`. A stop without an
    /// outcome or a failed stop keeps the session
    /// `runtime_supervision_unavailable` with its intent and returns `true`
    /// for the background re-check, which reconnects the worker and replays
    /// the stop again.
    async fn finish_reconciled_stop(
        &self,
        id: &SessionId,
        worker: Worker,
        record: SessionRecord,
        retry: bool,
    ) -> bool {
        let transaction_id = record
            .transaction
            .as_ref()
            .map_or_else(|| format!("stop-reconcile-{}", id.0), |tx| tx.id.clone());
        let transaction = match pohunek_worker_protocol::TransactionId::new(transaction_id) {
            Ok(transaction) => transaction,
            Err(error) => {
                return !self
                    .insert_unavailable_record(
                        record,
                        RuntimeState::Conflict,
                        &format!("invalid_stop_transaction:{error}"),
                    )
                    .await;
            }
        };
        match worker.stop(transaction).await {
            Ok(Some(exit)) => {
                let mut terminal = record;
                if let Ok(final_snapshot) = worker.inspect().await {
                    let snapshot = InspectSnapshot {
                        hook_schema: self.effective_hook_schema_id(&terminal, &final_snapshot),
                        ..final_snapshot
                    };
                    import_terminal_native_reference(
                        &mut terminal,
                        &snapshot,
                        reported_hook_schema(snapshot.hook_schema.as_deref()),
                    );
                }
                terminal.transaction = None;
                terminal.info.state = SessionState::Stopped;
                terminal.info.exit_code = exit.code;
                terminal
                    .info
                    .runtime
                    .get_or_insert(SessionRuntime {
                        state: RuntimeState::Terminal,
                        runtime_generation: protocol::RuntimeGeneration::new(1),
                        worker_id: terminal.runtime.worker_id.clone(),
                        worker_instance_id: terminal.runtime.worker_instance_id.clone(),
                        started_at: None,
                        last_connected_at: None,
                        loss_reason: None,
                    })
                    .state = RuntimeState::Terminal;
                !self
                    .insert_unavailable_record(
                        terminal,
                        RuntimeState::Terminal,
                        "worker_runtime_terminal",
                    )
                    .await
            }
            Ok(None) => {
                tracing::warn!(session_id = %id.0, "worker returned no terminal outcome for the replayed stop");
                self.mark_pending(
                    record,
                    RuntimeState::Reconnecting,
                    SUPERVISION_UNAVAILABLE,
                    retry,
                )
                .await;
                true
            }
            Err(error) => {
                tracing::warn!(session_id = %id.0, error = %error, "failed to replay the durable stop intent");
                self.mark_pending(
                    record,
                    RuntimeState::Reconnecting,
                    SUPERVISION_UNAVAILABLE,
                    retry,
                )
                .await;
                true
            }
        }
    }

    /// Replays the durable removal intent of `record` through its answering
    /// worker.
    ///
    /// Only a stop that returns the runtime's terminal outcome proves the PTY
    /// ended; the removal is then finished by [`Self::finish_removal_intent`],
    /// which retires the exact generation (the worker otherwise retains its
    /// final output) before the record is deleted. A stop without an outcome
    /// or a failed stop keeps the session `runtime_supervision_unavailable`
    /// and returns `true` for the background re-check, which reconnects the
    /// worker and replays the intent again.
    async fn finish_reconciled_removal(
        &self,
        worker: Worker,
        record: SessionRecord,
        retry: bool,
    ) -> bool {
        let id = SessionId(record.session_id.clone());
        let transaction_id = record
            .transaction
            .as_ref()
            .map_or_else(|| format!("remove-reconcile-{}", id.0), |tx| tx.id.clone());
        let transaction = match pohunek_worker_protocol::TransactionId::new(transaction_id) {
            Ok(transaction) => transaction,
            Err(error) => {
                return !self
                    .insert_unavailable_record(
                        record,
                        RuntimeState::Conflict,
                        &format!("invalid_remove_transaction:{error}"),
                    )
                    .await;
            }
        };
        match worker.stop(transaction).await {
            Ok(Some(_exit)) => {}
            Ok(None) => {
                tracing::warn!(session_id = %id.0, "worker returned no terminal outcome for the replayed removal");
                self.mark_pending(
                    record,
                    RuntimeState::Reconnecting,
                    SUPERVISION_UNAVAILABLE,
                    retry,
                )
                .await;
                return true;
            }
            Err(error) => {
                tracing::warn!(session_id = %id.0, error = %error, "failed to stop the worker of a replayed removal");
                self.mark_pending(
                    record,
                    RuntimeState::Reconnecting,
                    SUPERVISION_UNAVAILABLE,
                    retry,
                )
                .await;
                return true;
            }
        }
        // `reconcile_logical` already rejected a malformed generation.
        let generation = super::Generation::from_record(&record.session_id, &record.runtime)
            .ok()
            .flatten();
        let lifecycle = self.lifecycle().ok();
        Box::pin(self.finish_removal_intent(record, generation.as_ref(), lifecycle.as_ref(), retry))
            .await
    }
}

/// Outcome of converting an orphaned undelivered create
/// ([`SessionRegistry::roll_back_undelivered_create`]).
enum UndeliveredCreate {
    /// Reconcile this record: it is not orphaned, or its removal intent is
    /// durable.
    Proceed(SessionRecord),
    /// The intent was not persisted: nothing may be stopped or cleaned, and
    /// the session stays pending for the retry.
    Pending(SessionRecord),
    /// The record no longer exists.
    Gone(SessionId),
}

/// The daemon instance an `initial_input` marker of `record` names, which
/// keeps holding the undelivered input while it runs.
/// Whether `record` is a running create whose `initial_input` marker was not
/// written by the daemon instance `daemon_instance_id`.
fn initial_input_orphaned(record: &SessionRecord, daemon_instance_id: &str) -> bool {
    record.desired_state == DesiredState::Running
        && record
            .transaction
            .as_ref()
            .is_some_and(crate::store::SessionTransaction::is_initial_input)
        && initial_input_owner(record).as_deref() != Some(daemon_instance_id)
}

fn initial_input_owner(record: &SessionRecord) -> Option<String> {
    record
        .transaction
        .as_ref()
        .filter(|transaction| transaction.is_initial_input())
        .and_then(|transaction| transaction.daemon_instance_id.clone())
}

/// Whether `record` is a create that has not committed its session yet.
///
/// A committed create still delivering its initial input is a running
/// session, not an uncommitted create, so no create compensation applies.
fn uncommitted_create(record: &SessionRecord) -> bool {
    record.transaction.as_ref().is_some_and(|transaction| {
        transaction.kind == crate::store::TransactionKind::Create && !transaction.is_initial_input()
    })
}

/// Resolves the hook schema id a worker reported, if any, against the compiled
/// registry.
fn reported_hook_schema(
    id: Option<&str>,
) -> Result<Option<&'static pohunek_worker_protocol::HookSchema>, &'static str> {
    id.map(|id| pohunek_worker_protocol::hook_schema(id).ok_or("hook_schema_unknown"))
        .transpose()
}

pub(super) fn import_worker_subagents(
    snapshot: &InspectSnapshot,
) -> Result<Vec<SubagentInfo>, &'static str> {
    map_worker_subagents(
        &snapshot.subagents,
        reported_hook_schema(snapshot.hook_schema.as_deref())?,
    )
}

/// Maps journaled subagents to public state, admitting only the providers,
/// claim fields and outcomes the session's hook schema lets report subagents.
///
/// A schema that declares no subagent sequence rule admits no subagent record.
fn map_worker_subagents(
    subagents: &[pohunek_worker_protocol::SubagentSnapshot],
    schema: Option<&pohunek_worker_protocol::HookSchema>,
) -> Result<Vec<SubagentInfo>, &'static str> {
    let mut mapped = subagents
        .iter()
        .map(|subagent| {
            let schema = schema
                .filter(|schema| schema.admits_subagent_provider(&subagent.provider))
                .ok_or("subagent_provider_invalid")?;
            if !schema.admits_subagents() {
                return Err("subagent_sequence_rule_missing");
            }
            if (subagent.parent_id.is_some()
                && !schema.admits_subagent_field(SubagentField::ParentId))
                || (subagent.agent_type.is_some()
                    && !schema.admits_subagent_field(SubagentField::AgentType))
                || (matches!(
                    subagent.phase,
                    pohunek_worker_protocol::SubagentPhase::Failed
                        | pohunek_worker_protocol::SubagentPhase::Cancelled
                ) && !schema.admits_subagent_field(SubagentField::Outcome))
            {
                return Err("subagent_field_invalid");
            }
            let provider = RuntimeRef::from_wire(&subagent.provider);
            let lifecycle = match subagent.phase {
                pohunek_worker_protocol::SubagentPhase::Running => SubagentLifecycle::Running,
                pohunek_worker_protocol::SubagentPhase::Completed => SubagentLifecycle::Completed,
                pohunek_worker_protocol::SubagentPhase::Failed => SubagentLifecycle::Failed,
                pohunek_worker_protocol::SubagentPhase::Cancelled => SubagentLifecycle::Cancelled,
                pohunek_worker_protocol::SubagentPhase::Lost => SubagentLifecycle::Lost,
            };
            Ok(SubagentInfo {
                id: subagent.id.clone(),
                parent_id: subagent.parent_id.clone(),
                provider,
                agent_type: subagent.agent_type.clone(),
                lifecycle,
                activity: (lifecycle == SubagentLifecycle::Running)
                    .then_some(AgentActivity::Working),
                revision: SubagentRevision::new(subagent.revision),
                started_at_ms: subagent.started_at_ms,
                updated_at_ms: subagent.updated_at_ms,
                finished_at_ms: subagent.finished_at_ms,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    sort_subagents(&mut mapped);
    Ok(mapped)
}

/// Whether `record` carries a stop intent no outcome committed yet.
fn is_pending_stop(record: &SessionRecord) -> bool {
    record.desired_state == DesiredState::Stopped
        && record
            .transaction
            .as_ref()
            .is_some_and(|transaction| transaction.kind == crate::store::TransactionKind::Stop)
}

pub(super) fn merge_persisted_recovery(
    record: &mut SessionRecord,
    mut binding: crate::store::ResumeBinding,
) -> Result<(), &'static str> {
    if binding.agent != record.info.agent || binding.agent_base != record.info.agent_base {
        return Err("resume_binding_agent_mismatch");
    }
    if let Some(recovery) = record.recovery.as_ref() {
        if recovery.agent != binding.agent
            || recovery.agent_base != binding.agent_base
            || recovery.native_launch != binding.native_launch
        {
            return Err("resume_binding_shape_mismatch");
        }
    }

    // A sequenced native identity in the session record is authoritative over
    // the separately persisted resume projection. The session write commits
    // first, so a crash or I/O failure can legitimately leave the resume
    // projection one report behind. Every replacement of the reference carries
    // an ordering key, so a keyed record is the newer side.
    if record.native_identity_ordering.is_some() {
        if let Some(recovery) = record.recovery.as_ref().filter(|recovery| {
            recovery.native_session_id.is_some() || recovery.native_session_path.is_some()
        }) {
            binding
                .native_session_id
                .clone_from(&recovery.native_session_id);
            binding
                .native_session_path
                .clone_from(&recovery.native_session_path);
            binding.native_reference_provenance = recovery.native_reference_provenance;
        }
    }

    let persisted_kind = binding.reference_kind();
    let record_kind = record
        .recovery
        .as_ref()
        .and_then(ResumeBinding::reference_kind);
    let kind = persisted_kind.or(record_kind);
    let (native_id, native_path) = match (
        kind,
        binding.native_session_id.as_deref(),
        binding.native_session_path.as_deref(),
    ) {
        (Some(SessionRefKind::Id) | None, Some(native), None) => (
            Some(
                validate_native_reference(SessionRefKind::Id, native)
                    .ok_or("resume_binding_reference_invalid")?,
            ),
            None,
        ),
        (Some(SessionRefKind::Path) | None, None, Some(native)) => (
            None,
            Some(
                validate_native_reference(SessionRefKind::Path, native)
                    .ok_or("resume_binding_reference_invalid")?,
            ),
        ),
        (_, None, None) => return Ok(()),
        _ => return Err("resume_binding_reference_kind_mismatch"),
    };

    if record
        .info
        .native_session_id
        .as_ref()
        .is_some_and(|existing| Some(existing) != native_id.as_ref())
        || record
            .info
            .native_session_path
            .as_ref()
            .is_some_and(|existing| Some(existing) != native_path.as_ref())
    {
        return Err("resume_binding_reference_mismatch");
    }
    if let Some(recovery) = record.recovery.as_ref() {
        if recovery
            .native_session_id
            .as_ref()
            .is_some_and(|existing| Some(existing) != native_id.as_ref())
            || recovery
                .native_session_path
                .as_ref()
                .is_some_and(|existing| Some(existing) != native_path.as_ref())
        {
            return Err("resume_binding_reference_mismatch");
        }
    }

    record.info.native_session_id.clone_from(&native_id);
    record.info.native_session_path.clone_from(&native_path);
    let recovery = record.recovery.get_or_insert(binding);
    recovery.native_session_id = native_id;
    recovery.native_session_path = native_path;
    Ok(())
}

fn import_worker_identities(
    record: &mut SessionRecord,
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
) -> Result<WorkerIdentityProjection, &'static str> {
    let mut candidate = record.clone();
    let projection = apply_worker_identities(&mut candidate, snapshot)?;
    *record = candidate;
    Ok(projection)
}

fn apply_identity_projection(
    entry: &mut SessionEntry,
    candidate: &SessionRecord,
    projection: &WorkerIdentityProjection,
    snapshot: &InspectSnapshot,
) -> bool {
    let provenance = candidate
        .recovery
        .as_ref()
        .map(|binding| binding.native_reference_provenance);
    let provenance_changed =
        provenance.is_some_and(|provenance| provenance != entry.snapshot.reference_provenance);
    let native_changed = provenance_changed
        || entry.info.native_session_id != candidate.info.native_session_id
        || entry.info.native_session_path != candidate.info.native_session_path;
    let active_changed = projection.privately_reported
        && (entry.info.active_agent != candidate.info.active_agent
            || entry.info.active_agent_base != candidate.info.active_agent_base
            || entry.info.active_agent_pid != candidate.info.active_agent_pid
            || entry.info.active_agent_session_id != candidate.info.active_agent_session_id
            || entry.info.active_agent_session_path != candidate.info.active_agent_session_path);
    if !native_changed && !active_changed {
        return false;
    }
    entry
        .info
        .native_session_id
        .clone_from(&candidate.info.native_session_id);
    entry
        .info
        .native_session_path
        .clone_from(&candidate.info.native_session_path);
    if let Some(provenance) = provenance {
        entry.snapshot.reference_provenance = provenance;
    }
    if active_changed {
        entry
            .info
            .active_agent
            .clone_from(&candidate.info.active_agent);
        entry
            .info
            .active_agent_base
            .clone_from(&candidate.info.active_agent_base);
        entry.info.active_agent_pid = candidate.info.active_agent_pid;
        entry
            .info
            .active_agent_session_id
            .clone_from(&candidate.info.active_agent_session_id);
        entry
            .info
            .active_agent_session_path
            .clone_from(&candidate.info.active_agent_session_path);
        entry.active_agent.clone_from(&projection.active);
        entry.last_agent_report = projection.active.clone().or_else(|| {
            projection
                .release
                .as_ref()
                .map(|release| release_tombstone(snapshot, release))
        });
    }
    true
}

pub(super) fn worker_metadata_record_is_current(
    record: &SessionRecord,
    worker_id: &str,
    worker_instance_id: Option<&str>,
) -> bool {
    let Some(runtime) = record.info.runtime.as_ref() else {
        return false;
    };
    record.desired_state == DesiredState::Running
        && record.transaction.is_none()
        && record.info.state == SessionState::Running
        && runtime.state == RuntimeState::Live
        && runtime.worker_id.as_deref() == Some(worker_id)
        && runtime.worker_instance_id.as_deref() == worker_instance_id
        && record.runtime.state == RuntimeState::Live
        && record.runtime.worker_id.as_deref() == Some(worker_id)
        && record.runtime.worker_instance_id.as_deref() == worker_instance_id
}

fn metadata_write_failure_outcome(error: &ProtocolError) -> WorkerMetadataApplyOutcome {
    match error.code.as_str() {
        "session_runtime_commit_stale" => WorkerMetadataApplyOutcome::Discarded,
        _ => WorkerMetadataApplyOutcome::Retryable(super::WorkerMetadataRetryCause::Commit),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdentityValidationFailure {
    Retryable(&'static str),
    Permanent(&'static str),
}

impl IdentityValidationFailure {
    pub(super) fn reason_and_retryability(self) -> (&'static str, bool) {
        match self {
            Self::Retryable(reason) => (reason, true),
            Self::Permanent(reason) => (reason, false),
        }
    }

    fn is_missing_root(self) -> bool {
        self == Self::Permanent("identity_process_root_missing")
    }
}

/// The processes a worker's identity claims must find in its PTY tree.
#[derive(Debug, Clone, Copy)]
pub(super) struct IdentityClaims {
    /// The PTY root the worker reports.
    pub(super) root: Option<pohunek_worker_protocol::ProcessIdentity>,
    /// The process of the immutable launch identity.
    pub(super) launch: Option<pohunek_worker_protocol::ProcessIdentity>,
    /// The process of the live active identity.
    pub(super) active: Option<pohunek_worker_protocol::ProcessIdentity>,
    /// The matcher the active identity is held to: its schema's, or the core
    /// matcher, the strictest one, when the schema is absent or unknown (such
    /// a snapshot is refused when its identities are applied).
    pub(super) active_matcher: AncestryMatcher,
}

impl IdentityClaims {
    /// The claims a worker reports in its inspection snapshot.
    pub(super) fn from_snapshot(snapshot: &InspectSnapshot) -> Self {
        Self {
            root: snapshot.child_process,
            launch: snapshot.launch_identity.as_ref().map(|claim| claim.process),
            active: snapshot.active_identity.as_ref().map(|claim| claim.process),
            active_matcher: reported_hook_schema(snapshot.hook_schema.as_deref())
                .ok()
                .flatten()
                .map_or(AncestryMatcher::CORE, |schema| schema.ancestry),
        }
    }

    /// Whether any claim beyond the root needs validating.
    pub(super) fn has_claims(&self) -> bool {
        self.launch.is_some() || self.active.is_some()
    }
}

fn validate_worker_identity_processes(
    inspector: &dyn ProcessInspector,
    snapshot: &InspectSnapshot,
) -> Result<(), IdentityValidationFailure> {
    validate_identity_claims(inspector, &IdentityClaims::from_snapshot(snapshot))
}

/// Validates `claims` against the live process tree below the claimed root.
///
/// Startup adoption and the upgrade preflight both decide through this
/// function, so a claim is judged the same wherever its evidence came from.
pub(super) fn validate_identity_claims(
    inspector: &dyn ProcessInspector,
    claims: &IdentityClaims,
) -> Result<(), IdentityValidationFailure> {
    let root = claims
        .root
        .ok_or(IdentityValidationFailure::Permanent("worker_child_missing"))?;
    let expected_root = crate::procwatch::ProcessIdentity {
        pid: root.pid,
        start_identity: crate::procwatch::StartIdentity::new(root.start_identity),
    };
    if !inspector.is_running(expected_root).map_err(|_error| {
        IdentityValidationFailure::Retryable("identity_process_inspection_failed")
    })? {
        return Err(IdentityValidationFailure::Permanent(
            "identity_process_root_missing",
        ));
    }
    let current_root = inspector
        .process(root.pid)
        .map_err(|_error| {
            IdentityValidationFailure::Retryable("identity_process_inspection_failed")
        })?
        .ok_or(IdentityValidationFailure::Permanent(
            "identity_process_root_missing",
        ))?;
    let descendants = inspector.descendants(root.pid).map_err(|_error| {
        IdentityValidationFailure::Retryable("identity_process_inspection_failed")
    })?;
    claim_process_facts(claims, &current_root, &descendants)
        .map_err(IdentityValidationFailure::Permanent)
}

#[cfg(test)]
fn validate_worker_identity_process_facts(
    snapshot: &InspectSnapshot,
    current_root: &crate::procwatch::ProcessFact,
    descendants: &[crate::procwatch::ProcessFact],
) -> Result<(), &'static str> {
    claim_process_facts(
        &IdentityClaims::from_snapshot(snapshot),
        current_root,
        descendants,
    )
}

fn claim_process_facts(
    claims: &IdentityClaims,
    current_root: &crate::procwatch::ProcessFact,
    descendants: &[crate::procwatch::ProcessFact],
) -> Result<(), &'static str> {
    let root = claims.root.ok_or("worker_child_missing")?;
    if current_root.pid != root.pid || current_root.start_identity.get() != root.start_identity {
        return Err("identity_process_root_reused");
    }
    let relation = |process: &pohunek_worker_protocol::ProcessIdentity| {
        PtyRelation::from_in_tree(
            (process.pid == root.pid && process.start_identity == root.start_identity)
                || descendants.iter().any(|fact| {
                    fact.pid == process.pid && fact.start_identity.get() == process.start_identity
                }),
        )
    };
    // The launch identity is core-owned and always held to the core matcher.
    if claims
        .launch
        .as_ref()
        .is_some_and(|process| !AncestryMatcher::CORE.admits(relation(process)))
    {
        return Err("launch_identity_process_invalid");
    }
    if claims
        .active
        .as_ref()
        .is_some_and(|process| !claims.active_matcher.admits(relation(process)))
    {
        return Err("active_identity_process_invalid");
    }
    // A release is validated against the live PTY tree before the worker
    // commits it. It is durable history after that point: the released process
    // may legitimately be gone before the daemon observes or reloads it.
    Ok(())
}

fn active_report_from_info(info: &protocol::SessionInfo) -> Option<ActiveAgentReport> {
    Some(ActiveAgentReport {
        source: "persisted".to_owned(),
        agent: info.active_agent.clone()?,
        seq: None,
        pid: info.active_agent_pid,
        start_identity: None,
        reported_at: crate::time::now(),
        activity_reported: false,
    })
}

fn clear_active_identity(info: &mut protocol::SessionInfo) {
    info.active_agent = None;
    info.active_agent_base = None;
    info.active_agent_pid = None;
    info.active_agent_session_id = None;
    info.active_agent_session_path = None;
}

fn release_tombstone(
    snapshot: &InspectSnapshot,
    release: &ReleasedIdentityClaim,
) -> ActiveAgentReport {
    ActiveAgentReport {
        source: format!("worker:{}", snapshot.worker_id),
        agent: release.provider.clone(),
        seq: Some(release.sequence),
        pid: Some(release.process.pid),
        start_identity: Some(release.process.start_identity),
        reported_at: crate::time::now(),
        activity_reported: false,
    }
}

fn apply_worker_identities(
    record: &mut SessionRecord,
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
) -> Result<WorkerIdentityProjection, &'static str> {
    apply_worker_identities_with(
        record,
        snapshot,
        reported_hook_schema(snapshot.hook_schema.as_deref()),
    )
}

/// Projects the identities of `snapshot` under `schema`, the outcome of
/// resolving the snapshot's hook schema id.
///
/// An unresolvable schema is an error only when the snapshot reports an
/// active identity or a release that needs one.
fn apply_worker_identities_with(
    record: &mut SessionRecord,
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
    schema: Result<Option<&'static pohunek_worker_protocol::HookSchema>, &'static str>,
) -> Result<WorkerIdentityProjection, &'static str> {
    if snapshot.active_identity.is_some() && snapshot.active_identity_release.is_some() {
        return Err("active_identity_state_ambiguous");
    }
    let instance = snapshot
        .worker_instance_id
        .as_ref()
        .map(ToString::to_string);
    let journaled = snapshot.native_reference.as_ref();
    let journaled_reference_accepted = journaled_reference_matches_ordering(record, snapshot);
    apply_worker_launch_identity(
        record,
        snapshot.launch_identity.as_ref(),
        instance.as_deref(),
        journaled_reference_accepted,
    )?;

    let Some(identity) = &snapshot.active_identity else {
        let Some(release) = snapshot.active_identity_release.clone() else {
            return Ok(WorkerIdentityProjection::unreported());
        };
        let schema = schema?;
        let launch_runtime = super::agent_kind_label(&record.info.agent_base);
        if !schema.is_some_and(|schema| {
            schema.allows(pohunek_worker_protocol::HookAction::IdentityRelease)
                && schema.admits_identity_provider(&release.provider, Some(launch_runtime))
        }) {
            return Err("active_identity_release_provider_invalid");
        }
        let release_matches = record.info.active_agent.as_deref() == Some(&release.provider)
            && record.info.active_agent_pid == Some(release.process.pid);
        if release_matches {
            clear_active_identity(&mut record.info);
        }
        return Ok(WorkerIdentityProjection {
            active: None,
            release: Some(release),
            privately_reported: true,
        });
    };
    let agent_base = RuntimeRef::from_wire(&identity.provider);
    let reference = verify_active_identity_reference(record, schema, identity)?;
    let (active_id, active_path) = match reference {
        Some((kind, native)) => {
            if journaled.is_none() {
                apply_active_native_reference(record, snapshot, identity, kind, &native);
            }
            match kind {
                SessionRefKind::Id => (Some(native), None),
                SessionRefKind::Path => (None, Some(native)),
            }
        }
        None => (None, None),
    };
    record.info.active_agent = Some(identity.provider.clone());
    record.info.active_agent_base = Some(agent_base);
    record.info.active_agent_pid = Some(identity.process.pid);
    record.info.active_agent_session_id = active_id;
    record.info.active_agent_session_path = active_path;
    Ok(WorkerIdentityProjection {
        active: Some(ActiveAgentReport {
            source: format!("worker:{}", snapshot.worker_id),
            agent: identity.provider.clone(),
            seq: Some(identity.sequence),
            pid: Some(identity.process.pid),
            start_identity: Some(identity.process.start_identity),
            reported_at: crate::time::now(),
            activity_reported: false,
        }),
        release: None,
        privately_reported: true,
    })
}

/// Validates the reference part of a worker's active identity claim exactly as
/// the live identity import does: the claim must still be leased, the hook
/// schema must admit the provider for the launch runtime, and the claim's
/// frozen reference kind must carry a value.
///
/// Returns the validated reference, or `None` when the claim reports no
/// reference at all (a provider-activity report).
fn verify_active_identity_reference(
    record: &SessionRecord,
    schema: Result<Option<&'static pohunek_worker_protocol::HookSchema>, &'static str>,
    identity: &pohunek_worker_protocol::ActiveIdentityClaim,
) -> Result<Option<(SessionRefKind, String)>, &'static str> {
    if !identity_claim_expiry_is_valid(&identity.expires_at) {
        return Err("active_identity_expired_or_overlong");
    }
    let schema = schema?
        .filter(|schema| schema.allows(pohunek_worker_protocol::HookAction::IdentityReport))
        .filter(|schema| {
            schema.admits_identity_provider(
                &identity.provider,
                Some(super::agent_kind_label(&record.info.agent_base)),
            )
        })
        .ok_or("active_identity_provider_invalid")?;
    match (
        identity.reference_kind.as_deref(),
        identity.native_reference.as_deref(),
    ) {
        (Some(kind), Some(reference)) => {
            let kind = Some(kind)
                .filter(|kind| schema.admits_reference_kind(kind))
                .and_then(parse_reference_kind)
                .ok_or("active_identity_reference_kind_invalid")?;
            let native = validate_native_reference(kind, reference)
                .ok_or("active_identity_reference_invalid")?;
            Ok(Some((kind, native)))
        }
        (None, None) => Ok(None),
        _ => Err("active_identity_reference_incomplete"),
    }
}

fn journaled_reference_matches_ordering(
    record: &SessionRecord,
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
) -> bool {
    let Some(reference) = snapshot.native_reference.as_ref() else {
        return false;
    };
    let instance = snapshot
        .worker_instance_id
        .as_ref()
        .map(ToString::to_string);
    record.runtime.worker_instance_id.as_deref() == instance.as_deref()
        && snapshot
            .launch_identity
            .as_ref()
            .is_some_and(|launch| launch.process == reference.process)
        && reference.provider == super::agent_kind_label(&record.info.agent_base)
        && record.recovery.as_ref().is_some_and(|binding| {
            binding.reference_kind() == parse_reference_kind(&reference.reference_kind)
        })
        && record
            .native_identity_ordering
            .as_ref()
            .is_some_and(|ordering| {
                Some(ordering.worker_instance_id.as_str()) == instance.as_deref()
                    && ordering.worker_sequence == Some(reference.sequence)
                    && ordering.pid == reference.process.pid
                    && ordering.pid_start_identity == reference.process.start_identity
                    && match reference.reference_kind.as_str() {
                        "id" => record.info.native_session_id.as_deref(),
                        "path" => record.info.native_session_path.as_deref(),
                        _ => None,
                    } == Some(reference.native_reference.as_str())
            })
}

fn apply_worker_launch_identity(
    record: &mut SessionRecord,
    identity: Option<&pohunek_worker_protocol::ReportedLaunchIdentity>,
    worker_instance_id: Option<&str>,
    journaled_reference: bool,
) -> Result<(), &'static str> {
    let Some(identity) = identity else {
        return Ok(());
    };
    let replaced = apply_launch_identity(
        record,
        &identity.provider,
        &identity.reference_kind,
        &identity.native_reference,
        &identity.process,
        worker_instance_id,
        journaled_reference,
    )?;
    // A claim has no sequence of its own: a reference it replaces is keyed to
    // its generation with no report mark, which every real report outranks.
    let keyed_here = record
        .native_identity_ordering
        .as_ref()
        .is_some_and(|ordering| Some(ordering.worker_instance_id.as_str()) == worker_instance_id);
    let assigned = record.recovery.as_ref().is_some_and(is_assigned_runtime);
    if let (true, true, false, Some(instance)) =
        (replaced, assigned, keyed_here, worker_instance_id)
    {
        record.native_identity_ordering = Some(crate::store::NativeIdentityOrdering::unsequenced(
            instance,
            identity.process.pid,
            identity.process.start_identity,
        ));
    }
    Ok(())
}

/// Whether the recovery binding's runtime assigns its reference at launch.
fn is_assigned_runtime(binding: &ResumeBinding) -> bool {
    binding
        .native_launch
        .as_ref()
        .is_some_and(|launch| launch.assigned().is_some())
}

/// Binds the launch identity a worker accepted to the record's recovery, or
/// names the contradiction.
pub(super) fn apply_launch_identity(
    record: &mut SessionRecord,
    provider: &str,
    reference_kind: &str,
    native_reference: &str,
    process: &pohunek_worker_protocol::ProcessIdentity,
    worker_instance_id: Option<&str>,
    journaled_reference: bool,
) -> Result<bool, &'static str> {
    let expected_provider = super::agent_kind_label(&record.info.agent_base);
    if provider != expected_provider {
        return Err("launch_identity_provider_mismatch");
    }
    let kind =
        parse_reference_kind(reference_kind).ok_or("launch_identity_reference_kind_invalid")?;
    let binding = record
        .recovery
        .as_mut()
        .ok_or("launch_identity_recovery_missing")?;
    // A binding whose launch spec the migration could not restore has no kind
    // to compare; only a reference stored under the other kind contradicts the
    // report.
    let contradicts = match binding.reference_kind() {
        Some(bound) => bound != kind,
        None => stored_under_other_kind(
            record.info.native_session_id.as_deref(),
            record.info.native_session_path.as_deref(),
            binding,
            kind,
        ),
    };
    if contradicts {
        return Err("launch_identity_reference_kind_mismatch");
    }
    let native = validate_native_reference(kind, native_reference)
        .ok_or("launch_identity_reference_invalid")?;
    let existing = match kind {
        SessionRefKind::Id => record.info.native_session_id.as_deref(),
        SessionRefKind::Path => record.info.native_session_path.as_deref(),
    };
    let differs = existing.is_some_and(|existing| existing != native);
    // A current journaled report of the verified launch process supersedes
    // the immutable first launch claim, including after an in-agent switch.
    if journaled_reference {
        return Ok(false);
    }
    // A report from the verified launch process in this generation has
    // already superseded its first claim. An assigned runtime keeps its
    // existing generation-ordering behavior.
    if binding.native_reference_provenance != NativeReferenceProvenance::Assigned && differs {
        if record
            .native_identity_ordering
            .as_ref()
            .is_some_and(|ordering| {
                Some(ordering.worker_instance_id.as_str()) == worker_instance_id
                    && (is_assigned_runtime(binding)
                        || (ordering.pid == process.pid
                            && ordering.pid_start_identity == process.start_identity))
            })
        {
            return Ok(false);
        }
        if !is_assigned_runtime(binding) {
            return Err("launch_identity_reference_mismatch");
        }
    }
    binding.native_reference_provenance = NativeReferenceProvenance::Reported;
    match kind {
        SessionRefKind::Id => {
            record.info.native_session_id = Some(native.clone());
            record.info.native_session_path = None;
            binding.native_session_id = Some(native);
            binding.native_session_path = None;
        }
        SessionRefKind::Path => {
            record.info.native_session_path = Some(native.clone());
            record.info.native_session_id = None;
            binding.native_session_path = Some(native);
            binding.native_session_id = None;
        }
    }
    Ok(true)
}

/// Imports the native reference the worker journaled for its launch process
/// into `record`.
///
/// The reference is trusted by its generation binding, never by the liveness
/// of any process: the snapshot must belong to the record's own worker
/// instance and the reference must name the launch process the worker
/// verified. Anything unprovable leaves the stored reference alone. Every
/// path that applies a worker snapshot or commits terminal recovery metadata
/// calls it exactly once.
pub(super) fn import_native_reference(
    record: &mut SessionRecord,
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
) {
    let Some(reference) = snapshot.native_reference.as_ref() else {
        return;
    };
    let recorded = record.runtime.worker_instance_id.as_deref();
    let same_generation = recorded.is_some()
        && snapshot
            .worker_instance_id
            .as_ref()
            .map(pohunek_worker_protocol::WorkerInstanceId::as_str)
            == recorded;
    let bound_to_launch = snapshot
        .launch_identity
        .as_ref()
        .is_some_and(|launch| launch.process == reference.process);
    if same_generation && bound_to_launch {
        let _ = apply_journaled_native_reference(record, snapshot, reference);
    }
}

/// Imports a terminal snapshot's native reference, keeping the launch
/// process's active claim when the durable field is absent.
///
/// The journaled reference is the ordered record and always wins when the
/// worker reports it. A worker that predates that field keeps a conversation
/// switch only as its leased active identity claim, so a switch committed
/// between the last metadata import and the terminal transition would be lost
/// on recovery. When the durable field is absent, the fallback imports the
/// verified active claim's reference with the same proofs the live identity
/// import applies — the snapshot's worker instance, the verified launch
/// process and the ordering of the generation — and never imports a claim the
/// live import would refuse. Every terminal or final import of a worker
/// snapshot calls it in place of [`import_native_reference`].
pub(super) fn import_terminal_native_reference(
    record: &mut SessionRecord,
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
    schema: Result<Option<&'static pohunek_worker_protocol::HookSchema>, &'static str>,
) {
    import_native_reference(record, snapshot);
    if snapshot.native_reference.is_some() {
        // The worker journaled the ordered record the live metadata import
        // would have applied; the active claim never outranks it here.
        return;
    }
    let Some(identity) = snapshot.active_identity.as_ref() else {
        return;
    };
    match verify_active_identity_reference(record, schema, identity) {
        Ok(Some((kind, native))) => {
            apply_active_native_reference(record, snapshot, identity, kind, &native);
        }
        // A claim without a reference, or one the live import would refuse,
        // imports nothing.
        Ok(None) => {}
        Err(reason) => {
            tracing::warn!(
                session_id = %record.session_id,
                reason,
                "rejected terminal worker active identity claim; its native reference is not imported"
            );
        }
    }
}

/// Imports the native reference the worker journaled for its verified launch
/// process.
///
/// It is the ordered record of every report of the launch process and has no
/// lease, so a switch made while the daemon was absent is still read. Runtimes
/// that report their reference through a hook use the same proof.
fn apply_journaled_native_reference(
    record: &mut SessionRecord,
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
    reference: &pohunek_worker_protocol::ReportedNativeReference,
) -> Result<(), &'static str> {
    let kind =
        parse_reference_kind(&reference.reference_kind).ok_or("native_reference_kind_invalid")?;
    let native = validate_native_reference(kind, &reference.native_reference)
        .ok_or("native_reference_invalid")?;
    adopt_worker_reference(
        record,
        snapshot,
        &reference.provider,
        &reference.process,
        reference.sequence,
        kind,
        &native,
    );
    Ok(())
}

/// Imports the reference the launch process reports as its active identity,
/// for a worker that journals no separate native reference.
///
/// The worker rejects every launch claim after its first, so a conversation
/// switch inside the agent (`/clear`, in-session resume) reaches such a daemon
/// only as the active identity, for as long as its lease lasts.
fn apply_active_native_reference(
    record: &mut SessionRecord,
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
    identity: &pohunek_worker_protocol::ActiveIdentityClaim,
    kind: SessionRefKind,
    native: &str,
) {
    adopt_worker_reference(
        record,
        snapshot,
        &identity.provider,
        &identity.process,
        identity.sequence,
        kind,
        native,
    );
}

/// Supersedes the stored reference with one the launch process reported
/// through the worker.
///
/// It applies when the report names the launch runtime, is bound to the
/// verified launch process (never a nested agent), carries the frozen
/// reference kind and is newer than the last worker report accepted for the
/// runtime generation. The replacement carries its ordering key.
fn adopt_worker_reference(
    record: &mut SessionRecord,
    snapshot: &pohunek_worker_protocol::InspectSnapshot,
    provider: &str,
    process: &pohunek_worker_protocol::ProcessIdentity,
    sequence: u64,
    kind: SessionRefKind,
    native: &str,
) {
    let Some(binding) = record.recovery.as_mut() else {
        return;
    };
    let assigned = is_assigned_runtime(binding);
    let Some(worker_instance_id) = snapshot
        .worker_instance_id
        .as_ref()
        .map(ToString::to_string)
    else {
        return;
    };
    if record.runtime.worker_instance_id.as_deref() != Some(worker_instance_id.as_str()) {
        return;
    }
    // A hook launch in a new generation must first confirm the reference it
    // was asked to resume. Its later reports may then switch conversations.
    if !assigned
        && record
            .native_identity_ordering
            .as_ref()
            .is_none_or(|ordering| ordering.worker_instance_id != worker_instance_id)
        && match kind {
            SessionRefKind::Id => record.info.native_session_id.as_deref(),
            SessionRefKind::Path => record.info.native_session_path.as_deref(),
        }
        .is_some_and(|existing| {
            snapshot
                .launch_identity
                .as_ref()
                .is_none_or(|launch| launch.native_reference != existing)
        })
    {
        return;
    }
    if binding.reference_kind() != Some(kind)
        || provider != super::agent_kind_label(&record.info.agent_base)
        || (if assigned {
            snapshot
                .launch_identity
                .as_ref()
                .map_or(snapshot.child_process.as_ref(), |launch| {
                    Some(&launch.process)
                })
                != Some(process)
        } else {
            snapshot
                .launch_identity
                .as_ref()
                .map(|launch| &launch.process)
                != Some(process)
        })
    {
        return;
    }
    if !super::native_report_is_current(
        record.native_identity_ordering.as_ref(),
        &worker_instance_id,
        sequence,
        crate::store::ReportTransport::Worker,
    ) {
        return;
    }
    match kind {
        SessionRefKind::Id => {
            record.info.native_session_id = Some(native.to_owned());
            record.info.native_session_path = None;
            binding.native_session_id = Some(native.to_owned());
            binding.native_session_path = None;
        }
        SessionRefKind::Path => {
            record.info.native_session_path = Some(native.to_owned());
            record.info.native_session_id = None;
            binding.native_session_path = Some(native.to_owned());
            binding.native_session_id = None;
        }
    }
    binding.native_reference_provenance = NativeReferenceProvenance::Reported;
    record.native_identity_ordering = Some(crate::store::NativeIdentityOrdering::accepting(
        record.native_identity_ordering.as_ref(),
        &worker_instance_id,
        process.pid,
        process.start_identity,
        crate::store::ReportTransport::Worker,
        sequence,
    ));
}

/// Prefix of the conflict reasons that say a record disagrees with its stored
/// resume binding (see [`merge_persisted_recovery`]).
const RESUME_BINDING_CONFLICT_PREFIX: &str = "resume_binding_";

/// Whether `reason` is a conflict between a record and its stored resume
/// binding.
///
/// Only startup merges the binding into the record, so a background re-check
/// of the record cannot repeat that decision and must not re-adopt a runtime
/// the merge quarantined.
pub(super) fn is_resume_binding_conflict(reason: &str) -> bool {
    reason.starts_with(RESUME_BINDING_CONFLICT_PREFIX)
}

/// The worker a session's runtime names, when it names one.
fn info_worker_id(info: &protocol::SessionInfo) -> Option<&str> {
    info.runtime
        .as_ref()
        .and_then(|runtime| runtime.worker_id.as_deref())
}

/// Logs the one WARN of a session runtime classified `Conflict`, `Lost`,
/// `Reconnecting` or `Incompatible`.
///
/// `detail` is the operator-facing evidence behind `reason`, empty when the
/// reason says it all. Classifications into `Terminal` are not logged: the
/// runtime ended as its session did.
pub(super) fn warn_unavailable(
    session_id: &str,
    worker_id: Option<&str>,
    state: RuntimeState,
    reason: &str,
    detail: &str,
) {
    if !matches!(
        state,
        RuntimeState::Conflict
            | RuntimeState::Lost
            | RuntimeState::Reconnecting
            | RuntimeState::Incompatible
    ) {
        return;
    }
    tracing::warn!(
        name: "session.runtime.unavailable",
        session_id = %session_id,
        worker_id = %worker_id.unwrap_or("none"),
        runtime.state = %super::runtime_state_label(state),
        reason = %reason,
        detail = (!detail.is_empty()).then_some(detail),
        "session runtime is {{runtime.state}}: {{reason}}"
    );
}

/// Whether a reference is stored for the kind other than `kind`, on the
/// session (`info_id`, `info_path`) or on its recovery `binding`.
fn stored_under_other_kind(
    info_id: Option<&str>,
    info_path: Option<&str>,
    binding: &ResumeBinding,
    kind: SessionRefKind,
) -> bool {
    match kind {
        SessionRefKind::Id => info_path.is_some() || binding.native_session_path.is_some(),
        SessionRefKind::Path => info_id.is_some() || binding.native_session_id.is_some(),
    }
}

pub(super) fn parse_reference_kind(kind: &str) -> Option<SessionRefKind> {
    match kind {
        "id" => Some(SessionRefKind::Id),
        "path" => Some(SessionRefKind::Path),
        _ => None,
    }
}

fn validate_native_reference(kind: SessionRefKind, value: &str) -> Option<String> {
    let reference = match kind {
        SessionRefKind::Id => SessionRef::id(value),
        SessionRefKind::Path => SessionRef::path(value),
    }
    .ok()?;
    Some(reference.value().to_owned())
}

#[derive(Debug, Deserialize)]
struct LegacyMigrationManifest {
    schema_version: u32,
    created_at: String,
    store_sha256: String,
    accept_runtime_loss: bool,
    sessions: Vec<protocol::SessionInfo>,
    live_session_ids: Vec<String>,
}

/// Imports a pending migration manifest, then loads every logical record and
/// resume binding.
///
/// # Errors
///
/// Returns the typed migration error of [`import_legacy_manifest`], or
/// `session_reconcile_failed` when the store cannot be loaded.
async fn load_durable_state(
    store: Arc<crate::store::Store>,
) -> Result<(LegacyManifest, Vec<SessionRecord>, Vec<ResumeBinding>), ProtocolError> {
    let import_store = Arc::clone(&store);
    let manifest = tokio::task::spawn_blocking(move || import_legacy_manifest(&import_store))
        .await
        .map_err(|_join_error| {
            runtime_error(
                "migration_import_failed",
                "legacy migration import task panicked",
            )
        })??;
    let (records, resume_bindings) = tokio::task::spawn_blocking(move || {
        Ok::<_, std::io::Error>((store.load_sessions()?, store.load_resume()?))
    })
    .await
    .map_err(|_join_error| {
        runtime_error(
            "session_reconcile_failed",
            "logical-session store load task panicked",
        )
    })?
    .map_err(|error| {
        runtime_error(
            "session_reconcile_failed",
            format!("failed to load logical sessions: {error}"),
        )
    })?;
    Ok((manifest, records, resume_bindings))
}

/// What startup found at the one-time migration manifest path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyManifest {
    /// No manifest exists; nothing was imported.
    Missing,
    /// A manifest was imported, or archived over an already-migrated store.
    Processed,
}

/// Surfaces legacy resume bindings that no migration manifest imported.
///
/// Called only when startup found no manifest and no logical record: the
/// bindings were then written by a legacy daemon whose sessions were never
/// snapshotted by `pohunek migration preflight`, so nothing can import them
/// (a binding alone proves neither consent to runtime loss nor the session's
/// state). They stay on disk untouched, for a later manifest to import, and
/// each is listed as an orphaned inventory entry with reason
/// [`MIGRATION_MANIFEST_MISSING`] so the operator sees what is missing. While
/// any is listed, creating a session fails with the same code
/// ([`SessionRegistry::ensure_migration_settled`]).
fn unmigrated_legacy_bindings(bindings: &[ResumeBinding]) -> Vec<RuntimeInventoryEntry> {
    if bindings.is_empty() {
        return Vec::new();
    }
    tracing::error!(
        name: "reconcile.migration.manifest_missing",
        legacy_bindings = bindings.len(),
        "legacy resume bindings exist without logical records or a migration manifest; session creation is refused until `pohunek migration preflight` runs against the legacy daemon and a restart imports its manifest"
    );
    let mut entries = bindings
        .iter()
        .map(|binding| RuntimeInventoryEntry {
            runtime_slot: binding.session_id.clone(),
            claimed_session_id: Some(binding.session_id.clone()),
            worker_id: None,
            worker_instance_id: None,
            status: RuntimeInventoryStatus::Orphaned,
            reason: Some(MIGRATION_MANIFEST_MISSING.to_owned()),
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.runtime_slot.cmp(&right.runtime_slot));
    entries.dedup_by(|left, right| left.runtime_slot == right.runtime_slot);
    entries
}

/// Imports the one-time migration manifest `pohunek migration preflight`
/// wrote against the legacy daemon, then archives it.
///
/// # Errors
///
/// Returns a typed migration error (`migration_import_failed`,
/// `migration_store_changed`, `migration_runtime_loss_not_accepted`,
/// `migration_manifest_mismatch`, `migration_import_stale`) when the manifest
/// cannot be read, validated, or committed; the store is then left untouched.
#[expect(
    clippy::too_many_lines,
    reason = "one-time migration validation and import remain atomic and auditable together"
)]
fn import_legacy_manifest(store: &crate::store::Store) -> Result<LegacyManifest, ProtocolError> {
    let Some(data_dir) = store.path().parent() else {
        return Err(runtime_error(
            "migration_import_failed",
            "metadata store has no parent directory",
        ));
    };
    let path = data_dir
        .join("migrations")
        .join("durable-session-workers.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LegacyManifest::Missing)
        }
        Err(error) => {
            return Err(runtime_error(
                "migration_import_failed",
                format!("failed to read migration manifest: {error}"),
            ))
        }
    };
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(runtime_error(
            "migration_import_failed",
            "migration manifest exceeds the 16 MiB safety bound",
        ));
    }
    let manifest: LegacyMigrationManifest = serde_json::from_slice(&bytes).map_err(|error| {
        runtime_error(
            "migration_import_failed",
            format!("migration manifest is invalid: {error}"),
        )
    })?;
    if manifest.schema_version != 1 {
        return Err(runtime_error(
            "migration_import_failed",
            format!(
                "unsupported migration manifest schema {}",
                manifest.schema_version
            ),
        ));
    }
    let existing = store.load_sessions().map_err(|error| {
        runtime_error(
            "migration_import_failed",
            format!("failed to inspect logical records before migration: {error}"),
        )
    })?;
    if !existing.is_empty() {
        archive_imported_manifest(&path, &manifest)?;
        return Ok(LegacyManifest::Processed);
    }
    let store_bytes = match std::fs::read(store.path()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(runtime_error(
                "migration_import_failed",
                format!("failed to fingerprint metadata store: {error}"),
            ))
        }
    };
    let actual_fingerprint = format!("{:x}", Sha256::digest(store_bytes));
    // The preflight fingerprints the store as the legacy daemon left it, which
    // the startup schema migration has already rewritten; the pre-migration
    // backup proves the current bytes derive from exactly those bytes.
    if actual_fingerprint != manifest.store_sha256
        && !store
            .is_schema_migration_of(&manifest.store_sha256)
            .map_err(|error| {
                runtime_error(
                    "migration_import_failed",
                    format!("failed to verify the metadata store against its backup: {error}"),
                )
            })?
    {
        return Err(runtime_error(
            "migration_store_changed",
            "metadata store changed after migration preflight; rerun preflight",
        ));
    }
    if !manifest.live_session_ids.is_empty() && !manifest.accept_runtime_loss {
        return Err(runtime_error(
            "migration_runtime_loss_not_accepted",
            format!(
                "migration would lose live runtimes: {}",
                manifest.live_session_ids.join(", ")
            ),
        ));
    }
    let expected_live: std::collections::BTreeSet<&str> = manifest
        .live_session_ids
        .iter()
        .map(String::as_str)
        .collect();
    let actual_live: std::collections::BTreeSet<&str> = manifest
        .sessions
        .iter()
        .filter(|session| {
            matches!(
                session.state,
                protocol::SessionState::Starting | protocol::SessionState::Running
            )
        })
        .map(|session| session.id.0.as_str())
        .collect();
    if manifest.sessions.iter().any(|session| {
        session.agent_base.launchable().is_err()
            || session
                .active_agent_base
                .as_ref()
                .is_some_and(|active| active.launchable().is_err())
    }) {
        return Err(runtime_error(
            "migration_import_failed",
            "migration manifest contains an unsupported agent kind",
        ));
    }
    if expected_live != actual_live {
        return Err(runtime_error(
            "migration_manifest_mismatch",
            "migration live-session classification does not match its snapshot",
        ));
    }
    let resume = store.load_resume().map_err(|error| {
        runtime_error(
            "migration_import_failed",
            format!("failed to load legacy recovery records: {error}"),
        )
    })?;
    for mut info in manifest.sessions.clone() {
        let live = actual_live.contains(info.id.0.as_str());
        let runtime_state = if live {
            RuntimeState::Lost
        } else {
            RuntimeState::Terminal
        };
        info.runtime = Some(protocol::SessionRuntime {
            state: runtime_state,
            runtime_generation: protocol::RuntimeGeneration::new(1),
            worker_id: None,
            worker_instance_id: None,
            started_at: None,
            last_connected_at: None,
            loss_reason: live.then(|| "legacy_runtime_not_transferable".to_owned()),
        });
        let recovery = resume
            .iter()
            .find(|binding| binding.session_id == info.id.0)
            .cloned();
        let session_id = info.id.0.clone();
        let outcome = store
            .record_session(&SessionRecord {
                schema_version: 1,
                session_id: info.id.0.clone(),
                desired_state: if live {
                    DesiredState::Running
                } else {
                    DesiredState::Stopped
                },
                transaction: None,
                info,
                recovery,
                native_identity_ordering: None,
                runtime: crate::store::RuntimeRecord {
                    state: runtime_state,
                    worker_id: None,
                    worker_instance_id: None,
                    service_id: None,
                    generation: None,
                    executable: None,
                    reason: live.then(|| "legacy_runtime_not_transferable".to_owned()),
                },
            })
            .map_err(|error| {
                runtime_error(
                    "migration_import_failed",
                    format!("failed to import logical session: {error}"),
                )
            })?;
        match outcome {
            SessionWriteOutcome::Applied => {}
            SessionWriteOutcome::AppliedDurabilityUncertain { error } => {
                tracing::warn!(
                    session_id,
                    durability_error = %error,
                    "legacy session import is visible but directory durability is uncertain"
                );
            }
            SessionWriteOutcome::StaleRuntime => {
                return Err(runtime_error(
                    "migration_import_stale",
                    "legacy session import lost a concurrent runtime commit",
                ));
            }
            SessionWriteOutcome::StaleSnapshot => {
                return Err(runtime_error(
                    "migration_import_stale",
                    "legacy session import lost a concurrent record commit",
                ));
            }
        }
    }
    archive_imported_manifest(&path, &manifest)?;
    Ok(LegacyManifest::Processed)
}

fn archive_imported_manifest(
    path: &std::path::Path,
    manifest: &LegacyMigrationManifest,
) -> Result<(), ProtocolError> {
    let fingerprint = manifest
        .store_sha256
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(16)
        .collect::<String>();
    let created_hash = format!("{:x}", Sha256::digest(manifest.created_at.as_bytes()));
    let archive = path.with_file_name(format!(
        "durable-session-workers.imported-{}-{}.json",
        fingerprint,
        &created_hash[..16]
    ));
    std::fs::rename(path, archive).map_err(|error| {
        runtime_error(
            "migration_import_failed",
            format!("failed to archive imported migration manifest: {error}"),
        )
    })
}

enum TerminalJournalClassification {
    Exact(Box<JournalEvidence>),
    Conflict,
    Absent,
}

/// Why a journal file of a session is not usable as evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum JournalReject {
    /// The file or its directory cannot be read or parsed.
    Unreadable,
    /// The file declares a schema the daemon does not read.
    UnsupportedSchema(u32),
    /// The file disagrees with the path it was found under, or holds a
    /// malformed identity.
    Mismatched,
}

#[derive(Debug, Clone, Default)]
pub(super) struct WorkerJournalScan {
    pub(super) evidence: Vec<JournalEvidence>,
    pub(super) conflict: bool,
    /// The journal files behind `conflict`, by file name.
    pub(super) rejected: Vec<(String, JournalReject)>,
}

impl WorkerJournalScan {
    /// Keeps only the journals naming `generation`; `None` keeps every one.
    ///
    /// Journals of other generations are history, never evidence about the
    /// record's current generation.
    pub(super) fn scoped_to(&self, generation: Option<&super::Generation>) -> Self {
        Self {
            evidence: self
                .evidence
                .iter()
                .filter(|journal| {
                    generation
                        .is_none_or(|generation| journal.generation == generation.generation())
                })
                .cloned()
                .collect(),
            conflict: self.conflict,
            rejected: self.rejected.clone(),
        }
    }

    /// Returns the journal naming `generation`, or `None` when the session has
    /// none.
    ///
    /// # Errors
    ///
    /// Returns why the journal cannot be decided: an unreadable or mismatched
    /// journal file of the session could be that generation's, and several
    /// journals naming it are ambiguous.
    fn journal_of_generation(&self, generation: &str) -> Result<Option<&JournalEvidence>, String> {
        if self.conflict {
            return Err("the session has an unreadable or mismatched worker journal".to_owned());
        }
        let mut journals = self
            .evidence
            .iter()
            .filter(|journal| journal.generation == generation);
        match (journals.next(), journals.next()) {
            (None, _) => Ok(None),
            (Some(journal), None) => Ok(Some(journal)),
            (Some(_), Some(_)) => Err(format!("several journals name generation {generation}")),
        }
    }
}

fn classify_terminal_journals(
    scan: &WorkerJournalScan,
    record: &SessionRecord,
) -> TerminalJournalClassification {
    if scan.conflict {
        return TerminalJournalClassification::Conflict;
    }
    let mut terminal = scan
        .evidence
        .iter()
        .filter(|journal| {
            journal.phase == JournalPhase::Terminal || journal.phase == JournalPhase::Faulted
        })
        .collect::<Vec<_>>();
    if terminal.is_empty() {
        return TerminalJournalClassification::Absent;
    }
    terminal.retain(|journal| {
        journal.phase == JournalPhase::Terminal
            && journal.session_id == record.session_id
            && record
                .runtime
                .worker_id
                .as_deref()
                .is_none_or(|expected| expected == journal.worker_id)
            && record
                .runtime
                .worker_instance_id
                .as_deref()
                .is_none_or(|expected| journal.worker_instance_id.as_deref() == Some(expected))
    });
    match terminal.as_slice() {
        [journal] => TerminalJournalClassification::Exact(Box::new((*journal).clone())),
        [] | [_, _, ..] => TerminalJournalClassification::Conflict,
    }
}

pub(super) fn scan_worker_journals(
    state_root: &Path,
) -> std::io::Result<HashMap<String, WorkerJournalScan>> {
    let mut journals = HashMap::<String, WorkerJournalScan>::new();
    let root_metadata = match std::fs::symlink_metadata(state_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(journals),
        Err(error) => return Err(error),
    };
    validate_discovery_path(state_root, &root_metadata, true)?;
    let root = TrustedDir::open_absolute(state_root, 0o700).map_err(std::io::Error::other)?;
    for session_name in root.entry_names().map_err(std::io::Error::other)? {
        let Some(session_id) = session_name
            .to_str()
            .filter(|name| pohunek_paths::valid_worker_session_id(name).is_some())
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        // An unsafe or unreadable session directory may hold any worker's
        // journal, so it is a conflict rather than an absence.
        let Ok(session_directory) = root.open_child(&session_name, 0o700) else {
            let scan = journals.entry(session_id).or_default();
            scan.conflict = true;
            scan.rejected
                .push((String::new(), JournalReject::Unreadable));
            continue;
        };
        for journal_name in session_directory
            .entry_names()
            .map_err(std::io::Error::other)?
        {
            let Some(journal_stem) = journal_name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .filter(|stem| pohunek_paths::valid_worker_id(stem).is_some())
            else {
                continue;
            };
            let scan = journals.entry(session_id.clone()).or_default();
            let evidence = session_directory
                .read_file(&journal_name, 0o600, MAX_WORKER_JOURNAL_BYTES)
                .map_err(|_unreadable| JournalReject::Unreadable)
                .and_then(|bytes| journal_evidence(&bytes, &session_id, journal_stem));
            match evidence {
                Ok(evidence) => scan.evidence.push(evidence),
                Err(reject) => {
                    scan.conflict = true;
                    scan.rejected
                        .push((journal_name.to_string_lossy().into_owned(), reject));
                }
            }
        }
    }
    Ok(journals)
}

/// Decodes one journal file found under `session_id` as `journal_stem`.
fn journal_evidence(
    bytes: &[u8],
    session_id: &str,
    journal_stem: &str,
) -> Result<JournalEvidence, JournalReject> {
    if let Err(error) = require_supported_journal_schema(bytes, session_id, journal_stem) {
        return Err(error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<UnsupportedJournalSchema>())
            .map_or(JournalReject::Unreadable, |unsupported| {
                JournalReject::UnsupportedSchema(unsupported.found)
            }));
    }
    let evidence = serde_json::from_slice::<JournalEvidence>(bytes)
        .map_err(|_malformed| JournalReject::Unreadable)?;
    if !journal_schema_supported(evidence.schema_version)
        || evidence.session_id != session_id
        || evidence.worker_id != journal_stem
        || pohunek_paths::valid_worker_generation(&evidence.generation).is_none()
        || !evidence.executable.is_absolute()
        || BootIdentity::parse(evidence.boot_identity.clone()).is_err()
    {
        return Err(JournalReject::Mismatched);
    }
    Ok(evidence)
}

fn discover_runtime_slots(runtime_root: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut slots = Vec::new();
    let root_metadata = match std::fs::symlink_metadata(runtime_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(slots),
        Err(error) => return Err(error),
    };
    validate_discovery_path(runtime_root, &root_metadata, true)?;
    let entries = match std::fs::read_dir(runtime_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(slots),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let metadata = entry.metadata()?;
        if validate_discovery_path(&entry.path(), &metadata, true).is_err() {
            continue;
        }
        let Some(slot) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let socket = entry.path().join(pohunek_paths::WORKER_SOCKET_NAME);
        let socket_type = match std::fs::symlink_metadata(&socket) {
            Ok(metadata) => metadata.file_type(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let socket_metadata = std::fs::symlink_metadata(&socket)?;
        if socket_type.is_socket()
            && !socket_type.is_symlink()
            && validate_discovery_path(&socket, &socket_metadata, false).is_ok()
        {
            slots.push((slot, socket));
        }
    }
    slots.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(slots)
}

fn validate_discovery_path(
    path: &Path,
    metadata: &std::fs::Metadata,
    directory: bool,
) -> std::io::Result<()> {
    let expected_type = if directory {
        metadata.file_type().is_dir()
    } else {
        metadata.file_type().is_socket()
    };
    let owner_private = metadata.uid() == effective_uid() && metadata.mode().trailing_zeros() >= 6;
    if expected_type && !metadata.file_type().is_symlink() && owner_private {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "durable worker discovery rejected unsafe path {}",
                path.display()
            ),
        ))
    }
}

fn effective_uid() -> u32 {
    #[expect(
        unsafe_code,
        reason = "filesystem discovery must compare ownership to the effective Unix uid"
    )]
    // SAFETY: `geteuid` has no preconditions and only reads process identity.
    unsafe {
        libc::geteuid()
    }
}

/// Whether a socket-operation failure proves that nothing listens on the
/// discovered endpoint: the connect was refused or the endpoint vanished.
/// Any other failure — an unreadable or unowned path (`PermissionDenied`) or
/// an answered but incompatible or mismatched worker — stays quarantined in
/// the inventory.
fn no_listener_on_socket(error: &WorkerError) -> bool {
    match error {
        WorkerError::Socket { source, .. } => matches!(
            source.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        ),
        _ => false,
    }
}

fn persisted_identity_mismatch(record: &SessionRecord, snapshot: &InspectSnapshot) -> bool {
    record
        .runtime
        .worker_id
        .as_deref()
        .is_some_and(|expected| expected != snapshot.worker_id.as_str())
        || record
            .runtime
            .worker_instance_id
            .as_deref()
            .zip(snapshot.worker_instance_id.as_ref())
            .is_some_and(|(expected, actual)| expected != actual.as_str())
}

fn discovery_failure_entry(slot: String, error: &WorkerError) -> RuntimeInventoryEntry {
    let (runtime_state, reason) = classify_connect_error(error);
    let status = if runtime_state == RuntimeState::Incompatible {
        RuntimeInventoryStatus::Incompatible
    } else {
        RuntimeInventoryStatus::IdentityMismatch
    };
    RuntimeInventoryEntry {
        runtime_slot: slot,
        claimed_session_id: None,
        worker_id: None,
        worker_instance_id: None,
        status,
        reason: Some(reason.to_owned()),
    }
}

fn classify_connect_error(error: &WorkerError) -> (RuntimeState, &'static str) {
    match error {
        WorkerError::Rejected {
            code: ControlCode::WorkerProtocolIncompatible,
            ..
        } => (RuntimeState::Incompatible, "worker_protocol_incompatible"),
        WorkerError::Rejected {
            code: ControlCode::ControllerBusy,
            ..
        } => (RuntimeState::Conflict, "controller_busy"),
        WorkerError::ResponseMismatch => (RuntimeState::Conflict, "runtime_identity_mismatch"),
        // `connect_discovered` cannot currently produce this error: it is only
        // emitted by attach capability checks after a controller is connected.
        // Keep a semantically correct classification for future callers rather
        // than reporting an otherwise reachable capability mismatch as loss.
        WorkerError::AttachSnapshotUnsupported { .. } => {
            (RuntimeState::Incompatible, "attach_snapshot_unsupported")
        }
        WorkerError::ObservationUnsupported { .. } => {
            (RuntimeState::Incompatible, "observation_unsupported")
        }
        WorkerError::Socket { .. }
        | WorkerError::Protocol(_)
        | WorkerError::Rejected { .. }
        | WorkerError::NotInitialized
        | WorkerError::AttachReadyTimeout { .. }
        | WorkerError::TornStream => (RuntimeState::Lost, UNREACHABLE_SOCKET),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use pohunek_test_support::wait::{wait_until, HANG_GUARD};

    use crate::agent::NativeReferenceProvenance;
    #[cfg(target_os = "linux")]
    use base64::Engine as _;
    use pohunek_session_worker::{
        ChildIdentity as JournalChildIdentity, Journal, JournalRecord,
        RuntimeOutcome as JournalRuntimeOutcome, RuntimePhase as JournalRuntimePhase, Server,
        ServerArgs, WorkerConfig,
    };
    use pohunek_worker_protocol::AncestryMatcher;
    use pohunek_worker_protocol::{
        ActiveIdentityClaim, ControlCode, ControlError, ControlMessage, ControlReader,
        ControlResponse, ControlWriter, Dimensions, Initialize, InitializeLimits, InspectSnapshot,
        LaunchIdentity, ProcessIdentity, ReleasedIdentityClaim, ReportedLaunchIdentity,
        ResponseKind, RuntimePhase as WorkerRuntimePhase, SecretEnv, SessionId as WorkerSessionId,
        StopPolicy, SubagentField, SubagentPhase, SubagentSnapshot, TransactionId, Version,
        WorkerId, WorkerInstanceId,
    };
    use protocol::{
        AgentActivity, CwdSource, ForkCwdMode, ProcessStartIdentity, ReportSequence,
        RuntimeInventoryStatus, RuntimeRef, RuntimeState, SessionForkParams, SessionId,
        SessionInfo, SessionNewParams, SessionReportNativeIdParams, SessionRuntime, SessionState,
        StateSource,
    };
    use sha2::{Digest, Sha256};
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::net::UnixStream;

    use super::{
        apply_worker_identities_with, import_legacy_manifest, import_worker_identities,
        import_worker_subagents, map_worker_subagents, merge_persisted_recovery,
        validate_worker_identity_process_facts, SessionRegistry,
    };
    use crate::agent::{InputRules, NativeSessionLaunch, SessionRefKind};
    use crate::procwatch::readable_host::ReadableHost;
    use crate::procwatch::{
        ExitWatch, HostInspector, OwnershipMarkers, Pid, ProcessFact,
        ProcessIdentity as OsProcessIdentity, ProcessInspector,
    };
    use crate::runtime::lifecycle::DEV_WORKER_CONNECT;
    use crate::session::SessionRegistryConfig;
    use crate::store::{
        DesiredState, NativeIdentityOrdering, ResumeBinding, RuntimeRecord, SessionRecord,
        SessionWriteOutcome, Store, StoredInputRules,
    };

    /// Interactive shell for registries whose sessions run the default shell.
    ///
    /// A fixed `/bin/sh` keeps the sessions independent of the host user's
    /// `$SHELL` and its startup files, whose background helpers can hold the PTY
    /// open past the stop deadline.
    fn hermetic_shell() -> crate::session::ShellCommand {
        crate::session::ShellCommand::new("/bin/sh", std::iter::empty::<String>())
    }

    /// Claude's config-home variable the fixture registry declares, so a
    /// Claude recovery binding's transcript preflight verifies a fixture
    /// transcript instead of scanning the test host's own Claude home.
    const FIXTURE_CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";

    /// A registry whose launch environment points Claude's config home at
    /// `claude_home`, for tests whose fixtures restore a Claude recovery
    /// binding through the transcript preflight.
    fn replacement_registry_with_claude_fixture(
        config: SessionRegistryConfig,
        claude_home: &Path,
    ) -> SessionRegistry {
        let crate::runtime::EnvironmentSource::Fixed(mut variables) =
            crate::test_support::thread_environment_source()
        else {
            panic!("a test fixture supplies an explicit environment");
        };
        variables.insert(FIXTURE_CLAUDE_CONFIG_DIR.into(), claude_home.into());
        let allowlist = pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .map(|name| (*name).to_owned())
            .chain([FIXTURE_CLAUDE_CONFIG_DIR.to_owned()])
            .collect();
        SessionRegistry::new_with_runtimes_and_environment(
            config,
            crate::agent::host::fixture::builtin_host(),
            crate::runtime::EnvironmentSource::Fixed(variables),
            allowlist,
        )
    }

    fn temp_root() -> crate::test_support::ScopedDir {
        // Short enough for worker sockets named by full session ids on macOS.
        crate::test_support::scoped_dir("ph-rec-")
    }

    fn create_private_dir(path: &Path) {
        std::fs::create_dir_all(path).expect("create private test directory");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("secure private test directory");
    }

    /// Releases the drain fixture's barriers when the Linux-only drain test ends.
    #[cfg(target_os = "linux")]
    struct ReleaseFiles(Vec<PathBuf>);

    #[cfg(target_os = "linux")]
    impl Drop for ReleaseFiles {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::write(path, b"release");
            }
        }
    }

    /// The [`ReadableHost`] view with injectable enumeration, descendant and
    /// marker failures.
    #[derive(Debug, Default)]
    struct RetryInspector {
        fail_descendants: AtomicBool,
        fail_enumeration: AtomicBool,
        fail_markers: AtomicBool,
        descendant_calls: AtomicUsize,
        inner: ReadableHost,
    }

    impl RetryInspector {
        fn fail_enumeration(&self, fail: bool) {
            self.fail_enumeration.store(fail, Ordering::Release);
        }

        fn fail_descendants(&self, fail: bool) {
            self.fail_descendants.store(fail, Ordering::Release);
        }

        fn fail_markers(&self, fail: bool) {
            self.fail_markers.store(fail, Ordering::Release);
        }

        fn descendant_calls(&self) -> usize {
            self.descendant_calls.load(Ordering::Acquire)
        }
    }

    impl ProcessInspector for RetryInspector {
        fn identity(
            &self,
            pid: Pid,
        ) -> Result<Option<crate::procwatch::ProcessIdentity>, crate::procwatch::Error> {
            self.inner.identity(pid)
        }

        fn is_running(&self, identity: OsProcessIdentity) -> Result<bool, crate::procwatch::Error> {
            self.inner.is_running(identity)
        }

        fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, crate::procwatch::Error> {
            self.inner.parent_pid(pid)
        }

        fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, crate::procwatch::Error> {
            self.inner.process(pid)
        }

        fn same_user_processes(&self) -> Result<Vec<ProcessFact>, crate::procwatch::Error> {
            if self.fail_enumeration.load(Ordering::Acquire) {
                return Err(crate::procwatch::Error::from_io(
                    "test_enumeration",
                    std::io::Error::other("process table unreadable"),
                ));
            }
            self.inner.same_user_processes()
        }

        fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, crate::procwatch::Error> {
            self.descendant_calls.fetch_add(1, Ordering::AcqRel);
            if self.fail_descendants.load(Ordering::Acquire) {
                return Err(crate::procwatch::Error::from_io(
                    "test_descendants",
                    std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "transient process inspection failure",
                    ),
                ));
            }
            self.inner.descendants(root)
        }

        fn cwd(&self, pid: Pid) -> Result<PathBuf, crate::procwatch::Error> {
            self.inner.cwd(pid)
        }

        fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, crate::procwatch::Error> {
            self.inner.executable(pid)
        }

        fn exit_watch(
            &self,
            identity: OsProcessIdentity,
        ) -> Result<ExitWatch, crate::procwatch::Error> {
            self.inner.exit_watch(identity)
        }

        fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, crate::procwatch::Error> {
            if self.fail_markers.load(Ordering::Acquire) {
                return Err(crate::procwatch::Error::from_io(
                    "test_markers",
                    std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                ));
            }
            self.inner.ownership_markers(pid)
        }

        fn foreground_process_group(
            &self,
            root_pid: Pid,
        ) -> Result<Option<Pid>, crate::procwatch::Error> {
            self.inner.foreground_process_group(root_pid)
        }
    }

    /// In-PTY identity reporter used by the worker-identity fixtures.
    ///
    /// The worker accepts a hook report only from the reported process or one
    /// of its descendants, so a report cannot come from the test process. This
    /// runs inside the managed PTY, publishes its own process id, and forwards
    /// request files verbatim to the private worker socket. Callers that want a
    /// self-report use the published id; callers that report the PTY root rely
    /// on this process being a descendant of it.
    ///
    /// Requests and responses are published through a temporary file plus a
    /// rename so neither side can observe a partial file.
    const IDENTITY_REPORTER: &str = r#"import json
import os
import socket
import sys
import time

pid_path, inbox = sys.argv[1], sys.argv[2]
endpoint = os.environ["POHUNEK_WORKER_SOCKET_PATH"]
os.makedirs(inbox, exist_ok=True)


def publish(path, payload):
    staged = path + ".staged"
    with open(staged, "wb") as handle:
        handle.write(payload)
    os.replace(staged, path)


publish(pid_path, str(os.getpid()).encode())

# Exit as soon as the launching process is gone, so the reporter never holds the
# PTY open past the lifetime of the tree it belongs to.
parent = os.getppid()

while os.getppid() == parent:
    for name in sorted(os.listdir(inbox)):
        if not name.endswith(".req"):
            continue
        request_path = os.path.join(inbox, name)
        try:
            with open(request_path, "rb") as handle:
                payload = handle.read()
        except OSError:
            continue
        try:
            client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            client.settimeout(2.0)
            client.connect(endpoint)
            client.sendall(payload.rstrip() + b"\n")
            answer = client.recv(4096).splitlines()[0]
            client.close()
        except Exception as error:
            answer = json.dumps({"ok": False, "error": str(error)}).encode()
        publish(request_path[:-4] + ".res", answer)
        os.remove(request_path)
    time.sleep(0.02)
"#;

    /// Handle to one in-PTY reporter: its published id and its request inbox.
    #[derive(Debug, Clone)]
    struct Reporter {
        pid_path: std::path::PathBuf,
        inbox: std::path::PathBuf,
    }

    impl Reporter {
        /// Returns the shell fragment that starts this reporter in the PTY.
        fn launch(&self, script: &Path) -> String {
            format!(
                "python3 {} {} {} &",
                script.display(),
                self.pid_path.display(),
                self.inbox.display()
            )
        }
    }

    /// Writes the reporter script and declares one reporter per name.
    fn identity_reporters(root: &Path, names: &[&str]) -> (std::path::PathBuf, Vec<Reporter>) {
        let script = root.join("identity_reporter.py");
        std::fs::write(&script, IDENTITY_REPORTER).expect("write identity reporter");
        let reporters = names
            .iter()
            .map(|name| Reporter {
                pid_path: root.join(format!("{name}.pid")),
                inbox: root.join(format!("{name}-inbox")),
            })
            .collect();
        (script, reporters)
    }

    /// Sends one hook request from inside the managed PTY and returns its `ok`.
    ///
    /// Mirrors [`send_identity_hook`]'s contract so a call site only changes
    /// transport, never its assertion.
    async fn send_identity_hook_from(reporter: &Reporter, request: serde_json::Value) -> bool {
        static NEXT_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        let sequence = NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = reporter.inbox.join(format!("{sequence:06}"));
        let request_path = base.with_extension("req");
        let response_path = base.with_extension("res");
        let staged = base.with_extension("req.staged");
        let mut encoded = serde_json::to_vec(&request).expect("encode hook");
        encoded.push(b'\n');
        wait_for_directory(&reporter.inbox).await;
        std::fs::write(&staged, &encoded).expect("stage hook request");
        std::fs::rename(&staged, &request_path).expect("publish hook request");

        let answer = wait_until(
            &format!("in-PTY reporter answer to {}", request_path.display()),
            || async { std::fs::read(&response_path).ok() },
        )
        .await;
        serde_json::from_slice::<serde_json::Value>(&answer)
            .expect("decode hook response")
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    /// Waits until the reporter has created its inbox directory.
    async fn wait_for_directory(path: &Path) {
        wait_until(
            &format!("in-PTY reporter inbox {}", path.display()),
            || async { path.is_dir().then_some(()) },
        )
        .await;
    }

    async fn send_identity_hook(socket: &Path, request: serde_json::Value) -> bool {
        let stream = UnixStream::connect(socket).await.expect("connect hook");
        let (reader, mut writer) = stream.into_split();
        let mut encoded = serde_json::to_vec(&request).expect("encode hook");
        encoded.push(b'\n');
        writer.write_all(&encoded).await.expect("write hook");
        let mut response = String::new();
        BufReader::new(reader)
            .read_line(&mut response)
            .await
            .expect("read hook response");
        serde_json::from_str::<serde_json::Value>(&response)
            .expect("decode hook response")
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    async fn wait_for_pid_file(path: &Path) -> u32 {
        let contents = wait_until("the nested pid file", || async {
            std::fs::read_to_string(path).ok()
        })
        .await;
        contents.trim().parse().expect("nested pid")
    }

    fn process_start_identity(pid: u32) -> u64 {
        HostInspector::new()
            .identity(pid)
            .expect("inspect process identity")
            .expect("process is live")
            .start_identity
            .get()
    }

    #[test]
    fn reconciliation_imports_immutable_launch_and_nested_active_identity() {
        let mut record = identity_record();
        let snapshot = identity_snapshot("native-launch");

        let active =
            import_worker_identities(&mut record, &snapshot).expect("import worker identities");
        assert_eq!(
            record.info.native_session_id.as_deref(),
            Some("native-launch")
        );
        assert_eq!(
            record
                .recovery
                .as_ref()
                .and_then(|binding| binding.native_session_id.as_deref()),
            Some("native-launch")
        );
        assert_eq!(record.info.active_agent.as_deref(), Some("claude"));
        assert_eq!(record.info.active_agent_base, Some(RuntimeRef::claude()));
        assert_eq!(record.info.active_agent_pid, Some(60));
        assert_eq!(
            record.info.active_agent_session_id.as_deref(),
            Some("nested-native")
        );
        assert_eq!(active.active.expect("active report").pid, Some(60));

        let conflict =
            import_worker_identities(&mut record, &identity_snapshot("different-launch"))
                .expect_err("immutable launch identity conflict");
        assert_eq!(conflict, "launch_identity_reference_mismatch");
        assert_eq!(
            record.info.native_session_id.as_deref(),
            Some("native-launch"),
            "conflicting hook must not replace the accepted launch identity"
        );
    }

    /// How one stored recovery binding relates to a worker's launch identity.
    struct LaunchIdentityCase {
        name: &'static str,
        /// Edits the record before the worker's launch identity is imported.
        prepare: fn(&mut SessionRecord),
        /// Edits the identity the worker reports.
        report: fn(&mut ReportedLaunchIdentity),
        /// The rejection reason, or `None` when the identity is accepted.
        expected: Option<&'static str>,
    }

    fn no_spec(record: &mut SessionRecord) {
        record
            .recovery
            .as_mut()
            .expect("recovery binding")
            .native_launch = None;
    }
    fn no_spec_stored_id(record: &mut SessionRecord) {
        no_spec(record);
        record.info.native_session_id = Some("native-launch".to_owned());
        record
            .recovery
            .as_mut()
            .expect("recovery binding")
            .native_session_id = Some("native-launch".to_owned());
    }
    fn no_spec_stored_other_id(record: &mut SessionRecord) {
        no_spec_stored_id(record);
        record.info.native_session_id = Some("native-stored".to_owned());
        record
            .recovery
            .as_mut()
            .expect("recovery binding")
            .native_session_id = Some("native-stored".to_owned());
    }
    fn no_spec_stored_path(record: &mut SessionRecord) {
        no_spec(record);
        record.info.native_session_path = Some("/work/stored.jsonl".to_owned());
        record
            .recovery
            .as_mut()
            .expect("recovery binding")
            .native_session_path = Some("/work/stored.jsonl".to_owned());
    }
    fn path_spec(record: &mut SessionRecord) {
        record
            .recovery
            .as_mut()
            .expect("recovery binding")
            .native_launch = Some(test_native_launch(SessionRefKind::Path, false));
    }
    fn no_recovery(record: &mut SessionRecord) {
        record.recovery = None;
    }
    fn unchanged(_: &mut ReportedLaunchIdentity) {}
    fn other_provider(identity: &mut ReportedLaunchIdentity) {
        identity.provider = "claude".to_owned();
    }

    /// A binding whose launch spec the migration could not restore carries no
    /// reference kind; the worker's launch identity then decides nothing about
    /// the binding, and only a stored reference that contradicts it is a
    /// conflict.
    #[test]
    fn launch_identity_conflicts_only_on_a_real_contradiction() {
        let cases = [
            LaunchIdentityCase {
                name: "no spec, nothing stored",
                prepare: no_spec,
                report: unchanged,
                expected: None,
            },
            LaunchIdentityCase {
                name: "no spec, the same id stored",
                prepare: no_spec_stored_id,
                report: unchanged,
                expected: None,
            },
            LaunchIdentityCase {
                name: "no spec, another id stored",
                prepare: no_spec_stored_other_id,
                report: unchanged,
                expected: Some("launch_identity_reference_mismatch"),
            },
            LaunchIdentityCase {
                name: "no spec, a path stored for an id report",
                prepare: no_spec_stored_path,
                report: unchanged,
                expected: Some("launch_identity_reference_kind_mismatch"),
            },
            LaunchIdentityCase {
                name: "a path spec contradicts an id report",
                prepare: path_spec,
                report: unchanged,
                expected: Some("launch_identity_reference_kind_mismatch"),
            },
            LaunchIdentityCase {
                name: "no spec, another provider",
                prepare: no_spec,
                report: other_provider,
                expected: Some("launch_identity_provider_mismatch"),
            },
            LaunchIdentityCase {
                name: "no recovery binding",
                prepare: no_recovery,
                report: unchanged,
                expected: Some("launch_identity_recovery_missing"),
            },
        ];
        for case in cases {
            let mut record = identity_record();
            (case.prepare)(&mut record);
            let mut snapshot = identity_snapshot("native-launch");
            snapshot.active_identity = None;
            (case.report)(snapshot.launch_identity.as_mut().expect("launch identity"));

            let outcome = import_worker_identities(&mut record, &snapshot);

            assert_eq!(
                outcome.as_ref().err().copied(),
                case.expected,
                "{}",
                case.name
            );
            if case.expected.is_none() {
                assert_eq!(
                    record.info.native_session_id.as_deref(),
                    Some("native-launch"),
                    "{}: the reported reference is recorded",
                    case.name
                );
                assert_eq!(
                    record
                        .recovery
                        .as_ref()
                        .and_then(|binding| binding.native_session_id.as_deref()),
                    Some("native-launch"),
                    "{}: the binding keeps the reference",
                    case.name
                );
            }
        }
    }

    #[test]
    fn reconciliation_rejects_stale_or_overlong_active_identity_and_clears_release() {
        assert_eq!(
            protocol::MAX_IDENTITY_CLAIM_TTL_SECS,
            pohunek_worker_protocol::MAX_IDENTITY_CLAIM_TTL_SECS
        );
        let mut expired = identity_snapshot("native-launch");
        expired
            .active_identity
            .as_mut()
            .expect("active identity")
            .expires_at = (OffsetDateTime::now_utc() - time::Duration::seconds(1))
            .format(&Rfc3339)
            .expect("format expiry");
        assert_eq!(
            import_worker_identities(&mut identity_record(), &expired)
                .expect_err("expired identity"),
            "active_identity_expired_or_overlong"
        );

        let mut overlong = identity_snapshot("native-launch");
        overlong
            .active_identity
            .as_mut()
            .expect("active identity")
            .expires_at = (OffsetDateTime::now_utc()
            + time::Duration::seconds(
                i64::try_from(protocol::MAX_IDENTITY_CLAIM_TTL_SECS).expect("TTL fits i64") + 1,
            ))
        .format(&Rfc3339)
        .expect("format expiry");
        assert_eq!(
            import_worker_identities(&mut identity_record(), &overlong)
                .expect_err("overlong identity"),
            "active_identity_expired_or_overlong"
        );

        let mut record = identity_record();
        import_worker_identities(&mut record, &identity_snapshot("native-launch"))
            .expect("initial active identity");
        let mut released = identity_snapshot("native-launch");
        released.active_identity = None;
        let unreported = import_worker_identities(&mut record, &released).expect("empty identity");
        assert!(!unreported.privately_reported);
        assert_eq!(record.info.active_agent.as_deref(), Some("claude"));
        released.active_identity_release = Some(ReleasedIdentityClaim {
            provider: "claude".to_owned(),
            process: ProcessIdentity {
                pid: 60,
                start_identity: 600,
            },
            sequence: 8,
        });
        let projection =
            import_worker_identities(&mut record, &released).expect("released identity");
        assert!(projection.privately_reported);
        assert!(projection.active.is_none());
        assert_eq!(projection.release.expect("release").sequence, 8);
        assert!(record.info.active_agent.is_none());
        assert!(record.info.active_agent_base.is_none());
        assert!(record.info.active_agent_pid.is_none());
        assert!(record.info.active_agent_session_id.is_none());
        assert!(record.info.active_agent_session_path.is_none());
    }

    #[test]
    fn invalid_worker_snapshot_does_not_partially_mutate_record() {
        let mut record = identity_record();
        let original = record.clone();
        let mut snapshot = identity_snapshot("new-native-launch");
        snapshot
            .active_identity
            .as_mut()
            .expect("active identity")
            .reference_kind = Some("invalid".to_owned());

        assert_eq!(
            import_worker_identities(&mut record, &snapshot).expect_err("invalid active reference"),
            "active_identity_reference_kind_invalid"
        );
        assert_eq!(record, original);
    }

    #[test]
    fn restart_identity_requires_current_descendant_pid_and_start_identity() {
        let snapshot = identity_snapshot("native-launch");
        let root = crate::procwatch::ProcessFact {
            pid: 50,
            pgid: 50,
            ppid: 1,
            start_identity: crate::procwatch::StartIdentity::new(500),
            comm: "codex".to_owned(),
            cmdline: vec!["codex".to_owned()],
        };
        let descendant = crate::procwatch::ProcessFact {
            pid: 60,
            pgid: 50,
            ppid: 50,
            start_identity: crate::procwatch::StartIdentity::new(600),
            comm: "claude".to_owned(),
            cmdline: vec!["claude".to_owned()],
        };
        validate_worker_identity_process_facts(&snapshot, &root, std::slice::from_ref(&descendant))
            .expect("current descendant identity");

        let reused = crate::procwatch::ProcessFact {
            start_identity: crate::procwatch::StartIdentity::new(601),
            ..descendant
        };
        assert_eq!(
            validate_worker_identity_process_facts(&snapshot, &root, &[reused])
                .expect_err("reused descendant pid"),
            "active_identity_process_invalid"
        );

        let mut released = snapshot;
        let active = released.active_identity.take().expect("active identity");
        released.active_identity_release = Some(ReleasedIdentityClaim {
            provider: active.provider,
            process: active.process,
            sequence: active.sequence + 1,
        });
        validate_worker_identity_process_facts(&released, &root, &[])
            .expect("validated release remains history after its process exits");
    }

    #[test]
    fn identities_outside_the_pty_tree_are_refused_under_every_schema_resolution() {
        let root = crate::procwatch::ProcessFact {
            pid: 50,
            pgid: 50,
            ppid: 1,
            start_identity: crate::procwatch::StartIdentity::new(500),
            comm: "codex".to_owned(),
            cmdline: vec!["codex".to_owned()],
        };
        // The active identity (pid 60) is outside the tree: the root has no
        // descendants. The launch identity is the root itself.
        for schema in [
            Some("identity-subagent-v1"),
            Some("identity-v1"),
            Some("identity-v9"),
            None,
        ] {
            let mut snapshot = identity_snapshot("native-launch");
            snapshot.hook_schema = schema.map(str::to_owned);
            assert_eq!(
                validate_worker_identity_process_facts(&snapshot, &root, &[])
                    .expect_err("an active identity outside the PTY tree"),
                "active_identity_process_invalid",
                "{schema:?}"
            );
            let mut launch = identity_snapshot("native-launch");
            launch.hook_schema = schema.map(str::to_owned);
            launch.active_identity = None;
            launch
                .launch_identity
                .as_mut()
                .expect("launch identity")
                .process = ProcessIdentity {
                pid: 61,
                start_identity: 610,
            };
            assert_eq!(
                validate_worker_identity_process_facts(&launch, &root, &[])
                    .expect_err("a launch identity outside the PTY tree"),
                "launch_identity_process_invalid",
                "{schema:?}"
            );
        }
    }

    #[test]
    fn attach_snapshot_capability_error_is_not_classified_as_worker_loss() {
        let error = crate::runtime::WorkerError::AttachSnapshotUnsupported {
            selected_version: pohunek_worker_protocol::PREVIOUS_VERSION,
        };

        assert_eq!(
            super::classify_connect_error(&error),
            (RuntimeState::Incompatible, "attach_snapshot_unsupported")
        );
    }

    /// Frozen schema-4 layout: the next schema bump keeps it readable as N-1
    /// by decoding exactly these fields.
    const SCHEMA_FOUR_JOURNAL: &str = r#"{
        "schema_version": 4, "session_id": "s-1", "worker_id": "worker-1",
        "executable": "/opt/pohunek/libexec/pohunek/0.1.0/pohunek-sessiond",
        "version": "0.1.0", "generation": "abcd2345", "runtime_id": "runtime-1",
        "protocol_minimum": 5, "protocol_maximum": 6, "worker_pid": 41,
        "worker_start_identity": "410", "boot_identity": "boot-test",
        "child": {"pid": 50}, "cols": 80, "rows": 24, "phase": "live",
        "outcome": null, "subagents": []
    }"#;

    #[test]
    fn frozen_schema_four_layout_decodes_as_the_previous_schema_after_a_bump() {
        use crate::runtime::lifecycle::{schema_in_window, WORKER_JOURNAL_READABLE_SCHEMAS};

        let evidence = serde_json::from_str::<super::JournalEvidence>(SCHEMA_FOUR_JOURNAL)
            .expect("the daemon reads the schema-4 layout");
        assert_eq!(evidence.generation, "abcd2345");
        assert_eq!(evidence.phase, super::JournalPhase::Live);
        assert!(schema_in_window(4, 5, &[4, 5]));
        assert!(!schema_in_window(3, 4, WORKER_JOURNAL_READABLE_SCHEMAS));
        assert!(!schema_in_window(4, 6, &[4, 5, 6]));
    }

    /// The record of an assigned runtime whose worker instance is
    /// `runtime-identity`, holding `assigned-ref`.
    fn ended_assigned_record() -> SessionRecord {
        assigned_record("assigned-ref")
    }

    #[test]
    fn an_ended_worker_hands_its_journaled_reference_over_only_when_it_proves_its_binding() {
        let reported = |snapshot: &mut InspectSnapshot, pid: u32| {
            snapshot.native_reference = Some(pohunek_worker_protocol::ReportedNativeReference {
                provider: "codex".to_owned(),
                process: ProcessIdentity {
                    pid,
                    start_identity: u64::from(pid) * 10,
                },
                sequence: 8,
                reference_kind: "id".to_owned(),
                native_reference: "journaled-ref".to_owned(),
            });
        };
        let held = |record: &SessionRecord| record.info.native_session_id.clone();

        let mut snapshot = switch_snapshot(Some("assigned-ref"), None);
        reported(&mut snapshot, 50);
        let mut record = ended_assigned_record();
        super::import_native_reference(&mut record, &snapshot);
        assert_eq!(held(&record).as_deref(), Some("journaled-ref"));
        assert_eq!(provenance_of(&record), NativeReferenceProvenance::Reported);

        // Another worker instance, a reference of a process that is not the
        // verified launch process, and a missing launch claim prove nothing.
        let mut other_instance = snapshot.clone();
        other_instance.worker_instance_id =
            Some(WorkerInstanceId::new("another-instance").expect("instance id"));
        let mut nested = switch_snapshot(Some("assigned-ref"), None);
        reported(&mut nested, 60);
        let mut unverified = snapshot.clone();
        unverified.launch_identity = None;
        for (name, snapshot) in [
            ("another instance", other_instance),
            ("not the launch process", nested),
            ("no verified launch", unverified),
        ] {
            let mut record = ended_assigned_record();
            super::import_native_reference(&mut record, &snapshot);
            assert_eq!(held(&record).as_deref(), Some("assigned-ref"), "{name}");
        }
        let mut no_instance = ended_assigned_record();
        no_instance.runtime.worker_instance_id = None;
        super::import_native_reference(&mut no_instance, &snapshot);
        assert_eq!(held(&no_instance).as_deref(), Some("assigned-ref"));
    }

    /// The terminal journal of the assigned runtime's worker, whose launch
    /// process reported `journaled-ref` after the launch claim named
    /// `assigned-ref`.
    fn ended_evidence(record: &SessionRecord) -> super::JournalEvidence {
        super::JournalEvidence {
            schema_version: crate::runtime::lifecycle::WORKER_JOURNAL_SCHEMA_VERSION,
            session_id: record.session_id.clone(),
            worker_id: record.runtime.worker_id.clone().expect("worker id"),
            generation: "abcd2345".to_owned(),
            executable: PathBuf::from("/usr/libexec/pohunek-sessiond"),
            worker_pid: 41,
            worker_start_identity: "410".to_owned(),
            boot_identity: "boot-test".to_owned(),
            worker_spawn_id: None,
            worker_instance_id: record.runtime.worker_instance_id.clone(),
            child: Some(super::JournalChild {
                pid: 50,
                start_identity: "500".to_owned(),
            }),
            cols: Some(80),
            rows: Some(24),
            phase: super::JournalPhase::Terminal,
            outcome: Some(super::JournalOutcome {
                exit_code: Some(0),
                signal: None,
                success: true,
            }),
            subagents: Vec::new(),
            hook_schema: None,
            protocol_minimum: None,
            protocol_maximum: None,
            launch_identity: serde_json::from_value(serde_json::json!({
                "provider": "codex", "reference_kind": "id",
                "native_reference": "assigned-ref",
                "process": {"pid": 50, "start_identity": "500"}
            }))
            .expect("launch identity"),
            active_identity: None,
            active_identity_release: None,
            native_reference_claim: serde_json::from_value(serde_json::json!({
                "provider": "codex", "sequence": 8, "reference_kind": "id",
                "native_reference": "journaled-ref",
                "process": {"pid": 50, "start_identity": "500"}
            }))
            .expect("native reference"),
        }
    }

    #[test]
    fn a_terminal_journal_carries_the_journaled_reference_into_its_snapshot() {
        let record = ended_assigned_record();
        let snapshot = ended_evidence(&record)
            .snapshot()
            .expect("a readable journal");
        let reference = snapshot.native_reference.expect("journaled reference");
        assert_eq!(reference.native_reference, "journaled-ref");
        assert_eq!(reference.sequence, 8);
    }

    #[tokio::test]
    async fn a_terminal_journal_import_records_the_journaled_reference() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: crate::session::ShellCommand::new("/bin/sh", ["-c", "sleep 1"]),
            ..SessionRegistryConfig::default()
        });
        let record = ended_assigned_record();
        let id = SessionId(record.session_id.clone());
        assert!(
            registry
                .import_terminal_journal(record.clone(), ended_evidence(&record))
                .await
        );
        let info = registry
            .inspect(&id)
            .await
            .expect("inspect the ended session");
        assert_eq!(info.native_session_id.as_deref(), Some("journaled-ref"));
    }

    /// The terminal journal of a previous-release worker: it keeps the
    /// conversation switch only as its leased active identity claim and
    /// journals no separate native reference.
    fn legacy_evidence(record: &SessionRecord) -> super::JournalEvidence {
        let mut legacy = ended_evidence(record);
        legacy.native_reference_claim = None;
        legacy.active_identity = Some(
            serde_json::from_value(serde_json::json!({
                "provider": "codex", "sequence": 8, "reference_kind": "id",
                "native_reference": "switched-before-the-stop",
                "expires_at": (OffsetDateTime::now_utc() + time::Duration::seconds(30))
                    .format(&Rfc3339)
                    .expect("format identity expiry"),
                "process": {"pid": 50, "start_identity": "500"}
            }))
            .expect("active identity"),
        );
        legacy
    }

    /// A previous-release journal holds the switch as the active claim, so
    /// the terminal import keeps it the way the live identity import would.
    #[tokio::test]
    async fn a_terminal_legacy_journal_keeps_the_switched_conversation() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: crate::session::ShellCommand::new("/bin/sh", ["-c", "sleep 1"]),
            ..SessionRegistryConfig::default()
        });
        let record = ended_assigned_record();
        let id = SessionId(record.session_id.clone());
        assert!(
            registry
                .import_terminal_journal(record.clone(), legacy_evidence(&record))
                .await
        );
        let info = registry
            .inspect(&id)
            .await
            .expect("inspect the ended session");
        assert_eq!(
            info.native_session_id.as_deref(),
            Some("switched-before-the-stop")
        );
    }

    /// The active-claim fallback is as strict as the journaled reference: a
    /// foreign process and a sequence older than the accepted report never
    /// promote the recovery target, and the same shape with a current
    /// sequence does.
    #[test]
    fn a_terminal_legacy_journal_never_imports_a_claim_that_fails_its_proofs() {
        let schema = Ok(pohunek_worker_protocol::hook_schema("identity-subagent-v1"));
        let claim_at = |pid: u32, sequence: u64| {
            let mut snapshot = legacy_evidence(&ended_assigned_record())
                .snapshot()
                .expect("a readable journal");
            snapshot.active_identity = Some(pohunek_worker_protocol::ActiveIdentityClaim {
                provider: "codex".to_owned(),
                process: ProcessIdentity {
                    pid,
                    start_identity: u64::from(pid) * 10,
                },
                sequence,
                expires_at: snapshot
                    .active_identity
                    .as_ref()
                    .map(|claim| claim.expires_at.clone())
                    .expect("an expiry"),
                reference_kind: Some("id".to_owned()),
                native_reference: Some("switched-before-the-stop".to_owned()),
            });
            snapshot
        };

        // A claim of another process than the verified launch never imports.
        let mut foreign = ended_assigned_record();
        super::import_terminal_native_reference(&mut foreign, &claim_at(999, 8), schema);
        assert_eq!(
            foreign.info.native_session_id.as_deref(),
            Some("assigned-ref"),
            "a foreign process claim never promotes the recovery target"
        );

        // A record the launch process already reported at sequence 8 rejects
        // a delayed claim of sequence 3 and adopts sequence 9.
        let mut stale = ended_assigned_record();
        stale.native_identity_ordering = Some(crate::store::NativeIdentityOrdering::accepting(
            None,
            "runtime-identity",
            50,
            500,
            crate::store::ReportTransport::Worker,
            8,
        ));
        super::import_terminal_native_reference(&mut stale, &claim_at(50, 3), schema);
        assert_eq!(
            stale.info.native_session_id.as_deref(),
            Some("assigned-ref"),
            "a delayed claim never replaces the accepted report"
        );
        super::import_terminal_native_reference(&mut stale, &claim_at(50, 9), schema);
        assert_eq!(
            stale.info.native_session_id.as_deref(),
            Some("switched-before-the-stop"),
            "a newer claim of the verified launch process promotes the target"
        );
    }

    #[test]
    fn terminal_journal_identity_mismatch_and_duplicates_fail_closed() {
        let record = identity_record();
        let exact = super::JournalEvidence {
            schema_version: crate::runtime::lifecycle::WORKER_JOURNAL_SCHEMA_VERSION,
            session_id: record.session_id.clone(),
            worker_id: record.runtime.worker_id.clone().expect("worker id"),
            generation: "abcd2345".to_owned(),
            executable: PathBuf::from("/usr/libexec/pohunek-sessiond"),
            worker_pid: 41,
            worker_start_identity: "410".to_owned(),
            boot_identity: "boot-test".to_owned(),
            worker_spawn_id: None,
            worker_instance_id: record.runtime.worker_instance_id.clone(),
            child: Some(super::JournalChild {
                pid: 50,
                start_identity: "500".to_owned(),
            }),
            cols: Some(80),
            rows: Some(24),
            phase: super::JournalPhase::Terminal,
            outcome: Some(super::JournalOutcome {
                exit_code: Some(0),
                signal: None,
                success: true,
            }),
            subagents: Vec::new(),
            hook_schema: None,
            protocol_minimum: None,
            protocol_maximum: None,
            launch_identity: None,
            active_identity: None,
            active_identity_release: None,
            native_reference_claim: None,
        };
        let mut mismatch = exact.clone();
        mismatch.worker_id = "different-worker".to_owned();

        assert!(matches!(
            super::classify_terminal_journals(
                &super::WorkerJournalScan {
                    evidence: vec![mismatch],
                    ..Default::default()
                },
                &record
            ),
            super::TerminalJournalClassification::Conflict
        ));
        assert!(matches!(
            super::classify_terminal_journals(
                &super::WorkerJournalScan {
                    evidence: vec![exact.clone(), exact],
                    ..Default::default()
                },
                &record
            ),
            super::TerminalJournalClassification::Conflict
        ));
    }

    #[test]
    fn journal_scan_marks_malformed_and_identity_mismatched_managed_files_as_conflicts() {
        let root = temp_root();
        let state_root = root.join("workers");
        let session_dir = state_root.join("s-91");
        std::fs::create_dir_all(&session_dir).expect("create journal directory");
        for path in [&state_root, &session_dir] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .expect("secure journal directory");
        }
        let mut journal = JournalRecord::bootstrap(
            "s-91".to_owned(),
            "worker-valid".to_owned(),
            pohunek_session_worker::WorkerOrigin {
                executable: PathBuf::from("/usr/libexec/pohunek-sessiond"),
                version: "0.0.0-test".to_owned(),
                generation: "abcd2345".to_owned(),
            },
            41,
            "410".to_owned(),
            "boot-test".to_owned(),
            (1, 2),
            "2026-09-20T00:00:00Z".to_owned(),
        );
        journal.phase = JournalRuntimePhase::Terminal;
        Journal::new(session_dir.join("worker-valid.json"))
            .write(&journal)
            .expect("write valid journal");
        let valid_bytes = std::fs::read(session_dir.join("worker-valid.json"))
            .expect("read valid journal fixture");
        for (name, bytes) in [
            ("worker-valid.json.tmp", valid_bytes.as_slice()),
            ("worker-malformed.json", b"not json".as_slice()),
            ("worker-other.json", valid_bytes.as_slice()),
        ] {
            let path = session_dir.join(name);
            std::fs::write(&path, bytes).expect("write ignored journal fixture");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("secure ignored journal fixture");
        }

        let journals = super::scan_worker_journals(&state_root).expect("scan journals");
        let accepted = journals
            .get("s-91")
            .expect("valid session journal accepted");
        assert_eq!(accepted.evidence.len(), 1);
        assert_eq!(accepted.evidence[0].worker_id, "worker-valid");
        assert!(accepted.conflict);
        assert!(matches!(
            super::classify_terminal_journals(
                &super::WorkerJournalScan {
                    evidence: accepted.evidence.clone(),
                    conflict: accepted.conflict,
                    rejected: accepted.rejected.clone(),
                },
                &identity_record()
            ),
            super::TerminalJournalClassification::Conflict
        ));
    }

    #[test]
    fn same_provider_nested_identity_keeps_launch_reference_immutable() {
        for (provider, agent_base, kind, launch_reference, nested_reference) in [
            (
                "codex",
                RuntimeRef::codex(),
                SessionRefKind::Id,
                "codex-launch",
                "codex-nested",
            ),
            (
                "claude",
                RuntimeRef::claude(),
                SessionRefKind::Path,
                "/work/claude-launch.jsonl",
                "/work/claude-nested.jsonl",
            ),
            (
                "hermes",
                RuntimeRef::hermes(),
                SessionRefKind::Id,
                "hermes-launch",
                "hermes-nested",
            ),
        ] {
            let mut record = identity_record();
            record.info.agent = provider.to_owned();
            record.info.agent_base = agent_base.clone();
            let binding = record.recovery.as_mut().expect("recovery");
            binding.agent = provider.to_owned();
            binding.agent_base = agent_base;
            binding.native_launch = Some(test_native_launch(kind, false));
            let mut snapshot = identity_snapshot(launch_reference);
            snapshot.launch_identity.as_mut().expect("launch").provider = provider.to_owned();
            snapshot
                .launch_identity
                .as_mut()
                .expect("launch")
                .reference_kind = match kind {
                SessionRefKind::Id => "id".to_owned(),
                SessionRefKind::Path => "path".to_owned(),
            };
            let active = snapshot.active_identity.as_mut().expect("active");
            active.provider = provider.to_owned();
            active.reference_kind = Some(match kind {
                SessionRefKind::Id => "id".to_owned(),
                SessionRefKind::Path => "path".to_owned(),
            });
            active.native_reference = Some(nested_reference.to_owned());

            import_worker_identities(&mut record, &snapshot).expect("import same-provider nesting");

            match kind {
                SessionRefKind::Id => {
                    assert_eq!(
                        record.info.native_session_id.as_deref(),
                        Some(launch_reference)
                    );
                    assert_eq!(
                        record.info.active_agent_session_id.as_deref(),
                        Some(nested_reference)
                    );
                }
                SessionRefKind::Path => {
                    assert_eq!(
                        record.info.native_session_path.as_deref(),
                        Some(launch_reference)
                    );
                    assert_eq!(
                        record.info.active_agent_session_path.as_deref(),
                        Some(nested_reference)
                    );
                }
            }
        }
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "startup retry test owns one complete worker adoption lifecycle fixture"
    )]
    async fn startup_retries_transient_identity_validation_after_adopting_worker() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let session_id = "s-209";
        let worker_id = "worker-startup-identity-retry";
        let (controller, worker_instance_id, child_pid, server_task, reporter) =
            spawn_initialized_worker_with_reporter(&root, &runtime_root, session_id, worker_id)
                .await;
        let child = controller
            .inspect()
            .await
            .expect("inspect initialized worker")
            .child_process
            .expect("worker child identity");
        let expires_at = (OffsetDateTime::now_utc() + time::Duration::seconds(30))
            .format(&Rfc3339)
            .expect("format identity expiry");
        assert!(
            send_identity_hook_from(
                &reporter,
                serde_json::json!({
                    "type": "identity_report",
                    "runtime_id": worker_instance_id.as_str(),
                    "provider": "codex",
                    "pid": child.pid,
                    "start_identity": child.start_identity,
                    "sequence": 1,
                    "expires_at": expires_at,
                    "reference_kind": null,
                    "native_reference": null
                }),
            )
            .await,
            "worker accepts a report from inside the managed PTY"
        );
        assert!(
            controller
                .inspect()
                .await
                .expect("inspect reported identity")
                .active_identity
                .is_some(),
            "worker retains the active identity for startup reconciliation"
        );

        let mut record = identity_record();
        record.session_id = session_id.to_owned();
        record.info.id = SessionId(session_id.to_owned());
        record.info.agent = "shell".to_owned();
        record.info.agent_base = RuntimeRef::shell();
        record.info.cwd = root.clone();
        record.info.pid = child_pid;
        let info_runtime = record.info.runtime.as_mut().expect("runtime info");
        info_runtime.worker_id = Some(worker_id.to_owned());
        info_runtime.worker_instance_id = Some(worker_instance_id.to_string());
        record.runtime.worker_id = Some(worker_id.to_owned());
        record.runtime.worker_instance_id = Some(worker_instance_id.to_string());
        let recovery = record.recovery.as_mut().expect("recovery binding");
        recovery.session_id = session_id.to_owned();
        recovery.agent = "shell".to_owned();
        recovery.agent_base = RuntimeRef::shell();
        recovery.cwd = root.clone();
        recovery.program = "/bin/sh".to_owned();
        recovery.args = vec!["-c".to_owned(), "sleep 30".to_owned()];
        let store_path = root.join("data/metadata.jsonl");
        bind_test_generation(&mut record);
        Store::new(store_path.clone())
            .record_session(&record)
            .expect("persist logical record");
        drop(controller);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let inspector = Arc::new(RetryInspector::default());
        inspector.fail_descendants(true);
        let registry_inspector: Arc<dyn ProcessInspector> =
            Arc::<RetryInspector>::clone(&inspector);
        let replacement = SessionRegistry::new_with_inspector(
            SessionRegistryConfig {
                shell_command: hermetic_shell(),
                store_path: Some(store_path),
                worker_runtime_root: Some(runtime_root),
                worker_state_root: Some(root.join("state/workers")),
                worker_connect_deadline: Duration::from_millis(300),
                procwatch_poll: Duration::from_secs(60),
                ..SessionRegistryConfig::default()
            },
            registry_inspector,
        );

        Box::pin(replacement.reconcile_workers())
            .await
            .expect("adopt worker with deferred identity validation");
        let adopted = replacement
            .inspect(&SessionId(session_id.to_owned()))
            .await
            .expect("worker remains available during identity retry");
        assert_eq!(
            adopted.runtime.expect("runtime").state,
            RuntimeState::Live,
            "transient optional identity validation must not quarantine a live worker"
        );
        assert!(adopted.active_agent.is_none());
        replacement
            .inner
            .sessions
            .lock()
            .await
            .get(&SessionId(session_id.to_owned()))
            .expect("adopted session entry")
            .procwatch_cancel
            .cancel();

        inspector.fail_descendants(false);
        let deadline = tokio::time::Instant::now() + HANG_GUARD;
        loop {
            let retried = replacement
                .inspect(&SessionId(session_id.to_owned()))
                .await
                .expect("inspect retried identity");
            if retried.active_agent.as_deref() == Some("codex") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "startup watcher did not retry the deferred identity after {} validation calls",
                inspector.descendant_calls()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        replacement
            .stop(&SessionId(session_id.to_owned()))
            .await
            .expect("stop adopted worker");
        server_task.abort();
    }

    /// The worker binds a hook report to the process it names.
    ///
    /// A report is accepted from the subject itself and from a process it
    /// spawned — the shapes the pinned Hermes plugin and the Codex/Claude child
    /// hooks actually have — and rejected from a same-session sibling or from a
    /// process outside the managed PTY entirely. The `POHUNEK_*` environment the
    /// worker injects into the PTY root makes the sibling case reachable by any
    /// command the operator runs in that terminal, which is why it is covered
    /// here and not only in theory.
    #[tokio::test]
    async fn identity_reports_are_bound_to_the_process_they_name() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let session_id = "s-211";
        let worker_id = "worker-identity-subject-binding";
        let (controller, worker_instance_id, _child_pid, server_task, reporters) =
            spawn_initialized_worker_with_reporters(
                &root,
                &runtime_root,
                session_id,
                worker_id,
                &["first", "second"],
            )
            .await;
        let socket = controller.socket_path().to_path_buf();
        let child = controller
            .inspect()
            .await
            .expect("inspect initialized worker")
            .child_process
            .expect("worker child identity");
        let first = reporters[0].clone();
        let second = reporters[1].clone();
        let second_pid = wait_for_pid_file(&second.pid_path).await;
        let second_start = process_start_identity(second_pid);

        let report = |pid: u32, start_identity: u64, sequence: u64| {
            let expires_at = (OffsetDateTime::now_utc() + time::Duration::seconds(30))
                .format(&Rfc3339)
                .expect("format identity expiry");
            serde_json::json!({
                "type": "identity_report",
                "runtime_id": worker_instance_id.as_str(),
                "provider": "claude",
                "pid": pid,
                "start_identity": start_identity,
                "sequence": sequence,
                "expires_at": expires_at,
                "reference_kind": "id",
                "native_reference": "subject-binding"
            })
        };

        assert!(
            !send_identity_hook(&socket, report(child.pid, child.start_identity, 1)).await,
            "a process outside the managed PTY cannot report an identity in it"
        );
        assert!(
            !send_identity_hook_from(&first, report(second_pid, second_start, 2)).await,
            "a same-session sibling cannot report another process's identity"
        );
        assert!(
            send_identity_hook_from(&second, report(second_pid, second_start, 3)).await,
            "a process may report its own identity"
        );
        assert!(
            send_identity_hook_from(&first, report(child.pid, child.start_identity, 4)).await,
            "a child hook may report the agent that spawned it"
        );

        assert_eq!(
            controller
                .inspect()
                .await
                .expect("inspect reported identity")
                .active_identity
                .map(|identity| identity.process.pid),
            Some(child.pid),
            "only the accepted reports may reach the journal"
        );

        drop(controller);
        server_task.abort();
    }

    // Linux keeps the session's terminal usable for descendants after the
    // session leader exits; XNU revokes it (`proc_exit`), so this drain
    // behavior exists only on Linux. The worker's Darwin counterpart is
    // `root_exit_revokes_the_terminal_and_stop_still_ends_the_group`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "draining restart test owns the worker, durable identity, and output barriers"
    )]
    async fn replacement_registry_adopts_native_identity_while_worker_drains_pty() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let session_id = "s-210";
        let worker_id = "worker-draining-restart";
        let root_release = root.join("root-release");
        let root_exited = root.join("root-exited");
        let descendant_release = root.join("descendant-release");
        let _release_files = ReleaseFiles(vec![root_release.clone(), descendant_release.clone()]);
        let (script, mut reporters) = identity_reporters(&root, &["agent"]);
        let reporter = reporters.pop().expect("one reporter");
        let command = format!(
            concat!(
                "trap '' HUP; root_pid=$$; {} (",
                "while [ \"$(sed -n 's/^.*) \\([^ ]\\).*/\\1/p' \"/proc/$root_pid/stat\" 2>/dev/null)\" != Z ]; do sleep 0.01; done; ",
                "printf exited > '{}'; ",
                "while [ ! -e '{}' ]; do sleep 0.01; done; ",
                "printf 'late-restart-output\\n') & ",
                "while [ ! -e '{}' ]; do sleep 0.01; done"
            ),
            reporter.launch(&script),
            root_exited.display(),
            descendant_release.display(),
            root_release.display(),
        );
        let (controller, worker_instance_id, child_pid, server_task) =
            spawn_initialized_worker_with_command(
                &root,
                &runtime_root,
                session_id,
                worker_id,
                command,
            )
            .await;
        wait_for_directory(&reporter.inbox).await;
        let child = controller
            .inspect()
            .await
            .expect("inspect initialized worker")
            .child_process
            .expect("worker child identity");
        let expires_at = (OffsetDateTime::now_utc() + time::Duration::seconds(30))
            .format(&Rfc3339)
            .expect("format identity expiry");
        assert!(
            send_identity_hook_from(
                &reporter,
                serde_json::json!({
                    "type": "identity_report",
                    "runtime_id": worker_instance_id.as_str(),
                    "provider": "codex",
                    "pid": child.pid,
                    "start_identity": child.start_identity,
                    "sequence": 1,
                    "expires_at": expires_at,
                    "reference_kind": "id",
                    "native_reference": "native-before-drain"
                }),
            )
            .await,
            "worker accepts the durable native identity fixture from inside the PTY"
        );

        let mut record = identity_record();
        record.session_id = session_id.to_owned();
        record.info.id = SessionId(session_id.to_owned());
        record.info.agent = "shell".to_owned();
        record.info.agent_base = RuntimeRef::shell();
        record.info.cwd = root.clone();
        record.info.pid = child_pid;
        record.info.active_agent = Some("codex".to_owned());
        record.info.active_agent_base = Some(RuntimeRef::codex());
        record.info.active_agent_pid = Some(child_pid);
        record.info.active_agent_session_id = Some("native-before-drain".to_owned());
        record.info.native_session_id = Some("native-before-drain".to_owned());
        let info_runtime = record.info.runtime.as_mut().expect("runtime info");
        info_runtime.worker_id = Some(worker_id.to_owned());
        info_runtime.worker_instance_id = Some(worker_instance_id.to_string());
        record.runtime.worker_id = Some(worker_id.to_owned());
        record.runtime.worker_instance_id = Some(worker_instance_id.to_string());
        record.native_identity_ordering = Some(NativeIdentityOrdering {
            worker_instance_id: worker_instance_id.to_string(),
            pid: child.pid,
            pid_start_identity: child.start_identity,
            sequence: Some(1),
            worker_sequence: None,
        });
        let recovery = record.recovery.as_mut().expect("recovery binding");
        recovery.session_id = session_id.to_owned();
        recovery.agent = "shell".to_owned();
        recovery.agent_base = RuntimeRef::shell();
        recovery.cwd = root.clone();
        recovery.program = "/bin/sh".to_owned();
        recovery.args = vec!["-c".to_owned(), "draining fixture".to_owned()];
        recovery.native_session_id = Some("native-before-drain".to_owned());
        let store_path = root.join("data/metadata.jsonl");
        bind_test_generation(&mut record);
        Store::new(store_path.clone())
            .record_session(&record)
            .expect("persist draining logical record");

        std::fs::write(&root_release, b"release").expect("release root process");
        wait_until("identity-safe root exit", || async {
            root_exited.exists().then_some(())
        })
        .await;
        drop(controller);

        let replacement = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store_path),
            worker_runtime_root: Some(runtime_root),
            worker_state_root: Some(root.join("state/workers")),
            procwatch_poll: Duration::from_secs(60),
            ..SessionRegistryConfig::default()
        });
        Box::pin(replacement.reconcile_workers())
            .await
            .expect("adopt worker while PTY drains");
        let adopted = replacement
            .inspect(&SessionId(session_id.to_owned()))
            .await
            .expect("draining worker remains public");
        assert_eq!(adopted.state, SessionState::Running);
        assert!(
            adopted.active_agent.is_none(),
            "a dead root cannot retain an active process claim"
        );
        assert_eq!(
            adopted.native_session_id.as_deref(),
            Some("native-before-drain"),
            "restart must preserve immutable durable native recovery metadata"
        );

        let output_params = protocol::SessionOutputParams::new(
            SessionId(session_id.to_owned()),
            Some(
                protocol::SessionRuntimeIdentity::new(
                    worker_instance_id.as_str(),
                    protocol::RuntimeGeneration::new(1),
                )
                .expect("runtime identity"),
            ),
            Some(protocol::OutputOffset::new(0)),
            4_096,
            Some(1_000),
        )
        .expect("output params");
        let output = replacement.output(&output_params);
        tokio::pin!(output);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut output)
                .await
                .is_err(),
            "output wait must be registered before descendant release"
        );
        std::fs::write(&descendant_release, b"release").expect("release descendant");
        let page = pohunek_test_support::wait::guard("the late output page", &mut output)
            .await
            .expect("late output page");
        let decoded = base64::prelude::BASE64_STANDARD
            .decode(page.data_base64())
            .expect("valid output base64");
        assert!(decoded
            .windows(19)
            .any(|window| window == b"late-restart-output"));

        wait_until("terminal worker state after PTY EOF", || async {
            let state = replacement
                .inspect(&SessionId(session_id.to_owned()))
                .await
                .expect("inspect completed draining runtime")
                .state;
            (state != SessionState::Running).then_some(())
        })
        .await;

        server_task.abort();
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "restart continuity test keeps original and replacement registry assertions together"
    )]
    async fn replacement_registry_preserves_native_recovery_and_fork_binding() {
        let root = temp_root();
        // The Claude recovery binding this fixture forks clears the transcript
        // preflight against a fixture config home: it declares it through the
        // registry's launch environment, which its relaunch reads.
        let claude_home = root.join("claude-home");
        let fixture_project = claude_home.join("projects/fixture");
        std::fs::create_dir_all(&fixture_project).expect("create fixture Claude config home");
        std::fs::write(fixture_project.join("native-restart-test.jsonl"), "{}\n")
            .expect("write fixture Claude transcript");
        let runtime_root = root.join("runtime/workers");
        let session_id = "s-91";
        let worker_id = "worker-restart-test";
        let socket = runtime_root
            .join(session_id)
            .join(pohunek_paths::WORKER_SOCKET_NAME);
        let journal = root.join("state/workers/s-91/worker-restart-test.json");
        let mut worker_config = WorkerConfig::new();
        worker_config.initialize_deadline = Duration::from_secs(5);
        worker_config.terminal_retention = Duration::from_secs(1);
        let server = Server::bind(ServerArgs {
            session_id: session_id.to_owned(),
            worker_id: worker_id.to_owned(),
            generation: "abcd2345".to_owned(),
            socket_path: socket.clone(),
            journal_path: journal,
            daemon_socket_path: root.join("runtime/daemon.sock"),
            config: worker_config,
        })
        .await
        .expect("bind worker");
        std::fs::set_permissions(&runtime_root, std::fs::Permissions::from_mode(0o700))
            .expect("private worker runtime root");
        let server_task = tokio::spawn(server.serve());

        let first_controller = crate::runtime::Worker::connect(&socket, session_id, "daemon-old")
            .await
            .expect("first controller");
        let wire_worker_id = first_controller.worker_id().await;
        let wire_session_id = WorkerSessionId::new(session_id).expect("session id");
        // The nested agents report their own identity, exactly as a real nested
        // agent's in-process hook would, so each one is its own reporter rather
        // than a `sleep` that could never have spoken for itself.
        let (reporter_script, reporters) = identity_reporters(&root, &["released", "reassert"]);
        let released = reporters[0].clone();
        let reassert = reporters[1].clone();
        let nested_command = format!(
            "{} {} printf ready; while :; do sleep 1; done",
            released.launch(&reporter_script),
            reassert.launch(&reporter_script),
        );
        let released_pid_path = released.pid_path.clone();
        let reassert_pid_path = reassert.pid_path.clone();
        let worker_instance_id = first_controller
            .initialize(Initialize {
                session_id: wire_session_id,
                daemon_instance_id: Some(
                    pohunek_worker_protocol::DaemonId::new("daemon-restart-test")
                        .expect("daemon id"),
                ),
                transaction_id: TransactionId::new("create-restart-test").expect("transaction id"),
                expected_worker_id: wire_worker_id.clone(),
                launch: LaunchIdentity {
                    agent: "claude".to_owned(),
                    agent_base: "claude".to_owned(),
                    reference_kind: Some("id".to_owned()),
                },
                executable: PathBuf::from("/bin/sh"),
                arguments: vec!["-c".to_owned(), nested_command.clone()],
                cwd: root.clone(),
                dimensions: Dimensions::new(80, 24).expect("dimensions"),
                environment: SecretEnv::new(BTreeMap::new()).expect("environment"),
                base_environment: Some(
                    crate::runtime::environment::base_environment(
                        pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST,
                        &crate::test_support::thread_environment_source(),
                    )
                    .expect("base environment"),
                ),
                limits: InitializeLimits::new(1_000_000, 100_000, 128, 60_000).expect("limits"),
                stop_policy: StopPolicy::new(100).expect("stop policy"),
                hook_protocol_version: Version::new(1).expect("version"),
                public_protocol_version: protocol::PROTOCOL_VERSION.get(),
                hook_schema: Some("identity-subagent-v1".to_owned()),
            })
            .await
            .expect("initialize worker");
        let before = first_controller.inspect().await.expect("inspect before");
        let child = before.child_process.expect("child identity");
        let child_pid = child.pid;
        let released_pid = wait_for_pid_file(&released_pid_path).await;
        let released_start = process_start_identity(released_pid);
        let reassert_pid = wait_for_pid_file(&reassert_pid_path).await;
        let reassert_start = process_start_identity(reassert_pid);
        let claim_expiry = (OffsetDateTime::now_utc() + time::Duration::seconds(30))
            .format(&Rfc3339)
            .expect("format private claim expiry");
        assert!(
            send_identity_hook_from(
                &released,
                serde_json::json!({
                    "type": "identity_report",
                    "runtime_id": worker_instance_id.as_str(),
                    "provider": "claude",
                    "pid": released_pid,
                    "start_identity": released_start,
                    "sequence": 7,
                    "expires_at": claim_expiry,
                    "reference_kind": "id",
                    "native_reference": "nested-before-release"
                }),
            )
            .await
        );
        assert!(
            send_identity_hook_from(
                &released,
                serde_json::json!({
                    "type": "identity_release",
                    "runtime_id": worker_instance_id.as_str(),
                    "provider": "claude",
                    "pid": released_pid,
                    "start_identity": released_start,
                    "sequence": 8
                }),
            )
            .await
        );
        assert!(pohunek_test_support::process_env::command("kill")
            .args(["-KILL", &released_pid.to_string()])
            .status()
            .expect("kill released child")
            .success());

        let created_at = "2026-07-23T00:00:00Z".to_owned();
        let info = SessionInfo {
            id: SessionId(session_id.to_owned()),
            external: Some(false),
            name: Some("restart continuity".to_owned()),
            agent: "claude".to_owned(),
            agent_base: RuntimeRef::claude(),
            cwd: root.clone(),
            cwd_source: Some(CwdSource::Launch),
            pid: child_pid,
            runtime: Some(SessionRuntime {
                state: RuntimeState::Live,
                runtime_generation: protocol::RuntimeGeneration::new(1),
                worker_id: Some(worker_id.to_owned()),
                worker_instance_id: Some(worker_instance_id.to_string()),
                started_at: Some(created_at.clone()),
                last_connected_at: Some(created_at.clone()),
                loss_reason: None,
            }),
            cols: 80,
            rows: 24,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            subagents: Vec::new(),
            active_agent: Some("claude".to_owned()),
            active_agent_base: Some(RuntimeRef::claude()),
            active_agent_pid: Some(released_pid),
            active_agent_session_id: Some("nested-before-release".to_owned()),
            active_agent_session_path: None,
            native_session_id: None,
            native_session_path: None,
            native_last_activity_at: None,
            project_id: None,
            project_label: None,
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            metadata: BTreeMap::new(),
            created_at: created_at.clone(),
            updated_at: created_at,
            exit_code: None,
            capabilities: protocol::SessionCapabilities {
                resume: true,
                fork: true,
            },
        };
        let store_path = root.join("data/metadata.jsonl");
        let store = Store::new(store_path.clone());
        let stale_recovery = ResumeBinding {
            session_id: session_id.to_owned(),
            name: Some("restart continuity".to_owned()),
            agent: "claude".to_owned(),
            agent_base: RuntimeRef::claude(),
            cwd: root.clone(),
            cols: 80,
            rows: 24,
            native_session_id: None,
            native_session_path: None,
            project_id: None,
            is_linked_worktree: None,
            metadata: BTreeMap::new(),
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "printf ready; sleep 30".to_owned()],
            input_rules: StoredInputRules::from(InputRules::unrestricted(false, Duration::ZERO)),
            native_launch: Some(test_native_launch(SessionRefKind::Id, true)),
            launch_binding: crate::agent::host::LaunchPin::Unpinned,
            native_reference_provenance: crate::agent::NativeReferenceProvenance::default(),
            profile_revision: None,
            native_launch_unresolved: false,
        };
        store
            .record_session(&SessionRecord {
                schema_version: 1,
                session_id: session_id.to_owned(),
                desired_state: DesiredState::Running,
                transaction: Some(crate::store::SessionTransaction {
                    id: "create-restart-test".to_owned(),
                    kind: crate::store::TransactionKind::Create,
                    phase: "preparing".to_owned(),
                    previous_worker_id: None,
                    previous_worker_instance_id: None,
                    daemon_instance_id: None,
                }),
                native_identity_ordering: Some(NativeIdentityOrdering {
                    worker_instance_id: worker_instance_id.to_string(),
                    pid: child.pid,
                    pid_start_identity: child.start_identity,
                    sequence: Some(100),
                    worker_sequence: None,
                }),
                info: SessionInfo {
                    pid: 0,
                    state: SessionState::Starting,
                    runtime: Some(SessionRuntime {
                        state: RuntimeState::Starting,
                        runtime_generation: protocol::RuntimeGeneration::new(1),
                        worker_id: None,
                        worker_instance_id: None,
                        started_at: None,
                        last_connected_at: None,
                        loss_reason: None,
                    }),
                    ..info
                },
                recovery: Some(stale_recovery.clone()),
                runtime: RuntimeRecord {
                    state: RuntimeState::Starting,
                    worker_id: None,
                    worker_instance_id: None,
                    service_id: Some(format!("{session_id}.{TEST_GENERATION}")),
                    generation: Some(TEST_GENERATION.to_owned()),
                    executable: Some(PathBuf::from(TEST_WORKER_EXECUTABLE)),
                    reason: None,
                },
            })
            .expect("persist logical record");
        let mut reported_recovery = stale_recovery;
        reported_recovery.native_session_id = Some("native-restart-test".to_owned());
        store
            .record_resume(&reported_recovery)
            .expect("persist native recovery binding");
        drop(first_controller);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let replacement = replacement_registry_with_claude_fixture(
            SessionRegistryConfig {
                shell_command: hermetic_shell(),
                store_path: Some(store_path),
                worker_runtime_root: Some(runtime_root),
                worker_state_root: Some(root.join("state/workers")),
                ..SessionRegistryConfig::default()
            },
            &claude_home,
        );
        Box::pin(replacement.reconcile_workers())
            .await
            .expect("reconcile replacement daemon");
        let adopted = replacement
            .inspect(&SessionId(session_id.to_owned()))
            .await
            .expect("inspect adopted session");
        assert_eq!(adopted.pid, child_pid);
        assert!(
            adopted.active_agent.is_none(),
            "restart must apply a durable release after the released process exits"
        );
        assert_eq!(
            adopted
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.worker_instance_id.as_deref()),
            Some(worker_instance_id.as_str())
        );
        assert_eq!(
            adopted.native_session_id.as_deref(),
            Some("native-restart-test"),
            "replacement daemon must merge the latest persisted native binding"
        );
        let reassert_expiry = (OffsetDateTime::now_utc() + time::Duration::seconds(30))
            .format(&Rfc3339)
            .expect("format reassert expiry");
        let reassert_request = |sequence: u64| {
            serde_json::json!({
                "type": "identity_report",
                "runtime_id": worker_instance_id.as_str(),
                "provider": "claude",
                "pid": reassert_pid,
                "start_identity": reassert_start,
                "sequence": sequence,
                "expires_at": reassert_expiry,
                "reference_kind": "id",
                "native_reference": "nested-after-restart"
            })
        };
        assert!(
            !send_identity_hook_from(&reassert, reassert_request(8)).await,
            "release sequence must reject a late report after restart"
        );
        assert!(
            send_identity_hook_from(&reassert, reassert_request(9)).await,
            "higher sequence may reassert after restart"
        );
        let stale_native_report = SessionReportNativeIdParams::new(
            SessionId(session_id.to_owned()),
            worker_instance_id.as_str(),
            "claude",
            child.pid,
            ProcessStartIdentity::new(child.start_identity),
            ReportSequence::new(99),
            (OffsetDateTime::now_utc() + time::Duration::seconds(30))
                .format(&Rfc3339)
                .expect("format claim expiry"),
            "native-stale-after-restart",
            None,
        )
        .expect("valid stale native report");
        assert!(
            !replacement
                .report_native_id(stale_native_report)
                .await
                .recorded,
            "replacement daemon must reject a sequence below the persisted ordering tombstone"
        );
        assert_eq!(
            replacement
                .inspect(&SessionId(session_id.to_owned()))
                .await
                .expect("inspect after stale report")
                .native_session_id
                .as_deref(),
            Some("native-restart-test")
        );
        let committed = Store::new(root.join("data/metadata.jsonl"))
            .load_sessions()
            .expect("load committed record")
            .into_iter()
            .find(|record| record.session_id == session_id)
            .expect("committed logical record");
        assert_eq!(committed.transaction, None);
        assert_eq!(committed.runtime.state, RuntimeState::Live);
        assert_eq!(
            committed
                .recovery
                .as_ref()
                .and_then(|binding| binding.native_session_id.as_deref()),
            Some("native-restart-test"),
            "reconciliation must commit the merged fork binding"
        );
        let self_attach = replacement
            .attach(&protocol::SessionAttachParams {
                session_id: SessionId(session_id.to_owned()),
                initial_dimensions: None,
                origin_session_id: Some(SessionId(session_id.to_owned())),
                origin_daemon_id: Some("daemon-old".to_owned()),
                origin_worker_id: Some(worker_id.to_owned()),
            })
            .await
            .expect_err("stable worker origin must reject self-attach after daemon replacement");
        assert_eq!(self_attach.code, "attach_self_feedback");
        replacement
            .attach(&protocol::SessionAttachParams {
                session_id: SessionId(session_id.to_owned()),
                initial_dimensions: None,
                origin_session_id: Some(SessionId(session_id.to_owned())),
                origin_daemon_id: Some("daemon-old".to_owned()),
                origin_worker_id: Some("different-worker".to_owned()),
            })
            .await
            .expect("stale daemon id alone must not reject a worker-backed attach");
        replacement
            .resize(&SessionId(session_id.to_owned()), 100, 30)
            .await
            .expect("worker-backed resize after adoption");
        let forked = replacement
            .fork(SessionForkParams {
                session_id: SessionId(session_id.to_owned()),
                name: Some("restart recovery fork".to_owned()),
                cwd_mode: ForkCwdMode::Same,
                cols: 100,
                rows: 30,
                accept_profile_change: false,
            })
            .await
            .expect("fork adopted session from preserved native binding");
        assert_ne!(forked.id, SessionId(session_id.to_owned()));
        // A fork starts a distinct conversation: only the child's own verified
        // launch report may give it a recovery reference of its own.
        assert_eq!(
            forked.native_session_id, None,
            "the fork child cannot resume its source's conversation as its own"
        );
        replacement
            .stop(&forked.id)
            .await
            .expect("stop recovery fork");
        replacement
            .stop(&SessionId(session_id.to_owned()))
            .await
            .expect("stop adopted worker");
        server_task.abort();
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "restart adoption test keeps worker setup, durable state, and continuity assertions together"
    )]
    async fn replacement_registry_adopts_same_live_hermes_worker() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let session_id = "s-92";
        let worker_id = "worker-hermes-restart";
        let socket = runtime_root
            .join(session_id)
            .join(pohunek_paths::WORKER_SOCKET_NAME);
        let journal = root.join("state/workers/s-92/worker-hermes-restart.json");
        let mut worker_config = WorkerConfig::new();
        worker_config.initialize_deadline = Duration::from_secs(5);
        worker_config.terminal_retention = Duration::from_secs(1);
        let server = Server::bind(ServerArgs {
            session_id: session_id.to_owned(),
            worker_id: worker_id.to_owned(),
            generation: "abcd2345".to_owned(),
            socket_path: socket.clone(),
            journal_path: journal,
            daemon_socket_path: root.join("runtime/daemon.sock"),
            config: worker_config,
        })
        .await
        .expect("bind Hermes worker");
        std::fs::set_permissions(&runtime_root, std::fs::Permissions::from_mode(0o700))
            .expect("private worker runtime root");
        let server_task = tokio::spawn(server.serve());

        let first_controller = crate::runtime::Worker::connect(&socket, session_id, "daemon-old")
            .await
            .expect("first controller");
        let wire_worker_id = first_controller.worker_id().await;
        let worker_instance_id = first_controller
            .initialize(Initialize {
                session_id: WorkerSessionId::new(session_id).expect("session id"),
                daemon_instance_id: Some(
                    pohunek_worker_protocol::DaemonId::new("daemon-hermes-test")
                        .expect("daemon id"),
                ),
                transaction_id: TransactionId::new("create-hermes-restart")
                    .expect("transaction id"),
                expected_worker_id: wire_worker_id,
                launch: LaunchIdentity {
                    agent: "hermes".to_owned(),
                    agent_base: "hermes".to_owned(),
                    reference_kind: Some("id".to_owned()),
                },
                executable: PathBuf::from("/bin/sh"),
                arguments: vec![
                    "-c".to_owned(),
                    "printf ready; while :; do sleep 1; done".to_owned(),
                ],
                cwd: root.clone(),
                dimensions: Dimensions::new(80, 24).expect("dimensions"),
                environment: SecretEnv::new(BTreeMap::new()).expect("environment"),
                base_environment: Some(
                    crate::runtime::environment::base_environment(
                        pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST,
                        &crate::test_support::thread_environment_source(),
                    )
                    .expect("base environment"),
                ),
                limits: InitializeLimits::new(1_000_000, 100_000, 128, 60_000).expect("limits"),
                stop_policy: StopPolicy::new(100).expect("stop policy"),
                hook_protocol_version: Version::new(1).expect("version"),
                public_protocol_version: protocol::PROTOCOL_VERSION.get(),
                hook_schema: Some("identity-v1".to_owned()),
            })
            .await
            .expect("initialize Hermes worker");
        let child_pid = first_controller
            .inspect()
            .await
            .expect("inspect Hermes worker")
            .child_process
            .expect("child identity")
            .pid;

        let mut record = identity_record();
        record.session_id = session_id.to_owned();
        record.info.id = SessionId(session_id.to_owned());
        record.info.agent = "hermes".to_owned();
        record.info.agent_base = RuntimeRef::hermes();
        record.info.cwd = root.clone();
        record.info.pid = child_pid;
        record.info.capabilities = protocol::SessionCapabilities {
            resume: true,
            fork: false,
        };
        let info_runtime = record.info.runtime.as_mut().expect("runtime info");
        info_runtime.worker_id = Some(worker_id.to_owned());
        info_runtime.worker_instance_id = Some(worker_instance_id.to_string());
        let recovery = record.recovery.as_mut().expect("recovery binding");
        recovery.session_id = session_id.to_owned();
        recovery.agent = "hermes".to_owned();
        recovery.agent_base = RuntimeRef::hermes();
        recovery.cwd = root.clone();
        recovery.program = "hermes".to_owned();
        recovery.args = vec!["chat".to_owned()];
        recovery.input_rules =
            StoredInputRules::from(InputRules::hermes(true, Duration::from_millis(150)));
        recovery.native_launch = Some(test_native_launch(SessionRefKind::Id, false));
        record.runtime.worker_id = Some(worker_id.to_owned());
        record.runtime.worker_instance_id = Some(worker_instance_id.to_string());

        let store_path = root.join("data/metadata.jsonl");
        bind_test_generation(&mut record);
        Store::new(store_path.clone())
            .record_session(&record)
            .expect("persist Hermes logical record");
        drop(first_controller);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let replacement = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store_path),
            worker_runtime_root: Some(runtime_root),
            worker_state_root: Some(root.join("state/workers")),
            ..SessionRegistryConfig::default()
        });
        Box::pin(replacement.reconcile_workers())
            .await
            .expect("replacement daemon adopts Hermes worker");
        let adopted = replacement
            .inspect(&SessionId(session_id.to_owned()))
            .await
            .expect("inspect adopted Hermes session");
        assert_eq!(adopted.id, SessionId(session_id.to_owned()));
        assert_eq!(adopted.agent, "hermes");
        assert_eq!(adopted.agent_base, RuntimeRef::hermes());
        assert_eq!(adopted.pid, child_pid);
        assert_eq!(
            adopted
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.worker_instance_id.as_deref()),
            Some(worker_instance_id.as_str())
        );
        assert_eq!(
            adopted.capabilities,
            protocol::SessionCapabilities {
                resume: true,
                fork: false,
            }
        );

        replacement
            .resize(&SessionId(session_id.to_owned()), 100, 30)
            .await
            .expect("resize adopted Hermes worker");
        replacement
            .stop(&SessionId(session_id.to_owned()))
            .await
            .expect("stop adopted Hermes worker");
        server_task.abort();
    }

    #[tokio::test]
    async fn replacement_registry_allocates_a_new_ulid_session_id() {
        let root = temp_root();
        let store_path = root.join("data/metadata.jsonl");
        let mut record = identity_record();
        record.session_id = "s-91".to_owned();
        record.info.id = SessionId("s-91".to_owned());
        record.runtime.state = RuntimeState::Lost;
        if let Some(binding) = record.recovery.as_mut() {
            binding.session_id = "s-91".to_owned();
        }
        Store::new(store_path.clone())
            .record_session(&record)
            .expect("persist previous daemon session");

        let replacement = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store_path),
            ..SessionRegistryConfig::default()
        });
        Box::pin(replacement.reconcile_workers())
            .await
            .expect("replacement daemon reconciles store");
        let created = replacement
            .create(SessionNewParams {
                name: None,
                agent: "shell".to_owned(),
                cwd: Some(root.to_path_buf()),
                cols: 80,
                rows: 24,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                extended_input_ready_wait: None,
                metadata: BTreeMap::new(),
            })
            .await
            .expect("create after daemon replacement");

        assert_ne!(created.id, SessionId("s-91".to_owned()));
        assert!(
            created.id.0.starts_with("s-"),
            "session id must retain its public prefix: {}",
            created.id.0
        );
        assert!(
            ulid::Ulid::from_string(&created.id.0[2..]).is_ok(),
            "session id must have a valid ULID suffix: {}",
            created.id.0
        );
        replacement
            .stop(&created.id)
            .await
            .expect("stop fresh session");
    }

    #[tokio::test]
    async fn reconciliation_compensates_preparing_record_without_runtime_evidence() {
        let root = temp_root();
        let store_path = root.join("data/metadata.jsonl");
        let mut record = identity_record();
        record.session_id = "s-204".to_owned();
        record.info.id = SessionId("s-204".to_owned());
        record.info.state = SessionState::Starting;
        record.info.pid = 0;
        record.runtime.state = RuntimeState::Starting;
        record.runtime.worker_id = None;
        record.runtime.worker_instance_id = None;
        record.transaction = Some(crate::store::SessionTransaction {
            id: "create-s-204".to_owned(),
            kind: crate::store::TransactionKind::Create,
            phase: "preparing".to_owned(),
            previous_worker_id: None,
            previous_worker_instance_id: None,
            daemon_instance_id: None,
        });
        record.recovery.as_mut().expect("recovery").session_id = "s-204".to_owned();
        Store::new(store_path.clone())
            .record_session(&record)
            .expect("persist preparing record");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store_path.clone()),
            worker_runtime_root: Some(root.join("runtime/workers")),
            worker_state_root: Some(root.join("state/workers")),
            ..SessionRegistryConfig::default()
        });

        Box::pin(registry.reconcile_workers())
            .await
            .expect("compensate abandoned create");

        registry
            .inspect(&SessionId("s-204".to_owned()))
            .await
            .expect_err("compensated create must leave no adoptable session record");
        assert!(Store::new(store_path)
            .load_sessions()
            .expect("load compensated store")
            .is_empty());
    }

    #[tokio::test]
    async fn corrupted_expected_worker_journal_conflicts_and_blocks_resume() {
        let root = temp_root();
        let store_path = root.join("data/metadata.jsonl");
        let state_root = root.join("state/workers");
        let mut record = identity_record();
        record.session_id = "s-206".to_owned();
        record.info.id = SessionId("s-206".to_owned());
        record.runtime.worker_id = Some("worker-corrupt".to_owned());
        record.info.runtime.as_mut().expect("runtime").worker_id =
            Some("worker-corrupt".to_owned());
        record.recovery.as_mut().expect("recovery").session_id = "s-206".to_owned();
        Store::new(store_path.clone())
            .record_session(&record)
            .expect("persist logical record");
        let session_dir = state_root.join("s-206");
        std::fs::create_dir_all(&session_dir).expect("create worker journal directory");
        for path in [
            state_root.parent().expect("state parent"),
            state_root.as_path(),
            session_dir.as_path(),
        ] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .expect("set private journal directory mode");
        }
        let journal_path = session_dir.join("worker-corrupt.json");
        std::fs::write(&journal_path, b"not json").expect("write corrupt worker journal");
        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o600))
            .expect("set private journal mode");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store_path),
            worker_runtime_root: Some(root.join("runtime/workers")),
            worker_state_root: Some(state_root),
            ..SessionRegistryConfig::default()
        });

        Box::pin(registry.reconcile_workers())
            .await
            .expect("reconcile corrupt journal");
        let conflicted = registry
            .inspect(&SessionId("s-206".to_owned()))
            .await
            .expect("inspect conflicted session");
        assert_eq!(
            conflicted.runtime.expect("runtime").state,
            RuntimeState::Conflict
        );
        let error = registry
            .resume(&SessionId("s-206".to_owned()))
            .await
            .expect_err("conflicted session cannot resume");
        assert_eq!(error.code, "session_runtime_not_recoverable");
    }

    #[tokio::test]
    async fn reconciliation_imports_terminal_journal_after_worker_retention_exit() {
        let root = temp_root();
        let store_path = root.join("data/metadata.jsonl");
        let state_root = root.join("state/workers");
        let mut logical = identity_record();
        logical.session_id = "s-205".to_owned();
        logical.info.id = SessionId("s-205".to_owned());
        logical.runtime.worker_id = Some("worker-terminal".to_owned());
        logical.runtime.worker_instance_id = Some("runtime-terminal".to_owned());
        logical.recovery.as_mut().expect("recovery").session_id = "s-205".to_owned();
        Store::new(store_path.clone())
            .record_session(&logical)
            .expect("persist running logical record");

        std::fs::create_dir_all(&state_root).expect("create state root");
        std::fs::set_permissions(
            state_root.parent().expect("state parent"),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("private state parent");
        std::fs::set_permissions(&state_root, std::fs::Permissions::from_mode(0o700))
            .expect("private state root");
        let mut journal = JournalRecord::bootstrap(
            "s-205".to_owned(),
            "worker-terminal".to_owned(),
            pohunek_session_worker::WorkerOrigin {
                executable: PathBuf::from("/usr/libexec/pohunek-sessiond"),
                version: "0.0.0-test".to_owned(),
                generation: "abcd2345".to_owned(),
            },
            41,
            "410".to_owned(),
            "boot-test".to_owned(),
            (1, 2),
            "2026-07-23T00:00:00Z".to_owned(),
        );
        journal.worker_instance_id = Some("runtime-terminal".to_owned());
        journal.child = Some(JournalChildIdentity {
            pid: 51,
            process_group: 51,
            start_identity: "510".to_owned(),
        });
        journal.cols = Some(100);
        journal.rows = Some(30);
        journal.phase = JournalRuntimePhase::Terminal;
        journal.outcome = Some(JournalRuntimeOutcome {
            exit_code: Some(0),
            signal: None,
            success: true,
            exited_at: "2026-07-23T00:01:00Z".to_owned(),
            reason: "natural_exit".to_owned(),
        });
        let mut journal_value = serde_json::to_value(&journal).expect("serialize journal fixture");
        journal_value["subagent_revision"] = serde_json::json!(4);
        journal_value["subagents"] = serde_json::json!([{
            "id": "child-terminal",
            "parent_id": null,
            "provider": "codex",
            "agent_type": "worker",
            "phase": "running",
            "sequence": 1,
            "revision": 4,
            "started_at_ms": 1,
            "updated_at_ms": 1,
            "finished_at_ms": null
        }]);
        journal = serde_json::from_value(journal_value).expect("deserialize journal fixture");
        Journal::new(state_root.join("s-205/worker-terminal.json"))
            .write(&journal)
            .expect("persist terminal journal");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store_path),
            worker_runtime_root: Some(root.join("runtime/workers")),
            worker_state_root: Some(state_root),
            ..SessionRegistryConfig::default()
        });

        Box::pin(registry.reconcile_workers())
            .await
            .expect("import terminal journal");

        let imported = registry
            .inspect(&SessionId("s-205".to_owned()))
            .await
            .expect("terminal logical record remains visible");
        assert_eq!(imported.state, SessionState::Done);
        assert_eq!(imported.exit_code, Some(0));
        assert_eq!((imported.cols, imported.rows), (100, 30));
        assert_eq!(
            imported.runtime.expect("runtime").state,
            RuntimeState::Terminal
        );
        let subagent = imported.subagents.first().expect("terminal subagent");
        assert_eq!(subagent.lifecycle, protocol::SubagentLifecycle::Lost);
        assert!(subagent.revision.get() > 4);
    }

    #[tokio::test]
    async fn discovery_quarantines_orphan_without_stopping_worker() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let (socket, server_task) =
            spawn_uninitialized_worker(&root, &runtime_root, "s-201", "s-201", "worker-o").await;
        let registry = empty_registry(&root, &runtime_root);
        let mut events = registry.subscribe();

        Box::pin(registry.reconcile_workers())
            .await
            .expect("discover orphan");

        let event = events.recv().await.expect("orphan discovery event");
        assert_eq!(event.event(), protocol::event::SESSION_RUNTIME_DISCOVERED);
        let inventory = registry.runtime_inventory().await;
        assert_eq!(inventory.entries.len(), 1);
        assert_eq!(
            inventory.entries[0].status,
            RuntimeInventoryStatus::Orphaned
        );
        crate::runtime::Worker::connect(&socket, "s-201", "orphan-probe")
            .await
            .expect("quarantine releases controller lease and leaves worker alive");
        server_task.abort();
    }

    #[tokio::test]
    async fn discovery_fails_closed_on_runtime_slot_identity_mismatch() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let (socket, server_task) = spawn_uninitialized_worker(
            &root,
            &runtime_root,
            "mismatched-slot",
            "s-202",
            "worker-mismatch",
        )
        .await;
        persist_identity_record(&root, "s-202", "worker-mismatch");
        let registry = empty_registry(&root, &runtime_root);

        Box::pin(registry.reconcile_workers())
            .await
            .expect("discover mismatch");

        let inventory = registry.runtime_inventory().await;
        assert_eq!(
            inventory.entries[0].status,
            RuntimeInventoryStatus::IdentityMismatch
        );
        let session = registry
            .inspect(&SessionId("s-202".to_owned()))
            .await
            .expect("logical record remains visible");
        assert_eq!(
            session.runtime.expect("runtime").state,
            RuntimeState::Conflict
        );
        crate::runtime::Worker::connect(&socket, "s-202", "mismatch-probe")
            .await
            .expect("identity mismatch is quarantined without stopping worker");
        server_task.abort();
    }

    #[tokio::test]
    async fn discovery_fails_closed_on_duplicate_claims_without_stopping_workers() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let (canonical_socket, canonical_task) = spawn_uninitialized_worker(
            &root,
            &runtime_root,
            "s-203",
            "s-203",
            "worker-duplicate-a",
        )
        .await;
        let (shadow_socket, shadow_task) = spawn_uninitialized_worker(
            &root,
            &runtime_root,
            "duplicate-shadow",
            "s-203",
            "worker-duplicate-b",
        )
        .await;
        persist_identity_record(&root, "s-203", "worker-duplicate-a");
        let registry = empty_registry(&root, &runtime_root);

        Box::pin(registry.reconcile_workers())
            .await
            .expect("discover duplicate");

        let inventory = registry.runtime_inventory().await;
        assert_eq!(inventory.entries.len(), 2);
        assert!(inventory
            .entries
            .iter()
            .any(|entry| entry.status == RuntimeInventoryStatus::Conflict));
        let session = registry
            .inspect(&SessionId("s-203".to_owned()))
            .await
            .expect("logical record remains visible");
        assert_eq!(
            session.runtime.expect("runtime").state,
            RuntimeState::Conflict
        );
        crate::runtime::Worker::connect(&canonical_socket, "s-203", "duplicate-probe-a")
            .await
            .expect("canonical duplicate remains alive");
        crate::runtime::Worker::connect(&shadow_socket, "s-203", "duplicate-probe-b")
            .await
            .expect("shadow duplicate remains alive");
        canonical_task.abort();
        shadow_task.abort();
    }

    #[tokio::test]
    async fn discovery_exposes_incompatible_endpoint() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let fake_task = spawn_incompatible_worker(&runtime_root, "s-incompatible");
        let registry = empty_registry(&root, &runtime_root);

        Box::pin(registry.reconcile_workers())
            .await
            .expect("discover incompatible endpoint");

        let inventory = registry.runtime_inventory().await;
        assert_eq!(inventory.entries.len(), 1);
        assert_eq!(
            inventory.entries[0].status,
            RuntimeInventoryStatus::Incompatible
        );
        assert_eq!(
            inventory.entries[0].reason.as_deref(),
            Some("worker_protocol_incompatible")
        );
        fake_task.abort();
    }

    #[tokio::test]
    async fn reconciliation_replays_durable_stop_intent() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let (controller, worker_instance_id, child_pid, server_task) =
            spawn_initialized_worker(&root, &runtime_root, "s-206", "worker-stop-replay").await;
        persist_live_record(
            &root,
            "s-206",
            "worker-stop-replay",
            worker_instance_id.as_str(),
            child_pid,
            DesiredState::Stopped,
            crate::store::TransactionKind::Stop,
        );
        drop(controller);
        let registry = empty_registry(&root, &runtime_root);

        Box::pin(registry.reconcile_workers())
            .await
            .expect("replay stop");

        let stopped = registry
            .inspect(&SessionId("s-206".to_owned()))
            .await
            .expect("stopped session remains visible");
        assert_eq!(stopped.state, SessionState::Stopped);
        assert_eq!(
            stopped.runtime.expect("runtime").state,
            RuntimeState::Terminal
        );
        let persisted = Store::new(root.join("data/metadata.jsonl"))
            .load_sessions()
            .expect("load stopped record")
            .pop()
            .expect("stopped record");
        assert_eq!(persisted.transaction, None);
        server_task.abort();
    }

    #[tokio::test]
    async fn reconciliation_finishes_durable_remove_intent() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let (controller, worker_instance_id, child_pid, server_task) =
            spawn_initialized_worker(&root, &runtime_root, "s-207", "worker-remove-replay").await;
        persist_live_record(
            &root,
            "s-207",
            "worker-remove-replay",
            worker_instance_id.as_str(),
            child_pid,
            DesiredState::Removed,
            crate::store::TransactionKind::Remove,
        );
        drop(controller);
        let registry = empty_registry(&root, &runtime_root);

        Box::pin(registry.reconcile_workers())
            .await
            .expect("replay removal");

        registry
            .inspect(&SessionId("s-207".to_owned()))
            .await
            .expect_err("replayed removal must leave no adoptable session record");
        assert!(Store::new(root.join("data/metadata.jsonl"))
            .load_sessions()
            .expect("load removed store")
            .is_empty());
        server_task.abort();
    }

    #[tokio::test]
    async fn worker_protocol_upgrade_and_rollback_preserve_runtime_generation() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let (initial, worker_instance_id, child_pid, server_task) =
            spawn_initialized_worker(&root, &runtime_root, "s-208", "worker-version-fixture").await;
        let socket = initial.socket_path().to_path_buf();
        drop(initial);

        for (daemon_id, minimum, maximum) in [
            (
                "daemon-n-minus-one",
                pohunek_worker_protocol::PREVIOUS_VERSION,
                pohunek_worker_protocol::PREVIOUS_VERSION,
            ),
            (
                "daemon-n",
                pohunek_worker_protocol::PREVIOUS_VERSION,
                pohunek_worker_protocol::CURRENT_VERSION,
            ),
            (
                "daemon-rollback",
                pohunek_worker_protocol::PREVIOUS_VERSION,
                pohunek_worker_protocol::PREVIOUS_VERSION,
            ),
        ] {
            let controller = crate::runtime::Worker::connect_with_range(
                &socket, "s-208", daemon_id, minimum, maximum,
            )
            .await
            .expect("negotiate release fixture");
            let snapshot = controller.inspect().await.expect("inspect release fixture");
            assert_eq!(
                snapshot.worker_instance_id.as_ref(),
                Some(&worker_instance_id)
            );
            assert_eq!(
                snapshot.child_process.expect("child process").pid,
                child_pid
            );
            drop(controller);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        server_task.abort();
    }

    #[test]
    fn discovery_scan_rejects_symlink_non_socket_and_unsafe_permissions() {
        let root = temp_root();
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("private root");
        let unsafe_dir = root.join("unsafe");
        std::fs::create_dir_all(&unsafe_dir).expect("create unsafe");
        std::fs::set_permissions(&unsafe_dir, std::fs::Permissions::from_mode(0o755))
            .expect("unsafe mode");
        std::fs::write(
            unsafe_dir.join(pohunek_paths::WORKER_SOCKET_NAME),
            b"not a socket",
        )
        .expect("plain file");
        std::os::unix::fs::symlink(&unsafe_dir, root.join("symlink")).expect("symlink");

        let slots = super::discover_runtime_slots(&root).expect("safe scan");

        assert!(slots.is_empty());
    }

    /// Connect deadline of the discovery fixtures; short so a probe of a
    /// socket that never answers settles quickly. It also bounds every
    /// launch, so a fixture that creates a healthy session uses
    /// [`launching_registry`] instead.
    const DISCOVERY_CONNECT_DEADLINE: Duration = Duration::from_millis(300);

    fn empty_registry(root: &std::path::Path, runtime_root: &std::path::Path) -> SessionRegistry {
        registry_within(root, runtime_root, DISCOVERY_CONNECT_DEADLINE)
    }

    /// An [`empty_registry`] that launches healthy in-process workers within
    /// the dev/test connect contract, which covers their durable startup I/O.
    fn launching_registry(
        root: &std::path::Path,
        runtime_root: &std::path::Path,
    ) -> SessionRegistry {
        registry_within(root, runtime_root, DEV_WORKER_CONNECT)
    }

    fn registry_within(
        root: &std::path::Path,
        runtime_root: &std::path::Path,
        worker_connect_deadline: Duration,
    ) -> SessionRegistry {
        SessionRegistry::new(SessionRegistryConfig {
            // A fixed shell keeps the sessions independent of the host user's
            // `$SHELL` and its startup files, whose background helpers can
            // hold the PTY open past the stop deadline.
            shell_command: crate::session::ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            store_path: Some(root.join("data/metadata.jsonl")),
            worker_runtime_root: Some(runtime_root.to_path_buf()),
            worker_state_root: Some(root.join("state/workers")),
            worker_connect_deadline,
            ..SessionRegistryConfig::default()
        })
    }

    async fn spawn_uninitialized_worker(
        root: &std::path::Path,
        runtime_root: &std::path::Path,
        slot: &str,
        claimed_session: &str,
        worker_id: &str,
    ) -> (PathBuf, tokio::task::JoinHandle<()>) {
        let socket = runtime_root
            .join(slot)
            .join(pohunek_paths::WORKER_SOCKET_NAME);
        let server = Server::bind(ServerArgs {
            session_id: claimed_session.to_owned(),
            worker_id: worker_id.to_owned(),
            generation: "abcd2345".to_owned(),
            socket_path: socket.clone(),
            journal_path: root
                .join("state/workers")
                .join(claimed_session)
                .join(format!("{worker_id}.json")),
            daemon_socket_path: root.join("runtime/daemon.sock"),
            config: WorkerConfig::new(),
        })
        .await
        .expect("bind worker");
        std::fs::set_permissions(runtime_root, std::fs::Permissions::from_mode(0o700))
            .expect("private runtime root");
        let task = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        (socket, task)
    }

    async fn spawn_initialized_worker(
        root: &std::path::Path,
        runtime_root: &std::path::Path,
        session_id: &str,
        worker_id: &str,
    ) -> (
        crate::runtime::Worker,
        WorkerInstanceId,
        u32,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_initialized_worker_with_command(
            root,
            runtime_root,
            session_id,
            worker_id,
            "sleep 30".to_owned(),
        )
        .await
    }

    /// Starts a worker whose PTY also runs an in-tree identity reporter.
    ///
    /// The PTY command is compound on purpose: `sh` execs a single simple
    /// command, so `sh -c "sleep 30"` leaves a PTY root that can never spawn a
    /// hook client. Keeping the shell alive makes the reporter a descendant of
    /// the root, which is what lets it report the root's identity.
    async fn spawn_initialized_worker_with_reporter(
        root: &std::path::Path,
        runtime_root: &std::path::Path,
        session_id: &str,
        worker_id: &str,
    ) -> (
        crate::runtime::Worker,
        WorkerInstanceId,
        u32,
        tokio::task::JoinHandle<()>,
        Reporter,
    ) {
        let (controller, worker_instance_id, child_pid, task, mut reporters) =
            spawn_initialized_worker_with_reporters(
                root,
                runtime_root,
                session_id,
                worker_id,
                &["agent"],
            )
            .await;
        let reporter = reporters.pop().expect("one reporter");
        (controller, worker_instance_id, child_pid, task, reporter)
    }

    /// Starts a worker whose PTY runs one in-tree reporter per requested name.
    async fn spawn_initialized_worker_with_reporters(
        root: &std::path::Path,
        runtime_root: &std::path::Path,
        session_id: &str,
        worker_id: &str,
        names: &[&str],
    ) -> (
        crate::runtime::Worker,
        WorkerInstanceId,
        u32,
        tokio::task::JoinHandle<()>,
        Vec<Reporter>,
    ) {
        let (script, reporters) = identity_reporters(root, names);
        let mut command = String::new();
        for reporter in &reporters {
            command.push_str(&reporter.launch(&script));
            command.push(' ');
        }
        command.push_str("printf ready; while :; do sleep 1; done");
        let (controller, worker_instance_id, child_pid, task) =
            spawn_initialized_worker_with_command(
                root,
                runtime_root,
                session_id,
                worker_id,
                command,
            )
            .await;
        for reporter in &reporters {
            wait_for_directory(&reporter.inbox).await;
        }
        (controller, worker_instance_id, child_pid, task, reporters)
    }

    async fn spawn_initialized_worker_with_command(
        root: &std::path::Path,
        runtime_root: &std::path::Path,
        session_id: &str,
        worker_id: &str,
        command: String,
    ) -> (
        crate::runtime::Worker,
        WorkerInstanceId,
        u32,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_initialized_worker_with_launch(
            root,
            runtime_root,
            session_id,
            worker_id,
            LaunchIdentity {
                agent: "shell".to_owned(),
                agent_base: "shell".to_owned(),
                reference_kind: None,
            },
            PathBuf::from("/bin/sh"),
            command,
        )
        .await
    }

    /// Starts a worker whose PTY runs `executable -c command` and whose launch
    /// claims are matched against `launch`.
    async fn spawn_initialized_worker_with_launch(
        root: &std::path::Path,
        runtime_root: &std::path::Path,
        session_id: &str,
        worker_id: &str,
        launch: LaunchIdentity,
        executable: PathBuf,
        command: String,
    ) -> (
        crate::runtime::Worker,
        WorkerInstanceId,
        u32,
        tokio::task::JoinHandle<()>,
    ) {
        let (socket, task) =
            spawn_uninitialized_worker(root, runtime_root, session_id, session_id, worker_id).await;
        let controller = crate::runtime::Worker::connect(&socket, session_id, "crash-replay-setup")
            .await
            .expect("connect setup controller");
        let worker_instance_id = controller
            .initialize(Initialize {
                session_id: WorkerSessionId::new(session_id).expect("session id"),
                daemon_instance_id: Some(
                    pohunek_worker_protocol::DaemonId::new("daemon-reconcile-test")
                        .expect("daemon id"),
                ),
                transaction_id: TransactionId::new(format!("create-{session_id}"))
                    .expect("transaction id"),
                expected_worker_id: controller.worker_id().await,
                launch,
                executable,
                arguments: vec!["-c".to_owned(), command],
                cwd: root.to_path_buf(),
                dimensions: Dimensions::new(80, 24).expect("dimensions"),
                environment: SecretEnv::new(BTreeMap::new()).expect("environment"),
                base_environment: Some(
                    crate::runtime::environment::base_environment(
                        pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST,
                        &crate::test_support::thread_environment_source(),
                    )
                    .expect("base environment"),
                ),
                limits: InitializeLimits::new(1_000_000, 100_000, 128, 60_000).expect("limits"),
                stop_policy: StopPolicy::new(100).expect("stop policy"),
                hook_protocol_version: Version::new(1).expect("version"),
                public_protocol_version: protocol::PROTOCOL_VERSION.get(),
                hook_schema: Some("identity-subagent-v1".to_owned()),
            })
            .await
            .expect("initialize worker");
        let child_pid = controller
            .inspect()
            .await
            .expect("inspect initialized worker")
            .child_process
            .expect("child process")
            .pid;
        (controller, worker_instance_id, child_pid, task)
    }

    fn spawn_incompatible_worker(
        runtime_root: &std::path::Path,
        slot: &str,
    ) -> tokio::task::JoinHandle<()> {
        let directory = runtime_root.join(slot);
        std::fs::create_dir_all(&directory).expect("create incompatible runtime directory");
        std::fs::set_permissions(runtime_root, std::fs::Permissions::from_mode(0o700))
            .expect("private runtime root");
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .expect("private runtime directory");
        let socket = directory.join(pohunek_paths::WORKER_SOCKET_NAME);
        let listener = UnixListener::bind(&socket).expect("bind incompatible endpoint");
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .expect("private incompatible socket");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept discovery");
            let (read, write) = stream.into_split();
            let mut reader = ControlReader::new(read);
            let mut writer = ControlWriter::new(write);
            let Some(ControlMessage::Request(request)) = reader
                .read::<ControlMessage>()
                .await
                .expect("read negotiation")
            else {
                return;
            };
            writer
                .write(&ControlMessage::Response(ControlResponse {
                    request_id: request.request_id,
                    kind: ResponseKind::Error {
                        error: ControlError {
                            code: ControlCode::WorkerProtocolIncompatible,
                            message: "no compatible worker protocol version".to_owned(),
                            retryable: false,
                        },
                    },
                }))
                .await
                .expect("write incompatibility");
            writer.flush().await.expect("flush incompatibility");
        })
    }

    fn persist_identity_record(root: &std::path::Path, id: &str, worker_id: &str) {
        let mut record = identity_record();
        record.session_id = id.to_owned();
        record.info.id = SessionId(id.to_owned());
        record.runtime.worker_id = Some(worker_id.to_owned());
        record.runtime.worker_instance_id = None;
        record.info.runtime.as_mut().expect("runtime").worker_id = Some(worker_id.to_owned());
        record
            .info
            .runtime
            .as_mut()
            .expect("runtime")
            .worker_instance_id = None;
        record.recovery.as_mut().expect("recovery").session_id = id.to_owned();
        bind_test_generation(&mut record);
        Store::new(root.join("data/metadata.jsonl"))
            .record_session(&record)
            .expect("persist logical record");
    }

    fn persist_live_record(
        root: &std::path::Path,
        id: &str,
        worker_id: &str,
        worker_instance_id: &str,
        child_pid: u32,
        desired_state: DesiredState,
        transaction_kind: crate::store::TransactionKind,
    ) {
        let mut record = identity_record();
        record.session_id = id.to_owned();
        record.info.id = SessionId(id.to_owned());
        record.info.pid = child_pid;
        record.info.agent = "shell".to_owned();
        record.info.agent_base = RuntimeRef::shell();
        record.info.runtime.as_mut().expect("runtime").worker_id = Some(worker_id.to_owned());
        record
            .info
            .runtime
            .as_mut()
            .expect("runtime")
            .worker_instance_id = Some(worker_instance_id.to_owned());
        record.runtime.worker_id = Some(worker_id.to_owned());
        record.runtime.worker_instance_id = Some(worker_instance_id.to_owned());
        record.desired_state = desired_state;
        let transaction_label = match transaction_kind {
            crate::store::TransactionKind::Create => "create",
            crate::store::TransactionKind::Stop => "stop",
            crate::store::TransactionKind::Recover => "recover",
            crate::store::TransactionKind::Remove => "remove",
        };
        record.transaction = Some(crate::store::SessionTransaction {
            id: format!("{transaction_label}-{id}"),
            kind: transaction_kind,
            phase: "requested".to_owned(),
            previous_worker_id: None,
            previous_worker_instance_id: None,
            daemon_instance_id: None,
        });
        let recovery = record.recovery.as_mut().expect("recovery");
        recovery.session_id = id.to_owned();
        recovery.agent = "shell".to_owned();
        recovery.agent_base = RuntimeRef::shell();
        recovery.program = "/bin/sh".to_owned();
        recovery.args = vec!["-c".to_owned(), "sleep 30".to_owned()];
        bind_test_generation(&mut record);
        Store::new(root.join("data/metadata.jsonl"))
            .record_session(&record)
            .expect("persist live intent record");
    }

    /// Generation every hand-spawned test worker serves.
    const TEST_GENERATION: &str = "abcd2345";

    /// Executable recorded for hand-built test generations.
    const TEST_WORKER_EXECUTABLE: &str = "/opt/pohunek/libexec/pohunek/1.0.0/pohunek-sessiond";

    /// Names [`TEST_GENERATION`] of the record's session as its current job.
    fn bind_test_generation(record: &mut SessionRecord) {
        record.runtime.service_id = Some(format!("{}.{TEST_GENERATION}", record.session_id));
        record.runtime.generation = Some(TEST_GENERATION.to_owned());
        record.runtime.executable = Some(PathBuf::from(TEST_WORKER_EXECUTABLE));
    }

    fn test_native_launch(kind: SessionRefKind, fork: bool) -> NativeSessionLaunch {
        NativeSessionLaunch::from_templates(
            kind,
            &["--resume", "{reference}"],
            fork.then_some(&["--resume", "{reference}", "--fork-session"][..]),
        )
        .expect("valid test templates")
    }

    fn identity_record() -> SessionRecord {
        let created_at = "2026-07-23T00:00:00Z".to_owned();
        SessionRecord {
            schema_version: 1,
            session_id: "s-identity".to_owned(),
            desired_state: DesiredState::Running,
            transaction: None,
            native_identity_ordering: None,
            info: SessionInfo {
                id: SessionId("s-identity".to_owned()),
                external: Some(false),
                name: None,
                agent: "codex".to_owned(),
                agent_base: RuntimeRef::codex(),
                cwd: PathBuf::from("/repo"),
                cwd_source: Some(CwdSource::Launch),
                pid: 50,
                runtime: Some(SessionRuntime {
                    state: RuntimeState::Live,
                    runtime_generation: protocol::RuntimeGeneration::new(1),
                    worker_id: Some("worker-identity".to_owned()),
                    worker_instance_id: Some("runtime-identity".to_owned()),
                    started_at: Some(created_at.clone()),
                    last_connected_at: Some(created_at.clone()),
                    loss_reason: None,
                }),
                cols: 80,
                rows: 24,
                state: SessionState::Running,
                state_source: StateSource::Process,
                activity: None,
                subagents: Vec::new(),
                active_agent: None,
                active_agent_base: None,
                active_agent_pid: None,
                active_agent_session_id: None,
                active_agent_session_path: None,
                native_session_id: None,
                native_session_path: None,
                native_last_activity_at: None,
                project_id: None,
                project_label: None,
                is_linked_worktree: None,
                repo: None,
                branch: None,
                worktree_path: None,
                warnings: Vec::new(),
                metadata: BTreeMap::new(),
                created_at: created_at.clone(),
                updated_at: created_at,
                exit_code: None,
                capabilities: protocol::SessionCapabilities {
                    resume: true,
                    fork: false,
                },
            },
            recovery: Some(ResumeBinding {
                session_id: "s-identity".to_owned(),
                name: None,
                agent: "codex".to_owned(),
                agent_base: RuntimeRef::codex(),
                cwd: PathBuf::from("/repo"),
                cols: 80,
                rows: 24,
                native_session_id: None,
                native_session_path: None,
                project_id: None,
                is_linked_worktree: None,
                metadata: BTreeMap::new(),
                program: "codex".to_owned(),
                args: Vec::new(),
                input_rules: StoredInputRules::default(),
                native_launch: Some(test_native_launch(SessionRefKind::Id, false)),
                launch_binding: crate::agent::host::LaunchPin::Unpinned,
                native_reference_provenance: crate::agent::NativeReferenceProvenance::default(),
                profile_revision: None,
                native_launch_unresolved: false,
            }),
            runtime: RuntimeRecord {
                state: RuntimeState::Live,
                worker_id: Some("worker-identity".to_owned()),
                worker_instance_id: Some("runtime-identity".to_owned()),
                service_id: None,
                generation: None,
                executable: None,
                reason: None,
            },
        }
    }

    /// A record whose reference core assigned at launch, naming `reference`.
    fn assigned_record(reference: &str) -> SessionRecord {
        let mut record = identity_record();
        record.info.native_session_id = Some(reference.to_owned());
        let binding = record.recovery.as_mut().expect("recovery binding");
        binding.native_session_id = Some(reference.to_owned());
        binding.native_reference_provenance = NativeReferenceProvenance::Assigned;
        binding.native_launch = Some(
            test_native_launch(SessionRefKind::Id, false)
                .with_assigned(crate::agent::AssignedReference::new(
                    crate::agent::NativeArgs::from_template(&["--session-id", "{reference}"])
                        .expect("launch template"),
                    crate::agent::ReferenceExistence::Unchecked,
                ))
                .expect("an id spec accepts an assignment"),
        );
        record
    }

    /// A snapshot of the launch process (`codex`, pid 50) reporting `launch`
    /// as its launch claim and, when given, `active` (process, sequence,
    /// reference) as its active identity.
    fn switch_snapshot(launch: Option<&str>, active: Option<(u32, u64, &str)>) -> InspectSnapshot {
        let mut snapshot = identity_snapshot("unused");
        snapshot.launch_identity = launch.map(|reference| ReportedLaunchIdentity {
            provider: "codex".to_owned(),
            process: ProcessIdentity {
                pid: 50,
                start_identity: 500,
            },
            reference_kind: "id".to_owned(),
            native_reference: reference.to_owned(),
        });
        snapshot.active_identity = active.map(|(pid, sequence, reference)| ActiveIdentityClaim {
            provider: "codex".to_owned(),
            process: ProcessIdentity {
                pid,
                start_identity: u64::from(pid) * 10,
            },
            sequence,
            expires_at: (OffsetDateTime::now_utc() + time::Duration::seconds(30))
                .format(&Rfc3339)
                .expect("format expiry"),
            reference_kind: Some("id".to_owned()),
            native_reference: Some(reference.to_owned()),
        });
        snapshot
    }

    fn provenance_of(record: &SessionRecord) -> NativeReferenceProvenance {
        record
            .recovery
            .as_ref()
            .expect("recovery binding")
            .native_reference_provenance
    }

    #[test]
    fn a_launch_claim_supersedes_an_assigned_reference_whether_or_not_the_values_agree() {
        for claimed in ["assigned-ref", "reported-ref"] {
            let mut record = assigned_record("assigned-ref");

            import_worker_identities(&mut record, &switch_snapshot(Some(claimed), None))
                .expect("a launch claim supersedes an assigned reference");

            assert_eq!(
                record.info.native_session_id.as_deref(),
                Some(claimed),
                "{claimed}: the session holds the claimed reference"
            );
            assert_eq!(
                record
                    .recovery
                    .as_ref()
                    .and_then(|binding| binding.native_session_id.as_deref()),
                Some(claimed),
                "{claimed}: the binding holds the claimed reference"
            );
            assert_eq!(
                provenance_of(&record),
                NativeReferenceProvenance::Reported,
                "{claimed}: the claim is process-bound"
            );
            assert_eq!(
                record
                    .native_identity_ordering
                    .as_ref()
                    .map(|ordering| (ordering.sequence, ordering.worker_sequence)),
                Some((None, None)),
                "{claimed}: a launch claim keys its generation with no report mark"
            );
        }
    }

    #[test]
    fn a_launch_claim_supersedes_a_reference_that_predates_its_generation_only() {
        let ordered_in = |instance: &str| NativeIdentityOrdering {
            worker_instance_id: instance.to_owned(),
            pid: 50,
            pid_start_identity: 500,
            sequence: Some(4),
            worker_sequence: None,
        };

        // A reported reference of an earlier generation (a recovered
        // conversation) is what the new generation's first claim replaces.
        let mut recovered = assigned_record("first");
        import_worker_identities(&mut recovered, &switch_snapshot(Some("first"), None))
            .expect("the first claim");
        recovered.native_identity_ordering = Some(ordered_in("an-earlier-instance"));
        import_worker_identities(&mut recovered, &switch_snapshot(Some("other"), None))
            .expect("the claim of the new generation supersedes");
        assert_eq!(recovered.info.native_session_id.as_deref(), Some("other"));
        assert_eq!(
            provenance_of(&recovered),
            NativeReferenceProvenance::Reported
        );

        // A report this generation already accepted makes the claim history.
        let mut reported = assigned_record("first");
        import_worker_identities(&mut reported, &switch_snapshot(Some("first"), None))
            .expect("the first claim");
        reported.native_identity_ordering = Some(ordered_in("runtime-identity"));
        import_worker_identities(&mut reported, &switch_snapshot(Some("other"), None))
            .expect("a superseded launch claim is history");
        assert_eq!(reported.info.native_session_id.as_deref(), Some("first"));

        // An already accepted report of this hook runtime makes its launch
        // claim history too.
        let mut hooked = identity_record();
        hooked.info.native_session_id = Some("first".to_owned());
        hooked
            .recovery
            .as_mut()
            .expect("recovery binding")
            .native_session_id = Some("first".to_owned());
        hooked.native_identity_ordering = Some(ordered_in("runtime-identity"));
        import_worker_identities(&mut hooked, &switch_snapshot(Some("other"), None))
            .expect("the accepted hook report supersedes the launch claim");
        assert_eq!(hooked.info.native_session_id.as_deref(), Some("first"));

        // A same-generation key for another process cannot overrule the
        // verified launch process's first claim of a hook runtime.
        let mut foreign = hooked;
        let ordering = foreign
            .native_identity_ordering
            .as_mut()
            .expect("same-generation ordering");
        ordering.pid = 51;
        ordering.pid_start_identity = 510;
        assert_eq!(
            import_worker_identities(&mut foreign, &switch_snapshot(Some("other"), None))
                .expect_err("foreign ordering cannot supersede the hook launch claim"),
            "launch_identity_reference_mismatch"
        );
        assert_eq!(foreign.info.native_session_id.as_deref(), Some("first"));
    }

    #[test]
    fn a_hook_active_claim_without_a_journaled_reference_needs_launch_proof_and_order() {
        let mut record = identity_record();
        let current = switch_snapshot(Some("first"), Some((50, 5, "switched")));
        import_worker_identities(&mut record, &current).expect("verified hook switch");
        assert_eq!(record.info.native_session_id.as_deref(), Some("switched"));
        assert_eq!(
            record
                .native_identity_ordering
                .as_ref()
                .and_then(|ordering| ordering.worker_sequence),
            Some(5)
        );

        let stale = switch_snapshot(Some("first"), Some((50, 4, "stale")));
        import_worker_identities(&mut record, &stale).expect("stale claim is ignored");
        assert_eq!(record.info.native_session_id.as_deref(), Some("switched"));

        let nested = switch_snapshot(Some("first"), Some((51, 6, "nested")));
        import_worker_identities(&mut record, &nested).expect("nested claim is not a native ref");
        assert_eq!(record.info.native_session_id.as_deref(), Some("switched"));
    }

    #[test]
    fn the_active_identity_of_the_launch_process_replaces_the_reference_in_sequence_order() {
        let mut record = assigned_record("assigned-ref");
        let claim = switch_snapshot(Some("assigned-ref"), Some((50, 9, "cleared-ref")));

        import_worker_identities(&mut record, &claim).expect("import the switch");

        assert_eq!(
            record.info.native_session_id.as_deref(),
            Some("cleared-ref")
        );
        assert_eq!(
            record
                .recovery
                .as_ref()
                .and_then(|binding| binding.native_session_id.as_deref()),
            Some("cleared-ref")
        );
        assert_eq!(provenance_of(&record), NativeReferenceProvenance::Reported);
        assert_eq!(
            record.native_identity_ordering,
            Some(NativeIdentityOrdering {
                worker_instance_id: "runtime-identity".to_owned(),
                pid: 50,
                pid_start_identity: 500,
                sequence: None,
                worker_sequence: Some(9),
            })
        );

        // The worker still holds its first launch claim; re-importing the same
        // snapshot is neither a conflict nor a change.
        let before = record.clone();
        import_worker_identities(&mut record, &claim).expect("the same snapshot stays applied");
        assert_eq!(record.info.native_session_id, before.info.native_session_id);
        assert_eq!(
            record.native_identity_ordering,
            before.native_identity_ordering
        );

        let stale = switch_snapshot(Some("assigned-ref"), Some((50, 8, "older-ref")));
        import_worker_identities(&mut record, &stale).expect("import a stale claim");
        assert_eq!(
            record.info.native_session_id.as_deref(),
            Some("cleared-ref"),
            "a lower sequence never replaces a newer reference"
        );

        let newer = switch_snapshot(Some("assigned-ref"), Some((50, 10, "newest-ref")));
        import_worker_identities(&mut record, &newer).expect("import a newer claim");
        assert_eq!(record.info.native_session_id.as_deref(), Some("newest-ref"));
    }

    #[test]
    fn a_recovered_generations_first_report_with_sequence_zero_is_admitted() {
        // A recovered runtime keeps its reported reference and holds no key of
        // the new generation: its first worker report is numbered 0.
        let mut record = assigned_record("assigned-ref");
        record
            .recovery
            .as_mut()
            .expect("recovery binding")
            .native_reference_provenance = NativeReferenceProvenance::Reported;
        let mut snapshot = switch_snapshot(Some("launch-ref"), None);
        snapshot.native_reference = Some(pohunek_worker_protocol::ReportedNativeReference {
            provider: "codex".to_owned(),
            process: ProcessIdentity {
                pid: 50,
                start_identity: 500,
            },
            sequence: 0,
            reference_kind: "id".to_owned(),
            native_reference: "first-report".to_owned(),
        });

        super::import_native_reference(&mut record, &snapshot);
        import_worker_identities(&mut record, &snapshot).expect("import the launch claim");

        assert_eq!(
            record.info.native_session_id.as_deref(),
            Some("first-report")
        );
        assert_eq!(
            record
                .native_identity_ordering
                .as_ref()
                .map(|ordering| ordering.worker_sequence),
            Some(Some(0)),
            "only the accepted report sets the worker mark"
        );
    }

    #[test]
    fn the_verified_launch_process_below_a_wrapper_is_the_process_whose_switches_count() {
        // The worker designates the provider process below a wrapper; the
        // wrapper stays the root child.
        let wrapped = |active_pid: u32| {
            let mut snapshot =
                switch_snapshot(Some("assigned-ref"), Some((active_pid, 9, "cleared-ref")));
            snapshot
                .launch_identity
                .as_mut()
                .expect("launch claim")
                .process = ProcessIdentity {
                pid: 55,
                start_identity: 550,
            };
            snapshot
        };
        let mut record = assigned_record("assigned-ref");
        import_worker_identities(&mut record, &wrapped(55)).expect("import the switch");
        assert_eq!(
            record.info.native_session_id.as_deref(),
            Some("cleared-ref")
        );

        for (name, pid) in [("the wrapper", 50), ("another nested agent", 60)] {
            let mut record = assigned_record("assigned-ref");
            import_worker_identities(&mut record, &wrapped(pid)).expect("import the claim");
            assert_eq!(
                record.info.native_session_id.as_deref(),
                Some("assigned-ref"),
                "{name} is not the verified launch process"
            );
        }
    }

    #[test]
    fn a_stale_writer_never_demotes_a_newer_reference_of_the_other_transport() {
        let ordered = |sequence: u64, worker_sequence: Option<u64>, reference: &str| {
            let mut record = identity_record();
            record.native_identity_ordering = Some(NativeIdentityOrdering {
                worker_instance_id: "runtime-identity".to_owned(),
                pid: 50,
                pid_start_identity: 500,
                sequence: Some(sequence),
                worker_sequence,
            });
            record.info.native_session_id = Some(reference.to_owned());
            record
                .recovery
                .as_mut()
                .expect("recovery binding")
                .native_session_id = Some(reference.to_owned());
            record
        };
        // The store accepted a worker claim after the public report the
        // writer's record was built from: the writer is behind in the worker
        // domain and not ahead in the public one.
        let durable = ordered(5, Some(900), "worker-conversation");
        let mut stale = ordered(5, None, "public-conversation");
        crate::store::preserve_newer_native_identity(&durable, &mut stale);
        assert_eq!(
            stale.info.native_session_id.as_deref(),
            Some("worker-conversation")
        );
        assert_eq!(
            stale.native_identity_ordering,
            durable.native_identity_ordering
        );

        // A writer that accepted a newer public report keeps its reference and
        // inherits the worker mark, so a late public report numbered like the
        // old one is still stale.
        let mut newer = ordered(6, None, "newer-public-conversation");
        crate::store::preserve_newer_native_identity(&durable, &mut newer);
        assert_eq!(
            newer.info.native_session_id.as_deref(),
            Some("newer-public-conversation")
        );
        let merged = newer.native_identity_ordering.expect("merged ordering");
        assert_eq!(
            (merged.sequence, merged.worker_sequence),
            (Some(6), Some(900))
        );
        assert!(!merged.admits("runtime-identity", crate::store::ReportTransport::Public, 5));
        assert!(!merged.admits(
            "runtime-identity",
            crate::store::ReportTransport::Worker,
            900
        ));
    }

    #[test]
    fn report_sequences_are_compared_within_their_own_transport_only() {
        use crate::store::ReportTransport::{Public, Worker};
        let mut ordering = NativeIdentityOrdering::accepting(None, "gen", 1, 2, Worker, 5_000_000);
        assert!(!ordering.admits("gen", Worker, 5_000_000));
        assert!(ordering.admits("gen", Worker, 5_000_001));
        assert!(
            ordering.admits("gen", Public, 7),
            "a public sequence is not older than a worker clock"
        );
        assert!(
            ordering.admits("other-gen", Worker, 1),
            "a new generation restarts"
        );

        ordering = NativeIdentityOrdering::accepting(Some(&ordering), "gen", 1, 2, Public, 7);
        assert_eq!(
            (ordering.sequence, ordering.worker_sequence),
            (Some(7), Some(5_000_000))
        );
        assert!(!ordering.admits("gen", Public, 7));
        assert!(
            !ordering.admits("gen", Worker, 5_000_000),
            "an accepted public report keeps the worker mark: the old claim is not newer"
        );
        // A worker claim leaves the public mark absent, not at zero, so the
        // first public report of any number is admitted.
        let worker_only = NativeIdentityOrdering::accepting(None, "gen", 1, 2, Worker, 9);
        assert_eq!(worker_only.sequence, None);
        assert!(worker_only.admits("gen", Public, 0));
        let fresh = NativeIdentityOrdering::accepting(Some(&ordering), "next", 1, 2, Public, 3);
        assert_eq!((fresh.sequence, fresh.worker_sequence), (Some(3), None));
    }

    #[test]
    fn an_active_identity_that_is_not_the_launch_process_never_replaces_the_reference() {
        // A nested process, another runtime's claim and a runtime whose
        // reference comes from a hook all leave the stored reference alone.
        let nested = switch_snapshot(None, Some((60, 9, "nested-ref")));
        let mut other_provider = switch_snapshot(None, Some((50, 9, "claude-ref")));
        other_provider
            .active_identity
            .as_mut()
            .expect("active identity")
            .provider = "claude".to_owned();
        for (name, snapshot) in [("nested", nested), ("other provider", other_provider)] {
            let mut record = assigned_record("assigned-ref");
            import_worker_identities(&mut record, &snapshot).expect("import the claim");
            assert_eq!(
                record.info.native_session_id.as_deref(),
                Some("assigned-ref"),
                "{name}"
            );
            assert_eq!(
                provenance_of(&record),
                NativeReferenceProvenance::Assigned,
                "{name}"
            );
            assert!(record.native_identity_ordering.is_none(), "{name}");
        }

        let mut hooked = identity_record();
        import_worker_identities(
            &mut hooked,
            &switch_snapshot(None, Some((50, 9, "hook-ref"))),
        )
        .expect("import the claim");
        assert_eq!(
            hooked.info.native_session_id, None,
            "a hook runtime keeps its import rules"
        );
    }

    #[test]
    fn a_resume_projection_naming_the_assigned_value_loses_to_the_reported_record() {
        // The session record commits before the resume projection, so a crash
        // between them leaves the projection on the replaced assigned value.
        let mut record = assigned_record("assigned-ref");
        import_worker_identities(&mut record, &switch_snapshot(Some("reported-ref"), None))
            .expect("supersede the assigned reference");
        let mut stale = record.recovery.clone().expect("recovery binding");
        stale.native_session_id = Some("assigned-ref".to_owned());
        stale.native_reference_provenance = NativeReferenceProvenance::Assigned;

        merge_persisted_recovery(&mut record, stale).expect("the record wins");

        assert_eq!(
            record.info.native_session_id.as_deref(),
            Some("reported-ref")
        );
        assert_eq!(
            record
                .recovery
                .as_ref()
                .and_then(|binding| binding.native_session_id.as_deref()),
            Some("reported-ref")
        );
        assert_eq!(provenance_of(&record), NativeReferenceProvenance::Reported);

        // Without an ordering key neither side is newer, so two values that
        // disagree stay a conflict.
        record.native_identity_ordering = None;
        let mut conflicting = record.recovery.clone().expect("recovery binding");
        conflicting.native_session_id = Some("another-ref".to_owned());
        assert_eq!(
            merge_persisted_recovery(&mut record, conflicting)
                .expect_err("reported values must agree"),
            "resume_binding_reference_mismatch"
        );
    }

    #[test]
    fn equal_native_ordering_fills_only_missing_durable_reference() {
        let root = temp_root();
        let store = Store::new(root.join("metadata.jsonl"));
        let mut existing = identity_record();
        existing.native_identity_ordering = Some(NativeIdentityOrdering {
            worker_instance_id: "runtime-identity".to_owned(),
            pid: 50,
            pid_start_identity: 500,
            sequence: Some(7),
            worker_sequence: None,
        });
        assert_eq!(
            store.record_session(&existing).expect("seed session"),
            SessionWriteOutcome::Applied
        );

        let mut merged = existing;
        merged.info.native_session_id = Some("native-from-binding".to_owned());
        merged
            .recovery
            .as_mut()
            .expect("recovery")
            .native_session_id = Some("native-from-binding".to_owned());
        assert_eq!(
            store
                .record_session(&merged)
                .expect("fill missing native reference"),
            SessionWriteOutcome::Applied
        );
        let committed = store
            .load_sessions()
            .expect("load committed session")
            .pop()
            .expect("committed session");
        assert_eq!(
            committed.info.native_session_id.as_deref(),
            Some("native-from-binding")
        );
        assert_eq!(
            committed
                .recovery
                .and_then(|binding| binding.native_session_id),
            Some("native-from-binding".to_owned())
        );
    }

    #[test]
    fn equal_native_ordering_preserves_conflicting_durable_reference() {
        let root = temp_root();
        let store = Store::new(root.join("metadata.jsonl"));
        let mut existing = identity_record();
        existing.native_identity_ordering = Some(NativeIdentityOrdering {
            worker_instance_id: "runtime-identity".to_owned(),
            pid: 50,
            pid_start_identity: 500,
            sequence: Some(7),
            worker_sequence: None,
        });
        existing.info.native_session_id = Some("native-authoritative".to_owned());
        existing
            .recovery
            .as_mut()
            .expect("recovery")
            .native_session_id = Some("native-authoritative".to_owned());
        store.record_session(&existing).expect("seed session");

        let mut conflicting = existing;
        conflicting.info.native_session_id = Some("native-conflict".to_owned());
        conflicting
            .recovery
            .as_mut()
            .expect("recovery")
            .native_session_id = Some("native-conflict".to_owned());
        store
            .record_session(&conflicting)
            .expect("reject equal-sequence conflict by preservation");
        let committed = store
            .load_sessions()
            .expect("load committed session")
            .pop()
            .expect("committed session");
        assert_eq!(
            committed.info.native_session_id.as_deref(),
            Some("native-authoritative")
        );
        assert_eq!(
            committed
                .recovery
                .and_then(|binding| binding.native_session_id),
            Some("native-authoritative".to_owned())
        );
    }

    #[test]
    fn runtime_record_identity_authorizes_reconciliation_normalization() {
        let root = temp_root();
        let store = Store::new(root.join("metadata.jsonl"));
        let mut inconsistent = identity_record();
        inconsistent.runtime.worker_instance_id = Some("runtime-journal".to_owned());
        store
            .record_session(&inconsistent)
            .expect("seed denormalized runtime mismatch");

        let mut normalized = inconsistent;
        normalized
            .info
            .runtime
            .as_mut()
            .expect("runtime info")
            .worker_instance_id = Some("runtime-journal".to_owned());
        normalized.info.state = SessionState::Done;
        normalized.runtime.state = RuntimeState::Terminal;
        assert_eq!(
            store
                .record_session(&normalized)
                .expect("normalize from authoritative runtime record"),
            SessionWriteOutcome::Applied
        );
        let committed = store
            .load_sessions()
            .expect("load normalized session")
            .pop()
            .expect("normalized session");
        assert_eq!(committed.info.state, SessionState::Done);
        assert_eq!(
            committed
                .info
                .runtime
                .and_then(|runtime| runtime.worker_instance_id),
            Some("runtime-journal".to_owned())
        );
    }

    #[tokio::test]
    async fn deleted_profile_keeps_frozen_fork_disable_after_restart() {
        let root = temp_root();
        let store_path = root.join("data/metadata.jsonl");
        let mut record = identity_record();
        record.info.agent = "deleted-profile".to_owned();
        record.info.agent_base = RuntimeRef::claude();
        record.info.native_session_id = Some("native-before-restart".to_owned());
        let recovery = record.recovery.as_mut().expect("recovery binding");
        recovery.agent = "deleted-profile".to_owned();
        recovery.agent_base = RuntimeRef::claude();
        recovery.native_session_id = Some("native-before-restart".to_owned());
        recovery.program = "/bin/sh".to_owned();
        recovery.native_launch = Some(test_native_launch(SessionRefKind::Id, false));
        Store::new(store_path.clone())
            .record_session(&record)
            .expect("persist deleted-profile session");

        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store_path),
            worker_runtime_root: Some(root.join("runtime")),
            ..SessionRegistryConfig::default()
        });
        Box::pin(registry.reconcile_workers())
            .await
            .expect("rehydrate session without its profile");

        let error = registry
            .fork(SessionForkParams {
                session_id: SessionId("s-identity".to_owned()),
                name: None,
                cwd_mode: ForkCwdMode::Same,
                cols: 80,
                rows: 24,
                accept_profile_change: false,
            })
            .await
            .expect_err("frozen fork disable survives profile deletion and restart");
        assert_eq!(error.code, "agent_fork_unsupported");
        assert_eq!(registry.list().await.len(), 1);
    }

    #[tokio::test]
    async fn legacy_record_without_fork_fields_stays_fail_closed_after_restart() {
        let root = temp_root();
        let store_path = root.join("data/metadata.jsonl");
        let mut record = identity_record();
        record.info.agent = "claude".to_owned();
        record.info.agent_base = RuntimeRef::claude();
        record.info.native_session_id = Some("legacy-native".to_owned());
        let recovery = record.recovery.as_mut().expect("recovery binding");
        recovery.agent = "claude".to_owned();
        recovery.agent_base = RuntimeRef::claude();
        recovery.native_session_id = Some("legacy-native".to_owned());
        recovery.program = "claude".to_owned();
        recovery.native_launch = Some(test_native_launch(SessionRefKind::Id, false));
        Store::new(store_path.clone())
            .record_session(&record)
            .expect("persist pre-migration record shape");

        let line = std::fs::read_to_string(&store_path).expect("read stored record");
        let mut value: serde_json::Value = serde_json::from_str(line.trim()).expect("parse record");
        let recovery = value
            .get_mut("recovery")
            .and_then(serde_json::Value::as_object_mut)
            .expect("serialized recovery object");
        recovery.remove("native_launch");
        value
            .get_mut("info")
            .and_then(serde_json::Value::as_object_mut)
            .expect("serialized session info")
            .remove("capabilities");
        let legacy = format!(
            "{}\n",
            serde_json::to_string(&value).expect("serialize legacy record")
        );
        std::fs::write(&store_path, legacy).expect("write legacy record");

        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store_path),
            worker_runtime_root: Some(root.join("runtime")),
            ..SessionRegistryConfig::default()
        });
        Box::pin(registry.reconcile_workers())
            .await
            .expect("rehydrate legacy record");

        let resume_error = registry
            .resume(&SessionId("s-identity".to_owned()))
            .await
            .expect_err("legacy session capabilities must not infer current resume support");
        assert_eq!(resume_error.code, "agent_not_resumable");

        let error = registry
            .fork(SessionForkParams {
                session_id: SessionId("s-identity".to_owned()),
                name: None,
                cwd_mode: ForkCwdMode::Same,
                cols: 80,
                rows: 24,
                accept_profile_change: false,
            })
            .await
            .expect_err("legacy binding must not infer current Claude fork support");
        assert_eq!(error.code, "agent_fork_unsupported");
        assert_eq!(registry.list().await.len(), 1);
    }

    /// Store written by the v0.33.1 daemon for a session whose native launch
    /// spec was lost (see `store/fixtures/v0.33.1-damaged`).
    const V0_33_1_DAMAGED_STORE: &str =
        include_str!("../store/fixtures/v0.33.1-damaged/metadata.jsonl");

    /// Migrates `store` content for agent `agent`, then rehydrates it into a
    /// registry whose agent profiles come from `profiles` (name, TOML body).
    async fn rehydrate_damaged_store(
        agent: &str,
        profiles: &[(&str, &str)],
    ) -> (SessionRegistry, crate::test_support::ScopedDir) {
        let root = temp_root();
        let data = root.join("data");
        create_private_dir(&data);
        let store_path = data.join("metadata.jsonl");
        std::fs::write(
            &store_path,
            V0_33_1_DAMAGED_STORE
                .replace("\"agent\":\"claude\"", &format!("\"agent\":\"{agent}\"")),
        )
        .expect("write damaged store");
        std::fs::set_permissions(&store_path, std::fs::Permissions::from_mode(0o600))
            .expect("private store");
        let agents = root.join("agents");
        std::fs::create_dir_all(&agents).expect("agents dir");
        for (name, body) in profiles {
            std::fs::write(agents.join(format!("{name}.toml")), body).expect("write profile");
        }
        crate::store::migrate_at_startup(&store_path).expect("migrate the damaged store");

        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store_path),
            agents_dir: Some(agents),
            worker_runtime_root: Some(root.join("runtime")),
            ..SessionRegistryConfig::default()
        });
        Box::pin(registry.reconcile_workers())
            .await
            .expect("rehydrate damaged store");
        (registry, root)
    }

    #[tokio::test]
    async fn a_damaged_built_in_session_is_listed_as_recoverable_and_stays_lost() {
        let (registry, _root) = rehydrate_damaged_store("claude", &[]).await;

        let sessions = registry.list().await;
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert!(
            session.capabilities.resume && session.capabilities.fork,
            "{session:?}"
        );
        assert!(session.warnings.is_empty(), "{:?}", session.warnings);
        assert_ne!(
            session.runtime.as_ref().map(|runtime| runtime.state),
            Some(RuntimeState::Live),
            "startup never resumes a lost record"
        );
    }

    #[tokio::test]
    async fn a_damaged_profile_session_is_repaired_through_the_registry() {
        let (registry, _root) = rehydrate_damaged_store(
            "work",
            &[("work", "base = \"claude\"\nprogram = \"claude\"\n")],
        )
        .await;

        let sessions = registry.list().await;
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert!(
            session.capabilities.resume && session.capabilities.fork,
            "{session:?}"
        );
        assert!(session.warnings.is_empty(), "{:?}", session.warnings);
        assert_ne!(
            session.runtime.as_ref().map(|runtime| runtime.state),
            Some(RuntimeState::Live),
            "startup never resumes a lost record"
        );
    }

    #[tokio::test]
    async fn an_unrepairable_damaged_session_lists_a_recovery_warning_and_is_not_resumed() {
        let cases: [(&str, &[(&str, &str)]); 2] = [
            ("deleted-profile", &[]),
            (
                "no-resume-profile",
                &[(
                    "no-resume-profile",
                    "base = \"claude\"\nprogram = \"claude\"\n[resume]\nresumable = false\n",
                )],
            ),
        ];
        for (agent, profiles) in cases {
            let (registry, _root) = rehydrate_damaged_store(agent, profiles).await;

            let sessions = registry.list().await;
            assert_eq!(sessions.len(), 1, "{agent}: the session stays listed");
            let session = &sessions[0];
            assert!(!session.capabilities.resume, "{agent}");
            let warning = session
                .warnings
                .iter()
                .find(|warning| warning.kind == protocol::SessionWarningKind::NativeRecovery)
                .unwrap_or_else(|| panic!("{agent}: no native recovery warning: {session:?}"));
            assert!(
                warning
                    .message
                    .contains("Start a new session and resume the native conversation"),
                "{agent}: {}",
                warning.message
            );
            assert_eq!(
                warning.detail.as_deref(),
                Some("native session id: native-fixture-2"),
                "{agent}"
            );
            let error = registry
                .resume(&SessionId("s-fixture-session".to_owned()))
                .await
                .expect_err("an unrepairable session is never resumed");
            assert_eq!(error.code, "agent_not_resumable", "{agent}");
        }
    }

    fn identity_snapshot(native_launch: &str) -> InspectSnapshot {
        InspectSnapshot {
            session_id: WorkerSessionId::new("s-identity").expect("session id"),
            worker_id: WorkerId::new("worker-identity").expect("worker id"),
            worker_instance_id: Some(
                WorkerInstanceId::new("runtime-identity").expect("runtime id"),
            ),
            phase: WorkerRuntimePhase::Running,
            worker_process: ProcessIdentity {
                pid: 40,
                start_identity: 400,
            },
            child_process: Some(ProcessIdentity {
                pid: 50,
                start_identity: 500,
            }),
            dimensions: Some(Dimensions::new(80, 24).expect("dimensions")),
            history_start_offset: 0,
            next_offset: 0,
            exit: None,
            launch_identity: Some(ReportedLaunchIdentity {
                provider: "codex".to_owned(),
                process: ProcessIdentity {
                    pid: 50,
                    start_identity: 500,
                },
                reference_kind: "id".to_owned(),
                native_reference: native_launch.to_owned(),
            }),
            active_identity: Some(ActiveIdentityClaim {
                provider: "claude".to_owned(),
                process: ProcessIdentity {
                    pid: 60,
                    start_identity: 600,
                },
                sequence: 7,
                expires_at: (OffsetDateTime::now_utc() + time::Duration::seconds(30))
                    .format(&Rfc3339)
                    .expect("format expiry"),
                reference_kind: Some("id".to_owned()),
                native_reference: Some("nested-native".to_owned()),
            }),
            active_identity_release: None,
            native_reference: None,
            subagents: Vec::new(),
            hook_schema: Some("identity-subagent-v1".to_owned()),
        }
    }

    #[test]
    fn worker_subagents_map_to_public_state_without_provider_payloads() {
        let mut snapshot = identity_snapshot("launch-native");
        snapshot.subagents.push(SubagentSnapshot {
            id: "child-finished".to_owned(),
            parent_id: None,
            provider: "codex".to_owned(),
            agent_type: None,
            phase: SubagentPhase::Completed,
            revision: 6,
            started_at_ms: 200,
            updated_at_ms: 210,
            finished_at_ms: Some(210),
        });
        snapshot.subagents.push(SubagentSnapshot {
            id: "child-1".to_owned(),
            parent_id: Some("parent-1".to_owned()),
            provider: "claude".to_owned(),
            agent_type: Some("Explore".to_owned()),
            phase: SubagentPhase::Running,
            revision: 7,
            started_at_ms: 100,
            updated_at_ms: 110,
            finished_at_ms: None,
        });

        let mapped = import_worker_subagents(&snapshot).expect("valid subagent snapshot");

        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[0].id, "child-1");
        assert_eq!(mapped[0].provider, RuntimeRef::claude());
        assert_eq!(mapped[0].activity, Some(AgentActivity::Working));
        assert_eq!(mapped[0].revision, protocol::SubagentRevision::new(7));
    }

    /// Installs a package whose descriptor names only `[integration] handler`
    /// and returns the pin a session of it froze plus a registry serving it.
    fn pre_schema_package_registry() -> (SessionRegistry, crate::agent::host::LaunchPin) {
        use package::registry::{
            InstallRequest, PackageSource as InstallSource, Registry as PackageRegistry,
        };
        use package::{build_archive, read_archive, ArchiveEntry, Limits};
        use protocol::{
            BindingProvenance, LaunchBinding, PackageId, PackageIdentity, PackageVersion,
        };

        let descriptor = r#"schema = 1
id = "acme.agent"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "acme"
name = "Acme"
program = "acme-agent"
args = []
detect_manifest = "detect.toml"

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--session", "{reference}"]

[fork]
supported = false

[native_reference]
strategy = "hook"

[integration]
handler = "codex-hook-v1"
"#;
        let entries = [
            ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: descriptor.as_bytes().to_vec(),
                executable: false,
            },
            ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: b"[[rules]]\nid = \"idle\"\nstate = \"idle\"\npriority = 100\nregion = \"whole_recent\"\nany = [{ contains = \"ready\" }]\n".to_vec(),
                executable: false,
            },
        ];
        let bytes = build_archive(&entries, &Limits::DEFAULT).expect("archive");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("reads")
            .digest()
            .clone();
        let identity = PackageIdentity {
            id: PackageId::parse("acme.agent").expect("package id"),
            version: PackageVersion::parse("1.0.0").expect("version"),
        };
        let root = crate::test_support::thread_scoped_dir("pre-schema-");
        let plugins = root.join("plugins");
        PackageRegistry::open_at(&plugins, Limits::DEFAULT)
            .expect("registry")
            .install(&InstallRequest {
                archive: &bytes,
                expected: &digest,
                identity: identity.clone(),
                source: InstallSource::ExplicitDigest,
                enabled: true,
                select: true,
                installed_at_unix_seconds: 1_700_000_000,
            })
            .expect("install");
        let host = crate::agent::host::RuntimeHost::with_packages(
            crate::agent::host::BuiltinSource::new("/bin/sh"),
            crate::agent::host::PackageSource::new(
                crate::agent::host::PackageStore::open(&plugins).expect("store"),
            ),
        )
        .expect("host");
        let registry = SessionRegistry::new_with_runtimes(
            SessionRegistryConfig {
                shell_command: hermetic_shell(),
                ..SessionRegistryConfig::default()
            },
            host,
        );
        let pin = crate::agent::host::LaunchPin::Pinned(Box::new(LaunchBinding {
            runtime_id: protocol::RuntimeId::parse("acme").expect("runtime id"),
            provenance: BindingProvenance::Package {
                package: identity,
                package_digest: digest,
            },
        }));
        (registry, pin)
    }

    #[tokio::test]
    async fn a_schemaless_worker_of_a_pre_schema_package_pin_is_reprojected_not_refused() {
        let (registry, pin) = pre_schema_package_registry();
        let mut record = identity_record();
        record.info.agent = "acme".to_owned();
        record.info.agent_base = RuntimeRef::from_wire("acme");
        record
            .recovery
            .as_mut()
            .expect("recovery binding")
            .launch_binding = pin;
        let mut snapshot = identity_snapshot("launch-native");
        snapshot.hook_schema = None;
        snapshot.launch_identity = None;
        snapshot.subagents.push(SubagentSnapshot {
            id: "child".to_owned(),
            parent_id: None,
            provider: "codex".to_owned(),
            agent_type: None,
            phase: SubagentPhase::Running,
            revision: 1,
            started_at_ms: 1,
            updated_at_ms: 1,
            finished_at_ms: None,
        });

        let projected = InspectSnapshot {
            hook_schema: registry.effective_hook_schema_id(&record, &snapshot),
            ..snapshot
        };

        assert_eq!(
            projected.hook_schema.as_deref(),
            Some("identity-subagent-v1")
        );
        import_worker_identities(&mut record, &projected)
            .expect("the nested active identity is admitted");
        assert_eq!(record.info.active_agent.as_deref(), Some("claude"));
        assert_eq!(
            import_worker_subagents(&projected)
                .expect("the subagent is admitted")
                .len(),
            1
        );
    }

    #[test]
    fn worker_claims_are_validated_with_the_snapshot_hook_schema() {
        let mut record = identity_record();
        let mut snapshot = identity_snapshot("launch-native");

        snapshot.hook_schema = Some("identity-v1".to_owned());
        import_worker_identities(&mut record, &snapshot)
            .expect("the identity schema admits the nested provider");

        snapshot.hook_schema = None;
        assert_eq!(
            import_worker_identities(&mut identity_record(), &snapshot)
                .expect_err("a runtime without a schema admits no claim"),
            "active_identity_provider_invalid"
        );

        snapshot.hook_schema = Some("identity-v9".to_owned());
        assert_eq!(
            import_worker_identities(&mut identity_record(), &snapshot)
                .expect_err("an unknown schema id is refused"),
            "hook_schema_unknown"
        );
    }

    #[test]
    fn subagent_providers_follow_the_snapshot_hook_schema() {
        let subagent = |provider: &str| SubagentSnapshot {
            id: "child".to_owned(),
            parent_id: None,
            provider: provider.to_owned(),
            agent_type: None,
            phase: SubagentPhase::Running,
            revision: 1,
            started_at_ms: 1,
            updated_at_ms: 1,
            finished_at_ms: None,
        };
        let mut snapshot = identity_snapshot("launch-native");
        snapshot.subagents.push(subagent("codex"));

        snapshot.hook_schema = Some("identity-v1".to_owned());
        assert_eq!(
            import_worker_subagents(&snapshot).expect_err("identity schema has no subagents"),
            "subagent_provider_invalid"
        );
        snapshot.hook_schema = None;
        assert_eq!(
            import_worker_subagents(&snapshot).expect_err("no schema admits no subagents"),
            "subagent_provider_invalid"
        );
        snapshot.hook_schema = Some("identity-subagent-v1".to_owned());
        assert_eq!(
            import_worker_subagents(&snapshot).expect("admitted").len(),
            1
        );

        snapshot.subagents = vec![subagent("hermes")];
        assert_eq!(
            import_worker_subagents(&snapshot).expect_err("hermes reports no subagents"),
            "subagent_provider_invalid"
        );
        snapshot.subagents.clear();
        snapshot.hook_schema = None;
        assert!(import_worker_subagents(&snapshot)
            .expect("an empty collection needs no schema")
            .is_empty());
    }

    #[tokio::test]
    async fn a_journaled_hook_schema_wins_over_the_schema_of_the_pinned_runtime() {
        let (registry, pin) = pre_schema_package_registry();
        let mut record = identity_record();
        record.info.agent = "acme".to_owned();
        record.info.agent_base = RuntimeRef::from_wire("acme");
        record
            .recovery
            .as_mut()
            .expect("recovery binding")
            .launch_binding = pin;
        let mut snapshot = identity_snapshot("launch-native");
        snapshot.launch_identity = None;
        snapshot.subagents.push(SubagentSnapshot {
            id: "child".to_owned(),
            parent_id: None,
            provider: "codex".to_owned(),
            agent_type: None,
            phase: SubagentPhase::Running,
            revision: 1,
            started_at_ms: 1,
            updated_at_ms: 1,
            finished_at_ms: None,
        });

        snapshot.hook_schema = None;
        assert_eq!(
            registry
                .effective_hook_schema_id(&record, &snapshot)
                .as_deref(),
            Some("identity-subagent-v1"),
            "the pin's schema applies to a worker that journaled none"
        );

        snapshot.hook_schema = Some("identity-v1".to_owned());
        let effective = registry.effective_hook_schema_id(&record, &snapshot);
        assert_eq!(
            effective.as_deref(),
            Some("identity-v1"),
            "the schema the worker journaled outranks the pinned one"
        );
        let projected = InspectSnapshot {
            hook_schema: effective,
            ..snapshot
        };
        assert_eq!(
            import_worker_subagents(&projected)
                .expect_err("the journaled identity schema has no subagent surface"),
            "subagent_provider_invalid"
        );
    }

    #[test]
    fn subagent_claim_fields_and_outcomes_follow_the_snapshot_hook_schema() {
        use pohunek_worker_protocol::{HookSchema, SubagentSequence};

        static BASE: HookSchema = HookSchema {
            id: "fields-test",
            handlers: &[],
            identity_providers: &[],
            subagent_providers: &["codex"],
            actions: &[
                pohunek_worker_protocol::HookAction::SubagentStart,
                pohunek_worker_protocol::HookAction::SubagentStop,
            ],
            reference_kinds: &[],
            ancestry: AncestryMatcher::ManagedPtyDescendant,
            nested_active: true,
            subagent_fields: &[],
            subagent_sequence: Some(SubagentSequence::StopAfterStart),
        };
        static WITH_FIELDS: HookSchema = HookSchema {
            subagent_fields: &[SubagentField::ParentId, SubagentField::AgentType],
            ..BASE
        };
        static WITH_OUTCOME: HookSchema = HookSchema {
            subagent_fields: &[SubagentField::Outcome],
            ..BASE
        };
        static UNORDERED: HookSchema = HookSchema {
            subagent_sequence: None,
            subagent_fields: &[
                SubagentField::ParentId,
                SubagentField::AgentType,
                SubagentField::Outcome,
            ],
            ..BASE
        };
        let subagent = |parent: Option<&str>, kind: Option<&str>, phase: SubagentPhase| {
            vec![SubagentSnapshot {
                id: "child".to_owned(),
                parent_id: parent.map(str::to_owned),
                provider: "codex".to_owned(),
                agent_type: kind.map(str::to_owned),
                phase,
                revision: 1,
                started_at_ms: 1,
                updated_at_ms: 1,
                finished_at_ms: None,
            }]
        };
        let plain = subagent(None, None, SubagentPhase::Completed);
        let parented = subagent(Some("parent"), None, SubagentPhase::Running);
        let typed = subagent(None, Some("Explore"), SubagentPhase::Running);
        let failed = subagent(None, None, SubagentPhase::Failed);
        let cancelled = subagent(None, None, SubagentPhase::Cancelled);
        let lost = subagent(None, None, SubagentPhase::Lost);

        for (claims, schema, expected) in [
            (&plain, &BASE, Ok(())),
            (&lost, &BASE, Ok(())),
            (&parented, &BASE, Err("subagent_field_invalid")),
            (&typed, &BASE, Err("subagent_field_invalid")),
            (&failed, &BASE, Err("subagent_field_invalid")),
            (&cancelled, &BASE, Err("subagent_field_invalid")),
            (&parented, &WITH_FIELDS, Ok(())),
            (&typed, &WITH_FIELDS, Ok(())),
            (&failed, &WITH_FIELDS, Err("subagent_field_invalid")),
            (&failed, &WITH_OUTCOME, Ok(())),
            (&cancelled, &WITH_OUTCOME, Ok(())),
            (&parented, &WITH_OUTCOME, Err("subagent_field_invalid")),
            (&plain, &UNORDERED, Err("subagent_sequence_rule_missing")),
            (&parented, &UNORDERED, Err("subagent_sequence_rule_missing")),
        ] {
            assert_eq!(
                map_worker_subagents(claims, Some(schema)).map(|_mapped| ()),
                expected,
                "{} {claims:?}",
                schema.id
            );
        }
        let shipped = pohunek_worker_protocol::hook_schema("identity-subagent-v1");
        for claims in [&plain, &parented, &typed, &failed, &cancelled, &lost] {
            map_worker_subagents(claims, shipped).expect("the shipped schema admits every field");
        }
    }

    #[test]
    fn a_schema_without_nested_active_pins_snapshot_identities_to_the_launch_runtime() {
        use pohunek_worker_protocol::{HookAction, HookSchema};

        static PINNED: HookSchema = HookSchema {
            id: "pinned-test",
            handlers: &[],
            identity_providers: &["codex", "claude"],
            subagent_providers: &[],
            actions: &[HookAction::IdentityReport, HookAction::IdentityRelease],
            reference_kinds: &[
                pohunek_worker_protocol::ReferenceKind::Id,
                pohunek_worker_protocol::ReferenceKind::Path,
            ],
            ancestry: AncestryMatcher::ManagedPtyDescendant,
            nested_active: false,
            subagent_fields: &[],
            subagent_sequence: None,
        };
        static NESTED: HookSchema = HookSchema {
            nested_active: true,
            ..PINNED
        };

        // The record launched as codex; the snapshot's active identity is claude.
        let snapshot = identity_snapshot("launch-native");
        assert_eq!(
            apply_worker_identities_with(&mut identity_record(), &snapshot, Ok(Some(&PINNED)))
                .expect_err("a nested provider is not the launch runtime"),
            "active_identity_provider_invalid"
        );
        apply_worker_identities_with(&mut identity_record(), &snapshot, Ok(Some(&NESTED)))
            .expect("the same claim is admitted when the schema enables nesting");

        let mut own = identity_snapshot("launch-native");
        own.active_identity
            .as_mut()
            .expect("active identity")
            .provider = "codex".to_owned();
        apply_worker_identities_with(&mut identity_record(), &own, Ok(Some(&PINNED)))
            .expect("the launch runtime itself is always admitted");

        let mut release = identity_snapshot("launch-native");
        let active = release.active_identity.take().expect("active identity");
        release.active_identity_release = Some(ReleasedIdentityClaim {
            provider: active.provider,
            process: active.process,
            sequence: active.sequence + 1,
        });
        assert_eq!(
            apply_worker_identities_with(&mut identity_record(), &release, Ok(Some(&PINNED)))
                .expect_err("a nested provider cannot release"),
            "active_identity_release_provider_invalid"
        );
        apply_worker_identities_with(&mut identity_record(), &release, Ok(Some(&NESTED)))
            .expect("nesting admits the release");
    }

    #[test]
    fn an_active_identity_reference_kind_outside_the_hook_schema_is_rejected() {
        use pohunek_worker_protocol::{HookAction, HookSchema, ReferenceKind};

        // Every shipped schema admits both kinds, so only a schema narrower
        // than the registry's reaches the reference-kind filter.
        static BOTH_KINDS: HookSchema = HookSchema {
            id: "reference-kinds-test",
            handlers: &[],
            identity_providers: &["codex", "claude"],
            subagent_providers: &[],
            actions: &[HookAction::IdentityReport],
            reference_kinds: &[ReferenceKind::Id, ReferenceKind::Path],
            ancestry: AncestryMatcher::ManagedPtyDescendant,
            nested_active: true,
            subagent_fields: &[],
            subagent_sequence: None,
        };
        static ID_ONLY: HookSchema = HookSchema {
            reference_kinds: &[ReferenceKind::Id],
            ..BOTH_KINDS
        };
        static PATH_ONLY: HookSchema = HookSchema {
            reference_kinds: &[ReferenceKind::Path],
            ..BOTH_KINDS
        };

        for (kind, reference, excluding) in [
            ("id", "nested-native", &PATH_ONLY),
            ("path", "/home/operator/.claude/nested.jsonl", &ID_ONLY),
        ] {
            let mut snapshot = identity_snapshot("launch-native");
            // Without a launch claim the active identity is the only input
            // that can change the record.
            snapshot.launch_identity = None;
            let claim = snapshot.active_identity.as_mut().expect("active identity");
            claim.reference_kind = Some(kind.to_owned());
            claim.native_reference = Some(reference.to_owned());

            let mut record = identity_record();
            let original = record.clone();
            assert_eq!(
                apply_worker_identities_with(&mut record, &snapshot, Ok(Some(excluding)))
                    .map(|_projection| ()),
                Err("active_identity_reference_kind_invalid"),
                "{kind:?} is outside {:?}",
                excluding.reference_kinds
            );
            assert_eq!(
                record, original,
                "a rejected {kind:?} claim changes nothing"
            );

            apply_worker_identities_with(&mut record, &snapshot, Ok(Some(&BOTH_KINDS)))
                .unwrap_or_else(|reason| panic!("{kind:?} is admitted by both kinds: {reason}"));
            assert_eq!(record.info.active_agent.as_deref(), Some("claude"));
        }
    }

    /// A minimal legacy-manifest session snapshot, as `pohunek migration
    /// preflight` would have captured it: no runtime yet (the daemon fills
    /// that in on import) and just enough fields for `import_legacy_manifest`
    /// to classify and persist it.
    fn manifest_session_info(id: &str, state: SessionState) -> SessionInfo {
        let created_at = "2026-07-01T00:00:00Z".to_owned();
        SessionInfo {
            id: SessionId(id.to_owned()),
            external: Some(false),
            name: None,
            agent: "shell".to_owned(),
            agent_base: RuntimeRef::shell(),
            cwd: PathBuf::from("/repo"),
            cwd_source: Some(CwdSource::Launch),
            pid: 0,
            runtime: None,
            cols: 80,
            rows: 24,
            state,
            state_source: StateSource::Process,
            activity: None,
            subagents: Vec::new(),
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: None,
            native_session_path: None,
            native_last_activity_at: None,
            project_id: None,
            project_label: None,
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            metadata: BTreeMap::new(),
            created_at: created_at.clone(),
            updated_at: created_at,
            exit_code: None,
            capabilities: protocol::SessionCapabilities::default(),
        }
    }

    /// The sha256 fingerprint `import_legacy_manifest` expects a manifest's
    /// `store_sha256` to match: the hex digest of the store file's current
    /// bytes, or of an empty byte string when the store has not been written
    /// yet. Mirrors the production fingerprinting in `import_legacy_manifest`
    /// so tests can produce a manifest that passes the freshness check.
    fn store_fingerprint(store_path: &std::path::Path) -> String {
        let bytes = std::fs::read(store_path).unwrap_or_default();
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Builds a `durable-session-workers.json` manifest body matching the
    /// shape `pohunek migration preflight` writes (see
    /// `crates/cli/src/commands/migration.rs`).
    fn legacy_manifest_json(
        store_sha256: &str,
        accept_runtime_loss: bool,
        sessions: &[SessionInfo],
        live_session_ids: &[&str],
    ) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "created_at": "2026-07-01T00:00:00Z",
            "store_sha256": store_sha256,
            "accept_runtime_loss": accept_runtime_loss,
            "sessions": sessions,
            "live_session_ids": live_session_ids,
        })
    }

    /// Writes `manifest` to `<data_dir>/migrations/durable-session-workers.json`,
    /// the fixed path `import_legacy_manifest` reads at startup.
    fn write_legacy_manifest(data_dir: &std::path::Path, manifest: &serde_json::Value) -> PathBuf {
        let migrations_dir = data_dir.join("migrations");
        std::fs::create_dir_all(&migrations_dir).expect("create migrations dir");
        let path = migrations_dir.join("durable-session-workers.json");
        let bytes = serde_json::to_vec_pretty(manifest).expect("serialize legacy manifest");
        std::fs::write(&path, bytes).expect("write legacy manifest");
        path
    }

    #[test]
    fn import_legacy_manifest_mixed_recoverable_and_unrecoverable() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path.clone());

        // A legacy resume binding for the recoverable terminal session. This
        // alone does not count as an "existing logical record" (only
        // `SessionRecord`s do, per `import_legacy_manifest`'s idempotency
        // check), so import still proceeds; it is attached to the imported
        // record by matching `session_id`.
        let recoverable_binding = ResumeBinding {
            session_id: "s-recoverable".to_owned(),
            name: Some("recoverable".to_owned()),
            agent: "codex".to_owned(),
            agent_base: RuntimeRef::codex(),
            cwd: PathBuf::from("/repo"),
            cols: 80,
            rows: 24,
            native_session_id: None,
            native_session_path: None,
            project_id: None,
            is_linked_worktree: None,
            metadata: BTreeMap::new(),
            program: "codex".to_owned(),
            args: Vec::new(),
            input_rules: StoredInputRules::default(),
            native_launch: Some(test_native_launch(SessionRefKind::Id, false)),
            launch_binding: crate::agent::host::LaunchPin::Unpinned,
            native_reference_provenance: crate::agent::NativeReferenceProvenance::default(),
            profile_revision: None,
            native_launch_unresolved: false,
        };
        store
            .record_resume(&recoverable_binding)
            .expect("persist legacy resume binding");

        // Fingerprint the store as it stands right now (resume binding
        // written, no session records yet), matching what a real preflight
        // run would have captured.
        let store_sha256 = store_fingerprint(&store_path);
        let sessions = vec![
            manifest_session_info("s-recoverable", SessionState::Done),
            manifest_session_info("s-unrecoverable", SessionState::Done),
            manifest_session_info("s-live", SessionState::Running),
        ];
        let manifest = legacy_manifest_json(&store_sha256, true, &sessions, &["s-live"]);
        let manifest_path = write_legacy_manifest(&data_dir, &manifest);

        import_legacy_manifest(&store).expect("import legacy manifest");

        let records = store.load_sessions().expect("load imported sessions");
        assert_eq!(records.len(), 3);
        let live = records
            .iter()
            .find(|record| record.session_id == "s-live")
            .expect("live session imported");
        let recoverable = records
            .iter()
            .find(|record| record.session_id == "s-recoverable")
            .expect("recoverable session imported");
        let unrecoverable = records
            .iter()
            .find(|record| record.session_id == "s-unrecoverable")
            .expect("unrecoverable session imported");

        assert_eq!(live.desired_state, DesiredState::Running);
        assert_eq!(live.runtime.state, RuntimeState::Lost);
        assert_eq!(
            live.runtime.reason.as_deref(),
            Some("legacy_runtime_not_transferable")
        );
        assert_eq!(
            live.info
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.loss_reason.as_deref()),
            Some("legacy_runtime_not_transferable")
        );
        assert_eq!(live.recovery, None, "no resume binding matched s-live");

        assert_eq!(recoverable.desired_state, DesiredState::Stopped);
        assert_eq!(recoverable.runtime.state, RuntimeState::Terminal);
        assert_eq!(recoverable.runtime.reason, None);
        assert_eq!(
            recoverable
                .recovery
                .as_ref()
                .map(|binding| binding.session_id.clone()),
            Some("s-recoverable".to_owned()),
            "matching legacy resume binding must be attached as recovery"
        );

        assert_eq!(unrecoverable.desired_state, DesiredState::Stopped);
        assert_eq!(unrecoverable.runtime.state, RuntimeState::Terminal);
        assert_eq!(
            unrecoverable.recovery, None,
            "unrecoverable session has no resume binding to attach"
        );

        assert!(
            !manifest_path.exists(),
            "imported manifest must be archived, not left pending"
        );
    }

    #[test]
    fn import_archives_manifest_after_uncertain_post_rename_commit() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path.clone());
        let fingerprint = store_fingerprint(&store_path);
        let sessions = vec![manifest_session_info("s-uncertain", SessionState::Done)];
        let manifest = legacy_manifest_json(&fingerprint, true, &sessions, &[]);
        let manifest_path = write_legacy_manifest(&data_dir, &manifest);
        store.fail_next_parent_sync_after_rename();

        import_legacy_manifest(&store).expect("import committed uncertain session");

        assert_eq!(
            store
                .load_sessions()
                .expect("load uncertain import")
                .first()
                .map(|record| record.session_id.as_str()),
            Some("s-uncertain")
        );
        assert!(
            !manifest_path.exists(),
            "visible committed import may archive its manifest after warning"
        );
    }

    #[test]
    fn import_legacy_manifest_fingerprint_mismatch_fails_closed() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path);

        let sessions = vec![manifest_session_info("s-1", SessionState::Done)];
        // Well-formed but wrong: 64 hex chars that cannot equal the real
        // fingerprint of an empty (not-yet-written) store.
        let bogus_fingerprint = "0".repeat(64);
        let manifest = legacy_manifest_json(&bogus_fingerprint, false, &sessions, &[]);
        write_legacy_manifest(&data_dir, &manifest);

        let error =
            import_legacy_manifest(&store).expect_err("mismatched fingerprint must fail closed");
        assert_eq!(error.code, "migration_store_changed");
        assert!(
            store.load_sessions().expect("load sessions").is_empty(),
            "store must not be mutated when the fingerprint check fails"
        );
    }

    #[test]
    fn import_legacy_manifest_rejects_a_base_outside_the_runtime_id_grammar() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path.clone());

        let mut session = manifest_session_info("s-historical", SessionState::Done);
        session.agent_base = RuntimeRef::from_wire("Future Agent");
        let store_sha256 = store_fingerprint(&store_path);
        let manifest = legacy_manifest_json(&store_sha256, false, &[session], &[]);
        write_legacy_manifest(&data_dir, &manifest);

        let error =
            import_legacy_manifest(&store).expect_err("a historical agent base must fail closed");
        assert_eq!(error.code, "migration_import_failed");
        assert!(
            store.load_sessions().expect("load sessions").is_empty(),
            "store must not be mutated when the manifest names an unlaunchable base"
        );
    }

    #[test]
    fn import_legacy_manifest_keeps_a_grammar_valid_uninstalled_base() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path.clone());

        let mut session = manifest_session_info("s-acme", SessionState::Done);
        session.agent_base = RuntimeRef::from_wire("acme");
        let store_sha256 = store_fingerprint(&store_path);
        let manifest = legacy_manifest_json(&store_sha256, false, &[session], &[]);
        write_legacy_manifest(&data_dir, &manifest);

        import_legacy_manifest(&store).expect("a valid but uninstalled runtime id imports");
        let loaded = store.load_sessions().expect("load sessions");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].info.agent_base, RuntimeRef::from_wire("acme"));
    }

    #[test]
    fn import_legacy_manifest_live_loss_not_accepted_fails_closed() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path.clone());

        let store_sha256 = store_fingerprint(&store_path);
        let sessions = vec![manifest_session_info("s-live", SessionState::Running)];
        let manifest = legacy_manifest_json(&store_sha256, false, &sessions, &["s-live"]);
        write_legacy_manifest(&data_dir, &manifest);

        let error = import_legacy_manifest(&store)
            .expect_err("unaccepted live-runtime loss must fail closed");
        assert_eq!(error.code, "migration_runtime_loss_not_accepted");
        assert!(
            store.load_sessions().expect("load sessions").is_empty(),
            "store must not be mutated when runtime loss is not accepted"
        );
    }

    #[test]
    fn import_legacy_manifest_live_classification_mismatch_fails_closed() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path.clone());

        let store_sha256 = store_fingerprint(&store_path);
        // Models an interrupted or tampered manifest: the snapshot itself
        // carries a Running session, but `live_session_ids` was not updated
        // to match, so the two views of "what is live" disagree.
        let sessions = vec![manifest_session_info("s-running", SessionState::Running)];
        let manifest = legacy_manifest_json(&store_sha256, true, &sessions, &[]);
        write_legacy_manifest(&data_dir, &manifest);

        let error = import_legacy_manifest(&store)
            .expect_err("live-classification mismatch must fail closed");
        assert_eq!(error.code, "migration_manifest_mismatch");
        assert!(
            store.load_sessions().expect("load sessions").is_empty(),
            "store must not be mutated when live classification disagrees"
        );
    }

    #[test]
    fn import_legacy_manifest_unsupported_schema_fails_closed() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path.clone());

        let manifest = serde_json::json!({
            "schema_version": 2,
            "created_at": "2026-07-01T00:00:00Z",
            "store_sha256": store_fingerprint(&store_path),
            "accept_runtime_loss": false,
            "sessions": Vec::<SessionInfo>::new(),
            "live_session_ids": Vec::<String>::new(),
        });
        write_legacy_manifest(&data_dir, &manifest);

        let error = import_legacy_manifest(&store)
            .expect_err("unsupported schema version must fail closed");
        assert_eq!(error.code, "migration_import_failed");
        assert!(
            store.load_sessions().expect("load sessions").is_empty(),
            "store must not be mutated when the manifest schema is unsupported"
        );
    }

    #[test]
    fn import_legacy_manifest_already_migrated_is_idempotent() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path.clone());

        let mut existing = identity_record();
        existing.session_id = "s-existing".to_owned();
        existing.info.id = SessionId("s-existing".to_owned());
        store
            .record_session(&existing)
            .expect("persist pre-existing logical record");

        // The fingerprint is deliberately wrong: the idempotency short-circuit
        // must return before the fingerprint (or any other) check runs, so a
        // stale or tampered manifest cannot even be evaluated once the store
        // already holds logical records.
        let bogus_fingerprint = "f".repeat(64);
        let sessions = vec![manifest_session_info("s-new", SessionState::Done)];
        let manifest = legacy_manifest_json(&bogus_fingerprint, true, &sessions, &[]);
        let manifest_path = write_legacy_manifest(&data_dir, &manifest);

        import_legacy_manifest(&store)
            .expect("import over an already-migrated store must short-circuit cleanly");

        let records = store.load_sessions().expect("load sessions");
        assert_eq!(
            records.len(),
            1,
            "import must not duplicate or add records once the store is non-empty"
        );
        assert_eq!(records[0].session_id, "s-existing");
        assert!(
            !manifest_path.exists(),
            "manifest must be archived, not left pending, even on the idempotent path"
        );
        let migrations_dir = data_dir.join("migrations");
        let archived = std::fs::read_dir(&migrations_dir)
            .expect("read migrations dir")
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("durable-session-workers.imported-")
            });
        assert!(
            archived,
            "manifest must be archived under its imported-* name"
        );
    }

    /// A resume binding as a legacy daemon persisted it for `session_id`.
    fn legacy_binding(session_id: &str) -> ResumeBinding {
        ResumeBinding {
            session_id: session_id.to_owned(),
            name: None,
            agent: "codex".to_owned(),
            agent_base: RuntimeRef::codex(),
            cwd: PathBuf::from("/repo"),
            cols: 80,
            rows: 24,
            native_session_id: Some("native-legacy".to_owned()),
            native_session_path: None,
            project_id: None,
            is_linked_worktree: None,
            metadata: BTreeMap::new(),
            program: "codex".to_owned(),
            args: Vec::new(),
            input_rules: StoredInputRules::default(),
            native_launch: Some(test_native_launch(SessionRefKind::Id, false)),
            launch_binding: crate::agent::host::LaunchPin::Unpinned,
            native_reference_provenance: crate::agent::NativeReferenceProvenance::default(),
            profile_revision: None,
            native_launch_unresolved: false,
        }
    }

    /// Inventory entries naming a legacy binding no manifest imported.
    async fn unmigrated_entries(registry: &SessionRegistry) -> Vec<String> {
        registry
            .runtime_inventory()
            .await
            .entries
            .into_iter()
            .filter(|entry| {
                entry.status == RuntimeInventoryStatus::Orphaned
                    && entry.reason.as_deref() == Some(super::MIGRATION_MANIFEST_MISSING)
            })
            .map(|entry| entry.runtime_slot)
            .collect()
    }

    #[tokio::test]
    async fn legacy_bindings_without_a_manifest_are_surfaced_and_kept() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store = Store::new(data_dir.join("metadata.jsonl"));
        for session_id in ["s-legacy-b", "s-legacy-a"] {
            store
                .record_resume(&legacy_binding(session_id))
                .expect("persist legacy resume binding");
        }
        let registry = empty_registry(&root, &root.join("runtime/workers"));
        let mut events = registry.subscribe();

        Box::pin(registry.reconcile_workers())
            .await
            .expect("the daemon starts without a manifest");

        assert_eq!(
            unmigrated_entries(&registry).await,
            vec!["s-legacy-a".to_owned(), "s-legacy-b".to_owned()]
        );
        let mut announced = Vec::new();
        while let Ok(event) = events.try_recv() {
            if event.event() == protocol::event::SESSION_RUNTIME_DISCOVERED {
                announced.push(event);
            }
        }
        assert_eq!(announced.len(), 2, "every unmigrated binding is announced");
        registry
            .inspect(&SessionId("s-legacy-a".to_owned()))
            .await
            .expect_err("a binding alone never becomes a listed session");
        assert!(store.load_sessions().expect("load sessions").is_empty());
        assert_eq!(
            store.load_resume().expect("load resume bindings").len(),
            2,
            "the bindings stay on disk for a later manifest import"
        );
    }

    #[tokio::test]
    async fn legacy_bindings_are_not_flagged_once_logical_records_exist() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store = Store::new(data_dir.join("metadata.jsonl"));
        store
            .record_session(&identity_record())
            .expect("persist logical record");
        store
            .record_resume(&legacy_binding("s-legacy"))
            .expect("persist resume binding");
        let registry = empty_registry(&root, &root.join("runtime/workers"));

        Box::pin(registry.reconcile_workers())
            .await
            .expect("reconcile");

        assert!(unmigrated_entries(&registry).await.is_empty());
    }

    #[tokio::test]
    async fn a_durable_session_of_an_uninstalled_runtime_stays_listed_and_keeps_its_binding() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path.clone());
        let mut record = identity_record();
        let kind = RuntimeRef::from_wire("acme");
        record.info.agent = "acme".to_owned();
        record.info.agent_base = kind.clone();
        let recovery = record.recovery.as_mut().expect("recovery binding");
        recovery.agent = "acme".to_owned();
        recovery.agent_base = kind.clone();
        recovery.native_session_id = Some("native-acme".to_owned());
        store.record_session(&record).expect("persist inert record");
        let registry = empty_registry(&root, &root.join("runtime/workers"));

        Box::pin(registry.reconcile_workers())
            .await
            .expect("reconcile");

        let id = SessionId(record.session_id.clone());
        let listed = registry
            .inspect(&id)
            .await
            .expect("the session stays listed");
        assert_eq!(listed.agent_base, kind);
        let before = std::fs::read(&store_path).expect("read store");
        let refused = registry
            .resume(&id)
            .await
            .expect_err("an uninstalled runtime cannot be resumed");
        assert_eq!(refused.code, "runtime_not_installed");
        assert_eq!(
            std::fs::read(&store_path).expect("read store"),
            before,
            "a refused resume leaves the stored binding untouched"
        );
        let kept = store.load_sessions().expect("load sessions");
        assert_eq!(kept.len(), 1, "the inert session record is not dropped");
        assert_eq!(
            kept[0]
                .recovery
                .as_ref()
                .and_then(|binding| binding.native_session_id.as_deref()),
            Some("native-acme")
        );
    }

    /// Persists the inert record of runtime `acme`, which no definition backs,
    /// and returns the registry after startup reconciliation.
    async fn inert_registry(root: &Path) -> (SessionRegistry, Store, SessionId) {
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store = Store::new(data_dir.join("metadata.jsonl"));
        let mut record = identity_record();
        let kind = RuntimeRef::from_wire("acme");
        record.info.agent = "acme".to_owned();
        record.info.agent_base = kind.clone();
        let recovery = record.recovery.as_mut().expect("recovery binding");
        recovery.agent = "acme".to_owned();
        recovery.agent_base = kind;
        store.record_session(&record).expect("persist inert record");
        let registry = empty_registry(root, &root.join("runtime/workers"));
        Box::pin(registry.reconcile_workers())
            .await
            .expect("reconcile");
        (registry, store, SessionId(record.session_id))
    }

    #[tokio::test]
    async fn an_inert_session_is_removed_without_its_runtime_and_releases_its_record() {
        let root = temp_root();
        let (registry, store, id) = inert_registry(&root).await;
        registry.inspect(&id).await.expect("the session is listed");

        let removed = registry
            .remove(&id)
            .await
            .expect("an uninstalled runtime does not block removal");

        assert!(removed.removed);
        registry
            .inspect(&id)
            .await
            .expect_err("the removed session is no longer listed");
        assert!(store.load_sessions().expect("load sessions").is_empty());
        assert!(store.load_resume().expect("load resume").is_empty());
    }

    #[tokio::test]
    async fn an_inert_session_stays_unlaunchable_and_unwritable() {
        let root = temp_root();
        let (registry, store, id) = inert_registry(&root).await;
        let before = std::fs::read(root.join("data/metadata.jsonl")).expect("read store");

        let resume = registry.resume(&id).await.expect_err("resume is refused");
        assert_eq!(resume.code, "runtime_not_installed");
        let fork = registry
            .fork(protocol::SessionForkParams {
                session_id: id.clone(),
                name: None,
                cwd_mode: protocol::ForkCwdMode::default(),
                cols: 80,
                rows: 24,
                accept_profile_change: false,
            })
            .await
            .expect_err("fork is refused");
        assert_eq!(fork.code, "runtime_not_installed");
        let input = registry
            .input(protocol::SessionInputParams {
                session_id: id.clone(),
                text: "x".to_owned(),
                wait: None,
            })
            .await
            .expect_err("input is refused");
        assert_eq!(input.code, "runtime_not_installed");

        assert_eq!(
            std::fs::read(root.join("data/metadata.jsonl")).expect("read store"),
            before,
            "refused operations write nothing"
        );
        assert_eq!(store.load_sessions().expect("load sessions").len(), 1);
    }

    #[tokio::test]
    async fn a_session_with_a_live_worker_and_an_uninstalled_runtime_is_stopped_then_removed() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let registry = launching_registry(&root, &root.join("runtime/workers"));
        let created = registry
            .create(shell_params(&root))
            .await
            .expect("create a live session");
        registry
            .set_agent_base_for_test(&created.id, RuntimeRef::from_wire("acme"))
            .await;
        registry
            .input(protocol::SessionInputParams {
                session_id: created.id.clone(),
                text: "x".to_owned(),
                wait: None,
            })
            .await
            .expect_err("input to an uninstalled runtime is refused");

        let stopped = registry
            .stop(&created.id)
            .await
            .expect("the worker is stopped without its runtime definition");
        assert!(stopped.stopped);
        let removed = registry
            .remove(&created.id)
            .await
            .expect("the stopped session is removed");
        assert!(removed.removed);
        registry
            .inspect(&created.id)
            .await
            .expect_err("the removed session is gone");
    }

    /// A plain-shell `session.new` launched in `cwd`.
    fn shell_params(cwd: &Path) -> SessionNewParams {
        SessionNewParams {
            name: None,
            agent: "shell".to_owned(),
            cwd: Some(cwd.to_path_buf()),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            extended_input_ready_wait: None,
            metadata: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn unmigrated_legacy_bindings_refuse_session_creation_until_imported() {
        let root = temp_root();
        let data_dir = root.join("data");
        create_private_dir(&data_dir);
        let store_path = data_dir.join("metadata.jsonl");
        let store = Store::new(store_path.clone());
        store
            .record_resume(&legacy_binding("s-legacy"))
            .expect("persist legacy resume binding");
        let registry = launching_registry(&root, &root.join("runtime/workers"));
        Box::pin(registry.reconcile_workers())
            .await
            .expect("the daemon starts without a manifest");
        let store_before = std::fs::read(&store_path).expect("read store");

        let refused = registry
            .create(shell_params(&root))
            .await
            .expect_err("creating the first logical record is refused");
        assert_eq!(refused.code, super::MIGRATION_MANIFEST_MISSING);
        assert!(
            refused
                .recover
                .as_deref()
                .is_some_and(|recover| recover.contains("pohunek migration preflight")),
            "the refusal names the preflight: {refused:?}"
        );
        let refused_fork = registry
            .fork(SessionForkParams {
                session_id: SessionId("s-legacy".to_owned()),
                name: None,
                cwd_mode: ForkCwdMode::default(),
                cols: 80,
                rows: 24,
                accept_profile_change: false,
            })
            .await
            .expect_err("a fork is refused as well");
        assert_eq!(refused_fork.code, super::MIGRATION_MANIFEST_MISSING);
        assert_eq!(
            std::fs::read(&store_path).expect("read store"),
            store_before,
            "a refused create writes nothing"
        );
        assert!(registry.list().await.is_empty(), "nothing is listed");
        registry.begin_daemon_shutdown();
        drop(registry);

        // The preflight snapshots the legacy store, and the next start
        // imports its manifest.
        let manifest = legacy_manifest_json(
            &store_fingerprint(&store_path),
            true,
            &[manifest_session_info("s-legacy", SessionState::Done)],
            &[],
        );
        write_legacy_manifest(&data_dir, &manifest);
        let restarted = launching_registry(&root, &root.join("runtime/workers"));
        Box::pin(restarted.reconcile_workers())
            .await
            .expect("the daemon imports the manifest");
        assert!(unmigrated_entries(&restarted).await.is_empty());
        assert!(
            store
                .load_sessions()
                .expect("load sessions")
                .iter()
                .any(|record| record.session_id == "s-legacy"),
            "the legacy session is imported"
        );

        let created = restarted
            .create(shell_params(&root))
            .await
            .expect("an imported manifest lifts the gate");
        restarted
            .stop(&created.id)
            .await
            .expect("stop the created session");
    }

    /// The WARN lines `logs` holds for `session`.
    fn warn_lines(
        logs: &crate::runtime::lifecycle::tests::LogCapture,
        session: &str,
    ) -> Vec<String> {
        logs.text()
            .lines()
            .filter(|line| {
                line.contains(" WARN ") && line.contains(&format!("session_id={session} "))
            })
            .map(ToOwned::to_owned)
            .collect()
    }

    /// Supervisor-aware reconciliation fixtures (RFC §15 supervision rows).
    mod supervision {
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::sync::Arc;
        use std::time::Duration;

        use pohunek_platform::supervisor::{
            DefinitionFacts, Error as SupervisorError, JobDefinition, Operation, ServiceId,
            ServiceObservation, ServiceState, Supervisor,
        };
        use pohunek_session_worker::{Journal, JournalRecord, RuntimePhase as JournalRuntimePhase};
        use protocol::{RuntimeInventoryStatus, RuntimeState, SessionId};

        use super::super::super::supervision::{
            RUNTIME_LOST, RUNTIME_LOST_CLEANUP_UNCONFIRMED, STALE_GENERATION, UNSUPERVISED_WORKER,
        };
        use super::{
            bind_test_generation, identity_record, spawn_incompatible_worker,
            spawn_initialized_worker, temp_root, warn_lines, RetryInspector, TEST_GENERATION,
            TEST_WORKER_EXECUTABLE,
        };
        use crate::procwatch::{HostInspector, ProcessInspector};
        use crate::runtime::lifecycle::tests::{Call, InspectStep, JobScript, ScriptedSupervisor};
        use crate::runtime::lifecycle::{
            IDENTITY_MISMATCH, SUPERVISION_AMBIGUOUS, SUPERVISION_UNAVAILABLE,
        };
        use crate::session::{SessionRegistry, SessionRegistryConfig};
        use crate::store::{DesiredState, SessionRecord, Store};
        use time::format_description::well_known::Rfc3339;
        use time::OffsetDateTime;

        /// Sweep grace of the fixtures; short so a `SIGKILL` fallback stays fast.
        const SWEEP_GRACE: Duration = Duration::from_millis(300);
        use super::hermetic_shell;
        use pohunek_test_support::wait::HANG_GUARD;

        struct Fixture {
            root: crate::test_support::ScopedDir,
            registry: SessionRegistry,
            supervisor: Arc<ScriptedSupervisor>,
        }

        fn fixture(inspector: Arc<dyn ProcessInspector>) -> Fixture {
            fixture_with(inspector, None)
        }

        /// A fixture whose workers get `worker_initialize` to become ready.
        fn fixture_with(
            inspector: Arc<dyn ProcessInspector>,
            worker_initialize: Option<Duration>,
        ) -> Fixture {
            fixture_over(inspector, worker_initialize, ScriptedSupervisor::scripted())
        }

        /// A [`fixture_with`] whose jobs are managed by `supervisor`.
        fn fixture_over(
            inspector: Arc<dyn ProcessInspector>,
            worker_initialize: Option<Duration>,
            supervisor: ScriptedSupervisor,
        ) -> Fixture {
            let root = temp_root();
            let runtime_root = root.join("runtime/workers");
            let state_root = root.join("state/workers");
            for path in [
                root.join("runtime"),
                runtime_root.clone(),
                root.join("state"),
                state_root.clone(),
                root.join("logs"),
            ] {
                std::fs::create_dir_all(&path).expect("create worker root");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                    .expect("private worker root");
            }
            let mut supervision = crate::session::test_supervision(&runtime_root, &state_root);
            supervision.sweep_grace = SWEEP_GRACE;
            if let Some(worker_initialize) = worker_initialize {
                supervision.worker_initialize = worker_initialize;
            }
            let supervisor = Arc::new(supervisor);
            let registry = SessionRegistry::new_with_launcher_and_inspector(
                SessionRegistryConfig {
                    shell_command: hermetic_shell(),
                    store_path: Some(root.join("data/metadata.jsonl")),
                    worker_runtime_root: Some(runtime_root),
                    worker_state_root: Some(state_root),
                    worker_connect_deadline: Duration::from_millis(300),
                    supervision: Some(supervision),
                    log_dir: Some(root.join("logs")),
                    ..SessionRegistryConfig::default()
                },
                Arc::clone(&supervisor) as Arc<dyn crate::runtime::WorkerLauncher>,
                inspector,
            );
            Fixture {
                root,
                registry,
                supervisor,
            }
        }

        fn service_id(session_id: &str, generation: &str) -> ServiceId {
            ServiceId::parse(format!("{session_id}.{generation}")).expect("service id")
        }

        /// Persists a running record naming [`TEST_GENERATION`] of `session_id`.
        fn persist_record(
            root: &Path,
            session_id: &str,
            worker_instance_id: Option<&str>,
        ) -> SessionRecord {
            let mut record = identity_record();
            record.session_id = session_id.to_owned();
            record.info.id = SessionId(session_id.to_owned());
            let worker_id = format!("worker-{session_id}");
            record.runtime.worker_id = Some(worker_id.clone());
            record.runtime.worker_instance_id = worker_instance_id.map(ToOwned::to_owned);
            let runtime = record.info.runtime.as_mut().expect("runtime");
            runtime.worker_id = Some(worker_id);
            runtime.worker_instance_id = worker_instance_id.map(ToOwned::to_owned);
            record.recovery.as_mut().expect("recovery").session_id = session_id.to_owned();
            bind_test_generation(&mut record);
            Store::new(root.join("data/metadata.jsonl"))
                .record_session(&record)
                .expect("persist supervised record");
            record
        }

        /// Writes the live journal of `session_id`'s [`TEST_GENERATION`] worker.
        fn write_live_journal(
            root: &Path,
            session_id: &str,
            worker: (u32, u64),
            worker_instance_id: Option<&str>,
        ) {
            write_generation_journal(
                root,
                session_id,
                TEST_GENERATION,
                worker,
                worker_instance_id,
            );
        }

        /// Writes the live journal of `session_id`'s worker naming `generation`.
        fn write_generation_journal(
            root: &Path,
            session_id: &str,
            generation: &str,
            worker: (u32, u64),
            worker_instance_id: Option<&str>,
        ) {
            let worker_id = format!("worker-{session_id}");
            write_worker_journal(
                root,
                session_id,
                &worker_id,
                generation,
                worker,
                worker_instance_id,
            );
        }

        /// Writes the live journal of `session_id`'s worker `worker_id`
        /// naming `generation`.
        fn write_worker_journal(
            root: &Path,
            session_id: &str,
            worker_id: &str,
            generation: &str,
            worker: (u32, u64),
            worker_instance_id: Option<&str>,
        ) {
            write_journal_record(
                root,
                session_id,
                worker_id,
                generation,
                worker,
                worker_instance_id,
                |_| {},
            );
        }

        /// Writes the terminal journal of `session_id`'s [`TEST_GENERATION`]
        /// worker after its child exited successfully.
        fn write_terminal_journal(root: &Path, session_id: &str, worker_instance_id: &str) {
            write_journal_record(
                root,
                session_id,
                &format!("worker-{session_id}"),
                TEST_GENERATION,
                dead_worker(),
                Some(worker_instance_id),
                |journal| {
                    journal.phase = JournalRuntimePhase::Terminal;
                    journal.outcome = Some(pohunek_session_worker::RuntimeOutcome {
                        exit_code: Some(0),
                        signal: None,
                        success: true,
                        exited_at: "2026-09-24T00:01:00Z".to_owned(),
                        reason: "natural_exit".to_owned(),
                    });
                },
            );
        }

        /// Writes a live journal of `session_id`'s worker `worker_id` naming
        /// `generation`, adjusted by `finish` before it is persisted.
        fn write_journal_record(
            root: &Path,
            session_id: &str,
            worker_id: &str,
            generation: &str,
            worker: (u32, u64),
            worker_instance_id: Option<&str>,
            finish: impl FnOnce(&mut JournalRecord),
        ) {
            let mut journal = JournalRecord::bootstrap(
                session_id.to_owned(),
                worker_id.to_owned(),
                pohunek_session_worker::WorkerOrigin {
                    executable: PathBuf::from(TEST_WORKER_EXECUTABLE),
                    version: "1.0.0".to_owned(),
                    generation: generation.to_owned(),
                },
                worker.0,
                worker.1.to_string(),
                "boot-test".to_owned(),
                (1, 2),
                "2026-09-24T00:00:00Z".to_owned(),
            );
            journal.worker_instance_id = worker_instance_id.map(ToOwned::to_owned);
            journal.phase = JournalRuntimePhase::Live;
            finish(&mut journal);
            let session_dir = root.join("state/workers").join(session_id);
            std::fs::create_dir_all(&session_dir).expect("create journal directory");
            std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o700))
                .expect("private journal directory");
            Journal::new(session_dir.join(format!("{worker_id}.json")))
                .write(&journal)
                .expect("write worker journal");
        }

        /// Identity of a worker process that has exited and been reaped.
        fn dead_worker() -> (u32, u64) {
            let mut child = pohunek_test_support::process_env::command("true")
                .spawn()
                .expect("spawn short-lived worker stand-in");
            let pid = child.id();
            child.wait().expect("reap worker stand-in");
            (pid, 1)
        }

        /// A dead worker journaled with a realistic start identity.
        ///
        /// The sweep classifies unreadable-marker processes against the
        /// journaled worker start identity. The test process started after
        /// every long-lived non-dumpable process on the host (a desktop user
        /// manager, for example), so its own start time is the stand-in that
        /// proves such bystanders foreign, exactly as a real worker's journal
        /// would.
        fn dead_worker_with_realistic_start() -> (u32, u64) {
            (dead_worker().0, own_identity().1)
        }

        fn own_identity() -> (u32, u64) {
            let identity = HostInspector::new()
                .identity(std::process::id())
                .expect("inspect test process")
                .expect("test process exists");
            (identity.pid, identity.start_identity.get())
        }

        /// A hangup-ignoring process marked with `worker_instance_id`, reparented away
        /// from the test process so its exit is observable without reaping.
        struct Marked(u32);

        impl Marked {
            fn spawn(worker_instance_id: &str) -> Self {
                Self::spawn_with(&[("POHUNEK_WORKER_INSTANCE_ID", worker_instance_id)])
            }

            /// Like [`Self::spawn`] with exactly the given environment markers.
            fn spawn_with(markers: &[(&str, &str)]) -> Self {
                let output = std::process::Command::new("/bin/sh")
                    .args(["-c", "trap '' HUP; sleep 60 >/dev/null 2>&1 & echo $!"])
                    .envs(markers.iter().copied())
                    .output()
                    .expect("spawn marked process");
                let pid = String::from_utf8(output.stdout)
                    .expect("utf-8 pid")
                    .trim()
                    .parse()
                    .expect("numeric pid");
                Self(pid)
            }

            fn alive(&self) -> bool {
                HostInspector::new()
                    .identity(self.0)
                    .is_ok_and(|identity| identity.is_some())
            }

            async fn wait_gone(&self) {
                let deadline = tokio::time::Instant::now() + HANG_GUARD;
                while self.alive() {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "marked process {} survived the sweep",
                        self.0
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }

        impl Drop for Marked {
            fn drop(&mut self) {
                let _ = pohunek_test_support::process_env::command("kill")
                    .args(["-KILL", &self.0.to_string()])
                    .status();
            }
        }

        async fn runtime_of(
            registry: &SessionRegistry,
            session_id: &str,
        ) -> protocol::SessionRuntime {
            registry
                .inspect(&SessionId(session_id.to_owned()))
                .await
                .expect("session stays visible")
                .runtime
                .expect("runtime")
        }

        fn present(state: ServiceState, definition: Option<DefinitionFacts>) -> JobScript {
            JobScript::Present {
                state,
                process: None,
                definition,
            }
        }

        #[tokio::test]
        async fn present_job_without_a_reachable_worker_is_ambiguous_and_untouched() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let marked = Marked::spawn("runtime-s-301");
            persist_record(&fixture.root, "s-301", Some("runtime-s-301"));
            write_live_journal(&fixture.root, "s-301", dead_worker(), Some("runtime-s-301"));
            fixture.supervisor.script_job(
                service_id("s-301", TEST_GENERATION),
                present(ServiceState::Running, None),
            );

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-301").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired"
            );
            assert!(marked.alive(), "nothing is swept");
        }

        #[tokio::test]
        async fn ended_job_with_a_dead_worker_sweeps_exactly_its_runtime_and_is_lost() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let lost = Marked::spawn("runtime-s-302");
            let bystander = Marked::spawn("runtime-s-302-other");
            persist_record(&fixture.root, "s-302", Some("runtime-s-302"));
            write_live_journal(
                &fixture.root,
                "s-302",
                dead_worker_with_realistic_start(),
                Some("runtime-s-302"),
            );
            let id = service_id("s-302", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-302").await;
            assert_eq!(runtime.state, RuntimeState::Lost);
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            lost.wait_gone().await;
            assert!(bystander.alive(), "another runtime is never swept");
            assert_eq!(fixture.supervisor.retired(), vec![id]);
        }

        /// Rewrites the persisted journal of `session_id`'s worker as raw JSON.
        fn rewrite_journal(
            root: &Path,
            session_id: &str,
            worker_id: &str,
            edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
        ) {
            let path = root
                .join("state/workers")
                .join(session_id)
                .join(format!("{worker_id}.json"));
            let mut json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).expect("read journal"))
                    .expect("journal json");
            edit(json.as_object_mut().expect("journal object"));
            std::fs::write(&path, json.to_string()).expect("rewrite journal");
        }

        /// A dead worker whose journal `edit` rewrites; asserts the journal
        /// is no evidence: nothing is swept, retired or declared lost.
        async fn assert_unusable_journal_proves_nothing(
            session_id: &str,
            edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
        ) {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_id = format!("runtime-{session_id}");
            let marked = Marked::spawn(&runtime_id);
            persist_record(&fixture.root, session_id, Some(&runtime_id));
            write_live_journal(
                &fixture.root,
                session_id,
                dead_worker_with_realistic_start(),
                Some(&runtime_id),
            );
            rewrite_journal(
                &fixture.root,
                session_id,
                &format!("worker-{session_id}"),
                edit,
            );
            fixture.supervisor.script_job(
                service_id(session_id, TEST_GENERATION),
                present(ServiceState::Failed, None),
            );

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, session_id).await;
            assert_ne!(runtime.state, RuntimeState::Lost);
            assert!(marked.alive(), "nothing is swept without evidence");
            assert!(fixture.supervisor.retired().is_empty());
        }

        /// Schema 3 (never released) carries no generation, executable or
        /// version, so its journal cannot be generation evidence.
        #[tokio::test]
        async fn schema_three_journal_of_a_dead_worker_proves_nothing() {
            assert_unusable_journal_proves_nothing("s-303", |journal| {
                for key in ["executable", "version", "generation"] {
                    journal.remove(key);
                }
                journal.insert("schema_version".to_owned(), 3.into());
            })
            .await;
        }

        /// A journal missing required evidence is a conflict, never proof
        /// that its worker terminated.
        #[tokio::test]
        async fn current_schema_journal_missing_its_generation_proves_nothing() {
            assert_unusable_journal_proves_nothing("s-304", |journal| {
                journal.remove("generation");
            })
            .await;
        }

        /// A schema older than the previous one is not evidence either.
        #[tokio::test]
        async fn schema_older_than_the_previous_proves_nothing() {
            assert_unusable_journal_proves_nothing("s-305", |journal| {
                journal.insert(
                    "schema_version".to_owned(),
                    (crate::runtime::lifecycle::WORKER_JOURNAL_SCHEMA_VERSION - 2).into(),
                );
            })
            .await;
        }

        /// A daemon upgraded while a worker kept running sees descendants that
        /// carry only `POHUNEK_RUNTIME_ID`; after a crash the sweep reaps them
        /// and the runtime is lost with a confirmed cleanup.
        #[tokio::test]
        async fn crash_sweeps_a_descendant_that_carries_only_the_runtime_id_marker() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let lost = Marked::spawn_with(&[("POHUNEK_RUNTIME_ID", "runtime-s-310")]);
            persist_record(&fixture.root, "s-310", Some("runtime-s-310"));
            write_live_journal(
                &fixture.root,
                "s-310",
                dead_worker_with_realistic_start(),
                Some("runtime-s-310"),
            );
            let id = service_id("s-310", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-310").await;
            assert_eq!(runtime.state, RuntimeState::Lost);
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            lost.wait_gone().await;
        }

        /// A process claiming two worker instances is neither signalled nor
        /// allowed to make the cleanup look complete.
        #[tokio::test]
        async fn crash_next_to_a_process_with_conflicting_markers_keeps_the_cleanup_unconfirmed() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicting = Marked::spawn_with(&[
                ("POHUNEK_WORKER_INSTANCE_ID", "runtime-s-311"),
                ("POHUNEK_RUNTIME_ID", "runtime-s-311-other"),
            ]);
            persist_record(&fixture.root, "s-311", Some("runtime-s-311"));
            write_live_journal(
                &fixture.root,
                "s-311",
                dead_worker_with_realistic_start(),
                Some("runtime-s-311"),
            );
            fixture.supervisor.script_job(
                service_id("s-311", TEST_GENERATION),
                present(ServiceState::Failed, None),
            );

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-311").await;
            assert_eq!(runtime.state, RuntimeState::Lost);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(RUNTIME_LOST_CLEANUP_UNCONFIRMED)
            );
            assert!(
                conflicting.alive(),
                "an ambiguous process is never signalled"
            );
        }

        #[tokio::test]
        async fn failed_sweep_is_still_lost_with_unconfirmed_cleanup() {
            let inspector = Arc::new(RetryInspector::default());
            inspector.fail_enumeration(true);
            let fixture =
                fixture(Arc::<RetryInspector>::clone(&inspector) as Arc<dyn ProcessInspector>);
            let marked = Marked::spawn("runtime-s-303");
            persist_record(&fixture.root, "s-303", Some("runtime-s-303"));
            write_live_journal(&fixture.root, "s-303", dead_worker(), Some("runtime-s-303"));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-303").await;
            assert_eq!(runtime.state, RuntimeState::Lost);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(RUNTIME_LOST_CLEANUP_UNCONFIRMED)
            );
            assert!(marked.alive(), "an aborted sweep signals nothing");
        }

        #[tokio::test]
        async fn unreadable_markers_leave_the_runtime_lost_with_unconfirmed_cleanup() {
            let inspector = Arc::new(RetryInspector::default());
            inspector.fail_markers(true);
            let fixture =
                fixture(Arc::<RetryInspector>::clone(&inspector) as Arc<dyn ProcessInspector>);
            let marked = Marked::spawn("runtime-s-306");
            persist_record(&fixture.root, "s-306", Some("runtime-s-306"));
            write_live_journal(&fixture.root, "s-306", dead_worker(), Some("runtime-s-306"));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-306").await;
            assert_eq!(runtime.state, RuntimeState::Lost);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(RUNTIME_LOST_CLEANUP_UNCONFIRMED)
            );
            assert!(
                marked.alive(),
                "an unreadable marker is never a signal permit"
            );
        }

        #[tokio::test]
        async fn mismatched_job_definition_fails_closed() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let marked = Marked::spawn("runtime-s-304");
            persist_record(&fixture.root, "s-304", Some("runtime-s-304"));
            write_live_journal(&fixture.root, "s-304", dead_worker(), Some("runtime-s-304"));
            fixture.supervisor.script_job(
                service_id("s-304", TEST_GENERATION),
                present(
                    ServiceState::Stopped,
                    Some(DefinitionFacts {
                        executable: PathBuf::from("/opt/other/pohunek-sessiond"),
                        arguments: vec![
                            "--session-id".to_owned(),
                            "s-304".to_owned(),
                            "--worker-generation".to_owned(),
                            TEST_GENERATION.to_owned(),
                        ],
                    }),
                ),
            );

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-304").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(IDENTITY_MISMATCH));
            assert!(fixture.supervisor.retired().is_empty());
            assert!(marked.alive());
        }

        #[tokio::test]
        async fn live_worker_behind_a_foreign_job_definition_is_not_adopted() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-305", "worker-s-305")
                    .await;
            persist_record(&fixture.root, "s-305", Some(worker_instance_id.as_str()));
            drop(controller);
            fixture.supervisor.script_job(
                service_id("s-305", TEST_GENERATION),
                present(
                    ServiceState::Running,
                    Some(DefinitionFacts {
                        executable: PathBuf::from(TEST_WORKER_EXECUTABLE),
                        arguments: vec![
                            "--session-id".to_owned(),
                            "s-999".to_owned(),
                            "--worker-generation".to_owned(),
                            TEST_GENERATION.to_owned(),
                        ],
                    }),
                ),
            );

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-305").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(IDENTITY_MISMATCH));
            crate::runtime::Worker::connect(
                &runtime_root
                    .join("s-305")
                    .join(pohunek_paths::WORKER_SOCKET_NAME),
                "s-305",
                "mismatch-probe",
            )
            .await
            .expect("the unadopted worker stays alive");
            server_task.abort();
        }

        #[tokio::test]
        async fn live_worker_of_the_record_generation_is_adopted_without_supervisor_evidence() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-306", "worker-s-306")
                    .await;
            persist_record(&fixture.root, "s-306", Some(worker_instance_id.as_str()));
            drop(controller);
            fixture
                .supervisor
                .script_job(service_id("s-306", TEST_GENERATION), JobScript::Unavailable);

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            assert_eq!(
                runtime_of(&fixture.registry, "s-306").await.state,
                RuntimeState::Live
            );
            fixture
                .registry
                .stop(&SessionId("s-306".to_owned()))
                .await
                .expect("stop adopted worker");
            server_task.abort();
        }

        /// A retry pass probes every socket: a canonical worker is adopted
        /// only while no other socket claims its session.
        #[tokio::test]
        async fn a_retry_pass_keeps_a_duplicate_claim_in_conflict_until_the_shadow_is_gone() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let session = "s-5240";
            let (controller, worker_instance_id, _child_pid, canonical_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, session, "worker-s-5240")
                    .await;
            persist_record(&fixture.root, session, Some(worker_instance_id.as_str()));
            drop(controller);
            let (_shadow_socket, shadow_task) = super::spawn_uninitialized_worker(
                &fixture.root,
                &runtime_root,
                "s-5240-shadow",
                session,
                "worker-s-5240-shadow",
            )
            .await;
            let id = SessionId(session.to_owned());

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            assert_eq!(
                runtime_of(&fixture.registry, session).await.state,
                RuntimeState::Conflict
            );

            assert!(
                !fixture.registry.retry_supervised_session(&id).await,
                "the duplicate claim keeps the session pending"
            );
            assert_eq!(
                runtime_of(&fixture.registry, session).await.state,
                RuntimeState::Conflict,
                "the canonical worker is not adopted next to its shadow"
            );

            shadow_task.abort();
            let _ = shadow_task.await;
            assert!(
                fixture.registry.retry_supervised_session(&id).await,
                "the settled session needs no further retry"
            );
            assert_eq!(
                runtime_of(&fixture.registry, session).await.state,
                RuntimeState::Live
            );
            fixture
                .registry
                .stop(&id)
                .await
                .expect("stop adopted worker");
            canonical_task.abort();
        }

        #[tokio::test]
        async fn live_worker_with_an_absent_job_is_adopted_and_flagged() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-315", "worker-s-315")
                    .await;
            persist_record(&fixture.root, "s-315", Some(worker_instance_id.as_str()));
            drop(controller);
            let mut events = fixture.registry.subscribe();

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            assert_eq!(
                runtime_of(&fixture.registry, "s-315").await.state,
                RuntimeState::Live
            );
            let inventory = fixture.registry.runtime_inventory().await;
            let entry = inventory
                .entries
                .iter()
                .find(|entry| entry.runtime_slot == "s-315")
                .expect("adopted worker is inventoried");
            assert_eq!(entry.status, RuntimeInventoryStatus::Managed);
            assert_eq!(entry.reason.as_deref(), Some(UNSUPERVISED_WORKER));
            let mut announced = false;
            while let Ok(event) = events.try_recv() {
                if event.event() == protocol::event::SESSION_RUNTIME_DISCOVERED {
                    announced |=
                        event.payload()["entry"]["reason"].as_str() == Some(UNSUPERVISED_WORKER);
                }
            }
            assert!(announced, "the anomaly is announced to subscribers");
            fixture
                .registry
                .stop(&SessionId("s-315".to_owned()))
                .await
                .expect("stop adopted worker");
            server_task.abort();
        }

        #[tokio::test]
        async fn stale_generations_are_retired_when_ended_and_left_orphaned_when_live() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-307", None);
            let current = service_id("s-307", TEST_GENERATION);
            let ended = service_id("s-307", "zzzz2222");
            let live = service_id("s-308", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(current.clone(), present(ServiceState::Failed, None));
            fixture
                .supervisor
                .script_job(ended.clone(), present(ServiceState::Stopped, None));
            fixture
                .supervisor
                .script_job(live.clone(), present(ServiceState::Running, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let retired = fixture.supervisor.retired();
            assert!(retired.contains(&ended), "{retired:?}");
            assert!(
                !retired.contains(&live),
                "a live stale job is never retired"
            );
            let inventory = fixture.registry.runtime_inventory().await;
            let orphan = inventory
                .entries
                .iter()
                .find(|entry| entry.runtime_slot == live.as_str())
                .expect("live stale job is inventoried");
            assert_eq!(orphan.status, RuntimeInventoryStatus::Orphaned);
            assert_eq!(orphan.reason.as_deref(), Some(STALE_GENERATION));
        }

        #[tokio::test]
        async fn unavailable_supervisor_keeps_the_session_reconnecting_and_retries() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let marked = Marked::spawn("runtime-s-309");
            persist_record(&fixture.root, "s-309", Some("runtime-s-309"));
            write_live_journal(
                &fixture.root,
                "s-309",
                dead_worker_with_realistic_start(),
                Some("runtime-s-309"),
            );
            let id = service_id("s-309", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), JobScript::Unavailable);
            fixture.supervisor.fail_discovery();

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-309").await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(SUPERVISION_UNAVAILABLE)
            );
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired"
            );
            assert!(marked.alive(), "nothing is swept");

            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            loop {
                let runtime = runtime_of(&fixture.registry, "s-309").await;
                if runtime.state == RuntimeState::Lost {
                    assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the background retry never re-reconciled: {runtime:?}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            marked.wait_gone().await;
            assert_eq!(fixture.supervisor.retired(), vec![id]);
        }

        #[tokio::test]
        async fn incompatible_worker_of_a_record_is_left_alive() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            persist_record(&fixture.root, "s-310", Some("runtime-s-310"));
            let fake = spawn_incompatible_worker(&runtime_root, "s-310");

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-310").await;
            assert_eq!(runtime.state, RuntimeState::Incompatible);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some("worker_protocol_incompatible")
            );
            assert!(fixture.supervisor.retired().is_empty());
            assert!(!fixture.supervisor.calls().iter().any(
                |call| matches!(call, Call::Inspect(id) if id.as_str().starts_with("s-310."))
            ));
            fake.abort();
        }

        /// Whether `id` is still registered with the supervisor.
        async fn discovered(supervisor: &ScriptedSupervisor, id: &ServiceId) -> bool {
            pohunek_platform::supervisor::Supervisor::discover(supervisor)
                .await
                .expect("discover scripted jobs")
                .iter()
                .any(|observation| observation.id == *id)
        }

        fn retire_count(supervisor: &ScriptedSupervisor, id: &ServiceId) -> usize {
            supervisor
                .retired()
                .into_iter()
                .filter(|retired| retired == id)
                .count()
        }

        #[tokio::test]
        async fn terminal_journal_retires_the_loaded_job_of_its_generation() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-350", Some("runtime-s-350"));
            write_terminal_journal(&fixture.root, "s-350", "runtime-s-350");
            // A launchd `RunAtLoad` job stays loaded after its worker exits.
            let id = service_id("s-350", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Unknown, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let info = fixture
                .registry
                .inspect(&SessionId("s-350".to_owned()))
                .await
                .expect("terminal session stays visible");
            assert_eq!(info.state, protocol::SessionState::Done);
            assert_eq!(info.runtime.expect("runtime").state, RuntimeState::Terminal);
            assert_eq!(fixture.supervisor.retired(), vec![id.clone()]);
            assert!(!discovered(&fixture.supervisor, &id).await);
        }

        #[tokio::test]
        async fn failed_terminal_generation_retirement_is_retried() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-351", Some("runtime-s-351"));
            write_terminal_journal(&fixture.root, "s-351", "runtime-s-351");
            let id = service_id("s-351", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Stopped, None));
            fixture.supervisor.script_retire_unavailable(id.clone());

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            // The proven outcome is recorded even though the job survived.
            let runtime = runtime_of(&fixture.registry, "s-351").await;
            assert_eq!(runtime.state, RuntimeState::Terminal);
            assert_eq!(retire_count(&fixture.supervisor, &id), 1);
            assert!(discovered(&fixture.supervisor, &id).await);

            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while discovered(&fixture.supervisor, &id).await {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the terminal generation's job was never retired again"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(retire_count(&fixture.supervisor, &id), 2);
            let runtime = runtime_of(&fixture.registry, "s-351").await;
            assert_eq!(runtime.state, RuntimeState::Terminal);
            assert!(
                !fixture
                    .registry
                    .inner
                    .supervision_retries
                    .state
                    .lock()
                    .expect("retry state")
                    .pending
                    .contains("s-351"),
                "a retired generation leaves the retry"
            );
        }

        #[tokio::test]
        async fn terminal_generation_behind_an_answering_worker_is_not_retired() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            persist_record(&fixture.root, "s-352", Some("runtime-s-352"));
            write_terminal_journal(&fixture.root, "s-352", "runtime-s-352");
            let id = service_id("s-352", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));
            let fake = spawn_incompatible_worker(&runtime_root, "s-352");

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-352").await;
            assert_eq!(runtime.state, RuntimeState::Terminal);
            assert_eq!(retire_count(&fixture.supervisor, &id), 0);
            assert!(discovered(&fixture.supervisor, &id).await);
            fake.abort();
        }

        #[tokio::test]
        async fn reused_worker_pid_counts_as_not_running() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let (pid, start) = own_identity();
            persist_record(&fixture.root, "s-311", None);
            write_live_journal(&fixture.root, "s-311", (pid, start + 1), None);
            persist_record(&fixture.root, "s-312", None);
            write_live_journal(&fixture.root, "s-312", (pid, start), None);

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let reused = runtime_of(&fixture.registry, "s-311").await;
            assert_eq!(reused.state, RuntimeState::Lost);
            assert_eq!(reused.loss_reason.as_deref(), Some(RUNTIME_LOST));
            let running = runtime_of(&fixture.registry, "s-312").await;
            assert_eq!(running.state, RuntimeState::Conflict);
            assert_eq!(running.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
        }

        #[tokio::test]
        async fn abandoned_create_is_compensated_only_after_its_job_is_retired() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let mut record = persist_record(&fixture.root, "s-313", None);
            record.runtime.worker_id = None;
            record.transaction = Some(crate::store::SessionTransaction {
                id: "create-s-313".to_owned(),
                kind: crate::store::TransactionKind::Create,
                phase: "preparing".to_owned(),
                previous_worker_id: None,
                previous_worker_instance_id: None,
                daemon_instance_id: None,
            });
            Store::new(fixture.root.join("data/metadata.jsonl"))
                .record_session(&record)
                .expect("persist preparing record");
            let id = service_id("s-313", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            assert_eq!(fixture.supervisor.retired(), vec![id]);
            assert!(Store::new(fixture.root.join("data/metadata.jsonl"))
                .load_sessions()
                .expect("load store")
                .is_empty());
        }

        #[tokio::test]
        async fn abandoned_create_settled_by_a_retry_leaves_the_registry() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let mut record = persist_record(&fixture.root, "s-316", None);
            record.runtime.worker_id = None;
            record.transaction = Some(crate::store::SessionTransaction {
                id: "create-s-316".to_owned(),
                kind: crate::store::TransactionKind::Create,
                phase: "preparing".to_owned(),
                previous_worker_id: None,
                previous_worker_instance_id: None,
                daemon_instance_id: None,
            });
            Store::new(fixture.root.join("data/metadata.jsonl"))
                .record_session(&record)
                .expect("persist preparing record");
            let id = service_id("s-316", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), JobScript::Unavailable);
            fixture.supervisor.fail_discovery();
            let mut events = fixture.registry.subscribe();

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-316").await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(SUPERVISION_UNAVAILABLE)
            );

            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while fixture
                .registry
                .inspect(&SessionId("s-316".to_owned()))
                .await
                .is_ok()
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the settled create is still listed"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(fixture.supervisor.retired(), vec![id]);
            assert!(Store::new(fixture.root.join("data/metadata.jsonl"))
                .load_sessions()
                .expect("load store")
                .is_empty());
            let mut removed = false;
            while let Ok(event) = events.try_recv() {
                removed |= event.event() == protocol::event::SESSION_REMOVED
                    && event.payload()["session"]["id"].as_str() == Some("s-316");
            }
            assert!(removed, "subscribers see the settled create removed");
        }

        /// Persists the preparing record of a create whose worker never
        /// journaled, naming [`TEST_GENERATION`].
        fn persist_preparing_record(root: &Path, session_id: &str) {
            let mut record = persist_record(root, session_id, None);
            record.runtime.worker_id = None;
            record.transaction = Some(crate::store::SessionTransaction {
                id: format!("create-{session_id}"),
                kind: crate::store::TransactionKind::Create,
                phase: "preparing".to_owned(),
                previous_worker_id: None,
                previous_worker_instance_id: None,
                daemon_instance_id: None,
            });
            Store::new(root.join("data/metadata.jsonl"))
                .record_session(&record)
                .expect("persist preparing record");
        }

        /// Waits until `session_id` has left both the registry and the store.
        ///
        /// A removal evicts the entry before it deletes the durable record,
        /// the last ownership record it removes, so an unlisted session is
        /// not yet a finished removal.
        async fn wait_removed(fixture: &Fixture, session_id: &str) {
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while fixture
                .registry
                .inspect(&SessionId(session_id.to_owned()))
                .await
                .is_ok()
                || recorded(&fixture.root, session_id)
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "{session_id} is still listed or recorded"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// Waits until procwatch has recorded `session_id`'s observed cwd.
        async fn wait_procwatch_cwd(registry: &SessionRegistry, session_id: &str) {
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            loop {
                let info = registry
                    .inspect(&SessionId(session_id.to_owned()))
                    .await
                    .expect("session stays visible");
                if info.cwd_source == Some(protocol::CwdSource::Procwatch) {
                    return;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "procwatch never observed the cwd of {session_id}: {info:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// Waits until `session_id`'s runtime is in `state`.
        async fn wait_state(
            registry: &SessionRegistry,
            session_id: &str,
            state: RuntimeState,
        ) -> protocol::SessionRuntime {
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            loop {
                let runtime = runtime_of(registry, session_id).await;
                if runtime.state == state {
                    return runtime;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "{session_id} never reached {state:?}: {runtime:?}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        #[tokio::test]
        async fn abandoned_create_with_a_live_job_is_retired_after_its_initialization_deadline() {
            const INITIALIZE: Duration = Duration::from_secs(1);

            let fixture = fixture_with(Arc::new(RetryInspector::default()), Some(INITIALIZE));
            persist_preparing_record(&fixture.root, "s-317");
            let id = service_id("s-317", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));

            let started = tokio::time::Instant::now();
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            assert!(
                started.elapsed() < INITIALIZE,
                "startup does not wait for the abandoned create"
            );
            let runtime = runtime_of(&fixture.registry, "s-317").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired early"
            );

            wait_removed(&fixture, "s-317").await;
            assert!(
                started.elapsed() >= INITIALIZE,
                "the job was watched until its initialization deadline"
            );
            assert_eq!(fixture.supervisor.retired(), vec![id]);
            assert!(Store::new(fixture.root.join("data/metadata.jsonl"))
                .load_sessions()
                .expect("load store")
                .is_empty());
        }

        #[tokio::test]
        async fn abandoned_create_waits_for_an_unavailable_supervisor() {
            const INITIALIZE: Duration = Duration::from_secs(1);

            let fixture = fixture_with(Arc::new(RetryInspector::default()), Some(INITIALIZE));
            persist_preparing_record(&fixture.root, "s-318");
            let id = service_id("s-318", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            fixture
                .supervisor
                .script_job(id.clone(), JobScript::Unavailable);

            let runtime = wait_state(&fixture.registry, "s-318", RuntimeState::Reconnecting).await;
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(SUPERVISION_UNAVAILABLE)
            );
            assert!(fixture.supervisor.retired().is_empty(), "nothing is killed");

            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));
            wait_removed(&fixture, "s-318").await;
            assert_eq!(fixture.supervisor.retired(), vec![id]);
        }

        #[tokio::test]
        async fn ambiguous_generation_is_adopted_once_its_worker_is_reachable_again() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-319", "worker-s-319")
                    .await;
            persist_record(&fixture.root, "s-319", Some(worker_instance_id.as_str()));
            drop(controller);
            let socket = runtime_root
                .join("s-319")
                .join(pohunek_paths::WORKER_SOCKET_NAME);
            let hidden = socket.with_extension("hidden");
            std::fs::rename(&socket, &hidden).expect("hide the worker socket");
            fixture.supervisor.script_job(
                service_id("s-319", TEST_GENERATION),
                present(ServiceState::Running, None),
            );

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-319").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));

            std::fs::rename(&hidden, &socket).expect("restore the worker socket");
            let runtime = wait_state(&fixture.registry, "s-319", RuntimeState::Live).await;
            assert_eq!(
                runtime.worker_instance_id.as_deref(),
                Some(worker_instance_id.as_str())
            );
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing was retired"
            );
            fixture
                .registry
                .stop(&SessionId("s-319".to_owned()))
                .await
                .expect("stop the adopted worker");
            server_task.abort();
        }

        /// The watcher tokens and identity of `session_id`'s registry entry.
        async fn watchers_of(
            registry: &SessionRegistry,
            session_id: &str,
        ) -> (
            [tokio_util::sync::CancellationToken; 3],
            crate::session::RuntimeWatchIdentity,
        ) {
            let sessions = registry.inner.sessions.lock().await;
            let entry = sessions
                .get(&SessionId(session_id.to_owned()))
                .expect("session entry");
            let identity = crate::session::RuntimeWatchIdentity::from_info(&entry.info)
                .expect("live watcher identity");
            (
                [
                    entry.detector_cancel.clone(),
                    entry.procwatch_cancel.clone(),
                    entry.runtime_watch_cancel.clone(),
                ],
                identity,
            )
        }

        #[tokio::test]
        async fn readopted_worker_is_watched_only_by_its_new_watchers() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-350", "worker-s-350")
                    .await;
            persist_record(&fixture.root, "s-350", Some(worker_instance_id.as_str()));
            drop(controller);
            fixture.supervisor.script_job(
                service_id("s-350", TEST_GENERATION),
                present(ServiceState::Running, None),
            );
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            assert_eq!(
                runtime_of(&fixture.registry, "s-350").await.state,
                RuntimeState::Live
            );
            let id = SessionId("s-350".to_owned());
            // The adopted procwatch moves the recorded cwd to the worker's
            // shortly after adoption; a transition racing that move is a
            // retryable concurrent change, so the entry must settle first.
            wait_procwatch_cwd(&fixture.registry, "s-350").await;
            let (old_watchers, identity) = watchers_of(&fixture.registry, "s-350").await;
            assert!(old_watchers.iter().all(|token| !token.is_cancelled()));

            let outcome = fixture
                .registry
                .mark_worker_unavailable(
                    &id,
                    &identity,
                    RuntimeState::Conflict,
                    SUPERVISION_AMBIGUOUS,
                )
                .await;
            assert!(
                matches!(
                    outcome,
                    crate::session::RuntimeTransitionOutcome::Applied(_)
                ),
                "the live runtime is marked unavailable: {outcome:?}"
            );
            assert!(
                old_watchers
                    .iter()
                    .all(tokio_util::sync::CancellationToken::is_cancelled),
                "an unavailable runtime keeps no watcher"
            );

            fixture.registry.schedule_supervision_retry(&id);
            wait_state(&fixture.registry, "s-350", RuntimeState::Live).await;
            let (new_watchers, readopted) = watchers_of(&fixture.registry, "s-350").await;
            assert_eq!(readopted, identity, "the same runtime is re-adopted");
            assert!(
                new_watchers.iter().all(|token| !token.is_cancelled()),
                "the re-adopted runtime is watched"
            );
            assert!(
                old_watchers
                    .iter()
                    .all(tokio_util::sync::CancellationToken::is_cancelled),
                "the previous watchers never resume"
            );
            fixture
                .registry
                .stop(&id)
                .await
                .expect("stop the re-adopted worker");
            server_task.abort();
        }

        #[tokio::test]
        async fn reclassified_entry_cancels_the_watchers_it_replaces() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-351", "worker-s-351")
                    .await;
            let record = persist_record(&fixture.root, "s-351", Some(worker_instance_id.as_str()));
            drop(controller);
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            assert_eq!(
                runtime_of(&fixture.registry, "s-351").await.state,
                RuntimeState::Live
            );
            let (old_watchers, _) = watchers_of(&fixture.registry, "s-351").await;

            assert!(
                fixture
                    .registry
                    .insert_unavailable_record(record, RuntimeState::Conflict, IDENTITY_MISMATCH)
                    .await
            );

            assert_eq!(
                runtime_of(&fixture.registry, "s-351").await.state,
                RuntimeState::Conflict
            );
            assert!(
                old_watchers
                    .iter()
                    .all(tokio_util::sync::CancellationToken::is_cancelled),
                "a replaced entry keeps no watcher"
            );
            server_task.abort();
        }

        /// Whether the store still holds a record of `session_id`.
        fn recorded(root: &Path, session_id: &str) -> bool {
            durable_desired_state(root, session_id).is_some()
        }

        /// The durable desired state of `session_id`, when it is recorded.
        fn durable_desired_state(root: &Path, session_id: &str) -> Option<DesiredState> {
            Store::new(root.join("data/metadata.jsonl"))
                .load_sessions()
                .expect("load store")
                .into_iter()
                .find(|record| record.session_id == session_id)
                .map(|record| record.desired_state)
        }

        /// Asserts that removing `session_id` fails with `code` and keeps the
        /// session exactly as it was, without retiring anything.
        async fn assert_removal_refused(fixture: &Fixture, session_id: &str, code: &str) {
            let error = fixture
                .registry
                .remove(&SessionId(session_id.to_owned()))
                .await
                .expect_err("the removal is refused");
            assert_eq!(error.code, code, "{error:?}");
            assert_eq!(
                durable_desired_state(&fixture.root, session_id),
                Some(DesiredState::Running),
                "the record is kept without a removal intent"
            );
            assert_eq!(
                runtime_of(&fixture.registry, session_id).await.state,
                RuntimeState::Conflict
            );
        }

        #[tokio::test]
        async fn removing_a_conflict_whose_journaled_worker_still_runs_is_refused() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-356", Some("runtime-s-356"));
            // The test process stands in for a worker that outlived its job.
            write_live_journal(
                &fixture.root,
                "s-356",
                own_identity(),
                Some("runtime-s-356"),
            );
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-356").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));

            assert_removal_refused(&fixture, "s-356", SUPERVISION_AMBIGUOUS).await;
        }

        #[tokio::test]
        async fn removing_a_conflict_whose_journal_names_another_generation_is_refused() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-357", Some("runtime-s-357"));
            write_live_journal(&fixture.root, "s-357", dead_worker(), Some("runtime-s-357"));
            fixture.supervisor.script_job(
                service_id("s-357", TEST_GENERATION),
                present(ServiceState::Running, None),
            );
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-357").await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            write_generation_journal(
                &fixture.root,
                "s-357",
                "efgh2345",
                own_identity(),
                Some("runtime-s-357"),
            );

            assert_removal_refused(&fixture, "s-357", IDENTITY_MISMATCH).await;
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired for a foreign journal"
            );
        }

        #[tokio::test]
        async fn removing_a_conflict_whose_record_names_no_worker_proves_the_journaled_one() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let mut record = persist_record(&fixture.root, "s-360", Some("runtime-s-360"));
            record.runtime.worker_id = None;
            record.info.runtime.as_mut().expect("runtime").worker_id = None;
            Store::new(fixture.root.join("data/metadata.jsonl"))
                .record_session(&record)
                .expect("persist a record naming no worker");
            // The test process stands in for a worker that journaled before
            // the daemon recorded it, and outlived its job.
            write_live_journal(
                &fixture.root,
                "s-360",
                own_identity(),
                Some("runtime-s-360"),
            );
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-360").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert_eq!(runtime.worker_id, None);

            assert_removal_refused(&fixture, "s-360", SUPERVISION_AMBIGUOUS).await;
        }

        #[tokio::test]
        async fn removing_a_conflict_proves_every_journal_of_its_generation() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-361", Some("runtime-s-361"));
            write_live_journal(&fixture.root, "s-361", dead_worker(), Some("runtime-s-361"));
            // A second journal claims the same generation; its worker runs.
            write_worker_journal(
                &fixture.root,
                "s-361",
                "worker-s-361-other",
                TEST_GENERATION,
                own_identity(),
                Some("runtime-s-361"),
            );
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-361").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));

            assert_removal_refused(&fixture, "s-361", SUPERVISION_AMBIGUOUS).await;
        }

        #[tokio::test]
        async fn removing_a_conflict_over_a_foreign_job_is_refused() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-358", Some("runtime-s-358"));
            write_live_journal(&fixture.root, "s-358", dead_worker(), Some("runtime-s-358"));
            fixture.supervisor.script_job(
                service_id("s-358", TEST_GENERATION),
                present(
                    ServiceState::Running,
                    Some(DefinitionFacts {
                        executable: PathBuf::from("/opt/other/pohunek-sessiond"),
                        arguments: vec![
                            "--session-id".to_owned(),
                            "s-358".to_owned(),
                            "--worker-generation".to_owned(),
                            TEST_GENERATION.to_owned(),
                        ],
                    }),
                ),
            );
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-358").await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(IDENTITY_MISMATCH));

            assert_removal_refused(&fixture, "s-358", IDENTITY_MISMATCH).await;
            assert!(
                fixture.supervisor.retired().is_empty(),
                "a job reconciliation classified foreign is never retired"
            );
        }

        #[tokio::test]
        async fn reconciled_removal_intent_evicts_the_listed_session() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-359", "worker-s-359")
                    .await;
            let mut record =
                persist_record(&fixture.root, "s-359", Some(worker_instance_id.as_str()));
            drop(controller);
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            assert_eq!(
                runtime_of(&fixture.registry, "s-359").await.state,
                RuntimeState::Live
            );
            let (watchers, _) = watchers_of(&fixture.registry, "s-359").await;
            let mut events = fixture.registry.subscribe();
            record.desired_state = DesiredState::Removed;

            let adopted = fixture
                .registry
                .inner
                .sessions
                .lock()
                .await
                .get(&SessionId("s-359".to_owned()))
                .and_then(|entry| match &entry.runtime {
                    crate::session::RuntimeHandle::Worker(worker) => Some(worker.clone()),
                    crate::session::RuntimeHandle::Unavailable(_) => None,
                })
                .expect("adopted worker");
            let pending = Box::pin(
                fixture
                    .registry
                    .finish_reconciled_removal(adopted, record, true),
            )
            .await;

            assert!(!pending, "a confirmed stop finishes the removal");
            assert_eq!(
                fixture.supervisor.retired(),
                vec![service_id("s-359", TEST_GENERATION)],
                "the exact generation is retired before its record is deleted"
            );

            fixture
                .registry
                .inspect(&SessionId("s-359".to_owned()))
                .await
                .expect_err("the finished removal leaves the registry");
            assert!(!recorded(&fixture.root, "s-359"));
            assert!(watchers
                .iter()
                .all(tokio_util::sync::CancellationToken::is_cancelled));
            let removed = loop {
                let event = tokio::time::timeout(HANG_GUARD, events.recv())
                    .await
                    .expect("removal event")
                    .expect("event stream");
                if event.event() == protocol::event::SESSION_REMOVED {
                    break event;
                }
            };
            assert_eq!(removed.event(), protocol::event::SESSION_REMOVED);
            server_task.abort();
        }

        /// Persists a running record of `session_id` (see [`persist_record`])
        /// carrying a durable removal intent, as `remove` writes it before
        /// the daemon went away.
        fn persist_removal_intent(
            root: &Path,
            session_id: &str,
            worker_instance_id: Option<&str>,
        ) -> SessionRecord {
            let mut record = persist_record(root, session_id, worker_instance_id);
            record.desired_state = DesiredState::Removed;
            record.transaction = Some(crate::store::SessionTransaction {
                id: format!("remove-{session_id}"),
                kind: crate::store::TransactionKind::Remove,
                phase: "requested".to_owned(),
                previous_worker_id: None,
                previous_worker_instance_id: None,
                daemon_instance_id: None,
            });
            Store::new(root.join("data/metadata.jsonl"))
                .record_session(&record)
                .expect("persist removal intent");
            record
        }

        /// Leaves a worker log family and a resume binding of `record`, the
        /// state a finished removal must delete.
        fn seed_removal_leftovers(root: &Path, record: &SessionRecord) {
            let files = pohunek_logging::config::worker_files(&record.session_id)
                .expect("safe managed session id");
            let mut writer = pohunek_logging::Writer::open(
                &root.join("logs"),
                files,
                pohunek_logging::config::worker_policy().expect("valid worker log policy"),
            )
            .expect("open worker log family");
            std::io::Write::write_all(&mut writer, b"{\"worker\":\"running\"}\n")
                .expect("write worker log");
            drop(writer);
            Store::new(root.join("data/metadata.jsonl"))
                .record_resume(record.recovery.as_ref().expect("recovery"))
                .expect("persist resume binding");
        }

        /// Asserts that `session_id` left no record, listing, resume binding,
        /// or worker log behind.
        async fn assert_removal_finished(fixture: &Fixture, session_id: &str) {
            fixture
                .registry
                .inspect(&SessionId(session_id.to_owned()))
                .await
                .expect_err("the finished removal is not listed");
            assert!(!recorded(&fixture.root, session_id));
            assert!(
                !Store::new(fixture.root.join("data/metadata.jsonl"))
                    .load_resume()
                    .expect("load resume bindings")
                    .iter()
                    .any(|binding| binding.session_id == session_id),
                "the resume binding is cleared"
            );
            assert!(
                std::fs::read_dir(fixture.root.join("logs"))
                    .expect("read log directory")
                    .next()
                    .is_none(),
                "the worker log family is deleted"
            );
        }

        #[tokio::test]
        async fn removal_intent_with_a_terminal_journal_finishes_at_startup() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let record = persist_removal_intent(&fixture.root, "s-370", Some("runtime-s-370"));
            seed_removal_leftovers(&fixture.root, &record);
            write_terminal_journal(&fixture.root, "s-370", "runtime-s-370");
            let id = service_id("s-370", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Unknown, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            assert_removal_finished(&fixture, "s-370").await;
            assert_eq!(fixture.supervisor.retired(), vec![id.clone()]);
            assert!(!discovered(&fixture.supervisor, &id).await);
        }

        #[tokio::test]
        async fn removal_intent_whose_job_ended_finishes_at_startup() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let record = persist_removal_intent(&fixture.root, "s-371", Some("runtime-s-371"));
            seed_removal_leftovers(&fixture.root, &record);
            write_live_journal(
                &fixture.root,
                "s-371",
                dead_worker_with_realistic_start(),
                Some("runtime-s-371"),
            );
            let id = service_id("s-371", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            assert_removal_finished(&fixture, "s-371").await;
            assert_eq!(fixture.supervisor.retired(), vec![id]);
        }

        #[tokio::test]
        async fn removal_intent_whose_generation_cannot_be_retired_is_retried() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let record = persist_removal_intent(&fixture.root, "s-372", Some("runtime-s-372"));
            seed_removal_leftovers(&fixture.root, &record);
            write_terminal_journal(&fixture.root, "s-372", "runtime-s-372");
            let id = service_id("s-372", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Stopped, None));
            fixture.supervisor.script_retire_unavailable(id.clone());

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-372").await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(SUPERVISION_UNAVAILABLE)
            );
            assert_eq!(
                durable_desired_state(&fixture.root, "s-372"),
                Some(DesiredState::Removed),
                "an unretired generation keeps the removal intent"
            );
            assert_eq!(retire_count(&fixture.supervisor, &id), 1);

            wait_removed(&fixture, "s-372").await;
            assert_removal_finished(&fixture, "s-372").await;
            assert_eq!(retire_count(&fixture.supervisor, &id), 2);
        }

        #[tokio::test]
        async fn ended_generation_is_lost_only_after_its_job_is_retired() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-373", Some("runtime-s-373"));
            write_live_journal(
                &fixture.root,
                "s-373",
                dead_worker_with_realistic_start(),
                Some("runtime-s-373"),
            );
            let id = service_id("s-373", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));
            fixture.supervisor.script_retire_unavailable(id.clone());

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-373").await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(SUPERVISION_UNAVAILABLE)
            );
            assert!(discovered(&fixture.supervisor, &id).await);

            let runtime = wait_state(&fixture.registry, "s-373", RuntimeState::Lost).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            assert_eq!(retire_count(&fixture.supervisor, &id), 2);
            assert!(!discovered(&fixture.supervisor, &id).await);
        }

        #[tokio::test]
        async fn replayed_removal_retires_the_generation_and_cleans_up() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-374", "worker-s-374")
                    .await;
            let record =
                persist_removal_intent(&fixture.root, "s-374", Some(worker_instance_id.as_str()));
            seed_removal_leftovers(&fixture.root, &record);
            drop(controller);
            let id = service_id("s-374", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            assert_removal_finished(&fixture, "s-374").await;
            assert_eq!(fixture.supervisor.retired(), vec![id]);
            server_task.abort();
        }

        #[tokio::test]
        async fn replayed_removal_keeps_its_intent_until_the_generation_is_retired() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-375", "worker-s-375")
                    .await;
            let record =
                persist_removal_intent(&fixture.root, "s-375", Some(worker_instance_id.as_str()));
            seed_removal_leftovers(&fixture.root, &record);
            drop(controller);
            let id = service_id("s-375", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));
            fixture.supervisor.script_retire_unavailable(id.clone());

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-375").await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(SUPERVISION_UNAVAILABLE)
            );
            assert_eq!(
                durable_desired_state(&fixture.root, "s-375"),
                Some(DesiredState::Removed),
                "a stopped worker whose generation survived keeps the removal intent"
            );
            assert_eq!(retire_count(&fixture.supervisor, &id), 1);

            wait_removed(&fixture, "s-375").await;
            assert_removal_finished(&fixture, "s-375").await;
            assert_eq!(retire_count(&fixture.supervisor, &id), 2);
            server_task.abort();
        }

        /// A process-table scan that aborts after the replayed stop (a task
        /// mid-fork or mid-exit on a loaded host) proves no marked process
        /// gone, so the session waits as `runtime_supervision_ambiguous` with
        /// its removal intent and nothing deleted, and the re-check finishes
        /// the removal once the table reads.
        #[tokio::test]
        async fn replayed_removal_keeps_its_intent_until_the_sweep_is_confirmed() {
            let inspector = Arc::new(RetryInspector::default());
            let fixture =
                fixture(Arc::<RetryInspector>::clone(&inspector) as Arc<dyn ProcessInspector>);
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-382", "worker-s-382")
                    .await;
            let record =
                persist_removal_intent(&fixture.root, "s-382", Some(worker_instance_id.as_str()));
            seed_removal_leftovers(&fixture.root, &record);
            drop(controller);
            fixture.supervisor.script_job(
                service_id("s-382", TEST_GENERATION),
                present(ServiceState::Running, None),
            );
            inspector.fail_enumeration(true);

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            assert_removal_waits_for_the_sweep(&fixture, "s-382").await;
            assert!(
                std::fs::read_dir(fixture.root.join("logs"))
                    .expect("read log directory")
                    .next()
                    .is_some(),
                "nothing the session owns is deleted before the sweep is confirmed"
            );

            inspector.fail_enumeration(false);
            wait_removed(&fixture, "s-382").await;
            assert_removal_finished(&fixture, "s-382").await;
            server_task.abort();
        }

        /// Persists a running record of `session_id` (see [`persist_record`])
        /// carrying a durable stop intent, as `stop` writes it before the
        /// daemon went away.
        fn persist_stop_intent(
            root: &Path,
            session_id: &str,
            worker_instance_id: Option<&str>,
        ) -> SessionRecord {
            let mut record = persist_record(root, session_id, worker_instance_id);
            record.desired_state = DesiredState::Stopped;
            record.transaction = Some(crate::store::SessionTransaction {
                id: format!("stop-{session_id}"),
                kind: crate::store::TransactionKind::Stop,
                phase: "requested".to_owned(),
                previous_worker_id: None,
                previous_worker_instance_id: None,
                daemon_instance_id: None,
            });
            Store::new(root.join("data/metadata.jsonl"))
                .record_session(&record)
                .expect("persist stop intent");
            record
        }

        /// The durable transaction of `session_id`, when it is recorded.
        fn durable_transaction(
            root: &Path,
            session_id: &str,
        ) -> Option<crate::store::SessionTransaction> {
            Store::new(root.join("data/metadata.jsonl"))
                .load_sessions()
                .expect("load store")
                .into_iter()
                .find(|record| record.session_id == session_id)
                .and_then(|record| record.transaction)
        }

        #[tokio::test]
        async fn failed_stop_replay_keeps_the_intent_and_the_retry_replays_it() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-390", "worker-s-390")
                    .await;
            let record =
                persist_stop_intent(&fixture.root, "s-390", Some(worker_instance_id.as_str()));
            fixture.supervisor.script_job(
                service_id("s-390", TEST_GENERATION),
                present(ServiceState::Running, None),
            );
            let snapshot = controller.inspect().await.expect("inspect live worker");
            // A released lease is refused every scoped request, so this
            // replay fails while the worker and its PTY stay up.
            controller
                .release_controller()
                .await
                .expect("release the controller lease");

            let pending = Box::pin(fixture.registry.reconcile_record(
                record,
                Some((controller, snapshot)),
                false,
            ))
            .await;

            assert!(
                pending,
                "a failed stop replay needs the background re-check"
            );
            let runtime = runtime_of(&fixture.registry, "s-390").await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(SUPERVISION_UNAVAILABLE)
            );
            assert_eq!(
                durable_desired_state(&fixture.root, "s-390"),
                Some(DesiredState::Stopped)
            );
            assert!(
                durable_transaction(&fixture.root, "s-390").is_some(),
                "the failed replay keeps the stop intent"
            );
            assert!(
                HostInspector::new()
                    .identity(child_pid)
                    .is_ok_and(|identity| identity.is_some()),
                "the failed replay stopped nothing"
            );

            // Startup schedules the retry for a pending reconciliation.
            let session = SessionId("s-390".to_owned());
            fixture.registry.schedule_supervision_retry(&session);
            wait_state(&fixture.registry, "s-390", RuntimeState::Terminal).await;

            let stopped = fixture
                .registry
                .inspect(&session)
                .await
                .expect("stopped session stays visible");
            assert_eq!(stopped.state, protocol::SessionState::Stopped);
            assert_eq!(
                durable_transaction(&fixture.root, "s-390"),
                None,
                "the replayed stop commits the intent"
            );
            server_task.abort();
        }

        #[tokio::test]
        async fn stop_intent_of_a_stopping_worker_is_replayed() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, worker_instance_id, _child_pid, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-391", "worker-s-391")
                    .await;
            let record =
                persist_stop_intent(&fixture.root, "s-391", Some(worker_instance_id.as_str()));
            let mut snapshot = controller.inspect().await.expect("inspect live worker");
            // A worker whose earlier stop still waits for its terminal commit
            // reports `Stopping`; its stop intent is replayed, not imported.
            snapshot.phase = pohunek_worker_protocol::RuntimePhase::Stopping;

            let pending = Box::pin(fixture.registry.reconcile_record(
                record,
                Some((controller, snapshot)),
                false,
            ))
            .await;

            assert!(!pending);
            let stopped = fixture
                .registry
                .inspect(&SessionId("s-391".to_owned()))
                .await
                .expect("stopped session stays visible");
            assert_eq!(stopped.state, protocol::SessionState::Stopped);
            assert_eq!(
                stopped.runtime.expect("runtime").state,
                RuntimeState::Terminal
            );
            assert_eq!(durable_transaction(&fixture.root, "s-391"), None);
            server_task.abort();
        }

        #[tokio::test]
        async fn removing_a_conflicting_session_retires_its_generation_first() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-352", Some("runtime-s-352"));
            write_live_journal(&fixture.root, "s-352", dead_worker(), Some("runtime-s-352"));
            let id = service_id("s-352", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-352").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert!(fixture.supervisor.retired().is_empty());

            let removed = fixture
                .registry
                .remove(&SessionId("s-352".to_owned()))
                .await
                .expect("remove the conflicting session");

            assert!(removed.removed);
            assert_eq!(fixture.supervisor.retired(), vec![id]);
            assert!(!recorded(&fixture.root, "s-352"));
        }

        /// The background supervision retry sleeps before it re-attempts the
        /// removal, so the clock is paused: the retried removal below is issued
        /// before that delay can elapse, however slow the fsyncs are.
        #[tokio::test(start_paused = true)]
        async fn removal_whose_record_cannot_be_deleted_stays_listed_for_a_retry() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-376", Some("runtime-s-376"));
            write_live_journal(&fixture.root, "s-376", dead_worker(), Some("runtime-s-376"));
            let id = service_id("s-376", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            assert_eq!(
                runtime_of(&fixture.registry, "s-376").await.state,
                RuntimeState::Conflict
            );
            let session = SessionId("s-376".to_owned());
            let mut events = fixture.registry.subscribe();
            // The removal intent is the first write; the record deletion that
            // follows the eviction is the second.
            fixture
                .registry
                .inner
                .store
                .as_ref()
                .expect("registry store")
                .fail_write_before_rename_after(2);

            let error = fixture
                .registry
                .remove(&session)
                .await
                .expect_err("the record deletion fails");

            assert_eq!(error.code, "session_store_failed");
            assert_eq!(
                durable_desired_state(&fixture.root, "s-376"),
                Some(DesiredState::Removed),
                "the failed deletion keeps the removal intent"
            );
            assert_eq!(
                runtime_of(&fixture.registry, "s-376").await.state,
                RuntimeState::Conflict,
                "the session stays listed for a retried removal"
            );
            while let Ok(event) = events.try_recv() {
                assert_ne!(
                    event.event(),
                    protocol::event::SESSION_REMOVED,
                    "an unfinished removal is not announced"
                );
            }

            let removed = fixture
                .registry
                .remove(&session)
                .await
                .expect("the retried removal finishes");

            assert!(removed.removed);
            assert!(!recorded(&fixture.root, "s-376"));
            assert_eq!(retire_count(&fixture.supervisor, &id), 2);
            let mut announced = false;
            while let Ok(event) = events.try_recv() {
                announced |= event.event() == protocol::event::SESSION_REMOVED;
            }
            assert!(announced, "the finished removal is announced");
        }

        #[tokio::test]
        async fn reconciled_removal_whose_record_cannot_be_deleted_is_retried() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let record = persist_removal_intent(&fixture.root, "s-377", Some("runtime-s-377"));
            seed_removal_leftovers(&fixture.root, &record);
            write_terminal_journal(&fixture.root, "s-377", "runtime-s-377");
            let id = service_id("s-377", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Unknown, None));
            // Clearing the resume binding is the first write; the record
            // deletion is the second.
            fixture
                .registry
                .inner
                .store
                .as_ref()
                .expect("registry store")
                .fail_write_before_rename_after(2);

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-377").await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(SUPERVISION_UNAVAILABLE)
            );
            assert_eq!(
                durable_desired_state(&fixture.root, "s-377"),
                Some(DesiredState::Removed),
                "the failed deletion keeps the removal intent"
            );
            assert_eq!(retire_count(&fixture.supervisor, &id), 1);

            wait_removed(&fixture, "s-377").await;
            assert_removal_finished(&fixture, "s-377").await;
            assert_eq!(retire_count(&fixture.supervisor, &id), 2);
        }

        /// A fixture whose marker reads fail until the returned inspector
        /// is told otherwise; the process table is the readable host view.
        fn unreadable_marker_fixture() -> (Fixture, Arc<RetryInspector>) {
            let inspector = Arc::new(RetryInspector::default());
            inspector.fail_markers(true);
            let fixture =
                fixture(Arc::<RetryInspector>::clone(&inspector) as Arc<dyn ProcessInspector>);
            (fixture, inspector)
        }

        /// Asserts that `session_id` waits, listed and recorded with its
        /// removal intent, for its runtime's marked processes to be swept.
        async fn assert_removal_waits_for_the_sweep(fixture: &Fixture, session_id: &str) {
            let runtime = runtime_of(&fixture.registry, session_id).await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert_eq!(
                durable_desired_state(&fixture.root, session_id),
                Some(DesiredState::Removed),
                "an unconfirmed sweep keeps the removal intent"
            );
        }

        #[tokio::test]
        async fn removal_intent_with_a_terminal_journal_waits_for_a_confirmed_sweep() {
            let (fixture, inspector) = unreadable_marker_fixture();
            let marked = Marked::spawn("runtime-s-378");
            let record = persist_removal_intent(&fixture.root, "s-378", Some("runtime-s-378"));
            seed_removal_leftovers(&fixture.root, &record);
            write_terminal_journal(&fixture.root, "s-378", "runtime-s-378");
            let id = service_id("s-378", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Unknown, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            assert_removal_waits_for_the_sweep(&fixture, "s-378").await;
            assert!(
                marked.alive(),
                "an unreadable marker is never a signal permit"
            );

            inspector.fail_markers(false);
            wait_removed(&fixture, "s-378").await;
            assert_removal_finished(&fixture, "s-378").await;
            marked.wait_gone().await;
        }

        #[tokio::test]
        async fn removal_intent_whose_job_ended_waits_for_a_confirmed_sweep() {
            let (fixture, inspector) = unreadable_marker_fixture();
            let marked = Marked::spawn("runtime-s-379");
            let record = persist_removal_intent(&fixture.root, "s-379", Some("runtime-s-379"));
            seed_removal_leftovers(&fixture.root, &record);
            write_live_journal(&fixture.root, "s-379", dead_worker(), Some("runtime-s-379"));
            let id = service_id("s-379", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            assert_removal_waits_for_the_sweep(&fixture, "s-379").await;
            assert!(marked.alive());

            inspector.fail_markers(false);
            wait_removed(&fixture, "s-379").await;
            assert_removal_finished(&fixture, "s-379").await;
            marked.wait_gone().await;
        }

        #[tokio::test]
        async fn removal_sweeps_every_runtime_of_its_generation() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let recorded_runtime = Marked::spawn("runtime-s-380");
            let journaled_runtime = Marked::spawn("runtime-s-380-journaled");
            let bystander = Marked::spawn("runtime-s-380-other");
            persist_record(&fixture.root, "s-380", Some("runtime-s-380"));
            write_live_journal(&fixture.root, "s-380", dead_worker(), Some("runtime-s-380"));
            // A second journal of the generation names a runtime the record
            // does not.
            write_worker_journal(
                &fixture.root,
                "s-380",
                "worker-s-380-other",
                TEST_GENERATION,
                dead_worker(),
                Some("runtime-s-380-journaled"),
            );
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-380").await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));

            let removed = fixture
                .registry
                .remove(&SessionId("s-380".to_owned()))
                .await
                .expect("remove once every worker is gone");

            assert!(removed.removed);
            assert!(!recorded(&fixture.root, "s-380"));
            recorded_runtime.wait_gone().await;
            journaled_runtime.wait_gone().await;
            assert!(bystander.alive(), "another runtime is never swept");
        }

        #[tokio::test]
        async fn explicit_removal_keeps_its_intent_until_the_runtime_is_swept() {
            let inspector = Arc::new(RetryInspector::default());
            let fixture =
                fixture(Arc::<RetryInspector>::clone(&inspector) as Arc<dyn ProcessInspector>);
            let marked = Marked::spawn("runtime-s-381");
            persist_record(&fixture.root, "s-381", Some("runtime-s-381"));
            write_live_journal(&fixture.root, "s-381", dead_worker(), Some("runtime-s-381"));
            let id = service_id("s-381", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            assert_eq!(
                runtime_of(&fixture.registry, "s-381").await.state,
                RuntimeState::Conflict
            );
            let session = SessionId("s-381".to_owned());
            inspector.fail_markers(true);

            let error = fixture
                .registry
                .remove(&session)
                .await
                .expect_err("an unconfirmed sweep blocks the removal");

            assert_eq!(error.code, SUPERVISION_AMBIGUOUS, "{error:?}");
            assert_removal_waits_for_the_sweep(&fixture, "s-381").await;
            assert!(marked.alive());
            assert_eq!(fixture.supervisor.retired(), vec![id.clone()]);

            inspector.fail_markers(false);
            let removed = fixture
                .registry
                .remove(&session)
                .await
                .expect("the retried removal sweeps and finishes");

            assert!(removed.removed);
            assert!(!recorded(&fixture.root, "s-381"));
            marked.wait_gone().await;
        }

        #[tokio::test]
        async fn removing_a_reconnecting_session_keeps_it_until_its_generation_is_retired() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-353", Some("runtime-s-353"));
            write_live_journal(&fixture.root, "s-353", dead_worker(), Some("runtime-s-353"));
            let id = service_id("s-353", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), JobScript::Unavailable);
            fixture.supervisor.fail_discovery();
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-353").await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            let session = SessionId("s-353".to_owned());

            let error = fixture
                .registry
                .remove(&session)
                .await
                .expect_err("an unretirable generation blocks removal");

            assert_eq!(error.code, SUPERVISION_UNAVAILABLE);
            assert_eq!(fixture.supervisor.retired(), vec![id.clone()]);
            assert_eq!(
                durable_desired_state(&fixture.root, "s-353"),
                Some(DesiredState::Running),
                "a failed retirement leaves no removal intent"
            );
            fixture
                .registry
                .inspect(&session)
                .await
                .expect("the session stays listed");

            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));
            let removed = fixture
                .registry
                .remove(&session)
                .await
                .expect("remove once the generation can be retired");

            assert!(removed.removed);
            assert_eq!(fixture.supervisor.retired(), vec![id.clone(), id]);
            assert!(!recorded(&fixture.root, "s-353"));
        }

        #[tokio::test]
        async fn removing_a_possibly_live_runtime_without_a_generation_is_refused() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let mut record = persist_record(&fixture.root, "s-354", Some("runtime-s-354"));
            record.runtime.service_id = None;
            record.runtime.generation = None;
            record.runtime.executable = None;
            assert!(
                fixture
                    .registry
                    .insert_unavailable_record(record, RuntimeState::Conflict, IDENTITY_MISMATCH)
                    .await
            );
            let session = SessionId("s-354".to_owned());

            let error = fixture
                .registry
                .remove(&session)
                .await
                .expect_err("nothing identifies the worker to retire");

            assert_eq!(error.code, "session_runtime_conflict");
            assert!(fixture.supervisor.retired().is_empty());
            assert!(recorded(&fixture.root, "s-354"));
            let runtime = runtime_of(&fixture.registry, "s-354").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
        }

        #[tokio::test]
        async fn removing_an_incompatible_session_retires_its_generation_first() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            persist_record(&fixture.root, "s-355", Some("runtime-s-355"));
            let fake = spawn_incompatible_worker(&runtime_root, "s-355");
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            assert_eq!(
                runtime_of(&fixture.registry, "s-355").await.state,
                RuntimeState::Incompatible
            );

            let removed = fixture
                .registry
                .remove(&SessionId("s-355".to_owned()))
                .await
                .expect("remove the incompatible session");

            assert!(removed.removed);
            assert_eq!(
                fixture.supervisor.retired(),
                vec![service_id("s-355", TEST_GENERATION)]
            );
            assert!(!recorded(&fixture.root, "s-355"));
            fake.abort();
        }

        #[tokio::test]
        async fn ambiguous_generation_whose_job_ends_becomes_a_proven_crash() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let marked = Marked::spawn("runtime-s-320");
            persist_record(&fixture.root, "s-320", Some("runtime-s-320"));
            write_live_journal(
                &fixture.root,
                "s-320",
                dead_worker_with_realistic_start(),
                Some("runtime-s-320"),
            );
            let id = service_id("s-320", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-320").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(marked.alive(), "nothing is swept while ambiguous");

            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));
            let runtime = wait_state(&fixture.registry, "s-320", RuntimeState::Lost).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            marked.wait_gone().await;
            assert_eq!(fixture.supervisor.retired(), vec![id]);
        }

        /// Counts the supervisor inspections of `id` so far.
        fn inspections(supervisor: &ScriptedSupervisor, id: &ServiceId) -> usize {
            supervisor
                .calls()
                .iter()
                .filter(|call| matches!(call, Call::Inspect(inspected) if inspected == id))
                .count()
        }

        /// Waits until `id` was inspected more than `seen` times.
        async fn wait_inspected_after(
            supervisor: &ScriptedSupervisor,
            id: &ServiceId,
            seen: usize,
        ) {
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while inspections(supervisor, id) <= seen {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "{id} was never inspected again"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        #[tokio::test]
        async fn panicking_retry_pass_keeps_its_session_pending_and_the_loop_serving() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            for session_id in ["s-321", "s-322"] {
                persist_record(&fixture.root, session_id, None);
                write_live_journal(&fixture.root, session_id, dead_worker(), None);
                fixture.supervisor.script_job(
                    service_id(session_id, TEST_GENERATION),
                    JobScript::Unavailable,
                );
            }
            fixture.supervisor.fail_discovery();
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            for session_id in ["s-321", "s-322"] {
                let runtime = runtime_of(&fixture.registry, session_id).await;
                assert_eq!(runtime.state, RuntimeState::Reconnecting);
                assert_eq!(
                    runtime.loss_reason.as_deref(),
                    Some(SUPERVISION_UNAVAILABLE)
                );
            }

            let broken = service_id("s-321", TEST_GENERATION);
            let healthy = service_id("s-322", TEST_GENERATION);
            let seen = inspections(&fixture.supervisor, &broken);
            fixture
                .supervisor
                .script_job(broken.clone(), JobScript::Panic);
            fixture
                .supervisor
                .script_job(healthy.clone(), present(ServiceState::Failed, None));

            let runtime = wait_state(&fixture.registry, "s-322", RuntimeState::Lost).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            wait_inspected_after(&fixture.supervisor, &broken, seen).await;
            let panicked = inspections(&fixture.supervisor, &broken);
            wait_inspected_after(&fixture.supervisor, &broken, panicked).await;
            let runtime = runtime_of(&fixture.registry, "s-321").await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(SUPERVISION_UNAVAILABLE)
            );

            fixture
                .supervisor
                .script_job(broken.clone(), present(ServiceState::Failed, None));
            let runtime = wait_state(&fixture.registry, "s-321", RuntimeState::Lost).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            assert_eq!(fixture.supervisor.retired(), vec![healthy, broken]);
        }

        #[tokio::test]
        async fn panicking_abandoned_create_settlement_is_handed_to_the_retry() {
            const INITIALIZE: Duration = Duration::from_secs(30);

            let fixture = fixture_with(Arc::new(RetryInspector::default()), Some(INITIALIZE));
            persist_preparing_record(&fixture.root, "s-323");
            let id = service_id("s-323", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-323").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));

            let seen = inspections(&fixture.supervisor, &id);
            fixture.supervisor.script_job(id.clone(), JobScript::Panic);
            wait_inspected_after(&fixture.supervisor, &id, seen).await;
            let panicked = inspections(&fixture.supervisor, &id);
            wait_inspected_after(&fixture.supervisor, &id, panicked).await;
            let runtime = runtime_of(&fixture.registry, "s-323").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(fixture.supervisor.retired().is_empty(), "nothing is killed");

            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));
            wait_removed(&fixture, "s-323").await;
            assert_eq!(fixture.supervisor.retired(), vec![id]);
            assert!(Store::new(fixture.root.join("data/metadata.jsonl"))
                .load_sessions()
                .expect("load store")
                .is_empty());
        }

        /// Sets the mode of `path`; a group- or world-accessible worker root
        /// fails the private-root check, so its scan or enumeration fails.
        fn set_mode(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("change worker root mode");
        }

        /// Mode that makes a private worker root fail validation.
        const SHARED_ROOT_MODE: u32 = 0o750;
        /// Mode of a valid private worker root.
        const PRIVATE_ROOT_MODE: u32 = 0o700;

        #[tokio::test]
        async fn failed_journal_scan_is_ambiguous_until_a_scan_succeeds() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let marked = Marked::spawn("runtime-s-330");
            persist_record(&fixture.root, "s-330", Some("runtime-s-330"));
            write_live_journal(
                &fixture.root,
                "s-330",
                dead_worker_with_realistic_start(),
                Some("runtime-s-330"),
            );
            let id = service_id("s-330", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));
            let state_root = fixture.root.join("state/workers");
            set_mode(&state_root, SHARED_ROOT_MODE);

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-330").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired"
            );
            assert!(marked.alive(), "nothing is swept");

            set_mode(&state_root, PRIVATE_ROOT_MODE);
            let runtime = wait_state(&fixture.registry, "s-330", RuntimeState::Lost).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            marked.wait_gone().await;
            assert_eq!(fixture.supervisor.retired(), vec![id]);
        }

        #[tokio::test]
        async fn failed_runtime_root_enumeration_is_ambiguous_until_it_succeeds() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let marked = Marked::spawn("runtime-s-331");
            persist_record(&fixture.root, "s-331", Some("runtime-s-331"));
            write_live_journal(
                &fixture.root,
                "s-331",
                dead_worker_with_realistic_start(),
                Some("runtime-s-331"),
            );
            let id = service_id("s-331", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Failed, None));
            let runtime_root = fixture.root.join("runtime/workers");
            set_mode(&runtime_root, SHARED_ROOT_MODE);

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-331").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired"
            );
            assert!(marked.alive(), "nothing is swept");

            set_mode(&runtime_root, PRIVATE_ROOT_MODE);
            let runtime = wait_state(&fixture.registry, "s-331", RuntimeState::Lost).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            marked.wait_gone().await;
            assert_eq!(fixture.supervisor.retired(), vec![id]);
        }

        #[tokio::test]
        async fn abandoned_create_is_not_settled_while_its_journals_cannot_be_scanned() {
            const INITIALIZE: Duration = Duration::from_millis(500);

            let fixture = fixture_with(Arc::new(RetryInspector::default()), Some(INITIALIZE));
            persist_preparing_record(&fixture.root, "s-332");
            let id = service_id("s-332", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));
            let state_root = fixture.root.join("state/workers");
            set_mode(&state_root, SHARED_ROOT_MODE);

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            tokio::time::sleep(INITIALIZE * 3).await;

            let runtime = runtime_of(&fixture.registry, "s-332").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(
                fixture.supervisor.retired().is_empty(),
                "a create whose journals are unknown is never settled"
            );

            set_mode(&state_root, PRIVATE_ROOT_MODE);
            wait_removed(&fixture, "s-332").await;
            assert_eq!(fixture.supervisor.retired(), vec![id]);
        }

        #[test]
        fn unreadable_session_journal_directory_is_a_conflict() {
            let root = temp_root();
            let state_root = root.join("workers");
            let session_dir = state_root.join("s-333");
            std::fs::create_dir_all(&session_dir).expect("create journal directory");
            set_mode(&state_root, PRIVATE_ROOT_MODE);
            set_mode(&session_dir, SHARED_ROOT_MODE);

            let journals = super::super::scan_worker_journals(&state_root).expect("scan journals");

            let scan = journals.get("s-333").expect("the session is reported");
            assert!(scan.conflict);
            assert!(scan.evidence.is_empty());
        }

        #[tokio::test]
        async fn loaded_job_without_a_process_is_decided_by_its_journaled_worker() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let marked = Marked::spawn("runtime-s-334");
            persist_record(&fixture.root, "s-334", Some("runtime-s-334"));
            write_live_journal(
                &fixture.root,
                "s-334",
                dead_worker_with_realistic_start(),
                Some("runtime-s-334"),
            );
            persist_record(&fixture.root, "s-335", None);
            write_live_journal(&fixture.root, "s-335", own_identity(), None);
            persist_record(&fixture.root, "s-336", None);
            let crashed = service_id("s-334", TEST_GENERATION);
            for session_id in ["s-334", "s-335", "s-336"] {
                fixture.supervisor.script_job(
                    service_id(session_id, TEST_GENERATION),
                    present(ServiceState::Unknown, None),
                );
            }

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-334").await;
            assert_eq!(runtime.state, RuntimeState::Lost);
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            marked.wait_gone().await;
            for session_id in ["s-335", "s-336"] {
                let runtime = runtime_of(&fixture.registry, session_id).await;
                assert_eq!(runtime.state, RuntimeState::Conflict, "{session_id}");
                assert_eq!(
                    runtime.loss_reason.as_deref(),
                    Some(SUPERVISION_AMBIGUOUS),
                    "{session_id}"
                );
            }
            assert_eq!(fixture.supervisor.retired(), vec![crashed]);
        }

        /// The initialization deadline is the behavior under test, so the clock
        /// is paused: only `advance` moves it, however slow the fsyncs of
        /// reconciliation are.
        #[tokio::test(start_paused = true)]
        async fn stale_job_without_a_process_is_retired_by_its_journal_or_its_deadline() {
            const CURRENT_GENERATION: &str = "zzzz3333";
            const INITIALIZE: Duration = Duration::from_millis(500);

            let fixture = fixture_with(Arc::new(RetryInspector::default()), Some(INITIALIZE));
            write_live_journal(&fixture.root, "s-337", dead_worker(), None);
            write_live_journal(&fixture.root, "s-339", own_identity(), None);
            let current = |session_id| service_id(session_id, CURRENT_GENERATION);
            for session_id in ["s-337", "s-338", "s-339"] {
                let mut record = identity_record();
                record.session_id = session_id.to_owned();
                record.info.id = SessionId(session_id.to_owned());
                record.recovery.as_mut().expect("recovery").session_id = session_id.to_owned();
                bind_test_generation(&mut record);
                record.runtime.generation = Some(CURRENT_GENERATION.to_owned());
                record.runtime.service_id = Some(current(session_id).to_string());
                Store::new(fixture.root.join("data/metadata.jsonl"))
                    .record_session(&record)
                    .expect("persist a record past the stale generation");
                fixture
                    .supervisor
                    .script_job(current(session_id), present(ServiceState::Failed, None));
            }
            let proven = service_id("s-337", TEST_GENERATION);
            let unjournaled = service_id("s-338", TEST_GENERATION);
            let running = service_id("s-339", TEST_GENERATION);
            for id in [&proven, &unjournaled, &running] {
                fixture
                    .supervisor
                    .script_job(id.clone(), present(ServiceState::Unknown, None));
            }
            let orphaned = |inventory: &protocol::RuntimeInventoryResult, id: &ServiceId| {
                inventory.entries.iter().any(|entry| {
                    entry.runtime_slot == id.as_str()
                        && entry.status == RuntimeInventoryStatus::Orphaned
                        && entry.reason.as_deref() == Some(STALE_GENERATION)
                })
            };

            let started = tokio::time::Instant::now();
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let retired = fixture.supervisor.retired();
            assert!(retired.contains(&proven), "{retired:?}");
            assert!(
                started.elapsed() < INITIALIZE,
                "the initialization deadline has not passed on the paused clock"
            );
            assert!(
                !retired.contains(&unjournaled),
                "an unjournaled stale job is kept until its initialization deadline"
            );
            let inventory = fixture.registry.runtime_inventory().await;
            assert!(orphaned(&inventory, &unjournaled), "{inventory:?}");
            assert!(orphaned(&inventory, &running), "{inventory:?}");

            tokio::time::advance(INITIALIZE).await;
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while !fixture.supervisor.retired().contains(&unjournaled) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the unjournaled stale job was never re-checked"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(
                started.elapsed() >= INITIALIZE,
                "retired only after its initialization deadline"
            );
            tokio::time::sleep(INITIALIZE).await;
            let inventory = fixture.registry.runtime_inventory().await;
            assert!(
                !orphaned(&inventory, &unjournaled),
                "the retired job left the inventory: {inventory:?}"
            );
            assert!(orphaned(&inventory, &running), "{inventory:?}");
            assert!(
                !fixture.supervisor.retired().contains(&running),
                "a stale job whose journaled worker runs is never retired"
            );
        }

        /// Each re-check fires one initialization deadline after the previous
        /// one, so the clock is paused: the orphan observed between two
        /// re-checks cannot be settled by a slow host.
        #[tokio::test(start_paused = true)]
        async fn failed_stale_job_retirement_keeps_the_orphan_and_retries() {
            const CURRENT_GENERATION: &str = "zzzz4444";
            const INITIALIZE: Duration = Duration::from_millis(500);

            let fixture = fixture_with(Arc::new(RetryInspector::default()), Some(INITIALIZE));
            let current = |session_id| service_id(session_id, CURRENT_GENERATION);
            for session_id in ["s-341", "s-342"] {
                let mut record = identity_record();
                record.session_id = session_id.to_owned();
                record.info.id = SessionId(session_id.to_owned());
                record.recovery.as_mut().expect("recovery").session_id = session_id.to_owned();
                bind_test_generation(&mut record);
                record.runtime.generation = Some(CURRENT_GENERATION.to_owned());
                record.runtime.service_id = Some(current(session_id).to_string());
                Store::new(fixture.root.join("data/metadata.jsonl"))
                    .record_session(&record)
                    .expect("persist a record past the stale generation");
            }
            // s-341's stale job is proven ended, so reconciliation retires it
            // immediately; s-342's is unproven and settles at its deadline.
            let ended = service_id("s-341", TEST_GENERATION);
            write_live_journal(&fixture.root, "s-341", dead_worker(), None);
            fixture
                .supervisor
                .script_job(ended.clone(), present(ServiceState::Failed, None));
            let unproven = service_id("s-342", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(unproven.clone(), present(ServiceState::Unknown, None));
            // One transient supervisor failure per retirement.
            fixture.supervisor.script_retire_unavailable(ended.clone());
            fixture
                .supervisor
                .script_retire_unavailable(unproven.clone());
            let orphaned = |inventory: &protocol::RuntimeInventoryResult, id: &ServiceId| {
                inventory.entries.iter().any(|entry| {
                    entry.runtime_slot == id.as_str()
                        && entry.status == RuntimeInventoryStatus::Orphaned
                        && entry.reason.as_deref() == Some(STALE_GENERATION)
                })
            };
            let retire_count = |fixture: &Fixture, id: &ServiceId| {
                fixture
                    .supervisor
                    .retired()
                    .into_iter()
                    .filter(|retired| retired == id)
                    .count()
            };

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let inventory = fixture.registry.runtime_inventory().await;
            assert!(orphaned(&inventory, &ended), "{inventory:?}");
            assert!(orphaned(&inventory, &unproven), "{inventory:?}");
            assert_eq!(
                retire_count(&fixture, &ended),
                1,
                "{:?}",
                fixture.supervisor.retired()
            );

            // The ended job's re-check retires it and drops its orphan.
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while orphaned(&fixture.registry.runtime_inventory().await, &ended) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the ended stale job's orphan was never settled"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(retire_count(&fixture, &ended), 2);

            // The unproven job's first re-check also fails to retire: its
            // orphan entry must stay, and a further re-check must follow.
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while retire_count(&fixture, &unproven) < 1 {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the unproven stale job was never re-checked"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let inventory = fixture.registry.runtime_inventory().await;
            assert!(
                orphaned(&inventory, &unproven),
                "a failed retirement keeps the orphan: {inventory:?}"
            );
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while orphaned(&fixture.registry.runtime_inventory().await, &unproven) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the unproven stale job was never retried after its failed retirement"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(retire_count(&fixture, &unproven), 2);
        }

        #[tokio::test]
        async fn ended_stale_job_is_kept_while_its_worker_is_not_proven_gone() {
            const INITIALIZE: Duration = Duration::from_millis(300);

            let fixture = fixture_with(Arc::new(RetryInspector::default()), Some(INITIALIZE));
            // s-392's stopped job journaled a worker that still runs (this
            // test process); s-393's failed job sits next to an unreadable
            // journal that could be its worker's.
            write_live_journal(&fixture.root, "s-392", own_identity(), None);
            let unreadable_dir = fixture.root.join("state/workers/s-393");
            std::fs::create_dir_all(&unreadable_dir).expect("create journal directory");
            std::fs::set_permissions(&unreadable_dir, std::fs::Permissions::from_mode(0o700))
                .expect("private journal directory");
            let unreadable_journal = unreadable_dir.join("worker-s-393.json");
            std::fs::write(&unreadable_journal, b"not json").expect("write unreadable journal");
            std::fs::set_permissions(&unreadable_journal, std::fs::Permissions::from_mode(0o600))
                .expect("private unreadable journal");
            let running = service_id("s-392", TEST_GENERATION);
            let unreadable = service_id("s-393", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(running.clone(), present(ServiceState::Stopped, None));
            fixture
                .supervisor
                .script_job(unreadable.clone(), present(ServiceState::Failed, None));
            let orphaned = |inventory: &protocol::RuntimeInventoryResult, id: &ServiceId| {
                inventory.entries.iter().any(|entry| {
                    entry.runtime_slot == id.as_str()
                        && entry.status == RuntimeInventoryStatus::Orphaned
                        && entry.reason.as_deref() == Some(STALE_GENERATION)
                })
            };

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let inventory = fixture.registry.runtime_inventory().await;
            assert!(orphaned(&inventory, &running), "{inventory:?}");
            assert!(orphaned(&inventory, &unreadable), "{inventory:?}");
            assert!(
                fixture.supervisor.retired().is_empty(),
                "an ended job is never retired while its worker may run"
            );

            // The unreadable journal's re-check after the initialization
            // deadline still proves nothing, so nothing is retired then either.
            tokio::time::sleep(INITIALIZE * 3).await;
            let inventory = fixture.registry.runtime_inventory().await;
            assert!(orphaned(&inventory, &running), "{inventory:?}");
            assert!(orphaned(&inventory, &unreadable), "{inventory:?}");
            assert!(
                fixture.supervisor.retired().is_empty(),
                "{:?}",
                fixture.supervisor.retired()
            );
        }

        /// A backend that discovers exactly one job, finds it absent on
        /// inspection, and retires it.
        #[derive(Debug)]
        struct OneStaleJob(ServiceId);

        impl Supervisor for OneStaleJob {
            fn start<'a>(
                &'a self,
                _id: &'a ServiceId,
                _definition: &'a JobDefinition,
            ) -> Operation<'a, ()> {
                Box::pin(async { Ok(()) })
            }

            fn discover(&self) -> Operation<'_, Vec<ServiceObservation>> {
                Box::pin(async move {
                    Ok(vec![ServiceObservation {
                        id: self.0.clone(),
                        state: ServiceState::Unknown,
                        process: None,
                        definition: None,
                    }])
                })
            }

            fn inspect<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ServiceObservation> {
                Box::pin(async move { Err(SupervisorError::NotFound(id.clone())) })
            }

            fn retire<'a>(&'a self, _id: &'a ServiceId) -> Operation<'a, ()> {
                Box::pin(async { Ok(()) })
            }
        }

        #[tokio::test]
        async fn uninspectable_stale_job_stays_orphaned_and_is_retired_by_a_re_check() {
            const INITIALIZE: Duration = Duration::from_millis(300);

            let stale = service_id("s-343", TEST_GENERATION);
            let supervisor = ScriptedSupervisor::over(OneStaleJob(stale.clone()));
            // The startup inspection and the first re-check fail; the second
            // re-check finds the job gone.
            supervisor.script_inspects([
                InspectStep::Unavailable,
                InspectStep::Unavailable,
                InspectStep::NotFound,
            ]);
            let fixture = fixture_over(
                Arc::new(RetryInspector::default()),
                Some(INITIALIZE),
                supervisor,
            );
            let orphaned = |inventory: &protocol::RuntimeInventoryResult| {
                inventory.entries.iter().any(|entry| {
                    entry.runtime_slot == stale.as_str()
                        && entry.status == RuntimeInventoryStatus::Orphaned
                        && entry.reason.as_deref() == Some(STALE_GENERATION)
                })
            };

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let inventory = fixture.registry.runtime_inventory().await;
            assert!(
                orphaned(&inventory),
                "an uninspectable stale job stays inventoried: {inventory:?}"
            );
            assert!(fixture.supervisor.retired().is_empty());

            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while !fixture.supervisor.retired().contains(&stale) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the uninspectable stale job was never re-checked to its end"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(
                inspections(&fixture.supervisor, &stale),
                3,
                "retired only once an inspection proved it ended"
            );
            assert_eq!(fixture.supervisor.retired(), vec![stale.clone()]);
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while orphaned(&fixture.registry.runtime_inventory().await) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the retired job never left the inventory"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        /// Binds `slot`'s worker socket, accepts every connection, and never
        /// answers, standing in for a worker that hangs mid-negotiation.
        fn spawn_silent_worker(runtime_root: &Path, slot: &str) -> tokio::task::JoinHandle<()> {
            let directory = runtime_root.join(slot);
            std::fs::create_dir_all(&directory).expect("create silent runtime directory");
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .expect("private runtime directory");
            let socket = directory.join(pohunek_paths::WORKER_SOCKET_NAME);
            let listener = tokio::net::UnixListener::bind(&socket).expect("bind silent endpoint");
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                .expect("private silent socket");
            tokio::spawn(async move {
                let mut held = Vec::new();
                while let Ok((stream, _)) = listener.accept().await {
                    held.push(stream);
                }
            })
        }

        #[tokio::test]
        async fn unresponsive_worker_socket_is_unknown_evidence_within_the_connect_deadline() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let record = persist_record(&fixture.root, "s-361", Some("runtime-s-361"));
            let id = service_id("s-361", TEST_GENERATION);
            fixture
                .supervisor
                .script_job(id.clone(), present(ServiceState::Running, None));
            let silent = spawn_silent_worker(&runtime_root, "s-361");

            tokio::time::timeout(HANG_GUARD, Box::pin(fixture.registry.reconcile_workers()))
                .await
                .expect("startup reconciliation is bounded by the connect deadline")
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, "s-361").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));

            // The supervision retry runs both passes with the lock held.
            let session = SessionId("s-361".to_owned());
            let guard = fixture.registry.lock_lifecycle(&session).await;
            let pending = tokio::time::timeout(
                HANG_GUARD,
                fixture.registry.reconcile_single_session(record.clone()),
            )
            .await
            .expect("a retry pass is bounded by the connect deadline");
            assert!(pending, "unknown socket evidence keeps the session pending");
            let settled = tokio::time::timeout(
                HANG_GUARD,
                fixture.registry.retry_terminal_retirement(&record),
            )
            .await
            .expect("a retirement retry is bounded by the connect deadline");
            assert!(
                !settled,
                "unknown socket evidence keeps the retirement pending"
            );
            drop(guard);

            let runtime = runtime_of(&fixture.registry, "s-361").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired on unknown evidence"
            );
            silent.abort();
        }

        #[tokio::test]
        async fn reconciliation_waits_for_the_session_lifecycle_lock() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            persist_record(&fixture.root, "s-314", None);
            let guard = fixture
                .registry
                .lock_lifecycle(&SessionId("s-314".to_owned()))
                .await;
            let registry = fixture.registry.clone();
            let reconciling =
                tokio::spawn(async move { Box::pin(registry.reconcile_workers()).await });
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                !reconciling.is_finished(),
                "reconciliation ignored the lock"
            );
            drop(guard);
            reconciling
                .await
                .expect("reconciliation task")
                .expect("reconcile");
            assert_eq!(
                runtime_of(&fixture.registry, "s-314").await.state,
                RuntimeState::Lost
            );
        }

        /// Asserts that `session` logged exactly one WARN, and that it names
        /// the session, its worker and `reason`.
        fn assert_one_classification_warn(
            logs: &crate::runtime::lifecycle::tests::LogCapture,
            session: &str,
            reason: &str,
        ) {
            let warns = warn_lines(logs, session);
            assert_eq!(
                warns.len(),
                1,
                "{session}: exactly one WARN per classification: {warns:#?}"
            );
            let worker = format!("worker_id=worker-{session} ");
            assert!(
                warns[0].contains(&worker) && warns[0].contains(&format!("reason={reason}")),
                "{session}: the WARN names its worker and reason {reason}: {}",
                warns[0]
            );
        }

        #[tokio::test]
        async fn every_startup_classification_logs_one_warn_with_session_worker_and_reason() {
            let logs = crate::runtime::lifecycle::tests::LogCapture::default();
            let _subscriber = logs.install();
            let cases = [
                ("s-5221", RuntimeState::Conflict, SUPERVISION_AMBIGUOUS),
                ("s-5222", RuntimeState::Lost, RUNTIME_LOST),
                (
                    "s-5223",
                    RuntimeState::Reconnecting,
                    SUPERVISION_UNAVAILABLE,
                ),
                ("s-5224", RuntimeState::Conflict, IDENTITY_MISMATCH),
            ];
            for (session, state, reason) in cases {
                let fixture = fixture(Arc::new(RetryInspector::default()));
                let instance = format!("runtime-{session}");
                persist_record(&fixture.root, session, Some(&instance));
                let id = service_id(session, TEST_GENERATION);
                match session {
                    "s-5221" => {
                        write_live_journal(&fixture.root, session, dead_worker(), Some(&instance));
                        fixture
                            .supervisor
                            .script_job(id, present(ServiceState::Running, None));
                    }
                    "s-5222" => {
                        write_live_journal(
                            &fixture.root,
                            session,
                            dead_worker_with_realistic_start(),
                            Some(&instance),
                        );
                        fixture
                            .supervisor
                            .script_job(id, present(ServiceState::Failed, None));
                    }
                    "s-5223" => {
                        write_live_journal(&fixture.root, session, dead_worker(), Some(&instance));
                        fixture.supervisor.script_job(id, JobScript::Unavailable);
                    }
                    _ => {
                        write_worker_journal(
                            &fixture.root,
                            session,
                            "worker-another",
                            TEST_GENERATION,
                            dead_worker(),
                            Some(&instance),
                        );
                    }
                }

                Box::pin(fixture.registry.reconcile_workers())
                    .await
                    .expect("reconcile");

                let runtime = runtime_of(&fixture.registry, session).await;
                assert_eq!(runtime.state, state, "{session}");
                assert_eq!(runtime.loss_reason.as_deref(), Some(reason), "{session}");
                assert_one_classification_warn(&logs, session, reason);
            }
        }

        /// A worker that answers but cannot be adopted leaves its session
        /// `conflict`; the session is watched and becomes `lost` once the
        /// worker is gone and its job has ended.
        #[cfg(target_os = "linux")]
        #[tokio::test]
        async fn a_conflicted_worker_that_dies_becomes_lost_without_a_restart() {
            use super::previous_release::{
                spawn_launch_worker, write_previous_store, Previous, STORED_REFERENCE,
            };

            let logs = crate::runtime::lifecycle::tests::LogCapture::default();
            let _subscriber = logs.install();
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let session = "s-5225";
            let worker_id = "worker-s-5225";
            let runtime_root = fixture.root.join("runtime/workers");
            let worker = spawn_launch_worker(
                &fixture.root,
                &runtime_root,
                session,
                worker_id,
                "native-contradicts-the-stored-one",
            )
            .await;
            let previous = Previous {
                generation: "v0.33.1-damaged",
                fixture: super::V0_33_1_DAMAGED_STORE,
                agent: "claude",
                profiles: &[],
            };
            write_previous_store(
                &fixture.root,
                &previous,
                session,
                worker_id,
                worker.worker_instance_id.as_str(),
                worker.child_pid,
            );
            let job = service_id(session, TEST_GENERATION);
            fixture
                .supervisor
                .script_job(job.clone(), present(ServiceState::Running, None));
            drop(worker.controller);

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = runtime_of(&fixture.registry, session).await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some("launch_identity_reference_mismatch")
            );
            let conflict_warns = |logs: &crate::runtime::lifecycle::tests::LogCapture| {
                super::warn_lines(logs, session)
                    .into_iter()
                    .filter(|line| line.contains("reason=launch_identity_reference_mismatch"))
                    .count()
            };
            assert_eq!(conflict_warns(&logs), 1, "{}", logs.text());

            // The worker answers and the evidence is unchanged: the watch
            // keeps the conflict without announcing it again.
            let mut events = fixture.registry.subscribe();
            assert!(
                !fixture
                    .registry
                    .retry_supervised_session(&SessionId(session.to_owned()))
                    .await,
                "a conflicted session whose worker answers stays watched"
            );
            assert!(events.try_recv().is_err(), "nothing is announced again");
            assert_eq!(conflict_warns(&logs), 1, "{}", logs.text());

            // The worker crashes: nothing answers on its socket any more, its
            // journal names a process that no longer runs, and its job has
            // ended. The loss sweep then takes down the PTY tree the stand-in
            // worker leaves behind.
            worker.server_task.abort();
            let _ = worker.server_task.await;
            let journal = Journal::new(
                fixture
                    .root
                    .join("state/workers")
                    .join(session)
                    .join(format!("{worker_id}.json")),
            );
            let mut record = journal.load().expect("load the worker journal");
            let (pid, start) = dead_worker_with_realistic_start();
            record.worker_pid = pid;
            record.worker_start_identity = start.to_string();
            journal.write(&record).expect("rewrite the worker journal");
            fixture
                .supervisor
                .script_job(job, present(ServiceState::Failed, None));

            let runtime = wait_state(&fixture.registry, session, RuntimeState::Lost).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            // The conflicting worker's reference never replaces the stored one.
            let session_info = fixture
                .registry
                .inspect(&SessionId(session.to_owned()))
                .await
                .expect("the lost session stays listed");
            assert_eq!(
                session_info.native_session_id.as_deref(),
                Some(STORED_REFERENCE)
            );
            assert_eq!(
                durable_record(&fixture.root, session)
                    .recovery
                    .and_then(|binding| binding.native_session_id)
                    .as_deref(),
                Some(STORED_REFERENCE)
            );
        }

        /// The durable record of `session_id`.
        fn durable_record(root: &Path, session_id: &str) -> SessionRecord {
            Store::new(root.join("data/metadata.jsonl"))
                .load_sessions()
                .expect("load store")
                .into_iter()
                .find(|record| record.session_id == session_id)
                .expect("the session is recorded")
        }

        /// Process identity of `pid` as the host reports it.
        fn host_identity(pid: u32) -> crate::procwatch::ProcessIdentity {
            HostInspector::new()
                .identity(pid)
                .expect("inspect the stand-in worker")
                .expect("the stand-in worker runs")
        }

        /// A session left `conflict` over a worker that is alive and
        /// journal-proven: the stand-in worker process is the recorded
        /// worker's, the supervisor shows its job with that process, and
        /// retiring the job ends it.
        struct ConflictedSession {
            worker: Marked,
            job: ServiceId,
            id: SessionId,
        }

        async fn conflicted_session(fixture: &Fixture, session: &str) -> ConflictedSession {
            conflicted_session_with(fixture, session, true).await
        }

        /// A [`conflicted_session`] whose job the supervisor shows only when
        /// `supervised`.
        async fn conflicted_session_with(
            fixture: &Fixture,
            session: &str,
            supervised: bool,
        ) -> ConflictedSession {
            conflicted_session_recording(fixture, session, supervised, None).await
        }

        /// A [`conflicted_session_with`] whose record names the runtime
        /// `recorded` instead of the journaled one.
        async fn conflicted_session_recording(
            fixture: &Fixture,
            session: &str,
            supervised: bool,
            recorded: Option<&str>,
        ) -> ConflictedSession {
            let instance = format!("runtime-{session}");
            let worker = Marked::spawn(&instance);
            let identity = host_identity(worker.0);
            let record =
                persist_record(&fixture.root, session, Some(recorded.unwrap_or(&instance)));
            write_live_journal(
                &fixture.root,
                session,
                (identity.pid, identity.start_identity.get()),
                Some(&instance),
            );
            let job = service_id(session, TEST_GENERATION);
            if supervised {
                fixture.supervisor.script_job(
                    job.clone(),
                    JobScript::Present {
                        state: ServiceState::Running,
                        process: Some(identity),
                        definition: None,
                    },
                );
                fixture.supervisor.script_retire_ends(job.clone(), worker.0);
            }
            assert!(
                fixture
                    .registry
                    .insert_unavailable_record(
                        record,
                        RuntimeState::Conflict,
                        "launch_identity_reference_mismatch",
                    )
                    .await
            );
            ConflictedSession {
                worker,
                job,
                id: SessionId(session.to_owned()),
            }
        }

        /// What the store holds of the session's stop intent and runtime.
        fn stop_intent_of(root: &Path, session: &str) -> (DesiredState, bool, RuntimeState) {
            let record = durable_record(root, session);
            (
                record.desired_state,
                record.transaction.is_some(),
                record.runtime.state,
            )
        }

        #[tokio::test]
        async fn stop_of_a_conflicted_runtime_stops_its_job_by_the_recorded_identity() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicted = conflicted_session(&fixture, "s-5226").await;
            assert_eq!(
                runtime_of(&fixture.registry, "s-5226").await.state,
                RuntimeState::Conflict
            );

            let stopped = fixture
                .registry
                .stop(&conflicted.id)
                .await
                .expect("a journal-proven conflicted runtime is stopped");

            assert!(stopped.stopped);
            let session = fixture
                .registry
                .inspect(&conflicted.id)
                .await
                .expect("the session stays listed");
            assert_eq!(session.state, protocol::SessionState::Stopped);
            assert_eq!(
                session.runtime.expect("runtime").state,
                RuntimeState::Terminal
            );
            assert_eq!(fixture.supervisor.retired(), vec![conflicted.job.clone()]);
            conflicted.worker.wait_gone().await;
            let durable = durable_record(&fixture.root, "s-5226");
            assert_eq!(durable.desired_state, DesiredState::Stopped);
            assert_eq!(durable.info.state, protocol::SessionState::Stopped);
            assert_eq!(durable.runtime.state, RuntimeState::Terminal);
            assert!(
                durable.transaction.is_none(),
                "the stop transaction is committed: {:?}",
                durable.transaction
            );
            assert!(
                !fixture
                    .registry
                    .stop(&conflicted.id)
                    .await
                    .expect("a second stop is idempotent")
                    .stopped
            );
            let removed = fixture
                .registry
                .remove(&conflicted.id)
                .await
                .expect("a stopped session is removable");
            assert!(removed.removed);
        }

        #[tokio::test]
        async fn a_failed_stop_of_a_conflicted_runtime_leaves_no_durable_intent() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicted = conflicted_session(&fixture, "s-5227").await;
            let before = stop_intent_of(&fixture.root, "s-5227");
            assert_eq!(
                before,
                (DesiredState::Running, false, RuntimeState::Conflict)
            );
            fixture
                .supervisor
                .script_retire_unavailable(conflicted.job.clone());

            let error = fixture
                .registry
                .stop(&conflicted.id)
                .await
                .expect_err("the supervisor cannot retire the job");

            assert_eq!(error.code, SUPERVISION_UNAVAILABLE, "{error:?}");
            assert_eq!(
                stop_intent_of(&fixture.root, "s-5227"),
                before,
                "the failed stop leaves the record as it was"
            );
            let session = fixture
                .registry
                .inspect(&conflicted.id)
                .await
                .expect("the session stays listed");
            assert_eq!(session.state, protocol::SessionState::Running);
            assert_eq!(
                session.runtime.expect("runtime").state,
                RuntimeState::Conflict
            );
            assert!(conflicted.worker.alive(), "nothing was stopped");

            // The rollback leaves the session stoppable: the next stop retires.
            fixture
                .registry
                .stop(&conflicted.id)
                .await
                .expect("the retried stop succeeds");
            conflicted.worker.wait_gone().await;
        }

        /// A [`conflicted_session`] whose stop was interrupted after its job
        /// was retired: the durable record holds the stop intent, the job is
        /// gone, and the worker is dead.
        async fn interrupted_stop(fixture: &Fixture, session: &str) -> ConflictedSession {
            let conflicted = conflicted_session(fixture, session).await;
            let mut record = durable_record(&fixture.root, session);
            record.desired_state = DesiredState::Stopped;
            record.transaction = Some(crate::store::SessionTransaction {
                id: format!("stop-{session}"),
                kind: crate::store::TransactionKind::Stop,
                phase: "requested".to_owned(),
                previous_worker_id: None,
                previous_worker_instance_id: None,
                daemon_instance_id: None,
            });
            Store::new(fixture.root.join("data/metadata.jsonl"))
                .record_session(&record)
                .expect("persist the stop intent");
            fixture
                .supervisor
                .retire(&conflicted.job)
                .await
                .expect("retire the job of the interrupted stop");
            conflicted.worker.wait_gone().await;
            conflicted
        }

        /// Asserts the session ended as a committed stop, in memory and durably.
        async fn assert_stopped(fixture: &Fixture, session: &str) {
            let id = SessionId(session.to_owned());
            let listed = fixture
                .registry
                .inspect(&id)
                .await
                .expect("the session stays listed");
            assert_eq!(listed.state, protocol::SessionState::Stopped);
            assert_eq!(
                listed.runtime.expect("runtime").state,
                RuntimeState::Terminal
            );
            let durable = durable_record(&fixture.root, session);
            assert_eq!(durable.desired_state, DesiredState::Stopped);
            assert_eq!(durable.info.state, protocol::SessionState::Stopped);
            assert_eq!(durable.runtime.state, RuntimeState::Terminal);
            assert!(
                durable.transaction.is_none(),
                "the stop transaction is committed: {:?}",
                durable.transaction
            );
        }

        /// The job retirement of a stop kills the worker without a terminal
        /// journal, so a completed stop must survive reconciliation.
        #[tokio::test]
        async fn a_completed_stop_of_a_conflicted_runtime_survives_a_restart() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicted = conflicted_session(&fixture, "s-5230").await;
            fixture
                .registry
                .stop(&conflicted.id)
                .await
                .expect("the conflicted runtime is stopped");
            conflicted.worker.wait_gone().await;
            assert_stopped(&fixture, "s-5230").await;

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile after the restart");

            assert_stopped(&fixture, "s-5230").await;
            assert!(
                !fixture
                    .registry
                    .stop(&conflicted.id)
                    .await
                    .expect("a second stop is idempotent")
                    .stopped
            );
        }

        #[tokio::test]
        async fn an_interrupted_stop_of_a_conflicted_runtime_is_finished_after_a_restart() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicted = interrupted_stop(&fixture, "s-5231").await;

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile after the restart");

            assert_stopped(&fixture, "s-5231").await;
            assert!(
                !fixture
                    .registry
                    .stop(&conflicted.id)
                    .await
                    .expect("a further stop is idempotent")
                    .stopped
            );
        }

        /// A stop interrupted after its job was retired keeps the conversation
        /// its dead worker journaled: the startup replay reads that journal.
        #[tokio::test]
        async fn a_stop_replayed_at_startup_keeps_the_switch_the_dead_worker_journaled() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let session = "s-5241";
            interrupted_stop(&fixture, session).await;
            let mut record = durable_record(&fixture.root, session);
            let instance = record
                .runtime
                .worker_instance_id
                .clone()
                .expect("the record names its worker instance");
            record.info.native_session_id = Some("assigned-ref".to_owned());
            let binding = record.recovery.as_mut().expect("recovery binding");
            binding.native_session_id = Some("assigned-ref".to_owned());
            binding.native_reference_provenance = crate::agent::NativeReferenceProvenance::Assigned;
            binding.native_launch = Some(
                crate::agent::NativeSessionLaunch::from_templates(
                    crate::agent::SessionRefKind::Id,
                    &["--session", "{reference}"],
                    None,
                )
                .expect("resume template")
                .with_assigned(crate::agent::AssignedReference::new(
                    crate::agent::NativeArgs::from_template(&["--session-id", "{reference}"])
                        .expect("launch template"),
                    crate::agent::ReferenceExistence::Unchecked,
                ))
                .expect("an id spec accepts an assignment"),
            );
            Store::new(fixture.root.join("data/metadata.jsonl"))
                .record_session(&record)
                .expect("persist the assigned record");
            let existing: serde_json::Value = serde_json::from_slice(
                &std::fs::read(
                    fixture
                        .root
                        .join("state/workers")
                        .join(session)
                        .join(format!("worker-{session}.json")),
                )
                .expect("read the worker journal"),
            )
            .expect("decode the worker journal");
            let worker = (
                u32::try_from(existing["worker_pid"].as_u64().expect("worker pid"))
                    .expect("pid fits"),
                existing["worker_start_identity"]
                    .as_str()
                    .expect("worker start identity")
                    .parse::<u64>()
                    .expect("start identity number"),
            );
            let provider = crate::session::agent_kind_label(&record.info.agent_base).to_owned();
            let process = pohunek_session_worker::ChildIdentity {
                pid: 50,
                process_group: 50,
                start_identity: "500".to_owned(),
            };
            write_journal_record(
                &fixture.root,
                session,
                &format!("worker-{session}"),
                TEST_GENERATION,
                worker,
                Some(&instance),
                |journal| {
                    journal.child = Some(process.clone());
                    journal.launch_identity = Some(pohunek_session_worker::LaunchIdentity {
                        provider: provider.clone(),
                        process: process.clone(),
                        reference_kind: "id".to_owned(),
                        native_reference: "assigned-ref".to_owned(),
                    });
                    journal.native_reference_claim =
                        Some(pohunek_session_worker::NativeReferenceClaim {
                            provider: provider.clone(),
                            process,
                            sequence: 7,
                            reference_kind: "id".to_owned(),
                            native_reference: "switched-before-the-stop".to_owned(),
                        });
                },
            );
            // The restart starts from an empty registry.
            fixture.registry.inner.sessions.lock().await.clear();

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile after the restart");

            assert_stopped(&fixture, session).await;
            let durable = durable_record(&fixture.root, session);
            assert_eq!(
                durable.info.native_session_id.as_deref(),
                Some("switched-before-the-stop")
            );
        }

        /// A stop interrupted after its job was retired keeps the conversation
        /// a previous-release worker journaled: it keeps a switch only as its
        /// leased active identity claim, with no separate native reference,
        /// so the startup replay reads that claim.
        #[tokio::test]
        #[expect(
            clippy::too_many_lines,
            reason = "the legacy restart scenario constructs and verifies durable worker evidence"
        )]
        async fn a_stop_replayed_at_startup_keeps_the_switch_the_legacy_worker_journaled() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let session = "s-5242";
            interrupted_stop(&fixture, session).await;
            let mut record = durable_record(&fixture.root, session);
            let instance = record
                .runtime
                .worker_instance_id
                .clone()
                .expect("the record names its worker instance");
            record.info.native_session_id = Some("assigned-ref".to_owned());
            let binding = record.recovery.as_mut().expect("recovery binding");
            binding.native_session_id = Some("assigned-ref".to_owned());
            binding.native_reference_provenance = crate::agent::NativeReferenceProvenance::Assigned;
            binding.native_launch = Some(
                crate::agent::NativeSessionLaunch::from_templates(
                    crate::agent::SessionRefKind::Id,
                    &["--session", "{reference}"],
                    None,
                )
                .expect("resume template")
                .with_assigned(crate::agent::AssignedReference::new(
                    crate::agent::NativeArgs::from_template(&["--session-id", "{reference}"])
                        .expect("launch template"),
                    crate::agent::ReferenceExistence::Unchecked,
                ))
                .expect("an id spec accepts an assignment"),
            );
            Store::new(fixture.root.join("data/metadata.jsonl"))
                .record_session(&record)
                .expect("persist the assigned record");
            let existing: serde_json::Value = serde_json::from_slice(
                &std::fs::read(
                    fixture
                        .root
                        .join("state/workers")
                        .join(session)
                        .join(format!("worker-{session}.json")),
                )
                .expect("read the worker journal"),
            )
            .expect("decode the worker journal");
            let worker = (
                u32::try_from(existing["worker_pid"].as_u64().expect("worker pid"))
                    .expect("pid fits"),
                existing["worker_start_identity"]
                    .as_str()
                    .expect("worker start identity")
                    .parse::<u64>()
                    .expect("start identity number"),
            );
            let provider = crate::session::agent_kind_label(&record.info.agent_base).to_owned();
            let process = pohunek_session_worker::ChildIdentity {
                pid: 50,
                process_group: 50,
                start_identity: "500".to_owned(),
            };
            let expires_at = (OffsetDateTime::now_utc() + std::time::Duration::from_secs(30))
                .format(&Rfc3339)
                .expect("format identity expiry");
            write_journal_record(
                &fixture.root,
                session,
                &format!("worker-{session}"),
                TEST_GENERATION,
                worker,
                Some(&instance),
                |journal| {
                    journal.child = Some(process.clone());
                    journal.launch_identity = Some(pohunek_session_worker::LaunchIdentity {
                        provider: provider.clone(),
                        process: process.clone(),
                        reference_kind: "id".to_owned(),
                        native_reference: "assigned-ref".to_owned(),
                    });
                    // A v0.33.1 worker journals a conversation switch only as
                    // the active identity claim, never as a separate native
                    // reference, so the durable claim is the switch.
                    journal.active_identity = Some(pohunek_session_worker::ActiveIdentity {
                        provider,
                        process,
                        sequence: 7,
                        expires_at,
                        reference_kind: Some("id".to_owned()),
                        native_reference: Some("switched-before-the-stop".to_owned()),
                    });
                },
            );
            // The restart starts from an empty registry.
            fixture.registry.inner.sessions.lock().await.clear();

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile after the restart");

            assert_stopped(&fixture, session).await;
            let durable = durable_record(&fixture.root, session);
            assert_eq!(
                durable.info.native_session_id.as_deref(),
                Some("switched-before-the-stop")
            );
            assert_eq!(
                durable
                    .recovery
                    .as_ref()
                    .and_then(|binding| binding.native_session_id.as_deref()),
                Some("switched-before-the-stop")
            );
        }

        /// A store write that fails while a settled stop is committed keeps
        /// the session pending: its stop intent and conflicted runtime stay
        /// unresolved until a retry commits them.
        #[tokio::test]
        async fn a_failed_commit_of_a_settled_stop_keeps_the_retry_pending() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicted = interrupted_stop(&fixture, "s-5234").await;
            fixture
                .registry
                .inner
                .store
                .as_ref()
                .expect("registry store")
                .fail_next_write_before_rename();

            assert!(
                !fixture
                    .registry
                    .retry_supervised_session(&conflicted.id)
                    .await,
                "an uncommitted stop keeps the session pending"
            );
            assert_eq!(
                runtime_of(&fixture.registry, "s-5234").await.state,
                RuntimeState::Conflict
            );
            let durable = durable_record(&fixture.root, "s-5234");
            assert_eq!(durable.desired_state, DesiredState::Stopped);
            assert!(durable.transaction.is_some(), "the stop intent is kept");

            assert!(
                fixture
                    .registry
                    .retry_supervised_session(&conflicted.id)
                    .await,
                "the next pass commits the stop"
            );
            assert_stopped(&fixture, "s-5234").await;
        }

        /// At startup a failed commit leaves the session listed and pending,
        /// never absent from the registry.
        #[tokio::test]
        async fn a_failed_startup_commit_of_a_settled_stop_lists_the_session_as_pending() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicted = interrupted_stop(&fixture, "s-5235").await;
            // The restart starts from an empty registry.
            fixture.registry.inner.sessions.lock().await.clear();
            fixture
                .registry
                .inner
                .store
                .as_ref()
                .expect("registry store")
                .fail_next_write_before_rename();

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile after the restart");

            let runtime = runtime_of(&fixture.registry, "s-5235").await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some(SUPERVISION_UNAVAILABLE)
            );
            assert!(
                durable_record(&fixture.root, "s-5235")
                    .transaction
                    .is_some(),
                "the stop intent is kept"
            );
            assert!(
                fixture
                    .registry
                    .retry_supervised_session(&conflicted.id)
                    .await,
                "the retry commits the stop"
            );
            assert_stopped(&fixture, "s-5235").await;
        }

        /// A sweep that cannot confirm its processes after the job was
        /// retired keeps the stop intent and its retry, never rolls back.
        #[tokio::test]
        async fn a_sweep_failure_after_the_job_was_retired_keeps_the_stop_intent() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicted = conflicted_session(&fixture, "s-5236").await;
            let conflicting = Marked::spawn_with(&[
                ("POHUNEK_WORKER_INSTANCE_ID", "runtime-s-5236"),
                ("POHUNEK_RUNTIME_ID", "runtime-s-5236-other"),
            ]);

            let error = fixture
                .registry
                .stop(&conflicted.id)
                .await
                .expect_err("the sweep cannot confirm the marked process");

            assert_eq!(error.code, SUPERVISION_AMBIGUOUS, "{error:?}");
            conflicted.worker.wait_gone().await;
            let durable = durable_record(&fixture.root, "s-5236");
            assert_eq!(durable.desired_state, DesiredState::Stopped);
            assert!(durable.transaction.is_some(), "the stop intent is kept");

            assert!(
                !fixture
                    .registry
                    .retry_supervised_session(&conflicted.id)
                    .await,
                "the cleanup is retried while it is unconfirmed"
            );
            assert_eq!(
                durable_record(&fixture.root, "s-5236").info.state,
                protocol::SessionState::Running,
                "the stop is not committed"
            );

            drop(conflicting);
            assert!(
                fixture
                    .registry
                    .retry_supervised_session(&conflicted.id)
                    .await,
                "the confirmed cleanup finishes the stop"
            );
            assert_stopped(&fixture, "s-5236").await;
        }

        /// A record naming a runtime the journal does not is refused before
        /// anything is written, and the runtime it names is never swept.
        #[tokio::test]
        async fn a_stop_never_sweeps_a_runtime_the_journal_does_not_name() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let victim = Marked::spawn("runtime-s-5260-other-session");
            let conflicted = conflicted_session_recording(
                &fixture,
                "s-5260",
                true,
                Some("runtime-s-5260-other-session"),
            )
            .await;
            let before = stop_intent_of(&fixture.root, "s-5260");

            let error = fixture
                .registry
                .stop(&conflicted.id)
                .await
                .expect_err("the recorded runtime is not the journaled one");

            assert_eq!(error.code, IDENTITY_MISMATCH, "{error:?}");
            assert_eq!(stop_intent_of(&fixture.root, "s-5260"), before);
            assert!(fixture.supervisor.retired().is_empty());
            assert!(conflicted.worker.alive(), "nothing was stopped");
            assert!(victim.alive(), "the other runtime's process is untouched");
        }

        /// A retirement that stopped the worker before it failed is a partial
        /// stop: the intent is kept and the retry finishes it.
        #[tokio::test]
        async fn a_retirement_that_stops_the_worker_and_then_fails_keeps_the_stop_intent() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicted = conflicted_session(&fixture, "s-5261").await;
            fixture
                .supervisor
                .script_retire_ends_then_fails(conflicted.job.clone(), conflicted.worker.0);

            let error = fixture
                .registry
                .stop(&conflicted.id)
                .await
                .expect_err("the supervisor reports the retirement as failed");

            assert_eq!(error.code, SUPERVISION_UNAVAILABLE, "{error:?}");
            conflicted.worker.wait_gone().await;
            let durable = durable_record(&fixture.root, "s-5261");
            assert_eq!(durable.desired_state, DesiredState::Stopped);
            assert!(durable.transaction.is_some(), "the stop intent is kept");

            assert!(
                fixture
                    .registry
                    .retry_supervised_session(&conflicted.id)
                    .await,
                "the retry finishes the stop"
            );
            assert_stopped(&fixture, "s-5261").await;
        }

        /// Quarantines `session` over a resume binding that contradicts its
        /// record, both in the registry and in the store.
        async fn quarantine_over_resume_binding(fixture: &Fixture, session: &str) {
            let record = durable_record(&fixture.root, session);
            let mut binding = record.recovery.clone().expect("the record has a binding");
            binding.agent = "another-agent".to_owned();
            Store::new(fixture.root.join("data/metadata.jsonl"))
                .record_resume(&binding)
                .expect("persist the contradicting binding");
            assert!(
                fixture
                    .registry
                    .insert_unavailable_record(
                        record,
                        RuntimeState::Conflict,
                        "resume_binding_agent_mismatch",
                    )
                    .await
            );
        }

        #[tokio::test]
        async fn the_retry_finishes_an_interrupted_stop_over_a_resume_binding_conflict() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicted = interrupted_stop(&fixture, "s-5262").await;
            quarantine_over_resume_binding(&fixture, "s-5262").await;

            assert!(
                fixture
                    .registry
                    .retry_supervised_session(&conflicted.id)
                    .await,
                "the retry finishes the stop"
            );

            assert_stopped(&fixture, "s-5262").await;
        }

        #[tokio::test]
        async fn a_restart_finishes_an_interrupted_stop_over_a_resume_binding_conflict() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let _conflicted = interrupted_stop(&fixture, "s-5263").await;
            quarantine_over_resume_binding(&fixture, "s-5263").await;
            fixture.registry.inner.sessions.lock().await.clear();

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile after the restart");

            assert_stopped(&fixture, "s-5263").await;
        }

        /// A worker that exits with a terminal journal after its job was
        /// retired still leaves a stop pending until the cleanup is confirmed.
        #[tokio::test]
        async fn a_pending_stop_with_a_terminal_journal_waits_for_confirmed_cleanup() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicting = Marked::spawn_with(&[
                ("POHUNEK_WORKER_INSTANCE_ID", "runtime-s-5270"),
                ("POHUNEK_RUNTIME_ID", "runtime-s-5270-other"),
            ]);
            persist_stop_intent(&fixture.root, "s-5270", Some("runtime-s-5270"));
            write_terminal_journal(&fixture.root, "s-5270", "runtime-s-5270");

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, "s-5270").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(
                conflicting.alive(),
                "an ambiguous process is never signalled"
            );
            let durable = durable_record(&fixture.root, "s-5270");
            assert!(durable.transaction.is_some(), "the stop intent is kept");
            assert_ne!(durable.info.state, protocol::SessionState::Stopped);

            drop(conflicting);
            assert!(
                fixture
                    .registry
                    .retry_supervised_session(&SessionId("s-5270".to_owned()))
                    .await,
                "the confirmed cleanup finishes the stop"
            );
            assert_stopped(&fixture, "s-5270").await;
        }

        /// A quarantine whose classification could not be written is judged
        /// again by the retry, which never adopts over the contradiction.
        #[tokio::test]
        async fn a_resume_binding_quarantine_that_could_not_be_written_survives_the_retry() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let (controller, instance, _child, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, "s-5271", "worker-s-5271")
                    .await;
            let record = persist_record(&fixture.root, "s-5271", Some(instance.as_str()));
            drop(controller);
            let mut binding = record.recovery.clone().expect("the record has a binding");
            binding.agent = "another-agent".to_owned();
            Store::new(fixture.root.join("data/metadata.jsonl"))
                .record_resume(&binding)
                .expect("persist the contradicting binding");
            fixture
                .registry
                .inner
                .store
                .as_ref()
                .expect("registry store")
                .fail_next_write_before_rename();

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            assert_eq!(
                runtime_of(&fixture.registry, "s-5271").await.state,
                RuntimeState::Reconnecting
            );

            assert!(
                fixture
                    .registry
                    .retry_supervised_session(&SessionId("s-5271".to_owned()))
                    .await,
                "the quarantine is not retried"
            );

            let runtime = runtime_of(&fixture.registry, "s-5271").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some("resume_binding_agent_mismatch")
            );
            server_task.abort();
        }

        /// A conflict that became adoptable stays pending when the live record
        /// cannot be written, and a later pass adopts it.
        #[tokio::test]
        async fn a_failed_adoption_write_keeps_the_conflict_pending_for_a_later_retry() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let session = "s-5272";
            let (controller, instance, _child, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, session, "worker-s-5272")
                    .await;
            let record = persist_record(&fixture.root, session, Some(instance.as_str()));
            drop(controller);
            fixture.supervisor.script_job(
                service_id(session, TEST_GENERATION),
                present(ServiceState::Running, None),
            );
            assert!(
                fixture
                    .registry
                    .insert_unavailable_record(
                        record,
                        RuntimeState::Conflict,
                        "launch_identity_reference_mismatch",
                    )
                    .await
            );
            let id = SessionId(session.to_owned());
            fixture
                .registry
                .inner
                .store
                .as_ref()
                .expect("registry store")
                .fail_next_write_before_rename();

            assert!(
                !fixture.registry.retry_supervised_session(&id).await,
                "an unwritten adoption keeps the session pending"
            );
            assert_eq!(
                runtime_of(&fixture.registry, session).await.state,
                RuntimeState::Conflict
            );

            assert!(fixture.registry.retry_supervised_session(&id).await);
            assert_eq!(
                runtime_of(&fixture.registry, session).await.state,
                RuntimeState::Live
            );
            fixture
                .registry
                .stop(&id)
                .await
                .expect("stop adopted worker");
            server_task.abort();
        }

        /// At startup a failed adoption write lists the session and leaves it
        /// pending, and the retry adopts it.
        #[tokio::test]
        async fn a_failed_startup_adoption_write_lists_the_session_for_the_retry() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let session = "s-5273";
            let (controller, instance, _child, server_task) =
                spawn_initialized_worker(&fixture.root, &runtime_root, session, "worker-s-5273")
                    .await;
            persist_record(&fixture.root, session, Some(instance.as_str()));
            drop(controller);
            fixture
                .registry
                .inner
                .store
                .as_ref()
                .expect("registry store")
                .fail_next_write_before_rename();

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");
            assert_eq!(
                runtime_of(&fixture.registry, session).await.state,
                RuntimeState::Reconnecting
            );

            let id = SessionId(session.to_owned());
            assert!(fixture.registry.retry_supervised_session(&id).await);
            assert_eq!(
                runtime_of(&fixture.registry, session).await.state,
                RuntimeState::Live
            );
            fixture
                .registry
                .stop(&id)
                .await
                .expect("stop adopted worker");
            server_task.abort();
        }

        /// A session that newly joins the pending set is re-checked after the
        /// initial delay, however far the backoff of the others has grown.
        #[tokio::test(start_paused = true)]
        async fn a_session_scheduled_later_is_rechecked_after_the_initial_delay() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let record = persist_record(&fixture.root, "s-5280", None);
            assert!(
                fixture
                    .registry
                    .insert_unavailable_record(
                        record,
                        RuntimeState::Conflict,
                        "launch_identity_reference_mismatch",
                    )
                    .await
            );
            // The record cannot be read, so no pass settles the session.
            let store_path = fixture.root.join("data/metadata.jsonl");
            std::fs::remove_file(&store_path).expect("remove the store file");
            std::fs::create_dir(&store_path).expect("a directory cannot be read as the store");
            let (barrier, mut finished) = tokio::sync::mpsc::unbounded_channel();
            fixture
                .registry
                .inner
                .supervision_retries
                .state
                .lock()
                .expect("supervision retry state is never poisoned")
                .pass_finished = Some(barrier);
            let first = SessionId("s-5280".to_owned());
            let started = tokio::time::Instant::now();
            fixture.registry.schedule_supervision_retry(&first);

            // The delay doubles from the initial value with every pass.
            let mut expected = crate::session::supervision::SUPERVISION_RETRY_INITIAL;
            let mut elapsed = Duration::ZERO;
            for _ in 0..6 {
                let (id, resume) = finished.recv().await.expect("a pass runs");
                assert_eq!(id, first);
                elapsed += expected;
                assert_eq!(started.elapsed(), elapsed);
                expected *= 2;
                resume.send(()).expect("the loop waits at the barrier");
            }

            let second = SessionId("s-5281".to_owned());
            let scheduled = tokio::time::Instant::now();
            fixture.registry.schedule_supervision_retry(&second);
            loop {
                let (id, resume) = finished.recv().await.expect("a pass runs");
                resume.send(()).expect("the loop waits at the barrier");
                if id == second {
                    break;
                }
            }
            assert_eq!(
                scheduled.elapsed(),
                crate::session::supervision::SUPERVISION_RETRY_INITIAL,
                "the new session is re-checked after the initial delay"
            );
        }

        /// A classification that could not be written logs its one WARN when
        /// its placeholder is listed, is persisted by a retry pass, and is not
        /// logged again.
        #[tokio::test]
        async fn an_unpersisted_classification_logs_once_and_is_persisted_by_the_retry() {
            let logs = crate::runtime::lifecycle::tests::LogCapture::default();
            let _subscriber = logs.install();
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let session = "s-5290";
            persist_record(&fixture.root, session, None);
            write_live_journal(&fixture.root, session, dead_worker(), None);
            fixture
                .supervisor
                .script_job(service_id(session, TEST_GENERATION), JobScript::Unavailable);
            fixture.supervisor.fail_discovery();
            fixture
                .registry
                .inner
                .store
                .as_ref()
                .expect("registry store")
                .fail_next_write_before_rename();

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile");

            let runtime = runtime_of(&fixture.registry, session).await;
            assert_eq!(runtime.state, RuntimeState::Reconnecting);
            let classified = |logs: &crate::runtime::lifecycle::tests::LogCapture| {
                super::warn_lines(logs, session)
                    .into_iter()
                    .filter(|line| line.contains("session runtime is"))
                    .count()
            };
            assert_eq!(classified(&logs), 1, "{}", logs.text());
            assert_ne!(
                durable_record(&fixture.root, session).runtime.state,
                RuntimeState::Reconnecting,
                "the classification is not persisted yet"
            );

            assert!(
                !fixture
                    .registry
                    .retry_supervised_session(&SessionId(session.to_owned()))
                    .await,
                "the supervisor is still unavailable"
            );

            assert_eq!(
                durable_record(&fixture.root, session).runtime.state,
                RuntimeState::Reconnecting,
                "the retry persisted the classification"
            );
            assert_eq!(classified(&logs), 1, "{}", logs.text());
        }

        /// Lists `session`, whose stop is interrupted, as a placeholder after
        /// a failed startup write.
        async fn placeholder_listed_session(fixture: &Fixture, session: &str) -> SessionId {
            let conflicted = interrupted_stop(fixture, session).await;
            fixture.registry.inner.sessions.lock().await.clear();
            fixture
                .registry
                .inner
                .store
                .as_ref()
                .expect("registry store")
                .fail_next_write_before_rename();
            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile after the restart");
            assert!(fixture.registry.classification_unpersisted(session));
            conflicted.id
        }

        /// A placeholder flag lives exactly as long as the placeholder entry.
        #[tokio::test]
        async fn a_placeholder_flag_ends_with_its_entry() {
            let first = fixture(Arc::new(RetryInspector::default()));
            let installed = placeholder_listed_session(&first, "s-5300").await;
            assert!(
                first.registry.retry_supervised_session(&installed).await,
                "the retry commits the stop"
            );
            assert!(
                !first.registry.classification_unpersisted("s-5300"),
                "a real install clears the flag"
            );

            let second = fixture(Arc::new(RetryInspector::default()));
            let removed = placeholder_listed_session(&second, "s-5301").await;
            second
                .registry
                .remove(&removed)
                .await
                .expect("the placeholder-listed session is removed");
            assert!(
                !second.registry.classification_unpersisted("s-5301"),
                "a removal leaves no flag"
            );
        }

        /// A stop whose marked processes cannot be proven gone is not finished.
        #[tokio::test]
        async fn an_interrupted_stop_with_unconfirmed_cleanup_stays_pending() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let conflicting = Marked::spawn_with(&[
                ("POHUNEK_WORKER_INSTANCE_ID", "runtime-s-5233"),
                ("POHUNEK_RUNTIME_ID", "runtime-s-5233-other"),
            ]);
            let conflicted = interrupted_stop(&fixture, "s-5233").await;

            Box::pin(fixture.registry.reconcile_workers())
                .await
                .expect("reconcile after the restart");

            let runtime = runtime_of(&fixture.registry, "s-5233").await;
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(
                conflicting.alive(),
                "an ambiguous process is never signalled"
            );
            assert_eq!(
                durable_record(&fixture.root, "s-5233").info.state,
                protocol::SessionState::Running,
                "the stop is not committed"
            );
            assert_eq!(conflicted.id.0, "s-5233");
        }

        #[tokio::test]
        async fn a_stop_that_cannot_prove_the_recorded_runtime_refuses_and_writes_nothing() {
            let fixture = fixture(Arc::new(RetryInspector::default()));

            // The job under the service id is another executable's.
            let foreign = conflicted_session(&fixture, "s-5228").await;
            fixture.supervisor.script_job(
                foreign.job.clone(),
                JobScript::Present {
                    state: ServiceState::Running,
                    process: None,
                    definition: Some(DefinitionFacts {
                        executable: PathBuf::from("/opt/other/pohunek-sessiond"),
                        arguments: Vec::new(),
                    }),
                },
            );
            // The journal of the generation names another worker.
            let other_worker = conflicted_session(&fixture, "s-5229").await;
            write_worker_journal(
                &fixture.root,
                "s-5229",
                "worker-another",
                TEST_GENERATION,
                (
                    other_worker.worker.0,
                    host_identity(other_worker.worker.0).start_identity.get(),
                ),
                Some("runtime-s-5229"),
            );
            std::fs::remove_file(fixture.root.join("state/workers/s-5229/worker-s-5229.json"))
                .expect("remove the recorded worker's journal");

            // No job holds the worker process, which still runs.
            let unsupervised = conflicted_session_with(&fixture, "s-5233", false).await;
            for (session, conflicted) in [
                ("s-5228", &foreign),
                ("s-5229", &other_worker),
                ("s-5233", &unsupervised),
            ] {
                let before = stop_intent_of(&fixture.root, session);
                let error = fixture
                    .registry
                    .stop(&conflicted.id)
                    .await
                    .expect_err("an unproven runtime is not stopped");
                let expected = if session == "s-5233" {
                    SUPERVISION_AMBIGUOUS
                } else {
                    IDENTITY_MISMATCH
                };
                assert_eq!(error.code, expected, "{session}: {error:?}");
                assert_eq!(stop_intent_of(&fixture.root, session), before, "{session}");
                assert!(conflicted.worker.alive(), "{session}: nothing was stopped");
            }
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired on unproven identity"
            );
        }

        /// A conflict between a record and its resume binding is decided at
        /// startup from stored data; the watch never re-adopts the runtime it
        /// quarantined, while a conflict decided from worker evidence is
        /// re-checked and adopts a worker that has become adoptable.
        #[tokio::test]
        async fn the_watch_never_re_adopts_a_resume_binding_conflict() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            let runtime_root = fixture.root.join("runtime/workers");
            let cases = [
                (
                    "s-5234",
                    "resume_binding_shape_mismatch",
                    RuntimeState::Conflict,
                ),
                (
                    "s-5235",
                    "launch_identity_reference_mismatch",
                    RuntimeState::Live,
                ),
            ];
            for (session, reason, after_recheck) in cases {
                let (controller, instance, _child, server_task) = spawn_initialized_worker(
                    &fixture.root,
                    &runtime_root,
                    session,
                    &format!("worker-{session}"),
                )
                .await;
                let record = persist_record(&fixture.root, session, Some(instance.as_str()));
                drop(controller);
                fixture.supervisor.script_job(
                    service_id(session, TEST_GENERATION),
                    present(ServiceState::Running, None),
                );
                assert!(
                    fixture
                        .registry
                        .insert_unavailable_record(record, RuntimeState::Conflict, reason)
                        .await
                );

                assert!(
                    fixture
                        .registry
                        .retry_supervised_session(&SessionId(session.to_owned()))
                        .await,
                    "{session}: the pass settles the session"
                );

                let runtime = runtime_of(&fixture.registry, session).await;
                assert_eq!(runtime.state, after_recheck, "{session}: {runtime:?}");
                if after_recheck == RuntimeState::Conflict {
                    assert_eq!(runtime.loss_reason.as_deref(), Some(reason));
                } else {
                    fixture
                        .registry
                        .stop(&SessionId(session.to_owned()))
                        .await
                        .expect("stop the adopted worker");
                }
                server_task.abort();
            }
        }

        #[tokio::test]
        async fn a_refused_stop_of_an_unavailable_runtime_leaves_no_durable_intent() {
            let fixture = fixture(Arc::new(RetryInspector::default()));
            // A conflict whose record names no worker generation, as observed
            // in the field, and the other unavailable states.
            let cases = [
                (
                    "s-5230",
                    RuntimeState::Conflict,
                    IDENTITY_MISMATCH,
                    "session_runtime_conflict",
                ),
                (
                    "s-5231",
                    RuntimeState::Lost,
                    RUNTIME_LOST,
                    "session_runtime_lost",
                ),
                (
                    "s-5232",
                    RuntimeState::Reconnecting,
                    SUPERVISION_UNAVAILABLE,
                    "session_runtime_reconnecting",
                ),
            ];
            for (session, state, reason, code) in cases {
                let record = persist_record(&fixture.root, session, Some("runtime-unbound"));
                let mut unbound = record.clone();
                unbound.runtime.service_id = None;
                unbound.runtime.generation = None;
                unbound.runtime.executable = None;
                assert!(
                    fixture
                        .registry
                        .insert_unavailable_record(unbound, state, reason)
                        .await
                );
                let before = stop_intent_of(&fixture.root, session);

                let error = fixture
                    .registry
                    .stop(&SessionId(session.to_owned()))
                    .await
                    .expect_err("the runtime cannot be stopped");

                assert_eq!(error.code, code, "{session}: {error:?}");
                assert_eq!(
                    stop_intent_of(&fixture.root, session),
                    before,
                    "{session}: no `desired_state = stopped` or `requested` transaction is left"
                );
                let entry_stopping = fixture
                    .registry
                    .inner
                    .sessions
                    .lock()
                    .await
                    .get(&SessionId(session.to_owned()))
                    .map(|entry| (entry.stopping, entry.desired_state));
                assert_eq!(
                    entry_stopping,
                    Some((false, DesiredState::Running)),
                    "{session}"
                );
            }
        }
    }

    /// A running daemon classifies a crashed worker through its supervisor.
    ///
    /// These fixtures run the real `pohunek-sessiond` under the subprocess
    /// launcher, so the journal records a real worker process that the test
    /// kills, and the PTY runs a hangup-ignoring marked descendant.
    mod running_loss {
        use std::collections::BTreeSet;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::time::Duration;

        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        use pohunek_platform::supervisor::{DefinitionFacts, ServiceId, ServiceState};
        use pohunek_test_support::wait::{poll_until as wait_until_sync, wait_until};
        use pohunek_test_support::worker_binary;
        use protocol::{RuntimeState, SessionId, SessionInfo};

        use super::super::super::supervision::{
            WorkerBounds, RUNTIME_LOST, RUNTIME_LOST_CLEANUP_UNCONFIRMED,
        };
        use super::temp_root;
        use crate::procwatch::readable_host::ReadableHost;
        use crate::procwatch::scoped_host::ScopedHost;
        use crate::procwatch::{Error, HostInspector, ProcessInspector};
        use crate::runtime::lifecycle::tests::{JobScript, ScriptedSupervisor};
        use crate::runtime::lifecycle::{
            IDENTITY_MISMATCH, SUPERVISION_AMBIGUOUS, SUPERVISION_UNAVAILABLE,
        };
        use crate::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
        use crate::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
        use crate::store::Store;

        /// Connect deadline that a proven crash must not wait for.
        const LONG_CONNECT: Duration = Duration::from_secs(120);
        /// Connect deadline of fixtures whose classification needs it.
        const SHORT_CONNECT: Duration = Duration::from_millis(300);
        /// Sweep grace; short so the `SIGKILL` fallback stays fast.
        const SWEEP_GRACE: Duration = Duration::from_millis(300);
        use pohunek_test_support::wait::HANG_GUARD;
        struct Fixture {
            root: crate::test_support::ScopedDir,
            registry: SessionRegistry,
            launcher: SubprocessWorkerLauncher,
            supervisor: Arc<ScriptedSupervisor>,
            marker_pid_file: PathBuf,
        }

        fn fixture(connect: Duration) -> Fixture {
            fixture_over(connect, "sleep", |_root| Arc::new(ReadableHost::new()))
        }

        /// A fixture whose registry observes the host through the inspector
        /// `host` builds for the fixture's directory.
        ///
        /// The PTY runs `sleep_program` for its descendant and its heartbeat.
        fn fixture_over(
            connect: Duration,
            sleep_program: &str,
            host: impl FnOnce(&std::path::Path) -> Arc<dyn ProcessInspector>,
        ) -> Fixture {
            let root = temp_root();
            let inspector = host(&root);
            let environment = SubprocessWorkerEnvironment {
                runtime_home: root.join("r"),
                state_home: root.join("s"),
                data_home: root.join("d"),
                config_home: root.join("c"),
                cache_home: root.join("k"),
                home: root.clone(),
                daemon_socket: root.join("daemon.sock"),
            };
            let mut supervision = environment
                .supervision(worker_binary())
                .with_environment_source(crate::test_support::thread_environment_source());
            supervision.sweep_grace = SWEEP_GRACE;
            let marker_pid_file = root.join("descendant.pid");
            let script = format!(
                "trap '' HUP; (trap '' HUP; exec {sleep_program} 300) & echo $! > {}; while :; do {sleep_program} 1; done",
                marker_pid_file.display()
            );
            let launcher = SubprocessWorkerLauncher::new();
            let supervisor = Arc::new(ScriptedSupervisor::over(launcher.clone()));
            let registry = SessionRegistry::new_with_launcher_and_inspector(
                SessionRegistryConfig {
                    shell_command: ShellCommand::new("/bin/sh", ["-c".to_owned(), script]),
                    store_path: Some(root.join("data/metadata.jsonl")),
                    worker_runtime_root: Some(root.join("r/pohunek/workers")),
                    worker_state_root: Some(root.join("s/pohunek/workers")),
                    worker_connect_deadline: connect,
                    supervision: Some(supervision),
                    ..SessionRegistryConfig::default()
                },
                Arc::clone(&supervisor) as Arc<dyn crate::runtime::WorkerLauncher>,
                inspector,
            );
            Fixture {
                root,
                registry,
                launcher,
                supervisor,
                marker_pid_file,
            }
        }

        impl Fixture {
            /// Creates one shell session and returns it with its job and the
            /// PID of its hangup-ignoring marked descendant.
            async fn create(&self) -> (SessionInfo, ServiceId, u32) {
                let created = self
                    .registry
                    .create(protocol::SessionNewParams {
                        name: None,
                        agent: "shell".to_owned(),
                        cwd: Some(self.root.clone()),
                        cols: 80,
                        rows: 24,
                        project: None,
                        repo: None,
                        branch: None,
                        base_branch: None,
                        input: None,
                        extended_input_ready_wait: None,
                        metadata: std::collections::BTreeMap::new(),
                    })
                    .await
                    .expect("create a supervised session");
                let service_id = Store::new(self.root.join("data/metadata.jsonl"))
                    .load_sessions()
                    .expect("load store")
                    .into_iter()
                    .find(|record| record.session_id == created.id.0)
                    .and_then(|record| record.runtime.service_id)
                    .expect("the record names its job");
                let deadline = tokio::time::Instant::now() + HANG_GUARD;
                let descendant = loop {
                    if let Some(pid) = std::fs::read_to_string(&self.marker_pid_file)
                        .ok()
                        .and_then(|text| text.trim().parse().ok())
                    {
                        break pid;
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "PTY never started its descendant"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                };
                (
                    created,
                    ServiceId::parse(service_id).expect("service id"),
                    descendant,
                )
            }

            async fn crash(&self, id: &SessionId) {
                assert!(
                    self.launcher
                        .kill_worker(&id.0)
                        .await
                        .expect("kill the worker process"),
                    "the worker was running"
                );
            }

            /// Waits until the session's runtime leaves `live`/`reconnecting`
            /// for `state`.
            async fn wait_for(
                &self,
                id: &SessionId,
                state: RuntimeState,
            ) -> protocol::SessionRuntime {
                let deadline = tokio::time::Instant::now() + HANG_GUARD;
                loop {
                    let runtime = self
                        .registry
                        .inspect(id)
                        .await
                        .expect("session stays visible")
                        .runtime
                        .expect("runtime");
                    if runtime.state == state {
                        return runtime;
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "runtime never became {state:?}: {runtime:?}"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }

        fn alive(pid: u32) -> bool {
            HostInspector::new()
                .identity(pid)
                .is_ok_and(|identity| identity.is_some())
        }

        async fn wait_gone(pid: u32) {
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while alive(pid) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "marked descendant {pid} survived"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        #[tokio::test]
        async fn proven_crash_is_swept_retired_and_lost_before_the_connect_deadline() {
            let fixture = fixture(LONG_CONNECT);
            let (created, job, descendant) = fixture.create().await;
            let started = tokio::time::Instant::now();

            fixture.crash(&created.id).await;
            let runtime = fixture.wait_for(&created.id, RuntimeState::Lost).await;

            assert!(
                started.elapsed() < LONG_CONNECT,
                "a proven crash is classified without waiting for the connect deadline"
            );
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            wait_gone(descendant).await;
            assert_eq!(fixture.supervisor.retired(), vec![job]);
            let stored = Store::new(fixture.root.join("data/metadata.jsonl"))
                .load_sessions()
                .expect("load store")
                .into_iter()
                .find(|record| record.session_id == created.id.0)
                .expect("the logical record is kept");
            assert_eq!(stored.runtime.state, RuntimeState::Lost);
            assert_eq!(stored.runtime.reason.as_deref(), Some(RUNTIME_LOST));
        }

        #[tokio::test]
        async fn every_runtime_classification_logs_one_warn_with_session_worker_and_reason() {
            let logs = crate::runtime::lifecycle::tests::LogCapture::default();
            let _subscriber = logs.install();
            let fixture = fixture(LONG_CONNECT);
            let (created, _job, descendant) = fixture.create().await;
            let worker_id = created
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.worker_id.clone())
                .expect("created worker id");

            fixture.crash(&created.id).await;
            fixture.wait_for(&created.id, RuntimeState::Lost).await;

            let warns = super::warn_lines(&logs, &created.id.0);
            let reasons = ["worker_connection_lost", RUNTIME_LOST];
            assert_eq!(
                warns.len(),
                reasons.len(),
                "one WARN per classification: {warns:#?}"
            );
            for (warn, reason) in warns.iter().zip(reasons) {
                assert!(
                    warn.contains(&format!("worker_id={worker_id} "))
                        && warn.contains(&format!("reason={reason}")),
                    "the WARN names the worker and {reason}: {warn}"
                );
            }
            wait_gone(descendant).await;
        }

        /// A job the supervisor shows under the session's service id but that
        /// runs another executable: the crashed worker's job is not provably
        /// the recorded generation's.
        fn foreign_job() -> JobScript {
            JobScript::Present {
                state: ServiceState::Running,
                process: None,
                definition: Some(DefinitionFacts {
                    executable: PathBuf::from("/opt/other/pohunek-sessiond"),
                    arguments: Vec::new(),
                }),
            }
        }

        #[tokio::test]
        async fn a_conflicted_runtime_becomes_lost_when_its_job_ends_without_a_restart() {
            let logs = crate::runtime::lifecycle::tests::LogCapture::default();
            let _subscriber = logs.install();
            let fixture = fixture(SHORT_CONNECT);
            let (created, job, descendant) = fixture.create().await;
            fixture.supervisor.script_job(job.clone(), foreign_job());

            fixture.crash(&created.id).await;
            let runtime = fixture.wait_for(&created.id, RuntimeState::Conflict).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(IDENTITY_MISMATCH));
            assert!(
                fixture.supervisor.retired().is_empty(),
                "a foreign job is never retired"
            );
            assert!(alive(descendant), "nothing is swept while conflicted");

            // The watcher keeps the classification while the evidence holds:
            // no new WARN, no new event.
            let mut events = fixture.registry.subscribe();
            for _ in 0..2 {
                assert!(
                    !fixture.registry.retry_supervised_session(&created.id).await,
                    "an unchanged conflict stays watched"
                );
            }
            assert!(
                events.try_recv().is_err(),
                "an unchanged conflict announces nothing"
            );
            let conflict_warns = super::warn_lines(&logs, &created.id.0)
                .into_iter()
                .filter(|line| line.contains(&format!("reason={IDENTITY_MISMATCH}")))
                .count();
            assert_eq!(conflict_warns, 1, "{}", logs.text());

            fixture.supervisor.script_job(
                job.clone(),
                JobScript::Present {
                    state: ServiceState::Failed,
                    process: None,
                    definition: None,
                },
            );
            let runtime = fixture.wait_for(&created.id, RuntimeState::Lost).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            wait_gone(descendant).await;
            assert_eq!(fixture.supervisor.retired(), vec![job]);
        }

        #[tokio::test]
        async fn unavailable_manager_keeps_the_crashed_session_reconnecting_until_it_answers() {
            let fixture = fixture(SHORT_CONNECT);
            let (created, job, descendant) = fixture.create().await;
            fixture
                .supervisor
                .script_job(job.clone(), JobScript::Unavailable);

            fixture.crash(&created.id).await;
            let runtime = fixture
                .wait_for(&created.id, RuntimeState::Reconnecting)
                .await;
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            let mut runtime = runtime;
            while runtime.loss_reason.as_deref() != Some(SUPERVISION_UNAVAILABLE) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "runtime never recorded the unavailable supervisor: {runtime:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
                runtime = fixture
                    .wait_for(&created.id, RuntimeState::Reconnecting)
                    .await;
            }
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired"
            );
            assert!(alive(descendant), "nothing is swept while unavailable");

            fixture.supervisor.script_job(
                job.clone(),
                JobScript::Present {
                    state: ServiceState::Failed,
                    process: None,
                    definition: None,
                },
            );
            let runtime = fixture.wait_for(&created.id, RuntimeState::Lost).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            wait_gone(descendant).await;
            assert_eq!(fixture.supervisor.retired(), vec![job]);
        }

        #[tokio::test]
        async fn crashed_session_is_lost_only_after_its_job_is_retired() {
            let fixture = fixture(SHORT_CONNECT);
            let (created, job, descendant) = fixture.create().await;
            fixture.supervisor.script_retire_unavailable(job.clone());

            fixture.crash(&created.id).await;
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            loop {
                let runtime = fixture
                    .registry
                    .inspect(&created.id)
                    .await
                    .expect("session stays visible")
                    .runtime
                    .expect("runtime");
                assert_ne!(
                    runtime.state,
                    RuntimeState::Lost,
                    "a crash whose job survived is never lost"
                );
                if runtime.state == RuntimeState::Reconnecting
                    && runtime.loss_reason.as_deref() == Some(SUPERVISION_UNAVAILABLE)
                {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "runtime never waited for the failed retirement: {runtime:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(fixture.supervisor.retired(), vec![job.clone()]);

            let runtime = fixture.wait_for(&created.id, RuntimeState::Lost).await;
            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            assert_eq!(fixture.supervisor.retired(), vec![job.clone(), job]);
            wait_gone(descendant).await;
        }

        impl Fixture {
            /// Journal directory of `id`'s workers.
            fn journal_dir(&self, id: &SessionId) -> PathBuf {
                self.root.join("s/pohunek/workers").join(&id.0)
            }

            /// Reaps the hangup-ignoring PTY tree of a session left alone.
            async fn reap(&self, created: &SessionInfo, descendant: u32) {
                let worker_instance_id = created
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.worker_instance_id.clone())
                    .expect("created runtime id");
                self.registry
                    .sweep_lost_runtime(&created.id.0, &worker_instance_id, WorkerBounds::NONE)
                    .await;
                wait_gone(descendant).await;
            }
        }

        #[tokio::test]
        async fn crash_without_a_journal_retires_the_job_and_sweeps_nothing() {
            let fixture = fixture(SHORT_CONNECT);
            let (created, job, descendant) = fixture.create().await;
            for entry in std::fs::read_dir(fixture.journal_dir(&created.id)).expect("list journals")
            {
                std::fs::remove_file(entry.expect("journal entry").path()).expect("remove journal");
            }

            fixture.crash(&created.id).await;
            let runtime = fixture.wait_for(&created.id, RuntimeState::Lost).await;

            assert_eq!(runtime.loss_reason.as_deref(), Some("worker_unavailable"));
            assert_eq!(fixture.supervisor.retired(), vec![job]);
            assert!(
                alive(descendant),
                "nothing proves which runtime ran, so nothing is swept"
            );
            fixture.reap(&created, descendant).await;
        }

        #[tokio::test]
        async fn crash_next_to_an_unreadable_journal_is_ambiguous_and_untouched() {
            let fixture = fixture(SHORT_CONNECT);
            let (created, _job, descendant) = fixture.create().await;
            let unreadable = fixture
                .journal_dir(&created.id)
                .join("worker-unreadable.json");
            std::fs::write(&unreadable, b"not json").expect("write unreadable journal");
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o600))
                .expect("private unreadable journal");

            fixture.crash(&created.id).await;
            let runtime = fixture.wait_for(&created.id, RuntimeState::Conflict).await;

            assert!(
                matches!(
                    runtime.loss_reason.as_deref(),
                    Some(SUPERVISION_AMBIGUOUS | "worker_journal_identity_mismatch")
                ),
                "{runtime:?}"
            );
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired"
            );
            assert!(alive(descendant), "nothing is swept");
            fixture.reap(&created, descendant).await;
        }

        #[tokio::test]
        async fn present_job_with_an_unreachable_worker_is_ambiguous_and_untouched() {
            let fixture = fixture(SHORT_CONNECT);
            let (created, job, descendant) = fixture.create().await;
            fixture.supervisor.script_job(
                job,
                JobScript::Present {
                    state: ServiceState::Running,
                    process: None,
                    definition: None,
                },
            );

            fixture.crash(&created.id).await;
            let runtime = fixture.wait_for(&created.id, RuntimeState::Conflict).await;

            assert_eq!(runtime.loss_reason.as_deref(), Some(SUPERVISION_AMBIGUOUS));
            assert!(
                fixture.supervisor.retired().is_empty(),
                "nothing is retired"
            );
            assert!(alive(descendant), "nothing is swept");
            // The fixture's PTY tree ignores hangup; reap it explicitly.
            let worker_instance_id = created
                .runtime
                .and_then(|runtime| runtime.worker_instance_id)
                .expect("created runtime id");
            fixture
                .registry
                .sweep_lost_runtime(&created.id.0, &worker_instance_id, WorkerBounds::NONE)
                .await;
            wait_gone(descendant).await;
        }

        // A lost worker's runtime is swept on the real host, whose kernel may
        // withhold the environment of platform binaries such as `/bin/sh` and
        // `/bin/sleep`. The real worker runs a PTY of platform binaries and
        // journals its own kernel creation number. Each scenario sees only the
        // processes working in its own directory (`ScopedHost`), so the sweep
        // decides the runtime and the bystanders started beside it, never the
        // processes of a loaded host.

        /// Child processes a scenario starts as bystanders; they are killed
        /// and reaped when the scenario ends, also while a panic unwinds.
        #[derive(Default)]
        struct Bystanders(Vec<std::process::Child>);

        impl Bystanders {
            /// Starts `program` working in `directory` with an empty
            /// environment and returns its PID.
            fn start(&mut self, directory: &std::path::Path, program: &str, args: &[&str]) -> u32 {
                let child = std::process::Command::new(program)
                    .args(args)
                    .current_dir(directory)
                    .env_clear()
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("start a bystander");
                let pid = child.id();
                self.0.push(child);
                pid
            }
        }

        impl Drop for Bystanders {
            fn drop(&mut self) {
                for child in &mut self.0 {
                    let _killed = child.kill();
                    let _reaped = child.wait();
                }
            }
        }

        /// Kills every process working in a scenario's directory when the
        /// scenario ends, so a runtime the sweep left alive does not outlive it,
        /// and returns only once none of them runs.
        ///
        /// The scenario's processes are found by their working directory, so the
        /// reaper must drop while that directory exists.
        struct ScopeReaper(ScopedHost);

        impl ScopeReaper {
            fn new(directory: &std::path::Path) -> Self {
                Self(ScopedHost::new(directory))
            }

            /// Identities of the scenario's processes that still run.
            ///
            /// # Errors
            ///
            /// Returns the inspection failure: a process table or liveness read
            /// that failed proves nothing about whether the processes exited. A
            /// process that exits during the read is not running.
            fn running(&self) -> Result<Vec<crate::procwatch::ProcessIdentity>, Error> {
                let mut running = Vec::new();
                for fact in self.0.same_user_processes()? {
                    let identity = fact.identity();
                    match self.0.is_running(identity) {
                        Ok(true) => running.push(identity),
                        Ok(false) => {}
                        Err(error) if error.is_race() => {}
                        Err(error) => return Err(error),
                    }
                }
                Ok(running)
            }

            /// PIDs of the processes that still run in the scenario's directory.
            fn pids(&self) -> BTreeSet<u32> {
                self.running()
                    .expect("inspect the scenario's processes")
                    .into_iter()
                    .map(|identity| identity.pid)
                    .collect()
            }
        }

        impl Drop for ScopeReaper {
            /// Signals every running process of the scenario again until a
            /// listing finds none: a process that forked before it was signalled
            /// shows up in the next listing. A failed inspection is retried and
            /// never counts as an exit, so a persistent failure ends in the hang
            /// guard instead of a teardown that reports success.
            fn drop(&mut self) {
                wait_until_sync("the scenario's processes to exit", || {
                    let running = self.running().ok()?;
                    for identity in &running {
                        if let Ok(raw) = i32::try_from(identity.pid) {
                            let _signalled = kill(Pid::from_raw(raw), Signal::SIGKILL);
                        }
                    }
                    running.is_empty().then_some(())
                });
            }
        }

        /// A scenario's real-host registry with its bystanders: one started
        /// before the worker, and a tree (a shell and its child) started after
        /// it by the test process, which is older than the worker.
        ///
        /// Fields drop in declaration order, which is the teardown order: the
        /// reaper first, because it finds the runtime and the bystanders' children
        /// by the fixture's directory; then the bystanders; then the fixture,
        /// which removes the directory.
        struct Scene {
            scope: ScopeReaper,
            _bystanders: Bystanders,
            fixture: Fixture,
            earlier: u32,
            tree: u32,
            tree_child: u32,
            session: protocol::SessionInfo,
            job: ServiceId,
            descendant: u32,
        }

        impl Scene {
            async fn start() -> Self {
                let fixture = fixture_over(LONG_CONNECT, "/bin/sleep", |root| {
                    Arc::new(ScopedHost::new(root))
                });
                let scope = ScopeReaper::new(&fixture.root);
                let mut bystanders = Bystanders::default();
                let earlier = bystanders.start(&fixture.root, "/bin/sleep", &["300"]);
                let (session, job, descendant) = fixture.create().await;
                let tree =
                    bystanders.start(&fixture.root, "/bin/sh", &["-c", "/bin/sleep 300 & wait"]);
                let tree_child = wait_until("the sibling tree's child", || async {
                    HostInspector::new()
                        .descendants(tree)
                        .ok()?
                        .first()
                        .map(|fact| fact.pid)
                })
                .await;
                Self {
                    fixture,
                    scope,
                    _bystanders: bystanders,
                    earlier,
                    tree,
                    tree_child,
                    session,
                    job,
                    descendant,
                }
            }

            fn bystander_pids(&self) -> BTreeSet<u32> {
                BTreeSet::from([self.earlier, self.tree, self.tree_child])
            }

            fn assert_bystanders_alive(&self) {
                for pid in self.bystander_pids() {
                    assert!(alive(pid), "bystander {pid} was signalled");
                }
            }

            /// Whether the host withholds the environment of the runtime's own
            /// platform binaries, read from a live process of the runtime.
            fn environment_withheld(&self) -> bool {
                matches!(
                    HostInspector::new().ownership_markers(self.descendant),
                    Err(Error::Unobservable { .. } | Error::PermissionDenied { .. })
                )
            }

            /// Rewrites the only journal of the session while its worker is
            /// stopped, so the worker cannot write the file back over the edit.
            async fn rewrite_journal(
                &self,
                edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
            ) {
                let worker = self
                    .fixture
                    .launcher
                    .worker_process_id(&self.session.id.0)
                    .await
                    .expect("the worker runs");
                kill(
                    Pid::from_raw(i32::try_from(worker).expect("worker pid")),
                    Signal::SIGSTOP,
                )
                .expect("stop the worker");
                self.fixture.edit_journal(&self.session.id, edit);
            }
        }

        impl Fixture {
            /// Applies `edit` to the session's only journal in place.
            fn edit_journal(
                &self,
                id: &SessionId,
                edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
            ) {
                let mut journals = std::fs::read_dir(self.journal_dir(id))
                    .expect("list journals")
                    .map(|entry| entry.expect("journal entry").path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "json"));
                let path = journals.next().expect("the session journals");
                assert!(journals.next().is_none(), "exactly one journal");
                let mut value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).expect("read journal"))
                        .expect("journal is JSON");
                edit(value.as_object_mut().expect("journal is an object"));
                std::fs::write(&path, serde_json::to_vec(&value).expect("encode journal"))
                    .expect("write journal");
            }
        }

        #[tokio::test]
        async fn the_worker_journals_its_own_spawn_id_only_where_the_host_reports_lineage() {
            let scene = Scene::start().await;
            let worker = scene
                .fixture
                .launcher
                .worker_process_id(&scene.session.id.0)
                .await
                .expect("the worker runs");
            let mut journal = None;
            scene.fixture.edit_journal(&scene.session.id, |fields| {
                journal = Some(fields.clone());
            });
            let journal = journal.expect("journal read");
            let recorded = journal
                .get("worker_spawn_id")
                .and_then(|value| value.as_str());

            #[cfg(target_os = "macos")]
            {
                let lineage = HostInspector::new()
                    .lineage(worker)
                    .expect("the host reports lineage")
                    .expect("the worker runs");
                assert_eq!(recorded, Some(lineage.id.to_string().as_str()));
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = worker;
                assert_eq!(recorded, None, "a host without lineage journals none");
            }
        }

        #[tokio::test]
        async fn a_lost_runtime_of_platform_binaries_is_swept_to_a_confirmed_cleanup() {
            let scene = Scene::start().await;

            scene.fixture.crash(&scene.session.id).await;
            let runtime = scene
                .fixture
                .wait_for(&scene.session.id, RuntimeState::Lost)
                .await;

            assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
            wait_gone(scene.descendant).await;
            let expected = scene.bystander_pids();
            wait_until("only the bystanders to remain", || async {
                (scene.scope.pids() == expected).then_some(())
            })
            .await;
            scene.assert_bystanders_alive();
            assert_eq!(scene.fixture.supervisor.retired(), vec![scene.job.clone()]);
        }

        /// What a sweep that was given no spawn id leaves of a lost runtime.
        ///
        /// Without lineage, a host that withholds the runtime's environment
        /// keeps the cleanup unconfirmed and the runtime alive; a host that
        /// exposes it reaps the runtime by its markers. Either way no bystander
        /// is signalled.
        async fn assert_unbounded_sweep(scene: &Scene) {
            let withheld = scene.environment_withheld();
            scene.fixture.crash(&scene.session.id).await;
            let runtime = scene
                .fixture
                .wait_for(&scene.session.id, RuntimeState::Lost)
                .await;

            if withheld {
                assert_eq!(
                    runtime.loss_reason.as_deref(),
                    Some(RUNTIME_LOST_CLEANUP_UNCONFIRMED)
                );
                assert!(
                    alive(scene.descendant),
                    "unproven processes are not signalled"
                );
            } else {
                assert_eq!(runtime.loss_reason.as_deref(), Some(RUNTIME_LOST));
                wait_gone(scene.descendant).await;
            }
            scene.assert_bystanders_alive();
        }

        #[tokio::test]
        async fn a_journal_without_a_spawn_id_leaves_hidden_processes_unconfirmed() {
            let scene = Scene::start().await;
            scene
                .rewrite_journal(|fields| {
                    fields.remove("worker_spawn_id");
                })
                .await;

            assert_unbounded_sweep(&scene).await;
        }

        /// A spawn id recorded in another boot never reaches the sweep: the
        /// recorded number is that of a live bystander's parent, so trusting it
        /// would attribute the bystander to the lost worker and signal it.
        #[cfg(target_os = "macos")]
        #[tokio::test]
        async fn a_spawn_id_of_another_boot_is_never_given_to_the_sweep() {
            let scene = Scene::start().await;
            let stale = HostInspector::new()
                .lineage(scene.tree)
                .expect("the host reports lineage")
                .expect("the bystander's shell runs")
                .id;
            scene
                .rewrite_journal(|fields| {
                    fields.insert(
                        "boot_identity".to_owned(),
                        serde_json::Value::String("boot-of-another-run".to_owned()),
                    );
                    fields.insert(
                        "worker_spawn_id".to_owned(),
                        serde_json::Value::String(stale.to_string()),
                    );
                })
                .await;

            assert_unbounded_sweep(&scene).await;
        }

        #[tokio::test]
        async fn removal_needs_no_accepted_cleanup_once_the_journal_names_the_worker() {
            let scene = Scene::start().await;
            let recorded = {
                let mut value = None;
                scene.fixture.edit_journal(&scene.session.id, |fields| {
                    value = fields.get("worker_spawn_id").cloned();
                });
                value.expect("journal read")
            };
            scene
                .rewrite_journal(|fields| {
                    fields.remove("worker_spawn_id");
                })
                .await;
            scene.fixture.crash(&scene.session.id).await;
            scene
                .fixture
                .wait_for(&scene.session.id, RuntimeState::Lost)
                .await;
            scene.fixture.edit_journal(&scene.session.id, |fields| {
                fields.insert("worker_spawn_id".to_owned(), recorded);
            });

            scene
                .fixture
                .registry
                .remove(&scene.session.id)
                .await
                .expect("the removal sweep confirms the cleanup");

            wait_gone(scene.descendant).await;
            let expected = scene.bystander_pids();
            wait_until("only the bystanders to remain", || async {
                (scene.scope.pids() == expected).then_some(())
            })
            .await;
            scene.assert_bystanders_alive();
        }
    }

    /// Store written by the v0.33.0 daemon, with flat resume fields (see
    /// `store/fixtures/v0.33.0`).
    #[cfg(target_os = "linux")]
    const V0_33_0_STORE: &str = include_str!("../store/fixtures/v0.33.0/metadata.jsonl");

    /// Live workers started by the previous release and adopted by this one.
    ///
    /// The store is a migrated v0.33.0 or v0.33.1 fixture, and the worker is a
    /// real worker server whose PTY root is a copy of the shell named
    /// `claude`, so a launch identity reported from inside the PTY is verified
    /// against a process the worker can designate as the agent.
    #[cfg(target_os = "linux")]
    mod previous_release {
        use std::path::{Path, PathBuf};
        use std::sync::Arc;
        use std::time::Duration;

        use pohunek_test_support::wait::wait_until;
        use pohunek_worker_protocol::{LaunchIdentity, WorkerInstanceId};
        use protocol::{RuntimeState, SessionId, SessionInfo};
        use time::format_description::well_known::Rfc3339;
        use time::OffsetDateTime;

        use super::{
            hermetic_shell, identity_reporters, send_identity_hook_from, temp_root,
            wait_for_directory, Reporter, RetryInspector, V0_33_0_STORE, V0_33_1_DAMAGED_STORE,
        };
        use crate::procwatch::ProcessInspector;
        use crate::session::{SessionRegistry, SessionRegistryConfig};
        use crate::store::Store;

        /// Reference the fixture sessions store for their native conversation.
        pub(in super::super) const STORED_REFERENCE: &str = "native-fixture-2";

        /// Generation the in-process worker server journals.
        const GENERATION: &str = "abcd2345";

        /// Executable recorded for the worker job of the fixture sessions.
        const WORKER_EXECUTABLE: &str = "/opt/pohunek/libexec/pohunek/1.0.0/pohunek-sessiond";

        /// One live worker whose PTY root is named `claude` and whose launch
        /// identity has been accepted.
        pub(in super::super) struct LaunchWorker {
            pub controller: crate::runtime::Worker,
            pub worker_instance_id: WorkerInstanceId,
            pub child_pid: u32,
            pub server_task: tokio::task::JoinHandle<()>,
            pub reporter: Reporter,
        }

        /// Starts the worker of `session_id` and reports `reference` as its
        /// launch identity from inside the PTY.
        pub(in super::super) async fn spawn_launch_worker(
            root: &Path,
            runtime_root: &Path,
            session_id: &str,
            worker_id: &str,
            reference: &str,
        ) -> LaunchWorker {
            let executable = root.join("claude");
            std::fs::copy("/bin/sh", &executable).expect("copy the shell as the agent executable");
            std::fs::set_permissions(
                &executable,
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .expect("make the agent executable runnable");
            let (script, mut reporters) = identity_reporters(root, &["launch"]);
            let reporter = reporters.pop().expect("one reporter");
            let command = format!(
                "{} printf ready; while :; do sleep 1; done",
                reporter.launch(&script)
            );
            let (controller, worker_instance_id, child_pid, server_task) =
                super::spawn_initialized_worker_with_launch(
                    root,
                    runtime_root,
                    session_id,
                    worker_id,
                    LaunchIdentity {
                        agent: "claude".to_owned(),
                        agent_base: "claude".to_owned(),
                        reference_kind: Some("id".to_owned()),
                    },
                    executable,
                    command,
                )
                .await;
            wait_for_directory(&reporter.inbox).await;
            let child = controller
                .inspect()
                .await
                .expect("inspect the initialized worker")
                .child_process
                .expect("worker child identity");
            let expires_at = (OffsetDateTime::now_utc() + time::Duration::seconds(60))
                .format(&Rfc3339)
                .expect("format identity expiry");
            assert!(
                send_identity_hook_from(
                    &reporter,
                    serde_json::json!({
                        "type": "identity_report",
                        "runtime_id": worker_instance_id.as_str(),
                        "provider": "claude",
                        "pid": child.pid,
                        "start_identity": child.start_identity,
                        "sequence": 1,
                        "expires_at": expires_at,
                        "reference_kind": "id",
                        "native_reference": reference,
                    }),
                )
                .await,
                "the worker accepts the report from inside its PTY"
            );
            wait_until("the worker journals its launch identity", || async {
                controller
                    .inspect()
                    .await
                    .ok()
                    .and_then(|snapshot| snapshot.launch_identity)
            })
            .await;
            LaunchWorker {
                controller,
                worker_instance_id,
                child_pid,
                server_task,
                reporter,
            }
        }

        /// What the store of the previous release carries for the session.
        pub(in super::super) struct Previous {
            /// Release the store was written by.
            pub generation: &'static str,
            pub fixture: &'static str,
            /// Agent name the session ran, replacing the fixture's `claude`.
            pub agent: &'static str,
            /// Profile files of the new daemon's agents directory.
            pub profiles: &'static [(&'static str, &'static str)],
        }

        /// Rewrites the session and resume lines of a previous-release
        /// `fixture` so they name the live worker, then migrates the store as
        /// the upgraded daemon does at startup.
        pub(in super::super) fn write_previous_store(
            root: &Path,
            previous: &Previous,
            session_id: &str,
            worker_id: &str,
            worker_instance_id: &str,
            child_pid: u32,
        ) -> PathBuf {
            let mut lines = Vec::new();
            for line in previous.fixture.lines() {
                let mut value: serde_json::Value =
                    serde_json::from_str(line).expect("fixture line");
                if value["kind"] != "session" {
                    continue;
                }
                value["session_id"] = session_id.into();
                value["info"]["id"] = session_id.into();
                value["info"]["cwd"] = root.to_str().expect("utf-8 root").into();
                value["info"]["pid"] = child_pid.into();
                value["info"]["agent"] = previous.agent.into();
                value["recovery"]["agent"] = previous.agent.into();
                value["recovery"]["session_id"] = session_id.into();
                value["recovery"]["cwd"] = root.to_str().expect("utf-8 root").into();
                value["runtime"] = serde_json::json!({
                    "state": "live",
                    "worker_id": worker_id,
                    "runtime_id": worker_instance_id,
                    "service_id": format!("{session_id}.{GENERATION}"),
                    "generation": GENERATION,
                    "executable": WORKER_EXECUTABLE,
                });
                // The resume binding is persisted beside the record from the
                // same snapshot of the session's recovery.
                let mut resume = value["recovery"].clone();
                resume["kind"] = "resume".into();
                lines.push(resume.to_string());
                lines.push(value.to_string());
            }
            let data = root.join("data");
            super::create_private_dir(&data);
            let store_path = data.join("metadata.jsonl");
            std::fs::write(&store_path, lines.join("\n") + "\n").expect("write the store");
            std::fs::set_permissions(
                &store_path,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            )
            .expect("private store");
            crate::store::migrate_at_startup(&store_path).expect("migrate the store");
            store_path
        }

        /// The upgraded daemon over the same roots as the previous release.
        pub(in super::super) fn upgraded_registry(
            root: &Path,
            store_path: PathBuf,
            previous: &Previous,
            inspector: Arc<dyn ProcessInspector>,
        ) -> SessionRegistry {
            let agents = root.join("agents");
            std::fs::create_dir_all(&agents).expect("agents directory");
            for (name, body) in previous.profiles {
                std::fs::write(agents.join(format!("{name}.toml")), body).expect("write profile");
            }
            SessionRegistry::new_with_inspector(
                SessionRegistryConfig {
                    shell_command: hermetic_shell(),
                    store_path: Some(store_path),
                    agents_dir: Some(agents),
                    worker_runtime_root: Some(root.join("runtime/workers")),
                    worker_state_root: Some(root.join("state/workers")),
                    worker_connect_deadline: Duration::from_millis(300),
                    procwatch_poll: Duration::from_mins(1),
                    ..SessionRegistryConfig::default()
                },
                inspector,
            )
        }

        pub(in super::super) async fn runtime_state(
            registry: &SessionRegistry,
            id: &SessionId,
        ) -> SessionInfo {
            registry.inspect(id).await.expect("session is listed")
        }

        /// The previous-release shapes a live worker can have been left with:
        /// the migration restores the launch spec of a built-in agent and of a
        /// profile the registry still resolves, and cannot restore it for a
        /// deleted profile or one that no longer declares native resume.
        const SHAPES: [Previous; 5] = [
            Previous {
                generation: "v0.33.0",
                fixture: V0_33_0_STORE,
                agent: "claude",
                profiles: &[],
            },
            Previous {
                generation: "v0.33.1-damaged",
                fixture: V0_33_1_DAMAGED_STORE,
                agent: "claude",
                profiles: &[],
            },
            Previous {
                generation: "v0.33.1-damaged",
                fixture: V0_33_1_DAMAGED_STORE,
                agent: "work",
                profiles: &[("work", "base = \"claude\"\nprogram = \"claude\"\n")],
            },
            Previous {
                generation: "v0.33.1-damaged",
                fixture: V0_33_1_DAMAGED_STORE,
                agent: "deleted-profile",
                profiles: &[],
            },
            Previous {
                generation: "v0.33.1-damaged",
                fixture: V0_33_1_DAMAGED_STORE,
                agent: "no-resume-profile",
                profiles: &[(
                    "no-resume-profile",
                    "base = \"claude\"\nprogram = \"claude\"\n[resume]\nresumable = false\n",
                )],
            },
        ];

        /// Shows that `registry` serves the adopted session `id`: its screen
        /// shows the PTY, typed input reaches it, and a stop ends it.
        async fn assert_usable(registry: &SessionRegistry, id: &SessionId, label: &str) {
            wait_until(&format!("{label}: the screen shows the PTY"), || async {
                let screen = registry.screen(id).await.ok()?;
                screen
                    .visible_lines
                    .iter()
                    .any(|line| line.contains("ready"))
                    .then_some(())
            })
            .await;
            registry
                .input(protocol::SessionInputParams {
                    session_id: id.clone(),
                    text: "n1-typed-marker".to_owned(),
                    wait: None,
                })
                .await
                .unwrap_or_else(|error| panic!("{label}: input is delivered: {error:?}"));
            wait_until(
                &format!("{label}: the typed text reaches the PTY"),
                || async {
                    let screen = registry.screen(id).await.ok()?;
                    screen
                        .visible_lines
                        .iter()
                        .any(|line| line.contains("n1-typed-marker"))
                        .then_some(())
                },
            )
            .await;
            let stopped = registry
                .stop(id)
                .await
                .unwrap_or_else(|error| panic!("{label}: stop works: {error:?}"));
            assert!(stopped.stopped, "{label}");
            let session = runtime_state(registry, id).await;
            assert_eq!(session.state, protocol::SessionState::Stopped, "{label}");
        }

        #[tokio::test]
        async fn a_live_worker_started_by_the_previous_release_is_adopted_and_usable() {
            for previous in SHAPES {
                let label = format!("{} / {}", previous.generation, previous.agent);
                let root = temp_root();
                let runtime_root = root.join("runtime/workers");
                let session_id = "s-522";
                let worker = spawn_launch_worker(
                    &root,
                    &runtime_root,
                    session_id,
                    "worker-n1",
                    STORED_REFERENCE,
                )
                .await;
                let store_path = write_previous_store(
                    &root,
                    &previous,
                    session_id,
                    "worker-n1",
                    worker.worker_instance_id.as_str(),
                    worker.child_pid,
                );
                let registry = upgraded_registry(
                    &root,
                    store_path.clone(),
                    &previous,
                    Arc::new(RetryInspector::default()),
                );
                drop(worker.controller);

                Box::pin(registry.reconcile_workers())
                    .await
                    .expect("reconcile after the upgrade");

                let id = SessionId(session_id.to_owned());
                let session = runtime_state(&registry, &id).await;
                let runtime = session.runtime.expect("runtime");
                assert_eq!(
                    runtime.state,
                    RuntimeState::Live,
                    "{label}: adopted as live, not {:?}",
                    runtime.loss_reason
                );
                assert_eq!(
                    session.native_session_id.as_deref(),
                    Some(STORED_REFERENCE),
                    "{label}"
                );
                assert_usable(&registry, &id, &label).await;
                assert_eq!(
                    Store::new(store_path)
                        .load_sessions()
                        .expect("load store")
                        .into_iter()
                        .find(|record| record.session_id == session_id)
                        .map(|record| record.desired_state),
                    Some(crate::store::DesiredState::Stopped),
                    "{label}"
                );
                worker.server_task.abort();
                drop(worker.reporter);
            }
        }

        #[tokio::test]
        async fn a_contradicting_launch_reference_still_conflicts() {
            let logs = crate::runtime::lifecycle::tests::LogCapture::default();
            let _subscriber = logs.install();
            let previous = Previous {
                generation: "v0.33.1-damaged",
                fixture: V0_33_1_DAMAGED_STORE,
                agent: "deleted-profile",
                profiles: &[],
            };
            let root = temp_root();
            let runtime_root = root.join("runtime/workers");
            let worker = spawn_launch_worker(
                &root,
                &runtime_root,
                "s-522",
                "worker-n1",
                "native-contradicts-the-stored-one",
            )
            .await;
            let store_path = write_previous_store(
                &root,
                &previous,
                "s-522",
                "worker-n1",
                worker.worker_instance_id.as_str(),
                worker.child_pid,
            );
            let registry = upgraded_registry(
                &root,
                store_path,
                &previous,
                Arc::new(RetryInspector::default()),
            );
            drop(worker.controller);

            Box::pin(registry.reconcile_workers())
                .await
                .expect("reconcile after the upgrade");

            let runtime = runtime_state(&registry, &SessionId("s-522".to_owned()))
                .await
                .runtime
                .expect("runtime");
            assert_eq!(runtime.state, RuntimeState::Conflict);
            assert_eq!(
                runtime.loss_reason.as_deref(),
                Some("launch_identity_reference_mismatch")
            );
            let classified = super::warn_lines(&logs, "s-522")
                .into_iter()
                .filter(|line| {
                    line.contains("worker_id=worker-n1 ")
                        && line.contains("reason=launch_identity_reference_mismatch")
                })
                .count();
            assert_eq!(classified, 1, "{}", logs.text());
            worker.server_task.abort();
        }
    }

    /// What startup reconciliation does with one worker and record, against
    /// what the upgrade preflight says about the same files.
    #[expect(
        clippy::struct_excessive_bools,
        reason = "each flag is one independent column of the case table"
    )]
    struct Differential {
        name: &'static str,
        /// Whether the worker is initialized; an uninitialized one is still in
        /// its `bootstrap` phase.
        initialized: bool,
        edit_record: fn(&mut SessionRecord),
        edit_journal: fn(&mut serde_json::Value),
        /// Damages the store file after the record is written.
        damage_store: fn(&Path),
        /// Replaces the journal file with these bytes instead of editing it.
        journal_bytes: Option<&'static [u8]>,
        /// Whether the worker runs a `claude` agent that reported its active
        /// identity, so its journal carries one.
        reports_identity: bool,
        /// Whether the preflight must call the session adoptable.
        preflight_adoptable: bool,
        /// Whether reconciliation adopts the worker live.
        reconcile_live: bool,
    }

    fn untouched_record(_: &mut SessionRecord) {}

    /// Rebinds the record to the built-in `claude` runtime.
    fn claude_record(record: &mut SessionRecord) {
        record.info.agent = "claude".to_owned();
        record.info.agent_base = RuntimeRef::claude();
        let recovery = record.recovery.as_mut().expect("recovery binding");
        recovery.agent = "claude".to_owned();
        recovery.agent_base = RuntimeRef::claude();
    }

    /// Rebinds the record to a runtime no definition backs.
    fn unreadable_runtime_record(record: &mut SessionRecord) {
        let unknown = RuntimeRef::from_wire("acme-agent");
        record.info.agent = "acme-agent".to_owned();
        record.info.agent_base = unknown.clone();
        let recovery = record.recovery.as_mut().expect("recovery binding");
        recovery.agent = "acme-agent".to_owned();
        recovery.agent_base = unknown;
    }

    /// Drops the hook schema id the worker journaled.
    fn without_schema(journal: &mut serde_json::Value) {
        journal
            .as_object_mut()
            .expect("journal object")
            .remove("hook_schema");
    }

    /// Cuts the store file in the middle of its only line.
    fn truncate_store(path: &Path) {
        let body = std::fs::read_to_string(path).expect("read store");
        std::fs::write(path, &body[..body.len() / 2]).expect("truncate store");
    }
    fn untouched_journal(_: &mut serde_json::Value) {}

    /// The preflight is never more optimistic than reconciliation: it calls a
    /// session adoptable only when startup adopts its worker live. Each case
    /// runs the real reconcile over a real in-process worker, so a rule added
    /// to one side and not the other fails here.
    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "one table drives every case through the same real worker fixture"
    )]
    async fn the_upgrade_preflight_is_never_more_optimistic_than_reconciliation() {
        use super::upgrade_preflight::{run, PreflightInputs};
        use pohunek_service_config::preflight::Verdict;

        let cases = [
            Differential {
                name: "an intact initialized worker",
                initialized: true,
                edit_record: untouched_record,
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: true,
                reconcile_live: true,
            },
            Differential {
                name: "a record bound to another runtime instance",
                initialized: true,
                edit_record: |record| {
                    record.runtime.worker_instance_id = Some("other-instance".to_owned());
                },
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a record bound to another worker",
                initialized: true,
                edit_record: |record| record.runtime.worker_id = Some("other-worker".to_owned()),
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a record without a worker generation",
                initialized: true,
                edit_record: |record| {
                    record.runtime.service_id = None;
                    record.runtime.generation = None;
                    record.runtime.executable = None;
                },
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a recovery that waits for another worker",
                initialized: true,
                edit_record: |record| {
                    record.transaction = Some(crate::store::SessionTransaction {
                        id: "recover-diff".to_owned(),
                        kind: crate::store::TransactionKind::Recover,
                        phase: "starting".to_owned(),
                        previous_worker_id: Some(DIFFERENTIAL_WORKER.to_owned()),
                        previous_worker_instance_id: None,
                        daemon_instance_id: None,
                    });
                },
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a worker that never initialized",
                initialized: false,
                edit_record: |record| record.runtime.worker_instance_id = None,
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a stop intent the new daemon finishes",
                initialized: true,
                edit_record: |record| record.desired_state = DesiredState::Stopped,
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a removal intent the new daemon finishes",
                initialized: true,
                edit_record: |record| record.desired_state = DesiredState::Removed,
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a create whose initial input was never delivered",
                initialized: true,
                edit_record: |record| {
                    record.transaction = Some(crate::store::SessionTransaction {
                        id: "create-diff".to_owned(),
                        kind: crate::store::TransactionKind::Create,
                        phase: crate::store::INITIAL_INPUT_PHASE.to_owned(),
                        previous_worker_id: None,
                        previous_worker_instance_id: None,
                        daemon_instance_id: Some("previous-daemon".to_owned()),
                    });
                },
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a worker journal that cannot be read",
                initialized: true,
                edit_record: untouched_record,
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: Some(b"not json"),
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a worker journal of an unsupported schema",
                initialized: true,
                edit_record: untouched_record,
                edit_journal: |journal| journal["schema_version"] = serde_json::json!(3),
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a store line damaged beyond loading",
                initialized: true,
                edit_record: untouched_record,
                edit_journal: untouched_journal,
                damage_store: truncate_store,
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a damaged store line and an unreadable journal",
                initialized: true,
                edit_record: untouched_record,
                edit_journal: untouched_journal,
                damage_store: truncate_store,
                journal_bytes: Some(b"not json"),
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: false,
            },
            Differential {
                name: "a built-in runtime that reported its active identity",
                initialized: true,
                edit_record: claude_record,
                edit_journal: untouched_journal,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: true,
                preflight_adoptable: true,
                reconcile_live: true,
            },
            // A worker that journals no schema is validated with the schema its
            // runtime declares.
            Differential {
                name: "a built-in runtime whose journal carries no hook schema",
                initialized: true,
                edit_record: claude_record,
                edit_journal: without_schema,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: true,
                preflight_adoptable: true,
                reconcile_live: true,
            },
            // The definition of an unresolvable runtime cannot supply a schema;
            // reconciliation still holds the live worker's own.
            Differential {
                name: "a runtime whose definition cannot be read and a journal without a schema",
                initialized: true,
                edit_record: unreadable_runtime_record,
                edit_journal: without_schema,
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: true,
                preflight_adoptable: false,
                reconcile_live: true,
            },
            // The preflight reads the journal only; reconciliation asks the
            // live worker, so it stays more pessimistic here.
            Differential {
                name: "a launch identity process outside the PTY tree",
                initialized: true,
                edit_record: untouched_record,
                edit_journal: |journal| {
                    let parent = std::os::unix::process::parent_id();
                    let identity = crate::procwatch::HostInspector::new()
                        .identity(parent)
                        .expect("inspect the parent")
                        .expect("the parent runs");
                    journal["launch_identity"] = serde_json::json!({
                        "provider": "shell",
                        "process": {
                            "pid": identity.pid,
                            "process_group": 0,
                            "start_identity": identity.start_identity.get().to_string(),
                        },
                        "reference_kind": "id",
                        "native_reference": "native-diff",
                    });
                },
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: true,
            },
            Differential {
                name: "a journal whose protocol range this daemon cannot serve",
                initialized: true,
                edit_record: untouched_record,
                edit_journal: |journal| {
                    journal["protocol_minimum"] = serde_json::json!(1);
                    journal["protocol_maximum"] = serde_json::json!(2);
                },
                damage_store: |_| {},
                journal_bytes: None,
                reports_identity: false,
                preflight_adoptable: false,
                reconcile_live: true,
            },
        ];

        for case in cases {
            let root = temp_root();
            let runtime_root = root.join("runtime/workers");
            let session_id = DIFFERENTIAL_SESSION;
            let (instance, child_pid, controller, task) = if case.reports_identity {
                let (script, reporters) = identity_reporters(&root, &["agent"]);
                let command = format!(
                    "{} printf ready; while :; do sleep 1; done",
                    reporters[0].launch(&script)
                );
                let launch = LaunchIdentity {
                    agent: "claude".to_owned(),
                    agent_base: "claude".to_owned(),
                    reference_kind: None,
                };
                let (controller, instance, child_pid, task) = spawn_initialized_worker_with_launch(
                    &root,
                    &runtime_root,
                    session_id,
                    DIFFERENTIAL_WORKER,
                    launch,
                    PathBuf::from("/bin/sh"),
                    command,
                )
                .await;
                wait_for_directory(&reporters[0].inbox).await;
                let child = controller
                    .inspect()
                    .await
                    .expect("inspect initialized worker")
                    .child_process
                    .expect("worker child identity");
                let expires_at = (OffsetDateTime::now_utc() + time::Duration::seconds(30))
                    .format(&Rfc3339)
                    .expect("format identity expiry");
                assert!(
                    send_identity_hook_from(
                        &reporters[0],
                        serde_json::json!({
                            "type": "identity_report",
                            "runtime_id": instance.as_str(),
                            "provider": "claude",
                            "pid": child.pid,
                            "start_identity": child.start_identity,
                            "sequence": 1,
                            "expires_at": expires_at,
                            "reference_kind": null,
                            "native_reference": null
                        }),
                    )
                    .await,
                    "the worker accepts a report from inside the managed PTY"
                );
                (Some(instance), child_pid, Some(controller), task)
            } else if case.initialized {
                let (controller, instance, child_pid, task) =
                    spawn_initialized_worker(&root, &runtime_root, session_id, DIFFERENTIAL_WORKER)
                        .await;
                (Some(instance), child_pid, Some(controller), task)
            } else {
                let (_socket, task) = spawn_uninitialized_worker(
                    &root,
                    &runtime_root,
                    session_id,
                    session_id,
                    DIFFERENTIAL_WORKER,
                )
                .await;
                (None, 0, None, task)
            };
            let state_root = root.join("state/workers");
            for private in [
                root.join("state"),
                state_root.clone(),
                state_root.join(session_id),
            ] {
                std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))
                    .expect("private journal directories");
            }

            let mut record = identity_record();
            record.session_id = session_id.to_owned();
            record.info.id = SessionId(session_id.to_owned());
            record.info.agent = "shell".to_owned();
            record.info.agent_base = RuntimeRef::shell();
            record.info.cwd = root.clone();
            record.info.pid = child_pid;
            let info_runtime = record.info.runtime.as_mut().expect("runtime info");
            info_runtime.worker_id = Some(DIFFERENTIAL_WORKER.to_owned());
            info_runtime.worker_instance_id = instance.as_ref().map(ToString::to_string);
            record.runtime.worker_id = Some(DIFFERENTIAL_WORKER.to_owned());
            record.runtime.worker_instance_id = instance.as_ref().map(ToString::to_string);
            let recovery = record.recovery.as_mut().expect("recovery binding");
            recovery.session_id = session_id.to_owned();
            recovery.agent = "shell".to_owned();
            recovery.agent_base = RuntimeRef::shell();
            recovery.cwd = root.clone();
            recovery.program = "/bin/sh".to_owned();
            recovery.args = vec!["-c".to_owned(), "sleep 30".to_owned()];
            bind_test_generation(&mut record);
            (case.edit_record)(&mut record);
            let store_path = root.join("data/metadata.jsonl");
            Store::new(store_path.clone())
                .record_session(&record)
                .expect("persist logical record");
            let journal_path = state_root
                .join(session_id)
                .join(format!("{DIFFERENTIAL_WORKER}.json"));
            (case.damage_store)(&store_path);
            if let Some(bytes) = case.journal_bytes {
                std::fs::write(&journal_path, bytes).expect("damage journal");
            } else {
                let mut journal: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&journal_path).expect("read journal"))
                        .expect("journal json");
                (case.edit_journal)(&mut journal);
                std::fs::write(&journal_path, journal.to_string()).expect("write journal");
            }

            let report = run(
                &PreflightInputs {
                    store_path: store_path.clone(),
                    worker_state_root: state_root.clone(),
                    plugins_dir: root.join("state/plugins"),
                },
                &crate::procwatch::HostInspector::new(),
            );
            let verdict = report
                .sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(|session| session.verdict);
            // The worker serves one controller at a time.
            drop(controller);
            let registry = SessionRegistry::new(SessionRegistryConfig {
                shell_command: hermetic_shell(),
                store_path: Some(store_path),
                worker_runtime_root: Some(runtime_root),
                worker_state_root: Some(state_root),
                worker_connect_deadline: Duration::from_millis(300),
                procwatch_poll: Duration::from_secs(60),
                ..SessionRegistryConfig::default()
            });
            Box::pin(registry.reconcile_workers())
                .await
                .expect("reconcile");
            let runtime = registry
                .inspect(&SessionId(session_id.to_owned()))
                .await
                .ok()
                .and_then(|info| info.runtime);
            let live = runtime
                .as_ref()
                .is_some_and(|runtime| runtime.state == RuntimeState::Live);
            if let Some(entry) = registry
                .inner
                .sessions
                .lock()
                .await
                .get(&SessionId(session_id.to_owned()))
            {
                entry.procwatch_cancel.cancel();
            }
            task.abort();

            let adoptable = verdict == Some(Verdict::Adoptable);
            assert_eq!(live, case.reconcile_live, "{}: reconcile", case.name);
            assert_eq!(
                adoptable, case.preflight_adoptable,
                "{}: preflight {:?}",
                case.name, report.sessions
            );
            assert!(
                !adoptable || live,
                "{}: the preflight must not call adoptable what reconciliation does not adopt",
                case.name
            );
            assert!(
                live || report.at_risk().count() > 0,
                "{}: a session reconciliation does not adopt must be listed at risk: {:?}",
                case.name,
                report
            );
        }
    }

    /// A process table that cannot be read never makes a session adoptable.
    #[tokio::test]
    async fn the_upgrade_preflight_fails_closed_when_identity_processes_cannot_be_inspected() {
        use super::upgrade_preflight::{run, PreflightInputs};
        use pohunek_service_config::preflight::Verdict;

        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let session_id = DIFFERENTIAL_SESSION;
        let (_controller, instance, child_pid, task) =
            spawn_initialized_worker(&root, &runtime_root, session_id, DIFFERENTIAL_WORKER).await;
        let state_root = root.join("state/workers");
        for private in [
            root.join("state"),
            state_root.clone(),
            state_root.join(session_id),
        ] {
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))
                .expect("private journal directories");
        }
        let mut record = identity_record();
        record.session_id = session_id.to_owned();
        record.info.id = SessionId(session_id.to_owned());
        record.info.agent = "shell".to_owned();
        record.info.agent_base = RuntimeRef::shell();
        record.info.pid = child_pid;
        record.runtime.worker_id = Some(DIFFERENTIAL_WORKER.to_owned());
        record.runtime.worker_instance_id = Some(instance.to_string());
        let recovery = record.recovery.as_mut().expect("recovery binding");
        recovery.session_id = session_id.to_owned();
        recovery.agent = "shell".to_owned();
        recovery.agent_base = RuntimeRef::shell();
        bind_test_generation(&mut record);
        let store_path = root.join("data/metadata.jsonl");
        Store::new(store_path.clone())
            .record_session(&record)
            .expect("persist record");
        let journal_path = state_root
            .join(session_id)
            .join(format!("{DIFFERENTIAL_WORKER}.json"));
        let mut journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal_path).expect("read journal"))
                .expect("journal json");
        // The launch process is the PTY root itself, so a readable table passes.
        journal["launch_identity"] = serde_json::json!({
            "provider": "shell",
            "process": journal["child"].clone(),
            "reference_kind": "id",
            "native_reference": "native-diff",
        });
        std::fs::write(&journal_path, journal.to_string()).expect("write journal");
        let inputs = PreflightInputs {
            store_path,
            worker_state_root: state_root,
            plugins_dir: root.join("state/plugins"),
        };
        let verdict = |inspector: &dyn ProcessInspector| {
            run(&inputs, inspector)
                .sessions
                .into_iter()
                .find(|session| session.session_id == session_id)
                .expect("the session is judged")
        };

        let readable = verdict(&RetryInspector::default());
        let unreadable_table = RetryInspector::default();
        unreadable_table.fail_descendants(true);
        let unreadable = verdict(&unreadable_table);
        task.abort();

        assert!(
            !readable.code.starts_with("identity_process_") && !readable.code.contains("child"),
            "{readable:?}"
        );
        assert_eq!(unreadable.verdict, Verdict::WouldNotBeAdopted);
        assert_eq!(unreadable.code, "identity_process_inspection_failed");
    }

    /// Session of the differential cases.
    const DIFFERENTIAL_SESSION: &str = "s-301";
    /// Worker of the differential cases.
    const DIFFERENTIAL_WORKER: &str = "worker-differential";
}
