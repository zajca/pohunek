//! Decodes local shortcuts from raw terminal input.

// Rust guideline compliant 2026-08-03

/// ASCII Escape starts a seven-bit CSI sequence.
const ESC: u8 = 0x1b;
/// Seven-bit CSI sequences continue with `[` after Escape.
const CSI_OPEN: u8 = b'[';
/// Kitty keyboard reports terminate with lowercase `u`.
const CSI_U_FINAL: u8 = b'u';
/// CSI final bytes occupy the inclusive ECMA-48 `0x40..=0x7e` range.
const CSI_FINAL_START: u8 = 0x40;
const CSI_FINAL_END: u8 = 0x7e;
/// Ctrl-\ maps to ASCII File Separator in legacy terminal input.
const MENU_BYTE: u8 = 0x1c;
/// Ctrl-] maps to ASCII Group Separator in legacy terminal input.
const DETACH_BYTE: u8 = 0x1d;
/// Unicode code point reported for the backslash key by Kitty CSI-u.
const MENU_KEY: u32 = 92;
/// Unicode code point reported for the closing-bracket key by Kitty CSI-u.
const DETACH_KEY: u32 = 93;
/// Kitty adds one to the modifier bit field so zero can mean an omitted field.
const MODIFIER_OFFSET: u32 = 1;
/// Ctrl occupies bit two in the Kitty keyboard modifier bit field.
const CTRL_MASK: u32 = 0b100;
/// Kitty event type `1` is a key press and is omitted when it is the default.
const EVENT_PRESS: u32 = 1;
/// Kitty event type `2` is an auto-repeat event.
const EVENT_REPEAT: u32 = 2;
/// Decimal terminal parameters use base ten.
const DECIMAL_RADIX: u32 = 10;

/// Local action intercepted before input reaches the attached PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Shortcut {
    /// Detach the local client while leaving the session running.
    Detach,
    /// Open the attach session menu.
    Menu,
}

/// One decoded input chunk.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct DecodedInput {
    /// Bytes preceding an intercepted shortcut, or all complete non-shortcut input.
    pub(super) bytes: Vec<u8>,
    /// First local shortcut found in the stream.
    pub(super) shortcut: Option<Shortcut>,
}

/// Preserves split CSI sequences while decoding attach-local shortcuts.
#[derive(Debug, Default)]
pub(super) struct ShortcutDecoder {
    pending: Vec<u8>,
}

impl ShortcutDecoder {
    /// Creates an empty shortcut decoder.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Decodes one raw stdin chunk.
    ///
    /// Bytes after the first shortcut are intentionally dropped, matching the
    /// historical single-byte shortcut behavior. A suffix that can still be a
    /// split CSI sequence remains pending until the next push or timeout.
    pub(super) fn push(&mut self, input: &[u8], menu_available: bool) -> DecodedInput {
        let mut joined = std::mem::take(&mut self.pending);
        joined.extend_from_slice(input);

        let mut bytes = Vec::with_capacity(joined.len());
        let mut index = 0;
        while index < joined.len() {
            match parse_at(&joined[index..], menu_available) {
                Parse::Shortcut { shortcut } => {
                    return DecodedInput {
                        bytes,
                        shortcut: Some(shortcut),
                    };
                }
                Parse::Pending => {
                    self.pending.extend_from_slice(&joined[index..]);
                    break;
                }
                Parse::Pass => {
                    bytes.push(joined[index]);
                    index += 1;
                }
            }
        }

        DecodedInput {
            bytes,
            shortcut: None,
        }
    }

    /// Returns whether an ambiguous terminal sequence is awaiting more bytes.
    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Releases an incomplete sequence unchanged after its ambiguity timeout.
    pub(super) fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parse {
    Shortcut { shortcut: Shortcut },
    Pending,
    Pass,
}

fn parse_at(input: &[u8], menu_available: bool) -> Parse {
    match input[0] {
        DETACH_BYTE => Parse::Shortcut {
            shortcut: Shortcut::Detach,
        },
        MENU_BYTE if menu_available => Parse::Shortcut {
            shortcut: Shortcut::Menu,
        },
        ESC => parse_csi(input, menu_available),
        _ => Parse::Pass,
    }
}

fn parse_csi(input: &[u8], menu_available: bool) -> Parse {
    if input.len() == 1 {
        return Parse::Pending;
    }
    if input[1] != CSI_OPEN {
        return Parse::Pass;
    }
    let Some(final_offset) = input[2..]
        .iter()
        .position(|byte| (CSI_FINAL_START..=CSI_FINAL_END).contains(byte))
    else {
        return Parse::Pending;
    };
    let final_index = final_offset + 2;
    if input[final_index] != CSI_U_FINAL {
        return Parse::Pass;
    }

    parse_csi_u(&input[2..final_index], menu_available)
        .map_or(Parse::Pass, |shortcut| Parse::Shortcut { shortcut })
}

fn parse_csi_u(params: &[u8], menu_available: bool) -> Option<Shortcut> {
    let mut fields = params.split(|byte| *byte == b';');
    let key = parse_decimal(fields.next()?.split(|byte| *byte == b':').next()?)?;
    let mut modifier_fields = fields.next()?.split(|byte| *byte == b':');
    let modifiers = parse_decimal(modifier_fields.next()?)?;
    let event = match modifier_fields.next() {
        Some(bytes) => parse_decimal(bytes)?,
        None => EVENT_PRESS,
    };

    let ctrl = modifiers
        .checked_sub(MODIFIER_OFFSET)
        .is_some_and(|modifiers| modifiers & CTRL_MASK != 0);
    let actionable_event = matches!(event, EVENT_PRESS | EVENT_REPEAT);
    if !ctrl || !actionable_event {
        return None;
    }

    match key {
        DETACH_KEY => Some(Shortcut::Detach),
        MENU_KEY if menu_available => Some(Shortcut::Menu),
        _ => None,
    }
}

fn parse_decimal(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0_u32, |value, byte| {
        let digit = byte.checked_sub(b'0')?;
        (digit <= 9)
            .then_some(())
            .and_then(|()| value.checked_mul(DECIMAL_RADIX))?
            .checked_add(u32::from(digit))
    })
}

