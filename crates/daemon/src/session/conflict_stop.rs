//! Stopping and removing a session whose runtime is unavailable.
//!
//! A runtime that is `lost`, `reconnecting` or `incompatible` has no worker
//! connection to stop through, and a stop refuses it before anything is
//! written. Before anything a stop or a removal uses is retired, the
//! journal of the recorded generation must name the recorded worker and the
//! supervisor's job must be that worker's — this submodule only supports a
//! `conflict`, the one state that can prove its recorded generation this
//! way. A stop ends the session `stopped`; a removal persists its Remove
//! intent before retirement and releases the session with the caller's
//! cleanup choice. A stop interrupted after the job was retired is finished
//! by reconciliation from the persisted stop intent, and a removal
//! interrupted the same way by the persisted Remove intent.

// Rust guideline compliant 2026-06-26

use protocol::{RuntimeState, SessionId, SessionRemoveResult, SessionStopResult};
use tracing::warn;

use super::supervision::UNREADABLE_CANDIDATES_RECOVER;
use super::{
    is_terminal, runtime_error, runtime_state_label, session_not_found, unavailable_runtime_code,
    unavailable_runtime_error, DesiredState, ProtocolError, RuntimeExit, RuntimeHandle,
    RuntimeWatchIdentity, SessionEntry, SessionRegistry, SessionTransaction, TransactionKind,
    UnconfirmedCleanup,
};
use crate::procwatch::ProcessIdentity;
use crate::runtime::lifecycle::{observation_live, Generation, Lifecycle, PreviousLive};
use crate::store::SessionRecord;
use std::sync::atomic::Ordering;

/// Recovery hint of a stop refused because of unreadable-marker processes.
const UNREADABLE_CANDIDATES_STOP_RECOVER: &str =
    "inspect the listed processes and end the ones that belong to this session, then retry the stop";

/// Transaction phase of a Remove intent whose recorded identity
/// [`SessionRegistry::prove_conflicted_runtime`] already judged once, so
/// every replay ([`SessionRegistry::finish_removal_intent`]) re-proves it
/// before any retirement instead of trusting the job under the service ID.
pub(super) const PROVEN_CONFLICT_REMOVAL_PHASE: &str = "proven-conflict";

/// Whether `record` removes through a Remove intent whose recorded identity
/// was already proven once and must be re-proven on replay.
pub(super) fn is_conflict_proven_removal(record: &SessionRecord) -> bool {
    record.transaction.as_ref().is_some_and(|transaction| {
        transaction.kind == TransactionKind::Remove
            && transaction.phase == PROVEN_CONFLICT_REMOVAL_PHASE
    })
}

/// What a conflicted entry records about the runtime a stop would end.
struct ConflictedRuntime {
    generation: Generation,
    worker_id: String,
    worker_instance_id: Option<String>,
    expected: RuntimeWatchIdentity,
    previous_desired_state: DesiredState,
}

/// The refusal of a stop that cannot end a conflicted runtime because the
/// session `lacks` what the stop needs.
fn refusal(id: &SessionId, lacks: &str) -> ProtocolError {
    runtime_error(
        unavailable_runtime_code(RuntimeState::Conflict),
        format!(
            "session {} runtime is {} and {lacks}; its worker cannot be stopped by its recorded identity",
            id.0,
            runtime_state_label(RuntimeState::Conflict)
        ),
    )
}

impl ConflictedRuntime {
    /// Reads the recorded runtime of `entry`, or says what it lacks.
    fn of(id: &SessionId, entry: &SessionEntry) -> Result<Self, ProtocolError> {
        let refuse = |lacks: &str| refusal(id, lacks);
        let generation = entry
            .job
            .clone()
            .ok_or_else(|| refuse("its record names no worker generation"))?;
        let runtime = entry
            .info
            .runtime
            .as_ref()
            .ok_or_else(|| refuse("its record names no worker"))?;
        let worker_id = runtime
            .worker_id
            .clone()
            .ok_or_else(|| refuse("its record names no worker"))?;
        let expected = RuntimeWatchIdentity::from_info(&entry.info)
            .ok_or_else(|| refuse("its record names no complete runtime identity"))?;
        Ok(Self {
            generation,
            worker_id,
            worker_instance_id: runtime.worker_instance_id.clone(),
            expected,
            previous_desired_state: entry.desired_state,
        })
    }
}

