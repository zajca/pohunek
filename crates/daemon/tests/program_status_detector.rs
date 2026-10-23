//! OSC 7501 program-status evidence in the activity detector.
//!
//! The official packages do not opt in yet, so each scenario appends a
//! `[program_status]` table to the shipped manifest of a runtime and feeds the
//! detector the screens captured from the real agents (`compat/*/screens`)
//! together with raw 7501 records, the way the session's output stream does.
//! Time is a virtual `Instant`: nothing here sleeps.

// Rust guideline compliant 2026-06-26

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pohunek_daemon::agent::host::InputReadiness;
use pohunek_daemon::detect::{
    ActivityTransition, DetectionConfig, Detector, DetectorConfig, Manifest, ManifestError,
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

/// The shipped manifest of `runtime`, with the opt-in table appended.
fn opted_in_source(runtime: &str, app: &str, answer_probe: bool) -> String {
    format!(
        "{}\n[program_status]\napp = \"{app}\"\nanswer_probe = {answer_probe}\n",
        read(&format!("runtime-packages/{runtime}/detect.toml"))
    )
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

fn config(manifest: &str, readiness: InputReadiness) -> DetectorConfig {
    DetectorConfig {
        detection: fast_detection(),
        manifest: Some(Arc::new(
            Manifest::parse_str(manifest).expect("the manifest parses"),
        )),
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

    fn claude() -> Self {
        Self::new(config(
            &opted_in_source("claude", CLAUDE_APP, true),
            InputReadiness::ScreenIdle,
        ))
    }

    fn claude_without_opt_in() -> Self {
        Self::new(config(
            &read("runtime-packages/claude/detect.toml"),
            InputReadiness::ScreenIdle,
        ))
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

#[test]
fn every_record_state_is_held_whatever_the_terminator_and_the_chunking() {
    let cases: [(&str, Observed, &str); 7] = [
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
    for (state, expected, canonical) in cases {
        for terminator in ["\x1b\\", "\x07"] {
            let bytes =
                format!("\x1b]7501;state={state}:app={CLAUDE_APP}{terminator}").into_bytes();
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
        read("runtime-packages/claude/detect.toml")
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

/// A Claude screen whose only working evidence is a markdown bullet that the
/// spinner rule reads as a live status line.
fn claude_stale_spinner() -> Vec<u8> {
    let rule = "\u{2500}".repeat(40);
    frame(
        None,
        &[
            "* Then it waits\u{2026} (see below)",
            &rule,
            "\u{276f} ",
            &rule,
            "  ? for shortcuts",
        ],
    )
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

#[test]
fn an_idle_record_beats_a_stale_working_screen_winner() {
    let mut harness = Harness::claude();
    harness.feed(&claude_stale_spinner());
    assert_eq!(
        harness.activity(),
        Some((AgentActivity::Working, StateSource::Screen)),
        "without a record the bullet reads as a live spinner"
    );

    for state in ["idle", "done", "error"] {
        let mut harness = Harness::claude();
        harness.feed(&claude_stale_spinner());
        harness.feed(&claude_record(state));
        harness.feed(&claude_stale_spinner());

        assert_eq!(harness.activity(), IDLE, "{state}");
    }
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

#[test]
fn no_record_means_every_captured_screen_reads_exactly_as_without_the_opt_in() {
    for (runtime, app, readiness) in [
        ("claude", CLAUDE_APP, InputReadiness::ScreenIdle),
        ("codex", CODEX_APP, InputReadiness::ManifestIdle),
        ("pi", "pi", InputReadiness::ScreenIdle),
    ] {
        let directory = workspace_root().join(format!("compat/{runtime}/screens"));
        let mut names = Vec::new();
        for sub in ["", "widths"] {
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
            let mut plain = Harness::new(config(
                &read(&format!("runtime-packages/{runtime}/detect.toml")),
                readiness,
            ));
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
    let popup = captured("codex", "trust");
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
fn a_held_idle_record_suppresses_a_stale_spinner_latch() {
    let mut harness = Harness::claude();
    harness.feed(&captured("claude", "idle"));
    harness.feed(&claude_record("idle"));

    let chunk = [claude_stale_spinner(), captured("claude", "idle")].concat();
    let transitions = harness.feed_raw(&chunk);

    assert!(
        transitions
            .iter()
            .all(|transition| transition.activity != AgentActivity::Working),
        "no latched working over a held idle record: {transitions:?}"
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
fn a_report_is_not_an_event_without_the_opt_in() {
    // A BEL-terminated record splits the screen feed into segments only when the
    // manifest opted in; without it the output is handled as any other output.
    let bytes = [
        captured("codex", "idle"),
        b"\x1b]7501;state=working:app=codex\x07".to_vec(),
    ]
    .concat();
    let mut plain = Harness::new(config(
        &read("runtime-packages/codex/detect.toml"),
        InputReadiness::ManifestIdle,
    ));
    let mut stripped = Harness::new(config(
        &read("runtime-packages/codex/detect.toml"),
        InputReadiness::ManifestIdle,
    ));
    plain.feed(&bytes);
    stripped.feed(&captured("codex", "idle"));

    assert_eq!(plain.transitions, stripped.transitions);
}
