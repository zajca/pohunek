//! Typed CLI errors.
//!
//! These cover CLI-side failures: missing env, local bootstrap failures, daemon
//! protocol errors constructed by CLI logic, and setup/runtime I/O. SDK transport
//! failures are wrapped through [`CliError::Client`]. Errors are rendered for
//! humans on stderr and, where a command supports `--json`, can be surfaced as
//! structured error output.
//!
//! Argument-parse failures from clap are handled here too: under `--json` they
//! are mapped to the same `{class, code, msg, recover?}` envelope (see
//! [`render_clap_error`]) so that *every* failure a script can hit — including a
//! mis-typed command — is machine-readable, not just the ones that occur after a
//! successful parse.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use pohunek_client::ClientError as SdkClientError;
use protocol::{ErrorClass, ProtocolError};

/// CLI error.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// A required environment variable is missing (fail fast, no invented path).
    #[error("required environment variable {var} is not set (no safe default exists)")]
    MissingEnv {
        /// The missing variable name.
        var: String,
    },

    /// The daemon socket could not be reached (likely not running).
    ///
    /// The recovery hint ("start the daemon …") is surfaced uniformly through
    /// [`CliError::recover_hint`], not embedded in this message, so human and
    /// `--json` output render hints the same way.
    #[error("cannot reach the daemon at {socket}: {source}")]
    DaemonUnreachable {
        /// The socket path that was dialed.
        socket: PathBuf,
        /// Underlying connection error.
        #[source]
        source: io::Error,
    },

    /// The daemon returned a typed protocol error.
    #[error("daemon error: {0}")]
    Protocol(#[from] ProtocolError),

    /// The SDK client layer returned a typed client error.
    #[error(transparent)]
    Client(#[from] SdkClientError),

    /// Prompt rendering failed before any daemon request was made.
    #[error(transparent)]
    Prompt(#[from] pohunek_prompt::Error),

    /// A `session new --meta` key was supplied more than once. Caught client-side
    /// (before any connection is dialed) because metadata travels as a flat map:
    /// a repeated key would otherwise silently collapse to whichever occurrence
    /// is applied last, which is a confusing outcome for a caller-visible flag.
    #[error("duplicate --meta key {key:?} (each key may be set once)")]
    DuplicateMetaKey {
        /// The metadata key that was named more than once.
        key: String,
    },

    /// Input provided through stdin violated the CLI's bounded text contract.
    #[error("invalid stdin input: {detail}")]
    InvalidStdinInput {
        /// Payload-free explanation suitable for diagnostics and logs.
        detail: String,
    },

    /// Observation arguments failed the protocol's strict constructor checks.
    #[error("invalid observation arguments: {detail}")]
    InvalidObservation {
        /// Payload-free validation message.
        detail: String,
    },

    /// An exact display name identified more than one logical session.
    #[error("session name is ambiguous; matching session ids: {candidates}")]
    AmbiguousSessionName {
        /// Stable, sorted comma-separated candidate ids.
        candidates: String,
    },

    /// A long-running observation was cancelled by a local process signal.
    #[error("operation cancelled by process signal")]
    Cancelled,

    /// A remote `session new` named no target. No filesystem path crosses the
    /// wire to another host, so a remote session must be referenced by `--project`
    /// (or, for first-introduction, `--repo` with a path valid on that host). Fails
    /// fast before any connection is dialed (design Decision 1).
    #[error(
        "starting a session on a remote host requires a --project reference \
         (or --repo with a path valid on that host)"
    )]
    RemoteTargetRequired,

    /// A remote `project add` named no PATH. A local path is meaningless on
    /// another host, so a remote add must give a path valid on that host; fails
    /// fast before any connection is dialed.
    #[error("adding a project on a remote host requires a PATH valid on that host")]
    RemoteAddPathRequired,

    /// `assistant --degraded` was requested against a remote host. Degraded mode
    /// materializes the snapshot in a *local* runtime directory and embeds that
    /// local path into the opening prompt; a remote agent cannot read a path on
    /// the client's filesystem. Degraded is the local fallback when bundle
    /// materialization fails — on a remote host the remote daemon owns
    /// materialization, so this combination is rejected before any dial.
    #[error(
        "--degraded is not supported for a remote host '{host}': the snapshot would be \
         materialized locally and unreadable by the remote agent"
    )]
    DegradedRemoteUnsupported {
        /// The remote host the degraded launch targeted.
        host: String,
    },

    /// A remote `session new` was requested under `--json` without `--yes`. The
    /// machine path must not block on an interactive prompt, so it fails fast and
    /// asks the caller to pass `--yes` explicitly.
    #[error("starting a session on a remote host requires explicit confirmation: pass --yes")]
    RemoteConfirmationRequired,

    /// The interactive confirmation for a remote `session new` was declined.
    #[error("remote session on host '{host}' was not confirmed")]
    RemoteConfirmationDeclined {
        /// The host the session would have been started on.
        host: String,
    },

    /// Failed to spawn the daemon process.
    #[error("failed to start daemon: {0}")]
    Spawn(String),

    /// Generic I/O error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// JSON (de)serialization error at the client edge.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl CliError {
    /// Structured, serializable representation of this error.
    ///
    /// Used for `--json` error output and as the single source of recovery hints.
    /// Every variant maps to a stable `{class, code, msg, recover?}` shape so a
    /// script can branch on `code`; a daemon-returned [`CliError::Protocol`]
    /// passes through unchanged (its class/code/recover are already canonical).
    #[expect(
        clippy::too_many_lines,
        reason = "one exhaustive mapping keeps every CLI error code and recovery hint centralized"
    )]
    pub(crate) fn to_protocol_error(&self) -> ProtocolError {
        match self {
            CliError::Protocol(err) => err.clone(),
            CliError::Client(err) => err.to_protocol_error(),
            CliError::Prompt(err) => ProtocolError::new(
                ErrorClass::Configuration,
                "prompt_render_failed",
                err.to_string(),
                None,
            ),
            CliError::MissingEnv { var } => ProtocolError::new(
                ErrorClass::Configuration,
                "missing_env",
                format!("required environment variable {var} is not set (no safe default exists)"),
                None,
            ),
            CliError::DaemonUnreachable { socket, source } => ProtocolError::new(
                ErrorClass::Daemon,
                "daemon_unreachable",
                format!("cannot reach the daemon at {}: {source}", socket.display()),
                Some("start the daemon with `pohunek daemon start`".to_owned()),
            ),
            CliError::DuplicateMetaKey { key } => ProtocolError::new(
                ErrorClass::Configuration,
                // Same stable code as a clap usage failure: from a script's
                // perspective this is the same family of mistake ("I mis-invoked
                // the CLI"), just caught after clap's own parse succeeds.
                "cli_usage",
                format!("duplicate --meta key {key:?} (each key may be set once)"),
                Some("pass --meta only once per key; a repeated key would be ambiguous".to_owned()),
            ),
            CliError::InvalidStdinInput { detail } => ProtocolError::new(
                ErrorClass::Configuration,
                "cli_usage",
                format!("invalid stdin input: {detail}"),
                Some("pass bounded UTF-8 text without disallowed control characters".to_owned()),
            ),
            CliError::InvalidObservation { detail } => ProtocolError::new(
                ErrorClass::Configuration,
                "cli_usage",
                format!("invalid observation arguments: {detail}"),
                Some(
                    "check cursor/runtime pairing, nonzero limits, and wait predicates".to_owned(),
                ),
            ),
            CliError::AmbiguousSessionName { candidates } => ProtocolError::new(
                ErrorClass::Configuration,
                "ambiguous_session_name",
                format!("session name is ambiguous; matching session ids: {candidates}"),
                Some("retry with one of the listed full session ids".to_owned()),
            ),
            CliError::Cancelled => ProtocolError::new(
                ErrorClass::Runtime,
                "cancelled",
                "operation cancelled by process signal".to_owned(),
                Some("retry the bounded operation when ready".to_owned()),
            ),
            CliError::RemoteTargetRequired => ProtocolError::new(
                ErrorClass::Configuration,
                "remote_target_required",
                "starting a session on a remote host requires a --project reference".to_owned(),
                Some(
                    "pass --project <id|label> from `pohunek --host <host> project list` \
                     (or --repo with a path valid on that host the first time)"
                        .to_owned(),
                ),
            ),
            CliError::RemoteAddPathRequired => ProtocolError::new(
                ErrorClass::Configuration,
                "remote_add_path_required",
                "adding a project on a remote host requires a PATH valid on that host".to_owned(),
                Some("pass `pohunek --host <host> project add <path-on-that-host>`".to_owned()),
            ),
            CliError::DegradedRemoteUnsupported { host } => ProtocolError::new(
                ErrorClass::Configuration,
                "degraded_remote_unsupported",
                format!(
                    "--degraded is not supported for remote host '{host}': the snapshot would \
                     be materialized locally and unreadable by the remote agent"
                ),
                Some(
                    "drop --degraded for the remote launch (the remote daemon materializes its \
                     own bundle), or run the degraded launch locally"
                        .to_owned(),
                ),
            ),
            CliError::RemoteConfirmationRequired => ProtocolError::new(
                ErrorClass::Configuration,
                "confirmation_required",
                "starting a session on a remote host requires explicit confirmation".to_owned(),
                Some("re-run with `--yes` to confirm the remote session".to_owned()),
            ),
            CliError::RemoteConfirmationDeclined { host } => ProtocolError::new(
                ErrorClass::Configuration,
                "confirmation_declined",
                format!("remote session on host '{host}' was not confirmed"),
                Some("re-run and confirm, or pass `--yes` to skip the prompt".to_owned()),
            ),
            CliError::Spawn(msg) => ProtocolError::new(
                ErrorClass::Daemon,
                "daemon_spawn_failed",
                format!("failed to start daemon: {msg}"),
                None,
            ),
            CliError::Io(err) => ProtocolError::new(
                ErrorClass::Runtime,
                "io_error",
                format!("io error: {err}"),
                None,
            ),
            CliError::Json(err) => ProtocolError::new(
                ErrorClass::Daemon,
                "json_error",
                format!("json error: {err}"),
                None,
            ),
        }
    }

    /// The recovery hint to surface beneath this error, when one applies.
    pub(crate) fn recover_hint(&self) -> Option<String> {
        self.to_protocol_error().recover
    }
}

