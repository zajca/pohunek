//! `pohunek service upgrade` with live workers against the real native manager.
//!
//! One build has one version, so the test installs this build as the real
//! version A and upgrades to version B, a second directory holding the same
//! build. The engine's `test-util` override lets the staged binaries report A
//! while they are installed as B; staging, the transaction journal,
//! `service.toml`, the daemon replacement, readiness, and version GC all run
//! unchanged.
//!
//! Linux drives the systemd user manager and is ignored unless
//! `POHUNEK_SYSTEMD_E2E=1` opts in; the daemon unit goes to
//! `$XDG_RUNTIME_DIR/systemd/user` of the real session because the manager
//! never searches a temporary config home. macOS drives `gui/<uid>` in the
//! ordinary test run, with the agent definition below the temporary root; a
//! missing domain fails the test.
//!
//! The daemon and worker binaries come from `POHUNEK_DAEMON_BIN` and
//! `POHUNEK_WORKER_BIN`, or from the target directory of this build.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-09-24

use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pohunek_cli::service::settings::LAUNCHCTL_COMMAND;
use pohunek_cli::service::{Backend, Context, Engine, UninstallOptions, VERSION};
use pohunek_client::{Client, ClientOptions};
use pohunek_paths::{BasePaths, PathEnv, Platform};
use pohunek_platform::process::{HostInspector, ProcessIdentity, ProcessInspector as _};
use pohunek_platform::supervisor::ServiceObservation;
use protocol::{method, RuntimeState, SessionId, SessionInfo, SessionNewParams};

/// Deadline for any single condition the test waits for.
const WAIT: Duration = Duration::from_secs(60);

/// Interval between polls of a waited-for condition.
const POLL: Duration = Duration::from_millis(100);

/// Request deadline of the test's daemon clients.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// Version directory the upgrade installs this build under.
const UPGRADE_SUFFIX: &str = "-upgrade";

/// Live identity of one worker generation and its PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Runtime {
    job: String,
    worker: ProcessIdentity,
    executable: PathBuf,
    child: ProcessIdentity,
    tty: String,
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn upgrade_with_live_workers_keeps_their_pty_and_version() {
    let installation = Installation::new();
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let prefix = installation.root.join("prefix");
    let old = VERSION.to_owned();
    let new = format!("{VERSION}{UPGRADE_SUFFIX}");
    let versions = prefix.join("libexec/pohunek");

    Engine::new(&installation.context, &backend)
        .install(&from, &prefix, &old)
        .await
        .expect("install version A");
    let old_daemon = daemon_process(&backend).await;
    let sessions = [
        installation.new_session("upgrade-a").await,
        installation.new_session("upgrade-b").await,
    ];
    let before = runtimes(&installation, &backend, &sessions).await;
    for runtime in &before {
        assert_eq!(
            runtime.executable,
            versions.join(&old).join("pohunek-sessiond")
        );
    }

    let report = Engine::new(&installation.context, &backend)
        .with_reported_version(old.clone())
        .upgrade(&from, &new)
        .await
        .expect("upgrade to version B");
    assert_eq!(
        (report.from_version.as_str(), report.to_version.as_str()),
        (old.as_str(), new.as_str())
    );
    assert!(!report.unchanged);
    assert_eq!(report.gc_error, None);
    assert!(
        report.kept_versions.iter().any(|kept| kept.version == old),
        "GC keeps the version live workers run: {report:?}"
    );
    assert!(!report.removed_versions.contains(&old));
    assert!(versions.join(&old).join("pohunek-sessiond").is_file());

    // Both directories hold one build, so readiness cannot tell the daemons
    // apart; the restart is observed through the job and the process image.
    let daemon_executable = versions.join(&new).join("pohunekd");
    let new_daemon = eventually("the daemon restarted from version B", || async {
        let daemon = backend.daemon().inspect().await.ok()?;
        let process = daemon.process.filter(|process| *process != old_daemon)?;
        (daemon
            .definition
            .as_ref()
            .map(|facts| facts.executable.as_path())
            == Some(daemon_executable.as_path())
            && executable_of(process.pid).as_deref() == Some(daemon_executable.as_path()))
        .then_some(process)
    })
    .await;
    installation.client().await;
    eprintln!("daemon {} -> {}", old_daemon.pid, new_daemon.pid);

    let after = runtimes(&installation, &backend, &sessions).await;
    assert_eq!(after, before, "workers kept PID, child, PTY, and version A");

    let fresh = installation.new_session("upgrade-c").await;
    let fresh = runtimes(&installation, &backend, &[fresh]).await;
    assert_eq!(
        fresh[0].executable,
        versions.join(&new).join("pohunek-sessiond")
    );
}

