//! Parsing of `netbird status --json` and the subprocess that produces it.
//!
//! `NetBird`'s JSON output drifts across versions. Two shapes appear in the wild
//! and the types here tolerate both:
//!
//! - Shape A (current source: `OutputOverview` / `PeerStateDetailOutput`): the
//!   self IP is a root-level `netbirdIp` (no CIDR mask), peers live under
//!   `peers.details`, and each peer uses `status` / `connectionType`.
//! - Shape B (legacy/docs): the self IP is `localPeerState.IP` (which may carry
//!   a CIDR mask), `peers` is a bare array, and each peer uses
//!   `connectionStatus` / `connType`.
//!
//! Defensive parsing rules: unknown fields are ignored, all fields are
//! optional and default, and the parser never panics on real-world drift.

use std::process::Command;

use serde::de::Deserializer;
use serde::Deserialize;

use crate::{is_netbird_ip, parse_addr_strip_cidr};

/// Subcommand and flag used to ask `NetBird` for machine-readable status.
const NETBIRD_STATUS_ARGS: [&str; 2] = ["status", "--json"];
/// Default program name for the `NetBird` CLI (resolved via the OS through PATH).
const NETBIRD_PROGRAM: &str = "netbird";
/// Maximum number of bytes of captured CLI output to surface in an error
/// message. Bounds the size of a [`NetbirdError::StateUnavailable`] detail so a
/// misbehaving CLI cannot flood logs or the agent context.
const MAX_ERROR_DETAIL_BYTES: usize = 512;

/// Errors raised while reading or interpreting `NetBird`'s local state.
#[derive(Debug, thiserror::Error)]
pub enum NetbirdError {
    /// The `netbird` CLI binary could not be found on `PATH`.
    #[error("the `netbird` CLI was not found on PATH")]
    CliMissing,
    /// `NetBird` is installed but its local state could not be read (daemon down,
    /// not logged in, or a non-zero exit). Carries a short, bounded detail.
    #[error("NetBird local state is unavailable: {0}")]
    StateUnavailable(String),
    /// The `netbird status --json` output could not be parsed.
    #[error("failed to parse `netbird status --json`: {0}")]
    Parse(String),
    /// The requested host name did not match any `NetBird` peer.
    #[error("host '{0}' was not found among NetBird peers")]
    HostUnknown(String),
}

/// This host's local peer state (shape B: `localPeerState`).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct LocalPeerState {
    /// This host's `NetBird` IP. The wire key is uppercase `IP`; a lowercase
    /// `ip` alias is accepted defensively. May carry a CIDR mask.
    #[serde(rename = "IP", alias = "ip")]
    ip: Option<String>,
    /// This host's fully qualified `NetBird` name, when present.
    fqdn: Option<String>,
}

/// A single `NetBird` peer (another host on the mesh).
///
/// Field names are stable across both documented shapes; serde attributes
/// capture the differing wire keys. Unknown per-peer keys are ignored.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Peer {
    /// The peer's fully qualified `NetBird` name (e.g. `host-b.netbird.cloud`).
    pub fqdn: Option<String>,
    /// The peer's `NetBird` IP as a string (wire key `netbirdIp` in both shapes).
    pub netbird_ip: Option<String>,
    /// Connection state. Shape A spells this `status`; shape B spells it
    /// `connectionStatus`. The rust field is named `connection_status` and
    /// renamed to `status` (shape A) with an alias for shape B.
    #[serde(rename = "status", alias = "connectionStatus")]
    pub connection_status: Option<String>,
    /// Connection type (e.g. `P2P`, `Relayed`). Shape A spells this
    /// `connectionType`; shape B spells it `connType`.
    #[serde(rename = "connectionType", alias = "connType")]
    pub connection_type: Option<String>,
}

impl Peer {
    /// True when this peer's connection status equals `"Connected"`
    /// (case-insensitive). Absent status is treated as not connected.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connection_status
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("Connected"))
    }

    /// The peer's `NetBird` IP, parsed and with any CIDR mask stripped.
    ///
    /// Lenient: returns whatever parses as an [`std::net::IpAddr`] (the range
    /// check is left to callers such as [`resolve_host`](crate::resolve_host)),
    /// or `None` if absent/unparseable.
    #[must_use]
    pub fn ip(&self) -> Option<std::net::IpAddr> {
        self.netbird_ip.as_deref().and_then(parse_addr_strip_cidr)
    }
}

