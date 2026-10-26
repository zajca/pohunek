//! Pending input of a session's watchers.
//!
//! A watcher (the procwatch scan loop, the detector, the worker exit watcher,
//! the exit watches) applies input to a session: a scan request, an output
//! chunk, a snapshot, an exit. Each such input is counted from the moment it is
//! handed to the watcher until the watcher has applied it, including its store
//! write and event emission. `SessionRegistry::settled` (test builds) waits for
//! that count and for the transactions in flight to be zero together.
//!
//! What a count promises: everything the daemon *received or was asked to
//! process* has been applied. It cannot see input still on its way from outside
//! the daemon (a worker snapshot not yet read, an OS exit not yet delivered), so
//! a test that waits for "nothing else happened" after such an event first
//! awaits a positive signal that the event arrived. Work that is merely
//! scheduled (a scan timer, a retry delay) is not pending until it fires.
//!
//! Producer to gate, one gate per input source of a session:
//!
//! | gate | producer | consumer | counted |
//! |---|---|---|---|
//! | procwatch (`RescanSignal`) | every `RescanSignal::request` | scan loop | at the request, taken when the loop wakes for it; a scan the loop starts on its timer is counted from its first read until applied |
//! | detector (`DetectorOutput`) | detector output pump | detector | at the send, one per chunk, `skipped` on a lag; a detector tick is counted until its transitions are applied |
//! | worker snapshot | worker poll | worker exit watcher | when a snapshot is read, until applied |
//! | exit watch | OS exit watch | bound-reporter and observed-agent exit watch tasks | when the exit is delivered, until applied |
//! | supervision retry | retry loop | retry loop | when a pending session is taken after its delay, until its pass is accounted for |
//! | unattributed | OS exit watch | external exit watch task | like the exit watches; belongs to no session |

// Rust guideline compliant 2026-10-01

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use tokio::sync::{watch, Notify};

use super::SessionId;

/// What the sources of one session have not finished applying.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Pending {
    /// Inputs handed to a watcher and not yet applied.
    count: usize,
    /// Number of inputs ever counted; a wait compares it to see that nothing
    /// arrived while it looked at the other counters.
    generation: u64,
}

/// The count of pending input of one session, summed over its gates.
#[derive(Debug, Default)]
pub(super) struct SessionSettle {
    pending: watch::Sender<Pending>,
}

impl SessionSettle {
    fn add(&self, count: usize) {
        if count > 0 {
            self.pending.send_modify(|pending| {
                pending.count += count;
                pending.generation = pending.generation.wrapping_add(1);
            });
        }
    }

    fn sub(&self, count: usize) {
        if count > 0 {
            self.pending
                .send_modify(|pending| pending.count = pending.count.saturating_sub(count));
        }
    }

    /// Inputs ever counted.
    #[cfg(test)]
    pub(super) fn generation(&self) -> u64 {
        self.pending.borrow().generation
    }

    /// Whether no input is pending right now.
    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        self.pending.borrow().count == 0
    }

    /// Waits until no input is pending. Parks on the counter, never on a timer.
    #[cfg(test)]
    pub(super) async fn idle(&self) {
        let mut pending = self.pending.subscribe();
        let _ = pending.wait_for(|pending| pending.count == 0).await;
    }
}

#[derive(Debug, Default)]
struct GateState {
    /// Inputs noted and not yet taken by the consumer.
    held: usize,
    /// Set when the consumer ended: later inputs are not counted.
    closed: bool,
}

/// The input source of one watcher of one session.
///
/// The producer calls [`Self::note`] for every input it hands over, before the
/// consumer can see it. The consumer takes what it is about to apply
/// ([`Self::take`], [`Self::take_all`]) and holds the returned [`Applying`] until
/// the input is applied. A source that has no producer in the daemon (a timer, an
/// OS exit) counts the input when the consumer receives it ([`Self::begin`]).
#[derive(Debug)]
pub(super) struct InputGate {
    session: Arc<SessionSettle>,
    state: Mutex<GateState>,
}

impl InputGate {
    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one input handed to the consumer. Does nothing once the gate is
    /// closed, so an input aimed at an ended watcher cannot stay pending.
    pub(super) fn note(&self) {
        let mut state = self.state();
        if !state.closed {
            state.held += 1;
            self.session.add(1);
        }
    }

    /// Takes up to `count` of the noted inputs; they stay pending until the
    /// returned guard drops.
    pub(super) fn take(self: &Arc<Self>, count: usize) -> Applying {
        let mut state = self.state();
        let taken = count.min(state.held);
        state.held -= taken;
        Applying {
            gate: Arc::clone(self),
            count: taken,
        }
    }

