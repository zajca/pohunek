//! Maintains bounded output history and atomic subscriptions.

// Rust guideline compliant 2026-07-29

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;

use pohunek_terminal::{TerminalSnapshot, TerminalTracker};
use pohunek_worker_protocol::MAX_DATA_PAYLOAD_BYTES;

/// Output-ring failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RingError {
    /// A memory budget is zero.
    #[error("{field} must be greater than zero")]
    InvalidLimit {
        /// Invalid field name.
        field: &'static str,
    },
    /// The runtime's byte offset would overflow.
    #[error("runtime output offset overflowed")]
    OffsetOverflow,
    /// A requested offset is beyond produced output.
    #[error("requested output offset {requested} is beyond next offset {next}")]
    OffsetAhead {
        /// Rejected offset.
        requested: u64,
        /// Current next offset.
        next: u64,
    },
}

/// Contiguous PTY output with its runtime offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChunk {
    /// Offset of the first payload byte.
    pub offset: u64,
    /// Raw PTY bytes.
    pub bytes: Vec<u8>,
}

impl OutputChunk {
    fn end(&self) -> u64 {
        self.offset
            .saturating_add(u64::try_from(self.bytes.len()).unwrap_or(u64::MAX))
    }
}

/// Atomic output snapshot used to seed a subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSnapshot {
    /// Requested retained bytes are replayable.
    Replay {
        /// Retained bytes beginning at the requested offset.
        chunk: OutputChunk,
        /// First live-output offset after the snapshot.
        watermark: u64,
    },
    /// Requested bytes were evicted and require a terminal repaint.
    Gap {
        /// Missing output interval.
        missing: Range<u64>,
        /// Complete terminal state at the recovery watermark.
        terminal: TerminalSnapshot,
    },
}

/// Event delivered to one bounded subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputEvent {
    /// Initial retained output.
    Replay(OutputChunk),
    /// New output after subscription.
    Output(OutputChunk),
    /// Explicit loss followed by a complete current terminal state.
    Gap {
        /// Missing output interval.
        missing: Range<u64>,
        /// Snapshot and live-output resume watermark.
        watermark: u64,
    },
    /// One bounded ANSI fragment of the terminal repaint following a gap.
    TerminalSnapshot(TerminalChunk),
    /// PTY output reached EOF.
    Exit {
        /// Final output offset.
        next_offset: u64,
    },
}

impl OutputEvent {
    fn payload_bytes(&self) -> usize {
        match self {
            Self::Replay(chunk) | Self::Output(chunk) => chunk.bytes.len(),
            Self::TerminalSnapshot(chunk) => chunk.bytes.len(),
            Self::Gap { .. } | Self::Exit { .. } => 0,
        }
    }

    fn next_offset(&self) -> u64 {
        match self {
            Self::Replay(chunk) | Self::Output(chunk) => chunk.end(),
            Self::Gap { watermark, .. } => *watermark,
            Self::TerminalSnapshot(chunk) => chunk.terminal.watermark,
            Self::Exit { next_offset } => *next_offset,
        }
    }
}

/// One bounded fragment of a complete terminal repaint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalChunk {
    terminal: Arc<TerminalSnapshot>,
    /// Zero-based byte offset in the complete ANSI repaint.
    pub offset: usize,
    /// Complete ANSI repaint length.
    pub total_bytes: usize,
    /// Exact ANSI bytes at `offset`.
    pub bytes: Vec<u8>,
}

impl TerminalChunk {
    /// Borrows the structured terminal state shared by all repaint fragments.
    #[must_use]
    pub fn terminal(&self) -> &TerminalSnapshot {
        &self.terminal
    }
}

/// Cloneable output actor shared by the PTY reader and server.
#[derive(Debug, Clone)]
pub struct OutputHub {
    inner: Arc<Mutex<HubState>>,
    subscriber_limit: usize,
}

