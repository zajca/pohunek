//! Binary-level scenarios of the Tailscale overlay on the client side.
//!
//! Each scenario runs the real `pohunek` binary with `providers =
//! ["tailscale"]` against a fake `tailscale` that serves a document derived from
//! `ipnstate.Status` of `tailscale/tailscale@v1.104.2` (see
//! `support/tailscale_status.rs`). A `connect()` interposer records the
//! address a selector leads to, IPv4 and IPv6, and sends the connection to a
//! closed loopback port, so a resolved peer ends in `host_unreachable` and a
//! rejected one in `host_unknown`.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Output;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use pohunek_test_support::env::TestEnv;
use serde_json::Value;

#[path = "support/connect_redirect.rs"]
mod connect_redirect;
#[path = "support/tailscale_status.rs"]
mod tailscale_status;

use tailscale_status::{FakeTailscale, Node, Status};

/// The port `overlays.toml` configures in these scenarios.
const PORT: u16 = 9100;

/// This host's address in the fixtures.
const SELF_ADDRESS: &str = "100.64.0.7";

/// A hermetic home with `providers = ["tailscale"]`, a fake `tailscale` and the
/// connect capture.
struct Fixture {
    env: TestEnv,
    fake: FakeTailscale,
    redirect: PathBuf,
    capture: PathBuf,
}

impl Fixture {
    fn new(status: &Status) -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let fake = FakeTailscale::new(env.root());
        fake.install(status);
        let dir = env.config_home().join("pohunek");
        fs::create_dir_all(&dir).expect("create the config directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .expect("make the config directory private");
        let file = dir.join("overlays.toml");
        fs::write(
            &file,
            format!("providers = [\"tailscale\"]\nport = {PORT}\n"),
        )
        .expect("write overlays.toml");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
            .expect("make overlays.toml private");
        let (redirect, capture) = connect_redirect::build(env.root());
        Self {
            env,
            fake,
            redirect,
            capture,
        }
    }

    /// Rewrites `overlays.toml` with the provider, the port and `extra` keys.
    fn configure(&self, extra: &str) {
        let file = self.env.config_home().join("pohunek").join("overlays.toml");
        fs::write(
            &file,
            format!("providers = [\"tailscale\"]\nport = {PORT}\n{extra}\n"),
        )
        .expect("rewrite overlays.toml");
    }

    /// Starts the CLI without waiting for it.
    fn spawn(&self, args: &[&str]) -> std::process::Child {
        self.env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .env("PATH", self.fake.bin_dir())
            .env("LD_PRELOAD", &self.redirect)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &self.capture)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("start the pohunek CLI")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .env("PATH", self.fake.bin_dir())
            .env("LD_PRELOAD", &self.redirect)
            .env("POHUNEK_TEST_CONNECT_CAPTURE", &self.capture)
            .args(args)
            .output()
            .expect("run the pohunek CLI")
    }

    /// Runs the CLI with `path` as its whole `PATH`.
    fn run_on_path(&self, path: &std::path::Path, args: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("pohunek"))
            .env("PATH", path)
            .args(args)
            .output()
            .expect("run the pohunek CLI")
    }

    /// Runs `--host <selector> session list` and returns the output with the
    /// overlay targets the run dialed, then forgets them.
    fn dial(&self, selector: &str) -> (Output, Vec<String>) {
        let _ = fs::remove_file(&self.capture);
        let output = self.run(&["--host", selector, "session", "list", "--json"]);
        let targets = fs::read_to_string(&self.capture)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        (output, targets)
    }
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is one JSON document ({error}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn error_code(output: &Output) -> String {
    let document = json(output);
    document["err"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("a typed failure: {document}"))
        .to_owned()
}

fn error_message(output: &Output) -> String {
    json(output)["err"]["msg"]
        .as_str()
        .expect("error message")
        .to_owned()
}

/// The records of a listing, `local` ones apart.
fn records(output: &Output) -> (Vec<Value>, Option<Value>) {
    assert!(
        output.status.success(),
        "the listing failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let all = json(output)["ok"].as_array().expect("host records").clone();
    let local = all
        .iter()
        .find(|record| record.get("local").is_some())
        .cloned();
    let peers = all
        .into_iter()
        .filter(|record| record.get("local").is_none())
        .collect();
    (peers, local)
}

fn record<'a>(peers: &'a [Value], name: &str) -> &'a Value {
    peers
        .iter()
        .find(|peer| peer["name"] == name)
        .unwrap_or_else(|| panic!("{name} is listed: {peers:?}"))
}

