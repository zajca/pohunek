//! `NetBird` static peers under a restricted overview.
//!
//! The fake `netbird` prints a restricted `0.83` status (no peer details); the
//! CLI under test must resolve the owner-configured static peer in
//! `overlays.toml`, verify the host-id pin before any other request, and refuse
//! every static peer for a raw address or the `host discover`/`list` surface.

// Rust guideline compliant 2026-11-13

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command as ProcCommand, Stdio};
use std::sync::{Arc, Mutex};

/// A daemon process, killed and reaped on drop.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

use pohunek_test_support::env::TestEnv;
use protocol::{Request, Response, PROTOCOL_VERSION};
use serde_json::{json, Value};

#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[path = "support/pin_overlay.rs"]
mod pin_overlay;

/// The restricted `0.83` status from slice A: no peer detail, daemon connected.
const RESTRICTED: &str = include_str!("../../netbird/tests/fixtures/status_restricted_v0_83.json");

/// Display name, FQDN and address of the static peer and this machine.
const PEER_NAME: &str = "vps2";
const PEER_FQDN: &str = "vps2.remote-box.netbird.test";
const PEER_IP: &str = "100.64.0.21";
const SELF_IP: &str = "100.64.0.7";

/// Requests the fixture daemon served, by method, in arrival order.
type Served = Arc<Mutex<Vec<String>>>;

/// Host-id spellings the daemon reports and the pin expects.
fn host_id(byte: u8) -> String {
    format!(
        "host_{}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        byte as char
    )
}

/// A governance status carrying `reported`, built from `protocol` so no test
/// hand-writes the JSON shape.
fn governance_status(reported: &str) -> Value {
    let status = protocol::HostGovernanceStatus::new(
        protocol::HostId::parse(reported).expect("host id"),
        None,
        None,
        None,
        None,
        protocol::ApprovalKeyReference::from_ed25519_verifying_key_bytes([7; 32]),
    )
    .expect("valid governance status");
    serde_json::to_value(status).expect("status JSON")
}

fn result_for(request: &Request, reported: &str) -> Value {
    match request.method() {
        protocol::method::SESSION_LIST => json!([]),
        protocol::method::HOST_GOVERNANCE_INSPECT => governance_status(reported),
        protocol::method::SESSION_ATTACH => json!({"stream_id": "stream-1"}),
        _ => json!({}),
    }
}

fn serve(listener: TcpListener, served: Served, reported: String) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&served);
            let reported = reported.clone();
            std::thread::spawn(move || serve_connection(stream, &served, &reported));
        }
    });
}

/// Fixture that answers `host.governance.inspect` with `first` on the first
/// connection and `later` on every connection after that, recording methods.
fn serve_first_then_later(listener: TcpListener, served: Served, first: String, later: String) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let counter = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let served = Arc::clone(&served);
            let counter = Arc::clone(&counter);
            let first = first.clone();
            let later = later.clone();
            std::thread::spawn(move || {
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let reported = if index == 0 { first } else { later };
                serve_connection(stream, &served, &reported);
            });
        }
    });
}

/// A daemon recording every method served, grouped per connection, with a
/// switchable reported host id and a session list that names `s-1`. The
/// connection slot is allocated at accept time and each request is pushed
/// into its slot as it is read, so the test may assert on the recording
/// without waiting for the fixture threads to observe EOF.
fn serve_session_and_track(
    listener: TcpListener,
    connections: Arc<Mutex<Vec<Vec<String>>>>,
    reported: Arc<Mutex<String>>,
) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let connections = Arc::clone(&connections);
            let slot = connections.lock().expect("connections").len();
            connections.lock().expect("connections").push(Vec::new());
            let reported = Arc::clone(&reported);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).expect("read a fixture line") == 0 {
                        return;
                    }
                    let request: Request =
                        serde_json::from_str(line.trim_end()).expect("parse a request");
                    connections.lock().expect("connections")[slot]
                        .push(request.method().to_owned());
                    let reported = reported.lock().expect("reported").clone();
                    let result = match request.method() {
                        protocol::method::SESSION_INPUT => {
                            json!({"accepted": true, "activity": "working", "activity_source": "process", "activity_epoch": "e1", "activity_revision": "1", "submitted": true, "runtime": {"worker_instance_id": "w1", "runtime_generation": "1"}})
                        }
                        protocol::method::SESSION_LIST => {
                            let session = protocol::SessionInfo {
                                id: protocol::SessionId("s-1".to_owned()),
                                external: None,
                                capabilities: protocol::SessionCapabilities::default(),
                                capability_refusals: None,
                                placement: None,
                                name: Some("s-1".to_owned()),
                                agent: "test".to_owned(),
                                agent_base: protocol::RuntimeRef::shell(),
                                cwd: "/work".into(),
                                cwd_source: None,
                                pid: 1,
                                runtime: None,
                                cols: 80,
                                rows: 24,
                                state: protocol::SessionState::Running,
                                state_source: protocol::StateSource::Process,
                                activity: Some(protocol::AgentActivity::Idle),
                                blocked_kind: None,
                                subagents: Vec::new(),
                                background: None,
                                active_agent: None,
                                active_agent_base: None,
                                active_agent_pid: None,
                                active_agent_session_id: None,
                                active_agent_session_path: None,
                                native_session_id: None,
                                native_session_path: None,
                                native_last_activity_at: None,
                                project_id: None,
                                project_label: None,
                                is_linked_worktree: None,
                                repo: None,
                                branch: None,
                                worktree_path: None,
                                warnings: Vec::new(),
                                output_force_closed: false,
                                metadata: std::collections::BTreeMap::new(),
                                spawned_by: None,
                                attention_isolated_by: None,
                                created_at: "2026-01-01T00:00:00Z".to_owned(),
                                updated_at: "2026-01-01T00:00:00Z".to_owned(),
                                ended_at: None,
                                exit_code: None,
                            };
                            json!([session])
                        }
                        protocol::method::HOST_GOVERNANCE_INSPECT => governance_status(&reported),
                        _ => json!({}),
                    };
                    let response = Response::ok(protocol::PROTOCOL_VERSION, request.id(), result)
                        .expect("valid fixture response");
                    writeln!(
                        reader.get_mut(),
                        "{}",
                        serde_json::to_string(&response).expect("serialize the response")
                    )
                    .expect("write the fixture response");
                }
            });
        }
    });
}

