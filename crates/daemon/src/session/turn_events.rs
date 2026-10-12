//! In-memory turn-event journal, per-session turn progress, and the turn
//! waiter pool behind `session.wait_turn`.
//!
//! The journal is a bounded ring of turn events with one daemon-global
//! monotonic sequence. Waiters read it by sequence, so an event recorded while
//! a waiter was not polling is still found on its next scan; a version watch
//! only says "look again" and can never drop the event being waited for.
//! Nothing here is persisted: a cursor names the daemon instance that issued
//! it, and a cursor from another instance resets.

// Rust guideline compliant 2026-10-12

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    AgentActivity, NotificationId, SessionId, SessionState, SessionTurnCursor, SessionTurnEventKind,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::watch;
use tracing::{debug, warn};

/// Leading tag of every cursor this daemon issues.
///
/// It versions the cursor layout, so a later layout change makes an older
/// cursor unreadable (and therefore a reset) instead of misread.
const CURSOR_TAG: &str = "t1";

/// Separator between cursor fields.
///
/// Daemon instance identifiers never contain it, and it is printable ASCII as
/// the protocol requires of a cursor.
const CURSOR_SEPARATOR: char = '.';

/// One recorded turn event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnEntry {
    pub(crate) seq: u64,
    pub(crate) session_id: SessionId,
    pub(crate) kind: SessionTurnEventKind,
    pub(crate) at: String,
    pub(crate) notification_id: Option<NotificationId>,
}

/// A cursor this daemon instance can place in its journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorPosition {
    /// Events with a greater sequence are newer than the cursor.
    pub(crate) seq: u64,
    /// Unix nanoseconds at which the cursor was issued.
    pub(crate) issued_nanos: u64,
}

/// Result of matching the journal against a wait's targets and kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnScan {
    /// Oldest selected event newer than the scanned position.
    pub(crate) event: Option<TurnEntry>,
    /// Newest sequence in the journal when it was scanned.
    pub(crate) latest: u64,
    /// Whether events right after the scanned position were evicted, so the
    /// position lost its gap-free view of the journal.
    pub(crate) evicted: bool,
}

/// Observed output and turn-start times of one session.
///
/// Both values are Unix nanoseconds; zero means "not observed by this daemon".
/// The detector updates the output time for every PTY chunk without taking
/// the sessions lock.
#[derive(Debug, Default)]
pub(crate) struct TurnProgress {
    last_output_nanos: AtomicU64,
    turn_started_nanos: AtomicU64,
}

impl TurnProgress {
    /// Records terminal output observed now.
    pub(crate) fn note_output(&self) {
        self.last_output_nanos
            .store(unix_nanos_now(), Ordering::Relaxed);
    }

    /// Unix nanoseconds of the last observed output, when there was one.
    pub(crate) fn last_output_nanos(&self) -> Option<u64> {
        non_zero(self.last_output_nanos.load(Ordering::Relaxed))
    }

    /// Unix nanoseconds of the last observed transition into `working`.
    pub(crate) fn turn_started_nanos(&self) -> Option<u64> {
        non_zero(self.turn_started_nanos.load(Ordering::Relaxed))
    }
}

/// Turn-event journal, progress map, and turn waiter pool of one registry.
///
/// Cloning is cheap and shares the same journal.
#[derive(Debug, Clone)]
pub(crate) struct TurnEvents {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Daemon instance identifier carried by every issued cursor.
    epoch: String,
    /// Maximum number of retained journal entries.
    capacity: usize,
    journal: Mutex<Journal>,
    /// Bumped after every append and every session removal; waiters re-scan
    /// on each change.
    version: watch::Sender<u64>,
    waiters: Mutex<Waiters>,
}

#[derive(Debug, Default)]
struct Journal {
    entries: VecDeque<TurnEntry>,
    /// Newest assigned sequence; zero before the first append.
    latest: u64,
    /// Whether each known session was terminal when last observed, so
    /// `exited` is recorded once per transition into a terminal state.
    terminal: HashMap<SessionId, bool>,
    progress: HashMap<SessionId, Arc<TurnProgress>>,
}

#[derive(Debug, Default)]
struct Waiters {
    global: usize,
    per_session: HashMap<SessionId, usize>,
}

impl TurnEvents {
    /// Creates an empty journal for the daemon instance `epoch`.
    pub(crate) fn new(epoch: String, capacity: usize) -> Self {
        let (version, _receiver) = watch::channel(0);
        Self {
            inner: Arc::new(Inner {
                epoch,
                capacity: capacity.max(1),
                journal: Mutex::new(Journal::default()),
                version,
                waiters: Mutex::new(Waiters::default()),
            }),
        }
    }

