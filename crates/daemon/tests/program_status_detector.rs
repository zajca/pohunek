//! OSC 7501 program-status evidence in the activity detector.
//!
//! The Pi, Codex and Claude manifests ship their `[program_status]` table; a
//! scenario for a runtime without one appends the table to the shipped manifest.
//! The scenarios feed the detector the screens captured from the real agents (`compat/*/screens`)
//! together with raw 7501 records, the way the session's output stream does.
//! Time is a virtual `Instant`: nothing here sleeps.

// Rust guideline compliant 2026-06-26

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pohunek_daemon::agent::host::InputReadiness;
use pohunek_daemon::detect::{
    ActivityTransition, DetectionConfig, Detector, DetectorConfig, Manifest, ManifestError,
    ProgramStatusConfig,
};
use pohunek_test_support::workspace_root;
use protocol::{AgentActivity, DetectionRegionKind, StateSource};

/// Rows of the terminal the screens were captured on.
const ROWS: u16 = 24;
/// Columns of the terminal the screens were captured on.
const COLUMNS: u16 = 100;
/// Virtual time between two evidence steps.
const STEP: Duration = Duration::from_millis(10);
/// Virtual time the debounce needs to confirm an idle or blocked candidate.
const SETTLE_STEP: Duration = Duration::from_millis(100);
/// Settling ticks after which every debounced candidate has published.
const SETTLE_TICKS: u32 = 12;
/// The `app` the Claude test manifest opts in with.
const CLAUDE_APP: &str = "claude-code";
/// The `app` the Codex test manifest opts in with.
const CODEX_APP: &str = "codex";

fn read(relative: &str) -> String {
    fs::read_to_string(workspace_root().join(relative)).unwrap_or_else(|error| {
        panic!("read {relative}: {error}");
    })
}

/// The shipped manifest of `runtime` as an opted-in manifest: as shipped when it
/// already declares `[program_status]`, else with the table appended.
fn opted_in_source(runtime: &str, app: &str, answer_probe: bool) -> String {
    let shipped = read(&format!("runtime-packages/{runtime}/detect.toml"));
    if shipped.contains("\n[program_status]") {
        return shipped;
    }
    format!("{shipped}\n[program_status]\napp = \"{app}\"\nanswer_probe = {answer_probe}\n")
}

/// The shipped manifest of `runtime` without the opt-in: no `[program_status]`
/// table and no rule that overrides a record.
fn plain_source(runtime: &str) -> String {
    let mut table: toml::Table = read(&format!("runtime-packages/{runtime}/detect.toml"))
        .parse()
        .expect("the shipped manifest is TOML");
    table.remove("program_status");
    if let Some(rules) = table.get_mut("rules").and_then(toml::Value::as_array_mut) {
        rules.retain(|rule| rule.get("overrides_program_status").is_none());
        for rule in rules.iter_mut().filter_map(toml::Value::as_table_mut) {
            rule.remove("below_authority");
        }
    }
    table.to_string()
}

fn fast_detection() -> DetectionConfig {
    DetectionConfig {
        recheck_after: Duration::from_millis(100),
        confirmations: 1,
        cap: Duration::from_millis(700),
        stable_visible_refresh: Duration::from_millis(800),
        startup_grace: Duration::ZERO,
    }
}

/// A config whose runtime release sits exactly at the manifest's
/// `authority_from`, so the records are authoritative; a manifest without the
/// key is authoritative for every release.
fn config(manifest: &str, readiness: InputReadiness) -> DetectorConfig {
    let manifest = Manifest::parse_str(manifest).expect("the manifest parses");
    let runtime_version = manifest
        .program_status()
        .and_then(ProgramStatusConfig::authority_from);
    DetectorConfig {
        launch_name: None,
        runtime_version,
        gate_waived: false,
        detection: fast_detection(),
        manifest: Some(Arc::new(manifest)),
        input_readiness: readiness,
    }
}

/// The one-shot variants make the order deterministic: with the refresh
/// disabled the record's own publish is the only candidate, with a faster
/// refresh the refresh wins before the record's own debounce completes.
fn no_refresh_config(manifest: &str, readiness: InputReadiness) -> DetectorConfig {
    let detection = DetectionConfig {
        stable_visible_refresh: Duration::from_secs(3600),
        ..DetectionConfig::default()
    };
    detection_config(manifest, readiness, detection)
}

fn fast_refresh_config(manifest: &str, readiness: InputReadiness) -> DetectorConfig {
    let detection = DetectionConfig {
        stable_visible_refresh: Duration::from_millis(200),
        ..DetectionConfig::default()
    };
    detection_config(manifest, readiness, detection)
}

fn detection_config(
    manifest: &str,
    readiness: InputReadiness,
    detection: DetectionConfig,
) -> DetectorConfig {
    let manifest = Manifest::parse_str(manifest).expect("the manifest parses");
    let runtime_version = manifest
        .program_status()
        .and_then(ProgramStatusConfig::authority_from);
    DetectorConfig {
        launch_name: None,
        runtime_version,
        gate_waived: false,
        detection,
        manifest: Some(Arc::new(manifest)),
        input_readiness: readiness,
    }
}

/// A detector over a virtual clock, remembering the activity it published.
struct Harness {
    detector: Detector,
    now: Instant,
    published: Option<ActivityTransition>,
    transitions: Vec<ActivityTransition>,
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
            transitions: Vec::new(),
        }
    }

    fn claude_no_refresh() -> Self {
        Self::new(no_refresh_config(
            &opted_in_source("claude", CLAUDE_APP, true),
            InputReadiness::ScreenIdle,
        ))
    }

    fn claude_fast_refresh() -> Self {
        Self::new(fast_refresh_config(
            &opted_in_source("claude", CLAUDE_APP, true),
            InputReadiness::ScreenIdle,
        ))
    }

    fn claude() -> Self {
        Self::new(config(
            &opted_in_source("claude", CLAUDE_APP, true),
            InputReadiness::ScreenIdle,
        ))
    }

    fn claude_without_opt_in() -> Self {
        Self::new(config(&plain_source("claude"), InputReadiness::ScreenIdle))
    }

    fn codex() -> Self {
        Self::new(config(
            &opted_in_source("codex", CODEX_APP, false),
            InputReadiness::ManifestIdle,
        ))
    }

    fn note(&mut self, transitions: Vec<ActivityTransition>) {
        if let Some(last) = transitions.last() {
            self.published = Some(*last);
        }
        self.transitions.extend(transitions);
    }

    /// Feeds one chunk and lets the debounce settle.
    fn feed(&mut self, bytes: &[u8]) -> &mut Self {
        self.now += STEP;
        let transitions = self.detector.feed(self.now, bytes);
        self.note(transitions);
        self.settle()
    }

    /// Feeds one chunk without ticking, returning only its own transitions.
    fn feed_raw(&mut self, bytes: &[u8]) -> Vec<ActivityTransition> {
        self.now += STEP;
        let transitions = self.detector.feed(self.now, bytes);
        self.note(transitions.clone());
        transitions
    }

    fn settle(&mut self) -> &mut Self {
        for _ in 0..SETTLE_TICKS {
            self.now += SETTLE_STEP;
            let transitions = self.detector.tick(self.now);
            self.note(transitions);
        }
        self
    }

    fn advance(&mut self, by: Duration) -> &mut Self {
        self.now += by;
        self.settle()
    }

    /// Ticks of `by` without settling until one publishes (or `rounds` run
    /// out): for scenarios where a publish follows the fallback end.
    fn tick_until_published(&mut self, by: Duration, rounds: usize) -> Vec<ActivityTransition> {
        for _ in 0..rounds {
            self.now += by;
            let transitions = self.detector.tick(self.now);
            self.note(transitions.clone());
            if !transitions.is_empty() {
                return transitions;
            }
        }
        Vec::new()
    }

    /// The activity and source of the latest published transition.
    fn activity(&self) -> Option<(AgentActivity, StateSource)> {
        self.published
            .map(|transition| (transition.activity, transition.source))
    }

    /// The published transitions with the periodic re-announcements of an
    /// unchanged state removed.
    fn changes(&self) -> Vec<(AgentActivity, StateSource)> {
        let mut changes: Vec<(AgentActivity, StateSource)> = Vec::new();
        for transition in &self.transitions {
            let change = (transition.activity, transition.source);
            if changes.last() != Some(&change) {
                changes.push(change);
            }
        }
        changes
    }

    /// The canonical record the detector reports in `session.detection`.
    fn record_preview(&self) -> Option<String> {
        self.detector
            .region_previews()
            .into_iter()
            .find(|preview| preview.kind == DetectionRegionKind::ProgramStatus)
            .map(|preview| preview.text)
    }

    fn reconfigure(&mut self, config: DetectorConfig) {
        self.now += STEP;
        self.detector.reconfigure(self.now, config);
    }
}

