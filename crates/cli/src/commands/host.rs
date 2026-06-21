//! `pohunek host` — discover, list, and inspect remote hosts over NetBird.
//!
//! `discover` and `list` enumerate NetBird peers and classify each by probing its
//! daemon control port (so the operator sees which peers run a compatible
//! daemon). `inspect <host>` is a *live* query: it connects to the host's daemon
//! and returns its [`HostCapabilities`] snapshot.
//!
//! Without a persistence store (out of scope for this milestone), the set of
//! "known hosts" is exactly the set of live NetBird peers, so `list` and
//! `discover` share one discovery core; they differ only in presentation.

use std::net::SocketAddr;
use std::time::Duration;

use futures::future::join_all;
use protocol::{method, HostCapabilities, Request, Response, PROTOCOL_VERSION};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::client::Client;
use crate::commands::request_id;
use crate::error::CliError;
use crate::paths::Paths;

/// How long to wait for a single peer probe (TCP connect + one health exchange)
/// before classifying it as unreachable. Kept short so `discover` over many
/// peers stays responsive; probes run concurrently regardless.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Mirrors the control-line cap used elsewhere; bounds a misbehaving peer.
const MAX_PROBE_LINE_BYTES: usize = 1024 * 1024;

/// How a NetBird peer is classified for `host discover`/`list`.
///
/// Serializes with an internal `classification` tag so a `--json` consumer can
/// branch on it (e.g. `{"classification":"reachable_daemon","daemon_version":..}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "classification")]
pub(crate) enum HostClass {
    /// A compatible daemon answered with our protocol version.
    ReachableDaemon {
        /// The daemon version the peer reported.
        daemon_version: String,
    },
    /// A daemon answered but speaks a different protocol version.
    VersionMismatch {
        /// The protocol version the peer's daemon reported.
        daemon_protocol_version: u32,
    },
    /// The peer advertises a NetBird-range IP but its daemon port could not be
    /// reached, or it returned no usable health response.
    Unreachable,
    /// The peer had no NetBird-range IP to dial, so it was not probed.
    Candidate,
}

/// One enumerated host with its classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HostRecord {
    /// Short host name (first DNS label of the fqdn), when derivable.
    name: Option<String>,
    /// The peer's fully qualified NetBird name.
    fqdn: Option<String>,
    /// The peer's NetBird IP as a string.
    netbird_ip: Option<String>,
    /// Classification (flattened so its fields sit alongside the record).
    #[serde(flatten)]
    class: HostClass,
}

/// Run `host discover`: enumerate NetBird peers and probe connected ones.
///
/// # Errors
///
/// Returns [`CliError`] when NetBird state cannot be read (CLI missing, daemon
/// down, not logged in) or the port cannot be resolved.
pub(crate) async fn run_discover(json: bool) -> Result<(), CliError> {
    let records = discover_records().await?;
    if json {
        print!("{}", crate::commands::render_json(&records)?);
    } else {
        print!("{}", render_records_human(&records));
    }
    Ok(())
}

/// Run `host list`: the same discovery core as `discover`, emphasizing the
/// name / IP / classification / version columns.
///
/// Without a persistence store, "known hosts" are the live NetBird peers, so this
/// shares [`discover_records`]; the human rendering is identical for now.
///
/// # Errors
///
/// Same as [`run_discover`].
pub(crate) async fn run_list(json: bool) -> Result<(), CliError> {
    run_discover(json).await
}

/// Run `host inspect <host>`: a live capability query against the host's daemon.
///
/// # Errors
///
/// Returns [`CliError`] when the host cannot be resolved or reached, or the
/// daemon returns an error or an unexpected payload.
pub(crate) async fn run_inspect(host: &str, paths: &Paths, json: bool) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let request = Request::new(
        request_id(method::HOST_INSPECT),
        method::HOST_INSPECT,
        Value::Null,
    );
    let result = client.request(&request).await?;
    let caps: HostCapabilities = serde_json::from_value(result)?;

    if json {
        print!("{}", crate::commands::render_json(&caps)?);
    } else {
        print!("{}", render_capabilities_human(host, &caps));
    }
    Ok(())
}

