//! Integration test: `assistant.materialize` over the real control server.
//!
//! The assistant bundle lives in a context package installed by digest. A
//! Unix `ControlServer` serves a registry over a real plugin root, and each
//! scenario installs, disables or omits the package before it materializes.

// Rust guideline compliant 2026-11-09

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::worker_binary;
use protocol::{
    method, AssistantMaterializeResult, PackageInstallResult, ProtocolError,
    Request as ProtocolRequest, Response, ASSISTANT_CONTEXT_PACKAGE_ID,
};
use serde_json::{json, Value};
use sha2::Digest as _;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

/// A valid prompt contract: one safety rule and one navigation step.
const CONTRACT: &str = "---\ntype: PromptTemplate\nid: assistant/system\ntitle: Contract\ndescription: Fixture contract.\nsource_kind: manual\nintents: [help]\n---\n\n## Safety\n\n- Never print secrets.\n\n## Navigation\n\n1. Start at the index.\n";

const CONCEPT: &str = "---\ntype: Guide\nid: guide-overview\ntitle: Overview\ndescription: Explains the bundle.\nsource_kind: manual\nintents: [help]\n---\n\n# Overview\n";

struct Servers {
    socket: PathBuf,
    plugins: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    /// Restores the XDG variables the handler resolves its runtime root from.
    _env: ProcessEnv,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Servers {
    async fn stop(self) {
        let _ = self.shutdown.send(());
        self.task.await.expect("server stops");
    }
}

async fn spawn_servers(tag: &str) -> Servers {
    let socket_dir = pohunek_test_support::tempdir_with_prefix(&format!("ph-{tag}-"))
        .expect("create the socket directory");
    let socket = socket_dir.path().join("daemon.sock");
    let plugins = socket_dir.path().join("plugins");
    let agents = socket_dir.path().join("agents");
    std::fs::create_dir(&agents).expect("create the agents directory");
    std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700))
        .expect("make the agents directory owner-private");
    let host_state = socket_dir.path().join("host-state");
    std::fs::create_dir(&host_state).expect("create the host-state directory");
    std::fs::set_permissions(&host_state, std::fs::Permissions::from_mode(0o700))
        .expect("make the host-state directory owner-private");
    let worker_home_dir =
        pohunek_test_support::tempdir_with_prefix("pw-a-").expect("create the worker home");
    let worker_home = worker_home_dir.path().to_path_buf();
    let environment = SubprocessWorkerEnvironment {
        runtime_home: worker_home.join("runtime"),
        state_home: worker_home.join("state"),
        data_home: worker_home.join("data"),
        config_home: worker_home.join("config"),
        cache_home: worker_home.join("cache"),
        home: worker_home.clone(),
        daemon_socket: socket.clone(),
    };
    let mut env = ProcessEnv::lock();
    env.set("XDG_RUNTIME_DIR", socket_dir.path().join("runtime"))
        .set("XDG_STATE_HOME", socket_dir.path().join("xdg-state"))
        .set("XDG_DATA_HOME", socket_dir.path().join("xdg-data"))
        .set("XDG_CONFIG_HOME", socket_dir.path().join("xdg-config"))
        .set("XDG_CACHE_HOME", socket_dir.path().join("xdg-cache"))
        .set("HOME", socket_dir.path().join("home"));
    let config = SessionRegistryConfig {
        socket_path: Some(socket.clone()),
        plugins_dir: Some(plugins.clone()),
        agents_dir: Some(agents),
        host_state_dir: Some(host_state),
        worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
        worker_state_root: Some(environment.state_home.join("pohunek/workers")),
        supervision: Some(environment.supervision(worker_binary())),
        ..support::hermetic_registry_config()
    };
    let registry =
        SessionRegistry::new_production(config, Arc::new(SubprocessWorkerLauncher::new()))
            .expect("the production registry opens the plugin root");
    let governance = Arc::new(
        HostGovernanceService::open(socket_dir.path().join("state"))
            .await
            .expect("open the host-governance service"),
    );
    let state = DaemonState::new(
        HealthInfo::new("0.0.0-test"),
        registry,
        governance,
        support::overlay_registry(),
        pohunek_daemon::api::OverlaySource::Default,
    );
    let unix = ControlServer::bind_with_state(&socket, state)
        .await
        .expect("unix server binds");
    let (shutdown, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        unix.serve(
            pohunek_daemon::api::ConnectionTasks::new(
                "control_connections",
                &tokio_util::sync::CancellationToken::new(),
            ),
            async move {
                let _ = rx.await;
            },
        )
        .await;
    });
    Servers {
        socket,
        plugins,
        shutdown,
        task,
        _env: env,
        _dirs: (socket_dir, worker_home_dir),
    }
}

