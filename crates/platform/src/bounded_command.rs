//! Bounded execution of a short-lived helper command.
//!
//! The command runs in its own process group, so an expiry kills the helper
//! and everything it started, and the group is reaped with it. Output beyond
//! [`MAX_OUTPUT_BYTES`] per stream is discarded while the pipe keeps draining,
//! so a chatty helper can neither fill memory nor block on a full pipe.

use std::io;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use rustix::process::{kill_process_group, Pid, Signal};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::{Child, Command};

/// Largest number of bytes kept from each of stdout and stderr.
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Longest wait for a killed child to be reaped.
const REAP_GRACE: Duration = Duration::from_secs(5);

/// Size of one read from a command's output pipe.
const READ_CHUNK_BYTES: usize = 8192;

/// Why a bounded command produced no output.
#[derive(Debug)]
pub enum BoundedError {
    /// The command could not be started or its output could not be read.
    Io(io::Error),
    /// The command was still running when the deadline elapsed. Its process
    /// group has been killed and the command reaped.
    Expired(Duration),
}

/// Runs `program args` and collects its output within `deadline`.
///
/// The child gets no stdin. When the deadline elapses the whole process group
/// is killed and the child is waited for. Dropping the future kills the child.
///
/// # Errors
///
/// [`BoundedError::Io`] when the command cannot run, and
/// [`BoundedError::Expired`] when it, or a process in its group that keeps its
/// output open, outlasts `deadline`.
pub async fn run_bounded(
    program: &Path,
    args: &[&str],
    deadline: Duration,
) -> Result<Output, BoundedError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        // Dropping the future (daemon shutdown) must not leave the helper behind.
        .kill_on_drop(true)
        .spawn()
        .map_err(BoundedError::Io)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // The child is waited for only after both pipes closed, so it stays
    // unreaped, and its pid stays the group id, for as long as any process of
    // the group may still hold an output pipe.
    let collected = tokio::time::timeout(deadline, async {
        let (stdout, stderr) = tokio::join!(read_capped(stdout), read_capped(stderr));
        Ok::<_, io::Error>(Output {
            status: child.wait().await?,
            stdout: stdout?,
            stderr: stderr?,
        })
    })
    .await;
    match collected {
        Ok(output) => output.map_err(BoundedError::Io),
        Err(_elapsed) => {
            terminate(&mut child).await;
            Err(BoundedError::Expired(deadline))
        }
    }
}

/// Kills the child's process group and waits for the child, for at most
/// [`REAP_GRACE`].
///
/// The group id is the child's pid, which `process_group(0)` made the leader.
/// The child has not been reaped when this runs, so no other process can hold
/// that id. A failed kill means the group is already gone.
async fn terminate(child: &mut Child) {
    if let Some(group) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
    {
        let _ = kill_process_group(group, Signal::KILL);
    }
    // Reaps the child; the grace bounds a child stuck in uninterruptible sleep.
    let _ = tokio::time::timeout(REAP_GRACE, child.wait()).await;
}

async fn read_capped<R: AsyncRead + Unpin>(pipe: Option<R>) -> io::Result<Vec<u8>> {
    let mut kept = Vec::new();
    let Some(mut pipe) = pipe else {
        return Ok(kept);
    };
    // On the heap: a stack buffer would make every caller's future large.
    let mut chunk = vec![0_u8; READ_CHUNK_BYTES];
    loop {
        let read = pipe.read(&mut chunk).await?;
        if read == 0 {
            return Ok(kept);
        }
        let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(room)]);
    }
}
