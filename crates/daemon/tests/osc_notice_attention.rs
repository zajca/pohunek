//! Desktop notifications as an approval hint.
//!
//! `OSC 9`, `OSC 777 ; notify` and a complete `OSC 99` notice that starts with
//! "Approval requested" are held for the attention fallback. They never set
//! working or idle. The scenarios feed raw bytes to the shipped generic shell
//! manifest on a virtual clock.

// Rust guideline compliant 2026-06-26

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pohunek_daemon::agent::host::InputReadiness;
use pohunek_daemon::detect::{DetectionConfig, Detector, DetectorConfig, Manifest};
use pohunek_test_support::workspace_root;

fn detector() -> Detector {
    let source =
        fs::read_to_string(workspace_root().join("crates/daemon/src/detect/manifests/shell.toml"))
            .expect("read the shell manifest");
    let config = DetectorConfig {
        launch_name: None,
        runtime_version: None,
        gate_waived: false,
        detection: DetectionConfig {
            recheck_after: Duration::from_millis(100),
            confirmations: 1,
            cap: Duration::from_millis(700),
            stable_visible_refresh: Duration::from_millis(800),
            startup_grace: Duration::ZERO,
        },
        manifest: Some(Arc::new(Manifest::parse_str(&source).expect("manifest"))),
        input_readiness: InputReadiness::ScreenIdle,
    };
    let mut detector = Detector::new(24, 100, Instant::now(), config);
    detector.finish_initial_input();
    detector
}

/// Feeds `bytes` and returns whether an approval notice was held and whether
/// any activity transition was published.
fn feed(bytes: &[u8]) -> (bool, bool) {
    let mut detector = detector();
    let now = Instant::now();
    let transitions = detector.feed(now, bytes);
    (detector.take_approval_notice(), !transitions.is_empty())
}

#[test]
fn an_approval_notice_in_any_of_the_three_forms_is_held_without_a_state_change() {
    for bytes in [
        &b"\x1b]9;Approval requested: run ls\x07"[..],
        b"\x1b]9;approval requested\x1b\\",
        b"\x1b]777;notify;Codex;Approval requested: run ls\x07",
        b"\x1b]777;notify;Approval requested;run ls\x07",
        b"\x1b]99;;Approval requested: run ls\x1b\\",
        b"\x1b]99;i=1:p=body;Approval requested\x1b\\",
        b"\x1b]99;i=1:p=title:d=1;Approval requested\x1b\\",
    ] {
        let (held, transitioned) = feed(bytes);
        assert!(held, "{}", String::from_utf8_lossy(bytes));
        assert!(!transitioned, "{}", String::from_utf8_lossy(bytes));
    }
}

#[test]
fn other_notices_and_unsupported_osc_99_forms_hold_nothing() {
    for bytes in [
        &b"\x1b]9;Agent turn complete\x07"[..],
        b"\x1b]9;Working\x07",
        b"\x1b]9;Done\x07",
        b"\x1b]777;notify;Codex;Agent turn complete\x07",
        b"\x1b]777;precmd\x07",
        b"\x1b]99;;Agent turn complete\x1b\\",
        // A chunked notification, a base64 payload, a query, a close and an
        // icon are not complete plain notices.
        b"\x1b]99;i=1:d=0;Approval requested\x1b\\",
        b"\x1b]99;i=1:e=1;QXBwcm92YWwgcmVxdWVzdGVk\x1b\\",
        b"\x1b]99;i=1:p=?;Approval requested\x1b\\",
        b"\x1b]99;i=1:p=close;Approval requested\x1b\\",
        b"\x1b]99;i=1:p=icon;Approval requested\x1b\\",
    ] {
        let (held, transitioned) = feed(bytes);
        assert!(!held, "{}", String::from_utf8_lossy(bytes));
        assert!(!transitioned, "{}", String::from_utf8_lossy(bytes));
    }
}

#[test]
fn a_held_notice_is_taken_once() {
    let mut detector = detector();
    detector.feed(Instant::now(), b"\x1b]9;Approval requested\x07");
    assert!(detector.take_approval_notice());
    assert!(!detector.take_approval_notice());
}
