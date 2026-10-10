//! Reaps the processes one lost worker instance left behind.
//!
//! A session worker marks every child it launches with `POHUNEK_WORKER_INSTANCE_ID`.
//! When the worker dies, PTY descendants that ignore hangup can survive it.
//! [`sweep_runtime`] finds the same-user processes that carry exactly that
//! worker-instance marker and terminates them, first with `SIGTERM` and, after a grace
//! period, with `SIGKILL`. Every signal is preceded by a start-identity check,
//! so a reused PID is never signalled, and any inspection failure other than a
//! process disappearing aborts the sweep before further signals are sent.
//!
//! A process whose environment cannot be read carries no readable marker. When
//! the caller names the lost worker's [`SpawnId`], the sweep decides such a
//! process by its fork lineage instead: a process created, through any chain of
//! creators, by that worker is selected like a marked one, a process the worker
//! cannot have created is left out, and a process whose ancestry is not
//! established stays skipped and reported.

// Rust guideline compliant 2026-10-10

use std::cell::OnceCell;
use std::collections::HashMap;
use std::io;
use std::time::Duration;

use rustix::process::{Pid as NativePid, Signal};
use tokio::time::Instant;

use super::{
    Error, ProcessFact, ProcessIdentity, ProcessInspector, ProcessLineage, SpawnId, StartIdentity,
    WorkerInstanceMarker,
};

/// Largest accepted worker instance ID, in bytes.
///
/// Matches the worker-protocol identifier bound (`pohunek-worker-protocol`
/// `MAX_ID_BYTES`), which is where worker instance IDs are minted. It is below the
/// Darwin marker-value bound, so every valid worker instance ID can be observed.
pub const MAX_WORKER_INSTANCE_ID_BYTES: usize = 128;

/// Longest accepted grace period between `SIGTERM` and `SIGKILL`.
///
/// Matches the supervisor's upper bound for job exit timeouts; a longer wait
/// would stall reconciliation of the lost session for no realistic benefit.
pub const MAX_SWEEP_GRACE: Duration = Duration::from_mins(10);

/// Longest creator chain the lineage proof follows from one process.
///
/// Every step moves to a distinct live process of the same-user table, so no
/// real chain is longer than the table, which the Darwin inspector bounds at
/// 65,536 entries. A walk that reaches this bound proves nothing and leaves the
/// process skipped, so the bound only keeps a corrupt table from looping.
const MAX_LINEAGE_STEPS: usize = 65_536;

/// Validated parameters of one runtime sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepRequest {
    worker_instance_id: String,
    session_id: Option<String>,
    owner_uid: u32,
    grace: Duration,
    poll: Duration,
    /// Start identity of the worker process instance that owned the runtime,
    /// when the caller knows it; see [`Self::with_worker_start_identity`].
    worker_start_identity: Option<StartIdentity>,
    /// Spawn id of the worker process instance that owned the runtime, when the
    /// caller knows it; see [`Self::with_worker_spawn_id`].
    worker_spawn_id: Option<SpawnId>,
}

impl SweepRequest {
    /// Validates the parameters of a sweep of `worker_instance_id` owned by `owner_uid`.
    ///
    /// `grace` bounds both the wait between `SIGTERM` and `SIGKILL` and the
    /// wait for `SIGKILL` to take effect; `poll` is the liveness polling
    /// interval within those waits.
    ///
    /// # Errors
    ///
    /// Returns [`SweepError::InvalidWorkerInstanceId`] unless `worker_instance_id` is 1 to
    /// [`MAX_WORKER_INSTANCE_ID_BYTES`] bytes of `[A-Za-z0-9._-]` and not `.` or `..`,
    /// [`SweepError::InvalidGrace`] unless `grace` is positive and at most
    /// [`MAX_SWEEP_GRACE`], and [`SweepError::InvalidPoll`] unless `poll` is
    /// positive and at most `grace`.
    pub fn new(
        worker_instance_id: impl Into<String>,
        owner_uid: u32,
        grace: Duration,
        poll: Duration,
    ) -> Result<Self, SweepError> {
        let worker_instance_id = worker_instance_id.into();
        if !valid_worker_instance_id(&worker_instance_id) {
            return Err(SweepError::InvalidWorkerInstanceId);
        }
        if grace.is_zero() || grace > MAX_SWEEP_GRACE {
            return Err(SweepError::InvalidGrace);
        }
        if poll.is_zero() || poll > grace {
            return Err(SweepError::InvalidPoll);
        }
        Ok(Self {
            worker_instance_id,
            session_id: None,
            owner_uid,
            grace,
            poll,
            worker_start_identity: None,
            worker_spawn_id: None,
        })
    }

