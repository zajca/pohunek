//! Decodes local shortcuts from raw terminal input.

// Rust guideline compliant 2026-08-03

/// ASCII Escape starts a seven-bit CSI sequence.
const ESC: u8 = 0x1b;
/// Seven-bit CSI sequences continue with `[` after Escape.
const CSI_OPEN: u8 = b'[';
/// Kitty keyboard reports terminate with lowercase `u`.
const CSI_U_FINAL: u8 = b'u';
/// SS3 reports continue with `O` after Escape.
const SS3_OPEN: u8 = b'O';
/// Up-arrow final byte shared by CSI and SS3 reports.
const ARROW_UP_FINAL: u8 = b'A';
/// Down-arrow final byte shared by CSI and SS3 reports.
const ARROW_DOWN_FINAL: u8 = b'B';
/// SGR mouse reports open with `<` in their parameter bytes (`ESC [ < ...`).
const SGR_MOUSE_MARKER: u8 = b'<';
/// Legacy mouse head `ESC [ M ...` and SGR mouse final byte.
const MOUSE_FINAL: u8 = b'M';
/// Bracketed-paste open parameters (`ESC [ 200 ~`).
const PASTE_OPEN_PARAMS: &[u8] = b"200";
/// Bracketed-paste close parameters (`ESC [ 201 ~`).
const PASTE_CLOSE_PARAMS: &[u8] = b"201";
/// CSI `~` final shared by bracketed-paste markers.
const PASTE_FINAL: u8 = b'~';
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
/// Legacy up-arrow canonical CSI sequence emitted to the open menu.
const LEGACY_UP: &[u8] = b"\x1b[A";
/// Legacy down-arrow canonical CSI sequence emitted to the open menu.
const LEGACY_DOWN: &[u8] = b"\x1b[B";
/// Kitty event type `3` is a key release.
const EVENT_RELEASE: u32 = 3;
/// Largest incomplete terminal sequence retained between stdin reads.
///
/// Kitty keyboard reports are normally short. Sixty-four bytes leaves room for
/// alternate key codes while bounding state independently of the outer timer.
const MAX_PENDING_BYTES: usize = 64;
/// Keys remembered for release suppression after rollover. The kitty
/// protocol reports releases for every key held at once, and a person cannot
/// hold more keys than this at the same time.
const MAX_TRACKED_RELEASES: usize = 8;
/// CSI parameter bytes start at the ECMA-48 minimum (`0x20`).
const CSI_PARAM_START: u8 = 0x20;
/// CSI parameter bytes end at the ECMA-48 maximum (`0x3f`).
const CSI_PARAM_END: u8 = 0x3f;
/// SS3 reports are `ESC O <final>`.
const SS3_REPORT_LEN: usize = 3;
/// Bytes of the `ESC [` or `ESC O` prefix of a terminal sequence.
const CSI_PREFIX_LEN: usize = 2;

/// Attach-menu state relevant to shortcut decoding and key normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuInputMode {
    /// This attach cannot display an interactive menu.
    Unavailable,
    /// The menu is available but currently closed.
    Closed,
    /// The menu is currently consuming keyboard input.
    Open,
}

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
///
/// While the menu is open it also remembers every key whose press the menu
/// consumed, so a release report arriving after the menu closed (for example
/// the Escape release that follows the press which closed it, or the release
/// of a key still held when another key closed the menu) is swallowed once
/// instead of being forwarded to the attached program.
#[derive(Debug, Default)]
pub(super) struct ShortcutDecoder {
    pending: Vec<u8>,
    /// Keys the open menu consumed whose release has not arrived yet.
    pending_releases: Vec<PendingRelease>,
}

