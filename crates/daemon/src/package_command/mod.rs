//! Package commands: running a command a package declared, on request.
//!
//! [`PackageCommandService::run`] resolves a package id and a command name to
//! the command of the package version the registry selects, checks that the
//! connection's transport may call it, verifies the package root again and
//! runs the declared program once under the bounded runner. A call supplies
//! no argument, environment, working directory or input: the program is the
//! path of a file of the re-verified root, the arguments are the descriptor's
//! literals, the environment is the runner's fixed set, stdin is closed and
//! the working directory is the runner's private scratch directory.
//!
//! Admission is per command (one run at a time) and per host
//! ([`MAX_HOST_COMMAND_RUNS`]); a call over either limit fails at once and
//! nothing queues. A run holds its admission until its process group is gone.
//! Every error is fixed text; the program's output reaches the reply only on
//! success and never a log.

// Rust guideline compliant 2026-10-28

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use protocol::{
    CommandErrorKind, CommandExposure, CommandInfo, CommandName, CommandRunParams,
    CommandRunResult, PackageId, PackageIdentity, ProcessExit, ProtocolError,
    MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_TIMEOUT_MS,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};

use crate::agent::host::PackageStore;
use crate::api::ControlTransport;
use crate::bounded_run::{self, RunLimits, RunOutcome, RunSpec, StderrPolicy};

/// Most package command runs the host executes at once.
///
/// Each run holds a blocking thread and a process group for up to
/// [`MAX_COMMAND_TIMEOUT_MS`], so the cap bounds what a flood of calls can
/// cost. Raising it raises the number of threads and children a hostile peer
/// can hold.
pub const MAX_HOST_COMMAND_RUNS: usize = 4;

/// How long shutdown waits for runs whose cancel flag it set.
///
/// A cancelled run kills its group within one runner poll, so the wait only
/// covers scheduling; it bounds a stuck thread without stalling the daemon's
/// exit.
const SHUTDOWN_DRAIN_BOUND: Duration = Duration::from_secs(5);

/// What the service needs from the daemon.
#[derive(Debug, Clone)]
pub(crate) struct CommandHost {
    /// The package store, absent on a host that never installed a package.
    pub(crate) store: Option<PackageStore>,
    /// The `PATH` programs see, or why it cannot be formed.
    pub(crate) search_path: Result<Option<OsString>, ProtocolError>,
    /// Cancelled when the daemon begins shutting down.
    pub(crate) shutdown: CancellationToken,
}

/// Runs the commands installed packages declare.
///
/// One service serves every control transport of a daemon, so the limits hold
/// across the local socket and the overlay.
#[derive(Debug, Clone)]
pub struct PackageCommandService {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    host: CommandHost,
    /// Commands with a run in flight.
    running: Mutex<BTreeSet<(PackageId, CommandName)>>,
    /// Host-wide admission.
    slots: Arc<Semaphore>,
    /// Runs in flight, awaited at shutdown.
    tracker: TaskTracker,
}

/// What a resolved call is about to run.
struct Resolved {
    identity: PackageIdentity,
    digest: protocol::PackageDigest,
    command: CommandInfo,
    program: PathBuf,
}

/// Admission held by one run until its process group is gone.
struct Admission {
    inner: Arc<Inner>,
    key: (PackageId, CommandName),
    _slot: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

/// Sets the run's cancel flag when the call stops waiting, so a dropped
/// request kills the process group.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl PackageCommandService {
    pub(crate) fn new(host: CommandHost) -> Self {
        Self {
            inner: Arc::new(Inner {
                host,
                running: Mutex::default(),
                slots: Arc::new(Semaphore::new(MAX_HOST_COMMAND_RUNS)),
                tracker: TaskTracker::new(),
            }),
        }
    }

