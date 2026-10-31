//! Integration test: the `package.*` lifecycle methods over the real control
//! servers.
//!
//! A Unix `ControlServer` and a loopback-TCP `RemoteServer` (the stand-in for
//! the overlay connection) serve one shared registry over a real plugin root.
//! The Unix socket serves the whole lifecycle; every package method is refused
//! on the remote connection before it reads a parameter or touches the disk.
//! The same servers, over a registry that owns a metadata store, serve the
//! `store.rejected_*` methods and the package gates that depend on them.

// Rust guideline compliant 2026-10-12

mod support;

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
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
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
    let config = SessionRegistryConfig {
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

/// The `store_rejected_line_live` message names a `resume` line only for the
/// fixture's unreadable-pin `resume` line, not for a possible session record.
async fn assert_live_refusal_names_its_line_kind<S>(
    local: &mut Framed<S, LinesCodec>,
    fixture: &RejectedFixture,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let resume_refusal = refused(
        local,
        method::STORE_REJECTED_SET_ASIDE,
        json!({ "content_hash": sha256_hex(&fixture.resume) }),
    )
    .await;
    assert!(
        resume_refusal.msg.contains("`resume` line"),
        "{resume_refusal:?}"
    );
    let session_refusal = refused(
        local,
        method::STORE_REJECTED_SET_ASIDE,
        json!({ "content_hash": sha256_hex(&fixture.live) }),
    )
    .await;
    assert!(
        !session_refusal.msg.contains("`resume` line"),
        "{session_refusal:?}"
    );
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
    vec![
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: descriptor.as_bytes().to_vec(),
            executable: false,
        },
        ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: DETECT_MANIFEST.as_bytes().to_vec(),
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
    let (bytes, digest) = archive_of(&entries_of(document));
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
    let params = protocol::SessionNewParams {
        name: None,
        agent: agent.to_owned(),
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

#[tokio::test]
async fn launch_env_pairs_reach_new_resumed_and_forked_sessions_and_a_profile_overrides_them() {
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

    // A fork is launched the same way.
    let forked: protocol::SessionForkResult = call(
        &mut local,
        method::SESSION_FORK,
        json!({ "session_id": resumed.session.id, "cols": 80, "rows": 24 }),
    )
    .await;
    wait_for_text(&mut local, &forked.session.id, "LAUNCH_ENV_PAIR=<1>").await;
    for stopped in [&resumed.session.id, &forked.session.id] {
        let _: protocol::SessionStopResult =
            call(&mut local, method::SESSION_STOP, json!(stopped)).await;
    }

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
async fn a_fork_assignment_installs_only_next_to_an_assigned_fork_and_is_shown_for_review() {
    let servers = spawn_servers("pkg-assign-fork").await;
    let mut local = connect_unix(&servers.socket).await;
    let plain = descriptor("/bin/sh");
    let launch_args = "launch_args = [\"--session-id\", \"{reference}\"]\n";
    let fork = "[fork]\nsupported = true\nargs = [\"--fork\", \"{reference}\"]\n";
    let assigned = format!("strategy = \"assigned\"\n{launch_args}");
    assert!(plain.contains(fork) && plain.contains(&assigned));
    let assign_fork = |value: &str| {
        plain.replace(
            launch_args,
            &format!("{launch_args}assign_fork = {value}\n"),
        )
    };
    let assigning = assign_fork("true");
    let without_fork = |document: &str| document.replace(fork, "[fork]\nsupported = false\n");
    let hook = |line: &str| plain.replace(&assigned, &format!("strategy = \"hook\"\n{line}"));
    // Each refused descriptor next to the same descriptor without the
    // offending `assign_fork`, which a dry run accepts.
    let refused = [
        (
            "without-fork",
            without_fork(&assigning),
            without_fork(&plain),
        ),
        ("hook-strategy", hook("assign_fork = false\n"), hook("")),
        ("not-a-boolean", assign_fork("\"yes\""), plain.clone()),
    ];
    for (name, document, control) in &refused {
        let (_digest, result) =
            install_descriptor(&mut local, &servers.plugins, name, document, false).await;
        let error = result.expect_err(name);
        assert_eq!(error.code, "package_descriptor_invalid", "{name}");
        let (_digest, result) =
            install_descriptor(&mut local, &servers.plugins, name, control, true).await;
        result.unwrap_or_else(|error| panic!("{name} without assign_fork is valid: {error:?}"));
    }
    let listed: PackageListResult = call(&mut local, method::PACKAGE_LIST, Value::Null).await;
    assert!(
        listed.packages.is_empty(),
        "a refused descriptor or a dry run records nothing"
    );

    // Review shows what a fork appends after its own arguments.
    let (digest, installed) = install_descriptor(
        &mut local,
        &servers.plugins,
        "assign-fork",
        &assigning,
        false,
    )
    .await;
    installed.expect("a fork assignment next to an assigned fork installs");
    let inspected: PackageInspectResult = call(
        &mut local,
        method::PACKAGE_INSPECT,
        json!({ "digest": digest }),
    )
    .await;
    let runtime = inspected.runtime.expect("descriptor");
    let tokens = |tokens: &[&str]| Some(tokens.iter().map(|token| (*token).to_owned()).collect());
    assert_eq!(runtime.fork_args, tokens(&["--fork", "{reference}"]));
    assert_eq!(
        runtime.fork_launch_args,
        tokens(&["--session-id", "{reference}"])
    );
    assert_eq!(runtime.launch_args, runtime.fork_launch_args);

    drop(local);
    servers.stop().await;
}

/// A host profile slot equal to a `[launch_env]` name of its runtime cannot be
/// filled by a request: the pair is the runtime's behaviour flag, not a secret.
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

/// A current-schema session line that no loader accepts, with `runtime`.
fn rejected_session_line(session_id: &str, runtime: Value) -> String {
    let mut line = json!({
        "kind": "session",
        "schema_version": pohunek_daemon::store::STORE_SCHEMA_VERSION,
        "session_id": session_id,
        "desired_state": "suspended"
    });
    if !runtime.is_null() {
        line["runtime"] = runtime;
    }
    line.to_string()
}

/// Lowercase hex SHA-256 of `text`.
fn sha256_hex(text: &str) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn quarantine_records(servers: &Servers) -> Vec<Value> {
    let path = servers
        .store
        .as_ref()
        .expect("the registry owns a store")
        .with_file_name("metadata.rejected.jsonl");
    std::fs::read_to_string(path)
        .expect("read the quarantine file")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a quarantine record"))
        .collect()
}

fn store_text(servers: &Servers) -> String {
    std::fs::read_to_string(servers.store.as_ref().expect("the registry owns a store"))
        .expect("read the metadata store")
}

/// The rejected lines of the scenarios below, numbered as the store holds them
/// (a blank line sits at 2, a duplicate of the resume line at 7).
struct RejectedFixture {
    project: String,
    resume: String,
    live: String,
    ended: String,
    corrupt: String,
    content: String,
}

impl RejectedFixture {
    fn new() -> Self {
        let project = rejected_line("project");
        let resume = rejected_line("resume");
        let live = rejected_session_line("s-live", Value::Null);
        let ended = rejected_session_line("s-ended", json!({ "state": "terminal" }));
        let corrupt = "{not json".to_owned();
        let content = format!("{project}\n\n{resume}\n{live}\n{ended}\n{corrupt}\n{resume}\n");
        Self {
            project,
            resume,
            live,
            ended,
            corrupt,
            content,
        }
    }
}

async fn set_aside(
    local: &mut Framed<UnixStream, LinesCodec>,
    text: &str,
    accept_pin_loss: bool,
) -> protocol::StoreRejectedSetAsideResult {
    call(
        local,
        method::STORE_REJECTED_SET_ASIDE,
        json!({ "content_hash": sha256_hex(text), "accept_pin_loss": accept_pin_loss }),
    )
    .await
}

#[tokio::test]
async fn the_rejected_line_methods_are_local_only_and_list_every_line() {
    let fixture = RejectedFixture::new();
    let servers = spawn_servers_over("store-rejected-list", Some(&fixture.content)).await;
    let mut local = connect_unix(&servers.socket).await;
    let mut remote = connect_tcp(servers.addr).await;

    for (name, params) in [
        (method::STORE_REJECTED_LIST, Value::Null),
        (
            method::STORE_REJECTED_SET_ASIDE,
            json!({ "content_hash": sha256_hex(&fixture.project) }),
        ),
    ] {
        assert_eq!(
            refusal(&mut remote, name, params).await,
            LOCAL_ONLY,
            "{name}"
        );
    }
    assert_eq!(
        store_text(&servers),
        fixture.content,
        "the remote calls changed nothing"
    );

    let listed: protocol::StoreRejectedListResult =
        call(&mut local, method::STORE_REJECTED_LIST, Value::Null).await;
    assert_eq!(listed.total, 6);
    let summary: Vec<_> = listed
        .lines
        .iter()
        .map(|line| {
            (
                line.line,
                line.kind.as_deref(),
                line.may_pin,
                line.may_own_runtime,
                line.content_hash.clone(),
            )
        })
        .collect();
    let expected = [
        (
            1,
            Some("project"),
            false,
            false,
            sha256_hex(&fixture.project),
        ),
        (3, Some("resume"), true, true, sha256_hex(&fixture.resume)),
        (4, Some("session"), true, true, sha256_hex(&fixture.live)),
        (5, Some("session"), true, false, sha256_hex(&fixture.ended)),
        (6, None, true, true, sha256_hex(&fixture.corrupt)),
        (7, Some("resume"), true, true, sha256_hex(&fixture.resume)),
    ];
    assert_eq!(summary, expected);
    assert!(
        listed.lines.iter().all(|line| !line.reason.is_empty()),
        "every line names why it is rejected"
    );

    assert_eq!(
        refusal(
            &mut local,
            method::STORE_REJECTED_SET_ASIDE,
            json!({ "content_hash": sha256_hex(&fixture.live) })
        )
        .await,
        "store_rejected_line_live"
    );
    assert_eq!(
        refusal(
            &mut local,
            method::STORE_REJECTED_SET_ASIDE,
            json!({ "content_hash": sha256_hex(&fixture.resume) })
        )
        .await,
        "store_rejected_line_live",
        "a resume line whose pin cannot be read needs the same consent"
    );
    assert_live_refusal_names_its_line_kind(&mut local, &fixture).await;
    assert_eq!(
        refusal(
            &mut local,
            method::STORE_REJECTED_SET_ASIDE,
            json!({ "content_hash": "0".repeat(64) })
        )
        .await,
        "store_rejected_line_not_found"
    );
    assert_eq!(
        store_text(&servers),
        fixture.content,
        "a refusal changes nothing"
    );
    assert!(
        !quarantine_path(&servers).exists(),
        "a refusal writes no quarantine file"
    );

    drop(remote);
    drop(local);
    servers.stop().await;
}

fn quarantine_path(servers: &Servers) -> PathBuf {
    servers
        .store
        .as_ref()
        .expect("the registry owns a store")
        .with_file_name("metadata.rejected.jsonl")
}

#[tokio::test]
async fn setting_lines_aside_moves_them_into_an_audited_owner_private_quarantine_file() {
    let fixture = RejectedFixture::new();
    let servers = spawn_servers_over("store-rejected-move", Some(&fixture.content)).await;
    let mut local = connect_unix(&servers.socket).await;

    let moved = set_aside(&mut local, &fixture.project, false).await;
    assert_eq!(moved.lines, [1]);
    assert_eq!(moved.quarantine_file, "metadata.rejected.jsonl");
    // The first rewrite dropped the blank line, so the numbers moved up.
    let duplicate = set_aside(&mut local, &fixture.resume, true).await;
    assert_eq!(duplicate.lines, [1, 5], "identical lines move together");
    let ended = set_aside(&mut local, &fixture.ended, false).await;
    assert_eq!(ended.lines, [2]);
    let accepted = set_aside(&mut local, &fixture.live, true).await;
    assert_eq!(accepted.lines, [1], "the number is the one before the move");
    assert_eq!(
        store_text(&servers),
        format!("{}\n", fixture.corrupt),
        "the store keeps only the line nobody set aside"
    );

    let records = quarantine_records(&servers);
    let audited: Vec<_> = records
        .iter()
        .map(|record| {
            (
                record["line"].as_u64().expect("line"),
                record["text"].as_str().expect("text").to_owned(),
                record["accepted_pin_loss"].as_bool().expect("flag"),
            )
        })
        .collect();
    assert_eq!(
        audited,
        [
            (1, fixture.project.clone(), false),
            (1, fixture.resume.clone(), true),
            (5, fixture.resume.clone(), true),
            (2, fixture.ended.clone(), false),
            (1, fixture.live.clone(), true),
        ]
    );
    assert!(records
        .iter()
        .all(|record| record["quarantine_schema"] == 1 && record["set_aside_at"].is_string()));
    let mode = std::fs::metadata(quarantine_path(&servers))
        .expect("quarantine metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "the quarantine file is owner-private");

    let after: protocol::StoreRejectedListResult =
        call(&mut local, method::STORE_REJECTED_LIST, Value::Null).await;
    assert_eq!(after.total, 1);
    assert_eq!(after.lines[0].line, 1);

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_removal_refused_for_an_unreadable_line_proceeds_once_the_line_is_set_aside() {
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
        refusal(&mut local, method::PACKAGE_UNINSTALL, uninstall.clone()).await,
        "package_retention_uncertain",
        "a line that may pin the package keeps it installed"
    );
    let listed: protocol::StoreRejectedListResult =
        call(&mut local, method::STORE_REJECTED_LIST, Value::Null).await;
    assert_eq!(listed.total, 1);
    assert!(listed.lines[0].may_pin);

    let _: protocol::StoreRejectedSetAsideResult = call(
        &mut local,
        method::STORE_REJECTED_SET_ASIDE,
        json!({ "content_hash": listed.lines[0].content_hash, "accept_pin_loss": true }),
    )
    .await;
    let removed: PackageUninstallResult =
        call(&mut local, method::PACKAGE_UNINSTALL, uninstall).await;
    assert!(removed.reloaded);

    drop(local);
    servers.stop().await;
}

/// Connection tasks whose shutdown token nothing cancels.
fn untracked(name: &'static str) -> pohunek_daemon::api::ConnectionTasks {
    pohunek_daemon::api::ConnectionTasks::new(name, &tokio_util::sync::CancellationToken::new())
}