fn names(peers: &[Value]) -> Vec<&str> {
    let mut names = peers
        .iter()
        .map(|peer| peer["name"].as_str().expect("peer name"))
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

/// The typed selector of a stable node identity.
fn peer_selector(id: &str) -> String {
    format!("tailscale:peer~{}", URL_SAFE_NO_PAD.encode(id))
}

fn tailnet() -> Status {
    Status::running(
        SELF_ADDRESS,
        vec![
            Node::peer(
                "alpha",
                "nALPHA00001CNTRL",
                &["100.64.0.10", "fd7a:115c:a1e0::10"],
            )
            .with_host_name("alpha-laptop"),
            Node::peer("bravo", "nBRAVO00001CNTRL", &["fd7a:115c:a1e0::20"]),
            Node::peer("charlie", "nCHARL00001CNTRL", &["100.64.0.30"]).tagged("tag:server"),
            Node::peer("delta", "nDELTA00001CNTRL", &["100.64.0.40"]).offline(),
        ],
    )
}

/// Listing and discovery show the selectable peers of a normal tailnet with
/// their identity, `MagicDNS` name and dial address.
#[test]
fn a_normal_tailnet_is_listed_with_the_dial_addresses() {
    let fixture = Fixture::new(&tailnet());
    for command in ["list", "discover"] {
        let (peers, local) = records(&fixture.run(&["host", command, "--refresh", "--json"]));
        assert_eq!(
            names(&peers),
            ["alpha", "bravo", "charlie", "delta"],
            "{command}: the display name is the first MagicDNS label, never HostName"
        );
        for peer in &peers {
            assert_eq!(peer["overlay"], "tailscale", "{command}");
            assert_eq!(peer["port"], PORT, "{command}");
        }
        let alpha = record(&peers, "alpha");
        assert_eq!(
            alpha["address"], "100.64.0.10",
            "IPv4 is preferred: {alpha}"
        );
        assert_eq!(alpha["peer_id"], "nALPHA00001CNTRL");
        assert_eq!(alpha["fqdn"], "alpha.tail-test.ts.net", "no trailing dot");
        assert_eq!(
            record(&peers, "bravo")["address"],
            "fd7a:115c:a1e0::20",
            "an IPv6-only peer is listed on its IPv6 address"
        );
        assert_eq!(
            record(&peers, "charlie")["address"],
            "100.64.0.30",
            "a tagged peer is selectable like any other"
        );
        let delta = record(&peers, "delta");
        assert_eq!(
            delta["address"],
            Value::Null,
            "an offline peer is not probed"
        );
        assert_eq!(delta["classification"], "candidate");
        let local = local.unwrap_or_else(|| panic!("{command}: this host is listed"));
        assert_eq!(local["name"], "self-box");
        assert_eq!(local["address"], SELF_ADDRESS);
        assert_eq!(local["peer_id"], Value::Null);
    }
}

/// Every selector form leads to the same peer and the same dial address, on
/// IPv4 and on IPv6.
#[test]
fn every_selector_form_dials_the_peers_address() {
    let fixture = Fixture::new(&tailnet());
    let alpha = "100.64.0.10:9100";
    for selector in [
        "alpha",
        "ALPHA",
        "alpha.tail-test.ts.net",
        "Alpha.Tail-Test.TS.net.",
        "tailscale:alpha",
        "tailscale:alpha.tail-test.ts.net",
        "nALPHA00001CNTRL",
        "tailscale:nALPHA00001CNTRL",
        "100.64.0.10",
        "fd7a:115c:a1e0::10",
        &peer_selector("nALPHA00001CNTRL"),
    ] {
        let (output, targets) = fixture.dial(selector);
        assert_eq!(error_code(&output), "host_unreachable", "{selector}");
        assert_eq!(targets, [alpha], "{selector} dials the peer's IPv4 address");
    }
    for selector in ["bravo", "tailscale:bravo", "fd7a:115c:a1e0::20"] {
        let (output, targets) = fixture.dial(selector);
        assert_eq!(error_code(&output), "host_unreachable", "{selector}");
        assert_eq!(
            targets,
            ["[fd7a:115c:a1e0::20]:9100"],
            "{selector} dials the IPv6-only peer on IPv6"
        );
    }
    let (output, targets) = fixture.dial("charlie");
    assert_eq!(error_code(&output), "host_unreachable");
    assert_eq!(targets, ["100.64.0.30:9100"]);
    // An offline peer named explicitly is still resolved and dialed.
    let (output, targets) = fixture.dial("delta");
    assert_eq!(error_code(&output), "host_unreachable");
    assert_eq!(targets, ["100.64.0.40:9100"]);
}

/// A peer the owner must not route to is not listed, and no selector reaches
/// it, whatever address it advertises.
#[test]
fn ineligible_peers_are_neither_listed_nor_selectable() {
    let status = Status::running(
        SELF_ADDRESS,
        vec![
            Node::peer("good", "nGOOD000001CNTRL", &["100.64.0.10"]),
            Node::peer("shared", "nSHARE00001CNTRL", &["100.64.0.11"]).sharee(),
            Node::peer("expired", "nEXPIR00001CNTRL", &["100.64.0.12"]).expired(),
            Node::peer("gone", "nGONE000001CNTRL", &["100.64.0.13"]).out_of_netmap(),
            Node::peer("chromeos", "nCHROM00001CNTRL", &["100.115.92.5"]),
            Node::peer("service", "nSERVI00001CNTRL", &["100.100.100.100"]),
            Node::peer("lan", "nLAN0000001CNTRL", &["10.0.0.9"]),
            Node::peer("loopback", "nLOOP000001CNTRL", &["127.0.0.1"]),
            Node::peer("noaddress", "nNOADD00001CNTRL", &[]),
            Node::peer("via6", "nVIA6000001CNTRL", &["fd7a:115c:a1e0:b1a::5"]),
            Node::peer("service6", "nSERV600001CNTRL", &["fd7a:115c:a1e0::53"]),
            Node::peer("offlan", "nOFFLA00001CNTRL", &["10.0.0.9"]).offline(),
            Node::peer("offchrome", "nOFFCH00001CNTRL", &["100.115.92.6"]).offline(),
        ],
    );
    let fixture = Fixture::new(&status);
    let (peers, _) = records(&fixture.run(&["host", "list", "--refresh", "--json"]));
    assert_eq!(
        names(&peers),
        ["good"],
        "ineligible peers and peers without an in-range address are dropped, online or not"
    );
    assert_eq!(record(&peers, "good")["address"], "100.64.0.10");

    for selector in [
        "shared",
        "expired",
        "gone",
        "chromeos",
        "service",
        "lan",
        "loopback",
        "noaddress",
        "via6",
        "service6",
        "offlan",
        "offchrome",
        "nSHARE00001CNTRL",
        "tailscale:shared",
        &peer_selector("nEXPIR00001CNTRL"),
        // Listed for the sharee only: the address check is per peer.
        "100.64.0.11",
        "100.115.92.5",
        "100.100.100.100",
        "fd7a:115c:a1e0:b1a::5",
        "fd7a:115c:a1e0::53",
        // Listed for no peer.
        "100.64.0.99",
        "127.0.0.1",
    ] {
        let (output, targets) = fixture.dial(selector);
        assert_eq!(error_code(&output), "host_unknown", "{selector}");
        assert!(
            targets.is_empty(),
            "{selector} is never dialed: {targets:?}"
        );
    }
}

/// A shared `HostName` never selects a peer, and an unknown name is unknown.
#[test]
fn a_duplicate_host_name_selects_nothing() {
    let status = Status::running(
        SELF_ADDRESS,
        vec![
            Node::peer("alpha", "nALPHA00001CNTRL", &["100.64.0.10"]).with_host_name("laptop"),
            Node::peer("bravo", "nBRAVO00001CNTRL", &["100.64.0.20"]).with_host_name("laptop"),
        ],
    );
    let fixture = Fixture::new(&status);
    let (peers, _) = records(&fixture.run(&["host", "list", "--refresh", "--json"]));
    assert_eq!(names(&peers), ["alpha", "bravo"]);
    let (output, targets) = fixture.dial("laptop");
    assert_eq!(error_code(&output), "host_unknown");
    assert!(targets.is_empty(), "{targets:?}");
    let (output, targets) = fixture.dial("bravo");
    assert_eq!(error_code(&output), "host_unreachable");
    assert_eq!(targets, ["100.64.0.20:9100"]);
}

/// The node identity survives a re-key and an address change; the map key does
/// not matter.
#[test]
fn a_rekeyed_peer_keeps_its_identity() {
    let before = Status::running(
        SELF_ADDRESS,
        vec![Node::peer("alpha", "nALPHA00001CNTRL", &["100.64.0.10"])],
    );
    let fixture = Fixture::new(&before);
    let selector = peer_selector("nALPHA00001CNTRL");
    let (peers, _) = records(&fixture.run(&["host", "list", "--refresh", "--json"]));
    assert_eq!(record(&peers, "alpha")["peer_id"], "nALPHA00001CNTRL");
    let (_, targets) = fixture.dial(&selector);
    assert_eq!(targets, ["100.64.0.10:9100"]);

    let after = Status::running(
        SELF_ADDRESS,
        vec![Node::peer("alpha", "nALPHA00001CNTRL", &["100.64.0.11"]).rekeyed()],
    );
    fixture.fake.set_status(&after);
    let (peers, _) = records(&fixture.run(&["host", "list", "--refresh", "--json"]));
    let alpha = record(&peers, "alpha");
    assert_eq!(alpha["peer_id"], "nALPHA00001CNTRL");
    assert_eq!(alpha["address"], "100.64.0.11");
    let (_, targets) = fixture.dial(&selector);
    assert_eq!(
        targets,
        ["100.64.0.11:9100"],
        "the same identity follows the new address"
    );
}

/// A state that cannot carry overlay traffic is reported as such and nothing
/// is dialed: in userspace mode or before login a `100.64.0.0/10` dial would
/// leave through the default route.
#[test]
fn an_unusable_backend_state_fails_closed_without_dialing() {
    let cases: [(&str, Status, &str); 8] = [
        (
            "NeedsLogin",
            tailnet().with_state("NeedsLogin"),
            "NeedsLogin",
        ),
        (
            "InUseOtherUser",
            tailnet().with_state("InUseOtherUser"),
            "InUseOtherUser",
        ),
        (
            "NeedsMachineAuth",
            tailnet().with_state("NeedsMachineAuth"),
            "NeedsMachineAuth",
        ),
        ("Stopped", tailnet().with_state("Stopped"), "Stopped"),
        ("Starting", tailnet().with_state("Starting"), "Starting"),
        ("NoState", tailnet().with_state("NoState"), "NoState"),
        (
            "a state from a newer release",
            tailnet().with_state("FutureState"),
            "unrecognized",
        ),
        (
            "userspace networking",
            tailnet().userspace(),
            "userspace networking",
        ),
    ];
    for (label, status, detail) in cases {
        let fixture = Fixture::new(&status);
        let output = fixture.run(&["host", "list", "--refresh", "--json"]);
        assert_eq!(
            error_code(&output),
            "tailscale_state_unavailable",
            "{label}"
        );
        let message = error_message(&output);
        assert!(message.contains(detail), "{label}: {message}");
        assert!(
            !message.contains("FutureState"),
            "{label}: an unknown state is not echoed: {message}"
        );
        let (output, targets) = fixture.dial("alpha");
        assert_eq!(
            error_code(&output),
            "tailscale_state_unavailable",
            "{label}"
        );
        assert!(
            targets.is_empty(),
            "{label}: nothing is dialed: {targets:?}"
        );
    }
}

/// The redacted `tailscale status --json` of a real kernel-TUN node (Tailscale
/// 1.104.2 against Headscale, from the #1112 acceptance run) drives the same
/// listing, selector, dial and doctor paths as the spec-derived documents.
#[test]
fn a_recording_of_a_real_kernel_tun_node_is_parsed_and_routed() {
    let recording: Value = serde_json::from_str(include_str!(
        "fixtures/tailscale-status-linux-kernel-tun.json"
    ))
    .expect("the recording is JSON");
    let fixture = Fixture::new(&tailnet());
    // The CLI prints one line, so the recording is compacted.
    fixture.fake.set_status_text(&recording.to_string());

    for command in ["list", "discover"] {
        let (peers, local) = records(&fixture.run(&["host", command, "--refresh", "--json"]));
        assert_eq!(names(&peers), ["ph-b"], "{command}");
        let peer = record(&peers, "ph-b");
        assert_eq!(peer["overlay"], "tailscale", "{command}");
        assert_eq!(peer["peer_id"], "2", "{command}");
        assert_eq!(
            peer["fqdn"], "ph-b.ts.lab.internal",
            "{command}: no trailing dot"
        );
        assert_eq!(
            peer["address"], "100.64.0.2",
            "{command}: IPv4 is preferred"
        );
        let local = local.unwrap_or_else(|| panic!("{command}: this host is listed"));
        assert_eq!(local["name"], "ph-a", "{command}");
        assert_eq!(local["address"], "100.64.0.1", "{command}");
    }
    for selector in [
        "ph-b",
        "ph-b.ts.lab.internal",
        "tailscale:ph-b",
        "2",
        "100.64.0.2",
        &peer_selector("2"),
    ] {
        let (output, targets) = fixture.dial(selector);
        assert_eq!(error_code(&output), "host_unreachable", "{selector}");
        assert_eq!(targets, [format!("100.64.0.2:{PORT}")], "{selector}");
    }

    let output = fixture.run(&["doctor", "--json"]);
    let document = json(&output);
    let checks = document["ok"]["checks"].as_array().expect("doctor checks");
    let cli = checks
        .iter()
        .find(|check| check["name"] == "tailscale_cli")
        .expect("tailscale_cli check");
    assert_eq!(cli["status"], "ok", "{cli}");
    assert!(
        cli["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("100.64.0.1")),
        "{cli}"
    );
}

/// The doctor names the reason the node has no usable address: userspace
/// networking, a backend that is not running, or no IPv4 address.
#[test]
fn the_doctor_names_why_the_node_has_no_listener_address() {
    let ipv6_only = Node::peer("self-box", "nSELF00001CNTRL", &["fd7a:115c:a1e0::7"]);
    let cases: [(&str, Status, &str); 3] = [
        (
            "userspace networking",
            tailnet().userspace(),
            "userspace networking",
        ),
        (
            "a stopped node",
            tailnet().with_state("Stopped"),
            "not running (state: Stopped)",
        ),
        (
            "no IPv4 address",
            tailnet().with_this(ipv6_only),
            "no IPv4 node address",
        ),
    ];
    for (label, status, reason) in cases {
        let fixture = Fixture::new(&status);
        let output = fixture.run(&["doctor", "--json"]);
        let document = json(&output);
        let checks = document["ok"]["checks"]
            .as_array()
            .unwrap_or_else(|| panic!("{label}: doctor checks in {document}"));
        let check = checks
            .iter()
            .find(|check| check["name"] == "tailscale_cli")
            .unwrap_or_else(|| panic!("{label}: tailscale_cli in {checks:?}"));
        assert_eq!(check["status"], "warn", "{label}: {check}");
        let detail = check["detail"].as_str().expect("check detail");
        assert!(detail.contains(reason), "{label}: {detail}");
    }
}

/// A missing, failing or garbled CLI each has a distinct, non-guessing result.
#[test]
fn a_missing_or_failing_cli_is_reported_by_cause() {
    let fixture = Fixture::new(&tailnet());
    fs::remove_file(fixture.fake.bin_dir().join("tailscale")).expect("remove the fake");
    let output = fixture.run(&["host", "list", "--refresh", "--json"]);
    assert_eq!(error_code(&output), "tailscale_cli_missing");
    let message = error_message(&output);
    assert!(message.contains("not found on PATH"), "{message}");
    assert!(!message.contains("not trusted"), "{message}");
    let (output, targets) = fixture.dial("alpha");
    assert_eq!(error_code(&output), "tailscale_cli_missing");
    assert!(targets.is_empty(), "{targets:?}");

    // A candidate that exists in a directory others can write is named, with
    // the reason, while the code stays the same.
    let loose = fixture.env.root().join("loose");
    fs::create_dir(&loose).expect("create the loose directory");
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o775))
        .expect("make the directory group-writable");
    let candidate = loose.join("tailscale");
    pohunek_test_support::fs::write_executable(&candidate, "#!/bin/sh\nexit 0\n")
        .expect("write the candidate");
    let output = fixture.run_on_path(&loose, &["host", "list", "--refresh", "--json"]);
    assert_eq!(error_code(&output), "tailscale_cli_missing");
    let message = error_message(&output);
    assert!(
        message.contains(&candidate.display().to_string()) && message.contains("not trusted"),
        "the untrusted candidate is named: {message}"
    );
    let output = fixture.run_on_path(&loose, &["doctor", "--json"]);
    let document = json(&output);
    let detail = document["ok"]["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == "tailscale_cli")
        .expect("tailscale_cli check")["detail"]
        .as_str()
        .expect("detail")
        .to_owned();
    assert!(
        detail.contains(&candidate.display().to_string()) && detail.contains("not trusted"),
        "the doctor names the candidate: {detail}"
    );
    fs::remove_dir_all(&loose).expect("remove the loose directory");

    pohunek_test_support::fs::write_executable(
        fixture.fake.bin_dir().join("tailscale"),
        "#!/bin/sh\nprintf 'failed to connect to local tailscaled\\n' >&2\nexit 1\n",
    )
    .expect("write the failing fake");
    let output = fixture.run(&["host", "list", "--refresh", "--json"]);
    assert_eq!(error_code(&output), "tailscale_state_unavailable");
    assert!(
        error_message(&output).contains("failed to connect to local tailscaled"),
        "the CLI's own reason is carried: {}",
        error_message(&output)
    );

    for garbage in ["not json", "", "[1, 2]", "{\"Peer\": 7}"] {
        pohunek_test_support::fs::write_executable(
            fixture.fake.bin_dir().join("tailscale"),
            format!("#!/bin/sh\nprintf '%s\\n' '{garbage}'\n"),
        )
        .expect("write the garbling fake");
        let output = fixture.run(&["host", "list", "--refresh", "--json"]);
        assert_eq!(
            error_code(&output),
            "tailscale_state_unavailable",
            "output {garbage:?}"
        );
    }
}

/// A tailnet without peers, and a document that carries `null` for every Go
/// `nil` and fields this release does not know, are valid input.
#[test]
fn null_lists_and_unknown_fields_are_tolerated() {
    let fixture = Fixture::new(&Status::running(SELF_ADDRESS, vec![]));
    let (peers, local) = records(&fixture.run(&["host", "list", "--refresh", "--json"]));
    assert!(peers.is_empty(), "{peers:?}");
    assert_eq!(local.expect("this host")["address"], SELF_ADDRESS);

    fixture.fake.set_status_text(
        r#"{"TUN":true,"BackendState":"Running","FutureField":{"a":[1,2,3]},"Self":null,"Peer":{"nodekey:aa":null,"nodekey:bb":{"ID":"nBBB","DNSName":"bbb.tail-test.ts.net.","TailscaleIPs":["100.64.0.20"],"InNetworkMap":true,"Online":true,"Tags":null,"FutureFlag":true}}}"#,
    );
    let (peers, local) = records(&fixture.run(&["host", "list", "--refresh", "--json"]));
    assert_eq!(names(&peers), ["bbb"]);
    assert_eq!(record(&peers, "bbb")["address"], "100.64.0.20");
    assert!(local.is_none(), "a null Self has no local record");
}

/// A name that this machine and a peer both carry is ambiguous, never a silent
/// choice.
#[test]
fn a_name_shared_with_this_machine_is_ambiguous() {
    let status = Status::running(
        SELF_ADDRESS,
        vec![
            Node::peer("self-box", "nOTHER00001CNTRL", &["100.64.0.10"]).with_host_name("self-box")
        ],
    );
    let fixture = Fixture::new(&status);
    let (output, targets) = fixture.dial("self-box");
    assert_eq!(error_code(&output), "overlay_host_ambiguous");
    assert!(targets.is_empty(), "{targets:?}");
}

/// Replaces the fake CLI with one that records its pid and never answers.
fn install_hanging_cli(fixture: &Fixture) -> PathBuf {
    let pid = fixture.env.root().join("tailscale.pid");
    pohunek_test_support::fs::write_executable(
        fixture.fake.bin_dir().join("tailscale"),
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{pid}'\n/bin/sleep 3600 &\nprintf '%s' \"$!\" > '{pid}.child'\nwait\n",
            pid = pid.display()
        ),
    )
    .expect("write the hanging fake");
    pid
}