#[cfg(test)]
mod tests {
    use super::{Shortcut, ShortcutDecoder, DETACH_BYTE, MENU_BYTE};

    #[test]
    fn legacy_shortcuts_preserve_prefix_and_drop_suffix() {
        let mut decoder = ShortcutDecoder::new();
        let menu = decoder.push(b"prefix\x1csuffix", true);
        assert_eq!(menu.bytes, b"prefix");
        assert_eq!(menu.shortcut, Some(Shortcut::Menu));

        let mut decoder = ShortcutDecoder::new();
        let detach = decoder.push(b"prefix\x1dsuffix", true);
        assert_eq!(detach.bytes, b"prefix");
        assert_eq!(detach.shortcut, Some(Shortcut::Detach));
    }

    #[test]
    fn kitty_csi_u_shortcuts_are_decoded() {
        let mut decoder = ShortcutDecoder::new();
        let menu = decoder.push(b"\x1b[92;5u", true);
        assert!(menu.bytes.is_empty());
        assert_eq!(menu.shortcut, Some(Shortcut::Menu));

        let mut decoder = ShortcutDecoder::new();
        let detach = decoder.push(b"\x1b[93;5u", true);
        assert!(detach.bytes.is_empty());
        assert_eq!(detach.shortcut, Some(Shortcut::Detach));
    }

    #[test]
    fn kitty_csi_u_accepts_alternate_keys_and_repeat_events() {
        let mut parser = ShortcutDecoder::new();
        let decoded = parser.push(b"\x1b[92:124:92;6:2u", true);

        assert!(decoded.bytes.is_empty());
        assert_eq!(decoded.shortcut, Some(Shortcut::Menu));
    }

    #[test]
    fn kitty_csi_u_release_and_non_ctrl_events_are_forwarded() {
        for input in [b"\x1b[92;5:3u".as_slice(), b"\x1b[92;2u".as_slice()] {
            let mut parser = ShortcutDecoder::new();
            let decoded = parser.push(input, true);

            assert_eq!(decoded.bytes, input);
            assert_eq!(decoded.shortcut, None);
            assert!(!parser.has_pending());
        }
    }

    #[test]
    fn kitty_csi_u_shortcut_survives_every_read_boundary() {
        let input = b"\x1b[92:124:92;5:1u";
        for split in 1..input.len() {
            let mut decoder = ShortcutDecoder::new();
            let first = decoder.push(&input[..split], true);
            assert!(first.bytes.is_empty(), "split={split}");
            assert_eq!(first.shortcut, None, "split={split}");
            assert!(decoder.has_pending(), "split={split}");

            let second = decoder.push(&input[split..], true);
            assert!(second.bytes.is_empty(), "split={split}");
            assert_eq!(second.shortcut, Some(Shortcut::Menu), "split={split}");
            assert!(!decoder.has_pending(), "split={split}");
        }
    }

    #[test]
    fn menu_shortcuts_are_forwarded_when_the_menu_is_unavailable() {
        for input in [[MENU_BYTE].as_slice(), b"\x1b[92;5u".as_slice()] {
            let mut parser = ShortcutDecoder::new();
            let decoded = parser.push(input, false);

            assert_eq!(decoded.bytes, input);
            assert_eq!(decoded.shortcut, None);
            assert!(!parser.has_pending());
        }
    }

    #[test]
    fn unrelated_input_is_forwarded_byte_for_byte() {
        for input in [
            b"plain text".as_slice(),
            b"\x1b[A".as_slice(),
            b"\x1b[?1006h".as_slice(),
            b"\x1b[999999999999999999999;5u".as_slice(),
            [DETACH_BYTE.wrapping_add(1)].as_slice(),
        ] {
            let mut parser = ShortcutDecoder::new();
            let decoded = parser.push(input, true);

            assert_eq!(decoded.bytes, input);
            assert_eq!(decoded.shortcut, None);
            assert!(!parser.has_pending());
        }
    }

    #[test]
    fn incomplete_sequence_can_be_flushed_unchanged() {
        let mut parser = ShortcutDecoder::new();
        let decoded = parser.push(b"prefix\x1b[92;", true);

        assert_eq!(decoded.bytes, b"prefix");
        assert_eq!(decoded.shortcut, None);
        assert!(parser.has_pending());
        assert_eq!(parser.flush(), b"\x1b[92;");
        assert!(!parser.has_pending());
    }
}
