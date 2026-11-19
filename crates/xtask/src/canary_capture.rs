//! The Claude OSC 7501 canary `capture` command.
//!
//! `capture` starts an isolated daemon from `--release-dir`, installs the
//! official Claude runtime package, writes one host profile per scenario shape
//! and drives each selected scenario through the daemon's CLI. It writes
//! `capture.json` (the schema [`CANARY_SCHEMA`] describes) for `compare` to
//! diff and copies the daemon log beside it. A completed run exits 0 whatever
//! the records say; comparing is `compare`'s job. A driver step that runs out
//! of time, and a workdir the account has not trusted, exit 4 with the partial
//! capture written and the step named on stderr. Every other failure is a
//! usage or setup error (exit 2).
//!
//! Only root records of `app=claude-code` reach the capture, in their
//! canonical form without the app segment (`state=done`,
//! `state=blocked:kind=permission`); raw terminal bytes and `msg`, `title` or
//! `progress` tails are never written.
//!
//! Every session the run opens is stopped through the daemon before the
//! daemon stops, on every exit path: with a real `claude` the agent of a
//! session is the operator's logged-in account, so its end must not depend on
//! how the daemon's shutdown treats running workers.

// Rust guideline compliant 2026-11-14

mod records;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};

use self::records::{OutputPage, RecordScanner};
use crate::claude_canary::CANARY_SCHEMA;
use crate::release::{ANCHOR_FILE as RELEASE_ANCHOR_FILE, CATALOG_FILE, PACKAGES_DIR};
use crate::XtaskError;
use plugin_kit::isolated::{Host, Output, Root};

/// Process exit code of a run that stopped at a driver step: the step ran out
/// of time or the workdir is not trusted. The partial capture is written.
const STOPPED_EXIT_CODE: i32 = 4;

/// The `capture` outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureOutcome {
    /// Every selected scenario was captured.
    Completed,
    /// A driver step stopped the run; the partial capture is written.
    Stopped,
}

impl CaptureOutcome {
    /// The process exit code.
    #[must_use]
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Stopped => STOPPED_EXIT_CODE,
        }
    }
}

/// The inputs of `claude canary capture`.
pub(crate) struct CaptureArgs {
    pub(crate) release_dir: PathBuf,
    pub(crate) claude: PathBuf,
    pub(crate) claude_config_dir: Option<PathBuf>,
    pub(crate) workdir: PathBuf,
    pub(crate) out: PathBuf,
    pub(crate) scenarios: Vec<String>,
    /// Directory the isolation root is created in; `TMPDIR` when absent.
    pub(crate) scratch_parent: Option<PathBuf>,
    /// Bound of the record and the first notification waits the driver
    /// polls.
    pub(crate) step_timeout: Duration,
    /// Bound of the wait for the first screen of a session; the step
    /// timeout when the flag is absent. It never bounds the tested waits:
    /// a loaded host must not stop the run at an earlier screen than the
    /// step the scenario names.
    pub(crate) first_screen_timeout: Duration,
}

/// The canary scenarios, in the order of the expected table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    IdleAfterTurn,
    PermissionPrompt,
    McpElicitation,
    LoginScreen,
    RunningTurn,
}

impl Scenario {
    const ALL: [Self; 5] = [
        Self::IdleAfterTurn,
        Self::PermissionPrompt,
        Self::McpElicitation,
        Self::LoginScreen,
        Self::RunningTurn,
    ];

    /// The row name in `capture.json` and the expected table.
    const fn name(self) -> &'static str {
        match self {
            Self::IdleAfterTurn => "idle_after_turn",
            Self::PermissionPrompt => "permission_prompt",
            Self::McpElicitation => "mcp_elicitation",
            Self::LoginScreen => "login_screen",
            Self::RunningTurn => "running_turn",
        }
    }

    /// Whether the scenario runs under the account of `--claude-config-dir`.
    const fn uses_account(self) -> bool {
        !matches!(self, Self::LoginScreen)
    }

    /// The host profile the session starts with.
    const fn profile(self) -> Profile {
        match self {
            Self::IdleAfterTurn | Self::RunningTurn => Profile::Account,
            Self::PermissionPrompt => Profile::Permission,
            Self::McpElicitation => Profile::Mcp,
            Self::LoginScreen => Profile::Login,
        }
    }

    /// The text the driver submits once the prompt is ready.
    const fn prompt(self) -> Option<&'static str> {
        match self {
            Self::IdleAfterTurn => Some("Reply with the single word OK."),
            Self::PermissionPrompt => {
                Some("Run the shell command touch canary-permission with the Bash tool.")
            }
            Self::McpElicitation => Some("Call the ask tool of the canary MCP server."),
            Self::RunningTurn => Some("Write a 400 word story about a lighthouse."),
            Self::LoginScreen => None,
        }
    }
}

/// The host profiles the capture writes, one per launch shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    /// The account of `--claude-config-dir` with Claude's own settings.
    Account,
    /// The account in Claude's `default` permission mode, which asks before
    /// a Bash call.
    Permission,
    /// The account with the canary MCP server configured and its `ask` tool
    /// pre-allowed, so the call reaches the elicitation without a prompt.
    Mcp,
    /// An empty config home: Claude shows its first-run theme picker, then
    /// its login method list.
    Login,
}

impl Profile {
    const fn name(self) -> &'static str {
        match self {
            Self::Account => "canary",
            Self::Permission => "canary-permission",
            Self::Mcp => "canary-mcp",
            Self::Login => "canary-login",
        }
    }
}

/// Base runtime every profile extends.
const PROFILE_BASE: &str = "claude";

/// Claude flag selecting the permission mode, with the mode that asks before
/// every tool call no rule allows (code.claude.com/docs/en/cli-reference).
const PERMISSION_MODE_ARG: &str = "--permission-mode=default";

/// Name of the canary MCP server in `mcp.json`; Claude names its tools
/// `mcp__<server>__<tool>`.
const MCP_SERVER_NAME: &str = "canary";

/// Claude flag pre-allowing the canary server's `ask` tool.
const MCP_ALLOWED_TOOL_ARG: &str = "--allowedTools=mcp__canary__ask";

/// Claude flag that loads only the servers of `--mcp-config`, so the
/// operator's own MCP servers do not start in the canary session.
const MCP_STRICT_ARG: &str = "--strict-mcp-config";

/// The canary MCP server, written into the scratch directory: the checkout
/// is not needed at run time.
const ELICIT_SERVER: &str = include_str!("../../../compat/claude/canary/elicit_server.py");

