//! Integration test: `command.run` over the real control servers.
//!
//! A Unix `ControlServer` and a loopback-TCP `RemoteServer` (the stand-in for
//! the overlay connection) serve one shared registry over a real plugin root.
//! A fixture package declares commands that run one shell script; the script
//! reports what it observed into a directory the test owns, and blocks on
//! FIFOs the test releases, so no scenario waits on the clock.

// Rust guideline compliant 2026-10-28

mod support;

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use protocol::{
    method, CommandRunResult, PackageChangeResult, PackageInspectResult, PackageInstallResult,
    ProtocolError, Request as ProtocolRequest, Response, MAX_COMMAND_OUTPUT_BYTES,
};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::oneshot;
use tokio_util::codec::{Framed, LinesCodec};

use pohunek_daemon::api::{ControlServer, DaemonState, HealthInfo, RemoteServer};
use pohunek_daemon::governance::HostGovernanceService;
use pohunek_daemon::package_command::MAX_HOST_COMMAND_RUNS;
use pohunek_daemon::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use pohunek_test_support::worker_binary;

/// Id of the fixture package that declares the commands.
const PACKAGE: &str = "acme.fleet-state";

/// Id of a package that exports a runtime and no command.
const RUNTIME_PACKAGE: &str = "acme.runtime.pi";

/// Timeout, in milliseconds, of the scenarios that wait for a stop. It only
/// has to outlast the fixture's start-up, which the scenarios confirm by a
/// FIFO handshake before the deadline matters.
// timing-allowed: #1690 the command timeout is the behavior under test
const STOP_TIMEOUT_MS: u32 = 1_000;

/// The fixture program. `$1` is the test directory, `$2` the mode and `$3` the
/// command's tag; the modes are fixed in the descriptor's `args`.
const SCRIPT: &str = r#"#!/bin/sh
dir="$1"; mode="$2"; id="$3"
case "$mode" in
ok)
  : > "$dir/$id.ran"
  printf 'h\303\251llo\nsecond line\n'
  ;;
env)
  env | sed 's/=.*//' | sort > "$dir/env.names"
  pwd > "$dir/cwd"
  printf '%s' "$PATH" > "$dir/path"
  if [ -z "$(cat)" ]; then echo eof > "$dir/stdin"; else echo data > "$dir/stdin"; fi
  printf 'inspected'
  ;;
hold)
  echo $$ > "$dir/$id.pid"
  ( exec 3<>"$dir/$id.hold"; read x <&3 ) &
  echo $! > "$dir/$id.child"
  echo up > "$dir/$id.started"
  read x < "$dir/$id.release"
  printf 'released'
  ;;
big)
  head -c 2000000 /dev/zero | tr '\0' a
  : > "$dir/$id.after"
  ;;
exit)
  exit 3
  ;;
selfkill)
  kill -9 $$
  ;;
badutf8)
  printf '\377\376'
  ;;
background)
  ( exec 3<>"$dir/$id.hold"; read x <&3 ) &
  echo $! > "$dir/$id.child"
  printf 'done'
  ;;
esac
"#;

/// Variables an agent launch inherits (they pass the default allowlist) that a
/// command must not see.
const SENTINELS: [(&str, &str); 3] = [
    ("SSH_AUTH_SOCK", "/sentinel/agent.sock"),
    ("DISPLAY", ":sentinel"),
    ("USER", "sentinel-user"),
];

/// Commands of the fixture: name, mode, exposure.
const COMMANDS: [(&str, &str, &str); 14] = [
    ("ok", "ok", "peers"),
    ("ok-local", "ok", "local"),
    ("env", "env", "peers"),
    ("hold", "hold", "peers"),
    ("big", "big", "peers"),
    ("exit-three", "exit", "peers"),
    ("selfkill", "selfkill", "peers"),
    ("badutf8", "badutf8", "peers"),
    ("background", "background", "peers"),
    ("gate-a", "hold", "peers"),
    ("gate-b", "hold", "peers"),
    ("gate-c", "hold", "peers"),
    ("gate-d", "hold", "peers"),
    ("gate-e", "hold", "peers"),
];

