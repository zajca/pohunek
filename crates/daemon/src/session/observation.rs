//! Bounded provider-neutral terminal observation and session waits.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use base64::prelude::{Engine as _, BASE64_STANDARD};
use protocol::{
    ErrorClass, OutputOffset, ProtocolError, RuntimeGeneration, SessionInfo, SessionOutputGap,
    SessionOutputParams, SessionOutputResult, SessionRuntimeIdentity, SessionScreenResult,
    SessionWaitParams, SessionWaitReason, SessionWaitResult, TerminalCursor, TerminalDimensions,
    TerminalWatermark,
};
use tokio::sync::broadcast;

use super::{
    runtime_error, session_not_found, RuntimeHandle, SessionId, SessionRegistry, Worker,
    WorkerError,
};

pub(super) struct ManagedSession {
    pub(super) worker: Worker,
    pub(super) worker_id: String,
    pub(super) runtime_id: String,
    pub(super) runtime_generation: RuntimeGeneration,
}

struct WaitSnapshot {
    session: SessionInfo,
    runtime: Option<SessionRuntimeIdentity>,
    terminal_watermark: Option<TerminalWatermark>,
    output_offset: Option<OutputOffset>,
}

#[derive(Debug)]
struct WaitPermit {
    registry: SessionRegistry,
    session_id: SessionId,
}

impl Drop for WaitPermit {
    fn drop(&mut self) {
        self.registry
            .inner
            .observation_waiters
            .fetch_sub(1, Ordering::AcqRel);
        let mut per_session = self
            .registry
            .inner
            .observation_session_waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = per_session.get_mut(&self.session_id) {
            *count -= 1;
            if *count == 0 {
                per_session.remove(&self.session_id);
            }
        }
    }
}

impl SessionRegistry {
    /// Return a bounded rendered terminal snapshot without taking attach ownership.
    pub async fn screen(&self, id: &SessionId) -> Result<SessionScreenResult, ProtocolError> {
        let started = Instant::now();
        let managed = self.managed_session(id).await?;
        let snapshot = managed
            .worker
            .terminal_snapshot()
            .await
            .map_err(observation_worker_error)?;
        self.verify_managed_identity(id, &managed).await?;

        if snapshot.dimensions.rows() > self.inner.config.observation_screen_rows
            || snapshot.dimensions.columns() > self.inner.config.observation_screen_cols
        {
            return Err(ProtocolError::session_output_limit_exceeded());
        }
        let result = SessionScreenResult {
            session_id: id.clone(),
            worker_id: managed.worker_id,
            runtime: runtime_identity(managed.runtime_id, managed.runtime_generation)?,
            watermark: TerminalWatermark::new(snapshot.watermark),
            dimensions: TerminalDimensions::new(
                snapshot.dimensions.columns(),
                snapshot.dimensions.rows(),
            )
            .map_err(|_error| ProtocolError::session_terminal_unavailable())?,
            cursor: TerminalCursor {
                row: snapshot.cursor.row,
                col: snapshot.cursor.column,
                visible: snapshot.cursor.visible,
            },
            alternate_screen: snapshot.alternate_screen,
            title: snapshot.title,
            progress: snapshot.progress,
            visible_lines: snapshot.visible_lines,
        };
        let serialized = serde_json::to_vec(&result).map_err(|_error| {
            runtime_error(
                "screen_serialize_failed",
                "terminal snapshot serialization failed",
            )
        })?;
        if serialized.len() > self.inner.config.observation_screen_bytes {
            tracing::warn!(
                session_id = %id.0,
                response_bytes = serialized.len(),
                limit_bytes = self.inner.config.observation_screen_bytes,
                "session screen observation exceeded the configured response limit"
            );
            return Err(ProtocolError::session_output_limit_exceeded());
        }
        tracing::debug!(
            session_id = %id.0,
            duration_ms = started.elapsed().as_millis(),
            rows = result.dimensions.rows(),
            columns = result.dimensions.cols(),
            response_bytes = serialized.len(),
            "session screen observation completed"
        );
        Ok(result)
    }

