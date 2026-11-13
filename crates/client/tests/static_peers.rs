//! Host-id pin checks of a static peer on the framed and attach dial paths.
//!
//! A stub overlay resolves a static peer with a pin; the daemon under test
//! reports the pinned id, a different id, or refuses; the framed connection and
//! the raw attach connection must fail with the same typed error before any
//! real request is served.
//!
//! The by-host raw proof needs Linux (`LD_PRELOAD` interposition), so it is
//! Linux-gated; the remaining checks run on every platform.

// Rust guideline compliant 2026-11-13

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use pohunek_test_support::process_env::{in_own_process, ProcessEnv};

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, LocalIdentity,
    OverlayError, OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, RegistryError,
    ResolvedPeer, ResolvedSelector,
};
use pohunek_client::protocol::{self, Request, Response};
use pohunek_client::{Client, ClientError, ClientOptions, OriginSource};
use serde_json::{json, Value};

/// Display name of the static peer; the port is the daemon's ephemeral port.
const PEER_NAME: &str = "vps2";
const PEER_FQDN: &str = "vps2.remote-box.netbird.test";
/// The owner-configured static address of the peer (an overlay-range IP).
/// Linux-only: the by-host raw proof redirects it to a loopback fixture.
#[cfg(target_os = "linux")]
const PEER_STATIC_IP: &str = "100.64.0.21";

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
            protocol::method::SESSION_INPUT => {
                json!({"accepted": true, "activity": "working", "activity_source": "process", "activity_epoch": "e1", "activity_revision": "1", "submitted": true, "runtime": {"worker_instance_id": "w1", "runtime_generation": "1"}})
            }
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
    OverlayRegistry::new(vec![
        ConfiguredTransport::new(Arc::new(transport), port).expect("configured transport")
    ])
    .expect("registry")
}

/// The restricted `0.83` status from slice A: no peer detail, daemon connected.
const RESTRICTED: &str = include_str!("../../netbird/tests/fixtures/status_restricted_v0_83.json");

