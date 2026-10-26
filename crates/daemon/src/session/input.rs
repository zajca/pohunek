//! User/initial input framing and PTY delivery.

use pohunek_worker_protocol::{InputFragment as WorkerInputFragment, SecretBytes};

use protocol::{
    ActivityRevision, AgentStateEvent, InputNotSubmittedReason, SessionRuntimeIdentity,
};

use crate::agent::host::{RuntimeDefinition, RuntimeHost};
use crate::runtime::WriteReservation;
use pohunek_worker_protocol::InspectSnapshot;

use super::{
    broadcast, debug, event_concerns_other_session, not_running_refusal, runtime_loss_reason,
    session_not_found, unavailable_runtime_error, warn, worker_error_to_protocol, ActivityEvidence,
    AgentActivity, Duration, ErrorClass, InputRules, LaunchCommand, LaunchCommandPlan,
    ProtocolError, ResolvedAgent, RuntimeHandle, RuntimeRef, SessionEntry, SessionId,
    SessionInputParams, SessionInputResult, SessionInputWait, SessionRegistry,
    SessionRegistryConfig, SessionState, Worker,
};
use super::{live_runtime_identity, PromptAcceptance};

pub(super) const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
pub(super) const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
pub(super) const SUBMIT: &[u8] = b"\r";

