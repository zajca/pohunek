use pohunek_terminal::{
    parse_program_status, ProgramStatusReport, PROGRAM_STATUS_COMMAND as PROGRAM_STATUS,
};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const MAX_OSC_COMMAND_BYTES: usize = 16;
const MAX_SUPPORTED_OSC_PAYLOAD_BYTES: usize = 4096;
const FILE_URL_PREFIX: &[u8] = b"file://";
const PERCENT_HEX_DIGITS: usize = 2;
const HEX_HIGH_NIBBLE_SHIFT: u8 = 4;
const HEX_ALPHA_OFFSET: u8 = 10;

/// The debug-journal target of a 7501 record the detector cannot map.
const UNMAPPED_PROGRAM_STATUS_TARGET: &str = "session.program_status.unmapped";

/// Longest payload token the journal echoes.
///
/// The journal echoes untrusted tokens, which the grammar does not bound: an
/// unknown `state` or `kind`, or the `app` of a rejected payload. The bound
/// keeps such a token from turning into a long log line.
const MAX_JOURNAL_TOKEN_CHARS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvidence {
    Title(String),
    /// A well-formed `OSC 9 ; 4 ; <state> [; <percent>]` progress report.
    Progress(ProgressReport),
    /// A desktop notification: a plain `OSC 9 ; <text>`, an `OSC 777 ; notify`
    /// or a complete `OSC 99` title or body. Its title and body are joined by
    /// a newline. It is not progress.
    Notice(String),
    Cwd(String),
    /// A parsed OSC 7501 program-status report, never a probe.
    ProgramStatus(ProgramStatusReport),
}

/// The `ConEmu` progress state of an `OSC 9 ; 4` report.
///
/// State 4 (`Warning`) is parsed but no manifest maps it to an activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    /// State 0: the progress indicator is removed.
    Remove,
    /// State 1: determinate progress.
    Normal,
    /// State 2: an error.
    Error,
    /// State 3: indeterminate progress.
    Indeterminate,
    /// State 4: a warning.
    Warning,
}

impl ProgressState {
    fn from_digit(digit: &[u8]) -> Option<Self> {
        match digit {
            b"0" => Some(Self::Remove),
            b"1" => Some(Self::Normal),
            b"2" => Some(Self::Error),
            b"3" => Some(Self::Indeterminate),
            b"4" => Some(Self::Warning),
            _ => None,
        }
    }

    fn digit(self) -> u8 {
        match self {
            Self::Remove => 0,
            Self::Normal => 1,
            Self::Error => 2,
            Self::Indeterminate => 3,
            Self::Warning => 4,
        }
    }
}

/// A typed OSC 9;4 progress report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressReport {
    pub state: ProgressState,
    /// The reported percentage, 0 to 100, when the sequence carries one.
    pub percent: Option<u8>,
}

impl ProgressReport {
    /// The text manifests match in the `osc_progress` region: `4;<state>`.
    ///
    /// The percentage is left out so a rule such as `^4;0` keys on the state
    /// alone and a moving percentage is not a region change.
    #[must_use]
    pub fn region_text(&self) -> String {
        format!("{PROGRESS_SUBCOMMAND};{}", self.state.digit())
    }
}

/// The `ConEmu` OSC 9 subcommand that carries progress.
const PROGRESS_SUBCOMMAND: &str = "4";
const MAX_PROGRESS_PERCENT: u8 = 100;

/// Classifies an OSC 9 payload. `None` is a malformed `4` subcommand, which is
/// neither progress nor a notice.
fn classify_osc9(payload: &[u8]) -> Option<OscEvidence> {
    let mut parts = payload.split(|&byte| byte == b';');
    let head = parts.next().unwrap_or_default();
    if head != PROGRESS_SUBCOMMAND.as_bytes() {
        return Some(OscEvidence::Notice(
            String::from_utf8_lossy(payload).into_owned(),
        ));
    }
    let state = ProgressState::from_digit(parts.next()?)?;
    let percent = match parts.next() {
        None | Some(b"") => None,
        Some(digits) => Some(parse_percent(digits)?),
    };
    if parts.next().is_some() {
        return None;
    }
    Some(OscEvidence::Progress(ProgressReport { state, percent }))
}

/// The `OSC 777` extension that carries a notification.
const OSC777_NOTIFY: &[u8] = b"notify";

