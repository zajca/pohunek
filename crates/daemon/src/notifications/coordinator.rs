//! Session notification debounce coordinator.
//!
//! A single async task owns the lifecycle of debounced session notifications
//! (`agent_blocked`, `approval_required`, and session-scoped `turn_completed`)
//! end to end and is the only place that decides whether such a notification ever
//! becomes visible. Producers (the `notification.create` handler and the
//! session-state projector) do not persist debounced creates directly; they mint
//! the record, hand it to this task via a command channel, and the task holds it
//! *pending* for the policy debounce window. If the session resumes within the
//! window the record is dropped and nothing is ever persisted or broadcast;
//! otherwise it is committed through the store and a `notification_created` event
//! is emitted exactly as an immediate create would be.
//!
//! The same window decides when a session turn event is recorded for
//! `session.wait_turn`. A pending entry therefore carries two candidates: the
//! inbox record (only from a create the notification policy keeps) and the turn
//! signal (only from a trusted producer: the projector, or a provider hook claim
//! admitted for a session the registry holds). A flush commits the inbox record
//! when there is one and records the turn event when there is a signal; a
//! resolve inside the window drops both. A create that adds no inbox record
//! never changes the inbox candidate or its timer, so the inbox behaves exactly
//! as if that create never arrived. The stale-turn suppression after a resolve
//! applies to the inbox only; a `turn_completed` signal is instead dropped at
//! flush time while the session is working again.
//!
//! Pending entries are in-memory only. A daemon restart while an entry is pending
//! (shorter than the debounce window) drops that transient signal, which is
//! acceptable for a sub-10s window; pending state is deliberately not persisted.

// Rust guideline compliant 2026-10-12

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use protocol::{
    NotificationId, NotificationKind, NotificationRecord, NotificationSource, SessionTurnEventKind,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::time::DelayQueue;
use tracing::{debug, warn};

use super::{
    is_turn_dedupe_key, source_priority, DeferredNotification, NotificationService, SourcePriority,
};
use crate::session::{SessionRegistry, TurnEvents};
use protocol::AgentActivity;

/// Maximum time to wait for the coordinator task to stop on shutdown.
///
/// The shutdown path only drops in-memory pending entries and returns; it does no
/// blocking I/O. Five seconds matches the projector and event-log shutdown
/// budgets and bounds a wedged runtime.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Command accepted by the session notification debounce coordinator task.
#[derive(Debug)]
enum Command {
    /// Hold a session notification pending until the debounce window elapses.
    ///
    /// Boxed so the enum's variants stay similarly sized despite the large
    /// [`NotificationRecord`].
    Defer(Box<DeferredNotification>),
    /// Cancel any pending entry for the dedupe key and acknowledge visible records.
    Resolve(String),
    /// Stop the task, dropping all pending entries without persisting them.
    Shutdown,
}

/// Clonable command sink for the session notification debounce coordinator.
///
/// Injected into `DaemonState` and the session-state projector so both producers
/// route debounced session notifications to the single owning task. Cloning is
/// cheap (an [`mpsc::UnboundedSender`] handle).
#[derive(Debug, Clone)]
pub struct AttentionCoordinator {
    commands: mpsc::UnboundedSender<Command>,
}

impl AttentionCoordinator {
    /// Spawn the coordinator task, returning its command handle and task owner.
    ///
    /// The returned [`AttentionCoordinator`] is the clonable producer handle; the
    /// [`AttentionCoordinatorTask`] owns the spawned task and drives its graceful
    /// shutdown. Flushed session signals are recorded as turn events in the
    /// journal of `sessions`.
    #[must_use]
    pub fn spawn(
        notifications: NotificationService,
        sessions: &SessionRegistry,
    ) -> (Self, AttentionCoordinatorTask) {
        // Unbounded: session notification commands are produced at
        // human-interaction rate (an agent blocking, an owner resuming), so
        // growth is not a practical concern, and an unbounded sink lets both the
        // request handler and the projector enqueue without send-side
        // backpressure that could stall them.
        let (commands, receiver) = mpsc::unbounded_channel();
        let handle = tokio::spawn(run(notifications, sessions.clone(), receiver));
        (
            Self {
                commands: commands.clone(),
            },
            AttentionCoordinatorTask { commands, handle },
        )
    }

    /// Defer a session notification through the debounce window.
    ///
    /// The record must already be validated and have its id minted (see
    /// [`NotificationService::prepare_deferred`]). A closed channel means the
    /// coordinator task already stopped (daemon shutdown), where dropping the
    /// record is the correct no-op.
    pub(crate) fn defer(&self, deferred: DeferredNotification) {
        let _ = self.commands.send(Command::Defer(Box::new(deferred)));
    }

    /// Resolve pending and visible session notifications for a dedupe key.
    ///
    /// Cancels a matching pending entry (so it never surfaces) and acknowledges
    /// any already-visible record sharing the key.
    pub fn resolve(&self, dedupe_key: String) {
        let _ = self.commands.send(Command::Resolve(dedupe_key));
    }

    /// Create a handle with no backing task, for tests that never exercise debounce.
    ///
    /// `defer`/`resolve` become silent no-ops because the receiver is dropped, so
    /// synchronous projector tests covering non-attention kinds can pass a handle
    /// without spawning a runtime task.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn disconnected() -> Self {
        let (commands, _receiver) = mpsc::unbounded_channel();
        Self { commands }
    }
}

