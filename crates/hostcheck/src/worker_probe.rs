//! Bounded capability probe for a trusted session-worker executable.

// Rust guideline compliant 2026-10-11

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Maximum JSON response accepted from a worker executable.
pub const MAX_WORKER_INFO_BYTES: usize = 4096;

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
    let mut command = Command::new(executable);
    command
        .arg("--protocol-info")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::with_capacity(MAX_WORKER_INFO_BYTES);
        let result = stdout
            .take((MAX_WORKER_INFO_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });

    let started = Instant::now();
    let output = loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            terminate_group(group);
            let _ = child.wait();
            return Err(WorkerProbeError::TimedOut);
        }
        match receiver.recv_timeout(remaining.min(POLL_INTERVAL)) {
            Ok(Ok(bytes)) => break bytes,
            Ok(Err(_)) => {
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
    };
    // The leader is still unreaped here, so its process-group id is valid.
    terminate_group(group);
    let status = child
        .wait()
        .map_err(|_error| WorkerProbeError::Unavailable)?;
    if output.len() > MAX_WORKER_INFO_BYTES {
        return Err(WorkerProbeError::OutputTooLarge);
    }
    if !status.success() {
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