fn serve_connection(stream: TcpStream, served: &Served, reported: &str) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).expect("read a fixture line") == 0 {
            return;
        }
        let Ok(request): Result<Request, _> = serde_json::from_str(line.trim_end()) else {
            // Raw attach prelude, not a framed request: close without recording.
            return;
        };
        served
            .lock()
            .expect("served requests")
            .push(request.method().to_owned());
        let response = Response::ok(
            PROTOCOL_VERSION,
            request.id(),
            result_for(&request, reported),
        )
        .expect("valid fixture response");
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).expect("serialize the response")
        )
        .expect("write the fixture response");
    }
}

/// Writes the fake `netbird`, the `overlays.toml` and a loopback daemon, then
/// runs one CLI invocation against the static peer `vps2`.
struct Run {
    output: std::process::Output,
    served: Vec<String>,
    dialed: Vec<String>,
}

/// The `overlays.toml` for the static-peer scenarios.
fn overlays_toml(pinned: &str) -> String {
    format!(
        "providers = [\"netbird\"]\n\n\
         [netbird]\nsubnet = \"100.64.0.0/10\"\n\n\
         [netbird.static_peers.{PEER_NAME}]\n\
         address = \"{PEER_IP}\"\n\
         fqdn = \"{PEER_FQDN}\"\n\
         host_id = \"{pinned}\"\n"
    )
}

/// Human-readable form of one command output.
fn diagnostics(output: &std::process::Output) -> String {
    format!(
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Runs `pohunek --host vps2 session list --json` with `overlays.toml` pinning
/// `pinned`; the daemon reports `reported` from `host.governance.inspect`.
fn run_static_session(pinned: &str, reported: &str) -> Run {
    run_pinned(
        &["--host", PEER_NAME, "session", "list", "--json"],
        pinned,
        reported,
    )
}

fn run_pinned(args: &[&str], pinned: &str, reported: &str) -> Run {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, overlays_toml(pinned)).expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");

    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{RESTRICTED}'\n"),
    )
    .expect("write fake netbird");

    let (library, capture) = connect_redirect::build(&root);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let served = Served::default();
    serve(listener, Arc::clone(&served), reported.to_owned());

    let mut command = env.command(pohunek_test_support::bin_exe("pohunek"));
    command.env("PATH", &bin);
    command.env("LD_PRELOAD", &library);
    command.env("POHUNEK_TEST_CONNECT_CAPTURE", &capture);
    command.env("POHUNEK_REMOTE_PORT", port.to_string());
    command.args(args);
    let output = command.output().expect("run pohunek");

    let dialed = fs::read_to_string(&capture)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    let served = served.lock().expect("served requests").clone();
    Run {
        output,
        served,
        dialed,
    }
}

#[test]
fn a_static_peer_resolves_and_verifies_before_other_requests() {
    let pinned = host_id(b'A');
    let run = run_static_session(&pinned, &pinned);
    assert!(run.output.status.success(), "{}", diagnostics(&run.output));
    assert!(
        run.dialed.iter().any(|line| line.starts_with(PEER_IP)),
        "the CLI dials the static peer: {:?}",
        run.dialed
    );
    // The version-independent inspect is negotiated without a `daemon.health`
    // probe, so it is the first request the daemon serves.
    let inspect = run
        .served
        .iter()
        .position(|method| method == protocol::method::HOST_GOVERNANCE_INSPECT)
        .expect("the pin is checked");
    assert_eq!(
        inspect, 0,
        "the check precedes every other request: {:?}",
        run.served
    );
    assert!(
        run.served
            .iter()
            .any(|method| method == protocol::method::SESSION_LIST),
        "the session list follows the check: {:?}",
        run.served
    );
}

