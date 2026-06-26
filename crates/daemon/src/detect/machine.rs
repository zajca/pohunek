use std::time::{Duration, Instant};

use protocol::{AgentActivity, StateSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityEvidence {
    pub activity: AgentActivity,
    pub source: StateSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityTransition {
    pub activity: AgentActivity,
    pub source: StateSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectionConfig {
    pub recheck_after: Duration,
    pub confirmations: usize,
    pub cap: Duration,
    pub stable_visible_refresh: Duration,
    pub startup_grace: Duration,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            recheck_after: Duration::from_millis(100),
            confirmations: 3,
            cap: Duration::from_millis(700),
            stable_visible_refresh: Duration::from_millis(800),
            startup_grace: Duration::from_secs(3),
        }
    }
}

impl DetectionConfig {
    /// Returns a config safe for state-machine use.
    ///
    /// Zero-duration windows are valid and keep their immediate behavior.
    /// Confirmation counts are clamped to at least one so an observed candidate
    /// always represents a real piece of evidence.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.confirmations = self.confirmations.max(1);
        self
    }
}

#[derive(Debug)]
pub struct StateMachine {
    started_at: Instant,
    config: DetectionConfig,
    published: Option<PublishedState>,
    last_emitted: Option<ActivityTransition>,
    pending: Option<PendingCandidate>,
}

impl StateMachine {
    /// Creates a state machine starting at `started_at`.
    ///
    /// Callers should pass the same monotonic clock domain to every later
    /// observation and timer call. The config is normalized before use.
    #[must_use]
    pub fn new(started_at: Instant, config: DetectionConfig) -> Self {
        Self {
            started_at,
            config: config.normalized(),
            published: None,
            last_emitted: None,
            pending: None,
        }
    }

    /// Records process or PTY bytes flowing at `now`.
    ///
    /// Bytes are a strong working signal: they cancel any pending idle/blocked
    /// candidate and publish `Working/Process` when the visible activity was not
    /// already working. Repeated working observations do not re-emit solely to
    /// change the source.
    pub fn observe_bytes(&mut self, now: Instant) -> Option<ActivityTransition> {
        self.pending = None;
        self.observe_working(
            now,
            ActivityTransition {
                activity: AgentActivity::Working,
                source: StateSource::Process,
            },
        )
    }

    /// Records parsed OSC, screen, manifest, or process evidence at `now`.
    ///
    /// Working evidence cancels pending idle/blocked candidates and can publish
    /// immediately. Idle and blocked evidence are debounced; a transition is
    /// emitted only from this method when the same evidence is observed again
    /// after the configured confirmation or cap/startup-grace constraints.
    pub fn observe_evidence(
        &mut self,
        now: Instant,
        evidence: ActivityEvidence,
    ) -> Option<ActivityTransition> {
        match evidence.activity {
            AgentActivity::Working => {
                self.pending = None;
                self.observe_working(now, evidence.into())
            }
            AgentActivity::Blocked | AgentActivity::Idle => self.observe_debounced(now, evidence),
        }
    }

    /// Advances timer-driven behavior when no new evidence is available.
    ///
    /// `tick` may re-emit an already published visible non-process state after
    /// `stable_visible_refresh`. It never confirms or publishes pending idle or
    /// blocked candidates; those require a fresh `observe_evidence` call.
    pub fn tick(&mut self, now: Instant) -> Option<ActivityTransition> {
        let published = self.published?;
        if self.refresh_due(now, published) {
            return Some(self.publish(now, published.transition));
        }

        None
    }

    /// Drops any candidate that is waiting for debounce confirmation.
    pub fn clear_pending(&mut self) {
        self.pending = None;
    }

    fn observe_working(
        &mut self,
        now: Instant,
        transition: ActivityTransition,
    ) -> Option<ActivityTransition> {
        if let Some(published) = self.published {
            if published.transition.activity == AgentActivity::Working {
                if published.transition == transition && self.refresh_due(now, published) {
                    return Some(self.publish(now, transition));
                }

                if published.transition.source == StateSource::Process
                    && transition.source != StateSource::Process
                    && !self.last_emitted_is_visible_working()
                {
                    return Some(self.publish(now, transition));
                }

                if transition.source == StateSource::Process {
                    self.record_published(now, transition);
                }

                return None;
            }
        }

        Some(self.publish(now, transition))
    }