/// A raw record the way a program writes it, ST-terminated.
fn record(pairs: &str) -> Vec<u8> {
    format!("\x1b]7501;{pairs}\x1b\\").into_bytes()
}

/// A raw root record of Claude.
fn claude_record(state: &str) -> Vec<u8> {
    record(&format!("state={state}:app={CLAUDE_APP}"))
}

/// A screen the way the terminal receives it: an optional title, then the
/// cleared screen and its rows.
fn frame(title: Option<&str>, rows: &[&str]) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(title) = title {
        bytes.extend_from_slice(format!("\x1b]2;{title}\x07").as_bytes());
    }
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");
    bytes.extend_from_slice(rows.join("\r\n").as_bytes());
    bytes
}

/// A captured screen of `runtime`, with its title when one was captured.
fn captured(runtime: &str, name: &str) -> Vec<u8> {
    let directory = format!("compat/{runtime}/screens");
    let text = read(&format!("{directory}/{name}.txt"));
    let title = fs::read_to_string(workspace_root().join(format!("{directory}/{name}.title")))
        .ok()
        .map(|title| title.trim_end().to_owned());
    let rows: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    frame(title.as_deref(), &rows)
}

/// An activity and the source that reported it.
type Observed = Option<(AgentActivity, StateSource)>;

const WORKING: Observed = Some((AgentActivity::Working, StateSource::OscProgress));
const IDLE: Observed = Some((AgentActivity::Idle, StateSource::OscProgress));
const BLOCKED: Observed = Some((AgentActivity::Blocked, StateSource::OscProgress));

/// The record states and the canonical form each reports.
const RECORD_STATES: [(&str, Observed, &str); 7] = [
    ("working", WORKING, "app=claude-code:state=working"),
    ("blocked", BLOCKED, "app=claude-code:state=blocked"),
    (
        "blocked:kind=permission",
        BLOCKED,
        "app=claude-code:state=blocked:kind=permission",
    ),
    (
        "blocked:kind=question",
        BLOCKED,
        "app=claude-code:state=blocked:kind=question",
    ),
    (
        "blocked:kind=auth",
        BLOCKED,
        "app=claude-code:state=blocked:kind=auth",
    ),
    ("done", IDLE, "app=claude-code:state=done"),
    ("error", IDLE, "app=claude-code:state=error"),
];

/// A record state is held whatever the terminator and wherever the report is
/// split into two chunks.
fn a_record_state_is_held_whatever_the_terminator_and_the_chunking(case: usize) {
    let (state, expected, canonical) = RECORD_STATES[case];
    for terminator in ["\x1b\\", "\x07"] {
        let bytes = format!("\x1b]7501;state={state}:app={CLAUDE_APP}{terminator}").into_bytes();
        for split in 0..=bytes.len() {
            let mut harness = Harness::claude();
            harness.feed(&captured("claude", "idle"));
            harness.feed(&bytes[..split]);
            harness.feed(&bytes[split..]);

            assert_eq!(
                harness.activity(),
                expected,
                "{state} split at {split} with {terminator:?}"
            );
            assert_eq!(harness.record_preview().as_deref(), Some(canonical));
        }
    }
}

/// One test per `RECORD_STATES` row; `RECORD_STATE_CASES` lists the rows tested.
macro_rules! record_state_tests {
    ($($name:ident: $case:expr),+ $(,)?) => {
        const RECORD_STATE_CASES: &[usize] = &[$($case),+];
        $(
            #[test]
            fn $name() {
                a_record_state_is_held_whatever_the_terminator_and_the_chunking($case);
            }
        )+
    };
}

record_state_tests!(
    a_working_record_is_held_whatever_the_terminator_and_the_chunking: 0,
    a_blocked_record_is_held_whatever_the_terminator_and_the_chunking: 1,
    a_blocked_permission_record_is_held_whatever_the_terminator_and_the_chunking: 2,
    a_blocked_question_record_is_held_whatever_the_terminator_and_the_chunking: 3,
    a_blocked_auth_record_is_held_whatever_the_terminator_and_the_chunking: 4,
    a_done_record_is_held_whatever_the_terminator_and_the_chunking: 5,
    an_error_record_is_held_whatever_the_terminator_and_the_chunking: 6,
);

#[test]
fn every_record_state_has_a_test() {
    let every_row: Vec<usize> = (0..RECORD_STATES.len()).collect();
    assert_eq!(RECORD_STATE_CASES.to_vec(), every_row);
}

#[test]
fn an_idle_record_is_held_like_the_other_states() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "working_stream"));
    harness.feed(&claude_record("idle"));

    assert_eq!(harness.activity(), IDLE);
    assert_eq!(
        harness.record_preview().as_deref(),
        Some("app=claude-code:state=idle")
    );
}

#[test]
fn a_clear_drops_the_record_and_the_screen_decides_again() {
    for clear in ["state=clear", "state=clear:app=claude-code"] {
        let mut harness = Harness::claude();
        harness.feed(&captured("claude", "idle"));
        harness.feed(&claude_record("working"));
        assert_eq!(harness.activity(), WORKING);

        harness.feed(&record(clear));

        assert_eq!(harness.record_preview().as_deref(), Some(""), "{clear}");
        assert_eq!(
            harness.activity(),
            Some((AgentActivity::Idle, StateSource::Screen)),
            "{clear}: the idle screen takes over"
        );
    }
}

#[test]
fn a_probe_is_never_a_record() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));
    let before = harness.activity();

    harness.feed(b"\x1b]7501;?\x1b\\");

    assert_eq!(harness.activity(), before);
    assert_eq!(harness.record_preview().as_deref(), Some(""));
}

