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

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::notifications::DEFAULT_ATTENTION_DEBOUNCE_SECS;
use protocol::{
    AgentActivity, NotificationId, SessionId, SessionState, SessionTurnCursor, SessionTurnEventKind,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
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

/// Identity of one turn of one session.
///
/// Allocated from a per-session counter that lives as long as the session's
/// [`TurnProgress`], so an id is never reused for another turn of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TurnId(u64);

#[cfg(test)]
impl TurnId {
    /// The turn numbered `number`, for scenarios that feed the projector.
    pub(crate) fn numbered(number: u64) -> Self {
        Self(number)
    }
}

/// Who ran a turn: the registry entry (runtime incarnation) and the agent
/// incarnation within it. A turn belongs to its owner for its whole life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnOwner {
    pub(crate) entry: u64,
    pub(crate) agent: u64,
}

/// Where a turn stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnState {
    /// The session works on it.
    Open,
    /// The session waits in `blocked`; it resumes the same turn.
    Paused,
    /// The session went idle; until `deadline` it may still resume the turn.
    Settling { deadline: Instant },
    /// Over for good: it can no longer resume.
    Closed,
    /// It never completed: its owner left, it ended `blocked`, or the session
    /// ended.
    Abandoned,
}

#[derive(Debug, Clone)]
struct TurnRecord {
    owner: TurnOwner,
    /// The wall-clock start, when this daemon observed the turn begin.
    started_nanos: Option<u64>,
    started: Option<Instant>,
    ended: Option<Instant>,
    /// The session's metadata when the turn left `working`.
    ended_metadata: Option<BTreeMap<String, String>>,
    state: TurnState,
    /// Whether the turn took the session's input evidence when it opened.
    holds_input: bool,
    /// When the turn's record reached the inbox.
    inbox_reported_at: Option<Instant>,
    inbox_reported: bool,
    signal_reported: bool,
}

/// What an abandoned turn hands back to the session: the input evidence it
/// held, and the start it had observed.
#[derive(Debug, Clone, Copy)]
struct Handback {
    started_nanos: Option<u64>,
    started: Option<Instant>,
}

/// How many turns of a session stay known; an older id counts as reported.
const RECENT_TURNS: usize = 8;

#[derive(Debug, Default)]
struct TurnTable {
    next_id: u64,
    newest: Option<TurnId>,
    records: BTreeMap<TurnId, TurnRecord>,
    /// The input evidence of the session's current runtime generation: one
    /// token, taken by the turn that answers the input.
    token: bool,
    /// The start of the turn that gave the token back, for the turn that
    /// takes it next.
    returned_start: Option<Handback>,
}

impl TurnTable {
    fn open(&mut self, owner: TurnOwner, started_nanos: Option<u64>, now: Instant) -> TurnId {
        self.next_id += 1;
        let id = TurnId(self.next_id);
        self.records.insert(
            id,
            TurnRecord {
                owner,
                started_nanos,
                started: started_nanos.map(|_| now),
                ended: None,
                ended_metadata: None,
                state: TurnState::Open,
                holds_input: false,
                inbox_reported_at: None,
                inbox_reported: false,
                signal_reported: false,
            },
        );
        self.newest = Some(id);
        while self.records.len() > RECENT_TURNS {
            self.records.pop_first();
        }
        id
    }

    fn newest_mut(&mut self) -> Option<(TurnId, &mut TurnRecord)> {
        let id = self.newest?;
        self.records.get_mut(&id).map(|record| (id, record))
    }

    /// A turn whose owner is not `owner` departs: an open or paused one is
    /// abandoned, a settling one ends for good.
    fn depart_changed(&mut self, owner: TurnOwner) {
        let handback = self
            .newest_mut()
            .filter(|(_, record)| record.owner != owner)
            .and_then(|(_, record)| depart(record));
        self.give_back(handback);
    }

