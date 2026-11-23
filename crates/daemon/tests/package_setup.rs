//! Integration test: `[setup.<name>]` entries of runtime packages and the
//! `package.setup_resolve` method, plus `package.create_profile` with
//! `program`, over the real control servers.
//!
//! A Unix `ControlServer` and a loopback-TCP `RemoteServer` serve one shared
//! registry over a real plugin root. The fixture package declares one setup
//! entry whose program records that it ran, so every scenario can tell a
//! resolution from a run. The daemon must never run it.

// Rust guideline compliant 2026-11-23

mod support;

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use protocol::{
    method, PackageCreateProfileResult, PackageCreateProfileStatus, PackageInspectResult,
    PackageInstallResult, ProtocolError, Request as ProtocolRequest, Response,
    SetupResolveResult,
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

/// Id of the fixture package.
const PACKAGE: &str = "acme.runtime.pi";

/// Runtime id the fixture package serves.
const RUNTIME: &str = "pi";

/// The setup entry the fixture exports.
const SETUP: &str = "accounts";

/// Package-relative path of the entry's program.
const PROGRAM: &str = "bin/tool";

/// Fixed argument the entry carries before the owner's arguments.
const FIXED_ARG: &str = "fixed";

/// One-line description of the entry, shown in the install review.
const DESCRIPTION: &str = "Manage the account list";

/// Name of the file the fixture program writes if it ever runs.
const RAN_MARKER: &str = "ran";

/// Feature a descriptor must require to export `[setup]`.
const SETUP_FEATURE: &str = "\"package-setup-v1\"";

/// Detection manifest the fixture runtime ships.
const DETECT_MANIFEST: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

/// Code of a setup entry the descriptor does not declare.
const SETUP_NOT_DECLARED: &str = "command_setup_not_declared";

/// Code of a setup request on a transport other than the local socket.
const SETUP_NOT_LOCAL: &str = "command_setup_not_local";

/// Code of a package whose root no longer matches its digest.
const UNAVAILABLE: &str = "command_unavailable";

/// Code of a package version that is not selected.
const NOT_SELECTED: &str = "command_not_selected";

/// Code of a disabled package version.
const PACKAGE_DISABLED: &str = "command_package_disabled";

/// Code of a package that declares commands or third-party setup entries to a
/// client that did not opt in.
const COMMANDS_CLIENT_UNSUPPORTED: &str = "package_commands_client_unsupported";

/// Code of a descriptor that requires a feature this core lacks, or uses a key
/// its feature does not allow.
const FEATURE_UNSUPPORTED: &str = "runtime_feature_unsupported";

/// Code of a descriptor the parser refuses.
const DESCRIPTOR_INVALID: &str = "package_descriptor_invalid";

/// Code of a profile program the daemon refuses.
const PROGRAM_INVALID: &str = "package_profile_program_invalid";

/// Base runtime a profile is created over: a built-in, so no package pin.
const BUILTIN_BASE: &str = "shell";

/// Name of the profile the create scenarios write.
const PROFILE: &str = "work";

struct Servers {
    addr: SocketAddr,
    socket: PathBuf,
    plugins: PathBuf,
    /// Directory the fixture program reports into.
    work: PathBuf,
    /// Directory the daemon writes profile files into.
    agents: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    _registry: SessionRegistry,
    _state: DaemonState,
    _launch_env: pohunek_test_support::env::TestEnv,
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
    let work = socket_dir.path().join("work");
    std::fs::create_dir(&work).expect("create the fixture work directory");
    let agents = socket_dir.path().join("agents");
    std::fs::create_dir(&agents).expect("create the agents directory");
    std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700))
        .expect("make the agents directory owner-private");
    let host_state = socket_dir.path().join("host-state");
    std::fs::create_dir(&host_state).expect("create the host-state directory");
    std::fs::set_permissions(&host_state, std::fs::Permissions::from_mode(0o700))
        .expect("make the host-state directory owner-private");
    let worker_home_dir =
        pohunek_test_support::tempdir_with_prefix("pw-s-").expect("create the worker home");
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
    let launch_env =
        pohunek_test_support::env::TestEnv::new().expect("create the launch environment");
    let config = SessionRegistryConfig {
        socket_path: Some(socket.clone()),
        plugins_dir: Some(plugins.clone()),
        agents_dir: Some(agents.clone()),
        host_state_dir: Some(host_state),
        worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
        worker_state_root: Some(environment.state_home.join("pohunek/workers")),
        supervision: Some(
            environment
                .supervision(worker_binary())
                .with_environment_source(pohunek_daemon::runtime::EnvironmentSource::fixed(
                    launch_env.environment().clone(),
                )),
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
    let remote = RemoteServer::from_listener(listener, state.clone());
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
        plugins,
        work,
        agents,
        shutdown,
        task,
        _registry: registry,
        _state: state,
        _launch_env: launch_env,
        _dirs: (socket_dir, worker_home_dir),
    }
}