async fn connect(socket: &Path) -> Framed<UnixStream, LinesCodec> {
    let stream = UnixStream::connect(socket)
        .await
        .expect("connect to the bound test socket");
    Framed::new(stream, LinesCodec::new())
}

async fn exchange(
    framed: &mut Framed<UnixStream, LinesCodec>,
    method: &str,
    params: Value,
) -> Response {
    let request = ProtocolRequest::new("assistant-test", method, params).expect("valid request");
    let line = serde_json::to_string(&request).expect("serialize request");
    framed.send(line).await.expect("send");
    let reply = framed
        .next()
        .await
        .expect("a response line")
        .expect("response framing ok");
    serde_json::from_str(&reply).expect("parse response")
}

async fn materialize(
    framed: &mut Framed<UnixStream, LinesCodec>,
    params: Value,
) -> Result<AssistantMaterializeResult, ProtocolError> {
    exchange(framed, method::ASSISTANT_MATERIALIZE, params)
        .await
        .into_result()
        .map(|value| serde_json::from_value(value).expect("materialize result"))
}

fn entry(path: &str, contents: &str) -> ArchiveEntry {
    ArchiveEntry {
        path: path.to_owned(),
        contents: contents.as_bytes().to_vec(),
        executable: false,
    }
}

fn descriptor(extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{ASSISTANT_CONTEXT_PACKAGE_ID}\"\nversion = \"1.2.3\"\nruntime_api = 1\nrequires = [\"package-context-v1\"]\n\n[context]\ndir = \"bundle\"\n{extra}"
    )
}

fn bundle_entries(contract: &str) -> Vec<ArchiveEntry> {
    vec![
        entry("runtime.toml", &descriptor("")),
        entry("bundle/index.md", "# Index\n"),
        entry("bundle/assistant/system.md", contract),
        entry("bundle/concepts/overview.md", CONCEPT),
    ]
}

/// Installs `entries` by digest through `package.install`, the way a client
/// that cannot show commands for consent does (no `accept_commands`).
async fn install(
    framed: &mut Framed<UnixStream, LinesCodec>,
    plugins: &Path,
    entries: &[ArchiveEntry],
) -> Result<PackageDigest, ProtocolError> {
    install_selected(framed, plugins, entries, true).await
}

/// Like [`install`], with the choice of selecting the package.
async fn install_selected(
    framed: &mut Framed<UnixStream, LinesCodec>,
    plugins: &Path,
    entries: &[ArchiveEntry],
    select: bool,
) -> Result<PackageDigest, ProtocolError> {
    let bytes = build_archive(entries, &Limits::DEFAULT).expect("archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("archive reads")
        .digest()
        .clone();
    let archive_path = plugins.with_file_name("assistant.tar.zst");
    std::fs::write(&archive_path, &bytes).expect("write the archive");
    exchange(
        framed,
        method::PACKAGE_INSTALL,
        json!({
            "archive_path": archive_path,
            "trust": { "kind": "explicit_digest", "digest": digest },
            "enable": true,
            "select": select,
            "dry_run": false
        }),
    )
    .await
    .into_result()
    .map(|value| {
        let _: PackageInstallResult = serde_json::from_value(value).expect("install result");
        digest
    })
}

#[tokio::test]
async fn an_absent_package_is_named_with_the_install_command() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let servers = spawn_servers("asst-absent").await;
    let mut client = connect(&servers.socket).await;

    let error = materialize(&mut client, json!({ "snapshot": "{}" }))
        .await
        .expect_err("no package is installed");

    assert_eq!(error.code, "assistant_package_missing", "{error:?}");
    assert!(
        error
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("plugin install")),
        "{error:?}"
    );
    servers.stop().await;
}

#[tokio::test]
async fn an_installed_package_serves_the_bundle_and_the_contract_to_a_legacy_request() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let servers = spawn_servers("asst-served").await;
    let mut client = connect(&servers.socket).await;
    let digest = install(&mut client, &servers.plugins, &bundle_entries(CONTRACT))
        .await
        .expect("the context package installs without a command opt-in");

    // The request carries only `snapshot`, as every client before the package
    // move sends it.
    let result = materialize(&mut client, json!({ "snapshot": "{\"a\":1}" }))
        .await
        .expect("the package materializes");

    assert_eq!(result.version, "1.2.3");
    assert_eq!(result.content_hash, digest.to_string());
    assert_eq!(result.prompt_contract, CONTRACT);
    assert!(
        Path::new(&result.bundle_path).join("index.md").is_file(),
        "the bundle path is the package's bundle directory: {}",
        result.bundle_path
    );
    let ids: Vec<&str> = result.concepts.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["assistant/system", "guide-overview"]);
    let snapshot = Path::new(&result.snapshot_path);
    assert_eq!(
        std::fs::read_to_string(snapshot).expect("snapshot"),
        "{\"a\":1}"
    );
    let mode = std::fs::metadata(snapshot)
        .expect("snapshot metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "the snapshot is owner-private: {mode:o}");
    servers.stop().await;
}