/// `OSC 777 ; notify ; <title> ; <body>`. Any other `777` extension is
/// ignored.
fn classify_osc777(payload: &[u8]) -> Option<OscEvidence> {
    let mut parts = payload.splitn(3, |&byte| byte == b';');
    if parts.next()? != OSC777_NOTIFY {
        return None;
    }
    let title = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default();
    Some(OscEvidence::Notice(format!(
        "{}\n{}",
        String::from_utf8_lossy(title),
        String::from_utf8_lossy(body)
    )))
}

/// `OSC 99 ; <metadata> ; <payload>` of the kitty notification protocol.
///
/// Only a complete, unencoded `title` or `body` payload is a notice: a chunked
/// notification (`d=0`), a base64 payload (`e=1`), a query, a close and any
/// other payload type are ignored.
fn classify_osc99(payload: &[u8]) -> Option<OscEvidence> {
    let split = payload.iter().position(|&byte| byte == b';')?;
    let (metadata, text) = (&payload[..split], &payload[split + 1..]);
    let mut payload_type: &[u8] = b"title";
    for pair in metadata.split(|&byte| byte == b':') {
        let Some(eq) = pair.iter().position(|&byte| byte == b'=') else {
            continue;
        };
        let (key, value) = (&pair[..eq], &pair[eq + 1..]);
        match key {
            b"d" if value == b"0" => return None,
            b"e" if value != b"0" => return None,
            b"p" => payload_type = value,
            _ => {}
        }
    }
    if payload_type != b"title" && payload_type != b"body" {
        return None;
    }
    Some(OscEvidence::Notice(
        String::from_utf8_lossy(text).into_owned(),
    ))
}

fn parse_percent(digits: &[u8]) -> Option<u8> {
    if digits.is_empty() || digits.len() > 3 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    u8::try_from(value)
        .ok()
        .filter(|percent| *percent <= MAX_PROGRESS_PERCENT)
}

#[derive(Debug, Default)]
pub struct OscParser {
    state: State,
    command: Vec<u8>,
    payload: Vec<u8>,
}