struct Servers {
    addr: SocketAddr,
    socket: PathBuf,
    plugins: PathBuf,
    /// Directory the fixture reports into and blocks on.
    work: PathBuf,
    registry: SessionRegistry,
    state: DaemonState,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    /// The launch environment the daemon hands its agents; the sentinels in it
    /// must never reach a command.
    launch_env: pohunek_test_support::env::TestEnv,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Servers {
    async fn stop(self) {
        let _ = self.shutdown.send(());
        self.task.await.expect("servers stop");
    }
}

/// The launch environment of the daemon under test: the hermetic one plus the
/// [`SENTINELS`].
fn launch_environment() -> (
    pohunek_test_support::env::TestEnv,
    std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) {
    let launch_env =
        pohunek_test_support::env::TestEnv::new().expect("create the launch environment");
    let mut launch_vars = launch_env.environment().clone();
    for (name, value) in SENTINELS {
        launch_vars.insert(name.into(), value.into());
    }
    (launch_env, launch_vars)
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
        pohunek_test_support::tempdir_with_prefix("pw-c-").expect("create the worker home");
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
    let (launch_env, launch_vars) = launch_environment();
    let config = SessionRegistryConfig {
        socket_path: Some(socket.clone()),
        plugins_dir: Some(plugins.clone()),
        agents_dir: Some(agents),
        host_state_dir: Some(host_state),
        worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
        worker_state_root: Some(environment.state_home.join("pohunek/workers")),
        supervision: Some(
            environment
                .supervision(worker_binary())
                .with_environment_source(pohunek_daemon::runtime::EnvironmentSource::fixed(
                    launch_vars,
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
        work,
        registry,
        state,
        shutdown,
        task,
        launch_env,
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
    let request = ProtocolRequest::new("command-test", method, params).expect("valid request");
    let line = serde_json::to_string(&request).expect("serialize request");
    framed.send(line).await.expect("send");
    let reply = framed
        .next()
        .await
        .expect("a response line")
        .expect("response framing ok");
    serde_json::from_str(&reply).expect("parse response")
}

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

/// Runs `command` of `package` and returns the wire result.
async fn run_command<S>(
    framed: &mut Framed<S, LinesCodec>,
    package: &str,
    command: &str,
    extra: Value,
) -> Result<CommandRunResult, ProtocolError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut params = json!({ "package": package, "command": command });
    if let (Value::Object(base), Value::Object(extra)) = (&mut params, extra) {
        base.extend(extra);
    }
    exchange(framed, method::COMMAND_RUN, params)
        .await
        .into_result()
        .map(|value| serde_json::from_value(value).expect("command.run result"))
}

async fn expect_code<S>(
    framed: &mut Framed<S, LinesCodec>,
    package: &str,
    command: &str,
    extra: Value,
) -> ProtocolError
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    run_command(framed, package, command, extra)
        .await
        .expect_err("the run is refused")
}

fn descriptor(work: &Path) -> String {
    let mut text = format!(
        "schema = 1\nid = \"{PACKAGE}\"\nversion = \"1.0.0\"\nruntime_api = 1\nrequires = [\"package-commands-v1\"]\n"
    );
    for (name, mode, expose) in COMMANDS {
        let _ = write!(
            text,
            "\n[commands.{name}]\nprogram = \"bin/run.sh\"\nargs = [\"{}\", \"{mode}\", \"{name}\"]\nexpose = \"{expose}\"\n",
            work.display()
        );
    }
    text
}

fn archive_of(entries: &[ArchiveEntry]) -> (Vec<u8>, PackageDigest) {
    let bytes = build_archive(entries, &Limits::DEFAULT).expect("archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("archive reads")
        .digest()
        .clone();
    (bytes, digest)
}

fn command_entries(work: &Path) -> Vec<ArchiveEntry> {
    vec![
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: descriptor(work).into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "bin/run.sh".to_owned(),
            contents: SCRIPT.as_bytes().to_vec(),
            executable: true,
        },
    ]
}

/// Installs `entries` through `package.install`, enabled and selected as
/// asked, and returns the digest.
async fn install<S>(
    framed: &mut Framed<S, LinesCodec>,
    plugins: &Path,
    entries: &[ArchiveEntry],
    name: &str,
    enable: bool,
    select: bool,
) -> PackageDigest
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (bytes, digest) = archive_of(entries);
    let archive_path = plugins.with_file_name(format!("{name}.tar.zst"));
    std::fs::write(&archive_path, &bytes).expect("write the archive");
    let _installed: PackageInstallResult = call(
        framed,
        method::PACKAGE_INSTALL,
        json!({
            "archive_path": archive_path,
            "trust": { "kind": "explicit_digest", "digest": digest },
            "enable": enable,
            "select": select,
            "dry_run": false,
            "accept_commands": true
        }),
    )
    .await;
    digest
}

async fn install_fixture<S>(framed: &mut Framed<S, LinesCodec>, servers: &Servers) -> PackageDigest
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    install(
        framed,
        &servers.plugins,
        &command_entries(&servers.work),
        "fleet",
        true,
        true,
    )
    .await
}

/// Creates the FIFOs the `hold` mode of command `tag` blocks on.
fn make_fifos(work: &Path, tag: &str) {
    for suffix in ["hold", "release", "started"] {
        let path = work.join(format!("{tag}.{suffix}"));
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600))
            .unwrap_or_else(|error| panic!("mkfifo {}: {error}", path.display()));
    }
}