/// One isolated installation below a short canonical temporary root.
struct Installation {
    root: PathBuf,
    context: Context,
    _temporary: tempfile::TempDir,
}

impl Installation {
    fn new() -> Self {
        require_manager();
        // `/var` is a symlink on macOS and socket paths are bounded, so the
        // root is a short path below the canonical `/tmp`.
        let temporary = tempfile::Builder::new()
            .prefix("phk")
            .tempdir_in(std::fs::canonicalize("/tmp").expect("canonical /tmp"))
            .expect("temporary root");
        let root = temporary.path().to_path_buf();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("private root");
        let dir = |name: &str| {
            let path = root.join(name);
            std::fs::create_dir_all(&path).expect("create XDG root");
            path
        };
        let env = [
            ("XDG_RUNTIME_DIR", dir("run")),
            ("XDG_STATE_HOME", dir("state")),
            ("XDG_DATA_HOME", dir("data")),
            ("XDG_CACHE_HOME", dir("cache")),
            ("XDG_CONFIG_HOME", dir("config")),
            ("HOME", dir("home")),
        ];
        let value = |key: &str| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, path)| path.clone().into_os_string())
        };
        let paths = BasePaths::resolve_for(
            Platform::current().expect("platform"),
            nix::unistd::Uid::effective().as_raw(),
            &PathEnv {
                xdg_runtime_dir: value("XDG_RUNTIME_DIR"),
                xdg_data_home: value("XDG_DATA_HOME"),
                xdg_state_home: value("XDG_STATE_HOME"),
                xdg_cache_home: value("XDG_CACHE_HOME"),
                xdg_config_home: value("XDG_CONFIG_HOME"),
                home: value("HOME"),
            },
        )
        .expect("resolve isolated paths");
        let context = Context::new(
            paths,
            nix::unistd::Uid::effective().as_raw(),
            Some(root.join("home")),
            Some(root.join("run")),
            supervisor_dir(&root),
            PathBuf::from(env!("CARGO_BIN_EXE_pohunek")),
        );
        Self {
            root,
            context,
            _temporary: temporary,
        }
    }

    /// Stages this build's three binaries like a release archive.
    fn stage(&self) -> PathBuf {
        let from = self.root.join("archive");
        std::fs::create_dir_all(&from).expect("archive dir");
        let cli = PathBuf::from(env!("CARGO_BIN_EXE_pohunek"));
        for (name, source) in [
            ("pohunek", cli.clone()),
            ("pohunekd", binary("POHUNEK_DAEMON_BIN", &cli, "pohunekd")),
            (
                "pohunek-sessiond",
                binary("POHUNEK_WORKER_BIN", &cli, "pohunek-sessiond"),
            ),
        ] {
            std::fs::copy(&source, from.join(name)).expect("stage binary");
        }
        from
    }

    async fn client(&self) -> Client {
        eventually("daemon client", || async {
            Client::connect_local_with_options(
                &self.context.paths().socket,
                ClientOptions::default().with_request_timeout(REQUEST_TIMEOUT),
            )
            .await
            .ok()
        })
        .await
    }

    /// Creates a shell session and waits until its runtime is live.
    async fn new_session(&self, name: &str) -> String {
        let session = self
            .client()
            .await
            .call::<method::SessionNew>(SessionNewParams {
                agent: "shell".to_owned(),
                name: Some(name.to_owned()),
                cwd: Some(self.root.join("home")),
                cols: 80,
                rows: 24,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                metadata: BTreeMap::new(),
            })
            .await
            .expect("create a session")
            .session
            .id
            .0;
        self.live(&session).await;
        session
    }

    async fn live(&self, session: &str) -> SessionInfo {
        eventually(&format!("{session} live"), || async {
            self.client()
                .await
                .call::<method::SessionInspect>(SessionId(session.to_owned()))
                .await
                .ok()
                .filter(|info| {
                    info.runtime
                        .as_ref()
                        .is_some_and(|runtime| runtime.state == RuntimeState::Live)
                })
        })
        .await
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        let context = self.context.clone();
        // The backends are async and the test's runtime cannot be re-entered
        // from a synchronous drop, so teardown owns a runtime on its thread.
        let teardown = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Runtime::new() else {
                return;
            };
            runtime.block_on(async {
                let Ok(namespace) = context.namespace() else {
                    return;
                };
                let Ok(backend) = Backend::connect(&context, &namespace, LAUNCHCTL_COMMAND).await
                else {
                    return;
                };
                // The upgraded daemon runs this build, which reports the
                // real version rather than the upgrade's directory name.
                let _uninstalled = Engine::new(&context, &backend)
                    .with_reported_version(VERSION)
                    .uninstall(UninstallOptions {
                        stop_sessions: true,
                        purge: true,
                    })
                    .await;
                let _daemon = backend.daemon().uninstall().await;
                if let Ok(jobs) = backend.workers().discover().await {
                    for job in jobs {
                        let _retired = backend.workers().retire(&job.id).await;
                    }
                }
                stop_namespace_slices(&namespace);
            });
        });
        if teardown.join().is_err() {
            eprintln!("teardown of {} panicked", self.root.display());
        }
    }
}