/// Owns the spawned coordinator task and drives its graceful shutdown.
#[derive(Debug)]
pub struct AttentionCoordinatorTask {
    commands: mpsc::UnboundedSender<Command>,
    handle: JoinHandle<()>,
}

impl AttentionCoordinatorTask {
    /// Signal the coordinator to stop and wait for it to drain.
    ///
    /// Pending entries are dropped without being persisted. Bounded by
    /// [`SHUTDOWN_TIMEOUT`] so a wedged runtime cannot hang daemon exit.
    pub async fn shutdown(self) {
        // A closed channel means the task already stopped; the timeout below then
        // resolves immediately on the finished handle.
        let _ = self.commands.send(Command::Shutdown);
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, self.handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(error = %err, "attention coordinator task failed during shutdown");
            }
            Err(_) => {
                warn!("attention coordinator did not finish within the shutdown timeout");
            }
        }
    }
}

/// A session notification held until its debounce deadline.
#[derive(Debug)]
struct Pending {
    /// Best record the notification policy keeps, committed on flush.
    inbox: Option<NotificationRecord>,
    /// Best trusted record, recorded as a turn event on flush.
    signal: Option<NotificationRecord>,
    /// Monotonic generation stamped when this entry was last (re)armed.
    ///
    /// A [`DelayQueue`] expiry only flushes when its token carries the current
    /// generation, so superseded (re-armed) or resolved entries whose timers
    /// still fire afterwards are ignored instead of surfacing stale records.
    generation: u64,
}

/// Value stored in the [`DelayQueue`] identifying which pending entry a timer arms.
#[derive(Debug)]
struct FlushToken {
    dedupe_key: String,
    generation: u64,
}