#[test]
fn a_malformed_report_is_discarded_whole_and_the_next_one_parses() {
    let long_key = format!("state=working:app={CLAUDE_APP}:{}=x", "k".repeat(17));
    let oversized = format!("state=working:app={CLAUDE_APP}:x={}", "y".repeat(4097));
    let bad = [
        "state=sleeping:app=claude-code".to_owned(),
        "app=claude-code".to_owned(),
        "state=:app=claude-code".to_owned(),
        long_key,
        oversized,
    ];
    for payload in bad {
        let mut harness = Harness::claude();
        harness.feed(&captured("claude", "idle"));
        let before = harness.activity();

        harness.feed(&record(&payload));
        assert_eq!(harness.activity(), before, "{payload:.40}");
        assert_eq!(harness.record_preview().as_deref(), Some(""));

        harness.feed(&claude_record("working"));
        assert_eq!(harness.activity(), WORKING, "a later report still parses");
    }
}

#[test]
fn a_report_with_an_unreadable_text_is_discarded_and_no_text_is_kept() {
    // Base64 of "SECRET-MSG".
    let valid_msg = "U0VDUkVULU1TRw==";
    // Base64 of text holding a control character, an invalid string, and an
    // over-limit text (2049 decoded bytes).
    let control = "aGkAYQ==";
    let invalid = "!!not-base64!!";
    let too_long = "QUFB".repeat(683);
    for (label, pairs, held) in [
        ("valid msg", format!("msg={valid_msg}"), true),
        ("valid title", format!("title={valid_msg}"), true),
        ("control character", format!("msg={control}"), false),
        ("invalid base64", format!("msg={invalid}"), false),
        ("title control", format!("title={control}"), false),
        ("msg over the limit", format!("msg={too_long}"), false),
    ] {
        let mut harness = Harness::claude();
        harness.feed(&captured("claude", "idle"));

        harness.feed(&record(&format!("state=working:app={CLAUDE_APP}:{pairs}")));

        assert_eq!(
            harness.activity() == WORKING,
            held,
            "{label}: {:?}",
            harness.activity()
        );
        let exposed = format!(
            "{:?} {:?} {:?}",
            harness.detector,
            harness.detector.region_previews(),
            harness.transitions
        );
        assert!(
            !exposed.contains("SECRET") && !exposed.contains(valid_msg),
            "{label}: no text is stored or exposed"
        );
    }
}

#[test]
fn a_malformed_pair_an_unknown_key_and_a_stray_kind_are_ignored() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));

    harness.feed(&record(&format!(
        "junk:state=working:kind=permission:bogus=1:app={CLAUDE_APP}:progress=40"
    )));

    assert_eq!(harness.activity(), WORKING);
    assert_eq!(
        harness.record_preview().as_deref(),
        Some("app=claude-code:state=working"),
        "kind counts only with blocked"
    );
}

#[test]
fn a_child_record_never_changes_the_root_record() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));
    harness.feed(&claude_record("working"));
    assert_eq!(harness.activity(), WORKING);

    for payload in [
        format!("state=idle:app={CLAUDE_APP}:id=bg-1"),
        format!("state=clear:app={CLAUDE_APP}:id=bg-1"),
        "state=clear:id=bg-1".to_owned(),
    ] {
        harness.feed(&record(&payload));
        harness.advance(Duration::from_hours(1));

        assert_eq!(harness.activity(), WORKING, "{payload}");
        assert_eq!(
            harness.record_preview().as_deref(),
            Some("app=claude-code:state=working")
        );
    }
}

#[test]
fn a_record_of_another_app_or_without_an_app_is_ignored() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));
    let before = harness.activity();

    for payload in [
        "state=working:app=codex",
        "state=working",
        "state=working:app=%%",
    ] {
        harness.feed(&record(payload));

        assert_eq!(harness.activity(), before, "{payload}");
        assert_eq!(harness.record_preview().as_deref(), Some(""));
    }
}

#[test]
fn a_record_stays_held_for_virtual_hours() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));
    harness.feed(&claude_record("working"));

    harness.advance(Duration::from_hours(48));

    assert_eq!(harness.activity(), WORKING);
}

#[test]
fn a_reconfigure_and_a_lag_resync_drop_the_record() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));
    harness.feed(&claude_record("working"));
    harness.reconfigure(config(
        &opted_in_source("claude", CLAUDE_APP, true),
        InputReadiness::ScreenIdle,
    ));
    harness.settle();

    assert_eq!(harness.record_preview().as_deref(), Some(""));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Idle, StateSource::Screen)),
        "the screen takes over after a reconfigure"
    );

    harness.feed(&claude_record("working"));
    assert_eq!(harness.activity(), WORKING);
    harness.detector.resync_after_lag();
    harness.feed(&captured("claude", "idle"));

    assert_eq!(harness.record_preview().as_deref(), Some(""));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Idle, StateSource::Screen)),
        "the screen takes over after a lag resync"
    );
}

#[test]
fn a_restated_state_is_no_transition() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));
    harness.feed(&claude_record("done"));
    let changes = harness.changes();

    harness.feed(&claude_record("idle"));
    harness.feed(&claude_record("done"));
    harness.feed(&claude_record("error"));

    assert_eq!(
        harness.changes(),
        changes,
        "done, idle and error are one activity"
    );
}

#[test]
fn a_manifest_without_the_table_ignores_records_and_rejects_a_stray_override() {
    let mut harness = Harness::claude_without_opt_in();
    harness.feed(&captured("claude", "idle"));
    let before = harness.activity();

    harness.feed(&claude_record("working"));

    assert_eq!(harness.activity(), before);
    assert!(
        harness.record_preview().is_none(),
        "no program_status region"
    );

    let stray = format!(
        "{}\n[[rules]]\nid = \"x\"\nstate = \"idle\"\npriority = 1\nregion = \"whole_recent\"\n\
         contains = [\"x\"]\noverrides_program_status = true\n",
        plain_source("claude")
    );
    assert!(matches!(
        Manifest::parse_str(&stray),
        Err(ManifestError::OverrideWithoutProgramStatus { .. })
    ));
    assert!(matches!(
        Manifest::parse_str("[program_status]\napp = \"has space\"\n"),
        Err(ManifestError::InvalidProgramStatusApp { .. })
    ));
}

/// Screen text `rows` with the title `title`, as one frame.
fn titled(title: &str, rows: &[&str]) -> Vec<u8> {
    frame(Some(title), rows)
}

#[test]
fn a_working_record_beats_an_idle_screen_and_a_screen_only_chunk_cannot_undo_it() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Idle, StateSource::Screen))
    );

    harness.feed(&claude_record("working"));
    assert_eq!(harness.activity(), WORKING);

    // Chunks that change only the screen, and chunks that change nothing.
    for _ in 0..3 {
        let transitions = harness.feed_raw(&captured("claude", "idle"));
        assert!(
            transitions
                .iter()
                .all(|transition| transition.activity == AgentActivity::Working),
            "no idle edge while the record says working: {transitions:?}"
        );
    }
    harness.settle();
    assert_eq!(harness.activity(), WORKING);
}