    /// Names the exact worker process instance that owned the runtime.
    ///
    /// The daemon takes this from the dead worker's journal. With the bound,
    /// a same-user process whose markers cannot be read is still provably not
    /// a descendant of the worker when its own start identity strictly
    /// precedes the worker's, on a host whose start identities order process
    /// starts. Without the bound, and without
    /// [`Self::with_worker_spawn_id`], every unreadable-marker process stays
    /// skipped and the sweep is reported unconfirmed.
    #[must_use]
    pub fn with_worker_start_identity(
        mut self,
        worker_start_identity: Option<StartIdentity>,
    ) -> Self {
        self.worker_start_identity = worker_start_identity;
        self
    }

    /// Names the creation number of the worker process instance that owned the
    /// runtime.
    ///
    /// The daemon takes this from the dead worker's journal. With it, a
    /// same-user process whose markers cannot be read is decided by its fork
    /// lineage on a host that reports lineage (see
    /// [`ProcessInspector::lineage`]): the sweep selects it when the worker
    /// created it, directly or through other processes, leaves it out when the
    /// worker cannot have created it, and skips it as unreadable when neither is
    /// established, for example when a creator in between has exited. A process
    /// proven a descendant has no readable session marker, so under
    /// [`Self::with_session_id`] it is skipped as session-unverified instead of
    /// selected.
    ///
    /// Creation numbers restart at every boot, so the caller must pass the
    /// number only when it was recorded in the current boot.
    #[must_use]
    pub fn with_worker_spawn_id(mut self, worker_spawn_id: Option<SpawnId>) -> Self {
        self.worker_spawn_id = worker_spawn_id;
        self
    }

    /// Requires a matching session marker before signalling a runtime process.
    ///
    /// A runtime marker with no session marker remains unconfirmed. This
    /// protects removal when a stored runtime ID lacks journal evidence.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Returns the exact worker instance ID whose processes are swept.
    #[must_use]
    pub fn worker_instance_id(&self) -> &str {
        &self.worker_instance_id
    }

    /// Returns the user that must own the swept processes.
    #[must_use]
    pub const fn owner_uid(&self) -> u32 {
        self.owner_uid
    }

    /// Returns the bound of each wait for processes to exit.
    #[must_use]
    pub const fn grace(&self) -> Duration {
        self.grace
    }

    /// Returns the liveness polling interval.
    #[must_use]
    pub const fn poll(&self) -> Duration {
        self.poll
    }

    /// Returns the worker start identity bound, when the caller knows it.
    #[must_use]
    pub const fn worker_start_identity(&self) -> Option<StartIdentity> {
        self.worker_start_identity
    }

    /// Returns the worker creation number, when the caller knows it.
    #[must_use]
    pub const fn worker_spawn_id(&self) -> Option<SpawnId> {
        self.worker_spawn_id
    }
}

/// Accepts the worker-protocol identifier alphabet and bound.
fn valid_worker_instance_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_WORKER_INSTANCE_ID_BYTES
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Why a sweep left a process alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkipReason {
    /// The process exited before it could be signalled.
    Vanished,
    /// The PID now names a different process than the one selected.
    IdentityChanged,
    /// The process is the one running the sweep.
    CurrentProcess,
    /// The process environment could not be read (access was denied, the
    /// process was between images, or the kernel withholds the environment of
    /// a restricted platform binary), and neither its start time nor its fork
    /// lineage proves whether it belongs to the runtime.
    MarkersUnreadable,
    /// `POHUNEK_WORKER_INSTANCE_ID` and `POHUNEK_RUNTIME_ID` carry different
    /// values, so the process cannot be proven to belong to the runtime or not.
    MarkersConflicting,
    /// The target runtime marker has no matching session marker.
    SessionUnverified,
    /// The sweep aborted before signalling the selected process.
    Aborted,
}

