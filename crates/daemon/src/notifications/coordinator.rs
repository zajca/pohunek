//! Session notification debounce coordinator.
//!
//! A single async task owns the lifecycle of debounced session notifications
//! (`agent_blocked`, `approval_required`) and of the turn signals that feed
//! `session.wait_turn`, end to end. It is the only place that decides whether
//! such a notification ever becomes visible. Producers (the `notification.create`
//! handler and the session-state projector) do not persist debounced creates
//! directly; they mint the record, hand it to this task via a command channel,
//! and the task holds it *pending* for the debounce window. If the session
//! resumes within the window the record is dropped and nothing is ever persisted
//! or broadcast; otherwise it is committed through the store and a
//! `notification_created` event is emitted exactly as an immediate create would
//! be.
//!
//! The same window decides when a session turn event is recorded for
//! `session.wait_turn`. A pending entry therefore carries two candidates: the
//! inbox record (an attention kind only) and the [`TurnSignal`] (only from a
//! trusted producer: the projector, or a provider hook claim admitted for a
//! session the registry holds). A turn end (`turn_completed`) and a turn error
//! are signals only: they never become a record. A flush commits the inbox
//! record when there is one and records the turn event when there is a signal;
//! a resolve inside the window drops both. A create that adds no inbox record
//! never changes the inbox candidate or its timer. A `turn_completed` signal that
//! names no turn is dropped at flush time while the session is working again.
//!
//! Pending entries are in-memory only. A daemon restart while an entry is pending
//! (shorter than the debounce window) drops that transient signal, which is
//! acceptable for a sub-10s window; pending state is deliberately not persisted.

// Rust guideline compliant 2026-10-12

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant as StdInstant};

use futures::StreamExt;
use protocol::{
    NotificationId, NotificationKind, NotificationRecord, SessionId, SessionTurnEventKind,
};
use tokio::sync::mpsc;
use tokio_util::time::DelayQueue;
use tracing::{debug, warn};

use super::{source_priority, DeferredNotification, NotificationService, SourcePriority};
use crate::session::{SessionRegistry, TurnEvents, TurnId, TurnVerdict};
use crate::shutdown::{DaemonShutdown, DriverHandle, Stage};
use protocol::AgentActivity;

/// Command accepted by the session notification debounce coordinator task.
#[derive(Debug)]
enum Command {
    /// Hold a session notification pending until the debounce window elapses.
    ///
    /// Boxed so the enum's variants stay similarly sized despite the large
    /// [`NotificationRecord`].
    Defer(Box<DeferredNotification>),
    /// Cancel any pending entry for the dedupe key and acknowledge visible
    /// records, as of the given moment when there is one.
    Resolve(String, Option<ResolveAsOf>),
    /// Stop the task, dropping all pending entries without persisting them.
    Shutdown,
}

/// The moment a resolve stands for, when it is applied later than the input
/// that caused it.
///
/// A resolve acts on what existed when its input was produced. Producers that do
/// not queue behind the projector (provider hooks) keep creating records
/// while the input waits, and those records belong after the resolve.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolveAsOf {
    /// Production of the input, on the clock other producers stamp their
    /// contributions with.
    pub(crate) minted_before: StdInstant,
}

impl ResolveAsOf {
    /// The moment `produced`, when the input was produced earlier than it is
    /// handled.
    pub(crate) fn at(produced: StdInstant) -> Self {
        Self {
            minted_before: produced,
        }
    }
}

/// Clonable command sink for the session notification debounce coordinator.
///
/// Injected into `DaemonState` and the session-state projector so both producers
/// route debounced session notifications to the single owning task. Cloning is
/// cheap (an [`mpsc::UnboundedSender`] handle).
#[derive(Debug, Clone)]
pub struct AttentionCoordinator {
    commands: mpsc::UnboundedSender<Command>,
    /// The moment this handle's resolves stand for; none means the moment each
    /// resolve is sent.
    as_of: Option<ResolveAsOf>,
}

