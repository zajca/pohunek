//! Agent adapter trait and built-in PTY/TUI agent adapters.
//!
//! Per `docs/plan-phase-1.md` "Agent Adapter Boundary": a small trait per agent
//! carrying launch argv/env/cwd, input rules (Claude Ink submit-delay quirk),
//! the state manifest, and the resume command.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pohunek_platform::shell_env;
use protocol::{AgentActivity, ErrorClass, ProtocolError};
use serde::{Deserialize, Serialize};

use crate::detect::Manifest;

mod claude;
mod codex;
mod hermes;
mod profile;
mod shell;

pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use hermes::HermesAdapter;
pub(crate) use profile::{default_args, default_program, ProfileRegistry, ResolvedAgent};
pub use shell::ShellAdapter;

static SHELL_ADAPTER: ShellAdapter = ShellAdapter;
static CODEX_ADAPTER: CodexAdapter = CodexAdapter;
static CLAUDE_ADAPTER: ClaudeAdapter = ClaudeAdapter;
static HERMES_ADAPTER: HermesAdapter = HermesAdapter;
static UNSUPPORTED_ADAPTER: UnsupportedAdapter = UnsupportedAdapter;

#[derive(Debug)]
struct UnsupportedAdapter;

impl AgentAdapter for UnsupportedAdapter {
    fn id(&self) -> &'static str {
        "unsupported"
    }

    fn launch(&self, _opts: &LaunchOpts) -> Result<LaunchCommand, ProtocolError> {
        Err(ProtocolError::agent_kind_unsupported("unknown"))
    }

    fn input_rules(&self) -> InputRules {
        InputRules::unrestricted(false, Duration::ZERO)
    }

    fn manifest(&self) -> &Manifest {
        crate::detect::generic_shell_manifest()
    }
}

/// Default submit delay for Claude Code's Ink TUI.
pub const DEFAULT_CLAUDE_SUBMIT_DELAY: Duration = Duration::from_millis(150);
/// Claude Code flag that forks a resumed native conversation into a new branch.
const CLAUDE_FORK_SESSION_ARG: &str = "--fork-session";

/// A launch executable resolved and canonicalized before provider validation.
///
/// The private path invariant lets the session launch path consume the exact
/// executable that was probed without consulting `PATH` a second time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedLaunchProgram(PathBuf);

impl ValidatedLaunchProgram {
    /// Resolve one configured program and canonicalize its first executable match.
    pub(crate) fn resolve(program: &str) -> Option<Self> {
        let path = std::env::var_os("PATH");
        Self::resolve_with_path(program, path.as_deref())
    }

    fn resolve_with_path(program: &str, path: Option<&OsStr>) -> Option<Self> {
        let candidate = resolve_program(program, path)?;
        let canonical = candidate.canonicalize().ok()?;
        (canonical.is_absolute() && canonical.to_str().is_some()).then_some(Self(canonical))
    }

    /// The absolute canonical path used for both probing and launch.
    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }

    fn as_launch_str(&self) -> &str {
        self.0
            .to_str()
            .expect("validated launch paths have a UTF-8 representation")
    }
}

/// Options supplied by the session registry when launching an agent process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOpts {
    /// Working directory for the agent process.
    pub cwd: PathBuf,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
    /// Extra environment variables for the child process.
    pub env_extra: Vec<(String, String)>,
    /// Exact provider executable already resolved and validated for this launch.
    pub(crate) validated_program: Option<ValidatedLaunchProgram>,
}

/// Sanitized process launch plan passed to a durable worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommand {
    /// Program path or name.
    pub program: String,
    /// Program arguments.
    pub args: Vec<String>,
    /// Extra environment variables to add or override for the child process.
    pub env: Vec<(String, String)>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Initial terminal width in columns.
    pub cols: u16,
    /// Initial terminal height in rows.
    pub rows: u16,
}

/// Input framing rules for programmatic prompt injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRules {
    /// Whether prompt text should be wrapped in bracketed paste markers.
    pub bracketed_paste: bool,
    /// Delay before sending the submit byte as a separate write.
    pub submit_delay: Duration,
    /// Provider-specific validation applied before terminal framing.
    text_policy: InputTextPolicy,
    /// Whether programmatic input is safe while approval UI is visible.
    allow_while_blocked: bool,
}

/// Text accepted by one compiled input adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputTextPolicy {
    /// Preserve the historical behavior for Shell, Codex, and Claude.
    Unrestricted,
    /// Bounded UTF-8 text with only LF and tab from the control ranges.
    HermesSafeText,
}

impl InputRules {
    /// Builds historical unrestricted framing rules.
    #[must_use]
    pub const fn unrestricted(bracketed_paste: bool, submit_delay: Duration) -> Self {
        Self {
            bracketed_paste,
            submit_delay,
            text_policy: InputTextPolicy::Unrestricted,
            allow_while_blocked: true,
        }
    }

    /// Builds the pinned Hermes safe-text and approval-state contract.
    pub(crate) const fn hermes(bracketed_paste: bool, submit_delay: Duration) -> Self {
        Self {
            bracketed_paste,
            submit_delay,
            text_policy: InputTextPolicy::HermesSafeText,
            allow_while_blocked: false,
        }
    }

    /// Replaces framing while retaining the compiled provider safety contract.
    pub(crate) const fn with_framing(self, bracketed_paste: bool, submit_delay: Duration) -> Self {
        Self {
            bracketed_paste,
            submit_delay,
            ..self
        }
    }

