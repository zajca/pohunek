//! The official Claude Code runtime package (`runtime-packages/claude`).
//!
//! Pure tests always run: they build the package directory, parse it through
//! the same path `plugin install` uses, compare it with the built-in Claude
//! descriptor and detection manifest, and check it against the compatibility
//! lock and the screens captured from a real Claude Code
//! (`compat/claude/screens`). The daemon-backed tests install the built archive
//! through `pohunek plugin install --catalog` against a throwaway signing key
//! and trust anchor, so the package serves `claude` with official trust. The
//! real-Claude tests additionally drive an actual `claude` binary against a
//! loopback Messages stub with a throwaway home; see their documentation for
//! the opt-in.

// Rust guideline compliant 2026-10-06

#![cfg(unix)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use package::directory::build_directory_archive;
use package::{read_archive, CatalogEntry, Limits, PackageDigest};
use pohunek_daemon::agent::host::{
    definition_from_archive, BuiltinSource, HandlerId, LaunchProgram, ProbeVersion,
    RuntimeDefinition, RuntimeSource,
};
use pohunek_daemon::agent::{NativeReferenceStrategy, SessionRef};
use pohunek_daemon::catalog_anchor::CatalogTrust;
use pohunek_daemon::detect::{
    ActivityTransition, DetectionConfig, Detector, DetectorConfig, Manifest,
};
use pohunek_daemon::procwatch::{ProcessFact, StartIdentity};
use pohunek_daemon::session::HostTrustAnchor;
use pohunek_test_support::fs::write_executable;
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::workspace_root;
use protocol::{
    AgentActivity, BindingProvenance, PackageId, PackageVersion, RuntimeId, StateSource,
};
use serde_json::Value;

#[path = "support/catalog_fixture.rs"]
mod catalog_fixture;
#[path = "support/messages_stub.rs"]
mod messages_stub;
#[path = "support/plugin_harness.rs"]
mod plugin_harness;
#[path = "support/process_guard.rs"]
mod process_guard;

use catalog_fixture::{
    catalog_of, host_platform, root_of, signed, test_key, ANY_CORE, WINDOW_END, WINDOW_START,
};
use messages_stub::MessagesStub;
use plugin_harness::{path_str, Harness};
use process_guard::ProcessGuard;

/// Directory of the package source, relative to the workspace root.
const PACKAGE_DIR: &str = "runtime-packages/claude";

/// Compatibility lock of the package, relative to the workspace root.
const LOCK_PATH: &str = "compat/claude/compatibility-lock.json";

/// Captured screens, relative to the workspace root.
const SCREENS_DIR: &str = "compat/claude/screens";

/// Built-in Claude descriptor, relative to the workspace root.
const BUILTIN_DESCRIPTOR: &str = "crates/daemon/src/agent/builtin/claude.toml";

/// Built-in Claude detection manifest, relative to the workspace root.
const BUILTIN_MANIFEST: &str = "crates/daemon/src/detect/manifests/claude.toml";

/// Package id the descriptor declares.
const PACKAGE_ID: &str = "pohunek.runtime.claude";

/// Version of the package the descriptor declares.
const PACKAGE_VERSION: &str = "1.0.0";

/// Shell program of the built-in source the parity tests load; Claude does not
/// use it.
const BUILTIN_SHELL: &str = "/bin/sh";

fn package_dir() -> PathBuf {
    workspace_root().join(PACKAGE_DIR)
}

/// The canonical archive of the package directory and its digest.
fn built_archive() -> (Vec<u8>, PackageDigest) {
    let bytes = build_directory_archive(&package_dir(), &Limits::DEFAULT)
        .expect("the package directory builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    (bytes, digest)
}

/// The definition `plugin install` derives from the package directory.
fn installed_definition() -> RuntimeDefinition {
    let (bytes, digest) = built_archive();
    let archive = read_archive(&bytes, &Limits::DEFAULT).expect("the archive reads");
    definition_from_archive(archive.entries(), &digest)
        .expect("the daemon accepts the package descriptor")
}

/// The Claude definition compiled into the daemon.
fn builtin_definition() -> RuntimeDefinition {
    BuiltinSource::new(BUILTIN_SHELL)
        .load()
        .expect("the built-in definitions load")
        .into_iter()
        .find(|definition| definition.runtime_id().as_str() == "claude")
        .expect("the daemon embeds a Claude definition")
}

fn read_lock() -> Value {
    let text = fs::read_to_string(workspace_root().join(LOCK_PATH)).expect("read the lock");
    serde_json::from_str(&text).expect("the lock is JSON")
}

/// The release the compatibility lock pins.
fn locked_release() -> String {
    read_lock()["upstream"]["release"]
        .as_str()
        .expect("lock release")
        .to_owned()
}

/// The banner `claude --version` prints for `release`.
fn banner_of(release: &str) -> String {
    format!("{release} (Claude Code)")
}

fn read_toml(relative: &str) -> toml::Table {
    let text = fs::read_to_string(workspace_root().join(relative)).expect("read the TOML file");
    text.parse().expect("the file is TOML")
}

/// A screen the way the terminal tracker receives it: an optional OSC title,
/// then the cleared screen and its rows.
fn frame_bytes(title: Option<&str>, rows: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(title) = title {
        bytes.extend_from_slice(format!("\x1b]2;{title}\x07").as_bytes());
    }
    bytes.extend_from_slice(b"\x1b[2J\x1b[H");
    bytes.extend_from_slice(rows.join("\r\n").as_bytes());
    bytes
}

fn detector_for(manifest: &Manifest, now: Instant, columns: u16) -> Detector {
    Detector::new(
        24,
        columns,
        now,
        DetectorConfig {
            detection: DetectionConfig {
                recheck_after: Duration::from_millis(100),
                confirmations: 1,
                cap: Duration::from_millis(700),
                stable_visible_refresh: Duration::from_millis(800),
                startup_grace: Duration::ZERO,
            },
            manifest: Some(manifest.clone()),
        },
    )
}

/// Every transition a fresh detector with `definition`'s manifest emits for one
/// frame.
fn transitions(
    definition: &RuntimeDefinition,
    title: Option<&str>,
    rows: &[String],
    columns: u16,
) -> Vec<ActivityTransition> {
    let started = Instant::now();
    let mut detector = detector_for(definition.manifest(), started, columns);
    detector.feed(started, &frame_bytes(title, rows))
}

/// The activity the manifest reads from the screen text alone, when the screen
/// speaks at all.
fn screen_activity(
    definition: &RuntimeDefinition,
    rows: &[String],
    columns: u16,
) -> Option<AgentActivity> {
    transitions(definition, None, rows, columns)
        .iter()
        .rfind(|transition| transition.source == StateSource::Screen)
        .map(|transition| transition.activity)
}

/// The activity the manifest reads from the terminal title alone.
fn title_activity(definition: &RuntimeDefinition, title: &str) -> Option<AgentActivity> {
    let started = Instant::now();
    let mut detector = detector_for(definition.manifest(), started, 80);
    detector
        .feed(started, format!("\x1b]2;{title}\x07").as_bytes())
        .iter()
        .rfind(|transition| transition.source == StateSource::OscTitle)
        .map(|transition| transition.activity)
}

fn rows_of(text: &str) -> Vec<String> {
    text.trim_end_matches('\n')
        .split('\n')
        .map(str::to_owned)
        .collect()
}

/// What a captured screen means.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Meaning {
    Idle,
    Working,
    Blocked,
    /// A screen no rule speaks for: only the byte-activity fallback applies.
    Silent,
}

impl Meaning {
    fn of(state: &str) -> Self {
        match state {
            "idle" | "idle_after_turn" => Self::Idle,
            "working_early" | "working_stream" => Self::Working,
            "approval" | "askuser" | "apikey" => Self::Blocked,
            "theme" | "login" | "trust" => Self::Silent,
            other => panic!("unknown fixture state {other}"),
        }
    }

    fn activity(self) -> Option<AgentActivity> {
        match self {
            Self::Idle => Some(AgentActivity::Idle),
            Self::Working => Some(AgentActivity::Working),
            Self::Blocked => Some(AgentActivity::Blocked),
            Self::Silent => None,
        }
    }
}

/// One screen captured from a real Claude Code: `<state>.txt` at 100 columns, or
/// `widths/<state>_w<columns>.txt`, with the OSC title Claude had set in
/// `<same name>.title` when the capture had one.
struct Frame {
    name: String,
    state: String,
    columns: u16,
    rows: Vec<String>,
    title: Option<String>,
}

impl Frame {
    fn meaning(&self) -> Meaning {
        Meaning::of(&self.state)
    }

    fn transitions(&self, definition: &RuntimeDefinition) -> Vec<ActivityTransition> {
        transitions(definition, self.title.as_deref(), &self.rows, self.columns)
    }

    /// The activity of the last transition whose source is the title or the
    /// screen, i.e. everything but the byte-activity fallback.
    fn evidence(&self, definition: &RuntimeDefinition) -> Option<(AgentActivity, StateSource)> {
        self.transitions(definition)
            .iter()
            .rfind(|transition| transition.source != StateSource::Process)
            .map(|transition| (transition.activity, transition.source))
    }
}

fn load_frame(directory: &Path, name: &str, state: &str, columns: u16) -> Frame {
    let rows = rows_of(
        &fs::read_to_string(directory.join(format!("{name}.txt"))).expect("read the screen"),
    );
    let title = fs::read_to_string(directory.join(format!("{name}.title")))
        .ok()
        .map(|title| title.trim_end_matches('\n').to_owned());
    Frame {
        name: name.to_owned(),
        state: state.to_owned(),
        columns,
        rows,
        title,
    }
}

/// Names (without extension) of the `.txt` fixtures in `directory`, sorted.
fn fixture_names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .expect("read the fixture directory")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .into_string()
                .expect("UTF-8 file name")
        })
        .filter_map(|name| name.strip_suffix(".txt").map(str::to_owned))
        .collect();
    names.sort();
    names
}

/// The screens captured at 100 columns, one per state.
fn top_level_frames() -> Vec<Frame> {
    let directory = workspace_root().join(SCREENS_DIR);
    fixture_names(&directory)
        .into_iter()
        .map(|name| load_frame(&directory, &name, &name, DEFAULT_COLUMNS))
        .collect()
}

