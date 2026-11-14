//! Derived notifications from session events.

// Rust guideline compliant 2026-10-12

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use protocol::{
    event, AgentActivity, BlockedKind, Event, NotificationCreateParams, NotificationKind,
    NotificationSeverity, NotificationSource, RuntimeRef, RuntimeState, SessionId, SessionInfo,
    SessionState, StateSource,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::events::{EventOrigin, ProducedEvent};
use crate::session::{SessionRegistry, TurnEdge, TurnId};
use crate::shutdown::{DaemonShutdown, DriverHandle, Stage};

use super::{
    is_debounced_create, AttentionCoordinator, DeferredNotification, NotificationService,
    ResolveAsOf, ATTENTION_DEDUPE_KEY_PREFIX, TURN_DEDUPE_KEY_PREFIX,
};

/// Provider namespace used for daemon-derived notification records.
const PROJECTOR_PROVIDER: &str = "pohunek";

/// Source-id prefix for daemon-derived notification records.
const PROJECTOR_SOURCE_ID_PREFIX: &str = "projector";

/// Delay before the projector retries the epoch seed of sessions whose inputs
/// wait for it.
///
/// A failed seed is a failed blocking read of the in-memory notification
/// index, so a short fixed delay retries it without a busy loop.
const SEED_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Inputs the projector keeps per session while its epoch seed fails.
///
/// A session produces a handful of inputs per turn, so the bound is reached
/// only by an outage that lasts for many turns. The oldest input is dropped
/// with a warning then, to bound memory.
const MAX_DEFERRED_INPUTS_PER_SESSION: usize = 256;

/// Spawns the task that derives durable notifications from session lifecycle
/// events.
///
/// Derived debounced session notifications are routed through `attention` (the
/// debounce coordinator) rather than persisted directly, and the resume edge
/// resolves session-scoped notification keys through it too. The task is a
/// consumer of `tasks`: it stops after the producers of session events (idle
/// stop, retention), so the events they emit while stopping are projected, and
/// it drains the buffered session events first.
#[expect(
    clippy::must_use_candidate,
    reason = "the handle only stops the task before daemon shutdown does"
)]
pub fn spawn_projector(
    tasks: &DaemonShutdown,
    sessions: &SessionRegistry,
    notifications: NotificationService,
    attention: AttentionCoordinator,
) -> DriverHandle {
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    spawn_projector_task(
        tasks,
        sessions.clone(),
        notifications,
        attention,
        sessions.subscribe_produced(),
        shutdown,
        move || stop.cancel(),
    )
}

fn spawn_projector_task(
    tasks: &DaemonShutdown,
    sessions: SessionRegistry,
    notifications: NotificationService,
    attention: AttentionCoordinator,
    mut events: broadcast::Receiver<ProducedEvent>,
    shutdown: CancellationToken,
    stop: impl FnOnce() + Send + 'static,
) -> DriverHandle {
    tasks.spawn("notification_projector", Stage::Consumer, stop, async move {
        let mut state = ProjectorState::default();
        let mut turn_edges = sessions.turn_events().take_edge_receiver();
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    drain_buffered_events(&sessions, &notifications, &attention, &mut events, &mut turn_edges, &mut state).await;
                    break;
                }
                () = seed_retry_due(state.seed_retry_at) => {
                    state.retry_deferred(&notifications, &attention).await;
                }
                edge = next_turn_edge(&mut turn_edges) => match edge {
                    Some(edge) => {
                        state.handle_turn_edge_blocking(&notifications, &attention, edge).await;
                    }
                    None => turn_edges = None,
                },
                received = events.recv() => match received {
                    Ok(ProducedEvent { event, produced, origin }) => state.handle_produced_event_blocking(&notifications, &attention, &event, produced, origin).await,
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        warn!(
                            dropped,
                            "notification projector lagged; re-reading current session state"
                        );
                        resync_from_registry(&sessions, &notifications, &attention, &mut state).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    })
}

/// Completes when the seed retry is due; pending forever while none waits.
async fn seed_retry_due(retry_at: Option<Instant>) {
    match retry_at {
        Some(retry_at) => tokio::time::sleep_until(retry_at).await,
        None => std::future::pending().await,
    }
}

/// The next turn change, pending forever once the channel is gone.
async fn next_turn_edge(edges: &mut Option<mpsc::UnboundedReceiver<TurnEdge>>) -> Option<TurnEdge> {
    match edges {
        Some(edges) => edges.recv().await,
        None => std::future::pending().await,
    }
}

