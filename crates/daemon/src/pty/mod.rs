//! PTY actor: spawn a command into a daemon-owned pseudo terminal.
//!
//! `portable-pty` exposes blocking readers, so every PTY gets one dedicated OS
//! thread that drains output and waits for process exit. Async callers interact
//! with the process through this cloneable handle.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, watch};
use tracing::{debug, warn};

/// Output chunk size for the blocking PTY reader.
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// Command to spawn in a PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyCommand {
    /// Program path or name.
    pub program: String,
    /// Program arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
}

/// Terminal process exit information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyExit {
    /// Exit code when available.
    pub exit_code: Option<i32>,
    /// Whether the process exited successfully.
    pub success: bool,
}

/// Errors raised by PTY setup or operations.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    /// Opening the PTY failed.
    #[error("failed to allocate PTY: {0}")]
    Allocate(String),
    /// Spawning the child command failed.
    #[error("failed to spawn PTY command: {0}")]
    Spawn(String),
    /// The child process has no OS pid.
    #[error("PTY child did not expose a process id")]
    MissingPid,
    /// PTY I/O failed.
    #[error("PTY io error: {0}")]
    Io(#[from] io::Error),
    /// A shared PTY handle was poisoned.
    #[error("PTY handle lock was poisoned")]
    Poisoned,
    /// The PTY reader thread panicked.
    #[error("PTY reader thread panicked")]
    ThreadPanicked,
    /// Exit was not observed before the timeout elapsed.
    #[error("timed out waiting for PTY process exit")]
    ExitTimeout,
}

/// Cloneable handle for a running PTY actor.
#[derive(Clone)]
pub struct PtyHandle {
    pid: u32,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    reader_thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    exit_rx: watch::Receiver<Option<PtyExit>>,
    output_tx: broadcast::Sender<Vec<u8>>,
}

