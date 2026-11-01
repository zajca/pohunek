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
use std::time::{Duration, Instant as StdInstant};

use futures::StreamExt;
use protocol::{
    NotificationId, NotificationKind, NotificationRecord, NotificationSource, SessionTurnEventKind,
};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::time::DelayQueue;
use tracing::{debug, warn};

use super::{
    is_turn_dedupe_key, source_priority, DeferredNotification, NotificationService, SourcePriority,
};
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
    /// Handling of the input on the coordinator's clock, the start of the
    /// stale-stop suppression window.
    pub(crate) monotonic: Instant,
}

impl ResolveAsOf {
    /// The moment of this call.
    #[cfg(test)]
    pub(crate) fn now() -> Self {
        Self {
            minted_before: StdInstant::now(),
            monotonic: Instant::now(),
        }
    }

    /// The moment `produced`, when the input was produced earlier than it is
    /// handled. The stale-stop suppression window starts when it is handled.
    pub(crate) fn at(produced: StdInstant) -> Self {
        Self {
            minted_before: produced,
            monotonic: Instant::now(),
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

/// A session notification held until its debounce deadline.
#[derive(Debug)]
struct Pending {
    /// Best record the notification policy keeps, committed on flush.
    inbox: Option<NotificationRecord>,
    /// Best trusted record, recorded as a turn event on flush.
    signal: Option<NotificationRecord>,
    /// How soon after its turn reached the inbox a report that names no turn
    /// repeats it, as of the report's arrival.
    repeat_window: Duration,
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

/// Least time within which a provider report that names no turn repeats the
/// turn the inbox just received, whatever the debounce window.
const REPEAT_REPORT_FLOOR: Duration = Duration::from_secs(2);

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
    notifications.bind_attention_window(turns.flap_window_handle());
    let mut queue: DelayQueue<FlushToken> = DelayQueue::new();
    let mut pending: HashMap<PendingKey, Pending> = HashMap::new();
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
                        (&notifications, &turns),
                        *deferred,
                    );
                }
                Some(Command::Resolve(dedupe_key, as_of)) => {
                    resolve(
                        &mut pending,
                        &mut queue,
                        &mut recently_resolved_turns,
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
    recently_resolved_turns: &mut HashMap<String, Instant>,
    (notifications, turns): (&NotificationService, &TurnEvents),
    deferred: DeferredNotification,
) {
    let DeferredNotification {
        record,
        mut inbox,
        signal,
        turn,
        minted,
        ..
    } = deferred;
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
    let debounce = Duration::from_secs(u64::from(notifications.policy().attention_debounce_secs));
    prune_recently_resolved_turns(recently_resolved_turns, debounce);
    let repeat_window = (debounce * 2).max(REPEAT_REPORT_FLOOR);
    let repeats_reported_turn = turn.is_none()
        && record.kind == protocol::NotificationKind::TurnCompleted
        && record
            .session_id
            .as_ref()
            .is_some_and(|session_id| turns.recently_reported(session_id, repeat_window));
    if turn.is_none()
        && inbox
        && record.kind == protocol::NotificationKind::TurnCompleted
        && is_turn_dedupe_key(&dedupe_key)
        && source_priority(&record.source) != SourcePriority::Projector
        && (repeats_reported_turn
            || recently_resolved_turns
                .remove(&dedupe_key)
                .is_some_and(|resolved_at| resolved_at.elapsed() <= debounce))
    {
        // A provider turn report that names no turn, right after the session
        // resumed or right after its turn reached the inbox, repeats a turn
        // the inbox already holds; it stays a turn signal.
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
    // Reports of one turn merge; reports of other turns are other entries.
    let key = (dedupe_key, turn);
    let foreign_at =
        (source_priority(&record.source) != SourcePriority::Projector).then_some(minted);
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
            signal: signal_record,
            repeat_window,
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
    recently_resolved_turns: &mut HashMap<String, Instant>,
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
        let Some(session_id) = pending
            .get(&token.key)
            .and_then(|entry| entry.signal.as_ref().or(entry.inbox.as_ref()))
            .and_then(|record| record.session_id.clone())
        else {
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
    // The stale-turn suppression belongs to the inbox: a cancelled entry that
    // held only a turn signal counts as no pending inbox record, so the
    // resolve still arms the suppression for the next turn report.
    let unnamed = (dedupe_key.to_owned(), None);
    let refreshed_since = pending.get(&unnamed).is_some_and(|entry| {
        as_of
            .zip(entry.newest_foreign)
            .is_some_and(|(as_of, foreign)| foreign > as_of.minted_before)
    });
    let removed_inbox = if refreshed_since {
        None
    } else {
        pending.remove(&unnamed).and_then(|entry| entry.inbox)
    };
    if removed_inbox.is_none() && !refreshed_since && is_turn_dedupe_key(dedupe_key) {
        let debounce =
            Duration::from_secs(u64::from(notifications.policy().attention_debounce_secs));
        prune_recently_resolved_turns(recently_resolved_turns, debounce);
        recently_resolved_turns.insert(
            dedupe_key.to_owned(),
            as_of.map_or_else(Instant::now, |as_of| as_of.monotonic),
        );
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
    let Some(entry) = pending.get(&token.key) else {
        return;
    };
    let Some(turn) = token.key.1 else {
        let Some(entry) = pending.remove(&token.key) else {
            return;
        };
        // A report that names no turn can outlive the flush of the stamped
        // record of its turn (its agent left within the settle window): it
        // then repeats that record, which it must not supersede.
        let repeat_window = entry.repeat_window;
        let repeats_reported_turn = entry.inbox.as_ref().is_some_and(|record| {
            record.kind == protocol::NotificationKind::TurnCompleted
                && source_priority(&record.source) != SourcePriority::Projector
                && record
                    .session_id
                    .as_ref()
                    .is_some_and(|session_id| turns.recently_reported(session_id, repeat_window))
        });
        let inbox = entry.inbox.filter(|_| !repeats_reported_turn);
        let minted = entry.newest_foreign.unwrap_or_else(StdInstant::now);
        let committed = inbox.and_then(|record| commit(notifications, record, minted));
        if let Some(signal) = entry.signal {
            record_turn((sessions, turns), &signal, committed.as_ref(), false).await;
        }
        return;
    };

    // A report of a turn the daemon runs surfaces once the turn is over for
    // good, whatever the report's own age.
    let session_id = entry
        .signal
        .as_ref()
        .or(entry.inbox.as_ref())
        .and_then(|record| record.session_id.clone());
    let Some(session_id) = session_id else {
        pending.remove(&token.key);
        return;
    };
    let owner = sessions.turn_owner(&session_id).await;
    match turns.settle(&session_id, turn, owner) {
        TurnVerdict::Drop => {
            pending.remove(&token.key);
        }
        TurnVerdict::Hold(after) => {
            turns.note_held_recheck();
            let wait = after.unwrap_or_else(|| {
                Duration::from_secs(u64::from(notifications.policy().attention_debounce_secs))
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
            let minted = entry.newest_foreign.unwrap_or_else(StdInstant::now);
            let committed = match entry.inbox {
                Some(mut record) if !snapshot.inbox_reported => {
                    DeferredNotification::stamp(&mut record, &snapshot);
                    commit(notifications, record, minted)
                }
                _ => None,
            };
            let signaled = match entry.signal {
                Some(signal) if !snapshot.signal_reported => {
                    record_turn((sessions, turns), &signal, committed.as_ref(), true).await
                }
                _ => false,
            };
            turns.mark_reported(&session_id, turn, committed.is_some(), signaled);
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
/// that record is of the same kind; a signal the policy dropped while another
/// kind surfaced names no record. Returns whether a turn event was recorded.
async fn record_turn(
    (sessions, turns): (&SessionRegistry, &TurnEvents),
    signal: &NotificationRecord,
    committed: Option<&NotificationRecord>,
    names_turn: bool,
) -> bool {
    let (Some(session_id), Some(kind)) = (signal.session_id.as_ref(), turn_kind(signal.kind))
    else {
        return false;
    };
    if kind == SessionTurnEventKind::TurnCompleted
        && !names_turn
        && sessions.current_activity(session_id).await == Some(AgentActivity::Working)
    {
        debug!(
            session_id = %session_id.0,
            "stale turn_completed signal dropped while the session works"
        );
        return false;
    }
    let notification_id: Option<NotificationId> = committed
        .filter(|record| record.kind == signal.kind)
        .map(|record| record.id.clone());
    turns.record(session_id, kind, notification_id);
    true
}

/// The turn event kind a debounced notification kind stands for.
fn turn_kind(kind: NotificationKind) -> Option<SessionTurnEventKind> {
    match kind {
        NotificationKind::TurnCompleted => Some(SessionTurnEventKind::TurnCompleted),
        NotificationKind::AgentBlocked => Some(SessionTurnEventKind::AgentBlocked),
        NotificationKind::ApprovalRequired => Some(SessionTurnEventKind::ApprovalRequired),
        NotificationKind::SessionFinished
        | NotificationKind::Error
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
        event, Event, NotificationCreateParams, NotificationKind, NotificationListParams,
        NotificationRecord, NotificationSeverity, NotificationSource, NotificationStatus,
        SessionId,
    };
    use tokio::sync::broadcast;

    use super::{AttentionCoordinator, ResolveAsOf, StdInstant};
    use crate::notifications::{default_policy, DeferredNotification, NotificationService};
    use crate::session::{SessionRegistry, SessionRegistryConfig};
    use crate::shutdown::{DaemonShutdown, DriverHandle};

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
        policy.enabled = {
            let mut policy = policy.enabled;
            policy.set(protocol::NotificationKind::TurnCompleted, true);
            policy
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
            retryable_admission: false,
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
    fn spawn(service: &NotificationService) -> (AttentionCoordinator, DriverHandle) {
        AttentionCoordinator::spawn(
            &DaemonShutdown::default(),
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

    /// An arrival that precedes everything the test mints from now on.
    fn arrival_before_now() -> ResolveAsOf {
        ResolveAsOf {
            minted_before: StdInstant::now()
                .checked_sub(Duration::from_secs(60))
                .expect("a monotonic clock that has run for a minute"),
            monotonic: tokio::time::Instant::now(),
        }
    }

    /// An arrival that follows everything the test mints before now.
    fn arrival_after_now() -> ResolveAsOf {
        ResolveAsOf {
            minted_before: StdInstant::now() + Duration::from_secs(60),
            monotonic: tokio::time::Instant::now(),
        }
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
    #[tokio::test(start_paused = true)]
    async fn a_resolve_as_of_leaves_a_visible_record_other_producers_created_later() {
        let service = service("as-of-visible-later");
        let (coordinator, _task) = spawn(&service);

        let as_of = arrival_before_now();
        let later = service
            .create(provider_params("s-1"))
            .expect("create a hook record after the moment");
        coordinator.as_of(as_of).resolve("attention:s-1".to_owned());
        settle().await;

        let listed = list(&service);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, later.record.id);
        assert_eq!(listed[0].status, NotificationStatus::Unread);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resolve_as_of_acknowledges_the_records_that_existed_then() {
        let service = service("as-of-visible-before");
        let (coordinator, _task) = spawn(&service);

        service
            .create(provider_params("s-1"))
            .expect("create a hook record before the moment");
        let as_of = arrival_after_now();
        coordinator.as_of(as_of).resolve("attention:s-1".to_owned());
        settle().await;

        let listed = list(&service);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, NotificationStatus::Acknowledged);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resolve_as_of_acknowledges_a_projector_record_minted_after_the_moment() {
        let service = service("as-of-visible-projector");
        let (coordinator, _task) = spawn(&service);

        let as_of = arrival_before_now();
        service
            .create(projector_params("s-1"))
            .expect("create a projector record after the moment");
        coordinator.as_of(as_of).resolve("attention:s-1".to_owned());
        settle().await;

        let listed = list(&service);
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].status,
            NotificationStatus::Acknowledged,
            "the projector derives its records in input order"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_resolve_as_of_keeps_a_pending_entry_other_producers_refreshed_later() {
        let service = service("as-of-pending-later");
        let (coordinator, _task) = spawn(&service);

        coordinator.defer(record(&service, projector_params("s-1")));
        let as_of = arrival_before_now();
        coordinator.defer(record(&service, provider_params("s-1")));
        settle().await;
        coordinator.as_of(as_of).resolve("attention:s-1".to_owned());
        settle().await;
        tokio::time::advance(Duration::from_secs(TEST_DEBOUNCE_SECS + 1)).await;
        settle().await;

        let listed = list(&service);
        assert_eq!(listed.len(), 1, "the later hook report surfaces");
        assert_eq!(listed[0].status, NotificationStatus::Unread);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resolve_as_of_cancels_the_pending_entries_that_existed_then() {
        let service = service("as-of-pending-before");
        let (coordinator, _task) = spawn(&service);

        coordinator.defer(record(&service, provider_params("s-1")));
        coordinator.defer(record(&service, projector_params("s-2")));
        let as_of = arrival_after_now();
        // A projector entry minted after the moment is its own earlier input's.
        coordinator.defer(record(&service, projector_params("s-1")));
        settle().await;
        coordinator.as_of(as_of).resolve("attention:s-1".to_owned());
        settle().await;
        tokio::time::advance(Duration::from_secs(TEST_DEBOUNCE_SECS + 1)).await;
        settle().await;

        let listed = list(&service);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].session_id, Some(SessionId("s-2".to_owned())));
    }

    #[tokio::test(start_paused = true)]
    async fn a_resolve_as_of_arms_the_stale_turn_suppression_at_its_moment() {
        let service = service("as-of-suppression");
        enable_turn_completed(&service);
        let (coordinator, _task) = spawn(&service);

        let as_of = ResolveAsOf::now();
        tokio::time::advance(Duration::from_secs(TEST_DEBOUNCE_SECS + 1)).await;
        coordinator.as_of(as_of).resolve("turn:s-1".to_owned());
        settle().await;
        coordinator.defer(record(&service, turn_params("s-1")));
        settle().await;
        tokio::time::advance(Duration::from_secs(TEST_DEBOUNCE_SECS + 1)).await;
        settle().await;

        assert_eq!(
            list(&service).len(),
            1,
            "a window that ended before the resolve was applied suppresses nothing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_resolve_as_of_leaves_a_hook_report_that_upgraded_a_projector_record() {
        let service = service("as-of-upgraded");
        let (coordinator, _task) = spawn(&service);

        service
            .create(projector_params("s-1"))
            .expect("create a projector record");
        let as_of = arrival_before_now();
        let upgraded = service
            .create(provider_params("s-1"))
            .expect("a hook report upgrades the projector record");
        assert!(!upgraded.created, "the report merged into the record");
        coordinator.as_of(as_of).resolve("attention:s-1".to_owned());
        settle().await;

        let listed = list(&service);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, NotificationStatus::Unread);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resolve_as_of_leaves_a_hook_record_that_absorbed_a_later_hook_report() {
        let service = service("as-of-absorbed");
        let (coordinator, _task) = spawn(&service);
        let first_minted = StdInstant::now();
        let arrival = ResolveAsOf {
            minted_before: first_minted + Duration::from_secs(10),
            monotonic: tokio::time::Instant::now(),
        };

        let first = record(&service, provider_params("s-1")).record;
        service
            .commit_deferred(first, first_minted)
            .expect("commit the first report");
        let mut later = provider_params("s-1");
        later.source.host_local_source_id = "codex-hook-s-1-later".to_owned();
        later.source_id = Some("codex:s-1:attention:later".to_owned());
        let absorbed = service
            .commit_deferred(
                record(&service, later).record,
                first_minted + Duration::from_secs(20),
            )
            .expect("commit the later report");
        assert!(!absorbed.created, "the later report merged into the first");
        coordinator
            .as_of(arrival)
            .resolve("attention:s-1".to_owned());
        settle().await;

        let listed = list(&service);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, NotificationStatus::Unread);
    }
}