/// Every screen captured at a given terminal width. The file name is
/// `<state>_w<columns>.txt`.
fn width_frames() -> Vec<Frame> {
    let directory = workspace_root().join(SCREENS_DIR).join("widths");
    fixture_names(&directory)
        .into_iter()
        .map(|name| {
            let (state, columns) = name.rsplit_once("_w").expect("a `_w<columns>` suffix");
            let columns = columns.parse().expect("a column count");
            let state = state.to_owned();
            load_frame(&directory, &name, &state, columns)
        })
        .collect()
}

#[test]
fn the_package_directory_builds_reproducibly_with_only_its_two_files() {
    let (first, digest) = built_archive();
    let (second, again) = built_archive();
    assert_eq!(first, second, "two builds produce the same bytes");
    assert_eq!(digest, again);
    let archive = read_archive(&first, &Limits::DEFAULT).expect("the archive reads");
    let paths: Vec<&str> = archive
        .entries()
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(
        paths,
        ["detect.toml", "runtime.toml"],
        "the archive holds only descriptor and manifest; the hook reporter stays core-owned and evidence lives in compat/claude"
    );
}

/// Optional variable naming an archive built by `cargo xtask package build`.
///
/// The CI job sets it; the test then requires that archive and the in-process
/// build of the same directory to carry the same digest.
const ARCHIVE_VARIABLE: &str = "POHUNEK_CLAUDE_PACKAGE_ARCHIVE";

#[test]
fn the_xtask_build_has_the_digest_of_the_in_process_build() {
    let Some(path) = std::env::var_os(ARCHIVE_VARIABLE) else {
        return;
    };
    let supplied = fs::read(&path).expect("read the supplied archive");
    let supplied_digest = read_archive(&supplied, &Limits::DEFAULT)
        .expect("the supplied archive reads")
        .digest()
        .clone();
    let (_bytes, digest) = built_archive();
    assert_eq!(
        supplied_digest, digest,
        "`cargo xtask package build` and the in-process build of {PACKAGE_DIR} differ"
    );
}

#[test]
fn the_descriptor_declares_the_verified_claude_launch_contract() {
    let definition = installed_definition();
    assert_eq!(definition.runtime_id().as_str(), "claude");
    assert_eq!(definition.display_name(), "Claude Code");
    assert_eq!(
        definition.program(),
        &LaunchProgram::Fixed("claude".to_owned())
    );
    assert!(definition.default_args().is_empty());
    assert!(
        definition.prompt_arg(),
        "the prompt is a trailing positional argument"
    );
    match &definition.binding().provenance {
        BindingProvenance::Package { package, .. } => {
            assert_eq!(package.id.as_str(), PACKAGE_ID);
            assert_eq!(package.version.as_str(), PACKAGE_VERSION);
        }
        other @ BindingProvenance::Builtin { .. } => {
            panic!("a package-served runtime, got {other:?}")
        }
    }

    let rules = definition.input_rules();
    assert!(
        !rules.bracketed_paste,
        "Claude's Ink input takes text without bracketed paste"
    );
    assert_eq!(rules.submit_delay, Duration::from_millis(150));
    assert!(
        definition.submit_delay_configurable(),
        "a daemon-configured submit delay replaces the descriptor's"
    );

    assert_eq!(
        definition.native_reference_strategy(),
        NativeReferenceStrategy::Hook,
        "the SessionStart hook reports the conversation id"
    );
    let native = definition.native().expect("native recovery is declared");
    assert!(native.assigned().is_none(), "core assigns no reference");
    let reference = SessionRef::id("0197aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee").expect("reference");
    assert_eq!(
        native.resume_argv(&reference).expect("resume"),
        ["--resume", reference.value()]
    );
    assert!(native.supports_fork(), "Claude sessions are forkable");
    assert_eq!(
        native.fork_argv(&reference).expect("fork"),
        ["--resume", reference.value(), "--fork-session"]
    );

    assert_eq!(
        definition.integration_handler().map(HandlerId::as_str),
        Some("claude-hook-v1")
    );
    assert_eq!(
        definition.hook_schema().map(|schema| schema.id),
        Some("identity-subagent-v1")
    );
    let home = definition.config_home().expect("a config home is declared");
    assert_eq!(home.env(), "CLAUDE_CONFIG_DIR");
    assert_eq!(home.default_relative(), ".claude");
}

/// The package keeps every fact the built-in Claude descriptor carries; only
/// the manifest source and the version probe differ.
#[test]
fn the_package_descriptor_preserves_every_builtin_claude_fact() {
    let package = installed_definition();
    let builtin = builtin_definition();

    assert_eq!(package.runtime_id(), builtin.runtime_id());
    assert_eq!(package.display_name(), builtin.display_name());
    assert_eq!(package.program(), builtin.program());
    assert_eq!(package.default_args(), builtin.default_args());
    assert_eq!(package.input_rules(), builtin.input_rules());
    assert_eq!(
        package.submit_delay_configurable(),
        builtin.submit_delay_configurable()
    );
    assert_eq!(package.prompt_arg(), builtin.prompt_arg());
    assert_eq!(package.native(), builtin.native());
    assert_eq!(
        package.native_reference_strategy(),
        builtin.native_reference_strategy()
    );
    assert_eq!(package.integration_handler(), builtin.integration_handler());
    assert_eq!(package.hook_schema(), builtin.hook_schema());
    assert_eq!(package.config_home(), builtin.config_home());

    assert!(
        builtin.version_probe_policy().is_none(),
        "the built-in declares no probe"
    );
    assert!(package.version_probe_policy().is_some());
}

/// The same facts as data: the two descriptor files are equal tables once the
/// manifest reference and the version probe, the two fields that differ by
/// design, are removed.
#[test]
fn the_package_descriptor_file_equals_the_builtin_descriptor_file() {
    let strip = |mut table: toml::Table| {
        let runtime = table
            .get_mut("runtime")
            .and_then(toml::Value::as_table_mut)
            .expect("a [runtime] table");
        assert!(runtime.remove("detect_manifest").is_some());
        runtime.remove("version_probe");
        table
    };
    let package = strip(read_toml(&format!("{PACKAGE_DIR}/runtime.toml")));
    let builtin = strip(read_toml(BUILTIN_DESCRIPTOR));
    assert_eq!(package, builtin);
    for section in [
        "input",
        "resume",
        "fork",
        "native_reference",
        "integration",
        "config_home",
    ] {
        assert!(package.contains_key(section), "[{section}] is declared");
    }
}

/// The manifest file is the built-in manifest: same process matchers, same
/// rules in the same order with the same fields.
#[test]
fn the_package_manifest_file_equals_the_builtin_manifest_file() {
    let package = read_toml(&format!("{PACKAGE_DIR}/detect.toml"));
    let builtin = read_toml(BUILTIN_MANIFEST);
    assert_eq!(package, builtin);
    let ids: Vec<&str> = package["rules"]
        .as_array()
        .expect("rules")
        .iter()
        .map(|rule| rule["id"].as_str().expect("rule id"))
        .collect();
    assert_eq!(
        ids,
        [
            "osc_title_working",
            "live_status_spinner",
            "live_blocked_form",
            "dynamic_workflow_prompt",
            "live_prompt_box",
            "completed_turn_status",
            "bash_permission_prompt",
            "generic_permission_prompt",
            "legacy_no_prompt_blocker",
            "osc_title_idle",
            "osc_progress_idle",
        ]
    );
}

#[test]
fn the_supported_range_equals_the_compatibility_lock() {
    let definition = installed_definition();
    let policy = definition
        .version_probe_policy()
        .expect("the descriptor declares a version probe");
    assert_eq!(
        definition.version_probe_parser().map(HandlerId::as_str),
        Some("semver-line-v1")
    );
    assert!(policy.has_line_template());
    assert_eq!(policy.args(), ["--version"]);

    let lock = read_lock();
    assert_eq!(lock["package"], PACKAGE_ID);
    assert_eq!(lock["runtime"], definition.runtime_id().as_str());
    assert_eq!(lock["supported"]["min"], policy.min().to_string());
    assert_eq!(lock["supported"]["below"], policy.below().to_string());

    let release =
        ProbeVersion::parse(&locked_release()).expect("the locked release is MAJOR.MINOR.PATCH");
    assert_eq!(
        release,
        policy.min(),
        "the range starts at the one release that was verified"
    );
    assert!(
        policy.supports(release),
        "the locked release lies in the supported range"
    );
    assert!(
        !policy.supports(policy.below()),
        "the first release above the range is unsupported"
    );
}

#[test]
fn the_version_probe_reads_the_claude_banner_and_nothing_else() {
    let definition = installed_definition();
    let policy = definition.version_probe_policy().expect("a version probe");
    let supported = |output: &str| {
        policy
            .read_output(output)
            .map(|version| policy.supports(version))
    };

    // The banner `claude --version` prints on stdout.
    assert_eq!(supported("2.1.289 (Claude Code)\n"), Some(true));
    assert_eq!(supported("2.1.300 (Claude Code)"), Some(true));
    assert_eq!(
        supported("2.1.288 (Claude Code)"),
        Some(false),
        "below the verified release"
    );
    assert_eq!(
        supported("2.2.0 (Claude Code)"),
        Some(false),
        "the next minor is outside the range"
    );
    assert_eq!(supported("3.0.0 (Claude Code)"), Some(false));
    assert_eq!(supported("1.0.0 (Claude Code)"), Some(false));

    for unreadable in [
        "",
        "2.1.289",
        "Claude Code 2.1.289",
        "2.1.289 (Claude Code) extra",
        "2.1.289-rc.1 (Claude Code)",
        "2.1.289 (Claude)",
        "2.1 (Claude Code)",
        "v2.1.289 (Claude Code)",
        "claude 2.1.289 (Claude Code)",
        "WARNING: proceeding\n2.1.289 (Claude Code)",
    ] {
        assert_eq!(supported(unreadable), None, "{unreadable:?}");
    }
}

/// The activity the manifest reads from a screen that is drawn after `title`
/// was set, the way the daemon sees a live session.
fn live_activity(
    definition: &RuntimeDefinition,
    title: &str,
    rows: &[String],
    columns: u16,
) -> Option<AgentActivity> {
    transitions(definition, Some(title), rows, columns)
        .iter()
        .rfind(|transition| transition.source != StateSource::Process)
        .map(|transition| transition.activity)
}

