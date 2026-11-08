//! OSC 9 classification in the activity detector.
//!
//! Only `OSC 9 ; 4 ; <state>` is progress. A plain OSC 9 notification and a
//! malformed `9;4` sequence are no evidence. The scenarios feed raw bytes the
//! way the session's output stream does, under the shipped generic shell
//! manifest and the shipped Claude manifest, on a virtual clock.

// Rust guideline compliant 2026-06-26

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pohunek_daemon::agent::host::InputReadiness;
use pohunek_daemon::detect::{
    ActivityTransition, DetectionConfig, Detector, DetectorConfig, Manifest,
};
use pohunek_test_support::workspace_root;
use protocol::{AgentActivity, StateSource};

const ROWS: u16 = 24;
const COLUMNS: u16 = 100;
const STEP: Duration = Duration::from_millis(10);
const SETTLE_STEP: Duration = Duration::from_millis(100);
const SETTLE_TICKS: u32 = 12;

fn detection() -> DetectionConfig {
    DetectionConfig {
        recheck_after: Duration::from_millis(100),
        confirmations: 1,
        cap: Duration::from_millis(700),
        stable_visible_refresh: Duration::from_millis(800),
        startup_grace: Duration::ZERO,
    }
}

struct Harness {
    detector: Detector,
    now: Instant,
    published: Option<ActivityTransition>,
}

impl Harness {
    fn with_manifest(relative: &str) -> Self {
        let source = fs::read_to_string(workspace_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let config = DetectorConfig {
            launch_name: None,
            detection: detection(),
            manifest: Some(Arc::new(Manifest::parse_str(&source).expect("manifest"))),
            input_readiness: InputReadiness::ScreenIdle,
        };
        let now = Instant::now();
        let mut detector = Detector::new(ROWS, COLUMNS, now, config);
        detector.finish_initial_input();
        Self {
            detector,
            now,
            published: None,
        }
    }

    fn shell() -> Self {
        Self::with_manifest("crates/daemon/src/detect/manifests/shell.toml")
    }

    fn claude() -> Self {
        Self::with_manifest("runtime-packages/claude/detect.toml")
    }

    fn note(&mut self, transitions: &[ActivityTransition]) {
        if let Some(last) = transitions.last() {
            self.published = Some(*last);
        }
    }

    /// Feeds one chunk and lets the debounce settle.
    fn feed(&mut self, bytes: &[u8]) -> &mut Self {
        self.now += STEP;
        let transitions = self.detector.feed(self.now, bytes);
        self.note(&transitions);
        for _ in 0..SETTLE_TICKS {
            self.now += SETTLE_STEP;
            let transitions = self.detector.tick(self.now);
            self.note(&transitions);
        }
        self
    }

    fn activity(&self) -> Option<(AgentActivity, StateSource)> {
        self.published
            .map(|transition| (transition.activity, transition.source))
    }
}

const WORKING: Option<(AgentActivity, StateSource)> =
    Some((AgentActivity::Working, StateSource::OscProgress));
const IDLE: Option<(AgentActivity, StateSource)> =
    Some((AgentActivity::Idle, StateSource::OscProgress));

#[test]
fn a_plain_osc_9_notice_is_not_progress_whatever_its_words() {
    for text in [
        "Approval requested",
        "action required",
        "working",
        "blocked",
        "60%",
        "ready",
    ] {
        for terminator in ["\x07", "\x1b\\"] {
            let mut harness = Harness::shell();
            harness.feed(format!("\x1b]9;{text}{terminator}").as_bytes());
            assert_eq!(harness.activity(), None, "{text:?}");
            assert_eq!(harness.detector.latest_progress(), None, "{text:?}");
        }
    }
}

#[test]
fn a_notice_does_not_replace_or_disturb_a_held_progress_state() {
    let mut harness = Harness::shell();
    harness.feed(b"\x1b]9;4;3\x07");
    assert_eq!(harness.activity(), WORKING);
    harness.feed(b"\x1b]9;Approval requested\x07");
    assert_eq!(harness.activity(), WORKING);
    assert_eq!(harness.detector.latest_progress(), Some("4;3"));
}

#[test]
fn indeterminate_progress_reads_working_and_removal_reads_idle() {
    let mut harness = Harness::shell();
    // Fragmented across feeds, with a percentage, ST-terminated.
    harness.feed(b"\x1b]9;4;3;4");
    harness.feed(b"0\x1b\\");
    assert_eq!(harness.activity(), WORKING);
    assert_eq!(harness.detector.latest_progress(), Some("4;3"));

    harness.feed(b"\x1b]9;4;0\x07");
    assert_eq!(harness.activity(), IDLE);

    harness.feed(b"\x1b]9;4;3\x07");
    assert_eq!(harness.activity(), WORKING);
    harness.feed(b"\x1b]9;4;0;0\x07");
    assert_eq!(harness.activity(), IDLE);
}

#[test]
fn claude_removal_progress_still_reads_idle() {
    let mut harness = Harness::claude();
    harness.feed(b"\x1b]9;4;0\x07");
    assert_eq!(harness.activity(), IDLE);
}

#[test]
fn claude_ignores_an_approval_notice() {
    let mut harness = Harness::claude();
    harness.feed(b"\x1b]9;Approval requested\x07");
    assert_eq!(harness.activity(), None);
    assert_eq!(harness.detector.latest_progress(), None);
}

#[test]
fn progress_states_without_a_rule_and_malformed_sequences_change_nothing() {
    for sequence in [
        "9;4;1;50",
        "9;4;2",
        "9;4;4;10",
        "9;4",
        "9;4;",
        "9;4;9",
        "9;4;x",
        "9;4;3;abc",
        "9;4;3;101",
        "9;4;3;5;6",
        "9;4;;",
    ] {
        let mut harness = Harness::shell();
        harness.feed(b"\x1b]9;4;3\x07");
        assert_eq!(harness.activity(), WORKING);
        harness.feed(format!("\x1b]{sequence}\x07").as_bytes());
        assert_eq!(harness.activity(), WORKING, "{sequence}");
    }
}
