//! User/initial input framing and PTY delivery.

use pohunek_worker_protocol::{InputFragment as WorkerInputFragment, SecretBytes};

use protocol::AgentStateEvent;

use super::{
    adapter_for, broadcast, session_not_found, session_not_running, unavailable_runtime_error,
    worker_error_to_protocol, AgentActivity, AgentKind, Duration, ErrorClass, InputRules,
    LaunchCommand, LaunchCommandPlan, ProtocolError, ResolvedAgent, RuntimeHandle, SessionId,
    SessionInputParams, SessionInputResult, SessionInputWait, SessionRegistry,
    SessionRegistryConfig, SessionState,
};

pub(super) const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
pub(super) const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
pub(super) const SUBMIT: &[u8] = b"\r";

/// Activities that settle a wait when the caller supplies no explicit targets.
const DEFAULT_INPUT_WAIT_UNTIL: [AgentActivity; 2] = [AgentActivity::Idle, AgentActivity::Blocked];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InputActivitySnapshot {
    pub(super) activity: Option<AgentActivity>,
    pub(super) source: protocol::StateSource,
    pub(super) revision: u64,
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
            .input_activity_snapshot(&params.session_id)
            .await?
            .activity
            == Some(AgentActivity::Blocked)
        {
            return Err(ProtocolError::session_agent_blocked());
        }
        self.write_input_to_session(&params.session_id, &params.text)
            .await?;
        let submitted = self.input_activity_snapshot(&params.session_id).await?;
        self.await_input_settled(&params.session_id, wait, submitted.revision, &mut events)
            .await
    }

    pub(super) async fn write_input_to_session(
        &self,
        session_id: &SessionId,
        text: &str,
    ) -> Result<(), ProtocolError> {
        self.ensure_not_external(session_id).await?;
        let (runtime, rules) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get(session_id)
                .ok_or_else(|| session_not_found(&session_id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(session_id));
            }
            entry.input_rules.validate_activity(entry.info.activity)?;
            (entry.runtime.clone(), entry.input_rules)
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

        Ok(())
    }

    pub(super) async fn input_activity_snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<InputActivitySnapshot, ProtocolError> {
        let sessions = self.inner.sessions.lock().await;
        let entry = sessions
            .get(session_id)
            .ok_or_else(|| session_not_found(&session_id.0))?;
        Ok(InputActivitySnapshot {
            activity: entry.info.activity,
            source: entry.info.state_source,
            revision: entry.activity_revision,
        })
    }

    pub(super) async fn await_input_settled(
        &self,
        session_id: &SessionId,
        wait: SessionInputWait,
        submitted_revision: u64,
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
                        .evaluate_input_wait(session_id, targets, submitted_revision)
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
                    Ok(event) if event.event() != protocol::event::AGENT_STATE => continue,
                    Ok(event) => {
                    let state: AgentStateEvent = serde_json::from_value(event.payload().clone())
                        .map_err(|_error| {
                            ProtocolError::new(
                                ErrorClass::Daemon,
                                "daemon_event_invalid",
                                "agent state event payload was invalid during input wait",
                                None,
                            )
                        })?;
                    if state.session_id != *session_id {
                        continue;
                    }
                    }
                },
            }
            if let Some(result) = self
                .evaluate_input_wait(session_id, targets, submitted_revision)
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
        submitted_revision: u64,
    ) -> Result<Option<SessionInputResult>, ProtocolError> {
        let snapshot = self.input_activity_snapshot(session_id).await?;
        let Some(activity) = snapshot.activity else {
            return Ok(None);
        };
        if snapshot.revision <= submitted_revision || !targets.contains(&activity) {
            return Ok(None);
        }
        Ok(Some(SessionInputResult {
            accepted: true,
            activity: Some(activity),
            activity_source: Some(snapshot.source),
        }))
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
