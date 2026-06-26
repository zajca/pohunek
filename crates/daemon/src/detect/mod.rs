//! Synchronous activity detector pipeline.
//!
//! Per `docs/plan-phase-1.md` "State Engine": this module coordinates OSC
//! parsing, VT screen extraction, manifest matching, and a debounced state
//! machine. Session code can own a detector without coupling the pipeline to
//! async runtime concerns.

use std::sync::OnceLock;
use std::time::Instant;

use protocol::{AgentActivity, AgentKind, StateSource};

mod machine;
mod manifest;
mod osc;
mod screen;

pub use machine::{ActivityEvidence, ActivityTransition, DetectionConfig, StateMachine};
pub use manifest::{
    Manifest, ManifestError, ManifestMatch, ManifestRegion, MatchContext, MatcherKind,
};
pub use osc::{OscEvidence, OscParser};
pub use screen::{ScreenRegion, ScreenTracker};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Generic shell activity manifest shipped with the daemon.
///
/// Embedded at compile time so the production detector always has a manifest to
/// drive the screen/manifest layer. It is a trusted, unit-tested constant (not
/// user input), so parsing it with `.expect` at startup is acceptable.
const GENERIC_SHELL_MANIFEST: &str = include_str!("manifests/shell.toml");
const CODEX_MANIFEST: &str = include_str!("manifests/codex.toml");
const CLAUDE_MANIFEST: &str = include_str!("manifests/claude.toml");

#[derive(Debug, Default)]
pub struct DetectorConfig {
    pub detection: DetectionConfig,
    pub manifest: Option<Manifest>,
}

impl DetectorConfig {
    /// Production detector config using the embedded generic shell manifest.
    #[must_use]
    pub fn generic_shell() -> Self {
        Self {
            detection: DetectionConfig::default(),
            manifest: Some(generic_shell_manifest().clone()),
        }
    }

    /// Production detector config for a specific agent kind.
    #[must_use]
    pub fn for_agent(agent: AgentKind) -> Self {
        match agent {
            AgentKind::Shell => Self::generic_shell(),
            AgentKind::Codex => Self::codex(),
            AgentKind::Claude => Self::claude(),
        }
    }

    /// Detector config for a host agent profile (Part C): use the profile's
    /// override manifest when it declares one, else inherit the base kind's
    /// embedded manifest. The override is already parsed via the capped,
    /// non-panicking [`Manifest::parse_str`] (a malformed one disabled the profile
    /// before this point), so detection never `.expect`-panics on host input.
    #[must_use]
    pub fn for_profile(base: AgentKind, override_manifest: Option<Manifest>) -> Self {
        match override_manifest {
            Some(manifest) => Self {
                detection: DetectionConfig::default(),
                manifest: Some(manifest),
            },
            None => Self::for_agent(base),
        }
    }

    /// Production detector config using the embedded Codex manifest.
    #[must_use]
    pub fn codex() -> Self {
        Self {
            detection: DetectionConfig::default(),
            manifest: Some(codex_manifest().clone()),
        }
    }

    /// Production detector config using the embedded Claude Code manifest.
    #[must_use]
    pub fn claude() -> Self {
        Self {
            detection: DetectionConfig::default(),
            manifest: Some(claude_manifest().clone()),
        }
    }
}

/// Parses the embedded generic shell manifest.
///
/// The manifest is a shipped, trusted, unit-tested constant, so a parse failure
/// is a programming error rather than a recoverable condition.
pub fn generic_shell_manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        Manifest::parse_str(GENERIC_SHELL_MANIFEST).expect("generic shell manifest must parse")
    })
}

pub fn codex_manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| Manifest::parse_str(CODEX_MANIFEST).expect("codex manifest must parse"))
}

pub fn claude_manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST
        .get_or_init(|| Manifest::parse_str(CLAUDE_MANIFEST).expect("claude manifest must parse"))
}

#[derive(Debug)]
pub struct Detector {
    osc: OscParser,
    screen: ScreenTracker,
    state: StateMachine,
    manifest: Option<Manifest>,
    latest_title: Option<String>,
    latest_progress: Option<String>,
    process_activity: ProcessActivityScanner,
}

