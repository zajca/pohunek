//! Session-layer lifecycle hooks and the agent-state hook dispatcher.

use super::{
    broadcast, event, is_terminal, mpsc, run_hook, warn, AgentActivity, AgentKind,
    CancellationToken, Duration, Event, HashMap, HashSet, HookContext, HookEvent, JoinHandle,
    PathBuf, SessionId, SessionRegistry, Value, ENV_DAEMON_ID, ENV_FLAG, ENV_PROTOCOL_VERSION,
    ENV_SESSION_ID, ENV_SOCKET_PATH, EVENT_LOG_FLUSH_TIMEOUT, PROTOCOL_VERSION,
};

/// Debounce window for session-layer `agent-state` hooks. The detector/event log
/// still sees every transition immediately; only hook side effects wait briefly
/// so a short-lived visual flap does not run a hook for each intermediate value.
const AGENT_STATE_HOOK_DEBOUNCE: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub(super) struct AgentStateHookSnapshot {
    session_id: SessionId,
    project_id: Option<String>,
    cwd: PathBuf,
    agent: String,
    activity: AgentActivity,
}

#[derive(Debug, Clone)]
pub(super) struct SessionHookRequest {
    pub(super) event: HookEvent,
    pub(super) cwd: PathBuf,
    pub(super) session_id: String,
    pub(super) project_id: Option<String>,
    pub(super) agent: String,
    pub(super) stop_reason: Option<&'static str>,
    pub(super) activity: Option<&'static str>,
}

impl SessionRegistry {
    /// Start the agent-state hook dispatcher.
    ///
    /// This task subscribes to the registry's event stream, filters only
    /// `agent_state`, deduplicates by last-fired activity value, and runs
    /// `agent-state` hooks off the broadcast hot path. A no-op if it is already
    /// running.
    pub fn spawn_agent_state_hooks(&self) {
        let mut slot = self
            .inner
            .agent_state_hook_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return;
        }
        let handle = spawn_agent_state_hook_dispatcher(
            self.clone(),
            self.subscribe(),
            self.inner.agent_state_hook_shutdown.clone(),
        );
        *slot = Some(handle);
    }

    /// Stop the agent-state hook dispatcher and flush any pending debounced
    /// activity values before returning.
    pub async fn shutdown_agent_state_hooks(&self) {
        self.inner.agent_state_hook_shutdown.cancel();
        let handle = self
            .inner
            .agent_state_hook_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            if tokio::time::timeout(EVENT_LOG_FLUSH_TIMEOUT, handle)
                .await
                .is_err()
            {
                warn!("agent-state hook dispatcher did not finish within the shutdown timeout");
            }
        }
    }

    /// Fire a session-layer lifecycle hook without blocking the async hot path.
    ///
    /// The hook runner itself is synchronous (process spawn + bounded wait), so
    /// session events hand it to the blocking pool and only log non-fatal
    /// warnings. Worktree hook call sites can return warnings to `session.new`;
    /// session-start/stop/agent-state have no response field to carry them.
    pub(super) fn spawn_session_hook(&self, request: SessionHookRequest) {
        drop(self.spawn_session_hook_task(request));
    }

    fn spawn_session_hook_task(&self, request: SessionHookRequest) -> JoinHandle<()> {
        let timeout = self.inner.config.hook_timeout;
        let config_dir = self.inner.config.config_dir.clone();
        tokio::task::spawn_blocking(move || {
            let ctx = HookContext {
                session_id: request.session_id,
                project_id: request.project_id,
                agent: request.agent,
                repo: None,
                worktree: None,
                branch: None,
                base_branch: None,
                stop_reason: request.stop_reason,
                activity: request.activity,
            };
            let mut warnings = Vec::new();
            run_hook(
                request.event,
                &request.cwd,
                &ctx,
                timeout,
                config_dir.as_deref(),
                &mut warnings,
            );
            for warning in warnings {
                warn!(
                    event = request.event.as_env(),
                    session_id = %ctx.session_id,
                    cwd = %request.cwd.display(),
                    warning = %warning.message,
                    detail = ?warning.detail,
                    "session hook warning"
                );
            }
        })
    }

    /// Build the hook-handshake env injected into a session PTY so nested agent
    /// `SessionStart` hooks can report active-agent state back to the socket.
    /// Registries without a configured socket path get no hook env.
    pub(super) fn hook_env(
        &self,
        _agent: AgentKind,
        session_id: &SessionId,
    ) -> Vec<(String, String)> {
        match &self.inner.config.socket_path {
            Some(socket_path) => vec![
                (ENV_FLAG.to_owned(), "1".to_owned()),
                (
                    ENV_SOCKET_PATH.to_owned(),
                    socket_path.display().to_string(),
                ),
                (ENV_SESSION_ID.to_owned(), session_id.0.clone()),
                (
                    ENV_PROTOCOL_VERSION.to_owned(),
                    PROTOCOL_VERSION.get().to_string(),
                ),
            ],
            None => Vec::new(),
        }
    }

    /// Build the full env injected into a session's PTY.
    ///
    /// Always carries `POHUNEK_SESSION_ID` ([`ENV_SESSION_ID`]) and
    /// `POHUNEK_DAEMON_ID` ([`ENV_DAEMON_ID`]) for **every** agent kind —
    /// including a plain shell — so a `pohunek attach` launched inside the PTY can
    /// be recognized as a self-feeding loop and rejected (see
    /// [`SessionRegistry::attach`]); the daemon id scopes that decision to this
    /// instance regardless of which transport delivers the attach. When a socket
    /// is configured it additionally carries the hook handshake from
    /// [`Self::hook_env`] (which already includes the session id, so it is not
    /// duplicated here), including for shell sessions that may launch nested
    /// agents.
    pub(super) fn session_pty_env(
        &self,
        agent: AgentKind,
        session_id: &SessionId,
    ) -> Vec<(String, String)> {
        let mut env = self.hook_env(agent, session_id);
        if !env.iter().any(|(key, _)| key == ENV_SESSION_ID) {
            env.push((ENV_SESSION_ID.to_owned(), session_id.0.clone()));
        }
        env.push((
            ENV_DAEMON_ID.to_owned(),
            self.inner.daemon_instance_id.clone(),
        ));
        env
    }

    async fn agent_state_hook_snapshot(&self, id: &SessionId) -> Option<AgentStateHookSnapshot> {
        let sessions = self.inner.sessions.lock().await;
        let entry = sessions.get(id)?;
        if entry.stopping || is_terminal(entry.info.state) {
            return None;
        }
        Some(AgentStateHookSnapshot {
            session_id: entry.info.id.clone(),
            project_id: entry.info.project_id.clone(),
            cwd: entry.info.cwd.clone(),
            agent: entry.info.agent.clone(),
            activity: entry.info.activity?,
        })
    }

    async fn agent_state_hook_snapshots(&self) -> Vec<AgentStateHookSnapshot> {
        let sessions = self.inner.sessions.lock().await;
        sessions
            .values()
            .filter(|entry| !entry.stopping && !is_terminal(entry.info.state))
            .filter_map(|entry| {
                Some(AgentStateHookSnapshot {
                    session_id: entry.info.id.clone(),
                    project_id: entry.info.project_id.clone(),
                    cwd: entry.info.cwd.clone(),
                    agent: entry.info.agent.clone(),
                    activity: entry.info.activity?,
                })
            })
            .collect()
    }
}

