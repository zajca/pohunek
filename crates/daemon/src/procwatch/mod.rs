//! Process observation primitives.
//!
//! The session registry uses this module to validate agent lifecycle claims
//! against OS process facts. Linux is implemented first; other platforms can add
//! backends behind the same trait without changing reconciliation code.

// Rust guideline compliant 2026-07-07

use std::fmt::Debug;
use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;

use tokio::io::unix::AsyncFd;
#[cfg(test)]
use tokio::sync::watch;

mod linux;

pub use linux::LinuxInspector;

/// OS process id used by PTY children and procwatch.
pub type Pid = u32;

/// Process identity facts read from the operating system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFact {
    /// Process id.
    pub pid: Pid,
    /// Parent process id.
    pub ppid: Pid,
    /// Kernel task command name.
    pub comm: String,
    /// NUL-separated argv vector from `/proc/<pid>/cmdline`.
    pub cmdline: Vec<String>,
}

/// Pohunek ownership markers read from a process environment.
///
/// Every PTY a pohunek daemon spawns carries `POHUNEK_DAEMON_ID` and
/// `POHUNEK_SESSION_ID` (see `session_pty_env`), and children inherit them at
/// `execve`. Reconciliation uses these markers to tell *whose* agent a process
/// is: a `codex`/`claude` process carrying another daemon's or another
/// session's markers (a nested test-suite daemon, a self-hosted dev instance)
/// must never be adopted as this session's active agent. A process without
/// markers carries `None` in both fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnershipMarkers {
    /// Value of `POHUNEK_DAEMON_ID`, when present.
    pub daemon_id: Option<String>,
    /// Value of `POHUNEK_SESSION_ID`, when present.
    pub session_id: Option<String>,
}

impl OwnershipMarkers {
    /// Whether the process carries any pohunek ownership marker at all.
    #[must_use]
    pub fn is_marked(&self) -> bool {
        self.daemon_id.is_some() || self.session_id.is_some()
    }
}

/// Guaranteed process-exit notification.
#[derive(Debug)]
pub struct ExitWatch {
    inner: ExitWatchKind,
}

#[derive(Debug)]
enum ExitWatchKind {
    Fd(AsyncFd<OwnedFd>),
    #[cfg(test)]
    Signal(watch::Receiver<bool>),
}

impl ExitWatch {
    /// Creates an exit watch from a pidfd.
    ///
    /// # Errors
    ///
    /// Returns the [`io::Error`] from [`AsyncFd::new`] when the runtime cannot
    /// register the file descriptor for readiness notifications.
    pub(crate) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        Ok(Self {
            inner: ExitWatchKind::Fd(AsyncFd::new(fd)?),
        })
    }

    /// Creates a manually fired exit watch for unit tests.
    #[cfg(test)]
    pub(crate) fn from_test_signal(receiver: watch::Receiver<bool>) -> Self {
        Self {
            inner: ExitWatchKind::Signal(receiver),
        }
    }

    /// Waits until the watched process exits.
    ///
    /// A readable pidfd means the process has exited. The method does not reap the
    /// process; it only observes the kernel readiness signal.
    ///
    /// # Errors
    ///
    /// Returns readiness-registration errors from Tokio, or a broken-pipe error
    /// when a test signal is dropped before it fires.
    pub async fn wait(self) -> io::Result<()> {
        match self.inner {
            ExitWatchKind::Fd(fd) => {
                let _ready = fd.readable().await?;
                Ok(())
            }
            #[cfg(test)]
            ExitWatchKind::Signal(mut receiver) => {
                while !*receiver.borrow_and_update() {
                    receiver.changed().await.map_err(|_closed| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "test exit signal dropped")
                    })?;
                }
                Ok(())
            }
        }
    }
}

/// OS process inspector used by session lifecycle reconciliation.
pub trait ProcessInspector: Debug + Send + Sync + 'static {
    /// Returns process facts for processes owned by the current effective user.
    ///
    /// Implementations should scan the process table once and skip races where a
    /// process exits during inspection.
    ///
    /// # Errors
    ///
    /// Returns OS I/O errors that are not normal process-exit races.
    fn same_user_processes(&self) -> io::Result<Vec<ProcessFact>>;

    /// Returns direct and transitive descendants of `root`.
    ///
    /// The root process itself is excluded. Implementations should skip process
    /// table races where a process exits during inspection.
    ///
    /// # Errors
    ///
    /// Returns OS I/O errors that are not normal process-exit races.
    fn descendants(&self, root: Pid) -> io::Result<Vec<ProcessFact>>;

    /// Returns the current working directory for `pid`.
    ///
    /// # Errors
    ///
    /// Returns the OS error from reading the process cwd link.
    fn cwd(&self, pid: Pid) -> io::Result<PathBuf>;

    /// Arms an exit watch for `pid`.
    ///
    /// # Errors
    ///
    /// Returns the OS error from opening or registering the process exit handle.
    fn exit_watch(&self, pid: Pid) -> io::Result<ExitWatch>;

    /// Returns the pohunek ownership markers from the process environment of
    /// `pid`.
    ///
    /// Implementations read the environment the process was started with (on
    /// Linux `/proc/<pid>/environ`) and extract `POHUNEK_DAEMON_ID` /
    /// `POHUNEK_SESSION_ID`. A process that exited during inspection yields
    /// empty markers, mirroring the race handling of the other methods.
    ///
    /// # Errors
    ///
    /// Returns OS I/O errors that are not normal process-exit races.
    fn ownership_markers(&self, pid: Pid) -> io::Result<OwnershipMarkers>;
}
