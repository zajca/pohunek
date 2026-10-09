//! Per-session activity detector task and activity recording.

use super::{
    broadcast, debug, event, event_payload, is_terminal, log_lag_warn, record_activity_evidence,
    timestamp_now, ActivityTransition, AgentActivity, DetectionPreviewRequest, Detector,
    DetectorConfig, DetectorConfigUpdate, DetectorInputs, DetectorScope, LagWarnThrottle,
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
    detector.reconfigure(crate::time::now(), update.config);
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
            input_ready,
        } = inputs;
        let registry = self.clone();
        tokio::spawn(async move {
            let DetectorScope { id, runtime } = scope;
            let initial_config = detector_config_rx.borrow().clone();
            let mut applied_config_generation = initial_config.generation;
            let mut tick = detection_interval(&initial_config.config);
            tick.tick().await;
            let (rows, cols) = size;
            let mut detector = Detector::new(rows, cols, crate::time::now(), initial_config.config);
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
                        input_ready.send_replace(detector.input_ready());
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
                        input_ready.send_replace(detector.input_ready());
                        for transition in detector.tick(crate::time::now()) {
                            registry
                                .record_detector_activity(&id, &runtime, transition)
                                .await;
                        }
                        // Flush a folded lag batch whose window has elapsed, so a
                        // session that stopped lagging still reports its summary.
                        if let Some(warn_kind) = lag_warn.poll(crate::time::now()) {
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
                                let transitions = detector.feed(crate::time::now(), &chunk);
                                input_ready.send_replace(detector.input_ready());
                                for transition in transitions {
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
                                if let Some(warn_kind) = lag_warn.observe(crate::time::now(), skipped) {
                                    log_lag_warn(&id, warn_kind);
                                }
                                detector.resync_after_lag();
                                input_ready.send_replace(false);
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
    use std::collections::BTreeMap;
    use std::time::Duration;

    use protocol::{AgentActivity, DetectionRegionKind, SessionNewParams, StateSource};
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio_util::sync::CancellationToken;

    use super::{
        detection_interval, reply_to_preview, DetectionPreviewRequest, Detector, DetectorConfig,
        DetectorConfigUpdate,
    };
    use crate::detect::{DetectionConfig, Manifest};
    use crate::session::{
        DetectorInputs, DetectorScope, RuntimeWatchIdentity, SessionRegistry,
        SessionRegistryConfig, ShellCommand,
    };

    /// Detector recheck cadence of the loop under test.
    const TEST_RECHECK_AFTER: Duration = Duration::from_millis(100);
    /// Age at which a stable visible state is re-emitted by `Detector::tick`.
    const TEST_STABLE_VISIBLE_REFRESH: Duration = Duration::from_millis(800);
    /// Virtual-time bound on waiting for one agent-state event; a missing event
    /// resolves as a deterministic timeout once the paused clock auto-advances.
    const TEST_EVENT_CEILING: Duration = Duration::from_secs(5);

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
        let mut detector = Detector::new(24, 80, crate::time::now(), initial.clone());
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

    async fn next_agent_state(
        events: &mut broadcast::Receiver<protocol::Event>,
    ) -> protocol::AgentStateEvent {
        tokio::time::timeout(TEST_EVENT_CEILING, async {
            loop {
                let event = events.recv().await.expect("event stream stays open");
                if event.event() == protocol::event::AGENT_STATE {
                    return serde_json::from_value(event.payload().clone())
                        .expect("valid agent state event");
                }
            }
        })
        .await
        .expect("an agent state event arrives before the virtual-time ceiling")
    }

    /// The loop must read tokio's clock: a refresh that is due only because
    /// paused time was advanced reaches the session, which a detector reading
    /// `std::time::Instant::now()` never observes.
    #[tokio::test]
    async fn detector_loop_refreshes_a_stable_state_after_virtual_time_advances() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        });
        let cwd = pohunek_test_support::tempdir().expect("private session cwd");
        let created = registry
            .create(SessionNewParams {
                name: None,
                agent: "shell".to_owned(),
                cwd: Some(cwd.path().to_path_buf()),
                cols: 80,
                rows: 24,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                metadata: BTreeMap::new(),
            })
            .await
            .expect("create shell session");
        let mut events = registry.subscribe();

        let config = DetectorConfig {
            detection: DetectionConfig {
                recheck_after: TEST_RECHECK_AFTER,
                confirmations: 1,
                cap: TEST_EVENT_CEILING,
                stable_visible_refresh: TEST_STABLE_VISIBLE_REFRESH,
                startup_grace: Duration::ZERO,
            },
            manifest: Some(
                Manifest::parse_str(
                    r#"
                    [[rules]]
                    id = "visible-working"
                    state = "working"
                    priority = 1
                    region = "whole_recent"
                    contains = "compiling workspace"
                    "#,
                )
                .expect("manifest parses"),
            ),
        };
        let (output_tx, output_rx) = broadcast::channel(8);
        let (_resize_tx, resize_rx) = watch::channel((24, 80));
        let (_config_tx, config_rx) = watch::channel(DetectorConfigUpdate {
            generation: 0,
            config,
        });
        let (_preview_tx, preview_rx) = mpsc::channel(1);
        let (input_ready, _ready_rx) = watch::channel(false);
        let cancel = CancellationToken::new();
        registry.spawn_detector(DetectorInputs {
            scope: DetectorScope {
                id: created.id.clone(),
                runtime: RuntimeWatchIdentity::from_info(&created)
                    .expect("created session has a live runtime identity"),
            },
            output: output_rx,
            initial_size: (24, 80),
            cancel: cancel.clone(),
            resize: resize_rx,
            config: config_rx,
            preview: preview_rx,
            input_ready,
        });

        tokio::time::pause();
        output_tx
            .send(b"Compiling workspace".to_vec())
            .expect("detector holds the output receiver");
        let first = next_agent_state(&mut events).await;
        assert_eq!(first.activity, AgentActivity::Working);
        assert_eq!(first.source, StateSource::Screen);

        tokio::time::advance(TEST_STABLE_VISIBLE_REFRESH).await;
        let refreshed = next_agent_state(&mut events).await;
        assert_eq!(refreshed.activity, AgentActivity::Working);
        assert_eq!(refreshed.source, StateSource::Screen);
        assert!(
            refreshed.revision > first.revision,
            "the tick re-emits the stable state as a new activity revision"
        );

        tokio::time::resume();
        cancel.cancel();
        let _ = registry.stop(&created.id).await;
    }
}