impl OutputHub {
    /// Creates an empty output actor.
    ///
    /// # Errors
    ///
    /// Returns [`RingError::InvalidLimit`] when either memory budget is zero.
    pub fn new(
        history_limit: usize,
        subscriber_limit: usize,
        rows: u16,
        cols: u16,
    ) -> Result<Self, RingError> {
        if history_limit == 0 {
            return Err(RingError::InvalidLimit {
                field: "history_limit",
            });
        }
        if subscriber_limit == 0 {
            return Err(RingError::InvalidLimit {
                field: "subscriber_limit",
            });
        }
        let terminal = TerminalTracker::new(rows, cols);
        let terminal_snapshot = Arc::new(terminal.snapshot(0));
        Ok(Self {
            inner: Arc::new(Mutex::new(HubState {
                ring: OutputRing::new(history_limit),
                terminal,
                terminal_snapshot,
                subscribers: Vec::new(),
                exited: false,
            })),
            subscriber_limit,
        })
    }

    /// Appends PTY bytes and advances every subscriber.
    ///
    /// # Errors
    ///
    /// Returns [`RingError::OffsetOverflow`] instead of wrapping offsets.
    pub fn push(&self, bytes: &[u8]) -> Result<OutputChunk, RingError> {
        let mut state = lock(&self.inner);
        let chunk = state.ring.push(bytes)?;
        if bytes.is_empty() {
            return Ok(chunk);
        }
        state.terminal.feed(bytes);
        let snapshot = Arc::new(state.terminal.snapshot(state.ring.next_offset));
        state.terminal_snapshot = Arc::clone(&snapshot);
        let mut retained = Vec::with_capacity(state.subscribers.len());
        for weak in state.subscribers.drain(..) {
            if let Some(queue) = weak.upgrade() {
                queue.push_output(chunk.clone(), &snapshot);
                retained.push(Arc::downgrade(&queue));
            }
        }
        state.subscribers = retained;
        Ok(chunk)
    }

    /// Atomically snapshots retained output and registers a live subscriber.
    ///
    /// # Errors
    ///
    /// Returns [`RingError::OffsetAhead`] when `after_offset` is in the future.
    pub fn subscribe(&self, after_offset: Option<u64>) -> Result<OutputSubscriber, RingError> {
        let mut state = lock(&self.inner);
        let seed = state.subscription_seed(after_offset)?;
        Ok(self.register_subscriber(&mut state, seed))
    }

    /// Atomically captures the current terminal repaint and registers a live
    /// subscriber at the snapshot watermark.
    #[must_use]
    pub fn subscribe_terminal_snapshot(&self) -> OutputSubscriber {
        let mut state = lock(&self.inner);
        let seed = QueueSeed::Snapshot {
            terminal: Arc::clone(&state.terminal_snapshot),
        };
        self.register_subscriber(&mut state, seed)
    }

    fn register_subscriber(&self, state: &mut HubState, seed: QueueSeed) -> OutputSubscriber {
        let queue = Arc::new(SubscriberQueue::new(self.subscriber_limit, seed));
        if state.exited {
            queue.push_terminal(state.ring.next_offset);
        } else {
            state.subscribers.push(Arc::downgrade(&queue));
        }
        OutputSubscriber {
            hub: Arc::clone(&self.inner),
            queue,
        }
    }

    /// Captures output without registering a subscriber.
    ///
    /// # Errors
    ///
    /// Returns [`RingError::OffsetAhead`] when `after_offset` is in the future.
    pub fn snapshot(&self, after_offset: Option<u64>) -> Result<OutputSnapshot, RingError> {
        lock(&self.inner).snapshot(after_offset)
    }

    /// Resizes the terminal model after a successful PTY resize.
    pub fn resize(&self, rows: u16, cols: u16) {
        let mut state = lock(&self.inner);
        state.terminal.resize(rows, cols);
        state.terminal_snapshot = Arc::new(state.terminal.snapshot(state.ring.next_offset));
    }

    /// Marks output EOF and wakes every subscriber.
    pub fn mark_exit(&self) {
        let mut state = lock(&self.inner);
        state.exited = true;
        let next_offset = state.ring.next_offset;
        for weak in state.subscribers.drain(..) {
            if let Some(queue) = weak.upgrade() {
                queue.push_terminal(next_offset);
            }
        }
    }

    /// Returns the next output offset.
    #[must_use]
    pub fn next_offset(&self) -> u64 {
        lock(&self.inner).ring.next_offset
    }

    /// Returns the first retained output offset.
    #[must_use]
    pub fn history_start_offset(&self) -> u64 {
        lock(&self.inner).ring.start_offset
    }

