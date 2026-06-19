//! Typed CLI errors.
//!
//! These cover client-side failures: missing env, daemon-unreachable, framing,
//! protocol errors returned by the daemon, and version mismatch. They are
//! rendered for humans on stderr and, where a command supports `--json`, can be
//! surfaced as structured error output.

use std::io;
use std::path::PathBuf;

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

    /// A remote (non-local) target was requested before Phase 2 transport
    /// exists. Parsing is host-aware now; execution is local-only.
    #[error("remote target '{host}' is not supported yet (Phase 1 is local-only)")]
    RemoteNotSupported {
        /// The requested host name.
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
            CliError::RemoteNotSupported { host } => ProtocolError::new(
                ErrorClass::Transport,
                "remote_not_supported",
                format!("remote target '{host}' is not supported yet (Phase 1 is local-only)"),
                None,
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
        match serde_json::to_string_pretty(&err.to_protocol_error()) {
            Ok(doc) => println!("{doc}"),
            // Serializing our own typed error cannot fail in practice; fall back
            // to a minimal hand-built document rather than printing nothing.
            Err(_) => println!(
                r#"{{"class":"daemon","code":"serialize_failed","msg":"failed to serialize error"}}"#
            ),
        }
    } else {
        eprint!("{}", human_error_text(err));
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
}