    fn observe_debounced(
        &mut self,
        now: Instant,
        evidence: ActivityEvidence,
    ) -> Option<ActivityTransition> {
        if self
            .published
            .is_some_and(|published| published.transition == evidence.into())
        {
            self.pending = None;
            return self.publish_changed_or_refreshed(now, evidence.into());
        }

        match self.pending.as_mut() {
            Some(pending) if pending.evidence == evidence => {
                pending.note_observed(now, &self.config);
            }
            _ => {
                self.pending = Some(PendingCandidate::new(now, evidence));
            }
        }

        self.publish_pending_if_ready(now)
    }

    fn publish_pending_if_ready(&mut self, now: Instant) -> Option<ActivityTransition> {
        let pending = self.pending?;
        if !self.pending_ready(now, pending) {
            return None;
        }

        self.pending = None;
        Some(self.publish(now, pending.evidence.into()))
    }

    fn pending_ready(&self, now: Instant, pending: PendingCandidate) -> bool {
        self.startup_grace_elapsed(now)
            && (pending.confirmations >= self.config.confirmations
                || now.saturating_duration_since(pending.first_seen_at) >= self.config.cap)
    }

    fn startup_grace_elapsed(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) >= self.config.startup_grace
    }

    fn publish_changed_or_refreshed(
        &mut self,
        now: Instant,
        transition: ActivityTransition,
    ) -> Option<ActivityTransition> {
        if let Some(published) = self.published {
            if published.transition == transition {
                return self
                    .refresh_due(now, published)
                    .then(|| self.publish(now, transition));
            }
        }

        Some(self.publish(now, transition))
    }

    fn refresh_due(&self, now: Instant, published: PublishedState) -> bool {
        is_visible_source(published.transition.source)
            && now.saturating_duration_since(published.emitted_at)
                >= self.config.stable_visible_refresh
    }

    fn publish(&mut self, now: Instant, transition: ActivityTransition) -> ActivityTransition {
        self.last_emitted = Some(transition);
        self.record_published(now, transition);
        transition
    }

    fn record_published(&mut self, now: Instant, transition: ActivityTransition) {
        self.published = Some(PublishedState {
            transition,
            emitted_at: now,
        });
    }

    fn last_emitted_is_visible_working(&self) -> bool {
        self.last_emitted.is_some_and(|transition| {
            transition.activity == AgentActivity::Working && is_visible_source(transition.source)
        })
    }
}

fn is_visible_source(source: StateSource) -> bool {
    matches!(
        source,
        StateSource::OscTitle | StateSource::OscProgress | StateSource::Screen
    )
}

