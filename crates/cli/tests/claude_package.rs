//! The official Claude Code runtime package (`runtime-packages/claude`).
//!
//! Pure tests always run: they build the package directory, parse it through
//! the same path `plugin install` uses, compare it with the built-in Claude
//! descriptor and detection manifest release v0.34.2 shipped (golden copies in
//! `compat/claude/builtin-v0.34`; the manifest differs by the enumerated #613
//! rules), and check it against the compatibility lock
//! and the screens captured from a real Claude Code (`compat/claude/screens`).
//! The daemon-backed tests install the built archive through `pohunek plugin
//! install --catalog` against a throwaway signing key and trust anchor, so the
//! package serves `claude` with official trust; the daemon compiles no Claude
//! in, so nothing else serves it. The real-Claude tests
//! additionally drive an actual `claude` binary against a loopback Messages
//! stub with a throwaway home; see their documentation for the opt-in.

// Rust guideline compliant 2026-10-12

#![cfg(unix)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use package::directory::build_directory_archive;
use package::{read_archive, CatalogEntry, Limits, PackageDigest};
use pohunek_daemon::agent::host::{
    definition_from_archive, DefinitionError, DefinitionOrigin, HandlerId, InputReadiness,
    LaunchProgram, ProbeVersion, RuntimeDefinition, TranscriptParser,
};
use pohunek_daemon::agent::{NativeReferenceStrategy, SessionRef};
use pohunek_daemon::catalog_anchor::CatalogTrust;
use pohunek_daemon::detect::{
    ActivityTransition, DetectionConfig, Detector, DetectorConfig, Manifest, ManifestRegion,
    MatchContext,
};
use pohunek_daemon::procwatch::{ProcessFact, StartIdentity};
use pohunek_daemon::session::HostTrustAnchor;
use pohunek_test_support::fs::write_executable;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait::{wait_until, HANG_GUARD};
use pohunek_test_support::workspace_root;
use protocol::{
    AgentActivity, BindingProvenance, PackageId, PackageVersion, RuntimeId, StateSource,
};
use serde_json::Value;

#[path = "support/capture.rs"]
mod capture;
#[path = "support/messages_stub.rs"]
mod messages_stub;
#[path = "support/plugin_harness.rs"]
mod plugin_harness;
#[path = "support/process_guard.rs"]
mod process_guard;

use messages_stub::MessagesStub;
use plugin_harness::{path_str, Harness};
use pohunek_test_support::catalog::{
    attestations_for, catalog_of, host_platform, root_of, signed, test_key, ANY_CORE, RUNTIME_API,
    WINDOW_END, WINDOW_START,
};
use process_guard::{ChildGuard, ProcessGuard};

/// Directory of the package source, relative to the workspace root.
const PACKAGE_DIR: &str = "runtime-packages/claude";

/// Compatibility lock of the package, relative to the workspace root.
const LOCK_PATH: &str = "compat/claude/compatibility-lock.json";

/// Captured screens, relative to the workspace root.
const SCREENS_DIR: &str = "compat/claude/screens";

/// Where release [`GOLDEN_TAG`] kept the built-in Claude descriptor, relative
/// to the workspace root; the golden's header names it.
const BUILTIN_DESCRIPTOR: &str = "crates/daemon/src/agent/builtin/claude.toml";

/// Where release [`GOLDEN_TAG`] kept the built-in Claude detection manifest,
/// relative to the workspace root; the golden's header names it.
const BUILTIN_MANIFEST: &str = "crates/daemon/src/detect/manifests/claude.toml";

/// Release whose built-in Claude descriptor and manifest the goldens copy.
const GOLDEN_TAG: &str = "v0.34.2";

/// Golden copy of [`BUILTIN_DESCRIPTOR`] at [`GOLDEN_TAG`], relative to the
/// workspace root.
const GOLDEN_DESCRIPTOR: &str = "compat/claude/builtin-v0.34/runtime.toml";

/// Golden copy of [`BUILTIN_MANIFEST`] at [`GOLDEN_TAG`], relative to the
/// workspace root.
const GOLDEN_MANIFEST: &str = "compat/claude/builtin-v0.34/detect.toml";

/// Name the golden descriptor's `detect_manifest` gives the built-in manifest.
const GOLDEN_MANIFEST_NAME: &str = "claude";

/// Package id the descriptor declares.
const PACKAGE_ID: &str = "pohunek.runtime.claude";

/// Version of the package the descriptor declares.
const PACKAGE_VERSION: &str = "1.0.2";

/// Package version the golden built-in descriptor declares.
const GOLDEN_PACKAGE_VERSION: &str = "1.0.0";

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

/// Contents of the golden file `golden` below its header.
///
/// The header names `source`, the file the golden copies, and [`GOLDEN_TAG`];
/// the rest of the file is that source's bytes at the tag.
fn golden_body(golden: &str, source: &str) -> String {
    let text = fs::read_to_string(workspace_root().join(golden)).expect("read the golden");
    let header = format!(
        "# Golden copy of {source} at tag {GOLDEN_TAG}.\n\
         # Do not edit: the parity tests compare the runtime package with these bytes.\n\n"
    );
    text.strip_prefix(&header)
        .unwrap_or_else(|| panic!("{golden} starts with its header naming {source}"))
        .to_owned()
}