/// Human-readable error text: the message, then an optional `hint:` line.
///
/// Returned as a string (rather than printed inline) so it is unit-testable;
/// [`render`] writes it to stderr.
pub(crate) fn human_error_text(err: &CliError) -> String {
    let mut text = format!("pohunek: {err}\n");
    if let Some(hint) = err.recover_hint() {
        let _ = writeln!(text, "hint: {hint}");
    }
    text
}

/// Render a CLI error for the user.
///
/// Under `--json`, emits exactly one structured JSON document
/// (`{class, code, msg, recover?}`) to stdout so a script gets a single parseable
/// document and can branch on `code`. Otherwise writes a human message — plus any
/// recovery hint — to stderr. The caller exits non-zero either way.
pub(crate) fn render(err: &CliError, json: bool) {
    if json {
        print_json_error(&err.to_protocol_error());
    } else {
        eprint!("{}", human_error_text(err));
    }
}

/// Print a structured error document (`{class, code, msg, recover?}`) to stdout.
///
/// The single `--json` error sink: every JSON failure path (typed [`CliError`]
/// and clap usage errors alike) funnels through here so automation always
/// receives exactly one parseable document on stdout and nothing human leaks.
fn print_json_error(err: &ProtocolError) {
    match crate::commands::render_json_error(err) {
        Ok(doc) => print!("{doc}"),
        // Serializing our own typed error cannot fail in practice; fall back
        // to a minimal hand-built document rather than printing nothing.
        Err(_) => println!(
            "{}",
            serde_json::json!({
                "cli_version": env!("CARGO_PKG_VERSION"),
                "protocol": protocol::SUPPORTED_PROTOCOL_VERSIONS,
                "err": {
                    "class": "daemon",
                    "code": "serialize_failed",
                    "msg": "failed to serialize error"
                }
            })
        ),
    }
}

