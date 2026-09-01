//! Validates the pinned Hermes CLI and refreshes bounded PTY evidence.

// Rust guideline compliant 2026-08-28

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::hermes_mock::{Mock, Scenario as MockScenario, MODEL_ID};
use crate::XtaskError;

const LOCK_PATH: &str = "compat/hermes/compatibility-lock.json";
const GOLDEN_ROOT: &str = "compat/hermes/goldens";
const GOLDEN_MANIFEST: &str = "manifest.json";
const LOCK_SCHEMA: u32 = 1;
const GOLDEN_SCHEMA: u32 = 1;
/// Digest of the reviewed lock, independent of its mutable version fields.
///
/// Updating the upstream pin requires reviewing the complete lock and changing
/// this digest in the same compatibility-infrastructure change.
const EXPECTED_LOCK_SHA256: &str =
    "a4ccdde37aa679859f03485a36b895610c9c2f44f46a44562a9e180bb97825b4";
/// The pinned source archive digest recorded by the M2 baseline review.
const EXPECTED_SOURCE_SHA256: &str =
    "1e9319c58a7f5e95808546af1091d58472be7437adc63fae0cbb53316e2711aa";
const EXPECTED_SOURCE_FORMAT: &str = "git archive --format=tar";
const EXPECTED_FRESH_ARGV: [&str; 1] = ["chat"];
const EXPECTED_RESUME_ARGV: [&str; 3] = ["chat", "--resume", "<reference>"];
const EXPECTED_DISPLAY_MODES: [&str; 2] = ["classic", "alternate_screen_tui"];
const EXPECTED_PLUGIN_MANIFEST_FIELDS: [&str; 5] = [
    "name",
    "version",
    "description",
    "provides_tools",
    "provides_hooks",
];
const EXPECTED_PLUGIN_OPTIONAL_MANIFEST_FIELDS: [&str; 2] = ["requires_env", "kind"];
const EXPECTED_PLUGIN_KIND: &str = "implicit_standalone";
const EXPECTED_PLUGIN_ENTRYPOINT_FILE: &str = "__init__.py";
const EXPECTED_PLUGIN_ENTRYPOINT: &str = "register(ctx)";
const EXPECTED_PLUGIN_TOOL_METHOD: &str = "ctx.register_tool";
const EXPECTED_PLUGIN_TOOL_ARGS: [&str; 4] = ["name", "toolset", "schema", "handler"];
const EXPECTED_PLUGIN_TOOLSET: &str = "pohunek";
const EXPECTED_PLUGIN_TOOL_SCHEMA_TYPE: &str = "object";
const EXPECTED_PLUGIN_TOOL_HANDLER_ARGS: &str = "args: dict, **kwargs";
const EXPECTED_PLUGIN_TOOL_RETURN: &str = "JSON string";
const EXPECTED_PLUGIN_HOOK_METHOD: &str = "ctx.register_hook";
const EXPECTED_PLUGIN_HOOK_ARGS: [&str; 2] = ["hook_name", "callback"];
const EXPECTED_PLUGIN_SKILL_METHOD: &str = "ctx.register_skill";
const EXPECTED_PLUGIN_SKILL_ARGS: [&str; 3] = ["name", "path", "description"];
const EXPECTED_PLUGIN_SKILL_PATH_TYPE: &str = "pathlib.Path";
const EXPECTED_PLUGIN_NAME: &str = "pohunek";
const EXPECTED_PLUGIN_KEY: &str = "operators/pohunek";
const EXPECTED_PLUGIN_SKILL: &str = "pohunek:pohunek";
const EXPECTED_PLUGIN_DISCOVERY_ROOT: &str = "plugins";
const EXPECTED_PLUGIN_DISCOVERY_CATEGORY: &str = "operators";
/// Hermes scans one optional grouping directory below its plugin root.
const EXPECTED_PLUGIN_DISCOVERY_DEPTH: u8 = 1;
/// The plugin lifecycle validates disabled, enabled, and disabled terminal states.
const PLUGIN_LIFECYCLE_CHECKS: usize = 5;
/// The harness proves both named-profile and absolute custom-home targeting.
const PLUGIN_TARGET_CHECKS: usize = 2;
const EXPECTED_NAMED_PROFILE: &str = "pohunek-compat";
const EXPECTED_NAMED_PROFILE_ARGS: [&str; 2] = ["--profile", EXPECTED_NAMED_PROFILE];
const EXPECTED_NAMED_PROFILE_HOME: &str = "profiles/pohunek-compat";
const EXPECTED_CUSTOM_HOME_ENV: &str = "HERMES_HOME";
const PLUGIN_FIXTURE_VERSION: &str = "0.0.0-compat";
const PLUGIN_FIXTURE_TOOL: &str = "pohunek_hosts";
const PLUGIN_FIXTURE_HOOK: &str = "pre_llm_call";
const PLUGIN_FIXTURE_SKILL_BODY: &str = "Model-free Pohunek compatibility skill.";
const PLUGIN_RUNTIME_CHECKS: usize = 1;
/// Four CLI states plus one real production-plugin registration check.
const PRODUCTION_PLUGIN_CHECKS: usize = 5;
/// The current Pohunek CLI protocol required by the generated plugin policy.
const EXPECTED_POHUNEK_PROTOCOL: u32 = 3;
/// A normal executable search remains well below this abuse-resistant bound.
const MAX_EXECUTABLE_PATH_ENTRIES: usize = 64;
const PLUGIN_RUNTIME_MARKERS: [&str; 3] = [
    "plugin api tool pohunek_hosts",
    "plugin api hook pre_llm_call",
    "plugin api skill pohunek:pohunek",
];
const PLUGIN_RUNTIME_SMOKE: &str = r#"
from pathlib import Path

from hermes_cli.plugins import PluginManager

manager = PluginManager()
manager.discover_and_load(force=True)
loaded = manager._plugins.get("operators/pohunek")
if loaded is None or not loaded.enabled or loaded.error is not None:
    raise RuntimeError("pinned Hermes did not load the compatibility plugin")
if loaded.manifest.kind != "standalone":
    raise RuntimeError("pinned Hermes resolved the wrong plugin kind")
if set(loaded.tools_registered) != {"pohunek_hosts"}:
    raise RuntimeError("pinned Hermes registered the wrong tool inventory")
if set(loaded.hooks_registered) != {"pre_llm_call"}:
    raise RuntimeError("pinned Hermes registered the wrong hook inventory")
skill = manager._plugin_skills.get("pohunek:pohunek")
if skill is None or not isinstance(skill.get("path"), Path) or not skill["path"].is_file():
    raise RuntimeError("pinned Hermes registered the wrong skill inventory")
print("plugin api tool pohunek_hosts")
print("plugin api hook pre_llm_call")
print("plugin api skill pohunek:pohunek")
"#;
const PRODUCTION_PLUGIN_RUNTIME_SMOKE: &str = r#"
from pathlib import Path

from hermes_cli.plugins import PluginManager

expected_tools = {
    "pohunek_hosts", "pohunek_sessions", "pohunek_session_get",
    "pohunek_session_screen", "pohunek_session_output", "pohunek_session_wait",
    "pohunek_session_diff",
}
expected_hooks = {
    "on_session_start", "pre_llm_call", "pre_approval_request",
    "post_approval_response", "post_llm_call", "on_session_end",
    "on_session_finalize",
}
manager = PluginManager()
manager.discover_and_load(force=True)
loaded = manager._plugins.get("operators/pohunek")
if loaded is None or not loaded.enabled or loaded.error is not None:
    raise RuntimeError("pinned Hermes did not load the production Pohunek plugin")
if set(loaded.tools_registered) != expected_tools:
    raise RuntimeError("production Pohunek read-only tool inventory drifted")
if set(loaded.hooks_registered) != expected_hooks:
    raise RuntimeError("production Pohunek hook inventory drifted")
skill = manager._plugin_skills.get("pohunek:pohunek")
if skill is None or not isinstance(skill.get("path"), Path) or not skill["path"].is_file():
    raise RuntimeError("production Pohunek skill inventory drifted")
print("production plugin registration healthy")
"#;
const PRODUCTION_PLUGIN_RUNTIME_MARKER: &str = "production plugin registration healthy";
const GOLDEN_STATES: [&str; 10] = [
    "prompt-ready",
    "short-input",
    "multiline-input",
    "working",
    "approval-blocked",
    "completion",
    "interruption",
    "exit",
    "resume",
    "alternate-screen-tui",
];
/// Command output is bounded below the control protocol's one-MiB line cap.
const MAX_COMMAND_OUTPUT_BYTES: usize = 256 * 1024;
/// A help/version probe should never need more than ten seconds.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Packaged binaries need extra time for the Hermes install lifecycle.
///
/// One install launches several independently bounded cold-start Hermes and
/// Python probes. Two minutes preserves a finite CI bound with scheduling
/// margin for all probes and also accommodates the slower MUSL release binary.
const INTEGRATION_ACTION_TIMEOUT: Duration = Duration::from_mins(2);
/// One PTY transcript remains reviewable and cannot consume unbounded memory.
const MAX_PTY_OUTPUT_BYTES: usize = 512 * 1024;
/// Timeout diagnostics expose only a short reviewable suffix of safe output.
const MAX_PTY_DIAGNOSTIC_BYTES: usize = 8 * 1024;
/// Retain enough terminal redraw lines to include the preceding turn failure.
const MAX_PTY_DIAGNOSTIC_LINES: usize = 128;
const PTY_DIAGNOSTIC_WITHHELD: &str = "PTY diagnostic withheld";
const RAW_TRANSCRIPT_SECTION: &str = "raw_transcript:";
const ASSISTANT_PANEL_SECTION: &str = "terminal_assistant_panel:";
/// The fixed terminal size matches the common baseline used by PTY fixtures.
const PTY_COLS: u16 = 100;
const PTY_ROWS: u16 = 32;
/// Preserve enough rendered history for every reviewable classic fixture.
///
/// The parser remains bounded independently from the raw byte cap. If a future
/// Hermes release pushes semantic evidence beyond this history, refresh fails
/// closed instead of accepting cursor-control-stripped output.
const PTY_SCROLLBACK_ROWS: usize = 1024;
const ASSISTANT_PANEL_TITLE: &str = "⚕ Hermes";
/// Polling at 20 ms bounds timeout overshoot without busy-waiting.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// The classic prompt normally appears within this bounded startup window.
const STARTUP_WAIT: Duration = Duration::from_secs(8);
/// Local mock turns include a real Hermes startup and must remain bounded.
const TURN_WAIT: Duration = Duration::from_secs(45);
/// Working and interruption evidence is sampled shortly after submission.
const WORKING_WAIT: Duration = Duration::from_secs(3);
/// Give input handling and terminal cleanup a short bounded grace period.
const INPUT_SETTLE_WAIT: Duration = Duration::from_secs(2);
const EXIT_GRACE: Duration = Duration::from_secs(8);
/// Reader completion is bounded even if an escaped descendant retained a pipe.
const READER_GRACE: Duration = Duration::from_secs(2);
/// Pinned Hermes rejects models below its 64K tool-calling context floor.
///
/// The static metadata also prevents local model-context discovery requests.
const MOCK_CONTEXT_LENGTH: u32 = 64_000;
const _: () = assert!(MOCK_CONTEXT_LENGTH >= 64_000);
const MOCK_PROVIDER_NAME: &str = "pohunek-compat";
const MOCK_PROVIDER_KEY: &str = "custom:pohunek-compat";
const PINNED_HERMES_VERSION: &str = "0.20.0";
const UPDATE_CACHE_FILE: &str = ".update_check";
const MODELS_DEV_CACHE_FILE: &str = "models_dev_cache.json";
/// Fresh non-empty metadata prevents pinned Hermes from refreshing models.dev.
const MODELS_DEV_CACHE: &str = r#"{"pohunek-offline":{"name":"Pohunek offline compatibility cache","env":[],"api":"","doc":"","models":{}}}
"#;
const MODEL_CATALOG_CACHE_DIRECTORY: &str = "cache";
const MODEL_CATALOG_CACHE_FILE: &str = "model_catalog.json";
/// Pinned Hermes accepts this schema-valid empty catalog without network refresh.
const MODEL_CATALOG_CACHE: &str = r#"{"version":1,"providers":{}}
"#;
const AUTH_STORE_FILE: &str = "auth.json";
/// Suppress every pinned Hermes Copilot credential source before startup.
///
/// In particular, `gh_cli` otherwise executes `gh auth token` and reads the
/// operator's ambient keyring even though the process environment is empty.
const AUTH_STORE: &str = r#"{"version":1,"providers":{},"suppressed_sources":{"copilot":["gh_cli","env:COPILOT_GITHUB_TOKEN","env:GH_TOKEN","env:GITHUB_TOKEN"]}}
"#;
/// Pinned Hermes selects any non-classic-PAT value before its CLI fallback.
/// This repository-owned value is not a usable credential, and its exchange is locally denied.
const MOCK_COPILOT_CREDENTIAL: &str = "pohunek-compat-local-mock";

const SHORT_PROMPT: &[u8] = b"Reply with exactly HERMES_COMPAT_OK.";
const SHORT_PROMPT_TEXT: &str = "Reply with exactly HERMES_COMPAT_OK.";
const MULTILINE_PROMPT: &[u8] =
    b"\x1b[200~Treat all three lines as one prompt.\nalpha\nbeta\nReply with exactly HERMES_MULTILINE_OK.\x1b[201~";
const MULTILINE_PREVIEW: &str =
    "Treat all three lines as one prompt.\nalpha\nbeta\nReply with exactly HERMES_MULTILINE_OK.";
const WORKING_PROMPT: &[u8] =
    b"Use the terminal tool to run `sleep 8`, then reply exactly HERMES_WORKING_DONE.";
const WORKING_PROMPT_TEXT: &str =
    "Use the terminal tool to run `sleep 8`, then reply exactly HERMES_WORKING_DONE.";
/// This is relative to the isolated work directory and remains inert until approval.
const APPROVAL_PROMPT: &[u8] = b"Use the terminal tool to run `rm -rf HERMES_COMPAT_APPROVAL_SENTINEL`, then reply exactly HERMES_APPROVAL_DONE.";
const APPROVAL_PROMPT_TEXT: &str =
    "Use the terminal tool to run `rm -rf HERMES_COMPAT_APPROVAL_SENTINEL`, then reply exactly HERMES_APPROVAL_DONE.";
const INTERRUPTION_PROMPT: &[u8] =
    b"Use the terminal tool to run `sleep 30`, then reply exactly HERMES_INTERRUPT_DONE.";
const INTERRUPTION_PROMPT_TEXT: &str =
    "Use the terminal tool to run `sleep 30`, then reply exactly HERMES_INTERRUPT_DONE.";
const SUBMIT: &[u8] = b"\r";
const EXIT_COMMAND: &[u8] = b"/exit\r";
const INTERRUPT: &[u8] = b"\x03";
const PROMPT_READY_MARKER: &str = "\u{276f}";
/// Pinned Hermes prints this separator immediately before each submitted user turn.
const USER_TURN_SEPARATOR: &str = "────────────────────────────────────────";
/// Pinned Hermes prefixes the first line of a submitted user turn with this glyph.
const USER_TURN_MARKER: &str = "\u{25cf}";
/// Prompt-toolkit may repaint its bounded status area between boundary rows.
/// One fixed-height PTY screen is the largest accepted boundary gap.
const MAX_USER_TURN_BOUNDARY_GAP_LINES: usize = 32;
/// Rich wrapping at the fixed PTY width needs at most a few continuation lines.
const MAX_USER_TURN_PREVIEW_LINES: usize = 8;
/// Four multiline fragments may each be separated by one fixed-height status repaint.
const MAX_USER_TURN_PREVIEW_SCAN_LINES: usize = 4 * 32;
/// This bounds normalization well above every repository-owned compatibility prompt.
const MAX_USER_TURN_PREVIEW_BYTES: usize = 1024;
const SHORT_RESPONSE: &str = "HERMES_COMPAT_OK";
const MULTILINE_RESPONSE: &str = "HERMES_MULTILINE_OK";
const INTERRUPT_MARKER: &str = "Interrupting agent";
const TUI_UNAVAILABLE_MARKERS: [&str; 4] = [
    "not found \u{2014} install Node.js to use the TUI",
    "Error: the TUI workspace is missing from this Hermes checkout",
    "npm install failed",
    "TUI build failed",
];

#[derive(Debug)]
pub(crate) struct CompatibilitySummary {
    pub(crate) release: String,
    pub(crate) tag: String,
    pub(crate) cli_checks: usize,
    pub(crate) plugin_checks: usize,
    pub(crate) golden_records: usize,
}