/// Enumerate NetBird peers and build a classified record for each.
///
/// Every peer that advertises a parseable NetBird IP is probed concurrently —
/// regardless of its NetBird connection state. An `Idle` peer (lazy connection
/// not yet established) still has a routable NetBird address whose daemon may be
/// reachable; dialing it establishes the tunnel on demand. Only a peer with no
/// usable IP — nothing to dial — is left a [`HostClass::Candidate`].
async fn discover_records() -> Result<Vec<HostRecord>, CliError> {
    let status = netbird::run_status().map_err(map_netbird_err)?;
    let port = netbird::remote_port().map_err(map_netbird_err)?;

    // Build the probe futures up front so peers are probed concurrently rather
    // than one at a time.
    let mut probes = Vec::new();
    for peer in status.peers() {
        let name = peer.fqdn.as_deref().map(short_hostname).map(str::to_owned);
        let fqdn = peer.fqdn.clone();
        let netbird_ip = peer.netbird_ip.clone();
        let target = probe_target(peer, port);
        probes.push(async move {
            let class = match target {
                Some(addr) => classify(addr, PROBE_TIMEOUT).await,
                None => HostClass::Candidate,
            };
            HostRecord {
                name,
                fqdn,
                netbird_ip,
                class,
            }
        });
    }

    Ok(join_all(probes).await)
}

/// The control address to probe for a peer, or `None` when there is nothing
/// safe to dial.
///
/// A peer is probed whenever it advertises a parseable IP **inside the NetBird
/// CGNAT range** — its NetBird connection state is deliberately *not* a gate. An
/// `Idle` peer has a routable address whose daemon may answer (the dial
/// establishes the tunnel on demand), so excluding it would silently hide a
/// reachable host. The [`netbird::is_netbird_ip`] gate is the same fail-closed
/// boundary the resolver and bind validator apply: discovery auto-probes every
/// peer, so it must never be coerced into dialing a loopback, link-local
/// (e.g. cloud-metadata `169.254.169.254`), LAN, or public address a peer's
/// reported `netbirdIp` might (through drift or a compromised coordinator) carry.
/// A peer with no NetBird-range IP has no address worth dialing and stays a
/// [`HostClass::Candidate`].
fn probe_target(peer: &netbird::Peer, port: u16) -> Option<SocketAddr> {
    peer.ip()
        .filter(|ip| netbird::is_netbird_ip(*ip))
        .map(|ip| SocketAddr::new(ip, port))
}

/// Probe a daemon control address and classify it.
///
/// Opens a TCP connection, sends a single `daemon.health` request, and reads the
/// response. Classification keys off the **response envelope's protocol version**
/// (`v`), which a daemon stamps on *every* reply — including the
/// `version_mismatch` error it returns when negotiation rejects an incompatible
/// client (see `handler.rs`: `negotiate` runs before dispatch, so a skewed
/// daemon never emits an `ok` health body). Reading `v` rather than the `ok`
/// body is exactly what lets discovery tell a version-skewed daemon apart from
/// an absent one.
///
/// Transport-level failures (connect refused, timeout, a closed or garbled
/// connection) collapse to [`HostClass::Unreachable`] — reachable on the mesh,
/// but no usable daemon. The whole exchange is bounded by `timeout`.
///
/// Exposed at `pub(crate)` so tests can point it at a live loopback daemon, a
/// closed port, and a version-mismatch stub.
pub(crate) async fn classify(addr: SocketAddr, timeout: Duration) -> HostClass {
    match tokio::time::timeout(timeout, probe_health(addr)).await {
        Ok(Ok(response)) => classify_response(&response),
        // Connect/IO/framing error or a timeout: no usable daemon here.
        Ok(Err(_)) | Err(_) => HostClass::Unreachable,
    }
}

/// Open `addr`, send `daemon.health`, and return the parsed control response.
///
/// Returns the whole [`Response`] (ok *or* err) so the caller can classify on
/// the envelope version; only a transport/parse failure is an `Err` here.
async fn probe_health(addr: SocketAddr) -> Result<Response, CliError> {
    let mut stream = TcpStream::connect(addr).await?;

    // The control protocol is newline-delimited JSON. Send one request line.
    let request = Request::new(
        request_id(method::DAEMON_HEALTH),
        method::DAEMON_HEALTH,
        Value::Null,
    );
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;

    // Read a single response line, capped so a misbehaving peer cannot exhaust
    // memory. We stop at the first newline.
    let reply = read_line(&mut stream, MAX_PROBE_LINE_BYTES).await?;
    Ok(serde_json::from_slice(&reply)?)
}