/// Drive the coordinator command loop until shutdown.
///
/// Never panics: store errors on the resolve/flush paths are logged and the loop
/// continues, so one failed append cannot take down session notification handling.
async fn run(
    notifications: NotificationService,
    sessions: SessionRegistry,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let turns = sessions.turn_events();
    let mut queue: DelayQueue<FlushToken> = DelayQueue::new();
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut recently_resolved_turns: HashMap<String, Instant> = HashMap::new();
    // Global monotonic generation: never reused, so a token from a resolved and
    // re-deferred key can never be mistaken for a current entry.
    let mut generation: u64 = 0;

    loop {
        tokio::select! {
            // Bias command handling so a burst of defers/resolves is fully
            // applied before an already-armed timer is allowed to flush.
            biased;
            command = commands.recv() => match command {
                Some(Command::Defer(deferred)) => {
                    defer(
                        &mut pending,
                        &mut queue,
                        &mut generation,
                        &mut recently_resolved_turns,
                        &notifications,
                        *deferred,
                    );
                }
                Some(Command::Resolve(dedupe_key)) => {
                    resolve(
                        &mut pending,
                        &mut recently_resolved_turns,
                        &notifications,
                        &dedupe_key,
                    );
                }
                Some(Command::Shutdown) | None => break,
            },
            Some(expired) = queue.next(), if !queue.is_empty() => {
                flush(
                    &mut pending,
                    &notifications,
                    (&sessions, &turns),
                    &expired.into_inner(),
                )
                .await;
            }
        }
    }
}

/// Hold `deferred` pending, applying cross-producer priority and (re)arming the timer.
fn defer(
    pending: &mut HashMap<String, Pending>,
    queue: &mut DelayQueue<FlushToken>,
    generation: &mut u64,
    recently_resolved_turns: &mut HashMap<String, Instant>,
    notifications: &NotificationService,
    deferred: DeferredNotification,
) {
    let DeferredNotification {
        record,
        mut inbox,
        signal,
    } = deferred;
    let Some(dedupe_key) = record.dedupe_key.clone() else {
        // Debounced creates always carry a dedupe key; without one the record
        // cannot be cancelled by session activity, so surface it immediately
        // rather than dropping it. Such a create is never a turn signal: the
        // projector and hook claims always carry a session key.
        if inbox {
            commit(notifications, record);
        }
        return;
    };
    let debounce = Duration::from_secs(u64::from(notifications.policy().attention_debounce_secs));
    prune_recently_resolved_turns(recently_resolved_turns, debounce);
    if inbox
        && record.kind == protocol::NotificationKind::TurnCompleted
        && is_turn_dedupe_key(&dedupe_key)
        && recently_resolved_turns
            .remove(&dedupe_key)
            .is_some_and(|resolved_at| resolved_at.elapsed() <= debounce)
    {
        // A turn report right after the session resumed is the stale stop of
        // the previous turn for the inbox. The turn signal is judged at flush
        // time by the session's activity instead.
        inbox = false;
    }
    if !inbox && !signal {
        return;
    }

    // Provider reports outrank projector reports for the same session moment, so
    // keep the higher-priority record when one is already pending; the store's
    // dedupe remains the final authority at flush time. The inbox candidate
    // only ever considers records the policy keeps, and a create that adds no
    // inbox record re-arms the timer only while no inbox candidate is pending.
    let existing = pending.remove(&dedupe_key);
    let (existing_inbox, existing_signal, existing_generation) = match existing {
        Some(entry) => (entry.inbox, entry.signal, Some(entry.generation)),
        None => (None, None, None),
    };
    let signal_record = if signal {
        Some(prefer(existing_signal, record.clone()))
    } else {
        existing_signal
    };
    let (inbox_record, kept_generation) = if inbox {
        (Some(prefer(existing_inbox, record)), None)
    } else if existing_inbox.is_some() {
        (existing_inbox, existing_generation)
    } else {
        (None, None)
    };

    let generation = kept_generation.unwrap_or_else(|| {
        *generation += 1;
        let current = *generation;
        queue.insert(
            FlushToken {
                dedupe_key: dedupe_key.clone(),
                generation: current,
            },
            debounce,
        );
        current
    });
    pending.insert(
        dedupe_key,
        Pending {
            inbox: inbox_record,
            signal: signal_record,
            generation,
        },
    );
}

/// The pending record to keep when `incoming` arrives for the same key.
fn prefer(
    existing: Option<NotificationRecord>,
    incoming: NotificationRecord,
) -> NotificationRecord {
    match existing {
        Some(existing) if outranks(&existing, &incoming) => existing,
        _ => incoming,
    }
}

