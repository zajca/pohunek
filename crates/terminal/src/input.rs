//! Translates physical terminal input reports into the agent grid.
//!
//! The attach compositor reserves physical rows above the agent grid. This
//! module rewrites coordinate-carrying reports emitted by the physical terminal
//! back into coordinates relative to the agent's PTY grid.

// Rust guideline compliant 2026-07-07

use crate::{MouseProtocolEncoding, MouseProtocolMode};

/// ASCII Escape starts terminal control reports.
const ESC: u8 = 0x1b;
/// CSI reports handled here use `ESC [` rather than the single-byte C1 form.
const CSI_OPEN: u8 = b'[';
/// SGR mouse reports use `ESC [ < ...`.
const SGR_MARKER: u8 = b'<';
/// Legacy and UTF-8 mouse reports use `ESC [ M ...`.
const LEGACY_MARKER: u8 = b'M';
/// SGR mouse press, motion, and wheel reports end with uppercase `M`.
const SGR_EVENT_FINAL: u8 = b'M';
/// SGR mouse release reports end with lowercase `m`.
const SGR_RELEASE_FINAL: u8 = b'm';
/// Cursor-position reports end with uppercase `R`.
const CPR_FINAL: u8 = b'R';
/// CSI parameters are separated by semicolons.
const PARAM_SEPARATOR: u8 = b';';
/// Decimal parsing is base ten because terminal coordinates are decimal ASCII.
const DECIMAL_RADIX: u32 = 10;
/// ASCII digit values start at `0`.
const ASCII_ZERO: u8 = b'0';
/// `ESC [` has two bytes before the report discriminator.
const CSI_PREFIX_LEN: usize = 2;
/// SGR mouse parameters start after `ESC [ <`.
const SGR_PARAM_START: usize = 3;
/// CPR row parameters start after `ESC [`.
const CPR_PARAM_START: usize = 2;
/// Legacy/UTF-8 mouse payload starts after `ESC [ M`.
const LEGACY_PARAM_START: usize = 3;
/// Legacy X10 reports are exactly `ESC [ M Cb Cx Cy`.
const LEGACY_REPORT_LEN: usize = 6;
/// The row byte is the final byte in a legacy X10 report.
const LEGACY_ROW_INDEX: usize = 5;
/// X10 and UTF-8 mouse coordinates are encoded as value plus 32.
const MOUSE_COORD_OFFSET: u32 = 32;
/// Legacy X10 uses the same offset in a single byte.
const LEGACY_COORD_OFFSET: u8 = 32;
/// UTF-8 scalar values are at most four bytes long.
const UTF8_MAX_BYTES: usize = 4;
/// UTF-8 one-byte scalars are below this leading-byte value.
const UTF8_ONE_BYTE_LIMIT: u8 = 0x80;
/// Leading-byte mask for two-byte UTF-8 scalars.
const UTF8_TWO_BYTE_MASK: u8 = 0b1110_0000;
/// Leading-byte tag for two-byte UTF-8 scalars.
const UTF8_TWO_BYTE_TAG: u8 = 0b1100_0000;
/// Leading-byte mask for three-byte UTF-8 scalars.
const UTF8_THREE_BYTE_MASK: u8 = 0b1111_0000;
/// Leading-byte tag for three-byte UTF-8 scalars.
const UTF8_THREE_BYTE_TAG: u8 = 0b1110_0000;
/// Leading-byte mask for four-byte UTF-8 scalars.
const UTF8_FOUR_BYTE_MASK: u8 = 0b1111_1000;
/// Leading-byte tag for four-byte UTF-8 scalars.
const UTF8_FOUR_BYTE_TAG: u8 = 0b1111_0000;

/// Rewrites coordinate-carrying terminal input reports.
///
/// Construct one translator per attach loop and feed each stdin chunk through
/// [`InputTranslator::push`]. The translator owns partial input state so reports
/// split across reads can be completed before they are emitted.
#[derive(Debug, Clone)]
pub struct InputTranslator {
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
    pending: Vec<u8>,
}

