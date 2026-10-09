//! Host-aware target parsing.
//!
//! Sessions are addressed as `<host>/<session-id>` (or a bare `<session-id>` for
//! the implicit local host). The parser only extracts the two parts; the
//! *effective host* (which selects the transport) is decided by `main::
//! effective_host`, and whether a host string is local is decided by the
//! transport (`client::Client::connect`). Keeping `Target` a pure parse holder
//! avoids two competing notions of "is this local".
//!
//! Grammar:
//! - `s-42`            → local session `s-42` (host `None`, falls back to the
//!   global `--host` flag)
//! - `local/s-42`      → explicit local host, session `s-42`
//! - `host-b/s-42`     → remote host `host-b`, session `s-42`

use std::fmt;

pub(crate) use pohunek_client::{is_local_host, LOCAL_HOST};

/// A parsed session target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// `None` for an implicit local target; `Some(host)` for an explicit host
    /// (which may itself be the reserved `local` name).
    pub(crate) host: Option<String>,
    /// The session identifier portion.
    pub(crate) session_id: String,
}

/// Error parsing a target string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TargetParseError {
    /// The input was empty.
    #[error("empty target")]
    Empty,
    /// The session-id portion was empty (e.g. `host/`).
    #[error("missing session id in target '{0}'")]
    MissingSessionId(String),
    /// The host portion was empty (e.g. `/s-42`).
    #[error("missing host in target '{0}'")]
    MissingHost(String),
    /// More than one `/` separator.
    #[error("invalid target '{0}': expected at most one '/' separating host and session id")]
    TooManySeparators(String),
}

impl std::str::FromStr for Target {
    type Err = TargetParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(TargetParseError::Empty);
        }

        let mut parts = s.splitn(3, '/');
        // splitn with limit 3 lets us detect a third segment as "too many".
        let first = parts.next().unwrap_or_default();
        match (parts.next(), parts.next()) {
            // No separator: bare session id, implicit local.
            (None, _) => Ok(Target {
                host: None,
                session_id: first.to_owned(),
            }),
            // One separator: host/session.
            (Some(session), None) => {
                if first.is_empty() {
                    return Err(TargetParseError::MissingHost(s.to_owned()));
                }
                if session.is_empty() {
                    return Err(TargetParseError::MissingSessionId(s.to_owned()));
                }
                Ok(Target {
                    host: Some(first.to_owned()),
                    session_id: session.to_owned(),
                })
            }
            // Two separators: malformed.
            (Some(_), Some(_)) => Err(TargetParseError::TooManySeparators(s.to_owned())),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Some(h) => write!(f, "{h}/{}", self.session_id),
            None => f.write_str(&self.session_id),
        }
    }
}
