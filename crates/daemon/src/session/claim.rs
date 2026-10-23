//! Ownership and ordering of the active agent claim of a session.
//!
//! A claim is either journaled by the session's worker or kept by this daemon
//! only. The two are ordered by different sequences, so each variant carries
//! what its own ordering needs and a claim is only ever compared with another
//! claim of the same variant.

use super::{IdentityFence, SessionEntry};
use crate::runtime::ExchangeOrder;
use protocol::AgentActivity;

/// The identity of a journaled claim: who claims, under which worker sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClaimKey {
    pub(super) provider: String,
    pub(super) pid: u32,
    pub(super) start_identity: u64,
    /// The worker's monotonic sequence of the claim.
    pub(super) sequence: u64,
}

impl ClaimKey {
    /// Whether `other` names the same process, whatever its sequence.
    pub(super) fn same_process(&self, pid: u32, start_identity: u64) -> bool {
        self.pid == pid && self.start_identity == start_identity
    }
}

/// The public order of the last report a caller made for a journaled claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PublicOrder {
    pub(super) source: String,
    pub(super) seq: Option<u64>,
}

/// Where the active claim of a session came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClaimOrigin {
    /// Projected from the worker's journal. `revision` is the worker identity
    /// revision that carried it, absent for a worker that predates revisions.
    Journaled {
        key: ClaimKey,
        revision: Option<IdentityFence>,
        public: Option<PublicOrder>,
    },
    /// Recorded by this daemon only, or observed from the process table.
    /// `key` names the process of a bound report; `over` is the newest worker
    /// revision, committed or accepted and waiting, at the time the claim was
    /// recorded: a snapshot at or below it does not replace the claim.
    Memory {
        key: Option<ClaimKey>,
        over: Option<IdentityFence>,
        shadow: Vec<Shadow>,
    },
}

static ADMISSIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The next position in the order forwarded operations are admitted in. The
/// identity-commit guard serializes admissions of a session, so the order of
/// two operations of one session is the order of their stamps.
pub(super) fn next_admission() -> u64 {
    ADMISSIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// A journaled operation whose revision the daemon never learned: the worker
/// may hold it, so a memory claim recorded after it holds against a snapshot
/// whose change is that operation, at any revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Shadow {
    pub(super) key: ClaimKey,
    /// The shadowed operation is the release of `key`, not its claim.
    pub(super) released: bool,
}

impl ClaimOrigin {
    /// A claim that is not tied to a journaled one.
    pub(super) const fn memory() -> Self {
        Self::Memory {
            key: None,
            over: None,
            shadow: Vec::new(),
        }
    }

    pub(super) fn key(&self) -> Option<&ClaimKey> {
        match self {
            Self::Journaled { key, .. } => Some(key),
            Self::Memory { key, .. } => key.as_ref(),
        }
    }
}

/// Whether the active claim of `entry` is the journaled claim `key`.
pub(super) fn owns(entry: &SessionEntry, key: &ClaimKey) -> bool {
    entry.active_agent.as_ref().is_some_and(
        |active| matches!(&active.origin, ClaimOrigin::Journaled { key: held, .. } if held == key),
    )
}

/// Whether `held` and `incoming` are the same claim of one process, compared
/// on every field of the key.
pub(super) fn same_claim(held: &ClaimOrigin, incoming: &ClaimOrigin) -> bool {
    matches!((held.key(), incoming.key()), (Some(held), Some(incoming)) if held == incoming)
}

/// What an accepted forwarded report leaves for the commit of its identity:
/// the public order its caller made it under, and the activity it carried, if
/// any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingActivity {
    pub(super) key: ClaimKey,
    /// The revision the worker accepted the report at, or the fence at the time
    /// its answer timed out when `acknowledged` is false.
    pub(super) revision: IdentityFence,
    /// The worker answered, so `revision` is the revision of the claim.
    pub(super) acknowledged: bool,
    pub(super) public: PublicOrder,
    pub(super) activity: Option<AgentActivity>,
    pub(super) epoch: String,
    /// Where the operation stands in this daemon's admission order.
    pub(super) admitted: u64,
    /// Where the request stands in the order the worker handles them; absent
    /// for an accepted operation.
    pub(super) sent: Option<ExchangeOrder>,
}

/// The newest snapshot of the worker's identity this daemon read, with where
/// its request stands in the order the worker handles them. It is kept apart
/// from the committed fence: a snapshot that changes nothing the daemon
/// projects is never applied, yet it still proves what the worker had handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IdentityObservation {
    pub(super) fence: IdentityFence,
    pub(super) order: ExchangeOrder,
}

