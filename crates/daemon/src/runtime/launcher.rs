//! Worker supervisor abstraction and the dev/test subprocess supervisor.
//!
//! The daemon launches only worker processes. PTY allocation and child process
//! ownership remain inside `pohunek-sessiond` (or the worker server used by
//! daemon unit tests). Production uses the native supervisor of the target;
//! [`SubprocessWorkerLauncher`] runs the same explicit job definitions as
//! direct children for headless development and integration tests.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

pub use pohunek_platform::supervisor::{
    Error as WorkerLaunchError, Operation as WorkerLaunchFuture,
};
use pohunek_platform::supervisor::{
    JobDefinition, ServiceId, ServiceObservation, ServiceState, Supervisor, WorkerKey,
};
use pohunek_platform::{
    filesystem::TrustedDir,
    process::{HostInspector, ProcessInspector},
};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

// Rust guideline compliant 2026-09-24

/// XDG bases whose application directory the subprocess supervisor prepares.
///
/// A native manager starts workers inside an existing login session; direct
/// children in an isolated test tree need the owner-private roots created.
const PREPARED_XDG_BASES: [&str; 5] = [
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
];

/// Daemon-facing durable worker supervisor.
pub trait WorkerLauncher: Supervisor {}

impl<T: Supervisor + ?Sized> WorkerLauncher for T {}

/// One worker generation run as a direct child.
#[derive(Debug)]
struct SupervisedChild {
    child: Child,
    definition: JobDefinition,
}

/// Dev/test supervisor running each worker generation as a direct child.
///
/// It spawns exactly the definition's executable and arguments with a cleared
/// environment plus the definition's bootstrap variables, in a fresh process
/// group. Dropping the launcher kills its workers, but dropping a daemon server
/// or registry does not because the launcher is independently reference
/// counted by the harness.
#[derive(Debug, Clone, Default)]
pub struct SubprocessWorkerLauncher {
    children: Arc<Mutex<HashMap<ServiceId, SupervisedChild>>>,
}

impl SubprocessWorkerLauncher {
    /// Creates an empty subprocess supervisor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends `SIGKILL` to one session's worker process and waits for it to be
    /// reaped.
    ///
    /// This is an integration-harness crash injection point. It never targets
    /// the worker-owned agent process group or any other session worker. The
    /// job stays registered, as a crashed native job does, until retired.
    ///
    /// # Errors
    ///
    /// Returns a subprocess error when the selected child cannot be killed.
    pub async fn kill_worker(&self, session_id: &str) -> Result<bool, WorkerLaunchError> {
        let mut children = self.children.lock().await;
        let Some(supervised) = children
            .iter_mut()
            .find(|(id, _)| serves_session(id, session_id))
            .map(|(_, supervised)| supervised)
        else {
            return Ok(false);
        };
        if supervised
            .child
            .try_wait()
            .map_err(|source| operation("crash_injection", source))?
            .is_some()
        {
            return Ok(false);
        }
        supervised
            .child
            .start_kill()
            .map_err(|source| operation("crash_injection", source))?;
        supervised
            .child
            .wait()
            .await
            .map_err(|source| operation("crash_injection_wait", source))?;
        Ok(true)
    }

    /// Returns the running worker process ID of one session for integration
    /// assertions.
    pub async fn worker_process_id(&self, session_id: &str) -> Option<u32> {
        let mut children = self.children.lock().await;
        children
            .iter_mut()
            .filter(|(id, _)| serves_session(id, session_id))
            .find_map(|(_, supervised)| match supervised.child.try_wait() {
                Ok(None) => supervised.child.id(),
                Ok(Some(_)) | Err(_) => None,
            })
    }
}