#[test]
fn the_top_level_screens_cover_every_state() {
    let states: BTreeSet<String> = top_level_frames()
        .into_iter()
        .map(|frame| frame.state)
        .collect();
    assert_eq!(
        states.iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "apikey",
            "approval",
            "askuser",
            "idle",
            "idle_after_turn",
            "login",
            "theme",
            "trust",
            "working_early",
            "working_stream",
        ]
    );
}

/// What the package manifest reads from every captured screen, with the screen
/// and the title Claude set. The screen text carries the idle, working and
/// blocked evidence; the exceptions are the pinned gaps of the tests below.
#[test]
fn every_captured_screen_is_classified_by_its_screen_text() {
    let definition = installed_definition();
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        let expected = match (frame.meaning(), frame.state.as_str(), frame.columns) {
            // Pinned gaps: see the tests below.
            (Meaning::Working, "working_stream", _) => {
                Some((AgentActivity::Idle, StateSource::Screen))
            }
            (Meaning::Blocked, "askuser", columns) if QUESTION_UNREAD_WIDTHS.contains(&columns) => {
                Some((AgentActivity::Idle, StateSource::Screen))
            }
            (meaning, _, _) => meaning
                .activity()
                .map(|activity| (activity, StateSource::Screen)),
        };
        assert_eq!(frame.evidence(&definition), expected, "{}", frame.name);
    }
}

/// Captured widths at which the question form's footer wraps between its hints
/// ("Enter to select · ↑/↓ to" / "navigate · Esc to cancel"), so no rule
/// matches and the screen reads as the idle prompt box. At 20 columns every hint
/// has its own line and the rules match again.
const QUESTION_UNREAD_WIDTHS: [u16; 2] = [30, 40];

#[test]
fn the_captured_screens_span_the_supported_terminal_widths() {
    let frames = width_frames();
    let widths: BTreeSet<u16> = frames.iter().map(|frame| frame.columns).collect();
    assert_eq!(
        widths.into_iter().collect::<Vec<_>>(),
        [20, 30, 40, 60, 80, 100, 120, 160, 200]
    );
    for state in [
        "apikey",
        "approval",
        "askuser",
        "idle",
        "idle_after_turn",
        "trust",
        "working_stream",
    ] {
        let count = frames.iter().filter(|frame| frame.state == state).count();
        assert_eq!(count, 9, "{state} is captured at every width");
    }
    let titled = frames.iter().filter(|frame| frame.title.is_some()).count();
    assert!(titled >= 40, "titles captured with the screens: {titled}");
    assert!(
        frames
            .iter()
            .filter(|frame| frame.meaning() == Meaning::Silent || frame.state == "apikey")
            .all(|frame| frame.title.is_none()),
        "no title was emitted for the first-run and trust dialogs"
    );
}

/// Claude Code 2.1.289 sets `✳ Claude Code` while it waits for input, also
/// while a tool approval or question is on screen, and a rotating
/// circle-quadrant glyph (`◐ ◑ …`) while a turn runs.
#[test]
fn the_captured_titles_are_the_idle_asterisk_and_the_working_circle() {
    let titles: BTreeSet<String> = top_level_frames()
        .into_iter()
        .chain(width_frames())
        .filter_map(|frame| frame.title)
        .collect();
    let glyphs: BTreeSet<char> = titles
        .iter()
        .map(|title| title.chars().next().expect("a non-empty title"))
        .collect();
    assert_eq!(
        glyphs.into_iter().collect::<Vec<_>>(),
        ['\u{25d0}', '\u{25d1}', '\u{2733}']
    );
    assert!(titles.iter().all(|title| title.ends_with(" Claude Code")));
}

/// The title alone reads idle for the asterisk and, because the manifest's
/// working-title rule names braille spinners only, nothing for the circle
/// glyphs of 2.1.289. The working state of 2.1.289 is read from the status
/// line on screen instead; this test pins the gap so a rule change that
/// closes it must update the expectations.
#[test]
fn the_idle_title_is_a_rule_and_the_working_circle_title_is_not() {
    let definition = installed_definition();
    for (title, expected) in [
        ("\u{2733} Claude Code", Some(AgentActivity::Idle)),
        ("\u{280b} Claude Code", Some(AgentActivity::Working)),
        ("\u{25d0} Claude Code", None),
        ("\u{25d1} Claude Code", None),
        ("\u{25d3} Claude Code", None),
        ("Claude Code", None),
        ("", None),
    ] {
        assert_eq!(title_activity(&definition, title), expected, "{title:?}");
    }
}

/// The streaming turn of 2.1.289 shows its reply above the prompt box with no
/// status line, so the screen reads idle (the prompt box) while the title says a
/// turn is running. The daemon reports idle for such a frame; this test pins the
/// gap between the screen rules and the circle title.
#[test]
fn a_streaming_reply_without_a_status_line_reads_idle_from_the_screen() {
    let definition = installed_definition();
    for frame in top_level_frames()
        .into_iter()
        .chain(width_frames())
        .filter(|frame| frame.state == "working_stream")
    {
        let title = frame.title.as_deref().expect("a working title");
        assert!(title.starts_with(['\u{25d0}', '\u{25d1}']), "{title}");
        assert_eq!(
            live_activity(&definition, title, &frame.rows, frame.columns),
            Some(AgentActivity::Idle),
            "{}",
            frame.name
        );
    }
}

/// The glyphs that lead Claude's animated status line.
const STATUS_GLYPHS: [char; 7] = [
    '\u{b7}', '\u{2722}', '\u{2733}', '\u{2736}', '\u{273b}', '\u{273d}', '*',
];

/// While a turn runs, the status line (`✶ Billowing… (0s · ↓ 2 tokens)`) is
/// what makes the screen working, outranking the visible prompt box.
#[test]
fn the_status_spinner_line_makes_a_running_turn_working() {
    let definition = installed_definition();
    let early = load_frame(
        &workspace_root().join(SCREENS_DIR),
        "working_early",
        "working_early",
        DEFAULT_COLUMNS,
    );
    assert!(
        early
            .rows
            .iter()
            .any(|row| row.starts_with(STATUS_GLYPHS) && row.contains("\u{2026} (")),
        "the capture shows a status line: {:#?}",
        early.rows
    );
    assert!(
        early
            .rows
            .iter()
            .any(|row| row.trim_start().starts_with('\u{276f}')),
        "the prompt box stays visible while the turn runs"
    );
    assert_eq!(
        screen_activity(&definition, &early.rows, early.columns),
        Some(AgentActivity::Working)
    );
    let mut without = early.rows.clone();
    without.retain(|row| !row.contains('\u{2026}'));
    assert_eq!(
        screen_activity(&definition, &without, early.columns),
        Some(AgentActivity::Idle),
        "without the status line the visible prompt box reads idle"
    );
}

/// Blocking dialogs win over the idle title the real Claude keeps while they
/// are open.
#[test]
fn a_tool_approval_or_question_is_blocked_although_the_title_says_idle() {
    let definition = installed_definition();
    let mut checked = 0_usize;
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        let reads_blocked = frame.state == "approval"
            || (frame.state == "askuser" && !QUESTION_UNREAD_WIDTHS.contains(&frame.columns));
        if !reads_blocked {
            continue;
        }
        let title = frame.title.as_deref().expect("a title");
        assert_eq!(title, "\u{2733} Claude Code");
        assert_eq!(
            live_activity(&definition, title, &frame.rows, frame.columns),
            Some(AgentActivity::Blocked),
            "{}",
            frame.name
        );
        checked += 1;
    }
    assert!(checked >= 15, "approval and question screens: {checked}");
}

/// The first-run API-key approval is a blocked screen at every width; the theme
/// picker, the login method list and the folder trust dialog are not
/// classified. Pinned so a rule change that closes the gap updates the
/// fixtures.
#[test]
fn the_first_run_and_trust_dialogs_are_pinned_as_the_manifest_reads_them() {
    let definition = installed_definition();
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        let expected = match frame.state.as_str() {
            "apikey" => Some(AgentActivity::Blocked),
            "theme" | "login" | "trust" => None,
            _ => continue,
        };
        assert_eq!(
            screen_activity(&definition, &frame.rows, frame.columns),
            expected,
            "{}",
            frame.name
        );
    }
    let trust = load_frame(
        &workspace_root().join(SCREENS_DIR),
        "trust",
        "trust",
        DEFAULT_COLUMNS,
    );
    assert!(
        trust
            .rows
            .iter()
            .any(|row| row.contains("Yes, I trust this folder")),
        "the trust capture shows the real dialog"
    );
}

fn dialog(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_owned()).collect()
}

#[test]
fn the_permission_rules_read_the_options_of_a_visible_dialog() {
    let definition = installed_definition();
    let bash = [
        "Bash command",
        "touch approved.txt",
        "Do you want to proceed?",
        "\u{276f} 1. Yes",
        "  2. No",
    ];
    assert_eq!(
        screen_activity(&definition, &dialog(&bash), 80),
        Some(AgentActivity::Blocked)
    );
    let generic = [
        "Do you want to proceed?",
        "\u{276f} 1. Yes",
        "  3. No",
        "Esc to cancel",
    ];
    assert_eq!(
        screen_activity(&definition, &dialog(&generic), 80),
        Some(AgentActivity::Blocked)
    );
    // The question alone, without its options, is prose.
    assert_eq!(
        screen_activity(&definition, &dialog(&["Do you want to proceed?"]), 80),
        None
    );
    let form = [
        "Review",
        "\u{2500}\u{2500}\u{2500}\u{2500}",
        "enter to select",
        "esc to cancel",
        "\u{2191}/\u{2193} to navigate",
    ];
    assert_eq!(
        screen_activity(&definition, &dialog(&form), 80),
        Some(AgentActivity::Blocked)
    );
}

#[test]
fn the_completed_turn_status_line_needs_no_trailing_text() {
    let definition = installed_definition();
    let prompt = [
        "\u{2500}\u{2500}\u{2500}\u{2500}",
        "\u{276f} ",
        "\u{2500}\u{2500}\u{2500}\u{2500}",
    ];
    let with_status = |status: &str| {
        let mut rows = vec![status.to_owned()];
        rows.extend(dialog(&prompt));
        rows
    };
    // Both read idle through the prompt box; the completed-turn rule is the
    // precise one, and 2.1.289 appends ` · done <time>` which it does not
    // match.
    assert_eq!(
        screen_activity(&definition, &with_status("\u{273b} Cogitated for 26s"), 80),
        Some(AgentActivity::Idle)
    );
    assert_eq!(
        screen_activity(
            &definition,
            &with_status("\u{273b} Brewed for 7s \u{b7} done 3:34 PM"),
            80
        ),
        Some(AgentActivity::Idle)
    );
}