async fn drain_buffered_events(
    sessions: &SessionRegistry,
    notifications: &NotificationService,
    attention: &AttentionCoordinator,
    events: &mut broadcast::Receiver<ProducedEvent>,
    turn_edges: &mut Option<mpsc::UnboundedReceiver<TurnEdge>>,
    state: &mut ProjectorState,
) {
    while let Some(edge) = turn_edges.as_mut().and_then(|edges| edges.try_recv().ok()) {
        state
            .handle_turn_edge_blocking(notifications, attention, edge)
            .await;
    }
    loop {
        match events.try_recv() {
            Ok(ProducedEvent {
                event,
                produced,
                origin,
            }) => {
                state
                    .handle_produced_event_blocking(
                        notifications,
                        attention,
                        &event,
                        produced,
                        origin,
                    )
                    .await;
            }
            Err(broadcast::error::TryRecvError::Lagged(dropped)) => {
                warn!(
                    dropped,
                    "notification projector lagged during shutdown; re-reading current session state"
                );
                resync_from_registry(sessions, notifications, attention, state).await;
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break;
            }
        }
    }
    state.retry_deferred(notifications, attention).await;
    state.warn_unprojected();
}

async fn resync_from_registry(
    sessions: &SessionRegistry,
    notifications: &NotificationService,
    attention: &AttentionCoordinator,
    state: &mut ProjectorState,
) {
    // Every snapshot stands for the registry as read here, however long the
    // seeds of the sessions before it block.
    let read_at = StdInstant::now();
    for session in sessions.list().await {
        state
            .handle_session_snapshot_blocking(notifications, attention, &session, read_at)
            .await;
    }
}

/// Return the shared session attention dedupe key.
///
/// Scheme: `attention:<session_id>`. The key is source-independent so daemon
/// projectors and provider hooks can refer to the same waiting-for-input
/// condition without sharing producer-specific source ids.
#[must_use]
pub fn attention_dedupe_key(session_id: &SessionId) -> String {
    format!("{ATTENTION_DEDUPE_KEY_PREFIX}:{}", session_id.0)
}

/// Return the shared session turn-completion dedupe key.
///
/// Scheme: `turn:<session_id>`. The key keeps at most one unread completed-turn
/// notification visible for a session while preserving older superseded turns in
/// history.
#[must_use]
pub fn turn_dedupe_key(session_id: &SessionId) -> String {
    format!("{TURN_DEDUPE_KEY_PREFIX}:{}", session_id.0)
}

/// Resolve session notifications for a resumed session via the coordinator.
///
/// The coordinator unifies both paths: it cancels any still-pending (debounced)
/// session notification for this session so it never surfaces, and acknowledges
/// any already-visible record sharing the session's dedupe key.
fn resolve_session_notifications(attention: &AttentionCoordinator, session_id: &SessionId) {
    attention.resolve(attention_dedupe_key(session_id));
    attention.resolve(turn_dedupe_key(session_id));
}

/// Return a deterministic projector source id.
///
/// Scheme: `projector:<session_id>:<kind>:<epoch>`. The epoch increments each
/// time the session enters the projected state for the kind and continues from
/// the highest epoch the store holds for the session and kind, so a restarted
/// daemon never reuses the key of a record from before the restart, which the
/// store would merge the new transition into.
#[must_use]
fn projector_source_id(session_id: &SessionId, kind: NotificationKind, epoch: u64) -> String {
    format!("{}{epoch}", projector_source_id_prefix(session_id, kind))
}

/// The part of a projector source id before its transition epoch.
fn projector_source_id_prefix(session_id: &SessionId, kind: NotificationKind) -> String {
    format!("{}{}:", projector_session_prefix(session_id), kind.as_str())
}

/// The part of a projector source id before its notification kind.
fn projector_session_prefix(session_id: &SessionId) -> String {
    format!("{PROJECTOR_SOURCE_ID_PREFIX}:{}:", session_id.0)
}

/// The notification kind a projector source id names, if the projector
/// derives that kind as a record.
fn projected_kind(name: &str) -> Option<NotificationKind> {
    [
        NotificationKind::AgentBlocked,
        NotificationKind::ApprovalRequired,
    ]
    .into_iter()
    .find(|kind| kind.as_str() == name)
}

/// The session an event the projector derives notifications from names, with
/// the agent activity the projector reads from it.
fn event_subject(event: &Event) -> Option<(SessionId, Option<AgentActivity>)> {
    match event.event() {
        event::AGENT_STATE => parse_projector_payload::<AgentStatePayload>(event)
            .map(|payload| (payload.session_id, Some(payload.activity))),
        event::SESSION_UPDATED
        | event::SESSION_RUNTIME_LOST
        | event::SESSION_RUNTIME_RECONNECTED => parse_projector_payload::<SessionPayload>(event)
            .map(|payload| (payload.session.id, None)),
        _ => None,
    }
}

/// The session a `session.stopped` event names. The projector does not seed
/// for it, but it queues behind the inputs the session already waits with.
fn stopped_subject(event: &Event) -> Option<SessionId> {
    (event.event() == event::SESSION_STOPPED)
        .then(|| parse_projector_payload::<SessionPayload>(event))
        .flatten()
        .map(|payload| payload.session.id)
}

