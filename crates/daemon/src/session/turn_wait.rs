//! The bounded `session.wait_turn` long wait.
//!
//! A wait is edge-triggered: it returns the oldest selected turn event newer
//! than its cursor (or newer than the call when it has none), never the
//! current state. The journal in [`super::turn_events`] is read by sequence
//! after registering for change notifications, so no event recorded during
//! the wait can be missed.

// Rust guideline compliant 2026-10-12

use std::time::{Duration, Instant};

use protocol::{
    ErrorClass, ProtocolError, SessionId, SessionTurnEvent, SessionTurnProgress,
    SessionWaitTurnParams, SessionWaitTurnReason, SessionWaitTurnResult,
};
use tracing::{debug, warn};

use super::turn_events::{rfc3339_from_nanos, unix_nanos_now, TurnEntry};
use super::SessionRegistry;

/// How one check of a waiting request came out.
enum Outcome {
    /// A selected event newer than the position was recorded.
    Event(TurnEntry),
    /// Every target is terminal and no selected event is newer; `latest` is
    /// the journal sequence the check covered.
    Ended { latest: u64 },
    /// Nothing completes the wait yet; `latest` is the covered sequence.
    Pending { latest: u64 },
    /// Events after the wait's position were evicted while it waited; `latest`
    /// is the newest sequence, where the fresh cursor starts.
    Reset { latest: u64 },
}