#[test]
fn a_mismatching_host_id_refuses_and_dials_nothing_after_the_check() {
    let pinned = host_id(b'A');
    let reported = host_id(b'B');
    let run = run_static_session(&pinned, &reported);
    assert!(!run.output.status.success(), "{}", diagnostics(&run.output));
    let output = diagnostics(&run.output);
    assert!(
        output.contains("host_identity_mismatch"),
        "the typed error is reported: {output}"
    );
    assert!(
        output.contains(&pinned),
        "the message names the pin: {output}"
    );
    assert!(
        output.contains(&reported),
        "the message names the reported id: {output}"
    );
    assert!(
        output.contains("overlays.toml"),
        "the recovery names the file: {output}"
    );
    assert!(
        !run.served
            .iter()
            .any(|method| method == protocol::method::SESSION_LIST),
        "no other request reaches the daemon: {:?}",
        run.served
    );
}

/// Guard (raw address): a raw address is never matched against the static
/// table. The `netbird_state_restricted` code is asserted, so the test
/// fails if the raw literal started to resolve (or if the code changed).
#[test]
fn a_raw_address_is_never_matched_against_the_static_table() {
    let pinned = host_id(b'A');
    let run = run_pinned(
        &["--host", PEER_IP, "session", "list", "--json"],
        &pinned,
        &pinned,
    );
    let text = diagnostics(&run.output);
    assert!(!run.output.status.success(), "{text}");
    assert!(
        text.contains("netbird_state_restricted"),
        "the restricted code: {text}"
    );
    assert!(
        run.dialed.is_empty(),
        "no dial for a raw address: {:?}",
        run.dialed
    );
    assert!(
        run.served.is_empty(),
        "no request for a raw address: {:?}",
        run.served
    );
}

/// Row 5 (no-table shape, CLI side): a name absent from the table fails
/// with the restricted error rather than `host_unknown`. The
/// `netbird_state_restricted` code is asserted, so the test fails if the
/// code changed. (Empty table => `overlays.toml` without `[netbird]`.)
#[test]
fn a_restricted_overview_without_the_table_keeps_failing_closed() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, "providers = [\"netbird\"]\n").expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{RESTRICTED}'\n"),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["--host", PEER_NAME, "session", "list", "--json"])
        .output()
        .expect("run pohunek");
    assert!(!output.status.success(), "{}", diagnostics(&output));
    let text = diagnostics(&output);
    assert!(
        text.contains("netbird_state_restricted"),
        "the restricted code: {text}"
    );
    assert!(
        fs::read_to_string(&capture).unwrap_or_default().is_empty(),
        "no dial without the table"
    );
}

/// Row 4: a `Connecting` overview never resolves the static peer. The restricted
/// fixture's `Connected` is replaced once at write time, so the check is not
/// a tautology. The `netbird_state_restricted` code is asserted.
#[test]
fn a_connecting_overview_never_resolves_the_static_peer() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, overlays_toml(&pinned)).expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let connecting = RESTRICTED.replace(
        "\"daemonStatus\":\"Connected\"",
        "\"daemonStatus\":\"Connecting\"",
    );
    assert_ne!(connecting, RESTRICTED);
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{connecting}'\n"),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["--host", PEER_NAME, "session", "list", "--json"])
        .output()
        .expect("run pohunek");
    assert!(!output.status.success(), "{}", diagnostics(&output));
    let out = diagnostics(&output);
    assert!(
        out.contains("netbird_state_restricted"),
        "the restricted code: {out}"
    );
    assert!(
        fs::read_to_string(&capture).unwrap_or_default().is_empty(),
        "a Connecting overview dials nothing"
    );
}

/// Row 6: a restricted overview never names a static peer in discovery or
/// listing. `host discover --json` and `host list --json` both fail with the
/// restricted code and no peer record carries the static name or FQDN.
#[test]
fn host_discover_does_not_name_a_static_peer() {
    let pinned = host_id(b'A');
    let discover = run_pinned(&["host", "discover", "--json"], &pinned, &pinned);
    let discover_out = diagnostics(&discover.output);
    assert!(!discover.output.status.success(), "{discover_out}");
    assert!(
        discover_out.contains("netbird_state_restricted"),
        "the restricted code: {discover_out}"
    );
    assert!(
        !discover_out.contains(PEER_NAME) && !discover_out.contains(PEER_FQDN),
        "discovery names no static peer: {discover_out}"
    );
    let list = run_pinned(&["host", "list", "--json"], &pinned, &pinned);
    let list_out = diagnostics(&list.output);
    assert!(!list.output.status.success(), "{list_out}");
    assert!(
        list_out.contains("netbird_state_restricted"),
        "the restricted code: {list_out}"
    );
    assert!(
        !list_out.contains(PEER_NAME) && !list_out.contains(PEER_FQDN),
        "list names no static peer: {list_out}"
    );
}

