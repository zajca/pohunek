//! The libghostty-vt backend of [`TerminalTracker`](crate::TerminalTracker).
//!
//! All unsafe FFI lives in `pohunek-terminal-ghostty-sys`; this module is safe
//! code that maps the sys crate onto the screen seam. The repaint body is
//! `format_vt(unwrap = true, trim = false)` with the attach extras (everything
//! but the palette), preceded by the same prologue the vt100 backend emits:
//! the screen switch (`?1049h` on the alternate screen) and `CSI 2J CSI H`,
//! so an attach clears the client screen on either model (issue #2538 M13/M14).
//!
//! Unlike `visible_text`, the repaint body includes the history rows the
//! library retained, so an attach or gap repaint of a busy session grows with
//! the retained history, not with the active screen. Measured at the
//! 5000-line flood scenario of `crates/terminal/tests/screen_backend.rs`: a
//! 405,000-byte input repaints as 405,103 bytes (1.0003x), and one SGR pair
//! per line as 490,103 bytes for 450,000 input (1.089x) — so
//! `SCROLLBACK_MAX_BYTES` (8 MiB) bounds the retained text (an allocation
//! estimate), not the formatted output. Bounding the formatter is the open
//! follow-up (issue #2596 finding 17, #2593 finding 11; plan a-31d1b862e65e
//! section 6 routes the repaint-history cost there).

// Rust guideline compliant 2026-11-27

use pohunek_terminal_ghostty_sys::{
    GhosttyError, HeadlessConfig, HeadlessTerminal, FORMATTER_EXTRAS_ATTACH,
};

use crate::links::{combine_links, text_links, AnchorLog, AnchorScreen, ScreenLink};
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
    /// library actually has. The rejection is logged, never dropped
    /// silently (issue #2593 finding 9).
    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        if let Err(error) = self.terminal.resize(rows, cols) {
            tracing::warn!(
                error = %error,
                rows,
                cols,
                "libghostty rejected the resize; keeping the previous size"
            );
        }
    }

    /// Rows and columns of the active screen.
    pub(crate) fn size(&self) -> (u16, u16) {
        self.terminal
            .size()
            .expect("libghostty read the screen size")
    }

    /// The state [`TerminalTracker::snapshot`](crate::TerminalTracker::snapshot)
    /// fills a snapshot from.
    ///
    /// Fallible: every read and the format go through the C library and can
    /// fail with any `GhosttyResult` code, not only `OUT_OF_MEMORY` (issue
    /// #2593 finding 8), so the caller degrades to an error snapshot instead
    /// of panicking under the worker's output lock.
    pub(crate) fn state(&self) -> Result<ScreenState, GhosttyError> {
        let terminal = &self.terminal;
        let (rows, cols) = terminal.size()?;
        let (cursor_row, cursor_col) = terminal.cursor()?;
        let alternate_screen = terminal.alternate_screen()?;
        let body = terminal.format_vt(REPAINT_UNWRAP, REPAINT_TRIM, FORMATTER_EXTRAS_ATTACH)?;
        // The vt100 backend's prologue: screen switch first, then the clear,
        // so the repaint also wipes a pre-filled client screen on this model.
        let mut repaint = Vec::new();
        if alternate_screen {
            repaint.extend_from_slice(REPAINT_ALT_SCREEN);
        }
        repaint.extend_from_slice(REPAINT_CLEAR);
        repaint.extend_from_slice(&body);
        Ok(ScreenState {
            rows,
            cols,
            cursor_row,
            cursor_col,
            cursor_visible: terminal.cursor_visible()?,
            alternate_screen,
            // Active screen rows only, via the sys crate's two-view API: the
            // library retains scrollback (up to the configured byte limit) and
            // the history view `format_plain` would leak it into
            // `session.screen`; `visible_text` is the same definition the
            // spike's dump terminal cross-checks against (issue #2538 M2).
            visible_text: terminal.visible_text()?,
            repaint,
        })
    }

    /// The anchor-log view of this screen: size, cursor and the text between
    /// two cells, read through the sys crate.
    pub(crate) fn anchor_view(&self) -> AnchorView<'_> {
        AnchorView {
            terminal: &self.terminal,
        }
    }

    /// The links on the visible screen: the byte-driven OSC 8 anchors the
    /// shared [`AnchorLog`] records (same record as the vt100 backend, so a
    /// scroll, clear, overwrite, resize or buffer switch drops them on either
    /// model), plus plain-text URLs in the cell text.
    pub(crate) fn links(&self, anchors: &AnchorLog, max_url_bytes: usize) -> Vec<ScreenLink> {
        let terminal = &self.terminal;
        let (rows, cols) = self.size();
        let mut wrapped = Vec::with_capacity(usize::from(rows));
        for row in 0..rows {
            wrapped.push(
                terminal
                    .row_wrapped(row)
                    .expect("libghostty read the row wrap flag"),
            );
        }
        let osc8 = anchors.live(&self.anchor_view());
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

/// The [`AnchorScreen`] view of a libghostty screen, mirroring the vt100
/// accessors (`size`, `cursor_position`, `contents_between`) over the active
/// screen cells so the shared anchor log behaves the same on both models.
pub(crate) struct AnchorView<'a> {
    terminal: &'a HeadlessTerminal,
}

