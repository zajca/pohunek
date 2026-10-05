//! Shared harness of the package lifecycle tests: a real daemon served from
//! this process over a hermetic environment, and the `pohunek` binary run
//! against it.
//!
//! The harness owns no package content; each test file adds its own fixture
//! archives through an `impl Harness` block.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-04

use std::fs;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
    OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
};
use package::registry::{Registry, RegistryState};
use package::{Limits, PackageDigest};
use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::runtime::{
    EnvironmentSource, SubprocessWorkerEnvironment, SubprocessWorkerLauncher,
};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};
use pohunek_test_support::env::{check_socket_path, TestEnv};
use pohunek_test_support::worker_binary;
use serde_json::Value;
use tokio::sync::oneshot;

/// Mode of every private fixture directory.
pub(crate) const PRIVATE_MODE: u32 = 0o700;

/// Mode of a profile file.
pub(crate) const PROFILE_MODE: u32 = 0o600;

/// Overlay transport with no peers; the registry only has to be non-empty.
#[derive(Debug)]
struct EmptyTransport {
    id: OverlayId,
}

impl OverlayTransport for EmptyTransport {
    fn id(&self) -> &OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, _addr: IpAddr) -> Result<(), BindAddrError> {
        Ok(())
    }

    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
        Box::pin(async { Ok(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)) })
    }

    fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
        let overlay = self.id.clone();
        Box::pin(async move {
            Err(OverlayError::HostUnknown {
                host: host.to_owned(),
                overlay,
            })
        })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        let overlay = self.id.clone();
        Box::pin(async move {
            Err(OverlayError::HostUnknown {
                host: identity.value().to_owned(),
                overlay,
            })
        })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

pub(crate) fn overlay_registry() -> OverlayRegistry {
    let transport = Arc::new(EmptyTransport {
        id: OverlayId::new("test").expect("overlay id"),
    });
    let configured = ConfiguredTransport::new(transport, 18_722).expect("configured overlay");
    OverlayRegistry::new(vec![configured]).expect("registry")
}

/// A package archive on disk with the digest the daemon will see.
pub(crate) struct Built {
    pub(crate) path: PathBuf,
    pub(crate) digest: PackageDigest,
}

/// A real daemon serving the hermetic environment's runtime socket, plus the
/// directories the `pohunek` binary and the daemon share.
pub(crate) struct Harness {
    pub(crate) env: TestEnv,
    pub(crate) plugins: PathBuf,
    pub(crate) agents: PathBuf,
    pub(crate) shutdown: oneshot::Sender<()>,
    pub(crate) task: tokio::task::JoinHandle<()>,
}

impl Harness {
    pub(crate) async fn start() -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let runtime = env.runtime_dir().join("pohunek");
        fs::create_dir_all(&runtime).expect("create the runtime directory");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(PRIVATE_MODE))
            .expect("private runtime directory");
        let socket = runtime.join("daemon.sock");
        check_socket_path(&socket).expect("the socket path fits sun_path");
        let plugins = env.data_home().join("plugins");
        let agents = env.config_home().join("pohunek/agents");
        fs::create_dir_all(&agents).expect("create the agents directory");
        fs::set_permissions(&agents, fs::Permissions::from_mode(PRIVATE_MODE))
            .expect("private agents directory");
        let worker_home = env.root().join("worker");
        let environment = SubprocessWorkerEnvironment {
            runtime_home: worker_home.join("runtime"),
            state_home: worker_home.join("state"),
            data_home: worker_home.join("data"),
            config_home: worker_home.join("config"),
            cache_home: worker_home.join("cache"),
            home: worker_home.clone(),
            daemon_socket: socket.clone(),
        };
        let config = SessionRegistryConfig {
            socket_path: Some(socket.clone()),
            plugins_dir: Some(plugins.clone()),
            agents_dir: Some(agents.clone()),
            // A profile-backed launch freezes a keyed revision, which needs the
            // host's revision key directory.
            host_state_dir: Some(env.root().join("host-state")),
            worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
            worker_state_root: Some(environment.state_home.join("pohunek/workers")),
            supervision: Some(
                environment
                    .supervision(worker_binary())
                    .with_environment_source(EnvironmentSource::fixed(env.environment().clone())),
            ),
            shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
            ..SessionRegistryConfig::default()
        };
        let registry =
            SessionRegistry::new_production(config, Arc::new(SubprocessWorkerLauncher::new()))
                .expect("the production registry opens the plugin root");
        let governance = Arc::new(
            HostGovernanceService::open(env.root().join("governance"))
                .await
                .expect("open the host-governance service"),
        );
        let state = DaemonState::new(
            HealthInfo::new("0.0.0-test"),
            registry,
            governance,
            overlay_registry(),
        );
        let server = ControlServer::bind_with_state(&socket, state)
            .await
            .expect("the control server binds");
        let (shutdown, stop) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            server
                .serve(async move {
                    let _ = stop.await;
                })
                .await;
        });
        Self {
            env,
            plugins,
            agents,
            shutdown,
            task,
        }
    }

    /// Removes every session the test started, so no durable worker outlives
    /// the test, then stops the server.
    pub(crate) async fn stop(self) {
        let (code, listed) = self.json(&["session", "list"]).await;
        assert_eq!(code, 0, "{listed}");
        let ids: Vec<String> = listed["ok"]
            .as_array()
            .expect("sessions")
            .iter()
            .map(|entry| entry["id"].as_str().expect("session id").to_owned())
            .collect();
        for id in &ids {
            let (code, removed) = self.json(&["session", "rm", id]).await;
            assert_eq!(code, 0, "{removed}");
        }
        let _ = self.shutdown.send(());
        self.task.await.expect("the server stops");
    }

    /// Runs `pohunek` with the hermetic environment.
    pub(crate) async fn run(&self, arguments: &[&str]) -> Output {
        self.env
            .tokio_command(pohunek_test_support::bin_exe("pohunek"))
            .args(arguments)
            .output()
            .await
            .expect("run the pohunek binary")
    }

    /// Runs `pohunek <arguments> --json` and returns the exit code and the
    /// single JSON document on stdout.
    pub(crate) async fn json(&self, arguments: &[&str]) -> (i32, Value) {
        let mut all = arguments.to_vec();
        all.push("--json");
        let output = self.run(&all).await;
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout of {all:?} is one JSON document ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        (output.status.code().expect("exit code"), document)
    }

    /// The package registry as the plugin root records it on disk.
    pub(crate) fn registry(&self) -> RegistryState {
        Registry::open_at(&self.plugins, Limits::DEFAULT)
            .expect("open the plugin root")
            .state()
            .expect("read the registry state")
    }

    /// Writes `<config>/pohunek/agents/<name>.toml`.
    pub(crate) fn profile(&self, name: &str, text: &str) -> PathBuf {
        let path = self.agents.join(format!("{name}.toml"));
        fs::write(&path, text).expect("write the profile");
        fs::set_permissions(&path, fs::Permissions::from_mode(PROFILE_MODE)).expect("profile mode");
        path
    }

    /// The directory holding the installed tree of `digest`.
    pub(crate) fn root_of(&self, digest: &PackageDigest) -> PathBuf {
        let hex = digest.as_str().trim_start_matches("sha256:");
        self.plugins.join("packages").join(hex)
    }
}

pub(crate) fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

pub(crate) fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub(crate) fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}