#[test]
fn a_visible_dialog_wins_over_every_non_blocked_record() {
    for state in ["working", "idle", "done", "error"] {
        let mut harness = Harness::claude();
        harness.feed(&captured("claude", "idle"));
        harness.feed(&claude_record(state));

        harness.feed(&captured("claude", "trust"));

        assert_eq!(
            harness.activity(),
            Some((AgentActivity::Blocked, StateSource::Screen)),
            "{state}: the folder-trust dialog stays blocked"
        );

        // A forged record cannot hide the dialog afterwards either.
        harness.feed(&claude_record("idle"));
        harness.feed(&claude_record("working"));
        assert_eq!(
            harness.activity(),
            Some((AgentActivity::Blocked, StateSource::Screen)),
            "{state}: a later forged record"
        );
    }
}

#[test]
fn a_blocked_record_beats_the_screen_and_gives_way_to_the_next_record() {
    for (pairs, canonical) in [
        ("blocked", "app=claude-code:state=blocked"),
        (
            "blocked:kind=permission",
            "app=claude-code:state=blocked:kind=permission",
        ),
    ] {
        let mut harness = Harness::claude();
        harness.feed(&captured("claude", "idle"));

        harness.feed(&record(&format!("state={pairs}:app={CLAUDE_APP}")));
        assert_eq!(harness.activity(), BLOCKED, "{pairs}");
        assert_eq!(harness.record_preview().as_deref(), Some(canonical));

        harness.feed(&claude_record("working"));
        assert_eq!(harness.activity(), WORKING, "{pairs}");
    }
}

/// An OSC 9 "approval requested" notice. The detector holds every such notice
/// (OSC 9/777/99) as an attention hint, whatever the active manifest.
const APPROVAL_NOTICE: &[u8] = b"\x1b]9;Approval requested\x07";

/// Takes the notice the last feed held and reports whether an authoritative
/// record is present as well; the session loop turns the notice into attention
/// only when none is.
fn take_notice(harness: &mut Harness) -> (bool, bool) {
    let held = harness.detector.take_approval_notice();
    let record_present = harness.detector.status(harness.now).record_state.is_some();
    (held, record_present)
}

/// Distinct `blocked` states the detector published.
fn blocked_changes(harness: &Harness) -> usize {
    harness
        .changes()
        .iter()
        .filter(|(activity, _)| *activity == AgentActivity::Blocked)
        .count()
}

#[test]
fn a_blocked_record_and_the_notice_in_one_chunk_are_one_attention_source_in_either_order() {
    let record = claude_record("blocked");
    for (name, chunk) in [
        (
            "record first",
            [record.as_slice(), APPROVAL_NOTICE].concat(),
        ),
        (
            "notice first",
            [APPROVAL_NOTICE, record.as_slice()].concat(),
        ),
    ] {
        let mut harness = Harness::claude();
        harness.feed(&captured("claude", "idle"));

        harness.now += STEP;
        let transitions = harness.detector.feed(harness.now, &chunk);
        harness.note(transitions);
        let (held, record_present) = take_notice(&mut harness);
        harness.settle();
        assert!(held, "{name}: the notice is recognized");
        assert!(record_present, "{name}: the record drops the notice");
        assert_eq!(blocked_changes(&harness), 1, "{name}");
        assert_eq!(harness.activity(), BLOCKED, "{name}");
    }
}

#[test]
fn a_notice_after_a_blocked_record_is_dropped_by_the_record() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));
    harness.feed(&claude_record("blocked"));
    assert_eq!(harness.activity(), BLOCKED);

    harness.now += STEP;
    let transitions = harness.detector.feed(harness.now, APPROVAL_NOTICE);
    harness.note(transitions);
    assert_eq!(take_notice(&mut harness), (true, true));
    assert_eq!(blocked_changes(&harness), 1);
}

#[test]
fn a_notice_before_a_blocked_record_is_held_once_and_the_record_adds_the_blocked_state() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));

    // The notice is the only source until the record arrives; the attention
    // coordinator merges both under one session key.
    harness.now += STEP;
    let transitions = harness.detector.feed(harness.now, APPROVAL_NOTICE);
    harness.note(transitions);
    assert_eq!(take_notice(&mut harness), (true, false));

    harness.feed(&claude_record("blocked"));
    assert_eq!(harness.activity(), BLOCKED);
    assert_eq!(blocked_changes(&harness), 1);
    assert_eq!(take_notice(&mut harness), (false, true));
}

#[test]
fn a_working_record_beats_a_screen_blocker_that_is_not_visible() {
    let mut harness = Harness::codex();
    harness.feed(&titled(
        "cxw",
        &["  Run it? [y/n]", "", "\u{203a} Ask Codex to do anything"],
    ));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Blocked, StateSource::Screen)),
        "without a record the weak blocker reads blocked"
    );

    harness.feed(&record(&format!("state=working:app={CODEX_APP}")));

    assert_eq!(harness.activity(), WORKING);
}

#[test]
fn a_background_footer_reads_idle_beside_a_weak_blocker_only_with_a_working_record() {
    let footer = "  1 background terminal running · /ps to view · /stop to close";
    let screen = frame(
        Some("cx"),
        &[
            "  Run it? [y/n]",
            "",
            "\u{203a} Ask Codex to do anything",
            "",
            footer,
        ],
    );

    let mut without_record = Harness::codex();
    without_record.feed(&screen);
    assert_eq!(
        without_record.activity(),
        Some((AgentActivity::Blocked, StateSource::Screen)),
        "no record: the weak blocker reads, as without the override rule"
    );

    let mut with_record = Harness::codex();
    with_record.feed(&record(&format!("state=working:app={CODEX_APP}")));
    with_record.feed(&screen);
    assert_eq!(
        with_record.activity(),
        Some((AgentActivity::Idle, StateSource::Screen)),
        "a working record: the background footer outranks the weak blocker"
    );
}

/// A manifest with one program-status override rule over a Claude-shaped idle
/// editor, to exercise the override flag without a pin move.
const OVERRIDE_MANIFEST: &str = r#"
[program_status]
app = "probe"

[[rules]]
id = "turn_title"
state = "working"
priority = 1300
region = "osc_title"
contains = ["RUNNING"]

[[rules]]
id = "background_only"
state = "idle"
priority = 1200
region = "whole_recent"
contains = ["BACKGROUND-ONLY"]
overrides_program_status = true

[[rules]]
id = "plain_idle"
state = "idle"
priority = 100
region = "whole_recent"
contains = ["READY"]
"#;

fn override_harness() -> Harness {
    Harness::new(config(OVERRIDE_MANIFEST, InputReadiness::ScreenIdle))
}

#[test]
fn an_override_rule_beats_a_working_record_unless_a_stronger_rule_wins() {
    let mut harness = override_harness();
    harness.feed(&record("state=working:app=probe"));
    harness.feed(&frame(Some("idle"), &["BACKGROUND-ONLY"]));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Idle, StateSource::Screen)),
        "the override rule wins over the working record"
    );

    // The turn title outranks the override rule, so the record's working holds.
    harness.feed(&frame(Some("RUNNING"), &["BACKGROUND-ONLY"]));
    assert_eq!(
        harness.activity(),
        WORKING,
        "the stronger rule is no override, so the record's working stands"
    );

    // A rule that does not override yields to the record.
    let mut plain = override_harness();
    plain.feed(&record("state=working:app=probe"));
    plain.feed(&frame(Some("idle"), &["READY"]));
    assert_eq!(plain.activity(), WORKING);
}