/// Waits until the process whose pid `file` records is gone or only a zombie
/// awaiting its reaper.
///
/// `SIGKILL` is delivered asynchronously: the supervisor is reaped as soon as
/// it dies, while another member of its process group can still be on its way
/// out, so the end of the process is awaited rather than read once.
fn assert_not_running(file: &std::path::Path) {
    let recorded = fs::read_to_string(file).expect("the fake recorded its pid");
    let stat_path = std::path::Path::new("/proc")
        .join(recorded.trim())
        .join("stat");
    pohunek_test_support::wait::poll_until(
        &format!("the process of {} to end", file.display()),
        || {
            let state = fs::read_to_string(&stat_path)
                .ok()
                .and_then(|text| text.rsplit(')').next().map(str::to_owned))
                .and_then(|rest| rest.split_whitespace().next().map(str::to_owned));
            state.is_none_or(|state| state == "Z").then_some(())
        },
    );
}

/// A status probe that never answers ends at the configured deadline as
/// `tailscale_state_unavailable` for listing and for `--host` resolution, and
/// the CLI it started is gone.
#[test]
fn a_status_probe_that_never_answers_is_bounded_and_killed() {
    let fixture = Fixture::new(&tailnet());
    // timing-allowed: #1565 the one-second status deadline expiring is the behaviour under test.
    fixture.configure("status_timeout_secs = 1");
    let pid = install_hanging_cli(&fixture);

    let output = fixture.run(&["host", "list", "--refresh", "--json"]);
    assert_eq!(error_code(&output), "tailscale_state_unavailable");
    assert!(
        error_message(&output).contains("did not answer within"),
        "{}",
        error_message(&output)
    );
    for file in [pid.clone(), pid.with_extension("pid.child")] {
        assert_not_running(&file);
    }

    // A CLI that exits but leaves a helper holding its output is bounded and
    // its helper is killed too.
    let helper = fixture.env.root().join("tailscale.helper");
    let runs = fixture.env.root().join("tailscale.runs");
    pohunek_test_support::fs::write_executable(
        fixture.fake.bin_dir().join("tailscale"),
        format!(
            "#!/bin/sh\nprintf x >> '{runs}'\n/bin/sleep 3600 &\nprintf '%s' \"$!\" > '{helper}'\nexit 0\n",
            runs = runs.display(),
            helper = helper.display()
        ),
    )
    .expect("write the leaking fake");
    let output = fixture.run(&["host", "list", "--refresh", "--json"]);
    assert_eq!(error_code(&output), "tailscale_state_unavailable");
    assert_not_running(&helper);

    // A `--host` command probes the provider to learn whether the name is this
    // machine and again to resolve the peer: two probes, never more.
    let before = fs::read_to_string(&runs).expect("runs so far").len();
    let (output, targets) = fixture.dial("alpha");
    assert_eq!(error_code(&output), "tailscale_state_unavailable");
    assert!(targets.is_empty(), "{targets:?}");
    let after = fs::read_to_string(&runs).expect("runs so far").len();
    assert_eq!(after - before, 2, "the status probe ran twice per command");
}