    /// Validates text before any bytes are framed or written to the PTY.
    pub(crate) fn validate_text(self, text: &str) -> Result<(), ProtocolError> {
        if self.text_policy == InputTextPolicy::HermesSafeText
            && (text.len() > protocol::MAX_SESSION_INPUT_BYTES
                || text
                    .chars()
                    .any(|character| character.is_control() && !matches!(character, '\n' | '\t')))
        {
            return Err(ProtocolError::session_input_rejected());
        }
        Ok(())
    }

    /// Whether the adapter permits automated input while blocked on owner action.
    pub(crate) const fn allows_while_blocked(self) -> bool {
        self.allow_while_blocked
    }

    /// Validates whether programmatic input is permitted in the visible state.
    pub(crate) fn validate_activity(
        self,
        activity: Option<AgentActivity>,
    ) -> Result<(), ProtocolError> {
        if activity == Some(AgentActivity::Blocked) && !self.allows_while_blocked() {
            Err(ProtocolError::session_input_blocked())
        } else {
            Ok(())
        }
    }
}

/// Maximum length of an id-kind native session reference, in bytes.
const MAX_SESSION_ID_LEN: usize = 512;
/// Maximum length of a path-kind native session reference, in bytes.
const MAX_SESSION_PATH_LEN: usize = 4096;

/// Whether a native session reference is an opaque id or a filesystem path.
///
/// Ported from herdr `src/agent_resume.rs`: Claude and Codex resume by id; the
/// path variant exists for agents that resume from a transcript path. Both
/// kinds feed the same resume-argv builder via [`SessionRef::value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRefKind {
    /// An opaque native session id (e.g. `claude --resume <id>`).
    Id,
    /// An absolute filesystem path to a native session/transcript file.
    Path,
}

/// Native agent session reference used to build resume commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    kind: SessionRefKind,
    value: String,
}

impl SessionRef {
    /// Build a validated id-kind native session reference.
    ///
    /// Validation (herdr `valid_session_id`): non-empty, ≤512 bytes, no control
    /// characters.
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolError> {
        Self::id(value)
    }

    /// Build a validated id-kind native session reference.
    pub fn id(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        if value.is_empty() {
            return Err(invalid_session_ref("native session id cannot be empty"));
        }
        if value.len() > MAX_SESSION_ID_LEN {
            return Err(invalid_session_ref(
                "native session id cannot exceed 512 bytes",
            ));
        }
        if value.chars().any(char::is_control) {
            return Err(invalid_session_ref(
                "native session id cannot contain control characters",
            ));
        }
        // The value is exec'd as a positional resume argument (`claude --resume
        // <id>` / `codex resume <id>`) with no `--` separator, so a leading dash
        // would be parsed by the agent CLI as a flag. Reject it at this trust
        // boundary to prevent argv flag injection from a socket-supplied id.
        if value.starts_with('-') {
            return Err(invalid_session_ref(
                "native session id cannot begin with '-'",
            ));
        }

        Ok(Self {
            kind: SessionRefKind::Id,
            value,
        })
    }

    /// Build a validated path-kind native session reference.
    ///
    /// Validation (herdr `valid_session_path`): non-empty, ≤4096 bytes, no
    /// control characters, and an absolute path.
    pub fn path(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        if value.is_empty() {
            return Err(invalid_session_ref("native session path cannot be empty"));
        }
        if value.len() > MAX_SESSION_PATH_LEN {
            return Err(invalid_session_ref(
                "native session path cannot exceed 4096 bytes",
            ));
        }
        if value.chars().any(char::is_control) {
            return Err(invalid_session_ref(
                "native session path cannot contain control characters",
            ));
        }
        if !Path::new(&value).is_absolute() {
            return Err(invalid_session_ref("native session path must be absolute"));
        }

        Ok(Self {
            kind: SessionRefKind::Path,
            value,
        })
    }

    /// Whether this reference is an id or a path.
    #[must_use]
    pub fn kind(&self) -> SessionRefKind {
        self.kind
    }

    /// Native session reference value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// How an agent's resume invocation is shaped on the command line.
///
/// The two built-in kinds differ only in this: Claude resumes via a `--resume`
/// flag, Codex via a `resume` subcommand. A host profile (Part C) may override the
/// mode so a profile whose base is `claude` can still drive a `resume`-subcommand
/// CLI. Both produce a two-element argv ending in the native session reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeMode {
    /// `<program> --resume <ref>` (Claude).
    Flag,
    /// `<program> resume <ref>` (Codex).
    Subcommand,
}

impl ResumeMode {
    /// Build the resume argv (excluding `argv[0]`) for `value`.
    fn argv(self, value: &str) -> Vec<String> {
        match self {
            ResumeMode::Flag => vec!["--resume".to_owned(), value.to_owned()],
            ResumeMode::Subcommand => vec!["resume".to_owned(), value.to_owned()],
        }
    }
}

/// The resolved "how to resume" for a session: the argv mode plus which native
/// reference kind ([`SessionRefKind`]) its captured value is.
///
/// `ref_kind` decides which validating [`SessionRef`] constructor builds the
/// reference at resume — and therefore which trust-boundary guard applies: `Id`
/// carries the leading-dash argv-injection guard, `Path` carries the
/// must-be-absolute guard (the asymmetry is intentional, see [`SessionRef`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResumeTemplate {
    /// Whether the resume argv uses a `--resume` flag or a `resume` subcommand.
    pub mode: ResumeMode,
    /// Whether the captured native reference is an id or a path.
    pub ref_kind: SessionRefKind,
}