    /// Return one bounded page of retained PTY output.
    pub async fn output(
        &self,
        params: &SessionOutputParams,
    ) -> Result<SessionOutputResult, ProtocolError> {
        let started = Instant::now();
        let requested_bytes = usize::try_from(params.max_bytes()).unwrap_or(usize::MAX);
        if requested_bytes > self.inner.config.observation_output_bytes {
            tracing::warn!(
                session_id = %params.session_id().0,
                requested_bytes,
                limit_bytes = self.inner.config.observation_output_bytes,
                "session output observation exceeded the configured response limit"
            );
            return Err(ProtocolError::session_output_limit_exceeded());
        }
        let wait = Duration::from_millis(u64::from(params.wait_ms().unwrap_or(0)));
        if wait > self.inner.config.observation_output_wait {
            return Err(ProtocolError::session_wait_limit_exceeded());
        }
        let managed = self.managed_session(params.session_id()).await?;
        let current_runtime =
            runtime_identity(managed.runtime_id.clone(), managed.runtime_generation)?;
        if params
            .runtime()
            .is_some_and(|runtime| runtime != &current_runtime)
        {
            return Err(ProtocolError::session_runtime_changed());
        }
        let _permit = if wait.is_zero() {
            None
        } else {
            Some(self.acquire_waiter(params.session_id())?)
        };
        let output = managed
            .worker
            .read_output(
                params.after_offset().map(OutputOffset::get),
                params.max_bytes(),
                wait,
            )
            .await
            .map_err(observation_worker_error)?;
        if output.runtime_id.as_str() != managed.runtime_id {
            return Err(ProtocolError::session_runtime_changed());
        }
        self.verify_managed_identity(params.session_id(), &managed)
            .await?;
        let gap = output
            .gap
            .map(|gap| {
                SessionOutputGap::new(
                    OutputOffset::new(gap.missing_start),
                    OutputOffset::new(gap.missing_end),
                )
            })
            .transpose()
            .map_err(|_error| ProtocolError::session_terminal_unavailable())?;
        let output_bytes = output.data.expose().len();
        let result = SessionOutputResult::new(
            params.session_id().clone(),
            current_runtime,
            OutputOffset::new(output.history_start_offset),
            OutputOffset::new(output.start_offset),
            OutputOffset::new(output.next_offset),
            OutputOffset::new(output.runtime_end_offset),
            BASE64_STANDARD.encode(output.data.expose()),
            gap,
            output.has_more,
            output.timed_out,
        )
        .map_err(|_error| ProtocolError::session_terminal_unavailable())?;
        tracing::debug!(
            session_id = %params.session_id().0,
            duration_ms = started.elapsed().as_millis(),
            output_bytes,
            has_gap = result.gap().is_some(),
            has_more = result.has_more(),
            timed_out = result.timed_out(),
            "session output observation completed"
        );
        Ok(result)
    }