    /// The session ended: no turn answers its input any more. A settling turn
    /// whose window already passed finished before the end and stays reportable.
    fn abandon_all(&mut self) {
        let now = Instant::now();
        for record in self.records.values_mut() {
            record.state = match record.state {
                TurnState::Closed => TurnState::Closed,
                TurnState::Settling { deadline } if now >= deadline => TurnState::Closed,
                _ => TurnState::Abandoned,
            };
        }
        self.discard_input();
    }

    /// Drops the input evidence of the session, wherever it is: armed, held
    /// by a turn, or handed back.
    fn discard_input(&mut self) {
        for record in self.records.values_mut() {
            record.holds_input = false;
        }
        self.token = false;
        self.returned_start = None;
    }

    /// Returns the input evidence an abandoned turn held to the session.
    fn give_back(&mut self, handback: Option<Handback>) {
        if let Some(handback) = handback {
            self.token = true;
            self.returned_start = Some(handback);
        }
    }

    /// The turn `id`, just opened, takes the input evidence when it is armed,
    /// and the start of the turn that gave it back.
    fn take_token(&mut self, id: TurnId) {
        if !self.token {
            return;
        }
        self.token = false;
        let returned = self.returned_start.take();
        if let Some(record) = self.records.get_mut(&id) {
            record.holds_input = true;
            if let Some(returned) = returned.filter(|returned| returned.started.is_some()) {
                record.started_nanos = returned.started_nanos;
                record.started = returned.started;
            }
        }
    }
}

/// A turn that did not complete: it hands back the input evidence it held.
fn abandon(record: &mut TurnRecord) -> Option<Handback> {
    record.state = TurnState::Abandoned;
    std::mem::take(&mut record.holds_input).then_some(Handback {
        started_nanos: record.started_nanos,
        started: record.started,
    })
}

/// The owner of a turn left: an open or paused one is abandoned, a settling
/// one ends for good.
fn depart(record: &mut TurnRecord) -> Option<Handback> {
    match record.state {
        TurnState::Settling { .. } => {
            record.state = TurnState::Closed;
            None
        }
        TurnState::Open | TurnState::Paused => abandon(record),
        TurnState::Closed | TurnState::Abandoned => None,
    }
}

/// A change of the turns of a session, in the order it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnEdge {
    /// A new turn opened: what earlier turns of the session left unread is
    /// over.
    Started(SessionId, TurnId),
    /// The turn went idle and its report is due.
    Ended(SessionId, TurnId),
}

/// What one activity change did to the turns of a session.
#[derive(Debug, Default)]
struct TurnChange {
    started: Option<TurnId>,
    ended: Option<TurnId>,
}

/// A turn as the report of its end reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnSnapshot {
    /// Unix nanoseconds of the first transition into `working`, when observed.
    pub(crate) started_nanos: Option<u64>,
    /// Monotonic length from that transition to the turn leaving `working`.
    pub(crate) length: Option<Duration>,
    /// The session's metadata when the turn left `working`.
    pub(crate) metadata: BTreeMap<String, String>,
    pub(crate) inbox_reported: bool,
    pub(crate) signal_reported: bool,
}

impl TurnRecord {
    fn reported(&self) -> bool {
        self.inbox_reported || self.signal_reported
    }

    fn snapshot(&self) -> TurnSnapshot {
        TurnSnapshot {
            started_nanos: self.started_nanos,
            length: self
                .started
                .zip(self.ended)
                .map(|(started, ended)| ended - started),
            metadata: self.ended_metadata.clone().unwrap_or_default(),
            inbox_reported: self.inbox_reported,
            signal_reported: self.signal_reported,
        }
    }
}

/// What a flush does with the report of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnVerdict {
    /// The turn is not over: look again after the given wait, or after the
    /// recheck floor when none is given.
    Hold(Option<Duration>),
    /// The turn is over for good.
    Surface(TurnSnapshot),
    /// The turn will never be reported.
    Drop,
}