/// Cancel any pending entry for `dedupe_key` and acknowledge visible records.
fn resolve(
    pending: &mut HashMap<String, Pending>,
    recently_resolved_turns: &mut HashMap<String, Instant>,
    notifications: &NotificationService,
    dedupe_key: &str,
) {
    // A pending entry that is cancelled never becomes visible. Its DelayQueue
    // timer may still fire, but the generation guard in `flush` makes that a
    // no-op, so the token is left to expire on its own rather than tracked.
    // The stale-turn suppression belongs to the inbox: a cancelled entry that
    // held only a turn signal counts as no pending inbox record, so the
    // resolve still arms the suppression for the next turn report.
    let removed_inbox = pending.remove(dedupe_key).and_then(|entry| entry.inbox);
    if removed_inbox.is_none() && is_turn_dedupe_key(dedupe_key) {
        let debounce =
            Duration::from_secs(u64::from(notifications.policy().attention_debounce_secs));
        prune_recently_resolved_turns(recently_resolved_turns, debounce);
        recently_resolved_turns.insert(dedupe_key.to_owned(), Instant::now());
    }
    // Unify with the already-visible path: acknowledge any surfaced record
    // sharing this session's dedupe key.
    if let Err(error) = notifications.resolve_session_notifications(dedupe_key) {
        warn!(
            dedupe_key,
            error = %error,
            "failed to acknowledge visible session notifications on resolve"
        );
    }
}

fn prune_recently_resolved_turns(
    recently_resolved_turns: &mut HashMap<String, Instant>,
    debounce: Duration,
) {
    recently_resolved_turns.retain(|_, resolved_at| resolved_at.elapsed() <= debounce);
}

/// Commit a pending entry when its timer is still the current generation.
///
/// The inbox record is committed first, so a waiter woken by the turn event
/// already finds the inbox record its event names.
async fn flush(
    pending: &mut HashMap<String, Pending>,
    notifications: &NotificationService,
    (sessions, turns): (&SessionRegistry, &TurnEvents),
    token: &FlushToken,
) {
    let is_current = pending
        .get(&token.dedupe_key)
        .is_some_and(|entry| entry.generation == token.generation);
    if !is_current {
        // Superseded by a later defer or already resolved; the record must not
        // surface from this stale timer.
        return;
    }
    if let Some(entry) = pending.remove(&token.dedupe_key) {
        let committed = entry.inbox.and_then(|record| commit(notifications, record));
        if let Some(signal) = entry.signal {
            record_turn((sessions, turns), &signal, committed.as_ref()).await;
        }
    }
}

/// Persist a debounced record and emit its event, logging (not panicking) on error.
///
/// Returns the visible record the commit produced or deduplicated into.
fn commit(
    notifications: &NotificationService,
    record: NotificationRecord,
) -> Option<NotificationRecord> {
    match notifications.commit_deferred(record) {
        Ok(result) => Some(result.record),
        Err(error) => {
            warn!(error = %error, "failed to commit debounced session notification");
            None
        }
    }
}

/// Record the turn event a flushed session signal stands for.
///
/// A `turn_completed` signal is dropped while the session is working: it is
/// the stale stop of an earlier turn, and the running turn reports its own
/// stop. The event names the committed inbox record only when that record is
/// of the same kind; a signal the policy dropped while another kind surfaced
/// names no record.
async fn record_turn(
    (sessions, turns): (&SessionRegistry, &TurnEvents),
    signal: &NotificationRecord,
    committed: Option<&NotificationRecord>,
) {
    let (Some(session_id), Some(kind)) = (signal.session_id.as_ref(), turn_kind(signal.kind))
    else {
        return;
    };
    if kind == SessionTurnEventKind::TurnCompleted
        && sessions.current_activity(session_id).await == Some(AgentActivity::Working)
    {
        debug!(
            session_id = %session_id.0,
            "stale turn_completed signal dropped while the session works"
        );
        return;
    }
    let notification_id: Option<NotificationId> = committed
        .filter(|record| record.kind == signal.kind)
        .map(|record| record.id.clone());
    turns.record(session_id, kind, notification_id);
}