impl Detector {
    #[must_use]
    pub fn new(rows: u16, cols: u16, started_at: Instant, config: DetectorConfig) -> Self {
        Self {
            osc: OscParser::new(),
            screen: ScreenTracker::new(rows, cols),
            state: StateMachine::new(started_at, config.detection),
            manifest: config.manifest,
            latest_title: None,
            latest_progress: None,
            process_activity: ProcessActivityScanner::default(),
        }
    }

    pub fn feed(&mut self, now: Instant, bytes: &[u8]) -> Vec<ActivityTransition> {
        let has_process_activity = self.process_activity.observe(bytes);
        let recent_before = self.screen.recent_text();
        let osc_items = self.osc.advance(bytes);
        self.screen.feed(bytes);
        let screen_changed = self.screen.recent_text() != recent_before;

        let osc_changes = self.collect_osc_evidence(osc_items);
        let mut evidence = Vec::new();
        self.collect_manifest_evidence(
            &mut evidence,
            ContextFreshness {
                screen: screen_changed,
                osc_title: osc_changes.title,
                osc_progress: osc_changes.progress,
            },
        );
        let has_structured_evidence = !evidence.is_empty();

        let mut transitions = Vec::new();
        for item in evidence {
            push_transition(&mut transitions, self.state.observe_evidence(now, item));
        }

        if has_process_activity && !has_structured_evidence {
            push_transition(&mut transitions, self.state.observe_bytes(now));
        }

        transitions
    }

    pub fn tick(&mut self, now: Instant) -> Vec<ActivityTransition> {
        let mut transitions = Vec::new();

        // Working evidence is left to the byte/process path; tick only reconfirms
        // debounced idle/blocked states and refreshes stable visible states.
        if let Some(item) = self.manifest_evidence(ContextFreshness::all()) {
            if item.activity != AgentActivity::Working {
                push_transition(&mut transitions, self.state.observe_evidence(now, item));
            }
        }
        push_transition(&mut transitions, self.state.tick(now));
        transitions
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.screen.resize(rows, cols);
    }

    /// Recovers detector state after dropped PTY output.
    ///
    /// A skipped broadcast chunk may have severed an in-flight escape sequence,
    /// so we discard the OSC parser state, the process-activity scanner state,
    /// the cached OSC title/progress, and the `vt100` screen grid rather than
    /// trust any of them. Fresh state repaints on the next agent refresh.
    pub fn resync_after_lag(&mut self) {
        self.osc.reset();
        self.process_activity.reset();
        self.state.clear_pending();
        self.latest_title = None;
        self.latest_progress = None;
        self.screen.reset();
    }

    #[must_use]
    pub fn latest_title(&self) -> Option<&str> {
        self.latest_title.as_deref()
    }

    #[must_use]
    pub fn latest_progress(&self) -> Option<&str> {
        self.latest_progress.as_deref()
    }

    fn collect_osc_evidence(&mut self, osc_items: Vec<OscEvidence>) -> OscChanges {
        let mut changes = OscChanges::default();

        for item in osc_items {
            match item {
                OscEvidence::Title(title) => {
                    changes.title |= self.latest_title.as_deref() != Some(title.as_str());
                    self.latest_title = Some(title);
                }
                OscEvidence::Progress(progress) => {
                    changes.progress |= self.latest_progress.as_deref() != Some(progress.as_str());
                    self.latest_progress = Some(progress);
                }
            }
        }

        changes
    }

    fn collect_manifest_evidence(
        &self,
        evidence: &mut Vec<ActivityEvidence>,
        freshness: ContextFreshness,
    ) {
        if let Some(item) = self.manifest_evidence(freshness) {
            evidence.push(item);
        }
    }

    fn manifest_evidence(&self, freshness: ContextFreshness) -> Option<ActivityEvidence> {
        let manifest = self.manifest.as_ref()?;
        let matched = manifest.match_context(&self.match_context(manifest, freshness))?;
        let source = manifest_source(&matched.region);
        if matched.activity == AgentActivity::Blocked
            && matched.visible_blocker
            && !is_visible_manifest_source(source)
        {
            return None;
        }

        Some(ActivityEvidence {
            activity: matched.activity,
            source,
        })
    }

