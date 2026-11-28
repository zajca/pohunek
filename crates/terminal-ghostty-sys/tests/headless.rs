//! Hermetic tests of the safe wrapper against the prebuilt libghostty-vt
//! library named by `LIBGHOSTTY_VT_DIR` (issue #2538, scenario S6 at the sys
//! boundary). The whole file is gated on the `libghostty` feature: without it
//! the crate links nothing, so these tests do not exist either. Nothing is
//! fetched at build or test time; the library comes from
//! `scripts/build-libghostty-vt`.

#![cfg(feature = "libghostty")]

use base64::Engine as _;
use pohunek_terminal_ghostty_sys::{HeadlessConfig, HeadlessTerminal};

/// S6: an OSC 52 clipboard flood is answered with a bounded record — one
/// counter plus the decoded size of the last write, never one entry per
/// write — and every write is denied. A `?` read request records nothing.
#[test]
fn an_osc52_flood_is_recorded_as_a_bounded_counter() {
    const WRITES: usize = 4096;
    const PAYLOAD_BYTES: usize = 1024;
    let payload = base64::engine::general_purpose::STANDARD.encode([b'x'; PAYLOAD_BYTES]);
    let mut terminal =
        HeadlessTerminal::new(&HeadlessConfig::default(), 24, 80).expect("the terminal inits");
    assert_eq!(terminal.clipboard_write_count(), 0);
    assert_eq!(terminal.last_clipboard_write_bytes(), 0);
    for _ in 0..WRITES {
        let sequence = format!("\x1b]52;c;{payload}\x07");
        terminal.feed(sequence.as_bytes());
    }
    terminal.feed(b"\x1b]52;c;?\x07");
    assert_eq!(
        terminal.clipboard_write_count(),
        WRITES,
        "every OSC 52 write is counted"
    );
    assert_eq!(
        terminal.last_clipboard_write_bytes(),
        PAYLOAD_BYTES,
        "the last write's decoded size is recorded"
    );
}

/// The two text views of one terminal: `visible_text` covers the active
/// screen rows only (the `session.screen` definition), `format_plain` is the
/// history view and still contains the scrolled-off rows.
#[test]
fn visible_text_is_the_active_rows_and_format_plain_the_history() {
    let mut terminal =
        HeadlessTerminal::new(&HeadlessConfig::default(), 4, 20).expect("the terminal inits");
    for index in 0..50 {
        let line = format!("line-{index:02}\r\n");
        terminal.feed(line.as_bytes());
    }
    let visible = terminal.visible_text().expect("the active text reads");
    assert!(
        visible.contains("line-49"),
        "the last line is on the active screen: {visible:?}"
    );
    assert!(
        !visible.contains("line-00"),
        "scrolled-off rows are not in visible_text: {visible:?}"
    );
    // One cell's text is far below 32 bytes; 4 bytes per UTF-8 code point is
    // the safety margin on top.
    assert!(
        visible.len() <= 4 * (20 * 128 + 1),
        "visible_text of {} bytes is bounded by the 4x20 active screen",
        visible.len()
    );
    let history = String::from_utf8_lossy(&terminal.format_plain().expect("the history formats"))
        .into_owned();
    assert!(
        history.contains("line-00"),
        "format_plain keeps the history view"
    );
}

/// S6: malformed escape sequences and a kitty APC are parsed without
/// aborting, and the terminal keeps reporting a usable screen afterwards.
#[test]
fn malformed_sequences_do_not_abort_the_terminal() {
    let mut terminal =
        HeadlessTerminal::new(&HeadlessConfig::default(), 24, 80).expect("the terminal inits");
    terminal.feed(b"\x1b[12;34\x1b]8;;https://example.com\x07link\x1b[?8m\x1b_Ga=1;i=1\x1b\\");
    terminal.feed(b"still alive\r\n");
    // An unterminated OSC swallows what follows it, so it goes last.
    terminal.feed(b"\x1b]52;c;not base64!\x07\x1b[99999999999999999999H\x1b]0;unterminated");
    assert_eq!(terminal.size().expect("the size reads"), (24, 80));
    let text = terminal.format_plain().expect("the text formats");
    let text = String::from_utf8_lossy(&text);
    assert!(
        text.contains("still alive"),
        "the terminal still parses plain output: {text:?}"
    );
}