impl Supervisor for SubprocessWorkerLauncher {
    fn start<'a>(
        &'a self,
        id: &'a ServiceId,
        definition: &'a JobDefinition,
    ) -> WorkerLaunchFuture<'a, ()> {
        Box::pin(async move {
            WorkerKey::from_service_id(id)?;
            for key in PREPARED_XDG_BASES {
                if let Some(base) = definition.environment().get(key) {
                    // XDG bases are owner-managed and may legitimately be 0755.
                    // Creating the private application child also creates a
                    // missing base without ever repairing an existing entry.
                    prepare_private_directory(&Path::new(base).join(pohunek_paths::APP_DIR))?;
                }
            }
            for key in ["XDG_RUNTIME_DIR", "XDG_STATE_HOME"] {
                if let Some(base) = definition.environment().get(key) {
                    prepare_private_directory(
                        &Path::new(base)
                            .join(pohunek_paths::APP_DIR)
                            .join(pohunek_paths::WORKERS_SUBDIR),
                    )?;
                }
            }

            let mut children = self.children.lock().await;
            if children.contains_key(id) {
                return Err(WorkerLaunchError::AlreadyRegistered(id.clone()));
            }
            let mut command = Command::new(definition.executable());
            command
                .args(definition.arguments())
                .env_clear()
                .envs(definition.environment())
                .current_dir(definition.working_directory())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            std::os::unix::process::CommandExt::process_group(command.as_std_mut(), 0);
            let child = command
                .spawn()
                .map_err(|source| operation("spawn", source))?;
            children.insert(
                id.clone(),
                SupervisedChild {
                    child,
                    definition: definition.clone(),
                },
            );
            Ok(())
        })
    }

    fn discover(&self) -> WorkerLaunchFuture<'_, Vec<ServiceObservation>> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            let mut observations = Vec::with_capacity(children.len());
            for (id, supervised) in children.iter_mut() {
                observations.push(inspect_child(id, supervised)?);
            }
            observations.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(observations)
        })
    }

    fn inspect<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ServiceObservation> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            let supervised = children
                .get_mut(id)
                .ok_or_else(|| WorkerLaunchError::NotFound(id.clone()))?;
            inspect_child(id, supervised)
        })
    }

    fn retire<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
        Box::pin(async move {
            let mut supervised = self
                .children
                .lock()
                .await
                .remove(id)
                .ok_or_else(|| WorkerLaunchError::NotFound(id.clone()))?;
            if supervised
                .child
                .try_wait()
                .map_err(|source| operation("retire", source))?
                .is_none()
            {
                supervised
                    .child
                    .start_kill()
                    .map_err(|source| operation("retire", source))?;
            }
            supervised
                .child
                .wait()
                .await
                .map_err(|source| operation("retire_wait", source))?;
            Ok(())
        })
    }
}

/// Whether a worker service ID belongs to `session_id`.
fn serves_session(id: &ServiceId, session_id: &str) -> bool {
    WorkerKey::from_service_id(id).is_ok_and(|key| key.session_id() == session_id)
}

fn inspect_child(
    id: &ServiceId,
    supervised: &mut SupervisedChild,
) -> Result<ServiceObservation, WorkerLaunchError> {
    let pid = supervised.child.id();
    let exited = supervised
        .child
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
        definition: Some(supervised.definition.facts()),
    })
}

