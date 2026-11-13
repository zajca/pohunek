//! Host-id pin checks of a static peer on the framed and attach dial paths.
//!
//! A stub overlay resolves a static peer with a pin; the daemon under test
//! reports the pinned id, a different id, or refuses; the framed connection and
//! the raw attach connection must fail with the same typed error before any
//! real request is served.

// Rust guideline compliant 2026-11-13

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, LocalIdentity,
    OverlayError, OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
    ResolvedSelector,
};
use pohunek_client::protocol::{self, Request, Response};
use pohunek_client::{Client, ClientError, ClientOptions, OriginSource};
use serde_json::{json, Value};

/// Display name of the static peer; the port is the daemon's ephemeral port.
const PEER_NAME: &str = "vps2";
const PEER_FQDN: &str = "vps2.remote-box.netbird.test";

fn host_id(byte: u8) -> String {
    format!(
        "host_{}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        byte as char
    )
}

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

/// Overlay that resolves only the static peer, with its pin.
#[derive(Debug)]
struct StaticOverlay {
    id: OverlayId,
    pinned: String,
}

impl StaticOverlay {
    fn peer(&self) -> ResolvedPeer {
        ResolvedPeer {
            peer_id: None,
            display_name: Some(PEER_NAME.to_owned()),
            fqdn: Some(PEER_FQDN.to_owned()),
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            expected_host_id: Some(self.pinned.clone()),
        }
    }
}

impl OverlayTransport for StaticOverlay {
    fn id(&self) -> &OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, addr: IpAddr) -> Result<(), BindAddrError> {
        Err(BindAddrError::NotMember(addr))
    }

    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
        Box::pin(async { Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)) })
    }

    fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
        let result = if host == PEER_NAME {
            Ok(self.peer())
        } else {
            Err(OverlayError::HostUnknown {
                host: host.to_owned(),
                overlay: self.id.clone(),
            })
        };
        Box::pin(async move { result })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        _identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        Box::pin(async move {
            Err(OverlayError::HostUnknown {
                host: "peer".to_owned(),
                overlay: self.id.clone(),
            })
        })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn resolve_selector<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedSelector> {
        let result = if host == PEER_NAME {
            Ok(ResolvedSelector::Peer(self.peer()))
        } else {
            Err(OverlayError::HostUnknown {
                host: host.to_owned(),
                overlay: self.id.clone(),
            })
        };
        Box::pin(async move { result })
    }

    fn local_identity(&self) -> OverlayFuture<'_, Option<LocalIdentity>> {
        Box::pin(async { Ok(None) })
    }
}

fn options() -> ClientOptions {
    ClientOptions::default().with_origin_source(OriginSource::Omitted)
}

/// A daemon that answers the health probe, the governance inspect with
/// `reported`, and records every method served.
fn serve(reported: String) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let served = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&served);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let seen = Arc::clone(&seen);
            let reported = reported.clone();
            std::thread::spawn(move || serve_connection(stream, &seen, &reported));
        }
    });
    (port, served)
}

/// A daemon whose reported host id can change between connections: `(port,
/// served methods, current reported host id)`.
#[allow(
    clippy::type_complexity,
    reason = "(port, served methods, reported id) triple used by the fixture daemon"
)]
fn serve_switchable() -> (u16, Arc<Mutex<Vec<String>>>, Arc<Mutex<String>>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the fixture");
    let port = listener.local_addr().expect("fixture address").port();
    let served = Arc::new(Mutex::new(Vec::new()));
    let reported = Arc::new(Mutex::new(host_id(b'A')));
    let seen = Arc::clone(&served);
    let current = Arc::clone(&reported);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a fixture connection");
            let seen = Arc::clone(&seen);
            let current = Arc::clone(&current);
            std::thread::spawn(move || {
                let reported = current.lock().expect("reported id").clone();
                serve_connection(stream, &seen, &reported);
            });
        }
    });
    (port, served, reported)
}

