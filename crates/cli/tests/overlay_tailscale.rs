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

/// A missing, failing or garbled CLI each has a distinct, non-guessing result.
#[test]
fn a_missing_or_failing_cli_is_reported_by_cause() {
    let fixture = Fixture::new(&tailnet());
    fs::remove_file(fixture.fake.bin_dir().join("tailscale")).expect("remove the fake");
    let output = fixture.run(&["host", "list", "--refresh", "--json"]);
    assert_eq!(error_code(&output), "tailscale_cli_missing");
    let (output, targets) = fixture.dial("alpha");
    assert_eq!(error_code(&output), "tailscale_cli_missing");
    assert!(targets.is_empty(), "{targets:?}");

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
