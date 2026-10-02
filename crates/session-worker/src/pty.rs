//! Owns one PTY master and its managed process identity.

// Rust guideline compliant 2026-09-24

use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use pohunek_platform::process::{HostInspector, ProcessInspector};
use pohunek_worker_protocol::is_denylisted;
use portable_pty::{native_pty_system, Child as PtyChild, CommandBuilder, MasterPty, PtySize};
use rustix::event::{poll, PollFd, PollFlags};
use rustix::fd::{BorrowedFd, OwnedFd};
use rustix::fs::{open, Mode, OFlags};
use rustix::process::{waitid, Pid as RustixPid, WaitId, WaitIdOptions, WaitIdStatus};
use rustix::termios::{tcflow, Action};
use tokio::sync::{watch, Mutex as AsyncMutex};
use tracing::{event, Level};

use crate::output::OutputCompletion;
use crate::{OutputEvent, OutputHub, OutputSubscriber, RingError, WriteCoordinator};

/// Blocking PTY read size.
///
/// Eight KiB bounds temporary allocations while keeping terminal repaint
/// throughput efficient.
const READ_CHUNK_BYTES: usize = 8 * 1024;
/// Maximum output drained per turn while holding the snapshot ordering gate.
///
/// A quarter MiB amortizes lock traffic while making every gate hold finite
/// under an unbounded producer such as `yes`. This bounds one holder's work,
/// but does not guarantee which waiting operation acquires the gate next.
const OUTPUT_DRAIN_BATCH_BYTES: usize = 256 * 1024;
/// Longest wait for terminal-held output to reach the master at a boundary.
///
/// Released bytes are readable as soon as output flows again, so this only
/// absorbs scheduling delay. It bounds how long a resize or snapshot holds the
/// ordering gate before failing with a retryable error instead of crossing the
/// boundary with old-geometry bytes still queued.
const HELD_OUTPUT_SETTLE_TIMEOUT: Duration = Duration::from_secs(1);
/// How often a quiet master re-checks whether the terminal discarded held
/// output, which ends the boundary wait without reading those bytes.
const HELD_OUTPUT_RECHECK_INTERVAL: Duration = Duration::from_millis(10);
/// Blocking and non-blocking readiness timeouts.
///
/// Negative one waits indefinitely and zero checks without blocking, which is
/// the convention `poll(2)` uses directly.
const WAIT_FOREVER_MS: isize = -1;
const NO_WAIT_MS: isize = 0;
/// Position of the PTY master in the readiness set.
const MASTER_SLOT: usize = 0;
/// Position of the cancellation pipe in the readiness set.
const CANCEL_SLOT: usize = 1;
/// Maximum terminal grid cells accepted from one initialization.
///
/// Four million cells accommodate unusually large terminals while preventing
/// a local malformed request from forcing multi-gigabyte VT allocations.
const MAX_TERMINAL_CELLS: u64 = 4_000_000;

/// Selects the environment a PTY child starts from before [`Command::env`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvBase {
    /// No inherited variables: the child sees only [`Command::env`].
    ///
    /// `portable-pty` always sets `SHELL`; when `env` has none, it uses the
    /// account's login shell from the password database.
    Empty,
    /// The worker's own environment without `POHUNEK_*` markers and without
    /// service-manager variables, for sessions whose daemon predates the
    /// base-environment contract.
    Inherited,
}

/// Command launched inside one PTY.
#[derive(Clone, PartialEq, Eq)]
pub struct Command {
    /// Resolved executable.
    pub program: String,
    /// Executable arguments.
    pub args: Vec<String>,
    /// Environment the child starts from.
    pub base: EnvBase,
    /// Child environment additions or overrides.
    pub env: Vec<(String, String)>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Initial terminal columns.
    pub cols: u16,
    /// Initial terminal rows.
    pub rows: u16,
}

impl Debug for Command {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("program", &"[REDACTED]")
            .field("argument_count", &self.args.len())
            .field("base", &self.base)
            .field(
                "env",
                &format_args!("[REDACTED; {} entries]", self.env.len()),
            )
            .field("cwd", &"[REDACTED]")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .finish()
    }
}

/// Retained process identity protected against numeric PID reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    /// Root child PID.
    pub pid: u32,
    /// PTY process-group leader.
    pub process_group: i32,
    /// Opaque same-boot start identity reported by the platform inspector.
    pub start_identity: String,
}

/// Managed child exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    /// Exit code when termination was not signaled.
    pub exit_code: Option<i32>,
    /// Signal name when available.
    pub signal: Option<String>,
    /// Whether the process succeeded.
    pub success: bool,
}

