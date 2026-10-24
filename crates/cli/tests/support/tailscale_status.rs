//! Spec-derived `tailscale status --json` fixtures and a fake `tailscale`.
//!
//! The documents follow `ipnstate.Status` and `ipnstate.PeerStatus` of
//! `tailscale/tailscale@v1.104.2` (`ipn/ipnstate/ipnstate.go`): the field
//! names, a Go `nil` slice as `null`, `Tags`, `Expired` and `ShareeNode` only
//! when set (`omitempty`), the peer map keyed by the node public key, and a
//! `DNSName` with a trailing dot. The values are synthetic, so a recording of
//! a real client replaces or extends them in the acceptance run of #1112.

// Rust guideline compliant 2026-10-23

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// The tag of the Tailscale source the field layout is taken from.
pub(crate) const SPEC_TAG: &str = "v1.104.2";

/// `MagicDNS` suffix of the synthetic tailnet.
pub(crate) const SUFFIX: &str = "tail-test.ts.net";

/// One node of a status document: this host or a peer.
#[derive(Clone)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the flags mirror the independent booleans of ipnstate.PeerStatus"
)]
pub(crate) struct Node {
    pub(crate) id: String,
    pub(crate) host_name: String,
    pub(crate) dns_name: String,
    pub(crate) addresses: Vec<String>,
    pub(crate) online: bool,
    pub(crate) in_network_map: bool,
    pub(crate) expired: bool,
    pub(crate) sharee: bool,
    pub(crate) tags: Option<Vec<String>>,
    /// Seed of the map key; a re-key changes it and keeps `id`.
    pub(crate) key_seed: u8,
}

impl Node {
    /// A selectable peer named `name` (`<name>.tail-test.ts.net.`).
    pub(crate) fn peer(name: &str, id: &str, addresses: &[&str]) -> Self {
        Self {
            id: id.to_owned(),
            host_name: name.to_owned(),
            dns_name: format!("{name}.{SUFFIX}."),
            addresses: addresses.iter().map(|a| (*a).to_owned()).collect(),
            online: true,
            in_network_map: true,
            expired: false,
            sharee: false,
            tags: None,
            key_seed: 1,
        }
    }

    pub(crate) fn offline(mut self) -> Self {
        self.online = false;
        self
    }

    pub(crate) fn out_of_netmap(mut self) -> Self {
        self.in_network_map = false;
        self
    }

    pub(crate) fn expired(mut self) -> Self {
        self.expired = true;
        self
    }

    pub(crate) fn sharee(mut self) -> Self {
        self.sharee = true;
        self
    }

    pub(crate) fn tagged(mut self, tag: &str) -> Self {
        self.tags = Some(vec![tag.to_owned()]);
        self
    }

    pub(crate) fn rekeyed(mut self) -> Self {
        self.key_seed += 1;
        self
    }

    pub(crate) fn with_host_name(mut self, host_name: &str) -> Self {
        host_name.clone_into(&mut self.host_name);
        self
    }

    /// The `PeerStatus` object, with the `omitempty` fields only when set.
    fn to_json(&self) -> Value {
        let mut node = Map::new();
        node.insert("ID".into(), json!(self.id));
        node.insert("NodeID".into(), json!(format!("n{}", self.key_seed)));
        node.insert("PublicKey".into(), json!(self.map_key()));
        node.insert("HostName".into(), json!(self.host_name));
        node.insert("DNSName".into(), json!(self.dns_name));
        node.insert("OS".into(), json!("linux"));
        node.insert("UserID".into(), json!(1_000_000_001_i64));
        node.insert("TailscaleIPs".into(), json!(self.addresses));
        if let Some(tags) = &self.tags {
            node.insert("Tags".into(), json!(tags));
        }
        node.insert("Addrs".into(), Value::Null);
        node.insert("CurAddr".into(), json!(""));
        node.insert("Relay".into(), json!("fra"));
        node.insert("RxBytes".into(), json!(0));
        node.insert("TxBytes".into(), json!(0));
        node.insert("Online".into(), json!(self.online));
        node.insert("ExitNode".into(), json!(false));
        node.insert("ExitNodeOption".into(), json!(false));
        node.insert("Active".into(), json!(false));
        node.insert("PeerAPIURL".into(), Value::Null);
        node.insert("InNetworkMap".into(), json!(self.in_network_map));
        node.insert("InMagicSock".into(), json!(self.in_network_map));
        node.insert("InEngine".into(), json!(self.in_network_map));
        if self.sharee {
            node.insert("ShareeNode".into(), json!(true));
        }
        if self.expired {
            node.insert("Expired".into(), json!(true));
        }
        Value::Object(node)
    }

