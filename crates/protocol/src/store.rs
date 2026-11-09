//! The metadata-store line type the daemon's doctor counts rejected lines with.
//!
//! The raw line never leaves the daemon: a line is named by the SHA-256 of its
//! text.

// Rust guideline compliant 2026-10-29

use serde::{Deserialize, Serialize};

/// Longest `reason`, `kind` and `session_id` of a [`StoreRejectedLine`], in
/// bytes.
///
/// A rejection reason, a kind and a session id are all text of the rejected
/// line, so the bound keeps one entry small whatever the line holds.
pub const STORE_REJECTED_TEXT_MAX_BYTES: usize = 128;

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