#[derive(Debug)]
pub(crate) struct RefreshSummary {
    pub(crate) captures: usize,
    pub(crate) unsupported: usize,
    pub(crate) manifest_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lock {
    schema_version: u32,
    release: String,
    tag: String,
    published: String,
    repository: String,
    commit: String,
    tree: String,
    source_archive: SourceArchive,
    runtime: RuntimeContract,
    cli_checks: Vec<CliCheck>,
    plugin_contract: PluginContract,
    golden_states: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceArchive {
    format: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeContract {
    program: String,
    fresh_argv: Vec<String>,
    resume_argv: Vec<String>,
    default_display_mode: String,
    local_display_modes: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CliCheck {
    id: String,
    args: Vec<String>,
    required_text: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginContract {
    cli_checks: Vec<CliCheck>,
    lifecycle: PluginLifecycle,
    targets: PluginTargets,
    api: PluginApi,
    integration_lifecycle: IntegrationLifecycle,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginLifecycle {
    plugin_name: String,
    plugin_key: String,
    list_args: Vec<String>,
    enable_args: Vec<String>,
    disable_args: Vec<String>,
    not_enabled_text: Vec<String>,
    enable_text: Vec<String>,
    disabled_text: Vec<String>,
    enabled_text: Vec<String>,
    disable_text: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginTargets {
    named_profile: String,
    named_profile_args: Vec<String>,
    named_profile_relative_home: String,
    custom_home_env: String,
    custom_home_must_be_absolute: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginApi {
    manifest_required_fields: Vec<String>,
    manifest_optional_fields: Vec<String>,
    manifest_kind: String,
    entrypoint_file: String,
    entrypoint: String,
    tool_method: String,
    tool_registration_args: Vec<String>,
    toolset: String,
    tool_schema_type: String,
    tool_handler_args: String,
    tool_return: String,
    hook_method: String,
    hook_registration_args: Vec<String>,
    skill_method: String,
    skill_registration_args: Vec<String>,
    skill_path_type: String,
    skill_name: String,
    skill_qualified_name: String,
    discovery_root: String,
    discovery_category: String,
    maximum_category_depth: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegrationLifecycle {
    target_kind: String,
    target_value: String,
    access_mode: String,
    allowed_hosts: Vec<String>,
    steps: Vec<IntegrationStep>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum IntegrationAction {
    Install,
    Status,
    Doctor,
    Uninstall,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum IntegrationState {
    Installed,
    Healthy,
    Absent,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegrationStep {
    action: IntegrationAction,
    expected_state: IntegrationState,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GoldenManifest {
    schema_version: u32,
    release: String,
    records: Vec<GoldenRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GoldenRecord {
    id: String,
    mode: String,
    status: GoldenStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum GoldenStatus {
    Pending,
    Captured,
    Unsupported,
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    command_timeout: Duration,
    integration_action_timeout: Duration,
    command_output_bytes: usize,
    pty_output_bytes: usize,
    startup_wait: Duration,
    turn_wait: Duration,
    working_wait: Duration,
    input_settle_wait: Duration,
    exit_grace: Duration,
}

impl Limits {
    fn production() -> Self {
        Self {
            command_timeout: COMMAND_TIMEOUT,
            integration_action_timeout: INTEGRATION_ACTION_TIMEOUT,
            command_output_bytes: MAX_COMMAND_OUTPUT_BYTES,
            pty_output_bytes: MAX_PTY_OUTPUT_BYTES,
            startup_wait: STARTUP_WAIT,
            turn_wait: TURN_WAIT,
            working_wait: WORKING_WAIT,
            input_settle_wait: INPUT_SETTLE_WAIT,
            exit_grace: EXIT_GRACE,
        }
    }
}

#[derive(Debug)]
struct Isolation {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
    hermes_home: PathBuf,
    work: PathBuf,
    tui_path: PathBuf,
}

impl Isolation {
    fn new(prefix: &str) -> Result<Self, XtaskError> {
        let temp = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .map_err(|error| fail(format!("failed to create isolated Hermes root: {error}")))?;
        Self::from_temp(temp)
    }

    fn new_in(prefix: &str, parent: &Path) -> Result<Self, XtaskError> {
        let temp = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(parent)
            .map_err(|error| fail(format!("failed to create isolated Hermes root: {error}")))?;
        Self::from_temp(temp)
    }

    fn from_temp(temp: TempDir) -> Result<Self, XtaskError> {
        let root = temp.path().to_path_buf();
        let home = root.join("home");
        let hermes_home = root.join("hermes-home");
        let work = root.join("work");
        let tui_path = root.join("tui-path");
        for path in [
            &home,
            &hermes_home,
            &work,
            &tui_path,
            &root.join("xdg-config"),
            &root.join("xdg-cache"),
            &root.join("xdg-data"),
            &root.join("python-user"),
            &root.join("python-cache"),
            &root.join("uv-cache"),
        ] {
            fs::create_dir_all(path).map_err(|error| {
                fail(format!(
                    "failed to create an isolated Hermes directory: {error}"
                ))
            })?;
        }
        fs::write(hermes_home.join(MODELS_DEV_CACHE_FILE), MODELS_DEV_CACHE).map_err(|error| {
            fail(format!(
                "failed to seed isolated Hermes models.dev cache: {error}"
            ))
        })?;
        let model_catalog_cache_directory = hermes_home.join(MODEL_CATALOG_CACHE_DIRECTORY);
        fs::create_dir_all(&model_catalog_cache_directory).map_err(|error| {
            fail(format!(
                "failed to create isolated Hermes model catalog cache directory: {error}"
            ))
        })?;
        let model_catalog_cache = model_catalog_cache_directory.join(MODEL_CATALOG_CACHE_FILE);
        fs::write(&model_catalog_cache, MODEL_CATALOG_CACHE).map_err(|error| {
            fail(format!(
                "failed to seed isolated Hermes model catalog cache: {error}"
            ))
        })?;
        fs::write(hermes_home.join(AUTH_STORE_FILE), AUTH_STORE).map_err(|error| {
            fail(format!(
                "failed to seed isolated Hermes credential-source policy: {error}"
            ))
        })?;
        write_update_cache(&hermes_home)?;
        Ok(Self {
            _temp: temp,
            root,
            home,
            hermes_home,
            work,
            tui_path,
        })
    }

    fn model_free_env(&self, proxy_url: &str) -> Vec<(OsString, OsString)> {
        let mut env = self.refresh_env(proxy_url);
        env.push((OsString::from("NO_COLOR"), OsString::from("1")));
        env.push((OsString::from("TERM"), OsString::from("dumb")));
        env
    }

    fn isolation_env(&self) -> Vec<(OsString, OsString)> {
        let mut env = vec![
            (OsString::from("HOME"), self.home.as_os_str().to_owned()),
            (
                OsString::from("HERMES_HOME"),
                self.hermes_home.as_os_str().to_owned(),
            ),
            (
                OsString::from("XDG_CONFIG_HOME"),
                self.root.join("xdg-config").into_os_string(),
            ),
            (
                OsString::from("XDG_CACHE_HOME"),
                self.root.join("xdg-cache").into_os_string(),
            ),
            (
                OsString::from("XDG_DATA_HOME"),
                self.root.join("xdg-data").into_os_string(),
            ),
            (
                OsString::from("PYTHONUSERBASE"),
                self.root.join("python-user").into_os_string(),
            ),
            (
                OsString::from("PYTHONPYCACHEPREFIX"),
                self.root.join("python-cache").into_os_string(),
            ),
            (
                OsString::from("UV_CACHE_DIR"),
                self.root.join("uv-cache").into_os_string(),
            ),
            (
                OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                OsString::from(format!(
                    "unix:path={}",
                    self.root.join("no-session-bus").display()
                )),
            ),
            (OsString::from("PYTHONNOUSERSITE"), OsString::from("1")),
            (
                OsString::from("PYTHONDONTWRITEBYTECODE"),
                OsString::from("1"),
            ),
            (OsString::from("LC_ALL"), OsString::from("C.UTF-8")),
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
        ];
        if let Some(path) = std::env::var_os("PATH") {
            env.push((OsString::from("PATH"), path));
        }
        env
    }

    fn refresh_env(&self, proxy_url: &str) -> Vec<(OsString, OsString)> {
        let mut env = self.isolation_env();
        // Hermes otherwise may install optional tooling from the network during startup.
        env.push((
            OsString::from("HERMES_DISABLE_LAZY_INSTALLS"),
            OsString::from("1"),
        ));
        // TUI startup has a separate Node bootstrap path in the pinned release.
        env.push((
            OsString::from("HERMES_SKIP_NODE_BOOTSTRAP"),
            OsString::from("1"),
        ));
        // Tirith has an independent downloader and does not honor the generic
        // Hermes lazy-install switch.
        env.push((OsString::from("TIRITH_ENABLED"), OsString::from("0")));
        // Pinned Hermes otherwise shells out to `gh auth token` during generic
        // provider discovery. This is deliberately not a usable credential.
        env.push((
            OsString::from("COPILOT_GITHUB_TOKEN"),
            OsString::from(MOCK_COPILOT_CREDENTIAL),
        ));
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            env.push((OsString::from(name), OsString::from(proxy_url)));
        }
        for name in ["NO_PROXY", "no_proxy"] {
            env.push((OsString::from(name), OsString::from("127.0.0.1,localhost")));
        }
        env
    }
}

/// Selects a temporary parent that cannot make the production CLI target unsafe.
///
/// `tempfile` creates the child isolation root with private permissions. The
/// parent nevertheless must be outside every Git workspace because Pohunek's
/// production resolver intentionally treats Git ancestry as an unsafe target.
fn production_integration_isolation() -> Result<Isolation, XtaskError> {
    let mut candidates = Vec::new();
    if let Some(runtime_directory) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(runtime_directory));
    }
    candidates.push(std::env::temp_dir());
    let parent = select_production_integration_temp_parent(candidates)?;
    Isolation::new_in("pohunek-production-plugin-", &parent)
}

fn select_production_integration_temp_parent(
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Result<PathBuf, XtaskError> {
    candidates
        .into_iter()
        .filter_map(|candidate| fs::canonicalize(candidate).ok())
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| metadata.is_dir())
                && path_is_outside_git_workspaces(candidate)
        })
        .ok_or_else(|| {
            fail(
                "no isolated temporary parent outside a Git workspace is available for Pohunek production compatibility",
            )
        })
}

fn path_is_outside_git_workspaces(path: &Path) -> bool {
    ancestors_are_outside_git_workspaces(path.ancestors())
}

fn ancestors_are_outside_git_workspaces<'path>(
    ancestors: impl IntoIterator<Item = &'path Path>,
) -> bool {
    ancestors.into_iter().all(|candidate| {
        let marker = candidate.join(".git");
        match fs::symlink_metadata(marker) {
            Ok(metadata) => !metadata.is_dir() && !metadata.is_file(),
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        }
    })
}

#[derive(Debug)]
struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PohunekEnvelope {
    cli_version: String,
    protocol: PohunekProtocol,
    ok: PohunekIntegrationResult,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PohunekProtocol {
    minimum: u32,
    maximum: u32,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "the CLI lifecycle envelope intentionally exposes independent state findings"
)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PohunekIntegrationResult {
    action: String,
    target_kind: String,
    target_label: String,
    installed: bool,
    enabled: bool,
    modified: bool,
    stale_stage: bool,
    stale_backup: bool,
    access_mode: Option<String>,
    allowed_host_count: Option<usize>,
    doctor: Option<PohunekDoctorReport>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PohunekDoctorReport {
    ok: bool,
    checks: Vec<PohunekDoctorCheck>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PohunekDoctorCheck {
    code: String,
    status: String,
    recovery_hint: String,
}

#[derive(Debug)]
struct PtyCapture {
    bytes: Vec<u8>,
    exit_code: Option<u32>,
    killed: bool,
}

#[derive(Clone, Debug)]
enum Action {
    WaitFor(Evidence, Duration),
    Pause(Duration),
    Write(&'static [u8]),
}

#[derive(Clone, Debug)]
enum Evidence {
    PromptReady,
    AssistantPanel(&'static str),
    Working(&'static str),
    Approval,
    Interrupted,
    Resumed(String),
    AlternateScreen,
}

#[cfg(test)]
struct TerminalCapture {
    parser: vt100::Parser,
    observed_bytes: usize,
}

#[cfg(test)]
impl TerminalCapture {
    fn new() -> Self {
        Self {
            parser: vt100::Parser::new(PTY_ROWS, PTY_COLS, PTY_SCROLLBACK_ROWS),
            observed_bytes: 0,
        }
    }

    fn feed_snapshot(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() < self.observed_bytes {
            self.parser = vt100::Parser::new(PTY_ROWS, PTY_COLS, PTY_SCROLLBACK_ROWS);
            self.observed_bytes = 0;
        }
        if bytes.len() == self.observed_bytes {
            return false;
        }
        self.parser.process(&bytes[self.observed_bytes..]);
        self.observed_bytes = bytes.len();
        true
    }

    fn transcript(&mut self) -> String {
        terminal_history(&mut self.parser)
    }

    fn assistant_panel_count(&mut self, expected: &str) -> usize {
        assistant_panel_count(&self.transcript(), expected)
    }
}

struct AssistantPanelObserver {
    parser: vt100::Parser,
    expected: &'static str,
    observed_bytes: usize,
    visible_events: [usize; 3],
    occurrence_count: usize,
    captured_panels: Vec<String>,
    progress: PanelProgress,
    invalid: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanelProgress {
    Start,
    HeaderSeen,
    ContentSeen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanelRenderEvent {
    Top,
    Content,
    Bottom,
}

impl AssistantPanelObserver {
    fn new(expected: &'static str) -> Self {
        Self {
            parser: vt100::Parser::new(PTY_ROWS, PTY_COLS, PTY_SCROLLBACK_ROWS),
            expected,
            observed_bytes: 0,
            visible_events: [0; 3],
            occurrence_count: 0,
            captured_panels: Vec::new(),
            progress: PanelProgress::Start,
            invalid: false,
        }
    }

    fn feed_snapshot(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() < self.observed_bytes {
            *self = Self::new(self.expected);
        }
        if bytes.len() == self.observed_bytes {
            return false;
        }

        let appended = &bytes[self.observed_bytes..];
        let mut start = 0;
        while start < appended.len() {
            let checkpoint = next_panel_checkpoint(appended, start);
            let end = checkpoint.map_or(appended.len(), |checkpoint| checkpoint.end);
            let chunk = &appended[start..end];
            let candidate = contains_bytes(chunk, self.expected.as_bytes())
                || contains_bytes(chunk, ASSISTANT_PANEL_TITLE.as_bytes());
            if candidate {
                // Refresh rising-edge baselines before a possible new panel
                // segment, including after prior evidence left scrollback.
                self.observe();
            }
            self.parser.process(chunk);
            if checkpoint.is_some() && (candidate || self.progress != PanelProgress::Start) {
                self.observe();
            }
            start = end;
        }
        self.observed_bytes = bytes.len();
        self.observe();
        true
    }

    fn observe(&mut self) {
        let transcript = terminal_history(&mut self.parser);
        let (visible_events, events) =
            panel_render_events(&transcript, self.expected, self.visible_events);
        self.visible_events = visible_events;
        for event in events {
            self.record(event);
        }
    }

    fn record(&mut self, event: PanelRenderEvent) {
        self.progress = match (self.progress, event) {
            (PanelProgress::Start, PanelRenderEvent::Top) => PanelProgress::HeaderSeen,
            (PanelProgress::HeaderSeen, PanelRenderEvent::Content) => PanelProgress::ContentSeen,
            (PanelProgress::ContentSeen, PanelRenderEvent::Bottom) => {
                self.occurrence_count = self.occurrence_count.saturating_add(1).min(2);
                if self.captured_panels.len() < 2 {
                    self.captured_panels
                        .push(normalized_assistant_panel(self.expected));
                }
                PanelProgress::Start
            }
            // Pinned prompt-toolkit repaints the completed footer while the
            // status area settles. Without a new header or content this is not
            // a second assistant response.
            (PanelProgress::Start, PanelRenderEvent::Bottom) => PanelProgress::Start,
            _ => {
                self.invalid = true;
                self.progress
            }
        };
    }

    #[cfg(test)]
    fn occurrence_count(&self) -> usize {
        self.occurrence_count
    }

    fn one_panel(&self) -> Option<&str> {
        (self.occurrence_count == 1
            && self.captured_panels.len() == 1
            && self.progress == PanelProgress::Start
            && !self.invalid)
            .then(|| self.captured_panels[0].as_str())
    }
}

fn panel_render_events(
    transcript: &str,
    expected: &str,
    previous_counts: [usize; 3],
) -> ([usize; 3], Vec<PanelRenderEvent>) {
    let top = assistant_panel_top();
    let bottom = assistant_panel_bottom();
    let mut positions = [Vec::new(), Vec::new(), Vec::new()];
    for (index, line) in transcript.lines().enumerate() {
        let line = line.trim_end_matches(' ');
        if line == top {
            positions[0].push(index);
        }
        if line == expected {
            positions[1].push(index);
        }
        if line == bottom {
            positions[2].push(index);
        }
    }
    let counts = positions.each_ref().map(Vec::len);
    let mut events = Vec::new();
    for (kind, event) in [
        PanelRenderEvent::Top,
        PanelRenderEvent::Content,
        PanelRenderEvent::Bottom,
    ]
    .into_iter()
    .enumerate()
    {
        let added = counts[kind].saturating_sub(previous_counts[kind]);
        events.extend(
            positions[kind]
                .iter()
                .rev()
                .take(added)
                .map(|position| (*position, event)),
        );
    }
    events.sort_by_key(|(position, _event)| *position);
    (
        counts,
        events.into_iter().map(|(_position, event)| event).collect(),
    )
}

#[derive(Clone, Copy)]
struct PanelCheckpoint {
    end: usize,
}

fn next_panel_checkpoint(bytes: &[u8], start: usize) -> Option<PanelCheckpoint> {
    let mut index = start;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            return Some(PanelCheckpoint { end: index + 1 });
        }
        if bytes[index] == 0x1b && bytes.get(index + 1).copied() == Some(b'[') {
            let end = skip_csi(bytes, index + 2);
            return Some(PanelCheckpoint { end });
        }
        if bytes[index] == 0x9b {
            let end = skip_csi(bytes, index + 1);
            return Some(PanelCheckpoint { end });
        }
        index += 1;
    }
    None
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[derive(Debug)]
struct Scenario {
    id: &'static str,
    mode: &'static str,
    args: Vec<String>,
    actions: Vec<Action>,
}

/// Runs the model-free compatibility checks.
pub(crate) fn compatibility(
    repo: &Path,
    hermes_bin: &Path,
    pohunek_bin: &Path,
) -> Result<CompatibilitySummary, XtaskError> {
    compatibility_with(repo, hermes_bin, pohunek_bin, Limits::production())
}

/// Refreshes sanitized PTY goldens against the repository-owned local mock.
pub(crate) fn refresh_goldens(
    repo: &Path,
    hermes_bin: &Path,
) -> Result<RefreshSummary, XtaskError> {
    refresh_with(repo, hermes_bin, Limits::production())
}

fn compatibility_with(
    repo: &Path,
    hermes_bin: &Path,
    pohunek_bin: &Path,
    limits: Limits,
) -> Result<CompatibilitySummary, XtaskError> {
    let pohunek_bin = require_safe_pohunek_binary(pohunek_bin)?;
    let hermes_bin = resolve_compatibility_hermes_binary(hermes_bin)?;
    let lock = check_cli(repo, &hermes_bin, limits)?;
    let manifest = load_golden_manifest(repo)?;
    validate_golden_manifest(repo, &lock, &manifest, limits.pty_output_bytes, false)?;
    check_pohunek_integration(
        &hermes_bin,
        &pohunek_bin,
        &lock.plugin_contract.integration_lifecycle,
        limits,
    )?;

    Ok(compatibility_summary(lock, &manifest))
}

fn compatibility_summary(lock: Lock, manifest: &GoldenManifest) -> CompatibilitySummary {
    CompatibilitySummary {
        release: lock.release,
        tag: lock.tag,
        cli_checks: lock.cli_checks.len(),
        plugin_checks: lock.plugin_contract.cli_checks.len()
            + PLUGIN_LIFECYCLE_CHECKS
            + PLUGIN_TARGET_CHECKS
            + PLUGIN_RUNTIME_CHECKS
            + PRODUCTION_PLUGIN_CHECKS,
        golden_records: manifest.records.len(),
    }
}

fn check_cli(repo: &Path, hermes_bin: &Path, limits: Limits) -> Result<Lock, XtaskError> {
    let lock = load_lock(repo)?;
    validate_lock(&lock)?;
    let isolation = Isolation::new("pohunek-hermes-compat-")?;
    let mock = Mock::start().map_err(|error| fail(error.to_string()))?;
    mock.begin(MockScenario::no_request("cli-preflight"))
        .map_err(|error| fail(error.to_string()))?;
    let env = isolation.model_free_env(&mock.proxy_url());

    for check in &lock.cli_checks {
        run_cli_check(hermes_bin, check, &isolation, &env, limits)?;
    }
    check_plugin_contract(hermes_bin, &lock.plugin_contract, &isolation, &env, limits)?;
    mock.finish().map_err(|error| fail(error.to_string()))?;

    Ok(lock)
}

fn check_plugin_contract(
    hermes_bin: &Path,
    plugin: &PluginContract,
    isolation: &Isolation,
    env: &[(OsString, OsString)],
    limits: Limits,
) -> Result<(), XtaskError> {
    let custom_home_key = OsString::from(&plugin.targets.custom_home_env);
    let custom_home = env
        .iter()
        .find(|(name, _value)| name == &custom_home_key)
        .map(|(_name, value)| Path::new(value));
    if !plugin.targets.custom_home_must_be_absolute
        || custom_home != Some(isolation.hermes_home.as_path())
        || !isolation.hermes_home.is_absolute()
    {
        return Err(fail(
            "Hermes plugin custom-home compatibility target is not absolute",
        ));
    }
    materialize_plugin_fixture(&isolation.hermes_home, plugin)?;
    for check in &plugin.cli_checks {
        run_cli_check(hermes_bin, check, isolation, env, limits)?;
    }

    run_plugin_state_check(
        hermes_bin,
        "plugins-list-not-enabled-before-enable",
        &plugin.lifecycle.list_args,
        &plugin.lifecycle.not_enabled_text,
        isolation,
        env,
        limits,
    )?;
    run_plugin_state_check(
        hermes_bin,
        "plugins-enable",
        &plugin.lifecycle.enable_args,
        &plugin.lifecycle.enable_text,
        isolation,
        env,
        limits,
    )?;
    run_plugin_state_check(
        hermes_bin,
        "plugins-list-enabled",
        &plugin.lifecycle.list_args,
        &plugin.lifecycle.enabled_text,
        isolation,
        env,
        limits,
    )?;
    run_plugin_runtime_check(hermes_bin, isolation, env, limits)?;
    run_plugin_state_check(
        hermes_bin,
        "plugins-disable",
        &plugin.lifecycle.disable_args,
        &plugin.lifecycle.disable_text,
        isolation,
        env,
        limits,
    )?;
    run_plugin_state_check(
        hermes_bin,
        "plugins-list-disabled-after-disable",
        &plugin.lifecycle.list_args,
        &plugin.lifecycle.disabled_text,
        isolation,
        env,
        limits,
    )?;

    let named_home = isolation
        .hermes_home
        .join(&plugin.targets.named_profile_relative_home);
    materialize_plugin_fixture(&named_home, plugin)?;
    let mut named_list_args = plugin.targets.named_profile_args.clone();
    named_list_args.extend(plugin.lifecycle.list_args.iter().cloned());
    run_plugin_state_check(
        hermes_bin,
        "plugins-list-named-profile",
        &named_list_args,
        &plugin.lifecycle.not_enabled_text,
        isolation,
        env,
        limits,
    )
}

fn run_plugin_runtime_check(
    hermes_bin: &Path,
    isolation: &Isolation,
    env: &[(OsString, OsString)],
    limits: Limits,
) -> Result<(), XtaskError> {
    let python = sibling_python(hermes_bin)?;
    let args = vec!["-c".to_owned(), PLUGIN_RUNTIME_SMOKE.to_owned()];
    let required_text: Vec<_> = PLUGIN_RUNTIME_MARKERS
        .iter()
        .map(|marker| (*marker).to_owned())
        .collect();
    run_plugin_state_check(
        &python,
        "plugins-runtime-api",
        &args,
        &required_text,
        isolation,
        env,
        limits,
    )
}

fn sibling_python(hermes_bin: &Path) -> Result<PathBuf, XtaskError> {
    let directory = hermes_bin.parent().ok_or_else(|| {
        fail("Hermes executable has no installation directory for its Python runtime")
    })?;
    [directory.join("python"), directory.join("python3")]
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| fail("Hermes installation has no sibling Python runtime"))
}

fn run_cli_check(
    hermes_bin: &Path,
    check: &CliCheck,
    isolation: &Isolation,
    env: &[(OsString, OsString)],
    limits: Limits,
) -> Result<(), XtaskError> {
    run_plugin_state_check(
        hermes_bin,
        &check.id,
        &check.args,
        &check.required_text,
        isolation,
        env,
        limits,
    )
}

fn run_plugin_state_check(
    hermes_bin: &Path,
    id: &str,
    args: &[String],
    required_text: &[String],
    isolation: &Isolation,
    env: &[(OsString, OsString)],
    limits: Limits,
) -> Result<(), XtaskError> {
    let output = run_process(
        hermes_bin,
        args,
        &isolation.work,
        env,
        limits.command_timeout,
        limits.command_output_bytes,
    )?;
    if !output.status.success() {
        return Err(fail(format!(
            "Hermes CLI check `{id}` exited unsuccessfully"
        )));
    }
    let text = combined_text(&output);
    for required in required_text {
        if !text.contains(required) {
            return Err(fail(format!(
                "Hermes CLI check `{id}` is missing required text `{required}`"
            )));
        }
    }
    Ok(())
}

fn materialize_plugin_fixture(
    hermes_home: &Path,
    plugin: &PluginContract,
) -> Result<(), XtaskError> {
    let root = hermes_home.join(&plugin.api.discovery_root);
    let directory = root
        .join(&plugin.api.discovery_category)
        .join(&plugin.lifecycle.plugin_name);
    fs::create_dir_all(&directory)
        .map_err(|error| fail(format!("failed to create isolated plugin fixture: {error}")))?;
    let manifest = format!(
        "name: {}\nversion: {}\ndescription: Model-free Pohunek compatibility fixture\nprovides_tools:\n  - {}\nprovides_hooks:\n  - {}\n",
        plugin.lifecycle.plugin_name,
        PLUGIN_FIXTURE_VERSION,
        PLUGIN_FIXTURE_TOOL,
        PLUGIN_FIXTURE_HOOK,
    );
    fs::write(directory.join("plugin.yaml"), manifest)
        .map_err(|error| fail(format!("failed to write isolated plugin manifest: {error}")))?;
    let skill_directory = directory.join("skills").join(&plugin.api.skill_name);
    fs::create_dir_all(&skill_directory)
        .map_err(|error| fail(format!("failed to create isolated plugin skill: {error}")))?;
    fs::write(
        skill_directory.join("SKILL.md"),
        format!("# Pohunek\n\n{PLUGIN_FIXTURE_SKILL_BODY}\n"),
    )
    .map_err(|error| fail(format!("failed to write isolated plugin skill: {error}")))?;
    let entrypoint = format!(
        "import json\nfrom pathlib import Path\n\n_ROOT = Path(__file__).resolve().parent\n\ndef _tool(args: dict, **kwargs) -> str:\n    return json.dumps({{\"ok\": True}})\n\ndef _hook(*args, **kwargs):\n    return None\n\ndef register(ctx):\n    ctx.register_tool(\"{}\", \"{}\", {{\"name\": \"{}\", \"description\": \"List Pohunek hosts\", \"parameters\": {{\"type\": \"object\", \"properties\": {{}}, \"additionalProperties\": False}}}}, _tool, description=\"List Pohunek hosts\")\n    ctx.register_hook(\"{}\", _hook)\n    ctx.register_skill(\"{}\", _ROOT / \"skills\" / \"{}\" / \"SKILL.md\", description=\"Use Pohunek safely\")\n",
        PLUGIN_FIXTURE_TOOL,
        plugin.api.toolset,
        PLUGIN_FIXTURE_TOOL,
        PLUGIN_FIXTURE_HOOK,
        plugin.api.skill_name,
        plugin.api.skill_name,
    );
    fs::write(directory.join(&plugin.api.entrypoint_file), entrypoint).map_err(|error| {
        fail(format!(
            "failed to write isolated plugin entrypoint: {error}"
        ))
    })
}

fn refresh_with(
    repo: &Path,
    hermes_bin: &Path,
    limits: Limits,
) -> Result<RefreshSummary, XtaskError> {
    require_explicit_binary(hermes_bin)?;
    let lock = check_cli(repo, hermes_bin, limits)?;

    let isolation = Isolation::new("pohunek-hermes-goldens-")?;
    let mock = Mock::start().map_err(|error| fail(error.to_string()))?;
    let mock_base_url = mock.base_url();
    write_refresh_config(&isolation, &mock_base_url)?;
    let diagnostic_paths = sensitive_paths(repo, hermes_bin, &isolation, &mock_base_url);
    let scenarios = classic_scenarios(limits);
    let mut captures = Vec::new();
    let mut resume_reference = None;

    for scenario in &scenarios {
        let capture = run_mocked_pty(
            hermes_bin,
            scenario,
            &isolation,
            &mock,
            &diagnostic_paths,
            limits,
        )?;
        validate_classic_capture(scenario.id, &capture)
            .map_err(|error| with_pty_diagnostic(&error, &capture.bytes, &diagnostic_paths))?;
        if scenario.id == "completion" {
            resume_reference = extract_session_reference(&capture.bytes);
        }
        captures.push((scenario.id, scenario.mode, GoldenStatus::Captured, capture));
    }

    let reference = resume_reference.ok_or_else(|| {
        fail("Hermes completion capture did not expose a native session reference")
    })?;
    let resume = Scenario {
        id: "resume",
        mode: "classic",
        args: vec!["chat".to_owned(), "--resume".to_owned(), reference.clone()],
        actions: vec![
            Action::WaitFor(Evidence::Resumed(reference), limits.startup_wait),
            Action::Write(EXIT_COMMAND),
        ],
    };
    let resume_capture = run_mocked_pty(
        hermes_bin,
        &resume,
        &isolation,
        &mock,
        &diagnostic_paths,
        limits,
    )?;
    validate_resume_capture(&resume_capture, &resume.args[2])?;
    captures.push((
        resume.id,
        resume.mode,
        GoldenStatus::Captured,
        resume_capture,
    ));

    let tui = tui_scenario(limits);
    let tui_capture = run_mocked_pty(
        hermes_bin,
        &tui,
        &isolation,
        &mock,
        &diagnostic_paths,
        limits,
    );
    match tui_capture {
        Ok(capture) if has_alternate_screen(&capture.bytes) => {
            if !capture.killed
                && capture
                    .exit_code
                    .is_some_and(|code| code != 0 && code != 130)
            {
                return Err(fail(
                    "Hermes alternate-screen TUI crashed after entering alternate-screen mode",
                ));
            }
            captures.push((tui.id, tui.mode, GoldenStatus::Captured, capture));
        }
        Ok(capture) if recognized_tui_unavailable(&capture.bytes) => {
            captures.push((tui.id, tui.mode, GoldenStatus::Unsupported, capture));
        }
        Ok(_capture) => {
            return Err(fail(
                "Hermes alternate-screen TUI produced neither alternate-screen evidence nor a recognized local-unavailable diagnosis",
            ));
        }
        Err(error) => return Err(error),
    }

    write_goldens(
        repo,
        &lock,
        hermes_bin,
        &isolation,
        &mock_base_url,
        captures,
        limits,
    )
}

fn write_refresh_config(isolation: &Isolation, base_url: &str) -> Result<(), XtaskError> {
    if !base_url.starts_with("http://127.0.0.1:") || !base_url.ends_with("/v1") {
        return Err(fail(
            "Hermes compatibility mock endpoint is not IPv4 loopback",
        ));
    }
    // Hermes auto-generates a title after the first successful turn. Keep that
    // background model request disabled because the mock admits exactly one
    // primary request per deterministic capture scenario.
    let config = format!(
        "model:\n  default: {MODEL_ID}\n  provider: {MOCK_PROVIDER_KEY}\n  api_mode: chat_completions\n  context_length: {MOCK_CONTEXT_LENGTH}\nmodel_catalog:\n  enabled: false\nproviders:\n  {MOCK_PROVIDER_NAME}:\n    name: {MOCK_PROVIDER_NAME}\n    api: {base_url}\n    transport: chat_completions\n    default_model: {MODEL_ID}\n    discover_models: false\n    models:\n      {MODEL_ID}:\n        context_length: {MOCK_CONTEXT_LENGTH}\nfallback_providers: []\ntoolsets:\n  - terminal\napprovals:\n  mode: manual\n  timeout: 300\nsecurity:\n  tirith_enabled: false\n  allow_lazy_installs: false\nauxiliary:\n  title_generation:\n    enabled: false\ntelemetry:\n  shared_metrics:\n    enabled: false\n",
    );
    let config_path = isolation.hermes_home.join("config.yaml");
    fs::write(&config_path, config)
        .map_err(|error| fail(format!("failed to write isolated Hermes config: {error}")))?;

    Ok(())
}

fn write_update_cache(hermes_home: &Path) -> Result<(), XtaskError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| fail(format!("system clock predates the Unix epoch: {error}")))?
        .as_secs();
    let cache =
        format!(r#"{{"ts":{timestamp},"behind":null,"rev":null,"ver":"{PINNED_HERMES_VERSION}"}}"#);
    fs::write(hermes_home.join(UPDATE_CACHE_FILE), cache).map_err(|error| {
        fail(format!(
            "failed to write isolated Hermes update cache: {error}"
        ))
    })
}

fn mock_scenario(scenario: &Scenario) -> MockScenario {
    let scenario = match scenario.id {
        "prompt-ready" | "exit" | "resume" | "alternate-screen-tui" => {
            MockScenario::no_request(scenario.id)
        }
        "short-input" | "completion" => {
            MockScenario::text_with_local_discovery(scenario.id, SHORT_PROMPT_TEXT, SHORT_RESPONSE)
        }
        "multiline-input" => MockScenario::text_with_local_discovery(
            scenario.id,
            MULTILINE_PREVIEW,
            MULTILINE_RESPONSE,
        ),
        "working" => {
            MockScenario::terminal_with_local_discovery(scenario.id, WORKING_PROMPT_TEXT, "sleep 8")
        }
        "approval-blocked" => MockScenario::terminal_with_local_discovery(
            scenario.id,
            APPROVAL_PROMPT_TEXT,
            "rm -rf HERMES_COMPAT_APPROVAL_SENTINEL",
        ),
        "interruption" => MockScenario::terminal_with_local_discovery(
            scenario.id,
            INTERRUPTION_PROMPT_TEXT,
            "sleep 30",
        ),
        _ => unreachable!("the Hermes scenario inventory is closed"),
    };
    scenario.with_copilot_probe_denials()
}

fn run_mocked_pty(
    program: &Path,
    scenario: &Scenario,
    isolation: &Isolation,
    mock: &Mock,
    diagnostic_paths: &[(String, &'static str)],
    limits: Limits,
) -> Result<PtyCapture, XtaskError> {
    mock.begin(mock_scenario(scenario))
        .map_err(|error| fail(error.to_string()))?;
    let capture = run_pty(
        program,
        scenario,
        isolation,
        &mock.proxy_url(),
        diagnostic_paths,
        limits,
    );
    let verification = mock.finish().map_err(|error| fail(error.to_string()));
    finish_mocked_pty(capture, verification)
}

fn finish_mocked_pty(
    capture: Result<PtyCapture, XtaskError>,
    verification: Result<(), XtaskError>,
) -> Result<PtyCapture, XtaskError> {
    verification?;
    let capture = capture?;
    Ok(capture)
}

fn load_lock(repo: &Path) -> Result<Lock, XtaskError> {
    let path = repo.join(LOCK_PATH);
    let bytes = fs::read(&path)
        .map_err(|error| fail(format!("failed to read Hermes compatibility lock: {error}")))?;
    let digest = sha256(&bytes);
    if digest != EXPECTED_LOCK_SHA256 {
        return Err(fail(format!(
            "Hermes compatibility lock digest mismatch: expected {EXPECTED_LOCK_SHA256}, got {digest}"
        )));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| fail(format!("invalid Hermes compatibility lock: {error}")))
}

fn validate_lock(lock: &Lock) -> Result<(), XtaskError> {
    if lock.schema_version != LOCK_SCHEMA {
        return Err(fail("unsupported Hermes compatibility lock schema"));
    }
    if lock.source_archive.format != EXPECTED_SOURCE_FORMAT
        || lock.source_archive.sha256 != EXPECTED_SOURCE_SHA256
    {
        return Err(fail("Hermes source archive checksum contract changed"));
    }
    for (name, value) in [("commit", &lock.commit), ("tree", &lock.tree)] {
        if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(fail(format!(
                "Hermes lock {name} is not a full Git object id"
            )));
        }
    }
    if lock.release.is_empty()
        || lock.tag.is_empty()
        || lock.published.is_empty()
        || lock.repository != "https://github.com/NousResearch/hermes-agent"
    {
        return Err(fail("Hermes release metadata is incomplete"));
    }
    if lock.runtime.program != "hermes"
        || string_slice(&lock.runtime.fresh_argv) != EXPECTED_FRESH_ARGV
        || string_slice(&lock.runtime.resume_argv) != EXPECTED_RESUME_ARGV
        || lock.runtime.default_display_mode != "classic"
        || string_slice(&lock.runtime.local_display_modes) != EXPECTED_DISPLAY_MODES
    {
        return Err(fail("Hermes runtime command contract changed"));
    }
    if string_slice(&lock.golden_states) != GOLDEN_STATES {
        return Err(fail("Hermes golden-state inventory changed"));
    }
    if lock.cli_checks.len() != 8
        || lock.cli_checks.iter().any(|check| {
            check.id.is_empty() || check.args.is_empty() || check.required_text.is_empty()
        })
    {
        return Err(fail("Hermes CLI check inventory is incomplete"));
    }
    validate_plugin_contract(&lock.plugin_contract)?;
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "the pinned plugin contract is validated in one auditable sequence"
)]
fn validate_plugin_contract(plugin: &PluginContract) -> Result<(), XtaskError> {
    if plugin.cli_checks.len() != 4
        || plugin.cli_checks.iter().any(|check| {
            check.id.is_empty() || check.args.is_empty() || check.required_text.is_empty()
        })
    {
        return Err(fail("Hermes plugin CLI check inventory is incomplete"));
    }
    let cli_ids: Vec<_> = plugin
        .cli_checks
        .iter()
        .map(|check| check.id.as_str())
        .collect();
    let cli_args: Vec<_> = plugin
        .cli_checks
        .iter()
        .map(|check| string_slice(&check.args))
        .collect();
    if cli_ids
        != [
            "plugins-help",
            "plugins-list-help",
            "plugins-enable-help",
            "plugins-disable-help",
        ]
        || cli_args
            != [
                vec!["plugins", "--help"],
                vec!["plugins", "list", "--help"],
                vec!["plugins", "enable", "--help"],
                vec!["plugins", "disable", "--help"],
            ]
    {
        return Err(fail("Hermes plugin CLI parser contract changed"));
    }
    if plugin.lifecycle.plugin_name != EXPECTED_PLUGIN_NAME
        || plugin.lifecycle.plugin_key != EXPECTED_PLUGIN_KEY
        || string_slice(&plugin.lifecycle.list_args) != ["plugins", "list", "--json"]
        || string_slice(&plugin.lifecycle.enable_args)
            != [
                "plugins",
                "enable",
                EXPECTED_PLUGIN_NAME,
                "--no-allow-tool-override",
            ]
        || string_slice(&plugin.lifecycle.disable_args)
            != ["plugins", "disable", EXPECTED_PLUGIN_NAME]
        || string_slice(&plugin.lifecycle.not_enabled_text)
            != ["\"name\": \"pohunek\"", "\"status\": \"not enabled\""]
        || string_slice(&plugin.lifecycle.enable_text)
            != [
                "Plugin",
                EXPECTED_PLUGIN_NAME,
                "enabled.",
                "Takes effect on next session.",
            ]
        || string_slice(&plugin.lifecycle.enabled_text)
            != ["\"name\": \"pohunek\"", "\"status\": \"enabled\""]
        || string_slice(&plugin.lifecycle.disable_text)
            != [
                "Plugin",
                EXPECTED_PLUGIN_NAME,
                "disabled.",
                "Takes effect on next session.",
            ]
        || string_slice(&plugin.lifecycle.disabled_text)
            != ["\"name\": \"pohunek\"", "\"status\": \"disabled\""]
    {
        return Err(fail("Hermes plugin lifecycle contract changed"));
    }
    if plugin.targets.named_profile != EXPECTED_NAMED_PROFILE
        || string_slice(&plugin.targets.named_profile_args) != EXPECTED_NAMED_PROFILE_ARGS
        || plugin.targets.named_profile_relative_home != EXPECTED_NAMED_PROFILE_HOME
        || plugin.targets.custom_home_env != EXPECTED_CUSTOM_HOME_ENV
        || !plugin.targets.custom_home_must_be_absolute
    {
        return Err(fail("Hermes plugin target contract changed"));
    }
    if string_slice(&plugin.api.manifest_required_fields) != EXPECTED_PLUGIN_MANIFEST_FIELDS
        || string_slice(&plugin.api.manifest_optional_fields)
            != EXPECTED_PLUGIN_OPTIONAL_MANIFEST_FIELDS
        || plugin.api.manifest_kind != EXPECTED_PLUGIN_KIND
        || plugin.api.entrypoint_file != EXPECTED_PLUGIN_ENTRYPOINT_FILE
        || plugin.api.entrypoint != EXPECTED_PLUGIN_ENTRYPOINT
        || plugin.api.tool_method != EXPECTED_PLUGIN_TOOL_METHOD
        || string_slice(&plugin.api.tool_registration_args) != EXPECTED_PLUGIN_TOOL_ARGS
        || plugin.api.toolset != EXPECTED_PLUGIN_TOOLSET
        || plugin.api.tool_schema_type != EXPECTED_PLUGIN_TOOL_SCHEMA_TYPE
        || plugin.api.tool_handler_args != EXPECTED_PLUGIN_TOOL_HANDLER_ARGS
        || plugin.api.tool_return != EXPECTED_PLUGIN_TOOL_RETURN
        || plugin.api.hook_method != EXPECTED_PLUGIN_HOOK_METHOD
        || string_slice(&plugin.api.hook_registration_args) != EXPECTED_PLUGIN_HOOK_ARGS
        || plugin.api.skill_method != EXPECTED_PLUGIN_SKILL_METHOD
        || string_slice(&plugin.api.skill_registration_args) != EXPECTED_PLUGIN_SKILL_ARGS
        || plugin.api.skill_path_type != EXPECTED_PLUGIN_SKILL_PATH_TYPE
        || plugin.api.skill_name != EXPECTED_PLUGIN_NAME
        || plugin.api.skill_qualified_name != EXPECTED_PLUGIN_SKILL
        || plugin.api.discovery_root != EXPECTED_PLUGIN_DISCOVERY_ROOT
        || plugin.api.discovery_category != EXPECTED_PLUGIN_DISCOVERY_CATEGORY
        || plugin.api.maximum_category_depth != EXPECTED_PLUGIN_DISCOVERY_DEPTH
    {
        return Err(fail("Hermes plugin API contract changed"));
    }
    validate_integration_contract(&plugin.integration_lifecycle)?;
    Ok(())
}

fn validate_integration_contract(contract: &IntegrationLifecycle) -> Result<(), XtaskError> {
    let expected_steps = [
        (IntegrationAction::Install, IntegrationState::Installed),
        (IntegrationAction::Status, IntegrationState::Installed),
        (IntegrationAction::Doctor, IntegrationState::Healthy),
        (IntegrationAction::Uninstall, IntegrationState::Absent),
    ];
    let steps: Vec<_> = contract
        .steps
        .iter()
        .map(|step| (step.action, step.expected_state))
        .collect();
    if contract.target_kind != "named_profile"
        || contract.target_value != EXPECTED_NAMED_PROFILE
        || contract.access_mode != "read_only"
        || string_slice(&contract.allowed_hosts) != ["local"]
        || steps != expected_steps
    {
        return Err(fail("Pohunek integration lifecycle contract changed"));
    }
    Ok(())
}

fn check_pohunek_integration(
    hermes_bin: &Path,
    pohunek_bin: &Path,
    contract: &IntegrationLifecycle,
    limits: Limits,
) -> Result<(), XtaskError> {
    let isolation = production_integration_isolation()?;
    let hermes_home = isolation.home.join(".hermes");
    let profile_home = hermes_home.join("profiles").join(EXPECTED_NAMED_PROFILE);
    let state_home = isolation.root.join("xdg-state");
    let runtime_home = isolation.root.join("xdg-runtime");
    for path in [
        &isolation.home,
        &hermes_home,
        &hermes_home.join("profiles"),
        &profile_home,
        &state_home,
        &runtime_home,
    ] {
        create_private_directory(path)?;
    }
    let env = integration_environment(&isolation, &hermes_home, &state_home, &runtime_home);
    let [install, status, doctor, uninstall] = contract.steps.as_slice() else {
        return Err(fail("Pohunek integration lifecycle step count changed"));
    };

    run_pohunek_integration_action(
        pohunek_bin,
        hermes_bin,
        install,
        &isolation.work,
        &env,
        limits,
    )?;
    run_pohunek_integration_action(
        pohunek_bin,
        hermes_bin,
        status,
        &isolation.work,
        &env,
        limits,
    )?;
    run_production_plugin_runtime(hermes_bin, &profile_home, &isolation.work, &env, limits)?;
    run_pohunek_integration_action(
        pohunek_bin,
        hermes_bin,
        doctor,
        &isolation.work,
        &env,
        limits,
    )?;
    run_pohunek_integration_action(
        pohunek_bin,
        hermes_bin,
        uninstall,
        &isolation.work,
        &env,
        limits,
    )?;
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), XtaskError> {
    fs::create_dir_all(path).map_err(|error| {
        fail(format!(
            "failed to create private compatibility directory: {error}"
        ))
    })?;
    #[cfg(unix)]
    secure_private_directory(path)?;
    Ok(())
}

#[cfg(unix)]
fn secure_private_directory(path: &Path) -> Result<(), XtaskError> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        fail(format!(
            "failed to secure private compatibility directory: {error}"
        ))
    })
}

fn integration_environment(
    isolation: &Isolation,
    hermes_home: &Path,
    state_home: &Path,
    runtime_home: &Path,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = isolation
        .isolation_env()
        .into_iter()
        .filter(|(name, _value)| name != "HERMES_HOME")
        .collect();
    env.extend([
        (
            OsString::from("HERMES_HOME"),
            hermes_home.as_os_str().to_owned(),
        ),
        (
            OsString::from("XDG_STATE_HOME"),
            state_home.as_os_str().to_owned(),
        ),
        (
            OsString::from("XDG_RUNTIME_DIR"),
            runtime_home.as_os_str().to_owned(),
        ),
        (OsString::from("NO_COLOR"), OsString::from("1")),
        (OsString::from("TERM"), OsString::from("dumb")),
    ]);
    env
}

fn run_pohunek_integration_action(
    pohunek_bin: &Path,
    hermes_bin: &Path,
    step: &IntegrationStep,
    cwd: &Path,
    env: &[(OsString, OsString)],
    limits: Limits,
) -> Result<(), XtaskError> {
    let mut args = vec![
        "integration".to_owned(),
        integration_action_name(step.action).to_owned(),
        "--agent".to_owned(),
        "hermes".to_owned(),
        "--hermes-profile".to_owned(),
        EXPECTED_NAMED_PROFILE.to_owned(),
        "--hermes-bin".to_owned(),
        hermes_bin.as_os_str().to_string_lossy().into_owned(),
    ];
    if step.action == IntegrationAction::Install {
        args.extend([
            "--pohunek-bin".to_owned(),
            pohunek_bin.as_os_str().to_string_lossy().into_owned(),
            "--access-mode".to_owned(),
            "read_only".to_owned(),
            "--allow-host".to_owned(),
            "local".to_owned(),
        ]);
    }
    args.push("--json".to_owned());
    let output = run_process(
        pohunek_bin,
        &args,
        cwd,
        env,
        limits.integration_action_timeout,
        limits.command_output_bytes,
    )?;
    if output.status.code() != Some(0) {
        return Err(fail(format!(
            "Pohunek integration action `{}` exited unsuccessfully",
            integration_action_name(step.action)
        )));
    }
    let envelope: PohunekEnvelope = serde_json::from_slice(&output.stdout).map_err(|_error| {
        fail(format!(
            "Pohunek integration action `{}` returned a malformed envelope",
            integration_action_name(step.action)
        ))
    })?;
    validate_pohunek_envelope(step, &envelope)
}

fn integration_action_name(action: IntegrationAction) -> &'static str {
    match action {
        IntegrationAction::Install => "install",
        IntegrationAction::Status => "status",
        IntegrationAction::Doctor => "doctor",
        IntegrationAction::Uninstall => "uninstall",
    }
}

fn validate_pohunek_envelope(
    step: &IntegrationStep,
    envelope: &PohunekEnvelope,
) -> Result<(), XtaskError> {
    let result = &envelope.ok;
    if envelope.cli_version.is_empty()
        || envelope.protocol.minimum != EXPECTED_POHUNEK_PROTOCOL
        || envelope.protocol.maximum != EXPECTED_POHUNEK_PROTOCOL
        || result.action != integration_action_name(step.action)
        || result.target_kind != "profile"
        || result.target_label != EXPECTED_NAMED_PROFILE
        || result.modified
        || result.stale_stage
        || result.stale_backup
    {
        return Err(fail(format!(
            "Pohunek integration action `{}` returned an invalid envelope",
            integration_action_name(step.action)
        )));
    }
    let observed = match step.action {
        IntegrationAction::Install | IntegrationAction::Status
            if result.installed && result.enabled =>
        {
            IntegrationState::Installed
        }
        IntegrationAction::Doctor
            if result.installed
                && result.enabled
                && result.doctor.as_ref().is_some_and(doctor_is_healthy) =>
        {
            IntegrationState::Healthy
        }
        IntegrationAction::Uninstall if !result.installed && !result.enabled => {
            IntegrationState::Absent
        }
        _ => {
            return Err(fail(format!(
                "Pohunek integration action `{}` returned the wrong state",
                integration_action_name(step.action)
            )))
        }
    };
    if observed != step.expected_state {
        return Err(fail(format!(
            "Pohunek integration action `{}` violated the locked state",
            integration_action_name(step.action)
        )));
    }
    let policy_matches = match step.action {
        IntegrationAction::Install | IntegrationAction::Status => {
            result.access_mode.as_deref() == Some("read_only")
                && result.allowed_host_count == Some(1)
        }
        IntegrationAction::Doctor | IntegrationAction::Uninstall => {
            result.access_mode.is_none() && result.allowed_host_count.is_none()
        }
    };
    if !policy_matches {
        return Err(fail(format!(
            "Pohunek integration action `{}` returned the wrong policy state",
            integration_action_name(step.action)
        )));
    }
    Ok(())
}

fn doctor_is_healthy(report: &PohunekDoctorReport) -> bool {
    report.ok
        && report.checks.len() == 15
        && report.checks.iter().all(|check| {
            !check.code.is_empty() && check.status == "pass" && check.recovery_hint == "none"
        })
}

fn run_production_plugin_runtime(
    hermes_bin: &Path,
    profile_home: &Path,
    cwd: &Path,
    env: &[(OsString, OsString)],
    limits: Limits,
) -> Result<(), XtaskError> {
    let python = sibling_python(hermes_bin)?;
    let mut runtime_env = env.to_vec();
    runtime_env.retain(|(name, _value)| name != "HERMES_HOME");
    runtime_env.push((
        OsString::from("HERMES_HOME"),
        profile_home.as_os_str().to_owned(),
    ));
    let args = vec!["-c".to_owned(), PRODUCTION_PLUGIN_RUNTIME_SMOKE.to_owned()];
    let output = run_process(
        &python,
        &args,
        cwd,
        &runtime_env,
        limits.command_timeout,
        limits.command_output_bytes,
    )?;
    if output.status.code() != Some(0)
        || !String::from_utf8_lossy(&output.stdout).contains(PRODUCTION_PLUGIN_RUNTIME_MARKER)
    {
        return Err(fail(
            "pinned Hermes rejected the production Pohunek plugin registration",
        ));
    }
    Ok(())
}

fn string_slice(values: &[String]) -> Vec<&str> {
    values.iter().map(String::as_str).collect()
}

fn load_golden_manifest(repo: &Path) -> Result<GoldenManifest, XtaskError> {
    let path = repo.join(GOLDEN_ROOT).join(GOLDEN_MANIFEST);
    let bytes = fs::read(&path)
        .map_err(|error| fail(format!("failed to read Hermes golden manifest: {error}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| fail(format!("invalid Hermes golden manifest: {error}")))
}

fn validate_golden_manifest(
    repo: &Path,
    lock: &Lock,
    manifest: &GoldenManifest,
    max_bytes: usize,
    allow_pending: bool,
) -> Result<(), XtaskError> {
    if manifest.schema_version != GOLDEN_SCHEMA || manifest.release != lock.release {
        return Err(fail(
            "Hermes golden manifest does not match the pinned release",
        ));
    }
    let ids: Vec<_> = manifest
        .records
        .iter()
        .map(|record| record.id.as_str())
        .collect();
    if ids != GOLDEN_STATES {
        return Err(fail("Hermes golden manifest state inventory is incomplete"));
    }
    for record in &manifest.records {
        validate_golden_record(repo, &lock.release, record, max_bytes)?;
    }
    if !allow_pending {
        if let Some(record) = manifest
            .records
            .iter()
            .find(|record| record.status == GoldenStatus::Pending)
        {
            return Err(fail(format!(
                "Hermes golden `{}` is pending; run the explicit refresh command and review its output",
                record.id
            )));
        }
    }
    Ok(())
}

fn validate_golden_record(
    repo: &Path,
    release: &str,
    record: &GoldenRecord,
    max_bytes: usize,
) -> Result<(), XtaskError> {
    let expected_mode = if record.id == "alternate-screen-tui" {
        "alternate_screen_tui"
    } else {
        "classic"
    };
    if record.mode != expected_mode {
        return Err(fail(format!(
            "Hermes golden `{}` has an invalid display mode",
            record.id
        )));
    }
    match record.status {
        GoldenStatus::Pending => validate_pending_golden(record),
        GoldenStatus::Captured | GoldenStatus::Unsupported => {
            validate_committed_golden(repo, release, record, max_bytes)
        }
    }
}

fn validate_pending_golden(record: &GoldenRecord) -> Result<(), XtaskError> {
    if record.file.is_some() || record.sha256.is_some() || record.note.is_none() {
        return Err(fail(format!(
            "pending Hermes golden `{}` has invalid metadata",
            record.id
        )));
    }
    Ok(())
}

fn validate_committed_golden(
    repo: &Path,
    release: &str,
    record: &GoldenRecord,
    max_bytes: usize,
) -> Result<(), XtaskError> {
    if record.status == GoldenStatus::Unsupported && record.id != "alternate-screen-tui" {
        return Err(fail(
            "only the Hermes alternate-screen TUI golden may be unsupported",
        ));
    }
    let file = record
        .file
        .as_deref()
        .ok_or_else(|| fail(format!("Hermes golden `{}` has no file", record.id)))?;
    if file != format!("{}.txt", record.id) {
        return Err(fail(format!(
            "Hermes golden `{}` uses an unexpected file name",
            record.id
        )));
    }
    let bytes = fs::read(repo.join(GOLDEN_ROOT).join(file)).map_err(|error| {
        fail(format!(
            "failed to read Hermes golden `{}`: {error}",
            record.id
        ))
    })?;
    if bytes.len() > max_bytes {
        return Err(fail(format!("Hermes golden `{}` is oversized", record.id)));
    }
    let digest = record
        .sha256
        .as_deref()
        .ok_or_else(|| fail(format!("Hermes golden `{}` has no checksum", record.id)))?;
    if sha256(&bytes) != digest {
        return Err(fail(format!(
            "Hermes golden `{}` checksum mismatch",
            record.id
        )));
    }
    let fixture = std::str::from_utf8(&bytes).map_err(|error| {
        fail(format!(
            "Hermes golden `{}` is not valid UTF-8: {error}",
            record.id
        ))
    })?;
    validate_safe_fixture(fixture)?;
    if record.status == GoldenStatus::Unsupported {
        if record.note.as_deref().is_none_or(str::is_empty) || !recognized_tui_unavailable(&bytes) {
            return Err(fail(
                "unsupported Hermes TUI evidence lacks a recognized bounded local-unavailable diagnosis",
            ));
        }
    } else if record.note.is_some() {
        return Err(fail(format!(
            "captured Hermes golden `{}` has an unexpected note",
            record.id
        )));
    }
    validate_golden_fixture(record, release, fixture)
}

fn run_process(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(OsString, OsString)],
    timeout: Duration,
    max_bytes: usize,
) -> Result<ProcessOutput, XtaskError> {
    require_process_containment()?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| fail(format!("failed to start Hermes CLI check: {error}")))?;
    let process_id = child.id();
    let _group_guard = ProcessGroupGuard::new(process_id);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| fail("Hermes CLI stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| fail("Hermes CLI stderr was not captured"))?;
    let stdout_reader = spawn_bounded_reader(stdout, max_bytes);
    let stderr_reader = spawn_bounded_reader(stderr, max_bytes);
    let (status, timed_out) = finish_process(&mut child, process_id, timeout)?;
    let stdout = receive_process_reader(&stdout_reader, "stdout")?;
    let stderr = receive_process_reader(&stderr_reader, "stderr")?;
    if timed_out {
        return Err(fail("Hermes CLI check exceeded its time limit"));
    }
    if stdout.1 || stderr.1 {
        return Err(fail("Hermes CLI check exceeded its output limit"));
    }
    Ok(ProcessOutput {
        status,
        stdout: stdout.0,
        stderr: stderr.0,
    })
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

fn finish_process(
    child: &mut std::process::Child,
    process_id: u32,
    timeout: Duration,
) -> Result<(ExitStatus, bool), XtaskError> {
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut reap_deadline = None;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| fail(format!("failed to poll Hermes CLI check: {error}")))?
        {
            kill_process_group(process_id);
            return Ok((status, timed_out));
        }
        if !timed_out && Instant::now() >= deadline {
            timed_out = true;
            kill_process_group(process_id);
            let _ = child.kill();
            reap_deadline = Some(Instant::now() + READER_GRACE);
        }
        if reap_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(fail(
                "Hermes CLI check child could not be reaped within its time limit",
            ));
        }
        thread::sleep(PROCESS_POLL_INTERVAL.min(timeout));
    }
}

fn read_bounded(mut reader: impl Read, max_bytes: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut overflow = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
        overflow |= count > remaining;
    }
    Ok((output, overflow))
}

fn spawn_bounded_reader(
    reader: impl Read + Send + 'static,
    max_bytes: usize,
) -> mpsc::Receiver<io::Result<(Vec<u8>, bool)>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(read_bounded(reader, max_bytes));
    });
    receiver
}

fn receive_process_reader(
    receiver: &mpsc::Receiver<io::Result<(Vec<u8>, bool)>>,
    stream: &str,
) -> Result<(Vec<u8>, bool), XtaskError> {
    receiver
        .recv_timeout(READER_GRACE)
        .map_err(|_timeout| fail(format!("Hermes {stream} reader did not close in time")))?
        .map_err(|error| fail(format!("failed to read Hermes {stream}: {error}")))
}

fn combined_text(output: &ProcessOutput) -> String {
    let mut bytes = output.stdout.clone();
    bytes.push(b'\n');
    bytes.extend_from_slice(&output.stderr);
    strip_terminal_control_bytes(&bytes, false)
}

fn classic_scenarios(limits: Limits) -> Vec<Scenario> {
    vec![
        Scenario {
            id: "prompt-ready",
            mode: "classic",
            args: vec!["chat".to_owned()],
            actions: exit_actions(limits),
        },
        turn_scenario("short-input", SHORT_PROMPT, limits.turn_wait, limits),
        turn_scenario(
            "multiline-input",
            MULTILINE_PROMPT,
            limits.turn_wait,
            limits,
        ),
        turn_scenario("working", WORKING_PROMPT, limits.working_wait, limits),
        turn_scenario(
            "approval-blocked",
            APPROVAL_PROMPT,
            limits.turn_wait,
            limits,
        ),
        turn_scenario("completion", SHORT_PROMPT, limits.turn_wait, limits),
        Scenario {
            id: "interruption",
            mode: "classic",
            args: vec!["chat".to_owned()],
            actions: vec![
                Action::WaitFor(Evidence::PromptReady, limits.startup_wait),
                Action::Write(INTERRUPTION_PROMPT),
                Action::Pause(limits.input_settle_wait),
                Action::Write(SUBMIT),
                Action::WaitFor(Evidence::Working("sleep 30"), limits.working_wait),
                Action::Write(INTERRUPT),
                Action::WaitFor(Evidence::Interrupted, limits.input_settle_wait),
                Action::Write(EXIT_COMMAND),
            ],
        },
        Scenario {
            id: "exit",
            mode: "classic",
            args: vec!["chat".to_owned()],
            actions: exit_actions(limits),
        },
    ]
}

fn turn_scenario(
    id: &'static str,
    prompt: &'static [u8],
    turn_wait: Duration,
    limits: Limits,
) -> Scenario {
    let mut actions = vec![
        Action::WaitFor(Evidence::PromptReady, limits.startup_wait),
        Action::Write(prompt),
        Action::Pause(limits.input_settle_wait),
        Action::Write(SUBMIT),
        Action::WaitFor(turn_evidence(id), turn_wait),
    ];
    if matches!(id, "working" | "approval-blocked") {
        actions.push(Action::Write(INTERRUPT));
        actions.push(Action::WaitFor(
            Evidence::Interrupted,
            limits.input_settle_wait,
        ));
    }
    actions.push(Action::Write(EXIT_COMMAND));
    Scenario {
        id,
        mode: "classic",
        args: vec!["chat".to_owned()],
        actions,
    }
}

fn turn_evidence(id: &str) -> Evidence {
    match id {
        "short-input" | "completion" => Evidence::AssistantPanel(SHORT_RESPONSE),
        "multiline-input" => Evidence::AssistantPanel(MULTILINE_RESPONSE),
        "working" => Evidence::Working("sleep 8"),
        "approval-blocked" => Evidence::Approval,
        _ => unreachable!("turn scenarios use a closed evidence inventory"),
    }
}

fn exit_actions(limits: Limits) -> Vec<Action> {
    vec![
        Action::WaitFor(Evidence::PromptReady, limits.startup_wait),
        Action::Write(EXIT_COMMAND),
    ]
}

fn tui_scenario(limits: Limits) -> Scenario {
    Scenario {
        id: "alternate-screen-tui",
        mode: "alternate_screen_tui",
        args: vec!["chat".to_owned(), "--tui".to_owned()],
        actions: vec![
            Action::WaitFor(Evidence::AlternateScreen, limits.startup_wait),
            Action::Write(INTERRUPT),
        ],
    }
}

fn run_pty(
    program: &Path,
    scenario: &Scenario,
    isolation: &Isolation,
    proxy_url: &str,
    diagnostic_paths: &[(String, &'static str)],
    limits: Limits,
) -> Result<PtyCapture, XtaskError> {
    require_process_containment()?;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: PTY_ROWS,
            cols: PTY_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| fail(format!("failed to allocate Hermes PTY: {error}")))?;
    let mut command = CommandBuilder::new(program);
    command.args(&scenario.args);
    command.cwd(&isolation.work);
    command.env_clear();
    for (key, value) in isolation.refresh_env(proxy_url) {
        command.env(key, value);
    }
    if scenario.id == "alternate-screen-tui" {
        command.env("PATH", &isolation.tui_path);
    }
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| fail(format!("failed to open Hermes PTY input: {error}")))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| fail(format!("failed to open Hermes PTY output: {error}")))?;
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| fail(format!("failed to start Hermes PTY: {error}")))?;
    let Some(process_id) = child.process_id() else {
        reap_uncontained_pty_child(&mut *child)?;
        return Err(fail("Hermes PTY did not expose a process id"));
    };
    let _group_guard = ProcessGroupGuard::new(process_id);
    drop(pair.slave);
    let output = Arc::new(Mutex::new((Vec::new(), false)));
    let reader = spawn_pty_reader(reader, Arc::clone(&output), limits.pty_output_bytes);
    let action_result = run_pty_actions(scenario, &mut *child, &mut writer, &output);
    let finish_result = finish_pty_child(
        &mut *child,
        process_id,
        action_result.is_err(),
        limits.exit_grace,
    );
    drop(writer);
    drop(pair.master);
    reader
        .recv_timeout(READER_GRACE)
        .map_err(|_timeout| fail("Hermes PTY reader did not close within its time limit"))?
        .map_err(|error| fail(format!("failed to read Hermes PTY: {error}")))?;
    let (bytes, overflow) = output_snapshot(&output)?;
    if let Err(error) = action_result {
        return Err(with_pty_diagnostic(&error, &bytes, diagnostic_paths));
    }
    let (exit_code, killed) = finish_result?;
    if overflow {
        return Err(fail(format!(
            "Hermes PTY scenario `{}` exceeded its output limit",
            scenario.id
        )));
    }
    Ok(PtyCapture {
        bytes,
        exit_code,
        killed,
    })
}

fn spawn_pty_reader(
    mut reader: Box<dyn Read + Send>,
    output: Arc<Mutex<(Vec<u8>, bool)>>,
    max_bytes: usize,
) -> mpsc::Receiver<io::Result<()>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = (|| {
            let mut buffer = [0_u8; 8192];
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    return Ok(());
                }
                let mut locked = output
                    .lock()
                    .map_err(|_poison| io::Error::other("Hermes PTY output lock poisoned"))?;
                let remaining = max_bytes.saturating_sub(locked.0.len());
                locked.0.extend_from_slice(&buffer[..count.min(remaining)]);
                locked.1 |= count > remaining;
                if locked.1 {
                    return Ok(());
                }
            }
        })();
        let _ = sender.send(result);
    });
    receiver
}