/// Row 11: the daemon builds its registry **without** static peers. A real
/// `pohunekd` with the static-peer `overlays.toml` and a fake restricted
/// `netbird` answers `host.resolve` over its local Unix socket with
/// `netbird_state_restricted`, never a `Remote` resolution for the static
/// address `100.64.0.21`.
#[test]
fn the_daemon_never_resolves_a_static_peer() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, overlays_toml(&pinned)).expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{RESTRICTED}'\n"),
    )
    .expect("write fake netbird");

    let daemon =
        ProcCommand::new(pohunek_test_support::bin_exe("pohunek").with_file_name("pohunekd"))
            .env_clear()
            .envs(env.environment())
            .env("PATH", &bin)
            .env("SHELL", "/bin/sh")
            .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
            .env("POHUNEK_WORKER_BIN", pohunek_test_support::worker_binary())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn pohunekd");
    let _guard = Daemon(daemon);

    let socket = root.join("run/pohunek/daemon.sock");
    let stream = pohunek_test_support::wait::poll_until("the daemon to serve", || {
        UnixStream::connect(&socket).ok()
    });
    let mut stream = BufReader::new(stream);
    let params = protocol::HostResolveParams {
        route: PEER_NAME.to_owned(),
    };
    let request = protocol::Request::new(
        "static-peer-resolve",
        protocol::method::HOST_RESOLVE,
        serde_json::to_value(params).expect("params JSON"),
    )
    .expect("host.resolve request");
    stream
        .get_mut()
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&request).expect("request JSON")
            )
            .as_bytes(),
        )
        .expect("send host.resolve");
    let mut line = String::new();
    stream
        .read_line(&mut line)
        .expect("read host.resolve reply");
    let value: Value = serde_json::from_str(line.trim()).expect("reply JSON");
    assert!(
        value["ok"].is_null(),
        "host.resolve does not resolve a static peer: {value}"
    );
    let code = value["err"]["code"].as_str().expect("an error code");
    assert_eq!(
        code, "netbird_state_restricted",
        "the restricted error: {value}"
    );
}

/// Row 5: an unmatched name fails closed with the restricted error, never with
/// `host_unknown`, and dials nothing.
#[test]
fn an_unmatched_name_fails_with_the_restricted_error() {
    let pinned = host_id(b'A');
    let run = run_pinned(
        &["--host", "other-host", "session", "list", "--json"],
        &pinned,
        &pinned,
    );
    assert!(!run.output.status.success(), "{}", diagnostics(&run.output));
    let out = diagnostics(&run.output);
    assert!(
        out.contains("netbird_state_restricted"),
        "restricted code: {out}"
    );
    assert!(
        run.dialed.is_empty(),
        "an unmatched name dials nothing: {:?}",
        run.dialed
    );
}

/// Guard (raw-IP fqdn): an fqdn that is the raw IP must not make a raw-IP
/// selector match the table. Without the guard, `--host 100.64.0.21` would
/// resolve the static entry; with it, it must fail closed.
#[test]
fn a_raw_ip_bound_fqdn_never_matches_the_table() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    let table = format!(
        "providers = [\"netbird\"]\n\n[netbird]\nsubnet = \"100.64.0.0/10\"\n\n\
         [netbird.static_peers.{PEER_NAME}]\naddress = \"{PEER_IP}\"\nfqdn = \"{PEER_IP}\"\nhost_id = \"{pinned}\"\n"
    );
    fs::write(&file, table).expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{RESTRICTED}'\n"),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["--host", PEER_IP, "session", "list", "--json"])
        .output()
        .expect("run pohunek");
    assert!(!output.status.success(), "{}", diagnostics(&output));
    let out = diagnostics(&output);
    assert!(
        out.contains("netbird_state_restricted"),
        "restricted code: {out}"
    );
    assert!(
        fs::read_to_string(&capture).unwrap_or_default().is_empty(),
        "the raw selector dials nothing"
    );
}

/// Row 7 (address field): an address outside the subnet is
/// `overlay_configuration_invalid` and the provider is never called.
#[test]
fn a_static_address_outside_the_subnet_is_refused_at_load() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(
        &file,
        format!(
            "providers = [\"netbird\"]\n\n[netbird]\nsubnet = \"100.64.0.0/10\"\n\n\
             [netbird.static_peers.{PEER_NAME}]\naddress = \"192.0.2.1\"\nfqdn = \"{PEER_FQDN}\"\nhost_id = \"{pinned}\"\n"
        ),
    )
    .expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    let marker = root.join("netbird_called");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!(
            "#!/bin/sh\ntouch '{marker}'\nprintf '%s\\n' '{RESTRICTED}'\n",
            marker = marker.display()
        ),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["--host", PEER_NAME, "session", "list", "--json"])
        .output()
        .expect("run pohunek");
    assert!(!output.status.success(), "{}", diagnostics(&output));
    let out = diagnostics(&output);
    assert!(
        out.contains("overlay_configuration_invalid"),
        "config code: {out}"
    );
    assert!(
        out.contains("netbird.static_peers.vps2"),
        "names the key: {out}"
    );
    assert!(
        !out.contains("192.0.2.1"),
        "never carries the address: {out}"
    );
    assert!(!marker.exists(), "no provider call");
    assert!(
        fs::read_to_string(&capture).unwrap_or_default().is_empty(),
        "a refused config dials nothing"
    );
}

