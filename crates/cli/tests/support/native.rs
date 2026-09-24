//! Isolated installations against the real native service manager.
//!
//! Shared by the `service_*` integration tests. Every installation lives
//! below a short canonical temporary root with its own XDG roots, so its
//! namespace never matches a real installation sharing the manager.
//!
//! Linux drives the systemd user manager, opted in with
//! `POHUNEK_SYSTEMD_E2E=1`; the daemon unit goes to
//! `$XDG_RUNTIME_DIR/systemd/user` of the real session because the manager
//! never searches a temporary config home. macOS drives `gui/<uid>` with the
//! agent definition in a temporary `LaunchAgents` directory; a missing domain
//! fails the test.
//!
//! The daemon and worker binaries come from `POHUNEK_DAEMON_BIN` and
//! `POHUNEK_WORKER_BIN`, or from the target directory of this build.

#![allow(
    dead_code,
    reason = "each service test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-09-24

use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pohunek_cli::service::report::StatusReport;
use pohunek_cli::service::settings::LAUNCHCTL_COMMAND;
use pohunek_cli::service::{Backend, Context, Engine, Error, UninstallOptions, VERSION};
use pohunek_client::{Client, ClientOptions};
use pohunek_paths::{BasePaths, PathEnv, Platform};
use pohunek_platform::supervisor::Namespace;
use protocol::{method, RuntimeState, SessionId, SessionInfo, SessionNewParams};

/// Deadline for any single condition a test waits for.
pub(crate) const WAIT: Duration = Duration::from_secs(60);

/// Interval between polls of a waited-for condition.
pub(crate) const POLL: Duration = Duration::from_millis(100);

/// Request deadline of the tests' daemon clients.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// One isolated installation below a short canonical temporary root.
pub(crate) struct Installation {
    pub(crate) root: PathBuf,
    pub(crate) context: Context,
    /// Version the running daemon reports, when it differs from its
    /// directory name (the upgrade test installs one build twice).
    pub(crate) reported_version: Option<String>,
    _temporary: tempfile::TempDir,
}

