//! The official Codex runtime package (`runtime-packages/codex`).
//!
//! Pure tests always run: they build the package directory, parse it through
//! the same path `plugin install` uses, compare it with the built-in Codex
//! descriptor and detection manifest, and check it against the compatibility
//! lock and the screens captured from a real Codex (`compat/codex/screens`).
//! The daemon-backed tests install the built archive through `pohunek plugin
//! install --catalog` against a throwaway signing key and trust anchor, so the
//! package serves `codex` with official trust. The real-Codex tests additionally
//! drive an actual `codex` binary; see their documentation for the opt-in.

// Rust guideline compliant 2026-10-06

#![cfg(unix)]

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
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
use pohunek_test_support::wait::{poll_until, wait_until};
use pohunek_test_support::workspace_root;
use protocol::{
    AgentActivity, BindingProvenance, PackageId, PackageVersion, RuntimeId, StateSource,
};
use serde_json::Value;

#[path = "support/catalog_fixture.rs"]
mod catalog_fixture;
#[path = "support/plugin_harness.rs"]
mod plugin_harness;
#[path = "support/responses_stub.rs"]
mod responses_stub;

use catalog_fixture::{
    catalog_of, host_platform, root_of, signed, test_key, ANY_CORE, WINDOW_END, WINDOW_START,
};
use plugin_harness::{path_str, Harness};
use responses_stub::ResponsesStub;

/// Directory of the package source, relative to the workspace root.
const PACKAGE_DIR: &str = "runtime-packages/codex";

/// Compatibility lock of the package, relative to the workspace root.
const LOCK_PATH: &str = "compat/codex/compatibility-lock.json";

/// Captured screens, relative to the workspace root.
const SCREENS_DIR: &str = "compat/codex/screens";

/// Built-in Codex descriptor, relative to the workspace root.
const BUILTIN_DESCRIPTOR: &str = "crates/daemon/src/agent/builtin/codex.toml";

/// Built-in Codex detection manifest, relative to the workspace root.
const BUILTIN_MANIFEST: &str = "crates/daemon/src/detect/manifests/codex.toml";

/// Package id the descriptor declares.
const PACKAGE_ID: &str = "pohunek.runtime.codex";

/// Version of the package the descriptor declares.
const PACKAGE_VERSION: &str = "1.0.0";

/// Shell program of the built-in source the parity tests load; Codex does not
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

/// The Codex definition compiled into the daemon.
fn builtin_definition() -> RuntimeDefinition {
    BuiltinSource::new(BUILTIN_SHELL)
        .load()
        .expect("the built-in definitions load")
        .into_iter()
        .find(|definition| definition.runtime_id().as_str() == "codex")
        .expect("the daemon embeds a Codex definition")
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

fn read_toml(relative: &str) -> toml::Table {
    let text = fs::read_to_string(workspace_root().join(relative)).expect("read the TOML file");
    text.parse().expect("the file is TOML")
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
        "the archive holds only descriptor and manifest; the hook reporter stays core-owned and evidence lives in compat/codex"
    );
}

/// Optional variable naming an archive built by `cargo xtask package build`.
///
/// The CI job sets it; the test then requires that archive and the in-process
/// build of the same directory to carry the same digest.
const ARCHIVE_VARIABLE: &str = "POHUNEK_CODEX_PACKAGE_ARCHIVE";

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
fn the_descriptor_declares_the_verified_codex_launch_contract() {
    let definition = installed_definition();
    assert_eq!(definition.runtime_id().as_str(), "codex");
    assert_eq!(definition.display_name(), "Codex");
    assert_eq!(
        definition.program(),
        &LaunchProgram::Fixed("codex".to_owned())
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
    assert!(rules.bracketed_paste, "Codex enables bracketed paste");
    assert_eq!(rules.submit_delay, Duration::from_millis(150));

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
        ["resume", reference.value()]
    );
    assert!(!native.supports_fork(), "Codex sessions are not forkable");
    native.fork_argv(&reference).expect_err("fork has no argv");

    assert_eq!(
        definition.integration_handler().map(HandlerId::as_str),
        Some("codex-hook-v1")
    );
    assert_eq!(
        definition.hook_schema().map(|schema| schema.id),
        Some("identity-subagent-v1")
    );
    let home = definition.config_home().expect("a config home is declared");
    assert_eq!(home.env(), "CODEX_HOME");
    assert_eq!(home.default_relative(), ".codex");
}

