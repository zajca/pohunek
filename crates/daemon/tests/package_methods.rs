//! Integration test: the `package.*` lifecycle methods over the real control
//! servers.
//!
//! A Unix `ControlServer` and a loopback-TCP `RemoteServer` (the stand-in for
//! the overlay connection) serve one shared registry over a real plugin root.
//! The Unix socket serves the whole lifecycle; every package method is refused
//! on the remote connection before it reads a parameter or touches the disk.
//! The same servers, over a registry that owns a metadata store, serve the
//! package gates that depend on rejected store lines.

// Rust guideline compliant 2026-10-12

mod support;

pohunek_test_support::hold_image!();

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use protocol::{
    method, HostCapabilities, PackageChangeResult, PackageDoctorResult, PackageInspectResult,
    PackageInstallResult, PackageInstallStatus, PackageListResult, PackageUninstallResult,
    Request as ProtocolRequest, Response,
};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo, RemoteServer};
use pohunek_daemon::catalog_anchor::{CatalogTrust, IndexTrust};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{HostTrustAnchor, SessionRegistry, SessionRegistryConfig};
use pohunek_daemon::{CheckoutReadStep, CHECKOUT_TRUST_DEADLINE};
use pohunek_test_support::catalog::{root_of, test_key, WINDOW_END, WINDOW_START};
use pohunek_test_support::time::{AutoAdvanceInhibitor, TIMER_TICK};
use pohunek_test_support::worker_binary;

/// Runtime id of the fixture package.
const RUNTIME: &str = "pi";

/// Wire code of a package method refused on a remote connection.
const LOCAL_ONLY: &str = "local_only_method";

/// Every package method with parameters that would be valid on the local
/// socket.
fn package_methods(digest: &PackageDigest) -> Vec<(&'static str, Value)> {
    vec![
        (method::PACKAGE_LIST, Value::Null),
        (method::PACKAGE_INSPECT, json!({ "digest": digest })),
        (method::PACKAGE_DOCTOR, json!({})),
        (
            method::PACKAGE_INSTALL,
            json!({
                "archive_path": "/nonexistent/archive.tar.zst",
                "trust": { "kind": "explicit_digest", "digest": digest },
                "enable": true,
                "select": true,
                "dry_run": false
            }),
        ),
        (
            method::PACKAGE_LINK,
            json!({ "directory": "/nonexistent/dir", "dry_run": false }),
        ),
        (
            method::PACKAGE_SET_ENABLED,
            json!({ "digest": digest, "enabled": true }),
        ),
        (method::PACKAGE_SELECT, json!({ "digest": digest })),
        (
            method::PACKAGE_UNINSTALL,
            json!({ "digest": digest, "remove_modified": false }),
        ),
        (
            method::PACKAGE_BIND_PROFILE,
            json!({ "profile": "work", "digest": digest, "dry_run": true }),
        ),
        (
            method::PACKAGE_CREATE_PROFILE,
            json!({ "profile": "work", "base": "shell", "dry_run": true }),
        ),
    ]
}

struct Servers {
    addr: SocketAddr,
    socket: PathBuf,
    /// The metadata store, when the registry owns one.
    store: Option<PathBuf>,
    plugins: PathBuf,
    /// Directory the daemon reads host profiles from.
    agents: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    /// The path of the repository the checkout-trust predicate fetches the
    /// remote `HEAD` from; `None` is a failed lookup.
    remote_source: Arc<std::sync::Mutex<Option<String>>>,
    /// The registry the servers answer from, for a scenario that restarts the
    /// daemon over its store.
    registry: SessionRegistry,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Servers {
    async fn stop(self) {
        let _ = self.shutdown.send(());
        self.task.await.expect("servers stop");
    }
}

async fn spawn_servers(tag: &str) -> Servers {
    spawn_servers_over(tag, None).await
}

/// [`spawn_servers`] over a registry that owns a metadata store holding
/// `store_content`.
/// Writes `<root>/data/metadata.jsonl` owner-private and returns its path.
fn write_data_store(root: &Path, content: &str) -> PathBuf {
    let directory = root.join("data");
    std::fs::create_dir(&directory).expect("create the data directory");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .expect("make the data directory owner-private");
    let path = directory.join("metadata.jsonl");
    std::fs::write(&path, content).expect("write the metadata store");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("make the metadata store owner-private");
    path
}

async fn spawn_servers_over(tag: &str, store_content: Option<&str>) -> Servers {
    spawn_servers_trusting(tag, store_content, CatalogTrust::Absent).await
}

/// [`spawn_servers_over`] over a registry whose catalog trust is `trust`.
async fn spawn_servers_trusting(
    tag: &str,
    store_content: Option<&str>,
    trust: CatalogTrust,
) -> Servers {
    spawn_servers_configured(tag, store_content, |config| config.catalog_trust = trust).await
}

/// [`spawn_servers_trusting`] over a host whose `checkout_trust` lists
/// `trusted_repositories`.
async fn spawn_servers_trusting_repositories(
    tag: &str,
    store_content: Option<&str>,
    trust: CatalogTrust,
    trusted_repositories: Vec<String>,
) -> Servers {
    spawn_servers_configured(tag, store_content, |config| {
        config.catalog_trust = trust;
        config.trusted_repositories = trusted_repositories;
    })
    .await
}

/// [`spawn_servers_over`] with `configure` applied to the registry config.
async fn spawn_servers_configured(
    tag: &str,
    store_content: Option<&str>,
    configure: impl FnOnce(&mut SessionRegistryConfig),
) -> Servers {
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
        pohunek_test_support::tempdir_with_prefix("pw-p-").expect("create the worker home");
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
    let store = store_content.map(|content| write_data_store(socket_dir.path(), content));
    let mut config = SessionRegistryConfig {
        socket_path: Some(socket.clone()),
        store_path: store.clone(),
        plugins_dir: Some(plugins.clone()),
        agents_dir: Some(agents.clone()),
        host_state_dir: Some(host_state),
        worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
        worker_state_root: Some(environment.state_home.join("pohunek/workers")),
        supervision: Some(
            environment
                .supervision(worker_binary())
                .with_environment_source(support::hermetic_environment_source()),
        ),
        ..support::hermetic_registry_config()
    };
    configure(&mut config);
    let registry =
        SessionRegistry::new_production(config, Arc::new(SubprocessWorkerLauncher::new()))
            .expect("the production registry opens the plugin root");
    let remote_source: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
    let announced = Arc::clone(&remote_source);
    registry.set_checkout_remote_source(move |_repository| {
        announced.lock().expect("remote source").clone()
    });
    let governance = Arc::new(
        HostGovernanceService::open(socket_dir.path().join("state"))
            .await
            .expect("open the host-governance service"),
    );
    let state = DaemonState::new(
        HealthInfo::new("0.0.0-test"),
        registry.clone(),
        governance,
        support::overlay_registry(),
        pohunek_daemon::api::OverlaySource::Default,
    );
    let unix = ControlServer::bind_with_state(&socket, state.clone())
        .await
        .expect("unix server binds");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback tcp bind");
    let remote = RemoteServer::from_listener(listener, state);
    let addr = remote.local_addr();

    let (shutdown, rx) = oneshot::channel::<()>();
    let (unix_tx, unix_rx) = oneshot::channel::<()>();
    let (remote_tx, remote_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = rx.await;
        let _ = unix_tx.send(());
        let _ = remote_tx.send(());
    });
    let task = tokio::spawn(async move {
        let unix_serve = unix.serve(untracked("control_connections"), async move {
            let _ = unix_rx.await;
        });
        let remote_serve = remote.serve(untracked("remote_connections"), async move {
            let _ = remote_rx.await;
        });
        tokio::join!(unix_serve, remote_serve);
    });
    Servers {
        addr,
        socket,
        store,
        plugins,
        agents,
        shutdown,
        task,
        remote_source,
        registry,
        _dirs: (socket_dir, worker_home_dir),
    }
}

async fn connect_tcp(addr: SocketAddr) -> Framed<TcpStream, LinesCodec> {
    // The listener is bound before the server task starts, so the kernel queues
    // the connection until the accept loop runs.
    let stream = TcpStream::connect(addr)
        .await
        .expect("connect to the bound test tcp listener");
    Framed::new(stream, LinesCodec::new())
}

async fn connect_unix(socket: &Path) -> Framed<UnixStream, LinesCodec> {
    // The socket is bound before the server task starts, so the kernel queues
    // the connection until the accept loop runs.
    let stream = UnixStream::connect(socket)
        .await
        .expect("connect to the bound test socket");
    Framed::new(stream, LinesCodec::new())
}

async fn exchange<S>(framed: &mut Framed<S, LinesCodec>, method: &str, params: Value) -> Response
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request = ProtocolRequest::new("package-test", method, params).expect("valid request");
    let line = serde_json::to_string(&request).expect("serialize request");
    framed.send(line).await.expect("send");
    let reply = framed
        .next()
        .await
        .expect("a response line")
        .expect("response framing ok");
    serde_json::from_str(&reply).expect("parse response")
}

/// The decoded result of a successful call.
async fn call<S, T>(framed: &mut Framed<S, LinesCodec>, method: &str, params: Value) -> T
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: serde::de::DeserializeOwned,
{
    let value = exchange(framed, method, params)
        .await
        .into_result()
        .unwrap_or_else(|error| panic!("{method} failed: {error}"));
    serde_json::from_value(value).unwrap_or_else(|error| panic!("{method} result: {error}"))
}

/// The wire error code of a refused call.
async fn refusal<S>(framed: &mut Framed<S, LinesCodec>, method: &str, params: Value) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    refused(framed, method, params).await.code
}

/// The error of a refused call.
async fn refused<S>(
    framed: &mut Framed<S, LinesCodec>,
    method: &str,
    params: Value,
) -> protocol::ProtocolError
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    exchange(framed, method, params)
        .await
        .into_result()
        .expect_err("the call is refused")
}

fn descriptor(program: &str) -> String {
    format!(
        r#"schema = 1
id = "acme.runtime.pi"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "{RUNTIME}"
name = "Pi"
program = "{program}"
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

const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

fn entries(program: &str) -> Vec<ArchiveEntry> {
    entries_of(&descriptor(program))
}

fn entries_of(descriptor: &str) -> Vec<ArchiveEntry> {
    entries_with_manifest(descriptor, DETECT_MANIFEST)
}

fn entries_with_manifest(descriptor: &str, manifest: &str) -> Vec<ArchiveEntry> {
    vec![
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: descriptor.as_bytes().to_vec(),
            executable: false,
        },
        ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: manifest.as_bytes().to_vec(),
            executable: false,
        },
    ]
}

fn archive(program: &str) -> (Vec<u8>, PackageDigest) {
    archive_of(&entries(program))
}

fn archive_of(entries: &[ArchiveEntry]) -> (Vec<u8>, PackageDigest) {
    let bytes = build_archive(entries, &Limits::DEFAULT).expect("archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("archive reads")
        .digest()
        .clone();
    (bytes, digest)
}

async fn supported_agents<S>(framed: &mut Framed<S, LinesCodec>) -> Vec<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let capabilities: HostCapabilities = call(framed, method::HOST_INSPECT, Value::Null).await;
    capabilities.supported_agents
}

#[tokio::test]
async fn every_package_method_is_refused_on_a_remote_connection() {
    let servers = spawn_servers("pkg-remote").await;
    let (_bytes, digest) = archive("/bin/sh");
    let mut remote = connect_tcp(servers.addr).await;
    let mut local = connect_unix(&servers.socket).await;

    for (name, params) in package_methods(&digest) {
        assert_eq!(
            refusal(&mut remote, name, params).await,
            LOCAL_ONLY,
            "{name}"
        );
    }
    // The refusal precedes parameter parsing, so even a malformed request is
    // answered with the transport error and not an echo of its parameters.
    assert_eq!(
        refusal(
            &mut remote,
            method::PACKAGE_INSTALL,
            json!({ "argv": ["sh"] })
        )
        .await,
        LOCAL_ONLY
    );

    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "the remote calls changed nothing"
    );
    assert_eq!(listed.generation, 0);
    assert!(
        supported_agents(&mut remote)
            .await
            .contains(&"shell".to_owned()),
        "non-package methods stay available remotely"
    );

    drop(remote);
    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn the_lifecycle_runs_end_to_end_on_the_local_socket() {
    let servers = spawn_servers("pkg-lifecycle").await;
    let mut local = connect_unix(&servers.socket).await;
    let mut remote = connect_tcp(servers.addr).await;
    let (bytes, digest) = archive("/bin/sh");
    let archive_path = servers.plugins.with_file_name("pi.tar.zst");
    std::fs::write(&archive_path, &bytes).expect("write the archive");
    let install = |dry_run: bool| {
        json!({
            "archive_path": archive_path,
            "trust": { "kind": "explicit_digest", "digest": digest },
            "enable": false,
            "select": false,
            "dry_run": dry_run
        })
    };

    let preview: PackageInstallResult =
        call(&mut local, method::PACKAGE_INSTALL, install(true)).await;
    assert_eq!(preview.status, PackageInstallStatus::Preview);
    let installed: PackageInstallResult =
        call(&mut local, method::PACKAGE_INSTALL, install(false)).await;
    assert_eq!(installed.status, PackageInstallStatus::Installed);
    assert!(installed.reloaded);
    assert!(!supported_agents(&mut remote)
        .await
        .contains(&RUNTIME.to_owned()));

    let enabled: PackageChangeResult = call(
        &mut local,
        method::PACKAGE_SET_ENABLED,
        json!({ "digest": digest, "enabled": true }),
    )
    .await;
    assert!(enabled.package.enabled);
    let selected: PackageChangeResult = call(
        &mut local,
        method::PACKAGE_SELECT,
        json!({ "digest": digest }),
    )
    .await;
    assert!(selected.package.selected);
    assert!(
        supported_agents(&mut remote)
            .await
            .contains(&RUNTIME.to_owned()),
        "host.inspect reflects the reloaded registry"
    );

    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert_eq!(listed.packages.len(), 1);
    let inspected: PackageInspectResult = call(
        &mut local,
        method::PACKAGE_INSPECT,
        json!({ "digest": digest }),
    )
    .await;
    assert_eq!(inspected.runtime.expect("descriptor").program, "/bin/sh");
    let doctor: PackageDoctorResult = call(&mut local, method::PACKAGE_DOCTOR, Value::Null).await;
    assert!(doctor.findings.is_empty());

    // The host has no agents directory, so there is no profile to bind.
    assert_eq!(
        refusal(
            &mut local,
            method::PACKAGE_BIND_PROFILE,
            json!({ "profile": "work", "dry_run": true })
        )
        .await,
        "package_profile_not_found"
    );
    assert_eq!(
        refusal(
            &mut local,
            method::PACKAGE_UNINSTALL,
            json!({ "digest": format!("sha256:{}", "0".repeat(64)), "remove_modified": false })
        )
        .await,
        "package_not_installed"
    );
    let removed: PackageUninstallResult = call(
        &mut local,
        method::PACKAGE_UNINSTALL,
        json!({ "digest": digest, "remove_modified": false }),
    )
    .await;
    assert!(removed.reloaded);
    assert!(!supported_agents(&mut remote)
        .await
        .contains(&RUNTIME.to_owned()));

    drop(remote);
    drop(local);
    servers.stop().await;
}

/// `[config_home]` table the transcript scenarios declare next to
/// `[transcripts]`.
const CONFIG_HOME_TABLE: &str = "\n[config_home]\nenv = \"ACME_HOME\"\ndefault = \".acme\"\n";

/// A `[transcripts]` table naming `parser` and `dir`.
fn transcripts_table(parser: &str, dir: &str) -> String {
    format!("\n[transcripts]\nparser = \"{parser}\"\ndir = \"{dir}\"\n")
}

/// Installs the fixture descriptor followed by `tables` through
/// `package.install` and returns the wire result.
async fn install_with_tables<S>(
    framed: &mut Framed<S, LinesCodec>,
    plugins: &Path,
    name: &str,
    tables: &str,
) -> Result<Value, protocol::ProtocolError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    install_descriptor(
        framed,
        plugins,
        name,
        &format!("{}{tables}", descriptor("/bin/sh")),
        false,
    )
    .await
    .1
}

/// Installs a package whose descriptor is `document` through
/// `package.install`, only validating it when `dry_run`, and returns its
/// archive digest with the wire result.
async fn install_descriptor<S>(
    framed: &mut Framed<S, LinesCodec>,
    plugins: &Path,
    name: &str,
    document: &str,
    dry_run: bool,
) -> (PackageDigest, Result<Value, protocol::ProtocolError>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    install_with_manifest(framed, plugins, name, document, DETECT_MANIFEST, dry_run).await
}

async fn install_with_manifest<S>(
    framed: &mut Framed<S, LinesCodec>,
    plugins: &Path,
    name: &str,
    document: &str,
    manifest: &str,
    dry_run: bool,
) -> (PackageDigest, Result<Value, protocol::ProtocolError>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (bytes, digest) = archive_of(&entries_with_manifest(document, manifest));
    let archive_path = plugins.with_file_name(format!("{name}.tar.zst"));
    std::fs::write(&archive_path, &bytes).expect("write the archive");
    let result = exchange(
        framed,
        method::PACKAGE_INSTALL,
        json!({
            "archive_path": archive_path,
            "trust": { "kind": "explicit_digest", "digest": digest },
            "enable": true,
            "select": true,
            "dry_run": dry_run
        }),
    )
    .await
    .into_result();
    (digest, result)
}

/// `project.candidates` serves the pohunek source on a remote connection and
/// refuses the history source there before it reads any history.
#[tokio::test]
async fn project_candidates_history_is_served_on_the_local_socket_only() {
    let servers = spawn_servers_over("pkg-candidates", Some("")).await;
    let mut local = connect_unix(&servers.socket).await;
    let mut remote = connect_tcp(servers.addr).await;

    let refused = refused(
        &mut remote,
        method::PROJECT_CANDIDATES,
        json!({ "sources": ["history"] }),
    )
    .await;
    assert_eq!(refused.code, LOCAL_ONLY);
    for sources in [json!(["history"]), json!(["pohunek", "history"])] {
        let result: protocol::ProjectCandidatesResult = call(
            &mut local,
            method::PROJECT_CANDIDATES,
            json!({ "sources": sources }),
        )
        .await;
        assert!(result.candidates.is_empty());
    }
    for params in [Value::Null, json!({ "sources": ["pohunek"] })] {
        let result: protocol::ProjectCandidatesResult =
            call(&mut remote, method::PROJECT_CANDIDATES, params).await;
        assert!(result.candidates.is_empty());
        assert_eq!(result.dropped, 0);
    }

    drop(remote);
    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_descriptor_with_an_invalid_transcript_declaration_is_refused() {
    use pohunek_daemon::agent::host::{MAX_TRANSCRIPT_DIR_BYTES, MAX_TRANSCRIPT_DIR_COMPONENTS};

    let servers = spawn_servers("pkg-transcripts").await;
    let mut local = connect_unix(&servers.socket).await;
    let declared =
        |parser: &str, dir: &str| format!("{CONFIG_HOME_TABLE}{}", transcripts_table(parser, dir));
    let refused = [
        ("unknown-parser", declared("jsonl-head-v9", "sessions")),
        ("escaping-dir", declared("jsonl-head-v1", "../sessions")),
        (
            "nested-escape",
            declared("jsonl-head-v1", "logs/../../sessions"),
        ),
        ("absolute-dir", declared("jsonl-head-v1", "/var/log/acme")),
        ("empty-segment", declared("jsonl-head-v1", "logs//chats")),
        ("dot-segment", declared("jsonl-head-v1", "./sessions")),
        ("empty-dir", declared("jsonl-head-v1", "")),
        (
            "over-long-dir",
            declared("jsonl-head-v1", &"a".repeat(MAX_TRANSCRIPT_DIR_BYTES + 1)),
        ),
        (
            "over-deep-dir",
            declared(
                "jsonl-head-v1",
                &["a"; MAX_TRANSCRIPT_DIR_COMPONENTS + 1].join("/"),
            ),
        ),
        (
            "without-config-home",
            transcripts_table("jsonl-head-v1", "sessions"),
        ),
        (
            "unknown-key",
            format!("{}extra = 1\n", declared("jsonl-head-v1", "sessions")),
        ),
        (
            "missing-dir",
            format!("{CONFIG_HOME_TABLE}\n[transcripts]\nparser = \"jsonl-head-v1\"\n"),
        ),
    ];
    for (name, tables) in &refused {
        let error = install_with_tables(&mut local, &servers.plugins, name, tables)
            .await
            .expect_err(name);
        assert_eq!(error.code, "package_descriptor_invalid", "{name}");
    }
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    install_with_tables(
        &mut local,
        &servers.plugins,
        "valid",
        &declared("jsonl-head-v1", "logs/chats"),
    )
    .await
    .expect("a valid declaration installs");
    assert!(supported_agents(&mut local)
        .await
        .contains(&RUNTIME.to_owned()));

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_descriptor_requiring_an_unknown_runtime_feature_is_refused() {
    let servers = spawn_servers("pkg-requires").await;
    let mut local = connect_unix(&servers.socket).await;
    let plain = descriptor("/bin/sh");
    let requiring = |requires: &str| {
        plain.replace(
            "runtime_api = 1\n",
            &format!("runtime_api = 1\nrequires = {requires}\n"),
        )
    };

    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "unknown-feature",
        &requiring("[\"later-v1\"]"),
        false,
    )
    .await;
    let error = result.expect_err("an unknown feature is refused");
    assert_eq!(error.code, "runtime_feature_unsupported");
    let many = (0..=16)
        .map(|number| format!("\"f-v{number}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let malformed = [
        ("not-an-id", "[\"Later\"]".to_owned()),
        ("no-version", "[\"later\"]".to_owned()),
        ("duplicate", "[\"a-v1\", \"a-v1\"]".to_owned()),
        ("too-long", format!("[\"{}-v1\"]", "a".repeat(64))),
        ("too-many", format!("[{many}]")),
    ];
    for (name, requires) in &malformed {
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            name,
            &requiring(requires),
            false,
        )
        .await;
        assert_eq!(
            result.expect_err(name).code,
            "package_descriptor_invalid",
            "{name}"
        );
    }
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "empty-requires",
        &requiring("[]"),
        false,
    )
    .await;
    result.expect("an empty requires installs");

    drop(local);
    servers.stop().await;
}

/// The Pi-shaped descriptor listing `requires` (a TOML array body) before its
/// first table, followed by `tables`.
fn descriptor_requiring(requires: &str, tables: &str) -> String {
    descriptor("/bin/sh").replace(
        "runtime_api = 1\n",
        &format!("runtime_api = 1\nrequires = [{requires}]\n"),
    ) + tables
}

/// A `[config_home]` table declaring `subdir` below `ACME_HOME`.
fn subdir_home_table(subdir: &str) -> String {
    format!("\n[config_home]\nenv = \"ACME_HOME\"\nsubdir = \"{subdir}\"\ndefault = \".acme\"\n")
}

