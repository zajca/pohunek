//! Derived notifications from session events.

// Rust guideline compliant 2026-10-12

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use protocol::{
    event, AgentActivity, Event, NotificationCreateParams, NotificationKind, NotificationSeverity,
    NotificationSource, RuntimeGeneration, RuntimeRef, SessionId, SessionInfo, SessionState,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::session::SessionRegistry;

use super::{
    is_debounced_create, parse_timestamp, policy_enables_kind, AttentionCoordinator,
    DeferredNotification, NotificationService, ATTENTION_DEDUPE_KEY_PREFIX, TURN_DEDUPE_KEY_PREFIX,
};

/// Provider namespace used for daemon-derived notification records.
const PROJECTOR_PROVIDER: &str = "pohunek";

/// Source-id prefix for daemon-derived notification records.
const PROJECTOR_SOURCE_ID_PREFIX: &str = "projector";

/// Maximum time to wait for the projector task to flush buffered events.
///
/// Projector shutdown only drains local broadcast events and performs
/// owner-private JSONL appends through [`NotificationService`]. Five seconds
/// matches the daemon event-log shutdown budget and bounds a wedged filesystem.
const PROJECTOR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Derives durable notifications from session lifecycle events.
#[derive(Debug)]
pub struct NotificationProjector {
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
}

impl NotificationProjector {
    /// Spawn a projector task on the session event broadcast.
    ///
    /// Derived debounced session notifications are routed through `attention`
    /// (the debounce coordinator) rather than persisted directly, and the resume
    /// edge resolves session-scoped notification keys through it too.
    #[must_use]
    pub fn spawn(
        sessions: &SessionRegistry,
        notifications: NotificationService,
        attention: AttentionCoordinator,
    ) -> Self {
        let shutdown = CancellationToken::new();
        let handle = spawn_projector_task(
            sessions.clone(),
            notifications,
            attention,
            sessions.subscribe(),
            shutdown.clone(),
        );
        Self { shutdown, handle }
    }

    /// Stop the projector and drain buffered session events.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        match tokio::time::timeout(PROJECTOR_SHUTDOWN_TIMEOUT, self.handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(error = %err, "notification projector task failed during shutdown");
            }
            Err(_) => {
                warn!("notification projector did not finish within the shutdown timeout");
            }
        }
    }
}

