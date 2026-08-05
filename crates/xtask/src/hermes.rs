//! Validates the pinned Hermes CLI and refreshes bounded PTY evidence.

// Rust guideline compliant 2026-08-04

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

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
    "f53f9401ecc6a5d1ab138507f7e0805d14e196167a7a3e2eaa710e0b521b1a14";
/// The pinned source archive digest recorded by the M2 baseline review.
const EXPECTED_SOURCE_SHA256: &str =
    "1e9319c58a7f5e95808546af1091d58472be7437adc63fae0cbb53316e2711aa";
const EXPECTED_SOURCE_FORMAT: &str = "git archive --format=tar";
const EXPECTED_FRESH_ARGV: [&str; 1] = ["chat"];
const EXPECTED_RESUME_ARGV: [&str; 3] = ["chat", "--resume", "<reference>"];
const EXPECTED_DISPLAY_MODES: [&str; 2] = ["classic", "alternate_screen_tui"];
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
/// One PTY transcript remains reviewable and cannot consume unbounded memory.
const MAX_PTY_OUTPUT_BYTES: usize = 512 * 1024;
/// The fixed terminal size matches the common baseline used by PTY fixtures.
const PTY_COLS: u16 = 100;
const PTY_ROWS: u16 = 32;
/// Polling at 20 ms bounds timeout overshoot without busy-waiting.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// The classic prompt normally appears within this bounded startup window.
const STARTUP_WAIT: Duration = Duration::from_secs(8);
/// Real provider turns may be slow but golden refresh must still terminate.
const TURN_WAIT: Duration = Duration::from_secs(45);
/// Working and interruption evidence is sampled shortly after submission.
const WORKING_WAIT: Duration = Duration::from_secs(3);
/// Give input handling and terminal cleanup a short bounded grace period.
const INPUT_SETTLE_WAIT: Duration = Duration::from_secs(2);
const EXIT_GRACE: Duration = Duration::from_secs(8);
/// Reader completion is bounded even if an escaped descendant retained a pipe.
const READER_GRACE: Duration = Duration::from_secs(2);

const SHORT_PROMPT: &[u8] = b"Reply with exactly HERMES_COMPAT_OK.";
const MULTILINE_PROMPT: &[u8] =
    b"\x1b[200~Treat all three lines as one prompt.\nalpha\nbeta\nReply with exactly HERMES_MULTILINE_OK.\x1b[201~";
const MULTILINE_PREVIEW: &str =
    "Treat all three lines as one prompt.\nalpha\nbeta\nReply with exactly HERMES_MULTILINE_OK.";
const WORKING_PROMPT: &[u8] =
    b"Use the terminal tool to run `sleep 8`, then reply exactly HERMES_WORKING_DONE.";
const APPROVAL_PROMPT: &[u8] = b"Use the terminal tool to run `rm -f HERMES_COMPAT_APPROVAL_SENTINEL`, then reply exactly HERMES_APPROVAL_DONE.";
const INTERRUPTION_PROMPT: &[u8] =
    b"Use the terminal tool to run `sleep 30`, then reply exactly HERMES_INTERRUPT_DONE.";
const SUBMIT: &[u8] = b"\r";
const EXIT_COMMAND: &[u8] = b"/exit\r";
const INTERRUPT: &[u8] = b"\x03";
const PROMPT_READY_MARKER: &str = "\u{276f}";
/// Pinned Hermes prints this separator immediately before each submitted user turn.
const USER_TURN_SEPARATOR: &str = "────────────────────────────────────────";
/// Pinned Hermes prefixes the first line of a submitted user turn with this glyph.
const USER_TURN_MARKER: &str = "\u{25cf}";
/// Rich wrapping at the fixed PTY width needs at most a few continuation lines.
const MAX_USER_TURN_PREVIEW_LINES: usize = 8;
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
}