#[tokio::test]
async fn a_request_id_keeps_the_first_snapshot_and_another_id_gets_its_own() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let servers = spawn_servers("asst-keyed").await;
    let mut client = connect(&servers.socket).await;
    install(&mut client, &servers.plugins, &bundle_entries(CONTRACT))
        .await
        .expect("install");

    let first = materialize(
        &mut client,
        json!({ "snapshot": "first", "request_id": "r-1" }),
    )
    .await
    .expect("first launch");
    let again = materialize(
        &mut client,
        json!({ "snapshot": "second", "request_id": "r-1" }),
    )
    .await
    .expect("repeated launch");
    let other = materialize(
        &mut client,
        json!({ "snapshot": "third", "request_id": "r-2" }),
    )
    .await
    .expect("another launch");

    assert_eq!(first.snapshot_path, again.snapshot_path);
    assert_eq!(
        std::fs::read_to_string(&again.snapshot_path).expect("snapshot"),
        "first",
        "a repeat under the id leaves the file a running session reads alone"
    );
    assert_ne!(first.snapshot_path, other.snapshot_path);
    servers.stop().await;
}

#[tokio::test]
async fn a_disabled_package_is_refused_until_it_is_enabled_again() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let servers = spawn_servers("asst-disabled").await;
    let mut client = connect(&servers.socket).await;
    let digest = install(&mut client, &servers.plugins, &bundle_entries(CONTRACT))
        .await
        .expect("install");

    exchange(
        &mut client,
        method::PACKAGE_SET_ENABLED,
        json!({ "digest": digest, "enabled": false }),
    )
    .await
    .into_result()
    .expect("disable");
    let error = materialize(&mut client, json!({ "snapshot": "{}" }))
        .await
        .expect_err("a disabled package is not used");
    assert_eq!(error.code, "assistant_package_missing", "{error:?}");
    assert!(error.msg.contains("disabled"), "{error:?}");
    assert!(
        error
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("plugin enable")),
        "{error:?}"
    );

    exchange(
        &mut client,
        method::PACKAGE_SET_ENABLED,
        json!({ "digest": digest, "enabled": true }),
    )
    .await
    .into_result()
    .expect("enable");
    materialize(&mut client, json!({ "snapshot": "{}" }))
        .await
        .expect("an enabled package is used again");
    servers.stop().await;
}

/// The first file named `name` below `dir`.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for found in std::fs::read_dir(dir).ok()? {
        let path = found.ok()?.path();
        if path.is_dir() {
            if let Some(hit) = find_file(&path, name) {
                return Some(hit);
            }
        } else if path.file_name().is_some_and(|file| file == name) {
            return Some(path);
        }
    }
    None
}

#[tokio::test]
async fn an_installed_but_unselected_package_is_refused_with_the_select_command() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let servers = spawn_servers("asst-unselected").await;
    let mut client = connect(&servers.socket).await;
    install_selected(
        &mut client,
        &servers.plugins,
        &bundle_entries(CONTRACT),
        false,
    )
    .await
    .expect("install");

    let error = materialize(&mut client, json!({ "snapshot": "{}" }))
        .await
        .expect_err("an unselected package is not used");

    assert_eq!(error.code, "assistant_package_missing", "{error:?}");
    assert!(error.msg.contains("not selected"), "{error:?}");
    assert!(
        error
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("plugin select")),
        "{error:?}"
    );
    servers.stop().await;
}

#[tokio::test]
async fn a_package_whose_files_were_modified_after_install_is_refused() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let servers = spawn_servers("asst-modified").await;
    let mut client = connect(&servers.socket).await;
    install(&mut client, &servers.plugins, &bundle_entries(CONTRACT))
        .await
        .expect("install");
    materialize(&mut client, json!({ "snapshot": "{}" }))
        .await
        .expect("the intact package is used");

    let index = find_file(&servers.plugins, "index.md").expect("the installed bundle index");
    let mut permissions = std::fs::metadata(&index)
        .expect("stat the index")
        .permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(&index, permissions).expect("make the index writable");
    std::fs::write(&index, "# tampered\n").expect("modify the installed file");

    let error = materialize(&mut client, json!({ "snapshot": "{}" }))
        .await
        .expect_err("a modified package is not used");

    assert_eq!(error.code, "assistant_package_missing", "{error:?}");
    assert!(
        error
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("plugin doctor")),
        "{error:?}"
    );
    servers.stop().await;
}

