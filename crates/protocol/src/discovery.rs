//! Host discovery payloads.
//!
//! These types define the JSON shape carried by `host.discover` (see
//! `crate::method::HOST_DISCOVER`). Discovery enumerates the local host's `NetBird`
//! peers and classifies each by probing its daemon control port, so the operator
//! (and the rofi switcher) sees which peers run a compatible daemon.
//!
//! The work — peer enumeration plus concurrent probing — is performed by the
//! local daemon, which caches the result for a short TTL so repeated calls (e.g.
//! every launcher keypress) return instantly. The CLI is a thin client: it asks
//! the local daemon and renders the returned records.
//!
//! Like every other protocol payload these are additive: unknown fields are
//! ignored and absent optional fields default, so a newer peer and an older peer
//! interoperate on the common subset.

use serde::{Deserialize, Serialize};

/// How a `NetBird` peer is classified for `host.discover`.
///
/// Serializes with an internal `classification` tag so a `--json` consumer (and
/// the rofi switcher) can branch on it, e.g.
/// `{"classification":"reachable_daemon","daemon_version":"0.1.0"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostClass.ts"))]
#[serde(rename_all = "snake_case", tag = "classification")]
pub enum HostClass {
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
///
/// Field order and names are part of the wire contract the rofi switcher parses
/// (`name`, `netbird_ip`, and the flattened `classification`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostRecord.ts"))]
pub struct HostRecord {
    /// Short host name (first DNS label of the fqdn), when derivable.
    pub name: Option<String>,
    /// The peer's fully qualified `NetBird` name.
    pub fqdn: Option<String>,
    /// The peer's `NetBird` IP as a string.
    pub netbird_ip: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_record_roundtrips_and_keeps_wire_shape() {
        let record = HostRecord {
            name: Some("host-b".to_owned()),
            fqdn: Some("host-b.netbird.cloud".to_owned()),
            netbird_ip: Some("100.92.30.40".to_owned()),
            class: HostClass::ReachableDaemon {
                daemon_version: "0.1.0".to_owned(),
            },
        };
        let value = serde_json::to_value(&record).expect("serialize");
        assert_eq!(value["classification"], "reachable_daemon");
        assert_eq!(value["daemon_version"], "0.1.0");
        assert_eq!(value["name"], "host-b");
        assert_eq!(value["netbird_ip"], "100.92.30.40");

        let back: HostRecord = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, record);
    }

    #[test]
    fn host_class_tags_are_stable() {
        for (class, tag) in [
            (
                HostClass::VersionMismatch {
                    daemon_protocol_version: 2,
                },
                "version_mismatch",
            ),
            (HostClass::Unreachable, "unreachable"),
            (HostClass::Candidate, "candidate"),
        ] {
            let value = serde_json::to_value(&class).expect("serialize");
            assert_eq!(value["classification"], tag);
            let back: HostClass = serde_json::from_value(value).expect("deserialize");
            assert_eq!(back, class);
        }
    }

    #[test]
    fn discover_params_default_force_false_and_absent_ok() {
        let from_empty: HostDiscoverParams = serde_json::from_str("{}").expect("empty");
        assert!(!from_empty.force);
        let forced: HostDiscoverParams = serde_json::from_str(r#"{"force":true}"#).expect("force");
        assert!(forced.force);
    }
}