/// File names in the scratch directory.
const ELICIT_SERVER_FILE: &str = "elicit_server.py";
const MCP_CONFIG_FILE: &str = "mcp.json";

/// Directories below the isolation root.
const SCRATCH_DIRECTORY: &str = "scratch";
const LOGIN_CONFIG_DIRECTORY: &str = "login-config";

/// Mode of the directories the capture creates below the root.
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

/// Mode of the files the capture writes below the root.
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Interpreter the canary MCP server runs with, resolved on the caller's
/// `PATH` to an absolute path: Claude starts the server with the session's
/// own environment.
const PYTHON: &str = "python3";

/// The Claude runtime package archive inside a `release test-runtime` tree.
const CLAUDE_ARCHIVE: &str = "claude.tar.zst";

/// The detection manifest inside the Claude package.
const DETECT_MANIFEST: &str = "detect.toml";

/// Rules of the Claude detection manifest the driver evaluates on the screen.
const FOLDER_TRUST_RULE: &str = "folder_trust_dialog";
const LOGIN_METHOD_RULE: &str = "login_method_list";
const THEME_PICKER_RULE: &str = "theme_picker";

/// Keys a rule the driver evaluates may carry: it reads only `region` and
/// `regex`, so a rule with any other matcher is refused instead of misread.
const EVALUATED_RULE_KEYS: [&str; 6] = [
    "id",
    "state",
    "priority",
    "region",
    "visible_blocker",
    "regex",
];

/// Error code of a `session input` whose wait for the requested activity ran
/// out: the agent took neither the activity nor the prompt in time.
const INPUT_TIMEOUT_CODE: &str = "session_input_timeout";

/// `state_source` of an activity a screen rule of the manifest decided.
const SCREEN_STATE_SOURCE: &str = "screen";

/// Pause between two polls of the daemon readers.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Longest a scenario row may take to read after its record first arrived:
/// well inside the 30 s after which the detector replaces a held record that
/// disagrees with the screen (#2138), so the row shows the record itself.
const READ_AFTER_RECORD: Duration = Duration::from_secs(10);

/// How long the login scenario waits for a record after the login screen is
/// on screen before it records `final_record: null`.
const LOGIN_RECORD_WINDOW: Duration = Duration::from_secs(10);

/// Largest output page one read asks for.
const OUTPUT_PAGE_BYTES: u32 = 64 * 1024;

/// Runs `capture` and reports whether every scenario was captured.
pub(crate) fn run(args: &CaptureArgs) -> Result<CaptureOutcome, XtaskError> {
    let scenarios = select_scenarios(&args.scenarios)?;
    check_inputs(args, &scenarios)?;
    let screen_rules = ScreenRules::load(&args.release_dir)?;
    let python = if scenarios.contains(&Scenario::McpElicitation) {
        Some(find_on_path(PYTHON)?)
    } else {
        None
    };
    prepare_out_dir(&args.out)?;

    let root = Root::create_for_sessions(&scratch_parent(args))
        .map_err(|error| XtaskError::Usage(format!("the capture `--scratch-parent`: {error}")))?;
    let result = run_in_root(args, &scenarios, &screen_rules, python.as_deref(), &root);
    let cleanup = root.remove().map_err(XtaskError::Usage);
    let outcome = result?;
    cleanup?;
    Ok(outcome)
}

/// Starts the host in `root`, drives the scenarios and stops the host. The
/// daemon log is copied to `--out` whatever the outcome.
fn run_in_root(
    args: &CaptureArgs,
    scenarios: &[Scenario],
    screen_rules: &ScreenRules,
    python: Option<&Path>,
    root: &Root,
) -> Result<CaptureOutcome, XtaskError> {
    let anchor = args.release_dir.join(RELEASE_ANCHOR_FILE);
    let channel = args.release_dir.join("runtime").join(CATALOG_FILE);
    let mut host =
        Host::prepare(root, &args.release_dir, &anchor, &channel).map_err(XtaskError::Usage)?;
    let outcome = match host.launch_local_only(root) {
        Ok(()) => {
            let mut driver = Driver {
                host: &mut host,
                args,
                screen_rules,
                open_sessions: Vec::new(),
            };
            let outcome = driver.capture(scenarios, python, root);
            let leftovers = driver.stop_open_sessions();
            match (outcome, leftovers) {
                (Ok(outcome), Ok(())) => Ok(outcome),
                (Ok(_), Err(error)) | (Err(error), _) => Err(error),
            }
        }
        Err(error) => Err(XtaskError::Usage(error)),
    };
    let stop = host.stop().map_err(XtaskError::Usage);
    let log = copy_daemon_log(root, &args.out);
    let outcome = outcome?;
    stop?;
    log?;
    Ok(outcome)
}

/// The directory the isolation root is created in.
fn scratch_parent(args: &CaptureArgs) -> PathBuf {
    args.scratch_parent
        .clone()
        .unwrap_or_else(std::env::temp_dir)
}

/// The requested scenarios in table order, or every scenario by default.
fn select_scenarios(requested: &[String]) -> Result<Vec<Scenario>, XtaskError> {
    for name in requested {
        if !Scenario::ALL.iter().any(|scenario| scenario.name() == name) {
            let names: Vec<&str> = Scenario::ALL
                .iter()
                .map(|scenario| scenario.name())
                .collect();
            return Err(XtaskError::Usage(format!(
                "unknown canary scenario `{name}` (one of: {})",
                names.join(", ")
            )));
        }
    }
    Ok(Scenario::ALL
        .into_iter()
        .filter(|scenario| {
            requested.is_empty() || requested.iter().any(|name| name == scenario.name())
        })
        .collect())
}

