//! Per-session activity detector task and activity recording.

use super::{
    debug, event, is_terminal, json, log_lag_warn, timestamp_now, ActivityTransition, AgentKind,
    CancellationToken, Detector, DetectorConfig, Event, Instant, LagWarnThrottle, Manifest,
    SessionId, SessionRegistry, broadcast, watch,
};

impl SessionRegistry {
    // The detector spawn carries the session id, base kind, manifest override, the
    // output stream, the initial size, and its cancel/resize channels — all distinct
    // runtime inputs with no natural grouping struct.
    #[expect(
        clippy::too_many_arguments,
        reason = "distinct detector runtime inputs with no natural grouping struct"
    )]
    pub(super) fn spawn_detector(
        &self,
        id: SessionId,
        agent: AgentKind,
        manifest_override: Option<Manifest>,
        mut output_rx: broadcast::Receiver<Vec<u8>>,
        size: (u16, u16),
        cancel: CancellationToken,
        mut resize_rx: watch::Receiver<(u16, u16)>,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            // A host profile may override the base kind's detection manifest; absent
            // an override this is exactly `for_agent(agent)` (C.3).
            let detector_config = DetectorConfig::for_profile(agent, manifest_override);
            let mut tick = tokio::time::interval(detector_config.detection.recheck_after);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            let (rows, cols) = size;
            let mut detector = Detector::new(rows, cols, Instant::now(), detector_config);
            let mut lag_warn =
                LagWarnThrottle::new(registry.inner.config.detector_lag_warn_interval);

            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        for transition in detector.tick(Instant::now()) {
                            registry.record_activity(&id, transition).await;
                        }
                        // Flush a folded lag batch whose window has elapsed, so a
                        // session that stopped lagging still reports its summary.
                        if let Some(warn_kind) = lag_warn.poll(Instant::now()) {
                            log_lag_warn(&id, warn_kind);
                        }
                    }
                    changed = resize_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let (rows, cols) = *resize_rx.borrow();
                        detector.resize(rows, cols);
                    }
                    received = output_rx.recv() => {
                        match received {
                            Ok(chunk) => {
                                for transition in detector.feed(Instant::now(), &chunk) {
                                    registry.record_activity(&id, transition).await;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                // Always resync; only the logging is rate-limited
                                // so a runaway session cannot flood the log.
                                if let Some(warn_kind) = lag_warn.observe(Instant::now(), skipped) {
                                    log_lag_warn(&id, warn_kind);
                                }
                                detector.resync_after_lag();
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }

            // The loop exited (cancel / resize-closed / output-closed): flush any
            // lags folded into the final, not-yet-elapsed window so a session torn
            // down mid-storm still reports its trailing batch instead of dropping it.
            if let Some(warn_kind) = lag_warn.flush() {
                log_lag_warn(&id, warn_kind);
            }
        });
    }

    pub(super) async fn record_activity(&self, id: &SessionId, transition: ActivityTransition) {
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, "detector activity arrived for unknown session");
                return;
            };

            if entry.stopping || is_terminal(entry.info.state) {
                return;
            }

            entry.info.activity = Some(transition.activity);
            entry.info.state_source = transition.source;
            entry.info.updated_at = timestamp_now();
            transition
        };

        let event = Event::new(
            event::AGENT_STATE,
            json!({
                "session_id": id,
                "activity": updated.activity,
                "source": updated.source,
            }),
        );
        let _ = self.inner.events.send(event);
    }
}