    /// Wait until one requested session predicate becomes true or the deadline elapses.
    pub async fn wait(
        &self,
        params: &SessionWaitParams,
    ) -> Result<SessionWaitResult, ProtocolError> {
        let started = Instant::now();
        let timeout = Duration::from_millis(u64::from(params.timeout_ms()));
        if timeout > self.inner.config.session_wait {
            return Err(ProtocolError::session_wait_limit_exceeded());
        }

        // Snapshot/register/recheck closes the lost-wakeup interval without
        // retaining the registry lock while the request sleeps.
        let first = self.wait_snapshot(params).await?;
        if let Some(result) = evaluate_wait(params, first) {
            log_wait_completed(params, &result, started);
            return Ok(result);
        }
        let mut events = self.subscribe();
        let second = self.wait_snapshot(params).await?;
        if let Some(result) = evaluate_wait(params, second) {
            log_wait_completed(params, &result, started);
            return Ok(result);
        }
        let _permit = self.acquire_waiter(params.session_id())?;
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                let result = self.wait_timeout_result(params).await?;
                log_wait_completed(params, &result, started);
                return Ok(result);
            }
            let remaining = deadline - now;
            let output_cursor = params.after_output_offset().map(OutputOffset::get);
            let output_wait = self.wait_for_output(params.session_id(), output_cursor, remaining);
            tokio::select! {
                biased;
                () = self.inner.event_log_shutdown.cancelled() => {
                    tracing::warn!(
                        session_id = %params.session_id().0,
                        duration_ms = started.elapsed().as_millis(),
                        "daemon shutdown cancelled a session waiter"
                    );
                    return Err(ProtocolError::new(
                        ErrorClass::Daemon,
                        "daemon_shutting_down",
                        "daemon shutdown cancelled the bounded session wait",
                        None,
                    ));
                }
                () = tokio::time::sleep_until(deadline) => {
                    let result = self.wait_timeout_result(params).await?;
                    log_wait_completed(params, &result, started);
                    return Ok(result);
                }
                received = events.recv() => {
                    match received {
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::warn!(
                                session_id = %params.session_id().0,
                                duration_ms = started.elapsed().as_millis(),
                                "event channel closure cancelled a session waiter"
                            );
                            return Err(ProtocolError::new(
                                ErrorClass::Daemon,
                                "daemon_shutting_down",
                                "daemon event channel closed during bounded session wait",
                                None,
                            ));
                        }
                    }
                }
                output = output_wait, if params.after_output_offset().is_some()
                    || params.after_terminal_watermark().is_some() => {
                    if let Err(error) = output {
                        let snapshot = self.wait_snapshot(params).await?;
                        if let Some(result) = evaluate_wait(params, snapshot) {
                            log_wait_completed(params, &result, started);
                            return Ok(result);
                        }
                        return Err(error);
                    }
                }
            }
            let snapshot = self.wait_snapshot(params).await?;
            if let Some(result) = evaluate_wait(params, snapshot) {
                log_wait_completed(params, &result, started);
                return Ok(result);
            }
        }
    }

    pub(super) async fn managed_session(
        &self,
        id: &SessionId,
    ) -> Result<ManagedSession, ProtocolError> {
        let sessions = self.inner.sessions.lock().await;
        let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
        if entry.info.external == Some(true) {
            return Err(ProtocolError::session_has_no_managed_terminal());
        }
        let RuntimeHandle::Worker(worker) = &entry.runtime else {
            return Err(ProtocolError::session_terminal_unavailable());
        };
        let runtime = entry
            .info
            .runtime
            .as_ref()
            .filter(|runtime| runtime.state == protocol::RuntimeState::Live)
            .ok_or_else(ProtocolError::session_terminal_unavailable)?;
        let worker_id = runtime
            .worker_id
            .clone()
            .ok_or_else(ProtocolError::session_terminal_unavailable)?;
        let runtime_id = runtime
            .runtime_id
            .clone()
            .ok_or_else(ProtocolError::session_terminal_unavailable)?;
        let runtime_generation = runtime.runtime_generation;
        Ok(ManagedSession {
            worker: worker.clone(),
            worker_id,
            runtime_id,
            runtime_generation,
        })
    }

    pub(super) async fn verify_managed_identity(
        &self,
        id: &SessionId,
        observed: &ManagedSession,
    ) -> Result<(), ProtocolError> {
        let current = self.managed_session(id).await?;
        if current.worker_id != observed.worker_id
            || current.runtime_id != observed.runtime_id
            || current.runtime_generation != observed.runtime_generation
        {
            return Err(ProtocolError::session_runtime_changed());
        }
        Ok(())
    }

    async fn wait_snapshot(
        &self,
        params: &SessionWaitParams,
    ) -> Result<WaitSnapshot, ProtocolError> {
        let info = self.inspect(params.session_id()).await?;
        let needs_terminal = params.after_terminal_watermark().is_some();
        let needs_output = params.after_output_offset().is_some() || needs_terminal;
        let runtime = info.runtime.as_ref().and_then(|runtime| {
            (runtime.state == protocol::RuntimeState::Live)
                .then(|| {
                    runtime_identity(runtime.runtime_id.clone()?, runtime.runtime_generation).ok()
                })
                .flatten()
        });
        if params
            .runtime()
            .is_some_and(|expected| runtime.as_ref().is_none_or(|current| current != expected))
        {
            return Ok(WaitSnapshot {
                session: info,
                runtime,
                terminal_watermark: None,
                output_offset: None,
            });
        }
        if !needs_output {
            return Ok(WaitSnapshot {
                session: info,
                runtime,
                terminal_watermark: None,
                output_offset: None,
            });
        }
        let managed = self.managed_session(params.session_id()).await?;
        let inspect = managed
            .worker
            .inspect()
            .await
            .map_err(observation_worker_error)?;
        let terminal_watermark = if needs_terminal {
            Some(TerminalWatermark::new(
                managed
                    .worker
                    .terminal_snapshot()
                    .await
                    .map_err(observation_worker_error)?
                    .watermark,
            ))
        } else {
            None
        };
        self.verify_managed_identity(params.session_id(), &managed)
            .await?;
        Ok(WaitSnapshot {
            session: info,
            runtime,
            terminal_watermark,
            output_offset: needs_output.then(|| OutputOffset::new(inspect.next_offset)),
        })
    }

    async fn wait_for_output(
        &self,
        id: &SessionId,
        cursor: Option<u64>,
        wait: Duration,
    ) -> Result<(), ProtocolError> {
        let managed = self.managed_session(id).await?;
        let inspect = managed
            .worker
            .inspect()
            .await
            .map_err(observation_worker_error)?;
        let after = cursor.unwrap_or(inspect.next_offset);
        managed
            .worker
            .read_output(
                after.into(),
                1,
                wait.min(self.inner.config.observation_output_wait),
            )
            .await
            .map(|_output| ())
            .map_err(observation_worker_error)
    }

    async fn wait_timeout_result(
        &self,
        params: &SessionWaitParams,
    ) -> Result<SessionWaitResult, ProtocolError> {
        let snapshot = self.wait_snapshot(params).await?;
        Ok(wait_result(SessionWaitReason::Timeout, snapshot))
    }

    fn acquire_waiter(&self, id: &SessionId) -> Result<WaitPermit, ProtocolError> {
        let previous = self
            .inner
            .observation_waiters
            .fetch_add(1, Ordering::AcqRel);
        if previous >= self.inner.config.observation_global_waiters {
            self.inner
                .observation_waiters
                .fetch_sub(1, Ordering::AcqRel);
            tracing::warn!(
                session_id = %id.0,
                concurrent_waiters = previous,
                waiter_limit = self.inner.config.observation_global_waiters,
                limit_scope = "global",
                "session waiter limit reached"
            );
            return Err(ProtocolError::session_waiter_limit_reached());
        }
        let mut per_session = self
            .inner
            .observation_session_waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = per_session.entry(id.clone()).or_default();
        if *count >= self.inner.config.observation_session_waiters {
            self.inner
                .observation_waiters
                .fetch_sub(1, Ordering::AcqRel);
            tracing::warn!(
                session_id = %id.0,
                concurrent_waiters = *count,
                waiter_limit = self.inner.config.observation_session_waiters,
                limit_scope = "session",
                "session waiter limit reached"
            );
            return Err(ProtocolError::session_waiter_limit_reached());
        }
        *count += 1;
        tracing::debug!(
            session_id = %id.0,
            concurrent_waiters = previous + 1,
            session_waiters = *count,
            "session waiter acquired"
        );
        Ok(WaitPermit {
            registry: self.clone(),
            session_id: id.clone(),
        })
    }
}

