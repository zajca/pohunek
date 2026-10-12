// Rust guideline compliant 2026-10-13

//! Character classes shared by every pohunek crate that shows provider text.
//!
//! The worker keeps labels and provider type names that a client will display,
//! and the CLI renders them. Both must agree on which characters can hide,
//! reorder or split text, so the class is defined once here.

/// Code point ranges of the Cf, Zl and Zp general categories, Unicode 16.0.
///
/// Python 3.14's `unicodedata` reports the same version, and the Claude state
/// hook classifies with it. The ranges are the format characters and the line
/// and paragraph separators: U+2028 and U+2029 are Zl and Zp, and U+202A to
/// U+202E hold the bidirectional embeddings and overrides. A text holding any
/// of these could hide characters, reorder them or split a line.
const HIDDEN_OR_SEPARATOR_RANGES: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

/// Whether `character` is a control, a format character, or a separator.
///
/// Controls are the `Cc` category, tested with [`char::is_control`]. Format and
/// separator characters are the ranges of Unicode 16.0 listed in
/// `HIDDEN_OR_SEPARATOR_RANGES`. A label or type name that contains any such
/// character is refused, not truncated, so a client never displays text that
/// differs from what the provider sent.
#[must_use]
pub fn is_hidden_or_separator(character: char) -> bool {
    let code = u32::from(character);
    character.is_control()
        || HIDDEN_OR_SEPARATOR_RANGES
            .iter()
            .any(|&(first, last)| (first..=last).contains(&code))
}