/// Records the snapshot `fence` of the worker read at `order`, unless a
/// snapshot read later is held. Returns whether a pending operation of `entry`
/// is rejected by what the entry now holds.
pub(super) fn observe_identity(
    entry: &mut SessionEntry,
    fence: IdentityFence,
    order: ExchangeOrder,
) -> bool {
    if entry
        .identity_observed
        .as_ref()
        .is_none_or(|held| held.order < order)
    {
        entry.identity_observed = Some(IdentityObservation { fence, order });
    }
    has_rejected_pending(entry)
}

/// Whether an operation `entry` tracks is known to be rejected by the worker.
pub(super) fn has_rejected_pending(entry: &SessionEntry) -> bool {
    let fence = entry.identity_fence.as_ref();
    let observed = entry.identity_observed.as_ref();
    let report = entry.pending_activity.as_deref().map(Pending::Report);
    let release = entry.pending_release.as_deref().map(Pending::Release);
    let displaced = entry
        .displaced_pending
        .iter()
        .map(|operation| match operation {
            Unacknowledged::Report(pending) => Pending::Report(pending),
            Unacknowledged::Release(pending) => Pending::Release(pending),
        });
    report
        .into_iter()
        .chain(release)
        .chain(displaced)
        .any(|pending| pending.state(fence, observed) == PendingState::Rejected)
}

/// What a snapshot's fence position means for an activity waiting on `pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingState {
    /// The identity has not been committed yet; keep waiting.
    Waiting,
    /// The identity was committed, or a newer one superseded it.
    Settled,
    /// The worker instance changed, so the report cannot be committed.
    Obsolete,
    /// The worker handled the request and did not journal it.
    Rejected,
}

pub(super) fn pending_state(
    pending: &PendingActivity,
    fence: Option<&IdentityFence>,
    observed: Option<&IdentityObservation>,
) -> PendingState {
    revision_state(
        (&pending.revision, pending.acknowledged, pending.sent),
        fence,
        observed,
    )
}

/// Where a pending operation stands against the fence. An operation whose
/// answer timed out is settled only by a revision above the fence it was sent
/// at.
///
/// The worker raises its identity revision only for a request it journals, so
/// a snapshot read after the worker handled such a request that still shows the
/// revision the request was sent at proves the worker rejected it. A snapshot
/// read earlier proves nothing, because the request may still be queued, and
/// the operation waits.
fn revision_state(
    (revision, acknowledged, sent): (&IdentityFence, bool, Option<ExchangeOrder>),
    fence: Option<&IdentityFence>,
    observed: Option<&IdentityObservation>,
) -> PendingState {
    if fence.is_some_and(|fence| fence.worker_instance_id != revision.worker_instance_id) {
        return PendingState::Obsolete;
    }
    if !acknowledged && rejected_by(revision, sent, observed) {
        return PendingState::Rejected;
    }
    let Some(fence) = fence else {
        return PendingState::Waiting;
    };
    let passed = if acknowledged {
        fence.revision >= revision.revision
    } else {
        fence.revision > revision.revision
    };
    if passed {
        PendingState::Settled
    } else {
        PendingState::Waiting
    }
}

/// Whether `observed` is a snapshot of the worker instance of `revision`, read
/// after the request sent at `sent` was handled, at the revision it was sent at.
fn rejected_by(
    revision: &IdentityFence,
    sent: Option<ExchangeOrder>,
    observed: Option<&IdentityObservation>,
) -> bool {
    let (Some(sent), Some(observed)) = (sent, observed) else {
        return false;
    };
    observed.fence.worker_instance_id == revision.worker_instance_id
        && observed.order > sent
        && observed.fence.revision == revision.revision
}

/// What the worker's journal holds for the session as far as this daemon
/// knows.
pub(super) enum JournalState<'a> {
    /// No journaled claim.
    Empty,
    /// A claim: the one of a forwarded report the worker accepted and the daemon
    /// has not adopted yet, else the journaled active claim.
    Claim(&'a ClaimKey, Option<&'a PublicOrder>),
    /// The claim was released by a forwarded release the daemon has not adopted
    /// yet; the release carries its caller's label and public order.
    Released(&'a PublicOrder, &'a str),
}