/// The package keeps every fact the built-in Codex descriptor carries; only
/// the manifest source and the version probe differ.
#[test]
fn the_package_descriptor_preserves_every_builtin_codex_fact() {
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
            "osc_title_blocked",
            "osc_title_working",
            "workspace_trust_prompt",
            "live_strong_blocker",
            "weak_blocker",
            "osc_title_idle",
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
fn the_version_probe_reads_the_codex_banner_and_nothing_else() {
    let definition = installed_definition();
    let policy = definition.version_probe_policy().expect("a version probe");
    let supported = |output: &str| {
        policy
            .read_output(output)
            .map(|version| policy.supports(version))
    };

    // The banner `codex --version` prints on stdout (stderr carries warnings
    // the probe does not read).
    assert_eq!(supported("codex-cli 0.160.0\n"), Some(true));
    assert_eq!(supported("codex-cli 0.160.9"), Some(true));
    assert_eq!(
        supported("codex-cli 0.159.9"),
        Some(false),
        "below the range"
    );
    assert_eq!(
        supported("codex-cli 0.161.0"),
        Some(false),
        "the next minor is outside the range"
    );
    assert_eq!(supported("codex-cli 1.0.0"), Some(false));

    for unreadable in [
        "",
        "0.160.0",
        "codex 0.160.0",
        "codex-cli 0.160.0-rc.1",
        "codex-cli 0.160",
        "codex-cli v0.160.0",
        "Codex CLI 0.160.0",
        "WARNING: proceeding\ncodex-cli 0.160.0",
    ] {
        assert_eq!(supported(unreadable), None, "{unreadable:?}");
    }
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
            "approval" | "askuser" => Self::Blocked,
            "trust" | "login" => Self::Silent,
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

/// One screen captured from a real Codex: `<state>.txt` at 100 columns, or
/// `widths/<state>_w<columns>.txt`, with the OSC title Codex had set in
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

/// Columns of the top-level fixtures.
const DEFAULT_COLUMNS: u16 = 100;

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
fn the_top_level_screens_cover_every_state() {
    let states: BTreeSet<String> = top_level_frames()
        .into_iter()
        .map(|frame| frame.state)
        .collect();
    assert_eq!(
        states.iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "approval",
            "askuser",
            "idle",
            "idle_after_turn",
            "login",
            "trust",
            "working_early",
            "working_stream",
        ]
    );
}

#[test]
fn every_captured_screen_is_classified_by_its_title() {
    let definition = installed_definition();
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        let expected = frame.meaning();
        match expected.activity() {
            Some(activity) => assert_eq!(
                frame.evidence(&definition),
                Some((activity, StateSource::OscTitle)),
                "{}: Codex reports idle, working and blocked through its title",
                frame.name
            ),
            None => assert_eq!(
                frame.evidence(&definition),
                None,
                "{}: nothing speaks for this screen",
                frame.name
            ),
        }
    }
}

#[test]
fn the_captured_screens_span_the_supported_terminal_widths() {
    let frames = width_frames();
    let widths: BTreeSet<u16> = frames.iter().map(|frame| frame.columns).collect();
    assert_eq!(
        widths.into_iter().collect::<Vec<_>>(),
        [20, 30, 40, 60, 80, 100, 120, 160, 200]
    );
    for state in [
        "idle",
        "idle_after_turn",
        "working_early",
        "working_stream",
        "approval",
        "trust",
    ] {
        let count = frames.iter().filter(|frame| frame.state == state).count();
        assert_eq!(count, 9, "{state} is captured at every width");
    }
    let titled = frames.iter().filter(|frame| frame.title.is_some()).count();
    assert!(titled >= 40, "titles captured with the screens: {titled}");
    assert!(
        frames
            .iter()
            .filter(|frame| frame.meaning() == Meaning::Silent)
            .all(|frame| frame.title.is_none()),
        "no title was emitted for the trust and login dialogs"
    );
}

/// The title alone decides idle, working and blocked: the rows carry no
/// working or idle evidence, so the same screens read with another title mean
/// what the title says.
#[test]
fn the_terminal_title_and_not_the_rows_decides_idle_working_and_blocked() {
    let definition = installed_definition();
    let idle = load_frame(
        &workspace_root().join(SCREENS_DIR),
        "idle",
        "idle",
        DEFAULT_COLUMNS,
    );
    for (title, expected) in [
        ("\u{2807} \u{2807} | project", AgentActivity::Working),
        ("[ ! ] Action Required | project", AgentActivity::Blocked),
        ("project", AgentActivity::Idle),
    ] {
        let seen = transitions(&definition, Some(title), &idle.rows, idle.columns);
        assert_eq!(
            seen.iter()
                .rfind(|transition| transition.source == StateSource::OscTitle)
                .map(|transition| transition.activity),
            Some(expected),
            "{title}"
        );
    }
}