/// Refuses a relative path, an account scenario without
/// `--claude-config-dir`, and inputs that are not the directories and files
/// the driver needs.
fn check_inputs(args: &CaptureArgs, scenarios: &[Scenario]) -> Result<(), XtaskError> {
    for (flag, path) in [
        ("--claude", Some(&args.claude)),
        ("--release-dir", Some(&args.release_dir)),
        ("--workdir", Some(&args.workdir)),
        ("--out", Some(&args.out)),
        ("--claude-config-dir", args.claude_config_dir.as_ref()),
        ("--scratch-parent", args.scratch_parent.as_ref()),
    ] {
        if path.is_some_and(|path| path.is_relative()) {
            return Err(XtaskError::Usage(format!(
                "the capture `{flag}` must be an absolute path"
            )));
        }
    }
    Root::check_session_parent(&scratch_parent(args))
        .map_err(|error| XtaskError::Usage(format!("the capture `--scratch-parent`: {error}")))?;
    if args.step_timeout.is_zero() {
        return Err(XtaskError::Usage(
            "the capture `--step-timeout-secs` must be positive".to_owned(),
        ));
    }
    if args.first_screen_timeout.is_zero() {
        return Err(XtaskError::Usage(
            "the capture `--first-screen-timeout-secs` must be positive".to_owned(),
        ));
    }
    if args.claude_config_dir.is_none() && scenarios.iter().any(|scenario| scenario.uses_account())
    {
        return Err(XtaskError::Usage(
            "an account scenario needs `--claude-config-dir`, the CLAUDE_CONFIG_DIR of the account it runs as; without it select `--scenario login_screen`".to_owned(),
        ));
    }
    if !fs::metadata(&args.claude).is_ok_and(|metadata| metadata.is_file()) {
        return Err(XtaskError::Usage(format!(
            "the capture `--claude` `{}` is not a file",
            args.claude.display()
        )));
    }
    require_directory("--workdir", &args.workdir)?;
    if !args.workdir.join(".git").exists() {
        return Err(XtaskError::Usage(format!(
            "the capture `--workdir` `{}` is not a git directory",
            args.workdir.display()
        )));
    }
    if let Some(config) = &args.claude_config_dir {
        require_directory("--claude-config-dir", config)?;
    }
    require_directory("--release-dir", &args.release_dir)?;
    let runtime = args.release_dir.join("runtime");
    for path in [
        args.release_dir.join(RELEASE_ANCHOR_FILE),
        runtime.join(CATALOG_FILE),
        runtime.join(PACKAGES_DIR).join(CLAUDE_ARCHIVE),
    ] {
        if fs::symlink_metadata(&path).is_err() {
            return Err(XtaskError::Usage(format!(
                "the capture `--release-dir` `{}` is not a `release test-runtime --runtime claude` tree (missing `{}`)",
                args.release_dir.display(),
                path.strip_prefix(&args.release_dir).unwrap_or(&path).display()
            )));
        }
    }
    Ok(())
}

fn require_directory(flag: &str, path: &Path) -> Result<(), XtaskError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(XtaskError::Usage(format!(
            "the capture `{flag}` `{}` is not a directory",
            path.display()
        ))),
        Err(error) => Err(XtaskError::Usage(format!(
            "cannot read the capture `{flag}` `{}`: {error}",
            path.display()
        ))),
    }
}

/// The absolute path of `program` on the caller's `PATH`.
fn find_on_path(program: &str) -> Result<PathBuf, XtaskError> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(program))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| {
            XtaskError::Usage(format!(
                "the `mcp_elicitation` scenario runs its MCP server with `{program}`, which is not on PATH"
            ))
        })
}

/// Creates the output directory: it must not exist or must be empty, so a
/// capture never mixes with an older one.
fn prepare_out_dir(out: &Path) -> Result<(), XtaskError> {
    match fs::symlink_metadata(out) {
        Err(_) => fs::create_dir_all(out).map_err(|error| {
            XtaskError::Usage(format!("cannot create `{}`: {error}", out.display()))
        }),
        Ok(metadata) if !metadata.is_dir() => Err(XtaskError::Usage(format!(
            "the capture `--out` `{}` is not a directory",
            out.display()
        ))),
        Ok(_) => {
            let mut entries = fs::read_dir(out).map_err(|error| {
                XtaskError::Usage(format!("cannot read `{}`: {error}", out.display()))
            })?;
            if entries.next().is_some() {
                return Err(XtaskError::Usage(format!(
                    "the capture `--out` `{}` is not empty",
                    out.display()
                )));
            }
            Ok(())
        }
    }
}

/// One screen rule of the Claude detection manifest: every pattern must match
/// the text of its region.
struct ScreenRule {
    id: &'static str,
    region: String,
    patterns: Vec<Regex>,
}

impl ScreenRule {
    /// Whether the rule matches the `session detection` previews.
    fn matches(&self, detection: &Value) -> bool {
        detection
            .get("previews")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|preview| preview.get("region").and_then(Value::as_str) == Some(&self.region))
            .filter_map(|preview| preview.get("text").and_then(Value::as_str))
            .any(|text| self.patterns.iter().all(|pattern| pattern.is_match(text)))
    }
}

/// The folder-trust and login screen rules of the package under test.
struct ScreenRules {
    folder_trust: ScreenRule,
    login_method: ScreenRule,
    theme_picker: ScreenRule,
}

impl ScreenRules {
    /// Reads the rules from the Claude package archive of `release_dir`, the
    /// package the run installs.
    fn load(release_dir: &Path) -> Result<Self, XtaskError> {
        let archive_path = release_dir
            .join("runtime")
            .join(PACKAGES_DIR)
            .join(CLAUDE_ARCHIVE);
        let bytes = fs::read(&archive_path).map_err(|error| {
            XtaskError::Usage(format!("cannot read `{}`: {error}", archive_path.display()))
        })?;
        let archive =
            package::read_archive(&bytes, &package::Limits::DEFAULT).map_err(|error| {
                XtaskError::Usage(format!("cannot read `{}`: {error}", archive_path.display()))
            })?;
        let manifest = archive
            .entries()
            .iter()
            .find(|entry| entry.path == DETECT_MANIFEST)
            .ok_or_else(|| {
                XtaskError::Usage(format!("the Claude package has no `{DETECT_MANIFEST}`"))
            })?;
        let manifest: toml::Table = std::str::from_utf8(&manifest.contents)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| {
                XtaskError::Usage(format!(
                    "the Claude package's `{DETECT_MANIFEST}` is not TOML"
                ))
            })?;
        Ok(Self {
            folder_trust: screen_rule(&manifest, FOLDER_TRUST_RULE)?,
            login_method: screen_rule(&manifest, LOGIN_METHOD_RULE)?,
            theme_picker: screen_rule(&manifest, THEME_PICKER_RULE)?,
        })
    }
}