/// The built-in Claude definition release v0.34.2 shipped, parsed from the
/// goldens the way the daemon parses its embedded descriptors.
fn golden_definition() -> RuntimeDefinition {
    let manifest = Manifest::parse_str(&golden_body(GOLDEN_MANIFEST, BUILTIN_MANIFEST))
        .expect("the golden manifest parses");
    RuntimeDefinition::from_toml(
        &golden_body(GOLDEN_DESCRIPTOR, BUILTIN_DESCRIPTOR),
        |package| DefinitionOrigin::Builtin {
            package: Some(package),
        },
        |name| {
            if name == GOLDEN_MANIFEST_NAME {
                Ok(Arc::new(manifest))
            } else {
                Err(DefinitionError::UnknownManifest)
            }
        },
    )
    .expect("the golden descriptor parses")
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

fn detector_for(definition: &RuntimeDefinition, now: Instant, columns: u16) -> Detector {
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
            manifest: Some(Arc::clone(definition.manifest())),
            input_readiness: definition.input_readiness(),
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
    let mut detector = detector_for(definition, started, columns);
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
    let mut detector = detector_for(definition, started, 80);
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

/// What a captured screen means: Claude waits for a prompt, runs a turn, or
/// waits for an answer to a dialog (tool approval, question, first-run and
/// folder trust dialogs).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Meaning {
    Idle,
    Working,
    Blocked,
}

impl Meaning {
    fn of(state: &str) -> Self {
        match state {
            "idle" | "idle_after_turn" => Self::Idle,
            "working_early" | "working_star_frame" | "working_stream" => Self::Working,
            "approval" | "askuser" | "apikey" | "theme" | "login" | "trust" => Self::Blocked,
            other => panic!("unknown fixture state {other}"),
        }
    }

    fn activity(self) -> AgentActivity {
        match self {
            Self::Idle => AgentActivity::Idle,
            Self::Working => AgentActivity::Working,
            Self::Blocked => AgentActivity::Blocked,
        }
    }
}

/// First-run and folder trust dialogs: Claude shows them before it opens its
/// prompt and sets no title while they are on screen.
const FIRST_RUN_STATES: [&str; 4] = ["apikey", "theme", "login", "trust"];

/// One screen captured from a real Claude Code: `<state>.txt` at 100 columns, or
/// `widths/<state>_w<columns>.txt`, with the OSC title Claude had set in
/// `<same name>.title` when the capture had one.
struct Frame {
    set: ScreenSet,
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

/// The two release sets of captured screens: the locked release and the
/// previous verified release (`n-1`), which stays in the repository so the
/// bottom of the supported range remains evidenced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScreenSet {
    Current,
    Previous,
}

impl ScreenSet {
    const ALL: [Self; 2] = [Self::Current, Self::Previous];

    fn directory(self) -> PathBuf {
        let current = workspace_root().join(SCREENS_DIR);
        match self {
            Self::Current => current,
            Self::Previous => current.join("n-1"),
        }
    }
}

fn load_frame(set: ScreenSet, name: &str, state: &str, columns: u16) -> Frame {
    load_frame_from(set, &set.directory(), name, state, columns)
}

fn load_frame_from(
    set: ScreenSet,
    directory: &Path,
    name: &str,
    state: &str,
    columns: u16,
) -> Frame {
    let rows = rows_of(
        &fs::read_to_string(directory.join(format!("{name}.txt"))).expect("read the screen"),
    );
    let title = fs::read_to_string(directory.join(format!("{name}.title")))
        .ok()
        .map(|title| title.trim_end_matches('\n').to_owned());
    Frame {
        set,
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

/// The screens of `set` captured at 100 columns, one per state.
fn top_level_frames_of(set: ScreenSet) -> Vec<Frame> {
    let directory = set.directory();
    fixture_names(&directory)
        .into_iter()
        .map(|name| load_frame_from(set, &directory, &name, &name, DEFAULT_COLUMNS))
        .collect()
}

/// The 100-column screens of both release sets.
fn top_level_frames() -> Vec<Frame> {
    ScreenSet::ALL
        .into_iter()
        .flat_map(top_level_frames_of)
        .collect()
}

/// Every screen of `set` captured at a given terminal width. The file name is
/// `<state>_w<columns>.txt`.
fn width_frames_of(set: ScreenSet) -> Vec<Frame> {
    let directory = set.directory().join("widths");
    fixture_names(&directory)
        .into_iter()
        .map(|name| {
            let (state, columns) = name.rsplit_once("_w").expect("a `_w<columns>` suffix");
            let columns = columns.parse().expect("a column count");
            let state = state.to_owned();
            load_frame_from(set, &directory, &name, &state, columns)
        })
        .collect()
}

/// The width screens of both release sets.
fn width_frames() -> Vec<Frame> {
    ScreenSet::ALL
        .into_iter()
        .flat_map(width_frames_of)
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
        definition.input_readiness(),
        InputReadiness::ScreenIdle,
        "the prompt box, not the bracketed-paste enable, shows readiness"
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
        native.fork_argv(&reference).expect("fork").argv,
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
    let transcripts = definition
        .transcripts()
        .expect("a transcript tree is declared");
    assert_eq!(transcripts.parser(), TranscriptParser::JsonlHeadV1);
    assert_eq!(
        transcripts.dir(),
        "projects",
        "Claude keeps its transcripts below `projects`"
    );
}

/// The package keeps every fact the built-in Claude descriptor of release
/// v0.34.2 carries; only the manifest source, the version probe, the transcript
/// tree and the input readiness, which that descriptor does not declare, differ.
#[test]
fn the_package_descriptor_preserves_every_builtin_claude_fact() {
    let package = installed_definition();
    let golden = golden_definition();

    assert_eq!(package.runtime_id(), golden.runtime_id());
    assert_eq!(package.display_name(), golden.display_name());
    assert_eq!(package.program(), golden.program());
    assert_eq!(package.default_args(), golden.default_args());
    assert_eq!(package.input_rules(), golden.input_rules());
    assert_eq!(
        package.submit_delay_configurable(),
        golden.submit_delay_configurable()
    );
    assert_eq!(package.prompt_arg(), golden.prompt_arg());
    assert_eq!(package.native(), golden.native());
    assert_eq!(
        package.native_reference_strategy(),
        golden.native_reference_strategy()
    );
    assert_eq!(package.integration_handler(), golden.integration_handler());
    assert_eq!(package.hook_schema(), golden.hook_schema());
    assert_eq!(package.config_home(), golden.config_home());
    assert!(
        golden.transcripts().is_none(),
        "v0.34.2 declares no transcript tree"
    );
    assert!(package.transcripts().is_some());

    assert!(
        golden.version_probe_policy().is_none(),
        "v0.34.2 declares no probe"
    );
    assert!(package.version_probe_policy().is_some());
}

/// The same facts as data: the package descriptor and the golden built-in
/// descriptor are equal tables once the package version, the manifest
/// reference, the version probe, the transcript tree and the input readiness,
/// the fields that differ by design, are removed. The package version is above
/// the golden's because the package manifest carries the #613 rules (see
/// [`the_package_manifest_file_is_the_golden_manifest_with_the_613_rules`]).
#[test]
fn the_package_descriptor_file_equals_the_builtin_descriptor_file() {
    let strip = |mut table: toml::Table, version: &str| {
        assert_eq!(
            table
                .remove("version")
                .as_ref()
                .and_then(toml::Value::as_str),
            Some(version)
        );
        let runtime = table
            .get_mut("runtime")
            .and_then(toml::Value::as_table_mut)
            .expect("a [runtime] table");
        assert!(runtime.remove("detect_manifest").is_some());
        runtime.remove("version_probe");
        table
    };
    let mut package = strip(
        read_toml(&format!("{PACKAGE_DIR}/runtime.toml")),
        PACKAGE_VERSION,
    );
    for section in [
        "input",
        "resume",
        "fork",
        "native_reference",
        "integration",
        "config_home",
        "transcripts",
    ] {
        assert!(package.contains_key(section), "[{section}] is declared");
    }
    package.remove("transcripts");
    // `ready` is the one key the package declares that v0.34.2 does not: the
    // built-in Claude descriptor relied on the bracketed-paste enable signal.
    let input = package
        .get_mut("input")
        .and_then(toml::Value::as_table_mut)
        .expect("an [input] table");
    assert_eq!(
        input.remove("ready").as_ref().and_then(toml::Value::as_str),
        Some("screen_idle"),
        "Claude's prompt box is its only input readiness signal"
    );
    let golden_text = golden_body(GOLDEN_DESCRIPTOR, BUILTIN_DESCRIPTOR);
    let golden: toml::Table = golden_text.parse().expect("the golden is TOML");
    assert!(
        !golden.contains_key("transcripts"),
        "v0.34.2 declares no transcript tree"
    );
    assert!(
        golden
            .get("input")
            .and_then(toml::Value::as_table)
            .is_some_and(|input| !input.contains_key("ready")),
        "v0.34.2 declares no input readiness"
    );
    assert_eq!(package, strip(golden, GOLDEN_PACKAGE_VERSION));
}

/// Rules of the golden manifest the package changes for #613, each with the
/// matcher fields the change replaces; every other field stays equal.
const CHANGED_613_RULES: [(&str, &[&str]); 4] = [
    // The spinner line is one verb and an optional elapsed timer (#1524).
    ("live_status_spinner", &["line_regex"]),
    // The `◐`/`◑` running-turn title.
    ("osc_title_working", &["regex"]),
    // The selection form footer wrapped at narrow widths.
    ("live_blocked_form", &["contains", "regex", "any"]),
    // The ` · done <time>` suffix of the completed-turn line.
    ("completed_turn_status", &["line_regex"]),
];

/// Rules the package adds for #613, each with the rule it follows.
const ADDED_613_RULES: [(&str, &str); 4] = [
    // The `esc to interrupt` footer of a running turn.
    ("live_turn_footer", "live_status_spinner"),
    // The first-run and folder trust dialogs.
    ("theme_picker", "dynamic_workflow_prompt"),
    ("login_method_list", "theme_picker"),
    ("folder_trust_dialog", "login_method_list"),
];

/// The id of a manifest rule table.
fn rule_id(rule: &toml::Value) -> &str {
    rule["id"].as_str().expect("rule id")
}

/// A rule table without `fields`.
fn without(rule: &toml::Value, fields: &[&str]) -> toml::Table {
    let mut table = rule.as_table().expect("a rule table").clone();
    for field in fields {
        table.remove(*field);
    }
    table
}

/// The manifest file is the golden built-in manifest of release v0.34.2 with
/// exactly the #613 changes: the `[process]` matchers (the npm `claude.exe`
/// accepted, a script that merely names `claude` refused), the matcher fields
/// of [`CHANGED_613_RULES`] and the rules of [`ADDED_613_RULES`] at their
/// places. Every other rule keeps its golden fields and order, so drift beyond
/// the enumerated changes fails here.
#[test]
fn the_package_manifest_file_is_the_golden_manifest_with_the_613_rules() {
    let package = read_toml(&format!("{PACKAGE_DIR}/detect.toml"));
    let golden_text = golden_body(GOLDEN_MANIFEST, BUILTIN_MANIFEST);
    let golden: toml::Table = golden_text.parse().expect("the golden is TOML");

    let mut package = package;
    let program_status = package
        .remove("program_status")
        .expect("the manifest opts in to program status");
    assert_eq!(
        program_status,
        toml::Value::Table(toml::Table::from_iter([
            ("app".to_owned(), "claude-code".into()),
            ("answer_probe".to_owned(), true.into()),
        ]))
    );
    assert_eq!(
        package.keys().collect::<Vec<_>>(),
        golden.keys().collect::<Vec<_>>()
    );
    assert_ne!(package["process"], golden["process"], "[process] changes");
    assert_eq!(
        package["process"]
            .as_table()
            .expect("[process]")
            .keys()
            .collect::<Vec<_>>(),
        ["cmdline", "comm"]
    );

    let golden_rules = golden["rules"].as_array().expect("golden rules");
    let package_rules = package["rules"].as_array().expect("package rules");
    let mut expected_ids: Vec<&str> = golden_rules.iter().map(rule_id).collect();
    for (added, after) in ADDED_613_RULES {
        let place = expected_ids
            .iter()
            .position(|id| *id == after)
            .unwrap_or_else(|| panic!("{added} follows the rule {after}"));
        expected_ids.insert(place + 1, added);
    }
    assert_eq!(
        package_rules.iter().map(rule_id).collect::<Vec<_>>(),
        expected_ids
    );

    for golden_rule in golden_rules {
        let id = rule_id(golden_rule);
        let package_rule = package_rules
            .iter()
            .find(|rule| rule_id(rule) == id)
            .expect("the golden rule is kept");
        match CHANGED_613_RULES.iter().find(|(changed, _)| *changed == id) {
            Some((_, fields)) => {
                assert_ne!(package_rule, golden_rule, "{id} carries its #613 change");
                assert_eq!(
                    without(package_rule, fields),
                    without(golden_rule, fields),
                    "{id} changes only {fields:?}"
                );
            }
            None => assert_eq!(package_rule, golden_rule, "{id} equals the golden rule"),
        }
    }
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
        release > policy.min(),
        "the range starts at the previous verified release"
    );
    assert_eq!(
        screens_banner_release(&workspace_root().join(SCREENS_DIR)),
        release.to_string(),
        "the captured screens come from the locked release"
    );
    assert_eq!(
        screens_banner_release(&workspace_root().join(SCREENS_DIR).join("n-1")),
        policy.min().to_string(),
        "the previous release's screens stay in the repository"
    );
    assert!(
        policy.supports(release),
        "the locked release lies in the supported range"
    );
    assert!(
        !policy.supports(policy.below()),
        "the first release above the range is unsupported"
    );
    let next_major = policy
        .min()
        .to_string()
        .split('.')
        .next()
        .and_then(|major| major.parse::<u64>().ok())
        .expect("the minimum's major release")
        + 1;
    assert_eq!(
        policy.below().to_string(),
        format!("{next_major}.0.0"),
        "the range runs from the verified minimum to the next major release"
    );
}

/// The release in the `Claude Code v<release>` banner of a screen set.
fn screens_banner_release(directory: &Path) -> String {
    let idle = fs::read_to_string(directory.join("idle.txt")).expect("the idle screen");
    let banner = idle
        .lines()
        .find_map(|line| line.split_once("Claude Code v").map(|(_, rest)| rest))
        .expect("the banner row");
    banner
        .split_whitespace()
        .next()
        .expect("a release after the banner")
        .to_owned()
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
        Some(true),
        "a later 2.x release is inside the range"
    );
    assert_eq!(
        supported("3.0.0 (Claude Code)"),
        Some(false),
        "the next major is outside the range"
    );
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
    let states = |set| -> Vec<String> {
        top_level_frames_of(set)
            .into_iter()
            .map(|frame| frame.state)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let previous = [
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
    ];
    assert_eq!(states(ScreenSet::Previous), previous);
    let mut current = previous.to_vec();
    current.push("working_star_frame");
    current.sort_unstable();
    assert_eq!(states(ScreenSet::Current), current);
}

/// What the package manifest reads from every captured screen: from the screen
/// text alone, and with the title Claude had set. The screen reads the state of
/// every capture except a streaming reply too narrow for its turn footer
/// ([`TURN_FOOTER_UNREAD_WIDTHS`]); the running-turn title outranks the screen,
/// so with its title every capture reads its state.
#[test]
fn every_captured_screen_is_classified_by_its_screen_text_and_title() {
    let definition = installed_definition();
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        let meant = frame.meaning().activity();
        let from_screen = if frame.state == "working_stream"
            && TURN_FOOTER_UNREAD_WIDTHS.contains(&frame.columns)
        {
            AgentActivity::Idle
        } else {
            meant
        };
        assert_eq!(
            screen_activity(&definition, &frame.rows, frame.columns),
            Some(from_screen),
            "{} from the screen",
            frame.name
        );
        let glyph = frame
            .title
            .as_deref()
            .is_some_and(|title| title.starts_with(TURN_GLYPHS));
        let (titled, source) = if glyph {
            (meant, StateSource::OscTitle)
        } else {
            (from_screen, StateSource::Screen)
        };
        assert_eq!(
            frame.evidence(&definition),
            Some((titled, source)),
            "{} with title {:?}",
            frame.name,
            frame.title
        );
    }
}

/// Captured widths at which the footer of a running turn is cut before its
/// `esc to interrupt` hint (`⏸ manual mode on · esc to…` at 30 columns, the
/// mode alone at 20). A streaming reply hides the status line, so at these
/// widths its screen reads as the idle prompt box and only the running-turn
/// title says the turn runs.
const TURN_FOOTER_UNREAD_WIDTHS: [u16; 2] = [20, 30];

/// The glyphs Claude Code 2.1.289 alternates before its title while a turn
/// runs.
const TURN_GLYPHS: [char; 2] = ['\u{25d0}', '\u{25d1}'];

#[test]
fn the_captured_screens_span_the_supported_terminal_widths() {
    for set in ScreenSet::ALL {
        spans_the_supported_widths(set);
    }
}

fn spans_the_supported_widths(set: ScreenSet) {
    let frames = width_frames_of(set);
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
    assert!(
        titled >= 40,
        "titles captured with the screens of {set:?}: {titled}"
    );
    assert!(
        frames
            .iter()
            .filter(|frame| FIRST_RUN_STATES.contains(&frame.state.as_str()))
            .all(|frame| frame.title.is_none()),
        "no title was emitted for the first-run and trust dialogs"
    );
}

/// Claude Code 2.1.289 sets `✳ Claude Code` while it waits for input, also
/// while a tool approval or question is on screen, and a rotating
/// circle-quadrant glyph (`◐ ◑ …`) while a turn runs.
#[test]
fn the_captured_titles_are_the_idle_asterisk_and_the_working_circle() {
    let previous = ScreenSet::Previous;
    let titles: BTreeSet<String> = top_level_frames_of(previous)
        .into_iter()
        .chain(width_frames_of(previous))
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

/// Once the terminal answered Claude's program-status probe, Claude 2.1.309
/// sets the plain title `Claude Code` in every state and leaves the state to
/// the OSC 7501 records. The title rules read nothing from it.
#[test]
fn a_probe_answered_claude_sets_a_plain_title_in_every_state() {
    let definition = installed_definition();
    let current = ScreenSet::Current;
    let frames: Vec<Frame> = top_level_frames_of(current)
        .into_iter()
        .chain(width_frames_of(current))
        .filter(|frame| frame.title.is_some())
        .collect();
    assert!(frames.len() >= 40, "titled captures: {}", frames.len());
    for frame in frames {
        assert_eq!(
            frame.title.as_deref(),
            Some("Claude Code"),
            "{}",
            frame.name
        );
    }
    assert_eq!(title_activity(&definition, "Claude Code"), None);
}

/// The title alone reads idle for the asterisk and working for the two glyphs
/// Claude 2.1.289 alternates while a turn runs, as well as for a braille
/// spinner. The other circle quadrants, a bare name and an empty title are not
/// a Claude state.
#[test]
fn the_title_reads_idle_for_the_asterisk_and_working_for_the_turn_glyphs() {
    let definition = installed_definition();
    for (title, expected) in [
        ("\u{2733} Claude Code", Some(AgentActivity::Idle)),
        ("\u{280b} Claude Code", Some(AgentActivity::Working)),
        ("\u{25d0} Claude Code", Some(AgentActivity::Working)),
        ("\u{25d1} Claude Code", Some(AgentActivity::Working)),
        ("\u{25d2} Claude Code", None),
        ("\u{25d3} Claude Code", None),
        ("\u{25d0}Claude Code", None),
        ("Claude Code", None),
        ("", None),
    ] {
        assert_eq!(title_activity(&definition, title), expected, "{title:?}");
    }
}

/// A streaming reply of 2.1.289 shows its text above the prompt box with no
/// status line, while the title shows a running-turn glyph and the footer reads
/// `esc to interrupt`. Drawn with its title, every captured streaming screen
/// reads working. A daemon sees the reply re-rendered between title changes
/// (the title alternates every 960 ms), so the screen alone must say working
/// too: it does through the turn footer, except at the widths that cut the
/// footer, where the re-render reads idle until the next title change.
#[test]
fn a_streaming_reply_reads_working_from_its_title_and_turn_footer() {
    let definition = installed_definition();
    let previous = ScreenSet::Previous;
    for frame in top_level_frames_of(previous)
        .into_iter()
        .chain(width_frames_of(previous))
        .filter(|frame| frame.state == "working_stream")
    {
        let title = frame.title.as_deref().expect("a working title");
        assert!(title.starts_with(TURN_GLYPHS), "{title}");
        assert_eq!(
            live_activity(&definition, title, &frame.rows, frame.columns),
            Some(AgentActivity::Working),
            "{}",
            frame.name
        );

        // The next token re-renders the reply without a new title.
        let mut rerendered = frame.rows.clone();
        let reply = rerendered
            .iter_mut()
            .rfind(|row| row.starts_with("\u{25cf} "))
            .unwrap_or_else(|| panic!("{} shows the reply", frame.name));
        reply.push('.');
        let started = Instant::now();
        let mut detector = detector_for(&definition, started, frame.columns);
        let mut seen = detector.feed(started, &frame_bytes(Some(title), &frame.rows));
        seen.extend(detector.feed(started, &frame_bytes(None, &rerendered)));
        let published = seen.last().map(|transition| transition.activity);
        let expected = if TURN_FOOTER_UNREAD_WIDTHS.contains(&frame.columns) {
            AgentActivity::Idle
        } else {
            AgentActivity::Working
        };
        assert_eq!(published, Some(expected), "{} re-rendered", frame.name);
    }
}

/// The glyphs that lead Claude's animated status line.
const STATUS_GLYPHS: [char; 7] = [
    '\u{b7}', '\u{2722}', '\u{2733}', '\u{2736}', '\u{273b}', '\u{273d}', '*',
];

/// While a turn runs, the status line (`✶ Billowing… (0s · ↓ 2 tokens)`) makes
/// the screen working on its own, outranking the visible prompt box; the turn
/// footer below the prompt box says the same.
#[test]
fn the_status_spinner_line_makes_a_running_turn_working() {
    let definition = installed_definition();
    let early = load_frame(
        ScreenSet::Current,
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
    let mut without_footer = early.rows.clone();
    without_footer.retain(|row| !row.contains("esc to interrupt"));
    assert_ne!(
        without_footer, early.rows,
        "the capture shows the turn footer"
    );
    assert_eq!(
        screen_activity(&definition, &without_footer, early.columns),
        Some(AgentActivity::Working),
        "the status line alone reads working"
    );
    let mut without = without_footer;
    without.retain(|row| !row.contains('\u{2026}'));
    assert_eq!(
        screen_activity(&definition, &without, early.columns),
        Some(AgentActivity::Idle),
        "without the status line and the turn footer the visible prompt box reads idle"
    );
}

/// Blocking dialogs win over the idle title the real Claude keeps while they
/// are open.
#[test]
fn a_tool_approval_or_question_is_blocked_although_the_title_says_idle() {
    let definition = installed_definition();
    let mut checked = 0_usize;
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        if !["approval", "askuser"].contains(&frame.state.as_str()) {
            continue;
        }
        let title = frame.title.as_deref().expect("a title");
        let expected = match frame.set {
            ScreenSet::Previous => "\u{2733} Claude Code",
            ScreenSet::Current => PROGRAM_STATUS_TITLE,
        };
        assert_eq!(title, expected);
        assert_eq!(
            live_activity(&definition, title, &frame.rows, frame.columns),
            Some(AgentActivity::Blocked),
            "{}",
            frame.name
        );
        checked += 1;
    }
    assert_eq!(
        checked, 40,
        "approval and question screens at every width of both sets"
    );
}

/// The first-run dialogs (theme picker, API-key approval, login method list)
/// and the folder trust dialog wait for a choice: each reads blocked from the
/// screen, the trust dialog at every captured width, its heading scrolled away
/// and its option wrapped at 20 columns.
#[test]
fn the_first_run_and_trust_dialogs_are_blocked() {
    let definition = installed_definition();
    let mut checked = 0_usize;
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        if !FIRST_RUN_STATES.contains(&frame.state.as_str()) {
            continue;
        }
        assert_eq!(
            screen_activity(&definition, &frame.rows, frame.columns),
            Some(AgentActivity::Blocked),
            "{}",
            frame.name
        );
        checked += 1;
    }
    assert_eq!(
        checked, 44,
        "theme, login and every apikey and trust width of both sets"
    );
    let trust = load_frame(ScreenSet::Current, "trust", "trust", DEFAULT_COLUMNS);
    assert!(
        trust
            .rows
            .iter()
            .any(|row| row.contains("Yes, I trust this folder")),
        "the trust capture shows the real dialog"
    );
}

/// A reply that quotes the first-run and trust dialogs, the way this package's
/// guide or an explanation of `/theme` does.
const QUOTED_DIALOGS: [&str; 4] = [
    "\u{25cf} The trust dialog offers \"Yes, I trust this folder\" and \"No, exit\";",
    "  press Enter to confirm. /theme asks you to choose the text style:",
    "  Dark mode or Light mode. Select login method: Claude account with",
    "  subscription or Anthropic Console account.",
];

/// `frame` with [`QUOTED_DIALOGS`] in its transcript above the prompt box, in
/// the first run of blank rows that holds them with a blank row on each side,
/// so the screen keeps its height and its other lines.
fn with_quoted_dialogs(frame: &Frame) -> Vec<String> {
    let rules: Vec<usize> = frame
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.starts_with("\u{2500}\u{2500}\u{2500}"))
        .map(|(index, _)| index)
        .collect();
    let top_border = rules[rules.len() - 2];
    let span = QUOTED_DIALOGS.len() + 2;
    let start = (0..=top_border.saturating_sub(span))
        .find(|start| {
            frame.rows[*start..*start + span]
                .iter()
                .all(String::is_empty)
        })
        .unwrap_or_else(|| panic!("{} has room above its prompt box", frame.name));
    let mut rows = frame.rows.clone();
    for (offset, line) in QUOTED_DIALOGS.iter().enumerate() {
        (*line).clone_into(&mut rows[start + 1 + offset]);
    }
    rows
}