fn run_pty_actions(
    scenario: &Scenario,
    child: &mut dyn portable_pty::Child,
    writer: &mut dyn Write,
    output: &Arc<Mutex<(Vec<u8>, bool)>>,
) -> Result<(), XtaskError> {
    for action in &scenario.actions {
        match action {
            Action::WaitFor(evidence, timeout) => {
                let observed = wait_for_evidence(scenario.id, child, output, evidence, *timeout)?;
                if !observed && !matches!(evidence, Evidence::AlternateScreen) {
                    return Err(fail(format!(
                        "Hermes PTY scenario `{}` exited before required evidence appeared",
                        scenario.id
                    )));
                }
                if !observed {
                    break;
                }
            }
            Action::Pause(duration) => thread::sleep(*duration),
            Action::Write(bytes) => {
                writer
                    .write_all(bytes)
                    .map_err(|error| fail(format!("failed to write Hermes PTY input: {error}")))?;
                writer
                    .flush()
                    .map_err(|error| fail(format!("failed to flush Hermes PTY input: {error}")))?;
            }
        }
    }
    Ok(())
}

fn wait_for_evidence(
    scenario_id: &str,
    child: &mut dyn portable_pty::Child,
    output: &Arc<Mutex<(Vec<u8>, bool)>>,
    evidence: &Evidence,
    timeout: Duration,
) -> Result<bool, XtaskError> {
    let deadline = Instant::now() + timeout;
    let mut panel_observer = match evidence {
        Evidence::AssistantPanel(expected) => Some(AssistantPanelObserver::new(expected)),
        _ => None,
    };
    loop {
        let (bytes, overflow) = output_snapshot(output)?;
        if overflow {
            return Err(fail(format!(
                "Hermes PTY scenario `{scenario_id}` exceeded its output limit"
            )));
        }
        let matched = if let (Evidence::AssistantPanel(_), Some(observer)) =
            (evidence, panel_observer.as_mut())
        {
            observer.feed_snapshot(&bytes) && observer.one_panel().is_some()
        } else {
            evidence_matches(evidence, &bytes)
        };
        if matched {
            return Ok(true);
        }
        if child
            .try_wait()
            .map_err(|error| fail(format!("failed to poll Hermes PTY: {error}")))?
            .is_some()
        {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            if let Some(observer) = panel_observer {
                return Err(fail(format!(
                    "Hermes PTY scenario `{scenario_id}` did not reach required assistant-panel evidence within its time limit (occurrences: {}, progress: {:?}, invalid: {}, visible events: {:?})",
                    observer.occurrence_count,
                    observer.progress,
                    observer.invalid,
                    observer.visible_events,
                )));
            }
            return Err(fail(format!(
                "Hermes PTY scenario `{scenario_id}` did not reach required evidence within its time limit"
            )));
        }
        thread::sleep(PROCESS_POLL_INTERVAL.min(timeout));
    }
}