fn process(comm: &str, cmdline: &[&str]) -> ProcessFact {
    ProcessFact {
        pid: 4242,
        pgid: 4242,
        ppid: 1,
        start_identity: StartIdentity::new(1),
        comm: comm.to_owned(),
        cmdline: cmdline
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
    }
}

#[test]
fn the_process_matchers_accept_the_observed_claude_process_forms() {
    let definition = installed_definition();
    let matchers = definition
        .manifest()
        .process_matchers()
        .expect("the manifest declares process matchers");

    assert!(matchers.matches(&process("claude", &["claude"])));
    assert!(matchers.matches(&process("claude", &["/usr/bin/claude", "--resume", "abc"])));
    assert!(matchers.matches(&process(
        "node",
        &["node", "/opt/claude-code/bin/claude.js"]
    )));
    assert!(matchers.matches(&process("bun", &["bun", "/opt/claude-code/cli/claude"])));
    for cmdline in [
        &["/home/user/.local/bin/claude"][..],
        &["claude", "--resume", "0197aaaa", "--fork-session"],
    ] {
        assert!(
            matchers.matches(&process("sh", cmdline)),
            "{cmdline:?} by command line"
        );
    }

    // The npm package installs its executable as `bin/claude.exe`: the kernel
    // name is `claude.exe` and the command line ends in `claude.exe`, which
    // neither pattern accepts. Pinned; accepting it is a manifest change.
    assert!(!matchers.matches(&process(
        "claude.exe",
        &["/opt/npm/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe"]
    )));

    // The command-line pattern `node|bun … \bclaude\b` also takes a script whose
    // name merely contains `claude` as a word; the matcher is that loose.
    assert!(matchers.matches(&process("node", &["node", "/opt/claude-companion.mjs"])));

    for (comm, cmdline) in [
        ("zsh", vec!["zsh", "-c", "claude"]),
        ("python3", vec!["python3", "claude.py"]),
        ("claudebar", vec!["/usr/lib/claudebar/claudebar", "usage"]),
        ("bash", vec!["bash"]),
    ] {
        assert!(
            !matchers.matches(&process(comm, &cmdline)),
            "{comm} {cmdline:?}"
        );
    }
}

/// Everything the built-in manifest classifies, the package manifest
/// classifies the same way: the captured frames with and without their titles,
/// and the synthetic dialogs of the rule tests.
#[test]
fn the_package_manifest_behaves_like_the_builtin_manifest_on_every_frame() {
    let package = installed_definition();
    let builtin = builtin_definition();
    let mut compared = 0_usize;
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        for title in [frame.title.as_deref(), None] {
            assert_eq!(
                transitions(&package, title, &frame.rows, frame.columns),
                transitions(&builtin, title, &frame.rows, frame.columns),
                "{} with title {title:?}",
                frame.name
            );
            compared += 1;
        }
    }
    let synthetic: [&[&str]; 7] = [
        &["Bash command", "Do you want to proceed?", "\u{276f} 1. Yes"],
        &[
            "Do you want to proceed?",
            "\u{276f} 1. Yes",
            "3. No",
            "Esc to cancel",
        ],
        &["Do you want to allow this connection?"],
        &["Run a dynamic workflow?", "esc to cancel"],
        &[
            "\u{273b} Noodling\u{2026} (12s \u{b7} \u{2193} 1.2k tokens)",
            "\u{276f} ",
        ],
        &["\u{273b} Cogitated for 26s", "\u{276f} "],
        &["plain output"],
    ];
    for lines in synthetic {
        assert_eq!(
            transitions(&package, None, &dialog(lines), 80),
            transitions(&builtin, None, &dialog(lines), 80),
            "{lines:?}"
        );
        compared += 1;
    }
    for title in [
        "\u{280b} x",
        "\u{2733} Claude Code",
        "\u{25d0} Claude Code",
        "x",
        "",
    ] {
        assert_eq!(
            title_activity(&package, title),
            title_activity(&builtin, title),
            "{title:?}"
        );
        compared += 1;
    }
    assert!(compared > 100, "frames compared: {compared}");
}

// The real-Claude tests below need a `claude` on PATH and an explicit opt-in.
// They are `#[ignore]`d and fail by name, never skip, when selected without
// either.

/// Opt-in variable of the real-Claude tests.
const E2E_VARIABLE: &str = "POHUNEK_CLAUDE_E2E";

/// Opt-in variable of the screen capture: the absolute directory the captured
/// screens are written to. The capture is a refresh tool, not a check: without
/// the variable it does nothing.
const CAPTURE_VARIABLE: &str = "POHUNEK_CLAUDE_CAPTURE_DIR";

/// Profile the real-Claude tests launch through.
const PROFILE: &str = "claude-test";

/// Start of the terminal title Claude sets while it waits for input: a
/// six-teardrop-spoked asterisk (U+2733) and a space, then the product name.
const IDLE_TITLE_PREFIX: &str = "\u{2733} ";

/// How long the fork test waits for a hook report that the daemon rejects,
/// before it asserts that none was adopted. The hook runs within a second of
/// the fork's start; the wait is a multiple of that.
const FORK_REPORT_GRACE_SECS: u64 = 5;

/// The prompt the tests submit.
const PROMPT: &str = "say hi";

/// How long one `session wait` polls before the caller retries, in
/// milliseconds.
const WAIT_SLICE_MS: &str = "8000";

/// Looks `name` up on `PATH` the way the daemon resolves a program.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Fails the test, naming the opt-in, unless a real `claude` is available.
///
/// The tests are `#[ignore]`d so a run without Claude skips them with that
/// reason; a run that selects them (`--ignored`, as the `claude-package` CI job
/// does) must have the opt-in variable and a `claude` on `PATH`, and fails
/// loudly otherwise.
fn require_real_claude() {
    assert_eq!(
        std::env::var(E2E_VARIABLE).as_deref(),
        Ok("1"),
        "the real-Claude tests need {E2E_VARIABLE}=1 and the pinned `claude` on PATH"
    );
    assert!(
        find_on_path("claude").is_some(),
        "{E2E_VARIABLE}=1 but no `claude` executable is on PATH"
    );
}

/// The environment of a real Claude that keeps it away from every host state
/// and every service: a throwaway configuration directory and home, the dummy
/// key, the loopback Messages endpoint, and the switches that stop updates,
/// telemetry, error reporting, plugin-marketplace registration and the other
/// non-essential traffic.
fn claude_environment(home: &Path, base_url: &str) -> Vec<(&'static str, String)> {
    vec![
        ("CLAUDE_CONFIG_DIR", path_str(home).to_owned()),
        ("ANTHROPIC_BASE_URL", base_url.to_owned()),
        ("ANTHROPIC_API_KEY", messages_stub::API_KEY.to_owned()),
        ("ANTHROPIC_MODEL", messages_stub::MODEL_ID.to_owned()),
        ("DISABLE_AUTOUPDATER", "1".to_owned()),
        ("DISABLE_UPDATES", "1".to_owned()),
        ("DISABLE_TELEMETRY", "1".to_owned()),
        ("DISABLE_ERROR_REPORTING", "1".to_owned()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
        (
            "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL",
            "1".to_owned(),
        ),
    ]
}

/// The banner the real `claude --version` prints on stdout, run with an empty
/// environment and a throwaway home so no real configuration is read.
fn real_claude_banner() -> String {
    let home = pohunek_test_support::tempdir().expect("tempdir");
    let path = std::env::var_os("PATH").expect("PATH");
    let output = std::process::Command::new("claude")
        .arg("--version")
        .env_clear()
        .env("PATH", path)
        .env("HOME", home.path())
        .env("CLAUDE_CONFIG_DIR", home.path().join(".claude"))
        .output()
        .expect("run `claude --version`");
    assert!(output.status.success(), "claude --version failed");
    String::from_utf8(output.stdout).expect("a UTF-8 banner")
}

/// Length of the API-key suffix Claude records as approved: the key's last 20
/// characters.
const APPROVED_KEY_SUFFIX_LEN: usize = 20;

/// How the first-run state of the throwaway Claude home is seeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Seed {
    /// Nothing: Claude shows its theme picker, then the API-key and login
    /// screens.
    Fresh,
    /// Onboarding done and the dummy key approved, the project folder not yet
    /// trusted: Claude shows the folder trust dialog.
    Onboarded,
    /// Onboarding done, key approved and the project folder trusted: Claude
    /// opens its prompt.
    Trusted,
}

/// A real daemon with a hermetic Claude configuration pointing at a loopback
/// Messages stub, holding the official Claude package installed through a
/// signed catalog.
struct Fixture {
    /// Declared first so the fixture's processes end before the harness removes
    /// the directories they run in, on success and on unwind alike.
    processes: ProcessGuard,
    harness: Harness,
    stub: MessagesStub,
    claude_home: PathBuf,
    /// Digest of the installed package archive; `None` for a daemon that
    /// serves the built-in Claude.
    digest: Option<PackageDigest>,
}

impl Fixture {
    async fn start(seed: Seed) -> Self {
        Self::start_serving(seed, true).await
    }

    /// Starts a fixture whose daemon serves `claude` from the installed
    /// package when `package`, from the built-in descriptor otherwise.
    async fn start_serving(seed: Seed, package: bool) -> Self {
        let key = test_key();
        let anchor =
            HostTrustAnchor::new(vec![root_of(&key, WINDOW_START, WINDOW_END)], Vec::new())
                .expect("the test trust anchor");
        let harness = Harness::start_with_trust(CatalogTrust::Loaded(anchor)).await;
        let processes = ProcessGuard::new(harness.env.root());
        let digest = if package {
            Some(install_package(&harness, &key).await)
        } else {
            None
        };
        let stub = MessagesStub::start();
        let claude_home = harness.env.root().join("claude-home");
        fs::create_dir_all(&claude_home).expect("create the Claude home");
        let fixture = Self {
            processes,
            harness,
            stub,
            claude_home,
            digest,
        };
        fixture.seed(seed);
        fixture.write_profile(PROFILE, None);
        fixture
    }