/// Blocks until the `hold` fixture of `tag` reports that it started, then
/// returns its pid and its background child's pid.
async fn started(work: &Path, tag: &str) -> (u32, u32) {
    let work = work.to_path_buf();
    let tag = tag.to_owned();
    tokio::task::spawn_blocking(move || {
        let _ = std::fs::read(work.join(format!("{tag}.started"))).expect("read the start FIFO");
        let pid = read_pid(&work.join(format!("{tag}.pid")));
        let child = read_pid(&work.join(format!("{tag}.child")));
        (pid, child)
    })
    .await
    .expect("start handshake")
}

/// Lets the blocked `hold` fixture of `tag` finish.
async fn release(work: &Path, tag: &str) {
    let path = work.join(format!("{tag}.release"));
    tokio::task::spawn_blocking(move || std::fs::write(path, b"go").expect("write the release"))
        .await
        .expect("release");
}

fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .trim()
        .parse()
        .expect("a pid")
}

/// Whether process `pid` still runs. A zombie awaiting its reaper counts as
/// gone.
fn is_running(pid: u32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

/// Yields until `pid` is gone; a killed process leaves in bounded time.
async fn assert_gone(pid: u32, what: &str) {
    for _ in 0..20_000 {
        if !is_running(pid) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{what} (pid {pid}) is still running");
}

#[tokio::test]
async fn a_command_returns_its_stdout_and_the_package_identity_on_both_transports() {
    let servers = spawn_servers("cmd-ok").await;
    let mut local = connect_unix(&servers.socket).await;
    let mut remote = connect_tcp(servers.addr).await;
    let digest = install_fixture(&mut local, &servers).await;

    let local_result = run_command(&mut local, PACKAGE, "ok", json!({}))
        .await
        .expect("local run");
    assert_eq!(local_result.output, "h\u{e9}llo\nsecond line\n");
    assert_eq!(local_result.package.id.as_str(), PACKAGE);
    assert_eq!(local_result.package.version.as_str(), "1.0.0");
    assert_eq!(local_result.command.as_str(), "ok");
    assert_eq!(local_result.digest, digest);

    let remote_result = run_command(&mut remote, PACKAGE, "ok", json!({}))
        .await
        .expect("remote run of a peers command");
    assert_eq!(remote_result.output, local_result.output);
    assert_eq!(remote_result.digest, digest);
    assert_eq!(remote_result.package, local_result.package);

    // A `local` command runs on the Unix socket and is refused over the
    // overlay before anything starts.
    let ran = servers.work.join("ok-local.ran");
    let refused = expect_code(&mut remote, PACKAGE, "ok-local", json!({})).await;
    assert_eq!(refused.code, "command_not_exposed");
    assert!(!ran.exists(), "the program never started");
    assert!(refused.details.is_none());
    run_command(&mut local, PACKAGE, "ok-local", json!({}))
        .await
        .expect("a local command runs on the Unix socket");
    assert!(ran.exists());

    drop((local, remote));
    servers.stop().await;
}

#[tokio::test]
async fn a_call_is_refused_in_the_resolution_order() {
    let servers = spawn_servers("cmd-order").await;
    let mut local = connect_unix(&servers.socket).await;
    let mut remote = connect_tcp(servers.addr).await;

    assert_eq!(
        expect_code(&mut local, PACKAGE, "ok", json!({})).await.code,
        "command_package_not_installed"
    );

    let digest = install(
        &mut local,
        &servers.plugins,
        &command_entries(&servers.work),
        "fleet",
        true,
        false,
    )
    .await;
    assert_eq!(
        expect_code(&mut local, PACKAGE, "ok", json!({})).await.code,
        "command_not_selected"
    );
    let _selected: PackageChangeResult = call(
        &mut local,
        method::PACKAGE_SELECT,
        json!({ "digest": digest }),
    )
    .await;

    assert_eq!(
        expect_code(&mut local, PACKAGE, "missing", json!({}))
            .await
            .code,
        "command_not_declared"
    );

    let _disabled: PackageChangeResult = call(
        &mut local,
        method::PACKAGE_SET_ENABLED,
        json!({ "digest": digest, "enabled": false }),
    )
    .await;
    let disabled = expect_code(&mut local, PACKAGE, "ok", json!({})).await;
    assert_eq!(disabled.code, "command_package_disabled");
    // A `local` command is refused to a peer before the enabled flag is read.
    assert_eq!(
        expect_code(&mut remote, PACKAGE, "ok-local", json!({}))
            .await
            .code,
        "command_not_exposed"
    );
    assert!(!servers.work.join("ok.ran").exists());

    // A package that exports a runtime and no command declares nothing.
    let runtime_descriptor = format!(
        "schema = 1\nid = \"{RUNTIME_PACKAGE}\"\nversion = \"1.0.0\"\nruntime_api = 1\n\n[runtime]\nid = \"pi\"\nname = \"Pi\"\nprogram = \"/bin/sh\"\nargs = []\ndetect_manifest = \"detect.toml\"\nprompt_arg = true\n\n[input]\nbracketed_paste = false\nsubmit_delay_ms = 0\ntext_policy = \"unrestricted\"\n\n[resume]\nsupported = false\n\n[fork]\nsupported = false\n\n[native_reference]\nstrategy = \"none\"\n"
    );
    let _runtime = install(
        &mut local,
        &servers.plugins,
        &[
            ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: runtime_descriptor.into_bytes(),
                executable: false,
            },
            ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: b"[[rules]]\nid = \"idle_prompt\"\nstate = \"idle\"\npriority = 100\nregion = \"whole_recent\"\nany = [{ contains = \"ready\" }]\n".to_vec(),
                executable: false,
            },
        ],
        "runtime",
        true,
        true,
    )
    .await;
    assert_eq!(
        expect_code(&mut local, RUNTIME_PACKAGE, "ok", json!({}))
            .await
            .code,
        "command_not_declared"
    );

    drop((local, remote));
    servers.stop().await;
}