/// Dialog text quoted in the transcript is not a dialog: with the prompt box
/// below it, an idle screen stays idle and a running turn stays working.
#[test]
fn quoted_dialog_text_in_the_transcript_is_not_a_dialog() {
    let definition = installed_definition();
    for set in ScreenSet::ALL {
        for (name, expected) in [
            ("idle", AgentActivity::Idle),
            ("idle_after_turn", AgentActivity::Idle),
            ("working_early", AgentActivity::Working),
            ("working_stream", AgentActivity::Working),
        ] {
            let frame = load_frame(set, name, name, DEFAULT_COLUMNS);
            let rows = with_quoted_dialogs(&frame);
            assert_eq!(
                screen_activity(&definition, &rows, frame.columns),
                Some(expected),
                "{name} of {set:?} quoting the dialogs: {rows:#?}"
            );
        }
    }
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

/// The id of the manifest rule the daemon's detector reads from `rows` drawn
/// without a title, built from the regions the detector renders for it.
fn screen_rule(definition: &RuntimeDefinition, rows: &[String], columns: u16) -> Option<String> {
    let started = Instant::now();
    let mut detector = detector_for(definition, started, columns);
    detector.feed(started, &frame_bytes(None, rows));
    let context = detector
        .region_previews()
        .into_iter()
        // The program-status preview is no manifest region.
        .filter_map(|preview| {
            let region: ManifestRegion = preview.region.parse().ok()?;
            Some((region, preview.text))
        })
        .filter(|(region, _)| {
            !matches!(
                region,
                ManifestRegion::OscTitle | ManifestRegion::OscProgress
            )
        })
        .fold(MatchContext::default(), |context, (region, text)| {
            context.with_region_text(region, text)
        });
    definition
        .manifest()
        .match_context(&context)
        .map(|matched| matched.rule_id)
}

/// Captured widths at which the completed-turn line wraps (`✻ Sautéed for 7s ·`
/// / `done 3:37 PM`), so the line above the prompt box is its tail and only the
/// prompt box reads idle.
const COMPLETED_LINE_WRAPPED_WIDTHS: [u16; 2] = [20, 30];

/// The line a completed turn leaves above the prompt box, with or without the
/// ` · done <time>` 2.1.289 appends, is what reads idle: the precise
/// completed-turn rule, not the prompt box alone.
#[test]
fn the_completed_turn_line_with_its_done_time_reads_idle_through_its_rule() {
    let definition = installed_definition();
    let prompt = [
        "\u{2500}\u{2500}\u{2500}\u{2500}",
        "\u{276f} ",
        "\u{2500}\u{2500}\u{2500}\u{2500}",
    ];
    for status in [
        "\u{273b} Cogitated for 26s",
        "\u{273b} Brewed for 7s \u{b7} done 3:34 PM",
        "\u{273b} Brewed for 7s \u{b7} done 15:34",
    ] {
        let mut rows = vec![status.to_owned()];
        rows.extend(dialog(&prompt));
        assert_eq!(
            screen_rule(&definition, &rows, 80).as_deref(),
            Some("completed_turn_status"),
            "{status:?}"
        );
        assert_eq!(
            screen_activity(&definition, &rows, 80),
            Some(AgentActivity::Idle)
        );
    }

    let mut checked = 0_usize;
    for frame in top_level_frames()
        .into_iter()
        .chain(width_frames())
        .filter(|frame| frame.state == "idle_after_turn")
    {
        let expected = if COMPLETED_LINE_WRAPPED_WIDTHS.contains(&frame.columns) {
            "live_prompt_box"
        } else {
            assert!(
                frame.rows.iter().any(|row| row.contains(" \u{b7} done ")),
                "{} shows the done time",
                frame.name
            );
            "completed_turn_status"
        };
        assert_eq!(
            screen_rule(&definition, &frame.rows, frame.columns).as_deref(),
            Some(expected),
            "{}",
            frame.name
        );
        checked += 1;
    }
    assert_eq!(checked, 10, "idle_after_turn at every width");
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
    assert!(matchers.matches(&process(
        "node",
        &[
            "node",
            "--no-warnings",
            "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
        ]
    )));
    assert!(matchers.matches(&process("node", &["node", "/usr/local/bin/claude"])));
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
    // name is `claude.exe` and the command line ends in `claude.exe`. Each
    // matcher accepts it on its own.
    let npm = "/opt/npm/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe";
    assert!(matchers.matches(&process("claude.exe", &[npm])));
    assert!(matchers.matches(&process("claude.exe", &["other"])));
    assert!(matchers.matches(&process("sh", &[npm, "--resume", "abc"])));

    // A script that merely carries `claude` in its name is not Claude.
    for (comm, cmdline) in [
        ("node", vec!["node", "/opt/claude-companion.mjs"]),
        (
            "node",
            vec!["node", "/home/user/claude-companion.mjs", "claude"],
        ),
        ("bun", vec!["bun", "/home/user/claude-helper/index.ts"]),
        ("claude.exe.bak", vec!["/work/claude.exe.bak"]),
        // Commands that only name a Claude executable as an argument.
        ("ls", vec!["ls", "-l", "/x/bin/claude.exe"]),
        (
            "sha256sum",
            vec![
                "sha256sum",
                "/opt/npm/@anthropic-ai/claude-code/bin/claude.exe",
            ],
        ),
        ("tail", vec!["tail", "-f", "/work/claude"]),
        // A node option with a separate value is indistinguishable from a
        // script followed by arguments, so the script after it is not read.
        (
            "node",
            vec!["node", "-r", "./preload.js", "/usr/local/bin/claude"],
        ),
        // Launchers that name the package but run no Claude script themselves:
        // the `claude.exe` they start is matched on its own.
        (
            "node",
            vec!["node", "/usr/bin/npx", "@anthropic-ai/claude-code"],
        ),
        ("bun", vec!["bun", "x", "@anthropic-ai/claude-code"]),
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

/// Captured frames the #613 rules read differently from the golden manifest:
/// the frame, whether its captured title is fed with it, and the one
/// transition the package manifest emits for it.
const RECLASSIFIED_613: [(&str, bool, AgentActivity, StateSource); 35] = [
    // The running-turn title outranks the status line.
    (
        "working_early",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    // A streaming reply reads working from its title, ...
    (
        "working_stream",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    (
        "working_stream_w20",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    (
        "working_stream_w30",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    (
        "working_stream_w40",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    (
        "working_stream_w60",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    (
        "working_stream_w80",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    (
        "working_stream_w100",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    (
        "working_stream_w120",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    (
        "working_stream_w160",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    (
        "working_stream_w200",
        true,
        AgentActivity::Working,
        StateSource::OscTitle,
    ),
    // ... and from its turn footer wherever the footer shows the hint.
    (
        "working_stream",
        false,
        AgentActivity::Working,
        StateSource::Screen,
    ),
    (
        "working_stream_w40",
        false,
        AgentActivity::Working,
        StateSource::Screen,
    ),
    (
        "working_stream_w60",
        false,
        AgentActivity::Working,
        StateSource::Screen,
    ),
    (
        "working_stream_w80",
        false,
        AgentActivity::Working,
        StateSource::Screen,
    ),
    (
        "working_stream_w100",
        false,
        AgentActivity::Working,
        StateSource::Screen,
    ),
    (
        "working_stream_w120",
        false,
        AgentActivity::Working,
        StateSource::Screen,
    ),
    (
        "working_stream_w160",
        false,
        AgentActivity::Working,
        StateSource::Screen,
    ),
    (
        "working_stream_w200",
        false,
        AgentActivity::Working,
        StateSource::Screen,
    ),
    // The question form whose footer wraps.
    (
        "askuser_w30",
        true,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "askuser_w30",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "askuser_w40",
        true,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "askuser_w40",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    // The first-run and folder trust dialogs, captured without a title.
    ("theme", false, AgentActivity::Blocked, StateSource::Screen),
    ("login", false, AgentActivity::Blocked, StateSource::Screen),
    ("trust", false, AgentActivity::Blocked, StateSource::Screen),
    (
        "trust_w20",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "trust_w30",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "trust_w40",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "trust_w60",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "trust_w80",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "trust_w100",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "trust_w120",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "trust_w160",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
    (
        "trust_w200",
        false,
        AgentActivity::Blocked,
        StateSource::Screen,
    ),
];

/// Everything the golden built-in manifest of release v0.34.2 classifies, the
/// package manifest classifies the same way, except the frames and titles the
/// #613 rules read differently: the captured frames with and without their
/// titles, the synthetic dialogs of the rule tests and the titles. Each
/// exception is listed with what the package reads and must differ from the
/// golden, so the list can neither hide drift nor go stale.
#[test]
fn the_package_manifest_behaves_like_the_builtin_manifest_on_every_frame() {
    let package = installed_definition();
    let golden = golden_definition();
    let mut compared = 0_usize;
    let mut reclassified = BTreeSet::new();
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        let fed_titles = if frame.title.is_some() {
            vec![frame.title.as_deref(), None]
        } else {
            vec![None]
        };
        for title in fed_titles {
            let seen = transitions(&package, title, &frame.rows, frame.columns);
            let golden_seen = transitions(&golden, title, &frame.rows, frame.columns);
            let exception = RECLASSIFIED_613
                .iter()
                .find(|(name, titled, _, _)| *name == frame.name && *titled == title.is_some());
            match exception {
                Some(&(name, titled, activity, source)) => {
                    assert_eq!(
                        seen,
                        [ActivityTransition { activity, source }],
                        "{name} with title {title:?}"
                    );
                    assert_ne!(
                        seen, golden_seen,
                        "{name} with title {title:?} differs from the golden"
                    );
                    reclassified.insert((name, titled));
                }
                None => assert_eq!(seen, golden_seen, "{} with title {title:?}", frame.name),
            }
            compared += 1;
        }
    }
    assert_eq!(
        reclassified.len(),
        RECLASSIFIED_613.len(),
        "every listed exception names a captured frame"
    );
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
            transitions(&golden, None, &dialog(lines), 80),
            "{lines:?}"
        );
        compared += 1;
    }
    for title in [
        "\u{280b} x",
        "\u{2733} Claude Code",
        "\u{25d2} Claude Code",
        "x",
        "",
    ] {
        assert_eq!(
            title_activity(&package, title),
            title_activity(&golden, title),
            "{title:?}"
        );
        compared += 1;
    }
    // The #613 running-turn title glyphs.
    for title in ["\u{25d0} Claude Code", "\u{25d1} Claude Code"] {
        assert_eq!(title_activity(&golden, title), None, "{title:?}");
        assert_eq!(
            title_activity(&package, title),
            Some(AgentActivity::Working),
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

/// The title Claude sets once the terminal answered its program-status probe:
/// the status glyph is left to the records.
const PROGRAM_STATUS_TITLE: &str = "Claude Code";

/// The prompt the tests submit.
const PROMPT: &str = "say hi";

/// Prompts of the main turns a test submits after [`PROMPT`], each a plain
/// text turn.
const FOLLOW_UP_PROMPTS: [&str; 2] = ["say more", "say again"];

/// Claude Code's switch for its prompt-suggestion helper.
///
/// Without it Claude Code reads the helper's enablement from a remote feature
/// flag, which a hermetic run never fetches, so the helper stays off.
const PROMPT_SUGGESTION_VARIABLE: &str = "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION";

/// Directory of the session worker's log, relative to the harness root: the
/// harness gives the worker `worker/state` as its state home, and the worker
/// writes `pohunek-session-<id>.jsonl` to `pohunek/logs` below it.
const WORKER_LOG_DIR: &str = "worker/state/pohunek/logs";

/// How long one `session wait` polls before the caller retries, in
/// milliseconds.
const WAIT_SLICE_MS: &str = "8000";

/// Text that ends the verb of Claude's status line and opens its counters, e.g.
/// `✶ Billowing… (3s · ↓ 2 tokens)`.
const STATUS_LINE_MARK: &str = "\u{2026} (";

/// State sources of what Claude shows or reports in the terminal stream: the
/// screen, the terminal title and the OSC 7501 program-status record.
const VISIBLE_SOURCES: [&str; 3] = ["screen", "osc_title", "osc_progress"];

/// How long the held-stream test watches for an idle reading, in milliseconds:
/// longer than two of the 960 ms title changes of a running turn plus the
/// daemon's 700 ms debounce cap, so a screen that read idle between title
/// changes would publish.
const HELD_STREAM_WATCH_MS: &str = "3000";

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
    /// Digest of the installed package archive.
    digest: PackageDigest,
}

impl Fixture {
    /// Starts a fixture whose daemon serves `claude` from the installed
    /// official package, the only source of `claude`.
    async fn start(seed: Seed) -> Self {
        let key = test_key();
        let anchor =
            HostTrustAnchor::new(vec![root_of(&key, WINDOW_START, WINDOW_END)], Vec::new())
                .expect("the test trust anchor");
        let harness = Harness::start_with_trust(CatalogTrust::Loaded(anchor)).await;
        let processes = ProcessGuard::new(harness.env.root());
        let digest = install_package(&harness, &key).await;
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
        self.write_profile_with(name, program, &[]);
    }

    /// Writes the profile `name` as [`Fixture::write_profile`] does, with
    /// `extra` added to Claude's environment.
    fn write_profile_with(&self, name: &str, program: Option<&Path>, extra: &[(&str, &str)]) {
        let program = program.map_or_else(String::new, |program| {
            format!("program = \"{}\"\n", path_str(program))
        });
        let mut variables: Vec<(&str, String)> =
            claude_environment(&self.claude_home, &self.stub.base_url());
        variables.extend(extra.iter().map(|(key, value)| (*key, (*value).to_owned())));
        let env = variables
            .iter()
            .fold(String::new(), |mut env, (key, value)| {
                writeln!(env, "{key} = \"{value}\"").expect("write to a string");
                env
            });
        let pin = format!(
            "package = \"{PACKAGE_ID}\"\ndigest = \"{}\"\n",
            self.digest.as_str()
        );
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

    /// Removes every `event` hook from the fixture's Claude settings, so Claude
    /// reports that event to no hook.
    fn remove_hooks(&self, event: &str) {
        let path = self.claude_home.join("settings.json");
        let mut settings: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings.json"))
                .expect("settings.json is JSON");
        let removed = settings["hooks"]
            .as_object_mut()
            .and_then(|hooks| hooks.remove(event));
        assert!(
            removed.is_some(),
            "the integration registered {event}: {settings}"
        );
        fs::write(
            &path,
            serde_json::to_vec_pretty(&settings).expect("settings JSON"),
        )
        .expect("write settings.json");
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
        assert_eq!(package["digest"], self.digest.as_str(), "{listed}");

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

    /// Waits until the session reports `working` on the strength of what Claude
    /// shows, the screen or the running-turn title, not the byte-activity
    /// fallback.
    async fn wait_visible_working(&self, id: &str) -> Value {
        wait_until(
            &format!("working from the screen or title of {id}"),
            || async {
                let (code, inspected) = self.harness.json(&["session", "inspect", id]).await;
                assert_eq!(code, 0, "{inspected}");
                (inspected["ok"]["activity"] == "working"
                    && VISIBLE_SOURCES
                        .contains(&inspected["ok"]["state_source"].as_str().unwrap_or("")))
                .then(|| inspected["ok"].clone())
            },
        )
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

    /// The session record `session inspect` reports.
    async fn inspect(&self, id: &str) -> Value {
        let (code, inspected) = self.harness.json(&["session", "inspect", id]).await;
        assert_eq!(code, 0, "{inspected}");
        inspected["ok"].clone()
    }

    /// The subagent rows `session inspect` reports; none when it reports no
    /// collection.
    async fn subagents(&self, id: &str) -> Vec<Value> {
        self.inspect(id).await["subagents"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The session's one subagent row; fails when the session reports another
    /// number of rows.
    async fn only_subagent(&self, id: &str) -> Value {
        let rows = self.subagents(id).await;
        assert_eq!(rows.len(), 1, "one subagent row: {rows:#?}");
        rows[0].clone()
    }

    /// Waits until the session's one subagent row is `running` and returns it;
    /// `what` names the awaited run.
    async fn wait_running_subagent(&self, id: &str, what: &str) -> Value {
        wait_until(&format!("{what}: a running subagent row"), || async {
            let rows = self.subagents(id).await;
            assert!(rows.len() <= 1, "at most one subagent row: {rows:#?}");
            rows.into_iter().find(|row| row["lifecycle"] == "running")
        })
        .await
    }

    /// Waits until the session's one subagent row is `agent` and `completed`
    /// while the runtime launched as `pid` stays live, and returns that row.
    async fn wait_completed_subagent(&self, id: &str, agent: &str, pid: &Value) -> Value {
        wait_until(&format!("subagent {agent} to complete"), || async {
            let inspected = self.inspect(id).await;
            assert_eq!(
                inspected["runtime"]["state"], "live",
                "the parent runtime stays live: {inspected}"
            );
            assert_eq!(&inspected["pid"], pid, "{inspected}");
            let rows = inspected["subagents"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            assert!(
                rows.len() == 1 && rows[0]["id"] == agent,
                "the session keeps one row, for {agent}: {rows:#?}"
            );
            (rows[0]["lifecycle"] == "completed").then(|| rows[0].clone())
        })
        .await
    }

    /// The structured fields of every event the session worker logged so far,
    /// one JSON object per log line; a line still being written is skipped.
    fn worker_events(&self, id: &str) -> Vec<Value> {
        let path = self
            .harness
            .env
            .root()
            .join(WORKER_LOG_DIR)
            .join(format!("pohunek-session-{id}.jsonl"));
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .map(|event| event["fields"].clone())
            .collect()
    }

    /// The worker's log events of applied main-`Stop` snapshots, in log order.
    fn snapshots(&self, id: &str) -> Vec<Value> {
        self.worker_events(id)
            .into_iter()
            .filter(|event| event.get("subagent.snapshot.tasks").is_some())
            .collect()
    }

    /// Adds a second `SubagentStop` hook to the fixture's Claude settings that
    /// blocks each invocation until [`StopHold::release`]; Claude keeps the
    /// stopping agent running in its task registry until its hooks return.
    fn hold_subagent_stops(&self) -> StopHold {
        let root = self.harness.env.root();
        let hold = StopHold {
            fifo: root.join("subagent-stop.fifo"),
            waiting: root.join("subagent-stop.waiting"),
        };
        let made = std::process::Command::new("mkfifo")
            .arg(&hold.fifo)
            .status()
            .expect("run mkfifo");
        assert!(made.success(), "mkfifo {}", hold.fifo.display());
        let script = root.join("hold-subagent-stop.sh");
        write_executable(
            &script,
            format!(
                "#!/bin/sh\ncat >/dev/null\necho >>'{}'\ncat '{}' >/dev/null\n",
                path_str(&hold.waiting),
                path_str(&hold.fifo)
            ),
        )
        .expect("write the holding hook");
        let path = self.claude_home.join("settings.json");
        let mut settings: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings.json"))
                .expect("settings.json is JSON");
        settings["hooks"]["SubagentStop"]
            .as_array_mut()
            .expect("the integration registered SubagentStop")
            .push(serde_json::json!({
                "hooks": [{
                    "type": "command",
                    "command": path_str(&script),
                    "timeout": HELD_HOOK_TIMEOUT_SECS,
                }],
            }));
        fs::write(
            &path,
            serde_json::to_vec_pretty(&settings).expect("settings JSON"),
        )
        .expect("write settings.json");
        hold
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

/// Most [`HANG_GUARD`]-bounded waits one invocation of the blocking
/// `SubagentStop` hook stays blocked across: from the agent's stop, the wait
/// for its completed row, the wait for the next main-`Stop` snapshot, and the
/// two waits of [`StopHold::release`].
const HELD_HOOK_WAITS: u64 = 4;

/// Timeout, in seconds, of the blocking `SubagentStop` hook
/// [`Fixture::hold_subagent_stops`] adds.
///
/// Claude Code ends a command hook once its timeout elapses (600 s when the
/// entry sets none, in 2.1.289 and 2.1.296), and an ended hook lets the agent
/// finish before the test releases it. The value covers every wait a held
/// invocation spans, each bounded by [`HANG_GUARD`], plus one more for the
/// CLI calls between them, so a slow but healthy run never loses its hold; a
/// stuck run fails at a guard first, and the fixture's process guard ends
/// the held hook with the session.
const HELD_HOOK_TIMEOUT_SECS: u64 = (HELD_HOOK_WAITS + 1) * HANG_GUARD.as_secs();

const _: () = assert!(HELD_HOOK_TIMEOUT_SECS > HELD_HOOK_WAITS * HANG_GUARD.as_secs());

/// The blocking `SubagentStop` hook [`Fixture::hold_subagent_stops`] adds.
struct StopHold {
    /// FIFO each invocation reads until a writer opens and closes it.
    fifo: PathBuf,
    /// File each invocation appends one line to before it blocks.
    waiting: PathBuf,
}

impl StopHold {
    /// Waits until the hook's `invocation`-th run blocks, then releases every
    /// blocked run.
    ///
    /// The FIFO is opened without blocking: the open fails with `ENXIO` while
    /// no reader has it open, and the wait retries until the reader is there,
    /// bounded by [`HANG_GUARD`]. Closing the writer ends the reader's `cat`.
    async fn release(&self, invocation: usize) {
        wait_until(&format!("held SubagentStop hook {invocation}"), || async {
            let lines = fs::read_to_string(&self.waiting).map_or(0, |text| text.lines().count());
            (lines >= invocation).then_some(())
        })
        .await;
        wait_until(
            &format!("the reader of held SubagentStop hook {invocation}"),
            || async {
                match fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&self.fifo)
                {
                    Ok(_writer) => Some(()),
                    Err(error) if error.raw_os_error() == Some(libc::ENXIO) => None,
                    Err(error) => panic!("open the hook FIFO {}: {error}", self.fifo.display()),
                }
            },
        )
        .await;
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
        runtime_api: RUNTIME_API,
        platforms: vec![host_platform()],
        attestations: attestations_for(&[host_platform()]),
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

/// Largest page of retained output read at once, in bytes.
const OUTPUT_PAGE_BYTES: &str = "65536";

/// A JSON number or string as a command-line argument.
fn json_scalar(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

/// Columns of the top-level fixtures.
const DEFAULT_COLUMNS: u16 = 100;

/// How long a resized screen is given to redraw before it is read, in
/// milliseconds. Claude redraws within a frame; the pause only has to outlast
/// that and the daemon's screen tracker.
const REDRAW_PAUSE_MS: u64 = 700;

impl Fixture {
    /// Records the session's current screen and title as `<name>` below
    /// `directory`.
    async fn capture(&self, directory: &Path, id: &str, name: &str) {
        let (title, rows) = self.screen(id).await;
        capture::write_capture(
            directory,
            name,
            path_str(self.harness.env.root()),
            title.as_deref(),
            &rows,
        );
    }

    /// Every retained raw output byte of the session's current runtime.
    async fn raw_output(&self, id: &str) -> Vec<u8> {
        let inspected = self.inspect(id).await;
        let runtime = &inspected["runtime"];
        let worker = runtime["worker_instance_id"].as_str().expect("worker id");
        let generation = json_scalar(&runtime["runtime_generation"]);
        let mut bytes = Vec::new();
        let mut offset = "0".to_owned();
        loop {
            let (code, page) = self
                .harness
                .json(&[
                    "session",
                    "output",
                    id,
                    "--worker-instance-id",
                    worker,
                    "--runtime-generation",
                    &generation,
                    "--after-offset",
                    &offset,
                    "--max-bytes",
                    OUTPUT_PAGE_BYTES,
                ])
                .await;
            assert_eq!(code, 0, "{page}");
            bytes.extend(capture::output_bytes(&page));
            offset = json_scalar(&page["ok"]["next_offset"]);
            if page["ok"]["has_more"] != true {
                return bytes;
            }
        }
    }

    /// Records the ordered root program-status records seen so far as
    /// `<name>.program-status`.
    async fn capture_program_status(&self, directory: &Path, id: &str, name: &str) {
        let raw = self.raw_output(id).await;
        capture::write_program_status(directory, name, &raw);
    }

    /// Waits until a visible row starts with `prefix` and returns the rows.
    async fn wait_screen_row_starting(&self, id: &str, prefix: &str) -> Vec<String> {
        wait_until(
            &format!("a screen row starting with {prefix:?}"),
            || async {
                let (_title, rows) = self.screen(id).await;
                rows.iter()
                    .any(|row| row.starts_with(prefix))
                    .then_some(rows)
            },
        )
        .await
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
                capture::CAPTURE_ROWS,
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
        for columns in capture::CAPTURE_WIDTHS {
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

/// The package manifest reads the live screen of a real session as `expected`
/// from the screen alone, without the title Claude has set.
async fn assert_screen_alone_reads(
    fixture: &Fixture,
    definition: &RuntimeDefinition,
    id: &str,
    expected: AgentActivity,
) {
    let (_title, rows) = fixture.screen(id).await;
    let columns = u16::try_from(
        rows.iter()
            .map(|row| row.chars().count())
            .max()
            .unwrap_or(0),
    )
    .unwrap_or(u16::MAX)
    .max(DEFAULT_COLUMNS);
    assert_eq!(
        screen_activity(definition, &rows, columns),
        Some(expected),
        "the package manifest on the live screen alone: {rows:#?}"
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
    // the distribution and native-installer binary is named `claude`, the npm
    // package's executable `bin/claude.exe`.
    let launch_pid = fixture.inspect(&id).await["pid"]
        .as_u64()
        .expect("the launch process id");
    let matchers = package
        .manifest()
        .process_matchers()
        .expect("the manifest declares process matchers");
    let fact = process_fact_of(launch_pid);
    assert!(
        ["claude", "claude.exe"].contains(&fact.comm.as_str()),
        "an unexpected Claude process name: {fact:?}"
    );
    assert!(
        matchers.matches(&fact),
        "the real process {fact:?} is matched"
    );

    // Idle: the prompt box and the `✳` title.
    fixture.wait_activity(&id, "idle").await;
    fixture.wait_screen_line(&id, "for shortcuts").await;
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Idle).await;

    // Input: written text, then a separate submit after the descriptor's
    // delay. The stub sends no text until its start gate opens, so the status
    // line stays on screen and the working screen is stable.
    let prompt = format!("{} {PROMPT}", messages_stub::HOLD_MARKER);
    fixture.input(&id, &prompt).await;
    fixture.wait_visible_working(&id).await;
    fixture.wait_screen_line(&id, STATUS_LINE_MARK).await;
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Working).await;
    assert_screen_alone_reads(&fixture, &package, &id, AgentActivity::Working).await;
    let (_title, rows) = fixture.screen(&id).await;
    assert!(
        !rows
            .iter()
            .any(|row| row.contains(messages_stub::FIRST_CHUNK)),
        "no reply text is drawn while the start gate is closed: {rows:#?}"
    );
    assert!(
        fixture.stub.requests_containing(&prompt) >= 1,
        "the stub received the prompt intact in the turn's request"
    );

    fixture.stub.open_gate();
    fixture
        .wait_screen_line(&id, messages_stub::SECOND_CHUNK)
        .await;
    fixture.wait_activity(&id, "idle").await;
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Idle).await;

    fixture.stop(&id).await;
    fixture.finish().await;
}

/// A reply held mid-stream: Claude draws the first paragraph and removes the
/// status line, while the title keeps alternating its running-turn glyphs and
/// the footer reads `esc to interrupt`. The daemon keeps the session working
/// for longer than two title changes and the package manifest reads the live
/// screen as working. The turn finishes once the stream gate opens.
#[tokio::test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
async fn a_real_claude_reply_held_mid_stream_reads_working() {
    require_real_claude();
    let package = installed_definition();
    let fixture = Fixture::start(Seed::Trusted).await;
    let launched = fixture.new_session("stream").await;
    let id = launched["id"].as_str().expect("session id").to_owned();
    fixture.wait_activity(&id, "idle").await;

    let prompt = format!(
        "{} {} {PROMPT}",
        messages_stub::HOLD_MARKER,
        messages_stub::STREAM_HOLD_MARKER
    );
    fixture.input(&id, &prompt).await;
    fixture.wait_visible_working(&id).await;
    fixture.stub.open_gate();
    let rows = fixture
        .wait_screen_line(&id, messages_stub::FIRST_CHUNK)
        .await;
    assert!(
        !rows
            .iter()
            .any(|row| row.contains(messages_stub::SECOND_CHUNK)),
        "the second chunk waits for the stream gate: {rows:#?}"
    );
    let (title, _rows) = fixture.screen(&id).await;
    assert!(
        title.is_some_and(|title| title.starts_with(TURN_GLYPHS)),
        "the title still shows a running turn"
    );
    let (code, waited) = fixture
        .harness
        .json(&[
            "session",
            "wait",
            &id,
            "--activity",
            "idle",
            "--timeout-ms",
            HELD_STREAM_WATCH_MS,
        ])
        .await;
    assert_eq!(code, 0, "{waited}");
    assert_eq!(
        waited["ok"]["reason"], "timeout",
        "the held reply never reads idle: {waited}"
    );
    assert_eq!(fixture.inspect(&id).await["activity"], "working");
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Working).await;
    // The turn footer, not the title, makes the re-rendered reply working.
    fixture.wait_screen_line(&id, "esc to interrupt").await;
    assert_screen_alone_reads(&fixture, &package, &id, AgentActivity::Working).await;

    fixture.stub.open_stream_gate();
    fixture
        .wait_screen_line(&id, messages_stub::SECOND_CHUNK)
        .await;
    fixture.wait_activity(&id, "idle").await;

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
/// Resuming the fork runs `--resume` with the fork's own id.
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
            .expect("fork argv")
            .argv,
        ["--resume", reference.as_str(), "--fork-session"]
    );
    // The fork runs a conversation of its own: a second transcript appears
    // once it takes a turn.
    fixture.input(&fork_id, "again").await;
    wait_until("the fork's own transcript", || async {
        (fixture.transcript_count() == 2).then_some(())
    })
    .await;

    let fork_record = fixture.wait_reported_conversation(&fork_id).await;
    let fork_reference = fork_record["active_agent_session_id"]
        .as_str()
        .expect("the fork's own conversation id");
    assert_ne!(fork_reference, reference);
    assert_eq!(
        fork_record["native_session_id"], fork_reference,
        "the fork adopts only its own verified conversation: {fork_record}"
    );
    assert_eq!(
        fixture.inspect(&id).await["native_session_id"],
        reference.as_str(),
        "the fork's report never changes its source's conversation"
    );

    resume_the_fork(&fixture, &fork_id, fork_reference, &reference).await;

    fixture.stop(&fork_id).await;
    fixture.stop(&id).await;
    fixture.finish().await;
}

/// Stops the fork `fork_id` and resumes it: Claude relaunches the fork's own
/// conversation `fork_reference` with `--resume`, neither forking again nor
/// naming the `source` conversation, and the session keeps that reference.
async fn resume_the_fork(fixture: &Fixture, fork_id: &str, fork_reference: &str, source: &str) {
    fixture.stop(fork_id).await;
    let (code, resumed_fork) = fixture.harness.json(&["session", "resume", fork_id]).await;
    assert_eq!(code, 0, "{resumed_fork}");
    fixture.wait_activity(fork_id, "idle").await;
    let resumed_fork_process = fixture.inspect(fork_id).await["pid"]
        .as_u64()
        .expect("the resumed fork process id");
    let resumed_fork_argv = command_line(resumed_fork_process);
    assert!(
        resumed_fork_argv
            .windows(2)
            .any(|pair| pair[0] == "--resume" && pair[1] == fork_reference),
        "the resumed fork runs `--resume {fork_reference}`: {resumed_fork_argv:?}"
    );
    assert!(
        !resumed_fork_argv
            .iter()
            .any(|argument| argument == "--fork-session" || argument == source),
        "the resumed fork neither forks again nor names its source: {resumed_fork_argv:?}"
    );
    let resumed_fork_record = fixture.wait_reported_conversation(fork_id).await;
    assert_eq!(
        resumed_fork_record["native_session_id"], fork_reference,
        "the resumed fork keeps its own conversation: {resumed_fork_record}"
    );
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

    // The SubagentStop hook completes that same subagent while the parent
    // runtime is still live; a subagent the worker marks lost, failed or
    // cancelled because the parent exited is not evidence of the hook.
    let subagent_id = subagent["id"].as_str().expect("subagent id").to_owned();
    let launch_pid = fixture.inspect(&id).await["pid"].clone();
    let finished = wait_until("the subagent to complete", || async {
        let inspected = fixture.inspect(&id).await;
        assert_eq!(
            inspected["runtime"]["state"], "live",
            "the parent runtime stays live: {inspected}"
        );
        assert_eq!(inspected["pid"], launch_pid, "{inspected}");
        inspected["subagents"]
            .as_array()
            .and_then(|subagents| {
                subagents
                    .iter()
                    .find(|subagent| subagent["id"] == subagent_id.as_str())
                    .cloned()
            })
            .filter(|subagent| subagent["lifecycle"] == "completed")
    })
    .await;
    assert_eq!(finished["provider"], "claude", "{finished}");
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

/// Timeout, in seconds, of the recording hooks the prompt-submit scenarios add.
const RECORDING_HOOK_TIMEOUT_SECS: u64 = 120;

impl Fixture {
    /// Registers a command hook for `event` in the fixture's Claude settings
    /// that runs `body` as a shell script.
    fn add_user_hook(&self, event: &str, name: &str, body: &str) -> PathBuf {
        let script = self.harness.env.root().join(format!("{name}.sh"));
        write_executable(&script, format!("#!/bin/sh\n{body}\n")).expect("write the user hook");
        let path = self.claude_home.join("settings.json");
        let mut settings: Value =
            serde_json::from_slice(&fs::read(&path).expect("read settings.json"))
                .expect("settings.json is JSON");
        settings["hooks"][event]
            .as_array_mut()
            .map(|entries| {
                entries.push(serde_json::json!({
                    "hooks": [{
                        "type": "command",
                        "command": path_str(&script),
                        "timeout": RECORDING_HOOK_TIMEOUT_SECS,
                    }],
                }));
            })
            .unwrap_or_else(|| {
                settings["hooks"][event] = serde_json::json!([{
                    "hooks": [{
                        "type": "command",
                        "command": path_str(&script),
                        "timeout": RECORDING_HOOK_TIMEOUT_SECS,
                    }],
                }]);
            });
        fs::write(
            &path,
            serde_json::to_vec_pretty(&settings).expect("settings JSON"),
        )
        .expect("write settings.json");
        script
    }

    /// Adds a hook for `event` that copies each input it receives to
    /// `recorded/<event>-<unique>.json` and returns that directory.
    fn record_hook_inputs(&self, event: &str) -> PathBuf {
        let directory = self.harness.env.root().join("recorded");
        fs::create_dir_all(&directory).expect("create the recording directory");
        self.add_user_hook(
            event,
            &format!("record-{event}"),
            &format!(
                "cat > '{}/{event}-'\"$(date +%s%N)-$$\".json",
                path_str(&directory)
            ),
        );
        directory
    }

    /// The `UserPromptSubmit` applied-report log events of the worker.
    fn prompt_reports(&self, id: &str) -> usize {
        self.worker_events(id)
            .iter()
            .filter(|event| event.get("prompt.outcome").is_some())
            .count()
    }

    /// The identity-rejection log events of the worker with `reason`.
    fn rejections(&self, id: &str, reason: &str) -> usize {
        self.worker_events(id)
            .iter()
            .filter(|event| {
                event["identity.operation"] == "prompt_accepted"
                    && event["identity.reason"] == reason
            })
            .count()
    }
}

/// The inputs a recording hook stored for `event`, oldest first.
fn recorded_inputs(directory: &Path, event: &str) -> Vec<Value> {
    let mut files: Vec<PathBuf> = fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&format!("{event}-")))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
        .iter()
        .filter_map(|file| serde_json::from_slice(&fs::read(file).ok()?).ok())
        .collect()
}

/// Golden of the real `UserPromptSubmit` behavior the prompt-submit report
/// relies on, relative to the workspace root; the capture below writes it.
const PROMPT_SUBMIT_GOLDEN: &str = "compat/claude/prompt-submit-hook.json";

/// Text a hook prints on standard output to show Claude injects it.
const CONTEXT_CANARY: &str = "PH-CONTEXT-CANARY-1486";

/// Hook inputs of one prompt event, oldest first.
fn prompts(inputs: &[Value]) -> Vec<String> {
    inputs
        .iter()
        .map(|input| input["prompt"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The managed `UserPromptSubmit` hook of a real Claude reports each prompt it
/// is run for to the worker, once, before the prompt reaches the model, and
/// only from the launch process.
///
/// The scenario runs against the pinned release and a loopback stub:
/// - a second hook holds the prompt, so the report arrives while no model
///   request exists;
/// - a second hook prints a canary on standard output once, so the canary
///   shows Claude injects hook output into the context and the managed hook's
///   own empty output adds none;
/// - a second hook runs a nested `claude -p` that inherits the session's
///   environment: its hook fires with the nested process and is refused;
/// - a Task subagent does not fire the hook for its own prompt, while the
///   notification Claude starts a turn with when the subagent finishes does,
///   and the worker accepts that report from the launch process.
///
/// With `POHUNEK_CLAUDE_CAPTURE_DIR` set the facts are written there as
/// [`PROMPT_SUBMIT_GOLDEN`] for review.
#[tokio::test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
#[expect(
    clippy::too_many_lines,
    reason = "one real session carries every prompt-submit fact, so its hooks and stub are started once"
)]
async fn a_real_claude_prompt_submit_hook_reports_once_per_prompt_from_the_launch_process_only() {
    require_real_claude();
    let fixture = Fixture::start(Seed::Trusted).await;
    fixture.install_integration().await;
    let settings =
        fs::read_to_string(fixture.claude_home.join("settings.json")).expect("settings.json");
    assert!(
        settings.contains("UserPromptSubmit") && settings.contains("prompt-submit"),
        "the integration registered the prompt-submit hook: {settings}"
    );
    let root = fixture.harness.env.root().to_path_buf();
    let recorded = fixture.record_hook_inputs("UserPromptSubmit");
    let flags = root.join("flags");
    fs::create_dir_all(&flags).expect("create the flag directory");
    let hold = StopHold {
        fifo: root.join("prompt-hold.fifo"),
        waiting: root.join("prompt-hold.waiting"),
    };
    let made = std::process::Command::new("mkfifo")
        .arg(&hold.fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo {}", hold.fifo.display());
    fixture.add_user_hook(
        "UserPromptSubmit",
        "prompt-control",
        &format!(
            "cat >/dev/null\n\
             if [ -e '{flags}/hold' ]; then echo >>'{waiting}'; cat '{fifo}' >/dev/null; fi\n\
             if [ -e '{flags}/canary' ]; then printf '{CONTEXT_CANARY}'; fi\n\
             if [ -e '{flags}/nested' ] && [ -z \"$PH_NESTED\" ]; then \
             PH_NESTED=1 claude -p 'nested say hi' >/dev/null 2>&1; fi",
            flags = path_str(&flags),
            waiting = path_str(&hold.waiting),
            fifo = path_str(&hold.fifo),
        ),
    );
    let id = fixture.new_session_id("prompt-submit").await;
    fixture.wait_activity(&id, "idle").await;
    fixture.wait_reported_conversation(&id).await;

    // Once per typed prompt, and before the model request.
    fs::write(flags.join("hold"), b"").expect("hold the prompt");
    fixture.input(&id, PROMPT).await;
    wait_until("the worker's prompt report", || async {
        (fixture.prompt_reports(&id) == 1).then_some(())
    })
    .await;
    assert_eq!(
        fixture.stub.requests_containing(PROMPT),
        0,
        "the report arrived while Claude had not yet sent the prompt to the model"
    );
    fs::remove_file(flags.join("hold")).expect("stop holding");
    hold.release(1).await;
    wait_until("the model request", || async {
        (fixture.stub.requests_containing(PROMPT) >= 1).then_some(())
    })
    .await;
    fixture.wait_activity(&id, "idle").await;
    let first = recorded_inputs(&recorded, "UserPromptSubmit");
    assert_eq!(prompts(&first), [PROMPT], "{first:#?}");
    assert_eq!(fixture.prompt_reports(&id), 1, "one report for one prompt");
    let mut input_keys: Vec<String> = first[0]
        .as_object()
        .expect("the hook input is an object")
        .keys()
        .cloned()
        .collect();
    input_keys.sort();

    // The managed hook adds nothing to the context; a hook that prints does.
    let plain = fixture.stub.bodies_containing(PROMPT);
    assert!(
        plain
            .iter()
            .all(|body| !body.contains(CONTEXT_CANARY) && !body.contains("UserPromptSubmit")),
        "the managed hook's empty output leaves the context unchanged"
    );
    fs::write(flags.join("canary"), b"").expect("print the canary");
    fixture.input(&id, FOLLOW_UP_PROMPTS[0]).await;
    wait_until("the second model request", || async {
        (fixture.stub.requests_containing(FOLLOW_UP_PROMPTS[0]) >= 1).then_some(())
    })
    .await;
    fixture.wait_activity(&id, "idle").await;
    assert!(
        fixture
            .stub
            .bodies_containing(FOLLOW_UP_PROMPTS[0])
            .iter()
            .any(|body| body.contains(CONTEXT_CANARY)),
        "Claude injects hook output into the prompt context"
    );
    fs::remove_file(flags.join("canary")).expect("stop printing");
    assert_eq!(fixture.prompt_reports(&id), 2);

    // A nested `claude -p` fires with its own process and is refused.
    fs::write(flags.join("nested"), b"").expect("run a nested Claude");
    let before = recorded_inputs(&recorded, "UserPromptSubmit").len();
    fixture.input(&id, FOLLOW_UP_PROMPTS[1]).await;
    wait_until("the nested Claude's hook", || async {
        (recorded_inputs(&recorded, "UserPromptSubmit").len() >= before + 2).then_some(())
    })
    .await;
    fixture.wait_activity(&id, "idle").await;
    fs::remove_file(flags.join("nested")).expect("stop nesting");
    let nested = recorded_inputs(&recorded, "UserPromptSubmit");
    assert_eq!(nested.len(), before + 2, "the outer and the nested prompt");
    assert_eq!(
        fixture.rejections(&id, "prompt_reporter_not_launch"),
        1,
        "the nested process's report named another process than the launch process"
    );
    assert_eq!(fixture.prompt_reports(&id), 3, "only the launch process's");

    // A Task subagent fires no hook for its own prompt; the notification that
    // starts Claude's next turn when it finishes does.
    let typed = recorded_inputs(&recorded, "UserPromptSubmit").len();
    fixture
        .input(&id, &format!("{} start it", messages_stub::SUBAGENT_MARKER))
        .await;
    wait_until("the subagent's report-back turn", || async {
        (recorded_inputs(&recorded, "UserPromptSubmit").len() >= typed + 2).then_some(())
    })
    .await;
    fixture.wait_activity(&id, "idle").await;
    let events = recorded_inputs(&recorded, "UserPromptSubmit");
    let tail = prompts(&events[typed..]);
    assert_eq!(tail.len(), 2, "{tail:#?}");
    assert!(
        tail[0].starts_with(messages_stub::SUBAGENT_MARKER),
        "{tail:#?}"
    );
    assert!(tail[1].starts_with("<task-notification>"), "{tail:#?}");
    assert!(
        events.iter().all(|input| !input["prompt"]
            .as_str()
            .is_some_and(|prompt| prompt.contains(messages_stub::SUBAGENT_PROMPT))),
        "the subagent's own prompt fired no hook"
    );
    assert_eq!(
        fixture.prompt_reports(&id),
        5,
        "the typed prompt and the self-started report-back are both launch-process reports"
    );

    if let Some(directory) = std::env::var_os(CAPTURE_VARIABLE).map(PathBuf::from) {
        let golden = serde_json::json!({
            "schema": 1,
            "release": locked_release(),
            "event": "UserPromptSubmit",
            "input_keys": input_keys,
            "fires_once_per_typed_prompt": true,
            "fires_before_the_model_request": true,
            "hook_output_is_injected_into_the_context": true,
            "managed_hook_adds_no_context": true,
            "nested_claude_p_fires_with_its_own_process": true,
            "task_subagent_own_prompt_fires": false,
            "background_agent_report_back_fires": true,
        });
        fs::write(
            directory.join("prompt-submit-hook.json"),
            serde_json::to_vec_pretty(&golden).expect("golden JSON"),
        )
        .expect("write the golden");
    }
    fixture.stop(&id).await;
    fixture.finish().await;
}

/// The golden of the prompt-submit capture belongs to the locked release, and
/// every fact the report relies on holds in it.
#[test]
fn the_prompt_submit_golden_records_the_facts_of_the_locked_release() {
    let golden: Value = serde_json::from_slice(
        &fs::read(workspace_root().join(PROMPT_SUBMIT_GOLDEN)).expect("read the golden"),
    )
    .expect("the golden is JSON");
    assert_eq!(golden["schema"], 1);
    assert_eq!(golden["release"], locked_release());
    assert_eq!(golden["event"], "UserPromptSubmit");
    for fact in [
        "fires_once_per_typed_prompt",
        "fires_before_the_model_request",
        "hook_output_is_injected_into_the_context",
        "managed_hook_adds_no_context",
        "nested_claude_p_fires_with_its_own_process",
        "background_agent_report_back_fires",
    ] {
        assert_eq!(golden[fact], true, "{fact}");
    }
    assert_eq!(
        golden["task_subagent_own_prompt_fires"], false,
        "a subagent's own prompt must not fire the hook the report relies on"
    );
    assert!(
        golden["input_keys"]
            .as_array()
            .is_some_and(|keys| keys.iter().any(|key| key == "prompt")),
        "the input carries the prompt the hook drains and never reads"
    );
}

/// The row revision `row` reports.
fn revision(row: &Value) -> u64 {
    row["revision"]
        .as_str()
        .and_then(|revision| revision.parse().ok())
        .unwrap_or_else(|| panic!("a numeric revision: {row}"))
}

/// A background agent the main agent resumes with `SendMessage` keeps its one
/// row through both runs, and no main `Stop` reopens the row once a run's own
/// `SubagentStop` completed it.
///
/// Claude Code reports the resumed run with a second `SubagentStart` for the
/// same agent id (issue #883, R1). Its main-`Stop` `background_tasks` still
/// lists an agent as running while that agent's `SubagentStop` hooks run, so
/// the `Stop` of the turn that launched or resumed a fast agent lists an agent
/// pohunek already completed. That listing leaves the row `completed`.
///
/// Each step is deterministic:
/// - the stub holds each run until the test read the row `running`; while a
///   run is held, only its `SubagentStart` can make the row `running`;
/// - the stub holds the main turn that launched or resumed the agent until
///   the run's `SubagentStop` completed the row;
/// - a second `SubagentStop` hook of the fixture blocks until the test
///   releases it, so that turn's main `Stop` is built while Claude still lists
///   the agent;
/// - each main `Stop` is awaited through the worker's snapshot log event.
///
/// `session inspect` follows the worker's subagent changes asynchronously, so
/// the row is compared at points that need a later change to be visible: the
/// resumed run's start, and a second background agent launched at the end.
#[tokio::test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
async fn a_resumed_real_claude_background_agent_keeps_one_row_through_both_runs() {
    require_real_claude();
    let fixture = Fixture::start(Seed::Trusted).await;
    fixture.install_integration().await;
    let stops = fixture.hold_subagent_stops();
    let id = fixture.new_session_id("resume").await;
    fixture.wait_activity(&id, "idle").await;
    let pid = fixture.inspect(&id).await["pid"].clone();

    fixture
        .input(
            &id,
            &format!("{} start it", messages_stub::BACKGROUND_MARKER),
        )
        .await;
    wait_until("the background agent's first request", || async {
        (fixture.stub.background_requests() >= 1).then_some(())
    })
    .await;
    let first_run = fixture.wait_running_subagent(&id, "the first run").await;
    assert_eq!(first_run["provider"], "claude", "{first_run}");
    let agent = first_run["id"].as_str().expect("subagent id").to_owned();

    fixture.stub.open_run_gate();
    let first_done = fixture.wait_completed_subagent(&id, &agent, &pid).await;
    assert_eq!(
        first_done["started_at_ms"], first_run["started_at_ms"],
        "{first_done}"
    );
    fixture.stub.open_launch_result_gate();
    let lagging = no_snapshot_reopens(&fixture, &id, &agent, 1).await;
    assert_lists_finished_agent(&lagging);
    stops.release(1).await;
    no_snapshot_reopens(&fixture, &id, &agent, 2).await;

    fixture
        .input(&id, &format!("{} {agent}", messages_stub::RESUME_MARKER))
        .await;
    wait_until("the resumed run's first request", || async {
        (fixture.stub.resumed_requests() >= 1).then_some(())
    })
    .await;
    let resumed = fixture
        .wait_running_subagent(&id, "the resumed run's SubagentStart")
        .await;
    assert_eq!(resumed["id"], agent.as_str(), "{resumed}");
    assert!(resumed["finished_at_ms"].is_null(), "{resumed}");
    // The worker numbers subagent changes consecutively, so the resumed run's
    // start is the only change since the first run completed: neither main
    // Stop between them changed the row.
    assert_eq!(
        revision(&resumed),
        revision(&first_done) + 1,
        "only the resumed run's SubagentStart changed the row: {first_done} -> {resumed}"
    );
    assert_eq!(
        resumed["started_at_ms"], first_run["started_at_ms"],
        "the reopened row keeps its first start: {resumed}"
    );

    fixture.stub.open_resume_gate();
    let second_done = fixture.wait_completed_subagent(&id, &agent, &pid).await;
    assert!(revision(&second_done) > revision(&resumed), "{second_done}");
    assert_eq!(
        second_done["started_at_ms"], first_run["started_at_ms"],
        "{second_done}"
    );
    fixture.stub.open_resume_result_gate();
    let lagging = no_snapshot_reopens(&fixture, &id, &agent, 3).await;
    assert_lists_finished_agent(&lagging);
    stops.release(2).await;
    no_snapshot_reopens(&fixture, &id, &agent, 4).await;

    // A second background agent's row is a change after both main Stops, so
    // once it is visible the first agent's row shows every earlier change.
    fixture
        .input(
            &id,
            &format!("{} start another", messages_stub::BACKGROUND_MARKER),
        )
        .await;
    let rows = wait_until("the second background agent's row", || async {
        let rows = fixture.subagents(&id).await;
        (rows.len() == 2).then_some(rows)
    })
    .await;
    let last = rows
        .iter()
        .find(|row| row["id"] == agent.as_str())
        .unwrap_or_else(|| panic!("the first agent keeps its row: {rows:#?}"));
    assert_eq!(
        last, &second_done,
        "no main Stop changed the completed row: {rows:#?}"
    );
    stops.release(3).await;

    fixture.stop(&id).await;
    fixture.finish().await;
}

/// Waits until the worker applied its `snapshots`-th main-`Stop` snapshot and
/// checks that no snapshot so far reconciled `agent`, whose own
/// `SubagentStop` completed it. Returns that snapshot's log event.
///
/// The worker logs a snapshot's reconciliations before the snapshot itself,
/// so the log read after the snapshot's event holds them.
async fn no_snapshot_reopens(fixture: &Fixture, id: &str, agent: &str, snapshots: usize) -> Value {
    let applied = wait_until(&format!("main Stop snapshot {snapshots}"), || async {
        fixture.snapshots(id).get(snapshots - 1).cloned()
    })
    .await;
    let reconciled: Vec<Value> = fixture
        .worker_events(id)
        .into_iter()
        .filter(|event| event["subagent.id"] == agent && event.get("subagent.reason").is_some())
        .filter(|event| event.get("subagent.action").is_none())
        .collect();
    assert!(
        reconciled.is_empty(),
        "main Stop snapshot {snapshots} leaves the completed row alone ({applied}): {reconciled:#?}"
    );
    applied
}

/// Checks that a main-`Stop` snapshot listed the finished agent whose blocked
/// `SubagentStop` hook keeps it running in Claude's registry, after its own
/// stop completed its row: the snapshot names a task, yet counts no listed
/// running row (only running rows are counted as listed) and completes none.
fn assert_lists_finished_agent(applied: &Value) {
    assert!(
        applied["subagent.snapshot.tasks"]
            .as_u64()
            .is_some_and(|tasks| tasks >= 1),
        "the main Stop lists the agent whose SubagentStop already completed its row: {applied}"
    );
    assert_eq!(
        (
            &applied["subagent.snapshot.listed"],
            &applied["subagent.snapshot.terminalized"]
        ),
        (&Value::from(0), &Value::from(0)),
        "the listed agent's row was already completed when the snapshot arrived: {applied}"
    );
}

/// A background agent whose `SubagentStop` never reaches pohunek is settled by
/// the main agent's `Stop` (issue #883, R4). The `Stop` of the turn that
/// launched the agent lists it while it runs, which makes the row a known
/// background agent; the first main `Stop` after it finished lists it no
/// more, so the row is completed while the parent runtime stays live.
///
/// The lost stop is deterministic: pohunek's `SubagentStop` hook is removed
/// from the fixture's Claude settings before the session starts. The stub
/// holds the agent's run until the worker applied the launching turn's
/// snapshot, awaited through the worker's snapshot log event.
#[tokio::test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
async fn a_real_claude_background_agent_without_its_stop_is_settled_by_the_main_stop() {
    require_real_claude();
    let fixture = Fixture::start(Seed::Trusted).await;
    fixture.install_integration().await;
    fixture.remove_hooks("SubagentStop");
    let id = fixture.new_session_id("snapshot").await;
    fixture.wait_activity(&id, "idle").await;
    let pid = fixture.inspect(&id).await["pid"].clone();

    fixture
        .input(
            &id,
            &format!("{} start it", messages_stub::BACKGROUND_MARKER),
        )
        .await;
    wait_until("the background agent's first request", || async {
        (fixture.stub.background_requests() >= 1).then_some(())
    })
    .await;
    let started = fixture
        .wait_running_subagent(&id, "the background agent's SubagentStart")
        .await;
    let agent = started["id"].as_str().expect("subagent id").to_owned();

    fixture.stub.open_launch_result_gate();
    let listing = wait_until("the launching turn's main Stop snapshot", || async {
        fixture.snapshots(&id).first().cloned()
    })
    .await;
    assert_eq!(
        listing["subagent.snapshot.listed"], 1,
        "the snapshot lists the running agent: {listing}"
    );
    let listed = fixture.only_subagent(&id).await;
    assert_eq!(
        listed["lifecycle"], "running",
        "a Stop that lists the agent keeps it running: {listed}"
    );

    fixture.stub.open_run_gate();
    let settled = fixture.wait_completed_subagent(&id, &agent, &pid).await;
    assert_eq!(settled["provider"], "claude", "{settled}");
    // The worker's log writer appends on a background thread, so the line can
    // trail the journal change `inspect` already reports.
    wait_until("the worker's log of the snapshot completion", || async {
        fixture
            .worker_events(&id)
            .iter()
            .any(|event| {
                event["subagent.id"] == agent.as_str()
                    && event["subagent.reason"] == "snapshot_absent"
            })
            .then_some(())
    })
    .await;

    fixture.stop(&id).await;
    fixture.finish().await;
}

/// The prompt-suggestion helper Claude Code runs after a main turn reports a
/// `SubagentStop` with a fresh agent id, an empty agent type and no
/// `SubagentStart` (issue #883, R3). The session lists no row for those
/// stops, and the worker logs each one as an ignored stop without a start.
///
/// The profile enables the helper with [`PROMPT_SUGGESTION_VARIABLE`]. Claude
/// runs it only once the conversation holds two assistant messages, so the
/// second and third main turn each produce one helper stop. Each next turn is
/// submitted after the previous helper stop was handled, because a new turn
/// cancels a pending suggestion.
#[tokio::test]
#[ignore = "needs a real `claude` on PATH; run with POHUNEK_CLAUDE_E2E=1 and --ignored"]
async fn real_claude_prompt_suggestion_stops_create_no_subagent_row() {
    require_real_claude();
    let fixture = Fixture::start(Seed::Trusted).await;
    fixture.write_profile_with(PROFILE, None, &[(PROMPT_SUGGESTION_VARIABLE, "1")]);
    fixture.install_integration().await;
    let id = fixture.new_session_id("suggestion").await;
    fixture.wait_activity(&id, "idle").await;

    fixture.input(&id, PROMPT).await;
    wait_until("the first main turn's request", || async {
        (fixture.stub.requests_with_prompt(PROMPT) >= 1).then_some(())
    })
    .await;
    for (index, prompt) in FOLLOW_UP_PROMPTS.iter().enumerate() {
        let helpers = index + 1;
        fixture.input(&id, prompt).await;
        let (ignored, rows) = wait_until(&format!("helper stop {helpers}"), || async {
            let ignored = helper_stops(&fixture.worker_events(&id));
            let rows = fixture.subagents(&id).await;
            (ignored.len() >= helpers || !rows.is_empty()).then_some((ignored, rows))
        })
        .await;
        assert!(rows.is_empty(), "a helper stop creates no row: {rows:#?}");
        assert_eq!(ignored.len(), helpers, "{ignored:?}");
    }
    let ignored = helper_stops(&fixture.worker_events(&id));
    assert_ne!(ignored[0], ignored[1], "each helper run has its own id");
    assert!(
        fixture
            .stub
            .last_messages_containing(messages_stub::SUGGESTION_OPENING)
            >= FOLLOW_UP_PROMPTS.len(),
        "each helper stop followed a prompt-suggestion request"
    );
    assert!(fixture.subagents(&id).await.is_empty());

    fixture.stop(&id).await;
    fixture.finish().await;
}

/// Ids of the stops the worker logged as ignored because no start was
/// recorded for them, in log order.
fn helper_stops(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|event| {
            event["subagent.action"] == "stop" && event["subagent.reason"] == "stop_without_start"
        })
        .filter_map(|event| event["subagent.id"].as_str().map(str::to_owned))
        .collect()
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
    assert_eq!(
        title.as_deref(),
        Some(PROGRAM_STATUS_TITLE),
        "Claude sets no status glyph while it reports its state in program-status records"
    );
    assert_package_agrees(&fixture, &package, &id, AgentActivity::Blocked).await;

    fixture.stop(&id).await;
    fixture.finish().await;
}

/// `claude` is a reserved runtime id: an install trusted only by its digest is
/// refused, so the package serves it through a signed catalog alone.
#[tokio::test]
async fn installing_the_package_by_digest_is_refused_because_claude_is_reserved() {
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
    assert_eq!(packages[0]["digest"], fixture.digest.as_str(), "{listed}");
    fixture.finish().await;
}

/// Independent recordings of the installed Claude reporter scripts, relative to
/// the workspace root; the rendered templates must reproduce them byte for
/// byte.
const REPORTER_GOLDEN_DIR: &str = "crates/daemon/src/integration/reporter_golden";

/// File names of the Claude reporter scripts in the hook directory.
const REPORTER_SCRIPTS: [&str; 2] = ["pohunek-agent-state.sh", "pohunek-agent-notify.sh"];

/// The Claude handler renders its reporter scripts from the core-owned
/// template: `integration install --agent claude` writes the recorded scripts
/// byte for byte, and status and doctor report them as the current assets.
#[tokio::test]
async fn the_claude_integration_installs_the_recorded_reporter_scripts() {
    let key = test_key();
    let anchor = HostTrustAnchor::new(vec![root_of(&key, WINDOW_START, WINDOW_END)], Vec::new())
        .expect("the test trust anchor");
    let harness = Harness::start_with_trust(CatalogTrust::Loaded(anchor)).await;
    install_package(&harness, &key).await;
    let claude_home = harness.env.home().join(".claude");
    fs::create_dir_all(&claude_home).expect("create the Claude home");

    let (code, installed) = harness
        .json(&["integration", "install", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{installed}");
    for script in REPORTER_SCRIPTS {
        let golden = workspace_root()
            .join(REPORTER_GOLDEN_DIR)
            .join(format!("claude-{script}"));
        assert_eq!(
            fs::read(claude_home.join("hooks").join(script)).expect("read the installed script"),
            fs::read(&golden).expect("read the recorded script"),
            "{script} differs from {}",
            golden.display()
        );
    }
    let (code, status) = harness
        .json(&["integration", "status", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["ok"]["agents"][0]["state"], "current", "{status}");
    let (code, doctor) = harness
        .json(&["integration", "doctor", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{doctor}");
    let diagnosis = &doctor["ok"]["agents"][0];
    assert_eq!(diagnosis["agent"], "claude", "{doctor}");
    assert_eq!(diagnosis["ok"], true, "{doctor}");
    assert_eq!(diagnosis["status"]["state"], "current", "{doctor}");
    assert_eq!(
        diagnosis["status"]["warnings"],
        Value::Array(Vec::new()),
        "{doctor}"
    );
    let findings = diagnosis["findings"].as_array().expect("findings");
    assert!(
        findings.iter().all(|finding| finding["severity"] == "info"),
        "only informational findings: {doctor}"
    );

    harness.stop().await;
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
        banner_of("3.0.0"),
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

/// Starts a same-user process below `root` that hides its environment from
/// procfs (`PR_SET_DUMPABLE` off), the way a non-dumpable helper or another
/// test's process caught between images does, and returns it.
fn spawn_non_dumpable(root: &Path) -> ChildGuard {
    /// `prctl` option that clears the dumpable flag.
    const PR_SET_DUMPABLE: u32 = 4;
    /// Seconds the helper sleeps; the process guard ends it earlier.
    const HELPER_LIFETIME_SECS: u32 = 600;
    let code = format!(
        "import ctypes, time; ctypes.CDLL(None).prctl({PR_SET_DUMPABLE}, 0, 0, 0, 0); time.sleep({HELPER_LIFETIME_SECS})"
    );
    ChildGuard::new(
        std::process::Command::new("python3")
            .args(["-c", &code])
            .current_dir(root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("start the non-dumpable helper"),
    )
}

/// A session removal is judged on the runtime's own processes: a same-user
/// process of the test host that started after the worker and whose
/// environment cannot be read (the CI case: another test's fresh worker) does
/// not make the removal refuse, because the harness observes the host through
/// the test-host view. The daemon keeps its fail-closed production behaviour.
#[tokio::test]
async fn a_session_removal_ignores_an_unreadable_process_of_the_test_host() {
    // The unreadable helper this test starts would keep another test run's
    // forced-close sweep unconfirmed.
    let _host_lock = pohunek_test_support::host_lock::HostLock::acquire(
        pohunek_test_support::host_lock::UNREADABLE_HOLDERS,
        "a_session_removal_ignores_an_unreadable_process_of_the_test_host",
    );
    let fixture = Fixture::start(Seed::Trusted).await;
    let root = fixture.harness.env.root().to_path_buf();
    let program = write_fake_claude(&root, "fake-removal", &banner_of(&locked_release()));
    fixture.write_profile("fake-removal", Some(&program));
    let (code, launched) = fixture.launch("fake-removal", "removal").await;
    assert_eq!(code, 0, "{launched}");
    let id = launched["ok"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    // Ends the helper on every path out of the test, a panic included.
    let _helper = spawn_non_dumpable(&root);
    fixture.stop(&id).await;

    let (code, removed) = fixture.harness.json(&["session", "rm", &id]).await;
    assert_eq!(code, 0, "{removed}");
    fixture.finish().await;
}

/// The command that installs the official Claude package from an extracted
/// release archive, as a recovery hint names it.
const RELEASE_INSTALL_COMMAND: &str =
    "`pohunek plugin install runtime/packages/claude.tar.zst --catalog runtime/runtime-catalog.json`";

/// System directories the clean host's `PATH` searches after its fakes: they
/// hold the shell and `sleep` the fakes run, and no agent runtime of a test
/// host.
const SYSTEM_PATH: &str = "/usr/bin:/bin";

/// A daemon that ships no Claude and trusts a throwaway catalog key, on a host
/// whose `PATH` finds a fake `claude` of the locked release first and then
/// only the system directories, so no other agent runtime of the machine
/// running the test is available.
///
/// The daemon runs in this process and resolves launch programs on its
/// `PATH`, and the sessions inherit the hermetic environment's `PATH`, so the
/// environment lock is held for the host's lifetime.
struct CleanHost {
    harness: Harness,
    key: ed25519_dalek::SigningKey,
    _fakes: tempfile::TempDir,
    _path: ProcessEnv,
}

impl CleanHost {
    async fn start() -> Self {
        let mut path = ProcessEnv::lock();
        let fakes = pohunek_test_support::tempdir().expect("create the fake claude directory");
        let fake = write_fake_claude(fakes.path(), "bin", &banner_of(&locked_release()));
        let directory = fake.parent().expect("the fake claude directory");
        let mut search = std::ffi::OsString::from(directory);
        search.push(":");
        search.push(SYSTEM_PATH);
        path.set("PATH", search);
        let key = test_key();
        let anchor =
            HostTrustAnchor::new(vec![root_of(&key, WINDOW_START, WINDOW_END)], Vec::new())
                .expect("the test trust anchor");
        let harness = Harness::start_with_trust(CatalogTrust::Loaded(anchor)).await;
        Self {
            harness,
            key,
            _fakes: fakes,
            _path: path,
        }
    }

    /// `session new --agent <agent>` in the environment's working directory.
    async fn launch(&self, agent: &str) -> (i32, Value) {
        let cwd = self.harness.env.cwd().to_path_buf();
        self.harness
            .json(&["session", "new", "--agent", agent, "--cwd", path_str(&cwd)])
            .await
    }

    /// The default config home of `claude` on this host.
    fn claude_home(&self) -> PathBuf {
        self.harness.env.home().join(".claude")
    }

    async fn stop(self) {
        self.harness.stop().await;
    }
}

/// The recovery hint of the refusal `refused`.
fn recovery_hint(refused: &Value) -> &str {
    refused["err"]["recover"]
        .as_str()
        .unwrap_or_else(|| panic!("a recovery hint: {refused}"))
}

/// The daemon serves `claude` from nothing but the official package: a host
/// without it refuses the bare runtime and its hook integration with the
/// release install as the hint, and after the catalog install the same
/// requests launch a session pinned to the package and report the package's
/// integration.
#[tokio::test]
async fn claude_launches_only_from_the_installed_official_package() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;

    let (code, refused) = host.launch("claude").await;
    assert_eq!(code, 1, "no runtime serves claude: {refused}");
    assert_eq!(
        refused["err"]["code"], "agent_profile_not_found",
        "{refused}"
    );
    let hint = recovery_hint(&refused);
    assert!(hint.contains(RELEASE_INSTALL_COMMAND), "{hint}");
    assert!(!hint.contains("shell|codex|claude"), "{hint}");
    // Installing the hook integration of `claude` needs the package too.
    let (code, refused) = host
        .harness
        .json(&["integration", "install", "--agent", "claude"])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "runtime_not_installed", "{refused}");
    assert!(
        recovery_hint(&refused).contains(RELEASE_INSTALL_COMMAND),
        "{refused}"
    );
    // So does a bare install: no served runtime has hooks the daemon
    // installs, and the hint names the package install and the named install.
    let (code, refused) = host.harness.json(&["integration", "install"]).await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(
        refused["err"]["code"], "integration_no_managed_runtime",
        "{refused}"
    );
    let hint = recovery_hint(&refused);
    assert!(hint.contains(RELEASE_INSTALL_COMMAND), "{hint}");
    assert!(
        hint.contains("`pohunek integration install --agent claude`"),
        "{hint}"
    );

    let digest = install_package(&host.harness, &host.key).await;
    let (code, launched) = host.launch("claude").await;
    assert_eq!(code, 0, "{launched}");
    assert_eq!(launched["ok"]["agent_base"], "claude", "{launched}");
    let id = launched["ok"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    // The session pins the package digest, so the package cannot go away
    // under it.
    let (code, refused) = host
        .harness
        .json(&["plugin", "uninstall", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "package_referenced", "{refused}");
    let (code, listed) = host.harness.json(&["plugin", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    assert_eq!(
        listed["ok"]["packages"][0]["digest"],
        digest.as_str(),
        "{listed}"
    );
    assert_eq!(
        listed["ok"]["packages"][0]["origin"], "official",
        "{listed}"
    );
    let (code, status) = host
        .harness
        .json(&["integration", "status", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["ok"]["agents"][0]["agent"], "claude", "{status}");

    let (code, stopped) = host.harness.json(&["session", "stop", &id]).await;
    assert_eq!(code, 0, "{stopped}");
    host.stop().await;
}

/// A host profile over `claude` that pins no package names both steps while
/// nothing serves `claude`: the release install, then the profile migration.
/// Once the package is installed the profile must be migrated, and the
/// migrated profile launches from the package.
#[tokio::test]
async fn an_unpinned_claude_profile_names_the_install_and_the_migration() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    host.harness.profile("my-claude", "base = \"claude\"\n");

    let (code, refused) = host.launch("my-claude").await;
    assert_eq!(code, 1, "no runtime serves the base: {refused}");
    assert_eq!(refused["err"]["code"], "runtime_not_installed", "{refused}");
    let hint = recovery_hint(&refused);
    assert!(hint.contains(RELEASE_INSTALL_COMMAND), "{hint}");
    assert!(
        hint.contains("`pohunek plugin profile migrate my-claude`"),
        "{hint}"
    );

    install_package(&host.harness, &host.key).await;
    let (code, unpinned) = host.launch("my-claude").await;
    assert_eq!(code, 1, "{unpinned}");
    assert_eq!(unpinned["err"]["code"], "invalid_profile", "{unpinned}");
    assert!(
        unpinned["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("pohunek plugin profile migrate my-claude")),
        "{unpinned}"
    );
    let (code, migrated) = host
        .harness
        .json(&["plugin", "profile", "migrate", "my-claude", "--yes"])
        .await;
    assert_eq!(code, 0, "{migrated}");
    let (code, launched) = host.launch("my-claude").await;
    assert_eq!(code, 0, "{launched}");
    assert_eq!(launched["ok"]["agent_base"], "claude", "{launched}");
    let id = launched["ok"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();

    let (code, stopped) = host.harness.json(&["session", "stop", &id]).await;
    assert_eq!(code, 0, "{stopped}");
    host.stop().await;
}

/// The assistant picks an available Codex, Claude or Hermes runtime; on a host
/// without the official packages it finds none, although a `claude`
/// executable is on `PATH`, and its hint names the package install.
#[tokio::test]
async fn the_assistant_finds_no_capable_agent_without_the_official_packages() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    let (code, inspected) = host.harness.json(&["host", "inspect", "local"]).await;
    assert_eq!(code, 0, "{inspected}");
    let runtimes = inspected["ok"]["runtimes"].as_array().expect("runtimes");
    assert!(
        runtimes
            .iter()
            .all(|runtime| runtime["agent"] != "hermes" || runtime["available"] == false),
        "this scenario needs a host without `hermes` on {SYSTEM_PATH}: {inspected}"
    );

    let (code, refused) = host
        .harness
        .json(&[
            "assistant",
            "--print-prompt",
            "--no-snapshot",
            "--degraded",
            "--no-start-daemon",
        ])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "no_capable_agent", "{refused}");
    let hint = recovery_hint(&refused);
    assert!(
        hint.contains("pohunek plugin install runtime/packages/<runtime>.tar.zst --catalog runtime/runtime-catalog.json"),
        "{hint}"
    );
    assert!(!hint.contains("codex/claude runtime"), "{hint}");

    host.stop().await;
}

/// Hooks pohunek installed into `~/.claude` while a runtime served `claude`
/// stay visible and removable after nothing serves it any more (the built-in
/// of an earlier release is gone and no package is installed): status and
/// doctor report them and uninstall removes them through the core-owned Claude
/// handler, while a new install still needs the package.
#[tokio::test]
async fn claude_hooks_stay_removable_without_the_official_package() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    let claude_home = host.claude_home();
    fs::create_dir_all(&claude_home).expect("create the Claude home");
    install_package(&host.harness, &host.key).await;
    let (code, installed) = host
        .harness
        .json(&["integration", "install", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{installed}");
    assert!(
        claude_home
            .join("hooks")
            .join("pohunek-agent-state.sh")
            .is_file(),
        "{installed}"
    );
    let (code, removed) = host
        .harness
        .json(&["plugin", "uninstall", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 0, "{removed}");

    let (code, status) = host
        .harness
        .json(&["integration", "status", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["ok"]["agents"][0]["agent"], "claude", "{status}");
    assert_eq!(status["ok"]["agents"][0]["state"], "current", "{status}");
    let (code, doctor) = host
        .harness
        .json(&["integration", "doctor", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{doctor}");
    assert_eq!(doctor["ok"]["agents"][0]["agent"], "claude", "{doctor}");

    let (code, uninstalled) = host
        .harness
        .json(&["integration", "uninstall", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{uninstalled}");
    assert_eq!(
        uninstalled["ok"]["uninstalled"][0]["state"], "removed",
        "{uninstalled}"
    );
    let settings = fs::read_to_string(claude_home.join("settings.json")).unwrap_or_default();
    assert!(!settings.contains("pohunek-agent-state.sh"), "{settings}");
    assert!(!claude_home
        .join("hooks")
        .join("pohunek-agent-state.sh")
        .exists());
    let (code, status) = host
        .harness
        .json(&["integration", "status", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["ok"]["agents"][0]["state"], "not_installed",
        "{status}"
    );

    let (code, refused) = host
        .harness
        .json(&["integration", "install", "--agent", "claude"])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "runtime_not_installed", "{refused}");

    host.stop().await;
}

/// A host profile over `claude` whose agent keeps its config in `home`, pinned
/// to the installed package `digest` when there is one.
fn claude_profile(home: &Path, digest: Option<&PackageDigest>) -> String {
    let pin = digest.map_or_else(String::new, |digest| {
        format!(
            "package = \"{PACKAGE_ID}\"\ndigest = \"{}\"\n",
            digest.as_str()
        )
    });
    format!(
        "base = \"claude\"\n{pin}\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
        path_str(home)
    )
}

/// Asserts whether each of `homes` holds pohunek's Claude state hook.
fn assert_hooks(homes: &[&Path], present: bool, context: &Value) {
    for home in homes {
        assert_eq!(
            home.join("hooks").join("pohunek-agent-state.sh").is_file(),
            present,
            "{}: {context}",
            home.display()
        );
    }
}

/// The hooks pohunek installed into the `CLAUDE_CONFIG_DIR` of host profiles
/// over `claude` stay removable once nothing serves `claude`, although such a
/// profile does not resolve: `--profile` and `--all-profiles` address each
/// profile's home through its own environment, so no profile home is left
/// behind.
#[tokio::test]
async fn claude_profile_hooks_stay_removable_without_the_official_package() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    let root = host.harness.env.root().to_path_buf();
    let own_home = host.claude_home();
    let homes: Vec<(&str, PathBuf)> = vec![
        ("work", root.join("work-claude")),
        ("alt", root.join("alt-claude")),
    ];
    fs::create_dir_all(&own_home).expect("create the Claude home");
    for (_name, home) in &homes {
        fs::create_dir_all(home).expect("create a profile's Claude home");
    }
    let digest = install_package(&host.harness, &host.key).await;
    for (name, home) in &homes {
        host.harness
            .profile(name, &claude_profile(home, Some(&digest)));
    }
    let (code, installed) = host
        .harness
        .json(&[
            "integration",
            "install",
            "--agent",
            "claude",
            "--all-profiles",
        ])
        .await;
    assert_eq!(code, 0, "{installed}");
    let all_homes: Vec<&Path> = std::iter::once(own_home.as_path())
        .chain(homes.iter().map(|(_name, home)| home.as_path()))
        .collect();
    assert_hooks(&all_homes, true, &installed);
    // The profiles as a release with the built-in Claude wrote them: no pin.
    for (name, home) in &homes {
        host.harness.profile(name, &claude_profile(home, None));
    }
    let (code, removed) = host
        .harness
        .json(&["plugin", "uninstall", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 0, "{removed}");

    let (code, status) = host
        .harness
        .json(&[
            "integration",
            "status",
            "--agent",
            "claude",
            "--profile",
            "alt",
        ])
        .await;
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["ok"]["agents"][0]["state"], "current", "{status}");
    let (code, uninstalled) = host
        .harness
        .json(&[
            "integration",
            "uninstall",
            "--agent",
            "claude",
            "--profile",
            "alt",
        ])
        .await;
    assert_eq!(code, 0, "{uninstalled}");
    assert_eq!(
        uninstalled["ok"]["uninstalled"][0]["state"], "removed",
        "{uninstalled}"
    );
    assert_hooks(&[homes[1].1.as_path()], false, &uninstalled);
    assert_hooks(&[homes[0].1.as_path()], true, &uninstalled);

    let (code, uninstalled) = host
        .harness
        .json(&[
            "integration",
            "uninstall",
            "--agent",
            "claude",
            "--all-profiles",
        ])
        .await;
    assert_eq!(code, 0, "{uninstalled}");
    let reports = uninstalled["ok"]["uninstalled"]
        .as_array()
        .expect("uninstall reports");
    assert_eq!(
        reports.len(),
        3,
        "the runtime's home and both profile homes: {uninstalled}"
    );
    assert!(
        uninstalled["ok"]["failed"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{uninstalled}"
    );
    assert_hooks(&all_homes, false, &uninstalled);

    host.stop().await;
}

/// A host profile over `claude` that does not resolve while the package
/// serves `claude` (here unpinned, as a release with the built-in Claude wrote
/// it) keeps the hooks in its own `CLAUDE_CONFIG_DIR` reachable:
/// `--all-profiles` status and uninstall address that home through the
/// profile's own environment, and install lists it as failing with the
/// profile's error instead of leaving it out.
#[tokio::test]
async fn an_unresolvable_claude_profile_is_reached_by_all_profiles_while_the_package_serves() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    let own_home = host.claude_home();
    let profile_home = host.harness.env.root().join("work-claude");
    fs::create_dir_all(&own_home).expect("create the Claude home");
    fs::create_dir_all(&profile_home).expect("create the profile's Claude home");
    let digest = install_package(&host.harness, &host.key).await;
    host.harness
        .profile("work", &claude_profile(&profile_home, Some(&digest)));
    let all_profiles =
        |action: &'static str| ["integration", action, "--agent", "claude", "--all-profiles"];
    let (code, installed) = host.harness.json(&all_profiles("install")).await;
    assert_eq!(code, 0, "{installed}");
    let homes = [own_home.as_path(), profile_home.as_path()];
    assert_hooks(&homes, true, &installed);
    host.harness
        .profile("work", &claude_profile(&profile_home, None));

    let (code, status) = host.harness.json(&all_profiles("status")).await;
    assert_eq!(code, 0, "{status}");
    let work = status["ok"]["agents"]
        .as_array()
        .expect("status reports")
        .iter()
        .find(|report| report["home"]["profiles"] == serde_json::json!(["work"]))
        .unwrap_or_else(|| panic!("the profile's home is reported: {status}"));
    assert_eq!(work["state"], "current", "{status}");

    let (code, refused) = host.harness.json(&all_profiles("install")).await;
    assert_eq!(code, 1, "{refused}");
    let failed = refused["ok"]["failed"].as_array().expect("failed homes");
    assert_eq!(failed.len(), 1, "{refused}");
    assert_eq!(failed[0]["home"]["profiles"], serde_json::json!(["work"]));
    assert_eq!(failed[0]["error"]["code"], "invalid_profile", "{refused}");

    let (code, uninstalled) = host.harness.json(&all_profiles("uninstall")).await;
    assert_eq!(code, 0, "{uninstalled}");
    assert_eq!(
        uninstalled["ok"]["uninstalled"]
            .as_array()
            .expect("uninstall reports")
            .len(),
        2,
        "the runtime's home and the profile's home: {uninstalled}"
    );
    assert_hooks(&homes, false, &uninstalled);

    host.stop().await;
}

/// A config home that a resolvable and an unresolvable profile over `claude`
/// both name is attributed to the resolvable one under `--all-profiles`: the
/// report's selector and its recovery commands name the profile an install
/// accepts, never the one it refuses.
#[tokio::test]
async fn a_home_shared_with_an_unresolvable_profile_is_addressed_through_the_resolvable_one() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    let shared_home = host.harness.env.root().join("shared-claude");
    fs::create_dir_all(host.claude_home()).expect("create the Claude home");
    fs::create_dir_all(&shared_home).expect("create the shared Claude home");
    let digest = install_package(&host.harness, &host.key).await;
    host.harness
        .profile("work", &claude_profile(&shared_home, Some(&digest)));
    // Unpinned, as a release with the built-in Claude wrote it: it does not
    // resolve while the package serves `claude`.
    host.harness
        .profile("work-old", &claude_profile(&shared_home, None));

    for action in ["status", "doctor"] {
        let (code, report) = host
            .harness
            .json(&["integration", action, "--agent", "claude", "--all-profiles"])
            .await;
        assert_eq!(code, 0, "{action}: {report}");
        let shared = report["ok"]["agents"]
            .as_array()
            .expect("reports")
            .iter()
            .find(|entry| entry["home"]["profiles"] == serde_json::json!(["work", "work-old"]))
            .unwrap_or_else(|| panic!("{action} reports the shared home once: {report}"));
        assert_eq!(
            shared["home"]["selector"],
            serde_json::json!({ "kind": "profile", "name": "work" }),
            "{action}: {report}"
        );
        let text = shared.to_string();
        assert!(!text.contains("--profile work-old"), "{action}: {text}");
        if action == "doctor" {
            assert!(text.contains("--profile work`"), "{action}: {text}");
        }
    }

    host.stop().await;
}

/// A host profile over `claude` that a launch would refuse as malformed (here
/// a misspelled `[env]` table) is refused by the cleanup path as well, instead
/// of being read with an empty environment that would point it at the
/// runtime's own home.
#[tokio::test]
async fn a_malformed_claude_profile_is_refused_without_the_official_package() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    let own_home = host.claude_home();
    let profile_home = host.harness.env.root().join("work-claude");
    fs::create_dir_all(&own_home).expect("create the Claude home");
    fs::create_dir_all(&profile_home).expect("create the profile's Claude home");
    install_package(&host.harness, &host.key).await;
    let (code, installed) = host
        .harness
        .json(&["integration", "install", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{installed}");
    let (code, removed) = host
        .harness
        .json(&["plugin", "uninstall", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 0, "{removed}");
    host.harness.profile(
        "work",
        &format!(
            "base = \"claude\"\n\n[evn]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
            path_str(&profile_home)
        ),
    );

    for action in ["status", "uninstall"] {
        let (code, refused) = host
            .harness
            .json(&[
                "integration",
                action,
                "--agent",
                "claude",
                "--profile",
                "work",
            ])
            .await;
        assert_eq!(code, 1, "{action}: {refused}");
        assert_eq!(
            refused["err"]["code"], "invalid_profile",
            "{action}: {refused}"
        );
    }
    assert_hooks(&[own_home.as_path()], true, &Value::Null);

    host.stop().await;
}

/// A third-party runtime that names a compiled integration handler and a
/// config home.
struct HandlerRuntime {
    /// Package id of its package.
    package: &'static str,
    /// Its runtime id.
    runtime: &'static str,
    /// The integration handler it names.
    handler: &'static str,
    /// `[config_home] env`.
    env: &'static str,
    /// `[config_home] default`.
    default: &'static str,
}

/// A runtime that names the Claude handler and the config home `claude`
/// declares; its id orders before `claude`.
const SHARED_CLAUDE_HOME: HandlerRuntime = HandlerRuntime {
    package: "acme.runtime.aclaude",
    runtime: "aclaude",
    handler: "claude-hook-v1",
    env: "CLAUDE_CONFIG_DIR",
    default: ".claude",
};

/// A runtime that names the Codex handler and Codex's config home while no
/// Codex is served, so it alone reaches that home; its id orders before every
/// other runtime of the scenario.
const OWN_CODEX_HOME: HandlerRuntime = HandlerRuntime {
    package: "acme.runtime.aaa",
    runtime: "aaa",
    handler: "codex-hook-v1",
    env: "CODEX_HOME",
    default: ".codex",
};

/// Installs the package of `spec`, trusted by its digest.
async fn install_handler_runtime(harness: &Harness, spec: &HandlerRuntime) {
    let document = format!(
        r#"schema = 1
id = "{package}"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "{runtime}"
name = "Third-party runtime"
program = "/bin/sh"
args = []
detect_manifest = "detect.toml"

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = false

[fork]
supported = false

[native_reference]
strategy = "none"

[integration]
handler = "{handler}"
hook_schema = "identity-subagent-v1"

[config_home]
env = "{env}"
default = "{default}"
"#,
        package = spec.package,
        runtime = spec.runtime,
        handler = spec.handler,
        env = spec.env,
        default = spec.default,
    );
    let entries = [
        package::ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: document.into_bytes(),
            executable: false,
        },
        package::ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: b"[[rules]]\nid = \"idle_prompt\"\nstate = \"idle\"\npriority = 100\nregion = \"whole_recent\"\nany = [{ contains = \"ready\" }]\n".to_vec(),
            executable: false,
        },
    ];
    let bytes = package::build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    let archive = harness.env.root().join(format!("{}.tar.zst", spec.runtime));
    fs::write(&archive, bytes).expect("write the archive");
    let (code, installed) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&archive),
            "--sha256",
            digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{installed}");
}

/// The report of `runtime` in the `agents` list of `document`.
fn agent_report<'a>(document: &'a Value, runtime: &str) -> &'a Value {
    document["ok"]["agents"]
        .as_array()
        .expect("agent reports")
        .iter()
        .find(|agent| agent["agent"] == runtime)
        .unwrap_or_else(|| panic!("{runtime} is reported: {document}"))
}

/// A config home that the official `claude` and a third-party runtime reach
/// through the Claude handler belongs to neither: without `--agent`, status
/// and doctor report it as a home two runtimes share, with the fix of naming
/// one runtime, and install refuses before it writes any home, also the home
/// a runtime ordered earlier reaches alone. Naming one runtime still acts on
/// the shared home.
#[tokio::test]
async fn a_config_home_two_runtimes_reach_through_the_claude_handler_has_no_owner() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    let claude_home = host.claude_home();
    fs::create_dir_all(&claude_home).expect("create the Claude home");
    let codex_home = host.harness.env.home().join(".codex");
    fs::create_dir_all(&codex_home).expect("create the Codex home");
    install_package(&host.harness, &host.key).await;
    install_handler_runtime(&host.harness, &SHARED_CLAUDE_HOME).await;
    install_handler_runtime(&host.harness, &OWN_CODEX_HOME).await;

    let (_code, status) = host.harness.json(&["integration", "status"]).await;
    for runtime in ["claude", SHARED_CLAUDE_HOME.runtime] {
        let report = agent_report(&status, runtime);
        assert_eq!(report["state"], "outdated", "{runtime}: {status}");
        let warnings = report["warnings"].as_array().expect("warnings");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.as_str().is_some_and(|text| {
                    text.contains("integration_home_ambiguous_owner")
                        && text.contains("`--agent <runtime>`")
                })),
            "the warning carries the code and its hint: {runtime}: {status}"
        );
    }
    assert_eq!(
        agent_report(&status, OWN_CODEX_HOME.runtime)["state"],
        "not_installed",
        "{status}"
    );
    let (_code, doctor) = host.harness.json(&["integration", "doctor"]).await;
    for runtime in ["claude", SHARED_CLAUDE_HOME.runtime] {
        let report = agent_report(&doctor, runtime);
        assert_eq!(report["ok"], false, "{runtime}: {doctor}");
        let finding = report["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .find(|finding| {
                finding["summary"]
                    .as_str()
                    .is_some_and(|text| text.contains("integration_home_ambiguous_owner"))
            })
            .unwrap_or_else(|| panic!("a finding names the shared home: {doctor}"));
        assert_eq!(finding["code"], "config_root_invalid", "{finding}");
        let remediation = finding["remediation"].as_str().expect("a remediation");
        assert!(
            remediation.contains(&format!("`pohunek integration install --agent {runtime}`"))
                && remediation.contains("different config homes"),
            "{remediation}"
        );
        assert!(
            !remediation.contains("existing absolute directory"),
            "{remediation}"
        );
    }
    let (_code, daemon_doctor) = host.harness.json(&["doctor"]).await;
    let checks = daemon_doctor.to_string();
    assert!(
        checks.contains("several runtimes through the same integration handler"),
        "{daemon_doctor}"
    );
    assert!(
        !checks.contains("malformed, unsafe, or incomplete"),
        "{daemon_doctor}"
    );

    let (code, refused) = host.harness.json(&["integration", "install"]).await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(
        refused["err"]["code"], "integration_home_ambiguous_owner",
        "{refused}"
    );
    assert!(
        recovery_hint(&refused).contains("--agent <runtime>"),
        "{refused}"
    );
    assert!(!claude_home.join("hooks").exists(), "nothing is installed");
    assert!(
        !codex_home.join("hooks.json").exists(),
        "the home the earlier runtime reaches alone is not written either"
    );

    let (code, installed) = host
        .harness
        .json(&["integration", "install", "--agent", "claude"])
        .await;
    assert_eq!(code, 0, "{installed}");
    assert_eq!(
        installed["ok"]["installed"][0]["agent"], "claude",
        "{installed}"
    );
    assert!(claude_home
        .join("hooks")
        .join("pohunek-agent-state.sh")
        .is_file());

    host.stop().await;
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
fn the_child_guard_terminates_a_non_dumpable_helper_when_the_test_unwinds() {
    // The non-dumpable helper would keep another test run's forced-close sweep
    // unconfirmed while it lives.
    let _host_lock = pohunek_test_support::host_lock::HostLock::acquire(
        pohunek_test_support::host_lock::UNREADABLE_HOLDERS,
        "the_child_guard_terminates_a_non_dumpable_helper_when_the_test_unwinds",
    );
    let env = pohunek_test_support::env::TestEnv::new().expect("create the environment");
    let pid = std::cell::Cell::new(0);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let helper = spawn_non_dumpable(env.root());
        pid.set(i32::try_from(helper.id()).expect("a process id"));
        assert!(is_alive(pid.get()), "the helper runs");
        panic!("a failing step after the helper started");
    }));
    assert!(outcome.is_err(), "the closure panicked");
    assert!(
        !is_alive(pid.get()),
        "the guard terminated the helper during the unwind"
    );
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
    // Ends the foreign process if an assertion fails before the explicit reap.
    let foreign_guard = ProcessGuard::new(other.root());
    let guard = ProcessGuard::new(ours.root());
    guard.reap();
    assert!(is_alive(foreign), "a process below another root survives");
    foreign_guard.reap();
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
    fixture
        .capture_program_status(&directory, &id, "idle")
        .await;
    fixture.capture_widths(&directory, &id, "idle", true).await;

    fixture
        .input(
            &id,
            &format!(
                "{} {} {PROMPT}",
                messages_stub::HOLD_MARKER,
                messages_stub::STREAM_HOLD_MARKER
            ),
        )
        .await;
    fixture.wait_visible_working(&id).await;
    fixture.wait_screen_line(&id, STATUS_LINE_MARK).await;
    fixture.capture(&directory, &id, "working_early").await;
    fixture
        .capture_program_status(&directory, &id, "working_early")
        .await;
    // The `*` frame of the spinner glyph cycle, which a markdown bullet shares.
    fixture.wait_screen_row_starting(&id, "* ").await;
    fixture.capture(&directory, &id, "working_star_frame").await;
    fixture.stub.open_gate();
    fixture
        .wait_screen_line(&id, messages_stub::FIRST_CHUNK)
        .await;
    fixture.capture(&directory, &id, "working_stream").await;
    fixture
        .capture_widths(&directory, &id, "working_stream", false)
        .await;

    fixture.stub.open_stream_gate();
    fixture
        .wait_screen_line(&id, messages_stub::SECOND_CHUNK)
        .await;
    fixture.wait_activity(&id, "idle").await;
    fixture.capture(&directory, &id, "idle_after_turn").await;
    fixture
        .capture_program_status(&directory, &id, "idle_after_turn")
        .await;
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
        .capture_program_status(&directory, &id, "approval")
        .await;
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
        .capture_program_status(&directory, &id, "askuser")
        .await;
    fixture
        .capture_widths(&directory, &id, "askuser", true)
        .await;
    fixture.stop(&id).await;
    fixture.finish().await;
}