/// A compiled provider-native fork command shape.
///
/// Fork mechanics are intentionally independent from [`ResumeMode`]. A provider
/// may support resume without supporting a native conversation fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkMode {
    /// Append `--fork-session` to a Claude Code resume operation.
    ClaudeSession,
}

impl ForkMode {
    /// Append provider-specific fork arguments to `args`.
    fn append_args(self, args: &mut Vec<String>) {
        match self {
            Self::ClaudeSession => args.push(CLAUDE_FORK_SESSION_ARG.to_owned()),
        }
    }
}

/// A compiled provider-native fork capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ForkTemplate {
    /// The frozen provider resume operation used to select the conversation.
    pub resume: ResumeTemplate,
    /// The provider-owned argv shape for a fork operation.
    pub mode: ForkMode,
}

impl ForkTemplate {
    /// Build fork argv from the frozen resume operation and provider extension.
    fn argv(self, value: &str) -> Vec<String> {
        let mut args = self.resume.mode.argv(value);
        self.mode.append_args(&mut args);
        args
    }
}

/// The independently declared native recovery capabilities of an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentCapabilities {
    /// Native resume support, when the provider supports it.
    pub resume: Option<ResumeTemplate>,
    /// Native fork support, when the provider supports it.
    pub fork: Option<ForkTemplate>,
}

/// Return the compiled capabilities for a built-in base agent kind.
#[must_use]
pub(crate) fn base_capabilities(base: &protocol::AgentKind) -> AgentCapabilities {
    match base {
        protocol::AgentKind::Shell | protocol::AgentKind::Unknown(_) => AgentCapabilities {
            resume: None,
            fork: None,
        },
        protocol::AgentKind::Codex => AgentCapabilities {
            resume: Some(ResumeTemplate {
                mode: ResumeMode::Subcommand,
                ref_kind: SessionRefKind::Id,
            }),
            fork: None,
        },
        protocol::AgentKind::Claude => AgentCapabilities {
            resume: Some(ResumeTemplate {
                mode: ResumeMode::Flag,
                ref_kind: SessionRefKind::Id,
            }),
            fork: Some(ForkTemplate {
                resume: ResumeTemplate {
                    mode: ResumeMode::Flag,
                    ref_kind: SessionRefKind::Id,
                },
                mode: ForkMode::ClaudeSession,
            }),
        },
        protocol::AgentKind::Hermes => AgentCapabilities {
            resume: Some(ResumeTemplate {
                mode: ResumeMode::Flag,
                ref_kind: SessionRefKind::Id,
            }),
            fork: None,
        },
    }
}

/// The resume template for a bare base kind, or `None` when it has no native
/// resume (a shell).
pub(crate) fn base_resume_template(base: &protocol::AgentKind) -> Option<ResumeTemplate> {
    base_capabilities(base).resume
}

/// Return the compiled native fork template for a base agent kind.
#[must_use]
pub(crate) fn base_fork_template(base: &protocol::AgentKind) -> Option<ForkTemplate> {
    base_capabilities(base).fork
}

/// Thin per-agent adapter for launch, input, and activity behavior.
pub trait AgentAdapter: std::fmt::Debug + Send + Sync {
    /// Stable adapter id.
    fn id(&self) -> &str;
    /// Build the PTY launch command, resolving the executable from `PATH`.
    fn launch(&self, opts: &LaunchOpts) -> Result<LaunchCommand, ProtocolError>;
    /// Programmatic input injection rules.
    fn input_rules(&self) -> InputRules;
    /// Agent-specific activity manifest.
    fn manifest(&self) -> &Manifest;
}

fn launch_command(
    program: &str,
    args: Vec<String>,
    opts: &LaunchOpts,
) -> Result<LaunchCommand, ProtocolError> {
    build_pty_command(program, args, opts)
}

/// Return the built-in adapter for an agent kind.
#[must_use]
pub fn adapter_for(agent: &protocol::AgentKind) -> &'static dyn AgentAdapter {
    match agent {
        protocol::AgentKind::Shell => &SHELL_ADAPTER,
        protocol::AgentKind::Codex => &CODEX_ADAPTER,
        protocol::AgentKind::Claude => &CLAUDE_ADAPTER,
        protocol::AgentKind::Hermes => &HERMES_ADAPTER,
        protocol::AgentKind::Unknown(_) => &UNSUPPORTED_ADAPTER,
    }
}

/// Return the launch adapter, preserving the configured shell command seam.
pub(crate) fn launch_adapter_for<'a>(
    agent: &protocol::AgentKind,
    shell_command: &'a crate::session::ShellCommand,
) -> &'a dyn AgentAdapter {
    match agent {
        protocol::AgentKind::Shell => shell_command,
        protocol::AgentKind::Codex | protocol::AgentKind::Claude | protocol::AgentKind::Hermes => {
            adapter_for(agent)
        }
        protocol::AgentKind::Unknown(_) => &UNSUPPORTED_ADAPTER,
    }
}

