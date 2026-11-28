//! The libghostty-vt backend of [`TerminalTracker`](crate::TerminalTracker).
//!
//! All unsafe FFI lives in `pohunek-terminal-ghostty-sys`; this module is safe
//! code that maps the sys crate onto the screen seam. The repaint body is
//! `format_vt(unwrap = true, trim = false)` with the attach extras (everything
//! but the palette), preceded by the same prologue the vt100 backend emits:
//! the screen switch (`?1049h` on the alternate screen) and `CSI 2J CSI H`,
//! so an attach clears the client screen on either model (issue #2538 M13/M14).

// Rust guideline compliant 2026-11-27

use pohunek_terminal_ghostty_sys::{HeadlessConfig, HeadlessTerminal, FORMATTER_EXTRAS_ATTACH};

use crate::links::{combine_links, text_links, valid_uri, LinkSource, ScreenLink};
use crate::snapshot::ScreenState;

/// Repaint option: join soft-wrapped rows into one line.
const REPAINT_UNWRAP: bool = true;
/// Repaint option: keep trailing spaces of each line.
const REPAINT_TRIM: bool = false;
/// Screen switch to the alternate screen, emitted first when it is active.
const REPAINT_ALT_SCREEN: &[u8] = b"\x1b[?1049h";
/// Clear the screen and home the cursor, after the screen switch; the vt100
/// backend emits the same clear, so the attach path wipes stale content on
/// either model.
const REPAINT_CLEAR: &[u8] = b"\x1b[2J\x1b[H";

/// A libghostty-vt screen: the `Ghostty` variant of the tracker's backend.
pub(crate) struct GhosttyScreen {
    terminal: HeadlessTerminal,
}

impl std::fmt::Debug for GhosttyScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhosttyScreen")
            .field("size", &self.size())
            .finish_non_exhaustive()
    }
}

impl GhosttyScreen {
    /// Creates a headless terminal of `rows` x `cols` cells with the pinned
    /// headless settings.
    ///
    /// # Panics
    ///
    /// Panics when the linked library rejects the setup, which only a
    /// mis-built library or an out-of-memory condition can cause; untrusted
    /// PTY input cannot reach this, because the terminal parses only after it
    /// is created.
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        let terminal = HeadlessTerminal::new(&HeadlessConfig::default(), rows, cols)
            .expect("libghostty-vt headless terminal init");
        Self { terminal }
    }

    /// Applies the next PTY output chunk.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.terminal.feed(bytes);
    }

    /// Updates the modeled terminal dimensions.
    ///
    /// A rejected size leaves the terminal at its old size, like the vt100
    /// backend keeps its grid; the next snapshot reports the size the
    /// library actually has.
    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.terminal.resize(rows, cols);
    }

    /// Rows and columns of the active screen.
    pub(crate) fn size(&self) -> (u16, u16) {
        self.terminal
            .size()
            .expect("libghostty read the screen size")
    }

    /// The state [`TerminalTracker::snapshot`](crate::TerminalTracker::snapshot)
    /// fills a snapshot from.
    pub(crate) fn state(&self) -> ScreenState {
        let terminal = &self.terminal;
        let (rows, cols) = self.size();
        let (cursor_row, cursor_col) = terminal.cursor().expect("libghostty read the cursor");
        let alternate_screen = terminal
            .alternate_screen()
            .expect("libghostty read the active screen");
        let body = terminal
            .format_vt(REPAINT_UNWRAP, REPAINT_TRIM, FORMATTER_EXTRAS_ATTACH)
            .expect("libghostty format the repaint");
        // The vt100 backend's prologue: screen switch first, then the clear,
        // so the repaint also wipes a pre-filled client screen on this model.
        let mut repaint = Vec::new();
        if alternate_screen {
            repaint.extend_from_slice(REPAINT_ALT_SCREEN);
        }
        repaint.extend_from_slice(REPAINT_CLEAR);
        repaint.extend_from_slice(&body);
        ScreenState {
            rows,
            cols,
            cursor_row,
            cursor_col,
            cursor_visible: terminal
                .cursor_visible()
                .expect("libghostty read the cursor visibility"),
            alternate_screen,
            // Active screen rows only, via the sys crate's two-view API: the
            // library retains scrollback (up to the configured byte limit) and
            // the history view `format_plain` would leak it into
            // `session.screen`; `visible_text` is the same definition the
            // spike's dump terminal cross-checks against (issue #2538 M2).
            visible_text: terminal
                .visible_text()
                .expect("libghostty read the active screen text"),
            repaint,
        }
    }

    /// The links on the visible screen: OSC 8 anchors read from the per-cell
    /// state the library keeps, plus plain-text URLs in the cell text.
    pub(crate) fn links(&self, max_url_bytes: usize) -> Vec<ScreenLink> {
        let terminal = &self.terminal;
        let (rows, cols) = self.size();
        let mut wrapped = Vec::with_capacity(usize::from(rows));
        let mut osc8 = Vec::new();
        for row in 0..rows {
            wrapped.push(
                terminal
                    .row_wrapped(row)
                    .expect("libghostty read the row wrap flag"),
            );
            let mut col = 0;
            while col < cols {
                let cell = terminal.cell(row, col).expect("libghostty read a cell");
                if cell.hyperlink.is_empty() || !valid_uri(&cell.hyperlink) {
                    col += 1;
                    continue;
                }
                let url = cell.hyperlink;
                let start = col;
                while col < cols
                    && terminal
                        .cell(row, col)
                        .expect("libghostty read a cell")
                        .hyperlink
                        == url
                {
                    col += 1;
                }
                osc8.push(ScreenLink {
                    url,
                    source: LinkSource::Osc8,
                    row,
                    col: start,
                });
            }
        }
        let text = text_links(
            rows,
            cols,
            &wrapped,
            &|row, col| {
                let cell = terminal.cell(row, col).expect("libghostty read a cell");
                if cell.spacer_tail {
                    None
                } else if cell.text.is_empty() {
                    Some(" ".to_owned())
                } else {
                    Some(cell.text)
                }
            },
            max_url_bytes,
        );
        combine_links(osc8, text, max_url_bytes)
    }
}