/// PTY setup or operation failure.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    /// PTY dimensions are invalid.
    #[error("PTY dimensions must be nonzero, got {cols}x{rows}")]
    InvalidDimensions {
        /// Invalid columns.
        cols: u16,
        /// Invalid rows.
        rows: u16,
    },
    /// A terminal grid would consume unreasonable memory.
    #[error("PTY dimensions {cols}x{rows} exceed the safe grid limit")]
    DimensionsTooLarge {
        /// Rejected columns.
        cols: u16,
        /// Rejected rows.
        rows: u16,
    },
    /// Opening the PTY failed.
    #[error("failed to allocate PTY: {0}")]
    Allocate(String),
    /// Spawning the child failed.
    #[error("failed to spawn PTY command: {message}")]
    Spawn {
        /// Redacted upstream diagnostic.
        message: String,
        /// Whether an absolute executable disappeared.
        not_found: bool,
    },
    /// The child has no process identifier.
    #[error("PTY child did not expose a process id")]
    MissingPid,
    /// The PTY implementation did not expose a pollable master descriptor.
    #[error("PTY master did not expose a pollable file descriptor")]
    MissingMasterFd,
    /// The PTY implementation did not expose its slave device path.
    #[error("PTY master did not expose a terminal device path")]
    MissingTtyName,
    /// The child's process start identity was unavailable.
    #[error("failed to read process start identity for pid {pid}: {source}")]
    ProcessIdentity {
        /// Process PID.
        pid: u32,
        /// Underlying procfs failure.
        source: io::Error,
    },
    /// Output actor failed.
    #[error(transparent)]
    Output(#[from] RingError),
    /// PTY I/O failed.
    #[error("PTY I/O failed: {0}")]
    Io(#[from] io::Error),
    /// A retained PTY lock was poisoned.
    #[error("PTY handle lock was poisoned")]
    Poisoned,
    /// A blocking PTY task terminated unexpectedly.
    #[error("PTY blocking task terminated unexpectedly")]
    Task,
    /// Queued output exceeds the atomic resize or snapshot drain limit.
    #[error("queued PTY output exceeds the atomic resize or snapshot drain limit")]
    OutputDrainLimit,
    /// Output the terminal held back did not reach the resize or snapshot
    /// boundary in time; the operation changed nothing and can be retried.
    #[error("PTY output held by the terminal did not reach the resize or snapshot boundary")]
    OutputBoundaryUnsettled,
    /// Process identity changed before a signal.
    #[error("managed process identity no longer matches pid {pid}")]
    IdentityChanged {
        /// Reused or changed PID.
        pid: u32,
    },
    /// Process did not exit within both stop windows.
    #[error("timed out waiting for managed process exit")]
    ExitTimeout,
    /// PTY output did not close after hard process-group termination.
    #[error("PTY output remained open after hard process-group termination")]
    OutputCloseTimeout,
    /// PTY output was force-closed after its bounded cleanup deadline.
    #[error("PTY output was force-closed after its cleanup deadline")]
    OutputForcedClosed,
    /// The root process could not be observed without releasing its PID anchor.
    #[error("failed to observe the managed root process without reaping it: {message}")]
    ChildObservation {
        /// Sanitized operating-system failure.
        message: String,
    },
    /// Sending a process-group signal failed.
    #[error("failed to signal managed process group {process_group}: {source}")]
    Signal {
        /// Process group leader.
        process_group: i32,
        /// Underlying signal error.
        source: nix::errno::Errno,
    },
}

/// Cloneable handle to one worker-owned PTY runtime.
#[derive(Clone)]
pub struct PtyOwner {
    identity: ProcessIdentity,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    reader_thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    child_thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    exit_rx: watch::Receiver<Option<RootObservation>>,
    output: OutputHub,
    output_order: Arc<Mutex<()>>,
    output_reader: Arc<Mutex<Box<dyn Read + Send>>>,
    output_readiness: Arc<OutputReadiness>,
    tty_name: Arc<PathBuf>,
    input: WriteCoordinator,
    resize: Arc<AsyncMutex<ResizeState>>,
    cleanup: Arc<AsyncMutex<CleanupState>>,
    reap_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CleanupState {
    Active,
    AuthorityReleased,
    ForcedClosing,
    OutputAuthorityReleased(OutputCompletion),
    ForcedClosed,
    ObservationAuthorityReleased(String),
    ObservationFailed(String),
    Finished(Exit),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootObservation {
    Exited(Exit),
    Failed(String),
}

/// Owns a spawned child until construction commits it to the wait thread.
struct SpawnGuard {
    child: Option<Box<dyn PtyChild + Send + Sync>>,
    process_group: Option<i32>,
}

/// Holds startup threads until every fallible ownership handoff succeeds.
#[derive(Debug, Clone, Default)]
struct StartupLatch {
    inner: Arc<(Mutex<StartupState>, Condvar)>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum StartupState {
    #[default]
    Pending,
    Committed,
    Aborted,
}

impl StartupLatch {
    fn wait(&self) -> bool {
        let (state, changed) = &*self.inner;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *state == StartupState::Pending {
            state = changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *state == StartupState::Committed
    }

    fn commit(&self) {
        self.set(StartupState::Committed);
    }

    fn abort(&self) {
        self.set(StartupState::Aborted);
    }

    fn set(&self, next: StartupState) {
        let (state, changed) = &*self.inner;
        *state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        changed.notify_all();
    }
}

impl Debug for SpawnGuard {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnGuard")
            .field("has_child", &self.child.is_some())
            .field("process_group", &self.process_group)
            .finish()
    }
}

impl SpawnGuard {
    fn new(child: Box<dyn PtyChild + Send + Sync>) -> Self {
        Self {
            child: Some(child),
            process_group: None,
        }
    }

    fn set_process_group(&mut self, process_group: i32) {
        self.process_group = Some(process_group);
    }

    fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
        let child = self.child.as_mut().expect("spawn guard owns child");
        let result = child.wait();
        if result.is_ok() {
            self.child = None;
        }
        result
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Some(process_group) = self.process_group {
            let _ = killpg(Pid::from_raw(process_group), Signal::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Debug for PtyOwner {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyOwner")
            .field("identity", &self.identity)
            .field("next_output_offset", &self.output.next_offset())
            .finish_non_exhaustive()
    }
}

impl PtyOwner {
    /// Spawns a real command into a worker-owned PTY.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError`] when PTY allocation, spawn, or process identity
    /// capture fails.
    #[expect(
        clippy::too_many_lines,
        reason = "PTY allocation and thread handoff form one failure-atomic construction transaction"
    )]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "one-shot launch ownership ensures secret environment storage drops after construction"
    )]
    pub fn spawn(
        command: Command,
        history_bytes: usize,
        subscriber_bytes: usize,
        input_dedup_entries: usize,
    ) -> Result<Self, PtyError> {
        if command.cols == 0 || command.rows == 0 {
            return Err(PtyError::InvalidDimensions {
                cols: command.cols,
                rows: command.rows,
            });
        }
        if u64::from(command.cols) * u64::from(command.rows) > MAX_TERMINAL_CELLS {
            return Err(PtyError::DimensionsTooLarge {
                cols: command.cols,
                rows: command.rows,
            });
        }
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: command.rows,
                cols: command.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|source| PtyError::Allocate(source.to_string()))?;

        let mut builder = CommandBuilder::new(&command.program);
        builder.args(&command.args);
        // `CommandBuilder::new` captures this worker process's own environment,
        // which under a service manager describes the worker's supervision.
        match command.base {
            EnvBase::Empty => builder.env_clear(),
            // Strip every ambient `POHUNEK_*` marker so the child carries only
            // the worker-authoritative identity from `command.env` (RFC §11.5
            // environment sanitization; §17.5 removes daemon id from ownership).
            // Otherwise a daemon running inside another pohunek session leaks
            // that ancestor's `POHUNEK_DAEMON_ID`, and procwatch mis-attributes
            // the agent as foreign-owned. `vars_os` tolerates non-UTF-8 names.
            EnvBase::Inherited => {
                for name in std::env::vars_os() {
                    if let Some(name) = name.0.to_str() {
                        if name.starts_with("POHUNEK_") || is_denylisted(name) {
                            builder.env_remove(name);
                        }
                    }
                }
            }
        }
        for (key, value) in &command.env {
            builder.env(key, value);
        }
        builder.cwd(&command.cwd);

        let tty_name = Arc::new(pair.master.tty_name().ok_or(PtyError::MissingTtyName)?);
        // Every master descriptor below shares this file description, so the
        // output reader and the input writer both observe non-blocking mode.
        // See `drain_available_output` for why a read must never park.
        rustix::io::ioctl_fionbio(borrow_master(&*pair.master)?, true)
            .map_err(rustix_errno_to_pty_error)?;
        let output_readiness = Arc::new(OutputReadiness::new(borrow_master(&*pair.master)?)?);
        let output_reader = Arc::new(Mutex::new(
            pair.master
                .try_clone_reader()
                .map_err(|source| PtyError::Allocate(source.to_string()))?,
        ));
        let writer = MasterWriter {
            inner: pair
                .master
                .take_writer()
                .map_err(|source| PtyError::Allocate(source.to_string()))?,
            master: borrow_master(&*pair.master)?
                .try_clone_to_owned()
                .map_err(PtyError::Io)?,
        };
        let input = WriteCoordinator::new(writer, input_dedup_entries)
            .map_err(|source| PtyError::Io(io::Error::other(source)))?;
        let output = OutputHub::new(history_bytes, subscriber_bytes, command.rows, command.cols)?;
        let output_order = Arc::new(Mutex::new(()));

        let child = pair.slave.spawn_command(builder).map_err(|_source| {
            let program = Path::new(&command.program);
            PtyError::Spawn {
                message: "process launch failed".to_owned(),
                not_found: program.is_absolute() && !program.exists(),
            }
        })?;
        let mut child = SpawnGuard::new(child);
        let pid = child
            .child
            .as_ref()
            .and_then(|child| child.process_id())
            .ok_or(PtyError::MissingPid)?;
        let native_pid = i32::try_from(pid).map_err(|_range_error| PtyError::ProcessIdentity {
            pid,
            source: io::Error::new(io::ErrorKind::InvalidData, "PID exceeds pid_t range"),
        })?;
        child.set_process_group(native_pid);
        // portable-pty creates the PTY root as a session and process-group
        // leader. Unlike the terminal foreground group, this owned PGID stays
        // bound to the unreaped root throughout construction rollback.
        let process_group = native_pid;
        let start_identity =
            read_process_start(pid).map_err(|source| PtyError::ProcessIdentity { pid, source })?;
        let identity = ProcessIdentity {
            pid,
            process_group,
            start_identity,
        };
        drop(pair.slave);

        let reader_output = output.clone();
        let reader_output_order = Arc::clone(&output_order);
        let thread_output_reader = Arc::clone(&output_reader);
        let thread_output_readiness = Arc::clone(&output_readiness);
        let reader_pid = pid;
        let startup = StartupLatch::default();
        let reader_startup = startup.clone();
        let reader_thread = thread::Builder::new()
            .name(format!("pohunek-worker-pty-{pid}"))
            .spawn(move || {
                if !reader_startup.wait() {
                    reader_output.mark_exit();
                    return;
                }
                let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
                loop {
                    match wait_for_output(&*thread_output_readiness) {
                        Ok(OutputReadyState::Output) => {}
                        Ok(OutputReadyState::Cancelled) => break,
                        Err(error) => {
                            event!(
                                name: "worker.pty.read.failed",
                                Level::WARN,
                                process.pid = reader_pid,
                                error.type = "io",
                                error.message = %error,
                                "PTY readiness wait failed for {{process.pid}}: {{error.message}}",
                            );
                            break;
                        }
                    }
                    let _order = reader_output_order
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match drain_available_output(
                        &thread_output_reader,
                        &*thread_output_readiness,
                        &reader_output,
                        &mut buffer,
                        OUTPUT_DRAIN_BATCH_BYTES,
                    ) {
                        Ok(
                            OutputReadState::Open
                            | OutputReadState::Lapsed
                            | OutputReadState::BudgetExhausted,
                        ) => {}
                        Ok(OutputReadState::Eof | OutputReadState::Cancelled) => break,
                        Err(error) => {
                            event!(
                                name: "worker.pty.read.failed",
                                Level::WARN,
                                process.pid = reader_pid,
                                error.type = "io",
                                error.message = %error,
                                "PTY read failed for {{process.pid}}: {{error.message}}",
                            );
                            break;
                        }
                    }
                }
                reader_output.mark_exit();
            })
            .map_err(PtyError::Io)?;

        let (exit_tx, exit_rx) = watch::channel(None);
        let (reap_tx, reap_rx) = mpsc::channel();
        let child_startup = startup.clone();
        let child_thread = thread::Builder::new()
            .name(format!("pohunek-worker-child-{pid}"))
            .spawn(move || {
                if !child_startup.wait() {
                    return;
                }
                let observation = match observe_child_exit(pid) {
                    Ok(exit) => RootObservation::Exited(exit),
                    Err(error) => {
                        event!(
                            name: "worker.child.observe.failed",
                            Level::ERROR,
                            process.pid = pid,
                            error.type = "io",
                            error.message = %error,
                            "child non-reaping wait failed for {{process.pid}}: {{error.message}}",
                        );
                        RootObservation::Failed(error.to_string())
                    }
                };
                let _ = exit_tx.send(Some(observation));
                let _ = reap_rx.recv();
                let _ = reap_child(&mut child, pid);
            });
        let child_thread = match child_thread {
            Ok(child_thread) => child_thread,
            Err(source) => {
                startup.abort();
                reader_thread.join().map_err(|_panic| PtyError::Task)?;
                return Err(PtyError::Io(source));
            }
        };
        startup.commit();

        Ok(Self {
            identity,
            master: Arc::new(Mutex::new(pair.master)),
            reader_thread: Arc::new(Mutex::new(Some(reader_thread))),
            child_thread: Arc::new(Mutex::new(Some(child_thread))),
            exit_rx,
            output,
            output_order,
            output_reader,
            output_readiness,
            tty_name,
            input,
            resize: Arc::new(AsyncMutex::new(ResizeState {
                cols: command.cols,
                rows: command.rows,
                sequences: HashMap::new(),
            })),
            cleanup: Arc::new(AsyncMutex::new(CleanupState::Active)),
            reap_tx: Arc::new(Mutex::new(Some(reap_tx))),
        })
    }

    /// Returns the retained root and process-group identity.
    #[must_use]
    pub fn identity(&self) -> &ProcessIdentity {
        &self.identity
    }

    /// Returns the ordered input coordinator.
    #[must_use]
    pub fn input(&self) -> &WriteCoordinator {
        &self.input
    }

    /// Returns the output actor.
    #[must_use]
    pub fn output(&self) -> &OutputHub {
        &self.output
    }

    /// Atomically subscribes to retained and live output.
    ///
    /// # Errors
    ///
    /// Returns [`RingError`] when a requested offset is in the future.
    pub fn subscribe_output(
        &self,
        after_offset: Option<u64>,
    ) -> Result<OutputSubscriber, RingError> {
        self.output.subscribe(after_offset)
    }

    /// Applies a monotonic source-specific resize.
    ///
    /// Returns `false` for duplicate or older source sequences.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError`] for invalid dimensions, queued-output limits, or
    /// PTY I/O failure.
    pub async fn resize(
        &self,
        source_id: &str,
        source_sequence: u64,
        cols: u16,
        rows: u16,
    ) -> Result<bool, PtyError> {
        if cols == 0 || rows == 0 {
            return Err(PtyError::InvalidDimensions { cols, rows });
        }
        let mut resize = self.resize.lock().await;
        if resize
            .sequences
            .get(source_id)
            .is_some_and(|previous| *previous >= source_sequence)
        {
            return Ok(false);
        }
        let master = Arc::clone(&self.master);
        let output = self.output.clone();
        let output_order = Arc::clone(&self.output_order);
        let output_reader = Arc::clone(&self.output_reader);
        let output_readiness = Arc::clone(&self.output_readiness);
        let tty_name = Arc::clone(&self.tty_name);
        let output_pause = tokio::task::spawn_blocking(move || {
            // Stop the producer before contending for the ordering gate. This
            // makes the remaining queue finite even when the reader repeatedly
            // drains batches from an otherwise unbounded producer.
            let output_pause = OutputPause::new(&tty_name)?;
            let _order = output_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
            drain_snapshot_boundary(
                &output_reader,
                &*output_readiness,
                &output_pause,
                &output,
                &mut buffer,
                SnapshotDrain {
                    byte_budget: OUTPUT_DRAIN_BATCH_BYTES,
                    settle_timeout: HELD_OUTPUT_SETTLE_TIMEOUT,
                },
            )?;
            let master = master.lock().map_err(|_poison| PtyError::Poisoned)?;
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|source| PtyError::Io(io::Error::other(source)))?;
            output.resize(rows, cols);
            Ok::<OutputPause, PtyError>(output_pause)
        })
        .await
        .map_err(|_join_error| PtyError::Task)??;
        commit_resize_before_resume(
            &mut resize,
            ResizeCommit::Resize {
                source_id,
                source_sequence,
                cols,
                rows,
            },
            || output_pause.resume(),
        )?;
        Ok(true)
    }

    /// Applies the attach's initial dimensions and atomically registers a
    /// snapshot-first output subscriber.
    ///
    /// Output parsing, terminal-model resize, snapshot capture, and subscriber
    /// registration share one ordering gate. Bytes already readable when the
    /// gate is acquired are drained into the snapshot; later bytes are delivered
    /// live from the snapshot watermark.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError`] when dimensions are invalid, the PTY resize fails,
    /// or the blocking operation cannot complete.
    pub async fn attach_snapshot(
        &self,
        dimensions: Option<(u16, u16)>,
    ) -> Result<(OutputSubscriber, (u16, u16)), PtyError> {
        if let Some((cols, rows)) = dimensions {
            if cols == 0 || rows == 0 {
                return Err(PtyError::InvalidDimensions { cols, rows });
            }
        }

        let mut resize = self.resize.lock().await;
        let master = Arc::clone(&self.master);
        let output = self.output.clone();
        let output_order = Arc::clone(&self.output_order);
        let output_reader = Arc::clone(&self.output_reader);
        let output_readiness = Arc::clone(&self.output_readiness);
        let tty_name = Arc::clone(&self.tty_name);
        let (output_pause, subscriber) = tokio::task::spawn_blocking(move || {
            // Stop the producer before contending for the ordering gate. This
            // makes the remaining queue finite even when the reader repeatedly
            // drains batches from an otherwise unbounded producer.
            let output_pause = OutputPause::new(&tty_name)?;
            let _order = output_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
            drain_snapshot_boundary(
                &output_reader,
                &*output_readiness,
                &output_pause,
                &output,
                &mut buffer,
                SnapshotDrain {
                    byte_budget: OUTPUT_DRAIN_BATCH_BYTES,
                    settle_timeout: HELD_OUTPUT_SETTLE_TIMEOUT,
                },
            )?;
            if let Some((cols, rows)) = dimensions {
                let master = master.lock().map_err(|_poison| PtyError::Poisoned)?;
                master
                    .resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .map_err(|source| PtyError::Io(io::Error::other(source)))?;
                output.resize(rows, cols);
            }
            let subscriber = output.subscribe_terminal_snapshot();
            Ok::<(OutputPause, OutputSubscriber), PtyError>((output_pause, subscriber))
        })
        .await
        .map_err(|_join_error| PtyError::Task)??;

        if let Some((cols, rows)) = dimensions {
            commit_resize_before_resume(&mut resize, ResizeCommit::Attach { cols, rows }, || {
                output_pause.resume()
            })?;
        } else {
            output_pause.resume()?;
        }
        Ok((subscriber, (resize.cols, resize.rows)))
    }

    /// Returns the last successfully applied PTY dimensions.
    pub async fn dimensions(&self) -> (u16, u16) {
        let resize = self.resize.lock().await;
        (resize.cols, resize.rows)
    }

    /// Idempotently stops the retained process group.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError`] for signal, identity, timeout, or join failures.
    pub async fn stop(&self, _stop_id: &str, grace: Duration) -> Result<Exit, PtyError> {
        let mut cleanup = self.cleanup.lock().await;
        match &*cleanup {
            CleanupState::Finished(exit) => return Ok(exit.clone()),
            CleanupState::ForcedClosed => return Err(PtyError::OutputForcedClosed),
            CleanupState::ForcedClosing => {
                let completion = self.seal_and_release_output(&mut cleanup)?;
                return self.finish_sealed_cleanup(&mut cleanup, completion).await;
            }
            CleanupState::OutputAuthorityReleased(completion) => {
                let completion = *completion;
                return self.finish_sealed_cleanup(&mut cleanup, completion).await;
            }
            CleanupState::ObservationAuthorityReleased(message) => {
                let message = message.clone();
                return self.finish_observation_failure(&mut cleanup, message);
            }
            CleanupState::ObservationFailed(message) => {
                return Err(PtyError::ChildObservation {
                    message: message.clone(),
                });
            }
            CleanupState::AuthorityReleased => {
                let exit = self.wait_exit().await?;
                self.join_threads().await?;
                *cleanup = CleanupState::Finished(exit.clone());
                return Ok(exit);
            }
            CleanupState::Active => {}
        }

        self.signal_group(Signal::SIGTERM)?;
        let graceful = tokio::time::timeout(grace, self.wait_root_and_output()).await;
        let exit = match graceful {
            Ok(Err(PtyError::ChildObservation { message })) => {
                self.signal_group(Signal::SIGKILL)?;
                self.force_output_close()?;
                self.release_authority(
                    &mut cleanup,
                    CleanupState::ObservationAuthorityReleased(message.clone()),
                )?;
                return self.finish_observation_failure(&mut cleanup, message);
            }
            Ok(result) => result?,
            Err(_elapsed) => {
                self.signal_group(Signal::SIGKILL)?;
                match tokio::time::timeout(grace, self.wait_root_and_output()).await {
                    Ok(Err(PtyError::ChildObservation { message })) => {
                        self.force_output_close()?;
                        self.release_authority(
                            &mut cleanup,
                            CleanupState::ObservationAuthorityReleased(message.clone()),
                        )?;
                        return self.finish_observation_failure(&mut cleanup, message);
                    }
                    Ok(result) => result?,
                    Err(_elapsed)
                        if matches!(&*self.exit_rx.borrow(), Some(RootObservation::Exited(_))) =>
                    {
                        *cleanup = CleanupState::ForcedClosing;
                        let completion = self.seal_and_release_output(&mut cleanup)?;
                        return self.finish_sealed_cleanup(&mut cleanup, completion).await;
                    }
                    Err(_elapsed) => return Err(PtyError::ExitTimeout),
                }
            }
        };
        self.release_authority(&mut cleanup, CleanupState::AuthorityReleased)?;
        self.join_threads().await?;
        *cleanup = CleanupState::Finished(exit.clone());
        Ok(exit)
    }

    /// Releases the process-group authority after a natural PTY EOF.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError`] when the retained child threads cannot be joined.
    pub(crate) async fn finish_natural(&self) -> Result<Exit, PtyError> {
        let mut cleanup = self.cleanup.lock().await;
        match &*cleanup {
            CleanupState::Finished(exit) => return Ok(exit.clone()),
            CleanupState::ForcedClosed => return Err(PtyError::OutputForcedClosed),
            CleanupState::ForcedClosing => {
                let completion = self.seal_and_release_output(&mut cleanup)?;
                return self.finish_sealed_cleanup(&mut cleanup, completion).await;
            }
            CleanupState::OutputAuthorityReleased(completion) => {
                let completion = *completion;
                return self.finish_sealed_cleanup(&mut cleanup, completion).await;
            }
            CleanupState::ObservationAuthorityReleased(message)
            | CleanupState::ObservationFailed(message) => {
                return Err(PtyError::ChildObservation {
                    message: message.clone(),
                });
            }
            CleanupState::AuthorityReleased => {}
            CleanupState::Active => {
                self.release_authority(&mut cleanup, CleanupState::AuthorityReleased)?;
            }
        }
        let exit = self.wait_exit().await?;
        self.join_threads().await?;
        *cleanup = CleanupState::Finished(exit.clone());
        Ok(exit)
    }

    /// Waits for natural child exit.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::ExitTimeout`] if the exit channel closes.
    pub async fn wait_exit(&self) -> Result<Exit, PtyError> {
        let mut receiver = self.exit_rx.clone();
        loop {
            if let Some(observation) = receiver.borrow().clone() {
                return match observation {
                    RootObservation::Exited(exit) => Ok(exit),
                    RootObservation::Failed(message) => Err(PtyError::ChildObservation { message }),
                };
            }
            receiver
                .changed()
                .await
                .map_err(|_channel_closed| PtyError::ExitTimeout)?;
        }
    }

    /// Subscribes to the retained root-process outcome.
    pub(crate) fn exit_receiver(&self) -> watch::Receiver<Option<RootObservation>> {
        self.exit_rx.clone()
    }

    /// Returns whether the reader was closed by the lifecycle deadline.
    pub(crate) fn output_forced_closed(&self) -> bool {
        matches!(
            self.output.completion(),
            Some(OutputCompletion::ForcedClosed { .. })
        )
    }

    /// Returns the immutable PTY output completion once sealed.
    pub(crate) fn output_completion(&self) -> Option<OutputCompletion> {
        self.output.completion()
    }

    async fn wait_root_and_output(&self) -> Result<Exit, PtyError> {
        let (exit, ()) = tokio::try_join!(self.wait_exit(), self.wait_output_exit())?;
        Ok(exit)
    }

    async fn wait_output_exit(&self) -> Result<(), PtyError> {
        let mut subscriber = self.output.subscribe(None)?;
        loop {
            match subscriber.recv().await {
                Some(OutputEvent::Exit { .. }) => return Ok(()),
                Some(
                    OutputEvent::Replay(_)
                    | OutputEvent::Output(_)
                    | OutputEvent::Gap { .. }
                    | OutputEvent::TerminalSnapshot(_),
                ) => {}
                None => return Err(PtyError::OutputCloseTimeout),
            }
        }
    }

    fn force_output_close(&self) -> Result<OutputCompletion, PtyError> {
        let completion = self.output.force_close();
        self.output_readiness.cancel()?;
        Ok(completion)
    }

    fn seal_and_release_output(
        &self,
        cleanup: &mut CleanupState,
    ) -> Result<OutputCompletion, PtyError> {
        let completion = self.force_output_close()?;
        self.release_authority(cleanup, CleanupState::OutputAuthorityReleased(completion))?;
        Ok(completion)
    }

    async fn finish_sealed_cleanup(
        &self,
        cleanup: &mut CleanupState,
        completion: OutputCompletion,
    ) -> Result<Exit, PtyError> {
        match completion {
            OutputCompletion::Eof { .. } => {
                let exit = self.wait_exit().await?;
                self.reap_and_detach_reader().await?;
                *cleanup = CleanupState::Finished(exit.clone());
                Ok(exit)
            }
            OutputCompletion::ForcedClosed { .. } => {
                self.reap_and_detach_reader().await?;
                *cleanup = CleanupState::ForcedClosed;
                Err(PtyError::OutputForcedClosed)
            }
        }
    }

    fn finish_observation_failure(
        &self,
        cleanup: &mut CleanupState,
        message: String,
    ) -> Result<Exit, PtyError> {
        self.detach_threads()?;
        *cleanup = CleanupState::ObservationFailed(message.clone());
        Err(PtyError::ChildObservation { message })
    }

    /// Waits for the root to be reaped, then lets the reader go.
    ///
    /// Sealed cleanup starts only after the root was observed exiting and the
    /// process-group authority was released, so the reaper has nothing left but
    /// collecting an exited child and the join is bounded. It makes a returned
    /// cleanup mean a reaped root. The reader may still be serving a PTY that a
    /// process outside the group holds open, so it is not waited for.
    async fn reap_and_detach_reader(&self) -> Result<(), PtyError> {
        join_thread(&self.child_thread).await?;
        drop(lock_result(&self.reader_thread)?.take());
        Ok(())
    }

    fn detach_threads(&self) -> Result<(), PtyError> {
        drop(lock_result(&self.reader_thread)?.take());
        drop(lock_result(&self.child_thread)?.take());
        Ok(())
    }

    fn release_authority(
        &self,
        cleanup: &mut CleanupState,
        released: CleanupState,
    ) -> Result<(), PtyError> {
        let sender = lock_result(&self.reap_tx)?.take();
        *cleanup = released;
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        Ok(())
    }

    fn signal_group(&self, signal: Signal) -> Result<(), PtyError> {
        match read_process_start(self.identity.pid) {
            Ok(start) if start == self.identity.start_identity => {}
            Ok(_) => {
                return Err(PtyError::IdentityChanged {
                    pid: self.identity.pid,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(PtyError::ProcessIdentity {
                    pid: self.identity.pid,
                    source,
                });
            }
        }
        match killpg(Pid::from_raw(self.identity.process_group), signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            // XNU leaves zombies out when it signals a group and answers EPERM
            // when it signalled no member (`killpg1` in `bsd/kern/kern_sig.c`),
            // where Linux counts the exited root as signalled and succeeds.
            // Every live member this worker may signal is signalled either
            // way, so once the verified root has exited, EPERM means no such
            // member is left. A member owned by another user stays unsignalled
            // on both kernels; Linux reports that as success too.
            Err(nix::errno::Errno::EPERM)
                if cfg!(target_os = "macos")
                    && !root_is_running(self.identity.pid).map_err(|source| {
                        PtyError::ProcessIdentity {
                            pid: self.identity.pid,
                            source,
                        }
                    })? =>
            {
                Ok(())
            }
            Err(source) => Err(PtyError::Signal {
                process_group: self.identity.process_group,
                source,
            }),
        }
    }

    async fn join_threads(&self) -> Result<(), PtyError> {
        join_thread(&self.child_thread).await?;
        join_thread(&self.reader_thread).await
    }
}

#[derive(Debug)]
struct ResizeState {
    cols: u16,
    rows: u16,
    sequences: HashMap<String, u64>,
}

#[derive(Debug, Clone, Copy)]
enum ResizeCommit<'a> {
    Resize {
        source_id: &'a str,
        source_sequence: u64,
        cols: u16,
        rows: u16,
    },
    Attach {
        cols: u16,
        rows: u16,
    },
}

fn commit_resize_before_resume(
    state: &mut ResizeState,
    commit: ResizeCommit<'_>,
    resume: impl FnOnce() -> Result<(), PtyError>,
) -> Result<(), PtyError> {
    match commit {
        ResizeCommit::Resize {
            source_id,
            source_sequence,
            cols,
            rows,
        } => {
            state
                .sequences
                .insert(source_id.to_owned(), source_sequence);
            state.cols = cols;
            state.rows = rows;
        }
        ResizeCommit::Attach { cols, rows } => {
            state.cols = cols;
            state.rows = rows;
        }
    }
    resume()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputReadState {
    /// Nothing was readable when the drain looked.
    Open,
    /// Readiness was reported, but the read then found nothing.
    ///
    /// Harmless for the reader loop. At a snapshot boundary it can mean the
    /// terminal holds bytes the master cannot read, which
    /// [`drain_snapshot_boundary`] resolves before the boundary is taken.
    Lapsed,
    Eof,
    BudgetExhausted,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputReadyState {
    Output,
    Cancelled,
}

/// Waits for PTY output or for a forced close, whichever comes first.
///
/// Cancellation is a latch: once armed it is never consumed, so every later
/// wait from every holder reports it. The reader thread, `resize` and
/// `attach_snapshot` share one instance, and the latter two learn about a
/// forced close only through it — a signal consumed by the first waiter would
/// let them report success on a PTY whose output is already closed.
#[derive(Debug)]
struct OutputReadiness {
    /// Duplicate of the PTY master, owned so every wait polls a descriptor this
    /// struct itself keeps open.
    master: OwnedFd,
    /// Read end of the cancellation self-pipe, watched alongside the master and
    /// never read, which is what keeps an armed cancellation readable.
    cancel_reader: OwnedFd,
    /// Write end; one byte arms the cancellation for good.
    cancel_writer: OwnedFd,
}

/// Blocking input writer over the non-blocking PTY master description.
///
/// Output reads need the shared description non-blocking. Input keeps
/// blocking semantics by waiting for writability whenever the child's input
/// queue is full.
struct MasterWriter {
    inner: Box<dyn io::Write + Send>,
    /// Duplicate of the master used only to wait for writability.
    master: OwnedFd,
}

impl io::Write for MasterWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.inner.write(buf) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait_writable(&self.master)?;
                }
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Waits until the PTY master accepts input or reports why it cannot.
///
/// # Errors
///
/// Returns an I/O error when `poll` fails or the master reports a hangup or
/// error without writability, so a closed PTY cannot spin its writer.
fn wait_writable(master: &OwnedFd) -> io::Result<()> {
    loop {
        let mut fds = [PollFd::new(master, PollFlags::OUT)];
        match poll(&mut fds, None) {
            Ok(_woken) => {
                let revents = fds[MASTER_SLOT].revents();
                if revents.contains(PollFlags::OUT) {
                    return Ok(());
                }
                if revents.intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL) {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "PTY master cannot accept input",
                    ));
                }
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(io::Error::from(error)),
        }
    }
}

