//! Worker lifecycle launch abstraction.
//!
//! The daemon launches only worker processes. PTY allocation and child process
//! ownership remain inside `pohunek-sessiond` (or the worker server used by
//! daemon unit tests).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;

use super::{UnitTemplate, Units};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

// Rust guideline compliant 2026-07-24

/// Whether activation creates a new worker or replaces a terminal generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerLaunchMode {
    /// Start a previously absent worker unit.
    Start,
    /// Replace a stopped or failed worker for explicit native recovery.
    Replace,
}

/// Errors returned by worker launch backends.
#[derive(Debug, thiserror::Error)]
pub enum WorkerLaunchError {
    /// The systemd user manager could not activate the worker.
    #[error(transparent)]
    Systemd(#[from] super::UnitsError),
    /// A separate worker process could not be prepared or spawned.
    #[error("worker subprocess operation `{operation}` failed: {source}")]
    Subprocess {
        /// Stable operation label.
        operation: &'static str,
        /// Underlying operating-system failure.
        #[source]
        source: std::io::Error,
    },
    /// A test worker could not prepare its runtime resources.
    #[cfg(test)]
    #[error("test worker launch failed: {0}")]
    Test(String),
}

/// Boxed launch operation returned by [`WorkerLauncher`].
pub type WorkerLaunchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), WorkerLaunchError>> + Send + 'a>>;

/// Starts and replaces durable worker processes without owning their PTYs.
pub trait WorkerLauncher: std::fmt::Debug + Send + Sync {
    /// Activates the worker for `session_id`.
    fn launch<'a>(&'a self, session_id: &'a str, mode: WorkerLaunchMode) -> WorkerLaunchFuture<'a>;
}

/// XDG roots and daemon endpoint passed to a separate worker process.
#[derive(Debug, Clone)]
pub struct SubprocessWorkerEnvironment {
    /// Base for `$XDG_RUNTIME_DIR`.
    pub runtime_home: PathBuf,
    /// Base for `$XDG_STATE_HOME`.
    pub state_home: PathBuf,
    /// Base for `$XDG_DATA_HOME`.
    pub data_home: PathBuf,
    /// Base for `$XDG_CONFIG_HOME`.
    pub config_home: PathBuf,
    /// Base for `$XDG_CACHE_HOME`.
    pub cache_home: PathBuf,
    /// Actual daemon control socket used by worker-installed hooks.
    pub daemon_socket: PathBuf,
}

/// Integration launcher for a real `pohunek-sessiond` child process.
///
/// Every worker enters a fresh process group and owns its PTY below that
/// boundary. Dropping the launcher kills test workers, but dropping a daemon
/// server or registry does not because the launcher is independently reference
/// counted by the harness.
#[derive(Debug, Clone)]
pub struct SubprocessWorkerLauncher {
    binary: PathBuf,
    environment: SubprocessWorkerEnvironment,
    children: Arc<Mutex<std::collections::HashMap<String, Child>>>,
}