impl AttentionCoordinator {
    /// Spawn the coordinator task, returning its command handle and the handle
    /// of its task.
    ///
    /// The returned [`AttentionCoordinator`] is the clonable producer handle.
    /// The task is a sink of `tasks`: daemon shutdown stops it after every
    /// driver, so the last defers and resolves they hand over are still
    /// accepted, and pending entries are then dropped without being persisted.
    /// Flushed session signals are recorded as turn events in the journal of
    /// `sessions`.
    #[must_use]
    pub fn spawn(
        tasks: &DaemonShutdown,
        notifications: NotificationService,
        sessions: &SessionRegistry,
    ) -> (Self, DriverHandle) {
        // Unbounded: session notification commands are produced at
        // human-interaction rate (an agent blocking, an owner resuming), so
        // growth is not a practical concern, and an unbounded sink lets both the
        // request handler and the projector enqueue without send-side
        // backpressure that could stall them.
        let (commands, receiver) = mpsc::unbounded_channel();
        let stop = commands.clone();
        // A closed channel means the task already stopped.
        let task = tasks.spawn(
            "attention_coordinator",
            Stage::Sink,
            move || {
                let _ = stop.send(Command::Shutdown);
            },
            run(notifications, sessions.clone(), receiver),
        );
        (
            Self {
                commands,
                as_of: None,
            },
            task,
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
        let _ = self.commands.send(Command::Resolve(dedupe_key, self.as_of));
    }

    /// A handle whose resolves act on what existed at `as_of`: records that
    /// other producers created or merged a report into later, and pending
    /// entries they refreshed later, are left alone.
    #[must_use]
    pub(crate) fn as_of(&self, as_of: ResolveAsOf) -> Self {
        Self {
            commands: self.commands.clone(),
            as_of: Some(as_of),
        }
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
        Self {
            commands,
            as_of: None,
        }
    }
}

/// What a pending entry is for: the dedupe key of its session notification and,
/// for a `turn_completed` of a turn the daemon runs, that turn. Reports of
/// different turns are different entries and never merge.
type PendingKey = (String, Option<TurnId>);

/// A session turn event waiting for its debounce window, never serialized or
/// stored.
#[derive(Debug, Clone)]
struct TurnSignal {
    session_id: SessionId,
    kind: SessionTurnEventKind,
}

impl TurnSignal {
    /// The signal a record stands for, when its kind has a turn event.
    fn of(record: &NotificationRecord) -> Option<Self> {
        Some(Self {
            session_id: record.session_id.clone()?,
            kind: turn_kind(record.kind)?,
        })
    }
}

/// A session notification held until its debounce deadline.
#[derive(Debug)]
struct Pending {
    /// Best attention record, committed on flush.
    inbox: Option<NotificationRecord>,
    /// Best trusted signal, recorded as a turn event on flush.
    signal: Option<TurnSignal>,
    /// When the newest record of a producer other than the projector was
    /// minted into this entry.
    newest_foreign: Option<StdInstant>,
    /// Monotonic generation stamped when this entry was last (re)armed.
    ///
    /// A [`DelayQueue`] expiry only flushes when its token carries the current
    /// generation, so superseded (re-armed) or resolved entries whose timers
    /// still fire afterwards are ignored instead of surfacing stale records.
    generation: u64,
}

/// Shortest wait before a held report is evaluated again, so a zero settle
/// window cannot turn the wait for the end of a turn into a busy loop.
const HELD_REPORT_RECHECK_FLOOR: Duration = Duration::from_secs(1);

/// Value stored in the [`DelayQueue`] identifying which pending entry a timer arms.
#[derive(Debug)]
struct FlushToken {
    key: PendingKey,
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
    turns.flap_window_handle().store(
        notifications.settings().attention_debounce_secs * 1000,
        Ordering::Relaxed,
    );
    let mut queue: DelayQueue<FlushToken> = DelayQueue::new();
    let mut pending: HashMap<PendingKey, Pending> = HashMap::new();
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
                        &notifications,
                        *deferred,
                    );
                }
                Some(Command::Resolve(dedupe_key, as_of)) => {
                    resolve(
                        &mut pending,
                        &mut queue,
                        &notifications,
                        (&sessions, &turns),
                        (&dedupe_key, as_of),
                    )
                    .await;
                }
                Some(Command::Shutdown) | None => break,
            },
            Some(expired) = queue.next(), if !queue.is_empty() => {
                flush(
                    &mut pending,
                    &mut queue,
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
    pending: &mut HashMap<PendingKey, Pending>,
    queue: &mut DelayQueue<FlushToken>,
    generation: &mut u64,
    notifications: &NotificationService,
    deferred: DeferredNotification,
) {
    let DeferredNotification {
        record,
        inbox,
        signal,
        turn,
        minted,
        ..
    } = deferred;
    let signal = signal.then(|| TurnSignal::of(&record)).flatten();
    let Some(dedupe_key) = record.dedupe_key.clone() else {
        // Debounced creates always carry a dedupe key; without one the record
        // cannot be cancelled by session activity, so surface it immediately
        // rather than dropping it. Such a create is never a turn signal: the
        // projector and hook claims always carry a session key.
        if inbox {
            commit(notifications, record, minted);
        }
        return;
    };
    if !inbox && signal.is_none() {
        return;
    }
    let debounce = Duration::from_secs(notifications.settings().attention_debounce_secs);

    // Provider reports outrank projector reports for the same session moment, so
    // keep the higher-priority record when one is already pending; the store's
    // dedupe remains the final authority at flush time. A create that adds no
    // inbox record re-arms the timer only while no inbox candidate is pending.
    // Reports of one turn merge; reports of other turns are other entries.
    let key = (dedupe_key, turn);
    let foreign_at = (source_priority(&record) != SourcePriority::Projector).then_some(minted);
    let (existing_inbox, existing_signal, existing_generation, existing_foreign) =
        match pending.remove(&key) {
            Some(entry) => (
                entry.inbox,
                entry.signal,
                Some(entry.generation),
                entry.newest_foreign,
            ),
            None => (None, None, None, None),
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
                key: key.clone(),
                generation: current,
            },
            debounce,
        );
        current
    });
    pending.insert(
        key,
        Pending {
            inbox: inbox_record,
            signal: signal.or(existing_signal),
            newest_foreign: existing_foreign.max(foreign_at),
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

/// Cancel the pending entries that name no turn for `dedupe_key` and
/// acknowledge visible records.
///
/// A resolve `as_of` an earlier moment leaves alone the pending entry another
/// producer refreshed after it and the visible records that producer created or
/// merged a report into after it.
///
/// An entry for a turn the daemon runs is not cancelled: the state of its turn
/// decides whether it surfaces.
async fn resolve(
    pending: &mut HashMap<PendingKey, Pending>,
    queue: &mut DelayQueue<FlushToken>,
    notifications: &NotificationService,
    (sessions, turns): (&SessionRegistry, &TurnEvents),
    (dedupe_key, as_of): (&str, Option<ResolveAsOf>),
) {
    // A turn that closed because its successor opened surfaces before the
    // resolve, which then acknowledges it like any visible record of the session.
    let closed: Vec<FlushToken> = pending
        .iter()
        .filter(|((key, turn), _)| key == dedupe_key && turn.is_some())
        .map(|(key, entry)| FlushToken {
            key: key.clone(),
            generation: entry.generation,
        })
        .collect();
    for token in closed {
        let Some(turn) = token.key.1 else { continue };
        let Some(session_id) = pending.get(&token.key).and_then(|entry| {
            entry
                .signal
                .as_ref()
                .map(|signal| signal.session_id.clone())
        }) else {
            continue;
        };
        let owner = sessions.turn_owner(&session_id).await;
        if matches!(
            turns.settle(&session_id, turn, owner),
            TurnVerdict::Surface(_)
        ) {
            flush(pending, queue, notifications, (sessions, turns), &token).await;
        }
    }
    // A pending entry that is cancelled never becomes visible. Its DelayQueue
    // timer may still fire, but the generation guard in `flush` makes that a
    // no-op, so the token is left to expire on its own rather than tracked.
    let unnamed = (dedupe_key.to_owned(), None);
    let refreshed_since = pending.get(&unnamed).is_some_and(|entry| {
        as_of
            .zip(entry.newest_foreign)
            .is_some_and(|(as_of, foreign)| foreign > as_of.minted_before)
    });
    if !refreshed_since {
        pending.remove(&unnamed);
    }
    // Unify with the already-visible path: acknowledge any surfaced record
    // sharing this session's dedupe key.
    if let Err(error) = notifications.resolve_session_notifications(dedupe_key, as_of.as_ref()) {
        warn!(
            dedupe_key,
            error = %error,
            "failed to acknowledge visible session notifications on resolve"
        );
    }
}

/// Commit a pending entry when its timer is still the current generation.
///
/// The inbox record is committed first, so a waiter woken by the turn event
/// already finds the inbox record its event names.
async fn flush(
    pending: &mut HashMap<PendingKey, Pending>,
    queue: &mut DelayQueue<FlushToken>,
    notifications: &NotificationService,
    (sessions, turns): (&SessionRegistry, &TurnEvents),
    token: &FlushToken,
) {
    let is_current = pending
        .get(&token.key)
        .is_some_and(|entry| entry.generation == token.generation);
    if !is_current {
        // Superseded by a later defer or already resolved; the record must not
        // surface from this stale timer.
        return;
    }
    let Some(turn) = token.key.1 else {
        let Some(entry) = pending.remove(&token.key) else {
            return;
        };
        let minted = entry.newest_foreign.unwrap_or_else(StdInstant::now);
        let committed = entry
            .inbox
            .and_then(|record| commit(notifications, record, minted));
        if let Some(signal) = entry.signal {
            record_turn((sessions, turns), &signal, committed.as_ref(), false).await;
        }
        return;
    };

    // A report of a turn the daemon runs surfaces once the turn is over for
    // good, whatever the report's own age.
    let Some(session_id) = pending.get(&token.key).and_then(|entry| {
        entry
            .signal
            .as_ref()
            .map(|signal| signal.session_id.clone())
    }) else {
        pending.remove(&token.key);
        return;
    };
    let owner = sessions.turn_owner(&session_id).await;
    match turns.settle(&session_id, turn, owner) {
        TurnVerdict::Drop => {
            pending.remove(&token.key);
        }
        TurnVerdict::Hold(after) => {
            let wait = after.unwrap_or_else(|| {
                Duration::from_secs(notifications.settings().attention_debounce_secs)
                    .max(HELD_REPORT_RECHECK_FLOOR)
            });
            queue.insert(
                FlushToken {
                    key: token.key.clone(),
                    generation: token.generation,
                },
                wait,
            );
        }
        TurnVerdict::Surface(snapshot) => {
            let Some(entry) = pending.remove(&token.key) else {
                return;
            };
            let signaled = match entry.signal {
                Some(signal) if !snapshot.signal_reported => {
                    record_turn((sessions, turns), &signal, None, true).await
                }
                _ => false,
            };
            turns.mark_reported(&session_id, turn, signaled);
        }
    }
}

/// Persist a debounced record and emit its event, logging (not panicking) on error.
///
/// Returns the visible record the commit produced or deduplicated into.
fn commit(
    notifications: &NotificationService,
    record: NotificationRecord,
    minted: StdInstant,
) -> Option<NotificationRecord> {
    match notifications.commit_deferred(record, minted) {
        Ok(result) => Some(result.record),
        Err(error) => {
            warn!(error = %error, "failed to commit debounced session notification");
            None
        }
    }
}

/// Record the turn event a flushed session signal stands for.
///
/// A `turn_completed` signal that names no turn is dropped while the session is
/// working: it is the stale stop of an earlier turn, and the running turn
/// reports its own stop. The event names the committed inbox record only when
/// that record is of the same kind. Returns whether a turn event was recorded.
async fn record_turn(
    (sessions, turns): (&SessionRegistry, &TurnEvents),
    signal: &TurnSignal,
    committed: Option<&NotificationRecord>,
    names_turn: bool,
) -> bool {
    if signal.kind == SessionTurnEventKind::TurnCompleted
        && !names_turn
        && sessions.current_activity(&signal.session_id).await == Some(AgentActivity::Working)
    {
        debug!(
            session_id = %signal.session_id.0,
            "stale turn_completed signal dropped while the session works"
        );
        return false;
    }
    let notification_id: Option<NotificationId> = committed
        .filter(|record| turn_kind(record.kind) == Some(signal.kind))
        .map(|record| record.id.clone());
    turns.record(&signal.session_id, signal.kind, notification_id);
    true
}

/// The turn event kind a notification kind stands for.
fn turn_kind(kind: NotificationKind) -> Option<SessionTurnEventKind> {
    match kind {
        NotificationKind::TurnCompleted => Some(SessionTurnEventKind::TurnCompleted),
        NotificationKind::AgentBlocked => Some(SessionTurnEventKind::AgentBlocked),
        NotificationKind::ApprovalRequired => Some(SessionTurnEventKind::ApprovalRequired),
        NotificationKind::Error => Some(SessionTurnEventKind::TurnError),
        NotificationKind::SessionFinished
        | NotificationKind::System
        | NotificationKind::Unknown => None,
    }
}

/// Whether `existing` should be kept over `incoming` for the same pending key.
///
/// Provider hook reports outrank user reports, which outrank projector reports;
/// equal priority takes the incoming record so a fresh report refreshes the
/// pending payload.
fn outranks(existing: &NotificationRecord, incoming: &NotificationRecord) -> bool {
    priority_rank(existing) > priority_rank(incoming)
}

/// Rank a record's priority for pending-buffer conflict resolution.
fn priority_rank(record: &NotificationRecord) -> u8 {
    match source_priority(record) {
        SourcePriority::Projector => 0,
        SourcePriority::User => 1,
        SourcePriority::Provider => 2,
    }
}