#[tokio::test]
async fn a_config_home_subdir_installs_only_with_its_feature_and_as_a_plain_path() {
    use pohunek_daemon::agent::host::{MAX_TRANSCRIPT_DIR_BYTES, MAX_TRANSCRIPT_DIR_COMPONENTS};

    let servers = spawn_servers("pkg-subdir").await;
    let mut local = connect_unix(&servers.socket).await;
    let feature = "\"config-home-subdir-v1\"";

    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "without-requires",
        &descriptor_requiring("", &subdir_home_table("muse")),
        false,
    )
    .await;
    assert_eq!(
        result.expect_err("a gated key without requires").code,
        "runtime_feature_unsupported"
    );

    let too_long = "a".repeat(MAX_TRANSCRIPT_DIR_BYTES + 1);
    let too_deep = ["a"; MAX_TRANSCRIPT_DIR_COMPONENTS + 1].join("/");
    for (name, subdir) in [
        ("absolute", "/etc"),
        ("empty", ""),
        ("parent", "../muse"),
        ("nested-parent", "a/../b"),
        ("empty-segment", "a//b"),
        ("tilde", "~/muse"),
        ("glob", "mu*"),
        ("space", "a b"),
        ("too-long", too_long.as_str()),
        ("too-deep", too_deep.as_str()),
    ] {
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            name,
            &descriptor_requiring(feature, &subdir_home_table(subdir)),
            false,
        )
        .await;
        assert_eq!(
            result.expect_err(name).code,
            "package_descriptor_invalid",
            "{name}"
        );
    }
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "valid",
        &descriptor_requiring(feature, &subdir_home_table("muse")),
        false,
    )
    .await;
    result.expect("a plain subdir with its feature installs");

    drop(local);
    servers.stop().await;
}

/// `[config_home]` plus an `[agent_skill]` table naming `root` and `dir`.
fn agent_skill_tables(root: &str, dir: &str) -> String {
    format!("{CONFIG_HOME_TABLE}\n[agent_skill]\nroot = \"{root}\"\ndir = \"{dir}\"\n")
}

#[tokio::test]
async fn an_agent_skill_location_installs_only_with_its_feature_and_as_a_closed_skills_path() {
    let servers = spawn_servers("pkg-agent-skill").await;
    let mut local = connect_unix(&servers.socket).await;
    let feature = "\"agent-skill-v1\"";

    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "without-requires",
        &descriptor_requiring("", &agent_skill_tables("config_home", "skills")),
        false,
    )
    .await;
    assert_eq!(
        result.expect_err("a gated table without requires").code,
        "runtime_feature_unsupported"
    );

    for (name, root, dir) in [
        ("unknown-root", "system", "skills"),
        ("config-home-not-skills", "config_home", "plugins"),
        ("config-home-parent", "config_home", "../skills"),
        ("config-home-absolute", "config_home", "/skills"),
        ("user-home-other", "user_home", "skills"),
        ("user-home-nested", "user_home", ".agents/skills/x"),
    ] {
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            name,
            &descriptor_requiring(feature, &agent_skill_tables(root, dir)),
            false,
        )
        .await;
        assert_eq!(
            result.expect_err(name).code,
            "package_descriptor_invalid",
            "{name}"
        );
    }
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    for (name, root, dir) in [
        ("valid-config-home", "config_home", "tools/skills"),
        ("valid-user-home", "user_home", ".agents/skills"),
    ] {
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            name,
            &descriptor_requiring(feature, &agent_skill_tables(root, dir)),
            true,
        )
        .await;
        result.unwrap_or_else(|error| panic!("{name} validates: {error:?}"));
    }

    drop(local);
    servers.stop().await;
}

const DATA_ROOT_FEATURE: &str = "\"transcripts-data-root-v1\"";

/// A `[transcripts]` table whose tree lies below a root of its own.
fn data_root_transcripts(parser: &str, extra: &str) -> String {
    format!(
        "\n[transcripts]\nparser = \"{parser}\"\ndir = \"acme/sessions\"\n\
         root_env = \"ACME_DATA\"\nroot_home = \".local/share\"\n{extra}"
    )
}

/// The Pi-shaped descriptor with a `hook` strategy declaring `existence`.
fn hook_descriptor(requires: &str, existence: &str, tables: &str) -> String {
    descriptor_requiring(requires, tables)
        .replace(
            "strategy = \"assigned\"\nlaunch_args = [\"--session-id\", \"{reference}\"]\n",
            "strategy = \"hook\"\n",
        )
        .replace("check = \"none\"\n", &format!("{existence}\n"))
}

/// A `[native_reference.existence]` body naming the conversation directory.
const DIR_NAME_EXISTENCE: &str = "check = \"file\"\nroot_env = \"ACME_DATA\"\n\
     root_home = \".local/share\"\ndir = \"acme/sessions\"\ndir_name = \"{reference}\"\n\
     file_name = \"session.jsonl\"\nmax_depth = 3";

/// Installs each `(name, document)` and asserts the result code of all of them.
async fn assert_install_codes(
    local: &mut Framed<UnixStream, LinesCodec>,
    plugins: &Path,
    cases: &[(&str, String)],
    code: &str,
) {
    for (name, document) in cases {
        let (_digest, result) = install_descriptor(local, plugins, name, document, false).await;
        assert_eq!(result.expect_err(name).code, code, "{name}");
    }
}

/// Descriptors whose `[transcripts]` table (derived from `plain`) is invalid.
fn invalid_data_root_tables(plain: &str) -> Vec<(&'static str, String)> {
    [
        (
            "only-root-env",
            "\n[transcripts]\nparser = \"jsonl-head-v1\"\ndir = \"a\"\nroot_env = \"ACME_DATA\"\n"
                .to_owned(),
        ),
        (
            "only-root-home",
            "\n[transcripts]\nparser = \"jsonl-head-v1\"\ndir = \"a\"\nroot_home = \".local\"\n"
                .to_owned(),
        ),
        (
            "no-root-no-home",
            "\n[transcripts]\nparser = \"jsonl-head-v1\"\ndir = \"a\"\n".to_owned(),
        ),
        ("lower-env", plain.replace("ACME_DATA", "acme_data")),
        ("reserved-env", plain.replace("ACME_DATA", "POHUNEK_DATA")),
        ("absolute-home", plain.replace(".local/share", "/srv/share")),
        ("parent-home", plain.replace(".local/share", "../share")),
        (
            "dir-too-deep",
            plain.replace("acme/sessions", &["a"; 9].join("/")),
        ),
        (
            "depth-zero",
            data_root_transcripts("jsonl-head-v1", "max_depth = 0\n"),
        ),
        (
            "depth-over",
            data_root_transcripts("jsonl-head-v1", "max_depth = 7\n"),
        ),
        (
            "depth-huge",
            data_root_transcripts("jsonl-head-v1", "max_depth = 300\n"),
        ),
        (
            "depth-negative",
            data_root_transcripts("jsonl-head-v1", "max_depth = -1\n"),
        ),
        (
            "depth-text",
            data_root_transcripts("jsonl-head-v1", "max_depth = \"4\"\n"),
        ),
    ]
    .into_iter()
    .map(|(name, tables)| (name, descriptor_requiring(DATA_ROOT_FEATURE, &tables)))
    .collect()
}

#[tokio::test]
async fn a_transcript_root_of_its_own_installs_only_with_its_feature_and_valid() {
    let servers = spawn_servers("pkg-data-root").await;
    let mut local = connect_unix(&servers.socket).await;
    let plain = data_root_transcripts("jsonl-head-v1", "");

    // Every key and the parser the feature owns is refused without `requires`.
    let gated = [
        ("root-keys", plain.clone()),
        (
            "max-depth",
            "\n[config_home]\nenv = \"ACME_HOME\"\ndefault = \".acme\"\n\
             [transcripts]\nparser = \"jsonl-head-v1\"\ndir = \"sessions\"\nmax_depth = 2\n"
                .to_owned(),
        ),
        (
            "parser",
            "\n[config_home]\nenv = \"ACME_HOME\"\ndefault = \".acme\"\n\
             [transcripts]\nparser = \"muse-session-v1\"\ndir = \"sessions\"\n"
                .to_owned(),
        ),
    ]
    .map(|(name, tables)| (name, descriptor_requiring("", &tables)));
    assert_install_codes(
        &mut local,
        &servers.plugins,
        &gated,
        "runtime_feature_unsupported",
    )
    .await;

    let refused = invalid_data_root_tables(&plain);
    assert_install_codes(
        &mut local,
        &servers.plugins,
        &refused,
        "package_descriptor_invalid",
    )
    .await;
    // The variable that relocates the root is not a `[launch_env]` name.
    let both = format!("{DATA_ROOT_FEATURE}, \"launch-env-v1\"");
    let owned = [(
        "launch-env-root",
        descriptor_requiring(
            &both,
            &(plain.replace("ACME_DATA", "PI_DATA") + "\n[launch_env]\nPI_DATA = \"1\"\n"),
        ),
    )];
    assert_install_codes(
        &mut local,
        &servers.plugins,
        &owned,
        "package_descriptor_invalid",
    )
    .await;
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    let accepted = [
        ("own-root", plain),
        (
            "bounded-walk",
            data_root_transcripts("jsonl-head-v1", "max_depth = 6\n"),
        ),
        (
            "muse-parser",
            data_root_transcripts("muse-session-v1", "max_depth = 4\n"),
        ),
    ];
    for (index, (name, tables)) in accepted.iter().enumerate() {
        let document = descriptor_requiring(DATA_ROOT_FEATURE, tables)
            .replace("version = \"1.0.0\"", &format!("version = \"1.0.{index}\""));
        let (_digest, result) =
            install_descriptor(&mut local, &servers.plugins, name, &document, false).await;
        result.unwrap_or_else(|error| panic!("{name} installs: {error:?}"));
    }

    drop(local);
    servers.stop().await;
}

const HEAD_PATHS_FEATURE: &str = "\"transcripts-head-paths-v1\"";

/// A `[transcripts]` table below the config home with the given extra keys.
fn head_paths_transcripts(parser: &str, extra: &str) -> String {
    format!(
        "\n[config_home]\nenv = \"ACME_HOME\"\ndefault = \".acme\"\n\
         [transcripts]\nparser = \"{parser}\"\ndir = \"sessions\"\n{extra}"
    )
}

#[tokio::test]
async fn transcript_head_paths_install_only_with_their_feature_and_valid() {
    let servers = spawn_servers("pkg-head-paths").await;
    let mut local = connect_unix(&servers.socket).await;
    let valid = "id_path = [\"payload\", \"id\"]\ncwd_path = [\"payload\", \"cwd\"]\n";

    // Either key is refused without `requires`.
    let gated = [
        (
            "id-path",
            head_paths_transcripts("jsonl-head-v1", "id_path = [\"payload\", \"id\"]\n"),
        ),
        (
            "cwd-path",
            head_paths_transcripts("jsonl-head-v1", "cwd_path = [\"payload\", \"cwd\"]\n"),
        ),
    ]
    .map(|(name, tables)| (name, descriptor_requiring("", &tables)));
    assert_install_codes(
        &mut local,
        &servers.plugins,
        &gated,
        "runtime_feature_unsupported",
    )
    .await;

    let refused = [
        ("empty-path", "id_path = []\n".to_owned()),
        (
            "too-deep",
            "id_path = [\"a\", \"b\", \"c\", \"d\"]\n".to_owned(),
        ),
        ("empty-key", "id_path = [\"payload\", \"\"]\n".to_owned()),
        ("dotted-key", "id_path = [\"payload.id\"]\n".to_owned()),
        ("indexed-key", "cwd_path = [\"items[0]\"]\n".to_owned()),
        ("wildcard-key", "cwd_path = [\"*\"]\n".to_owned()),
        ("long-key", format!("id_path = [\"{}\"]\n", "k".repeat(33))),
        ("text-path", "id_path = \"payload.id\"\n".to_owned()),
        ("number-key", "id_path = [1]\n".to_owned()),
    ]
    .map(|(name, extra)| {
        (
            name,
            descriptor_requiring(
                HEAD_PATHS_FEATURE,
                &head_paths_transcripts("jsonl-head-v1", &extra),
            ),
        )
    });
    assert_install_codes(
        &mut local,
        &servers.plugins,
        &refused,
        "package_descriptor_invalid",
    )
    .await;
    // The fixed layout of the other parser takes no key paths.
    let other_parser = [(
        "muse-parser",
        descriptor_requiring(
            &format!("{HEAD_PATHS_FEATURE}, \"transcripts-data-root-v1\""),
            &head_paths_transcripts("muse-session-v1", valid),
        ),
    )];
    assert_install_codes(
        &mut local,
        &servers.plugins,
        &other_parser,
        "package_descriptor_invalid",
    )
    .await;
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    let document = descriptor_requiring(
        HEAD_PATHS_FEATURE,
        &head_paths_transcripts("jsonl-head-v1", valid),
    );
    let (_digest, result) =
        install_descriptor(&mut local, &servers.plugins, "nested", &document, false).await;
    result.expect("a valid head path with its feature installs");

    drop(local);
    servers.stop().await;
}

/// Hook descriptors whose directory-name existence is invalid.
fn invalid_dir_name_existences() -> Vec<(&'static str, String)> {
    let with = |existence: &str| hook_descriptor(DATA_ROOT_FEATURE, existence, "");
    let without = |field: &str| {
        DIR_NAME_EXISTENCE
            .lines()
            .filter(|line| !line.starts_with(field))
            .collect::<Vec<_>>()
            .join("\n")
    };
    [
        (
            "assigned-strategy",
            descriptor_requiring(DATA_ROOT_FEATURE, "")
                .replace("check = \"none\"\n", &format!("{DIR_NAME_EXISTENCE}\n")),
        ),
        (
            "other-dir-name",
            with(&DIR_NAME_EXISTENCE.replace("dir_name = \"{reference}\"", "dir_name = \"x\"")),
        ),
        (
            "name-match",
            with(&format!("{DIR_NAME_EXISTENCE}\nname_match = \"exact\"")),
        ),
        (
            "store-env",
            with(&format!(
                "{DIR_NAME_EXISTENCE}\nstore_env = \"ACME_STORE\"\nstore_max_depth = 1"
            )),
        ),
        (
            "relocating-args",
            with(&format!(
                "{DIR_NAME_EXISTENCE}\nrelocating_args = [\"--store\"]"
            )),
        ),
        (
            "relocating-settings",
            with(&format!(
                "{DIR_NAME_EXISTENCE}\nrelocating_settings = [{{ base = \"root\", path = \"s.json\", key = \"k\" }}]"
            )),
        ),
        (
            "store-depth-only",
            with(&format!("{DIR_NAME_EXISTENCE}\nstore_max_depth = 1")),
        ),
        (
            "placeholder-file",
            with(&DIR_NAME_EXISTENCE.replace("session.jsonl", "{reference}.jsonl")),
        ),
        (
            "path-file",
            with(&DIR_NAME_EXISTENCE.replace("session.jsonl", "a/session.jsonl")),
        ),
        ("no-file", with(&without("file_name"))),
        ("no-depth", with(&without("max_depth"))),
        (
            "deep",
            with(&DIR_NAME_EXISTENCE.replace("max_depth = 3", "max_depth = 5")),
        ),
        (
            "no-root",
            with(&without("root_env").replace("root_home = \".local/share\"\n", "")),
        ),
        (
            "check-none",
            with(&DIR_NAME_EXISTENCE.replace("check = \"file\"", "check = \"none\"")),
        ),
    ]
    .into()
}

#[tokio::test]
async fn a_directory_name_existence_installs_only_for_a_hook_runtime_with_its_feature() {
    let servers = spawn_servers("pkg-dir-name").await;
    let mut local = connect_unix(&servers.socket).await;

    let gated = [(
        "without-requires",
        hook_descriptor("", DIR_NAME_EXISTENCE, ""),
    )];
    assert_install_codes(
        &mut local,
        &servers.plugins,
        &gated,
        "runtime_feature_unsupported",
    )
    .await;

    let with = |existence: &str| hook_descriptor(DATA_ROOT_FEATURE, existence, "");
    let refused = invalid_dir_name_existences();
    assert_install_codes(
        &mut local,
        &servers.plugins,
        &refused,
        "package_descriptor_invalid",
    )
    .await;
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "valid",
        &with(DIR_NAME_EXISTENCE),
        false,
    )
    .await;
    result.expect("a directory-name existence of a hook runtime installs");

    drop(local);
    servers.stop().await;
}

/// Writes the executable shell `script` next to the plugin root and returns
/// its path.
fn write_program(plugins: &Path, name: &str, script: &str) -> PathBuf {
    let path = plugins.with_file_name(name);
    std::fs::write(&path, script).expect("write the program");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make the program executable");
    path
}

/// A probe program that prints `output` for any arguments.
fn probe_program(plugins: &Path, name: &str, output: &str) -> PathBuf {
    write_program(
        plugins,
        name,
        &format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n"),
    )
}

/// The Pi-shaped descriptor at `version`, launching `program` and probing it
/// with `parser` over `fixture {version}` lines for the range `[min, below)`.
/// `requires` is a TOML array body.
fn probed_descriptor(
    program: &Path,
    version: &str,
    parser: &str,
    range: (&str, &str),
    requires: &str,
) -> String {
    let (min, below) = range;
    let probe = format!(
        "version_probe = {{ parser = \"{parser}\", args = [\"--version\"], min = \"{min}\", below = \"{below}\", line = \"fixture {{version}}\" }}\n"
    );
    descriptor(&program.display().to_string())
        .replace("version = \"1.0.0\"", &format!("version = \"{version}\""))
        .replace(
            "prompt_arg = true\n",
            &format!("prompt_arg = true\n{probe}"),
        )
        .replace(
            "runtime_api = 1\n",
            &format!("runtime_api = 1\nrequires = [{requires}]\n"),
        )
}

/// The inventory entry `host.inspect` reports for the fixture runtime.
async fn inspected_runtime<S>(framed: &mut Framed<S, LinesCodec>) -> protocol::AgentRuntime
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let capabilities: HostCapabilities = call(framed, method::HOST_INSPECT, Value::Null).await;
    capabilities
        .runtimes
        .into_iter()
        .find(|runtime| runtime.agent == RUNTIME)
        .expect("the fixture runtime is inventoried")
}

/// The refusal of a `session.new` of the fixture runtime.
async fn refused_launch<S>(
    framed: &mut Framed<S, LinesCodec>,
    cwd: &Path,
) -> protocol::ProtocolError
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let params = protocol::SessionNewParams {
        client_request_id: None,
        name: None,
        agent: RUNTIME.to_owned(),
        cwd: Some(cwd.to_path_buf()),
        cols: 80,
        rows: 24,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        extended_input_ready_wait: None,
        branch_source: None,
        on_diverged: None,
        input_ack_timeout_ms: None,
        metadata: std::collections::BTreeMap::new(),
        secret_env: protocol::SessionSecretEnv::default(),
        context_package_digest: None,
    };
    exchange(
        framed,
        method::SESSION_NEW,
        serde_json::to_value(params).expect("session.new params"),
    )
    .await
    .into_result()
    .expect_err("the launch gate refuses the runtime")
}

const REVISION_PARSER: &str = "semver-revision-line-v1";
const REVISION_FEATURE: &str = "\"version-probe-revision-v1\"";

#[tokio::test]
async fn the_revision_parser_installs_only_with_its_feature_and_a_line_template() {
    let servers = spawn_servers("pkg-revision-install").await;
    let mut local = connect_unix(&servers.socket).await;
    let program = probe_program(&servers.plugins, "probe-gate", "fixture 1.4.13-R2");

    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "no-requires",
        &probed_descriptor(&program, "1.0.0", REVISION_PARSER, ("1.4.13", "2.0.0"), ""),
        false,
    )
    .await;
    assert_eq!(
        result.expect_err("the parser needs its feature").code,
        "runtime_feature_unsupported"
    );
    // A range that names a revision on a strict parser, and the revision parser
    // without a line template, are not valid descriptors.
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "strict-range",
        &probed_descriptor(
            &program,
            "1.0.0",
            "semver-line-v1",
            ("1.4.13-R1", "2.0.0"),
            "",
        ),
        false,
    )
    .await;
    assert_eq!(
        result.expect_err("a strict parser takes no revision").code,
        "package_descriptor_invalid"
    );
    let no_line = probed_descriptor(
        &program,
        "1.0.0",
        REVISION_PARSER,
        ("1.4.13", "2.0.0"),
        REVISION_FEATURE,
    )
    .replace(", line = \"fixture {version}\"", "");
    let (_digest, result) =
        install_descriptor(&mut local, &servers.plugins, "no-line", &no_line, false).await;
    assert_eq!(
        result.expect_err("the parser needs a line template").code,
        "package_descriptor_invalid"
    );
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn revision_versions_are_read_by_inspect_and_the_launch_gate() {
    let servers = spawn_servers("pkg-revision-read").await;
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();

    // (printed release, min, below, reported version, supported); a printed
    // release that is not read reports no version.
    let reads = |printed: &'static str, min, below, supported| {
        (printed, min, below, Some(printed), supported)
    };
    let mut cases = vec![
        reads("1.4.13-R2", "1.4.13", "2.0.0", true),
        reads("1.4.12-R9", "1.4.13", "2.0.0", false),
        reads("1.4.13", "1.4.13-R0", "2.0.0", true),
        reads("1.4.13-R0", "1.4.13", "2.0.0", true),
        reads("1.4.13", "1.4.13-R1", "2.0.0", false),
        reads("1.4.13-R2.1", "1.4.13-R2", "1.4.13-R2.1", false),
        reads("1.4.13-R2.1", "1.4.13-R2", "1.4.13-R3", true),
        reads("1.4.13-R2.0", "1.4.13-R2", "1.4.13-R2.1", true),
    ];
    for printed in [
        "1.4.13-r2",
        "1.4.13-R",
        "1.4.13-R02",
        "1.4.13-R1.2.3",
        "1.4.13-R1.",
        "1.4.13-R2-x",
        "1.4.13-R4294967296",
        "1.4.13-R99999999999999999999999999999999999999999999999999999999999",
        "1.4.13+b",
        "1.4-R2",
    ] {
        cases.push((printed, "1.4.13", "2.0.0", None, false));
    }
    for (index, (printed, min, below, version, supported)) in cases.into_iter().enumerate() {
        let program = probe_program(
            &servers.plugins,
            &format!("probe-{index}"),
            &format!("fixture {printed}"),
        );
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            &format!("case-{index}"),
            &probed_descriptor(
                &program,
                &format!("1.0.{index}"),
                REVISION_PARSER,
                (min, below),
                REVISION_FEATURE,
            ),
            false,
        )
        .await;
        result.unwrap_or_else(|error| panic!("{printed}: the descriptor installs: {error:?}"));

        let runtime = inspected_runtime(&mut local).await;
        assert_eq!(
            runtime.version.as_deref(),
            version,
            "{printed} [{min}, {below})"
        );
        assert_eq!(
            runtime.supported,
            Some(supported),
            "{printed} [{min}, {below})"
        );

        // The launch gate refuses exactly what the inventory reports as
        // unsupported; a supported runtime is not launched here.
        if !supported {
            let error = refused_launch(&mut local, &cwd).await;
            assert_eq!(error.code, "agent_runtime_unsupported", "{printed}");
            let expected = if version.is_some() {
                "outside its runtime's supported range"
            } else {
                "does not match its runtime's declared format"
            };
            assert!(error.msg.contains(expected), "{printed}: {error:?}");
        }
    }

    // The strict parsers still refuse a revision.
    let program = probe_program(&servers.plugins, "probe-strict", "fixture 1.4.13-R2");
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "strict",
        &probed_descriptor(&program, "2.0.0", "semver-line-v1", ("1.4.13", "2.0.0"), ""),
        false,
    )
    .await;
    result.expect("the strict descriptor installs");
    let runtime = inspected_runtime(&mut local).await;
    assert_eq!((runtime.version, runtime.supported), (None, Some(false)));

    drop(local);
    servers.stop().await;
}

