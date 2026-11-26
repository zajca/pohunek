//! Provider-independent terminal state tracking for durable PTY recovery.

use serde::{Deserialize, Serialize};

use crate::links::{screen_links, AnchorLog, Hyperlink, ScreenLink};
use crate::program_status::{ProbeSpan, PROGRAM_STATUS_COMMAND};

// Rust guideline compliant 2026-07-23

/// Maximum OSC value retained in memory.
///
/// Titles and progress summaries are short UI metadata. Bounding the parser
/// prevents an unterminated sequence from retaining arbitrary PTY output.
const MAX_OSC_BYTES: usize = 4 * 1024;

/// Provider-independent current terminal state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    /// Output offset represented by this state.
    pub watermark: u64,
    /// Terminal rows.
    pub rows: u16,
    /// Terminal columns.
    pub cols: u16,
    /// Cursor row.
    pub cursor_row: u16,
    /// Cursor column.
    pub cursor_col: u16,
    /// Whether the cursor is visible.
    pub cursor_visible: bool,
    /// Whether the alternate screen is active.
    pub alternate_screen: bool,
    /// Current OSC title, when observed.
    pub title: Option<String>,
    /// Current OSC progress payload, when observed.
    pub progress: Option<String>,
    /// Visible text without terminal styling.
    pub visible_text: String,
    /// ANSI stream that recreates the complete visible state.
    pub ansi: Vec<u8>,
}

/// Tracks one provider-independent VT screen and bounded OSC metadata.
pub struct TerminalTracker {
    parser: vt100::Parser,
    osc: OscTracker,
    anchors: AnchorLog,
}

impl std::fmt::Debug for TerminalTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalTracker")
            .field("size", &self.parser.screen().size())
            .field("osc", &self.osc)
            .field("anchors", &self.anchors)
            .finish()
    }
}

impl TerminalTracker {
    /// Creates an empty tracker with the initial terminal dimensions.
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            osc: OscTracker::new(),
            anchors: AnchorLog::default(),
        }
    }

    /// Applies the next ordered PTY output bytes.
    ///
    /// The VT parser is advanced up to the end of each OSC 8 sequence before
    /// the sequence is applied, so an anchor records the cursor cell the
    /// hyperlink takes effect at.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            let (used, hyperlink) = self.osc.feed_until_hyperlink(rest);
            self.parser.process(&rest[..used]);
            rest = &rest[used..];
            if let Some(hyperlink) = hyperlink {
                self.anchors.apply(self.parser.screen(), hyperlink);
            }
        }
    }

    /// Returns the links on the visible screen, in reading order.
    ///
    /// OSC 8 anchors are reported while the text they covered is still in the
    /// cells where it was printed; a scroll, clear, overwrite or buffer switch
    /// drops them, because the VT model keeps no per-cell hyperlink state.
    #[must_use]
    pub fn links(&self) -> Vec<ScreenLink> {
        screen_links(self.parser.screen(), &self.anchors)
    }

    /// Takes the OSC 7501 probes completed since the previous call.
    ///
    /// Spans are offsets in the stream fed to this tracker, in order.
    pub fn take_probes(&mut self) -> Vec<ProbeSpan> {
        std::mem::take(&mut self.osc.probes)
    }

    /// Updates the modeled terminal dimensions.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.anchors.clear();
    }

    /// Captures a complete state and ANSI repaint at `watermark`.
    #[must_use]
    pub fn snapshot(&self, watermark: u64) -> TerminalSnapshot {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let alternate_screen = screen.alternate_screen();
        let mut ansi = Vec::new();
        if alternate_screen {
            ansi.extend_from_slice(b"\x1b[?1049h");
        }
        ansi.extend_from_slice(b"\x1b[2J\x1b[H");
        ansi.extend_from_slice(&screen.input_mode_formatted());
        ansi.extend_from_slice(&screen.contents_formatted());
        ansi.extend_from_slice(&screen.cursor_state_formatted());

        TerminalSnapshot {
            watermark,
            rows,
            cols,
            cursor_row,
            cursor_col,
            cursor_visible: !screen.hide_cursor(),
            alternate_screen,
            title: self.osc.title.clone(),
            progress: self.osc.progress.clone(),
            visible_text: screen.contents(),
            ansi,
        }
    }
}

#[derive(Debug)]
struct OscTracker {
    state: OscState,
    command: Vec<u8>,
    payload: Vec<u8>,
    title: Option<String>,
    progress: Option<String>,
    /// Bytes fed so far; the index of the byte being advanced.
    consumed: u64,
    /// Index of the latest `ESC` seen in the ground state.
    escape_at: u64,
    /// Index of the `ESC` that introduced the current OSC.
    osc_start: u64,
    probes: Vec<ProbeSpan>,
    /// OSC 8 sequence completed by the byte just advanced, not yet taken.
    hyperlink: Option<Hyperlink>,
}