#[tokio::test]
async fn files_left_by_an_earlier_release_are_never_served() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let servers = spawn_servers("asst-earlier-release").await;
    let root = servers.socket.parent().expect("socket directory");
    // The earlier release cached its bundle under `knowledge/<version>-<hash>`.
    let earlier_name = format!("{}-{}", env!("CARGO_PKG_VERSION"), "0".repeat(64));
    let cache = root.join("xdg-cache/pohunek/knowledge").join(earlier_name);
    std::fs::create_dir_all(cache.join("assistant")).expect("create the earlier bundle");
    std::fs::write(cache.join("index.md"), "# earlier\n").expect("write the earlier index");
    std::fs::write(cache.join("assistant/system.md"), CONTRACT)
        .expect("write the earlier contract");
    let mut client = connect(&servers.socket).await;

    let error = materialize(&mut client, json!({ "snapshot": "{}" }))
        .await
        .expect_err("the earlier bundle cache is not a fallback");
    assert_eq!(error.code, "assistant_package_missing", "{error:?}");

    install(&mut client, &servers.plugins, &bundle_entries(CONTRACT))
        .await
        .expect("install");
    let probe = materialize(
        &mut client,
        json!({ "snapshot": "{}", "request_id": "probe" }),
    )
    .await
    .expect("the installed package is used");
    assert!(
        !probe.bundle_path.contains("xdg-cache"),
        "the bundle comes from the package: {}",
        probe.bundle_path
    );

    // The earlier release named a keyed launch directory
    // `launch-<version>-<hash>-request-<digest>` and left its snapshot behind.
    let launches = Path::new(&probe.snapshot_path)
        .parent()
        .and_then(Path::parent)
        .expect("the assistant runtime directory");
    let digest = format!("{:x}", sha2::Sha256::digest(b"r-earlier"));
    let earlier_launch = launches.join(format!(
        "launch-{}-{}-request-{digest}",
        env!("CARGO_PKG_VERSION"),
        "0".repeat(64)
    ));
    std::fs::create_dir_all(&earlier_launch).expect("create the earlier launch directory");
    std::fs::write(earlier_launch.join("snapshot.json"), "earlier").expect("write the snapshot");

    let launch = materialize(
        &mut client,
        json!({ "snapshot": "current", "request_id": "r-earlier" }),
    )
    .await
    .expect("a launch under the same request id");

    assert!(
        !Path::new(&launch.snapshot_path).starts_with(&earlier_launch),
        "{}",
        launch.snapshot_path
    );
    assert_eq!(
        std::fs::read_to_string(&launch.snapshot_path).expect("read the snapshot"),
        "current"
    );
    servers.stop().await;
}

#[tokio::test]
async fn a_package_with_a_malformed_contract_is_refused() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let servers = spawn_servers("asst-contract").await;
    let mut client = connect(&servers.socket).await;
    install(
        &mut client,
        &servers.plugins,
        &bundle_entries(&CONTRACT.replace("- Never print secrets.", "prose is not a rule")),
    )
    .await
    .expect("install");

    let error = materialize(&mut client, json!({ "snapshot": "{}" }))
        .await
        .expect_err("the contract is malformed");

    assert_eq!(error.code, "assistant_package_missing", "{error:?}");
    assert!(error.msg.contains("prompt contract"), "{error:?}");
    servers.stop().await;
}

#[tokio::test]
async fn a_context_descriptor_that_mixes_exports_or_names_nothing_is_refused_at_install() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let servers = spawn_servers("asst-invalid").await;
    let mut client = connect(&servers.socket).await;
    let cases = [
        (
            "commands",
            vec![
                entry(
                    "runtime.toml",
                    &descriptor("\n[commands.run]\nprogram = \"bundle/index.md\"\n"),
                ),
                entry("bundle/index.md", "# Index\n"),
            ],
        ),
        (
            "empty directory",
            vec![
                entry("runtime.toml", &descriptor("")),
                entry("other/x.md", "x"),
            ],
        ),
        (
            "escaping directory",
            vec![
                entry(
                    "runtime.toml",
                    &descriptor("").replace("dir = \"bundle\"", "dir = \"../bundle\""),
                ),
                entry("bundle/index.md", "# Index\n"),
            ],
        ),
    ];
    for (name, entries) in cases {
        let error = install(&mut client, &servers.plugins, &entries)
            .await
            .expect_err(name);
        assert_eq!(
            error.code, "package_descriptor_invalid",
            "{name}: {error:?}"
        );
    }
    servers.stop().await;
}
