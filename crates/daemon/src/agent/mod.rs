//! Agent launch primitives and the runtime host.
//!
//! The runtime host ([`host`]) holds one data-driven definition per runtime;
//! this module supplies the PTY launch command, input framing rules, native
//! session references and the executable-resolution policy those definitions
//! are launched with.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pohunek_platform::shell_env;
use protocol::{AgentActivity, ErrorClass, ProtocolError};
use serde::{Deserialize, Serialize};

pub mod host;
mod native_launch;
mod profile;

pub use native_launch::{
    NativeArg, NativeArgs, NativeLaunchError, NativeSessionLaunch, REFERENCE_PLACEHOLDER,
};
pub(crate) use profile::{ProfileRegistry, ResolvedAgent};

/// Shell launched when the host reports no usable login shell.
pub(crate) const FALLBACK_LOGIN_SHELL: &str = "/bin/sh";

/// Resolves the login shell from a raw `$SHELL` value.
///
/// The value is host input: it is used only when it is non-empty, at most
/// [`host::MAX_ARG_BYTES`] long and free of control characters, so it always
/// satisfies the runtime definition's program rules. Anything else, or an unset
/// value, resolves to [`FALLBACK_LOGIN_SHELL`]. Every consumer of the login
/// shell (registry source, shell launch, profile defaults, resume snapshots)
/// goes through here, so they cannot disagree.
pub(crate) fn resolve_login_shell(raw: Option<String>) -> String {
    raw.filter(|shell| {
        !shell.is_empty()
            && shell.len() <= host::MAX_ARG_BYTES
            && !shell.chars().any(char::is_control)
    })
    .unwrap_or_else(|| FALLBACK_LOGIN_SHELL.to_owned())
}

/// The login shell of this host's environment, resolved by [`resolve_login_shell`].
pub(crate) fn host_login_shell() -> String {
    resolve_login_shell(std::env::var("SHELL").ok())
}

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

/// The native-session launch spec of a built-in runtime kind, `None` for a
/// kind without native recovery or without an installed definition.
#[cfg(test)]
pub(crate) fn builtin_native_launch(kind: &protocol::AgentKind) -> Option<NativeSessionLaunch> {
    host::RuntimeHost::default()
        .resolve_kind(kind)
        .ok()
        .and_then(|definition| definition.native().cloned())
}

/// Input rules of the runtime `agent` names; a kind without an installed
/// definition gets unrestricted, unframed input.
pub(crate) fn input_rules_for_kind(
    host: &host::RuntimeHost,
    agent: &protocol::AgentKind,
) -> InputRules {
    host.resolve_kind(agent).map_or_else(
        |_unresolved| InputRules::unrestricted(false, Duration::ZERO),
        |definition| definition.input_rules(),
    )
}

/// Resolve `program` on `PATH` and build a PTY launch command in `opts`.
///
/// Shared by first launch ([`host::launch_command`]) and resume
/// ([`resume_pty_command_from_launch`]).
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
/// The `program` and the native-session `launch` spec come from the session's
/// launch-time snapshot, so a host profile that overrode the launch program or
/// the resume args resumes with exactly those values. `session_ref` must be of
/// the kind the spec names.
pub(crate) fn resume_pty_command_from_launch(
    program: &str,
    frozen_args: Vec<String>,
    launch: &NativeSessionLaunch,
    session_ref: &SessionRef,
    opts: &LaunchOpts,
) -> Result<LaunchCommand, ProtocolError> {
    let mut args = frozen_args;
    args.extend(launch.resume_argv(session_ref)?);
    build_pty_command(program, args, opts)
}