/// Activities that settle a wait when the caller supplies no explicit targets.
const DEFAULT_INPUT_WAIT_UNTIL: [AgentActivity; 2] = [AgentActivity::Idle, AgentActivity::Blocked];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InputActivitySnapshot {
    pub(super) activity: Option<AgentActivity>,
    pub(super) revision: ActivityRevision,
    pub(super) runtime: SessionRuntimeIdentity,
    /// The worker's `CLOCK_MONOTONIC` reading before the Enter write, on a
    /// snapshot returned by a completed write; absent otherwise.
    pub(super) prompt_mark: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InputSubmission {
    pub(super) runtime: SessionRuntimeIdentity,
    pub(super) activity_epoch: String,
    pub(super) after_revision: ActivityRevision,
    pub(super) deadline: tokio::time::Instant,
    /// Delayed-submit framing: until Enter is written the prompt box still
    /// shows the typed text and reads as `idle`, so an `idle` target counts
    /// only after `working` evidence above `after_revision`.
    pub(super) idle_requires_working: bool,
    /// The worker's `CLOCK_MONOTONIC` reading taken before Enter was written.
    /// Absent when the worker predates it, or when the agent was `working` at
    /// the boundary: a busy agent queues the input, and its own `working`
    /// evidence already answers the wait.
    pub(super) prompt_mark: Option<u64>,
    /// The worker reported a submit mark, so the session's launch runtime
    /// reports prompt acceptances. Sessions of other runtimes never carry
    /// `prompt_accepted` in a result.
    pub(super) prompt_capable: bool,
    /// The caller opted in to a prompt acceptance satisfying a `working`
    /// target and to `prompt_accepted` in the result.
    pub(super) prompt_opt_in: bool,
}

/// Phase of a daemon-paced submit that the waiting caller and the writing
/// task agree on without a lock. A caller that gives up before Enter claims
/// the submit guarantees Enter is never written.
#[derive(Debug, Default)]
struct SubmitProgress {
    state: std::sync::atomic::AtomicU8,
    abandon: tokio_util::sync::CancellationToken,
    reason: std::sync::OnceLock<InputNotSubmittedReason>,
}

impl SubmitProgress {
    const BODY_PHASE: u8 = 0;
    const ENTER_CLAIMED: u8 = 1;
    const ABANDONED: u8 = 2;

    /// Claims the right to write Enter; `false` once the caller abandoned.
    fn claim_enter(&self) -> bool {
        self.state
            .compare_exchange(
                Self::BODY_PHASE,
                Self::ENTER_CLAIMED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    /// Stops the submit before Enter; `false` when Enter was already claimed.
    fn abandon(&self, reason: InputNotSubmittedReason) -> bool {
        let _ignored = self.reason.set(reason);
        let abandoned = self
            .state
            .compare_exchange(
                Self::BODY_PHASE,
                Self::ABANDONED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok();
        if abandoned {
            self.abandon.cancel();
        }
        abandoned
    }

    /// Whether Enter's send began, so the text may have been submitted.
    fn enter_sent(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::Acquire) == Self::ENTER_CLAIMED
    }

    fn not_submitted(&self) -> ProtocolError {
        ProtocolError::session_input_not_submitted(
            self.reason
                .get()
                .copied()
                .unwrap_or(InputNotSubmittedReason::DeadlineElapsed),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InputWritePlan {
    pub(super) body: Vec<u8>,
    pub(super) submit_delay: Duration,
}

impl InputSubmission {
    /// Whether a prompt acceptance can answer this wait.
    fn accepts_prompt(&self) -> bool {
        self.prompt_opt_in && self.prompt_mark.is_some()
    }

    /// The deadline error: delayed-submit input reports that Enter was
    /// written, since the submission exists only once its Enter was acknowledged.
    fn timeout_error(&self, session_id: &SessionId, targets: &[AgentActivity]) -> ProtocolError {
        if self.accepts_prompt() && targets.contains(&AgentActivity::Working) {
            ProtocolError::session_input_timeout_unacknowledged(&session_id.0)
        } else if self.idle_requires_working {
            ProtocolError::session_input_timeout_after_enter(&session_id.0)
        } else {
            ProtocolError::session_input_timeout()
        }
    }
}

impl SessionRegistry {
    /// Inject text into a running session using the agent's input framing rules.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError`] for invalid input or wait contracts, unavailable
    /// runtimes, failed delivery, changed runtime identity, or unmet deadlines.
    pub async fn input(
        &self,
        params: SessionInputParams,
    ) -> Result<SessionInputResult, ProtocolError> {
        let Some(wait) = params.wait else {
            self.write_input_to_session(&params.session_id, &params.text)
                .await?;
            return Ok(SessionInputResult {
                accepted: true,
                activity: None,
                activity_source: None,
                runtime: None,
                activity_epoch: None,
                activity_revision: None,
                submitted: None,
                prompt_accepted: None,
            });
        };

        let mut wait = wait;
        let mut seen = Vec::new();
        wait.until.get_or_insert_with(Vec::new).retain(|activity| {
            if seen.contains(activity) {
                false
            } else {
                seen.push(*activity);
                true
            }
        });
        Self::validate_input_wait(&wait)?;
        let deadline_ms = wait.timeout_ms.unwrap_or(protocol::MAX_SESSION_WAIT_MS);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(deadline_ms));
        self.ensure_not_external(&params.session_id).await?;
        let mut events = self.subscribe();
        let _waiter_permit = self.acquire_waiter(&params.session_id)?;
        let _evidence_watch = self.watch_activity_evidence(&params.session_id);
        if self
            .input_activity_snapshot(&params.session_id, None)
            .await?
            .activity
            == Some(AgentActivity::Blocked)
        {
            return Err(ProtocolError::session_agent_blocked());
        }
        let mut submission = self
            .write_waited_input(&params.session_id, &params.text, deadline)
            .await?;
        submission.prompt_opt_in =
            wait.allow_prompt_accepted == Some(true) && submission.prompt_capable;
        self.input_activity_snapshot(&params.session_id, Some(&submission.runtime))
            .await?;
        self.await_input_settled(&params.session_id, wait, submission, &mut events)
            .await
    }

    /// Delivers the initial input of a create like a waited `session.input`
    /// with `until: [working]` and commits the durable `initial_input` marker
    /// once the submit was written.
    ///
    /// Delivery is bounded by [`protocol::MAX_SESSION_WAIT_MS`]; the
    /// acknowledgement window of `ack_timeout_ms` starts once the submit was
    /// acknowledged by the worker. Returns `Ok(true)` when `working` activity
    /// followed the submit within the window and `Ok(false)` when it did not
    /// (the window elapsed, the runtime changed, the session ended or the
    /// daemon began shutting down): the input was delivered either way.
    ///
    /// # Errors
    ///
    /// Returns the delivery error when the submit was not written (the caller
    /// rolls the create back), or the marker commit error.
    pub(super) async fn deliver_acknowledged_initial_input(
        &self,
        session_id: &SessionId,
        text: &str,
        ack_timeout_ms: u32,
    ) -> Result<bool, ProtocolError> {
        let delivery_deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::from(protocol::MAX_SESSION_WAIT_MS));
        let mut events = self.subscribe();
        let _waiter_permit = self.acquire_waiter(session_id)?;
        let _evidence_watch = self.watch_activity_evidence(session_id);
        let progress = std::sync::Arc::new(SubmitProgress::default());
        let written = self
            .write_tracked_waited_input(
                session_id,
                text,
                delivery_deadline,
                std::sync::Arc::clone(&progress),
            )
            .await;
        let mut submission = match written {
            Ok(submission) => submission,
            Err(error) if progress.enter_sent() && error.code != "session_input_stalled" => {
                // Enter's send began, so the prompt may have been submitted:
                // the create keeps its session and reports no acknowledgement.
                // A stalled write may have typed only part of the text, so it
                // rolls the create back like an input without the window.
                warn!(error = %error, "initial input delivery ended after Enter was sent");
                self.commit_initial_input(session_id).await?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        self.commit_initial_input(session_id).await?;
        submission.deadline =
            tokio::time::Instant::now() + Duration::from_millis(u64::from(ack_timeout_ms));
        submission.prompt_opt_in = submission.prompt_capable;
        let wait = SessionInputWait {
            until: Some(vec![AgentActivity::Working]),
            timeout_ms: Some(ack_timeout_ms),
            allow_prompt_accepted: Some(true),
        };
        match self
            .await_input_settled(session_id, wait, submission.clone(), &mut events)
            .await
        {
            Ok(_result) => Ok(true),
            Err(error) => {
                // The wait ends on runtime loss and shutdown before it reads
                // the evidence, which may already hold the acknowledgement.
                let acknowledged = self
                    .working_evidence_recorded(session_id, &submission)
                    .await;
                debug!(error = %error, acknowledged, "initial input wait ended early");
                Ok(acknowledged)
            }
        }
    }

    pub(super) async fn write_input_to_session(
        &self,
        session_id: &SessionId,
        text: &str,
    ) -> Result<(), ProtocolError> {
        let input_gate = self.input_gate(session_id).await?;
        let _input_guard = input_gate.lock_owned().await;
        let (runtime, rules) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get(session_id)
                .ok_or_else(|| session_not_found(&session_id.0))?;
            input_runtime_identity(entry, session_id)?;
            entry.input_rules.validate_activity(entry.info.activity)?;
            (entry.runtime.clone(), entry.input_rules)
        };

        let writes = build_input_writes(text, rules)?;
        match runtime {
            RuntimeHandle::Worker(worker) => {
                worker
                    .reserve_write(worker_input_fragments(writes, Duration::ZERO))
                    .await
                    .map_err(worker_error_to_protocol)?
                    .commit(|| self.inner.turns.note_input(session_id))
                    .await
                    .map_err(worker_error_to_protocol)?;
            }
            RuntimeHandle::Unavailable(state) => {
                return Err(unavailable_runtime_error(session_id, state, None));
            }
        }

        Ok(())
    }

    async fn write_waited_input(
        &self,
        session_id: &SessionId,
        text: &str,
        deadline: tokio::time::Instant,
    ) -> Result<InputSubmission, ProtocolError> {
        self.write_tracked_waited_input(
            session_id,
            text,
            deadline,
            std::sync::Arc::new(SubmitProgress::default()),
        )
        .await
    }

    /// [`Self::write_waited_input`] reporting through `progress` whether the
    /// Enter send began, which stays observable after an error.
    async fn write_tracked_waited_input(
        &self,
        session_id: &SessionId,
        text: &str,
        deadline: tokio::time::Instant,
        progress: std::sync::Arc<SubmitProgress>,
    ) -> Result<InputSubmission, ProtocolError> {
        self.write_waited_input_inner(
            session_id,
            text,
            deadline,
            Duration::ZERO,
            progress,
            #[cfg(test)]
            self.take_input_send_hold(),
        )
        .await
    }

    #[cfg(test)]
    pub(super) async fn write_waited_input_with_ack_delay(
        &self,
        session_id: &SessionId,
        text: &str,
        deadline: tokio::time::Instant,
        plan_ack_delay: Duration,
    ) -> Result<InputSubmission, ProtocolError> {
        self.write_waited_input_inner(
            session_id,
            text,
            deadline,
            plan_ack_delay,
            std::sync::Arc::new(SubmitProgress::default()),
            #[cfg(test)]
            self.take_input_send_hold(),
        )
        .await
    }

    /// Parks the next waited input write at its send boundary.
    ///
    /// The write meets `boundary` once after it captured its activity boundary
    /// under the session lock, and again before it starts sending. It keeps
    /// that lock until the send starts, so activity reported after the second
    /// meeting is ordered after the boundary without relying on wall-clock
    /// timing.
    #[cfg(test)]
    pub(super) fn hold_next_input_send(&self, boundary: std::sync::Arc<tokio::sync::Barrier>) {
        *self
            .inner
            .input_send_hold
            .lock()
            .expect("input send hold test lock") = Some(boundary);
    }

    /// Parks the next delayed-submit input between its body write and the
    /// submit delay: the write meets `boundary` once after the body was
    /// acknowledged and again before the delay starts.
    #[cfg(test)]
    pub(super) fn hold_next_submit_delay(&self, boundary: std::sync::Arc<tokio::sync::Barrier>) {
        *self
            .inner
            .input_delay_hold
            .lock()
            .expect("input delay hold test lock") = Some(boundary);
    }

    /// Parks the next waited input write after its Enter was sent and before
    /// it checks the session again: the write meets `boundary` once the send
    /// completed and again before the check.
    #[cfg(test)]
    pub(super) fn hold_next_input_sent(&self, boundary: std::sync::Arc<tokio::sync::Barrier>) {
        *self
            .inner
            .input_sent_hold
            .lock()
            .expect("input sent hold test lock") = Some(boundary);
    }

    #[cfg(test)]
    async fn hold_after_input_sent(&self) {
        let boundary = self
            .inner
            .input_sent_hold
            .lock()
            .expect("input sent hold test lock")
            .take();
        if let Some(boundary) = boundary {
            boundary.wait().await;
            boundary.wait().await;
        }
    }

    #[cfg(test)]
    fn take_input_send_hold(&self) -> Option<std::sync::Arc<tokio::sync::Barrier>> {
        self.inner
            .input_send_hold
            .lock()
            .expect("input send hold test lock")
            .take()
    }

    async fn write_waited_input_inner(
        &self,
        session_id: &SessionId,
        text: &str,
        deadline: tokio::time::Instant,
        plan_ack_delay: Duration,
        progress: std::sync::Arc<SubmitProgress>,
        #[cfg(test)] send_boundary: Option<std::sync::Arc<tokio::sync::Barrier>>,
    ) -> Result<InputSubmission, ProtocolError> {
        let input_gate = self.input_gate(session_id).await?;
        let input_guard = self
            .lock_input_before_deadline(input_gate, deadline)
            .await?;
        let (worker, rules, runtime) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get(session_id)
                .ok_or_else(|| session_not_found(&session_id.0))?;
            let runtime = input_runtime_identity(entry, session_id)?;
            reject_blocked_wait(entry.info.activity)?;
            let worker = match &entry.runtime {
                RuntimeHandle::Worker(worker) => worker.clone(),
                RuntimeHandle::Unavailable(state) => {
                    return Err(unavailable_runtime_error(
                        session_id,
                        *state,
                        runtime_loss_reason(entry).as_deref(),
                    ));
                }
            };
            (worker, entry.input_rules, runtime)
        };
        let writes = build_input_writes(text, rules)?;
        if !writes.submit_delay.is_zero() {
            return self
                .write_delayed_waited_input(
                    session_id,
                    worker,
                    writes,
                    DelayedSubmit {
                        input_guard,
                        runtime,
                        deadline,
                        ack_delay: plan_ack_delay,
                        progress,
                    },
                    #[cfg(test)]
                    send_boundary,
                )
                .await;
        }
        let reservation = self
            .reserve_worker_before_deadline(
                &worker,
                worker_input_fragments(writes, plan_ack_delay),
                deadline,
            )
            .await?;
        let boundary = self
            .commit_worker_before_deadline(
                reservation,
                input_guard,
                session_id.clone(),
                runtime.clone(),
                deadline,
                progress,
                #[cfg(test)]
                send_boundary,
            )
            .await?;
        #[cfg(test)]
        self.hold_after_input_sent().await;
        self.input_activity_snapshot(session_id, Some(&runtime))
            .await?;

        Ok(InputSubmission {
            runtime,
            activity_epoch: self.daemon_instance_id().to_owned(),
            after_revision: boundary.revision,
            deadline,
            idle_requires_working: false,
            prompt_mark: submission_prompt_mark(&boundary),
            prompt_capable: boundary.prompt_mark.is_some(),
            prompt_opt_in: false,
        })
    }

    /// Waited input for delayed-submit framing: the daemon writes the body,
    /// waits out the delay, re-checks the agent and only then writes Enter, so
    /// a prompt that appears during the delay is never answered by Enter.
    /// The session input gate and the worker connection stay held across both
    /// writes.
    async fn write_delayed_waited_input(
        &self,
        session_id: &SessionId,
        worker: Worker,
        writes: InputWritePlan,
        submit: DelayedSubmit,
        #[cfg(test)] send_boundary: Option<std::sync::Arc<tokio::sync::Barrier>>,
    ) -> Result<InputSubmission, ProtocolError> {
        let DelayedSubmit {
            input_guard,
            runtime,
            deadline,
            ack_delay,
            progress,
        } = submit;
        let delay = writes.submit_delay;
        let body_empty = writes.body.is_empty();
        let first_plan = if body_empty {
            enter_fragment(ack_delay)
        } else {
            body_fragment(writes.body)
        };
        let reservation = self
            .reserve_worker_before_deadline(&worker, vec![first_plan], deadline)
            .await?;
        let (body_started_tx, body_started_rx) = tokio::sync::oneshot::channel();
        let mut write = self.spawn_background(
            "delayed_submit",
            run_delayed_submit(
                self.clone(),
                DelayedSubmitRun {
                    session_id: session_id.clone(),
                    runtime: runtime.clone(),
                    delay,
                    ack_delay,
                    deadline,
                    body_empty,
                    progress: std::sync::Arc::clone(&progress),
                    body_started_tx,
                    #[cfg(test)]
                    send_boundary,
                    #[cfg(test)]
                    delay_boundary: self
                        .inner
                        .input_delay_hold
                        .lock()
                        .expect("input delay hold test lock")
                        .take(),
                },
                reservation,
                input_guard,
            ),
        );
        tokio::pin!(body_started_rx);

        tokio::select! {
            biased;
            started = &mut body_started_rx => {
                if started.is_err() {
                    let boundary = worker_write_task_result(write.await)?;
                    return self.delayed_submission(session_id, runtime, boundary, deadline).await;
                }
            }
            result = &mut write => {
                let boundary = worker_write_task_result(result)?;
                return self.delayed_submission(session_id, runtime, boundary, deadline).await;
            }
            () = self.inner.daemon_shutdown.cancelled() => {
                write.abort();
                let _result = write.await;
                return Err(input_wait_shutdown_error(
                    "daemon shutdown cancelled input before worker delivery",
                ));
            }
            () = tokio::time::sleep_until(deadline) => {
                write.abort();
                let _result = write.await;
                return Err(ProtocolError::session_input_timeout());
            }
        }

        let result = tokio::select! {
            biased;
            result = &mut write => worker_write_task_result(result),
            () = self.inner.daemon_shutdown.cancelled() => {
                return Err(self.give_up_delayed_submit(
                    write,
                    &progress,
                    InputNotSubmittedReason::DaemonShuttingDown,
                    input_wait_shutdown_error(
                        "daemon shutdown cancelled input after worker delivery began",
                    ),
                ));
            }
            () = tokio::time::sleep_until(deadline) => {
                return Err(self.give_up_delayed_submit(
                    write,
                    &progress,
                    InputNotSubmittedReason::DeadlineElapsed,
                    ProtocolError::session_input_timeout(),
                ));
            }
        };
        self.delayed_submission(session_id, runtime, result?, deadline)
            .await
    }

    async fn delayed_submission(
        &self,
        session_id: &SessionId,
        runtime: SessionRuntimeIdentity,
        boundary: InputActivitySnapshot,
        deadline: tokio::time::Instant,
    ) -> Result<InputSubmission, ProtocolError> {
        #[cfg(test)]
        self.hold_after_input_sent().await;
        self.input_activity_snapshot(session_id, Some(&runtime))
            .await?;
        Ok(InputSubmission {
            runtime,
            activity_epoch: self.daemon_instance_id().to_owned(),
            after_revision: boundary.revision,
            deadline,
            idle_requires_working: true,
            prompt_mark: submission_prompt_mark(&boundary),
            prompt_capable: boundary.prompt_mark.is_some(),
            prompt_opt_in: false,
        })
    }

    /// Ends a delayed submit whose caller ran out of time. Before Enter is
    /// claimed the submit stops with `session_input_not_submitted`; once Enter
    /// was claimed its acknowledgement is consumed in the background and the
    /// caller gets `after_enter`.
    fn give_up_delayed_submit(
        &self,
        write: tokio::task::JoinHandle<Result<InputActivitySnapshot, ProtocolError>>,
        progress: &SubmitProgress,
        reason: InputNotSubmittedReason,
        after_enter: ProtocolError,
    ) -> ProtocolError {
        if progress.abandon(reason) {
            self.spawn_background("abandoned_submit_waiter", async move {
                match write.await {
                    Ok(Err(error)) if error.code == "session_input_not_submitted" => {}
                    Ok(Err(error)) => warn!(
                        error = %error,
                        "worker input failed after the submit was abandoned before Enter"
                    ),
                    Ok(Ok(_boundary)) => warn!("Enter was written after the submit was abandoned"),
                    Err(error) => warn!(
                        error = %error,
                        "worker input task failed after the submit was abandoned before Enter"
                    ),
                }
            });
            ProtocolError::session_input_not_submitted(reason)
        } else {
            self.shield_worker_ack(write);
            after_enter
        }
    }

    async fn input_gate(
        &self,
        session_id: &SessionId,
    ) -> Result<std::sync::Arc<tokio::sync::Mutex<()>>, ProtocolError> {
        self.ensure_not_external(session_id).await?;
        let sessions = self.inner.sessions.lock().await;
        sessions
            .get(session_id)
            .map(|entry| std::sync::Arc::clone(&entry.input_gate))
            .ok_or_else(|| session_not_found(&session_id.0))
    }

    async fn lock_input_before_deadline(
        &self,
        input_gate: std::sync::Arc<tokio::sync::Mutex<()>>,
        deadline: tokio::time::Instant,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, ProtocolError> {
        tokio::select! {
            biased;
            () = self.inner.daemon_shutdown.cancelled() => Err(input_wait_shutdown_error(
                "daemon shutdown cancelled input while waiting for the session input gate",
            )),
            () = tokio::time::sleep_until(deadline) => {
                Err(ProtocolError::session_input_timeout())
            }
            guard = input_gate.lock_owned() => Ok(guard),
        }
    }

    async fn reserve_worker_before_deadline(
        &self,
        worker: &Worker,
        fragments: Vec<WorkerInputFragment>,
        deadline: tokio::time::Instant,
    ) -> Result<WriteReservation, ProtocolError> {
        if tokio::time::Instant::now() >= deadline {
            return Err(ProtocolError::session_input_timeout());
        }
        tokio::select! {
            biased;
            () = self.inner.daemon_shutdown.cancelled() => Err(input_wait_shutdown_error(
                "daemon shutdown cancelled input before worker delivery",
            )),
            () = tokio::time::sleep_until(deadline) => {
                Err(ProtocolError::session_input_timeout())
            }
            result = worker.reserve_write(fragments) => result.map_err(worker_error_to_protocol),
        }
    }

    #[cfg_attr(
        test,
        allow(
            clippy::too_many_arguments,
            reason = "the test build adds the send boundary"
        )
    )]
    async fn commit_worker_before_deadline(
        &self,
        reservation: WriteReservation,
        input_guard: tokio::sync::OwnedMutexGuard<()>,
        session_id: SessionId,
        runtime: SessionRuntimeIdentity,
        deadline: tokio::time::Instant,
        progress: std::sync::Arc<SubmitProgress>,
        #[cfg(test)] send_boundary: Option<std::sync::Arc<tokio::sync::Barrier>>,
    ) -> Result<InputActivitySnapshot, ProtocolError> {
        let (send_started_tx, send_started_rx) = tokio::sync::oneshot::channel();
        let registry = self.clone();
        let turns = self.inner.turns.clone();
        let mut write = self.spawn_background("worker_input", async move {
            let _input_guard = input_guard;
            let sessions = registry.inner.sessions.lock().await;
            let entry = sessions
                .get(&session_id)
                .ok_or_else(|| session_not_found(&session_id.0))?;
            let boundary = input_activity_snapshot_for_entry(entry, &session_id, Some(&runtime))?;
            reject_blocked_wait(boundary.activity)?;
            #[cfg(test)]
            if let Some(send_boundary) = send_boundary {
                send_boundary.wait().await;
                send_boundary.wait().await;
            }
            let completed_boundary = boundary.clone();
            let completion = reservation
                .commit(move || {
                    turns.note_input(&session_id);
                    let _sent = progress.claim_enter();
                    let _result = send_started_tx.send(());
                    drop(sessions);
                })
                .await
                .map_err(worker_error_to_protocol)?;
            Ok(InputActivitySnapshot {
                prompt_mark: completion.submit_mark_ns,
                ..completed_boundary
            })
        });
        tokio::pin!(send_started_rx);

        tokio::select! {
            biased;
            started = &mut send_started_rx => {
                if started.is_err() {
                    return worker_write_task_result(write.await);
                }
            }
            result = &mut write => return worker_write_task_result(result),
            () = self.inner.daemon_shutdown.cancelled() => {
                write.abort();
                let _result = write.await;
                return Err(input_wait_shutdown_error(
                    "daemon shutdown cancelled input before worker delivery",
                ));
            }
            () = tokio::time::sleep_until(deadline) => {
                write.abort();
                let _result = write.await;
                return Err(ProtocolError::session_input_timeout());
            }
        }

        tokio::select! {
            biased;
            result = &mut write => worker_write_task_result(result),
            () = self.inner.daemon_shutdown.cancelled() => {
                self.shield_worker_ack(write);
                Err(input_wait_shutdown_error(
                    "daemon shutdown cancelled input after worker delivery began",
                ))
            }
            () = tokio::time::sleep_until(deadline) => {
                self.shield_worker_ack(write);
                Err(ProtocolError::session_input_timeout())
            }
        }
    }

    pub(super) async fn input_activity_snapshot(
        &self,
        session_id: &SessionId,
        expected_runtime: Option<&SessionRuntimeIdentity>,
    ) -> Result<InputActivitySnapshot, ProtocolError> {
        let sessions = self.inner.sessions.lock().await;
        let entry = sessions
            .get(session_id)
            .ok_or_else(|| session_not_found(&session_id.0))?;
        input_activity_snapshot_for_entry(entry, session_id, expected_runtime)
    }

    pub(super) async fn await_input_settled(
        &self,
        session_id: &SessionId,
        wait: SessionInputWait,
        submission: InputSubmission,
        events: &mut broadcast::Receiver<protocol::Event>,
    ) -> Result<SessionInputResult, ProtocolError> {
        let until = wait.until.as_deref().unwrap_or_default();
        let targets: &[AgentActivity] = if until.is_empty() {
            &DEFAULT_INPUT_WAIT_UNTIL
        } else {
            until
        };
        // Only a `working` target can be answered by a prompt acceptance. The
        // receiver starts changed so the first pass reads acceptances imported
        // before the wait began.
        let mut waits_for_prompt =
            submission.accepts_prompt() && targets.contains(&AgentActivity::Working);
        let mut prompt_wake = self.inner.prompt_wake.subscribe();
        prompt_wake.mark_changed();
        loop {
            tokio::select! {
                biased;
                () = self.inner.daemon_shutdown.cancelled() => {
                    return Err(input_wait_shutdown_error(
                        "daemon shutdown cancelled the bounded input wait",
                    ));
                }
                () = tokio::time::sleep_until(submission.deadline) => {
                    if let Some(result) = self
                        .evaluate_input_wait(
                            session_id,
                            targets,
                            &submission,
                            None,
                        )
                        .await?
                    {
                        return Ok(result);
                    }
                    return Err(submission.timeout_error(session_id, targets));
                }
                woke = prompt_wake.changed(), if waits_for_prompt => {
                    waits_for_prompt = woke.is_ok();
                }
                received = events.recv() => match received {
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(input_wait_shutdown_error(
                            "daemon event channel closed during bounded input wait",
                        ));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Ok(event) => {
                        if event_concerns_other_session(&event, session_id) {
                            continue;
                        }
                        self.input_activity_snapshot(session_id, Some(&submission.runtime)).await?;
                        if event.event() == protocol::event::AGENT_STATE {
                            let state: AgentStateEvent = serde_json::from_value(event.payload().clone())
                                .map_err(|_error| {
                                    ProtocolError::new(
                                        ErrorClass::Daemon,
                                        "daemon_event_invalid",
                                        "agent state event payload was invalid during input wait",
                                        None,
                                    )
                            })?;
                            if state.session_id == *session_id
                                && state.runtime.as_ref() == Some(&submission.runtime)
                                && state.activity_epoch.as_deref()
                                    == Some(submission.activity_epoch.as_str())
                                && targets.contains(&state.activity)
                            {
                                if let Some(revision) = state.revision {
                                    if let Some(result) = self
                                        .evaluate_input_wait(
                                            session_id,
                                            targets,
                                            &submission,
                                            Some(revision),
                                        )
                                        .await?
                                    {
                                        return Ok(result);
                                    }
                                }
                            }
                        }
                    }
                },
            }
            if let Some(result) = self
                .evaluate_input_wait(session_id, targets, &submission, None)
                .await?
            {
                return Ok(result);
            }
        }
    }

    pub(super) async fn evaluate_input_wait(
        &self,
        session_id: &SessionId,
        targets: &[AgentActivity],
        submission: &InputSubmission,
        event_revision: Option<ActivityRevision>,
    ) -> Result<Option<SessionInputResult>, ProtocolError> {
        let sessions = self.inner.sessions.lock().await;
        let entry = sessions
            .get(session_id)
            .ok_or_else(|| session_not_found(&session_id.0))?;
        let current_runtime = input_runtime_identity(entry, session_id)?;
        if current_runtime != submission.runtime {
            return Err(ProtocolError::session_runtime_changed());
        }
        let accepted = submission.accepts_prompt() && prompt_acceptance_recorded(entry, submission);
        let reported = submission.prompt_opt_in.then_some(accepted);
        if let Some(evidence) = matching_input_evidence(entry, targets, submission, event_revision)
        {
            return Ok(Some(input_wait_result(evidence.clone(), reported)));
        }
        Ok((accepted && targets.contains(&AgentActivity::Working))
            .then(|| prompt_wait_result(submission)))
    }

    /// Whether `working` evidence of the submission's own runtime sits above
    /// its boundary, whatever became of the session since.
    async fn working_evidence_recorded(
        &self,
        session_id: &SessionId,
        submission: &InputSubmission,
    ) -> bool {
        let sessions = self.inner.sessions.lock().await;
        sessions.get(session_id).is_some_and(|entry| {
            matching_input_evidence(entry, &[AgentActivity::Working], submission, None).is_some()
                || (submission.accepts_prompt() && prompt_acceptance_recorded(entry, submission))
        })
    }

    /// Imports the prompt acceptance a worker snapshot reports for the live
    /// runtime, then wakes the bounded input waits.
    ///
    /// The acceptance is delivery evidence only. It is never recorded as
    /// activity, never bumps the activity revision, and never touches the turn
    /// journal, so a hook report cannot arm or close a turn.
    pub(super) async fn observe_prompt_accepted(&self, id: &SessionId, snapshot: &InspectSnapshot) {
        let Some(sequence) = snapshot.prompt_accepted_sequence else {
            return;
        };
        let imported = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                return;
            };
            let Some(runtime) = live_runtime_identity(entry) else {
                return;
            };
            if snapshot
                .worker_instance_id
                .as_ref()
                .is_none_or(|instance| instance.as_str() != runtime.worker_instance_id())
            {
                return;
            }
            let seen = match &entry.prompt_sequence_seen {
                Some((seen_runtime, seen)) if *seen_runtime == runtime => Some(*seen),
                _ => None,
            };
            if seen.is_some_and(|seen| sequence <= seen) {
                return;
            }
            if seen.is_none() {
                entry.prompt_acceptances.clear();
            }
            entry.prompt_sequence_seen = Some((runtime.clone(), sequence));
            let received_at = tokio::time::Instant::now();
            if let Some(cutoff) = received_at.checked_sub(super::ACTIVITY_EVIDENCE_RETENTION) {
                while entry
                    .prompt_acceptances
                    .front()
                    .is_some_and(|previous| previous.received_at < cutoff)
                {
                    entry.prompt_acceptances.pop_front();
                }
            }
            entry.prompt_acceptances.push_back(PromptAcceptance {
                runtime,
                activity_epoch: self.daemon_instance_id().to_owned(),
                sequence,
                received_at,
            });
            while entry.prompt_acceptances.len() > super::MAX_ACTIVITY_EVIDENCE_HISTORY {
                entry.prompt_acceptances.pop_front();
            }
            true
        };
        if imported {
            self.inner
                .prompt_wake
                .send_modify(|wakes| *wakes = wakes.wrapping_add(1));
        }
    }

    fn validate_input_wait(wait: &SessionInputWait) -> Result<(), ProtocolError> {
        if wait.timeout_ms == Some(0) {
            return Err(ProtocolError::observation(
                "session_input_invalid_wait",
                "timeout_ms must be greater than zero",
            ));
        }
        if wait
            .timeout_ms
            .is_some_and(|timeout| timeout > protocol::MAX_SESSION_WAIT_MS)
        {
            return Err(ProtocolError::session_wait_limit_exceeded());
        }

        Ok(())
    }
}

/// The earliest evidence of the submission's runtime above its boundary, up to
/// its deadline, whose activity is one of `targets`. On delayed-submit framing
/// an `idle` target counts only after `working` evidence.
fn matching_input_evidence<'a>(
    entry: &'a SessionEntry,
    targets: &[AgentActivity],
    submission: &InputSubmission,
    event_revision: Option<ActivityRevision>,
) -> Option<&'a ActivityEvidence> {
    let after_submit = |evidence: &&ActivityEvidence| {
        evidence.runtime == submission.runtime
            && evidence.activity_epoch == submission.activity_epoch
            && evidence.revision > submission.after_revision
            && evidence.observed_at <= submission.deadline
    };
    let first_working = submission
        .idle_requires_working
        .then(|| {
            entry
                .activity_evidence
                .iter()
                .filter(after_submit)
                .filter(|evidence| evidence.activity == AgentActivity::Working)
                .map(|evidence| evidence.revision)
                .min()
        })
        .flatten();
    entry
        .activity_evidence
        .iter()
        .filter(after_submit)
        .filter(|evidence| {
            targets.contains(&evidence.activity)
                && event_revision.is_none_or(|revision| evidence.revision == revision)
                && (!submission.idle_requires_working
                    || evidence.activity != AgentActivity::Idle
                    || first_working.is_some_and(|working| working < evidence.revision))
        })
        .min_by_key(|evidence| evidence.revision)
}