    /// Takes every noted input. A source that coalesces its wakeups (a
    /// `Notify`) is applied once for all the requests it merged.
    pub(super) fn take_all(self: &Arc<Self>) -> Applying {
        let mut state = self.state();
        let taken = std::mem::take(&mut state.held);
        Applying {
            gate: Arc::clone(self),
            count: taken,
        }
    }

    /// Counts one input the consumer itself received, until the guard drops.
    pub(super) fn begin(self: &Arc<Self>) -> Applying {
        let state = self.state();
        let count = usize::from(!state.closed);
        self.session.add(count);
        drop(state);
        Applying {
            gate: Arc::clone(self),
            count,
        }
    }

    /// Ends the consumer: what was noted and never taken is released, and later
    /// inputs are not counted. Idempotent.
    pub(super) fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        let held = std::mem::take(&mut state.held);
        self.session.sub(held);
    }
}

/// Releases the inputs it holds when dropped, after the consumer applied them.
#[must_use = "the inputs are applied while the guard is held"]
#[derive(Debug)]
pub(super) struct Applying {
    gate: Arc<InputGate>,
    count: usize,
}

impl Drop for Applying {
    fn drop(&mut self) {
        self.gate.session.sub(self.count);
    }
}

/// Requests for a rescan of one session, as a wakeup the scan loop waits on.
///
/// Every request is counted before the loop is woken, so the request cannot be
/// missed by `SessionRegistry::settled`: the wakeup is
/// only reachable through [`Self::request`]. Requests that arrive while the loop
/// is busy merge into one wakeup and are applied by one scan.
#[derive(Debug)]
pub(super) struct RescanSignal {
    wake: Notify,
    gate: Arc<InputGate>,
}

impl RescanSignal {
    pub(super) fn new(gate: Arc<InputGate>) -> Arc<Self> {
        Arc::new(Self {
            wake: Notify::new(),
            gate,
        })
    }

    /// A signal nobody listens to, for a session entry without a scan loop: its
    /// requests are not counted.
    pub(super) fn detached(gate: Arc<InputGate>) -> Arc<Self> {
        gate.close();
        Self::new(gate)
    }

    /// Asks the scan loop of the session to scan now.
    pub(super) fn request(&self) {
        self.gate.note();
        self.wake.notify_one();
    }

    /// Waits for a request and takes every request made so far.
    pub(super) async fn requested(&self) -> Applying {
        self.wake.notified().await;
        self.gate.take_all()
    }

    /// Counts a scan the loop starts on its own timer, until the guard drops.
    pub(super) fn begin(&self) -> Applying {
        self.gate.begin()
    }

    /// The gate of the scan loop, which closes it when the loop ends.
    pub(super) fn gate(&self) -> &Arc<InputGate> {
        &self.gate
    }
}

/// Closes its gate when the consumer's task ends, however it ends.
#[derive(Debug)]
pub(super) struct CloseOnDrop(pub(super) Arc<InputGate>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The pending-input counts of every session of one registry.
#[derive(Debug, Default)]
pub(super) struct SettleRegistry {
    sessions: Mutex<HashMap<SessionId, Weak<SessionSettle>>>,
    /// Input that belongs to no session, such as the exit of an external agent.
    unattributed: Arc<SessionSettle>,
}

impl SettleRegistry {
    /// A new gate on the counts of session `id`.
    ///
    /// The counts of a session live as long as one of its gates does; a session
    /// with no live gate has nothing pending.
    pub(super) fn gate(&self, id: &SessionId) -> Arc<InputGate> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let session = match sessions.get(id).and_then(Weak::upgrade) {
            Some(session) => session,
            None => {
                sessions.retain(|_, session| session.strong_count() > 0);
                let session = Arc::new(SessionSettle::default());
                sessions.insert(id.clone(), Arc::downgrade(&session));
                session
            }
        };
        Self::gate_on(session)
    }

    /// A new gate on the input that belongs to no session.
    pub(super) fn unattributed_gate(&self) -> Arc<InputGate> {
        Self::gate_on(Arc::clone(&self.unattributed))
    }

    fn gate_on(session: Arc<SessionSettle>) -> Arc<InputGate> {
        Arc::new(InputGate {
            session,
            state: Mutex::new(GateState::default()),
        })
    }

    /// The counts a wait for session `id` covers: its own, when it has any, and
    /// the unattributed ones.
    #[cfg(test)]
    pub(super) fn counts_for(&self, id: &SessionId) -> Vec<Arc<SessionSettle>> {
        let own = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .and_then(Weak::upgrade);
        own.into_iter()
            .chain(std::iter::once(Arc::clone(&self.unattributed)))
            .collect()
    }
}