/// Row 7 (provider field): `[netbird]` with a `tailscale` provider is
/// `overlay_configuration_invalid` and the provider is never called.
#[test]
fn a_netbird_table_with_a_tailscale_provider_is_refused_at_load() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(
        &file,
        format!(
            "providers = [\"tailscale\"]\n\n[netbird]\nsubnet = \"100.64.0.0/10\"\n\n\
             [netbird.static_peers.{PEER_NAME}]\naddress = \"{PEER_IP}\"\nfqdn = \"{PEER_FQDN}\"\nhost_id = \"{pinned}\"\n"
        ),
    )
    .expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    let marker = root.join("tailscale_called");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!(
            "#!/bin/sh\ntouch '{marker}'\nprintf '%s\\n' '{RESTRICTED}'\n",
            marker = marker.display()
        ),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["--host", PEER_NAME, "session", "list", "--json"])
        .output()
        .expect("run pohunek");
    assert!(!output.status.success(), "{}", diagnostics(&output));
    let out = diagnostics(&output);
    assert!(
        out.contains("overlay_configuration_invalid"),
        "config code: {out}"
    );
    assert!(
        out.contains("netbird table") && out.contains("netbird") && out.contains("providers"),
        "names the table: {out}"
    );
    assert!(!marker.exists(), "no provider call");
}

/// Row 7 (host-id field): a malformed `host_id` is
/// `overlay_configuration_invalid` and the provider is never called.
#[test]
fn a_bad_host_id_is_refused_at_load() {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(
        &file,
        format!(
            "providers = [\"netbird\"]\n\n[netbird]\nsubnet = \"100.64.0.0/10\"\n\n\
             [netbird.static_peers.{PEER_NAME}]\naddress = \"{PEER_IP}\"\nfqdn = \"{PEER_FQDN}\"\nhost_id = \"bogus\"\n"
        ),
    )
    .expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    let marker = root.join("netbird_called");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!(
            "#!/bin/sh\ntouch '{marker}'\nprintf '%s\\n' '{RESTRICTED}'\n",
            marker = marker.display()
        ),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["--host", PEER_NAME, "session", "list", "--json"])
        .output()
        .expect("run pohunek");
    assert!(!output.status.success(), "{}", diagnostics(&output));
    let out = diagnostics(&output);
    assert!(
        out.contains("overlay_configuration_invalid"),
        "config code: {out}"
    );
    assert!(
        out.contains("netbird.static_peers.vps2"),
        "names the key: {out}"
    );
    assert!(
        !out.contains("bogus"),
        "never carries the host id value: {out}"
    );
    assert!(!marker.exists(), "no provider call");
}

/// Row 7 (uniqueness, short name): two entries sharing a short name are
/// `overlay_configuration_invalid` and the provider is never called.
#[test]
fn a_shared_short_name_is_refused_at_load() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(
        &file,
        format!(
            "providers = [\"netbird\"]\n\n[netbird]\nsubnet = \"100.64.0.0/10\"\n\n\
             [netbird.static_peers.{PEER_NAME}]\naddress = \"{PEER_IP}\"\nfqdn = \"vps2.remote-box.netbird.test\"\nhost_id = \"{pinned}\"\n\n\
             [netbird.static_peers.twin]\naddress = \"100.64.0.22\"\nfqdn = \"vps2.other.netbird.test\"\nhost_id = \"host_CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n"
        ),
    )
    .expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    let marker = root.join("netbird_called");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!(
            "#!/bin/sh\ntouch '{marker}'\nprintf '%s\\n' '{RESTRICTED}'\n",
            marker = marker.display()
        ),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["--host", PEER_NAME, "session", "list", "--json"])
        .output()
        .expect("run pohunek");
    assert!(!output.status.success(), "{}", diagnostics(&output));
    let out = diagnostics(&output);
    assert!(
        out.contains("overlay_configuration_invalid"),
        "config code: {out}"
    );
    assert!(
        out.contains("share a selector name"),
        "the selector-name-arm message: {out}"
    );
    assert!(!marker.exists(), "no provider call");
    assert!(
        fs::read_to_string(&capture).unwrap_or_default().is_empty(),
        "a refused config dials nothing"
    );
}

