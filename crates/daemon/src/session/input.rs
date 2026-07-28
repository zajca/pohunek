//! User/initial input framing and PTY delivery.

use pohunek_worker_protocol::{InputFragment as WorkerInputFragment, SecretBytes};

use super::{
    adapter_for, session_not_found, session_not_running, unavailable_runtime_error,
    worker_error_to_protocol, AgentKind, Duration, InputRules, LaunchCommand, LaunchCommandPlan,
    ProtocolError, ResolvedAgent, RuntimeHandle, SessionId, SessionInputParams, SessionInputResult,
    SessionRegistry, SessionRegistryConfig, SessionState,
};

pub(super) const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
pub(super) const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
pub(super) const SUBMIT: &[u8] = b"\r";

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
        self.write_input_to_session(&params.session_id, &params.text)
            .await?;
        Ok(SessionInputResult { accepted: true })
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
            (entry.runtime.clone(), entry.input_rules)
        };

        let writes = build_input_writes(text, rules);
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
}

pub(super) fn plan_initial_input_delivery(
    resolved: &ResolvedAgent,
    mut command: LaunchCommand,
    initial_input: Option<String>,
) -> LaunchCommandPlan {
    if resolved.profile.is_none() && prompt_arg_supported(resolved.base) {
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

pub(super) fn prompt_arg_supported(agent: AgentKind) -> bool {
    matches!(agent, AgentKind::Codex | AgentKind::Claude)
}

pub(super) fn input_rules_for_agent(
    agent: AgentKind,
    config: &SessionRegistryConfig,
) -> InputRules {
    let mut rules = adapter_for(agent).input_rules();
    if agent == AgentKind::Claude {
        rules.submit_delay = config.claude_submit_delay;
    }
    rules
}

pub(super) fn build_input_writes(text: &str, rules: InputRules) -> InputWritePlan {
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

    InputWritePlan {
        immediate,
        delayed_submit,
    }
}