    /// Writes the first-run state `seed` names into the throwaway Claude home:
    /// `settings.json` holds the theme, `.claude.json` the onboarding flag, the
    /// approved key suffix and the trusted project folders.
    fn seed(&self, seed: Seed) {
        if seed == Seed::Fresh {
            return;
        }
        let cwd = path_str(self.harness.env.cwd()).to_owned();
        let key = messages_stub::API_KEY;
        let suffix = &key[key.len() - APPROVED_KEY_SUFFIX_LEN..];
        let mut config = serde_json::json!({
            "hasCompletedOnboarding": true,
            "customApiKeyResponses": { "approved": [suffix], "rejected": [] },
        });
        if seed == Seed::Trusted {
            config["projects"] = serde_json::json!({ cwd: { "hasTrustDialogAccepted": true } });
        }
        fs::write(
            self.claude_home.join(".claude.json"),
            serde_json::to_vec_pretty(&config).expect("config JSON"),
        )
        .expect("write .claude.json");
        fs::write(
            self.claude_home.join("settings.json"),
            serde_json::to_vec_pretty(&serde_json::json!({ "theme": "dark" }))
                .expect("settings JSON"),
        )
        .expect("write settings.json");
    }

    /// Writes the profile `name`, pinned to the installed package and to the
    /// fixture's Claude home. `program` replaces the runtime's `claude`.
    fn write_profile(&self, name: &str, program: Option<&Path>) {
        let program = program.map_or_else(String::new, |program| {
            format!("program = \"{}\"\n", path_str(program))
        });
        let env = claude_environment(&self.claude_home, &self.stub.base_url())
            .iter()
            .fold(String::new(), |mut env, (key, value)| {
                writeln!(env, "{key} = \"{value}\"").expect("write to a string");
                env
            });
        let pin = self.digest.as_ref().map_or_else(String::new, |digest| {
            format!(
                "package = \"{PACKAGE_ID}\"\ndigest = \"{}\"\n",
                digest.as_str()
            )
        });
        self.harness.profile(
            name,
            &format!("base = \"claude\"\n{pin}{program}\n[env]\n{env}"),
        );
    }

    /// Installs the hook integration into the fixture's Claude home through the
    /// profile selector, the way an owner installs it: the daemon resolves the
    /// home exactly as a launch does, so the real home is never touched.
    async fn install_integration(&self) {
        let (code, installed) = self
            .harness
            .json(&[
                "integration",
                "install",
                "--agent",
                "claude",
                "--profile",
                PROFILE,
            ])
            .await;
        assert_eq!(code, 0, "{installed}");
        assert!(
            installed["ok"]["failed"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "{installed}"
        );
        let settings =
            fs::read_to_string(self.claude_home.join("settings.json")).expect("settings.json");
        assert!(
            settings.contains("SessionStart") && settings.contains("SubagentStart"),
            "pohunek registered its hooks in the fixture home: {settings}"
        );
    }

    /// The inventory entry `host inspect local` reports for `profile`.
    async fn inventory_entry(&self, profile: &str) -> Value {
        let (code, host) = self.harness.json(&["host", "inspect", "local"]).await;
        assert_eq!(code, 0, "{host}");
        host["ok"]["runtimes"]
            .as_array()
            .expect("runtimes")
            .iter()
            .find(|runtime| runtime["agent"] == profile)
            .unwrap_or_else(|| panic!("the host lists {profile}: {host}"))
            .clone()
    }

    /// The installed package is the official, selected source of `claude`, and
    /// the daemon read the real executable's version through the package's
    /// probe: a runtime without a probe reports no `supported` verdict.
    async fn assert_package_serves_claude(&self) {
        let (code, listed) = self.harness.json(&["plugin", "list"]).await;
        assert_eq!(code, 0, "{listed}");
        let packages = listed["ok"]["packages"].as_array().expect("packages");
        assert_eq!(packages.len(), 1, "{listed}");
        let package = &packages[0];
        assert_eq!(package["package"]["id"], PACKAGE_ID, "{listed}");
        assert_eq!(package["origin"], "official", "{listed}");
        assert_eq!(package["enabled"], true, "{listed}");
        assert_eq!(package["selected"], true, "{listed}");
        assert_eq!(
            package["digest"],
            self.digest.as_ref().expect("a package fixture").as_str(),
            "{listed}"
        );

        let claude = self.inventory_entry(PROFILE).await;
        assert_eq!(claude["available"], true, "{claude}");
        assert_eq!(claude["supported"], true, "{claude}");
        assert_eq!(claude["version"], locked_release(), "{claude}");
    }

    /// `session new` of `profile` in the fixture's working directory, at
    /// `columns` wide.
    async fn launch_at(&self, profile: &str, name: &str, columns: u16) -> (i32, Value) {
        let cwd = self.harness.env.cwd().to_path_buf();
        self.harness
            .json(&[
                "session",
                "new",
                "--agent",
                profile,
                "--cwd",
                path_str(&cwd),
                "--name",
                name,
                "--cols",
                &columns.to_string(),
            ])
            .await
    }

    async fn launch(&self, profile: &str, name: &str) -> (i32, Value) {
        self.launch_at(profile, name, DEFAULT_COLUMNS).await
    }

    async fn new_session(&self, name: &str) -> Value {
        let (code, launched) = self.launch(PROFILE, name).await;
        assert_eq!(code, 0, "{launched}");
        launched["ok"].clone()
    }

    /// Launches a session and returns its id.
    async fn new_session_id(&self, name: &str) -> String {
        self.new_session(name).await["id"]
            .as_str()
            .expect("session id")
            .to_owned()
    }

    /// Sends `text` to the session; the daemon writes it, waits the submit
    /// delay, then writes the submit key.
    async fn input(&self, id: &str, text: &str) {
        let (code, sent) = self.harness.json(&["session", "input", id, text]).await;
        assert_eq!(code, 0, "{sent}");
    }

    /// Waits until the session's detected activity is `activity` and returns
    /// the session record that matched.
    async fn wait_activity(&self, id: &str, activity: &str) -> Value {
        wait_until(&format!("activity {activity} of {id}"), || async {
            let (code, waited) = self
                .harness
                .json(&[
                    "session",
                    "wait",
                    id,
                    "--activity",
                    activity,
                    "--timeout-ms",
                    WAIT_SLICE_MS,
                ])
                .await;
            assert_eq!(code, 0, "{waited}");
            (waited["ok"]["reason"] == "activity_matched").then(|| waited["ok"]["session"].clone())
        })
        .await
    }

    /// Waits until the session reports `activity` on the strength of `source`.
    async fn wait_evidence(&self, id: &str, activity: &str, source: &str) -> Value {
        wait_until(&format!("{activity} from {source} of {id}"), || async {
            let (code, inspected) = self.harness.json(&["session", "inspect", id]).await;
            assert_eq!(code, 0, "{inspected}");
            (inspected["ok"]["activity"] == activity && inspected["ok"]["state_source"] == source)
                .then(|| inspected["ok"].clone())
        })
        .await
    }

    /// The visible rows and the terminal title of the session.
    async fn screen(&self, id: &str) -> (Option<String>, Vec<String>) {
        let (code, screen) = self.harness.json(&["session", "screen", id]).await;
        assert_eq!(code, 0, "{screen}");
        let rows = screen["ok"]["visible_lines"]
            .as_array()
            .expect("visible lines")
            .iter()
            .map(|line| line.as_str().expect("line").trim_end().to_owned())
            .collect();
        let title = screen["ok"]["title"].as_str().map(str::to_owned);
        (title, rows)
    }

    /// Waits until the visible screen holds a line containing `needle`.
    async fn wait_screen_line(&self, id: &str, needle: &str) -> Vec<String> {
        wait_until(&format!("a screen line containing {needle:?}"), || async {
            let (_title, rows) = self.screen(id).await;
            rows.iter().any(|row| row.contains(needle)).then_some(rows)
        })
        .await
    }

    /// Waits until the terminal title starts with `prefix` and returns the
    /// rows on screen at that moment.
    async fn wait_title_prefix(&self, id: &str, prefix: &str) -> Vec<String> {
        wait_until(
            &format!("a terminal title starting with {prefix:?}"),
            || async {
                let (title, rows) = self.screen(id).await;
                title
                    .is_some_and(|title| title.starts_with(prefix))
                    .then_some(rows)
            },
        )
        .await
    }

    /// The session record `session inspect` reports.
    async fn inspect(&self, id: &str) -> Value {
        let (code, inspected) = self.harness.json(&["session", "inspect", id]).await;
        assert_eq!(code, 0, "{inspected}");
        inspected["ok"].clone()
    }

    /// Waits until the hook reported the conversation id and returns the
    /// session record that carries it.
    async fn wait_reported_conversation(&self, id: &str) -> Value {
        wait_until("the hook-reported conversation id", || async {
            let inspected = self.inspect(id).await;
            inspected["active_agent_session_id"]
                .is_string()
                .then_some(inspected)
        })
        .await
    }

    /// The transcript Claude wrote for conversation `reference`, below
    /// `projects/<folder>/<reference>.jsonl`.
    fn transcript_file(&self, reference: &str) -> Option<PathBuf> {
        let name = format!("{reference}.jsonl");
        fs::read_dir(self.claude_home.join("projects"))
            .ok()?
            .flatten()
            .map(|folder| folder.path().join(&name))
            .find(|candidate| candidate.is_file())
    }

    /// How many conversation transcripts Claude has written.
    fn transcript_count(&self) -> usize {
        let Ok(folders) = fs::read_dir(self.claude_home.join("projects")) else {
            return 0;
        };
        folders
            .flatten()
            .filter_map(|folder| fs::read_dir(folder.path()).ok())
            .flat_map(Iterator::flatten)
            .filter(|file| file.path().extension().is_some_and(|ext| ext == "jsonl"))
            .count()
    }

    /// Stops the session and waits until its runtime is no longer live.
    async fn stop(&self, id: &str) {
        let (code, stopped) = self.harness.json(&["session", "stop", id]).await;
        assert_eq!(code, 0, "{stopped}");
        wait_until(&format!("session {id} to stop"), || async {
            let inspected = self.inspect(id).await;
            (inspected["runtime"]["state"] != "live").then_some(())
        })
        .await;
    }

    /// Stops the daemon and every process of the fixture, so none survives the
    /// test. The guard repeats this when a test fails before it gets here.
    async fn finish(self) {
        let Self {
            processes,
            harness,
            stub,
            ..
        } = self;
        processes.reap();
        harness.stop().await;
        processes.reap();
        assert!(
            !stub.saw_credential(),
            "no request to the stub carried an Authorization header or a foreign key"
        );
    }
}

/// Installs the built package archive through a catalog signed by `key`, the
/// way a host with a catalog trust anchor installs an official package, and
/// returns the archive digest.
///
/// The catalog authorizes the package id, the runtime id `claude` and the
/// digest, which is what lets the package serve a reserved runtime id.
async fn install_package(harness: &Harness, key: &ed25519_dalek::SigningKey) -> PackageDigest {
    let (bytes, digest) = built_archive();
    let archive = harness.env.root().join("claude.tar.zst");
    fs::write(&archive, bytes).expect("write the archive");
    let entry = CatalogEntry {
        package_id: PackageId::parse(PACKAGE_ID).expect("package id"),
        runtime_id: RuntimeId::parse("claude").expect("runtime id"),
        version: PackageVersion::parse(PACKAGE_VERSION).expect("package version"),
        digest: digest.clone(),
        platforms: vec![host_platform()],
        core: ANY_CORE.to_owned(),
    };
    let catalog = harness.env.root().join("runtime-catalog.json");
    fs::write(
        &catalog,
        signed(key, catalog_of(1, WINDOW_END, vec![entry])),
    )
    .expect("write the catalog");
    let (code, installed) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&archive),
            "--catalog",
            path_str(&catalog),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{installed}");
    assert_eq!(installed["ok"]["status"], "installed", "{installed}");
    let package = &installed["ok"]["package"];
    assert_eq!(package["origin"], "official", "{installed}");
    assert_eq!(package["digest"], digest.as_str(), "{installed}");
    digest
}

