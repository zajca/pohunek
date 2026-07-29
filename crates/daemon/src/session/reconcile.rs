//! Startup adoption of durable session workers.

use std::collections::HashMap;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pohunek_worker_protocol::{ControlCode, InspectSnapshot, RuntimePhase};
use protocol::{RuntimeInventoryEntry, RuntimeInventoryEvent, RuntimeInventoryStatus};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
    event, event_payload, runtime_error, timestamp_now, watch, ActiveAgentReport,
    CancellationToken, DesiredState, DetectorConfig, Notify, ObservedAgent, ProtocolError,
    ResumeSnapshot, RuntimeHandle, RuntimeState, SessionEntry, SessionId, SessionRecord,
    SessionRef, SessionRefKind, SessionRegistry, SessionRuntime, SessionState, StateSource, Worker,
    WorkerError, WORKER_CONNECT_RETRY,
};
use crate::session::target::open_detector_output;

// Rust guideline compliant 2026-07-29

#[derive(Debug, Clone)]
struct DiscoveredWorker {
    slot: String,
    worker: Worker,
    snapshot: InspectSnapshot,
}

#[derive(Debug, Clone, Deserialize)]
struct JournalEvidence {
    session_id: String,
    worker_id: String,
    runtime_id: Option<String>,
    child: Option<JournalChild>,
    cols: Option<u16>,
    rows: Option<u16>,
    phase: JournalPhase,
    outcome: Option<JournalOutcome>,
}