pub(super) fn spawn_agent_state_hook_dispatcher(
    registry: SessionRegistry,
    mut events: broadcast::Receiver<Event>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_fired: HashMap<SessionId, AgentActivity> = HashMap::new();
        let mut pending: HashMap<SessionId, (AgentActivity, tokio::time::Instant)> = HashMap::new();
        let mut in_flight: HashSet<SessionId> = HashSet::new();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        let mut tick = tokio::time::interval(AGENT_STATE_HOOK_DEBOUNCE);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    drain_buffered_agent_state_events(
                        &registry,
                        &mut events,
                        &mut last_fired,
                        &mut pending,
                    ).await;
                    flush_pending_agent_state_hooks(
                        &registry,
                        &mut last_fired,
                        &mut pending,
                        &mut in_flight,
                        &done_tx,
                        true,
                    ).await;
                    while !in_flight.is_empty() {
                        let Some(session_id) = done_rx.recv().await else {
                            break;
                        };
                        in_flight.remove(&session_id);
                        flush_pending_agent_state_hooks(
                            &registry,
                            &mut last_fired,
                            &mut pending,
                            &mut in_flight,
                            &done_tx,
                            true,
                        ).await;
                    }
                    break;
                }
                Some(session_id) = done_rx.recv() => {
                    in_flight.remove(&session_id);
                    flush_pending_agent_state_hooks(
                        &registry,
                        &mut last_fired,
                        &mut pending,
                        &mut in_flight,
                        &done_tx,
                        false,
                    ).await;
                }
                _ = tick.tick() => {
                    flush_pending_agent_state_hooks(
                        &registry,
                        &mut last_fired,
                        &mut pending,
                        &mut in_flight,
                        &done_tx,
                        false,
                    ).await;
                }
                received = events.recv() => match received {
                    Ok(event) => {
                        if let Some((session_id, activity)) = parse_agent_state_event(&event) {
                            queue_agent_state_hook(&mut last_fired, &mut pending, session_id, activity);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        warn!(
                            dropped,
                            "agent-state hook dispatcher lagged; re-reading current activities"
                        );
                        for snapshot in registry.agent_state_hook_snapshots().await {
                            queue_agent_state_hook(
                                &mut last_fired,
                                &mut pending,
                                snapshot.session_id,
                                snapshot.activity,
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    })
}

async fn drain_buffered_agent_state_events(
    registry: &SessionRegistry,
    events: &mut broadcast::Receiver<Event>,
    last_fired: &mut HashMap<SessionId, AgentActivity>,
    pending: &mut HashMap<SessionId, (AgentActivity, tokio::time::Instant)>,
) {
    loop {
        match events.try_recv() {
            Ok(event) => {
                if let Some((session_id, activity)) = parse_agent_state_event(&event) {
                    queue_agent_state_hook(last_fired, pending, session_id, activity);
                }
            }
            Err(broadcast::error::TryRecvError::Lagged(dropped)) => {
                warn!(
                    dropped,
                    "agent-state hook dispatcher lagged during shutdown; re-reading current activities"
                );
                for snapshot in registry.agent_state_hook_snapshots().await {
                    queue_agent_state_hook(
                        last_fired,
                        pending,
                        snapshot.session_id,
                        snapshot.activity,
                    );
                }
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break;
            }
        }
    }
}

fn parse_agent_state_event(event: &Event) -> Option<(SessionId, AgentActivity)> {
    if event.event != event::AGENT_STATE {
        return None;
    }
    let payload = event.payload.as_object()?;
    let session_id = payload.get("session_id")?.as_str()?;
    let activity_value = payload.get("activity")?;
    let activity = match parse_agent_activity(activity_value) {
        Ok(activity) => activity,
        Err(err) => {
            warn!(
                session_id,
                activity = ?activity_value,
                error = %err,
                "failed to parse agent-state activity; hook not fired"
            );
            return None;
        }
    };
    Some((SessionId(session_id.to_owned()), activity))
}

pub(super) fn parse_agent_activity(value: &Value) -> Result<AgentActivity, serde_json::Error> {
    serde_json::from_value(value.clone())
}

fn queue_agent_state_hook(
    last_fired: &mut HashMap<SessionId, AgentActivity>,
    pending: &mut HashMap<SessionId, (AgentActivity, tokio::time::Instant)>,
    session_id: SessionId,
    activity: AgentActivity,
) {
    if last_fired.get(&session_id) == Some(&activity) {
        pending.remove(&session_id);
        return;
    }
    pending.insert(
        session_id,
        (
            activity,
            tokio::time::Instant::now() + AGENT_STATE_HOOK_DEBOUNCE,
        ),
    );
}

async fn flush_pending_agent_state_hooks(
    registry: &SessionRegistry,
    last_fired: &mut HashMap<SessionId, AgentActivity>,
    pending: &mut HashMap<SessionId, (AgentActivity, tokio::time::Instant)>,
    in_flight: &mut HashSet<SessionId>,
    done_tx: &mpsc::UnboundedSender<SessionId>,
    flush_all: bool,
) {
    let now = tokio::time::Instant::now();
    let due: Vec<SessionId> = pending
        .iter()
        .filter(|&(_session_id, (_, deadline))| flush_all || *deadline <= now)
        .map(|(session_id, (_, _deadline))| session_id.clone())
        .collect();

    for session_id in due {
        if in_flight.contains(&session_id) {
            continue;
        }
        let Some((pending_activity, _)) = pending.remove(&session_id) else {
            continue;
        };
        let Some(snapshot) = registry.agent_state_hook_snapshot(&session_id).await else {
            last_fired.remove(&session_id);
            continue;
        };
        if snapshot.activity != pending_activity {
            queue_agent_state_hook(last_fired, pending, session_id, snapshot.activity);
            continue;
        }
        if last_fired.get(&snapshot.session_id) == Some(&snapshot.activity) {
            continue;
        }
        fire_agent_state_hook(registry, snapshot.clone(), in_flight, done_tx);
        last_fired.insert(snapshot.session_id, snapshot.activity);
    }
}

fn fire_agent_state_hook(
    registry: &SessionRegistry,
    snapshot: AgentStateHookSnapshot,
    in_flight: &mut HashSet<SessionId>,
    done_tx: &mpsc::UnboundedSender<SessionId>,
) {
    let session_id = snapshot.session_id.clone();
    in_flight.insert(session_id.clone());
    let handle = registry.spawn_session_hook_task(SessionHookRequest {
        event: HookEvent::AgentState,
        cwd: snapshot.cwd,
        session_id: snapshot.session_id.0,
        project_id: snapshot.project_id,
        agent: snapshot.agent,
        stop_reason: None,
        activity: Some(agent_activity_env(snapshot.activity)),
    });
    let done_tx = done_tx.clone();
    tokio::spawn(async move {
        let _ = handle.await;
        let _ = done_tx.send(session_id);
    });
}

fn agent_activity_env(activity: AgentActivity) -> &'static str {
    match activity {
        AgentActivity::Working => "working",
        AgentActivity::Blocked => "blocked",
        AgentActivity::Idle => "idle",
    }
}