impl SessionRegistry {
    /// Waits for the next selected turn event of the targeted sessions.
    ///
    /// # Errors
    ///
    /// Returns `session_wait_limit_exceeded` when `timeout_ms` exceeds the
    /// configured ceiling, `session_not_found` when a target is unknown or is
    /// removed during the wait, `session_waiter_limit_reached` when the turn
    /// waiter pool is full, and `daemon_shutting_down` when daemon shutdown
    /// starts during the wait.
    pub async fn wait_turn(
        &self,
        params: &SessionWaitTurnParams,
    ) -> Result<SessionWaitTurnResult, ProtocolError> {
        let started = Instant::now();
        let call_nanos = unix_nanos_now();
        let config = &self.inner.config;
        let timeout = params
            .timeout_ms()
            .map_or(config.turn_wait_default, |millis| {
                Duration::from_millis(u64::from(millis))
            });
        if timeout > config.turn_wait_max {
            return Err(ProtocolError::session_wait_limit_exceeded());
        }
        let targets = params.session_ids();
        for id in targets {
            self.inspect(id).await?;
        }

        let turns = &self.inner.turns;
        // Registering before the first scan closes the lost-wakeup interval:
        // an event appended after this point bumps the version this receiver
        // has not seen yet.
        let mut journal_version = turns.subscribe();
        let (after_seq, since_nanos) = match params.after() {
            None => (turns.latest(), call_nanos),
            Some(cursor) => {
                if let Some(position) = turns.place(cursor) {
                    (position.seq, position.issued_nanos)
                } else {
                    let latest = turns.latest();
                    let result = self
                        .turn_result(
                            SessionWaitTurnReason::CursorReset,
                            None,
                            latest,
                            targets,
                            call_nanos,
                        )
                        .await?;
                    log_turn_wait_completed(targets, &result, started);
                    return Ok(result);
                }
            }
        };

        if let Some(result) = self
            .turn_outcome_result(params, after_seq, since_nanos, false)
            .await?
        {
            log_turn_wait_completed(targets, &result, started);
            return Ok(result);
        }
        let _permit = turns
            .acquire(
                targets,
                config.turn_global_waiters,
                config.turn_session_waiters,
            )
            .ok_or_else(ProtocolError::session_waiter_limit_reached)?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            tokio::select! {
                biased;
                () = self.inner.daemon_shutdown.cancelled() => {
                    warn!(
                        targets = targets.len(),
                        duration_ms = started.elapsed().as_millis(),
                        "daemon shutdown cancelled a session turn waiter"
                    );
                    return Err(turn_wait_shutdown_error());
                }
                () = tokio::time::sleep_until(deadline) => {
                    let result = self
                        .turn_outcome_result(params, after_seq, since_nanos, true)
                        .await?
                        .expect("a deadline check always produces a result");
                    log_turn_wait_completed(targets, &result, started);
                    return Ok(result);
                }
                changed = journal_version.changed() => {
                    if changed.is_err() {
                        return Err(turn_wait_shutdown_error());
                    }
                    journal_version.borrow_and_update();
                }
            }
            if let Some(result) = self
                .turn_outcome_result(params, after_seq, since_nanos, false)
                .await?
            {
                log_turn_wait_completed(targets, &result, started);
                return Ok(result);
            }
        }
    }

    /// Checks the wait once and builds its result when it completes.
    ///
    /// With `at_deadline`, a still-pending wait completes as a timeout.
    async fn turn_outcome_result(
        &self,
        params: &SessionWaitTurnParams,
        after_seq: u64,
        since_nanos: u64,
        at_deadline: bool,
    ) -> Result<Option<SessionWaitTurnResult>, ProtocolError> {
        let targets = params.session_ids();
        let (reason, event, cursor_seq) = match self.turn_outcome(params, after_seq).await? {
            Outcome::Event(entry) => {
                let seq = entry.seq;
                (SessionWaitTurnReason::Event, Some(entry), seq)
            }
            Outcome::Ended { latest } => (SessionWaitTurnReason::TargetsEnded, None, latest),
            Outcome::Reset { latest } => (SessionWaitTurnReason::CursorReset, None, latest),
            Outcome::Pending { latest } if at_deadline => {
                (SessionWaitTurnReason::Timeout, None, latest)
            }
            Outcome::Pending { .. } => return Ok(None),
        };
        self.turn_result(reason, event, cursor_seq, targets, since_nanos)
            .await
            .map(Some)
    }

    /// Reads target states, then the journal, for one check of a wait.
    ///
    /// States come first: the journal records a target's `exited` under the
    /// same sessions lock that makes its terminal state visible, so a scan
    /// after a terminal state was read always finds that exit. A position
    /// whose following events were evicted while the wait was parked resets,
    /// because those events cannot be reported.
    async fn turn_outcome(
        &self,
        params: &SessionWaitTurnParams,
        after_seq: u64,
    ) -> Result<Outcome, ProtocolError> {
        let mut all_terminal = true;
        for id in params.session_ids() {
            all_terminal &= self.inspect(id).await?.state.is_terminal();
        }
        let scan = self
            .inner
            .turns
            .scan(after_seq, params.session_ids(), params.until());
        if scan.evicted {
            return Ok(Outcome::Reset {
                latest: scan.latest,
            });
        }
        Ok(match scan.event {
            Some(entry) => Outcome::Event(entry),
            None if all_terminal => Outcome::Ended {
                latest: scan.latest,
            },
            None => Outcome::Pending {
                latest: scan.latest,
            },
        })
    }

    /// Builds a result with a fresh cursor at `cursor_seq` and current
    /// progress of every target.
    async fn turn_result(
        &self,
        reason: SessionWaitTurnReason,
        event: Option<TurnEntry>,
        cursor_seq: u64,
        targets: &[SessionId],
        since_nanos: u64,
    ) -> Result<SessionWaitTurnResult, ProtocolError> {
        let mut progress = Vec::with_capacity(targets.len());
        for id in targets {
            progress.push(self.turn_progress(id, since_nanos).await?);
        }
        Ok(SessionWaitTurnResult {
            reason,
            event: event.map(|entry| SessionTurnEvent {
                session_id: entry.session_id,
                kind: entry.kind,
                at: entry.at,
                notification_id: entry.notification_id,
            }),
            cursor: self.inner.turns.cursor(cursor_seq),
            progress,
        })
    }

    async fn turn_progress(
        &self,
        id: &SessionId,
        since_nanos: u64,
    ) -> Result<SessionTurnProgress, ProtocolError> {
        let info = self.inspect(id).await?;
        let progress = self.inner.turns.existing_progress(id);
        let last_output = progress
            .as_ref()
            .and_then(|progress| progress.last_output_nanos());
        let turn_started = progress
            .as_ref()
            .and_then(|progress| progress.turn_started_nanos());
        Ok(SessionTurnProgress {
            session_id: id.clone(),
            state: info.state,
            activity: info.activity,
            last_output_at: last_output.and_then(rfc3339_from_nanos),
            output_advanced: last_output.is_some_and(|nanos| nanos > since_nanos),
            turn_started_at: turn_started.and_then(rfc3339_from_nanos),
        })
    }
}

fn turn_wait_shutdown_error() -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Daemon,
        "daemon_shutting_down",
        "daemon shutdown cancelled the bounded session turn wait",
        None,
    )
}

fn log_turn_wait_completed(
    targets: &[SessionId],
    result: &SessionWaitTurnResult,
    started: Instant,
) {
    debug!(
        targets = targets.len(),
        duration_ms = started.elapsed().as_millis(),
        reason = ?result.reason,
        event.session_id = result.event.as_ref().map(|event| event.session_id.0.as_str()),
        event.kind = result.event.as_ref().map(|event| event.kind.as_str()),
        "session turn wait completed"
    );
}