    /// The `nodekey:<64 hex>` map key: the node `ID` and the key seed, so each
    /// node has its own key and a re-key changes only the key.
    fn map_key(&self) -> String {
        use std::fmt::Write as _;
        let mut hex = self.id.bytes().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
        let _ = write!(hex, "{:02x}", self.key_seed);
        format!("nodekey:{hex:0<64}")
    }
}

/// This host: node `n-self`, one IPv4 and one IPv6 address.
pub(crate) fn this_host(ipv4: &str) -> Node {
    Node::peer("self-box", "nSELF00001CNTRL", &[ipv4, "fd7a:115c:a1e0::7"])
}

/// A status document.
pub(crate) struct Status {
    pub(crate) backend_state: &'static str,
    pub(crate) tun: bool,
    pub(crate) this: Option<Node>,
    pub(crate) peers: Vec<Node>,
}

impl Status {
    /// A running node on a kernel interface with the given peers.
    pub(crate) fn running(this_ipv4: &str, peers: Vec<Node>) -> Self {
        Self {
            backend_state: "Running",
            tun: true,
            this: Some(this_host(this_ipv4)),
            peers,
        }
    }

    pub(crate) fn with_state(mut self, state: &'static str) -> Self {
        self.backend_state = state;
        self
    }

    pub(crate) fn userspace(mut self) -> Self {
        self.tun = false;
        self
    }

    pub(crate) fn with_this(mut self, this: Node) -> Self {
        self.this = Some(this);
        self
    }

    /// The JSON text, with fields the provider does not read (`Health`,
    /// `User`, `CurrentTailnet`) present and Go `nil` values as `null`.
    pub(crate) fn render(&self) -> String {
        let peers: Option<Map<String, Value>> = (!self.peers.is_empty()).then(|| {
            self.peers
                .iter()
                .map(|peer| (peer.map_key(), peer.to_json()))
                .collect()
        });
        let this_addresses = self
            .this
            .as_ref()
            .map_or_else(Vec::new, |this| this.addresses.clone());
        json!({
            "Version": format!("{SPEC_TAG}-fixture"),
            "TUN": self.tun,
            "BackendState": self.backend_state,
            "HaveNodeKey": true,
            "AuthURL": "",
            "TailscaleIPs": if this_addresses.is_empty() { Value::Null } else { json!(this_addresses) },
            "Self": self.this.as_ref().map(Node::to_json),
            "Health": Value::Null,
            "MagicDNSSuffix": SUFFIX,
            "CurrentTailnet": { "Name": "synthetic", "MagicDNSSuffix": SUFFIX, "MagicDNSEnabled": true },
            "CertDomains": Value::Null,
            "Peer": peers.map_or(Value::Null, Value::Object),
            "User": { "1000000001": { "ID": 1_000_000_001_i64, "LoginName": "owner@example.invalid" } },
        })
        .to_string()
    }
}

/// A fake `tailscale` on a directory of its own.
///
/// The executable reads the one line of its status file, then logs one line
/// per invocation, then prints the document, using shell builtins only because the scenarios run it
/// with a `PATH` that holds nothing else, so a scenario can rewrite the status between
/// invocations and count them.
pub(crate) struct FakeTailscale {
    bin: PathBuf,
    status: PathBuf,
    calls: PathBuf,
}

impl FakeTailscale {
    /// Describes a fake under `root` without installing it.
    pub(crate) fn new(root: &Path) -> Self {
        let bin = root.join("bin");
        Self {
            status: root.join("tailscale-status.json"),
            calls: root.join("tailscale-calls.log"),
            bin,
        }
    }

    /// The directory to put on `PATH`.
    pub(crate) fn bin_dir(&self) -> &Path {
        &self.bin
    }

    /// Publishes `status` and installs the executable.
    pub(crate) fn install(&self, status: &Status) {
        self.set_status_text(&status.render());
        fs::create_dir_all(&self.bin).expect("create the fake bin directory");
        pohunek_test_support::fs::write_executable(
            self.bin.join("tailscale"),
            format!(
                "#!/bin/sh\nIFS= read -r document < '{}'\nprintf 'invoked\\n' >> '{}'\nprintf '%s\\n' \"$document\"\n",
                self.status.display(),
                self.calls.display()
            ),
        )
        .expect("write the fake tailscale");
    }

    /// Replaces the document the next invocation prints.
    pub(crate) fn set_status(&self, status: &Status) {
        self.set_status_text(&status.render());
    }

    /// Replaces the output with arbitrary text.
    pub(crate) fn set_status_text(&self, text: &str) {
        let staging = self.status.with_extension("json.new");
        fs::write(&staging, text).expect("write the staged status");
        fs::rename(&staging, &self.status).expect("publish the status atomically");
    }

    /// How many times the CLI ran. A run is counted only after it read the
    /// document, so a count of `n` means `n` documents were served.
    pub(crate) fn invocations(&self) -> usize {
        fs::read_to_string(&self.calls).map_or(0, |text| text.lines().count())
    }
}