fn input_activity_snapshot_for_entry(
    entry: &SessionEntry,
    session_id: &SessionId,
    expected_runtime: Option<&SessionRuntimeIdentity>,
) -> Result<InputActivitySnapshot, ProtocolError> {
    let runtime = input_runtime_identity(entry, session_id)?;
    if expected_runtime.is_some_and(|expected| expected != &runtime) {
        return Err(ProtocolError::session_runtime_changed());
    }
    Ok(InputActivitySnapshot {
        activity: entry.info.activity,
        revision: ActivityRevision::new(entry.activity_revision),
        runtime,
        prompt_mark: None,
    })
}

fn input_runtime_identity(
    entry: &SessionEntry,
    session_id: &SessionId,
) -> Result<SessionRuntimeIdentity, ProtocolError> {
    if entry.info.state != SessionState::Running {
        return Err(not_running_refusal(session_id, entry));
    }
    let runtime = entry
        .info
        .runtime
        .as_ref()
        .ok_or_else(ProtocolError::session_terminal_unavailable)?;
    if runtime.state != protocol::RuntimeState::Live {
        return Err(unavailable_runtime_error(
            session_id,
            runtime.state,
            runtime.loss_reason.as_deref(),
        ));
    }
    let worker_instance_id = runtime
        .worker_instance_id
        .clone()
        .ok_or_else(ProtocolError::session_terminal_unavailable)?;
    SessionRuntimeIdentity::new(worker_instance_id, runtime.runtime_generation)
        .map_err(|_error| ProtocolError::session_terminal_unavailable())
}