/// Process exit code for a CLI usage error.
///
/// Mirrors clap's own usage exit status (`2`) so that toggling `--json` changes
/// only the output *format*, never the exit code a script observes.
const CLI_USAGE_EXIT_CODE: u8 = 2;

/// Whether a raw argv requested `--json`.
///
/// `--json` is a per-command flag, so on a *successful* parse we read it from the
/// typed [`crate::Commands`]. But a clap parse failure aborts before a typed
/// `Cli` exists, yet we still want to honor `--json` when rendering that failure.
/// We therefore scan the raw arguments. Scanning stops at the `--` end-of-options
/// separator so a literal `--json` *value* (e.g. the text passed to `session
/// input`) is never mistaken for the flag.
pub(crate) fn args_request_json<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let json_flag = std::ffi::OsStr::new("--json");
    let end_of_opts = std::ffi::OsStr::new("--");
    args.into_iter()
        .skip(1) // argv[0] is the program name, never a flag.
        .take_while(|arg| arg.as_ref() != end_of_opts)
        .any(|arg| arg.as_ref() == json_flag)
}

/// Whether a clap error is a help/version *display*, not a usage failure.
///
/// `--help` and `--version` make clap return an `Err` whose kind is one of these;
/// clap prints them to stdout and exits 0. They are explicit, successful requests
/// and must never be rendered as an error document, even under `--json`.
fn clap_kind_is_display(kind: clap::error::ErrorKind) -> bool {
    matches!(
        kind,
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    )
}

