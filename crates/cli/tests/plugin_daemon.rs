//! End-to-end contracts of `pohunek plugin` against a real daemon.
//!
//! The `pohunek` binary runs as a subprocess against a `ControlServer` served
//! from this test process over a real `SessionRegistry` and a real plugin root,
//! so every assertion exercises the daemon's package lifecycle, not a scripted
//! reply. `plugin_cli.rs` covers the binary's request shapes against a scripted
//! server; this file covers what the daemon does with them. Registry state is
//! also read straight from the plugin root through `package::registry`.

// Rust guideline compliant 2026-10-04

#![cfg(unix)]

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
use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
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
const PRIVATE_MODE: u32 = 0o700;

/// Mode of a file the tests write.
const PROFILE_MODE: u32 = 0o600;

/// Package id of the fixture archive.
const PACKAGE_ID: &str = "acme.runtime.pi";

/// Runtime id the fixture package serves.
const RUNTIME: &str = "pi";

/// Programs the fixture runtime launches; it only has to exist.
const PROGRAM: &str = "/bin/sh";

const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

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

fn overlay_registry() -> OverlayRegistry {
    let transport = Arc::new(EmptyTransport {
        id: OverlayId::new("test").expect("overlay id"),
    });
    let configured = ConfiguredTransport::new(transport, 18_722).expect("configured overlay");
    OverlayRegistry::new(vec![configured]).expect("registry")
}

/// A package archive on disk with the digest the daemon will see.
struct Built {
    path: PathBuf,
    digest: PackageDigest,
}

/// A real daemon serving the hermetic environment's runtime socket, plus the
/// directories the `pohunek` binary and the daemon share.
struct Harness {
    env: TestEnv,
    plugins: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn start() -> Self {
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
            shutdown,
            task,
        }
    }

    /// Removes every session the test started, so no durable worker outlives
    /// the test, then stops the server.
    async fn stop(self) {
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
    async fn run(&self, arguments: &[&str]) -> Output {
        self.env
            .tokio_command(pohunek_test_support::bin_exe("pohunek"))
            .args(arguments)
            .output()
            .await
            .expect("run the pohunek binary")
    }

    /// Runs `pohunek <arguments> --json` and returns the exit code and the
    /// single JSON document on stdout.
    async fn json(&self, arguments: &[&str]) -> (i32, Value) {
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
    fn registry(&self) -> RegistryState {
        Registry::open_at(&self.plugins, Limits::DEFAULT)
            .expect("open the plugin root")
            .state()
            .expect("read the registry state")
    }

    /// Writes a package archive of `version` that serves [`RUNTIME`].
    fn archive(&self, version: &str) -> Built {
        let document = runtime_document(version);
        let entries = [
            ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: document.into_bytes(),
                executable: false,
            },
            ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: DETECT_MANIFEST.as_bytes().to_vec(),
                executable: false,
            },
        ];
        let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
        let digest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the archive reads")
            .digest()
            .clone();
        let path = self.env.root().join(format!("pi-{version}.tar.zst"));
        fs::write(&path, bytes).expect("write the archive");
        Built { path, digest }
    }

    /// Writes a developer directory of `version` for `plugin link`.
    fn directory(&self, version: &str) -> PathBuf {
        let dir = self.env.root().join(format!("pi-dev-{version}"));
        fs::create_dir_all(&dir).expect("create the package directory");
        fs::write(dir.join("runtime.toml"), runtime_document(version)).expect("write runtime");
        fs::write(dir.join("detect.toml"), DETECT_MANIFEST).expect("write detect manifest");
        dir
    }

    /// The directory holding the installed tree of `digest`.
    fn root_of(&self, digest: &PackageDigest) -> PathBuf {
        let hex = digest.as_str().trim_start_matches("sha256:");
        self.plugins.join("packages").join(hex)
    }
}