const COMPOSER_ABSENT_MANIFEST: &str = r#"[[rules]]
id = "question"
state = "blocked"
priority = 100
region = "whole_recent"
contains = "which option"
composer_absent = true
"#;

#[tokio::test]
async fn a_manifest_with_composer_absent_installs_only_with_its_feature() {
    let servers = spawn_servers("pkg-composer-absent").await;
    let mut local = connect_unix(&servers.socket).await;
    let program = probe_program(&servers.plugins, "probe-composer", "fixture 1.4.13");

    // The matcher belongs to the feature: without it the package is refused
    // the way a core that does not know the feature refuses it.
    let (_digest, result) = install_with_manifest(
        &mut local,
        &servers.plugins,
        "composer-no-requires",
        &probed_descriptor(&program, "1.0.0", "semver-line-v1", ("1.0.0", "2.0.0"), ""),
        COMPOSER_ABSENT_MANIFEST,
        false,
    )
    .await;
    assert_eq!(
        result.expect_err("composer_absent needs its feature").code,
        "runtime_feature_unsupported"
    );
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    let (_digest, result) = install_with_manifest(
        &mut local,
        &servers.plugins,
        "composer-requires",
        &probed_descriptor(
            &program,
            "1.0.1",
            "semver-line-v1",
            ("1.0.0", "2.0.0"),
            "\"detect-composer-absent-v1\"",
        ),
        COMPOSER_ABSENT_MANIFEST,
        false,
    )
    .await;
    result.expect("the descriptor that requires the feature installs");

    drop(local);
    servers.stop().await;
}

/// A question dialog that replaces the composer, and the composer itself.
const COMPOSER_SCREEN_MANIFEST: &str = r#"[[rules]]
id = "question"
state = "blocked"
priority = 980
region = "whole_recent"
contains = ["which option do you want"]
composer_absent = true

[[rules]]
id = "composer"
state = "idle"
priority = 300
region = "whole_recent"
contains = ["ask anything"]
"#;

/// A program that draws the picker of a dialog whose list scrolled out of a
/// clipped region (the model text above it quotes a rule line, the dialog
/// draws none), or, once `<program>.composer` exists, the same question text
/// above a composer box.
fn dialog_program(plugins: &Path) -> PathBuf {
    write_program(
        plugins,
        "agent-dialog",
        concat!(
            "#!/bin/sh\n",
            "rule='────────────────────────────────────────'\n",
            "printf 'The plan has two steps:\\r\\n%s\\r\\n1. rename the module\\r\\n' \"$rule\"\n",
            "printf 'Which option do you want?\\r\\n'\n",
            "if [ -e \"$0.composer\" ]; then\n",
            "  printf '%s\\r\\nask anything\\r\\n%s\\r\\n' \"$rule\" \"$rule\"\n",
            "else\n",
            "  printf '> first option\\r\\n'\n",
            "fi\n",
            "exec sleep 600\n",
        ),
    )
}

/// The activity the daemon reports for session `id`, once it reports one.
async fn reported_activity<S>(
    framed: &mut Framed<S, LinesCodec>,
    id: &protocol::SessionId,
    expected: protocol::AgentActivity,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let shared = tokio::sync::Mutex::new(framed);
    let shared = &shared;
    pohunek_test_support::wait::wait_until(
        &format!("the session to read {expected:?}"),
        || async move {
            let mut framed = shared.lock().await;
            let info: protocol::SessionInfo =
                call(&mut **framed, method::SESSION_INSPECT, json!(id)).await;
            (info.activity == Some(expected)).then_some(())
        },
    )
    .await;
}

/// A runtime that refuses input while blocked and gates its question dialog on
/// `composer_absent` answers no typed input while the clipped picker is on
/// screen, and answers input again once the screen shows the composer box.
#[tokio::test]
async fn typed_input_is_refused_while_a_composer_absent_dialog_reads_blocked() {
    use protocol::AgentActivity;

    let servers = spawn_servers("pkg-composer-input").await;
    let mut local = connect_unix(&servers.socket).await;
    let program = dialog_program(&servers.plugins);
    let document = descriptor_requiring(
        "\"detect-composer-absent-v1\", \"input-refuse-while-blocked-v1\"",
        "",
    )
    .replace(
        "program = \"/bin/sh\"",
        &format!("program = \"{}\"", program.display()),
    )
    .replace(
        "text_policy = \"unrestricted\"\n",
        "text_policy = \"unrestricted\"\nrefuse_while_blocked = true\n",
    );
    let (_digest, installed) = install_with_manifest(
        &mut local,
        &servers.plugins,
        "agent-dialog",
        &document,
        COMPOSER_SCREEN_MANIFEST,
        false,
    )
    .await;
    installed.expect("the package installs");
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();

    let typed = json!({ "text": "1" });
    let input = |id: &protocol::SessionId| {
        let mut params = typed.clone();
        params["session_id"] = json!(id);
        params
    };

    let dialog = start_session(&mut local, RUNTIME, &cwd).await;
    reported_activity(&mut local, &dialog, AgentActivity::Blocked).await;
    assert_eq!(
        refusal(&mut local, method::SESSION_INPUT, input(&dialog)).await,
        "session_input_blocked"
    );
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(dialog)).await;

    // The same question text above a composer box is not a dialog.
    std::fs::write(format!("{}.composer", program.display()), "").expect("select the composer");
    let composer = start_session(&mut local, RUNTIME, &cwd).await;
    reported_activity(&mut local, &composer, AgentActivity::Idle).await;
    let accepted: Value = call(&mut local, method::SESSION_INPUT, input(&composer)).await;
    drop(accepted);
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(composer)).await;

    drop(local);
    servers.stop().await;
}

const OPEN_RANGE_FEATURE: &str = "\"version-probe-open-range-v1\"";

/// [`probed_descriptor`] for the open-ended range `[min, ..)`: `below` is
/// omitted.
fn open_range_descriptor(program: &Path, version: &str, min: &str, requires: &str) -> String {
    probed_descriptor(
        program,
        version,
        "semver-line-v1",
        (min, "99.0.0"),
        requires,
    )
    .replace(", below = \"99.0.0\"", "")
}

#[tokio::test]
async fn an_open_ended_range_installs_only_with_its_feature_and_as_a_bound_policy() {
    let servers = spawn_servers("pkg-open-range").await;
    let mut local = connect_unix(&servers.socket).await;
    let program = probe_program(&servers.plugins, "probe-open-gate", "fixture 1.4.13");

    // The omission of `below` belongs to the feature: without it the package
    // is refused the way a core that does not know the feature refuses it.
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "open-no-requires",
        &open_range_descriptor(&program, "1.0.0", "1.0.0", ""),
        false,
    )
    .await;
    assert_eq!(
        result.expect_err("an open range needs its feature").code,
        "runtime_feature_unsupported"
    );
    // An upper bound without a lower one, and a probe without a bound at all,
    // are not policies.
    for (name, document) in [
        (
            "below-only",
            probed_descriptor(
                &program,
                "1.0.0",
                "semver-line-v1",
                ("1.0.0", "2.0.0"),
                OPEN_RANGE_FEATURE,
            )
            .replace(", min = \"1.0.0\"", ""),
        ),
        (
            "neither",
            open_range_descriptor(&program, "1.0.0", "1.0.0", OPEN_RANGE_FEATURE)
                .replace(", min = \"1.0.0\"", ""),
        ),
    ] {
        let (_digest, result) =
            install_descriptor(&mut local, &servers.plugins, name, &document, false).await;
        assert_eq!(
            result.expect_err(name).code,
            "package_descriptor_invalid",
            "{name}"
        );
    }
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn an_open_ended_range_admits_every_release_from_min_and_names_the_minimum_it_refuses() {
    let servers = spawn_servers("pkg-open-range-read").await;
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();

    // (printed release, supported): the lower bound is inclusive and nothing
    // above it is refused, however far the major moves.
    let cases = [
        ("1.0.0", true),
        ("1.0.1", true),
        ("2.0.0", true),
        ("27.3.1", true),
        ("0.9.9", false),
    ];
    for (index, (printed, supported)) in cases.into_iter().enumerate() {
        let program = probe_program(
            &servers.plugins,
            &format!("probe-open-{index}"),
            &format!("fixture {printed}"),
        );
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            &format!("open-{index}"),
            &open_range_descriptor(
                &program,
                &format!("1.0.{index}"),
                "1.0.0",
                OPEN_RANGE_FEATURE,
            ),
            false,
        )
        .await;
        result.unwrap_or_else(|error| panic!("{printed}: the open range installs: {error:?}"));

        let runtime = inspected_runtime(&mut local).await;
        assert_eq!(runtime.version.as_deref(), Some(printed), "{printed}");
        assert_eq!(runtime.supported, Some(supported), "{printed}");
        if !supported {
            let error = refused_launch(&mut local, &cwd).await;
            assert_eq!(error.code, "agent_runtime_unsupported", "{printed}");
            assert!(
                error
                    .msg
                    .contains("reports version 0.9.9, below its runtime's supported minimum 1.0.0"),
                "{printed}: {error:?}"
            );
            assert!(
                !error.msg.contains('['),
                "an open range names no interval: {error:?}"
            );
            assert!(error.recover.is_some(), "{printed}: {error:?}");
        }
    }

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn the_revision_parser_takes_an_open_range_and_a_closed_range_with_the_feature_listed() {
    let servers = spawn_servers("pkg-open-revision").await;
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();
    for (index, (printed, supported)) in [
        ("1.4.13-R2", true),
        ("1.4.12-R9", false),
        ("127.0.0-R1", true),
    ]
    .into_iter()
    .enumerate()
    {
        let program = probe_program(
            &servers.plugins,
            &format!("probe-open-revision-{index}"),
            &format!("fixture {printed}"),
        );
        let document = probed_descriptor(
            &program,
            &format!("1.2.{index}"),
            REVISION_PARSER,
            ("1.4.13", "99.0.0"),
            &format!("{REVISION_FEATURE}, {OPEN_RANGE_FEATURE}"),
        )
        .replace(", below = \"99.0.0\"", "");
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            &format!("open-revision-{index}"),
            &document,
            false,
        )
        .await;
        result.unwrap_or_else(|error| panic!("{printed}: the revision range installs: {error:?}"));
        let runtime = inspected_runtime(&mut local).await;
        assert_eq!(runtime.version.as_deref(), Some(printed), "{printed}");
        assert_eq!(runtime.supported, Some(supported), "{printed}");
    }

    // Listing the feature does not oblige a descriptor to leave `below` out.
    let program = probe_program(&servers.plugins, "probe-closed", "fixture 2.0.0");
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "closed-with-feature",
        &probed_descriptor(
            &program,
            "1.1.0",
            "semver-line-v1",
            ("1.0.0", "2.0.0"),
            OPEN_RANGE_FEATURE,
        ),
        false,
    )
    .await;
    result.expect("a closed range installs with the feature listed");
    let runtime = inspected_runtime(&mut local).await;
    assert_eq!(
        runtime.supported,
        Some(false),
        "2.0.0 is the first release outside"
    );
    let error = refused_launch(&mut local, &cwd).await;
    assert!(
        error.msg.contains(
            "reports version 2.0.0, outside its runtime's supported range [1.0.0, 2.0.0)"
        ),
        "{error:?}"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_version_probe_receives_the_descriptors_launch_env() {
    let servers = spawn_servers("pkg-probe-env").await;
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();
    // The program prints an in-range release only when the runtime's launch
    // flag is set, so the printed release shows what the probe's environment held.
    let script = "#!/bin/sh\nif [ \"$PI_QUICK_VERSION\" = 1 ]; then printf '%s\\n' 'fixture 1.4.13'; else printf '%s\\n' 'fixture 0.0.1'; fi\n";
    let program = write_program(&servers.plugins, "probe-needs-flag", script);
    for (index, (value, version, supported)) in [("1", "1.4.13", true), ("0", "0.0.1", false)]
        .into_iter()
        .enumerate()
    {
        let mut document = probed_descriptor(
            &program,
            &format!("1.0.{index}"),
            "semver-line-v1",
            ("1.0.0", "2.0.0"),
            LAUNCH_ENV_FEATURE,
        );
        document.push_str(&launch_env_table(&[format!(
            "PI_QUICK_VERSION = \"{value}\""
        )]));
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            &format!("flag-{value}"),
            &document,
            false,
        )
        .await;
        result.unwrap_or_else(|error| panic!("flag {value} installs: {error:?}"));
        let runtime = inspected_runtime(&mut local).await;
        assert_eq!(
            (runtime.version.as_deref(), runtime.supported),
            (Some(version), Some(supported)),
            "the probe sees the descriptor's launch env (flag {value})"
        );
        // The launch gate probes under the same pairs: the runtime is admitted
        // with its flag and refused without it.
        if supported {
            let id = start_session(&mut local, RUNTIME, &cwd).await;
            let _: protocol::SessionStopResult =
                call(&mut local, method::SESSION_STOP, json!(id)).await;
        } else {
            let error = refused_launch(&mut local, &cwd).await;
            assert_eq!(error.code, "agent_runtime_unsupported", "flag {value}");
            assert!(
                error.msg.contains(
                    "reports version 0.0.1, outside its runtime's supported range [1.0.0, 2.0.0)"
                ),
                "flag {value}: {error:?}"
            );
        }
    }

    drop(local);
    servers.stop().await;
}

/// The check `name` of the daemon's doctor report.
async fn doctor_check<S>(framed: &mut Framed<S, LinesCodec>, name: &str) -> protocol::DoctorCheck
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let report: protocol::DaemonDoctorResult =
        call(framed, method::DAEMON_DOCTOR, Value::Null).await;
    report
        .report
        .checks
        .into_iter()
        .find(|check| check.name == name)
        .unwrap_or_else(|| panic!("the doctor report carries no {name} check"))
}

/// Writes the host profile `name` over the installed fixture package.
fn write_package_profile(servers: &Servers, name: &str, digest: &PackageDigest) {
    std::fs::write(
        servers.agents.join(format!("{name}.toml")),
        format!(
            "base = \"{RUNTIME}\"\npackage = \"acme.runtime.pi\"\ndigest = \"{}\"\n",
            digest.as_str()
        ),
    )
    .expect("write the profile");
}

#[tokio::test]
async fn the_doctor_names_the_minimum_an_open_ended_range_refuses_for_a_profile() {
    let servers = spawn_servers("pkg-doctor-open-range").await;
    let mut local = connect_unix(&servers.socket).await;
    let program = probe_program(&servers.plugins, "probe-below", "fixture 0.9.9");
    let (digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "below-min",
        &open_range_descriptor(&program, "1.0.0", "1.0.0", OPEN_RANGE_FEATURE),
        false,
    )
    .await;
    result.expect("the open range installs");
    write_package_profile(&servers, "below", &digest);

    let check = doctor_check(&mut local, "agent_profile_below").await;
    assert_eq!(check.status, protocol::DoctorStatus::Fail);
    assert!(
        check.detail.starts_with(
            "The configured executable reports version 0.9.9, below its runtime's supported minimum 1.0.0;"
        ),
        "{check:?}"
    );
    assert!(
        !check.detail.contains('['),
        "an open range names no interval: {check:?}"
    );

    drop(local);
    servers.stop().await;
}

const LAUNCH_ENV_FEATURE: &str = "\"launch-env-v1\"";

/// A `[launch_env]` table with the given `KEY = "value"` lines.
fn launch_env_table(lines: &[String]) -> String {
    format!("\n[launch_env]\n{}\n", lines.join("\n"))
}

/// Names a `pi` descriptor cannot declare: outside its namespace, or inside it
/// and saying where something lives, who it is or what to run.
const REFUSED_LAUNCH_ENV_NAMES: &[&str] = &[
    "SSH_AUTH_SOCK",
    "DISPLAY",
    "AWS_REGION",
    "OTHER_FLAG",
    "POHUNEK_X",
    "LD_PRELOAD",
    "DYLD_INSERT_LIBRARIES",
    "XDG_CONFIG_HOME",
    "LC_ALL",
    "PATH",
    "HOME",
    "TMPDIR",
    "NOTIFY_SOCKET",
    "pi_flag",
    "PI_",
    "PI_CONFIG_DIR",
    "PI_AUTH_PATH",
    // Every word after the namespace counts, last or not.
    "PI_EDITOR_DIRS",
    "PI_X_PATHS",
    "PI_RUN_CMD",
    "PI_BIN",
    "PI_NODE_OPTS",
    "PI_OPTIONS",
    "PI_ARGS",
    "PI_FLAGS",
    "PI_EXEC",
    "PI_SHELL",
    "PI_EDITOR",
    "PI_HOOK",
    "PI_PLUGIN",
    "PI_LIB",
    "PI_PRELOAD",
    "PI_CERT",
    "PI_CA",
    "PI_KEYRING",
    "PI_HOME_MODE",
    "PI_CONFIG_LEVEL",
    "PI_PATH_STYLE",
    "PI_PRELOAD_MODE",
    "PI_EXEC_TIMEOUT_MS",
    "PI_HOST",
    "PI_SESSION_ID",
    "PI_API_KEY",
    "PI_TOKEN",
    "PI_BAD-NAME",
];

