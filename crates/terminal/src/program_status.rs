//! OSC 7501 "program status": record parsing and the feature-detection probe.
//!
//! A program reports `ESC ] 7501 ; state=<state>[:app=..][:kind=..] ST` and
//! asks whether the terminal understands the protocol with `ESC ] 7501 ; ? ST`.
//! The session worker and the daemon share this module, so both read the same
//! grammar.

// Rust guideline compliant 2026-06-26

use base64::engine::general_purpose::GeneralPurpose;
use base64::engine::{DecodePaddingMode, GeneralPurposeConfig};
use base64::{alphabet, Engine as _};

/// OSC command number of the program status protocol.
pub const PROGRAM_STATUS_COMMAND: &[u8] = b"7501";

/// The one reply that answers a probe: `ESC ] 7501 ; ? ESC \`.
///
/// It carries no CR or LF, so a program that echoes its input cannot turn it
/// into a submission.
pub const PROBE_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

/// Longest key a record may carry; a longer key discards the whole record.
const MAX_KEY_BYTES: usize = 16;

/// Longest decoded `msg` text; a longer text discards the whole record.
const MAX_MESSAGE_BYTES: usize = 2048;

/// Longest decoded `title` text; a longer text discards the whole record.
const MAX_TITLE_BYTES: usize = 192;

/// Longest `app` name; the charset is `[A-Za-z0-9_.+-]`.
const MAX_APP_BYTES: usize = 32;

/// Longest probe reply the worker drops from an attach input frame.
///
/// Matches the OSC payload bound the output parsers apply.
const MAX_REPLY_BYTES: usize = 4096;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Base64 for `msg` and `title`: the standard alphabet, padding optional.
const TEXT_ENGINE: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// What a blocked program waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    /// A permission prompt.
    Permission,
    /// A question to the user.
    Question,
    /// An authentication step.
    Auth,
}

impl BlockedKind {
    /// Returns the wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }
}

/// State a program reports about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    /// The program runs a task.
    Working,
    /// The program waits for the user.
    Blocked(Option<BlockedKind>),
    /// The program finished a task.
    Done,
    /// The program waits for input.
    Idle,
    /// The program failed.
    Error,
}

impl ProgramState {
    /// Returns the wire spelling of the state without its kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Blocked(_) => "blocked",
            Self::Done => "done",
            Self::Idle => "idle",
            Self::Error => "error",
        }
    }
}

/// A root status record: the program's own state, not one of its children's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatusRecord {
    /// Program name, when the record names one.
    pub app: Option<String>,
    /// Reported state.
    pub state: ProgramState,
}

impl ProgramStatusRecord {
    /// Renders `app=<app>:state=<state>[:kind=<kind>]`, or an empty app part
    /// when the record names none.
    #[must_use]
    pub fn canonical(&self) -> String {
        let mut text = format!(
            "app={}:state={}",
            self.app.as_deref().unwrap_or_default(),
            self.state.as_str()
        );
        if let ProgramState::Blocked(Some(kind)) = self.state {
            text.push_str(":kind=");
            text.push_str(kind.as_str());
        }
        text
    }
}

/// One parsed OSC 7501 payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramStatusReport {
    /// A feature-detection query (`?`), never a record.
    Probe,
    /// A root record.
    Record(ProgramStatusRecord),
    /// A root `state=clear`, with or without an app.
    Clear,
    /// A record of a child (an `id=` pair), including a child clear.
    Child,
}

