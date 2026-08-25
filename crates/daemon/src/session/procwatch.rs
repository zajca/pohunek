//! Per-session process watcher and active-agent reconciliation.

// Rust guideline compliant 2026-07-07

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use protocol::{event, AgentKind, CwdSource, SessionInfo};
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use crate::detect::{identify_agent, DetectorConfig};
use crate::procwatch::{ExitWatch, Pid, ProcessFact};

use super::{
    agent_kind_label, clear_active_agent, is_terminal, report_is_current, timestamp_now,
    ActiveAgentReport, CancellationToken, Notify, ObservedAgent, Ordering, SessionEntry, SessionId,
    SessionRegistry,
};

const PROCWATCH_SOURCE: &str = "pohunek:procwatch";
/// Last observed foreground group for one reconciliation pass.
type ForegroundGroups = HashMap<SessionId, Option<Pid>>;

impl SessionRegistry {
    pub(super) fn spawn_procwatch(
        &self,
        id: SessionId,
        root_pid: Pid,
        cancel: CancellationToken,
        rescan: std::sync::Arc<Notify>,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(registry.inner.config.procwatch_poll);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        registry.rescan_procwatch_at(&id, root_pid, Instant::now()).await;
                    }
                    () = rescan.notified() => {
                        registry.rescan_procwatch_at(&id, root_pid, Instant::now()).await;
                    }
                }
            }
        });
    }

    fn probe_foreground_group(
        &self,
        id: &SessionId,
        root_pid: Pid,
        cached: Option<Pid>,
    ) -> Option<Pid> {
        match self.inner.inspector.foreground_process_group(root_pid) {
            Ok(foreground) => {
                if cached != foreground {
                    debug!(
                        session_id = %id.0,
                        root_pid,
                        foreground_pgid = foreground,
                        "session foreground process group changed"
                    );
                }
                foreground
            }
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    root_pid,
                    error = %err,
                    "failed to inspect foreground process group"
                );
                None
            }
        }
    }

    pub(super) async fn rescan_procwatch_at(&self, id: &SessionId, root_pid: Pid, now: Instant) {
        let observed_refresh = match self.inner.inspector.descendants(root_pid) {
            Ok(facts) => Some(self.observed_agents_from_facts(id, facts, now)),
            Err(err) => {
                warn!(
                    session_id = %id.0,
                    root_pid,
                    error = %err,
                    "failed to inspect session process descendants"
                );
                None
            }
        };
        let mut exit_watches = match observed_refresh.as_ref() {
            Some(observed) => {
                let existing_pids = self.existing_observed_pids(id).await;
                self.open_exit_watches(id, observed.keys(), &existing_pids)
            }
            None => HashMap::new(),
        };

        let cached_foreground_groups = ForegroundGroups::default();
        let (to_spawn, updated, focus_pid) = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, "procwatch rescan for unknown session");
                return;
            };
            if entry.stopping || is_terminal(entry.info.state) {
                return;
            }

            let mut to_spawn = Vec::new();
            if let Some(observed) = observed_refresh {
                let observed_pids = observed.keys().copied().collect::<HashSet<_>>();
                entry
                    .observed_agents
                    .retain(|agent| observed_pids.contains(&agent.pid));

                for observation in observed.into_values() {
                    if let Some(existing) = entry
                        .observed_agents
                        .iter_mut()
                        .find(|agent| agent.pid == observation.pid)
                    {
                        let base_changed = existing.agent_base != observation.agent_base;
                        existing.cwd = observation.cwd;
                        existing.agent_base = observation.agent_base;
                        if base_changed {
                            existing.first_seen = observation.first_seen;
                        }
                        continue;
                    }

                    let pid = observation.pid;
                    entry.observed_agents.push(observation);
                    if let Some(watch) = exit_watches.remove(&pid) {
                        to_spawn.push((pid, watch, entry.procwatch_cancel.clone()));
                    }
                }
            }

            let updated = self.reconcile_active_agent(entry, now);
            entry.foreground_process_group = self.probe_foreground_group(
                id,
                root_pid,
                cached_foreground_groups.get(id).copied().flatten(),
            );
            let foreground_update = choose_foreground_agent(entry, root_pid, now);
            let focus_pid = entry
                .active_agent
                .as_ref()
                .and_then(|active| active.pid)
                .unwrap_or(root_pid);
            (to_spawn, foreground_update.or(updated), focus_pid)
        };

        for (pid, watch, cancel) in to_spawn {
            self.spawn_observed_exit_watch(id.clone(), pid, watch, cancel);
        }
        if let Some(info) = updated {
            self.emit(event::SESSION_UPDATED, &info);
        }
        match self.inner.inspector.cwd(focus_pid) {
            Ok(cwd) => self.apply_cwd_change(id, cwd, CwdSource::Procwatch).await,
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    focus_pid,
                    error = %err,
                    "failed to inspect focus process cwd"
                );
            }
        }
    }

    pub(super) async fn on_observed_agent_exit(&self, id: &SessionId, pid: Pid) {
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, pid, "procwatch exit for unknown session");
                return;
            };
            if entry.stopping || is_terminal(entry.info.state) {
                return;
            }

            let before = entry.observed_agents.len();
            entry.observed_agents.retain(|observed| observed.pid != pid);
            if before == entry.observed_agents.len() {
                return;
            }

            self.reconcile_active_agent(entry, Instant::now())
        };

        if let Some(info) = updated {
            self.emit(event::SESSION_UPDATED, &info);
        }
    }

    fn observed_agents_from_facts(
        &self,
        id: &SessionId,
        facts: Vec<ProcessFact>,
        now: Instant,
    ) -> HashMap<Pid, ObservedAgent> {
        facts
            .into_iter()
            .filter_map(|fact| {
                let agent_base = identify_agent(&fact)?;
                if self.is_foreign_owned_agent(id, fact.pid) {
                    return None;
                }
                let cwd = self.inner.inspector.cwd(fact.pid).ok();
                Some((
                    fact.pid,
                    ObservedAgent {
                        pid: fact.pid,
                        agent_base,
                        first_seen: now,
                        cwd,
                    },
                ))
            })
            .collect()
    }

    /// Whether `pid` carries pohunek ownership markers naming a different
    /// daemon instance or session — an agent PTY spawned by a *nested* daemon
    /// (a test-suite loopback daemon, a self-hosted dev run) that lives inside
    /// this session's process subtree. Adopting it would hijack this session's
    /// active agent and cwd (and, transitively, its project association), so it
    /// must never become an observed agent. Missing markers keep the process
    /// eligible: an agent this session launched inherits the session's own
    /// markers, and an env-scrubbed process stays observable as before.
    /// Unreadable markers also keep it eligible — preferring the established
    /// behavior over dropping a legitimate agent on a transient read failure.
    fn is_foreign_owned_agent(&self, id: &SessionId, pid: Pid) -> bool {
        let markers = match self.inner.inspector.ownership_markers(pid) {
            Ok(markers) => markers,
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    pid,
                    error = %err,
                    "failed to read process ownership markers; keeping it observable"
                );
                return false;
            }
        };
        let foreign_daemon = markers
            .daemon_id
            .as_deref()
            .is_some_and(|daemon_id| daemon_id != self.inner.daemon_instance_id);
        let foreign_session = markers
            .session_id
            .as_deref()
            .is_some_and(|session_id| session_id != id.0);
        foreign_daemon || foreign_session
    }

    async fn existing_observed_pids(&self, id: &SessionId) -> HashSet<Pid> {
        let sessions = self.inner.sessions.lock().await;
        sessions
            .get(id)
            .map(|entry| {
                entry
                    .observed_agents
                    .iter()
                    .map(|observed| observed.pid)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn open_exit_watches<'a>(
        &self,
        id: &SessionId,
        pids: impl Iterator<Item = &'a Pid>,
        existing_pids: &HashSet<Pid>,
    ) -> HashMap<Pid, ExitWatch> {
        let mut watches = HashMap::new();
        for &pid in pids {
            if existing_pids.contains(&pid) {
                continue;
            }
            match self.inner.inspector.exit_watch(pid) {
                Ok(watch) => {
                    watches.insert(pid, watch);
                }
                Err(err) => {
                    debug!(
                        session_id = %id.0,
                        pid,
                        error = %err,
                        "failed to arm process exit watch; falling back to poll cleanup"
                    );
                }
            }
        }
        watches
    }

    fn spawn_observed_exit_watch(
        &self,
        id: SessionId,
        pid: Pid,
        watch: ExitWatch,
        cancel: CancellationToken,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                result = watch.wait() => {
                    if let Err(err) = result {
                        debug!(
                            session_id = %id.0,
                            pid,
                            error = %err,
                            "process exit watch failed"
                        );
                        return;
                    }
                    registry.on_observed_agent_exit(&id, pid).await;
                }
            }
        });
    }

    fn reconcile_active_agent(
        &self,
        entry: &mut SessionEntry,
        now: Instant,
    ) -> Option<SessionInfo> {
        let Some(active) = entry.active_agent.clone() else {
            let observed = choose_observed_agent(entry)?;
            return self.auto_report_observed_agent(entry, &observed, now);
        };
        let Some(active_base) = entry.info.active_agent_base.as_ref() else {
            return Some(clear_active_agent(
                entry,
                self.procwatch_tombstone_for(&active, now),
            ));
        };

        if let Some(pid) = active.pid {
            if pid == entry.info.pid {
                // SAFETY/why: descendant scans intentionally exclude the PTY-root
                // process. While the session is running, `record_exit` owns root
                // liveness and clears `active_agent` when that process exits.
                return None;
            }
            if observed_pid_matches(entry, pid, active_base) {
                return None;
            }
            if let Some(observed) = first_observed_agent_for_base(entry, active_base) {
                return self.auto_report_observed_agent(entry, &observed, now);
            }
            return Some(clear_active_agent(
                entry,
                self.procwatch_tombstone_for(&active, now),
            ));
        }

        if let Some(pid) = single_observed_pid_for_base(entry, active_base) {
            if let Some(active) = entry.active_agent.as_mut() {
                active.pid = Some(pid);
            }
            entry.info.active_agent_pid = Some(pid);
            entry.info.updated_at = timestamp_now();
            return Some(entry.info.clone());
        }

        if now.saturating_duration_since(active.reported_at)
            >= self.inner.config.active_agent_claim_ttl
        {
            return Some(clear_active_agent(
                entry,
                self.procwatch_tombstone_for(&active, now),
            ));
        }

        None
    }

    fn auto_report_observed_agent(
        &self,
        entry: &mut SessionEntry,
        observed: &ObservedAgent,
        now: Instant,
    ) -> Option<SessionInfo> {
        let agent = agent_kind_label(&observed.agent_base).to_owned();
        let seq = Some(self.next_procwatch_seq());
        if !report_is_current(
            entry.last_agent_report.as_ref(),
            PROCWATCH_SOURCE,
            &agent,
            seq,
        ) {
            return None;
        }

        let report = ActiveAgentReport {
            source: PROCWATCH_SOURCE.to_owned(),
            agent: agent.clone(),
            seq,
            pid: Some(observed.pid),
            reported_at: now,
            activity_reported: false,
        };
        entry.active_agent = Some(report.clone());
        entry.last_agent_report = Some(report);
        entry.info.active_agent = Some(agent);
        entry.info.active_agent_base = Some(observed.agent_base.clone());
        entry.info.active_agent_pid = Some(observed.pid);
        // Procwatch rebinds only after the previous pid disappears; the new
        // agent's own hook will republish native identity when available.
        entry.info.active_agent_session_id = None;
        entry.info.active_agent_session_path = None;
        let _ = entry
            .detector_config
            .send(DetectorConfig::for_agent(&observed.agent_base));
        entry.info.updated_at = timestamp_now();
        Some(entry.info.clone())
    }

    fn procwatch_tombstone_for(
        &self,
        active: &ActiveAgentReport,
        now: Instant,
    ) -> ActiveAgentReport {
        let seq = self.next_procwatch_seq();
        ActiveAgentReport {
            source: active.source.clone(),
            agent: active.agent.clone(),
            seq: Some(active.seq.map_or(seq, |active_seq| active_seq.max(seq))),
            pid: None,
            reported_at: now,
            activity_reported: false,
        }
    }

    fn next_procwatch_seq(&self) -> u64 {
        self.inner
            .procwatch_seq
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }
}