/// Map a clap argument-parse failure into the structured `{class, code, msg,
/// recover}` envelope.
///
/// A single stable `code` (`cli_usage`) lets a script branch on "I mis-invoked
/// the CLI" without string-matching the message; the specific problem (missing
/// argument, bad value, unknown command) stays in `msg`. The class is
/// `configuration` — a usage error is a client-side invocation problem, the same
/// family as [`CliError::MissingEnv`].
fn clap_error_to_protocol_error(err: &clap::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Configuration,
        "cli_usage",
        // clap's `color` feature is not enabled and `StyledStr`'s `Display` never
        // emits ANSI escapes, so this rendered message is plain text safe to
        // embed in JSON. Trim the trailing newline clap appends.
        err.render().to_string().trim_end().to_owned(),
        Some(
            "check the command syntax; run `pohunek --help` or `<command> --help` for usage"
                .to_owned(),
        ),
    )
}

/// Render a clap argument-parse failure, honoring `--json`.
///
/// `--help`/`--version` are delegated to clap unchanged (printed to stdout,
/// exit 0). A genuine usage error is emitted as a structured JSON document on
/// stdout under `--json` (exit [`CLI_USAGE_EXIT_CODE`]); otherwise it falls back
/// to clap's native human rendering (stderr, exit 2), so the human experience is
/// identical to a plain `Cli::parse()`. Returns the process exit code.
pub(crate) fn render_clap_error(err: &clap::Error, json: bool) -> ExitCode {
    if json && !clap_kind_is_display(err.kind()) {
        print_json_error(&clap_error_to_protocol_error(err));
        ExitCode::from(CLI_USAGE_EXIT_CODE)
    } else {
        // `clap::Error::exit` prints to the correct stream and never returns.
        err.exit()
    }
}

#[cfg(test)]
mod tests {
    use protocol::{ProtocolVersion, ProtocolVersionRange};

    use super::*;

    #[test]
    fn protocol_error_passes_through_for_json() {
        let pe = ProtocolError::version_mismatch(
            ProtocolVersionRange::new(
                ProtocolVersion::new(1).expect("valid version"),
                ProtocolVersion::new(1).expect("valid version"),
            )
            .expect("valid range"),
            ProtocolVersionRange::new(
                ProtocolVersion::new(2).expect("valid version"),
                ProtocolVersion::new(2).expect("valid version"),
            )
            .expect("valid range"),
        );
        let structured = CliError::Protocol(pe.clone()).to_protocol_error();
        assert_eq!(structured, pe);
        assert_eq!(structured.code, "version_mismatch");
    }