/// Stops PTY output at the terminal until resumed or dropped.
///
/// Snapshot-boundary operations pause before contending for the ordering gate,
/// so the bytes still readable from the master are finite. The reader thread
/// then finishes its current batch without parking in a read and returns to
/// its unlocked readiness wait, which hands the gate to the waiting operation.
#[derive(Debug)]
struct OutputPause {
    tty: OwnedFd,
    resumed: bool,
}

/// Terminal-side view of output that a pause keeps away from the master.
trait HeldOutput {
    /// Returns how many accepted bytes the terminal holds unreadable.
    fn held_bytes(&self) -> Result<usize, PtyError>;

    /// Lets held output reach the master, or suspends it again.
    fn set_output_flow(&self, flowing: bool) -> Result<(), PtyError>;
}

impl HeldOutput for OutputPause {
    fn held_bytes(&self) -> Result<usize, PtyError> {
        terminal_output_queue(&self.tty)
    }

    fn set_output_flow(&self, flowing: bool) -> Result<(), PtyError> {
        let action = if flowing { Action::OOn } else { Action::OOff };
        tcflow(&self.tty, action).map_err(rustix_errno_to_pty_error)
    }
}

/// Counts output a suspended Darwin terminal holds away from the master.
///
/// XNU keeps a stopped terminal's `t_outq` unreadable on the master and lets
/// writers keep filling it up to the high-water mark; `TIOCOUTQ` on the
/// terminal reports that queue.
///
/// # Errors
///
/// Returns [`PtyError::Io`] when the `ioctl` fails or reports a negative count.
#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "rustix exposes TIOCOUTQ only through its typed unsafe ioctl interface"
)]
fn terminal_output_queue(tty: &OwnedFd) -> Result<usize, PtyError> {
    // SAFETY: `TIOCOUTQ` is `_IOR('t', 115, int)` in Darwin's `sys/ttycom.h`:
    // the kernel writes exactly one `int`, which is the getter's output type.
    let getter = unsafe { rustix::ioctl::Getter::<{ libc::TIOCOUTQ }, libc::c_int>::new() };
    // SAFETY: `tty` is an open terminal descriptor, and the getter matches the
    // request's direction and size as stated above.
    let count = unsafe { rustix::ioctl::ioctl(tty, getter) }.map_err(rustix_errno_to_pty_error)?;
    usize::try_from(count).map_err(|_negative| {
        PtyError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal reported a negative output queue",
        ))
    })
}

/// Reports that a suspended Linux terminal holds no output.
///
/// Linux refuses PTY writes while output is suspended (`pty_write`), so the
/// writer blocks in the child and every accepted byte stays readable on the
/// master, where the boundary drain already consumes it.
///
/// # Errors
///
/// Never fails; the signature matches the Darwin query.
#[cfg(not(target_os = "macos"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "matches the fallible Darwin TIOCOUTQ query"
)]
fn terminal_output_queue(_tty: &OwnedFd) -> Result<usize, PtyError> {
    Ok(0)
}