fn log_wait_completed(params: &SessionWaitParams, result: &SessionWaitResult, started: Instant) {
    tracing::debug!(
        session_id = %params.session_id().0,
        duration_ms = started.elapsed().as_millis(),
        reason = ?result.reason,
        "session wait completed"
    );
}

pub(crate) fn runtime_identity(
    runtime_id: String,
    runtime_generation: RuntimeGeneration,
) -> Result<SessionRuntimeIdentity, ProtocolError> {
    SessionRuntimeIdentity::new(runtime_id, runtime_generation)
        .map_err(|_error| ProtocolError::session_terminal_unavailable())
}

fn evaluate_wait(params: &SessionWaitParams, snapshot: WaitSnapshot) -> Option<SessionWaitResult> {
    let reason = if params
        .states()
        .is_some_and(|states| states.contains(&snapshot.session.state))
    {
        Some(SessionWaitReason::StateMatched)
    } else if params.activities().is_some_and(|activities| {
        snapshot
            .session
            .activity
            .is_some_and(|activity| activities.contains(&activity))
    }) {
        Some(SessionWaitReason::ActivityMatched)
    } else if params
        .after_updated_at()
        .is_some_and(|updated| snapshot.session.updated_at.as_str() > updated)
    {
        Some(SessionWaitReason::SessionUpdated)
    } else if params.runtime().is_some_and(|expected| {
        snapshot
            .runtime
            .as_ref()
            .is_none_or(|current| current != expected)
    }) {
        Some(SessionWaitReason::RuntimeChanged)
    } else if params.after_terminal_watermark().is_some_and(|after| {
        snapshot
            .terminal_watermark
            .is_some_and(|current| current > after)
    }) {
        Some(SessionWaitReason::TerminalChanged)
    } else if params.after_output_offset().is_some_and(|after| {
        snapshot
            .output_offset
            .is_some_and(|current| current > after)
    }) {
        Some(SessionWaitReason::OutputAdvanced)
    } else {
        None
    }?;
    Some(wait_result(reason, snapshot))
}

