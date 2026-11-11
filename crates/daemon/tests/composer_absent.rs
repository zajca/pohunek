//! The detection manifest matcher `composer_absent`.
//!
//! A dialog rule that ANDs its text matchers with `composer_absent = true`
//! fails closed: it reads `blocked` only while the screen shows no prompt box,
//! however the quoted model text above the dialog is drawn. The scenarios feed
//! recorded screens to a detector on a virtual clock.

// Rust guideline compliant 2026-06-26

use std::sync::Arc;
use std::time::{Duration, Instant};

use pohunek_daemon::agent::host::InputReadiness;
use pohunek_daemon::detect::{
    ActivityTransition, DetectionConfig, Detector, DetectorConfig, Manifest, ManifestError,
};
use protocol::AgentActivity;

/// A question dialog that replaces the composer, and the composer itself.
const MANIFEST: &str = r#"
[[rules]]
id = "question"
state = "blocked"
priority = 980
region = "whole_recent"
contains = ["which option do you want"]
composer_absent = true

[[rules]]
id = "composer"
state = "idle"
priority = 300
region = "whole_recent"
contains = ["ask anything"]
"#;

const ROWS: u16 = 24;
const COLUMNS: u16 = 100;
const STEP: Duration = Duration::from_millis(10);
const SETTLE_STEP: Duration = Duration::from_millis(100);
const SETTLE_TICKS: u32 = 12;

/// A horizontal-rule line as the terminal heuristics read it.
const RULE: &str = "────────────────────────────────────────";

/// The picker as drawn after its list scrolled out of a clipped region: the
/// question text quotes a rule line, and the dialog has no rule of its own.
const CLIPPED_PICKER: &[&str] = &[
    "The plan has two steps:",
    RULE,
    "1. rename the module",
    "Which option do you want?",
    "> first option",
];

/// The same question text quoted by the model above the composer box.
const QUOTED_ABOVE_COMPOSER: &[&str] = &[
    "The plan has two steps:",
    RULE,
    "1. rename the module",
    "Which option do you want?",
    RULE,
    "ask anything",
    RULE,
];

fn detector() -> (Detector, Instant) {
    let manifest = Manifest::parse_str(MANIFEST).expect("the manifest parses");
    let now = Instant::now();
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
        manifest: Some(Arc::new(manifest)),
        input_readiness: InputReadiness::ScreenIdle,
    };
    let mut detector = Detector::new(ROWS, COLUMNS, now, config);
    detector.finish_initial_input();
    (detector, now)
}

/// The activity the detector publishes once `lines` settle on screen.
fn settled_activity(lines: &[&str]) -> Option<AgentActivity> {
    let (mut detector, mut now) = detector();
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    bytes.extend_from_slice(lines.join("\r\n").as_bytes());
    let mut published: Option<ActivityTransition> = None;
    now += STEP;
    published = detector.feed(now, &bytes).last().copied().or(published);
    for _ in 0..SETTLE_TICKS {
        now += SETTLE_STEP;
        published = detector.tick(now).last().copied().or(published);
    }
    published.map(|transition| transition.activity)
}

#[test]
fn a_clipped_picker_with_a_rule_in_the_model_text_reads_blocked() {
    assert_eq!(
        settled_activity(CLIPPED_PICKER),
        Some(AgentActivity::Blocked)
    );
}

#[test]
fn the_same_question_text_above_a_composer_box_is_not_a_dialog() {
    assert_eq!(
        settled_activity(QUOTED_ABOVE_COMPOSER),
        Some(AgentActivity::Idle)
    );
}

#[test]
fn a_gate_of_composer_absent_alone_is_refused() {
    let alone = r#"
[[rules]]
id = "any_screen_without_a_box"
state = "blocked"
priority = 10
region = "whole_recent"
composer_absent = true
"#;
    assert!(matches!(
        Manifest::parse_str(alone),
        Err(ManifestError::MissingGate { .. })
    ));
}

#[test]
fn a_nested_gate_of_composer_absent_alone_is_refused() {
    let nested = r#"
[[rules]]
id = "q"
state = "blocked"
priority = 10
region = "whole_recent"
contains = "x"
not = { composer_absent = true }
"#;
    assert!(matches!(
        Manifest::parse_str(nested),
        Err(ManifestError::MissingGate { .. })
    ));
}

#[test]
fn composer_absent_accepts_only_true() {
    let negated = r#"
[[rules]]
id = "q"
state = "blocked"
priority = 10
region = "whole_recent"
contains = "x"
composer_absent = false
"#;
    assert!(matches!(
        Manifest::parse_str(negated),
        Err(ManifestError::ComposerAbsentNotTrue { .. })
    ));
}

#[test]
fn a_rule_is_skipped_while_the_screen_fact_is_unknown() {
    use pohunek_daemon::detect::{ManifestRegion, MatchContext};

    let manifest = Manifest::parse_str(MANIFEST).expect("the manifest parses");
    let text = "Which option do you want?";
    let unknown = MatchContext::default().with_region_text(ManifestRegion::WholeRecent, text);
    assert_eq!(manifest.match_context(&unknown), None);

    let absent = MatchContext::default()
        .with_region_text(ManifestRegion::WholeRecent, text)
        .with_prompt_box(false);
    assert_eq!(
        manifest.match_context(&absent).map(|hit| hit.rule_id),
        Some("question".to_owned())
    );

    let present = MatchContext::default()
        .with_region_text(ManifestRegion::WholeRecent, text)
        .with_prompt_box(true);
    assert_eq!(manifest.match_context(&present), None);
}
