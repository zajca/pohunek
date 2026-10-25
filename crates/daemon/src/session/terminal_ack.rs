//! Acknowledgement of a committed terminal outcome.
//!
//! A worker keeps serving its exited runtime until the daemon that durably
//! committed the outcome acknowledges it. The acknowledgement makes the worker
//! exit, and the daemon then retires the job of that exact generation, so a
//! stopped or finished session holds neither a worker process nor a
//! supervisor job.

use protocol::SessionId;
use tracing::{debug, warn};

use pohunek_platform::process::{ProcessIdentity, StartIdentity};

use super::reconcile::Retirement;
use super::{Generation, RuntimeWatchIdentity, SessionRegistry};
use crate::runtime::Worker;
use crate::store::DesiredState;

/// What an acknowledgement task needs, captured at the terminal commit.
#[derive(Debug, Clone)]
pub(super) struct TerminalAcknowledgement {
    pub(super) worker: Worker,
    pub(super) expected: RuntimeWatchIdentity,
    pub(super) generation: Option<Generation>,
}

impl SessionRegistry {
    /// Acknowledges the terminal outcome in the background.
    ///
    /// Called by the one commit that made `ack.expected` terminal, so each
    /// generation is acknowledged once per commit.
    pub(super) fn spawn_terminal_acknowledgement(
        &self,
        id: SessionId,
        ack: TerminalAcknowledgement,
    ) {
        #[cfg(test)]
        if self
            .inner
            .terminal_acknowledgement_suppressed
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let registry = self.clone();
        self.spawn_transaction("terminal_acknowledgement", async move {
            registry.acknowledge_terminal_runtime(&id, ack).await;
        });
    }