/// Row 8 (cross-check, selector side): a full status that contradicts the
/// static table is `overlay_configuration_invalid`, names the entry, and
/// dials nothing.
#[test]
fn a_contradicting_full_status_is_refused_at_load() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, overlays_toml(&pinned)).expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    // Full status whose peer table gives a different address for the entry.
    let full = pin_overlay::status(
        Some(("self.netbird.test", SELF_IP)),
        &[pin_overlay::Peer::new(PEER_FQDN, "100.64.0.99", "key-one")],
    );
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{full}'\n"),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .args(["--host", PEER_NAME, "session", "list", "--json"])
        .output()
        .expect("run pohunek");
    assert!(!output.status.success(), "{}", diagnostics(&output));
    let out = diagnostics(&output);
    assert!(
        out.contains("overlay_configuration_invalid"),
        "config code: {out}"
    );
    assert!(
        out.contains("netbird.static_peers.vps2"),
        "names the entry: {out}"
    );
    assert!(
        fs::read_to_string(&capture).unwrap_or_default().is_empty(),
        "a contradicting table dials nothing"
    );
}

/// Row 9: a full status that agrees with the table resolves through the status
/// address and serves no `host.governance.inspect` (the daemon pin).
#[test]
fn a_consistent_full_status_dials_without_the_pin_check() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, overlays_toml(&pinned)).expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let full = pin_overlay::status(
        Some(("self.netbird.test", SELF_IP)),
        &[pin_overlay::Peer::new(PEER_FQDN, PEER_IP, "key-one")],
    );
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{full}'\n"),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let served: Served = Arc::new(Mutex::new(Vec::new()));
    serve(listener, Arc::clone(&served), pinned);
    // `POHUNEK_REMOTE_PORT` is the port the CLI dials for the resolved peer.
    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .env("PATH", &bin)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_TEST_CONNECT_CAPTURE", &capture)
        .env("POHUNEK_REMOTE_PORT", port.to_string())
        .args(["--host", PEER_NAME, "session", "list", "--json"])
        .output()
        .expect("run pohunek");
    assert!(output.status.success(), "{}", diagnostics(&output));
    let served = served.lock().expect("served requests");
    assert!(
        !served
            .iter()
            .any(|m| m.as_str() == protocol::method::HOST_GOVERNANCE_INSPECT),
        "no pin check against a status peer: {served:?}"
    );
    assert!(
        served
            .iter()
            .any(|m| m.as_str() == protocol::method::SESSION_LIST),
        "session.list served"
    );
    drop(served);
    let dialed = pin_overlay::dialed_ips(&capture);
    assert!(
        dialed.iter().any(|ip| ip == PEER_IP),
        "the status address 100.64.0.21 is dialled: {dialed:?}"
    );
}

/// Row 3 (CLI side): `pohunek --host vps2 session input s-1 hello --until working`
/// resolves the session through a pinned connection, then dials a dedicated
/// connection for `session.input`; each connection must carry
/// `host.governance.inspect` before any other method, and the dedicated
/// connection must prove the pin before `session.input`.
#[test]
fn a_dedicated_cli_connection_reproves_the_pin() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, overlays_toml(&pinned)).expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{RESTRICTED}'\n"),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let connections: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let reported: Arc<Mutex<String>> = Arc::new(Mutex::new(pinned));
    serve_session_and_track(listener, Arc::clone(&connections), Arc::clone(&reported));
    let mut command = env.command(pohunek_test_support::bin_exe("pohunek"));
    command.env("PATH", &bin);
    command.env("LD_PRELOAD", &library);
    command.env("POHUNEK_TEST_CONNECT_CAPTURE", &capture);
    command.env("POHUNEK_REMOTE_PORT", port.to_string());
    command.args([
        "--host", PEER_NAME, "session", "input", "s-1", "hello", "--json", "--until", "working",
    ]);
    let output = command.output().expect("run pohunek");
    assert!(output.status.success(), "{}", diagnostics(&output));
    let dialed = fs::read_to_string(&capture)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with(PEER_IP))
        .count();
    assert!(
        dialed >= 2,
        "at least two TCP connections to the peer: {dialed}"
    );
    let conns = connections.lock().expect("connections").clone();
    assert!(conns.len() >= 2, "two connections served: {conns:?}");
    for connection in &conns {
        assert_eq!(
            connection.first(),
            Some(&protocol::method::HOST_GOVERNANCE_INSPECT.to_owned()),
            "inspect first on each connection: {connection:?}"
        );
    }
    let input_connection = conns
        .iter()
        .find(|connection| {
            connection
                .iter()
                .any(|m| m == protocol::method::SESSION_INPUT)
        })
        .expect("the dedicated connection serves session.input");
    let input_pos = input_connection
        .iter()
        .position(|m| m == protocol::method::SESSION_INPUT)
        .expect("session.input present");
    assert!(
        input_connection[..input_pos]
            .iter()
            .any(|m| m == protocol::method::HOST_GOVERNANCE_INSPECT),
        "the pin is proven before session.input: {input_connection:?}"
    );
}

