//! Owns one PTY master and its managed process identity.

// Rust guideline compliant 2026-07-28

use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{watch, Mutex as AsyncMutex};
use tracing::{event, Level};

use crate::{OutputHub, OutputSubscriber, RingError, WriteCoordinator};

/// Blocking PTY read size.
///
/// Eight KiB bounds temporary allocations while keeping terminal repaint
/// throughput efficient.
const READ_CHUNK_BYTES: usize = 8 * 1024;
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
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|source| PtyError::Allocate(source.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|source| PtyError::Allocate(source.to_string()))?;
        let input = WriteCoordinator::new(writer, input_dedup_entries)
            .map_err(|source| PtyError::Io(io::Error::other(source)))?;
        let output = OutputHub::new(history_bytes, subscriber_bytes, command.rows, command.cols)?;

        let output_reader = output.clone();
        let reader_pid = pid;
        let reader_thread = thread::Builder::new()
            .name(format!("pohunek-worker-pty-{pid}"))
            .spawn(move || {
                let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if let Err(error) = output_reader.push(&buffer[..read]) {
                                event!(
                                    name: "worker.output.fault",
                                    Level::ERROR,
                                    process.pid = reader_pid,
                                    error.type = "output_offset",
                                    error.message = %error,
                                    "worker output fault for {{process.pid}}: {{error.message}}",
                                );
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
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
                output_reader.mark_exit();
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
    /// Returns [`PtyError`] for invalid dimensions or PTY I/O failure.
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
        tokio::task::spawn_blocking(move || {
            let master = master.lock().map_err(|_poison| PtyError::Poisoned)?;
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|source| PtyError::Io(io::Error::other(source)))
        })
        .await
        .map_err(|_join_error| PtyError::Task)??;
        resize
            .sequences
            .insert(source_id.to_owned(), source_sequence);
        resize.cols = cols;
        resize.rows = rows;
        self.output.resize(rows, cols);
        Ok(true)
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
    use super::{Command, PtyOwner};
    use crate::{InputFragment, InputPlan, OutputEvent, WorkerConfig};
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
