//! Bounded child-process primitives shared by the CLI's fixed runners.
//!
//! A caller builds its own [`Command`] (program, argv, environment, working
//! directory, piped stdout and stderr) and owns every policy value: the
//! deadline, the per-stream byte limit, and how each [`Error`] maps to its own
//! error contract. These primitives then enforce one lifecycle contract:
//!
//! - **Process group.** [`configure_child`] makes the child the leader of a new
//!   process group, so cleanup reaches descendants that stay in that group. A
//!   descendant that calls `setsid` leaves the group and escapes the cleanup.
//! - **Inherited descriptors closed.** In the child, after stdio setup and
//!   before `exec`, every descriptor from 3 upward is closed, whatever its
//!   close-on-exec flag. [`descriptor_scan_bound`] captures the bound for the
//!   fallback `close` loop before fork.
//! - **Deadline kill.** [`wait_for_child`] polls until the deadline, then kills
//!   the group with `SIGKILL` and reaps the direct child. After a normal exit
//!   it also kills the group, so descendants left in it do not outlive the
//!   run. [`supervise_child`] is the same wait with a grace period: at the
//!   deadline, or when the caller's stop check fires, the group gets
//!   `SIGTERM`, then `SIGKILL` once the direct child exited or the grace
//!   elapsed, whichever comes first.
//! - **Bounded output.** Each pipe is drained on its own thread to prevent a
//!   pipe deadlock, but at most the caller's limit is retained; the rest is
//!   discarded and reported as [`BoundedOutput::overflow`]. Collection shares
//!   the child's deadline. [`spawn_capped_reader`] instead forwards a stream
//!   to any [`Write`] sink (a terminal, a capture buffer, or a discarding
//!   counter) and stops reading at the limit, so the caller's stop check can
//!   end the run early; [`drain_capped`] collects it without counting time
//!   the sink's consumer takes.

// Rust guideline compliant 2026-10-13

use std::io::{self, Read, Write};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Polling keeps timeout latency bounded without busy-spinning.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// File descriptors zero through two are the only handles allowed across exec.
const FIRST_INHERITED_FD: libc::c_int = 3;
/// Largest exclusive descriptor bound the fallback `close` loop scans.
///
/// The soft `RLIMIT_NOFILE` can be `RLIM_INFINITY` or far above any real
/// descriptor table. Darwin caps a process at `kern.maxfilesperproc` (well
/// below this value) and Linux at `fs.nr_open` (default 1 048 576), so no
/// descriptor number reaches it. A larger value only slows the child's scan; a
/// smaller one could leave a high inherited descriptor open.
const MAX_DESCRIPTOR_SCAN: libc::c_int = 1 << 20;
/// Highest-descriptor value of an open-descriptor listing that found none.
const NO_OPEN_DESCRIPTOR: libc::c_int = -1;
/// Slack entries requested beyond the size probe when listing Darwin descriptors.
///
/// Another thread may open descriptors between the size probe and the listing;
/// the listing is retried with a doubled buffer whenever it fills completely,
/// so this only avoids the common retry.
#[cfg(target_os = "macos")]
const DESCRIPTOR_LIST_SLACK: usize = 16;
/// Bytes requested from a pipe per `read` call.
///
/// The size of the standard library's default I/O buffer. It lives on the
/// reader thread's stack and only sets how many reads drain a stream; the
/// retained byte limit is independent of it.
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// A failure of the bounded child lifecycle.
///
/// The variants carry no child output. Each caller maps them onto its own
/// redacted error contract.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// The deadline expired before the child exited or its output was collected.
    #[error("child process deadline expired")]
    Timeout,
    /// Observing or reaping the child failed.
    #[error("waiting for the child process failed")]
    Wait(#[source] io::Error),
    /// Reading a child output pipe failed.
    #[error("reading child process output failed")]
    Read(#[source] io::Error),
    /// A reader thread ended without delivering its result.
    #[error("child process output reader ended without a result")]
    ReaderLost,
}

/// Why [`supervise_child`] stopped the child's group before the child exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cut {
    /// The deadline expired first.
    Deadline,
    /// The caller's stop check fired first.
    Stopped,
}

/// How a supervised child ended.
#[derive(Debug)]
pub(crate) struct Ended {
    /// The reaped direct child's status.
    pub(crate) status: ExitStatus,
    /// Why the group was stopped, or `None` when the child exited by itself.
    pub(crate) cut: Option<Cut>,
}