#[tokio::test]
async fn a_launch_env_table_installs_only_with_its_feature_and_refuses_owned_names() {
    let servers = spawn_servers("pkg-launch-env").await;
    let mut local = connect_unix(&servers.socket).await;
    let pair = |name: &str| launch_env_table(&[format!("{name} = \"1\"")]);

    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "without-requires",
        &descriptor_requiring("", &pair("PI_NO_UPDATE")),
        false,
    )
    .await;
    assert_eq!(
        result.expect_err("a gated table without requires").code,
        "runtime_feature_unsupported"
    );

    let numbered: Vec<String> = (0..=16).map(|n| format!("PI_FLAG_{n} = \"1\"")).collect();
    // Names outside the runtime's namespace, names inside it that say where
    // something lives or who it is, then values and shapes outside the bounds.
    let mut refused: Vec<(String, String)> = REFUSED_LAUNCH_ENV_NAMES
        .iter()
        .map(|name| ((*name).to_owned(), pair(name)))
        .collect();
    refused.extend([
        (
            "config-home-variable".to_owned(),
            format!(
                "\n[config_home]\nenv = \"PI_STATE\"\ndefault = \".pi\"\n{}",
                pair("PI_STATE")
            ),
        ),
        ("too-many".to_owned(), launch_env_table(&numbered)),
        (
            "number-value".to_owned(),
            launch_env_table(&["PI_FLAG = 1".to_owned()]),
        ),
        (
            "nested-table".to_owned(),
            "\n[launch_env.PI_FLAG]\nvalue = \"1\"\n".to_owned(),
        ),
    ]);
    for (name, tables) in &refused {
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            name,
            &descriptor_requiring(LAUNCH_ENV_FEATURE, tables),
            false,
        )
        .await;
        assert_eq!(
            result.expect_err(name).code,
            "package_descriptor_invalid",
            "{name}"
        );
    }
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_runtime_in_a_reserved_namespace_has_no_launch_env() {
    use pohunek_daemon::agent::host::REFUSED_NAMESPACES;

    let servers = spawn_servers("pkg-launch-env-namespace").await;
    let mut local = connect_unix(&servers.socket).await;
    // One scenario per refused family: the runtime named after the family
    // declares a flag in its own namespace and is refused.
    for family in REFUSED_NAMESPACES {
        let runtime = family.trim_end_matches('_').to_ascii_lowercase();
        let name = format!("{}_FLAG", runtime.to_ascii_uppercase());
        let document = descriptor_requiring(
            LAUNCH_ENV_FEATURE,
            &launch_env_table(&[format!("{name} = \"1\"")]),
        )
        .replace("\nid = \"pi\"\n", &format!("\nid = \"{runtime}\"\n"));
        let (_digest, result) =
            install_descriptor(&mut local, &servers.plugins, &runtime, &document, false).await;
        assert_eq!(
            result.expect_err(&runtime).code,
            "package_descriptor_invalid",
            "{family}"
        );
    }

    // A namespace that only resembles a refused one is usable, and `-` and `.`
    // in the runtime id map to `_` in the namespace.
    for (index, (runtime, name)) in [
        ("sshx", "SSHX_FLAG"),
        ("my-agent.x", "MY_AGENT_X_FLAG"),
        // Only `GO_` and the exact Go variable names are refused.
        ("goose", "GOOSE_MODE"),
        // A word of the runtime's own namespace is not a word of the name.
        ("home-agent", "HOME_AGENT_FLAG"),
        ("containerx", "CONTAINERX_FLAG"),
    ]
    .into_iter()
    .enumerate()
    {
        let document = descriptor_requiring(
            LAUNCH_ENV_FEATURE,
            &launch_env_table(&[format!("{name} = \"1\"")]),
        )
        .replace("\nid = \"pi\"\n", &format!("\nid = \"{runtime}\"\n"))
        .replace("version = \"1.0.0\"", &format!("version = \"1.0.{index}\""));
        let (_digest, result) =
            install_descriptor(&mut local, &servers.plugins, runtime, &document, false).await;
        result.unwrap_or_else(|error| panic!("{runtime} installs: {error:?}"));
    }

    // The id itself stays installable; only its `[launch_env]` is refused.
    for (index, runtime) in ["glibc", "malloc", "lua", "dotnet", "cgo", "go"]
        .into_iter()
        .enumerate()
    {
        let document = descriptor_requiring("", "")
            .replace("\nid = \"pi\"\n", &format!("\nid = \"{runtime}\"\n"))
            .replace("version = \"1.0.0\"", &format!("version = \"1.1.{index}\""));
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            &format!("plain-{runtime}"),
            &document,
            false,
        )
        .await;
        result.unwrap_or_else(|error| panic!("{runtime} without a table installs: {error:?}"));
    }

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn launch_env_tables_within_the_bounds_install() {
    let servers = spawn_servers("pkg-launch-env-bounds").await;
    let mut local = connect_unix(&servers.socket).await;
    let pair = |name: &str| launch_env_table(&[format!("{name} = \"1\"")]);

    let sixteen: Vec<String> = (0..16).map(|n| format!("PI_FLAG_{n} = \"1\"")).collect();
    let accepted = [
        ("one-pair", pair("PI_NO_UPDATE")),
        // Whole words only: a refused word inside a longer one is fine.
        ("pathology", pair("PI_PATHOLOGY")),
        ("carbon", pair("PI_CARBON")),
        ("argsort", pair("PI_ARGSORT")),
        ("editorial", pair("PI_EDITORIAL")),
        ("log-level", pair("PI_LOG_LEVEL")),
        ("theme", pair("PI_THEME")),
        ("bounds", launch_env_table(&sixteen)),
    ];
    for (index, (name, tables)) in accepted.iter().enumerate() {
        let document = descriptor_requiring(LAUNCH_ENV_FEATURE, tables)
            .replace("version = \"1.0.0\"", &format!("version = \"1.0.{index}\""));
        let (_digest, result) =
            install_descriptor(&mut local, &servers.plugins, name, &document, false).await;
        result.unwrap_or_else(|error| panic!("{name} installs: {error:?}"));
    }

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn launch_env_values_are_flag_literals() {
    let servers = spawn_servers("pkg-launch-env-values").await;
    let mut local = connect_unix(&servers.socket).await;
    let long = format!("\"{}\"", "v".repeat(257));
    // TOML sources of values a flag cannot be: a program, a path, a host, a
    // port, a credential, an expansion, another spelling or no value.
    let refused = [
        "\"less\"",
        "\"x\"",
        "\"a/b\"",
        "\"https://h\"",
        "\"h:1\"",
        "\"user@h\"",
        "\"$HOME\"",
        "\"~\"",
        "\"\"",
        "\" 1\"",
        "\"1 \"",
        "\"TRUE\"",
        "\"yes\"",
        "\"2\"",
        "\"-1\"",
        "\"1.5\"",
        "\"8080\"",
        "\"a\\u0007b\"",
        long.as_str(),
    ];
    for (index, value) in refused.iter().enumerate() {
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            &format!("value-{index}"),
            &descriptor_requiring(
                LAUNCH_ENV_FEATURE,
                &launch_env_table(&[format!("PI_FLAG = {value}")]),
            ),
            false,
        )
        .await;
        assert_eq!(
            result.expect_err(value).code,
            "package_descriptor_invalid",
            "{value}"
        );
    }
    for (index, value) in ["0", "1", "true", "false"].into_iter().enumerate() {
        let document = descriptor_requiring(
            LAUNCH_ENV_FEATURE,
            &launch_env_table(&[format!("PI_FLAG = \"{value}\"")]),
        )
        .replace("version = \"1.0.0\"", &format!("version = \"1.0.{index}\""));
        let (_digest, result) =
            install_descriptor(&mut local, &servers.plugins, value, &document, false).await;
        result.unwrap_or_else(|error| panic!("{value} installs: {error:?}"));
    }

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn every_listed_name_word_is_refused_in_any_position() {
    use pohunek_daemon::agent::host::{GLUED_ENDINGS, LOCATION_WORDS, SECRET_WORDS};

    let servers = spawn_servers("pkg-launch-env-words").await;
    let mut local = connect_unix(&servers.socket).await;
    // Each listed word last, not last, and each glued ending after a letter, so
    // dropping an entry from a list fails here.
    let mut names: Vec<String> = Vec::new();
    for word in SECRET_WORDS.iter().chain(LOCATION_WORDS) {
        names.push(format!("PI_{word}"));
        names.push(format!("PI_{word}_MODE"));
    }
    names.extend(GLUED_ENDINGS.iter().map(|ending| format!("PI_X{ending}")));
    for name in &names {
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            name,
            &descriptor_requiring(
                LAUNCH_ENV_FEATURE,
                &launch_env_table(&[format!("{name} = \"1\"")]),
            ),
            false,
        )
        .await;
        assert_eq!(
            result.expect_err(name).code,
            "package_descriptor_invalid",
            "{name}"
        );
    }

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn launch_env_pairs_reach_the_version_probe_and_key_its_cache() {
    let servers = spawn_servers("pkg-probe-env").await;
    let mut local = connect_unix(&servers.socket).await;
    let features = format!("{REVISION_FEATURE}, {LAUNCH_ENV_FEATURE}");
    // The program prints its revision from PI_REVISION and nothing else
    // of its environment, so the version is the observation of the pair.
    let program = write_program(
        &servers.plugins,
        "probe-env",
        "#!/bin/sh\nprintf 'fixture 1.4.13-R%s\\n' \"$PI_REVISION\"\n",
    );
    let install = |version: &str, tables: &str| -> String {
        let mut document = probed_descriptor(
            &program,
            version,
            REVISION_PARSER,
            ("1.4.13", "2.0.0"),
            &features,
        );
        document.push_str(tables);
        document
    };

    // Without the table the variable is unset: the probe line has no revision.
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "no-table",
        &install("1.0.0", ""),
        false,
    )
    .await;
    result.expect("installs");
    let runtime = inspected_runtime(&mut local).await;
    assert_eq!((runtime.version, runtime.supported), (None, Some(false)));

    // The same program path under a declared pair reports the pair's revision;
    // a later package with another value reports its own, so the probe cache
    // keys on the pairs.
    for (version, value) in [("1.0.1", "0"), ("1.0.2", "1")] {
        let table = launch_env_table(&[format!("PI_REVISION = \"{value}\"")]);
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            &format!("pair-{value}"),
            &install(version, &table),
            false,
        )
        .await;
        result.expect("installs");
        let runtime = inspected_runtime(&mut local).await;
        assert_eq!(
            runtime.version.as_deref(),
            Some(format!("1.4.13-R{value}").as_str())
        );
        assert_eq!(runtime.supported, Some(true));
    }

    drop(local);
    servers.stop().await;
}

/// The text the session's PTY has printed so far.
async fn terminal_text<S>(framed: &mut Framed<S, LinesCodec>, id: &protocol::SessionId) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use base64::Engine as _;

    let params = protocol::SessionOutputParams::new(id.clone(), None, None, 4096, None)
        .expect("valid output params");
    let output: protocol::SessionOutputResult = call(
        framed,
        method::SESSION_OUTPUT,
        serde_json::to_value(params).expect("output params"),
    )
    .await;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(output.data_base64())
        .expect("base64 output");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Starts a session of `agent` and returns its id.
async fn start_session<S>(
    framed: &mut Framed<S, LinesCodec>,
    agent: &str,
    cwd: &Path,
) -> protocol::SessionId
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    start_named_session(framed, agent, cwd, None).await
}

/// Starts a session of `agent` named `name` and returns its id.
async fn start_named_session<S>(
    framed: &mut Framed<S, LinesCodec>,
    agent: &str,
    cwd: &Path,
    name: Option<&str>,
) -> protocol::SessionId
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    start_session_in(framed, agent, cwd, name, None).await
}

/// [`start_named_session`] by project reference `project`, which names the
/// project the session belongs to while `cwd` is where it starts.
async fn start_session_in<S>(
    framed: &mut Framed<S, LinesCodec>,
    agent: &str,
    cwd: &Path,
    name: Option<&str>,
    project: Option<&str>,
) -> protocol::SessionId
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let params = protocol::SessionNewParams {
        client_request_id: None,
        name: name.map(str::to_owned),
        agent: agent.to_owned(),
        cwd: Some(cwd.to_path_buf()),
        cols: 80,
        rows: 24,
        project: project.map(str::to_owned),
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        extended_input_ready_wait: None,
        branch_source: None,
        on_diverged: None,
        input_ack_timeout_ms: None,
        metadata: std::collections::BTreeMap::new(),
        secret_env: protocol::SessionSecretEnv::default(),
        context_package_digest: None,
    };
    let created: protocol::SessionNewResult = call(
        framed,
        method::SESSION_NEW,
        serde_json::to_value(params).expect("session.new params"),
    )
    .await;
    created.session.id
}

/// Waits until the session prints `marker`.
async fn wait_for_text<S>(
    framed: &mut Framed<S, LinesCodec>,
    id: &protocol::SessionId,
    marker: &str,
) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let shared = tokio::sync::Mutex::new(framed);
    let shared = &shared;
    pohunek_test_support::wait::wait_until(
        &format!("the session to print {marker}"),
        || async move {
            let mut framed = shared.lock().await;
            let text = terminal_text(&mut **framed, id).await;
            text.contains(marker).then_some(text)
        },
    )
    .await
}

fn store_text(servers: &Servers) -> String {
    std::fs::read_to_string(servers.store.as_ref().expect("the registry owns a store"))
        .expect("read the metadata store")
}

#[tokio::test]
async fn launch_env_pairs_reach_new_and_resumed_sessions_and_a_profile_overrides_them() {
    let servers = spawn_servers("pkg-launch-env-session").await;
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();
    // The program prints only the variable under test, never its environment.
    let program = write_program(
        &servers.plugins,
        "agent-env",
        "#!/bin/sh\nprintf 'LAUNCH_ENV_PAIR=<%s>\\n' \"$PI_MARK\"\nexec sleep 600\n",
    );
    let document = descriptor_requiring(
        LAUNCH_ENV_FEATURE,
        &launch_env_table(&["PI_MARK = \"1\"".to_owned()]),
    )
    .replace(
        "program = \"/bin/sh\"",
        &format!("program = \"{}\"", program.display()),
    );
    let (digest, installed) =
        install_descriptor(&mut local, &servers.plugins, "agent-env", &document, false).await;
    installed.expect("the package installs");
    std::fs::write(
        servers.agents.join("overridden.toml"),
        format!(
            "base = \"{RUNTIME}\"\npackage = \"acme.runtime.pi\"\ndigest = \"{}\"\n\n[env]\nPI_MARK = \"from-profile\"\n",
            digest.as_str()
        ),
    )
    .expect("write the profile");

    // A new session gets the descriptor pair.
    let id = start_session(&mut local, RUNTIME, &cwd).await;
    wait_for_text(&mut local, &id, "LAUNCH_ENV_PAIR=<1>").await;

    // A resumed session is relaunched with the pair of the definition that
    // serves the relaunch.
    let _: protocol::SessionStopResult = call(&mut local, method::SESSION_STOP, json!(id)).await;
    let resumed: protocol::SessionResumeResult =
        call(&mut local, method::SESSION_RESUME, json!(id)).await;
    wait_for_text(&mut local, &resumed.session.id, "LAUNCH_ENV_PAIR=<1>").await;
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(resumed.session.id)).await;

    // A host profile's `[env]` entry of the same name wins.
    let overridden = start_session(&mut local, "overridden", &cwd).await;
    let text = wait_for_text(&mut local, &overridden, "LAUNCH_ENV_PAIR=<from-profile>").await;
    assert!(!text.contains("LAUNCH_ENV_PAIR=<1>"), "{text:?}");
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(overridden)).await;

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_descriptor_with_or_without_fork_data_installs_and_shows_no_fork() {
    let log = DaemonLog::start();
    let servers = spawn_servers("pkg-fork-data").await;
    let mut local = connect_unix(&servers.socket).await;
    let plain = descriptor("/bin/sh");
    let launch_args = "launch_args = [\"--session-id\", \"{reference}\"]\n";
    let fork = "[fork]\nsupported = true\nargs = [\"--fork\", \"{reference}\"]\n";
    assert!(plain.contains(fork) && plain.contains(launch_args));
    // A descriptor of an earlier release: a `[fork]` table next to an
    // `assign_fork` key.
    let legacy = plain.replace(launch_args, &format!("{launch_args}assign_fork = true\n"));
    // A descriptor of this release: no fork data at all.
    let current = plain
        .replace(fork, "")
        .replace("version = \"1.0.0\"", "version = \"1.0.1\"");
    assert!(!current.contains("[fork]"));
    // The warning names the package id; each descriptor gets a package and a
    // runtime id no other test of the process uses, so the records below are
    // this scenario's alone and the two packages do not serve one runtime.
    let package_line = "id = \"acme.runtime.pi\"";
    let runtime_line = format!("id = \"{RUNTIME}\"");
    assert!(plain.contains(package_line) && plain.contains(&runtime_line));
    for (name, document, warns) in [
        ("legacy-fork", &legacy, true),
        ("current-no-fork", &current, false),
    ] {
        let package = format!("acme.runtime.{name}");
        let document = document
            .replace(package_line, &format!("id = \"{package}\""))
            .replace(&runtime_line, &format!("id = \"{name}\""));
        let (digest, installed) =
            install_descriptor(&mut local, &servers.plugins, name, &document, false).await;
        installed.unwrap_or_else(|error| panic!("{name} installs: {error:?}"));
        let warned = logged_records(&log.path(), "ignoring fork data in the runtime descriptor")
            .into_iter()
            .filter(|record| record["fields"]["runtime"] == package.as_str())
            .count();
        assert_eq!(warned > 0, warns, "{name}: {warned} warnings");
        let inspected: PackageInspectResult = call(
            &mut local,
            method::PACKAGE_INSPECT,
            json!({ "digest": digest }),
        )
        .await;
        let runtime = inspected.runtime.expect("descriptor");
        assert_eq!(
            runtime.launch_args,
            Some(vec!["--session-id".to_owned(), "{reference}".to_owned()]),
            "{name}"
        );
        assert!(runtime.resumable, "{name}");
        let wire = serde_json::to_value(&runtime).expect("serialize the runtime info");
        assert_eq!(wire["forkable"], false, "{name}: forkable is always false");
        for field in ["fork_args", "fork_launch_args"] {
            assert!(wire.get(field).is_none(), "{name}: {field} is not shown");
        }
    }

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_secret_slot_naming_a_launch_env_pair_is_refused_at_session_new() {
    const SECRET: &str = "SENTINEL_LAUNCH_ENV_SLOT_6f1e";
    let servers = spawn_servers("pkg-launch-env-slot").await;
    let mut local = connect_unix(&servers.socket).await;
    let (digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "launch-env-slot",
        &descriptor_requiring(
            LAUNCH_ENV_FEATURE,
            &launch_env_table(&["PI_NO_UPDATE = \"1\"".to_owned()]),
        ),
        false,
    )
    .await;
    installed.expect("a descriptor with a launch_env pair installs");
    let profile = servers.agents.join("flagged.toml");
    std::fs::write(
        &profile,
        format!(
            "base = \"{RUNTIME}\"\npackage = \"acme.runtime.pi\"\ndigest = \"{digest}\"\nsecret_env = [\"PI_NO_UPDATE\"]\n"
        ),
    )
    .expect("write the profile");
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o600))
        .expect("secure the profile");

    let refused = exchange(
        &mut local,
        method::SESSION_NEW,
        json!({
            "agent": "flagged", "cols": 80, "rows": 24,
            "secret_env": { "PI_NO_UPDATE": SECRET }
        }),
    )
    .await
    .into_result()
    .expect_err("a runtime launch flag is not a slot");

    assert_eq!(refused.code, "secret_env_not_declared", "{refused:?}");
    assert!(
        !format!("{refused:?}").contains(SECRET),
        "the refusal quotes the secret: {refused:?}"
    );
    servers.stop().await;
}

/// A current-schema line of `kind` that no loader accepts.
fn rejected_line(kind: &str) -> String {
    json!({
        "kind": kind,
        "schema_version": pohunek_daemon::store::STORE_SCHEMA_VERSION,
        "unexpected": 1
    })
    .to_string()
}

#[tokio::test]
async fn a_removal_refused_for_an_unreadable_line_keeps_the_package() {
    let unreadable = rejected_line("resume");
    let servers = spawn_servers_over("store-uninstall", Some(&format!("{unreadable}\n"))).await;
    let mut local = connect_unix(&servers.socket).await;
    let (bytes, digest) = archive("/bin/sh");
    let archive_path = servers.plugins.with_file_name("pi.tar.zst");
    std::fs::write(&archive_path, &bytes).expect("write the archive");
    let _: PackageInstallResult = call(
        &mut local,
        method::PACKAGE_INSTALL,
        json!({
            "archive_path": archive_path,
            "trust": { "kind": "explicit_digest", "digest": digest },
            "enable": false,
            "select": false,
            "dry_run": false
        }),
    )
    .await;
    let uninstall = json!({ "digest": digest, "remove_modified": false });

    assert_eq!(
        refusal(&mut local, method::PACKAGE_UNINSTALL, uninstall).await,
        "package_retention_uncertain",
        "a line that may pin the package keeps it installed"
    );

    drop(local);
    servers.stop().await;
}

/// Connection tasks whose shutdown token nothing cancels.
fn untracked(name: &'static str) -> pohunek_daemon::api::ConnectionTasks {
    pohunek_daemon::api::ConnectionTasks::new(name, &tokio_util::sync::CancellationToken::new())
}

const NAMING_FEATURE: &str = "\"naming-launch-args-v1\"";

/// A `[naming]` table declaring `tokens` as `launch_args`.
fn naming_table(tokens: &[&str]) -> String {
    let quoted: Vec<String> = tokens.iter().map(|token| format!("\"{token}\"")).collect();
    format!("\n[naming]\nlaunch_args = [{}]\n", quoted.join(", "))
}

/// A program that prints each argument on a line of its own, then a marker.
fn argv_program(plugins: &Path) -> PathBuf {
    write_program(
        plugins,
        "agent-argv",
        "#!/bin/sh\nfor a in \"$@\"; do printf 'ARG<%s>\\n' \"$a\"; done\nprintf 'ARGV_END\\n'\nexec sleep 600\n",
    )
}

/// The `fixture` descriptor launching the argv-printing `program`.
fn naming_descriptor(program: &Path, version: &str, naming: &str) -> String {
    let requires = if naming.is_empty() {
        ""
    } else {
        NAMING_FEATURE
    };
    descriptor_requiring(requires, naming)
        .replace(
            "program = \"/bin/sh\"",
            &format!("program = \"{}\"", program.display()),
        )
        .replace("version = \"1.0.0\"", &format!("version = \"{version}\""))
}

/// The arguments the session's agent printed.
async fn printed_argv<S>(
    framed: &mut Framed<S, LinesCodec>,
    id: &protocol::SessionId,
) -> Vec<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let text = wait_for_text(framed, id, "ARGV_END").await;
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            line.strip_prefix("ARG<")?
                .strip_suffix('>')
                .map(str::to_owned)
        })
        .collect()
}

/// `argv` with the generated native reference value dropped: the argument
/// after the first of `flags`.
fn without_reference(argv: &[String], flags: &[&str]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut skip = false;
    for arg in argv {
        if std::mem::take(&mut skip) {
            continue;
        }
        skip = flags.contains(&arg.as_str());
        kept.push(arg.clone());
    }
    kept
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[tokio::test]
async fn naming_args_follow_the_name_each_launch_uses() {
    let servers = spawn_servers_over("pkg-naming-launch", Some("")).await;
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();
    let program = argv_program(&servers.plugins);
    let document = naming_descriptor(&program, "1.0.0", &naming_table(&["--name={session_name}"]));
    let (_digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-naming",
        &document,
        false,
    )
    .await;
    installed.expect("the package installs");
    let create_flags = ["--session-id"];

    // The name sits between the frozen arguments and the native reference.
    let named = start_named_session(&mut local, RUNTIME, &cwd, Some("Alpha")).await;
    let argv = printed_argv(&mut local, &named).await;
    assert_eq!(
        without_reference(&argv, &create_flags),
        strings(&["--model", "fast", "--name=Alpha", "--session-id"])
    );

    // An unnamed session and a whitespace-only name get no group.
    for name in [None, Some("   ")] {
        let id = start_named_session(&mut local, RUNTIME, &cwd, name).await;
        let argv = printed_argv(&mut local, &id).await;
        assert_eq!(
            without_reference(&argv, &create_flags),
            strings(&["--model", "fast", "--session-id"]),
            "{name:?}"
        );
        let _: protocol::SessionStopResult =
            call(&mut local, method::SESSION_STOP, json!(id)).await;
    }

    // A dash-leading name stays one element of the joined form.
    let dashed = start_named_session(&mut local, RUNTIME, &cwd, Some("--help")).await;
    let argv = printed_argv(&mut local, &dashed).await;
    assert_eq!(
        without_reference(&argv, &create_flags),
        strings(&["--model", "fast", "--name=--help", "--session-id"])
    );
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(dashed)).await;

    // A resume carries the name the record holds now.
    let _: protocol::SessionStopResult = call(&mut local, method::SESSION_STOP, json!(named)).await;
    let _: protocol::SessionRenameResult = call(
        &mut local,
        method::SESSION_RENAME,
        json!({ "session_id": named, "name": "Beta" }),
    )
    .await;
    let resumed: protocol::SessionResumeResult =
        call(&mut local, method::SESSION_RESUME, json!(named)).await;
    let argv = printed_argv(&mut local, &resumed.session.id).await;
    assert_eq!(
        without_reference(&argv, &["--session"]),
        strings(&["--model", "fast", "--name=Beta", "--session"])
    );

    // The persisted resume snapshot never holds a naming token.
    let store = store_text(&servers);
    assert!(store.contains("Beta"), "the record holds the current name");
    assert!(
        !store.contains("--name"),
        "the store holds no naming argument"
    );

    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(resumed.session.id)).await;
    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_profile_backed_launch_gets_the_naming_args() {
    let servers = spawn_servers("pkg-naming-profile").await;
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();
    let program = argv_program(&servers.plugins);
    let document = naming_descriptor(&program, "1.0.0", &naming_table(&["--name={session_name}"]));
    let (digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-naming",
        &document,
        false,
    )
    .await;
    installed.expect("the package installs");
    std::fs::write(
        servers.agents.join("profiled.toml"),
        format!(
            "base = \"{RUNTIME}\"\npackage = \"acme.runtime.pi\"\ndigest = \"{}\"\nprogram = \"{}\"\nargs = [\"--from-profile\"]\n",
            digest.as_str(),
            program.display()
        ),
    )
    .expect("write the profile");

    let id = start_named_session(&mut local, "profiled", &cwd, Some("Prof")).await;
    let argv = printed_argv(&mut local, &id).await;
    assert_eq!(
        without_reference(&argv, &["--session-id"]),
        strings(&["--from-profile", "--name=Prof", "--session-id"])
    );
    let _: protocol::SessionStopResult = call(&mut local, method::SESSION_STOP, json!(id)).await;
    drop(local);
    servers.stop().await;
}