/// An override rule that outranks a weak blocker and a screen it sits below.
const OVERRIDE_OVER_WEAK_MANIFEST: &str = r#"
[program_status]
app = "probe"

[[rules]]
id = "turn_title"
state = "working"
priority = 1300
region = "osc_title"
contains = ["RUNNING"]

[[rules]]
id = "background_only"
state = "idle"
priority = 650
region = "whole_recent"
contains = ["BACKGROUND-ONLY"]
overrides_program_status = true

[[rules]]
id = "weak"
state = "blocked"
priority = 600
region = "whole_recent"
contains = ["[y/n]"]

[[rules]]
id = "plain_idle"
state = "idle"
priority = 100
region = "osc_title"
contains = ["idle"]
"#;

fn override_over_weak() -> Harness {
    Harness::new(config(
        OVERRIDE_OVER_WEAK_MANIFEST,
        InputReadiness::ScreenIdle,
    ))
}

#[test]
fn an_override_rule_beats_a_weak_blocker_only_while_a_record_is_held() {
    let screen = frame(Some("idle"), &["BACKGROUND-ONLY", "Run it? [y/n]"]);

    // No record held: the override rule is skipped and the weak blocker reads.
    let mut without_record = override_over_weak();
    without_record.feed(&screen);
    assert_eq!(
        without_record.activity(),
        Some((AgentActivity::Blocked, StateSource::Screen))
    );

    // A working record held: the override rule outranks the weak blocker.
    let mut with_record = override_over_weak();
    with_record.feed(&record("state=working:app=probe"));
    with_record.feed(&screen);
    assert_eq!(
        with_record.activity(),
        Some((AgentActivity::Idle, StateSource::Screen))
    );

    // The record is dropped by a reconfigure: the weak blocker reads again.
    with_record.detector.reconfigure(
        with_record.now,
        config(OVERRIDE_OVER_WEAK_MANIFEST, InputReadiness::ScreenIdle),
    );
    with_record.feed(&screen);
    assert_eq!(
        with_record.activity(),
        Some((AgentActivity::Blocked, StateSource::Screen))
    );

    // A record returns: the override rule applies again.
    with_record.feed(&record("state=working:app=probe"));
    with_record.feed(&screen);
    assert_eq!(
        with_record.activity(),
        Some((AgentActivity::Idle, StateSource::Screen))
    );
}

#[test]
fn a_turn_title_still_outranks_the_override_rule_over_a_weak_blocker() {
    let mut harness = override_over_weak();
    harness.feed(&record("state=working:app=probe"));
    harness.feed(&frame(
        Some("RUNNING"),
        &["BACKGROUND-ONLY", "Run it? [y/n]"],
    ));
    assert_eq!(harness.activity(), WORKING);
}

#[test]
fn an_override_rule_never_releases_the_initial_input() {
    let mut harness = pending(config(
        OVERRIDE_OVER_WEAK_MANIFEST,
        InputReadiness::ScreenIdle,
    ));
    harness.feed(&record("state=working:app=probe"));
    harness.feed(&frame(Some("x"), &["BACKGROUND-ONLY"]));
    assert!(!harness.detector.input_ready());
}

#[test]
fn a_manifest_without_the_table_ignores_records_while_they_arrive() {
    let shell = || {
        let mut config = DetectorConfig::generic_shell();
        config.detection = fast_detection();
        Harness::new(config)
    };
    let bytes = |with_records: bool| {
        let mut harness = shell();
        let mut chunks = vec![b"user@host:~$ ".to_vec(), b"ls\r\n".to_vec()];
        if with_records {
            chunks.insert(1, claude_record("working"));
            chunks.push(claude_record("idle"));
        }
        for chunk in chunks {
            harness.feed(&chunk);
        }
        harness.changes()
    };

    assert_eq!(bytes(true), bytes(false));
}

/// No record means every captured screen of `runtime` reads exactly as
/// without the opt-in.
fn no_record_means_every_captured_screen_reads_as_without_the_opt_in(
    runtime: &str,
    app: &str,
    readiness: InputReadiness,
) {
    let directory = workspace_root().join(format!("compat/{runtime}/screens"));
    let mut names = Vec::new();
    for sub in [
        "",
        "widths",
        "n-1",
        "n-1/widths",
        "background",
        "background/widths",
    ] {
        let Ok(entries) = fs::read_dir(directory.join(sub)) else {
            continue;
        };
        for entry in entries.map(|entry| entry.expect("directory entry").path()) {
            if entry
                .extension()
                .is_some_and(|extension| extension == "txt")
            {
                let stem = entry.file_stem().expect("file stem").to_string_lossy();
                names.push(if sub.is_empty() {
                    stem.into_owned()
                } else {
                    format!("{sub}/{stem}")
                });
            }
        }
    }
    names.sort();
    assert!(!names.is_empty(), "{runtime} has captured screens");
    for name in names {
        let bytes = captured(runtime, &name);
        let mut plain = Harness::new(config(&plain_source(runtime), readiness));
        let mut opted = Harness::new(config(&opted_in_source(runtime, app, true), readiness));
        plain.feed(&bytes);
        opted.feed(&bytes);

        assert_eq!(plain.transitions, opted.transitions, "{runtime}/{name}");
        assert_eq!(
            plain.detector.input_ready(),
            opted.detector.input_ready(),
            "{runtime}/{name}"
        );
    }
}

#[test]
fn no_record_means_every_captured_claude_screen_reads_exactly_as_without_the_opt_in() {
    no_record_means_every_captured_screen_reads_as_without_the_opt_in(
        "claude",
        CLAUDE_APP,
        InputReadiness::ScreenIdle,
    );
}

#[test]
fn no_record_means_every_captured_codex_screen_reads_exactly_as_without_the_opt_in() {
    no_record_means_every_captured_screen_reads_as_without_the_opt_in(
        "codex",
        CODEX_APP,
        InputReadiness::ManifestIdle,
    );
}

#[test]
fn no_record_means_every_captured_pi_screen_reads_exactly_as_without_the_opt_in() {
    no_record_means_every_captured_screen_reads_as_without_the_opt_in(
        "pi",
        "pi",
        InputReadiness::ScreenIdle,
    );
}

/// A harness whose initial-input wait is still pending, as at session start.
fn pending(config: DetectorConfig) -> Harness {
    let now = Instant::now();
    Harness {
        detector: Detector::new(ROWS, COLUMNS, now, config),
        now,
        published: None,
        transitions: Vec::new(),
    }
}