/// What one capped stream delivered, readable while its reader still runs.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    delivered: AtomicU64,
    overflowed: AtomicBool,
    /// Whether the reader is inside a sink write, waiting on the consumer.
    writing: AtomicBool,
}

impl Tally {
    /// Bytes handed to the sink so far; never more than the limit.
    pub(crate) fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Acquire)
    }

    /// Whether the stream produced more bytes than the limit.
    pub(crate) fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }

    /// Whether the reader is writing to its sink right now.
    fn writing(&self) -> bool {
        self.writing.load(Ordering::Acquire)
    }
}

/// One drained output stream, truncated to the caller's byte limit.
pub(crate) struct BoundedOutput {
    /// The first bytes of the stream, at most the limit.
    pub(crate) bytes: Vec<u8>,
    /// Whether the stream produced more bytes than the limit.
    pub(crate) overflow: bool,
}

/// Captures the soft descriptor limit before fork, outside the constrained child.
#[expect(
    unsafe_code,
    reason = "getrlimit is the libc boundary for the process open-file limit"
)]
fn soft_descriptor_limit() -> Option<libc::rlim_t> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is valid, writable storage for exactly one `rlimit`.
    let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) };
    (result == 0).then_some(limit.rlim_cur)
}

/// Converts a `proc_pidinfo` result into a byte count, or `None` on failure.
///
/// `proc_pidinfo` reports failure as zero (or a negative value) with `errno`
/// set. The Rust runtime keeps descriptors 0 through 2 open, so a zero-byte
/// listing is never a complete table; `None` makes the caller scan the whole
/// bounded range.
#[cfg(target_os = "macos")]
fn listed_bytes(result: libc::c_int) -> Option<usize> {
    usize::try_from(result).ok().filter(|bytes| *bytes > 0)
}

/// Returns the highest open descriptor number of this process on Darwin.
///
/// Darwin has no `close_range`, so the child closes descriptors one by one up
/// to a bound. The listing covers descriptors above a lowered soft limit.
/// `None` means the table could not be listed completely; an empty table is
/// `Some(NO_OPEN_DESCRIPTOR)`.
#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "proc_pidinfo(PROC_PIDLISTFDS) is the libc boundary for the descriptor table"
)]
fn highest_open_descriptor() -> Option<libc::c_int> {
    let pid = libc::c_int::try_from(std::process::id()).ok()?;
    let entry_size = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer with zero size only asks for the required byte count.
    let required =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let mut capacity = listed_bytes(required)? / entry_size + DESCRIPTOR_LIST_SLACK;
    loop {
        let mut entries = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            };
            capacity
        ];
        let buffer_bytes = libc::c_int::try_from(capacity.checked_mul(entry_size)?).ok()?;
        // SAFETY: `entries` provides `buffer_bytes` writable bytes of properly
        // aligned `proc_fdinfo` storage that outlives the call.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                entries.as_mut_ptr().cast(),
                buffer_bytes,
            )
        };
        let written = listed_bytes(written)?;
        if written < capacity * entry_size {
            return Some(
                entries[..written / entry_size]
                    .iter()
                    .map(|entry| entry.proc_fd)
                    .max()
                    .unwrap_or(NO_OPEN_DESCRIPTOR),
            );
        }
        // A completely filled buffer may be truncated; retry with more room.
        capacity = capacity.checked_mul(2)?;
    }
}