    /// Returns a complete current terminal snapshot.
    #[must_use]
    pub fn terminal_snapshot(&self) -> TerminalSnapshot {
        let state = lock(&self.inner);
        (*state.terminal_snapshot).clone()
    }
}

#[derive(Debug)]
struct HubState {
    ring: OutputRing,
    terminal: TerminalTracker,
    terminal_snapshot: Arc<TerminalSnapshot>,
    subscribers: Vec<Weak<SubscriberQueue>>,
    exited: bool,
}

impl HubState {
    fn subscription_seed(&self, after_offset: Option<u64>) -> Result<QueueSeed, RingError> {
        let requested = self.validate_offset(after_offset)?;
        if requested < self.ring.start_offset {
            return Ok(QueueSeed::Gap {
                missing: requested..self.ring.next_offset,
                terminal: Arc::clone(&self.terminal_snapshot),
            });
        }
        Ok(QueueSeed::Replay(requested..self.ring.next_offset))
    }

    fn snapshot(&self, after_offset: Option<u64>) -> Result<OutputSnapshot, RingError> {
        // A fresh consumer starts at generation offset zero. If early bytes
        // were evicted, replaying only the retained suffix could begin inside
        // an escape sequence and cannot reconstruct the screen; emit the same
        // explicit gap plus complete terminal snapshot used by reconnects.
        let requested = self.validate_offset(after_offset)?;
        if requested < self.ring.start_offset {
            return Ok(OutputSnapshot::Gap {
                missing: requested..self.ring.next_offset,
                terminal: (*self.terminal_snapshot).clone(),
            });
        }
        Ok(OutputSnapshot::Replay {
            chunk: OutputChunk {
                offset: requested,
                bytes: self.ring.bytes_from(requested),
            },
            watermark: self.ring.next_offset,
        })
    }

    fn validate_offset(&self, after_offset: Option<u64>) -> Result<u64, RingError> {
        let requested = after_offset.unwrap_or(0);
        if requested > self.ring.next_offset {
            return Err(RingError::OffsetAhead {
                requested,
                next: self.ring.next_offset,
            });
        }
        Ok(requested)
    }
}

/// Receives bounded output events.
#[derive(Debug)]
pub struct OutputSubscriber {
    hub: Arc<Mutex<HubState>>,
    queue: Arc<SubscriberQueue>,
}