impl OscParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&mut self, bytes: &[u8]) -> Vec<OscEvidence> {
        self.advance_with_offsets(bytes)
            .into_iter()
            .map(|(item, _end)| item)
            .collect()
    }

    /// Returns completed OSC events with exclusive byte offsets.
    ///
    /// Parsing matches [`Self::advance`]. Each offset includes the terminator
    /// and is relative to `bytes`, even when an OSC spans multiple feeds.
    /// Callers can apply screen prefixes before processing each event.
    pub fn advance_with_offsets(&mut self, bytes: &[u8]) -> Vec<(OscEvidence, usize)> {
        let mut evidence = Vec::new();

        for (offset, &byte) in bytes.iter().enumerate() {
            match self.state {
                State::Ground => self.advance_ground(byte),
                State::Escape => self.advance_escape(byte),
                State::OscCommand => self.advance_osc_command(byte),
                State::OscCommandEscape => self.advance_osc_command_escape(byte),
                State::OscPayload => self.advance_osc_payload(byte, offset, &mut evidence),
                State::OscPayloadEscape => {
                    self.advance_osc_payload_escape(byte, offset, &mut evidence);
                }
                State::OscDiscard => self.advance_osc_discard(byte),
                State::OscDiscardEscape => self.advance_osc_discard_escape(byte),
            }
        }

        evidence
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        !matches!(self.state, State::Ground)
    }

    /// Clears buffered OSC state.
    ///
    /// Milestone 5 can call this on foreground-process changes. Milestone 6
    /// will replace the coarse reset with tighter process tracking.
    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.command.clear();
        self.payload.clear();
    }

    fn advance_ground(&mut self, byte: u8) {
        if byte == ESC {
            self.state = State::Escape;
        }
    }

    fn advance_escape(&mut self, byte: u8) {
        match byte {
            b']' => self.start_osc(),
            ESC => self.state = State::Escape,
            _ => self.state = State::Ground,
        }
    }

    fn advance_osc_command(&mut self, byte: u8) {
        match byte {
            BEL => self.reset(),
            b';' => {
                if self.is_supported_command() {
                    self.state = State::OscPayload;
                } else {
                    self.discard_osc();
                }
            }
            ESC => self.state = State::OscCommandEscape,
            _ => self.push_command_byte(byte),
        }
    }

    fn advance_osc_command_escape(&mut self, byte: u8) {
        match byte {
            b']' => self.start_osc(),
            ESC => {
                self.reset();
                self.state = State::Escape;
            }
            _ => self.reset(),
        }
    }

    fn advance_osc_payload(
        &mut self,
        byte: u8,
        offset: usize,
        evidence: &mut Vec<(OscEvidence, usize)>,
    ) {
        match byte {
            BEL => self.finish_osc(offset, evidence),
            ESC => self.state = State::OscPayloadEscape,
            _ => self.push_payload_byte(byte),
        }
    }

    fn advance_osc_payload_escape(
        &mut self,
        byte: u8,
        offset: usize,
        evidence: &mut Vec<(OscEvidence, usize)>,
    ) {
        match byte {
            b'\\' => self.finish_osc(offset, evidence),
            b']' => self.start_osc(),
            ESC => {
                self.reset();
                self.state = State::Escape;
            }
            _ => self.reset(),
        }
    }

    fn advance_osc_discard(&mut self, byte: u8) {
        match byte {
            BEL => self.reset(),
            ESC => self.state = State::OscDiscardEscape,
            _ => {}
        }
    }

    fn advance_osc_discard_escape(&mut self, byte: u8) {
        match byte {
            BEL | b'\\' => self.reset(),
            b']' => self.start_osc(),
            ESC => self.state = State::OscDiscardEscape,
            _ => self.state = State::OscDiscard,
        }
    }

    fn start_osc(&mut self) {
        self.command.clear();
        self.payload.clear();
        self.state = State::OscCommand;
    }

    fn discard_osc(&mut self) {
        self.command.clear();
        self.payload.clear();
        self.state = State::OscDiscard;
    }

    fn push_command_byte(&mut self, byte: u8) {
        if self.command.len() >= MAX_OSC_COMMAND_BYTES {
            self.discard_osc();
            return;
        }

        self.command.push(byte);
    }

    fn push_payload_byte(&mut self, byte: u8) {
        if self.payload.len() >= MAX_SUPPORTED_OSC_PAYLOAD_BYTES {
            self.discard_osc();
            return;
        }

        self.payload.push(byte);
    }

    fn is_supported_command(&self) -> bool {
        matches!(
            self.command.as_slice(),
            b"0" | b"2" | b"7" | b"9" | b"99" | b"777"
        ) || self.command == PROGRAM_STATUS
    }

    fn finish_osc(&mut self, offset: usize, evidence: &mut Vec<(OscEvidence, usize)>) {
        let item = match self.command.as_slice() {
            b"0" | b"2" => Some(OscEvidence::Title(self.payload_string())),
            b"7" => self.cwd_payload().map(OscEvidence::Cwd),
            b"9" => classify_osc9(&self.payload),
            b"777" => classify_osc777(&self.payload),
            b"99" => classify_osc99(&self.payload),
            command if command == PROGRAM_STATUS => match parse_program_status(&self.payload) {
                Some(ProgramStatusReport::Probe) => None,
                report => {
                    journal_unmapped_program_status(&self.payload, report.as_ref());
                    report.map(OscEvidence::ProgramStatus)
                }
            },
            _ => None,
        };

        if let Some(item) = item {
            evidence.push((item, offset + 1));
        }

        self.reset();
    }

    fn payload_string(&self) -> String {
        String::from_utf8_lossy(&self.payload).into_owned()
    }

    fn cwd_payload(&self) -> Option<String> {
        parse_file_url_cwd(&self.payload)
    }
}

/// The `state`, `kind` and `app` tokens of a 7501 payload, plus the presence of
/// `msg` and `title`.
#[derive(Debug, Default)]
struct JournalTokens<'a> {
    state: Option<&'a [u8]>,
    kind: Option<&'a [u8]>,
    app: Option<&'a [u8]>,
    has_msg: bool,
    has_title: bool,
}

