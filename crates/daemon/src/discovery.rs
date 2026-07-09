//! `NetBird` host discovery with a short-lived TTL cache.
//!
//! Enumerates the local host's `NetBird` peers and classifies each by probing its
//! daemon control port (TCP connect + one `daemon.health` exchange). With many
//! mostly-dead peers and a per-probe timeout, a full probe is slow, so the result
//! is cached for [`DISCOVERY_CACHE_TTL`]: repeated calls (e.g. every launcher
//! keypress) return the cached snapshot instantly, while the first or an expired
//! call re-probes.
//!
//! The probe logic mirrors the original CLI implementation (`host discover`); it
//! moved into the daemon so the CLI becomes a thin client. Classification keys
//! off the **response envelope's protocol version**, which a daemon stamps on
//! every reply — including the `version_mismatch` error it returns when
//! negotiation rejects an incompatible client — so a version-skewed daemon is
//! told apart from an absent one.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use protocol::{
    method, HostClass, HostRecord, Request, Response, MAX_CONTROL_LINE_BYTES, PROTOCOL_VERSION,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// How long a discovery snapshot stays fresh before the next call re-probes.
///
/// Kept short so a host that goes up or down is reflected within seconds, but
/// long enough that a burst of launcher keypresses coalesces onto one snapshot.
const DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(30);

/// How long to wait for a single peer probe (TCP connect + one health exchange)
/// before classifying it as unreachable. Kept short so discovery over many peers
/// stays responsive; probes run concurrently regardless.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// A transport/parse failure while probing a peer. Swallowed by [`classify`]
/// (every such failure collapses to [`HostClass::Unreachable`]), so its only
/// job is to short-circuit [`probe_health`].
type ProbeError = Box<dyn std::error::Error + Send + Sync>;

/// A short-lived cache of the most recent discovery snapshot.
///
/// Clone is cheap (an `Arc`); all clones share the same cached entry. The lock
/// is held across the probe so a burst of concurrent calls coalesces into one
/// probe rather than racing N — acceptable for this single-user tool.
#[derive(Clone, Default, Debug)]
pub struct DiscoveryCache(Arc<Mutex<Option<CacheEntry>>>);

/// One cached discovery result and when it was produced.
#[derive(Debug)]
struct CacheEntry {
    /// When [`records`] last finished probing.
    fetched: Instant,
    /// The records from that probe.
    records: Vec<HostRecord>,
}

impl DiscoveryCache {
    /// Return the discovery records, probing only when necessary.
    ///
    /// With `force == false` a still-fresh cached snapshot (younger than
    /// [`DISCOVERY_CACHE_TTL`]) is returned without probing. Otherwise — `force`,
    /// an empty cache, or an expired entry — peers are re-probed, the result is
    /// cached, and returned. The lock is held across the probe so concurrent
    /// callers share the single in-flight probe instead of each starting their
    /// own.
    pub async fn records(&self, force: bool) -> Result<Vec<HostRecord>, netbird::NetbirdError> {
        let mut guard = self.0.lock().await;
        if !force {
            if let Some(entry) = guard.as_ref() {
                if entry.fetched.elapsed() < DISCOVERY_CACHE_TTL {
                    return Ok(entry.records.clone());
                }
            }
        }
        let records = discover_records().await?;
        *guard = Some(CacheEntry {
            fetched: Instant::now(),
            records: records.clone(),
        });
        Ok(records)
    }
}

