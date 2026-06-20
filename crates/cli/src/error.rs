//! Typed CLI errors.
//!
//! These cover client-side failures: missing env, daemon-unreachable, framing,
//! protocol errors returned by the daemon, and version mismatch. They are
//! rendered for humans on stderr and, where a command supports `--json`, can be
//! surfaced as structured error output.
//!
//! Argument-parse failures from clap are handled here too: under `--json` they
//! are mapped to the same `{class, code, msg, recover?}` envelope (see
//! [`render_clap_error`]) so that *every* failure a script can hit — including a
//! mis-typed command — is machine-readable, not just the ones that occur after a
//! successful parse.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

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

    /// A control line could not be framed/parsed.
    #[error("protocol framing error: {0}")]
    Framing(String),

    /// The daemon returned a typed protocol error.
    #[error("daemon error: {0}")]
    Protocol(#[from] ProtocolError),

    /// The local `netbird` CLI was not found on `PATH`, so a remote host could
    /// not be resolved. NetBird is optional (local-only use is fine); this only
    /// surfaces when a remote target is actually requested.
    #[error("the `netbird` CLI was not found on PATH")]
    NetbirdCliMissing,

    /// The `netbird` CLI is present but its local state could not be read (the
    /// NetBird daemon is down, or this host is not logged in).
    #[error("NetBird local state is unavailable: {detail}")]
    NetbirdStateUnavailable {
        /// A short, non-secret detail describing why the state was unreadable.
        detail: String,
    },

    /// The requested host name did not match any NetBird peer.
    #[error("host '{host}' was not found among NetBird peers")]
    HostUnknown {
        /// The requested host name.
        host: String,
    },

    /// A NetBird TCP connection to the host's daemon port could not be opened
    /// (the peer is offline or the control port is closed).
    #[error("could not open a NetBird connection to host '{host}': {source}")]
    HostUnreachable {
        /// The requested host name.
        host: String,
        /// Underlying connection error (a connect failure; carries no secrets).
        #[source]
        source: io::Error,
    },

    /// A NetBird TCP connection to the host opened, but no usable zagentmesh
    /// daemon answered the request — the connection closed without a reply or the
    /// daemon did not respond in time. Distinct from [`CliError::HostUnreachable`]
    /// (which is a failure to *connect*): here the transport succeeded but the
    /// remote daemon layer did not. Names the host so the operator knows which
    /// peer to investigate.
    #[error("connected to host '{host}' but no compatible zagentmesh daemon answered")]
    RemoteDaemonUnavailable {
        /// The host whose daemon did not answer.
        host: String,
    },

    /// A daemon on a *remote* host returned a typed protocol error. Wraps the
    /// canonical [`ProtocolError`] with the host so the human message names which
    /// peer failed, while preserving the daemon's stable `class`/`code`/`recover`
    /// (e.g. a remote `version_mismatch` stays `daemon/version_mismatch` but now
    /// names the host). A *local* daemon error keeps using [`CliError::Protocol`].
    #[error("host '{host}': {source}")]
    RemoteProtocol {
        /// The host whose daemon returned the error.
        host: String,
        /// The daemon's typed error, relayed unchanged except for host context.
        #[source]
        source: ProtocolError,
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
    pub(crate) fn to_protocol_error(&self) -> ProtocolError {
        match self {
            CliError::Protocol(err) => err.clone(),
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
                Some("start the daemon with `zagentmesh daemon start`".to_owned()),
            ),
            CliError::Framing(msg) => ProtocolError::new(
                ErrorClass::Transport,
                "framing",
                format!("protocol framing error: {msg}"),
                None,
            ),
            // NetBird/remote-resolution failures reuse the canonical protocol
            // constructors so the {class, code, recover} envelope is identical
            // whether the error originates in the CLI or is relayed from a daemon.
            CliError::NetbirdCliMissing => ProtocolError::netbird_cli_missing(),
            CliError::NetbirdStateUnavailable { detail } => {
                ProtocolError::netbird_state_unavailable(detail.clone())
            }
            CliError::HostUnknown { host } => ProtocolError::host_unknown(host),
            CliError::HostUnreachable { host, source } => {
                // The canonical constructor names the host and carries the
                // recover hint; append the underlying connect error to the
                // message so the operator sees *why* the dial failed. A connect
                // error never carries secret material.
                let mut err = ProtocolError::host_unreachable(host);
                err.msg = format!("{}: {source}", err.msg);
                err
            }
            // TCP connected but the remote daemon layer did not answer: the
            // canonical daemon-class error names the host and carries the hint.
            CliError::RemoteDaemonUnavailable { host } => {
                ProtocolError::remote_daemon_unavailable(host)
            }
            // A daemon on a remote host errored: relay the canonical class/code/
            // recover unchanged, but prepend the host so the message says which
            // peer failed (the daemon's own message — e.g. version_mismatch —
            // does not know the caller's host name).
            CliError::RemoteProtocol { host, source } => {
                let mut err = source.clone();
                err.msg = format!("host '{host}': {}", err.msg);
                err
            }
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
    let mut text = format!("zagentmesh: {err}\n");
    if let Some(hint) = err.recover_hint() {
        text.push_str(&format!("hint: {hint}\n"));
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
    match serde_json::to_string_pretty(err) {
        Ok(doc) => println!("{doc}"),
        // Serializing our own typed error cannot fail in practice; fall back
        // to a minimal hand-built document rather than printing nothing.
        Err(_) => println!(
            r#"{{"class":"daemon","code":"serialize_failed","msg":"failed to serialize error"}}"#
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
            "check the command syntax; run `zagentmesh --help` or `<command> --help` for usage"
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
pub(crate) fn render_clap_error(err: clap::Error, json: bool) -> ExitCode {
    if json && !clap_kind_is_display(err.kind()) {
        print_json_error(&clap_error_to_protocol_error(&err));
        ExitCode::from(CLI_USAGE_EXIT_CODE)
    } else {
        // `clap::Error::exit` prints to the correct stream and never returns.
        err.exit()
    }
}

#[cfg(test)]
mod tests {
    use protocol::ProtocolVersion;

    use super::*;

    #[test]
    fn protocol_error_passes_through_for_json() {
        let pe = ProtocolError::version_mismatch(ProtocolVersion(1), ProtocolVersion(2));
        let structured = CliError::Protocol(pe.clone()).to_protocol_error();
        assert_eq!(structured, pe);
        assert_eq!(structured.code, "version_mismatch");
    }

    #[test]
    fn daemon_unreachable_maps_to_structured_error_with_hint() {
        let err = CliError::DaemonUnreachable {
            socket: PathBuf::from("/run/zagentmesh/daemon.sock"),
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
    fn remote_resolution_errors_map_to_distinct_stable_codes_and_classes() {
        // Each NetBird/remote-resolution failure must surface a distinct, stable
        // code so a script can branch on it, with the class the DoD #7 layering
        // prescribes (discovery vs transport).
        let cases: Vec<(CliError, ErrorClass, &str)> = vec![
            (
                CliError::NetbirdCliMissing,
                ErrorClass::Discovery,
                "netbird_cli_missing",
            ),
            (
                CliError::NetbirdStateUnavailable {
                    detail: "not logged in".to_owned(),
                },
                ErrorClass::Discovery,
                "netbird_state_unavailable",
            ),
            (
                CliError::HostUnknown {
                    host: "host-b".to_owned(),
                },
                ErrorClass::Discovery,
                "host_unknown",
            ),
            (
                CliError::HostUnreachable {
                    host: "host-b".to_owned(),
                    source: io::Error::new(io::ErrorKind::ConnectionRefused, "refused"),
                },
                ErrorClass::Transport,
                "host_unreachable",
            ),
        ];

        let mut codes = std::collections::HashSet::new();
        for (err, class, code) in cases {
            let pe = err.to_protocol_error();
            assert_eq!(pe.class, class, "class for {code}");
            assert_eq!(pe.code, code, "code mismatch");
            assert!(codes.insert(pe.code.clone()), "duplicate code {code}");
        }
    }

    #[test]
    fn remote_daemon_unavailable_is_daemon_class_names_host_and_distinct_from_connect_failure() {
        // Distinct from HostUnreachable (a *connect* failure): here the TCP dial
        // succeeded but the daemon layer did not answer (DoD #7 layering).
        let unavailable = CliError::RemoteDaemonUnavailable {
            host: "build-box".to_owned(),
        }
        .to_protocol_error();
        assert_eq!(unavailable.class, ErrorClass::Daemon);
        assert_eq!(unavailable.code, "remote_daemon_unavailable");
        assert!(unavailable.msg.contains("build-box"), "msg: {}", unavailable.msg);
        assert!(unavailable.recover.is_some());

        let connect_failure = CliError::HostUnreachable {
            host: "build-box".to_owned(),
            source: io::Error::new(io::ErrorKind::ConnectionRefused, "refused"),
        }
        .to_protocol_error();
        assert_eq!(connect_failure.class, ErrorClass::Transport);
        assert_ne!(
            unavailable.code, connect_failure.code,
            "transport-connect and daemon-no-answer must be distinguishable"
        );
    }

    #[test]
    fn remote_protocol_names_host_while_preserving_source_code_and_class() {
        // A remote daemon error is relayed with the daemon's stable class/code/
        // recover; only the message gains host context.
        let source = ProtocolError::version_mismatch(ProtocolVersion(1), ProtocolVersion(2));
        let wrapped = CliError::RemoteProtocol {
            host: "build-box".to_owned(),
            source: source.clone(),
        }
        .to_protocol_error();
        assert_eq!(wrapped.class, source.class);
        assert_eq!(wrapped.code, source.code);
        assert_eq!(wrapped.recover, source.recover);
        assert!(wrapped.msg.contains("build-box"), "names host: {}", wrapped.msg);
        // The daemon's own message detail is retained alongside the host.
        assert!(
            wrapped.msg.contains("incompatible"),
            "keeps source detail: {}",
            wrapped.msg
        );
    }

    #[test]
    fn host_unreachable_message_names_host_and_appends_source() {
        let err = CliError::HostUnreachable {
            host: "host-b".to_owned(),
            source: io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"),
        };
        let pe = err.to_protocol_error();
        assert!(pe.msg.contains("host-b"), "msg names the host: {}", pe.msg);
        assert!(
            pe.msg.contains("connection refused"),
            "msg appends the source: {}",
            pe.msg
        );
        assert!(pe.recover.is_some(), "host_unreachable carries a hint");
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
            ProtocolVersion(1),
            ProtocolVersion(2),
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
        let text = human_error_text(&CliError::Framing("bad frame".to_owned()));
        assert!(!text.contains("hint:"), "text: {text}");
    }

    // --- clap usage-error handling --------------------------------------------

    use clap::Parser;

    #[test]
    fn args_request_json_detects_the_flag() {
        assert!(args_request_json([
            "zagentmesh",
            "session",
            "inspect",
            "s-1",
            "--json"
        ]));
        assert!(args_request_json(["zagentmesh", "doctor", "--json"]));
    }

    #[test]
    fn args_request_json_is_false_when_absent() {
        assert!(!args_request_json(["zagentmesh", "session", "inspect", "s-1"]));
        assert!(!args_request_json(["zagentmesh"]));
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
            "zagentmesh",
            "session",
            "input",
            "s-1",
            "--",
            "--json"
        ]));
    }

    #[test]
    fn clap_missing_required_arg_maps_to_cli_usage() {
        // `session inspect` without its required <target> — the exact case from
        // the finding (`session inspect --json`).
        let err = crate::Cli::try_parse_from(["zagentmesh", "session", "inspect"])
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
        // `session new --agent nonsense` — the second case from the finding.
        let err = crate::Cli::try_parse_from(["zagentmesh", "session", "new", "--agent", "nonsense"])
            .expect_err("invalid --agent value must fail to parse");
        let pe = clap_error_to_protocol_error(&err);
        assert_eq!(pe.code, "cli_usage");
        // Round-trips through serde like every other structured error, so a
        // script can deserialize and branch on `code`.
        let doc = serde_json::to_string(&pe).expect("serialize structured usage error");
        let parsed: ProtocolError = serde_json::from_str(&doc).expect("parse structured usage error");
        assert_eq!(parsed.code, "cli_usage");
        assert_eq!(parsed.class, ErrorClass::Configuration);
        assert!(parsed.recover.is_some());
    }

    #[test]
    fn help_and_version_are_display_kinds_but_usage_errors_are_not() {
        let help = crate::Cli::try_parse_from(["zagentmesh", "--help"]).expect_err("help");
        let version = crate::Cli::try_parse_from(["zagentmesh", "--version"]).expect_err("version");
        assert!(clap_kind_is_display(help.kind()), "help is a display kind");
        assert!(
            clap_kind_is_display(version.kind()),
            "version is a display kind"
        );

        // A genuine usage error is NOT a display kind, so under `--json` it
        // renders as a structured document rather than being delegated to clap.
        let usage = crate::Cli::try_parse_from(["zagentmesh", "session", "inspect"])
            .expect_err("usage error");
        assert!(
            !clap_kind_is_display(usage.kind()),
            "missing-arg usage error is not a display kind"
        );
    }
}