/// Key whose release report the shortcut decoder must swallow once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingRelease {
    /// A CSI-u key code (for example `27` for Escape).
    Unicode(u32),
    /// An arrow key, by final byte (`A` for up, `B` for down).
    Arrow(u8),
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
    pub(super) fn push(&mut self, input: &[u8], menu_mode: MenuInputMode) -> DecodedInput {
        let mut joined = std::mem::take(&mut self.pending);
        joined.extend_from_slice(input);

        let mut bytes = Vec::with_capacity(joined.len());
        let mut index = 0;
        while index < joined.len() {
            match parse_at(&joined[index..], menu_mode, self) {
                Parse::Shortcut { shortcut } => {
                    return DecodedInput {
                        bytes,
                        shortcut: Some(shortcut),
                    };
                }
                Parse::Pending => {
                    let pending = &joined[index..];
                    if pending.len() <= MAX_PENDING_BYTES {
                        self.pending.extend_from_slice(pending);
                    } else {
                        bytes.extend_from_slice(pending);
                    }
                    break;
                }
                Parse::Normalized { byte, consumed } => {
                    bytes.push(byte);
                    index += consumed;
                }
                Parse::Emit {
                    bytes: emitted,
                    consumed,
                } => {
                    bytes.extend_from_slice(emitted);
                    index += consumed;
                }
                Parse::Consumed { consumed } => {
                    index += consumed;
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

    /// Records a press the open menu consumed.
    fn remember_press(&mut self, release: PendingRelease) {
        if self.pending_releases.contains(&release) {
            return;
        }
        if self.pending_releases.len() == MAX_TRACKED_RELEASES {
            self.pending_releases.remove(0);
        }
        self.pending_releases.push(release);
    }

    /// Reports whether a release report names the remembered key, clearing it.
    fn take_matching_release(&mut self, release: PendingRelease) -> bool {
        let Some(position) = self.pending_releases.iter().position(|key| *key == release) else {
            return false;
        };
        self.pending_releases.remove(position);
        true
    }

    /// Forgets one remembered key, e.g. when its press or repeat is forwarded
    /// to the program in Closed mode.
    fn forget_release(&mut self, release: PendingRelease) {
        if let Some(position) = self.pending_releases.iter().position(|key| *key == release) {
            self.pending_releases.remove(position);
        }
    }

    /// Forgets every remembered release; called when the menu opens.
    ///
    /// The next press starts a fresh set, so a stale release from a terminal
    /// that never reports event types cannot swallow an unrelated release of
    /// a later physical press.
    pub(super) fn clear_pending_releases(&mut self) {
        self.pending_releases.clear();
    }

    /// Releases an incomplete sequence unchanged after its ambiguity timeout.
    //
    // `flush` returns the bytes to `route_stdin_input` first, and the caller
    // routes them before the next `push`, so a flushed lone Escape never
    // merges with a following read.
    pub(super) fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parse {
    Shortcut {
        shortcut: Shortcut,
    },
    Normalized {
        byte: u8,
        consumed: usize,
    },
    /// Canonical replacement bytes for the open menu, e.g. `ESC [ A`.
    Emit {
        bytes: &'static [u8],
        consumed: usize,
    },
    Consumed {
        consumed: usize,
    },
    Pending,
    Pass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CsiUKey {
    key: u32,
    modifiers: u32,
    event: u32,
    event_field_present: bool,
}

fn parse_at(input: &[u8], menu_mode: MenuInputMode, decoder: &mut ShortcutDecoder) -> Parse {
    match input[0] {
        DETACH_BYTE => Parse::Shortcut {
            shortcut: Shortcut::Detach,
        },
        MENU_BYTE if menu_mode == MenuInputMode::Closed => Parse::Shortcut {
            shortcut: Shortcut::Menu,
        },
        ESC => parse_csi(input, menu_mode, decoder),
        _ => Parse::Pass,
    }
}

fn parse_csi(input: &[u8], menu_mode: MenuInputMode, decoder: &mut ShortcutDecoder) -> Parse {
    if input.len() == 1 {
        return Parse::Pending;
    }
    if input[1] != CSI_OPEN {
        if input[1] == SS3_OPEN {
            return parse_ss3(input, menu_mode);
        }
        // Any other `ESC x` is a lone Escape followed by a new key, or a Meta
        // prefix: the Escape is handled on its own and the next byte is
        // re-parsed, so a CSI typed right after a lone Escape cannot swallow
        // it, and a shortcut byte right after Escape is not left pending.
        return Parse::Pass;
    }
    // The CSI body runs over ECMA-48 parameter bytes only. A byte outside
    // the parameter range before any final byte (CR, ESC, DEL, ...) aborts
    // the sequence: the Escape is passed and the rest is re-parsed, so a
    // stray byte can no longer swallow following keys.
    if let Some(abort) = input[CSI_PREFIX_LEN..]
        .iter()
        .position(|byte| !(CSI_PARAM_START..=CSI_PARAM_END).contains(byte))
    {
        let candidate = abort + CSI_PREFIX_LEN;
        if (CSI_FINAL_START..=CSI_FINAL_END).contains(&input[candidate]) {
            return parse_csi_body(input, candidate, menu_mode, decoder);
        }
        return Parse::Pass;
    }
    Parse::Pending
}

fn parse_csi_body(
    input: &[u8],
    final_index: usize,
    menu_mode: MenuInputMode,
    decoder: &mut ShortcutDecoder,
) -> Parse {
    let consumed = final_index + 1;
    let final_byte = input[final_index];
    let params = &input[CSI_PREFIX_LEN..final_index];

    // Bracketed-paste control sequences belong to the menu decoder, which
    // skips pasted text wholesale; a mouse report is consumed there too. A
    // bare `ESC [ M` is a legacy mouse head, not an arrow or paste report.
    if is_paste_control(params, final_byte)
        || params.first() == Some(&SGR_MOUSE_MARKER)
        || (final_byte == MOUSE_FINAL && final_index == CSI_PREFIX_LEN)
    {
        return Parse::Pass;
    }

    if final_byte == CSI_U_FINAL {
        return parse_csi_u_sequence(params, consumed, menu_mode, decoder);
    }

    // Arrow finals accept every CSI form: legacy `ESC [ A`, and the kitty
    // event-typed `ESC [ 1 ; <mods> [ : <event> ] A` (foot always sends the
    // `:1` subparameter). Release reports are swallowed, presses and repeats
    // become the canonical legacy report.
    if matches!(final_byte, ARROW_UP_FINAL | ARROW_DOWN_FINAL) {
        return parse_arrow_sequence(params, final_byte, consumed, menu_mode, decoder);
    }

    // Every other complete CSI is consumed whole by the open menu, so none of
    // its bytes (focus reports, colour-scheme reports, releases of other keys)
    // can act as a hotkey or enter the rename buffer. Closed input forwards.
    if menu_mode == MenuInputMode::Open {
        return Parse::Consumed { consumed };
    }
    Parse::Pass
}

fn is_paste_control(params: &[u8], final_byte: u8) -> bool {
    (params == PASTE_OPEN_PARAMS || params == PASTE_CLOSE_PARAMS) && final_byte == PASTE_FINAL
}

fn parse_arrow_sequence(
    params: &[u8],
    final_byte: u8,
    consumed: usize,
    menu_mode: MenuInputMode,
    decoder: &mut ShortcutDecoder,
) -> Parse {
    let event = arrow_event(params);
    if menu_mode != MenuInputMode::Open {
        if event == EVENT_RELEASE
            && decoder.take_matching_release(PendingRelease::Arrow(final_byte))
        {
            return Parse::Consumed { consumed };
        }
        // A press or repeat the program receives clears the tracked key: its
        // release, whenever it arrives, must reach the program.
        if event != EVENT_RELEASE {
            decoder.forget_release(PendingRelease::Arrow(final_byte));
        }
        return Parse::Pass;
    }
    if event == EVENT_RELEASE {
        let _ = decoder.take_matching_release(PendingRelease::Arrow(final_byte));
        return Parse::Consumed { consumed };
    }
    // Only a press with an explicit event field can be followed by a release
    // to suppress; a modified arrow (`ESC[1;5A`) carries no event field and
    // must not prime the slot.
    if arrow_event_present(params) {
        decoder.remember_press(PendingRelease::Arrow(final_byte));
    }
    let bytes = if final_byte == ARROW_UP_FINAL {
        LEGACY_UP
    } else {
        LEGACY_DOWN
    };
    if params.is_empty() {
        Parse::Pass
    } else {
        Parse::Emit { bytes, consumed }
    }
}

/// Kitty event type of an arrow parameter list (`ESC[1;<mods>[:<event>]A`).
pub(super) fn arrow_event(params: &[u8]) -> u32 {
    let mut fields = params.split(|byte| *byte == b';');
    // The first field carries the key number (`1`) and any subparameters.
    let _ = fields.next();
    match fields.next() {
        Some(field) => {
            let mut parts = field.split(|byte| *byte == b':');
            let _ = parts.next();
            match parts.next() {
                Some(bytes) => parse_decimal(bytes).unwrap_or(EVENT_PRESS),
                None => EVENT_PRESS,
            }
        }
        None => EVENT_PRESS,
    }
}

/// Whether an arrow parameter list carries an explicit `:event` subfield.
fn arrow_event_present(params: &[u8]) -> bool {
    let mut fields = params.split(|byte| *byte == b';');
    let _ = fields.next();
    match fields.next() {
        Some(field) => field.contains(&b':'),
        None => false,
    }
}

fn parse_ss3(input: &[u8], menu_mode: MenuInputMode) -> Parse {
    if input.len() < SS3_REPORT_LEN {
        return Parse::Pending;
    }
    if !(CSI_FINAL_START..=CSI_FINAL_END).contains(&input[CSI_PREFIX_LEN]) {
        // An SS3 final byte uses the CSI final-byte range; anything else is
        // not an SS3 final: the Escape is a lone Escape and the rest is
        // re-parsed, so a flush after the timeout cannot swallow Escape or
        // the detach byte.
        return Parse::Pass;
    }
    if menu_mode != MenuInputMode::Open {
        return Parse::Pass;
    }
    // SS3 reports carry no event field, so nothing here can be followed by a
    // release and no press needs remembering for release suppression.
    match input[CSI_PREFIX_LEN] {
        ARROW_UP_FINAL => Parse::Emit {
            bytes: LEGACY_UP,
            consumed: SS3_REPORT_LEN,
        },
        ARROW_DOWN_FINAL => Parse::Emit {
            bytes: LEGACY_DOWN,
            consumed: SS3_REPORT_LEN,
        },
        _ => Parse::Consumed {
            consumed: SS3_REPORT_LEN,
        },
    }
}

fn parse_csi_u_sequence(
    params: &[u8],
    consumed: usize,
    menu_mode: MenuInputMode,
    decoder: &mut ShortcutDecoder,
) -> Parse {
    let Some(key) = parse_csi_u(params) else {
        return Parse::Pass;
    };

    if key.is_actionable() && key.has_ctrl() {
        match key.key {
            DETACH_KEY => {
                return Parse::Shortcut {
                    shortcut: Shortcut::Detach,
                };
            }
            MENU_KEY if menu_mode == MenuInputMode::Closed => {
                return Parse::Shortcut {
                    shortcut: Shortcut::Menu,
                };
            }
            _ => {}
        }
    }

    if menu_mode != MenuInputMode::Open {
        if key.event == EVENT_RELEASE
            && decoder.take_matching_release(PendingRelease::Unicode(key.key))
        {
            return Parse::Consumed { consumed };
        }
        // A press or repeat the program receives clears the tracked key: its
        // release, whenever it arrives, must reach the program.
        if key.is_actionable() {
            decoder.forget_release(PendingRelease::Unicode(key.key));
        }
        return Parse::Pass;
    }
    if key.event == EVENT_RELEASE {
        let _ = decoder.take_matching_release(PendingRelease::Unicode(key.key));
        return Parse::Consumed { consumed };
    }
    if key.is_actionable() {
        // Only an event-typed press can be followed by a release to suppress;
        // a bare CSI-u press carries no release and must not prime the slot.
        if key.event_is_reported() {
            decoder.remember_press(PendingRelease::Unicode(key.key));
        }
        if key.modifiers == 0 {
            if let Ok(byte) = u8::try_from(key.key) {
                return Parse::Normalized { byte, consumed };
            }
        }
    }

    // A complete enhanced-keyboard report belongs to the open menu even when
    // it has no canonical byte representation, for example a release event.
    Parse::Consumed { consumed }
}

fn parse_csi_u(params: &[u8]) -> Option<CsiUKey> {
    let mut fields = params.split(|byte| *byte == b';');
    let key = parse_decimal(fields.next()?.split(|byte| *byte == b':').next()?)?;
    let (modifiers, event, event_field_present) = match fields.next() {
        Some(field) => {
            let mut modifier_fields = field.split(|byte| *byte == b':');
            let encoded_modifiers = parse_decimal(modifier_fields.next()?)?;
            let modifiers = encoded_modifiers.checked_sub(MODIFIER_OFFSET)?;
            match modifier_fields.next() {
                Some(bytes) => (modifiers, parse_decimal(bytes)?, true),
                None => (modifiers, EVENT_PRESS, false),
            }
        }
        None => (0, EVENT_PRESS, false),
    };

    Some(CsiUKey {
        key,
        modifiers,
        event,
        event_field_present,
    })
}

impl CsiUKey {
    fn has_ctrl(self) -> bool {
        self.modifiers & CTRL_MASK != 0
    }

    fn is_actionable(self) -> bool {
        matches!(self.event, EVENT_PRESS | EVENT_REPEAT)
    }

    /// Whether the report carried an explicit event-type field. The pressed
    /// event omits it, so only a press or repeat with an explicit field can
    /// be followed by a matching release.
    fn event_is_reported(self) -> bool {
        self.event_field_present
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
    use super::{
        MenuInputMode, Shortcut, ShortcutDecoder, DETACH_BYTE, MAX_PENDING_BYTES, MENU_BYTE,
    };

    #[test]
    fn legacy_shortcuts_preserve_prefix_and_drop_suffix() {
        let mut decoder = ShortcutDecoder::new();
        let menu = decoder.push(b"prefix\x1csuffix", MenuInputMode::Closed);
        assert_eq!(menu.bytes, b"prefix");
        assert_eq!(menu.shortcut, Some(Shortcut::Menu));

        let mut decoder = ShortcutDecoder::new();
        let detach = decoder.push(b"prefix\x1dsuffix", MenuInputMode::Closed);
        assert_eq!(detach.bytes, b"prefix");
        assert_eq!(detach.shortcut, Some(Shortcut::Detach));
    }

    #[test]
    fn kitty_csi_u_shortcuts_are_decoded() {
        let mut decoder = ShortcutDecoder::new();
        let menu = decoder.push(b"\x1b[92;5u", MenuInputMode::Closed);
        assert!(menu.bytes.is_empty());
        assert_eq!(menu.shortcut, Some(Shortcut::Menu));

        let mut decoder = ShortcutDecoder::new();
        let detach = decoder.push(b"\x1b[93;5u", MenuInputMode::Closed);
        assert!(detach.bytes.is_empty());
        assert_eq!(detach.shortcut, Some(Shortcut::Detach));
    }

    #[test]
    fn kitty_csi_u_accepts_alternate_keys_and_repeat_events() {
        let mut parser = ShortcutDecoder::new();
        let decoded = parser.push(b"\x1b[92:124:92;6:2u", MenuInputMode::Closed);

        assert!(decoded.bytes.is_empty());
        assert_eq!(decoded.shortcut, Some(Shortcut::Menu));
    }

    #[test]
    fn kitty_csi_u_release_and_non_ctrl_events_are_forwarded() {
        for input in [b"\x1b[92;5:3u".as_slice(), b"\x1b[92;2u".as_slice()] {
            let mut parser = ShortcutDecoder::new();
            let decoded = parser.push(input, MenuInputMode::Closed);

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
            let first = decoder.push(&input[..split], MenuInputMode::Closed);
            assert!(first.bytes.is_empty(), "split={split}");
            assert_eq!(first.shortcut, None, "split={split}");
            assert!(decoder.has_pending(), "split={split}");

            let second = decoder.push(&input[split..], MenuInputMode::Closed);
            assert!(second.bytes.is_empty(), "split={split}");
            assert_eq!(second.shortcut, Some(Shortcut::Menu), "split={split}");
            assert!(!decoder.has_pending(), "split={split}");
        }
    }

    #[test]
    fn menu_shortcuts_are_forwarded_when_the_menu_is_unavailable() {
        for input in [[MENU_BYTE].as_slice(), b"\x1b[92;5u".as_slice()] {
            let mut parser = ShortcutDecoder::new();
            let decoded = parser.push(input, MenuInputMode::Unavailable);

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
            let decoded = parser.push(input, MenuInputMode::Closed);

            assert_eq!(decoded.bytes, input);
            assert_eq!(decoded.shortcut, None);
            assert!(!parser.has_pending());
        }
    }

    #[test]
    fn incomplete_sequence_can_be_flushed_unchanged() {
        let mut parser = ShortcutDecoder::new();
        let decoded = parser.push(b"prefix\x1b[92;", MenuInputMode::Closed);

        assert_eq!(decoded.bytes, b"prefix");
        assert_eq!(decoded.shortcut, None);
        assert!(parser.has_pending());
        assert_eq!(parser.flush(), b"\x1b[92;");
        assert!(!parser.has_pending());
    }

    #[test]
    fn open_menu_normalizes_csi_u_keyboard_bytes() {
        for (input, expected) in [
            (b"\x1b[27u".as_slice(), 0x1b),
            (b"\x1b[13u".as_slice(), b'\r'),
            (b"\x1b[127u".as_slice(), 0x7f),
            (b"\x1b[114u".as_slice(), b'r'),
        ] {
            let mut decoder = ShortcutDecoder::new();
            let result = decoder.push(input, MenuInputMode::Open);

            assert_eq!(result.bytes, [expected]);
            assert_eq!(result.shortcut, None);
            assert!(!decoder.has_pending());
        }
    }

    #[test]
    fn open_menu_consumes_non_actionable_csi_u_reports() {
        let mut decoder = ShortcutDecoder::new();
        let result = decoder.push(b"\x1b[27;1:3u", MenuInputMode::Open);

        assert!(result.bytes.is_empty());
        assert_eq!(result.shortcut, None);
        assert!(!decoder.has_pending());
    }

    #[test]
    fn oversized_pending_sequence_is_released_unchanged() {
        let mut input = b"\x1b[".to_vec();
        input.resize(MAX_PENDING_BYTES + 1, b'1');
        let mut decoder = ShortcutDecoder::new();

        let result = decoder.push(&input, MenuInputMode::Closed);

        assert_eq!(result.bytes, input);
        assert_eq!(result.shortcut, None);
        assert!(!decoder.has_pending());
    }
}