    /// Records one turn event now and wakes every waiter.
    pub(crate) fn record(
        &self,
        session_id: &SessionId,
        kind: SessionTurnEventKind,
        notification_id: Option<NotificationId>,
    ) {
        let seq = {
            let mut journal = self.journal();
            self.append(&mut journal, session_id.clone(), kind, notification_id)
        };
        debug!(
            session_id = %session_id.0,
            turn.kind = kind.as_str(),
            turn.seq = seq,
            "session turn event recorded"
        );
        self.inner.version.send_modify(|version| *version += 1);
    }

    /// Notes the lifecycle state a registry entry takes, under the sessions
    /// lock that makes it visible.
    ///
    /// A known session moving from a non-terminal into a terminal state records
    /// `exited` before any reader can see the terminal state, so a wait that
    /// observes every target terminal also finds their exits in the journal.
    /// An unknown session takes the state as its baseline: loading a session
    /// that ended before this daemon started records nothing.
    pub(crate) fn observe_state(&self, session_id: &SessionId, state: SessionState) {
        let terminal = state.is_terminal();
        let exited = {
            let mut journal = self.journal();
            let previous = journal.terminal.insert(session_id.clone(), terminal);
            (terminal && previous == Some(false)).then(|| {
                self.append(
                    &mut journal,
                    session_id.clone(),
                    SessionTurnEventKind::Exited,
                    None,
                )
            })
        };
        if let Some(seq) = exited {
            debug!(
                session_id = %session_id.0,
                turn.kind = SessionTurnEventKind::Exited.as_str(),
                turn.seq = seq,
                session.state = ?state,
                "session turn event recorded"
            );
            self.inner.version.send_modify(|version| *version += 1);
        }
    }

    /// Forgets a removed session and wakes waiters, so a wait on it ends.
    pub(crate) fn forget(&self, session_id: &SessionId) {
        {
            let mut journal = self.journal();
            journal.terminal.remove(session_id);
            journal.progress.remove(session_id)
        };
        self.inner.version.send_modify(|version| *version += 1);
    }

    /// Returns the progress record of `session_id`, creating it when absent.
    pub(crate) fn progress(&self, session_id: &SessionId) -> Arc<TurnProgress> {
        Arc::clone(
            self.journal()
                .progress
                .entry(session_id.clone())
                .or_default(),
        )
    }

    /// Returns the progress record of `session_id` without creating one.
    pub(crate) fn existing_progress(&self, session_id: &SessionId) -> Option<Arc<TurnProgress>> {
        self.journal().progress.get(session_id).map(Arc::clone)
    }

    /// Records an activity change; entering `working` starts a new turn.
    pub(crate) fn note_activity(
        &self,
        session_id: &SessionId,
        previous: Option<AgentActivity>,
        next: AgentActivity,
    ) {
        if next == AgentActivity::Working && previous != Some(AgentActivity::Working) {
            self.progress(session_id)
                .turn_started_nanos
                .store(unix_nanos_now(), Ordering::Relaxed);
        }
    }