/// Row 10 (raw `attach` shape, CLI side): `pohunek --host vps2 attach s-1`
/// runs `PinnedHost::resolve` (pin lookup only, no dial), then a framed
/// control connection that proves the pin and serves `session.attach`,
/// then `attach_raw`, which re-proves the pin on a separate framed
/// connection before opening the raw stream. The fixture reports the
/// pinned id on the first connection and a different id from the second,
/// so the attach fails with `host_identity_mismatch` when `attach_raw`
/// re-proves the pin, before the raw stream opens. This test proves the
/// CLI-level select-and-connect flow through resolution plus the framed
/// pin check; the `attach_raw` pre-verify itself is guarded by the
/// client-crate test `a_mismatching_pin_refuses_the_attach_connection`,
/// where the raw path is reachable without CLI stdio.
#[test]
fn a_raw_attach_by_host_refuses_a_mismatching_pin() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, overlays_toml(&pinned)).expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{RESTRICTED}'\n"),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let served = Served::default();
    serve_first_then_later(listener, Arc::clone(&served), pinned, host_id(b'B'));
    let mut command = env.command(pohunek_test_support::bin_exe("pohunek"));
    command.env("PATH", &bin);
    command.env("LD_PRELOAD", &library);
    command.env("POHUNEK_TEST_CONNECT_CAPTURE", &capture);
    command.env("POHUNEK_REMOTE_PORT", port.to_string());
    command.args(["--host", PEER_NAME, "attach", "s-1"]);
    let output = command.output().expect("run pohunek");
    assert!(
        !output.status.success(),
        "the attach is refused: {}",
        diagnostics(&output)
    );
    let text = diagnostics(&output);
    assert!(
        text.contains("reports host id") && text.contains("overlays.toml pins"),
        "the pin mismatch happened in the pre-verify connection: {text}"
    );
    let calls = served.lock().expect("served requests").clone();
    let inspects = calls
        .iter()
        .filter(|m| *m == protocol::method::HOST_GOVERNANCE_INSPECT)
        .count();
    assert!(
        inspects >= 2,
        "the fresh attach connection proves the pin: {calls:?}"
    );
}

/// Row 8 (cross-check side, CLI): a `session list` action on `--host vps2`
/// under a contradicting full status is refused with
/// `overlay_configuration_invalid` and dials nothing. The host-name route
/// goes through `resolve_selector`, so this proves the cross-check arm in
/// that method; the `resolve_peer` and `resolve_peer_identity` arms are
/// proven in the client crate, where the qualified routes are reachable
/// without stdio. Sibling of `a_contradicting_full_status_is_refused_at_load`.
#[test]
fn a_pinned_route_under_a_contradicting_full_status_is_refused() {
    let pinned = host_id(b'A');
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, overlays_toml(&pinned)).expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    let full_status = pin_overlay::status(
        Some(("self.netbird.test", SELF_IP)),
        &[pin_overlay::Peer::new(PEER_FQDN, "100.64.0.99", "key-one")],
    );
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{full_status}'\n"),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    let mut command = env.command(pohunek_test_support::bin_exe("pohunek"));
    command.env("PATH", &bin);
    command.env("LD_PRELOAD", &library);
    command.env("POHUNEK_TEST_CONNECT_CAPTURE", &capture);
    command.args(["--host", PEER_NAME, "session", "list", "--json"]);
    let output = command.output().expect("run pohunek");
    let text = diagnostics(&output);
    assert!(
        !output.status.success(),
        "the contradictions refuse: {text}"
    );
    assert!(
        text.contains("overlay_configuration_invalid"),
        "the invalid config code: {text}"
    );
    let captured = fs::read_to_string(&capture).unwrap_or_default();
    assert!(captured.is_empty(), "nothing is dialed: {captured}");
}

/// Hermetic scaffold shared by one uniqueness arm: the owner-private
/// directories, `overlays.toml` with `table`, and a fake `netbird` on `PATH`
/// that touches `marker` when consulted.
struct UniquenessSetup {
    env: TestEnv,
    marker: std::path::PathBuf,
    capture: std::path::PathBuf,
    library: std::path::PathBuf,
    bin: std::path::PathBuf,
    file: std::path::PathBuf,
}

fn uniqueness_setup(table: &str) -> UniquenessSetup {
    let env = TestEnv::new().expect("create the hermetic test environment");
    let root = env.root().to_path_buf();
    for directory in ["run/pohunek", "state/pohunek", "config", "home"] {
        fs::create_dir_all(root.join(directory)).expect("create the test directory");
    }
    for directory in ["run/pohunek", "state/pohunek"] {
        fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
            .expect("make the directory owner-private");
    }
    let config_dir = env.config_home().join("pohunek");
    fs::create_dir_all(&config_dir).expect("create the config directory");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    fs::write(&file, table).expect("write overlays.toml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create bin");
    let marker = root.join("marker");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!(
            "#!/bin/sh\ntouch {}\n printf '%s\\n' '{RESTRICTED}'\n",
            marker.display()
        ),
    )
    .expect("write fake netbird");
    let (library, capture) = connect_redirect::build(&root);
    UniquenessSetup {
        env,
        marker,
        capture,
        library,
        bin,
        file,
    }
}