#[derive(Debug, Clone, Deserialize)]
struct JournalChild {
    pid: u32,
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

impl SessionRegistry {
    /// Loads logical records and adopts their exact surviving workers.
    ///
    /// This never invokes provider-native resume. An absent, conflicting, or
    /// incompatible runtime remains visible with an explicit runtime state.
    ///
    /// # Errors
    ///
    /// Returns a store error when logical records cannot be loaded. Individual
    /// worker failures are classified per session and do not abort startup.
    #[expect(
        clippy::too_many_lines,
        reason = "startup reconciliation walks discovery, journal replay, and per-record adoption in one linear pass"
    )]
    pub async fn reconcile_workers(&self) -> Result<(), ProtocolError> {
        let Some(store) = self.inner.store.clone() else {
            return Ok(());
        };
        let import_store = Arc::clone(&store);
        tokio::task::spawn_blocking(move || import_legacy_manifest(&import_store))
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
        let mut resume_bindings = resume_bindings
            .into_iter()
            .map(|binding| (binding.session_id.clone(), binding))
            .collect::<HashMap<_, _>>();

        let (mut discovered, inventory) = self.discover_workers(&records).await;
        let mut journals = self.discover_worker_journals().await;
        for entry in inventory
            .iter()
            .filter(|entry| entry.status != RuntimeInventoryStatus::Managed)
        {
            let event = protocol::Event::new(
                event::SESSION_RUNTIME_DISCOVERED,
                event_payload(RuntimeInventoryEvent {
                    entry: entry.clone(),
                }),
            );
            let _ = self.inner.events.send(event);
        }
        *self.inner.runtime_inventory.lock().await = inventory;

        for mut record in records {
            if let Some(binding) = resume_bindings.remove(&record.session_id) {
                if let Err(reason) = merge_persisted_recovery(&mut record, binding) {
                    self.insert_unavailable_record(record, RuntimeState::Conflict, reason)
                        .await;
                    continue;
                }
            }
            let candidates = discovered.remove(&record.session_id).unwrap_or_default();
            match candidates.as_slice() {
                [candidate] if candidate.slot == record.session_id => {
                    self.reconcile_record(
                        record,
                        Some((candidate.worker.clone(), candidate.snapshot.clone())),
                    )
                    .await;
                }
                [] => {
                    match classify_terminal_journals(
                        journals.remove(&record.session_id).unwrap_or_default(),
                        &record,
                    ) {
                        TerminalJournalClassification::Exact(evidence) => {
                            self.import_terminal_journal(record, evidence).await;
                            continue;
                        }
                        TerminalJournalClassification::Conflict => {
                            self.insert_unavailable_record(
                                record,
                                RuntimeState::Conflict,
                                "worker_journal_identity_mismatch",
                            )
                            .await;
                            continue;
                        }
                        TerminalJournalClassification::Absent => {}
                    }
                    let inventory_state = self.inventory_state_for_slot(&record.session_id).await;
                    if inventory_state.is_none()
                        && record.transaction.as_ref().is_some_and(|transaction| {
                            transaction.kind == crate::store::TransactionKind::Create
                        })
                    {
                        if let Err(error) = self
                            .delete_session_record(&SessionId(record.session_id.clone()))
                            .await
                        {
                            tracing::warn!(
                                session_id = %record.session_id,
                                error = %error,
                                "failed to compensate abandoned preparing session"
                            );
                        }
                        continue;
                    }
                    let state = inventory_state.unwrap_or(RuntimeState::Lost);
                    let reason = match state {
                        RuntimeState::Incompatible => "worker_protocol_incompatible",
                        RuntimeState::Conflict => "runtime_identity_mismatch",
                        _ => "worker_unavailable",
                    };
                    self.insert_unavailable_record(record, state, reason).await;
                }
                _ => {
                    self.insert_unavailable_record(
                        record,
                        RuntimeState::Conflict,
                        "multiple_worker_candidates",
                    )
                    .await;
                }
            }
        }
        Ok(())
    }

    async fn discover_worker_journals(&self) -> HashMap<String, Vec<JournalEvidence>> {
        let Some(state_root) = self.inner.config.worker_state_root.clone() else {
            return HashMap::new();
        };
        match tokio::task::spawn_blocking(move || scan_worker_journals(&state_root)).await {
            Ok(Ok(journals)) => journals,
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "failed to scan durable worker journals");
                HashMap::new()
            }
            Err(error) => {
                tracing::warn!(error = %error, "durable worker journal scan task panicked");
                HashMap::new()
            }
        }
    }

    async fn import_terminal_journal(&self, mut record: SessionRecord, evidence: JournalEvidence) {
        record.transaction = None;
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
        record.runtime.runtime_id.clone_from(&evidence.runtime_id);
        record.runtime.reason = None;
        record.info.runtime = Some(SessionRuntime {
            state: RuntimeState::Terminal,
            worker_id: Some(evidence.worker_id),
            runtime_id: evidence.runtime_id,
            started_at: record
                .info
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.started_at.clone()),
            last_connected_at: None,
            loss_reason: None,
        });
        self.insert_unavailable_record(record, RuntimeState::Terminal, "worker_journal_terminal")
            .await;
    }

    async fn inventory_state_for_slot(&self, slot: &str) -> Option<RuntimeState> {
        self.inner
            .runtime_inventory
            .lock()
            .await
            .iter()
            .find(|entry| {
                entry.runtime_slot == slot
                    || entry
                        .claimed_session_id
                        .as_deref()
                        .is_some_and(|id| id == slot)
            })
            .map(|entry| match entry.status {
                RuntimeInventoryStatus::Incompatible => RuntimeState::Incompatible,
                RuntimeInventoryStatus::Conflict
                | RuntimeInventoryStatus::IdentityMismatch
                | RuntimeInventoryStatus::Orphaned => RuntimeState::Conflict,
                RuntimeInventoryStatus::Managed => RuntimeState::Reconnecting,
            })
    }

    async fn reconcile_record(
        &self,
        mut record: SessionRecord,
        candidate: Option<(Worker, InspectSnapshot)>,
    ) {
        let id = SessionId(record.session_id.clone());
        let Some((worker, snapshot)) = candidate else {
            self.insert_unavailable_record(record, RuntimeState::Lost, "worker_unavailable")
                .await;
            return;
        };

        let identity_conflict = record
            .runtime
            .worker_id
            .as_deref()
            .is_some_and(|expected| expected != snapshot.worker_id.as_str())
            || record
                .runtime
                .runtime_id
                .as_deref()
                .zip(snapshot.runtime_id.as_ref())
                .is_some_and(|(expected, actual)| expected != actual.as_str());
        if identity_conflict {
            self.insert_unavailable_record(
                record,
                RuntimeState::Conflict,
                "runtime_identity_mismatch",
            )
            .await;
            return;
        }
        if record.transaction.as_ref().is_some_and(|transaction| {
            transaction.kind == crate::store::TransactionKind::Recover
                && transaction.previous_worker_id.as_deref() == Some(snapshot.worker_id.as_str())
        }) {
            self.insert_unavailable_record(
                record,
                RuntimeState::Lost,
                "worker_generation_not_advanced",
            )
            .await;
            return;
        }

        if record.desired_state == DesiredState::Removed {
            self.finish_reconciled_removal(&id, worker, &record).await;
            return;
        }

        if !matches!(
            snapshot.phase,
            RuntimePhase::Running | RuntimePhase::Starting
        ) {
            if record.transaction.as_ref().is_some_and(|transaction| {
                transaction.kind == crate::store::TransactionKind::Create
            }) {
                if let Err(error) = self.delete_session_record(&id).await {
                    tracing::warn!(
                        session_id = %id.0,
                        error = %error,
                        "failed to compensate non-live preparing session"
                    );
                }
                return;
            }
            let state = if snapshot.phase == RuntimePhase::Exited {
                RuntimeState::Terminal
            } else {
                RuntimeState::Lost
            };
            if let Some(exit) = snapshot.exit {
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
                .await;
            return;
        }

        if record.desired_state == DesiredState::Stopped {
            self.finish_reconciled_stop(&id, worker, record).await;
            return;
        }

        self.adopt_live_record(id, worker, record, snapshot).await;
    }

    async fn discover_workers(
        &self,
        records: &[SessionRecord],
    ) -> (
        HashMap<String, Vec<DiscoveredWorker>>,
        Vec<RuntimeInventoryEntry>,
    ) {
        let Some(runtime_root) = self.inner.config.worker_runtime_root.clone() else {
            return (HashMap::new(), Vec::new());
        };
        let slots = match tokio::task::spawn_blocking(move || discover_runtime_slots(&runtime_root))
            .await
        {
            Ok(Ok(slots)) => slots,
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "failed to enumerate durable worker runtime root");
                return (HashMap::new(), Vec::new());
            }
            Err(error) => {
                tracing::warn!(error = %error, "durable worker discovery task panicked");
                return (HashMap::new(), Vec::new());
            }
        };

        let mut candidates = Vec::new();
        let mut inventory = Vec::new();
        for (slot, socket) in slots {
            match self.connect_discovered_worker(&socket).await {
                Ok(worker) => match worker.inspect().await {
                    Ok(snapshot) => candidates.push(DiscoveredWorker {
                        slot,
                        worker,
                        snapshot,
                    }),
                    Err(error) => inventory.push(discovery_failure_entry(slot, &error)),
                },
                Err(error) => inventory.push(discovery_failure_entry(slot, &error)),
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
                runtime_id: candidate
                    .snapshot
                    .runtime_id
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
        (grouped, inventory)
    }

    async fn connect_discovered_worker(&self, socket_path: &Path) -> Result<Worker, WorkerError> {
        let controller_deadline =
            tokio::time::Instant::now() + self.inner.config.worker_connect_deadline;
        loop {
            match Worker::connect_discovered(socket_path, self.daemon_instance_id()).await {
                Ok(worker) => return Ok(worker),
                Err(
                    error @ WorkerError::Rejected {
                        code: ControlCode::ControllerBusy,
                        ..
                    },
                ) if tokio::time::Instant::now() < controller_deadline => {
                    tracing::debug!(
                        path = %socket_path.display(),
                        error = %error,
                        "waiting for the previous daemon discovery lease to close"
                    );
                    tokio::time::sleep(WORKER_CONNECT_RETRY).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "worker adoption reconstructs one complete observer-backed session entry"
    )]
    async fn adopt_live_record(
        &self,
        id: SessionId,
        worker: Worker,
        mut record: SessionRecord,
        snapshot: pohunek_worker_protocol::InspectSnapshot,
    ) {
        let native_recovery = record.transaction.as_ref().and_then(|transaction| {
            (transaction.kind == crate::store::TransactionKind::Recover)
                .then(|| transaction.previous_runtime_id.clone())
        });
        let active_agent = match import_worker_identities(&mut record, &snapshot) {
            Ok(active_agent) => active_agent,
            Err(reason) => {
                self.insert_unavailable_record(record, RuntimeState::Conflict, reason)
                    .await;
                return;
            }
        };
        let Some(child) = snapshot.child_process else {
            self.insert_unavailable_record(record, RuntimeState::Lost, "worker_child_missing")
                .await;
            return;
        };
        let detector_output = match open_detector_output(&worker, &id).await {
            Ok(output) => output,
            Err(error) => {
                self.insert_unavailable_record(
                    record,
                    RuntimeState::Reconnecting,
                    error.code.as_str(),
                )
                .await;
                return;
            }
        };
        let now = timestamp_now();
        let runtime_id = snapshot.runtime_id.as_ref().map(ToString::to_string);
        record.info.pid = child.pid;
        if let Some(dimensions) = snapshot.dimensions {
            record.info.cols = dimensions.columns();
            record.info.rows = dimensions.rows();
        }
        record.info.state = SessionState::Running;
        record.info.state_source = StateSource::Process;
        record.info.runtime = Some(SessionRuntime {
            state: RuntimeState::Live,
            worker_id: Some(snapshot.worker_id.to_string()),
            runtime_id: runtime_id.clone(),
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
        record.runtime.runtime_id = runtime_id;
        record.runtime.reason = None;

        let recovery = record.recovery.clone();
        let input_rules = recovery.as_ref().map_or_else(
            || super::input_rules_for_agent(record.info.agent_base, &self.inner.config),
            |binding| binding.input_rules.to_input_rules(),
        );
        let snapshot = recovery.as_ref().map_or_else(
            || ResumeSnapshot {
                program: String::new(),
                args: Vec::new(),
                resume: None,
            },
            |binding| ResumeSnapshot {
                program: binding.program.clone(),
                args: binding.args.clone(),
                resume: binding
                    .resume_mode
                    .zip(binding.ref_kind)
                    .map(|(mode, ref_kind)| super::ResumeTemplate { mode, ref_kind }),
            },
        );
        let manifest_override = self
            .inner
            .profiles
            .resolve_agent(&record.info.agent)
            .ok()
            .and_then(|resolved| resolved.profile.and_then(|profile| profile.manifest));
        let default_detector_config =
            DetectorConfig::for_profile(record.info.agent_base, manifest_override);
        let detector_cancel = CancellationToken::new();
        let procwatch_cancel = CancellationToken::new();
        let runtime_watch_cancel = CancellationToken::new();
        let procwatch_rescan = Arc::new(Notify::new());
        let (detector_resize, detector_resize_rx) =
            watch::channel((record.info.rows, record.info.cols));
        let (detector_config, detector_config_rx) = watch::channel(default_detector_config.clone());
        let info = record.info.clone();
        {
            let mut sessions = self.inner.sessions.lock().await;
            sessions.insert(
                id.clone(),
                SessionEntry {
                    info: info.clone(),
                    runtime: RuntimeHandle::Worker(worker.clone()),
                    desired_state: DesiredState::Running,
                    detector_cancel: detector_cancel.clone(),
                    detector_resize,
                    detector_config,
                    default_detector_config,
                    procwatch_cancel: procwatch_cancel.clone(),
                    runtime_watch_cancel: runtime_watch_cancel.clone(),
                    procwatch_rescan: Arc::clone(&procwatch_rescan),
                    stopping: false,
                    input_rules,
                    snapshot,
                    active_agent: active_agent.clone(),
                    last_agent_report: active_agent,
                    observed_agents: Vec::<ObservedAgent>::new(),
                },
            )
        };
        if let Err(error) = self.write_session_record(record).await {
            tracing::warn!(session_id = %id.0, error = %error, "failed to commit reconciled worker");
        }
        self.spawn_detector(
            id.clone(),
            detector_output,
            (info.rows, info.cols),
            detector_cancel,
            detector_resize_rx,
            detector_config_rx,
        );
        self.spawn_procwatch(id.clone(), child.pid, procwatch_cancel, procwatch_rescan);
        self.spawn_worker_exit_watcher(id, worker, runtime_watch_cancel);
        if let Some(previous_runtime_id) = native_recovery {
            self.emit_native_recovered(&info, previous_runtime_id);
        } else {
            self.emit(event::SESSION_RUNTIME_RECONNECTED, &info);
        }
    }

    async fn insert_unavailable_record(
        &self,
        mut record: SessionRecord,
        state: RuntimeState,
        reason: &str,
    ) {
        let id = SessionId(record.session_id.clone());
        let now = timestamp_now();
        let runtime = record.info.runtime.get_or_insert(SessionRuntime {
            state,
            worker_id: record.runtime.worker_id.clone(),
            runtime_id: record.runtime.runtime_id.clone(),
            started_at: None,
            last_connected_at: None,
            loss_reason: Some(reason.to_owned()),
        });
        runtime.state = state;
        runtime.loss_reason = (state != RuntimeState::Terminal).then(|| reason.to_owned());
        record.runtime.state = state;
        record.runtime.reason = runtime.loss_reason.clone();
        record.info.updated_at = now;
        let recovery = record.recovery.clone();
        let input_rules = recovery.as_ref().map_or_else(
            || super::input_rules_for_agent(record.info.agent_base, &self.inner.config),
            |binding| binding.input_rules.to_input_rules(),
        );
        let relaunch = recovery.as_ref().map_or_else(
            || ResumeSnapshot {
                program: String::new(),
                args: Vec::new(),
                resume: None,
            },
            |binding| ResumeSnapshot {
                program: binding.program.clone(),
                args: binding.args.clone(),
                resume: binding
                    .resume_mode
                    .zip(binding.ref_kind)
                    .map(|(mode, ref_kind)| super::ResumeTemplate { mode, ref_kind }),
            },
        );
        let default_detector_config = DetectorConfig::for_agent(record.info.agent_base);
        let (detector_resize, _) = watch::channel((record.info.rows, record.info.cols));
        let (detector_config, _) = watch::channel(default_detector_config.clone());
        let info = record.info.clone();
        {
            let mut sessions = self.inner.sessions.lock().await;
            sessions.insert(
                id.clone(),
                SessionEntry {
                    info: info.clone(),
                    runtime: RuntimeHandle::Unavailable(state),
                    desired_state: record.desired_state,
                    detector_cancel: CancellationToken::new(),
                    detector_resize,
                    detector_config,
                    default_detector_config,
                    procwatch_cancel: CancellationToken::new(),
                    runtime_watch_cancel: CancellationToken::new(),
                    procwatch_rescan: Arc::new(Notify::new()),
                    stopping: false,
                    input_rules,
                    snapshot: relaunch,
                    active_agent: None,
                    last_agent_report: None,
                    observed_agents: Vec::new(),
                },
            )
        };
        if let Err(error) = self.write_session_record(record).await {
            tracing::warn!(session_id = %id.0, error = %error, "failed to persist runtime classification");
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
    }

    async fn finish_reconciled_stop(&self, id: &SessionId, worker: Worker, record: SessionRecord) {
        let transaction_id = record
            .transaction
            .as_ref()
            .map_or_else(|| format!("stop-reconcile-{}", id.0), |tx| tx.id.clone());
        let transaction = match pohunek_worker_protocol::TransactionId::new(transaction_id) {
            Ok(transaction) => transaction,
            Err(error) => {
                self.insert_unavailable_record(
                    record,
                    RuntimeState::Conflict,
                    &format!("invalid_stop_transaction:{error}"),
                )
                .await;
                return;
            }
        };
        match worker.stop(transaction).await {
            Ok(Some(exit)) => {
                let mut terminal = record;
                terminal.transaction = None;
                terminal.info.state = SessionState::Stopped;
                terminal.info.exit_code = exit.code;
                terminal
                    .info
                    .runtime
                    .get_or_insert(SessionRuntime {
                        state: RuntimeState::Terminal,
                        worker_id: terminal.runtime.worker_id.clone(),
                        runtime_id: terminal.runtime.runtime_id.clone(),
                        started_at: None,
                        last_connected_at: None,
                        loss_reason: None,
                    })
                    .state = RuntimeState::Terminal;
                self.insert_unavailable_record(
                    terminal,
                    RuntimeState::Terminal,
                    "worker_runtime_terminal",
                )
                .await;
            }
            Ok(None) | Err(_) => {
                self.insert_unavailable_record(
                    record,
                    RuntimeState::Reconnecting,
                    "stop_reconciliation_pending",
                )
                .await;
            }
        }
    }

    async fn finish_reconciled_removal(
        &self,
        id: &SessionId,
        worker: Worker,
        record: &SessionRecord,
    ) {
        let transaction_id = record
            .transaction
            .as_ref()
            .map_or_else(|| format!("remove-reconcile-{}", id.0), |tx| tx.id.clone());
        let Ok(transaction) = pohunek_worker_protocol::TransactionId::new(transaction_id) else {
            return;
        };
        if worker.stop(transaction).await.is_ok() {
            if let Err(error) = self.cleanup_owned_worktrees_for_removal(id).await {
                tracing::warn!(
                    session_id = %id.0,
                    error = %error,
                    "failed to finish reconciled worktree removal"
                );
                return;
            }
            if let Err(error) = self.delete_session_record(id).await {
                tracing::warn!(session_id = %id.0, error = %error, "failed to finish reconciled removal");
            }
        }
    }
}

fn merge_persisted_recovery(
    record: &mut SessionRecord,
    binding: crate::store::ResumeBinding,
) -> Result<(), &'static str> {
    if binding.agent != record.info.agent || binding.agent_base != record.info.agent_base {
        return Err("resume_binding_agent_mismatch");
    }
    if let Some(recovery) = record.recovery.as_ref() {
        if recovery.agent != binding.agent
            || recovery.agent_base != binding.agent_base
            || recovery.ref_kind != binding.ref_kind
        {
            return Err("resume_binding_shape_mismatch");
        }
    }

    let kind = binding.ref_kind.or_else(|| {
        record
            .recovery
            .as_ref()
            .and_then(|recovery| recovery.ref_kind)
    });
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
) -> Result<Option<ActiveAgentReport>, &'static str> {
    if let Some(identity) = &snapshot.launch_identity {
        let expected_provider = super::agent_kind_label(record.info.agent_base);
        if identity.provider != expected_provider {
            return Err("launch_identity_provider_mismatch");
        }
        let kind = parse_reference_kind(&identity.reference_kind)
            .ok_or("launch_identity_reference_kind_invalid")?;
        let binding = record
            .recovery
            .as_mut()
            .ok_or("launch_identity_recovery_missing")?;
        if binding.ref_kind != Some(kind) {
            return Err("launch_identity_reference_kind_mismatch");
        }
        let native = validate_native_reference(kind, &identity.native_reference)
            .ok_or("launch_identity_reference_invalid")?;
        match kind {
            SessionRefKind::Id => {
                if record
                    .info
                    .native_session_id
                    .as_deref()
                    .is_some_and(|existing| existing != native.as_str())
                {
                    return Err("launch_identity_reference_mismatch");
                }
                record.info.native_session_id = Some(native.clone());
                record.info.native_session_path = None;
                binding.native_session_id = Some(native);
                binding.native_session_path = None;
            }
            SessionRefKind::Path => {
                if record
                    .info
                    .native_session_path
                    .as_deref()
                    .is_some_and(|existing| existing != native.as_str())
                {
                    return Err("launch_identity_reference_mismatch");
                }
                record.info.native_session_path = Some(native.clone());
                record.info.native_session_id = None;
                binding.native_session_path = Some(native);
                binding.native_session_id = None;
            }
        }
    }

    let Some(identity) = &snapshot.active_identity else {
        return Ok(None);
    };
    let Some(agent_base) = parse_provider(&identity.provider) else {
        return Err("active_identity_provider_invalid");
    };
    let (active_id, active_path) = match (
        identity.reference_kind.as_deref(),
        identity.native_reference.as_deref(),
    ) {
        (Some(kind), Some(reference)) => {
            let kind =
                parse_reference_kind(kind).ok_or("active_identity_reference_kind_invalid")?;
            let native = validate_native_reference(kind, reference)
                .ok_or("active_identity_reference_invalid")?;
            match kind {
                SessionRefKind::Id => (Some(native), None),
                SessionRefKind::Path => (None, Some(native)),
            }
        }
        (None, None) => (None, None),
        _ => return Err("active_identity_reference_incomplete"),
    };
    record.info.active_agent = Some(identity.provider.clone());
    record.info.active_agent_base = Some(agent_base);
    record.info.active_agent_pid = Some(identity.process.pid);
    record.info.active_agent_session_id = active_id;
    record.info.active_agent_session_path = active_path;
    Ok(Some(ActiveAgentReport {
        source: format!("worker:{}", snapshot.worker_id),
        agent: identity.provider.clone(),
        seq: Some(identity.sequence),
        pid: Some(identity.process.pid),
        reported_at: std::time::Instant::now(),
        activity_reported: false,
    }))
}

fn parse_provider(provider: &str) -> Option<protocol::AgentKind> {
    match provider {
        "shell" => Some(protocol::AgentKind::Shell),
        "codex" => Some(protocol::AgentKind::Codex),
        "claude" => Some(protocol::AgentKind::Claude),
        _ => None,
    }
}

fn parse_reference_kind(kind: &str) -> Option<SessionRefKind> {
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

#[expect(
    clippy::too_many_lines,
    reason = "one-time migration validation and import remain atomic and auditable together"
)]
fn import_legacy_manifest(store: &crate::store::Store) -> Result<(), ProtocolError> {
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
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
        return Ok(());
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
    if actual_fingerprint != manifest.store_sha256 {
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
            worker_id: None,
            runtime_id: None,
            started_at: None,
            last_connected_at: None,
            loss_reason: live.then(|| "legacy_runtime_not_transferable".to_owned()),
        });
        let recovery = resume
            .iter()
            .find(|binding| binding.session_id == info.id.0)
            .cloned();
        store
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
                runtime: crate::store::RuntimeRecord {
                    state: runtime_state,
                    worker_id: None,
                    runtime_id: None,
                    unit_name: None,
                    reason: live.then(|| "legacy_runtime_not_transferable".to_owned()),
                },
            })
            .map_err(|error| {
                runtime_error(
                    "migration_import_failed",
                    format!("failed to import logical session: {error}"),
                )
            })?;
    }
    archive_imported_manifest(&path, &manifest)
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
    Exact(JournalEvidence),
    Conflict,
    Absent,
}