impl AnchorView<'_> {
    fn wrapped(&self, row: u16) -> bool {
        self.terminal
            .row_wrapped(row)
            .expect("libghostty read the row wrap flag")
    }

    /// The text of `width` cells of `row` from `start_col`, with vt100's
    /// `Row::write_contents` layout: gap fill by column, a wide cell advances
    /// two columns and its spacer emits nothing.
    fn row_text(&self, row: u16, start_col: u16, width: u16) -> String {
        let (_, cols) = self
            .terminal
            .size()
            .expect("libghostty read the screen size");
        let end = start_col.saturating_add(width).min(cols);
        let mut text = String::new();
        let mut prev_was_wide = false;
        let mut prev_col = start_col;
        let mut col = start_col;
        while col < end {
            let cell = self
                .terminal
                .cell(row, col)
                .expect("libghostty read a cell");
            if prev_was_wide {
                prev_was_wide = false;
                col += 1;
                continue;
            }
            prev_was_wide = cell.wide;
            if !cell.text.is_empty() {
                for _ in 0..(col - prev_col) {
                    text.push(' ');
                }
                prev_col = col;
                text.push_str(&cell.text);
                prev_col += if cell.wide { 2 } else { 1 };
            }
            col += 1;
        }
        text
    }
}

impl AnchorScreen for AnchorView<'_> {
    fn size(&self) -> (u16, u16) {
        self.terminal
            .size()
            .expect("libghostty read the screen size")
    }

    fn cursor_position(&self) -> (u16, u16) {
        let (row, col) = self.terminal.cursor().expect("libghostty read the cursor");
        if self
            .terminal
            .cursor_pending_wrap()
            .expect("libghostty read the cursor pending wrap")
        {
            // vt100 reports a pending-wrap cursor one past the last column,
            // which the anchor log maps to the first cell of the next row.
            let (_, cols) = self.size();
            (row, cols)
        } else {
            (row, col)
        }
    }

    fn contents_between(
        &self,
        start_row: u16,
        start_col: u16,
        end_row: u16,
        end_col: u16,
    ) -> String {
        let (_, cols) = self.size();
        match start_row.cmp(&end_row) {
            std::cmp::Ordering::Less => {
                let mut contents = String::new();
                let mut row = start_row;
                while row <= end_row {
                    if row == start_row {
                        contents.push_str(&self.row_text(row, start_col, cols - start_col));
                        if !self.wrapped(row) {
                            contents.push('\n');
                        }
                    } else if row == end_row {
                        contents.push_str(&self.row_text(row, 0, end_col));
                    } else {
                        contents.push_str(&self.row_text(row, 0, cols));
                        if !self.wrapped(row) {
                            contents.push('\n');
                        }
                    }
                    row += 1;
                }
                contents
            }
            std::cmp::Ordering::Equal => {
                if start_col < end_col {
                    self.row_text(start_row, start_col, end_col - start_col)
                } else {
                    String::new()
                }
            }
            std::cmp::Ordering::Greater => String::new(),
        }
    }
}