    #[test]
    fn sdk_client_error_delegates_to_sdk_protocol_mapping() {
        let sdk = pohunek_client::ClientError::Framing("bad frame".to_owned());
        let expected = sdk.to_protocol_error();

        let structured = CliError::Client(sdk).to_protocol_error();

        assert_eq!(structured, expected);
        assert_eq!(structured.code, "framing");
    }

    #[test]
    fn sdk_descriptor_exhaustion_renders_specific_non_daemon_hint() {
        let err = CliError::Client(
            pohunek_client::ClientError::ClientFileDescriptorsExhausted {
                socket: PathBuf::from("/run/pohunek/daemon.sock"),
                source: io::Error::from_raw_os_error(libc::EMFILE),
            },
        );

        let text = human_error_text(&err);

        assert!(text.contains("client process"), "text: {text}");
        assert!(text.contains("RLIMIT_NOFILE"), "text: {text}");
        assert!(!text.contains("daemon start"), "text: {text}");
    }

    #[test]
    fn daemon_unreachable_maps_to_structured_error_with_hint() {
        let err = CliError::DaemonUnreachable {
            socket: PathBuf::from("/run/pohunek/daemon.sock"),
            source: io::Error::new(io::ErrorKind::NotFound, "no such file"),
        };
        let structured = err.to_protocol_error();
        assert_eq!(structured.class, ErrorClass::Daemon);
        assert_eq!(structured.code, "daemon_unreachable");
        let hint = structured
            .recover
            .expect("daemon-unreachable carries a hint");
        assert!(hint.contains("daemon start"), "hint: {hint}");
    }

    #[test]
    fn duplicate_meta_key_maps_to_cli_usage_with_hint() {
        let err = CliError::DuplicateMetaKey {
            key: "link.provider".to_owned(),
        };
        let structured = err.to_protocol_error();
        assert_eq!(structured.class, ErrorClass::Configuration);
        assert_eq!(structured.code, "cli_usage");
        assert!(structured.msg.contains("link.provider"), "{structured:?}");
        assert!(structured.recover.is_some());
    }

    #[test]
    fn confirmation_errors_are_configuration_class_with_distinct_codes() {
        let required = CliError::RemoteConfirmationRequired.to_protocol_error();
        let declined = CliError::RemoteConfirmationDeclined {
            host: "host-b".to_owned(),
        }
        .to_protocol_error();
        assert_eq!(required.class, ErrorClass::Configuration);
        assert_eq!(required.code, "confirmation_required");
        assert!(required.recover.is_some());
        assert_eq!(declined.class, ErrorClass::Configuration);
        assert_eq!(declined.code, "confirmation_declined");
        assert!(declined.msg.contains("host-b"));
        assert_ne!(required.code, declined.code);
    }

    #[test]
    fn structured_error_serializes_to_parseable_json_with_stable_code() {
        let err = CliError::Protocol(ProtocolError::agent_binary_missing("claude"));
        let doc =
            serde_json::to_string(&err.to_protocol_error()).expect("serialize structured error");
        let parsed: ProtocolError = serde_json::from_str(&doc).expect("parse structured error");
        assert_eq!(parsed.code, "agent_binary_missing");
        assert!(parsed.msg.contains("claude"));
        assert!(parsed.recover.is_some());
    }

    #[test]
    fn human_error_renders_recover_hint_for_version_mismatch() {
        let err = CliError::Protocol(ProtocolError::version_mismatch(
            ProtocolVersionRange::new(
                ProtocolVersion::new(1).expect("valid version"),
                ProtocolVersion::new(1).expect("valid version"),
            )
            .expect("valid range"),
            ProtocolVersionRange::new(
                ProtocolVersion::new(2).expect("valid version"),
                ProtocolVersion::new(2).expect("valid version"),
            )
            .expect("valid range"),
        ));
        let text = human_error_text(&err);
        // Names both versions (from the message) and the upgrade hint.
        assert!(text.contains('1') && text.contains('2'), "text: {text}");
        assert!(text.contains("hint:"), "text: {text}");
        assert!(text.contains("upgrade"), "text: {text}");
    }

    #[test]
    fn human_error_renders_recover_hint_for_agent_binary_missing() {
        let err = CliError::Protocol(ProtocolError::agent_binary_missing("claude"));
        let text = human_error_text(&err);
        assert!(text.contains("claude"), "text: {text}");
        assert!(text.contains("hint:"), "text: {text}");
    }

