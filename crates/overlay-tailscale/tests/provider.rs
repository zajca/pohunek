//! The Tailscale provider through the `OverlayTransport` contract.
//!
//! A fake `tailscale` on `PATH` serves documents derived from `ipnstate.Status`
//! of `tailscale/tailscale@v1.104.2` (the shared fixture module). The
//! scenarios check the decisions the registry and the daemon rely on: which
//! address the node may listen on, which peers are selectable, and which
//! addresses may be bound.

use std::net::IpAddr;

use overlay::{BindAddrError, ExternalIdentity, OverlayError, OverlayTransport, ResolvedSelector};
use overlay_tailscale::TailscaleTransport;
use pohunek_test_support::process_env::{in_own_process, ProcessEnv};

#[path = "../../cli/tests/support/tailscale_status.rs"]
mod tailscale_status;

use tailscale_status::{FakeTailscale, Node, Status};

fn address(text: &str) -> IpAddr {
    text.parse().expect("IP literal")
}

/// Puts a fake `tailscale` serving `status` first on `PATH`.
fn install(env: &mut ProcessEnv, status: &Status) -> (FakeTailscale, tempfile::TempDir) {
    let root = pohunek_test_support::tempdir().expect("create the fixture root");
    let fake = FakeTailscale::new(root.path());
    fake.install(status);
    env.set("PATH", fake.bin_dir());
    (fake, root)
}

/// The node listens only on its own IPv4 address, and only while Tailscale
/// runs on a kernel interface.
#[tokio::test]
async fn the_listener_address_is_the_nodes_ipv4_address_or_nothing() {
    if !in_own_process() {
        return;
    }
    let mut env = ProcessEnv::lock();
    let (fake, _root) = install(&mut env, &Status::running("100.64.0.7", vec![]));
    let transport = TailscaleTransport::new();

    assert_eq!(
        transport.listener_addr().await,
        Ok(address("100.64.0.7")),
        "a running node on a kernel interface listens on its IPv4 address"
    );

    let ipv6_only = Node::peer("self-box", "nSELF00001CNTRL", &["fd7a:115c:a1e0::7"]);
    let unusable: Vec<(&str, Status)> = vec![
        (
            "NoState",
            Status::running("100.64.0.7", vec![]).with_state("NoState"),
        ),
        (
            "InUseOtherUser",
            Status::running("100.64.0.7", vec![]).with_state("InUseOtherUser"),
        ),
        (
            "NeedsLogin",
            Status::running("100.64.0.7", vec![]).with_state("NeedsLogin"),
        ),
        (
            "NeedsMachineAuth",
            Status::running("100.64.0.7", vec![]).with_state("NeedsMachineAuth"),
        ),
        (
            "Stopped",
            Status::running("100.64.0.7", vec![]).with_state("Stopped"),
        ),
        (
            "Starting",
            Status::running("100.64.0.7", vec![]).with_state("Starting"),
        ),
        (
            "an unknown state",
            Status::running("100.64.0.7", vec![]).with_state("Hibernating"),
        ),
        (
            "userspace networking",
            Status::running("100.64.0.7", vec![]).userspace(),
        ),
        (
            "no IPv4 address",
            Status::running("100.64.0.7", vec![]).with_this(ipv6_only),
        ),
        (
            "the ChromeOS range",
            Status::running("100.115.92.5", vec![]),
        ),
        (
            "the ChromeOS range end",
            Status::running("100.115.93.255", vec![]),
        ),
        (
            "the service address",
            Status::running("100.100.100.100", vec![]),
        ),
        ("below the range", Status::running("100.63.255.255", vec![])),
        ("above the range", Status::running("100.128.0.0", vec![])),
        ("a LAN address", Status::running("192.168.1.2", vec![])),
        ("loopback", Status::running("127.0.0.1", vec![])),
    ];
    for (label, status) in unusable {
        fake.set_status(&status);
        assert!(
            matches!(
                transport.listener_addr().await,
                Err(OverlayError::ListenerAddressMissing { .. })
            ),
            "{label}: no listener address"
        );
    }

    // The edges of the range and of the excluded ChromeOS block are inside.
    for edge in [
        "100.64.0.0",
        "100.115.91.255",
        "100.115.94.0",
        "100.127.255.255",
    ] {
        fake.set_status(&Status::running(edge, vec![]));
        assert_eq!(transport.listener_addr().await, Ok(address(edge)), "{edge}");
    }
}