fn evidence_matches(evidence: &Evidence, output: &[u8]) -> bool {
    if matches!(evidence, Evidence::AlternateScreen) {
        return has_alternate_screen(output);
    }
    let text = raw_terminal_transcript(output);
    match evidence {
        Evidence::PromptReady => text.contains(PROMPT_READY_MARKER),
        Evidence::AssistantPanel(_) => {
            unreachable!("assistant panels use the terminal-state matcher")
        }
        Evidence::Working(command) => has_rendered_terminal_command(&text, command),
        Evidence::Approval => {
            text.contains("Dangerous Command")
                && text.contains("HERMES_COMPAT_APPROVAL_SENTINEL")
                && text.contains("Allow once")
                && text.contains("Deny")
        }
        Evidence::Interrupted => text.contains(INTERRUPT_MARKER),
        Evidence::Resumed(reference) => text.contains(&format!("Resumed session {reference}")),
        Evidence::AlternateScreen => unreachable!("alternate-screen handled above"),
    }
}

fn output_snapshot(output: &Arc<Mutex<(Vec<u8>, bool)>>) -> Result<(Vec<u8>, bool), XtaskError> {
    output
        .lock()
        .map(|locked| locked.clone())
        .map_err(|_poison| fail("Hermes PTY output lock poisoned"))
}

struct ProcessGroupGuard {
    process_id: u32,
}

impl ProcessGroupGuard {
    fn new(process_id: u32) -> Self {
        Self { process_id }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        kill_process_group(self.process_id);
    }
}

fn finish_pty_child(
    child: &mut dyn portable_pty::Child,
    process_id: u32,
    terminate_now: bool,
    timeout: Duration,
) -> Result<(Option<u32>, bool), XtaskError> {
    let mut killed = terminate_now;
    let mut reap_deadline = None;
    if terminate_now {
        kill_process_group(process_id);
        let _ = child.kill();
        reap_deadline = Some(Instant::now() + READER_GRACE);
    }
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| fail(format!("failed to poll Hermes PTY: {error}")))?
        {
            kill_process_group(process_id);
            return Ok((Some(status.exit_code()), killed));
        }
        if !killed && Instant::now() >= deadline {
            killed = true;
            kill_process_group(process_id);
            let _ = child.kill();
            reap_deadline = Some(Instant::now() + READER_GRACE);
        }
        if reap_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(fail(
                "Hermes PTY child could not be reaped within its time limit",
            ));
        }
        thread::sleep(PROCESS_POLL_INTERVAL.min(timeout));
    }
}

fn reap_uncontained_pty_child(child: &mut dyn portable_pty::Child) -> Result<(), XtaskError> {
    let _ = child.kill();
    let deadline = Instant::now() + READER_GRACE;
    loop {
        if child
            .try_wait()
            .map_err(|error| fail(format!("failed to poll Hermes PTY: {error}")))?
            .is_some()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(fail(
                "Hermes PTY without a process id could not be reaped in time",
            ));
        }
        thread::sleep(PROCESS_POLL_INTERVAL.min(READER_GRACE));
    }
}

#[cfg(unix)]
fn kill_process_group(process_id: u32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    if let Ok(process_id) = i32::try_from(process_id) {
        let _ = killpg(Pid::from_raw(process_id), Signal::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_process_id: u32) {}

fn require_process_containment() -> Result<(), XtaskError> {
    if cfg!(unix) {
        Ok(())
    } else {
        Err(fail(
            "Hermes compatibility requires process-tree containment on this platform",
        ))
    }
}

struct TerminalRow {
    text: String,
    wrapped: bool,
}

fn visible_terminal_rows(screen: &vt100::Screen) -> Vec<TerminalRow> {
    screen
        .rows(0, PTY_COLS)
        .enumerate()
        .map(|(row, text)| TerminalRow {
            text,
            wrapped: u16::try_from(row).is_ok_and(|row| screen.row_wrapped(row)),
        })
        .collect()
}

fn terminal_history(parser: &mut vt100::Parser) -> String {
    let screen = parser.screen_mut();
    screen.set_scrollback(usize::MAX);
    let max_scrollback = screen.scrollback();
    let mut rows = visible_terminal_rows(screen);
    for offset in (0..max_scrollback).rev() {
        screen.set_scrollback(offset);
        if let Some(row) = visible_terminal_rows(screen).pop() {
            rows.push(row);
        }
    }
    screen.set_scrollback(0);
    while rows.last().is_some_and(|row| row.text.is_empty()) {
        rows.pop();
    }

    let mut transcript = String::new();
    for row in rows {
        transcript.push_str(&row.text);
        if !row.wrapped {
            transcript.push('\n');
        }
    }
    if transcript.ends_with('\n') {
        transcript.pop();
    }
    transcript
}

#[cfg(test)]
fn terminal_transcript(output: &[u8]) -> String {
    let mut terminal = TerminalCapture::new();
    terminal.feed_snapshot(output);
    terminal.transcript()
}

fn observe_assistant_panels(output: &[u8], expected: &'static str) -> AssistantPanelObserver {
    let mut observer = AssistantPanelObserver::new(expected);
    observer.feed_snapshot(output);
    observer
}

#[cfg(test)]
fn assistant_panel_count(text: &str, expected: &str) -> usize {
    assistant_panels(text, expected).len()
}

fn assistant_panels(text: &str, expected: &str) -> Vec<String> {
    let lines = text.lines().collect::<Vec<_>>();
    lines
        .windows(3)
        .filter(|panel| is_assistant_panel(panel, expected))
        .map(|panel| {
            panel
                .iter()
                .map(|line| line.trim_end_matches(' '))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}

fn assistant_panel_top() -> String {
    let prefix = format!("╭─ {ASSISTANT_PANEL_TITLE} ");
    let rule_width = usize::from(PTY_COLS)
        .saturating_sub(1)
        .saturating_sub(prefix.chars().count());
    format!("{prefix}{}╮", "─".repeat(rule_width))
}

fn assistant_panel_bottom() -> String {
    format!("╰{}╯", "─".repeat(usize::from(PTY_COLS).saturating_sub(2)))
}

fn normalized_assistant_panel(expected: &str) -> String {
    format!(
        "{}\n{expected}\n{}",
        assistant_panel_top(),
        assistant_panel_bottom()
    )
}

fn is_assistant_panel(panel: &[&str], expected: &str) -> bool {
    let top = panel[0].trim_end_matches(' ');
    let bottom = panel[2].trim_end_matches(' ');

    top == assistant_panel_top()
        && panel[1].trim_end_matches(' ') == expected
        && bottom == assistant_panel_bottom()
}

fn submitted_user_turn_starts(lines: &[&str]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            if !line.trim().starts_with(USER_TURN_MARKER) {
                return None;
            }
            let first = index.saturating_sub(MAX_USER_TURN_BOUNDARY_GAP_LINES);
            lines[first..index]
                .iter()
                .any(|candidate| candidate.trim() == USER_TURN_SEPARATOR)
                .then_some(index)
        })
        .collect()
}

fn submitted_user_turn_count(text: &str) -> usize {
    let lines: Vec<_> = text.lines().collect();
    submitted_user_turn_starts(&lines).len()
}

fn normalize_preview_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extends_prompt_prefix(expected: &str, observed: &str) -> bool {
    observed == expected
        || expected
            .strip_prefix(observed)
            .is_some_and(|remainder| remainder.starts_with(' '))
}

fn has_one_submitted_user_turn(text: &str, expected_prompt: &str) -> bool {
    let lines: Vec<_> = text.lines().collect();
    let starts = submitted_user_turn_starts(&lines);
    // Pinned Hermes replays the exact submitted preview while rebuilding the
    // classic view after an interrupt. Every rendered copy must still decode
    // to the one repository-owned prompt; a different turn fails closed.
    !starts.is_empty()
        && starts
            .into_iter()
            .all(|start| submitted_user_turn_matches(&lines, start, expected_prompt))
}

fn submitted_user_turn_matches(lines: &[&str], start: usize, expected_prompt: &str) -> bool {
    let expected = normalize_preview_text(expected_prompt);
    if expected.is_empty() || expected.len() > MAX_USER_TURN_PREVIEW_BYTES {
        return false;
    }

    let Some(first) = lines[start]
        .trim()
        .strip_prefix(USER_TURN_MARKER)
        .map(str::trim_start)
    else {
        return false;
    };
    let mut observed = normalize_preview_text(first);
    if observed.is_empty()
        || observed.len() > MAX_USER_TURN_PREVIEW_BYTES
        || !extends_prompt_prefix(&expected, &observed)
    {
        return false;
    }
    if observed == expected {
        return true;
    }

    let mut fragments = 1;
    let mut gap = 0;
    for line in lines[start + 1..]
        .iter()
        .take(MAX_USER_TURN_PREVIEW_SCAN_LINES)
    {
        let trimmed = line.trim();
        if trimmed == USER_TURN_SEPARATOR {
            return false;
        }
        let fragment = normalize_preview_text(trimmed);
        let candidate = format!("{observed} {fragment}");
        if !fragment.is_empty() && extends_prompt_prefix(&expected, &candidate) {
            fragments += 1;
            if fragments > MAX_USER_TURN_PREVIEW_LINES
                || candidate.len() > MAX_USER_TURN_PREVIEW_BYTES
            {
                return false;
            }
            observed = candidate;
            gap = 0;
            if observed == expected {
                return true;
            }
        } else {
            gap += 1;
            if gap > MAX_USER_TURN_BOUNDARY_GAP_LINES {
                return false;
            }
        }
    }
    false
}

fn has_sanitized_session_reference(text: &str) -> bool {
    let pattern = Regex::new(r"(?im)(?:Session ID|Session|session_id):\s*<SESSION_ID>")
        .expect("sanitized session-reference regex is valid");
    pattern.is_match(text)
}

fn has_rendered_terminal_command(text: &str, command: &str) -> bool {
    let rich_prefix = format!("💻 {command}");
    let controlled = format!("Running {command}");
    text.lines().any(|line| {
        let line = line.trim();
        let rich = line.find(&rich_prefix).is_some_and(|start| {
            let suffix = &line[start + rich_prefix.len()..];
            suffix.chars().next().is_some_and(char::is_whitespace)
                && suffix.trim_start().starts_with('(')
        });
        rich || line == controlled
    })
}

fn expected_assistant_response(id: &str) -> Option<&'static str> {
    match id {
        "short-input" | "completion" => Some(SHORT_RESPONSE),
        "multiline-input" => Some(MULTILINE_RESPONSE),
        _ => None,
    }
}

fn validate_classic_transcript(id: &str, text: &str) -> Result<(), XtaskError> {
    if !text.contains(PROMPT_READY_MARKER) {
        return Err(fail(format!(
            "Hermes PTY scenario `{id}` lacks prompt-ready evidence"
        )));
    }
    let submitted_turns = submitted_user_turn_count(text);
    let valid = match id {
        "prompt-ready" => submitted_turns == 0,
        "short-input" =>
            has_one_submitted_user_turn(text, "Reply with exactly HERMES_COMPAT_OK."),
        "multiline-input" => {
            has_one_submitted_user_turn(text, MULTILINE_PREVIEW)
        }
        "working" => {
            has_one_submitted_user_turn(
                text,
                "Use the terminal tool to run `sleep 8`, then reply exactly HERMES_WORKING_DONE.",
            ) && has_rendered_terminal_command(text, "sleep 8")
        }
        "approval-blocked" => {
            has_one_submitted_user_turn(
                text,
                "Use the terminal tool to run `rm -rf HERMES_COMPAT_APPROVAL_SENTINEL`, then reply exactly HERMES_APPROVAL_DONE.",
            ) && text.contains("Dangerous Command")
                && text.contains("HERMES_COMPAT_APPROVAL_SENTINEL")
                && text.contains("Allow once")
                && text.contains("Deny")
        }
        "completion" => {
            has_one_submitted_user_turn(text, "Reply with exactly HERMES_COMPAT_OK.")
                && (extract_session_reference(text.as_bytes()).is_some()
                    || has_sanitized_session_reference(text))
        }
        "interruption" => {
            has_one_submitted_user_turn(
                text,
                "Use the terminal tool to run `sleep 30`, then reply exactly HERMES_INTERRUPT_DONE.",
            ) && has_rendered_terminal_command(text, "sleep 30")
                && text.contains(INTERRUPT_MARKER)
        }
        "exit" => submitted_turns == 0 && text.contains("Goodbye!"),
        _ => false,
    };
    if !valid {
        return Err(fail(format!(
            "Hermes PTY scenario `{id}` lacks its required semantic evidence (submitted turns: {submitted_turns})"
        )));
    }
    Ok(())
}

fn validate_classic_capture(id: &str, capture: &PtyCapture) -> Result<(), XtaskError> {
    validate_classic_mode_and_exit(id, capture)?;
    let text = raw_terminal_transcript(&capture.bytes);
    validate_classic_transcript(id, &text)?;
    if let Some(expected) = expected_assistant_response(id) {
        let observer = observe_assistant_panels(&capture.bytes, expected);
        if observer.one_panel().is_none() {
            return Err(fail(format!(
                "Hermes PTY scenario `{id}` lacks exactly one terminal-rendered assistant panel"
            )));
        }
    }
    Ok(())
}

fn validate_resume_capture(capture: &PtyCapture, reference: &str) -> Result<(), XtaskError> {
    validate_classic_mode_and_exit("resume", capture)?;
    let text = raw_terminal_transcript(&capture.bytes);
    if !text.contains(&format!("Resumed session {reference}")) {
        return Err(fail(
            "Hermes resume capture did not restore the exact captured native reference",
        ));
    }
    Ok(())
}

fn validate_classic_mode_and_exit(id: &str, capture: &PtyCapture) -> Result<(), XtaskError> {
    if has_alternate_screen(&capture.bytes) {
        return Err(fail(format!(
            "Hermes classic PTY scenario `{id}` unexpectedly entered alternate-screen mode"
        )));
    }
    if capture.killed || capture.exit_code.is_some_and(|code| code != 0) {
        return Err(fail(format!(
            "Hermes PTY scenario `{id}` exited unsuccessfully"
        )));
    }
    Ok(())
}

fn validate_golden_fixture(
    record: &GoldenRecord,
    release: &str,
    fixture: &str,
) -> Result<(), XtaskError> {
    let expected_terminal = match (record.mode.as_str(), record.status) {
        ("classic", GoldenStatus::Captured) => "classic",
        ("alternate_screen_tui", GoldenStatus::Captured) => "alternate_screen_observed",
        ("alternate_screen_tui", GoldenStatus::Unsupported) => "alternate_screen_not_observed",
        _ => {
            return Err(fail(format!(
                "Hermes golden `{}` has an invalid status/display combination",
                record.id
            )))
        }
    };
    let header = format!(
        "# Hermes PTY compatibility golden\nstate: {}\nmode: {}\nrelease: {release}\nterminal: {expected_terminal}\n",
        record.id, record.mode
    );
    let remainder = fixture.strip_prefix(&header).ok_or_else(|| {
        fail(format!(
            "Hermes golden `{}` has invalid evidence metadata",
            record.id
        ))
    })?;
    let (metadata, body) = remainder.split_once("\n\n").ok_or_else(|| {
        fail(format!(
            "Hermes golden `{}` has no bounded transcript",
            record.id
        ))
    })?;
    let (process, panel_label) = metadata
        .strip_prefix("process: ")
        .and_then(|metadata| metadata.split_once("\nassistant_panel: "))
        .ok_or_else(|| {
            fail(format!(
                "Hermes golden `{}` has invalid evidence metadata",
                record.id
            ))
        })?;
    let valid_process = match (record.mode.as_str(), record.status) {
        ("alternate_screen_tui", GoldenStatus::Captured) => {
            matches!(process, "exited" | "terminated_by_bounded_harness")
        }
        ("classic", GoldenStatus::Captured)
        | ("alternate_screen_tui", GoldenStatus::Unsupported) => process == "exited",
        _ => false,
    };
    let expected_response = expected_assistant_response(&record.id);
    let expected_panel_label = if expected_response.is_some() {
        ASSISTANT_PANEL_SECTION.trim_end_matches(':')
    } else {
        "none"
    };
    if !valid_process || panel_label != expected_panel_label {
        return Err(fail(format!(
            "Hermes golden `{}` has invalid process or panel metadata",
            record.id
        )));
    }
    let transcript = validate_golden_evidence_sections(record, body, expected_response)?;

    validate_golden_semantics(record, transcript)
}

fn validate_golden_evidence_sections<'a>(
    record: &GoldenRecord,
    body: &'a str,
    expected_response: Option<&str>,
) -> Result<&'a str, XtaskError> {
    let structured = body
        .strip_prefix(&format!("{RAW_TRANSCRIPT_SECTION}\n"))
        .ok_or_else(|| {
            fail(format!(
                "Hermes golden `{}` lacks its bounded raw transcript section",
                record.id
            ))
        })?;
    let panel_boundary = format!("\n\n{ASSISTANT_PANEL_SECTION}\n");
    let transcript = if let Some(expected) = expected_response {
        let (transcript, panel) = structured.split_once(&panel_boundary).ok_or_else(|| {
            fail(format!(
                "Hermes golden `{}` lacks derived terminal assistant-panel evidence",
                record.id
            ))
        })?;
        let normalized_panel = panel.trim_end_matches('\n');
        let panels = assistant_panels(normalized_panel, expected);
        if panels.len() != 1 || panels[0] != normalized_panel {
            return Err(fail(format!(
                "Hermes golden `{}` lacks exactly one normalized terminal assistant panel",
                record.id
            )));
        }
        transcript
    } else {
        if structured.contains(&panel_boundary) {
            return Err(fail(format!(
                "Hermes golden `{}` has unexpected terminal assistant-panel evidence",
                record.id
            )));
        }
        structured.trim_end_matches('\n')
    };
    if transcript.trim().is_empty() {
        return Err(fail(format!(
            "Hermes golden `{}` has invalid transcript or panel evidence",
            record.id
        )));
    }
    Ok(transcript)
}

