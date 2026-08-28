//! User/initial input framing and PTY delivery.

use pohunek_worker_protocol::{InputFragment as WorkerInputFragment, SecretBytes};

use protocol::{ActivityRevision, AgentStateEvent, SessionRuntimeIdentity};

use super::{
    adapter_for, broadcast, session_not_found, session_not_running, unavailable_runtime_error,
    worker_error_to_protocol, ActivityEvidence, AgentActivity, AgentKind, Duration, ErrorClass,
    InputRules, LaunchCommand, LaunchCommandPlan, ProtocolError, ResolvedAgent, RuntimeHandle,
    SessionEntry, SessionId, SessionInputParams, SessionInputResult, SessionInputWait,
    SessionRegistry, SessionRegistryConfig, SessionState,
};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InputSubmission {
    pub(super) runtime: SessionRuntimeIdentity,
    pub(super) completed_at: std::time::Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InputWritePlan {
    pub(super) immediate: Vec<u8>,
    pub(super) delayed_submit: Option<(Duration, Vec<u8>)>,
}

impl SessionRegistry {
    /// Inject text into a running session using the agent's input framing rules.
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
                activity_revision: None,
            });
        };

        let mut wait = wait;
        let mut seen = Vec::new();
        wait.until.retain(|activity| {
            if seen.contains(activity) {
                false
            } else {
                seen.push(*activity);
                true
            }
        });
        Self::validate_input_wait(&wait)?;
        let mut events = self.subscribe();
        let _waiter_permit = self.acquire_waiter(&params.session_id)?;
        if self
            .input_activity_snapshot(&params.session_id, None)
            .await?
            .activity
            == Some(AgentActivity::Blocked)
        {
            return Err(ProtocolError::session_agent_blocked());
        }
        let submission = self
            .write_input_to_session(&params.session_id, &params.text)
            .await?;
        self.input_activity_snapshot(&params.session_id, Some(&submission.runtime))
            .await?;
        self.await_input_settled(&params.session_id, wait, submission, &mut events)
            .await
    }

    pub(super) async fn write_input_to_session(
        &self,
        session_id: &SessionId,
        text: &str,
    ) -> Result<InputSubmission, ProtocolError> {
        self.ensure_not_external(session_id).await?;
        let (runtime, rules, runtime_identity) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get(session_id)
                .ok_or_else(|| session_not_found(&session_id.0))?;
            let runtime_identity = input_runtime_identity(entry, session_id)?;
            entry.input_rules.validate_activity(entry.info.activity)?;
            (entry.runtime.clone(), entry.input_rules, runtime_identity)
        };

        let writes = build_input_writes(text, rules)?;
        match runtime {
            RuntimeHandle::Worker(worker) => {
                let mut fragments = Vec::with_capacity(2);
                let delay_after_ms = writes.delayed_submit.as_ref().map_or(0, |(delay, _)| {
                    u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)
                });
                fragments.push(WorkerInputFragment {
                    bytes: SecretBytes::new(writes.immediate),
                    delay_after_ms,
                });
                if let Some((_delay, bytes)) = writes.delayed_submit {
                    fragments.push(WorkerInputFragment {
                        bytes: SecretBytes::new(bytes),
                        delay_after_ms: 0,
                    });
                }
                worker
                    .write(fragments)
                    .await
                    .map_err(worker_error_to_protocol)?;
            }
            RuntimeHandle::Unavailable(state) => {
                return Err(unavailable_runtime_error(session_id, state));
            }
        }

        Ok(InputSubmission {
            runtime: runtime_identity,
            completed_at: std::time::Instant::now(),
        })
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
        let runtime = input_runtime_identity(entry, session_id)?;
        if expected_runtime.is_some_and(|expected| expected != &runtime) {
            return Err(ProtocolError::session_runtime_changed());
        }
        Ok(InputActivitySnapshot {
            activity: entry.info.activity,
            revision: ActivityRevision::new(entry.activity_revision),
            runtime,
        })
    }

    pub(super) async fn await_input_settled(
        &self,
        session_id: &SessionId,
        wait: SessionInputWait,
        submission: InputSubmission,
        events: &mut broadcast::Receiver<protocol::Event>,
    ) -> Result<SessionInputResult, ProtocolError> {
        let targets: &[AgentActivity] = if wait.until.is_empty() {
            &DEFAULT_INPUT_WAIT_UNTIL
        } else {
            &wait.until
        };
        let deadline_ms = wait.timeout_ms.unwrap_or(protocol::MAX_SESSION_WAIT_MS);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(deadline_ms));

        loop {
            tokio::select! {
                biased;
                () = self.inner.event_log_shutdown.cancelled() => {
                    return Err(input_wait_shutdown_error(
                        "daemon shutdown cancelled the bounded input wait",
                    ));
                }
                () = tokio::time::sleep_until(deadline) => {
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
                    return Err(ProtocolError::session_input_timeout());
                }
                received = events.recv() => match received {
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(input_wait_shutdown_error(
                            "daemon event channel closed during bounded input wait",
                        ));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Ok(event) => {
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

    async fn evaluate_input_wait(
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
        Ok(entry
            .activity_evidence
            .values()
            .filter(|evidence| {
                evidence.runtime == submission.runtime
                    && evidence.observed_at > submission.completed_at
                    && targets.contains(&evidence.activity)
                    && event_revision.is_none_or(|revision| evidence.revision == revision)
            })
            .min_by_key(|evidence| evidence.revision)
            .cloned()
            .map(input_wait_result))
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

fn input_runtime_identity(
    entry: &SessionEntry,
    session_id: &SessionId,
) -> Result<SessionRuntimeIdentity, ProtocolError> {
    if entry.info.state != SessionState::Running {
        return Err(session_not_running(session_id));
    }
    let runtime = entry
        .info
        .runtime
        .as_ref()
        .ok_or_else(ProtocolError::session_terminal_unavailable)?;
    if runtime.state != protocol::RuntimeState::Live {
        return Err(unavailable_runtime_error(session_id, runtime.state));
    }
    let runtime_id = runtime
        .runtime_id
        .clone()
        .ok_or_else(ProtocolError::session_terminal_unavailable)?;
    SessionRuntimeIdentity::new(runtime_id, runtime.runtime_generation)
        .map_err(|_error| ProtocolError::session_terminal_unavailable())
}

fn input_wait_result(evidence: ActivityEvidence) -> SessionInputResult {
    SessionInputResult {
        accepted: true,
        activity: Some(evidence.activity),
        activity_source: Some(evidence.source),
        runtime: Some(evidence.runtime),
        activity_revision: Some(evidence.revision),
    }
}

fn input_wait_shutdown_error(message: &str) -> ProtocolError {
    ProtocolError::new(ErrorClass::Daemon, "daemon_shutting_down", message, None)
}

pub(super) fn plan_initial_input_delivery(
    resolved: &ResolvedAgent,
    mut command: LaunchCommand,
    initial_input: Option<String>,
) -> LaunchCommandPlan {
    if resolved.profile.is_none() && prompt_arg_supported(&resolved.base) {
        if let Some(input) = initial_input {
            command.args.push(input);
        }
        return LaunchCommandPlan {
            command,
            pending_initial_input: None,
        };
    }

    LaunchCommandPlan {
        command,
        pending_initial_input: initial_input,
    }
}

pub(super) fn prompt_arg_supported(agent: &AgentKind) -> bool {
    matches!(agent, AgentKind::Codex | AgentKind::Claude)
}

pub(super) fn input_rules_for_agent(
    agent: &AgentKind,
    config: &SessionRegistryConfig,
) -> InputRules {
    let mut rules = adapter_for(agent).input_rules();
    if *agent == AgentKind::Claude {
        rules.submit_delay = config.claude_submit_delay;
    }
    rules
}

pub(super) fn build_input_writes(
    text: &str,
    rules: InputRules,
) -> Result<InputWritePlan, ProtocolError> {
    rules.validate_text(text)?;
    let mut immediate = Vec::new();
    if rules.bracketed_paste {
        immediate.extend_from_slice(BRACKETED_PASTE_START);
    }
    immediate.extend_from_slice(text.as_bytes());
    if rules.bracketed_paste {
        immediate.extend_from_slice(BRACKETED_PASTE_END);
    }

    let delayed_submit = if rules.submit_delay.is_zero() {
        immediate.extend_from_slice(SUBMIT);
        None
    } else {
        Some((rules.submit_delay, SUBMIT.to_vec()))
    };

    Ok(InputWritePlan {
        immediate,
        delayed_submit,
    })
}