/// A flat list of peers tolerant of both `NetBird` shapes.
///
/// Shape A nests peers under `{ "details": [...] }`; shape B is a bare array.
/// This newtype deserializes from either via an untagged representation and
/// always yields a flat `Vec<Peer>` (empty when absent).
#[derive(Debug, Clone, Default)]
struct PeersField(Vec<Peer>);

/// Internal untagged representation distinguishing the two `peers` encodings.
#[derive(Deserialize)]
#[serde(untagged)]
enum PeersRepr {
    /// Shape B: `peers` is a bare array of peer objects.
    List(Vec<Peer>),
    /// Shape A: `peers` is an object whose `details` holds the array. Other
    /// sibling keys (`connected`, `total`, ...) are ignored.
    Object {
        #[serde(default)]
        details: Vec<Peer>,
    },
}

impl<'de> Deserialize<'de> for PeersField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // `null` (or an absent value routed here) collapses to an empty list so
        // the struct-level default and an explicit `"peers": null` agree.
        let repr = Option::<PeersRepr>::deserialize(deserializer)?;
        Ok(match repr {
            None => PeersField(Vec::new()),
            Some(PeersRepr::List(peers)) => PeersField(peers),
            Some(PeersRepr::Object { details }) => PeersField(details),
        })
    }
}

/// A parsed `netbird status --json` snapshot.
///
/// Construct via [`parse_status`] or [`run_status`]. All fields are optional
/// and default; the accessors paper over the two `NetBird` shapes.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NetbirdStatus {
    /// Shape A self IP: root-level `netbirdIp` (no CIDR mask).
    netbird_ip: Option<String>,
    /// Shape B self IP source: `localPeerState` (the `IP` may carry a mask).
    local_peer_state: Option<LocalPeerState>,
    /// `NetBird` daemon version string, when reported.
    daemon_version: Option<String>,
    /// Daemon/root status string. Shape A reports `daemonStatus`; shape B
    /// reports a root `status`, captured via the alias.
    #[serde(alias = "status")]
    daemon_status: Option<String>,
    /// This host's fully qualified `NetBird` name, when present at the root.
    fqdn: Option<String>,
    /// Peers across both shapes, flattened to a single list.
    peers: PeersField,
}

impl NetbirdStatus {
    /// This host's `NetBird` IP with any CIDR mask stripped.
    ///
    /// Tries the shape A root `netbirdIp` first, then the shape B
    /// `localPeerState.IP`. The result is validated to parse as an
    /// [`std::net::IpAddr`] inside `100.64.0.0/10`; returns `None` if absent or
    /// outside the `NetBird` range (fail closed for the bind-address path).
    #[must_use]
    pub fn self_netbird_ip(&self) -> Option<std::net::IpAddr> {
        let candidate = self
            .netbird_ip
            .as_deref()
            .or_else(|| self.local_peer_state.as_ref().and_then(|s| s.ip.as_deref()));
        candidate
            .and_then(parse_addr_strip_cidr)
            .filter(|ip| is_netbird_ip(*ip))
    }

    /// All peers, flattened across both `NetBird` shapes.
    #[must_use]
    pub fn peers(&self) -> &[Peer] {
        &self.peers.0
    }

    /// The daemon/root status string, if present (for doctor messaging).
    #[must_use]
    pub fn status_text(&self) -> Option<&str> {
        self.daemon_status.as_deref()
    }
}

/// Parse the JSON text of `netbird status --json` into a [`NetbirdStatus`].
///
/// Pure and fixture-tested. Never panics on malformed input.
///
/// # Errors
///
/// Returns [`NetbirdError::Parse`] when `json` is not valid `netbird status`
/// output.
pub fn parse_status(json: &str) -> Result<NetbirdStatus, NetbirdError> {
    serde_json::from_str(json).map_err(|e| NetbirdError::Parse(e.to_string()))
}