/// Returns the highest open descriptor number of this process on Linux.
///
/// The `close` loop is only the fallback for kernels without `close_range`;
/// listing `/proc/self/fd` keeps it correct below a lowered soft limit too.
/// The directory's own descriptor is included, which only raises the bound.
/// `None` means the directory could not be read completely (for example
/// `/proc` is not mounted).
#[cfg(target_os = "linux")]
fn highest_open_descriptor() -> Option<libc::c_int> {
    let mut highest = NO_OPEN_DESCRIPTOR;
    for entry in std::fs::read_dir("/proc/self/fd").ok()? {
        let descriptor: libc::c_int = entry.ok()?.file_name().to_str()?.parse().ok()?;
        highest = highest.max(descriptor);
    }
    Some(highest)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn highest_open_descriptor() -> Option<libc::c_int> {
    None
}

/// Computes the exclusive descriptor bound for the child's `close` loop.
///
/// An unlimited, unreadable, or out-of-range soft limit becomes
/// [`MAX_DESCRIPTOR_SCAN`]. A successful listing can only raise the bound: a
/// process may lower its soft limit below descriptors it already holds, and
/// those must still be closed. When the listing failed, the soft limit is not
/// trustworthy for the same reason, so the whole [`MAX_DESCRIPTOR_SCAN`] range
/// is scanned; that is about one million `close` calls in the child before
/// `exec`, counted against the caller's deadline, and only happens when no
/// listing is available (Darwin, or Linux without `close_range` and `/proc`).
fn descriptor_scan_end(
    soft_limit: Option<libc::rlim_t>,
    listing: Option<libc::c_int>,
) -> libc::c_int {
    let limit = soft_limit
        .and_then(|limit| libc::c_int::try_from(limit).ok())
        .map_or(MAX_DESCRIPTOR_SCAN, |limit| limit.min(MAX_DESCRIPTOR_SCAN));
    listing.map_or(MAX_DESCRIPTOR_SCAN, |highest| {
        limit.max(highest.saturating_add(1))
    })
}

/// Captures the descriptor bound before fork, outside the constrained child.
///
/// Pass the result to [`configure_child`] immediately before spawning, so the
/// bound covers every descriptor this process holds at that point.
pub(crate) fn descriptor_scan_bound() -> libc::c_int {
    descriptor_scan_end(soft_descriptor_limit(), highest_open_descriptor())
}

/// Makes `command` spawn as a group leader with only stdio inherited.
///
/// The child gets its own process group (`pgid` equals its PID) and, after
/// stdio setup and before `exec`, closes every descriptor from 3 upward; the
/// fallback `close` loop stops at `scan_end` from [`descriptor_scan_bound`].
/// Closing the standard library's close-on-exec error pipe means an `exec`
/// failure surfaces as a non-zero child exit, not as a spawn error.
pub(crate) fn configure_child(command: &mut Command, scan_end: libc::c_int) {
    use std::os::unix::process::CommandExt as _;

    // `pgid := child pid`, so timeout cleanup can terminate descendants too.
    command.process_group(0);
    // `pre_exec` runs after stdio has been duplicated to descriptors 0, 1,
    // and 2. Only async-signal-safe raw syscalls execute in the child.
    #[expect(
        unsafe_code,
        reason = "pre_exec is required to close inherited descriptors after stdio setup"
    )]
    // SAFETY: the closure calls only raw async-signal-safe descriptor syscalls.
    unsafe {
        command.pre_exec(move || {
            // This also closes std::process's CLOEXEC exec-error pipe, so exec
            // failures become non-zero child exits rather than spawn errors.
            close_inherited_fds(scan_end);
            Ok(())
        });
    }
}

/// Closes every non-stdio descriptor using only async-signal-safe syscalls.
///
/// Current Linux kernels support one `close_range` call. The bounded `close`
/// loop is the path on Darwin and the compatibility fallback for older Linux
/// kernels or restricted syscall policies; its upper bound was captured before
/// fork by [`descriptor_scan_bound`].
#[cfg_attr(
    target_os = "linux",
    expect(
        unsafe_code,
        reason = "the raw close_range syscall is required inside pre_exec"
    )
)]
fn close_inherited_fds(scan_end: libc::c_int) {
    #[cfg(target_os = "linux")]
    {
        let first = FIRST_INHERITED_FD as libc::c_uint;
        // SAFETY: close_range receives scalar descriptor bounds and no pointers.
        let result =
            unsafe { libc::syscall(libc::SYS_close_range, first, libc::c_uint::MAX, 0_u32) };
        if result == 0 {
            return;
        }
    }
    close_descriptor_range(FIRST_INHERITED_FD, scan_end);
}

/// Closes descriptors `first..end` one by one; both bounds are scalars.
#[expect(unsafe_code, reason = "raw close calls are required inside pre_exec")]
fn close_descriptor_range(first: libc::c_int, end: libc::c_int) {
    for descriptor in first..end {
        // SAFETY: close accepts any integer descriptor. EBADF and EINTR are
        // harmless here because the child is about to exec or abort.
        unsafe {
            libc::close(descriptor);
        }
    }
}