    fn match_context(&self, manifest: &Manifest, freshness: ContextFreshness) -> MatchContext {
        let mut context = MatchContext::default();

        for region in manifest.required_regions() {
            if !freshness.includes(&region) {
                continue;
            }

            context = match region {
                ManifestRegion::OscTitle => {
                    if let Some(title) = &self.latest_title {
                        context.with_region_text(ManifestRegion::OscTitle, title.clone())
                    } else {
                        context
                    }
                }
                ManifestRegion::OscProgress => {
                    if let Some(progress) = &self.latest_progress {
                        context.with_region_text(ManifestRegion::OscProgress, progress.clone())
                    } else {
                        context
                    }
                }
                ManifestRegion::WholeRecent => {
                    context.with_region_text(ManifestRegion::WholeRecent, self.screen.recent_text())
                }
                ManifestRegion::BottomLines(count) => context.with_region_text(
                    ManifestRegion::BottomLines(count),
                    region_text(&self.screen.bottom_lines(count)),
                ),
                ManifestRegion::BottomNonEmptyLines(count) => context.with_region_text(
                    ManifestRegion::BottomNonEmptyLines(count),
                    region_text(&self.screen.bottom_non_empty_lines(count)),
                ),
                ManifestRegion::AfterLastPromptMarker => context.with_region_text(
                    ManifestRegion::AfterLastPromptMarker,
                    self.screen.after_last_prompt_marker(),
                ),
                ManifestRegion::PromptBoxBody => context
                    .with_region_text(ManifestRegion::PromptBoxBody, self.screen.prompt_box_body()),
                ManifestRegion::AfterLastHorizontalRule => context.with_region_text(
                    ManifestRegion::AfterLastHorizontalRule,
                    self.screen.after_last_horizontal_rule(),
                ),
            };
        }

        context
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct OscChanges {
    title: bool,
    progress: bool,
}

#[derive(Debug, Clone, Copy)]
struct ContextFreshness {
    screen: bool,
    osc_title: bool,
    osc_progress: bool,
}

impl ContextFreshness {
    fn all() -> Self {
        Self {
            screen: true,
            osc_title: true,
            osc_progress: true,
        }
    }

    fn includes(self, region: &ManifestRegion) -> bool {
        match region {
            ManifestRegion::OscTitle => self.osc_title,
            ManifestRegion::OscProgress => self.osc_progress,
            ManifestRegion::WholeRecent
            | ManifestRegion::BottomLines(_)
            | ManifestRegion::BottomNonEmptyLines(_)
            | ManifestRegion::AfterLastPromptMarker
            | ManifestRegion::PromptBoxBody
            | ManifestRegion::AfterLastHorizontalRule => self.screen,
        }
    }
}

#[derive(Debug, Default)]
struct ProcessActivityScanner {
    state: ProcessActivityState,
}

impl ProcessActivityScanner {
    fn observe(&mut self, bytes: &[u8]) -> bool {
        let mut has_activity = false;

        for &byte in bytes {
            match self.state {
                ProcessActivityState::Ground => match byte {
                    ESC => self.state = ProcessActivityState::Escape,
                    _ => has_activity = true,
                },
                ProcessActivityState::Escape => match byte {
                    b']' => self.state = ProcessActivityState::OscString,
                    ESC => self.state = ProcessActivityState::Escape,
                    _ => {
                        self.state = ProcessActivityState::Ground;
                        has_activity = true;
                    }
                },
                ProcessActivityState::OscString => match byte {
                    BEL => self.state = ProcessActivityState::Ground,
                    ESC => self.state = ProcessActivityState::OscStringEscape,
                    _ => {}
                },
                ProcessActivityState::OscStringEscape => match byte {
                    b'\\' => self.state = ProcessActivityState::Ground,
                    b']' => self.state = ProcessActivityState::OscString,
                    ESC => self.state = ProcessActivityState::OscStringEscape,
                    _ => self.state = ProcessActivityState::Ground,
                },
            }
        }

        has_activity
    }

    fn reset(&mut self) {
        self.state = ProcessActivityState::Ground;
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ProcessActivityState {
    #[default]
    Ground,
    Escape,
    OscString,
    OscStringEscape,
}

fn manifest_source(region: &ManifestRegion) -> StateSource {
    match region {
        ManifestRegion::OscTitle => StateSource::OscTitle,
        ManifestRegion::OscProgress => StateSource::OscProgress,
        ManifestRegion::WholeRecent
        | ManifestRegion::BottomLines(_)
        | ManifestRegion::BottomNonEmptyLines(_)
        | ManifestRegion::AfterLastPromptMarker
        | ManifestRegion::PromptBoxBody
        | ManifestRegion::AfterLastHorizontalRule => StateSource::Screen,
    }
}

fn is_visible_manifest_source(source: StateSource) -> bool {
    matches!(
        source,
        StateSource::OscTitle | StateSource::OscProgress | StateSource::Screen
    )
}

fn region_text(region: &ScreenRegion) -> String {
    region.lines.join("\n")
}

fn push_transition(
    transitions: &mut Vec<ActivityTransition>,
    transition: Option<ActivityTransition>,
) {
    if let Some(transition) = transition {
        transitions.push(transition);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use protocol::{AgentActivity, StateSource};

    use super::{ActivityTransition, DetectionConfig, Detector, DetectorConfig, Manifest};

    fn instant() -> Instant {
        Instant::now()
    }

    /// Detector config used by most tests.
    ///
    /// Loads the same embedded generic shell manifest the production detector
    /// uses, so OSC title/progress and visible-screen keyword detection run
    /// through the manifest layer just like in the live daemon.
    fn config() -> DetectorConfig {
        DetectorConfig {
            detection: DetectionConfig {
                recheck_after: Duration::from_millis(100),
                confirmations: 1,
                cap: Duration::from_millis(700),
                stable_visible_refresh: Duration::from_millis(800),
                startup_grace: Duration::ZERO,
            },
            manifest: Some(super::generic_shell_manifest().clone()),
        }
    }

    fn debounce_config() -> DetectorConfig {
        DetectorConfig {
            detection: DetectionConfig {
                recheck_after: Duration::from_millis(100),
                confirmations: 3,
                cap: Duration::from_millis(700),
                stable_visible_refresh: Duration::from_millis(800),
                startup_grace: Duration::ZERO,
            },
            manifest: Some(super::generic_shell_manifest().clone()),
        }
    }

    fn transition(activity: AgentActivity, source: StateSource) -> ActivityTransition {
        ActivityTransition { activity, source }
    }

    fn manifest(source: &str) -> Manifest {
        Manifest::parse_str(source).expect("manifest should parse")
    }

    #[test]
    fn osc_title_working_split_across_chunks_emits_working_title_transition() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert!(detector.feed(started_at, b"\x1b]2;wor").is_empty());
        assert!(detector
            .feed(started_at + Duration::from_millis(10), b"ki")
            .is_empty());

        assert_eq!(
            detector.feed(started_at + Duration::from_millis(20), b"ng\x07"),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
        assert_eq!(detector.latest_title(), Some("working"));
    }

    #[test]
    fn osc_title_split_between_escape_and_bracket_does_not_emit_process() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert!(detector.feed(started_at, b"\x1b").is_empty());

        assert_eq!(
            detector.feed(started_at + Duration::from_millis(10), b"]2;working\x07"),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
        assert_eq!(detector.latest_title(), Some("working"));
    }

    #[test]
    fn mixed_output_and_partial_osc_emits_process_then_final_title_source() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert_eq!(
            detector.feed(started_at, b"output\x1b]2;blo"),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );
        assert_eq!(
            detector.feed(started_at + Duration::from_millis(10), b"cked\x07"),
            vec![transition(AgentActivity::Blocked, StateSource::OscTitle)]
        );
        assert_eq!(detector.latest_title(), Some("blocked"));
    }

    #[test]
    fn osc_st_split_between_escape_and_backslash_allows_later_process_fallback() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert!(detector.feed(started_at, b"\x1b]2;ready\x1b").is_empty());
        assert_eq!(
            detector.feed(started_at + Duration::from_millis(10), b"\\"),
            vec![transition(AgentActivity::Idle, StateSource::OscTitle)]
        );

        assert_eq!(
            detector.feed(started_at + Duration::from_millis(20), b"\x1b[?25l"),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );
    }

    #[test]
    fn osc_progress_action_required_emits_blocked_progress_transition() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert_eq!(
            detector.feed(started_at, b"\x1b]9;action required\x07"),
            vec![transition(AgentActivity::Blocked, StateSource::OscProgress)]
        );
        assert_eq!(detector.latest_progress(), Some("action required"));
    }

    #[test]
    fn non_empty_control_bytes_without_visible_text_emit_working_process_transition() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert_eq!(
            detector.feed(started_at, b"\x1b[?25l"),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );
    }

    #[test]
    fn manifest_osc_title_rule_emits_osc_title_source() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "title-blocked"
            state = "blocked"
            priority = 1
            region = "osc_title"
            contains = "needs-review"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, b"\x1b]2;needs-review\x07"),
            vec![transition(AgentActivity::Blocked, StateSource::OscTitle)]
        );
    }

    #[test]
    fn manifest_bottom_lines_11_region_can_match() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "bottom-11"
            state = "working"
            priority = 1
            region = "bottom_lines(11)"
            contains = "line-11-target"
            "#,
        ));
        let mut detector = Detector::new(12, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                b"line-01\r\nline-02\r\nline-03\r\nline-04\r\nline-05\r\nline-06\r\nline-07\r\nline-08\r\nline-09\r\nline-10\r\nline-11-target"
            ),
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
    }

    #[test]
    fn after_last_horizontal_rule_region_matches_text_below_the_rule() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "below-rule-blocked"
            state = "blocked"
            priority = 1
            region = "after_last_horizontal_rule"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[Hheader\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\napproval required".as_bytes()
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn after_last_prompt_marker_region_matches_text_below_the_marker() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "below-marker-blocked"
            state = "blocked"
            priority = 1
            region = "after_last_prompt_marker"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[H\u{203a} run tests\r\napproval required".as_bytes()
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn missing_osc_title_region_is_absent_from_manifest_context() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "missing-title"
            state = "blocked"
            priority = 1
            region = "osc_title"
            not = { contains = "working" }
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert!(detector.tick(started_at).is_empty());
        assert_eq!(
            detector.feed(started_at + Duration::from_millis(10), b"plain output"),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );
    }

    #[test]
    fn stale_manifest_context_does_not_suppress_process_fallback() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "visible-blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, b"approval required"),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
        assert_eq!(
            detector.feed(started_at + Duration::from_millis(10), b"\x1b[?25l"),
            vec![transition(AgentActivity::Working, StateSource::Process)]
        );
    }

    #[test]
    fn tick_reconfirms_static_manifest_evidence_for_default_debounce() {
        let started_at = instant();
        let mut detector_config = debounce_config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "visible-blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert!(detector.feed(started_at, b"approval required").is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(100))
            .is_empty());
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(200)),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn tick_prioritizes_manifest_evidence_over_generic_osc_to_confirm_debounce() {
        let started_at = instant();
        let mut detector_config = debounce_config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "screen-blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert!(detector
            .feed(started_at, b"\x1b]2;idle\x07approval required")
            .is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(100))
            .is_empty());
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(200)),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn tick_reconfirms_static_osc_evidence_for_default_debounce() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, debounce_config());

        assert!(detector.feed(started_at, b"\x1b]2;idle\x07").is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(100))
            .is_empty());
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(200)),
            vec![transition(AgentActivity::Idle, StateSource::OscTitle)]
        );
    }

    #[test]
    fn tick_does_not_reemit_cached_working_after_process_activity() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        assert_eq!(
            detector.feed(started_at, b"\x1b]2;working\x07"),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
        // Plain output with no working keyword: the screen manifest layer stays
        // dormant, so the only signal is process activity, which records the
        // published source as Process without re-emitting.
        assert!(detector
            .feed(started_at + Duration::from_millis(100), b"plain log output")
            .is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(900))
            .is_empty());
    }

    #[test]
    fn resync_after_lag_clears_cached_title_and_progress_values() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        let transitions = detector.feed(started_at, b"\x1b]2;working\x07\x1b]9;progress\x07");
        assert_eq!(
            transitions,
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
        assert_eq!(detector.latest_title(), Some("working"));
        assert_eq!(detector.latest_progress(), Some("progress"));

        detector.resync_after_lag();

        assert_eq!(detector.latest_title(), None);
        assert_eq!(detector.latest_progress(), None);
    }

    #[test]
    fn resync_after_lag_clears_visible_screen_so_stale_text_no_longer_matches() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "visible-blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "approval required"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        // The blocker text matched against the visible screen before the lag.
        assert_eq!(
            detector.feed(started_at, b"approval required"),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );

        detector.resync_after_lag();

        // After resync the screen is blank, so a tick finds no visible blocker to
        // reconfirm and emits nothing.
        assert!(detector
            .tick(started_at + Duration::from_millis(100))
            .is_empty());
    }

    #[test]
    fn resync_after_lag_clears_pending_osc_debounce_candidate() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, debounce_config());

        assert!(detector.feed(started_at, b"\x1b]2;idle\x07").is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(100))
            .is_empty());

        detector.resync_after_lag();

        assert!(detector
            .feed(started_at + Duration::from_millis(200), b"\x1b]2;idle\x07")
            .is_empty());
        assert!(detector
            .tick(started_at + Duration::from_millis(300))
            .is_empty());
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(400)),
            vec![transition(AgentActivity::Idle, StateSource::OscTitle)]
        );
    }

    #[test]
    fn manifest_screen_match_wins_with_screen_source_when_visible_text_updates() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "visible-working"
            state = "working"
            priority = 1
            region = "whole_recent"
            contains = "compiling workspace"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, b"Compiling workspace"),
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
    }

    #[test]
    fn tick_delegates_stable_visible_refresh() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "visible-working"
            state = "working"
            priority = 1
            region = "whole_recent"
            contains = "compiling workspace"
            "#,
        ));
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, b"Compiling workspace"),
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
        assert_eq!(
            detector.tick(started_at + Duration::from_millis(800)),
            vec![transition(AgentActivity::Working, StateSource::Screen)]
        );
    }

    #[test]
    fn resize_changes_screen_dimensions_used_by_manifest_context() {
        let started_at = instant();
        let mut detector_config = config();
        detector_config.manifest = Some(manifest(
            r#"
            [[rules]]
            id = "two-line-context"
            state = "blocked"
            priority = 1
            region = "bottom_non_empty_lines(2)"
            contains = ["ready marker", "approval required"]
            "#,
        ));
        let mut detector = Detector::new(1, 80, started_at, detector_config);

        assert!(!detector
            .feed(started_at, b"approval required")
            .contains(&transition(AgentActivity::Blocked, StateSource::Screen)));

        detector.resize(2, 80);

        assert_eq!(
            detector.feed(
                started_at + Duration::from_millis(10),
                b"\x1b[2J\x1b[Hready marker\r\napproval required"
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn embedded_generic_shell_manifest_parses() {
        // The production path relies on this constant parsing; a failure here is
        // a packaging error, not a runtime one. `generic_shell()` exercises the
        // same `.expect`-backed parse the live daemon uses at construction.
        let _ = super::generic_shell_manifest();
        let _ = super::DetectorConfig::generic_shell();
    }

    #[test]
    fn embedded_codex_manifest_parses() {
        let _ = super::codex_manifest();
        let _ = super::DetectorConfig::codex();
    }

    #[test]
    fn embedded_claude_manifest_parses() {
        let _ = super::claude_manifest();
        let _ = super::DetectorConfig::claude();
    }

    #[test]
    fn detector_config_for_agent_loads_agent_manifest() {
        let started_at = instant();
        let mut codex_config = super::DetectorConfig::for_agent(protocol::AgentKind::Codex);
        codex_config.detection = config().detection;
        let mut codex = Detector::new(3, 80, started_at, codex_config);
        assert_eq!(
            codex.feed(started_at, b"\x1b]2;Action Required\x07"),
            vec![transition(AgentActivity::Blocked, StateSource::OscTitle)]
        );

        let mut claude_config = super::DetectorConfig::for_agent(protocol::AgentKind::Claude);
        claude_config.detection = config().detection;
        let mut claude = Detector::new(3, 80, started_at, claude_config);
        assert_eq!(
            claude.feed(started_at, "\x1b]2;\u{280b} thinking\x07".as_bytes()),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
    }

    #[test]
    fn detector_config_for_profile_uses_override_manifest_when_present() {
        let override_manifest = manifest(
            r#"
            [[rules]]
            id = "custom-blocked"
            state = "blocked"
            priority = 1
            region = "whole_recent"
            contains = "custom blocker"
            "#,
        );

        let mut config =
            super::DetectorConfig::for_profile(protocol::AgentKind::Codex, Some(override_manifest));
        config.detection = self::config().detection;
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config);

        assert_eq!(
            detector.feed(started_at, b"custom blocker\r\n"),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn detector_config_for_profile_inherits_base_manifest_without_override() {
        // With no override, a profile inherits its base kind's manifest. The
        // detector config holds an owned manifest (not the &'static), so assert by
        // behavior: feeding a Claude blocking pattern yields the same blocked
        // transition the base Claude config produces.
        let mut profile_config =
            super::DetectorConfig::for_profile(protocol::AgentKind::Claude, None);
        profile_config.detection = self::config().detection;
        let mut base_config = super::DetectorConfig::claude();
        base_config.detection = self::config().detection;
        assert!(
            profile_config.manifest.is_some(),
            "a profile without an override must inherit a base manifest"
        );

        let blocked = "enter to select\nesc to cancel\n↑/↓ to navigate";
        let started_at = instant();
        let mut from_profile = Detector::new(6, 80, started_at, profile_config);
        let mut from_base = Detector::new(6, 80, started_at, base_config);
        assert_eq!(
            from_profile.feed(started_at, blocked.as_bytes()),
            from_base.feed(started_at, blocked.as_bytes()),
            "inherited manifest must detect exactly like the base Claude config"
        );
    }

    #[test]
    fn generic_shell_manifest_maps_osc_title_working_and_screen_action_required() {
        // Uses `config()`, which loads the SAME embedded manifest the production
        // detector uses, with fast test timings (immediate confirmation, no
        // startup grace) so the debounced blocked transition publishes at once.
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        // OSC title "working" resolves through the manifest to Working/OscTitle.
        assert_eq!(
            detector.feed(started_at, b"\x1b]2;working\x07"),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );

        // A visible "action required" on the screen resolves to Blocked via the
        // whole_recent rule, with a Screen source.
        assert_eq!(
            detector.feed(
                started_at + Duration::from_millis(10),
                b"\x1b[2J\x1b[Haction required"
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn codex_manifest_maps_action_required_title_to_blocked() {
        let started_at = instant();
        let mut detector_config = super::DetectorConfig::codex();
        detector_config.detection = config().detection;
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, b"\x1b]2;Action Required\x07"),
            vec![transition(AgentActivity::Blocked, StateSource::OscTitle)]
        );
    }

    #[test]
    fn codex_manifest_maps_braille_title_spinner_to_working() {
        let started_at = instant();
        let mut detector_config = super::DetectorConfig::codex();
        detector_config.detection = config().detection;
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, "\x1b]2;\u{280b} thinking\x07".as_bytes()),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
    }

    #[test]
    fn claude_manifest_maps_ink_selection_form_to_blocked() {
        let started_at = instant();
        let mut detector_config = super::DetectorConfig::claude();
        detector_config.detection = config().detection;
        let mut detector = Detector::new(8, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(
                started_at,
                "\x1b[2J\x1b[HReview\r\n\u{2500}\u{2500}\u{2500}\u{2500}\r\nenter to select\r\nesc to cancel\r\n\u{2191}/\u{2193} to navigate".as_bytes(),
            ),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }

    #[test]
    fn claude_manifest_maps_braille_title_spinner_to_working() {
        let started_at = instant();
        let mut detector_config = super::DetectorConfig::claude();
        detector_config.detection = config().detection;
        let mut detector = Detector::new(3, 80, started_at, detector_config);

        assert_eq!(
            detector.feed(started_at, "\x1b]2;\u{280b} thinking\x07".as_bytes()),
            vec![transition(AgentActivity::Working, StateSource::OscTitle)]
        );
    }

    #[test]
    fn generic_shell_manifest_blocked_outranks_working_within_one_screen() {
        let started_at = instant();
        let mut detector = Detector::new(3, 80, started_at, config());

        // "working" and "action required" both appear; blocked rules carry the
        // higher priority, so the resolved activity is Blocked.
        assert_eq!(
            detector.feed(started_at, b"\x1b[2J\x1b[Hworking: action required"),
            vec![transition(AgentActivity::Blocked, StateSource::Screen)]
        );
    }
}
