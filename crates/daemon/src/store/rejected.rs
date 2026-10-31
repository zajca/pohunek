//! Listing the lines no loader accepts and setting them aside.
//!
//! A rejected line stays in the store through every rewrite, so package gates
//! keep failing closed while one may pin a package. Setting a line aside moves
//! its text, with an audit record, into the quarantine file next to the store
//! and drops it from the store in one atomic rename.

// Rust guideline compliant 2026-10-29

use std::io;

use pohunek_paths::REJECTED_QUARANTINE_NAME;
use pohunek_platform::filesystem::{AtomicReplaceError, TrustedDir};
use protocol::{StoreRejectedLine, STORE_REJECTED_TEXT_MAX_BYTES};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::warn;

use super::{
    check_body_size, fs_error_to_io, parent_directory, partition_lines, RejectedLine, Store,
    MAX_METADATA_STORE_BYTES, OWNER_PRIVATE_DIRECTORY_MODE, OWNER_PRIVATE_FILE_MODE,
};

/// Version of the quarantine record layout.
const QUARANTINE_SCHEMA: u32 = 1;

/// Why a rejected line was not set aside.
#[derive(Debug, Error)]
pub enum SetAsideError {
    /// No rejected line has the requested content hash.
    #[error("no rejected metadata-store line has this content hash")]
    NotFound,
    /// The line may own a live runtime and the pin loss was not accepted.
    /// `resume_pin_unreadable` is true when the refused line is a `resume`
    /// line whose pin cannot be read, false when it may be a session record.
    #[error("the line may belong to a session with a live runtime")]
    Live { resume_pin_unreadable: bool },
    /// The store could not be read, or the quarantine file or the store
    /// rewrite failed. The store still holds the line.
    #[error("the line could not be moved into the quarantine file: {0}")]
    Failed(#[source] io::Error),
}

/// The lines set aside by one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetAside {
    /// Content hash of the lines.
    pub content_hash: String,
    /// The 1-based line numbers the text had in the store.
    pub lines: Vec<u32>,
}

/// One line of the quarantine file: the audit record and the line itself.
#[derive(Debug, Serialize)]
struct QuarantineRecord<'a> {
    quarantine_schema: u32,
    set_aside_at: String,
    line: u32,
    content_hash: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    reason: &'a str,
    may_pin: bool,
    may_own_runtime: bool,
    accepted_pin_loss: bool,
    text: &'a str,
}