/// Columns of the top-level fixtures.
const DEFAULT_COLUMNS: u16 = 100;

/// Terminal widths every captured state is also recorded at.
const CAPTURE_WIDTHS: [u16; 9] = [20, 30, 40, 60, 80, 100, 120, 160, 200];

/// Rows of every captured screen.
const CAPTURE_ROWS: &str = "24";

/// How long a resized screen is given to redraw before it is read, in
/// milliseconds. Claude redraws within a frame; the pause only has to outlast
/// that and the daemon's screen tracker.
const REDRAW_PAUSE_MS: u64 = 700;

/// Replacement for the random part of the throwaway root in captured text, so
/// the files are reproducible.
// hermetic-allowed: #146 the text is a fixed replacement for the random fixture root in captured screens, never a host path
const NEUTRAL_ROOT: &str = "/tmp/ph-sample";

/// Writes one captured screen: `<name>.txt` holds the rows and `<name>.title`
/// the terminal title when Claude had set one.
fn write_capture(directory: &Path, name: &str, root: &Path, title: Option<&str>, rows: &[String]) {
    fs::create_dir_all(directory).expect("create the capture directory");
    let root = path_str(root);
    let text = rows.iter().fold(String::new(), |mut text, row| {
        text.push_str(&row.replace(root, NEUTRAL_ROOT));
        text.push('\n');
        text
    });
    fs::write(directory.join(format!("{name}.txt")), text).expect("write the screen");
    if let Some(title) = title {
        fs::write(
            directory.join(format!("{name}.title")),
            format!("{}\n", title.replace(root, NEUTRAL_ROOT)),
        )
        .expect("write the title");
    }
}

impl Fixture {
    /// Records the session's current screen and title as `<name>` below
    /// `directory`.
    async fn capture(&self, directory: &Path, id: &str, name: &str) {
        let (title, rows) = self.screen(id).await;
        write_capture(
            directory,
            name,
            self.harness.env.root(),
            title.as_deref(),
            &rows,
        );
    }

    async fn resize(&self, id: &str, columns: u16) {
        let (code, resized) = self
            .harness
            .json(&[
                "session",
                "resize",
                id,
                "--cols",
                &columns.to_string(),
                "--rows",
                CAPTURE_ROWS,
            ])
            .await;
        assert_eq!(code, 0, "{resized}");
        // timing-allowed: #146 capture-only redraw pause; the screen tracker exposes no redraw-complete signal
        tokio::time::sleep(Duration::from_millis(REDRAW_PAUSE_MS)).await;
    }

    /// Records the current state of the session at every width as
    /// `widths/<state>_w<columns>`, waiting for the screen to stop changing
    /// first when `settle` (a streaming or animated screen never does), then
    /// restores the default width.
    async fn capture_widths(&self, directory: &Path, id: &str, state: &str, settle: bool) {
        let widths = directory.join("widths");
        for columns in CAPTURE_WIDTHS {
            self.resize(id, columns).await;
            if settle {
                let previous = std::cell::RefCell::new(self.screen(id).await);
                wait_until(&format!("{state} to settle at {columns}"), || async {
                    // timing-allowed: #146 capture-only redraw pause; the screen tracker exposes no redraw-complete signal
                    tokio::time::sleep(Duration::from_millis(REDRAW_PAUSE_MS)).await;
                    let current = self.screen(id).await;
                    let stable = current == *previous.borrow();
                    *previous.borrow_mut() = current;
                    stable.then_some(())
                })
                .await;
            }
            self.capture(&widths, id, &format!("{state}_w{columns}"))
                .await;
        }
        self.resize(id, DEFAULT_COLUMNS).await;
    }
}

#[test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
fn the_real_claude_banner_is_the_locked_release_the_probe_accepts() {
    require_real_claude();
    let definition = installed_definition();
    let policy = definition.version_probe_policy().expect("a version probe");
    let banner = real_claude_banner();
    let version = policy
        .read_output(&banner)
        .unwrap_or_else(|| panic!("the package template reads the real banner {banner:?}"));
    assert_eq!(version.to_string(), locked_release(), "the pinned release");
    assert_eq!(banner.trim_end(), banner_of(&locked_release()));
    assert!(policy.supports(version));
}

/// The package manifest, read from the package directory, classifies the live
/// screen and title of a real session as `expected`, the activity the daemon
/// detected from the installed package.
async fn assert_package_agrees(
    fixture: &Fixture,
    definition: &RuntimeDefinition,
    id: &str,
    expected: AgentActivity,
) {
    let (title, rows) = fixture.screen(id).await;
    let columns = u16::try_from(
        rows.iter()
            .map(|row| row.chars().count())
            .max()
            .unwrap_or(0),
    )
    .unwrap_or(u16::MAX)
    .max(DEFAULT_COLUMNS);
    let seen = transitions(definition, title.as_deref(), &rows, columns);
    assert_eq!(
        seen.iter()
            .rfind(|transition| transition.source != StateSource::Process)
            .map(|transition| transition.activity),
        Some(expected),
        "the package manifest on the live screen, title {title:?}: {rows:#?}"
    );
}

/// The command line of `pid`, read from `/proc`.
fn command_line(pid: u64) -> Vec<String> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).expect("read the command line");
    bytes
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect()
}

/// The process facts of `pid` as the daemon's process matchers see them.
fn process_fact_of(pid: u64) -> ProcessFact {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
        .expect("read the process name")
        .trim_end()
        .to_owned();
    ProcessFact {
        pid: u32::try_from(pid).expect("a process id"),
        pgid: 0,
        ppid: 0,
        start_identity: StartIdentity::new(1),
        comm,
        cmdline: command_line(pid),
    }
}

/// Parent process id of `pid`, read from `/proc`.
fn parent_pid(pid: u64) -> u64 {
    let status =
        fs::read_to_string(format!("/proc/{pid}/status")).expect("read the process status");
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|value| value.trim().parse().ok())
        .expect("a PPid line")
}

/// Drives a real `claude` through the installed official package with a fresh
/// `CLAUDE_CONFIG_DIR`, a loopback Messages stub and hooks registered by
/// pohunek's own integration install, and reads every live screen and title
/// through the package manifest.
///
/// The fixture installs the built archive through a signed catalog, so the
/// launch resolves the profile's pin to the package and runs the package's
/// version probe against the real executable before it starts. The input
/// reaches Claude as written text followed, after the descriptor's delay, by a
/// separate submit key: the stub receives the prompt intact and the turn runs.
///
/// Needs a `claude` on `PATH` and `POHUNEK_CLAUDE_E2E=1`; the model is a
/// loopback stub, so no provider or credential is involved, and the fixture's
/// environment switches off updates, telemetry, error reporting, plugin
/// marketplace registration and the other non-essential traffic.
#[tokio::test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
async fn a_real_claude_session_launches_and_is_detected_through_package_facts() {
    require_real_claude();
    let package = installed_definition();
    let fixture = Fixture::start(Seed::Trusted).await;
    fixture.install_integration().await;

    fixture.assert_package_serves_claude().await;

    let launched = fixture.new_session("e2e").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    assert_eq!(launched["agent_base"], "claude");
    assert_eq!(launched["capabilities"]["resume"], true);
    assert_eq!(launched["capabilities"]["fork"], true);

    // The launched process is what the package's process matchers describe:
    // the distribution and native-installer binary is named `claude`; the npm
    // package's executable is `bin/claude.exe`, which neither matcher accepts
    // (a pinned gap, see the process-form test).
    let launch_pid = fixture.inspect(&id).await["pid"]
        .as_u64()
        .expect("the launch process id");
    let matchers = package
        .manifest()
        .process_matchers()
        .expect("the manifest declares process matchers");
    let fact = process_fact_of(launch_pid);
    match fact.comm.as_str() {
        "claude" => assert!(
            matchers.matches(&fact),
            "the real process {fact:?} is matched"
        ),
        "claude.exe" => assert!(
            !matchers.matches(&fact),
            "the npm launch form {fact:?} is not matched"
        ),
        other => panic!("an unexpected Claude process name {other:?}: {fact:?}"),
    }

    // Idle: the prompt box and the `✳` title.
    fixture.wait_activity(&id, "idle").await;
    fixture.wait_screen_line(&id, "for shortcuts").await;
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Idle).await;

    // Input: written text, then a separate submit after the descriptor's
    // delay. The stub holds the reply so the status line is on screen.
    let prompt = format!("{} {PROMPT}", messages_stub::HOLD_MARKER);
    fixture.input(&id, &prompt).await;
    fixture.wait_evidence(&id, "working", "screen").await;
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Working).await;
    assert!(
        fixture.stub.requests_containing(&prompt) >= 1,
        "the stub received the prompt intact in the turn's request"
    );

    fixture.stub.open_gate();
    fixture
        .wait_screen_line(&id, messages_stub::SECOND_CHUNK)
        .await;
    fixture.wait_title_prefix(&id, IDLE_TITLE_PREFIX).await;
    fixture.wait_activity(&id, "idle").await;
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Idle).await;

    fixture.stop(&id).await;
    fixture.finish().await;
}

