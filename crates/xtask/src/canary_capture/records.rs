//! The OSC 7501 record reader of the canary capture.
//!
//! Retained output arrives in chunks that end at arbitrary byte offsets: a PTY
//! read boundary, the `--max-bytes` page size, or the end of the output so far.
//! The scanner works on bytes, never decodes the screen as text, and carries
//! an incomplete `ESC ] 7501 ;` sequence into the next chunk, so a record split
//! across two reads is parsed once, whole.

// Rust guideline compliant 2026-11-14

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use pohunek_terminal::{parse_program_status, ProgramStatusReport};
use serde_json::Value;

/// The `app` value of a root Claude Code record.
const CLAUDE_APP: &str = "claude-code";

/// Introducer of an OSC 7501 sequence: `ESC ] 7501 ;`.
const INTRODUCER: &[u8] = b"\x1b]7501;";

/// Escape, which starts both the introducer and the `ESC \` terminator.
const ESC: u8 = 0x1b;

/// Bell, the other OSC terminator.
const BEL: u8 = 0x07;

/// Backslash, the second byte of the `ESC \` terminator.
const BACKSLASH: u8 = b'\\';

/// Longest unterminated sequence carried into the next chunk. A record with
/// its largest `msg` tail (2048 bytes base64-encoded) fits; anything longer is
/// not a record the parser would accept, so it is dropped instead of growing.
const MAX_PENDING_BYTES: usize = 4096;

/// Scans output chunks for complete OSC 7501 root records of Claude Code.
#[derive(Debug, Default)]
pub(super) struct RecordScanner {
    pending: Vec<u8>,
}

impl RecordScanner {
    /// Appends `chunk` and returns the records it completed, each in the
    /// canonical form without the app segment (`state=done`,
    /// `state=blocked:kind=permission`). `msg`, `title` and `progress` tails
    /// never appear: the canonical form carries the state and kind only.
    pub(super) fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.extend_from_slice(chunk);
        let mut records = Vec::new();
        let mut index = 0;
        while index < buffer.len() {
            if buffer[index] != ESC {
                index += 1;
                continue;
            }
            let rest = &buffer[index..];
            if rest.len() < INTRODUCER.len() {
                if INTRODUCER.starts_with(rest) {
                    self.pending = rest.to_vec();
                    return records;
                }
                index += 1;
                continue;
            }
            if !rest.starts_with(INTRODUCER) {
                index += 1;
                continue;
            }
            let payload_start = index + INTRODUCER.len();
            match terminator(&buffer[payload_start..]) {
                Terminator::Found {
                    payload_len,
                    sequence_len,
                } => {
                    let payload = &buffer[payload_start..payload_start + payload_len];
                    if let Some(record) = root_record(payload) {
                        records.push(record);
                    }
                    index = payload_start + sequence_len;
                }
                Terminator::Aborted { at } => index = payload_start + at,
                Terminator::Incomplete => {
                    if rest.len() <= MAX_PENDING_BYTES {
                        self.pending = rest.to_vec();
                    }
                    return records;
                }
            }
        }
        records
    }
}

/// Where the OSC sequence that starts a payload ends.
enum Terminator {
    /// A `BEL` or `ESC \` ends the payload of `payload_len` bytes; the payload
    /// and terminator together take `sequence_len` bytes.
    Found {
        payload_len: usize,
        sequence_len: usize,
    },
    /// An escape that is not `ESC \` at `at` aborts the sequence; scanning
    /// resumes there, since that escape may start the next sequence.
    Aborted { at: usize },
    /// The chunk ends before the terminator.
    Incomplete,
}

fn terminator(payload: &[u8]) -> Terminator {
    for (offset, byte) in payload.iter().enumerate() {
        match *byte {
            BEL => {
                return Terminator::Found {
                    payload_len: offset,
                    sequence_len: offset + 1,
                }
            }
            ESC => {
                return match payload.get(offset + 1) {
                    Some(&BACKSLASH) => Terminator::Found {
                        payload_len: offset,
                        sequence_len: offset + 2,
                    },
                    Some(_) => Terminator::Aborted { at: offset },
                    None => Terminator::Incomplete,
                };
            }
            _ => {}
        }
    }
    Terminator::Incomplete
}

/// The canonical form of a root Claude Code record, or `None` for probes,
/// clears, child records, other apps and payloads the parser refuses.
fn root_record(payload: &[u8]) -> Option<String> {
    let ProgramStatusReport::Record(record) = parse_program_status(payload)? else {
        return None;
    };
    if record.app.as_deref() != Some(CLAUDE_APP) {
        return None;
    }
    let canonical = record.canonical();
    canonical
        .strip_prefix(&format!("app={CLAUDE_APP}:"))
        .map(str::to_owned)
}

/// One page of a `session output --json` result.
#[derive(Debug)]
pub(super) struct OutputPage {
    /// The decoded output bytes.
    pub(super) bytes: Vec<u8>,
    /// The worker instance and runtime generation every returned offset is
    /// scoped to.
    pub(super) identity: (String, String),
    /// First retained offset of the runtime.
    pub(super) history_start_offset: u64,
    /// Cursor of the next read.
    pub(super) next_offset: u64,
    /// Whether the requested cursor was evicted from retained history.
    pub(super) gap: bool,
    /// Whether more bytes are available right after `next_offset`.
    pub(super) has_more: bool,
}

impl OutputPage {
    /// Decodes the `ok` member of a `session output --json` document.
    pub(super) fn parse(value: &Value) -> Result<Self, String> {
        let text = |name: &str| -> Result<String, String> {
            match value.get(name) {
                Some(Value::String(text)) => Ok(text.clone()),
                Some(Value::Number(number)) => Ok(number.to_string()),
                _ => Err(format!("the output result has no `{name}`")),
            }
        };
        let offset = |name: &str| -> Result<u64, String> {
            text(name)?
                .parse::<u64>()
                .map_err(|error| format!("the output result `{name}` is not an offset: {error}"))
        };
        let flag = |name: &str| -> Result<bool, String> {
            value
                .get(name)
                .and_then(Value::as_bool)
                .ok_or_else(|| format!("the output result has no `{name}`"))
        };
        let bytes = STANDARD.decode(text("data_base64")?).map_err(|error| {
            format!("the output result `data_base64` is not standard base64: {error}")
        })?;
        Ok(Self {
            bytes,
            identity: (text("worker_instance_id")?, text("runtime_generation")?),
            history_start_offset: offset("history_start_offset")?,
            next_offset: offset("next_offset")?,
            gap: value.get("gap").is_some_and(|gap| !gap.is_null()),
            has_more: flag("has_more")?,
        })
    }
}