fn input_wait_result(
    evidence: ActivityEvidence,
    prompt_accepted: Option<bool>,
) -> SessionInputResult {
    SessionInputResult {
        accepted: true,
        activity: Some(evidence.activity),
        activity_source: Some(evidence.source),
        runtime: Some(evidence.runtime),
        activity_epoch: Some(evidence.activity_epoch),
        activity_revision: Some(evidence.revision),
        submitted: Some(true),
        prompt_accepted,
    }
}

/// The result of a wait answered by a prompt acceptance alone: no activity was
/// observed, so none is stated.
fn prompt_wait_result(submission: &InputSubmission) -> SessionInputResult {
    SessionInputResult {
        accepted: true,
        activity: None,
        activity_source: None,
        runtime: Some(submission.runtime.clone()),
        activity_epoch: Some(submission.activity_epoch.clone()),
        activity_revision: None,
        submitted: Some(true),
        prompt_accepted: Some(true),
    }
}

/// The submit mark a completed write contributes to its submission: dropped
/// when the agent was `working` at the boundary, because a busy agent queues
/// the input and its own `working` evidence already answers the wait.
fn submission_prompt_mark(boundary: &InputActivitySnapshot) -> Option<u64> {
    boundary
        .prompt_mark
        .filter(|_| boundary.activity != Some(AgentActivity::Working))
}

