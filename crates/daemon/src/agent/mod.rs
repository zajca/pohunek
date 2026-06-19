//! Agent adapter trait and built-in PTY/TUI agent adapters.
//!
//! Per `docs/plan-phase-1.md` "Agent Adapter Boundary": a small trait per agent
//! carrying launch argv/env/cwd, input rules (Claude Ink submit-delay quirk),
//! the state manifest, and the resume command.

use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::{ErrorClass, ProtocolError};

use crate::detect::Manifest;
use crate::pty::PtyCommand;

mod claude;
mod codex;

pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;

/// Default submit delay for Claude Code's Ink TUI.
pub const DEFAULT_CLAUDE_SUBMIT_DELAY: Duration = Duration::from_millis(150);

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
}

/// Input framing rules for programmatic prompt injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRules {
    /// Whether prompt text should be wrapped in bracketed paste markers.
    pub bracketed_paste: bool,
    /// Delay before sending the submit byte as a separate write.
    pub submit_delay: Duration,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            return Err(invalid_session_ref(
                "native session id cannot be empty",
            ));
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
            return Err(invalid_session_ref(
                "native session path cannot be empty",
            ));
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
            return Err(invalid_session_ref(
                "native session path must be absolute",
            ));
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

/// Command argv produced by a resume builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommand {
    /// Program name.
    pub program: String,
    /// Program arguments excluding argv[0].
    pub args: Vec<String>,
}

impl AgentCommand {
    fn new(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
        }
    }
}

/// Thin per-agent adapter for launch, input, and resume behavior.
pub trait AgentAdapter: std::fmt::Debug + Send + Sync {
    /// Stable adapter id.
    fn id(&self) -> &'static str;
    /// Build the PTY launch command, resolving the executable from `PATH`.
    fn launch(&self, opts: &LaunchOpts) -> Result<PtyCommand, ProtocolError>;
    /// Programmatic input injection rules.
    fn input_rules(&self) -> InputRules;
    /// Agent-specific activity manifest.
    fn manifest(&self) -> &'static Manifest;
    /// Build a native resume command argv. M6 only builds this; M7 wires resume.
    fn resume(&self, session_ref: &SessionRef) -> AgentCommand;
}

fn launch_command(
    program: &'static str,
    args: Vec<String>,
    opts: &LaunchOpts,
) -> Result<PtyCommand, ProtocolError> {
    build_pty_command(program, args, opts)
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
) -> Result<PtyCommand, ProtocolError> {
    Ok(PtyCommand {
        program: resolve_binary(program)?,
        args,
        env: opts.env_extra.clone(),
        cwd: opts.cwd.clone(),
        cols: opts.cols,
        rows: opts.rows,
    })
}

/// Build the PTY command that resumes an existing agent session.
///
/// Combines the M6 resume-argv builder (`claude --resume <id>` /
/// `codex resume <id>`) with `PATH` resolution and the launch env in `opts`, so
/// the daemon can relaunch-and-resume a session after a restart.
///
/// # Errors
///
/// Returns a typed error for `AgentKind::Shell` (shells have no resume argv) or
/// when the agent binary is not on `PATH`.
pub fn resume_pty_command(
    agent: protocol::AgentKind,
    session_ref: &SessionRef,
    opts: &LaunchOpts,
) -> Result<PtyCommand, ProtocolError> {
    let command = match agent {
        protocol::AgentKind::Claude => ClaudeAdapter.resume(session_ref),
        protocol::AgentKind::Codex => CodexAdapter.resume(session_ref),
        protocol::AgentKind::Shell => {
            return Err(ProtocolError::new(
                ErrorClass::Runtime,
                "agent_not_resumable",
                "shell sessions cannot be resumed",
                None,
            ));
        }
    };
    build_pty_command(&command.program, command.args, opts)
}

fn resume_command(program: &'static str, args: Vec<String>) -> AgentCommand {
    AgentCommand::new(program, args)
}