impl<'a> JournalTokens<'a> {
    /// Collects the tokens of `payload`.
    ///
    /// Pairs split as the parser splits them: on `:` first, then on the first
    /// `=`; a pair without `=` and a pair with an empty key carry no token. A
    /// repeated key keeps its last value. The journal can still carry a token
    /// the parsed report never saw or kept: the parser rejects the payload at
    /// its first invalid input (non-UTF-8 bytes, an overlong key, invalid
    /// `msg` or `title`, or a missing or unknown `state`) and drops an invalid
    /// `app` or an unknown `kind` it read, so a journaled token is one the
    /// parser never read or one it discarded.
    fn of(payload: &'a [u8]) -> Self {
        let mut tokens = Self::default();
        for pair in payload.split(|&byte| byte == b':') {
            let Some(eq) = pair.iter().position(|&byte| byte == b'=') else {
                continue;
            };
            let (key, value) = (&pair[..eq], &pair[eq + 1..]);
            match key {
                b"state" => tokens.state = Some(value),
                b"kind" => tokens.kind = Some(value),
                b"app" => tokens.app = Some(value),
                b"msg" => tokens.has_msg = true,
                b"title" => tokens.has_title = true,
                _ => {}
            }
        }
        tokens
    }

    /// Whether the payload names a `kind` the grammar does not know.
    fn kind_is_unknown(&self) -> bool {
        self.kind
            .is_some_and(|kind| !matches!(kind, b"permission" | b"question" | b"auth"))
    }
}

/// Bounds an untrusted payload token for the journal.
///
/// A token the payload does not carry renders empty, its control characters
/// become `?` so it cannot forge a log line, and it is cut to its first
/// [`MAX_JOURNAL_TOKEN_CHARS`] characters.
fn bounded_token(token: Option<&[u8]>) -> String {
    String::from_utf8_lossy(token.unwrap_or_default())
        .chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .take(MAX_JOURNAL_TOKEN_CHARS)
        .collect()
}

/// Journals a 7501 record the detector cannot map, never the text it carries.
///
/// A record is unmapped when the parser rejects the payload whole
/// (`reason = "rejected"`) or when the payload names a `kind` the grammar does
/// not know, which the parser drops (`reason = "unknown_kind"`). The journal
/// carries the bounded `state`, `kind` and `app` tokens and the presence of
/// `msg` and `title`: their text is untrusted and is never logged. The report
/// is only read, never changed.
fn journal_unmapped_program_status(payload: &[u8], report: Option<&ProgramStatusReport>) {
    let tokens = JournalTokens::of(payload);
    let reason = if report.is_none() {
        "rejected"
    } else {
        if !tokens.kind_is_unknown() {
            return;
        }
        "unknown_kind"
    };
    tracing::debug!(
        target: UNMAPPED_PROGRAM_STATUS_TARGET,
        reason,
        state = bounded_token(tokens.state),
        kind = bounded_token(tokens.kind),
        app = bounded_token(tokens.app),
        has_msg = tokens.has_msg,
        has_title = tokens.has_title,
        "unmapped program-status record"
    );
}

fn parse_file_url_cwd(payload: &[u8]) -> Option<String> {
    let without_scheme = payload.strip_prefix(FILE_URL_PREFIX)?;
    let path_start = without_scheme.iter().position(|&byte| byte == b'/')?;
    let path = &without_scheme[path_start..];
    if path.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&percent_decode(path)).into_owned())
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' && index + PERCENT_HEX_DIGITS < input.len() {
            if let (Some(high), Some(low)) =
                (hex_nibble(input[index + 1]), hex_nibble(input[index + 2]))
            {
                decoded.push((high << HEX_HIGH_NIBBLE_SHIFT) | low);
                index += PERCENT_HEX_DIGITS + 1;
                continue;
            }
        }
        decoded.push(input[index]);
        index += 1;
    }
    decoded
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + HEX_ALPHA_OFFSET),
        b'A'..=b'F' => Some(byte - b'A' + HEX_ALPHA_OFFSET),
        _ => None,
    }
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    OscCommand,
    OscCommandEscape,
    OscPayload,
    OscPayloadEscape,
    OscDiscard,
    OscDiscardEscape,
}

#[cfg(test)]
mod tests {
    use super::{OscEvidence, OscParser};

    #[test]
    fn parses_osc_0_title_fragmented_across_chunks_and_bel_terminated() {
        let mut parser = OscParser::new();

        assert!(parser.advance(b"\x1b").is_empty());
        assert!(parser.advance(b"]0;build").is_empty());
        assert_eq!(
            parser.advance(b" window\x07"),
            vec![OscEvidence::Title("build window".to_string())]
        );
    }

    #[test]
    fn parses_osc_2_title_fragmented_mid_payload_and_st_terminated() {
        let mut parser = OscParser::new();

        assert!(parser.advance(b"\x1b]2;edi").is_empty());
        assert!(parser.advance(b"tor").is_empty());
        assert!(parser.advance(b"\x1b").is_empty());
        assert_eq!(
            parser.advance(b"\\"),
            vec![OscEvidence::Title("editor".to_string())]
        );
    }