    /// Subscribes to journal changes; the current version counts as seen.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.version.subscribe()
    }

    /// Returns the newest assigned sequence.
    pub(crate) fn latest(&self) -> u64 {
        self.journal().latest
    }

    /// Places `cursor` in this journal.
    ///
    /// Returns `None` when the cursor is not one this daemon instance issued
    /// or the journal does not cover its position: another epoch or layout, a
    /// sequence ahead of the journal, or one older than the oldest retained
    /// entry minus one, whose following events were evicted.
    pub(crate) fn place(&self, cursor: &SessionTurnCursor) -> Option<CursorPosition> {
        let mut fields = cursor.as_str().splitn(4, CURSOR_SEPARATOR);
        if fields.next()? != CURSOR_TAG {
            return None;
        }
        let seq = fields.next()?.parse::<u64>().ok()?;
        let issued_nanos = fields.next()?.parse::<u64>().ok()?;
        if fields.next()? != self.inner.epoch {
            return None;
        }
        let journal = self.journal();
        if seq > journal.latest {
            return None;
        }
        let evicted = journal
            .entries
            .front()
            .is_some_and(|oldest| seq.saturating_add(1) < oldest.seq);
        (!evicted).then_some(CursorPosition { seq, issued_nanos })
    }

    /// Issues a cursor at `seq`, stamped with the current time.
    pub(crate) fn cursor(&self, seq: u64) -> SessionTurnCursor {
        let issued = unix_nanos_now();
        SessionTurnCursor::new(format!(
            "{CURSOR_TAG}{CURSOR_SEPARATOR}{seq}{CURSOR_SEPARATOR}{issued}{CURSOR_SEPARATOR}{}",
            self.inner.epoch
        ))
        .expect("a daemon cursor is short printable ASCII")
    }

    /// Finds the oldest event newer than `after` for one of `targets` with
    /// one of `kinds`, and reports whether events after `after` were evicted.
    pub(crate) fn scan(
        &self,
        after: u64,
        targets: &[SessionId],
        kinds: &[SessionTurnEventKind],
    ) -> TurnScan {
        let journal = self.journal();
        let evicted = journal
            .entries
            .front()
            .is_some_and(|oldest| after.saturating_add(1) < oldest.seq);
        let event = journal
            .entries
            .iter()
            .filter(|entry| entry.seq > after)
            .find(|entry| targets.contains(&entry.session_id) && kinds.contains(&entry.kind))
            .cloned();
        TurnScan {
            event,
            latest: journal.latest,
            evicted,
        }
    }

    /// Takes one global turn-waiter slot and one per-session slot on every
    /// target, or none of them.
    ///
    /// Returns `None` when a cap is reached; the caller reports
    /// `session_waiter_limit_reached`.
    pub(crate) fn acquire(
        &self,
        targets: &[SessionId],
        global_cap: usize,
        session_cap: usize,
    ) -> Option<TurnPermit> {
        let mut waiters = self
            .inner
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if waiters.global >= global_cap {
            warn!(
                concurrent_waiters = waiters.global,
                waiter_limit = global_cap,
                limit_scope = "global",
                "session turn waiter limit reached"
            );
            return None;
        }
        if let Some(full) = targets.iter().find(|id| {
            waiters
                .per_session
                .get(*id)
                .is_some_and(|count| *count >= session_cap)
        }) {
            warn!(
                session_id = %full.0,
                waiter_limit = session_cap,
                limit_scope = "session",
                "session turn waiter limit reached"
            );
            return None;
        }
        waiters.global += 1;
        for id in targets {
            *waiters.per_session.entry(id.clone()).or_default() += 1;
        }
        debug!(
            concurrent_waiters = waiters.global,
            targets = targets.len(),
            "session turn waiter acquired"
        );
        Some(TurnPermit {
            events: self.clone(),
            targets: targets.to_vec(),
        })
    }

    /// Number of turn waits currently holding slots.
    ///
    /// Scenarios use it as the readiness signal that a wait is parked with its
    /// deadline armed, before they move a paused clock.
    #[cfg(test)]
    pub(crate) fn active_waiters(&self) -> usize {
        self.inner
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .global
    }

    fn append(
        &self,
        journal: &mut Journal,
        session_id: SessionId,
        kind: SessionTurnEventKind,
        notification_id: Option<NotificationId>,
    ) -> u64 {
        journal.latest += 1;
        let seq = journal.latest;
        journal.entries.push_back(TurnEntry {
            seq,
            session_id,
            kind,
            at: crate::time::now_rfc3339(),
            notification_id,
        });
        while journal.entries.len() > self.inner.capacity {
            journal.entries.pop_front();
        }
        seq
    }

    fn journal(&self) -> MutexGuard<'_, Journal> {
        self.inner
            .journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Turn-waiter slots held by one `session.wait_turn` request; dropping it
/// releases them.
#[derive(Debug)]
pub(crate) struct TurnPermit {
    events: TurnEvents,
    targets: Vec<SessionId>,
}

impl Drop for TurnPermit {
    fn drop(&mut self) {
        let mut waiters = self
            .events
            .inner
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        waiters.global = waiters.global.saturating_sub(1);
        for id in &self.targets {
            if let Some(count) = waiters.per_session.get_mut(id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    waiters.per_session.remove(id);
                }
            }
        }
    }
}

/// Current wall-clock time in Unix nanoseconds.
///
/// Turn progress compares output time with cursor issue time, so both come
/// from this one clock. A clock before the epoch reads as zero ("unknown").
pub(crate) fn unix_nanos_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0)
}

/// Formats Unix nanoseconds as an RFC 3339 timestamp.
pub(crate) fn rfc3339_from_nanos(nanos: u64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(nanos))
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn non_zero(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}