fn validate_golden_semantics(record: &GoldenRecord, transcript: &str) -> Result<(), XtaskError> {
    match (record.id.as_str(), record.status) {
        ("alternate-screen-tui", GoldenStatus::Captured) => {
            if !transcript.contains("Hermes") || recognized_tui_unavailable(transcript.as_bytes()) {
                return Err(fail(
                    "captured Hermes alternate-screen TUI golden lacks semantic TUI evidence",
                ));
            }
        }
        ("alternate-screen-tui", GoldenStatus::Unsupported) => {
            if !recognized_tui_unavailable(transcript.as_bytes()) {
                return Err(fail(
                    "unsupported Hermes TUI evidence lacks a recognized bounded local-unavailable diagnosis",
                ));
            }
        }
        ("resume", GoldenStatus::Captured) => {
            if !transcript.contains("Resumed session <SESSION_ID>")
                || !has_sanitized_session_reference(transcript)
            {
                return Err(fail(
                    "Hermes resume golden lacks its exact sanitized native reference evidence",
                ));
            }
        }
        (id, GoldenStatus::Captured) => validate_classic_transcript(id, transcript)?,
        _ => {
            return Err(fail(format!(
                "Hermes golden `{}` has an invalid semantic state",
                record.id
            )))
        }
    }
    Ok(())
}

fn captured_assistant_panel(id: &str, capture: &PtyCapture) -> Result<Option<String>, XtaskError> {
    let Some(expected) = expected_assistant_response(id) else {
        return Ok(None);
    };
    let observer = observe_assistant_panels(&capture.bytes, expected);
    let Some(panel) = observer.one_panel() else {
        return Err(fail(format!(
            "Hermes golden `{id}` requires exactly one captured terminal assistant panel"
        )));
    };
    Ok(Some(panel.to_owned()))
}

fn recognized_tui_unavailable(output: &[u8]) -> bool {
    if output.is_empty() || output.len() > MAX_PTY_OUTPUT_BYTES {
        return false;
    }
    let text = strip_terminal_control_bytes(output, false);
    TUI_UNAVAILABLE_MARKERS
        .iter()
        .any(|marker| text.contains(marker))
}

fn extract_session_reference(output: &[u8]) -> Option<String> {
    let text = raw_terminal_transcript(output);
    let pattern = Regex::new(r"(?im)(?:Session ID|Session|session_id):\s*([A-Za-z0-9_.:-]+)")
        .expect("session-reference regex is valid");
    pattern
        .captures_iter(&text)
        .last()
        .map(|captures| captures[1].to_owned())
}