    #[test]
    fn parses_osc_7_cwd_fragmented_and_bel_terminated() {
        let mut parser = OscParser::new();

        assert!(parser.advance(b"\x1b]7;file://").is_empty());
        assert!(parser.advance(b"localhost/work/proj").is_empty());
        assert_eq!(
            parser.advance(b"ect\x07"),
            vec![OscEvidence::Cwd("/work/project".to_string())]
        );
    }

    #[test]
    fn parses_osc_7_cwd_st_terminated_and_percent_decoded() {
        let mut parser = OscParser::new();

        assert_eq!(
            parser.advance(b"\x1b]7;file:///work/has%20space\x1b\\"),
            vec![OscEvidence::Cwd("/work/has space".to_string())]
        );
    }

    #[test]
    fn ignores_osc_7_non_file_url() {
        let mut parser = OscParser::new();

        assert!(parser
            .advance(b"\x1b]7;ssh://host/work/project\x07")
            .is_empty());
    }

    #[test]
    fn ignores_unsupported_osc_command() {
        let mut parser = OscParser::new();

        assert!(parser.advance(b"\x1b]1;ignored\x07").is_empty());
        assert!(parser.advance(b"\x1b]52;c;abcdef\x1b\\").is_empty());
    }

    #[test]
    fn unsupported_unterminated_osc_discards_payload_and_recovers_on_new_osc_start() {
        let mut parser = OscParser::new();

        assert!(parser.advance(b"\x1b]52;c;").is_empty());
        assert!(parser.advance(&vec![b'a'; 8 * 1024]).is_empty());
        assert!(
            parser.payload.is_empty(),
            "unsupported OSC payload should be discarded instead of buffered"
        );
        assert_eq!(
            parser.advance(b"\x1b]2;ready\x07"),
            vec![OscEvidence::Title("ready".to_string())]
        );
    }

    #[test]
    fn unsupported_osc_escape_bel_resets_discard_and_later_title_parses() {
        let mut parser = OscParser::new();

        assert!(parser.advance(b"\x1b]52;c;discarded\x1b").is_empty());
        assert!(parser.advance(b"\x07").is_empty());
        assert!(
            matches!(&parser.state, super::State::Ground),
            "BEL after ESC in discard mode should reset to ground"
        );
        assert_eq!(
            parser.advance(b"\x1b]2;ready\x07"),
            vec![OscEvidence::Title("ready".to_string())]
        );
    }

    #[test]
    fn oversized_supported_payload_is_dropped_and_later_valid_title_parses() {
        let mut parser = OscParser::new();
        let oversized = vec![b'x'; 8 * 1024];

        assert!(parser.advance(b"\x1b]0;").is_empty());
        assert!(parser.advance(&oversized).is_empty());
        assert_eq!(
            parser.advance(b"\x07\x1b]2;after\x07"),
            vec![OscEvidence::Title("after".to_string())]
        );
    }

    #[test]
    fn oversized_command_is_dropped_and_later_valid_title_parses() {
        let mut parser = OscParser::new();
        let oversized = [b'1'; 128];

        assert!(parser.advance(b"\x1b]").is_empty());
        assert!(parser.advance(&oversized).is_empty());
        assert!(
            parser.command.is_empty(),
            "oversized OSC command should reset instead of growing unbounded"
        );
        assert_eq!(
            parser.advance(b"\x1b]2;after\x07"),
            vec![OscEvidence::Title("after".to_string())]
        );
    }

    #[test]
    fn aborted_or_stray_osc_does_not_wedge_later_valid_title() {
        let mut parser = OscParser::new();

        assert!(parser.advance(b"\x1b]0;unterminated\x1b[31m").is_empty());
        assert_eq!(
            parser.advance(b"\x1b]2;ready\x07"),
            vec![OscEvidence::Title("ready".to_string())]
        );
    }

    #[test]
    fn reset_clears_partial_osc_sequence() {
        let mut parser = OscParser::new();

        assert!(parser.advance(b"\x1b]0;old").is_empty());
        parser.reset();
        assert_eq!(
            parser.advance(b"\x07\x1b]2;new\x07"),
            vec![OscEvidence::Title("new".to_string())]
        );
    }
}