#[test]
fn a_record_never_releases_or_holds_the_initial_input() {
    for (runtime, app, readiness, idle_screen) in [
        (
            "claude",
            CLAUDE_APP,
            InputReadiness::ScreenIdle,
            captured("claude", "idle"),
        ),
        (
            "codex",
            CODEX_APP,
            InputReadiness::ManifestIdle,
            captured("codex", "idle"),
        ),
        (
            "pi",
            "pi",
            InputReadiness::ScreenIdle,
            captured("pi", "idle"),
        ),
    ] {
        let source = opted_in_source(runtime, app, true);
        let mut with_records = pending(config(&source, readiness));
        let mut without = pending(config(&source, readiness));

        // Records alone, over a blank screen, release nothing.
        for state in ["idle", "done", "working"] {
            with_records.feed(&record(&format!("state={state}:app={app}")));
        }
        assert!(
            !with_records.detector.input_ready(),
            "{runtime}: records alone"
        );

        // Once the screen shows the prompt, both agree, whatever the record says.
        with_records.feed(&idle_screen);
        without.feed(&idle_screen);
        assert_eq!(
            with_records.detector.input_ready(),
            without.detector.input_ready(),
            "{runtime}: the prompt is ready with and without records"
        );
        assert!(with_records.detector.input_ready(), "{runtime}");

        // A blocked record neither blocks nor changes the readiness verdict.
        with_records.feed(&record(&format!("state=blocked:app={app}")));
        assert!(
            with_records.detector.input_ready(),
            "{runtime}: a blocked record never raises the idle barrier"
        );
    }
}

#[test]
fn a_record_is_not_a_visible_blocker_for_the_codex_startup_barrier() {
    // A startup popup is drawn, cleared, and the idle title arrives; records are
    // interleaved at every step. The readiness verdicts match the run without.
    let popup = captured("codex", "n-1/trust");
    let idle = captured("codex", "idle");
    let steps: Vec<Vec<u8>> = vec![popup, b"\x1b[2J\x1b[H".to_vec(), idle];
    let source = opted_in_source("codex", CODEX_APP, false);
    let verdicts = |with_records: bool| {
        let mut harness = pending(config(&source, InputReadiness::ManifestIdle));
        let mut verdicts = Vec::new();
        for step in &steps {
            if with_records {
                harness.feed(&record(&format!("state=working:app={CODEX_APP}")));
            }
            harness.feed(step);
            if with_records {
                harness.feed(&record(&format!("state=done:app={CODEX_APP}")));
            }
            verdicts.push(harness.detector.input_ready());
        }
        verdicts
    };

    assert_eq!(verdicts(true), verdicts(false));
}

#[test]
fn a_report_text_with_bidirectional_or_format_characters_is_never_kept() {
    use base64::Engine as _;

    let text = "evil\u{202e}text\u{200b}";
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));

    harness.feed(&record(&format!(
        "state=working:app={CLAUDE_APP}:msg={encoded}"
    )));

    assert_eq!(harness.activity(), WORKING);
    let exposed = format!(
        "{:?} {:?} {:?}",
        harness.detector,
        harness.detector.region_previews(),
        harness.transitions
    );
    assert!(!exposed.contains("evil") && !exposed.contains(&encoded));
}

/// Two records in one chunk, the way a sub-sample turn arrives.
fn working_then_done() -> Vec<u8> {
    [claude_record("working"), claude_record("done")].concat()
}

#[test]
fn a_working_and_a_done_record_in_one_chunk_still_yield_working_evidence() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));

    let transitions = harness.feed_raw(&working_then_done());

    assert_eq!(
        transitions
            .first()
            .map(|transition| (transition.activity, transition.source)),
        WORKING,
        "the working record sighted inside the chunk publishes working: {transitions:?}"
    );
    harness.settle();
    assert_eq!(
        harness.activity(),
        IDLE,
        "the chunk-end record confirms idle"
    );
}

#[test]
fn a_record_only_chunk_never_flips_a_visible_dialog_to_working() {
    for record_state in ["working", "idle"] {
        let mut harness = Harness::claude();
        harness.feed(&captured("claude", "idle"));
        harness.feed(&captured("claude", "trust"));
        assert_eq!(
            harness.activity(),
            Some((AgentActivity::Blocked, StateSource::Screen))
        );
        let before = harness.changes();

        // Chunks that change nothing on screen: a record, cursor output.
        for chunk in [claude_record(record_state), b"\x1b[?25l".to_vec()] {
            let transitions = harness.feed_raw(&chunk);
            assert!(
                transitions
                    .iter()
                    .all(|transition| transition.activity == AgentActivity::Blocked),
                "{record_state}: the dialog stays blocked: {transitions:?}"
            );
        }
        harness.settle();
        assert_eq!(harness.changes(), before, "{record_state}");
    }
}

#[test]
fn an_override_repaint_with_a_working_record_adds_no_working_edge() {
    let mut harness = override_harness();
    harness.feed(&record("state=working:app=probe"));
    harness.feed(&frame(Some("idle"), &["BACKGROUND-ONLY"]));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Idle, StateSource::Screen))
    );

    // A full repaint passes through a cleared screen before the footer returns.
    let transitions = harness.feed_raw(&frame(Some("idle"), &["BACKGROUND-ONLY"]));

    assert!(
        transitions
            .iter()
            .all(|transition| transition.activity != AgentActivity::Working),
        "{transitions:?}"
    );
}

#[test]
fn an_override_rule_keeps_the_working_latch_without_a_working_record() {
    let latched = |prefix: &[u8], record_after: &[u8]| {
        let mut harness = override_harness();
        harness.feed(&frame(Some("idle"), &["BACKGROUND-ONLY", "READY"]));
        let chunk = [
            prefix,
            &frame(Some("RUNNING"), &["BACKGROUND-ONLY"]),
            record_after,
            &frame(Some("idle"), &["BACKGROUND-ONLY", "READY"]),
        ]
        .concat();
        harness.feed_raw(&chunk)
    };

    // No record at all: an older program that never reports. The override
    // rule is skipped, so the chunk ends idle through the plain idle rule.
    let without_record = latched(b"", b"");
    assert!(
        without_record
            .iter()
            .any(|transition| transition.activity == AgentActivity::Working),
        "the turn title latches working: {without_record:?}"
    );

    // A working record that moved to done inside the chunk.
    let finished = latched(
        &record("state=working:app=probe"),
        &record("state=done:app=probe"),
    );
    assert!(
        finished
            .iter()
            .any(|transition| transition.activity == AgentActivity::Working),
        "the finished turn still latches working: {finished:?}"
    );
}

#[test]
fn a_report_is_not_an_event_without_the_opt_in() {
    // A BEL-terminated record splits the screen feed into segments only when the
    // manifest opted in; without it the output is handled as any other output.
    let bytes = [
        captured("codex", "idle"),
        b"\x1b]7501;state=working:app=codex\x07".to_vec(),
    ]
    .concat();
    let mut plain = Harness::new(config(&plain_source("codex"), InputReadiness::ManifestIdle));
    let mut stripped = Harness::new(config(&plain_source("codex"), InputReadiness::ManifestIdle));
    plain.feed(&bytes);
    stripped.feed(&captured("codex", "idle"));

    assert_eq!(plain.transitions, stripped.transitions);
}