/// Build the PTY command that forks a native session from a frozen snapshot.
pub(crate) fn fork_pty_command_from_launch(
    program: &str,
    frozen_args: Vec<String>,
    launch: &NativeSessionLaunch,
    session_ref: &SessionRef,
    opts: &LaunchOpts,
) -> Result<LaunchCommand, ProtocolError> {
    let mut args = frozen_args;
    args.extend(launch.fork_argv(session_ref)?);
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

/// Resolve a helper program the daemon itself runs (`git`, the hook
/// interpreter) to the absolute path to spawn.
///
/// The lookup uses the shared trusted-executable policy, so a candidate another
/// account could replace is skipped, and spawning the returned path means the
/// kernel does no second `PATH` search. A program that is missing or untrusted
/// yields a message naming it, which callers surface like a failed spawn.
pub(crate) fn trusted_program(name: &str) -> Result<PathBuf, String> {
    which_executable(name)
        .ok_or_else(|| format!("{name} was not found, or is not trusted, on PATH"))
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
    use std::time::Duration;

    use pohunek_test_support::process_env::ProcessEnv;
    use protocol::{AgentActivity, ErrorClass};

    use super::host::{launch_command, RuntimeHost};
    use super::{
        build_pty_command, builtin_native_launch, fork_pty_command_from_launch, resolve_binary,
        resume_pty_command_from_launch, trusted_program, which_executable, LaunchCommand,
        LaunchOpts, NativeArgs, NativeSessionLaunch, SessionRef, SessionRefKind,
        ValidatedLaunchProgram,
    };
    use crate::detect::{ManifestRegion, MatchContext};

    fn launch_opts(cwd: impl AsRef<Path>) -> LaunchOpts {
        LaunchOpts {
            cwd: cwd.as_ref().to_path_buf(),
            cols: 120,
            rows: 40,
            env_extra: vec![("POHUNEK_SESSION_ID".to_owned(), "s-42".to_owned())],
            validated_program: None,
        }
    }

    fn temp_dir(tag: &str) -> crate::test_support::ScopedDir {
        crate::test_support::scoped_dir(&format!("pohunek-agent-test-{tag}-"))
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
        let mut env = ProcessEnv::lock();
        env.set("PATH", path);
        run()
    }

    fn with_path_and_shell<T>(path: &Path, shell: &str, run: impl FnOnce() -> T) -> T {
        let mut env = ProcessEnv::lock();
        env.set("PATH", path).set("SHELL", shell);
        run()
    }

    /// Launches the built-in runtime `name` the way a bare agent name does, with
    /// `$SHELL` as the shell program.
    fn launch_builtin(
        name: &str,
        opts: &LaunchOpts,
    ) -> Result<LaunchCommand, protocol::ProtocolError> {
        let host = RuntimeHost::from_host_environment();
        let runtime_id = protocol::RuntimeId::parse(name).expect("runtime id");
        let definition = host.resolve_id(&runtime_id).expect("built-in resolves");
        launch_command(&host, definition, opts)
    }

    #[test]
    fn codex_launch_resolves_binary_and_preserves_opts() {
        let bin_dir = temp_dir("codex-bin");
        let codex = write_executable(&bin_dir, "codex");
        let cwd = temp_dir("codex-cwd");

        let command = with_path(&bin_dir, || {
            launch_builtin("codex", &launch_opts(cwd.clone())).expect("codex launch command")
        });

        assert_eq!(command.program, codex.display().to_string());
        assert!(command.args.is_empty());
        assert_eq!(command.cwd, *cwd);
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
            launch_builtin("claude", &launch_opts(cwd.clone())).expect("claude launch command")
        });

        assert_eq!(command.program, claude.display().to_string());
        assert!(command.args.is_empty());
        assert_eq!(command.cwd, *cwd);
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
            launch_builtin("hermes", &launch_opts(cwd.clone())).expect("Hermes launch command")
        });

        assert_eq!(command.program, hermes.display().to_string());
        assert_eq!(command.args, vec!["chat"]);
        assert_eq!(command.cwd, *cwd);
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
        let _cwd_guard = ProcessEnv::lock();
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

        let cwd = temp_dir("validated-cwd");
        let mut opts = launch_opts(&cwd);
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

    /// Writes an executable `git` that prints `label` into `dir`.
    fn fake_git(dir: &Path, label: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("git");
        fs::write(&path, format!("#!/bin/sh\necho {label}\n")).expect("write git");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod git");
        path
    }

    #[test]
    fn helper_programs_run_the_trusted_candidate_never_an_unsafe_one_earlier_on_path() {
        let first = temp_dir("git-unsafe");
        let second = temp_dir("git-safe");
        // Mode 0777 lets any account replace the program the daemon would run.
        fake_git(&first, "unsafe", 0o777);
        let safe = fake_git(&second, "safe", 0o755);
        let path = std::env::join_paths([&first, &second]).expect("join");
        let repo = temp_dir("git-repo");
        let (detected, output, resolved) = with_path(Path::new(&path), || {
            let detected = crate::project::detect::git(&repo, &[]);
            let output = crate::worktree::git_command(&repo)
                .expect("a trusted git exists")
                .output()
                .expect("spawn git");
            (detected, output, trusted_program("git"))
        });
        let none = with_path(first.as_path(), || trusted_program("git"));
        assert_eq!(detected.as_deref(), Some("safe"));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "safe");
        assert_eq!(resolved, Ok(safe));
        // With only the unsafe candidate there is no git to run at all.
        assert!(none.expect_err("untrusted").contains("not trusted"));
    }

    #[test]
    fn doctor_capabilities_and_spawn_agree_on_an_unsafe_git() {
        use std::os::unix::fs::PermissionsExt as _;
        let loose_file = temp_dir("agree-file");
        let loose_dir = temp_dir("agree-dir");
        let safe_dir = temp_dir("agree-safe");
        fake_git(&loose_file, "unsafe", 0o777);
        fs::set_permissions(&loose_dir, fs::Permissions::from_mode(0o777)).expect("chmod");
        fake_git(&loose_dir, "unsafe", 0o755);
        let safe = fake_git(&safe_dir, "safe", 0o755);
        // What each of the three consults, for one `PATH`.
        let verdict = |path: &std::ffi::OsStr| {
            let doctor = hostcheck::binary_with_path("git", true, Some(path), "");
            let capability = hostcheck::resolve_executable("git", Some(path));
            let spawn = with_path(Path::new(path), || trusted_program("git").ok());
            (
                doctor.status == protocol::DoctorStatus::Ok,
                capability,
                spawn,
            )
        };
        for unsafe_only in [&loose_file, &loose_dir] {
            let (doctor_ok, capability, spawn) = verdict(unsafe_only.as_os_str());
            assert!(!doctor_ok, "doctor must not accept an unsafe git");
            assert_eq!(capability, None);
            assert_eq!(spawn, None);
        }
        let both = std::env::join_paths([&loose_file, &loose_dir, &safe_dir]).expect("join");
        let (doctor_ok, capability, spawn) = verdict(&both);
        assert!(doctor_ok);
        assert_eq!(capability, Some(safe.clone()));
        assert_eq!(spawn, Some(safe));
    }

    #[test]
    fn the_launch_path_refuses_relative_programs_and_path_entries() {
        let cwd = temp_dir("launch-relative");
        let relative_dir = cwd.join("bin");
        fs::create_dir(&relative_dir).expect("create bin");
        write_executable(&relative_dir, "agent");
        write_executable(&cwd, "agent");
        let mut env = ProcessEnv::lock();
        let old_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&cwd).expect("enter fixture cwd");
        let refused = [
            ("bin", "agent"),
            ("", "agent"),
            (".", "agent"),
            ("/usr/bin", "./agent"),
            ("/usr/bin", "bin/agent"),
        ]
        .map(|(path, program)| {
            env.set("PATH", path);
            (
                resolve_binary(program).is_err(),
                which_executable(program).is_none(),
            )
        });
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
            launch_builtin("shell", &launch_opts(cwd.clone())).expect("shell launch command")
        });

        assert_eq!(command.program, shell.display().to_string());
        assert!(command.args.is_empty());
        assert_eq!(command.cwd, *cwd);
        assert_eq!(command.cols, 120);
        assert_eq!(command.rows, 40);
        assert_eq!(
            command.env,
            vec![("POHUNEK_SESSION_ID".to_owned(), "s-42".to_owned())]
        );
    }

    #[test]
    fn built_in_runtimes_return_expected_input_rules() {
        let host = RuntimeHost::default();
        let rules = |kind: protocol::AgentKind| super::input_rules_for_kind(&host, &kind);
        let shell = rules(protocol::AgentKind::Shell);
        assert!(!shell.bracketed_paste);
        assert_eq!(shell.submit_delay, Duration::ZERO);

        let codex = rules(protocol::AgentKind::Codex);
        assert!(codex.bracketed_paste);
        assert_eq!(codex.submit_delay, Duration::from_millis(150));

        let claude = rules(protocol::AgentKind::Claude);
        assert!(!claude.bracketed_paste);
        assert_eq!(claude.submit_delay, Duration::from_millis(150));

        let hermes = rules(protocol::AgentKind::Hermes);
        assert!(hermes.bracketed_paste);
        assert_eq!(hermes.submit_delay, Duration::from_millis(150));
        assert!(!hermes.allows_while_blocked());

        let unresolved = rules(protocol::AgentKind::Unknown("acme".to_owned()));
        assert!(!unresolved.bracketed_paste);
        assert_eq!(unresolved.submit_delay, Duration::ZERO);
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

    /// Render a spec's resume/fork argv for an id reference, panicking on error.
    fn id_argv(launch: &NativeSessionLaunch, id: &str) -> (Vec<String>, Option<Vec<String>>) {
        let reference = SessionRef::id(id).expect("id reference");
        (
            launch.resume_argv(&reference).expect("resume argv"),
            launch.fork_argv(&reference).ok(),
        )
    }

    #[test]
    fn built_in_native_launch_specs_pin_exact_argv() {
        let claude = builtin_native_launch(&protocol::AgentKind::Claude).expect("claude recovers");
        assert_eq!(claude.reference_kind(), SessionRefKind::Id);
        assert_eq!(
            id_argv(&claude, "native-1"),
            (
                vec!["--resume".to_owned(), "native-1".to_owned()],
                Some(vec![
                    "--resume".to_owned(),
                    "native-1".to_owned(),
                    "--fork-session".to_owned(),
                ]),
            )
        );

        let codex = builtin_native_launch(&protocol::AgentKind::Codex).expect("codex recovers");
        assert_eq!(codex.reference_kind(), SessionRefKind::Id);
        assert_eq!(
            id_argv(&codex, "native-1"),
            (vec!["resume".to_owned(), "native-1".to_owned()], None)
        );

        let hermes = builtin_native_launch(&protocol::AgentKind::Hermes).expect("hermes recovers");
        assert_eq!(hermes.reference_kind(), SessionRefKind::Id);
        assert_eq!(
            id_argv(&hermes, "native-1"),
            (vec!["--resume".to_owned(), "native-1".to_owned()], None)
        );

        assert_eq!(builtin_native_launch(&protocol::AgentKind::Shell), None);
        assert_eq!(
            builtin_native_launch(&protocol::AgentKind::Unknown("x".to_owned())),
            None
        );
    }

    #[test]
    fn codex_and_hermes_report_fork_unsupported() {
        let reference = SessionRef::id("native-1").expect("id reference");
        for kind in [protocol::AgentKind::Codex, protocol::AgentKind::Hermes] {
            let launch = builtin_native_launch(&kind).expect("recovers");
            assert!(!launch.supports_fork());
            assert_eq!(
                launch.fork_argv(&reference).expect_err("no fork").code,
                "agent_fork_unsupported"
            );
        }
    }

    #[test]
    fn fork_pty_command_preserves_claude_argv_with_frozen_args() {
        let bin_dir = temp_dir("template-fork-bin");
        write_executable(&bin_dir, "claude-sonnet");
        let cwd = temp_dir("template-fork-cwd");
        let session = SessionRef::id("native-123").expect("id ref");
        let launch = builtin_native_launch(&protocol::AgentKind::Claude).expect("claude recovers");

        let command = with_path(&bin_dir, || {
            fork_pty_command_from_launch(
                "claude-sonnet",
                vec!["--model".to_owned(), "sonnet".to_owned()],
                &launch,
                &session,
                &launch_opts(&cwd),
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
    fn fork_pty_command_refuses_a_spec_without_fork() {
        let cwd = temp_dir("template-fork-none-cwd");
        let session = SessionRef::id("native-123").expect("id ref");
        let launch = builtin_native_launch(&protocol::AgentKind::Codex).expect("codex recovers");

        let error = fork_pty_command_from_launch(
            "codex",
            Vec::new(),
            &launch,
            &session,
            &launch_opts(&cwd),
        )
        .expect_err("codex cannot fork");
        assert_eq!(error.code, "agent_fork_unsupported");
    }

    #[test]
    fn resume_pty_command_from_launch_builds_exact_builtin_argv() {
        let bin_dir = temp_dir("template-resume-bin");
        write_executable(&bin_dir, "claude-sonnet");
        let cwd = temp_dir("template-resume-cwd");
        let session = SessionRef::id("native-123").expect("id ref");

        // The program is the snapshot's, NOT the base adapter's "claude".
        let claude = builtin_native_launch(&protocol::AgentKind::Claude).expect("claude recovers");
        let flag = with_path(&bin_dir, || {
            resume_pty_command_from_launch(
                "claude-sonnet",
                Vec::new(),
                &claude,
                &session,
                &launch_opts(cwd.clone()),
            )
            .expect("claude resume command")
        });
        assert!(flag.program.ends_with("claude-sonnet"));
        assert_eq!(flag.args, vec!["--resume", "native-123"]);

        let codex = builtin_native_launch(&protocol::AgentKind::Codex).expect("codex recovers");
        let sub = with_path(&bin_dir, || {
            resume_pty_command_from_launch(
                "claude-sonnet",
                Vec::new(),
                &codex,
                &session,
                &launch_opts(cwd.clone()),
            )
            .expect("codex resume command")
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
            resume_pty_command_from_launch(
                "hermes",
                vec!["chat".to_owned()],
                &builtin_native_launch(&protocol::AgentKind::Hermes).expect("Hermes resumes"),
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
    fn resume_keeps_shell_metacharacter_references_as_one_argv_element() {
        let bin_dir = temp_dir("template-meta-bin");
        write_executable(&bin_dir, "myagent");
        let cwd = temp_dir("template-meta-cwd");
        let launch = builtin_native_launch(&protocol::AgentKind::Claude).expect("claude recovers");
        let hostile_id = "a b;$(touch pwned)|`x` 'q' \"z\" *";
        let hostile_path = "/work/a b/$(touch pwned);|.jsonl";

        let by_id = with_path(&bin_dir, || {
            resume_pty_command_from_launch(
                "myagent",
                Vec::new(),
                &launch,
                &SessionRef::id(hostile_id).expect("id"),
                &launch_opts(&cwd),
            )
            .expect("id resume")
        });
        assert_eq!(
            by_id.args,
            vec!["--resume".to_owned(), hostile_id.to_owned()]
        );

        let path_launch = NativeSessionLaunch::new(
            SessionRefKind::Path,
            NativeArgs::from_template(&["--session", "{reference}"]).expect("resume"),
            Some(NativeArgs::from_template(&["--fork", "{reference}"]).expect("fork")),
        );
        let path = SessionRef::path(hostile_path).expect("path");
        let (resume, fork) = with_path(&bin_dir, || {
            (
                resume_pty_command_from_launch(
                    "myagent",
                    Vec::new(),
                    &path_launch,
                    &path,
                    &launch_opts(&cwd),
                )
                .expect("path resume"),
                fork_pty_command_from_launch(
                    "myagent",
                    Vec::new(),
                    &path_launch,
                    &path,
                    &launch_opts(&cwd),
                )
                .expect("path fork"),
            )
        });
        assert_eq!(
            resume.args,
            vec!["--session".to_owned(), hostile_path.to_owned()]
        );
        assert_eq!(
            fork.args,
            vec!["--fork".to_owned(), hostile_path.to_owned()]
        );
    }

    #[test]
    fn resume_pty_command_from_launch_refuses_a_reference_of_the_wrong_kind() {
        let cwd = temp_dir("template-kind-cwd");
        let launch = builtin_native_launch(&protocol::AgentKind::Claude).expect("claude recovers");
        let path = SessionRef::path("/abs/session.jsonl").expect("path ref");

        let error = resume_pty_command_from_launch(
            "claude",
            Vec::new(),
            &launch,
            &path,
            &launch_opts(&cwd),
        )
        .expect_err("id spec rejects a path reference");
        assert_eq!(error.code, "native_reference_kind_mismatch");
    }

    #[test]
    fn resume_pty_command_from_launch_carries_path_ref_value() {
        let bin_dir = temp_dir("template-path-bin");
        write_executable(&bin_dir, "myagent");
        let cwd = temp_dir("template-path-cwd");
        let session = SessionRef::path("/abs/session.jsonl").expect("path ref");
        let launch = NativeSessionLaunch::new(
            SessionRefKind::Path,
            NativeArgs::from_template(&["--resume", "{reference}"]).expect("resume"),
            None,
        );

        let command = with_path(&bin_dir, || {
            resume_pty_command_from_launch(
                "myagent",
                Vec::new(),
                &launch,
                &session,
                &launch_opts(&cwd),
            )
            .expect("path resume command")
        });
        assert_eq!(command.args, vec!["--resume", "/abs/session.jsonl"]);
    }

    #[test]
    fn resume_pty_command_from_launch_preserves_frozen_profile_args() {
        let bin_dir = temp_dir("template-args-bin");
        write_executable(&bin_dir, "myagent");
        let cwd = temp_dir("template-args-cwd");
        let session = SessionRef::id("native-123").expect("id ref");
        let launch = builtin_native_launch(&protocol::AgentKind::Claude).expect("claude recovers");

        let command = with_path(&bin_dir, || {
            resume_pty_command_from_launch(
                "myagent",
                vec!["--model".to_owned(), "sonnet".to_owned()],
                &launch,
                &session,
                &launch_opts(&cwd),
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
            launch_builtin("codex", &launch_opts(&cwd)).expect_err("missing codex binary")
        });

        assert_eq!(err.class, ErrorClass::Runtime);
        assert_eq!(err.code, "agent_binary_missing");
        assert!(err.msg.contains("codex"));
        assert!(err.recover.is_some());
    }

    #[test]
    fn built_in_manifests_match_agent_specific_rules() {
        let manifest = |kind: protocol::AgentKind| {
            (**super::host::RuntimeHost::default()
                .resolve_kind(&kind)
                .expect("built-in resolves")
                .manifest())
            .clone()
        };
        // Manifest holds compiled Regex and is not PartialEq; its Debug form is
        // the structural comparison.
        assert_eq!(
            format!("{:?}", manifest(protocol::AgentKind::Shell)),
            format!("{:?}", crate::detect::generic_shell_manifest()),
            "shell must inherit the generic-shell manifest"
        );

        let codex = manifest(protocol::AgentKind::Codex)
            .match_context(
                &MatchContext::default()
                    .with_region_text(ManifestRegion::OscTitle, "Action Required"),
            )
            .expect("codex blocked title should match");
        assert_eq!(codex.activity, AgentActivity::Blocked);
        assert!(codex.visible_blocker);

        let claude = manifest(protocol::AgentKind::Claude)
            .match_context(&MatchContext::default().with_region_text(
                ManifestRegion::AfterLastHorizontalRule,
                "enter to select\nesc to cancel\n↑/↓ to navigate",
            ))
            .expect("claude selection form should match");
        assert_eq!(claude.activity, AgentActivity::Blocked);
        assert!(claude.visible_blocker);
    }
}
