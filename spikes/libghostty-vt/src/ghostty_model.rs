//! The libghostty-vt model of the comparison harness.
//!
//! The FFI lives in `crates/terminal-ghostty-sys` now (issue #2538 P2); the
//! spike drives the shipped seam through `TerminalTracker::with_model` and
//! keeps a second headless terminal from the sys crate only for the cell dump,
//! the way `vt100_model` keeps a second parser.

// Rust guideline compliant 2026-06-26

use base64::Engine as _;
use pohunek_terminal::{ModelKind, TerminalTracker};
use pohunek_terminal_ghostty_sys::{HeadlessConfig, HeadlessTerminal};

use crate::config::ModelConfig;
use crate::model::{CellDump, GridDump, GridModel, ModelError};

const NAME: &str = "libghostty";

/// Largest OSC payload the recorder keeps; a longer sequence is dropped. The
/// library caps one clipboard transaction at 1 MiB decoded (the pin), so a
/// longer payload is refused there anyway.
const MAX_OSC52_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// `TerminalTracker::with_model(ModelKind::Libghostty)` next to a
/// `HeadlessTerminal` that exposes the cells, plus a recorder of the OSC 52
/// writes in the fed bytes.
///
/// The tracker supplies feed, resize and the repaint; the terminal supplies
/// the cell dump. `dump` fails when the two disagree on the visible text,
/// which keeps the dump a description of what the seam really runs. The
/// per-write clipboard record lives here, in the spike only: the product's
/// sys crate keeps a bounded counter and the last size (issue #2538 M3).
#[derive(Debug)]
pub struct GhosttyModel {
    tracker: TerminalTracker,
    terminal: HeadlessTerminal,
    osc52: Osc52Recorder,
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
            osc52: Osc52Recorder::default(),
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
        self.osc52.feed(bytes);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.tracker.resize(rows, cols);
        // A rejected size leaves the terminal at its old size; the pairwise
        // differences and the final-size guard of the report show it.
        let _ = self.terminal.resize(rows, cols);
    }

    fn repaint(&self) -> Result<Option<Vec<u8>>, ModelError> {
        // The seam's repaint, pinned in crates/terminal/src/ghostty_screen.rs:
        // the synchronized-output bracket around `format_vt(unwrap = true,
        // trim = false)` with an empty prefix.
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
            clipboard_writes: Some(self.osc52.writes.clone()),
        })
    }
}

/// Streams over fed bytes and records the decoded length of every OSC 52
/// clipboard write (`ESC ] 52 ; params ; base64 ST`); a `?` payload is a read
/// request and records nothing.
#[derive(Debug, Default)]
struct Osc52Recorder {
    state: RecState,
    command: Vec<u8>,
    payload: Vec<u8>,
    writes: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RecState {
    #[default]
    Ground,
    Escape,
    Osc,
    Payload,
    PayloadEscape,
    Discard,
    DiscardEscape,
}

impl Osc52Recorder {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                RecState::Ground => {
                    if byte == 0x1b {
                        self.state = RecState::Escape;
                    }
                }
                RecState::Escape => {
                    self.state = if byte == b']' {
                        self.command.clear();
                        self.payload.clear();
                        RecState::Osc
                    } else {
                        RecState::Ground
                    };
                }
                RecState::Osc => match byte {
                    b';' => self.state = RecState::Payload,
                    0x07 => self.finish(),
                    _ if self.command.len() < 16 => self.command.push(byte),
                    _ => self.state = RecState::Discard,
                },
                RecState::Payload => match byte {
                    0x07 => self.finish(),
                    0x1b => self.state = RecState::PayloadEscape,
                    _ if self.payload.len() < MAX_OSC52_PAYLOAD_BYTES => self.payload.push(byte),
                    _ => self.state = RecState::Discard,
                },
                RecState::PayloadEscape => {
                    if byte == b'\\' {
                        self.finish();
                    } else {
                        self.state = RecState::Discard;
                    }
                }
                RecState::Discard => {
                    if byte == 0x07 {
                        self.state = RecState::Ground;
                    } else if byte == 0x1b {
                        self.state = RecState::DiscardEscape;
                    }
                }
                RecState::DiscardEscape => {
                    self.state = if byte == b'\\' {
                        RecState::Ground
                    } else {
                        RecState::Discard
                    };
                }
            }
        }
    }

    fn finish(&mut self) {
        self.state = RecState::Ground;
        if self.command == b"52" {
            if let Some(separator) = self.payload.iter().position(|byte| *byte == b';') {
                let data = &self.payload[separator + 1..];
                if data != b"?" {
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(data) {
                        self.writes.push(decoded.len());
                    }
                }
            }
        }
        self.command.clear();
        self.payload.clear();
    }
}