/// One process a sweep left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Skipped {
    /// Identity observed when the process table was enumerated.
    pub identity: ProcessIdentity,
    /// Why the process was not signalled.
    pub reason: SkipReason,
}

/// Outcome of one runtime sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Processes that exited after `SIGTERM`, within the grace period.
    pub terminated: Vec<ProcessIdentity>,
    /// Processes that exited after `SIGKILL`.
    pub killed: Vec<ProcessIdentity>,
    /// Signalled processes whose exit was not observed: still running when the
    /// `SIGKILL` wait ended, or pending when the sweep aborted.
    pub unconfirmed: Vec<ProcessIdentity>,
    /// Processes that were left alone, with the reason.
    pub skipped: Vec<Skipped>,
}

impl SweepReport {
    /// Returns whether every signalled process was observed to exit.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unconfirmed.is_empty()
    }
}

/// Typed runtime-sweep failure.
#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    /// The worker instance ID is empty, oversized, or outside the identifier alphabet.
    #[error(
        "sweep worker instance id must be 1 to {MAX_WORKER_INSTANCE_ID_BYTES} bytes of [A-Za-z0-9._-] and not `.` or `..`"
    )]
    InvalidWorkerInstanceId,
    /// The grace period is zero or longer than [`MAX_SWEEP_GRACE`].
    #[error("sweep grace must be positive and at most {} seconds", MAX_SWEEP_GRACE.as_secs())]
    InvalidGrace,
    /// The polling interval is zero or longer than the grace period.
    #[error("sweep poll interval must be positive and no longer than the grace")]
    InvalidPoll,
    /// The sweep was requested for a user other than the caller.
    #[error("sweep owner uid {owner_uid} is not the effective uid {effective_uid}")]
    ForeignOwner {
        /// Owner named by the request.
        owner_uid: u32,
        /// Effective user of the calling process.
        effective_uid: u32,
    },
    /// Process evidence could not be inspected; no further signal was sent.
    #[error("runtime sweep aborted: process inspection failed: {source}")]
    Inspection {
        /// Signals already sent and outcomes already observed.
        progress: Box<SweepReport>,
        /// Underlying inspection failure.
        #[source]
        source: Error,
    },
    /// A signal could not be delivered; no further signal was sent.
    #[error("runtime sweep aborted: signal delivery failed: {source}")]
    Signal {
        /// Signals already sent and outcomes already observed.
        progress: Box<SweepReport>,
        /// Underlying operating-system failure.
        #[source]
        source: io::Error,
    },
}

/// Terminates every same-user process marked with exactly one worker instance ID.
///
/// The sweep enumerates the caller's processes and selects those whose
/// allowlisted `POHUNEK_WORKER_INSTANCE_ID` marker equals the request's worker instance ID
/// byte for byte; unmarked processes and other worker instance IDs are never
/// selected. A process whose markers cannot be read is selected only through
/// its fork lineage, when the request names the worker's creation number and
/// the process is proven to descend from that worker. Selection finishes
/// before the first signal, so an inspection failure during selection signals
/// nothing. Each selected process then gets
/// `SIGTERM`; those still running after the grace period get `SIGKILL`; and
/// the sweep waits up to the grace period again for them to exit. A process's
/// start identity is rechecked immediately before every signal, and a process
/// that vanished or changed identity is skipped. The calling process is never
/// signalled.
///
/// On Linux, signals go through a pidfd opened before the identity recheck,
/// so they cannot reach a process that reused the PID afterwards.
///
/// # Errors
///
/// Returns [`SweepError::ForeignOwner`] when the request's owner is not the
/// effective user. Returns [`SweepError::Inspection`] or
/// [`SweepError::Signal`] when process evidence cannot be read or a signal
/// cannot be delivered for a reason other than the process disappearing; the
/// error carries the progress made so far, and no further signal is sent.
pub async fn sweep_runtime(
    inspector: &dyn ProcessInspector,
    request: &SweepRequest,
) -> Result<SweepReport, SweepError> {
    sweep_with(
        inspector,
        request,
        rustix::process::geteuid().as_raw(),
        std::process::id(),
        deliver,
    )
    .await
}

