//! Raw attach stream tokens, redemption, and lifecycle.

use super::{
    debug, event, event_payload, session_not_found, session_not_running, AtomicU64, AttachEvent,
    CancellationToken, ErrorClass, Event, Ordering, ProtocolError, RuntimeHandle,
    SessionAttachParams, SessionId, SessionRegistry, SessionState, SystemTime, UNIX_EPOCH,
};
use crate::runtime::DataStream;
use pohunek_worker_protocol::{AttachStart, Dimensions as WorkerDimensions, StreamId};

#[derive(Debug, Clone)]
pub(super) struct PendingAttach {
    pub(super) session_id: SessionId,
    pub(super) initial_dimensions: Option<protocol::TerminalDimensions>,
    pub(super) expires_at: tokio::time::Instant,
}

#[derive(Debug, Clone)]
pub(super) struct ActiveAttach {
    pub(super) session_id: SessionId,
    pub(super) cancel: CancellationToken,
}

#[derive(Debug, Clone)]
pub(super) struct RecentAttachFailure {
    pub(super) stream_id: String,
    pub(super) error: ProtocolError,
    pub(super) finished_at: tokio::time::Instant,
}

/// Redeemed raw attach stream state for the API bridge.
#[derive(Debug)]
pub struct RedeemedAttach {
    /// One-shot stream id that was redeemed.
    pub stream_id: String,
    /// Session being attached.
    pub session_id: SessionId,
    /// Runtime stream backing the public bridge.
    pub runtime: RedeemedRuntime,
    /// Cancellation signal fired by `session.detach` or session exit.
    pub cancel: CancellationToken,
}

/// Runtime-specific stream already authorized for one public attach.
#[derive(Debug)]
pub enum RedeemedRuntime {
    /// Framed stream connected directly to the durable worker.
    Worker(DataStream),
}

