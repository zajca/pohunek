//! Hyperlinks visible on a terminal screen: plain-text URLs and OSC 8 anchors.
//!
//! The links are data only; nothing here opens or resolves a URL. URL text is
//! never written to a log: [`ScreenLink`] redacts it in `Debug` output.

use std::collections::VecDeque;
use std::fmt::{Debug, Formatter};

// Rust guideline compliant 2026-11-26

/// Largest URL, in bytes, that a link may carry.
///
/// Matches the OSC payload bound in the snapshot tracker, so an OSC 8 URI can
/// never exceed it; the text matcher skips a longer candidate instead of
/// reporting a truncated, different URL.
pub const MAX_LINK_URL_BYTES: usize = 4 * 1024;

/// Largest number of links one screen reports.
///
/// Bounds both the work and the size of a result for a screen full of URLs.
pub const MAX_SCREEN_LINKS: usize = 256;

/// OSC 8 anchors retained between snapshots; the oldest is evicted first.
const MAX_OSC8_ANCHORS: usize = 256;

/// Anchor text longer than this is not retained: the anchor is dropped.
///
/// The text only proves at snapshot time that the anchor still sits where it
/// was recorded, so a long anchor is not worth the memory.
const MAX_ANCHOR_TEXT_BYTES: usize = 1024;

/// Schemes recognised in plain text, matched case-insensitively.
const TEXT_SCHEMES: [&str; 2] = ["https://", "http://"];

/// Characters that end a plain-text URL, besides whitespace and controls.
const TEXT_URL_TERMINATORS: &str = "<>\"`\\";

/// Characters trimmed from the end of a plain-text URL: sentence punctuation
/// and closing brackets that belong to the surrounding prose.
const TRAILING_PUNCTUATION: &str = ".,;:!?)]}'";

/// Where a link was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSource {
    /// A URL matched in the visible text.
    Text,
    /// An OSC 8 hyperlink anchor.
    Osc8,
}

/// One link on the visible screen.
#[derive(Clone, PartialEq, Eq)]
pub struct ScreenLink {
    /// The URL, free of control characters.
    pub url: String,
    /// How the link was found.
    pub source: LinkSource,
    /// Zero-based row of the first cell.
    pub row: u16,
    /// Zero-based column of the first cell.
    pub col: u16,
}

impl Debug for ScreenLink {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenLink")
            .field("url", &"<redacted>")
            .field("source", &self.source)
            .field("row", &self.row)
            .field("col", &self.col)
            .finish()
    }
}

/// A closed OSC 8 anchor and the screen text it covered.
#[derive(Clone, PartialEq, Eq)]
struct Anchor {
    url: String,
    start: (u16, u16),
    end: (u16, u16),
    text: String,
}

impl Debug for Anchor {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Anchor")
            .field("url", &"<redacted>")
            .field("start", &self.start)
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

/// An OSC 8 anchor that has been opened and not yet closed.
#[derive(Clone, PartialEq, Eq)]
struct OpenAnchor {
    url: String,
    start: (u16, u16),
}

impl Debug for OpenAnchor {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAnchor")
            .field("url", &"<redacted>")
            .field("start", &self.start)
            .finish()
    }
}

/// An OSC 8 sequence that changes the active hyperlink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hyperlink {
    /// `OSC 8 ; params ; URI`: later text belongs to `URI`.
    Open(String),
    /// `OSC 8 ; ;`: later text belongs to no link.
    Close,
}

impl Hyperlink {
    /// Parses the payload after `OSC 8 ;`, which is `params ; URI`.
    ///
    /// Returns `None` for a malformed sequence: no second `;`, a URI that is
    /// not UTF-8, longer than [`MAX_LINK_URL_BYTES`] or containing a control
    /// character. A URI is rejected rather than cleaned, because removing a
    /// control character can build a URL the sender never wrote.
    pub(crate) fn parse(payload: &[u8]) -> Option<Self> {
        let separator = payload.iter().position(|byte| *byte == b';')?;
        let uri = &payload[separator + 1..];
        if uri.is_empty() {
            return Some(Self::Close);
        }
        if uri.len() > MAX_LINK_URL_BYTES {
            return None;
        }
        let uri = std::str::from_utf8(uri).ok()?;
        if uri.chars().any(char::is_control) {
            return None;
        }
        Some(Self::Open(uri.to_owned()))
    }
}