#[tokio::test]
async fn a_run_past_its_timeout_is_stopped_with_its_whole_process_group() {
    let servers = spawn_servers("cmd-timeout").await;
    let mut local = connect_unix(&servers.socket).await;
    install_fixture(&mut local, &servers).await;
    make_fifos(&servers.work, "hold");

    let work = servers.work.clone();
    let handshake = tokio::spawn(async move { started(&work, "hold").await });
    let error = expect_code(
        &mut local,
        PACKAGE,
        "hold",
        json!({ "timeout_ms": STOP_TIMEOUT_MS }),
    )
    .await;
    let (pid, child) = handshake.await.expect("handshake");
    assert_eq!(error.code, "command_timed_out");
    assert!(error.details.is_none());
    assert_gone(pid, "the program").await;
    assert_gone(child, "its background child").await;

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn output_over_the_cap_ends_the_run_before_the_program_exits() {
    let servers = spawn_servers("cmd-cap").await;
    let mut local = connect_unix(&servers.socket).await;
    install_fixture(&mut local, &servers).await;
    let after = servers.work.join("big.after");

    let capped = expect_code(
        &mut local,
        PACKAGE,
        "big",
        json!({ "max_output_bytes": 10 }),
    )
    .await;
    assert_eq!(capped.code, "command_output_too_large");
    let uncapped = expect_code(&mut local, PACKAGE, "big", json!({})).await;
    assert_eq!(uncapped.code, "command_output_too_large");
    const { assert!(MAX_COMMAND_OUTPUT_BYTES + 100_000 < 2_000_000) }; // the fixture writes more than the ceiling
    assert!(!after.exists(), "the program was stopped while writing");

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn an_unsuccessful_exit_carries_typed_details_and_nothing_else_does() {
    let servers = spawn_servers("cmd-exit").await;
    let mut local = connect_unix(&servers.socket).await;
    install_fixture(&mut local, &servers).await;

    let exited = expect_code(&mut local, PACKAGE, "exit-three", json!({})).await;
    assert_eq!(exited.code, "command_failed");
    let exit = exited
        .details
        .and_then(|details| details.process_exit)
        .expect("process_exit details");
    assert_eq!((exit.exit_code, exit.signal), (Some(3), None));

    let killed = expect_code(&mut local, PACKAGE, "selfkill", json!({})).await;
    assert_eq!(killed.code, "command_failed");
    let exit = killed
        .details
        .and_then(|details| details.process_exit)
        .expect("process_exit details");
    assert_eq!((exit.exit_code, exit.signal), (None, Some(9)));

    let not_utf8 = expect_code(&mut local, PACKAGE, "badutf8", json!({})).await;
    assert_eq!(not_utf8.code, "command_output_not_utf8");
    assert!(not_utf8.details.is_none());

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn no_request_field_reaches_the_program_and_bad_limits_are_refused() {
    let servers = spawn_servers("cmd-params").await;
    let mut local = connect_unix(&servers.socket).await;
    install_fixture(&mut local, &servers).await;
    let ran = servers.work.join("ok.ran");

    for extra in [
        json!({ "args": ["--evil"] }),
        json!({ "env": { "X": "1" } }),
        json!({ "cwd": "/" }),
        json!({ "stdin": "data" }),
        json!({ "unknown": true }),
        json!({ "timeout_ms": 0 }),
        json!({ "max_output_bytes": 0 }),
    ] {
        let error = expect_code(&mut local, PACKAGE, "ok", extra.clone()).await;
        assert_eq!(error.code, "bad_request", "{extra}");
        assert!(!ran.exists(), "the program never started for {extra}");
    }

    // A timeout above the ceiling is lowered to it, not refused.
    run_command(
        &mut local,
        PACKAGE,
        "ok",
        json!({ "timeout_ms": u32::MAX, "max_output_bytes": u32::MAX }),
    )
    .await
    .expect("oversized limits are clamped");

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn the_program_sees_only_the_fixed_environment_a_private_cwd_and_closed_stdin() {
    let servers = spawn_servers("cmd-env").await;
    let mut local = connect_unix(&servers.socket).await;
    let digest = install_fixture(&mut local, &servers).await;

    run_command(&mut local, PACKAGE, "env", json!({}))
        .await
        .expect("the env command runs");
    let names = std::fs::read_to_string(servers.work.join("env.names")).expect("env names");
    let seen: Vec<&str> = names.lines().collect();
    // Variables a shell adds on its own are tolerated; everything else must be
    // one of the runner's fixed names.
    let allowed = [
        "HOME",
        "LANG",
        "LC_ALL",
        "PATH",
        "TMPDIR",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
    ];
    for name in &seen {
        assert!(allowed.contains(name), "unexpected variable {name}");
    }
    for required in ["HOME", "LANG", "LC_ALL", "TMPDIR", "XDG_CONFIG_HOME"] {
        assert!(seen.contains(&required), "missing variable {required}");
    }
    let cwd = std::fs::read_to_string(servers.work.join("cwd")).expect("cwd");
    let root = package::registry::files_path(&servers.plugins, &digest);
    assert!(
        !Path::new(cwd.trim()).starts_with(&root),
        "the working directory is not the package root"
    );
    for (name, _value) in SENTINELS {
        assert!(!seen.contains(&name), "{name} reached the command");
    }
    let launch_path = servers
        .launch_env
        .environment()
        .get(std::ffi::OsStr::new("PATH"))
        .map(|path| path.to_string_lossy().into_owned());
    let child_path = std::fs::read_to_string(servers.work.join("path")).expect("path");
    assert_eq!(
        Some(child_path),
        launch_path,
        "only the agents' search path crosses"
    );
    let stdin = std::fs::read_to_string(servers.work.join("stdin")).expect("stdin marker");
    assert_eq!(stdin.trim(), "eof");

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn one_command_runs_once_and_the_host_runs_four_across_both_transports() {
    let servers = spawn_servers("cmd-busy").await;
    let mut local = connect_unix(&servers.socket).await;
    let mut remote = connect_tcp(servers.addr).await;
    install_fixture(&mut local, &servers).await;
    let tags = ["gate-a", "gate-b", "gate-c", "gate-d", "gate-e"];
    for tag in tags {
        make_fifos(&servers.work, tag);
    }
    assert_eq!(MAX_HOST_COMMAND_RUNS, 4);

    // Four runs in flight, alternating the transports of the callers.
    let mut running = Vec::new();
    for (index, tag) in tags.iter().take(MAX_HOST_COMMAND_RUNS).enumerate() {
        let socket = servers.socket.clone();
        let addr = servers.addr;
        let tag = (*tag).to_owned();
        running.push(tokio::spawn(async move {
            if index % 2 == 0 {
                let mut framed = connect_unix(&socket).await;
                run_command(&mut framed, PACKAGE, &tag, json!({})).await
            } else {
                let mut framed = connect_tcp(addr).await;
                run_command(&mut framed, PACKAGE, &tag, json!({})).await
            }
        }));
        started(&servers.work, tags[index]).await;
    }

    // The same command again is refused, and so is a fifth distinct one.
    assert_eq!(
        expect_code(&mut remote, PACKAGE, "gate-a", json!({}))
            .await
            .code,
        "command_busy"
    );
    assert_eq!(
        expect_code(&mut local, PACKAGE, "gate-e", json!({}))
            .await
            .code,
        "command_busy"
    );

    // Releasing one run frees its slot for the next call.
    release(&servers.work, "gate-a").await;
    let first = running.remove(0).await.expect("first run").expect("result");
    assert_eq!(first.output, "released");
    let fifth = tokio::spawn({
        let socket = servers.socket.clone();
        async move {
            let mut framed = connect_unix(&socket).await;
            run_command(&mut framed, PACKAGE, "gate-e", json!({})).await
        }
    });
    started(&servers.work, "gate-e").await;
    release(&servers.work, "gate-e").await;
    fifth.await.expect("fifth run").expect("fifth result");
    for tag in ["gate-b", "gate-c", "gate-d"] {
        release(&servers.work, tag).await;
    }
    for run in running {
        run.await.expect("run").expect("result");
    }

    drop((local, remote));
    servers.stop().await;
}

#[tokio::test]
async fn a_background_child_does_not_outlive_a_successful_run() {
    let servers = spawn_servers("cmd-background").await;
    let mut local = connect_unix(&servers.socket).await;
    install_fixture(&mut local, &servers).await;
    make_fifos(&servers.work, "background");

    let result = run_command(&mut local, PACKAGE, "background", json!({}))
        .await
        .expect("the run succeeds");
    assert_eq!(result.output, "done");
    assert_gone(
        read_pid(&servers.work.join("background.child")),
        "the child",
    )
    .await;

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn a_package_file_modified_after_install_is_not_run() {
    let servers = spawn_servers("cmd-tamper").await;
    let mut local = connect_unix(&servers.socket).await;
    let digest = install_fixture(&mut local, &servers).await;
    run_command(&mut local, PACKAGE, "ok", json!({}))
        .await
        .expect("an intact package runs");
    std::fs::remove_file(servers.work.join("ok.ran")).expect("reset the marker");
    let intact: PackageInspectResult = call(
        &mut local,
        method::PACKAGE_INSPECT,
        json!({ "digest": digest }),
    )
    .await;
    assert!(
        !intact.commands.is_empty(),
        "an intact package lists its commands"
    );
    assert!(
        intact.commands_only,
        "an intact commands-only package carries the marker"
    );

    let root = package::registry::files_path(&servers.plugins, &digest);
    let script = root.join("bin/run.sh");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .expect("make the script writable");
    let mut tampered = SCRIPT.as_bytes().to_vec();
    tampered.extend_from_slice(b"# tampered\n");
    std::fs::write(&script, tampered).expect("modify the script");

    let error = expect_code(&mut local, PACKAGE, "ok", json!({})).await;
    assert_eq!(error.code, "command_unavailable");
    assert!(
        !servers.work.join("ok.ran").exists(),
        "the program did not start"
    );
    let inspected: PackageInspectResult = call(
        &mut local,
        method::PACKAGE_INSPECT,
        json!({ "digest": digest }),
    )
    .await;
    assert!(
        inspected.commands.is_empty() && !inspected.commands_only,
        "a package that fails verification lists no commands and no marker"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn daemon_shutdown_stops_a_run_and_refuses_new_ones() {
    let servers = spawn_servers("cmd-shutdown").await;
    let mut local = connect_unix(&servers.socket).await;
    install_fixture(&mut local, &servers).await;
    make_fifos(&servers.work, "hold");

    let socket = servers.socket.clone();
    let running = tokio::spawn(async move {
        let mut framed = connect_unix(&socket).await;
        run_command(&mut framed, PACKAGE, "hold", json!({})).await
    });
    let (pid, child) = started(&servers.work, "hold").await;

    servers.registry.begin_daemon_shutdown();
    let error = running.await.expect("run task").expect_err("the run ends");
    assert_eq!(error.code, "command_unavailable");
    servers.state.package_commands.shutdown().await;
    assert_gone(pid, "the program").await;
    assert_gone(child, "its background child").await;

    let refused = expect_code(&mut local, PACKAGE, "ok", json!({})).await;
    assert_eq!(refused.code, "command_unavailable");
    assert!(!servers.work.join("ok.ran").exists());

    drop(local);
    servers.stop().await;
}

/// A tracing writer that keeps what the daemon logs.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The log buffer of the process-wide subscriber, installed once so every
/// test of this binary shares one callsite interest; a scenario finds its own
/// lines by its package digest.
fn captured_log() -> &'static Captured {
    static LOG: std::sync::OnceLock<Captured> = std::sync::OnceLock::new();
    LOG.get_or_init(|| {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other subscriber is installed in this test binary");
        captured
    })
}

#[tokio::test]
async fn a_run_is_logged_once_without_its_output() {
    let captured = captured_log();
    let servers = spawn_servers("cmd-log").await;
    let mut local = connect_unix(&servers.socket).await;
    let digest = install_fixture(&mut local, &servers).await;
    run_command(&mut local, PACKAGE, "ok", json!({}))
        .await
        .expect("the run succeeds");
    expect_code(&mut local, PACKAGE, "exit-three", json!({})).await;

    let log = String::from_utf8(captured.0.lock().expect("log buffer").clone()).expect("utf-8 log");
    let lines: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("package command run") && line.contains(&digest.to_string()))
        .collect();
    assert_eq!(lines.len(), 2, "one line per run: {log}");
    assert!(
        lines[0].contains(PACKAGE) && lines[0].contains("outcome=\"ok\""),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("outcome=\"command_failed\"") && lines[1].contains("exit_code=3"),
        "{}",
        lines[1]
    );
    assert!(
        !log.contains("second line") && !log.contains("h\u{e9}llo"),
        "the output never reaches a log"
    );

    drop(local);
    servers.stop().await;
}

#[tokio::test]
async fn the_first_remote_calls_after_install_are_gated_from_a_cold_cache() {
    let servers = spawn_servers("cmd-cold").await;
    let mut local = connect_unix(&servers.socket).await;
    let mut remote = connect_tcp(servers.addr).await;
    install_fixture(&mut local, &servers).await;

    // No call has read the descriptor yet.
    assert_eq!(
        expect_code(&mut remote, PACKAGE, "ok-local", json!({}))
            .await
            .code,
        "command_not_exposed"
    );
    assert!(!servers.work.join("ok-local.ran").exists());
    run_command(&mut remote, PACKAGE, "ok", json!({}))
        .await
        .expect("a peers command runs over the overlay");

    drop((local, remote));
    servers.stop().await;
}

#[tokio::test]
async fn a_caller_that_hangs_up_stops_the_run_and_frees_its_slot() {
    let captured = captured_log();
    let servers = spawn_servers("cmd-hangup").await;
    let mut local = connect_unix(&servers.socket).await;
    let digest = install_fixture(&mut local, &servers).await;
    make_fifos(&servers.work, "hold");

    let socket = servers.socket.clone();
    let caller = tokio::spawn(async move {
        let mut framed = connect_unix(&socket).await;
        let request = ProtocolRequest::new(
            "hangup",
            method::COMMAND_RUN,
            json!({ "package": PACKAGE, "command": "hold" }),
        )
        .expect("valid request");
        framed
            .send(serde_json::to_string(&request).expect("serialize"))
            .await
            .expect("send");
        framed
    });
    let (pid, child) = started(&servers.work, "hold").await;
    // Dropping the connection is the hangup.
    drop(caller.await.expect("caller"));
    assert_gone(pid, "the program").await;
    assert_gone(child, "its background child").await;

    // The slot returns once the killed group is gone: the same command is
    // admitted again, so a `command_busy` seen meanwhile only means the run is
    // still being reaped.
    let socket = servers.socket.clone();
    let again = tokio::spawn(async move {
        let mut framed = connect_unix(&socket).await;
        for _ in 0..20_000 {
            match run_command(&mut framed, PACKAGE, "hold", json!({})).await {
                Err(error) if error.code == "command_busy" => tokio::task::yield_now().await,
                other => return other,
            }
        }
        panic!("the slot was never freed");
    });
    started(&servers.work, "hold").await;
    release(&servers.work, "hold").await;
    again.await.expect("run task").expect("the slot was free");

    // Each spawned run is logged once, the abandoned one included.
    let log = String::from_utf8(captured.0.lock().expect("log buffer").clone()).expect("utf-8 log");
    let lines: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("package command run") && line.contains(&digest.to_string()))
        .collect();
    assert_eq!(lines.len(), 2, "one line per spawned run: {log}");
    assert!(
        lines[0].contains("outcome=\"command_unavailable\""),
        "{}",
        lines[0]
    );
    assert!(lines[1].contains("outcome=\"ok\""), "{}", lines[1]);

    drop(local);
    servers.stop().await;
}