#[tokio::test(flavor = "current_thread")]
async fn the_daemon_registry_without_static_peers_never_names_one() {
    // Hermetic: the transport resolves `netbird` through `PATH`, so the fake
    // script must shadow the real installation for this test process.
    if !in_own_process() {
        return;
    }
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
    let mut process_env = ProcessEnv::lock();
    process_env.set("PATH", bin.as_os_str());
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

/// Row 3 (dedicated shape, client crate): `session.input` with a wait goes
/// through `call_dedicated`, which dials a fresh connection and must re-prove
/// the pin before `session.input` on it.
#[tokio::test(flavor = "current_thread")]
async fn a_dedicated_static_connection_reproves_the_pin() {
    let pinned = host_id(b'A');
    let (port, served) = serve(pinned.clone());
    let mut client = Client::connect_with_registry_and_options(
        PEER_NAME,
        std::path::Path::new("/nonexistent/daemon.sock"),
        registry(port, &pinned),
        options(),
    )
    .await
    .expect("the matching connection");
    let params = protocol::SessionInputParams {
        session_id: protocol::SessionId("s-1".to_owned()),
        text: "hello".to_owned(),
        wait: Some(protocol::SessionInputWait {
            until: Some(vec![protocol::AgentActivity::Working]),
            timeout_ms: Some(100),
            allow_prompt_accepted: None,
        }),
    };
    let result = client.session_input(params).await;
    assert!(result.is_ok(), "the input call succeeded: {result:?}");
    let methods = served.lock().expect("served lock").clone();
    let inspect_count = methods
        .iter()
        .filter(|m| m.as_str() == protocol::method::HOST_GOVERNANCE_INSPECT)
        .count();
    assert!(
        inspect_count == 2,
        "the initial connection and the dedicated connection each probe: {inspect_count}: {methods:?}"
    );
}

/// Row 3 (fresh mismatch, client crate): a dedicated connection to a static
/// peer whose daemon now reports a different id must be refused with
/// `host_identity_mismatch` before `session.input` is served on the fresh
/// connection.
#[tokio::test(flavor = "current_thread")]
async fn a_dedicated_static_connection_refuses_a_fresh_mismatch() {
    let pinned = host_id(b'A');
    let (port, served, reported) = serve_switchable();
    let mut client = Client::connect_with_registry_and_options(
        PEER_NAME,
        std::path::Path::new("/nonexistent/daemon.sock"),
        registry(port, &pinned),
        options(),
    )
    .await
    .expect("the matching connection");
    // The daemon starts reporting another id after the initial connection;
    // the dedicated connection must re-prove the pin and refuse.
    *reported.lock().expect("reported id") = host_id(b'B');
    let params = protocol::SessionInputParams {
        session_id: protocol::SessionId("s-1".to_owned()),
        text: "hello".to_owned(),
        wait: Some(protocol::SessionInputWait {
            until: Some(vec![protocol::AgentActivity::Working]),
            timeout_ms: Some(100),
            allow_prompt_accepted: None,
        }),
    };
    let error = client
        .session_input(params)
        .await
        .expect_err("the fresh mismatch is refused");
    assert!(
        matches!(error, ClientError::HostIdentityMismatch { .. }),
        "the dedicated refusal: {error:?}"
    );
    let methods = served.lock().expect("served").clone();
    assert!(
        !methods
            .iter()
            .any(|m| m.as_str() == protocol::method::SESSION_INPUT),
        "`session.input` is never served after the fresh refusal: {methods:?}"
    );
}

/// Row 10 (raw by-host shape, at the client registry boundary; Linux only:
/// needs the `LD_PRELOAD` connect interposer below):
/// `connect_raw_with_options` resolves the static host through
/// `default_overlay_registry()` and, in the production code, proves the pin
/// on a framed connection before it opens the raw stream. The child half below
/// (`raw_by_host_child_probes_the_pin_first`) runs with the static table and a
/// `LD_PRELOAD` redirect, so it dials the real by-host raw path; the parent
/// half drives it as a subprocess and asserts the refusal. The interposer
/// maps the static address to a loopback fixture: the child sets
/// `POHUNEK_TEST_CONNECT_REDIRECT_PORT` to its loopback port, so the static
/// address dials the fixture.
///
/// The parent half keeps no hermetic setup of its own: the child builds it.
#[cfg(target_os = "linux")]
#[test]
fn a_raw_by_host_connection_proves_the_pin_first() {
    if !in_own_process() {
        return;
    }
    // The child half re-executes this test binary with
    // `POHUNEK_STATIC_PEERS_RAW_CHILD` set; it builds the hermetic setup
    // itself (config, fake netbird, LD_PRELOAD redirect, fixture daemon) and
    // dials the real by-host raw path. The parent only asserts the outcome.
    let exe = std::env::current_exe().expect("test executable");
    let output = std::process::Command::new(exe)
        .args(pohunek_test_support::exact_test_filter(
            module_path!(),
            "raw_by_host_child_probes_the_pin_first",
        ))
        .arg("--")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("POHUNEK_STATIC_PEERS_RAW_CHILD", "1")
        .output()
        .expect("run the raw by-host child");
    let text = format!(
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "the child by-host raw proof passed: {text}"
    );
}

/// Marker the parent half sets; the child setup half runs only with it.
/// Linux only: the by-host raw proof re-executes Linux-gated children.
#[cfg(target_os = "linux")]
const RAW_CHILD_MARKER: &str = "POHUNEK_STATIC_PEERS_RAW_CHILD";

/// Marker the setup child sets before it re-executes the dial grandchild.
/// Linux only: set together with the child marker on the grandchild.
#[cfg(target_os = "linux")]
const RAW_GRANDCHILD_MARKER: &str = "POHUNEK_STATIC_PEERS_RAW_GRANDCHILD";

/// C source of the `connect()` interposer the raw by-host child compiles: it
/// redirects the `100.64.0.0/10` overlay range to loopback on the port in
/// `POHUNEK_TEST_CONNECT_REDIRECT_PORT`. Linux only.
#[cfg(target_os = "linux")]
const CONNECT_REDIRECT_C: &str = "#define _GNU_SOURCE\n\
#include <arpa/inet.h>\n\
#include <dlfcn.h>\n\
#include <netinet/in.h>\n\
#include <stdlib.h>\n\
#include <sys/socket.h>\n\
#include <unistd.h>\n\
typedef int (*connect_fn)(int, const struct sockaddr *, socklen_t);\n\
int connect(int fd, const struct sockaddr *address, socklen_t length) {\n\
    static connect_fn real_connect = NULL;\n\
    if (real_connect == NULL) {\n\
        real_connect = (connect_fn)dlsym(RTLD_NEXT, \"connect\");\n\
    }\n\
    if (address != NULL && address->sa_family == AF_INET &&\n\
        length >= sizeof(struct sockaddr_in)) {\n\
        const struct sockaddr_in *original = (const struct sockaddr_in *)address;\n\
        unsigned int host = ntohl(original->sin_addr.s_addr);\n\
        if ((host & 0xffc00000U) == 0x64400000U) {\n\
            struct sockaddr_in redirected = *original;\n\
            redirected.sin_addr.s_addr = htonl(0x7f000001U);\n\
            const char *port = getenv(\"POHUNEK_TEST_CONNECT_REDIRECT_PORT\");\n\
            if (port != NULL && atoi(port) > 0 && atoi(port) < 65536) {\n\
                redirected.sin_port = htons((unsigned short)atoi(port));\n\
            }\n\
            return real_connect(fd, (const struct sockaddr *)&redirected, sizeof(redirected));\n\
        }\n\
    }\n\
    return real_connect(fd, address, length);\n\
}\n";

/// Hermetic static-peer setup the raw by-host child and grandchild share:
/// owner-private config with the pinned table plus a fake `netbird` on `PATH`
/// reporting the restricted overview.
#[cfg(target_os = "linux")]
struct RawChildSetup {
    env: pohunek_test_support::env::TestEnv,
    root: std::path::PathBuf,
    bin: std::path::PathBuf,
}

/// Write the static-peer config and the fake `netbird` into a fresh hermetic
/// home, returning the home and its `bin` directory.
#[cfg(target_os = "linux")]
fn raw_child_setup(pinned: &str) -> RawChildSetup {
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
             [netbird.static_peers.{PEER_NAME}]\naddress = \"{PEER_STATIC_IP}\"\n\
             fqdn = \"{PEER_FQDN}\"\nhost_id = \"{pinned}\"\n",
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
    RawChildSetup { env, root, bin }
}

/// Compile the `connect()` interposer in `root` and return its library path.
#[cfg(target_os = "linux")]
fn raw_child_redirect(root: &std::path::Path) -> std::path::PathBuf {
    let source = root.join("connect-redirect.c");
    let library = root.join("connect-redirect.so");
    std::fs::write(&source, CONNECT_REDIRECT_C).expect("write connect redirect source");
    let compiled = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-O2"])
        .arg(&source)
        .args(["-o"])
        .arg(&library)
        .arg("-ldl")
        .output()
        .expect("compile connect redirect");
    assert!(
        compiled.status.success(),
        "compile connect redirect: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    library
}

/// Child half of [`a_raw_by_host_connection_proves_the_pin_first`]: build the
/// hermetic static-peer setup, redirect the static address to a loopback
/// fixture, dial the real `connect_raw_with_options` by host name against a
/// daemon that reports another id, and expect `host_identity_mismatch` before
/// any raw byte is served.
///
/// Runs only in the re-executed child the parent half spawns; a plain test
/// run returns at once so the hermetic setup never disturbs other tests.
/// Linux only.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn raw_by_host_child_probes_the_pin_first() {
    if std::env::var_os(RAW_CHILD_MARKER).is_none() {
        return;
    }
    let pinned = host_id(b'A');
    let setup = raw_child_setup(&pinned);
    let library = raw_child_redirect(&setup.root);
    // The fixture daemon reports another id; the framed pre-verify must
    // refuse before `connect_raw_overlay_addr` opens the raw stream.
    // `LD_PRELOAD` takes effect at process start, so the grandchild
    // re-executes this test binary with the redirect in its exec environment.
    // The grandchild binds its own port and sets both redirect variables
    // in-process, so no other process can take the port in between. A marker
    // file carries the served-method log out of the grandchild: it alone
    // sees the dial.
    let served_log = setup.root.join("served-methods");
    let log_path = served_log.to_string_lossy().into_owned();
    let exe = std::env::current_exe().expect("test executable");
    let grandchild = std::process::Command::new(exe)
        .args(pohunek_test_support::exact_test_filter(
            module_path!(),
            "raw_by_host_grandchild_dials_the_pin_first",
        ))
        .arg("--")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env("XDG_CONFIG_HOME", setup.env.config_home())
        .env("PATH", &setup.bin)
        .env("LD_PRELOAD", &library)
        .env("POHUNEK_STATIC_PEERS_SERVED_LOG", log_path.as_str())
        .env(RAW_GRANDCHILD_MARKER, "1")
        .env(RAW_CHILD_MARKER, "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the raw by-host grandchild");
    let text = format!(
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&grandchild.stdout),
        String::from_utf8_lossy(&grandchild.stderr)
    );
    assert!(
        grandchild.status.success(),
        "the grandchild by-host raw proof passed: {text}"
    );
    let methods_raw = std::fs::read_to_string(&served_log).expect("served methods log");
    let methods: Vec<&str> = methods_raw.lines().collect();
    assert!(
        methods.contains(&protocol::method::HOST_GOVERNANCE_INSPECT),
        "the pin was probed on the framed connection: {methods:?}"
    );
    assert!(
        !methods.contains(&protocol::method::SESSION_LIST),
        "no other request reaches the daemon: {methods:?}"
    );
}