impl std::fmt::Debug for PtyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyHandle")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl PtyHandle {
    /// Spawn a command in a new PTY.
    pub fn spawn(command: PtyCommand) -> Result<Self, PtyError> {
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
        builder.cwd(&command.cwd);

        let mut child = pair
            .slave
            .spawn_command(builder)
            .map_err(|source| PtyError::Spawn(source.to_string()))?;
        let pid = child.process_id().ok_or(PtyError::MissingPid)?;
        let killer = child.clone_killer();
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|source| PtyError::Allocate(source.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|source| PtyError::Allocate(source.to_string()))?;

        let master = Arc::new(Mutex::new(pair.master));
        let writer = Arc::new(Mutex::new(writer));
        let killer = Arc::new(Mutex::new(killer));
        let (output_tx, _) = broadcast::channel(64);
        let (exit_tx, exit_rx) = watch::channel(None);
        let output_tx_for_thread = output_tx.clone();

        let reader_thread = thread::Builder::new()
            .name(format!("zagentmesh-pty-{pid}"))
            .spawn(move || {
                let mut buf = vec![0_u8; READ_CHUNK_BYTES];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let _ = output_tx_for_thread.send(buf[..n].to_vec());
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        Err(err) => {
                            debug!(pid, error = %err, "PTY reader stopped");
                            break;
                        }
                    }
                }

                let exit = match child.wait() {
                    Ok(status) => PtyExit {
                        exit_code: status.signal().is_none().then(|| status.exit_code() as i32),
                        success: status.success(),
                    },
                    Err(err) => {
                        warn!(pid, error = %err, "failed to wait for PTY child");
                        PtyExit {
                            exit_code: None,
                            success: false,
                        }
                    }
                };
                let _ = exit_tx.send(Some(exit));
            })
            .map_err(PtyError::Io)?;

        Ok(Self {
            pid,
            master,
            writer,
            killer,
            reader_thread: Arc::new(Mutex::new(Some(reader_thread))),
            exit_rx,
            output_tx,
        })
    }

    /// Child process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Subscribe to PTY output chunks for raw attach streams.
    #[must_use]
    pub fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    /// Write user input to the PTY.
    pub async fn write_user_input(&self, bytes: Vec<u8>) -> Result<(), PtyError> {
        let writer = Arc::clone(&self.writer);
        tokio::task::spawn_blocking(move || {
            let mut writer = writer.lock().map_err(|_| PtyError::Poisoned)?;
            writer.write_all(&bytes)?;
            writer.flush()?;
            Ok::<(), PtyError>(())
        })
        .await
        .map_err(|_| PtyError::ThreadPanicked)?
    }

    /// Resize the PTY.
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
        let master = Arc::clone(&self.master);
        tokio::task::spawn_blocking(move || {
            let master = master.lock().map_err(|_| PtyError::Poisoned)?;
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
        .map_err(|_| PtyError::ThreadPanicked)?
    }

    /// Ask the process to terminate, then kill it if it does not exit in time.
    ///
    /// If exit was already observed, no signal is sent: the child may have been
    /// reaped and its pid could now belong to an unrelated process, so signaling
    /// it would be a hazard.
    pub async fn shutdown(&self, grace: Duration) -> Result<PtyExit, PtyError> {
        // Short-circuit if the child already exited: avoid signaling a pid that
        // may have been reaped and reused. Clone out of the watch guard first so
        // it is not held across the await below (it is not `Send`).
        let already_exited = self.exit_rx.borrow().clone();
        if let Some(exit) = already_exited {
            self.join_reader_thread().await?;
            return Ok(exit);
        }

        self.terminate()?;
        match tokio::time::timeout(grace, self.wait_exit()).await {
            Ok(exit) => {
                let exit = exit?;
                self.join_reader_thread().await?;
                Ok(exit)
            }
            Err(_) => {
                // Re-check: if the child exited during the grace window, skip the
                // hard kill rather than risk signaling a recycled pid.
                if self.exit_rx.borrow().clone().is_none() {
                    self.kill()?;
                }
                let exit = tokio::time::timeout(grace, self.wait_exit())
                    .await
                    .map_err(|_| PtyError::ExitTimeout)??;
                self.join_reader_thread().await?;
                Ok(exit)
            }
        }
    }

    /// Wait until the child exits.
    pub async fn wait_exit(&self) -> Result<PtyExit, PtyError> {
        let mut exit_rx = self.exit_rx.clone();
        loop {
            if let Some(exit) = exit_rx.borrow().clone() {
                return Ok(exit);
            }
            exit_rx.changed().await.map_err(|_| PtyError::ExitTimeout)?;
        }
    }

    /// Join the dedicated blocking reader thread after process exit.
    pub async fn join_reader_thread(&self) -> Result<(), PtyError> {
        let join = {
            let mut guard = self.reader_thread.lock().map_err(|_| PtyError::Poisoned)?;
            guard.take()
        };

        if let Some(join) = join {
            tokio::task::spawn_blocking(move || join.join())
                .await
                .map_err(|_| PtyError::ThreadPanicked)?
                .map_err(|_| PtyError::ThreadPanicked)?;
        }
        Ok(())
    }

    fn terminate(&self) -> Result<(), PtyError> {
        send_signal(self.pid, libc::SIGTERM)
    }

    fn kill(&self) -> Result<(), PtyError> {
        let mut killer = self.killer.lock().map_err(|_| PtyError::Poisoned)?;
        killer.kill()?;
        Ok(())
    }
}

#[allow(unsafe_code)]
fn send_signal(pid: u32, signal: libc::c_int) -> Result<(), PtyError> {
    // The callers re-check the observed-exit watch before signaling, but a
    // residual TOCTOU between that check and the `kill(2)` syscall below cannot
    // be fully closed without a pidfd; treating ESRCH as success is the standard
    // mitigation for that lost race.
    //
    // SAFETY: `kill` is called with a pid obtained from the child process handle
    // returned by portable-pty and a constant signal value.
    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if result == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            // Process already exited (and possibly was reaped); nothing to signal.
            return Ok(());
        }
        Err(PtyError::Io(err))
    }
}