/// Whether the entry holds a prompt acceptance of the submission's runtime
/// that the agent reported after the submit mark and before the deadline.
///
/// An acceptance proves the agent started its `UserPromptSubmit` hooks, not
/// that the prompt reached the model: a parallel user hook that blocks the
/// prompt can still discard it, and a turn the agent starts on its own before
/// reading Enter is indistinguishable. Callers that need the work to have
/// started wait for `working` activity.
fn prompt_acceptance_recorded(entry: &SessionEntry, submission: &InputSubmission) -> bool {
    let Some(mark) = submission.prompt_mark else {
        return false;
    };
    entry.prompt_acceptances.iter().any(|acceptance| {
        acceptance.runtime == submission.runtime
            && acceptance.activity_epoch == submission.activity_epoch
            && acceptance.sequence > mark
            && acceptance.received_at <= submission.deadline
    })
}

fn input_wait_shutdown_error(message: &str) -> ProtocolError {
    ProtocolError::new(ErrorClass::Daemon, "daemon_shutting_down", message, None)
}

fn worker_input_task_error(_error: &tokio::task::JoinError) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "worker_operation_failed",
        "worker input task failed",
        None,
    )
}

fn worker_write_task_result(
    result: Result<Result<InputActivitySnapshot, ProtocolError>, tokio::task::JoinError>,
) -> Result<InputActivitySnapshot, ProtocolError> {
    result.map_err(|error| worker_input_task_error(&error))?
}