/// Grandchild of [`a_raw_by_host_connection_proves_the_pin_first`]: runs with
/// the hermetic environment the child half above installed (`LD_PRELOAD` in
/// the exec environment, so libc `connect` is interposed), serves the
/// mismatching fixture in-process, dials the real `connect_raw_with_options`
/// by host name, and expects `host_identity_mismatch`. It records the served
/// methods into `POHUNEK_STATIC_PEERS_SERVED_LOG` so the child can assert the
/// proof shape. Linux only.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn raw_by_host_grandchild_dials_the_pin_first() {
    if std::env::var_os(RAW_GRANDCHILD_MARKER).is_none() {
        return;
    }
    if std::env::var_os(RAW_CHILD_MARKER).is_none() {
        return;
    }
    let pinned = host_id(b'A');
    let reported = host_id(b'B');
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("bind the fixture on a free port");
    let redirect_port = listener.local_addr().expect("fixture address").port();
    // The interposer reads `POHUNEK_TEST_CONNECT_REDIRECT_PORT` on every
    // `connect()`, so the fixture port set here redirects this process's own
    // dials to the fixture. `POHUNEK_REMOTE_PORT` also points the framed
    // pre-verify at the same fixture address.
    let mut process_env = ProcessEnv::lock();
    process_env.set(
        "POHUNEK_TEST_CONNECT_REDIRECT_PORT",
        redirect_port.to_string(),
    );
    process_env.set("POHUNEK_REMOTE_PORT", redirect_port.to_string());
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
    let options = ClientOptions::default().with_origin_source(OriginSource::Omitted);
    let error = pohunek_client::connect_raw_with_options(
        PEER_NAME,
        std::path::Path::new("/nonexistent/daemon.sock"),
        options,
    )
    .await
    .expect_err("the by-host raw path proves the pin first");
    match error {
        ClientError::HostIdentityMismatch {
            expected, reported, ..
        } => {
            assert_eq!(expected, pinned, "the pinned id is expected");
            assert_eq!(reported, host_id(b'B'), "the daemon id is reported");
        }
        other => panic!("a typed identity error: {other:?}"),
    }
    let methods = served.lock().expect("served requests").clone();
    let log_path =
        std::env::var_os("POHUNEK_STATIC_PEERS_SERVED_LOG").expect("the served-methods log path");
    std::fs::write(log_path, methods.join("\n")).expect("write the served-methods log");
}