/// A blocking prompt is also recognised from the screen text when the title has
/// not caught up. At 20 columns the confirmation line wraps and only the title
/// speaks.
#[test]
fn a_tool_approval_or_question_is_blocked_from_the_screen_alone_when_its_line_is_intact() {
    let definition = installed_definition();
    let mut checked = 0_usize;
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        if frame.meaning() != Meaning::Blocked {
            continue;
        }
        let expected =
            (frame.columns >= SCREEN_BLOCKER_MIN_COLUMNS).then_some(AgentActivity::Blocked);
        assert_eq!(
            screen_activity(&definition, &frame.rows, frame.columns),
            expected,
            "{}",
            frame.name
        );
        checked += 1;
    }
    assert!(checked >= 12, "approval and question screens: {checked}");
}

/// Narrowest captured width at which the confirmation line of the approval and
/// question prompts is still on one row; below it the line wraps and the screen
/// rule cannot match.
const SCREEN_BLOCKER_MIN_COLUMNS: u16 = 30;

#[test]
fn working_is_read_from_the_title_because_the_screen_has_no_working_rule() {
    let definition = installed_definition();
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        if matches!(frame.meaning(), Meaning::Working | Meaning::Idle) {
            assert_eq!(
                screen_activity(&definition, &frame.rows, frame.columns),
                None,
                "{}: the screen text alone is neither working nor idle",
                frame.name
            );
        }
    }
}

/// The real Codex 0.160.0 first-run folder dialog reads "Trust this folder?"
/// with "1. Trust and continue". The `workspace_trust_prompt` rule matches the
/// older wording, so the dialog is not classified; this test pins that gap so a
/// rule change that closes it must update the fixture expectations.
#[test]
fn the_real_trust_folder_dialog_is_not_yet_a_blocked_screen() {
    let definition = installed_definition();
    let frames: Vec<Frame> = top_level_frames()
        .into_iter()
        .chain(width_frames())
        .filter(|frame| frame.state == "trust")
        .collect();
    assert_eq!(frames.len(), 10);
    for frame in &frames {
        assert!(
            frame
                .rows
                .iter()
                .any(|row| row.contains("Trust this folder?") || row.contains("Trust this")),
            "{} shows the dialog",
            frame.name
        );
        assert_eq!(
            screen_activity(&definition, &frame.rows, frame.columns),
            None,
            "{}",
            frame.name
        );
    }
}

fn dialog(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_owned()).collect()
}

#[test]
fn the_workspace_trust_rule_matches_its_dialog_at_the_top_of_the_screen_only() {
    let definition = installed_definition();
    let trust = [
        "Do you trust the contents of this directory?",
        "\u{203a} 1. Yes, continue",
        "  2. No, quit",
    ];
    assert_eq!(
        screen_activity(&definition, &dialog(&trust), 80),
        Some(AgentActivity::Blocked)
    );
    let repository = ["  Do you trust this repository?", "  1. Yes", "  2. No"];
    assert_eq!(
        screen_activity(&definition, &dialog(&repository), 80),
        Some(AgentActivity::Blocked)
    );

    // Prose in a transcript that discusses trust is not the dialog: the rule
    // needs the question and both choices, in the first eight lines.
    let prose = [
        "Do you trust the contents of this directory?",
        "I would answer yes.",
    ];
    assert_eq!(screen_activity(&definition, &dialog(&prose), 80), None);
    let mut buried = vec![String::from("line"); 9];
    buried.extend(dialog(&trust));
    assert_eq!(screen_activity(&definition, &buried, 80), None);
}

#[test]
fn the_confirmation_rules_read_the_text_after_the_last_prompt_marker() {
    let definition = installed_definition();
    for phrase in [
        "Press enter to confirm or esc to cancel",
        "tab to add notes | enter to submit answer | esc to interrupt",
        "enter to submit all",
        "Allow command? 1. Yes",
    ] {
        let live = dialog(&["\u{203a} do it", phrase]);
        assert_eq!(
            screen_activity(&definition, &live, 80),
            Some(AgentActivity::Blocked),
            "{phrase}"
        );
        // Above the newest prompt marker the phrase is history.
        let history = dialog(&[phrase, "\u{203a} next", "reply"]);
        assert_eq!(
            screen_activity(&definition, &history, 80),
            None,
            "{phrase} before the newest prompt"
        );
    }
}

#[test]
fn the_weak_blocker_phrases_block_anywhere_in_the_recent_screen() {
    let definition = installed_definition();
    for screen in [
        &["Apply the patch? [y/n]"][..],
        &["Run it: yes (y)"],
        &["Do you want to proceed?", "\u{276f} 1. Yes"],
        &["Would you like to continue?", "yes or no"],
    ] {
        assert_eq!(
            screen_activity(&definition, &dialog(screen), 80),
            Some(AgentActivity::Blocked),
            "{screen:?}"
        );
    }
    for screen in [
        &["Do you want to proceed?"][..],
        &["would you like"],
        &["nothing to see"],
    ] {
        assert_eq!(
            screen_activity(&definition, &dialog(screen), 80),
            None,
            "{screen:?}"
        );
    }
}