/// Run `netbird status --json` (resolved on `PATH`) and parse it.
///
/// Errors:
/// - [`NetbirdError::CliMissing`] when the binary is not found (`ENOENT`).
/// - [`NetbirdError::StateUnavailable`] on a non-zero exit (daemon down / not
///   logged in), carrying a short trimmed detail from stderr/stdout.
/// - [`NetbirdError::Parse`] when the output is not valid status JSON.
pub fn run_status() -> Result<NetbirdStatus, NetbirdError> {
    run_status_with_program(NETBIRD_PROGRAM)
}

/// Like [`run_status`] but with an explicit program name.
///
/// Useful for tests (pass a non-existent program to deterministically hit
/// [`NetbirdError::CliMissing`]).
pub fn run_status_with_program(program: &str) -> Result<NetbirdStatus, NetbirdError> {
    let output = Command::new(program)
        .args(NETBIRD_STATUS_ARGS)
        .output()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => NetbirdError::CliMissing,
            _ => NetbirdError::StateUnavailable(format!("failed to run `{program} status`: {err}")),
        })?;

    if !output.status.success() {
        return Err(NetbirdError::StateUnavailable(non_zero_detail(&output)));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_status(&stdout)
}

/// Build a bounded, human-readable detail for a non-zero `netbird` exit.
///
/// Prefers stderr, falling back to stdout; trims whitespace and clamps the
/// length so a misbehaving CLI cannot flood the error path.
fn non_zero_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let detail = if stderr.is_empty() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.trim().to_owned()
    } else {
        stderr.to_owned()
    };

    let code = output
        .status
        .code()
        .map_or_else(|| "signal".to_owned(), |c| c.to_string());

    let detail = if detail.is_empty() {
        format!("exit {code}")
    } else {
        clamp(&detail, MAX_ERROR_DETAIL_BYTES)
    };
    detail
}