impl OscTracker {
    fn new() -> Self {
        Self {
            state: OscState::Ground,
            command: Vec::new(),
            payload: Vec::new(),
            title: None,
            progress: None,
            consumed: 0,
            escape_at: 0,
            osc_start: 0,
            probes: Vec::new(),
            hyperlink: None,
        }
    }

    /// Advances over `bytes` up to and including the byte that completes an
    /// OSC 8 sequence, or over all of them when none completes.
    ///
    /// Returns the number of bytes consumed and the completed sequence.
    fn feed_until_hyperlink(&mut self, bytes: &[u8]) -> (usize, Option<Hyperlink>) {
        for (index, &byte) in bytes.iter().enumerate() {
            self.advance(byte);
            self.consumed += 1;
            if let Some(hyperlink) = self.hyperlink.take() {
                return (index + 1, Some(hyperlink));
            }
        }
        (bytes.len(), None)
    }

    fn advance(&mut self, byte: u8) {
        match self.state {
            OscState::Ground => {
                if byte == 0x1b {
                    self.escape_at = self.consumed;
                    self.state = OscState::Escape;
                }
            }
            OscState::Escape => {
                self.state = if byte == b']' {
                    self.osc_start = self.escape_at;
                    self.command.clear();
                    self.payload.clear();
                    OscState::Command
                } else {
                    OscState::Ground
                };
            }
            OscState::Command => match byte {
                b';' => self.state = OscState::Payload,
                0x07 => self.finish(),
                _ if self.command.len() < MAX_OSC_BYTES => self.command.push(byte),
                _ => self.state = OscState::Discard,
            },
            OscState::Payload => match byte {
                0x07 => self.finish(),
                0x1b => self.state = OscState::PayloadEscape,
                _ if self.payload.len() < MAX_OSC_BYTES => self.payload.push(byte),
                _ => self.state = OscState::Discard,
            },
            OscState::PayloadEscape => {
                if byte == b'\\' {
                    self.finish();
                } else {
                    if self.payload.len() < MAX_OSC_BYTES {
                        self.payload.push(0x1b);
                        self.payload.push(byte);
                    } else {
                        self.state = OscState::Discard;
                        return;
                    }
                    self.state = OscState::Payload;
                }
            }
            OscState::Discard => {
                if byte == 0x07 {
                    self.state = OscState::Ground;
                } else if byte == 0x1b {
                    self.state = OscState::DiscardEscape;
                }
            }
            OscState::DiscardEscape => {
                self.state = if byte == b'\\' {
                    OscState::Ground
                } else {
                    OscState::Discard
                };
            }
        }
    }

    fn finish(&mut self) {
        let command = std::str::from_utf8(&self.command).ok();
        let payload = String::from_utf8_lossy(&self.payload).into_owned();
        match command {
            Some("0" | "2") => self.title = Some(payload),
            Some("9") => self.progress = Some(payload),
            Some("8") => self.hyperlink = Hyperlink::parse(&self.payload),
            Some(number)
                if number.as_bytes() == PROGRAM_STATUS_COMMAND
                    && self.payload.first() == Some(&b'?') =>
            {
                self.probes.push(ProbeSpan {
                    start: self.osc_start,
                    end: self.consumed + 1,
                });
            }
            _ => {}
        }
        self.state = OscState::Ground;
        self.command.clear();
        self.payload.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OscState {
    Ground,
    Escape,
    Command,
    Payload,
    PayloadEscape,
    Discard,
    DiscardEscape,
}

#[cfg(test)]
mod tests {
    use super::TerminalTracker;

    #[test]
    fn fragmented_vt_and_osc_sequences_produce_complete_snapshot() {
        let mut tracker = TerminalTracker::new(3, 20);
        tracker.feed(b"\x1b]2;build");
        tracker.feed(b"ing\x07\x1b[2;4Hok");

        let snapshot = tracker.snapshot(42);

        assert_eq!(snapshot.watermark, 42);
        assert_eq!(snapshot.title.as_deref(), Some("building"));
        assert!(snapshot.visible_text.contains("ok"));
        assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (1, 5));
        assert!(snapshot.ansi.starts_with(b"\x1b[2J\x1b[H"));
    }

    #[test]
    fn oversized_osc_is_discarded_and_parser_recovers() {
        let mut tracker = TerminalTracker::new(2, 10);
        let oversized = vec![b'x'; super::MAX_OSC_BYTES + 1];
        tracker.feed(b"\x1b]2;");
        tracker.feed(&oversized);
        tracker.feed(b"\x07\x1b]2;safe\x07");

        assert_eq!(tracker.snapshot(0).title.as_deref(), Some("safe"));
    }
}