impl Default for InputTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl InputTranslator {
    /// Creates a translator with mouse translation disabled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mode: MouseProtocolMode::None,
            encoding: MouseProtocolEncoding::Default,
            pending: Vec::new(),
        }
    }

    /// Sets the mouse protocol state used for the next pushed chunk.
    pub fn set_mouse_protocol(&mut self, mode: MouseProtocolMode, encoding: MouseProtocolEncoding) {
        self.mode = mode;
        self.encoding = encoding;
    }

    /// Translates one stdin byte chunk.
    ///
    /// Mouse reports are translated only when mouse reporting is enabled.
    /// Cursor-position reports are translated regardless of mouse mode.
    #[must_use]
    pub fn push(&mut self, input: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.pending.len() + input.len());
        bytes.append(&mut self.pending);
        bytes.extend_from_slice(input);

        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match self.parse_at(&bytes[index..]) {
                Parse::Emit { consumed, bytes } => {
                    out.extend_from_slice(&bytes);
                    index += consumed;
                }
                Parse::Swallow { consumed } => {
                    index += consumed;
                }
                Parse::Pass { consumed } => {
                    out.extend_from_slice(&bytes[index..index + consumed]);
                    index += consumed;
                }
                Parse::Pending => {
                    self.pending.extend_from_slice(&bytes[index..]);
                    break;
                }
            }
        }
        out
    }

    fn parse_at(&self, bytes: &[u8]) -> Parse {
        if bytes[0] != ESC {
            return Parse::Pass { consumed: 1 };
        }
        if bytes.len() == 1 {
            return Parse::Pending;
        }
        if bytes[1] != CSI_OPEN {
            return Parse::Pass { consumed: 1 };
        }
        if bytes.len() == CSI_PREFIX_LEN {
            return Parse::Pending;
        }

        match bytes[CSI_PREFIX_LEN] {
            SGR_MARKER if self.mouse_enabled() && self.encoding == MouseProtocolEncoding::Sgr => {
                parse_sgr_mouse(bytes)
            }
            LEGACY_MARKER if self.mouse_enabled() => match self.encoding {
                MouseProtocolEncoding::Default => parse_legacy_mouse(bytes),
                MouseProtocolEncoding::Utf8 => parse_utf8_mouse(bytes),
                MouseProtocolEncoding::Sgr => Parse::Pass { consumed: 1 },
            },
            byte if byte.is_ascii_digit() => parse_cpr(bytes),
            _ => Parse::Pass { consumed: 1 },
        }
    }

    fn mouse_enabled(&self) -> bool {
        self.mode != MouseProtocolMode::None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Parse {
    Emit { consumed: usize, bytes: Vec<u8> },
    Swallow { consumed: usize },
    Pass { consumed: usize },
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Number {
    value: u32,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberParse {
    Complete(Number),
    Invalid { consumed: usize },
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Utf8Char {
    value: char,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Utf8Parse {
    Complete(Utf8Char),
    Invalid { consumed: usize },
    Pending,
}

fn parse_sgr_mouse(bytes: &[u8]) -> Parse {
    let button = match parse_number(bytes, SGR_PARAM_START) {
        NumberParse::Complete(number) => number,
        NumberParse::Invalid { consumed } => return Parse::Pass { consumed },
        NumberParse::Pending => return Parse::Pending,
    };
    if !consume_separator(bytes, button.end) {
        return parse_separator_result(bytes, button.end);
    }

    let column = match parse_number(bytes, button.end + 1) {
        NumberParse::Complete(number) => number,
        NumberParse::Invalid { consumed } => return Parse::Pass { consumed },
        NumberParse::Pending => return Parse::Pending,
    };
    if !consume_separator(bytes, column.end) {
        return parse_separator_result(bytes, column.end);
    }

    let row = match parse_number(bytes, column.end + 1) {
        NumberParse::Complete(number) => number,
        NumberParse::Invalid { consumed } => return Parse::Pass { consumed },
        NumberParse::Pending => return Parse::Pending,
    };
    if row.end == bytes.len() {
        return Parse::Pending;
    }

    let final_byte = bytes[row.end];
    if !matches!(final_byte, SGR_EVENT_FINAL | SGR_RELEASE_FINAL) {
        return Parse::Pass {
            consumed: row.end + 1,
        };
    }
    let consumed = row.end + 1;
    let Some(translated_row) = translate_mouse_row(row.value) else {
        return Parse::Swallow { consumed };
    };

    let mut out = Vec::with_capacity(consumed);
    out.extend_from_slice(&bytes[..row.start]);
    out.extend_from_slice(translated_row.to_string().as_bytes());
    out.push(final_byte);
    Parse::Emit {
        consumed,
        bytes: out,
    }
}

fn parse_legacy_mouse(bytes: &[u8]) -> Parse {
    if bytes.len() < LEGACY_REPORT_LEN {
        return Parse::Pending;
    }

    let consumed = LEGACY_REPORT_LEN;
    let row = bytes[LEGACY_ROW_INDEX];
    let Some(translated_row) = translate_legacy_row(row) else {
        return Parse::Swallow { consumed };
    };
    let mut out = bytes[..LEGACY_REPORT_LEN].to_vec();
    out[LEGACY_ROW_INDEX] = translated_row;
    Parse::Emit {
        consumed,
        bytes: out,
    }
}

fn parse_utf8_mouse(bytes: &[u8]) -> Parse {
    let button = match parse_utf8_char(bytes, LEGACY_PARAM_START) {
        Utf8Parse::Complete(value) => value,
        Utf8Parse::Invalid { consumed } => return Parse::Pass { consumed },
        Utf8Parse::Pending => return Parse::Pending,
    };
    let column = match parse_utf8_char(bytes, button.end) {
        Utf8Parse::Complete(value) => value,
        Utf8Parse::Invalid { consumed } => return Parse::Pass { consumed },
        Utf8Parse::Pending => return Parse::Pending,
    };
    let row = match parse_utf8_char(bytes, column.end) {
        Utf8Parse::Complete(value) => value,
        Utf8Parse::Invalid { consumed } => return Parse::Pass { consumed },
        Utf8Parse::Pending => return Parse::Pending,
    };

    let consumed = row.end;
    let Some(translated_row) = translate_encoded_mouse_row(u32::from(row.value)) else {
        return Parse::Swallow { consumed };
    };
    let Some(translated_char) = char::from_u32(translated_row) else {
        return Parse::Emit {
            consumed,
            bytes: bytes[..consumed].to_vec(),
        };
    };

    let mut row_buffer = [0_u8; UTF8_MAX_BYTES];
    let mut out = Vec::with_capacity(consumed);
    out.extend_from_slice(&bytes[..row.start]);
    out.extend_from_slice(translated_char.encode_utf8(&mut row_buffer).as_bytes());
    Parse::Emit {
        consumed,
        bytes: out,
    }
}

fn parse_cpr(bytes: &[u8]) -> Parse {
    let row = match parse_number(bytes, CPR_PARAM_START) {
        NumberParse::Complete(number) => number,
        NumberParse::Invalid { consumed } => return Parse::Pass { consumed },
        NumberParse::Pending => return Parse::Pending,
    };
    if !consume_separator(bytes, row.end) {
        return parse_separator_result(bytes, row.end);
    }

    let column = match parse_number(bytes, row.end + 1) {
        NumberParse::Complete(number) => number,
        NumberParse::Invalid { consumed } => return Parse::Pass { consumed },
        NumberParse::Pending => return Parse::Pending,
    };
    if column.end == bytes.len() {
        return Parse::Pending;
    }
    if bytes[column.end] != CPR_FINAL {
        return Parse::Pass {
            consumed: column.end + 1,
        };
    }

    let consumed = column.end + 1;
    let translated_row = row.value.saturating_sub(row_offset_u32());
    let mut out = Vec::with_capacity(consumed);
    out.extend_from_slice(&bytes[..row.start]);
    out.extend_from_slice(translated_row.to_string().as_bytes());
    out.extend_from_slice(&bytes[row.end..consumed]);
    Parse::Emit {
        consumed,
        bytes: out,
    }
}

fn consume_separator(bytes: &[u8], index: usize) -> bool {
    bytes.get(index) == Some(&PARAM_SEPARATOR)
}

fn parse_separator_result(bytes: &[u8], index: usize) -> Parse {
    if index == bytes.len() {
        Parse::Pending
    } else {
        Parse::Pass {
            consumed: index + 1,
        }
    }
}

fn parse_number(bytes: &[u8], start: usize) -> NumberParse {
    if start >= bytes.len() {
        return NumberParse::Pending;
    }
    if !bytes[start].is_ascii_digit() {
        return NumberParse::Invalid {
            consumed: start + 1,
        };
    }

    let mut value = 0_u32;
    let mut index = start;
    while let Some(byte) = bytes.get(index).copied() {
        if !byte.is_ascii_digit() {
            return NumberParse::Complete(Number {
                value,
                start,
                end: index,
            });
        }
        let digit = u32::from(byte - ASCII_ZERO);
        let Some(next) = value
            .checked_mul(DECIMAL_RADIX)
            .and_then(|value| value.checked_add(digit))
        else {
            return NumberParse::Invalid {
                consumed: index + 1,
            };
        };
        value = next;
        index += 1;
    }

    NumberParse::Pending
}

fn parse_utf8_char(bytes: &[u8], start: usize) -> Utf8Parse {
    let Some(first) = bytes.get(start).copied() else {
        return Utf8Parse::Pending;
    };
    let Some(width) = utf8_char_width(first) else {
        return Utf8Parse::Invalid {
            consumed: start + 1,
        };
    };
    let end = start + width;
    if end > bytes.len() {
        return Utf8Parse::Pending;
    }

    match std::str::from_utf8(&bytes[start..end]) {
        Ok(text) => {
            let value = text
                .chars()
                .next()
                .expect("validated UTF-8 character slice is non-empty");
            Utf8Parse::Complete(Utf8Char { value, start, end })
        }
        Err(_) => Utf8Parse::Invalid { consumed: end },
    }
}

fn utf8_char_width(first: u8) -> Option<usize> {
    if first < UTF8_ONE_BYTE_LIMIT {
        Some(1)
    } else if first & UTF8_TWO_BYTE_MASK == UTF8_TWO_BYTE_TAG {
        Some(2)
    } else if first & UTF8_THREE_BYTE_MASK == UTF8_THREE_BYTE_TAG {
        Some(3)
    } else if first & UTF8_FOUR_BYTE_MASK == UTF8_FOUR_BYTE_TAG {
        Some(4)
    } else {
        None
    }
}

fn translate_mouse_row(row: u32) -> Option<u32> {
    row.checked_sub(row_offset_u32())
        .filter(|translated| *translated > 0)
}

fn translate_encoded_mouse_row(row: u32) -> Option<u32> {
    let translated = row.checked_sub(row_offset_u32())?;
    (translated > MOUSE_COORD_OFFSET).then_some(translated)
}

fn translate_legacy_row(row: u8) -> Option<u8> {
    let translated = row.checked_sub(row_offset_u8())?;
    (translated > LEGACY_COORD_OFFSET).then_some(translated)
}

fn row_offset_u32() -> u32 {
    u32::from(crate::BANNER_ROWS)
}

fn row_offset_u8() -> u8 {
    u8::try_from(crate::BANNER_ROWS).expect("banner row offset fits legacy mouse encoding")
}

#[cfg(test)]
mod tests {
    use super::InputTranslator;
    use crate::{MouseProtocolEncoding, MouseProtocolMode};

    const BUTTON_PRESS: u8 = 0;
    const BUTTON_MOTION: u8 = 32;
    const TEST_COL: u8 = 7;
    const UTF8_TEST_ROW: u32 = 100;
    const LEGACY_COORD_OFFSET: u8 = 32;
    const UTF8_COORD_OFFSET: u32 = 32;

    fn translator(mode: MouseProtocolMode, encoding: MouseProtocolEncoding) -> InputTranslator {
        let mut translator = InputTranslator::new();
        translator.set_mouse_protocol(mode, encoding);
        translator
    }

    fn sgr_translator() -> InputTranslator {
        translator(MouseProtocolMode::AnyMotion, MouseProtocolEncoding::Sgr)
    }

    fn utf8_coord(value: u32) -> String {
        char::from_u32(UTF8_COORD_OFFSET + value)
            .expect("test coordinate is a valid scalar")
            .to_string()
    }

    #[test]
    fn sgr_reports_are_decremented_without_changing_columns() {
        let mut translator = sgr_translator();
        let input = b"\x1b[<0;7;2M\x1b[<0;7;3m\x1b[<64;7;4M\x1b[<32;7;5M";

        let output = translator.push(input);

        assert_eq!(
            output,
            b"\x1b[<0;7;1M\x1b[<0;7;2m\x1b[<64;7;3M\x1b[<32;7;4M"
        );
    }

    #[test]
    fn sgr_banner_row_reports_are_swallowed_not_clamped() {
        let mut translator = sgr_translator();

        assert_eq!(translator.push(b"\x1b[<0;7;1M"), b"");
        assert_eq!(translator.push(b"\x1b[<32;7;1M"), b"");
    }

    #[test]
    fn legacy_x10_rows_are_decremented() {
        let mut translator = translator(MouseProtocolMode::Press, MouseProtocolEncoding::Default);

        let output = translator.push(&[
            b'\x1b',
            b'[',
            b'M',
            LEGACY_COORD_OFFSET + BUTTON_PRESS,
            LEGACY_COORD_OFFSET + TEST_COL,
            LEGACY_COORD_OFFSET + 2,
        ]);

        assert_eq!(
            output,
            [
                b'\x1b',
                b'[',
                b'M',
                LEGACY_COORD_OFFSET + BUTTON_PRESS,
                LEGACY_COORD_OFFSET + TEST_COL,
                LEGACY_COORD_OFFSET + 1,
            ]
        );
    }

    #[test]
    fn utf8_rows_are_decremented() {
        let mut translator =
            translator(MouseProtocolMode::ButtonMotion, MouseProtocolEncoding::Utf8);
        let input = format!(
            "\x1b[M{}{}{}",
            utf8_coord(u32::from(BUTTON_MOTION)),
            utf8_coord(u32::from(TEST_COL)),
            utf8_coord(UTF8_TEST_ROW)
        );
        let expected = format!(
            "\x1b[M{}{}{}",
            utf8_coord(u32::from(BUTTON_MOTION)),
            utf8_coord(u32::from(TEST_COL)),
            utf8_coord(UTF8_TEST_ROW - 1)
        );

        let output = translator.push(input.as_bytes());

        assert_eq!(output, expected.as_bytes());
    }

    #[test]
    fn cpr_rows_are_decremented_even_when_mouse_is_off() {
        let mut translator = InputTranslator::new();

        let output = translator.push(b"\x1b[12;7R");

        assert_eq!(output, b"\x1b[11;7R");
    }

    #[test]
    fn split_sgr_report_is_reassembled_before_translation() {
        let mut translator = sgr_translator();

        assert_eq!(translator.push(b"\x1b[<0;7"), b"");
        assert_eq!(translator.push(b";2M"), b"\x1b[<0;7;1M");
    }

    #[test]
    fn incomplete_prefix_is_held_then_completed() {
        let mut translator = sgr_translator();

        assert_eq!(translator.push(b"\x1b[<"), b"");
        assert_eq!(translator.push(b"0;7;2M"), b"\x1b[<0;7;1M");
    }

    #[test]
    fn garbage_prefix_is_flushed_unchanged() {
        let mut translator = sgr_translator();

        assert_eq!(translator.push(b"\x1b[<"), b"");
        assert_eq!(translator.push(b"x"), b"\x1b[<x");
    }

    #[test]
    fn mouse_bypass_when_off_preserves_mouse_reports_but_translates_cpr() {
        let mut translator = InputTranslator::new();
        translator.set_mouse_protocol(MouseProtocolMode::None, MouseProtocolEncoding::Sgr);

        let output = translator.push(b"\x1b[<0;7;2M\x1b[2;7R");

        assert_eq!(output, b"\x1b[<0;7;2M\x1b[1;7R");
    }
}