fn has_alternate_screen(output: &[u8]) -> bool {
    const ENTER_SEQUENCES: [&[u8]; 3] = [b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h"];

    ENTER_SEQUENCES.iter().any(|sequence| {
        output
            .windows(sequence.len())
            .any(|window| window == *sequence)
    })
}

fn write_goldens(
    repo: &Path,
    lock: &Lock,
    hermes_bin: &Path,
    isolation: &Isolation,
    mock_base_url: &str,
    captures: Vec<(&'static str, &'static str, GoldenStatus, PtyCapture)>,
    limits: Limits,
) -> Result<RefreshSummary, XtaskError> {
    let golden_root = repo.join(GOLDEN_ROOT);
    fs::create_dir_all(&golden_root)
        .map_err(|error| fail(format!("failed to create Hermes golden directory: {error}")))?;
    let replacements = sensitive_paths(repo, hermes_bin, isolation, mock_base_url);
    let mut staged = Vec::new();
    let mut records = Vec::new();
    let mut unsupported = 0;

    for (id, mode, status, capture) in captures {
        let body = sanitize_output(&capture.bytes, &replacements);
        validate_safe_fixture(&body)?;
        let panel = captured_assistant_panel(id, &capture)?;
        if let Some(panel) = panel.as_deref() {
            validate_safe_fixture(panel)?;
        }
        let disposition = if capture.killed {
            "terminated_by_bounded_harness"
        } else {
            "exited"
        };
        let terminal = if has_alternate_screen(&capture.bytes) {
            "alternate_screen_observed"
        } else if mode == "classic" {
            "classic"
        } else {
            "alternate_screen_not_observed"
        };
        let panel_label = if panel.is_some() {
            ASSISTANT_PANEL_SECTION.trim_end_matches(':')
        } else {
            "none"
        };
        let mut evidence = format!("{RAW_TRANSCRIPT_SECTION}\n{}", body.trim_end());
        if let Some(panel) = panel {
            evidence.push_str("\n\n");
            evidence.push_str(ASSISTANT_PANEL_SECTION);
            evidence.push('\n');
            evidence.push_str(&panel);
        }
        let rendered = format!(
            "# Hermes PTY compatibility golden\nstate: {id}\nmode: {mode}\nrelease: {}\nterminal: {terminal}\nprocess: {disposition}\nassistant_panel: {panel_label}\n\n{}\n",
            lock.release,
            evidence.trim_end()
        );
        if rendered.len() > limits.pty_output_bytes {
            return Err(fail(format!("sanitized Hermes golden `{id}` is oversized")));
        }
        validate_safe_fixture(&rendered)?;
        let file = format!("{id}.txt");
        let digest = sha256(rendered.as_bytes());
        let note = match status {
            GoldenStatus::Unsupported => {
                unsupported += 1;
                Some("Local alternate-screen TUI could not be exercised in the isolated refresh environment.".to_owned())
            }
            GoldenStatus::Captured | GoldenStatus::Pending => None,
        };
        staged.push((golden_root.join(&file), rendered));
        records.push(GoldenRecord {
            id: id.to_owned(),
            mode: mode.to_owned(),
            status,
            file: Some(file),
            sha256: Some(digest),
            note,
        });
    }
    let ids: Vec<_> = records.iter().map(|record| record.id.as_str()).collect();
    if ids != GOLDEN_STATES {
        return Err(fail("refreshed Hermes golden inventory is incomplete"));
    }
    let manifest = GoldenManifest {
        schema_version: GOLDEN_SCHEMA,
        release: lock.release.clone(),
        records,
    };
    let mut manifest_json = serde_json::to_string_pretty(&manifest).map_err(|error| {
        fail(format!(
            "failed to serialize Hermes golden manifest: {error}"
        ))
    })?;
    manifest_json.push('\n');

    for (path, content) in &staged {
        fs::write(path, content)
            .map_err(|error| fail(format!("failed to write a Hermes golden: {error}")))?;
    }
    let manifest_path = golden_root.join(GOLDEN_MANIFEST);
    fs::write(&manifest_path, manifest_json)
        .map_err(|error| fail(format!("failed to write Hermes golden manifest: {error}")))?;
    validate_golden_manifest(repo, lock, &manifest, limits.pty_output_bytes, false)?;

    Ok(RefreshSummary {
        captures: staged.len() - unsupported,
        unsupported,
        manifest_path,
    })
}

fn sensitive_paths(
    repo: &Path,
    hermes_bin: &Path,
    isolation: &Isolation,
    mock_base_url: &str,
) -> Vec<(String, &'static str)> {
    let mut paths = vec![
        (isolation.root.display().to_string(), "<ISOLATED_ROOT>"),
        (repo.display().to_string(), "<REPOSITORY>"),
        (hermes_bin.display().to_string(), "<HERMES_BIN>"),
        (mock_base_url.to_owned(), "<HERMES_MOCK_ENDPOINT>"),
        (
            mock_base_url.trim_end_matches("/v1").to_owned(),
            "<HERMES_MOCK_PROXY>",
        ),
    ];
    if let Some(parent) = hermes_bin.parent() {
        paths.push((parent.display().to_string(), "<HERMES_INSTALL_DIR>"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push((PathBuf::from(home).display().to_string(), "<USER_HOME>"));
    }
    paths.sort_by_key(|path| std::cmp::Reverse(path.0.len()));
    paths
}

fn with_pty_diagnostic(
    error: &XtaskError,
    output: &[u8],
    paths: &[(String, &'static str)],
) -> XtaskError {
    let message = error.to_string();
    match sanitized_pty_tail(output, paths) {
        Some(tail) => fail(format!("{message}\nSanitized PTY tail:\n{tail}")),
        None => fail(format!("{message}\n{PTY_DIAGNOSTIC_WITHHELD}")),
    }
}

fn sanitized_pty_tail(output: &[u8], paths: &[(String, &'static str)]) -> Option<String> {
    let sanitized = sanitize_output(output, paths);
    if validate_safe_fixture(&sanitized).is_err() || contains_http_diagnostic_content(&sanitized) {
        return None;
    }
    let redacted = redact_diagnostic_prompts(sanitized);
    let lines = redacted
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let first = lines.len().saturating_sub(MAX_PTY_DIAGNOSTIC_LINES);
    let mut tail = lines[first..].join("\n");
    if tail.len() > MAX_PTY_DIAGNOSTIC_BYTES {
        let mut start = tail.len() - MAX_PTY_DIAGNOSTIC_BYTES;
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail.drain(..start);
    }
    if tail.is_empty() || validate_safe_fixture(&tail).is_err() {
        None
    } else {
        Some(tail)
    }
}

fn redact_diagnostic_prompts(mut text: String) -> String {
    for line in [
        SHORT_PROMPT_TEXT,
        MULTILINE_PREVIEW,
        WORKING_PROMPT_TEXT,
        APPROVAL_PROMPT_TEXT,
        INTERRUPTION_PROMPT_TEXT,
    ]
    .into_iter()
    .flat_map(str::lines)
    {
        text = text.replace(line, "<HERMES_PROMPT>");
    }
    text
}

fn contains_http_diagnostic_content(text: &str) -> bool {
    let request_line = Regex::new(
        r"(?im)(?:^|\s)(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+\S+\s+HTTP/\d(?:\.\d)?(?:\s|$)",
    )
    .expect("HTTP request-line regex is valid");
    let status_line = Regex::new(r"(?im)(?:^|\s)HTTP/\d(?:\.\d)?\s+\d{3}(?:\s|$)")
        .expect("HTTP status-line regex is valid");
    let header = Regex::new(
        r"(?im)^(?:authorization|proxy-authorization|cookie|set-cookie|x-api-key|content-length|content-type|transfer-encoding|host|user-agent|accept|connection|server|location|www-authenticate):\s*",
    )
    .expect("HTTP header regex is valid");
    let body = Regex::new(r#"(?i)\"(?:messages|tools|api_key|prompt|error)\"\s*:"#)
        .expect("HTTP body regex is valid");
    request_line.is_match(text)
        || status_line.is_match(text)
        || header.is_match(text)
        || body.is_match(text)
}

fn sanitize_output(output: &[u8], paths: &[(String, &'static str)]) -> String {
    let mut sanitized = raw_terminal_transcript(output);
    for (path, replacement) in paths {
        if !path.is_empty() {
            sanitized = sanitized.replace(path, replacement);
        }
    }
    let session =
        Regex::new(r"\b\d{8}_\d{6}_[0-9A-Za-z-]{6,}\b").expect("Hermes session regex is valid");
    sanitized = session.replace_all(&sanitized, "<SESSION_ID>").into_owned();
    let uuid = Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("UUID regex is valid");
    sanitized = uuid.replace_all(&sanitized, "<UUID>").into_owned();
    let unix_home =
        Regex::new(r"(?:/home|/Users)/[^/\s]+").expect("personal Unix path regex is valid");
    sanitized = unix_home
        .replace_all(&sanitized, "<USER_HOME>")
        .into_owned();
    let windows_home =
        Regex::new(r"(?i)\b[A-Z]:\\Users\\[^\\\s]+").expect("personal Windows path regex is valid");
    sanitized = windows_home
        .replace_all(&sanitized, "<USER_HOME>")
        .into_owned();
    let timestamp =
        Regex::new(r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})\b")
            .expect("timestamp regex is valid");
    sanitized = timestamp
        .replace_all(&sanitized, "<TIMESTAMP>")
        .into_owned();
    let tip =
        Regex::new(r"(?ms)^([ \t]*)✦ Tip:.*?(\n[ \t]*\n|\z)").expect("Hermes tip regex is valid");
    sanitized = tip
        .replace_all(&sanitized, "$1✦ Tip: <RANDOMIZED>$2")
        .into_owned();
    let status_runtime = Regex::new(r"(?m)([│]\s*)\d+(?:\.\d+)?(?:ms|s|m|h)(\s*[│])")
        .expect("Hermes status runtime regex is valid");
    sanitized = status_runtime
        .replace_all(&sanitized, "$1<RUNTIME>$2")
        .into_owned();
    let status_clock = Regex::new(r"([⏱⏲✓]\s*)\d+(?:\.\d+)?(?:ms|s|m|h)\b")
        .expect("Hermes status clock regex is valid");
    sanitized = status_clock
        .replace_all(&sanitized, "$1<RUNTIME>")
        .into_owned();
    let parenthesized_duration = Regex::new(r"\(\s*\d+(?:\.\d+)?(?:ms|s|m|h)\)")
        .expect("Hermes parenthesized duration regex is valid");
    sanitized = parenthesized_duration
        .replace_all(&sanitized, "(<DURATION>)")
        .into_owned();
    let tool_duration =
        Regex::new(r"(?m)^(\s*(?:┊\s+)?💻\s+\$\s+.*?\s+)\d+(?:\.\d+)?(?:ms|s|m|h)(\s+\[)")
            .expect("Hermes tool duration regex is valid");
    sanitized = tool_duration
        .replace_all(&sanitized, "$1<DURATION>$2")
        .into_owned();
    let final_duration = Regex::new(r"(?m)^(Duration:\s*)\d+(?:\.\d+)?(?:ms|s|m|h)\b")
        .expect("Hermes final duration regex is valid");
    sanitized = final_duration
        .replace_all(&sanitized, "$1<DURATION>")
        .into_owned();
    sanitized
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
fn strip_terminal_controls(text: &str) -> String {
    strip_terminal_control_bytes(text.as_bytes(), false)
}

fn raw_terminal_transcript(bytes: &[u8]) -> String {
    strip_terminal_control_bytes(bytes, true)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn strip_terminal_control_bytes(bytes: &[u8], preserve_carriage_returns: bool) -> String {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0xc2 {
            match bytes.get(index + 1).copied() {
                Some(0x9b) => {
                    index = skip_csi(bytes, index + 2);
                    continue;
                }
                Some(0x9d) => {
                    index = skip_control_string(bytes, index + 2, true);
                    continue;
                }
                Some(0x90 | 0x98 | 0x9e | 0x9f) => {
                    index = skip_control_string(bytes, index + 2, false);
                    continue;
                }
                Some(0x80..=0x9f) => {
                    index += 2;
                    continue;
                }
                _ => {}
            }
        }
        match bytes[index] {
            0x1b => {
                index = skip_escape_sequence(bytes, index);
            }
            0x9b => {
                index = skip_csi(bytes, index + 1);
            }
            0x9d => {
                index = skip_control_string(bytes, index + 1, true);
            }
            0x90 | 0x98 | 0x9e | 0x9f => {
                index = skip_control_string(bytes, index + 1, false);
            }
            b'\r' if preserve_carriage_returns => {
                output.push(bytes[index]);
                index += 1;
            }
            b'\n' | b'\t' | 0x20..=0x7e => {
                output.push(bytes[index]);
                index += 1;
            }
            0xc2..=0xf4 => {
                let width = utf8_sequence_width(bytes[index]);
                let end = index.saturating_add(width).min(bytes.len());
                if std::str::from_utf8(&bytes[index..end]).is_ok() && end - index == width {
                    output.extend_from_slice(&bytes[index..end]);
                    index = end;
                } else {
                    output.extend_from_slice("�".as_bytes());
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn skip_escape_sequence(bytes: &[u8], escape: usize) -> usize {
    let Some(&introducer) = bytes.get(escape + 1) else {
        return bytes.len();
    };
    match introducer {
        b'[' => skip_csi(bytes, escape + 2),
        b']' => skip_control_string(bytes, escape + 2, true),
        b'P' | b'X' | b'^' | b'_' => skip_control_string(bytes, escape + 2, false),
        0x20..=0x2f => {
            let mut index = escape + 2;
            while bytes
                .get(index)
                .is_some_and(|byte| (0x20..=0x2f).contains(byte))
            {
                index += 1;
            }
            if bytes
                .get(index)
                .is_some_and(|byte| (0x30..=0x7e).contains(byte))
            {
                index + 1
            } else {
                index
            }
        }
        0x30..=0x7e => escape + 2,
        _ => escape + 1,
    }
}

fn skip_csi(bytes: &[u8], mut index: usize) -> usize {
    while let Some(&byte) = bytes.get(index) {
        index += 1;
        if (0x40..=0x7e).contains(&byte) {
            break;
        }
    }
    index
}

fn skip_control_string(bytes: &[u8], mut index: usize, bell_terminated: bool) -> usize {
    while let Some(&byte) = bytes.get(index) {
        if (bell_terminated && byte == 0x07) || byte == 0x9c {
            return index + 1;
        }
        if byte == 0xc2 && bytes.get(index + 1).copied() == Some(0x9c) {
            return index + 2;
        }
        if byte == 0x1b && bytes.get(index + 1).copied() == Some(b'\\') {
            return index + 2;
        }
        if (0xc2..=0xf4).contains(&byte) {
            let width = utf8_sequence_width(byte);
            let end = index.saturating_add(width).min(bytes.len());
            if end - index == width && std::str::from_utf8(&bytes[index..end]).is_ok() {
                index = end;
                continue;
            }
        }
        index += 1;
    }
    index
}

fn utf8_sequence_width(byte: u8) -> usize {
    match byte {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
}

fn validate_safe_fixture(text: &str) -> Result<(), XtaskError> {
    if text
        .bytes()
        .any(|byte| byte < 0x20 && byte != b'\n' && byte != b'\t')
    {
        return Err(fail("Hermes fixture contains terminal control bytes"));
    }
    let assigned_secret = Regex::new(
        r#"(?i)(?:api[_ -]?key|auth[_ -]?token|access[_ -]?token|password|secret)\s*[:=]\s*["']?[A-Za-z0-9_./+=-]{8,}"#,
    )
    .expect("assigned-secret regex is valid");
    let token = Regex::new(r"(?i)\b(?:sk-[A-Za-z0-9_-]{8,}|gh[pousr]_[A-Za-z0-9]{8,})\b")
        .expect("token regex is valid");
    if assigned_secret.is_match(text) || token.is_match(text) {
        return Err(fail("Hermes fixture contains credential-shaped output"));
    }
    Ok(())
}

fn require_explicit_binary(path: &Path) -> Result<(), XtaskError> {
    if !path.is_absolute() {
        return Err(fail(
            "Hermes golden refresh requires an absolute --hermes-bin path",
        ));
    }
    if !path.is_file() {
        return Err(fail("Hermes golden refresh executable does not exist"));
    }
    Ok(())
}

fn require_safe_pohunek_binary(path: &Path) -> Result<PathBuf, XtaskError> {
    if !path.is_absolute() {
        return Err(fail(
            "Hermes compatibility requires an absolute --pohunek-bin path",
        ));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_error| fail("Hermes compatibility Pohunek executable does not exist"))?;
        if metadata.file_type().is_symlink() {
            return Err(fail(
                "Hermes compatibility Pohunek executable contains a symlink",
            ));
        }
    }
    let canonical = fs::canonicalize(path).map_err(|_error| {
        fail("Hermes compatibility Pohunek executable cannot be canonicalized")
    })?;
    if canonical != path {
        return Err(fail(
            "Hermes compatibility Pohunek executable is not canonical",
        ));
    }
    let metadata = fs::metadata(&canonical)
        .map_err(|_error| fail("Hermes compatibility Pohunek executable is unavailable"))?;
    if !metadata.is_file() {
        return Err(fail(
            "Hermes compatibility Pohunek executable is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 || mode & 0o022 != 0 {
            return Err(fail(
                "Hermes compatibility Pohunek executable has unsafe permissions",
            ));
        }
    }
    Ok(canonical)
}

fn resolve_compatibility_hermes_binary(path: &Path) -> Result<PathBuf, XtaskError> {
    if path.is_absolute() {
        return fs::canonicalize(path)
            .map_err(|_error| fail("Hermes compatibility executable does not exist"));
    }
    if path.components().count() != 1 {
        return Err(fail(
            "Hermes compatibility executable must be absolute or a PATH basename",
        ));
    }
    let search_path = std::env::var_os("PATH")
        .ok_or_else(|| fail("Hermes compatibility executable is not available on PATH"))?;
    for directory in std::env::split_paths(&search_path)
        .filter(|directory| directory.is_absolute())
        .take(MAX_EXECUTABLE_PATH_ENTRIES)
    {
        let candidate = directory.join(path);
        if candidate.is_file() {
            return fs::canonicalize(candidate)
                .map_err(|_error| fail("Hermes compatibility executable cannot be canonicalized"));
        }
    }
    Err(fail(
        "Hermes compatibility executable is not available on PATH",
    ))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fail(message: impl Into<String>) -> XtaskError {
    XtaskError::Usage(message.into())
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        assistant_panel_bottom, assistant_panel_count, assistant_panel_top, check_cli,
        classic_scenarios, compatibility_summary, compatibility_with, fail, finish_mocked_pty,
        has_alternate_screen, has_one_submitted_user_turn, load_golden_manifest, load_lock,
        mock_scenario, normalized_assistant_panel, observe_assistant_panels,
        raw_terminal_transcript, refresh_with, require_safe_pohunek_binary,
        run_pohunek_integration_action, run_process, sanitize_output, sanitized_pty_tail,
        select_production_integration_temp_parent, sensitive_paths, strip_terminal_controls,
        submitted_user_turn_count, terminal_transcript, validate_classic_capture,
        validate_classic_transcript, validate_golden_manifest, validate_safe_fixture,
        with_pty_diagnostic, GoldenManifest, GoldenStatus, IntegrationAction, IntegrationState,
        IntegrationStep, Isolation, Limits, PtyCapture, TerminalCapture, APPROVAL_PROMPT_TEXT,
        ASSISTANT_PANEL_SECTION, GOLDEN_MANIFEST, GOLDEN_ROOT, INTERRUPTION_PROMPT_TEXT,
        INTERRUPT_MARKER, LOCK_PATH, MAX_PTY_DIAGNOSTIC_BYTES, MAX_PTY_DIAGNOSTIC_LINES,
        MULTILINE_PREVIEW, MULTILINE_RESPONSE, PROMPT_READY_MARKER, PTY_COLS,
        PTY_DIAGNOSTIC_WITHHELD, PTY_SCROLLBACK_ROWS, SHORT_PROMPT_TEXT, SHORT_RESPONSE,
        USER_TURN_MARKER, USER_TURN_SEPARATOR, WORKING_PROMPT_TEXT,
    };

    fn fast_limits() -> Limits {
        Limits {
            command_timeout: Duration::from_secs(2),
            integration_action_timeout: Duration::from_secs(2),
            command_output_bytes: 16 * 1024,
            pty_output_bytes: 64 * 1024,
            startup_wait: Duration::from_secs(1),
            turn_wait: Duration::from_secs(1),
            working_wait: Duration::from_secs(1),
            input_settle_wait: Duration::from_secs(1),
            exit_grace: Duration::from_secs(1),
        }
    }

    fn fixture_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("create fixture repo");
        let lock = repo.path().join(LOCK_PATH);
        let manifest = repo.path().join(GOLDEN_ROOT).join(GOLDEN_MANIFEST);
        fs::create_dir_all(lock.parent().expect("lock parent")).expect("create lock parent");
        fs::create_dir_all(manifest.parent().expect("manifest parent"))
            .expect("create manifest parent");
        fs::write(
            lock,
            include_bytes!("../../../compat/hermes/compatibility-lock.json"),
        )
        .expect("write lock");
        fs::write(
            manifest,
            include_bytes!("../../../compat/hermes/goldens/manifest.json"),
        )
        .expect("write manifest");
        let source_goldens = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(GOLDEN_ROOT);
        let manifest = load_golden_manifest(repo.path()).expect("load fixture manifest");
        for file in manifest
            .records
            .iter()
            .filter_map(|record| record.file.as_deref())
        {
            fs::copy(
                source_goldens.join(file),
                repo.path().join(GOLDEN_ROOT).join(file),
            )
            .expect("copy committed golden fixture");
        }
        repo
    }

    fn terminal_row(text: &str) -> String {
        let padding = usize::from(PTY_COLS).saturating_sub(text.chars().count());
        format!("{text}{}\r\n", " ".repeat(padding))
    }

    fn assistant_panel_bytes(response: &str) -> Vec<u8> {
        [
            terminal_row(&assistant_panel_top()),
            terminal_row(response),
            terminal_row(&assistant_panel_bottom()),
        ]
        .concat()
        .into_bytes()
    }

    fn cursor_moved_panel_bytes(response: &str) -> Vec<u8> {
        format!(
            "{}\x1b[1E{}\x1b[1E{}",
            assistant_panel_top(),
            response,
            assistant_panel_bottom()
        )
        .into_bytes()
    }

    fn interleaved_panel_bytes(response: &str) -> Vec<u8> {
        format!(
            "{}\r\n\x1b[3A\x1b[Jprompt status redraw\r\n{response}\r\n\
             \x1b[3A\x1b[Jprompt status redraw\r\n{}\r\n",
            assistant_panel_top(),
            assistant_panel_bottom()
        )
        .into_bytes()
    }

    fn short_transcript(panel: &[u8]) -> Vec<u8> {
        let mut output = format!(
            "{PROMPT_READY_MARKER}\r\n{USER_TURN_SEPARATOR}\r\n{USER_TURN_MARKER} {SHORT_PROMPT_TEXT}\r\n"
        )
        .into_bytes();
        output.extend_from_slice(panel);
        output
    }

    #[test]
    fn assistant_panel_matches_terminal_redraw_and_split_escape() {
        let mut bytes = b"spinner\r\x1b[2K".to_vec();
        bytes.extend_from_slice(&cursor_moved_panel_bytes(SHORT_RESPONSE));
        let split = bytes
            .windows(2)
            .position(|window| window == b"\x1b[")
            .expect("panel contains a split-worthy CSI")
            + 2;
        let mut terminal = TerminalCapture::new();

        assert!(terminal.feed_snapshot(&bytes[..split]));
        assert_eq!(terminal.assistant_panel_count(SHORT_RESPONSE), 0);
        assert!(terminal.feed_snapshot(&bytes));
        assert_eq!(terminal.assistant_panel_count(SHORT_RESPONSE), 1);
        assert!(!terminal.feed_snapshot(&bytes));
    }

    #[test]
    fn assistant_panel_matches_ordered_interleaved_render_events() {
        let bytes = interleaved_panel_bytes(SHORT_RESPONSE);
        let final_screen = terminal_transcript(&bytes);
        assert_eq!(assistant_panel_count(&final_screen, SHORT_RESPONSE), 0);

        let observer = observe_assistant_panels(&bytes, SHORT_RESPONSE);
        assert_eq!(observer.occurrence_count(), 1);
        assert_eq!(
            observer.one_panel(),
            Some(normalized_assistant_panel(SHORT_RESPONSE).as_str())
        );
        assert_eq!(
            observer.one_panel().map(|panel| panel.lines().count()),
            Some(3)
        );
    }

    #[test]
    fn assistant_panel_rejects_echo_unframed_and_inexact_content() {
        let echoed = short_transcript(b"");
        assert_eq!(
            assistant_panel_count(&terminal_transcript(&echoed), SHORT_RESPONSE),
            0
        );

        let unframed = short_transcript(format!("{SHORT_RESPONSE}\r\n").as_bytes());
        assert_eq!(
            assistant_panel_count(&terminal_transcript(&unframed), SHORT_RESPONSE),
            0
        );
        assert!(observe_assistant_panels(&unframed, SHORT_RESPONSE)
            .one_panel()
            .is_none());
        let mut unframed_then_panel = unframed;
        unframed_then_panel.extend_from_slice(&assistant_panel_bytes(SHORT_RESPONSE));
        assert!(
            observe_assistant_panels(&unframed_then_panel, SHORT_RESPONSE)
                .one_panel()
                .is_none(),
            "an unframed exact line must invalidate a later framed response"
        );

        for response in [
            "prefix-HERMES_COMPAT_OK",
            "HERMES_COMPAT_OK-suffix",
            "HERMES_COMPAT_OTHER",
        ] {
            let panel = short_transcript(&assistant_panel_bytes(response));
            assert_eq!(
                assistant_panel_count(&terminal_transcript(&panel), SHORT_RESPONSE),
                0,
                "inexact response must not satisfy assistant-panel evidence"
            );
        }

        let mut missing_footer = assistant_panel_bytes(SHORT_RESPONSE);
        missing_footer
            .truncate(missing_footer.len() - terminal_row(&assistant_panel_bottom()).len());
        assert_eq!(
            assistant_panel_count(&terminal_transcript(&missing_footer), SHORT_RESPONSE),
            0
        );

        let mut hidden = b"\x1bP".to_vec();
        hidden.extend_from_slice(normalized_assistant_panel(SHORT_RESPONSE).as_bytes());
        hidden.extend_from_slice(b"\x1b\\");
        assert!(observe_assistant_panels(&hidden, SHORT_RESPONSE)
            .one_panel()
            .is_none());
    }

    #[test]
    fn assistant_panel_requires_exactly_one_framed_response() {
        let one = assistant_panel_bytes(SHORT_RESPONSE);
        assert_eq!(
            assistant_panel_count(&terminal_transcript(&one), SHORT_RESPONSE),
            1
        );

        let mut two = one.clone();
        two.extend_from_slice(b"\r\n");
        two.extend_from_slice(&one);
        let transcript = terminal_transcript(&two);
        assert_eq!(assistant_panel_count(&transcript, SHORT_RESPONSE), 2);
        let capture = PtyCapture {
            bytes: short_transcript(&two),
            exit_code: Some(0),
            killed: false,
        };
        assert!(validate_classic_capture("short-input", &capture).is_err());
    }

    #[test]
    fn assistant_panel_observer_rejects_duplicate_after_scrollback_eviction() {
        let panel = assistant_panel_bytes(SHORT_RESPONSE);
        let mut one = short_transcript(&panel);
        for index in 0..(PTY_SCROLLBACK_ROWS + usize::from(super::PTY_ROWS) + 8) {
            one.extend_from_slice(format!("noise-{index}\r\n").as_bytes());
        }
        let observer = observe_assistant_panels(&one, SHORT_RESPONSE);
        assert_eq!(observer.occurrence_count(), 1);
        assert!(observer.one_panel().is_some());

        let mut duplicate = one;
        duplicate.extend_from_slice(&panel);
        let observer = observe_assistant_panels(&duplicate, SHORT_RESPONSE);
        assert_eq!(observer.occurrence_count(), 2);
        let capture = PtyCapture {
            bytes: duplicate,
            exit_code: Some(0),
            killed: false,
        };
        assert!(validate_classic_capture("short-input", &capture).is_err());
    }

    #[test]
    fn assistant_panel_observer_counts_panel_after_clear_as_duplicate() {
        let panel = assistant_panel_bytes(SHORT_RESPONSE);
        let mut bytes = short_transcript(&panel);
        bytes.extend_from_slice(b"\x1b[2J\x1b[H");
        bytes.extend_from_slice(&panel);

        assert_eq!(
            observe_assistant_panels(&bytes, SHORT_RESPONSE).occurrence_count(),
            2
        );
    }

    #[test]
    fn assistant_panel_observer_ignores_prompt_status_repaint() {
        let panel = assistant_panel_bytes(SHORT_RESPONSE);
        let mut bytes = short_transcript(&panel);
        bytes.extend_from_slice(b"\r\x1b[2KWorking\r\x1b[2K\xe2\x9d\xaf ");

        assert_eq!(
            observe_assistant_panels(&bytes, SHORT_RESPONSE).occurrence_count(),
            1
        );
    }

    #[test]
    fn assistant_panel_observer_ignores_completed_footer_repaint() {
        let panel = assistant_panel_bytes(SHORT_RESPONSE);
        let mut bytes = short_transcript(&panel);
        let footer = terminal_row(&assistant_panel_bottom());
        bytes.extend_from_slice(footer.as_bytes());

        let observer = observe_assistant_panels(&bytes, SHORT_RESPONSE);

        assert_eq!(observer.occurrence_count(), 1);
        assert!(observer.one_panel().is_some());
    }

    #[test]
    fn prompt_ready_capture_uses_raw_history_after_terminal_clear() {
        let capture = PtyCapture {
            bytes: format!("{PROMPT_READY_MARKER} \r\n\x1b[2J\x1b[HGoodbye!\r\n").into_bytes(),
            exit_code: Some(0),
            killed: false,
        };

        let final_screen = terminal_transcript(&capture.bytes);
        assert!(!final_screen.contains(PROMPT_READY_MARKER));
        assert!(final_screen.contains("Goodbye!"));
        validate_classic_capture("prompt-ready", &capture)
            .expect("raw capture history retains prompt-ready evidence after terminal clear");
    }

    #[test]
    fn terminal_transcript_keeps_panel_in_bounded_scrollback() {
        let mut bytes = assistant_panel_bytes(SHORT_RESPONSE);
        for index in 0..64 {
            bytes.extend_from_slice(format!("noise-{index}\r\n").as_bytes());
        }

        let transcript = terminal_transcript(&bytes);
        assert_eq!(assistant_panel_count(&transcript, SHORT_RESPONSE), 1);
        assert!(transcript.contains("noise-63"));
    }

    #[test]
    fn terminal_transcript_preserves_classic_evidence_without_repaint_duplicates() {
        let raw = format!(
            "stale status\r\x1b[2K{PROMPT_READY_MARKER}\r\n{USER_TURN_SEPARATOR}\r\n\
             {USER_TURN_MARKER} {SHORT_PROMPT_TEXT}\r\nRunning sleep 8\r\nDangerous Command\r\n\
             Allow once\r\nDeny\r\n{INTERRUPT_MARKER}\r\nGoodbye!\r\nSession: native-reference\r\n"
        );
        let transcript = terminal_transcript(raw.as_bytes());

        assert!(!transcript.contains("stale status"));
        for expected in [
            PROMPT_READY_MARKER,
            USER_TURN_SEPARATOR,
            USER_TURN_MARKER,
            "Running sleep 8",
            "Dangerous Command",
            "Allow once",
            "Deny",
            INTERRUPT_MARKER,
            "Goodbye!",
            "Session: native-reference",
        ] {
            assert_eq!(
                transcript.matches(expected).count(),
                1,
                "terminal normalization must preserve one rendered `{expected}`"
            );
        }
    }

    #[test]
    fn terminal_command_evidence_requires_the_exact_rendered_command() {
        let transcript = "┊ 💻 preparing terminal…\n┊ 💻 sleep 8   ( 0.1s)\n";

        assert!(super::has_rendered_terminal_command(transcript, "sleep 8"));
        assert!(!super::has_rendered_terminal_command(
            transcript, "sleep 30"
        ));
        assert!(!super::has_rendered_terminal_command(
            "💻 sleep 80 ( 0.1s)",
            "sleep 8"
        ));
    }

    #[test]
    fn terminal_normalized_short_transcript_passes_classic_validation() {
        let bytes = short_transcript(&cursor_moved_panel_bytes(SHORT_RESPONSE));
        let transcript = terminal_transcript(&bytes);

        validate_classic_transcript("short-input", &transcript)
            .expect("terminal-normalized transcript retains exact classic evidence");
    }

    #[test]
    fn submitted_user_turn_allows_bounded_status_repaint_gap() {
        let text = format!(
            "{USER_TURN_SEPARATOR}\nstatus repaint\nrule repaint\n{USER_TURN_MARKER} {SHORT_PROMPT_TEXT}"
        );
        let mut distant = format!("{USER_TURN_SEPARATOR}\n");
        for _line in 0..=super::MAX_USER_TURN_BOUNDARY_GAP_LINES {
            distant.push_str("status repaint\n");
        }
        write!(&mut distant, "{USER_TURN_MARKER} {SHORT_PROMPT_TEXT}")
            .expect("write bounded submitted-turn fixture");

        assert_eq!(submitted_user_turn_count(&text), 1);
        assert!(has_one_submitted_user_turn(&text, SHORT_PROMPT_TEXT));
        assert_eq!(submitted_user_turn_count(&distant), 0);
    }

    #[test]
    fn identical_submitted_turn_repaint_is_one_semantic_turn() {
        let submitted = format!("{USER_TURN_SEPARATOR}\n{USER_TURN_MARKER} {SHORT_PROMPT_TEXT}");
        let repaint = format!("{submitted}\nrenderer output\n{submitted}");
        let different = format!(
            "{submitted}\nrenderer output\n{USER_TURN_SEPARATOR}\n{USER_TURN_MARKER} different prompt"
        );

        assert_eq!(submitted_user_turn_count(&repaint), 2);
        assert!(has_one_submitted_user_turn(&repaint, SHORT_PROMPT_TEXT));
        assert!(!has_one_submitted_user_turn(&different, SHORT_PROMPT_TEXT));
    }

    #[test]
    fn submitted_multiline_turn_skips_only_pinned_status_repaints() {
        let repaint = "⚕ pohunek-compat-v1 │ ctx --\n────────────────────────────────────────────────────────\n─\n⚕ ❯ msg=interrupt";
        let text = format!(
            "{USER_TURN_SEPARATOR}\n{USER_TURN_MARKER} Treat all three lines as one prompt.\n{repaint}\nalpha\n{repaint}\nbeta\n{repaint}\nReply with exactly HERMES_MULTILINE_OK."
        );
        let mut unbounded = format!(
            "{USER_TURN_SEPARATOR}\n{USER_TURN_MARKER} Treat all three lines as one prompt."
        );
        for _line in 0..=super::MAX_USER_TURN_BOUNDARY_GAP_LINES {
            unbounded.push_str("\nunrelated output");
        }
        unbounded.push_str("\nalpha\nbeta\nReply with exactly HERMES_MULTILINE_OK.");

        assert!(has_one_submitted_user_turn(&text, MULTILINE_PREVIEW));
        assert!(!has_one_submitted_user_turn(&unbounded, MULTILINE_PREVIEW));
    }

    #[test]
    fn terminal_transcript_rejoins_soft_wrapped_approval_prompt() {
        let raw = format!(
            "{PROMPT_READY_MARKER}\r\n{USER_TURN_SEPARATOR}\r\n\
             {USER_TURN_MARKER} {APPROVAL_PROMPT_TEXT}\r\nDangerous Command\r\n\
             rm -rf HERMES_COMPAT_APPROVAL_SENTINEL\r\nAllow once\r\nDeny\r\n"
        );
        let transcript = terminal_transcript(raw.as_bytes());

        assert!(transcript.contains(&format!("{USER_TURN_MARKER} {APPROVAL_PROMPT_TEXT}")));
        assert!(transcript.contains("HERMES_APPROVAL_DONE."));
        assert!(!transcript.contains("HERMES_A\nPPROVAL_DONE."));
        validate_classic_transcript("approval-blocked", &transcript)
            .expect("soft-wrapped repository approval prompt remains one submitted turn");
    }

    #[cfg(unix)]
    const FAKE_HERMES_TEMPLATE: &str = r#"#!/bin/sh
user_turn() {
  echo '__SEPARATOR__'
  printf '__MARKER__ %s\n' "$1"
}
assistant_panel() {
  printf '%s\n' '__PANEL_TOP__'
  printf '\033[3A\033[Jprompt status redraw\n'
  printf '%s\n' "$1"
  printf '\033[3A\033[Jprompt status redraw\n'
  printf '%s\n' '__PANEL_BOTTOM__'
}
copilot_probe() {
  python3 - <<'PY'
import os
import socket
from urllib.parse import urlsplit

proxy = urlsplit(os.environ["HTTPS_PROXY"])
for authority in ["api.github.com:443"] * 3 + ["api.githubcopilot.com:443"] * 3:
    connection = socket.create_connection((proxy.hostname, proxy.port), timeout=2)
    request = f"CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n".encode()
    connection.sendall(request)
    response = b""
    while b"\r\n\r\n" not in response:
        chunk = connection.recv(4096)
        assert chunk
        response += chunk
    assert response.startswith(b"HTTP/1.1 403 Forbidden\r\n")
    connection.close()
PY
}
mock_request() {
  HERMES_COMPAT_PROMPT="$1" HERMES_COMPAT_TOOL="$2" python3 - "$HERMES_HOME/config.yaml" <<'PY'
import json
import os
import sys
from http.client import HTTPConnection
from urllib.parse import urlsplit

config = open(sys.argv[1], encoding="utf-8").read().splitlines()
endpoint = next(line.split(":", 1)[1].strip() for line in config if line.lstrip().startswith("api:"))
parts = urlsplit(endpoint)
for detection_path in ("/api/v1/models", "/api/tags", "/v1/props", "/props", "/version"):
    connection = HTTPConnection(parts.hostname, parts.port, timeout=2)
    connection.request("GET", detection_path)
    response = connection.getresponse()
    assert response.status == 404
    response.read()
    connection.close()
tools = []
if os.environ["HERMES_COMPAT_TOOL"] == "terminal":
    tools = [{"type": "function", "function": {"name": "terminal", "parameters": {"type": "object"}}}]
body = json.dumps({"model": "pohunek-compat-v1", "stream": True, "messages": [{"role": "user", "content": os.environ["HERMES_COMPAT_PROMPT"]}], "tools": tools}).encode()
connection = HTTPConnection(parts.hostname, parts.port, timeout=2)
connection.request("POST", parts.path + "/chat/completions", body, {"Content-Type": "application/json"})
response = connection.getresponse()
assert response.status == 200
response.read()
connection.close()
PY
}
plugin_list() {
  state='not enabled'
  if [ -f "$HERMES_HOME/.pohunek-plugin-state" ]; then
    state=$(sed -n '1p' "$HERMES_HOME/.pohunek-plugin-state")
  fi
  printf '[\n  {\n    "name": "pohunek",\n    "status": "%s",\n    "version": "0.0.0-compat",\n    "description": "Model-free Pohunek compatibility fixture",\n    "source": "user"\n  }\n]\n' "$state"
}
if [ "$1" = "--profile" ]; then
  HERMES_HOME="$HERMES_HOME/profiles/$2"
  export HERMES_HOME
  shift 2
fi
case "$*" in
  --version) echo 'Hermes Agent v__VERSION__ (2026.8.3)' ;;
  --help) echo 'usage: hermes chat profile --version' ;;
  'chat --help') echo 'usage: hermes chat --resume --pass-session-id --tui --cli' ;;
  'profile --help') echo 'usage: hermes profile list create show rename' ;;
  'profile list --help') echo 'usage: hermes profile list [-h]' ;;
  'profile create --help') echo 'usage: hermes profile create [-h] [--clone-from SOURCE] profile_name' ;;
  'profile show --help') echo 'usage: hermes profile show [-h] profile_name' ;;
  'profile rename --help') echo 'usage: hermes profile rename [-h] old_name new_name' ;;
  'plugins --help') echo 'usage: hermes plugins [-h] {install,update,remove,rm,uninstall,list,ls,enable,disable} ...' ;;
  'plugins list --help') echo 'usage: hermes plugins list [-h] [--enabled] [--user] [--no-bundled] [--plain] [--json]' ;;
  'plugins enable --help') echo 'usage: hermes plugins enable [-h] [--allow-tool-override | --no-allow-tool-override] name' ;;
  'plugins disable --help') echo 'usage: hermes plugins disable [-h] name' ;;
  'plugins list --json') plugin_list ;;
  'plugins enable pohunek --no-allow-tool-override')
    echo 'enabled' > "$HERMES_HOME/.pohunek-plugin-state"
    echo 'Plugin pohunek enabled. Takes effect on next session.'
    ;;
  'plugins disable pohunek')
    echo 'disabled' > "$HERMES_HOME/.pohunek-plugin-state"
    echo 'Plugin pohunek disabled. Takes effect on next session.'
    ;;
  'chat --tui')
    copilot_probe
    trap 'exit 0' INT
    printf '\033[?1049hHermes TUI\n'
    while IFS= read -r _line; do :; done
    ;;
  chat\ --resume\ *)
    copilot_probe
    trap 'printf "\nInterrupting agent...\n❯ "' INT
    echo "↻ Resumed session $3"
    printf '❯ '
    while :; do
      if ! IFS= read -r line; then continue; fi
      case "$line" in
        /exit*) echo "Session:        $3"; exit 0 ;;
      esac
    done
    ;;
  chat)
    copilot_probe
    trap 'printf "\nInterrupting agent...\n❯ "' INT
    printf '❯ '
    had_turn=0
    in_paste=0
    split_paste=0
    wrap_approval=0
    while :; do
      if ! IFS= read -r line; then continue; fi
      case "$line" in
        *'Treat all three lines as one prompt.'*) in_paste=1; continue ;;
      esac
      if [ "$in_paste" -eq 1 ]; then
        case "$line" in
          *HERMES_MULTILINE_OK*)
            mock_request 'Treat all three lines as one prompt.