/// Observed output and turn state of one session.
///
/// The output time is Unix nanoseconds, zero meaning "not observed by this
/// daemon". The detector updates it for every PTY chunk without taking the
/// sessions lock.
#[derive(Debug, Default)]
pub(crate) struct TurnProgress {
    last_output_nanos: AtomicU64,
    /// Unix nanoseconds of the last transition into `working`; what
    /// `session.wait_turn` reports as the turn start.
    turn_started_nanos: AtomicU64,
    /// The largest start handed to a turn record, which later starts follow
    /// even when the wall clock stepped back.
    record_start_floor: AtomicU64,
    table: Mutex<TurnTable>,
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

    fn table(&self) -> MutexGuard<'_, TurnTable> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies an activity change of the session's `owner`; returns the turn
    /// that just went idle, whose end is to be reported.
    fn note_activity(
        &self,
        owner: TurnOwner,
        previous: Option<AgentActivity>,
        next: AgentActivity,
        metadata: &BTreeMap<String, String>,
        (wall_nanos, window): (u64, Duration),
        detector_observed: bool,
    ) -> TurnChange {
        let now = Instant::now();
        let mut table = self.table();
        table.depart_changed(owner);
        // `session.wait_turn` reports the last transition into `working` as
        // the turn start, whether or not the transition opens a turn. A turn
        // opened by a restatement or an owner change is ordered after it
        // without moving it. A wall clock that stepped back must not give a
        // start a smaller value.
        let floor = self
            .turn_started_nanos
            .load(Ordering::Relaxed)
            .max(self.record_start_floor.load(Ordering::Relaxed));
        let nanos = wall_nanos.max(floor + 1);
        if next == AgentActivity::Working && previous != Some(AgentActivity::Working) {
            self.turn_started_nanos.store(nanos, Ordering::Relaxed);
        }
        // A working report of a new owner opens its own turn even when the
        // session was already working, because the previous turn departed.
        let running = table
            .newest_mut()
            .is_some_and(|(_, record)| record.state == TurnState::Open);
        if next == AgentActivity::Working && (previous != Some(AgentActivity::Working) || !running)
        {
            // Output nobody asked for opens no turn; it still continues one
            // that is paused or settling.
            if detector_observed && !table.token && !continues_turn(&mut table, previous, now) {
                return TurnChange::default();
            }
            let started = enter_working(&mut table, owner, previous, non_zero(nanos), now);
            if let Some(id) = started {
                self.record_start_floor.store(nanos, Ordering::Relaxed);
                table.take_token(id);
            }
            return TurnChange {
                started,
                ended: None,
            };
        }
        let Some((id, record)) = table.newest_mut() else {
            return TurnChange::default();
        };
        let mut handback = None;
        let ended = match (previous, next) {
            // A repeated `working` of the running turn changes nothing.
            (Some(AgentActivity::Working), AgentActivity::Working) => None,
            (Some(AgentActivity::Working), AgentActivity::Blocked)
                if record.state == TurnState::Open =>
            {
                record.state = TurnState::Paused;
                None
            }
            (Some(AgentActivity::Working), AgentActivity::Idle)
                if record.state == TurnState::Open =>
            {
                record.ended = Some(now);
                record.ended_metadata = Some(metadata.clone());
                record.state = TurnState::Settling {
                    deadline: now + window,
                };
                Some(id)
            }
            (Some(AgentActivity::Working), _) if record.state == TurnState::Open => {
                handback = abandon(record);
                None
            }
            (Some(AgentActivity::Blocked), AgentActivity::Idle)
                if record.state == TurnState::Paused =>
            {
                handback = abandon(record);
                None
            }
            _ => None,
        };
        table.give_back(handback);
        TurnChange {
            started: None,
            ended,
        }
    }

    /// Opens a turn of unobserved start for an entry adopted while working.
    fn adopt(&self, owner: TurnOwner) -> Option<TurnId> {
        let mut table = self.table();
        table.depart_changed(owner);
        let live = table
            .newest_mut()
            .is_some_and(|(_, record)| matches!(record.state, TurnState::Open | TurnState::Paused));
        (!live).then(|| table.open(owner, None, Instant::now()))
    }