/// The deadline is a strict, range-checked `overlays.toml` key.
#[test]
fn an_unusable_status_deadline_is_refused_before_any_probe() {
    let fixture = Fixture::new(&tailnet());
    for value in ["0", "121", "-1", "\"fast\"", "1.5"] {
        fixture.configure(&format!("status_timeout_secs = {value}"));
        let output = fixture.run(&["host", "list", "--refresh", "--json"]);
        assert_eq!(
            error_code(&output),
            "overlay_configuration_invalid",
            "{value}"
        );
        assert!(
            error_message(&output).contains("status_timeout_secs")
                || error_message(&output).contains("schema"),
            "{value}: {}",
            error_message(&output)
        );
        assert_eq!(fixture.fake.invocations(), 0, "{value}");
    }
}

/// A node address that two peers list is no route: the selectors that reach it
/// are ambiguous between both peers, the listing keeps both as candidates and
/// names why, and a peer with its own address is unaffected.
#[test]
fn an_address_listed_for_two_peers_is_not_used_for_routing() {
    let status = Status::running(
        SELF_ADDRESS,
        vec![
            Node::peer("alpha", "nALPHA00001CNTRL", &["100.64.0.10"]),
            Node::peer("bravo", "nBRAVO00001CNTRL", &["100.64.0.10"]),
            Node::peer("charlie", "nCHARL00001CNTRL", &["100.64.0.30"]),
            // Shares only its IPv6 address with `echo`, and the address of
            // this machine with `foxtrot`.
            Node::peer(
                "delta",
                "nDELTA00001CNTRL",
                &["100.64.0.40", "fd7a:115c:a1e0::40"],
            ),
            Node::peer("echo", "nECHO000001CNTRL", &["fd7a:115c:a1e0::40"]),
            Node::peer("foxtrot", "nFOXTR00001CNTRL", &[SELF_ADDRESS]),
        ],
    );
    let fixture = Fixture::new(&status);
    let (peers, _) = records(&fixture.run(&["host", "list", "--refresh", "--json"]));
    for name in ["alpha", "bravo", "delta", "echo", "foxtrot"] {
        let entry = record(&peers, name);
        assert!(entry["address"].is_null(), "{name}: {entry}");
        assert_eq!(entry["classification"], "candidate", "{name}");
        assert_eq!(entry["reason"], "address_ambiguous", "{name}");
    }
    assert_eq!(record(&peers, "charlie")["address"], "100.64.0.30");

    for selector in ["alpha", "bravo", "100.64.0.10", "echo", "delta", "foxtrot"] {
        let (output, targets) = fixture.dial(selector);
        assert_eq!(error_code(&output), "overlay_host_ambiguous", "{selector}");
        assert!(targets.is_empty(), "{selector}: {targets:?}");
        let message = error_message(&output);
        let named = match selector {
            "alpha" | "bravo" | "100.64.0.10" => vec!["alpha", "bravo"],
            "echo" | "delta" => vec!["delta", "echo"],
            _ => vec!["foxtrot", "this machine"],
        };
        for name in named {
            assert!(message.contains(name), "{selector}: {name} in {message}");
        }
    }
    let (output, targets) = fixture.dial("charlie");
    assert_eq!(error_code(&output), "host_unreachable");
    assert_eq!(targets, ["100.64.0.30:9100"]);
}

