//! Per-session activity detector task and activity recording.

use super::{
    broadcast, debug, event, event_payload, is_terminal, log_lag_warn, record_activity_evidence,
    timestamp_now, ActivityTransition, AgentActivity, DetectionPreviewRequest, Detector,
    DetectorConfig, DetectorConfigUpdate, DetectorInputs, DetectorScope, Instant, LagWarnThrottle,
    RuntimeWatchIdentity, SessionId, SessionRegistry,
};

fn detection_interval(config: &DetectorConfig) -> tokio::time::Interval {
    let mut tick = tokio::time::interval(config.detection.recheck_after);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick
}

fn apply_detector_config(
    detector: &mut Detector,
    tick: &mut tokio::time::Interval,
    applied_generation: &mut u64,
    update: DetectorConfigUpdate,
) {
    *tick = detection_interval(&update.config);
    tick.reset();
    detector.reconfigure(Instant::now(), update.config);
    *applied_generation = update.generation;
}

fn reply_to_preview(
    detector: &mut Detector,
    tick: &mut tokio::time::Interval,
    config_rx: &mut tokio::sync::watch::Receiver<DetectorConfigUpdate>,
    applied_generation: &mut u64,
    request: DetectionPreviewRequest,
) {
    if *applied_generation < request.minimum_config_generation
        || config_rx.has_changed().is_ok_and(|changed| changed)
    {
        let update = config_rx.borrow_and_update().clone();
        if update.generation < request.minimum_config_generation {
            let _ = request
                .reply
                .send(Err(protocol::ProtocolError::session_terminal_unavailable()));
            return;
        }
        apply_detector_config(detector, tick, applied_generation, update);
    }

    let _ = request.reply.send(Ok(detector.region_previews()));
}