/// The staleness window the shipped Claude manifest declares, in milliseconds;
/// the scenarios bound their virtual advances to fractions of it, never a
/// second literal.
fn stale_window() -> Duration {
    let manifest = Manifest::parse_str(&read("runtime-packages/claude/detect.toml"))
        .expect("the shipped Claude manifest parses");
    manifest
        .program_status()
        .expect("Claude opts in to program-status")
        .stale_after()
        .expect("Claude declares stale_after_ms")
}

/// Records idle after `feed`, so the first feed that shows the disagreement
/// starts the staleness clock.
fn idle_record_first(harness: &mut Harness) {
    harness.feed(&captured("claude", "idle"));
}

#[test]
fn a_stale_blocked_record_gives_way_to_the_idle_screen() {
    let window = stale_window();
    let mut harness = Harness::claude();
    idle_record_first(&mut harness);
    harness.feed(&claude_record("blocked:kind=permission"));

    // The screen drops to the idle prompt box while the record stands.
    harness.feed(&captured("claude", "idle"));
    harness.advance(window / 2);
    assert_eq!(
        harness.activity(),
        BLOCKED,
        "a fresh held record is still authoritative"
    );

    harness.advance(window + Duration::from_secs(5));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Idle, StateSource::Screen)),
        "a blocked record over an idle screen for the window gives way"
    );
}

#[test]
fn a_stale_idle_record_gives_way_to_the_working_screen() {
    let window = stale_window();
    let mut harness = Harness::claude();
    idle_record_first(&mut harness);
    harness.feed(&claude_record("idle"));

    // The spinner frame is working screen evidence without a dialog.
    harness.feed(&captured("claude", "working_early"));
    harness.advance(window / 2);
    assert_eq!(harness.activity(), IDLE, "a fresh held record stands");

    harness.advance(window + Duration::from_secs(5));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Working, StateSource::Screen)),
        "an idle record over a spinner for the window gives way"
    );
}

#[test]
fn a_stale_working_record_gives_way_to_a_weak_screen_blocker() {
    let window = stale_window();
    let mut harness = Harness::claude();
    idle_record_first(&mut harness);
    harness.feed(&claude_record("working"));

    // The weak blocker is blocked screen evidence with no visible blocker.
    harness.feed(&frame(None, &["Waiting for permission"]));
    harness.advance(window / 2);
    assert_eq!(harness.activity(), WORKING, "a fresh held record stands");

    harness.advance(window + Duration::from_secs(5));
    let activity = harness.activity();
    assert_eq!(
        activity,
        Some((AgentActivity::Blocked, StateSource::Screen)),
        "a working record over a weak blocker for the window gives way"
    );
    assert!(
        harness.detector.status(harness.now).record_state.is_some(),
        "the record is still held; only the arbitration changed"
    );
}

#[test]
fn a_dialog_wins_over_a_non_blocked_record_without_waiting() {
    for state in ["working", "idle", "done", "error"] {
        let mut harness = Harness::claude();
        idle_record_first(&mut harness);
        harness.feed(&claude_record(state));

        harness.feed(&captured("claude", "trust"));
        assert_eq!(
            harness.activity(),
            Some((AgentActivity::Blocked, StateSource::Screen)),
            "{state}: a visible dialog wins at once"
        );
    }
}

#[test]
fn a_restated_record_restarts_the_staleness_clock() {
    let window = stale_window();
    let mut harness = Harness::claude();
    idle_record_first(&mut harness);
    harness.feed(&claude_record("blocked:kind=permission"));
    harness.feed(&captured("claude", "idle"));

    harness.advance(window.mul_f64(0.75));
    // A finished prompt answers late: the same record arrives again.
    harness.feed(&claude_record("blocked:kind=permission"));

    harness.advance(window.mul_f64(0.75));
    assert_eq!(
        harness.activity(),
        BLOCKED,
        "the restated record restarted the clock: past the first window"
    );

    harness.advance(window / 2);
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Idle, StateSource::Screen)),
        "past the restated window the screen decides"
    );
}

#[test]
fn a_fresh_record_restores_record_authority_at_once() {
    let window = stale_window();
    let mut harness = Harness::claude();
    idle_record_first(&mut harness);
    harness.feed(&claude_record("blocked:kind=permission"));
    harness.feed(&captured("claude", "idle"));
    harness.advance(window + Duration::from_secs(5));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Idle, StateSource::Screen)),
        "the setup reached the screen fallback"
    );

    harness.feed(&claude_record("working"));
    assert_eq!(
        harness.activity(),
        WORKING,
        "a fresh record decides at once"
    );
    assert!(
        harness.detector.status(harness.now).record_state.is_some(),
        "the fresh record is held"
    );
}

#[test]
fn agreement_with_the_screen_restores_record_authority() {
    let window = stale_window();
    let mut harness = Harness::claude();
    idle_record_first(&mut harness);
    harness.feed(&claude_record("idle"));
    harness.feed(&captured("claude", "working_early"));
    harness.advance(window + Duration::from_secs(5));
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Working, StateSource::Screen)),
        "the setup reached the screen fallback"
    );

    harness.feed(&captured("claude", "idle"));
    harness.settle();
    assert_eq!(
        harness.activity(),
        IDLE,
        "the record agrees with the screen again, so it decides"
    );
}

#[test]
fn every_captured_dialog_keeps_a_held_blocked_record() {
    for name in ["approval", "askuser", "trust", "theme", "apikey", "login"] {
        let window = stale_window();
        let mut harness = Harness::claude();
        idle_record_first(&mut harness);
        harness.feed(&claude_record("blocked"));
        harness.feed(&captured("claude", name));

        harness.advance(2 * window);
        assert_eq!(
            harness.activity(),
            Some((AgentActivity::Blocked, StateSource::Screen)),
            "{name}: a dialog never leaves blocked to the fallback"
        );
    }
}

#[test]
fn the_fallback_transition_and_status_carry_the_frozen_fallback() {
    let window = stale_window();
    let mut harness = Harness::claude();
    idle_record_first(&mut harness);
    harness.feed(&claude_record("blocked:kind=permission"));
    harness.feed(&captured("claude", "idle"));
    let published = harness.published.expect("the blocked record publishes");
    assert!(
        !published.fallback,
        "the record's own edge is not a fallback"
    );
    assert!(
        harness
            .detector
            .status(harness.now)
            .into_protocol(None)
            .fallback
            .is_none(),
        "no disagreement yet means no fallback in session.detection"
    );

    harness.advance(window + Duration::from_secs(5));
    let published = harness.published.expect("the fallback publishes");
    assert_eq!(
        (published.activity, published.source),
        (AgentActivity::Idle, StateSource::Screen),
        "the screen decides"
    );
    assert!(
        published.fallback,
        "the edge under a stale record is marked as a fallback"
    );
    let fallback = harness
        .detector
        .status(harness.now)
        .into_protocol(None)
        .fallback
        .expect("the fallback stays visible in session.detection");
    assert_eq!(fallback.record_state, "blocked");
    assert_eq!(fallback.rule_id, "live_prompt_box");
    assert_eq!(fallback.screen_state, AgentActivity::Idle);
    assert!(
        fallback.record_age_ms >= u64::try_from(window.as_millis()).unwrap_or(u64::MAX),
        "the frozen record age passed the window: {}",
        fallback.record_age_ms
    );

    // The restated screen keeps the source, so it publishes no edge, and the
    // frozen fallback stays on the status.
    harness.feed(&captured("claude", "idle"));
    assert_eq!(
        harness
            .detector
            .status(harness.now)
            .into_protocol(None)
            .fallback
            .as_ref()
            .map(|fallback| fallback.record_age_ms),
        Some(fallback.record_age_ms),
        "the frozen values stay the same on every restated event"
    );

    harness.feed(&claude_record("working"));
    assert!(
        harness
            .detector
            .status(harness.now)
            .into_protocol(None)
            .fallback
            .is_none(),
        "a fresh record ends the fallback"
    );
}

