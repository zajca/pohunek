//! Subprocess calls of the release consumer suite with a deadline.
//!
//! A prebuilt test executable has no per-test timeout, so a child that hangs,
//! or that exits after spawning a background process which keeps its output
//! pipes open, must not be able to stall the run: the already started daemon
//! would never be cleaned up. [`run`] puts the child in its own process
//! group, bounds both the child's lifetime and the collection of its output,
//! and on any overrun kills and reaps the whole group.

// Rust guideline compliant 2026-10-08

use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;

/// Deadline of an upstream `--version` read or a staged `--version`. These
/// start a Node or native binary that prints one line; 30 s is far above a
/// cold start on a loaded runner and far below a stalled run. Raising it only
/// delays the failure of a hung upstream.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline of one `pohunek` CLI call. The longest calls are `plugin install`
/// and `session new`, which return within seconds; `session wait` is bounded
/// by its own 8 s slice. 90 s keeps a hung CLI from stalling the run while
/// staying under the 120 s ceiling of one wait in `pohunek_test_support::wait`.
pub(crate) const CLI_TIMEOUT: Duration = Duration::from_secs(90);

/// How long output collection may take after the child itself has exited. A
/// background process that inherited the pipes holds them open; once this
/// grace has passed the group is killed, which closes them.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Most bytes kept from each of stdout and stderr; a probe prints a line, and
/// the CLI prints one JSON document.
const MAX_CAPTURE_BYTES: u64 = 8 * 1024 * 1024;

/// Runs `command` for at most `timeout` and collects its output.
///
/// `label` names the program in every error message. Standard input is
/// closed. The child leads a new process group; the group is killed after the
/// child exits (a leftover background process must not outlive the call) and
/// on a timeout.
///
/// # Errors
///
/// Fails with a message naming `label` when the child cannot be started, does
/// not exit within `timeout` (the group is killed and reaped first), or
/// cannot be waited for.
pub(crate) fn run(mut command: Command, label: &str, timeout: Duration) -> Result<Output, String> {
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start {label}: {error}"))?;
    let group = Pid::from_raw(i32::try_from(child.id()).map_err(|error| error.to_string())?);
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    for (mut pipe, tx) in [
        (Box::new(stdout) as Box<dyn std::io::Read + Send>, out_tx),
        (Box::new(stderr), err_tx),
    ] {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            // A read error ends the capture with what arrived so far.
            let _ = (&mut pipe).take(MAX_CAPTURE_BYTES).read_to_end(&mut bytes);
            let _ = tx.send(bytes);
        });
    }
    let (status_tx, status_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = status_tx.send(child.wait());
    });

    let kill_group = || {
        // The group may be gone already.
        let _ = killpg(group, Signal::SIGKILL);
    };
    let Ok(waited) = status_rx.recv_timeout(timeout) else {
        kill_group();
        // The kill makes the waiter return; reaping is bounded by it.
        let _reaped = status_rx.recv_timeout(DRAIN_GRACE);
        return Err(format!(
            "{label} did not exit within {timeout:?}; its process group was killed"
        ));
    };
    let status: ExitStatus = waited.map_err(|error| format!("cannot wait for {label}: {error}"))?;
    let collect = |rx: &mpsc::Receiver<Vec<u8>>| {
        if let Ok(bytes) = rx.recv_timeout(DRAIN_GRACE) {
            return Ok(bytes);
        }
        kill_group();
        rx.recv_timeout(DRAIN_GRACE)
            .map_err(|_closed| format!("the output of {label} could not be collected"))
    };
    let stdout = collect(&out_rx)?;
    let stderr = collect(&err_rx)?;
    kill_group();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
