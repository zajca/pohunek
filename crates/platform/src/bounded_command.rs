//! Bounded execution of a short-lived helper command.
//!
//! The command runs under a small `sh` supervisor in its own process group.
//! Every way the call can end takes the group down with it:
//!
//! - the deadline elapses;
//! - the caller's future is dropped (an outer budget, a shutdown);
//! - the calling process dies, by a signal or otherwise: the supervisor holds
//!   the read end of a pipe the caller keeps open, and kills the group when
//!   that pipe closes.
//!
//! The group is killed with `SIGKILL`. The deadline and drop paths send it up
//! to [`KILL_PASSES`] times, because on macOS a process that forks while the
//! signal walks the group can escape one pass. The watcher sends one pass: it
//! belongs to the group and dies with it, so it cannot repeat.
//!
//! Output beyond [`MAX_OUTPUT_BYTES`] per stream is discarded while the pipe
//! keeps draining, so a chatty helper can neither fill memory nor block on a
//! full pipe.

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

/// Number of `SIGKILL` passes sent to a process group.
///
/// A later pass reaches a child whose fork was still joining the group while an
/// earlier pass walked it. The passes narrow that window without closing it: a
/// fork that races every pass still escapes. The supervisor stays unreaped
/// while the passes run, which keeps its pid, the group id, from being reused.
/// Its zombie also keeps the group non-empty, so the passes are counted rather
/// than repeated until the group is gone.
const KILL_PASSES: u32 = 3;

/// Size of one read from a command's output pipe.
const READ_CHUNK_BYTES: usize = 8192;

/// The POSIX shell that runs the supervisor.
const SHELL: &str = "/bin/sh";

/// Runs the command (`"$@"`) with a null stdin and waits for it.
///
/// Its own stdin is a pipe held by the caller: a watcher kills the whole
/// process group of this shell, `$$`, when that pipe reaches end of file, which
/// happens when the caller exits for any reason, or closes the pipe after the
/// shell exited. The watcher outlives the shell so a helper that keeps the
/// output open is still reaped while the caller drains it; it holds no output
/// pipe itself.
const SUPERVISOR: &str = r#"exec 3<&0
"$@" </dev/null 3<&- &
child=$!
(while read -r _ <&3; do :; done; kill -s KILL -- "-$$") >/dev/null 2>&1 &
wait "$child"
exit "$?"
"#;

/// Why a bounded command produced no output.
#[derive(Debug)]
pub enum BoundedError {
    /// The command could not be started or its output could not be read.
    Io(io::Error),
    /// The command was still running when the deadline elapsed. Its process
    /// group has been killed and the command reaped.
    Expired(Duration),
}

/// Kills a process group when dropped while armed.
///
/// The group id is the pid of the supervisor, the group leader. The guard is
/// armed only while the supervisor is unreaped, so its pid cannot belong to
/// another process.
struct GroupGuard {
    group: Option<Pid>,
}

impl GroupGuard {
    fn disarm(&mut self) {
        self.group = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(group) = self.group {
            for pass in 0..KILL_PASSES {
                if pass > 0 {
                    std::thread::yield_now();
                }
                // A group with no member left is already gone.
                if kill_process_group(group, Signal::KILL).is_err() {
                    break;
                }
            }
        }
    }
}

/// Runs `program args` and collects its output within `deadline`.
///
/// The command gets no stdin. When the deadline elapses, or this future is
/// dropped before the command was reaped, the whole process group is killed
/// (see [`KILL_PASSES`]); on expiry the supervisor is also waited for.
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
    let mut child = Command::new(SHELL)
        .arg("-c")
        .arg(SUPERVISOR)
        .arg("pohunek-bounded-command")
        .arg(program)
        .args(args)
        // Held open until this call ends; its closing is the supervisor's
        // signal that the caller is gone.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(BoundedError::Io)?;
    // Declared after `child` so it drops first, while the child is unreaped.
    let mut guard = GroupGuard {
        group: child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // The child is waited for only after both pipes closed, so it stays
    // unreaped, and its pid stays the group id, for as long as any process of
    // the group may still hold an output pipe.
    let collected = tokio::time::timeout(deadline, async {
        let (stdout, stderr) = tokio::join!(read_capped(stdout), read_capped(stderr));
        let status = child.wait().await?;
        guard.disarm();
        Ok::<_, io::Error>(Output {
            status,
            stdout: stdout?,
            stderr: stderr?,
        })
    })
    .await;
    match collected {
        Ok(output) => output.map_err(BoundedError::Io),
        Err(_elapsed) => {
            terminate(&mut child).await;
            guard.disarm();
            Err(BoundedError::Expired(deadline))
        }
    }
}

/// Kills the child's process group in [`KILL_PASSES`] passes and waits for the
/// child, for at most [`REAP_GRACE`].
///
/// The child has not been reaped while the passes run, so no other process can
/// hold its pid. A failed kill means the group is already gone.
async fn terminate(child: &mut Child) {
    if let Some(group) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
    {
        for pass in 0..KILL_PASSES {
            if pass > 0 {
                tokio::task::yield_now().await;
            }
            if kill_process_group(group, Signal::KILL).is_err() {
                break;
            }
        }
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