impl SessionRegistry {
    /// Consumes the acknowledgement of a worker input whose caller stopped waiting.
    fn shield_worker_ack(
        &self,
        write: tokio::task::JoinHandle<Result<InputActivitySnapshot, ProtocolError>>,
    ) {
        self.spawn_background("input_acknowledgement_waiter", async move {
        match write.await {
            Ok(Ok(_boundary)) => {}
            Ok(Err(error)) => warn!(
                error = %error,
                "worker input failed after the public deadline while consuming its acknowledgement"
            ),
            Err(error) => warn!(
                error = %error,
                "worker input task failed after the public deadline"
            ),
        }
        });
    }
}

pub(super) fn plan_initial_input_delivery(
    resolved: &ResolvedAgent,
    mut command: LaunchCommand,
    initial_input: Option<String>,
) -> LaunchCommandPlan {
    if resolved.profile.is_none() && resolved.definition.prompt_arg() {
        let prompt_in_argv = initial_input.is_some();
        if let Some(input) = initial_input {
            command.args.push(input);
        }
        return LaunchCommandPlan {
            command,
            pending_initial_input: None,
            prompt_in_argv,
        };
    }

    LaunchCommandPlan {
        command,
        pending_initial_input: initial_input,
        prompt_in_argv: false,
    }
}