/// Waits for the direct child until `deadline`, then cleans up its group.
///
/// After the child exits, the rest of its process group is killed and the
/// child is reaped. At the deadline, or when the child cannot be observed,
/// the whole group is killed with `SIGKILL` and the child is reaped before
/// returning.
///
/// # Errors
///
/// Returns [`Error::Timeout`] when the deadline expires first, and
/// [`Error::Wait`] when observing or reaping the child fails, including a
/// failed reap during the deadline cleanup.
pub(crate) fn wait_for_child(child: &mut Child, deadline: Instant) -> Result<ExitStatus, Error> {
    let ended = supervise_child(child, deadline, Duration::ZERO, &mut || false)?;
    match ended.cut {
        None => Ok(ended.status),
        Some(_cut) => Err(Error::Timeout),
    }
}

/// Waits for the direct child until `deadline` or until `stop` returns
/// `true`, then stops its group with a grace period.
///
/// `stop` is checked before the deadline on every poll, so a stop request
/// that is pending when the deadline expires wins. Stopping sends `SIGTERM`
/// to the group, waits until the direct child exits or `grace` elapses, then
/// sends `SIGKILL` to the group, so descendants that ignore `SIGTERM` or
/// outlive the direct child still end. A zero `grace` sends only `SIGKILL`.
/// After a normal exit the rest of the group is killed with `SIGKILL`. The
/// direct child is always reaped before returning.
///
/// # Errors
///
/// Returns [`Error::Wait`] when observing or reaping the child fails; the
/// group is killed with `SIGKILL` first.
pub(crate) fn supervise_child(
    child: &mut Child,
    deadline: Instant,
    grace: Duration,
    stop: &mut dyn FnMut() -> bool,
) -> Result<Ended, Error> {
    loop {
        match child_exited(child) {
            Ok(true) => {
                kill_process_group(child.id(), libc::SIGKILL);
                let status = child.wait().map_err(Error::Wait)?;
                return Ok(Ended { status, cut: None });
            }
            Ok(false) => {
                let cut = if stop() {
                    Some(Cut::Stopped)
                } else {
                    (Instant::now() >= deadline).then_some(Cut::Deadline)
                };
                if let Some(cut) = cut {
                    let status = terminate_process_group(child, grace)?;
                    return Ok(Ended {
                        status,
                        cut: Some(cut),
                    });
                }
                thread::sleep(
                    PROCESS_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(error) => {
                terminate_process_group(child, Duration::ZERO)?;
                return Err(error);
            }
        }
    }
}

/// Observes exit without reaping, preserving the process-group ID until cleanup.
#[expect(
    unsafe_code,
    reason = "waitid with WNOWAIT is required to avoid a process-group ID reuse race"
)]
fn child_exited(child: &Child) -> Result<bool, Error> {
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: the output points to initialized storage, `child.id()` names our
    // direct child, and WNOWAIT deliberately leaves that child waitable.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(Error::Wait(io::Error::last_os_error()));
    }
    // SAFETY: successful waitid initializes `siginfo_t`, including a zero PID
    // when WNOHANG found no state change.
    let pid = unsafe { information.assume_init().si_pid() };
    Ok(pid != 0)
}

/// Stops the still-live child group and always reaps the direct child.
///
/// With a non-zero `grace` the group first gets `SIGTERM`, and the direct
/// child is polled without reaping, so the group ID stays reserved, until it
/// exits or the grace elapses. The group then gets `SIGKILL` either way.
fn terminate_process_group(child: &mut Child, grace: Duration) -> Result<ExitStatus, Error> {
    if !grace.is_zero() {
        kill_process_group(child.id(), libc::SIGTERM);
        let until = Instant::now() + grace;
        // An observation failure ends the grace early; SIGKILL follows.
        while matches!(child_exited(child), Ok(false)) && Instant::now() < until {
            thread::sleep(
                PROCESS_POLL_INTERVAL.min(until.saturating_duration_since(Instant::now())),
            );
        }
    }
    kill_process_group(child.id(), libc::SIGKILL);
    // The group kill includes the direct child. `Child::kill` is a safe
    // fallback if a platform or policy rejected negative-PID signalling.
    let _ = child.kill();
    child.wait().map_err(Error::Wait)
}

/// Sends `signal` to the group while its direct child still reserves the
/// group identifier.
#[expect(
    unsafe_code,
    reason = "libc kill(2) is the existing dependency API for a negative process-group PID"
)]
fn kill_process_group(child_id: u32, signal: libc::c_int) {
    #[expect(
        clippy::cast_possible_wrap,
        reason = "Linux PIDs are bounded below i32::MAX, so conversion to pid_t cannot wrap"
    )]
    let process_group = child_id as libc::pid_t;
    // SAFETY: `process_group(0)` made the direct child's PID its process-group
    // identifier. WNOWAIT or a still-running direct child reserves the PID, so
    // it cannot be recycled; negative PID targets precisely that process group.
    unsafe {
        let _ = libc::kill(-process_group, signal);
    }
}