impl SessionRegistry {
    /// Mint a one-shot raw attach stream token for a running session.
    ///
    /// Rejects a *self-feeding* attach: when the client reports (via the
    /// `POHUNEK_SESSION_ID` / `POHUNEK_WORKER_ID` it inherited from a PTY) that it
    /// is running inside this very session worker, its stdout is the
    /// session's own PTY slave, so streaming the PTY's output to it would be
    /// written straight back into the PTY as input and re-read as output — an
    /// infinite, log-flooding loop. Both the session id **and** the daemon
    /// worker id must match. The daemon instance id is not an ownership
    /// identity and therefore cannot weaken self-feedback protection after a
    /// restart. The existence/running check runs first so a stale origin
    /// pointing at a gone/stopped session yields the truthful `session_not_found`/
    /// `session_not_running` rather than a misleading self-feedback error.
    pub async fn attach(
        &self,
        params: &SessionAttachParams,
    ) -> Result<protocol::SessionAttachResult, ProtocolError> {
        let id = &params.session_id;
        self.prune_expired_pending_attaches().await;
        self.ensure_not_external(id).await?;
        let target_worker_id = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(id));
            }
            entry
                .info
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.worker_id.clone())
        };

        // The session exists and is running; only now is a self-feed possible.
        let same_origin_session = params.origin_session_id.as_ref() == Some(id);
        let same_worker = target_worker_id
            .as_deref()
            .is_some_and(|worker| params.origin_worker_id.as_deref() == Some(worker));
        if same_origin_session && same_worker {
            return Err(attach_self_feedback(id));
        }
        let stream_id = format!(
            "a-{}",
            self.inner.next_stream_id.fetch_add(1, Ordering::Relaxed)
        );
        let pending = PendingAttach {
            session_id: id.clone(),
            initial_dimensions: params.initial_dimensions,
            expires_at: tokio::time::Instant::now() + self.inner.config.attach_token_ttl,
        };
        let mut pending_attaches = self.inner.pending_attaches.lock().await;
        pending_attaches.insert(stream_id.clone(), pending);
        Ok(protocol::SessionAttachResult { stream_id })
    }

    /// Redeem a one-shot attach token and register a live attach stream.
    pub async fn redeem_attach(&self, stream_id: &str) -> Result<RedeemedAttach, ProtocolError> {
        self.prune_expired_pending_attaches().await;
        let pending = {
            let mut pending_attaches = self.inner.pending_attaches.lock().await;
            pending_attaches.remove(stream_id)
        }
        .ok_or_else(|| attach_token_error("attach_not_found", stream_id))?;

        if tokio::time::Instant::now() > pending.expires_at {
            return Err(attach_token_error("attach_expired", stream_id));
        }

        let runtime = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get(&pending.session_id)
                .ok_or_else(|| session_not_found(&pending.session_id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(&pending.session_id));
            }
            entry.runtime.clone()
        };
        let runtime = match runtime {
            RuntimeHandle::Worker(worker) => {
                let private_stream_id = StreamId::new(stream_id).map_err(|error| {
                    super::runtime_error("worker_attach_invalid", error.to_string())
                })?;
                let dimensions = pending
                    .initial_dimensions
                    .map(|dimensions| WorkerDimensions::new(dimensions.cols(), dimensions.rows()))
                    .transpose()
                    .map_err(|error| {
                        super::runtime_error("worker_attach_invalid", error.to_string())
                    })?;
                let mut data = worker
                    .open_attach(private_stream_id, AttachStart { dimensions })
                    .await
                    .map_err(super::worker_error_to_protocol)?;
                if let Some(update) = data.dimension_update.take() {
                    self.record_dimensions(&pending.session_id, &update).await?;
                }
                RedeemedRuntime::Worker(data)
            }
            RuntimeHandle::Unavailable(state) => {
                return Err(super::unavailable_runtime_error(&pending.session_id, state));
            }
        };

        let cancel = CancellationToken::new();
        {
            let mut active_attaches = self.inner.active_attaches.lock().await;
            active_attaches.insert(
                stream_id.to_owned(),
                ActiveAttach {
                    session_id: pending.session_id.clone(),
                    cancel: cancel.clone(),
                },
            )
        };

        if let Err(err) = self.ensure_session_running(&pending.session_id).await {
            let mut active_attaches = self.inner.active_attaches.lock().await;
            active_attaches.remove(stream_id);
            return Err(err);
        }

        self.emit_attach(event::ATTACH_OPENED, &pending.session_id, stream_id);
        Ok(RedeemedAttach {
            stream_id: stream_id.to_owned(),
            session_id: pending.session_id,
            runtime,
            cancel,
        })
    }

    /// Cancel an active raw attach stream. Unknown streams are a no-op.
    pub async fn detach(&self, stream_id: &str) -> protocol::SessionDetachResult {
        let cancel = {
            let active_attaches = self.inner.active_attaches.lock().await;
            active_attaches.get(stream_id).and_then(|active| {
                if active.cancel.is_cancelled() {
                    None
                } else {
                    Some(active.cancel.clone())
                }
            })
        };
        let detached = if let Some(cancel) = cancel {
            cancel.cancel();
            true
        } else {
            false
        };
        let error = if detached {
            None
        } else {
            self.take_attach_failure(stream_id).await
        };
        protocol::SessionDetachResult { detached, error }
    }

    /// Deregister a raw attach stream after its bridge exits.
    pub async fn finish_attach(&self, stream_id: &str, error: Option<ProtocolError>) {
        let active = {
            let mut active_attaches = self.inner.active_attaches.lock().await;
            active_attaches.remove(stream_id)
        };

        if let Some(active) = active {
            self.emit_attach(event::ATTACH_CLOSED, &active.session_id, stream_id);
        }
        if let Some(error) = error {
            self.store_attach_failure(stream_id, error).await;
        }
    }

    pub(super) async fn cancel_session_attaches(&self, id: &SessionId) {
        let active_attaches = self.inner.active_attaches.lock().await;
        for (stream_id, active) in active_attaches.iter() {
            if active.session_id == *id {
                debug!(session_id = %id.0, stream_id, "cancelling active attach");
                active.cancel.cancel();
            }
        }
    }

    async fn prune_expired_pending_attaches(&self) {
        let now = tokio::time::Instant::now();
        let mut pending_attaches = self.inner.pending_attaches.lock().await;
        pending_attaches.retain(|_, pending| pending.expires_at > now);
    }

    async fn store_attach_failure(&self, stream_id: &str, error: ProtocolError) {
        let capacity = self.inner.config.attach_result_capacity;
        if capacity == 0 {
            return;
        }
        let now = tokio::time::Instant::now();
        let mut failures = self.inner.recent_attach_failures.lock().await;
        prune_attach_failures(&mut failures, now, self.inner.config.attach_result_ttl);
        while failures.len() >= capacity {
            failures.pop_front();
        }
        failures.push_back(RecentAttachFailure {
            stream_id: stream_id.to_owned(),
            error,
            finished_at: now,
        });
    }

    async fn take_attach_failure(&self, stream_id: &str) -> Option<ProtocolError> {
        let now = tokio::time::Instant::now();
        let mut failures = self.inner.recent_attach_failures.lock().await;
        prune_attach_failures(&mut failures, now, self.inner.config.attach_result_ttl);
        let position = failures
            .iter()
            .position(|failure| failure.stream_id == stream_id)?;
        failures.remove(position).map(|failure| failure.error)
    }

    pub(super) async fn remove_pending_attaches_for_session(&self, id: &SessionId) {
        let mut pending_attaches = self.inner.pending_attaches.lock().await;
        pending_attaches.retain(|_, pending| pending.session_id != *id);
    }

    fn emit_attach(&self, name: &str, session_id: &SessionId, stream_id: &str) {
        let event = Event::new(
            name,
            event_payload(AttachEvent {
                session_id: session_id.clone(),
                stream_id: stream_id.to_owned(),
            }),
        );
        let _ = self.inner.events.send(event);
    }
}