/// The `SessionStart` hook runs from the process pohunek launched, so its report
/// becomes the session's native reference: the session record names the
/// reporting process as the launch process and carries Claude's own
/// conversation id, which names Claude's transcript file.
#[tokio::test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
async fn a_real_claude_session_start_hook_reports_the_native_reference() {
    require_real_claude();
    let fixture = Fixture::start(Seed::Trusted).await;
    fixture.install_integration().await;

    let launched = fixture.new_session("hooks").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    fixture.wait_activity(&id, "idle").await;
    fixture.input(&id, PROMPT).await;
    fixture
        .wait_screen_line(&id, messages_stub::SECOND_CHUNK)
        .await;

    let reported = fixture.wait_reported_conversation(&id).await;
    let launch_pid = reported["pid"].as_u64().expect("the launch process id");
    let reporter_pid = reported["active_agent_pid"]
        .as_u64()
        .expect("the reporting process id");
    eprintln!(
        "reporter {reporter_pid}, launch {launch_pid}, parent of the reporter {}",
        parent_pid(reporter_pid)
    );
    assert_eq!(
        reporter_pid, launch_pid,
        "Claude runs its hooks from the launched process"
    );
    let reference = reported["active_agent_session_id"]
        .as_str()
        .expect("the conversation id")
        .to_owned();
    assert_eq!(
        reported["native_session_id"], reference,
        "the reported id is the session's native reference: {reported}"
    );
    assert!(
        fixture.transcript_file(&reference).is_some(),
        "Claude wrote a transcript for the reported conversation {reference}"
    );

    fixture.stop(&id).await;
    fixture.finish().await;
}

/// Resume and fork launch Claude with the descriptor's arguments and the
/// hook-reported conversation id: `--resume <id>`, and `--resume <id>
/// --fork-session` for a fork, which is a new conversation with its own id.
#[tokio::test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
async fn a_real_claude_resumes_and_forks_with_the_descriptor_arguments() {
    require_real_claude();
    let package = installed_definition();
    let fixture = Fixture::start(Seed::Trusted).await;
    fixture.install_integration().await;

    let launched = fixture.new_session("origin").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    fixture.wait_activity(&id, "idle").await;
    fixture.input(&id, PROMPT).await;
    fixture
        .wait_screen_line(&id, messages_stub::SECOND_CHUNK)
        .await;
    let reported = fixture.wait_reported_conversation(&id).await;
    let reference = reported["active_agent_session_id"]
        .as_str()
        .expect("the conversation id")
        .to_owned();

    // Resume: the same conversation again, `--resume <id>`.
    fixture.stop(&id).await;
    let (code, resumed) = fixture.harness.json(&["session", "resume", &id]).await;
    assert_eq!(code, 0, "{resumed}");
    fixture.wait_activity(&id, "idle").await;
    let pid = fixture.inspect(&id).await["pid"]
        .as_u64()
        .expect("the resumed process id");
    let argv = command_line(pid);
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "--resume" && pair[1] == reference),
        "the resumed Claude runs `--resume {reference}`: {argv:?}"
    );
    assert!(
        !argv.iter().any(|argument| argument == "--fork-session"),
        "{argv:?}"
    );
    let native = package.native().expect("native recovery");
    assert_eq!(
        native
            .resume_argv(&SessionRef::id(&reference).expect("reference"))
            .expect("resume argv"),
        ["--resume", reference.as_str()]
    );
    fixture
        .wait_screen_line(&id, messages_stub::SECOND_CHUNK)
        .await;

    // Fork: a new session on a new conversation, `--resume <id> --fork-session`.
    let (code, forked) = fixture
        .harness
        .json(&["session", "fork", &id, "--name", "forked"])
        .await;
    assert_eq!(code, 0, "{forked}");
    let fork_id = forked["ok"]["id"]
        .as_str()
        .or_else(|| forked["ok"]["session"]["id"].as_str())
        .unwrap_or_else(|| panic!("the fork names its session: {forked}"))
        .to_owned();
    assert_ne!(fork_id, id);
    fixture.wait_activity(&fork_id, "idle").await;
    let forked_process = fixture.inspect(&fork_id).await["pid"]
        .as_u64()
        .expect("the fork process id");
    let forked_argv = command_line(forked_process);
    assert!(
        forked_argv.windows(3).any(|triple| triple[0] == "--resume"
            && triple[1] == reference
            && triple[2] == "--fork-session"),
        "the forked Claude runs `--resume {reference} --fork-session`: {forked_argv:?}"
    );
    assert_eq!(
        package
            .native()
            .expect("native recovery")
            .fork_argv(&SessionRef::id(&reference).expect("reference"))
            .expect("fork argv"),
        ["--resume", reference.as_str(), "--fork-session"]
    );
    // The fork runs a conversation of its own: a second transcript appears
    // once it takes a turn.
    fixture.input(&fork_id, "again").await;
    wait_until("the fork's own transcript", || async {
        (fixture.transcript_count() == 2).then_some(())
    })
    .await;

    // Pinned gap: Claude's SessionStart hook reports the fork's new
    // conversation id, but the child keeps the reference it inherited from its
    // source, so the daemon rejects the report as a launch identity mismatch
    // (`launch_identity_reference_mismatch`, reconcile.rs). Resuming the fork
    // would resume the source conversation. When the daemon accepts the fork's
    // own id, this test fails and must assert the new id instead.
    // timing-allowed: #146 negative wait: the rejected fork report has no readiness signal to wait on
    tokio::time::sleep(Duration::from_secs(FORK_REPORT_GRACE_SECS)).await;
    let fork_record = fixture.inspect(&fork_id).await;
    assert_eq!(
        fork_record["native_session_id"], reference,
        "the fork holds its source's reference: {fork_record}"
    );
    assert!(
        fork_record["active_agent_session_id"].is_null(),
        "the fork's own conversation id was not adopted: {fork_record}"
    );

    fixture.stop(&fork_id).await;
    fixture.stop(&id).await;
    fixture.finish().await;
}

/// A subagent the model starts is reported by the `SubagentStart` and
/// `SubagentStop` hooks through the `identity-subagent-v1` schema: the session
/// record lists it as a Claude subagent and ends it when the task completes.
#[tokio::test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
async fn a_real_claude_subagent_is_reported_through_the_hook_schema() {
    require_real_claude();
    let fixture = Fixture::start(Seed::Trusted).await;
    fixture.install_integration().await;

    let launched = fixture.new_session("subagent").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    fixture.wait_activity(&id, "idle").await;
    fixture
        .input(&id, &format!("{} start it", messages_stub::SUBAGENT_MARKER))
        .await;
    let subagent = wait_until("a reported subagent", || async {
        let inspected = fixture.inspect(&id).await;
        inspected["subagents"]
            .as_array()
            .and_then(|subagents| subagents.first().cloned())
    })
    .await;
    eprintln!("subagent record: {subagent}");
    assert_eq!(subagent["provider"], "claude", "{subagent}");

    let finished = wait_until("the subagent to end", || async {
        let inspected = fixture.inspect(&id).await;
        inspected["subagents"]
            .as_array()
            .and_then(|subagents| subagents.first().cloned())
            .filter(|subagent| subagent["lifecycle"] != "running")
    })
    .await;
    assert_ne!(finished["lifecycle"], "running", "{finished}");
    assert!(
        fixture
            .stub
            .requests_containing(messages_stub::SUBAGENT_PROMPT)
            >= 1,
        "the subagent made its own model request"
    );

    fixture.stop(&id).await;
    fixture.finish().await;
}

/// A tool approval prompt of the real Claude is blocked through the screen,
/// for the daemon and for the package manifest, although Claude keeps its
/// idle title while the dialog is open.
#[tokio::test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
async fn a_real_claude_approval_prompt_is_blocked_by_the_screen() {
    require_real_claude();
    let package = installed_definition();
    let fixture = Fixture::start(Seed::Trusted).await;
    fixture.install_integration().await;

    let launched = fixture.new_session("approval").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    fixture.wait_activity(&id, "idle").await;

    fixture
        .input(&id, &format!("{} run it", messages_stub::APPROVAL_MARKER))
        .await;
    let blocked = fixture.wait_activity(&id, "blocked").await;
    assert_eq!(blocked["state_source"], "screen", "{blocked}");
    let rows = fixture
        .wait_screen_line(&id, "Do you want to proceed?")
        .await;
    assert!(
        rows.iter()
            .any(|row| row.contains(messages_stub::APPROVAL_COMMAND)),
        "the prompt names the command: {rows:#?}"
    );
    let (title, _rows) = fixture.screen(&id).await;
    assert!(
        title.is_some_and(|title| title.starts_with(IDLE_TITLE_PREFIX)),
        "Claude keeps its idle title while the dialog is open"
    );
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Blocked).await;

    fixture.stop(&id).await;
    fixture.finish().await;
}

/// `claude` is a reserved runtime id: an install trusted only by its digest is
/// refused, so the package serves it through a signed catalog alone.
#[tokio::test]
async fn installing_the_package_is_refused_while_claude_is_a_reserved_builtin() {
    let harness = Harness::start().await;
    let (bytes, digest) = built_archive();
    let archive = harness.env.root().join("claude.tar.zst");
    fs::write(&archive, bytes).expect("write the archive");
    let (code, refused) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&archive),
            "--sha256",
            digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(
        refused["err"]["code"], "package_runtime_not_claimable",
        "{refused}"
    );
    harness.stop().await;
}

