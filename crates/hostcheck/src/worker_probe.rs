//! Bounded capability probe for a trusted session-worker executable.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Maximum JSON response accepted from a worker executable.
pub const MAX_WORKER_INFO_BYTES: usize = 4096;

/// Maximum time the daemon doctor gives the installed worker capability probe.
pub const WORKER_PROBE_DEADLINE: Duration = Duration::from_secs(5);

/// Bounds timeout detection latency while leaving the caller's deadline exact.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Machine-readable worker identity and compatibility facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerInfo {
    /// Capability response schema version.
    pub schema_version: u32,
    /// Worker package version.
    pub version: String,
    /// Worker journal schema version.
    pub journal_schema_version: u32,
    /// Lowest worker protocol version supported.
    pub protocol_minimum: u32,
    /// Highest worker protocol version supported.
    pub protocol_maximum: u32,
}

/// Failure while starting or validating worker protocol information.
#[derive(Debug, thiserror::Error)]
pub enum WorkerProbeError {
    /// The executable could not be started or observed.
    #[error("worker capability probe could not run")]
    Unavailable,
    /// The executable exceeded the caller's deadline.
    #[error("worker capability probe timed out")]
    TimedOut,
    /// The response exceeded the fixed size limit.
    #[error("worker capability response exceeded its size limit")]
    OutputTooLarge,
    /// The process failed or returned an invalid response.
    #[error("worker capability response was invalid")]
    InvalidResponse,
    /// The binary reported a different identity.
    #[error("worker executable reported the wrong binary identity")]
    IdentityMismatch,
    /// The worker's schema or protocol range cannot be used by this daemon.
    #[error("worker executable is incompatible with this daemon")]
    Incompatible,
    /// The executable predates the capability command, so compatibility is unverified.
    #[error("worker executable does not support capability reporting")]
    Unsupported,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolInfo {
    schema_version: u32,
    binary: String,
    version: String,
    journal_schema_version: u32,
    worker_protocol: ProtocolRange,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolRange {
    minimum: u32,
    maximum: u32,
}

/// Runs the worker's inert protocol-info command and validates its contract.
///
/// The executable is treated as trusted installation state. The probe passes
/// one fixed argument, clears its environment, limits output and runtime, and
/// kills its process group on every early exit.
///
/// # Errors
///
/// Returns an error when the process cannot complete within the bound, its
/// output is malformed, or the reported schema/protocol cannot be adopted.
pub fn probe_worker(
    executable: &Path,
    timeout: Duration,
    readable_journal_schemas: &[u32],
    supported_protocol: pohunek_worker_protocol::VersionRange,
) -> Result<WorkerInfo, WorkerProbeError> {
    let started = Instant::now();
    let (output, error_output, status) = run_probe_command(executable, "--protocol-info", timeout)?;
    if !status.success() {
        if !output.is_empty() || !reports_unsupported_protocol_info(&error_output) {
            return Err(WorkerProbeError::InvalidResponse);
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(WorkerProbeError::TimedOut);
        }
        let (legacy_output, _legacy_error_output, legacy_status) =
            run_probe_command(executable, "--version", remaining)?;
        if legacy_status.success() && legacy_worker_version(&legacy_output).is_some() {
            return Err(WorkerProbeError::Unsupported);
        }
        return Err(WorkerProbeError::InvalidResponse);
    }

    let info: ProtocolInfo =
        serde_json::from_slice(&output).map_err(|_error| WorkerProbeError::InvalidResponse)?;
    if info.binary != "pohunek-sessiond" {
        return Err(WorkerProbeError::IdentityMismatch);
    }
    if info.version.is_empty() || info.version.len() > 64 {
        return Err(WorkerProbeError::InvalidResponse);
    }
    if info.schema_version != pohunek_worker_protocol::WORKER_DIAGNOSTIC_SCHEMA_VERSION
        || !readable_journal_schemas.contains(&info.journal_schema_version)
        || info.worker_protocol.minimum > info.worker_protocol.maximum
        || info.worker_protocol.maximum < supported_protocol.minimum().get()
        || info.worker_protocol.minimum > supported_protocol.maximum().get()
    {
        return Err(WorkerProbeError::Incompatible);
    }
    Ok(WorkerInfo {
        schema_version: info.schema_version,
        version: info.version,
        journal_schema_version: info.journal_schema_version,
        protocol_minimum: info.worker_protocol.minimum,
        protocol_maximum: info.worker_protocol.maximum,
    })
}

fn run_probe_command(
    executable: &Path,
    argument: &str,
    timeout: Duration,
) -> Result<(Vec<u8>, Vec<u8>, ExitStatus), WorkerProbeError> {
    let mut command = Command::new(executable);
    command
        .arg(argument)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|_error| WorkerProbeError::Unavailable)?;
    let Ok(raw_group) = i32::try_from(child.id()) else {
        terminate(&mut child);
        return Err(WorkerProbeError::Unavailable);
    };
    let Some(group) = rustix::process::Pid::from_raw(raw_group) else {
        terminate(&mut child);
        return Err(WorkerProbeError::Unavailable);
    };
    let Some(stdout) = child.stdout.take() else {
        terminate_group(group);
        let _ = child.wait();
        return Err(WorkerProbeError::Unavailable);
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_group(group);
        let _ = child.wait();
        return Err(WorkerProbeError::Unavailable);
    };
    let (sender, receiver) = mpsc::channel();
    let readers: [(u8, Box<dyn Read + Send>); 2] = [(0, Box::new(stdout)), (1, Box::new(stderr))];
    for (stream, mut reader) in readers {
        let sender = sender.clone();
        thread::spawn(move || {
            let mut bytes = Vec::with_capacity(MAX_WORKER_INFO_BYTES);
            let result = reader
                .by_ref()
                .take((MAX_WORKER_INFO_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = sender.send((stream, result));
        });
    }
    drop(sender);

    let started = Instant::now();
    let (output, error_output) =
        read_probe_streams(&receiver, timeout, started, group, &mut child)?;
    // EOF can precede process exit when a broken worker closes its pipes and
    // hangs. Keep the leader unreaped so its process-group id remains valid
    // while enforcing the same deadline.
    let status = wait_for_probe_exit(&mut child, timeout, started, group)?;
    Ok((output, error_output, status))
}

fn read_probe_streams(
    receiver: &mpsc::Receiver<(u8, io::Result<Vec<u8>>)>,
    timeout: Duration,
    started: Instant,
    group: rustix::process::Pid,
    child: &mut std::process::Child,
) -> Result<(Vec<u8>, Vec<u8>), WorkerProbeError> {
    let mut output = None;
    let mut error_output = None;
    while output.is_none() || error_output.is_none() {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            terminate_group(group);
            let _ = child.wait();
            return Err(WorkerProbeError::TimedOut);
        }
        match receiver.recv_timeout(remaining.min(POLL_INTERVAL)) {
            Ok((stream, Ok(bytes))) if bytes.len() <= MAX_WORKER_INFO_BYTES => {
                if stream == 0 {
                    output = Some(bytes);
                } else {
                    error_output = Some(bytes);
                }
            }
            Ok((_, Ok(_))) => {
                terminate_group(group);
                let _ = child.wait();
                return Err(WorkerProbeError::OutputTooLarge);
            }
            Ok((_, Err(_))) => {
                terminate_group(group);
                let _ = child.wait();
                return Err(WorkerProbeError::InvalidResponse);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                terminate_group(group);
                let _ = child.wait();
                return Err(WorkerProbeError::Unavailable);
            }
        }
    }
    Ok((
        output.expect("stdout reader completed"),
        error_output.expect("stderr reader completed"),
    ))
}

fn wait_for_probe_exit(
    child: &mut std::process::Child,
    timeout: Duration,
    started: Instant,
    group: rustix::process::Pid,
) -> Result<ExitStatus, WorkerProbeError> {
    // EOF can precede process exit when a broken worker closes its pipes and
    // hangs. Keep the leader unreaped so its process-group id remains valid
    // while enforcing the same deadline.
    loop {
        match rustix::process::waitid(
            rustix::process::WaitId::Pid(group),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        ) {
            Ok(Some(_status)) => {
                // Signal while the exited leader is still waitable, so its
                // process-group id cannot have been reused by another job.
                terminate_group(group);
                return child.wait().map_err(|_error| WorkerProbeError::Unavailable);
            }
            Ok(None) => {
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    terminate_group(group);
                    let _ = child.wait();
                    return Err(WorkerProbeError::TimedOut);
                }
                thread::sleep(remaining.min(POLL_INTERVAL));
            }
            Err(_error) => {
                terminate_group(group);
                let _ = child.wait();
                return Err(WorkerProbeError::Unavailable);
            }
        }
    }
}

fn legacy_worker_version(output: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(output).ok()?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.contains(['\n', '\r']) {
        return None;
    }
    let version = text.strip_prefix("pohunek-sessiond ")?;
    (!version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')))
    .then_some(version)
}

fn reports_unsupported_protocol_info(error_output: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(error_output) else {
        return false;
    };
    let text = text.strip_suffix('\n').unwrap_or(text);
    text == "pohunek-sessiond: fatal: worker protocol failed: unknown worker argument `--protocol-info`"
}

fn terminate(child: &mut std::process::Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        if let Some(group) = rustix::process::Pid::from_raw(pid) {
            terminate_group(group);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_group(group: rustix::process::Pid) {
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
}