/// The turn event kind a debounced notification kind stands for.
fn turn_kind(kind: NotificationKind) -> Option<SessionTurnEventKind> {
    match kind {
        NotificationKind::TurnCompleted => Some(SessionTurnEventKind::TurnCompleted),
        NotificationKind::AgentBlocked => Some(SessionTurnEventKind::AgentBlocked),
        NotificationKind::ApprovalRequired => Some(SessionTurnEventKind::ApprovalRequired),
        NotificationKind::SessionFinished | NotificationKind::Error | NotificationKind::System => {
            None
        }
    }
}

/// Whether `existing` should be kept over `incoming` for the same pending key.
///
/// Provider reports outrank projector reports; equal priority takes the incoming
/// record so a fresh report refreshes the pending payload.
fn outranks(existing: &NotificationRecord, incoming: &NotificationRecord) -> bool {
    priority_rank(&existing.source) > priority_rank(&incoming.source)
}

/// Rank a source's priority for pending-buffer conflict resolution.
fn priority_rank(source: &NotificationSource) -> u8 {
    match source_priority(source) {
        SourcePriority::Projector => 0,
        SourcePriority::User => 1,
        SourcePriority::Provider => 2,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use std::time::Duration;

    use protocol::{
        event, Event, NotificationCreateParams, NotificationKind, NotificationKindPolicy,
        NotificationListParams, NotificationRecord, NotificationSeverity, NotificationSource,
        NotificationStatus, SessionId,
    };
    use tokio::sync::broadcast;

    use super::AttentionCoordinator;
    use crate::notifications::{default_policy, DeferredNotification, NotificationService};
    use crate::session::{SessionRegistry, SessionRegistryConfig};

    /// Debounce window used by the tests, advanced past to force a flush.
    const TEST_DEBOUNCE_SECS: u64 = 5;

    fn temp_data_dir(tag: &str) -> std::path::PathBuf {
        // The directory lives until the test's thread ends; the path below it
        // does not exist yet.
        crate::test_support::thread_scoped_dir(&format!("pohunek-attention-coordinator-{tag}-"))
            .join("data")
    }

    fn service(tag: &str) -> NotificationService {
        NotificationService::open(&temp_data_dir(tag)).expect("open notification service")
    }

    fn enable_turn_completed(service: &NotificationService) {
        let mut policy = default_policy();
        policy.enabled = NotificationKindPolicy {
            turn_completed: true,
            ..policy.enabled
        };
        policy.providers.clear();
        service.set_policy(policy).expect("set policy");
    }

    fn projector_params(session: &str) -> NotificationCreateParams {
        params(
            "pohunek",
            "projector.agent_state",
            "projector-s-1",
            NotificationKind::AgentBlocked,
            NotificationSeverity::ActionRequired,
            session,
        )
    }

    fn provider_params(session: &str) -> NotificationCreateParams {
        params(
            "codex",
            "PermissionRequest",
            "codex-hook-s-1",
            NotificationKind::ApprovalRequired,
            NotificationSeverity::ActionRequired,
            session,
        )
    }

    fn turn_params(session: &str) -> NotificationCreateParams {
        let mut params = params(
            "codex",
            "Stop",
            "codex-stop-s-1",
            NotificationKind::TurnCompleted,
            NotificationSeverity::Info,
            session,
        );
        params.title = "Turn completed".to_owned();
        params.body = "Codex completed a turn.".to_owned();
        params.dedupe_key = Some(format!("turn:{session}"));
        params.source_id = Some(format!("codex:{session}:stop:1"));
        params
    }

    fn params(
        provider: &str,
        provider_event: &str,
        host_local_source_id: &str,
        kind: NotificationKind,
        severity: NotificationSeverity,
        session: &str,
    ) -> NotificationCreateParams {
        NotificationCreateParams {
            source: NotificationSource {
                provider: provider.to_owned(),
                provider_event: provider_event.to_owned(),
                host_local_source_id: host_local_source_id.to_owned(),
            },
            kind,
            severity,
            title: "Agent needs attention".to_owned(),
            body: "The agent is waiting for the operator.".to_owned(),
            session_id: Some(SessionId(session.to_owned())),
            agent_kind: None,
            source_id: Some(format!("{provider}:{session}:attention")),
            dedupe_key: Some(format!("attention:{session}")),
            project_id: Some("p-1".to_owned()),
            metadata: BTreeMap::new(),
        }
    }

    fn record(
        service: &NotificationService,
        params: NotificationCreateParams,
    ) -> DeferredNotification {
        service
            .prepare_deferred(params)
            .expect("prepare deferred record")
    }

    /// Spawns a coordinator over a registry that holds no session.
    fn spawn(
        service: &NotificationService,
    ) -> (AttentionCoordinator, super::AttentionCoordinatorTask) {
        AttentionCoordinator::spawn(
            service.clone(),
            &SessionRegistry::new(SessionRegistryConfig::default()),
        )
    }

    fn list(service: &NotificationService) -> Vec<NotificationRecord> {
        service
            .list(NotificationListParams::default())
            .expect("list notifications")
            .notifications
    }

    /// Yield repeatedly so the single coordinator task drains its command channel
    /// and re-arms its timer before the test advances the (paused) clock.
    async fn settle() {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }

    /// Await a `notification_created` event, bounded so a bug fails instead of hanging.
    async fn next_created(events: &mut broadcast::Receiver<Event>) -> Option<NotificationRecord> {
        for _ in 0..128 {
            match events.try_recv() {
                Ok(event) if event.event() == event::NOTIFICATION_CREATED => {
                    let record = serde_json::from_value::<protocol::NotificationCreatedEvent>(
                        event.payload().clone(),
                    )
                    .expect("notification_created payload");
                    return Some(record.record);
                }
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Empty) => tokio::task::yield_now().await,
                Err(_) => return None,
            }
        }
        None
    }

    #[tokio::test(start_paused = true)]
    async fn resolve_before_window_suppresses_entirely() {
        let service = service("suppress");
        let mut events = service.subscribe();
        let (coordinator, _task) = spawn(&service);

        coordinator.defer(record(&service, projector_params("s-1")));
        settle().await;
        coordinator.resolve("attention:s-1".to_owned());
        settle().await;
        tokio::time::advance(Duration::from_secs(TEST_DEBOUNCE_SECS + 1)).await;
        settle().await;

        assert!(
            list(&service).is_empty(),
            "a resolved-before-window attention notification must never be persisted"
        );
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "a suppressed attention notification must not broadcast any event"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resolve_before_window_suppresses_turn_entirely() {
        let service = service("suppress-turn");
        enable_turn_completed(&service);
        let mut events = service.subscribe();
        let (coordinator, _task) = spawn(&service);

        coordinator.defer(record(&service, turn_params("s-1")));
        settle().await;
        coordinator.resolve("turn:s-1".to_owned());
        settle().await;
        tokio::time::advance(Duration::from_secs(TEST_DEBOUNCE_SECS + 1)).await;
        settle().await;

        assert!(
            list(&service).is_empty(),
            "a resolved-before-window turn notification must never be persisted"
        );
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "a suppressed turn notification must not broadcast any event"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resolve_before_defer_suppresses_delayed_turn_within_window() {
        let service = service("resolve-before-turn");
        enable_turn_completed(&service);
        let mut events = service.subscribe();
        let (coordinator, _task) = spawn(&service);

        coordinator.resolve("turn:s-1".to_owned());
        settle().await;
        coordinator.defer(record(&service, turn_params("s-1")));
        settle().await;
        tokio::time::advance(Duration::from_secs(TEST_DEBOUNCE_SECS + 1)).await;
        settle().await;

        assert!(
            list(&service).is_empty(),
            "a turn deferred just after a resolve must be treated as consumed"
        );
        assert!(
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "a consumed delayed turn must not broadcast any event"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_resolve_flushes_after_window() {
        let service = service("flush");
        let mut events = service.subscribe();
        let (coordinator, _task) = spawn(&service);

        coordinator.defer(record(&service, provider_params("s-1")));
        settle().await;
        assert!(
            list(&service).is_empty(),
            "a deferred attention notification must not be visible before the window"
        );

        tokio::time::advance(Duration::from_secs(TEST_DEBOUNCE_SECS + 1)).await;
        let created = next_created(&mut events)
            .await
            .expect("deferred attention notification flushes after the window");

        assert_eq!(created.kind, NotificationKind::ApprovalRequired);
        let listed = list(&service);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].status, NotificationStatus::Unread);
    }

    #[tokio::test(start_paused = true)]
    async fn resolve_cancels_pending_and_acks_visible() {
        let service = service("resolve-both");
        let (coordinator, _task) = spawn(&service);

        // A provider hook already surfaced a visible attention record for s-1.
        let visible = service
            .create(provider_params("s-1"))
            .expect("create visible attention record");
        assert!(visible.created);

        // A projector defer for the same session is held pending.
        coordinator.defer(record(&service, projector_params("s-1")));
        settle().await;
        coordinator.resolve("attention:s-1".to_owned());
        settle().await;
        tokio::time::advance(Duration::from_secs(TEST_DEBOUNCE_SECS + 1)).await;
        settle().await;

        let listed = list(&service);
        assert_eq!(
            listed.len(),
            1,
            "the pending record must be cancelled, leaving only the visible one"
        );
        assert_eq!(listed[0].id, visible.record.id);
        assert_eq!(
            listed[0].status,
            NotificationStatus::Acknowledged,
            "resolve must acknowledge the already-visible attention record"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn provider_defer_upgrades_pending_projector_record() {
        let service = service("upgrade");
        let mut events = service.subscribe();
        let (coordinator, _task) = spawn(&service);

        coordinator.defer(record(&service, projector_params("s-1")));
        settle().await;
        coordinator.defer(record(&service, provider_params("s-1")));
        settle().await;

        tokio::time::advance(Duration::from_secs(TEST_DEBOUNCE_SECS + 1)).await;
        let created = next_created(&mut events)
            .await
            .expect("upgraded attention notification flushes after the window");

        assert_eq!(
            created.kind,
            NotificationKind::ApprovalRequired,
            "the provider record must outrank the pending projector record"
        );
        assert_eq!(created.source.provider, "codex");
        let listed = list(&service);
        assert_eq!(listed.len(), 1, "only one record must surface for the key");
        assert_eq!(listed[0].source.provider, "codex");
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_window_is_read_from_policy() {
        let service = service("policy-window");
        let mut policy = service.policy();
        policy.attention_debounce_secs = 30;
        service.set_policy(policy).expect("set policy");
        let mut events = service.subscribe();
        let (coordinator, _task) = spawn(&service);

        coordinator.defer(record(&service, provider_params("s-1")));
        settle().await;

        // Just before the configured 30s window: nothing flushes yet.
        tokio::time::advance(Duration::from_secs(20)).await;
        settle().await;
        assert!(
            list(&service).is_empty(),
            "the configured 30s debounce window must not have elapsed yet"
        );
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));

        // Past the configured window: it flushes.
        tokio::time::advance(Duration::from_secs(11)).await;
        let created = next_created(&mut events)
            .await
            .expect("flush after the configured window elapses");
        assert_eq!(created.kind, NotificationKind::ApprovalRequired);
    }
}
