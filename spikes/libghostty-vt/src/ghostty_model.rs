//! The libghostty-vt model of the comparison harness.

// Rust guideline compliant 2026-06-26

use crate::config::ModelConfig;
use crate::ghostty::{GhosttyError, HeadlessTerminal};
use crate::model::{CellDump, GridDump, GridModel, ModelError};

const NAME: &str = "libghostty";

/// `HeadlessTerminal` behind the [`GridModel`] interface.
#[derive(Debug)]
pub struct GhosttyModel {
    terminal: HeadlessTerminal,
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used as a `map_err` function item, which passes the error by value"
)]
fn model_error(error: GhosttyError) -> ModelError {
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
            terminal: HeadlessTerminal::new(config, rows, cols).map_err(model_error)?,
        })
    }
}

impl GridModel for GhosttyModel {
    fn name(&self) -> &'static str {
        NAME
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.terminal.feed(bytes);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        // A rejected size leaves the terminal at its old size; the pairwise
        // differences and the final-size guard of the report show it.
        let _ = self.terminal.resize(rows, cols);
    }

    fn repaint(&self) -> Result<Option<Vec<u8>>, ModelError> {
        self.terminal.format_vt().map(Some).map_err(model_error)
    }

    fn dump(&self) -> Result<GridDump, ModelError> {
        let terminal = &self.terminal;
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
            title: terminal.title().map_err(model_error)?,
            progress: terminal.progress().map(str::to_owned),
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
