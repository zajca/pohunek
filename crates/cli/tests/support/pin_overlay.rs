//! A fake `netbird` whose peer table changes once a connection was dialed.
//!
//! Scenarios of the host pin use it to move a short name to another peer
//! between the connections of one CLI operation. The table switches when the
//! connect interposer (`connect_redirect.rs`) has recorded its first dialed
//! `NetBird` address, so every lookup before the first connection sees the
//! `before` table and every later lookup sees the `after` table.

// Rust guideline compliant 2026-10-20

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// One peer of a fake `netbird status --json` table.
pub(crate) struct Peer {
    /// Provider-qualified DNS name, absent for a peer reported without one.
    pub(crate) fqdn: Option<&'static str>,
    /// `NetBird` address of the peer.
    pub(crate) ip: &'static str,
    /// Provider peer key, absent for a peer reported without one.
    pub(crate) key: Option<&'static str>,
}

impl Peer {
    /// A peer with a name, an address and a key.
    pub(crate) const fn new(fqdn: &'static str, ip: &'static str, key: &'static str) -> Self {
        Self {
            fqdn: Some(fqdn),
            ip,
            key: Some(key),
        }
    }
}

/// The `status --json` document of a machine named `own` with `peers`.
pub(crate) fn status(own: Option<(&str, &str)>, peers: &[Peer]) -> String {
    let details: Vec<Value> = peers
        .iter()
        .map(|peer| {
            let mut entry = json!({ "netbirdIp": peer.ip, "status": "Connected" });
            if let Some(fqdn) = peer.fqdn {
                entry["fqdn"] = json!(fqdn);
            }
            if let Some(key) = peer.key {
                entry["publicKey"] = json!(key);
            }
            entry
        })
        .collect();
    let mut document = json!({ "peers": { "details": details } });
    if let Some((fqdn, ip)) = own {
        document["fqdn"] = json!(fqdn);
        document["netbirdIp"] = json!(ip);
    }
    document.to_string()
}

/// Installs the fake `netbird` in `bin`; it prints `before` until `capture`
/// is non-empty and `after` from then on. Returns the `PATH` value to run the
/// CLI with.
pub(crate) fn install(bin: &Path, capture: &Path, before: &str, after: &str) -> std::ffi::OsString {
    fs::create_dir_all(bin).expect("create fixture bin");
    let netbird = bin.join("netbird");
    let script = format!(
        "#!/bin/sh\nif [ -s '{capture}' ]; then\ncat <<'POHUNEK_EOF'\n{after}\nPOHUNEK_EOF\nelse\ncat <<'POHUNEK_EOF'\n{before}\nPOHUNEK_EOF\nfi\n",
        capture = capture.display()
    );
    pohunek_test_support::fs::write_file(&netbird, script).expect("write fake netbird");
    fs::set_permissions(&netbird, fs::Permissions::from_mode(0o700))
        .expect("make fake netbird executable");
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![PathBuf::from(bin)];
    paths.extend(std::env::split_paths(&inherited_path));
    std::env::join_paths(paths).expect("join PATH")
}

/// Like [`install`], but every run after the first dialed connection hangs for
/// `stall` seconds instead of answering.
pub(crate) fn install_stalling(
    bin: &Path,
    capture: &Path,
    before: &str,
    stall_seconds: u32,
) -> std::ffi::OsString {
    fs::create_dir_all(bin).expect("create fixture bin");
    let netbird = bin.join("netbird");
    let script = format!(
        "#!/bin/sh\nif [ -s '{capture}' ]; then\nexec sleep {stall_seconds}\nfi\ncat <<'POHUNEK_EOF'\n{before}\nPOHUNEK_EOF\n",
        capture = capture.display()
    );
    pohunek_test_support::fs::write_file(&netbird, script).expect("write fake netbird");
    fs::set_permissions(&netbird, fs::Permissions::from_mode(0o700))
        .expect("make fake netbird executable");
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![PathBuf::from(bin)];
    paths.extend(std::env::split_paths(&inherited_path));
    std::env::join_paths(paths).expect("join PATH")
}

/// The `NetBird` addresses the CLI dialed, in order, without ports.
pub(crate) fn dialed_ips(capture: &Path) -> Vec<String> {
    fs::read_to_string(capture)
        .unwrap_or_default()
        .lines()
        .map(|line| line.split(':').next().unwrap_or(line).to_owned())
        .collect()
}

/// A loopback daemon that answers every `daemon.health` probe.
pub(crate) fn serve_health() -> u16 {
    use std::io::{BufRead as _, BufReader, Write as _};
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind the health fixture");
    let port = listener.local_addr().expect("fixture address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept a probe");
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
                    let request: protocol::Request =
                        serde_json::from_str(line.trim_end()).expect("a probe request");
                    let response = protocol::Response::ok(
                        protocol::PROTOCOL_VERSION,
                        request.id(),
                        serde_json::json!({
                            "status": "ok",
                            "daemon_version": "fixture",
                            "protocol_version": protocol::PROTOCOL_VERSION.get()
                        }),
                    )
                    .expect("a probe response");
                    writeln!(
                        reader.get_mut(),
                        "{}",
                        serde_json::to_string(&response).expect("serialize the response")
                    )
                    .expect("write the probe response");
                    line.clear();
                }
            });
        }
    });
    port
}

/// A loopback daemon that closes every connection without answering, so a
/// probe of it fails the way a daemon that is not listening does.
pub(crate) fn serve_unresponsive() -> u16 {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind the unresponsive fixture");
    let port = listener.local_addr().expect("fixture address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    port
}