async fn connect_tcp(addr: SocketAddr) -> Framed<TcpStream, LinesCodec> {
    let stream = TcpStream::connect(addr)
        .await
        .expect("connect to the bound test tcp listener");
    Framed::new(stream, LinesCodec::new())
}

async fn connect_unix(socket: &Path) -> Framed<UnixStream, LinesCodec> {
    let stream = UnixStream::connect(socket)
        .await
        .expect("connect to the bound test socket");
    Framed::new(stream, LinesCodec::new())
}

async fn exchange<S>(framed: &mut Framed<S, LinesCodec>, method: &str, params: Value) -> Response
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request = ProtocolRequest::new("setup-test", method, params).expect("valid request");
    let line = serde_json::to_string(&request).expect("serialize request");
    framed.send(line).await.expect("send");
    let reply = framed
        .next()
        .await
        .expect("a response line")
        .expect("response framing ok");
    serde_json::from_str(&reply).expect("parse response")
}

/// Calls `method` and decodes its result, panicking on an error reply.
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

/// Calls `method` and returns the error it is refused with.
async fn refused<S>(framed: &mut Framed<S, LinesCodec>, method: &str, params: Value) -> ProtocolError
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    exchange(framed, method, params)
        .await
        .into_result()
        .expect_err("the call is refused")
}

/// Resolves `setup` of `package` through `package.setup_resolve`.
async fn resolve<S>(
    framed: &mut Framed<S, LinesCodec>,
    package: &str,
    setup: &str,
) -> Result<SetupResolveResult, ProtocolError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    exchange(
        framed,
        method::PACKAGE_SETUP_RESOLVE,
        json!({ "package": package, "setup": setup }),
    )
    .await
    .into_result()
    .map(|value| serde_json::from_value(value).expect("setup.resolve result"))
}

/// The descriptor of the fixture: the runtime export with the given `requires`
/// entries and `tables` appended.
fn descriptor(requires: &str, tables: &str) -> String {
    format!(
        r#"schema = 1
id = "{PACKAGE}"
version = "1.0.0"
runtime_api = 1
requires = [{requires}]

[runtime]
id = "{RUNTIME}"
name = "Pi"
program = "/bin/sh"
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
{tables}"#
    )
}

/// One more entry than a descriptor may export.
const TOO_MANY_ENTRIES: usize = 9;

/// `count` setup entries, each a valid table, for the entry-count limit.
fn many_entries(count: usize) -> String {
    let mut text = String::new();
    for index in 0..count {
        let _ = write!(
            text,
            "\n[setup.entry-{index}]\nprogram = \"{PROGRAM}\"\ndescription = \"entry\"\n"
        );
    }
    text
}

/// The `[setup.accounts]` table of the fixture.
fn setup_table() -> String {
    format!(
        "\n[setup.{SETUP}]\nprogram = \"{PROGRAM}\"\nargs = [\"{FIXED_ARG}\"]\ndescription = \"{DESCRIPTION}\"\n"
    )
}

/// The program the entry runs. It records that it ran, so a scenario can tell
/// a resolution from a run.
fn program_script(work: &Path) -> String {
    format!(
        "#!/bin/sh\n: > \"{}/{RAN_MARKER}\"\n",
        work.display()
    )
}

/// The archive entries of a package whose descriptor is `document`.
fn entries(document: String, work: &Path) -> Vec<ArchiveEntry> {
    vec![
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
        ArchiveEntry {
            path: PROGRAM.to_owned(),
            contents: program_script(work).into_bytes(),
            executable: true,
        },
    ]
}

/// The entries of the fixture package with the setup entry declared.
fn fixture_entries(work: &Path) -> Vec<ArchiveEntry> {
    entries(descriptor(SETUP_FEATURE, &setup_table()), work)
}

