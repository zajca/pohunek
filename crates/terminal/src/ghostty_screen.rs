//! The libghostty-vt backend of [`TerminalTracker`](crate::TerminalTracker).
//!
//! All unsafe FFI lives in `pohunek-terminal-ghostty-sys`; this module is safe
//! code that maps the sys crate onto the screen seam. The repaint is
//! `format_vt(unwrap = true, trim = false)` with an empty prefix, the pinned
//! 0.46 model settings of the spike (`spikes/libghostty-vt/config/model.toml`).

// Rust guideline compliant 2026-11-27

use pohunek_terminal_ghostty_sys::{HeadlessConfig, HeadlessTerminal};

use crate::links::{combine_links, text_links, valid_uri, LinkSource, ScreenLink};
use crate::snapshot::ScreenState;

/// Repaint option: join soft-wrapped rows into one line.
const REPAINT_UNWRAP: bool = true;
/// Repaint option: keep trailing spaces of each line.
const REPAINT_TRIM: bool = false;
/// Bytes written to a fresh instance before the repaint: none in the pinned
/// model settings.
const REPAINT_PREFIX: &[u8] = b"";

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
        let body = terminal
            .format_vt(REPAINT_UNWRAP, REPAINT_TRIM)
            .expect("libghostty format the repaint");
        let mut repaint = REPAINT_PREFIX.to_vec();
        repaint.extend_from_slice(&body);
        ScreenState {
            rows,
            cols,
            cursor_row,
            cursor_col,
            cursor_visible: terminal
                .cursor_visible()
                .expect("libghostty read the cursor visibility"),
            alternate_screen: terminal
                .alternate_screen()
                .expect("libghostty read the active screen"),
            visible_text: String::from_utf8_lossy(
                &terminal
                    .format_plain()
                    .expect("libghostty format the plain text"),
            )
            .into_owned(),
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