fn serve_connection(stream: TcpStream, served: &Arc<Mutex<Vec<String>>>, reported: &str) {
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
        let result = match request.method() {
            protocol::method::HOST_GOVERNANCE_INSPECT => governance_status(reported),
            protocol::method::SESSION_LIST => json!([]),
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
}

fn registry(port: u16, pinned: &str) -> OverlayRegistry {
    let transport = StaticOverlay {
        id: OverlayId::new("stub").expect("overlay id"),
        pinned: pinned.to_owned(),
    };
    let _ = port;
    OverlayRegistry::new(vec![
        ConfiguredTransport::new(Arc::new(transport), port).expect("configured transport")
    ])
    .expect("registry")
}

/// The restricted `0.83` status from slice A: no peer detail, daemon connected.
const RESTRICTED: &str = include_str!("../../netbird/tests/fixtures/status_restricted_v0_83.json");

#[tokio::test]
async fn the_daemon_registry_without_static_peers_never_names_one() {
    // Hermetic: the transport resolves `netbird` through `PATH`, so the fake
    // script must shadow the real installation for this test process. Tests in
    // this binary never run concurrently with other PATH readers (the pin
    // scenarios above use a stub overlay), and the value is restored after.
    let pinned = host_id(b'A');
    let env = pohunek_test_support::env::TestEnv::new().expect("create the test environment");
    let root = env.root().to_path_buf();
    let config_dir = env.config_home().join("pohunek");
    std::fs::create_dir_all(&config_dir).expect("create the config directory");
    std::fs::set_permissions(
        &config_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("make the config directory private");
    let file = config_dir.join("overlays.toml");
    std::fs::write(
        &file,
        format!(
            "providers = [\"netbird\"]\n\n[netbird]\nsubnet = \"100.64.0.0/10\"\n\n\
             [netbird.static_peers.{PEER_NAME}]\naddress = \"100.64.0.21\"\n\
             fqdn = \"{PEER_FQDN}\"\nhost_id = \"{pinned}\"\n"
        ),
    )
    .expect("write overlays.toml");
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("make overlays.toml private");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create the bin directory");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{RESTRICTED}'\n"),
    )
    .expect("write fake netbird");
    let original_path = std::env::var_os("PATH");
    let path = match &original_path {
        Some(path) => {
            std::ffi::OsString::from(format!("{}:{}", bin.display(), path.to_string_lossy()))
        }
        None => bin.as_os_str().to_owned(),
    };
    std::env::set_var("PATH", &path);
    async {
        let config =
            pohunek_client::OverlayConfig::load_from(&config_dir).expect("the config loads");
        let with_static = config.registry().expect("registry with static peers");
        let route = with_static
            .resolve_host(PEER_NAME)
            .await
            .expect("the static peer resolves");
        assert_eq!(
            route.expected_host_id.as_deref(),
            Some(pinned.as_str()),
            "the route carries the pin"
        );
        let without_static = config
            .registry_without_static_peers()
            .expect("registry without static peers");
        assert!(
            without_static.resolve_host(PEER_NAME).await.is_err(),
            "the daemon registry never names a static peer"
        );
    }
    .await;
    match original_path {
        Some(value) => std::env::set_var("PATH", value),
        None => std::env::remove_var("PATH"),
    }
}

#[tokio::test]
async fn a_matching_pin_connects_and_the_attach_succeeds() {
    let pinned = host_id(b'A');
    let (port, served) = serve(pinned.clone());
    let mut client = Client::connect_with_registry_and_options(
        PEER_NAME,
        std::path::Path::new("/nonexistent/daemon.sock"),
        registry(port, &pinned),
        options(),
    )
    .await
    .expect("a matching pin connects");
    let request = Request::new(
        pohunek_client::next_request_id(protocol::method::SESSION_LIST),
        protocol::method::SESSION_LIST,
        json!({}),
    )
    .expect("session list request");
    client.request(&request).await.expect("the list is served");
    let methods = served.lock().expect("served requests").clone();
    assert!(
        methods
            .iter()
            .any(|method| method == protocol::method::HOST_GOVERNANCE_INSPECT),
        "the pin was checked: {methods:?}"
    );
}

#[tokio::test]
async fn a_mismatching_pin_refuses_the_framed_connection() {
    let pinned = host_id(b'A');
    let reported = host_id(b'B');
    let (port, served) = serve(reported);
    let error = Client::connect_with_registry_and_options(
        PEER_NAME,
        std::path::Path::new("/nonexistent/daemon.sock"),
        registry(port, &pinned),
        options(),
    )
    .await
    .expect_err("a mismatching pin is refused");
    match error {
        ClientError::HostIdentityMismatch {
            expected, reported, ..
        } => {
            assert_eq!(expected, pinned);
            assert_eq!(reported, host_id(b'B'));
        }
        other => panic!("a typed identity error: {other:?}"),
    }
    let methods = served.lock().expect("served requests").clone();
    assert!(
        !methods
            .iter()
            .any(|method| method == protocol::method::SESSION_LIST),
        "no other request reaches the daemon: {methods:?}"
    );
}

#[tokio::test]
async fn a_mismatching_pin_refuses_the_attach_connection() {
    let pinned = host_id(b'A');
    let (port, served, current) = serve_switchable();
    let client = Client::connect_with_registry_and_options(
        PEER_NAME,
        std::path::Path::new("/nonexistent/daemon.sock"),
        registry(port, &pinned),
        options(),
    )
    .await
    .expect("a matching pin connects");
    *current.lock().expect("reported id") = host_id(b'B');
    let error = client
        .attach_raw("stream-1")
        .await
        .expect_err("a mismatching pin refuses the attach");
    match error {
        ClientError::HostIdentityMismatch {
            expected, reported, ..
        } => {
            assert_eq!(expected, pinned);
            assert_eq!(reported, host_id(b'B'));
        }
        other => panic!("a typed identity error: {other:?}"),
    }
    let methods = served.lock().expect("served requests").clone();
    assert!(
        !methods
            .iter()
            .any(|method| method == protocol::method::SESSION_LIST),
        "no other request reaches the daemon: {methods:?}"
    );
}
