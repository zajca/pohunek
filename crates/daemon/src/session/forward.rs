//! Forwarding of bound public agent reports and releases into the worker.
//!
//! A bound `session.report_agent` or `session.release_agent` names its subject
//! process and carries a monotonic sequence. The worker validates and journals
//! it like a hook claim, and the daemon commits the claim the worker holds
//! through the same projection it applies to every worker snapshot. A report
//! that cannot be forwarded takes the memory-only path.

use std::sync::Arc;

use pohunek_worker_protocol::{
    ActiveIdentityOutcome, ForwardedActiveRelease, ForwardedActiveReport, InspectSnapshot,
    ProcessIdentity, ReportedSession, ReportedSessionKind, ACTIVE_IDENTITY_FORWARD_VERSION,
};
use protocol::{
    event, AgentActivity, SessionId, SessionReleaseAgentParams, SessionReleaseAgentResult,
    SessionReportAgentParams, SessionReportAgentResult,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tracing::{debug, info, warn};

use super::claim::{
    self, is_memory_report, owns, pending_state, ClaimKey, ClaimOrigin, PendingActivity,
    PendingRelease, PendingState, PublicOrder, Unacknowledged,
};
use super::{
    event_payload, is_terminal, timestamp_now, IdentityFence, NotDurableCause, RuntimeHandle,
    SessionRegistry, WorkerMetadataApplyOutcome,
};
use crate::agent::ResolvedAgent;
use crate::runtime::{ExchangeOrder, SentAt, Worker, WorkerError};

/// What a bound release's forward left behind.
pub(super) struct ForwardedRelease {
    pub(super) outcome: Forwarded<SessionReleaseAgentResult>,
    /// Whether the forward already logged `session.active_identity.forward_failed`
    /// for this release, so the daemon's own decision does not log it again.
    pub(super) failure_logged: bool,
}

/// Why a report or release is not offered to the worker.
struct ForwardRefusal {
    /// Whether the refusal logged `session.active_identity.forward_failed`.
    failure_logged: bool,
}

impl ForwardRefusal {
    /// A report or release that cannot be forwarded and logged no failure.
    const QUIET: Self = Self {
        failure_logged: false,
    };
    /// A forward whose worker did not answer within the bound.
    const LOGGED: Self = Self {
        failure_logged: true,
    };
}

/// What became of a report or release offered to the worker.
pub(super) enum Forwarded<T> {
    /// The worker answered, and the answer is the public result.
    Answered(T),
    /// Nothing was journaled; the report takes the memory-only path.
    MemoryOnly,
    /// The worker did not answer in time and may hold the operation; the report
    /// takes the memory-only path and the operation stays pending.
    Unacknowledged(Unacknowledged),
}

/// What the daemon keeps of a report the worker accepted: the claim it made,
/// the public order of the caller and the activity it carried.
#[derive(Clone)]
pub(super) struct ForwardedReportContext {
    pub(super) key: ClaimKey,
    pub(super) instance: String,
    pub(super) source: String,
    pub(super) seq: Option<u64>,
    pub(super) activity: Option<(AgentActivity, String)>,
}

/// What the daemon keeps of a release the worker accepted: the claim it ended
/// and the public order of the caller.
#[derive(Clone)]
pub(super) struct ForwardedReleaseContext {
    pub(super) instance: String,
    pub(super) provider: String,
    pub(super) process: ProcessIdentity,
    pub(super) sequence: u64,
    pub(super) agent: String,
    pub(super) source: String,
    pub(super) seq: Option<u64>,
}

/// What the daemon keeps of an operation the worker accepted.
pub(super) enum ForwardedContext {
    Report(ForwardedReportContext),
    Release(ForwardedReleaseContext),
}

/// The session's identity-commit guard, owned by the operation that decides,
/// forwards and commits a claim change.
pub(super) type CommitGuard = tokio::sync::OwnedMutexGuard<()>;

/// The worker to forward to and the identity of the subject, once a report is
/// known to be forwardable.
struct ForwardTarget {
    worker: Worker,
    /// The worker instance the operation is sent to, read before it is sent so
    /// its commit never has to ask the connection again.
    instance: String,
    process: ProcessIdentity,
    sequence: u64,
}

impl SessionRegistry {
    /// Offers a bound report to the worker of its session.
    ///
    /// The forward and its commit run in a task the daemon owns, so a hook that
    /// disconnects at its socket timeout cannot cancel a half-done commit. The
    /// task holds the session's identity-commit guard from the caller; a report
    /// that stays memory-only gets the guard back.
    #[expect(
        clippy::too_many_lines,
        reason = "the report's context, its fallback pending and the send task are built together"
    )]
    pub(super) async fn forward_active_report(
        &self,
        params: &SessionReportAgentParams,
        resolved: &ResolvedAgent,
        (session_id, session_path): (Option<&str>, Option<&str>),
        attested_start: Option<u64>,
        activity: Option<(AgentActivity, &str)>,
        guard: &mut Option<CommitGuard>,
    ) -> Forwarded<SessionReportAgentResult> {
        let Ok(target) = self
            .forward_target(
                &params.session_id,
                params.pid,
                params.monotonic_sequence,
                attested_start,
                ForwardedOperation::Report,
            )
            .await
        else {
            return Forwarded::MemoryOnly;
        };
        let report = ForwardedActiveReport {
            provider: resolved.base.as_str().to_owned(),
            agent: (resolved.name != resolved.base.as_str()).then(|| resolved.name.clone()),
            process: target.process,
            sequence: target.sequence,
            expires_at: claim_expiry(),
            reported_session: reported_session(session_id, session_path),
        };
        let context = ForwardedReportContext {
            instance: target.instance,
            key: ClaimKey {
                provider: report.provider.clone(),
                pid: report.process.pid,
                start_identity: report.process.start_identity,
                sequence: report.sequence,
            },
            source: params.source.clone(),
            seq: params.seq.map(protocol::ReportSequence::get),
            activity: activity.map(|(activity, epoch)| (activity, epoch.to_owned())),
        };
        #[cfg(test)]
        self.hold_report().await;
        let registry = self.clone();
        let id = params.session_id.clone();
        let worker = target.worker;
        let deadline = self.forward_bound();
        // Built before the request leaves, so an operation the worker may hold
        // is tracked whatever happens to the task that sends it.
        let pending = Box::new(self.unacknowledged_report(&id, context.clone(), None).await);
        let sent_flag = Arc::new(SentAt::new());
        let task_sent = Arc::clone(&sent_flag);
        let held = guard.take();
        let task = self.spawn_transaction("forward_active_identity_report", async move {
            let outcome = tokio::time::timeout(
                deadline,
                worker.record_active_identity_tracked(report, &task_sent),
            )
            .await;
            let forwarded = match outcome {
                Ok(Ok(ActiveIdentityOutcome::Accepted { revision })) => {
                    Box::pin(registry.commit_forwarded(
                        &id,
                        &worker,
                        revision,
                        ForwardedContext::Report(context),
                    ))
                    .await;
                    Forwarded::Answered(SessionReportAgentResult { recorded: true })
                }
                Ok(Ok(ActiveIdentityOutcome::Rejected { reason })) => {
                    debug!(
                        session_id = %id.0,
                        reason = ?reason,
                        "the worker rejected a forwarded active-agent report"
                    );
                    Forwarded::Answered(SessionReportAgentResult { recorded: false })
                }
                Ok(Err(error @ WorkerError::AnswerLost(_))) => {
                    registry
                        .note_forward_failed(&id, ForwardedOperation::Report, &error)
                        .await;
                    Forwarded::Unacknowledged(Unacknowledged::Report(Box::new(
                        registry
                            .unacknowledged_report(&id, context, task_sent.get().copied())
                            .await,
                    )))
                }
                Ok(Err(error)) => {
                    registry
                        .note_forward_failed(&id, ForwardedOperation::Report, &error)
                        .await;
                    Forwarded::MemoryOnly
                }
                Err(_elapsed) => {
                    registry
                        .note_forward_timed_out(&id, deadline, ForwardedOperation::Report)
                        .await;
                    // A request that never left was not seen by the worker.
                    if let Some(sent) = task_sent.get().copied() {
                        Forwarded::Unacknowledged(Unacknowledged::Report(Box::new(
                            registry
                                .unacknowledged_report(&id, context, Some(sent))
                                .await,
                        )))
                    } else {
                        Forwarded::MemoryOnly
                    }
                }
            };
            (forwarded, held)
        });
        match task.await {
            Ok((forwarded, held)) => {
                if matches!(
                    forwarded,
                    Forwarded::MemoryOnly | Forwarded::Unacknowledged(_)
                ) {
                    *guard = held;
                }
                forwarded
            }
            Err(error) => {
                warn!(
                    name: "session.active_identity.forward_failed",
                    session_id = %params.session_id.0,
                    cause = %error,
                    "forwarding an active-agent report did not finish; tracking what it sent"
                );
                if let Some(sent) = sent_flag.get().copied() {
                    Forwarded::Unacknowledged(Unacknowledged::Report(Box::new(PendingActivity {
                        sent: Some(sent),
                        ..*pending
                    })))
                } else {
                    Forwarded::MemoryOnly
                }
            }
        }
    }

    /// Offers a bound release to the worker of its session.
    ///
    /// A worker refusal is final unless the identity it would release was never
    /// journaled, in which case the memory release rule decides.
    #[expect(
        clippy::too_many_lines,
        reason = "the release's context, its fallback pending and the send task are built together"
    )]
    pub(super) async fn forward_active_release(
        &self,
        params: &SessionReleaseAgentParams,
        resolved: &ResolvedAgent,
        attested_start: Option<u64>,
        guard: &mut Option<CommitGuard>,
    ) -> ForwardedRelease {
        let target = match self
            .forward_target(
                &params.session_id,
                params.pid,
                params.monotonic_sequence,
                attested_start,
                ForwardedOperation::Release,
            )
            .await
        {
            Ok(target) => target,
            Err(refusal) => {
                return ForwardedRelease {
                    outcome: Forwarded::MemoryOnly,
                    failure_logged: refusal.failure_logged,
                };
            }
        };
        let release = ForwardedActiveRelease {
            provider: resolved.base.as_str().to_owned(),
            process: target.process,
            sequence: target.sequence,
        };
        let context = ForwardedReleaseContext {
            instance: target.instance,
            provider: release.provider.clone(),
            process: release.process,
            sequence: release.sequence,
            agent: resolved.name.clone(),
            source: params.source.clone(),
            seq: params.seq.map(protocol::ReportSequence::get),
        };
        #[cfg(test)]
        self.hold_report().await;
        let registry = self.clone();
        let id = params.session_id.clone();
        let worker = target.worker;
        let deadline = self.forward_bound();
        // Built before the request leaves, so an operation the worker may hold
        // is tracked whatever happens to the task that sends it.
        let pending = Box::new(
            self.unacknowledged_release(&id, context.clone(), None)
                .await,
        );
        let sent_flag = Arc::new(SentAt::new());
        let task_sent = Arc::clone(&sent_flag);
        let held = guard.take();
        let task = self.spawn_transaction("forward_active_identity_release", async move {
            let outcome = tokio::time::timeout(
                deadline,
                worker.release_active_identity_tracked(release, &task_sent),
            )
            .await;
            let (forwarded, failure_logged) = match outcome {
                Ok(Ok(ActiveIdentityOutcome::Accepted { revision })) => {
                    Box::pin(registry.commit_forwarded(
                        &id,
                        &worker,
                        revision,
                        ForwardedContext::Release(context),
                    ))
                    .await;
                    (
                        Forwarded::Answered(SessionReleaseAgentResult { released: true }),
                        false,
                    )
                }
                Ok(Ok(ActiveIdentityOutcome::Rejected { reason })) => {
                    debug!(
                        session_id = %id.0,
                        reason = ?reason,
                        "the worker rejected a forwarded active-agent release"
                    );
                    let forwarded = if registry.identity_is_memory_only(&id).await {
                        Forwarded::MemoryOnly
                    } else {
                        Forwarded::Answered(SessionReleaseAgentResult { released: false })
                    };
                    (forwarded, false)
                }
                Ok(Err(error @ WorkerError::AnswerLost(_))) => {
                    registry
                        .note_forward_failed(&id, ForwardedOperation::Release, &error)
                        .await;
                    (
                        Forwarded::Unacknowledged(Unacknowledged::Release(Box::new(
                            registry
                                .unacknowledged_release(&id, context, task_sent.get().copied())
                                .await,
                        ))),
                        true,
                    )
                }
                Ok(Err(error)) => {
                    registry
                        .note_forward_failed(&id, ForwardedOperation::Release, &error)
                        .await;
                    (Forwarded::MemoryOnly, true)
                }
                Err(_elapsed) => {
                    registry
                        .note_forward_timed_out(&id, deadline, ForwardedOperation::Release)
                        .await;
                    // A request that never left was not seen by the worker.
                    let forwarded = if let Some(sent) = task_sent.get().copied() {
                        Forwarded::Unacknowledged(Unacknowledged::Release(Box::new(
                            registry
                                .unacknowledged_release(&id, context, Some(sent))
                                .await,
                        )))
                    } else {
                        Forwarded::MemoryOnly
                    };
                    (forwarded, true)
                }
            };
            (forwarded, held, failure_logged)
        });
        match task.await {
            Ok((forwarded, held, failure_logged)) => {
                if matches!(
                    forwarded,
                    Forwarded::MemoryOnly | Forwarded::Unacknowledged(_)
                ) {
                    *guard = held;
                }
                ForwardedRelease {
                    outcome: forwarded,
                    failure_logged,
                }
            }
            Err(error) => {
                warn!(
                    name: "session.active_identity.forward_failed",
                    session_id = %params.session_id.0,
                    cause = %error,
                    "forwarding an active-agent release did not finish; tracking what it sent"
                );
                let outcome = if let Some(sent) = sent_flag.get().copied() {
                    Forwarded::Unacknowledged(Unacknowledged::Release(Box::new(PendingRelease {
                        sent: Some(sent),
                        ..*pending
                    })))
                } else {
                    Forwarded::MemoryOnly
                };
                ForwardedRelease {
                    outcome,
                    failure_logged: true,
                }
            }
        }
    }

    /// The session's identity-commit guard, or `None` for a session that is
    /// not in the registry.
    pub(super) async fn lock_identity_commit(&self, id: &SessionId) -> Option<CommitGuard> {
        let commit = {
            let sessions = self.inner.sessions.lock().await;
            Arc::clone(&sessions.get(id)?.identity_commit)
        };
        Some(commit.lock_owned().await)
    }

    /// Keeps `snapshot`, read at `order`, as the newest observation of the
    /// worker's identity unless a later one is held. Returns whether an
    /// operation the session tracks is now known to be rejected.
    pub(super) async fn record_identity_observation(
        &self,
        id: &SessionId,
        snapshot: &InspectSnapshot,
        order: ExchangeOrder,
    ) -> bool {
        let (Some(instance), Some(revision)) = (
            snapshot.worker_instance_id.as_ref(),
            snapshot.active_identity_revision,
        ) else {
            return false;
        };
        let mut sessions = self.inner.sessions.lock().await;
        let Some(entry) = sessions.get_mut(id) else {
            return false;
        };
        claim::observe_identity(
            entry,
            IdentityFence {
                worker_instance_id: instance.to_string(),
                revision,
            },
            order,
        )
    }

    /// Records a snapshot the periodic sync read and drops the forwarded
    /// operations it shows the worker rejected. The identity-commit guard is
    /// taken only when there is one, so a sync that finds nothing to drop
    /// never queues behind a report.
    pub(super) async fn observe_worker_identity(
        &self,
        id: &SessionId,
        snapshot: &InspectSnapshot,
        order: ExchangeOrder,
    ) {
        if !self.record_identity_observation(id, snapshot, order).await {
            return;
        }
        if let Some(_commit) = self.lock_identity_commit(id).await {
            self.settle_pending_activity(id).await;
        }
    }

    /// Imports what the periodic sync's `snapshot` shows beyond metadata: the
    /// forwarded-operation settlement and the prompt acceptance.
    pub(super) async fn observe_worker_snapshot(
        &self,
        id: &SessionId,
        snapshot: &InspectSnapshot,
        order: ExchangeOrder,
    ) {
        self.observe_worker_identity(id, snapshot, order).await;
        self.observe_prompt_accepted(id, snapshot).await;
    }

    /// Applies the activity that waits for an identity commit once the commit
    /// is known: when the claim it belongs to is the session's, the activity is
    /// applied; when a newer worker state superseded the claim or the worker
    /// instance changed, it is dropped; otherwise it keeps waiting.
    pub(super) async fn settle_pending_activity(&self, id: &SessionId) {
        let (info, evidence) = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                return;
            };
            let fence = entry.identity_fence.clone();
            let observed = entry.identity_observed.clone();
            if entry.pending_release.as_ref().is_some_and(|release| {
                release.state(fence.as_ref(), observed.as_ref()) != PendingState::Waiting
            }) {
                entry.pending_release = None;
            }
            entry.displaced_pending.retain(|operation| {
                let state = match operation {
                    Unacknowledged::Report(pending) => {
                        pending_state(pending, fence.as_ref(), observed.as_ref())
                    }
                    Unacknowledged::Release(pending) => {
                        pending.state(fence.as_ref(), observed.as_ref())
                    }
                };
                state == PendingState::Waiting
            });
            let Some(pending) = entry.pending_activity.take() else {
                return;
            };
            match pending_state(&pending, fence.as_ref(), observed.as_ref()) {
                PendingState::Waiting => {
                    entry.pending_activity = Some(pending);
                    return;
                }
                PendingState::Obsolete | PendingState::Rejected => return,
                PendingState::Settled => {}
            }
            if is_terminal(entry.info.state) || !owns(entry, &pending.key) {
                debug!(
                    session_id = %id.0,
                    "a newer claim replaced the report that carried this activity; ignoring it"
                );
                return;
            }
            let Some(activity) = pending.activity else {
                return;
            };
            for report in [&mut entry.active_agent, &mut entry.last_agent_report]
                .into_iter()
                .flatten()
            {
                report.activity_reported = true;
            }
            let evidence = self.apply_reported_activity(entry, id, activity, &pending.epoch);
            entry.info.updated_at = timestamp_now();
            (entry.info.clone(), evidence)
        };
        self.emit(event::SESSION_UPDATED, &info);
        if let Some(evidence) = evidence {
            let event = crate::events::event(
                event::AGENT_STATE,
                event_payload(evidence.event(id.clone())),
            );
            self.publish(event);
        }
    }

    /// Records the public order of the caller on the journaled claim it made,
    /// and parks its activity until the identity is committed.
    async fn note_forwarded_report(
        &self,
        id: &SessionId,
        revision: u64,
        context: ForwardedReportContext,
    ) {
        let instance = Some(context.instance.clone());
        let mut sessions = self.inner.sessions.lock().await;
        let Some(entry) = sessions.get_mut(id) else {
            return;
        };
        for report in [&mut entry.active_agent, &mut entry.last_agent_report]
            .into_iter()
            .flatten()
        {
            if let ClaimOrigin::Journaled { key, public, .. } = &mut report.origin {
                if *key == context.key {
                    *public = Some(PublicOrder {
                        source: context.source.clone(),
                        seq: context.seq,
                    });
                }
            }
        }
        let Some(instance) = instance else {
            return;
        };
        let (activity, epoch) = match context.activity {
            Some((activity, epoch)) => (Some(activity), epoch),
            None => (None, String::new()),
        };
        let pending = Box::new(PendingActivity {
            acknowledged: true,
            key: context.key,
            revision: IdentityFence {
                worker_instance_id: instance,
                revision,
            },
            public: PublicOrder {
                source: context.source,
                seq: context.seq,
            },
            activity,
            epoch,
            admitted: claim::next_admission(),
            sent: None,
        });
        // A slower, older task never replaces the activity of a newer report.
        let newer_waits = entry.pending_activity.as_ref().is_some_and(|waiting| {
            waiting.revision.worker_instance_id == pending.revision.worker_instance_id
                && waiting.revision.revision >= pending.revision.revision
        });
        if !newer_waits {
            entry.pending_activity = Some(pending);
        }
    }

    /// The worker and subject of a forwardable report, or the refusal after
    /// noting why the report stays memory-only.
    async fn forward_target(
        &self,
        id: &SessionId,
        pid: Option<u32>,
        monotonic_sequence: Option<protocol::ReportSequence>,
        attested_start: Option<u64>,
        operation: ForwardedOperation,
    ) -> Result<ForwardTarget, ForwardRefusal> {
        let (Some(pid), Some(sequence)) = (pid, monotonic_sequence) else {
            self.note_not_durable(id, NotDurableCause::Unbound).await;
            return Err(ForwardRefusal::QUIET);
        };
        let worker = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or(ForwardRefusal::QUIET)?;
            if is_terminal(entry.info.state) {
                return Err(ForwardRefusal::QUIET);
            }
            match &entry.runtime {
                RuntimeHandle::Worker(worker) => worker.clone(),
                RuntimeHandle::Unavailable(_) => {
                    drop(sessions);
                    self.note_not_durable(id, NotDurableCause::ForwardFailed)
                        .await;
                    return Err(ForwardRefusal::QUIET);
                }
            }
        };
        let Some(version) = self.within_worker_bound(worker.selected_version()).await else {
            self.note_forward_timed_out(id, self.forward_bound(), operation)
                .await;
            return Err(ForwardRefusal::LOGGED);
        };
        if version < ACTIVE_IDENTITY_FORWARD_VERSION {
            self.note_not_durable(id, NotDurableCause::WorkerProtocolV7)
                .await;
            return Err(ForwardRefusal::QUIET);
        }
        let Some(instance) = self
            .within_worker_bound(worker.worker_instance_id())
            .await
            .flatten()
            .map(|instance| instance.to_string())
        else {
            self.note_not_durable(id, NotDurableCause::ForwardFailed)
                .await;
            return Err(ForwardRefusal::QUIET);
        };
        let start_identity = if let Some(start) = attested_start {
            start
        } else {
            let Ok(Some(current)) = self.inner.inspector.identity(pid) else {
                self.note_not_durable(id, NotDurableCause::Unbound).await;
                return Err(ForwardRefusal::QUIET);
            };
            current.start_identity.get()
        };
        Ok(ForwardTarget {
            worker,
            instance,
            process: ProcessIdentity {
                pid,
                start_identity,
            },
            sequence: sequence.get(),
        })
    }

    /// Commits the identity the worker journaled at `revision`, and then the
    /// activity of the report that carried it. The caller's task holds the
    /// session's identity-commit guard; the public order of the caller is
    /// recorded before the snapshot is applied, so the claim is adopted with
    /// it. A commit that cannot complete now is repeated by the next metadata
    /// sync.
    async fn commit_forwarded(
        &self,
        id: &SessionId,
        worker: &Worker,
        revision: u64,
        context: ForwardedContext,
    ) {
        #[cfg(test)]
        self.hold_report().await;
        match context {
            ForwardedContext::Report(context) => {
                self.note_forwarded_report(id, revision, context).await;
            }
            ForwardedContext::Release(context) => {
                self.note_forwarded_release(id, revision, context).await;
            }
        }
        Box::pin(self.commit_forwarded_snapshot(id, worker, revision)).await;
        self.settle_pending_activity(id).await;
    }

    /// Keeps the public order of a release the worker journaled until the
    /// daemon adopts the release.
    async fn note_forwarded_release(
        &self,
        id: &SessionId,
        revision: u64,
        context: ForwardedReleaseContext,
    ) {
        let mut sessions = self.inner.sessions.lock().await;
        if let Some(entry) = sessions.get_mut(id) {
            entry.pending_release = Some(Box::new(PendingRelease {
                acknowledged: true,
                provider: context.provider,
                pid: context.process.pid,
                start_identity: context.process.start_identity,
                sequence: context.sequence,
                revision: IdentityFence {
                    worker_instance_id: context.instance,
                    revision,
                },
                agent: context.agent,
                public: PublicOrder {
                    source: context.source,
                    seq: context.seq,
                },
                admitted: claim::next_admission(),
                sent: None,
            }));
        }
    }

    /// Awaits a read of the worker connection for at most the worker bound.
    /// The connection stays busy while a request waits on a worker that does
    /// not answer, and the identity-commit guard must not wait behind it.
    async fn within_worker_bound<T>(
        &self,
        read: impl std::future::Future<Output = T>,
    ) -> Option<T> {
        tokio::time::timeout(self.forward_bound(), read).await.ok()
    }

    /// The newest revision of the worker instance `instance` this daemon has
    /// committed or accepted: an operation sent now is ordered after it.
    async fn fence_at_send(&self, id: &SessionId, instance: &str) -> IdentityFence {
        let revision = self
            .inner
            .sessions
            .lock()
            .await
            .get(id)
            .and_then(|entry| claim::memory_floor(entry).0)
            .filter(|floor| floor.worker_instance_id == instance)
            .map_or(0, |floor| floor.revision);
        IdentityFence {
            worker_instance_id: instance.to_owned(),
            revision,
        }
    }

    /// A forwarded report the worker may hold although its answer timed out.
    async fn unacknowledged_report(
        &self,
        id: &SessionId,
        context: ForwardedReportContext,
        sent: Option<ExchangeOrder>,
    ) -> PendingActivity {
        PendingActivity {
            revision: self.fence_at_send(id, &context.instance).await,
            acknowledged: false,
            key: context.key,
            public: PublicOrder {
                source: context.source,
                seq: context.seq,
            },
            activity: None,
            epoch: String::new(),
            admitted: claim::next_admission(),
            sent,
        }
    }

    /// A forwarded release the worker may hold although its answer timed out.
    async fn unacknowledged_release(
        &self,
        id: &SessionId,
        context: ForwardedReleaseContext,
        sent: Option<ExchangeOrder>,
    ) -> PendingRelease {
        PendingRelease {
            revision: self.fence_at_send(id, &context.instance).await,
            acknowledged: false,
            provider: context.provider,
            pid: context.process.pid,
            start_identity: context.process.start_identity,
            sequence: context.sequence,
            agent: context.agent,
            public: PublicOrder {
                source: context.source,
                seq: context.seq,
            },
            admitted: claim::next_admission(),
            sent,
        }
    }

    /// Records that a forward exceeded its configured bound.
    async fn note_forward_timed_out(
        &self,
        id: &SessionId,
        deadline: std::time::Duration,
        operation: ForwardedOperation,
    ) {
        match operation {
            ForwardedOperation::Report => warn!(
                name: "session.active_identity.forward_failed",
                session_id = %id.0,
                deadline_ms = deadline.as_millis(),
                "the worker did not answer a forwarded active-agent report in time; keeping it in memory only"
            ),
            ForwardedOperation::Release => warn!(
                name: "session.active_identity.forward_failed",
                session_id = %id.0,
                deadline_ms = deadline.as_millis(),
                "the worker did not answer a forwarded active-agent release in time; the daemon ends a memory claim and leaves a claim the worker journaled to the worker's journal"
            ),
        }
        self.note_not_durable(id, NotDurableCause::ForwardFailed)
            .await;
    }

    /// Applies the worker's snapshot that holds the identity journaled at
    /// `revision`, unless a snapshot already carried it. A snapshot that is
    /// not read in time or not applied logs `session.active_identity.commit_deferred`
    /// at WARN; the worker journaled the identity and the next sync commits it.
    /// The caller holds the session's identity-commit guard.
    async fn commit_forwarded_snapshot(&self, id: &SessionId, worker: &Worker, revision: u64) {
        let deadline = self.forward_bound();
        let snapshot = match tokio::time::timeout(deadline, worker.inspect_ordered()).await {
            Ok(Ok((snapshot, order))) => {
                self.record_identity_observation(id, &snapshot, order).await;
                snapshot
            }
            Err(_) => {
                warn!(
                    name: "session.active_identity.commit_deferred",
                    session_id = %id.0,
                    deadline_ms = deadline.as_millis(),
                    "the worker did not answer the snapshot of a forwarded identity in time; the daemon commits it with the next sync"
                );
                return;
            }
            Ok(Err(error)) => {
                warn!(
                    name: "session.active_identity.commit_deferred",
                    session_id = %id.0,
                    cause = %error,
                    "the worker journaled a forwarded identity; the daemon commits it with the next sync"
                );
                return;
            }
        };
        let instance = snapshot
            .worker_instance_id
            .as_ref()
            .map(ToString::to_string);
        let already_committed = {
            let sessions = self.inner.sessions.lock().await;
            sessions.get(id).is_some_and(|entry| {
                entry.identity_fence.as_ref().is_some_and(|fence| {
                    Some(fence.worker_instance_id.as_str()) == instance.as_deref()
                        && fence.revision >= revision
                        && snapshot
                            .active_identity_revision
                            .is_none_or(|current| current <= fence.revision)
                })
            })
        };
        if already_committed {
            return;
        }
        let outcome = self
            .apply_worker_metadata_snapshot_locked(id, &snapshot)
            .await;
        if outcome != WorkerMetadataApplyOutcome::Applied {
            warn!(
                name: "session.active_identity.commit_deferred",
                session_id = %id.0,
                outcome = ?outcome,
                "the worker journaled a forwarded identity; the daemon commits it with the next sync"
            );
        }
    }

    /// Whether the session's active identity was reported to this daemon only.
    async fn identity_is_memory_only(&self, id: &SessionId) -> bool {
        let sessions = self.inner.sessions.lock().await;
        sessions.get(id).is_some_and(is_memory_report)
    }

    /// Logs, once per session and cause, that a report stays memory-only.
    async fn note_not_durable(&self, id: &SessionId, cause: NotDurableCause) {
        let first = {
            let mut sessions = self.inner.sessions.lock().await;
            sessions
                .get_mut(id)
                .is_some_and(|entry| entry.not_durable_logged.insert(cause))
        };
        if first {
            info!(
                name: "session.active_identity.not_durable",
                session_id = %id.0,
                cause = cause.as_str(),
                "an active-agent report stays in memory only and is lost on a restart"
            );
        }
    }

    /// A forward that failed in transit or in the worker's journal.
    async fn note_forward_failed(
        &self,
        id: &SessionId,
        operation: ForwardedOperation,
        error: &WorkerError,
    ) {
        match operation {
            ForwardedOperation::Report => warn!(
                name: "session.active_identity.forward_failed",
                session_id = %id.0,
                cause = %error,
                "forwarding an active-agent report failed; keeping it in memory only"
            ),
            ForwardedOperation::Release => warn!(
                name: "session.active_identity.forward_failed",
                session_id = %id.0,
                cause = %error,
                "forwarding an active-agent release failed; the daemon ends a memory claim and leaves a claim the worker journaled to the worker's journal"
            ),
        }
        self.note_not_durable(id, NotDurableCause::ForwardFailed)
            .await;
    }
}

/// The kind of operation a forward carried.
#[derive(Clone, Copy)]
enum ForwardedOperation {
    Report,
    Release,
}

/// Expiry of a forwarded claim: the receipt time plus the shared ceiling.
fn claim_expiry() -> String {
    let ttl = time::Duration::seconds(
        i64::try_from(protocol::MAX_IDENTITY_CLAIM_TTL_SECS)
            .expect("identity TTL ceiling fits i64"),
    );
    (OffsetDateTime::now_utc() + ttl)
        .format(&Rfc3339)
        .expect("an RFC 3339 timestamp formats")
}

/// The public session annotation of a report: the id with its transcript path
/// when both are given, otherwise whichever one is.
fn reported_session(id: Option<&str>, path: Option<&str>) -> Option<ReportedSession> {
    match (id, path) {
        (Some(id), path) => Some(ReportedSession {
            kind: ReportedSessionKind::Id,
            value: id.to_owned(),
            transcript_path: path.map(str::to_owned),
        }),
        (None, Some(path)) => Some(ReportedSession {
            kind: ReportedSessionKind::Path,
            value: path.to_owned(),
            transcript_path: None,
        }),
        (None, None) => None,
    }
}