fn archive_of(entries: &[ArchiveEntry]) -> (Vec<u8>, PackageDigest) {
    let bytes = build_archive(entries, &Limits::DEFAULT).expect("archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("archive reads")
        .digest()
        .clone();
    (bytes, digest)
}

/// Installs `entries` through `package.install` and returns the wire result.
async fn try_install<S>(
    framed: &mut Framed<S, LinesCodec>,
    plugins: &Path,
    entries: &[ArchiveEntry],
    name: &str,
    enable: bool,
    select: bool,
    accept_commands: bool,
) -> Result<(PackageInstallResult, PackageDigest), ProtocolError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (bytes, digest) = archive_of(entries);
    let archive_path = plugins.with_file_name(format!("{name}.tar.zst"));
    std::fs::write(&archive_path, &bytes).expect("write the archive");
    exchange(
        framed,
        method::PACKAGE_INSTALL,
        json!({
            "archive_path": archive_path,
            "trust": { "kind": "explicit_digest", "digest": digest },
            "enable": enable,
            "select": select,
            "dry_run": false,
            "accept_commands": accept_commands
        }),
    )
    .await
    .into_result()
    .map(|value| {
        (
            serde_json::from_value(value).expect("package.install result"),
            digest,
        )
    })
}

/// Installs the fixture enabled and selected, with the opt-in the review needs.
async fn install_fixture<S>(
    framed: &mut Framed<S, LinesCodec>,
    servers: &Servers,
) -> PackageDigest
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (_installed, digest) = try_install(
        framed,
        &servers.plugins,
        &fixture_entries(&servers.work),
        "setup-fixture",
        true,
        true,
        true,
    )
    .await
    .expect("the fixture installs");
    digest
}

fn ran(work: &Path) -> bool {
    work.join(RAN_MARKER).exists()
}

/// File names in `dir`, sorted, so a scenario can compare before and after.
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read the agents directory")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Connection tasks whose shutdown token nothing cancels.
fn untracked(name: &'static str) -> pohunek_daemon::api::ConnectionTasks {
    pohunek_daemon::api::ConnectionTasks::new(name, &tokio_util::sync::CancellationToken::new())
}

