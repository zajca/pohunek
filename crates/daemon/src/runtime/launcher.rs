//! Worker lifecycle launch abstraction.
//!
//! The daemon launches only worker processes. PTY allocation and child process
//! ownership remain inside `pohunek-sessiond` (or the worker server used by
//! daemon unit tests).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use super::{UnitTemplate, Units};
pub use pohunek_platform::supervisor::{
    Error as WorkerLaunchError, Operation as WorkerLaunchFuture,
};
use pohunek_platform::supervisor::{ServiceId, ServiceObservation, ServiceState, Supervisor};
use pohunek_platform::{
    filesystem::TrustedDir,
    process::{HostInspector, ProcessInspector},
};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

// Rust guideline compliant 2026-09-24

/// Application directory inserted below each XDG base by `pohunek-paths`.
const APPLICATION_DIRECTORY: &str = "pohunek";

/// Daemon-facing durable worker supervisor.
pub trait WorkerLauncher: Supervisor {}

impl<T: Supervisor + ?Sized> WorkerLauncher for T {}

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
            .map_err(|source| operation("crash_injection", source))?;
        child
            .wait()
            .await
            .map_err(|source| operation("crash_injection_wait", source))?;
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

impl SubprocessWorkerLauncher {
    fn activate<'a>(&'a self, id: &'a ServiceId, replace: bool) -> WorkerLaunchFuture<'a, ()> {
        Box::pin(async move {
            for base in [
                &self.environment.runtime_home,
                &self.environment.state_home,
                &self.environment.data_home,
                &self.environment.config_home,
                &self.environment.cache_home,
            ] {
                // XDG bases are owner-managed and may legitimately be 0755.
                // Creating the private application child also creates a missing
                // base without ever repairing an existing entry.
                prepare_private_directory(&base.join(APPLICATION_DIRECTORY))?;
            }
            prepare_private_directory(
                &self
                    .environment
                    .runtime_home
                    .join(APPLICATION_DIRECTORY)
                    .join(pohunek_paths::WORKERS_SUBDIR),
            )?;
            prepare_private_directory(
                &self
                    .environment
                    .state_home
                    .join(APPLICATION_DIRECTORY)
                    .join(pohunek_paths::WORKERS_SUBDIR),
            )?;

            let mut children = self.children.lock().await;
            let previous_exited = children
                .get_mut(id.as_str())
                .map(tokio::process::Child::try_wait)
                .transpose()
                .map_err(|source| operation("inspect_before_start", source))?
                .flatten()
                .is_some();
            if children.contains_key(id.as_str()) && !replace && !previous_exited {
                return Err(operation(
                    "start",
                    std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "worker is already supervised",
                    ),
                ));
            }
            if let Some(mut previous) = children.remove(id.as_str()) {
                if !previous_exited {
                    debug_assert!(replace, "active child replacement was validated above");
                    previous
                        .start_kill()
                        .map_err(|source| operation("replace", source))?;
                    let _ = previous.wait().await;
                }
            } else if replace {
                tracing::debug!(
                    session_id = id.as_str(),
                    "replacement worker has no subprocess retained by this launcher"
                );
            }

            let mut command = Command::new(&self.binary);
            command
                .arg("--session-id")
                .arg(id.as_str())
                .arg("--worker-generation")
                .arg(new_generation()?)
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
                .map_err(|source| operation("spawn", source))?;
            children.insert(id.to_string(), child);
            Ok(())
        })
    }
}

impl Supervisor for SubprocessWorkerLauncher {
    fn start<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
        self.activate(id, false)
    }

    fn discover(&self) -> WorkerLaunchFuture<'_, Vec<ServiceObservation>> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            let mut observations = Vec::with_capacity(children.len());
            for (value, child) in children.iter_mut() {
                let id = ServiceId::parse(value.clone())?;
                observations.push(inspect_child(&id, child)?);
            }
            observations.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(observations)
        })
    }

    fn inspect<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ServiceObservation> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            let child = children
                .get_mut(id.as_str())
                .ok_or_else(|| WorkerLaunchError::NotFound(id.clone()))?;
            inspect_child(id, child)
        })
    }

    fn replace<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
        self.activate(id, true)
    }

    fn retire<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
        Box::pin(async move {
            let mut child = self
                .children
                .lock()
                .await
                .remove(id.as_str())
                .ok_or_else(|| WorkerLaunchError::NotFound(id.clone()))?;
            child
                .start_kill()
                .map_err(|source| operation("retire", source))?;
            child
                .wait()
                .await
                .map_err(|source| operation("retire_wait", source))?;
            Ok(())
        })
    }
}

