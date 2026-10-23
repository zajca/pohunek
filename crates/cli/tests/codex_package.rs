//! The official Codex runtime package (`runtime-packages/codex`).
//!
//! Pure tests always run: they build the package directory, parse it through
//! the same path `plugin install` uses, compare it with the built-in Codex
//! descriptor and detection manifest release v0.34.2 shipped (golden copies in
//! `compat/codex/builtin-v0.34`), and check it against the compatibility lock
//! and the screens captured from a real Codex (`compat/codex/screens`).
//! The daemon-backed tests install the built archive through `pohunek plugin
//! install --catalog` against a throwaway signing key and trust anchor, so the
//! package serves `codex` with official trust; the daemon compiles no Codex in,
//! so nothing else serves it. The real-Codex tests additionally drive an actual
//! `codex` binary; see their documentation for the opt-in.

// Rust guideline compliant 2026-10-13

#![cfg(unix)]

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use package::directory::build_directory_archive;
use package::{read_archive, CatalogEntry, Limits, PackageDigest};
use pohunek_daemon::agent::host::{
    definition_from_archive, DefinitionError, DefinitionOrigin, HandlerId, InputReadiness,
    LaunchProgram, ProbeVersion, RuntimeDefinition, TranscriptParser,
};
use pohunek_daemon::agent::{NativeReferenceStrategy, SessionRef};
use pohunek_daemon::catalog_anchor::CatalogTrust;
use pohunek_daemon::detect::{
    ActivityTransition, DetectionConfig, Detector, DetectorConfig, Manifest,
};
use pohunek_daemon::procwatch::{ProcessFact, StartIdentity};
use pohunek_daemon::session::HostTrustAnchor;
#[cfg(target_os = "linux")]
use pohunek_test_support::fs::copy_shell_image;
use pohunek_test_support::fs::write_executable;
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait::{poll_until, wait_until};
use pohunek_test_support::workspace_root;
use protocol::{
    AgentActivity, BindingProvenance, PackageId, PackageVersion, RuntimeId, StateSource,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

#[path = "support/capture.rs"]
mod capture;
#[path = "support/native_reference_diagnostics.rs"]
mod native_reference_diagnostics;
#[path = "support/plugin_harness.rs"]
mod plugin_harness;
#[path = "support/responses_stub.rs"]
mod responses_stub;

use native_reference_diagnostics::{NativeReferenceDiagnostics, MAX_EVENTS};
use plugin_harness::{path_str, Harness};
use pohunek_test_support::catalog::{
    attestations_for, catalog_of, host_platform, root_of, signed, test_key, ANY_CORE, RUNTIME_API,
    WINDOW_END, WINDOW_START,
};
use responses_stub::ResponsesStub;

/// Directory of the package source, relative to the workspace root.
const PACKAGE_DIR: &str = "runtime-packages/codex";

/// Compatibility lock of the package, relative to the workspace root.
const LOCK_PATH: &str = "compat/codex/compatibility-lock.json";

/// Captured screens, relative to the workspace root.
const SCREENS_DIR: &str = "compat/codex/screens";

/// Where release [`GOLDEN_TAG`] kept the built-in Codex descriptor, relative
/// to the workspace root; the golden's header names it.
const BUILTIN_DESCRIPTOR: &str = "crates/daemon/src/agent/builtin/codex.toml";

/// Where release [`GOLDEN_TAG`] kept the built-in Codex detection manifest,
/// relative to the workspace root; the golden's header names it.
const BUILTIN_MANIFEST: &str = "crates/daemon/src/detect/manifests/codex.toml";

/// Release whose built-in Codex descriptor and manifest the goldens copy.
const GOLDEN_TAG: &str = "v0.34.2";

/// Golden copy of [`BUILTIN_DESCRIPTOR`] at [`GOLDEN_TAG`], relative to the
/// workspace root.
const GOLDEN_DESCRIPTOR: &str = "compat/codex/builtin-v0.34/runtime.toml";

/// Golden copy of [`BUILTIN_MANIFEST`] at [`GOLDEN_TAG`], relative to the
/// workspace root.
const GOLDEN_MANIFEST: &str = "compat/codex/builtin-v0.34/detect.toml";

/// Name the golden descriptor's `detect_manifest` gives the built-in manifest.
const GOLDEN_MANIFEST_NAME: &str = "codex";

/// Package id the descriptor declares.
const PACKAGE_ID: &str = "pohunek.runtime.codex";

/// Version of the package the descriptor declares.
const PACKAGE_VERSION: &str = "1.2.2";

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

/// The built-in Codex definition release v0.34.2 shipped, parsed from the
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
    let transcripts = definition
        .transcripts()
        .expect("a transcript tree is declared");
    assert_eq!(transcripts.parser(), TranscriptParser::JsonlHeadV1);
    assert_eq!(
        transcripts.dir(),
        "sessions",
        "Codex keeps its rollouts below `sessions`"
    );
}