#[tokio::test]
async fn a_setup_entry_is_listed_and_resolves_to_its_verified_program_without_running_it() {
    let servers = spawn_servers("setup-ok").await;
    let mut local = connect_unix(&servers.socket).await;
    let (installed, digest) = try_install(
        &mut local,
        &servers.plugins,
        &fixture_entries(&servers.work),
        "setup-fixture",
        true,
        true,
        true,
    )
    .await
    .expect("the fixture installs");
    assert_eq!(installed.setup.len(), 1, "the install lists the entry");
    assert_eq!(installed.setup[0].name.as_str(), SETUP);
    assert_eq!(installed.setup[0].program, PROGRAM);
    assert_eq!(installed.setup[0].args, vec![FIXED_ARG.to_owned()]);
    assert_eq!(installed.setup[0].description, DESCRIPTION);

    let inspected: PackageInspectResult = call(
        &mut local,
        method::PACKAGE_INSPECT,
        json!({ "digest": digest }),
    )
    .await;
    assert_eq!(
        inspected.setup, installed.setup,
        "inspect lists the same entry"
    );

    let resolved = resolve(&mut local, PACKAGE, SETUP)
        .await
        .expect("the selected package resolves its entry");
    let root = package::registry::files_path(&servers.plugins, &digest);
    assert_eq!(resolved.digest, digest);
    assert_eq!(resolved.package.id.as_str(), PACKAGE);
    assert_eq!(resolved.setup.as_str(), SETUP);
    assert_eq!(resolved.root, root.to_str().expect("utf-8 root"));
    assert_eq!(
        resolved.program,
        root.join(PROGRAM).to_str().expect("utf-8 program")
    );
    assert_eq!(resolved.args, vec![FIXED_ARG.to_owned()]);
    assert!(
        !ran(&servers.work),
        "resolving an entry never runs its program"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_setup_request_is_refused_on_the_remote_connection() {
    let servers = spawn_servers("setup-remote").await;
    let mut local = connect_unix(&servers.socket).await;
    let mut remote = connect_tcp(servers.addr).await;
    install_fixture(&mut local, &servers).await;

    let error = refused(
        &mut remote,
        method::PACKAGE_SETUP_RESOLVE,
        json!({ "package": PACKAGE, "setup": SETUP }),
    )
    .await;
    assert_eq!(error.code, SETUP_NOT_LOCAL);
    assert!(!ran(&servers.work), "nothing ran");

    drop((local, remote));
    servers.stop().await;
}

#[tokio::test]
async fn an_undeclared_disabled_unselected_or_tampered_setup_entry_is_not_resolved() {
    let servers = spawn_servers("setup-refuse").await;
    let mut local = connect_unix(&servers.socket).await;
    let digest = install_fixture(&mut local, &servers).await;

    let undeclared = refused(
        &mut local,
        method::PACKAGE_SETUP_RESOLVE,
        json!({ "package": PACKAGE, "setup": "missing" }),
    )
    .await;
    assert_eq!(undeclared.code, SETUP_NOT_DECLARED);

    let disabled: Value = call(
        &mut local,
        method::PACKAGE_SET_ENABLED,
        json!({ "digest": digest, "enabled": false }),
    )
    .await;
    assert!(disabled.is_object(), "the package is disabled");
    let error = refused(
        &mut local,
        method::PACKAGE_SETUP_RESOLVE,
        json!({ "package": PACKAGE, "setup": SETUP }),
    )
    .await;
    assert_eq!(error.code, PACKAGE_DISABLED);
    let _reenabled: Value = call(
        &mut local,
        method::PACKAGE_SET_ENABLED,
        json!({ "digest": digest, "enabled": true }),
    )
    .await;

    let script = package::registry::files_path(&servers.plugins, &digest).join(PROGRAM);
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .expect("make the program writable");
    let mut tampered = program_script(&servers.work).into_bytes();
    tampered.extend_from_slice(b"# tampered\n");
    std::fs::write(&script, tampered).expect("modify the program");
    let error = refused(
        &mut local,
        method::PACKAGE_SETUP_RESOLVE,
        json!({ "package": PACKAGE, "setup": SETUP }),
    )
    .await;
    assert_eq!(error.code, UNAVAILABLE, "a tampered root resolves nothing");
    let inspected: PackageInspectResult = call(
        &mut local,
        method::PACKAGE_INSPECT,
        json!({ "digest": digest }),
    )
    .await;
    assert!(
        inspected.setup.is_empty(),
        "a package whose root fails verification lists no setup entries"
    );
    assert!(!ran(&servers.work), "no refused resolution ran the program");

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn an_unselected_setup_entry_is_not_resolved() {
    let servers = spawn_servers("setup-unselected").await;
    let mut local = connect_unix(&servers.socket).await;
    let (_installed, _digest) = try_install(
        &mut local,
        &servers.plugins,
        &fixture_entries(&servers.work),
        "setup-fixture",
        true,
        false,
        true,
    )
    .await
    .expect("the fixture installs unselected");
    let error = refused(
        &mut local,
        method::PACKAGE_SETUP_RESOLVE,
        json!({ "package": PACKAGE, "setup": SETUP }),
    )
    .await;
    assert_eq!(error.code, NOT_SELECTED);

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_third_party_setup_entry_needs_the_commands_opt_in() {
    let servers = spawn_servers("setup-optin").await;
    let mut local = connect_unix(&servers.socket).await;
    let refused_install = try_install(
        &mut local,
        &servers.plugins,
        &fixture_entries(&servers.work),
        "setup-fixture",
        true,
        true,
        false,
    )
    .await
    .expect_err("an install without the opt-in is refused");
    assert_eq!(refused_install.code, COMMANDS_CLIENT_UNSUPPORTED);

    let (installed, _digest) = try_install(
        &mut local,
        &servers.plugins,
        &fixture_entries(&servers.work),
        "setup-fixture",
        true,
        true,
        true,
    )
    .await
    .expect("the install with the opt-in succeeds");
    assert_eq!(installed.setup.len(), 1);

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_descriptor_that_misuses_the_setup_table_is_refused_at_install() {
    let servers = spawn_servers("setup-bad").await;
    let mut local = connect_unix(&servers.socket).await;
    let setup_only = format!(
        "schema = 1\nid = \"{PACKAGE}\"\nversion = \"1.0.0\"\nruntime_api = 1\nrequires = [{SETUP_FEATURE}]\n{}",
        setup_table()
    );
    let cases: [(&str, String, &str); 6] = [
        (
            "without the feature in requires",
            descriptor("", &setup_table()),
            FEATURE_UNSUPPORTED,
        ),
        ("a setup entry without a runtime", setup_only, DESCRIPTOR_INVALID),
        (
            "an unknown key in the entry",
            descriptor(
                SETUP_FEATURE,
                &format!("{}extra = true\n", setup_table()),
            ),
            DESCRIPTOR_INVALID,
        ),
        (
            "a description with a control character",
            descriptor(
                SETUP_FEATURE,
                &setup_table().replace(DESCRIPTION, "two\\tlines"),
            ),
            DESCRIPTOR_INVALID,
        ),
        (
            "a program that is not an executable of the package",
            descriptor(
                SETUP_FEATURE,
                &setup_table().replace(PROGRAM, "bin/absent"),
            ),
            DESCRIPTOR_INVALID,
        ),
        (
            "more entries than the limit",
            descriptor(SETUP_FEATURE, &many_entries(TOO_MANY_ENTRIES)),
            DESCRIPTOR_INVALID,
        ),
    ];
    for (what, document, code) in cases {
        let error = try_install(
            &mut local,
            &servers.plugins,
            &entries(document, &servers.work),
            "setup-bad",
            true,
            true,
            true,
        )
        .await
        .expect_err(what);
        assert_eq!(error.code, code, "{what}");
    }
    let listed: Value = call(&mut local, method::PACKAGE_LIST, json!({})).await;
    assert_eq!(
        listed["packages"].as_array().map(Vec::len),
        Some(0),
        "a refused descriptor records nothing"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_profile_program_creates_a_profile_and_a_refused_one_writes_nothing() {
    let servers = spawn_servers("setup-program").await;
    let mut local = connect_unix(&servers.socket).await;
    let tool = servers.work.join("tool");
    std::fs::write(&tool, "#!/bin/sh\n").expect("write the program");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
        .expect("make the program executable");
    let before = listing(&servers.agents);

    let created: PackageCreateProfileResult = call(
        &mut local,
        method::PACKAGE_CREATE_PROFILE,
        json!({
            "profile": PROFILE,
            "base": BUILTIN_BASE,
            "program": tool,
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(created.status, PackageCreateProfileStatus::Preview);
    assert!(
        created
            .text
            .contains(&format!("program = \"{}\"", tool.display())),
        "the composed profile names the program: {}",
        created.text
    );
    assert_eq!(
        listing(&servers.agents),
        before,
        "a dry run writes nothing"
    );

    let refusals: Vec<(&str, PathBuf)> = {
        let relative = PathBuf::from("bin/tool");
        let writable = servers.work.join("writable");
        std::fs::write(&writable, "#!/bin/sh\n").expect("write a writable program");
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o757))
            .expect("make it world-writable");
        let link = servers.work.join("link");
        std::os::unix::fs::symlink(&tool, &link).expect("symlink the program");
        let plain = servers.work.join("plain");
        std::fs::write(&plain, "data\n").expect("write a non-executable file");
        vec![
            ("a relative path", relative),
            ("a world-writable file", writable),
            ("a symbolic link", link),
            ("a file that is not executable", plain),
            ("a missing file", servers.work.join("absent")),
        ]
    };
    for (what, program) in refusals {
        let error = refused(
            &mut local,
            method::PACKAGE_CREATE_PROFILE,
            json!({
                "profile": PROFILE,
                "base": BUILTIN_BASE,
                "program": program,
                "dry_run": false,
            }),
        )
        .await;
        assert_eq!(error.code, PROGRAM_INVALID, "{what}");
        assert_eq!(
            listing(&servers.agents),
            before,
            "{what}: nothing was written"
        );
    }

    let created: PackageCreateProfileResult = call(
        &mut local,
        method::PACKAGE_CREATE_PROFILE,
        json!({
            "profile": PROFILE,
            "base": BUILTIN_BASE,
            "program": tool,
            "dry_run": false,
        }),
    )
    .await;
    assert_eq!(
        created.status,
        PackageCreateProfileStatus::Created,
        "the name was never written by a refusal"
    );
    assert!(listing(&servers.agents).len() > before.len());

    drop(local);
    servers.stop().await;
}