fn choose_observed_agent(entry: &SessionEntry) -> Option<ObservedAgent> {
    entry
        .observed_agents
        .iter()
        .min_by_key(|observed| observed.first_seen)
        .cloned()
}

fn choose_foreground_agent(
    entry: &mut SessionEntry,
    root_pid: Pid,
    now: Instant,
) -> Option<SessionInfo> {
    let foreground_group = entry.foreground_process_group?;
    if foreground_group == root_pid || foreground_group == entry.info.pid {
        let active = entry.active_agent.clone()?;
        return Some(clear_active_agent(
            entry,
            ActiveAgentReport {
                seq: active.seq.map(|seq| seq.saturating_add(1)),
                ..active
            },
        ));
    }

    let observed = choose_foreground_observed(entry, foreground_group)?;
    if entry.active_agent.as_ref().and_then(|active| active.pid) == Some(observed.pid) {
        return None;
    }
    Some(auto_report_for_foreground(entry, &observed, now))
}

fn choose_foreground_observed(
    entry: &SessionEntry,
    foreground_group: Pid,
) -> Option<ObservedAgent> {
    entry.observed_agents.iter().find_map(|observed| {
        (observed.pid == foreground_group).then(|| {
            first_observed_agent_for_base(entry, &observed.agent_base)
                .unwrap_or_else(|| observed.clone())
        })
    })
}

