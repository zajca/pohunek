//! Wire types of `store.rejected_list` and `store.rejected_set_aside`, which
//! list the metadata-store lines no loader accepts and move one into the
//! quarantine file.
//!
//! The raw line never crosses the wire: a caller names a line by the SHA-256
//! of its text, and the daemon keeps the text in the quarantine file.

// Rust guideline compliant 2026-10-29

use serde::{Deserialize, Serialize};

use crate::error::{ErrorClass, ProtocolError};

/// Machine code: no rejected line has the requested content hash.
pub const STORE_REJECTED_LINE_NOT_FOUND: &str = "store_rejected_line_not_found";
/// Machine code: the line may own a live runtime and the caller did not accept
/// the loss of its package pin.
pub const STORE_REJECTED_LINE_LIVE: &str = "store_rejected_line_live";
/// Machine code: the quarantine file or the store rewrite failed.
pub const STORE_QUARANTINE_FAILED: &str = "store_quarantine_failed";
/// Machine code: the method is served on the local control socket only. It is
/// the code of `PackageErrorKind::LocalOnly`.
const STORE_LOCAL_ONLY: &str = "local_only_method";
/// Machine code: the store cannot be read or its schema is refused.
pub const STORE_READ_FAILED: &str = "store_read_failed";

/// Longest `reason`, `kind` and `session_id` of a [`StoreRejectedLine`], in
/// bytes.
///
/// A rejection reason, a kind and a session id are all text of the rejected
/// line, so the bound keeps one entry small whatever the line holds. With the
/// JSON escaping of control characters and [`STORE_REJECTED_LIST_MAX_LINES`]
/// entries, a list stays well inside one control line.
pub const STORE_REJECTED_TEXT_MAX_BYTES: usize = 128;

/// Most lines one `store.rejected_list` result carries. `total` reports how many
/// there are; a line left out is reached by setting the listed ones aside and
/// listing again.
pub const STORE_REJECTED_LIST_MAX_LINES: usize = 100;

/// One metadata-store line no loader accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "StoreRejectedLine.ts"))]
pub struct StoreRejectedLine {
    /// 1-based position in `metadata.jsonl`; a blank line counts.
    pub line: u32,
    /// The record kind the line names, when it is a JSON object that does; cut
    /// to [`STORE_REJECTED_TEXT_MAX_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub kind: Option<String>,
    /// The session the line names, when it does; cut to
    /// [`STORE_REJECTED_TEXT_MAX_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub session_id: Option<String>,
    /// Why no loader accepts the line, cut to
    /// [`STORE_REJECTED_TEXT_MAX_BYTES`].
    pub reason: String,
    /// Whether the line may pin a runtime package whose pin cannot be read:
    /// its readable `kind` is not `worktree` or `project`, and it does not
    /// decode as a record. Package gates fail closed while one exists.
    pub may_pin: bool,
    /// Whether setting the line aside may drop the pin of a live runtime's
    /// root. A line whose session state cannot be read counts as live, and so
    /// does a `resume` line whose pin cannot be read.
    pub may_own_runtime: bool,
    /// Lowercase hex SHA-256 of the line text without its terminator.
    pub content_hash: String,
}

/// Result of `store.rejected_list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "StoreRejectedListResult.ts"))]
pub struct StoreRejectedListResult {
    /// The first [`STORE_REJECTED_LIST_MAX_LINES`] rejected lines in file order.
    pub lines: Vec<StoreRejectedLine>,
    /// How many rejected lines the store holds.
    pub total: u32,
}

/// Parameters of `store.rejected_set_aside`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "StoreRejectedSetAsideParams.ts")
)]
#[serde(deny_unknown_fields)]
pub struct StoreRejectedSetAsideParams {
    /// Content hash of the line, as `store.rejected_list` reports it. Every
    /// line with this text is set aside.
    pub content_hash: String,
    /// Accepts that the package pin of a line that may own a live runtime is
    /// no longer kept. Without it such a line is refused with
    /// `store_rejected_line_live`.
    #[serde(default)]
    pub accept_pin_loss: bool,
}

/// Result of `store.rejected_set_aside`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "StoreRejectedSetAsideResult.ts")
)]
pub struct StoreRejectedSetAsideResult {
    /// Content hash that was set aside.
    pub content_hash: String,
    /// The line numbers the text had in `metadata.jsonl`.
    pub lines: Vec<u32>,
    /// File name, next to the store, that now holds the line and its audit
    /// record. Never a path.
    pub quarantine_file: String,
}

impl ProtocolError {
    /// No rejected store line has the requested content hash.
    #[must_use]
    pub fn store_rejected_line_not_found() -> Self {
        Self::new(
            ErrorClass::Runtime,
            STORE_REJECTED_LINE_NOT_FOUND,
            "no rejected metadata-store line has this content hash",
            Some("run `pohunek store rejected list` and use a hash it prints".to_owned()),
        )
    }

    /// The line may own a live runtime and the pin loss was not accepted.
    ///
    /// `resume_pin_unreadable` names the refused line kind: a `resume` line
    /// whose package pin cannot be read, or otherwise a line that may be a
    /// session record.
    #[must_use]
    pub fn store_rejected_line_live(resume_pin_unreadable: bool) -> Self {
        let (msg, recover) = if resume_pin_unreadable {
            (
                "the line is a `resume` line whose package pin cannot be read, and the session it resumes may be running",
                "stop that session or confirm no runtime depends on the pin, then repeat with `--accept-pin-loss`",
            )
        } else {
            (
                "the line may belong to a session with a live runtime, so setting it aside can drop the package pin it holds",
                "stop the session or confirm no runtime depends on the line, then repeat with `--accept-pin-loss`",
            )
        };
        Self::new(
            ErrorClass::Runtime,
            STORE_REJECTED_LINE_LIVE,
            msg,
            Some(recover.to_owned()),
        )
    }

    /// A store method arrived on a remote connection.
    #[must_use]
    pub fn store_local_only() -> Self {
        Self::new(
            ErrorClass::Daemon,
            STORE_LOCAL_ONLY,
            "rejected-store-line methods are served on the local control socket only",
            Some("run the command on the host that runs the daemon".to_owned()),
        )
    }

    /// The quarantine file or the store rewrite failed.
    #[must_use]
    pub fn store_quarantine_failed() -> Self {
        Self::new(
            ErrorClass::Daemon,
            STORE_QUARANTINE_FAILED,
            "the line could not be moved into the quarantine file; the store still holds it",
            Some("check the daemon log, then repeat the command".to_owned()),
        )
    }

    /// The store cannot be read or its schema is refused.
    #[must_use]
    pub fn store_read_failed() -> Self {
        Self::new(
            ErrorClass::Daemon,
            STORE_READ_FAILED,
            "the metadata store cannot be read",
            Some("check the daemon log".to_owned()),
        )
    }
}