#[derive(Debug, Default)]
struct ProjectorState {
    activity_by_session: HashMap<SessionId, AgentActivity>,
    /// The kind the last blocked reading of a session carried.
    blocked_kind_by_session: HashMap<SessionId, Option<BlockedKind>>,
    /// Sessions whose current blocked state is backed by the program's own
    /// status record (it came from `OscProgress` or named a kind), not by a
    /// screen or title reading alone.
    record_backed_blocks: HashSet<SessionId>,
    lifecycle_by_session: HashMap<SessionId, SessionState>,
    transition_epochs: HashMap<(SessionId, NotificationKind), u64>,
    /// Sessions whose transition epochs continue from the store's records.
    seeded_sessions: HashSet<SessionId>,
    /// Seeded sessions with an unread attention record and no blocked,
    /// working or terminal observation in this process yet. Their first
    /// blocked observation replays the condition that record surfaces.
    blocked_replay_candidates: HashSet<SessionId>,
    /// The inputs of sessions whose epoch seed failed, in arrival order. A
    /// session with a backlog queues every further input behind it, so the
    /// inputs project in the order they arrived once the seed succeeds.
    deferred: HashMap<SessionId, VecDeque<Deferred>>,
    /// When the seed of the sessions in `deferred` is retried next.
    seed_retry_at: Option<Instant>,
}

/// An input of one session the projector keeps until its epoch seed succeeds,
/// with the moment it arrived.
///
/// Its resolve acts on what existed at that moment, not on what other
/// producers created while the input waited.
#[derive(Debug)]
struct Deferred {
    input: DeferredInput,
    arrived: ResolveAsOf,
}

#[derive(Debug)]
enum DeferredInput {
    Event(Arc<Event>, EventOrigin),
    Snapshot(Box<SessionInfo>),
    Edge(TurnEdge),
}

impl ProjectorState {
    /// Projects `event`, which was produced at `produced`; the input keeps
    /// that moment as its arrival while it waits for a seed.
    async fn handle_produced_event_blocking(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        event: &Arc<Event>,
        produced: StdInstant,
        origin: EventOrigin,
    ) {
        let seeded_subject = event_subject(event);
        let ordered_subject = seeded_subject
            .as_ref()
            .map(|(session_id, _)| session_id.clone())
            .or_else(|| stopped_subject(event));
        if let Some(session_id) = ordered_subject {
            if self.deferred.contains_key(&session_id) {
                self.defer(
                    &session_id,
                    DeferredInput::Event(Arc::clone(event), origin),
                    ResolveAsOf::at(produced),
                );
                return;
            }
        }
        if let Some((session_id, _)) = &seeded_subject {
            if !self.seed_transition_epochs(notifications, session_id).await {
                self.defer(
                    session_id,
                    DeferredInput::Event(Arc::clone(event), origin),
                    ResolveAsOf::at(produced),
                );
                return;
            }
        }
        self.project_event(
            notifications,
            &attention.as_of(ResolveAsOf::at(produced)),
            event,
            origin,
        )
        .await;
    }

    async fn project_event(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        event: &Event,
        origin: EventOrigin,
    ) {
        match origin {
            EventOrigin::Live => {}
            EventOrigin::Replayed => {
                if let Some(payload) = parse_projector_payload::<SessionPayload>(event) {
                    self.learn_replayed(&payload.session);
                }
                return;
            }
        }
        if let Some((session_id, activity)) = event_subject(event) {
            self.confirm_blocked_replay(notifications, &session_id, activity)
                .await;
        }
        for pending in self.pending_event(attention, event) {
            create_pending_notification_blocking(notifications.clone(), attention, pending).await;
        }
    }

    /// Takes in a session state the registry announced again without a
    /// change: later transitions are measured against it.
    fn learn_replayed(&mut self, session: &SessionInfo) {
        self.lifecycle_by_session
            .insert(session.id.clone(), session.state);
    }

    /// Projects `session`, a snapshot of the registry read at `read_at`; a
    /// resolve it causes acts on what existed then.
    async fn handle_session_snapshot_blocking(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        session: &SessionInfo,
        read_at: StdInstant,
    ) {
        if self.deferred.contains_key(&session.id)
            || !self
                .seed_transition_epochs(notifications, &session.id)
                .await
        {
            self.defer(
                &session.id,
                DeferredInput::Snapshot(Box::new(session.clone())),
                ResolveAsOf::at(read_at),
            );
            return;
        }
        self.project_session_snapshot(
            notifications,
            &attention.as_of(ResolveAsOf::at(read_at)),
            session,
        )
        .await;
    }

    async fn project_session_snapshot(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        session: &SessionInfo,
    ) {
        self.confirm_blocked_replay(notifications, &session.id, session.activity)
            .await;
        for pending in self.pending_session_snapshot(attention, session) {
            create_pending_notification_blocking(notifications.clone(), attention, pending).await;
        }
    }

