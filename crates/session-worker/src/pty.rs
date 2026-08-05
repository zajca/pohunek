//! Owns one PTY master and its managed process identity.

// Rust guideline compliant 2026-07-29

use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::io::{self, Read};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::epoll::{self, EpollCreateFlags, EpollEvent, EpollFlags, EpollOp};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::{close, Pid};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use rustix::fd::OwnedFd;
use rustix::fs::{open, Mode, OFlags};
use rustix::termios::{tcflow, Action};
use tokio::sync::{watch, Mutex as AsyncMutex};
use tracing::{event, Level};

use crate::{OutputHub, OutputSubscriber, RingError, WriteCoordinator};

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
/// Blocking and non-blocking epoll timeout values.
///
/// The raw epoll API uses negative one for an indefinite wait and zero for an
/// immediate readiness check.
const EPOLL_WAIT_FOREVER_MS: isize = -1;
const EPOLL_NO_WAIT_MS: isize = 0;
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
    exit_rx: watch::Receiver<Option<Exit>>,
    output: OutputHub,
    output_order: Arc<Mutex<()>>,
    output_reader: Arc<Mutex<Box<dyn Read + Send>>>,
    output_readiness: Arc<OutputReadiness>,
    tty_name: Arc<PathBuf>,
    input: WriteCoordinator,
    resize: Arc<AsyncMutex<ResizeState>>,
    stop: Arc<AsyncMutex<Option<(String, Exit)>>>,
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

        let mut child = pair.slave.spawn_command(builder).map_err(|_source| {
            let program = Path::new(&command.program);
            PtyError::Spawn {
                message: "process launch failed".to_owned(),
                not_found: program.is_absolute() && !program.exists(),
            }
        })?;
        let pid = child.process_id().ok_or(PtyError::MissingPid)?;
        let process_group =
            pair.master
                .process_group_leader()
                .unwrap_or(i32::try_from(pid).map_err(|_range_error| {
                    PtyError::ProcessIdentity {
                        pid,
                        source: io::Error::new(
                            io::ErrorKind::InvalidData,
                            "PID exceeds pid_t range",
                        ),
                    }
                })?);
        let start_identity =
            read_process_start(pid).map_err(|source| PtyError::ProcessIdentity { pid, source })?;
        let identity = ProcessIdentity {
            pid,
            process_group,
            start_identity,
        };
        let tty_name = Arc::new(pair.master.tty_name().ok_or(PtyError::MissingTtyName)?);
        drop(pair.slave);

        let master_fd = pair.master.as_raw_fd().ok_or(PtyError::MissingMasterFd)?;
        let output_readiness = Arc::new(OutputReadiness::new(master_fd)?);
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

        let reader_output = output.clone();
        let reader_output_order = Arc::clone(&output_order);
        let thread_output_reader = Arc::clone(&output_reader);
        let thread_output_readiness = Arc::clone(&output_readiness);
        let reader_pid = pid;
        let reader_thread = thread::Builder::new()
            .name(format!("pohunek-worker-pty-{pid}"))
            .spawn(move || {
                let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
                loop {
                    if let Err(error) = wait_for_output(&*thread_output_readiness) {
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
                        Ok(OutputReadState::Eof) => break,
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
        let child_thread = thread::Builder::new()
            .name(format!("pohunek-worker-child-{pid}"))
            .spawn(move || {
                let exit = match child.wait() {
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
                };
                let _ = exit_tx.send(Some(exit));
            })
            .map_err(PtyError::Io)?;

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
            stop: Arc::new(AsyncMutex::new(None)),
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
            let _order = output_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let output_pause = OutputPause::new(&tty_name)?;
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
            let _order = output_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let output_pause = OutputPause::new(&tty_name)?;
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
    pub async fn stop(&self, stop_id: &str, grace: Duration) -> Result<Exit, PtyError> {
        let mut stop = self.stop.lock().await;
        if let Some((_, exit)) = stop.as_ref() {
            return Ok(exit.clone());
        }
        let already_exited = self.exit_rx.borrow().clone();
        if let Some(exit) = already_exited {
            self.join_threads().await?;
            *stop = Some((stop_id.to_owned(), exit.clone()));
            return Ok(exit);
        }

        self.signal_group(Signal::SIGTERM)?;
        let exit = if let Ok(result) = tokio::time::timeout(grace, self.wait_exit()).await {
            result?
        } else {
            if self.exit_rx.borrow().is_none() {
                self.signal_group(Signal::SIGKILL)?;
            }
            tokio::time::timeout(grace, self.wait_exit())
                .await
                .map_err(|_elapsed| PtyError::ExitTimeout)??
        };
        self.join_threads().await?;
        *stop = Some((stop_id.to_owned(), exit.clone()));
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
            if let Some(exit) = receiver.borrow().clone() {
                return Ok(exit);
            }
            receiver
                .changed()
                .await
                .map_err(|_channel_closed| PtyError::ExitTimeout)?;
        }
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
}

#[derive(Debug)]
struct OutputReadiness {
    epoll_fd: RawFd,
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
    #[expect(
        deprecated,
        reason = "portable-pty exposes only RawFd; nix's typed Epoll API requires AsFd"
    )]
    fn new(master_fd: RawFd) -> Result<Self, PtyError> {
        let epoll_fd =
            epoll::epoll_create1(EpollCreateFlags::EPOLL_CLOEXEC).map_err(errno_to_pty_error)?;
        let mut event = EpollEvent::new(
            EpollFlags::EPOLLIN | EpollFlags::EPOLLHUP | EpollFlags::EPOLLERR,
            0,
        );
        if let Err(error) =
            epoll::epoll_ctl(epoll_fd, EpollOp::EpollCtlAdd, master_fd, Some(&mut event))
        {
            let _ = close(epoll_fd);
            return Err(errno_to_pty_error(error));
        }
        Ok(Self { epoll_fd })
    }

    #[expect(
        deprecated,
        reason = "portable-pty exposes only RawFd; nix's typed Epoll API requires AsFd"
    )]
    fn is_ready(&self, timeout_ms: isize) -> Result<bool, PtyError> {
        loop {
            let mut events = [EpollEvent::empty()];
            match epoll::epoll_wait(self.epoll_fd, &mut events, timeout_ms) {
                Ok(0) => return Ok(false),
                Ok(_) => return Ok(true),
                Err(Errno::EINTR) => {}
                Err(error) => return Err(errno_to_pty_error(error)),
            }
        }
    }
}