/// Hermetic static-peer table plus a fake `netbird` printing a full status
/// that contradicts the table (the peer `fqdn` at another address).
/// Returns the config directory with the file owner-private and the `bin`
/// directory with the fake, so the caller can route `PATH` and the config
/// home at it.
struct ContradictingStatusSetup {
    _env: pohunek_test_support::env::TestEnv,
    config_dir: std::path::PathBuf,
    bin: std::path::PathBuf,
    config_home: std::path::PathBuf,
}

fn contradicting_status_setup(pinned: &str) -> ContradictingStatusSetup {
    let env = pohunek_test_support::env::TestEnv::new().expect("create the test environment");
    let root = env.root().to_path_buf();
    let config_home = env.config_home().to_path_buf();
    let config_dir = config_home.join("pohunek");
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
    let carrier = serde_json::json!({
        "daemonStatus": "Connected",
        "management": {"url": "https://api.netbird.io:443", "connected": true},
        "netbirdIp": "100.64.0.1",
        "publicKey": "key-self",
        "fqdn": "self.netbird.test",
        "peers": [{"publicKey": "key-one", "fqdn": PEER_FQDN, "netbirdIp": "100.64.0.99", "status": "Connected"}],
        "details": null,
    });
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!("#!/bin/sh\nprintf '%s\\n' '{carrier}'\n"),
    )
    .expect("write fake netbird");
    ContradictingStatusSetup {
        _env: env,
        config_dir,
        bin,
        config_home,
    }
}