/// Idle is read from the absence of evidence: any non-blank title that is not
/// a spinner and not "Action Required" means idle, including the project name
/// Codex sets when it starts.
#[test]
fn the_terminal_title_rules_read_idle_from_the_absence_of_a_spinner() {
    let definition = installed_definition();
    let working = Some(AgentActivity::Working);
    let idle = Some(AgentActivity::Idle);
    let blocked = Some(AgentActivity::Blocked);
    for (title, expected) in [
        ("\u{280b} project", working),
        ("\u{2834} \u{2834} | project", working),
        ("\u{28ff} ", working),
        ("[ ! ] Action Required | project", blocked),
        ("[ . ] Action Required | project", blocked),
        ("Action Required", blocked),
        // Blocked outranks working.
        ("\u{280b} Action Required", blocked),
        ("project", idle),
        ("Codex", idle),
        ("x \u{280b}", idle),
        ("done \u{2713}", idle),
        ("", None),
        ("   ", None),
    ] {
        assert_eq!(title_activity(&definition, title), expected, "{title:?}");
    }
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
fn the_process_matchers_accept_the_observed_codex_process_forms() {
    let definition = installed_definition();
    let matchers = definition
        .manifest()
        .process_matchers()
        .expect("the manifest declares process matchers");

    // The binary retitles itself `codex`; its helper process keeps the kernel
    // name `codex` with a long path as argv.
    assert!(matchers.matches(&process("codex", &["codex"])));
    assert!(matchers.matches(&process(
        "codex",
        &["/home/user/.codex/packages/app-server-daemon/releases/local-1"]
    )));
    for cmdline in [
        &["/usr/lib/openai-codex/bin/codex"][..],
        &["/usr/lib/openai-codex/bin/codex", "resume", "0197aaaa"],
        &["codex", "--no-alt-screen"],
        &["/home/user/.npm/bin/codex"],
    ] {
        assert!(
            matchers.matches(&process("node", cmdline)),
            "{cmdline:?} by command line"
        );
    }

    for (comm, cmdline) in [
        ("zsh", vec!["zsh", "-c", "codex"]),
        ("node", vec!["node", "/opt/codex-companion.mjs"]),
        ("python3", vec!["python3", "codex.py"]),
        ("codexbar", vec!["/usr/lib/codexbar-cli/codexbar", "usage"]),
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
    let synthetic: [&[&str]; 9] = [
        &[
            "Do you trust the contents of this directory?",
            "1. Yes, continue",
            "2. No",
        ],
        &["Do you trust this repository?", "1. Yes", "2. No"],
        &["\u{203a} go", "Press enter to confirm or esc to cancel"],
        &["\u{203a} go", "enter to submit answer"],
        &["\u{203a} go", "allow command?"],
        &["[y/n]"],
        &["Do you want to proceed?", "\u{276f} 1. Yes"],
        &["Would you like to go on?", "yes"],
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
        "[ ! ] Action Required | x",
        "x",
        "",
        "\u{280b} Action Required",
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

// The real-Codex tests below need a `codex` on PATH and an explicit opt-in. They
// are `#[ignore]`d and fail by name, never skip, when selected without either.

/// Opt-in variable of the real-Codex tests.
const E2E_VARIABLE: &str = "POHUNEK_CODEX_E2E";

/// Profile the real-Codex tests launch through.
const PROFILE: &str = "codex-test";

/// Feature switches that keep Codex from contacting anything but the stub:
/// plugin marketplace sync (a `git fetch`), connectors and in-app updates.
const OFFLINE_FEATURES: &str =
    "plugins = false\nremote_plugin = false\nplugin_sharing = false\napps = false\nin_app_updates = false";

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

/// Fails the test, naming the opt-in, unless a real `codex` is available.
///
/// The tests are `#[ignore]`d so a run without Codex skips them with that
/// reason; a run that selects them (`--ignored`, as the `codex-package` CI job
/// does) must have the opt-in variable and a `codex` on `PATH`, and fails
/// loudly otherwise.
fn require_real_codex() {
    assert_eq!(
        std::env::var(E2E_VARIABLE).as_deref(),
        Ok("1"),
        "the real-Codex tests need {E2E_VARIABLE}=1 and the pinned `codex` on PATH"
    );
    assert!(
        find_on_path("codex").is_some(),
        "{E2E_VARIABLE}=1 but no `codex` executable is on PATH"
    );
}

/// The banner the real `codex --version` prints on stdout, run with an empty
/// environment and a throwaway home so no real configuration is read.
fn real_codex_banner() -> String {
    let home = pohunek_test_support::tempdir().expect("tempdir");
    let path = std::env::var_os("PATH").expect("PATH");
    let output = std::process::Command::new("codex")
        .arg("--version")
        .env_clear()
        .env("PATH", path)
        .env("HOME", home.path())
        .env("CODEX_HOME", home.path().join(".codex"))
        .output()
        .expect("run `codex --version`");
    assert!(output.status.success(), "codex --version failed");
    String::from_utf8(output.stdout).expect("a UTF-8 banner")
}

#[test]
#[ignore = "needs a real `codex` on PATH; run with POHUNEK_CODEX_E2E=1 and --ignored"]
fn the_real_codex_banner_is_the_locked_release_the_probe_accepts() {
    require_real_codex();
    let definition = installed_definition();
    let policy = definition.version_probe_policy().expect("a version probe");
    let banner = real_codex_banner();
    let version = policy
        .read_output(&banner)
        .unwrap_or_else(|| panic!("the package template reads the real banner {banner:?}"));
    assert_eq!(version.to_string(), locked_release(), "the pinned release");
    assert!(policy.supports(version));
}

/// Terminates every process that belongs to one fixture.
///
/// A process belongs to the fixture when its executable, its working directory
/// or the value of one of its environment variables lies below the fixture's
/// root. That covers the session worker, Codex, and the detached `codex
/// app-server` Codex starts below `<home>/packages`, which outlives the TUI.
/// Dropping the guard reaps them, so an assertion failure or a timed-out wait
/// leaves no process behind; it must be dropped before the directories it
/// names are removed. The scan reads `/proc`, so it finds nothing elsewhere.
struct ProcessGuard {
    root: PathBuf,
}

impl ProcessGuard {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }

    /// Whether `directory`, a `/proc/<pid>` entry, holds a process of the
    /// fixture.
    fn owns(&self, directory: &Path) -> bool {
        let below_root = |link: &str| {
            fs::read_link(directory.join(link)).is_ok_and(|path| path.starts_with(&self.root))
        };
        if below_root("exe") || below_root("cwd") {
            return true;
        }
        fs::read(directory.join("environ")).is_ok_and(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter_map(|entry| {
                    let separator = entry.iter().position(|byte| *byte == b'=')?;
                    Some(&entry[separator + 1..])
                })
                .any(|value| Path::new(OsStr::from_bytes(value)).starts_with(&self.root))
        })
    }

    /// The live processes of the fixture, never the test process itself. A
    /// terminated process that is not yet reaped by its parent has no
    /// executable and does not count.
    fn members(&self) -> Vec<i32> {
        let Ok(processes) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let own = i32::try_from(std::process::id()).ok();
        processes
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
                (Some(pid) != own && self.owns(&entry.path())).then_some(pid)
            })
            .collect()
    }

    /// Kills the fixture's processes until none is left.
    ///
    /// # Panics
    ///
    /// Panics when a process survives until the hang guard of
    /// [`pohunek_test_support::wait::poll_until`] elapses.
    fn reap(&self) {
        poll_until("the fixture processes to end", || {
            let members = self.members();
            for pid in &members {
                // A process that exited since the scan is already gone.
                let _ = kill(Pid::from_raw(*pid), Signal::SIGKILL);
            }
            members.is_empty().then_some(())
        });
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        // A panic escaping a drop that runs during unwinding aborts the run.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.reap())).is_err() {
            eprintln!(
                "fixture processes survived the cleanup below {}",
                self.root.display()
            );
        }
    }
}