impl Drop for OutputReadiness {
    fn drop(&mut self) {
        let _ = close(self.epoll_fd);
    }
}

fn errno_to_pty_error(error: Errno) -> PtyError {
    PtyError::Io(io::Error::from_raw_os_error(error as i32))
}

fn rustix_errno_to_pty_error(error: rustix::io::Errno) -> PtyError {
    PtyError::Io(io::Error::from_raw_os_error(error.raw_os_error()))
}

trait OutputReady {
    fn is_ready(&self, timeout_ms: isize) -> Result<bool, PtyError>;
}

impl OutputReady for OutputReadiness {
    fn is_ready(&self, timeout_ms: isize) -> Result<bool, PtyError> {
        Self::is_ready(self, timeout_ms)
    }
}

fn wait_for_output(readiness: &impl OutputReady) -> Result<(), PtyError> {
    readiness.is_ready(EPOLL_WAIT_FOREVER_MS).map(|_ready| ())
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
            return readiness.is_ready(EPOLL_NO_WAIT_MS).map(|ready| {
                if ready {
                    OutputReadState::BudgetExhausted
                } else {
                    OutputReadState::Open
                }
            });
        }
        if !readiness.is_ready(EPOLL_NO_WAIT_MS)? {
            return Ok(OutputReadState::Open);
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
        OutputReadState::BudgetExhausted => Err(PtyError::OutputDrainLimit),
    }
}

fn read_process_start(pid: u32) -> Result<String, io::Error> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat.rfind(')').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "proc stat command field is unterminated",
        )
    })?;
    let start_identity = stat[close + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "proc stat has no start time"))?;
    Ok(start_identity.to_owned())
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
        commit_resize_before_resume, drain_available_output, drain_snapshot_boundary, Command,
        OutputReadState, OutputReady, PtyError, PtyOwner, ResizeCommit, ResizeState,
        OUTPUT_DRAIN_BATCH_BYTES, READ_CHUNK_BYTES,
    };
    use crate::{InputFragment, InputPlan, OutputEvent, OutputHub, WorkerConfig};
    use std::collections::{HashMap, VecDeque};
    use std::io::Cursor;
    use std::sync::Mutex;
    use std::time::Duration;

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
        fn is_ready(&self, _timeout_ms: isize) -> Result<bool, PtyError> {
            Ok(self
                .states
                .lock()
                .expect("test readiness lock")
                .pop_front()
                .unwrap_or(false))
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
}