    #[test]
    fn human_error_without_hint_has_no_hint_line() {
        let text = human_error_text(&CliError::Client(pohunek_client::ClientError::Framing(
            "bad frame".to_owned(),
        )));
        assert!(!text.contains("hint:"), "text: {text}");
    }

    // --- clap usage-error handling --------------------------------------------

    use clap::Parser;

    #[test]
    fn args_request_json_detects_the_flag() {
        assert!(args_request_json([
            "pohunek", "session", "inspect", "s-1", "--json"
        ]));
        assert!(args_request_json(["pohunek", "doctor", "--json"]));
    }

    #[test]
    fn args_request_json_is_false_when_absent() {
        assert!(!args_request_json(["pohunek", "session", "inspect", "s-1"]));
        assert!(!args_request_json(["pohunek"]));
    }

    #[test]
    fn args_request_json_ignores_program_name() {
        // argv[0] is the program name, not a flag — even if it were `--json`.
        assert!(!args_request_json(["--json"]));
    }

    #[test]
    fn args_request_json_ignores_value_after_double_dash() {
        // `--json` after `--` is a positional value (e.g. `session input` text),
        // not the flag, so it must not flip the rendering mode.
        assert!(!args_request_json([
            "pohunek", "session", "input", "s-1", "--", "--json"
        ]));
    }

    #[test]
    fn clap_missing_required_arg_maps_to_cli_usage() {
        // `session inspect` without its required <target> — the exact case from
        // the finding (`session inspect --json`).
        let err = crate::Cli::try_parse_from(["pohunek", "session", "inspect"])
            .expect_err("missing <target> must fail to parse");
        let pe = clap_error_to_protocol_error(&err);
        assert_eq!(pe.class, ErrorClass::Configuration);
        assert_eq!(pe.code, "cli_usage");
        assert!(pe.recover.is_some(), "usage error carries a recover hint");
        assert!(!pe.msg.is_empty(), "usage error has a message");
        // The structured message must be plain text: no ANSI escape may leak
        // into the JSON document.
        assert!(
            !pe.msg.contains('\u{1b}'),
            "msg must be ANSI-free: {:?}",
            pe.msg
        );
    }

    #[test]
    fn clap_invalid_value_maps_to_cli_usage_and_round_trips() {
        // A clap invalid-value error. `session new --agent` is a free string since
        // Part C (resolved daemon-side), so use the still-enum `integration install
        // --agent`, whose value_parser only accepts claude/codex.
        let err = crate::Cli::try_parse_from([
            "pohunek",
            "integration",
            "install",
            "--agent",
            "nonsense",
        ])
        .expect_err("invalid --agent value must fail to parse");
        let pe = clap_error_to_protocol_error(&err);
        assert_eq!(pe.code, "cli_usage");
        // Round-trips through serde like every other structured error, so a
        // script can deserialize and branch on `code`.
        let doc = serde_json::to_string(&pe).expect("serialize structured usage error");
        let parsed: ProtocolError =
            serde_json::from_str(&doc).expect("parse structured usage error");
        assert_eq!(parsed.code, "cli_usage");
        assert_eq!(parsed.class, ErrorClass::Configuration);
        assert!(parsed.recover.is_some());
    }

    #[test]
    fn help_and_version_are_display_kinds_but_usage_errors_are_not() {
        let help = crate::Cli::try_parse_from(["pohunek", "--help"]).expect_err("help");
        let version = crate::Cli::try_parse_from(["pohunek", "--version"]).expect_err("version");
        assert!(clap_kind_is_display(help.kind()), "help is a display kind");
        assert!(
            clap_kind_is_display(version.kind()),
            "version is a display kind"
        );

        // A genuine usage error is NOT a display kind, so under `--json` it
        // renders as a structured document rather than being delegated to clap.
        let usage =
            crate::Cli::try_parse_from(["pohunek", "session", "inspect"]).expect_err("usage error");
        assert!(
            !clap_kind_is_display(usage.kind()),
            "missing-arg usage error is not a display kind"
        );
    }
}