/// Enumerate `NetBird` peers and build a classified record for each.
///
/// Every peer that advertises a parseable `NetBird` IP is probed concurrently —
/// regardless of its `NetBird` connection state. An `Idle` peer (lazy connection
/// not yet established) still has a routable `NetBird` address whose daemon may be
/// reachable; dialing it establishes the tunnel on demand. Only a peer with no
/// usable IP — nothing to dial — is left a [`HostClass::Candidate`].
async fn discover_records() -> Result<Vec<HostRecord>, netbird::NetbirdError> {
    let status = netbird::run_status()?;
    let port = netbird::remote_port()?;

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
/// A peer is probed whenever it advertises a parseable IP **inside the `NetBird`
/// CGNAT range** — its `NetBird` connection state is deliberately *not* a gate. An
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
async fn classify(addr: SocketAddr, timeout: Duration) -> HostClass {
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
async fn probe_health(addr: SocketAddr) -> Result<Response, ProbeError> {
    let mut stream = TcpStream::connect(addr).await?;

    // The control protocol is newline-delimited JSON. Send one request line.
    let request = Request::new(probe_request_id(), method::DAEMON_HEALTH, Value::Null);
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;

    // Read a single response line, capped so a misbehaving peer cannot exhaust
    // memory. We stop at the first newline.
    let reply = read_line(&mut stream, MAX_CONTROL_LINE_BYTES).await?;
    Ok(serde_json::from_slice(&reply)?)
}

/// A unique correlation id for one probe request.
///
/// Format: `daemon-discover-<seq>`. The prefix keeps ids readable in a probed
/// host's logs; the sequence makes every id distinct across the concurrent
/// probes one discovery fires, so each line correlates to exactly its own probe.
fn probe_request_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("daemon-discover-{seq}")
}

/// Read bytes up to and excluding the first `\n`, bounded by `max`.
///
/// Returns a framing error if the cap is hit before a newline or the peer closes
/// the connection without sending one.
async fn read_line(stream: &mut TcpStream, max: usize) -> Result<Vec<u8>, ProbeError> {
    let mut buf = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err("peer closed the connection without a response".into());
        }
        if byte[0] == b'\n' {
            return Ok(buf);
        }
        if buf.len() >= max {
            return Err("probe response exceeded maximum length".into());
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
                .map_or_else(|| "<unknown>".to_owned(), str::to_owned),
        },
        // A matching-version daemon that errored on health is not usable.
        Response::Err { .. } => HostClass::Unreachable,
    }
}

/// The first DNS label of a fqdn (e.g. `host-b` from `host-b.netbird.cloud`).
fn short_hostname(fqdn: &str) -> &str {
    fqdn.split('.').next().unwrap_or(fqdn)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use protocol::ProtocolVersion;
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
        // reachable daemon on a lazily-connected peer.
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

    #[test]
    fn classify_response_keys_off_the_envelope_version() {
        use protocol::ProtocolError;

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
        // classify as version-mismatch, not unreachable.
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
        // must surface as VersionMismatch instead of collapsing to Unreachable.
        let other = PROTOCOL_VERSION.get() + 1;
        let addr = spawn_version_mismatch_stub(other).await;
        assert_eq!(
            classify(addr, PROBE_TIMEOUT).await,
            HostClass::VersionMismatch {
                daemon_protocol_version: other
            }
        );
    }

    /// A fresh cached entry is returned without probing, and repeated reads
    /// return the same snapshot. (We cannot stub `run_status`, so the freshness
    /// branch is exercised by pre-populating the cache; `force` would re-probe
    /// — and thus call `NetBird` — so it is not exercised here.)
    #[tokio::test]
    async fn fresh_cache_is_served_without_probing() {
        let records = vec![HostRecord {
            name: Some("host-b".to_owned()),
            fqdn: Some("host-b.netbird.cloud".to_owned()),
            netbird_ip: Some("100.92.30.40".to_owned()),
            class: HostClass::ReachableDaemon {
                daemon_version: "0.1.0".to_owned(),
            },
        }];

        let cache = DiscoveryCache::default();
        // Pre-populate with a snapshot that is fresh (fetched just now). If
        // `records(false)` re-probed it would call NetBird and almost certainly
        // not return this exact host, so getting it back proves the freshness
        // branch served the cache.
        *cache.0.lock().await = Some(CacheEntry {
            fetched: Instant::now(),
            records: records.clone(),
        });

        let first = cache.records(false).await.expect("served from cache");
        assert_eq!(first, records);
        // A second read still serves the same fresh snapshot.
        let second = cache.records(false).await.expect("served from cache");
        assert_eq!(second, records);
    }
}