/// The package keeps every fact the built-in Codex descriptor of release
/// v0.34.2 carries; only the manifest source, the version probe and the
/// transcript tree, which that descriptor does not declare, differ.
#[test]
fn the_package_descriptor_preserves_every_builtin_codex_fact() {
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
    // The readiness is the one behavioral difference: the package opts into
    // the strict manifest-idle readiness (#1114), which the built-in
    // descriptor does not declare.
    assert_eq!(package.input_readiness(), InputReadiness::ManifestIdle);
    assert_eq!(golden.input_readiness(), InputReadiness::BracketedPaste);
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
/// reference, the version probe and the transcript tree, the fields that differ
/// by design, are removed. The package version is above the golden's because
/// the package declares the transcript tree.
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
    // built-in descriptor relied on the bracketed-paste enable signal. The
    // package opts into manifest-idle readiness so the startup update popup
    // cannot take the initial prompt (#1114).
    let input = package
        .get_mut("input")
        .and_then(toml::Value::as_table_mut)
        .expect("an [input] table");
    assert_eq!(
        input.remove("ready").as_ref().and_then(toml::Value::as_str),
        Some("manifest_idle"),
        "the package waits for the manifest's idle evidence"
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

/// Rule the package adds to the golden built-in manifest: the folder trust
/// dialog of Codex 0.160, which the built-in never matched.
const FOLDER_TRUST_RULE: &str = "folder_trust_prompt";

/// Rule the package adds to the golden built-in manifest: the startup update
/// popup of Codex 0.160, which the built-in never matched.
const UPDATE_POPUP_RULE: &str = "startup_update_prompt";

/// The manifest file is the golden built-in manifest of release v0.34.2 plus
/// folder, update and login/settings rules: same process matchers, and
/// every built-in rule in the same order with the same fields.
#[test]
fn the_package_manifest_preserves_builtin_rules_and_adds_startup_dialog_rules() {
    let package = read_toml(&format!("{PACKAGE_DIR}/detect.toml"));
    let golden_text = golden_body(GOLDEN_MANIFEST, BUILTIN_MANIFEST);
    let golden: toml::Table = golden_text.parse().expect("the golden is TOML");
    let mut builtin_rules = package.clone();
    builtin_rules
        .get_mut("rules")
        .and_then(toml::Value::as_array_mut)
        .expect("rules")
        .retain(|rule| {
            !matches!(
                rule["id"].as_str(),
                Some(
                    FOLDER_TRUST_RULE
                        | UPDATE_POPUP_RULE
                        | "startup_login_prompt"
                        | "startup_api_key_prompt"
                        | "startup_incompatible_settings_prompt"
                )
            )
        });
    // The package deliberately diverges from the built-in copy in one rule: the
    // working title must name a thread or project after the spinner, so the
    // title-generation placeholder of an idle unnamed thread (`⠴ | cxw`) is not
    // a working turn (#1486).
    let golden_working = golden["rules"]
        .as_array()
        .and_then(|rules| {
            rules
                .iter()
                .find(|rule| rule["id"].as_str() == Some("osc_title_working"))
        })
        .expect("the golden working title rule")["regex"]
        .clone();
    let working = builtin_rules
        .get_mut("rules")
        .and_then(toml::Value::as_array_mut)
        .and_then(|rules| {
            rules
                .iter_mut()
                .find(|rule| rule["id"].as_str() == Some("osc_title_working"))
        })
        .expect("the package working title rule");
    assert_eq!(
        working["regex"].as_array().map(Vec::len),
        Some(1),
        "one working title pattern"
    );
    assert_eq!(
        working["regex"][0].as_str(),
        Some(r"^[\x{2800}-\x{28FF}] [^|\s]")
    );
    working["regex"] = golden_working;
    assert_eq!(builtin_rules, golden);
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
            FOLDER_TRUST_RULE,
            "startup_login_prompt",
            "startup_api_key_prompt",
            "startup_incompatible_settings_prompt",
            UPDATE_POPUP_RULE,
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
        Some(true),
        "a later 0.x release is inside the range"
    );
    assert_eq!(supported("codex-cli 0.162.1"), Some(true));
    assert_eq!(
        supported("codex-cli 1.0.0"),
        Some(false),
        "the next major is outside the range"
    );

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

fn detector_for(manifest: &Arc<Manifest>, now: Instant, columns: u16) -> Detector {
    Detector::new(
        24,
        columns,
        now,
        DetectorConfig {
            input_readiness: pohunek_daemon::agent::host::InputReadiness::default(),
            detection: DetectionConfig {
                recheck_after: Duration::from_millis(100),
                confirmations: 1,
                cap: Duration::from_millis(700),
                stable_visible_refresh: Duration::from_millis(800),
                startup_grace: Duration::ZERO,
            },
            manifest: Some(Arc::clone(manifest)),
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
    /// A dialog without a title that the screen reads as blocked once the
    /// terminal is at least [`FOLDER_TRUST_MIN_COLUMNS`] wide.
    ScreenBlocked,
    /// A screen no rule speaks for: only the byte-activity fallback applies.
    Silent,
}

/// Narrowest captured width covered by the wrapping-aware folder dialog rule.
const FOLDER_TRUST_MIN_COLUMNS: u16 = 20;

impl Meaning {
    fn of(state: &str) -> Self {
        match state {
            "idle" | "idle_after_turn" => Self::Idle,
            "working_early" | "working_stream" => Self::Working,
            "approval" | "askuser" => Self::Blocked,
            "trust" => Self::ScreenBlocked,
            "login" => Self::Silent,
            other => panic!("unknown fixture state {other}"),
        }
    }

    fn activity(self) -> Option<AgentActivity> {
        match self {
            Self::Idle => Some(AgentActivity::Idle),
            Self::Working => Some(AgentActivity::Working),
            Self::Blocked | Self::ScreenBlocked => Some(AgentActivity::Blocked),
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
        if frame.state == "login" {
            // At 60 columns and wider the captured welcome heading and all
            // choices remain intact; narrower legacy captures wrap them.
            let expected =
                (frame.columns >= 60).then_some((AgentActivity::Blocked, StateSource::Screen));
            assert_eq!(frame.evidence(&definition), expected, "{}", frame.name);
            continue;
        }
        let expected = frame.meaning();
        if expected == Meaning::ScreenBlocked {
            continue;
        }
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
            .filter(|frame| matches!(frame.meaning(), Meaning::ScreenBlocked | Meaning::Silent))
            .all(|frame| frame.title.is_none()),
        "no title was emitted for the trust and login dialogs"
    );
}

/// The folder trust dialog sets no title, so its screen alone reads it as
/// blocked, including the captured wrapping at 20 columns.
#[test]
fn the_folder_trust_dialog_is_blocked_from_the_screen_alone() {
    let definition = installed_definition();
    let mut checked = 0_usize;
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        if frame.meaning() != Meaning::ScreenBlocked {
            continue;
        }
        let expected = (frame.columns >= FOLDER_TRUST_MIN_COLUMNS)
            .then_some((AgentActivity::Blocked, StateSource::Screen));
        assert_eq!(frame.evidence(&definition), expected, "{}", frame.name);
        checked += 1;
    }
    assert_eq!(checked, 10, "the top-level and the nine width captures");
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
fn the_folder_trust_rule_needs_the_dialog_heading_at_the_top_of_the_screen() {
    let definition = installed_definition();
    let folder_trust = [
        "  Folder access",
        "  /work/project",
        "  Trust this folder? Codex can read, edit, and run files here.",
        "\u{203a} 1. Trust and continue",
        "  2. Back to Agent Command Center",
        "  enter continue \u{b7} esc back",
    ];
    assert_eq!(
        screen_activity(&definition, &dialog(&folder_trust), 80),
        Some(AgentActivity::Blocked)
    );
    // Moving the selection to the second choice keeps the dialog blocked.
    let second_selected = folder_trust.map(|line| match line {
        "\u{203a} 1. Trust and continue" => "  1. Trust and continue",
        "  2. Back to Agent Command Center" => "\u{203a} 2. Back to Agent Command Center",
        other => other,
    });
    assert_eq!(
        screen_activity(&definition, &dialog(&second_selected), 80),
        Some(AgentActivity::Blocked)
    );

    // A reply that quotes the question and both choices is not the dialog
    // without its heading line, nor is the dialog below the first sixteen lines.
    let quoted = [
        "\u{2022} The dialog reads:",
        "  Trust this folder? Codex can read, edit, and run files here.",
        "  1. Trust and continue",
        "  2. Back to Agent Command Center",
        "\u{203a} ",
    ];
    assert_eq!(screen_activity(&definition, &dialog(&quoted), 80), None);
    let mut buried = vec![String::from("line"); 17];
    buried.extend(dialog(&folder_trust));
    assert_eq!(screen_activity(&definition, &buried, 80), None);
}

/// Captures of the startup update popup, copied from the independent real-PTY
/// recordings behind #1114: `popup_w<columns>.txt` at 80, 100 and 160 columns.
/// The real popup set no terminal title, so the stale title this rule must
/// survive is only ever synthetic.
const POPUP_CAPTURES_DIR: &str = "compat/codex/screens/update-popup";

/// Columns the popup was captured at, widest to narrowest.
const POPUP_CAPTURE_COLUMNS: [u16; 3] = [160, 100, 80];

/// The popup rule reads the measured update popup as blocked from the screen
/// at every captured width, and a synthetic stale idle title under the popup
/// changes nothing: the screen blocker outranks the title's idle rule. The
/// grid is as tall as the capturing terminal's visible rows, so feeding the
/// measured rows cannot scroll the top of the screen away.
#[test]
fn the_startup_update_prompt_rule_blocks_the_measured_popup_and_outranks_a_stale_title() {
    let definition = installed_definition();
    let config = DetectorConfig {
        input_readiness: InputReadiness::default(),
        detection: DetectionConfig {
            recheck_after: Duration::from_millis(100),
            confirmations: 1,
            cap: Duration::from_millis(700),
            stable_visible_refresh: Duration::from_millis(800),
            startup_grace: Duration::ZERO,
        },
        manifest: Some(Arc::clone(definition.manifest())),
    };
    for columns in POPUP_CAPTURE_COLUMNS {
        let capture = fs::read_to_string(
            workspace_root()
                .join(POPUP_CAPTURES_DIR)
                .join(format!("popup_w{columns}.txt")),
        )
        .unwrap_or_else(|error| panic!("read the {columns}-column popup capture: {error}"));
        let rows: Vec<String> = capture.lines().map(str::to_owned).collect();
        let feed = |title: Option<&str>| {
            let mut detector = Detector::new(
                u16::try_from(rows.len()).expect("capture rows"),
                columns,
                Instant::now(),
                config.clone(),
            );
            detector.feed(Instant::now(), &frame_bytes(title, &rows))
        };
        assert_eq!(
            feed(None)
                .iter()
                .rfind(|transition| transition.source == StateSource::Screen)
                .map(|transition| (transition.activity, transition.source)),
            Some((AgentActivity::Blocked, StateSource::Screen)),
            "{columns} columns: the popup is blocked from the screen"
        );
        assert_eq!(
            feed(Some("work"))
                .iter()
                .rfind(|transition| transition.source != StateSource::Process)
                .map(|transition| (transition.activity, transition.source)),
            Some((AgentActivity::Blocked, StateSource::Screen)),
            "{columns} columns: the popup outranks a stale idle title"
        );
    }

    let quoted = ["1. Update now", "2. Skip", "3. Skip until next version"];
    assert_eq!(
        screen_activity(&definition, &dialog(&quoted), 80),
        None,
        "the choices without the heading are not the popup"
    );
    let mut buried = vec![String::from("line"); 17];
    buried.extend(dialog(&[
        "Update available · 0.160.0 → 0.162.1",
        "› 1. Update now",
        "2. Skip",
        "3. Skip until next version",
    ]));
    assert_eq!(
        screen_activity(&definition, &buried, 80),
        None,
        "the dialog below the first sixteen non-empty lines is not the popup"
    );
}

/// Real startup frames must beat cached titles; partial or quoted text must not.
#[test]
fn captured_startup_dialogs_require_the_complete_visible_surface() {
    let definition = installed_definition();
    for (surface, widths, required) in [
        (
            "login",
            &[80_u16, 160][..],
            &[
                "Welcome to Codex",
                "1. Sign in with ChatGPT",
                "2. Sign in with Device Code",
                "3. Provide your own API key",
                "Press enter to continue",
            ][..],
        ),
        (
            "api-entry",
            &[80, 160][..],
            &[
                "Welcome to Codex",
                "Use your own OpenAI API key",
                "Paste or type your API key below.",
                "╭API key",
                "Press enter to save",
                "Press esc to go back",
            ][..],
        ),
        (
            "settings",
            &[80, 160][..],
            &[
                "Background server has incompatible feature settings",
                "Restart will use these shared feature settings:",
                "1. Run without daemon this time",
                "2. Restart with these settings",
                "3. Cancel",
            ][..],
        ),
        (
            "trust",
            &[20, 30, 80, 160][..],
            &[
                "Folder access",
                "Trust this",
                "1. Trust and",
                "2. Back to Agent",
            ][..],
        ),
    ] {
        for &columns in widths {
            check_captured_startup_dialog(&definition, surface, columns, required);
        }
    }
}

fn check_captured_startup_dialog(
    definition: &RuntimeDefinition,
    surface: &str,
    columns: u16,
    required: &[&str],
) {
    let activity = |rows: &[String], columns| {
        let now = Instant::now();
        let mut detector = Detector::new(
            u16::try_from(rows.len().max(40)).expect("capture height"),
            columns,
            now,
            manifest_idle_probe_config(definition),
        );
        detector
            .feed(now, &frame_bytes(None, rows))
            .into_iter()
            .rfind(|transition| transition.source == StateSource::Screen)
            .map(|transition| transition.activity)
    };
    let path = workspace_root()
        .join("compat/codex/screens/startup-dialogs")
        .join(format!("{surface}-{columns}.json"));
    let capture: Value = serde_json::from_str(&fs::read_to_string(path).expect("read capture"))
        .expect("parse capture");
    let rows: Vec<String> = capture["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row.as_str().expect("row").to_owned())
        .collect();
    let raw = capture["raw_hex"].as_str().expect("raw PTY bytes");
    let bytes: Vec<u8> = (0..raw.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&raw[offset..offset + 2], 16).expect("hex byte"))
        .collect();
    let mut probe = LatchProbe::start(manifest_idle_probe_config(definition), 40, columns);
    probe.feed(&bytes);
    probe.feed(b"\x1b]0;synthetic-stale-idle\x07");
    probe.tick(3);
    probe.assert_ready(
        false,
        &format!("{surface}: captured dialog outranks stale title"),
    );
    assert_eq!(
        activity(&rows, columns),
        Some(AgentActivity::Blocked),
        "{surface}-{columns}"
    );
    for fragment in required {
        assert!(
            rows.iter().any(|row| row.contains(fragment)),
            "capture lacks {fragment}"
        );
        let partial: Vec<String> = rows
            .iter()
            .filter(|row| !row.contains(fragment))
            .cloned()
            .collect();
        assert_eq!(
            activity(&partial, columns),
            None,
            "{surface}-{columns}: missing {fragment}"
        );
    }
    let quoted: Vec<String> = rows.iter().map(|row| format!("| {row}")).collect();
    assert_eq!(
        activity(&quoted, columns),
        None,
        "quoted {surface}-{columns}"
    );
    let mut buried = vec![String::from("The transcript discusses this dialog:")];
    buried.extend(rows);
    assert_eq!(
        activity(&buried, columns),
        None,
        "buried {surface}-{columns}"
    );
}

/// Rows of the measured update popup capture at `columns`.
fn popup_rows(columns: u16) -> Vec<String> {
    fs::read_to_string(
        workspace_root()
            .join(POPUP_CAPTURES_DIR)
            .join(format!("popup_w{columns}.txt")),
    )
    .unwrap_or_else(|error| panic!("read the {columns}-column popup capture: {error}"))
    .lines()
    .map(str::to_owned)
    .collect()
}

/// Rows of the captured prompt composer Codex renders while (or after) it
/// loads, with no popup content and no title of its own.
fn composer_rows() -> Vec<String> {
    fs::read_to_string(workspace_root().join(SCREENS_DIR).join("idle.txt"))
        .unwrap_or_else(|error| panic!("read the idle capture: {error}"))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// One OSC title chunk.
fn title_chunk(title: &str) -> Vec<u8> {
    format!("\x1b]2;{title}\x07").into_bytes()
}

/// The clear-only chunk.
fn clear_chunk() -> Vec<u8> {
    b"\x1b[2J\x1b[H".to_vec()
}

/// A chunk that clears the screen and redraws `rows` with no title.
fn clear_composer_chunk(rows: &[String]) -> Vec<u8> {
    frame_bytes(None, rows)
}

/// An idle-looking terminal title that the popup never sets: it describes the
/// project instead of an activity, and the Codex manifest reads any
/// non-spinner, non-`Action Required` title as idle.
const FAKE_TITLE: &str = "pohunek-issue-1114";

/// The detection tuning the popup regressions share: one confirmation and no
/// startup grace, so every readiness boundary is observable within a tick.
fn latch_detection_config() -> DetectionConfig {
    DetectionConfig {
        recheck_after: Duration::from_millis(100),
        confirmations: 1,
        cap: Duration::from_millis(700),
        stable_visible_refresh: Duration::from_millis(800),
        startup_grace: Duration::ZERO,
    }
}

/// Probe of a strict `manifest_idle` detector over a chunked startup
/// sequence: `feed` steps PTY chunks, `tick` runs detector ticks, and `ready`
/// is the readiness after the last step.
struct LatchProbe {
    detector: Detector,
    columns: u16,
    now: Instant,
}

impl LatchProbe {
    /// A detector with `config`'s manifest, as tall as the tallest capture the
    /// sequence feeds.
    fn start(config: DetectorConfig, rows: u16, columns: u16) -> Self {
        Self {
            detector: Detector::new(rows, columns, Instant::now(), config),
            columns,
            now: Instant::now(),
        }
    }

    /// Feeds one PTY chunk.
    fn feed(&mut self, chunk: &[u8]) {
        self.now += Duration::from_millis(100);
        self.detector.feed(self.now, chunk);
    }

    fn tick(&mut self, count: usize) {
        for _ in 0..count {
            self.now += Duration::from_millis(100);
            self.detector.tick(self.now);
        }
    }

    /// Asserts the readiness after the last step.
    fn assert_ready(&self, expected: bool, context: &str) {
        assert_eq!(
            self.detector.input_ready(),
            expected,
            "{:>4} columns: {context}",
            self.columns
        );
    }
}

/// An adversarial startup sequence: a synthetic cached idle title, then the
/// update popup it never set, drawn over the screen at `columns`. The prompt
/// must not be released while any of this holds.
fn latched_popup_probe(config: DetectorConfig, columns: u16) -> (LatchProbe, Vec<String>) {
    let rows = popup_rows(columns);
    let height = u16::try_from(rows.len()).expect("capture rows").max(24);
    let mut probe = LatchProbe::start(config, height, columns);
    probe.feed(&frame_bytes(Some(FAKE_TITLE), &rows));
    probe.assert_ready(false, "the popup holds the prompt");
    (probe, rows)
}

/// The strict latch of the production Codex detector: the popup and each
/// split release path outlive every old piece of evidence, and only newly
/// received idle evidence (a title, even one repeating its value) releases.
#[test]
fn manifest_idle_ready_only_on_idle_evidence_newer_than_the_blocker() {
    let definition = installed_definition();
    let columns = DEFAULT_COLUMNS;
    let (mut probe, rows) = latched_popup_probe(manifest_idle_probe_config(&definition), columns);
    // The same title received again while the blocker holds must not arm
    // future readiness.
    probe.feed(&frame_bytes(Some(FAKE_TITLE), &rows));
    probe.assert_ready(
        false,
        "a fresh title received while the blocker holds must not arm readiness",
    );

    // A clear chunk alone removes the blocker; the cached title is older than
    // the blocker and must not release the latch, whatever repaints follow.
    probe.feed(&clear_chunk());
    probe.assert_ready(
        false,
        "a screen clear with only the cached title stays latched",
    );
    probe.feed(rows[8..].join("\r\n").as_bytes());
    probe.assert_ready(
        false,
        "a partial popup repaint without the heading stays latched",
    );
    probe.feed(&frame_bytes(None, &rows));
    probe.assert_ready(false, "a full popup redraw re-arms the latch");

    // The composer redraws without any fresh title; progress, process-only
    // bytes and ticks are not idle evidence either.
    probe.feed(&clear_chunk());
    probe.assert_ready(false, "the clear alone must not release the re-armed latch");
    probe.feed(&clear_composer_chunk(&composer_rows()));
    probe.assert_ready(
        false,
        "a composer redraw with the stale cached title stays latched",
    );
    probe.feed(b"\x1b]9;60%\x07");
    probe.tick(8);
    probe.assert_ready(false, "progress and ticks never clear the latch");
    probe.feed(b"\x1b[?25l");
    probe.assert_ready(false, "process-only bytes never clear the latch");

    // The release: a newly received idle title, same value as the cached one.
    // The repeated value itself no longer re-emits a transition — the
    // debounced classification keeps its title-change semantics — but the
    // reception is fresh idle evidence arriving after the blocker's last
    // sighting, so it releases the latch.
    probe.feed(&title_chunk(FAKE_TITLE));
    probe.assert_ready(
        true,
        "an idle title received after the blocker's last sighting releases the latch",
    );
}

/// A PTY chunk that coalesces the stale idle title, the full captured popup,
/// and the clear onto the provisional composer, the way one burst of startup
/// output can arrive.
fn coalesced_popup_chunk(rows: &[String], composer: &[String]) -> Vec<u8> {
    let mut chunk = frame_bytes(Some(FAKE_TITLE), rows);
    chunk.extend(clear_composer_chunk(composer));
    chunk
}

/// The initial popup must be sighted before a VT clear inside the same PTY
/// chunk erases it: an initial chunk with the title, the full popup, and the
/// provisional composer in one feed latches exactly like the same bytes split
/// after the popup, at every capture width. Partial popup repaints without
/// the heading keep the latch, and quiet blank output with the title-only
/// manifest is never readiness; only fresh idle evidence after the popup is
/// gone (a repeated idle title) releases it.
#[test]
fn manifest_idle_latches_the_coalesced_initial_popup_like_the_split_one() {
    let definition = installed_definition();
    let config = manifest_idle_probe_config(&definition);
    let composer = composer_rows();
    for columns in POPUP_CAPTURE_COLUMNS {
        let rows = popup_rows(columns);
        let height = u16::try_from(rows.len()).expect("capture rows").max(24);

        // Rule out the chunking itself: split after the popup, the sequence
        // stays latched the whole way.
        let mut split = LatchProbe::start(config.clone(), height, columns);
        split.feed(&frame_bytes(Some(FAKE_TITLE), &rows));
        split.assert_ready(false, "split: the popup holds the prompt");
        split.feed(&clear_composer_chunk(&composer));
        split.assert_ready(false, "split: the cached title stays stale after the clear");

        // Coalesced: the same bytes in one feed. The popup exists on screen
        // before the completed clear erases it, so the latch must still see it.
        let mut coalesced = LatchProbe::start(config.clone(), height, columns);
        coalesced.feed(&coalesced_popup_chunk(&rows, &composer));
        coalesced.assert_ready(
            false,
            "coalesced: the popup sighted before its clear holds the prompt",
        );
        coalesced.tick(4);
        coalesced.assert_ready(false, "coalesced: ticks do not lift a latched popup");

        // A clear with only a partial popup repaint without the heading must
        // not release...
        coalesced.feed(&clear_chunk());
        coalesced.assert_ready(false, "coalesced: the clear alone never releases the latch");
        coalesced.feed(rows[8..].join("\r\n").as_bytes());
        coalesced.assert_ready(
            false,
            "coalesced: a partial popup repaint without the heading stays latched",
        );
        // ...and a full popup redraw re-arms the latch.
        coalesced.feed(&frame_bytes(None, &rows));
        coalesced.feed(&clear_chunk());
        coalesced.assert_ready(
            false,
            "coalesced: the clear after a redrawn popup keeps the latch",
        );

        // Blank quiet output is not readiness under the title-only manifest.
        coalesced.tick(4);
        coalesced.assert_ready(false, "coalesced: a quiet blank screen is not readiness");

        // The release needs fresh idle evidence after the popup is gone.
        coalesced.feed(&title_chunk(FAKE_TITLE));
        coalesced.assert_ready(
            true,
            "coalesced: a fresh idle title after the popup lifts the latch",
        );
    }
}

#[test]
fn manifest_idle_keeps_the_popup_hold_across_implicit_scroll() {
    let definition = installed_definition();
    let popup = [
        "Update available! 0.160.0 -> 0.162.1",
        "› 1. Update now",
        "2. Skip",
        "3. Skip until next version",
    ]
    .map(str::to_owned);
    for columns in POPUP_CAPTURE_COLUMNS {
        let height = u16::try_from(popup_rows(columns).len()).expect("capture height");
        for (coalesced, styled) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut probe =
                LatchProbe::start(manifest_idle_probe_config(&definition), height, columns);
            let mut bytes = frame_bytes(Some(FAKE_TITLE), &popup);
            if !coalesced {
                probe.feed(&bytes);
                probe.assert_ready(false, "the minimal complete popup blocks");
                bytes.clear();
            }
            // Spaces preserve the last choice's text until automatic wrapping
            // scrolls the popup away, including SGR at the right margin.
            let padding: &[u8] = if styled { b" \x1b[0m" } else { b" " };
            bytes.extend(padding.repeat(usize::from(columns) * usize::from(height) * 2));
            bytes.extend(clear_composer_chunk(&composer_rows()));
            probe.feed(&bytes);
            probe.assert_ready(false, "implicit scrolling cannot hide a blocker sighting");
            probe.feed(&title_chunk(FAKE_TITLE));
            probe.assert_ready(true, "fresh idle evidence releases after the scroll");
        }
    }
}

fn manifest_idle_probe_config(definition: &RuntimeDefinition) -> DetectorConfig {
    DetectorConfig {
        input_readiness: InputReadiness::ManifestIdle,
        detection: latch_detection_config(),
        manifest: Some(Arc::clone(definition.manifest())),
    }
}

/// Feeds one popup-latched probe with a chunking of the synthetic sequence
/// and returns the probe after four settling ticks.
fn run_chunks(definition: &RuntimeDefinition, columns: u16, chunking: &[Vec<u8>]) -> LatchProbe {
    let (mut probe, _) = latched_popup_probe(manifest_idle_probe_config(definition), columns);
    for chunk in chunking {
        probe.feed(chunk);
    }
    probe.tick(4);
    probe
}

/// The strict readiness must not depend on how the PTY coalesces the same raw
/// byte stream after observing the popup. A synthetic title before its
/// clear must not release it; with the title last, every tested chunking
/// releases it even when the title repeats its value.
#[test]
fn manifest_idle_readiness_is_the_same_under_every_chunking_of_one_sequence() {
    let definition = installed_definition();
    let composer = composer_rows();
    let title = title_chunk(FAKE_TITLE);
    let (prefix, terminator) = title.split_at(title.len() - 1);
    for columns in POPUP_CAPTURE_COLUMNS {
        let chunkings: Vec<Vec<Vec<u8>>> = vec![
            vec![{
                let mut one = title_chunk(FAKE_TITLE);
                one.extend(frame_bytes(None, &composer));
                one
            }],
            vec![title_chunk(FAKE_TITLE), clear_composer_chunk(&composer)],
            vec![
                prefix.to_vec(),
                [terminator.to_vec(), clear_composer_chunk(&composer)].concat(),
            ],
            vec![
                {
                    let mut title_and_clear = title_chunk(FAKE_TITLE);
                    title_and_clear.extend(clear_chunk());
                    title_and_clear
                },
                clear_composer_chunk(&composer),
            ],
        ];
        for chunking in &chunkings {
            let mut probe = run_chunks(&definition, columns, chunking);
            probe.assert_ready(
                false,
                "a title arriving before the popup's clear stays latched under any chunking",
            );
            probe.feed(&title_chunk(FAKE_TITLE));
            probe.assert_ready(
                true,
                "a title received later, after the clear and composer, releases",
            );
        }

        let chunkings: Vec<Vec<Vec<u8>>> = vec![
            vec![{
                let mut one = clear_composer_chunk(&composer);
                one.extend(title_chunk(FAKE_TITLE));
                one
            }],
            vec![clear_composer_chunk(&composer), title_chunk(FAKE_TITLE)],
            vec![
                clear_composer_chunk(&composer),
                prefix.to_vec(),
                terminator.to_vec(),
            ],
            vec![
                clear_chunk(),
                frame_bytes(None, &composer),
                title_chunk(FAKE_TITLE),
            ],
        ];
        for chunking in &chunkings {
            let probe = run_chunks(&definition, columns, chunking);
            probe.assert_ready(
                true,
                "a title received after the clear and composer releases under any chunking",
            );
        }
    }
}

#[test]
fn manifest_idle_keeps_the_popup_hold_across_reconfigure_and_lag() {
    let definition = installed_definition();
    let config = manifest_idle_probe_config(&definition);
    let (mut probe, _) = latched_popup_probe(config.clone(), DEFAULT_COLUMNS);
    probe.detector.reconfigure(probe.now, config);
    probe.feed(&clear_composer_chunk(&composer_rows()));
    probe.assert_ready(false, "reconfigure does not revive the cached idle title");
    probe.detector.resync_after_lag();
    probe.assert_ready(false, "lost output requires new evidence");
    probe.feed(&clear_composer_chunk(&composer_rows()));
    probe.assert_ready(false, "a composer without its title is still not ready");
    probe.feed(&title_chunk(FAKE_TITLE));
    probe.assert_ready(true, "a new idle title releases after resync");
}

/// A repeated title in the same chunk retains the first changed title's
/// activity evidence under the packaged Codex manifest.
#[test]
fn coalesced_titles_keep_the_package_activity_signal() {
    let definition = installed_definition();
    let now = Instant::now();
    let mut detector = Detector::new(
        24,
        DEFAULT_COLUMNS,
        now,
        manifest_idle_probe_config(&definition),
    );
    let mut bytes = title_chunk("⠋ work");
    bytes.extend(title_chunk("⠋ work"));
    let transitions = detector.feed(now, &bytes);
    assert!(
        transitions.iter().any(|transition| {
            transition.activity == AgentActivity::Working
                && transition.source == StateSource::OscTitle
        }),
        "repeating a changed title must retain its activity evidence"
    );
}

/// Fresh idle evidence that arrives after the blocker's disappearance is
/// never title-only: with an idle rule on a screen region added to the
/// shipped manifest, the composer redraw itself releases the latched prompt.
fn manifest_with_screen_idle_rule() -> String {
    let mut source = fs::read_to_string(package_dir().join("detect.toml"))
        .expect("read the shipped Codex manifest");
    source.push_str(
        "\n[[rules]]\nid = \"fresh_composer_idle\"\nstate = \"idle\"\npriority = 1500\n\
         region = \"whole_recent\"\ncontains = [\"Ask Codex to do anything\"]\n",
    );
    source
}

#[test]
fn manifest_idle_releases_on_fresh_screen_idle_evidence() {
    let columns = DEFAULT_COLUMNS;
    let config = DetectorConfig {
        input_readiness: InputReadiness::ManifestIdle,
        detection: latch_detection_config(),
        manifest: Some(Arc::new(
            Manifest::parse_str(&manifest_with_screen_idle_rule())
                .expect("the manifest with the screen idle rule parses"),
        )),
    };
    let (mut probe, _rows) = latched_popup_probe(config, columns);

    // A title that arrives before the popup's clear stays latched; the fresh
    // composer redraw afterwards is itself the idle evidence the screen rule
    // reads, so it releases without waiting for any title.
    let mut held = title_chunk(FAKE_TITLE);
    held.extend(clear_chunk());
    probe.feed(&held);
    probe.tick(4);
    probe.assert_ready(
        false,
        "a title before the clear keeps the latch even under a screen idle rule",
    );
    probe.feed(&clear_composer_chunk(&composer_rows()));
    probe.tick(4);
    probe.assert_ready(
        true,
        "the freshly arrived composer redraw releases through the screen idle rule",
    );
}

/// The shipped Codex manifest extended with a fresh-screen idle rule and a
/// title blocker: the pairing the strict-latch screen regressions drive.
fn screen_idle_manifest_with_title_blocker() -> String {
    let mut source = manifest_with_screen_idle_rule();
    source.push_str(
        "\n[[rules]]\nid = \"title_blocker\"\nstate = \"blocked\"\npriority = 2000\n\
         region = \"osc_title\"\nvisible_blocker = true\ncontains = [\"Action Required\"]\n",
    );
    source
}

/// A probe whose title blocker is armed while the composer is drawn under it
/// and whose blocker title was then cleared: the composer sits on screen,
/// unchanged by any later output, and no redraw arrived after the clearing.
fn title_blocker_probe(config: DetectorConfig, columns: u16) -> LatchProbe {
    let (mut probe, _) = latched_popup_probe(config, columns);
    probe.feed(&title_chunk("Action Required"));
    let mut held = clear_composer_chunk(&composer_rows());
    held.extend(title_chunk(""));
    probe.feed(&held);
    probe
}

#[test]
fn manifest_idle_rejects_screen_evidence_received_while_a_title_blocker_holds() {
    let definition = installed_definition();
    let mut config = manifest_idle_probe_config(&definition);
    config.manifest = Some(Arc::new(
        Manifest::parse_str(&screen_idle_manifest_with_title_blocker())
            .expect("title blocker manifest"),
    ));
    let probe = title_blocker_probe(config, DEFAULT_COLUMNS);
    probe.assert_ready(
        false,
        "screen evidence before the title blocker clears is stale",
    );
}

#[test]
fn manifest_idle_identical_redraw_releases_under_every_chunking() {
    let definition = installed_definition();
    let mut config = manifest_idle_probe_config(&definition);
    config.manifest = Some(Arc::new(
        Manifest::parse_str(&screen_idle_manifest_with_title_blocker())
            .expect("title blocker manifest"),
    ));
    let redraw = clear_composer_chunk(&composer_rows());
    let (clear, repaint) = redraw.split_at(clear_chunk().len());
    for columns in POPUP_CAPTURE_COLUMNS {
        // Coalesced: the clear and the identical redraw arrive together, so
        // the segment's final text is the text it started from. The redraw
        // has still drawn after the blocker cleared and must be fresh.
        let mut probe = title_blocker_probe(config.clone(), columns);
        probe.assert_ready(
            false,
            "after clearing the blocker title, before any redraw, stays latched",
        );
        probe.feed(&redraw);
        probe.assert_ready(
            true,
            "the coalesced identical redraw counts as fresh screen evidence",
        );

        // Split: the same bytes split between the clear and the redraw.
        let mut probe = title_blocker_probe(config.clone(), columns);
        probe.feed(clear);
        probe.assert_ready(
            false,
            "after the clear but before the repaint still stays latched",
        );
        probe.feed(repaint);
        probe.assert_ready(true, "the split identical redraw releases the same way");
    }
}

#[test]
fn manifest_idle_counts_a_final_erase_as_fresh_screen_evidence() {
    let definition = installed_definition();
    let mut config = manifest_idle_probe_config(&definition);
    config.manifest = Some(Arc::new(
        Manifest::parse_str(&screen_idle_manifest_with_title_blocker())
            .expect("title blocker manifest"),
    ));
    for columns in POPUP_CAPTURE_COLUMNS {
        let mut probe = title_blocker_probe(config.clone(), columns);
        probe.assert_ready(false, "the composer is still stale");
        // Erase the header row, leaving the screen-idle composer's placeholder.
        // The erasing CSI is the final byte sequence in this feed.
        probe.feed(b"\x1b[2;1H\x1b[2K");
        probe.assert_ready(true, "the final VT action changed the idle screen");
    }
}

#[test]
fn manifest_idle_control_output_does_not_refresh_a_stale_composer() {
    let definition = installed_definition();
    let mut config = manifest_idle_probe_config(&definition);
    config.manifest = Some(Arc::new(
        Manifest::parse_str(&screen_idle_manifest_with_title_blocker())
            .expect("title blocker manifest"),
    ));
    let controls: &[(&str, &[u8])] = &[
        ("incomplete UTF-8", b"\xe2"),
        ("ignored C1 character", b"\xc2\x80"),
        ("replacement character", "\u{fffd}".as_bytes()),
        ("combining mark at cursor origin", b"\x1b[H\xcc\x81"),
        ("cursor and style", b"\x1b[H\x1b[0m\x1b[?25h"),
        ("OSC progress", b"\x1b]9;4;0;0\x07"),
        (
            "DCS payload including BEL",
            b"\x1bPq\x07not screen text\x1b\\",
        ),
        ("split OSC prefix", b"\x1b]52;c;not screen text"),
    ];
    for columns in POPUP_CAPTURE_COLUMNS {
        for &(name, bytes) in controls {
            for fragmented in [false, true] {
                let mut probe = title_blocker_probe(config.clone(), columns);
                if fragmented {
                    for byte in bytes {
                        probe.feed(std::slice::from_ref(byte));
                        probe.assert_ready(false, name);
                    }
                } else {
                    probe.feed(bytes);
                    probe.assert_ready(false, name);
                }
            }
        }
    }
}

/// A noisy terminal stream crosses the real VT parser and Codex manifest
/// without turning each formatting control into a full-grid observation.
#[test]
fn manifest_idle_formatting_storm_has_bounded_screen_observations() {
    let definition = installed_definition();
    for columns in POPUP_CAPTURE_COLUMNS {
        let (mut probe, rows) =
            latched_popup_probe(manifest_idle_probe_config(&definition), columns);
        let height = u16::try_from(rows.len()).expect("capture height");
        let mut screen = pohunek_daemon::detect::ScreenTracker::new(height, columns);
        screen.feed(&frame_bytes(None, &rows));
        let original = screen.recent_text();
        let resets = b"\x1b[0m".repeat(4096);
        let cursor = b"\x1b[H\x1b[0m".repeat(2048);
        for bytes in [&resets, &cursor] {
            let mut observations = 0;
            screen.feed_observed(bytes, &mut |_, _| observations += 1);
            assert!(
                observations <= 1,
                "formatting-only chunk caused {observations} grid observations"
            );
            assert_eq!(screen.recent_text(), original);
            probe.feed(bytes);
            probe.assert_ready(false, "formatting never releases the popup hold");
        }
        // Style changes between every printable fragment still need only
        // the final observation when no cursor movement or wrapping occurs.
        let colored = b"\x1b[31mx\x1b[0m".repeat(usize::from(columns) / 2);
        screen.feed(b"\x1b[H");
        let mut observations = 0;
        screen.feed_observed(&colored, &mut |_, _| observations += 1);
        assert_eq!(observations, 1, "SGR does not split a printable run");
        screen.finish_observation();
        screen.reset();
        screen.feed_observed(&colored, &mut |_, _| {
            panic!("finished observation re-armed")
        });
        assert!(
            screen.recent_text().contains('x'),
            "grid updates continue after observation ends"
        );
        screen.resume_observation();
        screen.feed(b"\x1b[H");
        let mut observations = 0;
        screen.feed_observed(&colored, &mut |_, _| observations += 1);
        assert_eq!(observations, 1, "a resumed observation reports again");
        screen.resume_observation();
        screen.feed(b"\x1b[H");
        let mut observations = 0;
        screen.feed_observed(&colored, &mut |_, _| observations += 1);
        assert_eq!(
            observations, 1,
            "resuming a live observation changes nothing"
        );
    }
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
        ("\u{280b} my-thread | project", working),
        // The title-generation placeholder of an idle unnamed thread, and a
        // spinner that names nothing, are not a running turn; the idle rule
        // skips every spinner title, so they assert no state at all (#1486).
        ("\u{2834} | project", None),
        ("\u{28ff} ", None),
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
    // An npm install starts `node …/@openai/codex/bin/codex.js`, which spawns
    // the native binary below the package and that binary starts the
    // app-server. The launcher matches neither pattern; the processes it
    // starts do.
    let native = "/opt/npm/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin/codex";
    assert!(!matchers.matches(&process(
        "node",
        &[
            "node",
            "/opt/npm/lib/node_modules/@openai/codex/bin/codex.js"
        ]
    )));
    assert!(matchers.matches(&process("codex", &[native])));
    assert!(matchers.matches(&process(
        "codex",
        &[
            "/home/user/.codex/packages/app-server-daemon/releases/0.160.0-x86_64-unknown-linux-musl/bin/codex",
            "app-server",
            "--listen",
            "unix://",
            "--managed-daemon"
        ]
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

/// Everything the golden built-in manifest of release v0.34.2 classifies, the
/// package manifest classifies the same way: the captured frames with and
/// without their titles, and the synthetic dialogs of the rule tests. The
/// login dialogs are checked separately above. The folder dialog is checked by
/// `the_folder_trust_dialog_is_blocked_from_the_screen_alone`.
#[test]
fn the_package_manifest_behaves_like_the_builtin_manifest_on_every_frame() {
    let package = installed_definition();
    let golden = golden_definition();
    let mut compared = 0_usize;
    for frame in top_level_frames().into_iter().chain(width_frames()) {
        if frame.meaning() == Meaning::ScreenBlocked || frame.state == "login" {
            continue;
        }
        for title in [frame.title.as_deref(), None] {
            assert_eq!(
                transitions(&package, title, &frame.rows, frame.columns),
                transitions(&golden, title, &frame.rows, frame.columns),
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
            transitions(&golden, None, &dialog(lines), 80),
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
            title_activity(&golden, title),
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
        self.write_profile_with_args(name, program, &[]);
    }

    /// [`Self::write_profile`] with the profile's fixed launch arguments.
    fn write_profile_with_args(&self, name: &str, program: Option<&Path>, args: &[String]) {
        let program = program.map_or_else(String::new, |program| {
            format!("program = \"{}\"\n", path_str(program))
        });
        let args = if args.is_empty() {
            String::new()
        } else {
            let list = args
                .iter()
                .map(|argument| format!("\"{argument}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!("args = [{list}]\n")
        };
        self.harness.profile(
            name,
            &format!(
                "base = \"codex\"\npackage = \"{PACKAGE_ID}\"\ndigest = \"{digest}\"\n{program}{args}\n[env]\nCODEX_HOME = \"{home}\"\n",
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

    // The SessionStart hook reported the launch conversation id used for
    // resume. Active-agent metadata is optional and belongs to another view.
    let diagnostics = NativeReferenceDiagnostics::new(
        fixture
            .harness
            .env
            .root()
            .join("worker/state/pohunek/workers"),
        fixture.harness.env.root().join("worker/state/pohunek/logs"),
        None,
        &id,
    );
    let reference = wait_until("the hook-reported native reference", || async {
        let (code, inspected) = fixture.harness.json(&["session", "inspect", &id]).await;
        assert_eq!(code, 0, "{inspected}");
        diagnostics.observe(&inspected["ok"]);
        inspected["ok"]["native_session_id"]
            .as_str()
            .map(str::to_owned)
    })
    .await;
    let evidence = diagnostics.describe();
    assert!(evidence.contains("journal: phase="), "{evidence}");
    assert!(evidence.contains("worker admission events:"), "{evidence}");
    assert!(!evidence.contains(&reference), "native reference leaked");
    drop(diagnostics);
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

/// Deepest process tree [`ancestry`] follows.
///
/// A launcher, the agent and its helper are three levels; the bound keeps a
/// walk that never meets its target finite.
const MAX_ANCESTRY_DEPTH: usize = 16;

/// The process ids from `pid` up through its parents, ending at `ancestor` or
/// after [`MAX_ANCESTRY_DEPTH`] steps, and whether `ancestor` was reached.
fn ancestry(pid: u64, ancestor: u64) -> (Vec<u64>, bool) {
    let mut chain = vec![pid];
    while chain.len() <= MAX_ANCESTRY_DEPTH {
        let last = *chain.last().expect("a non-empty chain");
        if last == ancestor {
            return (chain, true);
        }
        if last <= 1 {
            break;
        }
        chain.push(parent_pid(last));
    }
    (chain, false)
}

/// One line per process of `chain`: id, executable and command line.
fn describe_chain(chain: &[u64]) -> String {
    chain
        .iter()
        .map(|pid| {
            let exe = fs::read_link(format!("/proc/{pid}/exe"))
                .map_or_else(|_| "?".to_owned(), |path| path.display().to_string());
            let command = fs::read(format!("/proc/{pid}/cmdline")).map_or_else(
                |_| "?".to_owned(),
                |bytes| String::from_utf8_lossy(&bytes).replace('\0', " "),
            );
            format!("{pid} exe={exe} cmdline={command}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Exercise the real worker's rejection log with payload that must stay out of
/// the bounded timeout diagnostics.
async fn assert_private_hook_diagnostics(fixture: &Fixture, id: &str, pid: u64, reference: &str) {
    let socket = fixture
        .harness
        .env
        .root()
        .join("worker/runtime/pohunek/workers")
        .join(id)
        .join("control.sock");
    let poisoned_reference = format!("{reference} {PROMPT}");
    let expires_at = (time::OffsetDateTime::now_utc() + time::Duration::seconds(30))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC 3339 expiry");
    for sequence in 0..=MAX_EVENTS {
        let request = serde_json::json!({
            "type": "identity_report",
            "runtime_id": "foreign-runtime",
            "provider": "codex",
            "pid": pid,
            "start_identity": 1,
            "sequence": sequence,
            "expires_at": expires_at,
            "reference_kind": "id",
            "native_reference": poisoned_reference,
        });
        let stream = tokio::net::UnixStream::connect(&socket)
            .await
            .expect("connect to the private worker hook endpoint");
        let (read, mut write) = stream.into_split();
        let mut frame = serde_json::to_vec(&request).expect("serialize hook claim");
        frame.push(b'\n');
        write.write_all(&frame).await.expect("send hook claim");
        let mut reply = String::new();
        tokio::io::BufReader::new(read)
            .read_line(&mut reply)
            .await
            .expect("read hook refusal");
        let reply: Value = serde_json::from_str(&reply).expect("hook response JSON");
        assert_eq!(reply["ok"], false, "foreign runtime claim refused");
    }
    let diagnostics = NativeReferenceDiagnostics::new(
        fixture
            .harness
            .env
            .root()
            .join("worker/state/pohunek/workers"),
        fixture.harness.env.root().join("worker/state/pohunek/logs"),
        None,
        id,
    );
    let expected = format!("worker admission events: {MAX_EVENTS} shown, ");
    let evidence = wait_until("bounded redacted hook diagnostics", || async {
        let evidence = diagnostics.describe();
        let omitted = evidence.lines().find_map(|line| {
            line.strip_prefix(&expected)?
                .strip_suffix(" earlier omitted")?
                .parse::<usize>()
                .ok()
        });
        omitted.is_some_and(|count| count >= 1).then_some(evidence)
    })
    .await;
    assert!(!evidence.contains(reference), "native reference leaked");
    assert!(!evidence.contains(PROMPT), "prompt leaked");
    assert!(
        evidence.contains("reason=runtime_mismatch"),
        "the worker logged the rejection reason: {evidence}"
    );
}

/// Codex 0.160.0 runs its hooks from a `codex app-server` process below the one
/// pohunek launched (a child of it, or of the native binary an npm launcher
/// starts). The daemon records the conversation id that process reports as the
/// session's native reference, so `session resume` has something to resume with.
///
/// Needs a `codex` on `PATH` and `POHUNEK_CODEX_E2E=1`; the model is a
/// loopback stub.
#[tokio::test]
#[ignore = "needs a real `codex` on PATH; run with POHUNEK_CODEX_E2E=1 and --ignored"]
#[expect(
    clippy::too_many_lines,
    reason = "one real-agent lifecycle proves the descendant claim across a completed Working rescan and resume"
)]
async fn a_real_codex_reports_hooks_from_a_descendant_of_the_launched_process_and_resume_has_its_reference(
) {
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

    let reported = wait_until("the hook-reported native reference", || async {
        let (code, inspected) = fixture.harness.json(&["session", "inspect", &id]).await;
        assert_eq!(code, 0, "{inspected}");
        inspected["ok"]["native_session_id"]
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
    let (chain, reached) = ancestry(reporter_pid, launch_pid);
    let tree = describe_chain(&chain);
    eprintln!("process chain from the reporter up to the launch process:\n{tree}");
    assert!(
        reached,
        "the reporting process descends from the launched process; its ancestry:\n{tree}"
    );
    let reference = reported["native_session_id"]
        .as_str()
        .expect("the native reference")
        .to_owned();
    assert_eq!(
        reported["active_agent_session_id"], reference,
        "the nested reporter's id is the session's reference: {reported}"
    );
    assert!(
        fixture.rollout_file(&reference).is_some(),
        "Codex wrote a rollout file for the reported conversation {reference}"
    );

    assert_private_hook_diagnostics(&fixture, &id, reporter_pid, &reference).await;
    fixture.wait_activity(&id, "idle").await;
    let requests_before_working = fixture.stub.started();
    let replies_before_working = fixture.stub.finished();

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
    wait_until("the second model response is held", || async {
        (fixture.stub.started() > requests_before_working
            && fixture.stub.finished() == replies_before_working
            && !fixture
                .stub
                .bodies_containing(responses_stub::HOLD_MARKER)
                .is_empty())
        .then_some(())
    })
    .await;

    // Rename the real kernel cwd only after Working. Procwatch publishes its
    // cwd after reconciling identity, so observing the new path is a completed
    // rescan barrier. Recreate the old path for the CLI fixture's own commands.
    // The held response prevents a later turn-end hook masking replacement.
    let cwd = fixture.harness.env.cwd();
    let rescanned_cwd = fixture.harness.env.root().join("work-after-working");
    fs::rename(cwd, &rescanned_cwd).expect("rename the live process cwd");
    fs::DirBuilder::new()
        .mode(plugin_harness::PRIVATE_MODE)
        .create(cwd)
        .expect("restore the private CLI command cwd");
    let after_rescan = wait_until("procwatch reconciled the Working turn", || async {
        let (code, inspected) = fixture.harness.json(&["session", "inspect", &id]).await;
        assert_eq!(code, 0, "{inspected}");
        (inspected["ok"]["cwd"] == path_str(&rescanned_cwd)
            && inspected["ok"]["cwd_source"] == "procwatch")
            .then(|| inspected["ok"].clone())
    })
    .await;
    assert_eq!(
        after_rescan["active_agent_pid"], reporter_pid,
        "{after_rescan}"
    );
    assert_eq!(
        after_rescan["active_agent_session_id"], reference,
        "{after_rescan}"
    );
    assert_eq!(
        after_rescan["native_session_id"], reference,
        "{after_rescan}"
    );
    eprintln!("the reported descendant survived a completed Working rescan");
    fs::remove_dir(cwd).expect("remove the empty CLI cwd placeholder");
    fs::rename(&rescanned_cwd, cwd).expect("restore the agent's original cwd");
    wait_until("procwatch restored the original cwd", || async {
        let (code, inspected) = fixture.harness.json(&["session", "inspect", &id]).await;
        assert_eq!(code, 0, "{inspected}");
        (inspected["ok"]["cwd"] == path_str(cwd) && inspected["ok"]["cwd_source"] == "procwatch")
            .then_some(())
    })
    .await;
    fixture.stub.open_gate();
    fixture.wait_activity(&id, "idle").await;
    eprintln!("the held Working turn completed after restoring the cwd");

    fixture.stop(&id).await;
    let (code, resumed) = fixture.harness.json(&["session", "resume", &id]).await;
    assert_eq!(code, 0, "{resumed}");
    fixture.wait_activity(&id, "idle").await;
    let (code, inspected) = fixture.harness.json(&["session", "inspect", &id]).await;
    assert_eq!(code, 0, "{inspected}");
    assert_eq!(
        inspected["ok"]["native_session_id"], reference,
        "the resumed session keeps the conversation it resumed: {inspected}"
    );
    fixture.stop(&id).await;
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
async fn installing_the_package_by_digest_is_refused_because_codex_is_reserved() {
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
    write_fake_codex_exiting(root, directory, banner, 0)
}

/// [`write_fake_codex`] whose `--version` exits with `status`.
fn write_fake_codex_exiting(root: &Path, directory: &str, banner: &str, status: u8) -> PathBuf {
    let path = root.join(directory).join("codex");
    fs::create_dir_all(root.join(directory)).expect("create the fake codex directory");
    write_executable(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo '{banner}'\n  exit {status}\nfi\nexec sleep 600\n"),
    )
    .expect("write the fake codex");
    path
}

/// How the version check must classify a fake `codex`.
#[derive(Debug, Clone, Copy)]
enum ProbeVerdict {
    /// The banner names this release, outside the locked supported range.
    OutOfRange(&'static str),
    /// The banner is not the declared `codex-cli {version}` line.
    Unparseable,
    /// The probe itself fails.
    Failed,
}

impl ProbeVerdict {
    /// The fragments the launch refusal message and the doctor detail must
    /// hold, from the compatibility lock's supported range.
    fn expected(self) -> Vec<String> {
        let lock = read_lock();
        let range = format!(
            "[{}, {})",
            lock["supported"]["min"].as_str().expect("lock min"),
            lock["supported"]["below"].as_str().expect("lock below")
        );
        match self {
            Self::OutOfRange(version) => vec![format!("reports version {version}"), range],
            Self::Unparseable => vec!["does not match its runtime's declared format".to_owned()],
            Self::Failed => vec![],
        }
    }
}

/// The version probe the package declares decides every launch of `codex`, on
/// the executable the profile names: only the supported release starts. A
/// refused launch and `pohunek doctor` name the parsed release and the
/// supported range, or that the output did not parse, and never echo the
/// banner itself.
#[tokio::test]
async fn the_package_version_probe_refuses_an_unsupported_codex_and_starts_the_supported_one() {
    let fixture = Fixture::start().await;
    let root = fixture.harness.env.root().to_path_buf();
    let release = locked_release();
    let unsupported = [
        (
            "codex-cli 0.159.9".to_owned(),
            0,
            ProbeVerdict::OutOfRange("0.159.9"),
        ),
        (
            "codex-cli 1.0.0".to_owned(),
            0,
            ProbeVerdict::OutOfRange("1.0.0"),
        ),
        (
            format!("codex-cli {release}-rc.1"),
            0,
            ProbeVerdict::Unparseable,
        ),
        (format!("codex {release}"), 0, ProbeVerdict::Unparseable),
        ("unreadable".to_owned(), 0, ProbeVerdict::Unparseable),
        (format!("codex-cli {release}"), 3, ProbeVerdict::Failed),
    ];
    for (index, (banner, status, verdict)) in unsupported.iter().enumerate() {
        let profile = format!("fake-unsupported-{index}");
        let program = write_fake_codex_exiting(&root, &profile, banner, *status);
        fixture.write_profile(&profile, Some(&program));

        let entry = fixture.inventory_entry(&profile).await;
        assert_eq!(entry["supported"], false, "{banner:?}: {entry}");
        let (code, refused) = fixture.launch(&profile, "refused").await;
        assert_eq!(code, 1, "{banner:?}: {refused}");
        assert_eq!(
            refused["err"]["code"], "agent_runtime_unsupported",
            "{banner:?}: {refused}"
        );
        let message = refused["err"]["msg"].as_str().expect("refusal message");
        if matches!(verdict, ProbeVerdict::Failed) {
            assert!(
                message.starts_with(
                    "the selected agent executable did not pass this daemon's version check"
                ),
                "{refused}"
            );
            assert!(refused["err"]["recover"].is_null(), "{refused}");
        } else {
            for fragment in verdict.expected() {
                assert!(message.contains(&fragment), "{fragment:?}: {refused}");
            }
            assert!(refused["err"]["recover"].is_string(), "{refused}");
        }
        if matches!(verdict, ProbeVerdict::Unparseable) {
            assert!(!refused.to_string().contains(banner.as_str()), "{refused}");
        }
    }

    let doctor = fixture.harness.run(&["doctor", "--json"]).await;
    let doctor: Value = serde_json::from_slice(&doctor.stdout).expect("doctor JSON");
    for (index, (banner, _, verdict)) in unsupported.iter().enumerate() {
        let name = format!("agent_profile_fake-unsupported-{index}");
        let check = doctor["ok"]["checks"]
            .as_array()
            .expect("doctor checks")
            .iter()
            .find(|check| check["name"] == name.as_str())
            .unwrap_or_else(|| panic!("doctor reports {name}: {doctor}"));
        assert_eq!(check["status"], "fail", "{check}");
        let detail = check["detail"].as_str().expect("doctor detail");
        if matches!(verdict, ProbeVerdict::Failed) {
            assert!(detail.contains("version probe failed"), "{check}");
        } else {
            for fragment in verdict.expected() {
                assert!(detail.contains(&fragment), "{fragment:?}: {check}");
            }
        }
        if matches!(verdict, ProbeVerdict::Unparseable) {
            assert!(!detail.contains(banner.as_str()), "{check}");
        }
    }

    // The locked release and a later 0.x release, the one a host's Codex
    // auto-update installed (`codex-cli 0.162.1`), both launch.
    for (index, version) in [release.as_str(), "0.162.1"].into_iter().enumerate() {
        let profile = format!("fake-supported-{index}");
        let program = write_fake_codex(&root, &profile, &format!("codex-cli {version}"));
        fixture.write_profile(&profile, Some(&program));
        let entry = fixture.inventory_entry(&profile).await;
        assert_eq!(entry["supported"], true, "{entry}");
        assert_eq!(entry["version"], version, "{entry}");
        let (code, launched) = fixture.launch(&profile, "accepted").await;
        assert_eq!(code, 0, "{launched}");
        let id = launched["ok"]["id"]
            .as_str()
            .expect("session id")
            .to_owned();
        assert_eq!(launched["ok"]["agent_base"], "codex");
        fixture.stop(&id).await;
    }
    fixture.finish().await;
}

/// The recovery guidance a refused wrapper profile must carry (issue #1196):
/// the probe runs the program with only the package's probe arguments, so a
/// wrapper `program` must answer them itself, without the profile's fixed
/// arguments. Launch refusals and doctor details for unparseable and failed
/// probes carry it; raw probe output never appears.
#[cfg(target_os = "linux")]
fn assert_wrapper_guidance(text: &str, context: &str) {
    for fragment in [
        "wrapper",
        "version-probe arguments",
        "profile's fixed arguments",
    ] {
        assert!(
            text.contains(fragment),
            "{context} must tell the operator {fragment:?}: {text:?}"
        );
    }
}

/// Where the wrapper script named `directory` logs its invocations, one
/// space-joined argv per line.
#[cfg(target_os = "linux")]
fn wrapper_argv_log(root: &Path, directory: &str) -> PathBuf {
    root.join(format!("wrapper-{directory}.log"))
}

/// Writes the fake Codex a wrapper delegates to: a script whose interpreter
/// is a copy of `/bin/sh` named for the provider (the worker admits the
/// process as the launch process of a hook claim by its executable name),
/// which logs its argv, answers `--version` with `banner`, reports
/// `reference` through the real hook endpoint of its worker when launched,
/// and then idles.
#[cfg(target_os = "linux")]
fn write_wrapper_agent(root: &Path, directory: &str, banner: &str, reference: &str) -> PathBuf {
    let interpreter = root.join("fake-codex-sh");
    if !interpreter.exists() {
        copy_shell_image("/bin/sh", &interpreter)
            .expect("copy the shell image for the fake agent interpreter");
    }
    let reporter = write_identity_reporter(root);
    let path = root.join(directory).join("codex");
    fs::create_dir_all(path.parent().expect("the fake agent directory"))
        .expect("create the fake agent directory");
    write_executable(
        &path,
        format!(
            "#!{interpreter}\nprintf '%s\\n' \"$*\" >> '{agent_log}'\nif [ \"$1\" = \"--version\" ]; then\n  echo '{banner}'\n  exit 0\nfi\npython3 '{reporter}' '{reference}' '{report_log}'\nwhile :; do sleep 600; done\n",
            interpreter = path_str(&interpreter),
            agent_log = root.join(format!("agent-{directory}.log")).display(),
            reporter = reporter.display(),
            report_log = root.join("hook-report.log").display(),
        ),
    )
    .expect("write the fake agent a wrapper delegates to");
    path
}

/// Writes the Python script a fake agent runs to report a native reference
/// through the real hook endpoint of its worker: the claim names the parent
/// process (the launch process) and the reference, exactly as a provider
/// hook does, and appends the worker's reply to `hook-report.log`.
#[cfg(target_os = "linux")]
fn write_identity_reporter(root: &Path) -> PathBuf {
    let path = root.join("identity-reporter.py");
    fs::write(
        &path,
        r#"
import json, os, socket, sys, time
from datetime import datetime, timedelta, timezone

pid = os.getppid()
with open(f"/proc/{pid}/stat", encoding="ascii") as handle:
    start_identity = int(handle.read().rsplit(")", 1)[1].split()[19])
request = {
    "type": "identity_report",
    "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
    "provider": "codex",
    "pid": pid,
    "start_identity": start_identity,
    "sequence": time.clock_gettime_ns(time.CLOCK_MONOTONIC),
    "expires_at": (datetime.now(timezone.utc) + timedelta(seconds=30))
    .isoformat()
    .replace("+00:00", "Z"),
    "reference_kind": "id",
    "native_reference": sys.argv[1],
}
client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
client.connect(os.environ["POHUNEK_WORKER_SOCKET_PATH"])
client.sendall((json.dumps(request) + "\n").encode())
received = b""
while b"\n" not in received:
    chunk = client.recv(4096)
    if not chunk:
        raise RuntimeError(f"worker closed the socket before a full response line: {received!r}")
    received += chunk
client.close()
reply = json.loads(received.split(b"\n", 1)[0])
with open(sys.argv[2], "a", encoding="utf-8") as handle:
    handle.write(json.dumps(reply) + "\n")
"#,
    )
    .expect("write the identity reporter");
    path
}

/// Writes a direnv-style dispatch wrapper for the agent `target`: with the
/// profile's fixed arguments `exec <dir> <target>` it dispatches the agent
/// the way `direnv exec` would. `answers_the_probe` selects the conforming
/// version branch; `false` writes the wrapper that answers the package's
/// probe with its own version instead, which the daemon cannot parse.
#[cfg(target_os = "linux")]
fn write_direnv_wrapper(
    root: &Path,
    directory: &str,
    target: &Path,
    answers_the_probe: bool,
) -> PathBuf {
    let version_branch = if answers_the_probe {
        format!("exec '{target}' --version", target = target.display())
    } else {
        "echo '2.38.2'\n  exit 0".to_owned()
    };
    let log = wrapper_argv_log(root, directory);
    let path = root.join(directory).join("wrapper");
    fs::create_dir_all(path.parent().expect("the wrapper directory"))
        .expect("create the wrapper directory");
    write_executable(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\nif [ \"$1\" = \"--version\" ]; then\n  {version_branch}\nfi\nif [ \"$1\" = \"exec\" ] && [ \"$3\" = '{target}' ]; then\n  shift 3\n  exec '{target}' \"$@\"\nfi\necho 'unexpected dispatch arguments' >&2\nexit 64\n",
            log = log.display(),
            target = target.display(),
        ),
    )
    .expect("write the direnv-style wrapper");
    path
}

/// Writes a forwarding bridge for the agent `target`: with the profile's
/// fixed argument `<target>` it forwards every further argument to the
/// agent. `answers_the_probe` selects the conforming version branch;
/// `false` writes the bridge that treats the probe argument as the agent
/// path and fails the probe.
#[cfg(target_os = "linux")]
fn write_bridge_wrapper(
    root: &Path,
    directory: &str,
    target: &Path,
    answers_the_probe: bool,
) -> PathBuf {
    let version_branch = if answers_the_probe {
        format!(
            "if [ \"$1\" = \"--version\" ]; then\n  exec '{target}' --version\nfi\n",
            target = target.display()
        )
    } else {
        String::new()
    };
    let log = wrapper_argv_log(root, directory);
    let path = root.join(directory).join("wrapper");
    fs::create_dir_all(path.parent().expect("the wrapper directory"))
        .expect("create the wrapper directory");
    write_executable(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n{version_branch}target=$1\nshift\nexec \"$target\" \"$@\"\n",
            log = log.display(),
        ),
    )
    .expect("write the forwarding bridge");
    path
}

/// A wrapper profile (a `program` whose fixed arguments name the agent it
/// wraps) launches and resumes when the wrapper answers the package's
/// version probe itself: the probe runs the program with only the package's
/// probe arguments — never the fixed arguments or the profile environment —
/// the launch and the resume keep the fixed arguments, and the native
/// reference arrives through the real worker hook handshake.
#[cfg(target_os = "linux")]
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one integration scenario asserts the wrapper probe contract at the launch boundary"
)]
async fn a_conforming_wrapper_profile_launches_and_resumes_with_its_fixed_arguments() {
    let fixture = Fixture::start().await;
    let root = fixture.harness.env.root().to_path_buf();
    let cwd = fixture.harness.env.cwd().to_path_buf();
    let release = locked_release();

    let dispatch_agent = write_wrapper_agent(
        &root,
        "dispatch-agent",
        &format!("codex-cli {release}"),
        "wrapper-conversation-0",
    );
    let dispatch_wrapper = write_direnv_wrapper(&root, "dispatch-wrapper", &dispatch_agent, true);
    let bridge_agent = write_wrapper_agent(
        &root,
        "bridge-agent",
        &format!("codex-cli {release}"),
        "wrapper-conversation-1",
    );
    let bridge_wrapper = write_bridge_wrapper(&root, "bridge-wrapper", &bridge_agent, true);
    fixture.write_profile_with_args(
        "wrapper-dispatch-ok",
        Some(&dispatch_wrapper),
        &[
            "exec".to_owned(),
            cwd.display().to_string(),
            dispatch_agent.display().to_string(),
        ],
    );
    fixture.write_profile_with_args(
        "wrapper-bridge-ok",
        Some(&bridge_wrapper),
        &[bridge_agent.display().to_string()],
    );

    // The inventory probes the wrappers and reports the agent release.
    for profile in ["wrapper-dispatch-ok", "wrapper-bridge-ok"] {
        let entry = fixture.inventory_entry(profile).await;
        assert_eq!(entry["supported"], true, "{entry}");
        assert_eq!(entry["version"], release, "{entry}");
    }

    // Both wrappers launch.
    let mut sessions = Vec::new();
    for (profile, directory, agent, reference) in [
        (
            "wrapper-dispatch-ok",
            "dispatch-wrapper",
            &dispatch_agent,
            "wrapper-conversation-0",
        ),
        (
            "wrapper-bridge-ok",
            "bridge-wrapper",
            &bridge_agent,
            "wrapper-conversation-1",
        ),
    ] {
        let (code, launched) = fixture.launch(profile, "wrapper").await;
        assert_eq!(code, 0, "{launched}");
        let id = launched["ok"]["id"]
            .as_str()
            .expect("session id")
            .to_owned();
        sessions.push((directory, agent.display().to_string(), reference, id));
    }

    // The probe ran each wrapper with only the package's probe arguments,
    // and the launch kept the profile's fixed arguments. A returned launch is
    // not log readiness: the wrapper logs its argv only when the worker runs
    // it, which can trail the launch command, so wait for the exact launch
    // line and treat a missing or partially written log as pending.
    for (directory, launch_line) in [
        (
            "dispatch-wrapper",
            format!("exec {} {}", cwd.display(), dispatch_agent.display()),
        ),
        ("bridge-wrapper", bridge_agent.display().to_string()),
    ] {
        let log = wait_until("the wrapper's launch argv entry", || async {
            let log = fs::read_to_string(wrapper_argv_log(&root, directory)).ok()?;
            log.lines().any(|line| *line == launch_line).then_some(log)
        })
        .await;
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines.first(),
            Some(&"--version"),
            "the probe passes only the package's probe arguments: {log}"
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("--version") || *line == "--version"),
            "no probe invocation carries the fixed arguments: {log}"
        );
        assert!(
            lines.iter().any(|line| *line == launch_line),
            "the launch kept the profile's fixed arguments: {log}"
        );
    }

    // The hook handshake reported each conversation, and the resume keeps
    // the fixed arguments and the reference intact.
    for (directory, agent, reference, id) in sessions {
        let reported = wait_until("the hook-reported native reference", || async {
            let (code, inspected) = fixture.harness.json(&["session", "inspect", &id]).await;
            assert_eq!(code, 0, "{inspected}");
            inspected["ok"]["native_session_id"]
                .is_string()
                .then(|| inspected["ok"].clone())
        })
        .await;
        assert_eq!(
            reported["native_session_id"], reference,
            "the wrapper's agent reported the conversation: {reported}"
        );
        fixture.stop(&id).await;
        let (code, resumed) = fixture.harness.json(&["session", "resume", &id]).await;
        assert_eq!(code, 0, "{resumed}");
        // The dispatch wrapper sees the fixed arguments ahead of the resume
        // arguments; the bridge sees the agent path ahead of them. A returned
        // resume and the reported reference are not log readiness: the wrapper
        // and the agent log their argv only when the worker runs them, which
        // can trail the resume command, so wait for the exact resume lines and
        // treat a missing or partially written log as pending.
        let resumed_line = match directory {
            "dispatch-wrapper" => format!("exec {} {agent} resume {reference}", cwd.display()),
            _ => format!("{agent} resume {reference}"),
        };
        let log = wait_until("the wrapper's resume argv entry", || async {
            let log = fs::read_to_string(wrapper_argv_log(&root, directory)).ok()?;
            log.lines().any(|line| *line == resumed_line).then_some(log)
        })
        .await;
        assert!(
            log.lines().any(|line| line == resumed_line),
            "the resume kept the fixed arguments and the reference: {log}"
        );
        let agent_directory = match directory {
            "dispatch-wrapper" => "dispatch-agent",
            _ => "bridge-agent",
        };
        let agent_resumed_line = format!("resume {reference}");
        let agent_log = wait_until("the agent's resume argv entry", || async {
            let log = fs::read_to_string(root.join(format!("agent-{agent_directory}.log"))).ok()?;
            log.lines()
                .any(|line| *line == agent_resumed_line)
                .then_some(log)
        })
        .await;
        assert!(
            agent_log.lines().any(|line| line == agent_resumed_line),
            "the wrapper dispatched the resume to the agent: {agent_log}"
        );
        fixture.stop(&id).await;
    }

    // Doctor probes the wrappers through the same package and reports the
    // profiles ready.
    let doctor = fixture.harness.run(&["doctor", "--json"]).await;
    let doctor: Value = serde_json::from_slice(&doctor.stdout).expect("doctor JSON");
    for name in [
        "agent_profile_wrapper-dispatch-ok",
        "agent_profile_wrapper-bridge-ok",
    ] {
        let check = doctor["ok"]["checks"]
            .as_array()
            .expect("doctor checks")
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("doctor reports {name}: {doctor}"));
        assert_eq!(check["status"], "ok", "{check}");
    }
    fixture.finish().await;
}

/// A wrapper profile whose program cannot answer the package's version probe
/// is refused at `session new` and at `session resume`, and `pohunek doctor`
/// fails its check, each naming the wrapper requirement in its recovery
/// guidance — the probe runs the program with only the package's probe
/// arguments, without the profile's fixed arguments — and never echoing the
/// raw output. A release outside the supported range stays a distinct
/// refusal without the wrapper guidance.
#[cfg(target_os = "linux")]
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one integration scenario asserts the refusals, the doctor details and the resume refusals of the wrapper probe contract"
)]
async fn a_wrapper_that_cannot_answer_the_version_probe_is_refused_with_wrapper_guidance() {
    let fixture = Fixture::start().await;
    let root = fixture.harness.env.root().to_path_buf();
    let cwd = fixture.harness.env.cwd().to_path_buf();
    let release = locked_release();

    let target = write_fake_codex(&root, "wrapper-target", &format!("codex-cli {release}"));
    let ahead = write_fake_codex(&root, "wrapper-ahead", "codex-cli 1.0.0");
    let banner = write_direnv_wrapper(&root, "banner", &target, false);
    let failing = write_bridge_wrapper(&root, "failing", &target, false);
    let ranging = write_direnv_wrapper(&root, "ranging", &ahead, true);
    fixture.write_profile_with_args(
        "wrapper-dispatch-banner",
        Some(&banner),
        &[
            "exec".to_owned(),
            cwd.display().to_string(),
            target.display().to_string(),
        ],
    );
    fixture.write_profile_with_args(
        "wrapper-bridge-nonzero",
        Some(&failing),
        &[target.display().to_string()],
    );
    fixture.write_profile_with_args(
        "wrapper-dispatch-range",
        Some(&ranging),
        &[
            "exec".to_owned(),
            cwd.display().to_string(),
            ahead.display().to_string(),
        ],
    );

    for (profile, verdict) in [
        ("wrapper-dispatch-banner", ProbeVerdict::Unparseable),
        ("wrapper-bridge-nonzero", ProbeVerdict::Failed),
        ("wrapper-dispatch-range", ProbeVerdict::OutOfRange("1.0.0")),
    ] {
        let entry = fixture.inventory_entry(profile).await;
        assert_eq!(entry["supported"], false, "{entry}");
        let (code, refused) = fixture.launch(profile, "refused").await;
        assert_eq!(code, 1, "{refused}");
        assert_eq!(
            refused["err"]["code"], "agent_runtime_unsupported",
            "{refused}"
        );
        let message = refused["err"]["msg"].as_str().expect("refusal message");
        for fragment in verdict.expected() {
            assert!(message.contains(&fragment), "{fragment:?}: {refused}");
        }
        let recover = refused["err"]["recover"].as_str().unwrap_or("");
        match verdict {
            ProbeVerdict::OutOfRange(_) => assert!(
                !recover.contains("wrapper"),
                "out of range keeps its own guidance: {refused}"
            ),
            ProbeVerdict::Unparseable => assert_wrapper_guidance(recover, "the launch refusal"),
            ProbeVerdict::Failed => {
                assert!(refused["err"]["recover"].is_null(), "{refused}");
                assert_wrapper_guidance(message, "the failed launch refusal");
            }
        }
        if matches!(verdict, ProbeVerdict::Unparseable) {
            assert!(!refused.to_string().contains("2.38.2"), "{refused}");
        }
    }

    let doctor = fixture.harness.run(&["doctor", "--json"]).await;
    let doctor: Value = serde_json::from_slice(&doctor.stdout).expect("doctor JSON");
    for (name, verdict) in [
        (
            "agent_profile_wrapper-dispatch-banner",
            ProbeVerdict::Unparseable,
        ),
        ("agent_profile_wrapper-bridge-nonzero", ProbeVerdict::Failed),
        (
            "agent_profile_wrapper-dispatch-range",
            ProbeVerdict::OutOfRange("1.0.0"),
        ),
    ] {
        let check = doctor["ok"]["checks"]
            .as_array()
            .expect("doctor checks")
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("doctor reports {name}: {doctor}"));
        assert_eq!(check["status"], "fail", "{check}");
        let detail = check["detail"].as_str().expect("doctor detail");
        for fragment in verdict.expected() {
            assert!(detail.contains(&fragment), "{fragment:?}: {check}");
        }
        match verdict {
            ProbeVerdict::OutOfRange(_) => assert!(
                !detail.contains("wrapper"),
                "out of range keeps its own detail: {check}"
            ),
            _ => assert_wrapper_guidance(detail, "the doctor detail"),
        }
        if matches!(verdict, ProbeVerdict::Unparseable) {
            assert!(!detail.contains("2.38.2"), "{check}");
        }
    }

    // A conforming wrapper launches and holds a native reference; once only
    // the wrapper's probe behavior changes, resume refuses with the same
    // guidance.
    let resume_agent = write_wrapper_agent(
        &root,
        "resume-agent",
        &format!("codex-cli {release}"),
        "wrapper-conversation-2",
    );
    let resume_wrapper = write_direnv_wrapper(&root, "resume-wrapper", &resume_agent, true);
    fixture.write_profile_with_args(
        "wrapper-dispatch-resume",
        Some(&resume_wrapper),
        &[
            "exec".to_owned(),
            cwd.display().to_string(),
            resume_agent.display().to_string(),
        ],
    );
    let (code, launched) = fixture.launch("wrapper-dispatch-resume", "resumable").await;
    assert_eq!(code, 0, "{launched}");
    let id = launched["ok"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let reported = wait_until("the hook-reported native reference", || async {
        let (code, inspected) = fixture.harness.json(&["session", "inspect", &id]).await;
        assert_eq!(code, 0, "{inspected}");
        inspected["ok"]["native_session_id"]
            .is_string()
            .then(|| inspected["ok"].clone())
    })
    .await;
    assert_eq!(
        reported["native_session_id"], "wrapper-conversation-2",
        "{reported}"
    );
    fixture.stop(&id).await;
    write_direnv_wrapper(&root, "resume-wrapper", &resume_agent, false);
    let (code, refused) = fixture.harness.json(&["session", "resume", &id]).await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(
        refused["err"]["code"], "agent_runtime_unsupported",
        "{refused}"
    );
    assert!(
        refused["err"]["msg"]
            .as_str()
            .expect("refusal message")
            .contains("does not match its runtime's declared format"),
        "{refused}"
    );
    assert_wrapper_guidance(
        refused["err"]["recover"].as_str().unwrap_or(""),
        "the resume refusal",
    );
    assert!(!refused.to_string().contains("2.38.2"), "{refused}");

    // The same for a forwarding bridge whose probe fails.
    let bridge_agent = write_wrapper_agent(
        &root,
        "resume-bridge-agent",
        &format!("codex-cli {release}"),
        "wrapper-conversation-3",
    );
    let bridge_wrapper = write_bridge_wrapper(&root, "resume-bridge", &bridge_agent, true);
    fixture.write_profile_with_args(
        "wrapper-bridge-resume",
        Some(&bridge_wrapper),
        &[bridge_agent.display().to_string()],
    );
    let (code, launched) = fixture.launch("wrapper-bridge-resume", "bridged").await;
    assert_eq!(code, 0, "{launched}");
    let id = launched["ok"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let reported = wait_until("the hook-reported native reference", || async {
        let (code, inspected) = fixture.harness.json(&["session", "inspect", &id]).await;
        assert_eq!(code, 0, "{inspected}");
        inspected["ok"]["native_session_id"]
            .is_string()
            .then(|| inspected["ok"].clone())
    })
    .await;
    assert_eq!(
        reported["native_session_id"], "wrapper-conversation-3",
        "{reported}"
    );
    fixture.stop(&id).await;
    write_bridge_wrapper(&root, "resume-bridge", &bridge_agent, false);
    let (code, refused) = fixture.harness.json(&["session", "resume", &id]).await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(
        refused["err"]["code"], "agent_runtime_unsupported",
        "{refused}"
    );
    let message = refused["err"]["msg"].as_str().expect("refusal message");
    assert!(
        message
            .starts_with("the selected agent executable did not pass this daemon's version check"),
        "{refused}"
    );
    assert!(refused["err"]["recover"].is_null(), "{refused}");
    assert_wrapper_guidance(message, "the failed resume refusal");
    fixture.finish().await;
}

/// The command that installs the official Codex package from an extracted
/// release archive, as a recovery hint names it.
const RELEASE_INSTALL_COMMAND: &str =
    "`pohunek plugin install runtime/packages/codex.tar.zst --catalog runtime/runtime-catalog.json`";

/// A daemon that ships no Codex and trusts a throwaway catalog key, on a host
/// whose `PATH` finds a fake `codex` of the locked release first.
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
        let fakes = pohunek_test_support::tempdir().expect("create the fake codex directory");
        let fake = write_fake_codex(
            fakes.path(),
            "bin",
            &format!("codex-cli {}", locked_release()),
        );
        let directory = fake.parent().expect("the fake codex directory");
        let mut search = std::ffi::OsString::from(directory);
        if let Some(inherited) = std::env::var_os("PATH") {
            search.push(":");
            search.push(inherited);
        }
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

/// The daemon serves `codex` from nothing but the official package: a host
/// without it refuses the bare runtime and its hook integration with the
/// release install as the hint, and after the catalog install the same
/// requests launch a session pinned to the package and report the package's
/// integration.
#[tokio::test]
async fn codex_launches_only_from_the_installed_official_package() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;

    let (code, refused) = host.launch("codex").await;
    assert_eq!(code, 1, "no runtime serves codex: {refused}");
    assert_eq!(
        refused["err"]["code"], "agent_profile_not_found",
        "{refused}"
    );
    let hint = recovery_hint(&refused);
    assert!(hint.contains(RELEASE_INSTALL_COMMAND), "{hint}");
    assert!(!hint.contains("shell|codex"), "{hint}");
    // Installing the hook integration of `codex` needs the package too.
    let (code, refused) = host
        .harness
        .json(&["integration", "install", "--agent", "codex"])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "runtime_not_installed", "{refused}");
    assert!(
        recovery_hint(&refused).contains(RELEASE_INSTALL_COMMAND),
        "{refused}"
    );

    let digest = install_package(&host.harness, &host.key).await;
    let (code, launched) = host.launch("codex").await;
    assert_eq!(code, 0, "{launched}");
    assert_eq!(launched["ok"]["agent_base"], "codex", "{launched}");
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
        .json(&["integration", "status", "--agent", "codex"])
        .await;
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["ok"]["agents"][0]["agent"], "codex", "{status}");

    host.stop().await;
}

/// A host profile over `codex` that pins no package names both steps while
/// nothing serves `codex`: the release install, then the profile migration.
/// Once the package is installed the profile must be migrated, and the
/// migrated profile launches from the package.
#[tokio::test]
async fn an_unpinned_codex_profile_names_the_install_and_the_migration() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    host.harness.profile("my-codex", "base = \"codex\"\n");

    let (code, refused) = host.launch("my-codex").await;
    assert_eq!(code, 1, "no runtime serves the base: {refused}");
    assert_eq!(refused["err"]["code"], "runtime_not_installed", "{refused}");
    let hint = recovery_hint(&refused);
    assert!(hint.contains(RELEASE_INSTALL_COMMAND), "{hint}");
    assert!(
        hint.contains("`pohunek plugin profile migrate my-codex`"),
        "{hint}"
    );

    install_package(&host.harness, &host.key).await;
    let (code, unpinned) = host.launch("my-codex").await;
    assert_eq!(code, 1, "{unpinned}");
    assert_eq!(unpinned["err"]["code"], "invalid_profile", "{unpinned}");
    assert!(
        unpinned["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains("pohunek plugin profile migrate my-codex")),
        "{unpinned}"
    );
    let (code, migrated) = host
        .harness
        .json(&["plugin", "profile", "migrate", "my-codex", "--yes"])
        .await;
    assert_eq!(code, 0, "{migrated}");
    let (code, launched) = host.launch("my-codex").await;
    assert_eq!(code, 0, "{launched}");
    assert_eq!(launched["ok"]["agent_base"], "codex", "{launched}");

    host.stop().await;
}

/// Hooks pohunek installed into `~/.codex` while a runtime served `codex` stay
/// visible and removable after nothing serves it any more (the built-in of an
/// earlier release is gone and no package is installed): status reports them
/// and uninstall removes them through the core-owned Codex handler, while a new
/// install still needs the package.
#[tokio::test]
async fn codex_hooks_stay_removable_without_the_official_package() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    let codex_home = host.harness.env.home().join(".codex");
    fs::create_dir_all(&codex_home).expect("create the Codex home");
    install_package(&host.harness, &host.key).await;
    let (code, installed) = host
        .harness
        .json(&["integration", "install", "--agent", "codex"])
        .await;
    assert_eq!(code, 0, "{installed}");
    assert!(codex_home.join("hooks.json").is_file(), "{installed}");
    let (code, removed) = host
        .harness
        .json(&["plugin", "uninstall", PACKAGE_ID, "--yes"])
        .await;
    assert_eq!(code, 0, "{removed}");

    let (code, status) = host
        .harness
        .json(&["integration", "status", "--agent", "codex"])
        .await;
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["ok"]["agents"][0]["agent"], "codex", "{status}");
    assert_eq!(status["ok"]["agents"][0]["state"], "current", "{status}");
    let (code, doctor) = host
        .harness
        .json(&["integration", "doctor", "--agent", "codex"])
        .await;
    assert_eq!(code, 0, "{doctor}");
    assert_eq!(doctor["ok"]["agents"][0]["agent"], "codex", "{doctor}");

    let (code, uninstalled) = host
        .harness
        .json(&["integration", "uninstall", "--agent", "codex"])
        .await;
    assert_eq!(code, 0, "{uninstalled}");
    assert_eq!(
        uninstalled["ok"]["uninstalled"][0]["state"], "removed",
        "{uninstalled}"
    );
    let hooks = fs::read_to_string(codex_home.join("hooks.json")).unwrap_or_default();
    assert!(!hooks.contains("pohunek-agent-state.sh"), "{hooks}");
    assert!(!codex_home.join("pohunek-agent-state.sh").exists());
    let (code, status) = host
        .harness
        .json(&["integration", "status", "--agent", "codex"])
        .await;
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status["ok"]["agents"][0]["state"], "not_installed",
        "{status}"
    );

    let (code, refused) = host
        .harness
        .json(&["integration", "install", "--agent", "codex"])
        .await;
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["err"]["code"], "runtime_not_installed", "{refused}");

    host.stop().await;
}

/// A host profile over `codex` whose agent keeps its config in `home`, pinned
/// to the installed package `digest` when there is one.
fn codex_profile(home: &Path, digest: Option<&PackageDigest>) -> String {
    let pin = digest.map_or_else(String::new, |digest| {
        format!(
            "package = \"{PACKAGE_ID}\"\ndigest = \"{}\"\n",
            digest.as_str()
        )
    });
    format!(
        "base = \"codex\"\n{pin}\n[env]\nCODEX_HOME = \"{}\"\n",
        path_str(home)
    )
}

/// Asserts whether each of `homes` holds pohunek's Codex state hook.
fn assert_hooks(homes: &[&Path], present: bool, context: &Value) {
    for home in homes {
        assert_eq!(
            home.join("pohunek-agent-state.sh").is_file(),
            present,
            "{}: {context}",
            home.display()
        );
    }
}

/// The hooks pohunek installed into the `CODEX_HOME` of host profiles over
/// `codex` stay removable once nothing serves `codex`, although such a profile
/// does not resolve: `--profile` and `--all-profiles` address each profile's
/// home through its own environment, so no profile home is left behind.
#[tokio::test]
async fn codex_profile_hooks_stay_removable_without_the_official_package() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    let root = host.harness.env.root().to_path_buf();
    let own_home = host.harness.env.home().join(".codex");
    let homes: Vec<(&str, PathBuf)> = vec![
        ("work", root.join("work-codex")),
        ("alt", root.join("alt-codex")),
    ];
    fs::create_dir_all(&own_home).expect("create the Codex home");
    for (_name, home) in &homes {
        fs::create_dir_all(home).expect("create a profile's Codex home");
    }
    let digest = install_package(&host.harness, &host.key).await;
    for (name, home) in &homes {
        host.harness
            .profile(name, &codex_profile(home, Some(&digest)));
    }
    let (code, installed) = host
        .harness
        .json(&[
            "integration",
            "install",
            "--agent",
            "codex",
            "--all-profiles",
        ])
        .await;
    assert_eq!(code, 0, "{installed}");
    let all_homes: Vec<&Path> = std::iter::once(own_home.as_path())
        .chain(homes.iter().map(|(_name, home)| home.as_path()))
        .collect();
    assert_hooks(&all_homes, true, &installed);
    // The profiles as a release with the built-in Codex wrote them: no pin.
    for (name, home) in &homes {
        host.harness.profile(name, &codex_profile(home, None));
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
            "codex",
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
            "codex",
            "--profile",
            "alt",
        ])
        .await;
    assert_eq!(code, 0, "{uninstalled}");
    assert_eq!(
        uninstalled["ok"]["uninstalled"][0]["state"], "removed",
        "{uninstalled}"
    );
    assert!(!homes[1].1.join("pohunek-agent-state.sh").exists());
    assert!(homes[0].1.join("pohunek-agent-state.sh").is_file());

    let (code, uninstalled) = host
        .harness
        .json(&[
            "integration",
            "uninstall",
            "--agent",
            "codex",
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

/// A host profile over `codex` that a launch would refuse as malformed (here a
/// misspelled `[env]` table) is refused by the cleanup path as well, instead
/// of being read with an empty environment that would point it at the
/// runtime's own home.
#[tokio::test]
async fn a_malformed_codex_profile_is_refused_without_the_official_package() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let host = CleanHost::start().await;
    let own_home = host.harness.env.home().join(".codex");
    let profile_home = host.harness.env.root().join("work-codex");
    fs::create_dir_all(&own_home).expect("create the Codex home");
    fs::create_dir_all(&profile_home).expect("create the profile's Codex home");
    install_package(&host.harness, &host.key).await;
    let (code, installed) = host
        .harness
        .json(&["integration", "install", "--agent", "codex"])
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
            "base = \"codex\"\n\n[evn]\nCODEX_HOME = \"{}\"\n",
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
                "codex",
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

#[test]
fn program_status_records_keep_root_records_in_order_without_repeats() {
    let raw = b"x\x1b]7501;?\x07\x1b]7501;state=working:app=codex\x1b\\\
        \x1b]7501;state=working:app=codex\x07\x1b]7501;state=blocked:app=codex:kind=permission\x1b\\\
        \x1b]7501;state=idle:id=child1:app=codex\x07\x1b]7501;state=bogus\x07\
        \x1b]7501;state=done:app=codex\x07\x1b]7501;state=clear\x07\x1b]7501;state=idle:app=co";
    assert_eq!(
        capture::program_status_records(raw),
        [
            "app=codex:state=working",
            "app=codex:state=blocked:kind=permission",
            "app=codex:state=done",
            "clear",
        ]
    );
}

#[test]
fn a_program_status_golden_is_written_one_record_per_line() {
    let directory = pohunek_test_support::tempdir().expect("tempdir");
    capture::write_program_status(
        directory.path(),
        "sample",
        b"\x1b]7501;state=idle:app=codex\x07",
    );
    let text = fs::read_to_string(directory.path().join("sample.program-status")).expect("golden");
    assert_eq!(text, "app=codex:state=idle\n");
}