/// Creates sessions named with plain and dash-leading names and checks the pair
/// group each launch carries and the warning a withheld group logs.
async fn assert_pair_creates(
    local: &mut Framed<UnixStream, LinesCodec>,
    cwd: &Path,
    log_dir: &Path,
) {
    let create_flags = ["--session-id"];

    for (name, expected) in [
        (
            "Two Words",
            strings(&["--model", "fast", "--name", "Two Words", "--session-id"]),
        ),
        ("--offline", strings(&["--model", "fast", "--session-id"])),
        ("-n", strings(&["--model", "fast", "--session-id"])),
    ] {
        let id = start_named_session(local, RUNTIME, cwd, Some(name)).await;
        let argv = printed_argv(local, &id).await;
        assert_eq!(without_reference(&argv, &create_flags), expected, "{name}");
        // The withheld group is logged with the session id, never the name.
        let warned: Vec<Value> = logged_records(log_dir, "starts with a dash")
            .into_iter()
            .filter(|record| record["fields"]["session_id"] == id.0.as_str())
            .collect();
        assert_eq!(
            warned.len(),
            usize::from(name.starts_with('-')),
            "{name}: {warned:?}"
        );
        for record in &warned {
            assert_eq!(record["level"], "WARN", "{record}");
            assert!(!record.to_string().contains(name), "{record}");
        }
        let _: protocol::SessionStopResult = call(local, method::SESSION_STOP, json!(id)).await;
    }
}

/// The pair form `["--name", "{session_name}"]` hands the name over as a
/// separate element, after the contributor arguments and before the native
/// reference; a name with interior spaces stays one element, a dash-leading
/// name is withheld with its flag at create and resume, and a resume carries
/// the name the record holds now.
#[tokio::test]
async fn pair_naming_args_follow_the_name_each_launch_uses() {
    let servers = spawn_servers_over("pkg-naming-pair", Some("")).await;
    let log = DaemonLog::start();
    let log_dir = log.path();
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();
    let program = argv_program(&servers.plugins);
    let document = naming_descriptor(
        &program,
        "1.0.0",
        &naming_table(&["--name", "{session_name}"]),
    );
    let (_digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-naming-pair",
        &document,
        false,
    )
    .await;
    installed.expect("the pair form installs");
    assert_pair_creates(&mut local, &cwd, &log_dir).await;

    // A rename to a plain name reaches the next resume; a rename to a
    // dash-leading name withholds the group from it.
    let named = start_named_session(&mut local, RUNTIME, &cwd, Some("Alpha")).await;
    let _: protocol::SessionStopResult = call(&mut local, method::SESSION_STOP, json!(named)).await;
    for (rename, expected) in [
        (
            "Beta",
            strings(&["--model", "fast", "--name", "Beta", "--session"]),
        ),
        ("--Gamma", strings(&["--model", "fast", "--session"])),
    ] {
        let _: protocol::SessionRenameResult = call(
            &mut local,
            method::SESSION_RENAME,
            json!({ "session_id": named, "name": rename }),
        )
        .await;
        let resumed: protocol::SessionResumeResult =
            call(&mut local, method::SESSION_RESUME, json!(named)).await;
        let argv = printed_argv(&mut local, &resumed.session.id).await;
        assert_eq!(
            without_reference(&argv, &["--session"]),
            expected,
            "{rename}"
        );
        let _: protocol::SessionStopResult =
            call(&mut local, method::SESSION_STOP, json!(named)).await;
    }

    drop(local);
    servers.stop().await;
}

/// Every record of the production JSON log files in `log_dir`.
fn log_records(log_dir: &Path) -> Vec<Value> {
    let mut records = Vec::new();
    for entry in std::fs::read_dir(log_dir).expect("the capture directory") {
        let text = std::fs::read_to_string(entry.expect("a log file").path()).unwrap_or_default();
        records.extend(
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok()),
        );
    }
    records
}

/// The production JSON log records in `log_dir` whose message mentions
/// `needle`.
fn logged_records(log_dir: &Path, needle: &str) -> Vec<Value> {
    log_records(log_dir)
        .into_iter()
        .filter(|record| {
            record["fields"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(needle))
        })
        .collect()
}

/// `[naming]` tables the grammar refuses, next to the valid `valid` table.
fn refused_naming_tables(valid: &str) -> Vec<(&'static str, String)> {
    let nine = [
        "--a",
        "--b",
        "--c",
        "--d",
        "--e",
        "--f",
        "--g",
        "--h",
        "--n={session_name}",
    ];
    vec![
        ("bare", naming_table(&["{session_name}"])),
        ("after-double-dash", naming_table(&["--", "{session_name}"])),
        ("after-single-dash", naming_table(&["-", "{session_name}"])),
        (
            "after-joined-flag",
            naming_table(&["--zq9=y", "{session_name}"]),
        ),
        (
            "after-literal",
            naming_table(&["zq9name", "{session_name}"]),
        ),
        (
            "after-triple-dash",
            naming_table(&["---zq9", "{session_name}"]),
        ),
        (
            "two-pairs",
            naming_table(&["--name", "{session_name}", "--zq9also", "{session_name}"]),
        ),
        (
            "two",
            naming_table(&["--name={session_name}", "--zq9also={session_name}"]),
        ),
        ("none", naming_table(&["--name"])),
        (
            "text-after",
            naming_table(&["--marker-flag={session_name}tail"]),
        ),
        ("no-dash", naming_table(&["name={session_name}"])),
        ("no-equals", naming_table(&["-n{session_name}"])),
        (
            "reference",
            naming_table(&["--name={session_name}", "{reference}"]),
        ),
        (
            "other-brace",
            naming_table(&["--name={session_name}", "{x}"]),
        ),
        ("empty-list", naming_table(&[])),
        ("empty-token", naming_table(&["--name={session_name}", ""])),
        ("nine", naming_table(&nine)),
        (
            "long-prefix",
            naming_table(&[&format!("--{}={{session_name}}", "p".repeat(1000))]),
        ),
        ("unknown-key", format!("{valid}other = 1\n")),
    ]
}

#[tokio::test]
async fn naming_args_install_only_with_their_feature_and_in_a_valid_shape() {
    let servers = spawn_servers("pkg-naming-install").await;
    let mut local = connect_unix(&servers.socket).await;
    let valid = naming_table(&["--name={session_name}"]);

    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "without-requires",
        &descriptor_requiring("", &valid),
        false,
    )
    .await;
    assert_eq!(
        result.expect_err("a gated table without requires").code,
        "runtime_feature_unsupported"
    );

    let cases = refused_naming_tables(&valid);
    for (name, tables) in &cases {
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            name,
            &descriptor_requiring(NAMING_FEATURE, tables),
            false,
        )
        .await;
        let error = result.expect_err(name);
        assert_eq!(error.code, "package_descriptor_invalid", "{name}");
        // A refusal never reflects a token of the table it refuses; tokens of
        // one or two characters would match the reason's own words.
        for token in tables
            .lines()
            .filter_map(|line| line.strip_prefix("launch_args = ["))
            .flat_map(|list| list.trim_end_matches(']').split(", "))
            .map(|token| token.trim_matches('"'))
            .filter(|token| token.len() > 2 && *token != "{session_name}")
        {
            assert!(!error.msg.contains(token), "{name}: {token}: {}", error.msg);
        }
    }

    // The placeholder is not a launch argument of the runtime itself.
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "in-args",
        &descriptor("/bin/sh").replace(
            "args = [\"--model\", \"fast\"]",
            "args = [\"--model\", \"--name={session_name}\"]",
        ),
        false,
    )
    .await;
    assert_eq!(
        result.expect_err("a placeholder in runtime.args").code,
        "package_descriptor_invalid"
    );

    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor records nothing"
    );
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "valid",
        &descriptor_requiring(NAMING_FEATURE, &valid),
        false,
    )
    .await;
    result.expect("the joined form installs with its feature");

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_session_pinned_before_naming_args_resumes_without_the_name() {
    let servers = spawn_servers("pkg-naming-pinned").await;
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();
    let program = argv_program(&servers.plugins);
    let (_digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-plain",
        &naming_descriptor(&program, "1.0.0", ""),
        false,
    )
    .await;
    installed.expect("the plain package installs");
    let id = start_named_session(&mut local, RUNTIME, &cwd, Some("Delta")).await;
    let _ = printed_argv(&mut local, &id).await;
    let _: protocol::SessionStopResult = call(&mut local, method::SESSION_STOP, json!(id)).await;

    let (_digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-naming",
        &naming_descriptor(&program, "1.0.1", &naming_table(&["--name={session_name}"])),
        false,
    )
    .await;
    installed.expect("the naming package installs");

    let resumed: protocol::SessionResumeResult =
        call(&mut local, method::SESSION_RESUME, json!(id)).await;
    let argv = printed_argv(&mut local, &resumed.session.id).await;
    assert_eq!(
        without_reference(&argv, &["--session"]),
        strings(&["--model", "fast", "--session"])
    );
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(resumed.session.id)).await;
    drop(local);
    servers.stop().await;
}

/// The trust of a host whose anchor holds a throwaway root, with the plugin
/// index channel naming that root when `channel` is set.
fn index_trust(channel: bool) -> CatalogTrust {
    let root = root_of(&test_key(), WINDOW_START, WINDOW_END);
    let anchor = HostTrustAnchor::new(vec![root.clone()], Vec::new()).expect("anchor");
    CatalogTrust::Loaded(if channel {
        anchor.with_index(IndexTrust::Loaded(Box::new(root)))
    } else {
        anchor
    })
}

/// Whether `host.inspect` reports plugin index installs, over a request that
/// probes runtimes (`full`) or only asks for the flags.
async fn index_flag<S>(framed: &mut Framed<S, LinesCodec>, params: Value) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let capabilities: HostCapabilities = call(framed, method::HOST_INSPECT, params).await;
    capabilities.package_install_index_supported
}

#[tokio::test]
async fn the_index_trust_kind_is_served_only_with_a_loaded_channel() {
    let (bytes, _digest) = archive("/bin/sh");
    for (tag, trust, flag, code) in [
        (
            "index-wire-channel",
            index_trust(true),
            true,
            "package_source_unreadable",
        ),
        (
            "index-wire-no-channel",
            index_trust(false),
            false,
            "official_trust_unavailable",
        ),
        (
            "index-wire-no-anchor",
            CatalogTrust::Absent,
            false,
            "official_trust_unavailable",
        ),
    ] {
        let servers = spawn_servers_trusting(tag, None, trust).await;
        let mut local = connect_unix(&servers.socket).await;
        assert_eq!(index_flag(&mut local, Value::Null).await, flag, "{tag}");
        assert_eq!(
            index_flag(&mut local, json!({ "skip_runtime_probes": true })).await,
            flag,
            "{tag}"
        );

        // A missing index file is refused as unreadable only when the host
        // serves the kind; the dispatch itself is what the code proves.
        let archive_path = servers.plugins.with_file_name("pi.tar.zst");
        std::fs::write(&archive_path, &bytes).expect("write the archive");
        let missing = servers.plugins.with_file_name("absent-index.json");
        let refused = refusal(
            &mut local,
            method::PACKAGE_INSTALL,
            json!({
                "archive_path": archive_path,
                "trust": { "kind": "index", "index_path": missing },
                "enable": false,
                "select": false,
                "dry_run": false
            }),
        )
        .await;
        assert_eq!(refused, code, "{tag}");
        servers.stop().await;
    }
}

const PLACEMENT_FEATURE: &str = "\"placement-v1\"";

/// A `[placement]` table with `body` lines.
fn placement_table(body: &str) -> String {
    format!("\n[placement]\n{body}\n")
}

/// The `fixture` descriptor launching `program`, requiring `placement-v1` when
/// `requires` and followed by `table`.
fn placement_descriptor(program: &Path, requires: bool, table: &str) -> String {
    descriptor_requiring(if requires { PLACEMENT_FEATURE } else { "" }, table).replace(
        "program = \"/bin/sh\"",
        &format!("program = \"{}\"", program.display()),
    )
}

#[tokio::test]
async fn placement_declarations_install_only_in_their_valid_forms() {
    let servers = spawn_servers("pkg-placement-forms").await;
    let mut local = connect_unix(&servers.socket).await;
    let valid = [
        ("nothing", placement_table("mode = \"in_process\"")),
        (
            "args",
            placement_table("mode = \"in_process\"\nkeep_local_args = [\"--no-daemon\"]"),
        ),
    ];
    for (name, table) in &valid {
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            name,
            &placement_descriptor(Path::new("/bin/sh"), true, table),
            true,
        )
        .await;
        result.unwrap_or_else(|error| panic!("{name} is accepted: {error:?}"));
    }

    // A descriptor that does not list the feature stays valid without a table.
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "additive",
        &placement_descriptor(Path::new("/bin/sh"), false, ""),
        true,
    )
    .await;
    result.expect("a descriptor without the feature is untouched");

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn placement_declarations_the_core_cannot_honour_are_refused() {
    let servers = spawn_servers("pkg-placement-refused").await;
    let mut local = connect_unix(&servers.socket).await;
    let refused = [
        ("listed-without-table", true, String::new()),
        (
            "owned-server",
            true,
            placement_table("mode = \"owned_server\""),
        ),
        (
            "both-forms",
            true,
            placement_table(
                "mode = \"in_process\"\nkeep_local_args = [\"--a\"]\nkeep_local_handler = true",
            ),
        ),
        (
            "handler-without-contributor",
            true,
            placement_table("mode = \"in_process\"\nkeep_local_handler = true"),
        ),
        (
            "empty-args",
            true,
            placement_table("mode = \"in_process\"\nkeep_local_args = []"),
        ),
        (
            "empty-token",
            true,
            placement_table("mode = \"in_process\"\nkeep_local_args = [\"\"]"),
        ),
        (
            "control-token",
            true,
            placement_table("mode = \"in_process\"\nkeep_local_args = [\"--a\\u0007\"]"),
        ),
        (
            "name-placeholder",
            true,
            placement_table("mode = \"in_process\"\nkeep_local_args = [\"{session_name}\"]"),
        ),
        (
            "no-mode",
            true,
            placement_table("keep_local_args = [\"--a\"]"),
        ),
        (
            "unknown-key",
            true,
            placement_table("mode = \"in_process\"\nsetsid = true"),
        ),
    ];
    for (name, requires, table) in &refused {
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            name,
            &placement_descriptor(Path::new("/bin/sh"), *requires, table),
            true,
        )
        .await;
        assert_eq!(
            result.expect_err(name).code,
            "package_descriptor_invalid",
            "{name}"
        );
    }

    // A table without its feature in `requires` is a feature-owned key.
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "unlisted",
        &placement_descriptor(
            Path::new("/bin/sh"),
            false,
            &placement_table("mode = \"in_process\""),
        ),
        true,
    )
    .await;
    assert_eq!(
        result.expect_err("an unlisted feature").code,
        "runtime_feature_unsupported"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn keep_local_args_follow_each_launch_and_are_not_frozen() {
    let servers = spawn_servers_over("pkg-placement-args", Some("")).await;
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();
    let program = argv_program(&servers.plugins);
    let table = placement_table("mode = \"in_process\"\nkeep_local_args = [\"--keep-local\"]");
    let document = placement_descriptor(&program, true, &table);
    let (_digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-placement",
        &document,
        false,
    )
    .await;
    installed.expect("the package installs");

    let created = start_named_session(&mut local, RUNTIME, &cwd, None).await;
    let argv = printed_argv(&mut local, &created).await;
    assert_eq!(
        without_reference(&argv, &["--session-id"]),
        strings(&["--model", "fast", "--keep-local", "--session-id"])
    );

    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(created)).await;
    let resumed: protocol::SessionResumeResult =
        call(&mut local, method::SESSION_RESUME, json!(created)).await;
    let argv = printed_argv(&mut local, &resumed.session.id).await;
    assert_eq!(
        without_reference(&argv, &["--session"]),
        strings(&["--model", "fast", "--keep-local", "--session"])
    );

    assert!(
        !store_text(&servers).contains("--keep-local"),
        "the persisted resume snapshot holds no keep-local argument"
    );
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(resumed.session.id)).await;
    drop(local);
    servers.stop().await;
}

/// Filter word that marks a launch of this binary as the listening image.
#[cfg(target_os = "linux")]
const LISTENER_IMAGE: &str = "placement_listener_image";

/// The daemon-shaped fixture image: listens on a unix socket and lives while
/// its barrier directory exists. Started with the socket path and the barrier
/// path as its last two arguments, only by the escape scenario.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "fixture process image of the placement escape scenario"]
fn placement_listener_image() {
    let args: Vec<String> = std::env::args().collect();
    let [.., socket, barrier] = args.as_slice() else {
        return;
    };
    if !args.iter().any(|argument| argument == LISTENER_IMAGE) {
        return;
    }
    let _listener =
        std::os::unix::net::UnixListener::bind(socket).expect("listen on the fixture socket");
    let started = std::time::Instant::now();
    while Path::new(barrier).is_dir() && started.elapsed() < std::time::Duration::from_secs(300) {
        // timing-allowed: #1947 the image's own liveness poll; no scenario waits on this sleep, the barrier ends it
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Removes the listener images' barrier and kills the escaped processes when
/// dropped, so a failing scenario leaves nothing behind.
#[cfg(target_os = "linux")]
struct EscapeCleanup {
    barrier: PathBuf,
    /// Pids the fixture published, killed on drop; recorded as they appear
    /// because the scenario directory is gone by the time the guard drops.
    pids: std::cell::RefCell<Vec<i32>>,
}

#[cfg(target_os = "linux")]
impl Drop for EscapeCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.barrier);
        for pid in self.pids.borrow().iter() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

/// The pid a published fixture file holds, recorded for cleanup.
#[cfg(target_os = "linux")]
fn published_pid(directory: &Path, name: &str, cleanup: &EscapeCleanup) -> u32 {
    let pid =
        pohunek_test_support::wait::poll_until(&format!("the fixture to publish {name}"), || {
            std::fs::read_to_string(directory.join(name))
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
        });
    cleanup
        .pids
        .borrow_mut()
        .push(i32::try_from(pid).expect("a process id"));
    pid
}

/// The session's latest placement check, as `session.inspect` reports it.
#[cfg(target_os = "linux")]
async fn placement_of<S>(
    framed: &mut Framed<S, LinesCodec>,
    id: &protocol::SessionId,
) -> Option<protocol::SessionPlacement>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let info: protocol::SessionInfo = call(framed, method::SESSION_INSPECT, json!(id)).await;
    info.placement
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_daemon_the_agent_prewarms_is_reported_and_one_that_stays_in_the_tree_is_not() {
    let servers = spawn_servers_configured("pkg-placement-escape", Some(""), |config| {
        config.placement_check_interval = std::time::Duration::from_millis(50);
    })
    .await;
    let mut local = connect_unix(&servers.socket).await;
    let directory = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();
    let barrier = directory.join("barrier");
    std::fs::create_dir(&barrier).expect("create the barrier");
    let cleanup = EscapeCleanup {
        barrier: barrier.clone(),
        pids: std::cell::RefCell::default(),
    };
    let image = std::env::current_exe().expect("this test binary");
    let image = image.display();
    let dir = directory.display();
    // The provider-named daemon is the holding image installed under the
    // provider's name: one process that ends by itself.
    let daemon_path = directory.join("pi-daemon");
    pohunek_test_support::hold::install_at(&daemon_path).expect("install the holding image");
    let daemon_words = pohunek_test_support::hold::shell_words_for(&daemon_path);
    // In order: a listening image that stays in the tree, a `setsid` wrapper
    // that neither listens nor is named for the provider, a double-forked
    // listening image in a session of its own, and a daemon image named for the
    // provider (the runtime is `pi`) in a session of its own. Each step waits
    // for the previous process to publish its pid, so the escape is only
    // reported by a check that already judged the processes that stay.
    let filter = pohunek_test_support::exact_test_filter(module_path!(), LISTENER_IMAGE);
    let script = format!(
        "#!/bin/sh\n\
         cd {dir}\n\
         wait_for() {{ until [ -e \"$1\" ]; do /bin/sleep 0.02; done; }}\n\
         sh -c 'echo $$ > tree.pid; exec {image} {filter} --ignored --nocapture {dir}/tree.sock {barrier}' </dev/null >/dev/null 2>&1 &\n\
         wait_for tree.pid\n\
         /usr/bin/setsid sh -c 'echo $$ > wrapper.pid; exec /bin/sleep 600' </dev/null >/dev/null 2>&1 &\n\
         wait_for wrapper.pid\n\
         ( /usr/bin/setsid sh -c 'echo $$ > listener.pid; exec {image} {filter} --ignored --nocapture {dir}/escaped.sock {barrier}' </dev/null >/dev/null 2>&1 & )\n\
         wait_for listener.pid\n\
         /usr/bin/setsid sh -c \"echo \\$\\$ > daemon.pid; exec {daemon_words}\" &\n\
         wait_for daemon.pid\n\
         exec /bin/sleep 600\n",
        barrier = barrier.display(),
        filter = filter.join(" "),
    );
    let program = write_program(&servers.plugins, "agent-escape", &script);
    let table = placement_table("mode = \"in_process\"");
    let (_digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-escape",
        &placement_descriptor(&program, true, &table),
        false,
    )
    .await;
    installed.expect("the package installs");

    let id = start_named_session(&mut local, RUNTIME, &directory, None).await;
    let tree = published_pid(&directory, "tree.pid", &cleanup);
    let wrapper = published_pid(&directory, "wrapper.pid", &cleanup);
    let listener = published_pid(&directory, "listener.pid", &cleanup);
    let daemon = published_pid(&directory, "daemon.pid", &cleanup);

    let shared = tokio::sync::Mutex::new(&mut local);
    let shared = &shared;
    let session = &id;
    let found = pohunek_test_support::wait::wait_until(
        "the worker to report both escaped processes",
        || async move {
            let mut framed = shared.lock().await;
            let placement = placement_of(&mut **framed, session).await?;
            (placement.outcome == "escaped" && placement.processes.len() == 2).then_some(placement)
        },
    )
    .await;
    let mut reported: Vec<u32> = found.processes.iter().map(|process| process.pid).collect();
    reported.sort_unstable();
    let mut expected = vec![listener, daemon];
    expected.sort_unstable();
    assert_eq!(
        reported, expected,
        "{found:?} (tree {tree}, wrapper {wrapper})"
    );

    let report: protocol::DaemonDoctorResult =
        call(&mut local, method::DAEMON_DOCTOR, Value::Null).await;
    let doctor = report
        .report
        .checks
        .iter()
        .find(|check| check.name == "placement_escape")
        .expect("doctor carries the placement check");
    assert_eq!(doctor.status, protocol::DoctorStatus::Warn);
    assert!(
        doctor.detail.contains(&daemon.to_string())
            && doctor.detail.contains(&listener.to_string()),
        "{doctor:?}"
    );

    let _: protocol::SessionStopResult = call(&mut local, method::SESSION_STOP, json!(id)).await;
    drop(local);
    servers.stop().await;
}

/// A session that stays in its tree makes the doctor's placement check `ok`
/// once the worker has judged it, and counts the session.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_session_that_stays_in_its_tree_leaves_the_doctor_placement_check_ok() {
    let servers = spawn_servers_configured("pkg-placement-clean", Some(""), |config| {
        config.placement_check_interval = std::time::Duration::from_millis(50);
    })
    .await;
    let mut local = connect_unix(&servers.socket).await;
    let cwd = servers
        .plugins
        .parent()
        .expect("the socket directory")
        .to_path_buf();
    let program = argv_program(&servers.plugins);
    let table = placement_table("mode = \"in_process\"");
    let (_digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-placement-clean",
        &placement_descriptor(&program, true, &table),
        false,
    )
    .await;
    installed.expect("the package installs");

    let id = start_named_session(&mut local, RUNTIME, &cwd, None).await;
    let shared = tokio::sync::Mutex::new(&mut local);
    let shared = &shared;
    let session = &id;
    let judged =
        pohunek_test_support::wait::wait_until("the worker to judge the session", || async move {
            let mut framed = shared.lock().await;
            placement_of(&mut **framed, session).await
        })
        .await;
    assert_eq!(judged.outcome, "clean", "{judged:?}");
    assert!(judged.processes.is_empty(), "{judged:?}");

    let report: protocol::DaemonDoctorResult =
        call(&mut local, method::DAEMON_DOCTOR, Value::Null).await;
    let doctor = report
        .report
        .checks
        .iter()
        .find(|check| check.name == "placement_escape")
        .expect("doctor carries the placement check");
    assert_eq!(doctor.status, protocol::DoctorStatus::Ok, "{doctor:?}");
    assert!(doctor.detail.starts_with("1 live session(s)"), "{doctor:?}");

    let _: protocol::SessionStopResult = call(&mut local, method::SESSION_STOP, json!(id)).await;
    drop(local);
    servers.stop().await;
}