/// Read bytes up to and excluding the first `\n`, bounded by `max`.
///
/// Returns a framing error if the cap is hit before a newline or the peer closes
/// the connection without sending one.
async fn read_line(stream: &mut TcpStream, max: usize) -> Result<Vec<u8>, CliError> {
    let mut buf = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err(CliError::Framing(
                "peer closed the connection without a response".to_owned(),
            ));
        }
        if byte[0] == b'\n' {
            return Ok(buf);
        }
        if buf.len() >= max {
            return Err(CliError::Framing(
                "probe response exceeded maximum length".to_owned(),
            ));
        }
        buf.push(byte[0]);
    }
}

/// Classify a parsed control response against our protocol version.
///
/// The daemon's protocol version is read from the response envelope (`v`), so a
/// `version_mismatch` error — the reply a real daemon sends to an incompatible
/// client — classifies as [`HostClass::VersionMismatch`] rather than being
/// mistaken for an unreachable peer. A matching-version daemon that answered
/// `ok` is [`HostClass::ReachableDaemon`]; a matching-version daemon that
/// answered with an error has no usable health, so it is
/// [`HostClass::Unreachable`].
fn classify_response(response: &Response) -> HostClass {
    let daemon_protocol_version = response.version().get();
    if daemon_protocol_version != PROTOCOL_VERSION.get() {
        return HostClass::VersionMismatch {
            daemon_protocol_version,
        };
    }
    match response {
        Response::Ok { ok, .. } => HostClass::ReachableDaemon {
            daemon_version: ok
                .get("daemon_version")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| "<unknown>".to_owned()),
        },
        // A matching-version daemon that errored on health is not usable.
        Response::Err { .. } => HostClass::Unreachable,
    }
}

/// Map a [`netbird::NetbirdError`] to the matching CLI error.
fn map_netbird_err(err: netbird::NetbirdError) -> CliError {
    match err {
        netbird::NetbirdError::CliMissing => CliError::NetbirdCliMissing,
        netbird::NetbirdError::StateUnavailable(detail) => {
            CliError::NetbirdStateUnavailable { detail }
        }
        netbird::NetbirdError::Parse(detail) => CliError::NetbirdStateUnavailable { detail },
        netbird::NetbirdError::HostUnknown(host) => CliError::HostUnknown { host },
    }
}

/// The first DNS label of a fqdn (e.g. `host-b` from `host-b.netbird.cloud`).
fn short_hostname(fqdn: &str) -> &str {
    fqdn.split('.').next().unwrap_or(fqdn)
}

