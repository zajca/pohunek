//! Per-session activity detector task and activity recording.

use super::{
    broadcast, debug, event, event_payload, is_terminal, log_lag_warn, timestamp_now,
    ActivityTransition, AgentActivity, AgentStateEvent, Detector, DetectorConfig, DetectorInputs,
    Instant, LagWarnThrottle, SessionId, SessionRegistry,
};

fn detection_interval(config: &DetectorConfig) -> tokio::time::Interval {
    let mut tick = tokio::time::interval(config.detection.recheck_after);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick
}

impl SessionRegistry {
    pub(super) fn spawn_detector(&self, id: SessionId, inputs: DetectorInputs) {
        let DetectorInputs {
            output: mut output_rx,
            initial_size: size,
            cancel,
            resize: mut resize_rx,
            config: mut detector_config_rx,
            preview: mut preview_rx,
        } = inputs;
        let registry = self.clone();
        tokio::spawn(async move {
            let detector_config = detector_config_rx.borrow().clone();
            let mut tick = detection_interval(&detector_config);
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
                    changed = detector_config_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let detector_config = detector_config_rx.borrow().clone();
                        tick = detection_interval(&detector_config);
                        tick.tick().await;
                        detector.reconfigure(Instant::now(), detector_config);
                    }
                    request = preview_rx.recv() => {
                        let Some(reply) = request else {
                            break;
                        };
                        let _ = reply.send(detector.region_previews());
                    }
                    received = output_rx.recv() => {
                        match received {
                            Ok(chunk) => {
                                for transition in detector.feed(Instant::now(), &chunk) {
                                    registry.record_activity(&id, transition).await;
                                }
                                if let Some(path) = detector.take_cwd_hint() {
                                    registry.record_cwd_hint(&id, path).await;
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

    /// Returns on-demand previews from the live detector task.
    pub async fn detection(
        &self,
        id: &SessionId,
    ) -> Result<protocol::SessionDetectionResult, protocol::ProtocolError> {
        let preview = {
            let sessions = self.inner.sessions.lock().await;
            sessions
                .get(id)
                .map(|entry| entry.detector_preview.clone())
                .ok_or_else(|| super::session_not_found(&id.0))?
        };
        let (reply, response) = tokio::sync::oneshot::channel();
        preview
            .send(reply)
            .await
            .map_err(|_send_error| protocol::ProtocolError::session_terminal_unavailable())?;
        let previews = response
            .await
            .map_err(|_receive_error| protocol::ProtocolError::session_terminal_unavailable())?;

        Ok(protocol::SessionDetectionResult {
            session_id: id.clone(),
            supported_regions: protocol::DetectionRegionKind::ALL.to_vec(),
            previews,
        })
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

            if entry
                .active_agent
                .as_ref()
                .is_some_and(|report| report.activity_reported)
            {
                return;
            }

            entry.info.activity = Some(transition.activity);
            entry.info.state_source = transition.source;
            entry.info.updated_at = timestamp_now();
            let rescan = (transition.activity == AgentActivity::Working)
                .then(|| std::sync::Arc::clone(&entry.procwatch_rescan));
            (transition, rescan)
        };
        if let Some(rescan) = updated.1 {
            rescan.notify_one();
        }

        let event = crate::events::event(
            event::AGENT_STATE,
            event_payload(AgentStateEvent {
                session_id: id.clone(),
                activity: updated.0.activity,
                source: updated.0.source,
            }),
        );
        let _ = self.inner.events.send(event);
    }
}