impl SubprocessWorkerLauncher {
    /// Creates a separate-process launcher for cross-crate integration tests.
    #[must_use]
    pub fn new(binary: PathBuf, environment: SubprocessWorkerEnvironment) -> Self {
        Self {
            binary,
            environment,
            children: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Sends `SIGKILL` to one worker process and waits for it to be reaped.
    ///
    /// This is an integration-harness crash injection point. It never targets
    /// the worker-owned agent process group or any other session worker.
    ///
    /// # Errors
    ///
    /// Returns a subprocess error when the selected child cannot be killed.
    pub async fn kill_worker(&self, session_id: &str) -> Result<bool, WorkerLaunchError> {
        let Some(mut child) = self.children.lock().await.remove(session_id) else {
            return Ok(false);
        };
        child
            .start_kill()
            .map_err(|source| WorkerLaunchError::Subprocess {
                operation: "crash_injection",
                source,
            })?;
        child
            .wait()
            .await
            .map_err(|source| WorkerLaunchError::Subprocess {
                operation: "crash_injection_wait",
                source,
            })?;
        Ok(true)
    }

    /// Returns the retained worker process ID for integration assertions.
    pub async fn worker_process_id(&self, session_id: &str) -> Option<u32> {
        self.children
            .lock()
            .await
            .get(session_id)
            .and_then(Child::id)
    }
}

impl WorkerLauncher for SubprocessWorkerLauncher {
    fn launch<'a>(&'a self, session_id: &'a str, mode: WorkerLaunchMode) -> WorkerLaunchFuture<'a> {
        Box::pin(async move {
            prepare_private_directory(&self.environment.runtime_home)?;
            prepare_private_directory(&self.environment.state_home)?;
            prepare_private_directory(&self.environment.data_home)?;
            prepare_private_directory(&self.environment.config_home)?;
            prepare_private_directory(&self.environment.cache_home)?;
            prepare_private_directory(&self.environment.runtime_home.join("pohunek/workers"))?;
            prepare_private_directory(&self.environment.state_home.join("pohunek/workers"))?;

            let mut children = self.children.lock().await;
            if let Some(mut previous) = children.remove(session_id) {
                previous
                    .start_kill()
                    .map_err(|source| WorkerLaunchError::Subprocess {
                        operation: "replace",
                        source,
                    })?;
                let _ = previous.wait().await;
            } else if mode == WorkerLaunchMode::Replace {
                tracing::debug!(
                    session_id,
                    "replacement worker has no subprocess retained by this launcher"
                );
            }

            let mut command = Command::new(&self.binary);
            command
                .arg("--session-id")
                .arg(session_id)
                .arg("--daemon-socket-path")
                .arg(&self.environment.daemon_socket)
                .env("XDG_RUNTIME_DIR", &self.environment.runtime_home)
                .env("XDG_STATE_HOME", &self.environment.state_home)
                .env("XDG_DATA_HOME", &self.environment.data_home)
                .env("XDG_CONFIG_HOME", &self.environment.config_home)
                .env("XDG_CACHE_HOME", &self.environment.cache_home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            std::os::unix::process::CommandExt::process_group(command.as_std_mut(), 0);
            let child = command
                .spawn()
                .map_err(|source| WorkerLaunchError::Subprocess {
                    operation: "spawn",
                    source,
                })?;
            children.insert(session_id.to_owned(), child);
            Ok(())
        })
    }
}

fn prepare_private_directory(path: &std::path::Path) -> Result<(), WorkerLaunchError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path).map_err(|source| WorkerLaunchError::Subprocess {
        operation: "create_private_directory",
        source,
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        WorkerLaunchError::Subprocess {
            operation: "secure_private_directory",
            source,
        }
    })
}

/// Production launcher backed by the native systemd user-manager API.
#[derive(Debug, Clone)]
pub struct SystemdWorkerLauncher {
    template: UnitTemplate,
}

impl SystemdWorkerLauncher {
    /// Creates a launcher for one validated systemd template.
    #[must_use]
    pub const fn new(template: UnitTemplate) -> Self {
        Self { template }
    }
}

impl WorkerLauncher for SystemdWorkerLauncher {
    fn launch<'a>(&'a self, session_id: &'a str, mode: WorkerLaunchMode) -> WorkerLaunchFuture<'a> {
        Box::pin(async move {
            let units = Units::connect(self.template.clone()).await?;
            match mode {
                WorkerLaunchMode::Start => {
                    units.start(session_id).await?;
                }
                WorkerLaunchMode::Replace => {
                    units.restart(session_id).await?;
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
pub use test_support::InProcessWorkerLauncher;

#[cfg(test)]
mod test_support {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use pohunek_session_worker::{Server, ServerArgs, WorkerConfig};
    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;

    use super::{WorkerLaunchError, WorkerLaunchFuture, WorkerLaunchMode, WorkerLauncher};

    static WORKER_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    /// Unit-test launcher using the real worker server and worker-owned PTY.
    ///
    /// It exists only in `cfg(test)` builds. Production and integration builds
    /// cannot select it.
    #[derive(Debug)]
    pub struct InProcessWorkerLauncher {
        runtime_root: PathBuf,
        state_root: PathBuf,
        daemon_socket: PathBuf,
        tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    }

    impl InProcessWorkerLauncher {
        /// Creates a launcher rooted in one owner-private test directory.
        #[must_use]
        pub fn new(runtime_root: PathBuf, state_root: PathBuf) -> Self {
            Self {
                daemon_socket: runtime_root.join("test-daemon.sock"),
                runtime_root,
                state_root,
                tasks: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    impl WorkerLauncher for InProcessWorkerLauncher {
        fn launch<'a>(
            &'a self,
            session_id: &'a str,
            mode: WorkerLaunchMode,
        ) -> WorkerLaunchFuture<'a> {
            Box::pin(async move {
                let mut tasks = self.tasks.lock().await;
                let runtime_dir = self.runtime_root.join(session_id);
                let socket_path = runtime_dir.join(pohunek_paths::WORKER_SOCKET_NAME);
                if mode == WorkerLaunchMode::Replace {
                    if let Some(task) = tasks.remove(session_id) {
                        task.abort();
                        // `abort()` only requests cancellation; it does not wait
                        // for the task to actually stop. Awaiting the handle
                        // ensures the old server's listener is fully dropped
                        // (releasing the Unix socket) before `Server::bind`
                        // below tries to rebind the same path. A `Cancelled`
                        // join error is the expected outcome here.
                        let _ = task.await;
                    }
                    // Production replace goes through systemd `RestartUnit`, which
                    // fully stops the old unit (and clears its runtime socket)
                    // before starting the replacement. Model that here: remove the
                    // old socket file so the new `Server::bind` never observes the
                    // superseded listener still draining its accept backlog, which
                    // otherwise flakes as `worker socket already accepts
                    // connections` under load.
                    let _ = std::fs::remove_file(&socket_path);
                }
                let sequence = WORKER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let worker_id = format!("worker-test-{sequence}");
                let state_dir = self.state_root.join(session_id);
                let server = Server::bind(ServerArgs {
                    session_id: session_id.to_owned(),
                    worker_id: worker_id.clone(),
                    socket_path: socket_path.clone(),
                    journal_path: state_dir.join(format!("{worker_id}.json")),
                    daemon_socket_path: self.daemon_socket.clone(),
                    config: WorkerConfig::new(),
                })
                .await
                .map_err(|error| WorkerLaunchError::Test(error.to_string()))?;
                tasks.insert(
                    session_id.to_owned(),
                    tokio::spawn(async move {
                        let _ = server.serve().await;
                    }),
                );
                Ok(())
            })
        }
    }
}