/// A peer is selectable under every selector form exactly when it is in the
/// netmap, not expired and not shared in, and the status lists an address in
/// range for it.
#[tokio::test]
async fn only_selectable_peers_resolve() {
    if !in_own_process() {
        return;
    }
    let mut env = ProcessEnv::lock();
    let status = Status::running(
        "100.64.0.7",
        vec![
            Node::peer(
                "alpha",
                "nALPHA00001CNTRL",
                &["100.64.0.10", "fd7a:115c:a1e0::10"],
            ),
            Node::peer("bravo", "nBRAVO00001CNTRL", &["fd7a:115c:a1e0::20"]),
            Node::peer("shared", "nSHARE00001CNTRL", &["100.64.0.11"]).sharee(),
            Node::peer("expired", "nEXPIR00001CNTRL", &["100.64.0.12"]).expired(),
            Node::peer("gone", "nGONE000001CNTRL", &["100.64.0.13"]).out_of_netmap(),
            Node::peer("lan", "nLAN0000001CNTRL", &["10.0.0.9"]),
        ],
    );
    let (_fake, _root) = install(&mut env, &status);
    let transport = TailscaleTransport::new();

    let alpha = transport
        .resolve_peer("alpha")
        .await
        .expect("alpha resolves");
    assert_eq!(alpha.address, address("100.64.0.10"));
    assert_eq!(alpha.peer_id.as_deref(), Some("nALPHA00001CNTRL"));
    assert_eq!(alpha.fqdn.as_deref(), Some("alpha.tail-test.ts.net"));
    let bravo = transport
        .resolve_peer("bravo")
        .await
        .expect("bravo resolves");
    assert_eq!(bravo.address, address("fd7a:115c:a1e0::20"));

    let by_id = ExternalIdentity::peer_id("nALPHA00001CNTRL").expect("identity");
    let by_name = ExternalIdentity::fqdn("alpha.tail-test.ts.net").expect("identity");
    for identity in [&by_id, &by_name] {
        let peer = transport
            .resolve_peer_identity(identity)
            .await
            .expect("typed identity resolves");
        assert_eq!(peer.address, address("100.64.0.10"));
    }

    for name in [
        "shared",
        "expired",
        "gone",
        "lan",
        "100.64.0.11",
        "100.64.0.99",
        "missing",
    ] {
        assert!(
            matches!(
                transport.resolve_peer(name).await,
                Err(OverlayError::HostUnknown { .. })
            ),
            "{name} is not selectable"
        );
    }
    for identity in [
        ExternalIdentity::peer_id("nSHARE00001CNTRL").expect("identity"),
        ExternalIdentity::peer_id("nEXPIR00001CNTRL").expect("identity"),
        ExternalIdentity::fqdn("gone.tail-test.ts.net").expect("identity"),
        // A typed peer identity is not matched as a name.
        ExternalIdentity::peer_id("alpha").expect("identity"),
        ExternalIdentity::fqdn("nALPHA00001CNTRL").expect("identity"),
    ] {
        assert!(
            matches!(
                transport.resolve_peer_identity(&identity).await,
                Err(OverlayError::HostUnknown { .. })
            ),
            "{identity:?} is not selectable"
        );
    }

    let snapshot = transport.discover_snapshot().await.expect("snapshot");
    let mut listed = snapshot
        .peers
        .iter()
        .filter_map(|peer| peer.display_name.clone())
        .collect::<Vec<_>>();
    listed.sort();
    assert_eq!(listed, ["alpha", "bravo"]);
    let local = snapshot.local.expect("this host is reported");
    assert_eq!(local.fqdn.as_deref(), Some("self-box.tail-test.ts.net"));
    assert_eq!(local.address, Some(address("100.64.0.7")));
    assert!(matches!(
        transport.resolve_selector("self-box").await,
        Ok(ResolvedSelector::LocalHost)
    ));
}

/// Only an IPv4 node address of the range is a valid bind target.
#[test]
fn bind_validation_accepts_only_ipv4_node_addresses() {
    let transport = TailscaleTransport::new();
    for accepted in [
        "100.64.0.0",
        "100.64.0.7",
        "100.115.91.255",
        "100.115.94.0",
        "100.127.255.255",
    ] {
        assert_eq!(
            transport.validate_bind_addr(address(accepted)),
            Ok(()),
            "{accepted}"
        );
    }
    for forbidden in [
        "0.0.0.0",
        "::",
        "127.0.0.1",
        "::1",
        "169.254.1.1",
        "fe80::1",
        "224.0.0.1",
        "ff02::1",
        "255.255.255.255",
    ] {
        assert_eq!(
            transport.validate_bind_addr(address(forbidden)),
            Err(BindAddrError::Forbidden(address(forbidden))),
            "{forbidden}"
        );
    }
    for foreign in [
        "100.63.255.255",
        "100.128.0.0",
        "100.115.92.0",
        "100.115.93.255",
        "100.100.100.100",
        "10.0.0.1",
        "192.168.1.1",
        "8.8.8.8",
        "fd7a:115c:a1e0::7",
        "fd7a:115c:a1e1::7",
        "2001:db8::1",
        "::ffff:100.64.0.1",
    ] {
        assert_eq!(
            transport.validate_bind_addr(address(foreign)),
            Err(BindAddrError::NotMember(address(foreign))),
            "{foreign}"
        );
    }
}
