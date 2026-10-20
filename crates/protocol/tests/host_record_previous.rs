//! A host record with `local` read by the previous release's record type.
//!
//! `PreviousHostRecord` and `PreviousHostClass` copy the shapes the previous
//! release (0.35) shipped, so the expectation does not come from the code under
//! test. A `host.discover` consumer of that release decodes the new record in
//! every classification; the record shows as one more host to it.

// Rust guideline compliant 2026-10-20

use protocol::{HostClass, HostRecord};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", tag = "classification")]
enum PreviousHostClass {
    ReachableDaemon { daemon_version: String },
    VersionMismatch { daemon_protocol_version: u32 },
    Unreachable,
    Candidate,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct PreviousHostRecord {
    name: Option<String>,
    fqdn: Option<String>,
    address: Option<String>,
    port: u16,
    overlay: String,
    peer_id: Option<String>,
    #[serde(flatten)]
    class: PreviousHostClass,
}

fn record(local: bool, class: HostClass) -> HostRecord {
    HostRecord {
        name: Some("self-box".to_owned()),
        fqdn: Some("self-box.netbird.test".to_owned()),
        address: Some("100.64.0.7".to_owned()),
        port: 18722,
        overlay: "netbird".to_owned(),
        peer_id: None,
        local,
        class,
    }
}

#[test]
fn the_previous_release_decodes_a_local_record_in_every_classification() {
    let cases = [
        (
            HostClass::ReachableDaemon {
                daemon_version: "0.36.0".to_owned(),
            },
            PreviousHostClass::ReachableDaemon {
                daemon_version: "0.36.0".to_owned(),
            },
        ),
        (
            HostClass::VersionMismatch {
                daemon_protocol_version: 9,
            },
            PreviousHostClass::VersionMismatch {
                daemon_protocol_version: 9,
            },
        ),
        (HostClass::Unreachable, PreviousHostClass::Unreachable),
        (HostClass::Candidate, PreviousHostClass::Candidate),
    ];
    for (class, previous_class) in cases {
        let wire = serde_json::to_value(record(true, class)).expect("serialize the record");
        assert_eq!(wire["local"], json!(true));
        let previous: PreviousHostRecord =
            serde_json::from_value(wire).expect("the previous release decodes the record");
        assert_eq!(previous.name.as_deref(), Some("self-box"));
        assert_eq!(previous.address.as_deref(), Some("100.64.0.7"));
        assert_eq!(previous.port, 18722);
        assert_eq!(previous.overlay, "netbird");
        assert_eq!(previous.peer_id, None);
        assert_eq!(previous.class, previous_class);
    }
}

#[test]
fn a_peer_record_has_no_local_key_on_the_wire() {
    let wire = serde_json::to_value(record(false, HostClass::Candidate)).expect("serialize");
    assert!(wire.get("local").is_none(), "{wire}");
    let current: HostRecord = serde_json::from_value(wire).expect("decode without `local`");
    assert!(!current.local);
}