fn spawn_bounded_reader(
    reader: impl Read + Send + 'static,
    limit: usize,
) -> Receiver<Result<BoundedOutput, Error>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(read_bounded(reader, limit));
    });
    receiver
}

/// Drains the child's stdout on its own thread, retaining at most `limit` bytes.
pub(crate) fn spawn_bounded_reader_stdout(
    reader: ChildStdout,
    limit: usize,
) -> Receiver<Result<BoundedOutput, Error>> {
    spawn_bounded_reader(reader, limit)
}

/// Drains the child's stderr on its own thread, retaining at most `limit` bytes.
pub(crate) fn spawn_bounded_reader_stderr(
    reader: ChildStderr,
    limit: usize,
) -> Receiver<Result<BoundedOutput, Error>> {
    spawn_bounded_reader(reader, limit)
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Result<BoundedOutput, Error> {
    let mut bytes = Vec::with_capacity(limit);
    let mut buffer = [0_u8; READ_CHUNK_BYTES];
    let mut overflow = false;
    loop {
        let count = reader.read(&mut buffer).map_err(Error::Read)?;
        if count == 0 {
            return Ok(BoundedOutput { bytes, overflow });
        }
        let available = limit.saturating_sub(bytes.len());
        let retained = count.min(available);
        bytes.extend_from_slice(&buffer[..retained]);
        overflow |= retained != count;
    }
}

/// Collects both reader results, waiting no later than `deadline`.
///
/// # Errors
///
/// Failures resolve in a fixed order: a stdout collection failure, then a
/// stderr collection failure, then a stdout read failure, then a stderr read
/// failure. Collection fails with [`Error::Timeout`] when a stream is still
/// open at the deadline and [`Error::ReaderLost`] when a reader thread ended
/// without a result; a read fails with [`Error::Read`].
pub(crate) fn receive_readers(
    stdout: &Receiver<Result<BoundedOutput, Error>>,
    stderr: &Receiver<Result<BoundedOutput, Error>>,
    deadline: Instant,
) -> Result<(BoundedOutput, BoundedOutput), Error> {
    let stdout = receive_reader(stdout, deadline)?;
    let stderr = receive_reader(stderr, deadline)?;
    Ok((stdout?, stderr?))
}

fn receive_reader(
    receiver: &Receiver<Result<BoundedOutput, Error>>,
    deadline: Instant,
) -> Result<Result<BoundedOutput, Error>, Error> {
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(output) => Ok(output),
        Err(RecvTimeoutError::Timeout) => Err(Error::Timeout),
        Err(RecvTimeoutError::Disconnected) => Err(Error::ReaderLost),
    }
}

/// Forwards `reader` to `sink` on its own thread, at most `limit` bytes.
///
/// Every chunk is written and flushed as it arrives, so a terminal sink
/// streams. When the stream passes `limit`, only the bytes up to the limit
/// reach the sink, the [`Tally`] reports the overflow, and the reader stops
/// and closes the pipe: a child that keeps writing gets `EPIPE` or
/// `SIGPIPE`. A failing sink is handled the same way, like a closed pipe
/// downstream of a shell pipeline. The receiver yields the sink at end of
/// stream, or [`Error::Read`] when reading the pipe fails.
pub(crate) fn spawn_capped_reader<W>(
    mut reader: impl Read + Send + 'static,
    mut sink: W,
    limit: u64,
) -> (Arc<Tally>, Receiver<Result<W, Error>>)
where
    W: Write + Send + 'static,
{
    let tally = Arc::new(Tally::default());
    let shared = Arc::clone(&tally);
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut buffer = [0_u8; READ_CHUNK_BYTES];
        let result = loop {
            let count = match reader.read(&mut buffer) {
                Ok(0) => break Ok(sink),
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => break Err(Error::Read(error)),
            };
            let room = limit.saturating_sub(shared.delivered());
            let taken = usize::try_from(room).map_or(count, |room| room.min(count));
            shared.writing.store(true, Ordering::Release);
            let written = sink.write_all(&buffer[..taken]).and_then(|()| sink.flush());
            shared.writing.store(false, Ordering::Release);
            shared
                .delivered
                .fetch_add(u64::try_from(taken).unwrap_or(u64::MAX), Ordering::AcqRel);
            if taken < count {
                shared.overflowed.store(true, Ordering::Release);
            }
            if taken < count || written.is_err() {
                break Ok(sink);
            }
        };
        let _ = sender.send(result);
    });
    (tally, receiver)
}