// ---------------------------------------------------------------------------
// `[runtime] trusted_checkout_args`: the arguments a trusted checkout adds.
// ---------------------------------------------------------------------------

const CHECKOUT_TRUST_FEATURE: &str = "\"checkout-trust-args-v1\"";

/// Largest declared file the predicate compares, in bytes.
const MAX_COMPARED_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Size a declared file is extended to after its size check, in bytes (sparse).
const GROWN_FILE_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// The argument the fixture runtime adds to a trusted checkout.
const TRUST_ARG: &str = "--trust-workspace";

/// A `fixture` descriptor launching the argv-printing `program` that declares
/// `keys` (lines of the `[runtime]` table) next to its fixed arguments.
fn checkout_trust_descriptor(program: &Path, requires: &str, keys: &str) -> String {
    descriptor_requiring(requires, "")
        .replace(
            "program = \"/bin/sh\"",
            &format!("program = \"{}\"", program.display()),
        )
        .replace(
            "args = [\"--model\", \"fast\"]\n",
            &format!("args = [\"--model\", \"fast\"]\n{keys}"),
        )
}

/// The `[runtime]` keys of a runtime that adds [`TRUST_ARG`] to a trusted
/// checkout and compares `.muse` and `.agents` with the remote HEAD.
fn trusting_keys() -> String {
    format!(
        "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [\".muse\", \".agents\"]\n"
    )
}

/// Runs git in `dir`, failing the test on a non-zero exit.
fn git_in(dir: &Path, args: &[&str]) {
    let output = pohunek_test_support::git::command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs git in `dir` with `input` on stdin and returns its trimmed stdout,
/// failing the test on a non-zero exit.
fn git_stdin(dir: &Path, args: &[&str], input: &str) -> String {
    use std::io::Write as _;
    use std::process::Stdio;

    let mut child = pohunek_test_support::git::command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .take()
        .expect("git stdin")
        .write_all(input.as_bytes())
        .expect("write git stdin");
    let output = child.wait_with_output().expect("wait for git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 git output")
        .trim_end()
        .to_owned()
}

/// A checkout at `parent/name` whose `origin` is `origin` and whose default
/// branch `origin/HEAD` holds the one commit it has, with `.muse` and
/// `.agents` tracked. No network is involved: the remote-tracking refs are
/// written directly.
fn checkout(parent: &Path, name: &str, origin: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(dir.join(".muse")).expect("create .muse");
    std::fs::create_dir_all(dir.join(".agents")).expect("create .agents");
    std::fs::write(dir.join(".muse/hooks.json"), "{}\n").expect("write hooks");
    std::fs::write(dir.join(".agents/rules.md"), "rules\n").expect("write rules");
    std::fs::write(dir.join("README.md"), "readme\n").expect("write readme");
    git_in(&dir, &["init", "-q", "-b", "main"]);
    git_in(&dir, &["add", "."]);
    git_in(&dir, &["commit", "-q", "-m", "base"]);
    git_in(&dir, &["remote", "add", "origin", origin]);
    git_in(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git_in(
        &dir,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    dir
}

/// Installs the argv-printing runtime that declares the trusted-checkout keys.
async fn install_trusting_runtime<S>(servers: &Servers, local: &mut Framed<S, LinesCodec>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let program = argv_program(&servers.plugins);
    let document = checkout_trust_descriptor(&program, CHECKOUT_TRUST_FEATURE, &trusting_keys());
    let (_digest, installed) =
        install_descriptor(local, &servers.plugins, "agent-trusting", &document, false).await;
    installed.expect("the package installs");
}

/// A bare repository next to `dir` holding the commit `refs/remotes/origin/main`
/// of the checkout `dir` as its `HEAD`: what GitHub reports in these scenarios.
fn remote_snapshot(dir: &Path) -> String {
    let remote = dir.with_file_name(format!(
        "{}-remote.git",
        dir.file_name().expect("checkout name").to_string_lossy()
    ));
    git_in(
        dir.parent().expect("checkout parent"),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            &remote.to_string_lossy(),
        ],
    );
    git_in(
        &remote,
        &[
            "fetch",
            "-q",
            &dir.to_string_lossy(),
            "refs/remotes/origin/main:refs/heads/main",
        ],
    );
    remote.to_string_lossy().into_owned()
}

/// Makes the repository the predicate fetches the remote `HEAD` from `source`.
fn announce_remote_source(servers: &Servers, source: Option<String>) {
    *servers.remote_source.lock().expect("remote source") = source;
}

/// The arguments a new session in `cwd` is launched with when the remote
/// reports the checkout's own `origin/main`, after which the session is
/// stopped.
async fn created_argv<S>(
    local: &mut Framed<S, LinesCodec>,
    servers: &Servers,
    cwd: &Path,
) -> Vec<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    announce_remote_source(servers, Some(remote_snapshot(cwd)));
    launched_argv(local, cwd).await
}

/// The arguments a new session in `cwd` is launched with under the current
/// remote `HEAD`, after which the session is stopped.
async fn launched_argv<S>(local: &mut Framed<S, LinesCodec>, cwd: &Path) -> Vec<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let id = start_named_session(local, RUNTIME, cwd, None).await;
    let argv = printed_argv(local, &id).await;
    let _: protocol::SessionStopResult = call(local, method::SESSION_STOP, json!(id)).await;
    argv
}

fn carries_trust_arg(argv: &[String]) -> bool {
    argv.iter().any(|arg| arg == TRUST_ARG)
}

/// The host's trusted repositories in the form the config file records.
fn host_trusting(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one table of checkouts walks every case of the predicate against a single daemon"
)]
async fn only_a_clean_checkout_of_a_trusted_repository_gets_the_trusted_args() {
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-create",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let root = servers.plugins.parent().expect("the socket directory");
    let own = "https://github.com/acme/widgets.git";

    // DoD 1: a clean checkout of the trusted repository; the arguments follow
    // the frozen arguments and precede the native reference.
    let clean = checkout(root, "clean", own);
    let argv = created_argv(&mut local, &servers, &clean).await;
    assert_eq!(
        without_reference(&argv, &["--session-id"]),
        strings(&["--model", "fast", TRUST_ARG, "--session-id"])
    );

    // Both URL forms and any letter case name the same repository.
    for (name, origin) in [
        ("scp", "git@github.com:acme/widgets.git"),
        ("mixed-case", "https://github.com/Acme/Widgets"),
    ] {
        let dir = checkout(root, name, origin);
        assert!(
            carries_trust_arg(&created_argv(&mut local, &servers, &dir).await),
            "{name}"
        );
    }

    // DoD 2: a fork, a repository the host does not list, and every origin
    // outside the two exact GitHub forms.
    for (name, origin) in [
        ("fork", "https://github.com/mallory/widgets.git"),
        ("other-repo", "https://github.com/acme/gadgets.git"),
        ("gitlab", "https://gitlab.com/acme/widgets.git"),
        ("http", "http://github.com/acme/widgets.git"),
        (
            "lookalike-host",
            "https://github.com.evil.example/acme/widgets.git",
        ),
        ("userinfo", "https://token@github.com/acme/widgets.git"),
        ("extra-segment", "https://github.com/acme/widgets/extra.git"),
        ("trailing", "https://github.com/acme/widgets.git/"),
        ("ssh-url", "ssh://git@github.com/acme/widgets.git"),
    ] {
        let dir = checkout(root, name, origin);
        assert!(
            !carries_trust_arg(&created_argv(&mut local, &servers, &dir).await),
            "{name}"
        );
    }

    // DoD 3: a commit that changes, or adds, what the trust loads.
    let changed = checkout(root, "pr-changes-muse", own);
    std::fs::write(changed.join(".muse/hooks.json"), "{\"hooks\": 1}\n").expect("edit hooks");
    git_in(&changed, &["commit", "-q", "-a", "-m", "pr"]);
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &changed).await
    ));
    let added = checkout(root, "pr-adds-agents-file", own);
    std::fs::write(added.join(".agents/extra.md"), "extra\n").expect("add rule");
    git_in(&added, &["add", "."]);
    git_in(&added, &["commit", "-q", "-m", "pr"]);
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &added).await
    ));
    // A change outside the declared paths does not matter.
    let elsewhere = checkout(root, "pr-changes-readme", own);
    std::fs::write(elsewhere.join("README.md"), "changed\n").expect("edit readme");
    git_in(&elsewhere, &["commit", "-q", "-a", "-m", "pr"]);
    assert!(carries_trust_arg(
        &created_argv(&mut local, &servers, &elsewhere).await
    ));

    // DoD 4: local changes under a declared path, in every form git knows.
    let modified = checkout(root, "dirty-modified", own);
    std::fs::write(modified.join(".muse/hooks.json"), "{\"local\": 1}\n").expect("edit hooks");
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &modified).await
    ));
    let staged = checkout(root, "dirty-staged", own);
    std::fs::write(staged.join(".agents/rules.md"), "staged\n").expect("edit rules");
    git_in(&staged, &["add", ".agents/rules.md"]);
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &staged).await
    ));
    let untracked = checkout(root, "dirty-untracked", own);
    std::fs::write(untracked.join(".muse/new.json"), "{}\n").expect("add file");
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &untracked).await
    ));
    let ignored = checkout(root, "dirty-ignored", own);
    std::fs::write(ignored.join(".git/info/exclude"), ".muse/local.json\n").expect("exclude");
    std::fs::write(ignored.join(".muse/local.json"), "{}\n").expect("add ignored file");
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &ignored).await
    ));
    let hidden = checkout(root, "dirty-hidden", own);
    git_in(
        &hidden,
        &["update-index", "--assume-unchanged", ".muse/hooks.json"],
    );
    std::fs::write(hidden.join(".muse/hooks.json"), "{\"hidden\": 1}\n").expect("edit hooks");
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &hidden).await
    ));

    // A content filter in the repository configuration never runs: the
    // comparison hashes file contents without filters, and the checkout is
    // still trusted.
    let filtered = checkout(root, "local-filter", own);
    let marker = root.join("filter-ran");
    git_in(
        &filtered,
        &[
            "config",
            "filter.x.clean",
            &format!("touch {}", marker.display()),
        ],
    );
    std::fs::write(filtered.join(".git/info/attributes"), ".muse/** filter=x\n")
        .expect("write attributes");
    std::fs::write(filtered.join(".muse/hooks.json"), "{}\n").expect("touch hooks");
    assert!(carries_trust_arg(
        &created_argv(&mut local, &servers, &filtered).await
    ));
    assert!(!marker.exists(), "no filter ran");
    // Repository configuration that would run a program through a lazy fetch
    // is never read: no git command runs against the checkout.
    let promisor = checkout(root, "promisor", own);
    let probe = root.join("promisor-ran");
    let script = root.join("promisor.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch {}\nexit 1\n", probe.display()),
    )
    .expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    git_in(&promisor, &["config", "core.repositoryformatversion", "1"]);
    git_in(&promisor, &["config", "extensions.partialClone", "evil"]);
    git_in(
        &promisor,
        &["config", "remote.evil.url", &root.to_string_lossy()],
    );
    git_in(
        &promisor,
        &[
            "config",
            "remote.evil.uploadpack",
            &script.to_string_lossy(),
        ],
    );
    assert!(carries_trust_arg(
        &created_argv(&mut local, &servers, &promisor).await
    ));
    assert!(!probe.exists(), "no program ran");
    // A submodule below a declared path, in the remote HEAD's own tree.
    let submodule = checkout(root, "submodule", own);
    git_in(
        &submodule,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,.agents/sub",
        ],
    );
    git_in(&submodule, &["commit", "-q", "-m", "gitlink"]);
    git_in(
        &submodule,
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &submodule).await
    ));
    // An entry that differs from a declared path only by letter case, in the
    // remote HEAD's own tree. The commit is built from objects, so a
    // case-insensitive file system does not fold it into `.muse`.
    let collision = checkout(root, "case-collision", own);
    let blob = git_stdin(&collision, &["hash-object", "-w", "--stdin"], "{}\n");
    let subtree = git_stdin(
        &collision,
        &["mktree"],
        &format!("100644 blob {blob}\thooks.json\n"),
    );
    let head_tree = git_stdin(&collision, &["ls-tree", "HEAD"], "");
    let tree = git_stdin(
        &collision,
        &["mktree"],
        &format!("{head_tree}\n040000 tree {subtree}\t.MUSE\n"),
    );
    let commit = git_stdin(
        &collision,
        &["commit-tree", &tree, "-p", "HEAD", "-m", "pr"],
        "",
    );
    git_in(
        &collision,
        &["update-ref", "refs/remotes/origin/main", &commit],
    );
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &collision).await
    ));
    // A declared directory whose name on disk differs from the declared path
    // only by letter case is not the declared path, whether or not the file
    // system folds the two names.
    let renamed = checkout(root, "case-renamed", own);
    std::fs::rename(renamed.join(".muse"), renamed.join(".MUSE")).expect("rename .muse");
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &renamed).await
    ));
    // A symbolic link below a declared path.
    let linked = checkout(root, "symlink", own);
    std::fs::remove_file(linked.join(".agents/rules.md")).expect("remove rules");
    std::os::unix::fs::symlink("../README.md", linked.join(".agents/rules.md")).expect("link");
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &linked).await
    ));
    // An executable bit the remote tree does not have.
    let executable = checkout(root, "executable", own);
    std::fs::set_permissions(
        executable.join(".muse/hooks.json"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod hooks");
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &executable).await
    ));
    // A special file below a declared path is refused without being read.
    let fifo = checkout(root, "fifo", own);
    std::fs::remove_file(fifo.join(".agents/rules.md")).expect("remove rules");
    nix::unistd::mkfifo(
        &fifo.join(".agents/rules.md"),
        nix::sys::stat::Mode::from_bits_truncate(0o644),
    )
    .expect("create a fifo");
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &fifo).await
    ));
    // A declared file is hashed up to the size bound and refused above it,
    // although the remote tree holds the same bytes.
    for (name, size, trusted) in [
        ("file-at-bound", MAX_COMPARED_FILE_BYTES, true),
        ("file-over-bound", MAX_COMPARED_FILE_BYTES + 1, false),
    ] {
        let big = checkout(root, name, own);
        std::fs::write(
            big.join(".muse/big.bin"),
            vec![b'x'; usize::try_from(size).expect("size fits")],
        )
        .expect("write a large file");
        git_in(&big, &["add", "."]);
        git_in(&big, &["commit", "-q", "-m", "large"]);
        git_in(&big, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        assert_eq!(
            carries_trust_arg(&created_argv(&mut local, &servers, &big).await),
            trusted,
            "{name}"
        );
    }
    // Forged local refs and index do not help: the checkout's `origin/main`
    // follows a malicious commit, but the remote reports the real one.
    let forged = checkout(root, "forged-refs", own);
    let real_remote = remote_snapshot(&forged);
    std::fs::write(forged.join(".muse/hooks.json"), "{\"evil\": 1}\n").expect("edit hooks");
    git_in(&forged, &["commit", "-q", "-a", "-m", "pr"]);
    git_in(&forged, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    announce_remote_source(&servers, Some(real_remote));
    assert!(!carries_trust_arg(
        &launched_argv(&mut local, &forged).await
    ));

    // DoD 5: the remote repository cannot be fetched from.
    let unfetched = checkout(root, "unfetched-head", own);
    announce_remote_source(
        &servers,
        Some(root.join("missing.git").to_string_lossy().into_owned()),
    );
    assert!(!carries_trust_arg(
        &launched_argv(&mut local, &unfetched).await
    ));

    // A launch below the checkout root compares different trees, so it is not
    // trusted.
    let nested = clean.join(".muse");
    announce_remote_source(&servers, Some(remote_snapshot(&clean)));
    assert!(!carries_trust_arg(
        &launched_argv(&mut local, &nested).await
    ));

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn resume_evaluates_the_trust_again() {
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-relaunch",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(root, "relaunched", "https://github.com/acme/widgets.git");
    let hooks = dir.join(".muse/hooks.json");
    announce_remote_source(&servers, Some(remote_snapshot(&dir)));

    let created = start_named_session(&mut local, RUNTIME, &dir, None).await;
    assert!(carries_trust_arg(&printed_argv(&mut local, &created).await));
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(created)).await;

    // DoD 7: `.muse` changed after the create, so the resume is not trusted.
    std::fs::write(&hooks, "{\"changed\": 1}\n").expect("edit hooks");
    let resumed: protocol::SessionResumeResult =
        call(&mut local, method::SESSION_RESUME, json!(created)).await;
    let argv = printed_argv(&mut local, &resumed.session.id).await;
    assert!(!carries_trust_arg(&argv), "{argv:?}");
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(resumed.session.id)).await;

    // Restoring the file makes the next resume trusted again; the arguments
    // are never part of the persisted snapshot.
    std::fs::write(&hooks, "{}\n").expect("restore hooks");
    let resumed: protocol::SessionResumeResult =
        call(&mut local, method::SESSION_RESUME, json!(created)).await;
    let argv = printed_argv(&mut local, &resumed.session.id).await;
    assert_eq!(
        without_reference(&argv, &["--session"]),
        strings(&["--model", "fast", TRUST_ARG, "--session"])
    );
    assert!(
        !store_text(&servers).contains(TRUST_ARG),
        "the store holds no trusted argument"
    );

    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(resumed.session.id)).await;
    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_host_that_trusts_no_repository_never_adds_the_trusted_args() {
    let servers = spawn_servers_over("pkg-trust-none", Some("")).await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(root, "perfect", "https://github.com/acme/widgets.git");

    // DoD 2: an absent list trusts nothing, even for a perfect checkout.
    assert!(!carries_trust_arg(
        &created_argv(&mut local, &servers, &dir).await
    ));

    drop(local);
    servers.stop().await;
}

/// The `event` of the record [`DaemonLog::start`] writes to prove the capture
/// receives WARN records of the daemon target.
const LOG_PROBE_EVENT: &str = "daemon_log.probe";

/// The production JSON log of the daemon, captured for the lifetime of one
/// test.
///
/// Captures of one test binary run one after the other, each into its own
/// directory, and the log of every thread reaches the active one, so a scenario
/// selects its own records by a value only it produces: a checkout path, a
/// session id, or a runtime id.
struct DaemonLog {
    // Ended before the directory is removed.
    _capture: pohunek_daemon::logging::CaptureGuard,
    dir: tempfile::TempDir,
}

impl DaemonLog {
    fn start() -> Self {
        let dir = pohunek_test_support::tempdir().expect("log directory");
        let capture = pohunek_daemon::logging::capture(&dir.path().join("daemon-logs"))
            .expect("daemon logging initializes");
        let log = Self {
            _capture: capture,
            dir,
        };
        // The capture takes its filter from `RUST_LOG`. A WARN record of the
        // daemon's target that does not arrive would make every zero-count
        // assertion pass whatever the daemon does, so the log proves it
        // records before a scenario relies on it.
        tracing::warn!(target: "pohunek_daemon", event = LOG_PROBE_EVENT, "log capture probe");
        assert_eq!(
            log.event_records(LOG_PROBE_EVENT).len(),
            1,
            "the daemon log capture drops WARN records of the daemon target; unset RUST_LOG or let it enable them"
        );
        log
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("daemon-logs")
    }

    /// The records of the captured log whose `event` field is `event`.
    fn event_records(&self, event: &str) -> Vec<Value> {
        log_records(&self.path())
            .into_iter()
            .filter(|record| record["fields"]["event"] == event)
            .collect()
    }
}

#[tokio::test]
async fn a_git_failure_withholds_the_trusted_args_and_logs_one_line() {
    let log = DaemonLog::start();
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-failure",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(
        root,
        "unreachable-remote",
        "https://github.com/acme/widgets.git",
    );
    announce_remote_source(&servers, Some(remote_snapshot(&dir)));

    let created = start_named_session(&mut local, RUNTIME, &dir, None).await;
    assert!(carries_trust_arg(&printed_argv(&mut local, &created).await));
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(created)).await;

    // DoD 6: the remote HEAD cannot be read.
    announce_remote_source(&servers, None);
    let resumed: protocol::SessionResumeResult =
        call(&mut local, method::SESSION_RESUME, json!(created)).await;
    let argv = printed_argv(&mut local, &resumed.session.id).await;
    assert!(!carries_trust_arg(&argv), "{argv:?}");

    let lines = log.event_records("checkout_trust.failed");
    let lines: Vec<&Value> = lines
        .iter()
        .filter(|record| record["fields"]["checkout"] == dir.to_string_lossy().as_ref())
        .collect();
    assert_eq!(lines.len(), 1, "one trace line for the failure: {lines:?}");

    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(resumed.session.id)).await;
    drop(local);
    servers.stop().await;
}

/// What the read observer does at one step of one path, once.
type ReadAction = Box<dyn FnOnce() + Send>;

/// The pending action of [`on_read`].
type ReadHook = Arc<std::sync::Mutex<Option<(CheckoutReadStep, PathBuf, ReadAction)>>>;

/// Installs the read observer on `servers` and returns the slot that arms it.
fn read_hook(servers: &Servers) -> ReadHook {
    let hook: ReadHook = Arc::default();
    let armed = Arc::clone(&hook);
    servers
        .registry
        .set_checkout_read_observer(move |step, path| {
            let mut pending = armed.lock().expect("read hook");
            if pending
                .as_ref()
                .is_some_and(|(at_step, at_path, _)| *at_step == step && at_path == path)
            {
                if let Some((_, _, action)) = pending.take() {
                    action();
                }
            }
        });
    hook
}

/// Runs `action` once, when the evaluation reaches `step` of `path`.
fn on_read(hook: &ReadHook, step: CheckoutReadStep, path: PathBuf, action: ReadAction) {
    *hook.lock().expect("read hook") = Some((step, path, action));
}