/// Replaces the fake CLI with one that starts a helper holding the write end
/// of the named pipe `alive`. Opening the pipe for reading returns once the
/// helper exists, so a signal sent afterwards cannot miss it, and the pipe
/// reads as end of file once the helper is gone, with no polling.
fn install_pipe_holding_cli(fixture: &Fixture) -> PathBuf {
    let alive = fixture.env.root().join("tailscale.alive");
    nix::unistd::mkfifo(&alive, nix::sys::stat::Mode::from_bits_truncate(0o600))
        .expect("create the liveness pipe");
    pohunek_test_support::fs::write_executable(
        fixture.fake.bin_dir().join("tailscale"),
        format!(
            "#!/bin/sh\n/bin/sleep 3600 9> '{alive}' &\nwait\n",
            alive = alive.display()
        ),
    )
    .expect("write the pipe-holding fake");
    alive
}

/// Blocks until every holder of the write end of `alive` is gone, for at most
/// [`pohunek_test_support::wait::HANG_GUARD`].
fn read_until_released(mut alive: fs::File) {
    let (done, released) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let read = std::io::copy(&mut alive, &mut std::io::sink());
        let _ = done.send(read.map(|_bytes| ()));
    });
    released
        .recv_timeout(pohunek_test_support::wait::HANG_GUARD)
        .expect("a process of the hung CLI still holds the liveness pipe")
        .expect("read the liveness pipe to its end");
}