/// Result of one identity-checked signal attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// The signal reached the exact selected process.
    Sent,
    /// The process exited before the signal.
    Vanished,
    /// The PID named a different process at signal time.
    IdentityChanged,
}

/// Failure that aborts a sweep.
#[derive(Debug)]
enum Fault {
    Inspection(Error),
    Signal(io::Error),
}

impl From<Error> for Fault {
    fn from(source: Error) -> Self {
        Self::Inspection(source)
    }
}

impl Fault {
    fn into_error(self, progress: SweepReport) -> SweepError {
        let progress = Box::new(progress);
        match self {
            Self::Inspection(source) => SweepError::Inspection { progress, source },
            Self::Signal(source) => SweepError::Signal { progress, source },
        }
    }
}

/// Classification of one enumerated process.
enum Selection {
    /// The process carries exactly the requested runtime marker.
    Target,
    /// The process is not provably part of the runtime and is not reported.
    Foreign,
    /// The process is left alone and reported.
    Skip(SkipReason),
}

/// Runs a sweep with an injectable effective user, own PID, and signal sender.
async fn sweep_with<D>(
    inspector: &dyn ProcessInspector,
    request: &SweepRequest,
    effective_uid: u32,
    own_pid: u32,
    deliver: D,
) -> Result<SweepReport, SweepError>
where
    D: Fn(&dyn ProcessInspector, ProcessIdentity, Signal) -> Result<Delivery, Fault>,
{
    if request.owner_uid != effective_uid {
        return Err(SweepError::ForeignOwner {
            owner_uid: request.owner_uid,
            effective_uid,
        });
    }

    let mut report = SweepReport::default();
    let targets = match select_targets(inspector, request, own_pid, &mut report) {
        Ok(targets) => targets,
        Err(fault) => return Err(fault.into_error(report)),
    };

    let mut pending = Vec::with_capacity(targets.len());
    for (index, identity) in targets.iter().copied().enumerate() {
        match deliver(inspector, identity, Signal::TERM) {
            Ok(Delivery::Sent) => pending.push(identity),
            Ok(Delivery::Vanished) => skip(&mut report, identity, SkipReason::Vanished),
            Ok(Delivery::IdentityChanged) => {
                skip(&mut report, identity, SkipReason::IdentityChanged);
            }
            Err(fault) => {
                report.unconfirmed.extend(pending);
                // The failing target and every later one were never signalled.
                for identity in targets.into_iter().skip(index) {
                    skip(&mut report, identity, SkipReason::Aborted);
                }
                return Err(fault.into_error(report));
            }
        }
    }

    let still_running = match await_exits(inspector, request, pending).await {
        Ok((exited, running)) => {
            report.terminated.extend(exited);
            running
        }
        Err((fault, unobserved)) => {
            report.unconfirmed.extend(unobserved);
            return Err(fault.into_error(report));
        }
    };

    let mut killing = Vec::with_capacity(still_running.len());
    for (index, identity) in still_running.iter().copied().enumerate() {
        match deliver(inspector, identity, Signal::KILL) {
            Ok(Delivery::Sent) => killing.push(identity),
            // The process that received `SIGTERM` is gone either way.
            Ok(Delivery::Vanished | Delivery::IdentityChanged) => report.terminated.push(identity),
            Err(fault) => {
                report.unconfirmed.extend(killing);
                report
                    .unconfirmed
                    .extend(still_running.into_iter().skip(index));
                return Err(fault.into_error(report));
            }
        }
    }

    match await_exits(inspector, request, killing).await {
        Ok((exited, running)) => {
            report.killed.extend(exited);
            report.unconfirmed.extend(running);
            Ok(report)
        }
        Err((fault, unobserved)) => {
            report.unconfirmed.extend(unobserved);
            Err(fault.into_error(report))
        }
    }
}