fn prepare_private_directory(path: &Path) -> Result<(), WorkerLaunchError> {
    TrustedDir::open_or_create_absolute(path, 0o700)
        .map(|_| ())
        .map_err(|source| operation("prepare_private_directory", source))
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
    use std::time::Duration;

    use pohunek_platform::filesystem::FsError;
    use pohunek_session_worker::{Server, ServerArgs, WorkerConfig, WorkerError};
    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;

    use pohunek_platform::supervisor::{
        JobDefinition, ServiceId, ServiceObservation, ServiceState, Supervisor, WorkerKey,
    };

    use super::{operation, WorkerLaunchError, WorkerLaunchFuture};

    static WORKER_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    /// Upper bound for a retired generation's socket lock to become free.
    ///
    /// Covers the fork-to-exec window of a spawn made by any other thread of
    /// the test binary, which is milliseconds even on a loaded runner. A lock
    /// still held after this bound is reported as the contention it is.
    const FORKED_LOCK_RELEASE_BOUND: Duration = Duration::from_secs(5);

    /// Interval between bind attempts while that fork still holds the lock.
    const FORKED_LOCK_RELEASE_POLL: Duration = Duration::from_millis(10);

    /// Unit-test supervisor running the real worker server in-process.
    ///
    /// Each generation is one task serving the session's socket with the
    /// generation named by its service ID; the definition's executable is not
    /// run. It exists only in `cfg(test)` builds, so production and
    /// integration builds cannot select it.
    ///
    /// Retiring a generation drops its server, but every thread of the test
    /// binary shares one descriptor table: a child forked by another test
    /// keeps a copy of the retired socket lock until it execs. A real
    /// supervisor ends the worker process instead, so nothing can inherit the
    /// lock after `retire`. `start` therefore waits out that window, but only
    /// while no generation of the same session is still running.
    #[derive(Debug)]
    pub struct InProcessWorkerLauncher {
        runtime_root: PathBuf,
        state_root: PathBuf,
        daemon_socket: PathBuf,
        tasks: Arc<Mutex<HashMap<ServiceId, JoinHandle<()>>>>,
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

    /// Whether any generation of `session_id` still has a running task.
    fn session_running(tasks: &HashMap<ServiceId, JoinHandle<()>>, session_id: &str) -> bool {
        tasks.iter().any(|(id, task)| {
            !task.is_finished()
                && WorkerKey::from_service_id(id).is_ok_and(|key| key.session_id() == session_id)
        })
    }

    fn observe(id: &ServiceId, task: &JoinHandle<()>) -> ServiceObservation {
        ServiceObservation {
            id: id.clone(),
            state: if task.is_finished() {
                ServiceState::Stopped
            } else {
                ServiceState::Running
            },
            process: None,
            definition: None,
        }
    }

    impl Supervisor for InProcessWorkerLauncher {
        fn start<'a>(
            &'a self,
            id: &'a ServiceId,
            _definition: &'a JobDefinition,
        ) -> WorkerLaunchFuture<'a, ()> {
            Box::pin(async move {
                let key = WorkerKey::from_service_id(id)?;
                let mut tasks = self.tasks.lock().await;
                if tasks.contains_key(id) {
                    return Err(WorkerLaunchError::AlreadyRegistered(id.clone()));
                }
                let session_id = key.session_id();
                let socket_path = self
                    .runtime_root
                    .join(session_id)
                    .join(pohunek_paths::WORKER_SOCKET_NAME);
                let sequence = WORKER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let worker_id = format!("worker-test-{sequence}");
                let state_dir = self.state_root.join(session_id);
                let args = ServerArgs {
                    session_id: session_id.to_owned(),
                    worker_id: worker_id.clone(),
                    generation: key.generation().to_owned(),
                    socket_path,
                    journal_path: state_dir.join(format!("{worker_id}.json")),
                    daemon_socket_path: self.daemon_socket.clone(),
                    config: WorkerConfig::new(),
                };
                let deadline = tokio::time::Instant::now() + FORKED_LOCK_RELEASE_BOUND;
                let server = loop {
                    match Server::bind(args.clone()).await {
                        Ok(server) => break server,
                        // With no generation of this session left running, only a
                        // fork elsewhere in this process still holds the lock.
                        Err(WorkerError::TrustedFilesystem(FsError::LockContended { .. }))
                            if !session_running(&tasks, session_id)
                                && tokio::time::Instant::now() < deadline =>
                        {
                            tokio::time::sleep(FORKED_LOCK_RELEASE_POLL).await;
                        }
                        Err(error) => return Err(operation("start_test_worker", error)),
                    }
                };
                tasks.insert(
                    id.clone(),
                    tokio::spawn(async move {
                        let _ = server.serve().await;
                    }),
                );
                Ok(())
            })
        }

        fn discover(&self) -> WorkerLaunchFuture<'_, Vec<ServiceObservation>> {
            Box::pin(async move {
                let mut observations = self
                    .tasks
                    .lock()
                    .await
                    .iter()
                    .map(|(id, task)| observe(id, task))
                    .collect::<Vec<_>>();
                observations.sort_by(|left, right| left.id.cmp(&right.id));
                Ok(observations)
            })
        }

        fn inspect<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ServiceObservation> {
            Box::pin(async move {
                let tasks = self.tasks.lock().await;
                let task = tasks
                    .get(id)
                    .ok_or_else(|| WorkerLaunchError::NotFound(id.clone()))?;
                Ok(observe(id, task))
            })
        }

        fn retire<'a>(&'a self, id: &'a ServiceId) -> WorkerLaunchFuture<'a, ()> {
            Box::pin(async move {
                let task = self
                    .tasks
                    .lock()
                    .await
                    .remove(id)
                    .ok_or_else(|| WorkerLaunchError::NotFound(id.clone()))?;
                task.abort();
                // Awaiting the aborted task drops the server and its socket
                // lease, so the session's next generation can bind the path.
                let _ = task.await;
                Ok(())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use pohunek_platform::supervisor::{
        Error, JobDefinition, JobSpec, RestartPolicy, ServiceId, ServiceState, Supervisor,
    };

    use super::SubprocessWorkerLauncher;

    /// Bounds the regression test while allowing a loaded CI runner to reap the
    /// deliberately short-lived child.
    const CHILD_EXIT_ATTEMPTS: usize = 100;
    const CHILD_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

    fn definition(root: &std::path::Path, executable: &str, arguments: &[&str]) -> JobDefinition {
        let environment = [
            ("XDG_RUNTIME_DIR", "runtime"),
            ("XDG_STATE_HOME", "state"),
            ("HOME", "home"),
        ]
        .into_iter()
        .map(|(key, dir)| (key.to_owned(), root.join(dir).display().to_string()))
        .collect::<BTreeMap<_, _>>();
        std::fs::create_dir_all(root.join("home")).expect("create home");
        JobDefinition::new(JobSpec {
            executable: PathBuf::from(executable),
            arguments: arguments.iter().map(|arg| (*arg).to_owned()).collect(),
            environment,
            working_directory: root.join("home"),
            logs: None,
            start_timeout: Duration::from_secs(45),
            exit_timeout: Duration::from_secs(30),
            restart: RestartPolicy::Never,
            open_files: 8_192,
        })
        .expect("valid test definition")
    }

    fn service_id() -> ServiceId {
        ServiceId::parse("s-42.abcd2345").expect("valid service id")
    }

    #[tokio::test]
    async fn subprocess_runs_exactly_the_definition_with_a_cleared_environment() {
        let root = pohunek_test_support::tempdir().expect("temporary worker root");
        let output = root.path().join("env.txt");
        let script = format!("env > {}; pwd >> {}", output.display(), output.display());
        let launcher = SubprocessWorkerLauncher::new();
        let id = service_id();
        let definition = definition(root.path(), "/bin/sh", &["-c", &script]);

        launcher
            .start(&id, &definition)
            .await
            .expect("child starts");
        for path in [
            root.path().join("runtime/pohunek"),
            root.path().join("runtime/pohunek/workers"),
            root.path().join("state/pohunek/workers"),
        ] {
            let mode = std::fs::metadata(&path)
                .expect("application directory metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{} must remain owner-private", path.display());
        }
        wait_stopped(&launcher, &id).await;

        let written = std::fs::read_to_string(&output).expect("child output");
        let mut names = written
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .filter(|name| !matches!(*name, "PWD" | "OLDPWD" | "SHLVL" | "_"))
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(names, ["HOME", "XDG_RUNTIME_DIR", "XDG_STATE_HOME"]);
        assert!(written.ends_with(&format!("{}\n", root.path().join("home").display())));
        assert_eq!(
            launcher
                .inspect(&id)
                .await
                .expect("ended job stays registered")
                .definition,
            Some(definition.facts())
        );
    }

    #[tokio::test]
    async fn subprocess_keys_jobs_by_generation_and_retires_exactly_one() {
        let root = pohunek_test_support::tempdir().expect("temporary worker root");
        let launcher = SubprocessWorkerLauncher::new();
        let first = service_id();
        let second = ServiceId::parse("s-42.efgh2345").expect("valid service id");
        let definition = definition(root.path(), "/bin/sleep", &["30"]);

        launcher.start(&first, &definition).await.expect("first");
        assert!(matches!(
            launcher.start(&first, &definition).await,
            Err(Error::AlreadyRegistered(id)) if id == first
        ));
        launcher.start(&second, &definition).await.expect("second");
        launcher.retire(&first).await.expect("retire first");

        assert!(matches!(
            launcher.inspect(&first).await,
            Err(Error::NotFound(id)) if id == first
        ));
        assert_eq!(
            launcher.inspect(&second).await.expect("second").state,
            ServiceState::Running
        );
        assert!(matches!(
            launcher.retire(&first).await,
            Err(Error::NotFound(_))
        ));
        assert!(launcher.kill_worker("s-42").await.expect("kill second"));
        wait_stopped(&launcher, &second).await;
        launcher.retire(&second).await.expect("retire ended job");
    }

    #[tokio::test]
    async fn subprocess_rejects_service_ids_that_do_not_name_a_generation() {
        let root = pohunek_test_support::tempdir().expect("temporary worker root");
        let launcher = SubprocessWorkerLauncher::new();
        let id = ServiceId::parse("s-42").expect("valid service id");
        let definition = definition(root.path(), "/bin/true", &[]);

        assert!(matches!(
            launcher.start(&id, &definition).await,
            Err(Error::InvalidServiceId(_))
        ));
    }

    /// How long the concurrently forked child stays between `fork` and `exec`.
    ///
    /// Long enough that the next generation binds while the child still holds
    /// a copy of the retired generation's lock descriptors.
    const PRE_EXEC_HOLD_MICROS: u32 = 700_000;

    /// Upper bound for the forked child to report that it is holding.
    const FORK_REPORT_TIMEOUT: Duration = Duration::from_secs(10);

    /// Forks a child that, before it execs, holds a copy of every descriptor
    /// of this test process, and returns once the child reported the fork.
    fn spawn_child_holding_descriptors() -> std::thread::JoinHandle<()> {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        let (mut parent, child) = std::os::unix::net::UnixStream::pair().expect("report pair");
        let report_fd = child.as_raw_fd();
        let spawner = std::thread::spawn(move || {
            let mut command = std::process::Command::new("/usr/bin/true");
            #[expect(
                unsafe_code,
                reason = "the pre-exec hook models another thread's spawn between fork and exec"
            )]
            // SAFETY: the hook only calls the async-signal-safe `write` and
            // `usleep` on a descriptor inherited from the parent.
            unsafe {
                command.pre_exec(move || {
                    libc::write(report_fd, [1_u8].as_ptr().cast(), 1);
                    libc::usleep(PRE_EXEC_HOLD_MICROS);
                    Ok(())
                })
            };
            command.status().expect("pre-exec child runs");
            drop(child);
        });
        parent
            .set_read_timeout(Some(FORK_REPORT_TIMEOUT))
            .expect("report timeout");
        let mut byte = [0_u8; 1];
        parent
            .read_exact(&mut byte)
            .expect("child reports its fork");
        spawner
    }

    #[tokio::test]
    async fn in_process_generation_binds_after_a_forked_copy_of_the_retired_lock_closes() {
        let root = pohunek_test_support::tempdir().expect("temporary worker root");
        let runtime_root = root.path().join("r");
        let state_root = root.path().join("s");
        for directory in [&runtime_root, &state_root] {
            std::fs::create_dir_all(directory).expect("worker root");
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .expect("owner-private worker root");
        }
        let launcher = super::InProcessWorkerLauncher::new(runtime_root, state_root);
        let first = ServiceId::parse("s-42.abcd2345").expect("valid service id");
        let second = ServiceId::parse("s-42.efgh2345").expect("valid service id");
        let definition = definition(root.path(), "/bin/true", &[]);

        launcher.start(&first, &definition).await.expect("first");
        let holder = spawn_child_holding_descriptors();
        launcher.retire(&first).await.expect("retire first");

        launcher
            .start(&second, &definition)
            .await
            .expect("the retired generation's lock is released once the fork execs");
        holder.join().expect("holder thread");
        launcher.retire(&second).await.expect("retire second");
    }

    async fn wait_stopped(launcher: &SubprocessWorkerLauncher, id: &ServiceId) {
        for _ in 0..CHILD_EXIT_ATTEMPTS {
            if launcher.inspect(id).await.expect("child state").state == ServiceState::Stopped {
                return;
            }
            tokio::time::sleep(CHILD_EXIT_POLL_INTERVAL).await;
        }
        panic!("short-lived child did not exit before deadline");
    }
}