impl OutputPause {
    fn new(tty_name: &Path) -> Result<Self, PtyError> {
        let tty = open(
            tty_name,
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(rustix_errno_to_pty_error)?;
        tcflow(&tty, Action::OOff).map_err(rustix_errno_to_pty_error)?;
        Ok(Self {
            tty,
            resumed: false,
        })
    }

    fn resume(mut self) -> Result<(), PtyError> {
        tcflow(&self.tty, Action::OOn).map_err(rustix_errno_to_pty_error)?;
        self.resumed = true;
        Ok(())
    }
}

impl Drop for OutputPause {
    fn drop(&mut self) {
        if !self.resumed {
            if let Err(error) = tcflow(&self.tty, Action::OOn) {
                event!(
                    name: "worker.pty.output_resume.failed",
                    Level::WARN,
                    error.type = "io",
                    error.code = error.raw_os_error(),
                    "Last-resort PTY output resume failed; output may remain paused",
                );
            }
        }
    }
}

impl OutputReadiness {
    /// Builds a readiness over its own duplicate of `master`.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError`] when the master cannot be duplicated or the
    /// cancellation pipe cannot be created.
    fn new(master: BorrowedFd<'_>) -> Result<Self, PtyError> {
        let master = master.try_clone_to_owned().map_err(PtyError::Io)?;
        // `std::io::pipe` marks both ends close-on-exec: atomically through
        // `pipe2` on Linux, and right after `pipe` on Darwin, which has no
        // `pipe2`. PTY children additionally close every inherited descriptor
        // above stderr before `exec`, so that window cannot leak into them.
        let (cancel_reader, cancel_writer) = std::io::pipe().map_err(PtyError::Io)?;
        let cancel_reader = OwnedFd::from(cancel_reader);
        let cancel_writer = OwnedFd::from(cancel_writer);
        // Non-blocking so arming an already full pipe cannot stall the caller.
        // The read end is never read, so its blocking mode does not matter.
        rustix::io::ioctl_fionbio(&cancel_writer, true).map_err(rustix_errno_to_pty_error)?;
        Ok(Self {
            master,
            cancel_reader,
            cancel_writer,
        })
    }

    fn ready(&self, timeout_ms: isize) -> Result<Option<OutputReadyState>, PtyError> {
        let timeout = readiness_timeout(timeout_ms)?;
        loop {
            let watched = PollFlags::IN | PollFlags::HUP | PollFlags::ERR;
            let mut fds = [
                PollFd::new(&self.master, watched),
                PollFd::new(&self.cancel_reader, watched),
            ];
            match poll(&mut fds, timeout.as_ref()) {
                Ok(0) => return Ok(None),
                Ok(_woken) => {
                    if let Some(state) =
                        classify_readiness(fds[MASTER_SLOT].revents(), fds[CANCEL_SLOT].revents())
                            .map_err(readiness_fault_to_pty_error)?
                    {
                        return Ok(Some(state));
                    }
                }
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(rustix_errno_to_pty_error(error)),
            }
        }
    }

    /// Arms the cancellation latch; arming it again changes nothing.
    fn cancel(&self) -> Result<(), PtyError> {
        loop {
            match rustix::io::write(&self.cancel_writer, &[1_u8]) {
                Ok(_written) => return Ok(()),
                Err(rustix::io::Errno::INTR) => {}
                // A full pipe already carries an unconsumed cancellation, which
                // is exactly the state arming wants to reach.
                Err(rustix::io::Errno::AGAIN) => return Ok(()),
                Err(error) => return Err(rustix_errno_to_pty_error(error)),
            }
        }
    }
}

/// Borrows the descriptor `portable-pty` exposes for a PTY master.
///
/// `MasterPty` hands out the master only as a raw descriptor, while duplicating
/// it safely needs a `BorrowedFd`, so the conversion happens here and nowhere
/// else. The borrow is tied to `master`, which owns the descriptor.
///
/// # Errors
///
/// Returns [`PtyError::MissingMasterFd`] when the master exposes no descriptor.
#[expect(
    unsafe_code,
    reason = "portable-pty exposes the PTY master only as a RawFd; duplicating it safely needs a BorrowedFd"
)]
fn borrow_master(master: &dyn MasterPty) -> Result<BorrowedFd<'_>, PtyError> {
    let raw = master
        .as_raw_fd()
        .filter(|raw| *raw >= 0)
        .ok_or(PtyError::MissingMasterFd)?;
    // SAFETY: `as_raw_fd` returns the descriptor the master owns and closes only
    // when it is dropped. The returned borrow lives no longer than `&master`, so
    // the master cannot be dropped, and the descriptor cannot close, while it
    // exists. The filter above rules out the `-1` sentinel `borrow_raw` forbids.
    Ok(unsafe { BorrowedFd::borrow_raw(raw) })
}

/// Converts the shared millisecond convention to a `poll` timeout.
///
/// A negative value means "wait indefinitely", which `poll` expresses as an
/// absent timespec rather than a negative number.
///
/// # Errors
///
/// Returns [`PtyError`] when a non-negative value does not fit the native
/// timespec, which a caller could only produce by asking for an absurd wait.
fn readiness_timeout(timeout_ms: isize) -> Result<Option<rustix::event::Timespec>, PtyError> {
    /// Milliseconds per second, for splitting the caller's value.
    const MILLIS_PER_SECOND: i64 = 1_000;
    /// Nanoseconds per millisecond, for the sub-second remainder.
    const NANOS_PER_MILLI: i64 = 1_000_000;

    if timeout_ms < 0 {
        return Ok(None);
    }
    let millis = i64::try_from(timeout_ms)
        .map_err(|_range| PtyError::Io(io::Error::from(io::ErrorKind::InvalidInput)))?;
    let nanoseconds = i32::try_from((millis % MILLIS_PER_SECOND) * NANOS_PER_MILLI)
        .map_err(|_range| PtyError::Io(io::Error::from(io::ErrorKind::InvalidInput)))?;
    Ok(Some(rustix::event::Timespec {
        tv_sec: millis / MILLIS_PER_SECOND,
        tv_nsec: nanoseconds.into(),
    }))
}

/// Maps a rejected readiness classification onto the worker's typed error.
fn readiness_fault_to_pty_error(fault: ReadinessFault) -> PtyError {
    match fault {
        ReadinessFault::InvalidDescriptor => PtyError::Io(io::Error::from_raw_os_error(
            rustix::io::Errno::BADF.raw_os_error(),
        )),
    }
}

fn rustix_errno_to_pty_error(error: rustix::io::Errno) -> PtyError {
    PtyError::Io(io::Error::from_raw_os_error(error.raw_os_error()))
}

/// Why a readiness wake could not be classified.
///
/// Separate from [`PtyError`] so the classification stays a pure function with
/// no I/O vocabulary, testable on any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadinessFault {
    /// The kernel reported a descriptor that is not open.
    ///
    /// `poll` keeps answering this for a closed descriptor, so treating it as
    /// "nothing happened" would spin instead of failing.
    InvalidDescriptor,
}

/// Classifies one `poll` wake over the PTY master and the cancel pipe.
///
/// Cancellation wins over output: a caller that asked to stop must not be made
/// to drain first. Hangup and error on the master are deliberately reported as
/// output, because the read that follows is what turns them into EOF or a typed
/// I/O failure — and a hangup arriving together with buffered bytes must still
/// deliver those bytes.
///
/// `None` means the wait timed out with nothing ready.
fn classify_readiness(
    master: PollFlags,
    cancel: PollFlags,
) -> Result<Option<OutputReadyState>, ReadinessFault> {
    if master.contains(PollFlags::NVAL) || cancel.contains(PollFlags::NVAL) {
        return Err(ReadinessFault::InvalidDescriptor);
    }
    if cancel.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
        return Ok(Some(OutputReadyState::Cancelled));
    }
    if master.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
        return Ok(Some(OutputReadyState::Output));
    }
    Ok(None)
}

trait OutputReady {
    fn ready(&self, timeout_ms: isize) -> Result<Option<OutputReadyState>, PtyError>;
}

impl OutputReady for OutputReadiness {
    fn ready(&self, timeout_ms: isize) -> Result<Option<OutputReadyState>, PtyError> {
        Self::ready(self, timeout_ms)
    }
}

fn wait_for_output(readiness: &impl OutputReady) -> Result<OutputReadyState, PtyError> {
    readiness
        .ready(WAIT_FOREVER_MS)?
        .ok_or_else(|| PtyError::Io(io::Error::other("blocking PTY readiness returned no event")))
}

fn drain_available_output<R>(
    reader: &Mutex<Box<dyn Read + Send>>,
    readiness: &R,
    output: &OutputHub,
    buffer: &mut [u8],
    byte_budget: usize,
) -> Result<OutputReadState, PtyError>
where
    R: OutputReady + ?Sized,
{
    let mut drained = 0_usize;
    loop {
        if drained >= byte_budget {
            return readiness.ready(NO_WAIT_MS).map(|ready| match ready {
                Some(OutputReadyState::Output) => OutputReadState::BudgetExhausted,
                Some(OutputReadyState::Cancelled) => OutputReadState::Cancelled,
                None => OutputReadState::Open,
            });
        }
        match readiness.ready(NO_WAIT_MS)? {
            Some(OutputReadyState::Output) => {}
            Some(OutputReadyState::Cancelled) => return Ok(OutputReadState::Cancelled),
            None => return Ok(OutputReadState::Open),
        }
        match lock_result(reader)?.read(buffer) {
            Ok(0) => return Ok(OutputReadState::Eof),
            Ok(read) => {
                output.push(&buffer[..read])?;
                drained = drained.saturating_add(read);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // Readiness can lapse before the read. Darwin refuses to hand a
            // stopped terminal's queued output to the master even after `poll`
            // reported it, so an output pause or a `^S` landing in between
            // would park a blocking read with the ordering gate held until
            // output resumes; the resume itself waits for that gate.
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(OutputReadState::Lapsed);
            }
            Err(error) => return Err(PtyError::Io(error)),
        }
    }
}

/// Bounds for draining output up to a resize or snapshot boundary.
#[derive(Debug, Clone, Copy)]
struct SnapshotDrain {
    /// Most bytes one boundary may consume from the master.
    byte_budget: usize,
    /// Longest wait for terminal-held bytes to reach the master.
    settle_timeout: Duration,
}

/// Drains every byte the PTY accepted before a paused boundary.
///
/// The caller has suspended output and holds the ordering gate. Bytes readable
/// on the master are drained first. Bytes the terminal still holds away from
/// the master (Darwin keeps a suspended terminal's queue unreadable) are then
/// released under the same gate, read up to the counted amount, and suspended
/// again, so none of them is parsed after the geometry changes. Output the
/// child writes after that count is live output, exactly like a Linux writer
/// that blocked on the suspension.
///
/// # Errors
///
/// Returns [`PtyError::OutputDrainLimit`] when the queue exceeds the budget,
/// [`PtyError::OutputBoundaryUnsettled`] when held bytes do not arrive within
/// the settle timeout, and the closed or forced-close errors of the PTY.
fn drain_snapshot_boundary<R, H>(
    reader: &Mutex<Box<dyn Read + Send>>,
    readiness: &R,
    held: &H,
    output: &OutputHub,
    buffer: &mut [u8],
    drain: SnapshotDrain,
) -> Result<(), PtyError>
where
    R: OutputReady + ?Sized,
    H: HeldOutput + ?Sized,
{
    match drain_available_output(reader, readiness, output, buffer, drain.byte_budget)? {
        OutputReadState::Open | OutputReadState::Lapsed => {}
        OutputReadState::Eof => return Err(closed_before_boundary(output)),
        OutputReadState::Cancelled => return Err(PtyError::OutputForcedClosed),
        OutputReadState::BudgetExhausted => return Err(PtyError::OutputDrainLimit),
    }
    let held_bytes = held.held_bytes()?;
    if held_bytes == 0 {
        return Ok(());
    }
    if held_bytes > drain.byte_budget {
        return Err(PtyError::OutputDrainLimit);
    }
    held.set_output_flow(true)?;
    let consumed = consume_held_output(
        reader,
        readiness,
        held,
        output,
        buffer,
        held_bytes,
        drain.settle_timeout,
    );
    let suspended = held.set_output_flow(false);
    consumed.and(suspended)
}

/// Reads `remaining` released bytes from the master into the output model.
///
/// Reads never exceed the count, so a producer refilling the terminal
/// cannot extend the gate hold. A quiet master re-checks the held count, which
/// drops to zero when the terminal discarded its queue.
///
/// # Errors
///
/// Returns [`PtyError::OutputBoundaryUnsettled`] when the bytes do not arrive
/// before `settle_timeout`, or the PTY's closed, forced-close, and I/O errors.
fn consume_held_output<R, H>(
    reader: &Mutex<Box<dyn Read + Send>>,
    readiness: &R,
    held: &H,
    output: &OutputHub,
    buffer: &mut [u8],
    mut remaining: usize,
    settle_timeout: Duration,
) -> Result<(), PtyError>
where
    R: OutputReady + ?Sized,
    H: HeldOutput + ?Sized,
{
    let deadline = Instant::now() + settle_timeout;
    while remaining > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(PtyError::OutputBoundaryUnsettled);
        }
        let wait_ms = isize::try_from(left.min(HELD_OUTPUT_RECHECK_INTERVAL).as_millis())
            .map_err(|_range| PtyError::OutputBoundaryUnsettled)?;
        if readiness.ready(wait_ms)? == Some(OutputReadyState::Cancelled) {
            return Err(PtyError::OutputForcedClosed);
        }
        let limit = remaining.min(buffer.len());
        match lock_result(reader)?.read(&mut buffer[..limit]) {
            Ok(0) => return Err(closed_before_boundary(output)),
            Ok(read) => {
                output.push(&buffer[..read])?;
                remaining = remaining.saturating_sub(read);
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(PtyError::Io(error)),
        }
        if held.held_bytes()? == 0 {
            return Ok(());
        }
    }
    Ok(())
}

/// Records the PTY close that ended a boundary drain and reports it.
fn closed_before_boundary(output: &OutputHub) -> PtyError {
    output.mark_exit();
    PtyError::Io(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "PTY closed before atomic resize or snapshot",
    ))
}