alpha
beta
Reply with exactly HERMES_MULTILINE_OK.' text
            user_turn 'Treat all three lines as one prompt.'
            echo 'alpha'
            echo 'beta'
            echo 'Reply with exactly HERMES_MULTILINE_OK.'
            assistant_panel HERMES_MULTILINE_OK
            in_paste=0
            had_turn=1
            ;;
          *)
            if [ "$split_paste" -eq 1 ]; then user_turn "$line"; fi
            ;;
        esac
        continue
      fi
      case "$line" in
        *HERMES_COMPAT_OK*)
          mock_request 'Reply with exactly HERMES_COMPAT_OK.' text
          user_turn 'Reply with exactly HERMES_COMPAT_OK.'
          assistant_panel HERMES_COMPAT_OK
          had_turn=1
          ;;
        *'sleep 8'*)
          mock_request 'Use the terminal tool to run `sleep 8`, then reply exactly HERMES_WORKING_DONE.' terminal
          user_turn 'Use the terminal tool to run `sleep 8`, then reply exactly HERMES_WORKING_DONE.'
          echo 'Running sleep 8'
          had_turn=1
          ;;
        *HERMES_COMPAT_APPROVAL_SENTINEL*)
          mock_request 'Use the terminal tool to run `rm -rf HERMES_COMPAT_APPROVAL_SENTINEL`, then reply exactly HERMES_APPROVAL_DONE.' terminal
          if [ "$wrap_approval" -eq 1 ]; then
            echo '__SEPARATOR__'
            printf '__MARKER__ %s\n' 'Use the terminal tool to run `rm -rf HERMES_COMPAT_APPROVAL_SENTINEL`, then reply exactly'
            echo 'HERMES_APPROVAL_DONE.'
          else
            user_turn 'Use the terminal tool to run `rm -rf HERMES_COMPAT_APPROVAL_SENTINEL`, then reply exactly HERMES_APPROVAL_DONE.'
          fi
          echo 'Dangerous Command'
          echo 'rm -rf HERMES_COMPAT_APPROVAL_SENTINEL'
          echo 'Allow once'
          echo 'Deny'
          had_turn=1
          ;;
        *'sleep 30'*)
          mock_request 'Use the terminal tool to run `sleep 30`, then reply exactly HERMES_INTERRUPT_DONE.' terminal
          user_turn 'Use the terminal tool to run `sleep 30`, then reply exactly HERMES_INTERRUPT_DONE.'
          echo 'Running sleep 30'
          had_turn=1
          ;;
        /exit*)
          if [ "$had_turn" -eq 0 ]; then
            echo 'Goodbye!'
          else
            echo 'Session:        20260804_120000_abcdef12'
          fi
          exit 0
          ;;
      esac
    done
    ;;
  *) exit 2 ;;
esac
"#;

    #[cfg(unix)]
    const FAKE_PLUGIN_RUNTIME_TEMPLATE: &str = r#"#!/bin/sh
if [ "$1" != "-c" ]; then
  exit 2
fi
case "$2" in
  *PluginManager*) ;;
  *) exit 2 ;;
esac
python3 - "$HERMES_HOME/plugins/operators/pohunek" <<'PY'
import importlib.util
import inspect
import json
from pathlib import Path
import sys

plugin_dir = Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("pohunek_compat", plugin_dir / "__init__.py")
if spec is None or spec.loader is None:
    raise RuntimeError("controlled plugin entrypoint could not be loaded")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class Context:
    def __init__(self):
        self.tools = []
        self.hooks = []
        self.skills = []

    def register_tool(
        self,
        name,
        toolset,
        schema,
        handler,
        check_fn=None,
        requires_env=None,
        is_async=False,
        description="",
        emoji="",
        override=False,
    ):
        if name != "pohunek_hosts" or toolset != "pohunek":
            raise RuntimeError("controlled plugin registered the wrong tool")
        if schema.get("name") != name or schema.get("parameters", {}).get("type") != "object":
            raise RuntimeError("controlled plugin registered the wrong tool schema")
        if list(inspect.signature(handler).parameters) != ["args", "kwargs"]:
            raise RuntimeError("controlled plugin handler signature drifted")
        if json.loads(handler({})) != {"ok": True}:
            raise RuntimeError("controlled plugin handler return drifted")
        self.tools.append(name)

    def register_hook(self, hook_name, callback):
        if hook_name != "pre_llm_call" or not callable(callback):
            raise RuntimeError("controlled plugin registered the wrong hook")
        self.hooks.append(hook_name)

    def register_skill(self, name, path: Path, description=""):
        if name != "pohunek" or not isinstance(path, Path) or not path.is_file():
            raise RuntimeError("controlled plugin registered the wrong skill")
        self.skills.append(f"pohunek:{name}")

context = Context()
module.register(context)
if context.tools != ["pohunek_hosts"]:
    raise RuntimeError("controlled plugin tool inventory drifted")
if context.hooks != ["pre_llm_call"]:
    raise RuntimeError("controlled plugin hook inventory drifted")
if context.skills != ["pohunek:pohunek"]:
    raise RuntimeError("controlled plugin skill inventory drifted")
