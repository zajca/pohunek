//! Integration test: the `package.*` lifecycle methods over the real control
//! servers.
//!
//! A Unix `ControlServer` and a loopback-TCP `RemoteServer` (the stand-in for
//! the overlay connection) serve one shared registry over a real plugin root.
//! The Unix socket serves the whole lifecycle; every package method is refused
//! on the remote connection before it reads a parameter or touches the disk.

// Rust guideline compliant 2026-10-04

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
    vec![
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: descriptor(program).into_bytes(),
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
    let bytes = build_archive(&entries(program), &Limits::DEFAULT).expect("archive builds");
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