    /// Acts on a turn change of the registry: a turn that went idle derives its
    /// `turn_completed`; a turn that opened ends what the earlier turns left
    /// unread. Both arrive in the order the turns changed.
    async fn handle_turn_edge_blocking(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        edge: TurnEdge,
    ) {
        let session_id = match &edge {
            TurnEdge::Started(session_id, ..) | TurnEdge::Ended(session_id, ..) => {
                session_id.clone()
            }
        };
        let arrived = ResolveAsOf::at(edge.happened_at());
        if self.deferred.contains_key(&session_id) {
            self.defer(&session_id, DeferredInput::Edge(edge), arrived);
            return;
        }
        if matches!(edge, TurnEdge::Ended(..))
            && !self
                .seed_transition_epochs(notifications, &session_id)
                .await
        {
            self.defer(&session_id, DeferredInput::Edge(edge), arrived);
            return;
        }
        self.project_turn_edge(notifications, &attention.as_of(arrived), edge)
            .await;
    }

    async fn project_turn_edge(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        edge: TurnEdge,
    ) {
        let (session_id, turn) = match edge {
            TurnEdge::Started(session_id, ..) => {
                attention.resolve(turn_dedupe_key(&session_id));
                return;
            }
            TurnEdge::Ended(session_id, turn, _) => (session_id, turn),
        };
        let session_id = &session_id;
        let pending = self.prepare_derived(DerivedNotification {
            session_id: session_id.clone(),
            kind: NotificationKind::TurnCompleted,
            severity: NotificationSeverity::Info,
            source_event: event::AGENT_STATE,
            title: "Turn completed".to_owned(),
            body: format!("Session {} completed a turn.", session_id.0),
            metadata: BTreeMap::new(),
            agent_kind: None,
            project_id: None,
            dedupe_key: Some(turn_dedupe_key(session_id)),
        });
        if let Some(pending) = pending {
            create_pending_notification_blocking(
                notifications.clone(),
                attention,
                PendingNotification {
                    turn: Some(turn),
                    ..pending
                },
            )
            .await;
        }
    }

    /// Keeps `input` of `session_id`, which arrived at `arrived`, behind the
    /// inputs it waits for and arms the seed retry.
    ///
    /// Consecutive snapshots collapse into the newest one, which states the
    /// session's current condition. A backlog at its bound drops its oldest
    /// input.
    fn defer(&mut self, session_id: &SessionId, input: DeferredInput, arrived: ResolveAsOf) {
        let backlog = self.deferred.entry(session_id.clone()).or_default();
        match (backlog.back_mut(), input) {
            (
                Some(Deferred {
                    input: DeferredInput::Snapshot(newest),
                    arrived: newest_arrived,
                }),
                DeferredInput::Snapshot(snapshot),
            ) => {
                *newest = snapshot;
                *newest_arrived = arrived;
            }
            (_, input) => {
                if backlog.len() >= MAX_DEFERRED_INPUTS_PER_SESSION {
                    backlog.pop_front();
                    warn!(
                        session_id = %session_id.0,
                        limit = MAX_DEFERRED_INPUTS_PER_SESSION,
                        "projector backlog of a session whose epoch seed keeps failing is full; dropping its oldest input"
                    );
                }
                backlog.push_back(Deferred { input, arrived });
            }
        }
        self.seed_retry_at
            .get_or_insert_with(|| Instant::now() + SEED_RETRY_DELAY);
    }

    /// Retries the seed of every session with a backlog and projects the
    /// backlog of each one that seeds, in arrival order.
    async fn retry_deferred(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
    ) {
        self.seed_retry_at = None;
        let waiting: Vec<SessionId> = self.deferred.keys().cloned().collect();
        for session_id in waiting {
            if !self
                .seed_transition_epochs(notifications, &session_id)
                .await
            {
                continue;
            }
            let Some(backlog) = self.deferred.remove(&session_id) else {
                continue;
            };
            for Deferred { input, arrived } in backlog {
                let attention = &attention.as_of(arrived);
                match input {
                    DeferredInput::Event(event, origin) => {
                        self.project_event(notifications, attention, &event, origin)
                            .await;
                    }
                    DeferredInput::Snapshot(session) => {
                        self.project_session_snapshot(notifications, attention, &session)
                            .await;
                    }
                    DeferredInput::Edge(edge) => {
                        self.project_turn_edge(notifications, attention, edge).await;
                    }
                }
            }
        }
        if !self.deferred.is_empty() {
            self.seed_retry_at = Some(Instant::now() + SEED_RETRY_DELAY);
        }
    }

    /// Logs the inputs that are still waiting for a seed when the projector
    /// ends.
    fn warn_unprojected(&self) {
        for (session_id, backlog) in &self.deferred {
            warn!(
                session_id = %session_id.0,
                inputs = backlog.len(),
                "projector ended while the epoch seed of a session kept failing; its inputs are not projected"
            );
        }
    }