/// Copies the directory `from` to `to`, files only one level deep below it.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the copy");
    for entry in std::fs::read_dir(from).expect("read the original") {
        let entry = entry.expect("directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy a file");
        }
    }
}

#[tokio::test]
async fn a_declared_path_swapped_for_a_link_while_the_checkout_is_read_is_not_trusted() {
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-swap",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let hook = read_hook(&servers);
    let root = servers.plugins.parent().expect("the socket directory");

    // Each case replaces one component with a link to a copy that holds the
    // very same bytes, so only the link itself can make the checkout differ.
    // The swap runs after the walk classified the component and before it is
    // opened.
    for (name, swapped, whole_directory) in [
        ("declared-directory", ".muse", true),
        ("nested-directory", ".agents/nested", true),
        ("declared-file", ".muse/hooks.json", false),
    ] {
        let dir = checkout(root, name, "https://github.com/acme/widgets.git");
        std::fs::create_dir_all(dir.join(".agents/nested")).expect("create a nested directory");
        std::fs::write(dir.join(".agents/nested/deep.md"), "deep\n").expect("write a deep file");
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "nested"]);
        git_in(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        announce_remote_source(&servers, Some(remote_snapshot(&dir)));
        assert!(
            carries_trust_arg(&launched_argv(&mut local, &dir).await),
            "{name}: the unmodified checkout is trusted"
        );

        let outside = root.join(format!("{name}-outside"));
        let target = dir.join(swapped);
        let replacement = outside.clone();
        let original = root.join(format!("{name}-original"));
        let swapped_path = target.clone();
        if whole_directory {
            copy_tree(&target, &outside);
        } else {
            std::fs::create_dir_all(&outside).expect("create the outside directory");
            std::fs::copy(&target, outside.join("hooks.json")).expect("copy the file");
        }
        let link_to = if whole_directory {
            replacement
        } else {
            replacement.join("hooks.json")
        };
        on_read(
            &hook,
            CheckoutReadStep::BeforeOpen,
            target,
            Box::new(move || {
                std::fs::rename(&swapped_path, &original).expect("move the original aside");
                std::os::unix::fs::symlink(&link_to, &swapped_path).expect("link");
            }),
        );
        assert!(
            !carries_trust_arg(&launched_argv(&mut local, &dir).await),
            "{name}: a component swapped for a link is not trusted"
        );
        assert!(
            hook.lock().expect("read hook").is_none(),
            "{name}: the swap ran"
        );
    }

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_declared_file_that_grows_after_the_size_check_is_refused_without_a_failure() {
    let log = DaemonLog::start();
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-growth",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let hook = read_hook(&servers);
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(root, "growing", "https://github.com/acme/widgets.git");
    announce_remote_source(&servers, Some(remote_snapshot(&dir)));
    assert!(carries_trust_arg(&launched_argv(&mut local, &dir).await));

    // A writer of the checkout extends the opened file to a sparse 64 GiB
    // after its size passed the check. The read stops one byte past the bound
    // and the checkout is refused without a failure.
    let grown = dir.join(".muse/hooks.json");
    let to_grow = grown.clone();
    on_read(
        &hook,
        CheckoutReadStep::BeforeRead,
        grown,
        Box::new(move || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&to_grow)
                .and_then(|file| file.set_len(GROWN_FILE_BYTES))
                .expect("grow the file");
        }),
    );
    assert!(!carries_trust_arg(&launched_argv(&mut local, &dir).await));
    assert!(hook.lock().expect("read hook").is_none(), "the growth ran");
    assert_eq!(failure_lines(&log, &dir), 0, "a refusal is not a failure");

    drop(local);
    servers.stop().await;
}