impl OutputSubscriber {
    /// Waits for the next output event.
    pub async fn recv(&mut self) -> Option<OutputEvent> {
        loop {
            let notified = self.queue.notify.notified();
            {
                let hub = lock(&self.hub);
                let mut state = lock(&self.queue.state);
                if let Some(event) = state.next_event(&hub, self.queue.event_limit) {
                    return Some(event);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}

#[derive(Debug)]
struct SubscriberQueue {
    limit: usize,
    event_limit: usize,
    state: Mutex<SubscriberState>,
    notify: Notify,
}

impl SubscriberQueue {
    fn new(limit: usize, seed: QueueSeed) -> Self {
        let expected_offset = match &seed {
            QueueSeed::Replay(range) => range.start,
            QueueSeed::Gap { missing, .. } => missing.start,
            QueueSeed::Snapshot { terminal } => terminal.watermark,
        };
        let event_limit = MAX_DATA_PAYLOAD_BYTES.min(limit);
        let (replay, snapshot) = match seed {
            QueueSeed::Replay(range) => (Some(ReplayCursor::new(range)), None),
            QueueSeed::Gap { missing, terminal } => {
                (None, Some(SnapshotCursor::new(missing, terminal)))
            }
            QueueSeed::Snapshot { terminal } => (None, Some(SnapshotCursor::without_gap(terminal))),
        };
        Self {
            limit,
            event_limit,
            state: Mutex::new(SubscriberState {
                replay,
                snapshot,
                events: VecDeque::new(),
                queued_bytes: 0,
                expected_offset,
                closed: false,
            }),
            notify: Notify::new(),
        }
    }

    fn push_output(&self, chunk: OutputChunk, terminal: &Arc<TerminalSnapshot>) {
        let mut state = lock(&self.state);
        let required = chunk.bytes.len();
        if state.queued_bytes.saturating_add(required) > self.limit {
            let missing = state.expected_offset..terminal.watermark;
            state.replay = None;
            state.snapshot = Some(SnapshotCursor::new(missing, Arc::clone(terminal)));
            state.events.clear();
            state.queued_bytes = 0;
        } else {
            state.queued_bytes += required;
            state.events.push_back(OutputEvent::Output(chunk));
        }
        drop(state);
        self.notify.notify_one();
    }

    fn push_terminal(&self, next_offset: u64) {
        let mut state = lock(&self.state);
        state.events.push_back(OutputEvent::Exit { next_offset });
        state.closed = true;
        drop(state);
        self.notify.notify_one();
    }
}

#[derive(Debug)]
enum QueueSeed {
    Replay(Range<u64>),
    Gap {
        missing: Range<u64>,
        terminal: Arc<TerminalSnapshot>,
    },
    Snapshot {
        terminal: Arc<TerminalSnapshot>,
    },
}

#[derive(Debug)]
struct ReplayCursor {
    next: u64,
    end: u64,
    empty_pending: bool,
}

impl ReplayCursor {
    fn new(range: Range<u64>) -> Self {
        Self {
            next: range.start,
            end: range.end,
            empty_pending: range.is_empty(),
        }
    }
}

#[derive(Debug)]
struct SnapshotCursor {
    missing: Range<u64>,
    terminal: Arc<TerminalSnapshot>,
    gap_pending: bool,
    next: usize,
}

impl SnapshotCursor {
    fn new(missing: Range<u64>, terminal: Arc<TerminalSnapshot>) -> Self {
        Self {
            missing,
            terminal,
            gap_pending: true,
            next: 0,
        }
    }

    fn without_gap(terminal: Arc<TerminalSnapshot>) -> Self {
        let watermark = terminal.watermark;
        Self {
            missing: watermark..watermark,
            terminal,
            gap_pending: false,
            next: 0,
        }
    }
}

#[derive(Debug)]
struct SubscriberState {
    replay: Option<ReplayCursor>,
    snapshot: Option<SnapshotCursor>,
    events: VecDeque<OutputEvent>,
    queued_bytes: usize,
    expected_offset: u64,
    closed: bool,
}

impl SubscriberState {
    fn next_event(&mut self, hub: &HubState, event_limit: usize) -> Option<OutputEvent> {
        if let Some(replay) = &mut self.replay {
            if replay.empty_pending {
                replay.empty_pending = false;
                let offset = replay.next;
                self.replay = None;
                return Some(OutputEvent::Replay(OutputChunk {
                    offset,
                    bytes: Vec::new(),
                }));
            }
            if let Some(chunk) = hub.ring.chunk(replay.next, replay.end, event_limit) {
                replay.next = chunk.end();
                self.expected_offset = replay.next;
                if replay.next == replay.end {
                    self.replay = None;
                }
                return Some(OutputEvent::Replay(chunk));
            }

            let missing = self.expected_offset..hub.ring.next_offset;
            self.replay = None;
            self.snapshot = Some(SnapshotCursor::new(
                missing,
                Arc::clone(&hub.terminal_snapshot),
            ));
            self.events.clear();
            self.queued_bytes = 0;
        }

        if let Some(snapshot) = &mut self.snapshot {
            if snapshot.gap_pending {
                snapshot.gap_pending = false;
                self.expected_offset = snapshot.terminal.watermark;
                return Some(OutputEvent::Gap {
                    missing: snapshot.missing.clone(),
                    watermark: snapshot.terminal.watermark,
                });
            }
            if snapshot.next < snapshot.terminal.ansi.len() {
                let end = snapshot
                    .next
                    .saturating_add(event_limit)
                    .min(snapshot.terminal.ansi.len());
                let event = OutputEvent::TerminalSnapshot(TerminalChunk {
                    terminal: Arc::clone(&snapshot.terminal),
                    offset: snapshot.next,
                    total_bytes: snapshot.terminal.ansi.len(),
                    bytes: snapshot.terminal.ansi[snapshot.next..end].to_vec(),
                });
                snapshot.next = end;
                if snapshot.next == snapshot.terminal.ansi.len() {
                    self.snapshot = None;
                }
                return Some(event);
            }
            self.snapshot = None;
        }

        let event = self.events.pop_front()?;
        self.queued_bytes = self.queued_bytes.saturating_sub(event.payload_bytes());
        self.expected_offset = event.next_offset();
        Some(event)
    }
}

#[derive(Debug)]
struct OutputRing {
    bytes: VecDeque<u8>,
    limit: usize,
    start_offset: u64,
    next_offset: u64,
}

impl OutputRing {
    fn new(limit: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            limit,
            start_offset: 0,
            next_offset: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<OutputChunk, RingError> {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let next = self
            .next_offset
            .checked_add(len)
            .ok_or(RingError::OffsetOverflow)?;
        let chunk = OutputChunk {
            offset: self.next_offset,
            bytes: bytes.to_vec(),
        };
        self.bytes.extend(bytes.iter().copied());
        self.next_offset = next;
        while self.bytes.len() > self.limit {
            let _ = self.bytes.pop_front();
            self.start_offset = self
                .start_offset
                .checked_add(1)
                .ok_or(RingError::OffsetOverflow)?;
        }
        Ok(chunk)
    }

    fn bytes_from(&self, offset: u64) -> Vec<u8> {
        let relative =
            usize::try_from(offset.saturating_sub(self.start_offset)).unwrap_or(self.bytes.len());
        self.bytes.iter().skip(relative).copied().collect()
    }

    fn chunk(&self, offset: u64, end: u64, limit: usize) -> Option<OutputChunk> {
        if offset < self.start_offset || offset >= end || end > self.next_offset {
            return None;
        }
        let relative = usize::try_from(offset.checked_sub(self.start_offset)?).ok()?;
        let remaining = usize::try_from(end.checked_sub(offset)?).ok()?;
        let bytes = self
            .bytes
            .iter()
            .skip(relative)
            .take(remaining.min(limit))
            .copied()
            .collect();
        Some(OutputChunk { offset, bytes })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{OutputEvent, OutputHub, OutputSnapshot};
    use pohunek_terminal::TerminalSnapshot;
    use pohunek_worker_protocol::MAX_DATA_PAYLOAD_BYTES;

    #[test]
    fn offsets_advance_and_eviction_reports_gap() {
        let hub = OutputHub::new(4, 64, 2, 10).expect("hub");
        hub.push(b"abc").expect("push");
        hub.push(b"def").expect("push");

        assert_eq!(hub.next_offset(), 6);
        assert!(matches!(
            hub.snapshot(Some(0)).expect("snapshot"),
            OutputSnapshot::Gap { missing, .. } if missing == (0..6)
        ));
        assert!(matches!(
            hub.snapshot(Some(2)).expect("snapshot"),
            OutputSnapshot::Replay { chunk, watermark: 6 }
                if chunk.offset == 2 && chunk.bytes == b"cdef"
        ));
    }

    #[tokio::test]
    async fn subscribe_has_no_gap_between_replay_and_live_output() {
        let hub = OutputHub::new(64, 64, 2, 10).expect("hub");
        hub.push(b"before").expect("push");
        let mut subscriber = hub.subscribe(None).expect("subscribe");
        hub.push(b"after").expect("push");

        assert!(matches!(
            subscriber.recv().await,
            Some(OutputEvent::Replay(chunk)) if chunk.bytes == b"before"
        ));
        assert!(matches!(
            subscriber.recv().await,
            Some(OutputEvent::Output(chunk))
                if chunk.offset == 6 && chunk.bytes == b"after"
        ));
    }

    #[tokio::test]
    async fn terminal_snapshot_subscription_skips_history_and_continues_at_watermark() {
        let hub = OutputHub::new(1024, 4096, 24, 80).expect("hub");
        hub.push(b"\x1b[2J\x1b[Hfirst-size")
            .expect("initial output");
        hub.resize(40, 120);
        hub.push(b"\x1b[2J\x1b[Hsecond-size")
            .expect("resized output");
        hub.resize(12, 60);
        hub.push(b"\x1b[2J\x1b[Hfinal-size").expect("final output");
        let watermark = hub.next_offset();

        let mut subscriber = hub.subscribe_terminal_snapshot();
        hub.push(b"-live").expect("live output");

        let Some(OutputEvent::TerminalSnapshot(snapshot)) = subscriber.recv().await else {
            panic!("fresh attach must begin with a terminal snapshot");
        };
        assert_eq!(snapshot.terminal().watermark, watermark);
        assert_eq!(snapshot.terminal().rows, 12);
        assert_eq!(snapshot.terminal().cols, 60);
        assert!(snapshot.terminal().visible_text.contains("final-size"));
        assert!(!snapshot.terminal().visible_text.contains("first-size"));
        assert!(matches!(
            subscriber.recv().await,
            Some(OutputEvent::Output(chunk))
                if chunk.offset == watermark && chunk.bytes == b"-live"
        ));
    }

    #[tokio::test]
    async fn daemon_outage_replays_exact_counter_range_from_last_offset() {
        let hub = OutputHub::new(256, 256, 4, 40).expect("hub");
        let before = b"counter:0001\n";
        let during = b"counter:0002\ncounter:0003\n";
        hub.push(before).expect("push pre-outage counter");
        let last_daemon_offset = hub.next_offset();
        hub.push(during).expect("push outage counters");

        let mut replacement = hub
            .subscribe(Some(last_daemon_offset))
            .expect("replacement daemon subscribes");

        assert!(matches!(
            replacement.recv().await,
            Some(OutputEvent::Replay(chunk))
                if chunk.offset == last_daemon_offset && chunk.bytes == during
        ));
    }

    #[tokio::test]
    async fn forced_ring_overrun_delivers_gap_and_complete_terminal_repaint() {
        let hub = OutputHub::new(8, 128, 3, 40).expect("hub");
        hub.push(b"\x1b[2J\x1b[Hcounter:0042")
            .expect("push terminal state");
        hub.push(b"-after-outage").expect("force ring eviction");

        let OutputSnapshot::Gap { missing, terminal } =
            hub.snapshot(Some(0)).expect("gap snapshot")
        else {
            panic!("evicted offset must produce a gap");
        };
        assert_eq!(missing, 0..hub.next_offset());
        assert_eq!(terminal.watermark, hub.next_offset());
        assert!(terminal.visible_text.contains("counter:0042-after-outage"));
        assert!(terminal.ansi.starts_with(b"\x1b[2J\x1b[H"));

        let expected_ansi = terminal.ansi;
        let mut subscriber = hub.subscribe(Some(0)).expect("gap subscribe");
        assert!(matches!(
            subscriber.recv().await,
            Some(OutputEvent::Gap { missing, watermark })
                if missing == (0..hub.next_offset()) && watermark == hub.next_offset()
        ));
        let mut repaint = Vec::with_capacity(expected_ansi.len());
        while repaint.len() < expected_ansi.len() {
            let Some(OutputEvent::TerminalSnapshot(chunk)) = subscriber.recv().await else {
                panic!("gap must be followed by the complete terminal repaint");
            };
            assert_eq!(chunk.offset, repaint.len());
            assert_eq!(chunk.total_bytes, expected_ansi.len());
            repaint.extend_from_slice(&chunk.bytes);
        }
        assert_eq!(repaint, expected_ansi);
    }

    #[tokio::test]
    async fn slow_subscriber_gets_explicit_gap_and_terminal_snapshot() {
        let hub = OutputHub::new(128, 8, 2, 20).expect("hub");
        let mut subscriber = hub.subscribe(Some(0)).expect("subscribe");
        hub.push(b"123456").expect("push");
        hub.push(b"abcdef").expect("push");

        assert!(matches!(
            subscriber.recv().await,
            Some(OutputEvent::Gap { missing, watermark })
                if missing.start == 0
                    && missing.end == 12
                    && watermark == 12
        ));
        assert!(matches!(
            subscriber.recv().await,
            Some(OutputEvent::TerminalSnapshot(chunk))
                if chunk.terminal().watermark == 12 && chunk.bytes.len() <= 8
        ));
    }

    #[tokio::test]
    async fn exit_is_delivered_after_queued_output() {
        let hub = OutputHub::new(64, 64, 2, 10).expect("hub");
        let mut subscriber = hub.subscribe(None).expect("subscribe");
        hub.push(b"x").expect("push");
        hub.mark_exit();

        let _initial = subscriber.recv().await;
        let _output = subscriber.recv().await;
        assert_eq!(
            subscriber.recv().await,
            Some(OutputEvent::Exit { next_offset: 1 })
        );
        assert_eq!(subscriber.recv().await, None);
    }

    #[tokio::test]
    async fn fresh_subscriber_replays_more_than_one_wire_frame_exactly_once() {
        let retained_bytes = MAX_DATA_PAYLOAD_BYTES + 257;
        let payload = (0..retained_bytes)
            .map(|index| u8::try_from(index % 251).expect("value fits u8"))
            .collect::<Vec<_>>();
        let subscriber_limit = 64;
        let hub = OutputHub::new(retained_bytes + 1, subscriber_limit, 2, 10).expect("hub");
        hub.push(&payload).expect("retain output");
        let mut subscriber = hub.subscribe(None).expect("fresh subscribe");
        let mut replay = Vec::with_capacity(payload.len());
        let mut expected_offset = 0_u64;

        while replay.len() < payload.len() {
            assert!(super::lock(&subscriber.queue.state).queued_bytes <= subscriber_limit);
            let Some(OutputEvent::Replay(chunk)) = subscriber.recv().await else {
                panic!("fresh subscriber must receive only replay chunks");
            };
            assert_eq!(chunk.offset, expected_offset);
            assert!(chunk.bytes.len() <= subscriber_limit);
            expected_offset += u64::try_from(chunk.bytes.len()).expect("chunk length fits u64");
            replay.extend_from_slice(&chunk.bytes);
        }

        assert_eq!(replay, payload);
        assert_eq!(
            expected_offset,
            u64::try_from(retained_bytes).expect("test length fits u64")
        );
    }

    #[tokio::test]
    async fn replay_cursor_eviction_switches_to_current_gap_snapshot() {
        let hub = OutputHub::new(8, 64, 2, 10).expect("hub");
        hub.push(b"abcdefgh").expect("initial history");
        let mut subscriber = hub.subscribe(None).expect("lazy replay subscribe");
        hub.push(b"ijklmnop").expect("evict pending replay");

        assert!(matches!(
            subscriber.recv().await,
            Some(OutputEvent::Gap { missing, watermark })
                if missing == (0..16) && watermark == 16
        ));
        assert!(matches!(
            subscriber.recv().await,
            Some(OutputEvent::TerminalSnapshot(chunk))
                if chunk.terminal().watermark == 16
                    && chunk.total_bytes == chunk.terminal().ansi.len()
        ));
        assert!(super::lock(&subscriber.queue.state).queued_bytes <= 64);
    }

    #[tokio::test]
    async fn oversized_gap_repaint_is_delivered_as_bounded_events_before_exit() {
        let subscriber_limit = 32;
        let ansi = (0..MAX_DATA_PAYLOAD_BYTES + 257)
            .map(|index| u8::try_from(index % 251).expect("value fits u8"))
            .collect::<Vec<_>>();
        let terminal = TerminalSnapshot {
            watermark: 42,
            rows: 2,
            cols: 10,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            alternate_screen: false,
            title: None,
            progress: None,
            visible_text: "bounded repaint".to_owned(),
            ansi: ansi.clone(),
        };
        let hub = OutputHub::new(1, subscriber_limit, 2, 10).expect("hub");
        let queue = std::sync::Arc::new(super::SubscriberQueue::new(
            subscriber_limit,
            super::QueueSeed::Gap {
                missing: 0..42,
                terminal: std::sync::Arc::new(terminal),
            },
        ));
        queue.push_terminal(42);
        let mut subscriber = super::OutputSubscriber {
            hub: std::sync::Arc::clone(&hub.inner),
            queue,
        };

        assert_eq!(
            subscriber.recv().await,
            Some(OutputEvent::Gap {
                missing: 0..42,
                watermark: 42,
            })
        );
        let mut repaint = Vec::with_capacity(ansi.len());
        while repaint.len() < ansi.len() {
            assert!(super::lock(&subscriber.queue.state).queued_bytes <= subscriber_limit);
            let Some(OutputEvent::TerminalSnapshot(chunk)) = subscriber.recv().await else {
                panic!("snapshot must remain open until every repaint chunk is delivered");
            };
            assert_eq!(chunk.offset, repaint.len());
            assert_eq!(chunk.total_bytes, ansi.len());
            assert!(chunk.bytes.len() <= subscriber_limit);
            repaint.extend_from_slice(&chunk.bytes);
        }

        assert_eq!(repaint, ansi);
        assert_eq!(
            subscriber.recv().await,
            Some(OutputEvent::Exit { next_offset: 42 })
        );
        assert_eq!(subscriber.recv().await, None);
    }
}