/// Bounded record of the OSC 8 anchors seen on one terminal.
#[derive(Debug, Default)]
pub(crate) struct AnchorLog {
    open: Option<OpenAnchor>,
    closed: VecDeque<Anchor>,
}

impl AnchorLog {
    /// Applies one hyperlink sequence at the cursor of `screen`.
    pub(crate) fn apply(&mut self, screen: &vt100::Screen, hyperlink: Hyperlink) {
        let cursor = screen.cursor_position();
        if let Some(open) = self.open.take() {
            if let Some(anchor) = close(screen, open, cursor) {
                // The latest write to a cell wins: older anchors it overlaps
                // are stale and would otherwise reappear.
                self.closed
                    .retain(|old| !(old.start < anchor.end && anchor.start < old.end));
                if self.closed.len() == MAX_OSC8_ANCHORS {
                    self.closed.pop_front();
                }
                self.closed.push_back(anchor);
            }
        }
        if let Hyperlink::Open(url) = hyperlink {
            self.open = Some(OpenAnchor {
                url,
                start: text_start(screen, cursor),
            });
        }
    }

    /// Forgets every anchor; the recorded cells no longer exist.
    pub(crate) fn clear(&mut self) {
        self.open = None;
        self.closed.clear();
    }

    /// Returns the anchors whose recorded text is still where it was.
    fn live(&self, screen: &vt100::Screen) -> Vec<ScreenLink> {
        let open = self
            .open
            .clone()
            .and_then(|open| close(screen, open, screen.cursor_position()));
        self.closed
            .iter()
            .cloned()
            .chain(open)
            .filter(|anchor| {
                anchor_text(screen, anchor.start, anchor.end).as_deref() == Some(&anchor.text)
            })
            .map(|anchor| ScreenLink {
                url: anchor.url,
                source: LinkSource::Osc8,
                row: anchor.start.0,
                col: anchor.start.1,
            })
            .collect()
    }
}

/// The cell the next printed character lands in.
///
/// After a write to the last column the VT model keeps the cursor one past it
/// (a pending wrap), and the next character goes to column 0 of the next row.
fn text_start(screen: &vt100::Screen, cursor: (u16, u16)) -> (u16, u16) {
    let (_, cols) = screen.size();
    if cursor.1 >= cols {
        (cursor.0.saturating_add(1), 0)
    } else {
        cursor
    }
}

/// Closes `open` at `end`, or `None` when it covers no visible text.
fn close(screen: &vt100::Screen, open: OpenAnchor, end: (u16, u16)) -> Option<Anchor> {
    let text = anchor_text(screen, open.start, end)?;
    Some(Anchor {
        url: open.url,
        start: open.start,
        end,
        text,
    })
}

/// Returns the text between two cells when it is a usable anchor.
///
/// `None` for an empty or whitespace-only span, a span outside the screen,
/// or one longer than [`MAX_ANCHOR_TEXT_BYTES`].
fn anchor_text(screen: &vt100::Screen, start: (u16, u16), end: (u16, u16)) -> Option<String> {
    let (rows, cols) = screen.size();
    if start >= end || start.0 >= rows || end.0 >= rows || start.1 >= cols || end.1 > cols {
        return None;
    }
    let text = screen.contents_between(start.0, start.1, end.0, end.1);
    (!text.trim().is_empty() && text.len() <= MAX_ANCHOR_TEXT_BYTES).then_some(text)
}

