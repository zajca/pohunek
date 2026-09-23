//! Owns one PTY master and its managed process identity.

// Rust guideline compliant 2026-09-23

use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use pohunek_platform::process::{HostInspector, ProcessInspector};
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

/// Command launched inside one PTY.
#[derive(Clone, PartialEq, Eq)]
pub struct Command {
    /// Resolved executable.
    pub program: String,
    /// Executable arguments.
    pub args: Vec<String>,
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
    /// Linux `/proc` start-time field.
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
        // `CommandBuilder::new` captures this worker process's own environment as
        // the child's base. Strip every ambient `POHUNEK_*` marker from that base
        // so the child carries only the worker-authoritative identity set below
        // from `command.env` (RFC §11.5 environment sanitization; §17.5 removes
        // daemon id from ownership). Without this, a daemon that itself runs
        // inside another pohunek session leaks that ancestor's `POHUNEK_DAEMON_ID`
        // into the agent, and procwatch then mis-attributes the agent as
        // foreign-owned. `vars_os` avoids panicking on any non-UTF-8 sibling var.
        for name in std::env::vars_os() {
            if let Some(name) = name.0.to_str() {
                if name.starts_with("POHUNEK_") {
                    builder.env_remove(name);
                }
            }
        }
        for (key, value) in &command.env {
            builder.env(key, value);
        }
        builder.cwd(&command.cwd);

        let tty_name = Arc::new(pair.master.tty_name().ok_or(PtyError::MissingTtyName)?);
        let output_readiness = Arc::new(OutputReadiness::new(borrow_master(&*pair.master)?)?);
        let output_reader = Arc::new(Mutex::new(
            pair.master
                .try_clone_reader()
                .map_err(|source| PtyError::Allocate(source.to_string()))?,
        ));
        let writer = pair
            .master
            .take_writer()
            .map_err(|source| PtyError::Allocate(source.to_string()))?;
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
                        Ok(OutputReadState::Open | OutputReadState::BudgetExhausted) => {}
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
                &output,
                &mut buffer,
                OUTPUT_DRAIN_BATCH_BYTES,
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
                &output,
                &mut buffer,
                OUTPUT_DRAIN_BATCH_BYTES,
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
    Open,
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

#[derive(Debug)]
struct OutputPause {
    tty: OwnedFd,
    resumed: bool,
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
            Err(error) => return Err(PtyError::Io(error)),
        }
    }
}