impl Installation {
    pub(crate) fn new() -> Self {
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
            reported_version: None,
            _temporary: temporary,
        }
    }

    /// Returns the installation prefix.
    pub(crate) fn prefix(&self) -> PathBuf {
        self.root.join("prefix")
    }

    /// Stages this build's three binaries like a release archive.
    pub(crate) fn stage(&self) -> PathBuf {
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

    pub(crate) async fn client(&self) -> Client {
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
    pub(crate) async fn new_session(&self, name: &str) -> String {
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

    pub(crate) async fn live(&self, session: &str) -> SessionInfo {
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

    pub(crate) async fn status(&self, backend: &Backend) -> StatusReport {
        let config = pohunek_service_config::ServiceConfig::load(&self.context.config_path())
            .expect("load service.toml");
        Engine::new(&self.context, backend)
            .status(Some(&config))
            .await
            .expect("status")
    }
}

impl Drop for Installation {
    /// Uninstalls whatever the test left and fails loudly when that fails.
    ///
    /// A test that already uninstalled leaves `NotInstalled`, which is fine.
    /// Any other result means the teardown may have left jobs in the real
    /// manager, so it panics, or only reports when the test already failed.
    fn drop(&mut self) {
        let context = self.context.clone();
        let reported = self.reported_version.clone();
        // The backends are async and the test's runtime cannot be re-entered
        // from a synchronous drop, so teardown owns a runtime on its thread.
        let teardown = std::thread::spawn(move || -> Result<(), String> {
            let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
            runtime.block_on(async {
                let namespace = context.namespace().map_err(|error| error.to_string())?;
                let backend = Backend::connect(&context, &namespace, LAUNCHCTL_COMMAND)
                    .await
                    .map_err(|error| error.to_string())?;
                let engine = Engine::new(&context, &backend);
                let engine = match reported {
                    Some(reported) => engine.with_reported_version(reported),
                    None => engine,
                };
                let result = engine
                    .uninstall(UninstallOptions {
                        stop_sessions: true,
                        purge: true,
                    })
                    .await;
                stop_namespace_slices(&namespace);
                match result {
                    Ok(_) | Err(Error::NotInstalled { .. }) => Ok(()),
                    Err(error) => Err(format!("teardown uninstall failed: {error}")),
                }
            })
        });
        let outcome = teardown
            .join()
            .unwrap_or_else(|_| Err("teardown panicked".to_owned()));
        if let Err(error) = outcome {
            if std::thread::panicking() {
                eprintln!("{} ({})", error, self.root.display());
            } else {
                panic!("{error} ({})", self.root.display());
            }
        }
    }
}

/// Connects the native manager for `namespace`.
pub(crate) async fn connect(context: &Context, namespace: &Namespace) -> Backend {
    Backend::connect(context, namespace, LAUNCHCTL_COMMAND)
        .await
        .expect("connect to the native service manager")
}

/// Runs install, status, a same-version upgrade, refusal with a live
/// session, and `uninstall --stop-sessions`, then checks that nothing of the
/// installation is left in the manager or on disk.
///
/// Returns the worker service IDs seen while the session was live, so the
/// caller can prove their native jobs are gone too.
pub(crate) async fn lifecycle(installation: &Installation) -> Vec<String> {
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let engine = Engine::new(&installation.context, &backend);
    let version_dir = installation.prefix().join("libexec/pohunek").join(VERSION);

    let report = engine
        .install(&from, &installation.prefix(), VERSION)
        .await
        .expect("install");
    assert_eq!(report.namespace, namespace.as_str());

    let status = installation.status(&backend).await;
    assert_eq!(status.active_version.as_deref(), Some(VERSION));
    assert!(!status.transaction_in_progress);
    let daemon = status.daemon.expect("daemon job");
    assert_eq!(daemon.state, "running");
    assert_eq!(
        daemon.executable.as_deref(),
        Some(version_dir.join("pohunekd").as_path())
    );

    let upgrade = engine.upgrade(&from, VERSION).await.expect("upgrade");
    assert!(upgrade.unchanged, "one build can only re-install itself");

    let session = installation.new_session("lifecycle").await;
    let workers: Vec<String> = backend
        .workers()
        .discover()
        .await
        .expect("discover workers")
        .iter()
        .map(|job| job.id.to_string())
        .collect();
    assert!(
        workers
            .iter()
            .any(|id| id.starts_with(&format!("{session}."))),
        "the live session has a worker job: {workers:?}"
    );

    let refused = engine.uninstall(UninstallOptions::default()).await;
    let Err(Error::LiveSessions { sessions, .. }) = &refused else {
        panic!("uninstall with a live session was not refused: {refused:?}");
    };
    assert!(
        sessions.iter().any(|live| live.id == session),
        "{sessions:?}"
    );
    assert!(installation.context.config_path().is_file());
    assert!(
        backend.daemon().inspect().await.is_ok(),
        "the daemon job stays"
    );
    assert!(version_dir.is_dir(), "a refused uninstall changes nothing");

    let removed = engine
        .uninstall(UninstallOptions {
            stop_sessions: true,
            purge: false,
        })
        .await
        .expect("uninstall with --stop-sessions");
    assert_eq!(removed.stopped_sessions, [session]);
    assert!(!installation.context.config_path().exists());
    assert!(
        matches!(
            backend.daemon().inspect().await,
            Err(pohunek_platform::supervisor::Error::NotFound(_))
        ),
        "the daemon job is gone"
    );
    let remaining = backend.workers().discover().await.expect("discover");
    assert!(remaining.is_empty(), "worker jobs remain: {remaining:?}");
    assert!(!version_dir.exists(), "the version directory is gone");
    assert!(!installation.prefix().join("bin").join("pohunek").exists());
    workers
}

/// Polls `probe` until it yields a value, failing after [`WAIT`].
pub(crate) async fn eventually<T, F, Fut>(what: &str, mut probe: F) -> T
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
pub(crate) fn binary(variable: &str, cli: &Path, name: &str) -> PathBuf {
    let path = std::env::var_os(variable).map_or_else(|| cli.with_file_name(name), PathBuf::from);
    assert!(
        path.is_absolute() && path.is_file(),
        "{} is missing; build `cargo build -p pohunek-daemon -p pohunek-session-worker --bins` or set {variable}",
        path.display()
    );
    path
}

#[cfg(target_os = "linux")]
pub(crate) fn require_manager() {
    assert_eq!(
        std::env::var("POHUNEK_SYSTEMD_E2E").as_deref(),
        Ok("1"),
        "set POHUNEK_SYSTEMD_E2E=1 explicitly"
    );
}

#[cfg(target_os = "macos")]
pub(crate) fn require_manager() {
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
pub(crate) fn stop_namespace_slices(namespace: &Namespace) {
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
pub(crate) fn stop_namespace_slices(_namespace: &Namespace) {}

/// The manager's unit search path on Linux; never the production config home.
#[cfg(target_os = "linux")]
pub(crate) fn supervisor_dir(_root: &Path) -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR of the user session"))
        .join("systemd/user")
}

/// A private `LaunchAgents` directory; `bootstrap` loads from any path.
#[cfg(target_os = "macos")]
pub(crate) fn supervisor_dir(root: &Path) -> PathBuf {
    root.join("LaunchAgents")
}
