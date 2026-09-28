//! Reaps the processes one lost worker runtime generation left behind.
//!
//! A session worker marks every child it launches with `POHUNEK_RUNTIME_ID`.
//! When the worker dies, PTY descendants that ignore hangup can survive it.
//! [`sweep_runtime`] finds the same-user processes that carry exactly that
//! runtime marker and terminates them, first with `SIGTERM` and, after a grace
//! period, with `SIGKILL`. Every signal is preceded by a start-identity check,
//! so a reused PID is never signalled, and any inspection failure other than a
//! process disappearing aborts the sweep before further signals are sent.

// Rust guideline compliant 2026-09-24

use std::io;
use std::time::Duration;

use rustix::process::{Pid as NativePid, Signal};
use tokio::time::Instant;

use super::{Error, ProcessIdentity, ProcessInspector, StartIdentity};

/// Largest accepted runtime ID, in bytes.
///
/// Matches the worker-protocol identifier bound (`pohunek-worker-protocol`
/// `MAX_ID_BYTES`), which is where runtime IDs are minted. It is below the
/// Darwin marker-value bound, so every valid runtime ID can be observed.
pub const MAX_RUNTIME_ID_BYTES: usize = 128;

/// Longest accepted grace period between `SIGTERM` and `SIGKILL`.
///
/// Matches the supervisor's upper bound for job exit timeouts; a longer wait
/// would stall reconciliation of the lost session for no realistic benefit.
pub const MAX_SWEEP_GRACE: Duration = Duration::from_mins(10);

/// Validated parameters of one runtime sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepRequest {
    runtime_id: String,
    owner_uid: u32,
    grace: Duration,
    poll: Duration,
    /// Start identity of the worker process instance that owned the runtime,
    /// when the caller knows it; see [`Self::with_worker_start_identity`].
    worker_start_identity: Option<StartIdentity>,
}

impl SweepRequest {
    /// Validates the parameters of a sweep of `runtime_id` owned by `owner_uid`.
    ///
    /// `grace` bounds both the wait between `SIGTERM` and `SIGKILL` and the
    /// wait for `SIGKILL` to take effect; `poll` is the liveness polling
    /// interval within those waits.
    ///
    /// # Errors
    ///
    /// Returns [`SweepError::InvalidRuntimeId`] unless `runtime_id` is 1 to
    /// [`MAX_RUNTIME_ID_BYTES`] bytes of `[A-Za-z0-9._-]` and not `.` or `..`,
    /// [`SweepError::InvalidGrace`] unless `grace` is positive and at most
    /// [`MAX_SWEEP_GRACE`], and [`SweepError::InvalidPoll`] unless `poll` is
    /// positive and at most `grace`.
    pub fn new(
        runtime_id: impl Into<String>,
        owner_uid: u32,
        grace: Duration,
        poll: Duration,
    ) -> Result<Self, SweepError> {
        let runtime_id = runtime_id.into();
        if !valid_runtime_id(&runtime_id) {
            return Err(SweepError::InvalidRuntimeId);
        }
        if grace.is_zero() || grace > MAX_SWEEP_GRACE {
            return Err(SweepError::InvalidGrace);
        }
        if poll.is_zero() || poll > grace {
            return Err(SweepError::InvalidPoll);
        }
        Ok(Self {
            runtime_id,
            owner_uid,
            grace,
            poll,
            worker_start_identity: None,
        })
    }

    /// Names the exact worker process instance that owned the runtime.
    ///
    /// The daemon takes this from the dead worker's journal. With the bound,
    /// a same-user process whose markers cannot be read is still provably not
    /// a descendant of the worker when its own start identity strictly
    /// precedes the worker's. Without the bound, every unreadable-marker
    /// process stays skipped and the sweep is reported unconfirmed.
    #[must_use]
    pub fn with_worker_start_identity(
        mut self,
        worker_start_identity: Option<StartIdentity>,
    ) -> Self {
        self.worker_start_identity = worker_start_identity;
        self
    }

    /// Returns the exact runtime ID whose processes are swept.
    #[must_use]
    pub fn runtime_id(&self) -> &str {
        &self.runtime_id
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
}

/// Accepts the worker-protocol identifier alphabet and bound.
fn valid_runtime_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RUNTIME_ID_BYTES
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
    /// The process environment could not be read (access was denied, or the
    /// process was between images), so it cannot be proven to belong to the
    /// runtime.
    MarkersUnreadable,
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
    /// The runtime ID is empty, oversized, or outside the identifier alphabet.
    #[error(
        "sweep runtime id must be 1 to {MAX_RUNTIME_ID_BYTES} bytes of [A-Za-z0-9._-] and not `.` or `..`"
    )]
    InvalidRuntimeId,
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

/// Terminates every same-user process marked with exactly one runtime ID.
///
/// The sweep enumerates the caller's processes and selects those whose
/// allowlisted `POHUNEK_RUNTIME_ID` marker equals the request's runtime ID
/// byte for byte; unmarked processes and other runtime IDs are never
/// selected. Selection finishes before the first signal, so an inspection
/// failure during selection signals nothing. Each selected process then gets
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
    for fact in inspector.same_user_processes()? {
        let identity = fact.identity();
        match classify(inspector, identity, request)? {
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
) -> Result<Selection, Error> {
    let markers = match inspector.ownership_markers(identity.pid) {
        Ok(markers) => markers,
        // A process that exited has nothing left to reap.
        Err(error) if error.is_race() => return Ok(Selection::Foreign),
        // Same-user processes can still hide their environment (for example
        // non-dumpable agents) or be between images; without the marker they
        // are never signalled, and the rest of the table is still classified.
        Err(Error::PermissionDenied { .. } | Error::Unobservable { .. }) => {
            return Ok(classify_unreadable_markers(inspector, identity, request));
        }
        Err(error) => return Err(error),
    };
    if markers.runtime_id.as_deref() != Some(request.runtime_id()) {
        return Ok(Selection::Foreign);
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
/// that owns the runtime. A process whose start identity strictly precedes
/// the journaled worker's start identity therefore cannot be a descendant of
/// that worker, however unreadable its current markers are. The bound is the
/// worker's start identity, which identifies the exact process instance, so
/// a PID that was reused after the worker compares by its own, later start
/// time and is never dismissed by the bound. Without a bound, when the
/// process exited between the two reads, or when its start identity cannot
/// be read either, the process is not provably foreign: it stays skipped
/// unreadable, which makes the caller's cleanup unconfirmed. The same holds
/// on a platform whose start identities are not ordered by a monotonic clock
/// (see [`START_IDENTITY_ORDERS_PROCESS_STARTS`]).
fn classify_unreadable_markers(
    inspector: &dyn ProcessInspector,
    identity: ProcessIdentity,
    request: &SweepRequest,
) -> Selection {
    let Some(worker_start) = request.worker_start_identity() else {
        return Selection::Skip(SkipReason::MarkersUnreadable);
    };
    match inspector.identity(identity.pid) {
        // The process exited between the marker read and this read, so it
        // cannot survive the sweep.
        Ok(None) => Selection::Foreign,
        Ok(Some(current))
            if START_IDENTITY_ORDERS_PROCESS_STARTS && current.start_identity < worker_start =>
        {
            Selection::Foreign
        }
        // A process started at or after the worker, or one whose start read
        // failed, proves nothing: it stays skipped unreadable.
        _ => Selection::Skip(SkipReason::MarkersUnreadable),
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