    /// Makes the registry skip every later terminal acknowledgement, as a
    /// daemon that ended between the terminal commit and its acknowledgement.
    #[cfg(test)]
    pub(crate) fn suppress_terminal_acknowledgement(&self) {
        self.inner
            .terminal_acknowledgement_suppressed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The acknowledgement of a terminal entry the reconciliation just
    /// committed, with the identity the entry now carries.
    pub(super) async fn acknowledge_committed_terminal(
        &self,
        id: &SessionId,
        worker: Worker,
        generation: Option<Generation>,
    ) {
        let expected = {
            let sessions = self.inner.sessions.lock().await;
            sessions.get(id).and_then(|entry| {
                (super::is_terminal(entry.info.state)
                    && entry.desired_state != DesiredState::Removed)
                    .then(|| RuntimeWatchIdentity::from_info(&entry.info))
                    .flatten()
            })
        };
        if let Some(expected) = expected {
            self.spawn_terminal_acknowledgement(
                id.clone(),
                TerminalAcknowledgement {
                    worker,
                    expected,
                    generation,
                },
            );
        }
    }

    /// Whether `expected` is still the terminal runtime of a session that is
    /// not being removed; a removal retires the job itself.
    async fn terminal_acknowledgement_applies(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
    ) -> bool {
        let sessions = self.inner.sessions.lock().await;
        sessions.get(id).is_some_and(|entry| {
            super::is_terminal(entry.info.state)
                && entry.desired_state != DesiredState::Removed
                && expected.matches(entry)
        })
    }

    /// Arms a hold that parks the next terminal acknowledgement of session
    /// `id` before it takes the lifecycle lock.
    #[cfg(test)]
    pub(crate) fn hold_next_terminal_acknowledgement(
        &self,
        id: &SessionId,
    ) -> crate::runtime::lifecycle::tests::StartGate {
        let gate = crate::runtime::lifecycle::tests::StartGate {
            session_id: Some(id.0.clone()),
            entered: std::sync::Arc::new(tokio::sync::Notify::new()),
            release: std::sync::Arc::new(tokio::sync::Notify::new()),
        };
        *self
            .inner
            .terminal_acknowledgement_hold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(gate.clone());
        gate
    }

    #[cfg(test)]
    fn worker_drains_streams(&self, snapshot: &pohunek_worker_protocol::InspectSnapshot) -> bool {
        snapshot.drains_streams_on_terminal_ack
            && !self
                .inner
                .terminal_acknowledgement_old_worker
                .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(not(test))]
    fn worker_drains_streams(&self, snapshot: &pohunek_worker_protocol::InspectSnapshot) -> bool {
        let _ = self;
        snapshot.drains_streams_on_terminal_ack
    }

    /// Makes the registry treat every worker as one that does not drain
    /// its streams on the acknowledgement.
    #[cfg(test)]
    pub(crate) fn model_worker_without_stream_drain(&self) {
        self.inner
            .terminal_acknowledgement_old_worker
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Makes the next terminal acknowledgement fail after the worker took it,
    /// the way a lost answer does.
    #[cfg(test)]
    pub(crate) fn fail_next_terminal_acknowledgement(&self) {
        self.inner
            .terminal_acknowledgement_fails
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    async fn hold_terminal_acknowledgement(&self, id: &SessionId) {
        let gate = self
            .inner
            .terminal_acknowledgement_hold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_if(|gate| gate.session_id.as_deref().is_none_or(|held| held == id.0));
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
    }

    async fn acknowledge_terminal_runtime(&self, id: &SessionId, ack: TerminalAcknowledgement) {
        #[cfg(test)]
        self.hold_terminal_acknowledgement(id).await;
        // An in-flight stop, removal or resume finishes first, so the
        // acknowledgement never races the stop's own response.
        let barrier = self.lock_lifecycle(id).await;
        let applies = self
            .terminal_acknowledgement_applies(id, &ack.expected)
            .await;
        drop(barrier);
        if !applies {
            return;
        }
        // A worker that does not drain its attached streams exits at once on
        // the acknowledgement and would cut them off, so it keeps its retention.
        let worker_process = match ack.worker.inspect().await {
            Ok(snapshot) if self.worker_drains_streams(&snapshot) => snapshot.worker_process,
            Ok(_) => {
                debug!(
                    session_id = %id.0,
                    worker_instance_id = %ack.expected.worker_instance_id,
                    "worker does not drain streams on acknowledgement; it keeps its retention"
                );
                return;
            }
            Err(error) => {
                warn!(
                    name: "session.terminal.acknowledge_failed",
                    session_id = %id.0,
                    worker_instance_id = %ack.expected.worker_instance_id,
                    error = %error,
                    "terminal acknowledgement failed: the worker could not be inspected; it keeps its retention unless it ended"
                );
                self.schedule_supervision_retry(id);
                return;
            }
        };
        // The worker cancels its shutdown before it answers, so a lost answer
        // may follow an acknowledgement that took effect. The supervision
        // retry retires the generation only once its socket is gone and its
        // journal is terminal; a worker that still answers keeps its retention.
        #[cfg(test)]
        let sent = if self
            .inner
            .terminal_acknowledgement_fails
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            Err(crate::runtime::WorkerError::Protocol(
                "injected lost answer".to_owned(),
            ))
        } else {
            ack.worker.acknowledge_terminal().await
        };
        #[cfg(not(test))]
        let sent = ack.worker.acknowledge_terminal().await;
        if let Err(error) = sent {
            warn!(
                name: "session.terminal.acknowledge_failed",
                session_id = %id.0,
                worker_instance_id = %ack.expected.worker_instance_id,
                error = %error,
                "terminal acknowledgement failed; the worker keeps its retention unless it ended"
            );
            self.schedule_supervision_retry(id);
            return;
        }
        let _guard = self.lock_lifecycle(id).await;
        // A newer generation does not block the retirement: it names the
        // acknowledged generation, whose service id is generation-scoped.
        let removed = self
            .inner
            .sessions
            .lock()
            .await
            .get(id)
            .is_none_or(|entry| entry.desired_state == DesiredState::Removed);
        if removed {
            return;
        }
        let Ok(lifecycle) = self.lifecycle() else {
            return;
        };
        match self
            .retire_terminal_generation(
                &id.0,
                ack.generation.as_ref(),
                Some(&lifecycle),
                Some(worker_identity(worker_process)),
            )
            .await
        {
            Retirement::Done | Retirement::Refused => {}
            Retirement::Pending => self.schedule_supervision_retry(id),
        }
    }
}

/// The acknowledged worker's process identity in the supervisor's terms.
fn worker_identity(
    process: pohunek_worker_protocol::ProcessIdentity,
) -> Result<ProcessIdentity, String> {
    let start_identity = process
        .start_identity
        .to_string()
        .parse::<StartIdentity>()
        .map_err(|error| format!("worker start identity is malformed: {error}"))?;
    Ok(ProcessIdentity {
        pid: process.pid,
        start_identity,
    })
}