fn resolve_binary(name: &str) -> Result<String, ProtocolError> {
    let path = std::env::var_os("PATH").ok_or_else(|| missing_binary(name))?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }

    Err(missing_binary(name))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{LazyLock, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use protocol::{AgentActivity, ErrorClass};

    use super::{AgentAdapter, ClaudeAdapter, CodexAdapter, LaunchOpts, SessionRef, SessionRefKind};
    use crate::detect::{ManifestRegion, MatchContext};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn launch_opts(cwd: PathBuf) -> LaunchOpts {
        LaunchOpts {
            cwd,
            cols: 120,
            rows: 40,
            env_extra: vec![("ZAGENTMESH_SESSION_ID".to_owned(), "s-42".to_owned())],
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "zagentmesh-agent-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
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
        }

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
            vec![("ZAGENTMESH_SESSION_ID".to_owned(), "s-42".to_owned())]
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
            vec![("ZAGENTMESH_SESSION_ID".to_owned(), "s-42".to_owned())]
        );
    }

    #[test]
    fn adapters_return_expected_input_rules() {
        let codex = CodexAdapter.input_rules();
        assert!(codex.bracketed_paste);
        assert_eq!(codex.submit_delay, Duration::ZERO);

        let claude = ClaudeAdapter.input_rules();
        assert!(!claude.bracketed_paste);
        assert_eq!(claude.submit_delay, Duration::from_millis(150));
    }

    #[test]
    fn resume_builders_match_native_agent_argv() {
        let session = SessionRef::new("native-123").expect("session ref");

        let codex = CodexAdapter.resume(&session);
        assert_eq!(codex.program, "codex");
        assert_eq!(codex.args, vec!["resume", "native-123"]);

        let claude = ClaudeAdapter.resume(&session);
        assert_eq!(claude.program, "claude");
        assert_eq!(claude.args, vec!["--resume", "native-123"]);
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
            SessionRef::id("-x").expect_err("single-dash id rejected").code,
            "invalid_session_ref"
        );
        // A dash elsewhere is fine (real native ids contain hyphens).
        assert!(SessionRef::id("abc-123-def").is_ok());
    }

    #[test]
    fn session_ref_path_accepts_absolute_path_and_reports_kind() {
        let session = SessionRef::path("/home/user/.claude/transcripts/abc.jsonl")
            .expect("path session ref");
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
    fn resume_argv_carries_path_kind_value() {
        let session = SessionRef::path("/abs/session.jsonl").expect("path session ref");
        let claude = ClaudeAdapter.resume(&session);
        assert_eq!(claude.args, vec!["--resume", "/abs/session.jsonl"]);
        let codex = CodexAdapter.resume(&session);
        assert_eq!(codex.args, vec!["resume", "/abs/session.jsonl"]);
    }

    #[test]
    fn resume_pty_command_resolves_binary_and_builds_resume_argv() {
        let bin_dir = temp_dir("resume-claude-bin");
        write_executable(&bin_dir, "claude");
        let cwd = temp_dir("resume-claude-cwd");
        let session = SessionRef::id("native-123").expect("session ref");

        let command = with_path(&bin_dir, || {
            super::resume_pty_command(
                protocol::AgentKind::Claude,
                &session,
                &launch_opts(cwd.clone()),
            )
            .expect("resume pty command")
        });

        assert!(command.program.ends_with("claude"));
        assert_eq!(command.args, vec!["--resume", "native-123"]);
        assert_eq!(command.cwd, cwd);
        assert_eq!(
            command.env,
            vec![("ZAGENTMESH_SESSION_ID".to_owned(), "s-42".to_owned())]
        );
    }

    #[test]
    fn resume_pty_command_rejects_shell_agent() {
        let cwd = temp_dir("resume-shell-cwd");
        let session = SessionRef::id("native-123").expect("session ref");
        let err = super::resume_pty_command(
            protocol::AgentKind::Shell,
            &session,
            &launch_opts(cwd),
        )
        .expect_err("shell is not resumable");
        assert_eq!(err.code, "agent_not_resumable");
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