fn wait_result(reason: SessionWaitReason, snapshot: WaitSnapshot) -> SessionWaitResult {
    SessionWaitResult {
        reason,
        session: snapshot.session,
        terminal_watermark: snapshot.terminal_watermark,
        output_offset: snapshot.output_offset,
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err transfers ownership of the typed worker error into this protocol boundary"
)]
pub(crate) fn observation_worker_error(error: WorkerError) -> ProtocolError {
    match error {
        WorkerError::ObservationUnsupported { .. }
        | WorkerError::Rejected {
            code: pohunek_worker_protocol::ControlCode::WorkerFeatureUnavailable,
            ..
        } => ProtocolError::worker_feature_unavailable(),
        WorkerError::Rejected {
            code: pohunek_worker_protocol::ControlCode::ObservationLimitExceeded,
            ..
        } => ProtocolError::session_output_limit_exceeded(),
        WorkerError::ResponseMismatch => ProtocolError::session_runtime_changed(),
        WorkerError::Socket { .. }
        | WorkerError::Protocol(_)
        | WorkerError::Rejected { .. }
        | WorkerError::NotInitialized
        | WorkerError::AttachSnapshotUnsupported { .. }
        | WorkerError::AttachReadyTimeout { .. } => ProtocolError::session_terminal_unavailable(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionRegistryConfig;

    #[test]
    fn waiter_caps_are_global_per_session_and_released_by_drop() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            observation_global_waiters: 2,
            observation_session_waiters: 1,
            ..SessionRegistryConfig::default()
        });
        let first_id = SessionId("s-first".to_owned());
        let second_id = SessionId("s-second".to_owned());

        let first = registry.acquire_waiter(&first_id).expect("first permit");
        let same_session = registry
            .acquire_waiter(&first_id)
            .expect_err("per-session cap must reject a second waiter");
        assert_eq!(same_session.code, "session_waiter_limit_reached");

        let second = registry.acquire_waiter(&second_id).expect("second permit");
        let global = registry
            .acquire_waiter(&SessionId("s-third".to_owned()))
            .expect_err("global cap must reject a third waiter");
        assert_eq!(global.code, "session_waiter_limit_reached");

        drop(first);
        let replacement = registry
            .acquire_waiter(&first_id)
            .expect("dropping a permit releases both counters");
        drop(replacement);
        drop(second);
    }
}

// Rust guideline compliant 2026-08-04