fn screen_rule(manifest: &toml::Table, id: &'static str) -> Result<ScreenRule, XtaskError> {
    let refuse = |detail: &str| {
        XtaskError::Usage(format!(
            "the Claude package's `{DETECT_MANIFEST}` rule `{id}` {detail}"
        ))
    };
    let rule = manifest
        .get("rules")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .find(|rule| rule.get("id").and_then(toml::Value::as_str) == Some(id))
        .ok_or_else(|| refuse("is missing"))?;
    if let Some(key) = rule
        .keys()
        .find(|key| !EVALUATED_RULE_KEYS.contains(&key.as_str()))
    {
        return Err(refuse(&format!(
            "uses `{key}`, a matcher the capture does not evaluate"
        )));
    }
    let region = rule
        .get("region")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| refuse("has no region"))?
        .to_owned();
    let patterns = rule
        .get("regex")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| refuse("has no regex"))?
        .iter()
        .map(|pattern| {
            pattern
                .as_str()
                .and_then(|pattern| Regex::new(pattern).ok())
                .ok_or_else(|| refuse("has a pattern that is not a regex"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if patterns.is_empty() {
        return Err(refuse("has no regex"));
    }
    Ok(ScreenRule {
        id,
        region,
        patterns,
    })
}

/// What one scenario row of `capture.json` carries.
#[derive(Debug, Default)]
struct ScenarioRow {
    records: Vec<String>,
    final_record: Option<String>,
    activity: Option<String>,
    attention: Vec<String>,
}

/// Why a scenario ended without its row.
#[derive(Debug)]
enum StepFailure {
    /// A usage or setup error: the run ends with exit 2.
    Refused(String),
    /// A driver step ran out of time or met a screen the capture never
    /// answers: the partial capture is written and the run exits 4.
    Stopped(String),
}

impl From<XtaskError> for StepFailure {
    fn from(error: XtaskError) -> Self {
        match error {
            XtaskError::Usage(message) => Self::Refused(message),
            error => Self::Refused(error.to_string()),
        }
    }
}

/// What the first screen of a session shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstScreen {
    /// The prompt is ready for input.
    Prompt,
    /// The login method list.
    Login,
    /// The first-run theme picker, which comes before the login method list
    /// in an empty config home.
    Theme,
}

/// The output cursor of one session's runtime: the identity every offset is
/// scoped to, the next offset and the scanner holding a partial record.
struct OutputCursor {
    position: Option<(String, String, u64)>,
    scanner: RecordScanner,
}

/// One open scenario session and the records read from it so far.
struct Session {
    id: String,
    scenario: Scenario,
    cursor: OutputCursor,
    records: Vec<String>,
}

/// Drives the scenarios on a running host.
struct Driver<'a> {
    host: &'a mut Host,
    args: &'a CaptureArgs,
    screen_rules: &'a ScreenRules,
    /// Sessions started and not yet stopped.
    open_sessions: Vec<String>,
}

impl Driver<'_> {
    /// Installs the package, writes the profiles and captures every scenario;
    /// writes `capture.json` with the rows captured, complete or partial.
    fn capture(
        &mut self,
        scenarios: &[Scenario],
        python: Option<&Path>,
        root: &Root,
    ) -> Result<CaptureOutcome, XtaskError> {
        self.install_claude()?;
        let account_env = self.write_profiles(scenarios, python, root)?;
        let versions = self.read_versions(&account_env)?;
        let capture_path = self.args.out.join("capture.json");
        let mut rows = BTreeMap::new();
        for scenario in scenarios {
            let result = self.capture_scenario(*scenario);
            match result {
                Ok(row) => {
                    rows.insert(scenario.name(), row);
                }
                Err(StepFailure::Stopped(message)) => {
                    write_capture(&capture_path, &versions, &rows)?;
                    eprintln!(
                        "claude canary capture: scenario `{}` stopped: {message}; the partial capture is written",
                        scenario.name()
                    );
                    return Ok(CaptureOutcome::Stopped);
                }
                Err(StepFailure::Refused(message)) => {
                    write_capture(&capture_path, &versions, &rows)?;
                    return Err(XtaskError::Usage(format!(
                        "scenario `{}`: {message}",
                        scenario.name()
                    )));
                }
            }
        }
        write_capture(&capture_path, &versions, &rows)?;
        Ok(CaptureOutcome::Completed)
    }

    /// Installs the official Claude runtime package from the test-runtime
    /// tree through its signed catalog.
    fn install_claude(&mut self) -> Result<(), XtaskError> {
        let runtime = self.args.release_dir.join("runtime");
        let archive = runtime.join(PACKAGES_DIR).join(CLAUDE_ARCHIVE);
        let catalog = runtime.join(CATALOG_FILE);
        self.cli_json(
            &[
                "plugin",
                "install",
                &archive.to_string_lossy(),
                "--catalog",
                &catalog.to_string_lossy(),
                "--yes",
                "--json",
            ],
            "install the Claude runtime package",
        )
        .map(|_| ())
    }

    /// Writes the profiles the selected scenarios use into the isolated
    /// config home and pins each with `plugin profile migrate`. Returns the
    /// environment of the account profile (the login profile's when no
    /// account scenario runs), which the version read uses.
    fn write_profiles(
        &mut self,
        scenarios: &[Scenario],
        python: Option<&Path>,
        root: &Root,
    ) -> Result<Vec<(OsString, OsString)>, XtaskError> {
        let agents = root.config_home().join("pohunek").join("agents");
        private_directory(&agents, true)?;
        let scratch = root.path().join(SCRATCH_DIRECTORY);
        private_directory(&scratch, false)?;
        let login_config = root.path().join(LOGIN_CONFIG_DIRECTORY);
        private_directory(&login_config, false)?;
        let claude = absolute_string(&self.args.claude, "--claude")?;
        let account_config = match &self.args.claude_config_dir {
            Some(dir) => Some(absolute_string(dir, "--claude-config-dir")?),
            None => None,
        };
        let mut profiles: Vec<Profile> = Vec::new();
        for profile in scenarios.iter().map(|scenario| scenario.profile()) {
            if !profiles.contains(&profile) {
                profiles.push(profile);
            }
        }
        let mut version_env = None;
        for profile in profiles {
            let config = match (profile, &account_config) {
                (Profile::Login, _) => absolute_string(&login_config, "the login config")?,
                (_, Some(config)) => config.clone(),
                (_, None) => {
                    return Err(XtaskError::Usage(
                        "an account scenario needs `--claude-config-dir`".to_owned(),
                    ))
                }
            };
            let args = match profile {
                Profile::Account | Profile::Login => Vec::new(),
                Profile::Permission => vec![PERMISSION_MODE_ARG.to_owned()],
                Profile::Mcp => {
                    let python = python.ok_or_else(|| {
                        XtaskError::Usage("the MCP server needs python3".to_owned())
                    })?;
                    let config_path = write_mcp_config(&scratch, python)?;
                    vec![
                        format!(
                            "--mcp-config={}",
                            absolute_string(&config_path, "the MCP config")?
                        ),
                        MCP_ALLOWED_TOOL_ARG.to_owned(),
                        MCP_STRICT_ARG.to_owned(),
                    ]
                }
            };
            write_profile(&agents, profile.name(), &claude, &args, &config)?;
            self.cli_json(
                &[
                    "plugin",
                    "profile",
                    "migrate",
                    profile.name(),
                    "--yes",
                    "--json",
                ],
                &format!("pin the `{}` profile", profile.name()),
            )?;
            if version_env.is_none() || profile != Profile::Login {
                version_env = Some(vec![(
                    OsString::from("CLAUDE_CONFIG_DIR"),
                    OsString::from(config),
                )]);
            }
        }
        version_env.ok_or_else(|| XtaskError::Usage("no canary scenario is selected".to_owned()))
    }

    /// Reads the `claude` banner (run in the isolated environment with the
    /// profile's environment) and the `pohunek` version of the release.
    fn read_versions(
        &mut self,
        profile_env: &[(OsString, OsString)],
    ) -> Result<(String, String), XtaskError> {
        let claude = self
            .host
            .run_external(&self.args.claude, &["--version"], profile_env)
            .map_err(XtaskError::Usage)?;
        let claude = first_line(&claude, "`claude --version`")?;
        let pohunek = self.host.cli(&["--version"]).map_err(XtaskError::Usage)?;
        let pohunek = first_line(&pohunek, "`pohunek --version`")?;
        Ok((claude, pohunek))
    }

    /// Captures one scenario and stops its session on every path.
    fn capture_scenario(&mut self, scenario: Scenario) -> Result<ScenarioRow, StepFailure> {
        if scenario.uses_account() {
            eprintln!(
                "claude canary capture: scenario `{}` runs as the account of `--claude-config-dir` and may spend its quota",
                scenario.name()
            );
        }
        let mut session = self.open_session(scenario)?;
        let result = self.drive(&mut session);
        let stopped = self.stop_session(&session.id);
        let row = result?;
        stopped?;
        Ok(row)
    }

    fn open_session(&mut self, scenario: Scenario) -> Result<Session, StepFailure> {
        let workdir = self.args.workdir.to_str().ok_or_else(|| {
            StepFailure::Refused("the capture `--workdir` is not UTF-8".to_owned())
        })?;
        let created = self.cli_json(
            &[
                "session",
                "new",
                "--agent",
                scenario.profile().name(),
                "--cwd",
                workdir,
                "--json",
            ],
            &format!("start the `{}` session", scenario.name()),
        )?;
        let id = created
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| StepFailure::Refused("`session new` returned no session id".to_owned()))?
            .to_owned();
        self.open_sessions.push(id.clone());
        Ok(Session {
            id,
            scenario,
            cursor: OutputCursor {
                position: None,
                scanner: RecordScanner::default(),
            },
            records: Vec::new(),
        })
    }

    /// Drives the scenario's steps up to its read point and reads the row.
    fn drive(&mut self, session: &mut Session) -> Result<ScenarioRow, StepFailure> {
        let scenario = session.scenario;
        // The records that precede the theme picker the key answered; the
        // login row's `final_record` comes from the records after them.
        let mut picker_records = 0usize;
        let mut first = self.wait_first_screen(session, scenario, false, picker_records)?;
        if (scenario, first) == (Scenario::LoginScreen, FirstScreen::Theme) {
            picker_records = self.accept_theme(session)?;
            first = self.wait_first_screen(session, scenario, true, picker_records)?;
        }
        match (scenario, first) {
            (Scenario::LoginScreen, FirstScreen::Login) => {
                return self.read_login(session, picker_records)
            }
            (_, FirstScreen::Theme) => {
                return Err(StepFailure::Refused(
                    "Claude shows its first-run theme picker: finish Claude's first run for the account of `--claude-config-dir` first".to_owned(),
                ))
            }
            (Scenario::LoginScreen, FirstScreen::Prompt) => {
                return Err(StepFailure::Refused(
                    "Claude opened its prompt without a login in an empty config home".to_owned(),
                ))
            }
            (_, FirstScreen::Login) => {
                return Err(StepFailure::Refused(
                    "Claude shows its login method list: log in to the account of `--claude-config-dir` first".to_owned(),
                ))
            }
            (_, FirstScreen::Prompt) => {}
        }
        // Only records the prompt caused count from here on.
        let baseline = session.records.len();
        self.require_no_trust_dialog(session)?;
        let prompt = scenario.prompt().unwrap_or_default();
        // The CLI's own wait for `working` is bounded by the protocol's
        // longest session wait; the record wait below has its own deadline.
        let input_wait_ms = protocol::MAX_SESSION_WAIT_MS.to_string();
        let mut input = vec!["session", "input", session.id.as_str(), prompt, "--json"];
        if matches!(scenario, Scenario::IdleAfterTurn | Scenario::RunningTurn) {
            input.extend(["--until", "working", "--timeout", input_wait_ms.as_str()]);
        }
        let what = format!("send the `{}` prompt", scenario.name());
        let (code, document) = self.cli_document(&input, &what)?;
        if code != Some(0) {
            if document["err"]["code"] == INPUT_TIMEOUT_CODE {
                return Err(StepFailure::Stopped(format!(
                    "step `{what}` timed out: the agent took neither the prompt nor the `working` activity within {input_wait_ms} ms"
                )));
            }
            return Err(StepFailure::Refused(format!(
                "cannot {what}: {}",
                document["err"]
            )));
        }
        let read_point = match scenario {
            Scenario::IdleAfterTurn => self.wait_for_records(
                session,
                baseline,
                "a `working` record and then `done` or `idle`",
                |records| {
                    records
                        .iter()
                        .position(|record| record == "state=working")
                        .is_some_and(|working| {
                            records[working..]
                                .iter()
                                .any(|record| record == "state=done" || record == "state=idle")
                        })
                },
            )?,
            Scenario::PermissionPrompt | Scenario::McpElicitation => {
                self.wait_for_records(session, baseline, "a `blocked` record", |records| {
                    records
                        .iter()
                        .any(|record| record.starts_with("state=blocked"))
                })?
            }
            Scenario::RunningTurn => {
                self.wait_for_records(session, baseline, "the first `working` record", |records| {
                    records.iter().any(|record| record == "state=working")
                })?
            }
            Scenario::LoginScreen => unreachable!("the login scenario returned above"),
        };
        let wait_for_attention = matches!(
            scenario,
            Scenario::PermissionPrompt | Scenario::McpElicitation
        );
        self.read_row(session, read_point, wait_for_attention)
    }

    /// Waits until the first screen shows the prompt or the login method list.
    /// The folder-trust dialog stops the run: the capture never answers it.
    fn wait_first_screen(
        &mut self,
        session: &mut Session,
        scenario: Scenario,
        theme_answered: bool,
        picker_records: usize,
    ) -> Result<FirstScreen, StepFailure> {
        let deadline = Instant::now() + self.args.first_screen_timeout;
        loop {
            self.read_records(session)?;
            let activity = self.activity(session)?;
            let detection = self.detection(session)?;
            if self.screen_rules.folder_trust.matches(&detection) {
                return Err(self.untrusted());
            }
            if self.screen_rules.login_method.matches(&detection) {
                return Ok(FirstScreen::Login);
            }
            // After the answer the picker stays until Claude redraws; the
            // wait goes on for the screen that follows it. The login scenario
            // sends its key once the session awaits owner action, the state in
            // which only an attach may answer the dialog; every other scenario
            // refuses the picker as soon as it shows.
            if !theme_answered
                && self.screen_rules.theme_picker.matches(&detection)
                && (scenario != Scenario::LoginScreen || activity.as_deref() == Some("blocked"))
            {
                return Ok(FirstScreen::Theme);
            }
            // A startup `state=idle` can precede first-run screens, so in the login
            // scenario an idle record counts only after the picker; while Claude holds
            // the idle record the detector reports `osc_progress` over the prompt box,
            // so the step deadline decides "straight to the prompt".
            let last_record_idle = session
                .records
                .last()
                .is_some_and(|record| record == "state=idle");
            let screen_idle = activity.as_deref() == Some("idle")
                && detection["status"]["state_source"] == SCREEN_STATE_SOURCE;
            if scenario == Scenario::LoginScreen {
                if theme_answered {
                    let idle_record = session.records.len() > picker_records && last_record_idle;
                    if idle_record || screen_idle {
                        return Ok(FirstScreen::Prompt);
                    }
                } else if screen_idle {
                    return Ok(FirstScreen::Prompt);
                }
            } else {
                let idle_record = last_record_idle;
                if idle_record || screen_idle {
                    return Ok(FirstScreen::Prompt);
                }
            }
            if Instant::now() >= deadline {
                if scenario == Scenario::LoginScreen
                    && !theme_answered
                    && last_record_idle
                    && activity.as_deref() != Some("blocked")
                {
                    return Ok(FirstScreen::Prompt);
                }
                return Err(self.timed_out_first_screen());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Answers the first-run theme picker of an empty config home with its
    /// default, the one input the login scenario sends. The picker emits no
    /// program-status record (the 7501 contract has none for first-run
    /// dialogs), so the row's read point stays the login method list. The
    /// key is written through `pohunek attach` because the daemon refuses
    /// `session input` while the dialog blocks the session. The screen is
    /// checked once more right before the key: Enter on the folder-trust
    /// dialog would trust the directory.
    ///
    /// Returns how many records precede the picker the key answered. The
    /// worker retains output bytes before its screen shows them and the
    /// daemon's detection follows that screen, so a full read after the
    /// detection that matched the picker holds every record written before
    /// the picker was drawn.
    fn accept_theme(&mut self, session: &mut Session) -> Result<usize, StepFailure> {
        let detection = self.detection(session)?;
        if self.screen_rules.folder_trust.matches(&detection) {
            return Err(self.untrusted());
        }
        if !self.screen_rules.theme_picker.matches(&detection) {
            return Err(StepFailure::Refused(format!(
                "the screen no longer shows the theme picker (`{}`) the key was meant for",
                self.screen_rules.theme_picker.id
            )));
        }
        self.read_records(session)?;
        let picker_records = session.records.len();
        // `session.input` is refused while the dialog blocks the session; the
        // owner path is an attach whose standard input carries the key and then
        // ends, which detaches again.
        let output = self
            .host
            .cli_with_input(&["attach", session.id.as_str()], b"\r")
            .map_err(XtaskError::Usage)?;
        if output.code == Some(0) {
            Ok(picker_records)
        } else {
            Err(StepFailure::Refused(format!(
                "cannot accept the theme picker's default: `pohunek attach` exited {}",
                output
                    .code
                    .map_or_else(|| "on a signal".to_owned(), |code| code.to_string())
            )))
        }
    }

    /// Refuses to send input while the folder-trust dialog is on screen: a
    /// submitted prompt would confirm the dialog's default choice.
    fn require_no_trust_dialog(&mut self, session: &Session) -> Result<(), StepFailure> {
        let detection = self.detection(session)?;
        if self.screen_rules.folder_trust.matches(&detection) {
            return Err(self.untrusted());
        }
        Ok(())
    }

    fn untrusted(&self) -> StepFailure {
        StepFailure::Stopped(format!(
            "Claude shows its folder-trust dialog (`{}`): trust {} in Claude for this config first",
            self.screen_rules.folder_trust.id,
            self.args.workdir.display()
        ))
    }

    fn timed_out(&self, step: &str) -> StepFailure {
        StepFailure::Stopped(format!(
            "step `{step}` timed out after {}s",
            self.args.step_timeout.as_secs()
        ))
    }

    /// The first-screen wait stops the run with its own budget named: the
    /// earlier screen is not the step the scenario tests.
    fn timed_out_first_screen(&self) -> StepFailure {
        StepFailure::Stopped(format!(
            "step `wait for the first screen` timed out after {}s",
            self.args.first_screen_timeout.as_secs()
        ))
    }

    /// Polls the output until the records after `baseline` satisfy `done`, and
    /// returns when the satisfying record was first read.
    fn wait_for_records(
        &mut self,
        session: &mut Session,
        baseline: usize,
        what: &str,
        done: impl Fn(&[String]) -> bool,
    ) -> Result<Instant, StepFailure> {
        let deadline = Instant::now() + self.args.step_timeout;
        loop {
            self.read_records(session)?;
            if done(&session.records[baseline..]) {
                return Ok(Instant::now());
            }
            if Instant::now() >= deadline {
                return Err(self.timed_out(&format!("wait for {what}")));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// The login row: `records` holds every record read so far, those read
    /// before the theme picker was answered included; `final_record` is the
    /// last record after the first `picker_records`, waited for up to
    /// [`LOGIN_RECORD_WINDOW`] from the login screen, and `null` when none
    /// arrived. The 7501 contract has no record for the picker, so a record
    /// read before it (a startup `idle`) says nothing about the login screen.
    fn read_login(
        &mut self,
        session: &mut Session,
        picker_records: usize,
    ) -> Result<ScenarioRow, StepFailure> {
        let window_end = Instant::now() + LOGIN_RECORD_WINDOW;
        while session.records.len() == picker_records && Instant::now() < window_end {
            std::thread::sleep(POLL_INTERVAL);
            self.read_records(session)?;
        }
        let mut row = self.read_row(session, Instant::now(), false)?;
        row.final_record = row
            .records
            .get(picker_records..)
            .and_then(<[String]>::last)
            .cloned();
        Ok(row)
    }

    /// Reads the row at its read point. The activity and the stale-record
    /// check depend on the held record and finish within
    /// [`READ_AFTER_RECORD`] of `read_point`; the records are those read up to
    /// `read_point`. The unread attention kinds are read with them; when
    /// `wait_for_attention` and none is there yet, the first notification is
    /// waited for up to the step bound.
    fn read_row(
        &mut self,
        session: &mut Session,
        read_point: Instant,
        wait_for_attention: bool,
    ) -> Result<ScenarioRow, StepFailure> {
        let window_end = read_point + READ_AFTER_RECORD;
        // Waiting for the first notification inside the window leaves its
        // second half to the reads that depend on the held record.
        let early_attention_end = read_point + READ_AFTER_RECORD / 2;
        let mut attention = self.attention(session)?;
        while wait_for_attention && attention.is_empty() && Instant::now() < early_attention_end {
            std::thread::sleep(POLL_INTERVAL);
            attention = self.attention(session)?;
        }
        let activity = self.activity(session)?;
        let detection = self.detection(session)?;
        if Instant::now() > window_end {
            return Err(StepFailure::Stopped(format!(
                "the activity and detection of the row were not read within {}s of its record; they took {}s",
                READ_AFTER_RECORD.as_secs(),
                read_point.elapsed().as_secs()
            )));
        }
        if let Some(fallback) = detection
            .get("status")
            .and_then(|status| status.get("fallback"))
        {
            if !fallback.is_null() {
                return Err(StepFailure::Refused(
                    "the detector replaced the record with the screen (stale-record fallback); retry the capture".to_owned(),
                ));
            }
        }
        // The notification is not a held record: a late one is waited for up
        // to the step bound.
        let attention_deadline = Instant::now() + self.args.step_timeout;
        while wait_for_attention && attention.is_empty() {
            if Instant::now() >= attention_deadline {
                return Err(self.timed_out("wait for the first notification"));
            }
            std::thread::sleep(POLL_INTERVAL);
            attention = self.attention(session)?;
        }
        Ok(ScenarioRow {
            final_record: session.records.last().cloned(),
            records: session.records.clone(),
            activity,
            attention,
        })
    }

    /// Reads every output byte available after the cursor and appends the
    /// root records it completes.
    fn read_records(&mut self, session: &mut Session) -> Result<(), StepFailure> {
        loop {
            let page_bytes = OUTPUT_PAGE_BYTES.to_string();
            let mut arguments = vec![
                "session".to_owned(),
                "output".to_owned(),
                session.id.clone(),
            ];
            if let Some((worker, generation, offset)) = &session.cursor.position {
                arguments.extend([
                    "--worker-instance-id".to_owned(),
                    worker.clone(),
                    "--runtime-generation".to_owned(),
                    generation.clone(),
                    "--after-offset".to_owned(),
                    offset.to_string(),
                ]);
            }
            arguments.extend(["--max-bytes".to_owned(), page_bytes, "--json".to_owned()]);
            let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
            let value = self.cli_json(&arguments, "read the session output")?;
            let page = OutputPage::parse(&value).map_err(StepFailure::Refused)?;
            let Some((worker, generation, _)) = &session.cursor.position else {
                // The first read carries no cursor and returns the newest tail;
                // it only supplies the runtime identity. The scan starts at the
                // first retained byte, which must be the first byte of output.
                if page.history_start_offset != 0 {
                    return Err(StepFailure::Stopped(
                        "the session's first output bytes are no longer retained (`gap`)"
                            .to_owned(),
                    ));
                }
                let (worker, generation) = page.identity;
                session.cursor.position = Some((worker, generation, 0));
                continue;
            };
            if page.identity.0 != *worker || page.identity.1 != *generation {
                return Err(StepFailure::Refused(
                    "the session's runtime restarted during the scenario".to_owned(),
                ));
            }
            if page.gap {
                return Err(StepFailure::Stopped(
                    "output the record reader had not read was evicted (`gap`)".to_owned(),
                ));
            }
            session
                .records
                .extend(session.cursor.scanner.feed(&page.bytes));
            session.cursor.position = Some((page.identity.0, page.identity.1, page.next_offset));
            if !page.has_more {
                return Ok(());
            }
        }
    }

    fn detection(&mut self, session: &Session) -> Result<Value, StepFailure> {
        self.cli_json(
            &["session", "detection", &session.id, "--json"],
            "read the session detection",
        )
        .map_err(StepFailure::from)
    }

    fn activity(&mut self, session: &Session) -> Result<Option<String>, StepFailure> {
        let inspected = self.cli_json(
            &["session", "inspect", &session.id, "--json"],
            "inspect the session",
        )?;
        Ok(inspected
            .get("activity")
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// The `kind` of every unread notification of the session, sorted.
    fn attention(&mut self, session: &Session) -> Result<Vec<String>, StepFailure> {
        let listed = self.cli_json(
            &[
                "notifications",
                "list",
                "--session",
                &session.id,
                "--unread",
                "--json",
            ],
            "list the session's unread notifications",
        )?;
        let items = listed
            .get("notifications")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                StepFailure::Refused(
                    "`notifications list` returned no `notifications` array".to_owned(),
                )
            })?;
        let mut kinds = items
            .iter()
            .map(|item| {
                item.get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| StepFailure::Refused("a notification has no `kind`".to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        kinds.sort();
        Ok(kinds)
    }

    /// Stops one session and forgets it once the daemon confirmed the stop.
    fn stop_session(&mut self, id: &str) -> Result<(), StepFailure> {
        self.cli_json(&["session", "stop", id, "--json"], "stop the session")?;
        self.open_sessions.retain(|open| open != id);
        Ok(())
    }

    /// Stops every session a failed step left open; the first failure is
    /// reported after every stop was tried.
    fn stop_open_sessions(&mut self) -> Result<(), XtaskError> {
        let mut first_error = None;
        for id in self.open_sessions.clone() {
            if let Err(StepFailure::Refused(message) | StepFailure::Stopped(message)) =
                self.stop_session(&id)
            {
                first_error.get_or_insert(message);
            }
        }
        first_error.map_or(Ok(()), |message| Err(XtaskError::Usage(message)))
    }

    /// Runs one CLI command on the isolated host and returns the `ok` member
    /// of its JSON document, or the error it names.
    fn cli_json(&mut self, arguments: &[&str], what: &str) -> Result<Value, XtaskError> {
        let (code, document) = self.cli_document(arguments, what)?;
        if code == Some(0) {
            return Ok(document["ok"].clone());
        }
        Err(XtaskError::Usage(format!(
            "cannot {what}: {}",
            document["err"]
        )))
    }

    /// Runs one CLI command on the isolated host and returns its exit code and
    /// JSON document.
    fn cli_document(
        &mut self,
        arguments: &[&str],
        what: &str,
    ) -> Result<(Option<i32>, Value), XtaskError> {
        let output = self.host.cli(arguments).map_err(XtaskError::Usage)?;
        let document: Value = serde_json::from_slice(&output.stdout).map_err(|_error| {
            XtaskError::Usage(format!("cannot {what}: the CLI printed no JSON document"))
        })?;
        Ok((output.code, document))
    }
}

/// The first non-empty stdout line of a successful command.
fn first_line(output: &Output, what: &str) -> Result<String, XtaskError> {
    if output.code != Some(0) {
        return Err(XtaskError::Usage(format!(
            "{what} exited {}",
            output
                .code
                .map_or_else(|| "on a signal".to_owned(), |code| code.to_string())
        )));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| XtaskError::Usage(format!("{what} printed nothing")))
}

/// Creates an owner-only directory.
fn private_directory(path: &Path, recursive: bool) -> Result<(), XtaskError> {
    fs::DirBuilder::new()
        .recursive(recursive)
        .mode(PRIVATE_DIRECTORY_MODE)
        .create(path)
        .map_err(|error| XtaskError::Usage(format!("cannot create `{}`: {error}", path.display())))
}

/// Writes an owner-only file.
fn private_file(path: &Path, contents: &str) -> Result<(), XtaskError> {
    fs::write(path, contents)
        .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE)))
        .map_err(|error| XtaskError::Usage(format!("cannot write `{}`: {error}", path.display())))
}

/// The canonical absolute form of an existing path, as UTF-8 text for a
/// profile or config file.
fn absolute_string(path: &Path, what: &str) -> Result<String, XtaskError> {
    let canonical = path
        .canonicalize()
        .map_err(|error| XtaskError::Usage(format!("cannot resolve {what}: {error}")))?;
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| XtaskError::Usage(format!("{what} `{}` is not UTF-8", canonical.display())))
}

/// Writes the canary MCP server and the `mcp.json` that starts it into the
/// scratch directory; returns the config path.
fn write_mcp_config(scratch: &Path, python: &Path) -> Result<PathBuf, XtaskError> {
    let server = scratch.join(ELICIT_SERVER_FILE);
    private_file(&server, ELICIT_SERVER)?;
    let config = json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "type": "stdio",
                "command": absolute_string(python, "python3")?,
                "args": [absolute_string(&server, "the MCP server")?],
            }
        }
    });
    let path = scratch.join(MCP_CONFIG_FILE);
    let text = serde_json::to_string_pretty(&config)
        .map_err(|error| XtaskError::Usage(format!("cannot render the MCP config: {error}")))?;
    private_file(&path, &format!("{text}\n"))?;
    Ok(path)
}