fn inspect_child(
    id: &ServiceId,
    child: &mut Child,
) -> Result<ServiceObservation, WorkerLaunchError> {
    let pid = child.id();
    let exited = child
        .try_wait()
        .map_err(|source| operation("inspect", source))?
        .is_some();
    let process = if exited {
        None
    } else {
        match pid {
            Some(pid) => HostInspector::new()
                .identity(pid)
                .map_err(|source| operation("inspect_process_identity", source))?,
            None => None,
        }
    };
    Ok(ServiceObservation {
        id: id.clone(),
        state: if exited {
            ServiceState::Stopped
        } else {
            ServiceState::Running
        },
        process,
    })
}

fn prepare_private_directory(path: &std::path::Path) -> Result<(), WorkerLaunchError> {
    TrustedDir::open_or_create_absolute(path, 0o700)
        .map(|_| ())
        .map_err(|source| operation("prepare_private_directory", source))
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

impl Supervisor for SystemdWorkerLauncher {
    fn start<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
        Box::pin(async move {
            let units = self.units().await?;
            units
                .start(id.as_str())
                .await
                .map_err(|source| operation("start", source))?;
            Ok(())
        })
    }

    fn discover(&self) -> WorkerLaunchFuture<'_, Vec<ServiceObservation>> {
        Box::pin(async move { self.units().await?.discover().await })
    }

    fn inspect<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ServiceObservation> {
        Box::pin(async move { self.units().await?.inspect_service(id).await })
    }

    fn replace<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
        Box::pin(async move {
            self.units()
                .await?
                .restart(id.as_str())
                .await
                .map_err(|source| operation("replace", source))?;
            Ok(())
        })
    }

    fn retire<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
        Box::pin(async move {
            self.units()
                .await?
                .stop(id.as_str())
                .await
                .map_err(|source| operation("retire", source))?;
            Ok(())
        })
    }
}

impl SystemdWorkerLauncher {
    async fn units(&self) -> Result<Units, WorkerLaunchError> {
        Units::connect(self.template.clone())
            .await
            .map_err(|source| unavailable("connect", source))
    }
}

/// Draws a fresh daemon-issued worker generation token.
fn new_generation() -> Result<String, WorkerLaunchError> {
    let mut entropy = [0_u8; pohunek_paths::WORKER_GENERATION_ENTROPY_BYTES];
    getrandom::getrandom(&mut entropy)
        .map_err(|error| operation("generation", std::io::Error::other(error.to_string())))?;
    Ok(pohunek_paths::encode_worker_generation(entropy))
}