/// A real daemon with a hermetic Codex configuration pointing at a loopback
/// Responses stub, holding the official Codex package installed through a
/// signed catalog.
struct Fixture {
    /// Declared first so the fixture's processes end before the harness removes
    /// the directories they run in, on success and on unwind alike.
    processes: ProcessGuard,
    harness: Harness,
    stub: ResponsesStub,
    codex_home: PathBuf,
    /// Digest of the installed package archive.
    digest: PackageDigest,
}

impl Fixture {
    async fn start() -> Self {
        let key = test_key();
        let anchor =
            HostTrustAnchor::new(vec![root_of(&key, WINDOW_START, WINDOW_END)], Vec::new())
                .expect("the test trust anchor");
        let harness = Harness::start_with_trust(CatalogTrust::Loaded(anchor)).await;
        let processes = ProcessGuard::new(harness.env.root());
        let digest = install_package(&harness, &key).await;
        let stub = ResponsesStub::start();
        let codex_home = harness.env.root().join("codex-home");
        fs::create_dir_all(&codex_home).expect("create the Codex home");
        // Approval and sandbox policy are top-level keys and must precede the
        // provider table.
        fs::write(
            codex_home.join("config.toml"),
            format!(
                "model = \"{model}\"\nmodel_provider = \"stub\"\napproval_policy = \"on-request\"\nsandbox_mode = \"read-only\"\ncheck_for_update_on_startup = false\n\n[analytics]\nenabled = false\n\n[features]\n{offline_features}\n\n[model_providers.stub]\nname = \"stub\"\nbase_url = \"{base}\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n",
                model = responses_stub::MODEL_ID,
                base = stub.base_url(),
                offline_features = OFFLINE_FEATURES,
            ),
        )
        .expect("write config.toml");
        let fixture = Self {
            processes,
            harness,
            stub,
            codex_home,
            digest,
        };
        fixture.write_profile(PROFILE, None);
        fixture
    }