/// Lowercase hex SHA-256 of a line's text.
fn content_hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Whether setting the line aside may drop the pin of a live runtime's root.
///
/// A line that is not a JSON object names no state at all, so it may be any
/// record, including a live session; the upgrade preflight counts only lines
/// it can read, but dropping a pin consents to more than that. A `resume`
/// line holds no session state, yet its pin is unreadable and the session it
/// resumes may be running, so it needs the same consent.
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
/// Returns an error when a rejected line cannot be located. A caller that
/// drops lines from the store must not go on, because a line it cannot number
/// is one it would drop without a record.
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

    /// Moves every rejected line whose text hashes to `content_hash` into the
    /// quarantine file and drops it from the store.
    ///
    /// The quarantine records are written and made durable before the store is
    /// rewritten, so a failure at any point leaves the text in at least one of
    /// the two files; a repeat after a crash between the two steps appends a
    /// second record. The store's other records are rewritten as a rewrite
    /// does, and the lines that stay rejected are kept as stored. A line that
    /// may own a live runtime, or a `resume` line whose pin cannot be read, is
    /// refused unless `accept_pin_loss` is set, because dropping it drops the
    /// package pin that keeps the root a live worker runs from.
    ///
    /// # Errors
    ///
    /// Returns [`SetAsideError::NotFound`] when no rejected line has the hash,
    /// [`SetAsideError::Live`] for a possibly live line without
    /// `accept_pin_loss`, and [`SetAsideError::Failed`] when the store or the
    /// quarantine file cannot be read or written.
    pub fn set_aside_rejected(
        &self,
        content_hash_hex: &str,
        accept_pin_loss: bool,
    ) -> Result<SetAside, SetAsideError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let content = self
            .read_content()
            .map_err(SetAsideError::Failed)?
            .ok_or(SetAsideError::NotFound)?;
        let lines = self
            .checked_lines(&content)
            .map_err(SetAsideError::Failed)?;
        let ((resume, worktrees, projects, sessions), rejected) = partition_lines(lines);
        let (aside, kept): (Vec<_>, Vec<_>) = numbered(&content, rejected)
            .map_err(SetAsideError::Failed)?
            .into_iter()
            .partition(|(_, line)| content_hash(&line.text) == content_hash_hex);
        if aside.is_empty() {
            return Err(SetAsideError::NotFound);
        }
        if !accept_pin_loss {
            if let Some((_, line)) = aside.iter().find(|(_, line)| may_be_live(line)) {
                return Err(SetAsideError::Live {
                    resume_pin_unreadable: !line.may_own_runtime
                        && line.kind.as_deref() == Some("resume"),
                });
            }
        }
        let set_aside_at = crate::time::now_rfc3339();
        let records: Vec<QuarantineRecord<'_>> = aside
            .iter()
            .map(|(number, line)| QuarantineRecord {
                quarantine_schema: QUARANTINE_SCHEMA,
                set_aside_at: set_aside_at.clone(),
                line: *number,
                content_hash: content_hash_hex,
                kind: line.kind.as_deref().map(bounded),
                reason: bounded(&line.reason),
                may_pin: line.pin_unreadable(),
                may_own_runtime: may_be_live(line),
                accepted_pin_loss: accept_pin_loss && may_be_live(line),
                text: &line.text,
            })
            .collect();
        self.append_quarantine(&records)
            .map_err(SetAsideError::Failed)?;
        let kept_texts: Vec<String> = kept.into_iter().map(|(_, line)| line.text).collect();
        let outcome = self
            .write_all(&resume, &worktrees, &projects, &sessions, &kept_texts)
            .map_err(SetAsideError::Failed)?;
        // A committed rewrite whose durability is uncertain is logged by the
        // conversion; the lines are already in the quarantine file.
        let _mutation = outcome.with_value(());
        warn!(
            content_hash = content_hash_hex,
            lines = aside.len(),
            accepted_pin_loss = accept_pin_loss,
            "set aside rejected metadata-store lines into the quarantine file"
        );
        Ok(SetAside {
            content_hash: content_hash_hex.to_owned(),
            lines: aside.iter().map(|(number, _)| *number).collect(),
        })
    }

    /// Appends `records` to the quarantine file with one atomic replace, so a
    /// crash never leaves a partial record. The file is created owner-private.
    fn append_quarantine(&self, records: &[QuarantineRecord<'_>]) -> io::Result<()> {
        let parent = parent_directory(&self.path);
        TrustedDir::open_or_create_absolute(parent, OWNER_PRIVATE_DIRECTORY_MODE)
            .map_err(fs_error_to_io)?;
        let directory = TrustedDir::open_absolute(parent, OWNER_PRIVATE_DIRECTORY_MODE)
            .map_err(fs_error_to_io)?;
        let name = std::ffi::OsStr::new(REJECTED_QUARANTINE_NAME);
        let mut body =
            match directory.read_file(name, OWNER_PRIVATE_FILE_MODE, MAX_METADATA_STORE_BYTES) {
                Ok(bytes) => String::from_utf8(bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                Err(error) if error.io_kind() == Some(io::ErrorKind::NotFound) => String::new(),
                Err(error) => return Err(fs_error_to_io(error)),
            };
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        for record in records {
            let line = serde_json::to_string(record)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            body.push_str(&line);
            body.push('\n');
        }
        check_body_size(&body)?;
        let temporary = self.temp_path();
        let temporary = temporary.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "quarantine temporary has no filename",
            )
        })?;
        match directory.replace_file(name, temporary, body.as_bytes(), OWNER_PRIVATE_FILE_MODE) {
            Ok(()) => Ok(()),
            Err(AtomicReplaceError::BeforeCommit(error)) => Err(fs_error_to_io(error)),
            Err(AtomicReplaceError::CommittedDurabilityUncertain(error)) => {
                Err(io::Error::other(error))
            }
            Err(error) => Err(io::Error::other(error)),
        }
    }
}