impl From<ActivityEvidence> for ActivityTransition {
    fn from(evidence: ActivityEvidence) -> Self {
        Self {
            activity: evidence.activity,
            source: evidence.source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PublishedState {
    transition: ActivityTransition,
    emitted_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingCandidate {
    evidence: ActivityEvidence,
    first_seen_at: Instant,
    last_counted_at: Instant,
    confirmations: usize,
}

impl PendingCandidate {
    fn new(now: Instant, evidence: ActivityEvidence) -> Self {
        Self {
            evidence,
            first_seen_at: now,
            last_counted_at: now,
            confirmations: 1,
        }
    }

    fn note_observed(&mut self, now: Instant, config: &DetectionConfig) {
        if now.saturating_duration_since(self.last_counted_at) >= config.recheck_after {
            self.last_counted_at = now;
            self.confirmations += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use protocol::{AgentActivity, StateSource};

    use super::{ActivityEvidence, ActivityTransition, DetectionConfig, StateMachine};

    fn config() -> DetectionConfig {
        DetectionConfig {
            recheck_after: Duration::from_millis(100),
            confirmations: 3,
            cap: Duration::from_millis(700),
            stable_visible_refresh: Duration::from_millis(800),
            startup_grace: Duration::ZERO,
        }
    }

    fn transition(activity: AgentActivity, source: StateSource) -> ActivityTransition {
        ActivityTransition { activity, source }
    }

    fn evidence(activity: AgentActivity, source: StateSource) -> ActivityEvidence {
        ActivityEvidence { activity, source }
    }

    #[test]
    fn bytes_flowing_immediately_publishes_working_process() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());

        assert_eq!(
            machine.observe_bytes(started_at),
            Some(transition(AgentActivity::Working, StateSource::Process))
        );
    }

    #[test]
    fn duplicate_working_process_bytes_do_not_reemit() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());

        assert_eq!(
            machine.observe_bytes(started_at),
            Some(transition(AgentActivity::Working, StateSource::Process))
        );
        assert_eq!(
            machine.observe_bytes(started_at + Duration::from_millis(1)),
            None
        );
    }

    #[test]
    fn blocked_evidence_during_startup_grace_is_suppressed_until_grace_and_confirmations() {
        let started_at = Instant::now();
        let mut startup_config = config();
        startup_config.startup_grace = Duration::from_secs(3);
        let mut machine = StateMachine::new(started_at, startup_config);
        let blocked = evidence(AgentActivity::Blocked, StateSource::Screen);

        assert_eq!(machine.observe_evidence(started_at, blocked), None);
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(100), blocked),
            None
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(200), blocked),
            None
        );
        assert_eq!(
            machine.tick(
                (started_at + Duration::from_secs(3))
                    .checked_sub(Duration::from_millis(1))
                    .unwrap()
            ),
            None
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_secs(3), blocked),
            Some(transition(AgentActivity::Blocked, StateSource::Screen))
        );
    }

    #[test]
    fn pending_evidence_before_startup_grace_does_not_publish_on_tick_without_fresh_evidence() {
        let started_at = Instant::now();
        let mut startup_config = config();
        startup_config.startup_grace = Duration::from_secs(3);
        let mut machine = StateMachine::new(started_at, startup_config);
        let blocked = evidence(AgentActivity::Blocked, StateSource::Screen);

        assert_eq!(machine.observe_evidence(started_at, blocked), None);
        assert_eq!(
            machine.tick(started_at + Duration::from_secs(3) + Duration::from_millis(700)),
            None
        );
    }

    #[test]
    fn idle_and_blocked_evidence_need_three_confirmations_spaced_by_recheck_interval() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());
        let idle = evidence(AgentActivity::Idle, StateSource::OscTitle);

        assert_eq!(machine.observe_evidence(started_at, idle), None);
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(99), idle),
            None
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(100), idle),
            None
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(199), idle),
            None
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(200), idle),
            Some(transition(AgentActivity::Idle, StateSource::OscTitle))
        );
    }

    #[test]
    fn flicker_working_idle_working_inside_window_collapses_to_single_stable_working_transition() {
        let started_at = Instant::now();
        let mut flicker_config = config();
        flicker_config.stable_visible_refresh = Duration::from_secs(10);
        let mut machine = StateMachine::new(started_at, flicker_config);
        let working = evidence(AgentActivity::Working, StateSource::Screen);
        let idle = evidence(AgentActivity::Idle, StateSource::Screen);

        assert_eq!(
            machine.observe_evidence(started_at, working),
            Some(transition(AgentActivity::Working, StateSource::Screen))
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(50), idle),
            None
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(75), working),
            None
        );
        assert_eq!(machine.tick(started_at + Duration::from_millis(700)), None);
    }

    #[test]
    fn cap_allows_stable_pending_candidate_to_publish_even_when_confirmations_are_short() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());
        let blocked = evidence(AgentActivity::Blocked, StateSource::Screen);

        assert_eq!(machine.observe_evidence(started_at, blocked), None);
        assert_eq!(machine.tick(started_at + Duration::from_millis(699)), None);
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(700), blocked),
            Some(transition(AgentActivity::Blocked, StateSource::Screen))
        );
    }

    #[test]
    fn working_process_then_working_visible_source_upgrades_once_without_visible_source_flapping() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());

        assert_eq!(
            machine.observe_bytes(started_at),
            Some(transition(AgentActivity::Working, StateSource::Process))
        );
        assert_eq!(
            machine.observe_evidence(
                started_at + Duration::from_millis(100),
                evidence(AgentActivity::Working, StateSource::Screen)
            ),
            Some(transition(AgentActivity::Working, StateSource::Screen))
        );
        assert_eq!(
            machine.observe_evidence(
                started_at + Duration::from_millis(200),
                evidence(AgentActivity::Working, StateSource::OscProgress)
            ),
            None
        );
    }

    #[test]
    fn process_bytes_after_visible_working_do_not_enable_second_visible_working_upgrade() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());

        assert_eq!(
            machine.observe_bytes(started_at),
            Some(transition(AgentActivity::Working, StateSource::Process))
        );
        assert_eq!(
            machine.observe_evidence(
                started_at + Duration::from_millis(100),
                evidence(AgentActivity::Working, StateSource::Screen)
            ),
            Some(transition(AgentActivity::Working, StateSource::Screen))
        );
        assert_eq!(
            machine.observe_bytes(started_at + Duration::from_millis(200)),
            None
        );
        assert_eq!(
            machine.observe_evidence(
                started_at + Duration::from_millis(300),
                evidence(AgentActivity::Working, StateSource::OscProgress)
            ),
            None
        );
    }

    #[test]
    fn tick_does_not_refresh_working_process_after_stable_visible_refresh_interval() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());

        assert_eq!(
            machine.observe_bytes(started_at),
            Some(transition(AgentActivity::Working, StateSource::Process))
        );
        assert_eq!(machine.tick(started_at + Duration::from_millis(800)), None);
    }

    #[test]
    fn visible_state_refreshes_after_interval_even_when_unrelated_pending_candidate_exists() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());
        let blocked = evidence(AgentActivity::Blocked, StateSource::Screen);
        let idle = evidence(AgentActivity::Idle, StateSource::Screen);

        assert_eq!(machine.observe_evidence(started_at, blocked), None);
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(100), blocked),
            None
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(200), blocked),
            Some(transition(AgentActivity::Blocked, StateSource::Screen))
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(300), idle),
            None
        );
        assert_eq!(
            machine.tick(started_at + Duration::from_secs(1)),
            Some(transition(AgentActivity::Blocked, StateSource::Screen))
        );
    }

    #[test]
    fn bytes_after_visible_working_update_internal_state_to_process_without_later_refresh() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());
        let working = evidence(AgentActivity::Working, StateSource::Screen);

        assert_eq!(
            machine.observe_evidence(started_at, working),
            Some(transition(AgentActivity::Working, StateSource::Screen))
        );
        assert_eq!(
            machine.observe_bytes(started_at + Duration::from_millis(100)),
            None
        );
        assert_eq!(machine.tick(started_at + Duration::from_millis(900)), None);
    }

    #[test]
    fn stable_visible_refresh_reemits_same_visible_non_process_state_after_interval() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());
        let working = evidence(AgentActivity::Working, StateSource::Screen);

        assert_eq!(
            machine.observe_evidence(started_at, working),
            Some(transition(AgentActivity::Working, StateSource::Screen))
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(799), working),
            None
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(800), working),
            Some(transition(AgentActivity::Working, StateSource::Screen))
        );
    }

    #[test]
    fn zero_confirmations_normalizes_to_one_confirmation() {
        let started_at = Instant::now();
        let mut zero_config = config();
        zero_config.confirmations = 0;
        let mut machine = StateMachine::new(started_at, zero_config);
        let idle = evidence(AgentActivity::Idle, StateSource::Screen);

        assert_eq!(
            machine.observe_evidence(started_at, idle),
            Some(transition(AgentActivity::Idle, StateSource::Screen))
        );
    }

    #[test]
    fn normalized_config_clamps_zero_confirmations_to_one() {
        let mut zero_config = config();
        zero_config.confirmations = 0;

        assert_eq!(zero_config.normalized().confirmations, 1);
    }

    #[test]
    fn bytes_cancel_pending_idle_or_blocked_before_cap() {
        let started_at = Instant::now();
        let mut machine = StateMachine::new(started_at, config());
        let idle = evidence(AgentActivity::Idle, StateSource::Screen);

        assert_eq!(machine.observe_evidence(started_at, idle), None);
        assert_eq!(
            machine.observe_bytes(started_at + Duration::from_millis(200)),
            Some(transition(AgentActivity::Working, StateSource::Process))
        );
        assert_eq!(
            machine.observe_evidence(started_at + Duration::from_millis(700), idle),
            None
        );
    }
}