impl Isolation {
    fn new(prefix: &str) -> Result<Self, XtaskError> {
        let temp = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .map_err(|error| fail(format!("failed to create isolated Hermes root: {error}")))?;
        let root = temp.path().to_path_buf();
        let home = root.join("home");
        let hermes_home = root.join("hermes-home");
        let work = root.join("work");
        for path in [
            &home,
            &hermes_home,
            &work,
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
        Ok(Self {
            _temp: temp,
            root,
            home,
            hermes_home,
            work,
        })
    }

    fn model_free_env(&self) -> Vec<(OsString, OsString)> {
        let mut env = self.isolation_env();
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
}

#[derive(Debug)]
struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct ProviderEnv {
    name: String,
    value: String,
}

impl fmt::Debug for ProviderEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderEnv")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
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
    ExactLine(&'static str),
    Working,
    Approval,
    Interrupted,
    Resumed(String),
    AlternateScreen,
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
) -> Result<CompatibilitySummary, XtaskError> {
    compatibility_with(repo, hermes_bin, Limits::production())
}

/// Refreshes sanitized real-Hermes PTY goldens.
pub(crate) fn refresh_goldens(
    repo: &Path,
    hermes_bin: &Path,
    provider_env: &[String],
) -> Result<RefreshSummary, XtaskError> {
    let provider_env = resolve_provider_env(provider_env)?;
    refresh_with(repo, hermes_bin, &provider_env, Limits::production())
}

fn compatibility_with(
    repo: &Path,
    hermes_bin: &Path,
    limits: Limits,
) -> Result<CompatibilitySummary, XtaskError> {
    let lock = check_cli(repo, hermes_bin, limits)?;
    let manifest = load_golden_manifest(repo)?;
    validate_golden_manifest(repo, &lock, &manifest, limits.pty_output_bytes, false)?;

    Ok(CompatibilitySummary {
        release: lock.release,
        tag: lock.tag,
        cli_checks: lock.cli_checks.len(),
        golden_records: manifest.records.len(),
    })
}

fn check_cli(repo: &Path, hermes_bin: &Path, limits: Limits) -> Result<Lock, XtaskError> {
    let lock = load_lock(repo)?;
    validate_lock(&lock)?;
    let isolation = Isolation::new("pohunek-hermes-compat-")?;
    let env = isolation.model_free_env();

    for check in &lock.cli_checks {
        let output = run_process(
            hermes_bin,
            &check.args,
            &isolation.work,
            &env,
            limits.command_timeout,
            limits.command_output_bytes,
        )?;
        if !output.status.success() {
            return Err(fail(format!(
                "Hermes CLI check `{}` exited unsuccessfully",
                check.id
            )));
        }
        let text = combined_text(&output);
        for required in &check.required_text {
            if !text.contains(required) {
                return Err(fail(format!(
                    "Hermes CLI check `{}` is missing required text `{required}`",
                    check.id
                )));
            }
        }
    }

    Ok(lock)
}

fn refresh_with(
    repo: &Path,
    hermes_bin: &Path,
    provider_env: &[ProviderEnv],
    limits: Limits,
) -> Result<RefreshSummary, XtaskError> {
    require_explicit_binary(hermes_bin)?;
    let lock = check_cli(repo, hermes_bin, limits)?;

    let isolation = Isolation::new("pohunek-hermes-goldens-")?;
    let scenarios = classic_scenarios(limits);
    let mut captures = Vec::new();
    let mut resume_reference = None;

    for scenario in &scenarios {
        let capture = run_pty(hermes_bin, scenario, &isolation, provider_env, limits)?;
        validate_classic_capture(scenario.id, &capture)?;
        if scenario.id == "completion" {
            resume_reference = extract_session_reference(&capture.bytes);
        }
        captures.push((scenario.id, scenario.mode, GoldenStatus::Captured, capture));
    }

    let reference = resume_reference.ok_or_else(|| {
        fail(
            "Hermes completion capture did not expose a native session reference; an operator-configured provider is required",
        )
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
    let resume_capture = run_pty(hermes_bin, &resume, &isolation, provider_env, limits)?;
    validate_resume_capture(&resume_capture, &resume.args[2])?;
    captures.push((
        resume.id,
        resume.mode,
        GoldenStatus::Captured,
        resume_capture,
    ));

    let tui = tui_scenario(limits);
    let tui_capture = run_pty(hermes_bin, &tui, &isolation, provider_env, limits);
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
        provider_env,
        captures,
        limits,
    )
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
    strip_terminal_controls(&String::from_utf8_lossy(&bytes))
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
                Action::WaitFor(Evidence::Working, limits.working_wait),
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
        "short-input" | "completion" => Evidence::ExactLine(SHORT_RESPONSE),
        "multiline-input" => Evidence::ExactLine(MULTILINE_RESPONSE),
        "working" => Evidence::Working,
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
    provider_env: &[ProviderEnv],
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
    for (key, value) in isolation.isolation_env() {
        command.env(key, value);
    }
    for variable in provider_env {
        command.env(&variable.name, &variable.value);
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
    action_result?;
    let (exit_code, killed) = finish_result?;
    let (bytes, overflow) = output_snapshot(&output)?;
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
    let mut observed_bytes = 0;
    for action in &scenario.actions {
        match action {
            Action::WaitFor(evidence, timeout) => {
                let observed = wait_for_evidence(
                    scenario.id,
                    child,
                    output,
                    evidence,
                    observed_bytes,
                    *timeout,
                )?;
                if !observed && !matches!(evidence, Evidence::AlternateScreen) {
                    return Err(fail(format!(
                        "Hermes PTY scenario `{}` exited before required evidence appeared",
                        scenario.id
                    )));
                }
                observed_bytes = output_snapshot(output)?.0.len();
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
    observed_bytes: usize,
    timeout: Duration,
) -> Result<bool, XtaskError> {
    let deadline = Instant::now() + timeout;
    loop {
        let (bytes, overflow) = output_snapshot(output)?;
        if overflow {
            return Err(fail(format!(
                "Hermes PTY scenario `{scenario_id}` exceeded its output limit"
            )));
        }
        let evidence_start = observed_bytes.saturating_sub(32).min(bytes.len());
        if evidence_matches(evidence, &bytes[evidence_start..]) {
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
    let text = strip_terminal_controls(&String::from_utf8_lossy(output));
    match evidence {
        Evidence::PromptReady => text.contains(PROMPT_READY_MARKER),
        Evidence::ExactLine(expected) => has_exact_line(&text, expected),
        Evidence::Working => text.contains("Running") && text.contains("sleep"),
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

fn has_exact_line(text: &str, expected: &str) -> bool {
    text.lines().any(|line| line.trim() == expected)
}

fn exact_line_count(text: &str, expected: &str) -> usize {
    text.lines().filter(|line| line.trim() == expected).count()
}

fn submitted_user_turn_starts(lines: &[&str]) -> Vec<usize> {
    lines
        .windows(2)
        .enumerate()
        .filter_map(|(index, pair)| {
            (pair[0].trim() == USER_TURN_SEPARATOR)
                .then(|| pair[1].trim())
                .filter(|line| line.starts_with(USER_TURN_MARKER))
                .map(|_| index + 1)
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

fn has_one_submitted_user_turn(text: &str, expected_prompt: &str) -> bool {
    let lines: Vec<_> = text.lines().collect();
    let starts = submitted_user_turn_starts(&lines);
    if starts.len() != 1 {
        return false;
    }
    let expected = normalize_preview_text(expected_prompt);
    if expected.is_empty() || expected.len() > MAX_USER_TURN_PREVIEW_BYTES {
        return false;
    }

    let mut observed = String::new();
    let mut observed_bytes = 0;
    for (offset, line) in lines[starts[0]..]
        .iter()
        .take(MAX_USER_TURN_PREVIEW_LINES)
        .enumerate()
    {
        let trimmed = line.trim();
        let fragment = if offset == 0 {
            let Some(fragment) = trimmed.strip_prefix(USER_TURN_MARKER) else {
                return false;
            };
            fragment.trim_start()
        } else {
            trimmed
        };
        if fragment.is_empty() {
            return false;
        }
        observed_bytes += fragment.len();
        if observed_bytes > MAX_USER_TURN_PREVIEW_BYTES {
            return false;
        }
        if !observed.is_empty() {
            observed.push(' ');
        }
        observed.push_str(&normalize_preview_text(fragment));
        if observed == expected {
            return true;
        }
        if !expected
            .strip_prefix(&observed)
            .is_some_and(|remainder| remainder.starts_with(' '))
        {
            return false;
        }
    }
    false
}

fn has_sanitized_session_reference(text: &str) -> bool {
    let pattern = Regex::new(r"(?im)(?:Session ID|Session|session_id):\s*<SESSION_ID>")
        .expect("sanitized session-reference regex is valid");
    pattern.is_match(text)
}

fn validate_classic_transcript(id: &str, text: &str) -> Result<(), XtaskError> {
    if !text.contains(PROMPT_READY_MARKER) {
        return Err(fail(format!(
            "Hermes PTY scenario `{id}` lacks prompt-ready evidence"
        )));
    }
    let valid = match id {
        "prompt-ready" => submitted_user_turn_count(text) == 0,
        "short-input" => {
            has_one_submitted_user_turn(text, "Reply with exactly HERMES_COMPAT_OK.")
                && exact_line_count(text, SHORT_RESPONSE) == 1
        }
        "multiline-input" => {
            has_one_submitted_user_turn(text, MULTILINE_PREVIEW)
                && text.contains("alpha\nbeta")
                && exact_line_count(text, MULTILINE_RESPONSE) == 1
        }
        "working" => {
            has_one_submitted_user_turn(
                text,
                "Use the terminal tool to run `sleep 8`, then reply exactly HERMES_WORKING_DONE.",
            ) && text.contains("Running")
                && text.contains("sleep 8")
        }
        "approval-blocked" => {
            has_one_submitted_user_turn(
                text,
                "Use the terminal tool to run `rm -f HERMES_COMPAT_APPROVAL_SENTINEL`, then reply exactly HERMES_APPROVAL_DONE.",
            ) && text.contains("Dangerous Command")
                && text.contains("HERMES_COMPAT_APPROVAL_SENTINEL")
                && text.contains("Allow once")
                && text.contains("Deny")
        }
        "completion" => {
            has_one_submitted_user_turn(text, "Reply with exactly HERMES_COMPAT_OK.")
                && exact_line_count(text, SHORT_RESPONSE) == 1
                && (extract_session_reference(text.as_bytes()).is_some()
                    || has_sanitized_session_reference(text))
        }
        "interruption" => {
            has_one_submitted_user_turn(
                text,
                "Use the terminal tool to run `sleep 30`, then reply exactly HERMES_INTERRUPT_DONE.",
            ) && text.contains("Running")
                && text.contains("sleep 30")
                && text.contains(INTERRUPT_MARKER)
        }
        "exit" => submitted_user_turn_count(text) == 0 && text.contains("Goodbye!"),
        _ => false,
    };
    if !valid {
        return Err(fail(format!(
            "Hermes PTY scenario `{id}` lacks its required semantic evidence"
        )));
    }
    Ok(())
}

fn validate_classic_capture(id: &str, capture: &PtyCapture) -> Result<(), XtaskError> {
    validate_classic_mode_and_exit(id, capture)?;
    let text = strip_terminal_controls(&String::from_utf8_lossy(&capture.bytes));
    validate_classic_transcript(id, &text)
}

fn validate_resume_capture(capture: &PtyCapture, reference: &str) -> Result<(), XtaskError> {
    validate_classic_mode_and_exit("resume", capture)?;
    let text = strip_terminal_controls(&String::from_utf8_lossy(&capture.bytes));
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
        "# Hermes PTY compatibility golden\nstate: {}\nmode: {}\nrelease: {release}\nterminal: {expected_terminal}\nprocess: ",
        record.id, record.mode
    );
    let remainder = fixture.strip_prefix(&header).ok_or_else(|| {
        fail(format!(
            "Hermes golden `{}` has invalid evidence metadata",
            record.id
        ))
    })?;
    let (process, transcript) = remainder.split_once("\n\n").ok_or_else(|| {
        fail(format!(
            "Hermes golden `{}` has no bounded transcript",
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
    if !valid_process || transcript.trim().is_empty() {
        return Err(fail(format!(
            "Hermes golden `{}` has invalid process or transcript evidence",
            record.id
        )));
    }

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

fn recognized_tui_unavailable(output: &[u8]) -> bool {
    if output.is_empty() || output.len() > MAX_PTY_OUTPUT_BYTES {
        return false;
    }
    let text = strip_terminal_controls(&String::from_utf8_lossy(output));
    TUI_UNAVAILABLE_MARKERS
        .iter()
        .any(|marker| text.contains(marker))
}

fn extract_session_reference(output: &[u8]) -> Option<String> {
    let text = strip_terminal_controls(&String::from_utf8_lossy(output));
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
    provider_env: &[ProviderEnv],
    captures: Vec<(&'static str, &'static str, GoldenStatus, PtyCapture)>,
    limits: Limits,
) -> Result<RefreshSummary, XtaskError> {
    let golden_root = repo.join(GOLDEN_ROOT);
    fs::create_dir_all(&golden_root)
        .map_err(|error| fail(format!("failed to create Hermes golden directory: {error}")))?;
    let replacements = sensitive_paths(repo, hermes_bin, isolation, provider_env);
    let mut staged = Vec::new();
    let mut records = Vec::new();
    let mut unsupported = 0;

    for (id, mode, status, capture) in captures {
        let body = sanitize_output(&String::from_utf8_lossy(&capture.bytes), &replacements);
        validate_safe_fixture(&body)?;
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
        let rendered = format!(
            "# Hermes PTY compatibility golden\nstate: {id}\nmode: {mode}\nrelease: {}\nterminal: {terminal}\nprocess: {disposition}\n\n{}\n",
            lock.release,
            body.trim_end()
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
    provider_env: &[ProviderEnv],
) -> Vec<(String, &'static str)> {
    let mut paths = vec![
        (isolation.root.display().to_string(), "<ISOLATED_ROOT>"),
        (repo.display().to_string(), "<REPOSITORY>"),
        (hermes_bin.display().to_string(), "<HERMES_BIN>"),
    ];
    if let Some(parent) = hermes_bin.parent() {
        paths.push((parent.display().to_string(), "<HERMES_INSTALL_DIR>"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push((PathBuf::from(home).display().to_string(), "<USER_HOME>"));
    }
    paths.extend(
        provider_env
            .iter()
            .map(|variable| (variable.value.clone(), "<PROVIDER_CREDENTIAL>")),
    );
    paths.sort_by_key(|path| std::cmp::Reverse(path.0.len()));
    paths
}

fn sanitize_output(output: &str, paths: &[(String, &'static str)]) -> String {
    let mut sanitized = strip_terminal_controls(output)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
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
    sanitized
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_terminal_controls(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            0x1b => {
                index += 1;
                if index >= bytes.len() {
                    break;
                }
                match bytes[index] {
                    b'[' => {
                        index += 1;
                        while index < bytes.len() {
                            let byte = bytes[index];
                            index += 1;
                            if (0x40..=0x7e).contains(&byte) {
                                break;
                            }
                        }
                    }
                    b']' => {
                        index += 1;
                        while index < bytes.len() {
                            if bytes[index] == 0x07 {
                                index += 1;
                                break;
                            }
                            if bytes[index] == 0x1b && bytes.get(index + 1).copied() == Some(b'\\')
                            {
                                index += 2;
                                break;
                            }
                            index += 1;
                        }
                    }
                    _ => index += 1,
                }
            }
            b'\n' | b'\t' | 0x20..=0x7e => {
                output.push(bytes[index]);
                index += 1;
            }
            0x80..=0xff => {
                let rest = &text[index..];
                let Some(character) = rest.chars().next() else {
                    break;
                };
                let mut encoded = [0_u8; 4];
                output.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                index += character.len_utf8();
            }
            _ => index += 1,
        }
    }
    String::from_utf8_lossy(&output).into_owned()
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

fn resolve_provider_env(names: &[String]) -> Result<Vec<ProviderEnv>, XtaskError> {
    names
        .iter()
        .map(|name| {
            validate_provider_env_name(name)?;
            let value = std::env::var_os(name).ok_or_else(|| {
                fail(format!("provider environment variable `{name}` is not set"))
            })?;
            if value.is_empty() {
                return Err(fail(format!(
                    "provider environment variable `{name}` is empty"
                )));
            }
            let value = value.into_string().map_err(|_non_utf8| {
                fail(format!(
                    "provider environment variable `{name}` is not valid UTF-8"
                ))
            })?;
            Ok(ProviderEnv {
                name: name.clone(),
                value,
            })
        })
        .collect()
}

fn validate_provider_env_name(name: &str) -> Result<(), XtaskError> {
    /// Credential variables read by pinned Hermes model-provider adapters.
    ///
    /// Endpoint/path/environment-control variables are deliberately excluded.
    const PROVIDER_CREDENTIAL_ENV: [&str; 43] = [
        "AI_GATEWAY_API_KEY",
        "ALIBABA_CODING_PLAN_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_TOKEN",
        "ARCEEAI_API_KEY",
        "AZURE_FOUNDRY_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "COPILOT_GITHUB_TOKEN",
        "CUSTOM_API_KEY",
        "DASHSCOPE_API_KEY",
        "DEEPINFRA_API_KEY",
        "DEEPSEEK_API_KEY",
        "FIREWORKS_API_KEY",
        "GEMINI_API_KEY",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GLM_API_KEY",
        "GMI_API_KEY",
        "GOOGLE_API_KEY",
        "HF_TOKEN",
        "KILOCODE_API_KEY",
        "KIMI_API_KEY",
        "KIMI_CN_API_KEY",
        "KIMI_CODING_API_KEY",
        "LM_API_KEY",
        "MINIMAX_API_KEY",
        "MINIMAX_CN_API_KEY",
        "NOUS_API_KEY",
        "NOVITA_API_KEY",
        "NVIDIA_API_KEY",
        "OLLAMA_API_KEY",
        "OPENAI_API_KEY",
        "OPENCODE_GO_API_KEY",
        "OPENCODE_ZEN_API_KEY",
        "OPENROUTER_API_KEY",
        "QWEN_API_KEY",
        "STEPFUN_API_KEY",
        "TOKENHUB_API_KEY",
        "UPSTAGE_API_KEY",
        "XAI_API_KEY",
        "XIAOMI_API_KEY",
        "ZAI_API_KEY",
        "Z_AI_API_KEY",
    ];
    if !PROVIDER_CREDENTIAL_ENV.contains(&name) {
        return Err(fail(format!(
            "invalid --provider-env name `{name}`; only pinned Hermes provider credential variables are accepted"
        )));
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

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fail(message: impl Into<String>) -> XtaskError {
    XtaskError::Usage(message.into())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        compatibility_with, has_alternate_screen, load_golden_manifest, load_lock, refresh_with,
        run_process, sanitize_output, sensitive_paths, strip_terminal_controls,
        validate_golden_manifest, validate_provider_env_name, validate_safe_fixture,
        GoldenManifest, GoldenStatus, Isolation, Limits, ProviderEnv, GOLDEN_MANIFEST, GOLDEN_ROOT,
        LOCK_PATH,
    };

    fn fast_limits() -> Limits {
        Limits {
            command_timeout: Duration::from_secs(2),
            command_output_bytes: 16 * 1024,
            pty_output_bytes: 64 * 1024,
            startup_wait: Duration::from_millis(10),
            turn_wait: Duration::from_millis(10),
            working_wait: Duration::from_millis(10),
            input_settle_wait: Duration::from_millis(10),
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
        repo
    }

    #[cfg(unix)]
    const FAKE_HERMES_TEMPLATE: &str = r#"#!/bin/sh
user_turn() {
  echo '__SEPARATOR__'
  printf '__MARKER__ %s\n' "$1"
}
case "$*" in
  --version) echo 'Hermes Agent v__VERSION__ (2026.8.3)' ;;
  --help) echo 'usage: hermes chat profile --version' ;;
  'chat --help') echo 'usage: hermes chat --resume --pass-session-id --tui --cli' ;;
  'profile --help') echo 'usage: hermes profile list create show rename' ;;
  'profile list --help') echo 'usage: hermes profile list [-h]' ;;
  'profile create --help') echo 'usage: hermes profile create [-h] [--clone-from SOURCE] profile_name' ;;
  'profile show --help') echo 'usage: hermes profile show [-h] profile_name' ;;
  'profile rename --help') echo 'usage: hermes profile rename [-h] old_name new_name' ;;
  'chat --tui')
    trap 'exit 0' INT
    printf '\033[?1049hHermes TUI\n'
    while :; do sleep 1; done
    ;;
  chat\ --resume\ *)
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
            user_turn 'Treat all three lines as one prompt.'
            echo 'alpha'
            echo 'beta'
            echo 'Reply with exactly HERMES_MULTILINE_OK.'
            echo HERMES_MULTILINE_OK
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
          user_turn 'Reply with exactly HERMES_COMPAT_OK.'
          echo HERMES_COMPAT_OK
          had_turn=1
          ;;
        *'sleep 8'*)
          user_turn 'Use the terminal tool to run `sleep 8`, then reply exactly HERMES_WORKING_DONE.'
          echo 'Running sleep 8'
          had_turn=1
          ;;
        *HERMES_COMPAT_APPROVAL_SENTINEL*)
          if [ "$wrap_approval" -eq 1 ]; then
            echo '__SEPARATOR__'
            printf '__MARKER__ %s\n' 'Use the terminal tool to run `rm -f HERMES_COMPAT_APPROVAL_SENTINEL`, then reply exactly'
            echo 'HERMES_APPROVAL_DONE.'
          else
            user_turn 'Use the terminal tool to run `rm -f HERMES_COMPAT_APPROVAL_SENTINEL`, then reply exactly HERMES_APPROVAL_DONE.'
          fi
          echo 'Dangerous Command'
          echo 'rm -f HERMES_COMPAT_APPROVAL_SENTINEL'
          echo 'Allow once'
          echo 'Deny'
          had_turn=1
          ;;
        *'sleep 30'*)
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
    fn fake_hermes(root: &Path, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = root.join("hermes");
        let script = FAKE_HERMES_TEMPLATE
            .replace("__VERSION__", version)
            .replace("__SEPARATOR__", super::USER_TURN_SEPARATOR)
            .replace("__MARKER__", super::USER_TURN_MARKER);
        fs::write(&path, script).expect("write fake Hermes");
        let mut permissions = fs::metadata(&path).expect("fake metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("make fake executable");
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
    fn compatibility_accepts_pinned_model_free_shape() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        refresh_with(repo.path(), &binary, &[], fast_limits())
            .expect("controlled refresh succeeds");

        let summary = compatibility_with(repo.path(), &binary, fast_limits())
            .expect("pinned compatibility succeeds");

        assert_eq!(summary.release, "0.20.0");
        assert_eq!(summary.cli_checks, 8);
        assert_eq!(summary.golden_records, 10);
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_rejects_rehashed_captured_golden_without_state_evidence() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        refresh_with(repo.path(), &binary, &[], fast_limits())
            .expect("controlled refresh succeeds");

        let golden_path = repo.path().join(GOLDEN_ROOT).join("short-input.txt");
        let original = fs::read_to_string(&golden_path).expect("read captured golden");
        let (header, _) = original
            .split_once("\n\n")
            .expect("captured golden has a transcript boundary");
        let forged = format!("{header}\n\nHermes setup failed before prompt initialization.\n");
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

        let error = compatibility_with(repo.path(), &binary, fast_limits())
            .expect_err("every captured state is validated even when another state is pending");

        assert!(error.to_string().contains("prompt-ready evidence"));
    }

    #[test]
    #[cfg(unix)]
    fn prompt_echo_without_semantic_turn_evidence_is_rejected() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        rewrite_script(
            &binary,
            "echo HERMES_COMPAT_OK",
            "echo 'prompt echoed only'",
        );

        let error = refresh_with(repo.path(), &binary, &[], fast_limits())
            .expect_err("prompt echo cannot satisfy response evidence");

        assert!(error.to_string().contains("required evidence"));
    }

    #[test]
    #[cfg(unix)]
    fn multiline_requires_one_submitted_user_turn() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        rewrite_script(&binary, "split_paste=0", "split_paste=1");

        let error = refresh_with(repo.path(), &binary, &[], fast_limits())
            .expect_err("embedded newlines submitted as separate turns must fail");

        assert!(error.to_string().contains("required semantic evidence"));
    }

    #[test]
    #[cfg(unix)]
    fn rich_wrapped_approval_preview_is_one_submitted_user_turn() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");
        rewrite_script(&binary, "wrap_approval=0", "wrap_approval=1");

        refresh_with(repo.path(), &binary, &[], fast_limits())
            .expect("one approval preview wrapped at the fixed PTY width remains one turn");
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_rejects_wrong_version() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.21.0");

        let error = compatibility_with(repo.path(), &binary, fast_limits())
            .expect_err("wrong version fails closed");

        assert!(error.to_string().contains("missing required text"));
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_rejects_pending_goldens() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");

        let error = compatibility_with(repo.path(), &binary, fast_limits())
            .expect_err("pending evidence fails closed");

        assert!(error.to_string().contains("is pending"));
    }

    #[test]
    fn golden_manifest_enforces_mode_and_unsupported_matrix() {
        let repo = fixture_repo();
        let lock = load_lock(repo.path()).expect("load fixture lock");
        let mut manifest = load_golden_manifest(repo.path()).expect("load fixture manifest");
        manifest.records[0].status = GoldenStatus::Unsupported;

        let unsupported = validate_golden_manifest(repo.path(), &lock, &manifest, 4096, true)
            .expect_err("classic state cannot be unsupported");
        assert!(unsupported
            .to_string()
            .contains("only the Hermes alternate-screen TUI"));

        let mut manifest = load_golden_manifest(repo.path()).expect("reload fixture manifest");
        manifest.records[9].mode = "classic".to_owned();
        let mode = validate_golden_manifest(repo.path(), &lock, &manifest, 4096, true)
            .expect_err("alternate-screen state requires its exact mode");
        assert!(mode.to_string().contains("invalid display mode"));
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_rejects_modified_lock() {
        let repo = fixture_repo();
        let lock = repo.path().join(LOCK_PATH);
        let mut bytes = fs::read(&lock).expect("read lock");
        bytes.push(b'\n');
        fs::write(lock, bytes).expect("modify lock");

        let error = compatibility_with(repo.path(), Path::new("missing-hermes"), fast_limits())
            .expect_err("modified lock fails before process launch");

        assert!(error.to_string().contains("lock digest mismatch"));
    }

    #[test]
    #[cfg(unix)]
    fn compatibility_bounds_cli_time_and_output() {
        let repo = fixture_repo();
        let sleeper = script(repo.path(), "sleeping-hermes", "#!/bin/sh\nsleep 5\n");
        let mut timeout_limits = fast_limits();
        timeout_limits.command_timeout = Duration::from_millis(50);

        let timeout = compatibility_with(repo.path(), &sleeper, timeout_limits)
            .expect_err("slow CLI fails closed");
        assert!(timeout.to_string().contains("time limit"));

        let noisy = script(
            repo.path(),
            "noisy-hermes",
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 5000 ]; do printf x; i=$((i + 1)); done\n",
        );
        let mut output_limits = fast_limits();
        output_limits.command_output_bytes = 1024;

        let output = compatibility_with(repo.path(), &noisy, output_limits)
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
            "  chat)\n    /bin/sleep 30 &\n    echo $! > '{}'\n    trap",
            pid_file.display()
        );
        rewrite_script(&binary, "  chat)\n    trap", &replacement);

        refresh_with(repo.path(), &binary, &[], fast_limits())
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
        let tui_loop = "    trap 'exit 0' INT\n    printf '\\033[?1049hHermes TUI\\n'\n    while :; do sleep 1; done";
        rewrite_script(
            &unavailable,
            tui_loop,
            "    echo 'node not found \u{2014} install Node.js to use the TUI.'\n    exit 1",
        );
        let summary = refresh_with(repo.path(), &unavailable, &[], fast_limits())
            .expect("recognized local TUI unavailability is recorded");
        assert_eq!(summary.unsupported, 1);

        let crash_repo = fixture_repo();
        let crash = fake_hermes(crash_repo.path(), "0.20.0");
        rewrite_script(
            &crash,
            tui_loop,
            "    echo 'authentication failed'\n    exit 1",
        );
        let error = refresh_with(crash_repo.path(), &crash, &[], fast_limits())
            .expect_err("auth failure cannot be recorded as unsupported");
        assert!(error
            .to_string()
            .contains("neither alternate-screen evidence"));

        let alt_crash_repo = fixture_repo();
        let alt_crash = fake_hermes(alt_crash_repo.path(), "0.20.0");
        rewrite_script(
            &alt_crash,
            tui_loop,
            "    trap 'exit 2' INT\n    printf '\\033[?1049hHermes TUI\\n'\n    while :; do sleep 1; done",
        );
        let error = refresh_with(alt_crash_repo.path(), &alt_crash, &[], fast_limits())
            .expect_err("alternate-screen entry does not hide a subsequent crash");
        assert!(error.to_string().contains("crashed after entering"));
    }

    #[test]
    #[cfg(unix)]
    fn refresh_writes_sanitized_hashed_inventory() {
        let repo = fixture_repo();
        let binary = fake_hermes(repo.path(), "0.20.0");

        let summary = refresh_with(repo.path(), &binary, &[], fast_limits())
            .expect("controlled refresh succeeds");
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
            "\u{1b}[31m/home/operator\u{1b}[0m\r\nSession ID: 20260804_120000_abcdef12\n550e8400-e29b-41d4-a716-446655440000\nC:\\Users\\operator\\secret.txt",
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
    fn fixture_validation_rejects_secret_shaped_output() {
        let error = validate_safe_fixture("API_KEY=abcdefghijk")
            .expect_err("credential-shaped output fails closed");

        assert!(error.to_string().contains("credential-shaped"));
    }

    #[test]
    fn provider_environment_names_are_bounded_and_isolation_safe() {
        validate_provider_env_name("OPENAI_API_KEY").expect("provider name is valid");
        for invalid in [
            "",
            "lowercase_key",
            "HOME",
            "XDG_DATA_HOME",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "BASH_ENV",
            "NODE_OPTIONS",
            "PYTHONSTARTUP",
            "TMPDIR",
            "OPENAI_API_KEY_FILE",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "HERMES_TUI",
            "NAME-WITH-DASH",
            "THIS_PROVIDER_ENVIRONMENT_VARIABLE_NAME_IS_DELIBERATELY_LONGER_THAN_SIXTY_FOUR_BYTES",
        ] {
            assert!(
                validate_provider_env_name(invalid).is_err(),
                "{invalid:?} must be rejected"
            );
        }
    }

    #[test]
    fn provider_environment_debug_redacts_values() {
        let secret = "provider-value-must-not-appear";
        let variable = ProviderEnv {
            name: "OPENAI_API_KEY".to_owned(),
            value: secret.into(),
        };

        let debug = format!("{variable:?}");

        assert!(debug.contains("OPENAI_API_KEY"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(secret));
    }

    #[test]
    fn provider_environment_values_are_removed_from_fixtures() {
        let isolation =
            Isolation::new("hermes-provider-redaction-").expect("create isolated environment");
        let secret = "provider-value-must-not-appear";
        let variables = vec![ProviderEnv {
            name: "OPENAI_API_KEY".to_owned(),
            value: secret.to_owned(),
        }];
        let paths = sensitive_paths(
            Path::new("/repository"),
            Path::new("/usr/bin/hermes"),
            &isolation,
            &variables,
        );

        let sanitized = sanitize_output(&format!("credential={secret}"), &paths);

        assert_eq!(sanitized, "credential=<PROVIDER_CREDENTIAL>");
        assert!(!sanitized.contains(secret));
    }
}