impl SessionRegistry {
    pub(super) fn spawn_detector(&self, inputs: DetectorInputs) {
        let DetectorInputs {
            scope,
            output: mut output_rx,
            initial_size: size,
            cancel,
            resize: mut resize_rx,
            config: mut detector_config_rx,
            preview: mut preview_rx,
        } = inputs;
        let registry = self.clone();
        tokio::spawn(async move {
            let DetectorScope { id, runtime } = scope;
            let initial_config = detector_config_rx.borrow().clone();
            let mut applied_config_generation = initial_config.generation;
            let mut tick = detection_interval(&initial_config.config);
            tick.tick().await;
            let (rows, cols) = size;
            let mut detector = Detector::new(rows, cols, Instant::now(), initial_config.config);
            let mut lag_warn =
                LagWarnThrottle::new(registry.inner.config.detector_lag_warn_interval);

            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    changed = detector_config_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let update = detector_config_rx.borrow_and_update().clone();
                        apply_detector_config(
                            &mut detector,
                            &mut tick,
                            &mut applied_config_generation,
                            update,
                        );
                    }
                    request = preview_rx.recv() => {
                        let Some(request) = request else {
                            break;
                        };
                        reply_to_preview(
                            &mut detector,
                            &mut tick,
                            &mut detector_config_rx,
                            &mut applied_config_generation,
                            request,
                        );
                    }
                    _ = tick.tick() => {
                        for transition in detector.tick(Instant::now()) {
                            registry
                                .record_detector_activity(&id, &runtime, transition)
                                .await;
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
                                    registry
                                        .record_detector_activity(&id, &runtime, transition)
                                        .await;
                                }
                                if let Some(path) = detector.take_cwd_hint() {
                                    registry
                                        .record_cwd_hint_scoped(&id, path, Some(&runtime))
                                        .await;
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
        let managed = {
            let sessions = self.inner.sessions.lock().await;
            sessions.get(id).map(|entry| {
                (
                    entry.detector_preview.clone(),
                    entry.detector_config.borrow().generation,
                )
            })
        };
        let Some((preview, minimum_config_generation)) = managed else {
            if self.inner.external.contains_id(id).await {
                return Err(protocol::ProtocolError::session_has_no_managed_terminal());
            }
            return Err(super::session_not_found(&id.0));
        };
        let (reply, response) = tokio::sync::oneshot::channel();
        preview
            .send(DetectionPreviewRequest {
                minimum_config_generation,
                reply,
            })
            .await
            .map_err(|_send_error| protocol::ProtocolError::session_terminal_unavailable())?;
        let previews = response
            .await
            .map_err(|_receive_error| protocol::ProtocolError::session_terminal_unavailable())??;

        Ok(protocol::SessionDetectionResult {
            session_id: id.clone(),
            supported_regions: protocol::DetectionRegionKind::ALL.to_vec(),
            previews,
        })
    }

    #[cfg(test)]
    pub(super) async fn record_activity(&self, id: &SessionId, transition: ActivityTransition) {
        self.record_activity_scoped(id, None, transition).await;
    }

    pub(super) async fn record_detector_activity(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        transition: ActivityTransition,
    ) {
        self.record_activity_scoped(id, Some(expected), transition)
            .await;
    }

    async fn record_activity_scoped(
        &self,
        id: &SessionId,
        expected: Option<&RuntimeWatchIdentity>,
        transition: ActivityTransition,
    ) {
        let activity_epoch = self.daemon_instance_id().to_owned();
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, "detector activity arrived for unknown session");
                return;
            };

            if expected.is_some_and(|expected| !expected.matches(entry)) {
                debug!(
                    session_id = %id.0,
                    "detector activity arrived for a superseded runtime"
                );
                return;
            }

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
            let evidence = record_activity_evidence(
                entry,
                transition.activity,
                transition.source,
                &activity_epoch,
            );
            let rescan = (transition.activity == AgentActivity::Working)
                .then(|| std::sync::Arc::clone(&entry.procwatch_rescan));
            (rescan, evidence)
        };
        if let Some(rescan) = updated.0 {
            rescan.notify_one();
        }

        let Some(evidence) = updated.1 else {
            return;
        };
        let event = crate::events::event(
            event::AGENT_STATE,
            event_payload(evidence.event(id.clone())),
        );
        let _ = self.inner.events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use protocol::DetectionRegionKind;

    use super::{
        detection_interval, reply_to_preview, DetectionPreviewRequest, Detector, DetectorConfig,
        DetectorConfigUpdate, Instant,
    };
    use crate::detect::{DetectionConfig, Manifest};

    #[tokio::test]
    async fn preview_applies_an_accepted_pending_configuration_first() {
        let initial = DetectorConfig {
            detection: DetectionConfig::default(),
            manifest: Some(
                Manifest::parse_str(
                    r#"
                    [[rules]]
                    id = "old"
                    state = "idle"
                    priority = 1
                    region = "osc_title"
                    contains = "old"
                    "#,
                )
                .expect("initial manifest parses"),
            ),
        };
        let updated = DetectorConfig {
            detection: DetectionConfig::default(),
            manifest: Some(
                Manifest::parse_str(
                    r#"
                    [[rules]]
                    id = "new"
                    state = "idle"
                    priority = 1
                    region = "bottom_lines(3)"
                    contains = "new"
                    "#,
                )
                .expect("updated manifest parses"),
            ),
        };
        let (config_tx, mut config_rx) = tokio::sync::watch::channel(DetectorConfigUpdate {
            generation: 0,
            config: initial.clone(),
        });
        let mut detector = Detector::new(24, 80, Instant::now(), initial.clone());
        let mut tick = detection_interval(&initial);
        tick.tick().await;
        let mut applied_generation = 0;
        config_tx
            .send(DetectorConfigUpdate {
                generation: 1,
                config: updated,
            })
            .expect("detector accepts configuration");
        let (reply, response) = tokio::sync::oneshot::channel();
        reply_to_preview(
            &mut detector,
            &mut tick,
            &mut config_rx,
            &mut applied_generation,
            DetectionPreviewRequest {
                minimum_config_generation: 1,
                reply,
            },
        );

        let previews = response
            .await
            .expect("detector replies")
            .expect("preview succeeds");
        assert_eq!(applied_generation, 1);
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].kind, DetectionRegionKind::BottomLines);
        assert_eq!(previews[0].region, "bottom_lines(3)");
    }
}