/// Input rules for `definition`: the descriptor's framing, with its submit
/// delay replaced for the runtimes whose descriptor allows one — the
/// per-runtime override first, then the host setting, never a runtime name
/// here.
pub(super) fn input_rules_for_definition(
    definition: &RuntimeDefinition,
    config: &SessionRegistryConfig,
) -> InputRules {
    if definition.submit_delay_configurable() {
        let mut rules = definition.input_rules();
        let delay = config
            .submit_delay_overrides
            .get(definition.runtime_id())
            .copied()
            .or(config.host_submit_delay);
        if let Some(delay) = delay {
            rules.submit_delay = delay;
        }
        rules
    } else {
        definition.input_rules()
    }
}

/// [`input_rules_for_definition`] for the runtime `agent` names; a kind
/// without an installed definition gets unrestricted, unframed input.
///
/// Used for a record without a recovery binding, which carries neither a
/// launch pin nor its launch-time framing, so the definition serving the
/// runtime id now is the only framing and safety contract there is.
pub(super) fn input_rules_for_agent(
    host: &RuntimeHost,
    agent: &RuntimeRef,
    config: &SessionRegistryConfig,
) -> InputRules {
    host.resolve_ref(agent).map_or_else(
        |_unresolved| crate::agent::input_rules_for_kind(host, agent),
        |definition| input_rules_for_definition(&definition, config),
    )
}