fn skip(report: &mut SweepReport, identity: ProcessIdentity, reason: SkipReason) {
    report.skipped.push(Skipped { identity, reason });
}

/// Selects every process carrying the exact runtime marker, signalling nothing.
fn select_targets(
    inspector: &dyn ProcessInspector,
    request: &SweepRequest,
    own_pid: u32,
    report: &mut SweepReport,
) -> Result<Vec<ProcessIdentity>, Fault> {
    let mut targets = Vec::new();
    let facts = inspector.same_user_processes()?;
    let lineage = SpawnLineage::new(inspector, &facts);
    for fact in &facts {
        let identity = fact.identity();
        match classify(inspector, identity, request, &lineage)? {
            Selection::Target if identity.pid == own_pid => {
                skip(report, identity, SkipReason::CurrentProcess);
            }
            Selection::Target => targets.push(identity),
            Selection::Foreign => {}
            Selection::Skip(reason) => skip(report, identity, reason),
        }
    }
    Ok(targets)
}

/// Decides whether one enumerated process belongs to the runtime.
///
/// The marker read is bracketed by the enumerated identity and a fresh
/// identity check, so the markers are known to describe the enumerated
/// process rather than a process that reused its PID.
fn classify(
    inspector: &dyn ProcessInspector,
    identity: ProcessIdentity,
    request: &SweepRequest,
    lineage: &SpawnLineage<'_>,
) -> Result<Selection, Error> {
    let markers = match inspector.ownership_markers(identity.pid) {
        Ok(markers) => markers,
        // A process that exited has nothing left to reap.
        Err(error) if error.is_race() => return Ok(Selection::Foreign),
        // Same-user processes can still hide their environment (for example
        // non-dumpable agents) or be between images; without the marker they
        // are never signalled, and the rest of the table is still classified.
        Err(Error::PermissionDenied { .. } | Error::Unobservable { .. }) => {
            return classify_unreadable_markers(inspector, identity, request, lineage);
        }
        Err(error) => return Err(error),
    };
    match markers.worker_instance() {
        WorkerInstanceMarker::Instance(id) if id == request.worker_instance_id() => {}
        // A process claiming two instances, one of them the requested one, is
        // attributed to neither, so it is never signalled, and the skip keeps
        // the caller's cleanup unconfirmed. A conflicting pair that names
        // other instances only is foreign to this sweep.
        WorkerInstanceMarker::Conflicting
            if [&markers.worker_instance_id, &markers.runtime_id]
                .into_iter()
                .any(|marker| marker.as_deref() == Some(request.worker_instance_id())) =>
        {
            return Ok(Selection::Skip(SkipReason::MarkersConflicting));
        }
        WorkerInstanceMarker::Conflicting
        | WorkerInstanceMarker::Absent
        | WorkerInstanceMarker::Instance(_) => {
            return Ok(Selection::Foreign);
        }
    }
    if let Some(session_id) = request.session_id.as_deref() {
        match markers.session_id.as_deref() {
            Some(marked) if marked == session_id => {}
            Some(_) => return Ok(Selection::Foreign),
            None => return Ok(Selection::Skip(SkipReason::SessionUnverified)),
        }
    }
    Ok(match verify(inspector, identity)? {
        None => Selection::Target,
        Some(Delivery::IdentityChanged) => Selection::Skip(SkipReason::IdentityChanged),
        Some(Delivery::Vanished | Delivery::Sent) => Selection::Skip(SkipReason::Vanished),
    })
}

/// Whether comparing two [`StartIdentity`] values orders their processes' starts.
///
/// Linux start identities are `/proc/<pid>/stat` start times in clock ticks
/// since boot, a monotonic clock. Darwin encodes the wall-clock start time,
/// which can step backwards, so a descendant could compare older than its
/// worker; there the ordering proves nothing.
pub(super) const START_IDENTITY_ORDERS_PROCESS_STARTS: bool = cfg!(target_os = "linux");