/// Resolve `program` on `PATH` and build a PTY launch command in `opts`.
///
/// Shared by first-launch (adapter `launch`) and resume (`resume_pty_command`):
/// resume argv carries a runtime-built program name, so it cannot reuse the
/// `&'static str` launch path.
pub fn build_pty_command(
    program: &str,
    args: Vec<String>,
    opts: &LaunchOpts,
) -> Result<LaunchCommand, ProtocolError> {
    let program = opts.validated_program.as_ref().map_or_else(
        || resolve_binary(program),
        |validated| Ok(validated.as_launch_str().to_owned()),
    )?;
    Ok(LaunchCommand {
        program,
        args,
        env: opts.env_extra.clone(),
        cwd: opts.cwd.clone(),
        cols: opts.cols,
        rows: opts.rows,
    })
}

/// Build the PTY command that resumes a session from its frozen structural
/// snapshot (Part C: C.4).
///
/// The `program` and the resume `template` come from the session's launch-time
/// snapshot, so a host profile that overrode the launch program or the resume
/// mode resumes with exactly those values. The `session_ref` must already be
/// the kind named by `template.ref_kind` (its constructor enforced the matching
/// guard).
pub(crate) fn resume_pty_command_from_template(
    program: &str,
    frozen_args: Vec<String>,
    template: ResumeTemplate,
    session_ref: &SessionRef,
    opts: &LaunchOpts,
) -> Result<LaunchCommand, ProtocolError> {
    let mut args = frozen_args;
    args.extend(template.mode.argv(session_ref.value()));
    build_pty_command(program, args, opts)
}

/// Build the PTY command that forks a native session from a frozen snapshot.
pub(crate) fn fork_pty_command_from_template(
    program: &str,
    frozen_args: Vec<String>,
    template: ForkTemplate,
    session_ref: &SessionRef,
    opts: &LaunchOpts,
) -> Result<LaunchCommand, ProtocolError> {
    let mut args = frozen_args;
    args.extend(template.argv(session_ref.value()));
    build_pty_command(program, args, opts)
}

pub(crate) fn agent_not_resumable(agent: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_not_resumable",
        format!("{agent} sessions cannot be resumed"),
        None,
    )
}

pub(crate) fn agent_fork_unsupported() -> ProtocolError {
    ProtocolError::agent_fork_unsupported()
}

/// Resolve `program` with the shared executable policy of
/// [`pohunek_platform::shell_env`].
///
/// A program containing `/` must be absolute; a bare name is searched in the
/// absolute entries of `path` in order, and a candidate the effective user
/// cannot execute is skipped. Relative and empty `PATH` entries are skipped, not
/// resolved against the working directory, and the search goes on.
fn resolve_program(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    shell_env::resolve_executable_in_path_value(OsStr::new(program), path).ok()
}

fn resolve_binary(name: &str) -> Result<String, ProtocolError> {
    let path = std::env::var_os("PATH");
    resolve_program(name, path.as_deref())
        .and_then(|resolved| resolved.to_str().map(str::to_owned))
        .ok_or_else(|| missing_binary(name))
}

/// Resolve `name` to the first **executable** match on `PATH`, for capability
/// probing (`host.inspect`). Uses the same resolver as [`resolve_binary`], so
/// "available" in a capability snapshot agrees with what the launch path would
/// accept. Returns the resolved path, or `None` when nothing executable matches.
pub(crate) fn which_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH");
    resolve_program(name, path.as_deref())
}

fn missing_binary(name: &str) -> ProtocolError {
    // The canonical constructor lives in the protocol crate so this PATH-resolution
    // path and the daemon's PTY-spawn ENOENT path produce one identical error
    // (same stable code, message shape, and recover hint).
    ProtocolError::agent_binary_missing(name)
}

