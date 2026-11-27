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

/// Scheme occurrences examined on one screen before the text scan stops.
///
/// Every occurrence counts, including a bare `http://` that yields no link, so
/// a screen full of fragments costs a bounded number of candidates.
const MAX_TEXT_CANDIDATES: usize = 1024;

/// Schemes recognised in plain text, matched case-insensitively.
const TEXT_SCHEMES: [&str; 2] = ["https://", "http://"];

/// Unicode format characters (general category Cf), which `char::is_control`
/// does not cover. Bidi overrides and zero-width characters can make a
/// displayed URL differ from the one a client opens.
const FORMAT_RANGES: [(u32, u32); 21] = [
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
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

/// Whether `c` may not appear in a reported URL: a control or format character.
fn is_unsafe_char(c: char) -> bool {
    let code = u32::from(c);
    c.is_control()
        || FORMAT_RANGES
            .iter()
            .any(|(low, high)| (*low..=*high).contains(&code))
}

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
    /// not UTF-8, longer than [`MAX_LINK_URL_BYTES`] or containing a control or
    /// format character. A URI is rejected rather than cleaned, because
    /// removing such a character can build a URL the sender never wrote.
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
        if !valid_uri(uri) {
            return None;
        }
        Some(Self::Open(uri.to_owned()))
    }
}

/// Whether `uri` is a URL this crate reports: short enough and free of control
/// or format characters. A URI is rejected rather than cleaned, because
/// removing such a character can build a URL the sender never wrote.
pub(crate) fn valid_uri(uri: &str) -> bool {
    uri.len() <= MAX_LINK_URL_BYTES && !uri.chars().any(is_unsafe_char)
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
    // A span of more cells than the text limit has no usable text, so it is
    // rejected before the screen is walked to build it.
    let cells = (usize::from(end.0) - usize::from(start.0)) * usize::from(cols)
        + usize::from(end.1)
        - usize::from(start.1);
    if cells > MAX_ANCHOR_TEXT_BYTES {
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
pub(crate) fn screen_links(
    screen: &vt100::Screen,
    anchors: &AnchorLog,
    max_url_bytes: usize,
) -> Vec<ScreenLink> {
    let (rows, cols) = screen.size();
    let wrapped: Vec<bool> = (0..rows).map(|row| screen.row_wrapped(row)).collect();
    let text = text_links(
        rows,
        cols,
        &wrapped,
        &|row, col| {
            let cell = screen.cell(row, col)?;
            if cell.is_wide_continuation() {
                None
            } else if cell.has_contents() {
                Some(cell.contents().to_owned())
            } else {
                Some(" ".to_owned())
            }
        },
        max_url_bytes,
    );
    combine_links(anchors.live(screen), text, max_url_bytes)
}

/// Joins OSC 8 anchors and plain-text matches into the reported link list.
///
/// The URL cap applies before the dedupe and the count cap, so a URL that is
/// too long for the caller neither hides a text match nor takes a slot.
pub(crate) fn combine_links(
    osc8: Vec<ScreenLink>,
    text: Vec<ScreenLink>,
    max_url_bytes: usize,
) -> Vec<ScreenLink> {
    let max_url_bytes = max_url_bytes.min(MAX_LINK_URL_BYTES);
    let mut links = osc8;
    links.retain(|link| link.url.len() <= max_url_bytes);
    for link in text {
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
pub(crate) struct Glyph {
    pub text: String,
    pub row: u16,
    pub col: u16,
}

/// Matches `http(s)` URLs in the screen's logical lines.
///
/// `wrapped` says per row whether it soft-wraps into the next one, so a URL
/// that wraps at the right margin is matched whole; a hard line break ends a
/// URL. `cell` returns the text of one cell, or `None` to skip it (a wide-cell
/// continuation); an empty cell is reported as a single space.
pub(crate) fn text_links(
    rows: u16,
    cols: u16,
    wrapped: &[bool],
    cell: &dyn Fn(u16, u16) -> Option<String>,
    max_url_bytes: usize,
) -> Vec<ScreenLink> {
    let mut links = Vec::new();
    let mut candidates = MAX_TEXT_CANDIDATES;
    let mut line: Vec<Glyph> = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            if let Some(text) = cell(row, col) {
                line.push(Glyph { text, row, col });
            }
        }
        if !wrapped[usize::from(row)] {
            match_line(&line, max_url_bytes, &mut candidates, &mut links);
            line.clear();
            if links.len() >= MAX_SCREEN_LINKS || candidates == 0 {
                return links;
            }
        }
    }
    match_line(&line, max_url_bytes, &mut candidates, &mut links);
    links
}

/// Appends the URLs found in one logical line.
///
/// One pass over the line: each scheme occurrence is examined once and the
/// scan resumes after the candidate it consumed, so the cost is linear in the
/// line. Every occurrence is charged to `candidates`.
fn match_line(
    line: &[Glyph],
    max_url_bytes: usize,
    candidates: &mut usize,
    links: &mut Vec<ScreenLink>,
) {
    let mut text = String::new();
    let mut starts = Vec::with_capacity(line.len());
    for glyph in line {
        starts.push(text.len());
        text.push_str(&glyph.text);
    }
    let mut at = 0;
    while at < text.len() && links.len() < MAX_SCREEN_LINKS && *candidates > 0 {
        let Some(scheme_len) = scheme_at(&text, at) else {
            at += 1;
            continue;
        };
        *candidates -= 1;
        let continues_word = text[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        if continues_word {
            at += scheme_len;
            continue;
        }
        let rest = &text[at..];
        let end = rest
            .find(|c: char| {
                c.is_whitespace() || is_unsafe_char(c) || TEXT_URL_TERMINATORS.contains(c)
            })
            .unwrap_or(rest.len());
        let begin = at;
        at += end.max(scheme_len);
        let url = trim_url(&rest[..end]);
        // A URL longer than the cap is skipped whole, never truncated.
        if url.len() <= scheme_len || url.len() > max_url_bytes {
            continue;
        }
        if let Some(glyph) = starts
            .binary_search(&begin)
            .ok()
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

/// Length of the scheme that starts at byte `at` of `text`, if one does.
fn scheme_at(text: &str, at: usize) -> Option<usize> {
    let tail = &text.as_bytes()[at..];
    TEXT_SCHEMES
        .iter()
        .find(|scheme| {
            tail.get(..scheme.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(scheme.as_bytes()))
        })
        .map(|scheme| scheme.len())
}

/// Removes trailing prose punctuation; a closing bracket stays when the URL
/// opens as many as it closes.
///
/// The bracket counts are taken once and kept up to date as characters are
/// removed, so the cost is linear in the length of `url`.
fn trim_url(url: &str) -> &str {
    const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
    let opened = PAIRS.map(|(open, _)| url.matches(open).count());
    let mut closed = PAIRS.map(|(_, close)| url.matches(close).count());
    let mut url = url;
    while let Some(last) = url.chars().next_back() {
        let pair = PAIRS.iter().position(|(_, close)| *close == last);
        let keep_bracket = pair.is_some_and(|index| opened[index] >= closed[index]);
        if !TRAILING_PUNCTUATION.contains(last) || keep_bracket {
            break;
        }
        if let Some(index) = pair {
            closed[index] -= 1;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    url
}