    fn depart(&self, owner: TurnOwner) {
        self.table().depart_changed(owner);
    }

    fn note_input(&self) {
        let mut table = self.table();
        table.token = true;
        table.returned_start = None;
    }

    fn forget_input(&self) {
        self.table().discard_input();
    }

    fn abandon_all(&self) {
        self.table().abandon_all();
    }

    fn abandon_running(&self, owner: TurnOwner) {
        let mut table = self.table();
        let handback = table
            .newest_mut()
            .filter(|(_, record)| {
                record.owner == owner && matches!(record.state, TurnState::Open | TurnState::Paused)
            })
            .and_then(|(_, record)| abandon(record));
        table.give_back(handback);
    }

    /// The turn a provider report that carries no identity belongs to: the
    /// newest turn of `owner` that can still end, or that ended and was not
    /// reported yet, so the report merges into the pending one of that turn.
    fn attach_report(&self, owner: TurnOwner) -> Option<TurnId> {
        let now = Instant::now();
        let mut table = self.table();
        table.depart_changed(owner);
        let (id, record) = table.newest_mut()?;
        match record.state {
            TurnState::Open | TurnState::Paused => Some(id),
            TurnState::Settling { deadline } if now < deadline => Some(id),
            TurnState::Settling { .. } => {
                record.state = TurnState::Closed;
                (record.owner == owner && !record.reported()).then_some(id)
            }
            TurnState::Closed => (record.owner == owner && !record.reported()).then_some(id),
            TurnState::Abandoned => None,
        }
    }

    /// What a flush does with the report of turn `id` while `owner` runs the
    /// session (`None` when the session is gone).
    fn settle(&self, id: TurnId, owner: Option<TurnOwner>) -> TurnVerdict {
        let now = Instant::now();
        let mut table = self.table();
        if owner.is_none() {
            table.abandon_all();
        }
        // Only the settled turn departs: an owner read before the registry
        // lock was released may already be stale, and the turn a newer owner
        // opened is not this report's to abandon.
        let handback = table
            .records
            .get_mut(&id)
            .filter(|record| owner.is_some_and(|owner| record.owner != owner))
            .and_then(depart);
        table.give_back(handback);
        let Some(record) = table.records.get_mut(&id) else {
            return TurnVerdict::Drop;
        };
        match record.state {
            TurnState::Abandoned => TurnVerdict::Drop,
            TurnState::Open | TurnState::Paused => TurnVerdict::Hold(None),
            TurnState::Settling { deadline } if now < deadline => {
                TurnVerdict::Hold(Some(deadline - now))
            }
            TurnState::Settling { .. } => {
                record.state = TurnState::Closed;
                TurnVerdict::Surface(record.snapshot())
            }
            TurnState::Closed => TurnVerdict::Surface(record.snapshot()),
        }
    }

    /// Whether the record of the newest turn reached the inbox within
    /// `within`.
    fn recently_reported(&self, within: Duration) -> bool {
        let mut table = self.table();
        table.newest_mut().is_some_and(|(_, record)| {
            record
                .inbox_reported_at
                .is_some_and(|reported| reported.elapsed() <= within)
        })
    }

    /// Records what the flush of turn `id` delivered.
    pub(crate) fn mark_reported(&self, id: TurnId, inbox: bool, signal: bool) {
        if let Some(record) = self.table().records.get_mut(&id) {
            if inbox && !record.inbox_reported {
                record.inbox_reported_at = Some(Instant::now());
            }
            record.inbox_reported |= inbox;
            record.signal_reported |= signal;
        }
    }
}

/// Whether a `working` after `previous` continues the newest turn: a paused
/// one after `blocked`, a settling one before its deadline.
fn continues_turn(table: &mut TurnTable, previous: Option<AgentActivity>, now: Instant) -> bool {
    table
        .newest_mut()
        .is_some_and(|(_, record)| match record.state {
            TurnState::Paused => previous == Some(AgentActivity::Blocked),
            TurnState::Settling { deadline } => now < deadline,
            _ => false,
        })
}