pub(super) fn build_input_writes(
    text: &str,
    rules: InputRules,
) -> Result<InputWritePlan, ProtocolError> {
    rules.validate_text(text)?;
    let mut body = Vec::new();
    if rules.bracketed_paste {
        body.extend_from_slice(BRACKETED_PASTE_START);
    }
    body.extend_from_slice(text.as_bytes());
    if rules.bracketed_paste {
        body.extend_from_slice(BRACKETED_PASTE_END);
    }
    Ok(InputWritePlan {
        body,
        submit_delay: rules.submit_delay,
    })
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn reject_blocked_wait(activity: Option<AgentActivity>) -> Result<(), ProtocolError> {
    if activity == Some(AgentActivity::Blocked) {
        Err(ProtocolError::session_agent_blocked())
    } else {
        Ok(())
    }
}

fn body_fragment(body: Vec<u8>) -> WorkerInputFragment {
    WorkerInputFragment {
        bytes: SecretBytes::new(body),
        delay_after_ms: 0,
    }
}

fn enter_fragment(acknowledgement_delay: Duration) -> WorkerInputFragment {
    WorkerInputFragment {
        bytes: SecretBytes::new(SUBMIT.to_vec()),
        delay_after_ms: duration_millis(acknowledgement_delay),
    }
}

/// Everything a delayed waited input carries into its writing task.
struct DelayedSubmit {
    input_guard: tokio::sync::OwnedMutexGuard<()>,
    runtime: SessionRuntimeIdentity,
    deadline: tokio::time::Instant,
    ack_delay: Duration,
    progress: std::sync::Arc<SubmitProgress>,
}

struct DelayedSubmitRun {
    session_id: SessionId,
    runtime: SessionRuntimeIdentity,
    delay: Duration,
    ack_delay: Duration,
    deadline: tokio::time::Instant,
    /// The text frames to no body at all, so only Enter is written.
    body_empty: bool,
    progress: std::sync::Arc<SubmitProgress>,
    body_started_tx: tokio::sync::oneshot::Sender<()>,
    #[cfg(test)]
    send_boundary: Option<std::sync::Arc<tokio::sync::Barrier>>,
    #[cfg(test)]
    delay_boundary: Option<std::sync::Arc<tokio::sync::Barrier>>,
}

/// Writes the body, waits `delay`, re-checks the agent under the session lock
/// and writes Enter, returning the activity boundary captured immediately
/// before Enter's send began. The task owns the input gate until its last
/// acknowledgement is consumed, so it outlives a caller that gave up.
async fn run_delayed_submit(
    registry: SessionRegistry,
    run: DelayedSubmitRun,
    mut reservation: WriteReservation,
    input_guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<InputActivitySnapshot, ProtocolError> {
    let _input_guard = input_guard;
    let DelayedSubmitRun {
        session_id,
        runtime,
        delay,
        ack_delay,
        deadline,
        body_empty,
        progress,
        body_started_tx,
        #[cfg(test)]
        send_boundary,
        #[cfg(test)]
        delay_boundary,
    } = run;

    if body_empty {
        // Nothing to type: the reservation already holds the Enter plan.
        let _result = body_started_tx.send(());
    } else {
        let sessions = registry.inner.sessions.lock().await;
        let entry = sessions
            .get(&session_id)
            .ok_or_else(|| session_not_found(&session_id.0))?;
        let snapshot = input_activity_snapshot_for_entry(entry, &session_id, Some(&runtime))?;
        reject_blocked_wait(snapshot.activity)?;
        // A deadline that cannot outlast the submit delay would strand the
        // typed body in the input box, so nothing is written.
        if deadline.saturating_duration_since(tokio::time::Instant::now()) <= delay {
            return Err(ProtocolError::session_input_timeout());
        }
        reservation
            .commit_retaining(move || {
                let _result = body_started_tx.send(());
                drop(sessions);
            })
            .await
            .map_err(worker_error_to_protocol)?;

        #[cfg(test)]
        if let Some(delay_boundary) = delay_boundary {
            delay_boundary.wait().await;
            delay_boundary.wait().await;
        }
        tokio::select! {
            biased;
            () = progress.abandon.cancelled() => return Err(progress.not_submitted()),
            () = tokio::time::sleep(delay) => {}
        }
    }

    let sessions = registry.inner.sessions.lock().await;
    let entry = sessions
        .get(&session_id)
        .ok_or_else(|| session_not_found(&session_id.0))?;
    let boundary = input_activity_snapshot_for_entry(entry, &session_id, Some(&runtime))?;
    if boundary.activity == Some(AgentActivity::Blocked) {
        return Err(ProtocolError::session_input_not_submitted(
            InputNotSubmittedReason::AgentBlocked,
        ));
    }
    if !body_empty {
        reservation
            .prepare_next(vec![enter_fragment(ack_delay)])
            .map_err(worker_error_to_protocol)?;
    }
    #[cfg(test)]
    if let Some(send_boundary) = send_boundary {
        send_boundary.wait().await;
        send_boundary.wait().await;
    }
    if !progress.claim_enter() {
        return Err(progress.not_submitted());
    }
    let turns = registry.inner.turns.clone();
    let completion = reservation
        .commit_retaining(move || {
            turns.note_input(&session_id);
            drop(sessions);
        })
        .await
        .map_err(worker_error_to_protocol)?;
    Ok(InputActivitySnapshot {
        prompt_mark: completion.submit_mark_ns,
        ..boundary
    })
}

pub(super) fn worker_input_fragments(
    writes: InputWritePlan,
    acknowledgement_delay: Duration,
) -> Vec<WorkerInputFragment> {
    vec![
        WorkerInputFragment {
            bytes: SecretBytes::new(writes.body),
            delay_after_ms: duration_millis(writes.submit_delay),
        },
        WorkerInputFragment {
            bytes: SecretBytes::new(SUBMIT.to_vec()),
            delay_after_ms: duration_millis(acknowledgement_delay),
        },
    ]
}