/// A status deadline longer than the 5 s connect timeout ends the probe as
/// `tailscale_state_unavailable`: the resolution budget follows the status
/// deadline.
#[test]
fn host_resolution_waits_for_a_status_deadline_longer_than_the_connect_timeout() {
    let fixture = Fixture::new(&tailnet());
    // timing-allowed: #1565 the deadline versus the 5 s connect timeout is the behaviour under test and runs in a separate binary.
    fixture.configure("status_timeout_secs = 6");
    install_hanging_cli(&fixture);
    let (output, targets) = fixture.dial("alpha");
    assert_eq!(error_code(&output), "tailscale_state_unavailable");
    assert!(targets.is_empty(), "{targets:?}");
}

/// Interrupting the CLI during a status probe takes the provider and the
/// helper it started down with it.
#[test]
fn an_interrupted_command_leaves_no_provider_process() {
    let fixture = Fixture::new(&tailnet());
    fixture.configure("status_timeout_secs = 120");
    let alive = install_pipe_holding_cli(&fixture);
    let mut child = fixture.spawn(&["host", "list", "--refresh", "--json"]);
    // Opening the read end returns once the helper holds the write end.
    let reader = fs::File::open(&alive).expect("open the liveness pipe");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("pid")),
        nix::sys::signal::Signal::SIGINT,
    )
    .expect("interrupt the CLI");
    child.wait().expect("the CLI ends");
    read_until_released(reader);
}

