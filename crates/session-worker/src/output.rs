//! Maintains bounded output history and atomic subscriptions.

// Rust guideline compliant 2026-07-23

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;

use pohunek_terminal::{TerminalSnapshot, TerminalTracker};

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
        /// Current terminal snapshot.
        terminal: TerminalSnapshot,
    },
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
            Self::Gap { terminal, .. } => terminal.ansi.len(),
            Self::Exit { .. } => 0,
        }
    }

    fn next_offset(&self) -> u64 {
        match self {
            Self::Replay(chunk) | Self::Output(chunk) => chunk.end(),
            Self::Gap { terminal, .. } => terminal.watermark,
            Self::Exit { next_offset } => *next_offset,
        }
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
        Ok(Self {
            inner: Arc::new(Mutex::new(HubState {
                ring: OutputRing::new(history_limit),
                terminal: TerminalTracker::new(rows, cols),
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
        let snapshot = state.terminal.snapshot(state.ring.next_offset);
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
        let snapshot = state.snapshot(after_offset)?;
        let initial = match snapshot {
            OutputSnapshot::Replay { chunk, .. } => OutputEvent::Replay(chunk),
            OutputSnapshot::Gap { missing, terminal } => OutputEvent::Gap { missing, terminal },
        };
        let queue = Arc::new(SubscriberQueue::new(self.subscriber_limit, initial));
        if state.exited {
            queue.push_terminal(state.ring.next_offset);
        } else {
            state.subscribers.push(Arc::downgrade(&queue));
        }
        Ok(OutputSubscriber { queue })
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
        lock(&self.inner).terminal.resize(rows, cols);
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
        state.terminal.snapshot(state.ring.next_offset)
    }
}

#[derive(Debug)]
struct HubState {
    ring: OutputRing,
    terminal: TerminalTracker,
    subscribers: Vec<Weak<SubscriberQueue>>,
    exited: bool,
}

impl HubState {
    fn snapshot(&self, after_offset: Option<u64>) -> Result<OutputSnapshot, RingError> {
        // A fresh consumer starts at generation offset zero. If early bytes
        // were evicted, replaying only the retained suffix could begin inside
        // an escape sequence and cannot reconstruct the screen; emit the same
        // explicit gap plus complete terminal snapshot used by reconnects.
        let requested = after_offset.unwrap_or(0);
        if requested > self.ring.next_offset {
            return Err(RingError::OffsetAhead {
                requested,
                next: self.ring.next_offset,
            });
        }
        if requested < self.ring.start_offset {
            return Ok(OutputSnapshot::Gap {
                missing: requested..self.ring.next_offset,
                terminal: self.terminal.snapshot(self.ring.next_offset),
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
}

/// Receives bounded output events.
#[derive(Debug)]
pub struct OutputSubscriber {
    queue: Arc<SubscriberQueue>,
}

impl OutputSubscriber {
    /// Waits for the next output event.
    pub async fn recv(&mut self) -> Option<OutputEvent> {
        loop {
            let notified = self.queue.notify.notified();
            {
                let mut state = lock(&self.queue.state);
                if let Some(event) = state.events.pop_front() {
                    state.queued_bytes = state.queued_bytes.saturating_sub(event.payload_bytes());
                    state.expected_offset = event.next_offset();
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
    state: Mutex<SubscriberState>,
    notify: Notify,
}

impl SubscriberQueue {
    fn new(limit: usize, initial: OutputEvent) -> Self {
        let initial_bytes = initial.payload_bytes();
        let expected_offset = match &initial {
            OutputEvent::Replay(chunk) | OutputEvent::Output(chunk) => chunk.offset,
            OutputEvent::Gap { missing, .. } => missing.start,
            OutputEvent::Exit { next_offset } => *next_offset,
        };
        let mut events = VecDeque::new();
        events.push_back(initial);
        Self {
            limit,
            state: Mutex::new(SubscriberState {
                events,
                queued_bytes: initial_bytes,
                expected_offset,
                closed: initial_bytes > limit,
            }),
            notify: Notify::new(),
        }
    }

    fn push_output(&self, chunk: OutputChunk, terminal: &TerminalSnapshot) {
        let mut state = lock(&self.state);
        let required = chunk.bytes.len();
        if state.queued_bytes.saturating_add(required) > self.limit {
            let missing = state.expected_offset..terminal.watermark;
            state.events.clear();
            state.queued_bytes = terminal.ansi.len();
            state.events.push_back(OutputEvent::Gap {
                missing,
                terminal: terminal.clone(),
            });
            if state.queued_bytes > self.limit {
                state.closed = true;
            }
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
struct SubscriberState {
    events: VecDeque<OutputEvent>,
    queued_bytes: usize,
    expected_offset: u64,
    closed: bool,
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
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{OutputEvent, OutputHub, OutputSnapshot};

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

    #[test]
    fn forced_ring_overrun_returns_gap_and_complete_terminal_repaint() {
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
    }

    #[tokio::test]
    async fn slow_subscriber_gets_explicit_gap_and_terminal_snapshot() {
        let hub = OutputHub::new(128, 8, 2, 20).expect("hub");
        let mut subscriber = hub.subscribe(Some(0)).expect("subscribe");
        hub.push(b"123456").expect("push");
        hub.push(b"abcdef").expect("push");

        assert!(matches!(
            subscriber.recv().await,
            Some(OutputEvent::Gap { missing, terminal })
                if missing.start == 0
                    && missing.end == 12
                    && terminal.watermark == 12
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
}