/// A forwarded operation the worker accepted, or may have accepted, and the
/// daemon has not adopted.
enum Pending<'a> {
    Report(&'a PendingActivity),
    Release(&'a PendingRelease),
}

impl Pending<'_> {
    fn revision(&self) -> &IdentityFence {
        match self {
            Self::Report(pending) => &pending.revision,
            Self::Release(pending) => &pending.revision,
        }
    }

    fn acknowledged(&self) -> bool {
        match self {
            Self::Report(pending) => pending.acknowledged,
            Self::Release(pending) => pending.acknowledged,
        }
    }

    fn sent(&self) -> Option<ExchangeOrder> {
        match self {
            Self::Report(pending) => pending.sent,
            Self::Release(pending) => pending.sent,
        }
    }

    fn state(
        &self,
        fence: Option<&IdentityFence>,
        observed: Option<&IdentityObservation>,
    ) -> PendingState {
        revision_state(
            (self.revision(), self.acknowledged(), self.sent()),
            fence,
            observed,
        )
    }

    fn shadow(&self) -> Shadow {
        match self {
            Self::Report(pending) => Shadow {
                key: pending.key.clone(),
                released: false,
            },
            Self::Release(pending) => Shadow {
                key: pending.key(),
                released: true,
            },
        }
    }

    fn admitted(&self) -> u64 {
        match self {
            Self::Report(pending) => pending.admitted,
            Self::Release(pending) => pending.admitted,
        }
    }

    /// Later operations rank higher; one whose answer timed out ranks above an
    /// accepted one at the same revision, and the admission order decides
    /// between operations that tie.
    fn rank(&self) -> (u64, bool, u64) {
        (
            self.revision().revision,
            !self.acknowledged(),
            self.admitted(),
        )
    }
}

/// The operations of `entry` that wait for their adoption on the worker
/// instance in use, apart from the ones the worker rejected.
fn pending_operations(entry: &SessionEntry) -> impl Iterator<Item = Pending<'_>> {
    let fence = entry.identity_fence.as_ref();
    let observed = entry.identity_observed.as_ref();
    let report = entry.pending_activity.as_deref().map(Pending::Report);
    let release = entry.pending_release.as_deref().map(Pending::Release);
    let displaced = entry
        .displaced_pending
        .iter()
        .map(|operation| match operation {
            Unacknowledged::Report(pending) => Pending::Report(pending),
            Unacknowledged::Release(pending) => Pending::Release(pending),
        });
    report
        .into_iter()
        .chain(release)
        .chain(displaced)
        .filter(move |pending| {
            !matches!(
                pending.state(fence, observed),
                PendingState::Obsolete | PendingState::Rejected
            )
        })
}

/// The floor and shadow of the memory claim, or of the memory release that
/// ended one, that sits above the journal.
fn memory_overlay(entry: &SessionEntry) -> Option<(Option<&IdentityFence>, &[Shadow])> {
    let origin = match entry.active_agent.as_ref() {
        Some(active) if !active.observed => Some(&active.origin),
        Some(_) => None,
        None => entry.last_agent_report.as_ref().map(|last| &last.origin),
    };
    match origin {
        Some(ClaimOrigin::Memory { over, shadow, .. }) => Some((over.as_ref(), shadow.as_slice())),
        _ => None,
    }
}

/// Whether a memory claim recorded after `pending` was accepted sits above it.
fn overlay_above(entry: &SessionEntry, pending: &Pending<'_>) -> bool {
    let Some((over, shadow)) = memory_overlay(entry) else {
        return false;
    };
    if pending.acknowledged() {
        over.is_some_and(|over| {
            over.worker_instance_id == pending.revision().worker_instance_id
                && over.revision >= pending.revision().revision
        })
    } else {
        shadow.contains(&pending.shadow())
    }
}

/// What the journal holds. A pending operation counts as held, unless a memory
/// claim recorded after it sits above it: that claim is then judged as it
/// would be once the operation is adopted.
pub(super) fn journal_state(entry: &SessionEntry) -> JournalState<'_> {
    let newest = pending_operations(entry)
        .filter(|pending| !overlay_above(entry, pending))
        .max_by_key(Pending::rank);
    match newest {
        Some(Pending::Report(pending)) => {
            return JournalState::Claim(&pending.key, Some(&pending.public))
        }
        Some(Pending::Release(pending)) => {
            return JournalState::Released(&pending.public, &pending.agent)
        }
        None => {}
    }
    match entry.active_agent.as_ref().map(|active| &active.origin) {
        Some(ClaimOrigin::Journaled {
            key,
            public,
            revision: Some(_),
        }) => JournalState::Claim(key, public.as_ref()),
        _ => JournalState::Empty,
    }
}

/// The floor and shadow a memory claim recorded now carries: the newest worker
/// revision committed or accepted, and the operation the worker may hold at a
/// revision this daemon does not know.
pub(super) fn memory_floor(entry: &SessionEntry) -> (Option<IdentityFence>, Vec<Shadow>) {
    let mut over = entry.identity_fence.clone();
    let mut shadow = Vec::new();
    for pending in pending_operations(entry) {
        if pending.acknowledged() {
            let newer = over.as_ref().is_none_or(|over| {
                over.worker_instance_id == pending.revision().worker_instance_id
                    && over.revision < pending.revision().revision
            });
            if newer {
                over = Some(pending.revision().clone());
            }
        } else {
            shadow.push(pending.shadow());
        }
    }
    (over, shadow)
}

