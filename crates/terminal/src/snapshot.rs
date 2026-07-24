//! Provider-independent terminal state tracking for durable PTY recovery.

use serde::{Deserialize, Serialize};

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
}

impl std::fmt::Debug for TerminalTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalTracker")
            .field("size", &self.parser.screen().size())
            .field("osc", &self.osc)
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
        }
    }

    /// Applies the next ordered PTY output bytes.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.osc.feed(bytes);
    }

    /// Updates the modeled terminal dimensions.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
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
}

impl OscTracker {
    fn new() -> Self {
        Self {
            state: OscState::Ground,
            command: Vec::new(),
            payload: Vec::new(),
            title: None,
            progress: None,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.advance(byte);
        }
    }

    fn advance(&mut self, byte: u8) {
        match self.state {
            OscState::Ground => {
                if byte == 0x1b {
                    self.state = OscState::Escape;
                }
            }
            OscState::Escape => {
                self.state = if byte == b']' {
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