/// Classifies a process whose ownership markers could not be read.
///
/// A process inherits its environment only when it is forked or execs, and
/// only the worker and its descendants ever carry this runtime's marker, so
/// a process carrying the marker must have started at or after the worker
/// that owns the runtime, and must descend from it. Two bounds, each named by
/// the request, decide such a process without its markers.
///
/// The worker's start identity bounds the start time: a process whose start
/// identity strictly precedes it cannot be a descendant. It identifies the exact
/// worker process instance, so a PID that was reused after the worker compares
/// by its own, later start time and is never dismissed by the bound. It is used
/// only where start identities are ordered by a monotonic clock (see
/// [`START_IDENTITY_ORDERS_PROCESS_STARTS`]).
///
/// The worker's creation number bounds the fork lineage (see
/// [`SpawnLineage::ancestry`]): a process the worker created, through any chain
/// of creators, is selected like a marked one, after the same final identity
/// check, and a process the worker cannot have created is foreign.
///
/// Without a bound, when the process exited between the two reads, or when its
/// start identity cannot be read, the process is not provably foreign: it stays
/// skipped unreadable, which makes the caller's cleanup unconfirmed. The same
/// holds for a process whose ancestry the lineage does not establish.
fn classify_unreadable_markers(
    inspector: &dyn ProcessInspector,
    identity: ProcessIdentity,
    request: &SweepRequest,
    lineage: &SpawnLineage<'_>,
) -> Result<Selection, Error> {
    // An exited process can keep its identity while its parent has not reaped
    // it. Dismiss it only when a fresh read confirms the enumerated identity
    // and that exact process is no longer running. Missing or inconsistent
    // identity evidence still fails closed below.
    if matches!(inspector.identity(identity.pid), Ok(Some(current)) if current == identity)
        && matches!(inspector.is_running(identity), Ok(false))
    {
        return Ok(Selection::Foreign);
    }
    let worker_start = request.worker_start_identity();
    let worker_spawn = request.worker_spawn_id();
    if worker_start.is_none() && worker_spawn.is_none() {
        return Ok(Selection::Skip(SkipReason::MarkersUnreadable));
    }
    match inspector.identity(identity.pid) {
        // The process exited between the marker read and this read, so it
        // cannot survive the sweep.
        Ok(None) => return Ok(Selection::Foreign),
        Ok(Some(current))
            if START_IDENTITY_ORDERS_PROCESS_STARTS
                && worker_start.is_some_and(|start| current.start_identity < start) =>
        {
            return Ok(Selection::Foreign);
        }
        Ok(Some(_)) => {}
        // A process whose start read failed proves nothing: it stays skipped
        // unreadable.
        Err(_) => return Ok(Selection::Skip(SkipReason::MarkersUnreadable)),
    }
    let Some(worker) = worker_spawn else {
        return Ok(Selection::Skip(SkipReason::MarkersUnreadable));
    };
    Ok(match lineage.ancestry(identity, worker)? {
        // A session marker cannot be read from this process, so a request
        // that requires one never selects it through the lineage.
        Ancestry::Descendant if request.session_id.is_some() => {
            Selection::Skip(SkipReason::SessionUnverified)
        }
        Ancestry::Descendant => match verify(inspector, identity)? {
            None => Selection::Target,
            Some(Delivery::IdentityChanged) => Selection::Skip(SkipReason::IdentityChanged),
            Some(Delivery::Vanished | Delivery::Sent) => Selection::Skip(SkipReason::Vanished),
        },
        Ancestry::Unrelated => Selection::Foreign,
        Ancestry::Unproven => Selection::Skip(SkipReason::MarkersUnreadable),
    })
}

/// What the fork lineage proves about a process relative to one worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ancestry {
    /// The worker created the process, directly or through other processes.
    Descendant,
    /// The worker cannot have created the process.
    Unrelated,
    /// The lineage does not establish either.
    Unproven,
}

/// Fork lineage of the processes one selection enumerated.
///
/// The lineage of every enumerated process is read once, when the first
/// candidate needs it, so the cost of a selection is linear in the size of the
/// table however many candidates ask. Each read is bracketed by the enumerated
/// identity, so an entry describes the enumerated process and never a process
/// that reused its PID.
struct SpawnLineage<'a> {
    inspector: &'a dyn ProcessInspector,
    enumerated: &'a [ProcessFact],
    snapshot: OnceCell<Snapshot>,
}