fn classify_terminal_journals(
    journals: Vec<JournalEvidence>,
    record: &SessionRecord,
) -> TerminalJournalClassification {
    let mut terminal = journals
        .into_iter()
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
                .runtime_id
                .as_deref()
                .is_none_or(|expected| journal.runtime_id.as_deref() == Some(expected))
    });
    match terminal.as_slice() {
        [journal] => TerminalJournalClassification::Exact(journal.clone()),
        [] | [_, _, ..] => TerminalJournalClassification::Conflict,
    }
}

fn scan_worker_journals(
    state_root: &Path,
) -> std::io::Result<HashMap<String, Vec<JournalEvidence>>> {
    let mut journals = HashMap::<String, Vec<JournalEvidence>>::new();
    let root_metadata = match std::fs::symlink_metadata(state_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(journals),
        Err(error) => return Err(error),
    };
    validate_discovery_path(state_root, &root_metadata, true)?;
    for session_entry in std::fs::read_dir(state_root)? {
        let session_entry = session_entry?;
        if session_entry.file_type()?.is_symlink() || !session_entry.file_type()?.is_dir() {
            continue;
        }
        let session_path = session_entry.path();
        let session_metadata = session_entry.metadata()?;
        if validate_discovery_path(&session_path, &session_metadata, true).is_err() {
            continue;
        }
        let Some(session_id) = session_entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        for journal_entry in std::fs::read_dir(&session_path)? {
            let journal_entry = journal_entry?;
            let path = journal_entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink()
                || !metadata.file_type().is_file()
                || metadata.uid() != effective_uid()
                || metadata.mode() & 0o077 != 0
            {
                continue;
            }
            let evidence = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<JournalEvidence>(&bytes).ok())
                .unwrap_or_else(|| JournalEvidence {
                    session_id: session_id.clone(),
                    worker_id: String::new(),
                    runtime_id: None,
                    child: None,
                    cols: None,
                    rows: None,
                    phase: JournalPhase::Faulted,
                    outcome: None,
                });
            journals
                .entry(session_id.clone())
                .or_default()
                .push(evidence);
        }
    }
    Ok(journals)
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