/// The signed-catalog install records the package as official, enabled and
/// selected, and the daemon-built definition carries the catalog's provenance.
#[tokio::test]
async fn the_signed_catalog_install_serves_claude_with_official_provenance() {
    let fixture = Fixture::start(Seed::Trusted).await;
    let (code, listed) = fixture.harness.json(&["plugin", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    let packages = listed["ok"]["packages"].as_array().expect("packages");
    assert_eq!(packages.len(), 1, "{listed}");
    assert_eq!(packages[0]["package"]["id"], PACKAGE_ID, "{listed}");
    assert_eq!(
        packages[0]["package"]["version"], PACKAGE_VERSION,
        "{listed}"
    );
    assert_eq!(packages[0]["origin"], "official", "{listed}");
    assert_eq!(packages[0]["selected"], true, "{listed}");
    assert_eq!(
        packages[0]["digest"],
        fixture.digest.as_ref().expect("a package fixture").as_str(),
        "{listed}"
    );
    fixture.finish().await;
}

/// Writes an executable `claude` into a directory of its own below `root` that
/// answers `--version` with `banner` and otherwise idles, and returns its path.
fn write_fake_claude(root: &Path, directory: &str, banner: &str) -> PathBuf {
    let path = root.join(directory).join("claude");
    fs::create_dir_all(root.join(directory)).expect("create the fake claude directory");
    write_executable(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo '{banner}'\n  exit 0\nfi\nexec sleep 600\n"),
    )
    .expect("write the fake claude");
    path
}

/// The version probe the package declares decides every launch of `claude`, on
/// the executable the profile names: only the supported release starts.
#[tokio::test]
async fn the_package_version_probe_refuses_an_unsupported_claude_and_starts_the_supported_one() {
    let fixture = Fixture::start(Seed::Trusted).await;
    let root = fixture.harness.env.root().to_path_buf();
    let release = locked_release();
    let unsupported = [
        banner_of("2.1.288"),
        banner_of("2.2.0"),
        format!("{release}-rc.1 (Claude Code)"),
        format!("Claude Code {release}"),
        "unreadable".to_owned(),
    ];
    for (index, banner) in unsupported.iter().enumerate() {
        let profile = format!("fake-unsupported-{index}");
        let program = write_fake_claude(&root, &profile, banner);
        fixture.write_profile(&profile, Some(&program));

        let entry = fixture.inventory_entry(&profile).await;
        assert_eq!(entry["supported"], false, "{banner:?}: {entry}");
        let (code, refused) = fixture.launch(&profile, "refused").await;
        assert_eq!(code, 1, "{banner:?}: {refused}");
        assert_eq!(
            refused["err"]["code"], "agent_runtime_unsupported",
            "{banner:?}: {refused}"
        );
    }

    let banner = banner_of(&release);
    let program = write_fake_claude(&root, "fake-supported", &banner);
    fixture.write_profile("fake-supported", Some(&program));
    let entry = fixture.inventory_entry("fake-supported").await;
    assert_eq!(entry["supported"], true, "{entry}");
    assert_eq!(entry["version"], release, "{entry}");
    let (code, launched) = fixture.launch("fake-supported", "accepted").await;
    assert_eq!(code, 0, "{launched}");
    let id = launched["ok"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert_eq!(launched["ok"]["agent_base"], "claude");
    fixture.stop(&id).await;
    fixture.finish().await;
}

/// Whether `pid` is a live process; a terminated one that its parent has not
/// reaped yet is not.
#[cfg(target_os = "linux")]
fn is_alive(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| stat.rsplit(')').next().map(str::to_owned))
        .is_some_and(|rest| !rest.trim_start().starts_with('Z'))
}

/// Starts a detached `sleep` in `env`, the way an agent can leave a helper
/// behind, and returns its process id.
#[cfg(target_os = "linux")]
fn spawn_detached_sleeper(env: &pohunek_test_support::env::TestEnv) -> i32 {
    let output = env
        .command("/bin/sh")
        .args(["-c", "sleep 600 </dev/null >/dev/null 2>&1 & echo $!"])
        .output()
        .expect("start the detached process");
    String::from_utf8(output.stdout)
        .expect("a UTF-8 process id")
        .trim()
        .parse()
        .expect("a process id")
}

#[cfg(target_os = "linux")]
#[test]
fn the_process_guard_terminates_a_detached_process_when_the_test_unwinds() {
    let env = pohunek_test_support::env::TestEnv::new().expect("create the environment");
    let pid = std::cell::Cell::new(0);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = ProcessGuard::new(env.root());
        pid.set(spawn_detached_sleeper(&env));
        assert!(is_alive(pid.get()), "the detached process runs");
        panic!("a failing assertion after the process started");
    }));
    assert!(outcome.is_err(), "the closure panicked");
    assert!(
        !is_alive(pid.get()),
        "the guard terminated the detached process during the unwind"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_process_guard_leaves_a_process_of_another_environment_alone() {
    let ours = pohunek_test_support::env::TestEnv::new().expect("create the environment");
    let other = pohunek_test_support::env::TestEnv::new().expect("create the environment");
    let foreign = spawn_detached_sleeper(&other);
    let guard = ProcessGuard::new(ours.root());
    guard.reap();
    assert!(is_alive(foreign), "a process below another root survives");
    ProcessGuard::new(other.root()).reap();
    assert!(!is_alive(foreign));
}

/// The directory the capture tests write to, from `POHUNEK_CLAUDE_CAPTURE_DIR`.
fn capture_directory() -> Option<PathBuf> {
    let Some(directory) = std::env::var_os(CAPTURE_VARIABLE).map(PathBuf::from) else {
        eprintln!("{CAPTURE_VARIABLE} is not set; nothing captured");
        return None;
    };
    require_real_claude();
    assert!(directory.is_absolute(), "{CAPTURE_VARIABLE} is absolute");
    fs::create_dir_all(&directory).expect("create the capture directory");
    Some(directory)
}

/// Records the first-run screens of a real Claude Code (theme picker, API-key
/// approval, login method) from an empty home into the directory named by
/// `POHUNEK_CLAUDE_CAPTURE_DIR`.
///
/// Claude's first-run preflight sends an unauthenticated `HEAD /api/hello` to
/// `api.anthropic.com` as well as to the configured base URL and exits when
/// either fails, so this capture needs network egress; it carries only the
/// dummy key and a throwaway home. Every later screen is captured by
/// [`capture_the_real_claude_screens`] without egress. Without the variable the
/// test does nothing.
#[tokio::test]
#[ignore = "refreshes compat/claude/screens from a real `claude` with egress; run with POHUNEK_CLAUDE_CAPTURE_DIR and --ignored"]
async fn capture_the_real_claude_first_run_screens() {
    let Some(directory) = capture_directory() else {
        return;
    };
    let fixture = Fixture::start(Seed::Fresh).await;
    let id = fixture.new_session_id("first-run").await;
    fixture
        .wait_screen_line(&id, "Choose the text style that looks best")
        .await;
    fixture.capture(&directory, &id, "theme").await;
    fixture.input(&id, "").await;
    fixture
        .wait_screen_line(&id, "Detected a custom API key")
        .await;
    fixture.capture(&directory, &id, "apikey").await;
    fixture
        .capture_widths(&directory, &id, "apikey", true)
        .await;
    fixture.input(&id, "").await;
    fixture.wait_screen_line(&id, "Select login method").await;
    fixture.capture(&directory, &id, "login").await;
    fixture.stop(&id).await;
    fixture.finish().await;
}

/// Records the screens of a real Claude Code from a seeded home into the
/// directory named by `POHUNEK_CLAUDE_CAPTURE_DIR`: the folder trust dialog,
/// then idle, working, tool approval and question screens from a trusted home,
/// each at every width. A seeded home passes the first-run preflight, so the
/// run needs no network egress beyond the loopback stub. Without the variable
/// the test does nothing; the screens in `compat/claude/screens` are the output
/// of this test and [`capture_the_real_claude_first_run_screens`].
#[tokio::test]
#[ignore = "refreshes compat/claude/screens from a real `claude`; run with POHUNEK_CLAUDE_CAPTURE_DIR and --ignored"]
async fn capture_the_real_claude_screens() {
    let Some(directory) = capture_directory() else {
        return;
    };

    // Onboarded home, untrusted folder.
    let fixture = Fixture::start(Seed::Onboarded).await;
    let id = fixture.new_session_id("trust").await;
    fixture.wait_screen_line(&id, "Quick safety check").await;
    fixture.capture(&directory, &id, "trust").await;
    fixture.capture_widths(&directory, &id, "trust", true).await;
    fixture.stop(&id).await;
    fixture.finish().await;

    // Trusted home: idle, working, idle after a turn, approval, question.
    let fixture = Fixture::start(Seed::Trusted).await;
    fixture.install_integration().await;
    let id = fixture.new_session_id("states").await;
    fixture.wait_activity(&id, "idle").await;
    fixture.wait_screen_line(&id, "for shortcuts").await;
    fixture.capture(&directory, &id, "idle").await;
    fixture.capture_widths(&directory, &id, "idle", true).await;

    fixture
        .input(&id, &format!("{} {PROMPT}", messages_stub::HOLD_MARKER))
        .await;
    fixture.wait_evidence(&id, "working", "screen").await;
    fixture.capture(&directory, &id, "working_early").await;
    fixture
        .wait_screen_line(&id, messages_stub::FIRST_CHUNK)
        .await;
    fixture.capture(&directory, &id, "working_stream").await;
    fixture
        .capture_widths(&directory, &id, "working_stream", false)
        .await;

    fixture.stub.open_gate();
    fixture
        .wait_screen_line(&id, messages_stub::SECOND_CHUNK)
        .await;
    fixture.wait_title_prefix(&id, IDLE_TITLE_PREFIX).await;
    fixture.wait_activity(&id, "idle").await;
    fixture.capture(&directory, &id, "idle_after_turn").await;
    fixture
        .capture_widths(&directory, &id, "idle_after_turn", true)
        .await;

    fixture
        .input(&id, &format!("{} run it", messages_stub::APPROVAL_MARKER))
        .await;
    fixture.wait_activity(&id, "blocked").await;
    fixture
        .wait_screen_line(&id, "Do you want to proceed?")
        .await;
    fixture.capture(&directory, &id, "approval").await;
    fixture
        .capture_widths(&directory, &id, "approval", true)
        .await;
    fixture.stop(&id).await;

    let id = fixture.new_session_id("question").await;
    fixture.wait_activity(&id, "idle").await;
    fixture
        .input(&id, &format!("{} ask", messages_stub::QUESTION_MARKER))
        .await;
    fixture.wait_activity(&id, "blocked").await;
    fixture
        .wait_screen_line(&id, messages_stub::QUESTION_TEXT)
        .await;
    fixture.capture(&directory, &id, "askuser").await;
    fixture
        .capture_widths(&directory, &id, "askuser", true)
        .await;
    fixture.stop(&id).await;
    fixture.finish().await;
}