/// Run `pohunek --host vps2 session list --json` in a uniqueness setup and
/// assert the refusal carries `code` and `message` with no provider call
/// and no dial.
fn uniqueness_arm_refuses(setup: &UniquenessSetup, label: &str, code: &str, message: &str) {
    let mut cmd = setup.env.command(pohunek_test_support::bin_exe("pohunek"));
    cmd.env("PATH", &setup.bin);
    cmd.env("LD_PRELOAD", &setup.library);
    cmd.env("POHUNEK_TEST_CONNECT_CAPTURE", &setup.capture);
    cmd.args(["--host", PEER_NAME, "session", "list", "--json"]);
    let out = cmd.output().expect("run pohunek");
    let text = diagnostics(&out);
    assert!(!out.status.success(), "{label} refuses: {text}");
    assert!(text.contains(code), "the config code: {text}");
    assert!(text.contains(message), "the {label} message: {text}");
    assert!(!setup.marker.exists(), "no provider call after rejection");
    let captured = fs::read_to_string(&setup.capture).unwrap_or_default();
    assert!(captured.is_empty(), "nothing dialed: {captured}");
}

/// Row 7: the four uniqueness rules each fail alone: a shared address, a
/// shared fqdn, a shared host id, and a shared short name (`{key, first fqdn
/// label}` sets). Each arm asserts its exact `check_uniqueness` message, so
/// a message change or an arm removal is caught. A shared fqdn only reaches
/// its arm before the selector-name arm, and its message names `share an
/// fqdn` where the selector arm would say `share a selector name`.
#[test]
fn each_uniqueness_rule_fails_alone() {
    // Shared address arm.
    let pinned = host_id(b'A');
    let table = format!(
        "providers = [\"netbird\"]\n[netbird]\nsubnet = \"100.64.0.0/10\"\n[netbird.static_peers.vps2]\naddress = \"{PEER_IP}\"\nfqdn = \"{PEER_FQDN}\"\nhost_id = \"{pinned}\"\n[netbird.static_peers.vps3]\naddress = \"{PEER_IP}\"\nfqdn = \"vps3.remote-box.netbird.test\"\nhost_id = \"host_BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n"
    );
    let setup = uniqueness_setup(&table);
    uniqueness_arm_refuses(
        &setup,
        "shared address",
        "overlay_configuration_invalid",
        "netbird.static_peers.vps2 and netbird.static_peers.vps3 share an address",
    );

    // Shared fqdn arm: equal fqdns imply equal first labels, so the
    // selector-name arm would also reject this file; the fqdn arm fires
    // first and its message (`share an fqdn`) distinguishes it from the
    // selector-name arm (`share a selector name`).
    let table = format!(
        "providers = [\"netbird\"]\n[netbird]\nsubnet = \"100.64.0.0/10\"\n[netbird.static_peers.vps2]\naddress = \"{PEER_IP}\"\nfqdn = \"aaa.test\"\nhost_id = \"{pinned}\"\n[netbird.static_peers.vps3]\naddress = \"100.64.0.22\"\nfqdn = \"aaa.test\"\nhost_id = \"host_BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n"
    );
    fs::write(&setup.file, &table).expect("write overlays.toml");
    uniqueness_arm_refuses(
        &setup,
        "shared fqdn",
        "overlay_configuration_invalid",
        "netbird.static_peers.vps2 and netbird.static_peers.vps3 share an fqdn",
    );

    // Shared host-id arm: distinct addresses, fqdns and short names; the
    // host-id arm fires alone.
    let table = format!(
        "providers = [\"netbird\"]\n[netbird]\nsubnet = \"100.64.0.0/10\"\n[netbird.static_peers.vps2]\naddress = \"{PEER_IP}\"\nfqdn = \"{PEER_FQDN}\"\nhost_id = \"{pinned}\"\n[netbird.static_peers.vps3]\naddress = \"100.64.0.22\"\nfqdn = \"vps3.remote-box.netbird.test\"\nhost_id = \"{pinned}\"\n"
    );
    fs::write(&setup.file, &table).expect("write overlays.toml");
    uniqueness_arm_refuses(
        &setup,
        "shared host id",
        "overlay_configuration_invalid",
        "netbird.static_peers.vps2 and netbird.static_peers.vps3 share a host id",
    );

    // Shared short-name arm.
    let other_pinned = host_id(b'C');
    let table = format!(
        "providers = [\"netbird\"]\n[netbird]\nsubnet = \"100.64.0.0/10\"\n[netbird.static_peers.vps2]\naddress = \"{PEER_IP}\"\nfqdn = \"vps2.remote-box.netbird.test\"\nhost_id = \"{pinned}\"\n[netbird.static_peers.vps3]\naddress = \"100.64.0.22\"\nfqdn = \"vps2.remote-box2.netbird.test\"\nhost_id = \"{other_pinned}\"\n"
    );
    fs::write(&setup.file, &table).expect("write overlays.toml");
    uniqueness_arm_refuses(
        &setup,
        "shared short name",
        "overlay_configuration_invalid",
        "netbird.static_peers.vps2 and netbird.static_peers.vps3 share a selector name",
    );
}
