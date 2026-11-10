//! The release gate of OSC 7501 program-status records.
//!
//! A manifest's `[program_status] authority_from` makes its records
//! authoritative only for a runtime release at or above it; a manifest without
//! the key is authoritative for every release. The scenarios feed screens and
//! raw records to a detector configured with explicit runtime releases, on a
//! virtual clock: a synthetic manifest covers the gate itself, the shipped
//! Claude manifest the undeclared case and the shipped Codex manifest a gate
//! that is declared.

// Rust guideline compliant 2026-06-26

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use package::version_probe::ProbeVersion;
use pohunek_daemon::agent::host::InputReadiness;
use pohunek_daemon::detect::{
    ActivityTransition, DetectionConfig, Detector, DetectorConfig, GateReason, Manifest,
};
use pohunek_test_support::workspace_root;
use protocol::{AgentActivity, DetectionRegionKind, StateSource};

/// A manifest that declares a gate, with a screen rule the open gate retires,
/// a visible dialog and a prompt box.
const SYNTHETIC_MANIFEST: &str = r#"
[process]
comm = ['^synth$']

[program_status]
app = "synth-agent"
answer_probe = true
authority_from = "2.0.0"

[[rules]]
id = "working_banner"
state = "working"
priority = 960
below_authority = true
region = "whole_recent"
contains = ["synth is working"]

[[rules]]
id = "approval_dialog"
state = "blocked"
priority = 980
region = "whole_recent"
visible_blocker = true
contains = ["synth needs approval"]

[[rules]]
id = "prompt_box"
state = "idle"
priority = 950
region = "whole_recent"
contains = ["synth ready"]
"#;

/// The `app` of [`SYNTHETIC_MANIFEST`].
const SYNTHETIC_APP: &str = "synth-agent";

const ROWS: u16 = 24;
const COLUMNS: u16 = 100;
const STEP: Duration = Duration::from_millis(10);
const SETTLE_STEP: Duration = Duration::from_millis(100);
const SETTLE_TICKS: u32 = 12;

type Observed = Option<(AgentActivity, StateSource)>;

fn read(relative: &str) -> String {
    fs::read_to_string(workspace_root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn config(runtime: &str, version: Option<&str>, readiness: InputReadiness) -> DetectorConfig {
    config_of(
        &read(&format!("runtime-packages/{runtime}/detect.toml")),
        version,
        readiness,
    )
}

fn synthetic(version: Option<&str>) -> DetectorConfig {
    config_of(SYNTHETIC_MANIFEST, version, InputReadiness::ScreenIdle)
}

fn config_of(source: &str, version: Option<&str>, readiness: InputReadiness) -> DetectorConfig {
    let manifest = Manifest::parse_str(source).expect("the manifest parses");
    DetectorConfig {
        launch_name: None,
        runtime_version: version.map(|version| ProbeVersion::parse(version).expect("version")),
        gate_waived: false,
        detection: DetectionConfig {
            recheck_after: Duration::from_millis(100),
            confirmations: 1,
            cap: Duration::from_millis(700),
            stable_visible_refresh: Duration::from_millis(800),
            startup_grace: Duration::ZERO,
        },
        manifest: Some(Arc::new(manifest)),
        input_readiness: readiness,
    }
}

struct Harness {
    detector: Detector,
    now: Instant,
    published: Option<ActivityTransition>,
}

impl Harness {
    fn new(config: DetectorConfig) -> Self {
        let now = Instant::now();
        let mut detector = Detector::new(ROWS, COLUMNS, now, config);
        detector.finish_initial_input();
        Self {
            detector,
            now,
            published: None,
        }
    }

    fn claude(version: Option<&str>) -> Self {
        Self::new(config("claude", version, InputReadiness::ScreenIdle))
    }

    fn synthetic(version: Option<&str>) -> Self {
        Self::new(synthetic(version))
    }

    fn codex(version: Option<&str>) -> Self {
        Self::new(config("codex", version, InputReadiness::ManifestIdle))
    }

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

    fn note(&mut self, transitions: &[ActivityTransition]) {
        if let Some(last) = transitions.last() {
            self.published = Some(*last);
        }
    }

    fn activity(&self) -> Observed {
        self.published
            .map(|transition| (transition.activity, transition.source))
    }

    fn record_held(&self) -> bool {
        self.detector.region_previews().iter().any(|preview| {
            preview.kind == DetectionRegionKind::ProgramStatus && !preview.text.is_empty()
        })
    }
}

fn record(app: &str, state: &str) -> Vec<u8> {
    format!("\x1b]7501;state={state}:app={app}\x1b\\").into_bytes()
}

/// The bytes that redraw the screen with `lines`.
fn screen(lines: &[&str]) -> Vec<u8> {
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    bytes.extend_from_slice(lines.join("\r\n").as_bytes());
    bytes
}

/// A shipped screen capture of `runtime`, with the title it was captured with.
fn captured(runtime: &str, name: &str) -> Vec<u8> {
    let base = format!("compat/{runtime}/screens/{name}");
    let text = read(&format!("{base}.txt"));
    let title = fs::read_to_string(workspace_root().join(format!("{base}.title"))).ok();
    let rows: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    let mut bytes = Vec::new();
    if let Some(title) = title {
        bytes.extend_from_slice(format!("\x1b]2;{}\x07", title.trim_end()).as_bytes());
    }
    bytes.extend(screen(&rows));
    bytes
}

const WORKING_RECORD: Observed = Some((AgentActivity::Working, StateSource::OscProgress));

#[test]
fn below_the_gate_a_release_is_screen_only_and_ignores_records() {
    let config = synthetic(Some("1.9.9"));
    assert!(!config.answers_program_status_probe());
    let gate = config.authority_gate();
    assert!(!gate.open);
    assert_eq!(gate.reason, GateReason::Below);

    let mut harness = Harness::new(config);
    harness.feed(&screen(&["synth is working"]));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Working, StateSource::Screen))
    );

    harness.feed(&record(SYNTHETIC_APP, "idle"));
    harness.feed(&record(SYNTHETIC_APP, "working"));
    assert!(
        !harness.record_held(),
        "a record below the gate is not held"
    );
    assert_ne!(
        harness.activity().map(|(_, source)| source),
        Some(StateSource::OscProgress)
    );
}

