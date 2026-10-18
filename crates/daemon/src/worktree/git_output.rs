//! Synchronous git output capture with no pipe readers surviving the call.

use std::io::{self, PipeReader};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::Errno;

use super::{
    configure_process_group, terminate_process_group, GIT_COMMAND_POLL_INTERVAL,
    GIT_COMMAND_POLL_START,
};

// Buffered local output settles quickly; an inherited writer must not pin a bind.
const GIT_OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(250);
// Allow ordinary diffs while bounding the two retained streams to 16 MiB total.
pub(super) const GIT_OUTPUT_CAP_BYTES: usize = 8 * 1024 * 1024;
// One bounded read per stream keeps a busy writer from starving deadlines or stderr.
const READ_CHUNK_BYTES: usize = 16 * 1024;

pub(super) struct Captured {
    pub output: Output,
    pub deadline_hit: bool,
    pub cap_exceeded: bool,
}

struct Stream {
    pipe: Option<PipeReader>,
    bytes: Vec<u8>,
    capped: bool,
}

impl Stream {
    fn new() -> io::Result<(Self, Stdio)> {
        let (reader, writer) = io::pipe()?;
        rustix::io::ioctl_fionbio(&reader, true)?;
        Ok((
            Self {
                pipe: Some(reader),
                bytes: Vec::new(),
                capped: false,
            },
            writer.into(),
        ))
    }

    fn read(&mut self) -> io::Result<()> {
        let Some(pipe) = self.pipe.as_ref() else {
            return Ok(());
        };
        let mut chunk = [0_u8; READ_CHUNK_BYTES];
        match rustix::io::read(pipe, &mut chunk) {
            Ok(0) => {
                self.pipe = None;
            }
            Ok(count) => {
                let keep = count.min(GIT_OUTPUT_CAP_BYTES - self.bytes.len());
                self.bytes.extend_from_slice(&chunk[..keep]);
                self.capped |= keep < count;
            }
            Err(Errno::INTR | Errno::AGAIN) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

/// Reap the leader before returning and close every pipe on every path.
/// `after_exit` runs immediately on reap, before any post-exit drain.
pub(super) fn capture(
    mut command: Command,
    timeout: Duration,
    after_exit: impl FnOnce(),
) -> Result<Captured, String> {
    let (stdout, stdout_writer) = Stream::new().map_err(|error| capture_error(&error))?;
    let (stderr, stderr_writer) = Stream::new().map_err(|error| capture_error(&error))?;
    configure_process_group(&mut command);
    command.stdout(stdout_writer).stderr(stderr_writer);
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to run git: {error}"))?;
    // Command retains its configured writers after spawn; they must not keep EOF open.
    drop(command);
    let mut streams = [stdout, stderr];
    let mut after_exit = Some(after_exit);
    let mut status = None;
    let mut drain_deadline = None;
    let command_deadline = Instant::now() + timeout;
    let mut cadence = GIT_COMMAND_POLL_START;
    let result = collect(
        &mut child,
        &mut streams,
        &mut status,
        &mut drain_deadline,
        command_deadline,
        &mut cadence,
        &mut after_exit,
    );
    // A read/poll/wait error still owns an unreaped child. Never signal after reap.
    if status.is_none() {
        terminate_process_group(&mut child);
        if let Some(after_exit) = after_exit.take() {
            after_exit();
        }
    }
    let deadline_hit = result.map_err(|error| match error {
        CollectError::TimedOut => format!("git command timed out after {timeout:?}"),
        CollectError::Io(error) => capture_error(&error),
    })?;
    let [stdout, stderr] = streams;
    Ok(Captured {
        output: Output {
            status: status.expect("successful capture reaped git"),
            stdout: stdout.bytes,
            stderr: stderr.bytes,
        },
        deadline_hit,
        cap_exceeded: stdout.capped || stderr.capped,
    })
}

enum CollectError {
    TimedOut,
    Io(io::Error),
}

impl From<io::Error> for CollectError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn collect(
    child: &mut Child,
    streams: &mut [Stream; 2],
    status: &mut Option<ExitStatus>,
    drain_deadline: &mut Option<Instant>,
    command_deadline: Instant,
    cadence: &mut Duration,
    after_exit: &mut Option<impl FnOnce()>,
) -> Result<bool, CollectError> {
    loop {
        if status.is_none() {
            if let Some(exited) = child.try_wait()? {
                *status = Some(exited);
                *drain_deadline = Some(Instant::now() + GIT_OUTPUT_DRAIN_GRACE);
                if let Some(after_exit) = after_exit.take() {
                    after_exit();
                }
            } else if Instant::now() >= command_deadline {
                return Err(CollectError::TimedOut);
            }
        }
        for stream in &mut *streams {
            stream.read()?;
        }
        let open = streams.iter().any(|stream| stream.pipe.is_some());
        if status.is_some() && !open {
            return Ok(false);
        }
        let deadline = drain_deadline.unwrap_or(command_deadline);
        if Instant::now() >= deadline && status.is_some() {
            return Ok(open);
        }
        let wait = (*cadence).min(deadline.saturating_duration_since(Instant::now()));
        let wait = Timespec::try_from(wait).map_err(|error| io::Error::other(error.to_string()))?;
        let mut fds: Vec<_> = streams
            .iter()
            .filter_map(|stream| stream.pipe.as_ref())
            .map(|pipe| PollFd::new(pipe, PollFlags::IN))
            .collect();
        match poll(&mut fds, Some(&wait)) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(error) => return Err(io::Error::from(error).into()),
        }
        *cadence = (*cadence * 2).min(GIT_COMMAND_POLL_INTERVAL);
    }
}

fn capture_error(error: &io::Error) -> String {
    format!("failed to capture git output: {error}")
}