fn invalid_session_ref(message: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorClass::Runtime, "invalid_session_ref", message, None)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};
    use std::time::Duration;

    use protocol::{AgentActivity, ErrorClass};

    use super::{
        base_capabilities, base_resume_template, build_pty_command, fork_pty_command_from_template,
        resolve_binary, resume_pty_command_from_template, which_executable, AgentAdapter,
        ClaudeAdapter, CodexAdapter, ForkMode, ForkTemplate, HermesAdapter, LaunchOpts, ResumeMode,
        ResumeTemplate, SessionRef, SessionRefKind, ShellAdapter, ValidatedLaunchProgram,
    };
    use crate::detect::{ManifestRegion, MatchContext};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn launch_opts(cwd: PathBuf) -> LaunchOpts {
        LaunchOpts {
            cwd,
            cols: 120,
            rows: 40,
            env_extra: vec![("POHUNEK_SESSION_ID".to_owned(), "s-42".to_owned())],
            validated_program: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        // Exclusive creation under a random name; the directory is left behind
        // like the fixture it replaces.
        pohunek_test_support::tempdir_with_prefix(&format!("pohunek-agent-test-{tag}-"))
            .expect("create temp dir")
            .keep()
    }

    fn write_executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write executable fixture");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).expect("chmod executable fixture");
        };

        path
    }

    fn with_path<T>(path: &Path, run: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", path);
        let result = run();
        match old_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        result
    }

    fn with_path_and_shell<T>(path: &Path, shell: &str, run: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let old_path = std::env::var_os("PATH");
        let old_shell = std::env::var_os("SHELL");
        std::env::set_var("PATH", path);
        std::env::set_var("SHELL", shell);
        let result = run();
        match old_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        match old_shell {
            Some(value) => std::env::set_var("SHELL", value),
            None => std::env::remove_var("SHELL"),
        }
        result
    }

    #[test]
    fn codex_launch_resolves_binary_and_preserves_opts() {
        let bin_dir = temp_dir("codex-bin");
        let codex = write_executable(&bin_dir, "codex");
        let cwd = temp_dir("codex-cwd");

        let command = with_path(&bin_dir, || {
            CodexAdapter
                .launch(&launch_opts(cwd.clone()))
                .expect("codex launch command")
        });

        assert_eq!(command.program, codex.display().to_string());
        assert!(command.args.is_empty());
        assert_eq!(command.cwd, cwd);
        assert_eq!(command.cols, 120);
        assert_eq!(command.rows, 40);
        assert_eq!(
            command.env,
            vec![("POHUNEK_SESSION_ID".to_owned(), "s-42".to_owned())]
        );
    }

    #[test]
    fn claude_launch_resolves_binary_and_preserves_opts() {
        let bin_dir = temp_dir("claude-bin");
        let claude = write_executable(&bin_dir, "claude");
        let cwd = temp_dir("claude-cwd");

        let command = with_path(&bin_dir, || {
            ClaudeAdapter
                .launch(&launch_opts(cwd.clone()))
                .expect("claude launch command")
        });

        assert_eq!(command.program, claude.display().to_string());
        assert!(command.args.is_empty());
        assert_eq!(command.cwd, cwd);
        assert_eq!(command.cols, 120);
        assert_eq!(command.rows, 40);
        assert_eq!(
            command.env,
            vec![("POHUNEK_SESSION_ID".to_owned(), "s-42".to_owned())]
        );
    }

    #[test]
    fn hermes_launch_is_exact_chat_argv_and_preserves_opts() {
        let bin_dir = temp_dir("hermes-bin");
        let hermes = write_executable(&bin_dir, "hermes");
        let cwd = temp_dir("hermes-cwd");

        let command = with_path(&bin_dir, || {
            HermesAdapter
                .launch(&launch_opts(cwd.clone()))
                .expect("Hermes launch command")
        });

        assert_eq!(command.program, hermes.display().to_string());
        assert_eq!(command.args, vec!["chat"]);
        assert_eq!(command.cwd, cwd);
        assert_eq!((command.cols, command.rows), (120, 40));
        assert_eq!(
            command.env,
            vec![("POHUNEK_SESSION_ID".to_owned(), "s-42".to_owned())]
        );
    }

    #[test]
    fn validated_program_refuses_relative_and_empty_path_entries() {
        let cwd = temp_dir("validated-relative-path");
        let relative_dir = cwd.join("relative-bin");
        fs::create_dir(&relative_dir).expect("create relative bin");
        write_executable(&relative_dir, "hermes");
        write_executable(&cwd, "hermes");
        let _cwd_guard = ENV_LOCK.lock().expect("env lock");
        let old_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&cwd).expect("enter fixture cwd");
        let results = [
            // A relative entry, an empty entry (the working directory), and `.`
            // are never searched; neither is a relative program path.
            ValidatedLaunchProgram::resolve_with_path("hermes", Some(OsStr::new("relative-bin"))),
            ValidatedLaunchProgram::resolve_with_path("hermes", Some(OsStr::new(""))),
            ValidatedLaunchProgram::resolve_with_path("hermes", Some(OsStr::new("."))),
            ValidatedLaunchProgram::resolve_with_path("./hermes", Some(OsStr::new("/usr/bin"))),
            ValidatedLaunchProgram::resolve_with_path(
                "relative-bin/hermes",
                Some(OsStr::new("/usr/bin")),
            ),
        ];
        std::env::set_current_dir(old_cwd).expect("restore cwd");
        for (index, result) in results.iter().enumerate() {
            assert!(result.is_none(), "case {index}: {result:?}");
        }
    }

    #[test]
    fn validated_program_pins_the_canonical_absolute_match() {
        let bin = temp_dir("validated-absolute");
        let hermes = write_executable(&bin, "hermes");
        let validated = ValidatedLaunchProgram::resolve_with_path("hermes", Some(bin.as_os_str()))
            .expect("resolve on an absolute PATH entry");
        let expected = hermes.canonicalize().expect("canonical fixture");
        assert_eq!(validated.as_path(), expected);
        let by_absolute =
            ValidatedLaunchProgram::resolve_with_path(hermes.to_str().expect("utf-8"), None)
                .expect("an absolute program needs no PATH");
        assert_eq!(by_absolute.as_path(), expected);

        let mut opts = launch_opts(temp_dir("validated-cwd"));
        opts.validated_program = Some(validated);
        let command = build_pty_command("must-not-be-resolved", vec!["chat".to_owned()], &opts)
            .expect("build from validated program");
        assert_eq!(command.program, expected.display().to_string());
        assert_eq!(command.args, vec!["chat"]);
    }

    #[test]
    fn an_unusable_first_candidate_does_not_shadow_a_later_one() {
        use std::os::unix::fs::PermissionsExt as _;
        let first = temp_dir("shadow-first");
        let second = temp_dir("shadow-second");
        // No execute bit at all: refused for every user, root included.
        let unusable = first.join("agent");
        fs::write(&unusable, "#!/bin/sh\nexit 0\n").expect("write");
        fs::set_permissions(&unusable, fs::Permissions::from_mode(0o644)).expect("chmod");
        let usable = write_executable(&second, "agent");
        let path = std::env::join_paths([&first, &second]).expect("join");
        let resolved = with_path(Path::new(&path), || {
            (
                which_executable("agent"),
                resolve_binary("agent").expect("launch path resolves"),
                ValidatedLaunchProgram::resolve("agent"),
            )
        });
        assert_eq!(resolved.0.as_deref(), Some(usable.as_path()));
        assert_eq!(resolved.1, usable.display().to_string());
        assert_eq!(
            resolved.2.expect("validated").as_path(),
            usable.canonicalize().expect("canonical")
        );
    }

    #[test]
    fn empty_and_relative_path_entries_are_skipped_not_fatal() {
        let bin = temp_dir("skip-entries");
        let agent = write_executable(&bin, "agent");
        let absolute = bin.display().to_string();
        let filler = format!("/{}", "x".repeat(200));
        let long = format!("{}:{absolute}", vec![filler; 40].join(":"));
        for path in [
            format!("{absolute}:"),
            format!(":{absolute}"),
            format!("./node_modules/.bin:{absolute}"),
            long,
        ] {
            let resolved = with_path(Path::new(&path), || {
                (resolve_binary("agent"), which_executable("agent"))
            });
            assert_eq!(
                resolved.0.expect("launch path resolves"),
                agent.display().to_string(),
                "{path}"
            );
            assert_eq!(resolved.1.as_deref(), Some(agent.as_path()), "{path}");
        }
    }

    #[test]
    fn a_writable_candidate_is_never_launched_and_does_not_shadow_a_trusted_one() {
        use std::os::unix::fs::PermissionsExt as _;
        let first = temp_dir("writable-first");
        let second = temp_dir("writable-second");
        // Mode 0777 lets any account replace the file the daemon would run.
        let loose = write_executable(&first, "agent");
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o777)).expect("chmod");
        let trusted = write_executable(&second, "agent");
        let path = std::env::join_paths([&first, &second]).expect("join");
        let resolved = with_path(Path::new(&path), || {
            (which_executable("agent"), resolve_binary("agent"))
        });
        assert_eq!(resolved.0.as_deref(), Some(trusted.as_path()));
        assert_eq!(resolved.1.expect("resolves"), trusted.display().to_string());
        // Alone, the writable candidate does not resolve at all.
        let alone = with_path(first.as_path(), || resolve_binary("agent"));
        alone.expect_err("a writable candidate alone does not resolve");
    }

    #[test]
    fn the_launch_path_refuses_relative_programs_and_path_entries() {
        let cwd = temp_dir("launch-relative");
        let relative_dir = cwd.join("bin");
        fs::create_dir(&relative_dir).expect("create bin");
        write_executable(&relative_dir, "agent");
        write_executable(&cwd, "agent");
        let _guard = ENV_LOCK.lock().expect("env lock");
        let old_cwd = std::env::current_dir().expect("cwd");
        let old_path = std::env::var_os("PATH");
        std::env::set_current_dir(&cwd).expect("enter fixture cwd");
        let refused = [
            ("bin", "agent"),
            ("", "agent"),
            (".", "agent"),
            ("/usr/bin", "./agent"),
            ("/usr/bin", "bin/agent"),
        ]
        .map(|(path, program)| {
            std::env::set_var("PATH", path);
            (
                resolve_binary(program).is_err(),
                which_executable(program).is_none(),
            )
        });
        match old_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        std::env::set_current_dir(old_cwd).expect("restore cwd");
        for (index, (launch, probe)) in refused.iter().enumerate() {
            assert!(*launch && *probe, "case {index}");
        }
    }

    #[test]
    fn shell_launch_resolves_binary_and_preserves_opts() {
        let bin_dir = temp_dir("shell-bin");
        let shell = write_executable(&bin_dir, "shell");
        let cwd = temp_dir("shell-cwd");

        let command = with_path_and_shell(&bin_dir, "shell", || {
            ShellAdapter
                .launch(&launch_opts(cwd.clone()))
                .expect("shell launch command")
        });

        assert_eq!(command.program, shell.display().to_string());
        assert!(command.args.is_empty());
        assert_eq!(command.cwd, cwd);
        assert_eq!(command.cols, 120);
        assert_eq!(command.rows, 40);
        assert_eq!(
            command.env,
            vec![("POHUNEK_SESSION_ID".to_owned(), "s-42".to_owned())]
        );
    }

    #[test]
    fn adapters_return_expected_input_rules() {
        let shell = ShellAdapter.input_rules();
        assert!(!shell.bracketed_paste);
        assert_eq!(shell.submit_delay, Duration::ZERO);

        let codex = CodexAdapter.input_rules();
        assert!(codex.bracketed_paste);
        assert_eq!(codex.submit_delay, Duration::from_millis(150));

        let claude = ClaudeAdapter.input_rules();
        assert!(!claude.bracketed_paste);
        assert_eq!(claude.submit_delay, Duration::from_millis(150));

        let hermes = HermesAdapter.input_rules();
        assert!(hermes.bracketed_paste);
        assert_eq!(hermes.submit_delay, Duration::from_millis(150));
    }

    #[test]
    fn session_ref_id_accepts_valid_value_and_reports_kind() {
        let session = SessionRef::id("native-123").expect("id session ref");
        assert_eq!(session.kind(), SessionRefKind::Id);
        assert_eq!(session.value(), "native-123");
        // `new` is the id constructor.
        assert_eq!(SessionRef::new("native-123").expect("new"), session);
    }

    #[test]
    fn session_ref_id_rejects_empty_control_and_overlength() {
        assert_eq!(
            SessionRef::id("").expect_err("empty id rejected").code,
            "invalid_session_ref"
        );
        assert_eq!(
            SessionRef::id("bad\nid")
                .expect_err("control char rejected")
                .code,
            "invalid_session_ref"
        );
        let too_long = "a".repeat(513);
        assert_eq!(
            SessionRef::id(too_long)
                .expect_err("over-length id rejected")
                .code,
            "invalid_session_ref"
        );
    }

    #[test]
    fn session_ref_id_rejects_leading_dash_to_block_argv_flag_injection() {
        // A native id like `--dangerously-skip-permissions` must not become a
        // resume-argv flag.
        assert_eq!(
            SessionRef::id("--resume-evil")
                .expect_err("leading-dash id rejected")
                .code,
            "invalid_session_ref"
        );
        assert_eq!(
            SessionRef::id("-x")
                .expect_err("single-dash id rejected")
                .code,
            "invalid_session_ref"
        );
        // A dash elsewhere is fine (real native ids contain hyphens).
        SessionRef::id("abc-123-def").unwrap();
    }

    #[test]
    fn session_ref_path_accepts_absolute_path_and_reports_kind() {
        let session =
            SessionRef::path("/home/user/.claude/transcripts/abc.jsonl").expect("path session ref");
        assert_eq!(session.kind(), SessionRefKind::Path);
        assert_eq!(session.value(), "/home/user/.claude/transcripts/abc.jsonl");
    }

    #[test]
    fn session_ref_path_rejects_relative_empty_control_and_overlength() {
        assert_eq!(
            SessionRef::path("relative/path.jsonl")
                .expect_err("relative path rejected")
                .code,
            "invalid_session_ref"
        );
        assert_eq!(
            SessionRef::path("").expect_err("empty path rejected").code,
            "invalid_session_ref"
        );
        assert_eq!(
            SessionRef::path("/bad\npath")
                .expect_err("control char rejected")
                .code,
            "invalid_session_ref"
        );
        let too_long = format!("/{}", "a".repeat(4096));
        assert_eq!(
            SessionRef::path(too_long)
                .expect_err("over-length path rejected")
                .code,
            "invalid_session_ref"
        );
    }

    #[test]
    fn base_resume_template_defines_native_resume_modes() {
        // Claude → `--resume` flag, Codex → `resume` subcommand, both id-kind.
        // This is the single source of resume argv shape for built-in base kinds.
        let claude = base_resume_template(&protocol::AgentKind::Claude).expect("claude resumable");
        assert_eq!(claude.mode, ResumeMode::Flag);
        assert_eq!(claude.ref_kind, SessionRefKind::Id);
        let codex = base_resume_template(&protocol::AgentKind::Codex).expect("codex resumable");
        assert_eq!(codex.mode, ResumeMode::Subcommand);
        assert_eq!(codex.ref_kind, SessionRefKind::Id);
        let hermes = base_resume_template(&protocol::AgentKind::Hermes).expect("Hermes resumable");
        assert_eq!(hermes.mode, ResumeMode::Flag);
        assert_eq!(hermes.ref_kind, SessionRefKind::Id);
        // A shell has no native resume.
        assert!(base_resume_template(&protocol::AgentKind::Shell).is_none());
    }

    #[test]
    fn built_in_capabilities_keep_resume_and_fork_independent() {
        let shell = base_capabilities(&protocol::AgentKind::Shell);
        assert_eq!(shell.resume, None);
        assert_eq!(shell.fork, None);

        let codex = base_capabilities(&protocol::AgentKind::Codex);
        assert_eq!(
            codex.resume,
            base_resume_template(&protocol::AgentKind::Codex)
        );
        assert_eq!(codex.fork, None);

        let hermes = base_capabilities(&protocol::AgentKind::Hermes);
        assert_eq!(
            hermes.resume,
            base_resume_template(&protocol::AgentKind::Hermes)
        );
        assert_eq!(hermes.fork, None);

        let claude = base_capabilities(&protocol::AgentKind::Claude);
        assert_eq!(
            claude.resume,
            base_resume_template(&protocol::AgentKind::Claude)
        );
        assert_eq!(
            claude.fork,
            Some(ForkTemplate {
                resume: ResumeTemplate {
                    mode: ResumeMode::Flag,
                    ref_kind: SessionRefKind::Id,
                },
                mode: ForkMode::ClaudeSession,
            })
        );
    }

    #[test]
    fn fork_pty_command_preserves_claude_argv_without_resume_mode_coupling() {
        let bin_dir = temp_dir("template-fork-bin");
        write_executable(&bin_dir, "claude-sonnet");
        let cwd = temp_dir("template-fork-cwd");
        let session = SessionRef::id("native-123").expect("id ref");

        let command = with_path(&bin_dir, || {
            fork_pty_command_from_template(
                "claude-sonnet",
                vec!["--model".to_owned(), "sonnet".to_owned()],
                ForkTemplate {
                    resume: ResumeTemplate {
                        mode: ResumeMode::Flag,
                        ref_kind: SessionRefKind::Id,
                    },
                    mode: ForkMode::ClaudeSession,
                },
                &session,
                &launch_opts(cwd),
            )
            .expect("fork command")
        });

        assert_eq!(
            command.args,
            vec![
                "--model",
                "sonnet",
                "--resume",
                "native-123",
                "--fork-session",
            ]
        );
    }

    #[test]
    fn resume_pty_command_from_template_builds_argv_by_mode() {
        let bin_dir = temp_dir("template-resume-bin");
        write_executable(&bin_dir, "claude-sonnet");
        let cwd = temp_dir("template-resume-cwd");
        let session = SessionRef::id("native-123").expect("id ref");

        // Flag mode resumes with `--resume <id>`; the program is the snapshot's,
        // NOT the base adapter's "claude".
        let flag = with_path(&bin_dir, || {
            resume_pty_command_from_template(
                "claude-sonnet",
                Vec::new(),
                ResumeTemplate {
                    mode: ResumeMode::Flag,
                    ref_kind: SessionRefKind::Id,
                },
                &session,
                &launch_opts(cwd.clone()),
            )
            .expect("flag resume command")
        });
        assert!(flag.program.ends_with("claude-sonnet"));
        assert_eq!(flag.args, vec!["--resume", "native-123"]);

        // Subcommand mode resumes with `resume <id>`.
        let sub = with_path(&bin_dir, || {
            resume_pty_command_from_template(
                "claude-sonnet",
                Vec::new(),
                ResumeTemplate {
                    mode: ResumeMode::Subcommand,
                    ref_kind: SessionRefKind::Id,
                },
                &session,
                &launch_opts(cwd.clone()),
            )
            .expect("subcommand resume command")
        });
        assert_eq!(sub.args, vec!["resume", "native-123"]);
    }

    #[test]
    fn hermes_resume_keeps_reference_as_one_argument_after_chat() {
        let bin_dir = temp_dir("hermes-resume-bin");
        write_executable(&bin_dir, "hermes");
        let session = SessionRef::id("native id with spaces + symbols")
            .expect("spaces and non-control symbols are valid in opaque ids");

        let command = with_path(&bin_dir, || {
            resume_pty_command_from_template(
                "hermes",
                vec!["chat".to_owned()],
                base_resume_template(&protocol::AgentKind::Hermes).expect("Hermes resumes"),
                &session,
                &launch_opts(temp_dir("hermes-resume-cwd")),
            )
            .expect("Hermes resume command")
        });

        assert_eq!(
            command.args,
            vec!["chat", "--resume", "native id with spaces + symbols"]
        );
        assert!(!command
            .args
            .iter()
            .any(|arg| { matches!(arg.as_str(), "--continue" | "--pass-session-id") }));
    }

    #[test]
    fn resume_pty_command_from_template_carries_path_ref_value() {
        let bin_dir = temp_dir("template-path-bin");
        write_executable(&bin_dir, "myagent");
        let cwd = temp_dir("template-path-cwd");
        let session = SessionRef::path("/abs/session.jsonl").expect("path ref");

        let command = with_path(&bin_dir, || {
            resume_pty_command_from_template(
                "myagent",
                Vec::new(),
                ResumeTemplate {
                    mode: ResumeMode::Flag,
                    ref_kind: SessionRefKind::Path,
                },
                &session,
                &launch_opts(cwd),
            )
            .expect("path resume command")
        });
        assert_eq!(command.args, vec!["--resume", "/abs/session.jsonl"]);
    }

    #[test]
    fn resume_pty_command_from_template_preserves_frozen_profile_args() {
        let bin_dir = temp_dir("template-args-bin");
        write_executable(&bin_dir, "myagent");
        let cwd = temp_dir("template-args-cwd");
        let session = SessionRef::id("native-123").expect("id ref");

        let command = with_path(&bin_dir, || {
            resume_pty_command_from_template(
                "myagent",
                vec!["--model".to_owned(), "sonnet".to_owned()],
                ResumeTemplate {
                    mode: ResumeMode::Flag,
                    ref_kind: SessionRefKind::Id,
                },
                &session,
                &launch_opts(cwd),
            )
            .expect("resume command")
        });

        assert_eq!(
            command.args,
            vec!["--model", "sonnet", "--resume", "native-123"],
            "resume relaunch must preserve frozen profile args before resume argv"
        );
    }

    #[test]
    fn missing_agent_binary_returns_typed_error() {
        let empty_path = temp_dir("empty-path");
        let cwd = temp_dir("missing-cwd");

        let err = with_path(&empty_path, || {
            CodexAdapter
                .launch(&launch_opts(cwd))
                .expect_err("missing codex binary")
        });

        assert_eq!(err.class, ErrorClass::Runtime);
        assert_eq!(err.code, "agent_binary_missing");
        assert!(err.msg.contains("codex"));
        assert!(err.recover.is_some());
    }

    #[test]
    fn adapter_manifests_match_agent_specific_rules() {
        // Shell inherits the generic-shell manifest verbatim — assert it returns
        // that exact static instance (Manifest holds compiled Regex, so it is not
        // PartialEq; pointer identity is the meaningful check).
        assert!(
            std::ptr::eq(
                ShellAdapter.manifest(),
                crate::detect::generic_shell_manifest()
            ),
            "shell adapter must inherit the generic-shell manifest"
        );

        let codex = CodexAdapter
            .manifest()
            .match_context(
                &MatchContext::default()
                    .with_region_text(ManifestRegion::OscTitle, "Action Required"),
            )
            .expect("codex blocked title should match");
        assert_eq!(codex.activity, AgentActivity::Blocked);
        assert!(codex.visible_blocker);

        let claude = ClaudeAdapter
            .manifest()
            .match_context(&MatchContext::default().with_region_text(
                ManifestRegion::AfterLastHorizontalRule,
                "enter to select\nesc to cancel\n↑/↓ to navigate",
            ))
            .expect("claude selection form should match");
        assert_eq!(claude.activity, AgentActivity::Blocked);
        assert!(claude.visible_blocker);
    }
}