#[test]
fn at_the_gate_a_release_answers_the_probe_and_reads_the_record() {
    let config = synthetic(Some("2.0.0"));
    assert!(config.answers_program_status_probe());
    let gate = config.authority_gate();
    assert!(gate.open);
    assert_eq!(gate.reason, GateReason::AtOrAbove);

    let mut harness = Harness::synthetic(Some("2.0.0"));
    harness.feed(&screen(&["synth ready"]));
    harness.feed(&record(SYNTHETIC_APP, "working"));
    assert!(harness.record_held());
    assert_eq!(harness.activity(), WORKING_RECORD);
}

#[test]
fn a_held_record_outranks_a_working_screen_rule() {
    let mut harness = Harness::synthetic(Some("2.0.0"));
    harness.feed(&screen(&["synth is working", "synth ready"]));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Working, StateSource::Screen)),
        "before the first record the screen decides"
    );
    harness.feed(&record(SYNTHETIC_APP, "idle"));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Idle, StateSource::OscProgress)),
        "a held idle record outranks the stale working banner"
    );
}

#[test]
fn the_gate_boundary_is_authority_from() {
    for (version, open) in [("1.9.9", false), ("2.0.0", true), ("2.1.0", true)] {
        let gate = synthetic(Some(version)).authority_gate();
        assert_eq!(gate.open, open, "{version}");
    }
}

#[test]
fn an_unknown_release_closes_the_gate_and_never_defaults_to_idle() {
    let config = synthetic(None);
    assert!(!config.answers_program_status_probe());
    assert_eq!(config.authority_gate().reason, GateReason::VersionUnknown);

    let mut harness = Harness::new(config);
    assert_eq!(harness.activity(), None, "no evidence, no activity");
    harness.feed(&record(SYNTHETIC_APP, "working"));
    assert!(!harness.record_held());
    assert_eq!(harness.activity(), None);
}

#[test]
fn the_claude_manifest_declares_no_gate_so_its_records_are_authoritative_for_every_release() {
    for version in [Some("2.1.309"), Some("2.2.0"), None] {
        let config = config("claude", version, InputReadiness::ScreenIdle);
        let gate = config.authority_gate();
        assert!(!gate.declared, "{version:?}");
        assert_eq!(gate.reason, GateReason::NotDeclared, "{version:?}");
        assert!(config.answers_program_status_probe(), "{version:?}");

        let mut harness = Harness::new(config);
        harness.feed(&record("claude-code", "working"));
        assert!(harness.record_held(), "{version:?}");
        assert_eq!(harness.activity(), WORKING_RECORD, "{version:?}");
    }
}

#[test]
fn a_visible_dialog_beats_a_blocked_record_and_is_attributed_to_the_screen() {
    let mut harness = Harness::synthetic(Some("2.0.0"));
    harness.feed(&record(SYNTHETIC_APP, "working"));
    harness.feed(&screen(&["synth needs approval"]));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Blocked, StateSource::Screen))
    );
    harness.feed(&record(SYNTHETIC_APP, "blocked"));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Blocked, StateSource::Screen))
    );
    harness.feed(&record(SYNTHETIC_APP, "idle"));
    assert_eq!(
        harness.activity().map(|(activity, _)| activity),
        Some(AgentActivity::Blocked)
    );
}

#[test]
fn a_blocked_record_without_a_dialog_reports_the_record() {
    let mut harness = Harness::claude(None);
    harness.feed(&captured("claude", "idle"));
    harness.feed(&record("claude-code", "blocked"));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Blocked, StateSource::OscProgress))
    );
}

#[test]
fn a_claude_dialog_capture_beats_a_held_record() {
    let mut harness = Harness::claude(None);
    harness.feed(&record("claude-code", "working"));
    harness.feed(&captured("claude", "trust"));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Blocked, StateSource::Screen))
    );
}

#[test]
fn a_blocked_title_and_a_blocked_record_report_the_record() {
    let mut harness = Harness::codex(Some("0.164.0"));
    harness.feed(&captured("codex", "idle"));
    harness.feed(b"\x1b]0;Action Required\x07");
    harness.feed(&record("codex", "blocked"));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Blocked, StateSource::OscProgress))
    );
}

#[test]
fn a_codex_0_164_record_beats_a_stale_idle_title() {
    let mut harness = Harness::codex(Some("0.164.0"));
    harness.feed(&captured("codex", "idle"));
    harness.feed(&record("codex", "working"));
    assert_eq!(harness.activity(), WORKING_RECORD);
}

#[test]
fn a_reconfigure_with_the_same_release_keeps_the_gate_open() {
    let mut harness = Harness::synthetic(Some("2.0.0"));
    harness.feed(&record(SYNTHETIC_APP, "working"));
    assert_eq!(harness.activity(), WORKING_RECORD);
    harness
        .detector
        .reconfigure(harness.now, synthetic(Some("2.0.0")));
    assert!(harness.detector.authority_gate().open);
    harness.feed(&record(SYNTHETIC_APP, "working"));
    assert!(harness.record_held());
}