fn unavailable(
    operation_name: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> WorkerLaunchError {
    WorkerLaunchError::Unavailable {
        operation: operation_name,
        source: Box::new(source),
    }
}

fn operation(
    operation_name: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> WorkerLaunchError {
    WorkerLaunchError::Operation {
        operation: operation_name,
        source: Box::new(source),
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

    use pohunek_platform::supervisor::{ServiceId, ServiceObservation, ServiceState, Supervisor};

    use super::{operation, WorkerLaunchError, WorkerLaunchFuture};

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

    impl InProcessWorkerLauncher {
        fn activate<'a>(&'a self, id: &'a ServiceId, replace: bool) -> WorkerLaunchFuture<'a, ()> {
            Box::pin(async move {
                let mut tasks = self.tasks.lock().await;
                let session_id = id.as_str();
                let runtime_dir = self.runtime_root.join(session_id);
                let socket_path = runtime_dir.join(pohunek_paths::WORKER_SOCKET_NAME);
                if replace {
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
                } else if let Some(task) = tasks.get(session_id) {
                    if !task.is_finished() {
                        return Err(operation(
                            "start",
                            std::io::Error::new(
                                std::io::ErrorKind::AlreadyExists,
                                "worker is already supervised",
                            ),
                        ));
                    }
                    if let Some(task) = tasks.remove(session_id) {
                        let _ = task.await;
                    }
                    let _ = std::fs::remove_file(&socket_path);
                }
                let sequence = WORKER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let worker_id = format!("worker-test-{sequence}");
                let state_dir = self.state_root.join(session_id);
                let server = Server::bind(ServerArgs {
                    session_id: session_id.to_owned(),
                    worker_id: worker_id.clone(),
                    generation: super::new_generation()?,
                    socket_path: socket_path.clone(),
                    journal_path: state_dir.join(format!("{worker_id}.json")),
                    daemon_socket_path: self.daemon_socket.clone(),
                    config: WorkerConfig::new(),
                })
                .await
                .map_err(|error| operation("start_test_worker", error))?;
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

    impl Supervisor for InProcessWorkerLauncher {
        fn start<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
            self.activate(id, false)
        }

        fn discover(&self) -> WorkerLaunchFuture<'_, Vec<ServiceObservation>> {
            Box::pin(async move {
                let mut observations = self
                    .tasks
                    .lock()
                    .await
                    .iter()
                    .map(|(id, task)| {
                        Ok(ServiceObservation {
                            id: ServiceId::parse(id.clone())?,
                            state: if task.is_finished() {
                                ServiceState::Stopped
                            } else {
                                ServiceState::Running
                            },
                            process: None,
                        })
                    })
                    .collect::<Result<Vec<_>, WorkerLaunchError>>()?;
                observations.sort_by(|left, right| left.id.cmp(&right.id));
                Ok(observations)
            })
        }

        fn inspect<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ServiceObservation> {
            Box::pin(async move {
                let tasks = self.tasks.lock().await;
                let task = tasks
                    .get(id.as_str())
                    .ok_or_else(|| WorkerLaunchError::NotFound(id.clone()))?;
                Ok(ServiceObservation {
                    id: id.clone(),
                    state: if task.is_finished() {
                        ServiceState::Stopped
                    } else {
                        ServiceState::Running
                    },
                    process: None,
                })
            })
        }

        fn replace<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
            self.activate(id, true)
        }

        fn retire<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
            Box::pin(async move {
                let task = self
                    .tasks
                    .lock()
                    .await
                    .remove(id.as_str())
                    .ok_or_else(|| WorkerLaunchError::NotFound(id.clone()))?;
                task.abort();
                let _ = task.await;
                Ok(())
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use pohunek_platform::supervisor::{ServiceId, ServiceState, Supervisor};

        use super::InProcessWorkerLauncher;

        #[tokio::test]
        async fn start_reactivates_a_finished_task() {
            let root = tempfile::tempdir().expect("temporary worker root");
            let launcher = InProcessWorkerLauncher::new(
                root.path().join("runtime"),
                root.path().join("state"),
            );
            let id = ServiceId::parse("s-42").expect("valid service id");
            let task = tokio::spawn(async {});
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
            launcher.tasks.lock().await.insert(id.to_string(), task);

            launcher
                .start(&id)
                .await
                .expect("finished task can be started again");
            assert_eq!(
                launcher.inspect(&id).await.expect("active task").state,
                ServiceState::Running
            );
            launcher.retire(&id).await.expect("retire active task");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use pohunek_platform::supervisor::{ServiceId, ServiceState, Supervisor};

    use super::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};

    /// Bounds the regression test while allowing a loaded CI runner to reap the
    /// deliberately short-lived child.
    const CHILD_EXIT_ATTEMPTS: usize = 100;
    const CHILD_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

    #[tokio::test]
    async fn subprocess_start_reactivates_an_exited_child() {
        let root = tempfile::tempdir().expect("temporary worker root");
        let launcher = SubprocessWorkerLauncher::new(
            "true".into(),
            SubprocessWorkerEnvironment {
                runtime_home: root.path().join("runtime"),
                state_home: root.path().join("state"),
                data_home: root.path().join("data"),
                config_home: root.path().join("config"),
                cache_home: root.path().join("cache"),
                daemon_socket: root.path().join("daemon.sock"),
            },
        );
        let id = ServiceId::parse("s-42").expect("valid service id");
        launcher.start(&id).await.expect("first child starts");
        for path in [
            root.path().join("runtime/pohunek"),
            root.path().join("state/pohunek"),
        ] {
            let mode = std::fs::metadata(&path)
                .expect("application directory metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{} must remain owner-private", path.display());
        }

        let mut stopped = false;
        for _ in 0..CHILD_EXIT_ATTEMPTS {
            if launcher.inspect(&id).await.expect("child state").state == ServiceState::Stopped {
                stopped = true;
                break;
            }
            tokio::time::sleep(CHILD_EXIT_POLL_INTERVAL).await;
        }
        assert!(stopped, "short-lived child did not exit before deadline");

        launcher
            .start(&id)
            .await
            .expect("exited child can be started again");
    }
}
