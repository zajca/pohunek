//! Host discovery payloads.
//!
//! These types define the JSON shape carried by `host.discover` (see
//! `crate::method::HOST_DISCOVER`). Discovery enumerates the peers of the host's
//! configured overlay and classifies each by probing its daemon control port, so the operator
//! (and any host-switching client) sees which peers run a compatible daemon.
//!
//! The CLI performs peer enumeration plus bounded concurrent probing directly,
//! using a short owner-private persistent cache; it therefore needs local
//! overlay state but not a local daemon. The daemon retains this RPC and its
//! own memory cache for web and other protocol clients.
//!
//! Like every other protocol payload these are additive: unknown fields are
//! ignored and absent optional fields default, so a newer peer and an older peer
//! interoperate on the common subset.

use serde::{Deserialize, Serialize};

/// How an overlay peer is classified for `host.discover`.
///
/// Serializes with an internal `classification` tag so a `--json` consumer
/// can branch on it, e.g.
/// `{"classification":"reachable_daemon","daemon_version":"0.1.0"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostClass.ts"))]
#[serde(rename_all = "snake_case", tag = "classification")]
pub enum HostClass {
    /// A compatible daemon answered with a protocol version inside the asking
    /// client's range.
    ReachableDaemon {
        /// The daemon version the peer reported. At most
        /// [`MAX_HOST_DAEMON_VERSION_BYTES`] bytes when present.
        daemon_version: String,
    },
    /// A daemon answered but speaks a protocol version outside the asking
    /// client's range.
    VersionMismatch {
        /// The protocol version the peer's daemon reported.
        daemon_protocol_version: u32,
    },
    /// The peer advertises an overlay-range IP but its daemon port could not be
    /// reached, or it returned no usable health response.
    Unreachable,
    /// The peer had no overlay-range IP to dial, so it was not probed.
    Candidate,
}

/// Maximum bytes of a [`HostRecord::name`]: one DNS label.
pub const MAX_HOST_NAME_BYTES: usize = 63;

/// Maximum bytes of a [`HostRecord::fqdn`]: one DNS name.
pub const MAX_HOST_FQDN_BYTES: usize = 253;

/// Maximum bytes of a [`HostRecord::peer_id`]: a provider key (a `NetBird`
/// public key or a Tailscale node ID) with margin.
pub const MAX_HOST_PEER_ID_BYTES: usize = 64;

/// Maximum bytes of the `daemon_version` a [`HostClass::ReachableDaemon`]
/// carries: a release version string with margin.
pub const MAX_HOST_DAEMON_VERSION_BYTES: usize = 32;

/// Maximum bytes of a [`HostRecord::reason`] value; every defined reason code
/// fits inside it.
pub const MAX_HOST_REASON_BYTES: usize = 32;

/// `reason` value: the peer's address was withheld because another peer or
/// this machine lists it too.
pub const REASON_ADDRESS_AMBIGUOUS: &str = "address_ambiguous";

/// `reason` value: one of the peer's identity strings (`name`, `fqdn`,
/// `peer_id`) exceeded its field cap, so that field is omitted and not echoed.
pub const REASON_IDENTITY_TOO_LARGE: &str = "identity_too_large";

/// `reason` value: the peer's health reply carried a `daemon_version` over its
/// cap, so the peer is reported unreachable and the version is not echoed.
pub const REASON_DAEMON_VERSION_TOO_LARGE: &str = "daemon_version_too_large";

/// `reason` value on the local record: its own daemon did not answer within
/// the probe timeout.
pub const REASON_LOCAL_PROBE_TIMEOUT: &str = "local_probe_timeout";

/// `reason` value on the local record: the probe of its own daemon failed.
pub const REASON_LOCAL_PROBE_FAILED: &str = "local_probe_failed";

/// `reason` value on the local record: its own daemon answered with an error.
pub const REASON_LOCAL_DAEMON_ERROR: &str = "local_daemon_error";

/// One enumerated host with its overlay identity and classification.
///
/// Field order and names are part of the wire contract `--json` consumers parse.
/// The peer-supplied strings (`name`, `fqdn`, `peer_id`, the class's
/// `daemon_version` and `reason`) are capped at build time: a value over its
/// cap is omitted and reported through [`HostRecord::reason`], never echoed
/// verbatim, so one hostile peer cannot inflate the reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostRecord.ts"))]
pub struct HostRecord {
    /// Short host name (first DNS label of the fqdn), when derivable. At most
    /// [`MAX_HOST_NAME_BYTES`] bytes when present.
    pub name: Option<String>,
    /// The peer's provider-qualified DNS name. At most [`MAX_HOST_FQDN_BYTES`]
    /// bytes when present.
    pub fqdn: Option<String>,
    /// The peer's dialable IP address without a port.
    pub address: Option<String>,
    /// Effective daemon port configured for this overlay route.
    pub port: u16,
    /// Which overlay transport discovered this peer (e.g. `"netbird"`).
    pub overlay: String,
    /// The overlay's stable provider peer identity (e.g. a `NetBird` public key or
    /// a Tailscale node ID). At most [`MAX_HOST_PEER_ID_BYTES`] bytes when present.
    pub peer_id: Option<String>,
    /// Whether this record is the machine that enumerated the hosts rather than
    /// a peer. Omitted when `false`, so a peer record is unchanged on the wire
    /// and an older reader decodes the field as absent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
    pub local: bool,
    /// Why this record is not usable as probed or echoed: one of the
    /// `REASON_*` codes, at most [`MAX_HOST_REASON_BYTES`] bytes. Set on the
    /// local record when its own daemon did not answer, on a peer record whose
    /// probe failed in a way the classifier can name, or whose identity
    /// strings were over cap. Omitted otherwise, so a peer record is unchanged
    /// on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub reason: Option<String>,
    /// Classification (flattened so its fields sit alongside the record).
    #[serde(flatten)]
    pub class: HostClass,
}

/// Optional parameters for `host.discover`.
///
/// `force` bypasses the daemon's discovery cache and re-probes immediately;
/// omitted/false returns the cached snapshot when it is still fresh.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostDiscoverParams.ts"))]
pub struct HostDiscoverParams {
    /// Skip the cache and re-probe now.
    #[serde(default)]
    pub force: bool,
}

/// Parameters for `host.resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostResolveParams.ts"))]
pub struct HostResolveParams {
    /// The host route as `--host` accepts it: a short name, an FQDN, an overlay
    /// IP, an identity selector (`peer~<id>`, `fqdn~<name>`), an
    /// `<overlay>:`-qualified form, each optionally followed by `@<port>`.
    pub route: String,
}

/// What `host.resolve` selected.
///
/// Serializes with an internal `target` tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostResolution.ts"))]
#[serde(rename_all = "snake_case", tag = "target")]
pub enum HostResolution {
    /// The route names the machine that answered: `local`, the empty route or
    /// its own overlay name.
    Local,
    /// The route names one peer of an overlay.
    Remote {
        /// The dialable IP address of the peer, without a port.
        address: String,
        /// The effective daemon port: the route's explicit port or the
        /// overlay's.
        port: u16,
        /// `<overlay>:peer~<id>@<port>` (or `<overlay>:fqdn~<name>@<port>` when
        /// the provider reports no peer id). Resolving it selects the same peer
        /// even after a name or address changes.
        pinned_route: String,
        /// The overlay that reported the peer.
        overlay: String,
        /// The provider's stable peer identity, when it reports one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        peer_id: Option<String>,
        /// The peer's provider-qualified DNS name, when it reports one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        fqdn: Option<String>,
    },
}
