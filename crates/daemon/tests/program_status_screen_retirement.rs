//! The screen rules that OSC 7501 records cover.
//!
//! Claude's records are authoritative for every supported release, so its
//! manifest has no working screen rule: a running turn comes from the held
//! record, a dialog from the screen. Codex keeps screen rules that an open
//! release gate skips. The scenarios replay the screens and record sequences
//! captured from the real agents (`compat/*/screens`).

// Rust guideline compliant 2026-06-26

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use package::version_probe::ProbeVersion;
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
const CLAUDE_RELEASE: &str = "2.1.309";
const CODEX_RELEASE: &str = "0.164.0";

fn read(relative: &str) -> String {
    fs::read_to_string(workspace_root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

struct Harness {
    detector: Detector,
    now: Instant,
    transitions: Vec<ActivityTransition>,
}

impl Harness {
    fn new(runtime: &str, version: &str, readiness: InputReadiness) -> Self {
        let manifest =
            Manifest::parse_str(&read(&format!("runtime-packages/{runtime}/detect.toml")))
                .expect("the shipped manifest parses");
        let config = DetectorConfig {
            launch_name: None,
            runtime_version: Some(ProbeVersion::parse(version).expect("version")),
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
        };
        let now = Instant::now();
        let mut detector = Detector::new(ROWS, COLUMNS, now, config);
        detector.finish_initial_input();
        Self {
            detector,
            now,
            transitions: Vec::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> &mut Self {
        self.now += STEP;
        let fed = self.detector.feed(self.now, bytes);
        self.transitions.extend(fed);
        for _ in 0..SETTLE_TICKS {
            self.now += SETTLE_STEP;
            let ticked = self.detector.tick(self.now);
            self.transitions.extend(ticked);
        }
        self
    }

    /// The activity of the latest transition from the title, screen or a record.
    fn activity(&self) -> Option<AgentActivity> {
        self.transitions
            .iter()
            .rfind(|transition| transition.source != StateSource::Process)
            .map(|transition| transition.activity)
    }
}

fn captured(runtime: &str, directory: &str, name: &str) -> Vec<u8> {
    let base = format!("compat/{runtime}/screens/{directory}{name}");
    let text = read(&format!("{base}.txt"));
    let title = fs::read_to_string(workspace_root().join(format!("{base}.title"))).ok();
    let rows: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    let mut bytes = Vec::new();
    if let Some(title) = title {
        bytes.extend_from_slice(format!("\x1b]2;{}\x07", title.trim_end()).as_bytes());
    }
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");
    bytes.extend_from_slice(rows.join("\r\n").as_bytes());
    bytes
}

/// The captured record sequence of a screen, one raw OSC 7501 report per line.
fn captured_records(runtime: &str, name: &str) -> Vec<Vec<u8>> {
    read(&format!("compat/{runtime}/screens/{name}.program-status"))
        .lines()
        .map(|line| format!("\x1b]7501;{line}\x1b\\").into_bytes())
        .collect()
}

/// What each captured Claude screen means (the same table the capture tests
/// use): the prompt waits, a turn runs, or a dialog waits for an answer.
const CLAUDE_MEANING: [(&str, AgentActivity); 10] = [
    ("idle", AgentActivity::Idle),
    ("idle_after_turn", AgentActivity::Idle),
    ("working_early", AgentActivity::Working),
    ("working_stream", AgentActivity::Working),
    ("approval", AgentActivity::Blocked),
    ("askuser", AgentActivity::Blocked),
    ("trust", AgentActivity::Blocked),
    ("theme", AgentActivity::Blocked),
    ("login", AgentActivity::Blocked),
    ("apikey", AgentActivity::Blocked),
];

#[test]
fn claude_reads_every_capture_with_its_record_and_every_dialog_from_the_screen() {
    for (name, meant) in CLAUDE_MEANING {
        // The streaming-reply capture has no recorded sequence; the next
        // scenario covers it.
        if name == "working_stream" {
            continue;
        }
        let mut harness = Harness::new("claude", CLAUDE_RELEASE, InputReadiness::ScreenIdle);
        harness.feed(&captured("claude", "", name));
        let records = if std::path::Path::new(
            &workspace_root().join(format!("compat/claude/screens/{name}.program-status")),
        )
        .exists()
        {
            captured_records("claude", name)
        } else {
            Vec::new()
        };
        // A dialog capture with a record sequence ends blocked by the dialog
        // whatever the records say; a screen without records is a dialog.
        if meant != AgentActivity::Blocked {
            assert!(!records.is_empty(), "{name} has a captured record sequence");
        }
        for record in &records {
            harness.feed(record);
        }
        assert_eq!(harness.activity(), Some(meant), "{name}");
    }
}

#[test]
fn claude_without_a_held_record_reads_the_prompt_box_as_idle() {
    // Before the first record, and after a clear or a lag resync, the screen
    // decides: a running turn shows the prompt box too, so it reads idle until
    // the next record.
    for name in ["idle", "working_early", "working_stream", "idle_after_turn"] {
        let mut harness = Harness::new("claude", CLAUDE_RELEASE, InputReadiness::ScreenIdle);
        harness.feed(&captured("claude", "", name));
        assert_eq!(harness.activity(), Some(AgentActivity::Idle), "{name}");
    }

    let mut harness = Harness::new("claude", CLAUDE_RELEASE, InputReadiness::ScreenIdle);
    harness.feed(&captured("claude", "", "working_early"));
    harness.feed(b"\x1b]7501;state=working:app=claude-code\x1b\\");
    assert_eq!(harness.activity(), Some(AgentActivity::Working));
    harness.feed(b"\x1b]7501;state=clear\x1b\\");
    harness.feed(&captured("claude", "", "idle_after_turn"));
    assert_eq!(harness.activity(), Some(AgentActivity::Idle));
}

#[test]
fn a_codex_0_164_turn_never_flips_to_idle_between_its_records() {
    let mut harness = Harness::new("codex", CODEX_RELEASE, InputReadiness::ManifestIdle);
    harness.feed(&captured("codex", "", "idle"));
    for record in captured_records("codex", "turn") {
        harness.feed(&record);
    }
    // The last record of the captured turn is `done`.
    assert_eq!(harness.activity(), Some(AgentActivity::Idle));

    let mut turn = Harness::new("codex", CODEX_RELEASE, InputReadiness::ManifestIdle);
    turn.feed(b"\x1b]7501;app=codex:state=working\x1b\\");
    let before = turn.transitions.len();
    turn.feed(&captured("codex", "", "working_early"));
    turn.feed(&captured("codex", "", "working_stream"));
    assert!(
        turn.transitions[before..]
            .iter()
            .all(|transition| transition.activity != AgentActivity::Idle),
        "a running turn never reads idle: {:?}",
        &turn.transitions[before..]
    );
    assert_eq!(turn.activity(), Some(AgentActivity::Working));
}