    fn pending_event(
        &mut self,
        attention: &AttentionCoordinator,
        event: &Event,
    ) -> Vec<PendingNotification> {
        match event.event() {
            event::AGENT_STATE => {
                if let Some(payload) = parse_projector_payload::<AgentStatePayload>(event) {
                    let origin = ActivityOrigin {
                        source: Some(payload.source),
                        blocked_kind: payload.blocked_kind,
                        fallback: payload.fallback.is_some(),
                    };
                    return self
                        .pending_activity(attention, &payload.session_id, payload.activity, origin)
                        .into_iter()
                        .collect();
                }
            }
            // A lost runtime is announced by its own event, which is also how
            // startup reconciliation commits a loss.
            event::SESSION_UPDATED
            | event::SESSION_RUNTIME_LOST
            | event::SESSION_RUNTIME_RECONNECTED => {
                if let Some(payload) = parse_projector_payload::<SessionPayload>(event) {
                    self.pending_session_updated(attention, &payload.session);
                }
            }
            event::SESSION_STOPPED => {
                if let Some(payload) = parse_projector_payload::<SessionPayload>(event) {
                    self.handle_explicit_stop(attention, &payload.session);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn pending_session_snapshot(
        &mut self,
        attention: &AttentionCoordinator,
        session: &SessionInfo,
    ) -> Vec<PendingNotification> {
        let mut pending = Vec::new();
        if let Some(activity) = session.activity {
            pending.extend(self.pending_activity(
                attention,
                &session.id,
                activity,
                ActivityOrigin {
                    source: Some(session.state_source),
                    blocked_kind: session.blocked_kind,
                    // A snapshot carries no fallback marker: only a published
                    // `agent_state` event does.
                    fallback: false,
                },
            ));
        } else {
            self.activity_by_session.remove(&session.id);
            self.blocked_kind_by_session.remove(&session.id);
            self.record_backed_blocks.remove(&session.id);
        }
        if session.state == SessionState::Stopped {
            self.handle_explicit_stop(attention, session);
        } else {
            self.pending_session_updated(attention, session);
        }
        pending
    }

    fn pending_activity(
        &mut self,
        attention: &AttentionCoordinator,
        session_id: &SessionId,
        activity: AgentActivity,
        origin: ActivityOrigin,
    ) -> Option<PendingNotification> {
        let previous = self
            .activity_by_session
            .insert(session_id.clone(), activity);
        // Active work proves that a prior waiting condition has ended. Resolve on
        // the transition edge into `Working`; the terminal lifecycle paths below
        // resolve stale attention separately. Do not gate on a previous blocked
        // observation because provider hooks can create attention without the
        // detector seeing that edge first.
        if activity == AgentActivity::Working {
            self.blocked_replay_candidates.remove(session_id);
            if previous != Some(AgentActivity::Working) {
                // The turn key resolves with the turn that starts, in order
                // with the turns that ended.
                attention.resolve(attention_dedupe_key(session_id));
            }
        }
        // Idle from the program's status channel (`OscProgress`, normally a 7501
        // record) right after a record-backed `blocked` means the dialog closed
        // (Esc, a denial, an auto-denial). A fallback idle means the screen
        // showed no dialog for the whole staleness window, so it resolves too.
        // An idle read from the screen or the title does not resolve, and
        // neither does an idle record that follows a dialog only the screen
        // saw: that dialog can still be waiting.
        let record_backed = self.record_backed_blocks.contains(session_id);
        if activity == AgentActivity::Idle
            && previous == Some(AgentActivity::Blocked)
            && (origin.source == Some(StateSource::OscProgress) || origin.fallback)
            && record_backed
        {
            self.blocked_replay_candidates.remove(session_id);
            attention.resolve(attention_dedupe_key(session_id));
        }
        if activity == AgentActivity::Blocked {
            if origin.source == Some(StateSource::OscProgress) || origin.blocked_kind.is_some() {
                self.record_backed_blocks.insert(session_id.clone());
            } else if previous != Some(AgentActivity::Blocked) {
                self.record_backed_blocks.remove(session_id);
            }
        } else {
            self.record_backed_blocks.remove(session_id);
        }
        // A blocked reading that names no kind keeps the kind already derived.
        let named_kind = origin
            .blocked_kind
            .filter(|kind| *kind != BlockedKind::Unspecified);
        let kind_before = if activity == AgentActivity::Blocked {
            if previous == Some(AgentActivity::Blocked) && named_kind.is_none() {
                self.blocked_kind_by_session.get(session_id).copied()
            } else {
                self.blocked_kind_by_session
                    .insert(session_id.clone(), named_kind)
            }
        } else {
            self.blocked_kind_by_session.remove(session_id);
            None
        };
        // A blocked reading that names a different kind than the one derived
        // replaces that attention record.
        let kind_changed = previous == Some(AgentActivity::Blocked)
            && named_kind.is_some()
            && kind_before.flatten() != named_kind;
        if activity != AgentActivity::Blocked
            || (previous == Some(AgentActivity::Blocked) && !kind_changed)
        {
            return None;
        }
        if kind_changed {
            attention.resolve(attention_dedupe_key(session_id));
        }
        // A daemon process replays the blocked state of a session that was
        // blocked before it started; an unread record already surfaces it.
        if self.blocked_replay_candidates.remove(session_id) {
            return None;
        }
        let wait = BlockedWait::of(named_kind);
        self.prepare_derived(DerivedNotification {
            session_id: session_id.clone(),
            kind: wait.kind,
            severity: NotificationSeverity::ActionRequired,
            source_event: event::AGENT_STATE,
            title: wait.title.to_owned(),
            body: format!("Session {} {}.", session_id.0, wait.body),
            metadata: BTreeMap::from([("reason".to_owned(), wait.reason.to_owned())]),
            agent_kind: None,
            project_id: None,
            dedupe_key: Some(attention_dedupe_key(session_id)),
        })
    }

    /// Notes the lifecycle state of `session`. A session that left the states
    /// in which it can wait for the owner (failed, done, stopped, or running on
    /// a lost runtime) resolves its attention records; no record is derived for
    /// the state itself, `session list` shows it.
    fn pending_session_updated(&mut self, attention: &AttentionCoordinator, session: &SessionInfo) {
        let previous = self
            .lifecycle_by_session
            .insert(session.id.clone(), session.state);
        let ended = match session.state {
            SessionState::Failed | SessionState::Done => previous != Some(session.state),
            SessionState::Stopped => {
                self.handle_explicit_stop(attention, session);
                return;
            }
            SessionState::Starting | SessionState::Running => session
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.state == RuntimeState::Lost),
        };
        if ended {
            resolve_session_notifications(attention, &session.id);
            self.activity_by_session.remove(&session.id);
            self.blocked_kind_by_session.remove(&session.id);
            self.record_backed_blocks.remove(&session.id);
            self.blocked_replay_candidates.remove(&session.id);
        }
    }

    fn handle_explicit_stop(&mut self, attention: &AttentionCoordinator, session: &SessionInfo) {
        resolve_session_notifications(attention, &session.id);
        self.lifecycle_by_session
            .insert(session.id.clone(), SessionState::Stopped);
        self.activity_by_session.remove(&session.id);
        self.blocked_replay_candidates.remove(&session.id);
    }

    fn prepare_derived(&mut self, derived: DerivedNotification) -> Option<PendingNotification> {
        let Some(epoch) = self.next_transition_epoch(&derived.session_id, derived.kind) else {
            warn!(
                session_id = %derived.session_id.0,
                kind = derived.kind.as_str(),
                "projector transition epoch is exhausted; the notification is not created"
            );
            return None;
        };
        let source_id = projector_source_id(&derived.session_id, derived.kind, epoch);
        let params = NotificationCreateParams {
            source: NotificationSource {
                provider: PROJECTOR_PROVIDER.to_owned(),
                provider_event: derived.source_event.to_owned(),
                host_local_source_id: source_id.clone(),
            },
            kind: derived.kind,
            severity: derived.severity,
            title: derived.title,
            body: derived.body,
            metadata: derived.metadata,
            session_id: Some(derived.session_id.clone()),
            agent_kind: derived.agent_kind,
            source_id: Some(source_id),
            dedupe_key: derived.dedupe_key,
            project_id: derived.project_id,
            retryable_admission: false,
        };
        Some(PendingNotification {
            turn: None,
            session_id: derived.session_id,
            kind: derived.kind,
            params,
        })
    }

    /// Seeds the transition state of `session_id` from the store's records,
    /// once per session and daemon process:
    /// - the epochs continue from the highest ones the store holds;
    /// - an unread attention record makes the session a blocked replay
    ///   candidate.
    ///
    /// The store is read off the async runtime.
    ///
    /// Returns whether the session is seeded. An unseeded session's input is
    /// not projected, because its transition could reuse a stored key; the
    /// caller keeps the input and the projector retries the seed.
    async fn seed_transition_epochs(
        &mut self,
        notifications: &NotificationService,
        session_id: &SessionId,
    ) -> bool {
        if self.seeded_sessions.contains(session_id) {
            return true;
        }
        let service = notifications.clone();
        let prefix = projector_session_prefix(session_id);
        let attention_key = attention_dedupe_key(session_id);
        let (suffixes, unread_attention) = match run_projector_blocking(move || {
            (
                service.source_id_suffixes(PROJECTOR_PROVIDER, &prefix),
                service.has_unread_attention(&attention_key),
            )
        })
        .await
        {
            Ok(seed) => seed,
            Err(error) => {
                warn!(
                    session_id = %session_id.0,
                    %error,
                    "projector epoch seeding task failed; the session's inputs wait for the next retry"
                );
                return false;
            }
        };
        for (suffix, _created_at) in suffixes {
            let Some((kind, key)) = suffix.rsplit_once(':') else {
                continue;
            };
            let (Some(kind), Ok(epoch)) = (projected_kind(kind), key.parse::<u64>()) else {
                continue;
            };
            let current = self
                .transition_epochs
                .entry((session_id.clone(), kind))
                .or_insert(0);
            *current = (*current).max(epoch);
        }
        if unread_attention {
            self.blocked_replay_candidates.insert(session_id.clone());
        }
        self.seeded_sessions.insert(session_id.clone());
        true
    }

    /// Keeps a blocked replay candidate only while the store still holds an
    /// unread attention record for it, checked when its blocked state arrives.
    /// If the store cannot be read, the session stays a candidate and its
    /// replayed blocked state is not projected.
    async fn confirm_blocked_replay(
        &mut self,
        notifications: &NotificationService,
        session_id: &SessionId,
        activity: Option<AgentActivity>,
    ) {
        if activity != Some(AgentActivity::Blocked)
            || !self.blocked_replay_candidates.contains(session_id)
        {
            return;
        }
        let service = notifications.clone();
        let attention_key = attention_dedupe_key(session_id);
        match run_projector_blocking(move || service.has_unread_attention(&attention_key)).await {
            Ok(true) => {}
            Ok(false) => {
                self.blocked_replay_candidates.remove(session_id);
            }
            Err(error) => {
                warn!(
                    session_id = %session_id.0,
                    %error,
                    "projector attention check failed; the session's replayed blocked state is not projected"
                );
            }
        }
    }

    /// The next transition epoch of `session_id` and `kind`, or `None` once
    /// the counter is exhausted.
    fn next_transition_epoch(
        &mut self,
        session_id: &SessionId,
        kind: NotificationKind,
    ) -> Option<u64> {
        let epoch = self
            .transition_epochs
            .entry((session_id.clone(), kind))
            .or_insert(0);
        *epoch = epoch.checked_add(1)?;
        Some(*epoch)
    }
}

#[derive(Debug)]
struct PendingNotification {
    session_id: SessionId,
    kind: NotificationKind,
    params: NotificationCreateParams,
    /// The turn the notification reports, for a `turn_completed`.
    turn: Option<TurnId>,
}

async fn create_pending_notification_blocking(
    notifications: NotificationService,
    attention: &AttentionCoordinator,
    pending: PendingNotification,
) {
    if is_debounced_create(pending.kind, pending.params.dedupe_key.as_deref()) {
        // Preparing a deferred record does no store I/O and deferring is a
        // channel send, so this stays on the runtime worker; the coordinator
        // commits after the debounce window.
        defer_pending_notification(&notifications, attention, pending);
        return;
    }
    let session_id = pending.session_id;
    let kind = pending.kind;
    let params = pending.params;
    // Await each blocking create before handling the next event. That keeps
    // derived notifications in session event order while moving sync JSONL
    // append/flush work off the Tokio runtime worker.
    match run_projector_blocking(move || notifications.create(params)).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            warn!(
                error = %err,
                session_id = %session_id.0,
                kind = kind.as_str(),
                "failed to create derived notification"
            );
        }
        Err(err) => {
            warn!(
                error = %err,
                session_id = %session_id.0,
                kind = kind.as_str(),
                "notification projector blocking create task failed"
            );
        }
    }
}

/// Route a derived session notification through the debounce coordinator.
///
/// Best-effort: a preparation failure is logged and swallowed so the projector
/// event loop keeps consuming session events.
fn defer_pending_notification(
    notifications: &NotificationService,
    attention: &AttentionCoordinator,
    pending: PendingNotification,
) {
    match notifications.prepare_deferred(pending.params) {
        // The projector derives the signal from the registry's own events.
        Ok(deferred) => {
            let deferred = DeferredNotification {
                signal: true,
                ..deferred
            };
            attention.defer(match pending.turn {
                Some(turn) => deferred.for_turn(turn),
                None => deferred,
            });
        }
        Err(err) => {
            warn!(
                error = %err,
                session_id = %pending.session_id.0,
                kind = pending.kind.as_str(),
                "failed to prepare derived session notification for debounce"
            );
        }
    }
}

async fn run_projector_blocking<F, T>(op: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(op).await
}

#[derive(Debug)]
struct DerivedNotification {
    session_id: SessionId,
    kind: NotificationKind,
    severity: NotificationSeverity,
    source_event: &'static str,
    title: String,
    body: String,
    metadata: BTreeMap<String, String>,
    agent_kind: Option<RuntimeRef>,
    project_id: Option<String>,
    dedupe_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentStatePayload {
    session_id: SessionId,
    activity: AgentActivity,
    source: StateSource,
    #[serde(default)]
    blocked_kind: Option<BlockedKind>,
    /// Present while the activity was arbitrated under a stale-record
    /// fallback; only presence matters here. A snapshot carries no marker.
    #[serde(default)]
    fallback: Option<serde::de::IgnoredAny>,
}

/// Where an activity reading came from: the source and, for a blocked reading
/// the program's status record reported, the kind it carried.
#[derive(Debug, Default, Clone, Copy)]
struct ActivityOrigin {
    source: Option<StateSource>,
    blocked_kind: Option<BlockedKind>,
    /// Whether the activity was arbitrated under a stale-record fallback.
    fallback: bool,
}

/// The notification a blocked agent maps to, by what it reports waiting for.
///
/// The body never carries the program's own text: that text is not trusted.
struct BlockedWait {
    kind: NotificationKind,
    reason: &'static str,
    title: &'static str,
    body: &'static str,
}

impl BlockedWait {
    fn of(kind: Option<BlockedKind>) -> Self {
        match kind {
            Some(BlockedKind::Permission) => Self {
                kind: NotificationKind::ApprovalRequired,
                reason: "permission",
                title: "Agent approval required",
                body: "is waiting for approval",
            },
            Some(BlockedKind::Question) => Self {
                kind: NotificationKind::AgentBlocked,
                reason: "question",
                title: "Agent needs attention",
                body: "is waiting for an answer",
            },
            Some(BlockedKind::Auth) => Self {
                kind: NotificationKind::AgentBlocked,
                reason: "auth",
                title: "Agent needs attention",
                body: "is waiting for authentication",
            },
            Some(BlockedKind::Unspecified | BlockedKind::Unknown) | None => Self {
                kind: NotificationKind::AgentBlocked,
                reason: "blocked",
                title: "Agent needs attention",
                body: "is waiting for owner attention",
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct SessionPayload {
    session: SessionInfo,
}

fn parse_projector_payload<T>(event: &Event) -> Option<T>
where
    T: DeserializeOwned,
{
    match serde_json::from_value(event.payload().clone()) {
        Ok(payload) => Some(payload),
        Err(err) => {
            warn!(
                event = %event.event(),
                error = %err,
                "failed to parse session event for notification projector"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_id(name: &str) -> SessionId {
        SessionId(name.to_owned())
    }

    fn record_backed_blocked() -> ActivityOrigin {
        ActivityOrigin {
            source: Some(StateSource::OscProgress),
            blocked_kind: Some(BlockedKind::Permission),
            fallback: false,
        }
    }

    /// A non-fallback screen idle over a record-backed `blocked` sends no
    /// resolve to the coordinator: only the program channel's own idle and
    /// a fallback idle close the approval.
    #[test]
    fn a_non_fallback_screen_idle_sends_no_resolve() {
        let (attention, mut resolves) = AttentionCoordinator::resolve_probed();
        let mut state = ProjectorState::default();
        let id = session_id("plain-screen-idle-sends-no-resolve");

        let pending = state.pending_activity(
            &attention,
            &id,
            AgentActivity::Blocked,
            record_backed_blocked(),
        );
        assert!(pending.is_some(), "the blocked record opens the approval");
        assert!(state.record_backed_blocks.contains(&id));

        let pending = state.pending_activity(
            &attention,
            &id,
            AgentActivity::Idle,
            ActivityOrigin {
                source: Some(StateSource::Screen),
                blocked_kind: None,
                fallback: false,
            },
        );
        assert!(
            pending.is_none(),
            "a non-fallback screen idle derives nothing"
        );
        assert!(
            resolves.resolves().is_empty(),
            "the plain screen idle sends no resolve"
        );
        // The non-record idle clears the record-backed mark, so a later
        // idle cannot resolve against it: `activity_by_session` holds the
        // new idle, and the mark must go with the edge that replaced it.
        assert!(
            !state.record_backed_blocks.contains(&id),
            "a non-record idle clears the record-backed mark"
        );
    }

    /// The widened resolve: a fallback idle over a record-backed `blocked`
    /// sends the resolve command to the coordinator, while a non-fallback
    /// screen idle sends nothing. The probe counts resolves per run.
    #[test]
    fn a_fallback_idle_resolves_while_a_plain_screen_idle_sends_nothing() {
        for (fallback, expected_resolves) in [(false, 0), (true, 1)] {
            let (attention, mut receiver) = AttentionCoordinator::resolve_probed();
            let mut state = ProjectorState::default();
            let id = session_id("fallback-resolve");

            let pending = state.pending_activity(
                &attention,
                &id,
                AgentActivity::Blocked,
                record_backed_blocked(),
            );
            assert!(pending.is_some(), "the blocked record opens the approval");
            let pending = state.pending_activity(
                &attention,
                &id,
                AgentActivity::Idle,
                ActivityOrigin {
                    source: Some(StateSource::Screen),
                    blocked_kind: None,
                    fallback,
                },
            );
            assert!(pending.is_none(), "an idle derives no record");
            let expected = vec![attention_dedupe_key(&id); expected_resolves];
            assert_eq!(
                receiver.resolves(),
                expected,
                "fallback={fallback}: the coordinator gets exactly the widened resolve"
            );
        }
    }
}
