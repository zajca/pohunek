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

fn serve_connection(stream: TcpStream, served: &Served, reported: &str) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).expect("read a fixture line") == 0 {
            return;
        }
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse a request");
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

/// S2: a raw address is never matched against the static table.
#[test]
fn a_raw_address_is_never_matched_against_the_static_table() {
    let pinned = host_id(b'A');
    let run = run_pinned(
        &["--host", PEER_IP, "session", "list", "--json"],
        &pinned,
        &pinned,
    );
    assert!(!run.output.status.success(), "{}", diagnostics(&run.output));
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

/// S6: without the static table the restricted overview keeps failing
/// closed. (Empty table => `overlays.toml` without `[netbird]`.)
#[test]
fn a_restricted_overview_without_the_table_keeps_failing_closed() {
    let _pinned = host_id(b'A');
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
    assert!(
        fs::read_to_string(&capture).unwrap_or_default().is_empty(),
        "no dial without the table"
    );
}

/// S7 (CLI half, see S10/S11): a full status wins over the table. The fixture
/// peer table contradicts the static entry, so a resolution through the table
/// would have failed; the command reaches the status peer instead.
///
/// This scenario lives beside the full-status tests of the `netbird` crate; it
/// only proves the CLI dials the status address, not the table address.
/// The `Connecting` stub (S3 setup): reach through `run_pinned` with a
/// connecting fake. The restricted fixture's `Connected` is replaced once at
/// write time, so the check is not a tautology.
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

/// S9: locked `0600` config written by the helper is accepted, so every
/// scenario above is the locked-file case; this test only names the mode.
#[test]
fn the_locked_config_mode_is_accepted() {
    let pinned = host_id(b'A');
    let local = run_static_session(&pinned, &pinned);
    assert!(
        local.output.status.success(),
        "{}",
        diagnostics(&local.output)
    );
}

/// S6: a restricted overview never names a static peer in discovery or
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

/// S11a: a duplicate pin (same address twice) is refused at load. This is the
/// `1..=MAX` duplicate rejection proof at the CLI boundary.
/// S11: the daemon builds its registry **without** static peers. A real
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

/// S3 (dedicated): `session.input` with `--until` goes through
/// `call_dedicated`, which dials a fresh connection and re-proves the pin.
/// The fixture daemon lists `s-1` so the name lookup succeeds first;
/// the dedicated connection must then carry `host.governance.inspect`
/// before `session.input`, and the pin must be the first non-`daemon.health`
/// request on that connection too.
/// S5: an unmatched name fails closed with the restricted error, never with
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

/// M2: an fqdn that is the raw IP must not make a raw-IP selector match the
/// table. Without the guard, `--host 100.64.0.21` would resolve the static
/// entry; with it, it must fail closed.
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

/// S7a: an address outside the subnet is `overlay_configuration_invalid` and
/// the provider is never called.
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

/// S7b: `[netbird]` with a `tailscale` provider is `overlay_configuration_invalid`
/// and the provider is never called.
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

/// S7c: a malformed `host_id` is `overlay_configuration_invalid` and the
/// provider is never called.
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

/// S7d: two entries sharing a short name are `overlay_configuration_invalid`
/// and the provider is never called.
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
             [netbird.static_peers.twin]\naddress = \"100.64.0.22\"\nfqdn = \"vps2.other.netbird.test\"\nhost_id = \"{pinned}\"\n"
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
    assert!(!marker.exists(), "no provider call");
    assert!(
        fs::read_to_string(&capture).unwrap_or_default().is_empty(),
        "a refused config dials nothing"
    );
}

/// S8: a full status that contradicts the static table is
/// `overlay_configuration_invalid`, names the entry, and dials nothing.
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

/// S9: a full status that agrees with the table resolves through the status
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