/// Installs the argv-printing runtime that adds [`TRUST_ARG`] to a trusted
/// checkout and declares only `paths`.
async fn install_runtime_declaring<S>(
    servers: &Servers,
    local: &mut Framed<S, LinesCodec>,
    paths: &[&str],
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let program = argv_program(&servers.plugins);
    let quoted: Vec<String> = paths.iter().map(|path| format!("\"{path}\"")).collect();
    let keys = format!(
        "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [{}]\n",
        quoted.join(", ")
    );
    let document = checkout_trust_descriptor(&program, CHECKOUT_TRUST_FEATURE, &keys);
    let (_digest, installed) =
        install_descriptor(local, &servers.plugins, "agent-trusting", &document, false).await;
    installed.expect("the package installs");
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario walks the whole life of a stalled evaluation against a single daemon"
)]
async fn an_evaluation_stalled_in_a_file_system_call_is_abandoned_at_its_deadline_and_not_trusted()
{
    let log = DaemonLog::start();
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-stall",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let hook = read_hook(&servers);
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(root, "stalled", "https://github.com/acme/widgets.git");
    announce_remote_source(&servers, Some(remote_snapshot(&dir)));
    assert!(
        carries_trust_arg(&launched_argv(&mut local, &dir).await),
        "the unmodified checkout is trusted"
    );

    // A call that never returns: the observer blocks the evaluation's thread
    // right before a declared file is opened, and holds it until `release` is
    // sent or dropped. The ceiling only keeps an evaluation that is not
    // abandoned from hanging the test. Once released, the evaluation finds the
    // file unreadable, so its late outcome is a failure that would be logged
    // if the launch had not abandoned it.
    let rules = dir.join(".agents/rules.md");
    let reached = Arc::new(tokio::sync::Notify::new());
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (signal, unreadable) = (Arc::clone(&reached), rules.clone());
    on_read(
        &hook,
        CheckoutReadStep::BeforeOpen,
        rules.clone(),
        Box::new(move || {
            signal.notify_one();
            let _ = released.recv_timeout(pohunek_test_support::wait::HANG_GUARD);
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
                .expect("make the file unreadable");
        }),
    );

    // The deadline runs on the paused clock: it moves only when the test
    // advances it, once the evaluation is stalled. The clock runs again as
    // soon as the deadline passed, so the rest of the launch is real time.
    let inhibitor = AutoAdvanceInhibitor::new();
    tokio::time::pause();
    let past_deadline = async {
        reached.notified().await;
        tokio::time::advance(CHECKOUT_TRUST_DEADLINE + TIMER_TICK).await;
        tokio::time::resume();
        inhibitor.release().await;
    };
    let (id, ()) = tokio::join!(
        start_named_session(&mut local, RUNTIME, &dir, None),
        past_deadline
    );
    let argv = printed_argv(&mut local, &id).await;
    let _: protocol::SessionStopResult = call(&mut local, method::SESSION_STOP, json!(id)).await;
    assert!(
        !carries_trust_arg(&argv),
        "a stalled evaluation does not trust the checkout"
    );

    // The records of the checkout: the refusals while the evaluation is
    // blocked, and the failures.
    let checkout_reasons = || -> Vec<String> {
        log.event_records("checkout_trust.failed")
            .iter()
            .filter(|record| record["fields"]["checkout"] == dir.to_string_lossy().as_ref())
            .map(|record| {
                record["fields"]["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    };
    let refusals = || {
        checkout_reasons()
            .iter()
            .filter(|reason| reason.contains("still stalled"))
            .count()
    };

    // While the abandoned evaluation is still blocked, another launch of the
    // checkout is refused at once instead of starting a second one.
    assert!(
        !carries_trust_arg(&launched_argv(&mut local, &dir).await),
        "a checkout with a stalled evaluation is not trusted"
    );
    assert_eq!(refusals(), 1, "the refused launch is traced");
    drop(release);
    assert!(hook.lock().expect("read hook").is_none(), "the stall ran");

    // The blocked evaluation ends once a launch is no longer refused. That
    // launch evaluates the unreadable file itself and fails.
    let shared = tokio::sync::Mutex::new(&mut local);
    let (shared, dir_ref, refusals_ref) = (&shared, &dir, &refusals);
    pohunek_test_support::wait::wait_until("the stalled evaluation to end", || async move {
        let before = refusals_ref();
        let mut local = shared.lock().await;
        launched_argv(&mut **local, dir_ref).await;
        (refusals_ref() == before).then_some(())
    })
    .await;
    std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o644))
        .expect("make the file readable again");
    assert!(
        carries_trust_arg(&launched_argv(&mut local, &dir).await),
        "the checkout is trusted again once the blocked evaluation ended"
    );

    // Exactly the abandonment and the unreadable file of the one later launch
    // were reported: the abandoned evaluation's own late failure stays silent.
    let reasons: Vec<String> = checkout_reasons()
        .into_iter()
        .filter(|reason| !reason.contains("still stalled"))
        .collect();
    assert_eq!(
        reasons,
        [
            "the evaluation did not finish within its total deadline",
            "a declared path could not be read",
        ],
        "one failure line per evaluation that was waited for"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn an_evaluation_that_answers_as_its_deadline_passes_keeps_its_answer() {
    let log = DaemonLog::start();
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-edge",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let hook = read_hook(&servers);
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(root, "edge", "https://github.com/acme/widgets.git");
    announce_remote_source(&servers, Some(remote_snapshot(&dir)));

    // The evaluation records "trusted" and its thread is then held before it
    // returns, so the launch's deadline passes while the answer is known but
    // the task has not ended. `release` lets the thread return after the
    // launch completed; the ceiling only keeps a broken run from hanging.
    let reached = Arc::new(tokio::sync::Notify::new());
    let (release, released) = std::sync::mpsc::channel::<()>();
    let signal = Arc::clone(&reached);
    on_read(
        &hook,
        CheckoutReadStep::Evaluated,
        dir.clone(),
        Box::new(move || {
            signal.notify_one();
            let _ = released.recv_timeout(pohunek_test_support::wait::HANG_GUARD);
        }),
    );

    let inhibitor = AutoAdvanceInhibitor::new();
    tokio::time::pause();
    let past_deadline = async {
        reached.notified().await;
        tokio::time::advance(CHECKOUT_TRUST_DEADLINE + TIMER_TICK).await;
        tokio::time::resume();
        inhibitor.release().await;
    };
    let (id, ()) = tokio::join!(
        start_named_session(&mut local, RUNTIME, &dir, None),
        past_deadline
    );
    let argv = printed_argv(&mut local, &id).await;
    let _: protocol::SessionStopResult = call(&mut local, method::SESSION_STOP, json!(id)).await;
    drop(release);
    assert!(hook.lock().expect("read hook").is_none(), "the hold ran");
    assert!(
        carries_trust_arg(&argv),
        "an answer recorded before the deadline is the launch's answer: {argv:?}"
    );
    let failures: Vec<Value> = log
        .event_records("checkout_trust.failed")
        .into_iter()
        .filter(|record| record["fields"]["checkout"] == dir.to_string_lossy().as_ref())
        .collect();
    assert!(failures.is_empty(), "nothing failed: {failures:?}");

    drop(local);
    servers.stop().await;
}

/// The entries of the checkout trust scratch directory below `state_root`
/// whose name has the evaluation prefix.
fn trust_scratch_entries(state_root: &Path) -> Vec<String> {
    match std::fs::read_dir(state_root.join("checkout-trust")) {
        Ok(entries) => entries
            .map(|entry| entry.expect("scratch entry").file_name())
            .filter_map(|name| name.into_string().ok())
            .filter(|name| name.starts_with("pohunek-trust-"))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read the scratch directory: {error}"),
    }
}

#[tokio::test]
async fn a_daemon_start_removes_the_trust_scratch_a_stopped_daemon_left() {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut state_root = None;
    let servers = spawn_servers_configured("pkg-trust-sweep", Some(""), |config| {
        config.catalog_trust = CatalogTrust::Absent;
        config.trusted_repositories = host_trusting(&["acme/widgets"]);
        let root = config
            .worker_state_root
            .clone()
            .expect("the fixture configures a worker state root");
        // What a daemon that stopped during a blocked evaluation leaves: a
        // populated private repository, next to an entry the sweep must keep.
        let scratch = root.join("checkout-trust");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(scratch.join("pohunek-trust-left/objects/ab"))
            .expect("create the left-over repository");
        std::fs::write(
            scratch.join("pohunek-trust-left/HEAD"),
            "ref: refs/heads/main\n",
        )
        .expect("write a repository file");
        std::fs::write(scratch.join("pohunek-trust-left/objects/ab/cdef"), "tree")
            .expect("write an object");
        std::fs::write(scratch.join("unrelated"), "kept").expect("write an unrelated entry");
        state_root = Some(root);
    })
    .await;
    let state_root = state_root.expect("the configure step ran");
    assert_eq!(
        trust_scratch_entries(&state_root),
        Vec::<String>::new(),
        "the start removed the left-over repository"
    );
    assert!(
        state_root.join("checkout-trust/unrelated").exists(),
        "an entry without the evaluation prefix is kept"
    );

    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(root, "after-sweep", "https://github.com/acme/widgets.git");
    announce_remote_source(&servers, Some(remote_snapshot(&dir)));
    assert!(
        carries_trust_arg(&launched_argv(&mut local, &dir).await),
        "a trusted launch works after the sweep"
    );
    assert_eq!(
        trust_scratch_entries(&state_root),
        Vec::<String>::new(),
        "the evaluation removed its own repository"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn an_ancestor_of_a_nested_declared_path_swapped_for_a_link_is_not_trusted() {
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-ancestor",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_runtime_declaring(&servers, &mut local, &["outer/mid/inner"]).await;
    let hook = read_hook(&servers);
    let root = servers.plugins.parent().expect("the socket directory");

    // The runtime declares only a three-component path, so the swapped
    // component is an ancestor that the lookup of the declared path opens,
    // not a path the walk of the declared tree reaches. The link leads to a
    // copy with the very same bytes.
    for (name, swapped) in [("outermost", "outer"), ("middle", "outer/mid")] {
        let dir = checkout(root, name, "https://github.com/acme/widgets.git");
        std::fs::create_dir_all(dir.join("outer/mid/inner")).expect("create the declared path");
        std::fs::write(dir.join("outer/mid/inner/file.md"), "inner\n").expect("write a file");
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "nested"]);
        git_in(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        announce_remote_source(&servers, Some(remote_snapshot(&dir)));
        assert!(
            carries_trust_arg(&launched_argv(&mut local, &dir).await),
            "{name}: the unmodified checkout is trusted"
        );

        let target = dir.join(swapped);
        let outside = root.join(format!("{name}-outside"));
        copy_tree(&target, &outside);
        let original = root.join(format!("{name}-original"));
        let swapped_path = target.clone();
        on_read(
            &hook,
            CheckoutReadStep::BeforeOpen,
            target,
            Box::new(move || {
                std::fs::rename(&swapped_path, &original).expect("move the original aside");
                std::os::unix::fs::symlink(&outside, &swapped_path).expect("link");
            }),
        );
        assert!(
            !carries_trust_arg(&launched_argv(&mut local, &dir).await),
            "{name}: an ancestor swapped for a link is not trusted"
        );
        assert!(
            hook.lock().expect("read hook").is_none(),
            "{name}: the swap ran"
        );
    }

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn many_declared_files_are_hashed_by_one_git_process_and_each_hash_belongs_to_its_file() {
    use std::io::Write as _;

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-hash-once",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(root, "many-files", "https://github.com/acme/widgets.git");
    let names: Vec<String> = (0..8)
        .map(|index| format!(".agents/rule-{index}.md"))
        .collect();
    for name in &names {
        std::fs::write(dir.join(name), format!("rule {name}\n")).expect("write a declared file");
    }
    git_in(&dir, &["add", "."]);
    git_in(&dir, &["commit", "-q", "-m", "many"]);
    git_in(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    announce_remote_source(&servers, Some(remote_snapshot(&dir)));

    // A `git` first on the daemon's PATH that records each `fast-import` run
    // and then runs the real one. The fixtures above are built before the
    // environment is taken: the test's own git runs resolve `PATH` under the
    // same lock.
    let real_git = pohunek_test_support::process_env::command("git")
        .get_program()
        .to_owned();
    let wrapper_dir = pohunek_test_support::tempdir_with_prefix("ph-gitwrap-")
        .expect("create the wrapper directory");
    let record = wrapper_dir.path().join("fast-import-runs");
    let wrapper = wrapper_dir.path().join("git");
    let script = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = fast-import ]; then printf 'run\\n' >> '{}'; fi\ndone\nexec '{}' \"$@\"\n",
        record.display(),
        real_git.to_string_lossy()
    );
    std::fs::File::create(&wrapper)
        .and_then(|mut file| file.write_all(script.as_bytes()))
        .expect("write the git wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("make the wrapper executable");
    let mut env = pohunek_test_support::process_env::ProcessEnv::lock();
    let search = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![wrapper_dir.path().to_path_buf()];
    dirs.extend(std::env::split_paths(&search));
    env.set("PATH", std::env::join_paths(dirs).expect("join PATH"));

    let runs = || std::fs::read_to_string(&record).map_or(0, |text| text.lines().count());
    assert!(
        carries_trust_arg(&launched_argv(&mut local, &dir).await),
        "the unmodified checkout is trusted"
    );
    assert_eq!(runs(), 1, "eight declared files are hashed by one process");

    // Two files trade their contents: the set of contents is still the one
    // the remote holds, so only the pairing of each file with its own hash
    // can tell.
    let (first, second) = (dir.join(&names[1]), dir.join(&names[6]));
    let (kept, other) = (
        std::fs::read(&first).expect("read the first"),
        std::fs::read(&second).expect("read the second"),
    );
    std::fs::write(&first, other).expect("write the first");
    std::fs::write(&second, kept).expect("write the second");
    assert!(
        !carries_trust_arg(&launched_argv(&mut local, &dir).await),
        "files with exchanged contents are not trusted"
    );
    assert_eq!(runs(), 2, "the second evaluation hashes in one process too");

    drop(env);
    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_deleted_nested_file_under_a_declared_path_withholds_the_trusted_args() {
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-nested",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(root, "nested", "https://github.com/acme/widgets.git");
    let nested = dir.join(".agents/skills/deep");
    std::fs::create_dir_all(&nested).expect("create nested directories");
    std::fs::write(dir.join(".agents/skills/s.md"), "skill\n").expect("write skill");
    std::fs::write(nested.join("t.md"), "deep\n").expect("write deep skill");
    git_in(&dir, &["add", "."]);
    git_in(&dir, &["commit", "-q", "-m", "nested"]);
    git_in(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    announce_remote_source(&servers, Some(remote_snapshot(&dir)));

    assert!(carries_trust_arg(&launched_argv(&mut local, &dir).await));

    // Every file of the remote tree must be on disk: a deletion at any depth
    // withholds the arguments, and restoring the file trusts the checkout again.
    for (name, file, content) in [
        ("top", ".agents/rules.md", "rules\n"),
        ("middle", ".agents/skills/s.md", "skill\n"),
        ("deep", ".agents/skills/deep/t.md", "deep\n"),
    ] {
        let path = dir.join(file);
        std::fs::remove_file(&path).expect("remove a declared file");
        assert!(
            !carries_trust_arg(&launched_argv(&mut local, &dir).await),
            "{name}"
        );
        std::fs::write(&path, content).expect("restore the file");
        assert!(
            carries_trust_arg(&launched_argv(&mut local, &dir).await),
            "{name} restored"
        );
    }

    drop(local);
    servers.stop().await;
}

/// A clean checkout of `acme/widgets` whose remote `HEAD` holds no
/// `.muse` and no `.agents`, so no declared path of the trusting runtimes is
/// tracked.
fn checkout_without_declared_paths(root: &Path, name: &str) -> PathBuf {
    let dir = checkout(root, name, "https://github.com/acme/widgets.git");
    git_in(&dir, &["rm", "-q", "-r", ".muse", ".agents"]);
    git_in(&dir, &["commit", "-q", "-m", "drop"]);
    git_in(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    dir
}

/// Adds an entry named `spelling` below `dir` and reports whether this file
/// system opens `declared` through it.
fn add_variant_spelling(dir: &Path, declared: &str, spelling: &str) -> bool {
    std::fs::create_dir(dir.join(spelling)).expect("create the variant spelling");
    std::fs::write(dir.join(spelling).join("hooks.json"), "{}\n").expect("write hooks");
    dir.join(declared).exists()
}

#[tokio::test]
async fn a_declared_path_that_the_file_system_opens_under_another_spelling_is_not_trusted() {
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-spelling",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    install_trusting_runtime(&servers, &mut local).await;
    let root = servers.plugins.parent().expect("the socket directory");

    // U+017F (long s) folds to `s` under Unicode case folding: the sibling
    // refuses on every file system, and a folding one would open `.muse`
    // through it.
    let long_s = checkout_without_declared_paths(root, "long-s");
    add_variant_spelling(&long_s, ".muse", ".mu\u{17f}e");
    announce_remote_source(&servers, Some(remote_snapshot(&long_s)));
    assert!(!carries_trust_arg(
        &launched_argv(&mut local, &long_s).await
    ));

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_declared_non_ascii_path_stored_in_another_normalization_is_not_trusted() {
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-normalization",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    let program = argv_program(&servers.plugins);
    let keys = format!(
        "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [\"caf\u{e9}\"]\n"
    );
    let document = checkout_trust_descriptor(&program, CHECKOUT_TRUST_FEATURE, &keys);
    let (_digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-trusting-nfc",
        &document,
        false,
    )
    .await;
    installed.expect("the package installs");
    let root = servers.plugins.parent().expect("the socket directory");

    // The declared name is NFC; a normalization-insensitive file system
    // opens the NFD spelling as the same name.
    let nfd = checkout_without_declared_paths(root, "nfd");
    let folded = add_variant_spelling(&nfd, "caf\u{e9}", "cafe\u{301}");
    announce_remote_source(&servers, Some(remote_snapshot(&nfd)));
    assert_eq!(
        carries_trust_arg(&launched_argv(&mut local, &nfd).await),
        !folded,
        "withheld exactly when the file system opens the NFC name through the NFD one"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one table of descriptors walks every refusal against a single daemon"
)]
async fn trusted_checkout_keys_install_only_as_a_valid_declaration() {
    let servers = spawn_servers("pkg-trust-descriptor").await;
    let mut local = connect_unix(&servers.socket).await;
    let program = argv_program(&servers.plugins);

    let without_requires = checkout_trust_descriptor(&program, "", &trusting_keys());
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "trust-without-requires",
        &without_requires,
        false,
    )
    .await;
    assert_eq!(
        result.expect_err("a gated key without requires").code,
        "runtime_feature_unsupported"
    );

    let too_many_paths: Vec<String> = (0..17).map(|n| format!("\"p{n}\"")).collect();
    let long_path = format!("\"{}\"", "a".repeat(257));
    for (name, keys) in [
        ("args-only", format!("trusted_checkout_args = [\"{TRUST_ARG}\"]\n")),
        (
            "paths-only",
            "trusted_checkout_paths = [\".muse\"]\n".to_owned(),
        ),
        (
            "empty-paths",
            format!("trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = []\n"),
        ),
        (
            "empty-args",
            "trusted_checkout_args = []\ntrusted_checkout_paths = [\".muse\"]\n".to_owned(),
        ),
        (
            "placeholder",
            "trusted_checkout_args = [\"--name={session_name}\"]\ntrusted_checkout_paths = [\".muse\"]\n".to_owned(),
        ),
        (
            "too-many-paths",
            format!(
                "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [{}]\n",
                too_many_paths.join(", ")
            ),
        ),
        (
            "nested-path",
            format!(
                "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [\".agents\", \".agents/skills\"]\n"
            ),
        ),
        (
            "nested-path-reversed",
            format!(
                "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [\".agents/skills\", \".agents\"]\n"
            ),
        ),
        (
            "nested-path-letter-case",
            format!(
                "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [\".muse\", \".MUSE/hooks\"]\n"
            ),
        ),
        (
            "repeated-path-letter-case",
            format!(
                "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [\".muse\", \".Muse\"]\n"
            ),
        ),
        (
            "long-path",
            format!(
                "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [{long_path}]\n"
            ),
        ),
    ] {
        let document = checkout_trust_descriptor(&program, CHECKOUT_TRUST_FEATURE, &keys);
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            &format!("trust-{name}"),
            &document,
            false,
        )
        .await;
        assert!(result.is_err(), "{name} is refused");
    }
    for (name, path) in [
        ("absolute", "/etc"),
        ("parent", "../muse"),
        ("nested-parent", "a/../b"),
        ("dot", "."),
        ("current", "./muse"),
        ("empty-segment", "a//b"),
        ("trailing-slash", "muse/"),
        ("git-dir", ".git"),
        ("git-dir-nested", "x/.GIT/hooks"),
        ("backslash", "a\\\\b"),
        ("colon", "a:b"),
    ] {
        let keys = format!(
            "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [\"{path}\"]\n"
        );
        let document = checkout_trust_descriptor(&program, CHECKOUT_TRUST_FEATURE, &keys);
        let (_digest, result) = install_descriptor(
            &mut local,
            &servers.plugins,
            &format!("trust-path-{name}"),
            &document,
            false,
        )
        .await;
        assert!(result.is_err(), "the path {name} is refused");
    }
    let repeated = format!(
        "trusted_checkout_args = [\"{TRUST_ARG}\"]\ntrusted_checkout_paths = [\".muse\", \".muse\"]\n"
    );
    let document = checkout_trust_descriptor(&program, CHECKOUT_TRUST_FEATURE, &repeated);
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "trust-repeated",
        &document,
        false,
    )
    .await;
    assert!(result.is_err(), "a repeated path is refused");

    let document = checkout_trust_descriptor(&program, CHECKOUT_TRUST_FEATURE, &trusting_keys());
    let (_digest, result) = install_descriptor(
        &mut local,
        &servers.plugins,
        "trust-valid",
        &document,
        false,
    )
    .await;
    result.expect("a valid declaration installs");

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_frozen_trusted_arg_is_withheld_from_an_untrusted_resume() {
    let servers = spawn_servers_over("pkg-trust-frozen", Some("")).await;
    let mut local = connect_unix(&servers.socket).await;
    let program = argv_program(&servers.plugins);
    let document = checkout_trust_descriptor(&program, CHECKOUT_TRUST_FEATURE, &trusting_keys())
        .replace(
            "args = [\"--model\", \"fast\"]\n",
            &format!("args = [\"--model\", \"fast\", \"{TRUST_ARG}\"]\n"),
        );
    let (_digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-frozen",
        &document,
        false,
    )
    .await;
    installed.expect("the package installs");
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(root, "frozen", "https://github.com/acme/widgets.git");
    announce_remote_source(&servers, Some(remote_snapshot(&dir)));

    // The fixed argument is frozen into the session, but the host trusts no
    // repository, so neither the create nor a resume launches with it.
    let created = start_named_session(&mut local, RUNTIME, &dir, None).await;
    let argv = printed_argv(&mut local, &created).await;
    assert!(!carries_trust_arg(&argv), "{argv:?}");
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(created)).await;
    let resumed: protocol::SessionResumeResult =
        call(&mut local, method::SESSION_RESUME, json!(created)).await;
    let argv = printed_argv(&mut local, &resumed.session.id).await;
    assert!(!carries_trust_arg(&argv), "{argv:?}");

    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(resumed.session.id)).await;
    drop(local);
    servers.stop().await;
}

/// Occurrences of [`TRUST_ARG`] in `argv`.
fn trust_arg_count(argv: &[String]) -> usize {
    argv.iter().filter(|arg| *arg == TRUST_ARG).count()
}

#[tokio::test]
async fn a_trusted_arg_in_fixed_args_or_a_profile_is_added_only_by_the_predicate() {
    let servers = spawn_servers_trusting_repositories(
        "pkg-trust-fixed",
        Some(""),
        CatalogTrust::Absent,
        host_trusting(&["acme/widgets"]),
    )
    .await;
    let mut local = connect_unix(&servers.socket).await;
    let program = argv_program(&servers.plugins);
    let document = checkout_trust_descriptor(&program, CHECKOUT_TRUST_FEATURE, &trusting_keys())
        .replace(
            "args = [\"--model\", \"fast\"]\n",
            &format!("args = [\"--model\", \"fast\", \"{TRUST_ARG}\"]\n"),
        );
    let (digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-fixed",
        &document,
        false,
    )
    .await;
    installed.expect("the package installs");
    std::fs::write(
        servers.agents.join("profiled.toml"),
        format!(
            "base = \"{RUNTIME}\"\npackage = \"acme.runtime.pi\"\ndigest = \"{}\"\nprogram = \"{}\"\nargs = [\"--from-profile\", \"{TRUST_ARG}\"]\n",
            digest.as_str(),
            program.display()
        ),
    )
    .expect("write the profile");
    let root = servers.plugins.parent().expect("the socket directory");

    // A checkout the predicate does not accept launches without the argument,
    // whichever of the descriptor and the profile holds it.
    let fork = checkout(root, "fixed-fork", "https://github.com/mallory/widgets.git");
    announce_remote_source(&servers, Some(remote_snapshot(&fork)));
    for agent in [RUNTIME, "profiled"] {
        let id = start_named_session(&mut local, agent, &fork, None).await;
        let argv = printed_argv(&mut local, &id).await;
        assert_eq!(trust_arg_count(&argv), 0, "{agent}: {argv:?}");
        let _: protocol::SessionStopResult =
            call(&mut local, method::SESSION_STOP, json!(id)).await;
    }

    // A trusted checkout gets it once, from the predicate, after the fixed
    // arguments.
    let own = checkout(root, "fixed-own", "https://github.com/acme/widgets.git");
    announce_remote_source(&servers, Some(remote_snapshot(&own)));
    for (agent, fixed) in [
        (RUNTIME, strings(&["--model", "fast"])),
        ("profiled", strings(&["--from-profile"])),
    ] {
        let id = start_named_session(&mut local, agent, &own, None).await;
        let argv = printed_argv(&mut local, &id).await;
        assert_eq!(trust_arg_count(&argv), 1, "{agent}: {argv:?}");
        assert_eq!(
            without_reference(&argv, &["--session-id"]),
            [fixed, strings(&[TRUST_ARG, "--session-id"])].concat(),
            "{agent}"
        );
        let _: protocol::SessionStopResult =
            call(&mut local, method::SESSION_STOP, json!(id)).await;
    }

    drop(local);
    servers.stop().await;
}

// ---------------------------------------------------------------------------
// Trusted checkout arguments follow the directory a session was created in
// ---------------------------------------------------------------------------

/// A runtime program that prints its arguments, then moves its own working
/// directory on request: `osc <dir>` changes the directory of the process and
/// prints an OSC 7 hint for it, so the focus process agrees with the hint and
/// no procwatch tick can move the session back; `cd <dir>` changes the
/// directory of the process alone.
fn drifting_program(plugins: &Path) -> PathBuf {
    write_program(
        plugins,
        "agent-drift",
        "#!/bin/sh\nfor a in \"$@\"; do printf 'ARG<%s>\\n' \"$a\"; done\nprintf 'ARGV_END\\n'\nwhile read -r mode dir; do\n  case \"$mode\" in\n    osc) cd \"$dir\"; printf '\\033]7;file://%s\\007' \"$dir\" ;;\n    cd) cd \"$dir\" ;;\n  esac\ndone\nexec sleep 600\n",
    )
}

/// Installs the runtime that declares the trusted-checkout keys, holds
/// [`TRUST_ARG`] in its fixed arguments, assigns its forks a reference and
/// drifts its directory on request. `program` is the same path for every
/// daemon of a scenario, so a package installed again keeps its digest.
async fn install_drifting_runtime<S>(
    servers: &Servers,
    local: &mut Framed<S, LinesCodec>,
    program: &Path,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let document = checkout_trust_descriptor(program, CHECKOUT_TRUST_FEATURE, &trusting_keys())
        .replace(
            "args = [\"--model\", \"fast\"]\n",
            &format!("args = [\"--model\", \"fast\", \"{TRUST_ARG}\"]\n"),
        )
        .replace(
            "launch_args = [\"--session-id\", \"{reference}\"]\n",
            "launch_args = [\"--session-id\", \"{reference}\"]\nassign_fork = true\n",
        );
    let (_digest, installed) =
        install_descriptor(local, &servers.plugins, "agent-drifting", &document, false).await;
    installed.expect("the package installs");
}

/// Registers the project of `dir` by creating and stopping a session there, so
/// a later move into it resolves a project whose origin is trusted or not as
/// the checkout's is. Returns the project id the session was bound to.
async fn register_project<S>(framed: &mut Framed<S, LinesCodec>, dir: &Path) -> Option<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let id = start_named_session(framed, RUNTIME, dir, None).await;
    let info: protocol::SessionInfo = call(framed, method::SESSION_INSPECT, json!(id)).await;
    let _: protocol::SessionStopResult = call(framed, method::SESSION_STOP, json!(id)).await;
    info.project_id
}

/// The working directory the daemon reports for the session.
async fn reported_cwd<S>(framed: &mut Framed<S, LinesCodec>, id: &protocol::SessionId) -> PathBuf
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let info: protocol::SessionInfo = call(framed, method::SESSION_INSPECT, json!(id)).await;
    info.cwd
}

/// Moves the running session's reported directory to `dir` through the
/// terminal (`osc`) or the process (`cd`), and waits until the daemon reports
/// it.
async fn drift<S>(
    framed: &mut Framed<S, LinesCodec>,
    id: &protocol::SessionId,
    how: &str,
    dir: &Path,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let accepted: Value = call(
        framed,
        method::SESSION_INPUT,
        json!({ "session_id": id, "text": format!("{how} {}", dir.display()) }),
    )
    .await;
    drop(accepted);
    let shared = tokio::sync::Mutex::new(framed);
    let shared = &shared;
    pohunek_test_support::wait::wait_until(
        "the daemon to report the moved directory",
        || async move {
            let mut framed = shared.lock().await;
            (reported_cwd(&mut **framed, id).await == dir).then_some(())
        },
    )
    .await;
}

/// Stops the running session `id`, resumes it and returns the arguments the
/// resumed process printed with the id of the resumed session.
async fn resumed_argv<S>(
    framed: &mut Framed<S, LinesCodec>,
    id: &protocol::SessionId,
) -> Vec<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let _: protocol::SessionStopResult = call(framed, method::SESSION_STOP, json!(id)).await;
    let resumed: protocol::SessionResumeResult =
        call(framed, method::SESSION_RESUME, json!(id)).await;
    printed_argv(framed, &resumed.session.id).await
}

/// How many `checkout_trust.failed` lines the daemon logged for `dir`.
fn failure_lines(log: &DaemonLog, dir: &Path) -> usize {
    log.event_records("checkout_trust.failed")
        .into_iter()
        .filter(|record| record["fields"]["checkout"] == dir.to_string_lossy().as_ref())
        .count()
}

/// The create-time directory and project the store holds for session `id`.
fn recorded_launch_checkout(store: &str, id: &protocol::SessionId) -> Option<Value> {
    store
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["kind"] == "session" && line["session_id"] == json!(id))
        .and_then(|line| line["recovery"].get("launch_checkout").cloned())
}

#[tokio::test]
async fn the_trusted_args_follow_the_directory_the_session_was_created_in() {
    let log = DaemonLog::start();
    let servers = spawn_servers_configured("pkg-trust-anchor", Some(""), |config| {
        config.trusted_repositories = host_trusting(&["acme/widgets"]);
        config.procwatch_poll = std::time::Duration::from_millis(50);
    })
    .await;
    let mut local = connect_unix(&servers.socket).await;
    let program = drifting_program(&servers.plugins);
    install_drifting_runtime(&servers, &mut local, &program).await;
    let root = servers.plugins.parent().expect("the socket directory");
    let own = "https://github.com/acme/widgets.git";
    let anchor = checkout(root, "anchor", own);
    // A second clone of the same upstream: its declared paths equal the remote.
    let second = checkout(root, "second-clone", own);
    announce_remote_source(&servers, Some(remote_snapshot(&anchor)));
    register_project(&mut local, &second).await;

    // Rows 1, 2: create and an unmoved resume get the arguments once, from the
    // predicate, although the fixed arguments hold them too; the store holds
    // the create-time pair.
    let created = start_named_session(&mut local, RUNTIME, &anchor, None).await;
    let argv = printed_argv(&mut local, &created).await;
    assert_eq!(trust_arg_count(&argv), 1, "{argv:?}");
    let recorded = recorded_launch_checkout(&store_text(&servers), &created)
        .expect("the store records the launch checkout");
    assert_eq!(recorded["cwd"], json!(anchor));
    let project = recorded["project_id"]
        .as_str()
        .expect("the store records the project")
        .to_owned();
    let argv = resumed_argv(&mut local, &created).await;
    assert_eq!(trust_arg_count(&argv), 1, "{argv:?}");

    // Row 3: an OSC 7 hint moves the session into a second clone with trusted
    // content; the resume launches there without the arguments and logs one
    // failure.
    drift(&mut local, &created, "osc", &second).await;
    assert_eq!(failure_lines(&log, &second), 0);
    let argv = resumed_argv(&mut local, &created).await;
    assert_eq!(trust_arg_count(&argv), 0, "{argv:?}");
    assert_eq!(reported_cwd(&mut local, &created).await, second);
    assert_eq!(failure_lines(&log, &second), 1);

    // Row 6: the resume reset the cwd source to the launch, yet the next
    // resume in the moved directory still withholds the arguments.
    let argv = resumed_argv(&mut local, &created).await;
    assert_eq!(trust_arg_count(&argv), 0, "{argv:?}");
    assert_eq!(failure_lines(&log, &second), 2);

    // Row 5: back in the launch directory, the arguments return.
    drift(&mut local, &created, "osc", &anchor).await;
    let argv = resumed_argv(&mut local, &created).await;
    assert_eq!(trust_arg_count(&argv), 1, "{argv:?}");

    // Row 4: the same move reported by the focus process withholds too.
    drift(&mut local, &created, "cd", &second).await;
    let argv = resumed_argv(&mut local, &created).await;
    assert_eq!(trust_arg_count(&argv), 0, "{argv:?}");
    assert_eq!(
        recorded_launch_checkout(&store_text(&servers), &created),
        Some(recorded),
        "the launch checkout is never refreshed"
    );

    // Row 7: a session of the trusted project started in a checkout of another
    // origin records that project with that directory. After a move away and
    // back, its current project is the directory's own untrusted one, yet the
    // relaunch is evaluated on the recorded project and gets the arguments.
    let foreign = checkout(
        root,
        "foreign-origin",
        "https://github.com/mallory/widgets.git",
    );
    let foreign_project = register_project(&mut local, &foreign)
        .await
        .expect("the foreign checkout is a project");
    let by_reference = start_session_in(&mut local, RUNTIME, &foreign, None, Some(&project)).await;
    let argv = printed_argv(&mut local, &by_reference).await;
    assert_eq!(trust_arg_count(&argv), 1, "{argv:?}");
    assert_eq!(
        recorded_launch_checkout(&store_text(&servers), &by_reference),
        Some(json!({ "cwd": foreign, "project_id": project }))
    );
    drift(&mut local, &by_reference, "osc", &second).await;
    drift(&mut local, &by_reference, "osc", &foreign).await;
    let info: protocol::SessionInfo =
        call(&mut local, method::SESSION_INSPECT, json!(by_reference)).await;
    assert_eq!(info.project_id.as_deref(), Some(foreign_project.as_str()));
    let argv = resumed_argv(&mut local, &by_reference).await;
    assert_eq!(trust_arg_count(&argv), 1, "{argv:?}");

    for id in [&created, &by_reference] {
        let _: protocol::SessionStopResult =
            call(&mut local, method::SESSION_STOP, json!(id)).await;
    }
    drop(local);
    servers.stop().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one session walks a move, a restart and a store without the launch checkout"
)]
async fn a_resumed_session_keeps_its_launch_checkout_across_a_restart_and_a_migration() {
    let log = DaemonLog::start();
    let base = pohunek_test_support::tempdir().expect("checkout root");
    let own = "https://github.com/acme/widgets.git";
    let anchor = checkout(base.path(), "anchor", own);
    let second = checkout(base.path(), "second-clone", own);
    let trusting = |config: &mut pohunek_daemon::session::SessionRegistryConfig| {
        config.trusted_repositories = host_trusting(&["acme/widgets"]);
    };
    let program = drifting_program(&base.path().join("plugins"));
    let servers = spawn_servers_configured("pkg-trust-resume", Some(""), trusting).await;
    let mut local = connect_unix(&servers.socket).await;
    install_drifting_runtime(&servers, &mut local, &program).await;
    announce_remote_source(&servers, Some(remote_snapshot(&anchor)));
    register_project(&mut local, &second).await;

    // A session created in the anchor gets the arguments.
    let created = start_named_session(&mut local, RUNTIME, &anchor, None).await;
    assert_eq!(
        trust_arg_count(&printed_argv(&mut local, &created).await),
        1
    );

    // A session moved out of the anchor resumes in the moved directory without
    // the arguments, and the record keeps the anchor as its launch checkout.
    drift(&mut local, &created, "osc", &second).await;
    assert_eq!(
        trust_arg_count(&resumed_argv(&mut local, &created).await),
        0
    );
    let store = store_text(&servers);
    assert_eq!(
        recorded_launch_checkout(&store, &created).expect("the session records a launch checkout")
            ["cwd"],
        json!(anchor)
    );

    // After a daemon restart the stored launch checkout still lets the
    // unmoved session resume with the arguments.
    drift(&mut local, &created, "osc", &anchor).await;
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(created)).await;
    let store = store_text(&servers);
    drop(local);
    servers.stop().await;
    let servers = spawn_servers_configured("pkg-trust-restart", Some(&store), trusting).await;
    let mut local = connect_unix(&servers.socket).await;
    install_drifting_runtime(&servers, &mut local, &program).await;
    servers
        .registry
        .reconcile_workers()
        .await
        .expect("startup reconciles the stored sessions");
    announce_remote_source(&servers, Some(remote_snapshot(&anchor)));
    let resumed: protocol::SessionResumeResult =
        call(&mut local, method::SESSION_RESUME, json!(created)).await;
    let argv = printed_argv(&mut local, &resumed.session.id).await;
    assert_eq!(trust_arg_count(&argv), 1, "{argv:?}");
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(created)).await;
    let store = store_text(&servers);
    drop(local);
    servers.stop().await;

    // The same store as an older daemon wrote it, without any launch
    // checkout, migrates to the current schema and keeps launching, with no
    // arguments and one failure per resume.
    let older: Vec<String> = store
        .lines()
        .map(|line| match serde_json::from_str::<Value>(line) {
            Ok(mut record) if record["kind"] == "session" => {
                record["recovery"]
                    .as_object_mut()
                    .expect("a recovery binding")
                    .remove("launch_checkout");
                record["schema_version"] = json!(15);
                record.to_string()
            }
            _ => line.to_owned(),
        })
        .collect();
    let migrated = base.path().join("migrated");
    std::fs::create_dir(&migrated).expect("create the migration directory");
    let migrated_store = write_data_store(&migrated, &older.join("\n"));
    let outcome = pohunek_daemon::store::Store::new(migrated_store.clone())
        .migrate_to_current()
        .expect("the older store migrates");
    let pohunek_daemon::store::SchemaMigration::Migrated { from, backup, .. } = outcome else {
        panic!("a schema-15 store must migrate");
    };
    assert_eq!(from, 15);
    assert!(
        backup.ends_with("metadata.jsonl.pre-schema-15"),
        "{backup:?}"
    );
    let migrated_text = std::fs::read_to_string(&migrated_store).expect("read the migrated store");
    assert!(recorded_launch_checkout(&migrated_text, &created).is_none());
    let servers =
        spawn_servers_configured("pkg-trust-migrated", Some(&migrated_text), trusting).await;
    let mut local = connect_unix(&servers.socket).await;
    install_drifting_runtime(&servers, &mut local, &program).await;
    servers
        .registry
        .reconcile_workers()
        .await
        .expect("startup reconciles the stored sessions");
    announce_remote_source(&servers, Some(remote_snapshot(&anchor)));
    let before = failure_lines(&log, &anchor);
    let resumed: protocol::SessionResumeResult =
        call(&mut local, method::SESSION_RESUME, json!(created)).await;
    let argv = printed_argv(&mut local, &resumed.session.id).await;
    assert_eq!(trust_arg_count(&argv), 0, "{argv:?}");
    assert_eq!(failure_lines(&log, &anchor), before + 1);
    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(created)).await;
    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_runtime_without_trusted_args_records_the_launch_checkout_and_logs_nothing() {
    let log = DaemonLog::start();
    let servers = spawn_servers_configured("pkg-trust-plain", Some(""), |config| {
        config.trusted_repositories = host_trusting(&["acme/widgets"]);
    })
    .await;
    let mut local = connect_unix(&servers.socket).await;
    let program = argv_program(&servers.plugins);
    let document = checkout_trust_descriptor(&program, "", "");
    let (_digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "agent-plain",
        &document,
        false,
    )
    .await;
    installed.expect("the package installs");
    let root = servers.plugins.parent().expect("the socket directory");
    let dir = checkout(root, "plain", "https://github.com/acme/widgets.git");
    announce_remote_source(&servers, Some(remote_snapshot(&dir)));

    // Row 14: no arguments and no failure line, but the pair is recorded so a
    // later descriptor that declares the keys works for this session.
    let created = start_named_session(&mut local, RUNTIME, &dir, None).await;
    assert_eq!(
        trust_arg_count(&printed_argv(&mut local, &created).await),
        0
    );
    assert_eq!(
        trust_arg_count(&resumed_argv(&mut local, &created).await),
        0
    );
    assert_eq!(
        recorded_launch_checkout(&store_text(&servers), &created)
            .expect("the store records the launch checkout")["cwd"],
        json!(dir)
    );
    assert_eq!(failure_lines(&log, &dir), 0);

    let _: protocol::SessionStopResult =
        call(&mut local, method::SESSION_STOP, json!(created)).await;
    drop(local);
    servers.stop().await;
}