fn spawn_projector_task(
    sessions: SessionRegistry,
    notifications: NotificationService,
    attention: AttentionCoordinator,
    mut events: broadcast::Receiver<Event>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = ProjectorState::default();
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    drain_buffered_events(&sessions, &notifications, &attention, &mut events, &mut state).await;
                    break;
                }
                received = events.recv() => match received {
                    Ok(event) => state.handle_event_blocking(&notifications, &attention, &event).await,
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

async fn drain_buffered_events(
    sessions: &SessionRegistry,
    notifications: &NotificationService,
    attention: &AttentionCoordinator,
    events: &mut broadcast::Receiver<Event>,
    state: &mut ProjectorState,
) {
    loop {
        match events.try_recv() {
            Ok(event) => {
                state
                    .handle_event_blocking(notifications, attention, &event)
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
}

async fn resync_from_registry(
    sessions: &SessionRegistry,
    notifications: &NotificationService,
    attention: &AttentionCoordinator,
    state: &mut ProjectorState,
) {
    for session in sessions.list().await {
        state
            .handle_session_snapshot_blocking(notifications, attention, &session)
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

/// What tells apart the projected transitions of one session and kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionKey {
    /// Increments each time the session enters a projected state for the
    /// kind. It continues from the highest epoch the store holds for the
    /// session and kind, so a restarted daemon never reuses the key of a
    /// record from before the restart, which the store would merge the new
    /// transition into.
    Epoch(u64),
    /// The runtime generation a terminal state ends. A generation ends once,
    /// so a terminal state the daemon replays after a restart keeps the key
    /// of the record it already projected.
    Generation(RuntimeGeneration),
}

/// Marks a [`TransitionKey::Generation`] in a source id.
const GENERATION_KEY_PREFIX: char = 'g';

/// Return a deterministic projector source id.
///
/// Scheme: `projector:<session_id>:<kind>:<epoch>` or
/// `projector:<session_id>:<kind>:g<runtime_generation>`.
#[must_use]
fn projector_source_id(
    session_id: &SessionId,
    kind: NotificationKind,
    transition: TransitionKey,
) -> String {
    let prefix = projector_source_id_prefix(session_id, kind);
    match transition {
        TransitionKey::Epoch(epoch) => format!("{prefix}{epoch}"),
        TransitionKey::Generation(generation) => {
            format!("{prefix}{GENERATION_KEY_PREFIX}{generation}")
        }
    }
}

/// The transition key a stored projector source id suffix names.
fn parse_transition_key(key: &str) -> Option<TransitionKey> {
    match key.strip_prefix(GENERATION_KEY_PREFIX) {
        Some(generation) => RuntimeGeneration::parse(generation)
            .ok()
            .map(TransitionKey::Generation),
        None => key.parse().ok().map(TransitionKey::Epoch),
    }
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
/// derives that kind.
fn projected_kind(name: &str) -> Option<NotificationKind> {
    [
        NotificationKind::AgentBlocked,
        NotificationKind::ApprovalRequired,
        NotificationKind::TurnCompleted,
        NotificationKind::SessionFinished,
        NotificationKind::Error,
    ]
    .into_iter()
    .find(|kind| kind.as_str() == name)
}

/// Whether the projector derives `kind` from a session's terminal state.
fn is_terminal_kind(kind: NotificationKind) -> bool {
    matches!(
        kind,
        NotificationKind::Error | NotificationKind::SessionFinished
    )
}

/// Whether a deleted record can carry terminal replay evidence.
pub(super) fn is_terminal_tombstone(record: &protocol::NotificationRecord) -> bool {
    record.status == protocol::NotificationStatus::Deleted
        && record.source.provider == PROJECTOR_PROVIDER
        && is_terminal_kind(record.kind)
        && record.session_id.is_some()
        && record.source_id.is_some()
}

/// Whether this deleted terminal record still reports the durable runtime.
pub(super) fn terminal_tombstone_matches(
    record: &protocol::NotificationRecord,
    session: &SessionInfo,
) -> bool {
    if !is_terminal_tombstone(record) {
        return false;
    }
    let prefix = projector_source_id_prefix(&session.id, record.kind);
    let Some(key) = record
        .source_id
        .as_deref()
        .and_then(|id| id.strip_prefix(&prefix))
        .and_then(parse_transition_key)
    else {
        return false;
    };
    match key {
        TransitionKey::Generation(generation) => {
            session_runtime_generation(session) == Some(generation)
        }
        TransitionKey::Epoch(_) => parse_timestamp(&record.created_at)
            .is_ok_and(|created_at| epoch_keyed_record_reports_runtime(created_at, session)),
    }
}

/// The session an event the projector derives notifications from names, with
/// the agent activity the projector reads from it.
fn event_subject(event: &Event) -> Option<(SessionId, Option<AgentActivity>)> {
    match event.event() {
        event::AGENT_STATE => parse_projector_payload::<AgentStatePayload>(event)
            .map(|payload| (payload.session_id, Some(payload.activity))),
        event::SESSION_UPDATED => parse_projector_payload::<SessionPayload>(event)
            .map(|payload| (payload.session.id, None)),
        _ => None,
    }
}

/// Whether a stored terminal record keyed by transition epoch reports the
/// runtime generation `session` ends. Such a record names no generation, so it
/// counts only when it was created after the generation started. A generation
/// whose start is unknown, such as one a resume prepared before its worker
/// connected, is not covered: its terminal state may be newer than the record.
/// A session without runtime information keys its terminal records by epoch
/// and is never bridged.
fn epoch_keyed_record_reports_runtime(created_at: OffsetDateTime, session: &SessionInfo) -> bool {
    let Some(runtime) = session.runtime.as_ref() else {
        return false;
    };
    runtime
        .started_at
        .as_deref()
        .and_then(|started_at| parse_timestamp(started_at).ok())
        .is_some_and(|started_at| created_at >= started_at)
}

#[derive(Debug, Default)]
struct ProjectorState {
    activity_by_session: HashMap<SessionId, AgentActivity>,
    lifecycle_by_session: HashMap<SessionId, SessionState>,
    transition_epochs: HashMap<(SessionId, NotificationKind), u64>,
    /// Terminal states the store or this process holds a record key for.
    projected_generations: HashSet<(SessionId, NotificationKind, RuntimeGeneration)>,
    /// Sessions whose transition epochs continue from the store's records.
    seeded_sessions: HashSet<SessionId>,
    /// Seeded sessions with an unread attention record and no blocked,
    /// working or terminal observation in this process yet. Their first
    /// blocked observation replays the condition that record surfaces.
    blocked_replay_candidates: HashSet<SessionId>,
    /// The latest creation time of each terminal kind's stored records that
    /// are keyed by transition epoch, by session, until the session's first
    /// lifecycle observation in this process.
    epoch_keyed_terminal_records: HashMap<SessionId, HashMap<NotificationKind, OffsetDateTime>>,
}

impl ProjectorState {
    async fn handle_event_blocking(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        event: &Event,
    ) {
        if let Some((session_id, activity)) = event_subject(event) {
            if !self
                .seed_transition_epochs(notifications, &session_id)
                .await
            {
                return;
            }
            self.confirm_blocked_replay(notifications, &session_id, activity)
                .await;
        }
        for pending in self.pending_event(notifications, attention, event) {
            create_pending_notification_blocking(notifications.clone(), attention, pending).await;
        }
    }

    async fn handle_session_snapshot_blocking(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        session: &SessionInfo,
    ) {
        if !self
            .seed_transition_epochs(notifications, &session.id)
            .await
        {
            return;
        }
        self.confirm_blocked_replay(notifications, &session.id, session.activity)
            .await;
        for pending in self.pending_session_snapshot(notifications, attention, session) {
            create_pending_notification_blocking(notifications.clone(), attention, pending).await;
        }
    }

    fn pending_event(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        event: &Event,
    ) -> Vec<PendingNotification> {
        match event.event() {
            event::AGENT_STATE => {
                if let Some(payload) = parse_projector_payload::<AgentStatePayload>(event) {
                    return self
                        .pending_activity(
                            notifications,
                            attention,
                            &payload.session_id,
                            payload.activity,
                        )
                        .into_iter()
                        .collect();
                }
            }
            event::SESSION_UPDATED => {
                if let Some(payload) = parse_projector_payload::<SessionPayload>(event) {
                    return self
                        .pending_session_updated(notifications, attention, &payload.session)
                        .into_iter()
                        .collect();
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
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        session: &SessionInfo,
    ) -> Vec<PendingNotification> {
        let mut pending = Vec::new();
        if let Some(activity) = session.activity {
            pending.extend(self.pending_activity(notifications, attention, &session.id, activity));
        } else {
            self.activity_by_session.remove(&session.id);
        }
        if session.state == SessionState::Stopped {
            self.handle_explicit_stop(attention, session);
        } else {
            pending.extend(self.pending_session_updated(notifications, attention, session));
        }
        pending
    }

    fn pending_activity(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        session_id: &SessionId,
        activity: AgentActivity,
    ) -> Option<PendingNotification> {
        let previous = self
            .activity_by_session
            .insert(session_id.clone(), activity);
        // Active work proves that a prior waiting condition has ended. Idle does
        // not: an approval prompt may be technically idle while still requiring
        // owner input. Resolve only on the transition edge into `Working`; the
        // terminal lifecycle paths below resolve stale attention separately.
        // Do not gate on a previous blocked observation because provider hooks
        // can create attention without the detector seeing that edge first.
        if activity == AgentActivity::Working {
            self.blocked_replay_candidates.remove(session_id);
            if previous != Some(AgentActivity::Working) {
                resolve_session_notifications(attention, session_id);
            }
        }
        if activity != AgentActivity::Blocked || previous == Some(AgentActivity::Blocked) {
            return None;
        }
        // A daemon process replays the blocked state of a session that was
        // blocked before it started; an unread record already surfaces it.
        if self.blocked_replay_candidates.remove(session_id) {
            return None;
        }
        self.prepare_derived(
            notifications,
            DerivedNotification {
                session_id: session_id.clone(),
                kind: NotificationKind::AgentBlocked,
                severity: NotificationSeverity::ActionRequired,
                source_event: event::AGENT_STATE,
                title: "Agent needs attention".to_owned(),
                body: format!("Session {} is waiting for owner attention.", session_id.0),
                metadata: BTreeMap::from([("reason".to_owned(), "blocked".to_owned())]),
                agent_kind: None,
                project_id: None,
                dedupe_key: Some(attention_dedupe_key(session_id)),
                runtime_generation: None,
            },
        )
    }

    fn pending_session_updated(
        &mut self,
        notifications: &NotificationService,
        attention: &AttentionCoordinator,
        session: &SessionInfo,
    ) -> Option<PendingNotification> {
        let previous = self
            .lifecycle_by_session
            .insert(session.id.clone(), session.state);
        let epoch_keyed_records = self
            .epoch_keyed_terminal_records
            .remove(&session.id)
            .filter(|_| previous.is_none())
            .unwrap_or_default();
        let reported_before = |kind| {
            epoch_keyed_records
                .get(&kind)
                .is_some_and(|created_at| epoch_keyed_record_reports_runtime(*created_at, session))
        };
        match session.state {
            SessionState::Failed if previous != Some(SessionState::Failed) => {
                resolve_session_notifications(attention, &session.id);
                self.activity_by_session.remove(&session.id);
                self.blocked_replay_candidates.remove(&session.id);
                if reported_before(NotificationKind::Error) {
                    return None;
                }
                self.pending_failed_notification(notifications, session)
            }
            SessionState::Done if previous != Some(SessionState::Done) => {
                resolve_session_notifications(attention, &session.id);
                self.activity_by_session.remove(&session.id);
                self.blocked_replay_candidates.remove(&session.id);
                if reported_before(NotificationKind::SessionFinished) {
                    return None;
                }
                self.pending_finished_notification(notifications, session)
            }
            SessionState::Stopped => {
                self.handle_explicit_stop(attention, session);
                None
            }
            SessionState::Starting
            | SessionState::Running
            | SessionState::Failed
            | SessionState::Done => None,
        }
    }

    fn handle_explicit_stop(&mut self, attention: &AttentionCoordinator, session: &SessionInfo) {
        resolve_session_notifications(attention, &session.id);
        self.lifecycle_by_session
            .insert(session.id.clone(), SessionState::Stopped);
        self.activity_by_session.remove(&session.id);
        self.blocked_replay_candidates.remove(&session.id);
    }

    fn pending_failed_notification(
        &mut self,
        notifications: &NotificationService,
        session: &SessionInfo,
    ) -> Option<PendingNotification> {
        let (body, reason) = if let Some(exit_code) = session.exit_code {
            (
                format!(
                    "Session {} failed with exit code {exit_code}.",
                    session.id.0
                ),
                format!("exit_code={exit_code}"),
            )
        } else {
            (
                format!("Session {} failed.", session.id.0),
                "failed".to_owned(),
            )
        };
        self.prepare_derived(
            notifications,
            DerivedNotification {
                session_id: session.id.clone(),
                kind: NotificationKind::Error,
                severity: NotificationSeverity::Error,
                source_event: event::SESSION_UPDATED,
                title: "Session failed".to_owned(),
                body,
                metadata: BTreeMap::from([("reason".to_owned(), reason)]),
                agent_kind: Some(session_agent_kind(session)),
                project_id: session.project_id.clone(),
                dedupe_key: None,
                runtime_generation: session_runtime_generation(session),
            },
        )
    }

    fn pending_finished_notification(
        &mut self,
        notifications: &NotificationService,
        session: &SessionInfo,
    ) -> Option<PendingNotification> {
        self.prepare_derived(
            notifications,
            DerivedNotification {
                session_id: session.id.clone(),
                kind: NotificationKind::SessionFinished,
                severity: NotificationSeverity::Success,
                source_event: event::SESSION_UPDATED,
                title: "Session finished".to_owned(),
                body: format!("Session {} finished successfully.", session.id.0),
                metadata: BTreeMap::from([("summary".to_owned(), "session_finished".to_owned())]),
                agent_kind: Some(session_agent_kind(session)),
                project_id: session.project_id.clone(),
                dedupe_key: None,
                runtime_generation: session_runtime_generation(session),
            },
        )
    }

    fn prepare_derived(
        &mut self,
        notifications: &NotificationService,
        derived: DerivedNotification,
    ) -> Option<PendingNotification> {
        // A debounced session signal is derived even when the policy drops its
        // inbox record: the coordinator still records the turn event.
        if !is_debounced_create(derived.kind, derived.dedupe_key.as_deref())
            && !policy_enables_kind(&notifications.policy(), PROJECTOR_PROVIDER, derived.kind)
        {
            return None;
        }
        let transition = if let Some(generation) = derived.runtime_generation {
            if !self.projected_generations.insert((
                derived.session_id.clone(),
                derived.kind,
                generation,
            )) {
                return None;
            }
            TransitionKey::Generation(generation)
        } else {
            let Some(epoch) = self.next_transition_epoch(&derived.session_id, derived.kind) else {
                warn!(
                    session_id = %derived.session_id.0,
                    kind = derived.kind.as_str(),
                    "projector transition epoch is exhausted; the notification is not created"
                );
                return None;
            };
            TransitionKey::Epoch(epoch)
        };
        let source_id = projector_source_id(&derived.session_id, derived.kind, transition);
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
            session_id: derived.session_id,
            kind: derived.kind,
            params,
        })
    }

    /// Seeds the transition state of `session_id` from the store's records,
    /// once per session and daemon process:
    /// - the epochs continue from the highest ones the store holds;
    /// - a terminal state with a generation-keyed record is not projected
    ///   again, and an epoch-keyed terminal record can report the generation
    ///   the session's first lifecycle observation ends;
    /// - an unread attention record makes the session a blocked replay
    ///   candidate.
    ///
    /// The store is read off the async runtime.
    ///
    /// Returns whether the session is seeded. An unseeded session's event is
    /// not projected, because its transition could reuse a stored key; the
    /// next event of the session tries again.
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
                    "projector epoch seeding task failed; the session's event is not projected"
                );
                return false;
            }
        };
        for (suffix, created_at) in suffixes {
            let Some((kind, key)) = suffix.rsplit_once(':') else {
                continue;
            };
            let (Some(kind), Some(key)) = (projected_kind(kind), parse_transition_key(key)) else {
                continue;
            };
            match key {
                TransitionKey::Epoch(epoch) => {
                    let current = self
                        .transition_epochs
                        .entry((session_id.clone(), kind))
                        .or_insert(0);
                    *current = (*current).max(epoch);
                    if let (true, Ok(created_at)) =
                        (is_terminal_kind(kind), parse_timestamp(&created_at))
                    {
                        let latest = self
                            .epoch_keyed_terminal_records
                            .entry(session_id.clone())
                            .or_default()
                            .entry(kind)
                            .or_insert(created_at);
                        *latest = (*latest).max(created_at);
                    }
                }
                TransitionKey::Generation(generation) => {
                    self.projected_generations
                        .insert((session_id.clone(), kind, generation));
                }
            }
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
        Ok(deferred) => attention.defer(DeferredNotification {
            signal: true,
            ..deferred
        }),
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
    /// The runtime generation a terminal state ends, when the session has a
    /// runtime; it keys the record instead of the transition epoch.
    runtime_generation: Option<RuntimeGeneration>,
}

#[derive(Debug, Deserialize)]
struct AgentStatePayload {
    session_id: SessionId,
    activity: AgentActivity,
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

fn session_runtime_generation(session: &SessionInfo) -> Option<RuntimeGeneration> {
    session
        .runtime
        .as_ref()
        .map(|runtime| runtime.runtime_generation)
}

fn session_agent_kind(session: &SessionInfo) -> RuntimeRef {
    session
        .active_agent_base
        .clone()
        .unwrap_or_else(|| session.agent_base.clone())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use protocol::{
        event, AgentActivity, Event, NotificationCreateParams, NotificationDeleteParams,
        NotificationKind, NotificationKindPolicy, NotificationListParams, NotificationSeverity,
        NotificationSource, NotificationStatus, NotificationUpdateParams, RuntimeGeneration,
        RuntimeRef, RuntimeState, SessionId, SessionInfo, SessionRuntime, SessionState,
        StateSource,
    };
    use serde_json::json;

    use super::{
        attention_dedupe_key, projector_source_id, run_projector_blocking, AttentionCoordinator,
        ProjectorState, TransitionKey,
    };
    use crate::notifications::{
        default_policy, NotificationService, DEFAULT_ATTENTION_DEBOUNCE_SECS,
    };

    /// Yield repeatedly so the spawned coordinator task drains its command channel
    /// and (re)arms its timer before the test advances the paused clock.
    async fn settle() {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }

    /// Advance the paused clock past the default debounce window to force a flush.
    async fn advance_past_debounce() {
        tokio::time::advance(std::time::Duration::from_secs(
            u64::from(DEFAULT_ATTENTION_DEBOUNCE_SECS) + 1,
        ))
        .await;
    }

    fn temp_data_dir(tag: &str) -> std::path::PathBuf {
        // The directory lives until the test's thread ends; the path below it
        // does not exist yet.
        crate::test_support::thread_scoped_dir(&format!("pohunek-notification-projector-{tag}-"))
            .join("data")
    }

    fn service(tag: &str) -> NotificationService {
        NotificationService::open(&temp_data_dir(tag)).expect("open notification service")
    }

    /// Spawns a coordinator over a registry that holds no session.
    fn spawn_coordinator(
        service: &NotificationService,
    ) -> (
        AttentionCoordinator,
        crate::notifications::AttentionCoordinatorTask,
    ) {
        AttentionCoordinator::spawn(
            service.clone(),
            &crate::session::SessionRegistry::new(crate::session::SessionRegistryConfig::default()),
        )
    }

    fn list(service: &NotificationService) -> Vec<protocol::NotificationRecord> {
        service
            .list(NotificationListParams::default())
            .expect("list notifications")
            .notifications
    }

    fn agent_state_event(session_id: &str, activity: AgentActivity) -> Event {
        crate::events::event(
            event::AGENT_STATE,
            json!({
                "session_id": session_id,
                "activity": activity,
                "source": StateSource::Report,
            }),
        )
    }

    fn session_event(event_name: &str, info: &SessionInfo) -> Event {
        crate::events::event(event_name, json!({ "session": info }))
    }

    fn session_info(session_id: &str, state: SessionState, exit_code: Option<i32>) -> SessionInfo {
        SessionInfo {
            id: SessionId(session_id.to_owned()),
            external: Some(false),
            name: None,
            agent: "codex".to_owned(),
            agent_base: RuntimeRef::codex(),
            cwd: std::path::PathBuf::from("/workspace/project"),
            cwd_source: Some(protocol::CwdSource::Launch),
            pid: 1234,
            runtime: None,
            cols: 120,
            rows: 40,
            state,
            state_source: StateSource::Process,
            activity: None,
            subagents: Vec::new(),
            background: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: None,
            native_session_path: None,
            native_last_activity_at: None,
            project_id: Some("p-test".to_owned()),
            project_label: None,
            is_linked_worktree: Some(false),
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            capability_refusals: None,
            output_force_closed: false,
            metadata: BTreeMap::new(),
            created_at: "2026-07-03T10:00:00Z".to_owned(),
            updated_at: "2026-07-03T10:01:00Z".to_owned(),
            exit_code,
            capabilities: protocol::SessionCapabilities::default(),
        }
    }

    fn enable_session_finished(service: &NotificationService) {
        let mut policy = default_policy();
        policy.enabled = NotificationKindPolicy {
            session_finished: true,
            ..policy.enabled
        };
        policy.providers.clear();
        service.set_policy(policy).expect("set notification policy");
    }

    fn enable_turn_completed(service: &NotificationService) {
        let mut policy = default_policy();
        policy.enabled = NotificationKindPolicy {
            turn_completed: true,
            ..policy.enabled
        };
        policy.providers.clear();
        service.set_policy(policy).expect("set notification policy");
    }

    fn provider_approval_params(session_id: &str) -> NotificationCreateParams {
        let session_id = SessionId(session_id.to_owned());
        NotificationCreateParams {
            source: NotificationSource {
                provider: "codex".to_owned(),
                provider_event: "PermissionRequest".to_owned(),
                host_local_source_id: "codex-permission-s-1".to_owned(),
            },
            kind: NotificationKind::ApprovalRequired,
            severity: NotificationSeverity::ActionRequired,
            title: "Approval required".to_owned(),
            body: "Codex needs approval.".to_owned(),
            metadata: BTreeMap::new(),
            session_id: Some(session_id.clone()),
            agent_kind: Some(RuntimeRef::codex()),
            source_id: Some("codex:s-1:permission:1".to_owned()),
            dedupe_key: Some(attention_dedupe_key(&session_id)),
            project_id: Some("p-test".to_owned()),
            retryable_admission: false,
        }
    }

    fn provider_turn_params(session_id: &str) -> NotificationCreateParams {
        let session_id = SessionId(session_id.to_owned());
        NotificationCreateParams {
            source: NotificationSource {
                provider: "codex".to_owned(),
                provider_event: "Stop".to_owned(),
                host_local_source_id: "codex-stop-s-1".to_owned(),
            },
            kind: NotificationKind::TurnCompleted,
            severity: NotificationSeverity::Info,
            title: "Turn completed".to_owned(),
            body: "Codex completed a turn.".to_owned(),
            metadata: BTreeMap::new(),
            session_id: Some(session_id.clone()),
            agent_kind: Some(RuntimeRef::codex()),
            source_id: Some("codex:s-1:stop:1".to_owned()),
            dedupe_key: Some(format!("turn:{}", session_id.0)),
            project_id: Some("p-test".to_owned()),
            retryable_admission: false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn projector_blocking_helper_runs_store_create_off_runtime_worker() {
        let service = service("blocking-helper");
        let params = provider_approval_params("s-blocking-helper");
        let runtime_thread = std::thread::current().id();

        let blocking_thread = run_projector_blocking(move || {
            service
                .create(params)
                .expect("blocking helper creates notification");
            std::thread::current().id()
        })
        .await
        .expect("blocking helper task joins");

        assert_ne!(
            blocking_thread, runtime_thread,
            "projector store writes must not run on the async runtime worker"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_transition_defers_then_flushes_agent_blocked_notification() {
        let service = service("blocked");
        let (attention, _task) = spawn_coordinator(&service);
        let mut projector = ProjectorState::default();

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Blocked),
            )
            .await;
        settle().await;
        assert!(
            list(&service).is_empty(),
            "a derived agent_blocked notification is debounced, not immediately visible"
        );

        advance_past_debounce().await;
        settle().await;

        let notifications = list(&service);
        assert_eq!(notifications.len(), 1);
        let record = &notifications[0];
        assert_eq!(record.kind, NotificationKind::AgentBlocked);
        assert_eq!(record.severity, NotificationSeverity::ActionRequired);
        assert_eq!(record.status, NotificationStatus::Unread);
        assert_eq!(record.session_id, Some(SessionId("s-1".to_owned())));
        assert_eq!(record.source.provider, "pohunek");
        assert_eq!(record.source.provider_event, event::AGENT_STATE);
        assert_eq!(
            record.source_id.as_deref(),
            Some("projector:s-1:agent_blocked:1")
        );
        assert_eq!(record.dedupe_key.as_deref(), Some("attention:s-1"));
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_then_working_within_window_suppresses_notification() {
        let service = service("resolve-projector");
        let (attention, _task) = spawn_coordinator(&service);
        let mut projector = ProjectorState::default();

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Blocked),
            )
            .await;
        settle().await;
        assert!(
            list(&service).is_empty(),
            "the derived agent_blocked notification is still pending"
        );

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Working),
            )
            .await;
        settle().await;
        advance_past_debounce().await;
        settle().await;

        assert!(
            list(&service).is_empty(),
            "a debounced agent_blocked must be suppressed when the session resumes in-window"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn working_acknowledges_provider_hook_attention_notification() {
        let service = service("resolve-hook");
        let (attention, _task) = spawn_coordinator(&service);
        let mut projector = ProjectorState::default();

        // A provider hook creates the attention notification directly (visible),
        // so the projector never observes a blocked activity edge for this session.
        service
            .create(provider_approval_params("s-1"))
            .expect("create provider approval notification");
        let created = list(&service);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].kind, NotificationKind::ApprovalRequired);
        assert_eq!(created[0].status, NotificationStatus::Unread);

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Working),
            )
            .await;
        settle().await;

        let resolved = list(&service);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].status, NotificationStatus::Acknowledged);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_keeps_provider_approval_notification_active() {
        let service = service("keep-hook-idle");
        let (attention, _task) = spawn_coordinator(&service);
        let mut projector = ProjectorState::default();

        service
            .create(provider_approval_params("s-1"))
            .expect("create provider approval notification");
        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Idle),
            )
            .await;
        settle().await;

        let active = list(&service);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].status, NotificationStatus::Unread);
    }

    #[tokio::test(start_paused = true)]
    async fn working_acknowledges_provider_hook_turn_notification() {
        let service = service("resolve-turn-hook");
        enable_turn_completed(&service);
        let (attention, _task) = spawn_coordinator(&service);
        let mut projector = ProjectorState::default();

        service
            .create(provider_turn_params("s-1"))
            .expect("create provider turn notification");
        let created = list(&service);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].kind, NotificationKind::TurnCompleted);
        assert_eq!(created[0].status, NotificationStatus::Unread);

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Working),
            )
            .await;
        settle().await;

        let resolved = list(&service);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].status, NotificationStatus::Acknowledged);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_blocked_events_during_one_period_do_not_duplicate() {
        let service = service("blocked-dedupe");
        let (attention, _task) = spawn_coordinator(&service);
        let mut projector = ProjectorState::default();

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Blocked),
            )
            .await;
        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Blocked),
            )
            .await;
        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Blocked),
            )
            .await;
        settle().await;
        advance_past_debounce().await;
        settle().await;

        assert_eq!(list(&service).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn provider_approval_suppresses_projector_blocked_for_same_attention_key() {
        let service = service("provider-suppresses-projector");
        let (attention, _task) = spawn_coordinator(&service);
        let provider = service
            .create(provider_approval_params("s-1"))
            .expect("provider create");
        let mut projector = ProjectorState::default();

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Blocked),
            )
            .await;
        settle().await;
        advance_past_debounce().await;
        settle().await;

        let notifications = list(&service);
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].id, provider.record.id);
        assert_eq!(notifications[0].kind, NotificationKind::ApprovalRequired);
        assert_eq!(notifications[0].source.provider, "codex");
    }

    #[tokio::test]
    async fn failed_session_update_creates_error_with_exit_code() {
        let service = service("failed");
        let attention = AttentionCoordinator::disconnected();
        let mut projector = ProjectorState::default();

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &session_event(
                    event::SESSION_UPDATED,
                    &session_info("s-1", SessionState::Failed, Some(42)),
                ),
            )
            .await;

        let notifications = list(&service);
        assert_eq!(notifications.len(), 1);
        let record = &notifications[0];
        assert_eq!(record.kind, NotificationKind::Error);
        assert_eq!(record.severity, NotificationSeverity::Error);
        assert_eq!(record.source_id.as_deref(), Some("projector:s-1:error:1"));
        assert!(
            record.body.contains("exit code 42"),
            "body must include the safe exit code value: {}",
            record.body
        );
        assert_eq!(
            record.metadata.get("reason").map(String::as_str),
            Some("exit_code=42")
        );
    }

    #[tokio::test]
    async fn done_session_update_creates_session_finished_when_policy_enabled() {
        let service = service("done-enabled");
        enable_session_finished(&service);
        let attention = AttentionCoordinator::disconnected();
        let mut projector = ProjectorState::default();

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &session_event(
                    event::SESSION_UPDATED,
                    &session_info("s-1", SessionState::Done, Some(0)),
                ),
            )
            .await;

        let notifications = list(&service);
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].kind, NotificationKind::SessionFinished);
        assert_eq!(notifications[0].severity, NotificationSeverity::Success);
        assert_eq!(
            notifications[0].source_id.as_deref(),
            Some("projector:s-1:session_finished:1")
        );
    }

    #[tokio::test]
    async fn stopped_session_events_do_not_create_error_notifications() {
        let service = service("stopped");
        let attention = AttentionCoordinator::disconnected();
        let mut projector = ProjectorState::default();

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &session_event(
                    event::SESSION_STOPPED,
                    &session_info("s-1", SessionState::Stopped, None),
                ),
            )
            .await;
        projector
            .handle_event_blocking(
                &service,
                &attention,
                &session_event(
                    event::SESSION_UPDATED,
                    &session_info("s-2", SessionState::Stopped, None),
                ),
            )
            .await;

        assert!(list(&service).is_empty());
    }

    #[tokio::test]
    async fn disabled_policy_kinds_do_not_create_records() {
        let service = service("done-disabled");
        let attention = AttentionCoordinator::disconnected();
        let mut projector = ProjectorState::default();

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &session_event(
                    event::SESSION_UPDATED,
                    &session_info("s-1", SessionState::Done, Some(0)),
                ),
            )
            .await;

        assert!(list(&service).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn source_ids_are_deterministic_per_kind_transition_epoch() {
        let service = service("source-id");
        let (attention, _task) = spawn_coordinator(&service);
        let mut projector = ProjectorState::default();

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Blocked),
            )
            .await;
        settle().await;
        advance_past_debounce().await;
        settle().await;

        let source_ids = list(&service)
            .into_iter()
            .map(|record| record.source_id.expect("projector source id"))
            .collect::<Vec<_>>();
        assert_eq!(source_ids, vec!["projector:s-1:agent_blocked:1".to_owned()]);
        assert_eq!(
            projector_source_id(
                &SessionId("s-2".to_owned()),
                NotificationKind::Error,
                TransitionKey::Epoch(3)
            ),
            "projector:s-2:error:3"
        );
    }

    /// One blocked transition of session `s-1`, flushed past the debounce.
    async fn block_and_resume(
        projector: &mut ProjectorState,
        service: &NotificationService,
        attention: &AttentionCoordinator,
    ) {
        projector
            .handle_event_blocking(
                service,
                attention,
                &agent_state_event("s-1", AgentActivity::Blocked),
            )
            .await;
        settle().await;
        advance_past_debounce().await;
        settle().await;
        projector
            .handle_event_blocking(
                service,
                attention,
                &agent_state_event("s-1", AgentActivity::Working),
            )
            .await;
        settle().await;
    }

    fn agent_blocked_records(service: &NotificationService) -> usize {
        list(service)
            .iter()
            .filter(|record| record.kind == NotificationKind::AgentBlocked)
            .count()
    }

    #[tokio::test(start_paused = true)]
    async fn a_transition_after_a_daemon_restart_surfaces_beside_the_records_before_it() {
        let dir = temp_data_dir("restart");
        let before_restart = {
            let service = NotificationService::open(&dir).expect("open notification service");
            let (attention, _task) = spawn_coordinator(&service);
            let mut projector = ProjectorState::default();
            for _ in 0..2 {
                block_and_resume(&mut projector, &service, &attention).await;
            }
            agent_blocked_records(&service)
        };
        assert_eq!(before_restart, 2);

        // A restarted daemon opens the same store with a fresh projector.
        let service = NotificationService::open(&dir).expect("reopen notification service");
        let (attention, _task) = spawn_coordinator(&service);
        let mut projector = ProjectorState::default();
        block_and_resume(&mut projector, &service, &attention).await;
        assert_eq!(
            agent_blocked_records(&service),
            3,
            "the first transition after the restart is a new record: {:#?}",
            list(&service)
        );
    }

    #[tokio::test]
    async fn a_stored_projector_key_at_the_epoch_limit_drops_the_next_transition_without_panicking()
    {
        let service = service("epoch-limit");
        service
            .create(NotificationCreateParams {
                source: NotificationSource {
                    provider: super::PROJECTOR_PROVIDER.to_owned(),
                    provider_event: event::SESSION_UPDATED.to_owned(),
                    host_local_source_id: "stored".to_owned(),
                },
                kind: NotificationKind::Error,
                severity: NotificationSeverity::Error,
                title: "Session failed".to_owned(),
                body: "Session s-1 failed.".to_owned(),
                metadata: BTreeMap::new(),
                session_id: Some(SessionId("s-1".to_owned())),
                agent_kind: None,
                source_id: Some(projector_source_id(
                    &SessionId("s-1".to_owned()),
                    NotificationKind::Error,
                    TransitionKey::Epoch(u64::MAX),
                )),
                dedupe_key: None,
                project_id: None,
                retryable_admission: false,
            })
            .expect("store a record at the epoch limit");
        let attention = AttentionCoordinator::disconnected();
        let mut projector = ProjectorState::default();

        projector
            .handle_event_blocking(
                &service,
                &attention,
                &session_event(
                    event::SESSION_UPDATED,
                    &session_info("s-1", SessionState::Failed, Some(1)),
                ),
            )
            .await;

        assert_eq!(
            list(&service).len(),
            1,
            "an exhausted epoch creates nothing rather than reuse a key"
        );
    }

    /// `session_info` of a session whose worker runtime is in `generation`.
    fn session_in_generation(
        session_id: &str,
        state: SessionState,
        exit_code: Option<i32>,
        generation: u64,
    ) -> SessionInfo {
        session_started_in_generation(session_id, state, exit_code, generation, None)
    }

    /// `session_in_generation` whose generation started at `started_at`.
    fn session_started_in_generation(
        session_id: &str,
        state: SessionState,
        exit_code: Option<i32>,
        generation: u64,
        started_at: Option<&str>,
    ) -> SessionInfo {
        SessionInfo {
            runtime: Some(SessionRuntime {
                state: RuntimeState::Terminal,
                runtime_generation: RuntimeGeneration::new(generation),
                worker_id: None,
                worker_instance_id: None,
                started_at: started_at.map(str::to_owned),
                last_connected_at: None,
                loss_reason: None,
                exit: None,
            }),
            ..session_info(session_id, state, exit_code)
        }
    }

    async fn project_session_update(
        projector: &mut ProjectorState,
        service: &NotificationService,
        session: &SessionInfo,
    ) {
        projector
            .handle_event_blocking(
                service,
                &AttentionCoordinator::disconnected(),
                &session_event(event::SESSION_UPDATED, session),
            )
            .await;
    }

    fn records_of(service: &NotificationService, kind: NotificationKind) -> usize {
        list(service)
            .iter()
            .filter(|record| record.kind == kind)
            .count()
    }

    #[tokio::test]
    async fn a_terminal_state_replayed_after_a_restart_is_not_reported_again() {
        let dir = temp_data_dir("terminal-replay");
        let failed = session_in_generation("s-1", SessionState::Failed, Some(1), 1);
        let done = session_in_generation("s-2", SessionState::Done, Some(0), 1);
        let service = NotificationService::open(&dir).expect("open notification service");
        enable_session_finished(&service);
        let mut projector = ProjectorState::default();
        project_session_update(&mut projector, &service, &failed).await;
        project_session_update(&mut projector, &service, &done).await;
        drop(service);

        // A restarted daemon reconciles the sessions and publishes their
        // terminal states again.
        let service = NotificationService::open(&dir).expect("reopen notification service");
        let mut projector = ProjectorState::default();
        project_session_update(&mut projector, &service, &failed).await;
        project_session_update(&mut projector, &service, &done).await;
        assert_eq!(
            (
                records_of(&service, NotificationKind::Error),
                records_of(&service, NotificationKind::SessionFinished)
            ),
            (1, 1),
            "a replayed terminal state keeps its record: {:#?}",
            list(&service)
        );

        // The session is relaunched into its next runtime generation and fails.
        project_session_update(
            &mut projector,
            &service,
            &session_in_generation("s-1", SessionState::Running, None, 2),
        )
        .await;
        project_session_update(
            &mut projector,
            &service,
            &session_in_generation("s-1", SessionState::Failed, Some(1), 2),
        )
        .await;
        assert_eq!(records_of(&service, NotificationKind::Error), 2);
    }

    #[tokio::test]
    async fn a_terminal_record_the_owner_deleted_stays_deleted_after_a_restart() {
        let dir = temp_data_dir("terminal-deleted");
        let failed = session_in_generation("s-1", SessionState::Failed, Some(1), 1);
        let service = NotificationService::open(&dir).expect("open notification service");
        let mut projector = ProjectorState::default();
        project_session_update(&mut projector, &service, &failed).await;
        let record = list(&service).pop().expect("the failure is reported");
        service
            .delete(NotificationDeleteParams { id: record.id })
            .expect("delete the failure record");
        drop(service);

        let service = NotificationService::open(&dir).expect("reopen notification service");
        let mut projector = ProjectorState::default();
        project_session_update(&mut projector, &service, &failed).await;
        assert!(
            list(&service).is_empty(),
            "a replayed terminal state does not bring back a deleted record: {:#?}",
            list(&service)
        );
    }

    fn terminal_source_store(dir: &std::path::Path) -> crate::store::Store {
        use std::os::unix::fs::PermissionsExt as _;
        // Metadata authority requires the same owner-private root as production.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .expect("private source root");
        crate::store::Store::new(dir.join(pohunek_paths::METADATA_STORE_NAME))
    }

    fn persist_terminal_source(store: &crate::store::Store, info: &SessionInfo) {
        let record = crate::store::SessionRecord {
            schema_version: crate::store::STORE_SCHEMA_VERSION,
            session_id: info.id.0.clone(),
            desired_state: crate::store::DesiredState::Stopped,
            transaction: None,
            info: info.clone(),
            recovery: None,
            native_identity_ordering: None,
            runtime: crate::store::RuntimeRecord {
                state: protocol::RuntimeState::Terminal,
                worker_id: None,
                worker_instance_id: None,
                service_id: None,
                generation: info
                    .runtime
                    .as_ref()
                    .map(|runtime| runtime.runtime_generation.to_string()),
                executable: None,
                reason: None,
            },
            active_agent_origin: None,
        };
        assert_eq!(
            store
                .record_session(&record)
                .expect("persist terminal source"),
            crate::store::SessionWriteOutcome::Applied,
        );
    }

    fn compact_deleted_terminals(service: &NotificationService) {
        let mut policy = service.policy();
        policy.retention.compaction_min_actions = 1;
        service
            .set_policy(policy)
            .expect("set compaction threshold");
        service
            .prune_retention(&protocol::NotificationRetentionParams {
                dry_run: false,
                status: None,
                before: None,
                limit: None,
            })
            .expect("compact deleted notifications");
    }

    fn delete_visible_terminals(service: &NotificationService) {
        for record in list(service) {
            service
                .delete(NotificationDeleteParams { id: record.id })
                .expect("delete terminal");
        }
    }

    #[tokio::test]
    async fn deleted_terminal_generations_survive_compaction_and_replay() {
        let dir = temp_data_dir("terminal-compaction-replay");
        let service = NotificationService::open(&dir).expect("open notification service");
        enable_session_finished(&service);
        let store = terminal_source_store(&dir);
        let sessions = [
            session_in_generation("s-1", SessionState::Failed, Some(1), 1),
            session_in_generation("s-2", SessionState::Done, Some(0), 1),
        ];
        let mut projector = ProjectorState::default();
        for session in &sessions {
            persist_terminal_source(&store, session);
            project_session_update(&mut projector, &service, session).await;
        }
        assert_eq!(list(&service).len(), 2);
        // Generic source dedupe permits recreation after deletion. Multiple
        // deleted snapshots of that same terminal source must still compact to one.
        let error = list(&service)
            .into_iter()
            .find(|record| record.kind == NotificationKind::Error)
            .expect("error record");
        service
            .delete(NotificationDeleteParams { id: error.id })
            .expect("delete first error");
        service
            .create(NotificationCreateParams {
                source: error.source,
                kind: error.kind,
                severity: error.severity,
                title: error.title,
                body: error.body,
                metadata: error.metadata,
                session_id: error.session_id,
                agent_kind: error.agent_kind,
                source_id: error.source_id,
                dedupe_key: error.dedupe_key,
                project_id: error.project_id,
                retryable_admission: false,
            })
            .expect("recreate same deleted source");
        delete_visible_terminals(&service);
        compact_deleted_terminals(&service);
        assert_eq!(
            std::fs::read_to_string(service.store_path())
                .expect("compacted log")
                .lines()
                .count(),
            2
        );
        drop(service);
        let service = NotificationService::open(&dir).expect("reopen compacted service");
        let mut events = service.subscribe();
        let mut projector = ProjectorState::default();
        for session in &sessions {
            project_session_update(&mut projector, &service, session).await;
        }
        assert!(
            list(&service).is_empty(),
            "compaction must not resurrect deleted terminals"
        );
        assert!(
            events.try_recv().is_err(),
            "replay emits no notification event"
        );
        assert!(!service
            .inner
            .store
            .compact_if_needed(1)
            .expect("repeat compaction"));
    }

    #[tokio::test]
    async fn terminal_tombstones_are_bounded_by_current_durable_sources() {
        let dir = temp_data_dir("terminal-compaction-bound");
        let service = NotificationService::open(&dir).expect("open service");
        enable_session_finished(&service);
        let store = terminal_source_store(&dir);
        let mut projector = ProjectorState::default();
        let remaining = session_in_generation("s-2", SessionState::Done, Some(0), 1);
        persist_terminal_source(&store, &remaining);
        project_session_update(&mut projector, &service, &remaining).await;
        for generation in 1..=4 {
            let failed = session_in_generation("s-1", SessionState::Failed, Some(1), generation);
            persist_terminal_source(&store, &failed);
            // A fresh projector also exercises durable generation seeding.
            let mut projector = ProjectorState::default();
            project_session_update(&mut projector, &service, &failed).await;
            assert_eq!(records_of(&service, NotificationKind::Error), 1);
            delete_visible_terminals(&service);
            compact_deleted_terminals(&service);
            let log = std::fs::read_to_string(service.store_path()).expect("read compacted log");
            assert_eq!(
                log.lines().count(),
                2,
                "historical generations do not accumulate"
            );
            assert!(log.contains(&format!("projector:s-1:error:g{generation}")));
            assert!(log.contains("projector:s-2:session_finished:g1"));
        }
        store.remove_session("s-1").expect("remove source session");
        compact_deleted_terminals(&service);
        let log = std::fs::read_to_string(service.store_path()).expect("read bounded log");
        assert!(
            !log.contains("projector:s-1:"),
            "removed source tombstone is physically gone"
        );
        assert_eq!(log.lines().count(), 1);
        drop(service);
        let service = NotificationService::open(&dir).expect("reopen after source removal");
        let mut events = service.subscribe();
        let mut projector = ProjectorState::default();
        for source in store.load_sessions().expect("remaining durable sources") {
            project_session_update(&mut projector, &service, &source.info).await;
        }
        assert!(
            list(&service).is_empty(),
            "remaining source stays deleted after restart"
        );
        assert!(
            events.try_recv().is_err(),
            "removed source cannot resurrect on durable replay"
        );
        store
            .remove_session("s-2")
            .expect("remove remaining source");
        compact_deleted_terminals(&service);
        assert_eq!(
            std::fs::metadata(service.store_path())
                .expect("log metadata")
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn terminal_compaction_preserves_epoch_bridge() {
        let dir = temp_data_dir("terminal-compaction-epoch");
        let service = NotificationService::open(&dir).expect("open service");
        let store = terminal_source_store(&dir);
        let session = session_started_in_generation(
            "s-1",
            SessionState::Failed,
            Some(1),
            1,
            Some("2026-07-03T10:00:00Z"),
        );
        persist_terminal_source(&store, &session);
        store_epoch_keyed_error_record(&service, "2026-07-03T10:05:00Z");
        delete_visible_terminals(&service);
        compact_deleted_terminals(&service);
        drop(service);
        let service = NotificationService::open(&dir).expect("reopen compacted epoch record");
        let mut projector = ProjectorState::default();
        project_session_update(&mut projector, &service, &session).await;
        assert!(
            list(&service).is_empty(),
            "legacy epoch key still reports this generation"
        );
    }

    async fn compact_with_incomplete_sources(tag: &str, tail: &str) {
        let dir = temp_data_dir(tag);
        let service = NotificationService::open(&dir).expect("open service");
        let store = terminal_source_store(&dir);
        let current = session_in_generation("s-1", SessionState::Failed, Some(1), 2);
        persist_terminal_source(&store, &current);
        for generation in 1..=2 {
            let mut projector = ProjectorState::default();
            project_session_update(
                &mut projector,
                &service,
                &session_in_generation("s-1", SessionState::Failed, Some(1), generation),
            )
            .await;
            delete_visible_terminals(&service);
        }
        let path = dir.join(pohunek_paths::METADATA_STORE_NAME);
        let metadata = std::fs::read(&path).expect("source snapshot");
        let mut incomplete = metadata.clone();
        incomplete.extend_from_slice(tail.as_bytes());
        std::fs::write(&path, incomplete).expect("append rejected source data");
        let before = std::fs::read_to_string(service.store_path()).expect("original log");
        let logs = crate::runtime::lifecycle::tests::LogCapture::default();
        let _subscriber = logs.install();
        compact_deleted_terminals(&service);
        let compacted = std::fs::read_to_string(service.store_path()).expect("compacted log");
        assert!(
            compacted.len() < before.len(),
            "history still compacts with incomplete sources"
        );
        assert_eq!(
            compacted.lines().count(),
            2,
            "keep every terminal tombstone, not only current generation"
        );
        assert!(compacted.contains("projector:s-1:error:g1"));
        assert!(compacted.contains("projector:s-1:error:g2"));
        compact_deleted_terminals(&service);
        assert_eq!(
            logs.text()
                .lines()
                .filter(|line| line.contains("WARN"))
                .count(),
            1,
            "warn once rather than once per maintenance sweep: {}",
            logs.text()
        );
        drop(service);
        let service = NotificationService::open(&dir).expect("reopen conservative compacted log");
        let mut projector = ProjectorState::default();
        project_session_update(&mut projector, &service, &current).await;
        assert!(
            list(&service).is_empty(),
            "current terminal stays deleted after replay"
        );
        std::fs::write(&path, metadata).expect("repair source metadata");
        compact_deleted_terminals(&service);
        let repaired =
            std::fs::read_to_string(service.store_path()).expect("bounded log after repair");
        assert_eq!(
            repaired.lines().count(),
            1,
            "complete sources restore the generation bound"
        );
        assert!(!repaired.contains("projector:s-1:error:g1"));
    }

    #[tokio::test]
    async fn compaction_keeps_all_terminal_tombstones_with_rejected_metadata() {
        compact_with_incomplete_sources(
            "terminal-rejected-source",
            &format!(
                "{{\"kind\":\"future_kind\",\"schema_version\":{}}}\n",
                crate::store::STORE_SCHEMA_VERSION
            ),
        )
        .await;
    }

    #[tokio::test]
    async fn compaction_keeps_all_terminal_tombstones_with_torn_metadata_tail() {
        // No newline or closing brace: a registry write stopped mid-record.
        compact_with_incomplete_sources("terminal-torn-source", "{\"kind\":\"session\"").await;
    }

    /// Stores the projector's `agent_blocked` record of session `s-1` from a
    /// block reported long before the restart, outside the attention dedupe
    /// window.
    fn store_earlier_blocked_record(service: &NotificationService) -> protocol::NotificationId {
        let session_id = SessionId("s-1".to_owned());
        let source_id = projector_source_id(
            &session_id,
            NotificationKind::AgentBlocked,
            TransitionKey::Epoch(1),
        );
        service
            .create_at(
                NotificationCreateParams {
                    source: NotificationSource {
                        provider: super::PROJECTOR_PROVIDER.to_owned(),
                        provider_event: event::AGENT_STATE.to_owned(),
                        host_local_source_id: source_id.clone(),
                    },
                    kind: NotificationKind::AgentBlocked,
                    severity: NotificationSeverity::ActionRequired,
                    title: "Agent needs attention".to_owned(),
                    body: "Session s-1 is waiting for owner attention.".to_owned(),
                    metadata: BTreeMap::new(),
                    session_id: Some(session_id.clone()),
                    agent_kind: None,
                    source_id: Some(source_id),
                    dedupe_key: Some(attention_dedupe_key(&session_id)),
                    project_id: None,
                    retryable_admission: false,
                },
                "2026-07-03T10:00:00Z",
            )
            .expect("store the earlier blocked record")
            .record
            .id
    }

    /// The blocked state of session `s-1` a restarted daemon publishes,
    /// flushed past the debounce.
    async fn replay_blocked_after_restart(service: &NotificationService) {
        let (attention, _task) = spawn_coordinator(service);
        let mut projector = ProjectorState::default();
        projector
            .handle_event_blocking(
                service,
                &attention,
                &agent_state_event("s-1", AgentActivity::Blocked),
            )
            .await;
        settle().await;
        advance_past_debounce().await;
        settle().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_session_replayed_after_a_restart_keeps_its_unread_record() {
        let dir = temp_data_dir("blocked-replay-unread");
        store_earlier_blocked_record(
            &NotificationService::open(&dir).expect("open notification service"),
        );

        let service = NotificationService::open(&dir).expect("reopen notification service");
        replay_blocked_after_restart(&service).await;
        assert_eq!(
            agent_blocked_records(&service),
            1,
            "the unread record already surfaces the blocked state: {:#?}",
            list(&service)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_session_whose_record_was_acknowledged_is_raised_again_after_a_restart() {
        let dir = temp_data_dir("blocked-replay-acknowledged");
        let service = NotificationService::open(&dir).expect("open notification service");
        let id = store_earlier_blocked_record(&service);
        service
            .update(NotificationUpdateParams {
                id,
                status: NotificationStatus::Acknowledged,
            })
            .expect("acknowledge the blocked record");
        drop(service);

        let service = NotificationService::open(&dir).expect("reopen notification service");
        replay_blocked_after_restart(&service).await;
        assert_eq!(
            agent_blocked_records(&service),
            2,
            "a session still blocked after its record was acknowledged is raised again: {:#?}",
            list(&service)
        );
    }

    /// Stores the `error` record of session `s-1` that a daemon keying
    /// terminal records by transition epoch created at `created_at`.
    fn store_epoch_keyed_error_record(service: &NotificationService, created_at: &str) {
        let session_id = SessionId("s-1".to_owned());
        let source_id = projector_source_id(
            &session_id,
            NotificationKind::Error,
            TransitionKey::Epoch(1),
        );
        service
            .create_at(
                NotificationCreateParams {
                    source: NotificationSource {
                        provider: super::PROJECTOR_PROVIDER.to_owned(),
                        provider_event: event::SESSION_UPDATED.to_owned(),
                        host_local_source_id: source_id.clone(),
                    },
                    kind: NotificationKind::Error,
                    severity: NotificationSeverity::Error,
                    title: "Session failed".to_owned(),
                    body: "Session s-1 failed with exit code 1.".to_owned(),
                    metadata: BTreeMap::new(),
                    session_id: Some(session_id),
                    agent_kind: None,
                    source_id: Some(source_id),
                    dedupe_key: None,
                    project_id: None,
                    retryable_admission: false,
                },
                created_at,
            )
            .expect("store the epoch-keyed error record");
    }

    #[tokio::test]
    async fn a_failure_an_epoch_keyed_record_reports_is_not_reported_again_after_an_upgrade() {
        let dir = temp_data_dir("epoch-keyed-same-generation");
        store_epoch_keyed_error_record(
            &NotificationService::open(&dir).expect("open notification service"),
            "2026-07-03T10:05:00Z",
        );

        let service = NotificationService::open(&dir).expect("reopen notification service");
        let mut projector = ProjectorState::default();
        project_session_update(
            &mut projector,
            &service,
            &session_started_in_generation(
                "s-1",
                SessionState::Failed,
                Some(1),
                1,
                Some("2026-07-03T10:00:00Z"),
            ),
        )
        .await;
        assert_eq!(
            records_of(&service, NotificationKind::Error),
            1,
            "the record from before the upgrade reports this failure: {:#?}",
            list(&service)
        );
    }

    #[tokio::test]
    async fn a_failure_in_a_generation_started_after_an_epoch_keyed_record_is_reported() {
        let dir = temp_data_dir("epoch-keyed-next-generation");
        store_epoch_keyed_error_record(
            &NotificationService::open(&dir).expect("open notification service"),
            "2026-07-03T10:05:00Z",
        );

        // The session was relaunched and failed again while the daemon was
        // down; its worker outlives the daemon.
        let service = NotificationService::open(&dir).expect("reopen notification service");
        let mut projector = ProjectorState::default();
        project_session_update(
            &mut projector,
            &service,
            &session_started_in_generation(
                "s-1",
                SessionState::Failed,
                Some(1),
                2,
                Some("2026-07-03T11:00:00Z"),
            ),
        )
        .await;
        assert_eq!(
            records_of(&service, NotificationKind::Error),
            2,
            "the earlier record reports an earlier generation: {:#?}",
            list(&service)
        );
    }

    async fn observe_activity(
        projector: &mut ProjectorState,
        service: &NotificationService,
        attention: &AttentionCoordinator,
        activity: AgentActivity,
    ) {
        projector
            .handle_event_blocking(service, attention, &agent_state_event("s-1", activity))
            .await;
        settle().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_session_first_seen_idle_after_a_restart_keeps_its_unread_record() {
        let dir = temp_data_dir("blocked-replay-idle-first");
        store_earlier_blocked_record(
            &NotificationService::open(&dir).expect("open notification service"),
        );

        let service = NotificationService::open(&dir).expect("reopen notification service");
        let (attention, _task) = spawn_coordinator(&service);
        let mut projector = ProjectorState::default();
        observe_activity(&mut projector, &service, &attention, AgentActivity::Idle).await;
        observe_activity(&mut projector, &service, &attention, AgentActivity::Blocked).await;
        advance_past_debounce().await;
        settle().await;
        assert_eq!(
            agent_blocked_records(&service),
            1,
            "an idle observation does not end the unread blocked state: {:#?}",
            list(&service)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_record_acknowledged_after_the_restart_lets_the_replay_surface() {
        let dir = temp_data_dir("blocked-replay-acknowledged-later");
        let id = store_earlier_blocked_record(
            &NotificationService::open(&dir).expect("open notification service"),
        );

        let service = NotificationService::open(&dir).expect("reopen notification service");
        let (attention, _task) = spawn_coordinator(&service);
        let mut projector = ProjectorState::default();
        observe_activity(&mut projector, &service, &attention, AgentActivity::Idle).await;
        service
            .update(NotificationUpdateParams {
                id,
                status: NotificationStatus::Acknowledged,
            })
            .expect("acknowledge the blocked record");
        observe_activity(&mut projector, &service, &attention, AgentActivity::Blocked).await;
        advance_past_debounce().await;
        settle().await;
        assert_eq!(
            agent_blocked_records(&service),
            2,
            "the blocked state is raised again once its record is acknowledged: {:#?}",
            list(&service)
        );
    }

    #[tokio::test]
    async fn a_failure_of_a_generation_that_ended_during_a_restart_is_reported() {
        let dir = temp_data_dir("terminal-next-generation");
        let service = NotificationService::open(&dir).expect("open notification service");
        let mut projector = ProjectorState::default();
        project_session_update(
            &mut projector,
            &service,
            &session_in_generation("s-1", SessionState::Failed, Some(1), 1),
        )
        .await;
        drop(service);

        // The session was relaunched and failed again while the daemon was
        // down; the start of its generation is not known.
        let service = NotificationService::open(&dir).expect("reopen notification service");
        let mut projector = ProjectorState::default();
        project_session_update(
            &mut projector,
            &service,
            &session_in_generation("s-1", SessionState::Failed, Some(1), 2),
        )
        .await;
        assert_eq!(
            records_of(&service, NotificationKind::Error),
            2,
            "each generation's failure is reported: {:#?}",
            list(&service)
        );
    }

    #[tokio::test]
    async fn a_failure_of_a_generation_with_an_unknown_start_is_reported_beside_an_epoch_keyed_record(
    ) {
        let dir = temp_data_dir("epoch-keyed-unknown-start");
        store_epoch_keyed_error_record(
            &NotificationService::open(&dir).expect("open notification service"),
            "2026-07-03T10:05:00Z",
        );

        // A resume prepared the next generation, and its worker failed before
        // the daemon recorded when the generation started.
        let service = NotificationService::open(&dir).expect("reopen notification service");
        let mut projector = ProjectorState::default();
        project_session_update(
            &mut projector,
            &service,
            &session_in_generation("s-1", SessionState::Failed, Some(1), 2),
        )
        .await;
        assert_eq!(
            records_of(&service, NotificationKind::Error),
            2,
            "a record cannot be shown to report a generation of unknown start: {:#?}",
            list(&service)
        );
    }
}