fn observe_child_exit(pid: u32) -> Result<Exit, io::Error> {
    let native_pid = i32::try_from(pid)
        .ok()
        .and_then(RustixPid::from_raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid child PID"))?;
    let status =
        wait_for_child_status(native_pid, |pid, options| waitid(WaitId::Pid(pid), options))?;
    if let Some(exit_code) = status.exit_status() {
        return Ok(Exit {
            exit_code: Some(exit_code),
            signal: None,
            success: exit_code == 0,
        });
    }
    if let Some(signal_number) = status.terminating_signal() {
        let signal = Signal::try_from(signal_number).map_or_else(
            |_unknown| format!("signal-{signal_number}"),
            |signal| signal.as_str().to_owned(),
        );
        return Ok(Exit {
            exit_code: None,
            signal: Some(signal),
            success: false,
        });
    }
    Err(io::Error::other(
        "child observation returned a non-terminal status",
    ))
}

fn wait_for_child_status(
    native_pid: RustixPid,
    mut wait: impl FnMut(RustixPid, WaitIdOptions) -> Result<Option<WaitIdStatus>, rustix::io::Errno>,
) -> Result<WaitIdStatus, io::Error> {
    loop {
        match wait(native_pid, WaitIdOptions::EXITED | WaitIdOptions::NOWAIT) {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                return Err(io::Error::other(
                    "blocking child observation returned no status",
                ));
            }
            Err(error) if error == rustix::io::Errno::INTR => {}
            Err(error) => return Err(io::Error::from(error)),
        }
    }
}

fn reap_child(child: &mut SpawnGuard, pid: u32) -> Exit {
    match child.wait() {
        Ok(status) => Exit {
            exit_code: status
                .signal()
                .is_none()
                .then(|| i32::try_from(status.exit_code()).ok())
                .flatten(),
            signal: status.signal().map(str::to_owned),
            success: status.success(),
        },
        Err(error) => {
            event!(
                name: "worker.child.wait.failed",
                Level::ERROR,
                process.pid = pid,
                error.type = "io",
                error.message = %error,
                "child wait failed for {{process.pid}}: {{error.message}}",
            );
            Exit {
                exit_code: None,
                signal: None,
                success: false,
            }
        }
    }
}

/// Returns whether the process currently behind `pid` is still executing.
///
/// Callers verify the start identity first; the PTY root stays unreaped until
/// cleanup, so its id cannot name another process in between.
fn root_is_running(pid: u32) -> Result<bool, io::Error> {
    let inspector = HostInspector::new();
    match inspector.identity(pid).map_err(io::Error::other)? {
        Some(identity) => inspector.is_running(identity).map_err(io::Error::other),
        None => Ok(false),
    }
}

fn read_process_start(pid: u32) -> Result<String, io::Error> {
    HostInspector::new()
        .identity(pid)
        .map_err(io::Error::other)?
        .map(|identity| identity.start_identity.to_string())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "process no longer exists"))
}

async fn join_thread(slot: &Arc<Mutex<Option<thread::JoinHandle<()>>>>) -> Result<(), PtyError> {
    let thread = lock_result(slot)?.take();
    if let Some(thread) = thread {
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .map_err(|_join_error| PtyError::Task)?
            .map_err(|_thread_panic| PtyError::Task)?;
    }
    Ok(())
}

fn lock_result<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, PtyError> {
    mutex.lock().map_err(|_poison| PtyError::Poisoned)
}

#[cfg(test)]
mod tests {
    use super::{
        commit_resize_before_resume, drain_available_output, drain_snapshot_boundary,
        read_process_start, wait_for_child_status, Command, EnvBase, HeldOutput, OutputReadState,
        OutputReady, OutputReadyState, PtyError, PtyOwner, ResizeCommit, ResizeState,
        SnapshotDrain, SpawnGuard, StartupLatch, OUTPUT_DRAIN_BATCH_BYTES, READ_CHUNK_BYTES,
    };
    use crate::{InputFragment, InputPlan, OutputEvent, OutputHub, WorkerConfig};
    use pohunek_platform::process::{HostInspector, ProcessInspector};
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::collections::{HashMap, VecDeque};
    use std::ffi::OsStr;
    use std::io::Cursor;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Bounds observation of the rollback fixture's signal-resistant descendant.
    const ROLLBACK_CHILD_READY_TIMEOUT: Duration = Duration::from_secs(2);
    /// Avoids busy-spinning while the rollback fixture creates its descendant.
    const ROLLBACK_CHILD_READY_POLL: Duration = Duration::from_millis(10);
    /// Bounds delivery of the rollback kill to both exact process generations.
    const ROLLBACK_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
    /// Bounds a drain whose readiness lapsed; a parked read never returns.
    const LAPSED_READ_DEADLINE: Duration = Duration::from_secs(2);
    /// Settle timeout for fake terminals; short because nothing real is awaited.
    const TEST_HELD_OUTPUT_SETTLE_TIMEOUT: Duration = Duration::from_millis(50);
    /// Bounds each signal of the real Darwin held-output fixture.
    ///
    /// Every wait ends on an explicit fixture signal, so this only absorbs
    /// process scheduling on a loaded runner; expiry means the signal never came.
    #[cfg(target_os = "macos")]
    const HELD_QUEUE_DEADLINE: Duration = Duration::from_secs(5);
    /// Input size well beyond the Linux and Darwin PTY input queues.
    ///
    /// One MiB forces many would-block writes on both kernels, whose queues
    /// hold tens of KiB at most.
    const OVERSIZED_INPUT_BYTES: usize = 1024 * 1024;
    /// Bounds each step of the oversized-input round trip.
    const OVERSIZED_INPUT_DEADLINE: Duration = Duration::from_secs(10);

    #[test]
    fn non_reaping_wait_retries_interrupts_without_falling_back_to_reap() {
        let pid = rustix::process::Pid::from_raw(1).expect("positive pid");
        let mut calls = 0_u8;

        let error = wait_for_child_status(pid, |_wait_id, options| {
            assert!(options.contains(rustix::process::WaitIdOptions::NOWAIT));
            calls = calls.saturating_add(1);
            if calls == 1 {
                Err(rustix::io::Errno::INTR)
            } else {
                Err(rustix::io::Errno::CHILD)
            }
        })
        .expect_err("permanent observation failure");

        assert_eq!(calls, 2);
        assert_eq!(
            error.raw_os_error(),
            Some(rustix::io::Errno::CHILD.raw_os_error())
        );
    }

    /// The production stop grace: a stop under test never escalates early because
    /// a loaded host was slow, and a child that dies on SIGTERM does not wait it out.
    fn stop_grace() -> Duration {
        WorkerConfig::new().stop_grace
    }

    fn shell(script: &str, cwd: &std::path::Path) -> Command {
        Command {
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            base: EnvBase::Empty,
            env: crate::test_support::child_env(),
            cwd: cwd.to_path_buf(),
            cols: 80,
            rows: 24,
        }
    }

    fn spawn(command: Command) -> PtyOwner {
        let config = WorkerConfig::new();
        PtyOwner::spawn(
            command,
            config.history_bytes,
            config.subscriber_bytes,
            config.input_dedup_entries,
        )
        .expect("spawn PTY")
    }

    #[derive(Debug)]
    struct TestReadiness {
        states: Mutex<VecDeque<bool>>,
    }

    impl TestReadiness {
        fn new(states: impl IntoIterator<Item = bool>) -> Self {
            Self {
                states: Mutex::new(states.into_iter().collect()),
            }
        }
    }

    impl OutputReady for TestReadiness {
        fn ready(&self, _timeout_ms: isize) -> Result<Option<OutputReadyState>, PtyError> {
            Ok(self
                .states
                .lock()
                .expect("test readiness lock")
                .pop_front()
                .unwrap_or(false)
                .then_some(OutputReadyState::Output))
        }
    }

    /// Suspended terminal whose queue the master cannot read, as on Darwin.
    ///
    /// The same state backs both the master reader and the terminal-side
    /// [`HeldOutput`] view, so a test sees exactly which bytes a boundary read
    /// and in which flow state.
    #[derive(Debug, Clone)]
    struct FakeTerminal {
        state: std::sync::Arc<Mutex<FakeTerminalState>>,
    }

    #[derive(Debug, Default)]
    struct FakeTerminalState {
        queue: VecDeque<u8>,
        flowing: bool,
        flow_changes: Vec<bool>,
        /// Bytes a producer appends as soon as output flows again.
        refill: Vec<u8>,
        /// Whether releasing output discards the queue, as `tcflush` does.
        discard_on_release: bool,
        /// Whether released bytes never become readable.
        stuck: bool,
    }

    impl FakeTerminal {
        fn holding(bytes: &[u8]) -> Self {
            Self::with(bytes, |_state| {})
        }

        fn with(bytes: &[u8], configure: impl FnOnce(&mut FakeTerminalState)) -> Self {
            let mut state = FakeTerminalState {
                queue: bytes.iter().copied().collect(),
                ..FakeTerminalState::default()
            };
            configure(&mut state);
            Self {
                state: std::sync::Arc::new(Mutex::new(state)),
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, FakeTerminalState> {
            self.state.lock().expect("fake terminal lock")
        }

        fn master_reader(&self) -> Mutex<Box<dyn std::io::Read + Send>> {
            Mutex::new(Box::new(self.clone()))
        }
    }

    impl std::io::Read for FakeTerminal {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut state = self.lock();
            if !state.flowing || state.stuck || state.queue.is_empty() {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            let read = buf.len().min(state.queue.len());
            for (slot, byte) in buf.iter_mut().zip(state.queue.drain(..read)) {
                *slot = byte;
            }
            Ok(read)
        }
    }

    impl HeldOutput for FakeTerminal {
        fn held_bytes(&self) -> Result<usize, PtyError> {
            Ok(self.lock().queue.len())
        }

        fn set_output_flow(&self, flowing: bool) -> Result<(), PtyError> {
            let mut state = self.lock();
            state.flowing = flowing;
            state.flow_changes.push(flowing);
            if flowing {
                if state.discard_on_release {
                    state.queue.clear();
                }
                let refill = state.refill.clone();
                state.queue.extend(refill);
            }
            Ok(())
        }
    }

    fn test_drain(byte_budget: usize) -> SnapshotDrain {
        SnapshotDrain {
            byte_budget,
            settle_timeout: TEST_HELD_OUTPUT_SETTLE_TIMEOUT,
        }
    }

    /// Bytes a paused terminal hides at the boundary are parsed before it.
    ///
    /// Readiness is reported, the read finds nothing because output is
    /// suspended, and the terminal still holds old-geometry bytes. The drain
    /// must consume exactly those bytes and suspend output again, leaving
    /// bytes the producer adds after the count as live output.
    #[test]
    fn boundary_consumes_output_the_paused_terminal_holds() {
        let terminal = FakeTerminal::with(b"old-geometry", |state| {
            state.refill = b"live".to_vec();
        });
        let output = OutputHub::new(64 * 1024, 64 * 1024, 24, 80).expect("output hub");
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];

        drain_snapshot_boundary(
            &terminal.master_reader(),
            &TestReadiness::new([true]),
            &terminal,
            &output,
            &mut buffer,
            test_drain(OUTPUT_DRAIN_BATCH_BYTES),
        )
        .expect("held bytes settle before the boundary");

        assert_eq!(output.next_offset(), b"old-geometry".len() as u64);
        let state = terminal.lock();
        assert_eq!(state.flow_changes, [true, false]);
        assert!(!state.flowing, "output stays suspended for the boundary");
        assert_eq!(state.queue, b"live".to_vec(), "later bytes stay live");
    }

    /// Held bytes that never arrive fail the boundary before any commit.
    #[test]
    fn unsettled_held_output_fails_the_boundary_retryably() {
        let terminal = FakeTerminal::with(b"old-geometry", |state| state.stuck = true);
        let output = OutputHub::new(64 * 1024, 64 * 1024, 24, 80).expect("output hub");
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        let mut state = ResizeState {
            cols: 80,
            rows: 24,
            sequences: HashMap::new(),
        };
        let mut committed = false;

        let error = drain_snapshot_boundary(
            &terminal.master_reader(),
            &TestReadiness::new([true]),
            &terminal,
            &output,
            &mut buffer,
            test_drain(OUTPUT_DRAIN_BATCH_BYTES),
        )
        .and_then(|()| {
            committed = true;
            commit_resize_before_resume(
                &mut state,
                ResizeCommit::Attach { cols: 90, rows: 28 },
                || Ok(()),
            )
        })
        .expect_err("held bytes that never arrive must not cross the boundary");

        assert!(matches!(error, PtyError::OutputBoundaryUnsettled));
        assert!(!committed);
        assert_eq!((state.cols, state.rows), (80, 24));
        assert_eq!(output.next_offset(), 0);
        assert_eq!(terminal.lock().flow_changes, [true, false]);
    }

    /// A terminal that discards its held queue settles the boundary at once.
    #[test]
    fn discarded_held_output_settles_the_boundary() {
        let terminal = FakeTerminal::with(b"old-geometry", |state| {
            state.discard_on_release = true;
        });
        let output = OutputHub::new(64 * 1024, 64 * 1024, 24, 80).expect("output hub");
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];

        drain_snapshot_boundary(
            &terminal.master_reader(),
            &TestReadiness::new([true]),
            &terminal,
            &output,
            &mut buffer,
            test_drain(OUTPUT_DRAIN_BATCH_BYTES),
        )
        .expect("a discarded queue leaves nothing to cross the boundary");

        assert_eq!(output.next_offset(), 0);
        assert_eq!(terminal.lock().flow_changes, [true, false]);
    }