/// Render the discovered hosts as an aligned table.
fn render_records_human(records: &[HostRecord]) -> String {
    let name_of = |r: &HostRecord| r.name.clone().unwrap_or_else(|| "-".to_owned());
    let ip_of = |r: &HostRecord| r.netbird_ip.clone().unwrap_or_else(|| "-".to_owned());

    let name_width = records
        .iter()
        .map(|r| name_of(r).len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let ip_width = records
        .iter()
        .map(|r| ip_of(r).len())
        .max()
        .unwrap_or(0)
        .max("NETBIRD_IP".len());

    let mut output = String::new();
    output.push_str(&format!(
        "{:<name_width$}  {:<ip_width$}  STATUS         VERSION\n",
        "NAME",
        "NETBIRD_IP",
        name_width = name_width,
        ip_width = ip_width,
    ));
    for r in records {
        let (status, version) = class_columns(&r.class);
        output.push_str(&format!(
            "{:<name_width$}  {:<ip_width$}  {:<13}  {}\n",
            name_of(r),
            ip_of(r),
            status,
            version,
            name_width = name_width,
            ip_width = ip_width,
        ));
    }
    output
}

/// The status label + version cell for a classification.
fn class_columns(class: &HostClass) -> (&'static str, String) {
    match class {
        HostClass::ReachableDaemon { daemon_version } => ("reachable", daemon_version.clone()),
        HostClass::VersionMismatch {
            daemon_protocol_version,
        } => ("version_skew", format!("proto {daemon_protocol_version}")),
        HostClass::Unreachable => ("unreachable", "-".to_owned()),
        HostClass::Candidate => ("candidate", "-".to_owned()),
    }
}

/// Render a host's capability snapshot as a human table.
fn render_capabilities_human(host: &str, caps: &HostCapabilities) -> String {
    let mut output = format!("host {host} capabilities\n");
    output.push_str(&format!("  daemon_version:     {}\n", caps.daemon_version));
    output.push_str(&format!(
        "  protocol_version:   {}\n",
        caps.protocol_version
    ));
    output.push_str(&format!("  git_available:      {}\n", caps.git_available));
    output.push_str(&format!(
        "  worktree_supported: {}\n",
        caps.worktree_supported
    ));
    output.push_str("  supported_agents:   ");
    let agents: Vec<String> = caps
        .supported_agents
        .iter()
        .map(agent_label)
        .map(str::to_owned)
        .collect();
    output.push_str(&agents.join(", "));
    output.push('\n');
    output.push_str("  runtimes:\n");
    for rt in &caps.runtimes {
        let path = rt.path.as_deref().unwrap_or("-");
        output.push_str(&format!(
            "    {:<8} available={:<5} path={}\n",
            agent_label(&rt.agent),
            rt.available,
            path,
        ));
    }
    output
}

fn agent_label(agent: &protocol::AgentKind) -> &'static str {
    match agent {
        protocol::AgentKind::Shell => "shell",
        protocol::AgentKind::Codex => "codex",
        protocol::AgentKind::Claude => "claude",
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use protocol::{AgentKind, AgentRuntime, ProtocolVersion};
    use tokio::net::TcpListener;

    use super::*;

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    #[test]
    fn short_hostname_takes_first_label() {
        assert_eq!(short_hostname("host-b.netbird.cloud"), "host-b");
        assert_eq!(short_hostname("plain"), "plain");
        assert_eq!(short_hostname(""), "");
    }

    #[test]
    fn host_class_json_shapes_are_stable() {
        let reachable = serde_json::to_value(HostRecord {
            name: Some("host-b".to_owned()),
            fqdn: Some("host-b.netbird.cloud".to_owned()),
            netbird_ip: Some("100.92.30.40".to_owned()),
            class: HostClass::ReachableDaemon {
                daemon_version: "0.1.0".to_owned(),
            },
        })
        .expect("serialize");
        assert_eq!(reachable["classification"], "reachable_daemon");
        assert_eq!(reachable["daemon_version"], "0.1.0");
        assert_eq!(reachable["name"], "host-b");
        assert_eq!(reachable["netbird_ip"], "100.92.30.40");

        let skew = serde_json::to_value(HostClass::VersionMismatch {
            daemon_protocol_version: 2,
        })
        .expect("serialize");
        assert_eq!(skew["classification"], "version_mismatch");
        assert_eq!(skew["daemon_protocol_version"], 2);

        let unreachable = serde_json::to_value(HostClass::Unreachable).expect("serialize");
        assert_eq!(unreachable["classification"], "unreachable");
        let candidate = serde_json::to_value(HostClass::Candidate).expect("serialize");
        assert_eq!(candidate["classification"], "candidate");
    }

    #[test]
    fn classify_response_keys_off_the_envelope_version() {
        use protocol::{ProtocolError, ProtocolVersion};

        let ours = PROTOCOL_VERSION.get();

        // Matching-version ok health -> reachable; daemon_version from the body.
        let ok = Response::ok(
            "id",
            serde_json::json!({ "daemon_version": "0.1.0", "protocol_version": ours }),
        );
        assert_eq!(
            classify_response(&ok),
            HostClass::ReachableDaemon {
                daemon_version: "0.1.0".to_owned()
            }
        );

        // The real wire shape of a skewed daemon: a version_mismatch *error*
        // whose envelope `v` carries the daemon's own (higher) version. Must
        // classify as version-mismatch, not unreachable (finding #1).
        let other = ours + 1;
        let mismatch = Response::Err {
            v: ProtocolVersion(other),
            id: "id".to_owned(),
            err: ProtocolError::version_mismatch(PROTOCOL_VERSION, ProtocolVersion(other)),
        };
        assert_eq!(
            classify_response(&mismatch),
            HostClass::VersionMismatch {
                daemon_protocol_version: other
            }
        );

        // A matching-version daemon that errored on health is not usable.
        let same_version_err = Response::err("id", ProtocolError::bad_request("nope"));
        assert_eq!(classify_response(&same_version_err), HostClass::Unreachable);

        // A matching-version ok health with no daemon_version field still
        // classifies as reachable, with a placeholder version.
        let ok_no_version = Response::ok("id", serde_json::json!({ "status": "ok" }));
        assert_eq!(
            classify_response(&ok_no_version),
            HostClass::ReachableDaemon {
                daemon_version: "<unknown>".to_owned()
            }
        );
    }

    #[test]
    fn probe_target_probes_any_peer_with_an_ip_regardless_of_connection_state() {
        let port = 7421;

        // Connected peer with an IP -> probed at <ip>:<port>.
        let connected = netbird::Peer {
            netbird_ip: Some("100.92.30.40".to_owned()),
            connection_status: Some("Connected".to_owned()),
            ..netbird::Peer::default()
        };
        assert_eq!(
            probe_target(&connected, port),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(100, 92, 30, 40)),
                port
            ))
        );

        // Idle peer with an IP -> still probed; skipping it would hide a
        // reachable daemon on a lazily-connected peer (finding #4).
        let idle = netbird::Peer {
            netbird_ip: Some("100.92.30.41".to_owned()),
            connection_status: Some("Idle".to_owned()),
            ..netbird::Peer::default()
        };
        assert_eq!(
            probe_target(&idle, port),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(100, 92, 30, 41)),
                port
            ))
        );

        // Peer with no IP -> nothing to dial, stays a Candidate.
        let no_ip = netbird::Peer {
            netbird_ip: None,
            connection_status: Some("Connected".to_owned()),
            ..netbird::Peer::default()
        };
        assert_eq!(probe_target(&no_ip, port), None);

        // Peer advertising a NON-NetBird IP -> never dialed (fail-closed against
        // a loopback / cloud-metadata / LAN address slipping through). Stays a
        // Candidate rather than becoming an SSRF probe.
        for hostile in ["127.0.0.1", "169.254.169.254", "10.0.0.1", "8.8.8.8"] {
            let peer = netbird::Peer {
                netbird_ip: Some(hostile.to_owned()),
                connection_status: Some("Connected".to_owned()),
                ..netbird::Peer::default()
            };
            assert_eq!(
                probe_target(&peer, port),
                None,
                "non-NetBird IP {hostile} must not be dialed"
            );
        }
    }

    #[tokio::test]
    async fn classify_closed_port_is_unreachable() {
        // Bind then immediately drop the listener so the port is closed; a
        // connect attempt is refused, which must classify as Unreachable.
        let listener = TcpListener::bind(loopback(0)).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);

        assert_eq!(
            classify(addr, Duration::from_millis(500)).await,
            HostClass::Unreachable
        );
    }

    /// Drain one newline-terminated request line from a stub socket; returns
    /// `false` if the peer closed before sending one.
    async fn drain_request_line(sock: &mut tokio::net::TcpStream) -> bool {
        let mut byte = [0_u8; 1];
        loop {
            match sock.read(&mut byte).await {
                Ok(0) | Err(_) => return false,
                Ok(_) if byte[0] == b'\n' => return true,
                Ok(_) => {}
            }
        }
    }

    /// A tiny stub daemon that answers exactly one `daemon.health` with an `ok`
    /// body, stamping the response envelope with `protocol_version`, then closes.
    /// Returns its bound address.
    async fn spawn_health_stub(protocol_version: u32, daemon_version: &str) -> SocketAddr {
        let listener = TcpListener::bind(loopback(0)).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let daemon_version = daemon_version.to_owned();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                if !drain_request_line(&mut sock).await {
                    return;
                }
                let reply = serde_json::json!({
                    "v": protocol_version,
                    "id": "stub",
                    "ok": {
                        "status": "ok",
                        "daemon_version": daemon_version,
                        "protocol_version": protocol_version,
                    }
                });
                let mut line = serde_json::to_vec(&reply).expect("serialize");
                line.push(b'\n');
                let _ = sock.write_all(&line).await;
                let _ = sock.flush().await;
            }
        });
        addr
    }

    /// A stub daemon that negotiates like the real one: because its protocol
    /// version differs from the client's, it rejects the probe with a
    /// `version_mismatch` *error* whose envelope `v` is the daemon's own version
    /// — exactly what `crates/daemon`'s `negotiate` emits before the health
    /// handler ever runs. Returns its bound address.
    async fn spawn_version_mismatch_stub(daemon_protocol_version: u32) -> SocketAddr {
        let listener = TcpListener::bind(loopback(0)).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                if !drain_request_line(&mut sock).await {
                    return;
                }
                let reply = serde_json::json!({
                    "v": daemon_protocol_version,
                    "id": "stub",
                    "err": {
                        "class": "daemon",
                        "code": "version_mismatch",
                        "msg": "client protocol version is incompatible with daemon",
                        "recover": "upgrade the older side",
                    }
                });
                let mut line = serde_json::to_vec(&reply).expect("serialize");
                line.push(b'\n');
                let _ = sock.write_all(&line).await;
                let _ = sock.flush().await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn classify_matching_daemon_is_reachable() {
        let addr = spawn_health_stub(PROTOCOL_VERSION.get(), "0.1.0").await;
        assert_eq!(
            classify(addr, PROBE_TIMEOUT).await,
            HostClass::ReachableDaemon {
                daemon_version: "0.1.0".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn classify_real_version_mismatch_daemon_is_mismatch() {
        // End-to-end over a loopback socket: a skewed daemon answers the probe
        // with a version_mismatch error (not an ok body). The daemon's version
        // must surface as VersionMismatch instead of collapsing to Unreachable
        // (finding #1 / DoD #2, #7).
        let other = PROTOCOL_VERSION.get() + 1;
        let addr = spawn_version_mismatch_stub(other).await;
        assert_eq!(
            classify(addr, PROBE_TIMEOUT).await,
            HostClass::VersionMismatch {
                daemon_protocol_version: other
            }
        );
    }

    #[test]
    fn renders_capabilities_table() {
        let caps = HostCapabilities {
            daemon_version: "0.1.0".to_owned(),
            protocol_version: ProtocolVersion(1),
            supported_agents: vec![AgentKind::Shell, AgentKind::Codex, AgentKind::Claude],
            runtimes: vec![
                AgentRuntime {
                    agent: AgentKind::Shell,
                    available: true,
                    path: None,
                },
                AgentRuntime {
                    agent: AgentKind::Claude,
                    available: true,
                    path: Some("/usr/bin/claude".to_owned()),
                },
            ],
            git_available: true,
            worktree_supported: true,
        };
        let output = render_capabilities_human("host-b", &caps);
        assert!(output.contains("host host-b capabilities"));
        assert!(output.contains("daemon_version:     0.1.0"));
        assert!(output.contains("protocol_version:   1"));
        assert!(output.contains("shell, codex, claude"));
        assert!(output.contains("claude   available=true  path=/usr/bin/claude"));
        assert!(output.contains("shell    available=true  path=-"));
    }

    #[test]
    fn renders_discovery_table_with_each_classification() {
        let records = vec![
            HostRecord {
                name: Some("host-b".to_owned()),
                fqdn: Some("host-b.netbird.cloud".to_owned()),
                netbird_ip: Some("100.92.30.40".to_owned()),
                class: HostClass::ReachableDaemon {
                    daemon_version: "0.1.0".to_owned(),
                },
            },
            HostRecord {
                name: Some("host-c".to_owned()),
                fqdn: Some("host-c.netbird.cloud".to_owned()),
                netbird_ip: Some("100.92.30.41".to_owned()),
                class: HostClass::Candidate,
            },
        ];
        let output = render_records_human(&records);
        let header = output.lines().next().expect("header");
        for column in ["NAME", "NETBIRD_IP", "STATUS", "VERSION"] {
            assert!(header.contains(column), "header missing {column}: {header}");
        }
        assert!(output.contains("host-b"));
        assert!(output.contains("reachable"));
        assert!(output.contains("0.1.0"));
        assert!(output.contains("candidate"));
    }
}