fn runtime_document(version: &str) -> String {
    format!(
        r#"schema = 1
id = "{PACKAGE_ID}"
version = "{version}"
runtime_api = 1

[runtime]
id = "{RUNTIME}"
name = "Pi"
program = "{PROGRAM}"
args = ["--model", "fast"]
detect_manifest = "detect.toml"
prompt_arg = true

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--session", "{{reference}}"]

[fork]
supported = true
args = ["--fork", "{{reference}}"]

[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{{reference}}"]

[native_reference.existence]
check = "none"
"#
    )
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf-8 path")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Whether the host advertises `agent` among the runtimes a fresh session can
/// launch, read through `pohunek host inspect`.
async fn serves_agent(harness: &Harness, agent: &str) -> bool {
    let (code, document) = harness.json(&["host", "inspect", "local"]).await;
    assert_eq!(code, 0, "{document}");
    document["ok"]["supported_agents"]
        .as_array()
        .expect("supported_agents")
        .iter()
        .any(|entry| entry == agent)
}

/// Installs the `1.0.0` archive enabled and selected through the CLI.
async fn install_enabled(harness: &Harness, built: &Built) {
    let (code, document) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--sha256",
            built.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{document}");
    assert_eq!(document["ok"]["status"], "installed", "{document}");
}

/// Starts a session of `agent` in the hermetic working directory.
async fn launch(harness: &Harness, agent: &str) -> (i32, Value) {
    let cwd = harness.env.cwd().to_path_buf();
    harness
        .json(&["session", "new", "--agent", agent, "--cwd", path_str(&cwd)])
        .await
}

#[tokio::test]
async fn install_needs_consent_and_the_exact_digest() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    let wrong = format!("sha256:{}", "0".repeat(64));

    // Without --yes the daemon previews and nothing is recorded.
    let output = harness
        .run(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--sha256",
            built.digest.as_str(),
        ])
        .await;
    assert_eq!(output.status.code(), Some(1), "{}", stderr_text(&output));
    let stdout = stdout_text(&output);
    for needle in [PACKAGE_ID, "1.0.0", PROGRAM, built.digest.as_str()] {
        assert!(stdout.contains(needle), "missing {needle:?}: {stdout}");
    }
    assert!(
        stderr_text(&output).contains("needs your consent; nothing was changed"),
        "{}",
        stderr_text(&output)
    );
    let (code, document) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--sha256",
            built.digest.as_str(),
        ])
        .await;
    assert_eq!(code, 1);
    assert_eq!(document["err"]["code"], "consent_required");
    assert!(harness.registry().packages().is_empty());
    assert!(!serves_agent(&harness, RUNTIME).await);

    // A digest that is not the archive's is refused even with consent.
    let (code, document) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&built.path),
            "--sha256",
            &wrong,
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{document}");
    assert_eq!(
        document["err"]["code"], "package_archive_invalid",
        "{document}"
    );
    assert!(harness.registry().packages().is_empty());
    assert!(!serves_agent(&harness, RUNTIME).await);

    // The exact digest with consent installs it enabled and selected.
    install_enabled(&harness, &built).await;
    let state = harness.registry();
    let [record] = state.packages() else {
        panic!("one package is installed: {:?}", state.packages());
    };
    assert_eq!(record.digest(), &built.digest);
    assert!(record.enabled());
    assert_eq!(
        state.selected(&PACKAGE_ID.parse().expect("package id")),
        Some(&built.digest)
    );
    assert!(serves_agent(&harness, RUNTIME).await);

    harness.stop().await;
}

