//! Links on a tracked screen, through the public `TerminalTracker` boundary.
//!
//! These scenarios feed output in exact chunks, which a real session cannot
//! control, so a split is deterministic here.

// Rust guideline compliant 2026-11-26

use pohunek_terminal::{LinkSource, ScreenLink, TerminalTracker, MAX_LINK_URL_BYTES};

fn osc8(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

fn link(url: &str, source: LinkSource, row: u16, col: u16) -> ScreenLink {
    ScreenLink {
        url: url.to_owned(),
        source,
        row,
        col,
    }
}

#[test]
fn an_osc8_sequence_split_between_two_feeds_reports_like_an_unsplit_one() {
    let mut split = TerminalTracker::new(5, 40);
    split.feed(b"\x1b]8;;http://split.test/a");
    split.feed(b"\x1b\\split text\x1b]8;;\x1b\\\r\n");
    let mut whole = TerminalTracker::new(5, 40);
    whole.feed(b"\x1b]8;;http://split.test/a\x1b\\split text\x1b]8;;\x1b\\\r\n");

    let expected = vec![link("http://split.test/a", LinkSource::Osc8, 0, 0)];
    assert_eq!(whole.links(MAX_LINK_URL_BYTES), expected);
    assert_eq!(split.links(MAX_LINK_URL_BYTES), expected);
}

#[test]
fn output_fed_one_byte_at_a_time_reports_the_links_of_the_same_output_fed_whole() {
    let output = format!(
        "{}\x1b[?1049h\x1b[3;1H{}\x1b[?1049l\r\n{}\r\nhttp://text.test/t\r\n",
        osc8("http://gone.test/p", "click"),
        osc8("http://alt.test/a", "other"),
        osc8("http://after.test/b", "later"),
    );
    let mut whole = TerminalTracker::new(10, 60);
    whole.feed(output.as_bytes());
    let mut bytewise = TerminalTracker::new(10, 60);
    for byte in output.as_bytes() {
        bytewise.feed(std::slice::from_ref(byte));
    }

    assert_eq!(
        whole.links(MAX_LINK_URL_BYTES),
        vec![
            link("http://after.test/b", LinkSource::Osc8, 1, 0),
            link("http://text.test/t", LinkSource::Text, 2, 0),
        ]
    );
    assert_eq!(
        bytewise.links(MAX_LINK_URL_BYTES),
        whole.links(MAX_LINK_URL_BYTES)
    );
}

#[test]
fn a_link_longer_than_the_requested_cap_neither_hides_a_text_link_nor_takes_a_slot() {
    let long = format!("http://long.test/{}", "a".repeat(300));
    let mut tracker = TerminalTracker::new(5, 400);
    // The oversized anchor covers a URL typed in the text at the same cell.
    tracker.feed(osc8(&long, "http://short.test/s").as_bytes());
    tracker.feed(format!("\r\n{long}\r\nhttp://ok.test/k\r\n").as_bytes());

    assert_eq!(
        tracker.links(100),
        vec![
            link("http://short.test/s", LinkSource::Text, 0, 0),
            link("http://ok.test/k", LinkSource::Text, 2, 0),
        ]
    );
}

#[test]
fn every_encoding_of_an_alternate_screen_switch_drops_the_anchors() {
    let padded = format!("\x1b[?{}1049h", "0".repeat(40));
    for (name, switch) in [
        ("plain", "\x1b[?1049h"),
        ("leading zero", "\x1b[?01049h"),
        ("long padding", padded.as_str()),
        ("control inside", "\x1b[?10\x0749h"),
        ("with another mode", "\x1b[?25;1049h"),
        ("old mode", "\x1b[?47h"),
    ] {
        let mut tracker = TerminalTracker::new(5, 40);
        tracker.feed(osc8("http://gone.test/p", "click").as_bytes());
        // Plain text over the anchor's cells would otherwise match it again.
        tracker.feed(format!("{switch}\x1b[Hclick").as_bytes());

        assert_eq!(tracker.links(MAX_LINK_URL_BYTES), Vec::new(), "{name}");
    }
}

#[test]
fn a_url_of_exactly_the_cap_is_reported_with_prose_punctuation_after_it() {
    let url = format!("http://a.test/{}", "a".repeat(86));
    let mut tracker = TerminalTracker::new(5, 200);
    tracker.feed(format!("{url}.\r\n{url}a\r\n").as_bytes());

    assert_eq!(url.len(), 100);
    assert_eq!(tracker.links(100), vec![link(&url, LinkSource::Text, 0, 0)]);
}

#[test]
fn a_format_character_outside_the_bidi_blocks_rejects_an_osc8_uri() {
    let mut tracker = TerminalTracker::new(5, 40);
    tracker.feed(osc8("http://a.test/\u{0890}x", "arabic").as_bytes());

    assert_eq!(tracker.links(MAX_LINK_URL_BYTES), Vec::new());
}

#[test]
fn an_anchor_closed_across_the_whole_grid_is_not_reported() {
    let mut tracker = TerminalTracker::new(100, 200);
    tracker.feed(b"\x1b[H\x1b]8;;http://far.test/f\x1b\\x\x1b[100;200Hy\x1b]8;;\x1b\\");

    assert_eq!(tracker.links(MAX_LINK_URL_BYTES), Vec::new());
}
