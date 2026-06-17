//! Typed protocol errors.
//!
//! Error envelopes carry a `class` (broad category), a machine-readable `code`,
//! a human `msg`, and an optional `recover` hint. The classes mirror the error
//! taxonomy in `docs/architecture.md` "Error Handling": configuration, daemon,
//! transport, runtime, discovery. Keeping them typed lets `--json` consumers and
//! operator agents branch on the failure instead of string-matching messages.

use serde::{Deserialize, Serialize};

use crate::version::ProtocolVersion;

/// Broad error category for a control-protocol error.
///
/// Serialized in lowercase snake form on the wire (e.g. `"daemon"`). Mirrors the
/// distinctions in `docs/architecture.md` "Error Handling".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Missing or invalid required configuration.
    Configuration,
    /// Daemon-level failures: unavailable, version mismatch, framing.
    Daemon,
    /// Transport failures: NetBird unreachable, connection lost.
    Transport,
    /// Runtime failures: agent binary missing, PTY allocation, process exit,
    /// worktree conflict.
    Runtime,
    /// Discovery failures: NetBird CLI missing, local state unavailable.
    Discovery,
}

impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ErrorClass::Configuration => "configuration",
            ErrorClass::Daemon => "daemon",
            ErrorClass::Transport => "transport",
            ErrorClass::Runtime => "runtime",
            ErrorClass::Discovery => "discovery",
        };
        f.write_str(s)
    }
}

/// A typed control-protocol error.
///
/// This is both the body carried inside an error [`Response`](crate::Response)
/// and a Rust `Error` (via `thiserror`) so daemon code can return it directly
/// and the API layer can serialize it into the `err` field of a response.
///
/// Machine codes are stable strings; see the constructors for the canonical
/// ones used in Phase 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{class}/{code}: {msg}")]
pub struct ProtocolError {
    /// Broad category, for coarse branching.
    pub class: ErrorClass,
    /// Stable machine-readable code, for precise branching.
    pub code: String,
    /// Human-readable message. Never contains secrets or terminal content.
    pub msg: String,
    /// Optional suggested recovery action (e.g. "install claude").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recover: Option<String>,
}

impl ProtocolError {
    /// Construct an error with all fields explicit.
    #[must_use]
    pub fn new(
        class: ErrorClass,
        code: impl Into<String>,
        msg: impl Into<String>,
        recover: Option<String>,
    ) -> Self {
        Self {
            class,
            code: code.into(),
            msg: msg.into(),
            recover,
        }
    }

    /// The canonical `daemon/version_mismatch` error.
    ///
    /// Carries both versions in the message so the operator sees exactly what to
    /// upgrade. Code is stable: `version_mismatch`.
    #[must_use]
    pub fn version_mismatch(client_v: ProtocolVersion, daemon_v: ProtocolVersion) -> Self {
        Self::new(
            ErrorClass::Daemon,
            "version_mismatch",
            format!(
                "client protocol version {client_v} is incompatible with daemon protocol version {daemon_v}"
            ),
            Some("upgrade the older side so both speak the same protocol version".to_owned()),
        )
    }

    /// The canonical `daemon/method_not_found` error for an unknown method.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            ErrorClass::Daemon,
            "method_not_found",
            format!("unknown control method: {method}"),
            None,
        )
    }

    /// The canonical `daemon/bad_request` error for a malformed request body.
    #[must_use]
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorClass::Daemon, "bad_request", msg, None)
    }
}