/// Lineage of the live same-user processes at one moment.
#[derive(Debug, Default)]
struct Snapshot {
    /// Lineage of each enumerated process whose read was consistent.
    by_identity: HashMap<ProcessIdentity, ProcessLineage>,
    /// Creator of each created process, by creation number.
    creator_of: HashMap<SpawnId, SpawnId>,
}

impl<'a> SpawnLineage<'a> {
    fn new(inspector: &'a dyn ProcessInspector, enumerated: &'a [ProcessFact]) -> Self {
        Self {
            inspector,
            enumerated,
            snapshot: OnceCell::new(),
        }
    }

    /// Decides whether `worker` created the enumerated process `identity`.
    ///
    /// Creation numbers rise from creator to created, so the creators of a
    /// process, followed upward from its recorded creator, have strictly
    /// decreasing numbers. The chain reaches `worker` exactly when the worker is
    /// an ancestor. It cannot once a creator is older than the worker, because
    /// every creator above it is older still. A creator that has exited
    /// and is newer than the worker names a creator the table cannot follow
    /// further, so the chain, and with it the ancestry, stays unproven, and so
    /// does a process whose lineage the host does not report.
    fn ancestry(&self, identity: ProcessIdentity, worker: SpawnId) -> Result<Ancestry, Error> {
        let snapshot = self.snapshot()?;
        let Some(process) = snapshot.by_identity.get(&identity) else {
            return Ok(Ancestry::Unproven);
        };
        // The worker's own number is not a descendant's, and a process
        // created before the worker cannot descend from it.
        if process.id == worker {
            return Ok(Ancestry::Unproven);
        }
        if process.id < worker {
            return Ok(Ancestry::Unrelated);
        }
        let mut creator = process.parent;
        for _ in 0..MAX_LINEAGE_STEPS {
            if creator == worker {
                return Ok(Ancestry::Descendant);
            }
            if creator < worker {
                return Ok(Ancestry::Unrelated);
            }
            match snapshot.creator_of.get(&creator) {
                // Numbers must fall at every step; a creator that is not
                // older than the process it created contradicts the kernel.
                Some(next) if *next < creator => creator = *next,
                Some(_) | None => return Ok(Ancestry::Unproven),
            }
        }
        Ok(Ancestry::Unproven)
    }

    fn snapshot(&self) -> Result<&Snapshot, Error> {
        if let Some(snapshot) = self.snapshot.get() {
            return Ok(snapshot);
        }
        let read = self.read()?;
        Ok(self.snapshot.get_or_init(|| read))
    }

    /// Reads the lineage of every enumerated process once.
    ///
    /// A host that reports no lineage yields an empty snapshot, which proves
    /// nothing. A process that exited, changed identity, or cannot be read
    /// leaves no entry, so the chains through it stay unproven.
    fn read(&self) -> Result<Snapshot, Error> {
        let mut snapshot = Snapshot::default();
        for fact in self.enumerated {
            let identity = fact.identity();
            if verify(self.inspector, identity)?.is_some() {
                continue;
            }
            let lineage = match self.inspector.lineage(identity.pid) {
                Ok(Some(lineage)) => lineage,
                Ok(None) | Err(Error::PermissionDenied { .. } | Error::Unobservable { .. }) => {
                    continue
                }
                Err(error) if error.is_race() => continue,
                Err(Error::Unavailable { .. }) => return Ok(Snapshot::default()),
                Err(error) => return Err(error),
            };
            if verify(self.inspector, identity)?.is_some() {
                continue;
            }
            snapshot.by_identity.insert(identity, lineage);
            snapshot.creator_of.insert(lineage.id, lineage.parent);
        }
        Ok(snapshot)
    }
}