/// Parses the payload of an OSC 7501 sequence (the bytes after `7501;`).
///
/// Returns `None` when the whole report must be discarded: an unknown or
/// missing `state`, a key over 16 bytes, invalid base64 in `msg` or `title`, a
/// decoded control character, or text over its decoded limit. A malformed
/// pair is skipped and an unknown key is ignored. `msg`, `title` and
/// `progress` are never returned.
#[must_use]
pub fn parse_payload(payload: &[u8]) -> Option<ProgramStatusReport> {
    if payload.first() == Some(&b'?') {
        return Some(ProgramStatusReport::Probe);
    }
    let text = std::str::from_utf8(payload).ok()?;
    let mut state = None;
    let mut clear = false;
    let mut app = None;
    let mut kind = None;
    let mut child = false;
    for pair in text.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        if key.len() > MAX_KEY_BYTES {
            return None;
        }
        match key {
            "state" => match value {
                "clear" => {
                    clear = true;
                    state = None;
                }
                "working" => (state, clear) = (Some(ProgramState::Working), false),
                "blocked" => (state, clear) = (Some(ProgramState::Blocked(None)), false),
                "done" => (state, clear) = (Some(ProgramState::Done), false),
                "idle" => (state, clear) = (Some(ProgramState::Idle), false),
                "error" => (state, clear) = (Some(ProgramState::Error), false),
                _ => return None,
            },
            "app" => app = valid_app(value).then(|| value.to_owned()),
            "kind" => {
                kind = match value {
                    "permission" => Some(BlockedKind::Permission),
                    "question" => Some(BlockedKind::Question),
                    "auth" => Some(BlockedKind::Auth),
                    _ => None,
                };
            }
            "id" => child = true,
            "msg" => validate_text(value, MAX_MESSAGE_BYTES)?,
            "title" => validate_text(value, MAX_TITLE_BYTES)?,
            _ => {}
        }
    }
    if !clear && state.is_none() {
        return None;
    }
    if child {
        return Some(ProgramStatusReport::Child);
    }
    if clear {
        return Some(ProgramStatusReport::Clear);
    }
    let state = match (state?, kind) {
        (ProgramState::Blocked(_), kind) => ProgramState::Blocked(kind),
        (state, _) => state,
    };
    Some(ProgramStatusReport::Record(ProgramStatusRecord {
        app,
        state,
    }))
}

fn valid_app(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_APP_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-'))
}

/// Checks that `encoded` is base64 of UTF-8 text within `limit` bytes without
/// control characters. The text is dropped.
fn validate_text(encoded: &str, limit: usize) -> Option<()> {
    let decoded = TEXT_ENGINE.decode(encoded).ok()?;
    if decoded.len() > limit {
        return None;
    }
    let text = std::str::from_utf8(&decoded).ok()?;
    let has_control = text.chars().any(|ch| ch.is_control());
    (!has_control).then_some(())
}

/// Byte span of one probe sequence in the output stream, `end` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeSpan {
    /// Stream offset of the introducing `ESC`.
    pub start: u64,
    /// Stream offset after the terminator.
    pub end: u64,
}

/// Decides which probes the worker answers.
///
/// The reply equals the query, so a program that copies its input to its
/// output would otherwise loop. After a probe was answered, an immediately
/// following probe (no other output byte in between) is not answered; any
/// other byte re-arms answering.
#[derive(Debug, Default)]
pub struct ProbeGuard {
    last_end: Option<u64>,
    suppress: bool,
}

impl ProbeGuard {
    /// Returns whether `span` is answered.
    pub fn admit(&mut self, span: ProbeSpan) -> bool {
        if self.last_end != Some(span.start) {
            self.suppress = false;
        }
        self.last_end = Some(span.end);
        if self.suppress {
            return false;
        }
        self.suppress = true;
        true
    }
}

/// Removes every complete probe reply from one input frame.
///
/// The worker is the only responder for its PTY while it answers probes, so an
/// attached terminal's own reply must not reach the program a second time. A
/// reply split across frames is not recognized and passes through. The rest of
/// the frame keeps its order.
#[must_use]
pub fn strip_probe_replies(input: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    const PREFIX: &[u8] = b"\x1b]7501;?";
    let mut stripped: Option<Vec<u8>> = None;
    let mut copied = 0;
    let mut index = 0;
    while index < input.len() {
        if input[index] == ESC && input[index..].starts_with(PREFIX) {
            if let Some(length) = reply_length(&input[index..]) {
                let kept = stripped.get_or_insert_with(|| Vec::with_capacity(input.len()));
                kept.extend_from_slice(&input[copied..index]);
                index += length;
                copied = index;
                continue;
            }
        }
        index += 1;
    }
    match stripped {
        None => std::borrow::Cow::Borrowed(input),
        Some(mut kept) => {
            kept.extend_from_slice(&input[copied..]);
            std::borrow::Cow::Owned(kept)
        }
    }
}

/// Length of the complete reply at the start of `bytes`, which begins with
/// `ESC ] 7501 ; ?`.
fn reply_length(bytes: &[u8]) -> Option<usize> {
    let limit = bytes.len().min(MAX_REPLY_BYTES);
    let mut index = b"\x1b]7501;?".len();
    while index < limit {
        match bytes[index] {
            BEL => return Some(index + 1),
            ESC => {
                return (bytes.get(index + 1) == Some(&b'\\')).then_some(index + 2);
            }
            _ => index += 1,
        }
    }
    None
}