    /// Runs the command `params` names and returns its stdout.
    ///
    /// `transport` is the transport of the connection the call arrived on; it
    /// alone decides whether a `local` command may run.
    ///
    /// # Errors
    ///
    /// A [`ProtocolError`] of the `command_*` family, or `bad_request` for a
    /// zero timeout or output cap.
    pub async fn run(
        &self,
        params: CommandRunParams,
        transport: ControlTransport,
    ) -> Result<CommandRunResult, ProtocolError> {
        let timeout = clamp_ms(params.timeout_ms, MAX_COMMAND_TIMEOUT_MS)?;
        let cap = clamp_ms(
            params.max_output_bytes,
            u32::try_from(MAX_COMMAND_OUTPUT_BYTES).unwrap_or(u32::MAX),
        )?;
        if self.inner.host.shutdown.is_cancelled() || self.inner.tracker.is_closed() {
            return Err(CommandErrorKind::Unavailable.into());
        }
        let resolved = self.resolve(&params, transport).await?;
        let admission = self.admit(&params)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(Arc::clone(&cancel));
        let search_path = self
            .inner
            .host
            .search_path
            .clone()
            .map_err(|_error| ProtocolError::from(CommandErrorKind::Unavailable))?;
        let started = Instant::now();
        let work = {
            let cancel = Arc::clone(&cancel);
            let args = resolved.command.args.clone();
            let program = resolved.program.clone();
            self.inner.tracker.spawn_blocking(move || {
                let _admission = admission;
                let spec = RunSpec {
                    program: &program,
                    args: &args,
                    search_path: search_path.as_deref(),
                    env: &[],
                    seeded_env: &[],
                    limits: RunLimits::new(Duration::from_millis(u64::from(timeout)), cap as usize),
                    stderr: StderrPolicy::Discard,
                };
                bounded_run::run(&spec, &Instant::now, Some(&cancel))
            })
        };
        let outcome = tokio::select! {
            joined = work => joined,
            () = self.inner.host.shutdown.cancelled() => {
                cancel.store(true, Ordering::Relaxed);
                // The flag ends the run within one poll; the reply reports it.
                return Err(Self::finish(&resolved, transport, started, CommandErrorKind::Unavailable, None, 0));
            }
        };
        let elapsed = started.elapsed();
        let (result, exit, bytes) = match outcome {
            Ok(outcome) => classify(outcome),
            Err(_join) => (Err(CommandErrorKind::Unavailable), None, 0),
        };
        match result {
            Ok(output) => {
                Self::log(&resolved, transport, elapsed, "ok", None, bytes);
                Ok(CommandRunResult {
                    package: resolved.identity,
                    command: resolved.command.name,
                    digest: resolved.digest,
                    output,
                    duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                })
            }
            Err(kind) => Err(Self::finish(
                &resolved, transport, started, kind, exit, bytes,
            )),
        }
    }

    /// Stops accepting runs and waits, bounded, for the ones in flight.
    ///
    /// The daemon's shutdown signal has already set every run's cancel flag.
    pub async fn shutdown(&self) {
        self.inner.tracker.close();
        if tokio::time::timeout(SHUTDOWN_DRAIN_BOUND, self.inner.tracker.wait())
            .await
            .is_err()
        {
            warn!("package command runs did not end within the shutdown bound");
        }
    }

    /// Resolves the call to a verified program, in the order: installed,
    /// selected, declared, exposed, enabled, root verified.
    async fn resolve(
        &self,
        params: &CommandRunParams,
        transport: ControlTransport,
    ) -> Result<Resolved, ProtocolError> {
        let store = self
            .inner
            .host
            .store
            .clone()
            .ok_or_else(|| ProtocolError::from(CommandErrorKind::PackageNotInstalled))?;
        let package = params.package.clone();
        let name = params.command.clone();
        tokio::task::spawn_blocking(move || resolve_blocking(&store, &package, &name, transport))
            .await
            .map_err(|_join| ProtocolError::from(CommandErrorKind::Unavailable))?
            .map_err(ProtocolError::from)
    }

    /// Reserves the per-command and the host-wide slot, or fails `busy`.
    fn admit(&self, params: &CommandRunParams) -> Result<Admission, ProtocolError> {
        let busy = || ProtocolError::from(CommandErrorKind::Busy);
        let key = (params.package.clone(), params.command.clone());
        let slot = Arc::clone(&self.inner.slots)
            .try_acquire_owned()
            .map_err(|_error| busy())?;
        let mut running = self
            .inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !running.insert(key.clone()) {
            return Err(busy());
        }
        drop(running);
        Ok(Admission {
            inner: Arc::clone(&self.inner),
            key,
            _slot: slot,
        })
    }