    /// Held output beyond the budget is refused without releasing output.
    #[test]
    fn held_output_beyond_the_budget_is_refused() {
        let terminal = FakeTerminal::holding(&[b'x'; READ_CHUNK_BYTES + 1]);
        let output = OutputHub::new(64 * 1024, 64 * 1024, 24, 80).expect("output hub");
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];

        let error = drain_snapshot_boundary(
            &terminal.master_reader(),
            &TestReadiness::new([]),
            &terminal,
            &output,
            &mut buffer,
            test_drain(READ_CHUNK_BYTES),
        )
        .expect_err("held output beyond the budget");

        assert!(matches!(error, PtyError::OutputDrainLimit));
        assert!(terminal.lock().flow_changes.is_empty());
    }

    /// Spawns a fixture that prints `ready` once its FIFOs exist, then prints
    /// `held-before-boundary` after `release` delivers a line and acknowledges
    /// that returned write with one byte on `written`.
    #[cfg(target_os = "macos")]
    fn held_output_fixture(release: &std::path::Path, written: &std::path::Path) -> PtyOwner {
        let mut command = shell(
            "mkfifo \"$HELD_RELEASE\" \"$HELD_WRITTEN\" && printf ready \
             && read go < \"$HELD_RELEASE\" && printf held-before-boundary \
             && printf w > \"$HELD_WRITTEN\" && sleep 30",
            release.parent().expect("FIFO directory"),
        );
        command.env.extend([
            (
                "HELD_RELEASE".to_owned(),
                release.to_str().expect("UTF-8 FIFO path").to_owned(),
            ),
            (
                "HELD_WRITTEN".to_owned(),
                written.to_str().expect("UTF-8 FIFO path").to_owned(),
            ),
        ]);
        spawn(command)
    }

    /// Waits until the PTY has produced exactly `expected`.
    #[cfg(target_os = "macos")]
    async fn await_output(pty: &PtyOwner, expected: &[u8]) {
        let mut subscriber = pty.subscribe_output(None).expect("subscribe");
        let mut observed = Vec::new();
        tokio::time::timeout(HELD_QUEUE_DEADLINE, async {
            while observed.len() < expected.len() {
                match subscriber.recv().await {
                    Some(OutputEvent::Replay(chunk) | OutputEvent::Output(chunk)) => {
                        observed.extend_from_slice(&chunk.bytes);
                    }
                    other => panic!("fixture output ended early: {other:?}"),
                }
            }
        })
        .await
        .expect("fixture output");
        assert_eq!(observed, expected);
    }

    /// Waits for the fixture's one-byte acknowledgement on a non-blocking FIFO.
    #[cfg(target_os = "macos")]
    async fn await_acknowledgement(written: &tokio::io::unix::AsyncFd<rustix::fd::OwnedFd>) {
        tokio::time::timeout(HELD_QUEUE_DEADLINE, async {
            loop {
                let mut guard = written.readable().await.expect("acknowledgement readiness");
                let mut acknowledgement = [0_u8; 1];
                if let Ok(read) = guard.try_io(|fd| {
                    rustix::io::read(fd.get_ref(), &mut acknowledgement[..])
                        .map_err(std::io::Error::from)
                }) {
                    assert_eq!(read.expect("read the acknowledgement"), 1);
                    return;
                }
            }
        })
        .await
        .expect("the fixture's write to the suspended terminal returns");
    }

    /// A real suspended Darwin terminal hides output until the boundary drains it.
    ///
    /// XNU accepts the child's write into the stopped terminal's queue, keeps
    /// it unreadable on the master, and reports it through `TIOCOUTQ`. The
    /// boundary drain must parse those bytes before returning.
    ///
    /// The fixture is released and acknowledges its write through FIFOs, never
    /// through terminal input: a PTY starts with `IXANY`, and XNU's `ttyinput`
    /// restarts suspended output on any input byte, which would hand the write
    /// to the master instead of holding it.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn darwin_boundary_drains_output_the_suspended_terminal_holds() {
        use rustix::fs::{open, Mode, OFlags};

        const HELD: &[u8] = b"held-before-boundary";
        let fifos = pohunek_test_support::tempdir().expect("fixture FIFO directory");
        let release_path = fifos.path().join("release");
        let written_path = fifos.path().join("written");
        let pty = held_output_fixture(&release_path, &written_path);
        // `ready` follows `mkfifo`, so both FIFOs exist once it is parsed.
        await_output(&pty, b"ready").await;
        let before = pty.output().next_offset();

        let pause = super::OutputPause::new(&pty.tty_name).expect("pause output");
        // Read-write opens of a FIFO never wait for a peer on XNU, and holding
        // both ends keeps the release buffered until the fixture reads it.
        let written = tokio::io::unix::AsyncFd::with_interest(
            open(
                &written_path,
                OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("open the acknowledgement"),
            tokio::io::Interest::READABLE,
        )
        .expect("watch the acknowledgement");
        let release = open(&release_path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
            .expect("open the release");
        assert_eq!(
            rustix::io::write(&release, b"go\n").expect("release the fixture"),
            b"go\n".len()
        );
        await_acknowledgement(&written).await;
        assert_eq!(
            pause.held_bytes().expect("TIOCOUTQ"),
            HELD.len(),
            "the suspended terminal holds exactly the acknowledged write"
        );
        assert_eq!(
            pty.output().next_offset(),
            before,
            "held bytes must not be readable on the master"
        );

        let order = pty.output_order.lock().expect("ordering gate");
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        drain_snapshot_boundary(
            &pty.output_reader,
            &*pty.output_readiness,
            &pause,
            pty.output(),
            &mut buffer,
            SnapshotDrain {
                byte_budget: OUTPUT_DRAIN_BATCH_BYTES,
                settle_timeout: super::HELD_OUTPUT_SETTLE_TIMEOUT,
            },
        )
        .expect("held bytes settle before the boundary");
        drop(order);
        assert_eq!(pty.output().next_offset(), before + HELD.len() as u64);
        assert_eq!(pause.held_bytes().expect("TIOCOUTQ"), 0);
        pause.resume().expect("resume output");

        let _ = pty.stop("cleanup", stop_grace()).await.expect("cleanup");
    }

    #[tokio::test]
    async fn real_pty_drains_output_and_reports_exit() {
        let cwd = crate::test_support::child_cwd();
        let pty = spawn(shell("printf 'worker-ready'; exit 7", cwd.path()));
        let mut output = pty.subscribe_output(None).expect("subscribe");
        let exit = pty.wait_exit().await.expect("exit");

        assert_eq!(exit.exit_code, Some(7));
        let mut observed = Vec::new();
        while let Some(event) = output.recv().await {
            match event {
                OutputEvent::Replay(chunk) | OutputEvent::Output(chunk) => {
                    observed.extend_from_slice(&chunk.bytes);
                }
                OutputEvent::Exit { .. } => break,
                OutputEvent::Gap { .. } => observed.clear(),
                OutputEvent::TerminalSnapshot(chunk) => {
                    observed.extend_from_slice(&chunk.bytes);
                }
            }
        }
        assert!(
            String::from_utf8_lossy(&observed).contains("worker-ready"),
            "real PTY output was not drained"
        );
    }

    /// Marks each variable line the listing child prints, so libtest's own
    /// banner lines are never mistaken for part of the environment.
    const ENV_LISTING_PREFIX: &str = "env-listing ";
    /// Libtest name of [`environment_listing_child`], used to re-execute only it.
    const ENV_LISTING_CHILD_TEST: &str = "pty::tests::environment_listing_child";

    /// Renders one variable in the listing format owned by this module.
    ///
    /// `Debug` escapes control characters and non-UTF-8 bytes, so a value can
    /// never span or forge a line, and no host tool participates in formatting.
    fn env_listing_line(name: &OsStr, value: &OsStr) -> String {
        format!("{ENV_LISTING_PREFIX}{name:?}={value:?}")
    }

    /// Prints this process's complete environment, one `env_listing_line` each.
    ///
    /// Not a test of its own: `empty_base_child_sees_exactly_the_command_environment`
    /// re-executes the test binary to run it as the PTY child. It is `#[ignore]`
    /// so ordinary runs skip it; the parent selects it with `--ignored --exact`.
    #[test]
    #[ignore = "re-executed as the PTY child of the empty-base environment test"]
    fn environment_listing_child() {
        for (name, value) in std::env::vars_os() {
            println!("{}", env_listing_line(&name, &value));
        }
    }

    #[tokio::test]
    async fn empty_base_child_sees_exactly_the_command_environment() {
        let cwd = pohunek_test_support::tempdir().expect("child working directory");
        let pty = spawn(Command {
            program: std::env::current_exe()
                .expect("test executable")
                .into_os_string()
                .into_string()
                .expect("test executable path is UTF-8"),
            args: [
                "--ignored",
                "--exact",
                ENV_LISTING_CHILD_TEST,
                "--nocapture",
            ]
            .map(str::to_owned)
            .to_vec(),
            base: EnvBase::Empty,
            env: vec![
                ("SHELL".to_owned(), "/bin/sh".to_owned()),
                ("EXPLICIT".to_owned(), "value=with equals".to_owned()),
            ],
            cwd: cwd.path().to_path_buf(),
            cols: 200,
            rows: 24,
        });
        let mut output = pty.subscribe_output(None).expect("subscribe");
        let exit = pty.wait_exit().await.expect("exit");
        assert_eq!(exit.exit_code, Some(0));

        let mut observed = Vec::new();
        while let Some(event) = output.recv().await {
            match event {
                OutputEvent::Replay(chunk) | OutputEvent::Output(chunk) => {
                    observed.extend_from_slice(&chunk.bytes);
                }
                OutputEvent::Exit { .. } => break,
                OutputEvent::Gap { .. } => panic!("environment listing must fit the history"),
                OutputEvent::TerminalSnapshot(_) => {}
            }
        }
        let observed = String::from_utf8(observed).expect("environment listing is UTF-8");
        let mut listing = observed
            .lines()
            .filter(|line| line.starts_with(ENV_LISTING_PREFIX))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        listing.sort();

        // The test runner's own environment (PATH, HOME, ...) must not appear.
        let mut expected = vec![
            env_listing_line(OsStr::new("SHELL"), OsStr::new("/bin/sh")),
            env_listing_line(OsStr::new("EXPLICIT"), OsStr::new("value=with equals")),
        ];
        expected.sort();
        assert_eq!(listing, expected);
    }

    #[tokio::test]
    async fn real_pty_accepts_deduplicated_input_and_stops_group() {
        let cwd = crate::test_support::child_cwd();
        let pty = spawn(shell(
            "read line; printf 'got:%s' \"$line\"; sleep 30",
            cwd.path(),
        ));
        let operation = InputPlan {
            write_id: "input-1".to_owned(),
            fragments: vec![InputFragment {
                bytes: b"hello\n".to_vec(),
                delay_after: Duration::ZERO,
            }],
        };

        pty.input()
            .execute_control("daemon-test", 1, operation.clone())
            .await
            .expect("input");
        pty.input()
            .execute_control("daemon-test", 1, operation)
            .await
            .expect("deduplicate");
        let exit = pty.stop("stop-1", stop_grace()).await.expect("stop");

        assert!(!exit.success);
        assert_eq!(
            pty.stop("stop-1", stop_grace())
                .await
                .expect("duplicate stop"),
            exit
        );
    }

    /// Darwin ends the PTY for every descendant once the root exits.
    ///
    /// XNU's `proc_exit` drains the controlling terminal, hangs up its
    /// foreground group and revokes every open reference to it when the session
    /// leader exits. The root's trailing output still arrives before EOF, a
    /// descendant that ignores the hangup keeps running, and the unreaped root
    /// stays observable, so `stop` can still prove the group is the root's and
    /// terminate what is left of it.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn root_exit_revokes_the_terminal_and_stop_still_ends_the_group() {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid as NixPid;

        /// Kills the fixture descendant if an assertion fails before `stop`.
        struct Descendant(NixPid);

        impl Drop for Descendant {
            fn drop(&mut self) {
                let _ = kill(self.0, Signal::SIGKILL);
            }
        }

        let cwd = crate::test_support::child_cwd();
        let pty = spawn(shell(
            concat!(
                "trap '' HUP; ",
                "sh -c 'trap \"\" HUP; while :; do sleep 1; done' & ",
                "printf 'descendant:%s\\n' \"$!\"; ",
                "printf 'root-final\\n'"
            ),
            cwd.path(),
        ));
        let mut output = pty.subscribe_output(Some(0)).expect("subscribe output");
        let root_exit = pohunek_test_support::wait::guard("the root to exit", pty.wait_exit())
            .await
            .expect("root exit");

        let observed = pohunek_test_support::wait::guard(
            "the revoked terminal to reach EOF without waiting for the descendant",
            async {
                let mut observed = Vec::new();
                loop {
                    match output.recv().await.expect("output event before EOF") {
                        OutputEvent::Replay(chunk) | OutputEvent::Output(chunk) => {
                            observed.extend_from_slice(&chunk.bytes);
                        }
                        OutputEvent::TerminalSnapshot(chunk) => {
                            observed.extend_from_slice(&chunk.bytes);
                        }
                        OutputEvent::Gap { .. } => observed.clear(),
                        OutputEvent::Exit { .. } => break observed,
                    }
                }
            },
        )
        .await;
        let text = String::from_utf8_lossy(&observed);
        assert!(
            text.contains("root-final"),
            "the root's trailing output must precede EOF: {text:?}"
        );
        let descendant = text
            .split("descendant:")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|pid| pid.parse::<i32>().ok())
            .map(NixPid::from_raw)
            .map(Descendant)
            .expect("fixture reports its descendant");
        assert!(
            kill(descendant.0, None).is_ok(),
            "a descendant that ignores the hangup outlives the revoked terminal"
        );
        assert_eq!(
            read_process_start(pty.identity().pid).expect("unreaped root identity"),
            pty.identity().start_identity,
            "the unreaped root must stay observable as the group authority"
        );

        let stopped = pohunek_test_support::wait::guard(
            "stop to end the remaining group",
            pty.stop("stop-after-revoke", stop_grace()),
        )
        .await
        .expect("stop the remaining group");
        assert_eq!(stopped, root_exit);
        assert!(
            read_process_start(pty.identity().pid).is_err(),
            "root must be reaped after stop"
        );
        pohunek_test_support::wait::wait_until(
            "stop to terminate the descendant left in the root's group",
            || async { kill(descendant.0, None).is_err().then_some(()) },
        )
        .await;
    }

    // Needs a descendant that keeps the PTY open after the root exits. Darwin
    // revokes the controlling terminal from every holder when the session
    // leader exits, so this scenario exists only on Linux.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stop_terminates_descendants_after_root_exit_without_waiting_for_eof() {
        let cwd = crate::test_support::child_cwd();
        let pty = spawn(shell(
            concat!(
                "trap '' HUP; ",
                "sh -c 'trap \"\" HUP TERM; while :; do sleep 1; done' & ",
                "printf 'descendant:%s\\n' \"$!\""
            ),
            cwd.path(),
        ));
        let mut output = pty.subscribe_output(Some(0)).expect("subscribe output");
        let root_exit = pohunek_test_support::wait::guard("the root to exit", pty.wait_exit())
            .await
            .expect("root exit");
        assert!(root_exit.success);
        assert!(
            !pty.output().observe(None, 1).expect("output page").exited,
            "descendant must still hold the slave PTY after root exit"
        );
        assert_eq!(
            read_process_start(pty.identity().pid).expect("unreaped root identity"),
            pty.identity().start_identity,
            "root must remain as the process-group authority during drain"
        );

        // The descendant ignores SIGTERM, so stop spends the first grace window
        // and then needs the second one for the SIGKILLed group to close the PTY.
        // The production grace keeps that second window wide enough for a loaded
        // host; a stop that waited for descendant-held EOF instead of killing the
        // group would never return, which the hang guard reports by name.
        let stopped = pohunek_test_support::wait::guard(
            "stop to terminate the descendant group",
            pty.stop("stop-after-root-exit", WorkerConfig::new().stop_grace),
        )
        .await
        .expect("stop descendant group");
        assert_eq!(stopped, root_exit);

        loop {
            match output.recv().await.expect("output closes after stop") {
                OutputEvent::Exit { .. } => break,
                OutputEvent::Replay(_)
                | OutputEvent::Output(_)
                | OutputEvent::Gap { .. }
                | OutputEvent::TerminalSnapshot(_) => {}
            }
        }
        assert!(pty.output().observe(None, 1).expect("output page").exited);
        assert!(
            read_process_start(pty.identity().pid).is_err(),
            "root must be reaped after the last process-group signal"
        );
    }

    // Needs a descendant that keeps the PTY open after the root exits. Darwin
    // revokes the controlling terminal from every holder when the session
    // leader exits, so this scenario exists only on Linux.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stop_force_closes_output_retained_outside_the_owned_process_group() {
        let barrier = pohunek_test_support::tempdir().expect("create escaped descendant barrier");
        let ready = barrier.path().join("ready");
        let pty = spawn(Command {
            program: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                concat!(
                    "trap '' HUP; ",
                    "setsid sh -c 'trap \"\" HUP TERM; ",
                    "printf ready > \"$2\"; ",
                    "while [ -d \"$1\" ]; do sleep 0.01; done' escaped \"$1\" \"$2\" & ",
                    "while [ ! -e \"$2\" ]; do sleep 0.01; done; ",
                    "printf 'escaped:%s\\n' \"$!\""
                )
                .to_owned(),
                "pohunek-escaped-output".to_owned(),
                barrier.path().to_string_lossy().into_owned(),
                ready.to_string_lossy().into_owned(),
            ],
            base: EnvBase::Empty,
            env: crate::test_support::child_env(),
            cwd: barrier.path().to_path_buf(),
            cols: 80,
            rows: 24,
        });
        pohunek_test_support::wait::guard("the root to exit", pty.wait_exit())
            .await
            .expect("root exit");
        assert!(!pty.output().observe(None, 1).expect("output page").exited);

        // The escaped holder never closes the PTY, so the outcome is a forced close
        // whatever the grace is; the short grace only keeps the test fast, and the
        // hang guard replaces a wall-clock bound on the stop.
        let error = pohunek_test_support::wait::guard(
            "stop to force-close the escaped output",
            pty.stop("stop-escaped-descendant", Duration::from_millis(50)),
        )
        .await
        .expect_err("escaped PTY holder requires forced close");
        assert!(matches!(error, PtyError::OutputForcedClosed));
        assert!(matches!(
            pty.stop("stop-escaped-descendant-again", Duration::from_millis(50))
                .await,
            Err(PtyError::OutputForcedClosed)
        ));
        assert!(pty.output_forced_closed());
        assert!(pty.output().observe(None, 1).expect("output page").exited);
        assert!(
            read_process_start(pty.identity().pid).is_err(),
            "root must be reaped after the final safe group signal"
        );
    }

    // Needs a descendant that keeps the PTY open after the root exits. Darwin
    // revokes the controlling terminal from every holder when the session
    // leader exits, so this scenario exists only on Linux.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn natural_eof_wins_a_concurrent_forced_cleanup() {
        use super::CleanupState;
        use crate::output::OutputCompletion;

        let barrier = pohunek_test_support::tempdir().expect("create EOF race barrier");
        let ready = barrier.path().join("ready");
        let pty = spawn(Command {
            program: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                concat!(
                    "trap '' HUP; ",
                    "setsid sh -c 'trap \"\" HUP TERM; ",
                    "printf ready > \"$2\"; ",
                    "while [ -d \"$1\" ]; do sleep 0.01; done' escaped \"$1\" \"$2\" & ",
                    "while [ ! -e \"$2\" ]; do sleep 0.01; done"
                )
                .to_owned(),
                "pohunek-eof-race".to_owned(),
                barrier.path().to_string_lossy().into_owned(),
                ready.to_string_lossy().into_owned(),
            ],
            base: EnvBase::Empty,
            env: crate::test_support::child_env(),
            cwd: barrier.path().to_path_buf(),
            cols: 80,
            rows: 24,
        });
        let root_exit = pohunek_test_support::wait::guard("the root to exit", pty.wait_exit())
            .await
            .expect("root exit");
        pty.output().mark_exit();

        let mut cleanup = pty.cleanup.lock().await;
        *cleanup = CleanupState::ForcedClosing;
        let completion = pty
            .seal_and_release_output(&mut cleanup)
            .expect("seal raced output");
        assert!(matches!(completion, OutputCompletion::Eof { .. }));
        let finished = pty
            .finish_sealed_cleanup(&mut cleanup, completion)
            .await
            .expect("finish natural winner");
        drop(cleanup);

        assert_eq!(finished, root_exit);
        assert!(!pty.output_forced_closed());
        assert_eq!(
            pty.stop("repeat-after-natural-winner", stop_grace())
                .await
                .expect("repeat stop"),
            root_exit
        );
    }

    /// Cancellation wins over output, and hangup still delivers buffered bytes.
    ///
    /// The hangup rule is the one that matters: a child that writes and exits
    /// wakes with `IN | HUP` together, and reporting that as end-of-stream
    /// would drop its final output.
    #[test]
    fn readiness_classification_covers_every_documented_wake() {
        use super::{classify_readiness, OutputReadyState, PollFlags, ReadinessFault};

        let quiet = PollFlags::empty();
        assert_eq!(classify_readiness(quiet, quiet), Ok(None), "timeout");
        assert_eq!(
            classify_readiness(PollFlags::IN, quiet),
            Ok(Some(OutputReadyState::Output)),
            "readable master"
        );
        assert_eq!(
            classify_readiness(PollFlags::HUP, quiet),
            Ok(Some(OutputReadyState::Output)),
            "hangup still sends the caller to read, which is where EOF is seen"
        );
        assert_eq!(
            classify_readiness(PollFlags::IN | PollFlags::HUP, quiet),
            Ok(Some(OutputReadyState::Output)),
            "hangup arriving with buffered bytes must still drain them"
        );
        assert_eq!(
            classify_readiness(PollFlags::ERR, quiet),
            Ok(Some(OutputReadyState::Output)),
            "error readiness is surfaced by the read, not swallowed here"
        );
        assert_eq!(
            classify_readiness(quiet, PollFlags::IN),
            Ok(Some(OutputReadyState::Cancelled)),
            "cancellation"
        );
        assert_eq!(
            classify_readiness(PollFlags::IN, PollFlags::IN),
            Ok(Some(OutputReadyState::Cancelled)),
            "a caller that asked to stop must not be made to drain first"
        );
        for (master, cancel, what) in [
            (PollFlags::NVAL, quiet, "master"),
            (quiet, PollFlags::NVAL, "cancel pipe"),
            (
                PollFlags::IN,
                PollFlags::NVAL,
                "cancel pipe beside a readable master",
            ),
        ] {
            assert_eq!(
                classify_readiness(master, cancel),
                Err(ReadinessFault::InvalidDescriptor),
                "a closed {what} must fail rather than spin"
            );
        }
    }

    /// Cancellation is a latch that every later wait observes.
    ///
    /// The reader thread, `resize` and `attach_snapshot` share one readiness,
    /// so the first waiter must not consume the signal the others rely on.
    #[test]
    fn cancellation_stays_armed_for_every_later_wait() {
        use std::io::Write as _;
        use std::os::fd::AsFd as _;

        let (master, mut peer) = std::os::unix::net::UnixStream::pair().expect("readiness fixture");
        let readiness = super::OutputReadiness::new(master.as_fd()).expect("readiness");
        assert_eq!(
            readiness.ready(super::NO_WAIT_MS).expect("unarmed wait"),
            None,
            "a quiet, unarmed readiness times out"
        );

        readiness.cancel().expect("arm cancellation");
        for wait in 0..3 {
            assert_eq!(
                readiness.ready(super::NO_WAIT_MS).expect("armed wait"),
                Some(OutputReadyState::Cancelled),
                "wait {wait} after arming must still see the cancellation"
            );
        }
        assert_eq!(
            super::wait_for_output(&readiness).expect("blocking wait"),
            OutputReadyState::Cancelled,
            "a blocking wait must return at once on an armed latch"
        );

        peer.write_all(b"late output")
            .expect("write fixture output");
        readiness.cancel().expect("re-arm cancellation");
        assert_eq!(
            readiness
                .ready(super::NO_WAIT_MS)
                .expect("wait beside output"),
            Some(OutputReadyState::Cancelled),
            "re-arming is harmless and the latch still beats pending output"
        );
    }

    /// Arming past the pipe's capacity neither blocks nor fails.
    ///
    /// Every forced-close path arms the latch, and a caller must never stall
    /// there. The write end is non-blocking and a full pipe already carries
    /// the cancellation, so `AGAIN` counts as armed.
    #[test]
    fn arming_a_full_cancellation_pipe_does_not_block() {
        use std::os::fd::AsFd as _;

        /// Well past any pipe buffer on Linux (64 KiB) or Darwin (up to 64 KiB).
        const ARMINGS: usize = 256 * 1024;

        let (master, _peer) = std::os::unix::net::UnixStream::pair().expect("readiness fixture");
        let readiness = super::OutputReadiness::new(master.as_fd()).expect("readiness");
        for arming in 0..ARMINGS {
            readiness
                .cancel()
                .unwrap_or_else(|error| panic!("arming {arming} failed: {error}"));
        }
        assert_eq!(
            readiness.ready(super::NO_WAIT_MS).expect("armed wait"),
            Some(OutputReadyState::Cancelled)
        );
    }

    /// The readiness keeps its own close-on-exec descriptors open.
    ///
    /// Waiting must not depend on whoever handed the master in, so closing
    /// the original leaves the readiness polling a live descriptor. None of
    /// its descriptors may leak into a PTY child.
    #[test]
    fn readiness_outlives_the_descriptor_it_was_built_from() {
        use rustix::io::{fcntl_getfd, FdFlags};
        use std::io::Write as _;
        use std::os::fd::AsFd as _;

        let (master, mut peer) = std::os::unix::net::UnixStream::pair().expect("readiness fixture");
        let readiness = super::OutputReadiness::new(master.as_fd()).expect("readiness");
        drop(master);

        for (descriptor, what) in [
            (&readiness.master, "master duplicate"),
            (&readiness.cancel_reader, "cancel pipe read end"),
            (&readiness.cancel_writer, "cancel pipe write end"),
        ] {
            assert!(
                fcntl_getfd(descriptor)
                    .expect("descriptor flags")
                    .contains(FdFlags::CLOEXEC),
                "the {what} must be close-on-exec"
            );
        }

        assert_eq!(
            readiness.ready(super::NO_WAIT_MS).expect("quiet wait"),
            None,
            "the duplicate is open and quiet, not reported as invalid"
        );
        peer.write_all(b"output")
            .expect("the duplicate keeps the fixture connected");
        assert_eq!(
            readiness.ready(super::NO_WAIT_MS).expect("readable wait"),
            Some(OutputReadyState::Output)
        );
    }

    /// A forced close fails every snapshot-boundary operation after it.
    ///
    /// The reader thread is joined first, so it has already woken on the
    /// cancellation before `resize` and `attach_snapshot` consult the same
    /// readiness. Neither may report success on a force-closed PTY.
    #[tokio::test]
    async fn forced_close_fails_resize_and_snapshot_after_the_reader_has_woken() {
        let cwd = crate::test_support::child_cwd();
        let pty = spawn(shell("sleep 30", cwd.path()));
        pty.force_output_close().expect("force output close");

        let reader = pty
            .reader_thread
            .lock()
            .expect("reader thread lock")
            .take()
            .expect("reader thread handle");
        pohunek_test_support::wait::guard(
            "the output reader thread to exit",
            tokio::task::spawn_blocking(move || reader.join()),
        )
        .await
        .expect("reader join task")
        .expect("reader thread must exit cleanly");

        assert!(matches!(
            pty.resize("attach-1", 1, 100, 40).await,
            Err(PtyError::OutputForcedClosed)
        ));
        assert!(matches!(
            pty.attach_snapshot(Some((100, 40))).await,
            Err(PtyError::OutputForcedClosed)
        ));
        assert!(matches!(
            pty.attach_snapshot(None).await,
            Err(PtyError::OutputForcedClosed)
        ));
        assert_eq!(
            pty.dimensions().await,
            (80, 24),
            "a refused resize must not commit its dimensions"
        );

        let exit = pty
            .stop("cleanup", stop_grace())
            .await
            .expect("stop still reaps the root after a forced output close");
        assert!(!exit.success, "the root was signalled, not left to finish");
    }

    #[tokio::test]
    async fn resize_ignores_duplicate_and_older_source_sequences() {
        let cwd = crate::test_support::child_cwd();
        let pty = spawn(shell("sleep 30", cwd.path()));

        assert!(pty.resize("attach-1", 2, 100, 40).await.expect("resize"));
        assert!(!pty.resize("attach-1", 2, 80, 24).await.expect("duplicate"));
        assert!(!pty.resize("attach-1", 1, 80, 24).await.expect("older"));
        assert_eq!(pty.dimensions().await, (100, 40));

        let _ = pty.stop("cleanup", stop_grace()).await.expect("cleanup");
    }

    #[tokio::test]
    async fn snapshot_includes_output_ready_before_the_ordering_gate() {
        let readiness = TestReadiness::new([true, false]);
        let reader = Mutex::new(Box::new(Cursor::new(
            b"\x1b[2J\x1b[Hstable-before-snapshot".to_vec(),
        )) as Box<dyn std::io::Read + Send>);
        let output = OutputHub::new(64 * 1024, 64 * 1024, 24, 80).expect("output hub");

        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        assert_eq!(
            drain_available_output(
                &reader,
                &readiness,
                &output,
                &mut buffer,
                OUTPUT_DRAIN_BATCH_BYTES,
            )
            .expect("drain ready output"),
            OutputReadState::Open
        );
        output.resize(30, 100);
        let mut subscriber = output.subscribe_terminal_snapshot();

        let mut repaint = Vec::new();
        loop {
            let event = subscriber.recv().await.expect("snapshot event");
            let OutputEvent::TerminalSnapshot(chunk) = event else {
                panic!("snapshot subscriber emitted a non-snapshot seed");
            };
            repaint.extend_from_slice(&chunk.bytes);
            if repaint.len() == chunk.total_bytes {
                break;
            }
        }
        assert!(
            String::from_utf8_lossy(&repaint).contains("stable-before-snapshot"),
            "ready bytes must be parsed before the snapshot is captured"
        );
    }

    #[tokio::test]
    async fn resize_and_snapshot_complete_during_continuous_output() {
        let cwd = crate::test_support::child_cwd();
        let pty = spawn(shell("yes continuous-output", cwd.path()));
        pohunek_test_support::wait::wait_until("continuous output to start", || async {
            (pty.output().next_offset() >= READ_CHUNK_BYTES as u64).then_some(())
        })
        .await;

        // A starved resize or snapshot never completes while output keeps
        // flowing, which the hang guard reports by name.
        pohunek_test_support::wait::guard(
            "resize to complete during continuous output",
            pty.resize("attach-noisy", 1, 100, 30),
        )
        .await
        .expect("resize");
        let (subscriber, dimensions) = pohunek_test_support::wait::guard(
            "snapshot to complete during continuous output",
            pty.attach_snapshot(Some((90, 28))),
        )
        .await
        .expect("snapshot");
        assert_eq!(dimensions, (90, 28));
        drop(subscriber);

        let _ = pty
            .stop("cleanup-noisy", stop_grace())
            .await
            .expect("cleanup");
    }

    /// The master is non-blocking for every descriptor the owner reads through.
    #[tokio::test]
    async fn master_description_is_non_blocking() {
        let cwd = crate::test_support::child_cwd();
        let pty = spawn(shell("sleep 30", cwd.path()));

        let flags = rustix::fs::fcntl_getfl(&pty.output_readiness.master).expect("master flags");
        assert!(
            flags.contains(rustix::fs::OFlags::NONBLOCK),
            "a blocking master read can park while holding the ordering gate"
        );

        let _ = pty.stop("cleanup", stop_grace()).await.expect("cleanup");
    }

    /// A read whose readiness lapsed ends the drain instead of parking.
    ///
    /// Darwin reports a stopped terminal's queue as unreadable only after the
    /// stop lands, so an output pause racing the reader's readiness check
    /// leaves nothing to read. An idle Linux PTY reproduces that state by
    /// reporting readiness that the real master does not have.
    #[tokio::test]
    async fn drain_ends_when_readiness_lapses_before_the_read() {
        let cwd = crate::test_support::child_cwd();
        let pty = spawn(shell("sleep 30", cwd.path()));
        let reader = std::sync::Arc::clone(&pty.output_reader);
        let order = std::sync::Arc::clone(&pty.output_order);
        let output = pty.output().clone();

        let drain = tokio::task::spawn_blocking(move || {
            let _order = order.lock().expect("ordering gate");
            let readiness = TestReadiness::new([true]);
            let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
            drain_available_output(
                &reader,
                &readiness,
                &output,
                &mut buffer,
                OUTPUT_DRAIN_BATCH_BYTES,
            )
        });
        let state = tokio::time::timeout(LAPSED_READ_DEADLINE, drain)
            .await
            .expect("a lapsed read must not park with the ordering gate held")
            .expect("drain task")
            .expect("drain");
        assert_eq!(state, OutputReadState::Lapsed);

        let _ = pty.stop("cleanup", stop_grace()).await.expect("cleanup");
    }

    /// Input larger than the kernel's PTY queues is written in full.
    #[tokio::test]
    async fn input_beyond_the_pty_queue_waits_for_room() {
        let script =
            format!("stty raw -echo && printf ready && head -c {OVERSIZED_INPUT_BYTES} >/dev/null");
        let cwd = crate::test_support::child_cwd();
        let pty = spawn(shell(&script, cwd.path()));
        let mut output = pty.subscribe_output(None).expect("subscribe");
        tokio::time::timeout(OVERSIZED_INPUT_DEADLINE, async {
            let mut observed = Vec::new();
            while !String::from_utf8_lossy(&observed).contains("ready") {
                match output.recv().await.expect("output before readiness") {
                    OutputEvent::Replay(chunk) | OutputEvent::Output(chunk) => {
                        observed.extend_from_slice(&chunk.bytes);
                    }
                    OutputEvent::Exit { .. } => panic!("child exited before readiness"),
                    OutputEvent::Gap { .. } | OutputEvent::TerminalSnapshot(_) => {}
                }
            }
        })
        .await
        .expect("raw-mode readiness");

        tokio::time::timeout(
            OVERSIZED_INPUT_DEADLINE,
            pty.input().execute_stream(vec![InputFragment {
                bytes: vec![b'x'; OVERSIZED_INPUT_BYTES],
                delay_after: Duration::ZERO,
            }]),
        )
        .await
        .expect("input deadline")
        .expect("a full input queue waits for room instead of failing");
        let exit = tokio::time::timeout(OVERSIZED_INPUT_DEADLINE, pty.wait_exit())
            .await
            .expect("exit deadline")
            .expect("exit");
        assert_eq!(exit.exit_code, Some(0), "the child consumed every byte");
    }

    #[test]
    fn drain_available_output_stops_at_the_byte_budget() {
        let readiness = TestReadiness::new(std::iter::repeat_n(
            true,
            OUTPUT_DRAIN_BATCH_BYTES / READ_CHUNK_BYTES + 1,
        ));
        let reader = Mutex::new(Box::new(Cursor::new(vec![
            b'x';
            OUTPUT_DRAIN_BATCH_BYTES
                + READ_CHUNK_BYTES
        ])) as Box<dyn std::io::Read + Send>);
        let output = OutputHub::new(64 * 1024, 64 * 1024, 24, 80).expect("output hub");

        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        assert_eq!(
            drain_available_output(
                &reader,
                &readiness,
                &output,
                &mut buffer,
                OUTPUT_DRAIN_BATCH_BYTES,
            )
            .expect("drain the bounded batch"),
            OutputReadState::BudgetExhausted
        );
        assert_eq!(output.next_offset(), OUTPUT_DRAIN_BATCH_BYTES as u64);
    }

    #[test]
    fn queued_output_beyond_the_budget_prevents_resize_state_commit() {
        let readiness = TestReadiness::new([true, true]);
        let reader = Mutex::new(Box::new(Cursor::new(vec![b'x'; READ_CHUNK_BYTES * 2]))
            as Box<dyn std::io::Read + Send>);
        let output = OutputHub::new(64 * 1024, 64 * 1024, 24, 80).expect("output hub");
        let mut state = ResizeState {
            cols: 80,
            rows: 24,
            sequences: HashMap::new(),
        };
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        let mut committed = false;

        let error = drain_snapshot_boundary(
            &reader,
            &readiness,
            &FakeTerminal::holding(b""),
            &output,
            &mut buffer,
            test_drain(READ_CHUNK_BYTES),
        )
        .and_then(|()| {
            committed = true;
            commit_resize_before_resume(
                &mut state,
                ResizeCommit::Attach { cols: 90, rows: 28 },
                || Ok(()),
            )
        })
        .expect_err("queued bytes beyond the budget must reject the atomic operation");

        assert!(matches!(error, PtyError::OutputDrainLimit));
        assert!(!committed);
        assert_eq!((state.cols, state.rows), (80, 24));
        assert!(state.sequences.is_empty());
    }

    #[test]
    fn resize_state_commits_before_resume_failure() {
        let mut state = ResizeState {
            cols: 80,
            rows: 24,
            sequences: HashMap::new(),
        };

        let error = commit_resize_before_resume(
            &mut state,
            ResizeCommit::Resize {
                source_id: "attach-1",
                source_sequence: 7,
                cols: 100,
                rows: 40,
            },
            || Err(PtyError::Task),
        )
        .expect_err("injected resume failure");

        assert!(matches!(error, PtyError::Task));
        assert_eq!((state.cols, state.rows), (100, 40));
        assert_eq!(state.sequences.get("attach-1"), Some(&7));
    }

    #[test]
    fn attach_resize_state_commits_before_resume_failure() {
        let mut state = ResizeState {
            cols: 80,
            rows: 24,
            sequences: HashMap::new(),
        };

        let error = commit_resize_before_resume(
            &mut state,
            ResizeCommit::Attach { cols: 90, rows: 28 },
            || Err(PtyError::Task),
        )
        .expect_err("injected resume failure");

        assert!(matches!(error, PtyError::Task));
        assert_eq!((state.cols, state.rows), (90, 28));
        assert!(state.sequences.is_empty());
    }

    #[test]
    fn command_debug_redacts_environment_values() {
        let secret = "seeded-secret-environment";
        let mut command = shell("true", std::path::Path::new("/"));
        command.env.push(("SECRET".to_owned(), secret.to_owned()));
        command.args.push(secret.to_owned());

        let rendered = format!("{command:?}");
        assert!(rendered.contains("[REDACTED"));
        assert!(!rendered.contains(secret));
    }

    #[tokio::test]
    async fn spawn_guard_kills_the_uncommitted_process_group() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("allocate rollback PTY");
        let cwd = crate::test_support::child_cwd();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "trap '' HUP TERM; sleep 30 & wait"]);
        command.env_clear();
        command.env("PATH", crate::test_support::CHILD_PATH);
        command.cwd(cwd.path());
        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn rollback fixture");
        let pid = child.process_id().expect("fixture child PID");
        let process_group = i32::try_from(pid).expect("PID fits pid_t");
        let mut guard = SpawnGuard::new(child);
        guard.set_process_group(process_group);
        drop(pair.slave);

        let inspector = HostInspector::new();
        let root = inspector
            .identity(pid)
            .expect("inspect rollback root")
            .expect("rollback root remains live");
        let ready_deadline = Instant::now() + ROLLBACK_CHILD_READY_TIMEOUT;
        let descendant = loop {
            let descendants = inspector
                .descendant_identities(root)
                .expect("inspect rollback descendants");
            if let Some(descendant) = descendants.into_iter().next() {
                break descendant;
            }
            assert!(
                Instant::now() < ready_deadline,
                "rollback fixture did not create its descendant"
            );
            tokio::time::sleep(ROLLBACK_CHILD_READY_POLL).await;
        };
        assert_eq!(
            inspector
                .process(descendant.pid)
                .expect("inspect rollback descendant")
                .expect("rollback descendant remains live")
                .pgid,
            u32::try_from(process_group).expect("positive process group"),
            "rollback descendant did not join the owned process group"
        );
        let root_exit = inspector.exit_watch(root).expect("watch rollback root");
        let descendant_exit = inspector
            .exit_watch(descendant)
            .expect("watch rollback descendant");
        drop(guard);

        tokio::time::timeout(ROLLBACK_EXIT_TIMEOUT, async {
            tokio::try_join!(root_exit.wait(), descendant_exit.wait())
        })
        .await
        .expect("rollback did not terminate the exact process generations")
        .expect("rollback exit watch failed");
    }

    #[test]
    fn startup_abort_releases_reader_without_waiting_for_pty_eof() {
        let startup = StartupLatch::default();
        let waiter = startup.clone();
        let reader = std::thread::spawn(move || waiter.wait());

        startup.abort();

        assert!(!reader.join().expect("startup waiter"));
    }
}