/// Ending the fallback re-records the published transition without the mark
/// (and with a fresh refresh timestamp): no later evaluation restates a
/// fallback that is over, and no caller resolves a descriptor from no active
/// fallback on the task path. The one-shot variant with the refresh disabled
/// makes the record's own publish the only candidate; the faster-refresh
/// variant below first lets the fresh record's own unmarked publish land,
/// then waits for its refresh.
fn fallback_end_setup(harness: &mut Harness) {
    let window = stale_window();
    idle_record_first(harness);
    harness.feed(&claude_record("blocked:kind=permission"));
    harness.feed(&captured("claude", "idle"));
    harness.advance(window + Duration::from_secs(5));
    let fallback = harness.published.expect("the fallback publishes");
    assert_eq!(
        (fallback.activity, fallback.source),
        (AgentActivity::Idle, StateSource::Screen),
        "the setup reached the screen fallback"
    );
    assert!(fallback.fallback, "the edge under a stale record is marked");

    // A fresh `done` record ends the fallback (Record).
    let ended = harness.feed_raw(&claude_record("done"));
    assert!(ended.is_empty(), "the fresh record ends the fallback");
    assert!(
        harness.detector.status(harness.now).stale.is_none(),
        "the record is authoritative again"
    );
}

/// With the refresh disabled, the record's own idle publishes unmarked.
#[test]
fn a_record_publish_after_the_fallback_end_carries_no_fallback() {
    let mut harness = Harness::claude_no_refresh();
    fallback_end_setup(&mut harness);

    let published = harness.tick_until_published(Duration::from_millis(100), 30);
    assert_eq!(
        published.len(),
        1,
        "the record's own state publishes after the fallback end"
    );
    assert_eq!(
        (published[0].activity, published[0].source),
        (AgentActivity::Idle, StateSource::OscProgress),
        "the publish is the fresh record's idle"
    );
    assert!(
        !published[0].fallback,
        "the record's publish after the fallback end carries no fallback"
    );
    assert!(
        harness
            .detector
            .status(harness.now)
            .into_protocol(None)
            .fallback
            .is_none(),
        "the ended fallback stays absent from session.detection"
    );
}

/// A reconfigure during an active fallback ends the staleness before the
/// state replacement drops the published transition: no panic, and the
/// fallback is gone. The new manifest decides what the next evidence
/// publishes.
#[test]
fn a_reconfigure_during_an_active_fallback_ends_the_staleness() {
    let window = stale_window();
    let mut harness = Harness::claude();
    idle_record_first(&mut harness);
    harness.feed(&claude_record("blocked:kind=permission"));
    harness.feed(&captured("claude", "idle"));
    harness.advance(window + Duration::from_secs(5));
    assert!(
        harness.detector.status(harness.now).stale.is_some(),
        "the setup holds an active fallback"
    );

    let manifest = Manifest::parse_str(&opted_in_source("claude", CLAUDE_APP, true))
        .expect("the manifest parses");
    harness.now += STEP;
    harness.detector.reconfigure(
        harness.now,
        DetectorConfig {
            launch_name: None,
            runtime_version: manifest
                .program_status()
                .and_then(ProgramStatusConfig::authority_from),
            gate_waived: false,
            detection: DetectionConfig::default(),
            manifest: Some(Arc::new(manifest)),
            input_readiness: InputReadiness::ScreenIdle,
        },
    );
    assert!(
        harness.detector.status(harness.now).stale.is_none(),
        "the reconfigure ends the active fallback"
    );
    assert!(
        harness
            .detector
            .status(harness.now)
            .into_protocol(None)
            .fallback
            .is_none(),
        "the ended fallback stays absent from session.detection"
    );
}

/// With the faster refresh, the refresh after the interval restates the
/// arbitrated state without the mark. The fresh `done` record's own idle
/// publishes first (its source differs from the fallback screen edge), so
/// the test lets that debounce complete and then waits for the refresh of
/// the republished state: the 200 ms refresh must not restate a fallback
/// that is over. Dropping the `record_published` re-record from
/// `end_staleness` fails this deterministically: the stale fallback mark
/// stays on the published transition, so the fresh record's own publish
/// keeps it and the assertion below fires on the first publish.
#[test]
fn a_refresh_after_the_fallback_end_carries_no_fallback() {
    let mut harness = Harness::claude_fast_refresh();
    fallback_end_setup(&mut harness);

    let published = harness.tick_until_published(Duration::from_millis(100), 30);
    assert_eq!(published.len(), 1, "the fresh record publishes");
    assert!(
        !published[0].fallback,
        "the fresh record publishes without the fallback mark"
    );
    let restated = harness.tick_until_published(Duration::from_millis(100), 30);
    assert_eq!(restated.len(), 1, "a refresh follows the fresh publish");
    assert_eq!(
        (restated[0].activity, restated[0].source),
        (published[0].activity, published[0].source),
        "the refresh restates the republished state"
    );
    assert!(
        !restated[0].fallback,
        "a refresh after the fallback end carries no fallback"
    );
    assert!(
        harness
            .detector
            .status(harness.now)
            .into_protocol(None)
            .fallback
            .is_none(),
        "the ended fallback stays absent from session.detection"
    );
}

#[test]
fn a_codex_record_has_no_staleness_window() {
    let mut harness = Harness::codex();
    harness.feed(&record(&format!("state=working:app={CODEX_APP}")));
    harness.feed(&titled(
        "cxw",
        &["  Run it? [y/n]", "", "\u{203a} Ask Codex to do anything"],
    ));
    assert_eq!(harness.activity(), WORKING);

    harness.advance(Duration::from_hours(48));
    assert_eq!(
        harness.activity(),
        WORKING,
        "without stale_after_ms a held record never expires"
    );
}

#[test]
fn stale_after_ms_rejects_zero_and_an_hour_plus_a_millisecond() {
    for value in ["0", "3600001"] {
        let manifest = format!("[program_status]\napp = \"synth\"\nstale_after_ms = {value}\n");
        assert!(
            matches!(
                Manifest::parse_str(&manifest),
                Err(ManifestError::InvalidProgramStatusStaleAfter { .. })
            ),
            "{value}: outside 1..=3600000"
        );
    }
    let manifest = "[program_status]\napp = \"synth\"\nstale_after_ms = 30000\n";
    let parsed = Manifest::parse_str(manifest).expect("the in-range window parses");
    assert_eq!(
        parsed.program_status().expect("opted in").stale_after(),
        Some(Duration::from_secs(30)),
        "the window is kept as a duration"
    );
}
