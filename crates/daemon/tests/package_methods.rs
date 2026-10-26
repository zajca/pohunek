//! Integration test: the `package.*` lifecycle methods over the real control
//! servers.
//!
//! A Unix `ControlServer` and a loopback-TCP `RemoteServer` (the stand-in for
//! the overlay connection) serve one shared registry over a real plugin root.
//! The Unix socket serves the whole lifecycle; every package method is refused
//! on the remote connection before it reads a parameter or touches the disk.

// Rust guideline compliant 2026-10-12

mod support;

use std::net::SocketAddr;
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
    plugins: PathBuf,
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
    let socket_dir = pohunek_test_support::tempdir_with_prefix(&format!("ph-{tag}-"))
        .expect("create the socket directory");
    let socket = socket_dir.path().join("daemon.sock");
    let plugins = socket_dir.path().join("plugins");
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
    let config = SessionRegistryConfig {
        socket_path: Some(socket.clone()),
        plugins_dir: Some(plugins.clone()),
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
        let unix_serve = unix.serve(async move {
            let _ = unix_rx.await;
        });
        let remote_serve = remote.serve(async move {
            let _ = remote_rx.await;
        });
        tokio::join!(unix_serve, remote_serve);
    });
    Servers {
        addr,
        socket,
        plugins,
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

/// The wire error code of a refused call.
async fn refusal<S>(framed: &mut Framed<S, LinesCodec>, method: &str, params: Value) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    exchange(framed, method, params)
        .await
        .into_result()
        .expect_err("the call is refused")
        .code
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