/// A CLI that exited and left a helper holding its output does not outlive an
/// interrupted command: the helper is still running while the command drains
/// that output, and goes down with the command.
#[test]
fn an_interrupted_command_reaps_a_helper_that_outlives_the_provider() {
    let fixture = Fixture::new(&tailnet());
    fixture.configure("status_timeout_secs = 120");
    let exited = fixture.env.root().join("tailscale.exited");
    let alive = fixture.env.root().join("tailscale.alive");
    for pipe in [&exited, &alive] {
        nix::unistd::mkfifo(pipe, nix::sys::stat::Mode::from_bits_truncate(0o600))
            .expect("create a liveness pipe");
    }
    // `exited` is held by the CLI alone, `alive` by its helper alone.
    pohunek_test_support::fs::write_executable(
        fixture.fake.bin_dir().join("tailscale"),
        format!(
            "#!/bin/sh\nexec 8> '{exited}'\n/bin/sleep 3600 8>&- 9> '{alive}' &\nexit 0\n",
            exited = exited.display(),
            alive = alive.display()
        ),
    )
    .expect("write the leaking fake");
    let mut child = fixture.spawn(&["host", "list", "--refresh", "--json"]);
    // Each open returns once the fake holds the write end.
    let exited_reader = fs::File::open(&exited).expect("open the exit pipe");
    let alive_reader = fs::File::open(&alive).expect("open the liveness pipe");
    read_until_released(exited_reader);
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("pid")),
        nix::sys::signal::Signal::SIGINT,
    )
    .expect("interrupt the CLI");
    child.wait().expect("the CLI ends");
    read_until_released(alive_reader);
}