/// How [`drain_capped`] ended.
#[derive(Debug)]
pub(crate) enum Drain<W> {
    /// The stream ended; the reader handed back its sink.
    Ended(W),
    /// The stream stayed open for the idle budget; it is cut.
    Idle,
    /// The caller's stop check fired first.
    Stopped,
}

/// Receives a capped reader's result once its stream ends.
///
/// Call it after the child's group is gone. Every poll interval in which the
/// reader is not inside a sink write counts against `idle`, whether or not
/// bytes arrived in it, so a process outside the group that keeps the pipe
/// open, writing or not, is cut once the counted time reaches `idle`.
/// Intervals the reader spends blocked writing to its sink are never counted:
/// a consumer that applies backpressure (a pager, a slow pipe, a paused
/// terminal) is waited for, and it never has more than the cap to take.
/// `stop` is checked every interval and ends the wait at once.
///
/// # Errors
///
/// Returns [`Error::ReaderLost`] when the reader thread ended without a
/// result, and the reader's own [`Error::Read`].
pub(crate) fn drain_capped<W>(
    receiver: &Receiver<Result<W, Error>>,
    tally: &Tally,
    idle: Duration,
    stop: &mut dyn FnMut() -> bool,
) -> Result<Drain<W>, Error> {
    let mut counted = Duration::ZERO;
    loop {
        let polled = Instant::now();
        match receiver.recv_timeout(PROCESS_POLL_INTERVAL) {
            Ok(result) => return result.map(Drain::Ended),
            Err(RecvTimeoutError::Disconnected) => return Err(Error::ReaderLost),
            Err(RecvTimeoutError::Timeout) => {
                if stop() {
                    return Ok(Drain::Stopped);
                }
                if !tally.writing() {
                    counted += polled.elapsed();
                    if counted >= idle {
                        return Ok(Drain::Idle);
                    }
                }
            }
        }
    }
}