/// Writes one host profile extending the `claude` base.
fn write_profile(
    agents: &Path,
    name: &str,
    claude: &str,
    args: &[String],
    config_home: &str,
) -> Result<(), XtaskError> {
    let mut env = toml::Table::new();
    env.insert("CLAUDE_CONFIG_DIR".to_owned(), config_home.into());
    let mut profile = toml::Table::new();
    profile.insert("base".to_owned(), PROFILE_BASE.into());
    profile.insert("program".to_owned(), claude.into());
    if !args.is_empty() {
        profile.insert(
            "args".to_owned(),
            toml::Value::Array(args.iter().map(|arg| arg.as_str().into()).collect()),
        );
    }
    profile.insert("env".to_owned(), toml::Value::Table(env));
    let text = toml::to_string(&profile).map_err(|error| {
        XtaskError::Usage(format!("cannot render the `{name}` profile: {error}"))
    })?;
    private_file(&agents.join(format!("{name}.toml")), &text)
}

/// Writes `capture.json` with the captured rows.
fn write_capture(
    path: &Path,
    versions: &(String, String),
    rows: &BTreeMap<&'static str, ScenarioRow>,
) -> Result<(), XtaskError> {
    let (claude_version, pohunek_version) = versions;
    let scenarios: serde_json::Map<String, Value> = rows
        .iter()
        .map(|(name, row)| {
            let mut fields = serde_json::Map::new();
            fields.insert("records".to_owned(), json!(row.records));
            fields.insert("final_record".to_owned(), json!(row.final_record));
            if let Some(activity) = &row.activity {
                fields.insert("activity".to_owned(), json!(activity));
            }
            fields.insert("attention".to_owned(), json!(row.attention));
            ((*name).to_owned(), Value::Object(fields))
        })
        .collect();
    let document = json!({
        "schema": CANARY_SCHEMA,
        "runtime": "claude",
        "claude_version": claude_version,
        "pohunek_version": pohunek_version,
        "scenarios": scenarios,
    });
    let text = serde_json::to_string_pretty(&document)
        .map_err(|error| XtaskError::Usage(format!("cannot render the capture: {error}")))?;
    fs::write(path, format!("{text}\n"))
        .map_err(|error| XtaskError::Usage(format!("cannot write `{}`: {error}", path.display())))
}

/// Copies the isolated daemon log next to the capture.
fn copy_daemon_log(root: &Root, out: &Path) -> Result<(), XtaskError> {
    let source = root.daemon_log();
    if fs::symlink_metadata(&source).is_err() {
        return Ok(());
    }
    let target = out.join("daemon.log");
    fs::copy(&source, &target)
        .map(|_bytes| ())
        .map_err(|error| XtaskError::Usage(format!("cannot copy `{}`: {error}", target.display())))
}