    /// Writes the profile `name`, pinned to the installed package and to the
    /// fixture's Codex home. `program` replaces the runtime's `codex`.
    fn write_profile(&self, name: &str, program: Option<&Path>) {
        let program = program.map_or_else(String::new, |program| {
            format!("program = \"{}\"\n", path_str(program))
        });
        self.harness.profile(
            name,
            &format!(
                "base = \"codex\"\npackage = \"{PACKAGE_ID}\"\ndigest = \"{digest}\"\n{program}\n[env]\nCODEX_HOME = \"{home}\"\n",
                digest = self.digest.as_str(),
                home = path_str(&self.codex_home),
            ),
        );
    }

    /// Installs the hook integration into the fixture's Codex home through the
    /// profile selector, the way an owner installs it: the daemon resolves the
    /// home exactly as a launch does, so the real home is never touched.
    async fn install_integration(&self) {
        let (code, installed) = self
            .harness
            .json(&[
                "integration",
                "install",
                "--agent",
                "codex",
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
        assert!(
            self.codex_home.join("hooks.json").is_file(),
            "the hook registration landed in the fixture home: {installed}"
        );
        let config = fs::read_to_string(self.codex_home.join("config.toml")).expect("config.toml");
        assert!(
            config.contains("trusted_hash"),
            "pohunek wrote the hook trust records Codex requires: {config}"
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

    /// The installed package is the official, selected source of `codex`, and
    /// the daemon read the real executable's version through the package's
    /// probe: a runtime without a probe reports no `supported` verdict.
    async fn assert_package_serves_codex(&self) {
        let (code, listed) = self.harness.json(&["plugin", "list"]).await;
        assert_eq!(code, 0, "{listed}");
        let packages = listed["ok"]["packages"].as_array().expect("packages");
        assert_eq!(packages.len(), 1, "{listed}");
        let package = &packages[0];
        assert_eq!(package["package"]["id"], PACKAGE_ID, "{listed}");
        assert_eq!(package["origin"], "official", "{listed}");
        assert_eq!(package["enabled"], true, "{listed}");
        assert_eq!(package["selected"], true, "{listed}");
        assert_eq!(package["digest"], self.digest.as_str(), "{listed}");

        let codex = self.inventory_entry(PROFILE).await;
        assert_eq!(codex["available"], true, "{codex}");
        assert_eq!(codex["supported"], true, "{codex}");
        assert_eq!(codex["version"], locked_release(), "{codex}");
    }

    /// `session new` of `profile` in the fixture's working directory.
    async fn launch(&self, profile: &str, name: &str) -> (i32, Value) {
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
            ])
            .await
    }

    async fn new_session(&self, name: &str) -> Value {
        let (code, launched) = self.launch(PROFILE, name).await;
        assert_eq!(code, 0, "{launched}");
        launched["ok"].clone()
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

    /// Waits until the session reports `activity` on the strength of the
    /// terminal title.
    async fn wait_title_evidence(&self, id: &str, activity: &str) {
        wait_until(&format!("{activity} from the title of {id}"), || async {
            let (code, inspected) = self.harness.json(&["session", "inspect", id]).await;
            assert_eq!(code, 0, "{inspected}");
            (inspected["ok"]["activity"] == activity
                && inspected["ok"]["state_source"] == "osc_title")
                .then_some(())
        })
        .await;
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

    /// The rollout file Codex wrote for conversation `reference`, below
    /// `sessions/YYYY/MM/DD/rollout-<time>-<reference>.jsonl`.
    fn rollout_file(&self, reference: &str) -> Option<PathBuf> {
        let suffix = format!("-{reference}.jsonl");
        let mut pending = vec![self.codex_home.join("sessions")];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(&suffix))
                {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Stops the session and waits until its runtime is no longer live.
    async fn stop(&self, id: &str) {
        let (code, stopped) = self.harness.json(&["session", "stop", id]).await;
        assert_eq!(code, 0, "{stopped}");
        wait_until(&format!("session {id} to stop"), || async {
            let (code, inspected) = self.harness.json(&["session", "inspect", id]).await;
            assert_eq!(code, 0, "{inspected}");
            (inspected["ok"]["runtime"]["state"] != "live").then_some(())
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
            "no request to the stub carried an Authorization header"
        );
    }
}

/// Installs the built package archive through a catalog signed by `key`, the
/// way a host with a catalog trust anchor installs an official package, and
/// returns the archive digest.
///
/// The catalog authorizes the package id, the runtime id `codex` and the
/// digest, which is what lets the package serve a reserved runtime id.
async fn install_package(harness: &Harness, key: &ed25519_dalek::SigningKey) -> PackageDigest {
    let (bytes, digest) = built_archive();
    let archive = harness.env.root().join("codex.tar.zst");
    fs::write(&archive, bytes).expect("write the archive");
    let entry = CatalogEntry {
        package_id: PackageId::parse(PACKAGE_ID).expect("package id"),
        runtime_id: RuntimeId::parse("codex").expect("runtime id"),
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

/// Drives a real `codex` through the installed official package with a fresh
/// `CODEX_HOME`, a loopback Responses stub and hook trust written by pohunek's
/// own integration install, and reads every live screen and title through the
/// package manifest.
///
/// The fixture installs the built archive through a signed catalog, so the
/// launch resolves the profile's pin to the package and runs the package's
/// version probe against the real executable before it starts.
///
/// Needs a `codex` on `PATH` and `POHUNEK_CODEX_E2E=1`; the model is a
/// loopback stub, so no provider or credential is involved, and the fixture's
/// configuration switches off update checks, analytics, plugin sync and
/// connectors.
#[tokio::test]
#[ignore = "needs a real `codex` on PATH; run with POHUNEK_CODEX_E2E=1 and --ignored"]
async fn a_real_codex_session_launches_and_is_detected_through_package_facts() {
    require_real_codex();
    let package = installed_definition();
    let fixture = Fixture::start().await;
    fixture.install_integration().await;

    fixture.assert_package_serves_codex().await;

    let launched = fixture.new_session("e2e").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    assert_eq!(launched["agent_base"], "codex");
    assert_eq!(launched["capabilities"]["resume"], true);
    assert_eq!(launched["capabilities"]["fork"], false);

    // Idle: the title Codex sets at startup, read by the daemon and the package.
    let idle = fixture.wait_activity(&id, "idle").await;
    assert_eq!(idle["state_source"], "osc_title", "{idle}");
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Idle).await;

    // Input: bracketed paste, then a separate submit after the descriptor's
    // delay. The stub holds the reply so the spinner title is on screen.
    let (code, sent) = fixture
        .harness
        .json(&[
            "session",
            "input",
            &id,
            &format!("{} {PROMPT}", responses_stub::HOLD_MARKER),
        ])
        .await;
    assert_eq!(code, 0, "{sent}");
    fixture.wait_activity(&id, "working").await;
    // The byte-activity fallback reports working first; the spinner title then
    // takes over as the evidence.
    fixture.wait_title_evidence(&id, "working").await;
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Working).await;

    fixture.stub.open_gate();
    fixture.wait_activity(&id, "idle").await;
    fixture
        .wait_screen_line(
            &id,
            &format!(
                "{}{}",
                responses_stub::FIRST_CHUNK,
                responses_stub::SECOND_CHUNK
            ),
        )
        .await;
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Idle).await;

    // The SessionStart hook reported the conversation id; it names Codex's own
    // rollout file, and it is what the daemon resumes with.
    let reference = wait_until("the hook-reported conversation id", || async {
        let (code, inspected) = fixture.harness.json(&["session", "inspect", &id]).await;
        assert_eq!(code, 0, "{inspected}");
        inspected["ok"]["active_agent_session_id"]
            .as_str()
            .map(str::to_owned)
    })
    .await;
    assert!(
        fixture.rollout_file(&reference).is_some(),
        "Codex wrote a rollout file for the reported conversation {reference}"
    );

    // Fork stays unsupported.
    let (code, forked) = fixture
        .harness
        .json(&["session", "fork", &id, "--name", "e2e-fork"])
        .await;
    assert_ne!(code, 0, "{forked}");

    fixture.stop(&id).await;
    fixture.finish().await;
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

/// Pins a gap of Codex 0.160.0 against the shared integration: Codex runs its
/// hooks from a `codex app-server` child of the process pohunek launched, so
/// the daemon records the conversation id as the nested active agent's and
/// never as the session's native reference, and `session resume` has nothing to
/// resume with.
///
/// When the daemon accepts the app-server child as the launch agent, this test
/// fails on its first assertion and is replaced by a resume test that drives
/// `codex resume <id>` through the package.
#[tokio::test]
#[ignore = "needs a real `codex` on PATH; run with POHUNEK_CODEX_E2E=1 and --ignored"]
async fn a_real_codex_reports_hooks_from_its_app_server_child_so_resume_has_no_reference() {
    require_real_codex();
    let fixture = Fixture::start().await;
    fixture.install_integration().await;

    let launched = fixture.new_session("hooks").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    fixture.wait_activity(&id, "idle").await;
    let (code, sent) = fixture
        .harness
        .json(&["session", "input", &id, PROMPT])
        .await;
    assert_eq!(code, 0, "{sent}");
    fixture
        .wait_screen_line(
            &id,
            &format!(
                "{}{}",
                responses_stub::FIRST_CHUNK,
                responses_stub::SECOND_CHUNK
            ),
        )
        .await;

    let reported = wait_until("the hook-reported conversation id", || async {
        let (code, inspected) = fixture.harness.json(&["session", "inspect", &id]).await;
        assert_eq!(code, 0, "{inspected}");
        inspected["ok"]["active_agent_session_id"]
            .is_string()
            .then(|| inspected["ok"].clone())
    })
    .await;
    let launch_pid = reported["pid"].as_u64().expect("the launch process id");
    let reporter_pid = reported["active_agent_pid"]
        .as_u64()
        .expect("the reporting process id");
    assert_ne!(
        reporter_pid, launch_pid,
        "Codex 0.160.0 runs hooks outside the launched process"
    );
    assert_eq!(
        parent_pid(reporter_pid),
        launch_pid,
        "the reporting process is the app-server child of the launched process"
    );
    assert!(
        reported["native_session_id"].is_null(),
        "no native reference is recorded for a nested reporter: {reported}"
    );

    fixture.stop(&id).await;
    let (code, refused) = fixture.harness.json(&["session", "resume", &id]).await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "not_resumable", "{refused}");
    fixture.finish().await;
}

/// A tool approval prompt of the real Codex is blocked through the title and
/// through the screen, for the daemon and for the package manifest.
#[tokio::test]
#[ignore = "needs a real `codex` on PATH; run with POHUNEK_CODEX_E2E=1 and --ignored"]
async fn a_real_codex_approval_prompt_is_blocked_by_title_and_screen() {
    require_real_codex();
    let package = installed_definition();
    let fixture = Fixture::start().await;
    fixture.install_integration().await;

    let launched = fixture.new_session("approval").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    fixture.wait_activity(&id, "idle").await;

    let (code, sent) = fixture
        .harness
        .json(&[
            "session",
            "input",
            &id,
            &format!("{} run it", responses_stub::APPROVAL_MARKER),
        ])
        .await;
    assert_eq!(code, 0, "{sent}");
    let blocked = fixture.wait_activity(&id, "blocked").await;
    assert_eq!(blocked["state_source"], "osc_title", "{blocked}");
    let rows = fixture
        .wait_screen_line(&id, "Would you like to run the following command?")
        .await;
    assert!(
        rows.iter()
            .any(|row| row.contains(responses_stub::APPROVAL_COMMAND)),
        "the prompt names the command: {rows:#?}"
    );
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Blocked).await;
    assert_eq!(
        screen_activity(&package, &rows, DEFAULT_COLUMNS),
        Some(AgentActivity::Blocked),
        "the screen text alone is blocked too"
    );

    fixture.stop(&id).await;
    fixture.finish().await;
}

/// `codex` is a reserved runtime id: an install trusted only by its digest is
/// refused, so the package serves it through a signed catalog alone.
#[tokio::test]
async fn installing_the_package_is_refused_while_codex_is_a_reserved_builtin() {
    let harness = Harness::start().await;
    let (bytes, digest) = built_archive();
    let archive = harness.env.root().join("codex.tar.zst");
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

/// Writes an executable `codex` into a directory of its own below `root` that
/// answers `--version` with `banner` and otherwise idles, and returns its path.
fn write_fake_codex(root: &Path, directory: &str, banner: &str) -> PathBuf {
    let path = root.join(directory).join("codex");
    fs::create_dir_all(root.join(directory)).expect("create the fake codex directory");
    write_executable(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo '{banner}'\n  exit 0\nfi\nexec sleep 600\n"),
    )
    .expect("write the fake codex");
    path
}

/// The version probe the package declares decides every launch of `codex`, on
/// the executable the profile names: only the supported release starts.
#[tokio::test]
async fn the_package_version_probe_refuses_an_unsupported_codex_and_starts_the_supported_one() {
    let fixture = Fixture::start().await;
    let root = fixture.harness.env.root().to_path_buf();
    let release = locked_release();
    let unsupported = [
        "codex-cli 0.159.9".to_owned(),
        "codex-cli 0.161.0".to_owned(),
        format!("codex-cli {release}-rc.1"),
        format!("codex {release}"),
        "unreadable".to_owned(),
    ];
    for (index, banner) in unsupported.iter().enumerate() {
        let profile = format!("fake-unsupported-{index}");
        let program = write_fake_codex(&root, &profile, banner);
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

    let banner = format!("codex-cli {release}");
    let program = write_fake_codex(&root, "fake-supported", &banner);
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
    assert_eq!(launched["ok"]["agent_base"], "codex");
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

/// Starts a detached `sleep` in `env`, the way Codex leaves its app-server
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
