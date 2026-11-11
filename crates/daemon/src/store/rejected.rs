//! Listing the lines no loader accepts.
//!
//! A rejected line stays in the store through every rewrite, so package gates
//! keep failing closed while one may pin a package. The listing feeds the
//! doctor's rejected-lines check; the recovery is a manual edit of the store.

// Rust guideline compliant 2026-10-29

use std::io;

use protocol::{StoreRejectedLine, STORE_REJECTED_TEXT_MAX_BYTES};
use sha2::{Digest, Sha256};

use super::{partition_lines, RejectedLine, Store};

/// Lowercase hex SHA-256 of a line's text.
fn content_hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Whether the line may hold the pin of a live runtime's root.
///
/// A line that is not a JSON object names no state at all, so it may be any
/// record, including a live session; the upgrade preflight counts only lines
/// it can read, but removing the line by hand drops more than that. A `resume`
/// line holds no session state, yet its pin is unreadable and the session it
/// resumes may be running, so it counts the same.
fn may_be_live(line: &RejectedLine) -> bool {
    line.may_own_runtime
        || (line.kind.as_deref() == Some("resume") && line.pin_unreadable())
        || !serde_json::from_str::<serde_json::Value>(&line.text)
            .is_ok_and(|value| value.is_object())
}

/// `text` cut to [`STORE_REJECTED_TEXT_MAX_BYTES`] at a character boundary.
fn bounded(reason: &str) -> &str {
    if reason.len() <= STORE_REJECTED_TEXT_MAX_BYTES {
        return reason;
    }
    let mut end = STORE_REJECTED_TEXT_MAX_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    &reason[..end]
}

/// Pairs each rejected line with its 1-based line number in `content`.
///
/// `rejected` is in file order and holds the text of the lines of `content`,
/// so one forward pass matches them; blank lines count toward the number.
///
/// # Errors
///
/// Returns an error when a rejected line cannot be located, because a line
/// that cannot be numbered cannot be listed for the operator.
fn numbered(content: &str, rejected: Vec<RejectedLine>) -> io::Result<Vec<(u32, RejectedLine)>> {
    let expected = rejected.len();
    let mut pending = rejected.into_iter().peekable();
    let mut numbered = Vec::with_capacity(expected);
    for (index, text) in content.lines().enumerate() {
        let Some(next) = pending.peek() else {
            break;
        };
        if next.text == text {
            let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
            if let Some(line) = pending.next() {
                numbered.push((number, line));
            }
        }
    }
    if numbered.len() == expected {
        Ok(numbered)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a rejected metadata-store line could not be located in the file",
        ))
    }
}

impl Store {
    /// The lines no loader accepts, in file order. A missing file yields an
    /// empty list.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read or its schema is refused.
    pub fn rejected_lines(&self) -> io::Result<Vec<StoreRejectedLine>> {
        let Some(content) = self.read_content()? else {
            return Ok(Vec::new());
        };
        let lines = self.checked_lines(&content)?;
        let rejected = partition_lines(lines).1;
        Ok(numbered(&content, rejected)?
            .into_iter()
            .map(|(line, rejected)| StoreRejectedLine {
                line,
                may_pin: rejected.pin_unreadable(),
                content_hash: content_hash(&rejected.text),
                reason: bounded(&rejected.reason).to_owned(),
                kind: rejected
                    .kind
                    .as_deref()
                    .map(|kind| bounded(kind).to_owned()),
                session_id: rejected
                    .session_id
                    .as_deref()
                    .map(|id| bounded(id).to_owned()),
                may_own_runtime: may_be_live(&rejected),
            })
            .collect())
    }
}