/// Row 8 (`resolve_peer` side): an unqualified selector in the qualified route
/// (`netbird:{PEER_NAME}`) drives `resolve_peer`, the sibling of
/// `resolve_peer_identity` in `resolve_qualified_host`. A contradicting full
/// status is refused the same way, before resolving. The fake `netbird`
/// prints the contradicting full status; resolution alone dials nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_contradicting_full_status_is_refused_on_peer_resolution() {
    if !in_own_process() {
        return;
    }
    let pinned = host_id(b'A');
    let setup = contradicting_status_setup(&pinned);
    let mut process_env = ProcessEnv::lock();
    process_env.set("PATH", setup.bin.as_os_str());
    if let Some(path) = setup.config_home.to_str() {
        process_env.set("XDG_CONFIG_HOME", path);
    }
    async {
        let config =
            pohunek_client::OverlayConfig::load_from(&setup.config_dir).expect("the config loads");
        let registry = config.registry().expect("registry with static peers");
        let host = format!("netbird:{PEER_NAME}");
        let error = registry
            .resolve_host(&host)
            .await
            .expect_err("a contradicting full status is refused");
        assert!(
            matches!(
                error,
                RegistryError::HostUnavailable { ref failures, .. }
                    if failures
                        .iter()
                        .any(|failure| matches!(failure.error, OverlayError::InvalidConfig { .. }))
            ),
            "the typed cross-check refusal: {error:?}"
        );
    }
    .await;
}

// `fqdn~` in the qualified selector drives `resolve_peer_identity`
// (registry routes it only through the identity argument). The
// selector value is the unpadded base64url encoding of the fqdn.
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut buf = [0u8; 4];
        buf[1..=chunk.len()].copy_from_slice(chunk);
        let n = u32::from_be_bytes(buf);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// Row 8 (identity side): `resolve_peer_identity` (`netbird:fqdn~` route) also
/// cross-checks the static table against a full status. A contradicting
/// full status is `overlay_configuration_invalid`; the session list runs over
/// `resolve_selector` (row 8 CLI side), so this proves the identity-flavoured
/// branch of the same cross-check. The fake `netbird` prints the
/// contradicting full status; resolution alone dials nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_contradicting_full_status_is_refused_on_identity_resolution() {
    if !in_own_process() {
        return;
    }
    let pinned = host_id(b'A');
    let setup = contradicting_status_setup(&pinned);
    let mut process_env = ProcessEnv::lock();
    process_env.set("PATH", setup.bin.as_os_str());
    if let Some(path) = setup.config_home.to_str() {
        process_env.set("XDG_CONFIG_HOME", path);
    }
    async {
        let config =
            pohunek_client::OverlayConfig::load_from(&setup.config_dir).expect("the config loads");
        let registry = config.registry().expect("registry with static peers");
        let selector = format!("netbird:fqdn~{}", b64url(PEER_FQDN.as_bytes()));
        let error = registry
            .resolve_host(&selector)
            .await
            .expect_err("a contradicting full status is refused");
        assert!(
            matches!(
                error,
                RegistryError::HostUnavailable { ref failures, .. }
                    if failures
                        .iter()
                        .any(|failure| matches!(failure.error, OverlayError::InvalidConfig { .. }))
            ),
            "the typed cross-check refusal: {error:?}"
        );
    }
    .await;
}