/// Connects the native manager for `namespace`.
async fn connect(
    context: &Context,
    namespace: &pohunek_platform::supervisor::Namespace,
) -> Backend {
    Backend::connect(context, namespace, LAUNCHCTL_COMMAND)
        .await
        .expect("connect to the native service manager")
}

/// Captures each session's worker job, process, executable, and PTY.
async fn runtimes(
    installation: &Installation,
    backend: &Backend,
    sessions: &[String],
) -> Vec<Runtime> {
    let inspector = HostInspector::new();
    let jobs = backend
        .workers()
        .discover()
        .await
        .expect("discover workers");
    let mut runtimes = Vec::new();
    for session in sessions {
        let info = installation.live(session).await;
        let matching = jobs
            .iter()
            .filter(|job| job.id.as_str().starts_with(&format!("{session}.")))
            .collect::<Vec<&ServiceObservation>>();
        let [job] = matching.as_slice() else {
            panic!("{session} has exactly one worker job: {matching:?}");
        };
        runtimes.push(Runtime {
            job: job.id.to_string(),
            worker: job.process.expect("the worker job has a process"),
            executable: job
                .definition
                .as_ref()
                .expect("the worker job has a definition")
                .executable
                .clone(),
            child: inspector
                .identity(info.pid)
                .expect("inspect the PTY child")
                .expect("the PTY child runs"),
            tty: tty(info.pid),
        });
    }
    runtimes
}

async fn daemon_process(backend: &Backend) -> ProcessIdentity {
    eventually("daemon process", || async {
        backend
            .daemon()
            .inspect()
            .await
            .ok()
            .and_then(|job| job.process)
    })
    .await
}

fn executable_of(pid: u32) -> Option<PathBuf> {
    HostInspector::new()
        .executable(pid)
        .expect("inspect the daemon executable")
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
}

fn tty(pid: u32) -> String {
    let output = std::process::Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    String::from_utf8(output.stdout)
        .expect("ps output is UTF-8")
        .trim()
        .to_owned()
}

/// Polls `probe` until it yields a value, failing after [`WAIT`].
async fn eventually<T, F, Fut>(what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(POLL).await;
    }
}

/// A built binary named by `variable`, or `name` next to the CLI binary.
fn binary(variable: &str, cli: &Path, name: &str) -> PathBuf {
    let path = std::env::var_os(variable).map_or_else(|| cli.with_file_name(name), PathBuf::from);
    assert!(
        path.is_absolute() && path.is_file(),
        "{} is missing; build `cargo build -p pohunek-daemon -p pohunek-session-worker --bins` or set {variable}",
        path.display()
    );
    path
}

#[cfg(target_os = "linux")]
fn require_manager() {
    assert_eq!(
        std::env::var("POHUNEK_SYSTEMD_E2E").as_deref(),
        Ok("1"),
        "set POHUNEK_SYSTEMD_E2E=1 explicitly"
    );
}

#[cfg(target_os = "macos")]
fn require_manager() {
    let domain = format!("gui/{}", nix::unistd::Uid::effective().as_raw());
    let status = std::process::Command::new("/bin/launchctl")
        .args(["print", &domain])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run launchctl");
    assert!(status.success(), "launchd domain {domain} is unavailable");
}

/// Stops the namespace's slices, which stay active until stopped once the
/// daemon uninstall ran while workers were still retiring.
#[cfg(target_os = "linux")]
fn stop_namespace_slices(namespace: &pohunek_platform::supervisor::Namespace) {
    for slice in [
        namespace.sessions_slice(),
        format!("pohunek-{}.slice", namespace.as_str()),
    ] {
        // Teardown only: an absent slice is the desired state.
        let _status = std::process::Command::new("systemctl")
            .args(["--user", "--quiet", "stop", slice.as_str()])
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// launchd has no slices.
#[cfg(target_os = "macos")]
fn stop_namespace_slices(_namespace: &pohunek_platform::supervisor::Namespace) {}

/// The manager's unit search path on Linux; never the production config home.
#[cfg(target_os = "linux")]
fn supervisor_dir(_root: &Path) -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR of the user session"))
        .join("systemd/user")
}

/// A private `LaunchAgents` directory; `bootstrap` loads from any path.
#[cfg(target_os = "macos")]
fn supervisor_dir(root: &Path) -> PathBuf {
    root.join("LaunchAgents")
}