fn drain_snapshot_boundary<R>(
    reader: &Mutex<Box<dyn Read + Send>>,
    readiness: &R,
    output: &OutputHub,
    buffer: &mut [u8],
    byte_budget: usize,
) -> Result<(), PtyError>
where
    R: OutputReady + ?Sized,
{
    match drain_available_output(reader, readiness, output, buffer, byte_budget)? {
        OutputReadState::Open => Ok(()),
        OutputReadState::Eof => {
            output.mark_exit();
            Err(PtyError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PTY closed before atomic resize or snapshot",
            )))
        }
        OutputReadState::Cancelled => Err(PtyError::OutputForcedClosed),
        OutputReadState::BudgetExhausted => Err(PtyError::OutputDrainLimit),
    }
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
        read_process_start, wait_for_child_status, CleanupState, Command, OutputReadState,
        OutputReady, OutputReadyState, PtyError, PtyOwner, ResizeCommit, ResizeState, SpawnGuard,
        StartupLatch, OUTPUT_DRAIN_BATCH_BYTES, READ_CHUNK_BYTES,
    };
    use crate::output::OutputCompletion;
    use crate::{InputFragment, InputPlan, OutputEvent, OutputHub, WorkerConfig};
    use pohunek_platform::process::{HostInspector, ProcessInspector};
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::collections::{HashMap, VecDeque};
    use std::io::Cursor;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Bounds observation of the rollback fixture's signal-resistant descendant.
    const ROLLBACK_CHILD_READY_TIMEOUT: Duration = Duration::from_secs(2);
    /// Avoids busy-spinning while the rollback fixture creates its descendant.
    const ROLLBACK_CHILD_READY_POLL: Duration = Duration::from_millis(10);
    /// Bounds delivery of the rollback kill to both exact process generations.
    const ROLLBACK_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

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

    fn shell(script: &str) -> Command {
        Command {
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), script.to_owned()],
            env: Vec::new(),
            cwd: std::env::temp_dir(),
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

    #[tokio::test]
    async fn real_pty_drains_output_and_reports_exit() {
        let pty = spawn(shell("printf 'worker-ready'; exit 7"));
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

    #[tokio::test]
    async fn real_pty_accepts_deduplicated_input_and_stops_group() {
        let pty = spawn(shell("read line; printf 'got:%s' \"$line\"; sleep 30"));
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
        let exit = pty
            .stop("stop-1", Duration::from_millis(200))
            .await
            .expect("stop");

        assert!(!exit.success);
        assert_eq!(
            pty.stop("stop-1", Duration::from_millis(200))
                .await
                .expect("duplicate stop"),
            exit
        );
    }

    #[tokio::test]
    async fn stop_terminates_descendants_after_root_exit_without_waiting_for_eof() {
        let pty = spawn(shell(concat!(
            "trap '' HUP; ",
            "sh -c 'trap \"\" HUP TERM; while :; do sleep 1; done' & ",
            "printf 'descendant:%s\\n' \"$!\""
        )));
        let mut output = pty.subscribe_output(Some(0)).expect("subscribe output");
        let root_exit = tokio::time::timeout(Duration::from_secs(2), pty.wait_exit())
            .await
            .expect("root exit deadline")
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

        let started = Instant::now();
        let stopped = tokio::time::timeout(
            Duration::from_secs(1),
            pty.stop("stop-after-root-exit", Duration::from_millis(50)),
        )
        .await
        .expect("bounded stop deadline")
        .expect("stop descendant group");
        assert_eq!(stopped, root_exit);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "stop must not wait indefinitely for descendant-held PTY EOF"
        );

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

    #[tokio::test]
    async fn stop_force_closes_output_retained_outside_the_owned_process_group() {
        let barrier = tempfile::tempdir().expect("create escaped descendant barrier");
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
            env: Vec::new(),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
        });
        tokio::time::timeout(Duration::from_secs(2), pty.wait_exit())
            .await
            .expect("root exit deadline")
            .expect("root exit");
        assert!(!pty.output().observe(None, 1).expect("output page").exited);

        let error = tokio::time::timeout(
            Duration::from_secs(1),
            pty.stop("stop-escaped-descendant", Duration::from_millis(50)),
        )
        .await
        .expect("bounded forced-close deadline")
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

    #[tokio::test]
    async fn natural_eof_wins_a_concurrent_forced_cleanup() {
        let barrier = tempfile::tempdir().expect("create EOF race barrier");
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
            env: Vec::new(),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
        });
        let root_exit = tokio::time::timeout(Duration::from_secs(2), pty.wait_exit())
            .await
            .expect("root exit deadline")
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
            pty.stop("repeat-after-natural-winner", Duration::from_millis(50))
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
        let pty = spawn(shell("sleep 30"));
        pty.force_output_close().expect("force output close");

        let reader = pty
            .reader_thread
            .lock()
            .expect("reader thread lock")
            .take()
            .expect("reader thread handle");
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || reader.join()),
        )
        .await
        .expect("reader exit deadline")
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
            .stop("cleanup", Duration::from_millis(200))
            .await
            .expect("stop still reaps the root after a forced output close");
        assert!(!exit.success, "the root was signalled, not left to finish");
    }

    #[tokio::test]
    async fn resize_ignores_duplicate_and_older_source_sequences() {
        let pty = spawn(shell("sleep 30"));

        assert!(pty.resize("attach-1", 2, 100, 40).await.expect("resize"));
        assert!(!pty.resize("attach-1", 2, 80, 24).await.expect("duplicate"));
        assert!(!pty.resize("attach-1", 1, 80, 24).await.expect("older"));
        assert_eq!(pty.dimensions().await, (100, 40));

        let _ = pty
            .stop("cleanup", Duration::from_millis(200))
            .await
            .expect("cleanup");
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
        let pty = spawn(shell("yes continuous-output"));
        tokio::time::timeout(Duration::from_secs(2), async {
            while pty.output().next_offset() < READ_CHUNK_BYTES as u64 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("continuous output must start");

        tokio::time::timeout(
            Duration::from_secs(2),
            pty.resize("attach-noisy", 1, 100, 30),
        )
        .await
        .expect("resize must not starve")
        .expect("resize");
        let (subscriber, dimensions) =
            tokio::time::timeout(Duration::from_secs(2), pty.attach_snapshot(Some((90, 28))))
                .await
                .expect("snapshot must not starve")
                .expect("snapshot");
        assert_eq!(dimensions, (90, 28));
        drop(subscriber);

        let _ = pty
            .stop("cleanup-noisy", Duration::from_millis(200))
            .await
            .expect("cleanup");
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

        let error =
            drain_snapshot_boundary(&reader, &readiness, &output, &mut buffer, READ_CHUNK_BYTES)
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
        let mut command = shell("true");
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
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "trap '' HUP TERM; sleep 30 & wait"]);
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
