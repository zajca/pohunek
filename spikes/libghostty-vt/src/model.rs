//! The screen-model interface the replay drives and the grid it reports.

// Rust guideline compliant 2026-06-26

/// One screen cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellDump {
    /// Grapheme text of the cell; empty for a blank cell and for a spacer.
    pub text: String,
    /// Columns the cell occupies: 1, or 2 for the first half of a wide cell.
    pub width: u8,
    /// Whether this is the trailing half of a wide cell.
    pub spacer_tail: bool,
}

/// What a model reports about its screen after some input.
///
/// A field wrapped in `Option` that is `None` is a property the model cannot
/// observe; an assertion on it fails with that reason instead of erroring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridDump {
    /// Row count.
    pub rows: u16,
    /// Column count.
    pub cols: u16,
    /// Cursor row, zero based.
    pub cursor_row: u16,
    /// Cursor column, zero based.
    pub cursor_col: u16,
    /// Whether the cursor is visible.
    pub cursor_visible: bool,
    /// Whether the alternate screen is active.
    pub alternate_screen: bool,
    /// Current window title, when one was set.
    pub title: Option<String>,
    /// Payload of the latest OSC 9 sequence, when one was seen.
    pub progress: Option<String>,
    /// Rows scrolled off the top of the primary screen and retained.
    pub scrollback_rows: usize,
    /// `rows` x `cols` cells.
    pub cells: Vec<Vec<CellDump>>,
    /// Per row: whether the row soft-wraps into the next one.
    pub row_wrapped: Vec<bool>,
    /// Per cell: the OSC 8 target, empty when the cell has none.
    pub hyperlinks: Option<Vec<Vec<String>>>,
    /// Active Kitty keyboard protocol flags.
    pub kitty_keyboard_flags: Option<u32>,
    /// Decoded byte length of every OSC 52 clipboard write, in order.
    pub clipboard_writes: Option<Vec<usize>>,
}

/// A model could not produce a dump.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("model {model}: {reason}")]
pub struct ModelError {
    /// The model's name.
    pub model: &'static str,
    /// What went wrong.
    pub reason: String,
}

/// A terminal screen model fed PTY output.
pub trait GridModel {
    /// Name the report uses for this model.
    fn name(&self) -> &'static str;

    /// Applies the next PTY output chunk.
    fn feed(&mut self, bytes: &[u8]);

    /// Applies a PTY resize.
    fn resize(&mut self, rows: u16, cols: u16);

    /// Reports the current screen.
    ///
    /// # Errors
    ///
    /// Returns a [`ModelError`] when the model's own views of the screen
    /// disagree.
    fn dump(&self) -> Result<GridDump, ModelError>;

    /// The byte stream that recreates the current screen in a fresh model,
    /// the way an attach repaint does; `None` when the model offers none.
    ///
    /// # Errors
    ///
    /// Returns a [`ModelError`] when the model fails to format its screen.
    fn repaint(&self) -> Result<Option<Vec<u8>>, ModelError>;
}
