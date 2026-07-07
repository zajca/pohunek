const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const MAX_OSC_COMMAND_BYTES: usize = 16;
const MAX_SUPPORTED_OSC_PAYLOAD_BYTES: usize = 4096;
const FILE_URL_PREFIX: &[u8] = b"file://";
const PERCENT_HEX_DIGITS: usize = 2;
const HEX_HIGH_NIBBLE_SHIFT: u8 = 4;
const HEX_ALPHA_OFFSET: u8 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvidence {
    Title(String),
    Progress(String),
    Cwd(String),
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
        let mut evidence = Vec::new();

        for &byte in bytes {
            match self.state {
                State::Ground => self.advance_ground(byte),
                State::Escape => self.advance_escape(byte),
                State::OscCommand => self.advance_osc_command(byte),
                State::OscCommandEscape => self.advance_osc_command_escape(byte),
                State::OscPayload => self.advance_osc_payload(byte, &mut evidence),
                State::OscPayloadEscape => self.advance_osc_payload_escape(byte, &mut evidence),
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

    fn advance_osc_payload(&mut self, byte: u8, evidence: &mut Vec<OscEvidence>) {
        match byte {
            BEL => self.finish_osc(evidence),
            ESC => self.state = State::OscPayloadEscape,
            _ => self.push_payload_byte(byte),
        }
    }

    fn advance_osc_payload_escape(&mut self, byte: u8, evidence: &mut Vec<OscEvidence>) {
        match byte {
            b'\\' => self.finish_osc(evidence),
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
        matches!(self.command.as_slice(), b"0" | b"2" | b"7" | b"9")
    }

    fn finish_osc(&mut self, evidence: &mut Vec<OscEvidence>) {
        let item = match self.command.as_slice() {
            b"0" | b"2" => Some(OscEvidence::Title(self.payload_string())),
            b"7" => self.cwd_payload().map(OscEvidence::Cwd),
            b"9" => Some(OscEvidence::Progress(self.payload_string())),
            _ => None,
        };

        if let Some(item) = item {
            evidence.push(item);
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
    fn parses_osc_9_progress_fragmented_and_bel_terminated() {
        let mut parser = OscParser::new();

        assert!(parser.advance(b"\x1b]9;pro").is_empty());
        assert_eq!(
            parser.advance(b"gress=42\x07"),
            vec![OscEvidence::Progress("progress=42".to_string())]
        );
    }

    #[test]
    fn parses_osc_9_progress_st_terminated() {
        let mut parser = OscParser::new();

        assert_eq!(
            parser.advance(b"\x1b]9;50%\x1b\\"),
            vec![OscEvidence::Progress("50%".to_string())]
        );
    }

    #[test]
    fn parses_osc_7_cwd_fragmented_and_bel_terminated() {
        let mut parser = OscParser::new();

        assert!(parser.advance(b"\x1b]7;file://").is_empty());
        assert!(parser.advance(b"localhost/tmp/proj").is_empty());
        assert_eq!(
            parser.advance(b"ect\x07"),
            vec![OscEvidence::Cwd("/tmp/project".to_string())]
        );
    }

    #[test]
    fn parses_osc_7_cwd_st_terminated_and_percent_decoded() {
        let mut parser = OscParser::new();

        assert_eq!(
            parser.advance(b"\x1b]7;file:///tmp/has%20space\x1b\\"),
            vec![OscEvidence::Cwd("/tmp/has space".to_string())]
        );
    }

    #[test]
    fn ignores_osc_7_non_file_url() {
        let mut parser = OscParser::new();

        assert!(parser
            .advance(b"\x1b]7;ssh://host/tmp/project\x07")
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