/// Clamp `s` to at most `max` bytes on a char boundary, appending an ellipsis
/// marker when truncated.
fn clamp(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    const STATUS_CURRENT: &str = include_str!("../tests/fixtures/status_current.json");
    const STATUS_LEGACY: &str = include_str!("../tests/fixtures/status_legacy.json");
    const STATUS_UNKNOWN_FIELDS: &str =
        include_str!("../tests/fixtures/status_unknown_fields.json");
    const STATUS_PEER_WITHOUT_IP: &str =
        include_str!("../tests/fixtures/status_peer_without_ip.json");
    const STATUS_OFFLINE: &str = include_str!("../tests/fixtures/status_offline.json");
    const STATUS_MINIMAL: &str = include_str!("../tests/fixtures/status_minimal.json");

    #[test]
    fn parses_shape_a_current() {
        let status = parse_status(STATUS_CURRENT).expect("shape A parses");
        assert_eq!(
            status.self_netbird_ip(),
            Some("100.92.10.20".parse::<IpAddr>().unwrap())
        );
        assert_eq!(status.status_text(), Some("Connected"));
        assert_eq!(status.daemon_version.as_deref(), Some("0.30.0"));
        let peers = status.peers();
        assert_eq!(peers.len(), 2);
        // First peer: Connected P2P.
        assert!(peers[0].is_connected());
        assert_eq!(peers[0].connection_type.as_deref(), Some("P2P"));
        assert_eq!(
            peers[0].ip(),
            Some("100.92.30.40".parse::<IpAddr>().unwrap())
        );
        // Second peer: Idle -> not connected.
        assert!(!peers[1].is_connected());
        assert_eq!(peers[1].connection_status.as_deref(), Some("Idle"));
    }

    #[test]
    fn parses_shape_b_legacy() {
        let status = parse_status(STATUS_LEGACY).expect("shape B parses");
        // localPeerState.IP carries a CIDR mask that must be stripped.
        assert_eq!(
            status.self_netbird_ip(),
            Some("100.64.0.10".parse::<IpAddr>().unwrap())
        );
        assert_eq!(status.status_text(), Some("Connected"));
        assert_eq!(status.daemon_version.as_deref(), Some("0.27.0"));
        let peers = status.peers();
        assert_eq!(peers.len(), 1);
        assert!(peers[0].is_connected());
        // Shape B uses connType / connectionStatus.
        assert_eq!(peers[0].connection_type.as_deref(), Some("P2P"));
        assert_eq!(
            peers[0].ip(),
            Some("100.64.0.20".parse::<IpAddr>().unwrap())
        );
        assert_eq!(peers[0].fqdn.as_deref(), Some("host-b.netbird.cloud"));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let status = parse_status(STATUS_UNKNOWN_FIELDS).expect("unknown fields tolerated");
        assert_eq!(
            status.self_netbird_ip(),
            Some("100.92.10.20".parse::<IpAddr>().unwrap())
        );
        // The known per-peer fields still parse despite unknown siblings.
        let peers = status.peers();
        assert_eq!(peers.len(), 1);
        assert!(peers[0].is_connected());
        assert_eq!(peers[0].fqdn.as_deref(), Some("host-b.netbird.cloud"));
    }

    #[test]
    fn peer_without_ip_yields_none() {
        let status = parse_status(STATUS_PEER_WITHOUT_IP).expect("missing peer ip tolerated");
        let peers = status.peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].ip(), None);
        assert_eq!(peers[0].netbird_ip, None);
        // Other fields still present.
        assert_eq!(peers[0].fqdn.as_deref(), Some("host-c.netbird.cloud"));
    }

    #[test]
    fn offline_status_has_no_self_ip() {
        let status = parse_status(STATUS_OFFLINE).expect("offline parses");
        assert_eq!(status.self_netbird_ip(), None);
        // Daemon status present but not Connected.
        assert_eq!(status.status_text(), Some("NeedsLogin"));
        // No connected peers.
        assert!(status.peers().iter().all(|p| !p.is_connected()));
    }

    #[test]
    fn minimal_empty_object_defaults() {
        let status = parse_status(STATUS_MINIMAL).expect("`{}` parses");
        assert_eq!(status.self_netbird_ip(), None);
        assert!(status.peers().is_empty());
        assert_eq!(status.status_text(), None);
    }

    #[test]
    fn malformed_json_is_parse_error_not_panic() {
        let err = parse_status("{ this is not json ]").unwrap_err();
        assert!(matches!(err, NetbirdError::Parse(_)));
        // Empty input is not valid JSON.
        assert!(matches!(
            parse_status("").unwrap_err(),
            NetbirdError::Parse(_)
        ));
        // Scalar root types cannot deserialize into the struct.
        assert!(matches!(
            parse_status("\"hello\"").unwrap_err(),
            NetbirdError::Parse(_)
        ));
        assert!(matches!(
            parse_status("42").unwrap_err(),
            NetbirdError::Parse(_)
        ));
        assert!(matches!(
            parse_status("true").unwrap_err(),
            NetbirdError::Parse(_)
        ));
        // A non-empty array cannot map to the struct's fields positionally.
        assert!(matches!(
            parse_status("[1, 2, 3]").unwrap_err(),
            NetbirdError::Parse(_)
        ));
    }

    #[test]
    fn self_ip_outside_netbird_range_is_rejected() {
        // A root netbirdIp that is a public address must NOT be returned.
        let json = r#"{ "netbirdIp": "8.8.8.8" }"#;
        let status = parse_status(json).unwrap();
        assert_eq!(status.self_netbird_ip(), None);
    }

    #[test]
    fn peers_null_collapses_to_empty() {
        let json = r#"{ "netbirdIp": "100.64.0.1", "peers": null }"#;
        let status = parse_status(json).unwrap();
        assert!(status.peers().is_empty());
    }

    #[test]
    fn missing_program_is_cli_missing() {
        let err = run_status_with_program("definitely-not-a-real-binary-xyz").unwrap_err();
        assert!(matches!(err, NetbirdError::CliMissing));
    }

    #[test]
    fn clamp_respects_char_boundaries() {
        // A multi-byte char at the cut point must not be split.
        let s = "ααααα"; // each 'α' is 2 bytes
        let clamped = clamp(s, 3);
        assert!(clamped.is_char_boundary(clamped.len()));
        assert!(clamped.ends_with('…'));
    }
}