print("plugin api tool pohunek_hosts")
print("plugin api hook pre_llm_call")
print("plugin api skill pohunek:pohunek")
PY
"#;

    #[cfg(unix)]
    fn fake_hermes(root: &Path, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = root.join("hermes");
        let script_content = FAKE_HERMES_TEMPLATE
            .replace("__VERSION__", version)
            .replace("__SEPARATOR__", super::USER_TURN_SEPARATOR)
            .replace("__MARKER__", super::USER_TURN_MARKER)
            .replace("__PANEL_TOP__", &super::assistant_panel_top())
            .replace("__PANEL_BOTTOM__", &super::assistant_panel_bottom());
        fs::write(&path, script_content).expect("write fake Hermes");
        let mut permissions = fs::metadata(&path).expect("fake metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("make fake executable");
        script(root, "python", FAKE_PLUGIN_RUNTIME_TEMPLATE);
        path
    }

    #[cfg(unix)]
    fn script(root: &Path, name: &str, content: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = root.join(name);
        fs::write(&path, content).expect("write controlled executable");
        let mut permissions = fs::metadata(&path)
            .expect("controlled executable metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("make controlled executable runnable");
        path
    }

    #[cfg(unix)]
    fn hermes_start_marker(root: &Path) -> (PathBuf, PathBuf) {
        let hermes = script(
            root,
            "hermes-start-marker",
            "#!/bin/sh\n: > \"$0.started\"\nexit 2\n",
        );
        let marker = hermes.with_extension("started");
        (hermes, marker)
    }

    #[cfg(unix)]
    fn fake_pohunek(root: &Path) -> PathBuf {
        script(root, "pohunek", "#!/bin/sh\nexit 2\n")
    }

    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum ControlledPohunekFailure {
        None,
        WrongStatus,
        NonzeroDoctor,
        MalformedUninstall,
    }

    #[cfg(unix)]
    fn controlled_pohunek(
        root: &Path,
        record: &Path,
        failure: ControlledPohunekFailure,
    ) -> PathBuf {
        let document = |action: &str,
                        installed: bool,
                        enabled: bool,
                        access_mode: Option<&str>,
                        doctor: Option<serde_json::Value>| {
            serde_json::json!({
                "cli_version": "controlled",
                "protocol": {"minimum": 3, "maximum": 3},
                "ok": {
                    "action": action,
                    "target_kind": "profile",
                    "target_label": "pohunek-compat",
                    "installed": installed,
                    "enabled": enabled,
                    "modified": false,
                    "stale_stage": false,
                    "stale_backup": false,
                    "access_mode": access_mode,
                    "allowed_host_count": access_mode.map(|_| 1),
                    "doctor": doctor,
                }
            })
            .to_string()
        };
        let checks: Vec<_> = (0..15)
            .map(|index| {
                serde_json::json!({
                    "code": format!("controlled_{index}"),
                    "status": "pass",
                    "recovery_hint": "none",
                })
            })
            .collect();
        let install = document("install", true, true, Some("read_only"), None);
        let status = document(
            "status",
            !matches!(failure, ControlledPohunekFailure::WrongStatus),
            !matches!(failure, ControlledPohunekFailure::WrongStatus),
            Some("read_only"),
            None,
        );
        let doctor = document(
            "doctor",
            true,
            true,
            None,
            Some(serde_json::json!({"ok": true, "checks": checks})),
        );
        let uninstall = document("uninstall", false, false, None, None);
        let doctor_case = if matches!(failure, ControlledPohunekFailure::NonzeroDoctor) {
            "exit 9".to_owned()
        } else {
            format!("printf '%s\\n' '{doctor}'")
        };
        let uninstall_case = if matches!(failure, ControlledPohunekFailure::MalformedUninstall) {
            "printf '%s\\n' '{malformed'".to_owned()
        } else {
            format!("printf '%s\\n' '{uninstall}'")
        };
        script(
            root,
            &format!(
                "pohunek-{}",
                match failure {
                    ControlledPohunekFailure::None => "ok",
                    ControlledPohunekFailure::WrongStatus => "wrong",
                    ControlledPohunekFailure::NonzeroDoctor => "nonzero",
                    ControlledPohunekFailure::MalformedUninstall => "malformed",
                }
            ),
            &format!(
                "#!/bin/sh\nprintf 'BEGIN\\n' >> '{}'\nprintf '%s\\n' \"$@\" >> '{}'\nprintf 'END\\n' >> '{}'\ncase \"$2\" in\n  install) printf '%s\\n' '{install}' ;;\n  status) printf '%s\\n' '{status}' ;;\n  doctor) {doctor_case} ;;\n  uninstall) {uninstall_case} ;;\n  *) exit 8 ;;\nesac\n",
                record.display(),
                record.display(),
                record.display()
            ),
        )
    }

    #[cfg(unix)]
    fn rewrite_script(path: &Path, from: &str, to: &str) {
        let content = fs::read_to_string(path).expect("read controlled executable");
        assert!(
            content.contains(from),
            "controlled executable rewrite must match"
        );
        fs::write(path, content.replace(from, to)).expect("rewrite controlled executable");
    }

    #[cfg(unix)]
    fn assert_process_exited(process_id: i32) {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        use std::time::Instant;

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match kill(Pid::from_raw(process_id), None) {
                Err(Errno::ESRCH) => return,
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                result => panic!("controlled descendant remained alive: {result:?}"),
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn cli_and_fixture_plugin_accept_pinned_model_free_shape() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        refresh_with(repo.path(), &binary, fast_limits()).expect("controlled refresh succeeds");

        let lock = check_cli(repo.path(), &binary, fast_limits())
            .expect("pinned CLI and fixture plugin checks succeed");
        let manifest = load_golden_manifest(repo.path()).expect("load refreshed manifest");
        validate_golden_manifest(repo.path(), &lock, &manifest, 64 * 1024, false)
            .expect("refreshed model-free evidence succeeds");
        let summary = compatibility_summary(lock, &manifest);
        assert_eq!(summary.release, "0.20.0");
        assert_eq!(summary.cli_checks, 8);
        assert_eq!(summary.plugin_checks, 17);
        assert_eq!(summary.golden_records, 10);
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_rejects_missing_sibling_python_runtime() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        fs::remove_file(repo.path().join("python")).expect("remove controlled sibling Python");

        let error = check_cli(repo.path(), &binary, fast_limits())
            .expect_err("a missing sibling Python runtime fails closed");

        assert!(error.to_string().contains("no sibling Python runtime"));
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_rejects_plugin_cli_shape_drift() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        rewrite_script(
            &binary,
            "--no-allow-tool-override",
            "--unsafe-tool-override",
        );

        let error = check_cli(repo.path(), &binary, fast_limits())
            .expect_err("plugin CLI shape drift fails closed");

        assert!(error.to_string().contains("plugins-enable-help"));
        assert!(error.to_string().contains("--no-allow-tool-override"));
    }

    #[test]
    fn compatibility_rejects_missing_plugin_api_contract() {
        let mut value: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../compat/hermes/compatibility-lock.json"
        ))
        .expect("parse checked compatibility lock as JSON");
        value["plugin_contract"]["api"]
            .as_object_mut()
            .expect("plugin API is an object")
            .remove("skill_method");

        let error = serde_json::from_value::<super::Lock>(value)
            .expect_err("a required plugin API contract field cannot be omitted");

        assert!(error.to_string().contains("skill_method"));
    }

    #[test]
    #[cfg(unix)]
    fn production_integration_temp_parent_skips_git_ancestors_and_canonicalizes_fallback() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("create temporary parent fixture");
        let safe_parent = PathBuf::from("/var/tmp");
        let safe_alias = fixture.path().join("safe-alias");
        symlink(&safe_parent, &safe_alias).expect("create safe parent alias");
        let canonical_safe_parent =
            fs::canonicalize(&safe_parent).expect("canonicalize safe temporary parent");

        for (name, marker_is_directory) in [("file", false), ("directory", true)] {
            let workspace = fixture.path().join(format!("workspace-{name}"));
            let unsafe_parent = workspace.join("nested");
            fs::create_dir_all(&unsafe_parent).expect("create nested workspace directory");
            if marker_is_directory {
                fs::create_dir(workspace.join(".git"))
                    .expect("create controlled Git directory marker");
            } else {
                fs::write(workspace.join(".git"), "gitdir: controlled\n")
                    .expect("create controlled Git file marker");
            }

            let selected =
                select_production_integration_temp_parent(vec![unsafe_parent, safe_alias.clone()])
                    .expect("select safe fallback after Git workspace candidate");

            assert_eq!(selected, canonical_safe_parent);
        }
    }

    #[test]
    fn production_integration_temp_parent_fails_closed_without_safe_candidate() {
        let fixture = tempfile::tempdir().expect("create temporary parent fixture");
        let workspace = fixture.path().join("workspace");
        let unsafe_parent = workspace.join("nested");
        fs::create_dir_all(&unsafe_parent).expect("create nested workspace directory");
        fs::create_dir(workspace.join(".git")).expect("create controlled Git marker");

        let error = select_production_integration_temp_parent(vec![unsafe_parent])
            .expect_err("Git workspace ancestry must fail closed");

        assert!(error
            .to_string()
            .contains("outside a Git workspace is available"));
    }

    #[test]
    #[cfg(unix)]
    fn integration_process_actions_are_ordered_and_exact() {
        let lock: super::Lock = serde_json::from_slice(include_bytes!(
            "../../../compat/hermes/compatibility-lock.json"
        ))
        .expect("parse checked compatibility lock");
        let isolation = tempfile::tempdir().expect("create integration isolation");
        let record = isolation.path().join("actions");
        let pohunek = controlled_pohunek(isolation.path(), &record, ControlledPohunekFailure::None);
        for step in &lock.plugin_contract.integration_lifecycle.steps {
            run_pohunek_integration_action(
                &pohunek,
                Path::new("/controlled/hermes"),
                step,
                isolation.path(),
                &[],
                fast_limits(),
            )
            .expect("controlled lifecycle action succeeds");
        }
        let pohunek_path = pohunek.to_string_lossy();
        assert_eq!(
            fs::read_to_string(record).expect("read exact action arguments"),
            format!(
                "BEGIN\nintegration\ninstall\n--agent\nhermes\n--hermes-profile\npohunek-compat\n--hermes-bin\n/controlled/hermes\n--pohunek-bin\n{pohunek_path}\n--access-mode\nread_only\n--allow-host\nlocal\n--json\nEND\n\
                 BEGIN\nintegration\nstatus\n--agent\nhermes\n--hermes-profile\npohunek-compat\n--hermes-bin\n/controlled/hermes\n--json\nEND\n\
                 BEGIN\nintegration\ndoctor\n--agent\nhermes\n--hermes-profile\npohunek-compat\n--hermes-bin\n/controlled/hermes\n--json\nEND\n\
                 BEGIN\nintegration\nuninstall\n--agent\nhermes\n--hermes-profile\npohunek-compat\n--hermes-bin\n/controlled/hermes\n--json\nEND\n"
            )
        );
    }

    #[test]
    #[cfg(unix)]
    fn integration_process_rejects_wrong_nonzero_malformed_and_missing_executables() {
        let isolation = tempfile::tempdir().expect("create integration isolation");
        let step = |action, expected_state| IntegrationStep {
            action,
            expected_state,
        };
        for (failure, action, state, expected) in [
            (
                ControlledPohunekFailure::WrongStatus,
                IntegrationAction::Status,
                IntegrationState::Installed,
                "wrong state",
            ),
            (
                ControlledPohunekFailure::NonzeroDoctor,
                IntegrationAction::Doctor,
                IntegrationState::Healthy,
                "exited unsuccessfully",
            ),
            (
                ControlledPohunekFailure::MalformedUninstall,
                IntegrationAction::Uninstall,
                IntegrationState::Absent,
                "malformed envelope",
            ),
        ] {
            let executable = controlled_pohunek(
                isolation.path(),
                &isolation.path().join("failure-actions"),
                failure,
            );
            let error = run_pohunek_integration_action(
                &executable,
                Path::new("/controlled/hermes"),
                &step(action, state),
                isolation.path(),
                &[],
                fast_limits(),
            )
            .expect_err("controlled process failure is rejected");
            assert!(error.to_string().contains(expected), "{error}");
        }
        let missing = isolation.path().join("missing-pohunek");
        let error = run_pohunek_integration_action(
            &missing,
            Path::new("/controlled/hermes"),
            &step(IntegrationAction::Install, IntegrationState::Installed),
            isolation.path(),
            &[],
            fast_limits(),
        )
        .expect_err("missing executable is rejected");
        assert!(error
            .to_string()
            .contains("failed to start Hermes CLI check"));
    }

    #[test]
    #[cfg(unix)]
    fn integration_process_uses_its_dedicated_time_limit() {
        let isolation = tempfile::tempdir().expect("create integration isolation");
        let sleeper = script(isolation.path(), "sleeping-pohunek", "#!/bin/sh\nsleep 5\n");
        let step = IntegrationStep {
            action: IntegrationAction::Install,
            expected_state: IntegrationState::Installed,
        };
        let mut limits = fast_limits();
        limits.integration_action_timeout = Duration::from_millis(50);

        let error = run_pohunek_integration_action(
            &sleeper,
            Path::new("/controlled/hermes"),
            &step,
            isolation.path(),
            &[],
            limits,
        )
        .expect_err("slow integration action fails closed");

        assert!(error.to_string().contains("time limit"));
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_requires_an_absolute_canonical_safe_pohunek_executable() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let isolation = tempfile::tempdir().expect("create executable isolation");
        let executable = script(isolation.path(), "safe-pohunek", "#!/bin/sh\nexit 0\n");
        assert_eq!(
            require_safe_pohunek_binary(&executable).expect("safe executable"),
            executable
        );
        assert!(require_safe_pohunek_binary(Path::new("relative-pohunek"))
            .expect_err("relative executable")
            .to_string()
            .contains("absolute"));
        let link = isolation.path().join("linked-pohunek");
        symlink(&executable, &link).expect("create executable symlink");
        assert!(require_safe_pohunek_binary(&link)
            .expect_err("symlink executable")
            .to_string()
            .contains("symlink"));
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o722))
            .expect("make executable unsafe");
        assert!(require_safe_pohunek_binary(&executable)
            .expect_err("writable executable")
            .to_string()
            .contains("unsafe permissions"));
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_rejects_rehashed_model_golden_with_metadata_only_panel_claim() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        refresh_with(repo.path(), &binary, fast_limits()).expect("controlled refresh succeeds");

        let golden_path = repo.path().join(GOLDEN_ROOT).join("short-input.txt");
        let original = fs::read_to_string(&golden_path).expect("read captured golden");
        let (header, body) = original
            .split_once("\n\n")
            .expect("captured golden has a transcript boundary");
        let (raw_transcript, _) = body
            .split_once(&format!("\n\n{ASSISTANT_PANEL_SECTION}\n"))
            .expect("captured model golden has derived panel evidence");
        let forged = format!("{header}\n\n{raw_transcript}\n");
        fs::write(&golden_path, &forged).expect("write forged captured golden");

        let manifest_path = repo.path().join(GOLDEN_ROOT).join(GOLDEN_MANIFEST);
        let mut manifest = load_golden_manifest(repo.path()).expect("load refreshed manifest");
        let pending = &mut manifest.records[0];
        pending.status = GoldenStatus::Pending;
        pending.file = None;
        pending.sha256 = None;
        pending.note = Some("Controlled pending record.".to_owned());
        let record = manifest
            .records
            .iter_mut()
            .find(|record| record.id == "short-input")
            .expect("short-input record exists");
        record.sha256 = Some(super::sha256(forged.as_bytes()));
        let mut rendered = serde_json::to_string_pretty(&manifest).expect("serialize manifest");
        rendered.push('\n');
        fs::write(manifest_path, rendered).expect("write rehashed manifest");

        let pohunek = fake_pohunek(repo.path());
        let error = compatibility_with(repo.path(), &binary, &pohunek, fast_limits())
            .expect_err("every captured state is validated even when another state is pending");

        assert!(error
            .to_string()
            .contains("lacks derived terminal assistant-panel evidence"));
    }

    #[test]
    #[cfg(unix)]
    fn prompt_echo_without_semantic_turn_evidence_is_rejected() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        rewrite_script(
            &binary,
            "assistant_panel HERMES_COMPAT_OK",
            "echo 'prompt echoed only'",
        );

        let error = refresh_with(repo.path(), &binary, fast_limits())
            .expect_err("prompt echo cannot satisfy response evidence");

        assert!(
            error.to_string().contains("assistant-panel evidence"),
            "unexpected prompt-echo failure: {error}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn multiline_requires_one_submitted_user_turn() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        rewrite_script(&binary, "split_paste=0", "split_paste=1");

        let error = refresh_with(repo.path(), &binary, fast_limits())
            .expect_err("embedded newlines submitted as separate turns must fail");

        assert!(
            error.to_string().contains("required semantic evidence"),
            "unexpected controlled multiline failure: {error}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn rich_wrapped_approval_preview_is_one_submitted_user_turn() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        rewrite_script(&binary, "wrap_approval=0", "wrap_approval=1");

        refresh_with(repo.path(), &binary, fast_limits())
            .expect("one approval preview wrapped at the fixed PTY width remains one turn");
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_rejects_wrong_version() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.21.0");

        let error =
            check_cli(repo.path(), &binary, fast_limits()).expect_err("wrong version fails closed");

        assert!(error.to_string().contains("missing required text"));
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_preflight_routes_proxy_egress_to_mock() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        rewrite_script(
            &binary,
            "  --version) echo 'Hermes Agent v0.20.0 (2026.8.3)' ;;",
            r#"  --version)
    python3 - <<'PY'
import os
import socket
from urllib.parse import urlsplit

proxy = urlsplit(os.environ["HTTPS_PROXY"])
connection = socket.create_connection((proxy.hostname, proxy.port), timeout=2)
connection.sendall(b"CONNECT private.example:443 HTTP/1.1\r\nHost: private.example:443\r\n\r\n")
connection.recv(4096)
connection.close()
PY
    echo 'Hermes Agent v0.20.0 (2026.8.3)'
    ;;"#,
        );

        let error = check_cli(repo.path(), &binary, fast_limits())
            .expect_err("preflight proxy egress must fail the no-request mock scenario");

        assert!(error
            .to_string()
            .contains("blocked an outbound HTTPS proxy CONNECT request"));
        assert!(!error.to_string().contains("private.example"));
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_rejects_pending_goldens() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        let manifest_path = repo.path().join(GOLDEN_ROOT).join(GOLDEN_MANIFEST);
        let mut manifest = load_golden_manifest(repo.path()).expect("load fixture manifest");
        let pending = &mut manifest.records[0];
        pending.status = GoldenStatus::Pending;
        pending.file = None;
        pending.sha256 = None;
        pending.note = Some("Controlled pending record.".to_owned());
        let mut rendered = serde_json::to_string_pretty(&manifest).expect("serialize manifest");
        rendered.push('\n');
        fs::write(manifest_path, rendered).expect("write pending manifest");

        let pohunek = fake_pohunek(repo.path());
        let error = compatibility_with(repo.path(), &binary, &pohunek, fast_limits())
            .expect_err("pending evidence fails closed");

        assert!(error.to_string().contains("is pending"));
    }

    #[test]
    fn golden_manifest_enforces_mode_and_unsupported_matrix() {
        let repo = fixture_repo();
        let lock = load_lock(repo.path()).expect("load fixture lock");
        let mut manifest = load_golden_manifest(repo.path()).expect("load fixture manifest");
        manifest.records[0].status = GoldenStatus::Unsupported;

        let unsupported = validate_golden_manifest(
            repo.path(),
            &lock,
            &manifest,
            super::MAX_PTY_OUTPUT_BYTES,
            true,
        )
        .expect_err("classic state cannot be unsupported");
        assert!(unsupported
            .to_string()
            .contains("only the Hermes alternate-screen TUI"));

        let mut manifest = load_golden_manifest(repo.path()).expect("reload fixture manifest");
        manifest.records[9].mode = "classic".to_owned();
        let mode = validate_golden_manifest(
            repo.path(),
            &lock,
            &manifest,
            super::MAX_PTY_OUTPUT_BYTES,
            true,
        )
        .expect_err("alternate-screen state requires its exact mode");
        assert!(mode.to_string().contains("invalid display mode"));
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_rejects_modified_lock() {
        let repo = fixture_repo();
        let (hermes, marker) = hermes_start_marker(repo.path());
        let lock = repo.path().join(LOCK_PATH);
        let mut bytes = fs::read(&lock).expect("read lock");
        bytes.push(b'\n');
        fs::write(lock, bytes).expect("modify lock");

        let pohunek = fake_pohunek(repo.path());
        let error = compatibility_with(repo.path(), &hermes, &pohunek, fast_limits())
            .expect_err("modified lock fails before process launch");

        assert!(error.to_string().contains("lock digest mismatch"));
        assert!(
            !marker.exists(),
            "the modified lock must be rejected before the Hermes process starts"
        );
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_bounds_cli_time_and_output() {
        let repo = fixture_repo();
        let sleeper = script(repo.path(), "sleeping-hermes", "#!/bin/sh\nsleep 5\n");
        let mut timeout_limits = fast_limits();
        timeout_limits.command_timeout = Duration::from_millis(50);

        let pohunek = fake_pohunek(repo.path());
        let timeout = compatibility_with(repo.path(), &sleeper, &pohunek, timeout_limits)
            .expect_err("slow CLI fails closed");
        assert!(timeout.to_string().contains("time limit"));

        let noisy = script(
            repo.path(),
            "noisy-hermes",
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 5000 ]; do printf x; i=$((i + 1)); done\n",
        );
        let mut output_limits = fast_limits();
        output_limits.command_output_bytes = 1024;

        let output = compatibility_with(repo.path(), &noisy, &pohunek, output_limits)
            .expect_err("noisy CLI fails closed");
        assert!(output.to_string().contains("output limit"));
    }

    #[test]
    #[cfg(unix)]
    fn process_cleanup_kills_descendant_holding_inherited_pipe() {
        let root = tempfile::tempdir().expect("create process fixture");
        let executable = script(
            root.path(),
            "pipe-holder",
            "#!/bin/sh\n/bin/sleep 30 &\necho $! > held-child.pid\nexit 0\n",
        );
        let output = run_process(
            &executable,
            &[],
            root.path(),
            &[],
            Duration::from_secs(1),
            1024,
        )
        .expect("successful parent exit cleans up its process group");
        let process_id: i32 = fs::read_to_string(root.path().join("held-child.pid"))
            .expect("read descendant pid")
            .trim()
            .parse()
            .expect("parse descendant pid");

        assert!(output.status.success());
        assert_process_exited(process_id);
    }

    #[test]
    #[cfg(unix)]
    fn pty_cleanup_kills_descendant_holding_terminal() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        let pid_file = repo.path().join("held-pty-child.pid");
        let replacement = format!(
            "  chat)\n    copilot_probe\n    /bin/sleep 30 &\n    echo $! > '{}'\n    trap",
            pid_file.display()
        );
        rewrite_script(
            &binary,
            "  chat)\n    copilot_probe\n    trap",
            &replacement,
        );

        refresh_with(repo.path(), &binary, fast_limits())
            .expect("PTY refresh cleans up descendants after every scenario");
        let process_id: i32 = fs::read_to_string(pid_file)
            .expect("read PTY descendant pid")
            .trim()
            .parse()
            .expect("parse PTY descendant pid");

        assert_process_exited(process_id);
    }

    #[test]
    #[cfg(unix)]
    fn tui_unsupported_requires_recognized_local_diagnostic() {
        let repo = fixture_repo();
        let unavailable = fake_hermes(repo.path(), "0.20.0");
        let tui_loop = "    trap 'exit 0' INT\n    printf '\\033[?1049hHermes TUI\\n'\n    while IFS= read -r _line; do :; done";
        rewrite_script(
            &unavailable,
            tui_loop,
            "    echo 'node not found \u{2014} install Node.js to use the TUI.'\n    exit 1",
        );
        let summary = refresh_with(repo.path(), &unavailable, fast_limits())
            .expect("recognized local TUI unavailability is recorded");
        assert_eq!(summary.unsupported, 1);

        let crash_repo = fixture_repo();
        let crash = fake_hermes(crash_repo.path(), "0.20.0");
        rewrite_script(
            &crash,
            tui_loop,
            "    echo 'authentication failed'\n    exit 1",
        );
        let error = refresh_with(crash_repo.path(), &crash, fast_limits())
            .expect_err("auth failure cannot be recorded as unsupported");
        assert!(error
            .to_string()
            .contains("neither alternate-screen evidence"));

        let alt_crash_repo = fixture_repo();
        let alt_crash = fake_hermes(alt_crash_repo.path(), "0.20.0");
        rewrite_script(
            &alt_crash,
            tui_loop,
            "    trap 'exit 2' INT\n    printf '\\033[?1049hHermes TUI\\n'\n    while IFS= read -r _line; do :; done",
        );
        let error = refresh_with(alt_crash_repo.path(), &alt_crash, fast_limits())
            .expect_err("alternate-screen entry does not hide a subsequent crash");
        assert!(error.to_string().contains("crashed after entering"));
    }

    #[test]
    #[cfg(unix)]
    fn refresh_writes_sanitized_hashed_inventory() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");

        let summary =
            refresh_with(repo.path(), &binary, fast_limits()).expect("controlled refresh succeeds");
        let manifest: GoldenManifest = serde_json::from_slice(
            &fs::read(summary.manifest_path).expect("read refreshed manifest"),
        )
        .expect("parse refreshed manifest");

        assert_eq!(manifest.records.len(), 10);
        assert!(manifest
            .records
            .iter()
            .all(|record| record.status != GoldenStatus::Pending));
        for record in manifest.records {
            let text = fs::read_to_string(
                repo.path()
                    .join(GOLDEN_ROOT)
                    .join(record.file.expect("refreshed file")),
            )
            .expect("read refreshed golden");
            assert!(!text.contains(repo.path().to_string_lossy().as_ref()));
            assert!(!text.contains('\u{1b}'));
            assert!(record.sha256.is_some());
        }
    }

    #[test]
    fn sanitizer_removes_terminal_sequences_paths_and_dynamic_ids() {
        let paths = vec![("/home/operator".to_owned(), "<USER_HOME>")];
        let sanitized = sanitize_output(
            b"\x1b[31m/home/operator\x1b[0m\r\nSession ID: 20260804_120000_abcdef12\n550e8400-e29b-41d4-a716-446655440000\nC:\\Users\\operator\\secret.txt",
            &paths,
        );

        assert_eq!(
            sanitized,
            "<USER_HOME>\nSession ID: <SESSION_ID>\n<UUID>\n<USER_HOME>\\secret.txt"
        );
        assert_eq!(strip_terminal_controls("a\u{1b}]0;title\u{7}b"), "ab");
        assert!(has_alternate_screen(b"before\x1b[?1049hafter"));
        assert!(!has_alternate_screen(b"classic terminal"));
    }

    #[test]
    fn sanitizer_canonicalizes_random_tips_and_status_runtime() {
        let first = sanitize_output(
            "Welcome to Hermes Agent!\n✦ Tip: /redraw forces a full UI repaint.\n\n ⚕ pohunek-compat-v1 │ ctx -- │ [░░░░░░░░░░] -- │ 1s │ ⏲ 0s".as_bytes(),
            &[],
        );
        let second = sanitize_output(
            "Welcome to Hermes Agent!\n✦ Tip: credential_pool_strategies supports fill_first, round_robin, least_used, and\nrandom rotation.\n\n ⚕ pohunek-compat-v1 │ ctx -- │ [░░░░░░░░░░] -- │ 47s │ ⏲ 22s".as_bytes(),
            &[],
        );

        assert_eq!(first, second);
        assert_eq!(
            first,
            "Welcome to Hermes Agent!\n✦ Tip: <RANDOMIZED>\n\n ⚕ pohunek-compat-v1 │ ctx -- │ [░░░░░░░░░░] -- │ <RUNTIME> │ ⏲ <RUNTIME>"
        );
    }

    #[test]
    fn sanitizer_canonicalizes_approval_runtime_countdown_and_durations() {
        let first = sanitize_output(
            " ⚕ pohunek-compat-v1 │ 0/64K │ [░░░░░░░░░░] 0% │ 5s │ ⏱ 1s\n  💻 rm -rf HERMES_COMPAT_APPROVAL_SENTINEL  (  0.3s)\n  ↑/↓ to select, Enter to confirm  (299s)\n  ┊ 💻 $         rm -rf HERMES_COMPAT_APPROVAL_SENTINEL  0.3s [BLOCKED: User denied this command.]\n ⚕ pohunek-compat-v1 │ 0/64K │ [░░░░░░░░░░] 0% │ 5s │ ⏲ 1s │ ✓ 0s\nDuration:       4s".as_bytes(),
            &[],
        );
        let second = sanitize_output(
            " ⚕ pohunek-compat-v1 │ 0/64K │ [░░░░░░░░░░] 0% │ 42s │ ⏱ 17s\n  💻 rm -rf HERMES_COMPAT_APPROVAL_SENTINEL  (  1.7s)\n  ↑/↓ to select, Enter to confirm  (283s)\n  ┊ 💻 $         rm -rf HERMES_COMPAT_APPROVAL_SENTINEL  1.7s [BLOCKED: User denied this command.]\n ⚕ pohunek-compat-v1 │ 0/64K │ [░░░░░░░░░░] 0% │ 42s │ ⏲ 17s │ ✓ 3s\nDuration:       39s".as_bytes(),
            &[],
        );

        assert_eq!(first, second);
        assert_eq!(
            first,
            " ⚕ pohunek-compat-v1 │ 0/64K │ [░░░░░░░░░░] 0% │ <RUNTIME> │ ⏱ <RUNTIME>\n  💻 rm -rf HERMES_COMPAT_APPROVAL_SENTINEL  (<DURATION>)\n  ↑/↓ to select, Enter to confirm  (<DURATION>)\n  ┊ 💻 $         rm -rf HERMES_COMPAT_APPROVAL_SENTINEL  <DURATION> [BLOCKED: User denied this command.]\n ⚕ pohunek-compat-v1 │ 0/64K │ [░░░░░░░░░░] 0% │ <RUNTIME> │ ⏲ <RUNTIME> │ ✓ <RUNTIME>\nDuration:       <DURATION>"
        );
    }

    #[test]
    fn raw_transcript_discards_ecma48_string_payloads_and_complete_escapes() {
        let hidden = format!(
            "{PROMPT_READY_MARKER}\n{USER_TURN_SEPARATOR}\n{USER_TURN_MARKER} hidden\n\
             Running sleep\nDangerous Command\nAllow once\nDeny\n{INTERRUPT_MARKER}\n\
             Goodbye!\nResumed session hidden"
        );
        for introducer in [b"\x1bP".as_slice(), b"\x1bX", b"\x1b^", b"\x1b_"] {
            let mut bytes = b"visible-before".to_vec();
            bytes.extend_from_slice(introducer);
            bytes.extend_from_slice(hidden.as_bytes());
            bytes.extend_from_slice(b"\x1b\\visible-after");

            assert_eq!(
                raw_terminal_transcript(&bytes),
                "visible-beforevisible-after"
            );
        }
        for introducer in [0x90, 0x98, 0x9e, 0x9f] {
            let mut bytes = b"visible-before".to_vec();
            bytes.push(introducer);
            bytes.extend_from_slice(hidden.as_bytes());
            bytes.push(0x9c);
            bytes.extend_from_slice(b"visible-after");

            assert_eq!(
                raw_terminal_transcript(&bytes),
                "visible-beforevisible-after"
            );
        }
        assert_eq!(
            raw_terminal_transcript(b"visible-before\x1bPunterminated hidden marker"),
            "visible-before"
        );
        assert_eq!(
            strip_terminal_controls("visible-before\x1b(Bvisible-after"),
            "visible-beforevisible-after"
        );
    }

    #[test]
    fn fixture_validation_rejects_secret_shaped_output() {
        let error = validate_safe_fixture("API_KEY=abcdefghijk")
            .expect_err("credential-shaped output fails closed");

        assert!(error.to_string().contains("credential-shaped"));
    }

    #[test]
    fn mock_verification_error_precedes_capture_error() {
        let error = finish_mocked_pty(
            Err(fail("capture did not reach prompt-ready evidence")),
            Err(fail(
                "Hermes compatibility mock received an unexpected request",
            )),
        )
        .expect_err("specific mock verification failure must not be hidden by a PTY failure");

        assert_eq!(
            error.to_string(),
            "Hermes compatibility mock received an unexpected request"
        );
    }

    #[test]
    fn every_model_turn_requires_local_discovery() {
        use crate::hermes_mock::Scenario as MockScenario;

        let scenarios = classic_scenarios(fast_limits());
        let mapped = scenarios.iter().map(mock_scenario).collect::<Vec<_>>();

        let expected = vec![
            MockScenario::no_request("prompt-ready"),
            MockScenario::text_with_local_discovery(
                "short-input",
                SHORT_PROMPT_TEXT,
                SHORT_RESPONSE,
            ),
            MockScenario::text_with_local_discovery(
                "multiline-input",
                MULTILINE_PREVIEW,
                MULTILINE_RESPONSE,
            ),
            MockScenario::terminal_with_local_discovery("working", WORKING_PROMPT_TEXT, "sleep 8"),
            MockScenario::terminal_with_local_discovery(
                "approval-blocked",
                APPROVAL_PROMPT_TEXT,
                "rm -rf HERMES_COMPAT_APPROVAL_SENTINEL",
            ),
            MockScenario::text_with_local_discovery(
                "completion",
                SHORT_PROMPT_TEXT,
                SHORT_RESPONSE,
            ),
            MockScenario::terminal_with_local_discovery(
                "interruption",
                INTERRUPTION_PROMPT_TEXT,
                "sleep 30",
            ),
            MockScenario::no_request("exit"),
        ]
        .into_iter()
        .map(MockScenario::with_copilot_probe_denials)
        .collect::<Vec<_>>();

        assert_eq!(mapped, expected);
    }

    #[test]
    fn refresh_config_is_keyless_named_custom_provider_with_static_metadata() {
        let isolation =
            Isolation::new("hermes-refresh-config-").expect("create isolated environment");
        let endpoint = "http://127.0.0.1:45231/v1";

        super::write_refresh_config(&isolation, endpoint).expect("write isolated refresh config");

        let config = fs::read_to_string(isolation.hermes_home.join("config.yaml"))
            .expect("read isolated refresh config");
        assert!(config.contains("provider: custom:pohunek-compat"));
        assert!(config.contains("api: http://127.0.0.1:45231/v1"));
        assert!(config.contains("default_model: pohunek-compat-v1"));
        assert!(config.contains("context_length: 64000"));
        assert!(config.contains("model_catalog:\n  enabled: false"));
        assert!(config.contains("discover_models: false"));
        assert!(config.contains("fallback_providers: []"));
        assert!(config.contains("  - terminal"));
        assert!(config.contains("mode: manual"));
        assert!(config.contains("security:\n  tirith_enabled: false\n  allow_lazy_installs: false"));
        assert!(config.contains("auxiliary:\n  title_generation:\n    enabled: false"));
        assert!(config.contains("telemetry:\n  shared_metrics:\n    enabled: false"));
        assert!(!config.contains("tirith_enabled: true"));
        assert!(!config.contains("allow_lazy_installs: true"));
        assert!(!config.contains("tirith_path:"));
        assert!(!config.contains("api_key"));
        assert!(!config.contains("key_env"));

        let cache: serde_json::Value = serde_json::from_slice(
            &fs::read(isolation.hermes_home.join(super::UPDATE_CACHE_FILE))
                .expect("read isolated update cache"),
        )
        .expect("parse isolated update cache");
        assert_eq!(cache["behind"], serde_json::Value::Null);
        assert_eq!(cache["ver"], "0.20.0");
        assert!(cache["ts"].as_u64().is_some());

        let models_cache: serde_json::Value = serde_json::from_slice(
            &fs::read(isolation.hermes_home.join(super::MODELS_DEV_CACHE_FILE))
                .expect("read isolated models.dev cache"),
        )
        .expect("parse isolated models.dev cache");
        let offline = &models_cache["pohunek-offline"];
        assert_eq!(offline["name"], "Pohunek offline compatibility cache");
        assert_eq!(offline["env"], serde_json::json!([]));
        assert_eq!(offline["api"], "");
        assert_eq!(offline["models"], serde_json::json!({}));
        assert_eq!(models_cache.as_object().map(serde_json::Map::len), Some(1));

        let model_catalog_cache: serde_json::Value = serde_json::from_slice(
            &fs::read(
                isolation
                    .hermes_home
                    .join(super::MODEL_CATALOG_CACHE_DIRECTORY)
                    .join(super::MODEL_CATALOG_CACHE_FILE),
            )
            .expect("read isolated model catalog cache"),
        )
        .expect("parse isolated model catalog cache");
        assert_eq!(model_catalog_cache["version"], 1);
        assert_eq!(model_catalog_cache["providers"], serde_json::json!({}));
        assert_eq!(
            model_catalog_cache.as_object().map(serde_json::Map::len),
            Some(2)
        );
        let cache_age = fs::metadata(isolation.hermes_home.join(super::MODELS_DEV_CACHE_FILE))
            .expect("stat isolated models.dev cache")
            .modified()
            .expect("models.dev cache has modification time")
            .elapsed()
            .expect("models.dev cache is not future dated");
        assert!(cache_age < Duration::from_mins(1));
    }

    #[test]
    fn isolation_seeds_model_catalog_cache_before_refresh_configuration() {
        let isolation =
            Isolation::new("hermes-model-catalog-cache-").expect("create isolated environment");

        let cache: serde_json::Value = serde_json::from_slice(
            &fs::read(
                isolation
                    .hermes_home
                    .join(super::MODEL_CATALOG_CACHE_DIRECTORY)
                    .join(super::MODEL_CATALOG_CACHE_FILE),
            )
            .expect("read pre-configuration model catalog cache"),
        )
        .expect("parse pre-configuration model catalog cache");

        assert_eq!(cache, serde_json::json!({"version": 1, "providers": {}}));
        let auth_store: serde_json::Value = serde_json::from_slice(
            &fs::read(isolation.hermes_home.join(super::AUTH_STORE_FILE))
                .expect("read pre-configuration auth store"),
        )
        .expect("parse pre-configuration auth store");
        assert_eq!(
            auth_store,
            serde_json::json!({
                "version": 1,
                "providers": {},
                "suppressed_sources": {
                    "copilot": [
                        "gh_cli",
                        "env:COPILOT_GITHUB_TOKEN",
                        "env:GH_TOKEN",
                        "env:GITHUB_TOKEN"
                    ]
                }
            })
        );
        assert!(!isolation.hermes_home.join("config.yaml").exists());
    }

    #[test]
    fn refresh_environment_contains_only_isolation_and_loopback_controls() {
        let isolation = Isolation::new("hermes-refresh-env-").expect("create isolated environment");
        let proxy_url = "http://127.0.0.1:45231";
        let environment = isolation.refresh_env(proxy_url);
        let preflight_environment = isolation.model_free_env(proxy_url);
        let names: Vec<_> = environment
            .iter()
            .map(|(name, _value)| name.to_string_lossy().into_owned())
            .collect();

        assert!(names.contains(&"HERMES_HOME".to_owned()));
        assert!(names.contains(&"HERMES_DISABLE_LAZY_INSTALLS".to_owned()));
        assert!(environment
            .iter()
            .any(|(name, value)| { name == "TIRITH_ENABLED" && value == "0" }));
        assert!(environment.iter().any(|(name, value)| {
            name == "COPILOT_GITHUB_TOKEN" && value == super::MOCK_COPILOT_CREDENTIAL
        }));
        assert_eq!(super::MOCK_COPILOT_CREDENTIAL, "pohunek-compat-local-mock");
        assert!(environment.iter().any(|(name, value)| {
            name == "DBUS_SESSION_BUS_ADDRESS"
                && value.to_string_lossy().ends_with("/no-session-bus")
        }));
        assert!(environment.iter().any(|(name, value)| {
            matches!(name.to_str(), Some("NO_PROXY" | "no_proxy")) && value == "127.0.0.1,localhost"
        }));
        assert!(!names.contains(&"TIRITH_BIN".to_owned()));
        assert!(!names.iter().any(|name| name.ends_with("_API_KEY")));
        for proxy in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            assert!(environment
                .iter()
                .any(|(name, value)| name == proxy && value == proxy_url));
        }
        assert!(!names.iter().any(|name| name == "HERMES_INFERENCE_PROVIDER"));
        assert!(!names.iter().any(|name| name == "HERMES_YOLO_MODE"));
        assert!(preflight_environment
            .iter()
            .any(|(name, value)| name == "HTTPS_PROXY" && value == proxy_url));
        assert!(preflight_environment
            .iter()
            .any(|(name, value)| name == "NO_COLOR" && value == "1"));
    }

    #[test]
    fn sanitizer_removes_ephemeral_mock_endpoint() {
        let isolation =
            Isolation::new("hermes-mock-redaction-").expect("create isolated environment");
        let endpoint = "http://127.0.0.1:45231/v1";
        let paths = sensitive_paths(
            Path::new("/repository"),
            Path::new("/usr/bin/hermes"),
            &isolation,
            endpoint,
        );

        let sanitized = sanitize_output(
            b"provider endpoint http://127.0.0.1:45231/v1/chat/completions",
            &paths,
        );

        assert_eq!(
            sanitized,
            "provider endpoint <HERMES_MOCK_ENDPOINT>/chat/completions"
        );
    }

    #[test]
    fn safe_pty_diagnostic_redacts_prompts_paths_ids_and_bounds_the_tail() {
        let paths = vec![("/private/isolation".to_owned(), "<ISOLATED_ROOT>")];
        let mut output = String::new();
        for index in 0..14 {
            writeln!(
                &mut output,
                "discardable diagnostic {index}: {}",
                "x".repeat(500)
            )
            .expect("write diagnostic fixture");
        }
        output.push_str("\u{1b}[31mReply with exactly HERMES_COMPAT_OK.\u{1b}[0m\n");
        output.push_str("state at /private/isolation/work\n");
        output.push_str("id 550e8400-e29b-41d4-a716-446655440000\n");

        let tail = sanitized_pty_tail(output.as_bytes(), &paths)
            .expect("repository-owned diagnostic is safe");

        assert!(tail.len() <= MAX_PTY_DIAGNOSTIC_BYTES);
        assert!(tail.lines().count() <= MAX_PTY_DIAGNOSTIC_LINES);
        assert!(tail.contains("<HERMES_PROMPT>"));
        assert!(tail.contains("<ISOLATED_ROOT>/work"));
        assert!(tail.contains("<UUID>"));
        assert!(!tail.contains(SHORT_PROMPT_TEXT));
        assert!(!tail.contains("/private/isolation"));
        assert!(!tail.contains('\u{1b}'));

        let error = with_pty_diagnostic(&fail("evidence timeout"), output.as_bytes(), &paths);
        assert!(error.to_string().contains("Sanitized PTY tail:"));
        assert!(!error.to_string().contains(SHORT_PROMPT_TEXT));
    }

    #[test]
    fn unsafe_pty_diagnostic_is_withheld_before_tail_selection() {
        let mut output = String::from("API_KEY=abcdefghijk\n");
        for index in 0..20 {
            writeln!(&mut output, "safe trailing line {index}").expect("write diagnostic fixture");
        }

        assert!(sanitized_pty_tail(output.as_bytes(), &[]).is_none());
        assert!(sanitized_pty_tail(
            br#"Host: localhost
{"messages":[{"role":"user","content":"private"}]}"#,
            &[],
        )
        .is_none());
        let response_dump = b"HTTP/1.1 401 Unauthorized\nSet-Cookie: session=private-cookie\n{\"error\":\"private payload\"}\nsafe trailing line\n";
        assert!(sanitized_pty_tail(response_dump, &[]).is_none());

        let error = with_pty_diagnostic(&fail("evidence timeout"), output.as_bytes(), &[]);
        assert_eq!(
            error.to_string(),
            format!("evidence timeout\n{PTY_DIAGNOSTIC_WITHHELD}")
        );
        assert!(!error.to_string().contains("abcdefghijk"));
        assert!(!error.to_string().contains("safe trailing line"));

        let response_error = with_pty_diagnostic(&fail("evidence timeout"), response_dump, &[]);
        assert_eq!(
            response_error.to_string(),
            format!("evidence timeout\n{PTY_DIAGNOSTIC_WITHHELD}")
        );
        assert!(!response_error.to_string().contains("private-cookie"));
        assert!(!response_error.to_string().contains("private payload"));
    }

    #[test]
    fn pty_diagnostic_preserves_plain_status_and_unicode_after_unknown_escape() {
        assert_eq!(
            sanitized_pty_tail(b"Status: waiting", &[]).as_deref(),
            Some("Status: waiting")
        );
        assert_eq!(
            sanitized_pty_tail(b"\x1b\xe2\x9d\xa4", &[]).as_deref(),
            Some("❤")
        );
    }
}