fn prune_attach_failures(
    failures: &mut std::collections::VecDeque<RecentAttachFailure>,
    now: tokio::time::Instant,
    ttl: std::time::Duration,
) {
    failures.retain(|failure| now.saturating_duration_since(failure.finished_at) < ttl);
}

/// Raised when the attaching client reports (via `POHUNEK_SESSION_ID` +
/// `POHUNEK_DAEMON_ID`) that it is running inside the very session of this very
/// daemon instance it is attaching to. Stable code: `attach_self_feedback`.
fn attach_self_feedback(id: &SessionId) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Daemon,
        "attach_self_feedback",
        format!(
            "refusing to attach to session {} from inside its own terminal: \
             that would loop the session's output back into its own input",
            id.0
        ),
        Some(
            "run the attach from a different terminal (one not already inside this session)"
                .to_owned(),
        ),
    )
}

fn attach_token_error(code: &'static str, stream_id: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        code,
        format!("attach stream is not available: {stream_id}"),
        None,
    )
}

/// Generate an opaque id distinguishing this daemon process instance.
///
/// Combines the pid, the wall clock's distance from the epoch, and a process-wide
/// monotonic counter. Two live instances always differ (distinct live pids); a
/// restart on a recycled pid differs as long as the wall clock advances between
/// the two starts; the counter disambiguates registries built within one process
/// at the same instant (e.g. tests). The clock distance is taken in *either*
/// direction so the id never collapses to a fixed value when the clock is set
/// before 1970 (an RTC-less boot before NTP). Used to scope the
/// self-feeding-attach guard to this instance's own PTYs and to keep a stale
/// `POHUNEK_DAEMON_ID` from a previous daemon from matching (see
/// [`SessionAttachParams::origin_worker_id`]); a residual collision only
/// false-rejects an attach (never lets a loop through) and the lag-warn throttle
/// still bounds any such loop's log output. Not a secret.
pub(super) fn generate_daemon_instance_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_nanos(),
        // Clock is before the epoch: use how far before, so the value still varies
        // with the clock instead of pinning to a fixed 0.
        Err(before_epoch) => before_epoch.duration().as_nanos(),
    };
    format!("d-{}-{nanos}-{seq}", std::process::id())
}