/// Whether a report this daemon records in memory only may replace the active
/// claim of `entry`. A claim the journal holds is replaced only by a report of
/// the same process whose public sequence is newer than the last one that
/// caller made for it; the monotonic sequence of the journal is never compared
/// with it. Once the journal has released the claim, a report is judged against
/// that release only.
pub(super) fn memory_report_may_replace(
    entry: &SessionEntry,
    source: &str,
    agent: &str,
    seq: Option<u64>,
    process: (Option<u32>, Option<u64>),
) -> bool {
    match journal_state(entry) {
        JournalState::Empty => true,
        JournalState::Released(order, released) => {
            order.source != source
                || released != agent
                || super::report_seq_is_newer(order.seq, seq)
        }
        JournalState::Claim(key, public) => {
            let (Some(pid), Some(start_identity)) = process else {
                return false;
            };
            if !key.same_process(pid, start_identity) {
                return false;
            }
            match public {
                Some(order) if order.source == source => super::report_seq_is_newer(order.seq, seq),
                _ => true,
            }
        }
    }
}

/// Whether the active claim of `entry` was recorded by this daemon only and
/// is not an observed agent.
pub(super) fn is_memory_report(entry: &SessionEntry) -> bool {
    entry.active_agent.as_ref().is_some_and(|active| {
        !active.observed && matches!(active.origin, ClaimOrigin::Memory { .. })
    })
}

/// Whether the worker's journal holds the active claim of `entry`, or the
/// claim of a forwarded report it accepted.
pub(super) fn is_journaled(entry: &SessionEntry) -> bool {
    !matches!(journal_state(entry), JournalState::Empty)
}

/// Where a forwarded release leaves the public order of its caller until the
/// daemon adopts the release the worker journaled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingRelease {
    pub(super) provider: String,
    pub(super) pid: u32,
    pub(super) start_identity: u64,
    pub(super) sequence: u64,
    /// The revision the worker accepted the release at, or the fence at the time
    /// its answer timed out when `acknowledged` is false.
    pub(super) revision: IdentityFence,
    pub(super) acknowledged: bool,
    /// The label the caller released and its public order.
    pub(super) agent: String,
    pub(super) public: PublicOrder,
    /// Where the operation stands in this daemon's admission order.
    pub(super) admitted: u64,
    /// Where the request stands in the order the worker handles them; absent
    /// for an accepted operation.
    pub(super) sent: Option<ExchangeOrder>,
}

impl PendingRelease {
    /// The identity of the claim the release ends.
    pub(super) fn key(&self) -> ClaimKey {
        ClaimKey {
            provider: self.provider.clone(),
            pid: self.pid,
            start_identity: self.start_identity,
            sequence: self.sequence,
        }
    }

    /// Where the release stands against the fence and the newest snapshot read.
    pub(super) fn state(
        &self,
        fence: Option<&IdentityFence>,
        observed: Option<&IdentityObservation>,
    ) -> PendingState {
        revision_state(
            (&self.revision, self.acknowledged, self.sent),
            fence,
            observed,
        )
    }
}

/// An operation whose answer timed out: the worker may hold it, at a revision
/// the daemon does not know.
#[derive(Debug, Clone)]
pub(super) enum Unacknowledged {
    Report(Box<PendingActivity>),
    Release(Box<PendingRelease>),
}

/// Keeps `operation`, whose answer the daemon never saw, next to the pending
/// operations the entry holds. An accepted operation keeps its slot, which
/// carries the activity it applies once adopted; an unacknowledged one takes
/// the slot of its kind otherwise, and the one it replaces stays tracked.
///
/// None is dropped to make room: each one needs its own forward to reach the
/// worker and then wait out the worker bound. A snapshot's revision settles
/// every operation it covers, and a snapshot read after the worker handled an
/// operation's request without raising the revision drops that operation.
pub(super) fn attach_unacknowledged(entry: &mut SessionEntry, operation: Unacknowledged) {
    let displaced = match operation {
        Unacknowledged::Report(pending) => match &entry.pending_activity {
            Some(held) if held.acknowledged => Some(Unacknowledged::Report(pending)),
            _ => entry
                .pending_activity
                .replace(pending)
                .map(Unacknowledged::Report),
        },
        Unacknowledged::Release(pending) => match &entry.pending_release {
            Some(held) if held.acknowledged => Some(Unacknowledged::Release(pending)),
            _ => entry
                .pending_release
                .replace(pending)
                .map(Unacknowledged::Release),
        },
    };
    if let Some(displaced) = displaced {
        entry.displaced_pending.push(displaced);
    }
}