    /// Logs a failed run and returns its wire error.
    fn finish(
        resolved: &Resolved,
        transport: ControlTransport,
        started: Instant,
        kind: CommandErrorKind,
        exit: Option<ProcessExit>,
        bytes: usize,
    ) -> ProtocolError {
        Self::log(
            resolved,
            transport,
            started.elapsed(),
            kind.code(),
            exit,
            bytes,
        );
        match (kind, exit) {
            (CommandErrorKind::Failed, Some(exit)) => ProtocolError::command_failed(exit),
            _ => ProtocolError::from(kind),
        }
    }

    /// One INFO line per run; it names the package and the outcome, never the
    /// program's output.
    fn log(
        resolved: &Resolved,
        transport: ControlTransport,
        elapsed: Duration,
        outcome: &str,
        exit: Option<ProcessExit>,
        output_bytes: usize,
    ) {
        info!(
            package = %resolved.identity.id,
            version = %resolved.identity.version,
            digest = %resolved.digest,
            command = %resolved.command.name,
            transport = ?transport,
            duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            outcome,
            exit_code = exit.and_then(|exit| exit.exit_code),
            signal = exit.and_then(|exit| exit.signal),
            output_bytes,
            "package command run"
        );
    }
}

/// `value` lowered to `ceiling`; absent means the ceiling and zero is refused.
fn clamp_ms(value: Option<u32>, ceiling: u32) -> Result<u32, ProtocolError> {
    match value {
        None => Ok(ceiling),
        Some(0) => Err(ProtocolError::bad_request(
            "timeout_ms and max_output_bytes must be greater than zero",
        )),
        Some(value) => Ok(value.min(ceiling)),
    }
}

/// The registry-and-disk half of a call, run off the async threads.
fn resolve_blocking(
    store: &PackageStore,
    package: &PackageId,
    name: &CommandName,
    transport: ControlTransport,
) -> Result<Resolved, CommandErrorKind> {
    let state = store
        .registry_state()
        .map_err(|_error| CommandErrorKind::Unavailable)?;
    if !state
        .packages()
        .iter()
        .any(|record| &record.identity().id == package)
    {
        return Err(CommandErrorKind::PackageNotInstalled);
    }
    let digest = state
        .selected(package)
        .ok_or(CommandErrorKind::NotSelected)?
        .clone();
    let record = state
        .package(&digest)
        .ok_or(CommandErrorKind::Unavailable)?;
    let identity = record.identity().clone();
    let commands = store
        .commands(&digest, &identity)
        .map_err(|_rejection| CommandErrorKind::Unavailable)?;
    let command = commands
        .iter()
        .find(|command| &command.name == name)
        .ok_or(CommandErrorKind::NotDeclared)?
        .clone();
    // The transport of the connection decides; an exposure this build does not
    // know counts as `local`.
    // Exhaustive, so a transport added later must decide its exposure here.
    match transport {
        ControlTransport::Local => {}
        ControlTransport::Remote if command.expose == CommandExposure::Peers => {}
        ControlTransport::Remote => return Err(CommandErrorKind::NotExposed),
    }
    if !record.enabled() {
        return Err(CommandErrorKind::PackageDisabled);
    }
    let files = store
        .verified_files_path(&digest)
        .map_err(|_rejection| CommandErrorKind::Unavailable)?;
    let program = files.join(&command.program);
    Ok(Resolved {
        identity,
        digest,
        command,
        program,
    })
}

/// Maps the runner's outcome to the reply text or the error kind, the way the
/// process ended and the number of output bytes.
fn classify(outcome: RunOutcome) -> (Result<String, CommandErrorKind>, Option<ProcessExit>, usize) {
    match outcome {
        RunOutcome::Exited(output) => {
            let status = output.status();
            if status.success() {
                let bytes = output.stdout_len();
                return match output.stdout_utf8() {
                    Ok(text) => (Ok(text.to_owned()), None, bytes),
                    Err(_error) => (Err(CommandErrorKind::OutputNotUtf8), None, bytes),
                };
            }
            let exit = ProcessExit {
                exit_code: status.code(),
                signal: status.signal(),
            };
            (Err(CommandErrorKind::Failed), Some(exit), 0)
        }
        RunOutcome::TimedOut => (Err(CommandErrorKind::TimedOut), None, 0),
        RunOutcome::Overflow => (Err(CommandErrorKind::OutputTooLarge), None, 0),
        RunOutcome::Cancelled | RunOutcome::Unavailable => {
            (Err(CommandErrorKind::Unavailable), None, 0)
        }
    }
}
