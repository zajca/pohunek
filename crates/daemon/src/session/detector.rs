//! Per-session activity detector task and activity recording.

use super::{
    activity_already_recorded, broadcast, debug, event, event_payload, is_terminal, log_lag_warn,
    record_activity_evidence, timestamp_now, ActivityTransition, AgentActivity,
    DetectionPreviewRequest, Detector, DetectorConfig, DetectorConfigUpdate, DetectorInputs,
    DetectorScope, LagWarnThrottle, RuntimeWatchIdentity, SessionId, SessionRegistry,
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
    #[expect(
        clippy::too_many_lines,
        reason = "the detector task is one select loop over all of its inputs"
    )]
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
            initial_input_done,
        } = inputs;
        let registry = self.clone();
        let progress = self.inner.turns.progress(&scope.id);
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
                if initial_input_done.is_cancelled() {
                    detector.finish_initial_input();
                }
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
                        if !initial_input_done.is_cancelled() {
                            input_ready.send_replace(detector.input_ready());
                        }
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
                        if !initial_input_done.is_cancelled() {
                            input_ready.send_replace(detector.input_ready());
                        }
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
                                progress.note_output();
                                if initial_input_done.is_cancelled() {
                                    detector.finish_initial_input();
                                }
                                let transitions = detector.feed(crate::time::now(), &chunk);
                                if !initial_input_done.is_cancelled() {
                                    input_ready.send_replace(detector.input_ready());
                                }
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
                                // Skipped chunks are output too.
                                progress.note_output();
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

        // The program-status preview stays off the wire while
        // `DetectionRegionKind` is a strict enum, so a client that predates the
        // kind decodes every result.
        let previews = previews
            .into_iter()
            .filter(|preview| preview.kind != protocol::DetectionRegionKind::ProgramStatus)
            .collect();
        Ok(protocol::SessionDetectionResult {
            session_id: id.clone(),
            supported_regions: protocol::DetectionRegionKind::ALL
                .into_iter()
                .filter(|kind| *kind != protocol::DetectionRegionKind::ProgramStatus)
                .collect(),
            previews,
        })
    }

    #[cfg(test)]
    pub(super) async fn record_activity(&self, id: &SessionId, transition: ActivityTransition) {
        self.record_activity_scoped(id, None, transition).await;
    }

    /// Feeds one detector transition, as the detector task does.
    #[cfg(test)]
    pub(crate) async fn feed_detector_activity(&self, id: &SessionId, activity: AgentActivity) {
        let transition = ActivityTransition {
            activity,
            source: protocol::StateSource::Process,
        };
        self.record_activity(id, transition).await;
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

            // A restated activity is neither a new revision nor an event. A
            // waiter still gets it as evidence newer than its boundary. The
            // waiter is looked up under the session lock: a wait registers
            // before it takes its boundary under that lock, so a boundary this
            // restatement follows always finds its waiter.
            if !self.has_evidence_waiter(id)
                && entry.info.activity == Some(transition.activity)
                && entry.info.state_source == transition.source
                && activity_already_recorded(entry, transition.activity, transition.source)
            {
                // A turn that gave its input evidence back opens here.
                if transition.activity == AgentActivity::Working {
                    self.inner.turns.note_activity(
                        id,
                        entry.turn_owner(),
                        entry.info.activity,
                        transition.activity,
                        &entry.info.metadata,
                        true,
                    );
                }
                return;
            }

            self.inner.turns.note_activity(
                id,
                entry.turn_owner(),
                entry.info.activity,
                transition.activity,
                &entry.info.metadata,
                true,
            );
            entry.set_activity(transition.activity);
            self.wake_idle_stop();
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
        self.publish(event);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
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
            input_readiness: crate::agent::host::InputReadiness::default(),
            detection: DetectionConfig::default(),
            manifest: Some(Arc::new(
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
            )),
        };
        let updated = DetectorConfig {
            input_readiness: crate::agent::host::InputReadiness::default(),
            detection: DetectionConfig::default(),
            manifest: Some(Arc::new(
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
            )),
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
                    let state: protocol::AgentStateEvent =
                        serde_json::from_value(event.payload().clone())
                            .expect("valid agent state event");
                    // Only the detector under test reports from the screen.
                    if state.source == StateSource::Screen {
                        return state;
                    }
                }
            }
        })
        .await
        .expect("an agent state event arrives before the virtual-time ceiling")
    }

    struct StableDetector {
        registry: SessionRegistry,
        session: protocol::SessionInfo,
        events: broadcast::Receiver<protocol::Event>,
        output: broadcast::Sender<Vec<u8>>,
        cancel: CancellationToken,
        // Held so the detector loop's resize, config and preview channels stay open.
        _channels: (
            watch::Sender<(u16, u16)>,
            watch::Sender<DetectorConfigUpdate>,
            mpsc::Sender<DetectionPreviewRequest>,
        ),
    }

    impl StableDetector {
        async fn start(manifest: &str) -> Self {
            let registry = SessionRegistry::new(SessionRegistryConfig {
                shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
                stop_grace: Duration::from_millis(50),
                ..SessionRegistryConfig::default()
            });
            let cwd = pohunek_test_support::tempdir().expect("private session cwd");
            let session = registry
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
                    extended_input_ready_wait: None,
                    branch_source: None,
                    on_diverged: None,
                    input_ack_timeout_ms: None,
                    metadata: BTreeMap::new(),
                })
                .await
                .expect("create shell session");
            // The session's own detector would add process-sourced events of
            // its own; the detector under test is the only one.
            registry.inner.sessions.lock().await[&session.id]
                .detector_cancel
                .cancel();
            let events = registry.subscribe();

            let config = DetectorConfig {
                input_readiness: crate::agent::host::InputReadiness::default(),
                detection: DetectionConfig {
                    recheck_after: TEST_RECHECK_AFTER,
                    confirmations: 1,
                    cap: TEST_EVENT_CEILING,
                    stable_visible_refresh: TEST_STABLE_VISIBLE_REFRESH,
                    startup_grace: Duration::ZERO,
                },
                manifest: Some(Arc::new(
                    Manifest::parse_str(manifest).expect("manifest parses"),
                )),
            };
            let (output, output_rx) = broadcast::channel(8);
            let (resize_tx, resize_rx) = watch::channel((24, 80));
            let (config_tx, config_rx) = watch::channel(DetectorConfigUpdate {
                generation: 0,
                config,
            });
            let (preview_tx, preview_rx) = mpsc::channel(1);
            let (input_ready, _ready_rx) = watch::channel(false);
            let cancel = CancellationToken::new();
            registry.spawn_detector(DetectorInputs {
                scope: DetectorScope {
                    id: session.id.clone(),
                    runtime: RuntimeWatchIdentity::from_info(&session)
                        .expect("created session has a live runtime identity"),
                },
                output: output_rx,
                initial_size: (24, 80),
                cancel: cancel.clone(),
                resize: resize_rx,
                config: config_rx,
                preview: preview_rx,
                input_ready,
                initial_input_done: super::super::CancellationToken::new(),
            });
            Self {
                registry,
                session,
                events,
                output,
                cancel,
                _channels: (resize_tx, config_tx, preview_tx),
            }
        }

        async fn revision(&self) -> u64 {
            self.registry.inner.sessions.lock().await[&self.session.id].activity_revision
        }

        async fn finish(self) {
            tokio::time::resume();
            self.cancel.cancel();
            let _ = self.registry.stop(&self.session.id).await;
        }
    }

    const WORKING_MANIFEST: &str = r#"
        [[rules]]
        id = "visible-working"
        state = "working"
        priority = 1
        region = "whole_recent"
        contains = "compiling workspace"
    "#;

    const IDLE_MANIFEST: &str = r#"
        [[rules]]
        id = "visible-idle"
        state = "idle"
        priority = 1
        region = "whole_recent"
        contains = "ready for input"
    "#;

    /// Whether another `agent_state` event arrives within `window` of virtual time.
    async fn agent_state_within(
        events: &mut broadcast::Receiver<protocol::Event>,
        window: Duration,
    ) -> bool {
        tokio::time::timeout(window, async {
            loop {
                let event = events.recv().await.expect("event stream stays open");
                if event.event() == protocol::event::AGENT_STATE
                    && event.payload()["source"] == "screen"
                {
                    return;
                }
            }
        })
        .await
        .is_ok()
    }

    /// The loop must read tokio's clock: the refresh interval only elapses
    /// because paused time was advanced, and a stable state restated across
    /// it is neither a new event nor a new revision.
    #[tokio::test]
    async fn an_idle_session_left_alone_emits_no_further_agent_state() {
        let mut detector = StableDetector::start(IDLE_MANIFEST).await;

        tokio::time::pause();
        detector
            .output
            .send(b"ready for input".to_vec())
            .expect("detector holds the output receiver");
        let first = next_agent_state(&mut detector.events).await;
        assert_eq!(first.activity, AgentActivity::Idle);
        assert_eq!(first.source, StateSource::Screen);
        let revision = detector.revision().await;

        assert!(
            !agent_state_within(&mut detector.events, TEST_STABLE_VISIBLE_REFRESH * 5).await,
            "an unchanged idle state is not restated"
        );
        assert_eq!(detector.revision().await, revision);

        detector.finish().await;
    }

    #[tokio::test]
    async fn a_waiting_input_still_gets_the_idle_state_restated_after_its_boundary() {
        let mut detector = StableDetector::start(IDLE_MANIFEST).await;

        tokio::time::pause();
        detector
            .output
            .send(b"ready for input".to_vec())
            .expect("detector holds the output receiver");
        let first = next_agent_state(&mut detector.events).await;
        let _waiter = detector
            .registry
            .watch_activity_evidence(&detector.session.id);

        tokio::time::advance(TEST_STABLE_VISIBLE_REFRESH).await;
        let restated = next_agent_state(&mut detector.events).await;
        assert_eq!(restated.activity, AgentActivity::Idle);
        assert!(
            restated.revision > first.revision,
            "a registered waiter receives newer evidence of the unchanged state"
        );

        detector.finish().await;
    }

    #[tokio::test]
    async fn a_changed_state_still_publishes_a_new_revision() {
        let mut detector =
            StableDetector::start(&format!("{IDLE_MANIFEST}{WORKING_MANIFEST}")).await;

        tokio::time::pause();
        detector
            .output
            .send(b"ready for input".to_vec())
            .expect("detector holds the output receiver");
        let idle = next_agent_state(&mut detector.events).await;
        detector
            .output
            .send(b"\x1b[2J\x1b[HCompiling workspace".to_vec())
            .expect("detector holds the output receiver");
        let working = next_agent_state(&mut detector.events).await;
        assert_eq!(working.activity, AgentActivity::Working);
        assert!(working.revision > idle.revision);

        detector.finish().await;
    }
}