impl SessionRegistry {
    /// Stops the session when its runtime is unavailable.
    ///
    /// Returns `None` when the session ended or its worker is connected, so
    /// the ordinary stop applies. The caller holds the session's lifecycle
    /// lock.
    ///
    /// # Errors
    ///
    /// Returns the runtime-state code of an unavailable runtime that is not a
    /// stoppable conflict, with nothing written, or the error of
    /// [`Self::stop_conflicted_runtime`].
    pub(super) async fn stop_unavailable_runtime(
        &self,
        id: &SessionId,
    ) -> Result<Option<SessionStopResult>, ProtocolError> {
        let target = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            if is_terminal(entry.info.state) {
                return Ok(None);
            }
            let RuntimeHandle::Unavailable(state) = entry.runtime else {
                return Ok(None);
            };
            if state != RuntimeState::Conflict {
                return Err(unavailable_runtime_error(id, state));
            }
            ConflictedRuntime::of(id, entry)?
        };
        Box::pin(self.stop_conflicted_runtime(id, target))
            .await
            .map(Some)
    }

    /// Stops the supervised job of a conflicted session by its recorded
    /// identity.
    ///
    /// Every refusal comes before the stop intent is written: the journal of
    /// the recorded generation must name the recorded worker, and the job
    /// under the generation's service id must be that worker's
    /// ([`Self::prove_conflicted_runtime`]). The intent is then persisted
    /// before anything is stopped, the job is retired and every journaled
    /// worker proven gone, and the marked processes of the runtimes are swept
    /// ([`Self::sweep_removed_runtimes`], never accepting unconfirmed
    /// cleanup). A retirement failure that leaves the workers proven alive
    /// beside a live job rolls the intent back, so the record stays as it was.
    /// Any other failure (a worker still running after the retirement, an
    /// unconfirmed sweep, a retirement error that proves nothing) keeps the intent,
    /// and the supervision retry finishes the stop once the worker is gone and
    /// the cleanup confirmed (a conflict between the record and its resume
    /// binding is never re-checked). Only after all of it does the session end
    /// `stopped`.
    ///
    /// # Errors
    ///
    /// Returns `runtime_identity_mismatch`, `runtime_supervision_ambiguous`
    /// or `runtime_supervision_unavailable` when the runtime cannot be proven
    /// the recorded one, retired, or gone, and `session_runtime_conflict` when
    /// the session lacks a recorded generation, a recorded worker, or a
    /// configured supervisor.
    async fn stop_conflicted_runtime(
        &self,
        id: &SessionId,
        target: ConflictedRuntime,
    ) -> Result<SessionStopResult, ProtocolError> {
        let lifecycle = self
            .lifecycle()
            .map_err(|_unsupervised| refusal(id, "no worker supervisor is configured"))?;
        self.prove_conflicted_runtime(
            id,
            &target.generation,
            &target.worker_id,
            target.worker_instance_id.as_deref(),
            &lifecycle,
        )
        .await?;

        let sequence = self.inner.next_write_id.fetch_add(1, Ordering::Relaxed);
        let transaction_id = format!("stop-{sequence}");
        let intent = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get_mut(id)
                .ok_or_else(|| session_not_found(&id.0))?;
            if !target.expected.matches(entry)
                || !matches!(entry.runtime, RuntimeHandle::Unavailable(_))
            {
                return Err(runtime_error(
                    "session_runtime_transition_busy",
                    format!("session {} changed while its stop was being proven", id.0),
                ));
            }
            entry.stopping = true;
            entry.stop_transaction_id = Some(transaction_id.clone());
            entry.desired_state = DesiredState::Stopped;
            Self::session_record(
                id,
                entry,
                DesiredState::Stopped,
                Some(SessionTransaction {
                    id: transaction_id.clone(),
                    kind: TransactionKind::Stop,
                    phase: "requested".to_owned(),
                    previous_worker_id: None,
                    previous_worker_instance_id: None,
                    daemon_instance_id: None,
                }),
            )
        };
        let (terminal_base, previous) = match self.commit_stop_intent(id, intent).await {
            Ok(committed) => committed,
            Err(error) => {
                self.rollback_stop_intent(
                    id,
                    target.previous_desired_state,
                    DesiredState::Stopped,
                    &transaction_id,
                    Some(&target.expected),
                )
                .await;
                return Err(error);
            }
        };

        if let Err(failure) = self
            .release_conflicted_runtime(id, &target, &lifecycle)
            .await
        {
            return Err(self
                .settle_failed_release(
                    id,
                    &target,
                    &transaction_id,
                    (terminal_base, previous),
                    failure,
                )
                .await);
        }
        // The job is retired and the workers are gone, so the stop intent is
        // kept when the terminal commit fails: reconciliation finishes it
        // from the intent once the runtime is proven ended.
        if let Err(error) = self
            .commit_user_stop_exit(
                id,
                RuntimeExit {
                    exit_code: None,
                    success: false,
                },
                &terminal_base,
            )
            .await
        {
            self.schedule_supervision_retry(id);
            return Err(error);
        }
        self.persist_resume_binding(id).await;
        Ok(SessionStopResult { stopped: true })
    }

    /// Settles a stop whose release failed and returns the error to report.
    ///
    /// `records` are the committed stop intent and the durable record it
    /// replaced.
    async fn settle_failed_release(
        &self,
        id: &SessionId,
        target: &ConflictedRuntime,
        transaction_id: &str,
        records: (SessionRecord, SessionRecord),
        failure: ReleaseFailure,
    ) -> ProtocolError {
        match failure {
            ReleaseFailure::NotStopped(error) => {
                self.rollback_stop_intent(
                    id,
                    target.previous_desired_state,
                    DesiredState::Stopped,
                    transaction_id,
                    Some(&target.expected),
                )
                .await;
                // Conditional on the intent still being the durable record, so
                // a classification the watch committed meanwhile is never
                // undone.
                let (terminal_base, previous) = records;
                if let Err(rollback) = self
                    .write_session_record_if_current(terminal_base, previous)
                    .await
                {
                    warn!(
                        session_id = %id.0,
                        error = %rollback,
                        "the stop intent of a failed stop was not rolled back"
                    );
                }
                self.schedule_supervision_retry(id);
                error
            }
            // The job is retired and its worker may be gone: the intent stays
            // durable, so reconciliation finishes the stop and repeats the
            // cleanup instead of classifying the runtime lost.
            ReleaseFailure::PartiallyStopped(error) => {
                self.clear_stopping(id, transaction_id, Some(&target.expected))
                    .await;
                self.schedule_supervision_retry(id);
                error
            }
        }
    }

    /// Removes a conflicted session when its runtime is available only for a
    /// proven removal.
    ///
    /// The caller has already established that the entry shows a
    /// `conflict` runtime with a recorded generation whose identity loss is
    /// not the supervisor's own ambiguity, so
    /// [`Self::prove_conflicted_runtime`] has the last word on the identity:
    /// a refusal leaves the durable record and every foreign job untouched.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::remove_conflicted_runtime`].
    pub(super) async fn remove_conflict_unavailable_runtime(
        &self,
        id: &SessionId,
        cleanup: UnconfirmedCleanup,
    ) -> Result<SessionRemoveResult, ProtocolError> {
        let target = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            ConflictedRuntime::of(id, entry)?
        };
        Box::pin(self.remove_conflicted_runtime(id, target, cleanup)).await
    }

    /// Removes a proven conflicted session by its recorded identity.
    ///
    /// Identity proof comes first ([`Self::prove_conflicted_runtime`]), so a
    /// refusal writes nothing. The Remove intent is then committed
    /// conditionally over the durable record (the exact `conflict` runtime is
    /// re-checked under the sessions lock first), the exact recorded
    /// generation is retired ([`Self::retire_generation_for_removal`]), and
    /// [`Self::release_removed_session`] sweeps the session's runtimes with
    /// the caller's `cleanup` choice, deletes everything the session owns,
    /// and the durable record last. A failed or stale intent commit restores
    /// the in-memory desired state and retires nothing; a retirement or
    /// release failure keeps the Remove intent durable, so startup
    /// reconciliation finishes the removal.
    ///
    /// # Errors
    ///
    /// Returns `runtime_identity_mismatch` or `session_runtime_transition_busy`
    /// when the runtime cannot be proven the recorded one with nothing
    /// written, `runtime_supervision_ambiguous` when the proof scan or the
    /// marker sweep of an accepting cleanup cannot prove the runtimes, and
    /// `runtime_supervision_unavailable` when the job cannot be inspected or
    /// retired.
    async fn remove_conflicted_runtime(
        &self,
        id: &SessionId,
        target: ConflictedRuntime,
        cleanup: UnconfirmedCleanup,
    ) -> Result<SessionRemoveResult, ProtocolError> {
        let lifecycle = self
            .lifecycle()
            .map_err(|_unsupervised| refusal(id, "no worker supervisor is configured"))?;
        Box::pin(self.prove_conflicted_runtime(
            id,
            &target.generation,
            &target.worker_id,
            target.worker_instance_id.as_deref(),
            &lifecycle,
        ))
        .await?;

        let sequence = self.inner.next_write_id.fetch_add(1, Ordering::Relaxed);
        let (intent, previous_desired_state) = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get_mut(id)
                .ok_or_else(|| session_not_found(&id.0))?;
            // The exact state the identity proof judged: a concurrent
            // classification or a changed runtime never accepts this intent.
            if !target.expected.matches(entry)
                || !matches!(
                    entry.runtime,
                    RuntimeHandle::Unavailable(RuntimeState::Conflict)
                )
            {
                return Err(runtime_error(
                    "session_runtime_transition_busy",
                    format!(
                        "session {} changed while its removal was being proven",
                        id.0
                    ),
                ));
            }
            let previous_desired_state = entry.desired_state;
            entry.desired_state = DesiredState::Removed;
            (
                Self::session_record(
                    id,
                    entry,
                    DesiredState::Removed,
                    Some(SessionTransaction {
                        id: format!("remove-{sequence}"),
                        kind: TransactionKind::Remove,
                        phase: PROVEN_CONFLICT_REMOVAL_PHASE.to_owned(),
                        previous_worker_id: None,
                        previous_worker_instance_id: None,
                        daemon_instance_id: None,
                    }),
                ),
                previous_desired_state,
            )
        };
        // Conditionally over the durable record, so a classification another
        // writer committed meanwhile is never silently overwritten.
        if let Err(error) = Box::pin(self.commit_removal_intent(id, intent)).await {
            self.rollback_removal_intent(id, previous_desired_state, Some(&target.expected))
                .await;
            return Err(error);
        }

        if let Err(error) =
            Box::pin(self.retire_generation_for_removal(id, &target.generation, &lifecycle)).await
        {
            // The Remove intent stays durable and the entry keeps showing its
            // conflict, so startup reconciliation and a retried
            // `session.remove` both finish the removal from fresh evidence.
            warn!(
                session_id = %id.0,
                service_id = %target.generation.service_id(),
                error = %error,
                "removal intent of a conflicted session waits for its generation to be retired"
            );
            return Err(error);
        }
        let released = Box::pin(self.release_removed_session(
            id,
            Some(&target.generation),
            target.worker_instance_id.as_deref(),
            cleanup,
        ))
        .await?;
        Ok(SessionRemoveResult {
            removed: released.evicted,
            stopped: true,
            worktrees_removed: u32::try_from(released.worktrees.removed).unwrap_or(u32::MAX),
            worktrees_failed: u32::try_from(released.worktrees.failed).unwrap_or(u32::MAX),
            accepted_unconfirmed_processes: released.accepted_unconfirmed,
        })
    }

    /// Retires the job of `target`'s generation, proves its workers gone, and
    /// sweeps the marked processes of its runtimes.
    ///
    /// The sweep covers only the runtimes the journals of the generation
    /// prove: [`Self::prove_conflicted_runtime`] required the recorded runtime
    /// to be one of them. A failure before the retirement was attempted, or
    /// one after which the workers are proven untouched, leaves the runtime
    /// as it was ([`ReleaseFailure::NotStopped`]); every other failure may
    /// follow an effective retirement ([`ReleaseFailure::PartiallyStopped`]).
    async fn release_conflicted_runtime(
        &self,
        id: &SessionId,
        target: &ConflictedRuntime,
        lifecycle: &Lifecycle<'_>,
    ) -> Result<(), ReleaseFailure> {
        let workers = self
            .removal_worker_processes(id, &target.generation)
            .await
            .map_err(ReleaseFailure::NotStopped)?;
        match lifecycle
            .retire_for_removal(&target.generation, &workers)
            .await
        {
            Ok(()) => {}
            // The supervisor may have stopped the job before it failed (a
            // settlement check, a definition cleanup), so only workers proven
            // alive beside a live job show the retirement had no effect.
            Err(PreviousLive::Unavailable(error)) => {
                return Err(
                    if self
                        .untouched(&target.generation, &workers, lifecycle)
                        .await
                    {
                        ReleaseFailure::NotStopped(error)
                    } else {
                        ReleaseFailure::PartiallyStopped(error)
                    },
                );
            }
            Err(PreviousLive::Ambiguous(error)) => {
                return Err(ReleaseFailure::PartiallyStopped(error))
            }
        }
        self.sweep_removed_runtimes(
            id,
            Some(&target.generation),
            None,
            UnconfirmedCleanup::Refuse,
        )
        .await
        .map(|_accepted| ())
        .map_err(|mut error| {
            if error.recover.as_deref() == Some(UNREADABLE_CANDIDATES_RECOVER) {
                error.recover = Some(UNREADABLE_CANDIDATES_STOP_RECOVER.to_owned());
            }
            ReleaseFailure::PartiallyStopped(error)
        })
    }

    /// Whether the job of `generation` still runs and every one of its
    /// `workers` is proven alive: the failed retirement changed nothing.
    ///
    /// Anything unproven (no worker to check, a job that cannot be inspected,
    /// absent, or not live) is not untouched.
    async fn untouched(
        &self,
        generation: &Generation,
        workers: &[ProcessIdentity],
        lifecycle: &Lifecycle<'_>,
    ) -> bool {
        if workers.is_empty()
            || !workers
                .iter()
                .all(|worker| matches!(self.inner.inspector.is_running(*worker), Ok(true)))
        {
            return false;
        }
        matches!(
            lifecycle.inspect(generation).await,
            Ok(Some(observation)) if observation_live(&observation)
        )
    }
}

/// Where a conflicted runtime's release failed.
enum ReleaseFailure {
    /// Nothing was stopped; the stop intent is rolled back.
    NotStopped(ProtocolError),
    /// The job was retired; the stop intent is kept.
    PartiallyStopped(ProtocolError),
}
