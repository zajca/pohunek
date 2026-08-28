//! Per-session process watcher and active-agent reconciliation.

// Rust guideline compliant 2026-08-28

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
type ObservedIdentity = (Pid, u64);

/// Result of one terminal-foreground probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundProbe {
    /// The probe succeeded, including the no-controlling-terminal case.
    Observed(Option<Pid>),
    /// The OS inspection failed and the previous observation must remain valid.
    TransientError,
}

enum ForegroundDecision {
    /// No foreground identity is available; use descendant reconciliation.
    Fallback,
    /// Foreground identity is authoritative and current state remains valid.
    Preserve,
    /// Foreground identity changed the externally visible session state.
    Updated,
}

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

    fn probe_foreground_group(&self, id: &SessionId, root_pid: Pid) -> ForegroundProbe {
        match self.inner.inspector.foreground_process_group(root_pid) {
            Ok(foreground) => ForegroundProbe::Observed(foreground),
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    root_pid,
                    error = %err,
                    "failed to inspect foreground process group"
                );
                ForegroundProbe::TransientError
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
        // Procfs inspection is synchronous. Keep it outside the global session
        // mutex so a slow or permission-gated probe cannot stall unrelated RPCs.
        let foreground_probe = self.probe_foreground_group(id, root_pid);
        let mut exit_watches = match observed_refresh.as_ref() {
            Some(observed) => {
                let existing = self.existing_observed_identities(id).await;
                self.open_exit_watches(id, observed.values(), &existing)
            }
            None => HashMap::new(),
        };

        let (to_spawn, updated, focus_pid, foreground_group, foreground_changed) = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, "procwatch rescan for unknown session");
                return;
            };
            if entry.stopping || is_terminal(entry.info.state) {
                return;
            }

            let to_spawn = apply_observed_refresh(entry, observed_refresh, &mut exit_watches);

            let foreground_changed = apply_foreground_probe(entry, foreground_probe);
            let decision = self.reconcile_foreground_agent(entry, root_pid, now, foreground_probe);
            let updated = match decision {
                ForegroundDecision::Fallback => self.reconcile_active_agent(entry, now),
                ForegroundDecision::Preserve => None,
                ForegroundDecision::Updated => Some(entry.info.clone()),
            };
            let focus_pid = entry
                .active_agent
                .as_ref()
                .and_then(|active| active.pid)
                .unwrap_or(root_pid);
            (
                to_spawn,
                updated,
                focus_pid,
                entry.foreground_process_group,
                foreground_changed,
            )
        };

        for (identity, watch, cancel) in to_spawn {
            self.spawn_observed_exit_watch(id.clone(), identity, watch, cancel);
        }
        if foreground_changed || updated.is_some() {
            debug!(
                session_id = %id.0,
                root_pid,
                focus_pid,
                foreground_pgid = foreground_group,
                "session process focus reconciled"
            );
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

    pub(super) async fn on_observed_agent_exit(&self, id: &SessionId, identity: ObservedIdentity) {
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, pid = identity.0, "procwatch exit for unknown session");
                return;
            };
            if entry.stopping || is_terminal(entry.info.state) {
                return;
            }

            let before = entry.observed_agents.len();
            entry
                .observed_agents
                .retain(|observed| (observed.pid, observed.start_identity) != identity);
            if before == entry.observed_agents.len() {
                return;
            }

            match self.reconcile_foreground_agent(
                entry,
                entry.info.pid,
                Instant::now(),
                ForegroundProbe::Observed(entry.foreground_process_group),
            ) {
                ForegroundDecision::Fallback => self.reconcile_active_agent(entry, Instant::now()),
                ForegroundDecision::Preserve => None,
                ForegroundDecision::Updated => Some(entry.info.clone()),
            }
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
                        pgid: fact.pgid,
                        start_identity: fact.start_identity,
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

    async fn existing_observed_identities(&self, id: &SessionId) -> HashSet<ObservedIdentity> {
        let sessions = self.inner.sessions.lock().await;
        sessions
            .get(id)
            .map(|entry| {
                entry
                    .observed_agents
                    .iter()
                    .map(|observed| (observed.pid, observed.start_identity))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn open_exit_watches<'a>(
        &self,
        id: &SessionId,
        observed: impl Iterator<Item = &'a ObservedAgent>,
        existing: &HashSet<ObservedIdentity>,
    ) -> HashMap<ObservedIdentity, ExitWatch> {
        let mut watches = HashMap::new();
        for observation in observed {
            let identity = (observation.pid, observation.start_identity);
            if existing.contains(&identity) {
                continue;
            }
            match self.inner.inspector.exit_watch(observation.pid) {
                Ok(watch) => {
                    watches.insert(identity, watch);
                }
                Err(err) => {
                    debug!(
                        session_id = %id.0,
                        pid = observation.pid,
                        process_start_identity = observation.start_identity,
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
        identity: ObservedIdentity,
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
                            pid = identity.0,
                            process_start_identity = identity.1,
                            error = %err,
                            "process exit watch failed"
                        );
                        return;
                    }
                    registry.on_observed_agent_exit(&id, identity).await;
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
            if observed_process_matches(entry, pid, active.start_identity, active_base) {
                return None;
            }
            if let Some(observed) = first_observed_agent_for_base(entry, active_base) {
                return self.auto_report_observed_agent(entry, &observed, now);
            }
            if let Some(observed) = choose_observed_agent(entry) {
                return self.auto_report_observed_agent(entry, &observed, now);
            }
            return Some(clear_active_agent(
                entry,
                self.procwatch_tombstone_for(&active, now),
            ));
        }

        if let Some(observed) = single_observed_for_base(entry, active_base) {
            if let Some(active) = entry.active_agent.as_mut() {
                active.pid = Some(observed.pid);
                active.start_identity = Some(observed.start_identity);
            }
            entry.info.active_agent_pid = Some(observed.pid);
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
            start_identity: Some(observed.start_identity),
            reported_at: now,
            activity_reported: false,
        };
        Some(apply_observed_transition(entry, observed, report, agent))
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
            start_identity: None,
            reported_at: now,
            activity_reported: false,
        }
    }

    fn reconcile_foreground_agent(
        &self,
        entry: &mut SessionEntry,
        root_pid: Pid,
        now: Instant,
        probe: ForegroundProbe,
    ) -> ForegroundDecision {
        let ForegroundProbe::Observed(Some(foreground_group)) = probe else {
            return match probe {
                ForegroundProbe::Observed(None) => ForegroundDecision::Fallback,
                ForegroundProbe::TransientError => ForegroundDecision::Preserve,
                ForegroundProbe::Observed(Some(_)) => {
                    unreachable!("foreground group was matched by the enclosing pattern")
                }
            };
        };

        if foreground_group == root_pid {
            let Some(active) = entry.active_agent.clone() else {
                return ForegroundDecision::Preserve;
            };
            let active_is_launch_root = active.pid == Some(root_pid)
                && entry.info.agent_base != AgentKind::Shell
                && entry.info.active_agent_base.as_ref() == Some(&entry.info.agent_base);
            if active_is_launch_root {
                return ForegroundDecision::Preserve;
            }
            let _ = clear_active_agent(entry, self.procwatch_tombstone_for(&active, now));
            return ForegroundDecision::Updated;
        }

        if let Some(observed) = choose_foreground_observed(entry, foreground_group) {
            let active_matches = entry.active_agent.as_ref().is_some_and(|active| {
                active.pid == Some(observed.pid)
                    && active.start_identity == Some(observed.start_identity)
                    && entry.info.active_agent_base.as_ref() == Some(&observed.agent_base)
            });
            if active_matches {
                return ForegroundDecision::Preserve;
            }
            let agent = agent_kind_label(&observed.agent_base).to_owned();
            let report = ActiveAgentReport {
                source: PROCWATCH_SOURCE.to_owned(),
                agent: agent.clone(),
                seq: Some(self.next_procwatch_seq()),
                pid: Some(observed.pid),
                start_identity: Some(observed.start_identity),
                reported_at: now,
                activity_reported: false,
            };
            let _ = apply_observed_transition(entry, &observed, report, agent);
            return ForegroundDecision::Updated;
        }

        let Some(active) = entry.active_agent.clone() else {
            return ForegroundDecision::Preserve;
        };
        let active_is_live = active.pid == Some(entry.info.pid)
            || entry
                .info
                .active_agent_base
                .as_ref()
                .is_some_and(|active_base| {
                    active.pid.is_some_and(|pid| {
                        observed_process_matches(entry, pid, active.start_identity, active_base)
                    })
                });
        let unbound_is_current = active.pid.is_none()
            && now.saturating_duration_since(active.reported_at)
                < self.inner.config.active_agent_claim_ttl;
        if active_is_live || unbound_is_current {
            ForegroundDecision::Preserve
        } else {
            let _ = clear_active_agent(entry, self.procwatch_tombstone_for(&active, now));
            ForegroundDecision::Updated
        }
    }

    fn next_procwatch_seq(&self) -> u64 {
        self.inner
            .procwatch_seq
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }
}

fn apply_observed_refresh(
    entry: &mut SessionEntry,
    observed_refresh: Option<HashMap<Pid, ObservedAgent>>,
    exit_watches: &mut HashMap<ObservedIdentity, ExitWatch>,
) -> Vec<(ObservedIdentity, ExitWatch, CancellationToken)> {
    let Some(observed) = observed_refresh else {
        return Vec::new();
    };
    let observed_pids = observed.keys().copied().collect::<HashSet<_>>();
    entry
        .observed_agents
        .retain(|agent| observed_pids.contains(&agent.pid));

    let mut to_spawn = Vec::new();
    for observation in observed.into_values() {
        if let Some(existing) = entry
            .observed_agents
            .iter_mut()
            .find(|agent| agent.pid == observation.pid)
        {
            let identity_changed = existing.start_identity != observation.start_identity;
            let base_changed = existing.agent_base != observation.agent_base;
            existing.pgid = observation.pgid;
            existing.start_identity = observation.start_identity;
            existing.cwd = observation.cwd;
            existing.agent_base = observation.agent_base;
            if identity_changed || base_changed {
                existing.first_seen = observation.first_seen;
            }
            continue;
        }

        let identity = (observation.pid, observation.start_identity);
        entry.observed_agents.push(observation);
        if let Some(watch) = exit_watches.remove(&identity) {
            to_spawn.push((identity, watch, entry.procwatch_cancel.clone()));
        }
    }
    to_spawn
}

fn choose_observed_agent(entry: &SessionEntry) -> Option<ObservedAgent> {
    entry
        .observed_agents
        .iter()
        .min_by_key(|observed| (observed.first_seen, observed.pid))
        .cloned()
}

fn apply_foreground_probe(entry: &mut SessionEntry, probe: ForegroundProbe) -> bool {
    let ForegroundProbe::Observed(foreground_group) = probe else {
        return false;
    };
    if entry.foreground_process_group == foreground_group {
        return false;
    }
    entry.foreground_process_group = foreground_group;
    true
}

fn choose_foreground_observed(
    entry: &SessionEntry,
    foreground_group: Pid,
) -> Option<ObservedAgent> {
    entry
        .observed_agents
        .iter()
        .find(|observed| observed.pid == foreground_group)
        .cloned()
        .or_else(|| {
            entry
                .observed_agents
                .iter()
                .filter(|observed| observed.pgid == foreground_group)
                .min_by_key(|observed| (observed.first_seen, observed.pid))
                .cloned()
        })
}

fn apply_observed_transition(
    entry: &mut SessionEntry,
    observed: &ObservedAgent,
    report: ActiveAgentReport,
    agent: String,
) -> SessionInfo {
    let report_activity_was_current = entry
        .active_agent
        .as_ref()
        .is_some_and(|active| active.activity_reported);
    entry.active_agent = Some(report.clone());
    entry.last_agent_report = Some(report);
    entry.info.active_agent = Some(agent);
    entry.info.active_agent_base = Some(observed.agent_base.clone());
    entry.info.active_agent_pid = Some(observed.pid);
    entry.info.active_agent_session_id = None;
    entry.info.active_agent_session_path = None;
    if report_activity_was_current {
        entry.info.activity = None;
        entry.info.state_source = protocol::StateSource::Process;
    }
    let _ = entry
        .detector_config
        .send(DetectorConfig::for_agent(&observed.agent_base));
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
        .min_by_key(|observed| (observed.first_seen, observed.pid))
        .cloned()
}

fn single_observed_for_base(entry: &SessionEntry, agent_base: &AgentKind) -> Option<ObservedAgent> {
    let mut matching = entry
        .observed_agents
        .iter()
        .filter(|observed| &observed.agent_base == agent_base);
    let first = matching.next()?;
    matching.next().is_none().then(|| first.clone())
}

fn observed_process_matches(
    entry: &SessionEntry,
    pid: Pid,
    start_identity: Option<u64>,
    agent_base: &AgentKind,
) -> bool {
    entry.observed_agents.iter().any(|observed| {
        observed.pid == pid
            && &observed.agent_base == agent_base
            && start_identity.is_none_or(|identity| observed.start_identity == identity)
    })
}