fn persisted_identity_mismatch(record: &SessionRecord, snapshot: &InspectSnapshot) -> bool {
    record
        .runtime
        .worker_id
        .as_deref()
        .is_some_and(|expected| expected != snapshot.worker_id.as_str())
        || record
            .runtime
            .runtime_id
            .as_deref()
            .zip(snapshot.runtime_id.as_ref())
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
        runtime_id: None,
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
        WorkerError::Socket { .. }
        | WorkerError::Protocol(_)
        | WorkerError::Rejected { .. }
        | WorkerError::NotInitialized
        | WorkerError::AttachReadyTimeout { .. } => (RuntimeState::Lost, "worker_unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use pohunek_session_worker::{
        ChildIdentity as JournalChildIdentity, Journal, JournalRecord,
        RuntimeOutcome as JournalRuntimeOutcome, RuntimePhase as JournalRuntimePhase, Server,
        ServerArgs, WorkerConfig,
    };
    use pohunek_worker_protocol::{
        ActiveIdentityClaim, ControlCode, ControlError, ControlMessage, ControlReader,
        ControlResponse, ControlWriter, Dimensions, Initialize, InitializeLimits, InspectSnapshot,
        LaunchIdentity, ProcessIdentity, ReportedLaunchIdentity, ResponseKind, RuntimeId,
        RuntimePhase as WorkerRuntimePhase, SecretEnv, SessionId as WorkerSessionId, StopPolicy,
        TransactionId, Version, WorkerId,
    };
    use protocol::{
        AgentKind, CwdSource, ForkCwdMode, RuntimeInventoryStatus, RuntimeState, SessionForkParams,
        SessionId, SessionInfo, SessionNewParams, SessionRuntime, SessionState, StateSource,
    };
    use sha2::{Digest, Sha256};
    use tokio::net::UnixListener;

    use super::{import_legacy_manifest, import_worker_identities, SessionRegistry};
    use crate::agent::{InputRules, ResumeMode, SessionRefKind};
    use crate::session::SessionRegistryConfig;
    use crate::store::{
        DesiredState, ResumeBinding, RuntimeRecord, SessionRecord, Store, StoredInputRules,
    };

    fn temp_root() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("pohunek-reconcile-{}-{nanos}", std::process::id()))
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
        assert_eq!(record.info.active_agent_base, Some(AgentKind::Claude));
        assert_eq!(record.info.active_agent_pid, Some(60));
        assert_eq!(
            record.info.active_agent_session_id.as_deref(),
            Some("nested-native")
        );
        assert_eq!(active.expect("active report").pid, Some(60));

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

    #[test]
    fn terminal_journal_identity_mismatch_and_duplicates_fail_closed() {
        let record = identity_record();
        let exact = super::JournalEvidence {
            session_id: record.session_id.clone(),
            worker_id: record.runtime.worker_id.clone().expect("worker id"),
            runtime_id: record.runtime.runtime_id.clone(),
            child: Some(super::JournalChild { pid: 50 }),
            cols: Some(80),
            rows: Some(24),
            phase: super::JournalPhase::Terminal,
            outcome: Some(super::JournalOutcome {
                exit_code: Some(0),
                signal: None,
                success: true,
            }),
        };
        let mut mismatch = exact.clone();
        mismatch.worker_id = "different-worker".to_owned();

        assert!(matches!(
            super::classify_terminal_journals(vec![mismatch], &record),
            super::TerminalJournalClassification::Conflict
        ));
        assert!(matches!(
            super::classify_terminal_journals(vec![exact.clone(), exact], &record),
            super::TerminalJournalClassification::Conflict
        ));
    }

    #[test]
    fn codex_and_claude_same_provider_nested_identity_keeps_launch_reference_immutable() {
        for (provider, agent_base, kind, launch_reference, nested_reference) in [
            (
                "codex",
                AgentKind::Codex,
                SessionRefKind::Id,
                "codex-launch",
                "codex-nested",
            ),
            (
                "claude",
                AgentKind::Claude,
                SessionRefKind::Path,
                "/tmp/claude-launch.jsonl",
                "/tmp/claude-nested.jsonl",
            ),
        ] {
            let mut record = identity_record();
            record.info.agent = provider.to_owned();
            record.info.agent_base = agent_base;
            let binding = record.recovery.as_mut().expect("recovery");
            binding.agent = provider.to_owned();
            binding.agent_base = agent_base;
            binding.ref_kind = Some(kind);
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
        reason = "restart continuity test keeps original and replacement registry assertions together"
    )]
    async fn replacement_registry_preserves_native_recovery_and_fork_binding() {
        let root = temp_root();
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
        let runtime_id = first_controller
            .initialize(Initialize {
                session_id: wire_session_id,
                transaction_id: TransactionId::new("create-restart-test").expect("transaction id"),
                expected_worker_id: wire_worker_id.clone(),
                launch: LaunchIdentity {
                    agent: "claude".to_owned(),
                    agent_base: "claude".to_owned(),
                    reference_kind: Some("id".to_owned()),
                },
                executable: PathBuf::from("/bin/sh"),
                arguments: vec!["-c".to_owned(), "printf ready; sleep 30".to_owned()],
                cwd: root.clone(),
                dimensions: Dimensions::new(80, 24).expect("dimensions"),
                environment: SecretEnv::new(BTreeMap::new()).expect("environment"),
                limits: InitializeLimits::new(1_000_000, 100_000, 128, 60_000).expect("limits"),
                stop_policy: StopPolicy::new(100).expect("stop policy"),
                hook_protocol_version: Version::new(1).expect("version"),
                public_protocol_version: protocol::PROTOCOL_VERSION.get(),
            })
            .await
            .expect("initialize worker");
        let before = first_controller.inspect().await.expect("inspect before");
        let child_pid = before.child_process.expect("child identity").pid;

        let created_at = "2026-07-23T00:00:00Z".to_owned();
        let info = SessionInfo {
            id: SessionId(session_id.to_owned()),
            external: Some(false),
            name: Some("restart continuity".to_owned()),
            agent: "claude".to_owned(),
            agent_base: AgentKind::Claude,
            cwd: root.clone(),
            cwd_source: Some(CwdSource::Launch),
            pid: child_pid,
            runtime: Some(SessionRuntime {
                state: RuntimeState::Live,
                worker_id: Some(worker_id.to_owned()),
                runtime_id: Some(runtime_id.to_string()),
                started_at: Some(created_at.clone()),
                last_connected_at: Some(created_at.clone()),
                loss_reason: None,
            }),
            cols: 80,
            rows: 24,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: None,
            native_session_path: None,
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
        };
        let store_path = root.join("data/metadata.jsonl");
        let store = Store::new(store_path.clone());
        let stale_recovery = ResumeBinding {
            session_id: session_id.to_owned(),
            name: Some("restart continuity".to_owned()),
            agent: "claude".to_owned(),
            agent_base: AgentKind::Claude,
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
            input_rules: StoredInputRules::from(InputRules {
                bracketed_paste: false,
                submit_delay: Duration::ZERO,
            }),
            resume_mode: Some(ResumeMode::Flag),
            ref_kind: Some(SessionRefKind::Id),
            resumable: true,
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
                    previous_runtime_id: None,
                }),
                info: SessionInfo {
                    pid: 0,
                    state: SessionState::Starting,
                    runtime: Some(SessionRuntime {
                        state: RuntimeState::Starting,
                        worker_id: None,
                        runtime_id: None,
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
                    runtime_id: None,
                    unit_name: Some("pohunek-session@s-91.service".to_owned()),
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

        let replacement = SessionRegistry::new(SessionRegistryConfig {
            store_path: Some(store_path),
            worker_runtime_root: Some(runtime_root),
            ..SessionRegistryConfig::default()
        });
        replacement
            .reconcile_workers()
            .await
            .expect("reconcile replacement daemon");
        let adopted = replacement
            .inspect(&SessionId(session_id.to_owned()))
            .await
            .expect("inspect adopted session");
        assert_eq!(adopted.pid, child_pid);
        assert_eq!(
            adopted
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.runtime_id.as_deref()),
            Some(runtime_id.as_str())
        );
        assert_eq!(
            adopted.native_session_id.as_deref(),
            Some("native-restart-test"),
            "replacement daemon must merge the latest persisted native binding"
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
            })
            .await
            .expect("fork adopted session from preserved native binding");
        assert_ne!(forked.id, SessionId(session_id.to_owned()));
        assert_eq!(
            forked.native_session_id.as_deref(),
            Some("native-restart-test")
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
            store_path: Some(store_path),
            ..SessionRegistryConfig::default()
        });
        replacement
            .reconcile_workers()
            .await
            .expect("replacement daemon reconciles store");
        let created = replacement
            .create(SessionNewParams {
                name: None,
                agent: "shell".to_owned(),
                cwd: Some(root),
                cols: 80,
                rows: 24,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
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
        record.runtime.runtime_id = None;
        record.transaction = Some(crate::store::SessionTransaction {
            id: "create-s-204".to_owned(),
            kind: crate::store::TransactionKind::Create,
            phase: "preparing".to_owned(),
            previous_worker_id: None,
            previous_runtime_id: None,
        });
        record.recovery.as_mut().expect("recovery").session_id = "s-204".to_owned();
        Store::new(store_path.clone())
            .record_session(&record)
            .expect("persist preparing record");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            store_path: Some(store_path.clone()),
            worker_runtime_root: Some(root.join("runtime/workers")),
            worker_state_root: Some(root.join("state/workers")),
            ..SessionRegistryConfig::default()
        });

        registry
            .reconcile_workers()
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
    async fn reconciliation_imports_terminal_journal_after_worker_retention_exit() {
        let root = temp_root();
        let store_path = root.join("data/metadata.jsonl");
        let state_root = root.join("state/workers");
        let mut logical = identity_record();
        logical.session_id = "s-205".to_owned();
        logical.info.id = SessionId("s-205".to_owned());
        logical.runtime.worker_id = Some("worker-terminal".to_owned());
        logical.runtime.runtime_id = Some("runtime-terminal".to_owned());
        logical.recovery.as_mut().expect("recovery").session_id = "s-205".to_owned();
        Store::new(store_path.clone())
            .record_session(&logical)
            .expect("persist running logical record");

        std::fs::create_dir_all(&state_root).expect("create state root");
        std::fs::set_permissions(&state_root, std::fs::Permissions::from_mode(0o700))
            .expect("private state root");
        let mut journal = JournalRecord::bootstrap(
            "s-205".to_owned(),
            "worker-terminal".to_owned(),
            41,
            "410".to_owned(),
            (1, 2),
            "2026-07-23T00:00:00Z".to_owned(),
        );
        journal.runtime_id = Some("runtime-terminal".to_owned());
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
        Journal::new(state_root.join("s-205/worker-terminal.json"))
            .write(&journal)
            .expect("persist terminal journal");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            store_path: Some(store_path),
            worker_runtime_root: Some(root.join("runtime/workers")),
            worker_state_root: Some(state_root),
            ..SessionRegistryConfig::default()
        });

        registry
            .reconcile_workers()
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
    }

    #[tokio::test]
    async fn discovery_quarantines_orphan_without_stopping_worker() {
        let root = temp_root();
        let runtime_root = root.join("runtime/workers");
        let (socket, server_task) =
            spawn_uninitialized_worker(&root, &runtime_root, "s-201", "s-201", "worker-o").await;
        let registry = empty_registry(&root, &runtime_root);
        let mut events = registry.subscribe();

        registry.reconcile_workers().await.expect("discover orphan");

        let event = events.recv().await.expect("orphan discovery event");
        assert_eq!(event.event, protocol::event::SESSION_RUNTIME_DISCOVERED);
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

        registry
            .reconcile_workers()
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

        registry
            .reconcile_workers()
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

        registry
            .reconcile_workers()
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
        let (controller, runtime_id, child_pid, server_task) =
            spawn_initialized_worker(&root, &runtime_root, "s-206", "worker-stop-replay").await;
        persist_live_record(
            &root,
            "s-206",
            "worker-stop-replay",
            runtime_id.as_str(),
            child_pid,
            DesiredState::Stopped,
            crate::store::TransactionKind::Stop,
        );
        drop(controller);
        let registry = empty_registry(&root, &runtime_root);

        registry.reconcile_workers().await.expect("replay stop");

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
        let (controller, runtime_id, child_pid, server_task) =
            spawn_initialized_worker(&root, &runtime_root, "s-207", "worker-remove-replay").await;
        persist_live_record(
            &root,
            "s-207",
            "worker-remove-replay",
            runtime_id.as_str(),
            child_pid,
            DesiredState::Removed,
            crate::store::TransactionKind::Remove,
        );
        drop(controller);
        let registry = empty_registry(&root, &runtime_root);

        registry.reconcile_workers().await.expect("replay removal");

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
        let (initial, runtime_id, child_pid, server_task) =
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
            assert_eq!(snapshot.runtime_id.as_ref(), Some(&runtime_id));
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

    fn empty_registry(root: &std::path::Path, runtime_root: &std::path::Path) -> SessionRegistry {
        SessionRegistry::new(SessionRegistryConfig {
            store_path: Some(root.join("data/metadata.jsonl")),
            worker_runtime_root: Some(runtime_root.to_path_buf()),
            worker_state_root: Some(root.join("state/workers")),
            worker_connect_deadline: Duration::from_millis(300),
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
        RuntimeId,
        u32,
        tokio::task::JoinHandle<()>,
    ) {
        let (socket, task) =
            spawn_uninitialized_worker(root, runtime_root, session_id, session_id, worker_id).await;
        let controller = crate::runtime::Worker::connect(&socket, session_id, "crash-replay-setup")
            .await
            .expect("connect setup controller");
        let runtime_id = controller
            .initialize(Initialize {
                session_id: WorkerSessionId::new(session_id).expect("session id"),
                transaction_id: TransactionId::new(format!("create-{session_id}"))
                    .expect("transaction id"),
                expected_worker_id: controller.worker_id().await,
                launch: LaunchIdentity {
                    agent: "shell".to_owned(),
                    agent_base: "shell".to_owned(),
                    reference_kind: None,
                },
                executable: PathBuf::from("/bin/sh"),
                arguments: vec!["-c".to_owned(), "sleep 30".to_owned()],
                cwd: root.to_path_buf(),
                dimensions: Dimensions::new(80, 24).expect("dimensions"),
                environment: SecretEnv::new(BTreeMap::new()).expect("environment"),
                limits: InitializeLimits::new(1_000_000, 100_000, 128, 60_000).expect("limits"),
                stop_policy: StopPolicy::new(100).expect("stop policy"),
                hook_protocol_version: Version::new(1).expect("version"),
                public_protocol_version: protocol::PROTOCOL_VERSION.get(),
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
        (controller, runtime_id, child_pid, task)
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
        record.runtime.runtime_id = None;
        record.info.runtime.as_mut().expect("runtime").worker_id = Some(worker_id.to_owned());
        record.info.runtime.as_mut().expect("runtime").runtime_id = None;
        record.recovery.as_mut().expect("recovery").session_id = id.to_owned();
        Store::new(root.join("data/metadata.jsonl"))
            .record_session(&record)
            .expect("persist logical record");
    }

    fn persist_live_record(
        root: &std::path::Path,
        id: &str,
        worker_id: &str,
        runtime_id: &str,
        child_pid: u32,
        desired_state: DesiredState,
        transaction_kind: crate::store::TransactionKind,
    ) {
        let mut record = identity_record();
        record.session_id = id.to_owned();
        record.info.id = SessionId(id.to_owned());
        record.info.pid = child_pid;
        record.info.agent = "shell".to_owned();
        record.info.agent_base = AgentKind::Shell;
        record.info.runtime.as_mut().expect("runtime").worker_id = Some(worker_id.to_owned());
        record.info.runtime.as_mut().expect("runtime").runtime_id = Some(runtime_id.to_owned());
        record.runtime.worker_id = Some(worker_id.to_owned());
        record.runtime.runtime_id = Some(runtime_id.to_owned());
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
            previous_runtime_id: None,
        });
        let recovery = record.recovery.as_mut().expect("recovery");
        recovery.session_id = id.to_owned();
        recovery.agent = "shell".to_owned();
        recovery.agent_base = AgentKind::Shell;
        recovery.program = "/bin/sh".to_owned();
        recovery.args = vec!["-c".to_owned(), "sleep 30".to_owned()];
        Store::new(root.join("data/metadata.jsonl"))
            .record_session(&record)
            .expect("persist live intent record");
    }

    fn identity_record() -> SessionRecord {
        let created_at = "2026-07-23T00:00:00Z".to_owned();
        SessionRecord {
            schema_version: 1,
            session_id: "s-identity".to_owned(),
            desired_state: DesiredState::Running,
            transaction: None,
            info: SessionInfo {
                id: SessionId("s-identity".to_owned()),
                external: Some(false),
                name: None,
                agent: "codex".to_owned(),
                agent_base: AgentKind::Codex,
                cwd: PathBuf::from("/repo"),
                cwd_source: Some(CwdSource::Launch),
                pid: 50,
                runtime: Some(SessionRuntime {
                    state: RuntimeState::Live,
                    worker_id: Some("worker-identity".to_owned()),
                    runtime_id: Some("runtime-identity".to_owned()),
                    started_at: Some(created_at.clone()),
                    last_connected_at: Some(created_at.clone()),
                    loss_reason: None,
                }),
                cols: 80,
                rows: 24,
                state: SessionState::Running,
                state_source: StateSource::Process,
                activity: None,
                active_agent: None,
                active_agent_base: None,
                active_agent_pid: None,
                active_agent_session_id: None,
                active_agent_session_path: None,
                native_session_id: None,
                native_session_path: None,
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
            },
            recovery: Some(ResumeBinding {
                session_id: "s-identity".to_owned(),
                name: None,
                agent: "codex".to_owned(),
                agent_base: AgentKind::Codex,
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
                resume_mode: None,
                ref_kind: Some(SessionRefKind::Id),
                resumable: true,
            }),
            runtime: RuntimeRecord {
                state: RuntimeState::Live,
                worker_id: Some("worker-identity".to_owned()),
                runtime_id: Some("runtime-identity".to_owned()),
                unit_name: Some("pohunek-session@s-identity.service".to_owned()),
                reason: None,
            },
        }
    }

    fn identity_snapshot(native_launch: &str) -> InspectSnapshot {
        InspectSnapshot {
            session_id: WorkerSessionId::new("s-identity").expect("session id"),
            worker_id: WorkerId::new("worker-identity").expect("worker id"),
            runtime_id: Some(RuntimeId::new("runtime-identity").expect("runtime id")),
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
                expires_at: "2026-07-23T00:00:30Z".to_owned(),
                reference_kind: Some("id".to_owned()),
                native_reference: Some("nested-native".to_owned()),
            }),
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
            agent_base: AgentKind::Shell,
            cwd: PathBuf::from("/repo"),
            cwd_source: Some(CwdSource::Launch),
            pid: 0,
            runtime: None,
            cols: 80,
            rows: 24,
            state,
            state_source: StateSource::Process,
            activity: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: None,
            native_session_path: None,
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
        std::fs::create_dir_all(&data_dir).expect("create data dir");
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
            agent_base: AgentKind::Codex,
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
            resume_mode: None,
            ref_kind: Some(SessionRefKind::Id),
            resumable: true,
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
    fn import_legacy_manifest_fingerprint_mismatch_fails_closed() {
        let root = temp_root();
        let data_dir = root.join("data");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
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
    fn import_legacy_manifest_live_loss_not_accepted_fails_closed() {
        let root = temp_root();
        let data_dir = root.join("data");
        std::fs::create_dir_all(&data_dir).expect("create data dir");
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
        std::fs::create_dir_all(&data_dir).expect("create data dir");
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
        std::fs::create_dir_all(&data_dir).expect("create data dir");
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
        std::fs::create_dir_all(&data_dir).expect("create data dir");
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
}
