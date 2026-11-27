//! The libghostty-vt model of the comparison harness.
//!
//! The FFI lives in `crates/terminal-ghostty-sys` now (issue #2538 P2); the
//! spike drives the shipped seam through `TerminalTracker::with_model` and
//! keeps a second headless terminal from the sys crate only for the cell dump,
//! the way `vt100_model` keeps a second parser.

// Rust guideline compliant 2026-06-26

use pohunek_terminal::{ModelKind, TerminalTracker};
use pohunek_terminal_ghostty_sys::{HeadlessConfig, HeadlessTerminal};

use crate::config::ModelConfig;
use crate::model::{CellDump, GridDump, GridModel, ModelError};

const NAME: &str = "libghostty";

/// `TerminalTracker::with_model(ModelKind::Libghostty)` next to a
/// `HeadlessTerminal` that exposes the cells.
///
/// The tracker supplies feed, resize and the repaint; the terminal supplies
/// the cell dump. `dump` fails when the two disagree on the visible text,
/// which keeps the dump a description of what the seam really runs.
#[derive(Debug)]
pub struct GhosttyModel {
    tracker: TerminalTracker,
    terminal: HeadlessTerminal,
}

/// Maps the spike's `config/model.toml` headless keys onto the sys crate's
/// pinned settings. Public for the `throughput` binary of this package.
#[must_use]
pub fn headless_config(config: &ModelConfig) -> HeadlessConfig {
    HeadlessConfig {
        scrollback_max_bytes: config.ghostty_scrollback_max_bytes,
        apc_max_bytes: config.ghostty_apc_max_bytes,
        unknown_max_bytes: config.ghostty_unknown_max_bytes,
        continuation_max_bytes: config.ghostty_continuation_max_bytes,
        clipboard_write_max_bytes: config.ghostty_clipboard_write_max_bytes,
        cell_px: config.ghostty_cell_px,
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used as a `map_err` function item, which passes the error by value"
)]
fn model_error(error: pohunek_terminal_ghostty_sys::GhosttyError) -> ModelError {
    ModelError {
        model: NAME,
        reason: error.to_string(),
    }
}

impl GhosttyModel {
    /// Creates a model of `rows` x `cols` cells with the settings of `config`.
    ///
    /// # Errors
    ///
    /// Returns a [`ModelError`] when libghostty rejects the setup.
    pub fn new(config: &ModelConfig, rows: u16, cols: u16) -> Result<Self, ModelError> {
        Ok(Self {
            tracker: TerminalTracker::with_model(ModelKind::Libghostty, rows, cols),
            terminal: HeadlessTerminal::new(&headless_config(config), rows, cols)
                .map_err(model_error)?,
        })
    }
}

impl GridModel for GhosttyModel {
    fn name(&self) -> &'static str {
        NAME
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.tracker.feed(bytes);
        self.terminal.feed(bytes);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.tracker.resize(rows, cols);
        // A rejected size leaves the terminal at its old size; the pairwise
        // differences and the final-size guard of the report show it.
        let _ = self.terminal.resize(rows, cols);
    }

    fn repaint(&self) -> Result<Option<Vec<u8>>, ModelError> {
        // The seam's repaint: the synchronized-output bracket around
        // `format_vt(unwrap = true, trim = false)` with the configured prefix.
        Ok(Some(self.tracker.snapshot(0).ansi))
    }

    fn dump(&self) -> Result<GridDump, ModelError> {
        let snapshot = self.tracker.snapshot(0);
        let terminal = &self.terminal;
        let visible =
            String::from_utf8_lossy(&terminal.format_plain().map_err(model_error)?).into_owned();
        if snapshot.visible_text != visible {
            return Err(ModelError {
                model: NAME,
                reason: "the tracker and the headless terminal disagree on the visible text"
                    .to_owned(),
            });
        }
        let (rows, cols) = terminal.size().map_err(model_error)?;
        let (cursor_row, cursor_col) = terminal.cursor().map_err(model_error)?;
        let mut cells = Vec::new();
        let mut row_wrapped = Vec::new();
        let mut hyperlinks = Vec::new();
        for row in 0..rows {
            let mut line = Vec::new();
            let mut links = Vec::new();
            for col in 0..cols {
                let cell = terminal.cell(row, col).map_err(model_error)?;
                line.push(CellDump {
                    text: cell.text,
                    width: if cell.wide { 2 } else { 1 },
                    spacer_tail: cell.spacer_tail,
                });
                links.push(cell.hyperlink);
            }
            cells.push(line);
            hyperlinks.push(links);
            row_wrapped.push(terminal.row_wrapped(row).map_err(model_error)?);
        }
        Ok(GridDump {
            rows,
            cols,
            cursor_row,
            cursor_col,
            cursor_visible: terminal.cursor_visible().map_err(model_error)?,
            alternate_screen: terminal.alternate_screen().map_err(model_error)?,
            title: snapshot.title,
            progress: snapshot.progress,
            scrollback_rows: terminal.scrollback_rows().map_err(model_error)?,
            cells,
            row_wrapped,
            hyperlinks: Some(hyperlinks),
            kitty_keyboard_flags: Some(u32::from(
                terminal.kitty_keyboard_flags().map_err(model_error)?,
            )),
            clipboard_writes: Some(terminal.clipboard_writes().to_vec()),
        })
    }
}
