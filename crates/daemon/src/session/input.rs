//! User/initial input framing and PTY delivery.

use super::{
    adapter_for, pty_error_to_protocol, session_not_found, session_not_running, warn, AgentKind,
    Duration, InputRules, LaunchCommandPlan, ProtocolError, PtyCommand, ResolvedAgent, SessionId,
    SessionInputParams, SessionInputResult, SessionRegistry, SessionRegistryConfig, SessionState,
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
        let (pty, rules) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get(session_id)
                .ok_or_else(|| session_not_found(&session_id.0))?;
            if entry.info.state != SessionState::Running {
                return Err(session_not_running(session_id));
            }
            (entry.pty.clone(), entry.input_rules)
        };

        let writes = build_input_writes(text, rules);
        pty.write_user_input(writes.immediate)
            .await
            .map_err(pty_error_to_protocol)?;

        if let Some((delay, bytes)) = writes.delayed_submit {
            let delayed_pty = pty.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Err(err) = delayed_pty.write_user_input(bytes).await {
                    warn!(error = %err, "failed to write delayed agent submit byte");
                }
            });
        }

        Ok(())
    }
}

pub(super) fn plan_initial_input_delivery(
    resolved: &ResolvedAgent,
    mut command: PtyCommand,
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