/// Rechecks that `identity` still names a live process record.
///
/// Returns `None` when the identity is unchanged, or the delivery outcome that
/// replaces the signal otherwise.
fn verify(
    inspector: &dyn ProcessInspector,
    identity: ProcessIdentity,
) -> Result<Option<Delivery>, Error> {
    match inspector.identity(identity.pid) {
        Ok(Some(current)) if current == identity => Ok(None),
        Ok(Some(_)) => Ok(Some(Delivery::IdentityChanged)),
        Ok(None) => Ok(Some(Delivery::Vanished)),
        Err(error) if error.is_race() => Ok(Some(Delivery::Vanished)),
        Err(error) => Err(error),
    }
}

/// Waits up to the grace period for signalled processes to stop running.
///
/// Returns the processes that exited and those still running at the
/// deadline. On failure, returns the fault with every process whose exit was
/// not observed.
async fn await_exits(
    inspector: &dyn ProcessInspector,
    request: &SweepRequest,
    mut running: Vec<ProcessIdentity>,
) -> Result<(Vec<ProcessIdentity>, Vec<ProcessIdentity>), (Fault, Vec<ProcessIdentity>)> {
    let deadline = Instant::now() + request.grace;
    let mut exited = Vec::with_capacity(running.len());
    loop {
        let mut index = 0;
        while index < running.len() {
            match inspector.is_running(running[index]) {
                Ok(true) => index += 1,
                Ok(false) => exited.push(running.swap_remove(index)),
                Err(error) if error.is_race() => exited.push(running.swap_remove(index)),
                Err(error) => return Err((Fault::Inspection(error), running)),
            }
        }
        let now = Instant::now();
        if running.is_empty() || now >= deadline {
            return Ok((exited, running));
        }
        tokio::time::sleep(request.poll.min(deadline - now)).await;
    }
}

/// Sends `signal` to `identity` only if the PID still names that process.
#[cfg(target_os = "linux")]
fn deliver(
    inspector: &dyn ProcessInspector,
    identity: ProcessIdentity,
    signal: Signal,
) -> Result<Delivery, Fault> {
    use rustix::io::Errno;
    use rustix::process::{pidfd_open, pidfd_send_signal, PidfdFlags};

    // The pidfd pins the process opened here; verifying the identity after
    // opening it proves the pidfd refers to the selected process.
    let pidfd = match pidfd_open(native_pid(identity)?, PidfdFlags::empty()) {
        Ok(pidfd) => pidfd,
        Err(Errno::SRCH) => return Ok(Delivery::Vanished),
        Err(errno) => return Err(Fault::Signal(errno.into())),
    };
    if let Some(outcome) = verify(inspector, identity)? {
        return Ok(outcome);
    }
    match pidfd_send_signal(&pidfd, signal) {
        Ok(()) => Ok(Delivery::Sent),
        Err(Errno::SRCH) => Ok(Delivery::Vanished),
        Err(errno) => Err(Fault::Signal(errno.into())),
    }
}

/// Sends `signal` to `identity` only if the PID still names that process.
///
/// Without pidfds, a PID reused between the identity check and `kill` is an
/// unavoidable, microsecond-wide window; the start-identity check keeps it
/// from covering any process that existed before the check.
#[cfg(all(unix, not(target_os = "linux")))]
fn deliver(
    inspector: &dyn ProcessInspector,
    identity: ProcessIdentity,
    signal: Signal,
) -> Result<Delivery, Fault> {
    use rustix::io::Errno;

    let pid = native_pid(identity)?;
    if let Some(outcome) = verify(inspector, identity)? {
        return Ok(outcome);
    }
    match rustix::process::kill_process(pid, signal) {
        Ok(()) => Ok(Delivery::Sent),
        Err(Errno::SRCH) => Ok(Delivery::Vanished),
        Err(errno) => Err(Fault::Signal(errno.into())),
    }
}

/// Converts a selected PID to a positive native PID.
///
/// A non-positive PID would address a process group or every process, so it
/// is rejected rather than signalled.
fn native_pid(identity: ProcessIdentity) -> Result<NativePid, Fault> {
    i32::try_from(identity.pid)
        .ok()
        .and_then(NativePid::from_raw)
        .ok_or(Fault::Inspection(Error::OutOfRange {
            operation: "signal_runtime_process",
        }))
}

#[cfg(test)]
mod tests;