/// Collects the links on `screen`: OSC 8 anchors and plain-text URLs.
///
/// Results are in reading order (row, then column), at most
/// [`MAX_SCREEN_LINKS`]. A text match at the cell an OSC 8 anchor starts at
/// is the same link and is reported once, as `osc8`.
pub(crate) fn screen_links(screen: &vt100::Screen, anchors: &AnchorLog) -> Vec<ScreenLink> {
    let mut links = anchors.live(screen);
    for link in text_links(screen) {
        if !links
            .iter()
            .any(|known| (known.row, known.col) == (link.row, link.col))
        {
            links.push(link);
        }
    }
    links.sort_by_key(|link| (link.row, link.col));
    links.truncate(MAX_SCREEN_LINKS);
    links
}

/// One cell of a logical line: its text and screen position.
struct Glyph {
    text: String,
    row: u16,
    col: u16,
}

/// Matches `http(s)` URLs in the rows of `screen`.
///
/// Rows joined by soft wrapping form one logical line, so a URL that wraps at
/// the right margin is matched whole. A hard line break ends a URL.
fn text_links(screen: &vt100::Screen) -> Vec<ScreenLink> {
    let (rows, cols) = screen.size();
    let mut links = Vec::new();
    let mut line: Vec<Glyph> = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let text = if cell.has_contents() {
                cell.contents().to_owned()
            } else {
                " ".to_owned()
            };
            line.push(Glyph { text, row, col });
        }
        if !screen.row_wrapped(row) {
            match_line(&line, &mut links);
            line.clear();
            if links.len() >= MAX_SCREEN_LINKS {
                break;
            }
        }
    }
    match_line(&line, &mut links);
    links
}

/// Appends the URLs found in one logical line.
fn match_line(line: &[Glyph], links: &mut Vec<ScreenLink>) {
    let mut text = String::new();
    let mut starts = Vec::with_capacity(line.len());
    for glyph in line {
        starts.push(text.len());
        text.push_str(&glyph.text);
    }
    let mut from = 0;
    while from < text.len() && links.len() < MAX_SCREEN_LINKS {
        let Some((begin, scheme_len)) = next_scheme(&text, from) else {
            break;
        };
        let rest = &text[begin..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c.is_control() || TEXT_URL_TERMINATORS.contains(c))
            .unwrap_or(rest.len());
        let url = trim_url(&rest[..end]);
        from = begin + end.max(scheme_len);
        if url.len() <= scheme_len || url.len() > MAX_LINK_URL_BYTES {
            continue;
        }
        if let Some(glyph) = starts
            .iter()
            .position(|start| *start == begin)
            .and_then(|index| line.get(index))
        {
            links.push(ScreenLink {
                url: url.to_owned(),
                source: LinkSource::Text,
                row: glyph.row,
                col: glyph.col,
            });
        }
    }
}

/// Finds the next scheme at or after `from` that does not continue a word.
fn next_scheme(text: &str, from: usize) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for scheme in TEXT_SCHEMES {
        let mut search = from;
        while let Some(found) = find_ignore_ascii_case(&text[search..], scheme) {
            let begin = search + found;
            let continues_word = text[..begin]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
            if !continues_word {
                if best.is_none_or(|(current, _)| begin < current) {
                    best = Some((begin, scheme.len()));
                }
                break;
            }
            search = begin + scheme.len();
        }
    }
    best
}

/// Byte offset of the first ASCII-case-insensitive `needle` in `haystack`.
fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Removes trailing prose punctuation; a `)` stays when the URL opens one.
fn trim_url(url: &str) -> &str {
    let mut url = url;
    while let Some(last) = url.chars().next_back() {
        let opener = match last {
            ')' => Some('('),
            ']' => Some('['),
            '}' => Some('{'),
            _ => None,
        };
        let keep_bracket =
            opener.is_some_and(|open| url.matches(open).count() >= url.matches(last).count());
        if !TRAILING_PUNCTUATION.contains(last) || keep_bracket {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    url
}