fn auto_report_for_foreground(
    entry: &mut SessionEntry,
    observed: &ObservedAgent,
    now: Instant,
) -> SessionInfo {
    // Foreground identity is a stronger fact than first-seen ordering. Build the
    // report directly so an unrelated hook source cannot suppress reconciliation.
    let agent = agent_kind_label(&observed.agent_base).to_owned();
    entry.active_agent = Some(ActiveAgentReport {
        source: PROCWATCH_SOURCE.to_owned(),
        agent: agent.clone(),
        seq: Some(
            entry
                .active_agent
                .as_ref()
                .and_then(|active| active.seq)
                .unwrap_or(0)
                .saturating_add(1),
        ),
        pid: Some(observed.pid),
        reported_at: now,
        activity_reported: false,
    });
    entry.last_agent_report = entry.active_agent.clone();
    entry.info.active_agent = Some(agent);
    entry.info.active_agent_base = Some(observed.agent_base.clone());
    entry.info.active_agent_pid = Some(observed.pid);
    entry.info.updated_at = timestamp_now();
    entry.info.clone()
}

fn first_observed_agent_for_base(
    entry: &SessionEntry,
    agent_base: &AgentKind,
) -> Option<ObservedAgent> {
    entry
        .observed_agents
        .iter()
        .filter(|observed| &observed.agent_base == agent_base)
        .min_by_key(|observed| observed.first_seen)
        .cloned()
}

fn single_observed_pid_for_base(entry: &SessionEntry, agent_base: &AgentKind) -> Option<Pid> {
    let mut matching = entry
        .observed_agents
        .iter()
        .filter(|observed| &observed.agent_base == agent_base)
        .map(|observed| observed.pid);
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

fn observed_pid_matches(entry: &SessionEntry, pid: Pid, agent_base: &AgentKind) -> bool {
    entry
        .observed_agents
        .iter()
        .any(|observed| observed.pid == pid && &observed.agent_base == agent_base)
}
