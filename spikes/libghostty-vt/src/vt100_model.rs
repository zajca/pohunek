//! The model Pohunek ships today (`vt100` without scrollback) and the same
//! parser with a scrollback buffer.

// Rust guideline compliant 2026-06-26

use pohunek_terminal::TerminalTracker;

use crate::model::{CellDump, GridDump, GridModel, ModelError};

/// Name of the model without scrollback in reports.
const NAME: &str = "vt100";
/// Name of the model with scrollback in reports.
const NAME_SCROLLBACK: &str = "vt100-scrollback";

/// `TerminalTracker`, which the worker uses, next to a second
/// `vt100::Parser::new(rows, cols, 0)` that exposes cells.
///
/// The tracker has no cell access, so the parser supplies the cells. `dump`
/// fails when the two disagree on the visible text, which keeps the dump a
/// description of what Pohunek really uses.
pub struct Vt100Model {
    name: &'static str,
    tracker: TerminalTracker,
    parser: vt100::Parser,
}

impl std::fmt::Debug for Vt100Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vt100Model")
            .field("size", &self.parser.screen().size())
            .finish_non_exhaustive()
    }
}

impl Vt100Model {
    /// Creates a model of `rows` x `cols` cells without scrollback, as Pohunek
    /// runs it.
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            name: NAME,
            tracker: TerminalTracker::new(rows, cols),
            parser: vt100::Parser::new(rows, cols, 0),
        }
    }

    /// Creates a model whose parser keeps up to `scrollback_rows` scrolled-off
    /// rows. The tracker is the shipped one and keeps none.
    #[must_use]
    pub fn with_scrollback(rows: u16, cols: u16, scrollback_rows: usize) -> Self {
        Self {
            name: NAME_SCROLLBACK,
            tracker: TerminalTracker::new(rows, cols),
            parser: vt100::Parser::new(rows, cols, scrollback_rows),
        }
    }

    /// Rows the parser retains above the screen.
    fn retained_scrollback(&self) -> usize {
        // The scrollback offset is clamped to the retained rows, so asking for
        // the largest offset reads the count back.
        let mut probe = self.parser.screen().clone();
        probe.set_scrollback(usize::MAX);
        probe.scrollback()
    }
}

impl GridModel for Vt100Model {
    fn name(&self) -> &'static str {
        self.name
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.tracker.feed(bytes);
        self.parser.process(bytes);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.tracker.resize(rows, cols);
        self.parser.screen_mut().set_size(rows, cols);
    }

    fn repaint(&self) -> Result<Option<Vec<u8>>, ModelError> {
        Ok(Some(self.tracker.snapshot(0).ansi))
    }

    fn dump(&self) -> Result<GridDump, ModelError> {
        let snapshot = self.tracker.snapshot(0);
        let screen = self.parser.screen();
        if snapshot.visible_text != screen.contents() {
            return Err(ModelError {
                model: self.name,
                reason: "the tracker and the cell parser disagree on the visible text".to_owned(),
            });
        }
        let (rows, cols) = screen.size();
        let mut cells = Vec::with_capacity(usize::from(rows));
        let mut row_wrapped = Vec::with_capacity(usize::from(rows));
        for row in 0..rows {
            let mut line = Vec::with_capacity(usize::from(cols));
            for col in 0..cols {
                let cell = screen.cell(row, col).ok_or_else(|| ModelError {
                    model: self.name,
                    reason: format!("no cell at {row},{col} of a {rows}x{cols} screen"),
                })?;
                line.push(CellDump {
                    text: cell.contents().to_owned(),
                    width: if cell.is_wide() { 2 } else { 1 },
                    spacer_tail: cell.is_wide_continuation(),
                });
            }
            cells.push(line);
            row_wrapped.push(screen.row_wrapped(row));
        }
        let (cursor_row, cursor_col) = screen.cursor_position();
        Ok(GridDump {
            rows,
            cols,
            cursor_row,
            cursor_col,
            cursor_visible: !screen.hide_cursor(),
            alternate_screen: screen.alternate_screen(),
            title: snapshot.title,
            progress: snapshot.progress,
            scrollback_rows: self.retained_scrollback(),
            cells,
            row_wrapped,
            hyperlinks: None,
            kitty_keyboard_flags: None,
            clipboard_writes: None,
        })
    }
}