#[tokio::test]
async fn list_and_inspect_show_the_installed_package() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;

    let (code, listed) = harness.json(&["plugin", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    let packages = listed["ok"]["packages"].as_array().expect("packages");
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0]["digest"], built.digest.as_str());
    assert_eq!(packages[0]["enabled"], true);
    assert_eq!(packages[0]["selected"], true);

    let (code, inspected) = harness.json(&["plugin", "inspect", PACKAGE_ID]).await;
    assert_eq!(code, 0, "{inspected}");
    assert_eq!(inspected["ok"]["runtime"]["program"], PROGRAM);
    assert_eq!(inspected["ok"]["package"]["digest"], built.digest.as_str());

    let table = harness.run(&["plugin", "list"]).await;
    assert!(table.status.success(), "{}", stderr_text(&table));
    assert!(stdout_text(&table).contains(PACKAGE_ID));

    harness.stop().await;
}

#[tokio::test]
async fn disable_blocks_a_fresh_launch_and_enable_restores_it() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;
    let (code, launched) = launch(&harness, RUNTIME).await;
    assert_eq!(code, 0, "an enabled package launches: {launched}");
    assert!(launched["ok"]["id"].is_string(), "{launched}");

    let (code, disabled) = harness.json(&["plugin", "disable", PACKAGE_ID]).await;
    assert_eq!(code, 0, "{disabled}");
    assert!(!harness.registry().packages()[0].enabled());
    assert!(!serves_agent(&harness, RUNTIME).await);
    let (code, refused) = launch(&harness, RUNTIME).await;
    assert_eq!(code, 1, "a disabled package is not launchable: {refused}");
    assert_eq!(
        refused["err"]["code"], "agent_profile_not_found",
        "{refused}"
    );

    let (code, enabled) = harness.json(&["plugin", "enable", PACKAGE_ID]).await;
    assert_eq!(code, 0, "{enabled}");
    assert!(harness.registry().packages()[0].enabled());
    assert!(serves_agent(&harness, RUNTIME).await);
    let (code, relaunched) = launch(&harness, RUNTIME).await;
    assert_eq!(code, 0, "enable restores the launch: {relaunched}");

    harness.stop().await;
}