/// Enters `working`: a paused turn resumes after `blocked`, a settling one
/// before its deadline; anything else opens a new turn.
fn enter_working(
    table: &mut TurnTable,
    owner: TurnOwner,
    previous: Option<AgentActivity>,
    started_nanos: Option<u64>,
    now: Instant,
) -> Option<TurnId> {
    let mut handback = None;
    if let Some((_, record)) = table.newest_mut() {
        match record.state {
            TurnState::Paused if previous == Some(AgentActivity::Blocked) => {
                record.state = TurnState::Open;
                return None;
            }
            TurnState::Settling { deadline } if now < deadline => {
                record.state = TurnState::Open;
                record.ended = None;
                record.ended_metadata = None;
                return None;
            }
            TurnState::Settling { .. } => record.state = TurnState::Closed,
            TurnState::Open | TurnState::Paused => handback = abandon(record),
            TurnState::Closed | TurnState::Abandoned => {}
        }
    }
    table.give_back(handback);
    Some(table.open(owner, started_nanos, now))
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
    /// Milliseconds after a turn's end within which an `idle` to `working` edge
    /// continues the turn.
    flap_window_ms: Arc<AtomicU64>,
    /// Unix nanoseconds that replace the system clock for turn starts, or 0.
    wall_clock_pin: AtomicU64,
    /// How often a held provider report was evaluated again while its turn
    /// still worked.
    held_rechecks: AtomicU64,
    /// Turns that went idle, for the notification projector: `(session, turn)`.
    edges: mpsc::UnboundedSender<TurnEdge>,
    edge_receiver: Mutex<Option<mpsc::UnboundedReceiver<TurnEdge>>>,
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
        let (edges, edge_receiver) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(Inner {
                epoch,
                capacity: capacity.max(1),
                held_rechecks: AtomicU64::new(0),
                wall_clock_pin: AtomicU64::new(0),
                flap_window_ms: Arc::new(AtomicU64::new(
                    u64::from(DEFAULT_ATTENTION_DEBOUNCE_SECS) * 1000,
                )),
                edges,
                edge_receiver: Mutex::new(Some(edge_receiver)),
                journal: Mutex::new(Journal::default()),
                version,
                waiters: Mutex::new(Waiters::default()),
            }),
        }
    }

    /// The window in milliseconds, shared so that the notification policy
    /// keeps it current.
    pub(crate) fn flap_window_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.inner.flap_window_ms)
    }

    /// Takes the channel the turn changes arrive on; only the first
    /// taker gets it.
    pub(crate) fn take_edge_receiver(&self) -> Option<mpsc::UnboundedReceiver<TurnEdge>> {
        self.inner
            .edge_receiver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Lets a turn whose owner is not `owner` depart, ahead of the next
    /// operation that would notice.
    pub(crate) fn depart(&self, session_id: &SessionId, owner: TurnOwner) {
        if let Some(progress) = self.existing_progress(session_id) {
            progress.depart(owner);
        }
    }

    /// Input reached the runtime of `session_id`: its next detector-observed
    /// `working` is the answer to it, a turn.
    pub(crate) fn note_input(&self, session_id: &SessionId) {
        let progress = {
            let mut journal = self.journal();
            // A removed session is unknown to the journal; input to it arms nothing.
            journal
                .terminal
                .contains_key(session_id)
                .then(|| Arc::clone(journal.progress.entry(session_id.clone()).or_default()))
        };
        if let Some(progress) = progress {
            progress.note_input();
        }
    }

    /// The session runs on a new runtime generation (a launch, a resume, an
    /// adoption): input given to the previous one proves nothing about it.
    pub(crate) fn new_generation(&self, session_id: &SessionId) {
        if let Some(progress) = self.existing_progress(session_id) {
            progress.forget_input();
        }
    }

    /// A running turn of `owner` can no longer be observed to end.
    pub(crate) fn abandon_running(&self, session_id: &SessionId, owner: TurnOwner) {
        if let Some(progress) = self.existing_progress(session_id) {
            progress.abandon_running(owner);
        }
    }

    /// Opens a turn of unobserved start for an entry adopted while working.
    pub(crate) fn adopt(&self, session_id: &SessionId, owner: TurnOwner) {
        if let Some(id) = self.progress(session_id).adopt(owner) {
            let _ = self
                .inner
                .edges
                .send(TurnEdge::Started(session_id.clone(), id));
        }
    }

    /// The turn a provider report with no identity belongs to.
    pub(crate) fn attach_report(&self, session_id: &SessionId, owner: TurnOwner) -> Option<TurnId> {
        self.existing_progress(session_id)?.attach_report(owner)
    }

    /// Whether the session's newest turn reached the inbox within `within`:
    /// a provider report that names no turn then repeats it.
    pub(crate) fn recently_reported(&self, session_id: &SessionId, within: Duration) -> bool {
        self.existing_progress(session_id)
            .is_some_and(|progress| progress.recently_reported(within))
    }

    /// What a flush does with the report of turn `id`.
    pub(crate) fn settle(
        &self,
        session_id: &SessionId,
        id: TurnId,
        owner: Option<TurnOwner>,
    ) -> TurnVerdict {
        match self.existing_progress(session_id) {
            Some(progress) => progress.settle(id, owner),
            None => TurnVerdict::Drop,
        }
    }

    /// Records what the flush of a turn delivered.
    pub(crate) fn mark_reported(
        &self,
        session_id: &SessionId,
        id: TurnId,
        inbox: bool,
        signal: bool,
    ) {
        if let Some(progress) = self.existing_progress(session_id) {
            progress.mark_reported(id, inbox, signal);
        }
    }

    /// Counts one more evaluation of a held provider report.
    pub(crate) fn note_held_recheck(&self) {
        self.inner.held_rechecks.fetch_add(1, Ordering::Relaxed);
    }

    fn wall_nanos(&self) -> u64 {
        match self.inner.wall_clock_pin.load(Ordering::Relaxed) {
            0 => unix_nanos_now(),
            pinned => pinned,
        }
    }

    /// Reads turn starts as `nanos` instead of from the system clock.
    #[cfg(test)]
    pub(crate) fn pin_wall_clock(&self, nanos: u64) {
        self.inner.wall_clock_pin.store(nanos, Ordering::Relaxed);
    }

    /// How often held provider reports were evaluated again.
    #[cfg(test)]
    pub(crate) fn held_rechecks(&self) -> u64 {
        self.inner.held_rechecks.load(Ordering::Relaxed)
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
            if terminal && previous == Some(false) {
                if let Some(progress) = journal.progress.get(session_id) {
                    progress.abandon_all();
                }
            }
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

    /// Applies an activity change of the session's `owner`. A turn that goes
    /// idle is announced on the edge channel, in order with the change.
    /// `detector_observed` marks activity inferred from terminal output rather
    /// than reported by an agent.
    pub(crate) fn note_activity(
        &self,
        session_id: &SessionId,
        owner: TurnOwner,
        previous: Option<AgentActivity>,
        next: AgentActivity,
        metadata: &BTreeMap<String, String>,
        detector_observed: bool,
    ) {
        let window = Duration::from_millis(self.inner.flap_window_ms.load(Ordering::Relaxed));
        let change = self.progress(session_id).note_activity(
            owner,
            previous,
            next,
            metadata,
            (self.wall_nanos(), window),
            detector_observed,
        );
        if let Some(id) = change.started {
            let _ = self
                .inner
                .edges
                .send(TurnEdge::Started(session_id.clone(), id));
        }
        if let Some(id) = change.ended {
            let _ = self
                .inner
                .edges
                .send(TurnEdge::Ended(session_id.clone(), id));
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