/// Inheritable-descriptor fixtures for scenarios that probe what a child keeps.
///
/// They create descriptors without close-on-exec at controlled numbers, so a
/// scenario can assert that the child sees them closed.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::fs;
    use std::os::fd::AsRawFd as _;

    use super::{soft_descriptor_limit, FIRST_INHERITED_FD};

    /// Upper cap for the descriptor number the high-descriptor fixtures aim at.
    const HIGH_DESCRIPTOR_TARGET: libc::rlim_t = 200;

    /// Minimum descriptor number for the high-numbered inherited fixtures.
    ///
    /// Half of the live soft limit (capped at [`HIGH_DESCRIPTOR_TARGET`]) keeps
    /// the duplicate below the limit under any `ulimit -n`, and above the
    /// low numbers the harness itself holds.
    pub(crate) fn high_descriptor_floor() -> libc::c_int {
        let limit = soft_descriptor_limit().map_or(HIGH_DESCRIPTOR_TARGET, |limit| {
            limit.min(HIGH_DESCRIPTOR_TARGET)
        });
        libc::c_int::try_from(limit / 2).expect("bounded descriptor floor")
    }

    /// Descriptor numbers a shell moves its own state to: the script input, a
    /// saved standard stream behind a redirection, and here-documents.
    ///
    /// `dash` and `bash` allocate these from 10 upward, so a probe that
    /// reports a descriptor in this range would report the shell's own.
    pub(crate) const SHELL_DESCRIPTORS: std::ops::Range<libc::c_int> = 10..16;

    /// The descriptor `bash` keeps its script open on.
    const SHELL_SCRIPT_DESCRIPTOR: libc::c_int = 255;

    fn is_shell_descriptor(descriptor: libc::c_int) -> bool {
        SHELL_DESCRIPTORS.contains(&descriptor) || descriptor == SHELL_SCRIPT_DESCRIPTOR
    }

    /// Duplicates `file` to the lowest free inheritable number from
    /// [`FIRST_INHERITED_FD`] that the probe shell does not use itself.
    ///
    /// The number is allocated atomically and then belongs to this test, so
    /// no other thread can hold it. `dup2` onto a fixed slot is deliberately
    /// avoided: it would silently replace a descriptor a sibling test thread
    /// holds there.
    pub(crate) fn duplicate_inheritable_low(file: &fs::File) -> std::os::fd::OwnedFd {
        let mut skipped = Vec::new();
        let mut minimum = FIRST_INHERITED_FD;
        loop {
            let duplicate = duplicate_inheritable_from(file, minimum);
            let number = duplicate.as_raw_fd();
            if !is_shell_descriptor(number) {
                return duplicate;
            }
            minimum = if SHELL_DESCRIPTORS.contains(&number) {
                SHELL_DESCRIPTORS.end
            } else {
                number + 1
            };
            skipped.push(duplicate);
        }
    }

    #[expect(
        unsafe_code,
        reason = "fcntl(F_DUPFD) builds a controlled high-numbered inheritable descriptor"
    )]
    pub(crate) fn duplicate_inheritable_from(
        file: &fs::File,
        minimum: libc::c_int,
    ) -> std::os::fd::OwnedFd {
        use std::os::fd::{FromRawFd as _, OwnedFd};

        // SAFETY: F_DUPFD duplicates the live descriptor of `file` to the
        // lowest free number at or above `minimum` without close-on-exec.
        let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, minimum) };
        assert!(duplicate >= minimum, "duplicate descriptor above the floor");
        // SAFETY: `duplicate` is a fresh descriptor owned by nothing else.
        unsafe { OwnedFd::from_raw_fd(duplicate) }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd as _;
    use std::process::Stdio;

    use super::fixtures::{duplicate_inheritable_from, high_descriptor_floor};
    use super::*;

    /// Creates a private scratch directory that is removed when it drops.
    fn fixture(tag: &str) -> tempfile::TempDir {
        pohunek_test_support::tempdir_with_prefix(&format!("pohunek-bounded-{tag}-"))
            .expect("create fixture directory")
    }

    #[test]
    fn descriptor_loop_closes_below_the_bound_and_leaves_higher_descriptors() {
        use std::os::unix::process::CommandExt as _;

        let root = fixture("descriptor-loop");
        let low = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.path().join("low"))
            .expect("open low descriptor");
        make_inheritable(low.as_raw_fd());
        let high = duplicate_inheritable_from(&low, high_descriptor_floor());
        let end = high.as_raw_fd();
        let probed = [low.as_raw_fd(), high.as_raw_fd()];
        let mut command = Command::new("/bin/sh");
        command.args(["-c", ":"]).stdin(Stdio::null());
        #[expect(
            unsafe_code,
            reason = "pre_exec exercises the descriptor loop exactly as production runs it"
        )]
        // SAFETY: both closures call only async-signal-safe `close`, `fcntl`, and `write`.
        unsafe {
            command
                .pre_exec(move || {
                    close_descriptor_range(FIRST_INHERITED_FD, end);
                    Ok(())
                })
                .pre_exec(move || {
                    for descriptor in probed {
                        let state: &[u8; 1] = if libc::fcntl(descriptor, libc::F_GETFD) == -1 {
                            b"c"
                        } else {
                            b"o"
                        };
                        libc::write(1, state.as_ptr().cast(), 1);
                    }
                    Ok(())
                })
        };
        let output = command.output().expect("run descriptor loop child");
        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8 descriptor states"),
            "co"
        );
    }

    #[test]
    fn descriptor_scan_end_bounds_unlimited_huge_and_narrowed_limits() {
        let ceiling = MAX_DESCRIPTOR_SCAN;
        let none_open = Some(NO_OPEN_DESCRIPTOR);
        // An unreadable or infinite limit still scans the whole bounded table.
        assert_eq!(descriptor_scan_end(None, none_open), ceiling);
        assert_eq!(
            descriptor_scan_end(Some(libc::RLIM_INFINITY), none_open),
            ceiling
        );
        assert_eq!(descriptor_scan_end(Some(u64::MAX - 1), none_open), ceiling);
        assert_eq!(
            descriptor_scan_end(Some(u64::from(u32::MAX)), none_open),
            ceiling
        );
        assert_eq!(descriptor_scan_end(Some(u64::MAX / 2), none_open), ceiling);
        assert_eq!(descriptor_scan_end(Some(20_000_000), none_open), ceiling);
        // With a complete listing, a real limit below the ceiling is honoured
        // exactly, including tiny ones.
        assert_eq!(descriptor_scan_end(Some(10_240), none_open), 10_240);
        assert_eq!(descriptor_scan_end(Some(2), none_open), 2);
        // A failed listing is distinct from an empty one: the limit cannot be
        // trusted, so the whole bounded range is scanned.
        assert_eq!(descriptor_scan_end(Some(2), None), ceiling);
        assert_eq!(descriptor_scan_end(Some(10_240), None), ceiling);
        // The highest open descriptor only raises the bound, never narrows it.
        assert_eq!(
            descriptor_scan_end(Some(libc::RLIM_INFINITY), Some(9)),
            ceiling
        );
        assert_eq!(descriptor_scan_end(Some(10_240), Some(9)), 10_240);
        assert_eq!(descriptor_scan_end(Some(8), Some(9)), 10);
        // A soft limit lowered below an already-open high descriptor still covers it.
        assert_eq!(descriptor_scan_end(Some(32), Some(100)), 101);
        assert_eq!(
            descriptor_scan_end(Some(32), Some(libc::c_int::MAX)),
            libc::c_int::MAX
        );
    }

    /// Runs the production `close` loop up to `end` in a `pre_exec` child and
    /// reports whether `descriptor` is closed afterwards.
    fn descriptor_closed_after_loop(end: libc::c_int, descriptor: libc::c_int) -> bool {
        use std::os::unix::process::CommandExt as _;

        let mut command = Command::new("/bin/sh");
        command.args(["-c", ":"]).stdin(Stdio::null());
        #[expect(
            unsafe_code,
            reason = "pre_exec exercises the descriptor loop exactly as production runs it"
        )]
        // SAFETY: both closures call only async-signal-safe `close`, `fcntl`, and `write`.
        unsafe {
            command
                .pre_exec(move || {
                    close_descriptor_range(FIRST_INHERITED_FD, end);
                    Ok(())
                })
                .pre_exec(move || {
                    let state: &[u8; 1] = if libc::fcntl(descriptor, libc::F_GETFD) == -1 {
                        b"c"
                    } else {
                        b"o"
                    };
                    libc::write(1, state.as_ptr().cast(), 1);
                    Ok(())
                })
        };
        command.output().expect("run descriptor loop child").stdout == b"c"
    }

    #[test]
    fn descriptor_loop_covers_a_high_descriptor_above_a_lowered_soft_limit() {
        let root = fixture("lowered-limit");
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.path().join("probe"))
            .expect("open probe descriptor");
        make_inheritable(file.as_raw_fd());
        let high = duplicate_inheritable_from(&file, high_descriptor_floor());
        // A soft limit far below the live high descriptor, as `setrlimit` allows.
        let lowered = libc::rlim_t::try_from(FIRST_INHERITED_FD).expect("small limit");
        // Injected outcomes of the descriptor listing: it succeeded, or it failed.
        for listing in [Some(high.as_raw_fd()), None] {
            let end = descriptor_scan_end(Some(lowered), listing);
            assert!(end > high.as_raw_fd(), "listing {listing:?}");
            assert!(
                descriptor_closed_after_loop(end, high.as_raw_fd()),
                "listing {listing:?}"
            );
        }
    }

    #[test]
    fn host_scan_bound_covers_every_open_descriptor() {
        let root = fixture("scan-bound");
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.path().join("probe"))
            .expect("open probe descriptor");
        let high = duplicate_inheritable_from(&file, high_descriptor_floor());
        assert!(
            soft_descriptor_limit().is_some(),
            "RLIMIT_NOFILE is readable"
        );
        let bound = descriptor_scan_bound();
        assert!(
            bound > high.as_raw_fd(),
            "bound {bound} covers the highest descriptor"
        );
        assert!(bound > file.as_raw_fd());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_descriptor_listing_reports_the_highest_open_descriptor() {
        let root = fixture("darwin-listing");
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.path().join("probe"))
            .expect("open probe descriptor");
        let high = duplicate_inheritable_from(&file, high_descriptor_floor());
        let highest = highest_open_descriptor().expect("descriptor table listing");
        assert!(highest >= high.as_raw_fd());
    }

    #[expect(
        unsafe_code,
        reason = "fcntl is required to construct the controlled inheritance regression fixture"
    )]
    fn make_inheritable(descriptor: libc::c_int) {
        // SAFETY: the descriptor is owned by the live `File` in this test, and
        // F_GETFD/F_SETFD only inspect and clear its close-on-exec flag.
        unsafe {
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            assert!(flags >= 0, "read descriptor flags");
            assert_eq!(
                libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC),
                0,
                "clear close-on-exec"
            );
        }
    }
}