#[tokio::test]
async fn update_installs_side_by_side_and_select_rolls_back() {
    let harness = Harness::start().await;
    let old = harness.archive("1.0.0");
    let new = harness.archive("1.1.0");
    install_enabled(&harness, &old).await;

    let (code, updated) = harness
        .json(&[
            "plugin",
            "update",
            PACKAGE_ID,
            path_str(&new.path),
            "--sha256",
            new.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{updated}");
    let id = PACKAGE_ID.parse().expect("package id");
    let state = harness.registry();
    assert_eq!(state.packages().len(), 2, "both versions stay installed");
    assert!(state.package(&old.digest).is_some());
    assert_eq!(state.selected(&id), Some(&new.digest));

    let (code, rolled_back) = harness
        .json(&[
            "plugin",
            "select",
            PACKAGE_ID,
            "--digest",
            &old.digest.as_str()[7..19],
        ])
        .await;
    assert_eq!(code, 0, "{rolled_back}");
    assert_eq!(harness.registry().selected(&id), Some(&old.digest));

    // A bare id is now ambiguous only for mutations that need one version;
    // inspect prefers the selected version.
    let (code, inspected) = harness.json(&["plugin", "inspect", PACKAGE_ID]).await;
    assert_eq!(code, 0, "{inspected}");
    assert_eq!(inspected["ok"]["package"]["digest"], old.digest.as_str());

    harness.stop().await;
}

#[tokio::test]
async fn link_installs_disabled_and_enable_serves_the_runtime() {
    let harness = Harness::start().await;
    let dir = harness.directory("0.1.0");

    let without_consent = harness.run(&["plugin", "link", path_str(&dir)]).await;
    assert_eq!(without_consent.status.code(), Some(1));
    assert!(harness.registry().packages().is_empty());

    let (code, linked) = harness
        .json(&["plugin", "link", path_str(&dir), "--yes"])
        .await;
    assert_eq!(code, 0, "{linked}");
    let state = harness.registry();
    let [record] = state.packages() else {
        panic!("one package is linked: {:?}", state.packages());
    };
    assert!(!record.enabled(), "a linked package starts disabled");
    assert!(!serves_agent(&harness, RUNTIME).await);

    let (code, enabled) = harness.json(&["plugin", "enable", PACKAGE_ID]).await;
    assert_eq!(code, 0, "{enabled}");
    assert!(harness.registry().packages()[0].enabled());

    harness.stop().await;
}

#[tokio::test]
async fn a_modified_root_fails_doctor_and_needs_remove_modified() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;

    let clean = harness.run(&["plugin", "doctor"]).await;
    assert!(clean.status.success(), "{}", stderr_text(&clean));

    // The installed tree is read-only; a user with write access can still
    // change it, which is what verification must catch.
    let tampered = harness.root_of(&built.digest).join("files/runtime.toml");
    fs::set_permissions(&tampered, fs::Permissions::from_mode(PROFILE_MODE))
        .expect("make the file writable");
    fs::write(&tampered, "tampered").expect("tamper with the installed tree");

    let (code, doctor) = harness.json(&["plugin", "doctor"]).await;
    assert_eq!(code, 1, "{doctor}");
    assert_eq!(doctor["ok"]["findings"][0]["fault"], "root_modified");

    let (code, refused) = harness
        .json(&["plugin", "uninstall", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_root_invalid");
    assert!(harness.registry().package(&built.digest).is_some());

    let (code, removed) = harness
        .json(&[
            "plugin",
            "uninstall",
            PACKAGE_ID,
            "--remove-modified",
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{removed}");
    assert!(harness.registry().packages().is_empty());
    assert!(!harness.root_of(&built.digest).exists());

    let after = harness.run(&["plugin", "doctor"]).await;
    assert!(after.status.success(), "{}", stderr_text(&after));

    harness.stop().await;
}

#[tokio::test]
async fn a_remote_host_is_refused_without_touching_the_daemon() {
    let harness = Harness::start().await;
    let built = harness.archive("1.0.0");
    install_enabled(&harness, &built).await;
    let before = harness.registry();

    let output = harness
        .run(&["--host", "elsewhere", "plugin", "list", "--json"])
        .await;
    assert_eq!(output.status.code(), Some(1), "{}", stderr_text(&output));
    let document: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(document["err"]["code"], "plugin_local_only");

    let output = harness
        .run(&["--host", "elsewhere", "plugin", "disable", PACKAGE_ID])
        .await;
    assert_eq!(output.status.code(), Some(1));
    let after = harness.registry();
    assert_eq!(after.generation(), before.generation());
    assert!(after.packages()[0].enabled());

    harness.stop().await;
}

#[tokio::test]
async fn update_to_a_recorded_version_enables_and_selects_it() {
    let harness = Harness::start().await;
    let old = harness.archive("1.0.0");
    let new = harness.archive("1.1.0");
    install_enabled(&harness, &old).await;
    // A second version installed with `install` stays disabled and unselected.
    let (code, installed) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&new.path),
            "--sha256",
            new.digest.as_str(),
            "--no-enable",
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{installed}");
    let id = PACKAGE_ID.parse().expect("package id");
    let state = harness.registry();
    assert!(!state.package(&new.digest).expect("recorded").enabled());
    assert_eq!(state.selected(&id), Some(&old.digest));

    let (code, updated) = harness
        .json(&[
            "plugin",
            "update",
            PACKAGE_ID,
            path_str(&new.path),
            "--sha256",
            new.digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{updated}");
    assert_eq!(updated["ok"]["status"], "already_installed");
    assert_eq!(updated["ok"]["package"]["enabled"], true, "{updated}");
    assert_eq!(updated["ok"]["package"]["selected"], true, "{updated}");
    let state = harness.registry();
    assert!(state.package(&new.digest).expect("recorded").enabled());
    assert_eq!(state.selected(&id), Some(&new.digest));
    assert!(
        state.package(&old.digest).is_some(),
        "the old version stays"
    );

    harness.stop().await;
}
